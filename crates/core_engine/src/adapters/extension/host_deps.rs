//! `HostDeps` — capability dependencies injected into the sidecar adapter.
//!
//! The sidecar adapter dispatches inbound `HostCall` requests from extensions to
//! the **existing** outbound ports. `HostDeps` bundles those ports together so
//! they can be shared across the adapter's threads.
//!
//! # Hexagonal boundary
//!
//! All three ports are `Arc<dyn Trait>` so `HostDeps` is cheaply cloneable and
//! can be moved into the reader thread without copying data.

use std::sync::Arc;

use crate::adapters::formatter::FormatQueuePort;
use crate::adapters::mcp::PushEvent;
use crate::domain::DomainError;
use crate::ports::inbound::{ApplyEditsFileResult, ApplyEditsRequest};
use crate::ports::{AstIndexPort, FileSystemPort};

/// Capability dependencies required by [`SidecarHostAdapter`].
///
/// Pass this to [`SidecarHostAdapter::spawn`] to wire the extension's capability
/// callbacks to the existing port implementations.
///
/// # Example
///
/// ```rust
/// use std::sync::Arc;
/// use core_engine::adapters::extension::HostDeps;
/// use core_engine::adapters::{InMemoryAstIndex, InMemoryFs};
/// use core_engine::adapters::formatter::NoOpFormatQueue;
///
/// let deps = HostDeps {
///     fs: Arc::new(std::sync::Mutex::new(InMemoryFs::new())),
///     ast_index: Arc::new(InMemoryAstIndex::new()),
///     format_queue: Arc::new(NoOpFormatQueue),
///     apply_edits: Arc::new(core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost),
///     push_tx: None,
/// };
/// ```
#[derive(Clone)]
pub struct HostDeps {
    /// Filesystem port for `workspace/readFile` and `workspace/listFiles`.
    ///
    /// Wrapped in a `Mutex` because [`FileSystemPort`] requires `&mut self` for
    /// write operations. Read-only capability calls only need `&self`, but we
    /// hold the lock briefly for each call.
    pub fs: Arc<dyn FsAdapter>,

    /// AST index port for `index/get` and `index/put`.
    pub ast_index: Arc<dyn AstIndexPort>,

    /// Format queue port for `workspace/requestFormat`.
    pub format_queue: Arc<dyn FormatQueuePort>,

    /// Apply-edits port for `workspace/applyEdits`.
    pub apply_edits: Arc<dyn ApplyEditsHostPort>,

    /// Optional push sender for `notify/resourceUpdated` (spec 27 O1).
    ///
    /// When `Some`, the sidecar adapter forwards `NotifyResourceUpdated` host
    /// calls from extensions to the MCP transport's push channel so that
    /// subscribed MCP clients receive `notifications/resources/updated`.
    /// `None` means push is disabled (e.g. in unit tests or non-LSP setups).
    pub push_tx: Option<std::sync::mpsc::Sender<PushEvent>>,
}

/// Apply-edits dependency required by `workspace/applyEdits`.
#[rustfmt::skip]
pub trait ApplyEditsHostPort: Send + Sync {
    fn apply_edits_cas(&self, request: ApplyEditsRequest) -> Result<ApplyEditsFileResult, DomainError>;
    fn apply_edits_dry_run(&self, request: ApplyEditsRequest) -> Result<ApplyEditsFileResult, DomainError>;
}

pub struct UnsupportedApplyEditsHost;

impl ApplyEditsHostPort for UnsupportedApplyEditsHost {
    fn apply_edits_cas(
        &self,
        request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        Err(DomainError::UnsupportedOperation(format!(
            "workspace/applyEdits is not wired for {}",
            request.path.as_str()
        )))
    }

    fn apply_edits_dry_run(
        &self,
        request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        Err(DomainError::UnsupportedOperation(format!(
            "workspace/applyEdits dry-run is not wired for {}",
            request.path.as_str()
        )))
    }
}

/// Object-safe wrapper around `FileSystemPort` for shared read access.
///
/// `FileSystemPort::read` takes `&self`, so we only need a shared reference for
/// the read capability. We define this trait to avoid `Mutex` around the whole
/// FS adapter just for reads.
pub trait FsAdapter: Send + Sync {
    /// Read the file at the given workspace-relative path.
    ///
    /// # Errors
    ///
    /// Returns an error string on any I/O or not-found failure.
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;

    /// List all indexed workspace files (relative paths).
    fn list_files(&self) -> Vec<String>;

    /// Absolute workspace root, when the adapter is backed by a real filesystem.
    ///
    /// Used to tell a spawned extension where the workspace is (via the
    /// `TOWER_WORKSPACE` env var and the child's cwd), so the extension can find
    /// `.tower/config.toml` and root its language servers regardless of the host
    /// process's own working directory. In-memory adapters return `None`.
    fn workspace_root(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// Adapt a `Mutex<F: FileSystemPort>` to `FsAdapter`.
///
/// The mutex is needed because `FileSystemPort` methods are on `&mut self` for
/// write operations, but we only call `read` here (which takes `&self`). We
/// still take the lock for correctness (prevent concurrent writes).
impl<F: FileSystemPort + Send + 'static> FsAdapter for std::sync::Mutex<F> {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let guard = self.lock().map_err(|e| format!("fs mutex poisoned: {e}"))?;
        let rel = crate::domain::RelativePath::new(path);
        guard
            .read(&rel)
            .map_err(|e| format!("readFile({path:?}) failed: {e}"))
    }

    fn list_files(&self) -> Vec<String> {
        let Ok(guard) = self.lock() else {
            return Vec::new();
        };
        // Enumerate the REAL workspace via a recursive walk (honoring
        // `.towerignore`), mirroring `native_tools::call_reindex`. `scan()` only
        // reports paths written through THIS adapter instance this session — an
        // extension's fresh `RealFs` has touched nothing, so `scan()` would
        // (wrongly) return an empty list and `workspace/listFiles` would expose
        // no files. In-memory adapters (tests) have no real root → fall back to
        // `scan()`. `.tmp_write` shadow artifacts are never surfaced.
        match guard.workspace_root() {
            Some(root) => crate::adapters::fs::scan::collect_workspace_files(&root)
                .into_iter()
                .map(|(p, _meta)| p)
                .filter(|p| !crate::domain::mutation::is_tmp_artifact(p))
                .map(|p| p.as_str().to_owned())
                .collect(),
            None => guard
                .scan()
                .into_iter()
                .map(|p| p.as_str().to_owned())
                .collect(),
        }
    }

    fn workspace_root(&self) -> Option<std::path::PathBuf> {
        self.lock().ok().and_then(|g| g.workspace_root())
    }
}

impl std::fmt::Debug for HostDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostDeps").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::FsAdapter;
    use crate::adapters::fs::RealFs;

    /// Regression: `workspace/listFiles` must enumerate the REAL workspace, not
    /// the adapter's touched-paths cache (`scan()`). A fresh `RealFs` has touched
    /// nothing, yet `list_files` must still return the files on disk — otherwise
    /// `tower_fmt_format {}` (format-all over listFiles) sees zero files.
    #[test]
    fn list_files_enumerates_workspace_not_touched_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"hi\n").unwrap();
        // A shadow artifact that must never be surfaced.
        std::fs::write(dir.path().join("src/a.rs.tmp_write"), b"x").unwrap();

        // Fresh adapter — nothing has been read or written through it, so its
        // `scan()` (known_paths) is empty.
        let fs: Mutex<RealFs> = Mutex::new(RealFs::new(dir.path()));
        let mut files = FsAdapter::list_files(&fs);
        files.sort();

        assert!(
            files.contains(&"src/a.rs".to_owned()) && files.contains(&"b.txt".to_owned()),
            "listFiles must enumerate workspace files via a real walk, got: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.ends_with(".tmp_write")),
            ".tmp_write shadow artifacts must never be surfaced, got: {files:?}"
        );
    }

    /// `workspace_root` must surface the real fs root (used by `spawn` to export
    /// `TOWER_WORKSPACE` to extensions). In-memory adapters return `None`.
    #[test]
    fn workspace_root_is_exposed_for_real_fs() {
        let dir = tempfile::tempdir().unwrap();
        let fs: Mutex<RealFs> = Mutex::new(RealFs::new(dir.path()));
        assert_eq!(FsAdapter::workspace_root(&fs).as_deref(), Some(dir.path()));

        let mem = Mutex::new(crate::adapters::InMemoryFs::new());
        assert_eq!(FsAdapter::workspace_root(&mem), None);
    }
}
