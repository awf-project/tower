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
use extension_protocol::messages::{
    PathRenameError, PathRenameErrorCode, PathRenamePlan, PathRenamePlanningRequest,
    PathRenamePlanningResult, SafeDeleteSemanticOutcome, SafeDeleteSemanticRequest,
};
use extension_protocol::{
    HostCall, LspImplementationRequest, LspImplementationResult, LspOperation, LspOutcome,
    LspOutcomeCode, LspOutcomePhase, LspOutcomeStatus, RenameError, RenameErrorCode, RenamePreview,
    RenameRequest, RenameResult, TypeHierarchyQuery, TypeHierarchyResult,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult,
};
use serde_json::{Value, json};

use crate::lsp_adapter::LspSessionError;
use crate::lsp_adapter::decode::WorkspaceEditDecodeError;
use crate::protocol::{self, HostCallIdAllocator, QueuedFrame};
use crate::session::LspSessionPool;

#[derive(Debug)]
pub(crate) enum ToolDispatchError {
    InvalidParams(String),
    Legacy(String),
    ReadOnlyOutcome(LspOutcome),
}

impl fmt::Display for ToolDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidParams(message) => formatter.write_str(message),
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

fn unsupported_type_hierarchy(outcome: LspOutcome) -> Result<Value, ToolDispatchError> {
    serde_json::to_value(TypeHierarchyResult {
        supported: false,
        supertypes: Vec::new(),
        subtypes: Vec::new(),
        outcome: Some(outcome),
    })
    .map_err(|error| error.to_string().into())
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
        "type_hierarchy" => {
            type_hierarchy(params, pool, workspace_root, out, lines, next_id, deferred)
        }
        "rename" => {
            rename(params, pool, workspace_root, out, lines, next_id, deferred).map_err(Into::into)
        }
        "plan_path_rename" => {
            plan_path_rename(params, pool, workspace_root, out, lines, next_id, deferred)
        }
        "plan_safe_delete" => {
            plan_safe_delete(params, pool, workspace_root, out, lines, next_id, deferred)
        }
        other => Err(format!("unknown LSP tool: {other}").into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_safe_delete<W, R>(
    params: Value,
    pool: &mut LspSessionPool,
    _workspace_root: &std::path::Path,
    out: &Arc<Mutex<W>>,
    lines: &mut R,
    next_id: &mut HostCallIdAllocator,
    deferred: &mut VecDeque<QueuedFrame>,
) -> Result<Value, ToolDispatchError>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let request: SafeDeleteSemanticRequest = serde_json::from_value(params).map_err(|_| {
        ToolDispatchError::InvalidParams(
            "invalid_params: plan_safe_delete params must match SafeDeleteSemanticRequest"
                .to_owned(),
        )
    })?;
    let mut content_cache: HashMap<String, String> = HashMap::new();
    let mut reader = |path: &str| {
        let normalized_path = normalize_workspace_read_path(path)?;
        if let Some(content) = content_cache.get(&normalized_path) {
            return Ok(content.clone());
        }
        let content =
            read_file(&normalized_path, out, lines, next_id, deferred).map_err(|error| {
                if error == "workspace path resolves outside the workspace" {
                    LspSessionError::WorkspaceExternal
                } else {
                    LspSessionError::Backend(LspOutcomeCode::ServerError)
                }
            })?;
        content_cache.insert(normalized_path, content.clone());
        Ok(content)
    };
    let outcome = pool
        .plan_safe_delete(&request, &mut reader)
        .unwrap_or_else(normalize_safe_delete_pool_error);
    serde_json::to_value(outcome).map_err(|error| error.to_string().into())
}

fn normalize_workspace_read_path(requested_path: &str) -> Result<String, LspSessionError> {
    use std::path::Component;

    let requested = std::path::Path::new(requested_path);
    let mut normalized = PathBuf::new();
    for component in requested.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(LspSessionError::WorkspaceExternal);
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(LspSessionError::WorkspaceExternal);
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(LspSessionError::WorkspaceExternal);
    }

    // The host readFile capability owns canonical/symlink containment. The
    // sidecar must not inspect workspace paths through ambient filesystem I/O.

    normalized
        .to_str()
        .map(|path| path.replace('\\', "/"))
        .ok_or(LspSessionError::WorkspaceExternal)
}

fn normalize_safe_delete_pool_error(error: LspSessionError) -> SafeDeleteSemanticOutcome {
    match error {
        LspSessionError::Unconfigured
        | LspSessionError::CapabilityUnavailable
        | LspSessionError::NotRenameable
        | LspSessionError::Backend(
            LspOutcomeCode::LanguageNotConfigured | LspOutcomeCode::CapabilityUnavailable,
        ) => SafeDeleteSemanticOutcome::CapabilityUnavailable,
        LspSessionError::Backend(LspOutcomeCode::ServerTimeout) => {
            SafeDeleteSemanticOutcome::BackendTimeout
        }
        LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse) => {
            SafeDeleteSemanticOutcome::MalformedResponse
        }
        LspSessionError::Backend(_) => SafeDeleteSemanticOutcome::BackendCrashed,
        LspSessionError::WorkspaceExternal => SafeDeleteSemanticOutcome::WorkspaceExternal,
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_path_rename<W, R>(
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
    let request: PathRenamePlanningRequest = serde_json::from_value(params).map_err(|_| {
        ToolDispatchError::InvalidParams(
            "invalid_params: plan_path_rename params must match PathRenamePlanningRequest"
                .to_owned(),
        )
    })?;

    let result = match pool.plan_path_rename(&request) {
        Ok(raw_edit) => {
            let mut resolved: HashMap<String, String> = HashMap::new();
            match pool.decode_rename_workspace_edit(raw_edit, |path| {
                if let Some(text) = resolved.get(path) {
                    return Ok(text.clone());
                }
                let text = read_file(path, out, lines, next_id, deferred).map_err(|_| {
                    WorkspaceEditDecodeError::UnreadableFile {
                        path: path.to_owned(),
                        message: "workspace file could not be read".to_owned(),
                    }
                })?;
                resolved.insert(path.to_owned(), text.clone());
                Ok(text)
            }) {
                Ok(edits) => PathRenamePlanningResult {
                    plan: Some(PathRenamePlan {
                        moves: request.moves,
                        edits,
                        semantic_policy: request.semantic_policy,
                    }),
                    error: None,
                },
                Err(error) => PathRenamePlanningResult {
                    plan: None,
                    error: Some(path_rename_decode_error(error)),
                },
            }
        }
        Err(error) => PathRenamePlanningResult {
            plan: None,
            error: Some(error),
        },
    };

    if result.error.is_some() {
        protocol::send_log(out, next_id, "warn", "LSP path-rename planning failed");
    }
    serde_json::to_value(result).map_err(|error| error.to_string().into())
}

fn path_rename_decode_error(error: WorkspaceEditDecodeError) -> PathRenameError {
    let code = match error {
        WorkspaceEditDecodeError::UnsupportedWorkspaceEdit { .. } => {
            PathRenameErrorCode::SemanticUnsupported
        }
        _ => PathRenameErrorCode::SemanticBackend,
    };
    PathRenameError {
        code,
        path: None,
        message: if code == PathRenameErrorCode::SemanticUnsupported {
            "Semantic rename is unavailable"
        } else {
            "Semantic planning failed"
        }
        .to_owned(),
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
fn type_hierarchy<W, R>(
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
    let query: TypeHierarchyQuery = serde_json::from_value(params)
        .map_err(|error| format!("bad TypeHierarchyQuery: {error}"))?;
    let rel = RelativePath::new(&query.path);
    if !pool.serves(&rel) {
        return unsupported_type_hierarchy(unsupported_read_only_outcome(
            LspOperation::TypeHierarchy,
            LspOutcomeCode::LanguageNotConfigured,
            query.path,
        ));
    }

    let text = read_file(&query.path, out, lines, next_id, deferred)?;
    match pool.type_hierarchy(&query, &text) {
        Ok(result) => serde_json::to_value(result).map_err(|error| error.to_string().into()),
        Err(LspSessionError::Unconfigured) => {
            unsupported_type_hierarchy(unsupported_read_only_outcome(
                LspOperation::TypeHierarchy,
                LspOutcomeCode::LanguageNotConfigured,
                query.path,
            ))
        }
        Err(LspSessionError::CapabilityUnavailable) => {
            let mut outcome = unsupported_read_only_outcome(
                LspOperation::TypeHierarchy,
                LspOutcomeCode::CapabilityUnavailable,
                query.path,
            );
            if let Some((language, command)) = pool.binding_for(&rel) {
                outcome.language = Some(language.to_owned());
                outcome.command = Some(command.to_owned());
            }
            unsupported_type_hierarchy(outcome)
        }
        Err(error) => Err(read_only_session_error(
            error,
            LspOperation::TypeHierarchy,
            query.path,
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
        error @ (LspSessionError::WorkspaceExternal | LspSessionError::Backend(_)) => {
            rename_error_value_with_outcome(
                RenameErrorCode::BackendError,
                "language server request failed",
                path,
                error,
            )
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
        LspSessionError::WorkspaceExternal => {
            (LspOutcomeStatus::Error, LspOutcomeCode::ServerError)
        }
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
    use std::io::{BufRead, Write as _};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    use std::collections::BTreeMap;
    use std::path::Path;

    use core_engine::adapters::config::lsp::{LspConfig, LspServerConfig};
    use core_engine::domain::code_intel::Range;
    use core_engine::ports::{
        CodeIntelError, CodeIntelligencePort, DocumentSyncPort, NavigationPort,
        PrepareRenameResult, RenameNavigationError,
    };

    use super::*;

    const TEST_DIAGNOSTICS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

    /// A pool seam that supplies an edit requiring the production read resolver.
    /// The dispatch path, decoder, and HostCall transport remain real.
    struct PlanningSession {
        edit: crate::lsp_adapter::RawWorkspaceEdit,
    }

    impl crate::lsp_adapter::pool::PooledSession for PlanningSession {
        fn plan_path_rename(
            &self,
            _: &extension_protocol::messages::PathRenamePlanningRequest,
        ) -> Result<
            crate::lsp_adapter::RawWorkspaceEdit,
            extension_protocol::messages::PathRenameError,
        > {
            Ok(self.edit.clone())
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn shared_state(&self) -> crate::lsp_adapter::SharedState {
            Arc::new((
                Mutex::new(crate::lsp_adapter::SessionStateInner::default()),
                std::sync::Condvar::new(),
            ))
        }

        fn diagnostics_for(&self, _: &str) -> Vec<core_engine::domain::code_intel::Diagnostic> {
            Vec::new()
        }

        fn type_hierarchy_lsp(
            &self,
            _: &extension_protocol::TypeHierarchyQuery,
            _: &str,
        ) -> Result<extension_protocol::TypeHierarchyResult, crate::lsp_adapter::LspSessionError>
        {
            Err(crate::lsp_adapter::LspSessionError::CapabilityUnavailable)
        }

        fn rename(
            &self,
            _: &RelativePath,
            _: &str,
            _: Position,
            _: &str,
        ) -> Result<crate::lsp_adapter::RawWorkspaceEdit, RenameNavigationError> {
            Err(RenameNavigationError::UnsupportedLanguage)
        }

        fn kill_for_test(&self) {}
    }

    impl CodeIntelligencePort for PlanningSession {
        fn check(&self, _: &RelativePath, _: &str) -> Result<Vec<Diagnostic>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }
    }

    impl NavigationPort for PlanningSession {
        fn definition(
            &self,
            _: &RelativePath,
            _: &str,
            _: Position,
        ) -> Result<Vec<Location>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }
        fn implementations(
            &self,
            _: &RelativePath,
            _: &str,
            _: Position,
        ) -> Result<Vec<Location>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }
        fn references(
            &self,
            _: &RelativePath,
            _: &str,
            _: Position,
        ) -> Result<Vec<Location>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }
        fn hover(
            &self,
            _: &RelativePath,
            _: &str,
            _: Position,
        ) -> Result<Option<Hover>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }
        fn document_symbols(
            &self,
            _: &RelativePath,
            _: &str,
        ) -> Result<Vec<core_engine::domain::code_intel::Symbol>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }
        fn prepare_rename(
            &self,
            _: &RelativePath,
            _: &str,
            _: Position,
        ) -> Result<PrepareRenameResult, RenameNavigationError> {
            Err(RenameNavigationError::UnsupportedLanguage)
        }
    }

    impl DocumentSyncPort for PlanningSession {
        fn serves(&self, _: &RelativePath) -> bool {
            true
        }
        fn document_opened(&self, _: &RelativePath, _: &str) {}
        fn document_changed(&self, _: &RelativePath, _: &str) {}
        fn document_closed(&self, _: &RelativePath) {}
    }

    struct PlanningSpawner(Arc<PlanningSession>);

    impl crate::lsp_adapter::pool::SessionSpawner for PlanningSpawner {
        fn spawn(
            &self,
            _: &LspServerConfig,
            _: &str,
            _: PathBuf,
            _: &Path,
            _: Option<crate::lsp_adapter::DiagnosticsSender>,
            _: std::time::Duration,
        ) -> Result<
            Arc<dyn crate::lsp_adapter::pool::PooledSession>,
            crate::lsp_adapter::LspSessionError,
        > {
            Ok(self.0.clone())
        }
    }

    fn path_rename_fixture_config(workspace: &Path, mode: &str) -> LspConfig {
        let fixture = fixture_lsp_server::binary_path();
        let log = workspace.join("path-rename-lsp.jsonl");
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: fixture.to_string_lossy().into_owned(),
                extensions: vec!["rs".to_owned()],
                args: vec![mode.to_owned(), log.to_string_lossy().into_owned()],
            },
        );
        LspConfig {
            servers,
            idle_timeout: None,
        }
    }

    fn path_rename_params() -> Value {
        json!({
            "moves": [
                { "source": "src/old", "destination": "src/new", "kind": "directory" },
                { "source": "src/old/lib.rs", "destination": "src/new/lib.rs", "kind": "file" }
            ],
            "semantic_policy": "required"
        })
    }

    fn dispatch_path_rename(
        workspace: &Path,
        mode: &str,
        params: Value,
        input: Vec<Result<String, std::io::Error>>,
    ) -> (
        Result<Value, ToolDispatchError>,
        String,
        VecDeque<QueuedFrame>,
    ) {
        let workspace_root = workspace.to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            path_rename_fixture_config(workspace, mode),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let mut lines = input.into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();
        let result = dispatch(
            "plan_path_rename",
            params,
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        );
        let output = String::from_utf8(out.lock().expect("host output lock").clone())
            .expect("HostCall output is UTF-8");
        (result, output, deferred)
    }

    #[test]
    fn path_rename_advertises_the_distinct_internal_planning_request_schema() {
        let manifest: toml::Value = toml::from_str(include_str!("../extension.toml"))
            .expect("LSP extension manifest is valid TOML");
        let tools = manifest["tools"].as_array().expect("manifest tools array");
        let declaration = tools
            .iter()
            .find(|tool| tool["name"].as_str() == Some("plan_path_rename"))
            .expect("internal path-rename planner is advertised");
        let schema: Value = serde_json::from_str(
            declaration["schema_json"]
                .as_str()
                .expect("planner schema is JSON text"),
        )
        .expect("planner schema is valid JSON");

        assert_eq!(schema["required"], json!(["moves", "semantic_policy"]));
        assert!(schema["properties"].get("source").is_none());
        assert!(schema["properties"].get("destination").is_none());
        assert_eq!(schema["properties"]["moves"]["type"], "array");
        assert_eq!(
            schema["properties"]["semantic_policy"]["enum"],
            json!(["none", "best_effort", "required"])
        );
        assert!(
            include_str!("main.rs").contains("name: \"plan_path_rename\".to_owned()"),
            "initialize must advertise the same internal planning tool"
        );
    }

    #[test]
    fn path_rename_dispatches_exact_name_and_separates_invalid_params_from_planner_outcomes() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let (supported, _, _) = dispatch_path_rename(
            workspace.path(),
            "path_rename_supported",
            path_rename_params(),
            Vec::new(),
        );
        let supported = supported.expect("planner outcome is a successful tool payload");
        assert_eq!(supported["plan"]["moves"], path_rename_params()["moves"]);
        assert_eq!(supported["plan"]["semantic_policy"], "required");
        assert_eq!(supported["plan"]["edits"], json!([]));
        assert!(supported.get("error").is_none());

        let (invalid, _, _) = dispatch_path_rename(
            workspace.path(),
            "path_rename_supported",
            json!({ "moves": "not-an-array", "semantic_policy": "required" }),
            Vec::new(),
        );
        match invalid {
            Err(ToolDispatchError::InvalidParams(message)) => assert_eq!(
                message,
                "invalid_params: plan_path_rename params must match PathRenamePlanningRequest",
                "malformed planner arguments must retain the invocation invalid-params classification"
            ),
            other => panic!("expected an invalid-params invocation error, got {other:?}"),
        }

        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            LspConfig::default(),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let mut lines = std::iter::empty::<Result<String, std::io::Error>>();
        let mut ids = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();
        assert!(
            dispatch(
                "planpathrename",
                path_rename_params(),
                &mut pool,
                &workspace_root,
                &out,
                &mut lines,
                &mut ids,
                &mut deferred,
            )
            .is_err()
        );
    }

    #[test]
    fn path_rename_host_calls_are_read_only_and_never_apply_or_move_files() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let target = workspace.path().join("src/old/lib.rs");
        let raw_edit = serde_json::from_value(json!({
            "changes": {
                format!("file://{}", target.display()): [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    },
                    "newText": "renamed"
                }]
            }
        }))
        .expect("fixture workspace edit is valid");
        let session = Arc::new(PlanningSession { edit: raw_edit });
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: "unused-by-planning-spawner".to_owned(),
                extensions: vec!["rs".to_owned()],
                args: Vec::new(),
            },
        );
        let config = LspConfig {
            servers,
            idle_timeout: None,
        };
        let inner = crate::lsp_adapter::SessionPool::with_spawner(
            config,
            workspace.path().to_path_buf(),
            Arc::new(PlanningSpawner(session)),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let mut pool =
            LspSessionPool::with_session_pool_for_test(inner, workspace.path().to_path_buf());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut lines = vec![Ok(json!({
            "jsonrpc": "2.0", "id": 10_000, "result": "original contents"
        })
        .to_string())]
        .into_iter();
        let mut ids = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        let result = dispatch(
            "plan_path_rename",
            path_rename_params(),
            &mut pool,
            &workspace.path().to_path_buf(),
            &out,
            &mut lines,
            &mut ids,
            &mut deferred,
        )
        .expect("a resolved semantic edit is a successful planning payload");
        assert_eq!(result["plan"]["edits"][0]["path"], "src/old/lib.rs");
        assert_eq!(result["plan"]["edits"][0]["replacement"], "renamed");
        let host_calls = out.lock().expect("host output lock").clone();
        let host_calls = String::from_utf8(host_calls).expect("HostCalls are UTF-8");
        let frames = host_calls
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).expect("captured HostCall is valid JSON")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            frames,
            vec![json!({
                "jsonrpc": "2.0", "id": 10_000, "method": "workspace/readFile",
                "params": { "type": "ReadFile", "path": "src/old/lib.rs" }
            })]
        );
        assert!(deferred.is_empty());
    }

    fn dispatch_planning_edit(
        workspace: &Path,
        edit: crate::lsp_adapter::RawWorkspaceEdit,
        input: Vec<Result<String, std::io::Error>>,
    ) -> (Value, Vec<Value>) {
        let session = Arc::new(PlanningSession { edit });
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: "unused-by-planning-spawner".to_owned(),
                extensions: vec!["rs".to_owned()],
                args: Vec::new(),
            },
        );
        let inner = crate::lsp_adapter::SessionPool::with_spawner(
            LspConfig {
                servers,
                idle_timeout: None,
            },
            workspace.to_path_buf(),
            Arc::new(PlanningSpawner(session)),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let mut pool = LspSessionPool::with_session_pool_for_test(inner, workspace.to_path_buf());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut lines = input.into_iter();
        let mut ids = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();
        let result = dispatch(
            "plan_path_rename",
            path_rename_params(),
            &mut pool,
            &workspace.to_path_buf(),
            &out,
            &mut lines,
            &mut ids,
            &mut deferred,
        )
        .expect("decoder failures remain successful structured payloads");
        assert!(deferred.is_empty());
        let frames = String::from_utf8(out.lock().expect("host output lock").clone())
            .expect("HostCalls are UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("captured HostCall is valid JSON"))
            .collect();
        (result, frames)
    }

    #[test]
    fn path_rename_rejects_external_edit_targets_without_reading_them() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let edit = serde_json::from_value(json!({
            "changes": {
                "file:///outside/private.rs": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "newText": "private replacement"
                }]
            }
        }))
        .expect("fixture workspace edit is valid");

        let (result, frames) = dispatch_planning_edit(workspace.path(), edit, Vec::new());

        assert_eq!(result["error"]["code"], "semantic_backend");
        assert!(result.get("plan").is_none());
        assert!(
            frames
                .iter()
                .all(|frame| frame["method"] != "workspace/readFile")
        );
    }

    #[test]
    fn path_rename_returns_structured_error_when_edit_target_cannot_be_read() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let target = workspace.path().join("src/old/lib.rs");
        let edit = serde_json::from_value(json!({
            "changes": {
                format!("file://{}", target.display()): [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "newText": "renamed"
                }]
            }
        }))
        .expect("fixture workspace edit is valid");
        let input = vec![Ok(json!({
            "jsonrpc": "2.0",
            "id": 10_000,
            "error": {"code": -32000, "message": "PRIVATE_HOST_FAILURE"}
        })
        .to_string())];

        let (result, frames) = dispatch_planning_edit(workspace.path(), edit, input);

        assert_eq!(result["error"]["code"], "semantic_backend");
        assert!(result.get("plan").is_none());
        assert_eq!(frames[0]["method"], "workspace/readFile");
        assert_eq!(frames[0]["params"]["path"], "src/old/lib.rs");
        assert!(!result.to_string().contains("PRIVATE_HOST_FAILURE"));
    }

    #[test]
    fn path_rename_logs_at_the_real_host_call_boundary_exclude_private_backend_data() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let absolute_path = workspace.path().join("PRIVATE_ABSOLUTE_PATH");
        let external_uri = "PRIVATE_EXTERNAL_URI/secret.rs";
        let replacement = "PRIVATE_REPLACEMENT_TEXT";
        let contents = "PRIVATE_FILE_CONTENTS";
        let backend = "PRIVATE_BACKEND_DETAIL";
        let params = json!({
            "moves": [{
                "source": external_uri,
                "destination": format!("{replacement}/{contents}/{backend}.rs"),
                "kind": "file"
            }],
            "semantic_policy": "required"
        });
        let (result, host_calls, _) = dispatch_path_rename(
            workspace.path(),
            "path_rename_malformed",
            params,
            Vec::new(),
        );
        let payload = result.expect("backend failures remain successful structured payloads");
        assert_eq!(payload["error"]["code"], "semantic_backend");

        let logs = host_calls
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).expect("captured HostCall is valid JSON")
            })
            .filter(|frame| frame["method"] == "log")
            .collect::<Vec<_>>();
        assert!(
            !logs.is_empty(),
            "planner failure must emit a sanitized log HostCall"
        );
        for log in logs {
            let rendered = log.to_string();
            for secret in [
                absolute_path.to_string_lossy().as_ref(),
                external_uri,
                replacement,
                contents,
                backend,
            ] {
                assert!(!rendered.contains(secret), "log HostCall leaked {secret}");
            }
        }
    }

    #[test]
    fn path_rename_capability_waits_fifo_queue_and_replay_inbound_host_requests() {
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let first = json!({
            "jsonrpc": "2.0", "id": 41, "method": "deliverEvent",
            "params": { "data": { "type": "FileChanged", "path": "src/a.rs" } }
        });
        let second = json!({
            "jsonrpc": "2.0", "id": 42, "method": "shutdown", "params": {}
        });
        let mut lines = vec![
            Ok(first.to_string()),
            Ok(second.to_string()),
            Ok(json!({ "jsonrpc": "2.0", "id": 10_000, "result": "contents" }).to_string()),
        ]
        .into_iter();
        let mut ids = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        assert_eq!(
            read_file("src/a.rs", &out, &mut lines, &mut ids, &mut deferred).unwrap(),
            "contents"
        );
        assert_eq!(deferred.pop_front(), protocol::frame_from_envelope(first));
        assert_eq!(deferred.pop_front(), protocol::frame_from_envelope(second));
        assert!(deferred.is_empty());
    }

    #[test]
    fn path_rename_initialize_contract_requires_no_capability_host_call_after_initialized() {
        let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/lsp_extension");
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("prebuilt LSP sidecar binary is available for protocol testing");
        let mut stdin = child.stdin.take().expect("sidecar stdin");
        writeln!(
            stdin,
            "{}",
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocol_version": 1,
                    "client_info": "path-rename-test",
                    "extension_config": {
                        "servers": {},
                        "idle_timeout_secs": null,
                        "diagnostics_timeout_secs": 1
                    }
                }
            })
        )
        .expect("send initialize request");
        let stdout = child.stdout.take().expect("sidecar stdout");
        let first = std::io::BufReader::new(stdout)
            .lines()
            .next()
            .expect("initialize emits a response")
            .expect("read sidecar frame");
        let first: Value = serde_json::from_str(&first).expect("valid JSON-RPC frame");
        assert_eq!(first["result"]["type"], "Initialized");
        assert!(
            first["result"]["data"]["tools"]
                .as_array()
                .expect("initialized tools")
                .iter()
                .any(|tool| tool["name"] == "plan_path_rename")
        );
        let _ = child.kill();
        let _ = child.wait();
    }

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
        lsp_fixture_config_with_backend_failure(workspace, providers_available, false)
    }

    fn lsp_fixture_config_with_backend_failure(
        workspace: &Path,
        providers_available: bool,
        backend_failure: bool,
    ) -> LspConfig {
        let fixture = fixture_lsp_server::binary_path();
        let log = workspace.join("lsp-fixture.log");
        let mode = if !providers_available {
            "dispatch_unavailable"
        } else if backend_failure {
            "dispatch_backend_error"
        } else {
            "dispatch_normal"
        };

        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: fixture.to_string_lossy().into_owned(),
                extensions: vec!["rs".to_owned()],
                args: vec![mode.to_owned(), log.to_string_lossy().into_owned()],
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
        let mut pool = LspSessionPool::new(
            LspConfig::default(),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
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
    fn type_hierarchy_unconfigured_dispatch_is_unsupported_without_host_call() {
        let workspace_root = PathBuf::from("/workspace");
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            LspConfig::default(),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let mut lines = std::iter::empty::<Result<String, std::io::Error>>();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        for direction in ["supertypes", "subtypes", "both"] {
            let result = dispatch(
                "type_hierarchy",
                json!({ "path": "src/main.unknown", "line": 0, "character": 7, "direction": direction }),
                &mut pool,
                &workspace_root,
                &out,
                &mut lines,
                &mut next_id,
                &mut deferred,
            )
            .expect("unconfigured hierarchy remains a successful unsupported result");
            assert_eq!(
                result,
                json!({
                    "supported": false,
                    "supertypes": [],
                    "subtypes": [],
                    "outcome": {
                        "status": "unsupported",
                        "code": "language_not_configured",
                        "language": null,
                        "command": null,
                        "operation": "type_hierarchy",
                        "path": "src/main.unknown",
                        "phase": "runtime",
                        "message": "No language server is configured for this file."
                    }
                })
            );
        }
        assert!(out.lock().expect("host output lock").is_empty());
    }

    fn parse_fixture_messages(log: &str) -> Vec<Value> {
        log.lines()
            .map(|line| serde_json::from_str(line).expect("valid fixture JSON envelope"))
            .collect()
    }

    #[test]
    fn type_hierarchy_configured_dispatch_uses_one_read_only_host_call() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config(&workspace_root, true),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let source = "private immutable hierarchy source";
        let mut lines = vec![Ok(
            json!({ "jsonrpc": "2.0", "id": 10_000, "result": source }).to_string(),
        )]
        .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        let result = dispatch(
            "type_hierarchy",
            json!({ "path": "src/main.rs", "line": 0, "character": 7, "direction": "both" }),
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        )
        .expect("configured hierarchy dispatch succeeds");

        assert_eq!(result["supported"], true);
        assert_eq!(result["supertypes"][0]["name"], "Parent");
        assert_eq!(result["subtypes"][0]["name"], "Grandchild");
        assert!(result["outcome"].is_null());
        let host_calls = parse_fixture_messages(
            &String::from_utf8(out.lock().expect("host output lock").clone())
                .expect("host calls are UTF-8"),
        );
        assert_eq!(
            host_calls,
            vec![json!({
                "jsonrpc": "2.0",
                "id": 10_000,
                "method": "workspace/readFile",
                "params": { "type": "ReadFile", "path": "src/main.rs" }
            })],
            "the hierarchy invocation may issue exactly its one read-only HostCall"
        );
        assert!(!result.to_string().contains(source));
        assert!(deferred.is_empty());
    }

    #[test]
    fn type_hierarchy_dispatch_preserves_snapshot_and_replays_the_deferred_event() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config(&workspace_root, true),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let source = "private immutable hierarchy source";
        let deferred_content = "queued event content must not replace the active snapshot";
        std::fs::create_dir_all(workspace_root.join("src"))
            .expect("create fixture source directory");
        std::fs::write(workspace_root.join("src/main.rs"), deferred_content)
            .expect("write deferred-event fixture content");
        let queued_event = QueuedFrame::Request {
            id: Some(json!(77)),
            method: "deliverEvent".to_owned(),
            params: json!({ "data": { "type": "FileChanged", "path": "src/main.rs" } }),
        };
        let mut lines = vec![
            Ok(json!({
                "jsonrpc": "2.0",
                "id": 77,
                "method": "deliverEvent",
                "params": { "data": { "type": "FileChanged", "path": "src/main.rs" } }
            })
            .to_string()),
            Ok(json!({ "jsonrpc": "2.0", "id": 10_000, "result": source }).to_string()),
        ]
        .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        let result = dispatch(
            "type_hierarchy",
            json!({ "path": "src/main.rs", "line": 0, "character": 7, "direction": "both" }),
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        )
        .expect("configured hierarchy dispatch succeeds");

        assert!(!result.to_string().contains(source));
        let queued = deferred.pop_front().expect("the inbound event is deferred");
        assert_eq!(
            queued, queued_event,
            "the exact inbound fileChanged frame remains FIFO-queued until after ToolResult"
        );
        assert!(deferred.is_empty());

        let lsp_messages = parse_fixture_messages(
            &std::fs::read_to_string(workspace_root.join("lsp-fixture.log"))
                .expect("fixture records LSP traffic"),
        );
        let did_open = lsp_messages
            .iter()
            .find(|message| message["method"] == "textDocument/didOpen")
            .expect("active hierarchy query opens its document");
        assert_eq!(
            did_open["params"]["textDocument"]["text"], source,
            "the immutable readFile snapshot is forwarded to the LSP query"
        );

        let QueuedFrame::Request { method, params, .. } = queued else {
            panic!("the deferred frame must remain a host request");
        };
        assert_eq!(method, "deliverEvent");
        crate::handle_event(params, &mut pool, &workspace_root);
        let log_path = workspace_root.join("lsp-fixture.log");
        let updated_log = (0..100)
            .find_map(|_| {
                let log = std::fs::read_to_string(&log_path).ok()?;
                if log.contains(deferred_content) {
                    Some(log)
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    None
                }
            })
            .expect("fixture records the deferred event content");
        let updated_messages = parse_fixture_messages(&updated_log);
        let active_open = updated_messages
            .iter()
            .position(|message| {
                message["method"] == "textDocument/didOpen"
                    && message["params"]["textDocument"]["text"] == source
            })
            .expect("active query completes with its immutable source");
        let deferred_open = updated_messages
            .iter()
            .position(|message| {
                message["method"] == "textDocument/didOpen"
                    && message["params"]["textDocument"]["text"] == deferred_content
            })
            .expect("queued event is processed only after the active result");
        assert!(active_open < deferred_open);
    }

    #[test]
    fn type_hierarchy_unavailable_provider_is_unsupported_for_every_direction() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config(&workspace_root, false),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let mut lines = (10_000..10_003)
            .map(|id| {
                Ok(json!({ "jsonrpc": "2.0", "id": id, "result": "struct Child;" }).to_string())
            })
            .collect::<Vec<_>>()
            .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();

        let command = lsp_fixture_config(&workspace_root, false).servers["rust"]
            .command
            .clone();
        for direction in ["supertypes", "subtypes", "both"] {
            let result = dispatch(
                "type_hierarchy",
                json!({ "path": "src/main.rs", "line": 0, "character": 7, "direction": direction }),
                &mut pool,
                &workspace_root,
                &out,
                &mut lines,
                &mut next_id,
                &mut deferred,
            )
            .expect("missing provider remains a successful unsupported result");
            assert_eq!(
                result,
                json!({
                    "supported": false,
                    "supertypes": [],
                    "subtypes": [],
                    "outcome": {
                        "status": "unsupported",
                        "code": "capability_unavailable",
                        "language": "rust",
                        "command": command.clone(),
                        "operation": "type_hierarchy",
                        "path": "src/main.rs",
                        "phase": "runtime",
                        "message": "The configured language server does not provide this operation."
                    }
                }),
                "{direction} must expose the complete identical unsupported envelope"
            );
        }
    }

    #[test]
    fn type_hierarchy_invalid_query_is_legacy_error_before_host_call() {
        let workspace_root = PathBuf::from("/workspace");
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            LspConfig::default(),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let mut next_id = HostCallIdAllocator::new(10_000);

        for params in [
            json!({ "line": 0, "character": 7, "direction": "both" }),
            json!({ "path": "src/main.rs", "line": "zero", "character": 7, "direction": "sideways" }),
        ] {
            let mut lines = std::iter::empty::<Result<String, std::io::Error>>();
            let mut deferred = VecDeque::new();
            let error = dispatch(
                "type_hierarchy",
                params,
                &mut pool,
                &workspace_root,
                &out,
                &mut lines,
                &mut next_id,
                &mut deferred,
            )
            .expect_err("invalid hierarchy query is rejected");
            assert!(
                matches!(error, ToolDispatchError::Legacy(ref message) if message.starts_with("bad TypeHierarchyQuery:"))
            );
        }
        assert!(out.lock().expect("host output lock").is_empty());
    }

    #[test]
    fn type_hierarchy_backend_error_preserves_read_only_outcome_metadata() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let workspace_root = workspace.path().to_path_buf();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut pool = LspSessionPool::new(
            lsp_fixture_config_with_backend_failure(&workspace_root, true, true),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
        let secret = "opaque backend data must stay private";
        let mut lines = vec![Ok(
            json!({ "jsonrpc": "2.0", "id": 10_000, "result": secret }).to_string(),
        )]
        .into_iter();
        let mut next_id = HostCallIdAllocator::new(10_000);
        let mut deferred = VecDeque::new();
        let error = dispatch(
            "type_hierarchy",
            json!({ "path": "src/main.rs", "line": 0, "character": 7, "direction": "supertypes" }),
            &mut pool,
            &workspace_root,
            &out,
            &mut lines,
            &mut next_id,
            &mut deferred,
        )
        .expect_err("backend hierarchy failure maps through public dispatch");
        let error_text = error.to_string();
        let ToolDispatchError::ReadOnlyOutcome(outcome) = error else {
            panic!("backend failures use the typed read-only outcome variant");
        };
        assert_eq!(outcome.status, LspOutcomeStatus::Error);
        assert_eq!(outcome.code, LspOutcomeCode::ServerError);
        assert_eq!(outcome.operation, Some(LspOperation::TypeHierarchy));
        assert_eq!(outcome.path.as_deref(), Some("src/main.rs"));
        assert_eq!(outcome.phase, LspOutcomePhase::Runtime);
        assert_eq!(outcome.message, "language server request failed");
        assert_eq!(outcome.language.as_deref(), Some("rust"));
        assert!(
            outcome
                .command
                .as_deref()
                .is_some_and(|command| !command.is_empty())
        );
        assert!(!error_text.contains(secret));
        let host_frames = String::from_utf8(out.lock().expect("host output lock").clone())
            .expect("host calls are UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("valid HostCall envelope"))
            .collect::<Vec<_>>();
        assert!(
            host_frames
                .iter()
                .all(|frame| { frame["method"] != "log" && !frame.to_string().contains(secret) })
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
            TEST_DIAGNOSTICS_DEADLINE,
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
            TEST_DIAGNOSTICS_DEADLINE,
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
        let mut pool = LspSessionPool::new(
            LspConfig::default(),
            workspace_root.clone(),
            None,
            TEST_DIAGNOSTICS_DEADLINE,
        );
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
            TEST_DIAGNOSTICS_DEADLINE,
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
            TEST_DIAGNOSTICS_DEADLINE,
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
