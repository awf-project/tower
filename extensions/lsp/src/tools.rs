//! Tool dispatch for the LSP sidecar extension (spec 27).
//!
//! Implements the LSP tools: `diagnostics`, `definition`, `references`, `hover`,
//! `implementations`, and `rename`.
//! Each tool reads file content via `workspace/readFile` HostCall (using the
//! extension protocol capability), then delegates to the session pool.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::{Diagnostic, Hover, Location, Position, Severity};
use extension_protocol::{
    HostCall, LspImplementationRequest, LspImplementationResult, LspOperation, LspOutcome,
    LspOutcomeCode, LspOutcomePhase, LspOutcomeStatus, RenameError, RenameErrorCode, RenamePreview,
    RenameRequest, RenameResult, WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult,
};
use serde_json::{Value, json};

use crate::lsp_adapter::LspSessionError;
use crate::lsp_adapter::decode::WorkspaceEditDecodeError;
use crate::protocol::{self, HostCallIdAllocator, QueuedFrame};
use crate::session::LspSessionPool;

#[derive(Debug)]
pub(crate) enum ToolDispatchError {
    Legacy(String),
    ReadOnlyOutcome(LspOutcome),
}

impl fmt::Display for ToolDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy(message) => formatter.write_str(message),
            Self::ReadOnlyOutcome(outcome) => formatter.write_str(&outcome.message),
        }
    }
}

impl std::error::Error for ToolDispatchError {}

impl From<String> for ToolDispatchError {
    fn from(message: String) -> Self {
        Self::Legacy(message)
    }
}

fn unsupported_read_only_outcome(
    operation: LspOperation,
    code: LspOutcomeCode,
    path: String,
) -> LspOutcome {
    let message = match code {
        LspOutcomeCode::LanguageNotConfigured => "No language server is configured for this file.",
        LspOutcomeCode::CapabilityUnavailable => {
            "The configured language server does not provide this operation."
        }
        _ => "The language server operation is unavailable.",
    };
    LspOutcome {
        status: LspOutcomeStatus::Unsupported,
        code,
        language: None,
        command: None,
        operation: Some(operation),
        path: Some(path),
        phase: LspOutcomePhase::Runtime,
        message: message.to_owned(),
    }
}

fn read_only_session_error(
    error: LspSessionError,
    operation: LspOperation,
    path: String,
    binding: Option<(&str, &str)>,
) -> ToolDispatchError {
    let LspSessionError::Backend(code) = error else {
        return ToolDispatchError::Legacy(error.to_string());
    };
    let (language, command) = binding
        .map(|(language, command)| (Some(language.to_owned()), Some(command.to_owned())))
        .unwrap_or((None, None));
    ToolDispatchError::ReadOnlyOutcome(LspOutcome {
        status: LspOutcomeStatus::Error,
        code,
        language,
        command,
        operation: Some(operation),
        path: Some(path),
        phase: LspOutcomePhase::Runtime,
        message: error.to_string(),
    })
}

/// Dispatch an `invokeTool` call to the appropriate LSP tool.
#[allow(clippy::too_many_arguments)]
pub fn dispatch<W, R>(
    name: &str,
    params: Value,
    pool: &mut LspSessionPool,
    workspace_root: &PathBuf,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, ToolDispatchError>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    match name {
        "diagnostics" => diagnostics(params, pool, workspace_root, out, lines, next_id, deferred),
        "definition" => definition(params, pool, workspace_root, out, lines, next_id, deferred),
        "references" => references(params, pool, workspace_root, out, lines, next_id, deferred),
        "hover" => hover(params, pool, workspace_root, out, lines, next_id, deferred),
        "implementations" => {
            implementations(params, pool, workspace_root, out, lines, next_id, deferred)
        }
        "rename" => {
            rename(params, pool, workspace_root, out, lines, next_id, deferred).map_err(Into::into)
        }
        other => Err(format!("unknown LSP tool: {other}").into()),
    }
}

/// Read file content via the `workspace/readFile` HostCall.
fn read_file<W, R>(
    path: &str,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<String, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let call = HostCall::ReadFile {
        path: path.to_owned(),
    };
    let raw = protocol::host_call(out, lines, next_id, "workspace/readFile", &call, deferred)?;
    match raw {
        Value::String(s) => Ok(s),
        other => Err(format!(
            "workspace/readFile returned unexpected value: {other}"
        )),
    }
}

/// Serialize a `Diagnostic` to JSON (matches the MCP contract from spec 14a).
fn diagnostic_to_json(d: &Diagnostic) -> Value {
    json!({
        "line": d.range.start.line,
        "character": d.range.start.character,
        "endLine": d.range.end.line,
        "endCharacter": d.range.end.character,
        "severity": severity_str(d.severity),
        "message": d.message,
        "source": d.source,
        "code": d.code,
    })
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Information => "info",
        Severity::Hint => "hint",
    }
}

/// Serialize a `Location` to JSON.
fn location_to_json(l: &Location) -> Value {
    json!({
        "path": l.path.as_str(),
        "line": l.range.start.line,
        "character": l.range.start.character,
        "endLine": l.range.end.line,
        "endCharacter": l.range.end.character,
    })
}

/// Serialize a `Hover` to JSON.
fn hover_to_json(h: &Hover) -> Value {
    json!({
        "contents": h.contents,
    })
}

/// `diagnostics` tool: check a file and return diagnostics.
#[allow(clippy::too_many_arguments)]
fn diagnostics<W, R>(
    params: Value,
    pool: &mut LspSessionPool,
    _workspace_root: &PathBuf,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, ToolDispatchError>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing 'path' parameter".to_owned())?;

    let rel = RelativePath::new(path);
    if !pool.serves(&rel) {
        let outcome = unsupported_read_only_outcome(
            LspOperation::Diagnostics,
            LspOutcomeCode::LanguageNotConfigured,
            path.to_owned(),
        );
        return Ok(json!({ "supported": false, "diagnostics": [], "outcome": outcome }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;

    match pool.check(&rel, &text) {
        Ok(diags) => {
            let arr: Vec<Value> = diags.iter().map(diagnostic_to_json).collect();
            Ok(json!({ "supported": true, "diagnostics": arr }))
        }
        Err(LspSessionError::Unconfigured) => Ok(json!({
            "supported": false, "diagnostics": [], "outcome": unsupported_read_only_outcome(LspOperation::Diagnostics, LspOutcomeCode::LanguageNotConfigured, path.to_owned()),
        })),
        Err(LspSessionError::CapabilityUnavailable) => Ok(json!({
            "supported": false, "diagnostics": [], "outcome": unsupported_read_only_outcome(LspOperation::Diagnostics, LspOutcomeCode::CapabilityUnavailable, path.to_owned()),
        })),
        Err(error) => Err(read_only_session_error(
            error,
            LspOperation::Diagnostics,
            path.to_owned(),
            pool.binding_for(&rel),
        )),
    }
}

/// `definition` tool: go to definition at a given position.
#[allow(clippy::too_many_arguments)]
fn definition<W, R>(
    params: Value,
    pool: &mut LspSessionPool,
    _workspace_root: &PathBuf,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, ToolDispatchError>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing 'path' parameter".to_owned())?;
    let line = params
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing 'line' parameter".to_owned())? as u32;
    let character = params
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing 'character' parameter".to_owned())? as u32;

    let rel = RelativePath::new(path);
    if !pool.serves(&rel) {
        let outcome = unsupported_read_only_outcome(
            LspOperation::Definition,
            LspOutcomeCode::LanguageNotConfigured,
            path.to_owned(),
        );
        return Ok(json!({ "supported": false, "locations": [], "outcome": outcome }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;
    let pos = Position { line, character };

    match pool.definition(&rel, &text, pos) {
        Ok(locs) => {
            let arr: Vec<Value> = locs.iter().map(location_to_json).collect();
            Ok(json!({ "supported": true, "locations": arr }))
        }
        Err(LspSessionError::Unconfigured) => Ok(
            json!({ "supported": false, "locations": [], "outcome": unsupported_read_only_outcome(LspOperation::Definition, LspOutcomeCode::LanguageNotConfigured, path.to_owned()) }),
        ),
        Err(LspSessionError::CapabilityUnavailable) => Ok(
            json!({ "supported": false, "locations": [], "outcome": unsupported_read_only_outcome(LspOperation::Definition, LspOutcomeCode::CapabilityUnavailable, path.to_owned()) }),
        ),
        Err(error) => Err(read_only_session_error(
            error,
            LspOperation::Definition,
            path.to_owned(),
            pool.binding_for(&rel),
        )),
    }
}

/// `references` tool: find references at a given position.
#[allow(clippy::too_many_arguments)]
fn references<W, R>(
    params: Value,
    pool: &mut LspSessionPool,
    _workspace_root: &PathBuf,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, ToolDispatchError>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing 'path' parameter".to_owned())?;
    let line = params
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing 'line' parameter".to_owned())? as u32;
    let character = params
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing 'character' parameter".to_owned())? as u32;

    let rel = RelativePath::new(path);
    if !pool.serves(&rel) {
        let outcome = unsupported_read_only_outcome(
            LspOperation::References,
            LspOutcomeCode::LanguageNotConfigured,
            path.to_owned(),
        );
        return Ok(json!({ "supported": false, "locations": [], "outcome": outcome }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;
    let pos = Position { line, character };

    match pool.references(&rel, &text, pos) {
        Ok(locs) => {
            let arr: Vec<Value> = locs.iter().map(location_to_json).collect();
            Ok(json!({ "supported": true, "locations": arr }))
        }
        Err(LspSessionError::Unconfigured) => Ok(
            json!({ "supported": false, "locations": [], "outcome": unsupported_read_only_outcome(LspOperation::References, LspOutcomeCode::LanguageNotConfigured, path.to_owned()) }),
        ),
        Err(LspSessionError::CapabilityUnavailable) => Ok(
            json!({ "supported": false, "locations": [], "outcome": unsupported_read_only_outcome(LspOperation::References, LspOutcomeCode::CapabilityUnavailable, path.to_owned()) }),
        ),
        Err(error) => Err(read_only_session_error(
            error,
            LspOperation::References,
            path.to_owned(),
            pool.binding_for(&rel),
        )),
    }
}

/// `hover` tool: return hover information at a given position.
#[allow(clippy::too_many_arguments)]
fn hover<W, R>(
    params: Value,
    pool: &mut LspSessionPool,
    _workspace_root: &PathBuf,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, ToolDispatchError>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing 'path' parameter".to_owned())?;
    let line = params
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing 'line' parameter".to_owned())? as u32;
    let character = params
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing 'character' parameter".to_owned())? as u32;

    let rel = RelativePath::new(path);
    if !pool.serves(&rel) {
        let outcome = unsupported_read_only_outcome(
            LspOperation::Hover,
            LspOutcomeCode::LanguageNotConfigured,
            path.to_owned(),
        );
        return Ok(json!({ "supported": false, "hover": null, "outcome": outcome }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;
    let pos = Position { line, character };

    match pool.hover(&rel, &text, pos) {
        Ok(Some(h)) => Ok(json!({ "supported": true, "hover": hover_to_json(&h) })),
        Ok(None) => Ok(json!({ "supported": true, "hover": null })),
        Err(LspSessionError::Unconfigured) => Ok(
            json!({ "supported": false, "hover": null, "outcome": unsupported_read_only_outcome(LspOperation::Hover, LspOutcomeCode::LanguageNotConfigured, path.to_owned()) }),
        ),
        Err(LspSessionError::CapabilityUnavailable) => Ok(
            json!({ "supported": false, "hover": null, "outcome": unsupported_read_only_outcome(LspOperation::Hover, LspOutcomeCode::CapabilityUnavailable, path.to_owned()) }),
        ),
        Err(error) => Err(read_only_session_error(
            error,
            LspOperation::Hover,
            path.to_owned(),
            pool.binding_for(&rel),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn implementations<W, R>(
    params: Value,
    pool: &mut LspSessionPool,
    _workspace_root: &PathBuf,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, ToolDispatchError>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let request: LspImplementationRequest =
        serde_json::from_value(params).map_err(|e| format!("bad LspImplementationRequest: {e}"))?;
    let rel = RelativePath::new(&request.path);
    if !pool.serves(&rel) {
        return serde_json::to_value(LspImplementationResult {
            supported: false,
            locations: Vec::new(),
            outcome: Some(unsupported_read_only_outcome(
                LspOperation::Implementations,
                LspOutcomeCode::LanguageNotConfigured,
                request.path,
            )),
        })
        .map_err(|error| error.to_string().into());
    }

    let text = read_file(&request.path, out, lines, next_id, deferred)?;
    let position = Position {
        line: request.line,
        character: request.character,
    };

    match pool.implementations(&rel, &text, position) {
        Ok(locations) => {
            let locations = locations.iter().map(protocol_location).collect::<Vec<_>>();
            Ok(serde_json::to_value(LspImplementationResult {
                supported: true,
                locations,
                outcome: None,
            })
            .map_err(|e| format!("serialize LspImplementationResult failed: {e}"))?)
        }
        Err(LspSessionError::Unconfigured) => serde_json::to_value(LspImplementationResult {
            supported: false,
            locations: Vec::new(),
            outcome: Some(unsupported_read_only_outcome(
                LspOperation::Implementations,
                LspOutcomeCode::LanguageNotConfigured,
                request.path,
            )),
        })
        .map_err(|error| error.to_string().into()),
        Err(LspSessionError::CapabilityUnavailable) => {
            serde_json::to_value(LspImplementationResult {
                supported: false,
                locations: Vec::new(),
                outcome: Some(unsupported_read_only_outcome(
                    LspOperation::Implementations,
                    LspOutcomeCode::CapabilityUnavailable,
                    request.path,
                )),
            })
            .map_err(|error| error.to_string().into())
        }
        Err(error) => Err(read_only_session_error(
            error,
            LspOperation::Implementations,
            request.path,
            pool.binding_for(&rel),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn rename<W, R>(
    params: Value,
    pool: &mut LspSessionPool,
    _workspace_root: &PathBuf,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let request: RenameRequest =
        serde_json::from_value(params).map_err(|e| format!("bad RenameRequest: {e}"))?;
    let rel = RelativePath::new(&request.path);
    if !pool.serves(&rel) {
        return lsp_session_error_value(LspSessionError::Unconfigured, Some(request.path));
    }

    let text = read_file(&request.path, out, lines, next_id, deferred)?;
    let position = Position {
        line: request.line,
        character: request.character,
    };

    let raw_edit = match pool.rename(&rel, &text, position, &request.new_name) {
        Ok(raw_edit) => raw_edit,
        Err(error) => return lsp_session_error_value(error, Some(request.path)),
    };

    let mut text_cache = HashMap::from([(request.path.clone(), text)]);
    let spans = match pool.decode_rename_workspace_edit(raw_edit, |path| {
        if let Some(text) = text_cache.get(path) {
            return Ok(text.clone());
        }
        let text = read_file(path, out, lines, next_id, deferred).map_err(|message| {
            WorkspaceEditDecodeError::UnreadableFile {
                path: path.to_owned(),
                message,
            }
        })?;
        text_cache.insert(path.to_owned(), text.clone());
        Ok(text)
    }) {
        Ok(spans) => spans,
        Err(error) => return rename_decode_error_value(error),
    };

    let dry_run = request.dry_run.unwrap_or(false);
    let apply_request = WorkspaceApplyEditsRequest {
        edits: spans.clone(),
        dry_run: Some(dry_run),
    };
    let value = protocol::host_call_value(
        out,
        lines,
        next_id,
        "workspace/applyEdits",
        serde_json::to_value(apply_request)
            .map_err(|e| format!("serialize WorkspaceApplyEditsRequest failed: {e}"))?,
        deferred,
    )?;
    let apply_result = serde_json::from_value::<WorkspaceApplyEditsResult>(value)
        .map_err(|e| format!("malformed workspace/applyEdits response: {e}"))?;

    if dry_run {
        serde_json::to_value(RenamePreview {
            spans,
            preview: combined_preview(&apply_result),
            per_file: apply_result.per_file,
        })
        .map_err(|e| format!("serialize RenamePreview failed: {e}"))
    } else {
        serde_json::to_value(RenameResult {
            applied: apply_result.per_file.iter().any(|file| file.applied),
            files_changed: apply_result.files_changed,
            spans,
            preview: optional_combined_preview(&apply_result),
            per_file: apply_result.per_file,
        })
        .map_err(|e| format!("serialize RenameResult failed: {e}"))
    }
}

fn protocol_location(location: &Location) -> extension_protocol::Location {
    extension_protocol::Location {
        path: location.path.as_str().to_owned(),
        line: location.range.start.line,
        character: location.range.start.character,
        end_line: location.range.end.line,
        end_character: location.range.end.character,
    }
}

fn lsp_session_error_value(error: LspSessionError, path: Option<String>) -> Result<Value, String> {
    match error {
        LspSessionError::NotRenameable => {
            rename_error_value(RenameErrorCode::NotRenameable, "not renameable", path)
        }
        error @ (LspSessionError::Unconfigured | LspSessionError::CapabilityUnavailable) => {
            rename_error_value_with_outcome(
                RenameErrorCode::UnsupportedLanguage,
                "unsupported language for rename",
                path,
                error,
            )
        }
        error @ LspSessionError::Backend(_) => rename_error_value_with_outcome(
            RenameErrorCode::BackendError,
            "language server request failed",
            path,
            error,
        ),
    }
}

fn rename_decode_error_value(error: WorkspaceEditDecodeError) -> Result<Value, String> {
    let code = error.rename_error_code();
    let path = match &error {
        WorkspaceEditDecodeError::MissingText { path }
        | WorkspaceEditDecodeError::UnreadableFile { path, .. }
        | WorkspaceEditDecodeError::InvalidRange { path, .. } => Some(path.clone()),
        WorkspaceEditDecodeError::UnsupportedWorkspaceEdit { .. }
        | WorkspaceEditDecodeError::InvalidPath { .. } => None,
    };
    rename_error_value(code, workspace_edit_decode_message(error), path)
}

fn rename_error_value(
    code: RenameErrorCode,
    message: impl Into<String>,
    path: Option<String>,
) -> Result<Value, String> {
    serde_json::to_value(RenameError {
        code,
        message: message.into(),
        path,
        outcome: None,
    })
    .map_err(|e| format!("serialize RenameError failed: {e}"))
}

fn rename_error_value_with_outcome(
    code: RenameErrorCode,
    message: impl Into<String>,
    path: Option<String>,
    error: LspSessionError,
) -> Result<Value, String> {
    let message = message.into();
    serde_json::to_value(RenameError {
        code,
        outcome: rename_lsp_outcome(error, path.clone(), message.clone()),
        message,
        path,
    })
    .map_err(|e| format!("serialize RenameError failed: {e}"))
}

fn rename_lsp_outcome(
    error: LspSessionError,
    path: Option<String>,
    message: String,
) -> Option<LspOutcome> {
    let (status, code) = match error {
        LspSessionError::Unconfigured => (
            LspOutcomeStatus::Unsupported,
            LspOutcomeCode::LanguageNotConfigured,
        ),
        LspSessionError::CapabilityUnavailable => (
            LspOutcomeStatus::Unsupported,
            LspOutcomeCode::CapabilityUnavailable,
        ),
        LspSessionError::Backend(code) => (LspOutcomeStatus::Error, code),
        LspSessionError::NotRenameable => return None,
    };
    Some(LspOutcome {
        status,
        code,
        language: None,
        command: None,
        operation: Some(LspOperation::Rename),
        path,
        phase: LspOutcomePhase::Runtime,
        message,
    })
}

fn workspace_edit_decode_message(error: WorkspaceEditDecodeError) -> String {
    match error {
        WorkspaceEditDecodeError::MissingText { path } => {
            format!("missing file text for {path}")
        }
        WorkspaceEditDecodeError::UnreadableFile { path, message } => {
            format!("could not read {path}: {message}")
        }
        WorkspaceEditDecodeError::UnsupportedWorkspaceEdit { message } => message,
        WorkspaceEditDecodeError::InvalidRange { path, message } => {
            format!("invalid range in {path}: {message}")
        }
        WorkspaceEditDecodeError::InvalidPath { uri } => {
            format!("invalid workspace edit URI: {uri}")
        }
    }
}

fn combined_preview(result: &WorkspaceApplyEditsResult) -> String {
    result
        .per_file
        .iter()
        .filter_map(|file| file.preview.as_deref())
        .collect::<Vec<_>>()
        .join("")
}

fn optional_combined_preview(result: &WorkspaceApplyEditsResult) -> Option<String> {
    let preview = combined_preview(result);
    if preview.is_empty() {
        None
    } else {
        Some(preview)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use std::collections::BTreeMap;
    use std::path::Path;

    use core_engine::adapters::config::lsp::{LspConfig, LspServerConfig};
    use core_engine::domain::code_intel::Range;

    use super::*;

    #[test]
    fn diagnostic_severities_use_shared_mcp_vocabulary() {
        let severities = [
            (Severity::Error, "error"),
            (Severity::Warning, "warning"),
            (Severity::Information, "info"),
            (Severity::Hint, "hint"),
        ];

        for (severity, expected) in severities {
            let diagnostic = Diagnostic {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 2,
                    },
                    end: Position {
                        line: 3,
                        character: 4,
                    },
                },
                severity,
                message: "diagnostic".to_owned(),
                source: Some("test".to_owned()),
                code: Some("T001".to_owned()),
            };

            assert_eq!(diagnostic_to_json(&diagnostic)["severity"], expected);
        }
    }

    fn lsp_fixture_config(workspace: &Path, providers_available: bool) -> LspConfig {
        let fixture = workspace.join("lsp-fixture.sh");
        let log = workspace.join("lsp-fixture.log");
        let providers = if providers_available {
            r#""definitionProvider":True,"referencesProvider":True,"hoverProvider":True,"implementationProvider":True,"renameProvider":True"#
        } else {
            ""
        };
        let script = r#"#!/usr/bin/env python3
import json
import sys

LOG = sys.argv[1]
PROVIDERS = {__PROVIDERS__}

def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    body = sys.stdin.buffer.read(int(headers["content-length"]))
    return json.loads(body.decode("utf-8"))

def send_message(payload):
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    with open(LOG, "a", encoding="utf-8") as log:
        log.write(json.dumps(message, separators=(",", ":")) + "\n")
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    text_document = params.get("textDocument") or {}
    uri = text_document.get("uri") or "file:///workspace/src/main.rs"
    if method == "initialize":
        send_message({"jsonrpc": "2.0", "id": request_id, "result": {"capabilities": PROVIDERS}})
    elif method == "textDocument/didOpen":
        send_message({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": []}})
        send_message({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"quiescent": True}})
    elif method in ("textDocument/definition", "textDocument/references", "textDocument/implementation"):
        send_message({"jsonrpc": "2.0", "id": request_id, "result": [{"uri": uri.replace("main.rs", "answer.rs"), "range": {"start": {"line": 7, "character": 2}, "end": {"line": 7, "character": 8}}}]})
    elif method == "textDocument/hover":
        send_message({"jsonrpc": "2.0", "id": request_id, "result": None})
    elif method == "textDocument/rename":
        send_message({"jsonrpc": "2.0", "id": request_id, "result": {"changes": {uri: [{"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 11}}, "newText": params.get("newName", "renamed")}]}}})
"#
        .replace("__PROVIDERS__", providers);
        std::fs::write(&fixture, script).expect("write LSP fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&fixture)
                .expect("read fixture permissions")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&fixture, permissions).expect("make LSP fixture executable");
        }

        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: fixture.to_string_lossy().into_owned(),
                extensions: vec!["rs".to_owned()],
                args: vec![log.to_string_lossy().into_owned()],
            },
        );
        LspConfig {
            servers,
            idle_timeout: None,
        }
    }

    #[test]
    fn f008_read_outcome_public_dispatch_preserves_legacy_fields_and_exact_outcomes() {
        let cases = [
            ("diagnostics", json!({ "path": "src/example.unknown" })),
            (
                "definition",
                json!({ "path": "src/example.unknown", "line": 3, "character": 5 }),
            ),
            (
                "references",
                json!({ "path": "src/example.unknown", "line": 3, "character": 5 }),
            ),
            (
                "hover",
                json!({ "path": "src/example.unknown", "line": 3, "character": 5 }),
            ),
            (
                "implementations",
                json!({ "path": "src/example.unknown", "line": 3, "character": 5 }),
            ),
        ];

        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(LspConfig::default(), workspace_root.clone(), None);
        let mut lines = std::iter::empty::<Result<String, std::io::Error>>();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        for (name, params) in cases {
            let result = dispatch(
                name,
                params,
                &mut pool,
                &workspace_root,
                &out,
                &mut lines,
                &mut next_id,
                &mut deferred,
            )
            .expect("unsupported requests remain successful tool results");
            assert!(!result["supported"].as_bool().unwrap());
            assert_eq!(result["outcome"]["operation"], name);
            assert_eq!(result["outcome"]["code"], "language_not_configured");
            assert_eq!(result["outcome"]["path"], "src/example.unknown");
        }
        assert!(
            out.lock().expect("host output lock").is_empty(),
            "unconfigured dispatch must not make HostCalls"
        );
    }

    #[test]
    fn f008_read_outcome_capability_absence_is_reported_by_public_dispatch_without_semantic_or_edit_calls()
     {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config(&workspace_root, false),
            workspace_root.clone(),
            None,
        );
        let mut lines = vec![Ok(
            json!({ "jsonrpc": "2.0", "id": 10_000, "result": "fn main() {}" }).to_string(),
        )]
        .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();
        let result = dispatch(
            "definition",
            json!({ "path": "src/main.rs", "line": 3, "character": 5 }),
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        )
        .expect("missing provider remains a successful unsupported result");
        assert_eq!(result["outcome"]["code"], "capability_unavailable");
        assert_eq!(result["outcome"]["operation"], "definition");
        assert_eq!(result["outcome"]["path"], "src/main.rs");
        let host_calls = String::from_utf8(out.lock().expect("host output lock").clone())
            .expect("host calls are UTF-8");
        assert!(
            host_calls.contains("workspace/readFile"),
            "initialization may require the file read"
        );
        assert!(
            !host_calls.contains("workspace/applyEdits"),
            "read-only dispatch must not apply edits"
        );
        let lsp_input = std::fs::read_to_string(workspace_root.join("lsp-fixture.log"))
            .expect("read LSP fixture log");
        assert!(
            !lsp_input.contains("textDocument/definition"),
            "missing provider must not issue its semantic request"
        );
    }

    #[test]
    fn f008_error_envelope_serializes_exact_typed_metadata() {
        let outcome = unsupported_read_only_outcome(
            LspOperation::Definition,
            LspOutcomeCode::CapabilityUnavailable,
            "src/main.rs".to_owned(),
        );
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));

        protocol::send_read_only_outcome_error(&out, &Some(json!(41)), outcome.clone());

        let bytes = out.lock().expect("output lock").clone();
        let envelope: Value = serde_json::from_slice(&bytes).expect("JSON-RPC error envelope");
        assert_eq!(
            envelope,
            json!({
                "jsonrpc": "2.0",
                "id": 41,
                "error": {
                    "code": -32000,
                    "message": outcome.message,
                    "data": { "outcome": outcome }
                }
            })
        );
    }

    #[test]
    fn f008_error_envelope_success_and_legacy_controls_are_unchanged() {
        let success_out = Arc::new(Mutex::new(Vec::<u8>::new()));
        protocol::send_response(
            &success_out,
            &Some(json!(7)),
            &extension_protocol::Response::ToolResult(json!({ "supported": true })),
        );
        let success: Value =
            serde_json::from_slice(&success_out.lock().expect("success output lock").clone())
                .expect("success envelope");
        assert_eq!(success["result"]["type"], "ToolResult");

        let legacy_out = Arc::new(Mutex::new(Vec::<u8>::new()));
        protocol::send_error(&legacy_out, &Some(json!(8)), -32000, "legacy failure");
        let legacy: Value =
            serde_json::from_slice(&legacy_out.lock().expect("legacy output lock").clone())
                .expect("legacy envelope");
        assert_eq!(legacy["error"]["message"], "legacy failure");
        assert!(legacy["error"].get("data").is_none());
    }

    #[test]
    fn f008_read_outcome_supported_results_remain_compatible_through_public_dispatch() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config(&workspace_root, true),
            workspace_root.clone(),
            None,
        );
        let mut lines = (10_000..10_005)
            .map(|id| {
                Ok(json!({ "jsonrpc": "2.0", "id": id, "result": "fn main() {}" }).to_string())
            })
            .collect::<Vec<_>>()
            .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();
        let location = json!([{ "path": "src/answer.rs", "line": 7, "character": 2, "endLine": 7, "endCharacter": 8 }]);
        let cases = [
            (
                "diagnostics",
                json!({ "path": "src/main.rs" }),
                json!({ "supported": true, "diagnostics": [] }),
            ),
            (
                "definition",
                json!({ "path": "src/main.rs", "line": 0, "character": 0 }),
                json!({ "supported": true, "locations": location }),
            ),
            (
                "references",
                json!({ "path": "src/main.rs", "line": 0, "character": 0 }),
                json!({ "supported": true, "locations": location }),
            ),
            (
                "hover",
                json!({ "path": "src/main.rs", "line": 0, "character": 0 }),
                json!({ "supported": true, "hover": null }),
            ),
            (
                "implementations",
                json!({ "path": "src/main.rs", "line": 0, "character": 0 }),
                json!({ "supported": true, "locations": location }),
            ),
        ];
        for (name, params, expected) in cases {
            let result = dispatch(
                name,
                params,
                &mut pool,
                &workspace_root,
                &out,
                &mut lines,
                &mut next_id,
                &mut deferred,
            )
            .unwrap_or_else(|error| {
                let log = std::fs::read_to_string(workspace_root.join("lsp-fixture.log"))
                    .unwrap_or_else(|_| "<unavailable>".to_owned());
                panic!("configured {name} must succeed: {error}; fixture log: {log}")
            });
            assert_eq!(
                result, expected,
                "{name} must preserve its supported payload exactly"
            );
        }
    }

    #[test]
    fn f008_rename_outcome_decorates_unsupported_language_and_classified_server_failures() {
        let workspace_root = PathBuf::from("/workspace");
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(LspConfig::default(), workspace_root.clone(), None);
        let mut lines = std::iter::empty::<Result<String, std::io::Error>>();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        let rename_error = dispatch(
            "rename",
            json!({
                "path": "src/main.rs",
                "line": 3,
                "character": 5,
                "new_name": "renamed"
            }),
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        )
        .expect("unsupported rename requests remain structured tool results");
        assert_eq!(
            rename_error,
            json!({
                "code": "unsupported_language",
                "message": "unsupported language for rename",
                "path": "src/main.rs",
                "outcome": {
                    "status": "unsupported",
                    "code": "language_not_configured",
                    "language": null,
                    "command": null,
                    "operation": "rename",
                    "path": "src/main.rs",
                    "phase": "runtime",
                    "message": "unsupported language for rename"
                }
            }),
            "rename must retain its legacy unsupported-language error payload"
        );

        let backend_codes = [
            (LspOutcomeCode::ServerMissing, "server_missing"),
            (LspOutcomeCode::InvalidCommand, "invalid_command"),
            (LspOutcomeCode::ServerNotExecutable, "server_not_executable"),
            (LspOutcomeCode::ServerLaunchFailed, "server_launch_failed"),
            (LspOutcomeCode::ServerCrashed, "server_crashed"),
            (LspOutcomeCode::ServerTimeout, "server_timeout"),
            (
                LspOutcomeCode::ServerTransportError,
                "server_transport_error",
            ),
            (
                LspOutcomeCode::ServerMalformedResponse,
                "server_malformed_response",
            ),
            (LspOutcomeCode::ServerError, "server_error"),
        ];
        for (code, wire_code) in backend_codes {
            let backend_error = lsp_session_error_value(
                LspSessionError::Backend(code),
                Some("src/main.rs".to_owned()),
            )
            .expect("classified server failure remains a structured rename result");
            assert_eq!(backend_error["code"], "backend_error");
            assert_eq!(backend_error["message"], "language server request failed");
            assert_eq!(backend_error["path"], "src/main.rs");
            assert_eq!(backend_error["outcome"]["status"], "error");
            assert_eq!(backend_error["outcome"]["code"], wire_code);
        }
    }

    #[test]
    fn f008_rename_outcome_distinguishes_capability_absence_and_preserves_domain_errors() {
        let path = Some("src/main.rs".to_owned());
        let language_absent = lsp_session_error_value(LspSessionError::Unconfigured, path.clone())
            .expect("language absence remains a structured rename result");
        let capability_absent =
            lsp_session_error_value(LspSessionError::CapabilityUnavailable, path.clone())
                .expect("capability absence remains a structured rename result");

        assert_eq!(language_absent["code"], "unsupported_language");
        assert_eq!(capability_absent["code"], "unsupported_language");
        assert_eq!(
            language_absent["outcome"]["code"],
            "language_not_configured"
        );
        assert_eq!(
            capability_absent["outcome"]["code"],
            "capability_unavailable"
        );

        let unchanged = [
            lsp_session_error_value(LspSessionError::NotRenameable, path)
                .expect("not-renameable remains a structured rename result"),
            rename_decode_error_value(WorkspaceEditDecodeError::UnsupportedWorkspaceEdit {
                message: "resource operations are unsupported".to_owned(),
            })
            .expect("unsupported workspace edits remain structured rename results"),
            rename_decode_error_value(WorkspaceEditDecodeError::InvalidRange {
                path: "src/main.rs".to_owned(),
                message: "range is outside the file".to_owned(),
            })
            .expect("invalid ranges remain structured rename results"),
        ];
        assert_eq!(unchanged[0]["code"], "not_renameable");
        assert_eq!(unchanged[1]["code"], "unsupported_workspace_edit");
        assert_eq!(unchanged[2]["code"], "invalid_range");
        assert!(unchanged.iter().all(|error| error.get("outcome").is_none()));
    }

    #[test]
    fn f008_rename_outcome_shared_fields_agree_and_error_paths_do_not_apply_edits() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config(&workspace_root, false),
            workspace_root.clone(),
            None,
        );
        let mut lines = vec![Ok(
            json!({ "jsonrpc": "2.0", "id": 10_000, "result": "fn main() {}" }).to_string(),
        )]
        .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        let error = dispatch(
            "rename",
            json!({
                "path": "src/main.rs",
                "line": 0,
                "character": 3,
                "new_name": "renamed"
            }),
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        )
        .expect("missing rename capability remains a structured tool result");

        assert_eq!(error["message"], error["outcome"]["message"]);
        assert_eq!(error["path"], error["outcome"]["path"]);
        assert_eq!(error["outcome"]["operation"], "rename");
        let host_calls = String::from_utf8(out.lock().expect("host output lock").clone())
            .expect("host calls are UTF-8");
        assert!(!host_calls.contains("workspace/applyEdits"));
    }

    #[test]
    fn f008_rename_success_payload_remains_compatible_through_public_dispatch() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config(&workspace_root, true),
            workspace_root.clone(),
            None,
        );
        let mut lines = vec![
            Ok(
                json!({ "jsonrpc": "2.0", "id": 10_000, "result": "fn old_name() {}\n" })
                    .to_string(),
            ),
            Ok(json!({
                "jsonrpc": "2.0",
                "id": 10_001,
                "result": {
                    "files_changed": 1,
                    "per_file": [{
                        "path": "src/main.rs",
                        "applied": true,
                        "edits_applied": 1,
                        "edits_skipped": 0,
                        "new_version": "abc123",
                        "preview": "fn renamed() {}\n"
                    }]
                }
            })
            .to_string()),
        ]
        .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        let result = dispatch(
            "rename",
            json!({
                "path": "src/main.rs",
                "line": 0,
                "character": 3,
                "new_name": "renamed"
            }),
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        )
        .expect("supported rename must preserve its successful tool result");

        assert_eq!(
            result,
            json!({
                "applied": true,
                "files_changed": 1,
                "spans": [{
                    "path": "src/main.rs",
                    "start_byte": 3,
                    "end_byte": 11,
                    "replacement": "renamed",
                    "base_hash": "406d35e3189e161844967405e4f1acdfca5c07b150d3b3322f256bf69d8d3e69"
                }],
                "preview": "fn renamed() {}\n",
                "per_file": [{
                    "path": "src/main.rs",
                    "applied": true,
                    "edits_applied": 1,
                    "edits_skipped": 0,
                    "new_version": "abc123",
                    "preview": "fn renamed() {}\n"
                }]
            }),
            "successful rename must retain its pre-F008 public payload"
        );
    }
}
