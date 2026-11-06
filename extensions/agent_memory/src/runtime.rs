use std::{
    collections::{HashSet, VecDeque},
    io::{self, BufRead, BufReader},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::{Duration, Instant, SystemTime},
};

use extension_protocol::{
    HostCall, InitParams, InitResult, PROTOCOL_VERSION, Response,
    messages::{
        AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryScanRequest, AgentMemoryScanResult,
        AgentMemoryStorageFailure, AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
    },
};
use extension_sidecar_harness::{
    HostCallIdAllocator, QueuedFrame, frame_from_envelope, host_call, send_error, send_response,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    AgentMemoryLimitsConfig, Clock, EnrichmentProvider, EnrichmentProviderError, EnrichmentRequest,
    EnrichmentResult, IdGenerator, MemoryApplication, MemoryId, MemoryStore, PersistenceMode,
    PrivacyAction, PrivacyFilter, PrivacyRuleConfig, protocol,
};

const HOST_CALL_START_ID: u64 = 10_000;
const MAX_CONFIG_BOUND: u32 = 16_777_216;
const MAX_PROVIDER_TIMEOUT_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Copy)]
pub struct RuntimeError;

type Application =
    MemoryApplication<HostMemoryStore, RuntimeClock, RuntimeIdGenerator, RuntimeProvider>;

pub struct MemoryRuntime {
    application: Arc<Application>,
    enrichment_worker: Option<EnrichmentWorker>,
}

#[derive(Clone)]
pub struct HostMemoryStore {
    transport: Arc<Transport>,
}

impl MemoryRuntime {
    pub fn initialize(params: InitParams) -> Result<InitResult, RuntimeError> {
        validate_init_params(params).map(|_| protocol::initialize_result())
    }

    fn compose(params: InitParams, transport: Arc<Transport>) -> Result<Self, RuntimeError> {
        let config = validate_init_params(params)?;
        let privacy = PrivacyFilter::try_from_configs(
            config
                .privacy
                .rules
                .into_iter()
                .map(PrivacyRuleDto::into_config)
                .collect(),
        )
        .map_err(|_| RuntimeError)?;
        let provider_deadline = Duration::from_millis(
            config
                .provider
                .as_ref()
                .map_or(5_000, |provider| provider.timeout_ms),
        );
        let clock = RuntimeClock::new();
        let provider = config
            .provider
            .map(|provider| RuntimeProvider::configured(&clock, provider.name));
        let provider_enabled = provider.is_some();
        let application = MemoryApplication::new_configured(
            HostMemoryStore {
                transport: Arc::clone(&transport),
            },
            clock,
            RuntimeIdGenerator::default(),
            provider,
            privacy.clone(),
            config.limits.into(),
            config.persistence_mode.into_mode(),
            config.durable_data_unavailable,
            provider_deadline,
        );
        application.initialize_store().map_err(|_| RuntimeError)?;
        // Opt-in producer fixture: exercise privacy before the real logging
        // HostCall without introducing content logging during normal operation.
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("TOWER_TEST_AGENT_MEMORY_LOG_INPUT") {
            let raw = std::fs::read_to_string(path).map_err(|_| RuntimeError)?;
            let msg = privacy
                .apply("log", &raw, MAX_CONFIG_BOUND)
                .map_err(|_| RuntimeError)?;
            let _: bool = HostMemoryStore { transport }
                .call(
                    "host/log",
                    HostCall::Log {
                        level: "info".into(),
                        msg,
                    },
                )
                .map_err(|_| RuntimeError)?;
        }

        let application = Arc::new(application);
        let enrichment_worker = provider_enabled.then(|| EnrichmentWorker::spawn(&application));
        Ok(Self {
            application,
            enrichment_worker,
        })
    }
}

impl MemoryStore for HostMemoryStore {
    fn get(
        &self,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageFailure> {
        self.call(
            "agent_memory/storage/get",
            HostCall::AgentMemoryStorageGet { request },
        )
    }

    fn scan(
        &self,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageFailure> {
        self.call(
            "agent_memory/storage/scan",
            HostCall::AgentMemoryStorageScan { request },
        )
    }

    fn transact(
        &self,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageFailure> {
        self.call(
            "agent_memory/storage/transact",
            HostCall::AgentMemoryStorageTransact { request },
        )
    }
}

pub fn run() -> Result<(), RuntimeError> {
    let transport = Arc::new(Transport::new());
    let mut runtime: Option<MemoryRuntime> = None;
    while let Some(frame) = transport.next_frame()? {
        let QueuedFrame::Request { id, method, params } = frame else {
            continue;
        };
        if let Some(runtime) = &runtime {
            runtime.publish_ready_enrichment();
        }
        match method.as_str() {
            "initialize" => {
                let params =
                    serde_json::from_value::<InitParams>(params).map_err(|_| RuntimeError)?;
                match MemoryRuntime::compose(params, transport.clone()) {
                    Ok(value) => {
                        runtime = Some(value);
                        send_response(
                            &transport.output,
                            &id,
                            &Response::Initialized(protocol::initialize_result()),
                        )
                        .map_err(|_| RuntimeError)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            "invokeTool" => handle_tool(&transport, &runtime, &id, params)?,
            "deliverEvent" => {
                send_response(&transport.output, &id, &Response::Ack).map_err(|_| RuntimeError)?;
            }
            "shutdown" => {
                drop(runtime.take());
                send_response(&transport.output, &id, &Response::Ack).map_err(|_| RuntimeError)?;
                break;
            }
            _ => {
                send_error(&transport.output, &id, -32601, "Method not found")
                    .map_err(|_| RuntimeError)?;
            }
        }
    }
    Ok(())
}

impl HostMemoryStore {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        call: HostCall,
    ) -> Result<T, AgentMemoryStorageFailure> {
        let mut input = self
            .transport
            .input
            .lock()
            .map_err(|_| AgentMemoryStorageFailure::Unavailable)?;
        let Input { lines, ids, queued } = &mut *input;
        let value = host_call(&self.transport.output, lines, ids, method, &call, queued)
            .map_err(|_| AgentMemoryStorageFailure::Unavailable)?;
        serde_json::from_value(value).map_err(|_| AgentMemoryStorageFailure::Unavailable)
    }
}

struct Input {
    lines: io::Lines<BufReader<io::Stdin>>,
    ids: HostCallIdAllocator,
    queued: VecDeque<QueuedFrame>,
}

struct Transport {
    input: Mutex<Input>,
    output: Arc<Mutex<io::Stdout>>,
}

impl Transport {
    fn new() -> Self {
        Self {
            input: Mutex::new(Input {
                lines: BufReader::new(io::stdin()).lines(),
                ids: HostCallIdAllocator::new(HOST_CALL_START_ID),
                queued: VecDeque::new(),
            }),
            output: Arc::new(Mutex::new(io::stdout())),
        }
    }

    fn next_frame(&self) -> Result<Option<QueuedFrame>, RuntimeError> {
        let mut input = self.input.lock().map_err(|_| RuntimeError)?;
        if let Some(frame) = input.queued.pop_front() {
            return Ok(Some(frame));
        }
        loop {
            let Some(line) = input.lines.next() else {
                return Ok(None);
            };
            let line = line.map_err(|_| RuntimeError)?;
            let envelope = match serde_json::from_str::<Value>(&line) {
                Ok(envelope) => envelope,
                Err(_) => continue,
            };
            if let Some(frame) = frame_from_envelope(envelope) {
                return Ok(Some(frame));
            }
        }
    }
}

fn handle_tool(
    transport: &Transport,
    runtime: &Option<MemoryRuntime>,
    id: &Option<Value>,
    params: Value,
) -> Result<(), RuntimeError> {
    let name = params.get("name").and_then(Value::as_str);
    let args = params.get("params").cloned();
    let Some((name, args)) = name.zip(args) else {
        return send_invalid_params(transport, id);
    };
    if protocol::validate_tool_arguments(name, args.clone()).is_err() {
        return send_invalid_params(transport, id);
    }
    let Some(runtime) = runtime else {
        send_error(&transport.output, id, -32600, "Invalid Request").map_err(|_| RuntimeError)?;
        return Ok(());
    };
    match protocol::handle_tool(name, args, runtime.application.as_ref()) {
        Ok(value) => {
            send_response(&transport.output, id, &Response::ToolResult(value.clone()))
                .map_err(|_| RuntimeError)?;
            if name == "memory_remember"
                && value.get("code").and_then(Value::as_str) == Some("ok")
                && let Some(memory_id) = value.get("id").and_then(Value::as_str)
            {
                runtime.schedule_enrichment(memory_id.to_owned());
            }
            Ok(())
        }
        Err(protocol::ToolProtocolError::InvalidParams) => send_invalid_params(transport, id),
        Err(protocol::ToolProtocolError::UnknownTool) => {
            send_error(&transport.output, id, -32601, "Method not found").map_err(|_| RuntimeError)
        }
    }
}

impl MemoryRuntime {
    fn schedule_enrichment(&self, memory_id: String) {
        let Some(worker) = &self.enrichment_worker else {
            return;
        };
        let Ok(id) = MemoryId::try_from(memory_id) else {
            return;
        };
        let Ok(query) = crate::GetQuery::try_new(vec![id], self.application.limits()) else {
            return;
        };
        let Ok(mut result) = self.application.get(query) else {
            return;
        };
        let Some(crate::GetItemResult::Found { record, .. }) = result.items.pop() else {
            return;
        };
        let _ = worker
            .sender
            .as_ref()
            .expect("active worker")
            .try_send(record);
    }

    fn publish_ready_enrichment(&self) {
        if let Some(worker) = &self.enrichment_worker {
            while let Ok(enrichment) = worker.results.try_recv() {
                let _ = self.application.publish_enrichment(enrichment);
            }
        }
    }
}

// Only the protocol thread may issue HostCalls. Workers return provider results
// through a bounded mailbox so shutdown never joins a worker waiting on stdin.
struct EnrichmentWorker {
    sender: Option<SyncSender<crate::MemoryRecord>>,
    results: Receiver<EnrichmentResult>,
    cancelled: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EnrichmentWorker {
    fn spawn(application: &Arc<Application>) -> Self {
        let (sender, receiver) = sync_channel(1);
        let (results_sender, results) = sync_channel(1);
        let application = Arc::clone(application);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let handle = std::thread::spawn(move || {
            while let Ok(record) = receiver.recv() {
                worker_cancelled.store(false, Ordering::Release);
                if worker_stopped.load(Ordering::Acquire) {
                    break;
                }
                if let Ok(Some(enrichment)) =
                    application.run_enrichment(EnrichmentRequest { record }, &worker_cancelled)
                {
                    if worker_cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let _ = results_sender.try_send(enrichment);
                }
            }
        });
        Self {
            sender: Some(sender),
            results,
            cancelled,
            stopped,
            handle: Some(handle),
        }
    }
}

impl Drop for EnrichmentWorker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn send_invalid_params(transport: &Transport, id: &Option<Value>) -> Result<(), RuntimeError> {
    send_error(&transport.output, id, -32602, "Invalid params").map_err(|_| RuntimeError)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentMemoryInitConfig {
    project_key: String,
    persistence_mode: PersistenceModeDto,
    durable_data_unavailable: bool,
    limits: AgentMemoryLimitsConfigDto,
    privacy: PrivacyConfigDto,
    provider: Option<ProviderConfigDto>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistenceModeDto {
    Durable,
    ReadOnly,
    InMemory,
}

impl PersistenceModeDto {
    fn into_mode(self) -> PersistenceMode {
        match self {
            Self::Durable => PersistenceMode::Durable,
            Self::ReadOnly => PersistenceMode::ReadOnly,
            Self::InMemory => PersistenceMode::InMemory,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentMemoryLimitsConfigDto {
    max_title_bytes: u32,
    max_body_bytes: u32,
    max_metadata_items: u32,
    max_metadata_string_bytes: u32,
    max_batch_items: u32,
    default_results: u32,
    max_results: u32,
    default_response_bytes: u32,
    max_response_bytes: u32,
    max_query_bytes: u32,
}

impl AgentMemoryLimitsConfigDto {
    fn validate(&self) -> Result<(), RuntimeError> {
        let values = [
            self.max_title_bytes,
            self.max_body_bytes,
            self.max_metadata_items,
            self.max_metadata_string_bytes,
            self.max_batch_items,
            self.default_results,
            self.max_results,
            self.default_response_bytes,
            self.max_response_bytes,
            self.max_query_bytes,
        ];
        if values
            .iter()
            .any(|value| !(1..=MAX_CONFIG_BOUND).contains(value))
            || self.default_results > self.max_results
            || self.max_results > self.max_batch_items
            || self.default_response_bytes > self.max_response_bytes
        {
            return Err(RuntimeError);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivacyConfigDto {
    rules: Vec<PrivacyRuleDto>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivacyRuleDto {
    id: String,
    pattern: String,
    action: PrivacyActionDto,
    replacement: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PrivacyActionDto {
    Reject,
    Redact,
}

impl PrivacyRuleDto {
    fn validate(&self) -> Result<(), RuntimeError> {
        if self.id.is_empty() || self.id.len() > 128 {
            return Err(RuntimeError);
        }
        match (&self.action, &self.replacement) {
            (PrivacyActionDto::Reject, None) | (PrivacyActionDto::Redact, Some(_)) => Ok(()),
            _ => Err(RuntimeError),
        }
    }

    fn into_config(self) -> PrivacyRuleConfig {
        let action = match self.action {
            PrivacyActionDto::Reject => PrivacyAction::Reject,
            PrivacyActionDto::Redact => PrivacyAction::Redact {
                replacement: self.replacement.unwrap_or_default(),
            },
        };
        PrivacyRuleConfig {
            id: self.id,
            pattern: self.pattern,
            action,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfigDto {
    name: String,
    #[serde(default = "default_provider_timeout_ms")]
    timeout_ms: u64,
}

fn default_provider_timeout_ms() -> u64 {
    5_000
}

fn validate_init_params(params: InitParams) -> Result<AgentMemoryInitConfig, RuntimeError> {
    if params.protocol_version != PROTOCOL_VERSION {
        return Err(RuntimeError);
    }
    let config: AgentMemoryInitConfig =
        serde_json::from_value(params.extension_config.ok_or(RuntimeError)?)
            .map_err(|_| RuntimeError)?;
    if config.project_key.is_empty() {
        return Err(RuntimeError);
    }
    config.limits.validate()?;
    let mut rule_ids = HashSet::new();
    for rule in &config.privacy.rules {
        rule.validate()?;
        if !rule_ids.insert(&rule.id) {
            return Err(RuntimeError);
        }
    }
    if let Some(provider) = &config.provider
        && (provider.name.is_empty()
            || provider.name.len() > 128
            || !(1..=MAX_PROVIDER_TIMEOUT_MS).contains(&provider.timeout_ms))
    {
        return Err(RuntimeError);
    }
    Ok(config)
}

impl From<AgentMemoryLimitsConfigDto> for AgentMemoryLimitsConfig {
    fn from(value: AgentMemoryLimitsConfigDto) -> Self {
        Self {
            max_title_bytes: value.max_title_bytes,
            max_body_bytes: value.max_body_bytes,
            max_metadata_items: value.max_metadata_items,
            max_metadata_string_bytes: value.max_metadata_string_bytes,
            max_batch_items: value.max_batch_items,
            default_results: value.default_results,
            max_results: value.max_results,
            default_response_bytes: value.default_response_bytes,
            max_response_bytes: value.max_response_bytes,
            max_query_bytes: value.max_query_bytes,
        }
    }
}

#[derive(Clone)]
struct RuntimeClock {
    started: Arc<Instant>,
}
impl RuntimeClock {
    fn new() -> Self {
        Self {
            started: Arc::new(Instant::now()),
        }
    }
}
impl Clock for RuntimeClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn monotonic_now(&self) -> Duration {
        self.started.elapsed()
    }
}

#[derive(Default)]
struct RuntimeIdGenerator {
    next: AtomicU64,
}
impl IdGenerator for RuntimeIdGenerator {
    fn next_id(&self) -> MemoryId {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let process = std::process::id();
        let epoch_nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        MemoryId::try_from(format!("memory-{process}-{epoch_nanos}-{id}"))
            .expect("generated memory identifier is nonempty and bounded")
    }
}

/// Deadline-enforcing adapter for optional enrichment work.
pub struct RuntimeProvider {
    started: Arc<Instant>,
    endpoint: Option<String>,
}

impl RuntimeProvider {
    pub fn new() -> Self {
        Self {
            started: Arc::new(Instant::now()),
            endpoint: None,
        }
    }

    fn configured(clock: &RuntimeClock, endpoint: String) -> Self {
        Self {
            started: clock.started.clone(),
            endpoint: Some(endpoint),
        }
    }
}

impl Default for RuntimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl EnrichmentProvider for RuntimeProvider {
    fn enrich(
        &self,
        request: EnrichmentRequest,
        deadline: Duration,
        cancelled: &AtomicBool,
    ) -> Result<EnrichmentResult, EnrichmentProviderError> {
        let Some(endpoint) = &self.endpoint else {
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return Err(EnrichmentProviderError::Cancelled);
                }
                let remaining = deadline.saturating_sub(self.started.elapsed());
                if remaining.is_zero() {
                    // Publish cancellation before returning so a future transport
                    // implementation cannot publish a result after expiry.
                    cancelled.store(true, Ordering::Release);
                    return Err(EnrichmentProviderError::Timeout);
                }
                std::thread::park_timeout(remaining.min(Duration::from_millis(1)));
            }
        };
        if cancelled.load(Ordering::Acquire) {
            return Err(EnrichmentProviderError::Cancelled);
        }
        let remaining = deadline.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            cancelled.store(true, Ordering::Release);
            return Err(EnrichmentProviderError::Timeout);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| EnrichmentProviderError::Unavailable)?;
        let projection = runtime.block_on(async {
            let request_future = async {
                let response = reqwest::Client::builder()
                    .timeout(remaining).build()
                    .map_err(|_| EnrichmentProviderError::Unavailable)?
                    .post(endpoint)
                    .json(&serde_json::json!({
                        "id": request.record.id().as_str(),
                        "revision": request.record.revision().as_str(),
                        "title": request.record.title(),
                        "body": request.record.body(),
                    })).send().await.map_err(|error| {
                        if error.is_timeout() {
                            cancelled.store(true, Ordering::Release);
                            EnrichmentProviderError::Timeout
                        } else { EnrichmentProviderError::Unavailable }
                    })?;
                if !response.status().is_success() { return Err(EnrichmentProviderError::Unavailable); }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ProviderResponse { projection: String }
                response.json::<ProviderResponse>().await
                    .map(|response| response.projection)
                    .map_err(|_| EnrichmentProviderError::Unavailable)
            };
            tokio::pin!(request_future);
            loop {
                tokio::select! {
                    result = &mut request_future => return result,
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {
                        if cancelled.load(Ordering::Acquire) { return Err(EnrichmentProviderError::Cancelled); }
                    }
                }
            }
        })?;
        Ok(EnrichmentResult {
            id: request.record.id().clone(),
            expected_revision: request.record.revision().clone(),
            projection: projection.into_bytes(),
        })
    }
}
