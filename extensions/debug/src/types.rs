#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DebugSessionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DebugSessionState {
    Initializing,
    Stopped,
    Running,
    Terminated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugBreakpoint {
    pub path: String,
    pub line: u64,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub verified: bool,
    pub verified_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugStop {
    pub state: DebugSessionState,
    pub reason: Option<String>,
    pub thread_id: Option<u64>,
    pub top_frame: Option<DebugStackFrame>,
    pub hit_breakpoint_ids: Vec<u64>,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub output_since: Vec<DebugOutput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugStackFrame {
    pub id: u64,
    pub name: String,
    pub path: Option<String>,
    pub line: u64,
    pub column: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugThread {
    pub id: u64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugScope {
    pub name: String,
    pub variables_reference: u64,
    pub expensive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugVariable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    pub variables_reference: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugOutput {
    pub sequence: u64,
    pub category: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeFailureResult {
    pub ok: bool,
    pub error: RuntimeFailure,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeFailure {
    pub code: String,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", content = "message")]
pub enum DebugRuntimeError {
    #[serde(rename = "session-not-found")]
    SessionNotFound(String),
    #[serde(rename = "not-stopped")]
    NotStopped(String),
    #[serde(rename = "debug-timeout")]
    DebugTimeout(String),
    #[serde(rename = "adapter-exited")]
    AdapterExited(String),
    #[serde(rename = "launch-failed")]
    LaunchFailed(String),
    #[serde(rename = "reverse_unsupported")]
    ReverseUnsupported(String),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        DebugBreakpoint, DebugOutput, DebugRuntimeError, DebugScope, DebugSessionId,
        DebugSessionState, DebugStackFrame, DebugStop, DebugThread, DebugVariable,
    };

    #[test]
    fn publicly_defines_debug_runtime_dtos_as_serde_tool_facing_json_shape() {
        let session_id = DebugSessionId("debug-1".to_owned());
        let state = DebugSessionState::Stopped;
        let breakpoint = DebugBreakpoint {
            path: "src/main.rs".to_owned(),
            line: 42,
            condition: Some("x > 1".to_owned()),
            hit_condition: Some("3".to_owned()),
            verified: true,
            verified_id: Some(9),
        };
        let frame = DebugStackFrame {
            id: 7,
            name: "main".to_owned(),
            path: Some("src/main.rs".to_owned()),
            line: 42,
            column: 5,
        };
        let stop = DebugStop {
            state: state.clone(),
            reason: Some("breakpoint".to_owned()),
            thread_id: Some(1),
            top_frame: Some(frame.clone()),
            hit_breakpoint_ids: vec![9],
            timed_out: false,
            exit_code: None,
            output_since: vec![DebugOutput {
                sequence: 1,
                category: Some("stdout".to_owned()),
                text: "hello\n".to_owned(),
            }],
        };
        let thread = DebugThread {
            id: 1,
            name: "main thread".to_owned(),
        };
        let scope = DebugScope {
            name: "Locals".to_owned(),
            variables_reference: 100,
            expensive: false,
        };
        let variable = DebugVariable {
            name: "answer".to_owned(),
            value: "42".to_owned(),
            r#type: Some("i32".to_owned()),
            variables_reference: 0,
        };

        assert_eq!(serde_json::to_value(&session_id).unwrap(), json!("debug-1"));
        assert_eq!(serde_json::to_value(&state).unwrap(), json!("stopped"));
        assert_eq!(
            serde_json::to_value(&breakpoint).unwrap(),
            json!({
                "path": "src/main.rs",
                "line": 42,
                "condition": "x > 1",
                "hit_condition": "3",
                "verified": true,
                "verified_id": 9
            })
        );
        assert_eq!(
            serde_json::to_value(&stop).unwrap(),
            json!({
                "state": "stopped",
                "reason": "breakpoint",
                "thread_id": 1,
                "top_frame": {
                    "id": 7,
                    "name": "main",
                    "path": "src/main.rs",
                    "line": 42,
                    "column": 5
                },
                "hit_breakpoint_ids": [9],
                "timed_out": false,
                "output_since": [
                    { "sequence": 1, "category": "stdout", "text": "hello\n" }
                ]
            })
        );
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            json!({
                "id": 7,
                "name": "main",
                "path": "src/main.rs",
                "line": 42,
                "column": 5
            })
        );
        assert_eq!(
            serde_json::to_value(&thread).unwrap(),
            json!({ "id": 1, "name": "main thread" })
        );
        assert_eq!(
            serde_json::to_value(&scope).unwrap(),
            json!({ "name": "Locals", "variables_reference": 100, "expensive": false })
        );
        assert_eq!(
            serde_json::to_value(&variable).unwrap(),
            json!({
                "name": "answer",
                "value": "42",
                "type": "i32",
                "variables_reference": 0
            })
        );
    }

    #[test]
    fn debug_runtime_error_serializes_stable_codes() {
        let cases = [
            (
                DebugRuntimeError::SessionNotFound("missing".to_owned()),
                json!({ "code": "session-not-found", "message": "missing" }),
            ),
            (
                DebugRuntimeError::NotStopped("running".to_owned()),
                json!({ "code": "not-stopped", "message": "running" }),
            ),
            (
                DebugRuntimeError::DebugTimeout("timeout".to_owned()),
                json!({ "code": "debug-timeout", "message": "timeout" }),
            ),
            (
                DebugRuntimeError::AdapterExited("exited".to_owned()),
                json!({ "code": "adapter-exited", "message": "exited" }),
            ),
            (
                DebugRuntimeError::LaunchFailed("launch failed".to_owned()),
                json!({ "code": "launch-failed", "message": "launch failed" }),
            ),
            (
                DebugRuntimeError::ReverseUnsupported("reverse_unsupported".to_owned()),
                json!({ "code": "reverse_unsupported", "message": "reverse_unsupported" }),
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(serde_json::to_value(error).unwrap(), expected);
        }
    }

    #[test]
    fn debug_stop_includes_an_optional_exit_code_field_serialized_only_when_present() {
        let absent = DebugStop {
            state: DebugSessionState::Terminated,
            reason: Some("terminated".to_owned()),
            thread_id: None,
            top_frame: None,
            hit_breakpoint_ids: Vec::new(),
            timed_out: false,
            exit_code: None,
            output_since: Vec::new(),
        };
        let present = DebugStop {
            exit_code: Some(7),
            ..absent.clone()
        };

        let absent = serde_json::to_value(absent).unwrap();
        let present = serde_json::to_value(present).unwrap();

        assert!(absent.get("exit_code").is_none());
        assert_eq!(present["exit_code"], 7);
    }

    #[test]
    fn existing_debug_stop_serialization_tests_continue_to_pass_when_exit_code_is_absent() {
        let stop = DebugStop {
            state: DebugSessionState::Stopped,
            reason: Some("breakpoint".to_owned()),
            thread_id: Some(1),
            top_frame: None,
            hit_breakpoint_ids: vec![9],
            timed_out: false,
            exit_code: None,
            output_since: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(stop).unwrap(),
            json!({
                "state": "stopped",
                "reason": "breakpoint",
                "thread_id": 1,
                "top_frame": null,
                "hit_breakpoint_ids": [9],
                "timed_out": false,
                "output_since": []
            })
        );
    }

    #[test]
    fn debug_stop_serialization_includes_exit_code_0_for_a_normal_exited_outcome_with_code_zero() {
        let stop = DebugStop {
            state: DebugSessionState::Terminated,
            reason: Some("exited".to_owned()),
            thread_id: None,
            top_frame: None,
            hit_breakpoint_ids: Vec::new(),
            timed_out: false,
            exit_code: Some(0),
            output_since: Vec::new(),
        };

        assert_eq!(serde_json::to_value(stop).unwrap()["exit_code"], 0);
    }
}
