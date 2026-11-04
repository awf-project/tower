use std::{
    collections::VecDeque,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use extension_protocol::HostCall;
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    DeadlineLayer, EmbeddingProviderError, EmbeddingProviderPort, EmbeddingRequest,
    EmbeddingSpaceIdentity, IndexedEmbedding, OperationDeadline, PortFuture, SemanticOperation,
};

#[derive(Clone, Debug, PartialEq)]
pub struct DeterministicEmbeddingProviderConfig {
    pub identity: EmbeddingSpaceIdentity,
    pub dimensions: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeterministicEmbeddingFault {
    Unavailable,
    TimedOut,
    MalformedResponse,
    NonFiniteVector { index: usize },
    DimensionMismatch { expected: usize, actual: usize },
}

pub struct EmbeddingProviderContractHarness;

/// Supplies the provider-specific failure injection and deadline observation
/// needed by the reusable provider contract.  T004 can implement this fixture
/// around its local HTTP test server without coupling the contract to HTTP.
pub trait EmbeddingProviderContractFixture: Send + Sync {
    fn provider(&self) -> &dyn EmbeddingProviderPort;

    fn inject_fault(&self, fault: DeterministicEmbeddingFault);

    fn assert_all_calls_used_deadline(&self, expected: &OperationDeadline);

    fn deadline(&self) -> OperationDeadline {
        OperationDeadline::for_provider(
            Instant::now() + Duration::from_secs(30),
            SemanticOperation::Embedding,
            30_000,
        )
    }

    fn renew_deadline(&self, deadline: &OperationDeadline) -> OperationDeadline {
        deadline.clone()
    }
}

impl EmbeddingProviderContractHarness {
    pub fn new() -> Self {
        Self
    }

    pub fn verify<'a>(
        &'a self,
        fixture: &'a dyn EmbeddingProviderContractFixture,
    ) -> PortFuture<'a, ()> {
        Box::pin(async move {
            let request = EmbeddingRequest {
                inputs: vec![
                    "first contract input".to_owned(),
                    "second contract input".to_owned(),
                ],
            };
            let deadline = fixture.deadline();

            let first = fixture
                .provider()
                .embed(&request, fixture.renew_deadline(&deadline))
                .await
                .expect("the shared contract requires successful embeddings");
            let repeated = fixture
                .provider()
                .embed(&request, fixture.renew_deadline(&deadline))
                .await
                .expect("the shared contract requires repeatable embeddings");
            assert_eq!(first, repeated, "embedding results must be repeatable");
            assert_eq!(first.len(), request.inputs.len());
            let mut seen_indices = vec![false; request.inputs.len()];
            for (expected_index, embedding) in first.iter().enumerate() {
                assert_eq!(
                    embedding.index, expected_index,
                    "embeddings must stay ordered"
                );
                assert!(
                    !seen_indices[embedding.index],
                    "embedding indices must be unique"
                );
                seen_indices[embedding.index] = true;
                assert!(
                    embedding
                        .vector
                        .values
                        .iter()
                        .all(|value| value.is_finite()),
                    "embedding {expected_index} must be finite"
                );
                assert_eq!(
                    embedding.vector.values.len(),
                    first[0].vector.values.len(),
                    "all embeddings must use one dimension"
                );
            }
            assert!(
                seen_indices.into_iter().all(|seen| seen),
                "embedding indices must be complete"
            );

            let failures = [
                (
                    DeterministicEmbeddingFault::Unavailable,
                    EmbeddingProviderError::Unavailable,
                ),
                (
                    DeterministicEmbeddingFault::TimedOut,
                    EmbeddingProviderError::TimedOut {
                        operation: SemanticOperation::Embedding,
                        layer: DeadlineLayer::Provider,
                        budget_ms: deadline.budget_ms(),
                    },
                ),
                (
                    DeterministicEmbeddingFault::MalformedResponse,
                    EmbeddingProviderError::MalformedResponse,
                ),
                (
                    DeterministicEmbeddingFault::NonFiniteVector { index: 1 },
                    EmbeddingProviderError::NonFiniteVector { index: 1 },
                ),
                (
                    DeterministicEmbeddingFault::DimensionMismatch {
                        expected: 3,
                        actual: 2,
                    },
                    EmbeddingProviderError::DimensionMismatch {
                        expected: 3,
                        actual: 2,
                    },
                ),
            ];

            for (fault, expected) in failures {
                fixture.inject_fault(fault);
                assert_eq!(
                    fixture
                        .provider()
                        .embed(&request, fixture.renew_deadline(&deadline))
                        .await,
                    Err(expected),
                    "injected provider failures must preserve their typed payload"
                );
                assert_eq!(
                    fixture
                        .provider()
                        .embed(&request, fixture.renew_deadline(&deadline))
                        .await,
                    Ok(first.clone()),
                    "the provider must recover after each failed call"
                );
            }

            fixture.assert_all_calls_used_deadline(&deadline);
        })
    }
}

impl Default for EmbeddingProviderContractHarness {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DeterministicEmbeddingProvider {
    config: DeterministicEmbeddingProviderConfig,
    faults: Mutex<VecDeque<DeterministicEmbeddingFault>>,
    observed_deadlines: Mutex<Vec<OperationDeadline>>,
}

impl DeterministicEmbeddingProvider {
    pub fn new(config: DeterministicEmbeddingProviderConfig) -> Self {
        Self {
            config,
            faults: Mutex::new(VecDeque::new()),
            observed_deadlines: Mutex::new(Vec::new()),
        }
    }

    pub fn inject_fault(&self, fault: DeterministicEmbeddingFault) {
        self.faults
            .lock()
            .expect("deterministic embedding fault queue lock poisoned")
            .push_back(fault);
    }

    fn embedding_for(&self, index: usize, input: &str) -> IndexedEmbedding {
        let digest = Sha256::digest(input.as_bytes());
        let values = (0..self.config.dimensions)
            .map(|dimension| {
                let byte = digest[dimension % digest.len()];
                (f32::from(byte) + 1.0) / 256.0
            })
            .collect();

        IndexedEmbedding {
            index,
            vector: crate::EmbeddingVector { values },
        }
    }
}

impl EmbeddingProviderContractFixture for DeterministicEmbeddingProvider {
    fn provider(&self) -> &dyn EmbeddingProviderPort {
        self
    }

    fn inject_fault(&self, fault: DeterministicEmbeddingFault) {
        self.inject_fault(fault);
    }

    fn assert_all_calls_used_deadline(&self, expected: &OperationDeadline) {
        let observed = self
            .observed_deadlines
            .lock()
            .expect("deterministic embedding deadline log lock poisoned");
        assert!(!observed.is_empty(), "the contract must call the provider");
        for actual in observed.iter() {
            assert_eq!(actual.deadline(), expected.deadline());
            assert_eq!(actual.operation(), expected.operation());
            assert_eq!(actual.layer(), expected.layer());
            assert_eq!(actual.budget_ms(), expected.budget_ms());
        }
    }
}

impl EmbeddingProviderPort for DeterministicEmbeddingProvider {
    fn identity(&self) -> EmbeddingSpaceIdentity {
        self.config.identity.clone()
    }

    fn embed<'a>(
        &'a self,
        request: &'a EmbeddingRequest,
        deadline: OperationDeadline,
    ) -> PortFuture<'a, Result<Vec<IndexedEmbedding>, EmbeddingProviderError>> {
        Box::pin(async move {
            self.observed_deadlines
                .lock()
                .expect("deterministic embedding deadline log lock poisoned")
                .push(deadline.clone());

            let fault = self
                .faults
                .lock()
                .expect("deterministic embedding fault queue lock poisoned")
                .pop_front();
            if let Some(fault) = fault {
                return Err(match fault {
                    DeterministicEmbeddingFault::Unavailable => EmbeddingProviderError::Unavailable,
                    DeterministicEmbeddingFault::TimedOut => EmbeddingProviderError::TimedOut {
                        operation: deadline.operation().clone(),
                        layer: deadline.layer().clone(),
                        budget_ms: deadline.budget_ms(),
                    },
                    DeterministicEmbeddingFault::MalformedResponse => {
                        EmbeddingProviderError::MalformedResponse
                    }
                    DeterministicEmbeddingFault::NonFiniteVector { index } => {
                        EmbeddingProviderError::NonFiniteVector { index }
                    }
                    DeterministicEmbeddingFault::DimensionMismatch { expected, actual } => {
                        EmbeddingProviderError::DimensionMismatch { expected, actual }
                    }
                });
            }

            Ok(request
                .inputs
                .iter()
                .enumerate()
                .map(|(index, input)| self.embedding_for(index, input))
                .collect())
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedEmbeddingConfig {
    pub base_url: String,
    pub model: String,
    pub timeout_ms: u64,
    pub batch_size: usize,
    pub dimensions: Option<usize>,
    pub allow_remote: bool,
    pub api_key_env: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EmbeddingConfigurationError {
    InvalidEndpoint,
    EmptyModel,
    InvalidNumericValue,
    UrlCredentialsForbidden,
    RemoteOptInRequired,
    RemoteHttpsRequired,
    MissingApiKey,
}

pub struct OpenAiCompatibleEmbeddingProvider {
    client: Client,
    endpoint: Url,
    model: String,
    timeout: Duration,
    batch_size: usize,
    dimensions: Option<usize>,
    api_key: Option<String>,
    log_host_call: Option<Arc<dyn Fn(HostCall) + Send + Sync>>,
}

#[derive(Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiIndexedEmbedding>,
}

#[derive(Deserialize)]
struct OpenAiIndexedEmbedding {
    index: usize,
    embedding: Vec<f64>,
}

impl OpenAiCompatibleEmbeddingProvider {
    pub fn new(config: ValidatedEmbeddingConfig) -> Result<Self, EmbeddingConfigurationError> {
        let mut endpoint = Url::parse(&config.base_url)
            .map_err(|_| EmbeddingConfigurationError::InvalidEndpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(EmbeddingConfigurationError::InvalidEndpoint);
        }
        if config.model.trim().is_empty() {
            return Err(EmbeddingConfigurationError::EmptyModel);
        }
        if config.timeout_ms == 0
            || config.timeout_ms > 3_600_000
            || config.batch_size == 0
            || config.batch_size > 256
            || config
                .dimensions
                .is_some_and(|dimensions| dimensions == 0 || dimensions > 65_536)
        {
            return Err(EmbeddingConfigurationError::InvalidNumericValue);
        }
        if endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err(EmbeddingConfigurationError::InvalidEndpoint);
        }
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(EmbeddingConfigurationError::UrlCredentialsForbidden);
        }

        let loopback = endpoint.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if !loopback && !config.allow_remote {
            return Err(EmbeddingConfigurationError::RemoteOptInRequired);
        }
        if !loopback && endpoint.scheme() != "https" {
            return Err(EmbeddingConfigurationError::RemoteHttpsRequired);
        }

        let api_key = config
            .api_key_env
            .as_deref()
            .map(std::env::var)
            .transpose()
            .map_err(|_| EmbeddingConfigurationError::MissingApiKey)?;
        if api_key.as_ref().is_some_and(|key| key.is_empty()) {
            return Err(EmbeddingConfigurationError::MissingApiKey);
        }

        let mut path = endpoint.path().trim_end_matches('/').to_owned();
        path.push_str("/embeddings");
        endpoint.set_path(&path);
        let client = Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|_| EmbeddingConfigurationError::InvalidEndpoint)?;

        Ok(Self {
            client,
            endpoint,
            model: config.model,
            timeout: Duration::from_millis(config.timeout_ms),
            batch_size: config.batch_size,
            dimensions: config.dimensions,
            api_key,
            log_host_call: None,
        })
    }

    /// Connects the adapter's sanitized diagnostics to the sidecar HostCall
    /// transport. The callback receives only fixed, allowlisted messages.
    pub fn with_log_host_call_sink(
        mut self,
        sink: impl Fn(HostCall) + Send + Sync + 'static,
    ) -> Self {
        self.log_host_call = Some(Arc::new(sink));
        self
    }
}

impl EmbeddingProviderPort for OpenAiCompatibleEmbeddingProvider {
    fn identity(&self) -> EmbeddingSpaceIdentity {
        EmbeddingSpaceIdentity {
            provider: "openai-compatible".to_owned(),
            model: self.model.clone(),
            dimensions: self.dimensions,
        }
    }

    fn embed<'a>(
        &'a self,
        request: &'a EmbeddingRequest,
        deadline: OperationDeadline,
    ) -> PortFuture<'a, Result<Vec<IndexedEmbedding>, EmbeddingProviderError>> {
        Box::pin(async move {
            let result = async {
                let provider_limit = Instant::now() + self.timeout;
                let (absolute_deadline, timeout_operation, timeout_layer, timeout_budget_ms) =
                    match deadline.deadline() {
                        Some(outer) if outer <= provider_limit => (
                            outer,
                            deadline.operation().clone(),
                            deadline.layer().clone(),
                            deadline.budget_ms(),
                        ),
                        _ => (
                            provider_limit,
                            SemanticOperation::Embedding,
                            DeadlineLayer::Provider,
                            self.timeout.as_millis() as u64,
                        ),
                    };
                let timeout_error = || EmbeddingProviderError::TimedOut {
                    operation: timeout_operation.clone(),
                    layer: timeout_layer.clone(),
                    budget_ms: timeout_budget_ms,
                };
                let mut output = Vec::with_capacity(request.inputs.len());

                for (batch_number, inputs) in request.inputs.chunks(self.batch_size).enumerate() {
                    let payload = OpenAiEmbeddingRequest {
                        model: &self.model,
                        input: inputs,
                        encoding_format: "float",
                        dimensions: self.dimensions,
                    };
                    let mut builder = self.client.post(self.endpoint.clone()).json(&payload);
                    if let Some(api_key) = &self.api_key {
                        builder = builder.bearer_auth(api_key);
                    }
                    let response = tokio::time::timeout_at(
                        tokio::time::Instant::from_std(absolute_deadline),
                        builder.send(),
                    )
                    .await
                    .map_err(|_| timeout_error())?
                    .map_err(|_| EmbeddingProviderError::Unavailable)?;
                    if response.status().is_redirection() || !response.status().is_success() {
                        return Err(EmbeddingProviderError::Unavailable);
                    }
                    let response: OpenAiEmbeddingResponse = tokio::time::timeout_at(
                        tokio::time::Instant::from_std(absolute_deadline),
                        response.json(),
                    )
                    .await
                    .map_err(|_| timeout_error())?
                    .map_err(|_| EmbeddingProviderError::MalformedResponse)?;
                    let offset = batch_number * self.batch_size;
                    let indexed = response
                        .data
                        .into_iter()
                        .map(|item| IndexedEmbedding {
                            index: item.index,
                            vector: crate::EmbeddingVector {
                                values: item
                                    .embedding
                                    .into_iter()
                                    .map(|value| value as f32)
                                    .collect(),
                            },
                        })
                        .collect();
                    let vectors = crate::application::validate_embedding_batch(
                        indexed,
                        inputs.len(),
                        self.dimensions,
                    )?;
                    output.extend(vectors.into_iter().enumerate().map(|(index, vector)| {
                        IndexedEmbedding {
                            index: offset + index,
                            vector,
                        }
                    }));
                }
                Ok(output)
            }
            .await;
            if result.is_err()
                && let Some(log_host_call) = &self.log_host_call
            {
                log_host_call(HostCall::Log {
                    level: "warn".to_owned(),
                    msg: "Embedding provider request failed".to_owned(),
                });
            }
            result
        })
    }
}
