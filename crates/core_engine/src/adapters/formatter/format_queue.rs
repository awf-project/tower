//! Bounded, per-path-coalescing format queue (spec 13a).
//!
//! # Design
//!
//! The queue is keyed per path:
//! - `InFlight`: a job is currently running for this path.
//! - `InFlightDirty`: a job is running AND a new request arrived — one trailing
//!   job will be scheduled on completion.
//! - No entry: path is idle.
//!
//! Queue capacity is `QUEUE_CAPACITY`. Enqueue beyond capacity returns `Dropped`.
//! Worker pool size is capped at `MAX_WORKERS`.
//!
//! # Echo suppression
//!
//! After a successful atomic write the worker inserts the path into `echo_set`
//! (a `HashMap<String, ()>`) before the rename completes. The broadcaster in
//! `PluginHostRegistry::fan_out` checks this set for every `FileChanged` event
//! headed to the `fmt` plugin and consumes the entry if present, suppressing
//! the delivery only to `fmt` (other subscribers still receive it).
//!
//! # Failure backoff
//!
//! Per-path failure counters are tracked. After `MAX_CONSECUTIVE_FAILURES`
//! consecutive failures for a path, that path is silently dropped for
//! `BACKOFF_DURATION`. This prevents tight retry loops on persistently broken files.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::worker::FormatWorker;
use crate::adapters::config::FormatterConfig;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum number of paths that can be queued concurrently.
/// Requests beyond this are dropped (best-effort, non-blocking).
pub const QUEUE_CAPACITY: usize = 256;

/// Maximum number of concurrent worker threads.
pub const MAX_WORKERS: usize = 4;

/// Consecutive failures before a path is backed off.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// How long a path stays in backoff after exceeding `MAX_CONSECUTIVE_FAILURES`.
pub const BACKOFF_DURATION: Duration = Duration::from_secs(30);

// ── FormatQueuePort ───────────────────────────────────────────────────────────

/// Port trait for the format queue, used in `HostDeps`.
///
/// A `NoOpFormatQueue` sentinel fulfils this trait without starting any threads,
/// so existing tests that construct `HostDeps` compile without change.
pub trait FormatQueuePort: Send + Sync {
    /// Enqueue a format job for `path`.
    ///
    /// Returns `Accepted` if the job was queued (or coalesced into an in-flight
    /// job) and `Dropped` if the queue is full or the path is in backoff.
    /// Never blocks the caller.
    fn enqueue(&self, path: String) -> EnqueueResult;

    /// The echo-suppression set shared with the broadcaster, if this queue
    /// performs real formatting. `None` for no-op queues (no suppression).
    ///
    /// `PluginHostRegistry` uses this to suppress `FileChanged` re-delivery to
    /// `plugin_fmt` for paths the worker just wrote (loop break).
    fn shared_echo_set(&self) -> Option<Arc<Mutex<HashMap<String, ()>>>> {
        None
    }
}

/// Result of a format-queue enqueue operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    /// The job was accepted (queued or coalesced).
    Accepted,
    /// The queue is full or the path is in backoff. The job is dropped.
    Dropped,
}

// ── NoOpFormatQueue ───────────────────────────────────────────────────────────

/// A no-op sentinel that accepts every request and does nothing.
///
/// Used as the default `HostDeps.format_queue` so existing tests and the
/// production wiring before `FormatterConfig` is supplied compile unchanged.
pub struct NoOpFormatQueue;

impl FormatQueuePort for NoOpFormatQueue {
    fn enqueue(&self, _path: String) -> EnqueueResult {
        // Accept silently — no formatting will occur.
        EnqueueResult::Accepted
    }
}

// ── InFlightState ─────────────────────────────────────────────────────────────

/// State of a single path in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InFlightState {
    /// A job is running; no trailing job needed.
    InFlight,
    /// A job is running AND a subsequent request arrived.
    /// On completion, one trailing job will be scheduled.
    InFlightDirty,
}

// ── FailureRecord ─────────────────────────────────────────────────────────────

/// Failure tracking for a single path.
#[derive(Debug)]
struct FailureRecord {
    consecutive: u32,
    backoff_until: Option<Instant>,
}

impl FailureRecord {
    fn new() -> Self {
        Self {
            consecutive: 0,
            backoff_until: None,
        }
    }

    fn is_backed_off(&self) -> bool {
        self.backoff_until
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    fn record_failure(&mut self) {
        self.consecutive += 1;
        if self.consecutive >= MAX_CONSECUTIVE_FAILURES {
            self.backoff_until = Some(Instant::now() + BACKOFF_DURATION);
        }
    }

    fn record_success(&mut self) {
        self.consecutive = 0;
        self.backoff_until = None;
    }
}

// ── QueueState ────────────────────────────────────────────────────────────────

struct QueueState {
    /// Pending paths waiting for a worker slot (bounded at QUEUE_CAPACITY).
    pending: VecDeque<String>,
    /// Paths that currently have an in-flight or in-flight-dirty job.
    in_flight: HashMap<String, InFlightState>,
    /// Active worker count.
    active_workers: usize,
    /// Per-path failure records.
    failures: HashMap<String, FailureRecord>,
}

impl QueueState {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            in_flight: HashMap::new(),
            active_workers: 0,
            failures: HashMap::new(),
        }
    }

    fn is_backed_off(&self, path: &str) -> bool {
        self.failures
            .get(path)
            .map(|r| r.is_backed_off())
            .unwrap_or(false)
    }
}

// ── FormatQueue ───────────────────────────────────────────────────────────────

/// Bounded, per-path-coalescing format queue with a worker thread pool.
///
/// Construct with `FormatQueue::new(config, workspace_root)`. Worker threads
/// are started lazily on first job dispatch. On `Drop`, no further jobs are
/// dispatched; in-flight jobs run to completion on their threads (which exit
/// naturally).
///
/// # Path convention
///
/// The queue accepts **workspace-relative** paths from `host_request_format`.
/// The workspace root is used internally to resolve them to absolute paths
/// before handing them to `FormatWorker` (which needs absolute paths for
/// `std::fs` operations). The echo-suppression set key stays as the
/// workspace-relative path, consistent with what the watcher broadcasts and
/// `PluginHostRegistry::fan_out` checks.
pub struct FormatQueue {
    state: Arc<Mutex<QueueState>>,
    /// Shared echo-suppression set: workspace-relative path → () (presence only).
    ///
    /// Inserted by workers before rename; consumed by the broadcaster in
    /// `PluginHostRegistry::fan_out` before delivering `FileChanged` to `fmt`.
    /// Keys are workspace-relative to match the paths `fan_out` receives from
    /// the watcher's `on_file_changed` broadcast.
    echo_set: Arc<Mutex<HashMap<String, ()>>>,
    /// Formatter configuration (tool definitions by file extension).
    config: Arc<FormatterConfig>,
    /// Absolute path to the workspace root.
    ///
    /// Workers resolve relative paths against this root before performing
    /// any filesystem operations (read, atomic write, CAS re-read).
    workspace_root: Arc<PathBuf>,
}

impl FormatQueue {
    /// Create a new `FormatQueue` with the given formatter configuration and
    /// workspace root.
    ///
    /// `workspace_root` must be an absolute path to the workspace directory.
    /// The queue accepts workspace-relative paths from `host_request_format`;
    /// workers resolve them to absolute paths using this root.
    ///
    /// No threads are started until the first job is enqueued.
    #[must_use]
    pub fn new(config: FormatterConfig, workspace_root: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(QueueState::new())),
            echo_set: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            workspace_root: Arc::new(workspace_root),
        }
    }

    /// Return a clone of the echo-suppression set Arc.
    ///
    /// The broadcaster in `PluginHostRegistry` uses this to suppress `FileChanged`
    /// delivery to the `fmt` plugin for paths that were just formatted.
    #[must_use]
    pub fn echo_set(&self) -> Arc<Mutex<HashMap<String, ()>>> {
        Arc::clone(&self.echo_set)
    }

    /// Dispatch the next pending job to a worker thread, if capacity allows.
    ///
    /// Called after enqueueing a new path or after a worker completes.
    fn maybe_dispatch(&self, state: &mut QueueState) {
        while state.active_workers < MAX_WORKERS {
            let path = match state.pending.pop_front() {
                Some(p) => p,
                None => break,
            };
            // Mark as in-flight (may have been dirty from a coalesced request;
            // reset to plain InFlight since we're starting fresh for latest content).
            state
                .in_flight
                .insert(path.clone(), InFlightState::InFlight);
            state.active_workers += 1;

            let state_arc = Arc::clone(&self.state);
            let echo_set = Arc::clone(&self.echo_set);
            let config = Arc::clone(&self.config);
            let workspace_root = Arc::clone(&self.workspace_root);
            let path_clone = path.clone();

            std::thread::Builder::new()
                .name(format!("tower-fmt-{path_clone}"))
                .spawn(move || {
                    let worker = FormatWorker {
                        echo_set: Arc::clone(&echo_set),
                        config: Arc::clone(&config),
                        workspace_root: Arc::clone(&workspace_root),
                    };
                    let succeeded = worker.run(&path_clone);

                    // On completion: update state, handle dirty, dispatch next.
                    let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());

                    // Update failure tracking.
                    if succeeded {
                        if let Some(rec) = st.failures.get_mut(&path_clone) {
                            rec.record_success();
                        }
                    } else {
                        st.failures
                            .entry(path_clone.clone())
                            .or_insert_with(FailureRecord::new)
                            .record_failure();
                    }

                    // Check dirty: if in-flight-dirty, schedule a trailing job.
                    let was_dirty =
                        st.in_flight.get(&path_clone) == Some(&InFlightState::InFlightDirty);
                    st.in_flight.remove(&path_clone);
                    st.active_workers = st.active_workers.saturating_sub(1);

                    if was_dirty && !st.is_backed_off(&path_clone) {
                        // Re-enqueue a single trailing job only if not backed off.
                        if st.pending.len() < QUEUE_CAPACITY
                            && !st.in_flight.contains_key(&path_clone)
                        {
                            st.pending.push_back(path_clone.clone());
                            // Will be dispatched in the loop below.
                        }
                    }

                    // Dispatch pending jobs now that a worker slot freed up.
                    // We cannot call self.maybe_dispatch here (no &self), so
                    // we replicate the dispatch loop inline.
                    while st.active_workers < MAX_WORKERS {
                        let next_path = match st.pending.pop_front() {
                            Some(p) => p,
                            None => break,
                        };
                        st.in_flight
                            .insert(next_path.clone(), InFlightState::InFlight);
                        st.active_workers += 1;

                        let state_arc2 = Arc::clone(&state_arc);
                        let echo_set2 = Arc::clone(&echo_set);
                        let config2 = Arc::clone(&config);
                        let workspace_root2 = Arc::clone(&workspace_root);
                        let next_path_clone = next_path.clone();

                        // Spawn next worker; same pattern as above (recursive).
                        // Depth is bounded by active_workers < MAX_WORKERS.
                        spawn_worker(
                            state_arc2,
                            echo_set2,
                            config2,
                            workspace_root2,
                            next_path_clone,
                        );
                    }
                })
                .expect("failed to spawn format worker thread");
        }
    }
}

/// Spawn a format worker thread. Extracted to avoid infinite recursion in
/// the inline dispatch loop (the closure captures locals, not `self`).
fn spawn_worker(
    state_arc: Arc<Mutex<QueueState>>,
    echo_set: Arc<Mutex<HashMap<String, ()>>>,
    config: Arc<FormatterConfig>,
    workspace_root: Arc<PathBuf>,
    path: String,
) {
    let state_arc2 = Arc::clone(&state_arc);
    let echo_set2 = Arc::clone(&echo_set);
    let config2 = Arc::clone(&config);
    let workspace_root2 = Arc::clone(&workspace_root);
    let path2 = path.clone();

    std::thread::Builder::new()
        .name(format!("tower-fmt-{path}"))
        .spawn(move || {
            let worker = FormatWorker {
                echo_set: echo_set2,
                config: config2,
                workspace_root: workspace_root2,
            };
            let succeeded = worker.run(&path2);

            let mut st = state_arc2.lock().unwrap_or_else(|p| p.into_inner());

            if succeeded {
                if let Some(rec) = st.failures.get_mut(&path2) {
                    rec.record_success();
                }
            } else {
                st.failures
                    .entry(path2.clone())
                    .or_insert_with(FailureRecord::new)
                    .record_failure();
            }

            let was_dirty = st.in_flight.get(&path2) == Some(&InFlightState::InFlightDirty);
            st.in_flight.remove(&path2);
            st.active_workers = st.active_workers.saturating_sub(1);

            if was_dirty
                && !st.is_backed_off(&path2)
                && st.pending.len() < QUEUE_CAPACITY
                && !st.in_flight.contains_key(&path2)
            {
                st.pending.push_back(path2.clone());
            }

            while st.active_workers < MAX_WORKERS {
                let next_path = match st.pending.pop_front() {
                    Some(p) => p,
                    None => break,
                };
                st.in_flight
                    .insert(next_path.clone(), InFlightState::InFlight);
                st.active_workers += 1;
                spawn_worker(
                    Arc::clone(&state_arc2),
                    Arc::clone(&echo_set),
                    Arc::clone(&config),
                    Arc::clone(&workspace_root),
                    next_path,
                );
            }
        })
        .expect("failed to spawn format worker thread");
}

impl FormatQueuePort for FormatQueue {
    fn shared_echo_set(&self) -> Option<Arc<Mutex<HashMap<String, ()>>>> {
        Some(Arc::clone(&self.echo_set))
    }

    fn enqueue(&self, path: String) -> EnqueueResult {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());

        // Check backoff first.
        if st.is_backed_off(&path) {
            return EnqueueResult::Dropped;
        }

        // Coalescing: if already in-flight, mark dirty and return Accepted.
        if let Some(state) = st.in_flight.get_mut(&path) {
            *state = InFlightState::InFlightDirty;
            return EnqueueResult::Accepted;
        }

        // Already in the pending queue (not yet dispatched).
        if st.pending.contains(&path) {
            // Idempotent — already queued, nothing to do.
            return EnqueueResult::Accepted;
        }

        // Queue capacity check.
        if st.pending.len() >= QUEUE_CAPACITY {
            eprintln!("[tower/fmt] queue full, dropping job for: {path}");
            return EnqueueResult::Dropped;
        }

        // Push to pending and try to dispatch immediately.
        st.pending.push_back(path);
        drop(st); // release lock before spawning
        let mut st2 = self.state.lock().unwrap_or_else(|p| p.into_inner());
        self.maybe_dispatch(&mut st2);

        EnqueueResult::Accepted
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // ── RED-1 → GREEN-1: NoOpFormatQueue accepts without formatting ────────────

    /// GREEN-1: A NoOpFormatQueue accepts every request and never formats.
    /// This test covers the basic `FormatQueuePort` contract without any worker.
    #[test]
    fn noop_queue_accepts_without_formatting() {
        let queue = NoOpFormatQueue;
        let result = queue.enqueue("src/main.rs".to_owned());
        assert_eq!(result, EnqueueResult::Accepted);
    }

    /// GREEN-1 variant: FormatQueue with empty config accepts and returns Accepted.
    #[test]
    fn format_queue_empty_config_accepts_without_crashing() {
        let queue = FormatQueue::new(FormatterConfig::default(), PathBuf::new());
        let result = queue.enqueue("src/main.rs".to_owned());
        // With no tools configured the worker runs and returns immediately (no-op).
        assert_eq!(result, EnqueueResult::Accepted);
    }

    // ── RED-6 → GREEN-6: Overflow drops and non-blocking ─────────────────────

    /// GREEN-6 / AC3: Enqueuing beyond QUEUE_CAPACITY drops overflow without blocking.
    ///
    /// We use a NoOpFormatQueue sentinel to avoid spawning real workers.
    /// The drop behavior is tested on the real FormatQueue with a full pending queue.
    #[test]
    fn overflow_returns_dropped_without_blocking() {
        use crate::adapters::config::FormatterConfig;

        let queue = FormatQueue::new(FormatterConfig::default(), PathBuf::new());

        // Fill the pending queue directly by locking state.
        {
            let mut st = queue.state.lock().unwrap();
            // Fill all but the active-worker slots. We set active_workers to
            // MAX_WORKERS so no dispatching happens, then fill the pending queue.
            st.active_workers = MAX_WORKERS;
            for i in 0..QUEUE_CAPACITY {
                st.pending.push_back(format!("path_{i}.rs"));
            }
        }

        // Now enqueue one more — should return Dropped immediately.
        let start = std::time::Instant::now();
        let result = queue.enqueue("overflow.rs".to_owned());
        let elapsed = start.elapsed();

        assert_eq!(result, EnqueueResult::Dropped, "overflow must be Dropped");
        assert!(
            elapsed < Duration::from_millis(100),
            "enqueue must not block: elapsed={elapsed:?}"
        );
    }

    // ── AC9: Coalescing / single-flight ────────────────────────────────────────

    /// GREEN-6 / AC9: A second enqueue for an in-flight path sets InFlightDirty,
    /// returning Accepted without creating a second queue slot.
    #[test]
    fn second_enqueue_for_in_flight_path_coalesces() {
        let queue = FormatQueue::new(FormatterConfig::default(), PathBuf::new());

        // Manually mark a path as InFlight (simulate a running worker).
        {
            let mut st = queue.state.lock().unwrap();
            st.in_flight
                .insert("src/lib.rs".to_owned(), InFlightState::InFlight);
            st.active_workers = 1;
        }

        // Second request for the same path should coalesce.
        let result = queue.enqueue("src/lib.rs".to_owned());
        assert_eq!(
            result,
            EnqueueResult::Accepted,
            "coalesced request is Accepted"
        );

        // Verify it was marked dirty.
        let st = queue.state.lock().unwrap();
        assert_eq!(
            st.in_flight.get("src/lib.rs"),
            Some(&InFlightState::InFlightDirty),
            "path must be marked InFlightDirty after second enqueue"
        );
        // Pending queue must be empty (no duplicate slot created).
        assert!(
            !st.pending.contains(&"src/lib.rs".to_owned()),
            "coalesced path must not appear in pending queue"
        );
    }

    // ── AC6: Backoff after repeated failures ──────────────────────────────────

    /// GREEN-7 / AC6: A path with MAX_CONSECUTIVE_FAILURES is backed off.
    #[test]
    fn path_backed_off_after_repeated_failures_returns_dropped() {
        let queue = FormatQueue::new(FormatterConfig::default(), PathBuf::new());

        // Manually set up a failure record at the limit.
        {
            let mut st = queue.state.lock().unwrap();
            let rec = st
                .failures
                .entry("broken.rs".to_owned())
                .or_insert_with(FailureRecord::new);
            // Simulate MAX_CONSECUTIVE_FAILURES failures.
            for _ in 0..MAX_CONSECUTIVE_FAILURES {
                rec.record_failure();
            }
        }

        // Enqueue should now be Dropped (backed off).
        let result = queue.enqueue("broken.rs".to_owned());
        assert_eq!(
            result,
            EnqueueResult::Dropped,
            "backed-off path must return Dropped"
        );
    }
}
