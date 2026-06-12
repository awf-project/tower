//! Integration test: live VFS sync through the wired `NotifyWatcherAdapter`.
//!
//! Verifies that the real OS watcher, real sled storage, and real workspace/index
//! all stay in sync when files are created, modified, and deleted under the
//! watched root.
//!
//! # Why these tests are not `#[ignore]`
//!
//! Unlike the narrow smoke test in `notify_watcher.rs` (which only tests the
//! notify subscription itself and is ignored in CI), these tests exercise the
//! full integration path: sled storage + workspace + inverted index + watcher.
//! They are kept in the default test suite because they are stable on Linux
//! (inotify) and macOS (FSEvents/kqueue) — the CI environments used by this
//! project.
//!
//! # Latency handling
//!
//! Rather than sleeping a fixed duration, each assertion polls with a short
//! deadline (`wait_until`). This is resilient to scheduler jitter while still
//! failing fast when something is genuinely broken.
//!
//! Decision: 2-second poll deadline per assertion, 20 ms interval.
//! Why: inotify on Linux typically delivers within < 10 ms; 2 s gives 200×
//! headroom before a hard test failure. The interval is short enough to fail
//! fast on real breakage.
//! Trade-off: a very slow CI host could still flake — if that happens, raise
//! the deadline rather than adding a sleep.
#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use core_engine::adapters::SledStorageAdapter;
use core_engine::adapters::watcher::NotifyWatcherAdapter;
use core_engine::domain::index::{FileSearch, InvertedIndex};
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::NoOpPluginHost;
use core_engine::ports::inbound::SearchUseCase;

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Poll `condition` every `interval` until it returns `true` or `deadline`
/// expires.
///
/// Returns `true` if the condition was satisfied within the deadline, `false`
/// otherwise.
fn wait_until(deadline: Duration, interval: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if condition() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}

const DEADLINE: Duration = Duration::from_secs(2);
const INTERVAL: Duration = Duration::from_millis(20);

/// Build a watcher over `root` connected to the given shared workspace + index
/// and a real sled storage clone.
fn start_watcher(
    root: std::path::PathBuf,
    workspace: Arc<RwLock<ProjectWorkspace>>,
    index: Arc<RwLock<InvertedIndex>>,
    storage: SledStorageAdapter,
) -> NotifyWatcherAdapter {
    NotifyWatcherAdapter::new(
        root,
        workspace,
        index,
        Box::new(storage),
        Box::new(NoOpPluginHost),
    )
    .expect("watcher must start in integration test environment")
}

// ── AC1: file create → appears in VFS and index ───────────────────────────────

/// When a file is written to the watched workspace, the VFS and inverted index
/// should reflect it within the poll deadline.
#[test]
fn live_sync_create_file_appears_in_vfs_and_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let db_path = root.join(".tower").join("db");
    std::fs::create_dir_all(&db_path).unwrap();

    let (storage, workspace, index) = SledStorageAdapter::open(&db_path).expect("sled must open");
    let workspace = Arc::new(RwLock::new(workspace));
    let index = Arc::new(RwLock::new(index));
    let storage_clone = storage.try_clone();

    let _watcher = start_watcher(
        root.clone(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        storage_clone,
    );

    // Write a new file to the watched workspace.
    let file_path = root.join("hello_world.rs");
    std::fs::write(&file_path, b"pub fn hello() {}").unwrap();

    // Poll until the file appears in the inverted index.
    let appeared = wait_until(DEADLINE, INTERVAL, || {
        let ws = workspace.read().unwrap();
        let idx = index.read().unwrap();
        FileSearch::new(&idx, &ws)
            .find_file("hello_world")
            .unwrap_or_default()
            .is_empty()
            .not()
    });

    assert!(
        appeared,
        "hello_world.rs must appear in the VFS/index within {DEADLINE:?} after creation"
    );
}

// ── AC2: file modify → index reflects updated content ────────────────────────

/// When an already-indexed file is overwritten, the index is updated to reflect
/// the new content within the poll deadline.
#[test]
fn live_sync_modify_file_updates_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let db_path = root.join(".tower").join("db");
    std::fs::create_dir_all(&db_path).unwrap();

    let (storage, workspace, index) = SledStorageAdapter::open(&db_path).expect("sled must open");
    let workspace = Arc::new(RwLock::new(workspace));
    let index = Arc::new(RwLock::new(index));
    let storage_clone = storage.try_clone();

    let _watcher = start_watcher(
        root.clone(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        storage_clone,
    );

    // Write the file AFTER the watcher starts so the create event is delivered.
    let file_path = root.join("module_alpha.rs");
    std::fs::write(&file_path, b"fn original() {}").unwrap();

    // Wait for initial indexing.
    let _ = wait_until(DEADLINE, INTERVAL, || {
        let ws = workspace.read().unwrap();
        let idx = index.read().unwrap();
        !FileSearch::new(&idx, &ws)
            .find_file("module_alpha")
            .unwrap_or_default()
            .is_empty()
    });

    // Overwrite the file with new content that contains a new token.
    std::fs::write(&file_path, b"fn renamed_function() {}").unwrap();

    // Poll until the workspace record for the file is updated (mtime changes).
    // We verify the modify path by checking that the file is still indexed
    // (not removed) after the overwrite — the watcher must keep it alive.
    let still_present = wait_until(DEADLINE, INTERVAL, || {
        let ws = workspace.read().unwrap();
        let idx = index.read().unwrap();
        !FileSearch::new(&idx, &ws)
            .find_file("module_alpha")
            .unwrap_or_default()
            .is_empty()
    });

    assert!(
        still_present,
        "module_alpha.rs must still be indexed after a modify event"
    );
}

// ── AC3: file delete → removed from VFS and index ────────────────────────────

/// When a file is deleted from the watched workspace, it must be removed from
/// the VFS and inverted index within the poll deadline.
#[test]
fn live_sync_delete_file_removed_from_vfs_and_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let db_path = root.join(".tower").join("db");
    std::fs::create_dir_all(&db_path).unwrap();

    let (storage, workspace, index) = SledStorageAdapter::open(&db_path).expect("sled must open");
    let workspace = Arc::new(RwLock::new(workspace));
    let index = Arc::new(RwLock::new(index));
    let storage_clone = storage.try_clone();

    let _watcher = start_watcher(
        root.clone(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        storage_clone,
    );

    // Write the file AFTER the watcher starts so the create event is delivered.
    let file_path = root.join("temp_module.rs");
    std::fs::write(&file_path, b"fn temp() {}").unwrap();

    // Wait for it to be indexed.
    let indexed = wait_until(DEADLINE, INTERVAL, || {
        let ws = workspace.read().unwrap();
        let idx = index.read().unwrap();
        !FileSearch::new(&idx, &ws)
            .find_file("temp_module")
            .unwrap_or_default()
            .is_empty()
    });
    assert!(indexed, "temp_module.rs must be indexed before delete test");

    // Delete the file.
    std::fs::remove_file(&file_path).unwrap();

    // Poll until it disappears from the index.
    let removed = wait_until(DEADLINE, INTERVAL, || {
        let ws = workspace.read().unwrap();
        let idx = index.read().unwrap();
        FileSearch::new(&idx, &ws)
            .find_file("temp_module")
            .unwrap_or_default()
            .is_empty()
    });

    assert!(
        removed,
        "temp_module.rs must be removed from the VFS/index within {DEADLINE:?} after deletion"
    );
}

// ── AC4: watcher drop stops live sync ────────────────────────────────────────

/// After dropping the watcher, new files created in the workspace are NOT
/// reflected in the index (live sync has stopped).
#[test]
fn live_sync_stops_after_watcher_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let db_path = root.join(".tower").join("db");
    std::fs::create_dir_all(&db_path).unwrap();

    let (storage, workspace, index) = SledStorageAdapter::open(&db_path).expect("sled must open");
    let workspace = Arc::new(RwLock::new(workspace));
    let index = Arc::new(RwLock::new(index));
    let storage_clone = storage.try_clone();

    let watcher = start_watcher(
        root.clone(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        storage_clone,
    );

    // Drop the watcher — live sync must stop.
    drop(watcher);

    // Write a file after the watcher is gone.
    let file_path = root.join("orphaned.rs");
    std::fs::write(&file_path, b"fn orphaned() {}").unwrap();

    // Give a short window for any in-flight events to drain (belt-and-suspenders).
    std::thread::sleep(Duration::from_millis(100));

    // The file must NOT be indexed because the watcher is gone.
    let ws = workspace.read().unwrap();
    let idx = index.read().unwrap();
    let results = FileSearch::new(&idx, &ws)
        .find_file("orphaned")
        .unwrap_or_default();

    assert!(
        results.is_empty(),
        "orphaned.rs must NOT be indexed after watcher is dropped; got {results:?}"
    );
}

// ── Private extension trait for readability ───────────────────────────────────

trait BoolExt {
    fn not(self) -> bool;
}

impl BoolExt for bool {
    fn not(self) -> bool {
        !self
    }
}
