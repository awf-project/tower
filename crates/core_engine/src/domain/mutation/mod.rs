//! `FileMutationUseCase` implementation — spec 08 and spec 18 (CAS).
//!
//! # Wireframe
//!
//! ```text
//!  create_file(path, content):
//!    tmp = path + ".tmp_write"
//!    FileSystemPort.write(tmp, content)   ← durable bytes on disk / in-memory store
//!    FileSystemPort.rename(tmp, path)     ← OS-atomic: original never half-written
//!         │
//!         ├─ workspace.insert / update    ┐
//!         ├─ index.insert / delta-reindex ├─ VFS + index in sync
//!         ├─ storage.put                  ┘
//!         └─ extension_host.on_file_changed  ← broadcast after commit
//!
//!  delete_file(path):
//!    resolve FileId from workspace
//!    workspace.remove + index.remove  ← clear in-memory state
//!    FileSystemPort.delete(path)      ← physical remove first (crash-safe)
//!    storage.delete                   ← persist removal after physical delete
//!    extension_host.on_file_changed
//!
//!  create_directory(path):
//!    FileSystemPort.mkdir(path)    ← recursive
//! ```
//!
//! # Crash safety (UN1/AC2)
//!
//! `create_file` writes to a sibling `.tmp_write` file before renaming it over
//! the target. If the process crashes between the write and the rename, the
//! target is untouched and the stray `.tmp_write` is safely ignorable (AC5).
//! If the process crashes after the rename, the new content is fully present.
//!
//! # Watcher idempotency (UN3/AC6)
//!
//! After a successful `create_file`, the watcher will deliver a `Create` or
//! `Modify` event for the same path. `EventProcessor::handle_create` already
//! guards against duplicate insertion via `ws.get_by_path` and returns `Ok(())`
//! when the path is already tracked. `handle_modify` updates metadata in-place
//! preserving the `FileId`. No extra suppression layer is needed; the
//! idempotency is structural.
//!
//! # Temp-artifact exclusion (UN2/AC5)
//!
//! The `.tmp_write` suffix is filtered in both the scan walker and the watcher
//! via [`crate::domain::mutation::is_tmp_artifact`]. Neither the scanner nor the
//! watcher will ever index a `.tmp_write` file as a real user file.
#![forbid(unsafe_code)]

mod file_mutation;

pub use file_mutation::FileMutationService;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cas_tests;

#[cfg(test)]
mod apply_edits_tests;

/// Write `content` to `path` using the spec-08 shadow-file atomic pattern.
///
/// 1. Write to `<path>.tmp_write` via the FS port.
/// 2. Atomically rename `<path>.tmp_write` → `path`.
///
/// This is the single canonical implementation of the shadow-file primitive.
/// Both [`FileMutationService::create_file`] (spec 08) and
/// [`crate::domain::refactor::GlobalReplaceService`] (spec 09) delegate here
/// so any change to the suffix or fsync policy applies everywhere at once.
///
/// # Errors
///
/// Returns a human-readable `String` on any FS failure so callers can
/// aggregate it into a per-file error without leaking port types.
///
/// # Examples
///
/// ```rust
/// use core_engine::adapters::InMemoryFs;
/// use core_engine::domain::RelativePath;
/// use core_engine::domain::mutation::atomic_write;
/// use core_engine::ports::FileSystemPort;
///
/// let mut fs = InMemoryFs::new();
/// let path = RelativePath::new("src/main.rs");
/// atomic_write(&mut fs, &path, b"fn main() {}".to_vec()).unwrap();
/// assert_eq!(fs.read(&path).unwrap(), b"fn main() {}");
/// ```
pub fn atomic_write(
    fs: &mut dyn crate::ports::FileSystemPort,
    path: &crate::domain::RelativePath,
    content: Vec<u8>,
) -> Result<(), String> {
    let tmp_path = crate::domain::RelativePath::new(format!("{}.tmp_write", path.as_str()));

    fs.write(tmp_path.clone(), content)
        .map_err(|e| e.to_string())?;

    fs.rename(&tmp_path, path.clone())
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Compute the hex-encoded SHA-256 content version of `bytes`.
///
/// This is the canonical version representation used by the optimistic CAS
/// mechanism (spec 18). The result is a 64-character lowercase hex string.
///
/// # Design decision
///
/// The hash is computed from raw bytes, not from a `ContentHash` value object,
/// because the CAS check must operate on the **live** bytes read through the
/// `FileSystemPort` at mutation time — never on the cached `content_hash` field
/// in `VirtualFile` (which the watcher sets to `None` on Modify events and is
/// therefore unreliable as a freshness signal).
///
/// The `sha2` crate is already a declared dependency (spec 15 content hashing).
/// No new infrastructure import enters the domain: the SHA-256 computation is
/// pure CPU work on a `&[u8]` that the domain already holds after reading
/// through the port.
///
/// # Examples
///
/// ```rust
/// use core_engine::domain::mutation::compute_content_version;
/// let v = compute_content_version(b"hello");
/// assert_eq!(v.len(), 64);
/// assert!(v.chars().all(|c| c.is_ascii_hexdigit()));
/// ```
pub fn compute_content_version(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for byte in digest {
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    s
}

/// Return `true` if `path` is a shadow-file temp artifact that must not be
/// indexed as a real user file (UN2/AC5).
///
/// The domain uses a `.tmp_write` suffix for the write-side temp file.
/// `RealFs::write` uses a `.~tmp` suffix for its internal durable-write temp
/// file. Both suffixes must be excluded from indexing.
///
/// # Examples
///
/// ```
/// use core_engine::domain::mutation::is_tmp_artifact;
/// use core_engine::domain::RelativePath;
///
/// assert!(is_tmp_artifact(&RelativePath::new("src/main.rs.tmp_write")));
/// assert!(is_tmp_artifact(&RelativePath::new("src/main.rs.~tmp")));
/// assert!(!is_tmp_artifact(&RelativePath::new("src/main.rs")));
/// assert!(!is_tmp_artifact(&RelativePath::new("src/tmp_write.rs")));
/// ```
#[must_use]
pub fn is_tmp_artifact(path: &crate::domain::RelativePath) -> bool {
    let s = path.as_str();
    s.ends_with(".tmp_write") || s.ends_with(".~tmp")
}
