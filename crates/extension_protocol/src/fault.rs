//! Fault and error types for the extension protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// A typed fault that occurred in or with an extension.
///
/// Produced by the sidecar adapter (spec 23) and stored by the domain registry
/// (spec 22) to drive quarantine / restart policy (spec 24).
///
/// # Example
///
/// ```rust
/// use extension_protocol::ExtensionFault;
///
/// let f = ExtensionFault::Timeout;
/// let json = serde_json::to_string(&f).unwrap();
/// let back: ExtensionFault = serde_json::from_str(&json).unwrap();
/// assert!(matches!(back, ExtensionFault::Timeout));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Error)]
#[serde(tag = "type")]
pub enum ExtensionFault {
    /// The extension did not respond within the configured deadline.
    #[error("extension timed out")]
    Timeout,
    /// The extension process exited unexpectedly.
    #[error("extension crashed (exit code: {code:?})")]
    Crashed {
        /// OS exit code, if available.
        code: Option<i32>,
    },
    /// The extension sent or received a message that violated the protocol.
    #[error("extension protocol error: {message}")]
    ProtocolError {
        /// Description of the protocol violation.
        message: String,
    },
    /// The extension has been quarantined after repeated faults (spec 24).
    #[error("extension is quarantined")]
    Quarantined,
}

/// A JSON-RPC 2.0–style protocol error returned in [`crate::Response::Error`].
///
/// Standard error codes follow the JSON-RPC 2.0 specification:
/// - `-32700` Parse error
/// - `-32600` Invalid Request
/// - `-32601` Method not found
/// - `-32602` Invalid params
/// - `-32603` Internal error
///
/// # Example
///
/// ```rust
/// use extension_protocol::ProtocolError;
///
/// let e = ProtocolError {
///     code: -32600,
///     message: "Invalid Request".to_owned(),
///     data: None,
/// };
/// assert!(e.to_string().contains("Invalid Request"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Error)]
#[error("protocol error {code}: {message}")]
pub struct ProtocolError {
    /// JSON-RPC error code.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Optional additional error data (JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
