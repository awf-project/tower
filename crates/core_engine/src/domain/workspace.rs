//! `ProjectWorkspace` — aggregate root over a generational slotmap.

use std::collections::HashMap;

use super::error::DomainError;
use super::file_id::FileId;
use super::virtual_file::{FileMetadata, RelativePath, VirtualFile};

#[derive(Debug)]
struct Slot {
    generation: u32,
    occupant: Option<VirtualFile>,
}

#[derive(Debug, Default)]
pub struct ProjectWorkspace {
    slots: Vec<Slot>,
    path_index: HashMap<RelativePath, FileId>,
    /// Indices of removed slots, available for reuse with a bumped generation.
    free_slots: Vec<u32>,
}

impl ProjectWorkspace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        path: RelativePath,
        metadata: FileMetadata,
    ) -> Result<FileId, DomainError> {
        if self.path_index.contains_key(&path) {
            return Err(DomainError::DuplicatePath);
        }

        let id = match self.free_slots.pop() {
            // Reuse a freed slot, bumping its generation so old handles go stale.
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                // checked_add turns generation exhaustion (unreachable in practice:
                // 4B reuses of one slot) into a panic instead of silently wrapping
                // to 0, which would let a stale FileId alias a new file (spec UN1).
                slot.generation = slot
                    .generation
                    .checked_add(1)
                    .expect("FileId generation overflow");
                FileId::new(index, slot.generation)
            }
            // Otherwise grow the arena with a fresh slot at generation 0.
            None => {
                let index =
                    u32::try_from(self.slots.len()).expect("workspace exceeds u32 slot capacity");
                self.slots.push(Slot {
                    generation: 0,
                    occupant: None,
                });
                FileId::new(index, 0)
            }
        };

        self.slots[id.index() as usize].occupant = Some(VirtualFile {
            id,
            path: path.clone(),
            size: metadata.size,
            modified: metadata.modified,
            content_hash: metadata.content_hash,
        });
        self.path_index.insert(path, id);
        Ok(id)
    }

    pub fn get(&self, id: FileId) -> Result<&VirtualFile, DomainError> {
        let index = self.validated_index(id)?;
        Ok(self.slots[index]
            .occupant
            .as_ref()
            .expect("validated_index guarantees an occupant"))
    }

    pub fn get_by_path(&self, path: &RelativePath) -> Option<FileId> {
        self.path_index.get(path).copied()
    }

    pub fn remove(&mut self, id: FileId) -> Result<VirtualFile, DomainError> {
        let index = self.validated_index(id)?;
        let file = self.slots[index]
            .occupant
            .take()
            .expect("validated_index guarantees an occupant");
        self.path_index.remove(&file.path);
        self.free_slots.push(id.index());
        Ok(file)
    }

    /// Returns the slot index for a `FileId` that still resolves to a live file,
    /// or `StaleHandle` if the index is out of range, the generation no longer
    /// matches, or the slot is empty (spec UN1).
    fn validated_index(&self, id: FileId) -> Result<usize, DomainError> {
        let index = id.index() as usize;
        let slot = self.slots.get(index).ok_or(DomainError::StaleHandle)?;
        if slot.generation != id.generation() || slot.occupant.is_none() {
            return Err(DomainError::StaleHandle);
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::super::virtual_file::{ContentHash, Timestamp};
    use super::*;

    #[test]
    fn insert_then_lookup_by_path_and_id() {
        let mut ws = ProjectWorkspace::new();
        let path = RelativePath::new("src/a.rs");

        let id = ws.insert(path.clone(), FileMetadata::default()).unwrap();

        assert_eq!(ws.get_by_path(&path), Some(id));
        let file = ws.get(id).unwrap();
        assert_eq!(file.id, id);
        assert_eq!(file.path, path);
    }

    #[test]
    fn removed_index_is_reused_with_bumped_generation_and_old_id_goes_stale() {
        let mut ws = ProjectWorkspace::new();
        let a = ws
            .insert(RelativePath::new("src/a.rs"), FileMetadata::default())
            .unwrap();
        assert_eq!((a.index(), a.generation()), (0, 0));

        ws.remove(a).unwrap();
        let b = ws
            .insert(RelativePath::new("src/b.rs"), FileMetadata::default())
            .unwrap();

        // Index reused, generation bumped (spec EV1/EV2/AC2).
        assert_eq!((b.index(), b.generation()), (0, 1));
        // The old handle never resolves to the new file (spec UN1).
        assert_eq!(ws.get(a).unwrap_err(), DomainError::StaleHandle);
        assert!(ws.get(b).is_ok());
    }

    #[test]
    fn duplicate_live_path_is_rejected() {
        let mut ws = ProjectWorkspace::new();
        let path = RelativePath::new("src/a.rs");
        ws.insert(path.clone(), FileMetadata::default()).unwrap();

        assert_eq!(
            ws.insert(path, FileMetadata::default()).unwrap_err(),
            DomainError::DuplicatePath
        );
    }

    #[test]
    fn removing_a_stale_handle_errors() {
        let mut ws = ProjectWorkspace::new();
        let a = ws
            .insert(RelativePath::new("src/a.rs"), FileMetadata::default())
            .unwrap();
        ws.remove(a).unwrap();

        assert_eq!(ws.remove(a).unwrap_err(), DomainError::StaleHandle);
    }

    #[test]
    fn removing_frees_the_path_for_reinsertion() {
        let mut ws = ProjectWorkspace::new();
        let path = RelativePath::new("src/a.rs");
        let a = ws.insert(path.clone(), FileMetadata::default()).unwrap();

        let removed = ws.remove(a).unwrap();
        assert_eq!(removed.id, a);
        assert!(ws.get_by_path(&path).is_none());

        let reinserted = ws.insert(path.clone(), FileMetadata::default()).unwrap();
        assert_eq!(ws.get_by_path(&path), Some(reinserted));
    }

    #[test]
    fn stored_file_preserves_all_metadata_fields() {
        let mut ws = ProjectWorkspace::new();
        let path = RelativePath::new("src/a.rs");
        let metadata = FileMetadata {
            size: 42,
            modified: Timestamp(1_000),
            content_hash: Some(ContentHash::new([0xab; 32])),
        };

        let id = ws.insert(path.clone(), metadata.clone()).unwrap();
        let file = ws.get(id).unwrap();

        assert_eq!(file.path.as_str(), "src/a.rs");
        assert_eq!(file.size, 42);
        assert_eq!(file.modified, Timestamp(1_000));
        assert_eq!(file.content_hash.unwrap().as_bytes(), &[0xab; 32]);

        // `remove` returns the same fully-populated entity.
        assert_eq!(ws.remove(id).unwrap().content_hash, metadata.content_hash);
    }

    #[test]
    fn get_on_a_never_allocated_index_is_stale() {
        let ws = ProjectWorkspace::new();
        assert_eq!(
            ws.get(FileId::new(99, 0)).unwrap_err(),
            DomainError::StaleHandle
        );
    }

    #[test]
    fn free_slots_are_reused_in_lifo_order() {
        let mut ws = ProjectWorkspace::new();
        let a = ws
            .insert(RelativePath::new("a"), FileMetadata::default())
            .unwrap();
        let b = ws
            .insert(RelativePath::new("b"), FileMetadata::default())
            .unwrap();
        assert_eq!((a.index(), b.index()), (0, 1));

        ws.remove(a).unwrap();
        ws.remove(b).unwrap();

        // Last freed (index 1) is reused first.
        let c = ws
            .insert(RelativePath::new("c"), FileMetadata::default())
            .unwrap();
        let d = ws
            .insert(RelativePath::new("d"), FileMetadata::default())
            .unwrap();
        assert_eq!(c.index(), 1);
        assert_eq!(d.index(), 0);
    }

    #[test]
    fn domain_errors_render_distinct_messages() {
        assert_ne!(
            DomainError::StaleHandle.to_string(),
            DomainError::DuplicatePath.to_string()
        );
        assert!(!DomainError::StaleHandle.to_string().is_empty());
    }
}
