use super::delivery::ExtensionDelivery;
use crate::domain::ExtensionId;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::domain::{FileId, RelativePath};

#[derive(Clone, Debug)]
pub enum CommittedFileEvent {
    Indexed { file_id: FileId, path: RelativePath },
    Changed { file_id: FileId, path: RelativePath },
    Deleted { path: RelativePath },
}

#[derive(Clone, Debug)]
pub struct CommittedEventBatch {
    pub sequence: u64,
    pub events: Vec<CommittedFileEvent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationCut {
    pub sequence: u64,
}

#[derive(Clone, Debug)]
pub struct SnapshotEntry {
    pub file_id: FileId,
    pub path: RelativePath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationError {
    Closed,
    Overflow,
}

#[derive(Debug)]
struct PublicationState {
    closed: bool,
    next_sequence: u64,
    committed_sequence: u64,
    reservations: BTreeMap<u64, usize>,
}

/// Contention evidence contains durations/counters only, never workspace data.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct PublicationMetrics {
    pub acquisitions: u64,
    pub wait_ns: u64,
    pub held_ns: u64,
    pub max_held_ns: u64,
}

pub(crate) struct CommitGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    publisher: &'a CommittedEventPublisher,
    acquired: Instant,
}
impl Drop for CommitGuard<'_> {
    fn drop(&mut self) {
        let elapsed = self.acquired.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.publisher.held_ns.fetch_add(elapsed, Ordering::Relaxed);
        self.publisher
            .max_held_ns
            .fetch_max(elapsed, Ordering::Relaxed);
    }
}

/// Generation-local ordering gate for successfully committed file-event batches.
/// Reservation happens before durable mutation; publication happens only after commit.
pub struct CommittedEventPublisher {
    generation: u64,
    cancelled: std::sync::atomic::AtomicBool,
    commit_gate: Mutex<()>,
    acquisitions: AtomicU64,
    wait_ns: AtomicU64,
    held_ns: AtomicU64,
    max_held_ns: AtomicU64,
    subscribers: Mutex<HashMap<ExtensionId, Arc<ExtensionDelivery>>>,
    state: Mutex<PublicationState>,
}

impl CommittedEventPublisher {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            cancelled: std::sync::atomic::AtomicBool::new(false),
            commit_gate: Mutex::new(()),
            acquisitions: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            held_ns: AtomicU64::new(0),
            max_held_ns: AtomicU64::new(0),
            subscribers: Mutex::new(HashMap::new()),
            state: Mutex::new(PublicationState {
                closed: false,
                next_sequence: 1,
                committed_sequence: 0,
                reservations: BTreeMap::new(),
            }),
        }
    }

    pub(crate) fn commit_lock(&self) -> CommitGuard<'_> {
        let started = Instant::now();
        let guard = self.commit_gate.lock().unwrap_or_else(|p| p.into_inner());
        self.wait_ns.fetch_add(
            started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.acquisitions.fetch_add(1, Ordering::Relaxed);
        CommitGuard {
            _guard: guard,
            publisher: self,
            acquired: Instant::now(),
        }
    }

    pub fn metrics(&self) -> PublicationMetrics {
        PublicationMetrics {
            acquisitions: self.acquisitions.load(Ordering::Relaxed),
            wait_ns: self.wait_ns.load(Ordering::Relaxed),
            held_ns: self.held_ns.load(Ordering::Relaxed),
            max_held_ns: self.max_held_ns.load(Ordering::Relaxed),
        }
    }

    /// Must be called under commit_lock before touching durable state.
    pub(crate) fn check_capacity(&self) -> Result<(), PublicationError> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return Err(PublicationError::Closed);
        }
        state
            .next_sequence
            .checked_add(1)
            .ok_or(PublicationError::Overflow)?;
        state
            .committed_sequence
            .checked_add(1)
            .ok_or(PublicationError::Overflow)?;
        Ok(())
    }

    pub(crate) fn commit_events(
        &self,
        events: Vec<CommittedFileEvent>,
    ) -> Result<(), PublicationError> {
        if events.is_empty() {
            return Ok(());
        }
        let reservation = self.reserve_batch(events.len())?;
        self.publish(reservation, events)?;
        Ok(())
    }

    pub(crate) fn subscribe(
        &self,
        id: ExtensionId,
        delivery: Arc<ExtensionDelivery>,
    ) -> Result<(), super::catch_up::CatchUpError> {
        use super::catch_up::CatchUpError;
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            delivery.cancel();
            return Err(CatchUpError::BarrierClosed { extension_id: id });
        }
        let mut subscribers = self.subscribers.lock().unwrap_or_else(|p| p.into_inner());
        if subscribers.contains_key(&id) {
            return Err(CatchUpError::DuplicateEpoch { extension_id: id });
        }
        subscribers.insert(id, delivery);
        Ok(())
    }

    pub(crate) fn require_delivery(
        &self,
        id: &ExtensionId,
    ) -> Result<Arc<ExtensionDelivery>, super::catch_up::CatchUpError> {
        self.delivery(id)
            .ok_or_else(|| super::catch_up::CatchUpError::UnknownExtension {
                extension_id: id.clone(),
            })
    }

    pub(crate) fn delivery(&self, id: &ExtensionId) -> Option<Arc<ExtensionDelivery>> {
        self.subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }

    pub(crate) fn cancel_deliveries(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        for delivery in self
            .subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            delivery.cancel();
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn reserve_batch(&self, event_count: usize) -> Result<u64, PublicationError> {
        let count = u64::try_from(event_count).map_err(|_| PublicationError::Overflow)?;
        let mut state = self.state.lock().expect("publication state poisoned");
        if state.closed {
            return Err(PublicationError::Closed);
        }
        // Leave room for the exclusive end ordinal; generation identity is
        // independent of generation-local sequence/ordinal capacity.
        count.checked_add(1).ok_or(PublicationError::Overflow)?;
        let sequence = state.next_sequence;
        state.next_sequence = sequence.checked_add(1).ok_or(PublicationError::Overflow)?;
        state.reservations.insert(sequence, event_count);
        Ok(sequence)
    }

    pub fn publish(
        &self,
        reservation: u64,
        events: Vec<CommittedFileEvent>,
    ) -> Result<CommittedEventBatch, PublicationError> {
        let mut state = self.state.lock().expect("publication state poisoned");
        if state.closed {
            return Err(PublicationError::Closed);
        }
        let Some(expected_count) = state.reservations.remove(&reservation) else {
            return Err(PublicationError::Closed);
        };
        if expected_count != events.len()
            || state
                .reservations
                .first_key_value()
                .is_some_and(|(&earlier, _)| earlier < reservation)
        {
            state.reservations.insert(reservation, expected_count);
            return Err(PublicationError::Closed);
        }
        let sequence = state
            .committed_sequence
            .checked_add(1)
            .ok_or(PublicationError::Overflow)?;
        state.committed_sequence = sequence;
        let batch = CommittedEventBatch { sequence, events };
        for subscriber in self
            .subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            subscriber.enqueue(batch.clone());
        }
        Ok(batch)
    }

    /// Records a reserved mutation that did not commit. Aborted reservations
    /// never consume a journal sequence and therefore cannot create gaps for
    /// catch-up subscribers.
    pub fn abort(&self, reservation: u64) -> Result<(), PublicationError> {
        let mut state = self.state.lock().expect("publication state poisoned");
        if state.closed {
            return Err(PublicationError::Closed);
        }
        state
            .reservations
            .remove(&reservation)
            .map(|_| ())
            .ok_or(PublicationError::Closed)
    }

    pub fn capture_cut(&self) -> Result<PublicationCut, PublicationError> {
        let state = self.state.lock().expect("publication state poisoned");
        if state.closed {
            return Err(PublicationError::Closed);
        }
        Ok(PublicationCut {
            sequence: state.committed_sequence,
        })
    }

    pub fn close(&self) {
        self.state
            .lock()
            .expect("publication state poisoned")
            .closed = true;
    }
}

/// Mutation callbacks are data collection only. The producer publishes the
/// successful outcomes before releasing its authoritative commit guards.
#[derive(Default)]
pub(crate) struct BufferedEvents(Mutex<Vec<CommittedFileEvent>>);
impl BufferedEvents {
    pub(crate) fn take(&self) -> Vec<CommittedFileEvent> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}
impl crate::ports::ExtensionHostPort for BufferedEvents {
    fn on_file_indexed(&self, file_id: FileId, path: &RelativePath) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(CommittedFileEvent::Indexed {
                file_id,
                path: path.clone(),
            });
    }
    fn on_file_changed(&self, file_id: FileId, path: &RelativePath) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(CommittedFileEvent::Changed {
                file_id,
                path: path.clone(),
            });
    }
    fn on_file_deleted(&self, path: &RelativePath) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(CommittedFileEvent::Deleted { path: path.clone() });
    }
    fn declared_tools(&self) -> Vec<(ExtensionId, extension_protocol::ToolDecl)> {
        Vec::new()
    }
    fn invoke(
        &self,
        name: &str,
        _: serde_json::Value,
        _: &dyn crate::domain::CancellationSignal,
    ) -> Result<serde_json::Value, crate::domain::InvokeError> {
        Err(crate::domain::InvokeError::ToolNotFound(name.to_owned()))
    }
}

pub(crate) fn snapshot_events(
    before: &crate::domain::workspace::WorkspaceSnapshot,
    after: &crate::domain::workspace::WorkspaceSnapshot,
) -> Vec<CommittedFileEvent> {
    let before: BTreeMap<_, _> = before.files.iter().map(|file| (&file.path, file)).collect();
    let after: BTreeMap<_, _> = after.files.iter().map(|file| (&file.path, file)).collect();
    let mut events = Vec::new();
    for path in before.keys() {
        if !after.contains_key(path) {
            events.push(CommittedFileEvent::Deleted {
                path: (*path).clone(),
            });
        }
    }
    for (path, file) in &after {
        if before.get(path).is_none_or(|old| **old != **file) {
            events.push(CommittedFileEvent::Changed {
                file_id: file.id,
                path: (*path).clone(),
            });
        }
    }
    events
}
