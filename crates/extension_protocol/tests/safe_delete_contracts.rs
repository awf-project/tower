use extension_protocol::messages::*;
use serde::Serialize;
use serde_json::{Value, json};

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn position(line: u32, character: u32) -> SafeDeletePosition {
    SafeDeletePosition { line, character }
}

fn declaration() -> SafeDeleteDeclarationPlan {
    SafeDeleteDeclarationPlan {
        path: "src/lib.rs".into(),
        symbol_name: "remove_me".into(),
        kind: "function".into(),
        position: position(3, 7),
        deletion_span: SafeDeleteByteSpan {
            start_byte: 24,
            end_byte: 51,
        },
        base_hash: HASH_A.into(),
    }
}

fn reference() -> SafeDeleteReference {
    SafeDeleteReference {
        path: "src/consumer.rs".into(),
        start: position(8, 4),
        end: position(8, 13),
        base_hash: HASH_B.into(),
    }
}

fn cleanup_edit() -> SafeDeleteCleanupEdit {
    SafeDeleteCleanupEdit {
        path: "src/consumer.rs".into(),
        start_byte: 80,
        end_byte: 89,
        replacement: "replacement".into(),
        base_hash: HASH_B.into(),
    }
}

fn semantic_plan(coverage: CleanupCoverage) -> SafeDeleteSemanticPlan {
    SafeDeleteSemanticPlan {
        references: vec![reference()],
        cleanup_edits: vec![cleanup_edit()],
        coverage,
    }
}

fn file_outcome(
    path: &str,
    outcome: SafeDeleteFileOutcomeStatus,
    error: Option<SafeDeleteFileError>,
) -> SafeDeleteFileOutcome {
    SafeDeleteFileOutcome {
        path: path.into(),
        outcome,
        error,
    }
}

fn assert_outbound_rejected<T: Serialize>(value: &T) {
    assert!(serde_json::to_value(value).is_err());
}

fn declaration_json() -> Value {
    json!({
        "path": "src/lib.rs",
        "symbol_name": "remove_me",
        "kind": "function",
        "position": {"line": 3, "character": 7},
        "deletion_span": {"start_byte": 24, "end_byte": 51},
        "base_hash": HASH_A
    })
}

fn reference_json() -> Value {
    json!({
        "path": "src/consumer.rs",
        "start": {"line": 8, "character": 4},
        "end": {"line": 8, "character": 13},
        "base_hash": HASH_B
    })
}

fn cleanup_edit_json() -> Value {
    json!({
        "path": "src/consumer.rs",
        "start_byte": 80,
        "end_byte": 89,
        "replacement": "replacement",
        "base_hash": HASH_B
    })
}

fn semantic_plan_json(coverage: &str) -> Value {
    json!({
        "references": [reference_json()],
        "cleanup_edits": [cleanup_edit_json()],
        "coverage": coverage
    })
}

fn assert_inbound_rejected<T: serde::de::DeserializeOwned>(value: Value, context: &str) {
    let error = match serde_json::from_value::<T>(value) {
        Ok(_) => panic!("{context}: value was accepted"),
        Err(error) => error,
    };
    assert!(
        error.is_data(),
        "{context}: expected a data error, got {error}"
    );
}

#[test]
fn public_safe_delete_types_are_available_from_messages_without_changing_existing_delete_contract()
{
    let request = SafeDeleteRequest {
        path: "src/lib.rs".into(),
        symbol_name: "remove_me".into(),
        kind: None,
        dry_run: false,
    };
    let declaration_request = SafeDeleteDeclarationRequest {
        path: request.path.clone(),
        symbol_name: request.symbol_name.clone(),
        kind: request.kind.clone(),
    };
    let semantic_request = SafeDeleteSemanticRequest {
        declaration: declaration(),
    };
    let mutation_request = SafeDeleteMutationRequest {
        edits: vec![cleanup_edit()],
        dry_run: true,
    };

    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({
            "path": "src/lib.rs",
            "symbol_name": "remove_me",
            "dry_run": false
        })
    );
    assert_eq!(
        serde_json::to_value(declaration_request).unwrap(),
        json!({
            "path": "src/lib.rs",
            "symbol_name": "remove_me"
        })
    );
    assert_eq!(semantic_request.declaration, declaration());
    assert_eq!(mutation_request.edits, vec![cleanup_edit()]);

    let existing_delete = AnchoredSymbolEditRequest {
        path: "src/lib.rs".into(),
        symbol_name: "remove_me".into(),
        kind: Some("function".into()),
        replacement: None,
        dry_run: Some(true),
    };
    assert_eq!(
        serde_json::to_value(existing_delete).unwrap(),
        json!({
            "path": "src/lib.rs",
            "symbol_name": "remove_me",
            "kind": "function",
            "dry_run": true
        })
    );
}

#[test]
fn request_defaults_dry_run_and_report_uses_exact_optional_and_required_fields() {
    let request: SafeDeleteRequest = serde_json::from_value(json!({
        "path": "src/lib.rs",
        "symbol_name": "remove_me"
    }))
    .unwrap();
    assert!(!request.dry_run);
    assert_eq!(request.kind, None);
    assert!(
        serde_json::from_value::<SafeDeleteRequest>(json!({
            "path": "src/lib.rs",
            "symbol_name": "remove_me",
            "dry_run": false,
            "unexpected": true
        }))
        .is_err()
    );

    let report = SafeDeleteReport {
        status: SafeDeleteStatus::Blocked,
        blocking_reason: Some(SafeDeleteBlockingReason::NotFound),
        declaration: None,
        references: vec![],
        proposed_edits: vec![],
        affected_files: vec![],
        mutation_result: None,
    };
    assert_eq!(
        serde_json::to_value(report).unwrap(),
        json!({
            "status": "blocked",
            "blocking_reason": "not_found",
            "references": [],
            "proposed_edits": [],
            "affected_files": []
        })
    );
}

#[test]
fn statuses_and_blocking_reasons_serialize_to_the_exact_allowlisted_strings() {
    let statuses = [
        (SafeDeleteStatus::Blocked, "blocked"),
        (SafeDeleteStatus::Previewed, "previewed"),
        (SafeDeleteStatus::Applied, "applied"),
        (SafeDeleteStatus::PartialCommit, "partial_commit"),
    ];
    for (status, wire) in statuses {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(wire));
    }

    let reasons = [
        (SafeDeleteBlockingReason::NotFound, "not_found"),
        (
            SafeDeleteBlockingReason::AmbiguousSymbol,
            "ambiguous_symbol",
        ),
        (
            SafeDeleteBlockingReason::InvalidDeclaration,
            "invalid_declaration",
        ),
        (
            SafeDeleteBlockingReason::CapabilityUnavailable,
            "capability_unavailable",
        ),
        (
            SafeDeleteBlockingReason::ReferencesUnresolved,
            "references_unresolved",
        ),
        (SafeDeleteBlockingReason::LiveReferences, "live_references"),
        (
            SafeDeleteBlockingReason::CleanupIncomplete,
            "cleanup_incomplete",
        ),
        (SafeDeleteBlockingReason::InvalidCleanup, "invalid_cleanup"),
        (SafeDeleteBlockingReason::CasConflict, "cas_conflict"),
        (SafeDeleteBlockingReason::BackendError, "backend_error"),
    ];
    for (reason, wire) in reasons {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(wire));
    }
}

#[test]
fn declaration_outcome_is_exactly_one_tagged_result_with_a_valid_versioned_plan() {
    let planned = SafeDeleteDeclarationOutcome::Planned {
        plan: declaration(),
    };
    assert_eq!(
        serde_json::to_value(planned).unwrap(),
        json!({
            "status": "planned",
            "plan": {
                "path": "src/lib.rs",
                "symbol_name": "remove_me",
                "kind": "function",
                "position": {"line": 3, "character": 7},
                "deletion_span": {"start_byte": 24, "end_byte": 51},
                "base_hash": HASH_A
            }
        })
    );
    for wire in ["not_found", "ambiguous_symbol", "invalid_declaration"] {
        let value: SafeDeleteDeclarationOutcome =
            serde_json::from_value(json!({"status": wire})).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            json!({"status": wire})
        );
    }
    assert!(
        serde_json::from_value::<SafeDeleteDeclarationOutcome>(json!({
            "status": "planned"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SafeDeleteDeclarationOutcome>(json!({
            "status": "not_found",
            "plan": serde_json::to_value(declaration()).unwrap()
        }))
        .is_err()
    );
}

#[test]
fn semantic_outcomes_are_exact_and_backend_failures_expose_no_backend_data() {
    let complete = SafeDeleteSemanticOutcome::Complete {
        plan: semantic_plan(CleanupCoverage::Complete),
    };
    let incomplete = SafeDeleteSemanticOutcome::Incomplete {
        plan: semantic_plan(CleanupCoverage::Incomplete),
    };
    assert_eq!(
        serde_json::to_value(complete).unwrap()["status"],
        "complete"
    );
    assert_eq!(
        serde_json::to_value(incomplete).unwrap()["status"],
        "incomplete"
    );

    let failures = [
        ("capability_unavailable", "capability_unavailable"),
        ("backend_timeout", "backend_error"),
        ("backend_crashed", "backend_error"),
        ("malformed_response", "backend_error"),
        ("invalid_location", "invalid_cleanup"),
        ("workspace_external", "invalid_cleanup"),
    ];
    for (status, public_reason) in failures {
        let outcome: SafeDeleteSemanticOutcome =
            serde_json::from_value(json!({"status": status})).unwrap();
        assert_eq!(
            serde_json::to_value(outcome).unwrap(),
            json!({"status": status})
        );
        let reason: SafeDeleteBlockingReason =
            serde_json::from_value(json!(public_reason)).unwrap();
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(public_reason));
    }
    assert!(
        serde_json::from_value::<SafeDeleteSemanticOutcome>(json!({
            "status": "backend_timeout",
            "message": "secret backend details"
        }))
        .is_err()
    );
}

#[test]
fn tagged_outcomes_reject_duplicate_status_and_plan_keys() {
    let declaration_plan = r#"{
        "path":"src/lib.rs","symbol_name":"remove_me","kind":"function",
        "position":{"line":3,"character":7},
        "deletion_span":{"start_byte":24,"end_byte":51},
        "base_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }"#;
    for wire in [
        format!(r#"{{"status":"not_found","status":"planned","plan":{declaration_plan}}}"#),
        format!(r#"{{"status":"planned","plan":{declaration_plan},"plan":{declaration_plan}}}"#),
    ] {
        assert!(
            serde_json::from_str::<SafeDeleteDeclarationOutcome>(&wire).is_err(),
            "accepted declaration outcome with duplicate key: {wire}"
        );
    }

    let semantic_plan = r#"{"references":[],"cleanup_edits":[],"coverage":"complete"}"#;
    for wire in [
        format!(r#"{{"status":"backend_timeout","status":"complete","plan":{semantic_plan}}}"#),
        format!(r#"{{"status":"complete","plan":{semantic_plan},"plan":{semantic_plan}}}"#),
    ] {
        assert!(
            serde_json::from_str::<SafeDeleteSemanticOutcome>(&wire).is_err(),
            "accepted semantic outcome with duplicate key: {wire}"
        );
    }
}

#[test]
fn semantic_plan_preserves_complete_references_edits_coverage_and_versions() {
    let plan = semantic_plan(CleanupCoverage::Complete);
    assert_eq!(
        serde_json::to_value(&plan).unwrap(),
        json!({
            "references": [{
                "path": "src/consumer.rs",
                "start": {"line": 8, "character": 4},
                "end": {"line": 8, "character": 13},
                "base_hash": HASH_B
            }],
            "cleanup_edits": [{
                "path": "src/consumer.rs",
                "start_byte": 80,
                "end_byte": 89,
                "replacement": "replacement",
                "base_hash": HASH_B
            }],
            "coverage": "complete"
        })
    );

    let live_without_edits: SafeDeleteSemanticPlan = serde_json::from_value(json!({
        "references": [{
            "path": "src/consumer.rs",
            "start": {"line": 8, "character": 4},
            "end": {"line": 8, "character": 13},
            "base_hash": HASH_B
        }],
        "cleanup_edits": [],
        "coverage": "complete"
    }))
    .unwrap();
    assert_eq!(live_without_edits.coverage, CleanupCoverage::Complete);

    assert!(
        serde_json::from_value::<SafeDeleteSemanticPlan>(json!({
            "references": [],
            "cleanup_edits": []
        }))
        .is_err()
    );
}

#[test]
fn mutation_request_and_result_enforce_ordered_status_specific_outcomes() {
    let request = SafeDeleteMutationRequest {
        edits: vec![cleanup_edit()],
        dry_run: false,
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({
            "edits": [{
                "path": "src/consumer.rs",
                "start_byte": 80,
                "end_byte": 89,
                "replacement": "replacement",
                "base_hash": HASH_B
            }],
            "dry_run": false
        })
    );

    let partial = SafeDeleteMutationResult {
        status: SafeDeleteMutationStatus::PartialCommit,
        rejection: None,
        per_file: vec![
            file_outcome("src/a.rs", SafeDeleteFileOutcomeStatus::Committed, None),
            file_outcome(
                "src/b.rs",
                SafeDeleteFileOutcomeStatus::Failed,
                Some(SafeDeleteFileError::PersistenceError),
            ),
            file_outcome("src/c.rs", SafeDeleteFileOutcomeStatus::Unattempted, None),
        ],
    };
    assert_eq!(
        serde_json::to_value(partial).unwrap(),
        json!({
            "status": "partial_commit",
            "per_file": [
                {"path": "src/a.rs", "outcome": "committed"},
                {"path": "src/b.rs", "outcome": "failed", "error": "persistence_error"},
                {"path": "src/c.rs", "outcome": "unattempted"}
            ]
        })
    );

    let rejected_without_reason = SafeDeleteMutationResult {
        status: SafeDeleteMutationStatus::Rejected,
        rejection: None,
        per_file: vec![file_outcome(
            "src/a.rs",
            SafeDeleteFileOutcomeStatus::Unattempted,
            None,
        )],
    };
    assert_outbound_rejected(&rejected_without_reason);
}

#[test]
fn literal_fixtures_round_trip_for_requests_and_declaration_outcomes() {
    let fixtures: Vec<(&str, Value)> = vec![
        (
            "request",
            json!({"path":"src/lib.rs","symbol_name":"remove_me","kind":"function","dry_run":true}),
        ),
        (
            "declaration_request",
            json!({"path":"src/lib.rs","symbol_name":"remove_me","kind":"function"}),
        ),
        (
            "semantic_request",
            json!({"declaration": declaration_json()}),
        ),
        (
            "declaration_planned",
            json!({"status":"planned","plan": declaration_json()}),
        ),
        ("declaration_not_found", json!({"status":"not_found"})),
        (
            "declaration_ambiguous_symbol",
            json!({"status":"ambiguous_symbol"}),
        ),
        (
            "declaration_invalid_declaration",
            json!({"status":"invalid_declaration"}),
        ),
    ];

    for (name, fixture) in fixtures {
        let serialized = match name {
            "request" => serde_json::to_value(
                serde_json::from_value::<SafeDeleteRequest>(fixture.clone()).unwrap(),
            ),
            "declaration_request" => serde_json::to_value(
                serde_json::from_value::<SafeDeleteDeclarationRequest>(fixture.clone()).unwrap(),
            ),
            "semantic_request" => serde_json::to_value(
                serde_json::from_value::<SafeDeleteSemanticRequest>(fixture.clone()).unwrap(),
            ),
            _ => serde_json::to_value(
                serde_json::from_value::<SafeDeleteDeclarationOutcome>(fixture.clone()).unwrap(),
            ),
        }
        .unwrap();
        assert_eq!(serialized, fixture, "fixture {name}");
    }
}

#[test]
fn literal_fixtures_round_trip_for_semantic_outcomes_and_plans() {
    let fixtures: Vec<(&str, Value)> = vec![
        (
            "semantic_complete",
            json!({"status":"complete","plan": semantic_plan_json("complete")}),
        ),
        (
            "semantic_incomplete",
            json!({"status":"incomplete","plan": semantic_plan_json("incomplete")}),
        ),
        (
            "semantic_unavailable",
            json!({"status":"capability_unavailable"}),
        ),
        ("semantic_timeout", json!({"status":"backend_timeout"})),
        ("semantic_crashed", json!({"status":"backend_crashed"})),
        ("semantic_malformed", json!({"status":"malformed_response"})),
        (
            "semantic_invalid_location",
            json!({"status":"invalid_location"}),
        ),
        (
            "semantic_workspace_external",
            json!({"status":"workspace_external"}),
        ),
    ];

    for (name, fixture) in fixtures {
        let serialized = serde_json::to_value(
            serde_json::from_value::<SafeDeleteSemanticOutcome>(fixture.clone()).unwrap(),
        )
        .unwrap();
        assert_eq!(serialized, fixture, "fixture {name}");
    }
}

#[test]
fn literal_fixtures_round_trip_for_mutation_requests_and_every_result_status() {
    let fixtures: Vec<(&str, Value)> = vec![
        (
            "mutation_request",
            json!({"edits":[cleanup_edit_json()],"dry_run":false}),
        ),
        (
            "mutation_previewed",
            json!({"status":"previewed","per_file":[{"path":"src/lib.rs","outcome":"unattempted"}]}),
        ),
        (
            "mutation_applied",
            json!({"status":"applied","per_file":[{"path":"src/lib.rs","outcome":"committed"}]}),
        ),
        (
            "mutation_rejected",
            json!({"status":"rejected","rejection":"cas_conflict","per_file":[{"path":"src/lib.rs","outcome":"unattempted"}]}),
        ),
        (
            "mutation_partial_commit",
            json!({"status":"partial_commit","per_file":[{"path":"src/a.rs","outcome":"committed"},{"path":"src/b.rs","outcome":"failed","error":"filesystem_error"},{"path":"src/c.rs","outcome":"unattempted"}]}),
        ),
    ];

    for (name, fixture) in fixtures {
        let serialized = match name {
            "request" => serde_json::to_value(
                serde_json::from_value::<SafeDeleteRequest>(fixture.clone()).unwrap(),
            ),
            "declaration_request" => serde_json::to_value(
                serde_json::from_value::<SafeDeleteDeclarationRequest>(fixture.clone()).unwrap(),
            ),
            "mutation_request" => serde_json::to_value(
                serde_json::from_value::<SafeDeleteMutationRequest>(fixture.clone()).unwrap(),
            ),
            _ => serde_json::to_value(
                serde_json::from_value::<SafeDeleteMutationResult>(fixture.clone()).unwrap(),
            ),
        }
        .unwrap();
        assert_eq!(serialized, fixture, "fixture {name}");
    }
}

#[test]
fn literal_fixtures_round_trip_for_every_public_report_status() {
    let reports = [
        json!({"status":"blocked","blocking_reason":"not_found","references":[],"proposed_edits":[],"affected_files":[]}),
        json!({"status":"previewed","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"previewed","per_file":[]}}),
        json!({"status":"applied","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"applied","per_file":[]}}),
        json!({"status":"partial_commit","declaration":declaration_json(),"references":[],"proposed_edits":[cleanup_edit_json()],"affected_files":["src/consumer.rs"],"mutation_result":{"status":"partial_commit","per_file":[{"path":"src/consumer.rs","outcome":"failed","error":"filesystem_error"}]}}),
    ];
    for fixture in reports {
        let report: SafeDeleteReport = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(report).unwrap(), fixture);
    }
}

#[test]
fn malformed_paths_hashes_ranges_fields_and_outcome_shapes_are_rejected_inbound() {
    let invalid_paths = [
        "",
        "/tmp/file.rs",
        "C:/file.rs",
        "C:src/lib.rs",
        "C:",
        r"src\file.rs",
        "src/../file.rs",
        "src//file.rs",
        "src/./file.rs",
    ];
    for path in invalid_paths {
        assert!(
            serde_json::from_value::<SafeDeleteRequest>(json!({
                "path": path,
                "symbol_name": "remove_me",
                "dry_run": false
            }))
            .is_err(),
            "accepted invalid path {path:?}"
        );
    }

    for base_hash in [
        "abcd",
        "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
    ] {
        let mut fixture = serde_json::to_value(declaration()).unwrap();
        fixture["base_hash"] = json!(base_hash);
        assert!(serde_json::from_value::<SafeDeleteDeclarationPlan>(fixture).is_err());
    }

    let mut reversed_span = serde_json::to_value(declaration()).unwrap();
    reversed_span["deletion_span"] = json!({"start_byte": 52, "end_byte": 51});
    assert!(serde_json::from_value::<SafeDeleteDeclarationPlan>(reversed_span).is_err());

    assert!(
        serde_json::from_value::<SafeDeleteSemanticOutcome>(json!({
            "status": "complete",
            "plan": serde_json::to_value(semantic_plan(CleanupCoverage::Incomplete)).unwrap()
        }))
        .is_err()
    );
}

#[test]
fn missing_null_and_unknown_nested_fields_are_rejected_inbound() {
    assert_inbound_rejected::<SafeDeleteRequest>(
        json!({"path":"src/lib.rs","symbol_name":null,"dry_run":false}),
        "request symbol_name must not be null",
    );
    assert_inbound_rejected::<SafeDeleteDeclarationPlan>(
        json!({
            "path":"src/lib.rs", "symbol_name":"remove_me", "kind":"function",
            "position":{"line":3}, "deletion_span":{"start_byte":24,"end_byte":51},
            "base_hash":HASH_A
        }),
        "declaration position requires character",
    );
    assert_inbound_rejected::<SafeDeleteDeclarationPlan>(
        json!({
            "path":"src/lib.rs", "symbol_name":"remove_me", "kind":"function",
            "position":{"line":3,"character":7,"column":7},
            "deletion_span":{"start_byte":24,"end_byte":51}, "base_hash":HASH_A
        }),
        "declaration position rejects unknown nested fields",
    );
    assert_inbound_rejected::<SafeDeleteCleanupEdit>(
        json!({
            "path":"src/consumer.rs", "start_byte":80, "end_byte":89,
            "replacement":null, "base_hash":HASH_B
        }),
        "cleanup replacement must not be null",
    );
    assert_inbound_rejected::<SafeDeleteMutationResult>(
        json!({"status":"applied","per_file":[{"path":"src/lib.rs","outcome":"committed","extra":true}]}),
        "file outcome rejects unknown nested fields",
    );

    let null_kind = json!({"path":"src/lib.rs","symbol_name":"remove_me","kind":null});
    assert_inbound_rejected::<SafeDeleteRequest>(
        null_kind.clone(),
        "request kind must be omitted rather than null",
    );
    assert_inbound_rejected::<SafeDeleteDeclarationRequest>(
        null_kind,
        "declaration request kind must be omitted rather than null",
    );
    assert_inbound_rejected::<SafeDeleteMutationResult>(
        json!({"status":"rejected","rejection":null,"per_file":[]}),
        "mutation rejection must be omitted rather than null",
    );
    assert_inbound_rejected::<SafeDeleteMutationResult>(
        json!({"status":"applied","per_file":[{"path":"src/lib.rs","outcome":"committed","error":null}]}),
        "file error must be omitted rather than null",
    );
    assert_inbound_rejected::<SafeDeleteReport>(
        json!({"status":"blocked","blocking_reason":null,"references":[],"proposed_edits":[],"affected_files":[]}),
        "report blocking reason must be omitted rather than null",
    );
    assert_inbound_rejected::<SafeDeleteReport>(
        json!({"status":"blocked","blocking_reason":"not_found","declaration":null,"references":[],"proposed_edits":[],"affected_files":[]}),
        "report declaration must be omitted rather than null",
    );
    assert_inbound_rejected::<SafeDeleteReport>(
        json!({"status":"blocked","blocking_reason":"not_found","references":[],"proposed_edits":[],"affected_files":[],"mutation_result":null}),
        "report mutation result must be omitted rather than null",
    );
}

#[test]
fn every_path_symbol_kind_and_range_invariant_is_rejected_inbound() {
    for (field, value) in [
        ("path", json!("src/./lib.rs")),
        ("symbol_name", json!("")),
        ("kind", json!("unknown_kind")),
    ] {
        let mut fixture = json!({
            "path":"src/lib.rs", "symbol_name":"remove_me", "kind":"function",
            "position":{"line":3,"character":7},
            "deletion_span":{"start_byte":24,"end_byte":51}, "base_hash":HASH_A
        });
        fixture[field] = value;
        assert_inbound_rejected::<SafeDeleteDeclarationPlan>(fixture, field);
    }

    assert_inbound_rejected::<SafeDeleteReference>(
        json!({
            "path":"src/consumer.rs", "start":{"line":8,"character":13},
            "end":{"line":8,"character":4}, "base_hash":HASH_B
        }),
        "reference end must not precede start",
    );
    assert_inbound_rejected::<SafeDeleteMutationRequest>(
        json!({"edits":[],"dry_run":false}),
        "mutation request requires at least one edit",
    );
}

#[test]
fn every_mutation_rejection_code_requires_unattempted_files_inbound() {
    for rejection in [
        "invalid_path",
        "invalid_range",
        "overlapping_edits",
        "cas_conflict",
    ] {
        let fixture = json!({
            "status":"rejected", "rejection":rejection,
            "per_file":[{"path":"src/lib.rs","outcome":"unattempted"}]
        });
        let result: SafeDeleteMutationResult = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            fixture,
            "{rejection}"
        );
    }

    for malformed in [
        json!({"status":"rejected","per_file":[{"path":"src/lib.rs","outcome":"unattempted"}]}),
        json!({"status":"rejected","rejection":"invalid_path","per_file":[{"path":"src/lib.rs","outcome":"failed"}]}),
        json!({"status":"rejected","rejection":"invalid_path","per_file":[{"path":"src/lib.rs","outcome":"unattempted","error":"filesystem_error"}]}),
    ] {
        assert_inbound_rejected::<SafeDeleteMutationResult>(malformed, "rejected mutation matrix");
    }
}

#[test]
fn every_non_rejected_mutation_status_forbids_the_other_status_outcomes_inbound() {
    for malformed in [
        json!({"status":"previewed","rejection":"invalid_path","per_file":[{"path":"src/lib.rs","outcome":"unattempted"}]}),
        json!({"status":"previewed","per_file":[{"path":"src/lib.rs","outcome":"committed"}]}),
        json!({"status":"applied","per_file":[{"path":"src/lib.rs","outcome":"failed","error":"filesystem_error"}]}),
        json!({"status":"partial_commit","per_file":[{"path":"src/a.rs","outcome":"committed"}]}),
        json!({"status":"partial_commit","per_file":[{"path":"src/a.rs","outcome":"failed"}]}),
        json!({"status":"partial_commit","rejection":"cas_conflict","per_file":[{"path":"src/a.rs","outcome":"failed","error":"persistence_error"}]}),
    ] {
        assert_inbound_rejected::<SafeDeleteMutationResult>(
            malformed,
            "non-rejected mutation status matrix",
        );
    }
}

#[test]
fn every_mutation_status_matrix_row_is_enforced_outbound() {
    let valid = [
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Previewed,
            rejection: None,
            per_file: vec![file_outcome(
                "src/lib.rs",
                SafeDeleteFileOutcomeStatus::Unattempted,
                None,
            )],
        },
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Applied,
            rejection: None,
            per_file: vec![file_outcome(
                "src/lib.rs",
                SafeDeleteFileOutcomeStatus::Committed,
                None,
            )],
        },
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Rejected,
            rejection: Some(SafeDeleteRejectionCode::InvalidRange),
            per_file: vec![file_outcome(
                "src/lib.rs",
                SafeDeleteFileOutcomeStatus::Unattempted,
                None,
            )],
        },
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::PartialCommit,
            rejection: None,
            per_file: vec![file_outcome(
                "src/lib.rs",
                SafeDeleteFileOutcomeStatus::Failed,
                Some(SafeDeleteFileError::FilesystemError),
            )],
        },
    ];
    for result in valid {
        assert!(serde_json::to_value(result).is_ok());
    }

    let invalid = [
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Previewed,
            rejection: None,
            per_file: vec![file_outcome(
                "src/lib.rs",
                SafeDeleteFileOutcomeStatus::Committed,
                None,
            )],
        },
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Applied,
            rejection: None,
            per_file: vec![file_outcome(
                "src/lib.rs",
                SafeDeleteFileOutcomeStatus::Unattempted,
                None,
            )],
        },
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Rejected,
            rejection: Some(SafeDeleteRejectionCode::CasConflict),
            per_file: vec![file_outcome(
                "src/lib.rs",
                SafeDeleteFileOutcomeStatus::Committed,
                None,
            )],
        },
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::PartialCommit,
            rejection: None,
            per_file: vec![
                file_outcome(
                    "src/a.rs",
                    SafeDeleteFileOutcomeStatus::Failed,
                    Some(SafeDeleteFileError::FilesystemError),
                ),
                file_outcome("src/b.rs", SafeDeleteFileOutcomeStatus::Committed, None),
            ],
        },
        SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::PartialCommit,
            rejection: None,
            per_file: vec![
                file_outcome(
                    "src/a.rs",
                    SafeDeleteFileOutcomeStatus::Failed,
                    Some(SafeDeleteFileError::FilesystemError),
                ),
                file_outcome(
                    "src/b.rs",
                    SafeDeleteFileOutcomeStatus::Failed,
                    Some(SafeDeleteFileError::PersistenceError),
                ),
            ],
        },
    ];
    for result in invalid {
        assert_outbound_rejected(&result);
    }
}

#[test]
fn invalid_path_forms_are_rejected_on_outbound_serialization() {
    for path in [
        "",
        "/tmp/file.rs",
        "C:/file.rs",
        "C:src/lib.rs",
        "C:",
        r"src\file.rs",
        "src/../file.rs",
        "src//file.rs",
        "src/./file.rs",
        "src/\0file.rs",
    ] {
        let request = SafeDeleteRequest {
            path: path.into(),
            symbol_name: "remove_me".into(),
            kind: Some("function".into()),
            dry_run: false,
        };
        assert_outbound_rejected(&request);
    }
}

#[test]
fn malformed_constructed_values_and_contradictory_report_matrix_are_rejected_outbound() {
    let invalid_declaration = SafeDeleteDeclarationPlan {
        path: "../outside.rs".into(),
        base_hash: "not-a-hash".into(),
        deletion_span: SafeDeleteByteSpan {
            start_byte: 9,
            end_byte: 2,
        },
        ..declaration()
    };
    assert_outbound_rejected(&invalid_declaration);

    let coverage_mismatch = SafeDeleteSemanticOutcome::Complete {
        plan: semantic_plan(CleanupCoverage::Incomplete),
    };
    assert_outbound_rejected(&coverage_mismatch);

    let contradictory = SafeDeleteReport {
        status: SafeDeleteStatus::Applied,
        blocking_reason: Some(SafeDeleteBlockingReason::BackendError),
        declaration: None,
        references: vec![reference()],
        proposed_edits: vec![cleanup_edit()],
        affected_files: vec!["src/different.rs".into()],
        mutation_result: Some(SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Previewed,
            rejection: None,
            per_file: vec![file_outcome(
                "src/consumer.rs",
                SafeDeleteFileOutcomeStatus::Unattempted,
                None,
            )],
        }),
    };
    assert_outbound_rejected(&contradictory);

    let duplicate_paths = SafeDeleteMutationResult {
        status: SafeDeleteMutationStatus::Applied,
        rejection: None,
        per_file: vec![
            file_outcome("src/lib.rs", SafeDeleteFileOutcomeStatus::Committed, None),
            file_outcome("src/lib.rs", SafeDeleteFileOutcomeStatus::Committed, None),
        ],
    };
    assert_outbound_rejected(&duplicate_paths);
}

#[test]
fn report_status_and_mutation_status_matrix_is_enforced_inbound() {
    let reports = [
        json!({"status":"blocked","blocking_reason":"invalid_cleanup","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"rejected","rejection":"invalid_range","per_file":[]}}),
        json!({"status":"previewed","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"previewed","per_file":[]}}),
        json!({"status":"applied","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"applied","per_file":[]}}),
        json!({"status":"partial_commit","declaration":declaration_json(),"references":[],"proposed_edits":[cleanup_edit_json()],"affected_files":["src/consumer.rs"],"mutation_result":{"status":"partial_commit","per_file":[{"path":"src/consumer.rs","outcome":"failed","error":"persistence_error"}]}}),
    ];
    for report in reports {
        let decoded: SafeDeleteReport = serde_json::from_value(report.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), report);
    }

    for malformed in [
        json!({"status":"blocked","references":[],"proposed_edits":[],"affected_files":[]}),
        json!({"status":"blocked","blocking_reason":"not_found","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[]}),
        json!({"status":"blocked","blocking_reason":"backend_error","references":[],"proposed_edits":[],"affected_files":[]}),
        json!({"status":"blocked","blocking_reason":"backend_error","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"rejected","rejection":"cas_conflict","per_file":[]}}),
        json!({"status":"blocked","blocking_reason":"cas_conflict","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"rejected","rejection":"invalid_range","per_file":[]}}),
        json!({"status":"previewed","references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"previewed","per_file":[]}}),
        json!({"status":"applied","declaration":declaration_json(),"references":[],"proposed_edits":[],"affected_files":[],"mutation_result":{"status":"previewed","per_file":[]}}),
        json!({"status":"partial_commit","declaration":declaration_json(),"references":[],"proposed_edits":[cleanup_edit_json()],"affected_files":["src/consumer.rs"],"mutation_result":{"status":"partial_commit","per_file":[{"path":"src/other.rs","outcome":"failed","error":"filesystem_error"}]}}),
    ] {
        assert_inbound_rejected::<SafeDeleteReport>(malformed, "report and mutation status matrix");
    }
}

#[test]
fn report_status_and_mutation_status_matrix_is_enforced_outbound() {
    let valid = SafeDeleteReport {
        status: SafeDeleteStatus::Blocked,
        blocking_reason: Some(SafeDeleteBlockingReason::CasConflict),
        declaration: Some(declaration()),
        references: vec![],
        proposed_edits: vec![],
        affected_files: vec![],
        mutation_result: Some(SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Rejected,
            rejection: Some(SafeDeleteRejectionCode::CasConflict),
            per_file: vec![],
        }),
    };
    assert!(serde_json::to_value(valid).is_ok());

    let invalid = [
        SafeDeleteReport {
            status: SafeDeleteStatus::Blocked,
            blocking_reason: None,
            declaration: None,
            references: vec![],
            proposed_edits: vec![],
            affected_files: vec![],
            mutation_result: None,
        },
        SafeDeleteReport {
            status: SafeDeleteStatus::Previewed,
            blocking_reason: None,
            declaration: None,
            references: vec![],
            proposed_edits: vec![],
            affected_files: vec![],
            mutation_result: None,
        },
        SafeDeleteReport {
            status: SafeDeleteStatus::Blocked,
            blocking_reason: Some(SafeDeleteBlockingReason::BackendError),
            declaration: None,
            references: vec![],
            proposed_edits: vec![],
            affected_files: vec![],
            mutation_result: None,
        },
        SafeDeleteReport {
            status: SafeDeleteStatus::Blocked,
            blocking_reason: Some(SafeDeleteBlockingReason::InvalidCleanup),
            declaration: Some(declaration()),
            references: vec![],
            proposed_edits: vec![],
            affected_files: vec![],
            mutation_result: Some(SafeDeleteMutationResult {
                status: SafeDeleteMutationStatus::Rejected,
                rejection: Some(SafeDeleteRejectionCode::CasConflict),
                per_file: vec![],
            }),
        },
        SafeDeleteReport {
            status: SafeDeleteStatus::Applied,
            blocking_reason: Some(SafeDeleteBlockingReason::BackendError),
            declaration: Some(declaration()),
            references: vec![],
            proposed_edits: vec![],
            affected_files: vec![],
            mutation_result: Some(SafeDeleteMutationResult {
                status: SafeDeleteMutationStatus::Applied,
                rejection: None,
                per_file: vec![],
            }),
        },
        SafeDeleteReport {
            status: SafeDeleteStatus::PartialCommit,
            blocking_reason: None,
            declaration: Some(declaration()),
            references: vec![],
            proposed_edits: vec![cleanup_edit()],
            affected_files: vec!["src/not-consumer.rs".into()],
            mutation_result: Some(SafeDeleteMutationResult {
                status: SafeDeleteMutationStatus::PartialCommit,
                rejection: None,
                per_file: vec![file_outcome(
                    "src/consumer.rs",
                    SafeDeleteFileOutcomeStatus::Failed,
                    Some(SafeDeleteFileError::FilesystemError),
                )],
            }),
        },
    ];
    for report in invalid {
        assert_outbound_rejected(&report);
    }
}
