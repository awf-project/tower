//! `InMemoryStorage` — HashMap-backed [`StoragePort`] test double (spec 02 / AC1).
//!
//! No I/O, no dependencies outside `std`. Suitable for unit tests and for
//! running the port contract suite without a real Sled database.

use std::collections::{HashMap, HashSet};

use crate::domain::token::{Token, tokenize};
use crate::domain::{ContentHash, FileId, RelativePath, VirtualFile};
use crate::ports::{PortError, ReconciliationDelta, StoragePort};

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
    paths: HashMap<RelativePath, FileId>,
    postings: HashMap<Token, HashSet<FileId>>,
    slot_generations: Vec<u32>,
    free_slots: HashSet<u32>,
    blobs: HashMap<[u8; 32], Vec<u8>>,
    /// Scan-complete marker (equivalent of the sled meta flag, but in-memory).
    scan_complete: bool,
    #[cfg(any(test, feature = "testing"))]
    reconciliation_survivor_rewrites: usize,
    #[cfg(any(test, feature = "testing"))]
    fail_next_reconciliation_before_commit: bool,
}

impl InMemoryStorage {
    /// Create an empty storage.
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn reconciliation_survivor_rewrites_for_test(&self) -> usize {
        self.reconciliation_survivor_rewrites
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn fail_next_reconciliation_before_commit_for_test(&mut self) {
        self.fail_next_reconciliation_before_commit = true;
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn file_id_for_path_for_test(&self, path: &RelativePath) -> Option<FileId> {
        self.paths.get(path).copied()
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn search_ids_for_test(&self, query: &str) -> Vec<FileId> {
        let mut ids: Vec<_> = self
            .postings
            .get(&Token::new(query))
            .into_iter()
            .flatten()
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn slot_generation_for_test(&self, index: u32) -> Option<u32> {
        self.slot_generations.get(index as usize).copied()
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn slot_is_free_for_test(&self, index: u32) -> bool {
        self.free_slots.contains(&index)
    }

    fn stage_put(
        files: &mut HashMap<FileId, VirtualFile>,
        paths: &mut HashMap<RelativePath, FileId>,
        postings: &mut HashMap<Token, HashSet<FileId>>,
        slot_generations: &mut Vec<u32>,
        free_slots: &mut HashSet<u32>,
        file: VirtualFile,
    ) {
        if let Some(old_file) = files.insert(file.id, file.clone()) {
            paths.remove(&old_file.path);
            Self::remove_postings(postings, old_file.id);
        }

        paths.insert(file.path.clone(), file.id);
        for token in tokenize(file.path.as_str()) {
            postings.entry(token).or_default().insert(file.id);
        }

        let slot = file.id.index() as usize;
        if slot_generations.len() <= slot {
            slot_generations.resize(slot + 1, 0);
        }
        slot_generations[slot] = file.id.generation();
        free_slots.remove(&file.id.index());
    }

    fn stage_remove(
        files: &mut HashMap<FileId, VirtualFile>,
        paths: &mut HashMap<RelativePath, FileId>,
        postings: &mut HashMap<Token, HashSet<FileId>>,
        free_slots: &mut HashSet<u32>,
        id: FileId,
    ) -> Option<VirtualFile> {
        let file = files.remove(&id)?;
        paths.remove(&file.path);
        Self::remove_postings(postings, id);
        free_slots.insert(id.index());
        Some(file)
    }

    fn remove_postings(postings: &mut HashMap<Token, HashSet<FileId>>, id: FileId) {
        postings.retain(|_, ids| {
            ids.remove(&id);
            !ids.is_empty()
        });
    }
}

impl StoragePort for InMemoryStorage {
    fn apply_reconciliation_delta(&mut self, delta: &ReconciliationDelta) -> Result<(), PortError> {
        #[cfg(any(test, feature = "testing"))]
        let fail_before_commit = std::mem::take(&mut self.fail_next_reconciliation_before_commit);

        let mut staged_files = self.files.clone();
        let mut staged_paths = self.paths.clone();
        let mut staged_postings = self.postings.clone();
        let mut staged_slot_generations = self.slot_generations.clone();
        let mut staged_free_slots = self.free_slots.clone();
        for id in &delta.removals {
            Self::stage_remove(
                &mut staged_files,
                &mut staged_paths,
                &mut staged_postings,
                &mut staged_free_slots,
                *id,
            );
        }
        for file in &delta.additions {
            Self::stage_put(
                &mut staged_files,
                &mut staged_paths,
                &mut staged_postings,
                &mut staged_slot_generations,
                &mut staged_free_slots,
                file.clone(),
            );
        }

        #[cfg(any(test, feature = "testing"))]
        if fail_before_commit {
            return Err(PortError::ReconciliationFailed {
                operation: "apply_reconciliation_delta",
                reason: "injected failure before staged reconciliation commit".to_owned(),
            });
        }

        self.files = staged_files;
        self.paths = staged_paths;
        self.postings = staged_postings;
        self.slot_generations = staged_slot_generations;
        self.free_slots = staged_free_slots;
        Ok(())
    }

    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.files.get(&id).cloned().ok_or(PortError::NotFound)
    }

    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        Self::stage_put(
            &mut self.files,
            &mut self.paths,
            &mut self.postings,
            &mut self.slot_generations,
            &mut self.free_slots,
            file,
        );
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
        let mut staged_files = self.files.clone();
        let mut staged_paths = self.paths.clone();
        let mut staged_postings = self.postings.clone();
        let mut staged_slot_generations = self.slot_generations.clone();
        let mut staged_free_slots = self.free_slots.clone();
        for file in files {
            Self::stage_put(
                &mut staged_files,
                &mut staged_paths,
                &mut staged_postings,
                &mut staged_slot_generations,
                &mut staged_free_slots,
                file.clone(),
            );
        }
        self.files = staged_files;
        self.paths = staged_paths;
        self.postings = staged_postings;
        self.slot_generations = staged_slot_generations;
        self.free_slots = staged_free_slots;
        Ok(())
    }

    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
        Self::stage_remove(
            &mut self.files,
            &mut self.paths,
            &mut self.postings,
            &mut self.free_slots,
            id,
        )
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
