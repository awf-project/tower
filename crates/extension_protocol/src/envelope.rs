//! JSON-RPC 2.0 envelope types.
//!
//! The wire format for the tower extension protocol is JSON-RPC 2.0 over
//! newline-delimited stdio:
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{...}}
//! ```
//!
//! - `id` present → request (host expects a response with the same `id`).
//! - `id` absent → notification (no response expected).
//!
//! This module provides the raw envelope types for framing. Higher-level code
//! uses [`crate::Request`] / [`crate::Response`] for typed dispatch.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 request or notification envelope.
///
/// # Example
///
/// ```rust
/// use extension_protocol::envelope::JsonRpcRequest;
/// use serde_json::json;
///
/// let req = JsonRpcRequest {
///     jsonrpc: "2.0".to_owned(),
///     id: Some(json!(1)),
///     method: "initialize".to_owned(),
///     params: Some(json!({"protocol_version": 1, "client_info": "host"})),
/// };
/// let json = serde_json::to_string(&req).unwrap();
/// assert!(json.contains("initialize"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Request identifier. `None` for notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Method name (e.g. `"initialize"`, `"invokeTool"`).
    pub method: String,
    /// Method parameters (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response envelope.
///
/// # Example
///
/// ```rust
/// use extension_protocol::envelope::JsonRpcResponse;
/// use serde_json::json;
///
/// let resp = JsonRpcResponse {
///     jsonrpc: "2.0".to_owned(),
///     id: json!(1),
///     result: Some(json!({"type":"Ack"})),
///     error: None,
/// };
/// let json = serde_json::to_string(&resp).unwrap();
/// assert!(json.contains("Ack"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Must match the `id` from the corresponding request.
    pub id: Value,
    /// Success result. Exactly one of `result` or `error` must be present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error object. Exactly one of `result` or `error` must be present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object, embedded in [`JsonRpcResponse::error`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code (see [`crate::ProtocolError`] for standard values).
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Optional additional data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
