#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::protocol::DebugInitializeConfig;
use crate::traces::TraceId;
use crate::types::{
    DebugBreakpoint, DebugOutput, DebugRuntimeError, DebugScope, DebugSessionId, DebugSessionState,
    DebugStackFrame, DebugStop, DebugThread, DebugVariable, RuntimeFailure,
};

const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(1);

/// One admitted operation budget. Exhaustion stays finite and metadata keeps
/// the original budget even when an adapter receives only the remaining time.
pub(crate) struct OperationBudget {
    deadline: Option<Instant>,
    budget_ms: Option<u64>,
}

impl OperationBudget {
    pub(crate) fn new(timeout: Option<Duration>) -> Result<Self, DebugRuntimeError> {
        if timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(DebugRuntimeError::InvalidParams(
                "invalid debug operation timeout".to_owned(),
            ));
        }
        Self::from_remaining(timeout)
    }

    // A nested step may receive an exhausted but still finite budget.
    pub(crate) fn from_remaining(timeout: Option<Duration>) -> Result<Self, DebugRuntimeError> {
        let admitted = timeout
            .map(|timeout| {
                let invalid = || {
                    DebugRuntimeError::InvalidParams("invalid debug operation timeout".to_owned())
                };
                let budget_ms = u64::try_from(timeout.as_millis()).map_err(|_| invalid())?;
                let deadline = Instant::now().checked_add(timeout).ok_or_else(invalid)?;
                Ok::<_, DebugRuntimeError>((deadline, budget_ms))
            })
            .transpose()?;
        Ok(Self {
            deadline: admitted.map(|(deadline, _)| deadline),
            budget_ms: admitted.map(|(_, budget_ms)| budget_ms),
        })
    }

    pub(crate) fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub(crate) fn map_error(&self, error: DebugRuntimeError) -> DebugRuntimeError {
        match (error, self.budget_ms) {
            (
                DebugRuntimeError::DebugTimeout {
                    message,
                    mut detail,
                },
                Some(budget_ms),
            ) => {
                detail.budget_ms = budget_ms;
                DebugRuntimeError::DebugTimeout { message, detail }
            }
            (error, _) => error,
        }
    }
}

pub struct SessionManager {
    config: DebugInitializeConfig,
    adapter_factory: Arc<dyn DebugAdapterFactory>,
    manager_generation: String,
    state: Mutex<ManagerState>,
}

struct ManagerState {
    next_session_index: u64,
    sessions: BTreeMap<String, SessionEntry>,
}

struct SessionEntry {
    language: String,
    kind: SessionKind,
    state: DebugSessionState,
    last_stop: Option<DebugStop>,
    output: Vec<DebugOutput>,
    adapter: Box<dyn DebugAdapterSession>,
    timeout: Option<Duration>,
    idle_ttl: Duration,
    last_activity: Instant,
}

impl SessionManager {
    pub fn new(
        config: DebugInitializeConfig,
        adapter_factory: Arc<dyn DebugAdapterFactory>,
    ) -> Self {
        Self {
            config,
            adapter_factory,
            manager_generation: new_manager_generation(),
            state: Mutex::new(ManagerState {
                next_session_index: 1,
                sessions: BTreeMap::new(),
            }),
        }
    }

    pub fn default_timeout_for_language(
        &self,
        language: &str,
    ) -> Result<Option<Duration>, DebugRuntimeError> {
        self.config
            .languages
            .get(language)
            .map(|config| config.default_timeout_secs.map(Duration::from_secs))
            .ok_or_else(|| {
                DebugRuntimeError::LaunchFailed(format!(
                    "no debug adapter configured for language {language}"
                ))
            })
    }

    pub fn launch(&self, params: LaunchRequest) -> Result<LaunchResult, DebugRuntimeError> {
        self.launch_with_initial_breakpoints(params, Vec::new())
            .map(|(result, _)| result)
    }

    pub fn launch_with_initial_breakpoints(
        &self,
        params: LaunchRequest,
        initial_breakpoints: Vec<DebugBreakpoint>,
    ) -> Result<(LaunchResult, Vec<DebugBreakpoint>), DebugRuntimeError> {
        self.launch_with_initial_breakpoints_timeout(params, initial_breakpoints, None)
    }

    pub fn launch_with_initial_breakpoints_timeout(
        &self,
        params: LaunchRequest,
        initial_breakpoints: Vec<DebugBreakpoint>,
        timeout_override: Option<Duration>,
    ) -> Result<(LaunchResult, Vec<DebugBreakpoint>), DebugRuntimeError> {
        let adapter_config = self.config.languages.get(&params.language).ok_or_else(|| {
            DebugRuntimeError::LaunchFailed(format!(
                "no debug adapter configured for language {}",
                params.language
            ))
        })?;
        let timeout = timeout_override
            .or_else(|| adapter_config.default_timeout_secs.map(Duration::from_secs));
        let budget = OperationBudget::new(timeout)?;

        let idle_ttl = Duration::from_secs(adapter_config.idle_ttl_secs);

        let mut adapter = self.adapter_factory.start(&params)?;
        if let Err(error) = adapter.initialize(budget.remaining()) {
            let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
            return Err(budget.map_error(error));
        }
        if let Err(error) = adapter.launch(&params, budget.remaining()) {
            let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
            return Err(budget.map_error(error));
        }
        let configured_breakpoints = if initial_breakpoints.is_empty() {
            Vec::new()
        } else {
            match adapter.set_breakpoints(&initial_breakpoints, budget.remaining()) {
                Ok(breakpoints) => breakpoints,
                Err(error) => {
                    let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
                    return Err(budget.map_error(error));
                }
            }
        };
        if let Err(error) = adapter.set_breakpoints(&[], budget.remaining()) {
            let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
            return Err(budget.map_error(error));
        }

        let top_frame = match adapter.stack(1, budget.remaining()) {
            Ok(frames) => frames.into_iter().next(),
            Err(error @ DebugRuntimeError::DebugTimeout { .. }) => {
                let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
                return Err(budget.map_error(error));
            }
            Err(_) => None,
        };
        let stop = DebugStop {
            state: DebugSessionState::Stopped,
            reason: Some("breakpoint".to_owned()),
            thread_id: Some(1),
            top_frame,
            hit_breakpoint_ids: vec![1],
            timed_out: false,
            error: None,
            exit_code: None,
            output_since: Vec::new(),
        };

        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let session_id = DebugSessionId(format!(
            "debug-{}-{}",
            self.manager_generation, state.next_session_index
        ));
        state.next_session_index += 1;
        state.sessions.insert(
            session_id.0.clone(),
            SessionEntry {
                language: params.language,
                kind: SessionKind::Live,
                state: DebugSessionState::Stopped,
                last_stop: Some(stop.clone()),
                output: Vec::new(),
                adapter,
                timeout,
                idle_ttl,
                last_activity: Instant::now(),
            },
        );

        Ok((
            LaunchResult {
                session_id,
                state: DebugSessionState::Stopped,
                stop: Some(stop),
            },
            configured_breakpoints,
        ))
    }

    pub fn open_replay(
        &self,
        request: ReplayOpenRequest,
    ) -> Result<ReplayResult, DebugRuntimeError> {
        let adapter_config = self
            .config
            .languages
            .get(&request.language)
            .ok_or_else(|| {
                DebugRuntimeError::LaunchFailed(format!(
                    "no debug adapter configured for language {}",
                    request.language
                ))
            })?;
        let timeout = request
            .timeout_secs
            .map(Duration::from_secs)
            .or_else(|| adapter_config.default_timeout_secs.map(Duration::from_secs));

        let budget = OperationBudget::new(timeout)?;
        let idle_ttl = Duration::from_secs(adapter_config.idle_ttl_secs);
        let trace_id = request.trace_id;
        let language = request.language;
        let trace_path = request.trace_path;
        let (adapter_program, adapter_args) = match request.adapter {
            Some(adapter) => (adapter, request.adapter_args),
            None => {
                self.config.record.as_ref().ok_or_else(|| {
                    DebugRuntimeError::LaunchFailed(
                        "no rr replay adapter configured for replay sessions".to_owned(),
                    )
                })?;
                let (program, mut args) = if adapter_config.adapter_type == "fixture" {
                    (adapter_config.command.clone(), adapter_config.args.clone())
                } else {
                    ("rr".to_owned(), vec!["replay".to_owned()])
                };
                if let Some(trace_path) = trace_path {
                    args.push(trace_path);
                }
                args.extend(request.adapter_args);
                (program, args)
            }
        };
        let mut launch_overrides = Map::new();
        launch_overrides.insert("trace_id".to_owned(), Value::String(trace_id.to_string()));
        let start_request = LaunchRequest {
            language: language.clone(),
            program: adapter_program,
            cwd: None,
            args: adapter_args,
            env: BTreeMap::new(),
            launch_overrides,
        };

        let mut adapter = self.adapter_factory.start(&start_request)?;
        if let Err(error) = adapter.initialize(budget.remaining()) {
            let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
            return Err(budget.map_error(error));
        }
        if let Err(error) = adapter.launch(&start_request, budget.remaining()) {
            let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
            return Err(budget.map_error(error));
        }

        let top_frame = match adapter.stack(1, budget.remaining()) {
            Ok(frames) => frames.into_iter().next(),
            Err(error @ DebugRuntimeError::DebugTimeout { .. }) => {
                let _ = adapter.terminate(LIFECYCLE_TIMEOUT);
                return Err(budget.map_error(error));
            }
            Err(_) => None,
        };
        let stop = DebugStop {
            state: DebugSessionState::Stopped,
            reason: Some("replay".to_owned()),
            thread_id: Some(1),
            top_frame,
            hit_breakpoint_ids: Vec::new(),
            timed_out: false,
            error: None,
            exit_code: None,
            output_since: Vec::new(),
        };
        let supports_step_back = true;

        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let session_id = DebugSessionId(format!(
            "debug-{}-{}",
            self.manager_generation, state.next_session_index
        ));
        state.next_session_index += 1;
        state.sessions.insert(
            session_id.0.clone(),
            SessionEntry {
                language,
                kind: SessionKind::Replay {
                    trace_id: trace_id.clone(),
                    supports_step_back,
                },
                state: DebugSessionState::Stopped,
                last_stop: Some(stop.clone()),
                output: Vec::new(),
                adapter,
                timeout,
                idle_ttl,
                last_activity: Instant::now(),
            },
        );

        Ok(ReplayResult {
            session_id,
            trace_id,
            state: DebugSessionState::Stopped,
            stop: Some(stop),
            supports_step_back,
        })
    }

    pub fn session_kind(
        &self,
        session_id: &DebugSessionId,
    ) -> Result<SessionKind, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        session.last_activity = Instant::now();
        Ok(session.kind.clone())
    }

    pub fn reverse_continue(
        &self,
        session_id: &DebugSessionId,
        thread_id: Option<u64>,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        self.resume_replay(session_id, timeout, |adapter, timeout| {
            adapter.reverse_continue(thread_id, timeout)
        })
    }

    pub fn step_back(
        &self,
        session_id: &DebugSessionId,
        thread_id: Option<u64>,
        granularity: StepBackGranularity,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        self.resume_replay(session_id, timeout, |adapter, timeout| {
            adapter.step_back(thread_id, granularity, timeout)
        })
    }

    pub fn set_watchpoint(
        &self,
        session_id: &DebugSessionId,
        watchpoint: WatchpointSpec,
    ) -> Result<WatchpointResult, DebugRuntimeError> {
        self.set_watchpoint_with_timeout(session_id, watchpoint, None)
    }

    pub fn set_watchpoint_with_timeout(
        &self,
        session_id: &DebugSessionId,
        watchpoint: WatchpointSpec,
        timeout: Option<Duration>,
    ) -> Result<WatchpointResult, DebugRuntimeError> {
        self.with_replay_session(session_id, |session| {
            OperationBudget::new(timeout.or(session.timeout))?;
            session
                .adapter
                .set_watchpoint(watchpoint, timeout.or(session.timeout))
        })
    }

    pub fn seek_replay(
        &self,
        session_id: &DebugSessionId,
        target: ReplaySeekTarget,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        self.resume_replay(session_id, timeout, |adapter, timeout| {
            adapter.seek_replay(target, timeout)
        })
    }

    pub fn set_breakpoints(
        &self,
        session_id: &DebugSessionId,
        breakpoints: Vec<DebugBreakpoint>,
    ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        session.last_activity = Instant::now();
        match session
            .adapter
            .set_breakpoints(&breakpoints, session.timeout)
        {
            Ok(breakpoints) => Ok(breakpoints),
            Err(DebugRuntimeError::AdapterExited(_)) => {
                Err(Self::reap_adapter_exit_locked(&mut state, session_id))
            }
            Err(error) => Err(error),
        }
    }

    pub fn continue_session(
        &self,
        session_id: &DebugSessionId,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        self.resume(session_id, timeout, |adapter, timeout| {
            adapter.continue_session(timeout)
        })
    }

    pub fn step(
        &self,
        session_id: &DebugSessionId,
        thread_id: Option<u64>,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        self.resume(session_id, timeout, |adapter, timeout| {
            adapter.step(thread_id, timeout)
        })
    }

    pub fn pause(
        &self,
        session_id: &DebugSessionId,
        thread_id: Option<u64>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        session.last_activity = Instant::now();
        match session.adapter.pause(thread_id, session.timeout) {
            Ok(stop) => {
                update_session_stop(session, &stop);
                Ok(stop)
            }
            Err(DebugRuntimeError::AdapterExited(_)) => {
                Err(Self::reap_adapter_exit_locked(&mut state, session_id))
            }
            Err(error) => Err(error),
        }
    }

    pub fn threads(
        &self,
        session_id: &DebugSessionId,
    ) -> Result<Vec<DebugThread>, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        session.last_activity = Instant::now();
        match session.adapter.threads(session.timeout) {
            Ok(threads) => Ok(threads),
            Err(DebugRuntimeError::AdapterExited(_)) => {
                Err(Self::reap_adapter_exit_locked(&mut state, session_id))
            }
            Err(error) => Err(error),
        }
    }

    pub fn stack(
        &self,
        session_id: &DebugSessionId,
        thread_id: u64,
    ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
        self.stack_with_timeout(session_id, thread_id, None)
    }

    pub fn stack_with_timeout(
        &self,
        session_id: &DebugSessionId,
        thread_id: u64,
        timeout: Option<Duration>,
    ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
        self.with_stopped_session(session_id, |session| {
            OperationBudget::new(timeout.or(session.timeout))?;
            session
                .adapter
                .stack(thread_id, timeout.or(session.timeout))
        })
    }

    pub fn scopes(
        &self,
        session_id: &DebugSessionId,
        frame_id: u64,
    ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
        self.scopes_with_timeout(session_id, frame_id, None)
    }

    pub fn scopes_with_timeout(
        &self,
        session_id: &DebugSessionId,
        frame_id: u64,
        timeout: Option<Duration>,
    ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
        self.with_stopped_session(session_id, |session| {
            OperationBudget::new(timeout.or(session.timeout))?;
            session
                .adapter
                .scopes(frame_id, timeout.or(session.timeout))
        })
    }

    pub fn variables(
        &self,
        session_id: &DebugSessionId,
        variables_reference: u64,
    ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
        self.variables_with_timeout(session_id, variables_reference, None)
    }

    pub fn variables_with_timeout(
        &self,
        session_id: &DebugSessionId,
        variables_reference: u64,
        timeout: Option<Duration>,
    ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
        self.with_stopped_session(session_id, |session| {
            OperationBudget::new(timeout.or(session.timeout))?;
            session
                .adapter
                .variables(variables_reference, timeout.or(session.timeout))
        })
    }

    pub fn evaluate(
        &self,
        session_id: &DebugSessionId,
        frame_id: u64,
        expression: String,
    ) -> Result<DebugVariable, DebugRuntimeError> {
        self.evaluate_with_timeout(session_id, frame_id, expression, None)
    }

    pub fn evaluate_with_timeout(
        &self,
        session_id: &DebugSessionId,
        frame_id: u64,
        expression: String,
        timeout: Option<Duration>,
    ) -> Result<DebugVariable, DebugRuntimeError> {
        self.with_stopped_session(session_id, |session| {
            OperationBudget::new(timeout.or(session.timeout))?;
            session
                .adapter
                .evaluate(frame_id, &expression, timeout.or(session.timeout))
        })
    }

    pub fn output_since(
        &self,
        session_id: &DebugSessionId,
        sequence: u64,
    ) -> Result<Vec<DebugOutput>, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        session.last_activity = Instant::now();
        let mut returned = Vec::new();
        let mut retained = Vec::new();
        for output in session.output.drain(..) {
            if output.sequence > sequence {
                returned.push(output);
            } else {
                retained.push(output);
            }
        }
        session.output = retained;
        Ok(returned)
    }

    pub fn terminate(&self, session_id: &DebugSessionId) -> Result<(), DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let result = {
            let Some(session) = state.sessions.get_mut(&session_id.0) else {
                return Err(session_not_found(session_id));
            };
            session.last_activity = Instant::now();
            session.adapter.terminate(LIFECYCLE_TIMEOUT)
        };
        if result.is_ok() {
            state.sessions.remove(&session_id.0);
        };
        result
    }

    pub fn reap_after_recipe_timeout(&self, session_id: &DebugSessionId) {
        let session = {
            let mut state = self.state.lock().unwrap();
            self.cleanup_expired_locked(&mut state);
            state.sessions.remove(&session_id.0)
        };
        if let Some(mut session) = session {
            let _ = session.adapter.terminate(LIFECYCLE_TIMEOUT);
        }
    }

    pub fn disconnect(&self, session_id: &DebugSessionId) -> Result<(), DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let result = {
            let Some(session) = state.sessions.get_mut(&session_id.0) else {
                return Err(session_not_found(session_id));
            };
            session.last_activity = Instant::now();
            session.adapter.disconnect(LIFECYCLE_TIMEOUT)
        };
        if result.is_ok() {
            state.sessions.remove(&session_id.0);
        };
        result
    }

    pub fn sessions(&self) -> Vec<DebugSessionSummary> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        state
            .sessions
            .iter()
            .map(|(session_id, session)| DebugSessionSummary {
                session_id: DebugSessionId(session_id.clone()),
                language: session.language.clone(),
                state: session.state.clone(),
                last_stop: session.last_stop.clone(),
            })
            .collect()
    }

    pub fn shutdown_all(&self) {
        let mut state = self.state.lock().unwrap();
        let sessions = std::mem::take(&mut state.sessions);
        for mut session in sessions.into_values() {
            let _ = session.adapter.terminate(LIFECYCLE_TIMEOUT);
        }
    }

    fn resume(
        &self,
        session_id: &DebugSessionId,
        timeout: Option<Duration>,
        resume: impl FnOnce(
            &mut dyn DebugAdapterSession,
            Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        let timeout = timeout.or(session.timeout);
        OperationBudget::new(timeout)?;
        session.last_activity = Instant::now();
        session.state = DebugSessionState::Running;
        match resume(session.adapter.as_mut(), timeout) {
            Ok(stop) => {
                update_session_stop(session, &stop);
                Ok(stop)
            }
            Err(DebugRuntimeError::AdapterExited(_)) => {
                Err(Self::reap_adapter_exit_locked(&mut state, session_id))
            }
            Err(DebugRuntimeError::DebugTimeout { message, detail }) => {
                let stop = DebugStop {
                    state: DebugSessionState::Running,
                    reason: Some("timeout".to_owned()),
                    thread_id: None,
                    top_frame: None,
                    hit_breakpoint_ids: Vec::new(),
                    timed_out: true,
                    error: Some(timeout_failure(message, detail)),
                    exit_code: None,
                    output_since: Vec::new(),
                };
                update_session_stop(session, &stop);
                Ok(stop)
            }
            Err(error) => Err(error),
        }
    }

    fn with_stopped_session<T>(
        &self,
        session_id: &DebugSessionId,
        operation: impl FnOnce(&mut SessionEntry) -> Result<T, DebugRuntimeError>,
    ) -> Result<T, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        session.last_activity = Instant::now();
        if session.state != DebugSessionState::Stopped {
            return Err(DebugRuntimeError::NotStopped(format!(
                "debug session {} is not stopped",
                session_id.0
            )));
        }
        match operation(session) {
            Ok(result) => Ok(result),
            Err(DebugRuntimeError::AdapterExited(_)) => {
                Err(Self::reap_adapter_exit_locked(&mut state, session_id))
            }
            Err(error) => Err(error),
        }
    }

    fn resume_replay(
        &self,
        session_id: &DebugSessionId,
        timeout: Option<Duration>,
        resume: impl FnOnce(
            &mut dyn DebugAdapterSession,
            Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        OperationBudget::new(timeout)?;
        let mut session = {
            let mut state = self.state.lock().unwrap();
            self.cleanup_expired_locked(&mut state);
            let Some(mut session) = state.sessions.remove(&session_id.0) else {
                return Err(session_not_found(session_id));
            };
            if matches!(session.kind, SessionKind::Live) {
                state.sessions.insert(session_id.0.clone(), session);
                return Err(reverse_unsupported());
            }
            session.last_activity = Instant::now();
            session
        };
        let previous_state = session.state.clone();
        let previous_stop = session.last_stop.clone();
        session.state = DebugSessionState::Running;
        let timeout = timeout.or(session.timeout);

        let result = match resume(session.adapter.as_mut(), timeout) {
            Ok(stop) => {
                update_session_stop(&mut session, &stop);
                Ok(stop)
            }
            Err(DebugRuntimeError::AdapterExited(_)) => {
                let _ = session.adapter.terminate(LIFECYCLE_TIMEOUT);
                Err(session_not_found(session_id))
            }
            Err(DebugRuntimeError::DebugTimeout { message, detail }) => {
                let stop = timeout_stop(message, detail);
                update_session_stop(&mut session, &stop);
                Ok(stop)
            }
            Err(error) => {
                session.state = previous_state;
                session.last_stop = previous_stop;
                session.last_activity = Instant::now();
                Err(error)
            }
        };

        if !matches!(result, Err(DebugRuntimeError::SessionNotFound(_))) {
            let mut state = self.state.lock().unwrap();
            state.sessions.insert(session_id.0.clone(), session);
        }

        result
    }

    fn with_replay_session<T>(
        &self,
        session_id: &DebugSessionId,
        operation: impl FnOnce(&mut SessionEntry) -> Result<T, DebugRuntimeError>,
    ) -> Result<T, DebugRuntimeError> {
        let mut session = {
            let mut state = self.state.lock().unwrap();
            self.cleanup_expired_locked(&mut state);
            let Some(mut session) = state.sessions.remove(&session_id.0) else {
                return Err(session_not_found(session_id));
            };
            if matches!(session.kind, SessionKind::Live) {
                state.sessions.insert(session_id.0.clone(), session);
                return Err(reverse_unsupported());
            }
            session.last_activity = Instant::now();
            session
        };

        let result = match operation(&mut session) {
            Ok(result) => Ok(result),
            Err(DebugRuntimeError::AdapterExited(_)) => {
                let _ = session.adapter.terminate(LIFECYCLE_TIMEOUT);
                Err(session_not_found(session_id))
            }
            Err(error) => Err(error),
        };

        if !matches!(result, Err(DebugRuntimeError::SessionNotFound(_))) {
            let mut state = self.state.lock().unwrap();
            state.sessions.insert(session_id.0.clone(), session);
        }

        result
    }

    fn reap_adapter_exit_locked(
        state: &mut ManagerState,
        session_id: &DebugSessionId,
    ) -> DebugRuntimeError {
        if let Some(mut session) = state.sessions.remove(&session_id.0) {
            let _ = session.adapter.terminate(LIFECYCLE_TIMEOUT);
        }
        session_not_found(session_id)
    }

    fn cleanup_expired_locked(&self, state: &mut ManagerState) {
        let now = Instant::now();
        let expired: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, session)| now.duration_since(session.last_activity) >= session.idle_ttl)
            .map(|(session_id, _)| session_id.clone())
            .collect();

        for session_id in expired {
            if let Some(mut session) = state.sessions.remove(&session_id) {
                let _ = session.adapter.terminate(LIFECYCLE_TIMEOUT);
            }
        }
    }
}

static MANAGER_GENERATION_COUNTER: AtomicU64 = AtomicU64::new(1);

fn new_manager_generation() -> String {
    let counter = MANAGER_GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{}-{}", std::process::id(), timestamp, counter)
}

fn update_session_stop(session: &mut SessionEntry, stop: &DebugStop) {
    session.state = stop.state.clone();
    session.last_stop = Some(stop.clone());
    session.output.extend(stop.output_since.iter().cloned());
    session.last_activity = Instant::now();
}

fn timeout_stop(message: String, detail: extension_protocol::TimeoutDetail) -> DebugStop {
    DebugStop {
        state: DebugSessionState::Running,
        reason: Some("timeout".to_owned()),
        thread_id: None,
        top_frame: None,
        hit_breakpoint_ids: Vec::new(),
        timed_out: true,
        error: Some(timeout_failure(message, detail)),
        exit_code: None,
        output_since: Vec::new(),
    }
}

fn timeout_failure(message: String, detail: extension_protocol::TimeoutDetail) -> RuntimeFailure {
    RuntimeFailure {
        code: "debug-timeout".to_owned(),
        message,
        data: Some(serde_json::to_value(detail).expect("serialize timeout detail")),
    }
}

fn session_not_found(session_id: &DebugSessionId) -> DebugRuntimeError {
    DebugRuntimeError::SessionNotFound(format!("debug session {} was not found", session_id.0))
}

fn reverse_unsupported() -> DebugRuntimeError {
    DebugRuntimeError::ReverseUnsupported("reverse_unsupported".to_owned())
}

pub trait DebugAdapterFactory: Send + Sync {
    fn start(
        &self,
        request: &LaunchRequest,
    ) -> Result<Box<dyn DebugAdapterSession>, DebugRuntimeError>;
}

pub trait DebugAdapterSession: Send {
    fn initialize(&mut self, timeout: Option<Duration>) -> Result<(), DebugRuntimeError>;
    fn launch(
        &mut self,
        request: &LaunchRequest,
        timeout: Option<Duration>,
    ) -> Result<(), DebugRuntimeError>;
    fn set_breakpoints(
        &mut self,
        breakpoints: &[DebugBreakpoint],
        timeout: Option<Duration>,
    ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError>;
    fn continue_session(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError>;
    fn step(
        &mut self,
        thread_id: Option<u64>,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError>;
    fn pause(
        &mut self,
        thread_id: Option<u64>,
        timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError>;
    fn threads(&mut self, timeout: Option<Duration>)
    -> Result<Vec<DebugThread>, DebugRuntimeError>;
    fn stack(
        &mut self,
        thread_id: u64,
        timeout: Option<Duration>,
    ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError>;
    fn scopes(
        &mut self,
        frame_id: u64,
        timeout: Option<Duration>,
    ) -> Result<Vec<DebugScope>, DebugRuntimeError>;
    fn variables(
        &mut self,
        variables_reference: u64,
        timeout: Option<Duration>,
    ) -> Result<Vec<DebugVariable>, DebugRuntimeError>;
    fn evaluate(
        &mut self,
        frame_id: u64,
        expression: &str,
        timeout: Option<Duration>,
    ) -> Result<DebugVariable, DebugRuntimeError>;
    fn reverse_continue(
        &mut self,
        _thread_id: Option<u64>,
        _timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        Err(reverse_unsupported())
    }
    fn step_back(
        &mut self,
        _thread_id: Option<u64>,
        _granularity: StepBackGranularity,
        _timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        Err(reverse_unsupported())
    }
    fn set_watchpoint(
        &mut self,
        _watchpoint: WatchpointSpec,
        _timeout: Option<Duration>,
    ) -> Result<WatchpointResult, DebugRuntimeError> {
        Err(reverse_unsupported())
    }
    fn seek_replay(
        &mut self,
        _target: ReplaySeekTarget,
        _timeout: Option<Duration>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        Err(reverse_unsupported())
    }
    fn terminate(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError>;
    fn disconnect(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    Live,
    Replay {
        trace_id: TraceId,
        supports_step_back: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayOpenRequest {
    pub trace_id: TraceId,
    pub trace_path: Option<String>,
    pub language: String,
    pub timeout_secs: Option<u64>,
    pub adapter: Option<String>,
    pub adapter_args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayResult {
    pub session_id: DebugSessionId,
    pub trace_id: TraceId,
    pub state: DebugSessionState,
    pub stop: Option<DebugStop>,
    #[serde(rename = "supportsStepBack")]
    pub supports_step_back: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepBackGranularity {
    Line,
    Instruction,
    Over,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchpointKind {
    Write,
    Read,
    Access,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchpointSpec {
    pub expression: Option<String>,
    pub address: Option<String>,
    pub kind: WatchpointKind,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchpointResult {
    pub watchpoint_id: String,
    pub expression: Option<String>,
    pub address: Option<String>,
    pub kind: WatchpointKind,
    pub enabled: bool,
    pub verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplaySeekTarget {
    Crash,
    End,
    Source {
        path: String,
        line: u64,
        column: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub language: String,
    pub program: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub launch_overrides: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchResult {
    pub session_id: DebugSessionId,
    pub state: DebugSessionState,
    pub stop: Option<DebugStop>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DebugSessionSummary {
    pub session_id: DebugSessionId,
    pub language: String,
    pub state: DebugSessionState,
    pub last_stop: Option<DebugStop>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Map, json};

    use crate::protocol::{DebugAdapterConfig, DebugInitializeConfig};
    use crate::traces::TraceId;

    use super::{
        DebugAdapterFactory, DebugAdapterSession, DebugBreakpoint, DebugOutput, DebugRuntimeError,
        DebugScope, DebugSessionId, DebugSessionState, DebugStackFrame, DebugStop, DebugThread,
        DebugVariable, LaunchRequest, ReplayOpenRequest, ReplayResult, ReplaySeekTarget,
        SessionKind, SessionManager, StepBackGranularity, WatchpointKind, WatchpointResult,
        WatchpointSpec,
    };

    type RecordedTimeouts = Arc<Mutex<Vec<(&'static str, Option<Duration>)>>>;

    #[derive(Default)]
    struct FakeAdapterFactory {
        starts: Arc<Mutex<Vec<LaunchRequest>>>,
        calls: Arc<Mutex<Vec<FakeAdapterCall>>>,
        timeouts: RecordedTimeouts,
        lost_on_threads: bool,
        lost_on_evaluate: bool,
        launch_error: bool,
        terminate_error: bool,
        disconnect_error: bool,
        reverse_adapter_exit: bool,
        reverse_unsupported: bool,
        stage_delay: Duration,
        stack_timeout: bool,
    }

    impl FakeAdapterFactory {
        fn lost_on_threads() -> Self {
            Self {
                lost_on_threads: true,
                ..Self::default()
            }
        }

        fn lost_on_evaluate() -> Self {
            Self {
                lost_on_evaluate: true,
                ..Self::default()
            }
        }

        fn launch_error() -> Self {
            Self {
                launch_error: true,
                ..Self::default()
            }
        }

        fn terminate_error() -> Self {
            Self {
                terminate_error: true,
                ..Self::default()
            }
        }

        fn disconnect_error() -> Self {
            Self {
                disconnect_error: true,
                ..Self::default()
            }
        }

        fn reverse_adapter_exit() -> Self {
            Self {
                reverse_adapter_exit: true,
                ..Self::default()
            }
        }

        fn reverse_unsupported() -> Self {
            Self {
                reverse_unsupported: true,
                ..Self::default()
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeAdapterCall {
        Initialize,
        Launch,
        SetBreakpoints(Vec<u64>),
        ReverseContinue(Option<u64>),
        StepBack(Option<u64>, StepBackGranularity),
        SetWatchpoint(WatchpointSpec),
        SeekReplay(ReplaySeekTarget),
        Terminate,
        Disconnect,
    }

    impl DebugAdapterFactory for FakeAdapterFactory {
        fn start(
            &self,
            request: &LaunchRequest,
        ) -> Result<Box<dyn DebugAdapterSession>, DebugRuntimeError> {
            self.starts.lock().unwrap().push(request.clone());
            Ok(Box::new(FakeAdapterSession {
                calls: Arc::clone(&self.calls),
                timeouts: Arc::clone(&self.timeouts),
                lost_on_threads: self.lost_on_threads,
                lost_on_evaluate: self.lost_on_evaluate,
                launch_error: self.launch_error,
                terminate_error: self.terminate_error,
                disconnect_error: self.disconnect_error,
                reverse_adapter_exit: self.reverse_adapter_exit,
                reverse_unsupported: self.reverse_unsupported,
                stage_delay: self.stage_delay,
                stack_timeout: self.stack_timeout,
                ..FakeAdapterSession::default()
            }))
        }
    }

    #[derive(Default)]
    struct FakeAdapterSession {
        calls: Arc<Mutex<Vec<FakeAdapterCall>>>,
        timeouts: RecordedTimeouts,
        initialized: bool,
        launched: bool,
        configured: bool,
        terminated: bool,
        disconnected: bool,
        lost_on_threads: bool,
        lost_on_evaluate: bool,
        launch_error: bool,
        terminate_error: bool,
        disconnect_error: bool,
        reverse_timeout: bool,
        reverse_adapter_exit: bool,
        reverse_unsupported: bool,
        stage_delay: Duration,
        stack_timeout: bool,
    }

    impl FakeAdapterSession {
        fn stage(
            &self,
            operation: &'static str,
            timeout: Option<Duration>,
        ) -> Result<(), DebugRuntimeError> {
            self.record_timeout(operation, timeout);
            if timeout.is_some_and(|timeout| timeout <= self.stage_delay)
                || (operation == "stack" && self.stack_timeout)
            {
                return Err(DebugRuntimeError::DebugTimeout {
                    message: "debug operation timed out".to_owned(),
                    detail: extension_protocol::TimeoutDetail {
                        operation: operation.to_owned(),
                        layer: "dap".to_owned(),
                        budget_ms: u64::try_from(timeout.unwrap().as_millis()).unwrap(),
                    },
                });
            }
            std::thread::sleep(self.stage_delay);
            Ok(())
        }

        fn record_timeout(&self, operation: &'static str, timeout: Option<Duration>) {
            self.timeouts.lock().unwrap().push((operation, timeout));
        }
    }

    impl DebugAdapterSession for FakeAdapterSession {
        fn initialize(&mut self, timeout: Option<Duration>) -> Result<(), DebugRuntimeError> {
            self.stage("initialize", timeout)?;
            self.calls.lock().unwrap().push(FakeAdapterCall::Initialize);
            self.initialized = true;
            Ok(())
        }

        fn launch(
            &mut self,
            _request: &LaunchRequest,
            timeout: Option<Duration>,
        ) -> Result<(), DebugRuntimeError> {
            self.stage("launch", timeout)?;
            self.calls.lock().unwrap().push(FakeAdapterCall::Launch);
            if self.launch_error {
                return Err(DebugRuntimeError::LaunchFailed(
                    "fake adapter rejected launch".to_owned(),
                ));
            }
            self.launched = true;
            Ok(())
        }

        fn set_breakpoints(
            &mut self,
            breakpoints: &[DebugBreakpoint],
            timeout: Option<Duration>,
        ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError> {
            self.stage("set_breakpoints", timeout)?;
            self.calls
                .lock()
                .unwrap()
                .push(FakeAdapterCall::SetBreakpoints(
                    breakpoints
                        .iter()
                        .map(|breakpoint| breakpoint.line)
                        .collect(),
                ));
            self.configured = true;
            Ok(breakpoints
                .iter()
                .enumerate()
                .map(|(index, breakpoint)| DebugBreakpoint {
                    verified: true,
                    verified_id: Some((index + 1) as u64),
                    ..breakpoint.clone()
                })
                .collect())
        }

        fn continue_session(
            &mut self,
            timeout: Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record_timeout("continue", timeout);
            if timeout.is_some_and(|timeout| timeout <= Duration::from_millis(1)) {
                return Err(DebugRuntimeError::DebugTimeout {
                    message: "debug operation timed out".to_owned(),
                    detail: extension_protocol::TimeoutDetail {
                        operation: "continue".to_owned(),
                        layer: "dap".to_owned(),
                        budget_ms: u64::try_from(timeout.expect("finite fake timeout").as_millis())
                            .unwrap(),
                    },
                });
            }

            Ok(DebugStop {
                output_since: fake_output_events(),
                ..stopped_at_breakpoint()
            })
        }

        fn step(
            &mut self,
            _thread_id: Option<u64>,
            timeout: Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record_timeout("step", timeout);
            Ok(stopped_at_breakpoint())
        }

        fn pause(
            &mut self,
            thread_id: Option<u64>,
            timeout: Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record_timeout("pause", timeout);
            Ok(DebugStop {
                thread_id,
                reason: Some("pause".to_owned()),
                ..stopped_at_breakpoint()
            })
        }

        fn stack(
            &mut self,
            _thread_id: u64,
            timeout: Option<Duration>,
        ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
            self.stage("stack", timeout)?;
            Ok(vec![top_frame()])
        }

        fn scopes(
            &mut self,
            _frame_id: u64,
            timeout: Option<Duration>,
        ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
            self.record_timeout("scopes", timeout);
            Ok(vec![DebugScope {
                name: "Locals".to_owned(),
                variables_reference: 100,
                expensive: false,
            }])
        }

        fn variables(
            &mut self,
            _variables_reference: u64,
            timeout: Option<Duration>,
        ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
            self.record_timeout("variables", timeout);
            Ok(vec![DebugVariable {
                name: "answer".to_owned(),
                value: "42".to_owned(),
                r#type: Some("i32".to_owned()),
                variables_reference: 0,
            }])
        }

        fn evaluate(
            &mut self,
            _frame_id: u64,
            expression: &str,
            timeout: Option<Duration>,
        ) -> Result<DebugVariable, DebugRuntimeError> {
            self.record_timeout("evaluate", timeout);
            if self.lost_on_evaluate {
                return Err(DebugRuntimeError::AdapterExited(
                    "fake adapter exited before evaluate response".to_owned(),
                ));
            }
            Ok(DebugVariable {
                name: expression.to_owned(),
                value: "42".to_owned(),
                r#type: Some("i32".to_owned()),
                variables_reference: 0,
            })
        }

        fn reverse_continue(
            &mut self,
            thread_id: Option<u64>,
            timeout: Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record_timeout("reverse_continue", timeout);
            self.calls
                .lock()
                .unwrap()
                .push(FakeAdapterCall::ReverseContinue(thread_id));
            if self.reverse_adapter_exit {
                return Err(DebugRuntimeError::AdapterExited(
                    "fake replay adapter exited before reverse continue".to_owned(),
                ));
            }
            if self.reverse_unsupported {
                return Err(super::reverse_unsupported());
            }
            if self.reverse_timeout
                || timeout.is_some_and(|timeout| timeout <= Duration::from_millis(1))
            {
                return Err(DebugRuntimeError::DebugTimeout {
                    message: "debug operation timed out".to_owned(),
                    detail: extension_protocol::TimeoutDetail {
                        operation: "reverse_continue".to_owned(),
                        layer: "dap".to_owned(),
                        budget_ms: u64::try_from(timeout.expect("finite fake timeout").as_millis())
                            .unwrap(),
                    },
                });
            }
            Ok(DebugStop {
                reason: Some("reverse".to_owned()),
                ..stopped_at_breakpoint()
            })
        }

        fn step_back(
            &mut self,
            thread_id: Option<u64>,
            granularity: StepBackGranularity,
            timeout: Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record_timeout("step_back", timeout);
            self.calls
                .lock()
                .unwrap()
                .push(FakeAdapterCall::StepBack(thread_id, granularity));
            Ok(DebugStop {
                reason: Some("stepBack".to_owned()),
                ..stopped_at_breakpoint()
            })
        }

        fn set_watchpoint(
            &mut self,
            watchpoint: WatchpointSpec,
            timeout: Option<Duration>,
        ) -> Result<WatchpointResult, DebugRuntimeError> {
            self.record_timeout("set_watchpoint", timeout);
            self.calls
                .lock()
                .unwrap()
                .push(FakeAdapterCall::SetWatchpoint(watchpoint.clone()));
            Ok(WatchpointResult {
                watchpoint_id: "watch-1".to_owned(),
                expression: watchpoint.expression,
                address: watchpoint.address,
                kind: watchpoint.kind,
                enabled: watchpoint.enabled,
                verified: true,
            })
        }

        fn seek_replay(
            &mut self,
            target: ReplaySeekTarget,
            timeout: Option<Duration>,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record_timeout("seek_replay", timeout);
            self.calls
                .lock()
                .unwrap()
                .push(FakeAdapterCall::SeekReplay(target));
            if timeout.is_some_and(|timeout| timeout <= Duration::from_millis(1)) {
                return Err(DebugRuntimeError::DebugTimeout {
                    message: "debug operation timed out".to_owned(),
                    detail: extension_protocol::TimeoutDetail {
                        operation: "seek_replay".to_owned(),
                        layer: "dap".to_owned(),
                        budget_ms: u64::try_from(timeout.expect("finite fake timeout").as_millis())
                            .unwrap(),
                    },
                });
            }
            if self.reverse_unsupported {
                return Err(super::reverse_unsupported());
            }
            Ok(DebugStop {
                reason: Some("seek".to_owned()),
                ..stopped_at_breakpoint()
            })
        }

        fn terminate(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeAdapterCall::Terminate);
            if self.terminate_error {
                return Err(DebugRuntimeError::AdapterExited(
                    "fake terminate failed".to_owned(),
                ));
            }
            self.terminated = true;
            Ok(())
        }

        fn disconnect(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeAdapterCall::Disconnect);
            if self.disconnect_error {
                return Err(DebugRuntimeError::AdapterExited(
                    "fake disconnect failed".to_owned(),
                ));
            }
            self.disconnected = true;
            Ok(())
        }

        fn threads(
            &mut self,
            timeout: Option<Duration>,
        ) -> Result<Vec<DebugThread>, DebugRuntimeError> {
            self.record_timeout("threads", timeout);
            if self.lost_on_threads {
                return Err(DebugRuntimeError::AdapterExited(
                    "fake adapter exited before threads request".to_owned(),
                ));
            }

            Ok(vec![DebugThread {
                id: 1,
                name: "main".to_owned(),
            }])
        }
    }

    fn manager() -> SessionManager {
        SessionManager::new(config(), Arc::new(FakeAdapterFactory::default()))
    }

    fn config() -> DebugInitializeConfig {
        DebugInitializeConfig {
            languages: BTreeMap::from([(
                "rust".to_owned(),
                DebugAdapterConfig {
                    extensions: vec!["rs".to_owned()],
                    command: "fake-debug-adapter".to_owned(),
                    args: Vec::new(),
                    adapter_type: "fake".to_owned(),
                    launch: Map::new(),
                    default_timeout_secs: Some(1),
                    idle_ttl_secs: 1,
                },
            )]),
            record: Some(crate::protocol::DebugRecordConfig {
                backend: "rr".to_owned(),
                trace_dir: None,
                ttl_secs: None,
                max_traces: None,
                record_timeout_secs: None,
            }),
        }
    }

    fn launch_request() -> LaunchRequest {
        LaunchRequest {
            language: "rust".to_owned(),
            program: "target/debug/app".to_owned(),
            cwd: Some("/workspace".to_owned()),
            args: vec!["--flag".to_owned()],
            env: BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())]),
            launch_overrides: Map::new(),
        }
    }

    fn source_breakpoint(line: u64) -> DebugBreakpoint {
        DebugBreakpoint {
            path: "src/main.rs".to_owned(),
            line,
            condition: None,
            hit_condition: None,
            verified: false,
            verified_id: None,
        }
    }

    fn top_frame() -> DebugStackFrame {
        DebugStackFrame {
            id: 10,
            name: "main".to_owned(),
            path: Some("src/main.rs".to_owned()),
            line: 12,
            column: 5,
        }
    }

    fn stopped_at_breakpoint() -> DebugStop {
        DebugStop {
            state: DebugSessionState::Stopped,
            reason: Some("breakpoint".to_owned()),
            thread_id: Some(1),
            top_frame: Some(top_frame()),
            hit_breakpoint_ids: vec![1],
            timed_out: false,
            error: None,
            exit_code: None,
            output_since: Vec::new(),
        }
    }

    fn fake_output_events() -> Vec<DebugOutput> {
        vec![
            DebugOutput {
                sequence: 1,
                category: Some("stdout".to_owned()),
                text: "building\n".to_owned(),
            },
            DebugOutput {
                sequence: 2,
                category: Some("stderr".to_owned()),
                text: "warning: demo\n".to_owned(),
            },
        ]
    }

    fn assert_session_not_found(error: DebugRuntimeError) {
        assert!(matches!(error, DebugRuntimeError::SessionNotFound(_)));
    }

    fn assert_not_stopped(error: DebugRuntimeError) {
        assert!(matches!(error, DebugRuntimeError::NotStopped(_)));
    }

    fn trace_id() -> TraceId {
        TraceId::new("trace-replay-1").expect("valid test trace id")
    }

    fn replay_open_request() -> ReplayOpenRequest {
        ReplayOpenRequest {
            trace_id: trace_id(),
            trace_path: None,
            language: "rust".to_owned(),
            timeout_secs: Some(2),
            adapter: None,
            adapter_args: vec!["--chaos".to_owned()],
        }
    }

    fn watchpoint_spec() -> WatchpointSpec {
        WatchpointSpec {
            expression: Some("counter".to_owned()),
            address: None,
            kind: WatchpointKind::Write,
            enabled: true,
        }
    }

    fn assert_reverse_unsupported(error: DebugRuntimeError) {
        assert_eq!(
            error,
            DebugRuntimeError::ReverseUnsupported("reverse_unsupported".to_owned())
        );
    }

    #[test]
    fn session_manager_launch_performs_initialize_launch_breakpoint_configuration_sequencing_through_dap_transport_and_returns_new_session_id_plus_initial_state()
     {
        let factory = Arc::new(FakeAdapterFactory::default());
        let manager = SessionManager::new(config(), factory.clone());

        let result = manager
            .launch(launch_request())
            .expect("launch should initialize and configure a debug session");

        assert!(!result.session_id.0.is_empty());
        assert_eq!(result.state, DebugSessionState::Stopped);
        assert_eq!(result.stop, Some(stopped_at_breakpoint()));
        assert_eq!(
            *factory.calls.lock().unwrap(),
            vec![
                FakeAdapterCall::Initialize,
                FakeAdapterCall::Launch,
                FakeAdapterCall::SetBreakpoints(Vec::new()),
            ],
            "launch must initialize the adapter, launch the program, then complete breakpoint/configuration setup"
        );
    }

    #[test]
    fn session_manager_launch_with_initial_breakpoints_sets_them_before_configuration_done() {
        let factory = Arc::new(FakeAdapterFactory::default());
        let manager = SessionManager::new(config(), factory.clone());

        let (result, breakpoints) = manager
            .launch_with_initial_breakpoints(launch_request(), vec![source_breakpoint(12)])
            .expect("launch with initial breakpoints should configure and complete the session");

        assert!(!result.session_id.0.is_empty());
        assert_eq!(
            breakpoints,
            vec![DebugBreakpoint {
                verified: true,
                verified_id: Some(1),
                ..source_breakpoint(12)
            }]
        );
        assert_eq!(
            *factory.calls.lock().unwrap(),
            vec![
                FakeAdapterCall::Initialize,
                FakeAdapterCall::Launch,
                FakeAdapterCall::SetBreakpoints(vec![12]),
                FakeAdapterCall::SetBreakpoints(Vec::new()),
            ],
            "initial breakpoints must be sent before configurationDone"
        );
    }

    #[test]
    fn session_manager_does_not_reuse_session_ids_across_manager_lifetimes() {
        let first_manager = manager();
        let second_manager = manager();

        let first = first_manager.launch(launch_request()).unwrap();
        let second = second_manager.launch(launch_request()).unwrap();

        assert_ne!(
            first.session_id, second.session_id,
            "a stale session id from an earlier extension lifetime must not collide with a new manager"
        );
    }

    #[test]
    fn launch_failure_after_adapter_start_cleans_up_the_adapter_before_returning_error() {
        let factory = Arc::new(FakeAdapterFactory::launch_error());
        let manager = SessionManager::new(config(), factory.clone());

        assert!(matches!(
            manager.launch(launch_request()).unwrap_err(),
            DebugRuntimeError::LaunchFailed(_)
        ));
        assert_eq!(
            *factory.calls.lock().unwrap(),
            vec![
                FakeAdapterCall::Initialize,
                FakeAdapterCall::Launch,
                FakeAdapterCall::Terminate,
            ],
            "launch errors after adapter startup must not leave the adapter process running"
        );
    }

    #[test]
    fn session_manager_set_breakpoints_records_verified_breakpoint_ids_and_source_locations() {
        let manager = manager();
        let result = manager.launch(launch_request()).unwrap();

        let breakpoints = manager
            .set_breakpoints(
                &result.session_id,
                vec![source_breakpoint(12), source_breakpoint(24)],
            )
            .expect("set_breakpoints should return verified breakpoints");

        assert_eq!(
            breakpoints,
            vec![
                DebugBreakpoint {
                    verified: true,
                    verified_id: Some(1),
                    ..source_breakpoint(12)
                },
                DebugBreakpoint {
                    verified: true,
                    verified_id: Some(2),
                    ..source_breakpoint(24)
                },
            ]
        );
    }

    #[test]
    fn session_manager_continue_session_and_step_block_until_stopped_terminated_or_timeout() {
        let manager = manager();
        let result = manager.launch(launch_request()).unwrap();

        let continued = manager
            .continue_session(&result.session_id, Some(Duration::from_millis(50)))
            .expect("continue should wait for a stop");
        let stepped = manager
            .step(&result.session_id, Some(1), Some(Duration::from_millis(50)))
            .expect("step should wait for a stop");

        assert_eq!(continued.state, DebugSessionState::Stopped);
        assert!(!continued.timed_out);
        assert_eq!(stepped.state, DebugSessionState::Stopped);
        assert!(!stepped.timed_out);
    }

    #[test]
    fn resume_timeout_returns_running_state_and_leaves_session_controllable() {
        let manager = manager();
        let result = manager.launch(launch_request()).unwrap();

        let timeout = manager
            .continue_session(&result.session_id, Some(Duration::from_millis(1)))
            .expect("resume timeout is a structured stop result, not a hard failure");
        let pause = manager
            .pause(&result.session_id, Some(1))
            .expect("session should remain controllable after timeout");

        assert_eq!(timeout.state, DebugSessionState::Running);
        assert!(timeout.timed_out);
        assert_eq!(
            serde_json::to_value(&timeout).unwrap(),
            json!({
                "state": "running",
                "reason": "timeout",
                "thread_id": null,
                "top_frame": null,
                "hit_breakpoint_ids": [],
                "timed_out": true,
                "error": {
                    "code": "debug-timeout",
                    "message": "debug operation timed out",
                    "data": {
                        "operation": "continue",
                        "layer": "dap",
                        "budget_ms": 1
                    }
                },
                "output_since": []
            })
        );
        assert_eq!(pause.state, DebugSessionState::Stopped);
    }

    #[test]
    fn debug_operations_preserve_absent_and_explicit_timeouts_to_the_adapter() {
        let mut unlimited_config = config();
        unlimited_config
            .languages
            .get_mut("rust")
            .unwrap()
            .default_timeout_secs = None;
        let unlimited_factory = Arc::new(FakeAdapterFactory::default());
        let unlimited = SessionManager::new(unlimited_config, unlimited_factory.clone());
        let unlimited_session = unlimited
            .launch(launch_request())
            .expect("an absent adapter timeout must remain unlimited");
        unlimited
            .set_breakpoints(&unlimited_session.session_id, vec![source_breakpoint(12)])
            .expect("unlimited breakpoint update");
        unlimited
            .continue_session(&unlimited_session.session_id, None)
            .expect("unlimited continue");
        unlimited
            .step(&unlimited_session.session_id, Some(1), None)
            .expect("unlimited step");
        unlimited
            .pause(&unlimited_session.session_id, Some(1))
            .expect("unlimited pause");
        unlimited
            .threads(&unlimited_session.session_id)
            .expect("threads");
        unlimited
            .stack(&unlimited_session.session_id, 10)
            .expect("stack");
        unlimited
            .scopes(&unlimited_session.session_id, 10)
            .expect("scopes");
        unlimited
            .variables(&unlimited_session.session_id, 100)
            .expect("variables");
        unlimited
            .evaluate(&unlimited_session.session_id, 10, "answer".to_owned())
            .expect("evaluate");

        assert_eq!(
            *unlimited_factory.timeouts.lock().unwrap(),
            vec![
                ("initialize", None),
                ("launch", None),
                ("set_breakpoints", None),
                ("stack", None),
                ("set_breakpoints", None),
                ("continue", None),
                ("step", None),
                ("pause", None),
                ("threads", None),
                ("stack", None),
                ("scopes", None),
                ("variables", None),
                ("evaluate", None),
            ],
            "every ordinary session operation must pass an absent default unchanged to the adapter"
        );

        let explicit_factory = Arc::new(FakeAdapterFactory::default());
        let explicit = SessionManager::new(config(), explicit_factory.clone());
        let explicit_session = explicit.launch(launch_request()).expect("explicit launch");
        explicit
            .set_breakpoints(&explicit_session.session_id, vec![source_breakpoint(24)])
            .expect("explicit breakpoint update");
        explicit
            .continue_session(&explicit_session.session_id, None)
            .expect("explicit continue");
        explicit
            .step(&explicit_session.session_id, Some(1), None)
            .expect("explicit step");
        explicit
            .pause(&explicit_session.session_id, Some(1))
            .expect("explicit pause");
        explicit
            .threads(&explicit_session.session_id)
            .expect("threads");
        explicit
            .stack(&explicit_session.session_id, 10)
            .expect("stack");
        explicit
            .scopes(&explicit_session.session_id, 10)
            .expect("scopes");
        explicit
            .variables(&explicit_session.session_id, 100)
            .expect("variables");
        explicit
            .evaluate(&explicit_session.session_id, 10, "answer".to_owned())
            .expect("evaluate");

        let timeouts = explicit_factory.timeouts.lock().unwrap();
        assert_eq!(
            timeouts
                .iter()
                .map(|(operation, _)| *operation)
                .collect::<Vec<_>>(),
            vec![
                "initialize",
                "launch",
                "set_breakpoints",
                "stack",
                "set_breakpoints",
                "continue",
                "step",
                "pause",
                "threads",
                "stack",
                "scopes",
                "variables",
                "evaluate"
            ]
        );
        let mut previous = Duration::from_secs(1);
        for (_, timeout) in &timeouts[..4] {
            let timeout = timeout.expect("configured launch stays finite");
            assert!(timeout <= previous && !timeout.is_zero());
            previous = timeout;
        }
        for (_, timeout) in &timeouts[4..] {
            assert_eq!(*timeout, Some(Duration::from_secs(1)));
        }
    }

    #[test]
    fn launch_and_replay_consume_one_budget_across_staged_adapter_calls() {
        for replay in [false, true] {
            for explicit in [false, true] {
                let factory = Arc::new(FakeAdapterFactory {
                    stage_delay: Duration::from_millis(10),
                    ..FakeAdapterFactory::default()
                });
                let manager = SessionManager::new(config(), factory.clone());
                if replay {
                    let mut request = replay_open_request();
                    request.timeout_secs = explicit.then_some(2);
                    manager.open_replay(request).unwrap();
                } else {
                    manager
                        .launch_with_initial_breakpoints_timeout(
                            launch_request(),
                            vec![source_breakpoint(12)],
                            explicit.then_some(Duration::from_secs(2)),
                        )
                        .unwrap();
                }
                let timeouts = factory.timeouts.lock().unwrap();
                let budget = Duration::from_secs(if explicit { 2 } else { 1 });
                assert!(timeouts[0].1.unwrap() <= budget);
                for pair in timeouts.windows(2) {
                    assert!(
                        pair[1].1.unwrap() < pair[0].1.unwrap(),
                        "each completed stage must consume the same budget: {timeouts:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn cumulative_startup_expiry_cleans_up_before_a_partial_session_is_registered() {
        for replay in [false, true] {
            let factory = Arc::new(FakeAdapterFactory {
                stage_delay: Duration::from_millis(400),
                ..FakeAdapterFactory::default()
            });
            let manager = SessionManager::new(config(), factory.clone());
            let error = if replay {
                let mut request = replay_open_request();
                request.timeout_secs = None;
                manager.open_replay(request).unwrap_err()
            } else {
                manager
                    .launch_with_initial_breakpoints(launch_request(), vec![source_breakpoint(12)])
                    .unwrap_err()
            };
            assert!(
                matches!(error, DebugRuntimeError::DebugTimeout { detail, .. } if detail.budget_ms == 1000)
            );
            assert!(manager.sessions().is_empty());
            assert_eq!(
                factory.calls.lock().unwrap().last(),
                Some(&FakeAdapterCall::Terminate)
            );
        }
    }

    #[test]
    fn launch_and_replay_stack_expiry_preserves_original_budget_and_cleans_up() {
        for replay in [false, true] {
            let factory = Arc::new(FakeAdapterFactory {
                stage_delay: Duration::from_millis(5),
                stack_timeout: true,
                ..FakeAdapterFactory::default()
            });
            let manager = SessionManager::new(config(), factory.clone());
            let error = if replay {
                let mut request = replay_open_request();
                request.timeout_secs = None;
                manager.open_replay(request).unwrap_err()
            } else {
                manager.launch(launch_request()).unwrap_err()
            };
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                json!({"code":"debug-timeout", "message":{"message":"debug operation timed out", "detail":{"operation":"stack", "layer":"dap", "budget_ms":1000}}})
            );
            assert!(manager.sessions().is_empty());
            assert_eq!(
                factory.calls.lock().unwrap().last(),
                Some(&FakeAdapterCall::Terminate)
            );
        }
    }

    #[test]
    fn invalid_direct_session_budgets_are_rejected_before_adapter_dispatch() {
        for timeout in [Duration::ZERO, Duration::from_secs(u64::MAX)] {
            let factory = Arc::new(FakeAdapterFactory::default());
            let manager = SessionManager::new(config(), factory.clone());
            assert!(matches!(
                manager.launch_with_initial_breakpoints_timeout(
                    launch_request(),
                    vec![],
                    Some(timeout)
                ),
                Err(DebugRuntimeError::InvalidParams(_))
            ));
            assert!(factory.starts.lock().unwrap().is_empty());
            let session = manager.launch(launch_request()).unwrap();
            let before = factory.timeouts.lock().unwrap().len();
            assert!(matches!(
                manager.continue_session(&session.session_id, Some(timeout)),
                Err(DebugRuntimeError::InvalidParams(_))
            ));
            assert_eq!(factory.timeouts.lock().unwrap().len(), before);
            assert_eq!(manager.sessions()[0].state, DebugSessionState::Stopped);
            let mut request = replay_open_request();
            request.timeout_secs = Some(timeout.as_secs());
            let before = factory.starts.lock().unwrap().len();
            assert!(matches!(
                manager.open_replay(request),
                Err(DebugRuntimeError::InvalidParams(_))
            ));
            assert_eq!(factory.starts.lock().unwrap().len(), before);
        }
    }

    #[test]
    fn session_manager_stack_variables_and_evaluate_return_not_stopped_while_session_is_running() {
        let manager = manager();
        let result = manager.launch(launch_request()).unwrap();
        let timeout = manager
            .continue_session(&result.session_id, Some(Duration::from_millis(1)))
            .unwrap();
        assert_eq!(timeout.state, DebugSessionState::Running);

        assert_not_stopped(manager.stack(&result.session_id, 1).unwrap_err());
        assert_not_stopped(manager.scopes(&result.session_id, 10).unwrap_err());
        assert_not_stopped(manager.variables(&result.session_id, 100).unwrap_err());
        assert_not_stopped(
            manager
                .evaluate(&result.session_id, 10, "answer".to_owned())
                .unwrap_err(),
        );
    }

    #[test]
    fn unknown_ended_or_lost_session_ids_return_session_not_found() {
        let manager = manager();
        let unknown = DebugSessionId("missing-session".to_owned());
        assert_session_not_found(manager.threads(&unknown).unwrap_err());

        let result = manager.launch(launch_request()).unwrap();
        manager.terminate(&result.session_id).unwrap();
        assert_session_not_found(manager.threads(&result.session_id).unwrap_err());

        let lost_manager =
            SessionManager::new(config(), Arc::new(FakeAdapterFactory::lost_on_threads()));
        let lost = lost_manager.launch(launch_request()).unwrap();
        assert_session_not_found(lost_manager.threads(&lost.session_id).unwrap_err());
    }

    #[test]
    fn output_events_are_buffered_per_session_and_drained_through_output_since() {
        let manager = manager();
        let result = manager.launch(launch_request()).unwrap();
        let other_result = manager.launch(launch_request()).unwrap();

        let stop = manager
            .continue_session(&result.session_id, Some(Duration::from_millis(50)))
            .unwrap();
        let other_stop = manager
            .continue_session(&other_result.session_id, Some(Duration::from_millis(50)))
            .unwrap();
        let output = manager
            .output_since(&result.session_id, 0)
            .expect("output_since should drain buffered output events");
        let other_output = manager
            .output_since(&other_result.session_id, 0)
            .expect("output_since should drain the second session independently");

        assert_eq!(stop.output_since, output);
        assert_eq!(output, fake_output_events());
        assert_eq!(other_stop.output_since, other_output);
        assert_eq!(other_output, fake_output_events());
        assert!(
            manager
                .output_since(&result.session_id, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            manager
                .output_since(&other_result.session_id, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn idle_ttl_expiry_terminates_and_reaps_the_session_process_tree() {
        let manager = manager();
        let result = manager.launch(launch_request()).unwrap();

        std::thread::sleep(Duration::from_millis(1_100));

        assert_session_not_found(manager.threads(&result.session_id).unwrap_err());
        assert!(manager.sessions().is_empty());
    }

    #[test]
    fn session_manager_shutdown_all_cleans_up_active_sessions() {
        let manager = manager();
        let result = manager.launch(launch_request()).unwrap();

        manager.shutdown_all();

        assert_session_not_found(manager.threads(&result.session_id).unwrap_err());
        assert!(manager.sessions().is_empty());
    }

    #[test]
    fn session_manager_disconnect_cleans_up_the_session_and_adapter() {
        let factory = Arc::new(FakeAdapterFactory::default());
        let manager = SessionManager::new(config(), factory.clone());
        let result = manager.launch(launch_request()).unwrap();

        manager
            .disconnect(&result.session_id)
            .expect("disconnect should detach and clean up an active debug session");

        assert!(
            factory
                .calls
                .lock()
                .unwrap()
                .contains(&FakeAdapterCall::Disconnect),
            "disconnect must be forwarded through the adapter"
        );
        assert_session_not_found(manager.threads(&result.session_id).unwrap_err());
    }

    #[test]
    fn terminate_keeps_session_registered_when_adapter_cleanup_fails() {
        let manager =
            SessionManager::new(config(), Arc::new(FakeAdapterFactory::terminate_error()));
        let result = manager.launch(launch_request()).unwrap();

        assert!(matches!(
            manager.terminate(&result.session_id).unwrap_err(),
            DebugRuntimeError::AdapterExited(_)
        ));

        assert_eq!(manager.sessions().len(), 1);
        assert!(
            manager.threads(&result.session_id).is_ok(),
            "caller must be able to retry or inspect a session whose cleanup failed"
        );
    }

    #[test]
    fn disconnect_keeps_session_registered_when_adapter_cleanup_fails() {
        let manager =
            SessionManager::new(config(), Arc::new(FakeAdapterFactory::disconnect_error()));
        let result = manager.launch(launch_request()).unwrap();

        assert!(matches!(
            manager.disconnect(&result.session_id).unwrap_err(),
            DebugRuntimeError::AdapterExited(_)
        ));

        assert_eq!(manager.sessions().len(), 1);
        assert!(
            manager.threads(&result.session_id).is_ok(),
            "caller must be able to retry or inspect a session whose cleanup failed"
        );
    }

    #[test]
    fn evaluate_reaps_session_when_adapter_exits_during_expression_evaluation() {
        let factory = Arc::new(FakeAdapterFactory::lost_on_evaluate());
        let manager = SessionManager::new(config(), factory.clone());
        let result = manager.launch(launch_request()).unwrap();
        manager
            .continue_session(&result.session_id, None)
            .expect("session should be stopped before evaluate");

        assert_session_not_found(
            manager
                .evaluate(&result.session_id, 1, "answer".to_owned())
                .unwrap_err(),
        );

        assert!(
            factory
                .calls
                .lock()
                .unwrap()
                .contains(&FakeAdapterCall::Terminate)
        );
        assert!(manager.sessions().is_empty());
    }

    #[test]
    fn session_kind_exists_with_exact_variants_live_and_replay_and_session_entry_stores_kind() {
        let manager = manager();
        let live = manager.launch(launch_request()).unwrap();
        let replay = manager
            .open_replay(replay_open_request())
            .expect("valid trace id should open a replay session");

        assert_eq!(
            manager.session_kind(&live.session_id).unwrap(),
            SessionKind::Live
        );
        assert_eq!(
            manager.session_kind(&replay.session_id).unwrap(),
            SessionKind::Replay {
                trace_id: trace_id(),
                supports_step_back: true,
            }
        );
    }

    #[test]
    fn replay_open_request_exists_with_exact_public_fields_and_none_adapter_selects_configured_rr_replay_adapter()
     {
        let request = replay_open_request();
        let serialized = serde_json::to_value(&request).unwrap();

        assert_eq!(
            serialized,
            json!({
                "trace_id": "trace-replay-1",
                "language": "rust",
                "timeout_secs": 2,
                "adapter": null,
                "trace_path": null,
                "adapter_args": ["--chaos"]
            })
        );

        let round_trip: ReplayOpenRequest = serde_json::from_value(serialized).unwrap();
        assert_eq!(round_trip, request);
    }

    #[test]
    fn replay_result_exists_with_public_rust_fields_and_supports_step_back_json_field() {
        let result = ReplayResult {
            session_id: DebugSessionId("debug-replay-1".to_owned()),
            trace_id: trace_id(),
            state: DebugSessionState::Stopped,
            stop: Some(stopped_at_breakpoint()),
            supports_step_back: true,
        };

        let serialized = serde_json::to_value(&result).unwrap();

        assert_eq!(serialized["session_id"], "debug-replay-1");
        assert_eq!(serialized["trace_id"], "trace-replay-1");
        assert_eq!(serialized["state"], "stopped");
        assert_eq!(serialized["supportsStepBack"], true);
        assert!(serialized.get("supports_step_back").is_none());
        assert!(serialized["stop"].is_object());
    }

    #[test]
    fn session_manager_open_replay_opens_a_replay_session_from_valid_trace_id_and_returns_normal_session_id()
     {
        let manager = manager();

        let result = manager
            .open_replay(replay_open_request())
            .expect("open_replay should accept a valid trace id");

        assert!(!result.session_id.0.is_empty());
        assert_eq!(result.trace_id, trace_id());
        assert_eq!(result.state, DebugSessionState::Stopped);
        assert_eq!(
            manager.session_kind(&result.session_id).unwrap(),
            SessionKind::Replay {
                trace_id: trace_id(),
                supports_step_back: result.supports_step_back,
            }
        );
    }

    #[test]
    fn session_manager_session_kind_exists_so_tool_handlers_can_reject_live_sessions_without_duplicating_registry_logic()
     {
        let manager = manager();
        let live = manager.launch(launch_request()).unwrap();

        assert_eq!(
            manager.session_kind(&live.session_id).unwrap(),
            SessionKind::Live
        );
        assert_session_not_found(
            manager
                .session_kind(&DebugSessionId("missing".to_owned()))
                .unwrap_err(),
        );
    }

    #[test]
    fn existing_forward_operations_continue_to_work_on_replay_sessions_through_existing_debug_adapter_session_boundary()
     {
        let manager = manager();
        let replay = manager
            .open_replay(replay_open_request())
            .expect("valid replay session should open");

        let threads = manager.threads(&replay.session_id).unwrap();
        let frames = manager.stack(&replay.session_id, 1).unwrap();
        let variable = manager
            .evaluate(&replay.session_id, 10, "answer".to_owned())
            .unwrap();

        assert_eq!(threads[0].id, 1);
        assert_eq!(frames, vec![top_frame()]);
        assert_eq!(variable.value, "42");
    }

    #[test]
    fn replay_sessions_remain_read_only_by_reusing_replay_adapter_paths_without_launch_or_breakpoint_mutation_commands()
     {
        let factory = Arc::new(FakeAdapterFactory::default());
        let manager = SessionManager::new(config(), factory.clone());
        let replay = manager
            .open_replay(replay_open_request())
            .expect("valid replay session should open without launching a live debuggee");

        manager
            .reverse_continue(&replay.session_id, Some(1), Some(Duration::from_millis(50)))
            .expect("reverse continue should use the replay adapter");
        manager
            .step_back(
                &replay.session_id,
                Some(1),
                StepBackGranularity::Line,
                Some(Duration::from_millis(50)),
            )
            .expect("step back should use the replay adapter");
        manager
            .set_watchpoint(&replay.session_id, watchpoint_spec())
            .expect("watchpoint should use the replay adapter");

        let calls = factory.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, FakeAdapterCall::Launch))
                .count(),
            1,
            "replay sessions open the trace-backed adapter exactly once"
        );
        assert!(
            !calls
                .iter()
                .any(|call| matches!(call, FakeAdapterCall::SetBreakpoints(_))),
            "replay-specific operations must not configure live breakpoints"
        );
    }

    #[test]
    fn debug_adapter_session_adds_exact_reverse_continue_step_back_set_watchpoint_and_seek_replay_methods()
     {
        let mut session = FakeAdapterSession::default();

        assert_eq!(
            session
                .reverse_continue(Some(7), Some(Duration::from_millis(50)))
                .unwrap()
                .reason,
            Some("reverse".to_owned())
        );
        assert_eq!(
            session
                .step_back(
                    Some(7),
                    StepBackGranularity::Instruction,
                    Some(Duration::from_millis(50)),
                )
                .unwrap()
                .reason,
            Some("stepBack".to_owned())
        );
        assert_eq!(
            session
                .set_watchpoint(watchpoint_spec(), Some(Duration::from_millis(50)))
                .unwrap()
                .watchpoint_id,
            "watch-1"
        );
        assert_eq!(
            session
                .seek_replay(ReplaySeekTarget::Crash, Some(Duration::from_millis(50)))
                .unwrap()
                .reason,
            Some("seek".to_owned())
        );
    }

    #[test]
    fn open_replay_with_no_adapter_uses_configured_rr_replay_adapter_and_launches_trace_before_registering_session()
     {
        let factory = Arc::new(FakeAdapterFactory::default());
        let manager = SessionManager::new(config(), factory.clone());

        manager
            .open_replay(replay_open_request())
            .expect("valid replay session should open through configured rr replay adapter");

        let starts = factory.starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].program, "rr");
        assert_eq!(starts[0].args, vec!["replay", "--chaos"]);
        assert_eq!(
            starts[0].launch_overrides["trace_id"],
            serde_json::json!("trace-replay-1")
        );

        let calls = factory.calls.lock().unwrap();
        assert_eq!(
            &calls[..2],
            &[FakeAdapterCall::Initialize, FakeAdapterCall::Launch],
            "replay open must attach/open the trace before registering the session"
        );
    }

    #[test]
    fn open_replay_launch_failure_terminates_adapter_and_does_not_register_session() {
        let factory = Arc::new(FakeAdapterFactory::launch_error());
        let manager = SessionManager::new(config(), factory.clone());

        let error = manager
            .open_replay(replay_open_request())
            .expect_err("replay launch failure must be returned");

        assert_eq!(
            error,
            DebugRuntimeError::LaunchFailed("fake adapter rejected launch".to_owned())
        );
        assert!(manager.sessions().is_empty());
        assert_eq!(
            factory.calls.lock().unwrap().as_slice(),
            &[
                FakeAdapterCall::Initialize,
                FakeAdapterCall::Launch,
                FakeAdapterCall::Terminate,
            ]
        );
    }

    #[test]
    fn step_back_granularity_exists_with_exact_serde_string_values_line_instruction_and_over() {
        assert_eq!(
            serde_json::to_value(StepBackGranularity::Line).unwrap(),
            "line"
        );
        assert_eq!(
            serde_json::to_value(StepBackGranularity::Instruction).unwrap(),
            "instruction"
        );
        assert_eq!(
            serde_json::to_value(StepBackGranularity::Over).unwrap(),
            "over"
        );
    }

    #[test]
    fn watchpoint_kind_exists_with_exact_serde_string_values_and_watchpoint_spec_has_public_fields()
    {
        let spec = watchpoint_spec();
        let serialized = serde_json::to_value(&spec).unwrap();

        assert_eq!(
            serialized,
            json!({
                "expression": "counter",
                "address": null,
                "kind": "write",
                "enabled": true
            })
        );
        assert_eq!(serde_json::to_value(WatchpointKind::Read).unwrap(), "read");
        assert_eq!(
            serde_json::to_value(WatchpointKind::Access).unwrap(),
            "access"
        );
    }

    #[test]
    fn watchpoint_result_exists_with_exact_public_fields() {
        let result = WatchpointResult {
            watchpoint_id: "watch-1".to_owned(),
            expression: Some("counter".to_owned()),
            address: None,
            kind: WatchpointKind::Write,
            enabled: true,
            verified: true,
        };

        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "watchpoint_id": "watch-1",
                "expression": "counter",
                "address": null,
                "kind": "write",
                "enabled": true,
                "verified": true
            })
        );
    }

    #[test]
    fn replay_seek_target_exists_with_exact_variants_crash_end_and_source_path_line_column() {
        assert_eq!(
            serde_json::to_value(ReplaySeekTarget::Crash).unwrap(),
            "Crash"
        );
        assert_eq!(serde_json::to_value(ReplaySeekTarget::End).unwrap(), "End");
        assert_eq!(
            serde_json::to_value(ReplaySeekTarget::Source {
                path: "src/main.rs".to_owned(),
                line: 12,
                column: Some(5),
            })
            .unwrap(),
            json!({ "Source": { "path": "src/main.rs", "line": 12, "column": 5 } })
        );
    }

    #[test]
    fn existing_live_adapters_use_default_debug_adapter_session_implementations_that_return_reverse_unsupported()
     {
        struct LiveOnlyAdapter;

        impl DebugAdapterSession for LiveOnlyAdapter {
            fn initialize(&mut self, _timeout: Option<Duration>) -> Result<(), DebugRuntimeError> {
                Ok(())
            }

            fn launch(
                &mut self,
                _request: &LaunchRequest,
                _timeout: Option<Duration>,
            ) -> Result<(), DebugRuntimeError> {
                Ok(())
            }

            fn set_breakpoints(
                &mut self,
                breakpoints: &[DebugBreakpoint],
                _timeout: Option<Duration>,
            ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError> {
                Ok(breakpoints.to_vec())
            }

            fn continue_session(
                &mut self,
                _timeout: Option<Duration>,
            ) -> Result<DebugStop, DebugRuntimeError> {
                Ok(stopped_at_breakpoint())
            }

            fn step(
                &mut self,
                _thread_id: Option<u64>,
                _timeout: Option<Duration>,
            ) -> Result<DebugStop, DebugRuntimeError> {
                Ok(stopped_at_breakpoint())
            }

            fn pause(
                &mut self,
                _thread_id: Option<u64>,
                _timeout: Option<Duration>,
            ) -> Result<DebugStop, DebugRuntimeError> {
                Ok(stopped_at_breakpoint())
            }

            fn threads(
                &mut self,
                _timeout: Option<Duration>,
            ) -> Result<Vec<DebugThread>, DebugRuntimeError> {
                Ok(Vec::new())
            }

            fn stack(
                &mut self,
                _thread_id: u64,
                _timeout: Option<Duration>,
            ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
                Ok(Vec::new())
            }

            fn scopes(
                &mut self,
                _frame_id: u64,
                _timeout: Option<Duration>,
            ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
                Ok(Vec::new())
            }

            fn variables(
                &mut self,
                _variables_reference: u64,
                _timeout: Option<Duration>,
            ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
                Ok(Vec::new())
            }

            fn evaluate(
                &mut self,
                _frame_id: u64,
                expression: &str,
                _timeout: Option<Duration>,
            ) -> Result<DebugVariable, DebugRuntimeError> {
                Ok(DebugVariable {
                    name: expression.to_owned(),
                    value: "42".to_owned(),
                    r#type: None,
                    variables_reference: 0,
                })
            }

            fn terminate(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
                Ok(())
            }

            fn disconnect(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
                Ok(())
            }
        }

        let mut adapter = LiveOnlyAdapter;

        assert_reverse_unsupported(
            DebugAdapterSession::reverse_continue(
                &mut adapter,
                None,
                Some(Duration::from_millis(1)),
            )
            .unwrap_err(),
        );
        assert_reverse_unsupported(
            DebugAdapterSession::step_back(
                &mut adapter,
                None,
                StepBackGranularity::Line,
                Some(Duration::from_millis(1)),
            )
            .unwrap_err(),
        );
        assert_reverse_unsupported(
            DebugAdapterSession::set_watchpoint(
                &mut adapter,
                watchpoint_spec(),
                Some(Duration::from_millis(1)),
            )
            .unwrap_err(),
        );
        assert_reverse_unsupported(
            DebugAdapterSession::seek_replay(
                &mut adapter,
                ReplaySeekTarget::Crash,
                Some(Duration::from_millis(1)),
            )
            .unwrap_err(),
        );
    }

    #[test]
    fn debug_runtime_error_adds_reverse_unsupported_serialized_with_code_reverse_unsupported_and_tool_payloads_surface_runtime_failure_code()
     {
        let value = serde_json::to_value(DebugRuntimeError::ReverseUnsupported(
            "reverse_unsupported".into(),
        ))
        .unwrap();

        assert_eq!(
            value,
            json!({ "code": "reverse_unsupported", "message": "reverse_unsupported" })
        );
    }

    #[test]
    fn live_non_replay_sessions_return_reverse_unsupported_for_reverse_continue_step_back_and_set_watchpoint()
     {
        let manager = manager();
        let live = manager.launch(launch_request()).unwrap();

        assert_reverse_unsupported(
            manager
                .reverse_continue(&live.session_id, Some(1), Some(Duration::from_millis(50)))
                .unwrap_err(),
        );
        assert_reverse_unsupported(
            manager
                .step_back(
                    &live.session_id,
                    Some(1),
                    StepBackGranularity::Line,
                    Some(Duration::from_millis(50)),
                )
                .unwrap_err(),
        );
        assert_reverse_unsupported(
            manager
                .set_watchpoint(&live.session_id, watchpoint_spec())
                .unwrap_err(),
        );
    }

    #[test]
    fn replay_adapter_timeout_during_reverse_capable_and_seek_operations_maps_to_same_timeout_semantics_as_existing_resume_step_paths()
     {
        let manager = manager();
        let replay = manager.open_replay(replay_open_request()).unwrap();

        let timeout = manager
            .reverse_continue(&replay.session_id, Some(1), Some(Duration::from_millis(1)))
            .expect("reverse timeout should be a structured running stop");

        assert_eq!(timeout.state, DebugSessionState::Running);
        assert!(timeout.timed_out);

        let seek_timeout = manager
            .seek_replay(
                &replay.session_id,
                ReplaySeekTarget::End,
                Some(Duration::from_millis(1)),
            )
            .expect("seek timeout should be a structured running stop");

        assert_eq!(seek_timeout.state, DebugSessionState::Running);
        assert!(seek_timeout.timed_out);
    }

    #[test]
    fn unsupported_replay_reverse_request_restores_previous_stopped_state() {
        let factory = Arc::new(FakeAdapterFactory::reverse_unsupported());
        let manager = SessionManager::new(config(), factory);
        let replay = manager.open_replay(replay_open_request()).unwrap();

        assert_reverse_unsupported(
            manager
                .reverse_continue(&replay.session_id, Some(1), Some(Duration::from_millis(50)))
                .unwrap_err(),
        );

        manager
            .stack(&replay.session_id, 1)
            .expect("failed reverse operation must leave replay session stopped");
    }

    #[test]
    fn adapter_exit_during_replay_preserves_adapter_exited_state_without_panicking() {
        let manager = SessionManager::new(
            config(),
            Arc::new(FakeAdapterFactory::reverse_adapter_exit()),
        );
        let replay = manager.open_replay(replay_open_request()).unwrap();

        assert_session_not_found(
            manager
                .reverse_continue(&replay.session_id, Some(1), Some(Duration::from_millis(50)))
                .unwrap_err(),
        );
        assert!(manager.sessions().is_empty());
    }

    #[test]
    fn terminate_disconnect_and_shutdown_cleanup_replay_sessions_through_same_manager_paths_as_live_sessions()
     {
        let factory = Arc::new(FakeAdapterFactory::default());
        let manager = SessionManager::new(config(), factory.clone());
        let terminate_replay = manager.open_replay(replay_open_request()).unwrap();
        let disconnect_replay = manager.open_replay(replay_open_request()).unwrap();
        let shutdown_replay = manager.open_replay(replay_open_request()).unwrap();

        manager.terminate(&terminate_replay.session_id).unwrap();
        manager.disconnect(&disconnect_replay.session_id).unwrap();
        manager.shutdown_all();

        assert!(
            factory
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| matches!(call, FakeAdapterCall::Terminate))
                .count()
                >= 2
        );
        assert!(
            factory
                .calls
                .lock()
                .unwrap()
                .contains(&FakeAdapterCall::Disconnect)
        );
        assert_session_not_found(manager.threads(&shutdown_replay.session_id).unwrap_err());
    }

    #[test]
    fn existing_eval_at_launch_session_behavior_remains_stable() {
        let factory = Arc::new(FakeAdapterFactory::default());
        let manager = SessionManager::new(config(), factory.clone());

        let result = manager.launch(launch_request()).unwrap();

        assert_eq!(result.state, DebugSessionState::Stopped);
        assert_eq!(
            *factory.calls.lock().unwrap(),
            vec![
                FakeAdapterCall::Initialize,
                FakeAdapterCall::Launch,
                FakeAdapterCall::SetBreakpoints(Vec::new()),
            ]
        );
    }
}
