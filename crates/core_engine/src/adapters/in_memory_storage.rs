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
    /// Scan-complete marker (equivalent of the sled meta flag, but in-memory).
    scan_complete: bool,
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

    /// Persist all files in the slice in an all-or-nothing operation.
    ///
    /// # Decision
    ///
    /// `InMemoryStorage` has no real transaction engine. Atomicity is emulated
    /// by collecting the fully-serialised results first (pure computation, no
    /// side-effects), and only then writing into the map. If any step before the
    /// write phase fails the map is not touched, preserving the all-or-nothing
    /// guarantee at the in-memory level.
    ///
    /// Because `VirtualFile` is `Clone` and the HashMap insert is infallible
    /// for in-memory storage, this implementation satisfies the contract for all
    /// realistic in-memory use-cases. Failure-path atomicity is verified by the
    /// `put_batch_atomicity_on_failure` contract test in `test_support`.
    fn put_batch(&mut self, files: &[VirtualFile]) -> Result<(), PortError> {
        // Collect all entries before touching the map so that any pre-write
        // validation failure leaves the map untouched.
        let entries: Vec<(FileId, VirtualFile)> = files.iter().map(|f| (f.id, f.clone())).collect();
        for (id, file) in entries {
            self.files.insert(id, file);
        }
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

    fn mark_scan_complete(&mut self) -> Result<(), PortError> {
        self.scan_complete = true;
        Ok(())
    }

    fn is_scan_complete(&self) -> Result<bool, PortError> {
        Ok(self.scan_complete)
    }
}
