#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::eval_at::{CaptureError, CapturedVariable, capture_variables};
use crate::rr::{RrRecordRequest, RrRecordResult, RrRuntime};
use crate::session::{
    ReplayOpenRequest, ReplaySeekTarget, SessionManager, WatchpointKind, WatchpointSpec,
};
use crate::tools::RecordParams;
use crate::traces::{TraceId, TraceStore};
use crate::types::DebugOutput;
use crate::types::{
    DebugRuntimeError, DebugScope, DebugSessionId, DebugSessionState, DebugStackFrame,
    DebugVariable, RuntimeFailure,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OriginTarget {
    Crash,
    End,
    Source {
        path: String,
        line: u64,
        column: Option<u64>,
    },
}

impl From<OriginTarget> for ReplaySeekTarget {
    fn from(target: OriginTarget) -> Self {
        match target {
            OriginTarget::Crash => Self::Crash,
            OriginTarget::End => Self::End,
            OriginTarget::Source { path, line, column } => Self::Source { path, line, column },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginFailureCode {
    NoPriorWriteReached,
    WatchEvaluationFailed,
    TraceNotFound,
    ReplayOpenFailed,
    ReverseUnsupported,
    OriginTimeout,
    RecordFailed,
    CaptureFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindOriginRequest {
    pub trace_id: TraceId,
    pub language: String,
    pub at: OriginTarget,
    pub watch: String,
    pub timeout_secs: Option<u64>,
    pub max_depth: Option<usize>,
    pub max_children: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FindOriginResult {
    pub found: bool,
    pub reason: Option<OriginFailureCode>,
    pub trace_id: Option<TraceId>,
    pub write_frame: Option<DebugStackFrame>,
    pub stack: Vec<DebugStackFrame>,
    pub value: Option<CapturedVariable>,
    pub locals: Vec<CapturedVariable>,
    pub args: Vec<CapturedVariable>,
    pub output: Vec<DebugOutput>,
    pub truncated: bool,
    pub error: Option<RuntimeFailure>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordOriginParams {
    pub language: String,
    pub at: OriginTarget,
    pub watch: String,
    pub timeout_secs: Option<u64>,
    pub max_depth: Option<usize>,
    pub max_children: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordAndFindOriginRequest {
    pub record: RecordParams,
    pub origin: RecordOriginParams,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordAndFindOriginResult {
    pub record: RrRecordResult,
    pub origin: Option<FindOriginResult>,
}

pub fn find_origin(request: FindOriginRequest, sessions: &SessionManager) -> FindOriginResult {
    find_origin_with_trace_path(request, sessions, None)
}

pub fn find_origin_with_trace_store(
    request: FindOriginRequest,
    sessions: &SessionManager,
    trace_store: &TraceStore,
) -> FindOriginResult {
    let trace = match trace_store.trace(&request.trace_id) {
        Ok(trace) => trace,
        Err(error) => {
            return failure(
                request.trace_id,
                OriginFailureCode::TraceNotFound,
                error.to_string(),
            );
        }
    };
    find_origin_with_trace_path(request, sessions, Some(trace.path))
}

fn find_origin_with_trace_path(
    request: FindOriginRequest,
    sessions: &SessionManager,
    trace_path: Option<String>,
) -> FindOriginResult {
    let timeout = request.timeout_secs.map(std::time::Duration::from_secs);
    let replay = match sessions.open_replay(ReplayOpenRequest {
        trace_id: request.trace_id.clone(),
        trace_path,
        language: request.language.clone(),
        timeout_secs: request.timeout_secs,
        adapter: None,
        adapter_args: Vec::new(),
    }) {
        Ok(replay) => replay,
        Err(error) => {
            return failure(
                request.trace_id,
                OriginFailureCode::ReplayOpenFailed,
                runtime_message(error),
            );
        }
    };
    let session_id = replay.session_id;
    let trace_id = replay.trace_id;

    let mut result = match run_find_origin(&request, sessions, &session_id, &trace_id, timeout) {
        Ok(result) => result,
        Err((code, message)) => failure(trace_id, code, message),
    };

    if let Err(cleanup_error) = sessions.terminate(&session_id) {
        attach_cleanup_error(&mut result, runtime_message(cleanup_error));
    }

    result
}

pub fn record_and_find_origin(
    request: RecordAndFindOriginRequest,
    sessions: &SessionManager,
    rr_runtime: &mut RrRuntime,
) -> RecordAndFindOriginResult {
    let record = rr_runtime.record(RrRecordRequest {
        language: request.record.language,
        program: request.record.program,
        args: request.record.args,
        cwd: request.record.cwd,
        env: request.record.env,
        timeout_ms: request.record.timeout_ms,
        trace_policy: rr_runtime.store.policy().clone(),
    });

    let Some(trace_id) = record.trace_id.clone() else {
        let origin = if record.reason.as_deref() == Some("rr_unsupported") {
            None
        } else {
            Some(failure(
                TraceId::new("record-failed").expect("static trace id is valid"),
                OriginFailureCode::RecordFailed,
                record
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "record failed".to_owned()),
            ))
        };
        return RecordAndFindOriginResult { record, origin };
    };
    let trace_path = record.trace.as_ref().map(|trace| trace.path.clone());

    let origin = find_origin_with_trace_path(
        FindOriginRequest {
            trace_id,
            language: request.origin.language,
            at: request.origin.at,
            watch: request.origin.watch,
            timeout_secs: request.origin.timeout_secs,
            max_depth: request.origin.max_depth,
            max_children: request.origin.max_children,
        },
        sessions,
        trace_path,
    );

    RecordAndFindOriginResult {
        record,
        origin: Some(origin),
    }
}

fn run_find_origin(
    request: &FindOriginRequest,
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    trace_id: &TraceId,
    timeout: Option<std::time::Duration>,
) -> Result<FindOriginResult, (OriginFailureCode, String)> {
    let seek = match sessions.seek_replay(
        session_id,
        ReplaySeekTarget::from(request.at.clone()),
        timeout,
    ) {
        Ok(seek) => seek,
        Err(DebugRuntimeError::AdapterExited(_) | DebugRuntimeError::SessionNotFound(_)) => {
            return Ok(adapter_exited(trace_id.clone(), Vec::new()));
        }
        Err(error) => {
            return Err(map_runtime_error(
                error,
                OriginFailureCode::ReverseUnsupported,
            ));
        }
    };
    if seek.timed_out {
        return Err((
            OriginFailureCode::OriginTimeout,
            "seek replay timed out".to_owned(),
        ));
    }

    sessions
        .set_watchpoint(
            session_id,
            WatchpointSpec {
                expression: Some(request.watch.clone()),
                address: None,
                kind: WatchpointKind::Write,
                enabled: true,
            },
        )
        .map_err(|error| map_runtime_error(error, OriginFailureCode::WatchEvaluationFailed))?;

    let stop = match sessions.reverse_continue(session_id, None, timeout) {
        Ok(stop) => stop,
        Err(DebugRuntimeError::AdapterExited(_) | DebugRuntimeError::SessionNotFound(_)) => {
            return Ok(adapter_exited(trace_id.clone(), Vec::new()));
        }
        Err(error) => {
            return Err(map_runtime_error(
                error,
                OriginFailureCode::ReverseUnsupported,
            ));
        }
    };
    if stop.timed_out {
        return Err((
            OriginFailureCode::OriginTimeout,
            "origin search timed out".to_owned(),
        ));
    }
    if stop.reason.as_deref() == Some("adapter_exited") {
        return Ok(adapter_exited(trace_id.clone(), stop.output_since));
    }
    if stop.state != DebugSessionState::Stopped {
        return Ok(no_prior_write(trace_id.clone(), stop.output_since));
    }

    capture_origin(request, sessions, session_id, trace_id, &stop)
}

fn capture_origin(
    request: &FindOriginRequest,
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    trace_id: &TraceId,
    stop: &crate::types::DebugStop,
) -> Result<FindOriginResult, (OriginFailureCode, String)> {
    let thread_id = stop.thread_id.ok_or_else(|| {
        (
            OriginFailureCode::NoPriorWriteReached,
            "reverse execution reached trace start".to_owned(),
        )
    })?;
    let stack = sessions
        .stack(session_id, thread_id)
        .map_err(|error| map_runtime_error(error, OriginFailureCode::CaptureFailed))?;
    let frame = stop
        .top_frame
        .clone()
        .or_else(|| stack.first().cloned())
        .ok_or_else(|| {
            (
                OriginFailureCode::NoPriorWriteReached,
                "reverse execution reached trace start".to_owned(),
            )
        })?;
    let max_depth = request.max_depth.unwrap_or(2);
    let max_children = request.max_children.unwrap_or(50);
    let locals = capture_local_variables(sessions, session_id, frame.id, max_depth, max_children)
        .map_err(map_capture_error)?;
    let evaluated = sessions
        .evaluate(session_id, frame.id, request.watch.clone())
        .map_err(|error| map_runtime_error(error, OriginFailureCode::WatchEvaluationFailed))?;
    let value = locals
        .iter()
        .find(|variable| variable.name == evaluated.name || variable.name == request.watch)
        .cloned()
        .or_else(|| Some(from_debug_variable(evaluated)));
    let truncated = locals.iter().any(is_truncated) || value.as_ref().is_some_and(is_truncated);

    Ok(FindOriginResult {
        found: true,
        reason: None,
        trace_id: Some(trace_id.clone()),
        write_frame: Some(frame),
        stack,
        value,
        locals,
        args: Vec::new(),
        output: stop.output_since.clone(),
        truncated,
        error: None,
    })
}

fn capture_local_variables(
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    frame_id: u64,
    max_depth: usize,
    max_children: usize,
) -> Result<Vec<CapturedVariable>, CaptureError> {
    let scopes = sessions
        .scopes(session_id, frame_id)
        .map_err(CaptureError::Runtime)?;
    let mut locals = Vec::new();
    for scope in scopes {
        if is_local_scope(&scope) {
            locals.extend(capture_variables(
                sessions,
                session_id,
                scope.variables_reference,
                max_depth,
                max_children,
            )?);
        }
    }
    Ok(locals)
}

fn is_local_scope(scope: &DebugScope) -> bool {
    scope.name.eq_ignore_ascii_case("locals") || scope.name.eq_ignore_ascii_case("local")
}

fn from_debug_variable(variable: DebugVariable) -> CapturedVariable {
    CapturedVariable {
        name: variable.name,
        value: variable.value,
        r#type: variable.r#type,
        children: Vec::new(),
        truncated: variable.variables_reference != 0,
    }
}

fn is_truncated(variable: &CapturedVariable) -> bool {
    variable.truncated || variable.children.iter().any(is_truncated)
}

fn no_prior_write(trace_id: TraceId, output: Vec<DebugOutput>) -> FindOriginResult {
    FindOriginResult {
        found: false,
        reason: Some(OriginFailureCode::NoPriorWriteReached),
        trace_id: Some(trace_id),
        write_frame: None,
        stack: Vec::new(),
        value: None,
        locals: Vec::new(),
        args: Vec::new(),
        output,
        truncated: false,
        error: None,
    }
}

fn adapter_exited(trace_id: TraceId, output: Vec<DebugOutput>) -> FindOriginResult {
    FindOriginResult {
        found: false,
        reason: None,
        trace_id: Some(trace_id),
        write_frame: None,
        stack: Vec::new(),
        value: None,
        locals: Vec::new(),
        args: Vec::new(),
        output,
        truncated: false,
        error: Some(RuntimeFailure {
            code: "adapter_exited".to_owned(),
            message: "debug adapter exited during origin search".to_owned(),
            data: None,
        }),
    }
}

fn failure(trace_id: TraceId, code: OriginFailureCode, message: String) -> FindOriginResult {
    let error = (!matches!(code, OriginFailureCode::NoPriorWriteReached)).then(|| RuntimeFailure {
        code: failure_code_string(&code),
        message,
        data: None,
    });
    FindOriginResult {
        found: false,
        reason: Some(code),
        trace_id: Some(trace_id),
        write_frame: None,
        stack: Vec::new(),
        value: None,
        locals: Vec::new(),
        args: Vec::new(),
        output: Vec::new(),
        truncated: false,
        error,
    }
}

fn map_capture_error(error: CaptureError) -> (OriginFailureCode, String) {
    match error {
        CaptureError::AdapterGone => (
            OriginFailureCode::CaptureFailed,
            "debug adapter exited during capture".to_owned(),
        ),
        CaptureError::Runtime(error) => (OriginFailureCode::CaptureFailed, runtime_message(error)),
    }
}

fn map_runtime_error(
    error: DebugRuntimeError,
    default_code: OriginFailureCode,
) -> (OriginFailureCode, String) {
    let code = match error {
        DebugRuntimeError::DebugTimeout(_) => OriginFailureCode::OriginTimeout,
        DebugRuntimeError::ReverseUnsupported(_) => OriginFailureCode::ReverseUnsupported,
        DebugRuntimeError::SessionNotFound(_)
        | DebugRuntimeError::NotStopped(_)
        | DebugRuntimeError::AdapterExited(_)
        | DebugRuntimeError::LaunchFailed(_) => default_code,
    };
    (code, runtime_message(error))
}

fn runtime_message(error: DebugRuntimeError) -> String {
    match error {
        DebugRuntimeError::SessionNotFound(message)
        | DebugRuntimeError::NotStopped(message)
        | DebugRuntimeError::DebugTimeout(message)
        | DebugRuntimeError::AdapterExited(message)
        | DebugRuntimeError::LaunchFailed(message)
        | DebugRuntimeError::ReverseUnsupported(message) => message,
    }
}

fn attach_cleanup_error(result: &mut FindOriginResult, cleanup_error: String) {
    if let Some(error) = &mut result.error {
        let mut data = error
            .data
            .take()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        data.insert("cleanup_error".to_owned(), Value::String(cleanup_error));
        error.data = Some(Value::Object(data));
    }
}

fn failure_code_string(code: &OriginFailureCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "origin_failure".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Map, json};

    use super::{
        FindOriginRequest, FindOriginResult, OriginFailureCode, OriginTarget,
        RecordAndFindOriginRequest, RecordOriginParams, find_origin, record_and_find_origin,
    };
    use crate::eval_at::CapturedVariable;
    use crate::protocol::{DebugAdapterConfig, DebugInitializeConfig, DebugRecordConfig};
    use crate::rr::{
        FakeRrPreflight, FakeRrRecorder, RrPreflightStatus, RrRecordResult, RrRuntime,
    };
    use crate::session::{
        DebugAdapterFactory, DebugAdapterSession, LaunchRequest, ReplaySeekTarget, SessionManager,
        WatchpointKind, WatchpointResult, WatchpointSpec,
    };
    use crate::tools::RecordParams;
    use crate::traces::{TraceId, TracePolicy, TraceStore};
    use crate::types::{
        DebugBreakpoint, DebugOutput, DebugRuntimeError, DebugScope, DebugSessionState,
        DebugStackFrame, DebugStop, DebugThread, DebugVariable, RuntimeFailure,
    };

    #[test]
    fn origin_target_exists_with_exact_serde_shapes_crash_end_and_source_path_line_column() {
        assert_eq!(
            serde_json::to_value(OriginTarget::Crash).unwrap(),
            json!({ "kind": "crash" })
        );
        assert_eq!(
            serde_json::to_value(OriginTarget::End).unwrap(),
            json!({ "kind": "end" })
        );
        assert_eq!(
            serde_json::to_value(OriginTarget::Source {
                path: "src/main.rs".to_owned(),
                line: 42,
                column: Some(7),
            })
            .unwrap(),
            json!({ "kind": "source", "path": "src/main.rs", "line": 42, "column": 7 })
        );
    }

    #[test]
    fn find_origin_request_exists_with_public_fields_trace_id_language_at_watch_timeout_secs_max_depth_and_max_children()
     {
        let request: FindOriginRequest = serde_json::from_value(json!({
            "trace_id": "trace-origin-1",
            "language": "rust",
            "at": { "kind": "source", "path": "src/main.rs", "line": 42, "column": null },
            "watch": "answer",
            "timeout_secs": 3,
            "max_depth": 2,
            "max_children": 4
        }))
        .unwrap();

        assert_eq!(request.trace_id, trace_id());
        assert_eq!(request.language, "rust");
        assert_eq!(
            request.at,
            OriginTarget::Source {
                path: "src/main.rs".to_owned(),
                line: 42,
                column: None,
            }
        );
        assert_eq!(request.watch, "answer");
        assert_eq!(request.timeout_secs, Some(3));
        assert_eq!(request.max_depth, Some(2));
        assert_eq!(request.max_children, Some(4));
    }

    #[test]
    fn find_origin_result_exists_with_public_fields_found_reason_trace_id_write_frame_stack_value_locals_args_output_truncated_and_error()
     {
        let frame = write_frame();
        let variable = captured_variable("answer", "42", false);
        let result = FindOriginResult {
            found: true,
            reason: None,
            trace_id: Some(trace_id()),
            write_frame: Some(frame.clone()),
            stack: vec![frame],
            value: Some(variable.clone()),
            locals: vec![variable.clone()],
            args: vec![captured_variable("argv", "[]", false)],
            output: vec![debug_output(1, "stdout", "hit\n")],
            truncated: true,
            error: None,
        };

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({
                "found": true,
                "reason": null,
                "trace_id": "trace-origin-1",
                "write_frame": {
                    "id": 10,
                    "name": "write_answer",
                    "path": "src/main.rs",
                    "line": 42,
                    "column": 7
                },
                "stack": [{
                    "id": 10,
                    "name": "write_answer",
                    "path": "src/main.rs",
                    "line": 42,
                    "column": 7
                }],
                "value": {
                    "name": "answer",
                    "value": "42",
                    "type": "i32",
                    "children": [],
                    "truncated": false
                },
                "locals": [{
                    "name": "answer",
                    "value": "42",
                    "type": "i32",
                    "children": [],
                    "truncated": false
                }],
                "args": [{
                    "name": "argv",
                    "value": "[]",
                    "type": "Vec<String>",
                    "children": [],
                    "truncated": false
                }],
                "output": [{ "sequence": 1, "category": "stdout", "text": "hit\n" }],
                "truncated": true,
                "error": null
            })
        );
    }

    #[test]
    fn origin_failure_code_exists_with_exact_serde_string_values() {
        let cases = [
            (
                OriginFailureCode::NoPriorWriteReached,
                "no_prior_write_reached",
            ),
            (
                OriginFailureCode::WatchEvaluationFailed,
                "watch_evaluation_failed",
            ),
            (OriginFailureCode::TraceNotFound, "trace_not_found"),
            (OriginFailureCode::ReplayOpenFailed, "replay_open_failed"),
            (OriginFailureCode::ReverseUnsupported, "reverse_unsupported"),
            (OriginFailureCode::OriginTimeout, "origin_timeout"),
            (OriginFailureCode::RecordFailed, "record_failed"),
            (OriginFailureCode::CaptureFailed, "capture_failed"),
        ];

        for (code, expected) in cases {
            assert_eq!(serde_json::to_value(&code).unwrap(), json!(expected));
            assert_eq!(
                serde_json::from_value::<OriginFailureCode>(json!(expected)).unwrap(),
                code
            );
        }
    }

    #[test]
    fn find_origin_result_returns_found_true_reason_none_error_none_populated_write_frame_stack_watched_value_output_and_truncation_markers_when_last_write_is_found()
     {
        let factory = Arc::new(FakeOriginFactory::new(
            FakeOriginScenario::last_write_found(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = find_origin(base_find_request(), &sessions);

        assert!(result.found);
        assert_eq!(result.reason, None);
        assert_eq!(result.error, None);
        assert_eq!(result.trace_id, Some(trace_id()));
        assert_eq!(result.write_frame, Some(write_frame()));
        assert_eq!(result.stack, vec![write_frame(), caller_frame()]);
        assert_eq!(result.value, Some(captured_variable("answer", "42", true)));
        assert_eq!(
            result.output,
            vec![debug_output(1, "stdout", "watch hit\n")]
        );
        assert!(result.truncated);
        assert_eq!(
            factory.calls(),
            vec![
                FakeOriginCall::Initialize,
                FakeOriginCall::Launch,
                FakeOriginCall::Stack(1),
                FakeOriginCall::SeekReplay(ReplaySeekTarget::Crash),
                FakeOriginCall::SetWatchpoint(WatchpointSpec {
                    expression: Some("answer".to_owned()),
                    address: None,
                    kind: WatchpointKind::Write,
                    enabled: true,
                }),
                FakeOriginCall::ReverseContinue(None),
                FakeOriginCall::Stack(1),
                FakeOriginCall::Scopes(10),
                FakeOriginCall::Variables(100),
                FakeOriginCall::Variables(200),
                FakeOriginCall::Evaluate(10, "answer".to_owned()),
                FakeOriginCall::Terminate,
            ]
        );
        assert!(sessions.sessions().is_empty());
    }

    #[test]
    fn find_origin_result_returns_found_false_no_prior_write_reached_serialized_when_reverse_execution_reaches_trace_start_without_a_write()
     {
        let factory = Arc::new(FakeOriginFactory::new(FakeOriginScenario::no_prior_write()));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = find_origin(base_find_request(), &sessions);

        assert!(!result.found);
        assert_eq!(result.reason, Some(OriginFailureCode::NoPriorWriteReached));
        assert_eq!(result.error, None);
        assert_eq!(
            serde_json::to_value(&result).unwrap()["reason"],
            "no_prior_write_reached"
        );
        assert!(factory.calls().contains(&FakeOriginCall::Terminate));
        assert!(sessions.sessions().is_empty());
    }

    #[test]
    fn watch_expression_evaluation_failure_returns_find_origin_result_found_false_watch_evaluation_failed_runtime_failure_not_panic_or_transport_error()
     {
        let factory = Arc::new(FakeOriginFactory::new(
            FakeOriginScenario::watch_evaluation_failed(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = find_origin(base_find_request(), &sessions);

        assert!(!result.found);
        assert_eq!(
            result.reason,
            Some(OriginFailureCode::WatchEvaluationFailed)
        );
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("watch_evaluation_failed")
        );
        assert!(factory.calls().contains(&FakeOriginCall::Terminate));
    }

    #[test]
    fn replay_open_failure_returns_replay_open_failed_instead_of_trace_not_found() {
        let sessions = SessionManager::new(
            debug_config(),
            Arc::new(FakeOriginFactory::new(
                FakeOriginScenario::last_write_found(),
            )),
        );
        let result = find_origin(
            FindOriginRequest {
                language: "missing-language".to_owned(),
                ..base_find_request()
            },
            &sessions,
        );

        assert!(!result.found);
        assert_eq!(result.reason, Some(OriginFailureCode::ReplayOpenFailed));
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("replay_open_failed")
        );
    }

    #[test]
    fn origin_target_maps_to_replay_seek_target_exactly_crash_end_and_source_path_line_column() {
        assert_eq!(
            ReplaySeekTarget::from(OriginTarget::Crash),
            ReplaySeekTarget::Crash
        );
        assert_eq!(
            ReplaySeekTarget::from(OriginTarget::End),
            ReplaySeekTarget::End
        );
        assert_eq!(
            ReplaySeekTarget::from(OriginTarget::Source {
                path: "src/main.rs".to_owned(),
                line: 42,
                column: Some(7),
            }),
            ReplaySeekTarget::Source {
                path: "src/main.rs".to_owned(),
                line: 42,
                column: Some(7),
            }
        );
    }

    #[test]
    fn session_manager_seek_replay_is_called_before_setting_the_watchpoint_and_seek_timeout_maps_to_origin_timeout_runtime_failure_code()
     {
        let factory = Arc::new(FakeOriginFactory::new(FakeOriginScenario::seek_timeout()));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = find_origin(base_find_request(), &sessions);

        assert!(!result.found);
        assert_eq!(result.reason, Some(OriginFailureCode::OriginTimeout));
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("origin_timeout")
        );
        assert_eq!(
            factory.calls(),
            vec![
                FakeOriginCall::Initialize,
                FakeOriginCall::Launch,
                FakeOriginCall::Stack(1),
                FakeOriginCall::SeekReplay(ReplaySeekTarget::Crash),
                FakeOriginCall::Terminate,
            ]
        );
    }

    #[test]
    fn trace_ids_containing_fixture_words_still_drive_replay_instead_of_fabricating_origin_results()
    {
        let factory = Arc::new(FakeOriginFactory::new(
            FakeOriginScenario::last_write_found(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());
        let mut request = base_find_request();
        request.trace_id = TraceId::new("real-nopriorwrite-adapterexited-trace").unwrap();

        let result = find_origin(request, &sessions);

        assert!(result.found);
        assert_eq!(
            factory.calls(),
            vec![
                FakeOriginCall::Initialize,
                FakeOriginCall::Launch,
                FakeOriginCall::Stack(1),
                FakeOriginCall::SeekReplay(ReplaySeekTarget::Crash),
                FakeOriginCall::SetWatchpoint(WatchpointSpec {
                    expression: Some("answer".to_owned()),
                    address: None,
                    kind: WatchpointKind::Write,
                    enabled: true,
                }),
                FakeOriginCall::ReverseContinue(None),
                FakeOriginCall::Stack(1),
                FakeOriginCall::Scopes(10),
                FakeOriginCall::Variables(100),
                FakeOriginCall::Variables(200),
                FakeOriginCall::Evaluate(10, "answer".to_owned()),
                FakeOriginCall::Terminate,
            ]
        );
    }

    #[test]
    fn origin_failure_code_mapping_is_exact_for_reverse_unsupported_no_prior_write_timeout_watch_evaluation_capture_and_record_failed()
     {
        let reverse = find_origin(
            base_find_request(),
            &SessionManager::new(
                debug_config(),
                Arc::new(FakeOriginFactory::new(
                    FakeOriginScenario::reverse_unsupported(),
                )),
            ),
        );
        assert_eq!(reverse.reason, Some(OriginFailureCode::ReverseUnsupported));
        assert_eq!(
            reverse.error.as_ref().map(|error| error.code.as_str()),
            Some("reverse_unsupported")
        );

        let capture = find_origin(
            base_find_request(),
            &SessionManager::new(
                debug_config(),
                Arc::new(FakeOriginFactory::new(FakeOriginScenario::capture_failed())),
            ),
        );
        assert_eq!(capture.reason, Some(OriginFailureCode::CaptureFailed));
        assert_eq!(
            capture.error.as_ref().map(|error| error.code.as_str()),
            Some("capture_failed")
        );

        let mut runtime = rr_runtime(record_failed_result());
        let combined =
            record_and_find_origin(base_record_and_find_request(), &manager(), &mut runtime);
        assert_eq!(
            combined
                .origin
                .as_ref()
                .and_then(|origin| origin.error.as_ref())
                .map(|error| error.code.as_str()),
            Some("record_failed")
        );
    }

    #[test]
    fn for_every_mapped_failure_find_origin_result_error_code_equals_serialized_origin_failure_code_except_no_prior_write_reached_has_no_error()
     {
        let cases = [
            (
                FakeOriginScenario::seek_timeout(),
                OriginFailureCode::OriginTimeout,
                Some("origin_timeout"),
            ),
            (
                FakeOriginScenario::reverse_unsupported(),
                OriginFailureCode::ReverseUnsupported,
                Some("reverse_unsupported"),
            ),
            (
                FakeOriginScenario::watch_evaluation_failed(),
                OriginFailureCode::WatchEvaluationFailed,
                Some("watch_evaluation_failed"),
            ),
            (
                FakeOriginScenario::capture_failed(),
                OriginFailureCode::CaptureFailed,
                Some("capture_failed"),
            ),
            (
                FakeOriginScenario::no_prior_write(),
                OriginFailureCode::NoPriorWriteReached,
                None,
            ),
        ];

        for (scenario, expected_reason, expected_error_code) in cases {
            let result = find_origin(
                base_find_request(),
                &SessionManager::new(debug_config(), Arc::new(FakeOriginFactory::new(scenario))),
            );
            assert_eq!(result.reason, Some(expected_reason));
            assert_eq!(
                result.error.as_ref().map(|error| error.code.as_str()),
                expected_error_code
            );
        }
    }

    #[test]
    fn cleanup_failures_do_not_replace_the_primary_origin_failure_code_and_are_included_in_runtime_failure_data_cleanup_error()
     {
        let factory = Arc::new(FakeOriginFactory::new(
            FakeOriginScenario::watch_evaluation_failed_with_cleanup_error(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = find_origin(base_find_request(), &sessions);

        assert_eq!(
            result.reason,
            Some(OriginFailureCode::WatchEvaluationFailed)
        );
        let error = result
            .error
            .expect("primary failure should include runtime error");
        assert_eq!(error.code, "watch_evaluation_failed");
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("cleanup_error")),
            Some(&json!("fake terminate failed"))
        );
        assert!(factory.calls().contains(&FakeOriginCall::Terminate));
    }

    #[test]
    fn find_origin_tears_down_replay_sessions_on_every_success_and_failure_branch() {
        for scenario in [
            FakeOriginScenario::last_write_found(),
            FakeOriginScenario::no_prior_write(),
            FakeOriginScenario::seek_timeout(),
            FakeOriginScenario::watch_evaluation_failed(),
            FakeOriginScenario::capture_failed(),
        ] {
            let factory = Arc::new(FakeOriginFactory::new(scenario));
            let sessions = SessionManager::new(debug_config(), factory.clone());

            let _ = find_origin(base_find_request(), &sessions);

            assert!(factory.calls().contains(&FakeOriginCall::Terminate));
            assert!(sessions.sessions().is_empty());
        }
    }

    #[test]
    fn record_and_find_origin_request_exists_with_public_record_and_origin_fields_and_record_origin_params_include_language_at_watch_timeout_secs_max_depth_max_children_but_no_trace_id()
     {
        let request: RecordAndFindOriginRequest = serde_json::from_value(json!({
            "record": {
                "language": "rust",
                "program": "target/debug/app",
                "args": ["--case", "crash"],
                "cwd": "/workspace",
                "env": { "RUST_BACKTRACE": "1" },
                "timeout_ms": 1000
            },
            "origin": {
                "language": "rust",
                "at": { "kind": "end" },
                "watch": "answer",
                "timeout_secs": 3,
                "max_depth": 2,
                "max_children": 4
            }
        }))
        .unwrap();

        assert_eq!(request.record.language, "rust");
        assert_eq!(request.record.program, "target/debug/app");
        assert_eq!(request.origin.language, "rust");
        assert_eq!(request.origin.at, OriginTarget::End);
        assert_eq!(request.origin.watch, "answer");
        assert_eq!(request.origin.timeout_secs, Some(3));
        assert_eq!(request.origin.max_depth, Some(2));
        assert_eq!(request.origin.max_children, Some(4));
        assert!(
            serde_json::from_value::<RecordOriginParams>(json!({
                "language": "rust",
                "trace_id": "trace-origin-1",
                "at": { "kind": "end" },
                "watch": "answer",
                "timeout_secs": 3,
                "max_depth": 2,
                "max_children": 4
            }))
            .is_err()
        );
    }

    #[test]
    fn record_and_find_origin_result_exists_with_public_fields_record_and_origin() {
        let result = super::RecordAndFindOriginResult {
            record: successful_record_result(),
            origin: Some(successful_origin_result()),
        };

        let value = serde_json::to_value(result).unwrap();

        assert_eq!(value["record"]["recordable"], true);
        assert_eq!(value["record"]["trace_id"], "trace-origin-1");
        assert_eq!(value["origin"]["found"], true);
    }

    #[test]
    fn record_and_find_origin_returns_record_and_origin_some_found_true_when_both_steps_succeed() {
        let factory = Arc::new(FakeOriginFactory::new(
            FakeOriginScenario::last_write_found(),
        ));
        let sessions = SessionManager::new(debug_config(), factory);
        let mut runtime = rr_runtime(successful_record_result());

        let result =
            record_and_find_origin(base_record_and_find_request(), &sessions, &mut runtime);

        assert!(result.record.recordable);
        assert_eq!(result.record.trace_id, Some(trace_id()));
        let origin = result
            .origin
            .expect("origin should run after successful record");
        assert!(origin.found);
        assert_eq!(origin.reason, None);
    }

    #[test]
    fn record_and_find_origin_returns_record_and_origin_some_found_false_when_recording_succeeds_but_origin_finding_fails()
     {
        let factory = Arc::new(FakeOriginFactory::new(FakeOriginScenario::no_prior_write()));
        let sessions = SessionManager::new(debug_config(), factory);
        let mut runtime = rr_runtime(successful_record_result());

        let result =
            record_and_find_origin(base_record_and_find_request(), &sessions, &mut runtime);

        assert!(result.record.recordable);
        let origin = result
            .origin
            .expect("origin failure should still be returned");
        assert!(!origin.found);
        assert_eq!(origin.reason, Some(OriginFailureCode::NoPriorWriteReached));
    }

    #[test]
    fn record_and_find_origin_returns_record_unchanged_and_origin_none_when_rr_preflight_or_recording_is_unsupported()
     {
        let record = unsupported_record_result();
        let mut runtime = rr_runtime(record.clone());

        let result =
            record_and_find_origin(base_record_and_find_request(), &manager(), &mut runtime);

        assert_eq!(result.record, record);
        assert_eq!(result.origin, None);
    }

    fn manager() -> SessionManager {
        SessionManager::new(
            debug_config(),
            Arc::new(FakeOriginFactory::new(
                FakeOriginScenario::last_write_found(),
            )),
        )
    }

    fn base_find_request() -> FindOriginRequest {
        FindOriginRequest {
            trace_id: trace_id(),
            language: "rust".to_owned(),
            at: OriginTarget::Crash,
            watch: "answer".to_owned(),
            timeout_secs: Some(3),
            max_depth: Some(2),
            max_children: Some(1),
        }
    }

    fn base_record_and_find_request() -> RecordAndFindOriginRequest {
        RecordAndFindOriginRequest {
            record: RecordParams {
                language: "rust".to_owned(),
                program: "target/debug/app".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                timeout_ms: Some(1000),
            },
            origin: RecordOriginParams {
                language: "rust".to_owned(),
                at: OriginTarget::Crash,
                watch: "answer".to_owned(),
                timeout_secs: Some(3),
                max_depth: Some(2),
                max_children: Some(1),
            },
        }
    }

    fn debug_config() -> DebugInitializeConfig {
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
            record: Some(DebugRecordConfig {
                backend: "rr".to_owned(),
                trace_dir: Some("target/origin-test-traces".to_owned()),
                ttl_secs: Some(60),
                max_traces: Some(20),
                record_timeout_secs: Some(60),
            }),
        }
    }

    fn rr_runtime(record_result: RrRecordResult) -> RrRuntime {
        RrRuntime::with_parts(
            Box::new(FakeRrPreflight::new(RrPreflightStatus::Supported)),
            Box::new(FakeRrRecorder::new(record_result)),
            TraceStore::new(TracePolicy {
                trace_root: std::env::temp_dir().join("tower-origin-test-traces"),
                ttl_secs: Some(60),
                max_traces: 20,
                record_timeout_secs: 60,
            }),
        )
    }

    fn successful_origin_result() -> FindOriginResult {
        FindOriginResult {
            found: true,
            reason: None,
            trace_id: Some(trace_id()),
            write_frame: Some(write_frame()),
            stack: vec![write_frame()],
            value: Some(captured_variable("answer", "42", false)),
            locals: vec![captured_variable("answer", "42", false)],
            args: Vec::new(),
            output: vec![debug_output(1, "stdout", "watch hit\n")],
            truncated: false,
            error: None,
        }
    }

    fn successful_record_result() -> RrRecordResult {
        RrRecordResult {
            recordable: true,
            reason: None,
            trace_id: Some(trace_id()),
            trace: None,
            exit_code: Some(0),
            output: vec![debug_output(1, "stdout", "recorded\n")],
            output_truncated: false,
            error: None,
        }
    }

    fn unsupported_record_result() -> RrRecordResult {
        RrRecordResult {
            recordable: false,
            reason: Some("rr_unsupported".to_owned()),
            trace_id: None,
            trace: None,
            exit_code: None,
            output: Vec::new(),
            output_truncated: false,
            error: Some(RuntimeFailure {
                code: "rr_unsupported".to_owned(),
                message: "rr unsupported".to_owned(),
                data: None,
            }),
        }
    }

    fn record_failed_result() -> RrRecordResult {
        RrRecordResult {
            recordable: false,
            reason: Some("record_failed".to_owned()),
            trace_id: None,
            trace: None,
            exit_code: None,
            output: Vec::new(),
            output_truncated: false,
            error: Some(RuntimeFailure {
                code: "record_failed".to_owned(),
                message: "record failed".to_owned(),
                data: None,
            }),
        }
    }

    fn trace_id() -> TraceId {
        TraceId::new("trace-origin-1").unwrap()
    }

    fn write_frame() -> DebugStackFrame {
        DebugStackFrame {
            id: 10,
            name: "write_answer".to_owned(),
            path: Some("src/main.rs".to_owned()),
            line: 42,
            column: 7,
        }
    }

    fn caller_frame() -> DebugStackFrame {
        DebugStackFrame {
            id: 11,
            name: "caller".to_owned(),
            path: Some("src/lib.rs".to_owned()),
            line: 30,
            column: 9,
        }
    }

    fn captured_variable(name: &str, value: &str, truncated: bool) -> CapturedVariable {
        CapturedVariable {
            name: name.to_owned(),
            value: value.to_owned(),
            r#type: Some(if name == "argv" { "Vec<String>" } else { "i32" }.to_owned()),
            children: if truncated {
                vec![CapturedVariable {
                    name: "child".to_owned(),
                    value: "1".to_owned(),
                    r#type: Some("i32".to_owned()),
                    children: Vec::new(),
                    truncated: false,
                }]
            } else {
                Vec::new()
            },
            truncated,
        }
    }

    fn debug_output(sequence: u64, category: &str, text: &str) -> DebugOutput {
        DebugOutput {
            sequence,
            category: Some(category.to_owned()),
            text: text.to_owned(),
        }
    }

    #[derive(Clone)]
    struct FakeOriginFactory {
        scenario: FakeOriginScenario,
        calls: Arc<Mutex<Vec<FakeOriginCall>>>,
    }

    impl FakeOriginFactory {
        fn new(scenario: FakeOriginScenario) -> Self {
            Self {
                scenario,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<FakeOriginCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl DebugAdapterFactory for FakeOriginFactory {
        fn start(
            &self,
            _request: &LaunchRequest,
        ) -> Result<Box<dyn DebugAdapterSession>, DebugRuntimeError> {
            Ok(Box::new(FakeOriginSession {
                scenario: self.scenario.clone(),
                calls: self.calls.clone(),
            }))
        }
    }

    #[derive(Clone)]
    struct FakeOriginScenario {
        reverse_stops: Arc<Mutex<VecDeque<Result<DebugStop, DebugRuntimeError>>>>,
        seek_error: Option<DebugRuntimeError>,
        watch_error: Option<DebugRuntimeError>,
        evaluate_error: Option<DebugRuntimeError>,
        variables_error: Option<DebugRuntimeError>,
        terminate_error: Option<DebugRuntimeError>,
    }

    impl FakeOriginScenario {
        fn last_write_found() -> Self {
            Self::with_reverse_stop(Ok(DebugStop {
                reason: Some("watchpoint".to_owned()),
                output_since: vec![debug_output(1, "stdout", "watch hit\n")],
                ..stopped()
            }))
        }

        fn no_prior_write() -> Self {
            Self::with_reverse_stop(Ok(DebugStop {
                state: DebugSessionState::Terminated,
                reason: Some("trace_start".to_owned()),
                thread_id: None,
                top_frame: None,
                hit_breakpoint_ids: Vec::new(),
                timed_out: false,
                exit_code: None,
                output_since: Vec::new(),
            }))
        }

        fn seek_timeout() -> Self {
            Self {
                seek_error: Some(DebugRuntimeError::DebugTimeout(
                    "seek replay timed out".to_owned(),
                )),
                ..Self::last_write_found()
            }
        }

        fn reverse_unsupported() -> Self {
            Self {
                reverse_stops: Arc::new(Mutex::new(VecDeque::from([Err(
                    DebugRuntimeError::ReverseUnsupported("reverse unsupported".to_owned()),
                )]))),
                ..Self::last_write_found()
            }
        }

        fn watch_evaluation_failed() -> Self {
            Self {
                evaluate_error: Some(DebugRuntimeError::LaunchFailed(
                    "watch expression failed".to_owned(),
                )),
                ..Self::last_write_found()
            }
        }

        fn watch_evaluation_failed_with_cleanup_error() -> Self {
            Self {
                terminate_error: Some(DebugRuntimeError::AdapterExited(
                    "fake terminate failed".to_owned(),
                )),
                ..Self::watch_evaluation_failed()
            }
        }

        fn capture_failed() -> Self {
            Self {
                variables_error: Some(DebugRuntimeError::DebugTimeout(
                    "variables timed out".to_owned(),
                )),
                ..Self::last_write_found()
            }
        }

        fn with_reverse_stop(stop: Result<DebugStop, DebugRuntimeError>) -> Self {
            Self {
                reverse_stops: Arc::new(Mutex::new(VecDeque::from([stop]))),
                seek_error: None,
                watch_error: None,
                evaluate_error: None,
                variables_error: None,
                terminate_error: None,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeOriginCall {
        Initialize,
        Launch,
        SeekReplay(ReplaySeekTarget),
        SetWatchpoint(WatchpointSpec),
        ReverseContinue(Option<u64>),
        Stack(u64),
        Scopes(u64),
        Variables(u64),
        Evaluate(u64, String),
        Terminate,
    }

    struct FakeOriginSession {
        scenario: FakeOriginScenario,
        calls: Arc<Mutex<Vec<FakeOriginCall>>>,
    }

    impl DebugAdapterSession for FakeOriginSession {
        fn initialize(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeOriginCall::Initialize);
            Ok(())
        }

        fn launch(
            &mut self,
            _request: &LaunchRequest,
            _timeout: Duration,
        ) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeOriginCall::Launch);
            Ok(())
        }

        fn set_breakpoints(
            &mut self,
            breakpoints: &[DebugBreakpoint],
            _timeout: Duration,
        ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError> {
            Ok(breakpoints.to_vec())
        }

        fn continue_session(&mut self, _timeout: Duration) -> Result<DebugStop, DebugRuntimeError> {
            Ok(stopped())
        }

        fn step(
            &mut self,
            _thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            Ok(stopped())
        }

        fn pause(
            &mut self,
            thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            Ok(DebugStop {
                thread_id,
                ..stopped()
            })
        }

        fn threads(&mut self, _timeout: Duration) -> Result<Vec<DebugThread>, DebugRuntimeError> {
            Ok(vec![DebugThread {
                id: 1,
                name: "main".to_owned(),
            }])
        }

        fn stack(
            &mut self,
            thread_id: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeOriginCall::Stack(thread_id));
            Ok(vec![write_frame(), caller_frame()])
        }

        fn scopes(
            &mut self,
            frame_id: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeOriginCall::Scopes(frame_id));
            Ok(vec![
                DebugScope {
                    name: "Locals".to_owned(),
                    variables_reference: 100,
                    expensive: false,
                },
                DebugScope {
                    name: "Arguments".to_owned(),
                    variables_reference: 300,
                    expensive: false,
                },
            ])
        }

        fn variables(
            &mut self,
            variables_reference: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeOriginCall::Variables(variables_reference));
            if let Some(error) = &self.scenario.variables_error {
                return Err(error.clone());
            }
            Ok(match variables_reference {
                100 => vec![DebugVariable {
                    name: "answer".to_owned(),
                    value: "42".to_owned(),
                    r#type: Some("i32".to_owned()),
                    variables_reference: 200,
                }],
                200 => vec![
                    DebugVariable {
                        name: "child".to_owned(),
                        value: "1".to_owned(),
                        r#type: Some("i32".to_owned()),
                        variables_reference: 0,
                    },
                    DebugVariable {
                        name: "extra".to_owned(),
                        value: "2".to_owned(),
                        r#type: Some("i32".to_owned()),
                        variables_reference: 0,
                    },
                ],
                300 => vec![DebugVariable {
                    name: "argv".to_owned(),
                    value: "[]".to_owned(),
                    r#type: Some("Vec<String>".to_owned()),
                    variables_reference: 0,
                }],
                _ => Vec::new(),
            })
        }

        fn evaluate(
            &mut self,
            frame_id: u64,
            expression: &str,
            _timeout: Duration,
        ) -> Result<DebugVariable, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeOriginCall::Evaluate(frame_id, expression.to_owned()));
            if let Some(error) = &self.scenario.evaluate_error {
                return Err(error.clone());
            }
            Ok(DebugVariable {
                name: expression.to_owned(),
                value: "42".to_owned(),
                r#type: Some("i32".to_owned()),
                variables_reference: 200,
            })
        }

        fn reverse_continue(
            &mut self,
            thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeOriginCall::ReverseContinue(thread_id));
            self.scenario
                .reverse_stops
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(stopped()))
        }

        fn set_watchpoint(
            &mut self,
            watchpoint: WatchpointSpec,
            _timeout: Duration,
        ) -> Result<WatchpointResult, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeOriginCall::SetWatchpoint(watchpoint.clone()));
            if let Some(error) = &self.scenario.watch_error {
                return Err(error.clone());
            }
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
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeOriginCall::SeekReplay(target));
            if let Some(error) = &self.scenario.seek_error {
                return Err(error.clone());
            }
            Ok(stopped())
        }

        fn terminate(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeOriginCall::Terminate);
            if let Some(error) = &self.scenario.terminate_error {
                return Err(error.clone());
            }
            Ok(())
        }

        fn disconnect(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            Ok(())
        }
    }

    fn stopped() -> DebugStop {
        DebugStop {
            state: DebugSessionState::Stopped,
            reason: Some("stopped".to_owned()),
            thread_id: Some(1),
            top_frame: Some(write_frame()),
            hit_breakpoint_ids: Vec::new(),
            timed_out: false,
            exit_code: None,
            output_since: Vec::new(),
        }
    }
}
