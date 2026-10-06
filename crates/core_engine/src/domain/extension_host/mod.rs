//! Extension host registry — runtime-agnostic, domain-pure (spec 22).
//!
//! # Wireframe
//!
//! ```text
//! trait ExtensionInstance {
//!     manifest()                                     -> &ExtensionManifest
//!     call_tool(name, params) -> Result<Value, ExtensionCallError>
//!     deliver_event(Event)    -> Result<(), ExtensionFault>
//!     shutdown()
//! }
//! ExtensionRegistry : ExtensionHostPort
//!   register(Box<dyn ExtensionInstance>)
//!   on_file_indexed(FileId)  ─┐ fan-out only to instances subscribed to that event kind
//!   on_file_changed(FileId)  ─┘  (per-extension error isolation: one bad ext blocks no others)
//!   invoke(tool, params): route by tool name → owning instance; map fault → caller error
//!   declared_tools() -> Vec<(ExtensionId, ToolDecl)>   (consumed by MCP merge, spec 28)
//!   quarantine: per-ext consecutive-fault counter; ≥ MAX_CONSECUTIVE_FAILURES (=3) ⇒ Quarantined,
//!               stop routing/delivery, return Quarantined to callers   (policy only)
//! No-op when no extension registered (spec 02 OP1 parity).
//! ```
//!
//! # Hexagonal boundary
//!
//! This module imports only `extension_protocol` (pure types, no I/O) and
//! `crate::domain` / `crate::ports`. It deliberately imports no `std::process`,
//! `wasmtime`, `sled`, `std::fs`, or `notify`. The sidecar adapter (spec 23)
//! implements `ExtensionInstance` and is injected via `register`.
//!
//! # Quarantine policy (S1 / AC5)
//!
//! The registry tracks consecutive faults per extension. After
//! [`MAX_CONSECUTIVE_FAILURES`] (= 3) consecutive faults the extension is marked
//! `Quarantined`. Subsequent `invoke` calls and event deliveries return
//! [`ExtensionFault::Quarantined`] immediately without contacting the instance.
//! A successful `call_tool` or `deliver_event` resets the counter to zero.
//!
//! # Interior mutability
//!
//! `ExtensionHostPort` requires `&self` (object-safe) but `ExtensionInstance`
//! methods take `&mut self` (exclusive access for subprocess I/O). The registry
//! wraps each instance in `Mutex<Box<dyn ExtensionInstance>>` so `&self` fan-out
//! can obtain exclusive access per-instance without contending across extensions.
//!
//! Decision: `Mutex` per instance over a single `RwLock` over all instances.
//! Why: the extension host is not a hot path (event fan-out, not per-request).
//!      Per-instance locks mean a slow or blocked extension only stalls itself.
//! Trade-off: `Mutex` adds one word of overhead per instance; negligible.
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, TryLockError};

use extension_protocol::{ExtensionFault, ExtensionManifest, ToolDecl};
use serde_json::Value;

use crate::domain::FileId;
use crate::ports::ExtensionHostPort;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum consecutive faults before an extension is quarantined (S1).
///
/// After this many consecutive faults from `call_tool` or `deliver_event`, the
/// registry marks the extension quarantined and returns
/// [`ExtensionFault::Quarantined`] for all subsequent calls without invoking the
/// instance. A single successful call before reaching this threshold resets the
/// counter.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

// ── ExtensionId ───────────────────────────────────────────────────────────────

/// A stable identifier for a registered extension, derived from its manifest name.
///
/// `ExtensionId` is the namespace key used in [`ExtensionRegistry::declared_tools`]
/// to associate each tool with the extension that declared it (spec U1 / 28 MCP merge).
///
/// # Examples
///
/// ```rust
/// use core_engine::domain::extension_host::ExtensionId;
///
/// let id = ExtensionId::new("ast");
/// assert_eq!(id.as_str(), "ast");
/// assert_eq!(id.to_string(), "ast");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExtensionId(String);

impl ExtensionId {
    /// Create a new `ExtensionId` from the given name.
    ///
    /// The name should match [`ExtensionManifest::name`] for consistency.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Return the underlying name string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── RegistrationError ─────────────────────────────────────────────────────────

/// Errors that prevent an extension from being registered.
///
/// # Examples
///
/// ```rust
/// use core_engine::domain::extension_host::{ExtensionRegistry, RegistrationError};
///
/// let registry = ExtensionRegistry::new();
/// assert!(registry.declared_tools().is_empty());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum RegistrationError {
    /// A extension with this manifest name is already registered.
    DuplicateName(String),
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateName(name) => {
                write!(f, "an extension named '{name}' is already registered")
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionApplicationError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl std::fmt::Display for ExtensionApplicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExtensionApplicationError {}

#[derive(Clone, Debug, PartialEq)]
pub enum ExtensionCallError {
    Application(ExtensionApplicationError),
    Fault(ExtensionFault),
}

impl std::fmt::Display for ExtensionCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Application(error) => error.fmt(f),
            Self::Fault(fault) => fault.fmt(f),
        }
    }
}

impl std::error::Error for ExtensionCallError {}

impl From<ExtensionFault> for ExtensionCallError {
    fn from(fault: ExtensionFault) -> Self {
        Self::Fault(fault)
    }
}

// ── InvokeError ───────────────────────────────────────────────────────────────

/// Errors returned by [`ExtensionRegistry::invoke`].
#[derive(Clone, Debug, PartialEq)]
pub enum InvokeError {
    /// No registered extension owns the named tool.
    ToolNotFound(String),
    Application(ExtensionApplicationError),
    /// The owning extension returned a fault.
    Fault(ExtensionFault),
}

impl std::fmt::Display for InvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolNotFound(name) => write!(f, "no extension owns tool: {name}"),
            Self::Application(error) => error.fmt(f),
            Self::Fault(fault) => write!(f, "extension fault: {fault}"),
        }
    }
}

impl std::error::Error for InvokeError {}

// ── ExtensionInstance ─────────────────────────────────────────────────────────

/// The host-side abstraction over a single running extension (spec 22 U3).
///
/// The sidecar adapter (spec 23) implements this trait to wrap a live child
/// process communicating via JSON-RPC 2.0 over stdio. Tests use fake
/// implementations with no process machinery (DoD).
///
/// # Object safety
///
/// The trait is intentionally object-safe so each instance can be boxed and
/// dispatched through dynamic dispatch.
///
/// # Mutability contract
///
/// `call_tool`, `deliver_event`, and `shutdown` take `&mut self` because
/// subprocess I/O requires exclusive access. The registry wraps each instance in
/// a `Mutex` to satisfy `&self` callers while still providing exclusive access.
pub trait ExtensionInstance: Send {
    /// Return the manifest this extension declared at initialization time.
    ///
    /// The manifest is immutable for the extension's lifetime: tools and event
    /// subscriptions do not change after registration.
    fn manifest(&self) -> &ExtensionManifest;

    /// Invoke a named tool with the provided parameters.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionCallError`] when the extension reports an
    /// application error or encounters a runtime/protocol fault.
    fn call_tool(&mut self, name: &str, params: Value) -> Result<Value, ExtensionCallError>;

    /// Deliver a workspace event to this extension instance.
    ///
    /// The registry only calls this for events the extension subscribed to in
    /// its manifest (EV2). Unsubscribed events are never delivered.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionFault`] on any runtime or protocol error.
    fn deliver_event(&mut self, event: extension_protocol::Event) -> Result<(), ExtensionFault>;

    /// Gracefully shut down this extension instance.
    ///
    /// Called by the registry on drop or explicit shutdown. Implementations
    /// should send the `shutdown` request and wait for the process to exit.
    fn shutdown(&mut self);
}

// ── Private handle ────────────────────────────────────────────────────────────

/// Per-extension slot: cached manifest + locked instance + quarantine state.
struct ExtensionHandle {
    /// Cached (immutable for the extension's lifetime) manifest.
    manifest: ExtensionManifest,
    /// Live instance, exclusively accessed via Mutex.
    instance: Arc<Mutex<Box<dyn ExtensionInstance>>>,
    /// Consecutive-fault counter for quarantine policy (S1).
    ///
    /// Kept outside the instance mutex so quarantine-only reads don't block on
    /// an in-flight sidecar RPC. Any path that invokes the instance acquires
    /// the instance lock first, then checks and updates this counter before
    /// releasing that lock. This makes each invocation and its quarantine
    /// transition one serialized state change.
    ///
    /// Decision: `Mutex<u32>` separate from `Mutex<Box<dyn ExtensionInstance>>`.
    /// Why: the instance lock can be held for the full duration of an RPC call
    ///      (ms–s), while quarantine-only reads should remain cheap.
    consecutive_faults: Arc<Mutex<u32>>,
}

impl ExtensionHandle {
    fn new(instance: Box<dyn ExtensionInstance>) -> Self {
        let manifest = instance.manifest().clone();
        Self {
            manifest,
            instance: Arc::new(Mutex::new(instance)),
            consecutive_faults: Arc::new(Mutex::new(0)),
        }
    }

    /// Check whether this extension is currently quarantined.
    fn is_quarantined(&self) -> bool {
        *self
            .consecutive_faults
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            >= MAX_CONSECUTIVE_FAILURES
    }

    /// Record a successful call: reset the consecutive-fault counter.
    fn record_success(&self) {
        *self
            .consecutive_faults
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = 0;
    }

    /// Record a fault: increment the counter. Returns the new counter value.
    fn record_fault(&self) -> u32 {
        let mut guard = self
            .consecutive_faults
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *guard += 1;
        *guard
    }
}

// ── ExtensionRegistry ─────────────────────────────────────────────────────────

/// Registry implementing [`ExtensionHostPort`]: stores extension instances,
/// fans out workspace events, routes tool calls, and enforces quarantine policy.
///
/// # Registration
///
/// Call [`register`][Self::register] once per extension at startup. The registry
/// takes ownership of the boxed instance. Tools and event subscriptions are
/// read from the manifest on every relevant operation.
///
/// # Subscription filtering (EV2)
///
/// On `on_file_indexed` / `on_file_changed`, only instances whose manifest
/// subscribes to the corresponding event kind receive delivery. Unsubscribed
/// instances incur zero overhead.
///
/// # Error isolation (UN1)
///
/// If `deliver_event` returns a fault for one instance, the registry records it,
/// increments that extension's fault counter, and continues delivering to the
/// remaining instances. One bad extension cannot block others.
///
/// # Quarantine (S1 / AC5)
///
/// After [`MAX_CONSECUTIVE_FAILURES`] consecutive faults (from `call_tool` or
/// `deliver_event`), the extension is quarantined: all subsequent tool calls and
/// event deliveries return [`ExtensionFault::Quarantined`] without contacting
/// the instance. A successful call before the limit resets the counter.
///
/// # No-op when empty (OP1)
///
/// An empty registry is a no-op: events fire but nothing happens;
/// `declared_tools` returns an empty `Vec`.
///
/// # Examples
///
/// ```rust
/// use core_engine::domain::extension_host::ExtensionRegistry;
///
/// let registry = ExtensionRegistry::new();
/// // No extensions registered — declared_tools is empty (OP1 / AC6).
/// assert!(registry.declared_tools().is_empty());
/// ```
pub struct ExtensionRegistry {
    handles: Vec<ExtensionHandle>,
    deferred_events: Mutex<Vec<DeferredEvent>>,
}

struct DeferredEvent {
    extension_name: String,
    event: extension_protocol::Event,
}

impl std::fmt::Debug for ExtensionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRegistry")
            .field("extension_count", &self.handles.len())
            .finish()
    }
}

impl ExtensionRegistry {
    /// Create an empty registry (no extensions registered).
    ///
    /// Lifecycle events are no-ops until at least one extension is registered (OP1).
    #[must_use]
    pub fn new() -> Self {
        Self {
            handles: Vec::new(),
            deferred_events: Mutex::new(Vec::new()),
        }
    }

    /// Register an extension instance with the registry (EV1).
    ///
    /// The registry takes ownership of the boxed instance. The manifest is
    /// cached on registration for cheap reads.
    ///
    /// # Errors
    ///
    /// Returns [`RegistrationError::DuplicateName`] when an extension with the
    /// same manifest name is already registered.
    ///
    /// # Examples
    ///
    /// See [`ExtensionRegistry`] doc examples for usage.
    pub fn register(
        &mut self,
        instance: Box<dyn ExtensionInstance>,
    ) -> Result<(), RegistrationError> {
        let new_name = instance.manifest().name.clone();

        if self.handles.iter().any(|h| h.manifest.name == new_name) {
            return Err(RegistrationError::DuplicateName(new_name));
        }

        self.handles.push(ExtensionHandle::new(instance));
        Ok(())
    }

    /// Return all tools declared by registered extensions, each tagged by
    /// extension identity (U1).
    ///
    /// Each tool name is NOT yet prefixed here — the MCP merge layer (spec 28)
    /// applies the `tower_<ext>_<tool>` namespace at merge time. The [`ExtensionId`]
    /// provides the extension name for that prefix.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use core_engine::domain::extension_host::ExtensionRegistry;
    ///
    /// let registry = ExtensionRegistry::new();
    /// assert!(registry.declared_tools().is_empty());
    /// ```
    pub fn declared_tools(&self) -> Vec<(ExtensionId, ToolDecl)> {
        self.handles
            .iter()
            .flat_map(|h| {
                let id = ExtensionId::new(h.manifest.name.clone());
                h.manifest
                    .tools
                    .iter()
                    .map(move |tool| (id.clone(), tool.clone()))
            })
            .collect()
    }

    /// Invoke a named tool, routing to the owning extension instance (U2 / AC3).
    ///
    /// The `tool_name` is the bare tool name as declared in the extension's
    /// manifest (without the `tower_<ext>_` prefix). The caller (MCP merge layer,
    /// spec 28) strips the prefix before calling here.
    ///
    /// # Routing
    ///
    /// The first registered extension whose manifest declares a tool with this
    /// name is selected as the owner. If no extension owns the tool,
    /// [`InvokeError::ToolNotFound`] is returned.
    ///
    /// # Quarantine
    ///
    /// If the owning extension is quarantined, [`InvokeError::Fault`] wrapping
    /// [`ExtensionFault::Quarantined`] is returned immediately without invoking
    /// the instance.
    ///
    /// # Errors
    ///
    /// - [`InvokeError::ToolNotFound`] — no extension owns `tool_name`.
    /// - [`InvokeError::Application`] — the extension rejected the tool call.
    /// - [`InvokeError::Fault`] — the owning extension returned a fault (or is
    ///   quarantined).
    pub fn invoke(&self, tool_name: &str, params: Value) -> Result<Value, InvokeError> {
        // Find the handle that owns this tool.
        let handle = self
            .handles
            .iter()
            .find(|h| h.manifest.tools.iter().any(|t| t.name == tool_name))
            .ok_or_else(|| InvokeError::ToolNotFound(tool_name.to_owned()))?;

        let result = Self::invoke_handle(handle, tool_name, params);
        self.drain_deferred_events();
        result
    }

    /// Invoke a tool declared by a specific extension.
    ///
    /// The MCP merge layer has already matched the fully namespaced public tool
    /// name, so it must preserve that extension identity when local tool names
    /// collide across extensions.
    pub fn invoke_extension(
        &self,
        extension_id: &ExtensionId,
        tool_name: &str,
        params: Value,
    ) -> Result<Value, InvokeError> {
        let handle = self
            .handles
            .iter()
            .find(|h| {
                h.manifest.name == extension_id.as_str()
                    && h.manifest.tools.iter().any(|t| t.name == tool_name)
            })
            .ok_or_else(|| InvokeError::ToolNotFound(tool_name.to_owned()))?;

        let result = Self::invoke_handle(handle, tool_name, params);
        self.drain_deferred_events();
        result
    }

    fn invoke_handle(
        handle: &ExtensionHandle,
        tool_name: &str,
        params: Value,
    ) -> Result<Value, InvokeError> {
        let mut instance = handle.instance.lock().unwrap_or_else(|p| p.into_inner());
        if handle.is_quarantined() {
            return Err(InvokeError::Fault(ExtensionFault::Quarantined));
        }

        let result = instance.call_tool(tool_name, params);

        match result {
            Ok(value) => {
                handle.record_success();
                Ok(value)
            }
            Err(ExtensionCallError::Application(error)) => Err(InvokeError::Application(error)),
            Err(ExtensionCallError::Fault(fault)) => {
                handle.record_fault();
                Err(InvokeError::Fault(fault))
            }
        }
    }

    // ── Private fan-out helper ─────────────────────────────────────────────

    /// Fan out an event to all instances subscribed to `event_method`.
    ///
    /// `event_method` is the string subscription key from the manifest (e.g.
    /// `"event/fileIndexed"`). Only instances whose manifest `events.subscribe`
    /// contains this key receive the delivery.
    ///
    /// Faults from individual instances are isolated (UN1): the counter is
    /// incremented, a warning is printed to stderr, and delivery continues to
    /// remaining instances.
    fn fan_out(&self, event_method: &str, make_event: impl Fn() -> extension_protocol::Event) {
        for handle in &self.handles {
            if !handle
                .manifest
                .events
                .subscribe
                .contains(&event_method.to_owned())
            {
                continue;
            }

            if handle.is_quarantined() {
                // Quarantined extensions receive no deliveries (S1).
                continue;
            }

            let event = make_event();
            let mut instance = match handle.instance.try_lock() {
                Ok(instance) => instance,
                Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    self.deferred_events
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(DeferredEvent {
                            extension_name: handle.manifest.name.clone(),
                            event,
                        });
                    continue;
                }
            };
            if handle.is_quarantined() {
                continue;
            }
            let result = instance.deliver_event(event);

            match result {
                Ok(()) => {
                    handle.record_success();
                }
                Err(fault) => {
                    let count = handle.record_fault();
                    eprintln!(
                        "[tower] extension '{}' deliver_event fault ({count}/{MAX_CONSECUTIVE_FAILURES}): {fault}",
                        handle.manifest.name
                    );
                }
            }
        }
    }

    fn drain_deferred_events(&self) {
        loop {
            let events = {
                let mut guard = self
                    .deferred_events
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if guard.is_empty() {
                    return;
                }
                std::mem::take(&mut *guard)
            };

            let mut still_busy = Vec::new();
            for deferred in events {
                let Some(handle) = self
                    .handles
                    .iter()
                    .find(|handle| handle.manifest.name == deferred.extension_name)
                else {
                    continue;
                };
                if handle.is_quarantined() {
                    continue;
                }
                let mut instance = match handle.instance.try_lock() {
                    Ok(instance) => instance,
                    Err(TryLockError::Poisoned(poison)) => poison.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        still_busy.push(deferred);
                        continue;
                    }
                };
                if handle.is_quarantined() {
                    continue;
                }
                let result = instance.deliver_event(deferred.event);
                match result {
                    Ok(()) => handle.record_success(),
                    Err(fault) => {
                        let count = handle.record_fault();
                        eprintln!(
                            "[tower] extension '{}' deferred deliver_event fault ({count}/{MAX_CONSECUTIVE_FAILURES}): {fault}",
                            handle.manifest.name
                        );
                    }
                }
            }

            if !still_busy.is_empty() {
                self.deferred_events
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .extend(still_busy);
                return;
            }
        }
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ExtensionRegistry {
    /// Shut down every extension instance cleanly before dropping.
    fn drop(&mut self) {
        for handle in &mut self.handles {
            if let Ok(mut inst) = handle.instance.lock() {
                inst.shutdown();
            }
        }
    }
}

impl ExtensionHostPort for ExtensionRegistry {
    /// Called after a file has been indexed (EV2).
    ///
    /// Fans out `event/fileIndexed` to all subscribed instances.
    fn on_file_indexed(&self, id: FileId, path: &crate::domain::RelativePath) {
        let file_id = id.index() as u64;
        let path_str = path.as_str().to_owned();
        self.fan_out("event/fileIndexed", || {
            extension_protocol::Event::FileIndexed {
                file_id,
                path: path_str.clone(),
            }
        });
    }

    /// Called after a file's content has changed (EV2).
    ///
    /// Fans out `event/fileChanged` to all subscribed instances.
    fn on_file_changed(&self, id: FileId, path: &crate::domain::RelativePath) {
        let file_id = id.index() as u64;
        let path_str = path.as_str().to_owned();
        self.fan_out("event/fileChanged", || {
            extension_protocol::Event::FileChanged {
                file_id,
                path: path_str.clone(),
            }
        });
    }

    /// Called after a file has been deleted (spec 27 EV1).
    ///
    /// Fans out `event/fileDeleted` to all subscribed instances.
    fn on_file_deleted(&self, path: &crate::domain::RelativePath) {
        let path_str = path.as_str().to_owned();
        self.fan_out("event/fileDeleted", || {
            extension_protocol::Event::FileDeleted {
                path: path_str.clone(),
            }
        });
    }

    fn declared_tools(&self) -> Vec<(ExtensionId, ToolDecl)> {
        Self::declared_tools(self)
    }

    fn invoke(&self, tool_name: &str, params: Value) -> Result<Value, InvokeError> {
        Self::invoke(self, tool_name, params)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use extension_protocol::{Event, ExtensionFault, ExtensionManifest, ToolDecl};
    use serde_json::Value;

    use super::{
        ExtensionApplicationError, ExtensionCallError, ExtensionId, ExtensionInstance,
        ExtensionRegistry, InvokeError, MAX_CONSECUTIVE_FAILURES, RegistrationError,
    };
    use crate::domain::{FileId, RelativePath};
    use crate::ports::ExtensionHostPort;

    // ── Fake ExtensionInstance helpers ─────────────────────────────────────

    fn make_manifest(name: &str, tools: Vec<ToolDecl>, events: Vec<&str>) -> ExtensionManifest {
        use extension_protocol::manifest::{CapabilitiesSection, EventsSection};
        ExtensionManifest {
            name: name.to_owned(),
            version: "0.1.0".to_owned(),
            command: vec!["./stub".to_owned()],
            activation: extension_protocol::Activation::Lazy,
            tools,
            events: EventsSection {
                subscribe: events.into_iter().map(str::to_owned).collect(),
            },
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

    fn file_id() -> FileId {
        FileId::new_for_testing(0, 0)
    }

    fn file_path(s: &str) -> RelativePath {
        RelativePath::new(s)
    }

    #[test]
    fn f008_error_types_are_neutral_and_convert_faults() {
        fn assert_error_traits<T: std::error::Error + Clone + PartialEq + std::fmt::Debug>() {}

        assert_error_traits::<ExtensionApplicationError>();
        assert_error_traits::<ExtensionCallError>();

        let fault = ExtensionFault::Timeout;
        let converted = ExtensionCallError::from(fault.clone());

        assert_eq!(converted, ExtensionCallError::Fault(fault));
        assert_eq!(converted.to_string(), ExtensionFault::Timeout.to_string());
    }

    #[test]
    fn f008_error_types_retain_code_message_and_some_or_none_data_across_distinct_branches() {
        let with_data = ExtensionApplicationError {
            code: -32_001,
            message: "rename rejected".to_owned(),
            data: Some(serde_json::json!({"reason": "not renameable"})),
        };
        let without_data = ExtensionApplicationError {
            code: 42,
            message: "application failure".to_owned(),
            data: None,
        };

        assert_eq!(with_data.code, -32_001);
        assert_eq!(with_data.message, "rename rejected");
        assert_eq!(
            with_data.data,
            Some(serde_json::json!({"reason": "not renameable"}))
        );
        assert_eq!(without_data.code, 42);
        assert_eq!(without_data.message, "application failure");
        assert_eq!(without_data.data, None);
        assert_eq!(
            ExtensionCallError::Application(with_data.clone()),
            ExtensionCallError::Application(with_data)
        );
        assert_ne!(
            ExtensionCallError::Application(without_data),
            ExtensionCallError::Fault(ExtensionFault::Timeout)
        );
    }

    #[test]
    fn f008_error_types_expose_migrated_trait_and_retain_existing_invoke_variants() {
        fn call_migrated_trait(
            extension: &mut dyn ExtensionInstance,
        ) -> Result<Value, ExtensionCallError> {
            extension.call_tool("echo", Value::Bool(true))
        }

        let mut extension = RecordingExtension::new("existing", vec![], vec![]);
        assert_eq!(call_migrated_trait(&mut extension), Ok(Value::Bool(true)));
        assert_eq!(
            InvokeError::Fault(ExtensionFault::Timeout),
            InvokeError::Fault(ExtensionFault::Timeout)
        );
        assert_eq!(
            InvokeError::ToolNotFound("missing".to_owned()),
            InvokeError::ToolNotFound("missing".to_owned())
        );
    }

    // ── RecordingExtension: records every deliver_event call ──────────────

    /// A fake `ExtensionInstance` that records every `deliver_event` call and
    /// always succeeds.
    struct RecordingExtension {
        manifest: ExtensionManifest,
        delivered: Arc<Mutex<Vec<Event>>>,
    }

    impl RecordingExtension {
        fn new(name: &str, tools: Vec<ToolDecl>, events: Vec<&str>) -> Self {
            Self {
                manifest: make_manifest(name, tools, events),
                delivered: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl ExtensionInstance for RecordingExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(&mut self, _name: &str, params: Value) -> Result<Value, ExtensionCallError> {
            // Echo params back as the result.
            Ok(params)
        }

        fn deliver_event(&mut self, event: Event) -> Result<(), ExtensionFault> {
            self.delivered.lock().unwrap().push(event);
            Ok(())
        }

        fn shutdown(&mut self) {}
    }

    // ── FailingExtension: always returns a fault ─────────────────────────

    struct FailingExtension {
        manifest: ExtensionManifest,
    }

    impl FailingExtension {
        fn new(name: &str, events: Vec<&str>) -> Self {
            Self {
                manifest: make_manifest(name, vec![], events),
            }
        }
    }

    impl ExtensionInstance for FailingExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(&mut self, _name: &str, _params: Value) -> Result<Value, ExtensionCallError> {
            Err(ExtensionFault::Crashed { code: Some(1) }.into())
        }

        fn deliver_event(&mut self, _event: Event) -> Result<(), ExtensionFault> {
            Err(ExtensionFault::Crashed { code: Some(1) })
        }

        fn shutdown(&mut self) {}
    }

    // ── IdentifyingExtension: call_tool returns the extension's name ──────

    struct IdentifyingExtension {
        manifest: ExtensionManifest,
    }

    impl IdentifyingExtension {
        fn new(name: &str, tools: Vec<ToolDecl>) -> Self {
            Self {
                manifest: make_manifest(name, tools, vec![]),
            }
        }
    }

    impl ExtensionInstance for IdentifyingExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(&mut self, name: &str, _params: Value) -> Result<Value, ExtensionCallError> {
            if self.manifest.tools.iter().any(|t| t.name == name) {
                Ok(Value::String(self.manifest.name.clone()))
            } else {
                Err(ExtensionFault::ProtocolError {
                    message: format!("tool not found: {name}"),
                }
                .into())
            }
        }

        fn deliver_event(&mut self, _event: Event) -> Result<(), ExtensionFault> {
            Ok(())
        }

        fn shutdown(&mut self) {}
    }

    // ── CountingFaultExtension: faults on the first N calls ──────────────

    /// An `ExtensionInstance` that faults for its first `fault_count` `call_tool`
    /// invocations, then succeeds. Used to test quarantine reset.
    struct CountingFaultExtension {
        manifest: ExtensionManifest,
        remaining_faults: u32,
    }

    impl CountingFaultExtension {
        fn new(name: &str, tools: Vec<ToolDecl>, fault_count: u32) -> Self {
            Self {
                manifest: make_manifest(name, tools, vec![]),
                remaining_faults: fault_count,
            }
        }
    }

    impl ExtensionInstance for CountingFaultExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(&mut self, _name: &str, _params: Value) -> Result<Value, ExtensionCallError> {
            if self.remaining_faults > 0 {
                self.remaining_faults -= 1;
                Err(ExtensionFault::Timeout.into())
            } else {
                Ok(Value::Bool(true))
            }
        }

        fn deliver_event(&mut self, _event: Event) -> Result<(), ExtensionFault> {
            Ok(())
        }

        fn shutdown(&mut self) {}
    }

    /// An in-memory extension with a fixed sequence of tool outcomes.
    ///
    /// This lets registry tests drive application errors, genuine faults, and
    /// success through the public `invoke` APIs without inspecting counters.
    struct SequencedExtension {
        manifest: ExtensionManifest,
        outcomes: Vec<Result<Value, ExtensionCallError>>,
    }

    impl SequencedExtension {
        fn new(
            name: &str,
            tools: Vec<ToolDecl>,
            outcomes: Vec<Result<Value, ExtensionCallError>>,
        ) -> Self {
            Self {
                manifest: make_manifest(name, tools, vec![]),
                outcomes,
            }
        }
    }

    impl ExtensionInstance for SequencedExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(&mut self, _name: &str, _params: Value) -> Result<Value, ExtensionCallError> {
            assert!(
                !self.outcomes.is_empty(),
                "test invoked the extension more times than configured"
            );
            self.outcomes.remove(0)
        }

        fn deliver_event(&mut self, _event: Event) -> Result<(), ExtensionFault> {
            Ok(())
        }

        fn shutdown(&mut self) {}
    }

    // =========================================================================
    // TDD step 1 RED→GREEN: subscribed vs unsubscribed event delivery (AC1)
    // =========================================================================

    /// AC1: An extension subscribed to `event/fileChanged` receives the event;
    /// one subscribed only to `event/fileIndexed` does not.
    #[test]
    fn subscribed_extension_receives_event_unsubscribed_does_not() {
        let subscribed = RecordingExtension::new("sub", vec![], vec!["event/fileChanged"]);
        let subscribed_delivered = Arc::clone(&subscribed.delivered);

        let not_subscribed = RecordingExtension::new("not_sub", vec![], vec!["event/fileIndexed"]);
        let not_subscribed_delivered = Arc::clone(&not_subscribed.delivered);

        let mut registry = ExtensionRegistry::new();
        registry
            .register(Box::new(subscribed))
            .expect("register sub");
        registry
            .register(Box::new(not_subscribed))
            .expect("register not_sub");

        registry.on_file_changed(file_id(), &file_path("src/lib.rs"));

        let sub_calls = subscribed_delivered.lock().unwrap();
        assert_eq!(
            sub_calls.len(),
            1,
            "subscribed extension must receive 1 event"
        );
        assert!(
            matches!(&sub_calls[0], Event::FileChanged { path, .. } if path == "src/lib.rs"),
            "must receive FileChanged with correct path"
        );
        drop(sub_calls);

        let unsub_calls = not_subscribed_delivered.lock().unwrap();
        assert_eq!(
            unsub_calls.len(),
            0,
            "unsubscribed extension must receive 0 events"
        );
    }

    /// AC1 symmetric: `event/fileIndexed` delivered only to subscribed extension.
    #[test]
    fn subscribed_extension_receives_file_indexed_event() {
        let ext = RecordingExtension::new("indexer", vec![], vec!["event/fileIndexed"]);
        let delivered = Arc::clone(&ext.delivered);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext)).expect("register");

        registry.on_file_indexed(file_id(), &file_path("src/main.rs"));

        let calls = delivered.lock().unwrap();
        assert_eq!(calls.len(), 1, "must receive 1 fileIndexed event");
        assert!(
            matches!(&calls[0], Event::FileIndexed { path, .. } if path == "src/main.rs"),
            "event must be FileIndexed with correct path"
        );
    }

    /// Both event kinds delivered when subscribed to both.
    #[test]
    fn extension_subscribed_to_both_events_receives_both() {
        let ext = RecordingExtension::new(
            "dual",
            vec![],
            vec!["event/fileIndexed", "event/fileChanged"],
        );
        let delivered = Arc::clone(&ext.delivered);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext)).expect("register");

        registry.on_file_indexed(file_id(), &file_path("a.rs"));
        registry.on_file_changed(file_id(), &file_path("b.rs"));

        let calls = delivered.lock().unwrap();
        assert_eq!(calls.len(), 2, "must receive 2 events");
        assert!(matches!(&calls[0], Event::FileIndexed { .. }));
        assert!(matches!(&calls[1], Event::FileChanged { .. }));
    }

    // =========================================================================
    // TDD step 3 RED→GREEN: declared_tools namespacing + tool routing (AC2/AC3)
    // =========================================================================

    /// AC2: Two extensions each declaring tools — `declared_tools()` returns
    /// both sets tagged by extension identity.
    #[test]
    fn declared_tools_aggregates_all_extension_tools_with_ids() {
        let ext_a = RecordingExtension::new("ast", vec![make_tool("outline")], vec![]);
        let ext_b = RecordingExtension::new("lsp", vec![make_tool("diagnostics")], vec![]);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext_a)).expect("register ast");
        registry.register(Box::new(ext_b)).expect("register lsp");

        let tools = registry.declared_tools();
        assert_eq!(tools.len(), 2, "must aggregate 2 tools total");

        let (id_a, desc_a) = &tools[0];
        assert_eq!(id_a, &ExtensionId::new("ast"));
        assert_eq!(desc_a.name, "outline");

        let (id_b, desc_b) = &tools[1];
        assert_eq!(id_b, &ExtensionId::new("lsp"));
        assert_eq!(desc_b.name, "diagnostics");
    }

    /// An extension with multiple tools contributes all of them.
    #[test]
    fn extension_with_multiple_tools_all_appear_in_declared_tools() {
        let ext = RecordingExtension::new(
            "multi",
            vec![make_tool("tool1"), make_tool("tool2"), make_tool("tool3")],
            vec![],
        );

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext)).expect("register");

        let tools = registry.declared_tools();
        assert_eq!(tools.len(), 3);
        assert!(tools.iter().all(|(id, _)| id.as_str() == "multi"));
        let names: Vec<&str> = tools.iter().map(|(_, t)| t.name.as_str()).collect();
        assert_eq!(names, ["tool1", "tool2", "tool3"]);
    }

    /// AC3: `invoke` routes to the owning extension and returns its result.
    #[test]
    fn invoke_routes_to_owning_extension_and_returns_result() {
        let ext = IdentifyingExtension::new("ast", vec![make_tool("outline")]);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext)).expect("register");

        let result = registry
            .invoke("outline", Value::Null)
            .expect("invoke must succeed");
        assert_eq!(result, Value::String("ast".to_owned()));
    }

    /// AC3 routing: two extensions each owning a different tool — each routes correctly.
    #[test]
    fn invoke_routes_to_correct_extension_among_multiple() {
        let ext_a = IdentifyingExtension::new("ast", vec![make_tool("outline")]);
        let ext_b = IdentifyingExtension::new("lsp", vec![make_tool("diagnostics")]);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext_a)).expect("register ast");
        registry.register(Box::new(ext_b)).expect("register lsp");

        let r_a = registry
            .invoke("outline", Value::Null)
            .expect("ast/outline");
        assert_eq!(r_a, Value::String("ast".to_owned()), "must route to ast");

        let r_b = registry
            .invoke("diagnostics", Value::Null)
            .expect("lsp/diagnostics");
        assert_eq!(r_b, Value::String("lsp".to_owned()), "must route to lsp");
    }

    #[test]
    fn invoke_extension_routes_duplicate_local_tool_name_by_extension_id() {
        let ext_a = IdentifyingExtension::new("fmt", vec![make_tool("check")]);
        let ext_b = IdentifyingExtension::new("lint", vec![make_tool("check")]);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext_a)).expect("register fmt");
        registry.register(Box::new(ext_b)).expect("register lint");

        let result = registry
            .invoke_extension(&ExtensionId::new("lint"), "check", Value::Null)
            .expect("lint/check");

        assert_eq!(result, Value::String("lint".to_owned()));
    }

    #[test]
    fn f008_application_error_propagates_unchanged_through_invoke_extension() {
        let application = ExtensionApplicationError {
            code: -32_042,
            message: "rename rejected".to_owned(),
            data: Some(serde_json::json!({"outcome": "not_renameable"})),
        };
        let extension = SequencedExtension::new(
            "lsp",
            vec![make_tool("rename")],
            vec![Err(ExtensionCallError::Application(application.clone()))],
        );
        let mut registry = ExtensionRegistry::new();
        registry
            .register(Box::new(extension))
            .expect("register lsp");

        let error = registry
            .invoke_extension(&ExtensionId::new("lsp"), "rename", Value::Null)
            .expect_err("application errors must be returned to the caller");

        assert_eq!(error, InvokeError::Application(application));
    }

    /// Invoking a non-existent tool returns ToolNotFound.
    #[test]
    fn invoke_unknown_tool_returns_tool_not_found() {
        let registry = ExtensionRegistry::new();
        let err = registry
            .invoke("no_such_tool", Value::Null)
            .expect_err("must fail");
        assert!(
            matches!(err, InvokeError::ToolNotFound(ref name) if name == "no_such_tool"),
            "must return ToolNotFound: {err:?}"
        );
    }

    // =========================================================================
    // TDD step 5 RED→GREEN: per-extension error isolation (AC4)
    // =========================================================================

    /// AC4: One extension's `deliver_event` errors — the other still receives
    /// the event (per-extension error isolation, UN1).
    #[test]
    fn failing_extension_does_not_block_other_extensions() {
        let failing = FailingExtension::new("bad_ext", vec!["event/fileChanged"]);
        let good = RecordingExtension::new("good_ext", vec![], vec!["event/fileChanged"]);
        let good_delivered = Arc::clone(&good.delivered);

        let mut registry = ExtensionRegistry::new();
        // Register failing extension first — it must not block the good one.
        registry.register(Box::new(failing)).expect("register bad");
        registry.register(Box::new(good)).expect("register good");

        registry.on_file_changed(file_id(), &file_path("src/lib.rs"));

        let calls = good_delivered.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "good extension must still receive the event"
        );
    }

    /// Symmetrical: failing extension in second position does not block the first.
    #[test]
    fn failing_extension_in_second_position_does_not_block_first() {
        let good = RecordingExtension::new("good_ext", vec![], vec!["event/fileIndexed"]);
        let good_delivered = Arc::clone(&good.delivered);
        let failing = FailingExtension::new("bad_ext", vec!["event/fileIndexed"]);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(good)).expect("register good");
        registry.register(Box::new(failing)).expect("register bad");

        registry.on_file_indexed(file_id(), &file_path("a.rs"));

        let calls = good_delivered.lock().unwrap();
        assert_eq!(calls.len(), 1, "first extension must receive the event");
    }

    // =========================================================================
    // TDD step 7 RED→GREEN: quarantine after 3 faults + counter reset (AC5)
    // =========================================================================

    /// AC5 part 1: After 3 consecutive `call_tool` faults, the 4th call returns
    /// `Quarantined` without invoking the instance.
    #[test]
    fn invoke_quarantines_after_max_consecutive_failures() {
        let ext = CountingFaultExtension::new(
            "flaky",
            vec![make_tool("run")],
            MAX_CONSECUTIVE_FAILURES + 10, // always fault
        );

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext)).expect("register");

        // First MAX_CONSECUTIVE_FAILURES calls should fault (Crashed/Timeout, not Quarantined).
        for i in 0..MAX_CONSECUTIVE_FAILURES {
            let err = registry
                .invoke("run", Value::Null)
                .expect_err(&format!("call {i} must fault"));
            assert_eq!(err, InvokeError::Fault(ExtensionFault::Timeout));
        }

        // The (MAX_CONSECUTIVE_FAILURES + 1)-th call must return Quarantined.
        let err = registry
            .invoke("run", Value::Null)
            .expect_err("quarantined call must fail");
        assert!(
            matches!(err, InvokeError::Fault(ExtensionFault::Quarantined)),
            "must be Quarantined after {MAX_CONSECUTIVE_FAILURES} faults: {err:?}"
        );
    }

    /// A call queued behind the fault that reaches the quarantine threshold
    /// must observe that transition before it can enter the extension.
    #[test]
    fn concurrent_invoke_serializes_quarantine_transition_with_extension_call() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;
        use std::thread;

        struct BlockingThresholdExtension {
            manifest: ExtensionManifest,
            calls: Arc<AtomicU32>,
            threshold_entered: mpsc::Sender<()>,
            release_threshold: mpsc::Receiver<()>,
        }

        impl ExtensionInstance for BlockingThresholdExtension {
            fn manifest(&self) -> &ExtensionManifest {
                &self.manifest
            }

            fn call_tool(
                &mut self,
                _name: &str,
                _params: Value,
            ) -> Result<Value, ExtensionCallError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == MAX_CONSECUTIVE_FAILURES {
                    self.threshold_entered
                        .send(())
                        .expect("test must observe threshold call");
                    self.release_threshold
                        .recv()
                        .expect("test must release threshold call");
                }
                Err(ExtensionFault::Timeout.into())
            }

            fn deliver_event(&mut self, _event: Event) -> Result<(), ExtensionFault> {
                Ok(())
            }

            fn shutdown(&mut self) {}
        }

        let calls = Arc::new(AtomicU32::new(0));
        let (threshold_entered_tx, threshold_entered_rx) = mpsc::channel();
        let (release_threshold_tx, release_threshold_rx) = mpsc::channel();
        let extension = BlockingThresholdExtension {
            manifest: make_manifest("blocking", vec![make_tool("run")], vec![]),
            calls: Arc::clone(&calls),
            threshold_entered: threshold_entered_tx,
            release_threshold: release_threshold_rx,
        };
        let mut registry = ExtensionRegistry::new();
        registry
            .register(Box::new(extension))
            .expect("register blocking extension");

        for _ in 0..MAX_CONSECUTIVE_FAILURES - 1 {
            assert_eq!(
                registry.invoke("run", Value::Null),
                Err(InvokeError::Fault(ExtensionFault::Timeout))
            );
        }

        let registry = Arc::new(registry);
        let threshold_registry = Arc::clone(&registry);
        let threshold_call = thread::spawn(move || threshold_registry.invoke("run", Value::Null));
        threshold_entered_rx
            .recv()
            .expect("threshold call must enter extension");

        let queued_registry = Arc::clone(&registry);
        let queued_call = thread::spawn(move || queued_registry.invoke("run", Value::Null));
        release_threshold_tx
            .send(())
            .expect("release threshold call");

        assert_eq!(
            threshold_call.join().expect("threshold thread must join"),
            Err(InvokeError::Fault(ExtensionFault::Timeout))
        );
        assert_eq!(
            queued_call.join().expect("queued thread must join"),
            Err(InvokeError::Fault(ExtensionFault::Quarantined))
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_CONSECUTIVE_FAILURES,
            "queued call must not enter a quarantined extension"
        );
    }

    /// AC5 part 2: A successful call before the limit resets the counter.
    ///
    /// Scenario: 2 faults, 1 success, 2 faults — the extension is NOT quarantined
    /// because the success reset the counter mid-sequence.
    #[test]
    fn successful_call_resets_consecutive_fault_counter() {
        // F, F, OK, F, F, F must not quarantine until the final fault because
        // the successful call resets the pre-existing count.
        let ext = SequencedExtension::new(
            "flaky",
            vec![make_tool("run")],
            vec![
                Err(ExtensionFault::Timeout.into()),
                Err(ExtensionFault::Timeout.into()),
                Ok(Value::Bool(true)),
                Err(ExtensionFault::Timeout.into()),
                Err(ExtensionFault::Timeout.into()),
                Err(ExtensionFault::Timeout.into()),
            ],
        );

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext)).expect("register");

        // Calls 0 and 1: genuine faults (counter → 1, → 2).
        for i in 0..2u32 {
            let err = registry
                .invoke("run", Value::Null)
                .expect_err(&format!("call {i} must fault"));
            assert_eq!(err, InvokeError::Fault(ExtensionFault::Timeout));
        }

        // Call 2: success — resets counter to 0.
        let success = registry
            .invoke("run", Value::Null)
            .expect("call 2 must succeed and reset the counter");
        assert_eq!(success, Value::Bool(true));

        // Calls 3 through 5 are genuine faults, not a premature quarantine.
        // The next call confirms the third post-success fault reached the limit.
        for i in 3..6u32 {
            let error = registry
                .invoke("run", Value::Null)
                .expect_err(&format!("call {i} must be a genuine fault"));
            assert_eq!(error, InvokeError::Fault(ExtensionFault::Timeout));
        }
        assert_eq!(
            registry
                .invoke("run", Value::Null)
                .expect_err("call after three post-success faults must quarantine"),
            InvokeError::Fault(ExtensionFault::Quarantined)
        );
    }

    #[test]
    fn f008_application_error_preserves_an_existing_fault_count() {
        let application = ExtensionApplicationError {
            code: -32_043,
            message: "server declined request".to_owned(),
            data: None,
        };
        let ext = SequencedExtension::new(
            "lsp",
            vec![make_tool("references")],
            vec![
                Err(ExtensionFault::Timeout.into()),
                Err(ExtensionCallError::Application(application.clone())),
                Err(ExtensionFault::Timeout.into()),
                Err(ExtensionFault::Timeout.into()),
            ],
        );
        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(ext)).expect("register lsp");

        assert_eq!(
            registry
                .invoke("references", Value::Null)
                .expect_err("first call must be a genuine fault"),
            InvokeError::Fault(ExtensionFault::Timeout)
        );
        assert_eq!(
            registry
                .invoke("references", Value::Null)
                .expect_err("application error must be propagated"),
            InvokeError::Application(application)
        );
        for call in 3..=4 {
            assert_eq!(
                registry
                    .invoke("references", Value::Null)
                    .expect_err(&format!("call {call} must be a genuine fault")),
                InvokeError::Fault(ExtensionFault::Timeout)
            );
        }
        assert_eq!(
            registry
                .invoke("references", Value::Null)
                .expect_err("three genuine faults around an application error must quarantine"),
            InvokeError::Fault(ExtensionFault::Quarantined)
        );
    }

    /// AC5 part 3: Event delivery also increments the quarantine counter.
    #[test]
    fn deliver_event_faults_contribute_to_quarantine_counter() {
        let failing = FailingExtension::new("bad", vec!["event/fileChanged"]);
        let good = RecordingExtension::new("good", vec![], vec!["event/fileChanged"]);
        let good_delivered = Arc::clone(&good.delivered);

        let mut registry = ExtensionRegistry::new();
        registry.register(Box::new(failing)).expect("register bad");
        registry.register(Box::new(good)).expect("register good");

        // Deliver MAX_CONSECUTIVE_FAILURES events → bad is quarantined.
        for i in 0..MAX_CONSECUTIVE_FAILURES {
            registry.on_file_changed(file_id(), &file_path(&format!("f{i}.rs")));
        }
        // After quarantine, one more event → bad is skipped; good still gets it.
        registry.on_file_changed(file_id(), &file_path("final.rs"));

        let good_calls = good_delivered.lock().unwrap();
        // good must receive all MAX_CONSECUTIVE_FAILURES + 1 events.
        assert_eq!(
            good_calls.len() as u32,
            MAX_CONSECUTIVE_FAILURES + 1,
            "good extension must receive all events"
        );
    }

    // =========================================================================
    // AC6: No-op when no extensions registered
    // =========================================================================

    /// AC6: Empty registry — events fire without panicking, no errors raised.
    #[test]
    fn empty_registry_events_are_noop() {
        let registry = ExtensionRegistry::new();
        // Must not panic.
        registry.on_file_indexed(file_id(), &file_path("a.rs"));
        registry.on_file_changed(file_id(), &file_path("b.rs"));
        assert!(registry.declared_tools().is_empty());
    }

    /// OP1: Empty registry declared_tools returns empty vec.
    #[test]
    fn empty_registry_declared_tools_is_empty() {
        assert!(ExtensionRegistry::new().declared_tools().is_empty());
    }

    // =========================================================================
    // Registration error cases
    // =========================================================================

    /// Registering two extensions with the same manifest name is rejected.
    #[test]
    fn register_rejects_duplicate_extension_name() {
        let mut registry = ExtensionRegistry::new();
        let first = RecordingExtension::new("ast", vec![make_tool("outline")], vec![]);
        let second = RecordingExtension::new("ast", vec![make_tool("symbols")], vec![]);

        registry
            .register(Box::new(first))
            .expect("first registration");
        let err = registry
            .register(Box::new(second))
            .expect_err("duplicate must fail");
        assert_eq!(err, RegistrationError::DuplicateName("ast".to_owned()));

        // Only the first extension's tools are visible.
        let tools = registry.declared_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].1.name, "outline");
    }

    // =========================================================================
    // ExtensionId type checks
    // =========================================================================

    #[test]
    fn extension_id_equality_and_display() {
        let id1 = ExtensionId::new("ast");
        let id2 = ExtensionId::new("ast");
        let id3 = ExtensionId::new("lsp");
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
        assert_eq!(id1.to_string(), "ast");
        assert_eq!(id1.as_str(), "ast");
    }

    // =========================================================================
    // ExtensionHostPort trait object usage (U3: depends only on the trait)
    // =========================================================================

    /// The registry is usable as `&dyn ExtensionHostPort`.
    #[test]
    fn registry_usable_as_dyn_extension_host_port() {
        let registry = ExtensionRegistry::new();
        let port: &dyn ExtensionHostPort = &registry;
        // No-op calls on the trait object must not panic.
        port.on_file_indexed(file_id(), &file_path("x.rs"));
        port.on_file_changed(file_id(), &file_path("y.rs"));
        assert!(port.declared_tools().is_empty());
    }

    // =========================================================================
    // TDD steps 9-10 RED→GREEN: AC6 graceful shutdown (EV2)
    //
    // Spec 25 EV2: "When the engine exits, the host shall request `shutdown`
    // from each running extension, then escalate SIGTERM→SIGKILL on timeout."
    //
    // The domain obligation is: Drop for ExtensionRegistry calls shutdown() on
    // every registered instance.  The SIGTERM/SIGKILL escalation lives in the
    // SidecarHostAdapter (adapters/extension/sidecar.rs) and is the adapter's
    // contract; here we verify the registry's shutdown dispatch.
    // =========================================================================

    /// A fake `ExtensionInstance` that records whether `shutdown()` was called.
    struct ShutdownRecordingExtension {
        manifest: ExtensionManifest,
        /// Shared flag: set to `true` when `shutdown()` is called.
        was_shut_down: Arc<Mutex<bool>>,
    }

    impl ShutdownRecordingExtension {
        fn new(name: &str) -> (Self, Arc<Mutex<bool>>) {
            let flag = Arc::new(Mutex::new(false));
            let ext = Self {
                manifest: make_manifest(name, vec![], vec![]),
                was_shut_down: Arc::clone(&flag),
            };
            (ext, flag)
        }
    }

    impl ExtensionInstance for ShutdownRecordingExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }

        fn call_tool(&mut self, _name: &str, _params: Value) -> Result<Value, ExtensionCallError> {
            Ok(Value::Null)
        }

        fn deliver_event(&mut self, _event: Event) -> Result<(), ExtensionFault> {
            Ok(())
        }

        fn shutdown(&mut self) {
            *self.was_shut_down.lock().unwrap() = true;
        }
    }

    /// AC6 (EV2): Dropping an `ExtensionRegistry` calls `shutdown()` on every
    /// registered instance — a single instance case.
    #[test]
    fn drop_registry_calls_shutdown_on_registered_instance() {
        let (ext, was_shut_down) = ShutdownRecordingExtension::new("ast");

        {
            let mut registry = ExtensionRegistry::new();
            registry.register(Box::new(ext)).expect("register");
            // registry drops here at end of scope
        }

        assert!(
            *was_shut_down.lock().unwrap(),
            "shutdown() must be called on the instance when the registry is dropped"
        );
    }

    /// AC6 (EV2): Dropping a registry with multiple extensions calls `shutdown()`
    /// on **all** of them — not just the first.
    #[test]
    fn drop_registry_calls_shutdown_on_all_registered_instances() {
        let (ext_a, flag_a) = ShutdownRecordingExtension::new("ast");
        let (ext_b, flag_b) = ShutdownRecordingExtension::new("lsp");
        let (ext_c, flag_c) = ShutdownRecordingExtension::new("formatter");

        {
            let mut registry = ExtensionRegistry::new();
            registry.register(Box::new(ext_a)).expect("register ast");
            registry.register(Box::new(ext_b)).expect("register lsp");
            registry
                .register(Box::new(ext_c))
                .expect("register formatter");
            // registry drops here — must call shutdown() on all three
        }

        assert!(*flag_a.lock().unwrap(), "shutdown() must be called on ast");
        assert!(*flag_b.lock().unwrap(), "shutdown() must be called on lsp");
        assert!(
            *flag_c.lock().unwrap(),
            "shutdown() must be called on formatter"
        );
    }

    /// AC6 (EV2): Empty registry drop is a no-op — must not panic.
    #[test]
    fn drop_empty_registry_is_noop() {
        let registry = ExtensionRegistry::new();
        drop(registry); // must not panic
    }

    // ── Spec 27 TDD: on_file_deleted fan-out ─────────────────────────────────

    /// Spec 27 EV1: An extension subscribed to `event/fileDeleted` receives the
    /// event when `on_file_deleted` is called; one not subscribed does not.
    #[test]
    fn on_file_deleted_fans_out_to_subscribed_only() {
        let subscribed = RecordingExtension::new("lsp", vec![], vec!["event/fileDeleted"]);
        let subscribed_delivered = Arc::clone(&subscribed.delivered);

        let not_subscribed = RecordingExtension::new(
            "ast",
            vec![],
            vec!["event/fileIndexed", "event/fileChanged"],
        );
        let not_subscribed_delivered = Arc::clone(&not_subscribed.delivered);

        let mut registry = ExtensionRegistry::new();
        registry
            .register(Box::new(subscribed))
            .expect("register lsp");
        registry
            .register(Box::new(not_subscribed))
            .expect("register ast");

        registry.on_file_deleted(&file_path("src/lib.rs"));

        let sub_calls = subscribed_delivered.lock().unwrap();
        assert_eq!(
            sub_calls.len(),
            1,
            "subscribed extension must receive 1 FileDeleted event"
        );
        assert!(
            matches!(&sub_calls[0], Event::FileDeleted { path } if path == "src/lib.rs"),
            "must receive FileDeleted with correct path; got: {:?}",
            sub_calls[0]
        );
        drop(sub_calls);

        let unsub_calls = not_subscribed_delivered.lock().unwrap();
        assert_eq!(
            unsub_calls.len(),
            0,
            "unsubscribed extension must NOT receive the event"
        );
    }

    /// Spec 27: `NoOpExtensionHost::on_file_deleted` is a no-op — must not panic.
    #[test]
    fn noop_extension_host_on_file_deleted_is_noop() {
        use crate::ports::NoOpExtensionHost;
        let host = NoOpExtensionHost;
        host.on_file_deleted(&file_path("src/main.rs")); // must not panic
    }
}
