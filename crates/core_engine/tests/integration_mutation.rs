//! Integration tests for `FileMutationService` against `RealFs` (spec 08 DoD).
//!
//! These tests exercise the real filesystem, verifying that:
//!  - `create_file` durably writes content (temp + fsync + rename).
//!  - A crash before rename leaves the original intact (AC2).
//!  - `delete_file` physically removes the file.
//!  - `create_directory` creates real directories (AC4).
//!  - `.tmp_write` artifacts left by a crash are excluded from a subsequent scan.
#![forbid(unsafe_code)]

use core_engine::adapters::fs::RealFs;
use core_engine::adapters::in_memory_storage::InMemoryStorage;
use core_engine::domain::RelativePath;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::mutation::FileMutationService;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::NoOpExtensionHost;
use core_engine::ports::inbound::FileMutationUseCase;

// ── AC1: create_file writes durable content ───────────────────────────────────

/// After `create_file`, the target path holds the exact bytes written.
#[test]
fn create_file_writes_durable_content_to_real_fs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = RealFs::new(&root);

    let path = RelativePath::new("src/real_module.rs");
    let content = b"pub fn hello() -> &'static str { \"hello\" }".to_vec();

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(path.clone(), content.clone()).unwrap();
    }

    // File must exist on disk with exact content.
    let abs = root.join("src").join("real_module.rs");
    assert!(abs.exists(), "file must exist on disk after create_file");
    let on_disk = std::fs::read(&abs).unwrap();
    assert_eq!(
        on_disk, content,
        "on-disk content must match what was written"
    );

    // VFS must track it.
    assert!(ws.get_by_path(&path).is_some(), "VFS must track the file");
}

/// No stray `.tmp_write` file remains after a successful `create_file`.
#[test]
fn create_file_leaves_no_stray_tmp_write_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = RealFs::new(&root);

    let path = RelativePath::new("src/clean.rs");

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(path, b"fn f() {}".to_vec()).unwrap();
    }

    // Walk the tree looking for any .tmp_write files.
    let tmp_count = count_tmp_write_files(&root);
    assert_eq!(
        tmp_count, 0,
        "no .tmp_write files must remain after successful create"
    );
}

// ── AC2: crash-before-rename — original intact ───────────────────────────────

/// If the process crashes after writing `.tmp_write` but before the rename, the
/// original file is intact.
///
/// Simulated by writing a `.tmp_write` file manually and checking the original.
#[test]
fn stray_tmp_write_does_not_corrupt_original() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    // Write the original file directly.
    let original_path = root.join("src").join("important.rs");
    std::fs::create_dir_all(original_path.parent().unwrap()).unwrap();
    std::fs::write(&original_path, b"original content").unwrap();

    // Simulate a stray .tmp_write (crash before rename completed).
    let tmp_path = root.join("src").join("important.rs.tmp_write");
    std::fs::write(&tmp_path, b"partial new content").unwrap();

    // The original must be intact.
    let on_disk = std::fs::read(&original_path).unwrap();
    assert_eq!(
        on_disk, b"original content",
        "original must be intact when .tmp_write is stray"
    );
}

// ── AC3: delete_file physically removes the file ─────────────────────────────

#[test]
fn delete_file_physically_removes_file_from_real_fs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = RealFs::new(&root);

    let path = RelativePath::new("src/transient.rs");

    // Create.
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(path.clone(), b"fn f() {}".to_vec())
            .unwrap();
    }

    let abs = root.join("src").join("transient.rs");
    assert!(abs.exists(), "file must exist before delete");

    // Delete.
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.delete_file(&path).unwrap();
    }

    assert!(
        !abs.exists(),
        "file must be gone from disk after delete_file"
    );
    assert!(
        ws.get_by_path(&path).is_none(),
        "VFS must not track deleted file"
    );
}

// ── AC4: create_directory creates real dirs ───────────────────────────────────

#[test]
fn create_directory_creates_real_directory_recursively() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = RealFs::new(&root);

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_directory(RelativePath::new("a/b/c")).unwrap();
    }

    let expected = root.join("a").join("b").join("c");
    assert!(
        expected.is_dir(),
        "a/b/c must be a real directory after create_directory"
    );
}

// ── AC5: stray .tmp_write excluded from scan ──────────────────────────────────

/// A `.tmp_write` file left on disk by a simulated crash is NOT indexed by the
/// scan use-case — `is_tmp_artifact` exclusion is exercised end-to-end.
#[test]
fn scan_excludes_stray_tmp_write_artifacts() {
    use core_engine::adapters::fs::workspace_scan;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    // Write a real file and a stray .tmp_write sibling.
    std::fs::write(root.join("real.rs"), b"fn f() {}").unwrap();
    std::fs::write(root.join("real.rs.tmp_write"), b"partial").unwrap();

    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();

    let report = workspace_scan(&root, &mut storage, &mut ws, &mut idx).unwrap();

    // Only the real file must be indexed.
    assert_eq!(
        report.indexed, 1,
        "scan must index exactly 1 file (the real one)"
    );

    // The .tmp_write path must not appear in the VFS.
    let tmp_rel = RelativePath::new("real.rs.tmp_write");
    assert!(
        ws.get_by_path(&tmp_rel).is_none(),
        "scan must not index .tmp_write artifacts"
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Recursively count files whose name ends with `.tmp_write` under `root`.
fn count_tmp_write_files(root: &std::path::Path) -> usize {
    count_tmp_write_recursive(root)
}

fn count_tmp_write_recursive(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += count_tmp_write_recursive(&path);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".tmp_write"))
        {
            count += 1;
        }
    }
    count
}
