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

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::fault::ProtocolError;
use crate::manifest::{Capability, ToolDecl};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspOutcome {
    pub status: LspOutcomeStatus,
    pub code: LspOutcomeCode,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub language: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub command: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub operation: Option<LspOperation>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub path: Option<String>,
    pub phase: LspOutcomePhase,
    pub message: String,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

fn deserialize_non_null_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspOutcomeStatus {
    Unsupported,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspOutcomeCode {
    LanguageNotConfigured,
    CapabilityUnavailable,
    ServerMissing,
    InvalidCommand,
    ServerNotExecutable,
    ServerLaunchFailed,
    ServerCrashed,
    ServerTimeout,
    ServerTransportError,
    ServerMalformedResponse,
    ServerError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspOperation {
    Diagnostics,
    Definition,
    References,
    Hover,
    Implementations,
    TypeHierarchy,
    Rename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspOutcomePhase {
    Startup,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeHierarchyDirection {
    Supertypes,
    Subtypes,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeHierarchyQuery {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub direction: TypeHierarchyDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeHierarchyRange {
    pub line: u32,
    pub character: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    #[serde(rename = "endCharacter")]
    pub end_character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TypeHierarchyItem {
    pub name: String,
    pub kind: String,
    pub detail: Option<String>,
    pub range: TypeHierarchyRange,
    #[serde(rename = "selectionRange")]
    pub selection_range: TypeHierarchyRange,
    pub path: Option<String>,
    #[serde(rename = "externalUri")]
    pub external_uri: Option<String>,
    pub external: bool,
}

#[derive(Deserialize)]
struct TypeHierarchyItemWire {
    name: String,
    kind: String,
    detail: Option<String>,
    range: TypeHierarchyRange,
    #[serde(rename = "selectionRange")]
    selection_range: TypeHierarchyRange,
    path: Option<String>,
    #[serde(rename = "externalUri")]
    external_uri: Option<String>,
    external: bool,
}

impl TryFrom<TypeHierarchyItemWire> for TypeHierarchyItem {
    type Error = &'static str;

    fn try_from(wire: TypeHierarchyItemWire) -> Result<Self, Self::Error> {
        let TypeHierarchyItemWire {
            name,
            kind,
            detail,
            range,
            selection_range,
            path,
            external_uri,
            external,
        } = wire;
        match (external, path.as_ref(), external_uri.as_ref()) {
            (false, Some(_), None) | (true, None, Some(_)) => Ok(Self {
                name,
                kind,
                detail,
                range,
                selection_range,
                path,
                external_uri,
                external,
            }),
            _ => Err(
                "type hierarchy item location must match external: path for workspace items, externalUri for external items",
            ),
        }
    }
}

impl<'de> Deserialize<'de> for TypeHierarchyItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TypeHierarchyItemWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeHierarchyResult {
    pub supported: bool,
    pub supertypes: Vec<TypeHierarchyItem>,
    pub subtypes: Vec<TypeHierarchyItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LspOutcome>,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteRequest {
    pub path: String,
    pub symbol_name: String,
    pub kind: Option<String>,
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteRequestWire {
    path: String,
    symbol_name: String,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null_optional",
        skip_serializing_if = "Option::is_none"
    )]
    kind: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

impl Serialize for SafeDeleteRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_safe_delete_identity(&self.path, &self.symbol_name, self.kind.as_deref())
            .map_err(serde::ser::Error::custom)?;
        SafeDeleteRequestWire {
            path: self.path.clone(),
            symbol_name: self.symbol_name.clone(),
            kind: self.kind.clone(),
            dry_run: self.dry_run,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteRequestWire::deserialize(deserializer)?;
        validate_safe_delete_identity(&wire.path, &wire.symbol_name, wire.kind.as_deref())
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            path: wire.path,
            symbol_name: wire.symbol_name,
            kind: wire.kind,
            dry_run: wire.dry_run,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeDeleteStatus {
    Blocked,
    Previewed,
    Applied,
    PartialCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeDeleteBlockingReason {
    NotFound,
    AmbiguousSymbol,
    InvalidDeclaration,
    CapabilityUnavailable,
    ReferencesUnresolved,
    LiveReferences,
    CleanupIncomplete,
    InvalidCleanup,
    CasConflict,
    BackendError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeDeletePosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteByteSpan {
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteByteSpanWire {
    start_byte: usize,
    end_byte: usize,
}

impl Serialize for SafeDeleteByteSpan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_byte_range(self.start_byte, self.end_byte).map_err(serde::ser::Error::custom)?;
        SafeDeleteByteSpanWire {
            start_byte: self.start_byte,
            end_byte: self.end_byte,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteByteSpan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteByteSpanWire::deserialize(deserializer)?;
        validate_byte_range(wire.start_byte, wire.end_byte).map_err(serde::de::Error::custom)?;
        Ok(Self {
            start_byte: wire.start_byte,
            end_byte: wire.end_byte,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteDeclarationRequest {
    pub path: String,
    pub symbol_name: String,
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteDeclarationRequestWire {
    path: String,
    symbol_name: String,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null_optional",
        skip_serializing_if = "Option::is_none"
    )]
    kind: Option<String>,
}

impl Serialize for SafeDeleteDeclarationRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_safe_delete_identity(&self.path, &self.symbol_name, self.kind.as_deref())
            .map_err(serde::ser::Error::custom)?;
        SafeDeleteDeclarationRequestWire {
            path: self.path.clone(),
            symbol_name: self.symbol_name.clone(),
            kind: self.kind.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteDeclarationRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteDeclarationRequestWire::deserialize(deserializer)?;
        validate_safe_delete_identity(&wire.path, &wire.symbol_name, wire.kind.as_deref())
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            path: wire.path,
            symbol_name: wire.symbol_name,
            kind: wire.kind,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteDeclarationPlan {
    pub path: String,
    pub symbol_name: String,
    pub kind: String,
    pub position: SafeDeletePosition,
    pub deletion_span: SafeDeleteByteSpan,
    pub base_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteDeclarationPlanWire {
    path: String,
    symbol_name: String,
    kind: String,
    position: SafeDeletePosition,
    deletion_span: SafeDeleteByteSpan,
    base_hash: String,
}

impl Serialize for SafeDeleteDeclarationPlan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_safe_delete_identity(&self.path, &self.symbol_name, Some(&self.kind))
            .map_err(serde::ser::Error::custom)?;
        validate_hash(&self.base_hash).map_err(serde::ser::Error::custom)?;
        SafeDeleteDeclarationPlanWire {
            path: self.path.clone(),
            symbol_name: self.symbol_name.clone(),
            kind: self.kind.clone(),
            position: self.position.clone(),
            deletion_span: self.deletion_span.clone(),
            base_hash: self.base_hash.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteDeclarationPlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteDeclarationPlanWire::deserialize(deserializer)?;
        validate_safe_delete_identity(&wire.path, &wire.symbol_name, Some(&wire.kind))
            .map_err(serde::de::Error::custom)?;
        validate_hash(&wire.base_hash).map_err(serde::de::Error::custom)?;
        Ok(Self {
            path: wire.path,
            symbol_name: wire.symbol_name,
            kind: wire.kind,
            position: wire.position,
            deletion_span: wire.deletion_span,
            base_hash: wire.base_hash,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SafeDeleteDeclarationOutcome {
    Planned { plan: SafeDeleteDeclarationPlan },
    NotFound,
    AmbiguousSymbol,
    InvalidDeclaration,
}

impl<'de> Deserialize<'de> for SafeDeleteDeclarationOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            status: String,
            plan: Option<Option<SafeDeleteDeclarationPlan>>,
        }

        let wire = Wire::deserialize(deserializer)?;
        match (wire.status.as_str(), wire.plan) {
            ("planned", Some(Some(plan))) => Ok(Self::Planned { plan }),
            ("not_found", None) => Ok(Self::NotFound),
            ("ambiguous_symbol", None) => Ok(Self::AmbiguousSymbol),
            ("invalid_declaration", None) => Ok(Self::InvalidDeclaration),
            _ => Err(serde::de::Error::custom(
                "invalid safe-delete declaration outcome",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeDeleteSemanticRequest {
    pub declaration: SafeDeleteDeclarationPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupCoverage {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteReference {
    pub path: String,
    pub start: SafeDeletePosition,
    pub end: SafeDeletePosition,
    pub base_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteReferenceWire {
    path: String,
    start: SafeDeletePosition,
    end: SafeDeletePosition,
    base_hash: String,
}

impl Serialize for SafeDeleteReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_reference(self).map_err(serde::ser::Error::custom)?;
        SafeDeleteReferenceWire {
            path: self.path.clone(),
            start: self.start.clone(),
            end: self.end.clone(),
            base_hash: self.base_hash.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteReferenceWire::deserialize(deserializer)?;
        let value = Self {
            path: wire.path,
            start: wire.start,
            end: wire.end,
            base_hash: wire.base_hash,
        };
        validate_reference(&value).map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteCleanupEdit {
    pub path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
    pub base_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteCleanupEditWire {
    path: String,
    start_byte: usize,
    end_byte: usize,
    replacement: String,
    base_hash: String,
}

impl Serialize for SafeDeleteCleanupEdit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_cleanup_edit(self).map_err(serde::ser::Error::custom)?;
        SafeDeleteCleanupEditWire {
            path: self.path.clone(),
            start_byte: self.start_byte,
            end_byte: self.end_byte,
            replacement: self.replacement.clone(),
            base_hash: self.base_hash.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteCleanupEdit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteCleanupEditWire::deserialize(deserializer)?;
        let value = Self {
            path: wire.path,
            start_byte: wire.start_byte,
            end_byte: wire.end_byte,
            replacement: wire.replacement,
            base_hash: wire.base_hash,
        };
        validate_cleanup_edit(&value).map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeDeleteSemanticPlan {
    pub references: Vec<SafeDeleteReference>,
    pub cleanup_edits: Vec<SafeDeleteCleanupEdit>,
    pub coverage: CleanupCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeDeleteSemanticOutcome {
    Complete { plan: SafeDeleteSemanticPlan },
    Incomplete { plan: SafeDeleteSemanticPlan },
    CapabilityUnavailable,
    BackendTimeout,
    BackendCrashed,
    MalformedResponse,
    InvalidLocation,
    WorkspaceExternal,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum SafeDeleteSemanticOutcomeWire {
    Complete { plan: SafeDeleteSemanticPlan },
    Incomplete { plan: SafeDeleteSemanticPlan },
    CapabilityUnavailable,
    BackendTimeout,
    BackendCrashed,
    MalformedResponse,
    InvalidLocation,
    WorkspaceExternal,
}

impl From<SafeDeleteSemanticOutcomeWire> for SafeDeleteSemanticOutcome {
    fn from(value: SafeDeleteSemanticOutcomeWire) -> Self {
        match value {
            SafeDeleteSemanticOutcomeWire::Complete { plan } => Self::Complete { plan },
            SafeDeleteSemanticOutcomeWire::Incomplete { plan } => Self::Incomplete { plan },
            SafeDeleteSemanticOutcomeWire::CapabilityUnavailable => Self::CapabilityUnavailable,
            SafeDeleteSemanticOutcomeWire::BackendTimeout => Self::BackendTimeout,
            SafeDeleteSemanticOutcomeWire::BackendCrashed => Self::BackendCrashed,
            SafeDeleteSemanticOutcomeWire::MalformedResponse => Self::MalformedResponse,
            SafeDeleteSemanticOutcomeWire::InvalidLocation => Self::InvalidLocation,
            SafeDeleteSemanticOutcomeWire::WorkspaceExternal => Self::WorkspaceExternal,
        }
    }
}

impl Serialize for SafeDeleteSemanticOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Complete { plan } if plan.coverage != CleanupCoverage::Complete => Err(
                serde::ser::Error::custom("complete outcome requires complete coverage"),
            ),
            Self::Incomplete { plan } if plan.coverage != CleanupCoverage::Incomplete => Err(
                serde::ser::Error::custom("incomplete outcome requires incomplete coverage"),
            ),
            Self::Complete { plan } => {
                SafeDeleteSemanticOutcomeWire::Complete { plan: plan.clone() }.serialize(serializer)
            }
            Self::Incomplete { plan } => {
                SafeDeleteSemanticOutcomeWire::Incomplete { plan: plan.clone() }
                    .serialize(serializer)
            }
            Self::CapabilityUnavailable => {
                SafeDeleteSemanticOutcomeWire::CapabilityUnavailable.serialize(serializer)
            }
            Self::BackendTimeout => {
                SafeDeleteSemanticOutcomeWire::BackendTimeout.serialize(serializer)
            }
            Self::BackendCrashed => {
                SafeDeleteSemanticOutcomeWire::BackendCrashed.serialize(serializer)
            }
            Self::MalformedResponse => {
                SafeDeleteSemanticOutcomeWire::MalformedResponse.serialize(serializer)
            }
            Self::InvalidLocation => {
                SafeDeleteSemanticOutcomeWire::InvalidLocation.serialize(serializer)
            }
            Self::WorkspaceExternal => {
                SafeDeleteSemanticOutcomeWire::WorkspaceExternal.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for SafeDeleteSemanticOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            status: String,
            plan: Option<Option<SafeDeleteSemanticPlan>>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let value = match (wire.status.as_str(), wire.plan) {
            ("complete", Some(Some(plan))) => Self::Complete { plan },
            ("incomplete", Some(Some(plan))) => Self::Incomplete { plan },
            ("capability_unavailable", None) => Self::CapabilityUnavailable,
            ("backend_timeout", None) => Self::BackendTimeout,
            ("backend_crashed", None) => Self::BackendCrashed,
            ("malformed_response", None) => Self::MalformedResponse,
            ("invalid_location", None) => Self::InvalidLocation,
            ("workspace_external", None) => Self::WorkspaceExternal,
            _ => {
                return Err(serde::de::Error::custom(
                    "invalid safe-delete semantic outcome",
                ));
            }
        };
        match &value {
            Self::Complete { plan } if plan.coverage != CleanupCoverage::Complete => Err(
                serde::de::Error::custom("complete outcome requires complete coverage"),
            ),
            Self::Incomplete { plan } if plan.coverage != CleanupCoverage::Incomplete => Err(
                serde::de::Error::custom("incomplete outcome requires incomplete coverage"),
            ),
            _ => Ok(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteMutationRequest {
    pub edits: Vec<SafeDeleteCleanupEdit>,
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteMutationRequestWire {
    edits: Vec<SafeDeleteCleanupEdit>,
    dry_run: bool,
}

impl Serialize for SafeDeleteMutationRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.edits.is_empty() {
            return Err(serde::ser::Error::custom(
                "safe-delete mutation requires at least one edit",
            ));
        }
        SafeDeleteMutationRequestWire {
            edits: self.edits.clone(),
            dry_run: self.dry_run,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteMutationRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteMutationRequestWire::deserialize(deserializer)?;
        if wire.edits.is_empty() {
            return Err(serde::de::Error::custom(
                "safe-delete mutation requires at least one edit",
            ));
        }
        Ok(Self {
            edits: wire.edits,
            dry_run: wire.dry_run,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeDeleteMutationStatus {
    Previewed,
    Applied,
    Rejected,
    PartialCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeDeleteFileOutcomeStatus {
    Committed,
    Failed,
    Unattempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeDeleteFileError {
    FilesystemError,
    PersistenceError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeDeleteRejectionCode {
    InvalidPath,
    InvalidRange,
    OverlappingEdits,
    CasConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteFileOutcome {
    pub path: String,
    pub outcome: SafeDeleteFileOutcomeStatus,
    pub error: Option<SafeDeleteFileError>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteFileOutcomeWire {
    path: String,
    outcome: SafeDeleteFileOutcomeStatus,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null_optional",
        skip_serializing_if = "Option::is_none"
    )]
    error: Option<SafeDeleteFileError>,
}

impl Serialize for SafeDeleteFileOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_file_outcome(self).map_err(serde::ser::Error::custom)?;
        SafeDeleteFileOutcomeWire {
            path: self.path.clone(),
            outcome: self.outcome,
            error: self.error,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteFileOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteFileOutcomeWire::deserialize(deserializer)?;
        let value = Self {
            path: wire.path,
            outcome: wire.outcome,
            error: wire.error,
        };
        validate_file_outcome(&value).map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteMutationResult {
    pub status: SafeDeleteMutationStatus,
    pub rejection: Option<SafeDeleteRejectionCode>,
    pub per_file: Vec<SafeDeleteFileOutcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteMutationResultWire {
    status: SafeDeleteMutationStatus,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null_optional",
        skip_serializing_if = "Option::is_none"
    )]
    rejection: Option<SafeDeleteRejectionCode>,
    per_file: Vec<SafeDeleteFileOutcome>,
}

impl Serialize for SafeDeleteMutationResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_mutation_result(self).map_err(serde::ser::Error::custom)?;
        SafeDeleteMutationResultWire {
            status: self.status,
            rejection: self.rejection,
            per_file: self.per_file.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteMutationResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteMutationResultWire::deserialize(deserializer)?;
        let value = Self {
            status: wire.status,
            rejection: wire.rejection,
            per_file: wire.per_file,
        };
        validate_mutation_result(&value).map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeDeleteReport {
    pub status: SafeDeleteStatus,
    pub blocking_reason: Option<SafeDeleteBlockingReason>,
    pub declaration: Option<SafeDeleteDeclarationPlan>,
    pub references: Vec<SafeDeleteReference>,
    pub proposed_edits: Vec<SafeDeleteCleanupEdit>,
    pub affected_files: Vec<String>,
    pub mutation_result: Option<SafeDeleteMutationResult>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SafeDeleteReportWire {
    status: SafeDeleteStatus,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null_optional",
        skip_serializing_if = "Option::is_none"
    )]
    blocking_reason: Option<SafeDeleteBlockingReason>,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null_optional",
        skip_serializing_if = "Option::is_none"
    )]
    declaration: Option<SafeDeleteDeclarationPlan>,
    references: Vec<SafeDeleteReference>,
    proposed_edits: Vec<SafeDeleteCleanupEdit>,
    affected_files: Vec<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null_optional",
        skip_serializing_if = "Option::is_none"
    )]
    mutation_result: Option<SafeDeleteMutationResult>,
}

impl Serialize for SafeDeleteReport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_safe_delete_report(self).map_err(serde::ser::Error::custom)?;
        SafeDeleteReportWire {
            status: self.status,
            blocking_reason: self.blocking_reason,
            declaration: self.declaration.clone(),
            references: self.references.clone(),
            proposed_edits: self.proposed_edits.clone(),
            affected_files: self.affected_files.clone(),
            mutation_result: self.mutation_result.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SafeDeleteReport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SafeDeleteReportWire::deserialize(deserializer)?;
        let value = Self {
            status: wire.status,
            blocking_reason: wire.blocking_reason,
            declaration: wire.declaration,
            references: wire.references,
            proposed_edits: wire.proposed_edits,
            affected_files: wire.affected_files,
            mutation_result: wire.mutation_result,
        };
        validate_safe_delete_report(&value).map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

pub fn is_safe_delete_path(path: &str) -> bool {
    if path.is_empty() || path.contains(['\\', '\0']) || path.starts_with('/') {
        return false;
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }
    path.split('/')
        .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn is_safe_delete_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "method"
            | "module"
            | "type_alias"
            | "const"
            | "static"
            | "macro_def"
            | "class"
    )
}

fn validate_safe_delete_identity(
    path: &str,
    symbol_name: &str,
    kind: Option<&str>,
) -> Result<(), &'static str> {
    if !is_safe_delete_path(path) {
        return Err("safe-delete paths must be workspace-relative");
    }
    if symbol_name.is_empty() {
        return Err("safe-delete symbol names must not be empty");
    }
    if kind.is_some_and(|value| !is_safe_delete_kind(value)) {
        return Err("safe-delete symbol kind is not supported");
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), &'static str> {
    if !is_sha256(hash) {
        return Err("safe-delete base_hash must be a 64-character hexadecimal SHA-256 value");
    }
    Ok(())
}

fn validate_byte_range(start: usize, end: usize) -> Result<(), &'static str> {
    if start > end {
        return Err("safe-delete byte range start must not exceed end");
    }
    Ok(())
}

fn validate_reference(reference: &SafeDeleteReference) -> Result<(), &'static str> {
    if !is_safe_delete_path(&reference.path) {
        return Err("safe-delete reference path must be workspace-relative");
    }
    if (reference.start.line, reference.start.character)
        > (reference.end.line, reference.end.character)
    {
        return Err("safe-delete reference start must not follow end");
    }
    validate_hash(&reference.base_hash)
}

fn validate_cleanup_edit(edit: &SafeDeleteCleanupEdit) -> Result<(), &'static str> {
    if !is_safe_delete_path(&edit.path) {
        return Err("safe-delete edit path must be workspace-relative");
    }
    validate_byte_range(edit.start_byte, edit.end_byte)?;
    validate_hash(&edit.base_hash)
}

fn validate_file_outcome(file: &SafeDeleteFileOutcome) -> Result<(), &'static str> {
    if !is_safe_delete_path(&file.path) {
        return Err("safe-delete mutation paths must be workspace-relative");
    }
    match (file.outcome, file.error) {
        (SafeDeleteFileOutcomeStatus::Failed, Some(_)) => Ok(()),
        (SafeDeleteFileOutcomeStatus::Failed, None) => {
            Err("failed safe-delete mutation outcome requires an error")
        }
        (_, Some(_)) => Err("only failed safe-delete mutation outcomes may contain an error"),
        (_, None) => Ok(()),
    }
}

fn validate_mutation_result(result: &SafeDeleteMutationResult) -> Result<(), &'static str> {
    let mut paths = std::collections::HashSet::new();
    for file in &result.per_file {
        validate_file_outcome(file)?;
        if !paths.insert(&file.path) {
            return Err("safe-delete mutation paths must be unique");
        }
    }
    match result.status {
        SafeDeleteMutationStatus::Previewed
            if result.rejection.is_none()
                && result
                    .per_file
                    .iter()
                    .all(|file| file.outcome == SafeDeleteFileOutcomeStatus::Unattempted) =>
        {
            Ok(())
        }
        SafeDeleteMutationStatus::Applied
            if result.rejection.is_none()
                && result
                    .per_file
                    .iter()
                    .all(|file| file.outcome == SafeDeleteFileOutcomeStatus::Committed) =>
        {
            Ok(())
        }
        SafeDeleteMutationStatus::Rejected
            if result.rejection.is_some()
                && result.per_file.iter().all(|file| {
                    file.outcome == SafeDeleteFileOutcomeStatus::Unattempted && file.error.is_none()
                }) =>
        {
            Ok(())
        }
        SafeDeleteMutationStatus::PartialCommit if result.rejection.is_none() => {
            let failures: Vec<_> = result
                .per_file
                .iter()
                .enumerate()
                .filter(|(_, file)| file.outcome == SafeDeleteFileOutcomeStatus::Failed)
                .collect();
            if failures.len() != 1 {
                return Err("partial-commit safe-delete result requires exactly one failed file");
            }
            let failed_index = failures[0].0;
            if result.per_file[..failed_index]
                .iter()
                .any(|file| file.outcome != SafeDeleteFileOutcomeStatus::Committed)
                || result.per_file[failed_index + 1..]
                    .iter()
                    .any(|file| file.outcome != SafeDeleteFileOutcomeStatus::Unattempted)
            {
                return Err(
                    "partial-commit safe-delete outcomes must be committed, failed, then unattempted",
                );
            }
            Ok(())
        }
        _ => Err("safe-delete mutation status and outcomes are contradictory"),
    }
}

fn validate_safe_delete_report(report: &SafeDeleteReport) -> Result<(), &'static str> {
    let mut affected = Vec::new();
    for edit in &report.proposed_edits {
        if !affected.contains(&edit.path) {
            affected.push(edit.path.clone());
        }
    }
    if affected != report.affected_files {
        return Err("safe-delete affected_files must equal ordered unique proposal paths");
    }
    if report
        .affected_files
        .iter()
        .any(|path| !is_safe_delete_path(path))
    {
        return Err("safe-delete affected paths must be workspace-relative");
    }
    if let Some(mutation) = &report.mutation_result {
        let mutation_paths: Vec<_> = mutation
            .per_file
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        let affected_paths: Vec<_> = report.affected_files.iter().map(String::as_str).collect();
        if mutation_paths != affected_paths {
            return Err("safe-delete mutation paths must equal affected_files");
        }
    }
    match report.status {
        SafeDeleteStatus::Blocked => {
            let reason = report
                .blocking_reason
                .ok_or("blocked safe-delete report requires a blocking reason")?;
            if report
                .mutation_result
                .as_ref()
                .is_some_and(|result| result.status != SafeDeleteMutationStatus::Rejected)
            {
                return Err("blocked safe-delete report permits only a rejected mutation result");
            }
            let resolution_failure = matches!(
                reason,
                SafeDeleteBlockingReason::NotFound
                    | SafeDeleteBlockingReason::AmbiguousSymbol
                    | SafeDeleteBlockingReason::InvalidDeclaration
            );
            if resolution_failure == report.declaration.is_some() {
                return Err("blocked safe-delete declaration presence contradicts its reason");
            }
            if let Some(result) = &report.mutation_result {
                let expected_reason = match result.rejection {
                    Some(SafeDeleteRejectionCode::CasConflict) => {
                        SafeDeleteBlockingReason::CasConflict
                    }
                    Some(_) => SafeDeleteBlockingReason::InvalidCleanup,
                    None => return Err("rejected safe-delete mutation requires a rejection code"),
                };
                if reason != expected_reason {
                    return Err(
                        "safe-delete mutation rejection and blocking reason must correspond",
                    );
                }
            }
            Ok(())
        }
        SafeDeleteStatus::Previewed => {
            validate_successful_safe_delete_report(report, SafeDeleteMutationStatus::Previewed)
        }
        SafeDeleteStatus::Applied => {
            validate_successful_safe_delete_report(report, SafeDeleteMutationStatus::Applied)
        }
        SafeDeleteStatus::PartialCommit => {
            validate_successful_safe_delete_report(report, SafeDeleteMutationStatus::PartialCommit)
        }
    }
}

fn validate_successful_safe_delete_report(
    report: &SafeDeleteReport,
    required_status: SafeDeleteMutationStatus,
) -> Result<(), &'static str> {
    if report.blocking_reason.is_some() || report.declaration.is_none() {
        return Err("successful safe-delete report requires a declaration and no blocking reason");
    }
    if report
        .mutation_result
        .as_ref()
        .is_none_or(|result| result.status != required_status)
    {
        return Err("safe-delete report and mutation statuses must match");
    }
    Ok(())
}

/// JSON Schema for the public path-rename request exposed by the native tool.
pub const PATH_RENAME_REQUEST_SCHEMA_JSON: &str = r#"{
  "type":"object",
  "properties":{
    "source":{"type":"string"},
    "destination":{"type":"string"},
    "dry_run":{"type":"boolean","default":false},
    "semantic_policy":{"type":"string","enum":["none","best_effort","required"]},
    "expected_versions":{"type":"object","additionalProperties":{"type":"string","pattern":"^[0-9A-Fa-f]{64}$"},"default":{}}
  },
  "required":["source","destination","semantic_policy"],
  "additionalProperties":false
}"#;

/// JSON Schema for the internal sidecar path-planning request.
pub const PATH_RENAME_PLANNING_REQUEST_SCHEMA_JSON: &str = r#"{"type":"object","properties":{"moves":{"type":"array","items":{"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"},"kind":{"type":"string","enum":["file","directory"]}},"required":["source","destination","kind"],"additionalProperties":false}},"semantic_policy":{"type":"string","enum":["none","best_effort","required"]}},"required":["moves","semantic_policy"],"additionalProperties":false}"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathRenameRequest {
    pub source: String,
    pub destination: String,
    #[serde(default)]
    pub dry_run: bool,
    pub semantic_policy: PathRenameSemanticPolicy,
    #[serde(default)]
    pub expected_versions: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathRenameRequestWire {
    source: String,
    destination: String,
    #[serde(default)]
    dry_run: bool,
    semantic_policy: PathRenameSemanticPolicy,
    #[serde(default)]
    expected_versions: HashMap<String, String>,
}

impl<'de> Deserialize<'de> for PathRenameRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PathRenameRequestWire::deserialize(deserializer)?;
        if wire
            .expected_versions
            .values()
            .any(|version| !is_sha256(version))
        {
            return Err(serde::de::Error::custom(
                "expected versions must be 64-character hexadecimal SHA-256 values",
            ));
        }
        Ok(Self {
            source: wire.source,
            destination: wire.destination,
            dry_run: wire.dry_run,
            semantic_policy: wire.semantic_policy,
            expected_versions: wire.expected_versions,
        })
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMove {
    pub source: String,
    pub destination: String,
    pub kind: PathMoveKind,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathMoveWire {
    source: String,
    destination: String,
    kind: PathMoveKind,
}

impl Serialize for PathMove {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if !is_workspace_relative(&self.source) || !is_workspace_relative(&self.destination) {
            return Err(serde::ser::Error::custom(
                "path rename moves require workspace-relative paths",
            ));
        }
        PathMoveWire {
            source: self.source.clone(),
            destination: self.destination.clone(),
            kind: self.kind,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PathMove {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PathMoveWire::deserialize(deserializer)?;
        if !is_workspace_relative(&wire.source) || !is_workspace_relative(&wire.destination) {
            return Err(serde::de::Error::custom(
                "path rename moves require workspace-relative paths",
            ));
        }
        Ok(Self {
            source: wire.source,
            destination: wire.destination,
            kind: wire.kind,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathMoveKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathRenameSemanticPolicy {
    None,
    BestEffort,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRenamePlan {
    pub moves: Vec<PathMove>,
    pub edits: Vec<WorkspaceEditSpan>,
    pub semantic_policy: PathRenameSemanticPolicy,
}

impl Serialize for PathRenamePlan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.edits.iter().any(|edit| {
            !is_workspace_relative(&edit.path)
                || edit
                    .base_hash
                    .as_deref()
                    .is_none_or(|hash| !is_sha256(hash))
        }) {
            return Err(serde::ser::Error::custom(
                "path rename plan edits require workspace-relative paths and valid base_hash values",
            ));
        }
        PathRenamePlanWireRef {
            moves: &self.moves,
            edits: &self.edits,
            semantic_policy: self.semantic_policy,
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct PathRenamePlanWireRef<'a> {
    moves: &'a [PathMove],
    edits: &'a [WorkspaceEditSpan],
    semantic_policy: PathRenameSemanticPolicy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathRenamePlanWire {
    moves: Vec<PathMove>,
    edits: Vec<WorkspaceEditSpan>,
    semantic_policy: PathRenameSemanticPolicy,
}

impl<'de> Deserialize<'de> for PathRenamePlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PathRenamePlanWire::deserialize(deserializer)?;
        if wire.edits.iter().any(|edit| {
            !is_workspace_relative(&edit.path)
                || edit
                    .base_hash
                    .as_deref()
                    .is_none_or(|hash| !is_sha256(hash))
        }) {
            return Err(serde::de::Error::custom(
                "path rename plan edits require workspace-relative paths and valid base_hash values",
            ));
        }
        Ok(Self {
            moves: wire.moves,
            edits: wire.edits,
            semantic_policy: wire.semantic_policy,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathRenameStatus {
    Preview,
    Applied,
    Partial,
    Unsupported,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRenameResult {
    pub status: PathRenameStatus,
    pub plan: PathRenamePlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_result: Option<WorkspaceApplyEditsResult>,
    #[serde(default)]
    pub errors: Vec<PathRenameError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRenamePlanningRequest {
    pub moves: Vec<PathMove>,
    pub semantic_policy: PathRenameSemanticPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRenamePlanningResult {
    pub plan: Option<PathRenamePlan>,
    pub error: Option<PathRenameError>,
}

impl Serialize for PathRenamePlanningResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.plan.is_some() == self.error.is_some() {
            return Err(serde::ser::Error::custom(
                "path rename planning result requires exactly one of plan or error",
            ));
        }
        #[derive(Serialize)]
        struct Wire<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            plan: Option<&'a PathRenamePlan>,
            #[serde(skip_serializing_if = "Option::is_none")]
            error: Option<&'a PathRenameError>,
        }
        Wire {
            plan: self.plan.as_ref(),
            error: self.error.as_ref(),
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathRenamePlanningResultWire {
    plan: Option<PathRenamePlan>,
    error: Option<PathRenameError>,
}

impl<'de> Deserialize<'de> for PathRenamePlanningResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PathRenamePlanningResultWire::deserialize(deserializer)?;
        if wire.plan.is_some() == wire.error.is_some() {
            return Err(serde::de::Error::custom(
                "path rename planning result requires exactly one of plan or error",
            ));
        }
        Ok(Self {
            plan: wire.plan,
            error: wire.error,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRenameError {
    pub code: PathRenameErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub message: String,
}

impl Serialize for PathRenameError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            code: PathRenameErrorCode,
            #[serde(skip_serializing_if = "Option::is_none")]
            path: Option<&'a str>,
            message: &'a str,
        }

        let path = self
            .path
            .as_deref()
            .filter(|path| is_workspace_relative(path));
        let message = self.code.public_message();
        Wire {
            code: self.code,
            path,
            message,
        }
        .serialize(serializer)
    }
}

fn is_workspace_relative(path: &str) -> bool {
    !path.is_empty()
        && std::path::Path::new(path).components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathRenameErrorCode {
    SourceNotFound,
    DestinationExists,
    IdenticalPaths,
    DestinationInsideSource,
    DestinationExcluded,
    WorkspaceEscape,
    DuplicateDestination,
    InvalidExpectedVersion,
    SemanticUnsupported,
    SemanticBackend,
    FilesystemError,
    PersistenceError,
    CasConflict,
}

impl PathRenameErrorCode {
    fn public_message(self) -> &'static str {
        match self {
            Self::SourceNotFound => "Source path was not found",
            Self::DestinationExists => "Destination path already exists",
            Self::IdenticalPaths => "Source and destination paths are identical",
            Self::DestinationInsideSource => "Destination is inside the source path",
            Self::DestinationExcluded => "Destination path is excluded",
            Self::WorkspaceEscape => "Path is outside the workspace",
            Self::DuplicateDestination => "Multiple moves have the same destination",
            Self::InvalidExpectedVersion => "Expected version is invalid",
            Self::SemanticUnsupported => "Semantic rename is unavailable",
            Self::SemanticBackend => "Semantic planning failed",
            Self::FilesystemError => "Filesystem operation failed",
            Self::PersistenceError => "Persistence operation failed",
            Self::CasConflict => "File changed before semantic edits were applied",
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LspOutcome>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LspOutcome>,
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
