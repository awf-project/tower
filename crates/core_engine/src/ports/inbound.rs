//! Inbound port traits — the use-case API that external drivers (CLI, MCP,
//! language-server) call into the domain.
//!
//! # Status: declaration only (spec 02 scope)
//!
//! The trait signatures and minimal supporting value types are defined here so
//! adapters can be written against a stable API. Method bodies live in:
//! - `SearchUseCase`: spec 03b (find_file) and spec 07 (search_text).
//! - `FileMutationUseCase`: specs 08 / 09.
//!
//! The placeholder types ([`Match`], [`TxReport`]) are deliberately minimal.
//! They will gain fields in the specs cited above.

use crate::domain::{DomainError, FileId, RelativePath};

// ── Supporting value types ───────────────────────────────────────────────────

/// A single text-search hit returned by [`SearchUseCase::search_text`].
///
/// Fleshed out in spec 07 with all four fields required by EV1/U2.
///
/// # Ordering
///
/// Implements `Ord` so that a `Vec<Match>` can be sorted deterministically by
/// `(path, line_number)` regardless of Rayon scheduling order (spec 07 AC1).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Match {
    /// The path of the file that contains the match.
    ///
    /// Listed first so the derived `Ord` sorts by path before `line_number`.
    pub path: RelativePath,

    /// 1-based line number of the matching line (spec U2).
    pub line_number: u32,

    /// Full content of the matching line, trimmed of the trailing newline.
    pub line_content: String,

    /// Stable identity of the file (spec U2).
    ///
    /// Listed last in the struct so it does not participate in the primary sort
    /// key — path + line_number already uniquely identify a hit.
    pub file_id: FileId,
}

/// A per-file error recorded by a [`global_replace`] transaction.
///
/// Partial-failure semantics (UN1/AC2): if a single file cannot be rewritten
/// (e.g. read-only on disk, FS port rejects the write), the operation is
/// recorded here and the transaction continues with the remaining files.
/// Errors are sorted by `path` for deterministic output.
///
/// [`global_replace`]: FileMutationUseCase::global_replace
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct FileReplaceError {
    /// Workspace-relative path of the file that could not be rewritten.
    pub path: RelativePath,
    /// Human-readable reason (from the underlying port error).
    pub reason: String,
}

/// Summary of a `global_replace` transaction (spec 09).
///
/// Returned by [`FileMutationUseCase::global_replace`] after attempting to
/// rewrite every file that contains the search target.
///
/// # Determinism
///
/// `errors` is sorted by `path` so callers get stable output regardless of
/// Rayon scheduling order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TxReport {
    /// Number of files that were successfully rewritten.
    pub files_changed: usize,
    /// Total number of individual string replacements performed across all
    /// successfully rewritten files.
    pub replacements: usize,
    /// Files that could not be rewritten. Empty on full success.
    ///
    /// A non-empty `errors` vec does **not** mean the transaction failed: the
    /// successfully rewritten files are committed regardless (UN1/AC2).
    pub errors: Vec<FileReplaceError>,
}

// ── Inbound port traits ──────────────────────────────────────────────────────

/// Read-only search over the workspace.
///
/// # Object safety
///
/// Object-safe by design: all methods take `&self` and return owned values.
pub trait SearchUseCase {
    /// Find files whose path contains `query` (fuzzy or substring — algorithm
    /// decided in spec 03b).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] if the workspace state is inconsistent.
    fn find_file(&self, query: &str) -> Result<Vec<RelativePath>, DomainError>;

    /// Return all text matches of `pattern` across indexed file content.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] on workspace inconsistency.
    fn search_text(&self, pattern: &str) -> Result<Vec<Match>, DomainError>;

    /// Return up to `cap` text matches of `pattern` across indexed file content.
    ///
    /// When the cap is reached the search stops early and returns partial results
    /// promptly (spec 07 OP1). The default implementation calls [`Self::search_text`]
    /// and truncates after the fact — implementors should override this method to
    /// stop work at the source.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] on workspace inconsistency.
    fn search_text_capped(&self, pattern: &str, cap: usize) -> Result<Vec<Match>, DomainError> {
        let mut results = self.search_text(pattern)?;
        results.truncate(cap);
        Ok(results)
    }
}

/// Mutation operations over the workspace.
///
/// # Object safety
///
/// Object-safe: mutation methods take `&mut self` and return owned values.
pub trait FileMutationUseCase {
    /// Create or overwrite the file at `path` with `content` (upsert semantics).
    ///
    /// If the path already exists the file is overwritten atomically; the
    /// existing `FileId` is preserved and metadata is updated in-place.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] only on I/O failure (e.g. the FS port rejects
    /// the write or rename). A pre-existing path is never an error condition.
    fn create_file(&mut self, path: RelativePath, content: Vec<u8>) -> Result<(), DomainError>;

    /// Create a directory entry at `path` recursively.
    ///
    /// Directories are not tracked in the VFS; this is a pure FS operation.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] if the FS port rejects the mkdir.
    fn create_directory(&mut self, path: RelativePath) -> Result<(), DomainError>;

    /// Delete the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] if the path does not exist.
    fn delete_file(&mut self, path: &RelativePath) -> Result<(), DomainError>;

    /// Replace every occurrence of `target` with `replacement` across all
    /// indexed files, in a single logical transaction.
    ///
    /// Each file is rewritten atomically (shadow-file pattern — spec 08).
    /// If one file fails to rewrite, it is recorded in [`TxReport::errors`]
    /// and the remaining files are still committed (partial-failure semantics,
    /// UN1/AC2).
    ///
    /// The operation is parallelised with Rayon work-stealing (U2).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] only on a catastrophic failure that prevents
    /// the transaction from starting (e.g. workspace inconsistency). Per-file
    /// failures are reported in [`TxReport::errors`], not as an `Err`.
    fn global_replace(&mut self, target: &str, replacement: &str) -> Result<TxReport, DomainError>;

    /// Dry-run variant of [`global_replace`]: compute the would-change report
    /// without writing any file or mutating any state (OP1/AC4).
    ///
    /// Returns the same [`TxReport`] shape as `global_replace`, but all
    /// `files_changed` and `replacements` counts reflect what *would* happen;
    /// no files are written and no VFS/index/storage state is updated.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] only if the workspace cannot be read.
    fn global_replace_dry_run(
        &mut self,
        target: &str,
        replacement: &str,
    ) -> Result<TxReport, DomainError>;

    /// Replace the byte range `[start_byte, end_byte)` in the file at `path`
    /// with `replacement` (spec 17 — surgical range edit).
    ///
    /// # Wireframe
    ///
    /// ```text
    ///  edit_range(path, start_byte, end_byte, replacement):
    ///    bytes  ← FileSystemPort.read(path)           ← must exist (not create)
    ///    validate 0 ≤ start ≤ end ≤ len               ← DomainError::InvalidRange
    ///    validate start and end on UTF-8 char boundary ← DomainError::InvalidRange
    ///    spliced ← bytes[0..start] ++ replacement ++ bytes[end..]
    ///    atomic_write(fs, path, spliced)               ← shadow-file pattern
    ///         │
    ///         ├─ workspace.update (stable FileId)      ┐
    ///         ├─ index.remove + index.insert           ├─ VFS + index in sync
    ///         ├─ storage.put                           ┘
    ///         └─ plugin_host.on_file_changed           ← broadcast after commit
    /// ```
    ///
    /// # Contract
    ///
    /// - **Existing files only**: `path` must already be tracked in the VFS.
    ///   Returns [`DomainError::NotFound`] when the path is absent (UN2).
    /// - **Range validation (UN1/AC3/AC4)**: both `start_byte` and `end_byte`
    ///   must satisfy `0 ≤ start ≤ end ≤ file_len`, and both must fall on a
    ///   UTF-8 character boundary of the current file content. Any violation
    ///   returns [`DomainError::InvalidRange`] and leaves the file **unchanged**.
    /// - **Byte-exact (U2)**: the spliced content is
    ///   `bytes[..start] ++ replacement.as_bytes() ++ bytes[end..]`. No
    ///   normalisation, no re-encoding, no formatting.
    /// - **Atomicity (AC2)**: the write goes through the spec-08 shadow-file
    ///   primitive (`atomic_write`) — a crash mid-write never corrupts the target.
    /// - **Empty replacement** deletes the span; an equal `start == end` inserts
    ///   without removing any content.
    ///
    /// # Return value
    ///
    /// Returns a [`TxReport`] with `files_changed: 1`, `replacements: 1`, and
    /// an empty `errors` vec on success.  A single-file edit either fully
    /// succeeds or returns an `Err` — partial failure is not possible here.
    ///
    /// # Errors
    ///
    /// | Condition                              | Error variant                  |
    /// |----------------------------------------|--------------------------------|
    /// | `path` not in VFS                      | [`DomainError::NotFound`]      |
    /// | `end > len` or `start > end`           | [`DomainError::InvalidRange`]  |
    /// | `start`/`end` not on char boundary     | [`DomainError::InvalidRange`]  |
    /// | FS read/write failure                  | [`DomainError::IoError`]       |
    fn edit_range(
        &mut self,
        path: &RelativePath,
        start_byte: usize,
        end_byte: usize,
        replacement: &str,
    ) -> Result<TxReport, DomainError>;
}
