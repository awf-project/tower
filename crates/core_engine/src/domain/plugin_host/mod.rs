//! Plugin host registry — runtime-agnostic, domain-pure (spec 11b).
//!
//! # Wireframe
//!
//! ```text
//! trait PluginInstance { manifest(); call_tool(name,args); deliver_hook(kind,payload); }
//! PluginHostRegistry : PluginHostPort
//!   register(Box<dyn PluginInstance>)
//!   on_file_indexed(FileId, path)  ─┐ fan-out to instances subscribed to that hook
//!   on_file_changed(FileId, path)  ─┘
//!   declared_tools() -> Vec<(PluginId, ToolDesc)>
//! No-op behaviour when no plugin registered (OP1).
//! ```
//!
//! # Hexagonal boundary
//!
//! This module imports only `plugin_sdk` (pure, no I/O) and `crate::domain` /
//! `crate::ports`. It deliberately imports no `wasmtime`, `sled`, `std::fs`, or
//! `notify`. The wasmtime adapter (11c) will implement `PluginInstance` and
//! inject itself via `register`.
//!
//! # Interior mutability
//!
//! `PluginHostPort` is object-safe and requires `&self` on `on_file_indexed` /
//! `on_file_changed` so the domain can hold a `&dyn PluginHostPort`. Meanwhile,
//! `PluginInstance::deliver_hook` requires `&mut self` (exclusive wasm instance
//! access). The registry bridges these by wrapping each instance in a
//! `Mutex<Box<dyn PluginInstance>>`, which is both `Send + Sync`.
//!
//! Decision: `Mutex` over `RefCell` because `EventProcessor` stores
//! `Box<dyn PluginHostPort + Send + Sync>`. A single-threaded `RefCell` would
//! not satisfy `Sync`. The Mutex is uncontended in practice (fan-out is
//! sequential within one thread) so there is no performance concern.
//! Trade-off: slightly higher overhead per hook vs. `RefCell`; acceptable.
//!
//! # Error handling
//!
//! `deliver_hook` failures are isolated per-plugin (UN1): if one instance errors,
//! the error is logged to stderr and delivery continues to the remaining instances.
//! A single bad plugin cannot block others.
#![forbid(unsafe_code)]

use std::sync::Mutex;

use plugin_sdk::{ABI_VERSION, HookKind, HookPayload, PluginManifest, ToolDesc, Value};

use crate::domain::{FileId, RelativePath};
use crate::ports::PluginHostPort;

mod worker;

// Consumed by the registry refactor (Task 2); unused until then.
#[allow(unused_imports)]
pub(crate) use worker::{Command, PLUGIN_MAILBOX_CAPACITY, run_worker};

// ── PluginId ─────────────────────────────────────────────────────────────────

/// A stable identifier for a registered plugin, derived from its manifest name.
///
/// `PluginId` is the namespace key used in [`PluginHostRegistry::declared_tools`]
/// to associate each tool with the plugin that declared it (spec U1 / 12b MCP
/// merge).
///
/// # Examples
///
/// ```rust
/// use core_engine::domain::plugin_host::PluginId;
///
/// let id = PluginId::new("hello");
/// assert_eq!(id.as_str(), "hello");
/// assert_eq!(id.to_string(), "hello");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PluginId(String);

impl PluginId {
    /// Create a new `PluginId` from the given name.
    ///
    /// The name should match [`PluginManifest::name`] for consistency.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Return the underlying name string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── PluginHostError ───────────────────────────────────────────────────────────

/// Errors from [`PluginInstance`] operations.
///
/// Returned by [`PluginInstance::call_tool`] and [`PluginInstance::deliver_hook`].
/// The registry treats `deliver_hook` errors as isolated failures (UN1) and logs
/// them without propagating to callers.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginHostError {
    /// The named tool is not declared in the plugin's manifest.
    ToolNotFound(String),
    /// The tool arguments were structurally invalid.
    InvalidArgs(String),
    /// The tool ran but produced a runtime error.
    CallFailed(String),
    /// Hook delivery to this plugin instance failed.
    HookDeliveryFailed(String),
    /// The plugin sandbox trapped, panicked, or exceeded its compute budget
    /// (fuel exhaustion or epoch deadline). The sandbox is marked failed and
    /// will be restarted by the supervisor (spec 11d UN1/UN2).
    ///
    /// The host process and MCP link survive this error.
    PluginFault(PluginFaultKind),
}

/// The specific cause of a [`PluginHostError::PluginFault`].
///
/// All variants leave the host alive — the fault is isolated to the sandbox.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginFaultKind {
    /// The wasm guest raised an `unreachable` trap, panicked (which compiles to
    /// `unreachable` in wasm32-wasip1), or triggered a wasmtime runtime trap.
    Trapped(String),
    /// The guest exceeded its per-call fuel budget (deterministic compute bound).
    FuelExhausted,
    /// The guest exceeded its epoch deadline (wall-clock compute bound).
    EpochDeadlineExceeded,
    /// The sandbox has been quarantined after repeated restart failures and will
    /// not be restarted again. The plugin is reported unhealthy (UN3/AC4).
    Quarantined,
}

impl std::fmt::Display for PluginHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolNotFound(n) => write!(f, "tool not found: {n}"),
            Self::InvalidArgs(m) => write!(f, "invalid args: {m}"),
            Self::CallFailed(m) => write!(f, "call failed: {m}"),
            Self::HookDeliveryFailed(m) => write!(f, "hook delivery failed: {m}"),
            Self::PluginFault(kind) => write!(f, "plugin fault: {kind}"),
        }
    }
}

impl std::fmt::Display for PluginFaultKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trapped(msg) => write!(f, "wasm trap: {msg}"),
            Self::FuelExhausted => write!(f, "fuel budget exhausted"),
            Self::EpochDeadlineExceeded => write!(f, "epoch deadline exceeded"),
            Self::Quarantined => write!(f, "plugin is quarantined after repeated failures"),
        }
    }
}

impl std::error::Error for PluginHostError {}

// ── RegistrationError ─────────────────────────────────────────────────────────

/// Errors that prevent a plugin from being registered.
///
/// Returned by [`PluginHostRegistry::register`] when a pre-registration check
/// fails. The most important check is the ABI version guard: a plugin built
/// against a different ABI revision has an incompatible postcard layout for all
/// DTOs and must be rejected before any dispatch can occur.
///
/// # Examples
///
/// ```rust
/// use core_engine::domain::plugin_host::{PluginHostRegistry, RegistrationError};
///
/// // A registry starts empty and does not error.
/// let mut registry = PluginHostRegistry::new();
/// assert!(registry.declared_tools().is_empty());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum RegistrationError {
    /// The plugin was compiled against a different ABI version.
    ///
    /// `expected` is [`plugin_sdk::ABI_VERSION`]; `got` is `manifest.abi`.
    AbiMismatch {
        /// The ABI version the host requires.
        expected: u32,
        /// The ABI version the plugin was compiled against.
        got: u32,
    },
    /// A plugin with this manifest name is already registered.
    ///
    /// Plugin names are used as routing keys in `call_instance_tool` (spec 12b).
    /// Allowing two plugins with the same name would make the second one
    /// permanently unreachable: every `tools/call` for any of its tools would
    /// silently route to the first plugin instead — a violation of UN1
    /// ("never silently shadow"). Reject the duplicate at registration time so
    /// the problem is surfaced immediately rather than at call time.
    DuplicateName(String),
    /// The plugin manifest name begins with the reserved prefix `"tower"`.
    ///
    /// Plugin tools are namespaced as `"tower_<plugin_id>_<tool_name>"` in the
    /// MCP merged registry (spec 12b). The `"tower"` prefix is reserved for the
    /// host namespace. A plugin named e.g. `"tower_find"` with tool `"file"` would
    /// compose to `"tower_tower_find_file"` — confusing — and a plugin named
    /// exactly `"tower"` with tool `"find_file"` would compose to
    /// `"tower_tower_find_file"` — a namespace impersonation attack vector.
    ///
    /// This is a plugin-identity policy, not an MCP-transport rule: it is
    /// enforced here in the domain with no coupling to any transport or adapter.
    ReservedNamePrefix(String),
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AbiMismatch { expected, got } => {
                write!(f, "ABI mismatch: host={expected}, plugin={got}")
            }
            Self::DuplicateName(name) => {
                write!(f, "a plugin named '{name}' is already registered")
            }
            Self::ReservedNamePrefix(name) => {
                write!(
                    f,
                    "plugin name '{name}' begins with the reserved prefix 'tower'; \
                     plugin names must not start with 'tower' (host namespace)"
                )
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

// ── PluginInstance ────────────────────────────────────────────────────────────

/// The host-side abstraction over a single loaded plugin (spec 11b U2).
///
/// The wasmtime adapter (spec 11c) implements this trait to wrap a live wasm
/// instance. Tests use fake implementations with no wasm machinery (DoD).
///
/// # Object safety
///
/// The trait is intentionally object-safe so the registry can store
/// `Vec<Mutex<Box<dyn PluginInstance>>>` and dispatch through dynamic dispatch.
///
/// # Mutability contract
///
/// `call_tool` and `deliver_hook` take `&mut self` because wasm instance
/// execution requires exclusive access to the instance's linear memory and
/// call stack. The registry provides this exclusivity via a `Mutex` per instance.
pub trait PluginInstance: Send {
    /// Return the manifest this plugin declared at load time.
    ///
    /// The manifest is immutable for the plugin's lifetime: tools and hook
    /// subscriptions do not change after registration.
    fn manifest(&self) -> &PluginManifest;

    /// Invoke a named tool with the provided arguments.
    ///
    /// # Errors
    ///
    /// - [`PluginHostError::ToolNotFound`] — tool name not in manifest.
    /// - [`PluginHostError::InvalidArgs`] — arguments structurally invalid.
    /// - [`PluginHostError::CallFailed`] — tool ran but raised an error.
    fn call_tool(&mut self, name: &str, args: Value) -> Result<Value, PluginHostError>;

    /// Deliver a lifecycle hook to this plugin instance.
    ///
    /// The registry only calls this for hooks the plugin subscribed to in its
    /// manifest (EV2). Unsubscribed hooks are never delivered.
    ///
    /// # Errors
    ///
    /// [`PluginHostError::HookDeliveryFailed`] on any runtime error during
    /// delivery. The registry isolates this failure (UN1) and continues
    /// delivering to other plugins.
    fn deliver_hook(&mut self, kind: HookKind, payload: HookPayload)
    -> Result<(), PluginHostError>;
}

// ── PluginHostRegistry ────────────────────────────────────────────────────────

/// Registry implementing [`PluginHostPort`]: stores plugin instances, fans out
/// lifecycle hooks, and aggregates declared tools.
///
/// # Registration
///
/// Call [`register`][Self::register] once per plugin at startup (or hot-reload).
/// The registry takes ownership of the instance. Tools and hooks are read from
/// the manifest on every relevant operation.
///
/// # Subscription filtering (EV2)
///
/// On `on_file_indexed` / `on_file_changed`, only instances whose manifest
/// contains the corresponding [`HookKind`] receive the delivery call.
/// Unsubscribed instances incur zero overhead.
///
/// # Error isolation (UN1)
///
/// If `deliver_hook` returns an error for a given instance the registry logs the
/// error to stderr and continues delivering to remaining instances. One bad
/// plugin cannot block others.
///
/// # No-op when empty (OP1)
///
/// An empty registry behaves identically to [`crate::ports::NoOpPluginHost`]:
/// hooks fire but nothing happens; `declared_tools` returns an empty `Vec`.
///
/// # Examples
///
/// ```rust
/// use core_engine::domain::plugin_host::PluginHostRegistry;
///
/// let registry = PluginHostRegistry::new();
/// // No plugins registered — declared_tools is empty (OP1 / AC3).
/// assert!(registry.declared_tools().is_empty());
/// ```
pub struct PluginHostRegistry {
    // Decision: Mutex<Box<dyn PluginInstance>> per slot.
    // Why: PluginHostPort takes &self (object-safety), but deliver_hook needs
    //      &mut self. Mutex provides interior mutability that is Send+Sync.
    // Trade-off: each hook delivery acquires/releases one Mutex per subscribed
    //            instance, sequential — no contention in practice.
    instances: Vec<Mutex<Box<dyn PluginInstance>>>,
}

impl std::fmt::Debug for PluginHostRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHostRegistry")
            .field("plugin_count", &self.instances.len())
            .finish()
    }
}

impl PluginHostRegistry {
    /// Create an empty registry (no plugins registered).
    ///
    /// Lifecycle hooks are no-ops until at least one plugin is registered (OP1).
    #[must_use]
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
        }
    }

    /// Register a plugin instance with the registry (EV1).
    ///
    /// The registry takes ownership of the boxed instance. The manifest is
    /// read lazily on each dispatch — registration only moves the instance into
    /// the slot.
    ///
    /// # ABI guard
    ///
    /// The plugin's [`PluginManifest::abi`] is compared against
    /// [`plugin_sdk::ABI_VERSION`] before the instance is stored. A mismatch
    /// means the plugin's postcard DTO layout is incompatible with the host's
    /// types; continuing would produce silent data corruption. The instance is
    /// rejected and returned to the caller as [`RegistrationError::AbiMismatch`].
    ///
    /// # Errors
    ///
    /// Returns [`RegistrationError::AbiMismatch`] when the plugin's ABI version
    /// does not match the host's [`plugin_sdk::ABI_VERSION`].
    ///
    /// # Examples
    ///
    /// See [`PluginHostRegistry`] doc examples for usage.
    pub fn register(&mut self, instance: Box<dyn PluginInstance>) -> Result<(), RegistrationError> {
        let plugin_abi = instance.manifest().abi;
        if plugin_abi != ABI_VERSION {
            return Err(RegistrationError::AbiMismatch {
                expected: ABI_VERSION,
                got: plugin_abi,
            });
        }

        // Decision: reject plugin names that begin with "tower".
        // Why: the MCP merged registry (12b) composes plugin tool names as
        //      "tower_<plugin_id>_<tool_name>". A plugin named "tower" (or any
        //      name starting with "tower") would produce composed names that are
        //      indistinguishable from host-native tool names — namespace
        //      impersonation. This is a plugin-identity policy enforced here in
        //      the domain, independent of any transport or adapter.
        // Trade-off: plugin authors must choose names that do not begin with
        //      "tower". This is a permanent, documented invariant.
        let new_name = instance.manifest().name.clone();
        if new_name.starts_with("tower") {
            return Err(RegistrationError::ReservedNamePrefix(new_name));
        }

        // Decision: enforce unique manifest names at registration time.
        // Why: call_instance_tool() routes by name and returns on first match.
        //      A second plugin with the same name would be permanently unreachable:
        //      any tools/call for its tools silently routes to the first — UN1.
        // Trade-off: hot-reload must use a different name or remove the old plugin
        //      first; this is an acceptable constraint for the current design.
        let duplicate = self.instances.iter().any(|slot| {
            slot.lock()
                .unwrap_or_else(|p| p.into_inner())
                .manifest()
                .name
                == new_name
        });
        if duplicate {
            return Err(RegistrationError::DuplicateName(new_name));
        }

        self.instances.push(Mutex::new(instance));
        Ok(())
    }

    /// Return all tools declared by registered plugins, each tagged by plugin
    /// identity (U1).
    ///
    /// The [`PluginId`] is derived from [`PluginManifest::name`]. Tools from
    /// multiple plugins are concatenated in registration order.
    ///
    /// The MCP merge layer (spec 12b) consumes this to expose plugin tools
    /// alongside native tools.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use core_engine::domain::plugin_host::PluginHostRegistry;
    ///
    /// let registry = PluginHostRegistry::new();
    /// assert!(registry.declared_tools().is_empty());
    /// ```
    pub fn declared_tools(&self) -> Vec<(PluginId, ToolDesc)> {
        self.instances
            .iter()
            .flat_map(|slot| {
                // Lock is held briefly to read the manifest — no I/O inside.
                // Decision: recover poisoned mutex instead of panicking.
                // Why: a wasm trap inside deliver_hook (11c) can poison the
                //      mutex; recovering the inner value preserves UN1 isolation
                //      (one bad plugin must not block declared_tools for others).
                // Trade-off: we expose potentially partially-written state, but
                //      PluginInstance::manifest() is read-only so this is safe.
                let instance = slot.lock().unwrap_or_else(|p| p.into_inner());
                let id = PluginId::new(instance.manifest().name.clone());
                instance
                    .manifest()
                    .tools
                    .iter()
                    .map(|tool| (id.clone(), tool.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Invoke a named tool on the first registered plugin whose manifest name
    /// matches `plugin_id_str` (spec 12b routing).
    ///
    /// This is the single point where the MCP adapter layer (12b) calls into a
    /// specific plugin instance. The call goes through the per-instance `Mutex`
    /// so it is exclusive — no concurrent tool calls on the same instance.
    ///
    /// Fault isolation is transparent: if the instance is wrapped in an
    /// `IsolatedSandbox` (11d), the sandbox catches any wasm trap and returns
    /// `PluginHostError::PluginFault`. The adapter layer (12b) maps that to a
    /// `ToolError::ExecutionFailed` so the MCP link stays alive.
    ///
    /// # Errors
    ///
    /// - [`PluginHostError::ToolNotFound`] — no registered plugin has the given `plugin_id_str`.
    /// - Any [`PluginHostError`] variant returned by the matching plugin instance.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use core_engine::domain::plugin_host::{PluginHostRegistry, PluginHostError};
    ///
    /// let registry = PluginHostRegistry::new();
    /// let result = registry.call_instance_tool("nonexistent", "greet", plugin_sdk::Value::Null);
    /// assert!(matches!(result, Err(PluginHostError::ToolNotFound(_))));
    /// ```
    pub fn call_instance_tool(
        &self,
        plugin_id_str: &str,
        tool_name: &str,
        args: plugin_sdk::Value,
    ) -> Result<plugin_sdk::Value, PluginHostError> {
        for slot in &self.instances {
            // Recover a poisoned mutex (see declared_tools rationale) rather
            // than propagating a panic to the MCP request handler.
            let mut instance = slot.lock().unwrap_or_else(|p| p.into_inner());
            if instance.manifest().name == plugin_id_str {
                return instance.call_tool(tool_name, args);
            }
        }
        Err(PluginHostError::ToolNotFound(plugin_id_str.to_owned()))
    }

    // ── Private fan-out helper ────────────────────────────────────────────────

    /// Fan out a hook to all instances that subscribed to `kind`.
    ///
    /// `make_payload` is called once per subscribed instance so each instance
    /// receives an independent clone of the payload (no shared mutable state).
    ///
    /// Errors from individual instances are logged to stderr and do not
    /// propagate (UN1). The fan-out continues to the next instance regardless.
    ///
    /// # Latency note
    ///
    /// This method holds the per-instance `Mutex` for the entire duration of
    /// `deliver_hook`. With `IsolatedSandbox` wrapping each instance, a hook
    /// handler that runs until fuel exhaustion will hold the lock for up to
    /// ~`DEFAULT_FUEL_BUDGET` wasm instructions (roughly ≤100 ms at 1 BIPS).
    /// During that window any concurrent `call_tool` on the same instance blocks
    /// on the same lock. This is bounded by fuel and acceptable for the current
    /// synchronous fan-out design.
    fn fan_out(&self, kind: &HookKind, make_payload: impl Fn() -> HookPayload) {
        for slot in &self.instances {
            // Recover poisoned mutexes (see declared_tools rationale).
            // A wasm trap from 11c can poison the lock; UN1 requires fan-out
            // to continue to remaining instances regardless.
            let mut instance = slot.lock().unwrap_or_else(|p| p.into_inner());
            if instance.manifest().hooks.contains(kind) {
                let payload = make_payload();
                if let Err(e) = instance.deliver_hook(kind.clone(), payload) {
                    eprintln!(
                        "[tower] plugin '{}' hook delivery error ({kind:?}): {e}",
                        instance.manifest().name
                    );
                }
            }
        }
    }
}

impl Default for PluginHostRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginHostPort for PluginHostRegistry {
    /// Called after a file has been indexed (EV2).
    ///
    /// Fans out [`HookKind::FileIndexed`] to all subscribed instances.
    /// Unsubscribed instances and errors are handled per EV2 / UN1 respectively.
    fn on_file_indexed(&self, _id: FileId, path: &RelativePath) {
        let path_str = path.as_str().to_owned();
        self.fan_out(&HookKind::FileIndexed, || HookPayload::FileIndexed {
            path: path_str.clone(),
        });
    }

    /// Called after a file's content has changed (EV2).
    ///
    /// Fans out [`HookKind::FileChanged`] to all subscribed instances.
    /// Unsubscribed instances and errors are handled per EV2 / UN1 respectively.
    fn on_file_changed(&self, _id: FileId, path: &RelativePath) {
        let path_str = path.as_str().to_owned();
        self.fan_out(&HookKind::FileChanged, || HookPayload::FileChanged {
            path: path_str.clone(),
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use plugin_sdk::{ABI_VERSION, HookKind, HookPayload, PluginManifest, ToolDesc, Value};

    use super::{PluginHostError, PluginHostRegistry, PluginId, PluginInstance, RegistrationError};
    use crate::domain::{FileId, RelativePath};
    use crate::ports::PluginHostPort;

    // ── Fake PluginInstance helpers ───────────────────────────────────────────

    /// A fake `PluginInstance` that records every `deliver_hook` call it receives.
    struct RecordingPlugin {
        manifest: PluginManifest,
        /// All (kind, payload) pairs delivered to this instance.
        delivered: Arc<Mutex<Vec<(HookKind, HookPayload)>>>,
    }

    impl RecordingPlugin {
        fn new(name: &str, hooks: Vec<HookKind>, tools: Vec<ToolDesc>) -> Self {
            Self {
                manifest: PluginManifest {
                    name: name.to_owned(),
                    version: "0.1.0".to_owned(),
                    abi: ABI_VERSION,
                    tools,
                    hooks,
                },
                delivered: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl PluginInstance for RecordingPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value, PluginHostError> {
            Err(PluginHostError::ToolNotFound(name.to_owned()))
        }

        fn deliver_hook(
            &mut self,
            kind: HookKind,
            payload: HookPayload,
        ) -> Result<(), PluginHostError> {
            self.delivered.lock().unwrap().push((kind, payload));
            Ok(())
        }
    }

    /// A fake `PluginInstance` whose `deliver_hook` always returns an error.
    /// Used to test per-plugin error isolation (UN1 / AC4).
    struct FailingPlugin {
        manifest: PluginManifest,
    }

    impl FailingPlugin {
        fn new(name: &str, hooks: Vec<HookKind>) -> Self {
            Self {
                manifest: PluginManifest {
                    name: name.to_owned(),
                    version: "0.1.0".to_owned(),
                    abi: ABI_VERSION,
                    tools: vec![],
                    hooks,
                },
            }
        }
    }

    impl PluginInstance for FailingPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value, PluginHostError> {
            Err(PluginHostError::ToolNotFound(name.to_owned()))
        }

        fn deliver_hook(
            &mut self,
            _kind: HookKind,
            _payload: HookPayload,
        ) -> Result<(), PluginHostError> {
            Err(PluginHostError::HookDeliveryFailed(
                "injected failure".to_owned(),
            ))
        }
    }

    fn file_path(s: &str) -> RelativePath {
        RelativePath::new(s)
    }

    fn file_id() -> FileId {
        FileId::new_for_testing(0, 0)
    }

    // ── TDD step 1 RED→GREEN: subscribed vs unsubscribed (AC1) ───────────────

    /// AC1: A plugin subscribed to FileChanged receives the hook; one that is
    /// not subscribed does not.
    #[test]
    fn subscribed_plugin_receives_hook_unsubscribed_does_not() {
        let subscribed = RecordingPlugin::new("subscribed", vec![HookKind::FileChanged], vec![]);
        let subscribed_delivered = Arc::clone(&subscribed.delivered);

        let not_subscribed =
            RecordingPlugin::new("not_subscribed", vec![HookKind::FileIndexed], vec![]);
        let not_subscribed_delivered = Arc::clone(&not_subscribed.delivered);

        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(subscribed))
            .expect("registration must succeed");
        registry
            .register(Box::new(not_subscribed))
            .expect("registration must succeed");

        registry.on_file_changed(file_id(), &file_path("src/lib.rs"));

        let sub_calls = subscribed_delivered.lock().unwrap();
        assert_eq!(
            sub_calls.len(),
            1,
            "subscribed plugin must receive exactly 1 call"
        );
        assert_eq!(
            sub_calls[0].0,
            HookKind::FileChanged,
            "must receive FileChanged hook"
        );

        let unsub_calls = not_subscribed_delivered.lock().unwrap();
        assert_eq!(
            unsub_calls.len(),
            0,
            "unsubscribed plugin must receive 0 calls"
        );
    }

    /// FileIndexed variant of AC1: subscribed plugin receives FileIndexed.
    #[test]
    fn subscribed_plugin_receives_file_indexed_hook() {
        let plugin = RecordingPlugin::new("indexer", vec![HookKind::FileIndexed], vec![]);
        let delivered = Arc::clone(&plugin.delivered);

        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(plugin))
            .expect("registration must succeed");

        registry.on_file_indexed(file_id(), &file_path("src/main.rs"));

        let calls = delivered.lock().unwrap();
        assert_eq!(calls.len(), 1, "must receive exactly 1 FileIndexed call");
        match &calls[0].1 {
            HookPayload::FileIndexed { path } => {
                assert_eq!(path, "src/main.rs", "path in payload must match");
            }
            other => panic!("expected FileIndexed payload, got {other:?}"),
        }
    }

    // ── TDD step 3 RED→GREEN: declared_tools aggregation (AC2 / U1) ──────────

    /// AC2: Two plugins each declaring tools — declared_tools returns both sets
    /// tagged by plugin identity.
    #[test]
    fn declared_tools_aggregates_all_plugin_tools_with_ids() {
        let tool_a = ToolDesc {
            name: "greet".to_owned(),
            description: "say hello".to_owned(),
            schema_json: "{}".to_owned(),
        };
        let tool_b = ToolDesc {
            name: "search".to_owned(),
            description: "search files".to_owned(),
            schema_json: "{}".to_owned(),
        };
        let plugin_a = RecordingPlugin::new("plugin_a", vec![], vec![tool_a.clone()]);
        let plugin_b = RecordingPlugin::new("plugin_b", vec![], vec![tool_b.clone()]);

        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(plugin_a))
            .expect("registration must succeed");
        registry
            .register(Box::new(plugin_b))
            .expect("registration must succeed");

        let tools = registry.declared_tools();
        assert_eq!(tools.len(), 2, "must aggregate 2 tools total");

        let (id_a, desc_a) = &tools[0];
        assert_eq!(id_a, &PluginId::new("plugin_a"));
        assert_eq!(desc_a.name, "greet");

        let (id_b, desc_b) = &tools[1];
        assert_eq!(id_b, &PluginId::new("plugin_b"));
        assert_eq!(desc_b.name, "search");
    }

    /// A plugin with multiple tools contributes all of them.
    #[test]
    fn plugin_with_multiple_tools_all_appear_in_declared_tools() {
        let tools = vec![
            ToolDesc {
                name: "tool1".to_owned(),
                description: "".to_owned(),
                schema_json: "{}".to_owned(),
            },
            ToolDesc {
                name: "tool2".to_owned(),
                description: "".to_owned(),
                schema_json: "{}".to_owned(),
            },
        ];
        let plugin = RecordingPlugin::new("multi", vec![], tools);
        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(plugin))
            .expect("registration must succeed");

        let declared = registry.declared_tools();
        assert_eq!(declared.len(), 2);
        assert!(declared.iter().all(|(id, _)| id.as_str() == "multi"));
    }

    // ── TDD step 5 RED→GREEN: no-op + per-plugin error isolation ─────────────

    /// AC3: No plugins registered — hooks fire without panicking or erroring.
    #[test]
    fn empty_registry_hooks_are_noop() {
        let registry = PluginHostRegistry::new();
        // Must not panic.
        registry.on_file_indexed(file_id(), &file_path("a.rs"));
        registry.on_file_changed(file_id(), &file_path("b.rs"));
        assert!(registry.declared_tools().is_empty());
    }

    /// OP1: Empty registry declared_tools returns empty vec.
    #[test]
    fn empty_registry_declared_tools_is_empty() {
        assert!(PluginHostRegistry::new().declared_tools().is_empty());
    }

    /// AC4: One plugin whose deliver_hook errors — the other still receives
    /// the hook (per-plugin error isolation, UN1).
    #[test]
    fn failing_plugin_does_not_block_other_plugins() {
        let failing = FailingPlugin::new("bad_plugin", vec![HookKind::FileChanged]);
        let good = RecordingPlugin::new("good_plugin", vec![HookKind::FileChanged], vec![]);
        let good_delivered = Arc::clone(&good.delivered);

        let mut registry = PluginHostRegistry::new();
        // Register failing plugin first so it runs before the good one.
        registry
            .register(Box::new(failing))
            .expect("registration must succeed");
        registry
            .register(Box::new(good))
            .expect("registration must succeed");

        // Must not propagate the failing plugin's error.
        registry.on_file_changed(file_id(), &file_path("src/lib.rs"));

        let calls = good_delivered.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "good plugin must still receive the hook despite the failing plugin: got {} calls",
            calls.len()
        );
    }

    /// Symmetrical: failing plugin in second position does not prevent first
    /// plugin from receiving the hook.
    #[test]
    fn failing_plugin_in_second_position_does_not_block_first() {
        let good = RecordingPlugin::new("good_plugin", vec![HookKind::FileIndexed], vec![]);
        let good_delivered = Arc::clone(&good.delivered);
        let failing = FailingPlugin::new("bad_plugin", vec![HookKind::FileIndexed]);

        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(good))
            .expect("registration must succeed");
        registry
            .register(Box::new(failing))
            .expect("registration must succeed");

        registry.on_file_indexed(file_id(), &file_path("changed.rs"));

        let calls = good_delivered.lock().unwrap();
        assert_eq!(calls.len(), 1, "first plugin must have received the hook");
    }

    // ── Additional coverage ───────────────────────────────────────────────────

    /// A plugin subscribed to both FileIndexed and FileChanged receives both.
    #[test]
    fn plugin_subscribed_to_both_hooks_receives_both() {
        let plugin = RecordingPlugin::new(
            "dual",
            vec![HookKind::FileIndexed, HookKind::FileChanged],
            vec![],
        );
        let delivered = Arc::clone(&plugin.delivered);

        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(plugin))
            .expect("registration must succeed");

        registry.on_file_indexed(file_id(), &file_path("a.rs"));
        registry.on_file_changed(file_id(), &file_path("b.rs"));

        let calls = delivered.lock().unwrap();
        assert_eq!(calls.len(), 2, "must receive 2 hook deliveries");
        assert_eq!(calls[0].0, HookKind::FileIndexed);
        assert_eq!(calls[1].0, HookKind::FileChanged);
    }

    /// Payload carries the correct path string.
    #[test]
    fn file_changed_payload_carries_correct_path() {
        let plugin = RecordingPlugin::new("pathtest", vec![HookKind::FileChanged], vec![]);
        let delivered = Arc::clone(&plugin.delivered);

        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(plugin))
            .expect("registration must succeed");

        registry.on_file_changed(file_id(), &file_path("src/domain/mod.rs"));

        let calls = delivered.lock().unwrap();
        match &calls[0].1 {
            HookPayload::FileChanged { path } => {
                assert_eq!(path, "src/domain/mod.rs");
            }
            other => panic!("expected FileChanged payload, got {other:?}"),
        }
    }

    /// PluginId equality and display.
    #[test]
    fn plugin_id_equality_and_display() {
        let id1 = PluginId::new("hello");
        let id2 = PluginId::new("hello");
        let id3 = PluginId::new("world");
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
        assert_eq!(id1.to_string(), "hello");
        assert_eq!(id1.as_str(), "hello");
    }

    // ── ABI mismatch guard (Finding 7) ────────────────────────────────────────

    /// A plugin built against a stale ABI version is rejected at registration
    /// time with `RegistrationError::AbiMismatch`. This prevents silent data
    /// corruption from mismatched postcard DTO layouts.
    ///
    /// TDD: RED first (register returned () before this change), now GREEN.
    #[test]
    fn register_rejects_stale_abi_plugin() {
        struct StaleAbiPlugin {
            manifest: PluginManifest,
        }

        impl PluginInstance for StaleAbiPlugin {
            fn manifest(&self) -> &PluginManifest {
                &self.manifest
            }

            fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value, PluginHostError> {
                Err(PluginHostError::ToolNotFound(name.to_owned()))
            }

            fn deliver_hook(
                &mut self,
                _kind: HookKind,
                _payload: HookPayload,
            ) -> Result<(), PluginHostError> {
                Ok(())
            }
        }

        let stale_plugin = StaleAbiPlugin {
            manifest: PluginManifest {
                name: "old_plugin".to_owned(),
                version: "1.0.0".to_owned(),
                abi: ABI_VERSION.saturating_sub(1),
                tools: vec![],
                hooks: vec![],
            },
        };

        let mut registry = PluginHostRegistry::new();
        let result = registry.register(Box::new(stale_plugin));

        assert_eq!(
            result,
            Err(RegistrationError::AbiMismatch {
                expected: ABI_VERSION,
                got: ABI_VERSION.saturating_sub(1),
            }),
            "stale ABI plugin must be rejected at registration"
        );
        assert!(
            registry.declared_tools().is_empty(),
            "rejected plugin must not appear in declared_tools"
        );
    }

    /// A plugin with the correct ABI version is accepted.
    #[test]
    fn register_accepts_current_abi_plugin() {
        let plugin = RecordingPlugin::new("current", vec![], vec![]);
        let mut registry = PluginHostRegistry::new();
        assert!(
            registry.register(Box::new(plugin)).is_ok(),
            "current ABI plugin must be accepted"
        );
    }

    // ── Duplicate name guard (Finding 1) ──────────────────────────────────────

    /// Registering two plugins with the same manifest name must be rejected with
    /// `RegistrationError::DuplicateName`. The second plugin must not appear in
    /// `declared_tools` and must not be reachable via `call_instance_tool`.
    ///
    /// TDD: RED first (register returned Ok before this change), now GREEN.
    #[test]
    fn register_rejects_duplicate_plugin_name() {
        let mut registry = PluginHostRegistry::new();
        let first = RecordingPlugin::new(
            "analytics",
            vec![],
            vec![ToolDesc {
                name: "report".to_owned(),
                description: "".to_owned(),
                schema_json: "{}".to_owned(),
            }],
        );
        let second = RecordingPlugin::new(
            "analytics",
            vec![],
            vec![ToolDesc {
                name: "summary".to_owned(),
                description: "".to_owned(),
                schema_json: "{}".to_owned(),
            }],
        );

        registry
            .register(Box::new(first))
            .expect("first registration must succeed");
        let result = registry.register(Box::new(second));

        assert_eq!(
            result,
            Err(RegistrationError::DuplicateName("analytics".to_owned())),
            "duplicate name must be rejected"
        );

        // Only the first plugin's tool is visible; the second was never stored.
        let tools = registry.declared_tools();
        assert_eq!(tools.len(), 1, "only first plugin's tools present");
        assert_eq!(tools[0].1.name, "report", "first plugin tool visible");

        // call_instance_tool routes to the first plugin (it has 'report', not 'summary').
        let err = registry
            .call_instance_tool("analytics", "summary", Value::Null)
            .expect_err("second plugin's tool must not be reachable");
        assert!(
            matches!(err, PluginHostError::ToolNotFound(_)),
            "unreachable tool must return ToolNotFound: {err:?}"
        );
    }

    // ── Reserved name prefix guard ────────────────────────────────────────────

    /// Registering a plugin whose manifest name begins with `"tower"` must be
    /// rejected with `RegistrationError::ReservedNamePrefix`.
    ///
    /// The `"tower"` prefix is reserved for the host namespace: plugin tools are
    /// composed as `"tower_<plugin_id>_<tool_name>"` in the MCP merged registry.
    /// A plugin beginning with `"tower"` could produce composed names that
    /// impersonate host-native tools — a namespace collision attack vector.
    ///
    /// TDD: mirrors `register_rejects_duplicate_plugin_name` pattern.
    #[test]
    fn register_rejects_tower_prefixed_plugin_name() {
        let mut registry = PluginHostRegistry::new();

        // Exact "tower" prefix.
        let tower_exact = RecordingPlugin::new("tower", vec![], vec![]);
        let result = registry.register(Box::new(tower_exact));
        assert_eq!(
            result,
            Err(RegistrationError::ReservedNamePrefix("tower".to_owned())),
            "plugin named 'tower' must be rejected"
        );
        assert!(
            registry.declared_tools().is_empty(),
            "rejected plugin must not appear in declared_tools"
        );

        // Name starting with "tower_".
        let tower_prefixed = RecordingPlugin::new("tower_my_plugin", vec![], vec![]);
        let result2 = registry.register(Box::new(tower_prefixed));
        assert_eq!(
            result2,
            Err(RegistrationError::ReservedNamePrefix(
                "tower_my_plugin".to_owned()
            )),
            "plugin named 'tower_my_plugin' must be rejected"
        );
        assert!(
            registry.declared_tools().is_empty(),
            "no rejected plugin must leak into declared_tools"
        );

        // A plugin not beginning with "tower" must still be accepted.
        let valid = RecordingPlugin::new("ast", vec![], vec![]);
        assert!(
            registry.register(Box::new(valid)).is_ok(),
            "non-tower plugin must be accepted after rejections"
        );
    }

    /// Two differently-named plugins that both declare a tool named 'greet'
    /// must each be independently reachable via `call_instance_tool`.
    ///
    /// This is the routing correctness test that was missing from the spec suite:
    /// the previous `ac3_two_plugins_with_same_tool_name_both_appear_namespaced`
    /// only asserted `list()` output, not actual call routing.
    #[test]
    fn two_plugins_same_tool_name_each_route_to_correct_instance() {
        /// A fake whose call_tool echoes the plugin's own name as the result.
        struct IdentifyingPlugin {
            manifest: PluginManifest,
        }

        impl PluginInstance for IdentifyingPlugin {
            fn manifest(&self) -> &PluginManifest {
                &self.manifest
            }

            fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value, PluginHostError> {
                if self.manifest.tools.iter().any(|t| t.name == name) {
                    Ok(Value::Text(self.manifest.name.clone()))
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

        let greet_tool = ToolDesc {
            name: "greet".to_owned(),
            description: "".to_owned(),
            schema_json: "{}".to_owned(),
        };

        let plugin_a = IdentifyingPlugin {
            manifest: PluginManifest {
                name: "plugin_a".to_owned(),
                version: "0.1.0".to_owned(),
                abi: ABI_VERSION,
                tools: vec![greet_tool.clone()],
                hooks: vec![],
            },
        };
        let plugin_b = IdentifyingPlugin {
            manifest: PluginManifest {
                name: "plugin_b".to_owned(),
                version: "0.1.0".to_owned(),
                abi: ABI_VERSION,
                tools: vec![greet_tool],
                hooks: vec![],
            },
        };

        let mut registry = PluginHostRegistry::new();
        registry
            .register(Box::new(plugin_a))
            .expect("plugin_a register");
        registry
            .register(Box::new(plugin_b))
            .expect("plugin_b register");

        // Call plugin_a/greet → must return "plugin_a", not "plugin_b".
        let result_a = registry
            .call_instance_tool("plugin_a", "greet", Value::Null)
            .expect("plugin_a/greet must succeed");
        assert_eq!(
            result_a,
            Value::Text("plugin_a".to_owned()),
            "plugin_a/greet must route to plugin_a"
        );

        // Call plugin_b/greet → must return "plugin_b", not "plugin_a".
        let result_b = registry
            .call_instance_tool("plugin_b", "greet", Value::Null)
            .expect("plugin_b/greet must succeed");
        assert_eq!(
            result_b,
            Value::Text("plugin_b".to_owned()),
            "plugin_b/greet must route to plugin_b"
        );
    }
}
