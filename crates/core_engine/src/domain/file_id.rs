//! `FileId` — generational value object identifying a file slot.

/// A copyable handle to a workspace slot (spec U1).
///
/// The `generation` guards against stale handles: when an index is reused after
/// removal it carries a bumped generation, so an old `FileId` can never resolve
/// to a different file (spec UN1). Only the workspace mints `FileId`s.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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
}
