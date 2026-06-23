//! Extension supervisor — lazy respawn with exponential backoff (spec 24).
//!
//! `ExtensionSupervisor` wraps a [`SidecarHostAdapter`] and implements
//! [`ExtensionInstance`]. It owns the manifest + [`HostDeps`] + timeout config
//! needed to respawn a crashed or timed-out child.
//!
//! # Restart policy (spec 24 UN2)
//!
//! After a `Crashed` or `Timeout` fault the live instance is discarded. On the
//! **next** call the supervisor checks whether the backoff delay has elapsed:
//!   - If yes → respawn, clear the backoff state, attempt the call.
//!   - If no  → return the same fault kind immediately (without blocking).
//!
//! Backoff formula: `min(2^n · BASE_BACKOFF_MS ms, MAX_BACKOFF_MS ms)`.
//!
//! # Quarantine (spec 24 S1 / AC4)
//!
//! The domain registry (spec 22 `ExtensionRegistry`) tracks consecutive faults
//! and enforces the quarantine threshold. The supervisor itself does **not**
//! implement quarantine — it just respawns on next call as long as it is asked.
//! The registry stops calling it once it is quarantined.
//!
//! # Hexagonal boundary
//!
//! This module lives in `adapters/extension/`. It uses `std::process` (via
//! `SidecarHostAdapter::spawn`) and `std::time`. It must never be imported by
//! `domain/`.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use extension_protocol::{ExtensionFault, ExtensionManifest};
use serde_json::Value;

use super::host_deps::HostDeps;
use super::sidecar::SidecarHostAdapter;
use crate::domain::ExtensionInstance;

// ── Backoff constants ─────────────────────────────────────────────────────────

/// Base backoff for the first respawn attempt after a fault.
const BASE_BACKOFF: Duration = Duration::from_millis(100);

/// Maximum backoff cap (prevents unbounded growth).
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Compute the nth backoff duration: `min(2^n · BASE_BACKOFF, MAX_BACKOFF)`.
fn backoff_for_attempt(n: u32) -> Duration {
    // 2^n · BASE_BACKOFF, saturating at MAX_BACKOFF.
    // Use checked_shl to avoid overflow for large n; fall back to MAX_BACKOFF.
    let factor: u128 = 1u128.checked_shl(n).unwrap_or(u128::MAX);
    let ms = BASE_BACKOFF
        .as_millis()
        .saturating_mul(factor)
        .min(MAX_BACKOFF.as_millis());
    Duration::from_millis(ms as u64)
}

// ── RespawnState ──────────────────────────────────────────────────────────────

/// Respawn readiness state.
#[derive(Debug)]
enum RespawnState {
    /// No pending backoff — the supervisor will try to spawn/call immediately.
    Ready,
    /// A fault occurred; waiting until `ready_at` before retrying.
    Backoff {
        /// Earliest instant at which a respawn attempt is allowed.
        ready_at: Instant,
    },
}

// ── ExtensionSupervisor ───────────────────────────────────────────────────────

/// Wraps a [`SidecarHostAdapter`] with lazy-respawn and exponential-backoff
/// supervision (spec 24).
///
/// # Lifecycle
///
/// 1. First call → `SidecarHostAdapter::spawn`.
/// 2. Fault (`Timeout` / `Crashed`) → drop instance, enter backoff.
/// 3. Next call after backoff → respawn, reset backoff on success.
/// 4. Quarantine (decided externally by the registry) → supervisor keeps
///    offering to respawn; the registry simply stops calling it.
///
/// # Fault propagation
///
/// - `Timeout` and `Crashed` are re-emitted to the caller (so the registry
///   can count them toward the quarantine threshold).
/// - `ProtocolError` is re-emitted as-is (counts as a fault in the registry).
/// - `Quarantined` is never generated here; that is the registry's decision.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::{Arc, Mutex};
/// use std::time::Duration;
/// use core_engine::adapters::extension::supervisor::ExtensionSupervisor;
/// use core_engine::adapters::extension::HostDeps;
/// use core_engine::adapters::{InMemoryAstIndex, InMemoryFs};
/// use core_engine::adapters::formatter::NoOpFormatQueue;
/// use extension_protocol::ExtensionManifest;
/// use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
///
/// let manifest = ExtensionManifest {
///     name: "example".to_owned(),
///     version: "0.1.0".to_owned(),
///     command: vec!["/usr/bin/my-ext".to_owned()],
///     activation: Activation::Lazy,
///     tools: vec![],
///     events: EventsSection::default(),
///     capabilities: CapabilitiesSection::default(),
/// };
/// let deps = HostDeps {
///     fs: Arc::new(Mutex::new(InMemoryFs::new())),
///     ast_index: Arc::new(InMemoryAstIndex::new()),
///     format_queue: Arc::new(NoOpFormatQueue),
///     apply_edits: Arc::new(core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost),
///     push_tx: None,
/// };
/// let _supervisor = ExtensionSupervisor::new(manifest, deps, Duration::from_secs(30));
/// ```
pub struct ExtensionSupervisor {
    /// The extension manifest (immutable for lifetime of this supervisor).
    manifest: ExtensionManifest,
    /// Port dependencies — cloned for each respawn attempt.
    deps: HostDeps,
    /// Per-request wall-clock deadline forwarded to the adapter.
    request_timeout: Duration,
    /// Live adapter instance, `None` when the child has crashed/timed-out.
    instance: Option<Box<dyn ExtensionInstance>>,
    /// Current respawn readiness.
    respawn_state: RespawnState,
    /// Monotonically increasing count of consecutive faults (for backoff).
    consecutive_faults: u32,
}

impl std::fmt::Debug for ExtensionSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionSupervisor")
            .field("name", &self.manifest.name)
            .field("has_instance", &self.instance.is_some())
            .field("consecutive_faults", &self.consecutive_faults)
            .finish_non_exhaustive()
    }
}

impl ExtensionSupervisor {
    /// Create a new supervisor. The child is **not** spawned yet — spawning is
    /// lazy: the first `call_tool` or `deliver_event` triggers it.
    #[must_use]
    pub fn new(manifest: ExtensionManifest, deps: HostDeps, request_timeout: Duration) -> Self {
        Self {
            manifest,
            deps,
            request_timeout,
            instance: None,
            respawn_state: RespawnState::Ready,
            consecutive_faults: 0,
        }
    }

    /// Ensure a live instance is available, respawning if necessary.
    ///
    /// Returns `Err(ExtensionFault::Crashed)` if the backoff delay has not yet
    /// elapsed, or `Err(ExtensionFault::ProtocolError)` if the spawn itself
    /// fails.
    fn ensure_instance(&mut self) -> Result<(), ExtensionFault> {
        if self.instance.is_some() {
            return Ok(());
        }

        // Check backoff.
        match &self.respawn_state {
            RespawnState::Backoff { ready_at, .. } => {
                if Instant::now() < *ready_at {
                    // Backoff not elapsed — report Crashed so the registry
                    // counts the fault; we do NOT block the caller.
                    return Err(ExtensionFault::Crashed { code: None });
                }
            }
            RespawnState::Ready => {}
        }

        // Attempt a spawn.
        match SidecarHostAdapter::spawn(
            self.manifest.clone(),
            self.clone_deps(),
            self.request_timeout,
        ) {
            Ok(instance) => {
                self.instance = Some(instance);
                self.respawn_state = RespawnState::Ready;
                self.consecutive_faults = 0;
                Ok(())
            }
            Err(fault) => {
                self.record_fault();
                Err(fault)
            }
        }
    }

    /// Record a fault, drop the instance, and schedule the next backoff window.
    fn record_fault(&mut self) {
        self.instance = None;
        self.consecutive_faults = self.consecutive_faults.saturating_add(1);
        let delay = backoff_for_attempt(self.consecutive_faults.saturating_sub(1));
        self.respawn_state = RespawnState::Backoff {
            ready_at: Instant::now() + delay,
        };
    }

    /// Reset fault state after a successful call.
    fn record_success(&mut self) {
        self.consecutive_faults = 0;
        self.respawn_state = RespawnState::Ready;
    }

    /// Clone the `HostDeps` — cheap because all fields are `Arc`; `push_tx` is cloned too.
    fn clone_deps(&self) -> HostDeps {
        HostDeps {
            fs: std::sync::Arc::clone(&self.deps.fs),
            ast_index: std::sync::Arc::clone(&self.deps.ast_index),
            format_queue: std::sync::Arc::clone(&self.deps.format_queue),
            apply_edits: std::sync::Arc::clone(&self.deps.apply_edits),
            push_tx: self.deps.push_tx.clone(),
        }
    }
}

impl ExtensionInstance for ExtensionSupervisor {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn call_tool(&mut self, name: &str, params: Value) -> Result<Value, ExtensionFault> {
        self.ensure_instance()?;

        let result = self
            .instance
            .as_mut()
            .expect("ensure_instance succeeded")
            .call_tool(name, params);

        match &result {
            Ok(_) => self.record_success(),
            Err(_) => self.record_fault(),
        }
        result
    }

    fn deliver_event(&mut self, event: extension_protocol::Event) -> Result<(), ExtensionFault> {
        self.ensure_instance()?;

        let result = self
            .instance
            .as_mut()
            .expect("ensure_instance succeeded")
            .deliver_event(event);

        match &result {
            Ok(()) => self.record_success(),
            Err(_) => self.record_fault(),
        }
        result
    }

    fn shutdown(&mut self) {
        if let Some(mut inst) = self.instance.take() {
            inst.shutdown();
        }
    }
}
