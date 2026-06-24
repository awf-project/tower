#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::protocol::DebugInitializeConfig;
use crate::types::{
    DebugBreakpoint, DebugOutput, DebugRuntimeError, DebugScope, DebugSessionId, DebugSessionState,
    DebugStackFrame, DebugStop, DebugThread, DebugVariable,
};

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
    state: DebugSessionState,
    last_stop: Option<DebugStop>,
    output: Vec<DebugOutput>,
    adapter: Box<dyn DebugAdapterSession>,
    timeout: Duration,
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

    pub fn launch(&self, params: LaunchRequest) -> Result<LaunchResult, DebugRuntimeError> {
        self.launch_with_initial_breakpoints(params, Vec::new())
            .map(|(result, _)| result)
    }

    pub fn launch_with_initial_breakpoints(
        &self,
        params: LaunchRequest,
        initial_breakpoints: Vec<DebugBreakpoint>,
    ) -> Result<(LaunchResult, Vec<DebugBreakpoint>), DebugRuntimeError> {
        let adapter_config = self.config.languages.get(&params.language).ok_or_else(|| {
            DebugRuntimeError::LaunchFailed(format!(
                "no debug adapter configured for language {}",
                params.language
            ))
        })?;
        let timeout = Duration::from_secs(adapter_config.default_timeout_secs);
        let idle_ttl = Duration::from_secs(adapter_config.idle_ttl_secs);

        let mut adapter = self.adapter_factory.start(&params)?;
        if let Err(error) = adapter.initialize(timeout) {
            let _ = adapter.terminate(timeout);
            return Err(error);
        }
        if let Err(error) = adapter.launch(&params, timeout) {
            let _ = adapter.terminate(timeout);
            return Err(error);
        }
        let configured_breakpoints = if initial_breakpoints.is_empty() {
            Vec::new()
        } else {
            match adapter.set_breakpoints(&initial_breakpoints, timeout) {
                Ok(breakpoints) => breakpoints,
                Err(error) => {
                    let _ = adapter.terminate(timeout);
                    return Err(error);
                }
            }
        };
        if let Err(error) = adapter.set_breakpoints(&[], timeout) {
            let _ = adapter.terminate(timeout);
            return Err(error);
        }

        let top_frame = adapter
            .stack(1, timeout)
            .ok()
            .and_then(|frames| frames.into_iter().next());
        let stop = DebugStop {
            state: DebugSessionState::Stopped,
            reason: Some("breakpoint".to_owned()),
            thread_id: Some(1),
            top_frame,
            hit_breakpoint_ids: vec![1],
            timed_out: false,
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
        self.with_stopped_session(session_id, |session| {
            session.adapter.stack(thread_id, session.timeout)
        })
    }

    pub fn scopes(
        &self,
        session_id: &DebugSessionId,
        frame_id: u64,
    ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
        self.with_stopped_session(session_id, |session| {
            session.adapter.scopes(frame_id, session.timeout)
        })
    }

    pub fn variables(
        &self,
        session_id: &DebugSessionId,
        variables_reference: u64,
    ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
        self.with_stopped_session(session_id, |session| {
            session
                .adapter
                .variables(variables_reference, session.timeout)
        })
    }

    pub fn evaluate(
        &self,
        session_id: &DebugSessionId,
        frame_id: u64,
        expression: String,
    ) -> Result<DebugVariable, DebugRuntimeError> {
        self.with_stopped_session(session_id, |session| {
            session
                .adapter
                .evaluate(frame_id, &expression, session.timeout)
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
            session.adapter.terminate(session.timeout)
        };
        if result.is_ok() {
            state.sessions.remove(&session_id.0);
        };
        result
    }

    pub fn disconnect(&self, session_id: &DebugSessionId) -> Result<(), DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let result = {
            let Some(session) = state.sessions.get_mut(&session_id.0) else {
                return Err(session_not_found(session_id));
            };
            session.last_activity = Instant::now();
            session.adapter.disconnect(session.timeout)
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
            let _ = session.adapter.terminate(session.timeout);
        }
    }

    fn resume(
        &self,
        session_id: &DebugSessionId,
        timeout: Option<Duration>,
        resume: impl FnOnce(
            &mut dyn DebugAdapterSession,
            Duration,
        ) -> Result<DebugStop, DebugRuntimeError>,
    ) -> Result<DebugStop, DebugRuntimeError> {
        let mut state = self.state.lock().unwrap();
        self.cleanup_expired_locked(&mut state);
        let Some(session) = state.sessions.get_mut(&session_id.0) else {
            return Err(session_not_found(session_id));
        };
        session.last_activity = Instant::now();
        session.state = DebugSessionState::Running;
        let timeout = timeout.unwrap_or(session.timeout);
        match resume(session.adapter.as_mut(), timeout) {
            Ok(stop) => {
                update_session_stop(session, &stop);
                Ok(stop)
            }
            Err(DebugRuntimeError::AdapterExited(_)) => {
                Err(Self::reap_adapter_exit_locked(&mut state, session_id))
            }
            Err(DebugRuntimeError::DebugTimeout(_)) => {
                let stop = DebugStop {
                    state: DebugSessionState::Running,
                    reason: None,
                    thread_id: None,
                    top_frame: None,
                    hit_breakpoint_ids: Vec::new(),
                    timed_out: true,
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

    fn reap_adapter_exit_locked(
        state: &mut ManagerState,
        session_id: &DebugSessionId,
    ) -> DebugRuntimeError {
        if let Some(mut session) = state.sessions.remove(&session_id.0) {
            let _ = session.adapter.terminate(session.timeout);
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
                let _ = session.adapter.terminate(session.timeout);
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

fn session_not_found(session_id: &DebugSessionId) -> DebugRuntimeError {
    DebugRuntimeError::SessionNotFound(format!("debug session {} was not found", session_id.0))
}

pub trait DebugAdapterFactory: Send + Sync {
    fn start(
        &self,
        request: &LaunchRequest,
    ) -> Result<Box<dyn DebugAdapterSession>, DebugRuntimeError>;
}

pub trait DebugAdapterSession: Send {
    fn initialize(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError>;
    fn launch(
        &mut self,
        request: &LaunchRequest,
        timeout: Duration,
    ) -> Result<(), DebugRuntimeError>;
    fn set_breakpoints(
        &mut self,
        breakpoints: &[DebugBreakpoint],
        timeout: Duration,
    ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError>;
    fn continue_session(&mut self, timeout: Duration) -> Result<DebugStop, DebugRuntimeError>;
    fn step(
        &mut self,
        thread_id: Option<u64>,
        timeout: Duration,
    ) -> Result<DebugStop, DebugRuntimeError>;
    fn pause(
        &mut self,
        thread_id: Option<u64>,
        timeout: Duration,
    ) -> Result<DebugStop, DebugRuntimeError>;
    fn threads(&mut self, timeout: Duration) -> Result<Vec<DebugThread>, DebugRuntimeError>;
    fn stack(
        &mut self,
        thread_id: u64,
        timeout: Duration,
    ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError>;
    fn scopes(
        &mut self,
        frame_id: u64,
        timeout: Duration,
    ) -> Result<Vec<DebugScope>, DebugRuntimeError>;
    fn variables(
        &mut self,
        variables_reference: u64,
        timeout: Duration,
    ) -> Result<Vec<DebugVariable>, DebugRuntimeError>;
    fn evaluate(
        &mut self,
        frame_id: u64,
        expression: &str,
        timeout: Duration,
    ) -> Result<DebugVariable, DebugRuntimeError>;
    fn terminate(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError>;
    fn disconnect(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError>;
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

    use serde_json::Map;

    use crate::protocol::{DebugAdapterConfig, DebugInitializeConfig};

    use super::{
        DebugAdapterFactory, DebugAdapterSession, DebugBreakpoint, DebugOutput, DebugRuntimeError,
        DebugScope, DebugSessionId, DebugSessionState, DebugStackFrame, DebugStop, DebugThread,
        DebugVariable, LaunchRequest, SessionManager,
    };

    #[derive(Default)]
    struct FakeAdapterFactory {
        starts: Arc<Mutex<Vec<LaunchRequest>>>,
        calls: Arc<Mutex<Vec<FakeAdapterCall>>>,
        lost_on_threads: bool,
        lost_on_evaluate: bool,
        launch_error: bool,
        terminate_error: bool,
        disconnect_error: bool,
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
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeAdapterCall {
        Initialize,
        Launch,
        SetBreakpoints(Vec<u64>),
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
                lost_on_threads: self.lost_on_threads,
                lost_on_evaluate: self.lost_on_evaluate,
                launch_error: self.launch_error,
                terminate_error: self.terminate_error,
                disconnect_error: self.disconnect_error,
                ..FakeAdapterSession::default()
            }))
        }
    }

    #[derive(Default)]
    struct FakeAdapterSession {
        calls: Arc<Mutex<Vec<FakeAdapterCall>>>,
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
    }

    impl DebugAdapterSession for FakeAdapterSession {
        fn initialize(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeAdapterCall::Initialize);
            self.initialized = true;
            Ok(())
        }

        fn launch(
            &mut self,
            _request: &LaunchRequest,
            _timeout: Duration,
        ) -> Result<(), DebugRuntimeError> {
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
            _timeout: Duration,
        ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError> {
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

        fn continue_session(&mut self, timeout: Duration) -> Result<DebugStop, DebugRuntimeError> {
            if timeout <= Duration::from_millis(1) {
                return Err(DebugRuntimeError::DebugTimeout(
                    "fake adapter timed out waiting for stop".to_owned(),
                ));
            }

            Ok(DebugStop {
                output_since: fake_output_events(),
                ..stopped_at_breakpoint()
            })
        }

        fn step(
            &mut self,
            _thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            Ok(stopped_at_breakpoint())
        }

        fn pause(
            &mut self,
            thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            Ok(DebugStop {
                thread_id,
                reason: Some("pause".to_owned()),
                ..stopped_at_breakpoint()
            })
        }

        fn stack(
            &mut self,
            _thread_id: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
            Ok(vec![top_frame()])
        }

        fn scopes(
            &mut self,
            _frame_id: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
            Ok(vec![DebugScope {
                name: "Locals".to_owned(),
                variables_reference: 100,
                expensive: false,
            }])
        }

        fn variables(
            &mut self,
            _variables_reference: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
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
            _timeout: Duration,
        ) -> Result<DebugVariable, DebugRuntimeError> {
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

        fn threads(&mut self, _timeout: Duration) -> Result<Vec<DebugThread>, DebugRuntimeError> {
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
                    default_timeout_secs: 1,
                    idle_ttl_secs: 1,
                },
            )]),
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
    fn resume_timeout_returns_running_state_and_timed_out_true_while_leaving_session_controllable()
    {
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
        assert_eq!(pause.state, DebugSessionState::Stopped);
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
}
