use extension_protocol::{
    LspImplementationResult, LspOperation, LspOutcome, LspOutcomeCode, LspOutcomePhase,
    LspOutcomeStatus, ProtocolError, RenameError, TypeHierarchyDirection, TypeHierarchyItem,
    TypeHierarchyQuery, TypeHierarchyRange, TypeHierarchyResult,
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

fn hierarchy_range(line: u32, character: u32, end_character: u32) -> TypeHierarchyRange {
    TypeHierarchyRange {
        line,
        character,
        end_line: line,
        end_character,
    }
}

fn workspace_hierarchy_item() -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: "Child".into(),
        kind: "class".into(),
        detail: Some("crate::Child".into()),
        range: hierarchy_range(8, 0, 18),
        selection_range: hierarchy_range(8, 7, 12),
        path: Some("src/lib.rs".into()),
        external_uri: None,
        external: false,
    }
}

#[test]
fn type_hierarchy_direction_variants_use_exact_wire_spellings() {
    let cases = [
        (TypeHierarchyDirection::Supertypes, "supertypes"),
        (TypeHierarchyDirection::Subtypes, "subtypes"),
        (TypeHierarchyDirection::Both, "both"),
    ];

    for (direction, wire) in cases {
        assert_eq!(serde_json::to_value(direction).unwrap(), json!(wire));
        assert_eq!(
            serde_json::from_value::<TypeHierarchyDirection>(json!(wire)).unwrap(),
            direction
        );
    }
}

#[test]
fn type_hierarchy_query_requires_path_utf16_coordinates_and_direction() {
    let query = TypeHierarchyQuery {
        path: "src/lib.rs".into(),
        line: 4,
        character: 9,
        direction: TypeHierarchyDirection::Both,
    };
    let value = serde_json::to_value(&query).unwrap();

    assert_eq!(
        value,
        json!({
            "path": "src/lib.rs",
            "line": 4,
            "character": 9,
            "direction": "both"
        })
    );
    assert_eq!(
        serde_json::from_value::<TypeHierarchyQuery>(value).unwrap(),
        query
    );
    assert!(
        serde_json::from_value::<TypeHierarchyQuery>(json!({
            "path": "src/lib.rs",
            "line": 4,
            "direction": "both"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<TypeHierarchyQuery>(json!({
            "path": "src/lib.rs",
            "line": "four",
            "character": 9,
            "direction": "both"
        }))
        .is_err()
    );
}

#[test]
fn type_hierarchy_range_uses_the_shared_coordinate_wire_contract() {
    let range = hierarchy_range(3, 2, 11);
    let value = serde_json::to_value(&range).unwrap();

    assert_eq!(
        value,
        json!({
            "line": 3,
            "character": 2,
            "endLine": 3,
            "endCharacter": 11
        })
    );
    assert_eq!(
        serde_json::from_value::<TypeHierarchyRange>(value).unwrap(),
        range
    );
}

#[test]
fn type_hierarchy_item_serializes_exactly_one_workspace_or_external_location() {
    let workspace = workspace_hierarchy_item();
    let external: TypeHierarchyItem = serde_json::from_value(json!({
        "name": "ExternalBase",
        "kind": "interface",
        "detail": null,
        "range": {"line": 1, "character": 0, "endLine": 1, "endCharacter": 12},
        "selectionRange": {"line": 1, "character": 0, "endLine": 1, "endCharacter": 12},
        "path": null,
        "externalUri": "https://example.test/types#ExternalBase",
        "external": true
    }))
    .expect("deserialize an external hierarchy item");

    assert_eq!(
        serde_json::to_value(&workspace).unwrap(),
        json!({
            "name": "Child",
            "kind": "class",
            "detail": "crate::Child",
            "range": {"line": 8, "character": 0, "endLine": 8, "endCharacter": 18},
            "selectionRange": {"line": 8, "character": 7, "endLine": 8, "endCharacter": 12},
            "path": "src/lib.rs",
            "externalUri": null,
            "external": false
        })
    );
    assert_eq!(
        external.external_uri.as_deref(),
        Some("https://example.test/types#ExternalBase")
    );
    assert!(external.path.is_none());
}

#[test]
fn type_hierarchy_result_represents_supported_unsupported_and_combined_outcomes() {
    let child = workspace_hierarchy_item();
    let parent = TypeHierarchyItem {
        name: "Parent".into(),
        kind: "trait".into(),
        detail: None,
        range: hierarchy_range(1, 0, 16),
        selection_range: hierarchy_range(1, 6, 12),
        path: None,
        external_uri: Some("file:///opt/sdk/parent.rs".into()),
        external: true,
    };
    let combined = TypeHierarchyResult {
        supported: true,
        supertypes: vec![parent],
        subtypes: vec![child],
        outcome: None,
    };
    let value = serde_json::to_value(&combined).unwrap();
    let round_tripped: TypeHierarchyResult = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(round_tripped, combined);
    assert!(value.get("outcome").is_none());
    assert_eq!(
        value["supertypes"][0]["externalUri"],
        "file:///opt/sdk/parent.rs"
    );
    assert_eq!(value["subtypes"][0]["path"], "src/lib.rs");

    let unsupported = TypeHierarchyResult {
        supported: false,
        supertypes: Vec::new(),
        subtypes: Vec::new(),
        outcome: Some(LspOutcome {
            status: LspOutcomeStatus::Unsupported,
            code: LspOutcomeCode::CapabilityUnavailable,
            language: Some("rust".into()),
            command: Some("rust-analyzer".into()),
            operation: Some(LspOperation::TypeHierarchy),
            path: Some("src/lib.rs".into()),
            phase: LspOutcomePhase::Runtime,
            message: "type hierarchy is unsupported".into(),
        }),
    };
    assert_eq!(
        serde_json::to_value(unsupported).unwrap()["outcome"]["operation"],
        "type_hierarchy"
    );
}

#[test]
fn type_hierarchy_operation_is_a_copyable_fieldless_snake_case_unit_variant() {
    fn copy_operation(operation: LspOperation) -> (LspOperation, LspOperation) {
        (operation, operation)
    }

    let (first, second) = copy_operation(LspOperation::TypeHierarchy);
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        json!("type_hierarchy")
    );
    assert_eq!(
        serde_json::from_value::<LspOperation>(json!("type_hierarchy")).unwrap(),
        second
    );
}

#[test]
fn type_hierarchy_public_dtos_are_available_from_the_crate_root() {
    fn accepts_public_contract(
        _direction: TypeHierarchyDirection,
        _query: TypeHierarchyQuery,
        _range: TypeHierarchyRange,
        _item: TypeHierarchyItem,
        _result: TypeHierarchyResult,
    ) {
    }

    let range = hierarchy_range(0, 0, 1);
    let item = workspace_hierarchy_item();
    accepts_public_contract(
        TypeHierarchyDirection::Supertypes,
        TypeHierarchyQuery {
            path: "src/lib.rs".into(),
            line: 0,
            character: 0,
            direction: TypeHierarchyDirection::Supertypes,
        },
        range,
        item.clone(),
        TypeHierarchyResult {
            supported: true,
            supertypes: vec![item],
            subtypes: Vec::new(),
            outcome: None,
        },
    );
}

#[test]
fn type_hierarchy_item_rejects_invalid_location_forms_for_both_external_values() {
    let cases = [
        (false, None, None),
        (false, Some("src/lib.rs"), Some("file:///opt/sdk/lib.rs")),
        (false, None, Some("file:///opt/sdk/lib.rs")),
        (true, Some("src/lib.rs"), None),
        (true, None, None),
        (true, Some("src/lib.rs"), Some("file:///opt/sdk/lib.rs")),
    ];

    for (external, path, external_uri) in cases {
        let value = json!({
            "name": "Invalid",
            "kind": "class",
            "detail": null,
            "range": {"line": 0, "character": 0, "endLine": 0, "endCharacter": 7},
            "selectionRange": {"line": 0, "character": 0, "endLine": 0, "endCharacter": 7},
            "path": path,
            "externalUri": external_uri,
            "external": external
        });
        assert!(
            serde_json::from_value::<TypeHierarchyItem>(value).is_err(),
            "external={external}, path={path:?}, externalUri={external_uri:?} must be rejected"
        );
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
