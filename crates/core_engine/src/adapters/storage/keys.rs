//! Key encoding helpers for Sled tree lookups.
//!
//! All key encodings are deterministic. The key format uses fixed-width
//! big-endian encoding for numeric components so that Sled's lexicographic
//! iteration order matches natural numeric order (useful for future range scans).
//!
//! # Important
//!
//! Keys are used *only* for lookup. The authoritative `FileId` is always read
//! from the *value* (the postcard-serialised `VirtualFile`), never decoded from
//! the key. This avoids needing a non-test `FileId` constructor.

use crate::domain::{ContentHash, FileId, RelativePath};

/// Encode a [`FileId`] as an 8-byte big-endian key: `index (4B) ++ generation (4B)`.
///
/// Big-endian ensures Sled's lexicographic scan order matches numeric slot order.
pub(super) fn file_id_key(id: FileId) -> [u8; 8] {
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(&id.index().to_be_bytes());
    key[4..].copy_from_slice(&id.generation().to_be_bytes());
    key
}

/// Encode a [`RelativePath`] as its raw UTF-8 bytes.
///
/// Paths are valid UTF-8 strings. Using raw bytes avoids an extra allocation
/// and lets Sled store them directly.
pub(super) fn path_key(path: &RelativePath) -> &[u8] {
    path.as_str().as_bytes()
}

/// A [`ContentHash`]'s raw 32 bytes used directly as blob tree key.
pub(super) fn blob_key(hash: &ContentHash) -> &[u8; 32] {
    hash.as_bytes()
}
