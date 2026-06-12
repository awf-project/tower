//! Port-level errors — typed failures that cross the port boundary.
//!
//! Outbound port methods return `Result<_, PortError>` so the domain can
//! pattern-match on failures without touching infrastructure types (U2 / UN1).

/// Errors that can be returned by outbound port operations.
///
/// Every variant is explicit and carries enough context for the domain to
/// decide whether to retry, surface to the user, or panic (never silently
/// swallowed).
///
/// # Rename semantics
///
/// [`FileSystemPort::rename`] follows POSIX `rename(2)` semantics: it
/// unconditionally overwrites the destination if it already exists. There is
/// therefore no `AlreadyExists` variant — adapters must not return one for
/// rename operations.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PortError {
    /// The requested key / path was not found in the backing store.
    NotFound,
    /// A write or rename operation failed (e.g. permission denied, disk full).
    ///
    /// The `String` carries a human-readable reason. It is intentionally *not*
    /// a concrete OS error so the domain stays infrastructure-agnostic (U2).
    WriteFailed(String),
    /// A read operation failed.
    ReadFailed(String),
    /// The caller supplied an argument that violates the port's preconditions
    /// (e.g. an empty key, a key containing `/` or `..`).
    ///
    /// The `String` carries a human-readable reason. This variant is
    /// intentionally distinct from [`WriteFailed`] so callers can distinguish
    /// programmer errors (bad key) from runtime I/O errors.
    InvalidArgs(String),
}

impl core::fmt::Display for PortError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound => write!(f, "key or path not found"),
            Self::WriteFailed(reason) => write!(f, "write failed: {reason}"),
            Self::ReadFailed(reason) => write!(f, "read failed: {reason}"),
            Self::InvalidArgs(reason) => write!(f, "invalid arguments: {reason}"),
        }
    }
}

impl core::error::Error for PortError {}
