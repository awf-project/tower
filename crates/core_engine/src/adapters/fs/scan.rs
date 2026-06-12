//! Initial workspace scan — spec 05b.
//!
//! # Wireframe
//!
//! ```text
//!  scan(root, storage, workspace, index):
//!    ┌─ is_scan_complete? ──► return ScanReport { already_indexed: true }
//!    │
//!    └─ walk(root) respecting ignore rules (ignore crate)
//!         for each eligible file:
//!           metadata via std::fs::metadata  → FileMetadata
//!           workspace.insert(path, meta)    → FileId   (new path)
//!             OR get_by_path(path)          → FileId   (rescan: path already known)
//!           tokenize(path)                  → Vec<Token>
//!           batch.push((VirtualFile, tokens))
//!         flush batch → storage.put_batch (atomic) + index.insert
//!       storage.mark_scan_complete()
//!       return ScanReport { indexed, skipped_permission, skipped_ignore }
//! ```
//!
//! # Walker decision
//!
//! The [`ignore`] crate (ripgrep's) is used because it handles `.gitignore`
//! pattern matching, hidden files (dot-prefix), and symlink-loop detection in a
//! single well-tested crate. The alternative (walkdir + manual gitignore) would
//! require re-implementing everything the ignore crate already does correctly.
//!
//! # Ignore rules
//!
//! - `.git/` is always skipped (implicit in the ignore crate's `WalkBuilder`).
//! - Hidden files (names starting with `.`) are skipped via `hidden(true)`.
//! - `.gitignore` patterns are respected via `add_gitignore(true)` (default).
//! - Caller-supplied extra excludes extend the ignore stack.
//!
//! # Restart / no-rescan
//!
//! The restart guard checks `StoragePort::is_scan_complete()` rather than the
//! workspace file count.  This is the key correctness fix for the
//! partial-scan-then-crash bug: if the process crashes mid-scan, partial data
//! is persisted but the marker is never written, so on restart the guard sees
//! `false` and performs a full rescan.  Re-inserting already-persisted files
//! is idempotent (`put_batch` overwrites), so the rescan cannot corrupt or
//! duplicate state.
//!
//! # Batching (OP1)
//!
//! Files are accumulated into a batch of up to [`BATCH_SIZE`] entries.  Each
//! batch is flushed by calling `StoragePort::put_batch`, which persists all N
//! files in a single atomic sled transaction.  This reduces write amplification
//! compared to N individual `put` transactions while still bounding heap usage
//! to `batch_size` entries at a time.
//!
//! # Scan-completion marker
//!
//! `StoragePort::mark_scan_complete()` is called once, after the final batch
//! flush.  On persistent backends (sled) the flag is written to the `meta`
//! tree and flushed synchronously so it survives a process restart.
//!
//! # Permission errors (UN1/AC3)
//!
//! `std::fs::metadata` failures for individual files (e.g. EACCES) are
//! recorded in `ScanReport::skipped_permission` and the scan continues.
//! The walk never aborts.
//!
//! # Symlink loops (UN2/AC4)
//!
//! The `ignore` walker follows links with `follow_links(false)` (default),
//! so symlinks are never dereferenced and cycles are impossible.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use ignore::{Walk, WalkBuilder};
use sha2::{Digest, Sha256};

use crate::domain::index::InvertedIndex;
use crate::domain::mutation::is_tmp_artifact;
use crate::domain::token::tokenize;
use crate::domain::virtual_file::{ContentHash, FileMetadata, RelativePath, Timestamp};
use crate::domain::workspace::ProjectWorkspace;
use crate::ports::StoragePort;

/// Number of files accumulated per flush cycle (OP1 batching).
///
/// Decision: 256 entries. At typical path lengths this keeps each batch
/// under ~100 KiB of heap while still amortising the per-file overhead of
/// individual `put` calls on the sled transaction path. Adjusted via the
/// batch argument when the caller has different needs (tests use 1 or small
/// values to verify flushing behaviour).
pub const BATCH_SIZE: usize = 256;

/// Summary of a completed scan.
///
/// Returned by [`scan`] so callers can report progress without a logging
/// dependency (YAGNI — logging framework not yet in the project).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// Number of files successfully indexed.
    pub indexed: usize,
    /// Files skipped due to permission errors (EACCES, etc.).
    pub skipped_permission: Vec<PathBuf>,
    /// Files skipped because their path contains non-UTF-8 bytes.
    ///
    /// On Linux, file names may contain arbitrary byte sequences.
    /// [`RelativePath`] requires valid UTF-8 strings, so non-UTF-8 paths
    /// are explicitly skipped rather than silently mangled by
    /// `to_string_lossy`, which would replace invalid bytes with U+FFFD
    /// and could cause two distinct OS paths to collide.
    pub skipped_encoding: usize,
    /// `true` when the workspace was already populated and no walk was performed.
    pub already_indexed: bool,
}

/// Scan `root`, indexing every eligible file.
///
/// # Arguments
///
/// - `root`: the directory to walk.
/// - `storage`: outbound storage port (sled-backed in production).
/// - `workspace`: the domain aggregate. Must be either empty (first run) or
///   pre-populated from a previous session (restart path).
/// - `index`: in-memory inverted index updated in-place.
///
/// # Returns
///
/// A [`ScanReport`] describing what happened.
///
/// # Restart behaviour (AC5)
///
/// If `workspace` already contains any files the walk is skipped entirely
/// and `ScanReport::already_indexed` is `true`.
///
/// # Errors
///
/// Individual file metadata failures are recorded in `ScanReport::skipped_permission`.
/// Storage `put` failures abort the entire scan with the returned error so
/// callers can decide whether to retry or surface the failure.
///
/// # Example
///
/// ```rust,no_run
/// use std::path::Path;
/// use core_engine::adapters::fs::scan::scan;
/// use core_engine::adapters::storage::SledStorageAdapter;
/// use core_engine::domain::workspace::ProjectWorkspace;
/// use core_engine::domain::index::InvertedIndex;
///
/// let root = Path::new("/tmp/my-workspace");
/// let db_dir = Path::new("/tmp/my-workspace-db");
/// let (mut storage, mut workspace, mut index) = SledStorageAdapter::open(db_dir).unwrap();
/// let report = scan(root, &mut storage, &mut workspace, &mut index).unwrap();
/// println!("indexed {} files", report.indexed);
/// ```
pub fn scan(
    root: &Path,
    storage: &mut dyn StoragePort,
    workspace: &mut ProjectWorkspace,
    index: &mut InvertedIndex,
) -> Result<ScanReport, crate::ports::PortError> {
    scan_with_batch_size(root, storage, workspace, index, BATCH_SIZE)
}

/// Internal entry point that exposes `batch_size` for tests.
pub(crate) fn scan_with_batch_size(
    root: &Path,
    storage: &mut dyn StoragePort,
    workspace: &mut ProjectWorkspace,
    index: &mut InvertedIndex,
    batch_size: usize,
) -> Result<ScanReport, crate::ports::PortError> {
    // Restart guard: skip the walk only when a previous scan COMPLETED.
    //
    // Decision: use StoragePort::is_scan_complete() rather than the workspace
    // file count. The file-count check was the source of the
    // partial-scan-then-crash bug: a crash mid-scan persists partial data so
    // the file count is non-zero, but the index is incomplete. The scan-
    // complete marker is written atomically only after the final batch flush,
    // so it can never be set for a partial scan.
    //
    // Idempotency: re-inserting already-persisted files via put_batch is safe
    // because put_batch overwrites existing records.
    if storage.is_scan_complete()? {
        return Ok(ScanReport {
            already_indexed: true,
            ..Default::default()
        });
    }

    let mut report = ScanReport::default();

    // Accumulate (VirtualFile, tokens) pairs; flush every `batch_size` files.
    //
    // Decision: collect into a Vec<_> batch rather than writing one-by-one so
    // the hot-path is a tight loop over entries before hitting sled I/O.
    // Trade-off: up to `batch_size` entries in heap at once — acceptable.
    let mut batch: Vec<(crate::domain::VirtualFile, Vec<crate::domain::token::Token>)> =
        Vec::with_capacity(batch_size);

    let walker = workspace_walker(root);

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(ignore_err) => {
                // Extract all paths from the error.
                //
                // The ignore crate wraps I/O errors in several variants:
                // - `WithPath { path, .. }` carries a single actionable path.
                // - `Partial(vec)` wraps multiple sub-errors, some of which
                //   may themselves be `WithPath` variants.
                // - Other variants (Glob, InvalidDefinition, etc.) carry no
                //   path but still represent a non-fatal failure.
                //
                // Decision: extract paths recursively from Partial so callers
                // can observe all skipped paths. Non-path variants are still
                // counted implicitly — the scan continues regardless.
                collect_ignore_error_paths(&ignore_err, &mut report.skipped_permission);
                continue;
            }
        };

        // Only process regular files (not directories, symlinks, etc.).
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            // stdin pseudo-entry — skip.
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        let abs_path = entry.path();

        // Build a workspace-relative path from the root-relative portion.
        // strip_prefix panics only if abs_path does not start with root,
        // which the walker guarantees.
        let rel_path_os = match abs_path.strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => {
                // Defensive: skip paths that cannot be made relative.
                report.skipped_permission.push(abs_path.to_path_buf());
                continue;
            }
        };

        // Decision: use `to_str()` (returns None for non-UTF-8 paths) rather
        // than `to_string_lossy()` (which replaces invalid bytes with U+FFFD).
        // `to_string_lossy()` would silently mangle non-UTF-8 names, possibly
        // producing collisions that land in `skipped_permission` under a
        // misleading label. An explicit skip with a dedicated counter is honest.
        let rel_str_raw = match rel_path_os.to_str() {
            Some(s) => s,
            None => {
                report.skipped_encoding += 1;
                continue;
            }
        };

        // Normalise path separators to forward-slash on all platforms.
        #[cfg(not(windows))]
        let rel_str: &str = rel_str_raw;
        #[cfg(windows)]
        let rel_str: String = rel_str_raw.replace('\\', "/");

        let rel_path = RelativePath::new(rel_str);

        // UN2/AC5: skip shadow-file temp artifacts — never index them as user files.
        if is_tmp_artifact(&rel_path) {
            continue;
        }

        // Obtain metadata (size, mtime, content hash). A read/permission error
        // here is not fatal — the file is skipped and recorded.
        let metadata = match file_metadata_with_content(abs_path) {
            Some(m) => m,
            None => {
                report.skipped_permission.push(abs_path.to_path_buf());
                continue;
            }
        };

        // Assign a FileId via the workspace aggregate.
        //
        // During a rescan (crash-recovery path), the workspace may already
        // contain this path because it was rebuilt from persisted storage on
        // open().  In that case workspace.insert returns DuplicatePath — we
        // must NOT skip: we reuse the existing FileId and still push the entry
        // into the batch so that put_batch re-persists it and index.insert
        // re-adds the tokens to the in-memory index.
        //
        // Decision: use get_by_path on DuplicatePath rather than treating it
        // as an error.  Any other DomainError (there are none today, but
        // defensive) is treated as a skip.
        let file_id = match workspace.insert(rel_path.clone(), metadata) {
            Ok(id) => id,
            Err(crate::domain::error::DomainError::DuplicatePath) => {
                // Path was already registered (rescan path).  Reuse the
                // existing FileId so the entry still flows through flush_batch
                // and gets added to the in-memory index.
                match workspace.get_by_path(&rel_path) {
                    Some(id) => id,
                    None => {
                        // Workspace invariant violated — treat as a skip.
                        report.skipped_permission.push(abs_path.to_path_buf());
                        continue;
                    }
                }
            }
            Err(_) => {
                // Unexpected domain error — treat as a skip.
                report.skipped_permission.push(abs_path.to_path_buf());
                continue;
            }
        };

        let virtual_file = workspace
            .get(file_id)
            .expect("file_id was just inserted or looked up — slot must be live")
            .clone();

        let tokens = tokenize(rel_path.as_str());

        batch.push((virtual_file, tokens));

        if batch.len() >= batch_size {
            flush_batch(&mut batch, storage, index)?;
        }
    }

    // Flush any remaining entries.
    if !batch.is_empty() {
        flush_batch(&mut batch, storage, index)?;
    }

    // Mark the scan as complete AFTER all data is persisted.
    //
    // Decision: write the marker here, not inside flush_batch, so it is set
    // exactly once — after the full walk — and never mid-scan.  On sled this
    // flushes synchronously, so a crash after this point sees the marker and
    // correctly skips the rescan on restart.  A crash before this point leaves
    // the marker absent; the restart guard then reruns the full walk (idempotent).
    storage.mark_scan_complete()?;

    report.indexed = workspace.snapshot().files.len();
    Ok(report)
}

/// Flush one batch: persist all files atomically via `StoragePort::put_batch`
/// and insert their tokens into the in-memory index. Clears `batch` on success.
///
/// Using `put_batch` instead of individual `put` calls reduces the number of
/// sled transactions from N to 1 per batch (OP1 true batching).
fn flush_batch(
    batch: &mut Vec<(crate::domain::VirtualFile, Vec<crate::domain::token::Token>)>,
    storage: &mut dyn StoragePort,
    index: &mut InvertedIndex,
) -> Result<(), crate::ports::PortError> {
    let files: Vec<crate::domain::VirtualFile> = batch.iter().map(|(f, _)| f.clone()).collect();
    storage.put_batch(&files)?;
    for (file, tokens) in batch.iter() {
        index.insert(file.id, tokens);
    }
    batch.clear();
    Ok(())
}

/// Recursively collect file paths from an [`ignore::Error`] into `out`.
///
/// The ignore crate uses several error variants:
/// - [`ignore::Error::WithPath`] carries a single path for the failing entry.
/// - [`ignore::Error::Partial`] wraps a `Vec` of sub-errors, each of which
///   may itself be a `WithPath` (or another `Partial`).
/// - All other variants (Glob, InvalidDefinition, etc.) carry no path; they
///   are silently skipped here but the scan still continues past them.
///
/// This ensures that permission errors nested inside `Partial` batches are
/// surfaced in `ScanReport::skipped_permission` rather than silently dropped.
fn collect_ignore_error_paths(err: &ignore::Error, out: &mut Vec<PathBuf>) {
    match err {
        ignore::Error::WithPath { path, .. } => {
            out.push(path.clone());
        }
        ignore::Error::Partial(errors) => {
            for sub in errors {
                collect_ignore_error_paths(sub, out);
            }
        }
        // Glob, InvalidDefinition, WithDepth, WithLineNumber, Loop, Io:
        // no actionable path — do not push anything.
        _ => {}
    }
}

/// Build the shared workspace [`Walk`]er: honours `.gitignore`, skips hidden
/// files and `.git/`, never follows symlinks (UN2/AC4).
///
/// Shared by the initial [`scan`] and by [`collect_workspace_files`] (the forced
/// `tower_reindex` path) so both walk the tree with identical rules — preventing
/// the two from drifting apart.
fn workspace_walker(root: &Path) -> Walk {
    WalkBuilder::new(root)
        // Do not follow symlinks — prevents loops (UN2/AC4).
        .follow_links(false)
        // Respect .gitignore files (U1/AC2). `require_git(false)` is essential:
        // by default the ignore crate only applies .gitignore inside a git
        // repository, so a non-git workspace (or a temp dir) would silently
        // index ignored paths. A workspace scanner must honor .gitignore
        // regardless of whether the tree is a git repo.
        .require_git(false)
        .git_ignore(true)
        // Skip hidden files (names starting with '.') (U1).
        .hidden(true)
        // Respect .git/info/exclude and the global gitignore.
        .git_exclude(true)
        .git_global(true)
        .build()
}

/// Recursively walk `root` and return `(path, metadata)` for every eligible
/// file, with **real** size, mtime and content hash.
///
/// This is the disk-walk used by the forced `tower_reindex` tool. Unlike
/// `RealFs::scan()` (which only reports paths written through the adapter this
/// session), this performs a true filesystem walk — so it reflects files created
/// or edited outside the engine. Errors on individual files (permission, vanished
/// mid-walk, non-UTF-8 name) skip that file and continue.
pub(crate) fn collect_workspace_files(root: &Path) -> Vec<(RelativePath, FileMetadata)> {
    let mut out = Vec::new();
    for entry in workspace_walker(root) {
        let Ok(entry) = entry else { continue };
        match entry.file_type() {
            Some(ft) if ft.is_file() => {}
            _ => continue,
        }
        let abs_path = entry.path();
        let Ok(rel_os) = abs_path.strip_prefix(root) else {
            continue;
        };
        let Some(rel_raw) = rel_os.to_str() else {
            continue;
        };
        #[cfg(not(windows))]
        let rel_str: &str = rel_raw;
        #[cfg(windows)]
        let rel_str: String = rel_raw.replace('\\', "/");
        let rel_path = RelativePath::new(rel_str);
        // Never index shadow-file temp artifacts (UN2/AC5).
        if is_tmp_artifact(&rel_path) {
            continue;
        }
        if let Some(meta) = file_metadata_with_content(abs_path) {
            out.push((rel_path, meta));
        }
    }
    out
}

/// Build a [`FileMetadata`] for the file at `abs_path`, reading its content to
/// compute a SHA-256 [`ContentHash`].
///
/// `size` is the authoritative content length, `modified` the mtime mapped to a
/// `Timestamp(unix_secs)` (or `0` where unavailable). Returns `None` if the file
/// cannot be stat'd or read (permission, vanished mid-walk).
///
/// Reading content (rather than just `stat`) is what wires `content_hash`; the
/// trade-off is one read per file. The initial scan is guarded by
/// `is_scan_complete`, so this cost is paid once per fresh index, not per restart.
fn file_metadata_with_content(abs_path: &Path) -> Option<FileMetadata> {
    let std_meta = std::fs::metadata(abs_path).ok()?;
    let bytes = std::fs::read(abs_path).ok()?;
    Some(FileMetadata {
        size: bytes.len() as u64,
        modified: modified_from_std(&std_meta),
        content_hash: Some(content_hash_of(&bytes)),
    })
}

/// Map a `std::fs::Metadata` modification time to a domain [`Timestamp`].
fn modified_from_std(m: &std::fs::Metadata) -> Timestamp {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| Timestamp(d.as_secs()))
        .unwrap_or(Timestamp(0))
}

/// Compute the SHA-256 [`ContentHash`] of `bytes`.
pub(crate) fn content_hash_of(bytes: &[u8]) -> ContentHash {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    ContentHash::new(out)
}
