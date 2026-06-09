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

use crate::domain::{DomainError, RelativePath};

// ── Supporting value types (minimal placeholders) ───────────────────────────

/// A single text-search hit.
///
/// **Placeholder** — spec 07 will add line number, column, and surrounding
/// context once the search algorithm is implemented.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Match {
    /// The file that contains the match.
    pub path: RelativePath,
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
}

/// Mutation operations over the workspace.
///
/// # Object safety
///
/// Object-safe: mutation methods take `&mut self` and return owned values.
pub trait FileMutationUseCase {
    /// Create a new file at `path` with `content`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::DuplicatePath`] if the path already exists.
    fn create_file(&mut self, path: RelativePath, content: Vec<u8>) -> Result<(), DomainError>;

    /// Create a directory entry at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::DuplicatePath`] if the path already exists.
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
