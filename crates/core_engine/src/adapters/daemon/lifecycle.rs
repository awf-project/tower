use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::domain::extension_host::ExtensionId;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupPhase {
    OwnershipAcquired,
    TransportBound,
    StorageOpening,
    WorkspaceReconciling,
    WatcherCatchingUp,
    ExtensionsDiscovering,
    CapabilitiesCatchingUp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupFailureClass {
    Ownership,
    Storage,
    Reconciliation,
    Watcher,
    ExtensionTimeout,
    ExtensionCrash,
    ExtensionProtocol,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DaemonLifecycle {
    Starting { phase: StartupPhase },
    Ready,
    Degraded { failure_class: StartupFailureClass },
    Failed { failure_class: StartupFailureClass },
    Stopping,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDependency {
    Workspace,
    Ast,
    Lsp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapabilityReadiness {
    Preparing { phase: StartupPhase },
    Ready,
    Degraded { failure_class: StartupFailureClass },
    Failed { failure_class: StartupFailureClass },
    Unavailable,
    Stopping,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionReadiness {
    pub extension_id: ExtensionId,
    pub dependency: CapabilityDependency,
    pub readiness: CapabilityReadiness,
}

#[derive(Deserialize, Serialize)]
struct ExtensionReadinessWire {
    extension_id: String,
    dependency: CapabilityDependency,
    readiness: CapabilityReadiness,
}

impl Serialize for ExtensionReadiness {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ExtensionReadinessWire {
            extension_id: self.extension_id.as_str().to_owned(),
            dependency: self.dependency,
            readiness: self.readiness.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExtensionReadiness {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ExtensionReadinessWire::deserialize(deserializer)?;
        Ok(Self {
            extension_id: ExtensionId::new(wire.extension_id),
            dependency: wire.dependency,
            readiness: wire.readiness,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadinessSnapshot {
    pub generation: u64,
    pub lifecycle: DaemonLifecycle,
    pub workspace: CapabilityReadiness,
    pub ast: CapabilityReadiness,
    pub lsp: CapabilityReadiness,
    pub extensions: Vec<ExtensionReadiness>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleTransitionError {
    NonMonotonicPhase,
    InvalidDaemonTransition,
    InvalidCapabilityTransition,
    DuplicateExtension,
    UnknownExtension,
    ExtensionDependencyMismatch,
    RegistrationClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadinessWaitError {
    Timeout {
        dependency: CapabilityDependency,
        phase: StartupPhase,
    },
    Failed {
        dependency: CapabilityDependency,
        failure_class: StartupFailureClass,
    },
    Unavailable {
        dependency: CapabilityDependency,
    },
    Stopping {
        dependency: CapabilityDependency,
    },
}

pub struct DispatchPermit {
    snapshot: ReadinessSnapshot,
    shared: Arc<SharedState>,
}

impl DispatchPermit {
    pub fn snapshot(&self) -> &ReadinessSnapshot {
        &self.snapshot
    }
}

impl Drop for DispatchPermit {
    fn drop(&mut self) {
        let mut state = lock_state(&self.shared.state);
        debug_assert!(state.in_flight > 0);
        state.in_flight = state.in_flight.saturating_sub(1);
        self.shared.changed.notify_all();
    }
}

struct ControllerState {
    snapshot: ReadinessSnapshot,
    discovery_closed: bool,
    in_flight: usize,
    #[cfg(test)]
    readiness_waiters: usize,
}

struct SharedState {
    state: Mutex<ControllerState>,
    changed: Condvar,
    async_changed: tokio::sync::watch::Sender<()>,
}

pub struct ReadinessController {
    shared: Arc<SharedState>,
}

fn lock_state(state: &Mutex<ControllerState>) -> MutexGuard<'_, ControllerState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ReadinessController {
    pub fn new(generation: u64) -> Self {
        let initial = StartupPhase::OwnershipAcquired;
        Self {
            shared: Arc::new(SharedState {
                state: Mutex::new(ControllerState {
                    snapshot: ReadinessSnapshot {
                        generation,
                        lifecycle: DaemonLifecycle::Starting { phase: initial },
                        workspace: CapabilityReadiness::Preparing { phase: initial },
                        ast: CapabilityReadiness::Preparing { phase: initial },
                        lsp: CapabilityReadiness::Preparing { phase: initial },
                        extensions: Vec::new(),
                    },
                    discovery_closed: false,
                    in_flight: 0,
                    #[cfg(test)]
                    readiness_waiters: 0,
                }),
                changed: Condvar::new(),
                async_changed: tokio::sync::watch::channel(()).0,
            }),
        }
    }

    pub fn register_extension(
        &self,
        extension_id: ExtensionId,
        dependency: CapabilityDependency,
    ) -> Result<ReadinessSnapshot, LifecycleTransitionError> {
        let mut state = lock_state(&self.shared.state);
        if dependency == CapabilityDependency::Workspace {
            return Err(LifecycleTransitionError::ExtensionDependencyMismatch);
        }
        if let Some(existing) = state
            .snapshot
            .extensions
            .iter()
            .find(|extension| extension.extension_id == extension_id)
        {
            return Err(if existing.dependency == dependency {
                LifecycleTransitionError::DuplicateExtension
            } else {
                LifecycleTransitionError::ExtensionDependencyMismatch
            });
        }
        if state.discovery_closed {
            return Err(LifecycleTransitionError::RegistrationClosed);
        }
        let readiness = capability(&state.snapshot, dependency).clone();
        if !matches!(readiness, CapabilityReadiness::Preparing { .. }) {
            return Err(LifecycleTransitionError::InvalidCapabilityTransition);
        }
        state.snapshot.extensions.push(ExtensionReadiness {
            extension_id,
            dependency,
            readiness,
        });
        state
            .snapshot
            .extensions
            .sort_by(|left, right| left.extension_id.as_str().cmp(right.extension_id.as_str()));
        publish(&self.shared, &state)
    }

    pub fn close_discovery(&self) -> Result<ReadinessSnapshot, LifecycleTransitionError> {
        let mut state = lock_state(&self.shared.state);
        if !state.discovery_closed {
            state.discovery_closed = true;
            for dependency in [CapabilityDependency::Ast, CapabilityDependency::Lsp] {
                if !has_members(&state.snapshot, dependency) {
                    let readiness = capability_mut(&mut state.snapshot, dependency);
                    if matches!(readiness, CapabilityReadiness::Preparing { .. }) {
                        *readiness = CapabilityReadiness::Unavailable;
                    }
                }
            }
            recompute_daemon(&mut state);
            self.shared.changed.notify_all();
        }
        Ok(state.snapshot.clone())
    }

    /// Sticky observation: a failure published before subscription is still visible.
    pub(crate) async fn wait_for_workspace_failure(&self) {
        let mut changed = self.shared.async_changed.subscribe();
        loop {
            if matches!(
                self.snapshot().workspace,
                CapabilityReadiness::Failed { .. }
            ) {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn snapshot(&self) -> ReadinessSnapshot {
        lock_state(&self.shared.state).snapshot.clone()
    }

    pub fn transition_daemon(
        &self,
        lifecycle: DaemonLifecycle,
    ) -> Result<ReadinessSnapshot, LifecycleTransitionError> {
        let mut state = lock_state(&self.shared.state);
        if lifecycle == state.snapshot.lifecycle {
            return Ok(state.snapshot.clone());
        }
        if lifecycle == DaemonLifecycle::Ready && !ready_prerequisites(&state) {
            return Err(LifecycleTransitionError::InvalidDaemonTransition);
        }
        validate_daemon_transition(&state.snapshot.lifecycle, &lifecycle)?;
        state.snapshot.lifecycle = lifecycle.clone();
        if lifecycle == DaemonLifecycle::Stopping {
            state.snapshot.workspace = CapabilityReadiness::Stopping;
            state.snapshot.ast = CapabilityReadiness::Stopping;
            state.snapshot.lsp = CapabilityReadiness::Stopping;
            for extension in &mut state.snapshot.extensions {
                extension.readiness = CapabilityReadiness::Stopping;
            }
        }
        publish(&self.shared, &state)
    }

    pub fn transition_capability(
        &self,
        dependency: CapabilityDependency,
        readiness: CapabilityReadiness,
    ) -> Result<ReadinessSnapshot, LifecycleTransitionError> {
        let mut state = lock_state(&self.shared.state);
        let current = capability(&state.snapshot, dependency);
        if current == &readiness {
            return Ok(state.snapshot.clone());
        }
        if dependency != CapabilityDependency::Workspace {
            if has_members(&state.snapshot, dependency) {
                return Err(LifecycleTransitionError::InvalidCapabilityTransition);
            }
            if state.discovery_closed {
                if !matches!(readiness, CapabilityReadiness::Stopping) {
                    return Err(LifecycleTransitionError::InvalidCapabilityTransition);
                }
            } else if !matches!(
                readiness,
                CapabilityReadiness::Preparing { .. } | CapabilityReadiness::Stopping
            ) {
                return Err(LifecycleTransitionError::InvalidCapabilityTransition);
            }
        } else if readiness == CapabilityReadiness::Unavailable {
            return Err(LifecycleTransitionError::InvalidCapabilityTransition);
        }
        validate_capability_transition(current, &readiness, false)?;
        *capability_mut(&mut state.snapshot, dependency) = readiness;
        recompute_daemon(&mut state);
        publish(&self.shared, &state)
    }

    pub fn transition_extension(
        &self,
        extension_id: &ExtensionId,
        readiness: CapabilityReadiness,
    ) -> Result<ReadinessSnapshot, LifecycleTransitionError> {
        if readiness == CapabilityReadiness::Unavailable {
            return Err(LifecycleTransitionError::InvalidCapabilityTransition);
        }
        let mut state = lock_state(&self.shared.state);
        let Some(index) = state
            .snapshot
            .extensions
            .iter()
            .position(|extension| &extension.extension_id == extension_id)
        else {
            return Err(LifecycleTransitionError::UnknownExtension);
        };
        validate_capability_transition(
            &state.snapshot.extensions[index].readiness,
            &readiness,
            false,
        )?;
        let dependency = state.snapshot.extensions[index].dependency;
        state.snapshot.extensions[index].readiness = readiness;
        let aggregate = aggregate_family(&state.snapshot, dependency);
        *capability_mut(&mut state.snapshot, dependency) = aggregate;
        recompute_daemon(&mut state);
        publish(&self.shared, &state)
    }

    pub fn wait_until_ready(
        &self,
        dependency: CapabilityDependency,
        deadline: Duration,
    ) -> Result<ReadinessSnapshot, ReadinessWaitError> {
        let end = Instant::now().checked_add(deadline);
        let mut state = lock_state(&self.shared.state);
        loop {
            match wait_outcome(&state.snapshot, dependency) {
                Ok(()) => return Ok(state.snapshot.clone()),
                Err(WaitStatus::Terminal(error)) => return Err(error),
                Err(WaitStatus::Preparing(phase)) => {
                    let Some(remaining) = remaining(end) else {
                        return Err(ReadinessWaitError::Timeout { dependency, phase });
                    };
                    let waited = self
                        .shared
                        .changed
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state = waited.0;
                    if waited.1.timed_out()
                        && let Err(WaitStatus::Preparing(phase)) =
                            wait_outcome(&state.snapshot, dependency)
                    {
                        return Err(ReadinessWaitError::Timeout { dependency, phase });
                    }
                }
            }
        }
    }

    pub fn admit_dispatch(
        &self,
        dependencies: &[CapabilityDependency],
        deadline: Duration,
    ) -> Result<DispatchPermit, ReadinessWaitError> {
        let mut dependencies = dependencies.to_vec();
        dependencies.sort_unstable();
        dependencies.dedup();
        let stopping_dependency = dependencies
            .first()
            .copied()
            .unwrap_or(CapabilityDependency::Workspace);
        let end = Instant::now().checked_add(deadline);
        let mut state = lock_state(&self.shared.state);
        loop {
            if state.snapshot.lifecycle == DaemonLifecycle::Stopping {
                return Err(ReadinessWaitError::Stopping {
                    dependency: stopping_dependency,
                });
            }
            let mut waiting = None;
            for dependency in &dependencies {
                match wait_outcome(&state.snapshot, *dependency) {
                    Ok(()) => {}
                    Err(WaitStatus::Terminal(error)) => return Err(error),
                    Err(WaitStatus::Preparing(phase)) => {
                        waiting.get_or_insert((*dependency, phase));
                    }
                }
            }
            let Some((dependency, phase)) = waiting else {
                state.in_flight += 1;
                return Ok(DispatchPermit {
                    snapshot: state.snapshot.clone(),
                    shared: Arc::clone(&self.shared),
                });
            };
            let Some(remaining) = remaining(end) else {
                return Err(ReadinessWaitError::Timeout { dependency, phase });
            };
            #[cfg(test)]
            {
                state.readiness_waiters += 1;
                self.shared.changed.notify_all();
            }
            let waited = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
            #[cfg(test)]
            {
                state.readiness_waiters -= 1;
                self.shared.changed.notify_all();
            }
        }
    }

    #[cfg(test)]
    fn wait_for_readiness_waiter(&self, deadline: Duration) -> bool {
        let end = Instant::now().checked_add(deadline);
        let mut state = lock_state(&self.shared.state);
        while state.readiness_waiters == 0 {
            let Some(remaining) = remaining(end) else {
                return false;
            };
            let waited = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
            if waited.1.timed_out() && state.readiness_waiters == 0 {
                return false;
            }
        }
        true
    }

    pub fn wait_for_dispatch_drain(&self, deadline: Duration) -> bool {
        let end = Instant::now().checked_add(deadline);
        let mut state = lock_state(&self.shared.state);
        while state.in_flight != 0 {
            let Some(remaining) = remaining(end) else {
                return false;
            };
            let waited = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
            if waited.1.timed_out() && state.in_flight != 0 {
                return false;
            }
        }
        true
    }
}

fn publish(
    shared: &SharedState,
    state: &ControllerState,
) -> Result<ReadinessSnapshot, LifecycleTransitionError> {
    let snapshot = state.snapshot.clone();
    shared.changed.notify_all();
    shared.async_changed.send_replace(());
    Ok(snapshot)
}

fn capability(
    snapshot: &ReadinessSnapshot,
    dependency: CapabilityDependency,
) -> &CapabilityReadiness {
    match dependency {
        CapabilityDependency::Workspace => &snapshot.workspace,
        CapabilityDependency::Ast => &snapshot.ast,
        CapabilityDependency::Lsp => &snapshot.lsp,
    }
}

fn capability_mut(
    snapshot: &mut ReadinessSnapshot,
    dependency: CapabilityDependency,
) -> &mut CapabilityReadiness {
    match dependency {
        CapabilityDependency::Workspace => &mut snapshot.workspace,
        CapabilityDependency::Ast => &mut snapshot.ast,
        CapabilityDependency::Lsp => &mut snapshot.lsp,
    }
}

fn has_members(snapshot: &ReadinessSnapshot, dependency: CapabilityDependency) -> bool {
    snapshot
        .extensions
        .iter()
        .any(|extension| extension.dependency == dependency)
}

fn validate_daemon_transition(
    current: &DaemonLifecycle,
    next: &DaemonLifecycle,
) -> Result<(), LifecycleTransitionError> {
    if let (DaemonLifecycle::Starting { phase: from }, DaemonLifecycle::Starting { phase: to }) =
        (current, next)
    {
        return if to >= from {
            Ok(())
        } else {
            Err(LifecycleTransitionError::NonMonotonicPhase)
        };
    }
    let valid = matches!(
        (current, next),
        (
            DaemonLifecycle::Starting { .. },
            DaemonLifecycle::Ready
                | DaemonLifecycle::Degraded { .. }
                | DaemonLifecycle::Failed { .. }
                | DaemonLifecycle::Stopping
        ) | (
            DaemonLifecycle::Ready,
            DaemonLifecycle::Degraded { .. }
                | DaemonLifecycle::Failed { .. }
                | DaemonLifecycle::Stopping
        ) | (
            DaemonLifecycle::Degraded { .. },
            DaemonLifecycle::Ready | DaemonLifecycle::Failed { .. } | DaemonLifecycle::Stopping
        ) | (DaemonLifecycle::Failed { .. }, DaemonLifecycle::Stopping)
    );
    if valid {
        Ok(())
    } else {
        Err(LifecycleTransitionError::InvalidDaemonTransition)
    }
}

fn validate_capability_transition(
    current: &CapabilityReadiness,
    next: &CapabilityReadiness,
    allow_unavailable: bool,
) -> Result<(), LifecycleTransitionError> {
    if current == next {
        return Ok(());
    }
    if let (
        CapabilityReadiness::Preparing { phase: from },
        CapabilityReadiness::Preparing { phase: to },
    ) = (current, next)
    {
        return if to >= from {
            Ok(())
        } else {
            Err(LifecycleTransitionError::NonMonotonicPhase)
        };
    }
    let valid = matches!(
        (current, next),
        (
            CapabilityReadiness::Preparing { .. },
            CapabilityReadiness::Ready
                | CapabilityReadiness::Degraded { .. }
                | CapabilityReadiness::Failed { .. }
                | CapabilityReadiness::Stopping
        ) | (
            CapabilityReadiness::Ready,
            CapabilityReadiness::Degraded { .. }
                | CapabilityReadiness::Failed { .. }
                | CapabilityReadiness::Stopping
        ) | (
            CapabilityReadiness::Degraded { .. },
            CapabilityReadiness::Ready
                | CapabilityReadiness::Failed { .. }
                | CapabilityReadiness::Stopping
        ) | (
            CapabilityReadiness::Failed { .. },
            CapabilityReadiness::Stopping
        ) | (
            CapabilityReadiness::Unavailable,
            CapabilityReadiness::Stopping
        )
    ) || (allow_unavailable
        && matches!(
            (current, next),
            (
                CapabilityReadiness::Preparing { .. },
                CapabilityReadiness::Unavailable
            )
        ));
    if valid {
        Ok(())
    } else {
        Err(LifecycleTransitionError::InvalidCapabilityTransition)
    }
}

fn aggregate_family(
    snapshot: &ReadinessSnapshot,
    dependency: CapabilityDependency,
) -> CapabilityReadiness {
    let members: Vec<_> = snapshot
        .extensions
        .iter()
        .filter(|extension| extension.dependency == dependency)
        .collect();
    if members
        .iter()
        .any(|member| member.readiness == CapabilityReadiness::Stopping)
    {
        return CapabilityReadiness::Stopping;
    }
    for severity in [2_u8, 1] {
        if let Some(member) = members.iter().find(|member| {
            matches!(
                (&member.readiness, severity),
                (CapabilityReadiness::Failed { .. }, 2) | (CapabilityReadiness::Degraded { .. }, 1)
            )
        }) {
            return member.readiness.clone();
        }
    }
    if let Some(phase) = members
        .iter()
        .filter_map(|member| match member.readiness {
            CapabilityReadiness::Preparing { phase } => Some(phase),
            _ => None,
        })
        .min()
    {
        return CapabilityReadiness::Preparing { phase };
    }
    CapabilityReadiness::Ready
}

fn ready_prerequisites(state: &ControllerState) -> bool {
    state.discovery_closed
        && state.snapshot.workspace == CapabilityReadiness::Ready
        && family_ready_or_absent(&state.snapshot.ast)
        && family_ready_or_absent(&state.snapshot.lsp)
}

fn family_ready_or_absent(readiness: &CapabilityReadiness) -> bool {
    matches!(
        readiness,
        CapabilityReadiness::Ready | CapabilityReadiness::Unavailable
    )
}

fn recompute_daemon(state: &mut ControllerState) {
    if matches!(
        state.snapshot.lifecycle,
        DaemonLifecycle::Stopping | DaemonLifecycle::Failed { .. }
    ) {
        return;
    }
    if let CapabilityReadiness::Failed { failure_class }
    | CapabilityReadiness::Degraded { failure_class } = state.snapshot.workspace
    {
        state.snapshot.lifecycle = DaemonLifecycle::Failed { failure_class };
        return;
    }
    if state.snapshot.workspace == CapabilityReadiness::Stopping {
        state.snapshot.lifecycle = DaemonLifecycle::Stopping;
        return;
    }
    if [&state.snapshot.ast, &state.snapshot.lsp]
        .into_iter()
        .any(|readiness| *readiness == CapabilityReadiness::Stopping)
    {
        state.snapshot.lifecycle = DaemonLifecycle::Stopping;
        return;
    }
    if state.discovery_closed && state.snapshot.workspace == CapabilityReadiness::Ready {
        for readiness in [&state.snapshot.ast, &state.snapshot.lsp] {
            if let CapabilityReadiness::Failed { failure_class }
            | CapabilityReadiness::Degraded { failure_class } = readiness
            {
                state.snapshot.lifecycle = DaemonLifecycle::Degraded {
                    failure_class: *failure_class,
                };
                return;
            }
        }
        if ready_prerequisites(state) {
            state.snapshot.lifecycle = DaemonLifecycle::Ready;
        }
    }
}

enum WaitStatus {
    Preparing(StartupPhase),
    Terminal(ReadinessWaitError),
}

fn wait_outcome(
    snapshot: &ReadinessSnapshot,
    dependency: CapabilityDependency,
) -> Result<(), WaitStatus> {
    if snapshot.lifecycle == DaemonLifecycle::Stopping {
        return Err(WaitStatus::Terminal(ReadinessWaitError::Stopping {
            dependency,
        }));
    }
    match capability(snapshot, dependency) {
        CapabilityReadiness::Ready => Ok(()),
        CapabilityReadiness::Preparing { phase } => Err(WaitStatus::Preparing(*phase)),
        CapabilityReadiness::Degraded { failure_class }
        | CapabilityReadiness::Failed { failure_class } => {
            Err(WaitStatus::Terminal(ReadinessWaitError::Failed {
                dependency,
                failure_class: *failure_class,
            }))
        }
        CapabilityReadiness::Unavailable => {
            Err(WaitStatus::Terminal(ReadinessWaitError::Unavailable {
                dependency,
            }))
        }
        CapabilityReadiness::Stopping => Err(WaitStatus::Terminal(ReadinessWaitError::Stopping {
            dependency,
        })),
    }
}

fn remaining(end: Option<Instant>) -> Option<Duration> {
    let end = end?;
    end.checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;

    use serde_json::json;

    use super::*;

    const INITIAL_PHASE: StartupPhase = StartupPhase::OwnershipAcquired;

    fn preparing(phase: StartupPhase) -> CapabilityReadiness {
        CapabilityReadiness::Preparing { phase }
    }

    fn starting(phase: StartupPhase) -> DaemonLifecycle {
        DaemonLifecycle::Starting { phase }
    }

    fn assert_admission_error(
        result: Result<DispatchPermit, ReadinessWaitError>,
        expected: ReadinessWaitError,
    ) {
        match result {
            Err(actual) => assert_eq!(actual, expected),
            Ok(_) => std::panic::panic_any("dispatch was admitted unexpectedly"),
        }
    }

    fn transition_workspace_to_ready(controller: &ReadinessController) {
        controller
            .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
            .unwrap();
    }

    fn controller_with_ready_families() -> ReadinessController {
        let controller = ReadinessController::new(7);
        transition_workspace_to_ready(&controller);
        controller
            .register_extension(ExtensionId::new("ast"), CapabilityDependency::Ast)
            .unwrap();
        controller
            .register_extension(ExtensionId::new("lsp"), CapabilityDependency::Lsp)
            .unwrap();
        controller.close_discovery().unwrap();
        controller
            .transition_extension(&ExtensionId::new("ast"), CapabilityReadiness::Ready)
            .unwrap();
        controller
            .transition_extension(&ExtensionId::new("lsp"), CapabilityReadiness::Ready)
            .unwrap();
        controller
    }

    #[test]
    fn daemon_lifecycle_new_publishes_initial_generation_and_phases() {
        let controller = ReadinessController::new(41);
        let snapshot = controller.snapshot();

        assert_eq!(snapshot.generation, 41);
        assert_eq!(snapshot.lifecycle, starting(INITIAL_PHASE));
        assert_eq!(snapshot.workspace, preparing(INITIAL_PHASE));
        assert_eq!(snapshot.ast, preparing(INITIAL_PHASE));
        assert_eq!(snapshot.lsp, preparing(INITIAL_PHASE));
        assert!(snapshot.extensions.is_empty());
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({
                "generation": 41,
                "lifecycle": {"state": "starting", "phase": "ownership_acquired"},
                "workspace": {"state": "preparing", "phase": "ownership_acquired"},
                "ast": {"state": "preparing", "phase": "ownership_acquired"},
                "lsp": {"state": "preparing", "phase": "ownership_acquired"},
                "extensions": [],
            })
        );

        let owned_snapshot = snapshot;
        controller
            .transition_capability(
                CapabilityDependency::Workspace,
                preparing(StartupPhase::StorageOpening),
            )
            .unwrap();
        assert_eq!(owned_snapshot.workspace, preparing(INITIAL_PHASE));
    }

    #[test]
    fn daemon_lifecycle_daemon_legal_transitions_are_monotonic_and_idempotent() {
        let phases = [
            StartupPhase::TransportBound,
            StartupPhase::StorageOpening,
            StartupPhase::WorkspaceReconciling,
            StartupPhase::WatcherCatchingUp,
            StartupPhase::ExtensionsDiscovering,
            StartupPhase::CapabilitiesCatchingUp,
        ];
        let controller = ReadinessController::new(1);
        for phase in phases {
            let expected = starting(phase);
            assert_eq!(
                controller
                    .transition_daemon(expected.clone())
                    .unwrap()
                    .lifecycle,
                expected
            );
        }
        let current = starting(StartupPhase::CapabilitiesCatchingUp);
        assert_eq!(
            controller
                .transition_daemon(current.clone())
                .unwrap()
                .lifecycle,
            current
        );

        for terminal in [
            DaemonLifecycle::Degraded {
                failure_class: StartupFailureClass::ExtensionCrash,
            },
            DaemonLifecycle::Failed {
                failure_class: StartupFailureClass::Storage,
            },
            DaemonLifecycle::Stopping,
        ] {
            let controller = ReadinessController::new(2);
            assert_eq!(
                controller
                    .transition_daemon(terminal.clone())
                    .unwrap()
                    .lifecycle,
                terminal
            );
        }

        let ready = controller_with_ready_families();
        assert_eq!(
            ready
                .transition_daemon(DaemonLifecycle::Ready)
                .unwrap()
                .lifecycle,
            DaemonLifecycle::Ready
        );
        assert_eq!(
            ready
                .transition_daemon(DaemonLifecycle::Ready)
                .unwrap()
                .lifecycle,
            DaemonLifecycle::Ready
        );
        assert!(
            ready
                .transition_daemon(DaemonLifecycle::Degraded {
                    failure_class: StartupFailureClass::ExtensionTimeout,
                })
                .is_ok()
        );
        assert!(ready.transition_daemon(DaemonLifecycle::Ready).is_ok());
        assert!(
            ready
                .transition_daemon(DaemonLifecycle::Failed {
                    failure_class: StartupFailureClass::ExtensionProtocol,
                })
                .is_ok()
        );
        assert!(ready.transition_daemon(DaemonLifecycle::Stopping).is_ok());
    }

    #[test]
    fn daemon_lifecycle_rejects_non_monotonic_phase_without_mutation() {
        let controller = ReadinessController::new(3);
        controller
            .transition_daemon(starting(StartupPhase::StorageOpening))
            .unwrap();
        let before = controller.snapshot();
        assert_eq!(
            controller.transition_daemon(starting(StartupPhase::TransportBound)),
            Err(LifecycleTransitionError::NonMonotonicPhase)
        );
        assert_eq!(controller.snapshot(), before);

        controller
            .transition_capability(
                CapabilityDependency::Workspace,
                preparing(StartupPhase::StorageOpening),
            )
            .unwrap();
        let before = controller.snapshot();
        assert_eq!(
            controller.transition_capability(
                CapabilityDependency::Workspace,
                preparing(StartupPhase::TransportBound),
            ),
            Err(LifecycleTransitionError::NonMonotonicPhase)
        );
        assert_eq!(controller.snapshot(), before);
    }

    #[test]
    fn daemon_lifecycle_rejects_every_illegal_daemon_transition_without_mutation() {
        let cases = [
            (
                DaemonLifecycle::Ready,
                starting(StartupPhase::TransportBound),
            ),
            (
                DaemonLifecycle::Degraded {
                    failure_class: StartupFailureClass::ExtensionCrash,
                },
                DaemonLifecycle::Degraded {
                    failure_class: StartupFailureClass::ExtensionTimeout,
                },
            ),
            (
                DaemonLifecycle::Failed {
                    failure_class: StartupFailureClass::Storage,
                },
                DaemonLifecycle::Ready,
            ),
            (DaemonLifecycle::Stopping, DaemonLifecycle::Ready),
        ];

        for (from, to) in cases {
            let controller = controller_with_ready_families();
            controller.transition_daemon(from).unwrap();
            let before = controller.snapshot();
            assert_eq!(
                controller.transition_daemon(to),
                Err(LifecycleTransitionError::InvalidDaemonTransition)
            );
            assert_eq!(controller.snapshot(), before);
        }

        let controller = ReadinessController::new(4);
        let before = controller.snapshot();
        assert_eq!(
            controller.transition_daemon(DaemonLifecycle::Ready),
            Err(LifecycleTransitionError::InvalidDaemonTransition)
        );
        assert_eq!(controller.snapshot(), before);
    }

    #[test]
    fn daemon_lifecycle_capability_legal_transitions_are_monotonic_and_idempotent() {
        let controller = ReadinessController::new(5);
        for dependency in [
            CapabilityDependency::Workspace,
            CapabilityDependency::Ast,
            CapabilityDependency::Lsp,
        ] {
            for phase in [
                StartupPhase::TransportBound,
                StartupPhase::StorageOpening,
                StartupPhase::WorkspaceReconciling,
                StartupPhase::WatcherCatchingUp,
                StartupPhase::ExtensionsDiscovering,
                StartupPhase::CapabilitiesCatchingUp,
            ] {
                let state = preparing(phase);
                let snapshot = controller
                    .transition_capability(dependency, state.clone())
                    .unwrap();
                assert_eq!(
                    match dependency {
                        CapabilityDependency::Workspace => snapshot.workspace,
                        CapabilityDependency::Ast => snapshot.ast,
                        CapabilityDependency::Lsp => snapshot.lsp,
                    },
                    state
                );
            }
        }
        assert!(
            controller
                .transition_capability(
                    CapabilityDependency::Workspace,
                    preparing(StartupPhase::CapabilitiesCatchingUp),
                )
                .is_ok()
        );

        for target in [
            CapabilityReadiness::Ready,
            CapabilityReadiness::Degraded {
                failure_class: StartupFailureClass::Reconciliation,
            },
            CapabilityReadiness::Failed {
                failure_class: StartupFailureClass::Storage,
            },
            CapabilityReadiness::Stopping,
        ] {
            let controller = ReadinessController::new(6);
            assert!(
                controller
                    .transition_capability(CapabilityDependency::Workspace, target.clone())
                    .is_ok()
            );
            assert!(
                controller
                    .transition_capability(CapabilityDependency::Workspace, target)
                    .is_ok()
            );
        }
    }

    #[test]
    fn daemon_lifecycle_rejects_every_illegal_capability_transition_without_mutation() {
        let controller = ReadinessController::new(7);
        for dependency in [CapabilityDependency::Ast, CapabilityDependency::Lsp] {
            for illegal in [CapabilityReadiness::Ready, CapabilityReadiness::Unavailable] {
                let before = controller.snapshot();
                assert_eq!(
                    controller.transition_capability(dependency, illegal),
                    Err(LifecycleTransitionError::InvalidCapabilityTransition)
                );
                assert_eq!(controller.snapshot(), before);
            }
        }
        let before = controller.snapshot();
        assert_eq!(
            controller.transition_capability(
                CapabilityDependency::Workspace,
                CapabilityReadiness::Unavailable,
            ),
            Err(LifecycleTransitionError::InvalidCapabilityTransition)
        );
        assert_eq!(controller.snapshot(), before);

        controller
            .register_extension(ExtensionId::new("ast"), CapabilityDependency::Ast)
            .unwrap();
        let before = controller.snapshot();
        assert_eq!(
            controller.transition_capability(CapabilityDependency::Ast, CapabilityReadiness::Ready),
            Err(LifecycleTransitionError::InvalidCapabilityTransition)
        );
        assert_eq!(controller.snapshot(), before);

        controller.close_discovery().unwrap();
        let before = controller.snapshot();
        assert_eq!(
            controller.transition_capability(
                CapabilityDependency::Lsp,
                preparing(StartupPhase::CapabilitiesCatchingUp),
            ),
            Err(LifecycleTransitionError::InvalidCapabilityTransition)
        );
        assert_eq!(controller.snapshot(), before);
    }

    #[test]
    fn daemon_lifecycle_registers_extensions_once_in_lexical_snapshot_order() {
        let controller = ReadinessController::new(8);
        controller
            .transition_capability(
                CapabilityDependency::Ast,
                preparing(StartupPhase::ExtensionsDiscovering),
            )
            .unwrap();
        for name in ["zeta", "alpha", "middle"] {
            controller
                .register_extension(ExtensionId::new(name), CapabilityDependency::Ast)
                .unwrap();
        }
        let snapshot = controller.snapshot();
        assert_eq!(
            snapshot
                .extensions
                .iter()
                .map(|extension| extension.extension_id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "middle", "zeta"]
        );
        assert!(snapshot.extensions.iter().all(|extension| {
            extension.dependency == CapabilityDependency::Ast
                && extension.readiness == preparing(StartupPhase::ExtensionsDiscovering)
        }));
    }

    #[test]
    fn daemon_lifecycle_rejects_duplicate_and_mismatched_extension_registration() {
        let controller = ReadinessController::new(9);
        let id = ExtensionId::new("semantic");
        controller
            .register_extension(id.clone(), CapabilityDependency::Ast)
            .unwrap();

        let before = controller.snapshot();
        assert_eq!(
            controller.register_extension(id.clone(), CapabilityDependency::Ast),
            Err(LifecycleTransitionError::DuplicateExtension)
        );
        assert_eq!(controller.snapshot(), before);
        assert_eq!(
            controller.register_extension(id, CapabilityDependency::Lsp),
            Err(LifecycleTransitionError::ExtensionDependencyMismatch)
        );
        assert_eq!(controller.snapshot(), before);
        assert_eq!(
            controller.register_extension(
                ExtensionId::new("workspace"),
                CapabilityDependency::Workspace,
            ),
            Err(LifecycleTransitionError::ExtensionDependencyMismatch)
        );
        assert_eq!(controller.snapshot(), before);

        controller.close_discovery().unwrap();
        let before = controller.snapshot();
        assert_eq!(
            controller.register_extension(ExtensionId::new("late"), CapabilityDependency::Lsp),
            Err(LifecycleTransitionError::RegistrationClosed)
        );
        assert_eq!(controller.snapshot(), before);
    }

    #[test]
    fn daemon_lifecycle_rejects_unknown_extension_transition_without_mutation() {
        let controller = ReadinessController::new(10);
        let before = controller.snapshot();
        assert_eq!(
            controller
                .transition_extension(&ExtensionId::new("missing"), CapabilityReadiness::Ready,),
            Err(LifecycleTransitionError::UnknownExtension)
        );
        assert_eq!(controller.snapshot(), before);
    }

    #[test]
    fn daemon_lifecycle_extension_transitions_recompute_capability_aggregate() {
        let controller = ReadinessController::new(11);
        for name in ["zeta", "alpha", "middle"] {
            controller
                .register_extension(ExtensionId::new(name), CapabilityDependency::Ast)
                .unwrap();
        }
        controller.close_discovery().unwrap();

        controller
            .transition_extension(
                &ExtensionId::new("zeta"),
                preparing(StartupPhase::CapabilitiesCatchingUp),
            )
            .unwrap();
        controller
            .transition_extension(
                &ExtensionId::new("alpha"),
                preparing(StartupPhase::ExtensionsDiscovering),
            )
            .unwrap();
        assert_eq!(
            controller.snapshot().ast,
            preparing(StartupPhase::OwnershipAcquired)
        );

        controller
            .transition_extension(&ExtensionId::new("middle"), CapabilityReadiness::Ready)
            .unwrap();
        controller
            .transition_extension(
                &ExtensionId::new("alpha"),
                CapabilityReadiness::Degraded {
                    failure_class: StartupFailureClass::ExtensionProtocol,
                },
            )
            .unwrap();
        controller
            .transition_extension(
                &ExtensionId::new("zeta"),
                CapabilityReadiness::Degraded {
                    failure_class: StartupFailureClass::ExtensionCrash,
                },
            )
            .unwrap();
        assert_eq!(
            controller.snapshot().ast,
            CapabilityReadiness::Degraded {
                failure_class: StartupFailureClass::ExtensionProtocol,
            }
        );

        controller
            .transition_extension(
                &ExtensionId::new("zeta"),
                CapabilityReadiness::Failed {
                    failure_class: StartupFailureClass::ExtensionTimeout,
                },
            )
            .unwrap();
        assert_eq!(
            controller.snapshot().ast,
            CapabilityReadiness::Failed {
                failure_class: StartupFailureClass::ExtensionTimeout,
            }
        );
        controller
            .transition_extension(&ExtensionId::new("alpha"), CapabilityReadiness::Stopping)
            .unwrap();
        assert_eq!(controller.snapshot().ast, CapabilityReadiness::Stopping);

        let empty = ReadinessController::new(12);
        let first = empty.close_discovery().unwrap();
        assert_eq!(first.ast, CapabilityReadiness::Unavailable);
        assert_eq!(first.lsp, CapabilityReadiness::Unavailable);
        assert_eq!(empty.close_discovery().unwrap(), first);

        // Exercise the instance transition graph through the public controller,
        // including the all-ready aggregation that makes an enabled family ready.
        for target in [
            CapabilityReadiness::Ready,
            CapabilityReadiness::Degraded {
                failure_class: StartupFailureClass::ExtensionCrash,
            },
            CapabilityReadiness::Failed {
                failure_class: StartupFailureClass::ExtensionTimeout,
            },
            CapabilityReadiness::Stopping,
        ] {
            let controller = ReadinessController::new(120);
            let id = ExtensionId::new("only-member");
            controller
                .register_extension(id.clone(), CapabilityDependency::Lsp)
                .unwrap();
            controller.close_discovery().unwrap();
            let before = controller.snapshot();
            controller
                .transition_extension(&id, target.clone())
                .unwrap();
            assert_ne!(controller.snapshot(), before);
            assert_eq!(
                controller
                    .transition_extension(&id, target.clone())
                    .unwrap()
                    .extensions[0]
                    .readiness,
                target
            );
        }

        let all_ready = ReadinessController::new(121);
        for name in ["ast-a", "ast-b"] {
            all_ready
                .register_extension(ExtensionId::new(name), CapabilityDependency::Ast)
                .unwrap();
        }
        all_ready.close_discovery().unwrap();
        all_ready
            .transition_extension(&ExtensionId::new("ast-a"), CapabilityReadiness::Ready)
            .unwrap();
        assert_ne!(all_ready.snapshot().ast, CapabilityReadiness::Ready);
        all_ready
            .transition_extension(&ExtensionId::new("ast-b"), CapabilityReadiness::Ready)
            .unwrap();
        assert_eq!(all_ready.snapshot().ast, CapabilityReadiness::Ready);

        let illegal_cases = [
            (
                CapabilityReadiness::Ready,
                preparing(StartupPhase::TransportBound),
            ),
            (
                CapabilityReadiness::Failed {
                    failure_class: StartupFailureClass::ExtensionCrash,
                },
                CapabilityReadiness::Ready,
            ),
            (CapabilityReadiness::Stopping, CapabilityReadiness::Ready),
            (
                CapabilityReadiness::Degraded {
                    failure_class: StartupFailureClass::ExtensionCrash,
                },
                CapabilityReadiness::Degraded {
                    failure_class: StartupFailureClass::ExtensionProtocol,
                },
            ),
        ];
        for (from, to) in illegal_cases {
            let controller = ReadinessController::new(122);
            let id = ExtensionId::new("transition-under-test");
            controller
                .register_extension(id.clone(), CapabilityDependency::Ast)
                .unwrap();
            controller.close_discovery().unwrap();
            controller.transition_extension(&id, from).unwrap();
            let before = controller.snapshot();
            assert_eq!(
                controller.transition_extension(&id, to),
                Err(LifecycleTransitionError::InvalidCapabilityTransition)
            );
            assert_eq!(controller.snapshot(), before);
        }
    }

    #[test]
    fn daemon_lifecycle_stopping_and_unavailable_never_project_ready() {
        let controller = ReadinessController::new(13);
        controller
            .register_extension(ExtensionId::new("ast-a"), CapabilityDependency::Ast)
            .unwrap();
        controller
            .register_extension(ExtensionId::new("ast-b"), CapabilityDependency::Ast)
            .unwrap();
        controller.close_discovery().unwrap();
        controller
            .transition_extension(&ExtensionId::new("ast-a"), CapabilityReadiness::Ready)
            .unwrap();
        controller
            .transition_extension(&ExtensionId::new("ast-b"), CapabilityReadiness::Stopping)
            .unwrap();
        assert_eq!(controller.snapshot().ast, CapabilityReadiness::Stopping);
        assert_eq!(controller.snapshot().lifecycle, DaemonLifecycle::Stopping);
        assert_eq!(controller.snapshot().lsp, CapabilityReadiness::Unavailable);

        assert_eq!(
            controller.wait_until_ready(CapabilityDependency::Ast, Duration::ZERO),
            Err(ReadinessWaitError::Stopping {
                dependency: CapabilityDependency::Ast,
            })
        );
        assert_eq!(
            controller.wait_until_ready(CapabilityDependency::Lsp, Duration::ZERO),
            Err(ReadinessWaitError::Stopping {
                dependency: CapabilityDependency::Lsp,
            })
        );

        let degraded = ReadinessController::new(14);
        degraded
            .register_extension(ExtensionId::new("lsp"), CapabilityDependency::Lsp)
            .unwrap();
        degraded.close_discovery().unwrap();
        degraded
            .transition_extension(
                &ExtensionId::new("lsp"),
                CapabilityReadiness::Degraded {
                    failure_class: StartupFailureClass::ExtensionCrash,
                },
            )
            .unwrap();
        assert_eq!(
            degraded.wait_until_ready(CapabilityDependency::Lsp, Duration::ZERO),
            Err(ReadinessWaitError::Failed {
                dependency: CapabilityDependency::Lsp,
                failure_class: StartupFailureClass::ExtensionCrash,
            })
        );
    }

    #[test]
    fn daemon_lifecycle_wait_returns_ready_without_holding_controller_lock() {
        let controller = Arc::new(ReadinessController::new(15));
        let waiter_started = Arc::new(Barrier::new(2));
        let waiter = {
            let controller = Arc::clone(&controller);
            let waiter_started = Arc::clone(&waiter_started);
            thread::spawn(move || {
                waiter_started.wait();
                controller.wait_until_ready(CapabilityDependency::Workspace, Duration::from_secs(2))
            })
        };
        waiter_started.wait();
        transition_workspace_to_ready(&controller);

        let snapshot = waiter.join().unwrap().unwrap();
        assert_eq!(snapshot.workspace, CapabilityReadiness::Ready);
        assert_eq!(snapshot.generation, 15);
    }

    #[test]
    fn daemon_lifecycle_wait_reports_timeout_failed_and_stopping_exactly() {
        let timeout = ReadinessController::new(16);
        assert_eq!(
            timeout.wait_until_ready(CapabilityDependency::Workspace, Duration::ZERO),
            Err(ReadinessWaitError::Timeout {
                dependency: CapabilityDependency::Workspace,
                phase: INITIAL_PHASE,
            })
        );

        let failed = ReadinessController::new(17);
        failed
            .transition_capability(
                CapabilityDependency::Workspace,
                CapabilityReadiness::Failed {
                    failure_class: StartupFailureClass::Storage,
                },
            )
            .unwrap();
        assert_eq!(
            failed.wait_until_ready(CapabilityDependency::Workspace, Duration::ZERO),
            Err(ReadinessWaitError::Failed {
                dependency: CapabilityDependency::Workspace,
                failure_class: StartupFailureClass::Storage,
            })
        );

        let absent = ReadinessController::new(18);
        absent.close_discovery().unwrap();
        assert_eq!(
            absent.wait_until_ready(CapabilityDependency::Ast, Duration::ZERO),
            Err(ReadinessWaitError::Unavailable {
                dependency: CapabilityDependency::Ast,
            })
        );
        assert_ne!(
            absent.snapshot().workspace,
            CapabilityReadiness::Unavailable
        );

        let stopping = Arc::new(ReadinessController::new(19));
        let waiter_started = Arc::new(Barrier::new(2));
        let waiter = {
            let stopping = Arc::clone(&stopping);
            let waiter_started = Arc::clone(&waiter_started);
            thread::spawn(move || {
                waiter_started.wait();
                stopping.wait_until_ready(CapabilityDependency::Workspace, Duration::from_secs(2))
            })
        };
        waiter_started.wait();
        stopping
            .transition_daemon(DaemonLifecycle::Stopping)
            .unwrap();
        assert_eq!(
            waiter.join().unwrap(),
            Err(ReadinessWaitError::Stopping {
                dependency: CapabilityDependency::Workspace,
            })
        );
    }

    #[test]
    fn daemon_lifecycle_admission_and_stopping_have_one_linearization_point() {
        let controller = Arc::new(controller_with_ready_families());
        controller
            .transition_daemon(DaemonLifecycle::Ready)
            .unwrap();
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let pre_stop_admission = {
            let controller = Arc::clone(&controller);
            thread::spawn(move || {
                let permit = controller
                    .admit_dispatch(&[CapabilityDependency::Workspace], Duration::from_secs(1))
                    .unwrap();
                admitted_tx.send(permit.snapshot().clone()).unwrap();
                release_rx.recv().unwrap();
                drop(permit);
            })
        };
        let admitted_snapshot = admitted_rx.recv().unwrap();
        assert_eq!(admitted_snapshot.workspace, CapabilityReadiness::Ready);

        controller
            .transition_daemon(DaemonLifecycle::Stopping)
            .unwrap();
        assert_admission_error(
            controller.admit_dispatch(&[CapabilityDependency::Workspace], Duration::ZERO),
            ReadinessWaitError::Stopping {
                dependency: CapabilityDependency::Workspace,
            },
        );
        assert_admission_error(
            controller.admit_dispatch(&[], Duration::ZERO),
            ReadinessWaitError::Stopping {
                dependency: CapabilityDependency::Workspace,
            },
        );
        assert!(!controller.wait_for_dispatch_drain(Duration::ZERO));
        release_tx.send(()).unwrap();
        pre_stop_admission.join().unwrap();
        assert!(controller.wait_for_dispatch_drain(Duration::ZERO));

        let stopping_first = Arc::new(controller_with_ready_families());
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let stopper = {
            let controller = Arc::clone(&stopping_first);
            thread::spawn(move || {
                controller
                    .transition_daemon(DaemonLifecycle::Stopping)
                    .unwrap();
                stopped_tx.send(()).unwrap();
            })
        };
        stopped_rx.recv().unwrap();
        assert_admission_error(
            stopping_first.admit_dispatch(&[CapabilityDependency::Lsp], Duration::from_secs(1)),
            ReadinessWaitError::Stopping {
                dependency: CapabilityDependency::Lsp,
            },
        );
        stopper.join().unwrap();
    }

    #[test]
    fn daemon_lifecycle_permit_drop_releases_inflight_without_holding_locks() {
        let controller = controller_with_ready_families();
        controller
            .transition_daemon(DaemonLifecycle::Ready)
            .unwrap();
        let permit = controller.admit_dispatch(
            &[CapabilityDependency::Workspace, CapabilityDependency::Ast],
            Duration::ZERO,
        );
        let permit = permit.unwrap();
        assert_eq!(permit.snapshot().ast, CapabilityReadiness::Ready);
        assert!(controller.snapshot().extensions.len() >= 2);
        assert!(!controller.wait_for_dispatch_drain(Duration::ZERO));
        drop(permit);
        assert!(controller.wait_for_dispatch_drain(Duration::ZERO));

        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _permit = controller.admit_dispatch(&[], Duration::ZERO).unwrap();
            std::panic::panic_any("exercise RAII release during unwinding");
        }));
        assert!(unwind.is_err());
        assert!(controller.wait_for_dispatch_drain(Duration::ZERO));
    }

    #[test]
    fn daemon_lifecycle_multi_dependency_admission_uses_one_deadline() {
        let controller = ReadinessController::new(20);
        controller
            .register_extension(ExtensionId::new("ast"), CapabilityDependency::Ast)
            .unwrap();
        controller
            .register_extension(ExtensionId::new("lsp"), CapabilityDependency::Lsp)
            .unwrap();
        controller.close_discovery().unwrap();
        controller
            .transition_extension(
                &ExtensionId::new("lsp"),
                CapabilityReadiness::Failed {
                    failure_class: StartupFailureClass::ExtensionProtocol,
                },
            )
            .unwrap();

        assert_admission_error(
            controller.admit_dispatch(
                &[
                    CapabilityDependency::Ast,
                    CapabilityDependency::Workspace,
                    CapabilityDependency::Lsp,
                    CapabilityDependency::Ast,
                ],
                Duration::ZERO,
            ),
            ReadinessWaitError::Failed {
                dependency: CapabilityDependency::Lsp,
                failure_class: StartupFailureClass::ExtensionProtocol,
            },
        );

        let preparing = Arc::new(ReadinessController::new(21));
        preparing
            .register_extension(ExtensionId::new("ast"), CapabilityDependency::Ast)
            .unwrap();
        preparing
            .register_extension(ExtensionId::new("lsp"), CapabilityDependency::Lsp)
            .unwrap();
        preparing.close_discovery().unwrap();
        transition_workspace_to_ready(&preparing);
        let (admission_tx, admission_rx) = mpsc::channel();
        let admission = {
            let controller = Arc::clone(&preparing);
            thread::spawn(move || {
                let result = controller.admit_dispatch(
                    &[
                        CapabilityDependency::Workspace,
                        CapabilityDependency::Ast,
                        CapabilityDependency::Lsp,
                    ],
                    Duration::from_millis(120),
                );
                admission_tx.send(result).unwrap();
            })
        };

        assert!(preparing.wait_for_readiness_waiter(Duration::from_secs(2)));
        preparing
            .transition_extension(&ExtensionId::new("ast"), CapabilityReadiness::Ready)
            .unwrap();
        assert_admission_error(
            admission_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("admission deadline must remain bounded after AST becomes ready"),
            ReadinessWaitError::Timeout {
                dependency: CapabilityDependency::Lsp,
                phase: INITIAL_PHASE,
            },
        );
        admission.join().unwrap();
        assert!(preparing.wait_for_dispatch_drain(Duration::ZERO));
    }
}

#[cfg(test)]
mod async_failure_tests {
    use super::*;
    #[tokio::test]
    async fn workspace_failure_observation_is_sticky_and_wakes_waiters() {
        for before in [true, false] {
            let readiness = Arc::new(ReadinessController::new(1));
            let failure = CapabilityReadiness::Failed {
                failure_class: StartupFailureClass::Storage,
            };
            if before {
                readiness
                    .transition_capability(CapabilityDependency::Workspace, failure.clone())
                    .unwrap();
            }
            let observed = Arc::clone(&readiness);
            let waiter = tokio::spawn(async move {
                observed.wait_for_workspace_failure().await;
            });
            tokio::task::yield_now().await;
            if !before {
                assert!(!waiter.is_finished());
                readiness
                    .transition_capability(CapabilityDependency::Workspace, failure)
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap();
        }
    }
}
