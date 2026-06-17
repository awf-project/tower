//! `GlobalReplaceService` — parallel find-and-replace across all indexed files.
//!
//! # Architecture
//!
//! ```text
//!  global_replace(target, replacement, dry_run):
//!
//!  Phase 1 — Parallel read + apply (pure per-file, no shared mutation):
//!    file_ids = workspace.all_file_ids()          // snapshot (read-only)
//!      │ par_iter (Rayon work-stealing — U2)
//!      ▼
//!    for each file_id:
//!      path    = workspace.get(file_id).path      // read-only borrow
//!      bytes   = fs.read(path)                    // read-only through port
//!      text    = str::from_utf8(bytes)            // skip binary
//!      new_txt = text.replace(target, replacement)
//!      count   = occurrences(text, target)
//!      → FilePatch { path, new_content, count }   // owned, no shared state
//!      (non-matching files produce no patch)
//!
//!  Phase 2 — Sequential commit (shared state safely, no races):
//!    collect patches (Vec<FilePatch>) from par_iter
//!    sort by path for determinism
//!    if dry_run: return TxReport from patch counts (no writes, no VFS change)
//!    for each patch:
//!      shadow-file write (tmp + rename) via FileSystemPort    // spec-08 primitive
//!      on success: upsert VFS metadata + update storage record
//!      on failure: record FileReplaceError, continue
//!    put_batch(changed_virtual_files) → StoragePort           // single batch
//!    return TxReport { files_changed, replacements, errors }
//! ```
//!
//! # Concurrency safety reasoning
//!
//! Decision: ONLY the pure computation (read bytes → apply replacement) runs in
//! Rayon's parallel loop. The loop closes over immutable borrows: `workspace: &_`
//! and `fs: &(dyn FileSystemPort + Sync)`. All mutation — FS writes, VFS updates,
//! index updates, storage — happens in a sequential pass after `par_iter` collects
//! its results. This avoids any need for a global write lock inside the loop, which
//! would serialise the work and defeat U2.
//!
//! `dyn FileSystemPort + Sync` is required because Rayon shares the reference
//! across worker threads. Every concrete adapter (`InMemoryFs`, `RealFs`) is
//! `Sync` because their read paths use only shared references or `&self`.
//!
//! # Index invalidation
//!
//! A content-only replace leaves file PATHS unchanged, so the filename inverted-
//! index token sets do not change. Only VirtualFile METADATA (size, content_hash,
//! mtime) could be stale — and `create_file` already zeros those fields (the
//! watcher later updates them via Modify events). After `global_replace` the
//! domain re-calls `create_file` for each changed file, which triggers the same
//! metadata path as spec 08: metadata zeroed, storage record updated. The
//! `search_text` (spec 07) grep is brute-force over live FS content, so it
//! automatically sees the new bytes once the shadow-file rename completes —
//! satisfying AC5 without any extra cache invalidation.
//!
//! # `put_batch` usage
//!
//! After all shadow-file writes succeed, the updated `VirtualFile` records are
//! pushed via `StoragePort::put_batch` in a single all-or-nothing call, giving
//! batch-level storage atomicity for the metadata side of the transaction.
//! Per-file FS-level atomicity is already guaranteed by the shadow-file rename.
#![forbid(unsafe_code)]

use std::collections::HashSet;

use rayon::prelude::*;

use crate::domain::mutation::atomic_write;
use crate::domain::virtual_file::{FileMetadata, Timestamp};
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, RelativePath};
use crate::ports::inbound::{FileReplaceError, TxReport};
use crate::ports::{FileSystemPort, PortError, StoragePort};

// ── Internal type: the result of per-file parallel analysis ──────────────────

/// The pure-computation output for one file that contains the target string.
///
/// Produced in the parallel phase; consumed in the sequential commit phase.
struct FilePatch {
    path: RelativePath,
    /// New file content with all replacements applied.
    new_content: Vec<u8>,
    /// Number of occurrences replaced in this file.
    count: usize,
}

// ── Public service type ───────────────────────────────────────────────────────

/// Implements the parallel global find-and-replace operation (spec 09).
///
/// Composed from spec-07 grep logic (read + search through `FileSystemPort`)
/// and spec-08 atomic write logic (shadow-file via `FileSystemPort::write` +
/// `FileSystemPort::rename`).
///
/// # Ownership model
///
/// Borrows all shared state mutably for the duration of a single
/// `global_replace` or `global_replace_dry_run` call. The `FileSystemPort`
/// must also be `Sync` so the immutable read reference can be shared across
/// Rayon worker threads in the parallel phase.
pub struct GlobalReplaceService<'a> {
    fs: &'a mut dyn FileSystemPort,
    workspace: &'a mut ProjectWorkspace,
    storage: &'a mut dyn StoragePort,
}

impl<'a> GlobalReplaceService<'a> {
    /// Create a new service.
    pub fn new(
        fs: &'a mut dyn FileSystemPort,
        workspace: &'a mut ProjectWorkspace,
        storage: &'a mut dyn StoragePort,
    ) -> Self {
        Self {
            fs,
            workspace,
            storage,
        }
    }

    /// Execute the global replace, writing files when `dry_run` is `false`.
    ///
    /// # Errors
    ///
    /// Returns `DomainError` if the workspace cannot be snapshotted. Per-file
    /// failures are returned in `TxReport::errors`, not as an `Err`.
    pub fn execute(
        &mut self,
        target: &str,
        replacement: &str,
        dry_run: bool,
    ) -> Result<TxReport, DomainError> {
        self.execute_excluding(target, replacement, dry_run, &HashSet::new())
    }

    /// Like [`execute`](Self::execute) but skips every path in `excluded` during
    /// both the dry-run count and the commit phase.
    ///
    /// This is the shared engine behind the optimistic-CAS global replace
    /// (spec 18): the caller pre-computes the set of paths whose content version
    /// drifted under the write-lock and passes them here, so conflicting files
    /// are left untouched while every other match still commits — reusing this
    /// service's parallel read/apply, atomic shadow-file writes, and batched
    /// storage flush instead of re-implementing them.
    ///
    /// # Errors
    ///
    /// Same contract as [`execute`](Self::execute): per-file failures land in
    /// `TxReport::errors`, not the `Err` path.
    pub fn execute_excluding(
        &mut self,
        target: &str,
        replacement: &str,
        dry_run: bool,
        excluded: &HashSet<RelativePath>,
    ) -> Result<TxReport, DomainError> {
        // ── Phase 1: parallel read + apply ────────────────────────────────────
        //
        // Collect a snapshot of all live FileIds (O(n) read, no lock held during
        // the parallel phase).
        let file_ids = self.workspace.all_file_ids();

        // Gather immutable borrow artefacts: we need (path, content) per file.
        // We do this in the *current* thread so we can hold `&self.workspace`
        // without fighting borrow rules inside par_iter.
        //
        // Collect (path, bytes) pairs first — all reads are sequential here
        // because `FileSystemPort` is not `Sync` (it is `&mut`). Only the pure
        // string transformation step runs in parallel.
        let read_results: Vec<(RelativePath, Result<Vec<u8>, PortError>)> = file_ids
            .iter()
            .filter_map(|&fid| {
                let path = self.workspace.get(fid).ok()?.path.clone();
                let bytes = self.fs.read(&path);
                Some((path, bytes))
            })
            .collect();

        // Parallel: apply replacement and count occurrences for each file.
        // This is pure CPU work (no I/O, no shared mutation) — safe to parallelise.
        let patches: Vec<FilePatch> = read_results
            .into_par_iter()
            .filter_map(|(path, read_result)| {
                let bytes = match read_result {
                    Ok(b) => b,
                    // Unreadable file: skip (it will never appear as a match).
                    Err(_) => return None,
                };

                // Skip binary / non-UTF-8 files (same policy as spec-07 grep).
                let text = match std::str::from_utf8(&bytes) {
                    Ok(t) => t,
                    Err(_) => return None,
                };

                // Count occurrences before replacing (str::matches is an exact
                // count of non-overlapping literal occurrences).
                let count = text.matches(target).count();

                // Skip files that do not contain the target.
                if count == 0 {
                    return None;
                }

                let new_content = text.replace(target, replacement).into_bytes();
                Some(FilePatch {
                    path,
                    new_content,
                    count,
                })
            })
            .collect();

        // Sort patches deterministically (by path) so the TxReport output is
        // reproducible regardless of Rayon scheduling order.
        let mut patches = patches;
        patches.sort_by(|a, b| a.path.cmp(&b.path));

        // Drop patches the caller excluded (CAS conflicts — spec 18). Skipped
        // here so neither the dry-run count nor the commit phase touches them.
        if !excluded.is_empty() {
            patches.retain(|p| !excluded.contains(&p.path));
        }

        // ── Dry-run: return report without touching any state ─────────────────
        if dry_run {
            let files_changed = patches.len();
            let replacements = patches.iter().map(|p| p.count).sum();
            return Ok(TxReport {
                files_changed,
                replacements,
                errors: vec![],
            });
        }

        // ── Phase 2: sequential commit ────────────────────────────────────────
        //
        // Write each patched file using the spec-08 shadow-file atomic path, then
        // collect updated VirtualFile records for a single `put_batch` call.

        let mut files_changed: usize = 0;
        let mut total_replacements: usize = 0;
        let mut errors: Vec<FileReplaceError> = Vec::new();
        // VirtualFile records for successfully committed files — batched into
        // storage once all writes complete.
        let mut updated_virtual_files = Vec::new();

        for patch in patches {
            match atomic_write(self.fs, &patch.path, patch.new_content) {
                Ok(()) => {
                    files_changed += 1;
                    total_replacements += patch.count;

                    // Upsert VFS metadata: content-only replace leaves the path
                    // (and therefore the FileId) unchanged. We update the metadata
                    // in-place (zeroed size/mtime — the watcher will fill in real
                    // values via a subsequent Modify event, same as spec 08).
                    if let Some(fid) = self.workspace.get_by_path(&patch.path) {
                        let new_meta = FileMetadata {
                            size: 0,
                            modified: Timestamp(0),
                            content_hash: None,
                        };
                        // update() only fails with StaleHandle; fid came from
                        // get_by_path so it is live — unwrap is safe.
                        self.workspace
                            .update(fid, new_meta)
                            .expect("fid from get_by_path must be live during sequential commit");

                        // Collect updated VirtualFile for put_batch.
                        if let Ok(vf) = self.workspace.get(fid) {
                            updated_virtual_files.push(vf.clone());
                        }
                    }
                    // If get_by_path returns None the file disappeared between the
                    // read phase and the write phase (concurrent external deletion).
                    // The write already happened — we still count the replacement
                    // but we have no VFS record to update.
                }
                Err(reason) => {
                    errors.push(FileReplaceError {
                        path: patch.path,
                        reason,
                    });
                }
            }
        }

        // Flush all updated VirtualFile records to storage in a single batch.
        //
        // A put_batch failure does NOT abort the already-completed FS writes —
        // the shadow-file renames are durable on disk. We record it as a
        // synthetic per-file error so callers receive an accurate TxReport
        // (files_changed reflects what really landed on the FS) rather than an
        // Err that would suggest nothing happened and provoke a re-run that
        // doubles all replacements.
        //
        // Storage metadata will be repaired by the watcher on the next Modify
        // event once the OS notifies about the renamed files.
        if !updated_virtual_files.is_empty()
            && let Err(port_err) = self.storage.put_batch(&updated_virtual_files)
        {
            // Decision: represent the storage failure as a synthetic
            // FileReplaceError with a sentinel path rather than returning
            // Err. This preserves the accurate files_changed / replacements
            // counts and signals to the caller that the FS state is correct
            // but the metadata store is temporarily stale.
            //
            // Trade-off: the sentinel path "(storage)" is not a real
            // RelativePath. If a future caller iterates errors to re-apply
            // them they will see an unfamiliar path. Acceptable: the error
            // message is human-readable and the FS content is correct.
            errors.push(FileReplaceError {
                path: RelativePath::new("(storage)"),
                reason: format!("storage put_batch failed after successful FS writes: {port_err}"),
            });
        }

        // Sort errors deterministically (UN1/AC2 report requirement).
        errors.sort();

        Ok(TxReport {
            files_changed,
            replacements: total_replacements,
            errors,
        })
    }
}
