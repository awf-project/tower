use std::sync::Mutex;
use std::time::Duration;

use crate::adapters::daemon::lifecycle::{StartupFailureClass, StartupPhase};
use crate::adapters::daemon::wire::{
    ExtensionReplayMetrics, StartupInstrumentationSnapshot, StartupPhaseTiming,
};
use crate::domain::extension_host::ExtensionId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstrumentationError {
    DuplicatePhase(StartupPhase),
    DuplicateExtensionReplay(ExtensionId),
    DurationOverflow,
}

impl std::fmt::Display for InstrumentationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicatePhase(phase) => write!(formatter, "duplicate startup phase: {phase:?}"),
            Self::DuplicateExtensionReplay(extension_id) => {
                write!(formatter, "duplicate extension replay: {extension_id}")
            }
            Self::DurationOverflow => {
                formatter.write_str("startup duration exceeds u64 milliseconds")
            }
        }
    }
}

impl std::error::Error for InstrumentationError {}

pub struct StartupInstrumentation {
    snapshot: Mutex<StartupInstrumentationSnapshot>,
}

impl StartupInstrumentation {
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(StartupInstrumentationSnapshot::default()),
        }
    }

    pub fn record_phase(
        &self,
        phase: StartupPhase,
        duration: Duration,
    ) -> Result<StartupInstrumentationSnapshot, InstrumentationError> {
        let duration_ms = checked_duration_ms(duration)?;
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if snapshot.phases.iter().any(|timing| timing.phase == phase) {
            return Err(InstrumentationError::DuplicatePhase(phase));
        }
        snapshot
            .phases
            .push(StartupPhaseTiming { phase, duration_ms });
        Ok(sorted_snapshot(&snapshot))
    }

    pub fn record_extension_replay(
        &self,
        extension_id: ExtensionId,
        event_count: u64,
        duration: Duration,
        failure_class: Option<StartupFailureClass>,
    ) -> Result<StartupInstrumentationSnapshot, InstrumentationError> {
        let duration_ms = checked_duration_ms(duration)?;
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if snapshot
            .extension_replays
            .iter()
            .any(|metrics| metrics.extension_id == extension_id)
        {
            return Err(InstrumentationError::DuplicateExtensionReplay(extension_id));
        }
        snapshot.extension_replays.push(ExtensionReplayMetrics {
            extension_id,
            event_count,
            duration_ms,
            failure_class,
        });
        Ok(sorted_snapshot(&snapshot))
    }

    #[must_use]
    pub fn snapshot(&self) -> StartupInstrumentationSnapshot {
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        sorted_snapshot(&snapshot)
    }
}

fn checked_duration_ms(duration: Duration) -> Result<u64, InstrumentationError> {
    u64::try_from(duration.as_millis()).map_err(|_| InstrumentationError::DurationOverflow)
}

fn sorted_snapshot(snapshot: &StartupInstrumentationSnapshot) -> StartupInstrumentationSnapshot {
    let mut snapshot = snapshot.clone();
    snapshot.phases.sort_by_key(|timing| timing.phase);
    snapshot
        .extension_replays
        .sort_by(|left, right| left.extension_id.as_str().cmp(right.extension_id.as_str()));
    snapshot
}

impl Default for StartupInstrumentation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_instrumentation_empty_snapshot_is_stable() {
        let instrumentation = StartupInstrumentation::new();

        assert_eq!(instrumentation.snapshot(), Default::default());
        assert_eq!(instrumentation.snapshot(), instrumentation.snapshot());
    }

    #[test]
    fn startup_instrumentation_records_phase_once_in_startup_order() {
        let instrumentation = StartupInstrumentation::new();

        let first = instrumentation
            .record_phase(
                StartupPhase::WorkspaceReconciling,
                Duration::from_millis(17),
            )
            .expect("record completed reconciliation phase");
        let second = instrumentation
            .record_phase(StartupPhase::TransportBound, Duration::from_micros(2_999))
            .expect("record completed transport phase");

        assert_eq!(
            first.phases,
            vec![StartupPhaseTiming {
                phase: StartupPhase::WorkspaceReconciling,
                duration_ms: 17,
            }]
        );
        assert_eq!(
            second.phases,
            vec![
                StartupPhaseTiming {
                    phase: StartupPhase::TransportBound,
                    duration_ms: 2,
                },
                StartupPhaseTiming {
                    phase: StartupPhase::WorkspaceReconciling,
                    duration_ms: 17,
                },
            ]
        );
    }

    #[test]
    fn startup_instrumentation_records_extension_replay_once_by_extension_id() {
        let instrumentation = StartupInstrumentation::new();

        instrumentation
            .record_extension_replay(
                ExtensionId::new("zeta"),
                9,
                Duration::from_millis(31),
                Some(StartupFailureClass::ExtensionProtocol),
            )
            .expect("record failed replay at its finite cut");
        let snapshot = instrumentation
            .record_extension_replay(ExtensionId::new("alpha"), 4, Duration::from_millis(7), None)
            .expect("record successful replay at its finite cut");

        assert_eq!(
            snapshot.extension_replays,
            vec![
                ExtensionReplayMetrics {
                    extension_id: ExtensionId::new("alpha"),
                    event_count: 4,
                    duration_ms: 7,
                    failure_class: None,
                },
                ExtensionReplayMetrics {
                    extension_id: ExtensionId::new("zeta"),
                    event_count: 9,
                    duration_ms: 31,
                    failure_class: Some(StartupFailureClass::ExtensionProtocol),
                },
            ]
        );
    }

    #[test]
    fn startup_instrumentation_rejects_duplicate_phase_without_mutation() {
        let instrumentation = StartupInstrumentation::new();
        let expected = instrumentation
            .record_phase(StartupPhase::StorageOpening, Duration::from_millis(5))
            .expect("record storage completion");

        assert_eq!(
            instrumentation.record_phase(StartupPhase::StorageOpening, Duration::from_millis(999),),
            Err(InstrumentationError::DuplicatePhase(
                StartupPhase::StorageOpening
            ))
        );
        assert_eq!(instrumentation.snapshot(), expected);
    }

    #[test]
    fn startup_instrumentation_rejects_duplicate_extension_without_mutation() {
        let instrumentation = StartupInstrumentation::new();
        let extension_id = ExtensionId::new("slow_start");
        let expected = instrumentation
            .record_extension_replay(extension_id.clone(), 3, Duration::from_millis(11), None)
            .expect("record replay completion");

        assert_eq!(
            instrumentation.record_extension_replay(
                extension_id.clone(),
                500,
                Duration::from_millis(999),
                Some(StartupFailureClass::ExtensionCrash),
            ),
            Err(InstrumentationError::DuplicateExtensionReplay(extension_id))
        );
        assert_eq!(instrumentation.snapshot(), expected);
    }

    #[test]
    fn startup_instrumentation_rejects_duration_overflow_without_mutation() {
        let instrumentation = StartupInstrumentation::new();
        let overflow = Duration::from_secs(u64::MAX);

        assert_eq!(
            instrumentation.record_phase(StartupPhase::TransportBound, overflow),
            Err(InstrumentationError::DurationOverflow)
        );
        assert_eq!(instrumentation.snapshot(), Default::default());
        assert_eq!(
            instrumentation.record_extension_replay(
                ExtensionId::new("slow_start"),
                1,
                overflow,
                Some(StartupFailureClass::ExtensionTimeout),
            ),
            Err(InstrumentationError::DurationOverflow)
        );
        assert_eq!(instrumentation.snapshot(), Default::default());
    }
}
