//! JSON-RPC 2.0 wire types for the MCP stdio transport (spec 10a).
//!
//! All structures serialise/deserialise directly to JSON. The domain layer
//! never imports these — they are presentation-layer concerns only.
//!
//! # Framing
//!
//! Each message is a single JSON object terminated by `\n`. The spec mandates
//! newline-delimited framing (U1/EV1). No content-length headers (unlike LSP).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── JSON-RPC 2.0 identifiers ─────────────────────────────────────────────────

/// A JSON-RPC 2.0 request identifier.
///
/// Per spec §4: MUST be a String, Number, or `null`. Null is permitted when
/// no response is expected (notifications). We represent all three variants
/// so we can round-trip `id` faithfully.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// String identifier (most common in MCP clients).
    Str(String),
    /// Numeric identifier (integer).
    Num(i64),
    /// Null identifier (notifications — no response expected).
    Null,
}

// ── Inbound ──────────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request as received over stdin.
///
/// We use `#[serde(default)]` on `id` so notifications (which omit `id`) also
/// parse successfully as `RequestId::Null`.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    /// Always "2.0".
    pub jsonrpc: String,
    /// The RPC method name (e.g. `"initialize"`, `"tools/list"`).
    pub method: String,
    /// Positional or named parameters. Absent means no parameters.
    #[serde(default)]
    pub params: Value,
    /// Request identifier. Missing in notifications.
    #[serde(default)]
    pub id: Option<RequestId>,
}

// ── Outbound ─────────────────────────────────────────────────────────────────

/// A successful JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    /// Always "2.0".
    pub jsonrpc: &'static str,
    /// The result payload (any JSON value).
    pub result: Value,
    /// Echoed from the matching request.
    pub id: Option<RequestId>,
}

impl JsonRpcResponse {
    /// Construct a success response.
    pub fn ok(id: Option<RequestId>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            result,
            id,
        }
    }
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    /// Always "2.0".
    pub jsonrpc: &'static str,
    /// The error payload.
    pub error: ErrorObject,
    /// Echoed from the request (null if the request id could not be parsed).
    pub id: Option<RequestId>,
}

impl JsonRpcError {
    /// Build a `ParseError` response (malformed JSON frame, spec AC5).
    pub fn parse_error(id: Option<RequestId>) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32700,
                message: "Parse error".to_owned(),
                data: None,
            },
            id,
        }
    }

    /// Build a `MethodNotFound` response (the RPC method itself is unknown, spec AC4).
    ///
    /// Use this **only** when the JSON-RPC method (e.g. `"tools/call"`) is not
    /// recognised — not when a tool name inside `tools/call` is missing.
    pub fn method_not_found(id: Option<RequestId>, method: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32601,
                message: format!("Method not found: '{method}'"),
                data: None,
            },
            id,
        }
    }

    /// Build a `ToolNotFound` response (a named tool is absent from the registry).
    ///
    /// Uses application-level error code `-32001`. JSON-RPC 2.0 reserves
    /// `-32000` to `-32099` for server-defined errors; `-32601` is strictly for
    /// an unknown RPC method name, not for an unknown tool inside `tools/call`.
    pub fn tool_not_found(id: Option<RequestId>, name: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32001,
                message: format!("Tool not found: '{name}'"),
                data: None,
            },
            id,
        }
    }

    /// Build an `InvalidParams` response (e.g. missing required field, spec UN1).
    pub fn invalid_params(id: Option<RequestId>, detail: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32602,
                message: format!("Invalid params: {detail}"),
                data: None,
            },
            id,
        }
    }

    /// Build an `InternalError` response (tool execution failed).
    pub fn internal_error(id: Option<RequestId>, detail: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32603,
                message: format!("Internal error: {detail}"),
                data: None,
            },
            id,
        }
    }

    /// Build a `ResourceNotFound` response (domain resource does not exist).
    ///
    /// Uses server-defined code `-32002` (JSON-RPC 2.0 range `-32000..=-32099`).
    /// Clients branch on this stable code to show a "not found" message without
    /// parsing the error string (spec 10b AC5).
    pub fn resource_not_found(id: Option<RequestId>, detail: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32002,
                message: format!("Resource not found: {detail}"),
                data: None,
            },
            id,
        }
    }

    /// Build a `PreconditionFailed` response (optimistic CAS conflict, spec 18).
    ///
    /// Uses server-defined code `-32009` (JSON-RPC 2.0 range `-32000..=-32099`).
    /// Clients branch on this stable code to detect "file changed since you read
    /// it" without parsing the error string.
    pub fn precondition_failed(id: Option<RequestId>, detail: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32009,
                message: format!("Precondition failed: {detail}"),
                data: None,
            },
            id,
        }
    }

    /// Build an `InvalidRequest` response (e.g. wrong `jsonrpc` version, JSON-RPC 2.0 §4).
    pub fn invalid_request(id: Option<RequestId>, detail: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            error: ErrorObject {
                code: -32600,
                message: format!("Invalid Request: {detail}"),
                data: None,
            },
            id,
        }
    }
}

/// The `error` object nested inside a JSON-RPC error response.
#[derive(Debug, Serialize)]
pub struct ErrorObject {
    /// Standard JSON-RPC error code.
    pub code: i32,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data (omitted when `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ── Tool registry surface types ───────────────────────────────────────────────

/// Description of a single tool exposed via `tools/list`.
///
/// The registry returns a `Vec<ToolDesc>` so the dispatcher can advertise tools
/// to MCP clients without knowing anything about their implementations.
///
/// # Extension
///
/// 10b adds native tool descriptors; 12b merges plugin tool descriptors through
/// the same surface. Neither changes this type — they only provide different
/// `ToolRegistry` implementors.
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
/// The MCP dispatcher maps these to the appropriate JSON-RPC error codes.
#[derive(Debug)]
pub enum ToolError {
    /// The named tool does not exist in this registry.
    ///
    /// Maps to application error code `-32001` (server-defined, JSON-RPC 2.0
    /// range `-32000`..`-32099`). Not `-32601` (`MethodNotFound`), which is
    /// reserved for an unknown RPC method name, not an unknown tool name.
    NotFound(String),
    /// The arguments provided are invalid or missing required fields.
    ///
    /// Maps to JSON-RPC `-32602 InvalidParams`.
    InvalidArgs(String),
    /// The tool executed but encountered a runtime error.
    ///
    /// Maps to JSON-RPC `-32603 InternalError`.
    ExecutionFailed(String),
    /// The resource targeted by the tool does not exist (e.g. file not found).
    ///
    /// Distinct from [`ToolError::NotFound`] (which is for an unknown *tool*
    /// name). Maps to server-defined code `-32002` so clients can branch on
    /// "resource not found" without parsing the error message (spec 10b AC5).
    ResourceNotFound(String),
    /// Optimistic concurrency precondition failed: the caller's expected version
    /// did not match the file's current version at write time (spec 18).
    ///
    /// Maps to server-defined code `-32009`. The message carries both the
    /// `expected` and `actual` hashes so the client can surface a meaningful
    /// "file changed since you read it" diagnostic without parsing the string.
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
