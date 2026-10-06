use extension_protocol::{
    LspImplementationResult, LspOperation, LspOutcome, LspOutcomeCode, LspOutcomePhase,
    LspOutcomeStatus, ProtocolError, RenameError,
};
use serde_json::{Value, json};

fn runtime_outcome() -> LspOutcome {
    LspOutcome {
        status: LspOutcomeStatus::Error,
        code: LspOutcomeCode::ServerTimeout,
        language: Some("rust".into()),
        command: Some("rust-analyzer".into()),
        operation: Some(LspOperation::Definition),
        path: Some("src/lib.rs".into()),
        phase: LspOutcomePhase::Runtime,
        message: "language server request timed out".into(),
    }
}

#[test]
fn exports_lsp_outcome_family_with_required_fields_variants_and_wire_spellings() {
    let value = serde_json::to_value(runtime_outcome()).expect("serialize LSP outcome");

    assert_eq!(
        value,
        json!({
            "status": "error",
            "code": "server_timeout",
            "language": "rust",
            "command": "rust-analyzer",
            "operation": "definition",
            "path": "src/lib.rs",
            "phase": "runtime",
            "message": "language server request timed out"
        })
    );
    assert_eq!(
        serde_json::to_value(LspOutcomeStatus::Unsupported).expect("serialize status"),
        json!("unsupported")
    );
    assert_eq!(
        serde_json::to_value(LspOutcomePhase::Startup).expect("serialize phase"),
        json!("startup")
    );
}

#[test]
fn covers_eleven_outcome_codes_six_operations_required_fields_and_explicit_nulls() {
    let codes = [
        (
            LspOutcomeCode::LanguageNotConfigured,
            "language_not_configured",
        ),
        (
            LspOutcomeCode::CapabilityUnavailable,
            "capability_unavailable",
        ),
        (LspOutcomeCode::ServerMissing, "server_missing"),
        (LspOutcomeCode::InvalidCommand, "invalid_command"),
        (LspOutcomeCode::ServerNotExecutable, "server_not_executable"),
        (LspOutcomeCode::ServerLaunchFailed, "server_launch_failed"),
        (LspOutcomeCode::ServerCrashed, "server_crashed"),
        (LspOutcomeCode::ServerTimeout, "server_timeout"),
        (
            LspOutcomeCode::ServerTransportError,
            "server_transport_error",
        ),
        (
            LspOutcomeCode::ServerMalformedResponse,
            "server_malformed_response",
        ),
        (LspOutcomeCode::ServerError, "server_error"),
    ];
    for (code, wire) in codes {
        assert_eq!(
            serde_json::to_value(code).expect("serialize code"),
            json!(wire)
        );
        assert_eq!(
            serde_json::from_value::<LspOutcomeCode>(json!(wire)).expect("deserialize code"),
            code
        );
    }

    let operations = [
        (LspOperation::Diagnostics, "diagnostics"),
        (LspOperation::Definition, "definition"),
        (LspOperation::References, "references"),
        (LspOperation::Hover, "hover"),
        (LspOperation::Implementations, "implementations"),
        (LspOperation::Rename, "rename"),
    ];
    for (operation, wire) in operations {
        assert_eq!(
            serde_json::to_value(operation).expect("serialize operation"),
            json!(wire)
        );
        assert_eq!(
            serde_json::from_value::<LspOperation>(json!(wire)).expect("deserialize operation"),
            operation
        );
    }

    let null_value = json!({
        "status": "error",
        "code": "server_missing",
        "language": null,
        "command": null,
        "operation": null,
        "path": null,
        "phase": "startup",
        "message": "configured language server is missing"
    });
    let decoded: LspOutcome =
        serde_json::from_value(null_value.clone()).expect("deserialize explicit null fields");
    assert_eq!(
        serde_json::to_value(decoded).expect("serialize explicit null fields"),
        null_value
    );

    let complete = serde_json::to_value(runtime_outcome()).expect("serialize complete outcome");
    for field in [
        "status",
        "code",
        "language",
        "command",
        "operation",
        "path",
        "phase",
        "message",
    ] {
        let mut missing = complete.clone();
        missing
            .as_object_mut()
            .expect("outcome serializes as an object")
            .remove(field);
        assert!(
            serde_json::from_value::<LspOutcome>(missing).is_err(),
            "missing required field {field} must be rejected"
        );
    }

    let mut additive = complete;
    additive
        .as_object_mut()
        .expect("outcome serializes as an object")
        .insert("consumer_metadata".into(), Value::Bool(true));
    assert_eq!(
        serde_json::from_value::<LspOutcome>(additive).expect("ignore additive consumer field"),
        runtime_outcome()
    );
}

#[test]
fn registers_lsp_outcome_contracts_without_disabling_cargo_test_auto_discovery() {
    let manifest = include_str!("../Cargo.toml");

    assert!(manifest.contains("name = \"lsp_outcome_contracts\""));
    assert!(manifest.contains("path = \"tests/lsp_outcome_contracts.rs\""));
    assert!(!manifest.contains("autotests = false"));
}

#[test]
fn retained_results_add_optional_outcomes_for_unsupported_and_error_responses() {
    let outcome = runtime_outcome();
    let implementation: LspImplementationResult = serde_json::from_value(json!({
        "supported": false,
        "locations": [],
        "outcome": outcome
    }))
    .expect("deserialize implementation result with outcome");
    let rename: RenameError = serde_json::from_value(json!({
        "code": "backend_error",
        "message": "language server request timed out",
        "path": "src/lib.rs",
        "outcome": runtime_outcome()
    }))
    .expect("deserialize rename error with outcome");

    assert_eq!(
        serde_json::to_value(&implementation).expect("serialize implementation result")["outcome"],
        serde_json::to_value(runtime_outcome()).expect("serialize outcome")
    );
    assert_eq!(
        serde_json::to_value(&rename).expect("serialize rename error")["outcome"],
        serde_json::to_value(runtime_outcome()).expect("serialize outcome")
    );
}

#[test]
fn retained_results_tolerate_unknown_fields_and_default_absent_outcomes() {
    let implementation: LspImplementationResult = serde_json::from_value(json!({
        "supported": true,
        "locations": [],
        "future_metadata": true
    }))
    .expect("deserialize legacy implementation result with additive field");
    let rename: RenameError = serde_json::from_value(json!({
        "code": "not_renameable",
        "message": "not renameable",
        "path": "src/lib.rs",
        "future_metadata": true
    }))
    .expect("deserialize legacy rename error with additive field");

    assert!(
        serde_json::to_value(implementation).expect("serialize implementation result")["outcome"]
            .is_null()
    );
    assert!(serde_json::to_value(rename).expect("serialize rename error")["outcome"].is_null());
}

#[test]
fn retained_success_and_legacy_rename_error_serialization_remain_pre_f008_compatible() {
    let implementation: LspImplementationResult = serde_json::from_value(json!({
        "supported": true,
        "locations": [{
            "path": "src/lib.rs",
            "line": 4,
            "character": 12,
            "endLine": 4,
            "endCharacter": 20
        }]
    }))
    .expect("deserialize legacy implementation result");
    let rename: RenameError = serde_json::from_value(json!({
        "code": "not_renameable",
        "message": "not renameable",
        "path": "src/lib.rs"
    }))
    .expect("deserialize legacy rename error");

    assert_eq!(
        serde_json::to_value(implementation).expect("serialize implementation result"),
        json!({
            "supported": true,
            "locations": [{
                "path": "src/lib.rs",
                "line": 4,
                "character": 12,
                "endLine": 4,
                "endCharacter": 20
            }]
        })
    );
    assert_eq!(
        serde_json::to_value(rename).expect("serialize rename error"),
        json!({
            "code": "not_renameable",
            "message": "not renameable",
            "path": "src/lib.rs"
        })
    );
}

#[test]
fn protocol_error_data_round_trips_an_lsp_outcome() {
    let outcome = runtime_outcome();
    let error = ProtocolError {
        code: -32603,
        message: "language server request failed".into(),
        data: Some(json!({ "outcome": outcome })),
    };

    let decoded: ProtocolError = serde_json::from_value(
        serde_json::to_value(&error).expect("serialize protocol error with outcome"),
    )
    .expect("deserialize protocol error with outcome");

    assert_eq!(decoded, error);
    assert_eq!(
        serde_json::from_value::<LspOutcome>(decoded.data.expect("error data")["outcome"].clone())
            .expect("deserialize outcome from error data"),
        runtime_outcome()
    );
}
