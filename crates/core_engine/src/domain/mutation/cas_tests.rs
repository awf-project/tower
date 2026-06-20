//! Spec 18 TDD tests — optimistic compare-and-swap writes.
//!
//! # TDD sequence (from spec)
//!
//! 1. RED → GREEN: read_file with `with_version` returns stable hex hash;
//!    without → no version key.
//! 2. RED → GREEN: matching `expected_version` → commit (AC1).
//! 3. RED → GREEN: stale `expected_version` → VersionConflict, no write (AC2).
//! 4. RED → GREEN: omitted → unconditional (legacy, AC3).
//! 5. RED → GREEN: two CAS writers, one winner (AC5).
//! 6. RED → GREEN: global_replace per-file conflict in TxReport (AC6).
//!
//! All tests use `InMemoryFs` + `InMemoryStorage` — zero disk I/O.
#![forbid(unsafe_code)]

use crate::adapters::{InMemoryFs, InMemoryStorage};
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::{FileMutationService, compute_content_version};
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, RelativePath};
use crate::ports::inbound::FileMutationUseCase;
use crate::ports::{FileSystemPort, NoOpExtensionHost};

// ── Helper ────────────────────────────────────────────────────────────────────

fn make_state_with_file(
    path: &RelativePath,
    content: &[u8],
) -> (InMemoryFs, ProjectWorkspace, InvertedIndex, InMemoryStorage) {
    let mut fs = InMemoryFs::new();
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(path.clone(), content.to_vec()).unwrap();
    }
    (fs, ws, idx, storage)
}

// ── TDD step 1: compute_content_version is stable and deterministic ───────────

/// The hash helper produces a 64-char lowercase hex string and is pure
/// (same bytes → same hash, always).
#[test]
fn compute_content_version_is_stable_hex_sha256() {
    let bytes = b"fn main() {}";
    let v1 = compute_content_version(bytes);
    let v2 = compute_content_version(bytes);
    assert_eq!(v1, v2, "hash must be deterministic");
    assert_eq!(v1.len(), 64, "SHA-256 hex must be 64 characters");
    assert!(
        v1.chars().all(|c| c.is_ascii_hexdigit()),
        "hash must be lowercase hex; got {v1}"
    );
    // Different content → different hash.
    let other = compute_content_version(b"different");
    assert_ne!(v1, other, "distinct content must produce distinct hashes");
}

/// Empty byte slice has a stable SHA-256 hash (well-known constant).
#[test]
fn compute_content_version_empty_bytes_is_stable() {
    // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    let hash = compute_content_version(b"");
    assert_eq!(
        hash,
        "e3b0c44298fc1c149afbf4c8996fb924\
         27ae41e4649b934ca495991b7852b855"
    );
}

// ── TDD step 2: matching expected_version → commit (AC1) ──────────────────────

/// AC1: read hash v0, edit_range_cas with expected_version=v0 on unchanged
/// file → commits successfully and content is updated.
#[test]
fn edit_range_cas_matching_version_commits() {
    let path = RelativePath::new("src/main.rs");
    let (mut fs, mut ws, mut idx, mut storage) = make_state_with_file(&path, b"fn main() {}");

    // Compute v0 from the bytes the FS holds.
    let v0 = compute_content_version(&fs.read(&path).unwrap());

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    let report = svc
        .edit_range_cas(&path, 3, 7, "run", Some(v0))
        .expect("CAS with correct version must commit");

    assert_eq!(report.files_changed, 1);
    assert_eq!(report.replacements, 1);
    assert!(report.errors.is_empty());

    let bytes = fs.read(&path).unwrap();
    assert_eq!(bytes, b"fn run() {}");
}

// ── TDD step 3: stale expected_version → VersionConflict, no write (AC2) ──────

/// AC2: A reads v0; B writes (now v1); A mutates with expected_version=v0 →
/// VersionConflict{expected:v0, actual:v1}, file stays at B's content.
#[test]
fn edit_range_cas_stale_version_returns_conflict_and_no_write() {
    let path = RelativePath::new("src/shared.rs");
    let (mut fs, mut ws, mut idx, mut storage) = make_state_with_file(&path, b"fn original() {}");

    // A reads v0.
    let v0 = compute_content_version(b"fn original() {}");

    // B writes — now the file has new content.
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(path.clone(), b"fn modified_by_b() {}".to_vec())
            .unwrap();
    }

    // A tries to mutate with stale v0.
    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    let err = svc
        .edit_range_cas(&path, 0, 0, "// prepend", Some(v0.clone()))
        .unwrap_err();

    match err {
        DomainError::VersionConflict { expected, actual } => {
            assert_eq!(expected, v0, "conflict must report the expected hash");
            let v1 = compute_content_version(b"fn modified_by_b() {}");
            assert_eq!(actual, v1, "conflict must report the actual hash");
        }
        other => panic!("expected VersionConflict, got {other:?}"),
    }

    // File must still hold B's content — no partial write from A.
    let bytes = fs.read(&path).unwrap();
    assert_eq!(bytes, b"fn modified_by_b() {}");
}

/// AC3: Mutation with no expected_version → overwrites unconditionally.
/// (Legacy behaviour preserved — U2.)
#[test]
fn edit_range_cas_no_expected_version_is_unconditional() {
    let path = RelativePath::new("src/unconditional.rs");
    let (mut fs, mut ws, mut idx, mut storage) = make_state_with_file(&path, b"fn old() {}");

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    // No expected_version → must succeed even if content changed.
    let report = svc
        .edit_range_cas(&path, 3, 6, "new", None)
        .expect("unconditional (no expected_version) must always commit");

    assert_eq!(report.files_changed, 1);
    let bytes = fs.read(&path).unwrap();
    assert_eq!(bytes, b"fn new() {}");
}

// ── TDD step 4: create_file_cas ───────────────────────────────────────────────

/// AC1 variant for create_file_cas: matching version → commit.
#[test]
fn create_file_cas_matching_version_commits() {
    let path = RelativePath::new("src/cas_create.rs");
    let (mut fs, mut ws, mut idx, mut storage) = make_state_with_file(&path, b"v1");

    let v1 = compute_content_version(b"v1");

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    svc.create_file_cas(path.clone(), b"v2".to_vec(), Some(v1))
        .expect("CAS create_file with correct version must commit");

    assert_eq!(fs.read(&path).unwrap(), b"v2");
}

/// AC2 variant for create_file_cas: stale version → VersionConflict, no write.
#[test]
fn create_file_cas_stale_version_returns_conflict() {
    let path = RelativePath::new("src/cas_stale_create.rs");
    let (mut fs, mut ws, mut idx, mut storage) = make_state_with_file(&path, b"original");

    let stale = compute_content_version(b"some_old_content");

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    let err = svc
        .create_file_cas(path.clone(), b"new".to_vec(), Some(stale))
        .unwrap_err();

    assert!(
        matches!(err, DomainError::VersionConflict { .. }),
        "stale create_file_cas must return VersionConflict; got {err:?}"
    );
    // File unchanged.
    assert_eq!(fs.read(&path).unwrap(), b"original");
}

/// U2: create_file_cas with no expected_version is unconditional.
#[test]
fn create_file_cas_no_version_is_unconditional() {
    let path = RelativePath::new("src/cas_no_ver.rs");
    let (mut fs, mut ws, mut idx, mut storage) = make_state_with_file(&path, b"v1");

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    svc.create_file_cas(path.clone(), b"v2".to_vec(), None)
        .expect("unconditional must succeed");

    assert_eq!(fs.read(&path).unwrap(), b"v2");
}

/// A caller that supplies `expected_version` for a path that no longer exists
/// gets a `VersionConflict` (the version they read is gone), not `NotFound`.
/// The `actual` field is the empty string — an unambiguous "file absent"
/// sentinel, since a real SHA-256 hash is always 64 hex characters.
#[test]
fn create_file_cas_absent_file_with_expected_version_is_conflict() {
    let path = RelativePath::new("src/never_created.rs");
    let mut fs = InMemoryFs::new();
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();

    let expected = compute_content_version(b"what the caller thinks is there");

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    let err = svc
        .create_file_cas(path.clone(), b"new".to_vec(), Some(expected.clone()))
        .unwrap_err();

    match err {
        DomainError::VersionConflict {
            expected: e,
            actual,
        } => {
            assert_eq!(e, expected);
            assert_eq!(actual, "", "absent file must report an empty actual hash");
        }
        other => panic!("expected VersionConflict for absent file, got {other:?}"),
    }
    // No file was created on the conflict.
    assert!(
        fs.read(&path).is_err(),
        "no file must be created on conflict"
    );
}

/// Without `expected_version`, `create_file_cas` on an absent path simply
/// creates the file (unconditional — U2 preserved for the create case).
#[test]
fn create_file_cas_absent_file_without_version_creates() {
    let path = RelativePath::new("src/fresh.rs");
    let mut fs = InMemoryFs::new();
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    svc.create_file_cas(path.clone(), b"created".to_vec(), None)
        .expect("unconditional create on absent path must succeed");

    assert_eq!(fs.read(&path).unwrap(), b"created");
}

// ── TDD step 5: two CAS writers, exactly one wins (AC5) ───────────────────────

/// AC5: Two writers that both hold v0 race to write. The first commits; the
/// second sees VersionConflict. Exactly one winner.
///
/// In real concurrent usage the write-lock serialises the two calls;
/// here we simulate that ordering by calling them sequentially with the same
/// stale v0.
#[test]
fn cas_exactly_one_winner_when_two_writers_hold_same_version() {
    let path = RelativePath::new("src/race.rs");
    let (mut fs, mut ws, mut idx, mut storage) = make_state_with_file(&path, b"shared");

    let v0 = compute_content_version(b"shared");

    // Writer A commits with v0 → succeeds.
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file_cas(path.clone(), b"writer_a".to_vec(), Some(v0.clone()))
            .expect("writer A must win");
    }

    // Writer B tries the same v0 but the file has already changed → conflict.
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        let err = svc
            .create_file_cas(path.clone(), b"writer_b".to_vec(), Some(v0))
            .unwrap_err();
        assert!(
            matches!(err, DomainError::VersionConflict { .. }),
            "writer B must get VersionConflict; got {err:?}"
        );
    }

    // Final content must be A's write.
    assert_eq!(fs.read(&path).unwrap(), b"writer_a");
}

// ── TDD step 6: global_replace_cas — per-file conflict in TxReport (AC6) ──────

/// AC6: global_replace_cas where one file drifted → that file in TxReport.errors;
/// non-conflicting files still commit.
#[test]
fn global_replace_cas_per_file_conflict_in_tx_report() {
    use std::collections::HashMap;

    let mut fs = InMemoryFs::new();
    let mut ws = ProjectWorkspace::new();
    let mut idx = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();

    let path_a = RelativePath::new("src/a.rs");
    let path_b = RelativePath::new("src/b.rs");

    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(path_a.clone(), b"let x = foo;".to_vec())
            .unwrap();
        svc.create_file(path_b.clone(), b"let y = foo;".to_vec())
            .unwrap();
    }

    // Both callers read v0 for file_b; then b is modified externally.
    let v0_b = compute_content_version(b"let y = foo;");

    // Simulate b being modified after the read (it now has different content).
    {
        let mut svc =
            FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
        svc.create_file(path_b.clone(), b"let y = BAR;".to_vec())
            .unwrap();
    }

    // Now run global_replace_cas: a has no guard (no version), b has stale guard.
    let mut expected_versions = HashMap::new();
    expected_versions.insert(path_b.clone(), v0_b);

    let mut svc =
        FileMutationService::new(&mut fs, &mut ws, &mut idx, &mut storage, &NoOpExtensionHost);
    let report = svc
        .global_replace_cas("foo", "bar", expected_versions)
        .expect("global_replace_cas must return Ok (partial failure, not Err)");

    // File a must have been rewritten.
    assert_eq!(report.files_changed, 1, "only a (unguarded) must commit");
    assert_eq!(fs.read(&path_a).unwrap(), b"let x = bar;");

    // File b must be in errors (conflict).
    assert_eq!(report.errors.len(), 1, "b must be in TxReport::errors");
    assert_eq!(report.errors[0].path, path_b, "error must name file b");
    // b's content must be unchanged (B's write preserved).
    assert_eq!(fs.read(&path_b).unwrap(), b"let y = BAR;");
}
