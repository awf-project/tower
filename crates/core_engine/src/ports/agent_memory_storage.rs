use std::fmt;

use sha2::{Digest, Sha256};

pub use extension_protocol::messages::{
    AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryScanRequest, AgentMemoryScanResult,
    AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentMemoryNamespace([u8; 32]);

impl AgentMemoryNamespace {
    pub fn from_trusted_identity(
        project: &[u8],
        extension: &[u8],
    ) -> Result<Self, AgentMemoryStorageError> {
        if project.is_empty() || extension.is_empty() {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        let project_len =
            u64::try_from(project.len()).map_err(|_| AgentMemoryStorageError::BoundsExceeded)?;
        let extension_len =
            u64::try_from(extension.len()).map_err(|_| AgentMemoryStorageError::BoundsExceeded)?;

        let mut hasher = Sha256::new();
        hasher.update(b"tower.agent_memory.namespace.v1\0");
        hasher.update(project_len.to_be_bytes());
        hasher.update(project);
        hasher.update(extension_len.to_be_bytes());
        hasher.update(extension);
        Ok(Self(hasher.finalize().into()))
    }

    /// Exposes the opaque namespace bytes to host-owned contract tests only.
    #[cfg(test)]
    pub(crate) fn test_bytes(&self) -> [u8; 32] {
        self.0
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMemoryStorageError {
    BoundsExceeded,
    Unavailable,
    CommitOutcomeUnknown,
}

impl fmt::Display for AgentMemoryStorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BoundsExceeded => "agent memory storage bounds exceeded",
            Self::Unavailable => "agent memory storage unavailable",
            Self::CommitOutcomeUnknown => "agent memory storage commit outcome unknown",
        })
    }
}

impl std::error::Error for AgentMemoryStorageError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentMemoryStorageBounds {
    pub max_key_bytes: usize,
    pub max_value_bytes: usize,
    pub max_scan_items: u32,
    pub max_transaction_conditions: usize,
    pub max_transaction_mutations: usize,
    pub max_transaction_bytes: usize,
    pub max_outstanding_requests: usize,
}

impl AgentMemoryStorageBounds {
    pub fn try_new(
        max_key_bytes: usize,
        max_value_bytes: usize,
        max_scan_items: u32,
        max_transaction_conditions: usize,
        max_transaction_mutations: usize,
        max_transaction_bytes: usize,
        max_outstanding_requests: usize,
    ) -> Result<Self, AgentMemoryStorageError> {
        if max_key_bytes == 0
            || max_value_bytes == 0
            || max_scan_items == 0
            || max_transaction_conditions == 0
            || max_transaction_mutations == 0
            || max_transaction_bytes == 0
            || max_outstanding_requests == 0
        {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        Ok(Self {
            max_key_bytes,
            max_value_bytes,
            max_scan_items,
            max_transaction_conditions,
            max_transaction_mutations,
            max_transaction_bytes,
            max_outstanding_requests,
        })
    }
}

pub trait AgentMemoryStoragePort: Send + Sync {
    fn get(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageError>;

    fn scan(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageError>;

    fn transact(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageError>;
}
