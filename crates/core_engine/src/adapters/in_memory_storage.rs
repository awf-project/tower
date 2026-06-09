//! `InMemoryStorage` — HashMap-backed [`StoragePort`] test double (spec 02 / AC1).
//!
//! No I/O, no dependencies outside `std`. Suitable for unit tests and for
//! running the port contract suite without a real Sled database.

use std::collections::HashMap;

use crate::domain::{ContentHash, FileId, VirtualFile};
use crate::ports::{PortError, StoragePort};

/// A purely in-memory implementation of [`StoragePort`].
///
/// Backed by two `HashMap`s — one keyed by `FileId`, one keyed by
/// `ContentHash`. All operations are O(1) average.
///
/// # Example
///
/// ```rust
/// use core_engine::adapters::InMemoryStorage;
/// use core_engine::domain::{FileMetadata, ProjectWorkspace, RelativePath};
/// use core_engine::ports::StoragePort;
///
/// // Mint a real FileId through the workspace aggregate.
/// let mut ws = ProjectWorkspace::new();
/// let id = ws.insert(RelativePath::new("src/main.rs"), FileMetadata::default()).unwrap();
/// let file = ws.get(id).unwrap().clone();
///
/// let mut store = InMemoryStorage::default();
/// store.put(file.clone()).unwrap();
/// assert_eq!(store.get(id).unwrap(), file);
/// ```
#[derive(Debug, Default)]
pub struct InMemoryStorage {
    files: HashMap<FileId, VirtualFile>,
    blobs: HashMap<[u8; 32], Vec<u8>>,
}

impl InMemoryStorage {
    /// Create an empty storage.
    pub fn new() -> Self {
        Self::default()
    }
}

impl StoragePort for InMemoryStorage {
    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.files.get(&id).cloned().ok_or(PortError::NotFound)
    }

    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        self.files.insert(file.id, file);
        Ok(())
    }

    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
        self.files
            .remove(&id)
            .map(|_| ())
            .ok_or(PortError::NotFound)
    }

    fn put_blob(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), PortError> {
        self.blobs.insert(*hash.as_bytes(), bytes);
        Ok(())
    }

    fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, PortError> {
        self.blobs
            .get(hash.as_bytes())
            .cloned()
            .ok_or(PortError::NotFound)
    }
}
