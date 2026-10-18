use std::collections::HashMap;

use extension_protocol::messages::{
    PATH_RENAME_PLANNING_REQUEST_SCHEMA_JSON, PATH_RENAME_REQUEST_SCHEMA_JSON, PathMove,
    PathMoveKind, PathRenameError, PathRenameErrorCode, PathRenamePlan, PathRenamePlanningRequest,
    PathRenamePlanningResult, PathRenameRequest, PathRenameResult, PathRenameSemanticPolicy,
    PathRenameStatus, PerFileEditResult, RenameError, RenameErrorCode, RenameResult,
    WorkspaceApplyEditsResult, WorkspaceEditSpan,
};
use extension_protocol::{Capability, HostCall, RenameRequest};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

const SOURCE_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const EDIT_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + DeserializeOwned,
{
    serde_json::from_value(serde_json::to_value(value).expect("serialize fixture"))
        .expect("deserialize fixture")
}

fn schema_accepts(schema: &serde_json::Value, value: &serde_json::Value) -> bool {
    if let Some(allowed) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !allowed.contains(value)
    {
        return false;
    }
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("string") => {
            let Some(text) = value.as_str() else {
                return false;
            };
            schema.get("pattern").is_none()
                || (text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
        }
        Some("boolean") => value.is_boolean(),
        Some("array") => value.as_array().is_some_and(|items| {
            schema.get("items").is_none_or(|item_schema| {
                items.iter().all(|item| schema_accepts(item_schema, item))
            })
        }),
        Some("object") => {
            let Some(object) = value.as_object() else {
                return false;
            };
            let properties = schema
                .get("properties")
                .and_then(serde_json::Value::as_object);
            let required_present = schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|required| {
                    required.iter().all(|field| {
                        field
                            .as_str()
                            .is_some_and(|field| object.contains_key(field))
                    })
                });
            required_present
                && object.iter().all(|(key, item)| {
                    if let Some(property_schema) = properties.and_then(|props| props.get(key)) {
                        schema_accepts(property_schema, item)
                    } else {
                        match schema.get("additionalProperties") {
                            Some(serde_json::Value::Bool(allowed)) => *allowed,
                            Some(additional_schema) => schema_accepts(additional_schema, item),
                            None => true,
                        }
                    }
                })
        }
        _ => false,
    }
}

fn moves() -> Vec<PathMove> {
    vec![
        PathMove {
            source: "src/old".into(),
            destination: "src/new".into(),
            kind: PathMoveKind::Directory,
        },
        PathMove {
            source: "src/old/lib.rs".into(),
            destination: "src/new/lib.rs".into(),
            kind: PathMoveKind::File,
        },
    ]
}

fn edits() -> Vec<WorkspaceEditSpan> {
    vec![
        WorkspaceEditSpan {
            path: "src/consumer.rs".into(),
            start_byte: 11,
            end_byte: 21,
            replacement: "crate::new".into(),
            base_hash: Some(EDIT_HASH.into()),
        },
        WorkspaceEditSpan {
            path: "tests/imports.rs".into(),
            start_byte: 3,
            end_byte: 13,
            replacement: "crate::new".into(),
            base_hash: Some(SOURCE_HASH.into()),
        },
    ]
}

fn plan() -> PathRenamePlan {
    PathRenamePlan {
        moves: moves(),
        edits: edits(),
        semantic_policy: PathRenameSemanticPolicy::Required,
    }
}

/// Deliberately literal: exact wire expectations must not reuse the serializer
/// whose output this integration test protects.
fn canonical_plan_json() -> serde_json::Value {
    json!({
        "moves": [
            {"source": "src/old", "destination": "src/new", "kind": "directory"},
            {"source": "src/old/lib.rs", "destination": "src/new/lib.rs", "kind": "file"}
        ],
        "edits": [
            {"path": "src/consumer.rs", "start_byte": 11, "end_byte": 21, "replacement": "crate::new", "base_hash": EDIT_HASH},
            {"path": "tests/imports.rs", "start_byte": 3, "end_byte": 13, "replacement": "crate::new", "base_hash": SOURCE_HASH}
        ],
        "semantic_policy": "required"
    })
}

fn error(code: PathRenameErrorCode, path: Option<&str>, message: &str) -> PathRenameError {
    PathRenameError {
        code,
        path: path.map(str::to_owned),
        message: message.into(),
    }
}

#[test]
fn path_rename_request_uses_exact_fields_defaults_and_validates_expected_versions() {
    let mut expected_versions = HashMap::new();
    expected_versions.insert("src/old/lib.rs".into(), SOURCE_HASH.into());
    let request = PathRenameRequest {
        source: "src/old".into(),
        destination: "src/new".into(),
        dry_run: true,
        semantic_policy: PathRenameSemanticPolicy::BestEffort,
        expected_versions,
    };

    assert_eq!(
        serde_json::to_value(&request).expect("serialize request"),
        json!({
            "source": "src/old",
            "destination": "src/new",
            "dry_run": true,
            "semantic_policy": "best_effort",
            "expected_versions": {"src/old/lib.rs": SOURCE_HASH}
        })
    );

    let defaulted: PathRenameRequest = serde_json::from_value(json!({
        "source": "src/old",
        "destination": "src/new",
        "semantic_policy": "none"
    }))
    .expect("absent optional fields use safe defaults");
    assert!(!defaulted.dry_run);
    assert!(defaulted.expected_versions.is_empty());

    assert!(
        serde_json::from_value::<PathRenameRequest>(json!({
            "source": "src/old",
            "destination": "src/new",
            "semantic_policy": "required",
            "expected_versions": {"src/old/lib.rs": "NOT-A-SHA256"}
        }))
        .is_err(),
        "a present malformed SHA-256 must not be accepted"
    );
}

#[test]
fn path_rename_enums_use_only_the_exact_wire_values() {
    let statuses = [
        (PathRenameStatus::Preview, "preview"),
        (PathRenameStatus::Applied, "applied"),
        (PathRenameStatus::Partial, "partial"),
        (PathRenameStatus::Unsupported, "unsupported"),
        (PathRenameStatus::Rejected, "rejected"),
    ];
    let codes = [
        (PathRenameErrorCode::SourceNotFound, "source_not_found"),
        (PathRenameErrorCode::DestinationExists, "destination_exists"),
        (PathRenameErrorCode::IdenticalPaths, "identical_paths"),
        (
            PathRenameErrorCode::DestinationInsideSource,
            "destination_inside_source",
        ),
        (
            PathRenameErrorCode::DestinationExcluded,
            "destination_excluded",
        ),
        (PathRenameErrorCode::WorkspaceEscape, "workspace_escape"),
        (
            PathRenameErrorCode::DuplicateDestination,
            "duplicate_destination",
        ),
        (
            PathRenameErrorCode::InvalidExpectedVersion,
            "invalid_expected_version",
        ),
        (
            PathRenameErrorCode::SemanticUnsupported,
            "semantic_unsupported",
        ),
        (PathRenameErrorCode::SemanticBackend, "semantic_backend"),
        (PathRenameErrorCode::FilesystemError, "filesystem_error"),
        (PathRenameErrorCode::PersistenceError, "persistence_error"),
        (PathRenameErrorCode::CasConflict, "cas_conflict"),
    ];
    let policies = [
        (PathRenameSemanticPolicy::None, "none"),
        (PathRenameSemanticPolicy::BestEffort, "best_effort"),
        (PathRenameSemanticPolicy::Required, "required"),
    ];
    let kinds = [
        (PathMoveKind::File, "file"),
        (PathMoveKind::Directory, "directory"),
    ];

    for (value, wire) in statuses {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(wire));
    }
    for (value, wire) in codes {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(wire));
    }
    for (value, wire) in policies {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(wire));
    }
    for (value, wire) in kinds {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(wire));
    }

    assert!(
        serde_json::from_value::<PathRenameStatus>(json!("completed")).is_err(),
        "unknown result statuses must be rejected"
    );
    assert!(
        serde_json::from_value::<PathRenameErrorCode>(json!("permission_denied")).is_err(),
        "unknown path-rename error codes must be rejected"
    );
    assert!(
        serde_json::from_value::<PathMoveKind>(json!("symlink")).is_err(),
        "unknown path-move kinds must be rejected"
    );
}

#[test]
fn public_and_planning_request_schemas_match_canonical_fixtures_and_reject_malformed_inputs() {
    let public_schema: serde_json::Value =
        serde_json::from_str(PATH_RENAME_REQUEST_SCHEMA_JSON).expect("public schema is valid JSON");
    let planning_schema: serde_json::Value =
        serde_json::from_str(PATH_RENAME_PLANNING_REQUEST_SCHEMA_JSON)
            .expect("planning schema is valid JSON");
    let public = PathRenameRequest {
        source: "src/old".into(),
        destination: "src/new".into(),
        dry_run: true,
        semantic_policy: PathRenameSemanticPolicy::Required,
        expected_versions: HashMap::from([("src/old/lib.rs".into(), SOURCE_HASH.into())]),
    };
    let public_json = json!({
        "source": "src/old",
        "destination": "src/new",
        "dry_run": true,
        "semantic_policy": "required",
        "expected_versions": {"src/old/lib.rs": SOURCE_HASH}
    });
    assert_eq!(serde_json::to_value(&public).unwrap(), public_json);
    assert!(schema_accepts(&public_schema, &public_json));
    assert_eq!(
        serde_json::from_value::<PathRenameRequest>(public_json.clone()).unwrap(),
        public
    );
    assert!(
        !schema_accepts(
            &public_schema,
            &json!({
                "source": "src/old", "destination": "src/new", "semantic_policy": "required",
                "expected_versions": {"src/old/lib.rs": 42}
            })
        ),
        "the published public schema independently rejects non-string version values"
    );
    assert!(
        serde_json::from_value::<PathRenameRequest>(json!({
            "source": "src/old", "destination": "src/new", "semantic_policy": "required",
            "expected_versions": {"src/old/lib.rs": 42}
        }))
        .is_err(),
        "the public request schema must reject non-string version values"
    );

    let planning = PathRenamePlanningRequest {
        moves: moves(),
        semantic_policy: PathRenameSemanticPolicy::Required,
    };
    let planning_json = json!({
        "moves": [
            {"source": "src/old", "destination": "src/new", "kind": "directory"},
            {"source": "src/old/lib.rs", "destination": "src/new/lib.rs", "kind": "file"}
        ],
        "semantic_policy": "required"
    });
    assert_eq!(serde_json::to_value(&planning).unwrap(), planning_json);
    assert!(schema_accepts(&planning_schema, &planning_json));
    assert_eq!(
        serde_json::from_value::<PathRenamePlanningRequest>(planning_json).unwrap(),
        planning
    );
    assert!(
        !schema_accepts(
            &planning_schema,
            &json!({
                "moves": [{"source": "src/old", "destination": "src/new", "kind": "symlink"}],
                "semantic_policy": "required"
            })
        ),
        "the published planning schema independently rejects unknown move kinds"
    );
    assert!(
        serde_json::from_value::<PathRenamePlanningRequest>(json!({
            "moves": [{"source": "src/old", "destination": "src/new", "kind": "symlink"}],
            "semantic_policy": "required"
        }))
        .is_err(),
        "the planning request schema must reject an unknown move kind"
    );
}

#[test]
fn path_rename_request_result_and_planning_round_trips_preserve_order_and_optionals() {
    let request = PathRenameRequest {
        source: "src/old".into(),
        destination: "src/new".into(),
        dry_run: false,
        semantic_policy: PathRenameSemanticPolicy::Required,
        expected_versions: HashMap::from([
            ("src/old/lib.rs".into(), SOURCE_HASH.into()),
            ("src/consumer.rs".into(), EDIT_HASH.into()),
        ]),
    };
    let result = PathRenameResult {
        status: PathRenameStatus::Applied,
        plan: plan(),
        edit_result: Some(WorkspaceApplyEditsResult {
            files_changed: 1,
            per_file: vec![PerFileEditResult {
                path: "src/consumer.rs".into(),
                applied: true,
                edits_applied: 1,
                edits_skipped: 0,
                new_version: Some(SOURCE_HASH.into()),
                preview: None,
                error: None,
            }],
        }),
        errors: vec![],
    };
    let planning_request = PathRenamePlanningRequest {
        moves: moves(),
        semantic_policy: PathRenameSemanticPolicy::Required,
    };
    let planning_result = PathRenamePlanningResult {
        plan: Some(plan()),
        error: None,
    };

    assert_eq!(round_trip(&request), request);
    assert_eq!(round_trip(&result), result);
    assert_eq!(round_trip(&planning_request), planning_request);
    assert_eq!(round_trip(&planning_result), planning_result);
}

#[test]
fn path_rename_malformed_required_fields_unknown_values_and_invalid_plans_are_rejected() {
    let malformed = [
        json!({"destination": "src/new", "semantic_policy": "none"}),
        json!({"source": "src/old", "destination": "src/new"}),
        json!({"source": "src/old", "destination": "src/new", "semantic_policy": "fallback"}),
        json!({"source": "src/old", "destination": "src/new", "semantic_policy": "none", "unexpected": true}),
    ];
    for value in malformed {
        assert!(serde_json::from_value::<PathRenameRequest>(value).is_err());
    }

    assert!(
        serde_json::from_value::<PathRenamePlan>(json!({
            "moves": [{"source": "src/old.rs", "destination": "src/new.rs", "kind": "file"}],
            "edits": [{
                "path": "src/consumer.rs",
                "start_byte": 0,
                "end_byte": 10,
                "replacement": "crate::new"
            }],
            "semantic_policy": "required"
        }))
        .is_err(),
        "semantic planning edits require a base_hash"
    );

    for value in [
        json!({"plan": canonical_plan_json(), "error": {
            "code": "semantic_backend", "message": "planning failed"
        }}),
        json!({"plan": null, "error": null}),
    ] {
        assert!(
            serde_json::from_value::<PathRenamePlanningResult>(value).is_err(),
            "planning results must contain exactly one of plan or error"
        );
    }
}

#[test]
fn path_rename_outbound_values_enforce_plan_and_planning_result_invariants() {
    let mut missing_hash = plan();
    missing_hash.edits[0].base_hash = None;
    assert!(serde_json::to_value(missing_hash).is_err());

    let mut malformed_hash = plan();
    malformed_hash.edits[0].base_hash = Some("not-a-sha256".into());
    assert!(serde_json::to_value(malformed_hash).is_err());

    let success = PathRenamePlanningResult {
        plan: Some(plan()),
        error: None,
    };
    let success_json = serde_json::to_value(success).expect("serialize valid planning success");
    assert_eq!(success_json, json!({"plan": canonical_plan_json()}));

    for invalid in [
        PathRenamePlanningResult {
            plan: None,
            error: None,
        },
        PathRenamePlanningResult {
            plan: Some(plan()),
            error: Some(error(
                PathRenameErrorCode::SemanticBackend,
                None,
                "Semantic planning failed",
            )),
        },
    ] {
        assert!(
            serde_json::to_value(invalid).is_err(),
            "outbound planning results require exactly one response field"
        );
    }
}

#[test]
fn path_rename_outbound_plans_reject_workspace_external_paths() {
    for unsafe_path in ["/tmp/outside.rs", "../outside.rs", "src/../outside.rs"] {
        let mut unsafe_move = plan();
        unsafe_move.moves[0].destination = unsafe_path.into();
        assert!(serde_json::to_value(unsafe_move).is_err());

        let mut unsafe_edit = plan();
        unsafe_edit.edits[0].path = unsafe_path.into();
        assert!(serde_json::to_value(unsafe_edit).is_err());
    }
}

#[test]
fn existing_rename_host_call_and_manifest_capability_json_remains_compatible() {
    let rename = RenameRequest {
        path: "src/lib.rs".into(),
        line: 4,
        character: 7,
        new_name: "renamed".into(),
        dry_run: None,
    };
    let host_call = HostCall::ReadFile {
        path: "src/lib.rs".into(),
    };
    let rename_result = RenameResult {
        applied: true,
        files_changed: 1,
        spans: vec![WorkspaceEditSpan {
            path: "src/lib.rs".into(),
            start_byte: 4,
            end_byte: 11,
            replacement: "renamed".into(),
            base_hash: Some(SOURCE_HASH.into()),
        }],
        preview: None,
        per_file: vec![],
    };
    let rename_error = RenameError {
        code: RenameErrorCode::BackendError,
        message: "language server failed".into(),
        path: None,
        outcome: None,
    };
    let workspace_edit = WorkspaceEditSpan {
        path: "src/lib.rs".into(),
        start_byte: 4,
        end_byte: 11,
        replacement: "renamed".into(),
        base_hash: Some(SOURCE_HASH.into()),
    };

    assert_eq!(
        serde_json::to_string(&rename).unwrap(),
        r#"{"path":"src/lib.rs","line":4,"character":7,"new_name":"renamed"}"#
    );
    assert_eq!(
        serde_json::to_string(&host_call).unwrap(),
        r#"{"type":"ReadFile","path":"src/lib.rs"}"#
    );
    assert_eq!(
        serde_json::to_string(&Capability::RequestApplyEdits).unwrap(),
        r#""request_apply_edits""#
    );
    assert_eq!(
        serde_json::to_string(&rename_result).unwrap(),
        format!(
            r#"{{"applied":true,"files_changed":1,"spans":[{{"path":"src/lib.rs","start_byte":4,"end_byte":11,"replacement":"renamed","base_hash":"{SOURCE_HASH}"}}],"preview":null,"per_file":[]}}"#
        )
    );
    assert_eq!(
        serde_json::to_string(&rename_error).unwrap(),
        r#"{"code":"backend_error","message":"language server failed"}"#
    );
    assert_eq!(
        serde_json::to_string(&workspace_edit).unwrap(),
        format!(
            r#"{{"path":"src/lib.rs","start_byte":4,"end_byte":11,"replacement":"renamed","base_hash":"{SOURCE_HASH}"}}"#
        )
    );
}

#[test]
fn path_rename_typed_examples_match_the_canonical_request_and_every_result_status() {
    let empty_plan = PathRenamePlan {
        moves: vec![],
        edits: vec![],
        semantic_policy: PathRenameSemanticPolicy::Required,
    };
    let fixtures = [
        (
            PathRenameResult {
                status: PathRenameStatus::Preview,
                plan: plan(),
                edit_result: None,
                errors: vec![],
            },
            json!({"status": "preview", "plan": {
                "moves": [
                    {"source": "src/old", "destination": "src/new", "kind": "directory"},
                    {"source": "src/old/lib.rs", "destination": "src/new/lib.rs", "kind": "file"}
                ],
                "edits": [
                    {"path": "src/consumer.rs", "start_byte": 11, "end_byte": 21, "replacement": "crate::new", "base_hash": EDIT_HASH},
                    {"path": "tests/imports.rs", "start_byte": 3, "end_byte": 13, "replacement": "crate::new", "base_hash": SOURCE_HASH}
                ],
                "semantic_policy": "required"
            }, "errors": []}),
        ),
        (
            PathRenameResult {
                status: PathRenameStatus::Applied,
                plan: plan(),
                edit_result: Some(WorkspaceApplyEditsResult {
                    files_changed: 0,
                    per_file: vec![],
                }),
                errors: vec![],
            },
            json!({"status": "applied", "plan": canonical_plan_json(), "edit_result": {"files_changed": 0, "per_file": []}, "errors": []}),
        ),
        (
            PathRenameResult {
                status: PathRenameStatus::Partial,
                plan: plan(),
                edit_result: None,
                errors: vec![error(
                    PathRenameErrorCode::CasConflict,
                    Some("src/consumer.rs"),
                    "File changed before semantic edits were applied",
                )],
            },
            json!({"status": "partial", "plan": canonical_plan_json(), "errors": [{"code": "cas_conflict", "path": "src/consumer.rs", "message": "File changed before semantic edits were applied"}]}),
        ),
        (
            PathRenameResult {
                status: PathRenameStatus::Unsupported,
                plan: empty_plan.clone(),
                edit_result: None,
                errors: vec![error(
                    PathRenameErrorCode::SemanticUnsupported,
                    None,
                    "Semantic rename is unavailable",
                )],
            },
            json!({"status": "unsupported", "plan": {"moves": [], "edits": [], "semantic_policy": "required"}, "errors": [{"code": "semantic_unsupported", "message": "Semantic rename is unavailable"}]}),
        ),
        (
            PathRenameResult {
                status: PathRenameStatus::Rejected,
                plan: empty_plan,
                edit_result: None,
                errors: vec![error(
                    PathRenameErrorCode::SourceNotFound,
                    Some("src/missing.rs"),
                    "Source path was not found",
                )],
            },
            json!({"status": "rejected", "plan": {"moves": [], "edits": [], "semantic_policy": "required"}, "errors": [{"code": "source_not_found", "path": "src/missing.rs", "message": "Source path was not found"}]}),
        ),
    ];

    for (fixture, expected) in fixtures {
        assert_eq!(serde_json::to_value(fixture).unwrap(), expected);
    }
}

#[test]
fn path_rename_error_optional_path_is_omitted_and_backend_details_are_sanitized() {
    let safe = error(
        PathRenameErrorCode::SemanticBackend,
        None,
        "Semantic planning failed",
    );
    assert_eq!(
        serde_json::to_value(safe).unwrap(),
        json!({"code": "semantic_backend", "message": "Semantic planning failed"})
    );

    let unsafe_error = error(
        PathRenameErrorCode::SemanticBackend,
        Some("../outside.rs"),
        "backend failed at /home/alice/private.rs\nAuthorization: Bearer secret",
    );
    let value = serde_json::to_value(unsafe_error).expect("serialize sanitized error");
    let encoded = serde_json::to_string(&value).unwrap();

    assert!(value.get("path").is_none());
    assert!(!encoded.contains("/home/alice"));
    assert!(!encoded.contains("Bearer secret"));

    let windows_and_key = error(
        PathRenameErrorCode::SemanticBackend,
        None,
        r#"rename failed at C:\Users\alice\repo; api_key=secret"#,
    );
    assert_eq!(
        serde_json::to_value(windows_and_key).unwrap(),
        json!({"code": "semantic_backend", "message": "Semantic planning failed"})
    );
}
