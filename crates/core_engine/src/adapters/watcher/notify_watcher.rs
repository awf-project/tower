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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::RenameCoordinator;
use super::debounce::debounce_events;
use super::error::WatcherError;
use super::event_processor::{EventProcessor, WatchEvent};
use crate::adapters::fs::WorkspacePathExclusion;
use crate::adapters::fs::scan::ScanError;
use crate::domain::index::InvertedIndex;
use crate::domain::workspace::ProjectWorkspace;
use crate::ports::{DocumentSyncPort, ExtensionHostPort, NoOpDocumentSync, StoragePort};

enum WorkerMessage {
    Event(WatchEvent),
    CatchUp(Sender<Result<(), WatcherError>>),
    Barrier(Sender<Result<(), WatcherError>>),
}

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
/// use core_engine::ports::NoOpExtensionHost;
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
///     Box::new(NoOpExtensionHost),
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
    sender: Option<Sender<WorkerMessage>>,
    /// Worker thread handle; joined in `Drop` after the channel is closed.
    worker: Option<thread::JoinHandle<()>>,
    rename_coordinator: Arc<RenameCoordinator>,
    cancelled: Arc<AtomicBool>,
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
        extension_host: Box<dyn ExtensionHostPort + Send + Sync>,
    ) -> Result<Self, WatcherError> {
        Self::with_document_sync(
            root,
            workspace,
            index,
            storage,
            extension_host,
            Arc::new(NoOpDocumentSync),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    #[cfg(test)]
    fn with_rename_coordination(
        root: PathBuf,
        workspace: Arc<RwLock<ProjectWorkspace>>,
        index: Arc<RwLock<InvertedIndex>>,
        storage: Box<dyn StoragePort + Send>,
        extension_host: Box<dyn ExtensionHostPort + Send + Sync>,
        coordinator: Arc<RenameCoordinator>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, WatcherError> {
        Self::with_coordination(
            root,
            workspace,
            index,
            storage,
            extension_host,
            Arc::new(NoOpDocumentSync),
            Arc::new(Mutex::new(HashMap::new())),
            coordinator,
            cancelled,
        )
    }

    /// Like [`new`](Self::new), but also wires a language-server document-sync
    /// sink and the shared formatter `echo_set` into the `EventProcessor`
    /// (spec 14b live sync + echo suppression).
    ///
    /// # Errors
    ///
    /// Same as [`new`](Self::new).
    pub fn with_document_sync(
        root: PathBuf,
        workspace: Arc<RwLock<ProjectWorkspace>>,
        index: Arc<RwLock<InvertedIndex>>,
        storage: Box<dyn StoragePort + Send>,
        extension_host: Box<dyn ExtensionHostPort + Send + Sync>,
        doc_sync: Arc<dyn DocumentSyncPort + Send + Sync>,
        echo_set: Arc<Mutex<HashMap<String, ()>>>,
    ) -> Result<Self, WatcherError> {
        Self::with_coordination(
            root,
            workspace,
            index,
            storage,
            extension_host,
            doc_sync,
            echo_set,
            Arc::new(RenameCoordinator::default()),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_coordination(
        root: PathBuf,
        workspace: Arc<RwLock<ProjectWorkspace>>,
        index: Arc<RwLock<InvertedIndex>>,
        storage: Box<dyn StoragePort + Send>,
        extension_host: Box<dyn ExtensionHostPort + Send + Sync>,
        doc_sync: Arc<dyn DocumentSyncPort + Send + Sync>,
        echo_set: Arc<Mutex<HashMap<String, ()>>>,
        rename_coordinator: Arc<RenameCoordinator>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, WatcherError> {
        cancelled.store(false, Ordering::Release);
        let path_exclusion =
            Arc::new(WorkspacePathExclusion::new(root.clone()).map_err(|error| {
                WatcherError::Storage(crate::ports::PortError::ReadFailed(format!(
                    "path exclusion initialization failed: {error:?}"
                )))
            })?);
        let (tx, rx): (Sender<WorkerMessage>, Receiver<WorkerMessage>) = unbounded();

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
                        // This is an unbounded channel: sending does not wait for
                        // the worker to finish extension callbacks. Deletion
                        // events are index-correctness signals, so dropping them
                        // under backpressure would create permanent ghosts.
                        if tx_for_callback.send(WorkerMessage::Event(watch_event)).is_err() {
                            // Channel closed during shutdown — log and continue.
                            eprintln!(
                                "[tower/watcher] event channel closed during shutdown; dropping event"
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
        let mut processor = EventProcessor::new(root, workspace, index, storage, extension_host)
            .with_document_sync(doc_sync, echo_set)
            .with_rename_coordination(
                path_exclusion,
                Arc::clone(&rename_coordinator),
                Arc::clone(&cancelled),
            );

        // Decision: wrap each per-event invocation in `catch_unwind` so that a
        // panicking `ExtensionHostPort` or `StoragePort` implementation does not
        // kill the worker thread permanently. Without this, a panic inside
        // `process_event` (e.g. from a misbehaving extension host) terminates
        // the worker thread; subsequent OS events are no longer applied, causing
        // the workspace to diverge from disk indefinitely.
        //
        // Trade-off: `AssertUnwindSafe` is required because `EventProcessor`
        // holds boxed trait objects whose unwind safety we cannot
        // statically verify. We accept this because:
        //   a) the domain state (workspace + index) is guarded by `RwLock`s that
        //      are not held across the `extension_host` call, so a panic there does
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
                for message in rx.into_iter() {
                    match message {
                        WorkerMessage::Event(event) => {
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
                        WorkerMessage::CatchUp(acknowledgement) => {
                            let result = processor.reconcile_workspace().map(|_| ()).map_err(
                                |source| match source {
                                    ScanError::Persistence { source, .. } => {
                                        WatcherError::Storage(source)
                                    }
                                    source => WatcherError::Storage(
                                        crate::ports::PortError::ReadFailed(format!(
                                            "catch_up failed: {source}"
                                        )),
                                    ),
                                },
                            );
                            let _ = acknowledgement.send(result);
                        }
                        WorkerMessage::Barrier(acknowledgement) => {
                            let _ = acknowledgement.send(Ok(()));
                        }
                    }
                }
                // Channel closed — clean exit.
            })
            .map_err(|e| WatcherError::SpawnFailed(e.to_string()))?;

        rename_coordinator.set_watcher_active(true);
        Ok(Self {
            watcher: Some(watcher),
            sender: Some(tx),
            worker: Some(worker),
            rename_coordinator,
            cancelled,
        })
    }

    pub fn request_catch_up(&self) -> Result<(), WatcherError> {
        let (acknowledgement, result) = unbounded();
        self.sender
            .as_ref()
            .ok_or(WatcherError::ChannelClosed)?
            .send(WorkerMessage::CatchUp(acknowledgement))
            .map_err(|_| WatcherError::ChannelClosed)?;
        result.recv().map_err(|_| WatcherError::ChannelClosed)?
    }

    pub fn barrier(&self) -> Result<(), WatcherError> {
        let (acknowledgement, result) = unbounded();
        self.sender
            .as_ref()
            .ok_or(WatcherError::ChannelClosed)?
            .send(WorkerMessage::Barrier(acknowledgement))
            .map_err(|_| WatcherError::ChannelClosed)?;
        result.recv().map_err(|_| WatcherError::ChannelClosed)?
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
        self.cancelled.store(true, Ordering::Release);
        self.rename_coordinator.set_watcher_active(false);
        self.rename_coordinator.wake_waiters();

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

        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            paths.into_iter().map(WatchEvent::Delete).collect()
        }

        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            paths.into_iter().map(WatchEvent::Create).collect()
        }

        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
            let mut paths = paths.into_iter();
            let from = paths.next().expect("len checked");
            let to = paths.next().expect("len checked");
            vec![WatchEvent::Rename { from, to }]
        }

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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::adapters::in_memory_storage::InMemoryStorage;
    use crate::adapters::watcher::{
        RenameCoordinationError, RenameCoordinator, RenameEventDisposition,
    };
    use crate::domain::index::{FileSearch, InvertedIndex};
    use crate::domain::refactor::PathMove;
    use crate::domain::token::tokenize;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::domain::{ContentHash, FileId, FileMetadata, RelativePath, Timestamp, VirtualFile};
    use crate::ports::inbound::{DirectoryEntryKind, SearchUseCase as _};
    use crate::ports::{ExtensionHostPort, NoOpExtensionHost, StoragePort};

    fn file_move(source: &str, destination: &str) -> PathMove {
        PathMove {
            source: RelativePath::new(source),
            destination: RelativePath::new(destination),
            kind: DirectoryEntryKind::File,
        }
    }

    fn committed_file(path: &str) -> VirtualFile {
        VirtualFile {
            id: FileId::new_for_testing(7, 3),
            path: RelativePath::new(path),
            size: 19,
            modified: Timestamp(41),
            content_hash: None,
        }
    }

    /// Observes watcher-owned persistence without replacing its real in-memory
    /// storage semantics.  The ordering test needs this boundary because a
    /// queued event must not reach storage before the authoritative commit.
    struct RecordingStorage {
        inner: InMemoryStorage,
        mutations: Arc<AtomicUsize>,
        rename_probe: Option<Arc<NativeRenameProbe>>,
    }

    impl StoragePort for RecordingStorage {
        fn apply_reconciliation_delta(
            &mut self,
            delta: &crate::ports::ReconciliationDelta,
        ) -> Result<(), crate::ports::PortError> {
            if let Some(probe) = &self.rename_probe {
                probe.stage("before persistence");
                assert_eq!(delta.additions.len(), 1);
                assert_eq!(delta.additions[0].path, RelativePath::new("src/new.rs"));
                assert_eq!(delta.removals, vec![delta.additions[0].id]);
            }
            let result = self.inner.apply_reconciliation_delta(delta);
            if let Some(probe) = &self.rename_probe {
                probe.stage("after persistence");
            }
            result
        }

        fn get(&self, id: FileId) -> Result<VirtualFile, crate::ports::PortError> {
            self.inner.get(id)
        }

        fn put(&mut self, file: VirtualFile) -> Result<(), crate::ports::PortError> {
            self.mutations.fetch_add(1, Ordering::AcqRel);
            self.inner.put(file)
        }

        fn put_batch(&mut self, files: &[VirtualFile]) -> Result<(), crate::ports::PortError> {
            self.mutations.fetch_add(files.len(), Ordering::AcqRel);
            self.inner.put_batch(files)
        }

        fn delete(&mut self, id: FileId) -> Result<(), crate::ports::PortError> {
            self.mutations.fetch_add(1, Ordering::AcqRel);
            self.inner.delete(id)
        }

        fn put_blob(
            &mut self,
            hash: ContentHash,
            bytes: Vec<u8>,
        ) -> Result<(), crate::ports::PortError> {
            self.inner.put_blob(hash, bytes)
        }

        fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, crate::ports::PortError> {
            self.inner.get_blob(hash)
        }

        fn mark_scan_complete(&mut self) -> Result<(), crate::ports::PortError> {
            self.inner.mark_scan_complete()
        }

        fn is_scan_complete(&self) -> Result<bool, crate::ports::PortError> {
            self.inner.is_scan_complete()
        }
    }

    struct RecordingExtensionHost(Arc<Mutex<Vec<RelativePath>>>);

    impl ExtensionHostPort for RecordingExtensionHost {
        fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

        fn on_file_changed(&self, _id: FileId, path: &RelativePath) {
            self.0
                .lock()
                .expect("record extension change")
                .push(path.clone());
        }

        fn on_file_deleted(&self, _path: &RelativePath) {}

        fn declared_tools(
            &self,
        ) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
            Vec::new()
        }

        fn invoke(
            &self,
            tool_name: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, crate::domain::InvokeError> {
            Err(crate::domain::InvokeError::ToolNotFound(
                tool_name.to_owned(),
            ))
        }
    }

    #[test]
    fn live_worker_uses_shared_workspace_path_exclusion() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".towerignore"), b"private.rs\n").unwrap();
        let excluded = nested.join("private.rs");
        std::fs::write(&excluded, b"secret").unwrap();
        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        let watcher = NotifyWatcherAdapter::new(
            root.path().to_path_buf(),
            Arc::clone(&workspace),
            index,
            Box::new(InMemoryStorage::new()),
            Box::new(NoOpExtensionHost),
        )
        .expect("watcher starts with shared exclusion policy");

        watcher
            .sender
            .as_ref()
            .expect("worker sender remains open")
            .send(WorkerMessage::Event(WatchEvent::Create(excluded)))
            .unwrap();
        watcher.barrier().unwrap();

        assert!(workspace.read().unwrap().snapshot().files.is_empty());
    }

    #[test]
    fn engine_owned_rename_events_wait_for_explicit_commit() {
        let coordinator = Arc::new(RenameCoordinator::default());
        let source = RelativePath::new("src/old.rs");
        let destination = RelativePath::new("src/new.rs");

        let root = tempfile::tempdir().expect("temporary workspace exists");
        let source_on_disk = root.path().join(source.as_str());
        let destination_on_disk = root.path().join(destination.as_str());
        std::fs::create_dir_all(source_on_disk.parent().expect("source has parent"))
            .expect("source directory exists");
        std::fs::write(&source_on_disk, "before rename").expect("source file exists");

        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        let source_id = workspace
            .write()
            .expect("workspace lock is available")
            .insert(source.clone(), FileMetadata::default())
            .expect("source is indexed before the rename");
        index
            .write()
            .expect("index lock is available")
            .insert(source_id, &tokenize(source.as_str()));
        let storage_mutations = Arc::new(AtomicUsize::new(0));
        let extension_changes = Arc::new(Mutex::new(Vec::new()));
        let watcher = NotifyWatcherAdapter::with_rename_coordination(
            root.path().to_path_buf(),
            Arc::clone(&workspace),
            Arc::clone(&index),
            Box::new(RecordingStorage {
                inner: InMemoryStorage::new(),
                mutations: Arc::clone(&storage_mutations),
                rename_probe: None,
            }),
            Box::new(RecordingExtensionHost(Arc::clone(&extension_changes))),
            Arc::clone(&coordinator),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("real watcher worker starts");
        let mut guard = coordinator
            .register_engine_owned(&[file_move(source.as_str(), destination.as_str())])
            .expect("native path registers before filesystem mutation");

        std::fs::rename(&source_on_disk, &destination_on_disk)
            .expect("filesystem rename completes after registration");
        watcher
            .sender
            .as_ref()
            .expect("worker sender remains open")
            .send(WorkerMessage::Event(WatchEvent::Rename {
                from: source_on_disk,
                to: destination_on_disk.clone(),
            }))
            .expect("matching event is queued through the real worker");

        let (barrier_tx, barrier_rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                barrier_tx
                    .send(watcher.barrier())
                    .expect("report barrier result")
            });
            assert!(
                barrier_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "the worker must not acknowledge a queued matching rename before release"
            );
            assert!(
                workspace.try_read().is_ok(),
                "watcher waiting must leave workspace readers available"
            );
            assert!(
                workspace
                    .read()
                    .expect("workspace reader remains available")
                    .get_by_path(&source)
                    .is_some(),
                "the watcher must not remap workspace before the native commit"
            );
            assert_eq!(storage_mutations.load(Ordering::Acquire), 0);
            assert!(
                extension_changes
                    .lock()
                    .expect("read extension changes")
                    .is_empty(),
                "the watcher must not notify extensions before the native commit"
            );

            let committed = {
                let mut workspace = workspace.write().expect("workspace lock is available");
                workspace
                    .remap_paths(&[file_move(source.as_str(), destination.as_str())])
                    .expect("native workspace remap succeeds");
                let disk_metadata = std::fs::metadata(&destination_on_disk)
                    .expect("committed destination metadata is readable");
                let modified = disk_metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| Timestamp(duration.as_secs()))
                    .unwrap_or(Timestamp(0));
                workspace
                    .update(
                        source_id,
                        FileMetadata {
                            size: disk_metadata.len(),
                            modified,
                            content_hash: None,
                        },
                    )
                    .expect("native commit records destination metadata");
                workspace
                    .get_by_path(&destination)
                    .and_then(|id| workspace.get(id).ok())
                    .cloned()
                    .expect("native destination identity is available")
            };
            {
                let mut index = index.write().expect("index lock is available");
                index.remove(source_id);
                index.insert(source_id, &tokenize(destination.as_str()));
            }
            assert!(
                barrier_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "the worker must remain blocked after the native workspace/index remap"
            );

            let mut native_storage = InMemoryStorage::new();
            native_storage
                .put(committed.clone())
                .expect("native storage remap succeeds");
            assert!(
                barrier_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "the worker must remain blocked after native storage persistence"
            );

            let native_extension_changes = Arc::new(Mutex::new(Vec::new()));
            RecordingExtensionHost(Arc::clone(&native_extension_changes))
                .on_file_changed(committed.id, &destination);
            assert_eq!(
                native_extension_changes
                    .lock()
                    .expect("read native extension changes")
                    .as_slice(),
                std::slice::from_ref(&destination)
            );
            assert!(
                barrier_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "the worker must remain blocked after native extension notification"
            );

            guard.commit(&[committed]);
            drop(guard);

            assert!(
                barrier_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("worker reports barrier after release")
                    .is_ok(),
                "release lets the worker reconcile the queued event"
            );
        });
        assert_eq!(
            coordinator.classify_event(&source, &destination),
            Ok(RenameEventDisposition::External),
            "the real worker consumes the retained echo rather than duplicating the commit"
        );
        assert_eq!(
            storage_mutations.load(Ordering::Acquire),
            0,
            "the retained echo prevents duplicate watcher storage mutations"
        );
        assert!(
            extension_changes
                .lock()
                .expect("read watcher extension changes")
                .is_empty(),
            "the retained echo prevents duplicate watcher extension notifications"
        );
    }

    struct NativeRenameProbe {
        coordinator: Arc<RenameCoordinator>,
        stages: Mutex<Vec<&'static str>>,
        sender: Mutex<Option<Sender<WorkerMessage>>>,
        barrier: Mutex<Option<crossbeam_channel::Receiver<Result<(), WatcherError>>>>,
    }

    impl NativeRenameProbe {
        fn stage(&self, stage: &'static str) {
            assert_eq!(
                self.coordinator.classify_event(
                    &RelativePath::new("src/old.rs"),
                    &RelativePath::new("src/new.rs"),
                ),
                Ok(RenameEventDisposition::WaitForCommit),
                "the native handler must retain its guard at {stage}",
            );
            self.stages.lock().unwrap().push(stage);
        }
    }

    // A failed handler may return before invoking the instrumented filesystem.
    // Close its extra sender before watcher Drop joins the worker, also on panic.
    struct ProbeSenderCleanup(Arc<NativeRenameProbe>);

    impl Drop for ProbeSenderCleanup {
        fn drop(&mut self) {
            self.0.sender.lock().unwrap().take();
        }
    }

    struct ObservedRenameFs {
        inner: crate::adapters::RealFs,
        probe: Arc<NativeRenameProbe>,
    }

    impl crate::ports::FileSystemPort for ObservedRenameFs {
        fn exists(&self, path: &RelativePath) -> bool {
            self.inner.exists(path)
        }
        fn read(&self, path: &RelativePath) -> Result<Vec<u8>, crate::ports::PortError> {
            self.inner.read(path)
        }
        fn write(
            &mut self,
            path: RelativePath,
            bytes: Vec<u8>,
        ) -> Result<(), crate::ports::PortError> {
            self.inner.write(path, bytes)
        }
        fn rename(
            &mut self,
            from: &RelativePath,
            to: RelativePath,
        ) -> Result<(), crate::ports::PortError> {
            let sender = self.probe.sender.lock().unwrap().take().unwrap();
            self.probe.stage("before filesystem rename");
            let root = self.inner.workspace_root().unwrap();
            sender
                .send(WorkerMessage::Event(WatchEvent::Rename {
                    from: root.join(from.as_str()),
                    to: root.join(to.as_str()),
                }))
                .unwrap();
            let (ack, barrier) = unbounded();
            sender.send(WorkerMessage::Barrier(ack)).unwrap();
            *self.probe.barrier.lock().unwrap() = Some(barrier);
            self.inner.rename(from, to)?;
            self.probe.stage("after filesystem rename");
            Ok(())
        }
        fn delete(&mut self, path: &RelativePath) -> Result<(), crate::ports::PortError> {
            self.inner.delete(path)
        }
        fn mkdir(&mut self, path: RelativePath) -> Result<(), crate::ports::PortError> {
            self.inner.mkdir(path)
        }
        fn scan(&self) -> Vec<RelativePath> {
            self.inner.scan()
        }
        fn workspace_root(&self) -> Option<PathBuf> {
            self.inner.workspace_root()
        }
    }

    struct NativeRenameNotifications {
        state: std::sync::Weak<RwLock<crate::adapters::mcp::native_tools::EngineState>>,
        probe: Arc<NativeRenameProbe>,
        fail_persistence: bool,
        changes: Arc<Mutex<Vec<(FileId, RelativePath)>>>,
    }

    impl ExtensionHostPort for NativeRenameNotifications {
        fn on_file_indexed(&self, _: FileId, _: &RelativePath) {}
        fn on_file_deleted(&self, _: &RelativePath) {
            panic!("rename must not publish deletion")
        }
        fn on_file_changed(&self, id: FileId, path: &RelativePath) {
            self.probe.stage("notification");
            let state = self.state.upgrade().unwrap();
            let state = state
                .try_write()
                .expect("callback must run outside EngineState lock");
            let workspace = state
                .workspace
                .try_write()
                .expect("callback must run outside workspace lock");
            let index = state
                .index
                .try_write()
                .expect("callback must run outside index lock");
            assert_eq!(workspace.get_by_path(path), Some(id));
            assert!(
                workspace
                    .get_by_path(&RelativePath::new("src/old.rs"))
                    .is_none()
            );
            let search = FileSearch::new(&index, &workspace);
            assert_eq!(search.find_file("new").unwrap(), vec![path.clone()]);
            assert!(search.find_file("old").unwrap().is_empty());
            let persisted = state.storage.get(id).unwrap();
            assert_eq!(
                persisted.path,
                RelativePath::new(if self.fail_persistence {
                    "src/old.rs"
                } else {
                    "src/new.rs"
                })
            );
            assert_eq!(state.fs.read(path).unwrap(), b"native rename");
            // Release every state lock before allowing the worker a chance to run.
            drop(index);
            drop(workspace);
            drop(state);
            assert!(
                matches!(
                    self.probe
                        .barrier
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .recv_timeout(Duration::from_millis(30)),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout)
                ),
                "watcher must still wait through the real callback"
            );
            self.changes.lock().unwrap().push((id, path.clone()));
        }
        fn declared_tools(
            &self,
        ) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
            Vec::new()
        }
        fn invoke(
            &self,
            name: &str,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, crate::domain::InvokeError> {
            Err(crate::domain::InvokeError::ToolNotFound(name.to_owned()))
        }
    }

    fn native_handler_orders_watcher_and_notifications(fail_persistence: bool) {
        use crate::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
        use crate::adapters::mcp::registry::ToolRegistry;

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/old.rs"), b"native rename").unwrap();
        let metadata = std::fs::metadata(root.path().join("src/old.rs")).unwrap();
        let mut workspace = ProjectWorkspace::new();
        let id = workspace
            .insert(
                RelativePath::new("src/old.rs"),
                FileMetadata {
                    size: metadata.len(),
                    modified: Timestamp(
                        metadata
                            .modified()
                            .unwrap()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    ),
                    content_hash: None,
                },
            )
            .unwrap();
        let mut storage = InMemoryStorage::new();
        storage.put(workspace.get(id).unwrap().clone()).unwrap();
        if fail_persistence {
            storage.fail_next_reconciliation_before_commit_for_test();
        }
        let mut index = InvertedIndex::new();
        index.insert(id, &tokenize("src/old.rs"));
        let probe = Arc::new(NativeRenameProbe {
            coordinator: Arc::new(RenameCoordinator::default()),
            stages: Mutex::new(Vec::new()),
            sender: Mutex::new(None),
            barrier: Mutex::new(None),
        });
        let state = Arc::new(RwLock::new(EngineState::new(
            workspace,
            index,
            Box::new(RecordingStorage {
                inner: storage,
                mutations: Arc::new(AtomicUsize::new(0)),
                rename_probe: Some(Arc::clone(&probe)),
            }),
            Box::new(ObservedRenameFs {
                inner: crate::adapters::RealFs::new(root.path()),
                probe: Arc::clone(&probe),
            }),
        )));
        let changes = Arc::new(Mutex::new(Vec::new()));
        state
            .write()
            .unwrap()
            .set_plugin_host(Arc::new(NativeRenameNotifications {
                state: Arc::downgrade(&state),
                probe: Arc::clone(&probe),
                fail_persistence,
                changes: Arc::clone(&changes),
            }));
        let watcher_changes = Arc::new(Mutex::new(Vec::new()));
        let watcher_mutations = Arc::new(AtomicUsize::new(0));
        let watcher = {
            let state = state.read().unwrap();
            NotifyWatcherAdapter::with_rename_coordination(
                root.path().to_path_buf(),
                state.workspace_arc(),
                state.index_arc(),
                Box::new(RecordingStorage {
                    inner: InMemoryStorage::new(),
                    mutations: Arc::clone(&watcher_mutations),
                    rename_probe: None,
                }),
                Box::new(RecordingExtensionHost(Arc::clone(&watcher_changes))),
                Arc::clone(&probe.coordinator),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap()
        };
        *probe.sender.lock().unwrap() = watcher.sender.clone();
        let _sender_cleanup = ProbeSenderCleanup(Arc::clone(&probe));
        let result = NativeToolRegistry::with_rename_coordinator(Arc::clone(&state), Arc::clone(&probe.coordinator))
            .call("tower_rename_path", serde_json::json!({"source":"src/old.rs", "destination":"src/new.rs", "semantic_policy":"none"})).unwrap();
        assert_eq!(
            result["status"],
            if fail_persistence {
                "partial"
            } else {
                "applied"
            }
        );
        probe
            .barrier
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        watcher.barrier().unwrap();
        assert_eq!(
            probe.stages.lock().unwrap().as_slice(),
            &[
                "before filesystem rename",
                "after filesystem rename",
                "before persistence",
                "after persistence",
                "notification",
            ]
        );
        assert_eq!(
            changes.lock().unwrap().as_slice(),
            &[(id, RelativePath::new("src/new.rs"))]
        );
        assert!(watcher_changes.lock().unwrap().is_empty());
        assert_eq!(watcher_mutations.load(Ordering::Acquire), 0);
        assert!(!root.path().join("src/old.rs").exists());
        assert_eq!(
            state
                .read()
                .unwrap()
                .workspace
                .read()
                .unwrap()
                .get_by_path(&RelativePath::new("src/new.rs")),
            Some(id)
        );
    }

    #[test]
    fn native_rename_handler_holds_shared_watcher_guard_through_unlocked_notifications() {
        native_handler_orders_watcher_and_notifications(false);
    }

    #[test]
    fn native_rename_handler_notifies_after_persistence_partial_without_releasing_watcher_early() {
        native_handler_orders_watcher_and_notifications(true);
    }

    #[test]
    fn daemon_assembly_shares_native_coordinator_with_watcher_worker() {
        let root = tempfile::tempdir().expect("temporary workspace exists");
        let source = RelativePath::new("src/old.rs");
        let destination = RelativePath::new("src/new.rs");
        let source_on_disk = root.path().join(source.as_str());
        std::fs::create_dir_all(source_on_disk.parent().expect("source has parent"))
            .expect("source directory exists");
        std::fs::write(&source_on_disk, "before rename").expect("source file exists");

        let opts = crate::adapters::cli::GlobalOpts {
            workspace_dir: Some(root.path().to_path_buf()),
            extensions_dir: None,
        };
        let engine = crate::adapters::daemon::engine::build_engine(
            &opts,
            crate::adapters::config::TowerConfig::default(),
        )
        .expect("daemon assembly succeeds");
        let mut guard = engine
            .rename_coordinator
            .register_engine_owned(&[file_move(source.as_str(), destination.as_str())])
            .expect("native rename registration succeeds through assembled engine");

        engine
            ._watcher
            .sender
            .as_ref()
            .expect("assembled watcher sender remains open")
            .send(WorkerMessage::Event(WatchEvent::Rename {
                from: source_on_disk,
                to: root.path().join(destination.as_str()),
            }))
            .expect("matching event enters the assembled watcher worker");

        let (barrier_tx, barrier_rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                barrier_tx
                    .send(engine._watcher.barrier())
                    .expect("report assembled worker barrier")
            });
            assert!(
                barrier_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                "a native registration must block the assembled watcher worker"
            );

            guard.commit(&[committed_file(destination.as_str())]);
            drop(guard);
            assert!(
                barrier_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("assembled worker reports barrier after release")
                    .is_ok(),
                "release reaches the same coordinator used by the assembled watcher"
            );
        });
    }

    #[test]
    fn registration_rejects_empty_duplicate_and_overlapping_move_sets() {
        let coordinator = RenameCoordinator::default();
        assert_eq!(
            coordinator.register_engine_owned(&[]).unwrap_err(),
            RenameCoordinationError::EmptyMoveSet
        );

        let duplicate = file_move("src/old.rs", "src/new.rs");
        assert_eq!(
            coordinator
                .register_engine_owned(&[duplicate.clone(), duplicate])
                .unwrap_err(),
            RenameCoordinationError::DuplicateMove {
                source: RelativePath::new("src/old.rs"),
                destination: RelativePath::new("src/new.rs"),
            }
        );

        let _active = coordinator
            .register_engine_owned(&[file_move("src/old.rs", "src/new.rs")])
            .unwrap();
        assert_eq!(
            coordinator
                .register_engine_owned(&[file_move("src/old.rs", "src/other.rs")])
                .unwrap_err(),
            RenameCoordinationError::OverlappingMove {
                path: RelativePath::new("src/old.rs"),
            }
        );
    }

    #[test]
    fn dropping_an_uncommitted_guard_cancels_without_retaining_an_echo() {
        let coordinator = RenameCoordinator::default();
        coordinator.set_watcher_active(true);
        let source = RelativePath::new("src/old.rs");
        let destination = RelativePath::new("src/new.rs");
        let guard = coordinator
            .register_engine_owned(&[file_move(source.as_str(), destination.as_str())])
            .unwrap();

        drop(guard);

        assert_eq!(
            coordinator.classify_event(&source, &destination),
            Ok(RenameEventDisposition::External)
        );
    }

    #[test]
    fn registration_and_release_succeed_without_an_active_watcher() {
        let coordinator = RenameCoordinator::default();
        let source = RelativePath::new("src/old.rs");
        let destination = RelativePath::new("src/new.rs");
        let mut guard = coordinator
            .register_engine_owned(&[file_move(source.as_str(), destination.as_str())])
            .unwrap();

        guard.commit(&[committed_file(destination.as_str())]);
        drop(guard);

        assert_eq!(
            coordinator.classify_event(&source, &destination),
            Ok(RenameEventDisposition::External)
        );
    }

    #[test]
    fn retained_echoes_are_bounded_consumed_once_and_match_the_committed_pair() {
        let coordinator = RenameCoordinator::default();
        coordinator.set_watcher_active(true);

        let active_source = RelativePath::new("src/active.rs");
        let active_destination = RelativePath::new("src/active-renamed.rs");
        let active_guard = coordinator
            .register_engine_owned(&[file_move(
                active_source.as_str(),
                active_destination.as_str(),
            )])
            .expect("active registration succeeds");

        for number in 0..=1024 {
            let source = format!("src/old-{number}.rs");
            let destination = format!("src/new-{number}.rs");
            let mut guard = coordinator
                .register_engine_owned(&[file_move(&source, &destination)])
                .expect("completed registrations must not evict an active registration");
            guard.commit(&[committed_file(&destination)]);
            drop(guard);
        }

        assert_eq!(
            coordinator.classify_event(&active_source, &active_destination),
            Ok(RenameEventDisposition::WaitForCommit),
            "the eviction cap must never evict an active registration"
        );
        assert_eq!(
            coordinator.classify_event(
                &RelativePath::new("src/old-0.rs"),
                &RelativePath::new("src/new-0.rs"),
            ),
            Ok(RenameEventDisposition::External),
            "the oldest retained echo must be evicted at the 1,024-registration cap"
        );
        let source = RelativePath::new("src/old-1024.rs");
        let destination = RelativePath::new("src/new-1024.rs");
        assert_eq!(
            coordinator.classify_event(&source, &active_destination),
            Ok(RenameEventDisposition::External),
            "a mismatched destination must not consume the retained echo"
        );
        assert_eq!(
            coordinator.classify_event(&source, &destination),
            Ok(RenameEventDisposition::ConsumeEcho),
            "the exact pair must remain consumable after a mismatch"
        );
        assert_eq!(
            coordinator.classify_event(&source, &destination),
            Ok(RenameEventDisposition::External),
            "a retained echo must be consumed exactly once"
        );

        let expired_source = RelativePath::new("src/expired.rs");
        let expired_destination = RelativePath::new("src/expired-renamed.rs");
        let mut expired_guard = coordinator
            .register_engine_owned(&[file_move(
                expired_source.as_str(),
                expired_destination.as_str(),
            )])
            .expect("expiry fixture registration succeeds");
        expired_guard.commit(&[committed_file(expired_destination.as_str())]);
        drop(expired_guard);
        let retained_at = Instant::now();
        assert_eq!(
            coordinator.classify_event_at(
                &expired_source,
                &expired_destination,
                retained_at + Duration::from_millis(1_999),
            ),
            Ok(RenameEventDisposition::ConsumeEcho),
            "an echo remains valid immediately before the retention boundary"
        );

        let mut boundary_guard = coordinator
            .register_engine_owned(&[file_move(
                expired_source.as_str(),
                expired_destination.as_str(),
            )])
            .expect("boundary fixture registration succeeds");
        boundary_guard.commit(&[committed_file(expired_destination.as_str())]);
        drop(boundary_guard);
        let boundary_retained_at = Instant::now();
        assert_eq!(
            coordinator.classify_event_at(
                &expired_source,
                &expired_destination,
                boundary_retained_at + Duration::from_secs(2),
            ),
            Ok(RenameEventDisposition::External),
            "an echo expires at the two-second retention boundary"
        );

        drop(active_guard);
    }

    #[test]
    fn commit_retains_only_destination_metadata_verified_pairs() {
        let coordinator = RenameCoordinator::default();
        coordinator.set_watcher_active(true);
        let source = RelativePath::new("src/old.rs");
        let destination = RelativePath::new("src/new.rs");

        let mut missing_guard = coordinator
            .register_engine_owned(&[file_move(source.as_str(), destination.as_str())])
            .unwrap();
        missing_guard.commit(&[]);
        drop(missing_guard);
        assert_eq!(
            coordinator.classify_event(&source, &destination),
            Ok(RenameEventDisposition::External),
            "a move absent from committed files must not retain an echo"
        );

        let mut wrong_destination_guard = coordinator
            .register_engine_owned(&[file_move(source.as_str(), destination.as_str())])
            .unwrap();
        wrong_destination_guard.commit(&[committed_file("src/different.rs")]);
        drop(wrong_destination_guard);
        assert_eq!(
            coordinator.classify_event(&source, &destination),
            Ok(RenameEventDisposition::External),
            "destination metadata for another path must not verify the echo"
        );
    }

    #[test]
    fn watcher_shutdown_cancels_a_wait_while_a_guard_remains_live() {
        let coordinator = Arc::new(RenameCoordinator::default());
        coordinator.set_watcher_active(true);
        let source = RelativePath::new("src/old.rs");
        let destination = RelativePath::new("src/new.rs");
        let _guard = coordinator
            .register_engine_owned(&[file_move(source.as_str(), destination.as_str())])
            .unwrap();
        let root = tempfile::tempdir().expect("temporary watcher root exists");
        let cancelled = Arc::new(AtomicBool::new(false));
        let watcher = NotifyWatcherAdapter::with_rename_coordination(
            root.path().to_path_buf(),
            Arc::new(RwLock::new(ProjectWorkspace::new())),
            Arc::new(RwLock::new(InvertedIndex::new())),
            Box::new(InMemoryStorage::new()),
            Box::new(NoOpExtensionHost),
            Arc::clone(&coordinator),
            Arc::clone(&cancelled),
        )
        .expect("real watcher starts");
        let (result_tx, result_rx) = mpsc::channel();
        let waiting_coordinator = Arc::clone(&coordinator);
        let waiting_cancelled = Arc::clone(&cancelled);
        let waiter = thread::spawn(move || {
            let result = waiting_coordinator.wait_for_path(&source, &waiting_cancelled);
            result_tx.send(result).unwrap();
        });

        let shutdown = thread::spawn(move || drop(watcher));

        assert_eq!(
            result_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(RenameCoordinationError::Cancelled)
        );
        shutdown
            .join()
            .expect("watcher shutdown joins its worker within the bounded wait");
        waiter.join().unwrap();
    }

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
            Box::new(NoOpExtensionHost),
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

    /// Rename-from modify events are deletion signals for the old path.
    #[test]
    fn raw_to_watch_events_rename_from_is_delete() {
        use notify::event::EventAttributes;
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![PathBuf::from("/root/old.rs")],
            attrs: EventAttributes::default(),
        };
        let result = raw_to_watch_events(event, &PathBuf::from("/root"));
        assert_eq!(
            result,
            vec![WatchEvent::Delete(PathBuf::from("/root/old.rs"))]
        );
    }

    /// Rename-to modify events are creation signals for the new path.
    #[test]
    fn raw_to_watch_events_rename_to_is_create() {
        use notify::event::EventAttributes;
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            paths: vec![PathBuf::from("/root/new.rs")],
            attrs: EventAttributes::default(),
        };
        let result = raw_to_watch_events(event, &PathBuf::from("/root"));
        assert_eq!(
            result,
            vec![WatchEvent::Create(PathBuf::from("/root/new.rs"))]
        );
    }

    /// A paired rename event preserves source and destination in one WatchEvent.
    #[test]
    fn raw_to_watch_events_rename_both_is_rename() {
        use notify::event::EventAttributes;
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![PathBuf::from("/root/old.rs"), PathBuf::from("/root/new.rs")],
            attrs: EventAttributes::default(),
        };
        let result = raw_to_watch_events(event, &PathBuf::from("/root"));
        assert_eq!(
            result,
            vec![WatchEvent::Rename {
                from: PathBuf::from("/root/old.rs"),
                to: PathBuf::from("/root/new.rs"),
            }]
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
            NotifyWatcherAdapter::new(root, workspace, index, storage, Box::new(NoOpExtensionHost))
                .expect("watcher must start");

        drop(watcher); // Must not hang or panic.
    }
}
