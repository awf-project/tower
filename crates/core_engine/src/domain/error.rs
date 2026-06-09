//! Domain errors — explicit enum variants, no I/O.

/// Errors the domain layer can return. Every failure mode is an explicit
/// variant (spec DoD / UN1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
}

impl core::fmt::Display for DomainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::StaleHandle => write!(f, "stale file handle: slot freed or reused"),
            Self::DuplicatePath => write!(f, "path already maps to a live file"),
            Self::NotFound => write!(f, "path or entity not found in workspace"),
        }
    }
}

impl core::error::Error for DomainError {}
