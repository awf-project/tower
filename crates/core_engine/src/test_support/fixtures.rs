//! Shared test fixtures for contract tests.
//!
//! These helpers construct domain values in a deterministic way so the
//! contract macros produce reproducible, readable assertions.

use crate::domain::{ContentHash, FileId, RelativePath, Timestamp, VirtualFile};

/// Build a minimal `VirtualFile` for use in contract tests.
///
/// The `index` and `generation` identify the `FileId`; `path` is the
/// workspace-relative path string.
pub fn make_virtual_file(index: u32, generation: u32, path: &str) -> VirtualFile {
    VirtualFile {
        id: FileId::new_for_testing(index, generation),
        path: RelativePath::new(path),
        size: 0,
        modified: Timestamp(0),
        content_hash: None,
    }
}

/// A deterministic `ContentHash` for use in contract tests.
///
/// All bytes are `0xAB` so the value is easy to recognise in assertion output.
pub fn sample_content_hash() -> ContentHash {
    ContentHash::new([0xAB; 32])
}
