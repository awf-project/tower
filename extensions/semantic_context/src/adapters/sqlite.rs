use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::{
    BoundedSnippet, ChunkingFingerprint, EmbeddingVector, GenerationId, IndexCompatibility,
    IndexCompatibilityMismatch, IndexCompatibilityStatus, IndexedChunk, PathReplacement,
    PathRevision, RebuiltGeneration, RevisionOutcome, SemanticChunk, SemanticIndexError,
    SemanticIndexPort, SemanticSnapshot, SymbolSpan, WorkspacePath,
};

const STORAGE_SCHEMA_VERSION: i64 = 1;

pub struct SqliteSemanticIndex {
    connection: Mutex<Connection>,
    stored: IndexCompatibility,
    open_status: IndexCompatibilityStatus,
    fail_next_commits: Mutex<usize>,
    fail_next_read: Mutex<bool>,
}

impl SqliteSemanticIndex {
    pub fn open(
        database_path: impl AsRef<Path>,
        compatibility: IndexCompatibility,
    ) -> Result<Self, SemanticIndexError> {
        let path = database_path.as_ref();
        let existed = path
            .metadata()
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false);
        let validated = if existed {
            let validation = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(open_error)?;
            let version: i64 = validation
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(open_error)?;
            if version != STORAGE_SCHEMA_VERSION {
                return Err(SemanticIndexError::UnsupportedSchema);
            }
            let stored = read_compatibility(&validation).map_err(read_error)?;
            let status = compare(&compatibility, &stored);
            if status == IndexCompatibilityStatus::Compatible {
                read_snapshot(&validation)?;
            }
            Some((stored, status))
        } else {
            None
        };
        let mut connection = Connection::open(path).map_err(open_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(open_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(open_error)?;
        if !existed {
            initialize(&mut connection, &compatibility)?;
        }
        let (stored, open_status) = if let Some(validated) = validated {
            validated
        } else {
            let stored = read_compatibility(&connection).map_err(read_error)?;
            let status = compare(&compatibility, &stored);
            (stored, status)
        };
        Ok(Self {
            connection: Mutex::new(connection),
            stored,
            open_status,
            fail_next_commits: Mutex::new(0),
            fail_next_read: Mutex::new(false),
        })
    }

    /// Deterministically fails the next state-changing commit for contract testing.
    pub fn fail_next_persistence(&self) {
        self.fail_next_persistence_attempts(1);
    }

    /// Deterministically fails the requested number of state-changing commits.
    pub fn fail_next_persistence_attempts(&self, attempts: usize) {
        *self
            .fail_next_commits
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = attempts;
    }

    /// Starts an isolated shadow generation. Path mutations remain invisible until publication.
    pub fn begin_generation(
        &self,
        generation: RebuiltGeneration,
    ) -> Result<(), SemanticIndexError> {
        self.ensure_compatible()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|_| SemanticIndexError::Storage)?;
        transaction
            .execute(
                "DELETE FROM chunks WHERE generation=?1",
                [generation.id.0.to_string()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        transaction
            .execute(
                "DELETE FROM paths WHERE generation=?1",
                [generation.id.0.to_string()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        transaction
            .execute(
                "UPDATE state SET working_generation=?1 WHERE singleton=1",
                [generation.id.0.to_string()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        self.commit(transaction)
    }

    /// Abandon an unpublished rebuild so subsequent live edits target the
    /// active generation. The old published snapshot is never modified.
    pub fn abort_generation(&self) -> Result<(), SemanticIndexError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|_| SemanticIndexError::Storage)?;
        transaction.execute_batch("DELETE FROM chunks WHERE generation<>(SELECT active_generation FROM state WHERE singleton=1); DELETE FROM paths WHERE generation<>(SELECT active_generation FROM state WHERE singleton=1); UPDATE state SET working_generation=NULL WHERE singleton=1;").map_err(|_| SemanticIndexError::Storage)?;
        self.commit(transaction)
    }

    /// Deterministically exercises the runtime SQLite read-error classification boundary.
    pub fn fail_next_read(&self) {
        *self
            .fail_next_read
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, SemanticIndexError> {
        self.connection
            .lock()
            .map_err(|_| SemanticIndexError::Storage)
    }

    fn ensure_compatible(&self) -> Result<(), SemanticIndexError> {
        match &self.open_status {
            IndexCompatibilityStatus::Compatible => Ok(()),
            IndexCompatibilityStatus::ReindexRequired(mismatch) => {
                Err(SemanticIndexError::Incompatible(mismatch.clone()))
            }
        }
    }

    fn commit(&self, transaction: Transaction<'_>) -> Result<(), SemanticIndexError> {
        let mut fail = self
            .fail_next_commits
            .lock()
            .map_err(|_| SemanticIndexError::Storage)?;
        if *fail > 0 {
            *fail -= 1;
            return Err(SemanticIndexError::Storage);
        }
        transaction
            .commit()
            .map_err(|_| SemanticIndexError::Storage)
    }
}

impl SemanticIndexPort for SqliteSemanticIndex {
    fn compatibility(
        &self,
        expected: &IndexCompatibility,
    ) -> Result<IndexCompatibilityStatus, SemanticIndexError> {
        Ok(compare(expected, &self.stored))
    }

    fn snapshot(&self) -> Result<SemanticSnapshot, SemanticIndexError> {
        self.ensure_compatible()?;
        if std::mem::take(
            &mut *self
                .fail_next_read
                .lock()
                .map_err(|_| SemanticIndexError::Storage)?,
        ) {
            return Err(SemanticIndexError::Storage);
        }
        let connection = self.connection()?;
        read_snapshot(&connection)
    }

    fn replace_path(
        &self,
        replacement: PathReplacement,
    ) -> Result<RevisionOutcome, SemanticIndexError> {
        self.ensure_compatible()?;
        if replacement
            .indexed_chunks
            .iter()
            .any(|indexed| indexed.chunk.path != replacement.path)
        {
            return Err(SemanticIndexError::Corrupt);
        }
        let prepared = prepare_chunks(&replacement.indexed_chunks, self.stored.dimensions)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|_| SemanticIndexError::Storage)?;
        let generation = mutation_generation(&transaction)?;
        if let Some((current, deleted)) = path_state(&transaction, generation, &replacement.path)? {
            if replacement.revision.0 < current || replacement.revision.0 == current && deleted {
                return Err(SemanticIndexError::StaleRevision {
                    current: PathRevision(current),
                    attempted: replacement.revision,
                });
            }
            if replacement.revision.0 == current {
                let existing = read_chunks(&transaction, generation, Some(&replacement.path))?;
                return if existing == replacement.indexed_chunks {
                    Ok(RevisionOutcome::Unchanged)
                } else {
                    Err(SemanticIndexError::StaleRevision {
                        current: PathRevision(current),
                        attempted: replacement.revision,
                    })
                };
            }
        }
        transaction.execute("INSERT INTO paths(generation,path,revision,deleted) VALUES (?1,?2,?3,0) ON CONFLICT(generation,path) DO UPDATE SET revision=excluded.revision, deleted=0", params![generation.to_string(), replacement.path.as_str(), replacement.revision.0.to_string()]).map_err(|_| SemanticIndexError::Storage)?;
        transaction
            .execute(
                "DELETE FROM chunks WHERE generation=?1 AND path=?2",
                params![generation.to_string(), replacement.path.as_str()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        for chunk in &prepared {
            insert_chunk(&transaction, generation, chunk)?;
        }
        self.commit(transaction)?;
        Ok(RevisionOutcome::Applied)
    }

    fn delete_path(
        &self,
        path: &WorkspacePath,
        revision: PathRevision,
    ) -> Result<RevisionOutcome, SemanticIndexError> {
        self.ensure_compatible()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|_| SemanticIndexError::Storage)?;
        let generation = mutation_generation(&transaction)?;
        if let Some((current, deleted)) = path_state(&transaction, generation, path)? {
            if revision.0 < current || revision.0 == current && !deleted {
                return Err(SemanticIndexError::StaleRevision {
                    current: PathRevision(current),
                    attempted: revision,
                });
            }
            if revision.0 == current {
                return Ok(RevisionOutcome::Unchanged);
            }
        }
        transaction.execute("INSERT INTO paths(generation,path,revision,deleted) VALUES (?1,?2,?3,1) ON CONFLICT(generation,path) DO UPDATE SET revision=excluded.revision, deleted=1", params![generation.to_string(), path.as_str(), revision.0.to_string()]).map_err(|_| SemanticIndexError::Storage)?;
        transaction
            .execute(
                "DELETE FROM chunks WHERE generation=?1 AND path=?2",
                params![generation.to_string(), path.as_str()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        self.commit(transaction)?;
        Ok(RevisionOutcome::Applied)
    }

    fn publish_generation(
        &self,
        generation: RebuiltGeneration,
    ) -> Result<GenerationId, SemanticIndexError> {
        self.ensure_compatible()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|_| SemanticIndexError::Storage)?;
        let active = active_generation(&transaction)?;
        let working: Option<String> = transaction
            .query_row(
                "SELECT working_generation FROM state WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        if working.is_none() && active != generation.id.0 {
            transaction.execute("INSERT INTO paths(generation,path,revision,deleted) SELECT ?1,path,revision,deleted FROM paths WHERE generation=?2", params![generation.id.0.to_string(), active.to_string()]).map_err(|_| SemanticIndexError::Storage)?;
            transaction.execute("INSERT INTO chunks SELECT ?1,path,position,content_hash,start_byte,end_byte,language,snippet,symbol_start,symbol_end,algorithm_version,target_bytes,overlap_bytes,max_file_bytes,chunk_hash,embedding FROM chunks WHERE generation=?2", params![generation.id.0.to_string(), active.to_string()]).map_err(|_| SemanticIndexError::Storage)?;
        } else if working.as_deref() != Some(&generation.id.0.to_string()) {
            return Err(SemanticIndexError::Corrupt);
        }
        transaction
            .execute(
                "UPDATE state SET active_generation=?1, working_generation=NULL WHERE singleton=1",
                [generation.id.0.to_string()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        transaction
            .execute(
                "DELETE FROM chunks WHERE generation<>?1",
                [generation.id.0.to_string()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        transaction
            .execute(
                "DELETE FROM paths WHERE generation<>?1",
                [generation.id.0.to_string()],
            )
            .map_err(|_| SemanticIndexError::Storage)?;
        self.commit(transaction)?;
        Ok(generation.id)
    }
}

fn open_error(error: rusqlite::Error) -> SemanticIndexError {
    match error {
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::NotADatabase =>
        {
            SemanticIndexError::Corrupt
        }
        _ => SemanticIndexError::Storage,
    }
}

fn read_error(error: rusqlite::Error) -> SemanticIndexError {
    match error {
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::Unknown
            ) =>
        {
            SemanticIndexError::Corrupt
        }
        rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::InvalidColumnIndex(_)
        | rusqlite::Error::InvalidColumnName(_)
        | rusqlite::Error::InvalidQuery
        | rusqlite::Error::QueryReturnedNoRows => SemanticIndexError::Corrupt,
        _ => SemanticIndexError::Storage,
    }
}

fn initialize(
    connection: &mut Connection,
    compatibility: &IndexCompatibility,
) -> Result<(), SemanticIndexError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SemanticIndexError::Storage)?;
    transaction.execute_batch("CREATE TABLE metadata (singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL, algorithm_version TEXT NOT NULL, target_bytes TEXT NOT NULL, overlap_bytes TEXT NOT NULL, max_file_bytes TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL, embedding_dimensions TEXT, dimensions TEXT); CREATE TABLE state (singleton INTEGER PRIMARY KEY CHECK(singleton=1), active_generation TEXT NOT NULL, working_generation TEXT); CREATE TABLE paths (generation TEXT NOT NULL, path TEXT NOT NULL, revision TEXT NOT NULL, deleted INTEGER NOT NULL CHECK(deleted IN (0,1)), PRIMARY KEY(generation,path)); CREATE TABLE chunks (generation TEXT NOT NULL, path TEXT NOT NULL, position INTEGER NOT NULL, content_hash BLOB NOT NULL, start_byte TEXT NOT NULL, end_byte TEXT NOT NULL, language TEXT NOT NULL, snippet TEXT NOT NULL, symbol_start TEXT, symbol_end TEXT, algorithm_version TEXT NOT NULL, target_bytes TEXT NOT NULL, overlap_bytes TEXT NOT NULL, max_file_bytes TEXT NOT NULL, chunk_hash BLOB NOT NULL, embedding BLOB NOT NULL, PRIMARY KEY(generation,path,position), FOREIGN KEY(generation,path) REFERENCES paths(generation,path) ON DELETE CASCADE);").map_err(|_| SemanticIndexError::Storage)?;
    transaction
        .execute(
            "INSERT INTO metadata VALUES (1,?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                compatibility.schema_version,
                compatibility.chunking_fingerprint.algorithm_version,
                compatibility.chunking_fingerprint.target_bytes.to_string(),
                compatibility.chunking_fingerprint.overlap_bytes.to_string(),
                compatibility
                    .chunking_fingerprint
                    .max_file_bytes
                    .to_string(),
                compatibility.embedding_space.provider,
                compatibility.embedding_space.model,
                compatibility
                    .embedding_space
                    .dimensions
                    .map(|v| v.to_string()),
                compatibility.dimensions.map(|v| v.to_string())
            ],
        )
        .map_err(|_| SemanticIndexError::Storage)?;
    transaction
        .execute("INSERT INTO state VALUES (1, '0', NULL)", [])
        .map_err(|_| SemanticIndexError::Storage)?;
    transaction
        .pragma_update(None, "user_version", STORAGE_SCHEMA_VERSION)
        .map_err(|_| SemanticIndexError::Storage)?;
    transaction
        .commit()
        .map_err(|_| SemanticIndexError::Storage)
}

fn read_compatibility(connection: &Connection) -> rusqlite::Result<IndexCompatibility> {
    connection.query_row("SELECT schema_version,algorithm_version,target_bytes,overlap_bytes,max_file_bytes,provider,model,embedding_dimensions,dimensions FROM metadata WHERE singleton=1", [], |row| {
        let u64_value = |value: String| value.parse::<u64>().map_err(|_| rusqlite::Error::InvalidQuery);
        let usize_value = |value: String| value.parse::<usize>().map_err(|_| rusqlite::Error::InvalidQuery);
        let optional_usize = |value: Option<String>| value.map(usize_value).transpose();
        Ok(IndexCompatibility { schema_version: row.get(0)?, chunking_fingerprint: ChunkingFingerprint { algorithm_version: row.get(1)?, target_bytes: u64_value(row.get(2)?)?, overlap_bytes: u64_value(row.get(3)?)?, max_file_bytes: u64_value(row.get(4)?)? }, embedding_space: crate::EmbeddingSpaceIdentity { provider: row.get(5)?, model: row.get(6)?, dimensions: optional_usize(row.get(7)?)? }, dimensions: optional_usize(row.get(8)?)? })
    })
}

fn compare(expected: &IndexCompatibility, stored: &IndexCompatibility) -> IndexCompatibilityStatus {
    let mismatch = if expected.schema_version != stored.schema_version {
        Some(IndexCompatibilityMismatch::Schema)
    } else if expected.chunking_fingerprint != stored.chunking_fingerprint {
        Some(IndexCompatibilityMismatch::ChunkingFingerprint)
    } else if expected.embedding_space.provider != stored.embedding_space.provider
        || expected.embedding_space.model != stored.embedding_space.model
    {
        Some(IndexCompatibilityMismatch::EmbeddingSpace)
    } else if expected.embedding_space.dimensions != stored.embedding_space.dimensions
        || expected.dimensions != stored.dimensions
    {
        Some(IndexCompatibilityMismatch::Dimensions)
    } else {
        None
    };
    mismatch
        .map(IndexCompatibilityStatus::ReindexRequired)
        .unwrap_or(IndexCompatibilityStatus::Compatible)
}

fn path_state(
    connection: &Connection,
    generation: u64,
    path: &WorkspacePath,
) -> Result<Option<(u64, bool)>, SemanticIndexError> {
    let row: Option<(String, bool)> = connection
        .query_row(
            "SELECT revision, deleted FROM paths WHERE generation=?1 AND path=?2",
            params![generation.to_string(), path.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| SemanticIndexError::Storage)?;
    row.map(|(revision, deleted)| {
        revision
            .parse()
            .map(|value| (value, deleted))
            .map_err(|_| SemanticIndexError::Corrupt)
    })
    .transpose()
}

fn active_generation(connection: &Connection) -> Result<u64, SemanticIndexError> {
    let value: String = connection
        .query_row(
            "SELECT active_generation FROM state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(read_error)?;
    value.parse().map_err(|_| SemanticIndexError::Corrupt)
}

fn mutation_generation(connection: &Connection) -> Result<u64, SemanticIndexError> {
    let (active, working): (String, Option<String>) = connection
        .query_row(
            "SELECT active_generation,working_generation FROM state WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(read_error)?;
    working
        .unwrap_or(active)
        .parse()
        .map_err(|_| SemanticIndexError::Corrupt)
}

struct PreparedChunk<'a> {
    position: usize,
    chunk: &'a SemanticChunk,
    embedding: Vec<u8>,
}

fn prepare_chunks(
    indexed_chunks: &[IndexedChunk],
    dimensions: Option<usize>,
) -> Result<Vec<PreparedChunk<'_>>, SemanticIndexError> {
    indexed_chunks
        .iter()
        .enumerate()
        .map(|(position, indexed)| {
            if dimensions.is_some_and(|expected| indexed.embedding.values.len() != expected)
                || indexed.embedding.values.is_empty()
                || indexed
                    .embedding
                    .values
                    .iter()
                    .any(|value| !value.is_finite())
            {
                return Err(SemanticIndexError::Corrupt);
            }
            let embedding = indexed
                .embedding
                .values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            Ok(PreparedChunk {
                position,
                chunk: &indexed.chunk,
                embedding,
            })
        })
        .collect()
}

fn insert_chunk(
    transaction: &Transaction<'_>,
    generation: u64,
    prepared: &PreparedChunk<'_>,
) -> Result<(), SemanticIndexError> {
    let chunk = prepared.chunk;
    transaction
        .execute(
            "INSERT INTO chunks VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                generation.to_string(),
                chunk.path.as_str(),
                prepared.position as i64,
                chunk.content_hash.as_slice(),
                chunk.start_byte.to_string(),
                chunk.end_byte.to_string(),
                chunk.language,
                chunk.snippet.as_str(),
                chunk.symbol.map(|v| v.start_byte.to_string()),
                chunk.symbol.map(|v| v.end_byte.to_string()),
                chunk.chunking_fingerprint.algorithm_version,
                chunk.chunking_fingerprint.target_bytes.to_string(),
                chunk.chunking_fingerprint.overlap_bytes.to_string(),
                chunk.chunking_fingerprint.max_file_bytes.to_string(),
                chunk.chunk_hash.as_slice(),
                prepared.embedding
            ],
        )
        .map_err(|_| SemanticIndexError::Storage)?;
    Ok(())
}

fn read_snapshot(connection: &Connection) -> Result<SemanticSnapshot, SemanticIndexError> {
    let generation: String = connection
        .query_row(
            "SELECT active_generation FROM state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(read_error)?;
    let generation: u64 = generation
        .parse()
        .map_err(|_| SemanticIndexError::Corrupt)?;
    let mut statement = connection
        .prepare("SELECT path FROM paths WHERE generation=?1 AND deleted=0 ORDER BY path")
        .map_err(read_error)?;
    let rows = statement
        .query_map([generation.to_string()], |row| row.get::<_, String>(0))
        .map_err(read_error)?;
    let mut paths = Vec::new();
    for row in rows {
        paths.push(
            WorkspacePath::new(row.map_err(read_error)?)
                .map_err(|_| SemanticIndexError::Corrupt)?,
        );
    }
    Ok(SemanticSnapshot {
        generation: GenerationId(generation),
        paths,
        chunks: read_chunks(connection, generation, None)?,
    })
}

fn read_chunks(
    connection: &Connection,
    generation: u64,
    path: Option<&WorkspacePath>,
) -> Result<Vec<IndexedChunk>, SemanticIndexError> {
    let sql = "SELECT c.path,c.content_hash,c.start_byte,c.end_byte,c.language,c.snippet,c.symbol_start,c.symbol_end,c.algorithm_version,c.target_bytes,c.overlap_bytes,c.max_file_bytes,c.chunk_hash,c.embedding FROM chunks c JOIN paths p ON p.generation=c.generation AND p.path=c.path WHERE c.generation=?1 AND p.deleted=0 AND (?2 IS NULL OR c.path=?2) ORDER BY c.path,c.start_byte+0,c.end_byte+0,c.chunk_hash";
    let mut statement = connection.prepare(sql).map_err(read_error)?;
    let mut rows = statement
        .query(params![
            generation.to_string(),
            path.map(WorkspacePath::as_str)
        ])
        .map_err(read_error)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().map_err(read_error)? {
        let usize_value = |value: String| {
            value
                .parse::<usize>()
                .map_err(|_| SemanticIndexError::Corrupt)
        };
        let u64_value = |value: String| {
            value
                .parse::<u64>()
                .map_err(|_| SemanticIndexError::Corrupt)
        };
        let blob32 =
            |value: Vec<u8>| <[u8; 32]>::try_from(value).map_err(|_| SemanticIndexError::Corrupt);
        let symbol_start: Option<String> = row.get(6).map_err(|_| SemanticIndexError::Corrupt)?;
        let symbol_end: Option<String> = row.get(7).map_err(|_| SemanticIndexError::Corrupt)?;
        let symbol = match (symbol_start, symbol_end) {
            (Some(start), Some(end)) => Some(SymbolSpan {
                start_byte: usize_value(start)?,
                end_byte: usize_value(end)?,
            }),
            (None, None) => None,
            _ => return Err(SemanticIndexError::Corrupt),
        };
        let embedding: Vec<u8> = row.get(13).map_err(|_| SemanticIndexError::Corrupt)?;
        if embedding.is_empty() || !embedding.len().is_multiple_of(4) {
            return Err(SemanticIndexError::Corrupt);
        }
        let values = embedding
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("exact embedding width")))
            .collect();
        result.push(IndexedChunk {
            chunk: SemanticChunk {
                path: WorkspacePath::new(row.get(0).map_err(|_| SemanticIndexError::Corrupt)?)
                    .map_err(|_| SemanticIndexError::Corrupt)?,
                content_hash: blob32(row.get(1).map_err(|_| SemanticIndexError::Corrupt)?)?,
                start_byte: usize_value(row.get(2).map_err(|_| SemanticIndexError::Corrupt)?)?,
                end_byte: usize_value(row.get(3).map_err(|_| SemanticIndexError::Corrupt)?)?,
                language: row.get(4).map_err(|_| SemanticIndexError::Corrupt)?,
                snippet: BoundedSnippet::new(row.get(5).map_err(|_| SemanticIndexError::Corrupt)?)
                    .map_err(|_| SemanticIndexError::Corrupt)?,
                symbol,
                chunking_fingerprint: ChunkingFingerprint {
                    algorithm_version: row.get(8).map_err(|_| SemanticIndexError::Corrupt)?,
                    target_bytes: u64_value(row.get(9).map_err(|_| SemanticIndexError::Corrupt)?)?,
                    overlap_bytes: u64_value(
                        row.get(10).map_err(|_| SemanticIndexError::Corrupt)?,
                    )?,
                    max_file_bytes: u64_value(
                        row.get(11).map_err(|_| SemanticIndexError::Corrupt)?,
                    )?,
                },
                chunk_hash: blob32(row.get(12).map_err(|_| SemanticIndexError::Corrupt)?)?,
            },
            embedding: EmbeddingVector::new(values, None)
                .map_err(|_| SemanticIndexError::Corrupt)?,
        });
    }
    Ok(result)
}
