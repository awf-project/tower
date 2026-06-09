//! `FileId` — generational value object identifying a file slot.

use serde::{Deserialize, Serialize};

/// A copyable handle to a workspace slot (spec U1).
///
/// The `generation` guards against stale handles: when an index is reused after
/// removal it carries a bumped generation, so an old `FileId` can never resolve
/// to a different file (spec UN1). Only the workspace mints `FileId`s.
// Decision: derive PartialOrd + Ord on FileId (index first, then generation).
// Why: Match derives Ord with file_id as its last field; sorting by (path,
// line_number) is the primary key but Ord on the whole struct requires every
// field to be Ord. The lexicographic order (index, generation) has no semantic
// meaning in application logic — it only exists to satisfy the derive.
// Trade-off: Ord is now part of the public surface; callers must not interpret
// the ordering as document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct FileId {
    index: u32,
    generation: u32,
}

impl FileId {
    pub(super) fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Construct a `FileId` directly from raw parts for use in tests and
    /// in-memory fakes.
    ///
    /// Only available with `#[cfg(any(test, feature = "testing"))]` so
    /// production code cannot mint arbitrary ids (invariant: only the workspace
    /// aggregate mints `FileId`s).
    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_testing(index: u32, generation: u32) -> Self {
        Self::new(index, generation)
    }
}
