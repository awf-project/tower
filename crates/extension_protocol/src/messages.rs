//! Protocol message types: Request, Response, Event, HostCall.
//!
//! These types form the complete vocabulary of the host ↔ extension wire
//! protocol. All variants are `serde`-serializable to/from JSON.
//!
//! ## Serde tagging strategy
//!
//! `Request` and `Response` use **adjacently-tagged** encoding
//! (`#[serde(tag = "type", content = "data")]`). This avoids the serde
//! limitation where an internally-tagged enum cannot contain another
//! internally-tagged enum as a newtype variant (the `DeliverEvent(Event)` case),
//! and cannot contain non-map newtypes (`ToolResult(Value)`).
//!
//! `Event` and `HostCall` are leaves with only struct variants (or unit
//! variants), so they can safely use the simpler internally-tagged encoding.
//!
//! Wire shape examples:
//! - `{"type":"Initialize","data":{"protocol_version":1,"client_info":"host"}}`
//! - `{"type":"DeliverEvent","data":{"type":"FileIndexed","file_id":1,"path":"/x"}}`
//! - `{"type":"Shutdown"}`  ← unit variant: no `data` key emitted

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fault::ProtocolError;
use crate::manifest::{Capability, ToolDecl};

// ── Lifecycle types ───────────────────────────────────────────────────────────

/// Parameters sent by the host in the `initialize` request.
///
/// The extension must echo `protocol_version` back in [`InitResult`] so the
/// host can detect version mismatches early.
///
/// # Example
///
/// ```rust
/// use extension_protocol::{InitParams, PROTOCOL_VERSION};
///
/// let p = InitParams {
///     protocol_version: PROTOCOL_VERSION,
///     client_info: "tower-host/0.1.0".to_owned(),
/// };
/// assert_eq!(p.protocol_version, PROTOCOL_VERSION);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitParams {
    /// The protocol version the host speaks.
    pub protocol_version: u32,
    /// Human-readable host identifier for logging/diagnostics.
    pub client_info: String,
}

/// Result returned by the extension in response to `initialize`.
///
/// Lists the tools, events, and capabilities the extension declares. The host
/// merges declared tools into its MCP registry.
///
/// # Example
///
/// ```rust
/// use extension_protocol::{Capability, InitResult, ToolDecl};
///
/// let r = InitResult {
///     tools: vec![],
///     events: vec!["event/fileIndexed".to_owned()],
///     capabilities: vec![Capability::ReadFile],
/// };
/// assert_eq!(r.capabilities.len(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitResult {
    /// Tools contributed by this extension (merged into the MCP registry).
    pub tools: Vec<ToolDecl>,
    /// Event method names this extension subscribes to (e.g. `"event/fileIndexed"`).
    pub events: Vec<String>,
    /// Capabilities the extension requires from the host.
    pub capabilities: Vec<Capability>,
}

// ── Request ───────────────────────────────────────────────────────────────────

/// A message sent from the host to an extension.
///
/// Adjacently tagged: `{"type":"<Variant>","data":<payload>}` for variants with
/// data; `{"type":"<Variant>"}` for unit variants (e.g. `Shutdown`).
///
/// # Example
///
/// ```rust
/// use extension_protocol::{InitParams, Request, PROTOCOL_VERSION};
///
/// let req = Request::Initialize(InitParams {
///     protocol_version: PROTOCOL_VERSION,
///     client_info: "host".to_owned(),
/// });
/// let json = serde_json::to_string(&req).unwrap();
/// let back: Request = serde_json::from_str(&json).unwrap();
/// assert!(matches!(back, Request::Initialize(_)));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Request {
    /// Sent once after spawning the extension process; the extension must
    /// respond with [`Response::Initialized`].
    Initialize(InitParams),
    /// Ask the extension to invoke one of its declared tools.
    InvokeTool {
        /// Tool name as declared in the manifest.
        name: String,
        /// Caller-supplied parameters (JSON object).
        params: Value,
    },
    /// Deliver a workspace event to the extension (may also be a notification,
    /// i.e. no `id` in the JSON-RPC envelope).
    DeliverEvent(Event),
    /// Graceful shutdown; the extension should flush and exit.
    Shutdown,
}

// ── Response ──────────────────────────────────────────────────────────────────

/// A message returned from an extension to the host.
///
/// Adjacently tagged: `{"type":"<Variant>","data":<payload>}` for variants with
/// data; `{"type":"<Variant>"}` for unit variants (e.g. `Ack`).
///
/// # Example
///
/// ```rust
/// use extension_protocol::Response;
///
/// let resp = Response::Ack;
/// let json = serde_json::to_string(&resp).unwrap();
/// let back: Response = serde_json::from_str(&json).unwrap();
/// assert!(matches!(back, Response::Ack));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Response {
    /// Response to [`Request::Initialize`]: declares the extension's tools,
    /// subscribed events, and required capabilities.
    Initialized(InitResult),
    /// Response to [`Request::InvokeTool`]: the JSON result value.
    ToolResult(Value),
    /// Generic acknowledgement (e.g. for `deliverEvent` or `shutdown`).
    Ack,
    /// Protocol-level error; maps to JSON-RPC error object.
    Error(ProtocolError),
}

// ── Event ─────────────────────────────────────────────────────────────────────

/// Workspace events the host may deliver to a subscribed extension.
///
/// Internally tagged: all variants have struct fields, so no nesting issue.
///
/// # Example
///
/// ```rust
/// use extension_protocol::Event;
///
/// let e = Event::FileIndexed { file_id: 1, path: "/src/lib.rs".to_owned() };
/// let json = serde_json::to_string(&e).unwrap();
/// let back: Event = serde_json::from_str(&json).unwrap();
/// assert!(matches!(back, Event::FileIndexed { .. }));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// A file was indexed (or re-indexed) in the VFS.
    FileIndexed {
        /// Numeric file identifier from the VFS.
        file_id: u64,
        /// Workspace-relative path.
        path: String,
    },
    /// A file's content changed on disk and the VFS was updated.
    FileChanged {
        /// Numeric file identifier from the VFS.
        file_id: u64,
        /// Workspace-relative path.
        path: String,
    },
    /// A file was deleted from the workspace (spec 27).
    ///
    /// Extensions that track open documents (e.g. the LSP extension) should
    /// issue `textDocument/didClose` to the owning language server when they
    /// receive this event (EV1).
    FileDeleted {
        /// Workspace-relative path of the deleted file.
        path: String,
    },
}

// ── HostCall ──────────────────────────────────────────────────────────────────

/// Requests an extension may make back to the host (capability callbacks).
///
/// The sidecar adapter (spec 23) routes each variant to the appropriate
/// outbound port — `FileSystemPort`, `AstIndexPort`, `FormatQueuePort` — without
/// granting the extension any privileged access beyond what is declared in its
/// manifest's `capabilities`.
///
/// Internally tagged: all variants are either unit or have struct fields only.
///
/// # Example
///
/// ```rust
/// use extension_protocol::HostCall;
///
/// let call = HostCall::Log {
///     level: "info".to_owned(),
///     msg: "extension started".to_owned(),
/// };
/// let json = serde_json::to_string(&call).unwrap();
/// let back: HostCall = serde_json::from_str(&json).unwrap();
/// assert!(matches!(back, HostCall::Log { .. }));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HostCall {
    /// Read file contents from the workspace (`workspace/readFile`).
    ReadFile {
        /// Workspace-relative path.
        path: String,
    },
    /// List all indexed files in the workspace (`workspace/listFiles`).
    ListFiles,
    /// Retrieve a value from the AST index (`index/get`).
    IndexGet {
        /// Cache key (e.g. `"ast/<relative-path>"`).
        key: String,
    },
    /// Store a value in the AST index (`index/put`).
    IndexPut {
        /// Cache key.
        key: String,
        /// Raw bytes to store.
        bytes: Vec<u8>,
    },
    /// Request that the host format a file (`workspace/requestFormat`).
    RequestFormat {
        /// Workspace-relative path of the file to format.
        path: String,
    },
    /// Emit a log message through the host's logging subsystem (`log`).
    Log {
        /// Severity level: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`.
        level: String,
        /// Log message.
        msg: String,
    },
    /// Push a `notifications/resources/updated` notification to subscribed MCP
    /// clients (`notify/resourceUpdated`, spec 27 O1).
    ///
    /// The host immediately responds with `true` (best-effort) and re-emits the
    /// notification on the MCP transport's push channel. No round-trip blocking
    /// is needed from the extension's perspective.
    NotifyResourceUpdated {
        /// The resource URI that was updated (e.g. a `file://…` diagnostics URI).
        uri: String,
    },
}
