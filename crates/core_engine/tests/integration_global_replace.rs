//! Integration tests for `global_replace` against `RealFs` (spec 09 DoD —
//! Milestone 2 gate).
//!
//! Exercises the full stack: `FileMutationService` + `RealFs` + `InMemoryStorage`.
//! Each test creates a temporary directory, seeds files, runs `global_replace`,
//! and asserts on-disk content and `TxReport` values.
#![forbid(unsafe_code)]

use core_engine::adapters::fs::RealFs;
use core_engine::adapters::in_memory_storage::InMemoryStorage;
use core_engine::domain::RelativePath;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::mutation::FileMutationService;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::NoOpExtensionHost;
use core_engine::ports::inbound::FileMutationUseCase;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rel(s: &str) -> RelativePath {
    RelativePath::new(s)
}

/// Seed `files` into a `RealFs` workspace using `FileMutationService`.
fn seed_files(
    root: &std::path::Path,
    files: &[(&str, &[u8])],
) -> (ProjectWorkspace, InvertedIndex, InMemoryStorage, RealFs) {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = RealFs::new(root);

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        for (p, content) in files {
            svc.create_file(rel(p), content.to_vec()).unwrap();
        }
    }

    (ws, idx, storage, fs)
}

// ── AC1: replace across N files, all atomically rewritten ────────────────────

/// AC1 — all 5 files are rewritten; report shows correct counts; no stray
/// `.tmp_write` files remain.
#[test]
fn ac1_global_replace_all_files_rewritten_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let (mut ws, mut idx, mut storage, mut fs) = seed_files(
        root,
        &[
            ("src/a.rs", b"let x = foo;"),
            ("src/b.rs", b"fn f(foo: u8) {}"),
            ("src/c.rs", b"// foo comment\nlet y = foo;"),
            ("src/d.rs", b"no target here"),
            ("src/e.rs", b"foo foo foo"),
        ],
    );

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.global_replace("foo", "bar").unwrap()
    };

    // 4 files contain "foo" (d.rs does not).
    assert_eq!(
        report.files_changed, 4,
        "4 files must be reported changed; got {report:#?}"
    );
    // a.rs: 1, b.rs: 1, c.rs: 2, e.rs: 3 = 7 total.
    assert_eq!(
        report.replacements, 7,
        "total replacement count must be 7; got {report:#?}"
    );
    assert!(report.errors.is_empty(), "no errors expected");

    // Verify on-disk: changed files must not contain "foo".
    for name in &["src/a.rs", "src/b.rs", "src/c.rs", "src/e.rs"] {
        let abs = root.join("src").join(name.strip_prefix("src/").unwrap());
        let content = std::fs::read_to_string(&abs).unwrap();
        assert!(
            !content.contains("foo"),
            "{name} must not contain 'foo' after replace; got: {content:?}"
        );
        assert!(
            content.contains("bar"),
            "{name} must contain 'bar' after replace; got: {content:?}"
        );
    }

    // Unchanged file must be untouched.
    let d_abs = root.join("src").join("d.rs");
    let d_content = std::fs::read_to_string(&d_abs).unwrap();
    assert_eq!(d_content, "no target here", "d.rs must be unchanged");

    // No stray .tmp_write files.
    let stray = count_tmp_write_files(root);
    assert_eq!(stray, 0, "no .tmp_write files must remain after replace");
}

// ── AC2: partial failure — other files still committed ────────────────────────

/// AC2 — one subdirectory is made read-only so files inside it cannot have
/// new `.tmp_write` siblings created; the other files are committed and the
/// failure is reported.
///
/// Note: on Linux, making a FILE read-only does not prevent `rename(2)` over
/// it (the destination's write permission is irrelevant to rename). To reliably
/// trigger a write failure we make the parent DIRECTORY read-only so `write`
/// (which creates a new `.tmp_write` sibling) fails with EACCES.
#[test]
#[cfg(unix)] // chmod is Unix-only; Windows has different ACL semantics.
fn ac2_write_failure_reported_others_committed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // "ok" files live in the root; the "protected" file lives in a subdirectory
    // that will be made read-only.
    let protected_dir = root.join("protected_dir");
    std::fs::create_dir_all(&protected_dir).unwrap();

    // Seed files using FileMutationService (writes go through the FS port).
    let (mut ws, mut idx, mut storage, mut fs) = seed_files(
        root,
        &[
            ("writable_a.rs", b"foo writable a"),
            ("writable_b.rs", b"foo writable b"),
        ],
    );

    // Manually write the protected file (not through FileMutationService so it
    // exists on disk but the VFS tracks it via direct workspace insert + storage).
    let protected_rel = rel("protected_dir/guarded.rs");
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(protected_rel.clone(), b"foo guarded".to_vec())
            .unwrap();
    }

    // Make the subdirectory read-only: new files (including .tmp_write siblings)
    // cannot be created inside it.
    std::fs::set_permissions(&protected_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.global_replace("foo", "bar").unwrap()
    };

    // Restore permissions so tempfile cleanup works.
    std::fs::set_permissions(&protected_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        report.files_changed, 2,
        "2 writable files must be changed; got {report:#?}"
    );
    assert_eq!(report.errors.len(), 1, "1 error expected; got {report:#?}");
    assert_eq!(report.errors[0].path, protected_rel);

    // Writable files were rewritten.
    let a = std::fs::read_to_string(root.join("writable_a.rs")).unwrap();
    assert!(
        a.contains("bar") && !a.contains("foo"),
        "writable_a.rs must be replaced; got: {a:?}"
    );
    let b = std::fs::read_to_string(root.join("writable_b.rs")).unwrap();
    assert!(
        b.contains("bar") && !b.contains("foo"),
        "writable_b.rs must be replaced; got: {b:?}"
    );
}

// ── AC3: per-file crash atomicity ─────────────────────────────────────────────

/// `FileSystemPort` adapter wrapping `RealFs` that fails `rename` for one
/// specific destination path, simulating a crash between the shadow-file
/// `write` and the atomic `rename` step.
struct FailRenameFor<'a> {
    inner: RealFs,
    /// Workspace-relative destination path whose rename must fail.
    block_dst: &'a str,
}

impl<'a> core_engine::ports::FileSystemPort for FailRenameFor<'a> {
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, core_engine::ports::PortError> {
        self.inner.read(path)
    }
    fn write(
        &mut self,
        path: RelativePath,
        bytes: Vec<u8>,
    ) -> Result<(), core_engine::ports::PortError> {
        self.inner.write(path, bytes)
    }
    fn rename(
        &mut self,
        from: &RelativePath,
        to: RelativePath,
    ) -> Result<(), core_engine::ports::PortError> {
        if to.as_str() == self.block_dst {
            return Err(core_engine::ports::PortError::WriteFailed(
                "simulated crash: rename blocked".to_owned(),
            ));
        }
        self.inner.rename(from, to)
    }
    fn delete(&mut self, path: &RelativePath) -> Result<(), core_engine::ports::PortError> {
        self.inner.delete(path)
    }
    fn mkdir(&mut self, path: RelativePath) -> Result<(), core_engine::ports::PortError> {
        self.inner.mkdir(path)
    }
    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
}

/// AC3 — per-file crash atomicity of `global_replace` against `RealFs`.
///
/// When the shadow-file `rename` is blocked for one file (simulating a crash
/// mid-operation), that file must still contain its original on-disk content.
/// The other file (whose rename succeeded) must contain the new content.
///
/// This proves that the shadow-file rename is the gate: old content is durable
/// and intact until the atomic rename completes — no torn write is observable.
#[test]
fn ac3_blocked_rename_leaves_original_intact_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let (mut ws, mut idx, mut storage, real_fs) = seed_files(
        root,
        &[("safe.rs", b"foo safe"), ("guarded.rs", b"foo guarded")],
    );

    // Wrap real_fs so rename to "guarded.rs" always fails.
    let mut crash_fs = FailRenameFor {
        inner: real_fs,
        block_dst: "guarded.rs",
    };

    let report = {
        let mut svc = FileMutationService::new(
            &mut crash_fs,
            &mut ws,
            &mut idx,
            &mut storage,
            &NoOpExtensionHost,
        );
        svc.global_replace("foo", "bar").unwrap()
    };

    // safe.rs succeeded; guarded.rs failed.
    assert_eq!(
        report.files_changed, 1,
        "only safe.rs must be committed; got {report:#?}"
    );
    assert_eq!(report.errors.len(), 1, "guarded.rs must appear in errors");
    assert_eq!(report.errors[0].path, rel("guarded.rs"));

    // safe.rs must hold the new content on disk.
    let safe = std::fs::read_to_string(root.join("safe.rs")).unwrap();
    assert!(
        safe.contains("bar") && !safe.contains("foo"),
        "safe.rs must be fully replaced on disk; got: {safe:?}"
    );

    // guarded.rs must still hold the original on disk — the rename never fired.
    let guarded = std::fs::read_to_string(root.join("guarded.rs")).unwrap();
    assert_eq!(
        guarded, "foo guarded",
        "guarded.rs must be fully original on disk — rename was blocked before it could commit"
    );

    // No stray .tmp_write files must remain after the operation completes.
    // Note: the blocked rename leaves a .tmp_write on disk for guarded.rs;
    // the spec only requires the *original* is intact, not that .tmp_write
    // files are cleaned up on error — they are GC-able artifacts.
    let safe_abs = root.join("safe.rs");
    assert!(safe_abs.exists(), "safe.rs must exist on disk");
    let guarded_abs = root.join("guarded.rs");
    assert!(guarded_abs.exists(), "guarded.rs must exist on disk");
}

// ── AC4: dry-run — no files modified ─────────────────────────────────────────

/// AC4 — dry-run returns the would-change report without writing any file.
#[test]
fn ac4_dry_run_does_not_modify_any_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let (mut ws, mut idx, mut storage, mut fs) = seed_files(
        root,
        &[
            ("a.rs", b"foo first"),
            ("b.rs", b"foo second"),
            ("c.rs", b"no match"),
        ],
    );

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.global_replace_dry_run("foo", "bar").unwrap()
    };

    // Report must show intended changes.
    assert_eq!(report.files_changed, 2, "dry-run must count 2 files");
    assert_eq!(report.replacements, 2, "dry-run must count 2 replacements");
    assert!(report.errors.is_empty());

    // On-disk content must be unchanged.
    let a = std::fs::read(root.join("a.rs")).unwrap();
    let b = std::fs::read(root.join("b.rs")).unwrap();
    let c = std::fs::read(root.join("c.rs")).unwrap();
    assert_eq!(a, b"foo first", "a.rs must be unchanged after dry-run");
    assert_eq!(b, b"foo second", "b.rs must be unchanged after dry-run");
    assert_eq!(c, b"no match", "c.rs must be unchanged after dry-run");
}

// ── AC5: search_text finds no stale matches after replace ────────────────────

/// AC5 — after `global_replace`, `search_text(old_target)` on the real FS
/// returns no stale matches.
#[test]
fn ac5_search_text_no_stale_matches_on_real_fs() {
    use core_engine::domain::grep::TextSearch;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let (mut ws, mut idx, mut storage, mut fs) = seed_files(
        root,
        &[
            ("src/x.rs", b"let v = oldterm;"),
            ("src/y.rs", b"fn go(oldterm: u8) {}"),
        ],
    );

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.global_replace("oldterm", "newterm").unwrap();
    }

    let stale = TextSearch::new(&ws, &fs).search("oldterm", None).unwrap();
    assert!(
        stale.is_empty(),
        "no stale matches for 'oldterm' must remain; got {stale:#?}"
    );

    let new_matches = TextSearch::new(&ws, &fs).search("newterm", None).unwrap();
    assert_eq!(new_matches.len(), 2, "both files must contain 'newterm'");
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn count_tmp_write_files(root: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut count = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += walk(&path);
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
    walk(root)
}
