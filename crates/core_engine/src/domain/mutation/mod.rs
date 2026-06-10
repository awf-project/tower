//! `FileMutationUseCase` implementation — spec 08.
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
//!         └─ plugin_host.on_file_changed  ← broadcast after commit
//!
//!  delete_file(path):
//!    resolve FileId from workspace
//!    workspace.remove + index.remove  ← clear in-memory state
//!    FileSystemPort.delete(path)      ← physical remove first (crash-safe)
//!    storage.delete                   ← persist removal after physical delete
//!    plugin_host.on_file_changed
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
