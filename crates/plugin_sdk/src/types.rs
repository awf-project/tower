//! Shared serialisable types for the tower plugin ABI.
//!
//! All types in this module derive [`serde::Serialize`] and [`serde::Deserialize`].
//! They are the only data structures that cross the host ↔ guest boundary.
//! The postcard binary format is used as the wire format (see crate-level docs).
//!
//! # Stability
//!
//! Adding a new variant to an existing enum or a new optional field to a struct
//! is a minor ABI change; removing or reordering variants/fields is breaking.
//! Either way: bump [`crate::ABI_VERSION`].

use serde::{Deserialize, Serialize};

// ── Value ────────────────────────────────────────────────────────────────────

/// A dynamically-typed value that can cross the host ↔ guest boundary.
///
/// This is the argument and return type for [`crate::Plugin::call_tool`].
/// It is intentionally minimal: it covers JSON-like primitives plus a `Bytes`
/// variant for binary payloads. Nesting is supported via [`Value::List`] and
/// [`Value::Map`].
///
/// The host (12b) maps between `Value` and `serde_json::Value` at the MCP
/// adapter layer; plugin_sdk itself never imports serde_json.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::Value;
///
/// let v = Value::Map(vec![
///     ("name".to_owned(), Value::Text("Alice".to_owned())),
///     ("age".to_owned(), Value::Integer(30)),
/// ]);
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    /// JSON `null` / Rust `()`.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit float. NaN and infinity are not supported; callers must validate.
    Float(f64),
    /// UTF-8 string.
    Text(String),
    /// Raw bytes (e.g. binary file contents).
    Bytes(Vec<u8>),
    /// Ordered list of values.
    List(Vec<Value>),
    /// Ordered key-value pairs. Keys are always strings.
    Map(Vec<(String, Value)>),
}

// ── ToolDesc ─────────────────────────────────────────────────────────────────

/// Description of a single tool exported by a plugin.
///
/// Carried in [`PluginManifest::tools`]. The host (12b) maps these to MCP
/// `ToolDesc` objects at the adapter boundary; plugin_sdk does not depend on
/// core_engine's `ToolDesc` — the mapping happens in the other direction.
///
/// # Fields
///
/// - `name`: unique tool identifier within this plugin (e.g. `"greet"`).
/// - `description`: human-readable description for the MCP client.
/// - `schema_json`: JSON Schema string for the tool's input. Use `"{}"` for
///   tools that accept no parameters.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::ToolDesc;
///
/// let tool = ToolDesc {
///     name: "greet".to_owned(),
///     description: "Say hello to a person by name".to_owned(),
///     schema_json: r#"{"type":"object","properties":{"name":{"type":"string"}}}"#.to_owned(),
/// };
/// assert_eq!(tool.name, "greet");
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDesc {
    /// Unique tool name (the key used in `call_tool` dispatch).
    pub name: String,
    /// Human-readable description shown to the MCP client.
    pub description: String,
    /// JSON Schema for the tool's input parameters as a JSON string.
    ///
    /// Use `"{}"` for tools with no parameters. The host validates this is
    /// valid JSON when loading the plugin; invalid schema is a load-time error.
    pub schema_json: String,
}

// ── HookKind ─────────────────────────────────────────────────────────────────

/// The kinds of lifecycle hooks a plugin may subscribe to.
///
/// Plugins list the hooks they want in [`PluginManifest::hooks`]. The host
/// delivers only subscribed hooks — unsubscribed hooks incur zero overhead.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::HookKind;
///
/// let hooks = vec![HookKind::BeforeToolCall, HookKind::AfterToolCall];
/// assert_eq!(hooks.len(), 2);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// Fired before the host dispatches any tool call (native or plugin).
    BeforeToolCall,
    /// Fired after a tool call completes (native or plugin).
    AfterToolCall,
    /// Fired after a file has been indexed (content hash computed and stored).
    ///
    /// Added in ABI version 2 (spec 11b). The payload carries the workspace-relative
    /// path of the indexed file as a UTF-8 string.
    FileIndexed,
    /// Fired after a file's content has changed (re-indexed after a write or
    /// an OS filesystem event).
    ///
    /// Added in ABI version 2 (spec 11b). The payload carries the workspace-relative
    /// path of the changed file as a UTF-8 string.
    FileChanged,
}

// ── HookPayload ───────────────────────────────────────────────────────────────

/// The payload delivered with a hook.
///
/// The variant must match the [`HookKind`] declared in [`HookEnvelope::kind`].
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::{HookKind, HookPayload, Value};
///
/// let payload = HookPayload::BeforeToolCall {
///     tool_name: "greet".to_owned(),
///     args: Value::Map(vec![("name".to_owned(), Value::Text("Alice".to_owned()))]),
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPayload {
    /// Payload for [`HookKind::BeforeToolCall`].
    BeforeToolCall {
        /// Name of the tool about to be called.
        tool_name: String,
        /// Arguments the tool will receive.
        args: Value,
    },
    /// Payload for [`HookKind::AfterToolCall`].
    AfterToolCall {
        /// Name of the tool that was called.
        tool_name: String,
        /// The result returned by the tool (or an error string).
        result: Result<Value, String>,
    },
    /// Payload for [`HookKind::FileIndexed`].
    ///
    /// Added in ABI version 2 (spec 11b).
    FileIndexed {
        /// Workspace-relative path of the indexed file (UTF-8, forward-slash separated).
        path: String,
    },
    /// Payload for [`HookKind::FileChanged`].
    ///
    /// Added in ABI version 2 (spec 11b).
    FileChanged {
        /// Workspace-relative path of the changed file (UTF-8, forward-slash separated).
        path: String,
    },
}

// ── HookEnvelope ─────────────────────────────────────────────────────────────

/// The envelope sent from the host to the guest for a lifecycle hook.
///
/// Serialised with postcard and passed to `__plugin_on_hook(ptr, len)`.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::{HookKind, HookPayload, Value};
/// use plugin_sdk::HookEnvelope;
///
/// let env = HookEnvelope {
///     kind: HookKind::AfterToolCall,
///     payload: HookPayload::AfterToolCall {
///         tool_name: "greet".to_owned(),
///         result: Ok(Value::Text("hello".to_owned())),
///     },
/// };
/// assert_eq!(env.kind, HookKind::AfterToolCall);
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookEnvelope {
    /// Which hook fired.
    pub kind: HookKind,
    /// The data for this hook invocation.
    pub payload: HookPayload,
}

// ── PluginManifest ────────────────────────────────────────────────────────────

/// The manifest a plugin returns from `__plugin_init`.
///
/// Carries the plugin's identity, the ABI it was built against, the tools it
/// exports, and the hooks it subscribes to. The host (11c) reads this once at
/// load time and caches it for the lifetime of the plugin.
///
/// # ABI mismatch detection (AC4 / UN1)
///
/// The host compares `manifest.abi` to its own [`crate::ABI_VERSION`]. If they
/// differ, the host rejects the plugin before calling any other export. This
/// avoids crashes caused by layout or signature mismatches.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::{ABI_VERSION, HookKind, PluginManifest, ToolDesc};
///
/// let manifest = PluginManifest {
///     name: "hello".to_owned(),
///     version: "0.1.0".to_owned(),
///     abi: ABI_VERSION,
///     tools: vec![ToolDesc {
///         name: "greet".to_owned(),
///         description: "Say hello".to_owned(),
///         schema_json: "{}".to_owned(),
///     }],
///     hooks: vec![HookKind::AfterToolCall],
/// };
/// assert_eq!(manifest.abi, ABI_VERSION);
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin name (used as a namespace for tool names).
    pub name: String,
    /// Semver-compatible version string (e.g. `"0.1.0"`).
    pub version: String,
    /// ABI version this plugin was compiled against.
    ///
    /// The host rejects plugins where `abi != ABI_VERSION`.
    pub abi: u32,
    /// Tools this plugin exports.
    pub tools: Vec<ToolDesc>,
    /// Lifecycle hooks this plugin subscribes to.
    pub hooks: Vec<HookKind>,
}

// ── SdkError ─────────────────────────────────────────────────────────────────

/// Errors a plugin may return from [`crate::Plugin::call_tool`].
///
/// The host (11c) maps these to JSON-RPC error codes at the adapter boundary.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::SdkError;
///
/// let e = SdkError::ToolNotFound("missing_tool".to_owned());
/// assert!(matches!(e, SdkError::ToolNotFound(_)));
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdkError {
    /// The tool name is not registered in this plugin's manifest.
    ToolNotFound(String),
    /// The tool's arguments failed validation.
    InvalidArgs(String),
    /// The tool executed but encountered a runtime error.
    CallFailed(String),
}

impl core::fmt::Display for SdkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ToolNotFound(name) => write!(f, "tool not found: {name}"),
            Self::InvalidArgs(msg) => write!(f, "invalid args: {msg}"),
            Self::CallFailed(msg) => write!(f, "call failed: {msg}"),
        }
    }
}

impl core::error::Error for SdkError {}

// ── CallRequest / CallResponse ────────────────────────────────────────────────

/// The request payload sent from the host to the guest for a tool call.
///
/// Serialised with postcard and passed to `__plugin_call_tool(ptr, len)`.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::{CallRequest, Value};
///
/// let req = CallRequest {
///     tool_name: "greet".to_owned(),
///     args: Value::Map(vec![("name".to_owned(), Value::Text("Alice".to_owned()))]),
/// };
/// assert_eq!(req.tool_name, "greet");
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallRequest {
    /// Name of the tool to invoke.
    pub tool_name: String,
    /// Arguments to pass to the tool.
    pub args: Value,
}

/// The response returned by the guest after a tool call.
///
/// Serialised with postcard and returned via `__plugin_call_tool` pointer.
/// The host deserialises this to extract the result or error.
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::{CallResponse, SdkError, Value};
///
/// let resp = CallResponse { result: Ok(Value::Text("hello".to_owned())) };
/// assert!(resp.result.is_ok());
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallResponse {
    /// The tool result, or the error if the call failed.
    pub result: Result<Value, SdkError>,
}
