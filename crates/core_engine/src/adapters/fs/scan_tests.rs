//! Integration tests for spec 05b — Initial Workspace Scan.
//!
//! # TDD sequence
//!
//! 1. RED  : AC1 — N eligible files → exactly N FileIds, all findable.
//! 2. GREEN: implement scan().
//! 3. RED  : AC2 — .gitignore excludes target/.
//! 4. GREEN.
//! 5. RED  : AC3 — unreadable file skipped, others indexed.
//!    AC4 — symlink loop terminates.
//! 6. GREEN.
//! 7. RED  : AC5 — restart skips rescan, search still works.
//! 8. GREEN.

#[cfg(test)]
mod scan_integration {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::adapters::fs::scan::{scan, scan_with_batch_size};
    use crate::adapters::storage::SledStorageAdapter;
    use crate::domain::index::{FileSearch, InvertedIndex};
    use crate::domain::workspace::ProjectWorkspace;
    use crate::ports::inbound::SearchUseCase;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Create a fresh sled-backed triple (adapter, workspace, index) in a
    /// dedicated tempdir.
    fn open_storage(db_dir: &Path) -> (SledStorageAdapter, ProjectWorkspace, InvertedIndex) {
        SledStorageAdapter::open(db_dir).expect("SledStorageAdapter::open")
    }

    /// Write `content` to `root/relative_path`, creating parent dirs as needed.
    fn write_file(root: &Path, relative_path: &str, content: &[u8]) {
        let full = root.join(relative_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    // ── AC1: N eligible files → exactly N distinct FileIds ───────────────────

    /// AC1 — given a tempdir tree with N eligible files, scan produces exactly
    /// N distinct FileIds and all are findable via find_file.
    #[test]
    fn ac1_n_eligible_files_produce_n_file_ids() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "src/main.rs", b"fn main() {}");
        write_file(workspace_dir.path(), "src/lib.rs", b"pub mod foo;");
        write_file(workspace_dir.path(), "README.md", b"# project");

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        assert_eq!(
            report.indexed, 3,
            "expected 3 indexed files; got {report:?}"
        );
        assert!(
            report.skipped_permission.is_empty(),
            "no permission errors expected; got {:?}",
            report.skipped_permission
        );
        assert!(!report.already_indexed);

        // All files findable via find_file.
        let search = FileSearch::new(&index, &workspace);

        let hits = search.find_file("main").unwrap();
        let paths: Vec<&str> = hits.iter().map(|p| p.as_str()).collect();
        assert!(
            paths.iter().any(|p| p.ends_with("main.rs")),
            "main.rs must be findable; got {paths:?}"
        );

        let hits2 = search.find_file("lib").unwrap();
        let paths2: Vec<&str> = hits2.iter().map(|p| p.as_str()).collect();
        assert!(
            paths2.iter().any(|p| p.ends_with("lib.rs")),
            "lib.rs must be findable; got {paths2:?}"
        );

        let hits3 = search.find_file("README").unwrap();
        let paths3: Vec<&str> = hits3.iter().map(|p| p.as_str()).collect();
        assert!(
            paths3.iter().any(|p| p.ends_with("README.md")),
            "README.md must be findable; got {paths3:?}"
        );
    }

    /// AC1 variant — FileIds are all distinct.
    #[test]
    fn ac1_file_ids_are_distinct() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        for i in 0..5u8 {
            write_file(
                workspace_dir.path(),
                &format!("src/module_{i}/file_{i}.rs"),
                b"// content",
            );
        }

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        assert_eq!(report.indexed, 5, "expected 5 indexed files; {report:?}");

        // Collect all FileIds from the workspace snapshot.
        let snapshot = workspace.snapshot();
        let ids: Vec<_> = snapshot.files.iter().map(|f| f.id).collect();
        let unique_ids: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            unique_ids.len(),
            "all FileIds must be distinct; ids={ids:?}"
        );
    }

    // ── AC2: .gitignore exclusion ─────────────────────────────────────────────

    /// AC2 — given a .gitignore excluding `target/`, scan must not index any
    /// file under target/.
    #[test]
    fn ac2_gitignore_excludes_target_directory() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        // .gitignore says: ignore target/
        write_file(workspace_dir.path(), ".gitignore", b"target/\n");

        write_file(workspace_dir.path(), "src/main.rs", b"fn main() {}");
        write_file(
            workspace_dir.path(),
            "target/debug/mybin",
            b"\x7fELF binary",
        );
        write_file(
            workspace_dir.path(),
            "target/release/mybin",
            b"\x7fELF binary",
        );

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        // Only main.rs should be indexed (.gitignore and target/ skipped).
        assert_eq!(
            report.indexed, 1,
            "only src/main.rs should be indexed; {report:?}"
        );

        let snapshot = workspace.snapshot();
        let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();

        assert!(
            !paths.iter().any(|p| p.contains("target")),
            "no target/ paths must be indexed; got {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("main.rs")),
            "src/main.rs must be indexed; got {paths:?}"
        );
    }

    /// AC2 variant — hidden files (dot-prefix) are skipped.
    #[test]
    fn ac2_hidden_files_are_skipped() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "visible.rs", b"fn foo() {}");
        write_file(workspace_dir.path(), ".hidden_config", b"secret=1");

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        assert_eq!(
            report.indexed, 1,
            "only visible.rs should be indexed; {report:?}"
        );

        let snapshot = workspace.snapshot();
        let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !paths.iter().any(|p| p.starts_with('.')),
            "hidden file must not be indexed; got {paths:?}"
        );
    }

    // ── AC3: permission error skipped, others still indexed ───────────────────

    /// AC3 — an unreadable file (permissions set to 0o000 on Unix) is skipped;
    /// other files are still indexed.
    ///
    /// On Linux, `stat(2)` on a regular file with mode 0o000 *succeeds* —
    /// file read/write permissions do not block metadata access. The ignore
    /// walker therefore enumerates and indexes the file normally, so we cannot
    /// assert `indexed == 1` here without running as root. What we do assert:
    ///
    /// 1. readable.rs is always indexed.
    /// 2. The scan does not panic or abort.
    ///
    /// The definitive AC3 probe for the skip-branch on Linux is
    /// `ac3_unreadable_directory_skipped_other_files_still_indexed` below,
    /// which uses a 0o000 *directory* — directory execute bits DO block
    /// `stat(2)` on entries inside, reliably triggering the skip code path.
    #[cfg(unix)]
    #[test]
    fn ac3_unreadable_file_skipped_others_indexed() {
        use std::os::unix::fs::PermissionsExt;

        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "readable.rs", b"fn ok() {}");
        write_file(workspace_dir.path(), "noperm.rs", b"fn secret() {}");

        // Remove all permissions from noperm.rs
        let noperm_path = workspace_dir.path().join("noperm.rs");
        fs::set_permissions(&noperm_path, fs::Permissions::from_mode(0o000)).unwrap();

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        // Restore permissions so TempDir cleanup succeeds.
        fs::set_permissions(&noperm_path, fs::Permissions::from_mode(0o644)).unwrap();

        // readable.rs must always be indexed; the scan must not panic.
        // (On Linux stat() on a 0o000 file still succeeds, so noperm.rs may
        // also appear in the index — that is correct platform behaviour.)
        assert!(
            report.indexed >= 1,
            "at least readable.rs must be indexed; {report:?}"
        );

        let snapshot = workspace.snapshot();
        let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.iter().any(|p| p.ends_with("readable.rs")),
            "readable.rs must be indexed; got {paths:?}"
        );
    }

    /// AC3 variant — a subdirectory with mode 0o000 reliably triggers the
    /// permission-skip code path on Linux. Directory execute bits block
    /// `stat(2)` on entries inside the directory, so the walker surfaces an
    /// I/O error for the directory itself (or its contents), which the scan
    /// must handle by skipping and continuing.
    ///
    /// This is the authoritative AC3/UN1 probe on Linux: it proves that the
    /// skip branch is actually taken and that `skipped_permission` is non-empty.
    #[cfg(unix)]
    #[test]
    fn ac3_unreadable_directory_skipped_other_files_still_indexed() {
        use std::os::unix::fs::PermissionsExt;

        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "top.rs", b"fn top() {}");
        write_file(workspace_dir.path(), "secret/hidden.rs", b"fn secret() {}");

        let secret_dir = workspace_dir.path().join("secret");
        fs::set_permissions(&secret_dir, fs::Permissions::from_mode(0o000)).unwrap();

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        // Restore permissions so TempDir cleanup works.
        fs::set_permissions(&secret_dir, fs::Permissions::from_mode(0o755)).unwrap();

        // Exactly 1 file (top.rs) must be indexed; hidden.rs is inside the
        // unreadable directory so it must not appear.
        assert_eq!(report.indexed, 1, "only top.rs must be indexed; {report:?}");

        // The skip code path must have been exercised: the permission error on
        // the secret/ directory must appear in skipped_permission.
        assert!(
            !report.skipped_permission.is_empty(),
            "skipped_permission must be non-empty (directory error); {report:?}"
        );

        let snapshot = workspace.snapshot();
        let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.iter().any(|p| p.ends_with("top.rs")),
            "top.rs must be indexed; got {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.ends_with("hidden.rs")),
            "hidden.rs must NOT be indexed (inside unreadable dir); got {paths:?}"
        );
    }

    // ── AC4: symlink loop terminates ──────────────────────────────────────────

    /// AC4 — a symlink pointing back to an ancestor directory must not cause an
    /// infinite loop. The scan must terminate.
    #[cfg(unix)]
    #[test]
    fn ac4_symlink_loop_terminates() {
        use std::os::unix::fs::symlink;

        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "normal.rs", b"fn x() {}");

        // Create a symlink loop: workspace/loop_link → workspace/
        let link_path = workspace_dir.path().join("loop_link");
        symlink(workspace_dir.path(), &link_path).unwrap();

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());

        // This must terminate — the ignore walker does not follow symlinks.
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        // normal.rs must be indexed.
        assert!(report.indexed >= 1, "normal.rs must be indexed; {report:?}");
        // The scan must have returned (not hung).
    }

    // ── AC5: restart skips rescan, search still works ─────────────────────────

    /// AC5 — after a completed scan the indexed set is persisted; reopening
    /// the engine must not rescan, and search must still work.
    #[test]
    fn ac5_restart_skips_rescan_and_search_still_works() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "src/alpha.rs", b"fn alpha() {}");
        write_file(workspace_dir.path(), "src/beta.rs", b"fn beta() {}");

        // First scan.
        {
            let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
            let report = scan(
                workspace_dir.path(),
                &mut storage,
                &mut workspace,
                &mut index,
            )
            .unwrap();
            assert_eq!(
                report.indexed, 2,
                "first scan must index 2 files; {report:?}"
            );
            assert!(!report.already_indexed);
        }

        // Second open: workspace is reconstructed from sled; scan must skip.
        {
            let (mut storage2, mut workspace2, mut index2) = open_storage(db_dir.path());

            let report2 = scan(
                workspace_dir.path(),
                &mut storage2,
                &mut workspace2,
                &mut index2,
            )
            .unwrap();

            assert!(
                report2.already_indexed,
                "second scan must detect populated workspace and skip; {report2:?}"
            );

            // Search must still work from the persisted index.
            let search = FileSearch::new(&index2, &workspace2);

            let hits_alpha = search.find_file("alpha").unwrap();
            let alpha_paths: Vec<&str> = hits_alpha.iter().map(|p| p.as_str()).collect();
            assert!(
                alpha_paths.iter().any(|p| p.ends_with("alpha.rs")),
                "alpha.rs must be findable after restart; got {alpha_paths:?}"
            );

            let hits_beta = search.find_file("beta").unwrap();
            let beta_paths: Vec<&str> = hits_beta.iter().map(|p| p.as_str()).collect();
            assert!(
                beta_paths.iter().any(|p| p.ends_with("beta.rs")),
                "beta.rs must be findable after restart; got {beta_paths:?}"
            );
        }
    }

    /// AC5 variant — adding a new file to the workspace directory AFTER the
    /// first scan and then restarting must NOT index the new file (no rescan).
    #[test]
    fn ac5_new_file_added_after_scan_not_indexed_without_rescan() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "original.rs", b"fn a() {}");

        // First scan.
        {
            let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
            scan(
                workspace_dir.path(),
                &mut storage,
                &mut workspace,
                &mut index,
            )
            .unwrap();
        }

        // Add a new file after the scan.
        write_file(workspace_dir.path(), "added_later.rs", b"fn b() {}");

        // Reopen — must not rescan.
        let (mut storage2, mut workspace2, mut index2) = open_storage(db_dir.path());
        let report2 = scan(
            workspace_dir.path(),
            &mut storage2,
            &mut workspace2,
            &mut index2,
        )
        .unwrap();

        assert!(
            report2.already_indexed,
            "second open must skip scan; {report2:?}"
        );

        // added_later.rs must NOT be indexed (no rescan happened).
        let search = FileSearch::new(&index2, &workspace2);
        let hits = search.find_file("added_later").unwrap();
        assert!(
            hits.is_empty(),
            "added_later.rs must not appear (no rescan); got {hits:?}"
        );
    }

    // ── Batching (OP1) ────────────────────────────────────────────────────────

    /// OP1 — batching: scan with batch_size=1 (flush per file) produces the
    /// same final result as the default batch_size.
    #[test]
    fn op1_batching_with_size_one_produces_correct_result() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        for i in 0..4u8 {
            write_file(
                workspace_dir.path(),
                &format!("module_{i}.rs"),
                b"fn f() {}",
            );
        }

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan_with_batch_size(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
            1, // flush per file
        )
        .unwrap();

        assert_eq!(
            report.indexed, 4,
            "must index 4 files with batch_size=1; {report:?}"
        );

        let snapshot = workspace.snapshot();
        assert_eq!(snapshot.files.len(), 4, "workspace must hold 4 files");
    }

    /// OP1 — batching: scan with batch_size > number of files (no mid-scan flush)
    /// must still produce the correct final result.
    #[test]
    fn op1_batching_with_large_batch_no_mid_flush() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        for i in 0..3u8 {
            write_file(workspace_dir.path(), &format!("file_{i}.rs"), b"// hi");
        }

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan_with_batch_size(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
            1000, // larger than file count — no mid-scan flush
        )
        .unwrap();

        assert_eq!(report.indexed, 3, "3 files with large batch; {report:?}");
    }

    // ── .git directory is always skipped ──────────────────────────────────────

    /// The ignore walker always skips .git/ regardless of any .gitignore entry.
    #[test]
    fn git_directory_is_always_skipped() {
        let workspace_dir = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        write_file(workspace_dir.path(), "src/main.rs", b"fn main() {}");
        // Simulate a .git directory with a file inside.
        write_file(workspace_dir.path(), ".git/config", b"[core]");
        write_file(workspace_dir.path(), ".git/HEAD", b"ref: refs/heads/main");

        let (mut storage, mut workspace, mut index) = open_storage(db_dir.path());
        let report = scan(
            workspace_dir.path(),
            &mut storage,
            &mut workspace,
            &mut index,
        )
        .unwrap();

        // Only src/main.rs — .git and its contents are hidden (dot-prefix).
        assert_eq!(report.indexed, 1, "only src/main.rs indexed; {report:?}");

        let snapshot = workspace.snapshot();
        let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !paths.iter().any(|p| p.contains(".git")),
            "no .git paths must appear; got {paths:?}"
        );
    }
}
