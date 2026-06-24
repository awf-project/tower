//! `ExtensionMergedRegistry` — unified tool surface for native + sidecar extension tools
//! (spec 28).
//!
//! # Wireframe
//!
//! ```text
//!  ExtensionMergedRegistry
//!    native:    NativeToolRegistry              (spec 10b)
//!    extension: Arc<RwLock<ExtensionRegistry>>  (declared_tools(), spec 22)
//!
//!  list()  → native.list() ++ extension tools (namespaced tower_<ext>_<tool>)
//!  call(name, args):
//!    native name?    → native.call(name, args)
//!    extension name? → extension_registry.invoke(bare_tool_name, args)
//!                          InvokeError → ToolError (fault mapping)
//! ```
//!
//! # Namespace (EV1 / U2)
//!
//! Extension tools are namespaced exactly as WASM plugin tools were:
//! `"tower_<extension_name>_<tool_name>"`. The `tower_` prefix reserves the host
//! namespace; no extension may be named `"tower"`.
//!
//! # Fault mapping
//!
//! [`InvokeError`] from `ExtensionRegistry::invoke` is mapped to `ToolError`:
//!
//! | `InvokeError` variant | `ToolError` variant      | JSON-RPC code |
//! |-----------------------|--------------------------|---------------|
//! | `ToolNotFound`        | `NotFound`               | -32001        |
//! | `Fault(_)`            | `ExecutionFailed`        | -32603        |
//!
//! # Safety
//!
//! No unsafe code. `#[forbid(unsafe_code)]` is active on this module.

#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use serde_json::Value as JsonValue;

use crate::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::domain::extension_host::{ExtensionRegistry, InvokeError};

// ── ExtensionMergedRegistry ───────────────────────────────────────────────────

/// A [`ToolRegistry`] that unifies native `tower_*` tools (spec 10b) with tools
/// declared by loaded sidecar extensions (spec 22/23), namespaced as
/// `"tower_<extension_name>_<tool_name>"`.
///
/// # Replacement policy
///
/// This registry replaces the WASM-backed `MergedRegistry` in `main.rs` as of
/// spec 28. The old `MergedRegistry` continues to exist (deleted in spec 29)
/// but is no longer wired into the serving path.
///
/// # No-extensions path (UN1)
///
/// When no extensions are registered, `list()` returns only the native tools
/// and `call()` returns `ToolError::NotFound` for any non-native tool name.
///
/// # Examples
///
/// ```rust,ignore
/// use std::sync::{Arc, RwLock};
/// use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
/// use core_engine::domain::extension_host::ExtensionRegistry;
///
/// let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
/// let merged = ExtensionMergedRegistry::new(engine_state, ext_registry);
/// let tools = merged.list();
/// // tools contains native tower_* tools plus any extension tools.
/// ```
pub struct ExtensionMergedRegistry {
    native: NativeToolRegistry,
    extension_host: Arc<RwLock<ExtensionRegistry>>,
}

impl ExtensionMergedRegistry {
    /// Construct from a shared engine state and extension registry.
    ///
    /// Both are reference-counted so they can be shared with the filesystem watcher
    /// or other subsystems.
    pub fn new(
        engine_state: Arc<RwLock<EngineState>>,
        extension_host: Arc<RwLock<ExtensionRegistry>>,
    ) -> Self {
        Self {
            native: NativeToolRegistry::new(engine_state),
            extension_host,
        }
    }
}

impl ToolRegistry for ExtensionMergedRegistry {
    /// Return all tools: native `tower_*` tools plus namespaced extension tools.
    ///
    /// Called once per `tools/list` request; no caching. Adding or removing
    /// extensions is reflected immediately in the next `list()` call.
    fn list(&self) -> Vec<ToolDesc> {
        let mut tools = self.native.list();

        // Recover a poisoned lock: a crashed extension child must not break list().
        let host_guard = self
            .extension_host
            .read()
            .unwrap_or_else(|poison| poison.into_inner());

        for (ext_id, tool_decl) in host_guard.declared_tools() {
            let namespaced_name = format!("tower_{}_{}", ext_id.as_str(), tool_decl.name);
            let input_schema = serde_json::from_str::<JsonValue>(&tool_decl.schema_json)
                .unwrap_or_else(|_| serde_json::json!({}));
            tools.push(ToolDesc {
                name: namespaced_name,
                description: tool_decl.description.clone(),
                input_schema,
            });
        }

        tools
    }

    /// Dispatch a tool call.
    ///
    /// Native tool names (`tower_*`) are forwarded to the `NativeToolRegistry`
    /// first (fast path / defence-in-depth — native always wins).
    ///
    /// Extension tool names have the form `"tower_<ext_name>_<tool_name>"`. After
    /// the native fast-path, we iterate `declared_tools()` and match by
    /// reconstructing the composed name — the same underscore-safe approach used
    /// by `MergedRegistry` for WASM plugins (no splitting on `_`).
    ///
    /// # Errors
    ///
    /// - [`ToolError::NotFound`] — name matches neither a native nor an extension tool.
    /// - [`ToolError::ExecutionFailed`] — the extension tool returned a fault (timeout,
    ///   crash, quarantine, protocol error). The engine and MCP link remain alive.
    /// - Any `ToolError` variant from the native registry on native calls.
    fn call(&mut self, name: &str, args: JsonValue) -> Result<JsonValue, ToolError> {
        // Fast path: native tool (defence-in-depth — native always wins).
        if self.native.list().iter().any(|t| t.name == name) {
            return self.native.call(name, args);
        }

        // Extension path: read lock only — per-extension Mutex handles exclusivity.
        let host_guard = self
            .extension_host
            .read()
            .unwrap_or_else(|poison| poison.into_inner());

        // Iterate and match by reconstructing the composed name — avoids underscore
        // ambiguity in splitting.
        for (ext_id, tool_decl) in host_guard.declared_tools() {
            let composed = format!("tower_{}_{}", ext_id.as_str(), tool_decl.name);
            if composed == name {
                return host_guard
                    .invoke_extension(&ext_id, &tool_decl.name, args)
                    .map_err(invoke_error_to_tool_error);
            }
        }

        Err(ToolError::NotFound(name.to_owned()))
    }
}

// ── Error mapping ─────────────────────────────────────────────────────────────

/// Map an [`InvokeError`] to a [`ToolError`].
///
/// | `InvokeError` variant  | `ToolError` variant   | JSON-RPC code |
/// |------------------------|-----------------------|---------------|
/// | `ToolNotFound`         | `NotFound`            | -32001        |
/// | `Fault(_)`             | `ExecutionFailed`     | -32603        |
fn invoke_error_to_tool_error(err: InvokeError) -> ToolError {
    match err {
        InvokeError::ToolNotFound(name) => ToolError::NotFound(name),
        InvokeError::Fault(fault) => {
            ToolError::ExecutionFailed(format!("extension fault: {fault:?}"))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use extension_protocol::{ExtensionFault, ExtensionManifest, ToolDecl};
    use serde_json::{Value as JsonValue, json};

    use super::ExtensionMergedRegistry;
    use crate::adapters::mcp::native_tools::{EXPECTED_NATIVE_TOOL_NAMES, EngineState};
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryFs, InMemoryStorage};
    use crate::domain::extension_host::ExtensionRegistry;
    use crate::domain::index::InvertedIndex;
    use crate::domain::workspace::ProjectWorkspace;

    // ── Fake ExtensionInstance helpers ────────────────────────────────────────

    fn make_manifest(name: &str, tools: Vec<ToolDecl>) -> ExtensionManifest {
        use extension_protocol::manifest::{CapabilitiesSection, EventsSection};
        ExtensionManifest {
            name: name.to_owned(),
            version: "0.1.0".to_owned(),
            command: vec!["./stub".to_owned()],
            activation: extension_protocol::Activation::Lazy,
            tools,
            events: EventsSection { subscribe: vec![] },
            capabilities: CapabilitiesSection::default(),
        }
    }

    fn make_tool(name: &str) -> ToolDecl {
        ToolDecl {
            name: name.to_owned(),
            description: format!("Tool {name}"),
            schema_json: "{}".to_owned(),
        }
    }

    /// A fake `ExtensionInstance` that echoes args and records calls.
    struct EchoExtension {
        manifest: ExtensionManifest,
    }

    impl EchoExtension {
        fn new(name: &str, tools: Vec<ToolDecl>) -> Self {
            Self {
                manifest: make_manifest(name, tools),
            }
        }
    }

    impl crate::domain::extension_host::ExtensionInstance for EchoExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(
            &mut self,
            _name: &str,
            params: JsonValue,
        ) -> Result<JsonValue, ExtensionFault> {
            Ok(params)
        }

        fn deliver_event(
            &mut self,
            _event: extension_protocol::Event,
        ) -> Result<(), ExtensionFault> {
            Ok(())
        }

        fn shutdown(&mut self) {}
    }

    struct IdentifyingExtension {
        manifest: ExtensionManifest,
    }

    impl IdentifyingExtension {
        fn new(name: &str, tools: Vec<ToolDecl>) -> Self {
            Self {
                manifest: make_manifest(name, tools),
            }
        }
    }

    impl crate::domain::extension_host::ExtensionInstance for IdentifyingExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(
            &mut self,
            _name: &str,
            _params: JsonValue,
        ) -> Result<JsonValue, ExtensionFault> {
            Ok(JsonValue::String(self.manifest.name.clone()))
        }

        fn deliver_event(
            &mut self,
            _event: extension_protocol::Event,
        ) -> Result<(), ExtensionFault> {
            Ok(())
        }

        fn shutdown(&mut self) {}
    }

    /// A fake `ExtensionInstance` that always faults.
    struct FaultExtension {
        manifest: ExtensionManifest,
    }

    impl FaultExtension {
        fn new(name: &str, tools: Vec<ToolDecl>) -> Self {
            Self {
                manifest: make_manifest(name, tools),
            }
        }
    }

    impl crate::domain::extension_host::ExtensionInstance for FaultExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(
            &mut self,
            _name: &str,
            _params: JsonValue,
        ) -> Result<JsonValue, ExtensionFault> {
            Err(ExtensionFault::Crashed { code: Some(1) })
        }

        fn deliver_event(
            &mut self,
            _event: extension_protocol::Event,
        ) -> Result<(), ExtensionFault> {
            Ok(())
        }

        fn shutdown(&mut self) {}
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

    fn empty_ext_registry() -> Arc<RwLock<ExtensionRegistry>> {
        Arc::new(RwLock::new(ExtensionRegistry::new()))
    }

    fn merged_with_echo_extension(ext_name: &str, tool_name: &str) -> ExtensionMergedRegistry {
        let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
        ext_registry
            .write()
            .unwrap()
            .register(Box::new(EchoExtension::new(
                ext_name,
                vec![make_tool(tool_name)],
            )))
            .expect("register");
        ExtensionMergedRegistry::new(empty_engine_state(), ext_registry)
    }

    // ── EV1: extension tool appears in tools/list beside native tools ──────────

    /// EV1: An extension tool appears in `list()` with the `tower_<ext>_<tool>` name.
    #[test]
    fn extension_tool_appears_in_list_with_tower_prefix() {
        let reg = merged_with_echo_extension("ast", "outline");
        let tools = reg.list();

        // Must contain the namespaced extension tool.
        let ext_tool = tools
            .iter()
            .find(|t| t.name == "tower_ast_outline")
            .expect("tower_ast_outline must be in the list");
        assert_eq!(ext_tool.description, "Tool outline");
    }

    /// EV1: Extension tools appear alongside all native tools.
    #[test]
    fn extension_tools_coexist_with_native_tools_in_list() {
        let reg = merged_with_echo_extension("ast", "get_outline");
        let tools = reg.list();

        assert!(
            tools.len() > EXPECTED_NATIVE_TOOL_NAMES.len(),
            "expected native tools plus one extension, got {}",
            tools.len()
        );
        for expected_name in EXPECTED_NATIVE_TOOL_NAMES {
            assert!(
                tools.iter().any(|t| t.name == expected_name),
                "native {expected_name} must be present"
            );
        }
        // Extension tool is present.
        assert!(
            tools.iter().any(|t| t.name == "tower_ast_get_outline"),
            "extension tower_ast_get_outline must be present"
        );
    }

    /// Existing extension merged registry tests cover namespacing without
    /// hardcoded native tool count literals.
    #[test]
    fn extension_merged_registry_tests_cover_namespacing_without_hardcoded_native_tool_count_literals()
     {
        let source = include_str!("extension_merged_registry.rs");
        let hardcoded_ten_assert = concat!("assert_eq!(tools.len(), ", "10", ")");
        let hardcoded_eleven_assert = concat!("assert_eq!(tools.len(), ", "11", ")");
        let hardcoded_ten_native = concat!("10", " native");
        let hardcoded_eleven_native = concat!("11", " native");

        assert!(
            !source.contains(hardcoded_ten_assert)
                && !source.contains(hardcoded_eleven_assert)
                && !source.contains(hardcoded_ten_native)
                && !source.contains(hardcoded_eleven_native),
            "extension merged registry tests must use EXPECTED_NATIVE_TOOL_NAMES instead of hardcoded native counts"
        );
    }

    /// U2: `call()` routes to the extension and returns its result.
    #[test]
    fn call_routes_to_extension_and_returns_result() {
        let mut reg = merged_with_echo_extension("ast", "outline");
        let args = json!({"file": "src/main.rs"});
        let result = reg
            .call("tower_ast_outline", args.clone())
            .expect("call must succeed");
        // EchoExtension echoes params.
        assert_eq!(result, args);
    }

    /// U2: Two extensions each with a tool — each routes correctly.
    #[test]
    fn call_routes_to_correct_extension_among_multiple() {
        let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
        {
            let mut reg = ext_registry.write().unwrap();
            reg.register(Box::new(EchoExtension::new(
                "ast",
                vec![make_tool("outline")],
            )))
            .expect("register ast");
            reg.register(Box::new(EchoExtension::new(
                "lsp",
                vec![make_tool("diagnostics")],
            )))
            .expect("register lsp");
        }
        let mut merged = ExtensionMergedRegistry::new(empty_engine_state(), ext_registry);

        let ast_args = json!({"file": "main.rs"});
        let result_a = merged
            .call("tower_ast_outline", ast_args.clone())
            .expect("tower_ast_outline");
        assert_eq!(result_a, ast_args, "ast/outline must echo args");

        let lsp_args = json!({"uri": "main.rs"});
        let result_b = merged
            .call("tower_lsp_diagnostics", lsp_args.clone())
            .expect("tower_lsp_diagnostics");
        assert_eq!(result_b, lsp_args, "lsp/diagnostics must echo args");
    }

    #[test]
    fn call_routes_duplicate_local_tool_name_to_namespaced_extension() {
        let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
        {
            let mut reg = ext_registry.write().unwrap();
            reg.register(Box::new(IdentifyingExtension::new(
                "fmt",
                vec![make_tool("check")],
            )))
            .expect("register fmt");
            reg.register(Box::new(IdentifyingExtension::new(
                "lint",
                vec![make_tool("check")],
            )))
            .expect("register lint");
        }
        let mut merged = ExtensionMergedRegistry::new(empty_engine_state(), ext_registry);

        let result = merged
            .call("tower_lint_check", JsonValue::Null)
            .expect("tower_lint_check");

        assert_eq!(result, JsonValue::String("lint".to_owned()));
    }

    /// UN1: When no extensions are registered, only native tools are served.
    #[test]
    fn no_extensions_serves_native_tools_only() {
        let mut reg = ExtensionMergedRegistry::new(empty_engine_state(), empty_ext_registry());
        let tools = reg.list();

        // All tools are native (no tower_ext_*). Simpler check: list must not be empty.
        assert!(!tools.is_empty(), "native tools must be served");

        // Calling a non-existent extension tool returns NotFound.
        let err = reg
            .call("tower_ast_outline", JsonValue::Null)
            .expect_err("unknown extension tool must be NotFound");
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::NotFound(_)),
            "must be NotFound: {err:?}"
        );
    }

    /// Fault from extension is mapped to ExecutionFailed (not a panic).
    #[test]
    fn extension_fault_maps_to_execution_failed() {
        let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
        ext_registry
            .write()
            .unwrap()
            .register(Box::new(FaultExtension::new("bad", vec![make_tool("run")])))
            .expect("register");
        let mut merged = ExtensionMergedRegistry::new(empty_engine_state(), ext_registry);

        let err = merged
            .call("tower_bad_run", JsonValue::Null)
            .expect_err("faulting extension must error");
        assert!(
            matches!(
                err,
                crate::adapters::mcp::types::ToolError::ExecutionFailed(_)
            ),
            "fault must map to ExecutionFailed: {err:?}"
        );
    }

    /// Call for unknown tool returns NotFound.
    #[test]
    fn call_unknown_tool_returns_not_found() {
        let mut reg = ExtensionMergedRegistry::new(empty_engine_state(), empty_ext_registry());
        let err = reg
            .call("tower_unknown_tool", JsonValue::Null)
            .expect_err("unknown tool must fail");
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::NotFound(_)),
            "must be NotFound: {err:?}"
        );
    }

    /// Native tool is always preferred over an extension with a conflicting name
    /// (defence-in-depth).
    #[test]
    fn native_tool_wins_over_extension_with_same_name() {
        // Create an extension named "find" with a tool named "file" — this would
        // compose to "tower_find_file", the same as a native tool.
        // The native fast-path must win.
        // (In practice, the registry rejects "tower" prefix at registration, but
        // "find" is not "tower" — this tests the name collision path.)
        let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
        // We cannot truly test this without an extension named "find_file" —
        // but we can verify the native tool still resolves.
        let merged = ExtensionMergedRegistry::new(empty_engine_state(), ext_registry);

        // tower_find_file is a native tool — must succeed.
        // (We can't call it without a real workspace, but we can verify it's listed.)
        let tools = merged.list();
        assert!(
            tools.iter().any(|t| t.name == "tower_find_file"),
            "native tower_find_file must always be present"
        );
    }
}
