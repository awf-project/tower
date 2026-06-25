use extension_protocol::envelope::JsonRpcRequest;
use extension_protocol::{
    AnchoredSymbolEditError, AnchoredSymbolEditErrorCode, AnchoredSymbolEditRequest,
    AnchoredSymbolEditResult, ApplyEditsHostCallTextEdit, Capability, ExtensionManifest, HostCall,
    Location, LspImplementationRequest, LspImplementationResult, PerFileEditResult, RenameError,
    RenameErrorCode, RenamePreview, RenameRequest, RenameResult, SymbolCandidate,
    WorkspaceApplyEditsError, WorkspaceApplyEditsErrorCode, WorkspaceApplyEditsRequest,
    WorkspaceApplyEditsResult, WorkspaceEditSpan,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + DeserializeOwned,
{
    serde_json::from_value(serde_json::to_value(value).expect("serialize")).expect("deserialize")
}

fn edit_span() -> WorkspaceEditSpan {
    WorkspaceEditSpan {
        path: "src/lib.rs".to_owned(),
        start_byte: 10,
        end_byte: 20,
        replacement: "new_symbol".to_owned(),
        base_hash: Some("0123456789abcdef".to_owned()),
    }
}

fn edit_error() -> WorkspaceApplyEditsError {
    WorkspaceApplyEditsError {
        code: WorkspaceApplyEditsErrorCode::Conflict,
        message: "file changed before apply".to_owned(),
        path: Some("src/lib.rs".to_owned()),
    }
}

fn per_file_result() -> PerFileEditResult {
    PerFileEditResult {
        path: "src/lib.rs".to_owned(),
        applied: true,
        edits_applied: 1,
        edits_skipped: 0,
        new_version: Some("fedcba9876543210".to_owned()),
        preview: Some("-old\n+new\n".to_owned()),
        error: None,
    }
}

fn symbol_candidate() -> SymbolCandidate {
    SymbolCandidate {
        path: "src/lib.rs".to_owned(),
        kind: "function".to_owned(),
        name: "old_symbol".to_owned(),
        start_byte: 10,
        end_byte: 42,
        start_row: 3,
        end_row: 5,
    }
}

#[test]
fn semantic_edit_contracts() {
    let batch = WorkspaceApplyEditsRequest {
        edits: vec![edit_span()],
        dry_run: Some(true),
    };

    assert_eq!(
        serde_json::to_value(batch).expect("serialize batch apply edits"),
        json!({
            "edits": [{
                "path": "src/lib.rs",
                "start_byte": 10,
                "end_byte": 20,
                "replacement": "new_symbol",
                "base_hash": "0123456789abcdef"
            }],
            "dry_run": true
        })
    );
    assert_eq!(
        serde_json::to_value(Capability::RequestApplyEdits).expect("serialize capability"),
        json!("request_apply_edits")
    );
    assert_eq!(
        serde_json::to_value(RenameErrorCode::BackendError).expect("serialize rename error code"),
        json!("backend_error")
    );
    assert_eq!(
        serde_json::to_value(AnchoredSymbolEditErrorCode::AmbiguousSymbol)
            .expect("serialize anchored edit error code"),
        json!("ambiguous_symbol")
    );
}

#[test]
fn workspace_apply_edits_request_serializes_as_the_workspace_apply_edits_host_call_payload() {
    let request = WorkspaceApplyEditsRequest {
        edits: vec![edit_span()],
        dry_run: Some(true),
    };

    let value = serde_json::to_value(&request).expect("serialize request");
    assert_eq!(
        value,
        json!({
            "edits": [{
                "path": "src/lib.rs",
                "start_byte": 10,
                "end_byte": 20,
                "replacement": "new_symbol",
                "base_hash": "0123456789abcdef"
            }],
            "dry_run": true
        })
    );
    assert_eq!(round_trip::<WorkspaceApplyEditsRequest>(&request), request);

    let legacy = HostCall::RequestApplyEdits {
        path: "src/lib.rs".to_owned(),
        expected_version: "0123456789abcdef".to_owned(),
        edits: vec![ApplyEditsHostCallTextEdit {
            start_byte: 10,
            end_byte: 20,
            replacement: "new_symbol".to_owned(),
        }],
        dry_run: true,
    };
    assert!(matches!(
        round_trip::<HostCall>(&legacy),
        HostCall::RequestApplyEdits { .. }
    ));
}

#[test]
fn workspace_edit_span_serializes_fields_exactly_as_path_start_byte_end_byte_replacement_and_optional_base_hash()
 {
    let value = serde_json::to_value(edit_span()).expect("serialize span");

    assert_eq!(
        value,
        json!({
            "path": "src/lib.rs",
            "start_byte": 10,
            "end_byte": 20,
            "replacement": "new_symbol",
            "base_hash": "0123456789abcdef"
        })
    );
}

#[test]
fn workspace_apply_edits_result_serializes_fields_exactly_as_files_changed_and_per_file() {
    let result = WorkspaceApplyEditsResult {
        files_changed: 1,
        per_file: vec![per_file_result()],
    };

    let value = serde_json::to_value(&result).expect("serialize result");
    assert_eq!(value["files_changed"], 1);
    assert_eq!(value["per_file"][0]["path"], "src/lib.rs");
    assert_eq!(value.as_object().expect("object").len(), 2);
    assert_eq!(round_trip::<WorkspaceApplyEditsResult>(&result), result);
}

#[test]
fn per_file_edit_result_serializes_fields_exactly_as_path_applied_counts_optional_version_preview_and_error()
 {
    let result = PerFileEditResult {
        error: Some(edit_error()),
        ..per_file_result()
    };

    assert_eq!(
        serde_json::to_value(result).expect("serialize per-file result"),
        json!({
            "path": "src/lib.rs",
            "applied": true,
            "edits_applied": 1,
            "edits_skipped": 0,
            "new_version": "fedcba9876543210",
            "preview": "-old\n+new\n",
            "error": {
                "code": "cas_conflict",
                "message": "file changed before apply",
                "path": "src/lib.rs"
            }
        })
    );
}

#[test]
fn workspace_apply_edits_error_serializes_fields_exactly_as_code_message_and_optional_path() {
    assert_eq!(
        serde_json::to_value(edit_error()).expect("serialize apply-edits error"),
        json!({
            "code": "cas_conflict",
            "message": "file changed before apply",
            "path": "src/lib.rs"
        })
    );
}

#[test]
fn workspace_apply_edits_error_code_includes_stable_variants_and_snake_case_json_strings() {
    let cases = [
        (
            WorkspaceApplyEditsErrorCode::CapabilityDenied,
            "capability_denied",
        ),
        (WorkspaceApplyEditsErrorCode::InvalidPath, "invalid_path"),
        (WorkspaceApplyEditsErrorCode::EmptyEdits, "empty_edit_list"),
        (
            WorkspaceApplyEditsErrorCode::OverlappingSpans,
            "overlapping_edits",
        ),
        (WorkspaceApplyEditsErrorCode::InvalidRange, "invalid_range"),
        (WorkspaceApplyEditsErrorCode::Conflict, "cas_conflict"),
        (
            WorkspaceApplyEditsErrorCode::Unsupported,
            "unsupported_operation",
        ),
        (WorkspaceApplyEditsErrorCode::Internal, "backend_error"),
    ];

    for (code, expected) in cases {
        assert_eq!(
            serde_json::to_value(&code).expect("serialize code"),
            json!(expected)
        );
        assert_eq!(
            serde_json::from_value::<WorkspaceApplyEditsErrorCode>(json!(expected))
                .expect("deserialize code"),
            code
        );
    }
}

#[test]
fn json_rpc_envelope_method_workspace_apply_edits_routes_batch_params_payload_and_host_call_remains_params_only()
 {
    let params = WorkspaceApplyEditsRequest {
        edits: vec![edit_span()],
        dry_run: Some(true),
    };
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_owned(),
        id: Some(json!(7)),
        method: "workspace/applyEdits".to_owned(),
        params: Some(serde_json::to_value(&params).expect("serialize params")),
    };

    assert_eq!(
        serde_json::to_value(request).expect("serialize envelope"),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "workspace/applyEdits",
            "params": {
                "edits": [{
                    "path": "src/lib.rs",
                    "start_byte": 10,
                    "end_byte": 20,
                    "replacement": "new_symbol",
                    "base_hash": "0123456789abcdef"
                }],
                "dry_run": true
            }
        })
    );

    let host_call = serde_json::to_value(HostCall::RequestApplyEdits {
        path: "src/lib.rs".to_owned(),
        expected_version: "0123456789abcdef".to_owned(),
        edits: vec![ApplyEditsHostCallTextEdit {
            start_byte: 10,
            end_byte: 20,
            replacement: "new_symbol".to_owned(),
        }],
        dry_run: true,
    })
    .expect("serialize host call");
    assert!(host_call.get("method").is_none());
}

#[test]
fn capability_request_apply_edits_serializes_as_request_apply_edits_and_rejects_unknown_capability_strings()
 {
    assert_eq!(
        serde_json::to_value(Capability::RequestApplyEdits).expect("serialize capability"),
        json!("request_apply_edits")
    );
    assert_eq!(
        serde_json::from_value::<Capability>(json!("request_apply_edits"))
            .expect("deserialize capability"),
        Capability::RequestApplyEdits
    );
    assert!(serde_json::from_value::<Capability>(json!("unknown_capability")).is_err());
}

#[test]
fn lsp_implementation_request_serializes_fields_exactly_as_path_line_and_character() {
    let request = LspImplementationRequest {
        path: "src/lib.rs".to_owned(),
        line: 4,
        character: 12,
    };

    assert_eq!(
        serde_json::to_value(request).expect("serialize implementation request"),
        json!({
            "path": "src/lib.rs",
            "line": 4,
            "character": 12
        })
    );
}

#[test]
fn lsp_implementation_result_serializes_fields_exactly_as_supported_and_locations() {
    let result = LspImplementationResult {
        supported: true,
        locations: vec![Location {
            path: "src/lib.rs".to_owned(),
            line: 4,
            character: 12,
            end_line: 4,
            end_character: 20,
        }],
    };

    assert_eq!(
        serde_json::to_value(result).expect("serialize implementation result"),
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
}

#[test]
fn rename_request_serializes_fields_exactly_as_path_line_character_new_name_and_optional_dry_run() {
    let request = RenameRequest {
        path: "src/lib.rs".to_owned(),
        line: 4,
        character: 12,
        new_name: "new_symbol".to_owned(),
        dry_run: Some(true),
    };

    assert_eq!(
        serde_json::to_value(request).expect("serialize rename request"),
        json!({
            "path": "src/lib.rs",
            "line": 4,
            "character": 12,
            "new_name": "new_symbol",
            "dry_run": true
        })
    );
}

#[test]
fn rename_result_serializes_fields_exactly_as_applied_files_changed_spans_preview_and_per_file() {
    let result = RenameResult {
        applied: true,
        files_changed: 1,
        spans: vec![edit_span()],
        preview: Some("-old\n+new\n".to_owned()),
        per_file: vec![per_file_result()],
    };

    let value = serde_json::to_value(&result).expect("serialize rename result");
    assert_eq!(value["applied"], true);
    assert_eq!(value["files_changed"], 1);
    assert_eq!(value["spans"][0]["path"], "src/lib.rs");
    assert_eq!(value["preview"], "-old\n+new\n");
    assert_eq!(value["per_file"][0]["path"], "src/lib.rs");
    assert_eq!(value.as_object().expect("object").len(), 5);
    assert_eq!(round_trip::<RenameResult>(&result), result);
}

#[test]
fn rename_preview_serializes_fields_exactly_as_spans_preview_and_per_file() {
    let preview = RenamePreview {
        spans: vec![edit_span()],
        preview: "-old\n+new\n".to_owned(),
        per_file: vec![per_file_result()],
    };

    let value = serde_json::to_value(&preview).expect("serialize rename preview");
    assert_eq!(value["spans"][0]["path"], "src/lib.rs");
    assert_eq!(value["preview"], "-old\n+new\n");
    assert_eq!(value["per_file"][0]["path"], "src/lib.rs");
    assert_eq!(value.as_object().expect("object").len(), 3);
    assert_eq!(round_trip::<RenamePreview>(&preview), preview);
}

#[test]
fn rename_error_serializes_fields_exactly_as_code_message_and_optional_path() {
    let error = RenameError {
        code: RenameErrorCode::UnsupportedWorkspaceEdit,
        message: "server returned an unsupported workspace edit".to_owned(),
        path: Some("src/lib.rs".to_owned()),
    };

    assert_eq!(
        serde_json::to_value(error).expect("serialize rename error"),
        json!({
            "code": "unsupported_workspace_edit",
            "message": "server returned an unsupported workspace edit",
            "path": "src/lib.rs"
        })
    );
}

#[test]
fn rename_error_code_includes_stable_variants_and_snake_case_json_strings() {
    let cases = [
        (RenameErrorCode::NotRenameable, "not_renameable"),
        (
            RenameErrorCode::UnsupportedWorkspaceEdit,
            "unsupported_workspace_edit",
        ),
        (RenameErrorCode::UnsupportedLanguage, "unsupported_language"),
        (RenameErrorCode::InvalidRange, "invalid_range"),
        (RenameErrorCode::BackendError, "backend_error"),
    ];

    for (code, expected) in cases {
        assert_eq!(
            serde_json::to_value(&code).expect("serialize code"),
            json!(expected)
        );
        assert_eq!(
            serde_json::from_value::<RenameErrorCode>(json!(expected)).expect("deserialize code"),
            code
        );
    }
}

#[test]
fn anchored_symbol_edit_request_serializes_fields_exactly_as_path_symbol_name_kind_replacement_and_dry_run()
 {
    let request = AnchoredSymbolEditRequest {
        path: "src/lib.rs".to_owned(),
        symbol_name: "old_symbol".to_owned(),
        kind: Some("function".to_owned()),
        replacement: Some("fn old_symbol() {}".to_owned()),
        dry_run: Some(true),
    };

    assert_eq!(
        serde_json::to_value(request).expect("serialize anchored symbol edit request"),
        json!({
            "path": "src/lib.rs",
            "symbol_name": "old_symbol",
            "kind": "function",
            "replacement": "fn old_symbol() {}",
            "dry_run": true
        })
    );
}

#[test]
fn anchored_symbol_edit_result_serializes_fields_exactly_as_applied_files_changed_span_preview_and_per_file()
 {
    let result = AnchoredSymbolEditResult {
        applied: true,
        files_changed: 1,
        span: Some(edit_span()),
        preview: Some("-old\n+new\n".to_owned()),
        per_file: vec![per_file_result()],
    };

    let value = serde_json::to_value(&result).expect("serialize anchored symbol edit result");
    assert_eq!(value["applied"], true);
    assert_eq!(value["files_changed"], 1);
    assert_eq!(value["span"]["path"], "src/lib.rs");
    assert_eq!(value["preview"], "-old\n+new\n");
    assert_eq!(value["per_file"][0]["path"], "src/lib.rs");
    assert_eq!(round_trip::<AnchoredSymbolEditResult>(&result), result);
}

#[test]
fn anchored_symbol_edit_error_serializes_fields_exactly_as_code_message_optional_path_and_optional_candidates()
 {
    let error = AnchoredSymbolEditError {
        code: AnchoredSymbolEditErrorCode::AmbiguousSymbol,
        message: "multiple matching symbols".to_owned(),
        path: Some("src/lib.rs".to_owned()),
        candidates: Some(vec![symbol_candidate()]),
    };

    assert_eq!(
        serde_json::to_value(error).expect("serialize anchored symbol edit error"),
        json!({
            "code": "ambiguous_symbol",
            "message": "multiple matching symbols",
            "path": "src/lib.rs",
            "candidates": [{
                "path": "src/lib.rs",
                "kind": "function",
                "name": "old_symbol",
                "start_byte": 10,
                "end_byte": 42,
                "start_row": 3,
                "end_row": 5
            }]
        })
    );
}

#[test]
fn anchored_symbol_edit_error_code_includes_stable_variants_and_snake_case_json_strings() {
    let cases = [
        (AnchoredSymbolEditErrorCode::NotFound, "not_found"),
        (
            AnchoredSymbolEditErrorCode::AmbiguousSymbol,
            "ambiguous_symbol",
        ),
        (
            AnchoredSymbolEditErrorCode::UnsupportedLanguage,
            "unsupported_language",
        ),
        (AnchoredSymbolEditErrorCode::InvalidRange, "invalid_range"),
        (AnchoredSymbolEditErrorCode::BackendError, "backend_error"),
    ];

    for (code, expected) in cases {
        assert_eq!(
            serde_json::to_value(&code).expect("serialize code"),
            json!(expected)
        );
        assert_eq!(
            serde_json::from_value::<AnchoredSymbolEditErrorCode>(json!(expected))
                .expect("deserialize code"),
            code
        );
    }
}

#[test]
fn symbol_candidate_serializes_fields_exactly_as_path_kind_name_byte_span_and_rows() {
    assert_eq!(
        serde_json::to_value(symbol_candidate()).expect("serialize symbol candidate"),
        json!({
            "path": "src/lib.rs",
            "kind": "function",
            "name": "old_symbol",
            "start_byte": 10,
            "end_byte": 42,
            "start_row": 3,
            "end_row": 5
        })
    );
}

#[test]
fn protocol_tests_assert_representative_json_for_semantic_edit_contracts_and_all_stable_error_code_strings()
 {
    assert_eq!(
        serde_json::to_value(WorkspaceApplyEditsRequest {
            edits: vec![edit_span()],
            dry_run: Some(true),
        })
        .expect("serialize batch apply edits"),
        json!({
            "edits": [{
                "path": "src/lib.rs",
                "start_byte": 10,
                "end_byte": 20,
                "replacement": "new_symbol",
                "base_hash": "0123456789abcdef"
            }],
            "dry_run": true
        })
    );
    assert_eq!(
        serde_json::to_value(Capability::RequestApplyEdits).expect("serialize capability"),
        json!("request_apply_edits")
    );
    assert_eq!(
        serde_json::to_value(LspImplementationRequest {
            path: "src/lib.rs".to_owned(),
            line: 4,
            character: 12,
        })
        .expect("serialize implementation request"),
        json!({"path": "src/lib.rs", "line": 4, "character": 12})
    );
    assert_eq!(
        serde_json::to_value(RenameErrorCode::UnsupportedWorkspaceEdit)
            .expect("serialize rename code"),
        json!("unsupported_workspace_edit")
    );
    assert_eq!(
        serde_json::to_value(AnchoredSymbolEditErrorCode::AmbiguousSymbol)
            .expect("serialize anchored code"),
        json!("ambiguous_symbol")
    );
    assert_eq!(
        serde_json::to_value(WorkspaceApplyEditsErrorCode::EmptyEdits)
            .expect("serialize apply code"),
        json!("empty_edit_list")
    );
}

#[test]
fn existing_host_call_and_manifest_serialization_tests_continue_to_pass_unchanged() {
    let read_file = serde_json::to_value(HostCall::ReadFile {
        path: "src/lib.rs".to_owned(),
    })
    .expect("serialize read file");
    assert_eq!(read_file, json!({"type": "ReadFile", "path": "src/lib.rs"}));

    let request_apply_edits = serde_json::to_value(HostCall::RequestApplyEdits {
        path: "src/lib.rs".to_owned(),
        expected_version: "0123456789abcdef".to_owned(),
        edits: vec![ApplyEditsHostCallTextEdit {
            start_byte: 10,
            end_byte: 20,
            replacement: "new_symbol".to_owned(),
        }],
        dry_run: true,
    })
    .expect("serialize legacy request apply edits");
    assert_eq!(
        request_apply_edits,
        json!({
            "type": "RequestApplyEdits",
            "path": "src/lib.rs",
            "expected_version": "0123456789abcdef",
            "edits": [{
                "start_byte": 10,
                "end_byte": 20,
                "replacement": "new_symbol"
            }],
            "dry_run": true
        })
    );

    let manifest: ExtensionManifest = toml::from_str(
        r#"
        name = "ast"
        version = "0.1.0"
        command = ["ast_extension"]

        [capabilities]
        required = ["read_file", "request_apply_edits"]
        "#,
    )
    .expect("parse manifest");
    assert_eq!(
        manifest.capabilities.required,
        vec!["read_file", "request_apply_edits"]
    );
}
