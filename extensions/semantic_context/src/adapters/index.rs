use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{
    ChunkingFingerprint, EmbeddingSpaceIdentity, GenerationId, IndexCompatibility,
    IndexCompatibilityMismatch, IndexCompatibilityStatus, IndexedChunk, PathReplacement,
    PathRevision, RebuiltGeneration, RevisionOutcome, SemanticIndexError, SemanticIndexPort,
    SemanticSnapshot, WorkspacePath,
};

#[derive(Clone)]
enum PathState {
    Present {
        revision: PathRevision,
        indexed_chunks: Vec<IndexedChunk>,
    },
    Deleted {
        revision: PathRevision,
    },
}

impl PathState {
    fn revision(&self) -> PathRevision {
        match self {
            Self::Present { revision, .. } | Self::Deleted { revision } => *revision,
        }
    }
}

#[derive(Clone)]
struct IndexState {
    generation: GenerationId,
    paths: BTreeMap<WorkspacePath, PathState>,
}

struct Inner {
    active: Arc<IndexState>,
    fail_next_commit: bool,
}

pub struct InMemorySemanticIndex {
    compatibility: IndexCompatibility,
    inner: Mutex<Inner>,
}

impl InMemorySemanticIndex {
    pub fn new() -> Self {
        Self::with_compatibility(IndexCompatibility {
            schema_version: 1,
            chunking_fingerprint: ChunkingFingerprint {
                algorithm_version: "v1".to_owned(),
                target_bytes: 1_024,
                overlap_bytes: 128,
                max_file_bytes: 1_048_576,
            },
            embedding_space: EmbeddingSpaceIdentity {
                provider: "contract-provider".to_owned(),
                model: "contract-model".to_owned(),
                dimensions: Some(3),
            },
            dimensions: Some(3),
        })
    }

    pub fn with_compatibility(compatibility: IndexCompatibility) -> Self {
        Self {
            compatibility,
            inner: Mutex::new(Inner {
                active: Arc::new(IndexState {
                    generation: GenerationId(0),
                    paths: BTreeMap::new(),
                }),
                fail_next_commit: false,
            }),
        }
    }

    /// Makes the next state-changing operation fail at the persistence boundary.
    ///
    /// This deterministic seam models the commit failure that persistent adapters
    /// must handle without exposing a partially published state.
    pub fn fail_next_persistence(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .fail_next_commit = true;
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, Inner>, SemanticIndexError> {
        self.inner.lock().map_err(|_| SemanticIndexError::Storage)
    }

    fn commit(inner: &mut Inner, next: IndexState) -> Result<(), SemanticIndexError> {
        if inner.fail_next_commit {
            inner.fail_next_commit = false;
            return Err(SemanticIndexError::Storage);
        }
        inner.active = Arc::new(next);
        Ok(())
    }
}

impl Default for InMemorySemanticIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticIndexPort for InMemorySemanticIndex {
    fn compatibility(
        &self,
        expected: &IndexCompatibility,
    ) -> Result<IndexCompatibilityStatus, SemanticIndexError> {
        let mismatch = if expected.schema_version != self.compatibility.schema_version {
            Some(IndexCompatibilityMismatch::Schema)
        } else if expected.chunking_fingerprint != self.compatibility.chunking_fingerprint {
            Some(IndexCompatibilityMismatch::ChunkingFingerprint)
        } else if expected.embedding_space.provider != self.compatibility.embedding_space.provider
            || expected.embedding_space.model != self.compatibility.embedding_space.model
        {
            Some(IndexCompatibilityMismatch::EmbeddingSpace)
        } else if expected.embedding_space.dimensions
            != self.compatibility.embedding_space.dimensions
            || expected.dimensions != self.compatibility.dimensions
        {
            Some(IndexCompatibilityMismatch::Dimensions)
        } else {
            None
        };

        Ok(match mismatch {
            Some(mismatch) => IndexCompatibilityStatus::ReindexRequired(mismatch),
            None => IndexCompatibilityStatus::Compatible,
        })
    }

    fn snapshot(&self) -> Result<SemanticSnapshot, SemanticIndexError> {
        if self.compatibility.dimensions == Some(0) {
            return Err(SemanticIndexError::Corrupt);
        }
        let active = Arc::clone(&self.lock_state()?.active);
        let paths = active
            .paths
            .iter()
            .filter_map(|(path, state)| {
                matches!(state, PathState::Present { .. }).then_some(path.clone())
            })
            .collect();
        let mut indexed_chunks = active
            .paths
            .values()
            .filter_map(|state| match state {
                PathState::Present { indexed_chunks, .. } => Some(indexed_chunks),
                PathState::Deleted { .. } => None,
            })
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        indexed_chunks.sort_by(|left, right| {
            left.chunk
                .path
                .cmp(&right.chunk.path)
                .then(left.chunk.start_byte.cmp(&right.chunk.start_byte))
                .then(left.chunk.end_byte.cmp(&right.chunk.end_byte))
                .then(left.chunk.chunk_hash.cmp(&right.chunk.chunk_hash))
        });

        Ok(SemanticSnapshot {
            generation: active.generation,
            paths,
            chunks: indexed_chunks,
        })
    }

    fn replace_path(
        &self,
        replacement: PathReplacement,
    ) -> Result<RevisionOutcome, SemanticIndexError> {
        let mut inner = self.lock_state()?;
        if let Some(current) = inner.active.paths.get(&replacement.path) {
            let current_revision = current.revision();
            if replacement.revision < current_revision {
                return Err(SemanticIndexError::StaleRevision {
                    current: current_revision,
                    attempted: replacement.revision,
                });
            }
            if replacement.revision == current_revision {
                return match current {
                    PathState::Present { indexed_chunks, .. }
                        if *indexed_chunks == replacement.indexed_chunks =>
                    {
                        Ok(RevisionOutcome::Unchanged)
                    }
                    _ => Err(SemanticIndexError::StaleRevision {
                        current: current_revision,
                        attempted: replacement.revision,
                    }),
                };
            }
        }

        let mut next = (*inner.active).clone();
        next.paths.insert(
            replacement.path,
            PathState::Present {
                revision: replacement.revision,
                indexed_chunks: replacement.indexed_chunks,
            },
        );
        Self::commit(&mut inner, next)?;
        Ok(RevisionOutcome::Applied)
    }

    fn delete_path(
        &self,
        path: &WorkspacePath,
        revision: PathRevision,
    ) -> Result<RevisionOutcome, SemanticIndexError> {
        let mut inner = self.lock_state()?;
        if let Some(current) = inner.active.paths.get(path) {
            let current_revision = current.revision();
            if revision < current_revision {
                return Err(SemanticIndexError::StaleRevision {
                    current: current_revision,
                    attempted: revision,
                });
            }
            if revision == current_revision {
                return match current {
                    PathState::Deleted { .. } => Ok(RevisionOutcome::Unchanged),
                    PathState::Present { .. } => Err(SemanticIndexError::StaleRevision {
                        current: current_revision,
                        attempted: revision,
                    }),
                };
            }
        }

        let mut next = (*inner.active).clone();
        next.paths
            .insert(path.clone(), PathState::Deleted { revision });
        Self::commit(&mut inner, next)?;
        Ok(RevisionOutcome::Applied)
    }

    fn publish_generation(
        &self,
        generation: RebuiltGeneration,
    ) -> Result<GenerationId, SemanticIndexError> {
        let mut inner = self.lock_state()?;
        let mut next = (*inner.active).clone();
        next.generation = generation.id;
        Self::commit(&mut inner, next)?;
        Ok(generation.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_state_lock_returns_storage_errors() {
        let index = Arc::new(InMemorySemanticIndex::new());
        let poisoner = Arc::clone(&index);
        std::thread::spawn(move || {
            let _guard = poisoner.inner.lock().expect("acquire index state");
            panic!("poison semantic index state");
        })
        .join()
        .expect_err("poisoning thread must panic");

        let replacement = PathReplacement {
            path: WorkspacePath::new("src/lib.rs".to_owned()).expect("valid path"),
            revision: PathRevision(1),
            indexed_chunks: Vec::new(),
        };
        let path = replacement.path.clone();

        assert_eq!(index.snapshot(), Err(SemanticIndexError::Storage));
        assert_eq!(
            index.replace_path(replacement),
            Err(SemanticIndexError::Storage)
        );
        assert_eq!(
            index.delete_path(&path, PathRevision(1)),
            Err(SemanticIndexError::Storage)
        );
        assert_eq!(
            index.publish_generation(RebuiltGeneration {
                id: GenerationId(1),
            }),
            Err(SemanticIndexError::Storage)
        );
    }

    #[test]
    fn zero_dimensions_return_corrupt_snapshot_error() {
        let mut compatibility = InMemorySemanticIndex::new().compatibility;
        compatibility.dimensions = Some(0);
        let index = InMemorySemanticIndex::with_compatibility(compatibility);

        assert_eq!(index.snapshot(), Err(SemanticIndexError::Corrupt));
    }
}
