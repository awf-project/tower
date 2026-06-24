//! Tool dispatch for the LSP sidecar extension (spec 27).
//!
//! Implements the four tools: `diagnostics`, `definition`, `references`, `hover`.
//! Each tool reads file content via `workspace/readFile` HostCall (using the
//! extension protocol capability), then delegates to the session pool.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::{Diagnostic, Hover, Location, Position, Severity};
use core_engine::ports::CodeIntelError;
use extension_protocol::HostCall;
use serde_json::{Value, json};

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
