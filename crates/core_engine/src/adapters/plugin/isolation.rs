//! Plugin fault isolation — fuel/epoch compute bounds, trap catching, and
//! sandbox restart with quarantine (spec 11d).
//!
//! # Wireframe
//!
//! ```text
//!  IsolatedSandbox (wraps one WasmInstance or a fault state)
//!    state: Ready(WasmInstance)
//!         | Failed { consecutive_failures: u32 }
//!         | Quarantined
//!
//!  call_tool / deliver_hook:
//!    if Quarantined  -> PluginFault::Quarantined
//!    if Failed       -> try recreate (supervisor step); if MAX exceeded -> Quarantine
//!    instance.apply_compute_bounds(config)        [fuel + epoch on store]
//!    match guest_call():
//!      Ok(v)                -> state stays Ready; return v
//!      Err(fuel-exhausted)  -> state = Failed; return PluginFault::FuelExhausted
//!      Err(epoch-exceeded)  -> state = Failed; return PluginFault::EpochDeadlineExceeded
//!      Err(trap/other)      -> state = Failed; return PluginFault::Trapped(msg)
//! ```
//!
//! # Design decisions
//!
//! ## Supervisor: lazy recreate vs. background thread
//!
//! Decision: **lazy recreate on next call** — the sandbox is recreated the next
//! time a caller invokes a tool, not asynchronously in the background.
//!
//! Why: `PluginInstance` calls are synchronous and mediated by a `Mutex`.
//! A background thread would need channels and complex shared state, adding
//! complexity without benefit: the recreate is fast (`WasmtimeHost::load_with_engine`
//! takes ~1–2 ms for a small `.wasm`) and callers already expect `Err(PluginFault)`
//! on the failing call. The next call triggers recreate and either succeeds or
//! increments the failure counter.
//!
//! Trade-off: the first call after a failure pays the recreate cost. A background
//! thread would amortise that cost but adds cross-thread state sharing complexity
//! disproportionate to the benefit.
//!
//! ## Quarantine threshold
//!
//! `MAX_CONSECUTIVE_FAILURES = 3`. After 3 consecutive restart failures the
//! sandbox is quarantined: no further restart attempts; all calls return
//! `PluginFault::Quarantined`.
//!
//! Why 3: stops tight restart loops quickly while surviving transient failures.
//! The spec requires "back off / quarantine" (UN3); the exact threshold is a
//! policy decision not specified in EARS.
//!
//! ## Fuel vs. epoch
//!
//! Both are configured via `IsolationConfig`:
//! - **Fuel** (`Config::consume_fuel`): deterministic instruction budget. Tests
//!   use a small fuel budget to interrupt the loop fixture in microseconds with
//!   no wall-clock dependency (per spec NOTE: prefer deterministic triggers).
//! - **Epoch** (`Config::epoch_interruption`): wall-clock bound. The store's
//!   epoch deadline is set to `current_epoch + epoch_deadline_ticks` before
//!   each call. In tests, `engine.increment_epoch()` advances the epoch
//!   deterministically rather than relying on a real background timer.
//!
//! ## Concrete type in state, not trait object
//!
//! Decision: `SandboxState::Ready(WasmInstance)` holds the concrete wasmtime
//! type, not `Box<dyn PluginInstance>`. This allows calling
//! `instance.apply_compute_bounds(config)` directly without Any-downcasting.
//!
//! Why: adding `as_any_mut` to the domain `PluginInstance` trait would let
//! wasmtime knowledge leak across the hexagonal boundary. Holding the concrete
//! type in the adapter layer is the correct home.
//!
//! Trade-off: `IsolatedSandbox` is coupled to `WasmInstance`; acceptable
//! because both live in `adapters/plugin/`.
//!
//! ## Trap/panic catch strategy
//!
//! wasmtime's `TypedFunc::call` returns `Err(wasmtime::Error)` for all trap
//! kinds: guest `unreachable`, fuel exhaustion, epoch deadline. The error
//! message distinguishes the cause. We inspect it to classify the fault kind.
//!
//! Host-fn panics: wasmtime propagates host-function panics through the wasm
//! frame as `anyhow::Error`. They arrive as `Err(...)` from `TypedFunc::call`
//! and are therefore caught by the same `Err` path. No `catch_unwind` needed.
//!
//! # Hexagonal boundary
//!
//! This module lives in `adapters/plugin/` — the only wasmtime-importing layer.
//! The domain (`plugin_host`) receives only `PluginFaultKind` via `PluginHostError`
//! and never sees wasmtime types.

// Decision: no #![forbid(unsafe_code)] here because this module contains
// `unsafe impl Send for IsolatedSandbox`. The Send impl is bounded by the
// same invariant as WasmInstance in loader.rs: exclusive access via Mutex.
// All other code in this file is safe.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wasmtime::{Config, Engine};

use plugin_sdk::{HookKind, HookPayload, PluginManifest, Value};

use crate::domain::{PluginFaultKind, PluginHostError, PluginInstance};
use crate::ports::FileSystemPort;

use super::loader::{WasmInstance, WasmtimeHost};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of consecutive restart failures before the sandbox is quarantined.
///
/// Decision: 3. Low enough to prevent tight restart loops quickly; high enough
/// to survive a transient OS file I/O hiccup.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Default per-call fuel budget (wasmtime instruction units).
///
/// 100_000_000 units is generous for normal tool handlers. An infinite loop
/// exhausts this budget in well under 1 second. Tests use smaller budgets
/// (e.g. 1_000) for near-instant interruption.
pub const DEFAULT_FUEL_BUDGET: u64 = 100_000_000;

// ── IsolationEngine ────────────────────────────────────────────────────────────

/// A wasmtime `Engine` pre-configured for fault isolation.
///
/// Both `consume_fuel` and `epoch_interruption` are enabled. Share one
/// `IsolationEngine` across all sandboxes to exploit the JIT compilation cache.
///
/// A background ticker thread calls [`Engine::increment_epoch`] every 10 ms
/// so that `epoch_deadline_ticks`-based deadlines fire without any additional
/// caller setup. The thread is stopped cleanly when `IsolationEngine` is dropped.
///
/// # Example
///
/// ```rust,ignore
/// let ie = IsolationEngine::new().unwrap();
/// let sandbox = IsolatedSandbox::load(
///     ie.engine(), wasm_path, fs_port, IsolationConfig::default()
/// ).unwrap();
/// ```
pub struct IsolationEngine {
    engine: Engine,
    /// Signals the epoch ticker thread to stop (set true on drop).
    ticker_stop: Arc<AtomicBool>,
    /// Background thread handle — joined on drop.
    _ticker_thread: std::thread::JoinHandle<()>,
}

impl IsolationEngine {
    /// Create an engine with fuel consumption and epoch interruption enabled.
    ///
    /// Spawns a background thread that increments the engine epoch every 10 ms,
    /// making `epoch_deadline_ticks` deadlines functional without caller setup.
    ///
    /// # Errors
    ///
    /// Returns an error string if wasmtime fails to initialise (extremely rare).
    pub fn new() -> Result<Self, String> {
        let mut config = Config::new();
        // Decision: enable both fuel and epoch.
        // Why: fuel = deterministic instruction budget (required for tests per
        //      spec NOTE); epoch = wall-clock bound for production. Both are
        //      built-in wasmtime features — zero new dependencies.
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| e.to_string())?;

        // Spawn the background epoch ticker so that epoch_deadline_ticks
        // deadlines fire in production without any additional setup.
        // Decision: 10 ms tick interval.
        // Why: coarse-grained — a 10 ms tick means worst-case latency for
        //      epoch interruption is ~10 ms × epoch_deadline_ticks, which is
        //      acceptable for a per-plugin compute bound. Finer ticks increase
        //      the ticker's syscall rate for negligible accuracy benefit.
        // Trade-off: 10 ms wakeup thread always running; negligible CPU cost
        //            (sleep-dominated). Alternative: no ticker — but that
        //            leaves epoch_deadline_ticks a silent no-op in production
        //            (UN2 violation for infinite-loop plugins).
        let ticker_stop = Arc::new(AtomicBool::new(false));
        let ticker_engine = engine.clone();
        let ticker_stop_clone = Arc::clone(&ticker_stop);
        let ticker_thread = std::thread::Builder::new()
            .name("tower-epoch-ticker".to_owned())
            .spawn(move || {
                while !ticker_stop_clone.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                    ticker_engine.increment_epoch();
                }
            })
            .map_err(|e| format!("failed to spawn epoch ticker thread: {e}"))?;

        Ok(Self {
            engine,
            ticker_stop,
            _ticker_thread: ticker_thread,
        })
    }

    /// Borrow the inner [`Engine`].
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

impl Drop for IsolationEngine {
    fn drop(&mut self) {
        // Signal the ticker thread to stop. We do not join because the thread
        // may be sleeping (up to 10 ms) and the caller should not block on drop.
        // The JoinHandle is held as `_ticker_thread`; it will be dropped (and
        // the thread eventually cleaned up by the OS) after this flag is set.
        self.ticker_stop.store(true, Ordering::Relaxed);
    }
}

// ── IsolationConfig ────────────────────────────────────────────────────────────

/// Per-sandbox compute bound configuration.
///
/// Applied before every guest call to enforce the configured limits.
#[derive(Debug, Clone)]
pub struct IsolationConfig {
    /// Fuel units granted before each guest call.
    ///
    /// `None` disables fuel (not recommended for production).
    pub fuel_budget: Option<u64>,

    /// Epoch deadline: `store.set_epoch_deadline(ticks)` before each call.
    ///
    /// `None` disables epoch interruption.
    ///
    /// In tests: call `engine.increment_epoch()` to advance the epoch
    /// deterministically (per spec NOTE: prefer deterministic triggers in tests).
    pub epoch_deadline_ticks: Option<u64>,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            fuel_budget: Some(DEFAULT_FUEL_BUDGET),
            // Epoch disabled by default: a background ticker is needed for
            // useful wall-clock bounds in production. Callers opt in explicitly.
            epoch_deadline_ticks: None,
        }
    }
}

// ── SandboxState ──────────────────────────────────────────────────────────────

/// Internal lifecycle state of one sandbox slot.
enum SandboxState {
    /// Ready to accept calls.
    Ready(WasmInstance),
    /// Last call trapped or exceeded its budget; next call triggers recreate.
    Failed {
        /// Number of consecutive restart attempts that have already failed.
        consecutive_failures: u32,
    },
    /// Permanently disabled after `MAX_CONSECUTIVE_FAILURES` restart failures.
    Quarantined,
}

// ── IsolatedSandbox ────────────────────────────────────────────────────────────

/// A fault-isolating wrapper around one wasm plugin sandbox (spec 11d).
///
/// Enforces per-call fuel and epoch budgets, catches wasm traps (including
/// guest panics compiled to `unreachable`), and drives the lazy-recreate
/// supervisor with quarantine on repeated failures.
///
/// Implements [`PluginInstance`] so the domain registry treats it as an opaque
/// plugin — the isolation mechanism is transparent to domain code.
///
/// # Thread safety
///
/// Not `Sync`. The registry wraps it in `Mutex<Box<dyn PluginInstance>>`.
pub struct IsolatedSandbox {
    /// Path used to recreate the sandbox on failure.
    wasm_path: PathBuf,
    /// Filesystem port forwarded to recreated instances.
    fs_port: Arc<dyn FileSystemPort + Send + Sync>,
    /// Shared engine (fuel + epoch enabled).
    engine: Engine,
    /// Per-call compute bound settings.
    config: IsolationConfig,
    /// Cached manifest — always available even when Failed or Quarantined.
    manifest: PluginManifest,
    /// Current lifecycle state.
    state: SandboxState,
}

// Safety: same rationale as WasmInstance in loader.rs.
// IsolatedSandbox is accessed exclusively through a Mutex; Engine is Send;
// WasmInstance is explicitly Send (marked in loader.rs).
unsafe impl Send for IsolatedSandbox {}

impl IsolatedSandbox {
    /// Load a `.wasm` plugin into a fault-isolated sandbox.
    ///
    /// The `engine` must have `consume_fuel(true)` and
    /// `epoch_interruption(true)` — use [`IsolationEngine::new`].
    ///
    /// # Errors
    ///
    /// Returns [`super::error::PluginLoadError`] on wasm parse/ABI/link errors.
    pub fn load(
        engine: &Engine,
        wasm_path: impl Into<PathBuf>,
        fs_port: Arc<dyn FileSystemPort + Send + Sync>,
        config: IsolationConfig,
    ) -> Result<Self, super::error::PluginLoadError> {
        let wasm_path = wasm_path.into();
        let wasm_instance =
            WasmtimeHost::load_with_engine(engine, &wasm_path, Arc::clone(&fs_port))?;
        let manifest = wasm_instance.manifest().clone();
        Ok(Self {
            wasm_path,
            fs_port,
            engine: engine.clone(),
            config,
            manifest,
            state: SandboxState::Ready(wasm_instance),
        })
    }

    /// Return `true` if the sandbox is permanently quarantined.
    pub fn is_quarantined(&self) -> bool {
        matches!(self.state, SandboxState::Quarantined)
    }

    /// Return `true` if the sandbox is operational.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, SandboxState::Ready(_))
    }

    // ── Supervisor: lazy recreate ─────────────────────────────────────────────

    /// Attempt to recreate the sandbox from the `.wasm` file.
    ///
    /// On success: transitions `Failed` → `Ready`.
    /// On failure: increments `consecutive_failures`; transitions to
    /// `Quarantined` when `MAX_CONSECUTIVE_FAILURES` is reached.
    fn try_recreate(&mut self) -> Result<(), PluginHostError> {
        let consecutive_failures = match &self.state {
            SandboxState::Quarantined => {
                return Err(PluginHostError::PluginFault(PluginFaultKind::Quarantined))
            }
            SandboxState::Ready(_) => return Ok(()),
            SandboxState::Failed {
                consecutive_failures,
            } => *consecutive_failures,
        };

        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            self.state = SandboxState::Quarantined;
            eprintln!(
                "[tower] plugin '{}' quarantined after {consecutive_failures} restart failures",
                self.manifest.name
            );
            return Err(PluginHostError::PluginFault(PluginFaultKind::Quarantined));
        }

        match WasmtimeHost::load_with_engine(
            &self.engine,
            &self.wasm_path,
            Arc::clone(&self.fs_port),
        ) {
            Ok(new_instance) => {
                eprintln!(
                    "[tower] plugin '{}' sandbox recreated successfully",
                    self.manifest.name
                );
                self.state = SandboxState::Ready(new_instance);
                Ok(())
            }
            Err(load_err) => {
                let new_failures = consecutive_failures + 1;
                eprintln!(
                    "[tower] plugin '{}' restart {new_failures}/{MAX_CONSECUTIVE_FAILURES} failed: {load_err}",
                    self.manifest.name
                );
                if new_failures >= MAX_CONSECUTIVE_FAILURES {
                    self.state = SandboxState::Quarantined;
                    eprintln!("[tower] plugin '{}' quarantined", self.manifest.name);
                    Err(PluginHostError::PluginFault(PluginFaultKind::Quarantined))
                } else {
                    self.state = SandboxState::Failed {
                        consecutive_failures: new_failures,
                    };
                    Err(PluginHostError::PluginFault(PluginFaultKind::Trapped(
                        load_err.to_string(),
                    )))
                }
            }
        }
    }

    // ── Guarded call ──────────────────────────────────────────────────────────

    /// Ensure the sandbox is `Ready`, apply compute limits, and invoke `f`.
    ///
    /// On any error from the guest, marks the sandbox `Failed` and returns a
    /// `PluginFault` error so the caller can react without killing the host.
    fn guarded_call<T>(
        &mut self,
        f: impl FnOnce(&mut WasmInstance) -> Result<T, PluginHostError>,
    ) -> Result<T, PluginHostError> {
        // Ensure we are in the Ready state.
        match &self.state {
            SandboxState::Quarantined => {
                return Err(PluginHostError::PluginFault(PluginFaultKind::Quarantined))
            }
            SandboxState::Failed { .. } => {
                self.try_recreate()?;
                // try_recreate either transitions to Ready or returns Err.
            }
            SandboxState::Ready(_) => {}
        }

        let SandboxState::Ready(instance) = &mut self.state else {
            // Unreachable: try_recreate returns Err if state is not Ready.
            return Err(PluginHostError::PluginFault(PluginFaultKind::Quarantined));
        };

        // Apply fuel and epoch limits before the call.
        instance.apply_compute_bounds(&self.config);

        // Invoke the guest.
        match f(instance) {
            Ok(value) => Ok(value),
            Err(err) => {
                let fault = classify_error(&err);
                eprintln!("[tower] plugin '{}' fault: {fault}", self.manifest.name);
                // Reset failure counter: this is the first failure in a new run.
                self.state = SandboxState::Failed {
                    consecutive_failures: 0,
                };
                Err(PluginHostError::PluginFault(fault))
            }
        }
    }
}

// ── Error classification ──────────────────────────────────────────────────────

/// Classify a [`PluginHostError`] from the guest into a [`PluginFaultKind`].
fn classify_error(err: &PluginHostError) -> PluginFaultKind {
    match err {
        PluginHostError::CallFailed(msg) | PluginHostError::HookDeliveryFailed(msg) => {
            classify_trap_message(msg)
        }
        PluginHostError::PluginFault(kind) => kind.clone(),
        PluginHostError::ToolNotFound(_) | PluginHostError::InvalidArgs(_) => {
            // These are guest-side logic errors, not traps. Map to Trapped so
            // the caller always gets a PluginFault when guarded_call wraps them.
            PluginFaultKind::Trapped("unexpected non-fault error from guest".to_owned())
        }
    }
}

/// Inspect a trap/error message string to identify the fault kind.
///
/// wasmtime 45.x observed message fragments:
/// - `"wasm trap: out of fuel"` — fuel exhaustion.
/// - `"wasm trap: interrupt"` — epoch deadline.
/// - `"epoch deadline exceeded"` — epoch (alternative phrasing).
/// - `"wasm trap: unreachable executed"` — guest panic / explicit unreachable.
pub(crate) fn classify_trap_message(msg: &str) -> PluginFaultKind {
    let lower = msg.to_lowercase();
    if lower.contains("out of fuel") || (lower.contains("fuel") && lower.contains("trap")) {
        PluginFaultKind::FuelExhausted
    } else if lower.contains("epoch") || lower.contains("interrupt") {
        PluginFaultKind::EpochDeadlineExceeded
    } else {
        PluginFaultKind::Trapped(msg.to_owned())
    }
}

// ── PluginInstance impl ────────────────────────────────────────────────────────

impl PluginInstance for IsolatedSandbox {
    fn manifest(&self) -> &PluginManifest {
        // Always return the cached manifest, even when Failed or Quarantined.
        &self.manifest
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Result<Value, PluginHostError> {
        let name = name.to_owned();
        self.guarded_call(move |instance| instance.call_tool(&name, args))
    }

    fn deliver_hook(
        &mut self,
        kind: HookKind,
        payload: HookPayload,
    ) -> Result<(), PluginHostError> {
        self.guarded_call(move |instance| instance.deliver_hook(kind, payload))
    }
}

// ── Tests (unit) ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_trap_message ─────────────────────────────────────────────────

    #[test]
    fn classifies_fuel_exhaustion() {
        assert_eq!(
            classify_trap_message("wasm trap: out of fuel"),
            PluginFaultKind::FuelExhausted
        );
    }

    #[test]
    fn classifies_epoch_interrupt() {
        assert_eq!(
            classify_trap_message("wasm trap: interrupt"),
            PluginFaultKind::EpochDeadlineExceeded
        );
    }

    #[test]
    fn classifies_epoch_deadline() {
        assert_eq!(
            classify_trap_message("epoch deadline exceeded"),
            PluginFaultKind::EpochDeadlineExceeded
        );
    }

    #[test]
    fn classifies_unreachable_as_trapped() {
        let kind = classify_trap_message("wasm trap: unreachable executed");
        assert!(matches!(kind, PluginFaultKind::Trapped(_)));
    }

    #[test]
    fn classifies_generic_as_trapped() {
        let kind = classify_trap_message("something unexpected");
        assert!(matches!(kind, PluginFaultKind::Trapped(_)));
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn max_consecutive_failures_constant() {
        // Verify the quarantine threshold is set to the expected policy value.
        // Use a runtime comparison to avoid the constant-expression lint.
        let threshold: u32 = MAX_CONSECUTIVE_FAILURES;
        assert_eq!(threshold, 3, "quarantine threshold must be 3");
    }

    #[test]
    fn default_fuel_budget_is_large() {
        let budget: u64 = DEFAULT_FUEL_BUDGET;
        assert!(
            budget >= 1_000_000,
            "default fuel budget must be at least 1M"
        );
    }

    // ── IsolationEngine ───────────────────────────────────────────────────────

    #[test]
    fn isolation_engine_new_succeeds() {
        assert!(IsolationEngine::new().is_ok());
    }

    // ── PluginFaultKind display ───────────────────────────────────────────────

    #[test]
    fn fault_kind_display_messages() {
        assert!(PluginFaultKind::FuelExhausted.to_string().contains("fuel"));
        assert!(PluginFaultKind::EpochDeadlineExceeded
            .to_string()
            .contains("epoch"));
        assert!(PluginFaultKind::Quarantined
            .to_string()
            .contains("quarantined"));
        assert!(PluginFaultKind::Trapped("boom".to_owned())
            .to_string()
            .contains("boom"));
    }
}
