use std::time::{Duration, Instant};

use crate::{
    ChunkingConfig, DeadlineLayer, EmbeddingProviderError, EmbeddingProviderPort, EmbeddingRequest,
    EmbeddingVector, IndexedChunk, IndexedEmbedding, OperationDeadline, PathReplacement,
    PathRevision, RankedHit, RelatedFile, SemanticDomainError, SemanticIndexError,
    SemanticIndexPort, SemanticOperation, SemanticOperationError, SemanticSearchRequest,
    TaskContext, WorkspacePath, WorkspaceReadError, WorkspaceReadPort,
    chunking::chunk_document,
    retrieval::{
        context_for_task_with_check, related_files_with_check, semantic_search_with_check,
    },
};

pub trait SemanticClock: Send + Sync {
    fn now(&self) -> Instant;
}

pub trait SemanticCancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticTimeouts {
    pub operation: Duration,
    pub rebuild: Option<Duration>,
    pub provider: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticApplicationError {
    Operation(SemanticOperationError),
    Provider(EmbeddingProviderError),
    Index(SemanticIndexError),
    Workspace(WorkspaceReadError),
    Domain(SemanticDomainError),
}

pub struct SemanticApplication<'a> {
    provider: &'a dyn EmbeddingProviderPort,
    index: &'a dyn SemanticIndexPort,
    workspace: &'a dyn WorkspaceReadPort,
    clock: &'a dyn SemanticClock,
    cancellation: &'a dyn SemanticCancellation,
    timeouts: SemanticTimeouts,
}

impl<'a> SemanticApplication<'a> {
    pub fn new(
        provider: &'a dyn EmbeddingProviderPort,
        index: &'a dyn SemanticIndexPort,
        workspace: &'a dyn WorkspaceReadPort,
        clock: &'a dyn SemanticClock,
        cancellation: &'a dyn SemanticCancellation,
        timeouts: SemanticTimeouts,
    ) -> Self {
        Self {
            provider,
            index,
            workspace,
            clock,
            cancellation,
            timeouts,
        }
    }

    pub async fn search(
        &self,
        request: &SemanticSearchRequest,
    ) -> Result<Vec<RankedHit>, SemanticApplicationError> {
        self.query_hits(request, SemanticOperation::Search).await
    }

    pub async fn context_candidates(
        &self,
        request: &SemanticSearchRequest,
    ) -> Result<Vec<RankedHit>, SemanticApplicationError> {
        self.query_hits(request, SemanticOperation::ContextForTask)
            .await
    }

    async fn query_hits(
        &self,
        request: &SemanticSearchRequest,
        kind: SemanticOperation,
    ) -> Result<Vec<RankedHit>, SemanticApplicationError> {
        let operation = self.operation_deadline(kind, Some(self.timeouts.operation));
        self.check_active(&operation)?;
        let snapshot = self
            .index
            .snapshot()
            .map_err(SemanticApplicationError::Index)?;
        let vectors = self
            .embed_once(vec![request.query_text.clone()], &operation)
            .await?;
        self.check_active(&operation)?;
        semantic_search_with_check(&snapshot, &vectors[0], request, || {
            self.check_active(&operation)
        })
    }

    pub async fn related_files(
        &self,
        source_path: &WorkspacePath,
        request: &SemanticSearchRequest,
        chunking: &ChunkingConfig,
    ) -> Result<Vec<RelatedFile>, SemanticApplicationError> {
        let operation = self.operation_deadline(
            SemanticOperation::RelatedFiles,
            Some(self.timeouts.operation),
        );
        self.check_active(&operation)?;
        let mut document = self
            .workspace
            .read_file(source_path)
            .await
            .map_err(SemanticApplicationError::Workspace)?;
        if let Some(spans) = self
            .workspace
            .symbol_spans(source_path)
            .await
            .map_err(SemanticApplicationError::Workspace)?
        {
            document.symbol_spans = spans;
        }
        let chunks =
            chunk_document(&document, chunking).map_err(SemanticApplicationError::Domain)?;
        let snapshot = self
            .index
            .snapshot()
            .map_err(SemanticApplicationError::Index)?;
        let mut hits = Vec::new();
        for chunk in chunks {
            let vectors = self
                .embed_once(vec![chunk.snippet.as_str().to_owned()], &operation)
                .await?;
            hits.extend(semantic_search_with_check(
                &snapshot,
                &vectors[0],
                request,
                || self.check_active(&operation),
            )?);
        }
        self.check_active(&operation)?;
        related_files_with_check(&hits, request.limit, || self.check_active(&operation))
    }

    pub async fn context_for_task(
        &self,
        request: &SemanticSearchRequest,
        token_budget: u32,
    ) -> Result<TaskContext, SemanticApplicationError> {
        self.context_for_task_with_hits(request, token_budget, request.limit)
            .await
            .map(|(_, context)| context)
    }

    pub async fn context_for_task_with_hits(
        &self,
        request: &SemanticSearchRequest,
        token_budget: u32,
        limit: usize,
    ) -> Result<(Vec<RankedHit>, TaskContext), SemanticApplicationError> {
        let operation = self.operation_deadline(
            SemanticOperation::ContextForTask,
            Some(self.timeouts.operation),
        );
        self.check_active(&operation)?;
        let snapshot = self
            .index
            .snapshot()
            .map_err(SemanticApplicationError::Index)?;
        let vectors = self
            .embed_once(vec![request.query_text.clone()], &operation)
            .await?;
        self.check_active(&operation)?;
        let hits = semantic_search_with_check(&snapshot, &vectors[0], request, || {
            self.check_active(&operation)
        })?;
        let context = context_for_task_with_check(&hits, token_budget, limit, || {
            self.check_active(&operation)
        })?;
        self.check_active(&operation)?;
        Ok((hits, context))
    }

    pub async fn replace_path(
        &self,
        path: &crate::WorkspacePath,
        revision: PathRevision,
        chunking: &ChunkingConfig,
    ) -> Result<crate::RevisionOutcome, SemanticApplicationError> {
        let operation =
            self.operation_deadline(SemanticOperation::FileUpdate, self.timeouts.rebuild);
        self.check_active(&operation)?;
        let document = self
            .workspace
            .read_file(path)
            .await
            .map_err(SemanticApplicationError::Workspace)?;
        let chunks =
            chunk_document(&document, chunking).map_err(SemanticApplicationError::Domain)?;
        let inputs = chunks
            .iter()
            .map(|chunk| chunk.snippet.as_str().to_owned())
            .collect();
        let vectors = self.embed_once(inputs, &operation).await?;
        self.check_active(&operation)?;
        self.index
            .replace_path(PathReplacement {
                path: path.clone(),
                revision,
                indexed_chunks: chunks
                    .into_iter()
                    .zip(vectors)
                    .map(|(chunk, embedding)| IndexedChunk { chunk, embedding })
                    .collect(),
            })
            .map_err(SemanticApplicationError::Index)
    }

    /// Build a path replacement under an enclosing epoch or rebuild deadline.
    pub async fn prepare_path(
        &self,
        path: &WorkspacePath,
        revision: PathRevision,
        chunking: &ChunkingConfig,
        operation: &OperationDeadline,
    ) -> Result<PathReplacement, SemanticApplicationError> {
        self.check_active(operation)?;
        let document = self
            .workspace
            .read_file(path)
            .await
            .map_err(SemanticApplicationError::Workspace)?;
        let chunks =
            chunk_document(&document, chunking).map_err(SemanticApplicationError::Domain)?;
        let mut indexed_chunks = Vec::new();
        for chunk in chunks {
            let vectors = self
                .embed_once(vec![chunk.snippet.as_str().to_owned()], operation)
                .await?;
            indexed_chunks.push(IndexedChunk {
                chunk,
                embedding: vectors[0].clone(),
            });
        }
        self.check_active(operation)?;
        Ok(PathReplacement {
            path: path.clone(),
            revision,
            indexed_chunks,
        })
    }

    /// Prepare a complete replacement under one cumulative rebuild deadline.
    /// The caller publishes these paths atomically only after this succeeds.
    pub async fn prepare_rebuild(
        &self,
        chunking: &ChunkingConfig,
        kind: SemanticOperation,
    ) -> Result<Vec<PathReplacement>, SemanticApplicationError> {
        let operation = self.operation_deadline(kind, self.timeouts.rebuild);
        self.check_active(&operation)?;
        let paths = self
            .workspace
            .list_files()
            .await
            .map_err(SemanticApplicationError::Workspace)?;
        let mut replacements = Vec::new();
        for path in paths {
            match self
                .prepare_path(&path, PathRevision(1), chunking, &operation)
                .await
            {
                Ok(replacement) => replacements.push(replacement),
                Err(SemanticApplicationError::Workspace(
                    WorkspaceReadError::NotFound
                    | WorkspaceReadError::UnsupportedFileType
                    | WorkspaceReadError::InvalidUtf8,
                ))
                | Err(SemanticApplicationError::Domain(SemanticDomainError::DocumentTooLarge {
                    ..
                })) => continue,
                Err(error) => return Err(error),
            }
        }
        self.check_active(&operation)?;
        Ok(replacements)
    }

    async fn embed_once(
        &self,
        inputs: Vec<String>,
        operation: &OperationDeadline,
    ) -> Result<Vec<EmbeddingVector>, SemanticApplicationError> {
        let provider_deadline = self.provider_deadline(operation);
        self.embed_with_deadline(inputs, &provider_deadline).await
    }

    fn provider_deadline(&self, operation: &OperationDeadline) -> OperationDeadline {
        let provider_deadline = OperationDeadline::for_provider(
            self.clock.now() + self.timeouts.provider,
            SemanticOperation::Embedding,
            duration_ms(self.timeouts.provider),
        );
        OperationDeadline::select_earlier(operation.clone(), provider_deadline)
    }

    async fn embed_with_deadline(
        &self,
        inputs: Vec<String>,
        deadline: &OperationDeadline,
    ) -> Result<Vec<EmbeddingVector>, SemanticApplicationError> {
        self.check_active(deadline)?;
        let batch_len = inputs.len();
        let response = self
            .provider
            .embed(&EmbeddingRequest { inputs }, deadline.clone())
            .await
            .map_err(|error| match error {
                EmbeddingProviderError::TimedOut {
                    operation,
                    layer: DeadlineLayer::Semantic,
                    budget_ms,
                } => SemanticApplicationError::Operation(SemanticOperationError::TimedOut {
                    operation,
                    layer: DeadlineLayer::Semantic,
                    budget_ms,
                }),
                error => SemanticApplicationError::Provider(error),
            })?;
        self.check_active(deadline)?;
        validate_embedding_batch(response, batch_len, self.provider.identity().dimensions)
            .map_err(SemanticApplicationError::Provider)
    }

    fn operation_deadline(
        &self,
        operation: SemanticOperation,
        budget: Option<Duration>,
    ) -> OperationDeadline {
        OperationDeadline::for_operation(
            budget.map(|duration| self.clock.now() + duration),
            operation,
            budget.map(duration_ms),
        )
    }

    fn check_active(&self, deadline: &OperationDeadline) -> Result<(), SemanticApplicationError> {
        if self.cancellation.is_cancelled() {
            return Err(SemanticApplicationError::Operation(
                SemanticOperationError::Cancelled,
            ));
        }
        if deadline
            .deadline()
            .is_some_and(|expires| self.clock.now() >= expires)
        {
            let error = match deadline.layer() {
                DeadlineLayer::Semantic => {
                    SemanticApplicationError::Operation(SemanticOperationError::TimedOut {
                        operation: deadline.operation().clone(),
                        layer: DeadlineLayer::Semantic,
                        budget_ms: deadline.budget_ms(),
                    })
                }
                DeadlineLayer::Provider => {
                    SemanticApplicationError::Provider(EmbeddingProviderError::TimedOut {
                        operation: deadline.operation().clone(),
                        layer: DeadlineLayer::Provider,
                        budget_ms: deadline.budget_ms(),
                    })
                }
            };
            return Err(error);
        }
        Ok(())
    }
}

impl From<SemanticDomainError> for SemanticApplicationError {
    fn from(error: SemanticDomainError) -> Self {
        Self::Domain(error)
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Application-level results which the protocol boundary can render without
/// exposing provider, index, or workspace implementation details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicSemanticOutcome {
    Success,
    ProviderNotConfigured,
    Provider(EmbeddingProviderError),
    Index(SemanticIndexError),
    Operation(SemanticOperationError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicSemanticResponse {
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub code: Option<&'static str>,
    pub message: &'static str,
    pub timeout: Option<PublicTimeoutDetails>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicTimeoutDetails {
    pub operation: SemanticOperation,
    pub layer: DeadlineLayer,
    pub budget_ms: u64,
}

pub fn map_public_outcome(outcome: PublicSemanticOutcome) -> PublicSemanticResponse {
    let response = |status, reason, code, message, timeout| PublicSemanticResponse {
        status,
        reason,
        code,
        message,
        timeout,
    };
    match outcome {
        PublicSemanticOutcome::Success => {
            response("ok", None, None, "Semantic operation completed", None)
        }
        PublicSemanticOutcome::ProviderNotConfigured => response(
            "unsupported",
            Some("provider_not_configured"),
            None,
            "Semantic provider is not configured",
            None,
        ),
        PublicSemanticOutcome::Index(SemanticIndexError::Incompatible(_)) => response(
            "reindex_required",
            Some("incompatible_index"),
            None,
            "Semantic index is incompatible",
            None,
        ),
        PublicSemanticOutcome::Index(SemanticIndexError::Corrupt) => response(
            "reindex_required",
            Some("corrupt_index"),
            None,
            "Semantic index is corrupt",
            None,
        ),
        PublicSemanticOutcome::Index(SemanticIndexError::UnsupportedSchema) => response(
            "reindex_required",
            Some("unsupported_schema"),
            None,
            "Semantic index schema is unsupported",
            None,
        ),
        PublicSemanticOutcome::Provider(EmbeddingProviderError::Unavailable) => response(
            "error",
            None,
            Some("provider_unavailable"),
            "Semantic provider is unavailable",
            None,
        ),
        PublicSemanticOutcome::Provider(EmbeddingProviderError::TimedOut {
            operation,
            layer,
            budget_ms,
        }) => response(
            "error",
            None,
            Some("provider_timeout"),
            "Semantic provider timed out",
            Some(PublicTimeoutDetails {
                operation,
                layer,
                budget_ms,
            }),
        ),
        PublicSemanticOutcome::Provider(EmbeddingProviderError::DimensionMismatch { .. }) => {
            response(
                "error",
                None,
                Some("provider_dimension_mismatch"),
                "Semantic provider dimensions do not match",
                None,
            )
        }
        PublicSemanticOutcome::Provider(EmbeddingProviderError::Cancelled)
        | PublicSemanticOutcome::Operation(SemanticOperationError::Cancelled) => response(
            "error",
            None,
            Some("cancelled"),
            "Semantic operation was cancelled",
            None,
        ),
        PublicSemanticOutcome::Provider(_) => response(
            "error",
            None,
            Some("provider_invalid_response"),
            "Semantic provider returned an invalid response",
            None,
        ),
        PublicSemanticOutcome::Operation(SemanticOperationError::TimedOut {
            operation,
            layer,
            budget_ms,
        }) => response(
            "error",
            None,
            Some("operation_timeout"),
            "Semantic operation timed out",
            Some(PublicTimeoutDetails {
                operation,
                layer,
                budget_ms,
            }),
        ),
        PublicSemanticOutcome::Index(_) => response(
            "error",
            None,
            Some("storage_error"),
            "Semantic index storage failed",
            None,
        ),
    }
}

pub fn validate_embedding_batch(
    batch: Vec<IndexedEmbedding>,
    batch_len: usize,
    expected_dimensions: Option<usize>,
) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
    let mut vectors = Vec::with_capacity(batch_len);
    let mut previous = None;
    let mut dimensions = expected_dimensions;
    let mut seen = vec![false; batch_len];

    for item in &batch {
        if item.index >= batch_len {
            return Err(EmbeddingProviderError::OutOfRangeIndex {
                index: item.index,
                batch_len,
            });
        }
        if seen[item.index] {
            return Err(EmbeddingProviderError::DuplicateIndex { index: item.index });
        }
        seen[item.index] = true;
        if let Some(previous) = previous
            && item.index < previous
        {
            return Err(EmbeddingProviderError::UnorderedIndex {
                previous,
                current: item.index,
            });
        }
        if item.vector.values.iter().any(|value| !value.is_finite()) {
            return Err(EmbeddingProviderError::NonFiniteVector { index: item.index });
        }
        if item.vector.values.iter().all(|value| *value == 0.0) {
            return Err(EmbeddingProviderError::ZeroNormVector { index: item.index });
        }
        let expected = dimensions.get_or_insert(item.vector.values.len());
        if item.vector.values.len() != *expected {
            return Err(EmbeddingProviderError::DimensionMismatch {
                expected: *expected,
                actual: item.vector.values.len(),
            });
        }
        previous = Some(item.index);
        vectors.push(item.vector.clone());
    }
    for expected in 0..vectors.len() {
        if batch.get(expected).map(|entry| entry.index) != Some(expected) {
            return Err(EmbeddingProviderError::MissingIndex { index: expected });
        }
    }
    if vectors.len() != batch_len {
        return Err(EmbeddingProviderError::MissingIndex {
            index: vectors.len(),
        });
    }
    Ok(vectors)
}
