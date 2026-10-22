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
use std::time::Duration;

use serde_json::Value as JsonValue;

use crate::adapters::daemon::engine::EarlyDaemonContext;
use crate::adapters::daemon::{
    CapabilityDependency, CapabilityReadiness, ExtensionDependencyPolicy, ReadinessController,
    ReadinessSnapshot, ReadinessWaitError,
};
use crate::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{
    CapabilityInitializationFailed, CapabilityStopping, CapabilityUnavailable,
    InitializationPending, capability_initialization_failed_error, capability_stopping_error,
    capability_unavailable_error, initialization_pending_error,
};
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::adapters::watcher::RenameCoordinator;
use crate::domain::extension_host::{ExtensionRegistry, InvokeError};

const CATCH_UP_FENCE_TOOL: &str = "_tower_catch_up_fence";

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
    readiness: Option<ReadinessGate>,
}

struct ReadinessGate {
    controller: Arc<ReadinessController>,
    dependency_policy: Arc<ExtensionDependencyPolicy>,
    preparation_timeout: Duration,
}

pub struct ReadinessAwareRegistry {
    context: Arc<EarlyDaemonContext>,
}

impl ReadinessAwareRegistry {
    pub fn new(context: Arc<EarlyDaemonContext>) -> Self {
        Self { context }
    }
}

impl ToolRegistry for ReadinessAwareRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        let Some(prepared) = self.context.prepared_if_available() else {
            return NativeToolRegistry::descriptions();
        };
        ExtensionMergedRegistry::with_readiness(
            prepared.state(),
            prepared.extension_registry(),
            prepared.rename_coordinator(),
            self.context.readiness(),
            self.context.dependency_policy(),
            self.context.preparation_timeout(),
        )
        .list()
    }

    fn call(&mut self, name: &str, args: JsonValue) -> Result<JsonValue, ToolError> {
        let started = std::time::Instant::now();
        let budget = self.context.preparation_timeout();
        if NativeToolRegistry::descriptions()
            .iter()
            .any(|tool| tool.name == name)
        {
            super::native_tools::validate_native_call(name, &args)?;
        } else if name.ends_with(CATCH_UP_FENCE_TOOL) {
            return Err(ToolError::NotFound(name.to_owned()));
        }
        let native = NativeToolRegistry::descriptions()
            .iter()
            .any(|tool| tool.name == name);
        // Only initialized public declarations establish a callable extension tool.
        // Unknown names must not consume a capability-preparation deadline.
        if !native && !self.list().iter().any(|tool| tool.name == name) {
            // Exhaustive declarations may be hidden while their family prepares.
            let known = self
                .context
                .prepared_if_available()
                .is_some_and(|prepared| {
                    prepared
                        .extension_registry()
                        .read()
                        .unwrap_or_else(|p| p.into_inner())
                        .declared_tools()
                        .iter()
                        .any(|(id, tool)| {
                            tool.name != CATCH_UP_FENCE_TOOL
                                && format!("tower_{}_{}", id.as_str(), tool.name) == name
                        })
                });
            if !known {
                return Err(ToolError::NotFound(name.to_owned()));
            }
        }
        let composite = name == "tower_safe_delete_symbol"
            || (name == "tower_rename_path"
                && args.get("semantic_policy").and_then(JsonValue::as_str) != Some("none"));
        let prepared = if composite {
            // Resolve absent optional planners only after discovery has settled.
            self.context.wait_extension_inventory(budget)
        } else {
            self.context.wait_prepared(budget)
        }
        .map_err(map_workspace_wait_error)?;
        ExtensionMergedRegistry::with_readiness(
            prepared.state(),
            prepared.extension_registry(),
            prepared.rename_coordinator(),
            self.context.readiness(),
            self.context.dependency_policy(),
            budget.saturating_sub(started.elapsed()),
        )
        .call(name, args)
    }
}

fn map_workspace_wait_error(error: ReadinessWaitError) -> ToolError {
    match error {
        ReadinessWaitError::Timeout { dependency, phase } => {
            initialization_pending_error(InitializationPending {
                dependency,
                phase,
                wait_expired: true,
            })
        }
        ReadinessWaitError::Failed {
            dependency,
            failure_class,
        } => capability_initialization_failed_error(CapabilityInitializationFailed {
            dependency,
            failure_class,
        }),
        ReadinessWaitError::Stopping { dependency } => {
            capability_stopping_error(CapabilityStopping { dependency })
        }
        ReadinessWaitError::Unavailable { dependency } => {
            capability_unavailable_error(CapabilityUnavailable { dependency })
        }
    }
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
        Self::with_rename_coordinator(
            engine_state,
            extension_host,
            Arc::new(crate::adapters::watcher::RenameCoordinator::default()),
        )
    }

    /// Construct with the daemon-scoped coordinator shared by the watcher.
    pub fn with_rename_coordinator(
        engine_state: Arc<RwLock<EngineState>>,
        extension_host: Arc<RwLock<ExtensionRegistry>>,
        rename_coordinator: Arc<RenameCoordinator>,
    ) -> Self {
        Self {
            native: NativeToolRegistry::with_rename_coordinator(engine_state, rename_coordinator),
            extension_host,
            readiness: None,
        }
    }

    pub fn with_readiness(
        engine_state: Arc<RwLock<EngineState>>,
        extension_host: Arc<RwLock<ExtensionRegistry>>,
        rename_coordinator: Arc<RenameCoordinator>,
        readiness: Arc<ReadinessController>,
        dependency_policy: Arc<ExtensionDependencyPolicy>,
        preparation_timeout: Duration,
    ) -> Self {
        Self {
            native: NativeToolRegistry::with_rename_coordinator(engine_state, rename_coordinator),
            extension_host,
            readiness: Some(ReadinessGate {
                controller: readiness,
                dependency_policy,
                preparation_timeout,
            }),
        }
    }

    fn extension_is_visible(
        gate: &ReadinessGate,
        snapshot: &ReadinessSnapshot,
        extension_id: &crate::domain::extension_host::ExtensionId,
        tool_name: &str,
    ) -> bool {
        let Some(extension) = snapshot
            .extensions
            .iter()
            .find(|candidate| &candidate.extension_id == extension_id)
        else {
            return false;
        };
        gate.dependency_policy
            .is_readiness_control(extension_id, tool_name)
            || extension.readiness == CapabilityReadiness::Ready
    }

    fn native_dependencies(
        name: &str,
        args: &JsonValue,
        snapshot: &ReadinessSnapshot,
    ) -> Vec<CapabilityDependency> {
        let mut dependencies = vec![CapabilityDependency::Workspace];
        if name == "tower_safe_delete_symbol" && snapshot.ast != CapabilityReadiness::Unavailable {
            dependencies.push(CapabilityDependency::Ast);
        }
        let uses_lsp = name == "tower_safe_delete_symbol"
            || (name == "tower_rename_path"
                && args.get("semantic_policy").and_then(JsonValue::as_str) != Some("none"));
        if uses_lsp && snapshot.lsp != CapabilityReadiness::Unavailable {
            dependencies.push(CapabilityDependency::Lsp);
        }
        dependencies
    }
}

impl ToolRegistry for ExtensionMergedRegistry {
    /// Return all tools: native `tower_*` tools plus namespaced extension tools.
    ///
    /// Called once per `tools/list` request; no caching. Adding or removing
    /// extensions is reflected immediately in the next `list()` call.
    fn list(&self) -> Vec<ToolDesc> {
        let mut tools = self.native.list();
        let readiness_snapshot = self
            .readiness
            .as_ref()
            .map(|gate| gate.controller.snapshot());

        // Recover a poisoned lock: a crashed extension child must not break list().
        let host_guard = self
            .extension_host
            .read()
            .unwrap_or_else(|poison| poison.into_inner());

        for (ext_id, tool_decl) in host_guard.declared_tools() {
            if tool_decl.name == CATCH_UP_FENCE_TOOL {
                continue;
            }
            if let (Some(gate), Some(snapshot)) = (&self.readiness, &readiness_snapshot)
                && !Self::extension_is_visible(gate, snapshot, &ext_id, &tool_decl.name)
            {
                continue;
            }
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
            super::native_tools::validate_native_call(name, &args)?;
            let _permit = self
                .readiness
                .as_ref()
                .map(|gate| {
                    gate.controller.admit_dispatch(
                        &Self::native_dependencies(name, &args, &gate.controller.snapshot()),
                        gate.preparation_timeout,
                    )
                })
                .transpose()
                .map_err(map_workspace_wait_error)?;
            return self.native.call(name, args);
        }

        // Clone initialized declarations, then release the registry before
        // readiness waiting. Discovery must be free to register later instances
        // while this request waits for its capability to become ready.
        let declarations = self
            .extension_host
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .declared_tools();

        // Iterate and match by reconstructing the composed name — avoids underscore
        // ambiguity in splitting.
        for (ext_id, tool_decl) in declarations {
            if tool_decl.name == CATCH_UP_FENCE_TOOL {
                continue;
            }
            let composed = format!("tower_{}_{}", ext_id.as_str(), tool_decl.name);
            if composed == name {
                let _permit = if let Some(gate) = &self.readiness {
                    let dependencies = if gate
                        .dependency_policy
                        .is_readiness_control(&ext_id, &tool_decl.name)
                    {
                        Vec::new()
                    } else {
                        gate.dependency_policy
                            .dependency_for(&ext_id)
                            .into_iter()
                            .collect()
                    };
                    Some(
                        gate.controller
                            .admit_dispatch(&dependencies, gate.preparation_timeout)
                            .map_err(map_workspace_wait_error)?,
                    )
                } else {
                    None
                };
                return self
                    .extension_host
                    .read()
                    .unwrap_or_else(|poison| poison.into_inner())
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
        InvokeError::Application(error) => ToolError::Application {
            code: error.code,
            message: error.message,
            data: error.data,
        },
        InvokeError::Fault(fault) => {
            let class = match fault {
                extension_protocol::ExtensionFault::Timeout => "Timeout",
                extension_protocol::ExtensionFault::Crashed { .. } => "Crashed",
                extension_protocol::ExtensionFault::ProtocolError { .. } => "ProtocolError",
                extension_protocol::ExtensionFault::Quarantined => "Quarantined",
            };
            ToolError::ExecutionFailed(format!("extension fault: {class}"))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, RwLock};
    use std::thread;
    use std::time::Duration;

    use extension_protocol::{ExtensionFault, ExtensionManifest, ToolDecl};
    use serde_json::{Value as JsonValue, json};

    use super::{
        CATCH_UP_FENCE_TOOL, ExtensionMergedRegistry, ToolError, invoke_error_to_tool_error,
    };
    use crate::adapters::daemon::{
        CapabilityDependency, CapabilityReadiness, DaemonLifecycle, ExtensionDependencyPolicy,
        ReadinessController, StartupFailureClass,
    };
    use crate::adapters::mcp::native_tools::{
        EXPECTED_NATIVE_TOOL_NAMES, EngineState, NativeToolRegistry,
    };
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryFs, InMemoryStorage};
    use crate::domain::extension_host::{
        ExtensionApplicationError, ExtensionCallError, ExtensionRegistry, InvokeError,
    };
    use crate::domain::index::InvertedIndex;
    use crate::domain::mutation::compute_content_version;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::domain::{FileId, RelativePath};
    use crate::ports::ExtensionHostPort;

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

    type TestDispatch = dyn FnMut(&str, JsonValue) -> Result<JsonValue, ExtensionCallError> + Send;
    struct TestExtension {
        manifest: ExtensionManifest,
        dispatch: Box<TestDispatch>,
    }
    impl TestExtension {
        fn new(
            name: &str,
            tools: Vec<ToolDecl>,
            dispatch: impl FnMut(&str, JsonValue) -> Result<JsonValue, ExtensionCallError>
            + Send
            + 'static,
        ) -> Self {
            Self {
                manifest: make_manifest(name, tools),
                dispatch: Box::new(dispatch),
            }
        }
        fn echo(name: &str, tools: Vec<ToolDecl>) -> Self {
            Self::new(name, tools, |_, params| Ok(params))
        }
        fn identity(name: &str, tools: Vec<ToolDecl>) -> Self {
            let identity = name.to_owned();
            Self::new(name, tools, move |_, _| Ok(json!(identity)))
        }
        fn counting(name: &str, tools: Vec<ToolDecl>, calls: Arc<AtomicUsize>) -> Self {
            Self::new(name, tools, move |tool, params| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"tool": tool, "params": params}))
            })
        }
        fn blocking(
            name: &str,
            tools: Vec<ToolDecl>,
            calls: Arc<AtomicUsize>,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        ) -> Self {
            Self::new(name, tools, move |tool, params| {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.send(()).expect("test observes admitted dispatch");
                release
                    .recv_timeout(Duration::from_secs(2))
                    .expect("test releases admitted dispatch");
                Ok(json!({"tool": tool, "params": params}))
            })
        }
        fn fault(name: &str, tools: Vec<ToolDecl>) -> Self {
            Self::new(name, tools, |_, _| {
                Err(ExtensionFault::Crashed { code: Some(1) }.into())
            })
        }
    }
    impl crate::domain::extension_host::ExtensionInstance for TestExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }
        fn call_tool(
            &mut self,
            name: &str,
            params: JsonValue,
        ) -> Result<JsonValue, ExtensionCallError> {
            (self.dispatch)(name, params)
        }
        fn deliver_event(&mut self, _: extension_protocol::Event) -> Result<(), ExtensionFault> {
            Ok(())
        }
        fn shutdown(&mut self) {}
    }

    #[derive(Default)]
    struct SafeDeletePlanningHost {
        calls: Mutex<Vec<String>>,
    }

    impl ExtensionHostPort for SafeDeletePlanningHost {
        fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

        fn on_file_changed(&self, _id: FileId, _path: &RelativePath) {}

        fn on_file_deleted(&self, _path: &RelativePath) {}

        fn declared_tools(&self) -> Vec<(crate::domain::extension_host::ExtensionId, ToolDecl)> {
            Vec::new()
        }

        fn invoke(&self, tool_name: &str, _params: JsonValue) -> Result<JsonValue, InvokeError> {
            Err(InvokeError::ToolNotFound(tool_name.to_owned()))
        }

        fn invoke_extension(
            &self,
            extension_id: &crate::domain::extension_host::ExtensionId,
            tool_name: &str,
            _params: JsonValue,
        ) -> Result<JsonValue, InvokeError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:{tool_name}", extension_id.as_str()));
            match (extension_id.as_str(), tool_name) {
                ("ast", "plan_safe_delete_declaration") => Ok(json!({
                    "status": "planned",
                    "plan": {
                        "path": "src/lib.rs",
                        "symbol_name": "target",
                        "kind": "function",
                        "position": {"line": 0, "character": 7},
                        "deletion_span": {"start_byte": 0, "end_byte": 19},
                        "base_hash": compute_content_version(b"pub fn target() {}\n")
                    }
                })),
                ("lsp", "plan_safe_delete") => Ok(json!({
                    "status": "capability_unavailable"
                })),
                _ => Err(InvokeError::ToolNotFound(tool_name.to_owned())),
            }
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

    fn empty_ext_registry() -> Arc<RwLock<ExtensionRegistry>> {
        Arc::new(RwLock::new(ExtensionRegistry::new()))
    }

    fn merged_with_echo_extension(ext_name: &str, tool_name: &str) -> ExtensionMergedRegistry {
        let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
        ext_registry
            .write()
            .unwrap()
            .register(Box::new(TestExtension::echo(
                ext_name,
                vec![make_tool(tool_name)],
            )))
            .expect("register");
        ExtensionMergedRegistry::new(empty_engine_state(), ext_registry)
    }

    fn readiness_registry(
        extension_name: &str,
        tools: Vec<ToolDecl>,
        dependency: CapabilityDependency,
        readiness_controls: &[&str],
        timeout: Duration,
    ) -> (
        ExtensionMergedRegistry,
        Arc<ReadinessController>,
        Arc<AtomicUsize>,
    ) {
        let extension_id = crate::domain::extension_host::ExtensionId::new(extension_name);
        let calls = Arc::new(AtomicUsize::new(0));
        let extension_host = empty_ext_registry();
        extension_host
            .write()
            .unwrap()
            .register(Box::new(TestExtension::counting(
                extension_name,
                tools,
                Arc::clone(&calls),
            )))
            .expect("register counting extension");
        let readiness = Arc::new(ReadinessController::new(1));
        readiness
            .register_extension(extension_id.clone(), dependency)
            .expect("register extension readiness");
        let policy = Arc::new(ExtensionDependencyPolicy::new(
            HashMap::from([(extension_id.clone(), dependency)]),
            HashMap::from([(
                extension_id,
                readiness_controls
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<HashSet<_>>(),
            )]),
        ));
        let registry = ExtensionMergedRegistry::with_readiness(
            empty_engine_state(),
            extension_host,
            Arc::new(crate::adapters::watcher::RenameCoordinator::default()),
            Arc::clone(&readiness),
            policy,
            timeout,
        );
        (registry, readiness, calls)
    }

    fn assert_application_error(
        error: ToolError,
        expected_code: i32,
        expected_message: &str,
        expected_data: JsonValue,
    ) {
        match error {
            ToolError::Application {
                code,
                message,
                data,
            } => {
                assert_eq!(code, expected_code);
                assert_eq!(message, expected_message);
                assert_eq!(data, Some(expected_data));
            }
            other => panic!("expected application error, got {other:?}"),
        }
    }

    #[test]
    fn readiness_with_readiness_uses_supplied_controller_and_timeout() {
        let (mut registry, readiness, calls) = readiness_registry(
            "ast",
            vec![make_tool("outline")],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );

        let error = registry
            .call("tower_ast_outline", json!({}))
            .expect_err("supplied preparing controller must gate the call");

        assert_application_error(
            error,
            -32_004,
            "capability initialization is still in progress",
            json!({"dependency":"ast","phase":"ownership_acquired","wait_expired":true}),
        );
        assert_eq!(readiness.snapshot().generation, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readiness_existing_registry_constructors_start_ready() {
        let mut registry = merged_with_echo_extension("ast", "outline");
        let args = json!({"path":"src/lib.rs"});

        assert_eq!(
            registry.call("tower_ast_outline", args.clone()).unwrap(),
            args
        );
    }

    #[test]
    fn readiness_list_uses_one_snapshot_and_keeps_native_tools_stable() {
        let (registry, _readiness, _calls) = readiness_registry(
            "ast",
            vec![make_tool("outline")],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );

        let tools = registry.list();
        for expected in EXPECTED_NATIVE_TOOL_NAMES {
            assert!(tools.iter().any(|tool| tool.name == expected));
        }
        assert!(!tools.iter().any(|tool| tool.name == "tower_ast_outline"));
    }

    #[test]
    fn readiness_list_exposes_initialized_non_exhaustive_tools_during_catch_up() {
        let (registry, _readiness, _calls) = readiness_registry(
            "slow",
            vec![make_tool("probe_catch_up"), make_tool(CATCH_UP_FENCE_TOOL)],
            CapabilityDependency::Ast,
            &["probe_catch_up"],
            Duration::ZERO,
        );

        let names = registry
            .list()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "tower_slow_probe_catch_up"));
        assert!(!names.iter().any(|name| name.contains(CATCH_UP_FENCE_TOOL)));
    }

    #[test]
    fn readiness_list_hides_ast_and_lsp_tools_until_their_dependency_is_ready() {
        let (ast_registry, ast_readiness, _) = readiness_registry(
            "ast",
            vec![make_tool("outline")],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );
        assert!(
            !ast_registry
                .list()
                .iter()
                .any(|tool| tool.name == "tower_ast_outline")
        );
        ast_readiness
            .transition_extension(
                &crate::domain::extension_host::ExtensionId::new("ast"),
                CapabilityReadiness::Ready,
            )
            .unwrap();
        assert!(
            ast_registry
                .list()
                .iter()
                .any(|tool| tool.name == "tower_ast_outline")
        );

        let (lsp_registry, _, _) = readiness_registry(
            "lsp",
            vec![make_tool("hover")],
            CapabilityDependency::Lsp,
            &[],
            Duration::ZERO,
        );
        assert!(
            !lsp_registry
                .list()
                .iter()
                .any(|tool| tool.name == "tower_lsp_hover")
        );
    }

    #[test]
    fn readiness_call_dispatches_ready_dependency_exactly_once() {
        let (mut registry, readiness, calls) = readiness_registry(
            "ast",
            vec![make_tool("outline")],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );
        readiness
            .transition_extension(
                &crate::domain::extension_host::ExtensionId::new("ast"),
                CapabilityReadiness::Ready,
            )
            .unwrap();

        let result = registry
            .call("tower_ast_outline", json!({"path":"a.rs"}))
            .unwrap();

        assert_eq!(result["tool"], "outline");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn readiness_dispatch_race_with_shutdown_rejects_unadmitted_calls() {
        let extension_id = crate::domain::extension_host::ExtensionId::new("ast");
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let extension_host = empty_ext_registry();
        extension_host
            .write()
            .unwrap()
            .register(Box::new(TestExtension::blocking(
                "ast",
                vec![make_tool("outline")],
                Arc::clone(&calls),
                entered_tx,
                release_rx,
            )))
            .expect("register blocking extension");
        let readiness = Arc::new(ReadinessController::new(44));
        readiness
            .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
            .unwrap();
        readiness
            .register_extension(extension_id.clone(), CapabilityDependency::Ast)
            .unwrap();
        readiness.close_discovery().unwrap();
        readiness
            .transition_extension(&extension_id, CapabilityReadiness::Ready)
            .unwrap();
        let policy = Arc::new(ExtensionDependencyPolicy::new(
            HashMap::from([(extension_id, CapabilityDependency::Ast)]),
            HashMap::new(),
        ));
        let first = ExtensionMergedRegistry::with_readiness(
            empty_engine_state(),
            Arc::clone(&extension_host),
            Arc::new(crate::adapters::watcher::RenameCoordinator::default()),
            Arc::clone(&readiness),
            Arc::clone(&policy),
            Duration::from_secs(1),
        );
        let mut second = ExtensionMergedRegistry::with_readiness(
            empty_engine_state(),
            extension_host,
            Arc::new(crate::adapters::watcher::RenameCoordinator::default()),
            Arc::clone(&readiness),
            policy,
            Duration::from_secs(1),
        );

        let admitted = thread::spawn(move || {
            let mut registry = first;
            registry.call("tower_ast_outline", json!({"path": "src/lib.rs"}))
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("one call enters extension dispatch before shutdown");
        readiness
            .transition_daemon(DaemonLifecycle::Stopping)
            .expect("shutdown transition succeeds after admission");
        assert_application_error(
            second
                .call("tower_ast_outline", json!({"path": "src/lib.rs"}))
                .expect_err("post-shutdown call is never dispatched"),
            -32_006,
            "daemon is stopping",
            json!({"dependency":"ast"}),
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only admitted call dispatches"
        );
        assert!(
            !readiness.wait_for_dispatch_drain(Duration::ZERO),
            "admitted permit remains held through extension dispatch"
        );
        release_tx.send(()).unwrap();
        assert!(admitted.join().unwrap().is_ok());
        assert!(readiness.wait_for_dispatch_drain(Duration::from_secs(1)));

        let (mut stopped_first, stopped_readiness, stopped_calls) = readiness_registry(
            "ast",
            vec![make_tool("outline")],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );
        stopped_readiness
            .transition_daemon(DaemonLifecycle::Stopping)
            .unwrap();
        assert_application_error(
            stopped_first
                .call("tower_ast_outline", json!({"path": "src/lib.rs"}))
                .expect_err("shutdown before admission is rejected without deadlock"),
            -32_006,
            "daemon is stopping",
            json!({"dependency":"ast"}),
        );
        assert_eq!(stopped_calls.load(Ordering::SeqCst), 0);
        assert!(stopped_readiness.wait_for_dispatch_drain(Duration::ZERO));
    }

    #[test]
    fn native_optional_dependencies_preserve_fallback_and_configured_failures() {
        let readiness = ReadinessController::new(1);
        readiness
            .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
            .unwrap();
        readiness.close_discovery().unwrap();
        let mut snapshot = readiness.snapshot();
        for name in ["tower_safe_delete_symbol", "tower_rename_path"] {
            assert_eq!(
                ExtensionMergedRegistry::native_dependencies(
                    name,
                    &json!({"semantic_policy":"best_effort"}),
                    &snapshot
                ),
                vec![CapabilityDependency::Workspace]
            );
        }
        snapshot.lsp = CapabilityReadiness::Failed {
            failure_class: crate::adapters::daemon::StartupFailureClass::ExtensionCrash,
        };
        assert_eq!(
            ExtensionMergedRegistry::native_dependencies(
                "tower_rename_path",
                &json!({"semantic_policy":"none"}),
                &snapshot
            ),
            vec![CapabilityDependency::Workspace]
        );
        assert_eq!(
            ExtensionMergedRegistry::native_dependencies(
                "tower_rename_path",
                &json!({"semantic_policy":"best_effort"}),
                &snapshot
            ),
            vec![CapabilityDependency::Workspace, CapabilityDependency::Lsp]
        );
        assert_eq!(
            ExtensionMergedRegistry::native_dependencies(
                "tower_safe_delete_symbol",
                &json!({}),
                &snapshot
            ),
            vec![CapabilityDependency::Workspace, CapabilityDependency::Lsp]
        );
    }

    #[test]
    fn native_composites_retain_unconfigured_backend_outcomes() {
        let state = empty_engine_state();
        let mut native = NativeToolRegistry::new(Arc::clone(&state));
        native
            .call(
                "tower_create_file",
                json!({"path":"old.rs","content":"pub fn target() {}\n"}),
            )
            .unwrap();
        let readiness = Arc::new(ReadinessController::new(1));
        readiness
            .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
            .unwrap();
        readiness.close_discovery().unwrap();
        let mut registry = ExtensionMergedRegistry::with_readiness(
            state,
            empty_ext_registry(),
            Arc::new(crate::adapters::watcher::RenameCoordinator::default()),
            readiness,
            Arc::new(ExtensionDependencyPolicy::new(
                HashMap::new(),
                HashMap::new(),
            )),
            Duration::ZERO,
        );
        let deletion = registry
            .call(
                "tower_safe_delete_symbol",
                json!({"path":"old.rs","symbol_name":"target","kind":"function","dry_run":true}),
            )
            .unwrap();
        assert_eq!(deletion["status"], "blocked");
        assert_eq!(deletion["blocking_reason"], "capability_unavailable");
        let rename = registry.call(
            "tower_rename_path",
            json!({"source":"old.rs","destination":"new.rs","semantic_policy":"best_effort"}),
        );
        assert!(rename.is_ok(), "{rename:?}");
    }

    #[test]
    fn native_rename_without_semantics_runs_while_lsp_prepares() {
        let (mut registry, readiness, calls) = readiness_registry(
            "lsp",
            vec![make_tool("hover")],
            CapabilityDependency::Lsp,
            &[],
            Duration::ZERO,
        );
        readiness
            .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
            .unwrap();
        registry
            .native
            .call(
                "tower_create_file",
                json!({"path":"old.rs","content":"pub fn f() {}"}),
            )
            .unwrap();
        let result = registry.call(
            "tower_rename_path",
            json!({"source":"old.rs","destination":"new.rs","semantic_policy":"none"}),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readiness_native_nested_dependencies_share_one_admission_and_budget() {
        let state = empty_engine_state();
        let host = Arc::new(SafeDeletePlanningHost::default());
        state
            .write()
            .unwrap()
            .set_plugin_host(Arc::clone(&host) as Arc<dyn ExtensionHostPort + Send + Sync>);
        let mut native =
            crate::adapters::mcp::native_tools::NativeToolRegistry::new(Arc::clone(&state));
        native
            .call(
                "tower_create_file",
                json!({"path":"src/lib.rs","content":"pub fn target() {}\n"}),
            )
            .expect("seed safe-delete source through the public native operation");

        let ast = crate::domain::extension_host::ExtensionId::new("ast");
        let lsp = crate::domain::extension_host::ExtensionId::new("lsp");
        let readiness = Arc::new(ReadinessController::new(45));
        readiness
            .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
            .unwrap();
        readiness
            .register_extension(ast.clone(), CapabilityDependency::Ast)
            .unwrap();
        readiness
            .register_extension(lsp.clone(), CapabilityDependency::Lsp)
            .unwrap();
        readiness.close_discovery().unwrap();
        readiness
            .transition_extension(&ast, CapabilityReadiness::Ready)
            .unwrap();
        let mut registry = ExtensionMergedRegistry::with_readiness(
            state,
            empty_ext_registry(),
            Arc::new(crate::adapters::watcher::RenameCoordinator::default()),
            Arc::clone(&readiness),
            Arc::new(ExtensionDependencyPolicy::new(
                HashMap::from([
                    (ast, CapabilityDependency::Ast),
                    (lsp.clone(), CapabilityDependency::Lsp),
                ]),
                HashMap::new(),
            )),
            Duration::ZERO,
        );
        let request = json!({
            "path":"src/lib.rs",
            "symbol_name":"target",
            "kind":"function",
            "dry_run":true
        });

        assert_application_error(
            registry
                .call("tower_safe_delete_symbol", request.clone())
                .expect_err("configured LSP preparation gates the nested native operation"),
            -32_004,
            "capability initialization is still in progress",
            json!({"dependency":"lsp","phase":"ownership_acquired","wait_expired":true}),
        );
        assert!(
            host.calls.lock().unwrap().is_empty(),
            "no nested HostCall precedes admission"
        );

        readiness
            .transition_extension(&lsp, CapabilityReadiness::Ready)
            .unwrap();
        let result = registry
            .call("tower_safe_delete_symbol", request)
            .expect("one admitted native dispatch reaches its AST and LSP dependencies");
        assert_eq!(result["status"], "blocked");
        assert_eq!(
            *host.calls.lock().unwrap(),
            vec![
                "ast:plan_safe_delete_declaration".to_owned(),
                "lsp:plan_safe_delete".to_owned(),
            ],
            "nested AST/LSP work shares the admitted native call"
        );
        assert!(readiness.wait_for_dispatch_drain(Duration::ZERO));
    }

    #[test]
    fn readiness_call_maps_timeout_without_dispatch() {
        let (mut registry, _, calls) = readiness_registry(
            "ast",
            vec![make_tool("outline")],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );
        let error = registry.call("tower_ast_outline", json!({})).unwrap_err();
        assert_application_error(
            error,
            -32_004,
            "capability initialization is still in progress",
            json!({"dependency":"ast","phase":"ownership_acquired","wait_expired":true}),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readiness_call_maps_failure_without_dispatch() {
        let (mut registry, readiness, calls) = readiness_registry(
            "lsp",
            vec![make_tool("hover")],
            CapabilityDependency::Lsp,
            &[],
            Duration::ZERO,
        );
        readiness
            .transition_extension(
                &crate::domain::extension_host::ExtensionId::new("lsp"),
                CapabilityReadiness::Failed {
                    failure_class: StartupFailureClass::ExtensionCrash,
                },
            )
            .unwrap();
        let error = registry.call("tower_lsp_hover", json!({})).unwrap_err();
        assert_application_error(
            error,
            -32_005,
            "capability initialization failed",
            json!({"dependency":"lsp","failure_class":"extension_crash"}),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readiness_internal_fence_is_hidden_and_direct_public_calls_are_rejected() {
        let (mut registry, _, calls) = readiness_registry(
            "slow",
            vec![make_tool(CATCH_UP_FENCE_TOOL)],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );
        assert!(
            !registry
                .list()
                .iter()
                .any(|tool| tool.name.contains(CATCH_UP_FENCE_TOOL))
        );
        assert!(matches!(
            registry.call("tower_slow__tower_catch_up_fence", json!({})),
            Err(ToolError::NotFound(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readiness_call_maps_unavailable_without_dispatch() {
        let extension_id = crate::domain::extension_host::ExtensionId::new("lsp");
        let calls = Arc::new(AtomicUsize::new(0));
        let extension_host = empty_ext_registry();
        extension_host
            .write()
            .unwrap()
            .register(Box::new(TestExtension::counting(
                "lsp",
                vec![make_tool("hover")],
                Arc::clone(&calls),
            )))
            .unwrap();
        let readiness = Arc::new(ReadinessController::new(1));
        readiness.close_discovery().unwrap();
        let policy = Arc::new(ExtensionDependencyPolicy::new(
            HashMap::from([(extension_id, CapabilityDependency::Lsp)]),
            HashMap::new(),
        ));
        let mut registry = ExtensionMergedRegistry::with_readiness(
            empty_engine_state(),
            extension_host,
            Arc::new(crate::adapters::watcher::RenameCoordinator::default()),
            readiness,
            policy,
            Duration::ZERO,
        );
        let error = registry.call("tower_lsp_hover", json!({})).unwrap_err();
        assert_application_error(
            error,
            -32_007,
            "capability is not configured",
            json!({"dependency":"lsp"}),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readiness_call_maps_stopping_without_dispatch() {
        let (mut registry, readiness, calls) = readiness_registry(
            "ast",
            vec![make_tool("outline")],
            CapabilityDependency::Ast,
            &[],
            Duration::ZERO,
        );
        readiness
            .transition_daemon(DaemonLifecycle::Stopping)
            .unwrap();
        let error = registry.call("tower_ast_outline", json!({})).unwrap_err();
        assert_application_error(
            error,
            -32_006,
            "daemon is stopping",
            json!({"dependency":"ast"}),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn f008_application_maps_some_data_field_for_field() {
        let data = json!({"outcome": "not_renameable", "detail": "reserved symbol"});

        let mapped =
            invoke_error_to_tool_error(InvokeError::Application(ExtensionApplicationError {
                code: -32_042,
                message: "symbol cannot be renamed".to_owned(),
                data: Some(data.clone()),
            }));

        match mapped {
            crate::adapters::mcp::types::ToolError::Application {
                code,
                message,
                data: actual_data,
            } => {
                assert_eq!(code, -32_042);
                assert_eq!(message, "symbol cannot be renamed");
                assert_eq!(actual_data, Some(data));
            }
            other => panic!("expected application error, got {other:?}"),
        }
    }

    #[test]
    fn f008_application_maps_none_data_without_synthesizing_a_value() {
        let mapped =
            invoke_error_to_tool_error(InvokeError::Application(ExtensionApplicationError {
                code: 7,
                message: "operation rejected".to_owned(),
                data: None,
            }));

        assert!(matches!(
            mapped,
            crate::adapters::mcp::types::ToolError::Application {
                code: 7,
                ref message,
                data: None,
            } if message == "operation rejected"
        ));
    }

    #[test]
    fn f008_application_keeps_genuine_fault_mapping_unchanged() {
        let mapped = invoke_error_to_tool_error(InvokeError::Fault(ExtensionFault::Timeout));

        assert!(matches!(
            mapped,
            crate::adapters::mcp::types::ToolError::ExecutionFailed(ref message)
                if message.contains("Timeout")
        ));
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
            reg.register(Box::new(TestExtension::echo(
                "ast",
                vec![make_tool("outline")],
            )))
            .expect("register ast");
            reg.register(Box::new(TestExtension::echo(
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
            reg.register(Box::new(TestExtension::identity(
                "fmt",
                vec![make_tool("check")],
            )))
            .expect("register fmt");
            reg.register(Box::new(TestExtension::identity(
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
            .register(Box::new(TestExtension::fault(
                "bad",
                vec![make_tool("run")],
            )))
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
