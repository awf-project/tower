//! Production composition of the semantic ports at the sidecar boundary.
use crate::protocol::{
    SemanticContextForTaskRequest, SemanticReindexRequest, SemanticRelatedFilesRequest,
    SemanticSearchRequest as SearchDto,
};
use semantic_context_extension::{
    adapters::{
        embedding::{OpenAiCompatibleEmbeddingProvider, ValidatedEmbeddingConfig},
        sqlite::SqliteSemanticIndex,
        workspace::{HostCallWorkspaceReadAdapter, WorkspaceHostCallTransport},
    },
    application::{
        PublicSemanticOutcome, SemanticApplication, SemanticApplicationError, SemanticCancellation,
        SemanticClock, SemanticTimeouts, map_public_outcome,
    },
    *,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Config {
    enabled: bool,
    embedding: EmbeddingConfig,
    timeouts: Timeouts,
    chunking: Chunking,
    store: Store,
}
#[derive(Deserialize)]
struct EmbeddingConfig {
    base_url: String,
    model: String,
    timeout_ms: u64,
    batch_size: usize,
    dimensions: Option<usize>,
    #[serde(default)]
    allow_remote: bool,
    api_key_env: Option<String>,
}
#[derive(Deserialize)]
struct Timeouts {
    search_ms: u64,
    reindex_ms: Option<u64>,
}
#[derive(Deserialize)]
struct Chunking {
    target_bytes: usize,
    overlap_bytes: usize,
    max_file_bytes: usize,
}
#[derive(Deserialize)]
struct Store {
    backend: String,
    path: String,
}
struct Clock {
    control: Option<PathBuf>,
}
impl Clock {
    fn production() -> Self {
        Self {
            control: std::env::var_os("TOWER_SEMANTIC_TEST_CLOCK").map(PathBuf::from),
        }
    }
}
impl SemanticClock for Clock {
    fn now(&self) -> Instant {
        match &self.control {
            Some(path) => {
                let offset_ms = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .unwrap_or(0);
                // HTTP deadlines use real Instants too. Advance that clock
                // rather than freezing it at startup: otherwise short provider
                // budgets expire before a request even starts on a busy host.
                Instant::now() + Duration::from_millis(offset_ms)
            }
            None => Instant::now(),
        }
    }
}

struct Cancellation;
impl SemanticCancellation for Cancellation {
    fn is_cancelled(&self) -> bool {
        false
    }
}

pub struct Runtime {
    provider: Option<OpenAiCompatibleEmbeddingProvider>,
    index: Option<SqliteSemanticIndex>,
    workspace: HostCallWorkspaceReadAdapter,
    chunking: ChunkingConfig,
    timeouts: SemanticTimeouts,
    processed: u64,
    revision: u64,
    reconciled: bool,
    pending_error: Option<Value>,
    staged: BTreeMap<WorkspacePath, PathReplacement>,
    epoch: Option<OperationDeadline>,
    catch_up_retry_required: bool,
    index_fault: bool,
    clock: Clock,
    observer: Option<Arc<dyn WorkspaceHostCallTransport>>,
}

struct RuntimeComponents {
    provider: Option<OpenAiCompatibleEmbeddingProvider>,
    index: Option<SqliteSemanticIndex>,
    chunking: ChunkingConfig,
    timeouts: SemanticTimeouts,
}

fn configure_runtime(
    config: Config,
    transport: &Arc<dyn WorkspaceHostCallTransport>,
) -> Result<RuntimeComponents, ()> {
    if config.timeouts.search_ms == 0
        || config.timeouts.reindex_ms == Some(0)
        || config.chunking.target_bytes < 4
        || config.chunking.overlap_bytes >= config.chunking.target_bytes
        || config.chunking.max_file_bytes < config.chunking.target_bytes
    {
        return Err(());
    }
    let chunking = ChunkingConfig {
        algorithm_version: "v1".into(),
        target_bytes: config.chunking.target_bytes,
        overlap_bytes: config.chunking.overlap_bytes,
        max_file_bytes: config.chunking.max_file_bytes,
    };
    let timeouts = SemanticTimeouts {
        operation: Duration::from_millis(config.timeouts.search_ms),
        rebuild: config.timeouts.reindex_ms.map(Duration::from_millis),
        provider: Duration::from_millis(config.embedding.timeout_ms),
    };
    if !config.enabled {
        return Ok(RuntimeComponents {
            provider: None,
            index: None,
            chunking,
            timeouts,
        });
    }

    let log_transport = Arc::clone(transport);
    let provider = OpenAiCompatibleEmbeddingProvider::new(ValidatedEmbeddingConfig {
        base_url: config.embedding.base_url,
        model: config.embedding.model,
        timeout_ms: config.embedding.timeout_ms,
        batch_size: config.embedding.batch_size,
        dimensions: config.embedding.dimensions,
        allow_remote: config.embedding.allow_remote,
        api_key_env: config.embedding.api_key_env,
    })
    .map_err(|_| ())?
    .with_log_host_call_sink(move |call| {
        if let Ok(params) = serde_json::to_value(call) {
            let _ = log_transport.call("log", params);
        }
    });
    if config.store.backend != "sqlite"
        || config.store.path != ".tower/semantic_context/index.sqlite3"
    {
        return Err(());
    }
    // The host validates confinement before spawn; check it again at the
    // only direct filesystem boundary before opening the derived store.
    let root = std::env::current_dir()
        .map_err(|_| ())?
        .canonicalize()
        .map_err(|_| ())?;
    let directory = root.join(".tower/semantic_context");
    std::fs::create_dir_all(&directory).map_err(|_| ())?;
    let directory = directory.canonicalize().map_err(|_| ())?;
    if !directory.starts_with(&root) {
        return Err(());
    }
    let path = directory.join("index.sqlite3");
    if path.exists() && !path.canonicalize().map_err(|_| ())?.starts_with(&root) {
        return Err(());
    }
    let compatibility = IndexCompatibility {
        schema_version: 1,
        chunking_fingerprint: ChunkingFingerprint {
            algorithm_version: chunking.algorithm_version.clone(),
            target_bytes: chunking.target_bytes as u64,
            overlap_bytes: chunking.overlap_bytes as u64,
            max_file_bytes: chunking.max_file_bytes as u64,
        },
        embedding_space: provider.identity(),
        dimensions: config.embedding.dimensions,
    };
    let index = SqliteSemanticIndex::open(path, compatibility).map_err(|_| ())?;
    Ok(RuntimeComponents {
        provider: Some(provider),
        index: Some(index),
        chunking,
        timeouts,
    })
}

impl Runtime {
    pub async fn new(
        config: Value,
        transport: Arc<dyn WorkspaceHostCallTransport>,
    ) -> Result<Self, ()> {
        let config: Config = serde_json::from_value(config).map_err(|_| ())?;
        let components = configure_runtime(config, &transport)?;
        Ok(Self {
            provider: components.provider,
            index: components.index,
            workspace: HostCallWorkspaceReadAdapter::new(Arc::clone(&transport)),
            chunking: components.chunking,
            timeouts: components.timeouts,
            processed: 0,
            revision: 0,
            reconciled: false,
            pending_error: None,
            staged: BTreeMap::new(),
            epoch: None,
            catch_up_retry_required: false,
            index_fault: false,
            clock: Clock::production(),
            observer: (std::env::var_os("TOWER_SEMANTIC_TEST_OBSERVER").is_some())
                .then_some(transport),
        })
    }
    fn observe(&self, marker: &str) {
        let Some(transport) = &self.observer else {
            return;
        };
        // The optional test observer intentionally carries only fixed lifecycle
        // markers and a counter: no workspace paths or document content cross
        // the log HostCall boundary.
        let _ = transport.call(
            "log",
            json!({"level":"debug","message":format!("semantic-test-observer {marker} processed={}", self.processed)}),
        );
    }
    fn app(&self) -> SemanticApplication<'_> {
        SemanticApplication::new(
            self.provider.as_ref().expect("enabled"),
            self.index.as_ref().expect("enabled"),
            &self.workspace,
            &self.clock,
            &Cancellation,
            self.timeouts,
        )
    }
    fn recover_index_fault(&mut self) -> Result<(), SemanticApplicationError> {
        if !self.index_fault {
            return Ok(());
        }
        self.index
            .as_ref()
            .expect("enabled")
            .abort_generation()
            .map_err(SemanticApplicationError::Index)?;
        self.index_fault = false;
        Ok(())
    }
    fn generation(&self) -> u64 {
        self.index
            .as_ref()
            .and_then(|index| index.snapshot().ok())
            .map_or(0, |snapshot| snapshot.generation.0)
    }
    pub async fn event(&mut self, event: Value) {
        self.processed = self.processed.saturating_add(1);
        self.observe(if self.reconciled {
            "live-event-delivered"
        } else {
            "snapshot-event-delivered"
        });
        if self.provider.is_none() {
            return;
        }
        let Some(path) = event["path"]
            .as_str()
            .and_then(|s| WorkspacePath::new(s.to_owned()).ok())
        else {
            return;
        };
        self.revision += 1;
        // Snapshot events are reconciled together at the first ordering fence.
        // Live changes use the same application and durable path mutations.
        if !self.reconciled {
            let epoch = self
                .epoch
                .get_or_insert_with(|| {
                    OperationDeadline::for_operation(
                        self.timeouts
                            .rebuild
                            .map(|budget| self.clock.now() + budget),
                        SemanticOperation::CatchUp,
                        self.timeouts
                            .rebuild
                            .map(|budget| budget.as_millis() as u64),
                    )
                })
                .clone();
            if event["type"] == "FileDeleted" {
                self.staged.remove(&path);
                return;
            }
            match self
                .app()
                .prepare_path(&path, PathRevision(self.revision), &self.chunking, &epoch)
                .await
            {
                Ok(replacement) => {
                    self.staged.insert(path, replacement);
                }
                Err(SemanticApplicationError::Workspace(
                    WorkspaceReadError::NotFound
                    | WorkspaceReadError::UnsupportedFileType
                    | WorkspaceReadError::InvalidUtf8,
                ))
                | Err(SemanticApplicationError::Domain(SemanticDomainError::DocumentTooLarge {
                    ..
                })) => {
                    self.staged.remove(&path);
                }
                Err(error) => {
                    self.pending_error = Some(public_error(error));
                }
            }
            return;
        }
        if let Err(error) = self.recover_index_fault() {
            self.pending_error = Some(public_error(error));
            return;
        }
        let result = if event["type"] == "FileDeleted" {
            self.index
                .as_ref()
                .expect("enabled")
                .delete_path(&path, PathRevision(self.revision))
                .map_err(SemanticApplicationError::Index)
        } else {
            self.app()
                .replace_path(&path, PathRevision(self.revision), &self.chunking)
                .await
        };
        if let Err(error) = result {
            self.pending_error = Some(public_error(error));
        }
    }
    async fn rebuild(
        &mut self,
        operation: SemanticOperation,
    ) -> Result<Value, SemanticApplicationError> {
        // Prepare through domain ports before touching SQLite's shadow generation.
        // Failed provider/deadline work cannot publish or divert later live writes.
        let replacements = self
            .app()
            .prepare_rebuild(&self.chunking, operation)
            .await?;
        self.publish(replacements)
    }
    fn publish(
        &mut self,
        replacements: Vec<PathReplacement>,
    ) -> Result<Value, SemanticApplicationError> {
        let index = self.index.as_ref().expect("enabled");
        let previous = index.snapshot().map_err(SemanticApplicationError::Index)?;
        let generation = RebuiltGeneration {
            id: GenerationId(previous.generation.0 + 1),
        };
        index
            .begin_generation(generation.clone())
            .map_err(SemanticApplicationError::Index)?;
        let files = replacements.len();
        let chunks: usize = replacements
            .iter()
            .map(|path| path.indexed_chunks.len())
            .sum();
        let commit = (|| {
            for replacement in replacements {
                index.replace_path(replacement)?;
            }
            index.publish_generation(generation)
        })();
        let id = match commit {
            Ok(id) => id,
            Err(error) => {
                if let Err(abort_error) = index.abort_generation() {
                    self.index_fault = true;
                    return Err(SemanticApplicationError::Index(abort_error));
                }
                return Err(SemanticApplicationError::Index(error));
            }
        };
        self.revision = self.revision.max(1);
        Ok(json!({"status":"ok","indexed_files":files,"indexed_chunks":chunks,"generation":id.0}))
    }
    fn request(
        &self,
        text: String,
        limit: u32,
        prefixes: Option<Vec<String>>,
        languages: Option<Vec<String>>,
        lexical: bool,
    ) -> Result<SemanticSearchRequest, ()> {
        if limit > 1000 {
            return Err(());
        }
        let prefixes = prefixes.unwrap_or_default();
        for prefix in &prefixes {
            if !prefix.is_empty() && WorkspacePath::new(prefix.clone()).is_err() {
                return Err(());
            }
        }
        let paths = if prefixes.is_empty() || prefixes.iter().any(String::is_empty) {
            Vec::new()
        } else {
            self.index
                .as_ref()
                .and_then(|i| i.snapshot().ok())
                .map(|s| {
                    s.paths
                        .into_iter()
                        .filter(|p| prefixes.iter().any(|prefix| p.as_str().starts_with(prefix)))
                        .collect()
                })
                .unwrap_or_default()
        };
        // An unmatched prefix must not become an unrestricted empty filter.
        let paths =
            if !prefixes.is_empty() && !prefixes.iter().any(String::is_empty) && paths.is_empty() {
                vec![WorkspacePath::new("__no_matching_semantic_path__".into()).map_err(|_| ())?]
            } else {
                paths
            };
        Ok(SemanticSearchRequest {
            query_text: text,
            limit: limit as usize,
            filters: SemanticFilter {
                paths,
                languages: languages.unwrap_or_default(),
            },
            lexical_reranking: lexical,
        })
    }
    pub async fn dispatch(&mut self, name: &str, params: Value) -> Result<Value, ()> {
        match name {
            "semantic_reindex" => self.dispatch_reindex(params).await,
            "semantic_search" => self.dispatch_search(params).await,
            "semantic_related_files" => self.dispatch_related_files(params).await,
            "semantic_context_for_task" => self.dispatch_context_for_task(params).await,
            "_tower_catch_up_fence" => self.dispatch_catch_up_fence(params).await,
            _ => Err(()),
        }
    }

    async fn dispatch_reindex(&mut self, params: Value) -> Result<Value, ()> {
        serde_json::from_value::<SemanticReindexRequest>(params).map_err(|_| ())?;
        if self.provider.is_none() {
            return Ok(unsupported());
        }
        if let Err(error) = self.recover_index_fault() {
            return Ok(public_error(error));
        }
        Ok(self
            .rebuild(SemanticOperation::Reindex)
            .await
            .unwrap_or_else(public_error))
    }

    async fn dispatch_search(&self, params: Value) -> Result<Value, ()> {
        let q: SearchDto = serde_json::from_value(params).map_err(|_| ())?;
        let request = self.request(
            q.query,
            q.limit,
            q.path_prefixes,
            q.languages,
            q.lexical_rerank.unwrap_or(false),
        )?;
        if self.provider.is_none() {
            return Ok(unsupported());
        }
        if request.query_text.is_empty() || request.limit == 0 {
            return Ok(json!({"status":"ok","hits":[],"truncated":false}));
        }
        Ok(match self.app().search(&request).await {
            Ok(hits) => {
                json!({"status":"ok","hits":hits.iter().map(hit).collect::<Vec<_>>(),"truncated":false})
            }
            Err(error) => public_error(error),
        })
    }

    async fn dispatch_related_files(&self, params: Value) -> Result<Value, ()> {
        let q: SemanticRelatedFilesRequest = serde_json::from_value(params).map_err(|_| ())?;
        let path = WorkspacePath::new(q.path).map_err(|_| ())?;
        let request = self.request(String::new(), q.limit, q.path_prefixes, q.languages, false)?;
        if self.provider.is_none() {
            return Ok(unsupported());
        }
        if request.limit == 0 {
            return Ok(json!({"status":"ok","files":[],"truncated":false}));
        }
        Ok(
            match self
                .app()
                .related_files(&path, &request, &self.chunking)
                .await
            {
                Ok(files) => {
                    json!({"status":"ok","files":files.iter().map(|f| json!({"path":f.path.as_str(),"score":f.score,"reason":"semantic_similarity"})).collect::<Vec<_>>(),"truncated":false})
                }
                Err(error) => public_error(error),
            },
        )
    }

    async fn dispatch_context_for_task(&self, params: Value) -> Result<Value, ()> {
        let q: SemanticContextForTaskRequest = serde_json::from_value(params).map_err(|_| ())?;
        if q.token_budget > 1_000_000 {
            return Err(());
        }
        let mut request = self.request(q.task, q.limit, q.path_prefixes, q.languages, false)?;
        if self.provider.is_none() {
            return Ok(unsupported());
        }
        if request.query_text.is_empty() || q.token_budget == 0 || q.limit == 0 {
            return Ok(self.empty_task_context(&request));
        }
        request.limit = usize::MAX;
        Ok(
            match self
                .app()
                .context_for_task_with_hits(&request, q.token_budget, q.limit as usize)
                .await
            {
                Ok((hits, context)) => {
                    let chunks: Vec<_> = context
                        .chunks
                        .iter()
                        .filter_map(|chunk| {
                            hits.iter()
                                .find(|hit| hit.chunk.chunk_hash == chunk.chunk_hash)
                        })
                        .map(hit)
                        .collect();
                    json!({"status":"ok","chunks":chunks,"estimated_tokens":context.tokens_used,"truncated":context.truncated,"eligible_unfitted":context.eligible_unfitted})
                }
                Err(error) => public_error(error),
            },
        )
    }

    fn empty_task_context(&self, request: &SemanticSearchRequest) -> Value {
        match self.index.as_ref().expect("enabled").snapshot() {
            Ok(snapshot) => {
                let eligible = snapshot
                    .chunks
                    .iter()
                    .filter(|indexed| {
                        (request.filters.paths.is_empty()
                            || request.filters.paths.contains(&indexed.chunk.path))
                            && (request.filters.languages.is_empty()
                                || request.filters.languages.contains(&indexed.chunk.language))
                    })
                    .count();
                json!({"status":"ok","chunks":[],"estimated_tokens":0,"truncated":eligible > 0,"eligible_unfitted":eligible})
            }
            Err(error) => public_error(SemanticApplicationError::Index(error)),
        }
    }

    async fn dispatch_catch_up_fence(&mut self, params: Value) -> Result<Value, ()> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fence {
            epoch: u64,
            delivered_events: u64,
        }
        let fence: Fence = serde_json::from_value(params).map_err(|_| ())?;
        if fence.delivered_events != self.processed {
            return Ok(
                json!({"epoch":fence.epoch,"processed_events":self.processed,"ready":false,"generation":self.generation()}),
            );
        }
        if self.provider.is_some() && !self.reconciled {
            if let Err(error) = self.recover_index_fault() {
                return Ok(self.catch_up_failure(error));
            }
            if let Some(error) = self.pending_error.clone() {
                return Ok(error);
            }
            if let Some(epoch) = &self.epoch
                && epoch
                    .deadline()
                    .is_some_and(|deadline| self.clock.now() >= deadline)
            {
                let budget_ms = epoch.budget_ms();
                self.epoch = None;
                self.staged.clear();
                self.pending_error = None;
                self.catch_up_retry_required = true;
                let error = SemanticApplicationError::Operation(SemanticOperationError::TimedOut {
                    operation: SemanticOperation::CatchUp,
                    layer: DeadlineLayer::Semantic,
                    budget_ms,
                });
                return Ok(self.catch_up_failure(error));
            }
            if self.catch_up_retry_required {
                if let Err(error) = self.rebuild(SemanticOperation::CatchUp).await {
                    return Ok(self.catch_up_failure(error));
                }
                self.catch_up_retry_required = false;
            } else {
                let replacements = self.staged.values().cloned().collect();
                if let Err(error) = self.publish(replacements) {
                    return Ok(self.catch_up_failure(error));
                }
            }
            self.epoch = None;
            self.staged.clear();
        }
        if let Some(error) = self.pending_error.take() {
            return Ok(error);
        }
        self.reconciled = true;
        let generation = self
            .index
            .as_ref()
            .and_then(|index| index.snapshot().ok())
            .map_or(0, |snapshot| snapshot.generation.0);
        self.observe("catch-up-fence-complete");
        Ok(
            json!({"epoch":fence.epoch,"processed_events":self.processed,"ready":true,"generation":generation}),
        )
    }

    fn catch_up_failure(&self, error: SemanticApplicationError) -> Value {
        let mut result = public_error(error);
        result["ready"] = json!(false);
        result["generation"] = json!(self.generation());
        result
    }
}

fn unsupported() -> Value {
    json!({"status":"unsupported","reason":"provider_not_configured"})
}
fn hit(hit: &RankedHit) -> Value {
    json!({"path":hit.chunk.path.as_str(),"start_byte":hit.chunk.start_byte,"end_byte":hit.chunk.end_byte,"score":hit.final_score,"language":hit.chunk.language,"snippet":hit.chunk.snippet.as_str(),"reason":"semantic_similarity"})
}
fn public_error(error: SemanticApplicationError) -> Value {
    let outcome = match error {
        SemanticApplicationError::Provider(e) => PublicSemanticOutcome::Provider(e),
        SemanticApplicationError::Index(e) => PublicSemanticOutcome::Index(e),
        SemanticApplicationError::Operation(e) => PublicSemanticOutcome::Operation(e),
        SemanticApplicationError::Workspace(_) | SemanticApplicationError::Domain(_) => {
            PublicSemanticOutcome::Index(SemanticIndexError::Storage)
        }
    };
    let response = map_public_outcome(outcome);
    let mut value = json!({"status":response.status,"message":response.message});
    if let Some(code) = response.code {
        value["code"] = json!(code);
    }
    if let Some(reason) = response.reason {
        value["reason"] = json!(reason);
    }
    if let Some(timeout) = response.timeout {
        let operation = match timeout.operation {
            SemanticOperation::Search => "search",
            SemanticOperation::RelatedFiles => "related_files",
            SemanticOperation::ContextForTask => "context_for_task",
            SemanticOperation::Reindex => "reindex",
            SemanticOperation::CatchUp => "catch_up",
            SemanticOperation::FileUpdate => "file_update",
            SemanticOperation::Embedding => "embedding",
        };
        value["timeout"] = json!({"operation":operation,"layer":match timeout.layer { DeadlineLayer::Semantic => "semantic", DeadlineLayer::Provider => "provider" },"budget_ms":timeout.budget_ms});
    }
    value
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    #[test]
    fn controlled_clock_tracks_real_time_and_applies_the_requested_offset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clock-ms");
        std::fs::write(&path, "0").unwrap();
        let clock = Clock {
            control: Some(path.clone()),
        };
        let first = clock.now();
        std::thread::sleep(Duration::from_millis(20));
        let real_now = Instant::now();
        let later = clock.now();
        assert!(later >= real_now);
        assert!(later > first);

        std::fs::write(&path, "101").unwrap();
        let real_now = Instant::now();
        assert!(clock.now() >= real_now + Duration::from_millis(101));
    }
}
