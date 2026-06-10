//! Spec 09 TDD tests for `FileMutationService::global_replace`.
//!
//! # TDD sequence (from spec)
//!
//! 1. RED → GREEN: replace across N files — all rewritten + report count.
//! 2. RED → GREEN: partial-failure reporting (read-only / FS-failure file).
//! 3. RED → GREEN: dry-run — no files written, report shows intended changes.
//! 4. RED → GREEN: index freshness after replace + per-file crash atomicity.
//!
//! All domain unit tests use `InMemoryFs` + `InMemoryStorage` (no disk I/O).
//! Integration tests against `RealFs` live in `tests/integration_global_replace.rs`.
#![forbid(unsafe_code)]

use crate::adapters::{InMemoryFs, InMemoryStorage};
use crate::domain::grep::TextSearch;
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::FileMutationService;
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{ContentHash, FileId, RelativePath, VirtualFile};
use crate::ports::inbound::FileMutationUseCase;
use crate::ports::{FileSystemPort, NoOpPluginHost, PortError, StoragePort};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn path(s: &str) -> RelativePath {
    RelativePath::new(s)
}

/// Build workspace+index+storage+fs and seed a set of files.
///
/// Returns `(workspace, index, storage, fs)` ready for a `FileMutationService`.
fn setup_workspace_with_files(
    files: &[(&str, &[u8])],
) -> (ProjectWorkspace, InvertedIndex, InMemoryStorage, InMemoryFs) {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        for (p, content) in files {
            svc.create_file(path(p), content.to_vec()).unwrap();
        }
    }

    (ws, idx, storage, fs)
}

// ── TDD step 1: replace across N files — all rewritten + report count ─────────

/// AC1 — `foo` present in 5 files: all 5 rewritten, report shows 5 files_changed.
#[test]
fn ac1_replace_across_five_files_all_rewritten() {
    let files: Vec<(&str, &[u8])> = (0..5)
        .map(|i| {
            let name: &'static str = Box::leak(format!("src/file_{i}.rs").into_boxed_str());
            let content: &'static [u8] = Box::leak(
                format!("let x = foo; // file {i}")
                    .into_bytes()
                    .into_boxed_slice(),
            );
            (name, content)
        })
        .collect();

    let (mut ws, mut idx, mut storage, mut fs) = setup_workspace_with_files(&files);

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("foo", "bar").unwrap()
    };

    assert_eq!(
        report.files_changed, 5,
        "all 5 files must be reported as changed; got {report:#?}"
    );
    assert_eq!(
        report.replacements, 5,
        "1 replacement per file = 5 total; got {report:#?}"
    );
    assert!(
        report.errors.is_empty(),
        "no errors expected; got {:#?}",
        report.errors
    );

    // Verify on-disk content: every file must now contain "bar" not "foo".
    for i in 0..5 {
        let p = path(&format!("src/file_{i}.rs"));
        let bytes = fs.read(&p).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains("bar"),
            "file {i} must contain replacement 'bar'; got: {text:?}"
        );
        assert!(
            !text.contains("foo"),
            "file {i} must not contain original 'foo'; got: {text:?}"
        );
    }
}

/// Files that do NOT contain the target are not counted as changed.
#[test]
fn files_without_target_are_not_counted() {
    let (mut ws, mut idx, mut storage, mut fs) = setup_workspace_with_files(&[
        ("a.rs", b"let x = foo;"),
        ("b.rs", b"no target here"),
        ("c.rs", b"foo again"),
    ]);

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("foo", "bar").unwrap()
    };

    assert_eq!(report.files_changed, 2, "only 2 files contain 'foo'");
    assert_eq!(report.replacements, 2);
    assert!(report.errors.is_empty());
}

/// Multiple occurrences per file — replacement count is additive.
#[test]
fn multiple_occurrences_per_file_all_replaced() {
    let (mut ws, mut idx, mut storage, mut fs) =
        setup_workspace_with_files(&[("a.rs", b"foo foo foo")]);

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("foo", "bar").unwrap()
    };

    assert_eq!(report.files_changed, 1);
    assert_eq!(report.replacements, 3, "3 occurrences in one file");

    let bytes = fs.read(&path("a.rs")).unwrap();
    assert_eq!(bytes, b"bar bar bar");
}

/// Empty workspace — zero changes, no errors.
#[test]
fn empty_workspace_returns_zero_report() {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("foo", "bar").unwrap()
    };

    assert_eq!(report.files_changed, 0);
    assert_eq!(report.replacements, 0);
    assert!(report.errors.is_empty());
}

/// Pattern not found in any file — zero changes, no errors.
#[test]
fn pattern_not_found_returns_zero_report() {
    let (mut ws, mut idx, mut storage, mut fs) =
        setup_workspace_with_files(&[("a.rs", b"hello world"), ("b.rs", b"fn main() {}")]);

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("zzz", "ZZZ").unwrap()
    };

    assert_eq!(report.files_changed, 0);
    assert_eq!(report.replacements, 0);
    assert!(report.errors.is_empty());
}

// ── TDD step 2: partial-failure reporting ────────────────────────────────────

/// AC2 — one file fails to rewrite: others succeed, failure is reported.
///
/// Simulated by a `FileSystemPort` wrapper that rejects writes to a specific path.
#[test]
fn ac2_partial_failure_others_succeed_failure_reported() {
    // Adapter that fails writes to any path containing "read_only".
    struct FailReadOnly(InMemoryFs);

    impl FileSystemPort for FailReadOnly {
        fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
            self.0.read(path)
        }
        fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
            if path.as_str().contains("read_only") {
                Err(PortError::WriteFailed("permission denied".to_owned()))
            } else {
                self.0.write(path, bytes)
            }
        }
        fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
            self.0.rename(from, to)
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

    // Create the files via a plain FS first.
    let (mut ws, mut idx, mut storage, plain_fs) = setup_workspace_with_files(&[
        ("ok_file.rs", b"foo here"),
        ("read_only.rs", b"foo protected"),
        ("another_ok.rs", b"foo also"),
    ]);

    // Wrap with the failing adapter.
    let mut fail_fs = FailReadOnly(plain_fs);

    let report = {
        let mut svc = FileMutationService::new(
            &mut fail_fs,
            &mut ws,
            &mut idx,
            &mut storage,
            &NoOpPluginHost,
        );
        svc.global_replace("foo", "bar").unwrap()
    };

    // 2 files succeeded.
    assert_eq!(
        report.files_changed, 2,
        "2 writable files must be reported changed; got {report:#?}"
    );
    // 1 file failed.
    assert_eq!(
        report.errors.len(),
        1,
        "exactly 1 error expected; got {:#?}",
        report.errors
    );
    assert_eq!(report.errors[0].path, path("read_only.rs"));
    assert!(
        report.errors[0].reason.contains("permission denied"),
        "error reason must mention 'permission denied'; got {:?}",
        report.errors[0].reason
    );

    // Successful files were actually rewritten.
    let ok_bytes = fail_fs.0.read(&path("ok_file.rs")).unwrap();
    assert!(
        std::str::from_utf8(&ok_bytes).unwrap().contains("bar"),
        "ok_file.rs must be rewritten"
    );
    let another_bytes = fail_fs.0.read(&path("another_ok.rs")).unwrap();
    assert!(
        std::str::from_utf8(&another_bytes).unwrap().contains("bar"),
        "another_ok.rs must be rewritten"
    );

    // Errors are sorted by path for determinism.
    let mut sorted = report.errors.clone();
    sorted.sort();
    assert_eq!(report.errors, sorted, "errors must be sorted by path");
}

/// Errors are deterministically sorted by path.
#[test]
fn errors_are_sorted_by_path() {
    struct AlwaysFail(InMemoryFs);

    impl FileSystemPort for AlwaysFail {
        fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
            self.0.read(path)
        }
        fn write(&mut self, _path: RelativePath, _bytes: Vec<u8>) -> Result<(), PortError> {
            Err(PortError::WriteFailed("always fail".to_owned()))
        }
        fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
            self.0.rename(from, to)
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

    let (mut ws, mut idx, mut storage, plain_fs) = setup_workspace_with_files(&[
        ("z_file.rs", b"foo z"),
        ("a_file.rs", b"foo a"),
        ("m_file.rs", b"foo m"),
    ]);
    let mut fail_fs = AlwaysFail(plain_fs);

    let report = {
        let mut svc = FileMutationService::new(
            &mut fail_fs,
            &mut ws,
            &mut idx,
            &mut storage,
            &NoOpPluginHost,
        );
        svc.global_replace("foo", "bar").unwrap()
    };

    assert_eq!(report.errors.len(), 3, "all 3 files fail");

    let mut sorted = report.errors.clone();
    sorted.sort();
    assert_eq!(
        report.errors, sorted,
        "errors must be sorted lexicographically by path"
    );
}

// ── TDD step 3: dry-run — no writes ──────────────────────────────────────────

/// AC4 — dry-run: report shows intended changes, no file is modified.
#[test]
fn ac4_dry_run_returns_report_without_writing() {
    let (mut ws, mut idx, mut storage, mut fs) = setup_workspace_with_files(&[
        ("a.rs", b"foo first"),
        ("b.rs", b"foo second"),
        ("c.rs", b"no match"),
    ]);

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace_dry_run("foo", "bar").unwrap()
    };

    // Report must show what would change.
    assert_eq!(
        report.files_changed, 2,
        "dry-run must count files that would change"
    );
    assert_eq!(report.replacements, 2);
    assert!(report.errors.is_empty());

    // Files on FS must be unchanged.
    assert_eq!(fs.read(&path("a.rs")).unwrap(), b"foo first");
    assert_eq!(fs.read(&path("b.rs")).unwrap(), b"foo second");

    // VFS/workspace must be unchanged (same file count, same content).
    assert_eq!(
        ws.snapshot().files.len(),
        3,
        "VFS must be unchanged after dry-run"
    );
}

/// Dry-run on empty workspace returns zero report without error.
#[test]
fn dry_run_empty_workspace_returns_zero_report() {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let mut fs = InMemoryFs::new();

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace_dry_run("foo", "bar").unwrap()
    };

    assert_eq!(report.files_changed, 0);
    assert_eq!(report.replacements, 0);
    assert!(report.errors.is_empty());
}

// ── TDD step 4: index freshness after replace ─────────────────────────────────

/// AC5 — after `global_replace`, `search_text(old_target)` finds no stale matches.
///
/// Verifies that the live file content (the authoritative source for brute-force
/// grep) has actually been rewritten on the FS side, so a subsequent search
/// finds no occurrences of the replaced term.
#[test]
fn ac5_search_text_finds_no_stale_matches_after_replace() {
    let (mut ws, mut idx, mut storage, mut fs) = setup_workspace_with_files(&[
        ("a.rs", b"let x = oldterm;"),
        ("b.rs", b"fn f() { oldterm }"),
    ]);

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("oldterm", "newterm").unwrap();
    }

    // Brute-force content search must find no occurrences of the old term.
    let stale_matches = TextSearch::new(&ws, &fs).search("oldterm", None).unwrap();
    assert!(
        stale_matches.is_empty(),
        "no stale matches for 'oldterm' must remain after replace; got {stale_matches:#?}"
    );

    // New term is findable.
    let new_matches = TextSearch::new(&ws, &fs).search("newterm", None).unwrap();
    assert_eq!(
        new_matches.len(),
        2,
        "both files must contain 'newterm' after replace"
    );
}

/// VFS metadata is updated after a successful replace (EV2).
///
/// After `global_replace`, the VirtualFile entries for changed files must
/// still be present in the workspace (the files were not deleted or
/// replaced with new FileIds — paths are unchanged so FileIds are stable).
#[test]
fn ev2_vfs_entries_present_after_replace() {
    let (mut ws, mut idx, mut storage, mut fs) =
        setup_workspace_with_files(&[("a.rs", b"foo content"), ("b.rs", b"unrelated")]);

    let id_a_before = ws.get_by_path(&path("a.rs")).unwrap();
    let id_b_before = ws.get_by_path(&path("b.rs")).unwrap();

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("foo", "bar").unwrap();
    }

    // Both files must still be present in the VFS.
    let id_a_after = ws.get_by_path(&path("a.rs")).unwrap();
    let id_b_after = ws.get_by_path(&path("b.rs")).unwrap();

    // FileIds must be stable — content-only replace leaves paths unchanged.
    assert_eq!(
        id_a_before, id_a_after,
        "FileId for changed file must be stable"
    );
    assert_eq!(
        id_b_before, id_b_after,
        "FileId for unchanged file must be stable"
    );
}

/// TxReport `replacements` counts each occurrence, not each file.
#[test]
fn report_counts_each_occurrence_not_each_file() {
    let (mut ws, mut idx, mut storage, mut fs) = setup_workspace_with_files(&[
        ("a.rs", b"foo foo"),   // 2 occurrences
        ("b.rs", b"foo"),       // 1 occurrence
        ("c.rs", b"no target"), // 0 occurrences
    ]);

    let report = {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.global_replace("foo", "bar").unwrap()
    };

    assert_eq!(report.files_changed, 2, "2 files contain 'foo'");
    assert_eq!(report.replacements, 3, "total occurrences = 2 + 1 = 3");
}

// ── put_batch failure: FS writes durable, storage flush fails ────────────────

/// `StoragePort` wrapper whose `put_batch` always rejects.
///
/// All other methods delegate to the inner `InMemoryStorage` so that the
/// seed phase (which uses `put` not `put_batch`) works normally.
struct FailBatchStorage(InMemoryStorage);

impl StoragePort for FailBatchStorage {
    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.0.get(id)
    }
    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        self.0.put(file)
    }
    fn put_batch(&mut self, _files: &[VirtualFile]) -> Result<(), PortError> {
        Err(PortError::WriteFailed(
            "simulated put_batch failure".to_owned(),
        ))
    }
    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
        self.0.delete(id)
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

/// When `put_batch` fails after all FS shadow-file renames have completed,
/// `global_replace` must still return `Ok(TxReport)` — the FS writes are
/// durable and callers must receive the accurate `files_changed` count.
///
/// Returning `Err` here would cause callers to believe nothing happened and
/// re-run the replace, producing double replacements on disk.
#[test]
fn put_batch_failure_after_fs_writes_returns_ok_with_report() {
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut fail_storage = FailBatchStorage(InMemoryStorage::new());
    let mut fs = InMemoryFs::new();

    // Seed via `put` (which FailBatchStorage delegates through) — normal path.
    {
        let mut svc = FileMutationService::new(
            &mut fs,
            &mut ws,
            &mut idx,
            &mut fail_storage,
            &NoOpPluginHost,
        );
        svc.create_file(path("a.rs"), b"foo first".to_vec())
            .unwrap();
        svc.create_file(path("b.rs"), b"foo second".to_vec())
            .unwrap();
    }

    let report = {
        let mut svc = FileMutationService::new(
            &mut fs,
            &mut ws,
            &mut idx,
            &mut fail_storage,
            &NoOpPluginHost,
        );
        // Must return Ok even though put_batch fails.
        svc.global_replace("foo", "bar")
            .expect("global_replace must return Ok when put_batch fails after FS writes")
    };

    // The FS writes completed — report must reflect the actual replacements.
    assert_eq!(
        report.files_changed, 2,
        "both files were rewritten on the FS; report must show 2 changed"
    );
    assert_eq!(report.replacements, 2);

    // FS content must be the new text (the renames completed before put_batch).
    let a = fs.read(&path("a.rs")).unwrap();
    assert_eq!(a, b"bar first", "a.rs must contain replacement on FS");
    let b_content = fs.read(&path("b.rs")).unwrap();
    assert_eq!(
        b_content, b"bar second",
        "b.rs must contain replacement on FS"
    );
}

// ── AC3: per-file crash atomicity (implementation-level) ─────────────────────

/// `FileSystemPort` wrapper that panics on `rename` for a designated path.
///
/// Used to simulate a mid-sequence crash: `write` (to `.tmp_write`) completes
/// but `rename` never fires for the targeted path.
struct PanicOnRename {
    inner: InMemoryFs,
    /// The destination path whose rename should be intercepted.
    block_dst: String,
}

impl FileSystemPort for PanicOnRename {
    fn read(&self, p: &RelativePath) -> Result<Vec<u8>, PortError> {
        self.inner.read(p)
    }
    fn write(&mut self, p: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.write(p, bytes)
    }
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        if to.as_str() == self.block_dst {
            // Simulate a crash / hard failure: return an error without
            // completing the rename.  The `.tmp_write` file is still in the
            // map (write already succeeded); the destination is untouched.
            return Err(PortError::WriteFailed(
                "simulated crash during rename".to_owned(),
            ));
        }
        self.inner.rename(from, to)
    }
    fn delete(&mut self, p: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(p)
    }
    fn mkdir(&mut self, p: RelativePath) -> Result<(), PortError> {
        self.inner.mkdir(p)
    }
    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
}

/// AC3 — per-file crash atomicity of the implementation.
///
/// When `rename` fails for one file after `write` has already deposited
/// bytes into the shadow `.tmp_write` location, the destination file must
/// still contain its original content — never a torn or empty state.
///
/// This proves the shadow-file rename is the gate: old content is intact
/// until rename completes; the `.tmp_write` artifact is isolated.
#[test]
fn ac3_rename_failure_leaves_original_intact() {
    // Seed two files via the normal path so the workspace tracks them.
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let plain_fs = {
        let mut fs = InMemoryFs::new();
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpPluginHost);
        svc.create_file(path("safe.rs"), b"foo safe".to_vec())
            .unwrap();
        svc.create_file(path("guarded.rs"), b"foo guarded".to_vec())
            .unwrap();
        fs
    };

    // Wrap with the rename-blocking adapter targeting "guarded.rs".
    let mut crash_fs = PanicOnRename {
        inner: plain_fs,
        block_dst: "guarded.rs".to_owned(),
    };

    let report = {
        let mut svc = FileMutationService::new(
            &mut crash_fs,
            &mut ws,
            &mut idx,
            &mut storage,
            &NoOpPluginHost,
        );
        svc.global_replace("foo", "bar").unwrap()
    };

    // safe.rs succeeded; guarded.rs failed.
    assert_eq!(report.files_changed, 1, "only safe.rs must be committed");
    assert_eq!(report.errors.len(), 1, "guarded.rs must appear in errors");
    assert_eq!(report.errors[0].path, path("guarded.rs"));

    // safe.rs must contain the new content.
    let safe = crash_fs.inner.read(&path("safe.rs")).unwrap();
    assert_eq!(safe, b"bar safe", "safe.rs must be fully replaced");

    // guarded.rs must still contain the original content — rename never fired.
    let guarded = crash_fs.inner.read(&path("guarded.rs")).unwrap();
    assert_eq!(
        guarded, b"foo guarded",
        "guarded.rs must be fully original — rename was blocked before it could commit"
    );
}
