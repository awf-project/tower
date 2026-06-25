//! Tool dispatch for the LSP sidecar extension (spec 27).
//!
//! Implements the LSP tools: `diagnostics`, `definition`, `references`, `hover`,
//! `implementations`, and `rename`.
//! Each tool reads file content via `workspace/readFile` HostCall (using the
//! extension protocol capability), then delegates to the session pool.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::{Diagnostic, Hover, Location, Position, Severity};
use core_engine::ports::{CodeIntelError, RenameNavigationError};
use extension_protocol::{
    HostCall, LspImplementationRequest, LspImplementationResult, RenameError, RenameErrorCode,
    RenamePreview, RenameRequest, RenameResult, WorkspaceApplyEditsRequest,
    WorkspaceApplyEditsResult,
};
use serde_json::{Value, json};

use crate::lsp_adapter::decode::WorkspaceEditDecodeError;
use crate::protocol::{self, HostCallIdAllocator, QueuedFrame};
use crate::session::LspSessionPool;

/// Dispatch an `invokeTool` call to the appropriate LSP tool.
///
/// Returns `Ok(Value)` on success, `Err(String)` on failure.
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
) -> Result<Value, String>
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
        "rename" => rename(params, pool, workspace_root, out, lines, next_id, deferred),
        other => Err(format!("unknown LSP tool: {other}")),
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
) -> Result<Value, String>
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
        return Ok(json!({ "supported": false, "diagnostics": [] }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;

    match pool.check(&rel, &text) {
        Ok(diags) => {
            let arr: Vec<Value> = diags.iter().map(diagnostic_to_json).collect();
            Ok(json!({ "supported": true, "diagnostics": arr }))
        }
        Err(CodeIntelError::Unsupported) => Ok(json!({ "supported": false, "diagnostics": [] })),
        Err(CodeIntelError::Backend(msg)) => Err(msg),
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
) -> Result<Value, String>
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
        return Ok(json!({ "supported": false, "locations": [] }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;
    let pos = Position { line, character };

    match pool.definition(&rel, &text, pos) {
        Ok(locs) => {
            let arr: Vec<Value> = locs.iter().map(location_to_json).collect();
            Ok(json!({ "supported": true, "locations": arr }))
        }
        Err(CodeIntelError::Unsupported) => Ok(json!({ "supported": false, "locations": [] })),
        Err(CodeIntelError::Backend(msg)) => Err(msg),
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
) -> Result<Value, String>
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
        return Ok(json!({ "supported": false, "locations": [] }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;
    let pos = Position { line, character };

    match pool.references(&rel, &text, pos) {
        Ok(locs) => {
            let arr: Vec<Value> = locs.iter().map(location_to_json).collect();
            Ok(json!({ "supported": true, "locations": arr }))
        }
        Err(CodeIntelError::Unsupported) => Ok(json!({ "supported": false, "locations": [] })),
        Err(CodeIntelError::Backend(msg)) => Err(msg),
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
) -> Result<Value, String>
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
        return Ok(json!({ "supported": false, "hover": null }));
    }

    let text = read_file(path, out, lines, next_id, deferred)?;
    let pos = Position { line, character };

    match pool.hover(&rel, &text, pos) {
        Ok(Some(h)) => Ok(json!({ "supported": true, "hover": hover_to_json(&h) })),
        Ok(None) => Ok(json!({ "supported": true, "hover": null })),
        Err(CodeIntelError::Unsupported) => Ok(json!({ "supported": false, "hover": null })),
        Err(CodeIntelError::Backend(msg)) => Err(msg),
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
) -> Result<Value, String>
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
        })
        .map_err(|e| format!("serialize LspImplementationResult failed: {e}"));
    }

    let text = read_file(&request.path, out, lines, next_id, deferred)?;
    let position = Position {
        line: request.line,
        character: request.character,
    };

    match pool.implementations(&rel, &text, position) {
        Ok(locations) => {
            let locations = locations.iter().map(protocol_location).collect::<Vec<_>>();
            serde_json::to_value(LspImplementationResult {
                supported: true,
                locations,
            })
            .map_err(|e| format!("serialize LspImplementationResult failed: {e}"))
        }
        Err(CodeIntelError::Unsupported) => serde_json::to_value(LspImplementationResult {
            supported: false,
            locations: Vec::new(),
        })
        .map_err(|e| format!("serialize LspImplementationResult failed: {e}")),
        Err(CodeIntelError::Backend(msg)) => Err(msg),
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
        return rename_error_value(
            RenameErrorCode::UnsupportedLanguage,
            "unsupported language for rename",
            Some(request.path),
        );
    }

    let text = read_file(&request.path, out, lines, next_id, deferred)?;
    let position = Position {
        line: request.line,
        character: request.character,
    };

    let raw_edit = match pool.rename(&rel, &text, position, &request.new_name) {
        Ok(raw_edit) => raw_edit,
        Err(error) => return rename_navigation_error_value(error, Some(request.path)),
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

fn rename_navigation_error_value(
    error: RenameNavigationError,
    path: Option<String>,
) -> Result<Value, String> {
    match error {
        RenameNavigationError::NotRenameable => {
            rename_error_value(RenameErrorCode::NotRenameable, "not renameable", path)
        }
        RenameNavigationError::UnsupportedLanguage => rename_error_value(
            RenameErrorCode::UnsupportedLanguage,
            "unsupported language for rename",
            path,
        ),
        RenameNavigationError::Backend(message) => {
            rename_error_value(RenameErrorCode::BackendError, message, path)
        }
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
    })
    .map_err(|e| format!("serialize RenameError failed: {e}"))
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
}
