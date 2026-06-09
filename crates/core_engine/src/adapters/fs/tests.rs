//! Tests for `RealFs` (spec 05a).
//!
//! # TDD sequence
//!
//! 1. RED  : `filesystem_contract_tests!` wired against `RealFs` on a tempdir.
//! 2. GREEN: `RealFs` implementation passes all contract tests.
//! 3. RED  : rename atomicity, durable write, binary-safe read, path traversal.
//! 4. GREEN: all extra tests pass.

use tempfile::TempDir;

use crate::adapters::fs::RealFs;
use crate::{
    domain::RelativePath,
    filesystem_contract_tests,
    ports::{FileSystemPort, PortError},
};

/// Constructor used by the contract macro: each invocation gets a fresh RealFs
/// rooted in a new tempdir.  The TempDir value is stored inside the RealFs via
/// a wrapper so it is not dropped prematurely.
fn make_real_fs() -> (RealFs, TempDir) {
    let dir = TempDir::new().expect("tempdir creation failed");
    let fs = RealFs::new(dir.path());
    (fs, dir)
}

// ── AC1: spec-02 FileSystemPort contract suite on a real tempdir ─────────────
//
// The macro expects a constructor expression that returns an implementor.
// We use a newtype wrapper that owns both the RealFs and the TempDir so the
// temp directory is not deleted while the test runs.

struct TempRealFs {
    inner: RealFs,
    // Kept alive so the tempdir is not deleted during the test.
    _dir: TempDir,
}

impl crate::ports::FileSystemPort for TempRealFs {
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        self.inner.read(path)
    }
    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.write(path, bytes)
    }
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        self.inner.rename(from, to)
    }
    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(path)
    }
    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
}

filesystem_contract_tests!(|| {
    let dir = TempDir::new().expect("tempdir creation failed");
    let inner = RealFs::new(dir.path());
    TempRealFs { inner, _dir: dir }
});

// ── AC3: binary-safe read ─────────────────────────────────────────────────────

#[test]
fn read_binary_file_returns_raw_bytes_without_panic() {
    let (mut fs, _dir) = make_real_fs();
    // Bytes that are not valid UTF-8.
    let binary: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x80, 0x81, 0xBF];
    let path = RelativePath::new("data.bin");
    fs.write(path.clone(), binary.clone()).unwrap();
    let got = fs
        .read(&path)
        .expect("read must not panic on binary content");
    assert_eq!(got, binary);
}

// ── AC4: typed error on missing path ─────────────────────────────────────────

#[test]
fn read_missing_file_returns_not_found() {
    let (fs, _dir) = make_real_fs();
    let err = fs
        .read(&RelativePath::new("does_not_exist.rs"))
        .unwrap_err();
    assert_eq!(err, PortError::NotFound);
}

#[test]
fn rename_missing_source_returns_not_found() {
    let (mut fs, _dir) = make_real_fs();
    let err = fs
        .rename(
            &RelativePath::new("ghost.tmp"),
            RelativePath::new("ghost.rs"),
        )
        .unwrap_err();
    assert_eq!(err, PortError::NotFound);
}

#[test]
fn delete_missing_file_returns_not_found() {
    let (mut fs, _dir) = make_real_fs();
    let err = fs
        .delete(&RelativePath::new("never_existed.rs"))
        .unwrap_err();
    assert_eq!(err, PortError::NotFound);
}

// ── AC2: rename atomicity — dst holds bytes and src is gone ──────────────────

#[test]
fn rename_dst_holds_bytes_and_src_is_absent() {
    let (mut fs, _dir) = make_real_fs();
    let src = RelativePath::new("draft.tmp");
    let dst = RelativePath::new("draft.rs");
    let content = b"fn main() {}".to_vec();
    fs.write(src.clone(), content.clone()).unwrap();
    fs.rename(&src, dst.clone()).unwrap();
    assert_eq!(fs.read(&dst).unwrap(), content);
    assert_eq!(fs.read(&src).unwrap_err(), PortError::NotFound);
}

// ── Path traversal safety ─────────────────────────────────────────────────────

#[test]
fn read_with_traversal_path_returns_error() {
    let (fs, _dir) = make_real_fs();
    // "../../../etc/passwd" would escape the root.
    let err = fs
        .read(&RelativePath::new("../../../etc/passwd"))
        .unwrap_err();
    match err {
        PortError::ReadFailed(msg) => assert!(
            msg.contains("path traversal"),
            "expected 'path traversal' in error message, got: {msg}"
        ),
        other => panic!("expected ReadFailed, got {other:?}"),
    }
}

#[test]
fn write_with_traversal_path_returns_error() {
    let (mut fs, _dir) = make_real_fs();
    let err = fs
        .write(RelativePath::new("sub/../../escape.rs"), b"evil".to_vec())
        .unwrap_err();
    match err {
        PortError::WriteFailed(msg) => assert!(
            msg.contains("path traversal"),
            "expected 'path traversal' in error message, got: {msg}"
        ),
        other => panic!("expected WriteFailed, got {other:?}"),
    }
}

#[test]
fn nested_path_within_root_is_accepted() {
    let (mut fs, _dir) = make_real_fs();
    // A/B/../C resolves to A/C, which is still inside root.
    let path = RelativePath::new("a/b/../c.rs");
    fs.write(path.clone(), b"ok".to_vec()).unwrap();
    assert_eq!(fs.read(&path).unwrap(), b"ok");
}

// ── Durable write: temp-file rename pattern ───────────────────────────────────

#[test]
fn write_leaves_no_tmp_file_after_success() {
    let (mut fs, dir) = make_real_fs();
    let path = RelativePath::new("important.rs");
    fs.write(path, b"content".to_vec()).unwrap();
    // Glob for any `.~tmp` files under the root — should be empty.
    let tmp_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".~tmp"))
        .collect();
    assert!(
        tmp_files.is_empty(),
        "orphaned temp files found: {tmp_files:?}"
    );
}

// ── Subdirectory write: parent dirs created automatically ─────────────────────

#[test]
fn write_creates_parent_directories() {
    let (mut fs, _dir) = make_real_fs();
    let path = RelativePath::new("src/sub/module.rs");
    fs.write(path.clone(), b"mod foo;".to_vec()).unwrap();
    assert_eq!(fs.read(&path).unwrap(), b"mod foo;");
}

// ── Absolute-path injection: must be rejected as path traversal ───────────────
//
// A RelativePath whose string starts with '/' (POSIX absolute) must be
// rejected immediately.  If it were allowed, self.root.join("/etc/passwd")
// would silently discard the root and return /etc/passwd — a complete sandbox
// escape.  Component::RootDir is a no-op in the depth counter unless we
// explicitly handle it.

#[test]
fn read_absolute_path_is_rejected_as_path_traversal() {
    let (fs, _dir) = make_real_fs();
    let err = fs.read(&RelativePath::new("/etc/passwd")).unwrap_err();
    match err {
        PortError::ReadFailed(msg) => assert!(
            msg.contains("path traversal"),
            "expected 'path traversal' in error message, got: {msg}"
        ),
        other => panic!("expected ReadFailed, got {other:?}"),
    }
}

#[test]
fn write_absolute_path_is_rejected_as_path_traversal() {
    let (mut fs, _dir) = make_real_fs();
    let err = fs
        .write(RelativePath::new("/etc/passwd"), b"evil".to_vec())
        .unwrap_err();
    match err {
        PortError::WriteFailed(msg) => assert!(
            msg.contains("path traversal"),
            "expected 'path traversal' in error message, got: {msg}"
        ),
        other => panic!("expected WriteFailed, got {other:?}"),
    }
}
