use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::adapters::daemon::lifecycle::CapabilityDependency;
use crate::adapters::daemon::publication::{
    CommittedEventBatch, CommittedEventPublisher, PublicationCut, SnapshotEntry,
};
use crate::domain::extension_host::ExtensionId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatchUpError {
    UnknownExtension { extension_id: ExtensionId },
    DuplicateEpoch { extension_id: ExtensionId },
    ClosedEpoch { extension_id: ExtensionId },
    InvalidSequence,
    Cancelled { extension_id: ExtensionId },
    DeliveryFailed { extension_id: ExtensionId },
    FenceMismatch { extension_id: ExtensionId },
    FenceUnsupported { extension_id: ExtensionId },
    Overflow { extension_id: ExtensionId },
    BarrierClosed { extension_id: ExtensionId },
}

/// Runtime catch-up errors retain identity and a fixed public failure class.
/// Backend text is deliberately discarded at this adapter boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatchUpFailure {
    pub extension_id: ExtensionId,
    pub error: CatchUpError,
    pub failure_class: super::lifecycle::StartupFailureClass,
}
impl CatchUpFailure {
    pub fn protocol(extension_id: ExtensionId, error: CatchUpError) -> Self {
        let failure_class = if matches!(error, CatchUpError::Cancelled { .. }) {
            super::lifecycle::StartupFailureClass::Cancelled
        } else {
            super::lifecycle::StartupFailureClass::ExtensionProtocol
        };
        Self {
            extension_id,
            error,
            failure_class,
        }
    }
    pub fn delivery(extension_id: ExtensionId, fault: &extension_protocol::ExtensionFault) -> Self {
        use super::lifecycle::StartupFailureClass;
        let failure_class = match fault {
            extension_protocol::ExtensionFault::Timeout => StartupFailureClass::ExtensionTimeout,
            extension_protocol::ExtensionFault::Crashed { .. } => {
                StartupFailureClass::ExtensionCrash
            }
            _ => StartupFailureClass::ExtensionProtocol,
        };
        Self {
            error: CatchUpError::DeliveryFailed {
                extension_id: extension_id.clone(),
            },
            extension_id,
            failure_class,
        }
    }
    pub fn wire_fault(&self) -> extension_protocol::ExtensionFault {
        use super::lifecycle::StartupFailureClass;
        match self.failure_class {
            StartupFailureClass::ExtensionTimeout => extension_protocol::ExtensionFault::Timeout,
            StartupFailureClass::ExtensionCrash => {
                extension_protocol::ExtensionFault::Crashed { code: None }
            }
            _ => extension_protocol::ExtensionFault::ProtocolError {
                message: "extension catch-up failed".into(),
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExtensionDependencyPolicy {
    dependencies: HashMap<ExtensionId, CapabilityDependency>,
    readiness_controls: HashMap<ExtensionId, HashSet<String>>,
}

impl ExtensionDependencyPolicy {
    pub fn new(
        dependencies: HashMap<ExtensionId, CapabilityDependency>,
        readiness_controls: HashMap<ExtensionId, HashSet<String>>,
    ) -> Self {
        Self {
            dependencies,
            readiness_controls,
        }
    }
    pub fn dependency_for(&self, extension_id: &ExtensionId) -> Option<CapabilityDependency> {
        self.dependencies.get(extension_id).copied()
    }
    pub fn is_readiness_control(&self, extension_id: &ExtensionId, tool_name: &str) -> bool {
        self.readiness_controls
            .get(extension_id)
            .is_some_and(|controls| controls.contains(tool_name))
    }

    fn has_explicit_controls(&self, extension_id: &ExtensionId) -> bool {
        self.readiness_controls.contains_key(extension_id)
    }
}

#[derive(Clone, Debug)]
pub struct CatchUpEpoch {
    extension_id: ExtensionId,
    epoch: u64,
}

impl CatchUpEpoch {
    pub fn extension_id(&self) -> &ExtensionId {
        &self.extension_id
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

#[derive(Debug)]
struct EpochState {
    epoch: u64,
    watermark: u64,
    snapshot: Vec<SnapshotEntry>,
    batches: VecDeque<CommittedEventBatch>,
    delivered_through: u64,
    delivered_events: u64,
    ready: bool,
}

#[derive(Debug, Default)]
struct CoordinatorState {
    closed: bool,
    last_accepted_sequence: u64,
    epochs: HashMap<ExtensionId, EpochState>,
    closed_extensions: HashSet<ExtensionId>,
}

/// Owns replay queues and finite cuts. It never invokes child code while its lock is held.
pub struct ExtensionCatchUpCoordinator {
    publisher: Arc<CommittedEventPublisher>,
    dependency_policy: Arc<ExtensionDependencyPolicy>,
    state: Mutex<CoordinatorState>,
}

impl ExtensionCatchUpCoordinator {
    pub fn new(
        publisher: Arc<CommittedEventPublisher>,
        dependency_policy: Arc<ExtensionDependencyPolicy>,
    ) -> Self {
        Self {
            publisher,
            dependency_policy,
            state: Mutex::new(CoordinatorState::default()),
        }
    }

    pub fn begin(
        &self,
        extension_id: ExtensionId,
        snapshot: Vec<SnapshotEntry>,
    ) -> Result<CatchUpEpoch, CatchUpError> {
        if self
            .dependency_policy
            .dependency_for(&extension_id)
            .is_none()
        {
            return Err(CatchUpError::UnknownExtension { extension_id });
        }
        let watermark = self
            .publisher
            .capture_cut()
            .map_err(|_| CatchUpError::BarrierClosed {
                extension_id: extension_id.clone(),
            })?
            .sequence;
        let mut state = self.state.lock().expect("catch-up state poisoned");
        if state.closed {
            return Err(CatchUpError::BarrierClosed { extension_id });
        }
        if state.closed_extensions.contains(&extension_id) {
            return Err(CatchUpError::ClosedEpoch { extension_id });
        }
        if state.epochs.contains_key(&extension_id) {
            return Err(CatchUpError::DuplicateEpoch { extension_id });
        }
        let epoch = 1;
        state.epochs.insert(
            extension_id.clone(),
            EpochState {
                epoch,
                watermark,
                snapshot,
                batches: VecDeque::new(),
                delivered_through: watermark,
                delivered_events: 0,
                ready: false,
            },
        );
        Ok(CatchUpEpoch {
            extension_id,
            epoch,
        })
    }

    pub fn accept_committed_batch(&self, batch: CommittedEventBatch) -> Result<(), CatchUpError> {
        let mut state = self.state.lock().expect("catch-up state poisoned");
        if state.closed {
            return Err(CatchUpError::InvalidSequence);
        }
        let expected = state
            .last_accepted_sequence
            .checked_add(1)
            .ok_or(CatchUpError::InvalidSequence)?;
        if batch.sequence != expected {
            return Err(CatchUpError::InvalidSequence);
        }
        state.last_accepted_sequence = batch.sequence;
        for epoch in state.epochs.values_mut() {
            if !epoch.ready && batch.sequence > epoch.watermark {
                epoch.batches.push_back(batch.clone());
            }
        }
        Ok(())
    }

    pub fn deliver_through(
        &self,
        epoch: &CatchUpEpoch,
        cut: PublicationCut,
    ) -> Result<(), CatchUpError> {
        let mut state = self.state.lock().expect("catch-up state poisoned");
        let last_accepted = state.last_accepted_sequence;
        let current =
            state
                .epochs
                .get_mut(&epoch.extension_id)
                .ok_or_else(|| CatchUpError::ClosedEpoch {
                    extension_id: epoch.extension_id.clone(),
                })?;
        if current.epoch != epoch.epoch {
            return Err(CatchUpError::ClosedEpoch {
                extension_id: epoch.extension_id.clone(),
            });
        }
        if cut.sequence < current.delivered_through || cut.sequence > last_accepted {
            return Err(CatchUpError::InvalidSequence);
        }
        if current.delivered_events == 0 {
            current.delivered_events =
                u64::try_from(current.snapshot.len()).map_err(|_| CatchUpError::Overflow {
                    extension_id: epoch.extension_id.clone(),
                })?;
        }
        while current
            .batches
            .front()
            .is_some_and(|batch| batch.sequence <= cut.sequence)
        {
            let batch = current.batches.pop_front().expect("front checked");
            current.delivered_events = current
                .delivered_events
                .checked_add(batch.events.len() as u64)
                .ok_or_else(|| CatchUpError::Overflow {
                    extension_id: epoch.extension_id.clone(),
                })?;
            current.delivered_through = batch.sequence;
        }
        if current.delivered_through < cut.sequence {
            return Err(CatchUpError::InvalidSequence);
        }
        Ok(())
    }

    pub fn publish_ready(&self, epoch: CatchUpEpoch) -> Result<(), CatchUpError> {
        let mut state = self.state.lock().expect("catch-up state poisoned");
        let current =
            state
                .epochs
                .get_mut(&epoch.extension_id)
                .ok_or_else(|| CatchUpError::ClosedEpoch {
                    extension_id: epoch.extension_id.clone(),
                })?;
        if current.epoch != epoch.epoch {
            return Err(CatchUpError::FenceMismatch {
                extension_id: epoch.extension_id,
            });
        }
        let implicit_builtin_fence = matches!(epoch.extension_id.as_str(), "ast" | "lsp")
            && (!self
                .dependency_policy
                .has_explicit_controls(&epoch.extension_id)
                || current.delivered_events > 0);
        if !implicit_builtin_fence
            && !self
                .dependency_policy
                .is_readiness_control(&epoch.extension_id, "_tower_catch_up_fence")
        {
            return Err(CatchUpError::FenceUnsupported {
                extension_id: epoch.extension_id,
            });
        }
        current.ready = true;
        Ok(())
    }

    pub fn cancel(&self, epoch: CatchUpEpoch) -> Result<(), CatchUpError> {
        let mut state = self.state.lock().expect("catch-up state poisoned");
        let Some(current) = state.epochs.get(&epoch.extension_id) else {
            return Err(CatchUpError::ClosedEpoch {
                extension_id: epoch.extension_id,
            });
        };
        if current.epoch != epoch.epoch {
            return Err(CatchUpError::ClosedEpoch {
                extension_id: epoch.extension_id,
            });
        }
        state.epochs.remove(&epoch.extension_id);
        state.closed_extensions.insert(epoch.extension_id);
        Ok(())
    }

    pub fn close(&self) {
        self.state.lock().expect("catch-up state poisoned").closed = true;
    }
}
