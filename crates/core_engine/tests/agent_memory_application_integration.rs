use std::{
    sync::Arc,
    sync::atomic::AtomicBool,
    time::{Duration, SystemTime},
};

use agent_memory_extension::{
    AgentMemoryLimitsConfig, Clock, EnrichmentProvider, EnrichmentProviderError, EnrichmentRequest,
    EnrichmentResult, IdGenerator, MemoryApplication, MemoryId, MemoryStore, PersistenceMode,
    PrivacyFilter,
};
use core_engine::{
    adapters::InMemoryAgentMemoryStorage,
    ports::{
        AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryNamespace, AgentMemoryScanRequest,
        AgentMemoryScanResult, AgentMemoryStorageBounds, AgentMemoryStorageError,
        AgentMemoryStoragePort, AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
    },
};

struct OwnedStore {
    storage: Arc<InMemoryAgentMemoryStorage>,
    namespace: AgentMemoryNamespace,
}

impl MemoryStore for OwnedStore {
    fn get(
        &self,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, extension_protocol::messages::AgentMemoryStorageFailure> {
        self.storage
            .get(&self.namespace, request)
            .map_err(map_storage_error)
    }

    fn scan(
        &self,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, extension_protocol::messages::AgentMemoryStorageFailure>
    {
        self.storage
            .scan(&self.namespace, request)
            .map_err(map_storage_error)
    }

    fn transact(
        &self,
        request: AgentMemoryTransactionRequest,
    ) -> Result<
        AgentMemoryTransactionOutcome,
        extension_protocol::messages::AgentMemoryStorageFailure,
    > {
        self.storage
            .transact(&self.namespace, request)
            .map_err(map_storage_error)
    }
}

fn map_storage_error(
    error: AgentMemoryStorageError,
) -> extension_protocol::messages::AgentMemoryStorageFailure {
    match error {
        AgentMemoryStorageError::BoundsExceeded => {
            extension_protocol::messages::AgentMemoryStorageFailure::BoundsExceeded
        }
        AgentMemoryStorageError::Unavailable => {
            extension_protocol::messages::AgentMemoryStorageFailure::Unavailable
        }
        AgentMemoryStorageError::CommitOutcomeUnknown => {
            extension_protocol::messages::AgentMemoryStorageFailure::CommitOutcomeUnknown
        }
    }
}

#[derive(Clone, Copy)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    fn monotonic_now(&self) -> Duration {
        Duration::ZERO
    }
}

#[derive(Clone)]
struct FixedId;

impl IdGenerator for FixedId {
    fn next_id(&self) -> MemoryId {
        MemoryId::try_from("integration-id".to_owned()).unwrap()
    }
}

struct NoProvider;

impl EnrichmentProvider for NoProvider {
    fn enrich(
        &self,
        _request: EnrichmentRequest,
        _deadline: Duration,
        _cancelled: &AtomicBool,
    ) -> Result<EnrichmentResult, EnrichmentProviderError> {
        unreachable!("the integration scenario configures no provider")
    }
}

#[test]
fn public_application_initializes_through_the_owned_storage_adapter() {
    let bounds =
        AgentMemoryStorageBounds::try_new(4_096, 1 << 20, 1_024, 4_096, 4_096, 1 << 24, 16)
            .unwrap();
    let storage = Arc::new(InMemoryAgentMemoryStorage::new(bounds));
    let namespace =
        AgentMemoryNamespace::from_trusted_identity(b"integration-project", b"agent_memory")
            .unwrap();
    let application = MemoryApplication::<_, _, _, NoProvider>::new_configured(
        OwnedStore { storage, namespace },
        FixedClock,
        FixedId,
        None,
        PrivacyFilter::default(),
        AgentMemoryLimitsConfig {
            max_title_bytes: 1_024,
            max_body_bytes: 65_536,
            max_metadata_items: 100,
            max_metadata_string_bytes: 1_024,
            max_batch_items: 100,
            default_results: 20,
            max_results: 100,
            default_response_bytes: 32_768,
            max_response_bytes: 131_072,
            max_query_bytes: 4_096,
        },
        PersistenceMode::InMemory,
        true,
        Duration::from_millis(5_000),
    );

    let status = application.initialize_store().unwrap();

    assert_eq!(status.schema_version, 1);
    assert_eq!(status.persistence_mode, PersistenceMode::InMemory);
    assert!(status.durable_data_unavailable);
}
