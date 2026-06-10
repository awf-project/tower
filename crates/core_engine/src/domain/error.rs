//! Domain errors — explicit enum variants, no I/O.

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
    /// A port-level I/O failure crossed the domain boundary.
    ///
    /// The inner string carries the human-readable reason from the port
    /// (e.g. "permission denied"). The domain does not expose OS error types;
    /// only the reason string is preserved (U2).
    ///
    /// Introduced in spec 08 so mutation use-cases can surface write/read
    /// failures without mapping them to an unrelated variant.
    IoError(String),
}

impl core::fmt::Display for DomainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::StaleHandle => write!(f, "stale file handle: slot freed or reused"),
            Self::DuplicatePath => write!(f, "path already maps to a live file"),
            Self::NotFound => write!(f, "path or entity not found in workspace"),
            Self::IoError(reason) => write!(f, "I/O error: {reason}"),
        }
    }
}

impl core::error::Error for DomainError {}
