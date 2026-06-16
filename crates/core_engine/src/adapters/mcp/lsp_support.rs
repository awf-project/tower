//! Shared helpers for the LSP-backed MCP tools (`tower_lsp_diagnostics` and the
//! navigation tools). Extracted per the 14a review to remove the duplicated
//! argument-validation and file-read boilerplate across the tool registries.

#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::mcp::types::ToolError;
use crate::domain::RelativePath;
use crate::ports::PortError;

/// Extract a required string argument, or an [`ToolError::InvalidArgs`].
pub fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        ToolError::InvalidArgs(format!(
            "required field '{field}' is missing or not a string"
        ))
    })
}

/// Extract a required non-negative integer argument as `u32`.
pub fn require_u32(args: &Value, field: &str) -> Result<u32, ToolError> {
    args.get(field)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            ToolError::InvalidArgs(format!(
                "required field '{field}' is missing or not a non-negative integer"
            ))
        })
}

/// Read the current bytes of `path` from the shared [`EngineState`]'s
/// `FileSystemPort`, decoded lossily as UTF-8.
///
/// Maps [`PortError::NotFound`] to [`ToolError::ResourceNotFound`] so a missing
/// file is reported as a clean tool result, not a generic execution failure.
pub fn read_text(
    state: &Arc<RwLock<EngineState>>,
    path: &RelativePath,
) -> Result<String, ToolError> {
    let guard = state
        .read()
        .map_err(|_| ToolError::ExecutionFailed("engine state lock poisoned".to_owned()))?;
    let bytes = guard.fs.read(path).map_err(|e| match e {
        PortError::NotFound => {
            ToolError::ResourceNotFound(format!("file not found: {}", path.as_str()))
        }
        other => ToolError::ExecutionFailed(other.to_string()),
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
