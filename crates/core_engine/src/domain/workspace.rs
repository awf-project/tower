//! `ProjectWorkspace` — aggregate root over a generational slotmap.

use std::collections::{HashMap, HashSet};

use super::error::DomainError;
use super::file_id::FileId;
use super::refactor::{PathMove, PathRenameError};
use super::virtual_file::{FileMetadata, RelativePath, VirtualFile};
use crate::ports::inbound::DirectoryEntryKind;

/// Snapshot of the allocator state used when persisting and restoring a
/// `ProjectWorkspace` across restarts (spec 04a EV1/AC2).
///
/// The adapter serialises this into the `meta` Sled tree alongside the
/// individual `VirtualFile` records in the `files` tree.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    /// Per-slot generation counters, indexed by slot position. A slot at index
    /// `i` with generation `g` and an occupant means slot `i` is live.
    pub slot_generations: Vec<u32>,
    /// Indices of freed (tombstoned) slots available for reuse (LIFO order).
    pub free_slots: Vec<u32>,
    /// Live `VirtualFile` records, in arbitrary order.
    pub files: Vec<VirtualFile>,
}

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
                // Checked arithmetic makes generation exhaustion explicit after
                // 4B reuses of one slot. Silent wrapping to zero could let a
                // stale FileId alias a new file (spec UN1).
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

    pub fn remap_paths(&mut self, moves: &[PathMove]) -> Result<(), PathRenameError> {
        let mut destinations = HashSet::with_capacity(moves.len());
        let mut file_moves = HashMap::with_capacity(moves.len());
        let mut directory_move = None;

        for path_move in moves {
            if path_move.source == path_move.destination {
                return Err(PathRenameError::IdenticalPaths {
                    path: path_move.source.clone(),
                });
            }
            if !destinations.insert(path_move.destination.clone()) {
                return Err(PathRenameError::DuplicateDestination {
                    path: path_move.destination.clone(),
                });
            }

            match path_move.kind {
                DirectoryEntryKind::File => {
                    if !self.path_index.contains_key(&path_move.source) {
                        return Err(PathRenameError::SourceNotFound {
                            path: path_move.source.clone(),
                        });
                    }
                    if file_moves
                        .insert(path_move.source.clone(), path_move.destination.clone())
                        .is_some()
                    {
                        return Err(PathRenameError::SemanticBackend);
                    }
                }
                DirectoryEntryKind::Dir => {
                    if descendant_suffix(&path_move.destination, &path_move.source).is_some() {
                        return Err(PathRenameError::DestinationInsideSource {
                            source: path_move.source.clone(),
                            destination: path_move.destination.clone(),
                        });
                    }
                    if directory_move.replace(path_move).is_some() {
                        return Err(PathRenameError::SemanticBackend);
                    }
                }
            }
        }

        if let Some(root) = directory_move {
            for path_move in moves
                .iter()
                .filter(|path_move| path_move.kind == DirectoryEntryKind::File)
            {
                let Some(source_suffix) = descendant_suffix(&path_move.source, &root.source) else {
                    return Err(PathRenameError::SemanticBackend);
                };
                let Some(destination_suffix) =
                    descendant_suffix(&path_move.destination, &root.destination)
                else {
                    return Err(PathRenameError::SemanticBackend);
                };
                if source_suffix != destination_suffix {
                    return Err(PathRenameError::SemanticBackend);
                }
            }
        }

        let mut changes = Vec::new();
        for slot in &self.slots {
            let Some(file) = slot.occupant.as_ref() else {
                continue;
            };
            let destination = file_moves.get(&file.path).cloned().or_else(|| {
                directory_move.and_then(|root| {
                    descendant_suffix(&file.path, &root.source).map(|suffix| {
                        RelativePath::new(format!("{}{suffix}", root.destination.as_str()))
                    })
                })
            });
            if let Some(destination) = destination {
                changes.push((file.id, file.path.clone(), destination));
            }
        }

        let mut staged_index = self.path_index.clone();
        for (_, source, _) in &changes {
            staged_index.remove(source);
        }
        if let Some(root) = directory_move
            && staged_index.contains_key(&root.destination)
        {
            return Err(PathRenameError::DestinationExists {
                path: root.destination.clone(),
            });
        }
        for (id, _, destination) in &changes {
            if let Some(existing_id) = staged_index.insert(destination.clone(), *id) {
                return Err(
                    if changes
                        .iter()
                        .any(|(moved_id, _, _)| *moved_id == existing_id)
                    {
                        PathRenameError::DuplicateDestination {
                            path: destination.clone(),
                        }
                    } else {
                        PathRenameError::DestinationExists {
                            path: destination.clone(),
                        }
                    },
                );
            }
        }

        for (id, _, destination) in changes {
            self.slots[id.index() as usize]
                .occupant
                .as_mut()
                .expect("remap candidates come from live slots")
                .path = destination;
        }
        self.path_index = staged_index;
        Ok(())
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

    /// Capture the full allocator state as a [`WorkspaceSnapshot`].
    ///
    /// Used by `SledStorageAdapter` to persist workspace state to the `meta`
    /// tree so that a restart can reconstruct the exact same workspace
    /// (spec 04a EV1/AC2).
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        let slot_generations: Vec<u32> = self.slots.iter().map(|s| s.generation).collect();
        let free_slots = self.free_slots.clone();
        let files: Vec<VirtualFile> = self
            .slots
            .iter()
            .filter_map(|s| s.occupant.as_ref().cloned())
            .collect();
        WorkspaceSnapshot {
            slot_generations,
            free_slots,
            files,
        }
    }

    /// Reconstruct a `ProjectWorkspace` from a persisted [`WorkspaceSnapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::DuplicatePath`] if two files in the snapshot
    /// carry the same path (corrupt snapshot).
    pub fn from_snapshot(snapshot: WorkspaceSnapshot) -> Result<Self, DomainError> {
        // Build an index of files by slot index so we can populate occupants.
        let mut file_by_index: HashMap<u32, VirtualFile> = HashMap::new();
        for file in snapshot.files {
            file_by_index.insert(file.id.index(), file);
        }

        let mut slots: Vec<Slot> = snapshot
            .slot_generations
            .into_iter()
            .enumerate()
            .map(|(i, generation)| Slot {
                generation,
                occupant: file_by_index.remove(&(i as u32)),
            })
            .collect();

        // Ensure any remaining files with out-of-range indices are appended.
        // (Should never happen in a non-corrupt snapshot, but be defensive.)
        for (index, file) in file_by_index {
            let target = index as usize;
            while slots.len() <= target {
                slots.push(Slot {
                    generation: 0,
                    occupant: None,
                });
            }
            slots[target].occupant = Some(file);
        }

        // Rebuild path index from live occupants.
        let mut path_index: HashMap<RelativePath, FileId> = HashMap::new();
        for slot in &slots {
            if let Some(ref file) = slot.occupant
                && path_index.insert(file.path.clone(), file.id).is_some()
            {
                return Err(DomainError::DuplicatePath);
            }
        }

        Ok(Self {
            slots,
            path_index,
            free_slots: snapshot.free_slots,
        })
    }

    /// Update metadata on an existing live slot without changing its `FileId`.
    ///
    /// This is the correct operation for a Modify event: the file's identity
    /// (path ↔ `FileId` bijection) is preserved; only `size`, `modified`, and
    /// `content_hash` are replaced. The generational counter is **not** bumped,
    /// so existing callers holding the `FileId` continue to resolve to the same
    /// file (spec EV2 — "update metadata/hash", not "reallocate").
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::StaleHandle`] if `id` does not refer to a live
    /// slot (out-of-range index, wrong generation, or empty slot).
    pub fn update(&mut self, id: FileId, metadata: FileMetadata) -> Result<(), DomainError> {
        let index = self.validated_index(id)?;
        let occupant = self.slots[index]
            .occupant
            .as_mut()
            .expect("validated_index guarantees an occupant");
        occupant.size = metadata.size;
        occupant.modified = metadata.modified;
        occupant.content_hash = metadata.content_hash;
        Ok(())
    }

    /// Return a snapshot of all live [`FileId`]s currently in the workspace.
    ///
    /// The snapshot is taken under a read of the internal slot vector so
    /// concurrent writers (if any) do not interfere with the parallel grep
    /// (spec 07 EV1). The returned `Vec` owns its contents; the caller may
    /// freely iterate it on any thread.
    #[must_use]
    pub fn all_file_ids(&self) -> Vec<FileId> {
        self.slots
            .iter()
            .filter_map(|s| s.occupant.as_ref().map(|f| f.id))
            .collect()
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

fn descendant_suffix<'a>(path: &'a RelativePath, root: &RelativePath) -> Option<&'a str> {
    path.as_str()
        .strip_prefix(root.as_str())
        .filter(|suffix| suffix.starts_with('/'))
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

        let id = ws.insert(path.clone(), metadata).unwrap();
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

    /// EV2: update() mutates metadata in-place without changing the FileId.
    #[test]
    fn update_preserves_file_id_and_path() {
        let mut ws = ProjectWorkspace::new();
        let path = RelativePath::new("src/main.rs");
        let id = ws.insert(path.clone(), FileMetadata::default()).unwrap();

        let new_meta = FileMetadata {
            size: 99,
            modified: Timestamp(42),
            content_hash: None,
        };
        ws.update(id, new_meta).unwrap();

        // Same FileId resolves to the updated metadata.
        let file = ws.get(id).unwrap();
        assert_eq!(file.id, id, "FileId must not change on update");
        assert_eq!(file.path, path, "path must not change on update");
        assert_eq!(file.size, 99);
        assert_eq!(file.modified, Timestamp(42));
    }

    /// EV2: update() on a stale handle returns StaleHandle.
    #[test]
    fn update_on_stale_handle_errors() {
        let mut ws = ProjectWorkspace::new();
        let id = ws
            .insert(RelativePath::new("src/a.rs"), FileMetadata::default())
            .unwrap();
        ws.remove(id).unwrap();

        assert_eq!(
            ws.update(id, FileMetadata::default()).unwrap_err(),
            DomainError::StaleHandle
        );
    }

    mod path_remap {
        use crate::ports::inbound::DirectoryEntryKind;

        use super::*;

        fn file_move(source: &str, destination: &str) -> PathMove {
            PathMove {
                source: RelativePath::new(source),
                destination: RelativePath::new(destination),
                kind: DirectoryEntryKind::File,
            }
        }

        fn directory_move(source: &str, destination: &str) -> PathMove {
            PathMove {
                source: RelativePath::new(source),
                destination: RelativePath::new(destination),
                kind: DirectoryEntryKind::Dir,
            }
        }

        fn workspace(paths: &[&str]) -> ProjectWorkspace {
            let mut workspace = ProjectWorkspace::new();
            for path in paths {
                workspace
                    .insert(RelativePath::new(*path), FileMetadata::default())
                    .unwrap();
            }
            workspace
        }

        fn indexed_paths(workspace: &ProjectWorkspace) -> Vec<(RelativePath, FileId)> {
            let mut paths = workspace
                .all_file_ids()
                .into_iter()
                .map(|id| (workspace.get(id).unwrap().path.clone(), id))
                .collect::<Vec<_>>();
            paths.sort_by(|left, right| left.0.cmp(&right.0));
            paths
        }

        fn assert_path_index_unchanged(
            workspace: &ProjectWorkspace,
            before: &[(RelativePath, FileId)],
            moves: &[PathMove],
        ) {
            for (path, id) in before {
                assert_eq!(
                    workspace.get_by_path(path),
                    Some(*id),
                    "original path {path:?} must retain its FileId"
                );
            }

            for path in moves
                .iter()
                .map(|path_move| &path_move.destination)
                .filter(|destination| !before.iter().any(|(path, _)| path == *destination))
            {
                assert_eq!(
                    workspace.get_by_path(path),
                    None,
                    "new destination {path:?} must not be indexed after rejection"
                );
            }
        }

        #[test]
        fn remap_paths_is_all_or_nothing_over_validated_moves() {
            let mut workspace = workspace(&["src/a.rs", "src/b.rs"]);
            let a_id = workspace
                .get_by_path(&RelativePath::new("src/a.rs"))
                .unwrap();
            let b_id = workspace
                .get_by_path(&RelativePath::new("src/b.rs"))
                .unwrap();

            workspace
                .remap_paths(&[
                    file_move("src/a.rs", "lib/a.rs"),
                    file_move("src/b.rs", "lib/b.rs"),
                ])
                .unwrap();

            assert_eq!(
                workspace.get_by_path(&RelativePath::new("lib/a.rs")),
                Some(a_id)
            );
            assert_eq!(
                workspace.get_by_path(&RelativePath::new("lib/b.rs")),
                Some(b_id)
            );
            assert!(
                workspace
                    .get_by_path(&RelativePath::new("src/a.rs"))
                    .is_none()
            );
            assert!(
                workspace
                    .get_by_path(&RelativePath::new("src/b.rs"))
                    .is_none()
            );
        }

        #[test]
        fn preserves_exact_file_identity_and_rewrites_directory_descendants_by_prefix() {
            let mut workspace = workspace(&["src/nested/a.rs", "src/nested/b.rs", "README.md"]);
            let before = indexed_paths(&workspace);

            workspace
                .remap_paths(&[
                    directory_move("src", "lib"),
                    file_move("src/nested/a.rs", "lib/nested/a.rs"),
                    file_move("src/nested/b.rs", "lib/nested/b.rs"),
                ])
                .unwrap();

            for (old_path, id) in before {
                let expected_path = match old_path.as_str() {
                    "src/nested/a.rs" => "lib/nested/a.rs",
                    "src/nested/b.rs" => "lib/nested/b.rs",
                    path => path,
                };
                assert_eq!(
                    workspace.get_by_path(&RelativePath::new(expected_path)),
                    Some(id)
                );
                assert_eq!(workspace.get(id).unwrap().path.as_str(), expected_path);
            }
        }

        #[test]
        fn rejects_collisions_duplicates_missing_sources_and_invalid_prefixes_without_mutation() {
            let cases = [
                (
                    vec![file_move("src/a.rs", "src/existing.rs")],
                    PathRenameError::DestinationExists {
                        path: RelativePath::new("src/existing.rs"),
                    },
                ),
                (
                    vec![
                        file_move("src/a.rs", "lib/shared.rs"),
                        file_move("src/b.rs", "lib/shared.rs"),
                    ],
                    PathRenameError::DuplicateDestination {
                        path: RelativePath::new("lib/shared.rs"),
                    },
                ),
                (
                    vec![file_move("src/missing.rs", "lib/missing.rs")],
                    PathRenameError::SourceNotFound {
                        path: RelativePath::new("src/missing.rs"),
                    },
                ),
                (
                    vec![
                        directory_move("src", "lib"),
                        file_move("src/a.rs", "other/a.rs"),
                    ],
                    PathRenameError::SemanticBackend,
                ),
            ];

            for (moves, expected_error) in cases {
                let mut workspace = workspace(&["src/a.rs", "src/b.rs", "src/existing.rs"]);
                let before = indexed_paths(&workspace);

                assert_eq!(workspace.remap_paths(&moves), Err(expected_error));
                assert_eq!(indexed_paths(&workspace), before);
                assert_path_index_unchanged(&workspace, &before, &moves);
            }
        }

        #[test]
        fn conflict_rejection_rolls_back_every_path_and_identity() {
            let mut workspace = workspace(&["src/a.rs", "src/b.rs", "lib/b.rs"]);
            let before = indexed_paths(&workspace);
            let moves = [
                file_move("src/a.rs", "lib/a.rs"),
                file_move("src/b.rs", "lib/b.rs"),
            ];

            assert_eq!(
                workspace.remap_paths(&moves),
                Err(PathRenameError::DestinationExists {
                    path: RelativePath::new("lib/b.rs"),
                })
            );

            assert_eq!(indexed_paths(&workspace), before);
            assert_path_index_unchanged(&workspace, &before, &moves);
            assert_eq!(
                workspace.get_by_path(&RelativePath::new("lib/a.rs")),
                None,
                "late conflict must not leave the earlier destination indexed"
            );
        }

        #[test]
        fn directory_destination_file_collision_rolls_back_every_path_and_identity() {
            let mut workspace = workspace(&["src/a.rs", "lib"]);
            let before = indexed_paths(&workspace);
            let moves = [
                directory_move("src", "lib"),
                file_move("src/a.rs", "lib/a.rs"),
            ];

            assert_eq!(
                workspace.remap_paths(&moves),
                Err(PathRenameError::DestinationExists {
                    path: RelativePath::new("lib"),
                })
            );

            assert_eq!(indexed_paths(&workspace), before);
            assert_path_index_unchanged(&workspace, &before, &moves);
            assert_eq!(
                workspace.get_by_path(&RelativePath::new("lib/a.rs")),
                None,
                "directory-root conflict must not publish descendant destinations"
            );
        }
    }
}
