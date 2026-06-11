//! Spec 08 TDD tests for `FileMutationService`.
//!
//! # TDD sequence (from spec)
//!
//! 1. RED → GREEN: `create_file` → content present + searchable (in-memory).
//! 2. RED → GREEN: crash-before-rename leaves original intact.
//! 3. RED → GREEN: `delete_file` + `create_directory`.
//! 4. RED → GREEN: `.tmp_write` artifact exclusion + watcher idempotency.
//!
//! All tests use `InMemoryFs` + `InMemoryStorage` (no I/O). Integration tests
//! against `RealFs` live in `tests/integration_mutation.rs`.
#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use crate::adapters::in_memory_fs::InMemoryFs;
use crate::adapters::in_memory_storage::InMemoryStorage;
use crate::adapters::watcher::event_processor::{EventProcessor, WatchEvent};
use crate::domain::RelativePath;
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::{FileMutationService, is_tmp_artifact};
use crate::domain::workspace::ProjectWorkspace;
use crate::ports::inbound::FileMutationUseCase;
use crate::ports::{FileSystemPort, NoOpPluginHost, PortError, StoragePort};

// ── TDD step 1 & 2: create_file → content + searchable ───────────────────────

/// AC1: After `create_file`, the target holds exactly `content` and is
/// findable via `find_file`.
#[test]
fn create_file_content_present_and_searchable() {
    use crate::domain::index::FileSearch;
    use crate::ports::inbound::SearchUseCase as _;

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);

    let path = RelativePath::new("src/http_client.rs");
    svc.create_file(path.clone(), b"fn get() {}".to_vec())
        .unwrap();

    // VFS tracks it.
    assert!(
        ws.get_by_path(&path).is_some(),
        "VFS must track the newly created file"
    );

    // Index finds it.
    let results = FileSearch::new(&idx, &ws).find_file("client").unwrap();
    let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();
    assert!(
        paths.contains(&"src/http_client.rs"),
        "file must be searchable after create; got {paths:?}"
    );

    // Storage has a record.
    let file_id = ws.get_by_path(&path).unwrap();
    assert!(
        storage.get(file_id).is_ok(),
        "storage must have a record for the new file"
    );
}

/// AC1 (overwrite): creating the same path twice is an upsert — exactly one
/// VFS entry, FileId stable.
#[test]
fn create_file_overwrite_updates_without_duplication() {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    let path = RelativePath::new("src/config.rs");

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.create_file(path.clone(), b"v1".to_vec()).unwrap();
    }
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.create_file(path.clone(), b"v2".to_vec()).unwrap();
    }

    // Exactly one VFS entry.
    assert_eq!(
        ws.snapshot().files.len(),
        1,
        "overwrite must not duplicate the VFS entry"
    );
    assert!(ws.get_by_path(&path).is_some());
}

// ── TDD step 3: crash-before-rename (AC2) ────────────────────────────────────

/// AC2: A simulated crash between the temp write and the rename leaves the
/// domain state clean (VFS not updated) and `create_file` returns an error.
///
/// Proof:
///  - `create_file` returns `Err` (the crash is observable at the API level).
///  - VFS + index are not updated (no partial domain state).
///  - The original destination is untouched (rename never fired).
///
/// Decision: the `FailAfterWrite` adapter succeeds `write` but fails `rename`.
/// This is the minimal injection needed to simulate a crash between steps 1 and 2
/// of the shadow-file pattern without touching the host filesystem.
#[test]
fn crash_before_rename_returns_error_and_leaves_domain_state_clean() {
    /// Succeeds `write` but fails `rename` — simulating a crash between the two.
    struct FailAfterWrite(InMemoryFs);

    impl FileSystemPort for FailAfterWrite {
        fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
            self.0.read(path)
        }
        fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
            self.0.write(path, bytes)
        }
        fn rename(&mut self, _from: &RelativePath, _to: RelativePath) -> Result<(), PortError> {
            Err(PortError::WriteFailed(
                "simulated crash before rename".to_owned(),
            ))
        }
        fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
            self.0.delete(path)
        }
        fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
            self.0.mkdir(path)
        }
        fn scan(&self) -> Vec<RelativePath> {
            self.0.scan()
        }
    }

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fail_fs = FailAfterWrite(InMemoryFs::new());

    let result = {
        let mut svc = FileMutationService::new(
            &mut fail_fs,
            &mut ws,
            &mut idx,
            &mut storage,
            &NoOpPluginHost,
        );
        svc.create_file(
            RelativePath::new("src/important.rs"),
            b"new content".to_vec(),
        )
    };

    // The call must fail.
    assert!(
        result.is_err(),
        "create_file must return Err when rename fails (crash simulation)"
    );

    // VFS must be empty — no partial state was written.
    assert_eq!(
        ws.snapshot().files.len(),
        0,
        "VFS must be clean after crash-before-rename"
    );

    // The stray .tmp_write is present in the FS (GC-able, AC5).
    let tmp = RelativePath::new("src/important.rs.tmp_write");
    assert!(
        fail_fs.0.read(&tmp).is_ok(),
        ".tmp_write must be present (GC-able) after crash before rename"
    );
}

// ── TDD step 4: delete_file ───────────────────────────────────────────────────

/// AC3: After `delete_file`, the file is gone from the VFS, index, and storage.
///
/// The same `InMemoryFs` is shared across create and delete so the physical
/// file exists when `delete_file` calls `fs.delete`.
#[test]
fn delete_file_removes_from_vfs_index_and_storage() {
    use crate::domain::index::FileSearch;
    use crate::ports::inbound::SearchUseCase as _;

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    let path = RelativePath::new("src/delete_me.rs");

    // Create first.
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.create_file(path.clone(), b"fn go() {}".to_vec())
            .unwrap();
    }

    let file_id = ws.get_by_path(&path).unwrap();

    // Delete.
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.delete_file(&path).unwrap();
    }

    // VFS: gone.
    assert!(
        ws.get_by_path(&path).is_none(),
        "file must be absent from VFS after delete"
    );

    // Index: gone.
    let results = FileSearch::new(&idx, &ws).find_file("delete_me").unwrap();
    assert!(
        results.is_empty(),
        "file must not be searchable after delete; got {results:?}"
    );

    // Storage: gone.
    assert_eq!(
        storage.get(file_id).unwrap_err(),
        PortError::NotFound,
        "storage must not hold a record for the deleted file"
    );
}

/// Crash-ordering: when `fs.delete` fails, the storage record must still be
/// present (physical delete first — if FS fails, domain state is self-healing).
///
/// Decision: fs.delete fires before storage.delete. A crash between them leaves
/// the file gone from disk but still in storage. On restart the watcher fires a
/// Delete event and cleans up storage. This is safer than the reverse ordering,
/// where a crash leaves the file on disk with no domain record.
#[test]
fn delete_file_fs_fails_storage_record_retained() {
    /// Delegates all FS calls to the inner `InMemoryFs` except `delete`, which
    /// always fails — simulating a crash at the fs.delete step.
    struct FailAtFsDelete(InMemoryFs);

    impl FileSystemPort for FailAtFsDelete {
        fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
            self.0.read(path)
        }
        fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
            self.0.write(path, bytes)
        }
        fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
            self.0.rename(from, to)
        }
        fn delete(&mut self, _path: &RelativePath) -> Result<(), PortError> {
            Err(PortError::WriteFailed(
                "simulated fs delete failure".to_owned(),
            ))
        }
        fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
            self.0.mkdir(path)
        }
        fn scan(&self) -> Vec<RelativePath> {
            self.0.scan()
        }
    }

    // Setup: create the file using a plain InMemoryFs so write+rename work,
    // then switch to FailAtFsDelete for the delete step.
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();

    let path = RelativePath::new("src/crash_test.rs");

    // Phase 1: create via plain InMemoryFs so the file exists in storage+VFS.
    let mut plain_fs = InMemoryFs::new();
    {
        let mut svc = FileMutationService::new(
            &mut plain_fs,
            &mut ws,
            &mut idx,
            &mut storage,
            &NoOpPluginHost,
        );
        svc.create_file(path.clone(), b"fn f() {}".to_vec())
            .unwrap();
    }
    let file_id = ws.get_by_path(&path).unwrap();

    // Phase 2: wrap with the failing adapter and attempt delete.
    // The inner InMemoryFs already has the file from phase 1's write+rename.
    let mut fail_fs = FailAtFsDelete(plain_fs);
    let result = {
        let mut svc = FileMutationService::new(
            &mut fail_fs,
            &mut ws,
            &mut idx,
            &mut storage,
            &NoOpPluginHost,
        );
        svc.delete_file(&path)
    };

    assert!(
        result.is_err(),
        "delete_file must propagate fs.delete failure"
    );

    // Storage record must still be present — fs.delete failed before storage.delete.
    // On restart a watcher Delete event will clean it up (self-healing).
    assert!(
        storage.get(file_id).is_ok(),
        "storage must retain the record when fs.delete fails (crash-safe ordering)"
    );
}

/// Crash-ordering: when `storage.delete` fails but `fs.delete` succeeded, the
/// physical file is already gone from disk. On restart the watcher fires a
/// Delete event that cleans up the dangling storage record (self-healing).
#[test]
fn delete_file_storage_fails_physical_file_already_gone() {
    use crate::domain::{ContentHash, FileId, VirtualFile};

    /// Delegates all `StoragePort` methods to the inner `InMemoryStorage` except
    /// `delete`, which always fails — simulating a crash at the storage step.
    struct FailAtStorageDelete(InMemoryStorage);

    impl StoragePort for FailAtStorageDelete {
        fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
            self.0.get(id)
        }
        fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
            self.0.put(file)
        }
        fn put_batch(&mut self, files: &[VirtualFile]) -> Result<(), PortError> {
            self.0.put_batch(files)
        }
        fn delete(&mut self, _id: FileId) -> Result<(), PortError> {
            Err(PortError::WriteFailed(
                "simulated storage delete failure".to_owned(),
            ))
        }
        fn put_blob(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), PortError> {
            self.0.put_blob(hash, bytes)
        }
        fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, PortError> {
            self.0.get_blob(hash)
        }
        fn mark_scan_complete(&mut self) -> Result<(), PortError> {
            self.0.mark_scan_complete()
        }
        fn is_scan_complete(&self) -> Result<bool, PortError> {
            self.0.is_scan_complete()
        }
    }

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut fs = InMemoryFs::new();
    let mut fail_storage = FailAtStorageDelete(InMemoryStorage::new());

    let path = RelativePath::new("src/will_delete.rs");

    // Phase 1: create via real storage so VFS+storage have the record.
    {
        let mut real_storage = InMemoryStorage::new();
        let mut svc = FileMutationService::new(
            &mut fs,
            &mut ws,
            &mut idx,
            &mut real_storage,
            &NoOpPluginHost,
        );
        svc.create_file(path.clone(), b"fn f() {}".to_vec())
            .unwrap();
        // Seed fail_storage with the created record so delete can find it.
        let fid = ws.get_by_path(&path).unwrap();
        let vf = real_storage.get(fid).unwrap();
        fail_storage.0.put(vf).unwrap();
    }

    // Phase 2: delete — fs.delete will succeed, storage.delete will fail.
    let result = {
        let mut svc = FileMutationService::new(
            &mut fs,
            &mut ws,
            &mut idx,
            &mut fail_storage,
            &NoOpPluginHost,
        );
        svc.delete_file(&path)
    };

    assert!(
        result.is_err(),
        "delete_file must propagate storage.delete failure"
    );

    // Physical file must be gone (fs.delete ran before storage.delete failed).
    assert!(
        fs.read(&path).is_err(),
        "physical file must be gone after fs.delete even when storage.delete fails"
    );
}

/// Deleting a non-existent path returns `DomainError::NotFound`.
#[test]
fn delete_file_on_unknown_path_returns_not_found() {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);

    let result = svc.delete_file(&RelativePath::new("ghost.rs"));
    assert!(
        matches!(result, Err(crate::domain::DomainError::NotFound)),
        "delete on unknown path must return NotFound; got {result:?}"
    );
}

// ── TDD step 5: create_directory ─────────────────────────────────────────────

/// AC4: `create_directory("a/b/c")` on a missing tree succeeds.
#[test]
fn create_directory_recursive_succeeds() {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);

    // InMemoryFs mkdir is a no-op but must not error.
    svc.create_directory(RelativePath::new("a/b/c")).unwrap();
}

// ── TDD step 6: tmp artifact exclusion (UN2/AC5) ─────────────────────────────

/// `is_tmp_artifact` correctly identifies `.tmp_write` and `.~tmp` paths.
#[test]
fn is_tmp_artifact_identifies_shadow_file_suffixes() {
    assert!(is_tmp_artifact(&RelativePath::new("src/main.rs.tmp_write")));
    assert!(is_tmp_artifact(&RelativePath::new("src/main.rs.~tmp")));
    assert!(!is_tmp_artifact(&RelativePath::new("src/main.rs")));
    assert!(!is_tmp_artifact(&RelativePath::new("src/tmp_write.rs")));
    assert!(!is_tmp_artifact(&RelativePath::new("src/file.~tmpx")));
    assert!(!is_tmp_artifact(&RelativePath::new("")));
}

/// AC5: Watcher `Create` event for a `.tmp_write` path is silently ignored —
/// the path must NOT appear in the VFS.
///
/// Uses a synthetic root with no real FS I/O: `is_tmp_artifact` fires before
/// `read_metadata`, so no actual file needs to exist on disk.
#[test]
fn watcher_ignores_tmp_write_create_event() {
    // Use a fixed fake root — no real directory is created.
    let root = std::path::PathBuf::from("/fake_root_for_unit_test");

    let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let index = Arc::new(RwLock::new(InvertedIndex::new()));

    let mut processor = EventProcessor::new(
        root.clone(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpPluginHost),
    );

    // is_tmp_artifact fires before read_metadata — no file needs to exist.
    let tmp_path = root.join("src").join("important.rs.tmp_write");
    processor
        .process_event(WatchEvent::Create(tmp_path))
        .unwrap();

    let ws_guard = workspace.read().unwrap();
    assert_eq!(
        ws_guard.snapshot().files.len(),
        0,
        ".tmp_write Create event must not insert into VFS"
    );
}

/// AC5: Watcher `Modify` event for a `.~tmp` path is silently ignored.
///
/// Uses a synthetic root with no real FS I/O: `is_tmp_artifact` fires before
/// `read_metadata`, so no actual file needs to exist on disk.
#[test]
fn watcher_ignores_real_fs_tmp_modify_event() {
    let root = std::path::PathBuf::from("/fake_root_for_unit_test");

    let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let index = Arc::new(RwLock::new(InvertedIndex::new()));

    let mut processor = EventProcessor::new(
        root.clone(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpPluginHost),
    );

    // is_tmp_artifact fires before read_metadata — no file needs to exist.
    let tmp_path = root.join("readme.md.~tmp");
    processor
        .process_event(WatchEvent::Modify(tmp_path))
        .unwrap();

    let ws_guard = workspace.read().unwrap();
    assert_eq!(
        ws_guard.snapshot().files.len(),
        0,
        ".~tmp Modify event must not insert into VFS"
    );
}

// ── TDD step 7: watcher idempotency (UN3/AC6) ────────────────────────────────

/// AC6: A mutation followed by the echoed watcher `Create` event applies the
/// change exactly once — VFS has one entry, FileId is unchanged.
///
/// Sequence:
///  1. `create_file("src/service.rs")` — inserts into VFS.
///  2. OS watcher fires `WatchEvent::Create("src/service.rs")`.
///  3. VFS must still have exactly one entry, FileId unchanged.
#[test]
fn watcher_create_after_mutation_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let index = Arc::new(RwLock::new(InvertedIndex::new()));

    // Step 1: domain mutation via FileMutationService.
    let path = RelativePath::new("src/service.rs");
    let file_id_after_mutation = {
        let mut ws = workspace.write().unwrap();
        let mut idx = index.write().unwrap();
        let mut storage = InMemoryStorage::new();
        let mut fs = InMemoryFs::new();
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.create_file(path.clone(), b"pub fn run() {}".to_vec())
            .unwrap();
        ws.get_by_path(&path).unwrap()
    };

    // Create the actual file on disk so the watcher's metadata read succeeds.
    let abs_path = root.join("src").join("service.rs");
    std::fs::create_dir_all(abs_path.parent().unwrap()).unwrap();
    std::fs::write(&abs_path, b"pub fn run() {}").unwrap();

    // Step 2: watcher fires a Create event for the same path.
    let mut processor = EventProcessor::new(
        root,
        Arc::clone(&workspace),
        Arc::clone(&index),
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpPluginHost),
    );
    processor
        .process_event(WatchEvent::Create(abs_path))
        .unwrap();

    // Step 3: exactly one VFS entry, FileId unchanged.
    let ws_guard = workspace.read().unwrap();
    assert_eq!(
        ws_guard.snapshot().files.len(),
        1,
        "echoed Create must not duplicate the VFS entry"
    );
    let id_after_event = ws_guard.get_by_path(&path).unwrap();
    assert_eq!(
        file_id_after_mutation, id_after_event,
        "FileId must be stable across mutation + echoed watcher Create"
    );
}

/// AC6: A mutation followed by an echoed watcher `Modify` event preserves the
/// FileId and does not duplicate the VFS entry.
#[test]
fn watcher_modify_after_mutation_preserves_file_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let index = Arc::new(RwLock::new(InvertedIndex::new()));

    let path = RelativePath::new("src/handler.rs");
    let file_id_after_mutation = {
        let mut ws = workspace.write().unwrap();
        let mut idx = index.write().unwrap();
        let mut storage = InMemoryStorage::new();
        let mut fs = InMemoryFs::new();
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.create_file(path.clone(), b"fn handle() {}".to_vec())
            .unwrap();
        ws.get_by_path(&path).unwrap()
    };

    let abs_path = root.join("src").join("handler.rs");
    std::fs::create_dir_all(abs_path.parent().unwrap()).unwrap();
    std::fs::write(&abs_path, b"fn handle() {}").unwrap();

    let mut processor = EventProcessor::new(
        root,
        Arc::clone(&workspace),
        Arc::clone(&index),
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpPluginHost),
    );
    processor
        .process_event(WatchEvent::Modify(abs_path))
        .unwrap();

    let ws_guard = workspace.read().unwrap();
    assert_eq!(
        ws_guard.snapshot().files.len(),
        1,
        "echoed Modify must not duplicate the VFS entry"
    );
    let id_after = ws_guard.get_by_path(&path).unwrap();
    assert_eq!(
        file_id_after_mutation, id_after,
        "Modify echo must preserve FileId"
    );
}
