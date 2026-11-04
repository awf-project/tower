use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use extension_protocol::messages::{
    AgentMemoryCondition, AgentMemoryKeyValue, AgentMemoryMutation,
};
use sled::transaction::{ConflictableTransactionError, TransactionError};

use crate::ports::{
    AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryNamespace, AgentMemoryScanRequest,
    AgentMemoryScanResult, AgentMemoryStorageBounds, AgentMemoryStorageError,
    AgentMemoryStoragePort, AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
};

#[derive(Debug)]
pub struct InMemoryAgentMemoryStorage {
    bounds: AgentMemoryStorageBounds,
    state: Mutex<HashMap<AgentMemoryNamespace, NamespaceState>>,
    outstanding_requests: AtomicUsize,
    #[cfg(test)]
    request_barrier: Option<RequestBarrier>,
}

pub struct SledAgentMemoryStorage {
    bounds: AgentMemoryStorageBounds,
    backend: Box<dyn AgentMemoryBackend>,
    outstanding_requests: AtomicUsize,
    snapshot_lock: Mutex<()>,
    transaction_barriers: Option<TransactionBarriers>,
}

impl std::fmt::Debug for SledAgentMemoryStorage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SledAgentMemoryStorage")
            .finish_non_exhaustive()
    }
}

/// Failure phases that the durable adapter's contract tests must be able to
/// inject at the backend boundary. This is intentionally crate-private and
/// test-only: production construction always uses the real Sled backend.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SledBackendFailure {
    Open,
    SnapshotRead,
    AbortBeforePublication,
    FlushAfterCommit,
}

#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct TransactionBarriers {
    pub before_transaction_entered: std::sync::Arc<std::sync::Barrier>,
    pub before_transaction_release: std::sync::Arc<std::sync::Barrier>,
    pub before_flush_entered: std::sync::Arc<std::sync::Barrier>,
    pub before_flush_release: std::sync::Arc<std::sync::Barrier>,
}

impl TransactionBarriers {
    fn new() -> Self {
        Self {
            before_transaction_entered: std::sync::Arc::new(std::sync::Barrier::new(2)),
            before_transaction_release: std::sync::Arc::new(std::sync::Barrier::new(2)),
            before_flush_entered: std::sync::Arc::new(std::sync::Barrier::new(2)),
            before_flush_release: std::sync::Arc::new(std::sync::Barrier::new(2)),
        }
    }
}

#[derive(Debug)]
enum BackendTransactionOutcome {
    Conflict,
    Committed(u64),
}

#[derive(Debug)]
enum SledTransactionAbort {
    ConditionConflict,
    BoundsExceeded,
}

#[derive(Debug)]
struct BackendScanSnapshot {
    entries: Vec<AgentMemoryKeyValue>,
    next_cursor: Option<Vec<u8>>,
    generation: u64,
}

trait AgentMemoryBackend: Send + Sync {
    fn get_snapshot(
        &self,
        namespace: &[u8; 32],
        key: &[u8],
    ) -> Result<(Option<Vec<u8>>, u64), AgentMemoryStorageError>;
    fn scan_snapshot(
        &self,
        namespace: &[u8; 32],
        prefix: &[u8],
        cursor: Option<&[u8]>,
        limit: usize,
    ) -> Result<BackendScanSnapshot, AgentMemoryStorageError>;
    fn transact(
        &self,
        namespace: &[u8; 32],
        request: &AgentMemoryTransactionRequest,
    ) -> Result<BackendTransactionOutcome, AgentMemoryStorageError>;
    fn flush(&self) -> Result<(), AgentMemoryStorageError>;
}

trait AgentMemoryBackendFactory {
    fn open(&self, root: &Path) -> Result<Box<dyn AgentMemoryBackend>, AgentMemoryStorageError>;
}

struct SledBackendFactory {
    #[cfg(test)]
    failure: Option<SledBackendFailure>,
}

impl AgentMemoryBackendFactory for SledBackendFactory {
    fn open(&self, root: &Path) -> Result<Box<dyn AgentMemoryBackend>, AgentMemoryStorageError> {
        let db = crate::adapters::storage::SledStorageAdapter::open_db_with_retry(root)
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        let tree = db
            .open_tree("agent_memory_v1")
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        Ok(Box::new(SledBackend {
            tree,
            #[cfg(debug_assertions)]
            process_probe: std::env::var_os("TOWER_TEST_AGENT_MEMORY_CONTROL").map(Into::into),
            #[cfg(test)]
            failure: self.failure,
        }))
    }
}

#[cfg(test)]
struct FailingOpenBackendFactory;

#[cfg(test)]
impl AgentMemoryBackendFactory for FailingOpenBackendFactory {
    fn open(&self, _root: &Path) -> Result<Box<dyn AgentMemoryBackend>, AgentMemoryStorageError> {
        Err(AgentMemoryStorageError::Unavailable)
    }
}

#[derive(Debug)]
struct SledBackend {
    tree: sled::Tree,
    #[cfg(debug_assertions)]
    process_probe: Option<std::path::PathBuf>,
    #[cfg(test)]
    failure: Option<SledBackendFailure>,
}

const GENERATION_TAG: u8 = 0;
const VALUE_TAG: u8 = 1;

fn generation_key(namespace: &[u8; 32]) -> Vec<u8> {
    let mut key = Vec::with_capacity(33);
    key.push(GENERATION_TAG);
    key.extend_from_slice(namespace);
    key
}

fn value_prefix(namespace: &[u8; 32]) -> Vec<u8> {
    let mut key = Vec::with_capacity(33);
    key.push(VALUE_TAG);
    key.extend_from_slice(namespace);
    key
}

fn value_key(namespace: &[u8; 32], logical_key: &[u8]) -> Vec<u8> {
    let mut key = value_prefix(namespace);
    key.extend_from_slice(logical_key);
    key
}

fn decode_generation(bytes: Option<impl AsRef<[u8]>>) -> Result<u64, AgentMemoryStorageError> {
    let Some(bytes) = bytes else { return Ok(0) };
    let bytes: [u8; 8] = bytes
        .as_ref()
        .try_into()
        .map_err(|_| AgentMemoryStorageError::Unavailable)?;
    Ok(u64::from_be_bytes(bytes))
}

impl SledBackend {
    /// Explicit process-test control, captured per adapter at construction.
    /// Release builds contain neither the environment lookup nor these hooks.
    /// Failures occur at the real read/transaction/flush boundary, never after
    /// a successful durable acknowledgement has already been fabricated.
    #[cfg(debug_assertions)]
    fn process_probe(&self, phase: &str) -> Result<(), AgentMemoryStorageError> {
        let Some(control) = &self.process_probe else {
            return Ok(());
        };
        let mode = std::fs::read_to_string(control).unwrap_or_default();
        if mode == "unavailable" || mode.starts_with("unavailable:") {
            return Err(AgentMemoryStorageError::Unavailable);
        }
        if mode == "flush_failure" && phase == "before_flush" {
            return Err(AgentMemoryStorageError::CommitOutcomeUnknown);
        }
        if mode == phase {
            let observation_error = if phase == "before_transaction" || phase == "read" {
                AgentMemoryStorageError::Unavailable
            } else {
                AgentMemoryStorageError::CommitOutcomeUnknown
            };
            let staged = control.with_extension("reached.tmp");
            std::fs::write(&staged, phase).map_err(|_| observation_error)?;
            std::fs::rename(staged, control.with_extension("reached"))
                .map_err(|_| observation_error)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while !control.with_extension("release").exists() {
                if std::time::Instant::now() >= deadline {
                    return Err(AgentMemoryStorageError::CommitOutcomeUnknown);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Ok(())
    }
}

impl AgentMemoryBackend for SledBackend {
    fn get_snapshot(
        &self,
        namespace: &[u8; 32],
        key: &[u8],
    ) -> Result<(Option<Vec<u8>>, u64), AgentMemoryStorageError> {
        #[cfg(debug_assertions)]
        self.process_probe("read")?;
        #[cfg(test)]
        if self.failure == Some(SledBackendFailure::SnapshotRead) {
            return Err(AgentMemoryStorageError::Unavailable);
        }
        let generation = decode_generation(
            self.tree
                .get(generation_key(namespace))
                .map_err(|_| AgentMemoryStorageError::Unavailable)?,
        )?;
        let value = self
            .tree
            .get(value_key(namespace, key))
            .map_err(|_| AgentMemoryStorageError::Unavailable)?
            .map(|value| value.to_vec());
        Ok((value, generation))
    }

    fn scan_snapshot(
        &self,
        namespace: &[u8; 32],
        prefix: &[u8],
        cursor: Option<&[u8]>,
        limit: usize,
    ) -> Result<BackendScanSnapshot, AgentMemoryStorageError> {
        #[cfg(debug_assertions)]
        self.process_probe("read")?;
        #[cfg(test)]
        if self.failure == Some(SledBackendFailure::SnapshotRead) {
            return Err(AgentMemoryStorageError::Unavailable);
        }
        let generation = decode_generation(
            self.tree
                .get(generation_key(namespace))
                .map_err(|_| AgentMemoryStorageError::Unavailable)?,
        )?;
        let storage_prefix = value_prefix(namespace);
        let mut logical_prefix = storage_prefix.clone();
        logical_prefix.extend_from_slice(prefix);
        let mut entries = Vec::with_capacity(limit);
        let mut has_more = false;
        for item in self.tree.scan_prefix(logical_prefix) {
            let (key, value) = item.map_err(|_| AgentMemoryStorageError::Unavailable)?;
            let logical_key = &key[storage_prefix.len()..];
            if cursor.is_some_and(|cursor| logical_key <= cursor) {
                continue;
            }
            if entries.len() == limit {
                has_more = true;
                break;
            }
            entries.push(AgentMemoryKeyValue {
                key: logical_key.to_vec(),
                value: value.to_vec(),
            });
        }
        let next_cursor = has_more
            .then(|| entries.last().map(|entry| entry.key.clone()))
            .flatten();
        Ok(BackendScanSnapshot {
            entries,
            next_cursor,
            generation,
        })
    }

    fn transact(
        &self,
        namespace: &[u8; 32],
        request: &AgentMemoryTransactionRequest,
    ) -> Result<BackendTransactionOutcome, AgentMemoryStorageError> {
        #[cfg(debug_assertions)]
        self.process_probe("before_transaction")?;
        #[cfg(test)]
        if self.failure == Some(SledBackendFailure::AbortBeforePublication) {
            return Err(AgentMemoryStorageError::Unavailable);
        }
        let generation_key = generation_key(namespace);
        let result = self.tree.transaction(|tree| {
            for condition in &request.conditions {
                let matches = match condition {
                    AgentMemoryCondition::KeyAbsent { key } => {
                        tree.get(value_key(namespace, key))?.is_none()
                    }
                    AgentMemoryCondition::ValueEquals { key, value } => tree
                        .get(value_key(namespace, key))?
                        .is_some_and(|actual| actual.as_ref() == value.as_slice()),
                };
                if !matches {
                    return Err(ConflictableTransactionError::Abort(
                        SledTransactionAbort::ConditionConflict,
                    ));
                }
            }
            let generation = decode_generation(tree.get(&generation_key)?)
                .map_err(|_| {
                    ConflictableTransactionError::Storage(sled::Error::Unsupported(
                        "invalid agent memory generation".to_owned(),
                    ))
                })?
                .checked_add(1)
                .ok_or_else(|| {
                    ConflictableTransactionError::Abort(SledTransactionAbort::BoundsExceeded)
                })?;
            for mutation in &request.mutations {
                match mutation {
                    AgentMemoryMutation::Put { key, value } => {
                        tree.insert(value_key(namespace, key), value.as_slice())?;
                    }
                    AgentMemoryMutation::Delete { key } => {
                        tree.remove(value_key(namespace, key))?;
                    }
                }
            }
            tree.insert(generation_key.as_slice(), &generation.to_be_bytes())?;
            Ok(generation)
        });
        match result {
            Ok(generation) => {
                #[cfg(debug_assertions)]
                self.process_probe("after_transaction")?;
                Ok(BackendTransactionOutcome::Committed(generation))
            }
            Err(TransactionError::Abort(SledTransactionAbort::ConditionConflict)) => {
                Ok(BackendTransactionOutcome::Conflict)
            }
            Err(TransactionError::Abort(SledTransactionAbort::BoundsExceeded)) => {
                Err(AgentMemoryStorageError::BoundsExceeded)
            }
            Err(TransactionError::Storage(_)) => Err(AgentMemoryStorageError::CommitOutcomeUnknown),
        }
    }

    fn flush(&self) -> Result<(), AgentMemoryStorageError> {
        #[cfg(debug_assertions)]
        self.process_probe("before_flush")?;
        #[cfg(test)]
        if self.failure == Some(SledBackendFailure::FlushAfterCommit) {
            return Err(AgentMemoryStorageError::CommitOutcomeUnknown);
        }
        self.tree
            .flush()
            .map(|_| ())
            .map_err(|_| AgentMemoryStorageError::CommitOutcomeUnknown)
    }
}

impl SledAgentMemoryStorage {
    pub fn open(
        root: &Path,
        bounds: AgentMemoryStorageBounds,
    ) -> Result<Self, AgentMemoryStorageError> {
        Self::open_with_factory(
            root,
            bounds,
            &SledBackendFactory {
                #[cfg(test)]
                failure: None,
            },
        )
    }

    /// Constructs the same adapter logic with a backend that fails at one
    /// actual storage phase. T002 implements this through its internal backend
    /// interface; it must not be a post-success wrapper.
    #[cfg(test)]
    pub(crate) fn open_with_backend_failure(
        root: &Path,
        bounds: AgentMemoryStorageBounds,
        failure: SledBackendFailure,
    ) -> Result<Self, AgentMemoryStorageError> {
        if failure == SledBackendFailure::Open {
            return Self::open_with_factory(root, bounds, &FailingOpenBackendFactory);
        }
        Self::open_with_factory(
            root,
            bounds,
            &SledBackendFactory {
                failure: Some(failure),
            },
        )
    }

    fn open_with_factory(
        root: &Path,
        bounds: AgentMemoryStorageBounds,
        factory: &dyn AgentMemoryBackendFactory,
    ) -> Result<Self, AgentMemoryStorageError> {
        Ok(Self {
            bounds,
            backend: factory.open(root)?,
            outstanding_requests: AtomicUsize::new(0),
            snapshot_lock: Mutex::new(()),
            transaction_barriers: None,
        })
    }

    /// Test harness seam for observing the durable acknowledgement boundary.
    /// Normal production construction through [`Self::open`] always uses inert hooks.
    #[doc(hidden)]
    pub fn open_with_transaction_barriers(
        root: &Path,
        bounds: AgentMemoryStorageBounds,
    ) -> Result<(Self, TransactionBarriers), AgentMemoryStorageError> {
        let barriers = TransactionBarriers::new();
        let mut storage = Self::open(root, bounds)?;
        storage.transaction_barriers = Some(barriers.clone());
        Ok((storage, barriers))
    }

    fn acquire_request(&self) -> Result<RequestPermit<'_>, AgentMemoryStorageError> {
        self.outstanding_requests
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |current| {
                (current < self.bounds.max_outstanding_requests).then_some(current + 1)
            })
            .map_err(|_| AgentMemoryStorageError::BoundsExceeded)?;
        Ok(RequestPermit {
            outstanding_requests: &self.outstanding_requests,
        })
    }

    fn validator(&self) -> InMemoryAgentMemoryStorage {
        InMemoryAgentMemoryStorage::new(self.bounds)
    }
}

impl AgentMemoryStoragePort for SledAgentMemoryStorage {
    fn get(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageError> {
        let _permit = self.acquire_request()?;
        if !self.validator().valid_key(&request.key) {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        let _snapshot = self
            .snapshot_lock
            .lock()
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        let (value, generation) = self
            .backend
            .get_snapshot(namespace.as_bytes(), &request.key)?;
        Ok(AgentMemoryGetResult { value, generation })
    }

    fn scan(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageError> {
        let _permit = self.acquire_request()?;
        let validator = self.validator();
        if !validator.valid_key(&request.prefix)
            || request
                .cursor
                .as_deref()
                .is_some_and(|key| !validator.valid_key(key))
            || request.limit == 0
            || request.limit > self.bounds.max_scan_items
        {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        let _snapshot = self
            .snapshot_lock
            .lock()
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        let snapshot = self.backend.scan_snapshot(
            namespace.as_bytes(),
            &request.prefix,
            request.cursor.as_deref(),
            request.limit as usize,
        )?;
        Ok(AgentMemoryScanResult {
            entries: snapshot.entries,
            next_cursor: snapshot.next_cursor,
            generation: snapshot.generation,
        })
    }

    fn transact(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageError> {
        let _permit = self.acquire_request()?;
        if request.conditions.len() > self.bounds.max_transaction_conditions
            || request.mutations.is_empty()
            || request.mutations.len() > self.bounds.max_transaction_mutations
        {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        self.validator().checked_transaction_bytes(&request)?;
        let _snapshot = self
            .snapshot_lock
            .lock()
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        if let Some(barriers) = &self.transaction_barriers {
            barriers.before_transaction_entered.wait();
            barriers.before_transaction_release.wait();
        }
        match self.backend.transact(namespace.as_bytes(), &request)? {
            BackendTransactionOutcome::Conflict => Ok(AgentMemoryTransactionOutcome::Conflict),
            BackendTransactionOutcome::Committed(generation) => {
                if let Some(barriers) = &self.transaction_barriers {
                    barriers.before_flush_entered.wait();
                    barriers.before_flush_release.wait();
                }
                self.backend.flush()?;
                Ok(AgentMemoryTransactionOutcome::Committed {
                    generation,
                    durable: true,
                })
            }
        }
    }
}

#[derive(Debug, Default)]
struct NamespaceState {
    values: BTreeMap<Vec<u8>, Vec<u8>>,
    generation: u64,
}

struct RequestPermit<'a> {
    outstanding_requests: &'a AtomicUsize,
}

impl Drop for RequestPermit<'_> {
    fn drop(&mut self) {
        self.outstanding_requests.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct RequestBarrier {
    entered: std::sync::Arc<std::sync::Barrier>,
    release: std::sync::Arc<std::sync::Barrier>,
}

impl InMemoryAgentMemoryStorage {
    pub fn new(bounds: AgentMemoryStorageBounds) -> Self {
        Self {
            bounds,
            state: Mutex::new(HashMap::new()),
            outstanding_requests: AtomicUsize::new(0),
            #[cfg(test)]
            request_barrier: None,
        }
    }

    fn acquire_request(&self) -> Result<RequestPermit<'_>, AgentMemoryStorageError> {
        self.outstanding_requests
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |current| {
                (current < self.bounds.max_outstanding_requests).then_some(current + 1)
            })
            .map_err(|_| AgentMemoryStorageError::BoundsExceeded)?;
        #[cfg(test)]
        if let Some(barrier) = &self.request_barrier {
            barrier.entered.wait();
            barrier.release.wait();
        }
        Ok(RequestPermit {
            outstanding_requests: &self.outstanding_requests,
        })
    }

    fn valid_key(&self, key: &[u8]) -> bool {
        key.len() <= self.bounds.max_key_bytes
    }

    fn checked_transaction_bytes(
        &self,
        request: &AgentMemoryTransactionRequest,
    ) -> Result<usize, AgentMemoryStorageError> {
        let mut total = 0usize;
        let mut mutation_keys = HashSet::with_capacity(request.mutations.len());
        for condition in &request.conditions {
            let (key, value) = match condition {
                AgentMemoryCondition::KeyAbsent { key } => (key.as_slice(), None),
                AgentMemoryCondition::ValueEquals { key, value } => {
                    (key.as_slice(), Some(value.as_slice()))
                }
            };
            if !self.valid_key(key)
                || value.is_some_and(|value| value.len() > self.bounds.max_value_bytes)
            {
                return Err(AgentMemoryStorageError::BoundsExceeded);
            }
            total = total
                .checked_add(1)
                .and_then(|size| size.checked_add(key.len()))
                .and_then(|size| {
                    value
                        .and_then(|value| size.checked_add(value.len()))
                        .or(Some(size))
                })
                .ok_or(AgentMemoryStorageError::BoundsExceeded)?;
        }
        for mutation in &request.mutations {
            let (key, value) = match mutation {
                AgentMemoryMutation::Put { key, value } => (key.as_slice(), Some(value.as_slice())),
                AgentMemoryMutation::Delete { key } => (key.as_slice(), None),
            };
            if !self.valid_key(key)
                || value.is_some_and(|value| value.len() > self.bounds.max_value_bytes)
                || !mutation_keys.insert(key)
            {
                return Err(AgentMemoryStorageError::BoundsExceeded);
            }
            total = total
                .checked_add(1)
                .and_then(|size| size.checked_add(key.len()))
                .and_then(|size| {
                    value
                        .and_then(|value| size.checked_add(value.len()))
                        .or(Some(size))
                })
                .ok_or(AgentMemoryStorageError::BoundsExceeded)?;
        }
        if total > self.bounds.max_transaction_bytes {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        Ok(total)
    }

    #[cfg(test)]
    fn with_request_barrier(bounds: AgentMemoryStorageBounds) -> (Self, RequestBarrier) {
        let barrier = RequestBarrier {
            entered: std::sync::Arc::new(std::sync::Barrier::new(2)),
            release: std::sync::Arc::new(std::sync::Barrier::new(2)),
        };
        let mut storage = Self::new(bounds);
        storage.request_barrier = Some(barrier.clone());
        (storage, barrier)
    }
}

impl AgentMemoryStoragePort for InMemoryAgentMemoryStorage {
    fn get(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageError> {
        let _permit = self.acquire_request()?;
        if !self.valid_key(&request.key) {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        let state = self
            .state
            .lock()
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        let namespace = state.get(namespace);
        Ok(AgentMemoryGetResult {
            value: namespace.and_then(|state| state.values.get(&request.key).cloned()),
            generation: namespace.map_or(0, |state| state.generation),
        })
    }

    fn scan(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageError> {
        let _permit = self.acquire_request()?;
        if !self.valid_key(&request.prefix)
            || request
                .cursor
                .as_deref()
                .is_some_and(|key| !self.valid_key(key))
            || request.limit == 0
            || request.limit > self.bounds.max_scan_items
        {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        let state = self
            .state
            .lock()
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        let Some(namespace) = state.get(namespace) else {
            return Ok(AgentMemoryScanResult {
                entries: Vec::new(),
                next_cursor: None,
                generation: 0,
            });
        };
        let mut matching = namespace.values.iter().filter(|(key, _)| {
            key.starts_with(&request.prefix)
                && request.cursor.as_ref().is_none_or(|cursor| *key > cursor)
        });
        let mut entries = matching
            .by_ref()
            .take(request.limit as usize)
            .map(|(key, value)| AgentMemoryKeyValue {
                key: key.clone(),
                value: value.clone(),
            })
            .collect::<Vec<_>>();
        let next_cursor = matching
            .next()
            .and_then(|_| entries.last().map(|entry| entry.key.clone()));
        Ok(AgentMemoryScanResult {
            entries: std::mem::take(&mut entries),
            next_cursor,
            generation: namespace.generation,
        })
    }

    fn transact(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageError> {
        let _permit = self.acquire_request()?;
        if request.conditions.len() > self.bounds.max_transaction_conditions
            || request.mutations.is_empty()
            || request.mutations.len() > self.bounds.max_transaction_mutations
        {
            return Err(AgentMemoryStorageError::BoundsExceeded);
        }
        self.checked_transaction_bytes(&request)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AgentMemoryStorageError::Unavailable)?;
        let namespace = state.entry(namespace.clone()).or_default();
        let conditions_match = request.conditions.iter().all(|condition| match condition {
            AgentMemoryCondition::KeyAbsent { key } => !namespace.values.contains_key(key),
            AgentMemoryCondition::ValueEquals { key, value } => {
                namespace.values.get(key) == Some(value)
            }
        });
        if !conditions_match {
            return Ok(AgentMemoryTransactionOutcome::Conflict);
        }
        let generation = namespace
            .generation
            .checked_add(1)
            .ok_or(AgentMemoryStorageError::BoundsExceeded)?;
        for mutation in request.mutations {
            match mutation {
                AgentMemoryMutation::Put { key, value } => {
                    namespace.values.insert(key, value);
                }
                AgentMemoryMutation::Delete { key } => {
                    namespace.values.remove(&key);
                }
            }
        }
        namespace.generation = generation;
        Ok(AgentMemoryTransactionOutcome::Committed {
            generation,
            durable: false,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests;

/// Opt-in process-test observer around the real adapter. It records only
/// operation names, never keys, values, or errors. Absent from release builds.
#[cfg(debug_assertions)]
pub(crate) struct ProcessStorageSpy {
    inner: std::sync::Arc<dyn AgentMemoryStoragePort>,
    trace: std::path::PathBuf,
    lock: Mutex<()>,
}

#[cfg(debug_assertions)]
impl ProcessStorageSpy {
    pub(crate) fn wrap(
        inner: std::sync::Arc<dyn AgentMemoryStoragePort>,
    ) -> std::sync::Arc<dyn AgentMemoryStoragePort> {
        match std::env::var_os("TOWER_TEST_AGENT_MEMORY_ACCESS_TRACE") {
            Some(trace) => std::sync::Arc::new(Self {
                inner,
                trace: trace.into(),
                lock: Mutex::new(()),
            }),
            None => inner,
        }
    }
    fn record(&self, operation: &str) {
        use std::io::Write;
        let _guard = self.lock.lock().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.trace)
            .expect("process storage trace");
        writeln!(file, "{operation}").unwrap();
    }
}

#[cfg(debug_assertions)]
impl AgentMemoryStoragePort for ProcessStorageSpy {
    fn get(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageError> {
        self.record("get");
        self.inner.get(namespace, request)
    }
    fn scan(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageError> {
        self.record("scan");
        self.inner.scan(namespace, request)
    }
    fn transact(
        &self,
        namespace: &AgentMemoryNamespace,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageError> {
        self.record("transact");
        self.inner.transact(namespace, request)
    }
}
