//! Tower Extension Protocol
//!
//! Wire contract between the tower host and sidecar extensions. This crate
//! contains types and (de)serialization only — no process, filesystem, or
//! transport code.
//!
//! # Protocol overview
//!
//! The extension protocol is JSON-RPC 2.0 over stdio. Each message is a
//! newline-delimited JSON object conforming to the JSON-RPC 2.0 envelope:
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
//! ```
//!
//! When `id` is present the message is a request; when absent it is a
//! notification.
//!
//! # Lifecycle
//!
//! ```text
//! host  ──initialize──►  extension
//!       ◄──Initialized─
//!       ──invokeTool───►
//!       ◄──ToolResult──
//!       ──deliverEvent─►  (notification, no id)
//!       ◄──Ack─────────
//!       ──shutdown─────►
//! ```
//!
//! # Example — building an `initialize` request
//!
//! ```rust
//! use extension_protocol::{InitParams, Request, PROTOCOL_VERSION};
//!
//! let req = Request::Initialize(InitParams {
//!     protocol_version: PROTOCOL_VERSION,
//!     client_info: "tower-host/0.1.0".to_owned(),
//!     extension_config: None,
//! });
//! let json = serde_json::to_string(&req).unwrap();
//! assert!(json.contains("Initialize"));
//! ```

pub mod envelope;
pub mod fault;
pub mod manifest;
pub mod messages;

pub use fault::{ExtensionFault, ProtocolError};
pub use manifest::{Activation, Capability, EventKind, ExtensionManifest, ToolDecl};
pub use messages::{
    AnchoredSymbolEditError, AnchoredSymbolEditErrorCode, AnchoredSymbolEditRequest,
    AnchoredSymbolEditResult, ApplyEditsHostCallTextEdit, Event, HostCall, InitParams, InitResult,
    Location, LspImplementationRequest, LspImplementationResult, PerFileEditResult, RenameError,
    RenameErrorCode, RenamePreview, RenameRequest, RenameResult, Request, Response,
    SymbolCandidate, WorkspaceApplyEditsError, WorkspaceApplyEditsErrorCode,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult, WorkspaceEditSpan,
};

/// The current protocol version carried in every [`InitParams`].
///
/// Bump this constant whenever the request/response/event type layouts change
/// in a backward-incompatible way. The host (spec 22/23) compares the
/// `protocol_version` received in `Initialize` against this constant and
/// rejects extensions built against an incompatible version.
///
/// # Version history
///
/// - `1`: Initial version (spec 21).
pub const PROTOCOL_VERSION: u32 = 1;
