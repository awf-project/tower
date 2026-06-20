//! Extension manifest schema — the `extension.toml` structure.
//!
//! An `extension.toml` at the root of a sidecar extension's distribution
//! describes its identity, the process to spawn, when to activate it, the
//! tools it contributes, the events it subscribes to, and the host capabilities
//! it requires.

use serde::{Deserialize, Serialize};

/// Parsed contents of an `extension.toml` manifest file.
///
/// # Example (TOML)
///
/// ```toml
/// name    = "ast"
/// version = "0.1.0"
/// command = ["./ast-extension"]
/// activation = "eager"
///
/// [[tools]]
/// name        = "ast_outline"
/// description = "Return the AST outline for a file"
/// schema_json = "{}"
///
/// [events]
/// subscribe = ["event/fileIndexed"]
///
/// [capabilities]
/// required = ["read_file"]
/// ```
///
/// # Example (Rust)
///
/// ```rust
/// use extension_protocol::{Activation, ExtensionManifest};
///
/// let toml = r#"
///     name = "hello"
///     version = "0.1.0"
///     command = ["./hello"]
///     activation = "eager"
/// "#;
/// let m: ExtensionManifest = toml::from_str(toml).unwrap();
/// assert_eq!(m.name, "hello");
/// assert!(matches!(m.activation, Activation::Eager));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// Unique extension identifier (snake_case recommended).
    pub name: String,
    /// SemVer version string.
    pub version: String,
    /// Command to spawn the extension process (`argv[0]` + args).
    pub command: Vec<String>,
    /// When to activate the extension.
    #[serde(default)]
    pub activation: Activation,
    /// Tools contributed to the MCP registry.
    #[serde(default)]
    pub tools: Vec<ToolDecl>,
    /// Events the extension subscribes to.
    #[serde(default)]
    pub events: EventsSection,
    /// Host capabilities the extension requires.
    #[serde(default)]
    pub capabilities: CapabilitiesSection,
}

/// When the host should activate (spawn) an extension.
///
/// # Example
///
/// ```rust
/// use extension_protocol::{Activation, ExtensionManifest};
///
/// let toml = r#"
///     name = "x"
///     version = "0.1.0"
///     command = ["./x"]
///     activation = "eager"
/// "#;
/// let m: ExtensionManifest = toml::from_str(toml).unwrap();
/// assert!(matches!(m.activation, Activation::Eager));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Activation {
    /// Spawn immediately on host startup.
    Eager,
    /// Spawn on first tool invocation or event delivery.
    #[default]
    Lazy,
}

/// Declaration of a single tool contributed by an extension.
///
/// # Example
///
/// ```rust
/// use extension_protocol::ToolDecl;
///
/// let t = ToolDecl {
///     name: "ast_outline".to_owned(),
///     description: "Get AST outline".to_owned(),
///     schema_json: "{}".to_owned(),
/// };
/// assert_eq!(t.name, "ast_outline");
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDecl {
    /// Tool name (prefixed with `tower_<extension>_` by the host at merge time).
    pub name: String,
    /// Human-readable description surfaced to MCP clients.
    pub description: String,
    /// JSON Schema for the tool's input parameters (serialized as a JSON string).
    pub schema_json: String,
}

/// Event subscription section in `extension.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EventsSection {
    /// Event method names the extension subscribes to.
    ///
    /// Example: `["event/fileIndexed", "event/fileChanged"]`.
    #[serde(default)]
    pub subscribe: Vec<String>,
}

/// Capabilities section in `extension.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesSection {
    /// Capability names the extension requires from the host.
    #[serde(default)]
    pub required: Vec<String>,
}

/// A host capability an extension may require.
///
/// The sidecar adapter (spec 23) enforces that only declared capabilities are
/// reachable from a given extension. Undeclared `HostCall` variants are rejected
/// with a [`ProtocolError`](crate::ProtocolError).
///
/// # Example
///
/// ```rust
/// use extension_protocol::Capability;
///
/// let c = Capability::ReadFile;
/// let json = serde_json::to_string(&c).unwrap();
/// let back: Capability = serde_json::from_str(&json).unwrap();
/// assert_eq!(c, back);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `workspace/readFile` — read file content from the VFS.
    ReadFile,
    /// `workspace/listFiles` — enumerate indexed workspace files.
    ListFiles,
    /// `index/get` — read from the AST index cache.
    IndexGet,
    /// `index/put` — write to the AST index cache.
    IndexPut,
    /// `workspace/requestFormat` — enqueue a file for formatting.
    RequestFormat,
    /// `log` — emit a log message through the host's logging subsystem.
    Log,
    /// `notify/resourceUpdated` — push a `notifications/resources/updated` event
    /// to subscribed MCP clients (spec 27 O1).
    Notify,
}

/// Named event kinds for subscription matching.
///
/// Used by the domain registry (spec 22) to fan-out events to subscribed
/// extensions. String-valued so that future events can be added without an
/// ABI break.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventKind {
    /// `event/fileIndexed`
    FileIndexed,
    /// `event/fileChanged`
    FileChanged,
    /// `event/fileDeleted` (spec 27 EV1).
    FileDeleted,
}
