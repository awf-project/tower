//! `NotifyWatcherAdapter` — OS filesystem event producer (spec 06).
//!
//! # Architecture
//!
//! ```text
//!  OS FS events
//!    │
//!    ▼  notify::recommended_watcher (crossbeam-channel backend)
//!  raw notify::Event stream
//!    │
//!    ▼  raw_to_watch_events (normalise to WatchEvent)
//!    │
//!    ▼  debounce_events (coalesce bursts)
//!    │
//!    ▼  Sender<WatchEvent> ──channel──► worker thread
//!                                             │
//!                                   EventProcessor::process_event
//! ```
//!
//! # Lifecycle
//!
//! `NotifyWatcherAdapter::new` starts a worker thread that reads from the
//! channel and calls `EventProcessor::process_event`. The thread runs until
//! the `Sender` half is dropped (when `NotifyWatcherAdapter` is dropped),
//! at which point the `Receiver` iterator ends and the thread exits cleanly.
//!
//! `Drop` joins the worker thread so no threads are leaked after the adapter
//! is discarded.
//!
//! # Debouncing
//!
//! Raw notify events are accumulated in the notify callback and forwarded as
//! batches (notify already aggregates a burst into one callback invocation
//! with `recommended_watcher`). Each batch is passed through `debounce_events`
//! before being sent on the channel so the processor thread never sees
//! redundant events for the same path.
//!
//! # Error handling
//!
//! Errors on the processor thread are logged to stderr rather than panicking
//! (a poisoned lock is treated as unrecoverable and the thread exits, which
//! causes the next `Sender::send` to return an error that is also logged).
//! This ensures the watcher thread never brings down the host process.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::debounce::debounce_events;
use super::error::WatcherError;
use super::event_processor::{EventProcessor, WatchEvent};
use crate::domain::index::InvertedIndex;
use crate::domain::workspace::ProjectWorkspace;
use crate::ports::{PluginHostPort, StoragePort};

/// Capacity of the internal event channel.
///
/// A small buffer is sufficient because the worker thread drains it
/// continuously. A larger buffer would increase memory usage without benefit.
const CHANNEL_CAPACITY: usize = 256;

/// OS-backed filesystem watcher adapter (spec 06).
///
/// Subscribes to native filesystem events under a workspace root via the
/// `notify` crate, debounces burst events, and drives an [`EventProcessor`]
/// to keep the workspace + index + storage in sync.
///
/// # Drop behaviour
///
/// Dropping `NotifyWatcherAdapter` sends a shutdown signal (by dropping the
/// `Sender` half of the event channel) and joins the worker thread.  This
/// guarantees no leaked threads and no dangling cross-thread references.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::{Arc, RwLock};
/// use core_engine::adapters::watcher::NotifyWatcherAdapter;
/// use core_engine::adapters::in_memory_storage::InMemoryStorage;
/// use core_engine::domain::index::InvertedIndex;
/// use core_engine::domain::workspace::ProjectWorkspace;
/// use core_engine::ports::NoOpPluginHost;
///
/// let root = std::env::temp_dir().join("my-workspace");
/// std::fs::create_dir_all(&root).unwrap();
///
/// let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
/// let index = Arc::new(RwLock::new(InvertedIndex::new()));
/// let storage = Box::new(InMemoryStorage::new());
///
/// let _watcher = NotifyWatcherAdapter::new(
///     root,
///     Arc::clone(&workspace),
///     Arc::clone(&index),
///     storage,
///     Box::new(NoOpPluginHost),
/// )
/// .expect("watcher must start");
/// // Watcher is now running; drop it to shut down cleanly.
/// ```
pub struct NotifyWatcherAdapter {
    /// The `notify` watcher; wrapped in `Option` so `Drop` can explicitly drop
    /// it before `sender` to deregister the OS watch before closing the channel.
    watcher: Option<RecommendedWatcher>,
    /// Sender half of the event channel; wrapped in `Option` so `Drop` can
    /// explicitly drop it (closing the channel) before joining the worker thread.
    sender: Option<Sender<WatchEvent>>,
    /// Worker thread handle; joined in `Drop` after the channel is closed.
    worker: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for NotifyWatcherAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyWatcherAdapter")
            .field("running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl NotifyWatcherAdapter {
    /// Create a new watcher that subscribes to OS events under `root`.
    ///
    /// # Errors
    ///
    /// - [`WatcherError::NotifyInit`] — the notify crate could not create the watcher.
    /// - [`WatcherError::NotifyWatch`] — the watch path could not be registered.
    pub fn new(
        root: PathBuf,
        workspace: Arc<RwLock<ProjectWorkspace>>,
        index: Arc<RwLock<InvertedIndex>>,
        storage: Box<dyn StoragePort + Send>,
        plugin_host: Box<dyn PluginHostPort + Send + Sync>,
    ) -> Result<Self, WatcherError> {
        let (tx, rx): (Sender<WatchEvent>, Receiver<WatchEvent>) = bounded(CHANNEL_CAPACITY);

        // Clone root for the notify callback closure.
        let root_for_callback = root.clone();
        let tx_for_callback = tx.clone();

        // Build the notify watcher.
        //
        // Decision: use `recommended_watcher` which selects the best backend
        // for the current platform (inotify on Linux, kqueue on macOS,
        // ReadDirectoryChangesW on Windows).
        //
        // The callback receives a batch of events; we normalise them to
        // `WatchEvent` values and debounce them before sending on the channel.
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            match result {
                Ok(event) => {
                    let raw_events: Vec<WatchEvent> =
                        raw_to_watch_events(event, &root_for_callback);
                    let coalesced = debounce_events(raw_events);
                    for watch_event in coalesced {
                        // If the channel is full, drop the event rather than
                        // blocking the notify callback thread (which could
                        // deadlock the OS event queue).
                        if tx_for_callback.try_send(watch_event).is_err() {
                            // Channel full or closed — log and continue.
                            eprintln!(
                                "[tower/watcher] event channel full or closed; dropping event"
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[tower/watcher] notify error: {e}");
                }
            }
        })
        .map_err(WatcherError::NotifyInit)?;

        // Register the root directory for recursive watching.
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(WatcherError::NotifyWatch)?;

        // Spawn the worker thread that drains the channel.
        let mut processor = EventProcessor::new(root, workspace, index, storage, plugin_host);

        // Decision: wrap each per-event invocation in `catch_unwind` so that a
        // panicking `PluginHostPort` or `StoragePort` implementation does not
        // kill the worker thread permanently. Without this, a panic inside
        // `process_event` (e.g. from a user-supplied plugin host) terminates
        // the worker thread; subsequent OS events fill the bounded channel (256
        // slots) and are then silently dropped, causing the workspace to diverge
        // from disk indefinitely.
        //
        // Trade-off: `AssertUnwindSafe` is required because `EventProcessor`
        // holds `Box<dyn ...>` trait objects whose unwind safety we cannot
        // statically verify. We accept this because:
        //   a) the domain state (workspace + index) is guarded by `RwLock`s that
        //      are not held across the `plugin_host` call, so a panic there does
        //      not leave the domain in an inconsistent state;
        //   b) a poisoned lock (from a panic *inside* a lock guard) is treated as
        //      unrecoverable by the `.expect()` calls in `process_event`, which
        //      will themselves panic — those panics are also caught here.
        //
        // A caught panic is logged; the worker then moves on to the next event.
        let worker = thread::Builder::new()
            .name("tower-watcher".to_owned())
            .spawn(move || {
                // `rx.into_iter()` blocks until the Sender is dropped.
                for event in rx.into_iter() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        processor.process_event(event)
                    }));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            eprintln!("[tower/watcher] process_event error: {e}");
                        }
                        Err(payload) => {
                            let msg = payload
                                .downcast_ref::<&str>()
                                .copied()
                                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                                .unwrap_or("(unknown panic payload)");
                            eprintln!(
                                "[tower/watcher] process_event panicked (event dropped): {msg}"
                            );
                        }
                    }
                }
                // Channel closed — clean exit.
            })
            .map_err(|e| WatcherError::SpawnFailed(e.to_string()))?;

        Ok(Self {
            watcher: Some(watcher),
            sender: Some(tx),
            worker: Some(worker),
        })
    }
}

impl Drop for NotifyWatcherAdapter {
    /// Shut down the watcher cleanly on drop.
    ///
    /// Shutdown order (critical for correctness):
    ///
    /// 1. Drop `watcher` — deregisters the OS watch, preventing new raw events
    ///    from arriving.
    /// 2. Drop `sender` — closes the channel, causing the worker thread's
    ///    `rx.into_iter()` to return `None` and exit its loop.
    /// 3. Join `worker` — waits for the thread to flush all pending events and
    ///    exit cleanly.
    ///
    /// Note: Rust auto-drops struct fields in declaration order *after* `drop()`
    /// returns.  We must explicitly take `watcher` and `sender` here so the
    /// channel is closed *before* we call `join()`.  Without this, `join()` would
    /// block forever: the worker would be waiting for the channel to close, but
    /// the channel would only close after `drop()` returned.
    fn drop(&mut self) {
        // Step 1: deregister the OS watch so no new raw events arrive.
        drop(self.watcher.take());

        // Step 2: close the channel — this unblocks the worker thread's iterator.
        drop(self.sender.take());

        // Step 3: join the worker thread so callers can rely on a quiesced state.
        if let Some(handle) = self.worker.take() {
            // If the thread panicked, log rather than panicking here (panicking
            // inside Drop is UB-adjacent; it aborts the process).
            if let Err(e) = handle.join() {
                eprintln!("[tower/watcher] worker thread panicked: {e:?}");
            }
        }
    }
}

// ── Raw event normalisation ───────────────────────────────────────────────────

/// Convert a single raw [`notify::Event`] into zero or more [`WatchEvent`]s.
///
/// Only file-related events are mapped; directory events and unknown kinds
/// are silently dropped.
///
/// Events whose paths are outside `root` are not filtered here — that is the
/// responsibility of `EventProcessor::to_relative` (UN1/AC5).
fn raw_to_watch_events(event: Event, _root: &PathBuf) -> Vec<WatchEvent> {
    let paths = event.paths;

    match event.kind {
        EventKind::Create(_) => paths.into_iter().map(WatchEvent::Create).collect(),

        EventKind::Modify(_) => paths.into_iter().map(WatchEvent::Modify).collect(),

        EventKind::Remove(_) => paths.into_iter().map(WatchEvent::Delete).collect(),

        EventKind::Access(_) => {
            // Access events (open, close) carry no semantic change — ignore.
            Vec::new()
        }

        EventKind::Other => Vec::new(),
        EventKind::Any => {
            // Unknown event kind: treat as Modify to be conservative.
            paths.into_iter().map(WatchEvent::Modify).collect()
        }
    }
}

// ── Smoke test for the real OS watcher ────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use super::*;
    use crate::adapters::in_memory_storage::InMemoryStorage;
    use crate::domain::index::FileSearch;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::ports::NoOpPluginHost;
    use crate::ports::inbound::SearchUseCase as _;

    /// Smoke test: the real OS watcher picks up a file creation under a tempdir.
    ///
    /// This test uses a real notify subscription and a short sleep to let the OS
    /// deliver the event. It is intentionally thin — correctness is proven by the
    /// injectable EventProcessor tests in `event_processor.rs`.
    ///
    /// The test is marked `#[ignore]` by default so it does not run in CI
    /// environments where OS event delivery timing is unreliable.
    /// Run it explicitly with `cargo test -- --ignored` to verify on your machine.
    #[test]
    #[ignore = "OS timing: run with -- --ignored to verify on local machine"]
    fn smoke_real_fs_watcher_detects_create() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        let storage = Box::new(InMemoryStorage::new());

        let _watcher = NotifyWatcherAdapter::new(
            root.clone(),
            Arc::clone(&workspace),
            Arc::clone(&index),
            storage,
            Box::new(NoOpPluginHost),
        )
        .expect("watcher must start");

        // Write a file — this triggers the OS inotify/kqueue/etc. event.
        let file_path = root.join("smoke_test.rs");
        std::fs::write(&file_path, b"// smoke").unwrap();

        // Allow OS event delivery + processing.
        std::thread::sleep(Duration::from_millis(200));

        let ws_guard = workspace.read().unwrap();
        let idx_guard = index.read().unwrap();
        let results = FileSearch::new(&idx_guard, &ws_guard)
            .find_file("smoke")
            .unwrap();

        assert!(
            !results.is_empty(),
            "smoke_test.rs must be searchable after real FS write; got {results:?}"
        );
    }

    /// `raw_to_watch_events` normalises notify Create events correctly.
    #[test]
    fn raw_to_watch_events_create() {
        use notify::event::{CreateKind, EventAttributes};
        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/root/a.rs")],
            attrs: EventAttributes::default(),
        };
        let result = raw_to_watch_events(event, &PathBuf::from("/root"));
        assert_eq!(
            result,
            vec![WatchEvent::Create(PathBuf::from("/root/a.rs"))]
        );
    }

    /// `raw_to_watch_events` normalises notify Remove events correctly.
    #[test]
    fn raw_to_watch_events_remove() {
        use notify::event::{EventAttributes, RemoveKind};
        let event = Event {
            kind: EventKind::Remove(RemoveKind::File),
            paths: vec![PathBuf::from("/root/b.rs")],
            attrs: EventAttributes::default(),
        };
        let result = raw_to_watch_events(event, &PathBuf::from("/root"));
        assert_eq!(
            result,
            vec![WatchEvent::Delete(PathBuf::from("/root/b.rs"))]
        );
    }

    /// Access events produce no WatchEvents (noise suppression).
    #[test]
    fn raw_to_watch_events_access_is_empty() {
        use notify::event::{AccessKind, EventAttributes};
        let event = Event {
            kind: EventKind::Access(AccessKind::Open(notify::event::AccessMode::Any)),
            paths: vec![PathBuf::from("/root/a.rs")],
            attrs: EventAttributes::default(),
        };
        let result = raw_to_watch_events(event, &PathBuf::from("/root"));
        assert!(result.is_empty());
    }

    /// `NotifyWatcherAdapter` drops cleanly (no thread leak, no panic).
    #[test]
    fn watcher_drops_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        let storage = Box::new(InMemoryStorage::new());

        let watcher =
            NotifyWatcherAdapter::new(root, workspace, index, storage, Box::new(NoOpPluginHost))
                .expect("watcher must start");

        drop(watcher); // Must not hang or panic.
    }
}
