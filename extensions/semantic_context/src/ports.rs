use std::{future::Future, pin::Pin};

use crate::domain::{
    EmbeddingProviderError, EmbeddingRequest, EmbeddingSpaceIdentity, GenerationId,
    IndexCompatibility, IndexCompatibilityStatus, IndexedEmbedding, OperationDeadline,
    PathReplacement, PathRevision, RebuiltGeneration, RevisionOutcome, SemanticIndexError,
    SemanticSnapshot, SymbolSpan, WorkspaceDocument, WorkspacePath, WorkspaceReadError,
};

pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait EmbeddingProviderPort: Send + Sync {
    fn identity(&self) -> EmbeddingSpaceIdentity;

    fn embed<'a>(
        &'a self,
        request: &'a EmbeddingRequest,
        deadline: OperationDeadline,
    ) -> PortFuture<'a, Result<Vec<IndexedEmbedding>, EmbeddingProviderError>>;
}

pub trait SemanticIndexPort: Send + Sync {
    fn compatibility(
        &self,
        expected: &IndexCompatibility,
    ) -> Result<IndexCompatibilityStatus, SemanticIndexError>;

    fn snapshot(&self) -> Result<SemanticSnapshot, SemanticIndexError>;

    fn replace_path(
        &self,
        replacement: PathReplacement,
    ) -> Result<RevisionOutcome, SemanticIndexError>;

    fn delete_path(
        &self,
        path: &WorkspacePath,
        revision: PathRevision,
    ) -> Result<RevisionOutcome, SemanticIndexError>;

    fn publish_generation(
        &self,
        generation: RebuiltGeneration,
    ) -> Result<GenerationId, SemanticIndexError>;
}

pub trait WorkspaceReadPort: Send + Sync {
    fn list_files<'a>(&'a self) -> PortFuture<'a, Result<Vec<WorkspacePath>, WorkspaceReadError>>;

    fn read_file<'a>(
        &'a self,
        path: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<WorkspaceDocument, WorkspaceReadError>>;

    fn symbol_spans<'a>(
        &'a self,
        path: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<Option<Vec<SymbolSpan>>, WorkspaceReadError>>;
}
