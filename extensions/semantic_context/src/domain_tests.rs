use std::time::{Duration, Instant};

use crate::{
    BoundedSnippet, ChunkingFingerprint, DeadlineLayer, EmbeddingProviderError,
    EmbeddingProviderPort, EmbeddingVector, IndexCompatibilityMismatch, OperationDeadline,
    PortFuture, SemanticChunk, SemanticChunkInput, SemanticDomainError, SemanticIndexError,
    SemanticIndexPort, SemanticOperation, SemanticOperationError, SymbolSpan, WorkspacePath,
    WorkspaceReadError, WorkspaceReadPort,
};

fn fingerprint() -> ChunkingFingerprint {
    ChunkingFingerprint {
        algorithm_version: "tree-sitter-v1".to_owned(),
        target_bytes: 2_048,
        overlap_bytes: 256,
        max_file_bytes: 1_048_576,
    }
}

fn hash(hex: &str) -> [u8; 32] {
    assert_eq!(hex.len(), 64);
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
    }
    bytes
}

#[test]
fn semantic_chunk_accepts_valid_workspace_span() {
    let source = "fn greet() {\n    println!(\"héllo\");\n}\n";
    let path = WorkspacePath::new("src/lib.rs".to_owned()).unwrap();
    let expected_chunk_hash =
        hash("f9162ff1e78617c29c6f07c14d128b1125602c9e5e8e44eeaf540a83f993623d");
    let chunk = SemanticChunk::new(SemanticChunkInput {
        path: path.clone(),
        source,
        start_byte: 0,
        end_byte: source.len(),
        language: "rust".to_owned(),
        symbol: Some(SymbolSpan {
            start_byte: 0,
            end_byte: source.len(),
        }),
        chunking_fingerprint: fingerprint(),
        expected_chunk_hash: Some(expected_chunk_hash),
    })
    .unwrap();

    assert_eq!(chunk.path, path);
    assert_eq!(
        chunk.content_hash,
        hash("0e12663da1cca08b2e0153573043326d1abb680d0a2f1083fc4d15c2f7c50b2c")
    );
    assert_eq!((chunk.start_byte, chunk.end_byte), (0, source.len()));
    assert_eq!(chunk.language, "rust");
    assert_eq!(chunk.snippet.as_str(), source);
    assert_eq!(
        chunk.symbol,
        Some(SymbolSpan {
            start_byte: 0,
            end_byte: source.len(),
        })
    );
    assert_eq!(chunk.chunking_fingerprint, fingerprint());
    assert_eq!(chunk.chunk_hash, expected_chunk_hash);

    let long_source = format!("{}éafter", "a".repeat(4_095));
    let bounded = SemanticChunk::new(SemanticChunkInput {
        path: WorkspacePath::new("src/long.rs".to_owned()).unwrap(),
        source: &long_source,
        start_byte: 0,
        end_byte: long_source.len(),
        language: "rust".to_owned(),
        symbol: None,
        chunking_fingerprint: fingerprint(),
        expected_chunk_hash: None,
    })
    .unwrap();
    assert_eq!(bounded.snippet.as_str(), "a".repeat(4_095));
    assert!(bounded.snippet.as_str().len() <= 4_096);
    assert_eq!(
        BoundedSnippet::new("a".repeat(4_097)),
        Err(SemanticDomainError::SnippetTooLarge)
    );
}

#[test]
fn semantic_chunk_rejects_invalid_paths_and_spans() {
    for path in [
        "",
        "/absolute.rs",
        "../outside.rs",
        "src/../outside.rs",
        "C:",
        "C:secret.rs",
        "C:/secret.rs",
        "z:secret.rs",
    ] {
        assert_eq!(
            WorkspacePath::new(path.to_owned()),
            Err(SemanticDomainError::InvalidWorkspacePath),
            "path {path:?} must remain workspace-relative"
        );
    }

    let source = "aéz";
    let path = WorkspacePath::new("src/lib.rs".to_owned()).unwrap();
    for (start_byte, end_byte, symbol) in [
        (4, 3, None),
        (0, source.len() + 1, None),
        (2, 3, None),
        (
            0,
            source.len(),
            Some(SymbolSpan {
                start_byte: 1,
                end_byte: 3,
            }),
        ),
        (
            1,
            3,
            Some(SymbolSpan {
                start_byte: 3,
                end_byte: 1,
            }),
        ),
    ] {
        assert_eq!(
            SemanticChunk::new(SemanticChunkInput {
                path: path.clone(),
                source,
                start_byte,
                end_byte,
                language: "rust".to_owned(),
                symbol,
                chunking_fingerprint: fingerprint(),
                expected_chunk_hash: None,
            }),
            Err(SemanticDomainError::InvalidUtf8Span)
        );
    }

    assert_eq!(
        SemanticChunk::new(SemanticChunkInput {
            path,
            source,
            start_byte: 0,
            end_byte: source.len(),
            language: "rust".to_owned(),
            symbol: None,
            chunking_fingerprint: fingerprint(),
            expected_chunk_hash: Some([0; 32]),
        }),
        Err(SemanticDomainError::InvalidChunkHash)
    );
}

#[test]
fn embedding_vector_rejects_invalid_values_and_dimensions() {
    assert_eq!(
        EmbeddingVector::new(Vec::new(), None),
        Err(SemanticDomainError::EmptyEmbedding)
    );
    assert_eq!(
        EmbeddingVector::new(vec![0.0, -0.0], None),
        Err(SemanticDomainError::ZeroNormEmbedding)
    );
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            EmbeddingVector::new(vec![1.0, invalid], None),
            Err(SemanticDomainError::NonFiniteEmbedding)
        );
    }
    assert_eq!(
        EmbeddingVector::new(vec![1.0, 2.0], Some(3)),
        Err(SemanticDomainError::DimensionMismatch {
            expected: 3,
            actual: 2,
        })
    );
    assert_eq!(
        EmbeddingVector::new(vec![3.0, 4.0], Some(2))
            .unwrap()
            .values,
        vec![3.0, 4.0]
    );
}

#[test]
fn port_errors_preserve_stable_classification() {
    fn assert_embedding_port_object_safe(_: Option<&dyn EmbeddingProviderPort>) {}
    fn assert_index_port_object_safe(_: Option<&dyn SemanticIndexPort>) {}
    fn assert_workspace_port_object_safe(_: Option<&dyn WorkspaceReadPort>) {}
    fn assert_port_future(_: PortFuture<'_, usize>) {}

    assert_embedding_port_object_safe(None);
    assert_index_port_object_safe(None);
    assert_workspace_port_object_safe(None);
    assert_port_future(Box::pin(async { 1 }));

    let now = Instant::now();
    let operation = OperationDeadline::for_operation(
        Some(now + Duration::from_secs(30)),
        SemanticOperation::Search,
        30_000,
    );
    let earlier_provider = OperationDeadline::for_provider(
        now + Duration::from_secs(10),
        SemanticOperation::Embedding,
        10_000,
    );
    let selected = OperationDeadline::select_earlier(operation.clone(), earlier_provider.clone());
    assert_eq!(selected.deadline(), earlier_provider.deadline());
    assert_eq!(selected.operation(), &SemanticOperation::Embedding);
    assert_eq!(selected.layer(), &DeadlineLayer::Provider);
    assert!(
        selected.deadline().is_some(),
        "provider deadlines remain finite"
    );

    let earlier_operation = OperationDeadline::for_operation(
        Some(now + Duration::from_secs(5)),
        SemanticOperation::Search,
        30_000,
    );
    let selected =
        OperationDeadline::select_earlier(earlier_operation.clone(), earlier_provider.clone());
    assert_eq!(selected.deadline(), earlier_operation.deadline());
    assert_eq!(selected.operation(), &SemanticOperation::Search);
    assert_eq!(selected.layer(), &DeadlineLayer::Semantic);

    let tied_provider = OperationDeadline::for_provider(
        earlier_operation.deadline().unwrap(),
        SemanticOperation::Embedding,
        10_000,
    );
    let selected = OperationDeadline::select_earlier(earlier_operation.clone(), tied_provider);
    assert_eq!(
        selected.operation(),
        &SemanticOperation::Search,
        "operation expiry wins ties"
    );
    assert_eq!(selected.layer(), &DeadlineLayer::Semantic);

    let unbounded_rebuild =
        OperationDeadline::for_operation(None, SemanticOperation::Reindex, None);
    assert_eq!(unbounded_rebuild.configured_budget_ms(), None);
    let unbounded_catch_up =
        OperationDeadline::for_operation(None, SemanticOperation::CatchUp, None);
    assert_eq!(unbounded_catch_up.configured_budget_ms(), None);
    assert_eq!(unbounded_catch_up.deadline(), None);
    assert_eq!(
        unbounded_rebuild.deadline(),
        None,
        "an absent rebuild budget is represented by no absolute deadline"
    );
    let selected = OperationDeadline::select_earlier(unbounded_rebuild, operation);
    assert_eq!(selected.deadline(), Some(now + Duration::from_secs(30)));
    assert_eq!(
        SemanticOperationError::TimedOut {
            operation: SemanticOperation::Search,
            layer: DeadlineLayer::Semantic,
            budget_ms: 125,
        },
        SemanticOperationError::TimedOut {
            operation: SemanticOperation::Search,
            layer: DeadlineLayer::Semantic,
            budget_ms: 125,
        }
    );

    let provider_errors = [
        EmbeddingProviderError::Unavailable,
        EmbeddingProviderError::TimedOut {
            operation: SemanticOperation::Embedding,
            layer: DeadlineLayer::Provider,
            budget_ms: 30_000,
        },
        EmbeddingProviderError::MalformedResponse,
        EmbeddingProviderError::MissingIndex { index: 1 },
        EmbeddingProviderError::DuplicateIndex { index: 1 },
        EmbeddingProviderError::OutOfRangeIndex {
            index: 2,
            batch_len: 2,
        },
        EmbeddingProviderError::UnorderedIndex {
            previous: 2,
            current: 1,
        },
        EmbeddingProviderError::NonFiniteVector { index: 0 },
        EmbeddingProviderError::ZeroNormVector { index: 0 },
        EmbeddingProviderError::DimensionMismatch {
            expected: 3,
            actual: 2,
        },
        EmbeddingProviderError::Cancelled,
    ];
    assert_eq!(provider_errors.len(), 11);

    let index_errors = [
        SemanticIndexError::Storage,
        SemanticIndexError::Corrupt,
        SemanticIndexError::UnsupportedSchema,
        SemanticIndexError::Incompatible(IndexCompatibilityMismatch::Schema),
        SemanticIndexError::StaleRevision {
            current: crate::PathRevision(2),
            attempted: crate::PathRevision(1),
        },
    ];
    assert_eq!(index_errors.len(), 5);

    let workspace_errors = [
        WorkspaceReadError::InvalidPath,
        WorkspaceReadError::NotFound,
        WorkspaceReadError::UnsupportedFileType,
        WorkspaceReadError::InvalidUtf8,
        WorkspaceReadError::HostUnavailable,
    ];
    assert_eq!(workspace_errors.len(), 5);
}
