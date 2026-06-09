//! Domain errors — explicit enum variants, no I/O.

/// Errors the `ProjectWorkspace` aggregate can return. Every failure mode is an
/// explicit variant (spec DoD).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomainError {
    /// A `FileId` no longer matches its slot — the index was freed or reused
    /// with a higher generation. Never resolves to a different file (spec UN1).
    StaleHandle,
    /// A path already maps to a live `FileId`; the path↔id bijection forbids it
    /// (spec UN2).
    DuplicatePath,
}

impl core::fmt::Display for DomainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::StaleHandle => write!(f, "stale file handle: slot freed or reused"),
            Self::DuplicatePath => write!(f, "path already maps to a live file"),
        }
    }
}

impl core::error::Error for DomainError {}
