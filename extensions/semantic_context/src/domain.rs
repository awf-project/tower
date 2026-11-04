use std::time::Instant;

use sha2::{Digest, Sha256};

const MAX_SNIPPET_BYTES: usize = 4_096;
const CHUNK_HASH_TAG: &[u8] = b"tower-semantic-chunk-v1";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    pub fn new(value: String) -> Result<Self, SemanticDomainError> {
        if has_windows_drive_prefix(&value)
            || value.is_empty()
            || value.starts_with('/')
            || value.contains('\\')
            || value
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return Err(SemanticDomainError::InvalidWorkspacePath);
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn has_windows_drive_prefix(value: &str) -> bool {
    matches!(value.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SymbolSpan {
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkingFingerprint {
    pub algorithm_version: String,
    pub target_bytes: u64,
    pub overlap_bytes: u64,
    pub max_file_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct SemanticChunkInput<'a> {
    pub path: WorkspacePath,
    pub source: &'a str,
    pub start_byte: usize,
    pub end_byte: usize,
    pub language: String,
    pub symbol: Option<SymbolSpan>,
    pub chunking_fingerprint: ChunkingFingerprint,
    pub expected_chunk_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SemanticChunk {
    pub path: WorkspacePath,
    pub content_hash: [u8; 32],
    pub start_byte: usize,
    pub end_byte: usize,
    pub language: String,
    pub snippet: BoundedSnippet,
    pub symbol: Option<SymbolSpan>,
    pub chunking_fingerprint: ChunkingFingerprint,
    pub chunk_hash: [u8; 32],
}

/// A snippet whose independently constructed form is constrained to the
/// semantic-context maximum.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BoundedSnippet(String);

impl BoundedSnippet {
    pub fn new(value: String) -> Result<Self, SemanticDomainError> {
        if value.len() > MAX_SNIPPET_BYTES {
            return Err(SemanticDomainError::SnippetTooLarge);
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SemanticChunk {
    pub fn new(input: SemanticChunkInput<'_>) -> Result<Self, SemanticDomainError> {
        validate_span(input.source, input.start_byte, input.end_byte)?;
        if let Some(symbol) = input.symbol {
            validate_span(input.source, symbol.start_byte, symbol.end_byte)?;
            if symbol.start_byte > input.start_byte || symbol.end_byte < input.end_byte {
                return Err(SemanticDomainError::InvalidUtf8Span);
            }
        }

        let content_hash: [u8; 32] = Sha256::digest(input.source.as_bytes()).into();
        let chunk_hash = chunk_hash(
            &input.path,
            content_hash,
            input.start_byte,
            input.end_byte,
            &input.chunking_fingerprint,
        );
        if input
            .expected_chunk_hash
            .is_some_and(|expected| expected != chunk_hash)
        {
            return Err(SemanticDomainError::InvalidChunkHash);
        }

        let selected = &input.source[input.start_byte..input.end_byte];
        let mut snippet_end = selected.len().min(MAX_SNIPPET_BYTES);
        while !selected.is_char_boundary(snippet_end) {
            snippet_end -= 1;
        }
        let snippet = BoundedSnippet::new(selected[..snippet_end].to_owned())?;

        Ok(Self {
            path: input.path,
            content_hash,
            start_byte: input.start_byte,
            end_byte: input.end_byte,
            language: input.language,
            snippet,
            symbol: input.symbol,
            chunking_fingerprint: input.chunking_fingerprint,
            chunk_hash,
        })
    }
}

fn validate_span(source: &str, start: usize, end: usize) -> Result<(), SemanticDomainError> {
    if start > end
        || end > source.len()
        || !source.is_char_boundary(start)
        || !source.is_char_boundary(end)
    {
        return Err(SemanticDomainError::InvalidUtf8Span);
    }
    Ok(())
}

fn chunk_hash(
    path: &WorkspacePath,
    content_hash: [u8; 32],
    start_byte: usize,
    end_byte: usize,
    fingerprint: &ChunkingFingerprint,
) -> [u8; 32] {
    let mut fingerprint_bytes = Vec::new();
    append_length_prefixed(
        &mut fingerprint_bytes,
        fingerprint.algorithm_version.as_bytes(),
    );
    fingerprint_bytes.extend_from_slice(&fingerprint.target_bytes.to_le_bytes());
    fingerprint_bytes.extend_from_slice(&fingerprint.overlap_bytes.to_le_bytes());
    fingerprint_bytes.extend_from_slice(&fingerprint.max_file_bytes.to_le_bytes());

    let mut hasher = Sha256::new();
    update_length_prefixed(&mut hasher, CHUNK_HASH_TAG);
    update_length_prefixed(&mut hasher, path.as_str().as_bytes());
    update_length_prefixed(&mut hasher, &content_hash);
    update_length_prefixed(&mut hasher, &(start_byte as u64).to_le_bytes());
    update_length_prefixed(&mut hasher, &(end_byte as u64).to_le_bytes());
    update_length_prefixed(&mut hasher, &fingerprint_bytes);
    hasher.finalize().into()
}

fn append_length_prefixed(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_le_bytes());
    output.extend_from_slice(value);
}

fn update_length_prefixed(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddingVector {
    pub values: Vec<f32>,
}

impl EmbeddingVector {
    pub fn new(
        values: Vec<f32>,
        expected_dimensions: Option<usize>,
    ) -> Result<Self, SemanticDomainError> {
        if values.is_empty() {
            return Err(SemanticDomainError::EmptyEmbedding);
        }
        if values.iter().any(|value| !value.is_finite()) {
            return Err(SemanticDomainError::NonFiniteEmbedding);
        }
        if values.iter().all(|value| *value == 0.0) {
            return Err(SemanticDomainError::ZeroNormEmbedding);
        }
        if let Some(expected) = expected_dimensions
            && values.len() != expected
        {
            return Err(SemanticDomainError::DimensionMismatch {
                expected,
                actual: values.len(),
            });
        }

        Ok(Self { values })
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EmbeddingSpaceIdentity {
    pub provider: String,
    pub model: String,
    pub dimensions: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexedEmbedding {
    pub index: usize,
    pub vector: EmbeddingVector,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SemanticOperation {
    Search,
    RelatedFiles,
    ContextForTask,
    Reindex,
    CatchUp,
    FileUpdate,
    Embedding,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DeadlineLayer {
    Semantic,
    Provider,
}

#[derive(Clone, Debug)]
pub struct OperationDeadline {
    deadline: Option<Instant>,
    operation: SemanticOperation,
    layer: DeadlineLayer,
    budget_ms: Option<u64>,
}

impl OperationDeadline {
    pub fn for_operation(
        deadline: Option<Instant>,
        operation: SemanticOperation,
        budget_ms: impl Into<Option<u64>>,
    ) -> Self {
        Self {
            deadline,
            operation,
            layer: DeadlineLayer::Semantic,
            budget_ms: budget_ms.into(),
        }
    }

    pub fn for_provider(deadline: Instant, operation: SemanticOperation, budget_ms: u64) -> Self {
        Self {
            deadline: Some(deadline),
            operation,
            layer: DeadlineLayer::Provider,
            budget_ms: Some(budget_ms),
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn operation(&self) -> &SemanticOperation {
        &self.operation
    }

    pub fn layer(&self) -> &DeadlineLayer {
        &self.layer
    }

    pub fn budget_ms(&self) -> u64 {
        self.budget_ms
            .expect("only finite deadlines carry timeout budget metadata")
    }

    pub fn configured_budget_ms(&self) -> Option<u64> {
        self.budget_ms
    }

    /// Chooses the deadline that expires first; the operation owns an equal
    /// deadline so outer-operation expiry retains its classification.
    pub fn select_earlier(operation: Self, provider: Self) -> Self {
        match (operation.deadline, provider.deadline) {
            (None, None) | (Some(_), None) => operation,
            (None, Some(_)) => provider,
            (Some(operation_deadline), Some(provider_deadline)) => {
                if operation_deadline <= provider_deadline {
                    operation
                } else {
                    provider
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EmbeddingRequest {
    pub inputs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PathRevision(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GenerationId(pub u64);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceDocument {
    pub path: WorkspacePath,
    pub source: String,
    pub content_hash: [u8; 32],
    pub language: String,
    /// Reliable structural spans supplied by the workspace reader.  Chunking
    /// prefers these boundaries over heuristic line and UTF-8 splitting.
    pub symbol_spans: Vec<SymbolSpan>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathReplacement {
    pub path: WorkspacePath,
    pub revision: PathRevision,
    pub indexed_chunks: Vec<IndexedChunk>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RebuiltGeneration {
    pub id: GenerationId,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IndexCompatibility {
    pub schema_version: u32,
    pub chunking_fingerprint: ChunkingFingerprint,
    pub embedding_space: EmbeddingSpaceIdentity,
    pub dimensions: Option<usize>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum IndexCompatibilityMismatch {
    Schema,
    ChunkingFingerprint,
    EmbeddingSpace,
    Dimensions,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum IndexCompatibilityStatus {
    Compatible,
    ReindexRequired(IndexCompatibilityMismatch),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticSnapshot {
    pub generation: GenerationId,
    pub paths: Vec<WorkspacePath>,
    pub chunks: Vec<IndexedChunk>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RevisionOutcome {
    Applied,
    Unchanged,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SemanticQuery {
    pub text: String,
    pub limit: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SemanticFilter {
    pub paths: Vec<WorkspacePath>,
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChunkingConfig {
    pub algorithm_version: String,
    pub target_bytes: usize,
    pub overlap_bytes: usize,
    pub max_file_bytes: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SemanticSearchRequest {
    pub query_text: String,
    pub limit: usize,
    pub filters: SemanticFilter,
    pub lexical_reranking: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexedChunk {
    pub chunk: SemanticChunk,
    pub embedding: EmbeddingVector,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RankedHit {
    pub chunk: SemanticChunk,
    pub rank: usize,
    pub cosine_score: f32,
    pub lexical_score: Option<f32>,
    pub final_score: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelatedFile {
    pub path: WorkspacePath,
    pub score: f32,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TaskContext {
    pub chunks: Vec<SemanticChunk>,
    pub tokens_used: u32,
    pub token_budget: u32,
    pub eligible_unfitted: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SemanticDomainError {
    InvalidWorkspacePath,
    InvalidUtf8Span,
    SnippetTooLarge,
    InvalidChunkHash,
    EmptyEmbedding,
    ZeroNormEmbedding,
    NonFiniteEmbedding,
    DimensionMismatch {
        expected: usize,
        actual: usize,
    },
    DocumentTooLarge {
        actual_bytes: usize,
        max_file_bytes: usize,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SemanticOperationError {
    TimedOut {
        operation: SemanticOperation,
        layer: DeadlineLayer,
        budget_ms: u64,
    },
    Cancelled,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum EmbeddingProviderError {
    Unavailable,
    TimedOut {
        operation: SemanticOperation,
        layer: DeadlineLayer,
        budget_ms: u64,
    },
    MalformedResponse,
    MissingIndex {
        index: usize,
    },
    DuplicateIndex {
        index: usize,
    },
    OutOfRangeIndex {
        index: usize,
        batch_len: usize,
    },
    UnorderedIndex {
        previous: usize,
        current: usize,
    },
    NonFiniteVector {
        index: usize,
    },
    ZeroNormVector {
        index: usize,
    },
    DimensionMismatch {
        expected: usize,
        actual: usize,
    },
    Cancelled,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SemanticIndexError {
    Storage,
    Corrupt,
    UnsupportedSchema,
    Incompatible(IndexCompatibilityMismatch),
    StaleRevision {
        current: PathRevision,
        attempted: PathRevision,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WorkspaceReadError {
    InvalidPath,
    NotFound,
    UnsupportedFileType,
    InvalidUtf8,
    HostUnavailable,
}
