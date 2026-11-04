use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};

use semantic_context_extension::{
    ChunkingFingerprint, EmbeddingSpaceIdentity, EmbeddingVector, GenerationId, IndexCompatibility,
    IndexCompatibilityMismatch, IndexCompatibilityStatus, IndexedChunk, PathReplacement,
    PathRevision, RebuiltGeneration, RevisionOutcome, SemanticChunk, SemanticChunkInput,
    SemanticIndexError, SemanticIndexPort, WorkspacePath,
    adapters::{index::InMemorySemanticIndex, sqlite::SqliteSemanticIndex},
};
use tempfile::TempDir;

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value.to_owned()).expect("valid contract path")
}

fn fingerprint(version: &str) -> ChunkingFingerprint {
    ChunkingFingerprint {
        algorithm_version: version.to_owned(),
        target_bytes: 1_024,
        overlap_bytes: 128,
        max_file_bytes: 1_048_576,
    }
}

fn compatibility() -> IndexCompatibility {
    IndexCompatibility {
        schema_version: 1,
        chunking_fingerprint: fingerprint("v1"),
        embedding_space: EmbeddingSpaceIdentity {
            provider: "contract-provider".to_owned(),
            model: "contract-model".to_owned(),
            dimensions: Some(3),
        },
        dimensions: Some(3),
    }
}

fn chunk(path_value: &str, source: &str, start: usize, end: usize) -> SemanticChunk {
    SemanticChunk::new(SemanticChunkInput {
        path: path(path_value),
        source,
        start_byte: start,
        end_byte: end,
        language: "rust".to_owned(),
        symbol: None,
        chunking_fingerprint: fingerprint("v1"),
        expected_chunk_hash: None,
    })
    .expect("valid contract chunk")
}

fn replacement(
    path_value: &str,
    revision: u64,
    source: &str,
    spans: &[(usize, usize)],
) -> PathReplacement {
    PathReplacement {
        path: path(path_value),
        revision: PathRevision(revision),
        indexed_chunks: spans
            .iter()
            .enumerate()
            .map(|(index, &(start, end))| IndexedChunk {
                chunk: chunk(path_value, source, start, end),
                embedding: EmbeddingVector::new(
                    vec![index as f32 + 1.0, index as f32 + 2.0, index as f32 + 3.0],
                    Some(3),
                )
                .expect("valid contract embedding"),
            })
            .collect(),
    }
}

fn snapshot_chunk_hashes(index: &dyn SemanticIndexPort, path_value: &str) -> Vec<[u8; 32]> {
    index
        .snapshot()
        .expect("read semantic snapshot")
        .chunks
        .into_iter()
        .filter(|indexed| indexed.chunk.path.as_str() == path_value)
        .map(|indexed| indexed.chunk.chunk_hash)
        .collect()
}

fn snapshot_chunk_order(
    index: &dyn SemanticIndexPort,
) -> Vec<(WorkspacePath, usize, usize, [u8; 32])> {
    index
        .snapshot()
        .expect("read semantic snapshot")
        .chunks
        .into_iter()
        .map(|indexed| {
            (
                indexed.chunk.path,
                indexed.chunk.start_byte,
                indexed.chunk.end_byte,
                indexed.chunk.chunk_hash,
            )
        })
        .collect()
}

fn verify_revision_and_compatibility_contract(index: &dyn SemanticIndexPort) {
    let expected = compatibility();
    assert_eq!(
        index.compatibility(&expected),
        Ok(IndexCompatibilityStatus::Compatible)
    );

    let mut mismatch = expected.clone();
    mismatch.schema_version += 1;
    assert_eq!(
        index.compatibility(&mismatch),
        Ok(IndexCompatibilityStatus::ReindexRequired(
            IndexCompatibilityMismatch::Schema
        ))
    );
    mismatch = expected.clone();
    mismatch.chunking_fingerprint.algorithm_version = "v2".to_owned();
    assert_eq!(
        index.compatibility(&mismatch),
        Ok(IndexCompatibilityStatus::ReindexRequired(
            IndexCompatibilityMismatch::ChunkingFingerprint
        ))
    );
    mismatch = expected.clone();
    mismatch.embedding_space.model = "other-model".to_owned();
    assert_eq!(
        index.compatibility(&mismatch),
        Ok(IndexCompatibilityStatus::ReindexRequired(
            IndexCompatibilityMismatch::EmbeddingSpace
        ))
    );
    mismatch = expected.clone();
    mismatch.dimensions = Some(4);
    assert_eq!(
        index.compatibility(&mismatch),
        Ok(IndexCompatibilityStatus::ReindexRequired(
            IndexCompatibilityMismatch::Dimensions
        ))
    );

    let first = replacement("src/revision.rs", 4, "alpha beta", &[(0, 5)]);
    assert_eq!(
        index.replace_path(first.clone()),
        Ok(RevisionOutcome::Applied)
    );
    let committed = index.snapshot().expect("snapshot committed replacement");

    assert_eq!(
        index.replace_path(first),
        Ok(RevisionOutcome::Unchanged),
        "an identical replay must not write"
    );
    assert_eq!(index.snapshot().unwrap(), committed);

    assert_eq!(
        index.replace_path(replacement("src/revision.rs", 3, "older", &[(0, 5)])),
        Err(SemanticIndexError::StaleRevision {
            current: PathRevision(4),
            attempted: PathRevision(3),
        })
    );
    assert_eq!(
        index.replace_path(replacement("src/revision.rs", 4, "different", &[(0, 9)])),
        Err(SemanticIndexError::StaleRevision {
            current: PathRevision(4),
            attempted: PathRevision(4),
        })
    );

    assert_eq!(
        index.delete_path(&path("src/revision.rs"), PathRevision(5)),
        Ok(RevisionOutcome::Applied)
    );
    assert!(snapshot_chunk_hashes(index, "src/revision.rs").is_empty());
    assert_eq!(
        index.delete_path(&path("src/revision.rs"), PathRevision(5)),
        Ok(RevisionOutcome::Unchanged)
    );
    assert_eq!(
        index.replace_path(replacement("src/revision.rs", 5, "resurrect", &[(0, 9)])),
        Err(SemanticIndexError::StaleRevision {
            current: PathRevision(5),
            attempted: PathRevision(5),
        })
    );
    assert_eq!(
        index.replace_path(replacement("src/revision.rs", 6, "current", &[(0, 7)])),
        Ok(RevisionOutcome::Applied)
    );
}

fn verify_persistence_failure_preserves_committed_state(index: &InMemorySemanticIndex) {
    assert_eq!(
        index.replace_path(replacement("src/stable.rs", 1, "stable", &[(0, 6)])),
        Ok(RevisionOutcome::Applied)
    );
    let committed = index.snapshot().expect("read committed state");

    index.fail_next_persistence();
    assert_eq!(
        index.replace_path(replacement("src/stable.rs", 2, "changed", &[(0, 7)])),
        Err(SemanticIndexError::Storage)
    );
    assert_eq!(
        index
            .snapshot()
            .expect("read state after failed persistence"),
        committed,
        "a failed persistence boundary must preserve the complete prior state"
    );

    index.fail_next_persistence();
    assert_eq!(
        index.publish_generation(RebuiltGeneration {
            id: GenerationId(42),
        }),
        Err(SemanticIndexError::Storage)
    );
    assert_eq!(index.snapshot().unwrap(), committed);
}

fn verify_atomic_path_contract(index: Arc<dyn SemanticIndexPort>) {
    let unaffected = replacement("src/other.rs", 1, "other", &[(0, 5)]);
    let initial = replacement("src/lib.rs", 1, "one two", &[(0, 3), (4, 7)]);
    assert_eq!(
        index.replace_path(unaffected.clone()),
        Ok(RevisionOutcome::Applied)
    );
    assert_eq!(index.replace_path(initial), Ok(RevisionOutcome::Applied));

    let other_hashes = snapshot_chunk_hashes(index.as_ref(), "src/other.rs");
    let updated = replacement("src/lib.rs", 2, "three", &[(0, 5)]);
    let replacement_hashes: Vec<_> = updated
        .indexed_chunks
        .iter()
        .map(|indexed| indexed.chunk.chunk_hash)
        .collect();
    assert_eq!(index.replace_path(updated), Ok(RevisionOutcome::Applied));
    assert_eq!(
        snapshot_chunk_hashes(index.as_ref(), "src/lib.rs"),
        replacement_hashes
    );
    assert_eq!(
        snapshot_chunk_hashes(index.as_ref(), "src/other.rs"),
        other_hashes
    );

    assert_eq!(
        index.delete_path(&path("src/lib.rs"), PathRevision(3)),
        Ok(RevisionOutcome::Applied)
    );
    assert!(snapshot_chunk_hashes(index.as_ref(), "src/lib.rs").is_empty());
    assert_eq!(
        snapshot_chunk_hashes(index.as_ref(), "src/other.rs"),
        other_hashes
    );
    assert_eq!(
        index.replace_path(replacement("src/lib.rs", 2, "stale", &[(0, 5)])),
        Err(SemanticIndexError::StaleRevision {
            current: PathRevision(3),
            attempted: PathRevision(2),
        })
    );

    assert_eq!(
        index.publish_generation(RebuiltGeneration {
            id: GenerationId(9)
        }),
        Ok(GenerationId(9))
    );
    let published = index.snapshot().expect("read published generation");
    assert_eq!(published.generation, GenerationId(9));
    assert_eq!(published.paths, vec![path("src/other.rs")]);

    let zeta = replacement("src/zeta.rs", 1, "zeta alpha", &[(5, 10), (0, 4)]);
    let alpha = replacement(
        "src/alpha.rs",
        1,
        "first second third",
        &[(13, 18), (0, 5), (6, 12)],
    );
    assert_eq!(
        index.replace_path(zeta.clone()),
        Ok(RevisionOutcome::Applied)
    );
    assert_eq!(
        index.replace_path(alpha.clone()),
        Ok(RevisionOutcome::Applied)
    );

    let expected_paths = vec![
        path("src/alpha.rs"),
        path("src/other.rs"),
        path("src/zeta.rs"),
    ];
    let expected_chunks = vec![
        &alpha.indexed_chunks[1].chunk,
        &alpha.indexed_chunks[2].chunk,
        &alpha.indexed_chunks[0].chunk,
        &unaffected.indexed_chunks[0].chunk,
        &zeta.indexed_chunks[1].chunk,
        &zeta.indexed_chunks[0].chunk,
    ]
    .into_iter()
    .map(|chunk| {
        (
            chunk.path.clone(),
            chunk.start_byte,
            chunk.end_byte,
            chunk.chunk_hash,
        )
    })
    .collect::<Vec<_>>();
    let deterministic = index.snapshot().expect("read ordered snapshot");
    assert_eq!(deterministic.paths, expected_paths);
    assert_eq!(snapshot_chunk_order(index.as_ref()), expected_chunks);
    assert_eq!(
        index.snapshot().expect("repeat ordered snapshot"),
        deterministic,
        "repeated snapshots must retain their exact path and chunk ordering"
    );

    let first = replacement("src/coherent.rs", 10, "aa bb", &[(0, 2), (3, 5)]);
    assert_eq!(index.replace_path(first), Ok(RevisionOutcome::Applied));
    let before_publication = index.snapshot().expect("read pre-publication snapshot");
    let mut after_publication = before_publication.clone();
    after_publication.generation = GenerationId(10);

    let start = Arc::new(Barrier::new(2));
    let finished = Arc::new(AtomicBool::new(false));
    let writer_index = Arc::clone(&index);
    let writer_start = Arc::clone(&start);
    let writer_finished = Arc::clone(&finished);
    let writer = std::thread::spawn(move || {
        writer_start.wait();
        assert_eq!(
            writer_index.publish_generation(RebuiltGeneration {
                id: GenerationId(10),
            }),
            Ok(GenerationId(10))
        );
        writer_finished.store(true, Ordering::Release);
    });

    start.wait();
    loop {
        let observed = index.snapshot().expect("read concurrent semantic snapshot");
        assert!(
            observed == before_publication || observed == after_publication,
            "a reader observed a mixed generation snapshot: {observed:?}"
        );
        if finished.load(Ordering::Acquire) {
            break;
        }
    }
    writer.join().expect("coherent-generation writer panicked");
    assert_eq!(
        index.snapshot().expect("read published snapshot"),
        after_publication
    );
}

#[test]
fn in_memory_index_replaces_and_deletes_paths_atomically() {
    verify_atomic_path_contract(Arc::new(InMemorySemanticIndex::new()));
}

#[test]
fn in_memory_index_satisfies_shared_contract() {
    let index = InMemorySemanticIndex::new();
    verify_revision_and_compatibility_contract(&index);
    verify_persistence_failure_preserves_committed_state(&index);
}

fn sqlite_index() -> (TempDir, SqliteSemanticIndex) {
    let directory = tempfile::tempdir().expect("create SQLite contract directory");
    let index = SqliteSemanticIndex::open(directory.path().join("semantic.db"), compatibility())
        .expect("open SQLite semantic index");
    (directory, index)
}

#[test]
fn sqlite_index_replaces_and_deletes_paths_atomically() {
    let (_directory, index) = sqlite_index();
    verify_atomic_path_contract(Arc::new(index));
}

#[test]
fn sqlite_index_satisfies_shared_contract() {
    let (_directory, index) = sqlite_index();
    verify_revision_and_compatibility_contract(&index);

    assert_eq!(
        index.replace_path(replacement("src/stable.rs", 1, "stable", &[(0, 6)])),
        Ok(RevisionOutcome::Applied)
    );
    let committed = index.snapshot().expect("read committed SQLite state");
    index.fail_next_persistence();
    assert_eq!(
        index.replace_path(replacement("src/stable.rs", 2, "changed", &[(0, 7)])),
        Err(SemanticIndexError::Storage)
    );
    assert_eq!(
        index
            .snapshot()
            .expect("read SQLite state after failed commit"),
        committed
    );
}

#[test]
fn sqlite_index_reopens_compatible_committed_state() {
    let directory = tempfile::tempdir().expect("create SQLite reopen directory");
    let database = directory.path().join("semantic.db");
    let committed = {
        let index =
            SqliteSemanticIndex::open(&database, compatibility()).expect("create SQLite index");
        let replacement = replacement(
            "src/persisted.rs",
            7,
            "persisted chunks",
            &[(10, 16), (0, 9)],
        );
        let expected_indexed_chunks = replacement.indexed_chunks.clone();
        assert_eq!(
            index.replace_path(replacement),
            Ok(RevisionOutcome::Applied)
        );
        assert_eq!(
            index.publish_generation(RebuiltGeneration {
                id: GenerationId(23),
            }),
            Ok(GenerationId(23))
        );
        let committed = index.snapshot().expect("read committed SQLite snapshot");
        let mut expected_indexed_chunks = expected_indexed_chunks;
        expected_indexed_chunks.sort_by(|left, right| {
            left.chunk
                .start_byte
                .cmp(&right.chunk.start_byte)
                .then(left.chunk.end_byte.cmp(&right.chunk.end_byte))
                .then(left.chunk.chunk_hash.cmp(&right.chunk.chunk_hash))
        });
        assert_eq!(committed.chunks, expected_indexed_chunks);
        committed
    };

    let reopened =
        SqliteSemanticIndex::open(&database, compatibility()).expect("reopen compatible index");
    assert_eq!(reopened.snapshot(), Ok(committed));

    let mut mismatch = compatibility();
    mismatch.embedding_space.provider = "replacement-provider".to_owned();
    assert_eq!(
        reopened.compatibility(&mismatch),
        Ok(IndexCompatibilityStatus::ReindexRequired(
            IndexCompatibilityMismatch::EmbeddingSpace,
        ))
    );
}

#[test]
fn sqlite_index_classifies_mismatch_before_reading_committed_data() {
    let directory = tempfile::tempdir().expect("create SQLite mismatch directory");
    let database = directory.path().join("semantic.db");
    {
        let index =
            SqliteSemanticIndex::open(&database, compatibility()).expect("create SQLite index");
        assert_eq!(
            index.replace_path(replacement("src/private.rs", 1, "private", &[(0, 7)])),
            Ok(RevisionOutcome::Applied)
        );
    }

    let mut requested = compatibility();
    requested.embedding_space.model = "replacement-model".to_owned();
    let reopened = SqliteSemanticIndex::open(&database, requested.clone())
        .expect("inspect incompatible index");
    assert_eq!(
        reopened.compatibility(&requested),
        Ok(IndexCompatibilityStatus::ReindexRequired(
            IndexCompatibilityMismatch::EmbeddingSpace,
        ))
    );
}

#[test]
fn corrupt_sqlite_index_is_preserved_for_recovery() {
    let directory = tempfile::tempdir().expect("create corrupt SQLite directory");
    let database = directory.path().join("semantic.db");
    let corrupt_bytes = b"not a SQLite database\0private recovery evidence";
    std::fs::write(&database, corrupt_bytes).expect("write corrupt SQLite fixture");

    assert!(matches!(
        SqliteSemanticIndex::open(&database, compatibility()),
        Err(SemanticIndexError::Corrupt)
    ));
    assert_eq!(
        std::fs::read(&database).expect("read preserved corrupt store"),
        corrupt_bytes
    );
}

#[test]
fn valid_sqlite_with_corrupt_metadata_is_preserved_before_writable_open() {
    let directory = tempfile::tempdir().expect("create corrupt SQLite directory");
    let database = directory.path().join("semantic.db");
    {
        let connection = rusqlite::Connection::open(&database).expect("create SQLite fixture");
        connection
            .execute_batch(
                "PRAGMA journal_mode=DELETE; PRAGMA user_version=1; CREATE TABLE recovery_evidence(value TEXT NOT NULL); INSERT INTO recovery_evidence VALUES ('preserve me');",
            )
            .expect("write corrupt metadata fixture");
    }
    let before = std::fs::read(&database).expect("read corrupt store before open");

    assert!(matches!(
        SqliteSemanticIndex::open(&database, compatibility()),
        Err(SemanticIndexError::Corrupt)
    ));
    assert_eq!(
        std::fs::read(&database).expect("read corrupt store after rejected open"),
        before
    );
    assert!(!database.with_extension("db-wal").exists());
    assert!(!database.with_extension("db-shm").exists());
}

#[test]
fn sqlite_runtime_read_failure_is_storage_and_preserves_state() {
    let (_directory, index) = sqlite_index();
    assert_eq!(
        index.replace_path(replacement("src/stable.rs", 1, "stable", &[(0, 6)])),
        Ok(RevisionOutcome::Applied)
    );
    let committed = index.snapshot().expect("read committed state");

    index.fail_next_read();
    assert_eq!(index.snapshot(), Err(SemanticIndexError::Storage));
    assert_eq!(index.snapshot(), Ok(committed));
}

#[test]
fn unsupported_sqlite_schema_is_preserved_for_recovery() {
    let directory = tempfile::tempdir().expect("create unsupported SQLite directory");
    let database = directory.path().join("semantic.db");
    {
        let connection = rusqlite::Connection::open(&database).expect("create SQLite fixture");
        connection
            .execute_batch(
                "PRAGMA user_version=99; CREATE TABLE recovery_evidence(value TEXT NOT NULL); INSERT INTO recovery_evidence VALUES ('preserve me');",
            )
            .expect("write unsupported schema fixture");
    }
    let before = std::fs::read(&database).expect("read unsupported store before open");

    assert!(matches!(
        SqliteSemanticIndex::open(&database, compatibility()),
        Err(SemanticIndexError::UnsupportedSchema)
    ));
    assert_eq!(
        std::fs::read(&database).expect("read preserved unsupported store"),
        before
    );
    let connection = rusqlite::Connection::open(&database).expect("reopen unsupported fixture");
    assert_eq!(
        connection
            .query_row("SELECT value FROM recovery_evidence", [], |row| row
                .get::<_, String>(0))
            .expect("read recovery evidence"),
        "preserve me"
    );
}

#[test]
fn rebuilt_generation_reconstructs_disposable_index() {
    let (_directory, index) = sqlite_index();
    assert_eq!(
        index.replace_path(replacement("src/removed.rs", 1, "removed", &[(0, 7)])),
        Ok(RevisionOutcome::Applied)
    );
    assert_eq!(
        index.replace_path(replacement("src/current.rs", 1, "old", &[(0, 3)])),
        Ok(RevisionOutcome::Applied)
    );
    let active = index
        .snapshot()
        .expect("read active generation before rebuild");
    index
        .begin_generation(RebuiltGeneration {
            id: GenerationId(2),
        })
        .expect("start shadow generation");

    assert_eq!(
        index.replace_path(replacement(
            "src/current.rs",
            2,
            "current workspace",
            &[(0, 7), (8, 17)],
        )),
        Ok(RevisionOutcome::Applied)
    );
    assert_eq!(
        index.replace_path(replacement("src/added.rs", 1, "added", &[(0, 5)])),
        Ok(RevisionOutcome::Applied)
    );
    assert_eq!(
        index.snapshot(),
        Ok(active),
        "partial shadow-generation mutations must remain invisible before publication"
    );
    assert_eq!(
        index.publish_generation(RebuiltGeneration {
            id: GenerationId(2),
        }),
        Ok(GenerationId(2))
    );

    let rebuilt = index.snapshot().expect("read rebuilt SQLite generation");
    assert_eq!(rebuilt.generation, GenerationId(2));
    assert_eq!(
        rebuilt.paths,
        vec![path("src/added.rs"), path("src/current.rs")]
    );
    assert_eq!(
        snapshot_chunk_order(&index),
        vec![
            (
                path("src/added.rs"),
                0,
                5,
                chunk("src/added.rs", "added", 0, 5).chunk_hash,
            ),
            (
                path("src/current.rs"),
                0,
                7,
                chunk("src/current.rs", "current workspace", 0, 7).chunk_hash,
            ),
            (
                path("src/current.rs"),
                8,
                17,
                chunk("src/current.rs", "current workspace", 8, 17).chunk_hash,
            ),
        ]
    );
}

#[test]
fn aborted_sqlite_generation_preserves_snapshot_and_live_mutation_target() {
    let directory = tempfile::tempdir().unwrap();
    let index =
        SqliteSemanticIndex::open(directory.path().join("index.sqlite3"), compatibility()).unwrap();
    index
        .replace_path(replacement("src/current.rs", 1, "old", &[(0, 3)]))
        .unwrap();
    let active = index.snapshot().unwrap();
    index
        .begin_generation(RebuiltGeneration {
            id: GenerationId(2),
        })
        .unwrap();
    index
        .replace_path(replacement("src/shadow.rs", 1, "shadow", &[(0, 6)]))
        .unwrap();
    index.fail_next_persistence();
    assert_eq!(
        index.publish_generation(RebuiltGeneration {
            id: GenerationId(2)
        }),
        Err(SemanticIndexError::Storage)
    );
    index.abort_generation().unwrap();
    assert_eq!(index.snapshot().unwrap(), active);
    index
        .replace_path(replacement("src/current.rs", 2, "new", &[(0, 3)]))
        .unwrap();
    let snapshot = index.snapshot().unwrap();
    assert_eq!(snapshot.paths, vec![path("src/current.rs")]);
    assert_eq!(snapshot.chunks[0].chunk.snippet.as_str(), "new");
}

#[test]
fn failed_generation_abort_remains_recoverable_before_live_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let index =
        SqliteSemanticIndex::open(directory.path().join("index.sqlite3"), compatibility()).unwrap();
    index
        .replace_path(replacement("src/current.rs", 1, "old", &[(0, 3)]))
        .unwrap();
    let active = index.snapshot().unwrap();
    index
        .begin_generation(RebuiltGeneration {
            id: GenerationId(2),
        })
        .unwrap();
    index.fail_next_persistence_attempts(2);
    assert_eq!(
        index.publish_generation(RebuiltGeneration {
            id: GenerationId(2),
        }),
        Err(SemanticIndexError::Storage)
    );
    assert_eq!(index.abort_generation(), Err(SemanticIndexError::Storage));
    assert_eq!(index.snapshot().unwrap(), active);

    index.abort_generation().unwrap();
    index
        .replace_path(replacement("src/current.rs", 2, "new", &[(0, 3)]))
        .unwrap();
    assert_eq!(
        index.snapshot().unwrap().chunks[0].chunk.snippet.as_str(),
        "new"
    );
}
