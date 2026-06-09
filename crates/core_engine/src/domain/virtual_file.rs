//! `VirtualFile` entity and its value-object fields.
//!
//! The VFS holds *metadata*, never file bytes — content is fetched via ports
//! (spec 02 onward). All types here are pure data.

use super::file_id::FileId;

/// A workspace-relative path. Minimal newtype for now; path validation lands in
/// spec 05 where real filesystem paths are scanned.
///
/// Derives `Ord` / `PartialOrd` (delegated to the inner `String`) so that
/// `Vec<RelativePath>` can be sorted for deterministic search results (spec 03b
/// AC2 ranking).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A domain timestamp (opaque units). Kept I/O-free and deterministic so domain
/// tests never touch the system clock.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Timestamp(pub u64);

/// Content fingerprint (e.g. a 32-byte digest). Opaque to the domain.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Metadata supplied when inserting a file. The workspace mints the `FileId`,
/// so it is not part of the input.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: Timestamp,
    pub content_hash: Option<ContentHash>,
}

/// A file tracked by the workspace (entity). Identity is its `FileId`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VirtualFile {
    pub id: FileId,
    pub path: RelativePath,
    pub size: u64,
    pub modified: Timestamp,
    pub content_hash: Option<ContentHash>,
}
