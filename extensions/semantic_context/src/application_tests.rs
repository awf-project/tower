use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    time::{Duration, Instant},
};

use crate::{
    ChunkingConfig, ChunkingFingerprint, DeadlineLayer, EmbeddingProviderError, EmbeddingVector,
    GenerationId, IndexedChunk, IndexedEmbedding, OperationDeadline, RankedHit, SemanticChunk,
    SemanticChunkInput, SemanticDomainError, SemanticFilter, SemanticIndexError, SemanticOperation,
    SemanticOperationError, SemanticSearchRequest, SemanticSnapshot, SymbolSpan, WorkspaceDocument,
    WorkspacePath,
    application::{
        PublicSemanticOutcome, PublicSemanticResponse, PublicTimeoutDetails, SemanticApplication,
        SemanticApplicationError, SemanticCancellation, SemanticClock, SemanticTimeouts,
        map_public_outcome, validate_embedding_batch,
    },
    chunking::chunk_document,
    retrieval::{context_for_task, related_files, semantic_search},
};
use crate::{
    EmbeddingProviderPort, EmbeddingRequest, EmbeddingSpaceIdentity, IndexCompatibility,
    IndexCompatibilityStatus, PathReplacement, PathRevision, PortFuture, RebuiltGeneration,
    RevisionOutcome, SemanticIndexPort, WorkspaceReadError, WorkspaceReadPort,
};

#[derive(Clone)]
struct TestClock(Arc<Mutex<Instant>>);

impl TestClock {
    fn new(now: Instant) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }
    fn advance(&self, duration: Duration) {
        *self.0.lock().unwrap() += duration;
    }
}

impl SemanticClock for TestClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

#[derive(Default)]
struct TestCancellation(AtomicBool);

struct InterruptDuringRetrieval {
    checks: AtomicUsize,
    interrupt_at: usize,
    clock: TestClock,
    cancel: bool,
}

impl SemanticCancellation for InterruptDuringRetrieval {
    fn is_cancelled(&self) -> bool {
        let check = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
        if check == self.interrupt_at {
            if self.cancel {
                return true;
            }
            self.clock.advance(Duration::from_millis(100));
        }
        false
    }
}

#[test]
fn retrieval_interruptions_preserve_public_operation_outcomes() {
    for operation in [
        SemanticOperation::Search,
        SemanticOperation::ContextForTask,
        SemanticOperation::RelatedFiles,
    ] {
        for cancel in [false, true] {
            // First count the checks in a successful operation, then interrupt
            // every later checkpoint, including ranking and packing completion.
            let mut total = 0;
            for interrupt_at in std::iter::once(usize::MAX).chain(7..100) {
                if interrupt_at != usize::MAX && interrupt_at > total {
                    break;
                }
                let clock = TestClock::new(Instant::now());
                let provider = TestProvider {
                    clock: clock.clone(),
                    advance: Duration::ZERO,
                    calls: AtomicUsize::new(0),
                    deadlines: Mutex::new(Vec::new()),
                };
                let index = TestIndex {
                    snapshot: SemanticSnapshot {
                        generation: GenerationId(1),
                        paths: vec![path("src/lib.rs")],
                        chunks: (0..8)
                            .map(|i| IndexedChunk {
                                chunk: ranked_hit(&format!("src/file{i}.rs"), "needle", 1, 1.0)
                                    .chunk,
                                embedding: EmbeddingVector {
                                    values: vec![1.0, 0.0],
                                },
                            })
                            .collect(),
                    },
                    replacements: AtomicUsize::new(0),
                };
                let workspace = TestWorkspace(document("src/lib.rs", "needle"));
                let cancellation = InterruptDuringRetrieval {
                    checks: AtomicUsize::new(0),
                    interrupt_at,
                    clock: clock.clone(),
                    cancel,
                };
                let app = SemanticApplication::new(
                    &provider,
                    &index,
                    &workspace,
                    &clock,
                    &cancellation,
                    SemanticTimeouts {
                        operation: Duration::from_millis(100),
                        rebuild: None,
                        provider: Duration::from_millis(200),
                    },
                );
                let request = SemanticSearchRequest {
                    query_text: "needle".into(),
                    limit: 8,
                    filters: SemanticFilter {
                        paths: vec![],
                        languages: vec![],
                    },
                    lexical_reranking: true,
                };
                let result = match operation {
                    SemanticOperation::ContextForTask => {
                        block_on(app.context_for_task(&request, 100)).map(|_| ())
                    }
                    SemanticOperation::RelatedFiles => block_on(app.related_files(
                        &path("src/lib.rs"),
                        &request,
                        &ChunkingConfig {
                            algorithm_version: "v1".into(),
                            target_bytes: 4096,
                            overlap_bytes: 0,
                            max_file_bytes: 4096,
                        },
                    ))
                    .map(|_| ()),
                    _ => block_on(app.search(&request)).map(|_| ()),
                };
                if interrupt_at == usize::MAX {
                    result.unwrap();
                    total = cancellation.checks.load(Ordering::SeqCst);
                    assert!(total > 15, "fixture reaches retrieval CPU checkpoints");
                    continue;
                }
                let Err(SemanticApplicationError::Operation(error)) = result else {
                    panic!("interruption at {interrupt_at} of {total} returned {result:?}");
                };
                if !cancel {
                    assert_eq!(
                        error,
                        SemanticOperationError::TimedOut {
                            operation: operation.clone(),
                            layer: DeadlineLayer::Semantic,
                            budget_ms: 100,
                        }
                    );
                }
                let response = map_public_outcome(PublicSemanticOutcome::Operation(error));
                assert_eq!(response.status, "error");
                assert_eq!(
                    response.code,
                    Some(if cancel {
                        "cancelled"
                    } else {
                        "operation_timeout"
                    })
                );
            }
        }
    }
}

impl SemanticCancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct TestProvider {
    clock: TestClock,
    advance: Duration,
    calls: AtomicUsize,
    deadlines: Mutex<Vec<OperationDeadline>>,
}

impl EmbeddingProviderPort for TestProvider {
    fn identity(&self) -> EmbeddingSpaceIdentity {
        EmbeddingSpaceIdentity {
            provider: "test".into(),
            model: "test".into(),
            dimensions: Some(2),
        }
    }
    fn embed<'a>(
        &'a self,
        request: &'a EmbeddingRequest,
        deadline: OperationDeadline,
    ) -> PortFuture<'a, Result<Vec<IndexedEmbedding>, EmbeddingProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.deadlines.lock().unwrap().push(deadline);
        self.clock.advance(self.advance);
        Box::pin(async move {
            Ok(request
                .inputs
                .iter()
                .enumerate()
                .map(|(index, _)| IndexedEmbedding {
                    index,
                    vector: EmbeddingVector {
                        values: vec![1.0, 0.0],
                    },
                })
                .collect())
        })
    }
}

struct DeadlineTimeoutProvider;

impl EmbeddingProviderPort for DeadlineTimeoutProvider {
    fn identity(&self) -> EmbeddingSpaceIdentity {
        EmbeddingSpaceIdentity {
            provider: "timeout".into(),
            model: "timeout".into(),
            dimensions: Some(2),
        }
    }

    fn embed<'a>(
        &'a self,
        _: &'a EmbeddingRequest,
        deadline: OperationDeadline,
    ) -> PortFuture<'a, Result<Vec<IndexedEmbedding>, EmbeddingProviderError>> {
        Box::pin(async move {
            Err(EmbeddingProviderError::TimedOut {
                operation: deadline.operation().clone(),
                layer: deadline.layer().clone(),
                budget_ms: deadline.budget_ms(),
            })
        })
    }
}

struct TestIndex {
    snapshot: SemanticSnapshot,
    replacements: AtomicUsize,
}

impl SemanticIndexPort for TestIndex {
    fn compatibility(
        &self,
        _: &IndexCompatibility,
    ) -> Result<IndexCompatibilityStatus, SemanticIndexError> {
        Ok(IndexCompatibilityStatus::Compatible)
    }
    fn snapshot(&self) -> Result<SemanticSnapshot, SemanticIndexError> {
        Ok(self.snapshot.clone())
    }
    fn replace_path(&self, _: PathReplacement) -> Result<RevisionOutcome, SemanticIndexError> {
        self.replacements.fetch_add(1, Ordering::SeqCst);
        Ok(RevisionOutcome::Applied)
    }
    fn delete_path(
        &self,
        _: &WorkspacePath,
        _: PathRevision,
    ) -> Result<RevisionOutcome, SemanticIndexError> {
        Ok(RevisionOutcome::Applied)
    }
    fn publish_generation(
        &self,
        generation: RebuiltGeneration,
    ) -> Result<GenerationId, SemanticIndexError> {
        Ok(generation.id)
    }
}

struct TestWorkspace(WorkspaceDocument);

impl WorkspaceReadPort for TestWorkspace {
    fn list_files<'a>(&'a self) -> PortFuture<'a, Result<Vec<WorkspacePath>, WorkspaceReadError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn read_file<'a>(
        &'a self,
        _: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<WorkspaceDocument, WorkspaceReadError>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
    fn symbol_spans<'a>(
        &'a self,
        _: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<Option<Vec<SymbolSpan>>, WorkspaceReadError>> {
        Box::pin(async {
            Ok((!self.0.symbol_spans.is_empty()).then(|| self.0.symbol_spans.clone()))
        })
    }
}

fn block_on<T>(mut future: impl Future<Output = T>) -> T {
    fn clone(_: *const ()) -> RawWaker {
        raw_waker()
    }
    fn wake(_: *const ()) {}
    fn wake_by_ref(_: *const ()) {}
    fn drop(_: *const ()) {}
    fn raw_waker() -> RawWaker {
        RawWaker::new(
            std::ptr::null(),
            &RawWakerVTable::new(clone, wake, wake_by_ref, drop),
        )
    }
    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut context = Context::from_waker(&waker);
    let mut future = unsafe { Pin::new_unchecked(&mut future) };
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value.to_owned()).unwrap()
}

fn fingerprint(target_bytes: u64, overlap_bytes: u64) -> ChunkingFingerprint {
    ChunkingFingerprint {
        algorithm_version: "semantic-v1".to_owned(),
        target_bytes,
        overlap_bytes,
        max_file_bytes: 2 * 1024 * 1024,
    }
}

fn document(path_value: &str, source: &str) -> WorkspaceDocument {
    WorkspaceDocument {
        path: path(path_value),
        source: source.to_owned(),
        content_hash: [0; 32],
        language: "rust".to_owned(),
        symbol_spans: Vec::new(),
    }
}

fn chunk(path_value: &str, source: &str, start_byte: usize, end_byte: usize) -> SemanticChunk {
    SemanticChunk::new(SemanticChunkInput {
        path: path(path_value),
        source,
        start_byte,
        end_byte,
        language: "rust".to_owned(),
        symbol: None,
        chunking_fingerprint: fingerprint(4_096, 512),
        expected_chunk_hash: None,
    })
    .unwrap()
}

fn chunk_with(
    path_value: &str,
    source: &str,
    start_byte: usize,
    end_byte: usize,
    language: &str,
    fingerprint: ChunkingFingerprint,
) -> SemanticChunk {
    SemanticChunk::new(SemanticChunkInput {
        path: path(path_value),
        source,
        start_byte,
        end_byte,
        language: language.to_owned(),
        symbol: None,
        chunking_fingerprint: fingerprint,
        expected_chunk_hash: None,
    })
    .unwrap()
}

fn ranked_hit(path_value: &str, snippet: &str, rank: usize, final_score: f32) -> RankedHit {
    RankedHit {
        chunk: chunk(path_value, snippet, 0, snippet.len()),
        rank,
        cosine_score: final_score,
        lexical_score: None,
        final_score,
    }
}

#[test]
fn semantic_chunking_preserves_utf8_spans_and_is_deterministic() {
    let config = ChunkingConfig {
        algorithm_version: "semantic-v1".to_owned(),
        target_bytes: 12,
        overlap_bytes: 3,
        max_file_bytes: 64,
    };
    let source = "alpha\nééé\nomega line\n";
    let workspace_document = document("src/lib.rs", source);

    let first = chunk_document(&workspace_document, &config).unwrap();
    let second = chunk_document(&workspace_document, &config).unwrap();

    assert_eq!(first, second);
    assert!(!first.is_empty());
    assert!(
        first
            .windows(2)
            .all(|pair| pair[0].end_byte > pair[1].start_byte)
    );
    for item in &first {
        assert!(source.is_char_boundary(item.start_byte));
        assert!(source.is_char_boundary(item.end_byte));
        assert_eq!(
            item.snippet.as_str(),
            &source[item.start_byte..item.end_byte]
        );
    }

    let empty = chunk_document(&document("src/empty.rs", ""), &config).unwrap();
    assert!(empty.is_empty());

    let symbol_source = "fn one() {}\nfn two() {}\n";
    let symbol_document = WorkspaceDocument {
        path: path("src/symbols.rs"),
        source: symbol_source.to_owned(),
        content_hash: [0; 32],
        language: "rust".to_owned(),
        symbol_spans: vec![
            SymbolSpan {
                start_byte: 0,
                end_byte: 11,
            },
            SymbolSpan {
                start_byte: 12,
                end_byte: 23,
            },
        ],
    };
    let symbol_chunks = chunk_document(
        &symbol_document,
        &ChunkingConfig {
            target_bytes: 8,
            ..config.clone()
        },
    )
    .unwrap();
    assert_eq!(
        symbol_chunks
            .iter()
            .map(|item| (item.start_byte, item.end_byte, item.symbol))
            .collect::<Vec<_>>(),
        vec![
            (
                0,
                11,
                Some(SymbolSpan {
                    start_byte: 0,
                    end_byte: 11
                })
            ),
            (
                12,
                23,
                Some(SymbolSpan {
                    start_byte: 12,
                    end_byte: 23
                })
            ),
        ]
    );
    assert_eq!(
        chunk_document(&document("src/too-large.rs", &"x".repeat(65)), &config),
        Err(SemanticDomainError::DocumentTooLarge {
            actual_bytes: 65,
            max_file_bytes: 64,
        })
    );
    assert_eq!(
        chunk_document(
            &document("src/line.rs", "aaaa\nbbbbb\n"),
            &ChunkingConfig {
                target_bytes: 8,
                overlap_bytes: 0,
                ..config.clone()
            },
        )
        .unwrap()
        .iter()
        .map(|item| (item.start_byte, item.end_byte))
        .collect::<Vec<_>>(),
        vec![(0, 5), (5, 11)]
    );
    assert_eq!(
        chunk_document(
            &document("src/hard.rs", "ééé"),
            &ChunkingConfig {
                target_bytes: 5,
                overlap_bytes: 0,
                ..config.clone()
            },
        )
        .unwrap()
        .iter()
        .map(|item| (item.start_byte, item.end_byte))
        .collect::<Vec<_>>(),
        vec![(0, 4), (4, 6)]
    );
    assert_ne!(
        first[0].chunking_fingerprint,
        chunk_document(
            &workspace_document,
            &ChunkingConfig {
                target_bytes: 13,
                ..config
            },
        )
        .unwrap()[0]
            .chunking_fingerprint
    );
}

#[test]
fn provider_responses_require_complete_unique_finite_consistent_vectors() {
    let vector = |index, values| IndexedEmbedding {
        index,
        vector: EmbeddingVector { values },
    };

    assert_eq!(
        validate_embedding_batch(
            vec![vector(0, vec![1.0, 0.0]), vector(1, vec![0.0, 1.0])],
            2,
            Some(2),
        )
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        validate_embedding_batch(vec![vector(2, vec![1.0])], 2, None),
        Err(EmbeddingProviderError::OutOfRangeIndex {
            index: 2,
            batch_len: 2,
        })
    );
    assert_eq!(
        validate_embedding_batch(vec![vector(0, vec![1.0]), vector(0, vec![1.0])], 2, None,),
        Err(EmbeddingProviderError::DuplicateIndex { index: 0 })
    );
    assert_eq!(
        validate_embedding_batch(
            vec![
                vector(0, vec![1.0]),
                vector(1, vec![1.0]),
                vector(0, vec![1.0]),
            ],
            3,
            None,
        ),
        Err(EmbeddingProviderError::DuplicateIndex { index: 0 })
    );
    assert_eq!(
        validate_embedding_batch(vec![vector(1, vec![1.0]), vector(0, vec![1.0])], 2, None,),
        Err(EmbeddingProviderError::UnorderedIndex {
            previous: 1,
            current: 0,
        })
    );
    assert_eq!(
        validate_embedding_batch(vec![vector(1, vec![1.0])], 2, None),
        Err(EmbeddingProviderError::MissingIndex { index: 0 })
    );
    assert_eq!(
        validate_embedding_batch(vec![vector(0, vec![f32::NAN])], 1, None),
        Err(EmbeddingProviderError::NonFiniteVector { index: 0 })
    );
    assert_eq!(
        validate_embedding_batch(vec![vector(0, vec![0.0, -0.0])], 1, None),
        Err(EmbeddingProviderError::ZeroNormVector { index: 0 })
    );
    assert_eq!(
        validate_embedding_batch(vec![vector(0, vec![1.0, 2.0])], 1, Some(3)),
        Err(EmbeddingProviderError::DimensionMismatch {
            expected: 3,
            actual: 2,
        })
    );
    assert_eq!(
        validate_embedding_batch(
            vec![vector(0, vec![1.0, 0.0]), vector(1, vec![0.0, 1.0, 0.0])],
            2,
            None,
        ),
        Err(EmbeddingProviderError::DimensionMismatch {
            expected: 2,
            actual: 3,
        })
    );
}

#[test]
fn semantic_search_applies_filters_and_stable_score_ties() {
    let a_source = "foo_bar foo";
    let b_source = "foo_bar";
    let c_source = "foo_bar";
    let snapshot = SemanticSnapshot {
        generation: GenerationId(7),
        paths: vec![path("src/b.rs"), path("src/a.rs"), path("tests/c.rs")],
        chunks: vec![
            IndexedChunk {
                chunk: chunk_with(
                    "src/b.rs",
                    b_source,
                    0,
                    b_source.len(),
                    "rust",
                    fingerprint(4_096, 512),
                ),
                embedding: EmbeddingVector {
                    values: vec![0.8, 0.6],
                },
            },
            IndexedChunk {
                chunk: chunk_with(
                    "src/a.rs",
                    a_source,
                    0,
                    a_source.len(),
                    "rust",
                    fingerprint(4_096, 512),
                ),
                embedding: EmbeddingVector {
                    values: vec![1.0, 0.0],
                },
            },
            IndexedChunk {
                chunk: chunk_with(
                    "tests/c.py",
                    c_source,
                    0,
                    c_source.len(),
                    "python",
                    fingerprint(4_096, 512),
                ),
                embedding: EmbeddingVector {
                    values: vec![1.0, 0.0],
                },
            },
        ],
    };
    let request = SemanticSearchRequest {
        query_text: "FOO_bar, foo_bar!!! foo".to_owned(),
        limit: 2,
        filters: SemanticFilter {
            paths: vec![path("src/a.rs"), path("src/b.rs"), path("tests/c.py")],
            languages: vec!["rust".to_owned()],
        },
        lexical_reranking: true,
    };

    let hits = semantic_search(
        &snapshot,
        &EmbeddingVector {
            values: vec![1.0, 0.0],
        },
        &request,
    )
    .unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].chunk.path, path("src/a.rs"));
    assert_eq!(hits[0].lexical_score, Some(1.0));
    assert_eq!(hits[0].final_score, 1.0);
    assert_eq!(hits[1].chunk.path, path("src/b.rs"));
    assert_eq!(hits[1].lexical_score, Some(0.5));
    assert_eq!(hits[1].cosine_score, 0.8);
    assert_eq!(hits[1].final_score, 0.8_f32 * 0.8 + 0.2 * 0.5);

    let language_independent = semantic_search(
        &snapshot,
        &EmbeddingVector {
            values: vec![1.0, 0.0],
        },
        &SemanticSearchRequest {
            query_text: "!!!".to_owned(),
            limit: 2,
            filters: SemanticFilter {
                paths: vec![path("src/a.rs"), path("tests/c.py")],
                languages: vec!["rust".to_owned(), "python".to_owned()],
            },
            lexical_reranking: true,
        },
    )
    .unwrap();
    assert_eq!(
        language_independent
            .iter()
            .map(|hit| hit.chunk.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.rs", "tests/c.py"]
    );
    assert!(
        language_independent
            .iter()
            .all(|hit| hit.lexical_score == Some(0.0))
    );

    let without_reranking = semantic_search(
        &snapshot,
        &EmbeddingVector {
            values: vec![1.0, 0.0],
        },
        &SemanticSearchRequest {
            lexical_reranking: false,
            ..request
        },
    )
    .unwrap();
    assert_eq!(
        without_reranking
            .iter()
            .map(|hit| hit.chunk.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.rs", "src/b.rs"]
    );

    let tie_source = "alpha\nbravo\n";
    let mut tied = vec![
        IndexedChunk {
            chunk: chunk_with(
                "src/tie.rs",
                tie_source,
                6,
                11,
                "rust",
                fingerprint(4_096, 512),
            ),
            embedding: EmbeddingVector {
                values: vec![1.0, 0.0],
            },
        },
        IndexedChunk {
            chunk: chunk_with(
                "src/tie.rs",
                tie_source,
                0,
                5,
                "rust",
                fingerprint(4_096, 512),
            ),
            embedding: EmbeddingVector {
                values: vec![1.0, 0.0],
            },
        },
    ];
    let first_hash = chunk_with("src/hash.rs", "same", 0, 4, "rust", fingerprint(4_096, 512));
    let second_hash = chunk_with("src/hash.rs", "same", 0, 4, "rust", fingerprint(4_097, 512));
    tied.push(IndexedChunk {
        chunk: first_hash.clone(),
        embedding: EmbeddingVector {
            values: vec![1.0, 0.0],
        },
    });
    tied.push(IndexedChunk {
        chunk: second_hash.clone(),
        embedding: EmbeddingVector {
            values: vec![1.0, 0.0],
        },
    });
    let ties = semantic_search(
        &SemanticSnapshot {
            generation: GenerationId(8),
            paths: vec![path("src/tie.rs"), path("src/hash.rs")],
            chunks: tied,
        },
        &EmbeddingVector {
            values: vec![1.0, 0.0],
        },
        &SemanticSearchRequest {
            query_text: String::new(),
            limit: 4,
            filters: SemanticFilter {
                paths: Vec::new(),
                languages: Vec::new(),
            },
            lexical_reranking: false,
        },
    )
    .unwrap();
    assert_eq!(ties[0].chunk.path, path("src/hash.rs"));
    assert_eq!(ties[1].chunk.path, path("src/hash.rs"));
    assert!(ties[0].chunk.chunk_hash < ties[1].chunk.chunk_hash);
    assert_eq!(
        ties[2..]
            .iter()
            .map(|hit| hit.chunk.start_byte)
            .collect::<Vec<_>>(),
        vec![0, 6]
    );
}

#[test]
fn related_files_keep_best_score_and_deduplicate_paths() {
    let hits = vec![
        ranked_hit("src/a.rs", "second", 3, 0.4),
        ranked_hit("src/b.rs", "best b", 1, 0.8),
        ranked_hit("src/a.rs", "best a", 2, 0.8),
        ranked_hit("src/c.rs", "ignored", 4, 0.1),
    ];

    let related = related_files(&hits, 2);

    assert_eq!(related.len(), 2);
    assert_eq!(related[0].path, path("src/a.rs"));
    assert_eq!(related[0].score, 0.8);
    assert_eq!(related[1].path, path("src/b.rs"));
    assert_eq!(related[1].score, 0.8);
}

#[test]
fn task_context_respects_budget_and_diversifies_sources() {
    let duplicate = ranked_hit("src/a.rs", "12345678", 4, 0.5);
    let hits = vec![
        ranked_hit("src/a.rs", "12345678", 1, 1.0),
        ranked_hit("src/a.rs", "abcdefghijkl", 2, 0.9),
        ranked_hit("src/b.rs", "abcdefgh", 3, 0.8),
        duplicate.clone(),
    ];

    let exact = context_for_task(&hits, 6, 4);
    assert_eq!(
        exact
            .chunks
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.rs", "src/b.rs"]
    );
    assert_eq!(exact.tokens_used, 6);
    assert_eq!(exact.eligible_unfitted, 1);
    assert!(exact.truncated);
    assert_eq!(
        exact
            .chunks
            .iter()
            .map(|item| item.chunk_hash)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        exact.chunks.len()
    );

    let zero = context_for_task(&hits, 0, 0);
    assert!(zero.chunks.is_empty());
    assert_eq!(zero.tokens_used, 0);
    assert_eq!(zero.eligible_unfitted, 3);
    assert!(zero.truncated);

    let tiny = context_for_task(&hits, 1, hits.len());
    assert!(tiny.chunks.is_empty());
    assert_eq!(tiny.eligible_unfitted, 3);

    let partial = context_for_task(&hits, 2, hits.len());
    assert_eq!(partial.chunks.len(), 1);
    assert_eq!(partial.chunks[0].path, path("src/a.rs"));
    assert_eq!(partial.tokens_used, 2);
    assert_eq!(partial.eligible_unfitted, 2);
    assert!(partial.truncated);
}

#[test]
fn application_failures_map_to_stable_public_outcomes() {
    let assert_public = |outcome, expected: PublicSemanticResponse| {
        let response = map_public_outcome(outcome);
        assert_eq!(response, expected);
        for confidential in [
            "file:///private/store",
            "user:secret@host",
            "api-key",
            "response-body",
        ] {
            assert!(!response.message.contains(confidential));
        }
    };
    assert_public(
        PublicSemanticOutcome::Success,
        PublicSemanticResponse {
            status: "ok",
            reason: None,
            code: None,
            message: "Semantic operation completed",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::ProviderNotConfigured,
        PublicSemanticResponse {
            status: "unsupported",
            reason: Some("provider_not_configured"),
            code: None,
            message: "Semantic provider is not configured",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Index(SemanticIndexError::Incompatible(
            crate::IndexCompatibilityMismatch::Schema,
        )),
        PublicSemanticResponse {
            status: "reindex_required",
            reason: Some("incompatible_index"),
            code: None,
            message: "Semantic index is incompatible",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Index(SemanticIndexError::Corrupt),
        PublicSemanticResponse {
            status: "reindex_required",
            reason: Some("corrupt_index"),
            code: None,
            message: "Semantic index is corrupt",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Index(SemanticIndexError::UnsupportedSchema),
        PublicSemanticResponse {
            status: "reindex_required",
            reason: Some("unsupported_schema"),
            code: None,
            message: "Semantic index schema is unsupported",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Provider(EmbeddingProviderError::Unavailable),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("provider_unavailable"),
            message: "Semantic provider is unavailable",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Provider(EmbeddingProviderError::MalformedResponse),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("provider_invalid_response"),
            message: "Semantic provider returned an invalid response",
            timeout: None,
        },
    );
    for invalid_response in [
        EmbeddingProviderError::MissingIndex { index: 0 },
        EmbeddingProviderError::DuplicateIndex { index: 0 },
        EmbeddingProviderError::OutOfRangeIndex {
            index: 2,
            batch_len: 2,
        },
        EmbeddingProviderError::UnorderedIndex {
            previous: 1,
            current: 0,
        },
        EmbeddingProviderError::NonFiniteVector { index: 0 },
        EmbeddingProviderError::ZeroNormVector { index: 0 },
    ] {
        assert_public(
            PublicSemanticOutcome::Provider(invalid_response),
            PublicSemanticResponse {
                status: "error",
                reason: None,
                code: Some("provider_invalid_response"),
                message: "Semantic provider returned an invalid response",
                timeout: None,
            },
        );
    }
    assert_public(
        PublicSemanticOutcome::Provider(EmbeddingProviderError::DimensionMismatch {
            expected: 2,
            actual: 3,
        }),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("provider_dimension_mismatch"),
            message: "Semantic provider dimensions do not match",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Provider(EmbeddingProviderError::TimedOut {
            operation: SemanticOperation::Embedding,
            layer: DeadlineLayer::Provider,
            budget_ms: 30,
        }),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("provider_timeout"),
            message: "Semantic provider timed out",
            timeout: Some(PublicTimeoutDetails {
                operation: SemanticOperation::Embedding,
                layer: DeadlineLayer::Provider,
                budget_ms: 30,
            }),
        },
    );
    assert_public(
        PublicSemanticOutcome::Operation(SemanticOperationError::TimedOut {
            operation: SemanticOperation::Search,
            layer: DeadlineLayer::Semantic,
            budget_ms: 100,
        }),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("operation_timeout"),
            message: "Semantic operation timed out",
            timeout: Some(PublicTimeoutDetails {
                operation: SemanticOperation::Search,
                layer: DeadlineLayer::Semantic,
                budget_ms: 100,
            }),
        },
    );
    assert_public(
        PublicSemanticOutcome::Operation(SemanticOperationError::Cancelled),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("cancelled"),
            message: "Semantic operation was cancelled",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Provider(EmbeddingProviderError::Cancelled),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("cancelled"),
            message: "Semantic operation was cancelled",
            timeout: None,
        },
    );
    assert_public(
        PublicSemanticOutcome::Index(SemanticIndexError::Storage),
        PublicSemanticResponse {
            status: "error",
            reason: None,
            code: Some("storage_error"),
            message: "Semantic index storage failed",
            timeout: None,
        },
    );
    let now = Instant::now();
    let operation = OperationDeadline::for_operation(
        Some(now + Duration::from_millis(100)),
        SemanticOperation::Search,
        100,
    );
    let provider = OperationDeadline::for_provider(
        now + Duration::from_millis(30),
        SemanticOperation::Embedding,
        30,
    );
    let selected = OperationDeadline::select_earlier(operation.clone(), provider.clone());
    assert_eq!(selected.operation(), &SemanticOperation::Embedding);
    assert_eq!(selected.layer(), &DeadlineLayer::Provider);
    assert_eq!(selected.budget_ms(), 30);

    let tied_provider = OperationDeadline::for_provider(
        operation.deadline().unwrap(),
        SemanticOperation::Embedding,
        100,
    );
    let selected = OperationDeadline::select_earlier(operation.clone(), tied_provider);
    assert_eq!(selected.operation(), &SemanticOperation::Search);
    assert_eq!(selected.layer(), &DeadlineLayer::Semantic);
    assert_eq!(selected.budget_ms(), 100);

    let rebuild = OperationDeadline::for_operation(None, SemanticOperation::Reindex, 0);
    assert_eq!(rebuild.deadline(), None);
    let capped = OperationDeadline::select_earlier(rebuild, provider);
    assert_eq!(capped.layer(), &DeadlineLayer::Provider);

    assert_eq!(
        validate_embedding_batch(
            vec![IndexedEmbedding {
                index: 1,
                vector: EmbeddingVector { values: vec![1.0] },
            }],
            1,
            None,
        ),
        Err(EmbeddingProviderError::OutOfRangeIndex {
            index: 1,
            batch_len: 1,
        })
    );

    let run = |operation: Duration,
               provider_budget: Duration,
               provider_advance: Duration,
               cancelled: bool| {
        let clock = TestClock::new(Instant::now());
        let provider = TestProvider {
            clock: clock.clone(),
            advance: provider_advance,
            calls: AtomicUsize::new(0),
            deadlines: Mutex::new(Vec::new()),
        };
        let indexed = chunk("src/lib.rs", "needle", 0, 6);
        let index = TestIndex {
            snapshot: SemanticSnapshot {
                generation: GenerationId(1),
                paths: vec![path("src/lib.rs")],
                chunks: vec![IndexedChunk {
                    chunk: indexed,
                    embedding: EmbeddingVector {
                        values: vec![1.0, 0.0],
                    },
                }],
            },
            replacements: AtomicUsize::new(0),
        };
        let workspace = TestWorkspace(document("src/lib.rs", "needle"));
        let cancellation = TestCancellation(AtomicBool::new(cancelled));
        let app = SemanticApplication::new(
            &provider,
            &index,
            &workspace,
            &clock,
            &cancellation,
            SemanticTimeouts {
                operation,
                rebuild: Some(operation),
                provider: provider_budget,
            },
        );
        let result = block_on(app.search(&SemanticSearchRequest {
            query_text: "needle".into(),
            limit: 1,
            filters: SemanticFilter {
                paths: Vec::new(),
                languages: Vec::new(),
            },
            lexical_reranking: false,
        }));
        (
            result,
            provider.calls.load(Ordering::SeqCst),
            provider.deadlines.into_inner().unwrap(),
        )
    };

    let (result, calls, deadlines) = run(
        Duration::from_millis(100),
        Duration::from_millis(30),
        Duration::ZERO,
        false,
    );
    assert_eq!(
        result.unwrap().len(),
        1,
        "public application search composes snapshot, provider, and retrieval"
    );
    assert_eq!(calls, 1);
    assert_eq!(deadlines.len(), 1);
    assert_eq!(deadlines[0].layer(), &DeadlineLayer::Provider);

    let clock = TestClock::new(Instant::now());
    let index = TestIndex {
        snapshot: SemanticSnapshot {
            generation: GenerationId(2),
            paths: Vec::new(),
            chunks: Vec::new(),
        },
        replacements: AtomicUsize::new(0),
    };
    let workspace = TestWorkspace(document("src/lib.rs", "needle"));
    let cancellation = TestCancellation::default();
    let app = SemanticApplication::new(
        &DeadlineTimeoutProvider,
        &index,
        &workspace,
        &clock,
        &cancellation,
        SemanticTimeouts {
            operation: Duration::from_millis(10),
            rebuild: None,
            provider: Duration::from_millis(30),
        },
    );
    let semantic_timeout = block_on(app.search(&SemanticSearchRequest {
        query_text: "needle".into(),
        limit: 1,
        filters: SemanticFilter {
            paths: Vec::new(),
            languages: Vec::new(),
        },
        lexical_reranking: false,
    }));
    assert_eq!(
        semantic_timeout,
        Err(SemanticApplicationError::Operation(
            SemanticOperationError::TimedOut {
                operation: SemanticOperation::Search,
                layer: DeadlineLayer::Semantic,
                budget_ms: 10,
            }
        ))
    );

    let clock = TestClock::new(Instant::now());
    let provider = TestProvider {
        clock: clock.clone(),
        advance: Duration::ZERO,
        calls: AtomicUsize::new(0),
        deadlines: Mutex::new(Vec::new()),
    };
    let indexed = chunk("src/result.rs", "needle", 0, 6);
    let index = TestIndex {
        snapshot: SemanticSnapshot {
            generation: GenerationId(2),
            paths: vec![path("src/result.rs")],
            chunks: vec![IndexedChunk {
                chunk: indexed,
                embedding: EmbeddingVector {
                    values: vec![1.0, 0.0],
                },
            }],
        },
        replacements: AtomicUsize::new(0),
    };
    let source = "fn one() {}\nfn two() {}\n";
    let workspace = TestWorkspace(WorkspaceDocument {
        path: path("src/source.rs"),
        source: source.into(),
        content_hash: [0; 32],
        language: "rust".into(),
        symbol_spans: vec![
            SymbolSpan {
                start_byte: 0,
                end_byte: 11,
            },
            SymbolSpan {
                start_byte: 12,
                end_byte: 23,
            },
        ],
    });
    let cancellation = TestCancellation::default();
    let app = SemanticApplication::new(
        &provider,
        &index,
        &workspace,
        &clock,
        &cancellation,
        SemanticTimeouts {
            operation: Duration::from_millis(100),
            rebuild: None,
            provider: Duration::from_millis(30),
        },
    );
    let request = SemanticSearchRequest {
        query_text: "needle".into(),
        limit: 4,
        filters: SemanticFilter {
            paths: Vec::new(),
            languages: Vec::new(),
        },
        lexical_reranking: false,
    };
    let related = block_on(app.related_files(
        &path("src/source.rs"),
        &request,
        &ChunkingConfig {
            algorithm_version: "v1".into(),
            target_bytes: 8,
            overlap_bytes: 0,
            max_file_bytes: 1024,
        },
    ))
    .unwrap();
    assert_eq!(related[0].path, path("src/result.rs"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let deadlines = provider.deadlines.lock().unwrap();
    assert_eq!(deadlines.len(), 2);
    assert_eq!(deadlines[0].deadline(), deadlines[1].deadline());
    assert_eq!(deadlines[0].layer(), &DeadlineLayer::Provider);
    drop(deadlines);

    let context = block_on(app.context_for_task(&request, 8)).unwrap();
    assert_eq!(context.chunks.len(), 1);
    assert_eq!(context.chunks[0].path, path("src/result.rs"));

    let clock = TestClock::new(Instant::now());
    let cumulative_provider = TestProvider {
        clock: clock.clone(),
        advance: Duration::from_millis(20),
        calls: AtomicUsize::new(0),
        deadlines: Mutex::new(Vec::new()),
    };
    let app = SemanticApplication::new(
        &cumulative_provider,
        &index,
        &workspace,
        &clock,
        &cancellation,
        SemanticTimeouts {
            operation: Duration::from_millis(100),
            rebuild: None,
            provider: Duration::from_millis(30),
        },
    );
    let result = block_on(app.related_files(
        &path("src/source.rs"),
        &request,
        &ChunkingConfig {
            algorithm_version: "v1".into(),
            target_bytes: 8,
            overlap_bytes: 0,
            max_file_bytes: 1024,
        },
    ));
    assert!(
        result.is_ok(),
        "separate successful application-issued calls may cumulatively exceed a provider-call budget"
    );
    assert_eq!(cumulative_provider.calls.load(Ordering::SeqCst), 2);
    let deadlines = cumulative_provider.deadlines.into_inner().unwrap();
    assert!(deadlines[1].deadline() > deadlines[0].deadline());

    let (expired, calls, _) = run(
        Duration::ZERO,
        Duration::from_millis(30),
        Duration::ZERO,
        false,
    );
    assert_eq!(
        expired,
        Err(SemanticApplicationError::Operation(
            SemanticOperationError::TimedOut {
                operation: SemanticOperation::Search,
                layer: DeadlineLayer::Semantic,
                budget_ms: 0,
            }
        ))
    );
    assert_eq!(calls, 0, "expired operations never call the provider");

    let (tied, _, _) = run(
        Duration::from_millis(30),
        Duration::from_millis(30),
        Duration::from_millis(30),
        false,
    );
    assert!(matches!(
        tied,
        Err(SemanticApplicationError::Operation(
            SemanticOperationError::TimedOut {
                layer: DeadlineLayer::Semantic,
                ..
            }
        ))
    ));

    let (provider_timeout, _, _) = run(
        Duration::from_millis(100),
        Duration::from_millis(30),
        Duration::from_millis(30),
        false,
    );
    assert!(matches!(
        provider_timeout,
        Err(SemanticApplicationError::Provider(
            EmbeddingProviderError::TimedOut {
                layer: DeadlineLayer::Provider,
                ..
            }
        ))
    ));

    let (cancelled, calls, _) = run(
        Duration::from_millis(100),
        Duration::from_millis(30),
        Duration::ZERO,
        true,
    );
    assert_eq!(
        cancelled,
        Err(SemanticApplicationError::Operation(
            SemanticOperationError::Cancelled
        ))
    );
    assert_eq!(calls, 0);

    let clock = TestClock::new(Instant::now());
    let provider = TestProvider {
        clock: clock.clone(),
        advance: Duration::from_millis(30),
        calls: AtomicUsize::new(0),
        deadlines: Mutex::new(Vec::new()),
    };
    let index = TestIndex {
        snapshot: SemanticSnapshot {
            generation: GenerationId(1),
            paths: Vec::new(),
            chunks: Vec::new(),
        },
        replacements: AtomicUsize::new(0),
    };
    let workspace = TestWorkspace(document("src/lib.rs", "needle"));
    let cancellation = TestCancellation::default();
    let app = SemanticApplication::new(
        &provider,
        &index,
        &workspace,
        &clock,
        &cancellation,
        SemanticTimeouts {
            operation: Duration::from_millis(100),
            rebuild: Some(Duration::from_millis(100)),
            provider: Duration::from_millis(30),
        },
    );
    let replacement = block_on(app.replace_path(
        &path("src/lib.rs"),
        PathRevision(1),
        &ChunkingConfig {
            algorithm_version: "v1".into(),
            target_bytes: 4_096,
            overlap_bytes: 512,
            max_file_bytes: 2 * 1024 * 1024,
        },
    ));
    assert!(matches!(
        replacement,
        Err(SemanticApplicationError::Provider(
            EmbeddingProviderError::TimedOut { .. }
        ))
    ));
    assert_eq!(
        index.replacements.load(Ordering::SeqCst),
        0,
        "expired embeddings are never published"
    );
}

#[test]
fn rebuild_preparation_shares_operation_budget_and_refreshes_provider_calls() {
    use crate::adapters::workspace::InMemoryWorkspaceReader;
    for budget in [None, Some(Duration::from_millis(50))] {
        let clock = TestClock::new(Instant::now());
        let provider = TestProvider {
            clock: clock.clone(),
            advance: Duration::from_millis(20),
            calls: AtomicUsize::new(0),
            deadlines: Mutex::new(Vec::new()),
        };
        let index = TestIndex {
            snapshot: SemanticSnapshot {
                generation: GenerationId(1),
                paths: Vec::new(),
                chunks: Vec::new(),
            },
            replacements: AtomicUsize::new(0),
        };
        let workspace = InMemoryWorkspaceReader::new()
            .with_file("a.rs", "alpha")
            .with_file("b.rs", "beta")
            .with_file("c.rs", "gamma");
        let cancellation = TestCancellation::default();
        let app = SemanticApplication::new(
            &provider,
            &index,
            &workspace,
            &clock,
            &cancellation,
            SemanticTimeouts {
                operation: Duration::from_millis(100),
                rebuild: budget,
                provider: Duration::from_millis(25),
            },
        );
        let result = block_on(app.prepare_rebuild(
            &ChunkingConfig {
                algorithm_version: "v1".into(),
                target_bytes: 64,
                overlap_bytes: 0,
                max_file_bytes: 4096,
            },
            SemanticOperation::Reindex,
        ));
        if budget.is_none() {
            assert_eq!(
                result.unwrap().len(),
                3,
                "total rebuild may exceed one provider budget"
            );
            assert!(
                provider
                    .deadlines
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|d| d.layer() == &DeadlineLayer::Provider && d.budget_ms() == 25)
            );
        } else {
            assert_eq!(
                result,
                Err(SemanticApplicationError::Operation(
                    SemanticOperationError::TimedOut {
                        operation: SemanticOperation::Reindex,
                        layer: DeadlineLayer::Semantic,
                        budget_ms: 50
                    }
                ))
            );
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            index.replacements.load(Ordering::SeqCst),
            0,
            "preparation never publishes partial data"
        );
    }
}
