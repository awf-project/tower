#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::eval_at::{EvalAtRequest, run_eval_at};
pub use crate::origin::{
    FindOriginRequest, FindOriginResult, OriginFailureCode, OriginTarget,
    RecordAndFindOriginRequest, RecordAndFindOriginResult, RecordOriginParams,
};
use crate::protocol::{DebugToolError, DebugToolErrorCode};
use crate::rr::{RrRecordRequest, RrRecordResult, RrRuntime};
use crate::session::{
    DebugSessionSummary, LaunchRequest, LaunchResult, ReplayOpenRequest, ReplayResult,
    SessionManager, StepBackGranularity, WatchpointKind, WatchpointResult, WatchpointSpec,
};
use crate::traces::{TraceId, TraceMetadata, TraceStoreError};
use crate::types::{
    DebugBreakpoint, DebugRuntimeError, DebugSessionId, DebugStackFrame, DebugStop, DebugThread,
    DebugVariable, RuntimeFailure, RuntimeFailureResult,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchParams {
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
pub struct SetBreakpointsParams {
    pub session_id: DebugSessionId,
    pub path: String,
    pub breakpoints: Vec<SourceBreakpoint>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceBreakpoint {
    pub line: u64,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContinueParams {
    pub session_id: DebugSessionId,
    pub thread_id: Option<u64>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepParams {
    pub session_id: DebugSessionId,
    pub thread_id: Option<u64>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PauseParams {
    pub session_id: DebugSessionId,
    pub thread_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadsParams {
    pub session_id: DebugSessionId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StackParams {
    pub session_id: DebugSessionId,
    pub thread_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VariablesParams {
    pub session_id: DebugSessionId,
    pub variables_reference: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluateParams {
    pub session_id: DebugSessionId,
    pub frame_id: u64,
    pub expression: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordParams {
    pub language: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayParams {
    pub trace_id: TraceId,
    pub language: String,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminateParams {
    pub session_id: DebugSessionId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisconnectParams {
    pub session_id: DebugSessionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsParams {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseContinueParams {
    pub session_id: DebugSessionId,
    pub thread_id: Option<u64>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepBackParams {
    pub session_id: DebugSessionId,
    pub thread_id: Option<u64>,
    pub granularity: Option<StepBackGranularity>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchpointParams {
    pub session_id: DebugSessionId,
    pub expression: Option<String>,
    pub address: Option<String>,
    pub kind: WatchpointKind,
    pub enabled: Option<bool>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TracesParams {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteTraceParams {
    pub trace_id: TraceId,
}

pub type LaunchToolResult = LaunchResult;
pub type ReplayToolResult = ReplayResult;
pub type RecordResult = RrRecordResult;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetBreakpointsResult {
    pub breakpoints: Vec<DebugBreakpoint>,
}

pub type ContinueResult = DebugStop;
pub type StepResult = DebugStop;
pub type PauseResult = DebugStop;
pub type ReverseContinueResult = DebugStop;
pub type StepBackResult = DebugStop;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadsResult {
    pub threads: Vec<DebugThread>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StackResult {
    pub frames: Vec<DebugStackFrame>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VariablesResult {
    pub variables: Vec<DebugVariable>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluateResult {
    pub result: DebugVariable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupResult {
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionsResult {
    pub sessions: Vec<DebugSessionSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WatchpointToolResult {
    pub ok: bool,
    pub watchpoint: Option<WatchpointResult>,
    pub error: Option<RuntimeFailure>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TracesResult {
    pub traces: Vec<TraceMetadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeleteTraceResult {
    pub deleted: bool,
    pub trace_id: TraceId,
    pub error: Option<RuntimeFailure>,
}

pub fn tower_debug_launch(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: LaunchParams = parse_params(params)?;
    let request = LaunchRequest {
        language: params.language,
        program: params.program,
        cwd: params.cwd,
        args: params.args,
        env: params.env,
        launch_overrides: params.launch_overrides,
    };
    tool_result(sessions.launch(request))
}

pub fn tower_debug_replay(
    params: Value,
    sessions: &SessionManager,
    rr_runtime: &RrRuntime,
) -> Result<Value, DebugToolError> {
    let params: ReplayParams = parse_params(params)?;
    let trace = match rr_runtime.store.trace(&params.trace_id) {
        Ok(trace) => trace,
        Err(error) => {
            return serialize(RuntimeFailureResult {
                ok: false,
                error: trace_store_failure(error),
            });
        }
    };
    tool_result(sessions.open_replay(ReplayOpenRequest {
        trace_id: params.trace_id,
        trace_path: Some(trace.path),
        language: params.language,
        timeout_secs: params.timeout_secs,
        adapter: None,
        adapter_args: Vec::new(),
    }))
}

pub fn tower_debug_set_breakpoints(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: SetBreakpointsParams = parse_params(params)?;
    let breakpoints = params
        .breakpoints
        .into_iter()
        .map(|breakpoint| DebugBreakpoint {
            path: params.path.clone(),
            line: breakpoint.line,
            condition: breakpoint.condition,
            hit_condition: breakpoint.hit_condition,
            verified: false,
            verified_id: None,
        })
        .collect();
    tool_result(
        sessions
            .set_breakpoints(&params.session_id, breakpoints)
            .map(|breakpoints| SetBreakpointsResult { breakpoints }),
    )
}

pub fn tower_debug_continue(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: ContinueParams = parse_params(params)?;
    tool_result(sessions.continue_session(&params.session_id, timeout(params.timeout_secs)))
}

pub fn tower_debug_step(params: Value, sessions: &SessionManager) -> Result<Value, DebugToolError> {
    let params: StepParams = parse_params(params)?;
    tool_result(sessions.step(
        &params.session_id,
        params.thread_id,
        timeout(params.timeout_secs),
    ))
}

pub fn tower_debug_pause(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: PauseParams = parse_params(params)?;
    tool_result(sessions.pause(&params.session_id, params.thread_id))
}

pub fn tower_debug_reverse_continue(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: ReverseContinueParams = parse_params(params)?;
    tool_result(sessions.reverse_continue(
        &params.session_id,
        params.thread_id,
        timeout(params.timeout_secs),
    ))
}

pub fn tower_debug_step_back(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: StepBackParams = parse_params(params)?;
    tool_result(sessions.step_back(
        &params.session_id,
        params.thread_id,
        params.granularity.unwrap_or(StepBackGranularity::Line),
        timeout(params.timeout_secs),
    ))
}

pub fn tower_debug_watchpoint(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: WatchpointParams = parse_params(params)?;
    let watchpoint = WatchpointSpec {
        expression: params.expression,
        address: params.address,
        kind: params.kind,
        enabled: params.enabled.unwrap_or(true),
    };
    match sessions.set_watchpoint(&params.session_id, watchpoint) {
        Ok(watchpoint) => serialize(WatchpointToolResult {
            ok: true,
            watchpoint: Some(watchpoint),
            error: None,
        }),
        Err(error) => {
            let failure = runtime_failure(error).error;
            serialize(WatchpointToolResult {
                ok: false,
                watchpoint: None,
                error: Some(failure),
            })
        }
    }
}

pub fn tower_debug_threads(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: ThreadsParams = parse_params(params)?;
    tool_result(
        sessions
            .threads(&params.session_id)
            .map(|threads| ThreadsResult { threads }),
    )
}

pub fn tower_debug_stack(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: StackParams = parse_params(params)?;
    tool_result(
        sessions
            .stack(&params.session_id, params.thread_id)
            .map(|frames| StackResult { frames }),
    )
}

pub fn tower_debug_variables(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: VariablesParams = parse_params(params)?;
    tool_result(
        sessions
            .variables(&params.session_id, params.variables_reference)
            .map(|variables| VariablesResult { variables }),
    )
}

pub fn tower_debug_evaluate(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: EvaluateParams = parse_params(params)?;
    tool_result(
        sessions
            .evaluate(&params.session_id, params.frame_id, params.expression)
            .map(|result| EvaluateResult { result }),
    )
}

pub fn tower_debug_eval_at(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: EvalAtRequest = parse_params(params)?;
    match run_eval_at(params, sessions) {
        Ok(result) => serialize(result),
        Err(error) => serialize(tool_error_failure(error)),
    }
}

pub fn tower_debug_record(
    params: Value,
    rr_runtime: &mut RrRuntime,
) -> Result<Value, DebugToolError> {
    let params: RecordParams = parse_params(params)?;
    let request = RrRecordRequest {
        language: params.language,
        program: params.program,
        args: params.args,
        cwd: params.cwd,
        env: params.env,
        timeout_ms: params.timeout_ms,
        trace_policy: rr_runtime.store.policy().clone(),
    };
    serialize(rr_runtime.record(request))
}

pub fn tower_debug_traces(params: Value, rr_runtime: &RrRuntime) -> Result<Value, DebugToolError> {
    let _params: TracesParams = parse_params(params)?;
    match rr_runtime.store.list_traces() {
        Ok(traces) => serialize(TracesResult { traces }),
        Err(error) => serialize(RuntimeFailureResult {
            ok: false,
            error: trace_store_failure(error),
        }),
    }
}

pub fn tower_debug_delete_trace(
    params: Value,
    rr_runtime: &mut RrRuntime,
) -> Result<Value, DebugToolError> {
    let params: DeleteTraceParams = parse_params(params)?;
    match rr_runtime.store.delete_trace(&params.trace_id) {
        Ok(()) => serialize(DeleteTraceResult {
            deleted: true,
            trace_id: params.trace_id,
            error: None,
        }),
        Err(error) => serialize(DeleteTraceResult {
            deleted: false,
            trace_id: params.trace_id,
            error: Some(trace_store_failure(error)),
        }),
    }
}

pub fn tower_debug_find_origin(
    params: Value,
    sessions: &SessionManager,
    rr_runtime: &RrRuntime,
) -> Result<Value, DebugToolError> {
    let params: FindOriginRequest = parse_params(params)?;
    serialize(crate::origin::find_origin_with_trace_store(
        params,
        sessions,
        &rr_runtime.store,
    ))
}

pub fn tower_debug_record_and_find_origin(
    params: Value,
    sessions: &SessionManager,
    rr_runtime: &mut RrRuntime,
) -> Result<Value, DebugToolError> {
    let params: RecordAndFindOriginRequest = parse_params(params)?;
    serialize(crate::origin::record_and_find_origin(
        params, sessions, rr_runtime,
    ))
}

pub fn tower_debug_terminate(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: TerminateParams = parse_params(params)?;
    tool_result(
        sessions
            .terminate(&params.session_id)
            .map(|()| CleanupResult { ok: true }),
    )
}

pub fn tower_debug_disconnect(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let params: DisconnectParams = parse_params(params)?;
    tool_result(
        sessions
            .disconnect(&params.session_id)
            .map(|()| CleanupResult { ok: true }),
    )
}

pub fn tower_debug_sessions(
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, DebugToolError> {
    let _params: SessionsParams = parse_params(params)?;
    serialize(SessionsResult {
        sessions: sessions.sessions(),
    })
}

fn parse_params<T>(params: Value) -> Result<T, DebugToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(params).map_err(|_| invalid_params())
}

fn invalid_params() -> DebugToolError {
    DebugToolError {
        code: DebugToolErrorCode::InvalidParams,
        message: "InvalidParams".to_owned(),
    }
}

fn timeout(timeout_secs: Option<u64>) -> Option<Duration> {
    timeout_secs.map(Duration::from_secs)
}

fn tool_result<T>(result: Result<T, DebugRuntimeError>) -> Result<Value, DebugToolError>
where
    T: Serialize,
{
    match result {
        Ok(result) => serialize(result),
        Err(error) => serialize(runtime_failure(error)),
    }
}

fn serialize<T>(value: T) -> Result<Value, DebugToolError>
where
    T: Serialize,
{
    serde_json::to_value(value).map_err(|error| DebugToolError {
        code: DebugToolErrorCode::InvalidParams,
        message: format!("serialize debug tool result: {error}"),
    })
}

fn runtime_failure(error: DebugRuntimeError) -> RuntimeFailureResult {
    let serialized = serde_json::to_value(error).expect("serialize DebugRuntimeError");
    let code = serialized
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("debug-runtime-error")
        .to_owned();
    let message = serialized
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("debug runtime error")
        .to_owned();
    let data = serialized.get("data").cloned();

    RuntimeFailureResult {
        ok: false,
        error: RuntimeFailure {
            code,
            message,
            data,
        },
    }
}

fn tool_error_failure(error: DebugToolError) -> RuntimeFailureResult {
    let code = debug_tool_error_code(&error.code).to_owned();
    RuntimeFailureResult {
        ok: false,
        error: RuntimeFailure {
            code,
            message: error.message,
            data: None,
        },
    }
}

fn trace_store_failure(error: TraceStoreError) -> RuntimeFailure {
    let code = match &error {
        TraceStoreError::TraceNotFound { .. } => "trace_not_found",
        TraceStoreError::InvalidTraceId { .. } => "invalid_trace_id",
        TraceStoreError::InvalidTraceRoot { .. } => "invalid_trace_root",
        TraceStoreError::TracePathEscaped { .. } => "trace_path_escaped",
        TraceStoreError::DeleteFailed { .. } => "trace_delete_failed",
        TraceStoreError::MetadataWriteFailed { .. } => "trace_metadata_write_failed",
        TraceStoreError::MetadataReadFailed { .. } => "trace_metadata_read_failed",
    };
    RuntimeFailure {
        code: code.to_owned(),
        message: error.to_string(),
        data: None,
    }
}

fn debug_tool_error_code(code: &DebugToolErrorCode) -> &'static str {
    match code {
        DebugToolErrorCode::DebugNotInitialized => "debug-not-initialized",
        DebugToolErrorCode::DebugNotImplemented => concat!("debug-", "not-", "implemented"),
        DebugToolErrorCode::SessionNotFound => "session-not-found",
        DebugToolErrorCode::NotStopped => "not-stopped",
        DebugToolErrorCode::DebugTimeout => "debug-timeout",
        DebugToolErrorCode::InvalidParams => "invalid-params",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde_json::{Map, Value, json};

    use crate::protocol::{DebugAdapterConfig, DebugInitializeConfig, DebugToolError};
    use crate::rr::{FakeRrPreflight, FakeRrRecorder, RrPreflightStatus, RrRuntime};
    use crate::session::{
        DebugAdapterFactory, DebugAdapterSession, LaunchRequest, SessionManager,
        StepBackGranularity, WatchpointResult, WatchpointSpec,
    };
    use crate::traces::{TraceCompletion, TraceId, TracePolicy, TraceStore};
    use crate::types::{
        DebugBreakpoint, DebugOutput, DebugRuntimeError, DebugScope, DebugSessionId,
        DebugSessionState, DebugStackFrame, DebugStop, DebugThread, DebugVariable,
    };

    use super::{
        FindOriginRequest, FindOriginResult, RecordAndFindOriginRequest, RecordAndFindOriginResult,
        RecordParams, ReplayParams, ReverseContinueParams, StepBackParams, TracesParams,
        WatchpointParams, tower_debug_continue, tower_debug_disconnect, tower_debug_eval_at,
        tower_debug_evaluate, tower_debug_find_origin, tower_debug_launch, tower_debug_pause,
        tower_debug_record, tower_debug_record_and_find_origin, tower_debug_replay,
        tower_debug_reverse_continue, tower_debug_sessions, tower_debug_set_breakpoints,
        tower_debug_stack, tower_debug_step, tower_debug_step_back, tower_debug_terminate,
        tower_debug_threads, tower_debug_traces, tower_debug_variables, tower_debug_watchpoint,
    };

    #[derive(Clone, Copy, Debug, Default)]
    enum ResumeBehavior {
        #[default]
        Stopped,
        Terminated,
    }

    struct FakeAdapterFactory {
        resume_behavior: ResumeBehavior,
        supports_breakpoint_conditions: bool,
        fail_evaluate: bool,
        calls: Option<Arc<Mutex<Vec<String>>>>,
    }

    impl Default for FakeAdapterFactory {
        fn default() -> Self {
            Self {
                resume_behavior: ResumeBehavior::Stopped,
                supports_breakpoint_conditions: true,
                fail_evaluate: false,
                calls: None,
            }
        }
    }

    impl FakeAdapterFactory {
        fn terminated_on_resume() -> Self {
            Self {
                resume_behavior: ResumeBehavior::Terminated,
                supports_breakpoint_conditions: true,
                fail_evaluate: false,
                calls: None,
            }
        }

        fn without_breakpoint_condition_support() -> Self {
            Self {
                supports_breakpoint_conditions: false,
                ..Self::default()
            }
        }

        fn with_evaluate_failure() -> Self {
            Self {
                fail_evaluate: true,
                ..Self::default()
            }
        }

        fn recording(calls: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                calls: Some(calls),
                ..Self::default()
            }
        }
    }

    impl DebugAdapterFactory for FakeAdapterFactory {
        fn start(
            &self,
            _request: &LaunchRequest,
        ) -> Result<Box<dyn DebugAdapterSession>, DebugRuntimeError> {
            Ok(Box::new(FakeAdapterSession {
                resume_behavior: self.resume_behavior,
                supports_breakpoint_conditions: self.supports_breakpoint_conditions,
                fail_evaluate: self.fail_evaluate,
                calls: self.calls.clone(),
            }))
        }
    }

    #[derive(Default)]
    struct FakeAdapterSession {
        resume_behavior: ResumeBehavior,
        supports_breakpoint_conditions: bool,
        fail_evaluate: bool,
        calls: Option<Arc<Mutex<Vec<String>>>>,
    }

    impl DebugAdapterSession for FakeAdapterSession {
        fn initialize(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.record("initialize");
            Ok(())
        }

        fn launch(
            &mut self,
            _request: &LaunchRequest,
            _timeout: Duration,
        ) -> Result<(), DebugRuntimeError> {
            self.record("launch");
            Ok(())
        }

        fn set_breakpoints(
            &mut self,
            breakpoints: &[DebugBreakpoint],
            _timeout: Duration,
        ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError> {
            let conditions = breakpoints
                .iter()
                .filter_map(|breakpoint| breakpoint.condition.as_deref())
                .collect::<Vec<_>>()
                .join(",");
            self.record(format!("set_breakpoints:{conditions}"));
            Ok(breakpoints
                .iter()
                .enumerate()
                .map(|(index, breakpoint)| DebugBreakpoint {
                    verified: breakpoint.condition.is_none() || self.supports_breakpoint_conditions,
                    verified_id: Some((index + 1) as u64),
                    ..breakpoint.clone()
                })
                .collect())
        }

        fn continue_session(&mut self, timeout: Duration) -> Result<DebugStop, DebugRuntimeError> {
            self.record("continue");
            if timeout <= Duration::from_millis(1) {
                return Err(DebugRuntimeError::DebugTimeout(
                    "fake adapter timed out".to_owned(),
                ));
            }
            Ok(resume_stop(self.resume_behavior, "breakpoint"))
        }

        fn step(
            &mut self,
            _thread_id: Option<u64>,
            timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record("step");
            if timeout <= Duration::from_millis(1) {
                return Err(DebugRuntimeError::DebugTimeout(
                    "fake adapter timed out".to_owned(),
                ));
            }
            Ok(resume_stop(self.resume_behavior, "step"))
        }

        fn pause(
            &mut self,
            thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record("pause");
            Ok(DebugStop {
                reason: Some("pause".to_owned()),
                thread_id,
                ..stopped_at_breakpoint()
            })
        }

        fn threads(&mut self, _timeout: Duration) -> Result<Vec<DebugThread>, DebugRuntimeError> {
            self.record("threads");
            Ok(vec![DebugThread {
                id: 1,
                name: "main".to_owned(),
            }])
        }

        fn stack(
            &mut self,
            _thread_id: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
            self.record("stack");
            Ok(vec![top_frame()])
        }

        fn scopes(
            &mut self,
            _frame_id: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
            self.record("scopes");
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
            self.record("variables");
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
            self.record(format!("evaluate:{expression}"));
            if self.fail_evaluate {
                return Err(DebugRuntimeError::LaunchFailed(format!(
                    "fake evaluate failed for {expression}"
                )));
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
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record("reverse_continue");
            Ok(DebugStop {
                reason: Some("reverse_continue".to_owned()),
                thread_id,
                ..stopped_at_breakpoint()
            })
        }

        fn step_back(
            &mut self,
            thread_id: Option<u64>,
            granularity: StepBackGranularity,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.record(format!("step_back:{granularity:?}"));
            Ok(DebugStop {
                reason: Some("step_back".to_owned()),
                thread_id,
                ..stopped_at_breakpoint()
            })
        }

        fn set_watchpoint(
            &mut self,
            watchpoint: WatchpointSpec,
            _timeout: Duration,
        ) -> Result<WatchpointResult, DebugRuntimeError> {
            self.record("watchpoint");
            Ok(WatchpointResult {
                watchpoint_id: "watchpoint-7".to_owned(),
                expression: watchpoint.expression,
                address: watchpoint.address,
                kind: watchpoint.kind,
                enabled: watchpoint.enabled,
                verified: true,
            })
        }

        fn terminate(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.record("terminate");
            Ok(())
        }

        fn disconnect(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.record("disconnect");
            Ok(())
        }
    }

    impl FakeAdapterSession {
        fn record(&self, call: impl Into<String>) {
            if let Some(calls) = &self.calls {
                calls
                    .lock()
                    .expect("call log lock should not be poisoned")
                    .push(call.into());
            }
        }
    }

    #[test]
    fn tower_debug_launch_maps_params_to_session_manager_launch_and_returns_session_id_plus_state()
    {
        let sessions = manager();

        let result = tower_debug_launch(launch_params(), &sessions).expect("launch should succeed");

        assert!(
            result["session_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(result["state"], "stopped");
        assert_eq!(result["stop"]["state"], "stopped");
    }

    #[test]
    fn tower_debug_set_breakpoints_maps_source_breakpoints_and_returns_verified_breakpoint_ids() {
        let sessions = manager();
        let session_id = launch_session(&sessions);

        let result = tower_debug_set_breakpoints(
            json!({
                "session_id": session_id.0,
                "path": "src/main.rs",
                "breakpoints": [
                    { "line": 12, "condition": "answer == 42", "hit_condition": null },
                    { "line": 24, "condition": null, "hit_condition": "2" }
                ]
            }),
            &sessions,
        )
        .expect("set_breakpoints should succeed");

        assert_eq!(
            result,
            json!({
                "breakpoints": [
                    {
                        "path": "src/main.rs",
                        "line": 12,
                        "condition": "answer == 42",
                        "hit_condition": null,
                        "verified": true,
                        "verified_id": 1
                    },
                    {
                        "path": "src/main.rs",
                        "line": 24,
                        "condition": null,
                        "hit_condition": "2",
                        "verified": true,
                        "verified_id": 2
                    }
                ]
            })
        );
    }

    #[test]
    fn tower_debug_continue_and_tower_debug_step_return_stopped_terminated_or_timed_out_true_running_results_with_output_since()
     {
        let sessions = manager();
        let session_id = launch_session(&sessions);

        let continued = tower_debug_continue(
            json!({ "session_id": session_id.0, "thread_id": 1, "timeout_secs": 1 }),
            &sessions,
        )
        .expect("continue should succeed");
        let stepped = tower_debug_step(
            json!({ "session_id": session_id.0, "thread_id": 1, "timeout_secs": 1 }),
            &sessions,
        )
        .expect("step should succeed");
        assert_invalid_params(
            tower_debug_step(
                json!({ "session_id": session_id.0, "thread_id": 1, "granularity": "line", "timeout_secs": 1 }),
                &sessions,
            ),
            "tower_debug_step",
        );
        let timed_out = tower_debug_continue(
            json!({ "session_id": session_id.0, "thread_id": 1, "timeout_secs": 0 }),
            &sessions,
        )
        .expect("resume timeout should be returned as a running stop result");
        let terminated_sessions = manager_with_factory(FakeAdapterFactory::terminated_on_resume());
        let terminated_continue_id = launch_session(&terminated_sessions);
        let terminated_step_id = launch_session(&terminated_sessions);
        let terminated_continue = tower_debug_continue(
            json!({ "session_id": terminated_continue_id.0, "thread_id": 1, "timeout_secs": 1 }),
            &terminated_sessions,
        )
        .expect("continue may complete with a terminated result");
        let terminated_step = tower_debug_step(
            json!({ "session_id": terminated_step_id.0, "thread_id": 1, "timeout_secs": 1 }),
            &terminated_sessions,
        )
        .expect("step may complete with a terminated result");

        assert_eq!(continued["state"], "stopped");
        assert_eq!(continued["timed_out"], false);
        assert_eq!(continued["output_since"], json!(fake_output()));
        assert_eq!(stepped["state"], "stopped");
        assert_eq!(stepped["output_since"], json!(fake_output()));
        assert_eq!(terminated_continue["state"], "terminated");
        assert_eq!(terminated_continue["timed_out"], false);
        assert_eq!(terminated_continue["output_since"], json!(fake_output()));
        assert_eq!(terminated_step["state"], "terminated");
        assert_eq!(terminated_step["timed_out"], false);
        assert_eq!(terminated_step["output_since"], json!(fake_output()));
        assert_eq!(timed_out["state"], "running");
        assert_eq!(timed_out["timed_out"], true);
    }

    #[test]
    fn tower_debug_pause_regains_control_of_a_running_session_or_returns_a_stable_session_error() {
        let sessions = manager();
        let session_id = launch_session(&sessions);
        tower_debug_continue(
            json!({ "session_id": session_id.0, "thread_id": 1, "timeout_secs": 0 }),
            &sessions,
        )
        .expect("timeout leaves session running");

        let paused = tower_debug_pause(
            json!({ "session_id": session_id.0, "thread_id": 1 }),
            &sessions,
        )
        .expect("pause should regain control");
        let missing = tower_debug_pause(
            json!({ "session_id": "missing-session", "thread_id": 1 }),
            &sessions,
        )
        .expect("missing session is a structured tool result");

        assert_eq!(paused["state"], "stopped");
        assert_eq!(paused["reason"], "pause");
        assert_eq!(missing["ok"], false);
        assert_eq!(missing["error"]["code"], "session-not-found");
    }

    #[test]
    fn tower_debug_threads_stack_variables_and_evaluate_expose_stopped_runtime_inspection_dtos() {
        let sessions = manager();
        let session_id = launch_session(&sessions);

        let threads = tower_debug_threads(json!({ "session_id": session_id.0 }), &sessions)
            .expect("threads should succeed");
        let stack = tower_debug_stack(
            json!({ "session_id": session_id.0, "thread_id": 1 }),
            &sessions,
        )
        .expect("stack should succeed");
        let variables = tower_debug_variables(
            json!({ "session_id": session_id.0, "variables_reference": 100 }),
            &sessions,
        )
        .expect("variables should succeed");
        let evaluated = tower_debug_evaluate(
            json!({ "session_id": session_id.0, "frame_id": 10, "expression": "answer" }),
            &sessions,
        )
        .expect("evaluate should succeed");

        assert_eq!(threads, json!({ "threads": [{ "id": 1, "name": "main" }] }));
        assert_eq!(stack, json!({ "frames": [top_frame()] }));
        assert_eq!(variables, json!({ "variables": [answer_variable()] }));
        assert_eq!(evaluated, json!({ "result": answer_variable() }));
    }

    #[test]
    fn tower_debug_eval_at_maps_params_and_never_exposes_session_id() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sessions = manager_with_factory(FakeAdapterFactory::recording(Arc::clone(&calls)));

        let result =
            tower_debug_eval_at(eval_at_params(), &sessions).expect("eval_at should succeed");

        assert_eq!(
            calls
                .lock()
                .expect("call log lock should not be poisoned")
                .as_slice(),
            [
                "initialize",
                "launch",
                "set_breakpoints:answer == 42",
                "set_breakpoints:",
                "stack",
                "continue",
                "stack",
                "scopes",
                "variables",
                "evaluate:answer",
                "terminate",
            ],
            "tower_debug_eval_at must delegate orchestration to run_eval_at"
        );
        assert_eq!(result["hit"], true);
        assert_eq!(result["finished"], "stopped");
        assert_eq!(result["hits"][0]["frame"], json!(top_frame()));
        assert_eq!(result["hits"][0]["evaluated"]["answer"]["value"], "42");
        assert_object_has_no_key_recursively(&result, "session_id");
    }

    #[test]
    fn malformed_eval_at_params_return_invalid_params() {
        let sessions = manager();

        assert_invalid_params(
            tower_debug_eval_at(json!({ "program": "target/debug/app" }), &sessions),
            "tower_debug_eval_at",
        );
    }

    #[test]
    fn eval_at_params_reject_unknown_top_level_and_nested_fields() {
        let sessions = manager();

        assert_invalid_params(
            tower_debug_eval_at(
                json!({
                    "lang": "rust",
                    "program": "target/debug/app",
                    "max_hit": 99
                }),
                &sessions,
            ),
            "tower_debug_eval_at",
        );
        assert_invalid_params(
            tower_debug_eval_at(
                json!({
                    "lang": "rust",
                    "program": "target/debug/app",
                    "breakpoint": {
                        "path": "src/main.rs",
                        "line": 12,
                        "hit_condition": "3"
                    }
                }),
                &sessions,
            ),
            "tower_debug_eval_at",
        );
        assert_invalid_params(
            tower_debug_eval_at(
                json!({
                    "lang": "rust",
                    "program": "target/debug/app",
                    "capture": {
                        "stack": true,
                        "locals": true,
                        "args": true,
                        "globals": true
                    }
                }),
                &sessions,
            ),
            "tower_debug_eval_at",
        );
    }

    #[test]
    fn tower_debug_eval_at_runtime_probe_outcomes_serialize_as_successful_json_payloads() {
        let timeout_sessions = manager();
        let unsupported_condition_sessions =
            manager_with_factory(FakeAdapterFactory::without_breakpoint_condition_support());
        let expression_error_sessions =
            manager_with_factory(FakeAdapterFactory::with_evaluate_failure());

        let timeout = tower_debug_eval_at(
            json!({
                "lang": "rust",
                "program": "target/debug/app",
                "timeout_ms": 0
            }),
            &timeout_sessions,
        )
        .expect("timeout is probe evidence, not a transport error");
        let condition_unsupported =
            tower_debug_eval_at(eval_at_params(), &unsupported_condition_sessions)
                .expect("unsupported conditions are probe evidence");
        let expression_error = tower_debug_eval_at(
            json!({
                "lang": "rust",
                "program": "target/debug/app",
                "breakpoint": { "path": "src/main.rs", "line": 12 },
                "expressions": ["answer"]
            }),
            &expression_error_sessions,
        )
        .expect("per-expression failures are probe evidence");

        assert_eq!(timeout["finished"], "timeout");
        assert_eq!(condition_unsupported["condition_unsupported"], json!(true));
        assert_eq!(
            expression_error["hits"][0]["evaluated"]["answer"]["error"],
            "fake evaluate failed for answer"
        );
    }

    #[test]
    fn tower_debug_terminate_and_tower_debug_disconnect_clean_up_and_remove_sessions() {
        let sessions = manager();
        let terminated = launch_session(&sessions);
        let disconnected = launch_session(&sessions);

        let terminate = tower_debug_terminate(json!({ "session_id": terminated.0 }), &sessions)
            .expect("terminate should succeed");
        let disconnect = tower_debug_disconnect(json!({ "session_id": disconnected.0 }), &sessions)
            .expect("disconnect should succeed");
        let remaining =
            tower_debug_sessions(json!({}), &sessions).expect("sessions should succeed");

        assert_eq!(terminate, json!({ "ok": true }));
        assert_eq!(disconnect, json!({ "ok": true }));
        assert_eq!(remaining, json!({ "sessions": [] }));
    }

    #[test]
    fn tower_debug_sessions_returns_current_ephemeral_session_states() {
        let sessions = manager();
        let session_id = launch_session(&sessions);

        let result = tower_debug_sessions(json!({}), &sessions).expect("sessions should succeed");

        assert_eq!(result["sessions"][0]["session_id"], session_id.0);
        assert_eq!(result["sessions"][0]["language"], "rust");
        assert_eq!(result["sessions"][0]["state"], "stopped");
        assert_eq!(result["sessions"][0]["last_stop"]["state"], "stopped");
    }

    #[test]
    fn unknown_session_ids_return_session_not_found_and_running_inspection_returns_not_stopped() {
        let sessions = manager();
        let session_id = launch_session(&sessions);
        tower_debug_continue(
            json!({ "session_id": session_id.0, "thread_id": 1, "timeout_secs": 0 }),
            &sessions,
        )
        .expect("timeout leaves session running");

        let unknown = tower_debug_threads(json!({ "session_id": "missing-session" }), &sessions)
            .expect("missing sessions are structured tool results");
        let running = tower_debug_stack(
            json!({ "session_id": session_id.0, "thread_id": 1 }),
            &sessions,
        )
        .expect("running inspection is a structured tool result");

        assert_eq!(unknown["ok"], false);
        assert_eq!(unknown["error"]["code"], "session-not-found");
        assert_eq!(running["ok"], false);
        assert_eq!(running["error"]["code"], "not-stopped");
    }

    #[test]
    fn unit_tests_in_tools_cover_valid_params_malformed_params_and_structured_runtime_failures_for_every_public_tool_handler()
     {
        let sessions = manager();
        type ToolHandler = fn(Value, &SessionManager) -> Result<Value, DebugToolError>;

        let handlers: [(&str, ToolHandler); 13] = [
            ("tower_debug_launch", tower_debug_launch),
            ("tower_debug_set_breakpoints", tower_debug_set_breakpoints),
            ("tower_debug_continue", tower_debug_continue),
            ("tower_debug_step", tower_debug_step),
            ("tower_debug_pause", tower_debug_pause),
            ("tower_debug_threads", tower_debug_threads),
            ("tower_debug_stack", tower_debug_stack),
            ("tower_debug_variables", tower_debug_variables),
            ("tower_debug_evaluate", tower_debug_evaluate),
            ("tower_debug_eval_at", tower_debug_eval_at),
            ("tower_debug_terminate", tower_debug_terminate),
            ("tower_debug_disconnect", tower_debug_disconnect),
            ("tower_debug_sessions", tower_debug_sessions),
        ];

        for (name, handler) in handlers {
            assert_invalid_params(handler(json!("malformed"), &sessions), name);
        }

        let runtime_failure_inputs = [
            tower_debug_launch(
                json!({
                    "language": "python",
                    "program": "target/debug/app",
                    "cwd": "/workspace",
                    "args": [],
                    "env": {},
                    "launch_overrides": {}
                }),
                &sessions,
            ),
            tower_debug_set_breakpoints(
                json!({ "session_id": "missing-session", "path": "src/main.rs", "breakpoints": [] }),
                &sessions,
            ),
            tower_debug_continue(
                json!({ "session_id": "missing-session", "thread_id": 1, "timeout_secs": 1 }),
                &sessions,
            ),
            tower_debug_step(
                json!({ "session_id": "missing-session", "thread_id": 1, "timeout_secs": 1 }),
                &sessions,
            ),
            tower_debug_pause(
                json!({ "session_id": "missing-session", "thread_id": 1 }),
                &sessions,
            ),
            tower_debug_threads(json!({ "session_id": "missing-session" }), &sessions),
            tower_debug_stack(
                json!({ "session_id": "missing-session", "thread_id": 1 }),
                &sessions,
            ),
            tower_debug_variables(
                json!({ "session_id": "missing-session", "variables_reference": 100 }),
                &sessions,
            ),
            tower_debug_evaluate(
                json!({ "session_id": "missing-session", "frame_id": 10, "expression": "answer" }),
                &sessions,
            ),
            tower_debug_terminate(json!({ "session_id": "missing-session" }), &sessions),
            tower_debug_disconnect(json!({ "session_id": "missing-session" }), &sessions),
        ];

        let expected_codes = [
            "launch-failed",
            "session-not-found",
            "session-not-found",
            "session-not-found",
            "session-not-found",
            "session-not-found",
            "session-not-found",
            "session-not-found",
            "session-not-found",
            "session-not-found",
            "session-not-found",
        ];

        for (result, expected_code) in runtime_failure_inputs.into_iter().zip(expected_codes) {
            let value = result.expect("runtime failures are structured tool results");
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"]["code"], expected_code);
        }

        let eval_at_launch_error = tower_debug_eval_at(
            json!({
                "lang": "python",
                "program": "target/debug/app"
            }),
            &sessions,
        )
        .expect("eval-at launch failure should use the runtime failure envelope");
        assert_eq!(eval_at_launch_error["ok"], false);
        assert_eq!(eval_at_launch_error["error"]["code"], "invalid-params");
        assert_eq!(
            eval_at_launch_error["error"]["message"],
            "no debug adapter configured for language python"
        );

        let sessions_result =
            tower_debug_sessions(json!({}), &sessions).expect("sessions should succeed");
        assert_eq!(sessions_result, json!({ "sessions": [] }));
    }

    #[test]
    fn tower_debug_record_is_declared_only_with_rr_accepts_record_params_and_returns_rr_record_result()
     {
        let params: RecordParams = serde_json::from_value(json!({
            "language": "rust",
            "program": "target/debug/app",
            "args": ["--flag"],
            "cwd": "/workspace",
            "env": { "RUST_LOG": "debug" },
            "timeout_ms": 1000
        }))
        .expect("RecordParams accepts the public record request fields");
        assert_eq!(params.language, "rust");
        assert_eq!(params.program, "target/debug/app");
        assert_eq!(params.args, vec!["--flag"]);
        assert_eq!(params.cwd.as_deref(), Some("/workspace"));
        assert_eq!(params.env["RUST_LOG"], "debug");
        assert_eq!(params.timeout_ms, Some(1000));
        assert!(
            serde_json::from_value::<RecordParams>(json!({
                "language": "rust",
                "program": "target/debug/app",
                "trace_policy": {}
            }))
            .is_err()
        );

        let mut runtime = rr_runtime("record");
        let result = tower_debug_record(
            json!({
                "language": "rust",
                "program": "target/debug/app",
                "args": [],
                "cwd": null,
                "env": {},
                "timeout_ms": 1000
            }),
            &mut runtime,
        )
        .expect("record runtime outcomes are successful tool payloads");

        assert_eq!(result["recordable"], true);
        assert_eq!(result["trace_id"], "trace-record");
        assert_eq!(result["error"], Value::Null);
    }

    #[test]
    fn tower_debug_replay_accepts_replay_params_opens_replay_session_and_returns_supports_step_back_field()
     {
        let sessions = manager_with_config(config_with_rr());
        let mut runtime = rr_runtime("replay");
        let trace = register_trace(&mut runtime.store, "target/debug/app", 10);

        let result = tower_debug_replay(
            json!({
                "trace_id": trace.trace_id,
                "language": "rust",
                "timeout_secs": 1
            }),
            &sessions,
            &runtime,
        )
        .expect("replay should open a replay session");

        assert_eq!(result["trace_id"], trace.trace_id.to_string());
        assert_eq!(result["state"], "stopped");
        assert_eq!(result["stop"]["reason"], "replay");
        assert_eq!(result["supportsStepBack"], true);
        assert!(result.get("supports_step_back").is_none());
        assert!(
            serde_json::from_value::<ReplayParams>(json!({
                "trace_id": "trace-replay",
                "language": "rust",
                "timeout_secs": 1,
                "typo": true
            }))
            .is_err()
        );
    }

    #[test]
    fn tower_debug_reverse_continue_and_step_back_accept_strict_params_and_return_debug_stop_results()
     {
        let sessions = manager_with_config(config_with_rr());
        let mut runtime = rr_runtime("reverse");
        let trace = register_trace(&mut runtime.store, "target/debug/app", 10);
        let replay = tower_debug_replay(
            json!({
                "trace_id": trace.trace_id,
                "language": "rust",
                "timeout_secs": 1
            }),
            &sessions,
            &runtime,
        )
        .expect("replay should open");
        let session_id = replay["session_id"].as_str().expect("session id");

        let continued = tower_debug_reverse_continue(
            json!({ "session_id": session_id, "thread_id": 3, "timeout_secs": 1 }),
            &sessions,
        )
        .expect("reverse continue returns DebugStop");
        let stepped = tower_debug_step_back(
            json!({
                "session_id": session_id,
                "thread_id": 3,
                "granularity": "instruction",
                "timeout_secs": 1
            }),
            &sessions,
        )
        .expect("step back returns DebugStop");

        assert_eq!(continued["reason"], "reverse_continue");
        assert_eq!(continued["thread_id"], 3);
        assert_eq!(stepped["reason"], "step_back");
        assert_invalid_params(
            tower_debug_reverse_continue(
                json!({ "session_id": session_id, "unknown": true }),
                &sessions,
            ),
            "tower_debug_reverse_continue",
        );
        assert_invalid_params(
            tower_debug_step_back(
                json!({ "session_id": session_id, "granularity": "statement" }),
                &sessions,
            ),
            "tower_debug_step_back",
        );
        assert!(
            serde_json::from_value::<ReverseContinueParams>(json!({
                "session_id": session_id,
                "thread_id": 3,
                "timeout_secs": 1,
                "typo": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<StepBackParams>(json!({
                "session_id": session_id,
                "granularity": "instruction",
                "typo": true
            }))
            .is_err()
        );
    }

    #[test]
    fn tower_debug_watchpoint_accepts_watchpoint_params_and_unsupported_adapters_return_reverse_unsupported_payload()
     {
        let live_sessions = manager();
        let live_session_id = launch_session(&live_sessions);

        let unsupported = tower_debug_watchpoint(
            json!({
                "session_id": live_session_id.0,
                "expression": "answer",
                "address": null,
                "kind": "write",
                "enabled": true,
                "timeout_secs": 1
            }),
            &live_sessions,
        )
        .expect("unsupported reverse capability is a successful payload");

        assert_eq!(unsupported["ok"], false);
        assert_eq!(unsupported["error"]["code"], "reverse_unsupported");
        assert_invalid_params(
            tower_debug_watchpoint(
                json!({
                    "session_id": live_session_id.0,
                    "expression": "answer",
                    "kind": "execute"
                }),
                &live_sessions,
            ),
            "tower_debug_watchpoint",
        );
        assert!(
            serde_json::from_value::<WatchpointParams>(json!({
                "session_id": "debug-1",
                "expression": "answer",
                "kind": "write",
                "typo": true
            }))
            .is_err()
        );
    }

    #[test]
    fn tower_debug_traces_accepts_empty_params_and_returns_traces_result_from_trace_store_list_traces()
     {
        let mut runtime = rr_runtime("traces");
        let trace = register_trace(&mut runtime.store, "target/debug/app", 10);

        let result =
            tower_debug_traces(json!({}), &runtime).expect("traces should list trace metadata");

        assert_eq!(result, json!({ "traces": [trace] }));
        assert!(serde_json::from_value::<TracesParams>(json!({ "typo": true })).is_err());
    }

    #[test]
    fn tower_debug_delete_trace_successful_delete_returns_deleted_true_trace_id_and_no_error() {
        let mut runtime = rr_runtime("delete-success");
        let trace = register_trace(&mut runtime.store, "target/debug/app", 10);

        let result =
            super::tower_debug_delete_trace(json!({ "trace_id": trace.trace_id }), &mut runtime)
                .expect("delete should return a structured payload");

        assert_eq!(
            result,
            json!({
                "deleted": true,
                "trace_id": trace.trace_id,
                "error": null
            })
        );
    }

    #[test]
    fn tower_debug_delete_trace_missing_deleted_or_expired_trace_returns_trace_not_found_payload() {
        let mut runtime = rr_runtime("delete-missing");
        let trace_id = TraceId::new("missing-trace").expect("valid trace id");

        let result = super::tower_debug_delete_trace(json!({ "trace_id": trace_id }), &mut runtime)
            .expect("missing trace should be a structured payload");

        assert_eq!(result["deleted"], false);
        assert_eq!(result["trace_id"], "missing-trace");
        assert_eq!(result["error"]["code"], "trace_not_found");
    }

    #[test]
    fn origin_tools_accept_strict_public_request_dtos_and_malformed_params_return_invalid_params() {
        let sessions = manager_with_config(config_with_rr());
        let mut runtime = rr_runtime("origin");
        let trace = register_trace(&mut runtime.store, "target/debug/app", 10);
        let find_origin = json!({
            "trace_id": trace.trace_id,
            "language": "rust",
            "watch": "answer",
            "at": { "kind": "crash" },
            "max_depth": 3,
            "max_children": 8,
            "timeout_secs": 1
        });
        let record_and_find_origin = json!({
            "record": {
                "language": "rust",
                "program": "target/debug/app",
                "args": [],
                "cwd": null,
                "env": {},
                "timeout_ms": 1000
            },
            "origin": {
                "language": "rust",
                "watch": "answer",
                "at": { "kind": "end" },
                "timeout_secs": 1,
                "max_depth": 3,
                "max_children": 8
            }
        });

        serde_json::from_value::<FindOriginRequest>(find_origin.clone())
            .expect("FindOriginRequest accepts public fields");
        serde_json::from_value::<RecordAndFindOriginRequest>(record_and_find_origin.clone())
            .expect("RecordAndFindOriginRequest accepts public fields");
        let find_origin_result = tower_debug_find_origin(find_origin, &sessions, &runtime)
            .expect("find_origin should return FindOriginResult");
        serde_json::from_value::<FindOriginResult>(find_origin_result)
            .expect("find_origin success payload must deserialize as FindOriginResult");
        let record_and_find_origin_result =
            tower_debug_record_and_find_origin(record_and_find_origin, &sessions, &mut runtime)
                .expect("record_and_find_origin should return RecordAndFindOriginResult");
        serde_json::from_value::<RecordAndFindOriginResult>(record_and_find_origin_result).expect(
            "record_and_find_origin success payload must deserialize as RecordAndFindOriginResult",
        );
        assert_invalid_params(
            tower_debug_find_origin(
                json!({ "trace_id": "trace-origin", "typo": true }),
                &sessions,
                &runtime,
            ),
            "tower_debug_find_origin",
        );
        assert_invalid_params(
            tower_debug_record_and_find_origin(
                json!({ "record": {}, "origin": { "watch": "answer", "at": "middle" } }),
                &sessions,
                &mut runtime,
            ),
            "tower_debug_record_and_find_origin",
        );
    }

    fn manager() -> SessionManager {
        manager_with_factory(FakeAdapterFactory::default())
    }

    fn manager_with_factory(factory: FakeAdapterFactory) -> SessionManager {
        SessionManager::new(config(), Arc::new(factory))
    }

    fn manager_with_config(config: DebugInitializeConfig) -> SessionManager {
        SessionManager::new(config, Arc::new(FakeAdapterFactory::default()))
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
                    idle_ttl_secs: 60,
                },
            )]),
            record: None,
        }
    }

    fn config_with_rr() -> DebugInitializeConfig {
        let mut config = config();
        config.record = Some(crate::protocol::DebugRecordConfig {
            backend: "rr".to_owned(),
            trace_dir: Some(".tower/traces".to_owned()),
            ttl_secs: Some(86_400),
            max_traces: Some(25),
            record_timeout_secs: Some(30),
        });
        config
    }

    fn rr_runtime(name: &str) -> RrRuntime {
        let store = TraceStore::new(trace_policy(name));
        let trace_id = TraceId::new("trace-record").expect("valid trace id");
        let trace = crate::traces::TraceMetadata {
            trace_id: trace_id.clone(),
            path: store
                .policy()
                .trace_root
                .join("trace-record")
                .display()
                .to_string(),
            created_unix_secs: 1,
            program: "target/debug/app".to_owned(),
            args_summary: Vec::new(),
            exit_code: Some(0),
            output_summary: Vec::new(),
            output_truncated: false,
            expires_unix_secs: None,
            ttl_secs: None,
            prune_generation: 0,
        };
        let result = crate::rr::RrRecordResult {
            recordable: true,
            reason: None,
            trace_id: Some(trace_id),
            trace: Some(trace),
            exit_code: Some(0),
            output: Vec::new(),
            output_truncated: false,
            error: None,
        };
        RrRuntime::with_parts(
            Box::new(FakeRrPreflight::new(RrPreflightStatus::Supported)),
            Box::new(FakeRrRecorder::new(result)),
            store,
        )
    }

    fn trace_policy(name: &str) -> TracePolicy {
        TracePolicy {
            trace_root: temp_trace_root(name),
            ttl_secs: None,
            max_traces: 20,
            record_timeout_secs: 60,
        }
    }

    fn temp_trace_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tower-debug-tools-{name}-{unique}"));
        fs::create_dir_all(&root).expect("create temp trace root");
        root
    }

    fn register_trace(
        store: &mut TraceStore,
        program: &str,
        created_unix_secs: u64,
    ) -> crate::traces::TraceMetadata {
        let allocation = store
            .allocate_trace(program, created_unix_secs)
            .expect("trace allocation");
        store
            .register_completed(
                allocation,
                TraceCompletion {
                    program: program.to_owned(),
                    args_summary: vec!["--flag".to_owned()],
                    exit_code: Some(0),
                    output_summary: vec!["ok".to_owned()],
                    output_truncated: false,
                },
                created_unix_secs,
            )
            .expect("trace registration")
    }

    fn launch_params() -> serde_json::Value {
        json!({
            "language": "rust",
            "program": "target/debug/app",
            "cwd": "/workspace",
            "args": ["--flag"],
            "env": { "RUST_LOG": "debug" },
            "launch_overrides": {}
        })
    }

    fn eval_at_params() -> serde_json::Value {
        json!({
            "lang": "rust",
            "program": "target/debug/app",
            "args": ["--flag"],
            "cwd": "/workspace",
            "env": { "RUST_LOG": "debug" },
            "breakpoint": {
                "path": "src/main.rs",
                "line": 12,
                "condition": "answer == 42"
            },
            "expressions": ["answer"],
            "capture": {
                "stack": true,
                "locals": true,
                "args": true
            },
            "on_hit": "first",
            "max_hits": 1,
            "max_depth": 1,
            "max_children": 4,
            "timeout_ms": 1000
        })
    }

    fn launch_session(sessions: &SessionManager) -> DebugSessionId {
        sessions
            .launch(LaunchRequest {
                language: "rust".to_owned(),
                program: "target/debug/app".to_owned(),
                cwd: Some("/workspace".to_owned()),
                args: vec!["--flag".to_owned()],
                env: BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())]),
                launch_overrides: Map::new(),
            })
            .expect("test session should launch")
            .session_id
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

    fn resume_stop(behavior: ResumeBehavior, reason: &str) -> DebugStop {
        match behavior {
            ResumeBehavior::Stopped => DebugStop {
                reason: Some(reason.to_owned()),
                output_since: fake_output(),
                ..stopped_at_breakpoint()
            },
            ResumeBehavior::Terminated => DebugStop {
                state: DebugSessionState::Terminated,
                reason: Some("terminated".to_owned()),
                thread_id: Some(1),
                top_frame: None,
                hit_breakpoint_ids: Vec::new(),
                timed_out: false,
                exit_code: None,
                output_since: fake_output(),
            },
        }
    }

    fn assert_invalid_params(result: Result<Value, DebugToolError>, handler_name: &str) {
        let error = result.expect_err("malformed params must return a protocol error");
        let error_json = serde_json::to_value(error).expect("serialize DebugToolError");

        assert_eq!(
            error_json["code"], -32602,
            "{handler_name} malformed params must use JSON-RPC -32602 InvalidParams"
        );
        assert_eq!(
            error_json["message"], "InvalidParams",
            "{handler_name} malformed params must use the JSON-RPC InvalidParams message"
        );
    }

    fn assert_object_has_no_key_recursively(value: &Value, forbidden: &str) {
        match value {
            Value::Object(map) => {
                assert!(
                    !map.contains_key(forbidden),
                    "debug eval-at response must not expose {forbidden}: {value}"
                );
                for child in map.values() {
                    assert_object_has_no_key_recursively(child, forbidden);
                }
            }
            Value::Array(items) => {
                for item in items {
                    assert_object_has_no_key_recursively(item, forbidden);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    fn fake_output() -> Vec<DebugOutput> {
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

    fn answer_variable() -> DebugVariable {
        DebugVariable {
            name: "answer".to_owned(),
            value: "42".to_owned(),
            r#type: Some("i32".to_owned()),
            variables_reference: 0,
        }
    }
}
