#![allow(clippy::pedantic)]

#[path = "../src/protocol.rs"]
mod protocol;

use protocol::{CheckRequest, CheckResult, LintDiagnosticDto, LintToolErrorResponse, QueuedFrame};
use serde_json::json;

#[test]
fn protocol_rs_defines_the_lint_tool_dtos_and_skips_absent_error() {
    let request = CheckRequest {
        path: Some("src/main.rs".to_owned()),
    };
    assert_eq!(
        serde_json::to_value(&request).expect("serialize request"),
        json!({ "path": "src/main.rs" })
    );

    let result = CheckResult {
        supported: false,
        diagnostics: Vec::new(),
        error: None,
    };
    assert_eq!(
        serde_json::to_value(&result).expect("serialize result"),
        json!({ "supported": false, "diagnostics": [] })
    );

    let result = CheckResult {
        supported: true,
        diagnostics: vec![LintDiagnosticDto {
            path: "src/lib.rs".to_owned(),
            line: 3,
            character: 7,
            end_line: 3,
            end_character: 16,
            severity: "warning".to_owned(),
            code: Some("clippy::len_zero".to_owned()),
            message: "use `is_empty`".to_owned(),
            source: Some("rustc".to_owned()),
        }],
        error: None,
    };
    assert_eq!(
        serde_json::to_value(&result).expect("serialize diagnostic-only result"),
        json!({
            "supported": true,
            "diagnostics": [{
                "path": "src/lib.rs",
                "line": 3,
                "character": 7,
                "endLine": 3,
                "endCharacter": 16,
                "severity": "warning",
                "code": "clippy::len_zero",
                "message": "use `is_empty`",
                "source": "rustc"
            }]
        })
    );

    let result = CheckResult {
        supported: false,
        diagnostics: Vec::new(),
        error: Some(LintToolErrorResponse {
            code: "lint_missing_binary".to_owned(),
            message: "lint command is unavailable".to_owned(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&result).expect("serialize result with error"),
        json!({
            "supported": false,
            "diagnostics": [],
            "error": {
                "code": "lint_missing_binary",
                "message": "lint command is unavailable"
            }
        })
    );
}

#[test]
fn protocol_rs_defines_lint_diagnostic_dto_matching_shared_json_contract_with_path() {
    let diagnostic = LintDiagnosticDto {
        path: "src/main.rs".to_owned(),
        line: 4,
        character: 8,
        end_line: 4,
        end_character: 12,
        severity: "warning".to_owned(),
        code: Some("lint/warn".to_owned()),
        message: "prefer a clearer name".to_owned(),
        source: Some("example-lint".to_owned()),
    };

    assert_eq!(
        serde_json::to_value(&diagnostic).expect("serialize diagnostic"),
        json!({
            "path": "src/main.rs",
            "line": 4,
            "character": 8,
            "endLine": 4,
            "endCharacter": 12,
            "severity": "warning",
            "code": "lint/warn",
            "message": "prefer a clearer name",
            "source": "example-lint"
        })
    );

    let without_optional = LintDiagnosticDto {
        path: "src/lib.rs".to_owned(),
        line: 1,
        character: 2,
        end_line: 1,
        end_character: 3,
        severity: "error".to_owned(),
        code: None,
        message: "broken".to_owned(),
        source: None,
    };

    assert_eq!(
        serde_json::to_value(&without_optional).expect("serialize diagnostic"),
        json!({
            "path": "src/lib.rs",
            "line": 1,
            "character": 2,
            "endLine": 1,
            "endCharacter": 3,
            "severity": "error",
            "message": "broken"
        })
    );
}

#[test]
fn protocol_rs_defines_queued_frame_for_inbound_host_requests_and_notifications() {
    let request = QueuedFrame::Request {
        id: Some(json!(42)),
        method: "invokeTool".to_owned(),
        params: json!({ "name": "check", "params": {} }),
    };
    let notification = QueuedFrame::Notification {
        method: "telemetry/event".to_owned(),
        params: json!({ "ok": true }),
    };

    assert_eq!(
        serde_json::to_value(&request).expect("serialize request frame"),
        json!({
            "type": "Request",
            "id": 42,
            "method": "invokeTool",
            "params": { "name": "check", "params": {} }
        })
    );
    assert_eq!(
        serde_json::to_value(&notification).expect("serialize notification frame"),
        json!({
            "type": "Notification",
            "method": "telemetry/event",
            "params": { "ok": true }
        })
    );
}
