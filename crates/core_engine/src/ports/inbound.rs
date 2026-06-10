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

/// Summary of a `global_replace` transaction.
///
/// **Placeholder** — spec 09 will add per-file edit counts, a rollback token,
/// and the list of affected `FileId`s once the transaction engine exists.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TxReport {
    /// Number of individual replacements made across all files.
    pub replacements: usize,
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
    /// # Errors
    ///
    /// Returns [`DomainError`] if the transaction cannot be applied.
    fn global_replace(&mut self, target: &str, replacement: &str) -> Result<TxReport, DomainError>;
}
