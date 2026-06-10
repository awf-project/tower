//! `SledStorageAdapter` — Sled-backed [`StoragePort`] implementation (specs 04a + 04b).
//!
//! # Wireframe
//!
//! ```text
//!  ┌──────────────────────────────────────────────────────────────────────────┐
//!  │  SledStorageAdapter                                                       │
//!  │                                                                           │
//!  │  db: sled::Db                                                             │
//!  │  ├─ tree "files"  : FileId (8B BE) → VirtualFile (postcard)              │
//!  │  ├─ tree "paths"  : RelativePath bytes → FileId (8B BE)                  │
//!  │  ├─ tree "blobs"  : [u8;32] hash → raw blob bytes                        │
//!  │  ├─ tree "meta"   : "schema_version" / slot_gens / free_slots            │
//!  │  └─ tree "index"  : Token bytes → Vec<FileId> (postcard) [spec 04b]      │
//!  │                                                                           │
//!  │  open(path) → Result<(Self, ProjectWorkspace, InvertedIndex), StorageError>│
//!  │  ─────────────────────────────────────────────────────────────────────── │
//!  │  put/delete: files+paths+meta+index in ONE atomic transaction (EV2/UN1)  │
//!  │  get: direct tree read (no global lock → ST1)                            │
//!  └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Atomicity (specs 04a + 04b)
//!
//! `put` and `delete` mutate the `files`, `paths`, `meta`, and `index` trees
//! inside a single [`sled::Transactional`] call over a 4-tree tuple.  Sled
//! aborts the entire transaction atomically if the closure returns `Err`,
//! leaving no partial state (spec UN1/AC3 for both 04a and 04b).
//!
//! # Index persistence (spec 04b)
//!
//! Posting lists (`HashSet<FileId>` in the domain, see 03b) are stored as
//! `Vec<FileId>` serialised with `postcard` in the `index` tree.  The key is
//! the token's UTF-8 bytes.  On `open()` the index tree is read back, each
//! posting list deserialised, and any `FileId` absent from the live files set
//! is silently dropped (self-healing reconciliation, spec UN2/AC4).  Entries
//! whose value bytes are corrupt (not valid postcard) are also dropped — they
//! are treated like dangling postings and logged to stderr.
//!
//! # Allocator state (spec EV1/AC2)
//!
//! `slot_generations` and `free_slots` are written inside the same transaction
//! as the file data.  The `slot_generations` read is performed *inside* the
//! transaction closure so the read-modify-write is atomic under sled's
//! optimistic concurrency control.  `free_slots` is derived authoritatively at
//! write time.
//!
//! # Schema versioning (spec UN2/AC4)
//!
//! The `meta` tree stores `"schema_version"` as a little-endian `u32`. On
//! `open()`, the stored version is compared to [`SCHEMA_VERSION`]. A mismatch
//! returns [`StorageError::IncompatibleSchema`] before any data is read.
//!
//! # Concurrent reads (spec ST1)
//!
//! `get` and `get_blob` call `sled::Tree::get` directly with `&self`, acquiring
//! no application-level lock.

use std::collections::HashSet;
use std::path::Path;

use sled::transaction::ConflictableTransactionError;
use sled::Transactional;

use super::error::{to_port_error, to_read_error, StorageError};
use super::keys::{blob_key, file_id_key, path_key};
use crate::domain::index::InvertedIndex;
use crate::domain::token::tokenize;
use crate::domain::{ContentHash, FileId, ProjectWorkspace, VirtualFile, WorkspaceSnapshot};
use crate::ports::{PortError, StoragePort};

/// Schema version embedded in every new database.
///
/// Increment this constant when the on-disk format changes in a
/// backward-incompatible way. `open()` refuses to start if the stored version
/// does not match (spec UN2/AC4).
pub const SCHEMA_VERSION: u32 = 1;

const META_SCHEMA_KEY: &[u8] = b"schema_version";
const META_SLOT_GENS_KEY: &[u8] = b"slot_generations";
const META_FREE_SLOTS_KEY: &[u8] = b"free_slots";
/// Scan-completion marker key in the `meta` tree.
///
/// Absent or zero means "scan never completed (or crashed mid-scan)".
/// Value `[1u8]` means "scan completed successfully".
const META_SCAN_COMPLETE_KEY: &[u8] = b"scan_complete";

/// Abort reasons carried through sled transaction closures.
///
/// Sled transactions are generic over the abort payload — using an enum lets
/// callers distinguish "file not found" from "data is corrupt" instead of
/// collapsing both into `NotFound`.
#[derive(Debug)]
enum TxAbort {
    /// The requested file was not present in the files tree.
    NotFound,
    /// A file entry exists but could not be deserialised.
    Corrupt(postcard::Error),
    /// The posting list for a token could not be serialised or deserialised,
    /// or the allocator state could not be serialised.  Must not silently
    /// corrupt the index — the transaction is aborted.
    IndexCodec(postcard::Error),
    /// Forced abort injected by tests to verify joint-rollback behaviour.
    #[cfg(test)]
    ForcedAbort,
}

/// Sled-backed implementation of [`StoragePort`].
///
/// Obtain an instance via [`SledStorageAdapter::open`]. The returned
/// [`ProjectWorkspace`] and [`InvertedIndex`] are fully reconstructed from the
/// on-disk state.
///
/// # Example
///
/// ```rust,no_run
/// use core_engine::adapters::storage::SledStorageAdapter;
///
/// let dir = tempfile::tempdir().unwrap();
/// let (_adapter, _workspace, _index) = SledStorageAdapter::open(dir.path()).unwrap();
/// ```
#[derive(Debug)]
pub struct SledStorageAdapter {
    files: sled::Tree,
    paths: sled::Tree,
    blobs: sled::Tree,
    meta: sled::Tree,
    /// Dedicated tree for inverted-index posting lists (spec 04b U1).
    ///
    /// Key: token UTF-8 bytes.  Value: `postcard`-serialised `Vec<FileId>`.
    index: sled::Tree,
}

impl SledStorageAdapter {
    /// Open (or create) a Sled database at `path` and reconstruct the
    /// [`ProjectWorkspace`] and [`InvertedIndex`] from persisted state.
    ///
    /// # Errors
    ///
    /// - [`StorageError::IncompatibleSchema`] — the on-disk `schema_version`
    ///   does not match [`SCHEMA_VERSION`] (spec UN2/AC4).
    /// - [`StorageError::Sled`] — a Sled I/O error occurred.
    /// - [`StorageError::Codec`] — a serialisation round-trip failed.
    pub fn open(path: &Path) -> Result<(Self, ProjectWorkspace, InvertedIndex), StorageError> {
        let db = sled::open(path)?;
        let files = db.open_tree("files")?;
        let paths = db.open_tree("paths")?;
        let blobs = db.open_tree("blobs")?;
        let meta = db.open_tree("meta")?;
        let index = db.open_tree("index")?;

        let adapter = Self {
            files,
            paths,
            blobs,
            meta,
            index,
        };

        adapter.check_or_init_schema()?;

        // Decision: single pass over the files tree — rebuild_workspace_and_ids
        // builds both the ProjectWorkspace and the live-ids set in one iteration,
        // avoiding the double full-scan that the original rebuild_workspace +
        // collect_live_ids pair required.
        let (workspace, live_ids) = adapter.rebuild_workspace_and_ids()?;
        let inverted = adapter.rehydrate_index(&live_ids)?;

        Ok((adapter, workspace, inverted))
    }

    /// Verify the on-disk schema version or write it for a fresh database.
    fn check_or_init_schema(&self) -> Result<(), StorageError> {
        match self.meta.get(META_SCHEMA_KEY)? {
            None => {
                // Fresh database — write the current version.
                self.meta
                    .insert(META_SCHEMA_KEY, &SCHEMA_VERSION.to_le_bytes())?;
                self.meta.flush()?;
                Ok(())
            }
            Some(bytes) => {
                let on_disk = read_u32_le(&bytes).ok_or_else(|| {
                    StorageError::CorruptMetadata("schema_version is not 4 bytes".to_owned())
                })?;
                if on_disk != SCHEMA_VERSION {
                    return Err(StorageError::IncompatibleSchema {
                        on_disk,
                        expected: SCHEMA_VERSION,
                    });
                }
                Ok(())
            }
        }
    }

    /// Rebuild a [`ProjectWorkspace`] from the `files` tree and the allocator
    /// state stored in `meta` (spec EV1/AC2), returning both the workspace and
    /// the set of live [`FileId`]s in a single pass over the files tree.
    ///
    /// The live-id set is consumed by [`rehydrate_index`] to identify dangling
    /// postings.  Combining both operations avoids iterating the files tree
    /// twice on every `open()`.
    fn rebuild_workspace_and_ids(
        &self,
    ) -> Result<(ProjectWorkspace, HashSet<FileId>), StorageError> {
        let mut live_files: Vec<VirtualFile> = Vec::new();
        for entry in self.files.iter() {
            let (_key, value) = entry?;
            let file: VirtualFile = postcard::from_bytes(&value)?;
            live_files.push(file);
        }

        // Build the live-ids set from the already-collected files — no second
        // scan of the tree.
        let live_ids: HashSet<FileId> = live_files.iter().map(|f| f.id).collect();

        let slot_generations: Vec<u32> = match self.meta.get(META_SLOT_GENS_KEY)? {
            None => Vec::new(),
            Some(bytes) => postcard::from_bytes::<Vec<u32>>(&bytes)?,
        };

        // Derive free_slots from the files tree: any slot index that has a
        // slot_generations entry but no live file is free.
        let occupied: HashSet<u32> = live_files.iter().map(|f| f.id.index()).collect();
        let free_slots: Vec<u32> = (0u32..slot_generations.len() as u32)
            .filter(|&i| !occupied.contains(&i))
            .collect();

        let snapshot = WorkspaceSnapshot {
            slot_generations,
            free_slots,
            files: live_files,
        };

        let workspace = ProjectWorkspace::from_snapshot(snapshot)
            .map_err(|e| StorageError::CorruptMetadata(format!("workspace rebuild: {e}")))?;

        Ok((workspace, live_ids))
    }

    /// Rehydrate an [`InvertedIndex`] from the `index` tree.
    ///
    /// Each stored posting list is deserialised and filtered: any [`FileId`]
    /// not present in `live_ids` is dropped before inserting into the index
    /// (self-healing reconciliation, spec UN2/AC4).  Entries with corrupt value
    /// bytes (non-postcard) are also dropped — they are treated as irrecoverable
    /// dangling state and skipped rather than aborting startup.
    fn rehydrate_index(&self, live_ids: &HashSet<FileId>) -> Result<InvertedIndex, StorageError> {
        let mut inverted = InvertedIndex::new();

        for entry in self.index.iter() {
            let (key_bytes, value_bytes) = entry?;

            // Key is raw token UTF-8 bytes.
            let token_str = match std::str::from_utf8(&key_bytes) {
                Ok(s) => s,
                Err(_) => {
                    // Corrupt key — skip; cannot affect correctness.
                    continue;
                }
            };
            let token = crate::domain::token::Token::new(token_str);

            // Value is postcard-encoded Vec<FileId>.  A decode failure means
            // the entry is corrupt; drop it (self-healing, AC4).
            let posting: Vec<FileId> = match postcard::from_bytes(&value_bytes) {
                Ok(v) => v,
                Err(_) => {
                    // Corrupt value — skip; the entry is irrecoverable.
                    continue;
                }
            };

            // Filter out dangling FileIds (reconciliation, AC4).
            for id in posting {
                if live_ids.contains(&id) {
                    inverted.insert(id, std::slice::from_ref(&token));
                }
            }
        }

        Ok(inverted)
    }

    /// Test-only: attempt a `put` that writes to both the files tree and the
    /// index tree, then unconditionally aborts the transaction so callers can
    /// verify that neither the file record nor the postings commit.
    ///
    /// Writing to tx_index before aborting is essential: it ensures that a
    /// regression which commits index changes before the abort would be caught
    /// by the AC3 test.
    ///
    /// Returns `Err(PortError::WriteFailed)` always (the abort is intentional).
    ///
    /// # Decision
    ///
    /// Rather than parameterising the production `put` path with a fault-injection
    /// hook (which adds complexity to production code), we provide this
    /// test-only method that exercises the same 4-tree transaction machinery
    /// and writes to both trees before forcing a `ConflictableTransactionError::Abort`.
    #[cfg(test)]
    pub fn put_aborting_for_test(&mut self, file: VirtualFile) -> Result<(), PortError> {
        let key = file_id_key(file.id);
        let new_path_bytes: Vec<u8> = path_key(&file.path).to_vec();
        let file_bytes =
            postcard::to_allocvec(&file).map_err(|e| to_port_error(StorageError::Codec(e)))?;

        // Compute tokens outside the closure (pure, no I/O).
        let tokens = tokenize(file.path.as_str());
        // Use the first token as a sentinel posting to write into tx_index.
        let sentinel_token: Option<Vec<u8>> =
            tokens.first().map(|t| t.as_str().as_bytes().to_vec());

        let result = (&self.files, &self.paths, &self.meta, &self.index).transaction(
            |(tx_files, tx_paths, _tx_meta, tx_index)| {
                // Write to files and paths (mirrors real put).
                tx_files.insert(key.as_ref(), file_bytes.as_slice())?;
                tx_paths.insert(new_path_bytes.as_slice(), key.as_ref())?;

                // Write a sentinel posting entry to the index tree so a
                // regression that commits index writes before the abort would
                // be caught by the test.
                if let Some(token_bytes) = &sentinel_token {
                    let sentinel_posting = postcard::to_allocvec(&vec![file.id])
                        .map_err(TxAbort::IndexCodec)
                        .map_err(ConflictableTransactionError::Abort)?;
                    tx_index.insert(token_bytes.as_slice(), sentinel_posting.as_slice())?;
                }

                // Force an abort after writing to both trees.
                Err(ConflictableTransactionError::Abort(TxAbort::ForcedAbort))
            },
        );

        match result {
            Ok(()) => Ok(()), // Should not happen.
            Err(sled::transaction::TransactionError::Abort(TxAbort::ForcedAbort)) => {
                Err(to_port_error(StorageError::CorruptMetadata(
                    "forced abort for test".to_owned(),
                )))
            }
            Err(e) => Err(to_port_error(map_tx_abort_error(e))),
        }
    }
}

// ── StoragePort implementation ────────────────────────────────────────────────

impl StoragePort for SledStorageAdapter {
    /// Retrieve a `VirtualFile` by `FileId`.
    ///
    /// Reads directly from the Sled tree with `&self` — no application-level
    /// lock is acquired, so concurrent readers are not serialised (spec ST1).
    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        let key = file_id_key(id);
        match self
            .files
            .get(key)
            .map_err(|e| to_read_error(StorageError::Sled(e)))?
        {
            None => Err(PortError::NotFound),
            Some(bytes) => {
                postcard::from_bytes(&bytes).map_err(|e| to_read_error(StorageError::Codec(e)))
            }
        }
    }

    /// Persist a `VirtualFile`. Mutates `files`, `paths`, `meta`, and `index` in
    /// ONE Sled transaction (spec EV2/UN1/AC3 for both 04a and 04b).
    ///
    /// Token delta computation:
    /// - If a previous file existed at this `FileId`, tokenise the old path and
    ///   the new path, remove `file_id` from tokens only in `old - new`, add to
    ///   tokens only in `new - old`.
    /// - If no previous file existed, add `file_id` to all tokens of the new path.
    ///
    /// # Allocator state atomicity
    ///
    /// `slot_generations` is read *inside* the transaction closure so the
    /// read-modify-write is a single atomic operation under sled's optimistic
    /// concurrency control.  A concurrent `put` that commits between the
    /// outer read and the inner write will cause sled to retry the closure with
    /// the latest value, preventing the TOCTOU race that an outer read would
    /// introduce.
    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        let key = file_id_key(file.id);
        let new_path_bytes: Vec<u8> = path_key(&file.path).to_vec();
        let file_bytes =
            postcard::to_allocvec(&file).map_err(|e| to_port_error(StorageError::Codec(e)))?;

        // Compute new tokens outside the transaction (pure function, no I/O).
        let new_tokens = tokenize(file.path.as_str());
        let new_token_set: HashSet<String> =
            new_tokens.iter().map(|t| t.as_str().to_owned()).collect();

        let file_index = file.id.index();
        let file_gen = file.id.generation();
        let file_id = file.id;

        (&self.files, &self.paths, &self.meta, &self.index)
            .transaction(|(tx_files, tx_paths, tx_meta, tx_index)| {
                // ── Compute old token set (for delta) ───────────────────────
                //
                // `?` on transactional-tree methods uses the
                // `From<UnabortableTransactionError> for
                // ConflictableTransactionError<TxAbort>` impl provided by
                // sled, so the abort type stays `TxAbort` throughout.
                let old_token_set: HashSet<String> = if let Some(old_bytes) =
                    tx_files.get(key.as_ref())?
                {
                    if let Ok(old_file) = postcard::from_bytes::<VirtualFile>(old_bytes.as_ref()) {
                        // Remove old path entry if the path changed.
                        let old_path = path_key(&old_file.path);
                        if old_path != new_path_bytes.as_slice() {
                            tx_paths.remove(old_path)?;
                        }
                        tokenize(old_file.path.as_str())
                            .iter()
                            .map(|t| t.as_str().to_owned())
                            .collect()
                    } else {
                        HashSet::new()
                    }
                } else {
                    HashSet::new()
                };

                // ── Write file record ───────────────────────────────────────
                tx_files.insert(key.as_ref(), file_bytes.as_slice())?;
                tx_paths.insert(new_path_bytes.as_slice(), key.as_ref())?;

                // ── Update allocator state (read inside transaction) ────────
                //
                // Decision: read slot_generations here rather than outside the
                // closure.  This makes the read-modify-write atomic under
                // sled's optimistic concurrency control, preventing the TOCTOU
                // race where two concurrent puts both snapshot the same
                // slot_generations and the second writer silently discards the
                // first's generation bump.
                let mut slot_gens: Vec<u32> = match tx_meta.get(META_SLOT_GENS_KEY)? {
                    None => Vec::new(),
                    Some(bytes) => postcard::from_bytes::<Vec<u32>>(bytes.as_ref())
                        .map_err(TxAbort::IndexCodec)
                        .map_err(ConflictableTransactionError::Abort)?,
                };

                let target = file_index as usize;
                while slot_gens.len() <= target {
                    slot_gens.push(0);
                }
                slot_gens[target] = file_gen;

                let prev_free: Vec<u32> = match tx_meta.get(META_FREE_SLOTS_KEY)? {
                    None => Vec::new(),
                    Some(bytes) => postcard::from_bytes::<Vec<u32>>(bytes.as_ref())
                        .map_err(TxAbort::IndexCodec)
                        .map_err(ConflictableTransactionError::Abort)?,
                };
                let free_slots: Vec<u32> =
                    prev_free.into_iter().filter(|&i| i != file_index).collect();

                let gens_bytes = postcard::to_allocvec(&slot_gens)
                    .map_err(TxAbort::IndexCodec)
                    .map_err(ConflictableTransactionError::Abort)?;
                let free_bytes = postcard::to_allocvec(&free_slots)
                    .map_err(TxAbort::IndexCodec)
                    .map_err(ConflictableTransactionError::Abort)?;

                tx_meta.insert(META_SLOT_GENS_KEY, gens_bytes.as_slice())?;
                tx_meta.insert(META_FREE_SLOTS_KEY, free_bytes.as_slice())?;

                // ── Update index (spec 04b EV2) ─────────────────────────────
                //
                // Tokens to ADD: new_token_set - old_token_set
                // Tokens to REMOVE: old_token_set - new_token_set
                for token_str in &new_token_set {
                    if !old_token_set.contains(token_str) {
                        // Add file_id to this token's posting list.
                        update_posting(tx_index, token_str.as_bytes(), file_id, true)
                            .map_err(lift_index_codec)?;
                    }
                }
                for token_str in &old_token_set {
                    if !new_token_set.contains(token_str) {
                        // Remove file_id from this token's posting list.
                        update_posting(tx_index, token_str.as_bytes(), file_id, false)
                            .map_err(lift_index_codec)?;
                    }
                }

                Ok::<(), ConflictableTransactionError<TxAbort>>(())
            })
            .map_err(|e| to_port_error(map_tx_abort_error(e)))?;

        Ok(())
    }

    /// Persist N `VirtualFile` records in a single all-or-nothing Sled
    /// transaction over the `files`, `paths`, `meta`, and `index` trees.
    ///
    /// # Decision
    ///
    /// The transaction closure iterates over all files and calls the same
    /// per-file logic used by `put`.  Wrapping all N files in one
    /// `Transactional` call means Sled either commits every record or commits
    /// none of them — satisfying the OP1 batch-atomicity contract.
    ///
    /// Re-inserting an already-persisted file is idempotent (the existing
    /// record is overwritten), which makes the initial scan restartable after a
    /// crash.
    fn put_batch(&mut self, files: &[VirtualFile]) -> Result<(), PortError> {
        if files.is_empty() {
            return Ok(());
        }

        // Pre-serialise all files outside the transaction (pure computation).
        // If any serialisation fails we return early without touching sled.
        struct FileRecord {
            key: [u8; 8],
            path_bytes: Vec<u8>,
            file_bytes: Vec<u8>,
            new_token_set: HashSet<String>,
            file_id: FileId,
            file_index: u32,
            file_gen: u32,
        }

        let mut records: Vec<FileRecord> = Vec::with_capacity(files.len());
        for file in files {
            let key = file_id_key(file.id);
            let path_bytes = path_key(&file.path).to_vec();
            let file_bytes =
                postcard::to_allocvec(file).map_err(|e| to_port_error(StorageError::Codec(e)))?;
            let new_tokens = tokenize(file.path.as_str());
            let new_token_set: HashSet<String> =
                new_tokens.iter().map(|t| t.as_str().to_owned()).collect();
            records.push(FileRecord {
                key,
                path_bytes,
                file_bytes,
                new_token_set,
                file_id: file.id,
                file_index: file.id.index(),
                file_gen: file.id.generation(),
            });
        }

        (&self.files, &self.paths, &self.meta, &self.index)
            .transaction(|(tx_files, tx_paths, tx_meta, tx_index)| {
                // Read the current slot_generations once at the top of the
                // transaction; update it for each file in the batch.
                let mut slot_gens: Vec<u32> = match tx_meta.get(META_SLOT_GENS_KEY)? {
                    None => Vec::new(),
                    Some(bytes) => postcard::from_bytes::<Vec<u32>>(bytes.as_ref())
                        .map_err(TxAbort::IndexCodec)
                        .map_err(ConflictableTransactionError::Abort)?,
                };
                let mut free_slots: Vec<u32> = match tx_meta.get(META_FREE_SLOTS_KEY)? {
                    None => Vec::new(),
                    Some(bytes) => postcard::from_bytes::<Vec<u32>>(bytes.as_ref())
                        .map_err(TxAbort::IndexCodec)
                        .map_err(ConflictableTransactionError::Abort)?,
                };

                for rec in &records {
                    // ── Compute old token set (for delta) ───────────────────
                    let old_token_set: HashSet<String> =
                        if let Some(old_bytes) = tx_files.get(rec.key.as_ref())? {
                            if let Ok(old_file) =
                                postcard::from_bytes::<VirtualFile>(old_bytes.as_ref())
                            {
                                let old_path = path_key(&old_file.path);
                                if old_path != rec.path_bytes.as_slice() {
                                    tx_paths.remove(old_path)?;
                                }
                                tokenize(old_file.path.as_str())
                                    .iter()
                                    .map(|t| t.as_str().to_owned())
                                    .collect()
                            } else {
                                HashSet::new()
                            }
                        } else {
                            HashSet::new()
                        };

                    // ── Write file record ────────────────────────────────────
                    tx_files.insert(rec.key.as_ref(), rec.file_bytes.as_slice())?;
                    tx_paths.insert(rec.path_bytes.as_slice(), rec.key.as_ref())?;

                    // ── Update allocator state ───────────────────────────────
                    let target = rec.file_index as usize;
                    while slot_gens.len() <= target {
                        slot_gens.push(0);
                    }
                    slot_gens[target] = rec.file_gen;
                    free_slots.retain(|&i| i != rec.file_index);

                    // ── Update index ─────────────────────────────────────────
                    for token_str in &rec.new_token_set {
                        if !old_token_set.contains(token_str) {
                            update_posting(tx_index, token_str.as_bytes(), rec.file_id, true)
                                .map_err(lift_index_codec)?;
                        }
                    }
                    for token_str in &old_token_set {
                        if !rec.new_token_set.contains(token_str) {
                            update_posting(tx_index, token_str.as_bytes(), rec.file_id, false)
                                .map_err(lift_index_codec)?;
                        }
                    }
                }

                // Persist updated allocator state once for the whole batch.
                let gens_bytes = postcard::to_allocvec(&slot_gens)
                    .map_err(TxAbort::IndexCodec)
                    .map_err(ConflictableTransactionError::Abort)?;
                let free_bytes = postcard::to_allocvec(&free_slots)
                    .map_err(TxAbort::IndexCodec)
                    .map_err(ConflictableTransactionError::Abort)?;
                tx_meta.insert(META_SLOT_GENS_KEY, gens_bytes.as_slice())?;
                tx_meta.insert(META_FREE_SLOTS_KEY, free_bytes.as_slice())?;

                Ok::<(), ConflictableTransactionError<TxAbort>>(())
            })
            .map_err(|e| to_port_error(map_tx_abort_error(e)))?;

        Ok(())
    }

    /// Remove the entry for `id`. Mutates `files`, `paths`, `meta`, and `index`
    /// in ONE Sled transaction (spec EV2/UN1/AC3).
    ///
    /// Returns `PortError::NotFound` if absent, `PortError::WriteFailed` if
    /// the stored bytes are corrupt.
    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
        let key = file_id_key(id);

        let result = (&self.files, &self.paths, &self.meta, &self.index).transaction(
            |(tx_files, tx_paths, tx_meta, tx_index)| {
                let maybe = tx_files.remove(key.as_ref())?;
                match maybe {
                    None => Err(ConflictableTransactionError::Abort(TxAbort::NotFound)),
                    Some(bytes) => {
                        let file: VirtualFile =
                            postcard::from_bytes(bytes.as_ref()).map_err(|e| {
                                ConflictableTransactionError::Abort(TxAbort::Corrupt(e))
                            })?;
                        tx_paths.remove(path_key(&file.path))?;

                        // Mark the freed slot in meta.
                        let freed_index = id.index();
                        let mut prev_free: Vec<u32> = match tx_meta.get(META_FREE_SLOTS_KEY)? {
                            None => Vec::new(),
                            Some(b) => postcard::from_bytes::<Vec<u32>>(b.as_ref())
                                .map_err(TxAbort::IndexCodec)
                                .map_err(ConflictableTransactionError::Abort)?,
                        };
                        if !prev_free.contains(&freed_index) {
                            prev_free.push(freed_index);
                        }
                        let free_bytes = postcard::to_allocvec(&prev_free)
                            .map_err(TxAbort::IndexCodec)
                            .map_err(ConflictableTransactionError::Abort)?;
                        tx_meta.insert(META_FREE_SLOTS_KEY, free_bytes.as_slice())?;

                        // ── Remove from index (spec 04b EV2) ────────────────
                        let tokens = tokenize(file.path.as_str());
                        for token in &tokens {
                            update_posting(tx_index, token.as_str().as_bytes(), id, false)
                                .map_err(lift_index_codec)?;
                        }

                        Ok(())
                    }
                }
            },
        );

        match result {
            Ok(()) => Ok(()),
            Err(sled::transaction::TransactionError::Abort(TxAbort::NotFound)) => {
                Err(PortError::NotFound)
            }
            Err(sled::transaction::TransactionError::Abort(TxAbort::Corrupt(e))) => {
                Err(to_port_error(StorageError::Codec(e)))
            }
            Err(sled::transaction::TransactionError::Abort(TxAbort::IndexCodec(e))) => {
                Err(to_port_error(StorageError::Codec(e)))
            }
            Err(sled::transaction::TransactionError::Storage(e)) => {
                Err(to_port_error(StorageError::Sled(e)))
            }
            #[cfg(test)]
            Err(sled::transaction::TransactionError::Abort(TxAbort::ForcedAbort)) => {
                Err(to_port_error(StorageError::CorruptMetadata(
                    "forced abort for test".to_owned(),
                )))
            }
        }
    }

    /// Store a raw content blob indexed by its `ContentHash`.
    ///
    /// Blobs are content-addressed and idempotent — no transaction needed.
    fn put_blob(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), PortError> {
        self.blobs
            .insert(blob_key(&hash), bytes)
            .map(|_| ())
            .map_err(|e| to_port_error(StorageError::Sled(e)))
    }

    /// Retrieve a raw content blob by its `ContentHash`.
    ///
    /// Reads with `&self` — no lock (spec ST1).
    fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, PortError> {
        match self
            .blobs
            .get(blob_key(hash))
            .map_err(|e| to_read_error(StorageError::Sled(e)))?
        {
            None => Err(PortError::NotFound),
            Some(bytes) => Ok(bytes.to_vec()),
        }
    }

    /// Durably record that the initial workspace scan completed.
    ///
    /// Writes a single byte `[1]` under `META_SCAN_COMPLETE_KEY` in the `meta`
    /// tree and flushes synchronously so the flag survives a process restart.
    fn mark_scan_complete(&mut self) -> Result<(), PortError> {
        self.meta
            .insert(META_SCAN_COMPLETE_KEY, &[1u8])
            .map_err(|e| to_port_error(StorageError::Sled(e)))?;
        self.meta
            .flush()
            .map_err(|e| to_port_error(StorageError::Sled(e)))?;
        Ok(())
    }

    /// Return `true` if the scan-complete flag is set in the `meta` tree.
    ///
    /// Reads with `&self` — no application-level lock (spec ST1).
    fn is_scan_complete(&self) -> Result<bool, PortError> {
        match self
            .meta
            .get(META_SCAN_COMPLETE_KEY)
            .map_err(|e| to_read_error(StorageError::Sled(e)))?
        {
            Some(bytes) => Ok(bytes.first() == Some(&1u8)),
            None => Ok(false),
        }
    }
}

// ── Index-tree helpers ────────────────────────────────────────────────────────

/// Read, mutate, and write back the posting list for a single token.
///
/// `add = true`  → insert `file_id` (idempotent).
/// `add = false` → remove `file_id`; drops the key entirely when the list
///                 becomes empty.
///
/// Called inside a sled transaction closure, so `tx` is a
/// `sled::transaction::TransactionalTree`.
///
/// Returns `Err(postcard::Error)` if serialisation of the updated list fails.
/// Sled I/O errors from `tx.get`, `tx.remove`, and `tx.insert` are propagated
/// via `?` — the `From<UnabortableTransactionError> for
/// ConflictableTransactionError<postcard::Error>` impl in sled converts them
/// transparently so the caller can map the return value to a single
/// `ConflictableTransactionError<TxAbort>`.
fn update_posting(
    tx: &sled::transaction::TransactionalTree,
    token_key: &[u8],
    file_id: FileId,
    add: bool,
) -> Result<(), ConflictableTransactionError<postcard::Error>> {
    let mut posting: Vec<FileId> = match tx.get(token_key)? {
        Some(bytes) => {
            postcard::from_bytes(bytes.as_ref()).map_err(ConflictableTransactionError::Abort)?
        }
        None => Vec::new(),
    };

    if add {
        if !posting.contains(&file_id) {
            posting.push(file_id);
        }
    } else {
        posting.retain(|&id| id != file_id);
    }

    if posting.is_empty() {
        // Prune empty lists to keep the index tree lean.
        tx.remove(token_key)?;
    } else {
        let bytes = postcard::to_allocvec(&posting).map_err(ConflictableTransactionError::Abort)?;
        tx.insert(token_key, bytes.as_slice())?;
    }

    Ok(())
}

// ── Utility ───────────────────────────────────────────────────────────────────

/// Convert a `ConflictableTransactionError<postcard::Error>` (returned by
/// [`update_posting`]) into a `ConflictableTransactionError<TxAbort>` so it
/// can be propagated with `?` inside a transaction closure whose abort type is
/// `TxAbort`.
///
/// - `Abort(e)` → `Abort(TxAbort::IndexCodec(e))`: a serialisation failure in
///   the index tree aborts the whole transaction with the `IndexCodec` reason.
/// - `Conflict` and `Storage` variants pass through unchanged.
fn lift_index_codec(
    e: ConflictableTransactionError<postcard::Error>,
) -> ConflictableTransactionError<TxAbort> {
    match e {
        ConflictableTransactionError::Abort(codec_err) => {
            ConflictableTransactionError::Abort(TxAbort::IndexCodec(codec_err))
        }
        ConflictableTransactionError::Conflict => ConflictableTransactionError::Conflict,
        ConflictableTransactionError::Storage(sled_err) => {
            ConflictableTransactionError::Storage(sled_err)
        }
    }
}

/// Decode a 4-byte little-endian `u32` from a byte slice.
fn read_u32_le(bytes: &[u8]) -> Option<u32> {
    let arr: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}

/// Map a `TransactionError<TxAbort>` to a [`StorageError`].
fn map_tx_abort_error(e: sled::transaction::TransactionError<TxAbort>) -> StorageError {
    match e {
        sled::transaction::TransactionError::Storage(inner) => StorageError::Sled(inner),
        sled::transaction::TransactionError::Abort(TxAbort::NotFound) => {
            // Callers of put() never abort with NotFound — only delete() does.
            // If this path is somehow reached, surface it as metadata corruption.
            StorageError::CorruptMetadata("unexpected NotFound abort in put".to_owned())
        }
        sled::transaction::TransactionError::Abort(TxAbort::Corrupt(e)) => StorageError::Codec(e),
        sled::transaction::TransactionError::Abort(TxAbort::IndexCodec(e)) => {
            StorageError::Codec(e)
        }
        #[cfg(test)]
        sled::transaction::TransactionError::Abort(TxAbort::ForcedAbort) => {
            StorageError::CorruptMetadata("forced abort for test".to_owned())
        }
    }
}
