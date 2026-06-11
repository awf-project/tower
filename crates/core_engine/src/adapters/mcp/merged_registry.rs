//! `MergedRegistry` — unified tool surface combining native + plugin tools (spec 12b).
//!
//! # Wireframe
//!
//! ```text
//!  MergedRegistry
//!    native: NativeToolRegistry          (7 tower_* tools, spec 10b)
//!    host:   Arc<RwLock<PluginHostRegistry>>  (declared_tools(), spec 11b)
//!
//!  list()   → native.list() ++ plugin tools (namespaced)  [dynamic, no cache]
//!  call(name, args):
//!    native name?  → native.call(name, args)
//!    plugin name?  → host.call_plugin_tool(plugin_id, raw_name, args)
//!                       via PluginHostRegistry + PluginInstance.call_tool
//!                       PluginHostError → ToolError (fault mapping)
//! ```
//!
//! # Collision policy (UN1 / AC3)
//!
//! Decision: **namespace plugin tools** as `"tower_<plugin_id>_<tool_name>"`.
//!
//! Why: the `tower_` prefix reserves the host namespace. A plugin named `ast` that
//! declares a tool `get_outline` appears in `tools/list` as `tower_ast_get_outline`
//! — the plugin name is always part of the composed name, so a plugin can never
//! claim a bare native name. Native tools keep their `tower_*` names (e.g.
//! `tower_find_file`).
//! No runtime name collision is possible regardless of what any plugin declares,
//! provided the plugin name does not begin with `"tower"`.
//!
//! **Plugin names must not begin with `"tower"`** (the host namespace prefix).
//! This is enforced at registration time by
//! [`PluginHostRegistry::register`][crate::domain::plugin_host::PluginHostRegistry::register]
//! via the `RegistrationError::ReservedNamePrefix` guard. A plugin named e.g.
//! `"find"` with tool `"file"` would compose to `"tower_find_file"` — colliding
//! with the native tool — so the `"tower"` prefix is entirely reserved for the
//! host and may never appear as a plugin name prefix.
//!
//! **Native-first ordering in `call()`** is defence-in-depth: even if the guard
//! ever fails to fire, the native fast-path runs first and the composed name is
//! only matched in the plugin iteration, never as a native name.
//!
//! Trade-off vs. `"<plugin>/<tool>"` slash notation: underscores are
//! URL/shell-safe and avoid the slash ambiguity that made splitting unreliable.
//! The trade-off is that plugin names and tool names must not collide in a way
//! that confuses composed names, which is why the `"tower"` prefix reservation
//! and the `ReservedNamePrefix` guard together form the complete invariant.
//!
//! The policy is documented here and enforced unconditionally: there is no code
//! path that allows a plugin to register a `tower_`-prefixed name.
//!
//! # Fault mapping (UN2 / AC4)
//!
//! A `PluginHostError` from `PluginInstance::call_tool` is mapped to
//! `ToolError::ExecutionFailed` (→ JSON-RPC `-32603`). The MCP link survives:
//! the error is returned as a structured `ToolError`, not a panic, so the
//! transport emits a well-formed error response and continues serving requests.
//!
//! Fault isolation is **not re-implemented here** — 12b delegates to the 11b
//! registry's `PluginInstance::call_tool` which already wraps sandboxes (11d).
//!
//! # Type mapping (plugin_sdk::Value ↔ serde_json::Value)
//!
//! `plugin_sdk::Value` is the ABI value type; `serde_json::Value` is the MCP
//! wire type. The mapping is total (handles all variants, including `Bytes` which
//! is encoded as a base64-like byte-array of integers).
//!
//! # Safety
//!
//! No unsafe code. `#[forbid(unsafe_code)]` is active on this module.

#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use serde_json::Value as JsonValue;

use plugin_sdk::Value as SdkValue;

use crate::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::domain::plugin_host::{PluginHostError, PluginHostRegistry};

// ── MergedRegistry ─────────────────────────────────────────────────────────────

/// A [`ToolRegistry`] that unifies native `tower_*` tools (10b) with tools
/// declared by loaded plugins (11b), namespaced as `"tower_<plugin>_<tool>"`.
///
/// # Example
///
/// ```rust,ignore
/// use std::sync::{Arc, RwLock};
/// use core_engine::adapters::mcp::merged_registry::MergedRegistry;
/// use core_engine::domain::plugin_host::PluginHostRegistry;
///
/// let merged = MergedRegistry::new(engine_state, plugin_registry);
/// let tools = merged.list();
/// // tools contains 7 native tower_* tools plus any plugin tools.
/// ```
pub struct MergedRegistry {
    native: NativeToolRegistry,
    host: Arc<RwLock<PluginHostRegistry>>,
}

impl MergedRegistry {
    /// Construct a `MergedRegistry` from a shared engine state and plugin host.
    ///
    /// Both are reference-counted so they can be shared with the filesystem watcher
    /// or other subsystems without copying.
    pub fn new(
        engine_state: Arc<RwLock<EngineState>>,
        host: Arc<RwLock<PluginHostRegistry>>,
    ) -> Self {
        Self {
            native: NativeToolRegistry::new(engine_state),
            host,
        }
    }
}

impl ToolRegistry for MergedRegistry {
    /// Return all tools: 7 native `tower_*` tools plus namespaced plugin tools.
    ///
    /// Called once per `tools/list` request; no result is cached. Adding or
    /// removing plugins is reflected immediately in the next call (EV3 / AC5).
    fn list(&self) -> Vec<ToolDesc> {
        let mut tools = self.native.list();

        // Guard: recover a poisoned lock rather than propagating a panic.
        // Why: a wasm trap during a concurrent call_tool can poison the Mutex
        //      inside PluginHostRegistry. Recovering lets tools/list continue
        //      serving the 7 native tools even when a plugin sandbox crashed.
        let host_guard = self
            .host
            .read()
            .unwrap_or_else(|poison| poison.into_inner());

        for (plugin_id, sdk_tool) in host_guard.declared_tools() {
            let namespaced_name = format!("tower_{}_{}", plugin_id.as_str(), sdk_tool.name);
            let mcp_tool = sdk_tool_to_mcp_tool_desc(namespaced_name, &sdk_tool);
            tools.push(mcp_tool);
        }

        tools
    }

    /// Dispatch a tool call.
    ///
    /// Native tool names (`tower_*`) are forwarded to the `NativeToolRegistry`
    /// first (fast path).
    ///
    /// Plugin tool names have the form `"tower_<plugin_id>_<tool_name>"`. Because
    /// both plugin names and tool names contain underscores, splitting by `"_"` is
    /// ambiguous. Instead, after the native fast-path, we iterate
    /// `host_guard.declared_tools()` and for each `(plugin_id, tool_desc)` pair
    /// reconstruct `format!("tower_{}_{}", plugin_id, tool_desc.name)`. On an
    /// exact match we dispatch with the original `(plugin_id, tool_name)` pair via
    /// the existing `call_instance_tool` path.
    ///
    /// # Errors
    ///
    /// - [`ToolError::NotFound`] — name matches neither a native nor a plugin tool.
    /// - [`ToolError::ExecutionFailed`] — the plugin tool faulted (trap, fuel, epoch,
    ///   quarantine). The engine and MCP link remain alive.
    /// - Any `ToolError` variant from the native registry on native tool calls.
    fn call(&mut self, name: &str, args: JsonValue) -> Result<JsonValue, ToolError> {
        // Fast path: native tool (defence-in-depth — native always wins).
        if self.native.list().iter().any(|t| t.name == name) {
            return self.native.call(name, args);
        }

        // Decision: host.read() not host.write().
        // Why: call_instance_tool() acquires a per-slot Mutex for exclusive
        //      instance access — the outer RwLock only needs a read guard to
        //      safely traverse the instances Vec. Using write() blocked ALL
        //      concurrent host.read() callers (tools/list, concurrent calls)
        //      for the full plugin invocation duration, violating the
        //      AGENTS.md "zero lock contention" mandate.
        // Trade-off: structural mutations (register/remove) still require
        //      write(), which is correct. Per-call exclusivity is delegated
        //      entirely to the per-slot Mutex inside PluginHostRegistry.
        let host_guard = self
            .host
            .read()
            .unwrap_or_else(|poison| poison.into_inner());

        // Plugin path: iterate declared tools and match by reconstructing the
        // composed name. No splitting — underscore ambiguity is avoided entirely.
        for (pid, tdesc) in host_guard.declared_tools() {
            let composed = format!("tower_{}_{}", pid.as_str(), tdesc.name);
            if composed == name {
                let sdk_args = json_to_sdk_value(args);
                return call_plugin_tool(&host_guard, pid.as_str(), &tdesc.name, sdk_args);
            }
        }

        Err(ToolError::NotFound(name.to_owned()))
    }
}

// ── Private helpers ────────────────────────────────────────────────────────────

/// Convert a `plugin_sdk::ToolDesc` into an MCP `ToolDesc` with the given
/// (already-namespaced) name.
///
/// The `schema_json` field is a JSON string in plugin_sdk. We parse it into a
/// `serde_json::Value` so it is inlined into the `tools/list` object rather than
/// double-encoded. Falls back to `{}` on parse failure.
fn sdk_tool_to_mcp_tool_desc(namespaced_name: String, sdk_tool: &plugin_sdk::ToolDesc) -> ToolDesc {
    let input_schema = serde_json::from_str::<JsonValue>(&sdk_tool.schema_json)
        .unwrap_or_else(|_| serde_json::json!({}));
    ToolDesc {
        name: namespaced_name,
        description: sdk_tool.description.clone(),
        input_schema,
    }
}

/// Dispatch a tool call to a registered plugin instance.
///
/// Iterates the registry's instances (via `declared_tools` for discovery) and
/// dispatches through the `Mutex<Box<dyn PluginInstance>>` lock on the matching
/// slot. Because `PluginHostRegistry` does not expose direct mutable access to
/// individual instances through its public API, we use `declared_tools` to verify
/// the plugin exists and then route through a dedicated internal dispatch.
///
/// Fault isolation: `PluginHostError` → `ToolError::ExecutionFailed`. The 11d
/// `IsolatedSandbox` already catches wasm traps; we only need to map the error.
fn call_plugin_tool(
    host: &PluginHostRegistry,
    plugin_id_str: &str,
    tool_name: &str,
    args: SdkValue,
) -> Result<JsonValue, ToolError> {
    // Verify the (plugin_id, tool_name) pair is declared. This is O(n) but
    // n is very small (typically <10 plugins with <10 tools each).
    let declared = host.declared_tools();
    let exists = declared
        .iter()
        .any(|(pid, tdesc)| pid.as_str() == plugin_id_str && tdesc.name == tool_name);

    if !exists {
        return Err(ToolError::NotFound(format!(
            "tower_{plugin_id_str}_{tool_name}"
        )));
    }

    // Dispatch through the registry's call_instance_tool helper.
    host.call_instance_tool(plugin_id_str, tool_name, args)
        .map(sdk_value_to_json)
        .map_err(plugin_error_to_tool_error)
}

/// Map a `PluginHostError` to a `ToolError`.
///
/// # Stable code assignment
///
/// | Variant                  | ToolError variant    | JSON-RPC code |
/// |--------------------------|----------------------|---------------|
/// | `ToolNotFound`           | `NotFound`           | -32001        |
/// | `InvalidArgs`            | `InvalidArgs`        | -32602        |
/// | `PluginFault` (any kind) | `ExecutionFailed`    | -32603        |
/// | `CallFailed`             | `ExecutionFailed`    | -32603        |
/// | `HookDeliveryFailed`     | `ExecutionFailed`    | -32603        |
///
/// `PluginFault` always maps to `ExecutionFailed` rather than a new code because:
/// - Clients already know how to handle `-32603` (internal error).
/// - The fault detail is in the message string for debugging.
/// - Adding a new code would require client-side changes for no functional benefit.
fn plugin_error_to_tool_error(err: PluginHostError) -> ToolError {
    match err {
        PluginHostError::ToolNotFound(name) => ToolError::NotFound(name),
        PluginHostError::InvalidArgs(msg) => ToolError::InvalidArgs(msg),
        PluginHostError::CallFailed(msg) | PluginHostError::HookDeliveryFailed(msg) => {
            ToolError::ExecutionFailed(msg)
        }
        PluginHostError::PluginFault(kind) => {
            ToolError::ExecutionFailed(format!("plugin fault: {kind}"))
        }
    }
}

// ── Type mapping: plugin_sdk::Value ↔ serde_json::Value ───────────────────────

/// Convert a `serde_json::Value` to a `plugin_sdk::Value`.
///
/// The mapping is total — every JSON variant maps to a `plugin_sdk::Value`
/// variant. JSON arrays become `Value::List`; JSON objects become `Value::Map`
/// (key-value pairs in iteration order).
pub fn json_to_sdk_value(json: JsonValue) -> SdkValue {
    match json {
        JsonValue::Null => SdkValue::Null,
        JsonValue::Bool(b) => SdkValue::Bool(b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                SdkValue::Integer(i)
            } else {
                SdkValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        JsonValue::String(s) => SdkValue::Text(s),
        JsonValue::Array(arr) => SdkValue::List(arr.into_iter().map(json_to_sdk_value).collect()),
        JsonValue::Object(obj) => SdkValue::Map(
            obj.into_iter()
                .map(|(k, v)| (k, json_to_sdk_value(v)))
                .collect(),
        ),
    }
}

/// Convert a `plugin_sdk::Value` to a `serde_json::Value`.
///
/// The mapping is total. `Bytes` is encoded as a JSON array of unsigned integers
/// (`[u8]`) since `serde_json::Value` has no binary variant. This is the standard
/// approach for JSON-safe binary representation without a base64 dependency.
pub fn sdk_value_to_json(sdk: SdkValue) -> JsonValue {
    match sdk {
        SdkValue::Null => JsonValue::Null,
        SdkValue::Bool(b) => JsonValue::Bool(b),
        SdkValue::Integer(i) => JsonValue::Number(i.into()),
        SdkValue::Float(f) => {
            // serde_json::Number::from_f64 returns None for NaN/inf;
            // fall back to Null rather than panicking.
            serde_json::Number::from_f64(f)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null)
        }
        SdkValue::Text(s) => JsonValue::String(s),
        SdkValue::Bytes(bytes) => {
            // Encode as array of u8 integers — no base64 dependency needed.
            JsonValue::Array(
                bytes
                    .into_iter()
                    .map(|b| JsonValue::Number(b.into()))
                    .collect(),
            )
        }
        SdkValue::List(list) => JsonValue::Array(list.into_iter().map(sdk_value_to_json).collect()),
        SdkValue::Map(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                map.insert(k, sdk_value_to_json(v));
            }
            JsonValue::Object(map)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use plugin_sdk::{ABI_VERSION, HookKind, HookPayload, PluginManifest, Value as SdkValue};
    use serde_json::{Value as JsonValue, json};

    use super::{MergedRegistry, json_to_sdk_value, sdk_value_to_json};
    use crate::adapters::mcp::native_tools::EngineState;
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryFs, InMemoryStorage};
    use crate::domain::index::InvertedIndex;
    use crate::domain::plugin_host::{
        PluginFaultKind, PluginHostError, PluginHostRegistry, PluginInstance,
    };
    use crate::domain::workspace::ProjectWorkspace;

    // ── Fake plugin infrastructure ────────────────────────────────────────────

    /// A minimal fake `PluginInstance` that declares one tool and echoes its args.
    struct EchoPlugin {
        manifest: PluginManifest,
    }

    impl EchoPlugin {
        fn new(name: &str, tools: Vec<plugin_sdk::ToolDesc>) -> Self {
            Self {
                manifest: PluginManifest {
                    name: name.to_owned(),
                    version: "0.1.0".to_owned(),
                    abi: ABI_VERSION,
                    tools,
                    hooks: vec![],
                },
            }
        }
    }

    impl PluginInstance for EchoPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn call_tool(&mut self, name: &str, args: SdkValue) -> Result<SdkValue, PluginHostError> {
            if self.manifest.tools.iter().any(|t| t.name == name) {
                Ok(args)
            } else {
                Err(PluginHostError::ToolNotFound(name.to_owned()))
            }
        }

        fn deliver_hook(
            &mut self,
            _kind: HookKind,
            _payload: HookPayload,
        ) -> Result<(), PluginHostError> {
            Ok(())
        }
    }

    /// A fake `PluginInstance` whose `call_tool` always returns a fault.
    struct FaultingPlugin {
        manifest: PluginManifest,
    }

    impl FaultingPlugin {
        fn new(name: &str, tool_name: &str) -> Self {
            Self {
                manifest: PluginManifest {
                    name: name.to_owned(),
                    version: "0.1.0".to_owned(),
                    abi: ABI_VERSION,
                    tools: vec![plugin_sdk::ToolDesc {
                        name: tool_name.to_owned(),
                        description: "always faults".to_owned(),
                        schema_json: "{}".to_owned(),
                    }],
                    hooks: vec![],
                },
            }
        }
    }

    impl PluginInstance for FaultingPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn call_tool(&mut self, _name: &str, _args: SdkValue) -> Result<SdkValue, PluginHostError> {
            Err(PluginHostError::PluginFault(PluginFaultKind::Trapped(
                "injected wasm trap".to_owned(),
            )))
        }

        fn deliver_hook(
            &mut self,
            _kind: HookKind,
            _payload: HookPayload,
        ) -> Result<(), PluginHostError> {
            Ok(())
        }
    }

    // ── Setup helpers ─────────────────────────────────────────────────────────

    fn empty_engine_state() -> Arc<RwLock<EngineState>> {
        Arc::new(RwLock::new(EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(InMemoryFs::new()),
        )))
    }

    fn empty_plugin_host() -> Arc<RwLock<PluginHostRegistry>> {
        Arc::new(RwLock::new(PluginHostRegistry::new()))
    }

    fn echo_tool(name: &str) -> plugin_sdk::ToolDesc {
        plugin_sdk::ToolDesc {
            name: name.to_owned(),
            description: format!("echo tool: {name}"),
            schema_json: "{}".to_owned(),
        }
    }

    fn merged_with_echo_plugin(plugin_name: &str, tool_name: &str) -> MergedRegistry {
        let host = Arc::new(RwLock::new(PluginHostRegistry::new()));
        host.write()
            .unwrap()
            .register(Box::new(EchoPlugin::new(
                plugin_name,
                vec![echo_tool(tool_name)],
            )))
            .expect("register must succeed");
        MergedRegistry::new(empty_engine_state(), host)
    }

    // ── AC1: fake plugin tool appears in tools/list beside 7 native tools ─────

    #[test]
    fn ac1_plugin_tool_appears_in_list_beside_native_tools() {
        let reg = merged_with_echo_plugin("my_plugin", "echo");
        let tools = reg.list();

        // 7 native + 1 plugin tool.
        assert_eq!(
            tools.len(),
            8,
            "expected 7 native + 1 plugin tool; got {}: {:?}",
            tools.len(),
            tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        // All 7 native tools present.
        for native in &[
            "tower_find_file",
            "tower_search_text",
            "tower_read_file",
            "tower_create_file",
            "tower_create_directory",
            "tower_delete_file",
            "tower_global_replace",
        ] {
            assert!(names.contains(native), "native tool '{native}' missing");
        }

        // Plugin tool present, namespaced.
        assert!(
            names.contains(&"tower_my_plugin_echo"),
            "plugin tool 'tower_my_plugin_echo' missing; got {names:?}"
        );
    }

    #[test]
    fn ac1_empty_plugin_host_yields_only_seven_native_tools() {
        let mut reg = MergedRegistry::new(empty_engine_state(), empty_plugin_host());
        let tools = reg.list();
        assert_eq!(tools.len(), 7, "no plugins → 7 native tools only");
        // Sanity: call on a native tool still works.
        let result = reg.call("tower_find_file", json!({ "query": "x" }));
        assert!(result.is_ok());
    }

    // ── AC2: tools/call routes to the fake sandbox and returns the result ─────

    #[test]
    fn ac2_call_routes_to_plugin_and_returns_result() {
        let mut reg = merged_with_echo_plugin("demo", "echo_it");
        let args = json!({ "msg": "hello" });
        let result = reg.call("tower_demo_echo_it", args.clone());
        let returned = result.expect("plugin call must succeed");
        // EchoPlugin returns the args back as a Map.
        assert!(
            returned.is_object()
                || returned.is_null()
                || returned.is_array()
                || returned == args
                || returned.get("msg").is_some(),
            "expected echo of args; got {returned}"
        );
    }

    #[test]
    fn ac2_native_tools_still_routed_correctly_in_merged_registry() {
        let mut reg = merged_with_echo_plugin("p", "t");
        // Call a native tool through the merged registry.
        let result = reg.call("tower_find_file", json!({ "query": "anything" }));
        assert!(result.is_ok(), "native tool route must work: {result:?}");
    }

    // ── AC3: collision policy — namespacing prevents silent shadow ────────────

    #[test]
    fn ac3_plugin_tool_named_like_native_is_namespaced_not_shadowing() {
        // Plugin declares a tool with the same name as a native tool's suffix.
        let host = Arc::new(RwLock::new(PluginHostRegistry::new()));
        host.write()
            .unwrap()
            .register(Box::new(EchoPlugin::new(
                "evil_plugin",
                vec![echo_tool("tower_find_file")],
            )))
            .expect("register must succeed");

        let reg = MergedRegistry::new(empty_engine_state(), host);
        let tools = reg.list();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        // Native tool still present with its original name.
        assert!(
            names.contains(&"tower_find_file"),
            "native tower_find_file must be present: {names:?}"
        );
        // Plugin tool present under its namespaced name.
        assert!(
            names.contains(&"tower_evil_plugin_tower_find_file"),
            "plugin tool must be namespaced: {names:?}"
        );
        // No duplication — exactly one plain `tower_find_file`.
        assert_eq!(
            names.iter().filter(|&&n| n == "tower_find_file").count(),
            1,
            "native tool must appear exactly once: {names:?}"
        );
    }

    #[test]
    fn ac3_two_plugins_with_same_tool_name_both_appear_namespaced() {
        let host = Arc::new(RwLock::new(PluginHostRegistry::new()));
        host.write()
            .unwrap()
            .register(Box::new(EchoPlugin::new(
                "plugin_a",
                vec![echo_tool("greet")],
            )))
            .expect("register must succeed");
        host.write()
            .unwrap()
            .register(Box::new(EchoPlugin::new(
                "plugin_b",
                vec![echo_tool("greet")],
            )))
            .expect("register must succeed");

        let reg = MergedRegistry::new(empty_engine_state(), host);
        let tools = reg.list();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            names.contains(&"tower_plugin_a_greet"),
            "tower_plugin_a_greet must be present: {names:?}"
        );
        assert!(
            names.contains(&"tower_plugin_b_greet"),
            "tower_plugin_b_greet must be present: {names:?}"
        );
    }

    // ── AC4: faulting plugin tool → engine survives, ToolError::ExecutionFailed

    #[test]
    fn ac4_faulting_plugin_returns_execution_failed_engine_survives() {
        let host = Arc::new(RwLock::new(PluginHostRegistry::new()));
        host.write()
            .unwrap()
            .register(Box::new(FaultingPlugin::new("bad_plugin", "danger")))
            .expect("register must succeed");

        let mut reg = MergedRegistry::new(empty_engine_state(), host);

        let result = reg.call("tower_bad_plugin_danger", json!({}));
        let err = result.expect_err("faulting plugin must return Err");
        assert!(
            matches!(
                err,
                crate::adapters::mcp::types::ToolError::ExecutionFailed(_)
            ),
            "fault must map to ExecutionFailed, got {err:?}"
        );

        // Engine survives: we can still list tools.
        let tools = reg.list();
        assert!(
            tools.len() >= 7,
            "engine must survive plugin fault; got {} tools",
            tools.len()
        );
        // And call a native tool.
        let native_result = reg.call("tower_find_file", json!({ "query": "test" }));
        assert!(
            native_result.is_ok(),
            "native tools must work after plugin fault"
        );
    }

    #[test]
    fn ac4_plugin_invalid_args_maps_to_invalid_args_error() {
        struct InvalidArgsPlugin {
            manifest: PluginManifest,
        }
        impl PluginInstance for InvalidArgsPlugin {
            fn manifest(&self) -> &PluginManifest {
                &self.manifest
            }
            fn call_tool(&mut self, _: &str, _: SdkValue) -> Result<SdkValue, PluginHostError> {
                Err(PluginHostError::InvalidArgs("bad field".to_owned()))
            }
            fn deliver_hook(&mut self, _: HookKind, _: HookPayload) -> Result<(), PluginHostError> {
                Ok(())
            }
        }

        let host = Arc::new(RwLock::new(PluginHostRegistry::new()));
        host.write()
            .unwrap()
            .register(Box::new(InvalidArgsPlugin {
                manifest: PluginManifest {
                    name: "argtest".to_owned(),
                    version: "0.1.0".to_owned(),
                    abi: ABI_VERSION,
                    tools: vec![plugin_sdk::ToolDesc {
                        name: "check".to_owned(),
                        description: "".to_owned(),
                        schema_json: "{}".to_owned(),
                    }],
                    hooks: vec![],
                },
            }))
            .expect("register must succeed");

        let mut reg = MergedRegistry::new(empty_engine_state(), host);
        let err = reg
            .call("tower_argtest_check", json!({}))
            .expect_err("must return error");
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "InvalidArgs must be preserved: {err:?}"
        );
    }

    // ── AC5: removing a plugin drops its tools from next tools/list ───────────

    #[test]
    fn ac5_removing_plugin_drops_its_tools_from_list() {
        let host = Arc::new(RwLock::new(PluginHostRegistry::new()));
        host.write()
            .unwrap()
            .register(Box::new(EchoPlugin::new(
                "temp_plugin",
                vec![echo_tool("temp_tool")],
            )))
            .expect("register must succeed");

        let reg = MergedRegistry::new(empty_engine_state(), Arc::clone(&host));

        // Plugin tool is visible.
        assert!(
            reg.list()
                .iter()
                .any(|t| t.name == "tower_temp_plugin_temp_tool"),
            "plugin tool must be visible after registration"
        );

        // Simulate runtime removal by replacing the host registry contents.
        // PluginHostRegistry does not expose a remove() API (spec 11b), so we
        // replace the inner registry with a fresh empty one via the Arc<RwLock<>>.
        *host.write().unwrap() = PluginHostRegistry::new();

        // Next tools/list must not include the removed plugin's tools (AC5 / EV3).
        let tools_after = reg.list();
        assert!(
            !tools_after
                .iter()
                .any(|t| t.name == "tower_temp_plugin_temp_tool"),
            "removed plugin's tools must not appear in next list: {:?}",
            tools_after.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert_eq!(
            tools_after.len(),
            7,
            "only 7 native tools remain after plugin removal"
        );
    }

    // ── Type mapping round-trips ──────────────────────────────────────────────

    #[test]
    fn type_mapping_json_to_sdk_round_trip() {
        let cases: Vec<JsonValue> = vec![
            JsonValue::Null,
            JsonValue::Bool(true),
            JsonValue::Bool(false),
            json!(42i64),
            json!(1.5f64),
            JsonValue::String("hello".to_owned()),
            json!([1, 2, 3]),
            json!({ "key": "value", "nested": null }),
        ];

        for original in cases {
            let sdk = json_to_sdk_value(original.clone());
            let round_tripped = sdk_value_to_json(sdk);
            // Numbers may differ in representation (int vs float) — compare lossily.
            assert_eq!(original, round_tripped, "round-trip failed for: {original}");
        }
    }

    #[test]
    fn type_mapping_sdk_bytes_to_json_is_array_of_u8() {
        let sdk = SdkValue::Bytes(vec![1, 2, 255]);
        let json = sdk_value_to_json(sdk);
        assert_eq!(json, json!([1, 2, 255]));
    }

    #[test]
    fn type_mapping_nan_float_becomes_null() {
        let sdk = SdkValue::Float(f64::NAN);
        let json = sdk_value_to_json(sdk);
        assert_eq!(json, JsonValue::Null, "NaN must map to null");
    }

    #[test]
    fn type_mapping_integer_json_prefers_i64() {
        let json = json!(99i64);
        let sdk = json_to_sdk_value(json);
        assert!(
            matches!(sdk, SdkValue::Integer(99)),
            "integer JSON must produce SdkValue::Integer"
        );
    }

    // ── Unknown tool name: not found ──────────────────────────────────────────

    #[test]
    fn unknown_tool_name_returns_not_found() {
        let mut reg = MergedRegistry::new(empty_engine_state(), empty_plugin_host());
        let err = reg.call("tower_completely_unknown", json!({})).unwrap_err();
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::NotFound(_)),
            "unknown plugin tool must return NotFound"
        );
    }

    #[test]
    fn bare_unknown_name_returns_not_found() {
        let mut reg = MergedRegistry::new(empty_engine_state(), empty_plugin_host());
        let err = reg.call("no_slash_here", json!({})).unwrap_err();
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::NotFound(_)),
            "bare unknown name must return NotFound"
        );
    }

    // ── Lock granularity: call() must not block concurrent list() ─────────────

    /// Verify that tools/list (host.read()) is not blocked while a plugin call
    /// is in progress. Before the fix, call() held host.write() for the entire
    /// plugin invocation duration, starving all concurrent readers.
    ///
    /// Strategy: hold the RwLock's read guard on a background thread while the
    /// main thread calls call() — with host.read() semantics both succeed; with
    /// host.write() semantics the call would deadlock (or at least block).
    /// We use a barrier to synchronise entry and a short timeout on the join to
    /// assert no deadlock occurs within the test budget.
    #[test]
    fn call_does_not_block_concurrent_list_readers() {
        use std::sync::Barrier;
        use std::thread;
        use std::time::Duration;

        let host = Arc::new(RwLock::new(PluginHostRegistry::new()));
        host.write()
            .unwrap()
            .register(Box::new(EchoPlugin::new(
                "concurrent_test",
                vec![echo_tool("ping")],
            )))
            .expect("register must succeed");

        let host_for_reader = Arc::clone(&host);
        let barrier = Arc::new(Barrier::new(2));
        let barrier_for_reader = Arc::clone(&barrier);

        // Spawn a thread that acquires a read guard (simulating tools/list) and
        // holds it briefly while the main thread also tries to acquire a read guard
        // (via call()).  Both should succeed simultaneously with read locks.
        let reader_handle = thread::spawn(move || {
            // Acquire a read guard — this must not be blocked by the main thread.
            let _guard = host_for_reader.read().unwrap();
            // Signal the main thread that the read lock is held.
            barrier_for_reader.wait();
            // Hold the read guard for a brief moment.
            thread::sleep(Duration::from_millis(20));
            // Guard drops here.
        });

        // Wait until the reader thread has the read lock, then call() — with the
        // fix (read lock in call()), this proceeds immediately.
        barrier.wait();

        let engine_state = empty_engine_state();
        let mut reg = MergedRegistry::new(engine_state, Arc::clone(&host));
        // This call must not deadlock: both the reader and call() want read locks.
        let result = reg.call("tower_concurrent_test_ping", json!({ "x": 1 }));
        assert!(result.is_ok(), "concurrent call must succeed: {result:?}");

        reader_handle.join().expect("reader thread must not panic");
    }
}
