use std::{
    sync::atomic::AtomicBool,
    time::{Duration, SystemTime},
};

use extension_protocol::messages::{
    AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryScanRequest, AgentMemoryScanResult,
    AgentMemoryStorageFailure, AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
};

use crate::{MemoryId, MemoryRecord, Revision};

pub trait MemoryStore: Send + Sync {
    fn get(
        &self,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageFailure>;

    fn scan(
        &self,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageFailure>;

    fn transact(
        &self,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageFailure>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;

    fn monotonic_now(&self) -> Duration;
}

pub trait IdGenerator: Send + Sync {
    fn next_id(&self) -> MemoryId;
}

pub trait EnrichmentProvider: Send + Sync {
    fn enrich(
        &self,
        request: EnrichmentRequest,
        deadline: Duration,
        cancelled: &AtomicBool,
    ) -> Result<EnrichmentResult, EnrichmentProviderError>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnrichmentRequest {
    pub record: MemoryRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrichmentResult {
    pub id: MemoryId,
    pub expected_revision: Revision,
    pub projection: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnrichmentProviderError {
    Timeout,
    Cancelled,
    Unavailable,
}

impl std::fmt::Display for EnrichmentProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Timeout => "enrichment provider timed out",
            Self::Cancelled => "enrichment provider cancelled",
            Self::Unavailable => "enrichment provider unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for EnrichmentProviderError {}
