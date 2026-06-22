//! Domain-facing types for the MCP tool registry surface (spec 10a/10b).
//!
//! This module contains only the types used by [`ToolRegistry`] implementations
//! and their callers. The hand-rolled JSON-RPC wire types (JsonRpcRequest,
//! JsonRpcResponse, JsonRpcError, RequestId) have been removed: rmcp 1.7 now
//! owns all wire-format concerns.
//!
//! # Remaining types
//!
//! - [`ToolDesc`] — describes a single tool (name, description, JSON schema).
//! - [`ToolError`] — typed errors returned from [`ToolRegistry::call`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Tool registry surface types ───────────────────────────────────────────────

/// Description of a single tool exposed via `tools/list`.
///
/// The registry returns a `Vec<ToolDesc>` so the dispatcher can advertise tools
/// to MCP clients without knowing anything about their implementations.
///
/// # Extension
///
/// 10b adds native tool descriptors; 28 merges sidecar extension tool
/// descriptors through the same surface. Neither changes this type — they
/// only provide different `ToolRegistry` implementors.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDesc {
    /// Unique tool name (used as the key in `tools/call` dispatch).
    pub name: String,
    /// Human-readable description shown to the MCP client.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    ///
    /// An empty object schema (`{}`) means "no parameters required". Clients
    /// may use this for validation before sending `tools/call`.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Errors that a `ToolRegistry` implementation can return from `call`.
///
/// The MCP handler maps these to the appropriate rmcp response shapes.
#[derive(Debug)]
pub enum ToolError {
    /// The named tool does not exist in this registry.
    NotFound(String),
    /// The arguments provided are invalid or missing required fields.
    ///
    /// Maps to a protocol-level `-32602 InvalidParams` error.
    InvalidArgs(String),
    /// The tool executed but encountered a runtime error.
    ExecutionFailed(String),
    /// The resource targeted by the tool does not exist (e.g. file not found).
    ///
    /// Distinct from [`ToolError::NotFound`] (which is for an unknown *tool*
    /// name). Reported as `is_error:true` in the content array.
    ResourceNotFound(String),
    /// Optimistic concurrency precondition failed: the caller's expected version
    /// did not match the file's current version at write time (spec 18).
    PreconditionFailed(String),
}

impl core::fmt::Display for ToolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound(name) => write!(f, "tool not found: {name}"),
            Self::InvalidArgs(msg) => write!(f, "invalid arguments: {msg}"),
            Self::ExecutionFailed(msg) => write!(f, "tool execution failed: {msg}"),
            Self::ResourceNotFound(msg) => write!(f, "resource not found: {msg}"),
            Self::PreconditionFailed(msg) => write!(f, "precondition failed: {msg}"),
        }
    }
}

impl core::error::Error for ToolError {}
