//! Initial workspace scan — spec 05b.
//!
//! # Wireframe
//!
//! ```text
//!  scan(root, storage, workspace, index):
//!    ┌─ already_populated? ──► return ScanReport { skipped_because_populated: true }
//!    │
//!    └─ walk(root) respecting ignore rules (ignore crate)
//!         for each eligible file:
//!           metadata via std::fs::metadata  → FileMetadata
//!           workspace.insert(path, meta)    → FileId
//!           tokenize(path)                  → Vec<Token>
//!           batch.push((VirtualFile, tokens))
//!         flush batch → storage.put + index.insert
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
//! If the workspace already contains at least one file (i.e., a previous scan
//! was persisted), `scan` returns immediately without walking the filesystem.
//! The caller detects this via `ScanReport::already_indexed`.
//!
//! # Batching (OP1)
//!
//! Files are accumulated into a batch of up to [`BATCH_SIZE`] entries.  Each
//! batch is flushed by calling `StoragePort::put` for every entry in the batch
//! and then inserting all tokens into the in-memory index.
//!
//! **Scope note**: "batching" in spec OP1 is defined here as *memory
//! accumulation* — the hot-path is a tight loop that avoids calling into sled
//! for each file individually during the walk, instead grouping the sled I/O
//! into bounded bursts.  Each individual `StoragePort::put` call is still one
//! sled transaction (specs 04a/04b require this for atomicity per file), so
//! this does not reduce the total number of sled transactions.  Adding a
//! `put_batch` method to `StoragePort` that wraps all N files in one sled
//! `Transactional` call would reduce write amplification further but is
//! deferred to a future spec (it requires a storage-port contract change).
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

use ignore::WalkBuilder;

use crate::domain::index::InvertedIndex;
use crate::domain::token::tokenize;
use crate::domain::virtual_file::{FileMetadata, RelativePath, Timestamp};
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
    // AC5 / EV2: if the workspace already has files from a previous scan,
    // skip the walk entirely.
    //
    // Decision: use workspace.snapshot().files.is_empty() as the population
    // check. This is O(n) on the slot count but n is bounded by the number of
    // files, which is exactly the information we need. An alternative would be
    // to expose a `file_count()` on ProjectWorkspace — but that would add API
    // surface to the domain for a single use-site, which violates YAGNI.
    if !workspace.snapshot().files.is_empty() {
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

    let walker = WalkBuilder::new(root)
        // Do not follow symlinks — prevents loops (UN2/AC4).
        .follow_links(false)
        // Respect .gitignore files (U1/AC2).
        .git_ignore(true)
        // Skip hidden files (names starting with '.') (U1).
        .hidden(true)
        // Respect .git/info/exclude and the global gitignore.
        .git_exclude(true)
        .git_global(true)
        .build();

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

        // Obtain metadata (size, mtime). A permission error here is not fatal.
        let metadata = match std::fs::metadata(abs_path) {
            Ok(m) => file_metadata_from_std(&m),
            Err(_) => {
                report.skipped_permission.push(abs_path.to_path_buf());
                continue;
            }
        };

        // Assign a FileId via the workspace aggregate.
        // DuplicatePath is theoretically impossible from a well-formed walk
        // (each path appears once), but handle it defensively by skipping.
        let file_id = match workspace.insert(rel_path.clone(), metadata) {
            Ok(id) => id,
            Err(_) => {
                // Path collision — treat as a skip.
                report.skipped_permission.push(abs_path.to_path_buf());
                continue;
            }
        };

        let virtual_file = workspace
            .get(file_id)
            .expect("just inserted — slot must be live")
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

    report.indexed = workspace.snapshot().files.len();
    Ok(report)
}

/// Flush one batch: persist each file via `StoragePort::put` and insert tokens
/// into the in-memory index. Clears `batch` on success.
fn flush_batch(
    batch: &mut Vec<(crate::domain::VirtualFile, Vec<crate::domain::token::Token>)>,
    storage: &mut dyn StoragePort,
    index: &mut InvertedIndex,
) -> Result<(), crate::ports::PortError> {
    for (file, tokens) in batch.iter() {
        storage.put(file.clone())?;
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

/// Convert a `std::fs::Metadata` value to a domain `FileMetadata`.
///
/// Modification time is mapped to a `Timestamp(unix_secs)`.  On platforms
/// where `modified()` is unavailable (WASM, some embedded targets) we fall
/// back to `Timestamp(0)`.  Content hash is always `None` — the scan only
/// indexes paths, not content (spec 05b scope, see spec 07 for content grep).
fn file_metadata_from_std(m: &std::fs::Metadata) -> FileMetadata {
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| Timestamp(d.as_secs()))
        .unwrap_or(Timestamp(0));

    FileMetadata {
        size: m.len(),
        modified,
        content_hash: None,
    }
}
