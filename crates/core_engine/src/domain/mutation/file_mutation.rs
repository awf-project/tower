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

    /// Shared "commit an indexed full-file write" tail (spec 17 REFACTOR).
    ///
    /// Shared commit tail for a full-file rewrite of an **already-tracked**
    /// file. Called only by [`FileMutationUseCase::edit_range`], which resolves
    /// the live `file_id` before computing the spliced bytes. Performs:
    ///
    /// 1. Atomically write `content` to disk via the spec-08 shadow-file pattern.
    /// 2. Update VFS metadata in-place on the existing slot (stable `FileId`).
    /// 3. Delta-reindex (remove old tokens + insert new).
    /// 4. `storage.put` to persist the updated `VirtualFile` record.
    /// 5. Broadcast `on_file_changed` to the plugin host.
    ///
    /// [`FileMutationUseCase::create_file`] does **not** route through this
    /// helper: it has upsert semantics (insert-or-update via `workspace.insert`
    /// with a `DuplicatePath` fallback) and keeps its own inline commit sequence.
    /// This helper deliberately handles only the update-existing case, so it
    /// takes a pre-resolved `file_id` and calls `workspace.update` directly.
    ///
    /// # Arguments
    ///
    /// - `path`    — workspace-relative path (used for tokenisation + broadcast).
    /// - `file_id` — pre-resolved, live `FileId` for the existing VFS slot.
    /// - `content` — the full new file bytes to write and commit.
    ///
    /// # Errors
    ///
    /// Propagates `atomic_write` failures as `DomainError::IoError` and
    /// `storage.put` failures likewise.
    fn commit_indexed_write(
        &mut self,
        path: &RelativePath,
        file_id: crate::domain::FileId,
        content: Vec<u8>,
    ) -> Result<(), DomainError> {
        // Step A: shadow-file write + atomic rename.
        super::atomic_write(self.fs, path, content).map_err(DomainError::IoError)?;

        // Step B: VFS — update metadata in-place (stable FileId; the slot is
        // already live for both create_file's overwrite branch and edit_range).
        let metadata = FileMetadata {
            size: 0,
            modified: Timestamp(0),
            content_hash: None,
        };
        self.workspace
            .update(file_id, metadata)
            .expect("file_id is live — update must succeed");

        // Step C: delta-reindex.
        let tokens = tokenize(path.as_str());
        self.index.remove(file_id);
        self.index.insert(file_id, &tokens);

        // Step D: persist the updated VirtualFile record.
        let virtual_file = self
            .workspace
            .get(file_id)
            .expect("just updated — slot must be live")
            .clone();
        self.storage.put(virtual_file).map_err(port_err_to_domain)?;

        // Step E: broadcast.
        self.plugin_host.on_file_changed(file_id, path);

        Ok(())
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

    /// Replace the byte range `[start_byte, end_byte)` with `replacement`
    /// (spec 17 — surgical range edit).
    ///
    /// # Algorithm
    ///
    /// 1. Read current bytes via `FileSystemPort::read`.
    /// 2. Validate range: `0 ≤ start ≤ end ≤ len` and both on UTF-8 char
    ///    boundaries — return [`DomainError::InvalidRange`] without touching
    ///    the file if validation fails.
    /// 3. Splice: `bytes[..start] ++ replacement ++ bytes[end..]`.
    /// 4. Atomic write via the spec-08 shadow-file primitive.
    /// 5. Commit: update VFS metadata + delta-reindex + `storage.put` +
    ///    broadcast `on_file_changed`.
    ///
    /// # Errors
    ///
    /// See [`FileMutationUseCase::edit_range`] for the full error table.
    fn edit_range(
        &mut self,
        path: &RelativePath,
        start_byte: usize,
        end_byte: usize,
        replacement: &str,
    ) -> Result<TxReport, DomainError> {
        // ── Step 1: read current bytes ─────────────────────────────────────────
        //
        // We read through the FS port rather than the VFS so we always have
        // the actual on-disk bytes (same source as `atomic_write` will target).
        // The FS port returns `PortError::NotFound` if the file does not exist
        // on disk; however, we want to surface the "not tracked in VFS" case
        // first (the spec's UN2 requirement states "edit_range edits existing
        // files ONLY; never creates"). Check the workspace before reading.
        let file_id = self
            .workspace
            .get_by_path(path)
            .ok_or(DomainError::NotFound)?;

        let bytes = self.fs.read(path).map_err(port_err_to_domain)?;

        // ── Step 2: validate range ─────────────────────────────────────────────
        //
        // Both `start_byte` and `end_byte` must satisfy:
        //   0 ≤ start ≤ end ≤ bytes.len()
        // AND the content must be valid UTF-8 with both positions on char boundaries.
        //
        // Decision: validate UTF-8 on the full file bytes first (a single pass),
        // then check char boundaries with `str::is_char_boundary`. This avoids
        // indexing into potentially invalid UTF-8 with raw byte offsets.
        //
        // Trade-off: if the stored file is not valid UTF-8 (e.g. a binary blob),
        // we return `InvalidRange` with a clear message rather than silently
        // corrupting the file. The spec states replacement is a UTF-8 string
        // and the result must stay valid UTF-8.
        let len = bytes.len();

        if start_byte > end_byte {
            return Err(DomainError::InvalidRange(format!(
                "start ({start_byte}) must be ≤ end ({end_byte})"
            )));
        }
        if end_byte > len {
            return Err(DomainError::InvalidRange(format!(
                "end ({end_byte}) exceeds file length ({len})"
            )));
        }

        // Validate UTF-8 and char boundaries in one step.
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            DomainError::InvalidRange(format!(
                "target file is not UTF-8 text; edit_range only edits text files ({e})"
            ))
        })?;

        if !text.is_char_boundary(start_byte) {
            return Err(DomainError::InvalidRange(format!(
                "start byte {start_byte} is not on a UTF-8 character boundary"
            )));
        }
        if !text.is_char_boundary(end_byte) {
            return Err(DomainError::InvalidRange(format!(
                "end byte {end_byte} is not on a UTF-8 character boundary"
            )));
        }

        // ── Step 3: splice ────────────────────────────────────────────────────
        //
        // Concatenate three segments: prefix | replacement | suffix.
        // All three are valid UTF-8 (prefix/suffix at char boundaries, replacement
        // from JSON string), so the result is guaranteed valid UTF-8.
        let mut spliced = Vec::with_capacity(start_byte + replacement.len() + (len - end_byte));
        spliced.extend_from_slice(&bytes[..start_byte]);
        spliced.extend_from_slice(replacement.as_bytes());
        spliced.extend_from_slice(&bytes[end_byte..]);

        // ── Step 4 & 5: atomic write + commit ────────────────────────────────
        self.commit_indexed_write(path, file_id, spliced)?;

        Ok(TxReport {
            files_changed: 1,
            replacements: 1,
            errors: vec![],
        })
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
