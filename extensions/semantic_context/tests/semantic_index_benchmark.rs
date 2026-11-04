use std::{env, fs, path::Path, process::Command, time::Instant};

use semantic_context_extension::{
    ChunkingFingerprint, EmbeddingSpaceIdentity, EmbeddingVector, GenerationId, IndexCompatibility,
    IndexedChunk, PathReplacement, PathRevision, RebuiltGeneration, RevisionOutcome, SemanticChunk,
    SemanticChunkInput, SemanticIndexPort, WorkspacePath, adapters::sqlite::SqliteSemanticIndex,
};

const FIXTURE_SEED: u64 = 0x5EED_2026;
const VECTOR_DIMENSIONS: usize = 3;
const CHUNK_COUNT: usize = 64;

fn real_disk_root() -> (std::path::PathBuf, String, String) {
    let root = env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .expect("set TMPDIR to an operator-selected real-disk directory");
    let absolute = fs::canonicalize(&root).expect("TMPDIR must exist and be canonicalizable");
    assert!(
        !absolute.starts_with("/tmp"),
        "TMPDIR must not be under /tmp"
    );

    let filesystem = command_output("stat", &["-f", "-c", "%T", path_text(&absolute)]);
    assert!(
        !filesystem.is_empty(),
        "TMPDIR filesystem provenance must be available"
    );
    assert!(
        !matches!(filesystem.as_str(), "tmpfs" | "ramfs"),
        "TMPDIR must not use a memory-backed filesystem"
    );
    let device = command_output("df", &["--output=source", path_text(&absolute)]);
    let device = device
        .lines()
        .nth(1)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .expect("TMPDIR device provenance must be available")
        .to_owned();
    (absolute, filesystem, device)
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("real-disk path must be UTF-8")
}

fn command_output(program: &str, arguments: &[&str]) -> String {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .expect("run provenance command");
    assert!(
        output.status.success(),
        "{program} failed during real-disk preflight"
    );
    String::from_utf8(output.stdout)
        .expect("provenance output must be UTF-8")
        .trim()
        .to_owned()
}

fn embedding_bytes(index: &impl SemanticIndexPort) -> Vec<u8> {
    index
        .snapshot()
        .expect("read durability snapshot")
        .chunks
        .into_iter()
        .flat_map(|chunk| chunk.embedding.values)
        .flat_map(f32::to_le_bytes)
        .collect()
}

fn compatibility() -> IndexCompatibility {
    IndexCompatibility {
        schema_version: 1,
        chunking_fingerprint: ChunkingFingerprint {
            algorithm_version: "v1".to_owned(),
            target_bytes: 1_024,
            overlap_bytes: 128,
            max_file_bytes: 1_048_576,
        },
        embedding_space: EmbeddingSpaceIdentity {
            provider: "durability-fixture".to_owned(),
            model: "fixed-seed".to_owned(),
            dimensions: Some(VECTOR_DIMENSIONS),
        },
        dimensions: Some(VECTOR_DIMENSIONS),
    }
}

#[test]
#[ignore = "requires operator-selected verified real-disk TMPDIR"]
fn semantic_index_durability_on_real_disk() {
    let (root, filesystem, device) = real_disk_root();
    let directory = tempfile::Builder::new()
        .prefix("tower-semantic-durability-")
        .tempdir_in(&root)
        .expect("create real-disk durability directory");
    let database = directory.path().join("semantic.db");
    let source = "x".repeat(CHUNK_COUNT);
    let chunks: Vec<_> = (0..CHUNK_COUNT)
        .map(|offset| {
            SemanticChunk::new(SemanticChunkInput {
                path: WorkspacePath::new("src/generated.rs".to_owned()).unwrap(),
                source: &source,
                start_byte: offset,
                end_byte: offset + 1,
                language: "rust".to_owned(),
                symbol: None,
                chunking_fingerprint: compatibility().chunking_fingerprint,
                expected_chunk_hash: None,
            })
            .expect("fixed generated chunk")
        })
        .collect();

    let started = Instant::now();
    let (committed, committed_vectors) = {
        let index =
            SqliteSemanticIndex::open(&database, compatibility()).expect("open durability index");
        assert_eq!(
            index.replace_path(PathReplacement {
                path: WorkspacePath::new("src/generated.rs".to_owned()).unwrap(),
                revision: PathRevision(FIXTURE_SEED),
                indexed_chunks: chunks
                    .into_iter()
                    .enumerate()
                    .map(|(index, chunk)| IndexedChunk {
                        chunk,
                        embedding: EmbeddingVector::new(
                            (0..VECTOR_DIMENSIONS)
                                .map(|dimension| (index * VECTOR_DIMENSIONS + dimension + 1) as f32)
                                .collect(),
                            Some(VECTOR_DIMENSIONS),
                        )
                        .expect("valid fixed durability embedding"),
                    })
                    .collect(),
            }),
            Ok(RevisionOutcome::Applied)
        );
        assert_eq!(
            index.publish_generation(RebuiltGeneration {
                id: GenerationId(FIXTURE_SEED)
            }),
            Ok(GenerationId(FIXTURE_SEED))
        );
        (
            index.snapshot().expect("snapshot committed fixture"),
            embedding_bytes(&index),
        )
    };
    let reopened =
        SqliteSemanticIndex::open(&database, compatibility()).expect("reopen durability index");
    let reopened_snapshot = reopened.snapshot().expect("snapshot reopened fixture");
    assert_eq!(reopened_snapshot, committed);
    assert_eq!(embedding_bytes(&reopened), committed_vectors);

    let database_text = path_text(&database);
    let sqlite_settings = command_output(
        "sqlite3",
        &[database_text, "PRAGMA journal_mode; PRAGMA synchronous;"],
    );
    let mut settings = sqlite_settings.lines();
    assert_eq!(settings.next(), Some("wal"));
    assert_eq!(settings.next(), Some("2"));
    let git_commit = command_output("git", &["rev-parse", "HEAD"]);
    let rust_version = command_output("rustc", &["--version"]);
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    println!(
        "git_commit={git_commit} profile={profile} rust_version={rust_version:?} fixture_seed={FIXTURE_SEED} chunk_count={} vector_dimensions={VECTOR_DIMENSIONS} sqlite_journal_mode=wal sqlite_synchronous=full tested_directory={} filesystem_type={filesystem} device_identifier={device} elapsed_ms={} committed_row_count={} reopen_verified=true result=pass",
        committed.chunks.len(),
        directory.path().display(),
        started.elapsed().as_millis(),
        reopened_snapshot.chunks.len(),
    );
}
