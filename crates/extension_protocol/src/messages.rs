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
///     extension_config: None,
/// };
/// assert_eq!(p.protocol_version, PROTOCOL_VERSION);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitParams {
    /// The protocol version the host speaks.
    pub protocol_version: u32,
    /// Human-readable host identifier for logging/diagnostics.
    pub client_info: String,
    /// Host-provided per-extension configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_config: Option<serde_json::Value>,
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
///     extension_config: None,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyEditsHostCallTextEdit {
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceApplyEditsRequest {
    pub edits: Vec<WorkspaceEditSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEditSpan {
    pub path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceApplyEditsResult {
    pub files_changed: usize,
    pub per_file: Vec<PerFileEditResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerFileEditResult {
    pub path: String,
    pub applied: bool,
    pub edits_applied: usize,
    pub edits_skipped: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkspaceApplyEditsError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceApplyEditsError {
    pub code: WorkspaceApplyEditsErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceApplyEditsErrorCode {
    CapabilityDenied,
    InvalidPath,
    #[serde(rename = "empty_edit_list")]
    EmptyEdits,
    #[serde(rename = "overlapping_edits")]
    OverlappingSpans,
    InvalidRange,
    #[serde(rename = "cas_conflict")]
    Conflict,
    #[serde(rename = "unsupported_operation")]
    Unsupported,
    #[serde(rename = "backend_error")]
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub line: u32,
    pub character: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    #[serde(rename = "endCharacter")]
    pub end_character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspImplementationRequest {
    pub path: String,
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspImplementationResult {
    pub supported: bool,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameRequest {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub new_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameResult {
    pub applied: bool,
    pub files_changed: usize,
    pub spans: Vec<WorkspaceEditSpan>,
    pub preview: Option<String>,
    pub per_file: Vec<PerFileEditResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenamePreview {
    pub spans: Vec<WorkspaceEditSpan>,
    pub preview: String,
    pub per_file: Vec<PerFileEditResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameError {
    pub code: RenameErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameErrorCode {
    NotRenameable,
    UnsupportedWorkspaceEdit,
    UnsupportedLanguage,
    InvalidRange,
    #[serde(rename = "backend_error")]
    BackendError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchoredSymbolEditRequest {
    pub path: String,
    pub symbol_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchoredSymbolEditResult {
    pub applied: bool,
    pub files_changed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<WorkspaceEditSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub per_file: Vec<PerFileEditResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchoredSymbolEditError {
    pub code: AnchoredSymbolEditErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<SymbolCandidate>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchoredSymbolEditErrorCode {
    NotFound,
    AmbiguousSymbol,
    UnsupportedLanguage,
    InvalidRange,
    #[serde(rename = "backend_error")]
    BackendError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolCandidate {
    pub path: String,
    pub kind: String,
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub end_row: usize,
}

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
    RequestApplyEdits {
        path: String,
        expected_version: String,
        edits: Vec<ApplyEditsHostCallTextEdit>,
        dry_run: bool,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ApplyEditsHostCallTextEdit, HostCall};

    #[test]
    fn request_apply_edits_hostcall_exists() {
        let call = HostCall::RequestApplyEdits {
            path: "src/lib.rs".to_owned(),
            expected_version: "abc123".to_owned(),
            edits: Vec::new(),
            dry_run: false,
        };

        assert!(matches!(call, HostCall::RequestApplyEdits { .. }));
    }

    #[test]
    fn request_apply_edits_hostcall_is_struct_variant_with_required_fields() {
        let call = HostCall::RequestApplyEdits {
            path: "src/lib.rs".to_owned(),
            expected_version: "abc123".to_owned(),
            edits: vec![ApplyEditsHostCallTextEdit {
                start_byte: 1,
                end_byte: 4,
                replacement: "let".to_owned(),
            }],
            dry_run: true,
        };

        assert!(matches!(call, HostCall::RequestApplyEdits { .. }));
        match call {
            HostCall::RequestApplyEdits {
                path,
                expected_version,
                edits,
                dry_run,
            } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(expected_version, "abc123");
                assert_eq!(
                    edits,
                    vec![ApplyEditsHostCallTextEdit {
                        start_byte: 1,
                        end_byte: 4,
                        replacement: "let".to_owned(),
                    }]
                );
                assert!(dry_run);
            }
            other => {
                let value = serde_json::to_value(other).expect("serialize HostCall variant");
                assert_eq!(value["type"], "RequestApplyEdits");
            }
        }
    }

    #[test]
    fn request_apply_edits_hostcall_preserves_existing_hostcall_serde_tagging_convention() {
        let call = HostCall::RequestApplyEdits {
            path: "src/lib.rs".to_owned(),
            expected_version: "abc123".to_owned(),
            edits: vec![ApplyEditsHostCallTextEdit {
                start_byte: 0,
                end_byte: 0,
                replacement: "use crate::x;\n".to_owned(),
            }],
            dry_run: false,
        };

        let value = serde_json::to_value(call).unwrap();

        assert_eq!(
            value,
            json!({
                "type": "RequestApplyEdits",
                "path": "src/lib.rs",
                "expected_version": "abc123",
                "edits": [
                    {
                        "start_byte": 0,
                        "end_byte": 0,
                        "replacement": "use crate::x;\n"
                    }
                ],
                "dry_run": false
            })
        );
        assert!(value.get("method").is_none());
    }

    #[test]
    fn apply_edits_hostcall_text_edit_exists_with_public_fields() {
        let edit = ApplyEditsHostCallTextEdit {
            start_byte: 3,
            end_byte: 8,
            replacement: "replacement".to_owned(),
        };

        assert_eq!(edit.start_byte, 3);
        assert_eq!(edit.end_byte, 8);
        assert_eq!(edit.replacement, "replacement");
    }

    #[test]
    fn public_wire_dto_names_are_exact_for_request_apply_edits() {
        fn accept_edit(_edit: ApplyEditsHostCallTextEdit) {}
        fn accept_host_call(_call: HostCall) {}

        accept_edit(ApplyEditsHostCallTextEdit {
            start_byte: 0,
            end_byte: 1,
            replacement: "x".to_owned(),
        });
        accept_host_call(HostCall::RequestApplyEdits {
            path: "src/lib.rs".to_owned(),
            expected_version: "abc123".to_owned(),
            edits: Vec::new(),
            dry_run: true,
        });
    }

    #[test]
    fn malformed_request_apply_edits_hostcall_payloads_fail_to_deserialize() {
        let missing_expected_version = json!({
            "type": "RequestApplyEdits",
            "path": "src/lib.rs",
            "edits": [],
            "dry_run": false
        });
        let missing_edits = json!({
            "type": "RequestApplyEdits",
            "path": "src/lib.rs",
            "expected_version": "abc123",
            "dry_run": false
        });
        let invalid_edit_range_field = json!({
            "type": "RequestApplyEdits",
            "path": "src/lib.rs",
            "expected_version": "abc123",
            "edits": [
                {
                    "start_byte": "not-a-usize",
                    "end_byte": 4,
                    "replacement": "let"
                }
            ],
            "dry_run": false
        });

        assert!(serde_json::from_value::<HostCall>(missing_expected_version).is_err());
        assert!(serde_json::from_value::<HostCall>(missing_edits).is_err());
        assert!(serde_json::from_value::<HostCall>(invalid_edit_range_field).is_err());
    }

    #[test]
    fn request_apply_edits_hostcall_round_trips() {
        let call = HostCall::RequestApplyEdits {
            path: "src/lib.rs".to_owned(),
            expected_version: "abc123".to_owned(),
            edits: vec![
                ApplyEditsHostCallTextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    replacement: "use crate::x;\n".to_owned(),
                },
                ApplyEditsHostCallTextEdit {
                    start_byte: 12,
                    end_byte: 17,
                    replacement: "value".to_owned(),
                },
            ],
            dry_run: true,
        };

        let json = serde_json::to_string(&call).unwrap();
        let round_tripped: HostCall = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped, call);
    }
}
