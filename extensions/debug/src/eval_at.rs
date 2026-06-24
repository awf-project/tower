#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::protocol::{DebugToolError, DebugToolErrorCode};
use crate::session::{LaunchRequest, SessionManager};
use crate::types::{
    DebugBreakpoint, DebugOutput, DebugRuntimeError, DebugScope, DebugSessionId, DebugSessionState,
    DebugStackFrame, DebugStop, DebugVariable,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalAtRequest {
    pub lang: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub breakpoint: Option<BreakpointProbe>,
    #[serde(default)]
    pub expressions: Vec<String>,
    #[serde(default)]
    pub capture: CaptureOptions,
    #[serde(default)]
    pub on_hit: EvalAtHitMode,
    #[serde(default = "default_max_hits")]
    pub max_hits: usize,
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    #[serde(default = "default_max_children")]
    pub max_children: usize,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BreakpointProbe {
    pub path: String,
    pub line: u64,
    pub condition: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureOptions {
    pub stack: bool,
    pub locals: bool,
    pub args: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            stack: true,
            locals: true,
            args: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalAtHitMode {
    #[default]
    First,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalAtFinished {
    Stopped,
    Exited,
    Timeout,
    Terminated,
    AdapterExited,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedVariable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    pub children: Vec<CapturedVariable>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EvalAtExpressionResult {
    Value {
        value: String,
        #[serde(rename = "type")]
        r#type: Option<String>,
    },
    Error {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalAtHit {
    pub thread_id: Option<u64>,
    pub frame: Option<DebugStackFrame>,
    pub stack: Vec<DebugStackFrame>,
    pub locals: Vec<CapturedVariable>,
    pub args: Vec<CapturedVariable>,
    pub evaluated: BTreeMap<String, EvalAtExpressionResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalAtResult {
    pub hit: bool,
    pub hits: Vec<EvalAtHit>,
    pub output: Vec<DebugOutput>,
    pub finished: EvalAtFinished,
    pub exit_code: Option<i64>,
    pub condition_unsupported: Option<bool>,
}

pub fn run_eval_at(
    request: EvalAtRequest,
    sessions: &SessionManager,
) -> Result<EvalAtResult, DebugToolError> {
    let timeout = request.timeout_ms.map(Duration::from_millis);
    let initial_breakpoints = request
        .breakpoint
        .as_ref()
        .map(|probe| {
            vec![DebugBreakpoint {
                path: probe.path.clone(),
                line: probe.line,
                condition: probe.condition.clone(),
                hit_condition: None,
                verified: false,
                verified_id: None,
            }]
        })
        .unwrap_or_default();
    let (launch, configured_breakpoints) = sessions
        .launch_with_initial_breakpoints(
            LaunchRequest {
                language: request.lang.clone(),
                program: request.program.clone(),
                cwd: request.cwd.clone(),
                args: request.args.clone(),
                env: request.env.clone(),
                launch_overrides: Map::new(),
            },
            initial_breakpoints,
        )
        .map_err(runtime_tool_error)?;

    let session_id = launch.session_id;
    if let Some(probe) = &request.breakpoint {
        let condition_unsupported = (probe.condition.is_some()
            && configured_breakpoints
                .iter()
                .any(|breakpoint| !breakpoint.verified))
        .then_some(true);
        run_eval_at_loop(
            request,
            sessions,
            session_id,
            timeout,
            condition_unsupported,
        )
    } else {
        run_eval_at_loop(request, sessions, session_id, timeout, None)
    }
}

fn run_eval_at_loop(
    request: EvalAtRequest,
    sessions: &SessionManager,
    session_id: DebugSessionId,
    timeout: Option<Duration>,
    condition_unsupported: Option<bool>,
) -> Result<EvalAtResult, DebugToolError> {
    let max_hits = request.max_hits;
    let mut hits = Vec::new();
    let mut output = Vec::new();

    loop {
        let stop = match sessions.continue_session(&session_id, timeout) {
            Ok(stop) => stop,
            Err(DebugRuntimeError::DebugTimeout(_)) => {
                return finish_after_cleanup(
                    sessions,
                    &session_id,
                    hits,
                    output,
                    EvalAtFinished::Timeout,
                    None,
                    condition_unsupported,
                );
            }
            Err(error) if is_adapter_gone(&error) => {
                return finish_after_cleanup(
                    sessions,
                    &session_id,
                    Vec::new(),
                    output,
                    EvalAtFinished::AdapterExited,
                    None,
                    condition_unsupported,
                );
            }
            Err(error) => return fail_after_cleanup(sessions, &session_id, error),
        };

        output.extend(stop.output_since.iter().cloned());

        if stop.timed_out {
            return finish_after_cleanup(
                sessions,
                &session_id,
                hits,
                output,
                EvalAtFinished::Timeout,
                None,
                condition_unsupported,
            );
        }

        match stop.state {
            DebugSessionState::Stopped => {
                match capture_hit(&request, sessions, &session_id, &stop) {
                    Ok(hit) => hits.push(hit),
                    Err(CaptureError::AdapterGone) => {
                        return finish_after_cleanup(
                            sessions,
                            &session_id,
                            Vec::new(),
                            output,
                            EvalAtFinished::AdapterExited,
                            None,
                            condition_unsupported,
                        );
                    }
                    Err(CaptureError::Runtime(error)) => {
                        return fail_after_cleanup(sessions, &session_id, error);
                    }
                }

                if matches!(request.on_hit, EvalAtHitMode::First) || hits.len() >= max_hits {
                    return finish_after_cleanup(
                        sessions,
                        &session_id,
                        hits,
                        output,
                        EvalAtFinished::Stopped,
                        None,
                        condition_unsupported,
                    );
                }
            }
            DebugSessionState::Terminated => {
                return finish_after_cleanup(
                    sessions,
                    &session_id,
                    hits,
                    output,
                    EvalAtFinished::Exited,
                    stop.exit_code,
                    condition_unsupported,
                );
            }
            DebugSessionState::Running | DebugSessionState::Initializing => {
                return finish_after_cleanup(
                    sessions,
                    &session_id,
                    hits,
                    output,
                    EvalAtFinished::Timeout,
                    None,
                    condition_unsupported,
                );
            }
        }
    }
}

enum CaptureError {
    AdapterGone,
    Runtime(DebugRuntimeError),
}

fn capture_hit(
    request: &EvalAtRequest,
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    stop: &DebugStop,
) -> Result<EvalAtHit, CaptureError> {
    let frame = stop.top_frame.clone();
    let stack = if request.capture.stack {
        match stop.thread_id {
            Some(thread_id) => inspect(sessions.stack(session_id, thread_id))?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let frame_for_inspection = frame.clone().or_else(|| stack.first().cloned());

    let (locals, args) = match frame_for_inspection.as_ref() {
        Some(frame) if request.capture.locals || request.capture.args => {
            capture_scopes(request, sessions, session_id, frame.id)?
        }
        _ => (Vec::new(), Vec::new()),
    };

    let evaluated = match frame_for_inspection.as_ref() {
        Some(frame) => evaluate_expressions(request, sessions, session_id, frame.id)?,
        None => BTreeMap::new(),
    };

    Ok(EvalAtHit {
        thread_id: stop.thread_id,
        frame,
        stack,
        locals,
        args,
        evaluated,
    })
}

fn capture_scopes(
    request: &EvalAtRequest,
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    frame_id: u64,
) -> Result<(Vec<CapturedVariable>, Vec<CapturedVariable>), CaptureError> {
    let scopes = inspect(sessions.scopes(session_id, frame_id))?;
    let mut locals = Vec::new();
    let mut args = Vec::new();

    for scope in scopes {
        if request.capture.locals && is_local_scope(&scope) {
            locals.extend(capture_variables(
                sessions,
                session_id,
                scope.variables_reference,
                request.max_depth,
                request.max_children,
            )?);
        } else if request.capture.args && is_argument_scope(&scope) {
            args.extend(capture_variables(
                sessions,
                session_id,
                scope.variables_reference,
                request.max_depth,
                request.max_children,
            )?);
        }
    }

    Ok((locals, args))
}

fn capture_variables(
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    variables_reference: u64,
    max_depth: usize,
    max_children: usize,
) -> Result<Vec<CapturedVariable>, CaptureError> {
    inspect(sessions.variables(session_id, variables_reference))?
        .into_iter()
        .map(|variable| {
            expand_variable(
                sessions,
                session_id,
                variable,
                max_depth.saturating_sub(1),
                max_children,
            )
        })
        .collect()
}

fn expand_variable(
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    variable: DebugVariable,
    remaining_depth: usize,
    max_children: usize,
) -> Result<CapturedVariable, CaptureError> {
    let mut children = Vec::new();
    let mut truncated = false;

    if variable.variables_reference != 0 {
        if remaining_depth == 0 {
            truncated = true;
        } else {
            let raw_children =
                inspect(sessions.variables(session_id, variable.variables_reference))?;
            truncated = raw_children.len() > max_children;
            for child in raw_children.into_iter().take(max_children) {
                children.push(expand_variable(
                    sessions,
                    session_id,
                    child,
                    remaining_depth.saturating_sub(1),
                    max_children,
                )?);
            }
        }
    }

    Ok(CapturedVariable {
        name: variable.name,
        value: variable.value,
        r#type: variable.r#type,
        children,
        truncated,
    })
}

fn evaluate_expressions(
    request: &EvalAtRequest,
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    frame_id: u64,
) -> Result<BTreeMap<String, EvalAtExpressionResult>, CaptureError> {
    let mut evaluated = BTreeMap::new();
    for expression in &request.expressions {
        let result = match sessions.evaluate(session_id, frame_id, expression.clone()) {
            Ok(variable) => EvalAtExpressionResult::Value {
                value: variable.value,
                r#type: variable.r#type,
            },
            Err(error) if is_adapter_gone(&error) => return Err(CaptureError::AdapterGone),
            Err(error) => EvalAtExpressionResult::Error {
                error: runtime_message(error),
            },
        };
        evaluated.insert(expression.clone(), result);
    }
    Ok(evaluated)
}

fn finish_after_cleanup(
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    hits: Vec<EvalAtHit>,
    output: Vec<DebugOutput>,
    finished: EvalAtFinished,
    exit_code: Option<i64>,
    condition_unsupported: Option<bool>,
) -> Result<EvalAtResult, DebugToolError> {
    let result = EvalAtResult {
        hit: !hits.is_empty(),
        hits,
        output,
        finished,
        exit_code,
        condition_unsupported,
    };
    match sessions.terminate(session_id) {
        Ok(()) | Err(DebugRuntimeError::SessionNotFound(_)) => Ok(result),
        Err(error) => Err(runtime_tool_error(error)),
    }
}

fn fail_after_cleanup<T>(
    sessions: &SessionManager,
    session_id: &DebugSessionId,
    error: DebugRuntimeError,
) -> Result<T, DebugToolError> {
    let _ = sessions.terminate(session_id);
    Err(runtime_tool_error(error))
}

fn inspect<T>(result: Result<T, DebugRuntimeError>) -> Result<T, CaptureError> {
    result.map_err(|error| {
        if is_adapter_gone(&error) {
            CaptureError::AdapterGone
        } else {
            CaptureError::Runtime(error)
        }
    })
}

fn is_local_scope(scope: &DebugScope) -> bool {
    scope.name.eq_ignore_ascii_case("locals") || scope.name.eq_ignore_ascii_case("local")
}

fn is_argument_scope(scope: &DebugScope) -> bool {
    matches!(
        scope.name.to_ascii_lowercase().as_str(),
        "arguments" | "args" | "parameters" | "params"
    )
}

fn is_adapter_gone(error: &DebugRuntimeError) -> bool {
    matches!(
        error,
        DebugRuntimeError::AdapterExited(_) | DebugRuntimeError::SessionNotFound(_)
    )
}

fn runtime_tool_error(error: DebugRuntimeError) -> DebugToolError {
    DebugToolError {
        code: match error {
            DebugRuntimeError::SessionNotFound(_) => DebugToolErrorCode::SessionNotFound,
            DebugRuntimeError::NotStopped(_) => DebugToolErrorCode::NotStopped,
            DebugRuntimeError::DebugTimeout(_) => DebugToolErrorCode::DebugTimeout,
            DebugRuntimeError::AdapterExited(_) | DebugRuntimeError::LaunchFailed(_) => {
                DebugToolErrorCode::InvalidParams
            }
        },
        message: runtime_message(error),
    }
}

fn runtime_message(error: DebugRuntimeError) -> String {
    match error {
        DebugRuntimeError::SessionNotFound(message)
        | DebugRuntimeError::NotStopped(message)
        | DebugRuntimeError::DebugTimeout(message)
        | DebugRuntimeError::AdapterExited(message)
        | DebugRuntimeError::LaunchFailed(message) => message,
    }
}

fn default_max_hits() -> usize {
    1
}

fn default_max_depth() -> usize {
    2
}

fn default_max_children() -> usize {
    50
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Map, Value, json};

    use super::{
        BreakpointProbe, CaptureOptions, CapturedVariable, EvalAtExpressionResult, EvalAtFinished,
        EvalAtHit, EvalAtHitMode, EvalAtRequest, EvalAtResult, run_eval_at,
    };
    use crate::protocol::{DebugAdapterConfig, DebugInitializeConfig, DebugToolErrorCode};
    use crate::session::{DebugAdapterFactory, DebugAdapterSession, LaunchRequest, SessionManager};
    use crate::types::{
        DebugBreakpoint, DebugOutput, DebugRuntimeError, DebugScope, DebugSessionState,
        DebugStackFrame, DebugStop, DebugThread, DebugVariable,
    };

    #[test]
    fn eval_at_request_deserializes_lang_program_args_cwd_env_breakpoint_expressions_capture_on_hit_max_hits_max_depth_max_children_and_timeout_ms()
     {
        let request: EvalAtRequest = serde_json::from_value(json!({
            "lang": "rust",
            "program": "target/debug/probe_fixture",
            "args": ["--case", "breakpoint"],
            "cwd": "/workspace",
            "env": { "RUST_LOG": "debug" },
            "breakpoint": {
                "path": "src/main.rs",
                "line": 12,
                "condition": "answer == 42"
            },
            "expressions": ["answer", "answer + 1"],
            "capture": {
                "stack": false,
                "locals": true,
                "args": false
            },
            "on_hit": "all",
            "max_hits": 3,
            "max_depth": 4,
            "max_children": 5,
            "timeout_ms": 250
        }))
        .unwrap();

        assert_eq!(request.lang, "rust");
        assert_eq!(request.program, "target/debug/probe_fixture");
        assert_eq!(request.args, ["--case", "breakpoint"]);
        assert_eq!(request.cwd.as_deref(), Some("/workspace"));
        assert_eq!(
            request.env.get("RUST_LOG").map(String::as_str),
            Some("debug")
        );
        assert_eq!(
            request.breakpoint,
            Some(BreakpointProbe {
                path: "src/main.rs".to_owned(),
                line: 12,
                condition: Some("answer == 42".to_owned()),
            })
        );
        assert_eq!(request.expressions, ["answer", "answer + 1"]);
        assert_eq!(
            request.capture,
            CaptureOptions {
                stack: false,
                locals: true,
                args: false,
            }
        );
        assert_eq!(request.on_hit, EvalAtHitMode::All);
        assert_eq!(request.max_hits, 3);
        assert_eq!(request.max_depth, 4);
        assert_eq!(request.max_children, 5);
        assert_eq!(request.timeout_ms, Some(250));
    }

    #[test]
    fn eval_at_request_defaults_args_env_expressions_capture_on_hit_max_hits_max_depth_and_max_children()
     {
        let request: EvalAtRequest = serde_json::from_value(json!({
            "lang": "rust",
            "program": "target/debug/probe_fixture"
        }))
        .unwrap();

        assert!(request.args.is_empty());
        assert!(request.env.is_empty());
        assert!(request.expressions.is_empty());
        assert_eq!(
            request.capture,
            CaptureOptions {
                stack: true,
                locals: true,
                args: true,
            }
        );
        assert_eq!(request.on_hit, EvalAtHitMode::First);
        assert_eq!(request.max_hits, 1);
        assert_eq!(request.max_depth, 2);
        assert_eq!(request.max_children, 50);
    }

    #[test]
    fn breakpoint_probe_serializes_and_deserializes_path_line_and_optional_condition() {
        let breakpoint = BreakpointProbe {
            path: "src/main.rs".to_owned(),
            line: 12,
            condition: None,
        };

        let value = serde_json::to_value(&breakpoint).unwrap();
        assert_eq!(
            value,
            json!({
                "path": "src/main.rs",
                "line": 12,
                "condition": null
            })
        );
        assert_eq!(
            serde_json::from_value::<BreakpointProbe>(value).unwrap(),
            breakpoint
        );
    }

    #[test]
    fn eval_at_hit_mode_accepts_exactly_json_strings_first_and_all_and_rejects_other_strings() {
        assert_eq!(
            serde_json::from_value::<EvalAtHitMode>(json!("first")).unwrap(),
            EvalAtHitMode::First
        );
        assert_eq!(
            serde_json::from_value::<EvalAtHitMode>(json!("all")).unwrap(),
            EvalAtHitMode::All
        );
        assert!(serde_json::from_value::<EvalAtHitMode>(json!("FIRST")).is_err());
        assert!(serde_json::from_value::<EvalAtHitMode>(json!("once")).is_err());
        assert!(serde_json::from_value::<EvalAtHitMode>(json!("")).is_err());
    }

    #[test]
    fn eval_at_finished_serializes_stable_json_strings_stopped_exited_timeout_terminated_and_adapter_exited()
     {
        assert_eq!(
            serde_json::to_value(EvalAtFinished::Stopped).unwrap(),
            json!("stopped")
        );
        assert_eq!(
            serde_json::to_value(EvalAtFinished::Exited).unwrap(),
            json!("exited")
        );
        assert_eq!(
            serde_json::to_value(EvalAtFinished::Timeout).unwrap(),
            json!("timeout")
        );
        assert_eq!(
            serde_json::to_value(EvalAtFinished::Terminated).unwrap(),
            json!("terminated")
        );
        assert_eq!(
            serde_json::to_value(EvalAtFinished::AdapterExited).unwrap(),
            json!("adapter_exited")
        );
    }

    #[test]
    fn captured_variable_serializes_name_value_type_children_and_truncated_and_nested_children_round_trip_without_losing_order()
     {
        let variable = CapturedVariable {
            name: "root".to_owned(),
            value: "{fields}".to_owned(),
            r#type: Some("Struct".to_owned()),
            children: vec![
                CapturedVariable {
                    name: "first".to_owned(),
                    value: "1".to_owned(),
                    r#type: Some("i32".to_owned()),
                    children: Vec::new(),
                    truncated: false,
                },
                CapturedVariable {
                    name: "second".to_owned(),
                    value: "2".to_owned(),
                    r#type: None,
                    children: vec![CapturedVariable {
                        name: "nested".to_owned(),
                        value: "3".to_owned(),
                        r#type: Some("i32".to_owned()),
                        children: Vec::new(),
                        truncated: false,
                    }],
                    truncated: true,
                },
            ],
            truncated: true,
        };

        let value = serde_json::to_value(&variable).unwrap();
        assert_eq!(value["name"], json!("root"));
        assert_eq!(value["value"], json!("{fields}"));
        assert_eq!(value["type"], json!("Struct"));
        assert_eq!(value["truncated"], json!(true));
        assert_eq!(value["children"][0]["name"], json!("first"));
        assert_eq!(value["children"][0]["type"], json!("i32"));
        assert_eq!(value["children"][1]["name"], json!("second"));
        assert_optional_null_or_absent(&value["children"][1], "type");
        assert_eq!(value["children"][1]["children"][0]["name"], json!("nested"));
        assert_eq!(value["children"][1]["children"][0]["type"], json!("i32"));
        assert_eq!(
            serde_json::from_value::<CapturedVariable>(value).unwrap(),
            variable
        );
    }

    #[test]
    fn eval_at_expression_result_represents_value_or_error_without_failing_the_enclosing_eval_at_result()
     {
        let mut evaluated = BTreeMap::new();
        evaluated.insert(
            "answer".to_owned(),
            EvalAtExpressionResult::Value {
                value: "42".to_owned(),
                r#type: Some("i32".to_owned()),
            },
        );
        evaluated.insert(
            "missing".to_owned(),
            EvalAtExpressionResult::Error {
                error: "unknown identifier".to_owned(),
            },
        );
        let result = EvalAtResult {
            hit: true,
            hits: vec![EvalAtHit {
                thread_id: None,
                frame: None,
                stack: Vec::new(),
                locals: Vec::new(),
                args: Vec::new(),
                evaluated,
            }],
            output: Vec::new(),
            finished: EvalAtFinished::Stopped,
            exit_code: None,
            condition_unsupported: None,
        };

        let value = serde_json::to_value(result).unwrap();
        assert_eq!(
            value["hits"][0]["evaluated"]["answer"],
            json!({ "value": "42", "type": "i32" })
        );
        assert_eq!(
            value["hits"][0]["evaluated"]["missing"],
            json!({ "error": "unknown identifier" })
        );
        assert!(serde_json::from_value::<EvalAtResult>(value).is_ok());

        let untyped = serde_json::to_value(EvalAtExpressionResult::Value {
            value: "opaque".to_owned(),
            r#type: None,
        })
        .unwrap();
        assert_eq!(untyped["value"], json!("opaque"));
        assert_optional_null_or_absent(&untyped, "type");
    }

    #[test]
    fn eval_at_hit_serializes_thread_id_frame_stack_locals_args_and_evaluated_map_keyed_by_expression_text()
     {
        let frame = DebugStackFrame {
            id: 7,
            name: "main".to_owned(),
            path: Some("src/main.rs".to_owned()),
            line: 12,
            column: 3,
        };
        let mut evaluated = BTreeMap::new();
        evaluated.insert(
            "answer + 1".to_owned(),
            EvalAtExpressionResult::Value {
                value: "43".to_owned(),
                r#type: Some("i32".to_owned()),
            },
        );
        let hit = EvalAtHit {
            thread_id: Some(1),
            frame: Some(frame.clone()),
            stack: vec![frame],
            locals: vec![CapturedVariable {
                name: "answer".to_owned(),
                value: "42".to_owned(),
                r#type: Some("i32".to_owned()),
                children: Vec::new(),
                truncated: false,
            }],
            args: vec![CapturedVariable {
                name: "argv".to_owned(),
                value: "[]".to_owned(),
                r#type: None,
                children: Vec::new(),
                truncated: false,
            }],
            evaluated,
        };

        let value = serde_json::to_value(hit).unwrap();
        assert_eq!(value["thread_id"], json!(1));
        assert_eq!(value["frame"]["name"], json!("main"));
        assert_eq!(value["stack"][0]["id"], json!(7));
        assert_eq!(value["locals"][0]["name"], json!("answer"));
        assert_eq!(value["args"][0]["name"], json!("argv"));
        assert_eq!(
            value["evaluated"],
            json!({
                "answer + 1": {
                    "value": "43",
                    "type": "i32"
                }
            })
        );
    }

    #[test]
    fn eval_at_result_serializes_hit_hits_output_finished_optional_exit_code_and_optional_condition_unsupported()
     {
        let result = EvalAtResult {
            hit: false,
            hits: Vec::new(),
            output: vec![DebugOutput {
                sequence: 1,
                category: Some("stdout".to_owned()),
                text: "done\n".to_owned(),
            }],
            finished: EvalAtFinished::Exited,
            exit_code: Some(0),
            condition_unsupported: Some(false),
        };

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({
                "hit": false,
                "hits": [],
                "output": [
                    {
                        "sequence": 1,
                        "category": "stdout",
                        "text": "done\n"
                    }
                ],
                "finished": "exited",
                "exit_code": 0,
                "condition_unsupported": false
            })
        );

        let no_optional_values = serde_json::to_value(EvalAtResult {
            hit: false,
            hits: Vec::new(),
            output: Vec::new(),
            finished: EvalAtFinished::Timeout,
            exit_code: None,
            condition_unsupported: None,
        })
        .unwrap();
        assert_optional_null_or_absent(&no_optional_values, "exit_code");
        assert_optional_null_or_absent(&no_optional_values, "condition_unsupported");
    }

    #[test]
    fn eval_at_result_never_serializes_a_session_id_field() {
        let result = EvalAtResult {
            hit: false,
            hits: Vec::new(),
            output: Vec::new(),
            finished: EvalAtFinished::Timeout,
            exit_code: None,
            condition_unsupported: None,
        };

        let value = serde_json::to_value(result).unwrap();
        assert_object_has_no_key_recursively(&value, "session_id");
    }

    #[test]
    fn eval_at_run_eval_at_launches_through_session_manager_and_sets_breakpoint_from_probe() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::single_hit()));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                breakpoint: Some(BreakpointProbe {
                    path: "src/main.rs".to_owned(),
                    line: 12,
                    condition: Some("answer == 42".to_owned()),
                }),
                timeout_ms: Some(250),
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at should launch and capture the first breakpoint hit");

        assert!(result.hit);
        assert_eq!(result.finished, EvalAtFinished::Stopped);
        assert_eq!(
            factory.starts.lock().unwrap().as_slice(),
            &[LaunchRequest {
                language: "rust".to_owned(),
                program: "target/debug/probe_fixture".to_owned(),
                cwd: Some("/workspace".to_owned()),
                args: vec!["--case".to_owned()],
                env: BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())]),
                launch_overrides: Map::new(),
            }]
        );
        let calls = factory.calls();
        let initial_breakpoints = FakeEvalAtCall::SetBreakpoints(vec![DebugBreakpoint {
            path: "src/main.rs".to_owned(),
            line: 12,
            condition: Some("answer == 42".to_owned()),
            hit_condition: None,
            verified: false,
            verified_id: None,
        }]);
        let initial_position = calls
            .iter()
            .position(|call| call == &initial_breakpoints)
            .expect("eval-at should set its breakpoint during launch");
        let configuration_position = calls
            .iter()
            .position(|call| call == &FakeEvalAtCall::SetBreakpoints(Vec::new()))
            .expect("eval-at launch should still complete DAP configuration");
        let continue_position = calls
            .iter()
            .position(|call| matches!(call, FakeEvalAtCall::Continue(_)))
            .expect("eval-at should continue after launch configuration");
        assert!(
            initial_position < configuration_position && configuration_position < continue_position,
            "eval-at must install breakpoints before configurationDone and continue; got {calls:?}"
        );
        assert!(
            factory
                .calls()
                .contains(&FakeEvalAtCall::Continue(Duration::from_millis(250)))
        );
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_missing_breakpoint_launches_continues_and_returns_exit_evidence_without_setting_breakpoints()
     {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::exited(0)));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                breakpoint: None,
                capture: CaptureOptions {
                    stack: false,
                    locals: false,
                    args: false,
                },
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at should report normal no-hit process exit");

        assert!(!result.hit);
        assert!(result.hits.is_empty());
        assert_eq!(result.finished, EvalAtFinished::Exited);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(factory.source_breakpoint_calls().len(), 0);
        assert!(
            factory
                .calls()
                .contains(&FakeEvalAtCall::Continue(Duration::from_secs(1)))
        );
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_first_hit_captures_exactly_one_stopped_hit_requested_evidence_and_terminates() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::single_hit()));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                expressions: vec!["answer + 1".to_owned()],
                on_hit: EvalAtHitMode::First,
                max_hits: 5,
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at should capture one hit and then terminate");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.finished, EvalAtFinished::Stopped);
        let hit = &result.hits[0];
        assert_eq!(hit.thread_id, Some(1));
        assert_eq!(hit.frame, Some(top_frame()));
        assert_eq!(hit.stack, vec![top_frame(), caller_frame()]);
        assert_eq!(hit.locals[0].name, "answer");
        assert_eq!(hit.args[0].name, "argv");
        assert_eq!(
            hit.evaluated.get("answer + 1"),
            Some(&EvalAtExpressionResult::Value {
                value: "43".to_owned(),
                r#type: Some("i32".to_owned()),
            })
        );
        assert_eq!(factory.continue_count(), 1);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_all_continues_after_each_hit_until_max_hits_is_reached() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::two_hits()));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                on_hit: EvalAtHitMode::All,
                max_hits: 2,
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at should collect hits until max_hits");

        assert!(result.hit);
        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.finished, EvalAtFinished::Stopped);
        assert_eq!(factory.continue_count(), 2);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_all_continues_after_hit_until_exit_evidence_is_observed() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::with_stops([
            Ok(stopped_hit()),
            Ok(exited(17)),
        ])));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                on_hit: EvalAtHitMode::All,
                max_hits: 5,
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at all mode should continue after a hit until process exit");

        assert!(result.hit);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.finished, EvalAtFinished::Exited);
        assert_eq!(result.exit_code, Some(17));
        assert_eq!(factory.continue_count(), 2);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_all_omitted_max_hits_defaults_to_one_captured_hit() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::two_hits()));
        let sessions = SessionManager::new(debug_config(), factory.clone());
        let request: EvalAtRequest = serde_json::from_value(json!({
            "lang": "rust",
            "program": "target/debug/probe_fixture",
            "args": ["--case"],
            "cwd": "/workspace",
            "env": { "RUST_LOG": "debug" },
            "breakpoint": { "path": "src/main.rs", "line": 12 },
            "on_hit": "all"
        }))
        .expect("request with omitted max_hits should deserialize using the default");

        let result = run_eval_at(request, &sessions)
            .expect("eval-at all mode should honor the default max_hits limit");

        assert!(result.hit);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.finished, EvalAtFinished::Stopped);
        assert_eq!(factory.continue_count(), 1);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_capture_stack_false_returns_empty_stack_while_preserving_top_frame_evidence() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::single_hit()));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                capture: CaptureOptions {
                    stack: false,
                    locals: true,
                    args: true,
                },
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at should capture top-frame evidence even when stack capture is disabled");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].frame, Some(top_frame()));
        assert!(result.hits[0].stack.is_empty());
        assert_eq!(result.hits[0].locals[0].name, "answer");
        assert_eq!(result.hits[0].args[0].name, "argv");
    }

    #[test]
    fn eval_at_capture_locals_false_args_false_returns_empty_lists_and_empty_expressions_still_capture_stack()
     {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::single_hit()));
        let sessions = SessionManager::new(debug_config(), factory);

        let result = run_eval_at(
            EvalAtRequest {
                expressions: Vec::new(),
                capture: CaptureOptions {
                    stack: true,
                    locals: false,
                    args: false,
                },
                ..base_request()
            },
            &sessions,
        )
        .expect("disabled locals/args should not prevent stack capture");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].frame, Some(top_frame()));
        assert_eq!(result.hits[0].stack, vec![top_frame(), caller_frame()]);
        assert!(result.hits[0].locals.is_empty());
        assert!(result.hits[0].args.is_empty());
        assert!(result.hits[0].evaluated.is_empty());
    }

    #[test]
    fn eval_at_variable_expansion_enforces_max_depth_and_marks_unexpanded_children_truncated() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::single_hit()));
        let sessions = SessionManager::new(debug_config(), factory);

        let result = run_eval_at(
            EvalAtRequest {
                max_depth: 1,
                max_children: 10,
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at should bound recursive variable expansion by max_depth");

        let root = result.hits[0]
            .locals
            .iter()
            .find(|variable| variable.name == "root")
            .expect("root variable should be captured");
        assert!(root.truncated);
        assert!(root.children.is_empty());
    }

    #[test]
    fn eval_at_variable_expansion_enforces_max_children_and_marks_partially_expanded_nodes_truncated()
     {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::single_hit()));
        let sessions = SessionManager::new(debug_config(), factory);

        let result = run_eval_at(
            EvalAtRequest {
                max_depth: 2,
                max_children: 1,
                ..base_request()
            },
            &sessions,
        )
        .expect("eval-at should bound variable expansion by max_children");

        let root = result.hits[0]
            .locals
            .iter()
            .find(|variable| variable.name == "root")
            .expect("root variable should be captured");
        assert_eq!(root.children.len(), 1);
        assert!(root.truncated);
    }

    #[test]
    fn eval_at_expression_failure_records_error_and_does_not_fail_the_whole_probe() {
        let factory = Arc::new(FakeEvalAtFactory::new(
            FakeEvalAtScenario::expression_error(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                expressions: vec!["answer".to_owned(), "missing".to_owned()],
                ..base_request()
            },
            &sessions,
        )
        .expect("per-expression failures should remain local to the hit evidence");

        assert_eq!(result.finished, EvalAtFinished::Stopped);
        assert_eq!(
            result.hits[0].evaluated.get("answer"),
            Some(&EvalAtExpressionResult::Value {
                value: "42".to_owned(),
                r#type: Some("i32".to_owned()),
            })
        );
        assert_eq!(
            result.hits[0].evaluated.get("missing"),
            Some(&EvalAtExpressionResult::Error {
                error: "unknown identifier: missing".to_owned(),
            })
        );
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_timeout_and_adapter_exit_paths_return_successful_finished_states_and_attempt_cleanup()
     {
        let timeout_factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::timeout()));
        let timeout_sessions = SessionManager::new(debug_config(), timeout_factory.clone());
        let timeout = run_eval_at(base_request(), &timeout_sessions)
            .expect("timeout is probe evidence, not a JSON-RPC failure");

        assert!(!timeout.hit);
        assert_eq!(timeout.finished, EvalAtFinished::Timeout);
        assert!(timeout_factory.calls().contains(&FakeEvalAtCall::Terminate));

        let exited_factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::adapter_exited()));
        let exited_sessions = SessionManager::new(debug_config(), exited_factory.clone());
        let exited = run_eval_at(base_request(), &exited_sessions)
            .expect("post-launch adapter exit should be mapped to successful probe evidence");

        assert!(!exited.hit);
        assert_eq!(exited.finished, EvalAtFinished::AdapterExited);
        assert!(exited_factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_launch_failure_before_session_exists_returns_debug_tool_error() {
        let factory = Arc::new(FakeEvalAtFactory::new(FakeEvalAtScenario::launch_failed()));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let error = run_eval_at(base_request(), &sessions)
            .expect_err("pre-session launch failure should remain a DebugToolError");

        assert_eq!(error.code, DebugToolErrorCode::InvalidParams);
        assert!(
            error
                .message
                .contains("fake launch failed before session id"),
            "unexpected launch failure message: {}",
            error.message
        );
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
        assert!(sessions.sessions().is_empty());
    }

    #[test]
    fn eval_at_post_launch_session_not_found_maps_to_adapter_exited_evidence_and_attempts_cleanup()
    {
        let factory = Arc::new(FakeEvalAtFactory::new(
            FakeEvalAtScenario::session_not_found_after_launch(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(base_request(), &sessions)
            .expect("post-launch missing session should be adapter-exit probe evidence");

        assert!(!result.hit);
        assert!(result.hits.is_empty());
        assert_eq!(result.finished, EvalAtFinished::AdapterExited);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
    }

    #[test]
    fn eval_at_cleanup_failure_returns_error_and_preserves_retryable_session_state() {
        let factory = Arc::new(FakeEvalAtFactory::new(
            FakeEvalAtScenario::terminate_fails_after_hit(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let error = run_eval_at(base_request(), &sessions)
            .expect_err("cleanup failure must be visible for a stateless eval-at probe");

        assert_eq!(error.code, DebugToolErrorCode::InvalidParams);
        assert!(error.message.contains("fake terminate failed"));
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
        assert_eq!(sessions.sessions().len(), 1);
    }

    #[test]
    fn eval_at_unsupported_breakpoint_conditions_set_condition_unsupported_in_result() {
        let factory = Arc::new(FakeEvalAtFactory::new(
            FakeEvalAtScenario::unsupported_condition(),
        ));
        let sessions = SessionManager::new(debug_config(), factory);

        let result = run_eval_at(
            EvalAtRequest {
                breakpoint: Some(BreakpointProbe {
                    path: "src/main.rs".to_owned(),
                    line: 12,
                    condition: Some("answer == 42".to_owned()),
                }),
                ..base_request()
            },
            &sessions,
        )
        .expect("unsupported breakpoint conditions should be represented in the probe result");

        assert_eq!(result.condition_unsupported, Some(true));
        assert_eq!(result.finished, EvalAtFinished::Stopped);
    }

    #[test]
    fn eval_at_set_breakpoints_runtime_error_attempts_cleanup_before_returning_error() {
        let factory = Arc::new(FakeEvalAtFactory::new(
            FakeEvalAtScenario::set_breakpoints_timeout(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let error = run_eval_at(base_request(), &sessions)
            .expect_err("setBreakpoints runtime failure should remain a tool error");

        assert_eq!(error.code, DebugToolErrorCode::DebugTimeout);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
        assert!(sessions.sessions().is_empty());
    }

    #[test]
    fn eval_at_capture_runtime_error_attempts_cleanup_before_returning_error() {
        let factory = Arc::new(FakeEvalAtFactory::new(
            FakeEvalAtScenario::variables_timeout(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let error = run_eval_at(base_request(), &sessions)
            .expect_err("capture runtime failure should remain a tool error");

        assert_eq!(error.code, DebugToolErrorCode::DebugTimeout);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
        assert!(sessions.sessions().is_empty());
    }

    #[test]
    fn eval_at_expression_adapter_exit_returns_adapter_exited_evidence_and_attempts_cleanup() {
        let factory = Arc::new(FakeEvalAtFactory::new(
            FakeEvalAtScenario::expression_adapter_exited(),
        ));
        let sessions = SessionManager::new(debug_config(), factory.clone());

        let result = run_eval_at(
            EvalAtRequest {
                expressions: vec!["adapter_crash".to_owned()],
                ..base_request()
            },
            &sessions,
        )
        .expect("adapter exit during expression evaluation should be probe evidence");

        assert!(!result.hit);
        assert!(result.hits.is_empty());
        assert_eq!(result.finished, EvalAtFinished::AdapterExited);
        assert!(factory.calls().contains(&FakeEvalAtCall::Terminate));
        assert!(sessions.sessions().is_empty());
    }

    fn assert_object_has_no_key_recursively(value: &Value, forbidden_key: &str) {
        match value {
            Value::Object(object) => {
                assert!(!object.contains_key(forbidden_key));
                for nested in object.values() {
                    assert_object_has_no_key_recursively(nested, forbidden_key);
                }
            }
            Value::Array(values) => {
                for nested in values {
                    assert_object_has_no_key_recursively(nested, forbidden_key);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    fn base_request() -> EvalAtRequest {
        EvalAtRequest {
            lang: "rust".to_owned(),
            program: "target/debug/probe_fixture".to_owned(),
            args: vec!["--case".to_owned()],
            cwd: Some("/workspace".to_owned()),
            env: BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())]),
            breakpoint: Some(BreakpointProbe {
                path: "src/main.rs".to_owned(),
                line: 12,
                condition: None,
            }),
            expressions: Vec::new(),
            capture: CaptureOptions::default(),
            on_hit: EvalAtHitMode::First,
            max_hits: 1,
            max_depth: 2,
            max_children: 50,
            timeout_ms: None,
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

    fn caller_frame() -> DebugStackFrame {
        DebugStackFrame {
            id: 11,
            name: "caller".to_owned(),
            path: Some("src/lib.rs".to_owned()),
            line: 30,
            column: 9,
        }
    }

    fn stopped_hit() -> DebugStop {
        DebugStop {
            state: DebugSessionState::Stopped,
            reason: Some("breakpoint".to_owned()),
            thread_id: Some(1),
            top_frame: Some(top_frame()),
            hit_breakpoint_ids: vec![1],
            timed_out: false,
            exit_code: None,
            output_since: vec![DebugOutput {
                sequence: 1,
                category: Some("stdout".to_owned()),
                text: "hit\n".to_owned(),
            }],
        }
    }

    fn exited(exit_code: i64) -> DebugStop {
        DebugStop {
            state: DebugSessionState::Terminated,
            reason: Some("exited".to_owned()),
            thread_id: None,
            top_frame: None,
            hit_breakpoint_ids: Vec::new(),
            timed_out: false,
            exit_code: Some(exit_code),
            output_since: vec![DebugOutput {
                sequence: 1,
                category: Some("stdout".to_owned()),
                text: "done\n".to_owned(),
            }],
        }
    }

    #[derive(Clone)]
    struct FakeEvalAtFactory {
        scenario: FakeEvalAtScenario,
        starts: Arc<Mutex<Vec<LaunchRequest>>>,
        calls: Arc<Mutex<Vec<FakeEvalAtCall>>>,
    }

    impl FakeEvalAtFactory {
        fn new(scenario: FakeEvalAtScenario) -> Self {
            Self {
                scenario,
                starts: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<FakeEvalAtCall> {
            self.calls.lock().unwrap().clone()
        }

        fn continue_count(&self) -> usize {
            self.calls()
                .iter()
                .filter(|call| matches!(call, FakeEvalAtCall::Continue(_)))
                .count()
        }

        fn source_breakpoint_calls(&self) -> Vec<Vec<DebugBreakpoint>> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    FakeEvalAtCall::SetBreakpoints(breakpoints) if !breakpoints.is_empty() => {
                        Some(breakpoints)
                    }
                    _ => None,
                })
                .collect()
        }
    }

    impl DebugAdapterFactory for FakeEvalAtFactory {
        fn start(
            &self,
            request: &LaunchRequest,
        ) -> Result<Box<dyn DebugAdapterSession>, DebugRuntimeError> {
            self.starts.lock().unwrap().push(request.clone());
            Ok(Box::new(FakeEvalAtSession {
                scenario: self.scenario.clone(),
                calls: self.calls.clone(),
            }))
        }
    }

    #[derive(Clone)]
    struct FakeEvalAtScenario {
        stops: Arc<Mutex<VecDeque<Result<DebugStop, DebugRuntimeError>>>>,
        expression_errors: Vec<String>,
        expression_adapter_exits: Vec<String>,
        set_breakpoints_error: Option<DebugRuntimeError>,
        variables_error: Option<DebugRuntimeError>,
        unsupported_condition: bool,
        launch_error: Option<String>,
        terminate_error: Option<String>,
    }

    impl FakeEvalAtScenario {
        fn single_hit() -> Self {
            Self::with_stops([Ok(stopped_hit())])
        }

        fn two_hits() -> Self {
            Self::with_stops([Ok(stopped_hit()), Ok(stopped_hit())])
        }

        fn exited(exit_code: i64) -> Self {
            Self::with_stops([Ok(exited(exit_code))])
        }

        fn timeout() -> Self {
            Self::with_stops([Err(DebugRuntimeError::DebugTimeout(
                "timed out waiting for debug stop".to_owned(),
            ))])
        }

        fn adapter_exited() -> Self {
            Self::with_stops([Err(DebugRuntimeError::AdapterExited(
                "adapter exited after launch".to_owned(),
            ))])
        }

        fn session_not_found_after_launch() -> Self {
            Self::with_stops([Err(DebugRuntimeError::SessionNotFound(
                "debug session disappeared after adapter exit".to_owned(),
            ))])
        }

        fn launch_failed() -> Self {
            Self {
                launch_error: Some("fake launch failed before session id".to_owned()),
                ..Self::single_hit()
            }
        }

        fn terminate_fails_after_hit() -> Self {
            Self {
                terminate_error: Some("fake terminate failed; retry remains possible".to_owned()),
                ..Self::single_hit()
            }
        }

        fn expression_error() -> Self {
            Self {
                expression_errors: vec!["missing".to_owned()],
                ..Self::single_hit()
            }
        }

        fn expression_adapter_exited() -> Self {
            Self {
                expression_adapter_exits: vec!["adapter_crash".to_owned()],
                ..Self::single_hit()
            }
        }

        fn set_breakpoints_timeout() -> Self {
            Self {
                set_breakpoints_error: Some(DebugRuntimeError::DebugTimeout(
                    "fake setBreakpoints timed out".to_owned(),
                )),
                ..Self::single_hit()
            }
        }

        fn variables_timeout() -> Self {
            Self {
                variables_error: Some(DebugRuntimeError::DebugTimeout(
                    "fake variables timed out".to_owned(),
                )),
                ..Self::single_hit()
            }
        }

        fn unsupported_condition() -> Self {
            Self {
                unsupported_condition: true,
                ..Self::single_hit()
            }
        }

        fn with_stops<const N: usize>(stops: [Result<DebugStop, DebugRuntimeError>; N]) -> Self {
            Self {
                stops: Arc::new(Mutex::new(VecDeque::from(stops))),
                expression_errors: Vec::new(),
                expression_adapter_exits: Vec::new(),
                set_breakpoints_error: None,
                variables_error: None,
                unsupported_condition: false,
                launch_error: None,
                terminate_error: None,
            }
        }
    }

    struct FakeEvalAtSession {
        scenario: FakeEvalAtScenario,
        calls: Arc<Mutex<Vec<FakeEvalAtCall>>>,
    }

    impl DebugAdapterSession for FakeEvalAtSession {
        fn initialize(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeEvalAtCall::Initialize);
            Ok(())
        }

        fn launch(
            &mut self,
            _request: &LaunchRequest,
            _timeout: Duration,
        ) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeEvalAtCall::Launch);
            if let Some(message) = &self.scenario.launch_error {
                return Err(DebugRuntimeError::LaunchFailed(message.clone()));
            }
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
                .push(FakeEvalAtCall::SetBreakpoints(breakpoints.to_vec()));
            if let Some(error) = &self.scenario.set_breakpoints_error {
                return Err(error.clone());
            }
            Ok(breakpoints
                .iter()
                .map(|breakpoint| DebugBreakpoint {
                    verified: !self.scenario.unsupported_condition,
                    verified_id: (!self.scenario.unsupported_condition).then_some(1),
                    ..breakpoint.clone()
                })
                .collect())
        }

        fn continue_session(&mut self, timeout: Duration) -> Result<DebugStop, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeEvalAtCall::Continue(timeout));
            self.scenario
                .stops
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(exited(0)))
        }

        fn step(
            &mut self,
            _thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            Ok(stopped_hit())
        }

        fn pause(
            &mut self,
            _thread_id: Option<u64>,
            _timeout: Duration,
        ) -> Result<DebugStop, DebugRuntimeError> {
            Ok(stopped_hit())
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
                .push(FakeEvalAtCall::Stack(thread_id));
            Ok(vec![top_frame(), caller_frame()])
        }

        fn scopes(
            &mut self,
            frame_id: u64,
            _timeout: Duration,
        ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
            self.calls
                .lock()
                .unwrap()
                .push(FakeEvalAtCall::Scopes(frame_id));
            Ok(vec![
                DebugScope {
                    name: "Locals".to_owned(),
                    variables_reference: 100,
                    expensive: false,
                },
                DebugScope {
                    name: "Arguments".to_owned(),
                    variables_reference: 200,
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
                .push(FakeEvalAtCall::Variables(variables_reference));
            if let Some(error) = &self.scenario.variables_error {
                return Err(error.clone());
            }
            Ok(match variables_reference {
                100 => vec![
                    DebugVariable {
                        name: "answer".to_owned(),
                        value: "42".to_owned(),
                        r#type: Some("i32".to_owned()),
                        variables_reference: 0,
                    },
                    DebugVariable {
                        name: "root".to_owned(),
                        value: "{a,b}".to_owned(),
                        r#type: Some("Root".to_owned()),
                        variables_reference: 300,
                    },
                ],
                200 => vec![DebugVariable {
                    name: "argv".to_owned(),
                    value: "[--case]".to_owned(),
                    r#type: Some("Vec<String>".to_owned()),
                    variables_reference: 0,
                }],
                300 => vec![
                    DebugVariable {
                        name: "a".to_owned(),
                        value: "1".to_owned(),
                        r#type: Some("i32".to_owned()),
                        variables_reference: 0,
                    },
                    DebugVariable {
                        name: "b".to_owned(),
                        value: "2".to_owned(),
                        r#type: Some("i32".to_owned()),
                        variables_reference: 0,
                    },
                ],
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
                .push(FakeEvalAtCall::Evaluate(frame_id, expression.to_owned()));
            if self
                .scenario
                .expression_errors
                .iter()
                .any(|error_expression| error_expression == expression)
            {
                return Err(DebugRuntimeError::LaunchFailed(format!(
                    "unknown identifier: {expression}"
                )));
            }
            if self
                .scenario
                .expression_adapter_exits
                .iter()
                .any(|error_expression| error_expression == expression)
            {
                return Err(DebugRuntimeError::AdapterExited(format!(
                    "adapter exited while evaluating {expression}"
                )));
            }
            Ok(DebugVariable {
                name: expression.to_owned(),
                value: if expression == "answer + 1" {
                    "43"
                } else {
                    "42"
                }
                .to_owned(),
                r#type: Some("i32".to_owned()),
                variables_reference: 0,
            })
        }

        fn terminate(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeEvalAtCall::Terminate);
            if let Some(message) = &self.scenario.terminate_error {
                return Err(DebugRuntimeError::AdapterExited(message.clone()));
            }
            Ok(())
        }

        fn disconnect(&mut self, _timeout: Duration) -> Result<(), DebugRuntimeError> {
            self.calls.lock().unwrap().push(FakeEvalAtCall::Disconnect);
            Ok(())
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeEvalAtCall {
        Initialize,
        Launch,
        SetBreakpoints(Vec<DebugBreakpoint>),
        Continue(Duration),
        Stack(u64),
        Scopes(u64),
        Variables(u64),
        Evaluate(u64, String),
        Terminate,
        Disconnect,
    }

    fn assert_optional_null_or_absent(value: &Value, key: &str) {
        let object = value.as_object().unwrap();
        if let Some(optional_value) = object.get(key) {
            assert_eq!(optional_value, &Value::Null);
        }
    }
}
