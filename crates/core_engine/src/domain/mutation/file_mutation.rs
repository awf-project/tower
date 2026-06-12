//! `FileMutationService` — concrete implementation of [`FileMutationUseCase`].
#![forbid(unsafe_code)]

use crate::domain::index::InvertedIndex;
use crate::domain::refactor::GlobalReplaceService;
use crate::domain::token::tokenize;
use crate::domain::virtual_file::{FileMetadata, Timestamp};
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, RelativePath};
use crate::ports::inbound::{FileMutationUseCase, TxReport};
use crate::ports::{FileSystemPort, PluginHostPort, PortError, StoragePort};

// ── FileMutationService ───────────────────────────────────────────────────────

/// Implements [`FileMutationUseCase`]: create/overwrite files, create
/// directories, and delete files — keeping the VFS, inverted index, and
/// storage in sync.
///
/// # Ownership model
///
/// The service borrows all state mutably for the duration of a call sequence.
/// The [`FileSystemPort`] is borrowed rather than owned so the same FS
/// instance can be reused across multiple service method calls and shared
/// with other components (e.g. an `InMemoryFs` in tests).
///
/// # Example
///
/// ```rust
/// use core_engine::domain::mutation::FileMutationService;
/// use core_engine::domain::workspace::ProjectWorkspace;
/// use core_engine::domain::index::InvertedIndex;
/// use core_engine::domain::RelativePath;
/// use core_engine::adapters::{InMemoryFs, InMemoryStorage};
/// use core_engine::ports::inbound::FileMutationUseCase;
/// use core_engine::ports::NoOpPluginHost;
///
/// let mut workspace = ProjectWorkspace::new();
/// let mut index = InvertedIndex::new();
/// let mut storage = InMemoryStorage::new();
/// let mut fs = InMemoryFs::new();
///
/// let mut svc = FileMutationService::new(
///     &mut fs,
///     &mut workspace,
///     &mut index,
///     &mut storage,
///     &NoOpPluginHost,
/// );
///
/// let path = RelativePath::new("src/lib.rs");
/// svc.create_file(path.clone(), b"fn main() {}".to_vec()).unwrap();
///
/// // The file is now tracked in the VFS.
/// assert!(workspace.get_by_path(&path).is_some());
/// ```
pub struct FileMutationService<'ws> {
    fs: &'ws mut dyn FileSystemPort,
    workspace: &'ws mut ProjectWorkspace,
    index: &'ws mut InvertedIndex,
    storage: &'ws mut dyn StoragePort,
    plugin_host: &'ws dyn PluginHostPort,
}

impl<'ws> FileMutationService<'ws> {
    /// Construct a new service.
    ///
    /// # Arguments
    ///
    /// - `fs`          — file-system port (borrowed mutably so the same
    ///   instance can be reused across multiple calls).
    /// - `workspace`   — mutable workspace aggregate.
    /// - `index`       — mutable inverted index.
    /// - `storage`     — mutable storage port.
    /// - `plugin_host` — lifecycle hook receiver (`NoOpPluginHost` when none).
    pub fn new(
        fs: &'ws mut dyn FileSystemPort,
        workspace: &'ws mut ProjectWorkspace,
        index: &'ws mut InvertedIndex,
        storage: &'ws mut dyn StoragePort,
        plugin_host: &'ws dyn PluginHostPort,
    ) -> Self {
        Self {
            fs,
            workspace,
            index,
            storage,
            plugin_host,
        }
    }
}

impl<'ws> FileMutationUseCase for FileMutationService<'ws> {
    /// Create or overwrite the file at `path` with `content`.
    ///
    /// Uses the shadow-file pattern:
    /// 1. Write `content` to `<path>.tmp_write` (durable flush by the port).
    /// 2. Atomically rename `<path>.tmp_write` → `path`.
    /// 3. Upsert VFS + index + storage.
    /// 4. Broadcast `on_file_changed`.
    ///
    /// # Errors
    ///
    /// Returns a domain error on port failure (WriteFailed from rename maps to
    /// `DomainError::IoError`). `DuplicatePath` is handled internally as an
    /// overwrite (upsert semantics).
    ///
    /// # Crash safety
    ///
    /// A crash after step 1 but before step 2 leaves `<path>.tmp_write` on
    /// disk; the original `path` is untouched. The stray `.tmp_write` is
    /// filtered out by the scanner and watcher (AC5). Steps 3–4 only execute
    /// if step 2 succeeds, so a crash mid-rename leaves the domain state clean.
    ///
    /// # Watcher idempotency
    ///
    /// After this call the OS watcher will emit a `Create`/`Modify` event for
    /// `path`. `EventProcessor::handle_create` is guarded by `ws.get_by_path`
    /// — a path already tracked is a silent no-op. `handle_modify` updates
    /// metadata in-place, preserving the `FileId`. No extra suppression needed.
    fn create_file(&mut self, path: RelativePath, content: Vec<u8>) -> Result<(), DomainError> {
        // ── Step 1 & 2: shadow-file write + atomic rename ─────────────────────
        //
        // Decision: delegate to the canonical `atomic_write` primitive in
        // `mutation/mod.rs` rather than inlining the tmp+rename logic here.
        //
        // Why: `InMemoryFs::write` is a direct insert (no internal
        // temp+rename). If we called `fs.write(dst)`, a crash between write
        // and index/storage update would leave the file on disk with no VFS
        // entry — observable partial state. With the domain-level `.tmp_write`
        // + rename, a crash before rename leaves `.tmp_write` (GC-able) and
        // the original dst untouched; a crash after rename but before VFS
        // update is not possible in single-threaded domain code.
        //
        // For `RealFs`, `fs.write(tmp)` also does an internal `.~tmp` +
        // fsync + rename. This "double-shadow" is intentional: the inner
        // `.~tmp` ensures the `.tmp_write` file itself is durably written
        // before the domain renames it over the destination.
        super::atomic_write(self.fs, &path, content).map_err(DomainError::IoError)?;

        // ── Step 3: VFS + index + storage ─────────────────────────────────────
        //
        // Size is derived from what the FS just accepted. We use 0 here because
        // the domain must not re-read the FS for metadata (that would introduce
        // an unnecessary round-trip and a potential TOCTOU race). The watcher
        // will update the size via a Modify event with real metadata.
        let metadata = FileMetadata {
            size: 0,
            modified: Timestamp(0),
            content_hash: None,
        };

        let tokens = tokenize(path.as_str());

        let file_id = match self.workspace.insert(path.clone(), metadata) {
            Ok(id) => {
                // New file: insert into index.
                self.index.insert(id, &tokens);
                id
            }
            Err(DomainError::DuplicatePath) => {
                // Overwrite path: update metadata in-place (stable FileId),
                // then delta-reindex.
                let id = self
                    .workspace
                    .get_by_path(&path)
                    .expect("DuplicatePath guarantees get_by_path succeeds");
                self.workspace
                    .update(id, metadata)
                    .expect("get_by_path returned a valid id — slot must be live");
                self.index.remove(id);
                self.index.insert(id, &tokens);
                id
            }
            Err(e) => return Err(e),
        };

        let virtual_file = self
            .workspace
            .get(file_id)
            .expect("just inserted/updated — slot must be live")
            .clone();

        self.storage.put(virtual_file).map_err(port_err_to_domain)?;

        // ── Step 4: broadcast ─────────────────────────────────────────────────
        self.plugin_host.on_file_changed(file_id, &path);

        Ok(())
    }

    /// Create a directory at `path` recursively (EV4/AC4).
    ///
    /// Directories are not tracked in the VFS — only files are indexed.
    ///
    /// # Errors
    ///
    /// Returns a domain error if the port signals a write failure.
    fn create_directory(&mut self, path: RelativePath) -> Result<(), DomainError> {
        self.fs.mkdir(path).map_err(port_err_to_domain)
    }

    /// Delete the file at `path` (EV3/AC3).
    ///
    /// Order:
    /// 1. Resolve `FileId` from the VFS — `DomainError::NotFound` if absent.
    /// 2. Remove from workspace + index (in-memory state cleared).
    /// 3. Physically remove the file via the FS port.
    /// 4. Delete from storage (persist the removal).
    /// 5. Broadcast `on_file_changed`.
    ///
    /// # Crash-safe ordering
    ///
    /// Decision: physical delete (step 3) fires before storage delete (step 4).
    ///
    /// Why: a crash between step 3 and step 4 leaves the file gone from disk
    /// with a dangling sled entry. On restart the watcher fires a Delete event
    /// that cleans up the stale storage record — self-healing. The inverse
    /// ordering (storage first) would leave the file on disk with no domain
    /// record and no watcher event to repair it, making it permanently invisible
    /// to the domain API until a full rescan.
    ///
    /// Trade-off: a crash after fs.delete but before storage.delete leaves a
    /// dangling storage entry (minor leak). The watcher heals it. A crash after
    /// storage.delete but before fs.delete would leave the file permanently
    /// unreachable — we explicitly avoid that failure mode.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when `path` is not tracked.
    fn delete_file(&mut self, path: &RelativePath) -> Result<(), DomainError> {
        // Step 1: resolve the FileId from the VFS.
        let file_id = self
            .workspace
            .get_by_path(path)
            .ok_or(DomainError::NotFound)?;

        // Step 2: remove from VFS + index (in-memory; always succeeds).
        self.workspace
            .remove(file_id)
            .expect("get_by_path returned a valid id — slot must be live");
        self.index.remove(file_id);

        // Step 3: physical removal first (crash-safe ordering — see doc above).
        // A NotFound from the FS is tolerated — if the file was already removed
        // from disk we still want to clean up the domain state.
        match self.fs.delete(path) {
            Ok(()) | Err(PortError::NotFound) => {}
            Err(e) => return Err(port_err_to_domain(e)),
        }

        // Step 4: persist the removal.
        // Ignore NotFound from storage — the file may never have been persisted
        // (e.g. created and deleted between flush cycles).
        match self.storage.delete(file_id) {
            Ok(()) | Err(PortError::NotFound) => {}
            Err(e) => return Err(port_err_to_domain(e)),
        }

        // Step 5: broadcast.
        self.plugin_host.on_file_changed(file_id, path);

        Ok(())
    }

    /// Global find-and-replace across all indexed files (spec 09).
    ///
    /// Composes spec-07 content reading and spec-08 atomic shadow-file writes.
    /// Parallelises the read+apply phase with Rayon; commits writes sequentially
    /// to avoid data races on shared mutable state (VFS + index + storage).
    ///
    /// Per-file failures (e.g. read-only files) are collected in
    /// `TxReport::errors`; the operation continues for all remaining files
    /// (partial-failure semantics, UN1/AC2).
    ///
    /// # Errors
    ///
    /// Returns `DomainError::IoError` only if the batch storage flush fails
    /// after all FS writes succeeded. Individual per-file errors are in
    /// `TxReport::errors`, not in the `Err` path.
    fn global_replace(&mut self, target: &str, replacement: &str) -> Result<TxReport, DomainError> {
        GlobalReplaceService::new(self.fs, self.workspace, self.storage).execute(
            target,
            replacement,
            false,
        )
    }

    /// Dry-run variant: compute the would-change report without writing any
    /// file or mutating any state (OP1/AC4).
    ///
    /// # Errors
    ///
    /// Returns `DomainError` only if the workspace cannot be read.
    fn global_replace_dry_run(
        &mut self,
        target: &str,
        replacement: &str,
    ) -> Result<TxReport, DomainError> {
        GlobalReplaceService::new(self.fs, self.workspace, self.storage).execute(
            target,
            replacement,
            true,
        )
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Map a [`PortError`] to a [`DomainError`].
///
/// The domain layer must not expose infrastructure error types in its return
/// values; this conversion preserves the cause information in a domain-safe way.
fn port_err_to_domain(err: PortError) -> DomainError {
    match err {
        PortError::NotFound => DomainError::NotFound,
        PortError::WriteFailed(reason)
        | PortError::ReadFailed(reason)
        | PortError::InvalidArgs(reason) => DomainError::IoError(reason),
    }
}
