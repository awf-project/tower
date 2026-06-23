//! Domain errors — explicit enum variants, no I/O.

use super::RelativePath;

/// Errors the domain layer can return. Every failure mode is an explicit
/// variant (spec DoD / UN1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DomainError {
    /// A `FileId` no longer matches its slot — the index was freed or reused
    /// with a higher generation. Never resolves to a different file (spec UN1).
    StaleHandle,
    /// A path already maps to a live `FileId`; the path↔id bijection forbids it
    /// (spec UN2).
    DuplicatePath,
    /// The requested path or entity does not exist in the workspace.
    ///
    /// Used by mutation use-cases (spec 02 inbound port declarations) when an
    /// operation targets a path that has not been indexed.
    NotFound,
    /// The requested path is tracked as a file, so it cannot be listed as a
    /// directory.
    NotADirectory(RelativePath),
    /// A port-level I/O failure crossed the domain boundary.
    ///
    /// The inner string carries the human-readable reason from the port
    /// (e.g. "permission denied"). The domain does not expose OS error types;
    /// only the reason string is preserved (U2).
    ///
    /// Introduced in spec 08 so mutation use-cases can surface write/read
    /// failures without mapping them to an unrelated variant.
    IoError(String),
    /// A byte range supplied to a range-edit operation is invalid.
    ///
    /// Introduced in spec 17 (`tower_edit_range`). The inner string describes
    /// why the range was rejected, e.g.:
    /// - `end` exceeds the file length,
    /// - `start > end`,
    /// - `start` or `end` falls in the middle of a UTF-8 multi-byte sequence.
    ///
    /// No write, VFS update, index update, storage mutation, or broadcast is
    /// performed when this error is returned (UN1/AC3/AC4).
    InvalidRange(String),
    /// Optimistic concurrency check failed: the file's current content hash does
    /// not match the caller's expected version.
    ///
    /// Introduced in spec 18 (compare-and-swap writes). The `expected` field is
    /// the hex SHA-256 the caller read earlier; `actual` is the hash recomputed
    /// under the write-lock at mutation time. No filesystem change has been made.
    ///
    /// The caller must re-read the file, incorporate any changes made by the
    /// winning writer, and retry the mutation.
    VersionConflict {
        /// The hex SHA-256 the caller expected (from a prior `read_file` with
        /// `with_version: true`).
        expected: String,
        /// The hex SHA-256 actually present at mutation time (under the lock),
        /// or the empty string when the target file does not exist (a real
        /// SHA-256 hash is always 64 hex chars, so `""` unambiguously means
        /// "absent").
        actual: String,
    },
}

impl core::fmt::Display for DomainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::StaleHandle => write!(f, "stale file handle: slot freed or reused"),
            Self::DuplicatePath => write!(f, "path already maps to a live file"),
            Self::NotFound => write!(f, "path or entity not found in workspace"),
            Self::NotADirectory(path) => write!(f, "not a directory: {}", path.as_str()),
            Self::IoError(reason) => write!(f, "I/O error: {reason}"),
            Self::InvalidRange(reason) => write!(f, "invalid byte range: {reason}"),
            Self::VersionConflict { expected, actual } => {
                write!(f, "version conflict: expected {expected}, actual {actual}")
            }
        }
    }
}

impl core::error::Error for DomainError {}
