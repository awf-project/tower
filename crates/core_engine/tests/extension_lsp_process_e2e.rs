//! LSP extension **process-level** e2e tests.
//!
//! These tests spawn the actual `lsp_extension` binary via stdio and drive it
//! through the JSON-RPC protocol. They do NOT require a real language server.
//!
//! # What this verifies
//!
//! - **AC1/U1**: `lsp_extension` spawns, initialises, and declares the LSP tools
//!   (`diagnostics`, `definition`, `references`, `hover`, `implementations`,
//!   `rename`) over the protocol.
//! - **EV1**: `deliverEvent` for `fileChanged` and `fileDeleted` completes cleanly.
//! - **Diagnostics unsupported**: with no LSP configured, `diagnostics` returns
//!   `{supported: false}` rather than an error.
//! - **Push-response hazard regression** (KNOWN PROTOCOL HAZARDS §1): a
//!   response frame that arrives on stdin during an idle main-loop spin (i.e. the
//!   push thread sent `notify/resourceUpdated` and the host ACKed it) is silently
//!   discarded. The subsequent `invokeTool` call must NOT receive a spurious
//!   `ProtocolError`.
//! - **Concurrency stress** (KNOWN PROTOCOL HAZARDS §1 mandate): ≥ 20 parallel
//!   spawns, each running initialize → invokeTool → shutdown, with no race or
//!   deadlock.
//!
//! # Binary location
//!
//! `lsp_extension` is a `default-members` binary, so `cargo test --workspace`
//! compiles it into `target/debug/` before running these tests.

// Feature: F010

#![allow(clippy::pedantic)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost;
use core_engine::adapters::extension::{HostDeps, SidecarHostAdapter};
use core_engine::adapters::formatter::NoOpFormatQueue;
use core_engine::adapters::{InMemoryAstIndex, InMemoryFs};
use core_engine::domain::mutation::compute_content_version;
use core_engine::ports::FileSystemPort;
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{Event, ExtensionManifest, InitResult};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

// ── Helpers ───────────────────────────────────────────────────────────────────

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent() // crates/
        .unwrap()
        .parent() // tower/
        .unwrap()
        .to_path_buf()
}

#[test]
fn safe_delete_tool_advertises_the_exact_internal_name_and_request_schema() {
    let mut child = RawLspChild::spawn();
    let initialized = initialized_raw_lsp_child(&mut child);
    let tools = initialized["result"]["data"]["tools"]
        .as_array()
        .expect("initialized tools must be an array");
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "plan_safe_delete")
        .expect("plan_safe_delete must be advertised");
    let schema: serde_json::Value = serde_json::from_str(
        tool["schema_json"]
            .as_str()
            .expect("plan_safe_delete schema_json must be a string"),
    )
    .expect("plan_safe_delete schema must be valid JSON");

    assert_eq!(
        schema,
        serde_json::json!({
            "type": "object",
            "properties": {
                "declaration": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "symbol_name": {"type": "string"},
                        "kind": {"type": "string"},
                        "position": {
                            "type": "object",
                            "properties": {
                                "line": {"type": "integer", "minimum": 0},
                                "character": {"type": "integer", "minimum": 0}
                            },
                            "required": ["line", "character"],
                            "additionalProperties": false
                        },
                        "deletion_span": {
                            "type": "object",
                            "properties": {
                                "start_byte": {"type": "integer", "minimum": 0},
                                "end_byte": {"type": "integer", "minimum": 0}
                            },
                            "required": ["start_byte", "end_byte"],
                            "additionalProperties": false
                        },
                        "base_hash": {"type": "string"}
                    },
                    "required": ["path", "symbol_name", "kind", "position", "deletion_span", "base_hash"],
                    "additionalProperties": false
                }
            },
            "required": ["declaration"],
            "additionalProperties": false
        })
    );
}

#[test]
fn safe_delete_tool_dispatches_to_the_pool_and_serializes_the_exact_complete_payload() {
    let declaration = "fn old_name() {}\n";
    let consumer = "fn caller() { old_name(); }\n";
    let workspace = fake_lsp_workspace("safe_delete_complete");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    fs::write(workspace.path().join("src/consumer.rs"), consumer).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let (response, host_calls) = invoke_safe_delete(
        &mut child,
        4900,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration), ("src/consumer.rs", consumer)],
    );

    assert_eq!(
        response,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4900,
            "result": {
                "type": "ToolResult",
                "data": {
                    "status": "complete",
                    "plan": {
                        "references": [{
                            "path": "src/consumer.rs",
                            "start": {"line": 0, "character": 14},
                            "end": {"line": 0, "character": 22},
                            "base_hash": compute_content_version(consumer.as_bytes())
                        }],
                        "cleanup_edits": [{
                            "path": "src/consumer.rs",
                            "start_byte": 14,
                            "end_byte": 22,
                            "replacement": "",
                            "base_hash": compute_content_version(consumer.as_bytes())
                        }],
                        "coverage": "complete"
                    }
                }
            }
        })
    );
    assert_no_mutation_host_calls(&host_calls);
}

#[test]
fn safe_delete_tool_maps_missing_wrong_and_unknown_fields_to_invalid_params() {
    let mut child = RawLspChild::spawn();
    initialize_raw_lsp_child_bounded(&mut child);
    for (offset, params) in [
        serde_json::json!({}),
        serde_json::json!({"declaration": "wrong"}),
        serde_json::json!({"declaration": {
            "path": "src/lib.rs",
            "symbol_name": "old_name",
            "kind": "function",
            "position": {"line": 0, "character": 3},
            "deletion_span": {"start_byte": 0, "end_byte": 17},
            "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "unknown": true
        }}),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 4910 + offset as u64;
        child.write_frame(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "invokeTool",
            "params": {"name": "plan_safe_delete", "params": params}
        }));
        assert_eq!(
            child.read_frame_bounded()["error"]["code"],
            -32602,
            "invalid request case {offset} must use InvalidParams"
        );
    }
}

#[test]
fn safe_delete_tool_serializes_every_blocked_backend_outcome_without_private_text() {
    let declaration = "fn old_name() {}\n";
    let incomplete_plan = serde_json::json!({
        "status": "incomplete",
        "plan": {
            "references": [{
                "path": "src/lib.rs",
                "start": {"line": 0, "character": 3},
                "end": {"line": 0, "character": 11},
                "base_hash": compute_content_version(declaration.as_bytes()),
            }],
            "cleanup_edits": [],
            "coverage": "incomplete",
        },
    });
    for (offset, (mode, expected_data)) in [
        (
            "safe_delete_capability_unavailable",
            serde_json::json!({"status": "capability_unavailable"}),
        ),
        (
            "safe_delete_backend_timeout",
            serde_json::json!({"status": "backend_timeout"}),
        ),
        (
            "safe_delete_backend_crashed",
            serde_json::json!({"status": "backend_crashed"}),
        ),
        (
            "safe_delete_malformed_response",
            serde_json::json!({"status": "malformed_response"}),
        ),
        ("safe_delete_incomplete", incomplete_plan),
        (
            "safe_delete_invalid_location",
            serde_json::json!({"status": "invalid_location"}),
        ),
        (
            "safe_delete_workspace_external",
            serde_json::json!({"status": "workspace_external"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            4920 + offset as u64,
            safe_delete_request(declaration),
            &[("src/lib.rs", declaration)],
        );
        assert_eq!(
            safe_delete_data(&response),
            &expected_data,
            "mode {mode} must serialize the exact SafeDeleteSemanticOutcome DTO"
        );
        let rendered = response.to_string();
        for private in [
            "BACKEND_TEXT_SENTINEL",
            "/private/",
            "file:///external/",
            "credential",
        ] {
            assert!(
                !rendered.contains(private),
                "{mode} leaked {private}: {rendered}"
            );
        }
        assert!(host_calls.iter().all(|call| call["method"] != "host/log"));
        assert_no_mutation_host_calls(&host_calls);
    }
}

#[test]
fn safe_delete_tool_does_not_mutate_workspace_content_or_versions() {
    let declaration = "fn old_name() {}\n";
    let consumer = "fn caller() { old_name(); }\n";
    let workspace = fake_lsp_workspace("safe_delete_complete");
    let declaration_path = workspace.path().join("src/lib.rs");
    let consumer_path = workspace.path().join("src/consumer.rs");
    fs::write(&declaration_path, declaration).unwrap();
    fs::write(&consumer_path, consumer).unwrap();
    let before = [
        (
            fs::read(&declaration_path).unwrap(),
            compute_content_version(declaration.as_bytes()),
        ),
        (
            fs::read(&consumer_path).unwrap(),
            compute_content_version(consumer.as_bytes()),
        ),
    ];
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let (_, host_calls) = invoke_safe_delete(
        &mut child,
        4930,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration), ("src/consumer.rs", consumer)],
    );

    let after_declaration = fs::read(&declaration_path).unwrap();
    let after_consumer = fs::read(&consumer_path).unwrap();
    assert_eq!(after_declaration, before[0].0);
    assert_eq!(after_consumer, before[1].0);
    assert_eq!(compute_content_version(&after_declaration), before[0].1);
    assert_eq!(compute_content_version(&after_consumer), before[1].1);
    assert_no_mutation_host_calls(&host_calls);
}

#[test]
fn safe_delete_tool_does_not_log_a_backend_server_error() {
    // Each value below is injected through a real producer: the source-content
    // HostCall and the fixture's unexpected backend fault.  The request sentinel
    // is carried by the actual public request, not constructed for assertion only.
    let source_sentinel = "SOURCE_CONTENT_SENTINEL_7b88";
    let absolute_path_sentinel = "/private/ABSOLUTE_PATH_SENTINEL_5f14.rs";
    let external_uri_sentinel = "file:///external/URI_SENTINEL_39c1.rs";
    let credential_sentinel = "credential=SECRET_CREDENTIAL_SENTINEL";
    let replacement_sentinel = "REPLACEMENT_CONTENT_SENTINEL_52de";
    let request_sentinel = "SERIALIZED_REQUEST_RESULT_SENTINEL_91af";
    let backend_sentinel = "safe-delete fixture failure";
    let declaration = format!(
        "fn old_name() {{ /* {source_sentinel} {absolute_path_sentinel} {external_uri_sentinel} {credential_sentinel} {replacement_sentinel} */ }}\n"
    );
    let workspace = fake_lsp_workspace("safe_delete_pool_error_once");
    fs::write(workspace.path().join("src/lib.rs"), &declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let mut request = safe_delete_request(&declaration);
    request["declaration"]["symbol_name"] = serde_json::json!(request_sentinel);
    let (response, host_calls) =
        invoke_safe_delete(&mut child, 4940, request, &[("src/lib.rs", &declaration)]);
    assert_eq!(safe_delete_data(&response)["status"], "backend_crashed");
    let logs: Vec<_> = host_calls
        .iter()
        .filter(|call| call["method"] == "host/log")
        .collect();
    assert!(
        logs.is_empty(),
        "an expected backend server error must not emit host/log: {logs:?}"
    );
    let public_response = response.to_string();
    for private in [
        source_sentinel,
        absolute_path_sentinel,
        external_uri_sentinel,
        credential_sentinel,
        replacement_sentinel,
        request_sentinel,
        backend_sentinel,
    ] {
        assert!(
            !public_response.contains(private),
            "public response leaked {private}: {public_response}"
        );
    }
    assert_no_mutation_host_calls(&host_calls);
}

#[test]
fn safe_delete_tool_queues_inbound_requests_and_emits_no_late_mutation_frame() {
    let declaration = "fn old_name() {}\n";
    let workspace = fake_lsp_workspace("safe_delete_reference_free");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": 4950, "method": "invokeTool",
        "params": {"name": "plan_safe_delete", "params": safe_delete_request(declaration)}
    }));
    let read = child.read_frame_bounded();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": 4951, "method": "invokeTool",
        "params": {"name": "plan_safe_delete", "params": {}}
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": 4952, "method": "shutdown", "params": {}
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": read["id"], "result": declaration
    }));

    let first = child.read_frame_bounded();
    assert_eq!(first["id"], 4950);
    assert_eq!(safe_delete_data(&first)["status"], "complete");
    let queued = child.read_frame_bounded();
    assert_eq!(queued["id"], 4951);
    assert_eq!(queued["error"]["code"], -32602);
    let shutdown = child.read_frame_bounded();
    assert_eq!(shutdown["id"], 4952);
    assert!(shutdown.get("result").is_some());
    assert!(
        child
            .stdout
            .recv_timeout(Duration::from_millis(250))
            .is_err(),
        "no protocol frame, especially a mutation HostCall, may arrive after shutdown"
    );
}

#[test]
fn safe_delete_pool_facade_returns_strict_reference_plan_without_mutation_host_calls() {
    let declaration = "fn old_name() {}\n";
    let consumer = "fn caller() { old_name(); }\n";
    let workspace = fake_lsp_workspace("safe_delete_complete");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    fs::write(workspace.path().join("src/consumer.rs"), consumer).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let (response, host_calls) = invoke_safe_delete(
        &mut child,
        5001,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration), ("src/consumer.rs", consumer)],
    );
    assert_eq!(safe_delete_data(&response)["status"], "complete");
    assert_eq!(safe_delete_data(&response)["plan"]["coverage"], "complete");
    assert_eq!(
        safe_delete_data(&response)["plan"]["references"][0]["path"],
        "src/consumer.rs"
    );
    assert_eq!(
        safe_delete_data(&response)["plan"]["cleanup_edits"][0]["base_hash"],
        compute_content_version(consumer.as_bytes())
    );
    assert_no_mutation_host_calls(&host_calls);
}

#[test]
fn safe_delete_session_facades_preserve_resident_session_lifecycle() {
    let declaration = "fn old_name() {}\n";
    let workspace = fake_lsp_workspace("safe_delete_reference_free");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    for id in [5010, 5011] {
        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            id,
            safe_delete_request(declaration),
            &[("src/lib.rs", declaration)],
        );
        assert_eq!(safe_delete_data(&response)["status"], "complete");
        assert_eq!(
            safe_delete_data(&response)["plan"]["references"],
            serde_json::json!([])
        );
        assert_eq!(
            safe_delete_data(&response)["plan"]["cleanup_edits"],
            serde_json::json!([])
        );
        assert_no_mutation_host_calls(&host_calls);
    }
    let pids = fs::read_to_string(workspace.path().join(".tower/fake_lsp.pids")).unwrap();
    assert_eq!(pids.lines().count(), 1, "resident session must be reused");
}

#[test]
fn safe_delete_reference_lookup_checks_unicode_positions_against_versioned_content() {
    let declaration = "fn old_name() {}\n";
    let consumer = "fn caller() { let _ = \"😀\"; old_name(); }\n";
    let workspace = fake_lsp_workspace("safe_delete_unicode");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    fs::write(workspace.path().join("src/consumer.rs"), consumer).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let (response, host_calls) = invoke_safe_delete(
        &mut child,
        5020,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration), ("src/consumer.rs", consumer)],
    );
    let reference = &safe_delete_data(&response)["plan"]["references"][0];
    assert_eq!(reference["path"], "src/consumer.rs");
    assert_eq!(
        reference["start"],
        serde_json::json!({"line": 0, "character": 29})
    );
    assert_eq!(
        reference["end"],
        serde_json::json!({"line": 0, "character": 37})
    );
    assert_eq!(
        reference["base_hash"],
        compute_content_version(consumer.as_bytes())
    );
    assert_no_mutation_host_calls(&host_calls);
}

#[test]
fn safe_delete_native_fixture_produces_deterministic_structured_outcomes() {
    let declaration = "fn old_name() {}\n";
    for (offset, (mode, status)) in [
        ("safe_delete_incomplete", "incomplete"),
        (
            "safe_delete_capability_unavailable",
            "capability_unavailable",
        ),
        ("safe_delete_backend_timeout", "backend_timeout"),
        ("safe_delete_backend_crashed", "backend_crashed"),
        ("safe_delete_malformed_response", "malformed_response"),
        ("safe_delete_invalid_location", "invalid_location"),
        ("safe_delete_workspace_external", "workspace_external"),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            5030 + offset as u64,
            safe_delete_request(declaration),
            &[("src/lib.rs", declaration)],
        );
        assert_eq!(safe_delete_data(&response)["status"], status, "mode {mode}");
        assert_no_mutation_host_calls(&host_calls);
    }
}

#[test]
fn safe_delete_real_backends_never_infer_complete_coverage_from_unsupported_lookup_or_cleanup() {
    let declaration = "fn old_name() {}\n";
    let consumer = "old_name();\n";
    for (offset, (mode, status, plan)) in [
        (
            "safe_delete_no_cleanup_with_references",
            "incomplete",
            Some(serde_json::json!({
                "coverage": "incomplete",
                "references": [{
                    "path": "src/consumer.rs",
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 8},
                    "base_hash": compute_content_version(consumer.as_bytes()),
                }],
                "cleanup_edits": [],
            })),
        ),
        (
            "safe_delete_no_cleanup_reference_free",
            "complete",
            Some(serde_json::json!({
                "coverage": "complete",
                "references": [],
                "cleanup_edits": [],
            })),
        ),
        (
            "safe_delete_reference_lookup_unavailable",
            "capability_unavailable",
            None,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            5050 + offset as u64,
            safe_delete_request(declaration),
            &[("src/lib.rs", declaration), ("src/consumer.rs", consumer)],
        );
        let data = safe_delete_data(&response);
        assert_eq!(data["status"], status, "mode {mode}");
        match plan {
            Some(plan) => assert_eq!(data["plan"], plan, "mode {mode}"),
            None => assert!(data.get("plan").is_none(), "mode {mode}"),
        }
        assert_no_mutation_host_calls(&host_calls);
    }
}

#[test]
fn safe_delete_treats_a_null_reference_result_as_a_complete_empty_lookup() {
    let declaration = "fn old_name() {}\n";
    let workspace = fake_lsp_workspace("safe_delete_null_references");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let (response, host_calls) = invoke_safe_delete(
        &mut child,
        5070,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration)],
    );

    assert_eq!(
        safe_delete_data(&response),
        &serde_json::json!({
            "status": "complete",
            "plan": {
                "coverage": "complete",
                "references": [],
                "cleanup_edits": [],
            },
        })
    );
    assert_no_mutation_host_calls(&host_calls);
}

#[test]
fn safe_delete_normalizes_every_expected_backend_error_and_recovers_the_session() {
    let declaration = "fn old_name() {}\n";
    for (offset, (mode, expected_status)) in [
        ("safe_delete_unconfigured_once", "capability_unavailable"),
        ("safe_delete_not_renameable_once", "capability_unavailable"),
        ("safe_delete_server_missing_once", "backend_crashed"),
        ("safe_delete_invalid_command_once", "backend_crashed"),
        ("safe_delete_server_not_executable_once", "backend_crashed"),
        ("safe_delete_server_launch_failed_once", "backend_crashed"),
        ("safe_delete_server_transport_error_once", "backend_crashed"),
        ("safe_delete_server_error_once", "backend_crashed"),
        (
            "safe_delete_language_not_configured_once",
            "capability_unavailable",
        ),
        ("safe_delete_pool_error_once", "backend_crashed"),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);

        let (failure, failure_calls) = invoke_safe_delete(
            &mut child,
            5080 + offset as u64 * 2,
            safe_delete_request(declaration),
            &[("src/lib.rs", declaration)],
        );
        assert_eq!(
            safe_delete_data(&failure)["status"],
            expected_status,
            "mode {mode}"
        );
        assert!(
            failure_calls
                .iter()
                .all(|call| call["method"] != "host/log"),
            "expected backend mode {mode} emitted host/log: {failure_calls:?}"
        );
        assert_no_mutation_host_calls(&failure_calls);

        let (recovery, recovery_calls) = invoke_safe_delete(
            &mut child,
            5081 + offset as u64 * 2,
            safe_delete_request(declaration),
            &[("src/lib.rs", declaration)],
        );
        assert_eq!(
            safe_delete_data(&recovery)["status"],
            "complete",
            "mode {mode}"
        );
        assert_no_mutation_host_calls(&recovery_calls);
    }
}

#[test]
fn safe_delete_rejects_stale_declarations_before_backend_lookup() {
    let declaration = "fn old_name() {}\n";
    let fresh = "fn old_name() { /* changed */ }\n";
    let workspace = fake_lsp_workspace("safe_delete_complete");
    fs::write(workspace.path().join("src/lib.rs"), fresh).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let (response, host_calls) = invoke_safe_delete(
        &mut child,
        5100,
        safe_delete_request(declaration),
        &[("src/lib.rs", fresh)],
    );

    let data = safe_delete_data(&response);
    assert_eq!(data["status"], "incomplete");
    assert_eq!(data["plan"]["coverage"], "incomplete");
    assert_eq!(data["plan"]["references"], serde_json::json!([]));
    assert_eq!(data["plan"]["cleanup_edits"], serde_json::json!([]));
    assert_no_mutation_host_calls(&host_calls);
    let backend_log =
        fs::read_to_string(workspace.path().join(".tower/fake_lsp.log")).unwrap_or_default();
    assert!(
        !backend_log.contains("textDocument/references"),
        "a stale declaration must stop before reference lookup: {backend_log}"
    );
}

#[test]
fn safe_delete_normalizes_unreadable_content_and_invalid_coordinates_without_mutation() {
    let declaration = "fn old_name() {}\n";

    let workspace = fake_lsp_workspace("safe_delete_complete");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let mut unreadable =
        |_path: &str| Err(serde_json::json!({"code": -32001, "message": "READ_CONTENT_SENTINEL"}));
    let (response, host_calls) = invoke_safe_delete_with_read_file(
        &mut child,
        5110,
        safe_delete_request(declaration),
        &mut unreadable,
    );
    assert_eq!(safe_delete_data(&response)["status"], "invalid_location");
    assert_no_mutation_host_calls(&host_calls);

    for (offset, mode) in [
        "safe_delete_invalid_utf16_position",
        "safe_delete_split_surrogate_position",
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            5111 + offset as u64,
            safe_delete_request(declaration),
            &[("src/lib.rs", declaration)],
        );
        assert_eq!(
            safe_delete_data(&response)["status"],
            "invalid_location",
            "mode {mode}"
        );
        assert_no_mutation_host_calls(&host_calls);
    }
}

#[test]
fn safe_delete_rejects_invalid_declaration_positions_before_backend_lookup() {
    for (offset, (declaration, position)) in [
        (
            "fn old_name() {}\n".to_owned(),
            serde_json::json!({"line": 99, "character": 0}),
        ),
        (
            "fn 😀old_name() {}\n".to_owned(),
            serde_json::json!({"line": 0, "character": 4}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = fake_lsp_workspace("safe_delete_complete");
        fs::write(workspace.path().join("src/lib.rs"), &declaration).unwrap();
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let mut request = safe_delete_request(&declaration);
        request["declaration"]["position"] = position;

        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            5115 + offset as u64,
            request,
            &[("src/lib.rs", &declaration)],
        );

        assert_eq!(safe_delete_data(&response)["status"], "invalid_location");
        assert_no_mutation_host_calls(&host_calls);
        let backend_log =
            fs::read_to_string(workspace.path().join(".tower/fake_lsp.log")).unwrap_or_default();
        assert!(
            !backend_log.contains("textDocument/references"),
            "invalid declaration position must stop before lookup: {backend_log}"
        );
    }
}

#[test]
fn safe_delete_rejects_a_symlink_escape_without_mutation() {
    let declaration = "fn old_name() {}\n";
    let workspace = fake_lsp_workspace("safe_delete_symlink_escape");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let mut read_file = |path: &str| {
        if path == "src/escape.rs" {
            Err(serde_json::json!({
                "code": -32602,
                "message": "workspace path resolves outside the workspace"
            }))
        } else {
            Ok(serde_json::json!(declaration))
        }
    };
    let (response, host_calls) = invoke_safe_delete_with_read_file(
        &mut child,
        5120,
        safe_delete_request(declaration),
        &mut read_file,
    );
    assert_eq!(safe_delete_data(&response)["status"], "workspace_external");
    assert_no_mutation_host_calls(&host_calls);
}

#[test]
fn safe_delete_never_leaks_real_producer_sentinels_to_results_or_host_logs() {
    let source_sentinel = "SOURCE_CONTENT_SENTINEL_7b88";
    let backend_sentinel = "BACKEND_TEXT_SENTINEL_0a42";
    let absolute_path_sentinel = "/private/ABSOLUTE_PATH_SENTINEL_5f14.rs";
    let external_uri_sentinel = "file:///external/URI_SENTINEL_39c1.rs";
    let declaration = format!("fn old_name() {{ /* {source_sentinel} */ }}\n");

    for (offset, (mode, producer_sentinel)) in [
        ("safe_delete_backend_text_sentinel", backend_sentinel),
        ("safe_delete_absolute_path_sentinel", absolute_path_sentinel),
        ("safe_delete_external_uri_sentinel", external_uri_sentinel),
        ("safe_delete_source_content_sentinel", source_sentinel),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        fs::write(workspace.path().join("src/lib.rs"), &declaration).unwrap();
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            5130 + offset as u64,
            safe_delete_request(&declaration),
            &[("src/lib.rs", &declaration)],
        );
        let public_response = response.to_string();
        let captured_logs: Vec<_> = host_calls
            .iter()
            .filter(|call| call["method"] == "host/log")
            .collect();
        assert!(
            !public_response.contains(producer_sentinel),
            "public response leaked {producer_sentinel}: {public_response}"
        );
        assert!(
            captured_logs
                .iter()
                .all(|call| !call.to_string().contains(producer_sentinel)),
            "captured host/log call leaked {producer_sentinel}: {captured_logs:?}"
        );
        assert_no_mutation_host_calls(&host_calls);
    }
}

#[test]
fn safe_delete_expected_backend_failures_are_non_transport_and_the_session_remains_usable() {
    let declaration = "fn old_name() {}\n";
    let workspace = fake_lsp_workspace("safe_delete_malformed_once");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let (failure, failure_calls) = invoke_safe_delete(
        &mut child,
        5060,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration)],
    );
    assert_eq!(safe_delete_data(&failure)["status"], "malformed_response");
    assert_no_mutation_host_calls(&failure_calls);
    let (recovery, recovery_calls) = invoke_safe_delete(
        &mut child,
        5061,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration)],
    );
    assert_eq!(safe_delete_data(&recovery)["status"], "complete");
    assert_no_mutation_host_calls(&recovery_calls);
}

#[test]
fn safe_delete_process_contract_rejects_external_results_and_emits_zero_mutations() {
    let declaration = "fn old_name() {}\n";
    let workspace = fake_lsp_workspace("safe_delete_workspace_external");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let (response, host_calls) = invoke_safe_delete(
        &mut child,
        5070,
        safe_delete_request(declaration),
        &[("src/lib.rs", declaration)],
    );
    assert_eq!(safe_delete_data(&response)["status"], "workspace_external");
    assert_no_mutation_host_calls(&host_calls);
}

#[cfg(unix)]
#[test]
fn safe_delete_tool_defers_symlink_containment_to_the_read_file_host_call() {
    use std::os::unix::fs::symlink;

    let declaration = "fn old_name() {}\n";
    let external = tempfile::tempdir().expect("create external directory");
    let external_file = external.path().join("outside.rs");
    fs::write(&external_file, "old_name();\n").expect("write external source");

    let workspace = fake_lsp_workspace("safe_delete_symlink_escape");
    fs::write(workspace.path().join("src/lib.rs"), declaration).unwrap();
    symlink(&external_file, workspace.path().join("src/escape.rs"))
        .expect("create workspace symlink escape");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let mut read_file = |path: &str| match path {
        "src/lib.rs" => Ok(serde_json::json!(declaration)),
        "src/escape.rs" => Err(serde_json::json!({
            "code": -32602,
            "message": "workspace path resolves outside the workspace"
        })),
        unexpected => panic!("planner requested unexpected path {unexpected}"),
    };
    let (response, host_calls) = invoke_safe_delete_with_read_file(
        &mut child,
        5071,
        safe_delete_request(declaration),
        &mut read_file,
    );

    assert_eq!(safe_delete_data(&response)["status"], "workspace_external");
    assert!(host_calls.iter().any(|call| {
        call["method"] == "workspace/readFile" && call["params"]["path"] == "src/escape.rs"
    }));
    assert_no_mutation_host_calls(&host_calls);
}

fn lsp_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("lsp_extension")
        .to_str()
        .unwrap()
        .to_owned()
}

fn lsp_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "lsp".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Lazy,
        tools: vec![],
        events: EventsSection {
            subscribe: vec![
                "event/fileChanged".to_owned(),
                "event/fileDeleted".to_owned(),
            ],
        },
        capabilities: CapabilitiesSection {
            required: vec![
                "read_file".to_owned(),
                "notify".to_owned(),
                "log".to_owned(),
                "request_apply_edits".to_owned(),
            ],
        },
    }
}

fn make_deps() -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(InMemoryFs::new())),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    }
}

struct RawLspChild {
    child: Child,
    stdin: ChildStdin,
    stdout: mpsc::Receiver<String>,
    initialize_config: serde_json::Value,
}

impl RawLspChild {
    fn spawn() -> Self {
        Self::spawn_with_workspace(None)
    }

    fn spawn_with_workspace(workspace: Option<&std::path::Path>) -> Self {
        let initialize_config = workspace
            .and_then(|root| fs::read_to_string(root.join(".tower/config.toml")).ok())
            .and_then(|source| core_engine::adapters::config::lsp::parse_lsp_config(&source).ok())
            .map(|config| {
                let mut payload =
                    core_engine::adapters::config::lsp::lsp_initialize_payload(&config);
                payload["diagnostics_timeout_secs"] = serde_json::json!(2);
                payload
            })
            .unwrap_or_else(empty_lsp_initialize_payload);
        let mut command = Command::new(lsp_extension_bin());
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::inherit());
        if let Some(path) = workspace {
            command.env("TOWER_WORKSPACE", path);
        }

        let mut child = command.spawn().expect("spawn lsp extension");
        let stdin = child.stdin.take().expect("stdin");
        let child_stdout = child.stdout.take().expect("stdout");
        let (stdout_tx, stdout) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(child_stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if stdout_tx.send(line).is_err() => break,
                    Ok(_) => {}
                }
            }
        });

        Self {
            child,
            stdin,
            stdout,
            initialize_config,
        }
    }

    fn write_frame(&mut self, frame: serde_json::Value) {
        self.write_frame_for_drop(frame).expect("write frame");
    }

    fn write_frame_for_drop(&mut self, frame: serde_json::Value) -> std::io::Result<()> {
        let line = serde_json::to_string(&frame).expect("serialize frame");
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }

    fn read_frame(&mut self) -> serde_json::Value {
        let line = self
            .stdout
            .recv()
            .expect("child stdout closed before a frame arrived");
        serde_json::from_str(line.trim()).expect("valid JSON frame")
    }

    fn read_frame_bounded(&mut self) -> serde_json::Value {
        let line = self
            .stdout
            .recv_timeout(TEST_TIMEOUT)
            .expect("child must emit a frame before the protocol deadline");
        serde_json::from_str(line.trim()).expect("valid JSON frame")
    }
}

fn empty_lsp_initialize_payload() -> serde_json::Value {
    serde_json::json!({
        "servers": {},
        "idle_timeout_secs": null,
        "diagnostics_timeout_secs": 2,
    })
}

fn fake_lsp_workspace(mode: &str) -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("create fake lsp workspace");
    fs::create_dir_all(workspace.path().join(".tower")).expect("create .tower");
    fs::create_dir_all(workspace.path().join("src")).expect("create src");
    fs::write(workspace.path().join("src/lib.rs"), "fn old_name() {}\n")
        .expect("write source file");

    let config = format!(
        r#"
[lsp.rust]
command = {command}
args = [{mode}, {log}, {pids}]
extensions = ["rs"]
"#,
        command = serde_json::to_string(&fixture_lsp_server::binary_path()).unwrap(),
        mode = serde_json::to_string(mode).unwrap(),
        log = serde_json::to_string(&workspace.path().join(".tower/fake_lsp.log")).unwrap(),
        pids = serde_json::to_string(&workspace.path().join(".tower/fake_lsp.pids")).unwrap(),
    );
    fs::write(workspace.path().join(".tower/config.toml"), config).expect("write lsp config");

    workspace
}

fn initialize_raw_lsp_child(child: &mut RawLspChild) {
    initialize_raw_lsp_child_with(child, RawLspChild::read_frame);
}

fn initialize_raw_lsp_child_bounded(child: &mut RawLspChild) {
    initialize_raw_lsp_child_with(child, RawLspChild::read_frame_bounded);
}

fn initialize_raw_lsp_child_with(
    child: &mut RawLspChild,
    receive: fn(&mut RawLspChild) -> serde_json::Value,
) {
    let extension_config = child.initialize_config.clone();
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
            "extension_config": extension_config,
        }
    }));
    let initialized = receive(child);
    assert!(
        initialized.get("result").is_some(),
        "initialize must succeed; got: {initialized}"
    );
}

fn invoke_configured_read_tool(
    child: &mut RawLspChild,
    id: u64,
    name: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    invoke_configured_read_tool_with_content(child, id, name, params, "fn old_name() {}\n")
}

fn invoke_configured_read_tool_with_content(
    child: &mut RawLspChild,
    id: u64,
    name: &str,
    params: serde_json::Value,
    content: &str,
) -> serde_json::Value {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "invokeTool",
        "params": {"name": name, "params": params},
    }));
    let read_file = loop {
        let frame = child.read_frame_bounded();
        match frame["method"].as_str() {
            Some("workspace/readFile") => break frame,
            Some("notify/resourceUpdated") => {
                child.write_frame(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": frame["id"].clone(),
                    "result": null,
                }));
            }
            Some(method) => panic!(
                "configured {name} emitted unexpected HostCall before workspace/readFile: {method}; frame: {frame}"
            ),
            None => {
                panic!("configured {name} returned a response before workspace/readFile: {frame}")
            }
        }
    };
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"].clone(),
        "result": content,
    }));
    loop {
        let frame = child.read_frame_bounded();
        if frame["method"] == "notify/resourceUpdated" {
            child.write_frame(serde_json::json!({
                "jsonrpc": "2.0",
                "id": frame["id"].clone(),
                "result": null,
            }));
            continue;
        }
        assert_eq!(
            frame["id"], id,
            "configured {name} must return the invokeTool response; got {frame}"
        );
        return frame;
    }
}

#[test]
fn diagnostics_follow_the_servers_advertised_push_or_pull_capability() {
    let diagnostic = serde_json::json!({
        "line": 0,
        "character": 0,
        "endLine": 0,
        "endCharacter": 1,
        "severity": "error",
        "code": "fixture-syntax",
        "source": "fixture-lsp",
        "message": "fixture syntax error",
    });
    let cases = [
        ("diagnostics_push_empty", Some(serde_json::json!([])), None),
        (
            "diagnostics_push_nonempty",
            Some(serde_json::json!([diagnostic.clone()])),
            None,
        ),
        ("diagnostics_push_absent", None, Some("server_timeout")),
        (
            "diagnostics_pull_full",
            Some(serde_json::json!([diagnostic.clone()])),
            None,
        ),
        ("diagnostics_pull_timeout", None, Some("server_timeout")),
        (
            "diagnostics_pull_unchanged",
            None,
            Some("server_malformed_response"),
        ),
    ];

    for (offset, (mode, expected_diagnostics, expected_error)) in cases.into_iter().enumerate() {
        let workspace = fake_lsp_workspace(mode);
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let id = 440 + offset as u64;
        let response = invoke_configured_read_tool(
            &mut child,
            id,
            "diagnostics",
            read_tool_params("diagnostics", "src/lib.rs"),
        );

        if let Some(diagnostics) = expected_diagnostics {
            assert_eq!(
                response,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "type": "ToolResult",
                        "data": {"supported": true, "diagnostics": diagnostics},
                    },
                }),
                "{mode} must return the fixture diagnostics through invokeTool"
            );
        } else {
            assert_eq!(
                response,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": "language server request failed",
                        "data": {"outcome": {
                            "status": "error",
                            "code": expected_error.expect("failure case must name its outcome"),
                            "language": "rust",
                            "command": fixture_lsp_server::binary_path().to_str(),
                            "operation": "diagnostics",
                            "path": "src/lib.rs",
                            "phase": "runtime",
                            "message": "language server request failed",
                        }}
                    }
                }),
                "{mode} must preserve the typed diagnostics failure"
            );
        }

        let trace = read_trace(&workspace);
        assert_eq!(trace[0]["event"], "initialized", "{mode}: {trace:?}");
        assert!(trace[0]["pid"].is_u64(), "{mode}: {trace:?}");
        let synchronized = trace
            .iter()
            .position(|entry| entry["event"] == "didOpen")
            .expect("diagnostics fixture must trace document synchronization");
        assert!(
            trace.iter().skip(1).all(|entry| {
                entry["pid"].is_u64()
                    && entry["uri"]
                        .as_str()
                        .is_some_and(|uri| uri.ends_with("/src/lib.rs"))
            }),
            "every diagnostics operation trace must identify its fixture process and URI: {trace:?}"
        );
        let pull = trace
            .iter()
            .position(|entry| entry["event"] == "textDocument/diagnostic");
        if mode.starts_with("diagnostics_pull_") {
            assert!(
                pull.is_some_and(|index| index > synchronized),
                "{mode}: {trace:?}"
            );
        } else {
            assert_eq!(
                pull, None,
                "push capability must not trigger a pull: {trace:?}"
            );
        }
        if matches!(mode, "diagnostics_push_empty" | "diagnostics_push_nonempty") {
            let published = trace
                .iter()
                .position(|entry| entry["event"] == "published")
                .expect("fixed push fixture must trace its publication");
            assert!(published > synchronized, "{mode}: {trace:?}");
        }
        if mode == "diagnostics_push_absent" {
            assert!(
                trace.iter().all(|entry| entry["event"] != "published"),
                "absent push fixture must not publish: {trace:?}"
            );
        }
        if mode == "diagnostics_pull_full" {
            let published = trace
                .iter()
                .position(|entry| entry["event"] == "published")
                .expect("full pull fixture must trace its decoy publication");
            let responded = trace
                .iter()
                .position(|entry| entry["event"] == "diagnosticResponse")
                .expect("full pull fixture must trace its correlated response");
            assert!(
                trace[pull.unwrap()]["requestId"].is_u64(),
                "full pull request trace must include its JSON-RPC id: {trace:?}"
            );
            assert_eq!(
                trace[pull.unwrap()]["requestId"],
                trace[responded]["requestId"],
                "full pull response must identify the request it completes: {trace:?}"
            );
            assert!(
                published < pull.unwrap() && pull.unwrap() < responded,
                "{trace:?}"
            );
        }
    }

    let workspace = fake_lsp_workspace("diagnostics_by_content");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let invalid = invoke_configured_read_tool_with_content(
        &mut child,
        450,
        "diagnostics",
        read_tool_params("diagnostics", "src/lib.rs"),
        "fn fixture_invalid() {}\n",
    );
    assert_eq!(
        invalid["result"]["data"]["diagnostics"],
        serde_json::json!([diagnostic])
    );
    let valid = invoke_configured_read_tool_with_content(
        &mut child,
        451,
        "diagnostics",
        read_tool_params("diagnostics", "src/lib.rs"),
        "fn valid() {}\n",
    );
    assert_eq!(
        valid["result"]["data"]["diagnostics"],
        serde_json::json!([])
    );
    let trace = read_trace(&workspace);
    let target_uri = format!("file://{}", workspace.path().join("src/lib.rs").display());
    let did_open = trace
        .iter()
        .position(|entry| entry["event"] == "didOpen" && entry["uri"] == target_uri)
        .expect("content-driven diagnostics must trace didOpen for the exact target URI");
    let did_open_entry = &trace[did_open];
    let fixture_pid = did_open_entry["pid"]
        .as_u64()
        .expect("content-driven didOpen must identify its fixture process");
    assert_eq!(did_open_entry["uri"], target_uri);
    let first_published = trace
        .iter()
        .enumerate()
        .skip(did_open + 1)
        .find(|(_, entry)| entry["event"] == "published" && entry["uri"] == target_uri)
        .expect("content-driven diagnostics must publish after didOpen");
    assert_eq!(first_published.1["pid"], fixture_pid);
    assert_eq!(first_published.1["uri"], target_uri);

    let did_change = trace
        .iter()
        .enumerate()
        .skip(first_published.0 + 1)
        .find(|(_, entry)| entry["event"] == "didChange" && entry["uri"] == target_uri)
        .expect("content-driven diagnostics must trace didChange for the exact target URI");
    assert_eq!(did_change.1["pid"], fixture_pid);
    assert_eq!(did_change.1["uri"], target_uri);
    let changed_published = trace
        .iter()
        .enumerate()
        .skip(did_change.0 + 1)
        .find(|(_, entry)| entry["event"] == "published" && entry["uri"] == target_uri)
        .expect("content-driven diagnostics must publish after didChange");
    assert_eq!(changed_published.1["pid"], fixture_pid);
    assert_eq!(changed_published.1["uri"], target_uri);

    let timeout = invoke_configured_read_tool_with_content(
        &mut child,
        452,
        "diagnostics",
        read_tool_params("diagnostics", "src/timeout.rs"),
        "fn fixture_invalid() {}\n",
    );
    assert_eq!(
        timeout["error"]["data"]["outcome"]["code"],
        "server_timeout"
    );
    assert_eq!(
        timeout["error"]["data"]["outcome"]["path"],
        "src/timeout.rs"
    );
    let trace = read_trace(&workspace);
    let timeout_uri = trace
        .iter()
        .find(|entry| {
            entry["event"] == "didOpen"
                && entry["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with("/src/timeout.rs"))
        })
        .and_then(|entry| entry["uri"].as_str())
        .expect("timeout path synchronization must be traced");
    assert!(trace.iter().all(|entry| {
        entry["event"] != "published" || entry["uri"].as_str() != Some(timeout_uri)
    }));
}

#[test]
fn push_diagnostics_return_explicit_empty_and_nonempty_results() {
    let cases = [
        ("diagnostics_push_empty", serde_json::json!([])),
        (
            "diagnostics_push_nonempty",
            serde_json::json!([{
                "line": 0,
                "character": 0,
                "endLine": 0,
                "endCharacter": 1,
                "severity": "error",
                "code": "fixture-syntax",
                "source": "fixture-lsp",
                "message": "fixture syntax error",
            }]),
        ),
    ];

    for (offset, (mode, diagnostics)) in cases.into_iter().enumerate() {
        let workspace = fake_lsp_workspace(mode);
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let id = 460 + offset as u64;

        let response = invoke_configured_read_tool(
            &mut child,
            id,
            "diagnostics",
            serde_json::json!({"path": "src/lib.rs"}),
        );

        assert_eq!(
            response,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "type": "ToolResult",
                    "data": {"supported": true, "diagnostics": diagnostics},
                },
            })
        );
        let trace = read_trace(&workspace);
        assert!(trace.iter().any(|entry| entry["event"] == "published"));
        assert!(
            trace
                .iter()
                .all(|entry| entry["event"] != "textDocument/diagnostic")
        );
    }
}

#[test]
fn diagnostics_send_did_open_then_did_change_with_current_content() {
    let workspace = fake_lsp_workspace("diagnostics_by_content");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);
    let cases = [
        (470, "fn valid() {}\n", serde_json::json!([])),
        (
            471,
            "fn fixture_invalid() {}\n",
            serde_json::json!([{
                "line": 0,
                "character": 0,
                "endLine": 0,
                "endCharacter": 1,
                "severity": "error",
                "code": "fixture-syntax",
                "source": "fixture-lsp",
                "message": "fixture syntax error",
            }]),
        ),
        (472, "fn valid_again() {}\n", serde_json::json!([])),
    ];

    for (id, content, expected) in cases {
        let trace_path = workspace.path().join(".tower/fake_lsp.log.trace");
        let trace_before_len = if trace_path.exists() {
            read_trace(&workspace).len()
        } else {
            0
        };
        let response = invoke_configured_read_tool_with_content(
            &mut child,
            id,
            "diagnostics",
            serde_json::json!({"path": "src/lib.rs"}),
            content,
        );
        assert_eq!(response["result"]["data"]["diagnostics"], expected);

        let trace = read_trace(&workspace);
        let new_entries = &trace[trace_before_len..];
        let sync = new_entries
            .iter()
            .position(|entry| {
                entry["event"] == if id == 470 { "didOpen" } else { "didChange" }
                    && entry["text"] == content
            })
            .expect("each invocation must synchronize its current content");
        let publication = new_entries
            .iter()
            .position(|entry| entry["event"] == "published")
            .expect("each invocation must observe a fresh publication");
        assert!(
            sync < publication,
            "publication must follow synchronization"
        );
    }

    let trace = read_trace(&workspace);
    let fixture_pids: std::collections::BTreeSet<_> = trace
        .iter()
        .filter_map(|entry| entry["pid"].as_u64())
        .collect();
    assert_eq!(
        fixture_pids.len(),
        1,
        "all updates must use one LSP session"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry["event"] == "initialized")
            .count(),
        1
    );
}

fn expected_diagnostics_error(id: u64, outcome_code: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32000,
            "message": "language server request failed",
            "data": {"outcome": {
                "status": "error",
                "code": outcome_code,
                "language": "rust",
                "command": fixture_lsp_server::binary_path().to_str(),
                "operation": "diagnostics",
                "path": "src/lib.rs",
                "phase": "runtime",
                "message": "language server request failed",
            }}
        }
    })
}

#[test]
fn absent_diagnostics_publication_returns_a_structured_timeout() {
    let workspace = fake_lsp_workspace("diagnostics_push_absent");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let response = invoke_configured_read_tool(
        &mut child,
        480,
        "diagnostics",
        serde_json::json!({"path": "src/lib.rs"}),
    );

    assert_eq!(response, expected_diagnostics_error(480, "server_timeout"));
    let trace = read_trace(&workspace);
    assert!(trace.iter().any(|entry| entry["event"] == "didOpen"));
    assert!(trace.iter().all(|entry| entry["event"] != "published"));
}

#[test]
fn pull_diagnostics_timeout_returns_the_complete_structured_error() {
    let workspace = fake_lsp_workspace("diagnostics_pull_timeout");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let response = invoke_configured_read_tool(
        &mut child,
        481,
        "diagnostics",
        serde_json::json!({"path": "src/lib.rs"}),
    );

    assert_eq!(response, expected_diagnostics_error(481, "server_timeout"));
    let trace = read_trace(&workspace);
    let synchronized = trace
        .iter()
        .position(|entry| entry["event"] == "didOpen")
        .expect("pull diagnostics must synchronize first");
    let requested = trace
        .iter()
        .position(|entry| entry["event"] == "textDocument/diagnostic")
        .expect("pull diagnostics must send a request");
    assert!(synchronized < requested);
    assert!(
        trace
            .iter()
            .all(|entry| entry["event"] != "diagnosticResponse")
    );
}

#[test]
fn diagnostics_reject_ungrounded_pull_unchanged_with_complete_malformed_response_error() {
    let workspace = fake_lsp_workspace("diagnostics_pull_unchanged");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let response = invoke_configured_read_tool(
        &mut child,
        482,
        "diagnostics",
        serde_json::json!({"path": "src/lib.rs"}),
    );

    assert_eq!(
        response,
        expected_diagnostics_error(482, "server_malformed_response")
    );
    let trace = read_trace(&workspace);
    let requested = trace
        .iter()
        .position(|entry| entry["event"] == "textDocument/diagnostic")
        .expect("pull diagnostics must send a request");
    let responded = trace
        .iter()
        .position(|entry| entry["event"] == "diagnosticResponse")
        .expect("fixture must return its ungrounded unchanged report");
    assert!(
        trace[requested]["requestId"].is_u64(),
        "unchanged pull request trace must include its JSON-RPC id: {trace:?}"
    );
    assert_eq!(
        trace[requested]["requestId"], trace[responded]["requestId"],
        "unchanged pull response must identify the request it completes: {trace:?}"
    );
    assert!(requested < responded);
}

#[test]
fn diagnostics_business_timeout_keeps_the_sidecar_usable() {
    let workspace = fake_lsp_workspace("diagnostics_pull_timeout");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    let sidecar_pid = child.child.id();
    initialize_raw_lsp_child_bounded(&mut child);

    let timeout = invoke_configured_read_tool(
        &mut child,
        483,
        "diagnostics",
        serde_json::json!({"path": "src/lib.rs"}),
    );
    assert_eq!(
        timeout,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 483,
            "error": {
                "code": -32000,
                "message": "language server request failed",
                "data": {"outcome": {
                    "status": "error",
                    "code": "server_timeout",
                    "language": "rust",
                    "command": fixture_lsp_server::binary_path().to_str(),
                    "operation": "diagnostics",
                    "path": "src/lib.rs",
                    "phase": "runtime",
                    "message": "language server request failed",
                }}
            }
        })
    );
    let before_recovery = read_trace(&workspace);
    let fixture_pid = before_recovery[0]["pid"]
        .as_u64()
        .expect("initialized trace must identify the fixture process");

    let recovery = invoke_configured_read_tool(
        &mut child,
        484,
        "hover",
        serde_json::json!({"path": "src/lib.rs", "line": 0, "character": 3}),
    );

    assert_eq!(child.child.id(), sidecar_pid);
    assert_eq!(
        recovery,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 484,
            "result": {
                "type": "ToolResult",
                "data": {
                    "supported": true,
                    "hover": {"contents": "fn old_name()"},
                },
            },
        }),
        "the same sidecar must return the supported hover payload after its diagnostics timeout"
    );
    let after_recovery = read_trace(&workspace);
    assert_eq!(after_recovery[0]["pid"], fixture_pid);
    assert!(
        after_recovery
            .iter()
            .all(|entry| entry["pid"].as_u64() == Some(fixture_pid))
    );
    assert_eq!(
        after_recovery
            .iter()
            .filter(|entry| entry["event"] == "initialized")
            .count(),
        1
    );
}

fn read_tool_params(name: &str, path: &str) -> serde_json::Value {
    if name == "diagnostics" {
        serde_json::json!({"path": path})
    } else {
        serde_json::json!({"path": path, "line": 0, "character": 3})
    }
}

fn type_hierarchy_params(direction: &str) -> serde_json::Value {
    serde_json::json!({
        "path": "src/lib.rs",
        "line": 0,
        "character": 0,
        "direction": direction,
    })
}

fn invoke_type_hierarchy(
    child: &mut RawLspChild,
    id: u64,
    direction: &str,
    source: &str,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "invokeTool",
        "params": {"name": "type_hierarchy", "params": type_hierarchy_params(direction)},
    }));
    let read_file = child.read_frame_bounded();
    assert_eq!(read_file["method"], "workspace/readFile");
    assert_eq!(read_file["params"]["path"], "src/lib.rs");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"].clone(),
        "result": source,
    }));

    let mut logs = Vec::new();
    loop {
        let frame = child.read_frame_bounded();
        if frame["id"] == id {
            return (frame, logs);
        }
        assert_eq!(frame["method"], "host/log", "unexpected HostCall: {frame}");
        logs.push(frame["params"].clone());
        child.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": frame["id"].clone(),
            "result": null,
        }));
    }
}

fn read_trace(workspace: &tempfile::TempDir) -> Vec<serde_json::Value> {
    fs::read_to_string(workspace.path().join(".tower/fake_lsp.log.trace"))
        .expect("read exact fake LSP trace")
        .lines()
        .map(|line| serde_json::from_str(line).expect("trace line must be JSON"))
        .collect()
}

fn wait_for_trace_event(workspace: &tempfile::TempDir, expected: &str) -> Vec<serde_json::Value> {
    let deadline = std::time::Instant::now() + TEST_TIMEOUT;
    loop {
        let trace = read_trace(workspace);
        if trace.last().and_then(|entry| entry["event"].as_str()) == Some(expected) {
            return trace;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fake LSP did not record {expected} before the protocol deadline; trace: {trace:?}"
        );
        std::thread::yield_now();
    }
}

fn safe_delete_request(content: &str) -> serde_json::Value {
    serde_json::json!({
        "declaration": {
            "path": "src/lib.rs",
            "symbol_name": "old_name",
            "kind": "function",
            "position": {"line": 0, "character": 3},
            "deletion_span": {"start_byte": 0, "end_byte": content.len()},
            "base_hash": compute_content_version(content.as_bytes()),
        }
    })
}

fn invoke_safe_delete(
    child: &mut RawLspChild,
    id: u64,
    request: serde_json::Value,
    files: &[(&str, &str)],
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut read_file = |path: &str| {
        Ok(serde_json::json!(
            files
                .iter()
                .find_map(|(candidate, content)| (*candidate == path).then_some(*content))
                .expect("planner requested an unexpected fixture path")
        ))
    };
    invoke_safe_delete_with_read_file(child, id, request, &mut read_file)
}

fn invoke_safe_delete_with_read_file(
    child: &mut RawLspChild,
    id: u64,
    request: serde_json::Value,
    read_file: &mut dyn FnMut(&str) -> Result<serde_json::Value, serde_json::Value>,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "invokeTool",
        "params": {"name": "plan_safe_delete", "params": request},
    }));

    let mut host_calls = Vec::new();
    loop {
        let frame = child.read_frame_bounded();
        if frame["id"] == id {
            return (frame, host_calls);
        }
        let method = frame["method"]
            .as_str()
            .expect("non-response frame must be a HostCall");
        host_calls.push(frame.clone());
        let reply = match method {
            "workspace/readFile" => {
                let path = frame["params"]["path"]
                    .as_str()
                    .expect("readFile path must be a string");
                match read_file(path) {
                    Ok(result) => serde_json::json!({"result": result}),
                    Err(error) => serde_json::json!({"error": error}),
                }
            }
            "host/log" | "notify/resourceUpdated" => serde_json::json!({"result": null}),
            _ => serde_json::json!({"result": null}),
        };
        let mut response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": frame["id"].clone(),
        });
        response
            .as_object_mut()
            .expect("JSON-RPC reply must be an object")
            .extend(
                reply
                    .as_object()
                    .expect("HostCall reply must be an object")
                    .clone(),
            );
        child.write_frame(response);
    }
}

fn safe_delete_data(response: &serde_json::Value) -> &serde_json::Value {
    response["result"]["data"]
        .as_object()
        .map(|_| &response["result"]["data"])
        .unwrap_or_else(|| panic!("safe-delete planning must return ToolResult data: {response}"))
}

fn assert_no_mutation_host_calls(host_calls: &[serde_json::Value]) {
    assert!(
        host_calls.iter().all(|frame| {
            !matches!(
                frame["method"].as_str(),
                Some("workspace/applyEdits" | "workspace/writeFile" | "workspace/deleteFile")
            )
        }),
        "semantic planning emitted a mutation HostCall: {host_calls:?}"
    );
}

fn unsupported_read_envelope(
    id: u64,
    name: &str,
    path: &str,
    code: &str,
    message: &str,
) -> serde_json::Value {
    let legacy = if name == "hover" {
        serde_json::json!({"supported": false, "hover": null})
    } else if name == "diagnostics" {
        serde_json::json!({"supported": false, "diagnostics": []})
    } else {
        serde_json::json!({"supported": false, "locations": []})
    };
    let mut data = legacy.as_object().expect("legacy payload object").clone();
    data.insert(
        "outcome".to_owned(),
        serde_json::json!({
            "status": "unsupported",
            "code": code,
            "language": null,
            "command": null,
            "operation": name,
            "path": path,
            "phase": "runtime",
            "message": message,
        }),
    );
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"type": "ToolResult", "data": data},
    })
}

#[test]
fn f008_bare_read_extends_the_existing_mode_driven_fixture_once_and_table_tests_the_five_read_only_operations_for_supported_results_valid_empty_null_results_and_unconfigured_language()
 {
    let location = serde_json::json!([{
        "path": "src/lib.rs",
        "line": 0,
        "character": 3,
        "endLine": 0,
        "endCharacter": 11,
    }]);
    let cases = [
        (
            "diagnostics",
            "diagnostics_ready",
            serde_json::json!({"supported": true, "diagnostics": []}),
        ),
        (
            "definition",
            "normal",
            serde_json::json!({"supported": true, "locations": location.clone()}),
        ),
        (
            "references",
            "normal",
            serde_json::json!({"supported": true, "locations": location.clone()}),
        ),
        (
            "hover",
            "normal",
            serde_json::json!({"supported": true, "hover": {"contents": "fn old_name()"}}),
        ),
        (
            "implementations",
            "normal",
            serde_json::json!({"supported": true, "locations": location.clone()}),
        ),
        (
            "definition",
            "empty",
            serde_json::json!({"supported": true, "locations": []}),
        ),
        (
            "references",
            "empty",
            serde_json::json!({"supported": true, "locations": []}),
        ),
        (
            "hover",
            "empty",
            serde_json::json!({"supported": true, "hover": null}),
        ),
        (
            "implementations",
            "empty",
            serde_json::json!({"supported": true, "locations": []}),
        ),
    ];

    for (id, (name, mode, expected_data)) in cases.into_iter().enumerate() {
        let workspace = fake_lsp_workspace(mode);
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child(&mut child);
        let id = 100 + id as u64;
        let response =
            invoke_configured_read_tool(&mut child, id, name, read_tool_params(name, "src/lib.rs"));
        assert_eq!(
            response,
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"type": "ToolResult", "data": expected_data}}),
            "{name} in {mode} mode must preserve its exact legacy success envelope"
        );
    }

    let mut unconfigured = RawLspChild::spawn();
    initialize_raw_lsp_child(&mut unconfigured);
    for (offset, name) in [
        "diagnostics",
        "definition",
        "references",
        "hover",
        "implementations",
    ]
    .into_iter()
    .enumerate()
    {
        let id = 200 + offset as u64;
        unconfigured.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "invokeTool",
            "params": {"name": name, "params": read_tool_params(name, "src/file.unmapped")},
        }));
        assert_eq!(
            unconfigured.read_frame(),
            unsupported_read_envelope(
                id,
                name,
                "src/file.unmapped",
                "language_not_configured",
                "No language server is configured for this file.",
            ),
            "unmapped {name} must respond directly without a HostCall"
        );
    }
}

#[test]
fn f008_bare_read_checks_absent_false_and_method_not_found_semantic_providers_for_definition_references_hover_and_implementations()
 {
    for mode in ["provider_absent", "provider_false", "method_not_found"] {
        for (offset, name) in ["definition", "references", "hover", "implementations"]
            .into_iter()
            .enumerate()
        {
            let workspace = fake_lsp_workspace(mode);
            let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
            initialize_raw_lsp_child(&mut child);
            let id = 300 + offset as u64;
            let response = invoke_configured_read_tool(
                &mut child,
                id,
                name,
                read_tool_params(name, "src/lib.rs"),
            );
            assert_eq!(
                response,
                unsupported_read_envelope(
                    id,
                    name,
                    "src/lib.rs",
                    "capability_unavailable",
                    "The configured language server does not provide this operation.",
                ),
                "{name} must report {mode} as capability_unavailable"
            );
            if mode != "method_not_found" {
                let log = fs::read_to_string(workspace.path().join(".tower/fake_lsp.log"))
                    .expect("read fake LSP method log");
                let semantic_method = [
                    ("definition", "textDocument/definition"),
                    ("references", "textDocument/references"),
                    ("hover", "textDocument/hover"),
                    ("implementations", "textDocument/implementation"),
                ]
                .into_iter()
                .find_map(|(tool, method)| (tool == name).then_some(method))
                .expect("read-tool table must have a corresponding semantic method");
                assert!(
                    !log.lines().any(|method| method == semantic_method),
                    "{mode} {name} provider must prevent the unavailable semantic request; log: {log}"
                );
            }
        }
    }
}

#[test]
fn f008_bare_read_diagnostics_without_pull_provider_follows_published_readiness_with_timeout_on_unresolved_readiness()
 {
    let ready_workspace = fake_lsp_workspace("diagnostics_ready");
    let mut ready = RawLspChild::spawn_with_workspace(Some(ready_workspace.path()));
    initialize_raw_lsp_child(&mut ready);
    assert_eq!(
        invoke_configured_read_tool(
            &mut ready,
            401,
            "diagnostics",
            read_tool_params("diagnostics", "src/lib.rs"),
        ),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 401,
            "result": {"type": "ToolResult", "data": {"supported": true, "diagnostics": []}},
        })
    );

    let timeout_workspace = fake_lsp_workspace("diagnostics_timeout");
    let mut timeout = RawLspChild::spawn_with_workspace(Some(timeout_workspace.path()));
    initialize_raw_lsp_child(&mut timeout);
    assert_eq!(
        invoke_configured_read_tool(
            &mut timeout,
            402,
            "diagnostics",
            read_tool_params("diagnostics", "src/lib.rs"),
        ),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 402,
            "error": {
                "code": -32000,
                "message": "language server request failed",
                "data": {
                    "outcome": {
                        "status": "error",
                        "code": "server_timeout",
                        "language": "rust",
                        "command": fixture_lsp_server::binary_path(),
                        "operation": "diagnostics",
                        "path": "src/lib.rs",
                        "phase": "runtime",
                        "message": "language server request failed",
                    },
                },
            },
        }),
        "unresolved diagnostics readiness must preserve the complete typed timeout envelope"
    );
}

fn invoke_bare_rename_without_apply_edits(child: &mut RawLspChild, id: u64) -> serde_json::Value {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name",
            },
        },
    }));
    let read_file = child.read_frame();
    assert_eq!(read_file["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"].clone(),
        "result": "fn old_name() {}\n",
    }));
    child.read_frame()
}

fn classified_runtime_failure_fixtures() -> [(&'static str, &'static str); 5] {
    [
        ("server_crashed", "server_crashed"),
        ("server_timeout", "server_timeout"),
        ("server_transport_error", "server_transport_error"),
        ("server_malformed_response", "server_malformed_response"),
        ("backend_error", "server_error"),
    ]
}

fn expected_lsp_outcome(code: &str, command: Option<&str>, operation: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "code": code,
        "language": command.map(|_| "rust"),
        "command": command,
        "operation": operation,
        "path": "src/lib.rs",
        "phase": "runtime",
        "message": "language server request failed",
    })
}

#[cfg(target_os = "linux")]
fn assert_fixture_servers_stopped(workspace: &std::path::Path) {
    let pids = fs::read_to_string(workspace.join(".tower/fake_lsp.pids"))
        .expect("fixture server must record its pid");
    for pid in pids.lines() {
        let proc_entry = std::path::Path::new("/proc").join(pid);
        assert!(
            !proc_entry.exists(),
            "fixture language-server process {pid} survived sidecar shutdown"
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn assert_fixture_servers_stopped(_workspace: &std::path::Path) {}

fn classified_command_failure_workspace(code: &str) -> (tempfile::TempDir, String) {
    let workspace = fake_lsp_workspace("normal");
    let command = if code == "server_launch_failed" {
        let executable = workspace.path().join("missing-interpreter-server");
        fs::write(&executable, "#!/definitely/missing/f008-interpreter\n")
            .expect("write launch fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&executable)
                .expect("read launch fixture metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).expect("make launch fixture executable");
        }
        "./missing-interpreter-server".to_owned()
    } else {
        [
            ("invalid_command", ""),
            ("server_missing", "definitely-missing-f008-language-server"),
            ("server_not_executable", "."),
        ]
        .into_iter()
        .find_map(|(failure_code, command)| (failure_code == code).then_some(command))
        .expect("classified command failure table must contain the requested code")
        .to_owned()
    };
    let config = format!("[lsp.rust]\ncommand = {command:?}\nextensions = [\"rs\"]\nargs = []\n");
    fs::write(workspace.path().join(".tower/config.toml"), config).expect("write failure config");
    (workspace, command)
}

#[test]
fn f008_bare_rename_failure_table_tests_success_preview_availability_and_preserved_domain_payloads()
{
    for (mode, dry_run) in [("normal", false), ("normal", true), ("no_prepare", false)] {
        let workspace = fake_lsp_workspace(mode);
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child(&mut child);
        let response = invoke_supported_rename(&mut child, dry_run);
        let data = &response["result"]["data"];
        assert_eq!(data["spans"][0]["replacement"], "new_name");
        assert_eq!(data.get("applied").is_none(), dry_run);
    }

    let mut unconfigured = RawLspChild::spawn();
    initialize_raw_lsp_child(&mut unconfigured);
    unconfigured.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 510,
        "method": "invokeTool",
        "params": {"name": "rename", "params": {"path": "src/file.unmapped", "line": 0, "character": 0, "new_name": "new_name"}},
    }));
    let unavailable = unconfigured.read_frame();
    assert_eq!(
        unavailable["result"]["data"]["code"],
        "unsupported_language"
    );
    assert_eq!(unavailable["result"]["data"]["path"], "src/file.unmapped");
    assert_eq!(
        unavailable["result"]["data"]["outcome"]["code"],
        "language_not_configured"
    );
    assert_eq!(
        unavailable["result"]["data"]["message"],
        unavailable["result"]["data"]["outcome"]["message"]
    );
    assert_eq!(
        unavailable["result"]["data"]["path"],
        unavailable["result"]["data"]["outcome"]["path"]
    );

    for (mode, expected_code, expected_path) in [
        (
            "prepare_method_not_found",
            "unsupported_language",
            Some("src/lib.rs"),
        ),
        ("reject", "not_renameable", Some("src/lib.rs")),
        ("unsupported_edit", "unsupported_workspace_edit", None),
        ("invalid_range", "invalid_range", Some("src/lib.rs")),
    ] {
        let workspace = fake_lsp_workspace(mode);
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child(&mut child);
        let response = invoke_bare_rename_without_apply_edits(&mut child, 511);
        let data = &response["result"]["data"];
        assert_eq!(
            data["code"], expected_code,
            "unexpected {mode} payload: {response}"
        );
        assert_eq!(data["path"].as_str(), expected_path);
        if mode == "prepare_method_not_found" {
            assert_eq!(data["outcome"]["code"], "capability_unavailable");
            assert_eq!(data["message"], data["outcome"]["message"]);
            assert_eq!(data["path"], data["outcome"]["path"]);
        } else {
            assert!(
                data.get("outcome").is_none(),
                "{mode} must retain its domain payload without reclassification: {response}"
            );
        }
    }
}

#[test]
fn f008_bare_rename_failure_table_preserves_read_only_and_rename_server_failure_envelopes() {
    for (offset, (mode, code)) in classified_runtime_failure_fixtures()
        .into_iter()
        .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        let mut read_child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child(&mut read_child);
        let operation = if mode == "backend_error" {
            "definition"
        } else {
            "diagnostics"
        };
        let read_response = invoke_configured_read_tool(
            &mut read_child,
            520 + offset as u64,
            operation,
            read_tool_params(operation, "src/lib.rs"),
        );
        let read_outcome = &read_response["error"]["data"]["outcome"];
        assert_eq!(
            read_response["error"]["code"], -32000,
            "unexpected {mode} read envelope: {read_response}"
        );
        assert_eq!(
            read_outcome,
            &expected_lsp_outcome(code, fixture_lsp_server::binary_path().to_str(), operation)
        );
        assert!(
            !read_response
                .to_string()
                .contains("SENTINEL_PRIVATE_BACKEND_DETAIL")
        );

        let mut rename_child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child(&mut rename_child);
        let rename_response =
            invoke_bare_rename_without_apply_edits(&mut rename_child, 530 + offset as u64);
        let rename = &rename_response["result"]["data"];
        assert_eq!(
            rename["code"], "backend_error",
            "unexpected {mode} rename envelope: {rename_response}"
        );
        assert_eq!(rename["message"], "language server request failed");
        assert_eq!(rename["path"], "src/lib.rs");
        assert_eq!(
            rename["outcome"],
            expected_lsp_outcome(code, None, "rename")
        );
        assert_eq!(rename["message"], rename["outcome"]["message"]);
        assert_eq!(rename["path"], rename["outcome"]["path"]);
        assert!(
            !rename_response
                .to_string()
                .contains("SENTINEL_PRIVATE_BACKEND_DETAIL")
        );
        drop(read_child);
        drop(rename_child);
        assert_fixture_servers_stopped(workspace.path());
    }

    for (offset, code) in [
        "invalid_command",
        "server_missing",
        "server_not_executable",
        "server_launch_failed",
    ]
    .into_iter()
    .enumerate()
    {
        let (read_workspace, command) = classified_command_failure_workspace(code);
        let mut read_child = RawLspChild::spawn_with_workspace(Some(read_workspace.path()));
        initialize_raw_lsp_child(&mut read_child);
        let read_response = invoke_configured_read_tool(
            &mut read_child,
            550 + offset as u64,
            "diagnostics",
            read_tool_params("diagnostics", "src/lib.rs"),
        );
        let read_outcome = &read_response["error"]["data"]["outcome"];
        assert_eq!(read_response["error"]["code"], -32000);
        assert_eq!(
            read_outcome,
            &expected_lsp_outcome(code, Some(&command), "diagnostics")
        );

        let (rename_workspace, rename_command) = classified_command_failure_workspace(code);
        let mut rename_child = RawLspChild::spawn_with_workspace(Some(rename_workspace.path()));
        initialize_raw_lsp_child(&mut rename_child);
        let rename_response =
            invoke_bare_rename_without_apply_edits(&mut rename_child, 560 + offset as u64);
        let rename = &rename_response["result"]["data"];
        assert_eq!(rename["code"], "backend_error");
        assert_eq!(rename["message"], rename["outcome"]["message"]);
        assert_eq!(rename["path"], rename["outcome"]["path"]);
        assert_eq!(
            rename["outcome"],
            expected_lsp_outcome(code, None, "rename")
        );
        assert_eq!(rename_command, command);
    }
}

#[test]
fn f008_bare_rename_failure_never_applies_edits_and_excludes_sentinel_secrets() {
    let workspace = fake_lsp_workspace("backend_error");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);
    let response = invoke_bare_rename_without_apply_edits(&mut child, 540);
    let serialized = response.to_string();
    assert_eq!(
        response["result"]["data"]["message"],
        "language server request failed"
    );
    assert_eq!(response["result"]["data"]["path"], "src/lib.rs");
    assert!(!serialized.contains("SENTINEL_PRIVATE_BACKEND_DETAIL"));
    assert!(
        response.get("method").is_none(),
        "backend failure must not issue workspace/applyEdits: {response}"
    );
}

fn initialized_raw_lsp_child(child: &mut RawLspChild) -> serde_json::Value {
    let extension_config = child.initialize_config.clone();
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
            "extension_config": extension_config,
        }
    }));
    let initialized = child.read_frame_bounded();
    assert!(
        initialized.get("result").is_some(),
        "initialize must succeed; got: {initialized}"
    );
    initialized
}

/// T024: the public sidecar boundary must distinguish a typed read-only LSP
/// failure from legacy dispatch errors, without changing successful tool-result
/// envelopes for unsupported and rename requests.
#[test]
fn f008_error_envelope_preserves_typed_and_legacy_dispatch_outcomes() {
    let workspace = fake_lsp_workspace("backend_error");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 81,
        "method": "invokeTool",
        "params": {
            "name": "implementations",
            "params": {"path": "src/lib.rs", "line": 0, "character": 3},
        },
    }));
    let read_file = child.read_frame();
    assert_eq!(read_file["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"],
        "result": "fn old_name() {}\n",
    }));
    let typed_failure = child.read_frame();
    assert_eq!(
        typed_failure,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 81,
            "error": {
                "code": -32000,
                "message": "language server request failed",
                "data": {
                    "outcome": {
                        "status": "error",
                        "code": "server_error",
                        "language": "rust",
                        "command": fixture_lsp_server::binary_path(),
                        "operation": "implementations",
                        "path": "src/lib.rs",
                        "phase": "runtime",
                        "message": "language server request failed",
                    },
                },
            },
        }),
        "a read-only server failure must cross the sidecar boundary as a typed JSON-RPC error"
    );

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 82,
        "method": "invokeTool",
        "params": {"name": "implementations", "params": {"path": "src/lib.rs", "line": 0}},
    }));
    let malformed = child.read_frame();
    assert_eq!(malformed["error"]["code"], -32000);
    assert_eq!(
        malformed["error"]["message"],
        "bad LspImplementationRequest: missing field `character`"
    );
    assert!(malformed["error"].get("data").is_none());

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 83,
        "method": "invokeTool",
        "params": {"name": "implementations", "params": {"path": "src/lib.rs", "line": 0, "character": 3}},
    }));
    let read_file = child.read_frame();
    assert_eq!(read_file["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"],
        "error": {"code": -32001, "message": "workspace read denied"},
    }));
    let read_failure = child.read_frame();
    assert_eq!(read_failure["error"]["code"], -32000);
    assert_eq!(read_failure["error"]["message"], "workspace read denied");
    assert!(read_failure["error"].get("data").is_none());

    let mut unconfigured = RawLspChild::spawn();
    initialize_raw_lsp_child(&mut unconfigured);
    unconfigured.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 84,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {"path": "src/lib.rs", "line": 0, "character": 3, "new_name": "new_name"},
        },
    }));
    let rename = unconfigured.read_frame();
    assert_eq!(rename["id"], 84);
    assert_eq!(rename["result"]["type"], "ToolResult");
    assert_eq!(rename["result"]["data"]["code"], "unsupported_language");
    assert_eq!(
        rename["result"]["data"]["outcome"]["code"],
        "language_not_configured"
    );
    assert!(rename.get("error").is_none());

    unconfigured.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 85,
        "method": "invokeTool",
        "params": {"name": "definition", "params": {"path": "src/lib.rs", "line": 0, "character": 3}},
    }));
    let unsupported = unconfigured.read_frame();
    assert_eq!(unsupported["id"], 85);
    assert_eq!(unsupported["result"]["type"], "ToolResult");
    assert_eq!(unsupported["result"]["data"]["supported"], false);
    assert_eq!(
        unsupported["result"]["data"]["outcome"]["code"],
        "language_not_configured"
    );
    assert!(unsupported.get("error").is_none());

    let successful_workspace = fake_lsp_workspace("normal");
    let mut successful = RawLspChild::spawn_with_workspace(Some(successful_workspace.path()));
    initialize_raw_lsp_child(&mut successful);
    successful.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 86,
        "method": "invokeTool",
        "params": {"name": "implementations", "params": {"path": "src/lib.rs", "line": 0, "character": 3}},
    }));
    let read_file = successful.read_frame();
    assert_eq!(read_file["method"], "workspace/readFile");
    successful.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"],
        "result": "fn old_name() {}\n",
    }));
    let success = successful.read_frame();
    assert_eq!(success["id"], 86);
    assert_eq!(success["result"]["type"], "ToolResult");
    assert_eq!(success["result"]["data"]["supported"], true);
    assert_eq!(
        success["result"]["data"]["locations"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert!(success.get("error").is_none());
}

impl Drop for RawLspChild {
    fn drop(&mut self) {
        let _ = self.write_frame_for_drop(serde_json::json!({
            "jsonrpc": "2.0",
            "id": u64::MAX,
            "method": "shutdown",
            "params": {},
        }));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── AC1/U1: spawn and initialize ─────────────────────────────────────────────

#[test]
fn lsp_process_initialization_requires_an_explicit_diagnostics_deadline() {
    let mut child = RawLspChild::spawn();

    for (id, extension_config) in [
        (
            31,
            serde_json::json!({"servers": {}, "idle_timeout_secs": null}),
        ),
        (
            32,
            serde_json::json!({
                "servers": {},
                "idle_timeout_secs": null,
                "diagnostics_timeout_secs": 0,
            }),
        ),
    ] {
        child.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocol_version": extension_protocol::PROTOCOL_VERSION,
                "client_info": "lsp-e2e/0.1.0",
                "extension_config": extension_config,
            },
        }));
        assert_eq!(
            child.read_frame(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "invalid LSP initialization configuration",
                    "data": {
                        "kind": "invalid_lsp_initialize_config",
                        "reason": "invalid_configuration",
                    },
                },
            })
        );
    }

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 33,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
            "extension_config": empty_lsp_initialize_payload(),
        },
    }));
    assert_eq!(child.read_frame()["result"]["type"], "Initialized");
}

#[test]
fn f008_initialize_uses_exact_errors_without_fallback_or_partial_initialization() {
    let mut child = RawLspChild::spawn();

    for (id, extension_config, reason) in [
        (41, None, "missing_configuration"),
        (42, Some(serde_json::Value::Null), "invalid_configuration"),
        (
            43,
            Some(serde_json::json!({"servers": {"rust": {"command": 7}}})),
            "invalid_configuration",
        ),
    ] {
        let mut params = serde_json::json!({
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
        });
        if let Some(extension_config) = extension_config {
            params["extension_config"] = extension_config;
        }
        child.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": params,
        }));

        assert_eq!(
            child.read_frame(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "invalid LSP initialization configuration",
                    "data": {
                        "kind": "invalid_lsp_initialize_config",
                        "reason": reason,
                    },
                },
            })
        );
    }

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 44,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
            "extension_config": empty_lsp_initialize_payload(),
        },
    }));
    let initialized = child.read_frame();
    assert_eq!(initialized["id"], 44);
    assert_eq!(initialized["result"]["type"], "Initialized");
}

#[test]
fn f008_initialize_decodes_host_config_before_pool_creation_and_ignores_disk_config() {
    let workspace = fake_lsp_workspace("backend_error");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 51,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
            "extension_config": empty_lsp_initialize_payload(),
        },
    }));
    assert_eq!(child.read_frame()["result"]["type"], "Initialized");

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 52,
        "method": "invokeTool",
        "params": {
            "name": "diagnostics",
            "params": {"path": "src/lib.rs"},
        },
    }));
    let response = child.read_frame();
    assert_eq!(response["id"], 52);
    assert_eq!(response["result"]["data"]["supported"], false);
}

#[test]
fn f008_initialize_process_fixture_accepts_explicit_empty_and_valid_payloads_without_disk_config() {
    for (id, extension_config) in [
        (61, empty_lsp_initialize_payload()),
        (
            62,
            serde_json::json!({
                "servers": {
                    "rust": {
                        "command": "definitely-missing-tower-lsp-server",
                        "extensions": ["rs"],
                        "args": [],
                    },
                },
                "idle_timeout_secs": null,
                "diagnostics_timeout_secs": 2,
            }),
        ),
    ] {
        let mut child = RawLspChild::spawn();
        child.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocol_version": extension_protocol::PROTOCOL_VERSION,
                "client_info": "lsp-e2e/0.1.0",
                "extension_config": extension_config,
            },
        }));

        let initialized = child.read_frame();
        assert_eq!(initialized["id"], id);
        assert_eq!(initialized["result"]["type"], "Initialized");
    }
}

/// Once initialization completes, the sidecar must already be back in its main
/// read loop so the host's next request cannot be lost.
#[test]
fn f008_initialize_is_ready_for_the_next_request() {
    let mut child = RawLspChild::spawn();
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 71,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
            "extension_config": {
                "servers": {
                    "rust": {
                        "command": "definitely-missing-tower-lsp-server",
                        "extensions": ["rs"],
                        "args": [],
                    },
                },
                "idle_timeout_secs": null,
                "diagnostics_timeout_secs": 2,
            },
        },
    }));

    let initialized = child.read_frame();
    assert_eq!(initialized["id"], 71);
    assert_eq!(initialized["result"]["type"], "Initialized");

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 72,
        "method": "invokeTool",
        "params": {
            "name": "diagnostics",
            "params": {"path": "src/lib.rs"},
        },
    }));

    let read_file = child.read_frame();
    assert_eq!(read_file["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"],
        "result": "",
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], 72);
    assert!(
        response.get("result").is_some() || response.get("error").is_some(),
        "next request after initialize must receive a response; got: {response}"
    );
}

/// Runtime `InitResult` advertises the complete bare LSP tool set; MCP tool
/// discovery exposes those tools publicly through the merge-layer prefix.
#[test]
fn f007_t015_lsp_process_spawns_and_declares_implementations_and_rename_tools() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    let tool_names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tool_names.len(),
        10,
        "must declare 9 tools plus the host-internal fence; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"diagnostics"),
        "must declare 'diagnostics'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"definition"),
        "must declare 'definition'; got: {tool_names:?}"
    );
    assert!(tool_names.contains(&"plan_path_rename"));
    assert!(tool_names.contains(&"plan_safe_delete"));
    assert!(
        tool_names.contains(&"references"),
        "must declare 'references'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"hover"),
        "must declare 'hover'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"implementations"),
        "must declare 'implementations'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"type_hierarchy"),
        "must declare 'type_hierarchy'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"rename"),
        "must declare 'rename'; got: {tool_names:?}"
    );

    adapter.shutdown();
}

/// E2E/protocol tests assert tool discovery parity between `extension.toml` and
/// runtime `InitResult` metadata.
#[test]
fn f007_t015_lsp_runtime_initresult_tool_metadata_matches_extension_toml() {
    let manifest_path = workspace_root().join("extensions/lsp/extension.toml");
    let toml = fs::read_to_string(&manifest_path).expect("read lsp extension.toml");
    let manifest: ExtensionManifest = toml::from_str(&toml).expect("manifest must parse");
    let mut manifest_tools = manifest.tools.clone();
    manifest_tools.sort_by(|a, b| a.name.cmp(&b.name));

    let mut child = RawLspChild::spawn();
    let initialized = initialized_raw_lsp_child(&mut child);
    assert_eq!(initialized["result"]["type"], "Initialized");
    let init: InitResult = serde_json::from_value(initialized["result"]["data"].clone())
        .expect("runtime InitResult must deserialize");
    let mut runtime_tools = init.tools;
    let catch_up_fence = runtime_tools
        .iter()
        .find(|tool| tool.name == "_tower_catch_up_fence")
        .expect("runtime must advertise the host-internal catch-up fence");
    assert!(catch_up_fence.schema_json.contains("delivered_events"));
    let internal_safe_delete = runtime_tools
        .iter()
        .find(|tool| tool.name == "plan_safe_delete")
        .expect("runtime must advertise the internal plan_safe_delete tool");
    assert!(internal_safe_delete.schema_json.contains("declaration"));
    runtime_tools.retain(|tool| tool.name != "_tower_catch_up_fence");
    runtime_tools.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(
        runtime_tools, manifest_tools,
        "runtime InitResult tool metadata must stay in parity with extension.toml"
    );
    assert!(
        runtime_tools
            .iter()
            .any(|tool| tool.name == "implementations")
    );
    assert!(runtime_tools.iter().any(|tool| tool.name == "rename"));
}

#[test]
fn type_hierarchy_runtime_metadata_matches_manifest_schema() {
    let manifest_path = workspace_root().join("extensions/lsp/extension.toml");
    let toml = fs::read_to_string(&manifest_path).expect("read lsp extension.toml");
    let manifest: ExtensionManifest = toml::from_str(&toml).expect("manifest must parse");
    let manifest_tool = manifest
        .tools
        .iter()
        .find(|tool| tool.name == "type_hierarchy")
        .expect("manifest type_hierarchy tool");

    let mut child = RawLspChild::spawn();
    let initialized = initialized_raw_lsp_child(&mut child);
    let init: InitResult = serde_json::from_value(initialized["result"]["data"].clone())
        .expect("runtime InitResult must deserialize");
    let runtime_tool = init
        .tools
        .iter()
        .find(|tool| tool.name == "type_hierarchy")
        .expect("runtime type_hierarchy tool");

    assert_eq!(runtime_tool, manifest_tool);
}

#[test]
fn type_hierarchy_both_preserves_relationship_order_locations_ranges_and_coordinates() {
    let workspace = fake_lsp_workspace("hierarchy_mixed");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    let source = "PRIVATE_SOURCE_BYTES";
    let (response, logs) = invoke_type_hierarchy(&mut child, 600, "both", source);
    let data = &response["result"]["data"];

    assert_eq!(data["supported"], true);
    assert!(data.get("outcome").is_none());
    assert_eq!(
        data["subtypes"],
        serde_json::json!([
            {
                "name": "DerivedOne",
                "kind": "class",
                "detail": null,
                "range": {"line": 3, "character": 4, "endLine": 3, "endCharacter": 10},
                "selectionRange": {"line": 3, "character": 5, "endLine": 3, "endCharacter": 9},
                "path": "src/lib.rs",
                "externalUri": null,
                "external": false
            },
            {
                "name": "DerivedTwo",
                "kind": "class",
                "detail": null,
                "range": {"line": 7, "character": 8, "endLine": 7, "endCharacter": 14},
                "selectionRange": {"line": 7, "character": 9, "endLine": 7, "endCharacter": 13},
                "path": null,
                "externalUri": "file:///outside/external.rs",
                "external": true
            }
        ])
    );
    assert_eq!(
        data["supertypes"],
        serde_json::json!([{
            "name": "Base",
            "kind": "class",
            "detail": null,
            "range": {"line": 11, "character": 12, "endLine": 11, "endCharacter": 18},
            "selectionRange": {"line": 11, "character": 13, "endLine": 11, "endCharacter": 17},
            "path": "src/lib.rs",
            "externalUri": null,
            "external": false
        }])
    );

    let public = response.to_string();
    assert!(!public.contains(source));
    assert!(!public.contains("OPAQUE_"));
    assert!(logs.iter().all(|payload| {
        let payload = payload.to_string();
        !payload.contains(source) && !payload.contains("OPAQUE_")
    }));
}

#[test]
fn type_hierarchy_empty_and_unavailable_results_use_supported_envelopes() {
    for (offset, mode) in ["hierarchy_null", "hierarchy_empty"]
        .into_iter()
        .enumerate()
    {
        let workspace = fake_lsp_workspace(mode);
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let (response, _) =
            invoke_type_hierarchy(&mut child, 610 + offset as u64, "both", "struct Empty;");
        assert_eq!(response["result"]["data"]["supported"], true, "{mode}");
        assert_eq!(
            response["result"]["data"]["supertypes"],
            serde_json::json!([])
        );
        assert_eq!(
            response["result"]["data"]["subtypes"],
            serde_json::json!([])
        );
        assert!(response["result"]["data"].get("outcome").is_none());
        assert!(response.get("error").is_none());
    }

    for (offset, direction) in ["supertypes", "subtypes", "both"].into_iter().enumerate() {
        let workspace = fake_lsp_workspace("hierarchy_unavailable");
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let (response, _) = invoke_type_hierarchy(
            &mut child,
            620 + offset as u64,
            direction,
            "struct Unsupported;",
        );
        let data = &response["result"]["data"];
        assert_eq!(data["supported"], false, "{direction}");
        assert_eq!(data["supertypes"], serde_json::json!([]));
        assert_eq!(data["subtypes"], serde_json::json!([]));
        assert_eq!(data["outcome"]["code"], "capability_unavailable");
        assert!(response.get("error").is_none());
    }
}

/// AC1: `lsp_extension` subscribes to `event/fileChanged` and `event/fileDeleted`.
#[test]
fn lsp_process_subscribes_to_file_events() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    assert!(
        m.events.subscribe.contains(&"event/fileChanged".to_owned()),
        "must subscribe to event/fileChanged; got: {:?}",
        m.events.subscribe
    );
    assert!(
        m.events.subscribe.contains(&"event/fileDeleted".to_owned()),
        "must subscribe to event/fileDeleted; got: {:?}",
        m.events.subscribe
    );

    adapter.shutdown();
}

/// `extensions/lsp/extension.toml` declares bare tool names `implementations`
/// and `rename`, plus privileged capability `request_apply_edits`.
#[test]
fn f007_t015_lsp_process_declares_request_apply_edits_capability() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    assert!(
        m.capabilities
            .required
            .contains(&"request_apply_edits".to_owned()),
        "must declare 'request_apply_edits' capability; got: {:?}",
        m.capabilities.required
    );

    adapter.shutdown();
}

#[test]
fn lsp_process_queues_host_requests_seen_during_read_file_hostcall_and_discards_idle_responses() {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    let tower_dir = workspace.path().join(".tower");
    fs::create_dir_all(&tower_dir).expect("create .tower");
    fs::write(
        tower_dir.join("config.toml"),
        r#"
[lsp.rust]
command = "definitely-missing-tower-lsp-server"
extensions = ["rs"]
"#,
    )
    .expect("write lsp config");

    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    let extension_config = child.initialize_config.clone();
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0",
            "extension_config": extension_config,
        }
    }));
    let initialized = child.read_frame();
    assert!(
        initialized.get("result").is_some(),
        "initialize must succeed; got: {initialized}"
    );

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "diagnostics",
            "params": { "path": "src/lib.rs" }
        }
    }));
    let first_read = child.read_frame();
    assert_eq!(
        first_read["method"], "workspace/readFile",
        "configured diagnostics must request file contents before answering; got: {first_read}"
    );
    let first_read_id = first_read["id"].clone();

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "invokeTool",
        "params": {
            "name": "diagnostics",
            "params": { "path": "src/lib.rs" }
        }
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 20_000,
        "result": true
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "shutdown",
        "params": {}
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": first_read_id,
        "result": "fn main() {}\n"
    }));

    let first_response = child.read_frame();
    assert_eq!(
        first_response["id"], 1,
        "original invokeTool response must be returned before queued frames; got: {first_response}"
    );

    let queued_read = child.read_frame();
    assert_eq!(
        queued_read["method"], "workspace/readFile",
        "queued invokeTool must replay before shutdown and issue its own readFile; got: {queued_read}"
    );
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": queued_read["id"].clone(),
        "result": "fn main() {}\n"
    }));

    let queued_response = child.read_frame();
    assert_eq!(
        queued_response["id"], 2,
        "queued invokeTool response must preserve FIFO order before shutdown; got: {queued_response}"
    );

    let shutdown = child.read_frame();
    assert_eq!(
        shutdown["id"], 3,
        "shutdown request queued during HostCall wait must run after earlier invokeTool; got: {shutdown}"
    );
    assert!(
        shutdown.get("result").is_some(),
        "shutdown must return a result; got: {shutdown}"
    );
}

#[test]
fn type_hierarchy_queues_file_change_until_snapshot_query_releases_its_document_hold() {
    let workspace = fake_lsp_workspace("hierarchy_snapshot");
    fs::write(workspace.path().join("src/lib.rs"), "struct SNAPSHOT_B;\n")
        .expect("write event snapshot B");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 630,
        "method": "invokeTool",
        "params": {"name": "type_hierarchy", "params": type_hierarchy_params("subtypes")},
    }));
    let first_read = child.read_frame_bounded();
    assert_eq!(first_read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 631,
        "method": "deliverEvent",
        "params": {"data": {"type": "FileChanged", "file_id": 1, "path": "src/lib.rs"}},
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": first_read["id"].clone(),
        "result": "struct SNAPSHOT_A;\n",
    }));

    let first_result = child.read_frame_bounded();
    assert_eq!(first_result["id"], 630);
    assert_eq!(
        first_result["result"]["data"]["subtypes"][0]["name"],
        "SnapshotA"
    );
    let event_ack = child.read_frame_bounded();
    assert_eq!(
        event_ack["id"], 631,
        "ToolResult must precede queued event ACK"
    );

    let (second_result, _) =
        invoke_type_hierarchy(&mut child, 632, "subtypes", "struct SNAPSHOT_B;\n");
    assert_eq!(
        second_result["result"]["data"]["subtypes"][0]["name"],
        "SnapshotB"
    );
    let second_trace = read_trace(&workspace);
    let events = second_trace
        .iter()
        .map(|entry| entry["event"].as_str().expect("trace event"))
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        [
            "didOpen", "prepare", "subtypes", "didClose", "didOpen", "prepare", "subtypes",
        ],
        "the second backend response is the ordering barrier for queued event processing"
    );
    assert_eq!(second_trace[0]["text"], "struct SNAPSHOT_A;\n");
    assert_eq!(second_trace[1]["text"], "struct SNAPSHOT_A;\n");
    assert_eq!(second_trace[4]["text"], "struct SNAPSHOT_B;\n");
    assert_eq!(
        second_trace
            .iter()
            .filter(|entry| matches!(
                entry["event"].as_str(),
                Some("didOpen" | "didChange" | "didClose")
            ))
            .count(),
        3,
        "the resident B hold must avoid redundant document notifications"
    );
    assert_eq!(second_trace.last().unwrap()["event"], "subtypes");

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 633,
        "method": "deliverEvent",
        "params": {"data": {"type": "FileDeleted", "path": "src/lib.rs"}},
    }));
    assert_eq!(child.read_frame_bounded()["id"], 633);
    let final_trace = wait_for_trace_event(&workspace, "didClose");
    assert_eq!(final_trace.last().unwrap()["event"], "didClose");
}

#[test]
fn type_hierarchy_invalid_and_backend_failures_use_public_error_envelopes() {
    let workspace = fake_lsp_workspace("hierarchy_backend_error");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 640,
        "method": "invokeTool",
        "params": {
            "name": "type_hierarchy",
            "params": {"path": "src/lib.rs", "line": 0, "character": 0},
        },
    }));
    let malformed = child.read_frame_bounded();
    assert_eq!(malformed["error"]["code"], -32000);
    assert!(
        malformed["error"]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("bad TypeHierarchyQuery:"))
    );
    assert!(malformed.get("result").is_none());
    assert!(malformed["error"].get("data").is_none());
    assert!(!workspace.path().join(".tower/fake_lsp.log").exists());

    let (failure, logs) =
        invoke_type_hierarchy(&mut child, 641, "subtypes", "PRIVATE_SOURCE_BYTES");
    assert_eq!(
        failure,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 641,
            "error": {
                "code": -32000,
                "message": "language server request failed",
                "data": {"outcome": {
                    "status": "error",
                    "code": "server_error",
                    "language": "rust",
                    "command": fixture_lsp_server::binary_path(),
                    "operation": "type_hierarchy",
                    "path": "src/lib.rs",
                    "phase": "runtime",
                    "message": "language server request failed"
                }}
            }
        })
    );
    assert!(failure.get("result").is_none());
    let serialized_logs = serde_json::to_string(&logs).unwrap();
    assert!(!serialized_logs.contains("PRIVATE_SOURCE_BYTES"));
    assert!(!serialized_logs.contains("PRIVATE_BACKEND_FAILURE"));
}

#[test]
fn path_rename_malformed_params_use_invalid_params_error_at_process_boundary() {
    let mut child = RawLspChild::spawn();
    initialize_raw_lsp_child_bounded(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 650,
        "method": "invokeTool",
        "params": {
            "name": "plan_path_rename",
            "params": {"moves": "not-an-array", "semantic_policy": "required"},
        },
    }));

    let response = child.read_frame_bounded();
    assert_eq!(response["id"], 650);
    assert_eq!(response["error"]["code"], -32602);
    assert!(response.get("result").is_none());
}

#[test]
fn path_rename_replays_deferred_host_requests_fifo_after_tool_result() {
    let workspace = fake_lsp_workspace("path_rename_edit");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 651,
        "method": "invokeTool",
        "params": {
            "name": "plan_path_rename",
            "params": {
                "moves": [{
                    "source": "src/lib.rs",
                    "destination": "src/renamed.rs",
                    "kind": "file"
                }],
                "semantic_policy": "required"
            },
        },
    }));
    let read = child.read_frame_bounded();
    assert_eq!(read["method"], "workspace/readFile");
    assert_eq!(read["params"]["path"], "src/lib.rs");

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 652,
        "method": "deliverEvent",
        "params": {"data": {"type": "FileChanged", "path": "src/lib.rs"}}
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": 653, "method": "shutdown", "params": {}
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": read["id"], "result": "fn old_name() {}\n"
    }));

    let tool_result = child.read_frame_bounded();
    assert_eq!(tool_result["id"], 651);
    assert_eq!(tool_result["result"]["type"], "ToolResult");
    assert!(tool_result["result"]["data"]["plan"].is_object());
    assert_eq!(child.read_frame_bounded()["id"], 652);
    assert_eq!(child.read_frame_bounded()["id"], 653);
}

#[test]
fn lsp_initialize_errors_preserve_json_rpc_codes_for_malformed_params_version_mismatch_and_unknown_methods()
 {
    let mut child = RawLspChild::spawn();

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "initialize",
        "params": { "protocol_version": extension_protocol::PROTOCOL_VERSION }
    }));
    let malformed = child.read_frame();
    assert_eq!(malformed["id"], 10);
    assert_eq!(malformed["error"]["code"], -32602);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION + 1,
            "client_info": "lsp-e2e/0.1.0"
        }
    }));
    let mismatch = child.read_frame();
    assert_eq!(mismatch["id"], 11);
    assert_eq!(mismatch["error"]["code"], -32600);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "definitelyUnknown",
        "params": null
    }));
    let unknown = child.read_frame();
    assert_eq!(unknown["id"], 12);
    assert_eq!(unknown["error"]["code"], -32601);
}

// ── Diagnostics unsupported path ──────────────────────────────────────────────

/// With no language servers configured, `tower_lsp_diagnostics` returns
/// `{supported: false}` (not an error) for any path.
#[test]
fn lsp_process_diagnostics_unsupported_when_no_lsp_configured() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    // Build InMemoryFs before wrapping so we can write to it.
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        b"fn main() {}".to_vec(),
    )
    .expect("write must succeed");
    let deps = HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    };

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let result = adapter
        .call_tool("diagnostics", serde_json::json!({ "path": "src/lib.rs" }))
        .expect("diagnostics must not error");

    // Without any LSP config, the pool does not serve .rs — returns unsupported.
    let supported = result.get("supported").and_then(|v| v.as_bool());
    assert_eq!(
        supported,
        Some(false),
        "must return {{supported: false}} when no LSP configured; got: {result}"
    );

    adapter.shutdown();
}

/// `tower_lsp_implementations` parses `LspImplementationRequest` fields `path`,
/// `line`, and `character`.
#[test]
fn f007_t015_tower_lsp_implementations_parses_lsp_implementation_request_fields() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let result = adapter
        .call_tool(
            "implementations",
            serde_json::json!({
                "path": "src/lib.rs",
                "line": 3,
                "character": 14
            }),
        )
        .expect("implementations request with path, line, and character must parse");

    assert!(
        result.is_object(),
        "implementations must return a structured result after parsing; got: {result}"
    );

    adapter.shutdown();
}

/// `tower_lsp_implementations` returns `LspImplementationResult { supported:
/// false, locations: [] }` when `NavigationPort::implementations` returns
/// `CodeIntelError::Unsupported`; backend failures still surface as the existing
/// sidecar tool error path.
#[test]
fn f007_t015_tower_lsp_implementations_returns_unsupported_result_for_unsupported_navigation() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let result = adapter
        .call_tool(
            "implementations",
            serde_json::json!({
                "path": "README.md",
                "line": 0,
                "character": 0
            }),
        )
        .expect("unsupported implementations lookup must return a result, not a sidecar error");

    assert_eq!(result["supported"], serde_json::json!(false));
    assert_eq!(result["locations"], serde_json::json!([]));

    adapter.shutdown();
}

/// `tower_lsp_implementations` surfaces backend failures through the existing
/// sidecar tool error path rather than converting them to unsupported results.
#[test]
fn f007_t015_tower_lsp_implementations_surfaces_backend_failures_as_sidecar_tool_errors() {
    let workspace = fake_lsp_workspace("backend_error");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "implementations",
            "params": { "path": "src/lib.rs", "line": 0, "character": 3 }
        }
    }));

    let read = child.read_frame();
    assert_eq!(
        read["method"],
        serde_json::json!("workspace/readFile"),
        "configured implementations lookup must request file contents before LSP query; got {read}"
    );
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["error"]["code"],
        serde_json::json!(-32000),
        "backend failure must use the sidecar tool error path; got {response}"
    );
    let message = response["error"]["message"]
        .as_str()
        .expect("sidecar error must include a message");
    assert_eq!(
        message, "language server request failed",
        "typed backend errors must not expose provider details"
    );
}

/// `tower_lsp_implementations(path, line, character)` returns
/// `LspImplementationResult { supported: true, locations }`, with each location
/// in the same `Location` shape as existing `tower_lsp_definition` and
/// `tower_lsp_references`.
#[test]
fn f007_t015_tower_lsp_implementations_returns_supported_locations_in_existing_location_shape() {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "implementations",
            "params": { "path": "src/lib.rs", "line": 0, "character": 3 }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    let result = &response["result"]["data"];
    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(result["supported"], serde_json::json!(true));
    assert_eq!(
        result["locations"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(result["locations"][0]["line"], serde_json::json!(0));
    assert_eq!(result["locations"][0]["character"], serde_json::json!(3));
    assert_eq!(result["locations"][0]["endLine"], serde_json::json!(0));
    assert_eq!(
        result["locations"][0]["endCharacter"],
        serde_json::json!(11)
    );
}

/// `tower_lsp_rename(path, line, character, new_name, dry_run?)` parses
/// `RenameRequest`, calls prepareRename when available, and returns
/// `RenameError` with `RenameErrorCode::NotRenameable` serialized as
/// `not_renameable` without HostCall when prepareRename rejects the position.
#[test]
fn f007_t015_tower_lsp_rename_returns_not_renameable_without_apply_edits_hostcall_when_prepare_rename_rejects()
 {
    let workspace = fake_lsp_workspace("reject");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name"
            }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], serde_json::json!(1));
    assert!(
        response.get("method").is_none(),
        "prepareRename rejection must not issue workspace/applyEdits; got {response}"
    );
    assert_eq!(
        response["result"]["data"]["code"],
        serde_json::json!("not_renameable")
    );
}

/// `tower_lsp_rename` rejects unsupported WorkspaceEdit operations with
/// `RenameError` with `RenameErrorCode::UnsupportedWorkspaceEdit`
/// serialized as `unsupported_workspace_edit` before calling
/// `workspace/applyEdits`.
#[test]
fn f007_t015_tower_lsp_rename_rejects_unsupported_workspace_edit_before_apply_edits_hostcall() {
    let workspace = fake_lsp_workspace("unsupported_edit");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name"
            }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], serde_json::json!(1));
    assert!(
        response.get("method").is_none(),
        "unsupported WorkspaceEdit must not issue workspace/applyEdits; got {response}"
    );
    assert_eq!(
        response["result"]["data"]["code"],
        serde_json::json!("unsupported_workspace_edit")
    );
}

fn respond_to_apply_edits(child: &mut RawLspChild, dry_run: bool) -> serde_json::Value {
    let apply = child.read_frame();
    assert_eq!(apply["method"], "workspace/applyEdits");
    assert_eq!(apply["params"]["dry_run"], serde_json::json!(dry_run));
    assert_eq!(
        apply["params"]["edits"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(
        apply["params"]["edits"][0]["start_byte"],
        serde_json::json!(3)
    );
    assert_eq!(
        apply["params"]["edits"][0]["end_byte"],
        serde_json::json!(11)
    );
    assert_eq!(
        apply["params"]["edits"][0]["replacement"],
        serde_json::json!("new_name")
    );
    assert_eq!(
        apply["params"]["edits"][0]["base_hash"],
        serde_json::json!(compute_content_version(b"fn old_name() {}\n"))
    );
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": apply["id"].clone(),
        "result": {
            "files_changed": if dry_run { 0 } else { 1 },
            "per_file": [{
                "path": "src/lib.rs",
                "applied": !dry_run,
                "edits_applied": 1,
                "edits_skipped": 0,
                "new_version": if dry_run { serde_json::Value::Null } else { serde_json::json!("abc123") },
                "preview": "fn new_name() {}\n"
            }]
        }
    }));
    apply
}

fn invoke_supported_rename(child: &mut RawLspChild, dry_run: bool) -> serde_json::Value {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name",
                "dry_run": dry_run
            }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    respond_to_apply_edits(child, dry_run);
    child.read_frame()
}

/// For non-dry-run rename with supported text edits, `tower_lsp_rename`
/// performs exactly one `workspace/applyEdits` HostCall containing all decoded
/// spans and `dry_run: false`.
#[test]
fn f007_t015_tower_lsp_rename_non_dry_run_performs_one_apply_edits_hostcall_with_decoded_spans_and_dry_run_false()
 {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, false);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["applied"],
        serde_json::json!(true)
    );
    assert_eq!(
        response["result"]["data"]["files_changed"],
        serde_json::json!(1)
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
}

#[test]
fn f007_t015_tower_lsp_rename_skips_prepare_when_server_does_not_advertise_prepare_provider() {
    let workspace = fake_lsp_workspace("no_prepare");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, false);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
}

#[test]
fn f008_t020_tower_lsp_rename_reports_unsupported_when_prepare_method_is_missing() {
    let workspace = fake_lsp_workspace("prepare_method_not_found");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name",
                "dry_run": false
            }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(response["result"]["data"]["code"], "unsupported_language");
    assert!(
        response.get("method").is_none(),
        "capability-unavailable rename must not request workspace edits: {response}"
    );
}

/// For `dry_run: true`, `tower_lsp_rename` performs exactly one
/// `workspace/applyEdits` HostCall containing all decoded spans and
/// `dry_run: true`; the host returns preview/per-file data and performs no
/// mutation.
#[test]
fn f007_t015_tower_lsp_rename_dry_run_performs_one_apply_edits_hostcall_with_decoded_spans_and_dry_run_true()
 {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, true);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["preview"],
        serde_json::json!("fn new_name() {}\n")
    );
    assert_eq!(
        response["result"]["data"]["per_file"][0]["applied"],
        serde_json::json!(false)
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
}

#[test]
fn f007_t015_tower_lsp_rename_accepts_versioned_document_changes_text_edits() {
    let workspace = fake_lsp_workspace("versioned_document_changes");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, true);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["spans"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
    assert_eq!(
        response["result"]["data"]["preview"],
        serde_json::json!("fn new_name() {}\n")
    );
}

/// Rename success returns `RenameResult` with fields `applied`, `files_changed`,
/// `spans`, `preview`, and `per_file`; rename dry-run returns `RenamePreview`
/// with fields `spans`, `preview`, and `per_file`.
#[test]
fn f007_t015_tower_lsp_rename_success_and_dry_run_return_rename_result_and_rename_preview_fields() {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let result_response = invoke_supported_rename(&mut child, false);
    let result = &result_response["result"]["data"];
    assert!(result.get("applied").is_some());
    assert!(result.get("files_changed").is_some());
    assert!(result.get("spans").is_some());
    assert!(result.get("preview").is_some());
    assert!(result.get("per_file").is_some());

    let preview_response = invoke_supported_rename(&mut child, true);
    let preview = &preview_response["result"]["data"];
    assert!(preview.get("spans").is_some());
    assert!(preview.get("preview").is_some());
    assert!(preview.get("per_file").is_some());
    assert!(preview.get("applied").is_none());
    assert!(preview.get("files_changed").is_none());
    assert_eq!(preview["spans"][0]["start_byte"], serde_json::json!(3));
    assert_eq!(preview["spans"][0]["end_byte"], serde_json::json!(11));
    assert_eq!(
        preview["spans"][0]["base_hash"],
        serde_json::json!(compute_content_version(b"fn old_name() {}\n"))
    );
}

/// Host apply failures are surfaced in `RenameResult.per_file[*].error` as
/// `WorkspaceApplyEditsError` with exact `WorkspaceApplyEditsErrorCode` values
/// from T010.
#[test]
fn f007_t015_tower_lsp_rename_surfaces_host_apply_failures_in_per_file_error_with_exact_workspace_apply_edits_error_code()
 {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name"
            }
        }
    }));
    let read = child.read_frame();
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));
    let apply = child.read_frame();
    assert_eq!(apply["method"], "workspace/applyEdits");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": apply["id"].clone(),
        "result": {
            "files_changed": 0,
            "per_file": [{
                "path": "src/lib.rs",
                "applied": false,
                "edits_applied": 0,
                "edits_skipped": 1,
                "error": {
                    "code": "cas_conflict",
                    "message": "stale base hash",
                    "path": "src/lib.rs"
                }
            }]
        }
    }));

    let response = child.read_frame();
    assert_eq!(
        response["result"]["data"]["per_file"][0]["error"]["code"],
        serde_json::json!("cas_conflict")
    );
}

// ── EV1: document sync events ─────────────────────────────────────────────────

/// EV1: `event/fileChanged` is delivered without error.
#[test]
fn lsp_process_file_changed_event_completes() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let event = Event::FileChanged {
        file_id: 1,
        path: "src/main.rs".to_owned(),
    };
    adapter
        .deliver_event(event)
        .expect("fileChanged must succeed");

    adapter.shutdown();
}

/// EV1: `event/fileDeleted` is delivered without error.
#[test]
fn lsp_process_file_deleted_event_completes() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(empty_lsp_initialize_payload()),
    )
    .expect("lsp_extension must spawn");

    let event = Event::FileDeleted {
        path: "src/lib.rs".to_owned(),
    };
    adapter
        .deliver_event(event)
        .expect("fileDeleted must succeed");

    adapter.shutdown();
}

#[test]
fn catch_up_lsp_query_cannot_overtake_document_sync() {
    let workspace = fake_lsp_workspace("delayed_sync");
    fs::write(
        workspace.path().join("src/lib.rs"),
        "fn current_after_catch_up() {}\n",
    )
    .expect("commit current LSP document");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    let initialized = initialized_raw_lsp_child(&mut child);
    let tools = initialized["result"]["data"]["tools"]
        .as_array()
        .expect("initialized LSP tools must be an array");
    assert!(
        tools
            .iter()
            .any(|tool| tool["name"] == "_tower_catch_up_fence"),
        "the private fence must be declared by the real LSP sidecar so the daemon can synchronize it"
    );

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 701,
        "method": "deliverEvent",
        "params": {
            "type": "fileIndexed",
            "file_id": 1,
            "path": "src/lib.rs"
        }
    }));
    let event_ack = child.read_frame_bounded();
    assert_eq!(event_ack["id"], 701);
    assert!(
        event_ack.get("result").is_some(),
        "indexed event failed: {event_ack}"
    );

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 702,
        "method": "invokeTool",
        "params": {
            "name": "_tower_catch_up_fence",
            "params": {"epoch": 19, "delivered_events": 1}
        }
    }));
    let fence = child.read_frame_bounded();
    assert_eq!(
        fence["result"]["data"],
        serde_json::json!({"epoch": 19, "processed_events": 1, "ready": true}),
        "LSP fence must wait for document synchronization: {fence}"
    );

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 703,
        "method": "invokeTool",
        "params": {
            "name": "definition",
            "params": {"path": "src/lib.rs", "line": 0, "character": 3}
        }
    }));
    let read_file = child.read_frame_bounded();
    assert_eq!(read_file["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"].clone(),
        "result": "fn current_after_catch_up() {}\n"
    }));
    let definition = child.read_frame_bounded();
    assert_eq!(definition["id"], 703);
    assert!(
        definition.to_string().contains("current_after_catch_up"),
        "semantic query overtook synchronized document state: {definition}"
    );

    // A sidecar without a configured language server has no backend barrier to
    // acknowledge.  It must fail closed instead of claiming that its capability
    // is ready merely because it received a file event.
    let mut unsupported = RawLspChild::spawn();
    let initialized = initialized_raw_lsp_child(&mut unsupported);
    assert!(
        initialized["result"]["data"]["tools"]
            .as_array()
            .expect("initialized LSP tools must be an array")
            .iter()
            .any(|tool| tool["name"] == "_tower_catch_up_fence"),
        "fence support is declared independently from backend availability"
    );
    unsupported.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 704,
        "method": "invokeTool",
        "params": {
            "name": "_tower_catch_up_fence",
            "params": {"epoch": 20, "delivered_events": 0}
        }
    }));
    let unsupported_fence = unsupported.read_frame_bounded();
    assert_eq!(unsupported_fence["id"], 704);
    assert!(
        unsupported_fence.get("error").is_some()
            || unsupported_fence["result"]["data"]["ready"] == serde_json::json!(false),
        "an unconfigured LSP backend must not acknowledge readiness: {unsupported_fence}"
    );
}

#[test]
fn configured_lazy_lsp_session_synchronizes_document_before_first_query() {
    let workspace = fake_lsp_workspace("delayed_sync");
    fs::write(
        workspace.path().join("src/lib.rs"),
        "fn first_query_document() {}\n",
    )
    .expect("commit document before the first semantic query");
    let backend_log = workspace.path().join(".tower/fake_lsp.log");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child_bounded(&mut child);

    assert!(
        !backend_log.exists(),
        "configured language server must remain lazy until the first query"
    );

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 705,
        "method": "invokeTool",
        "params": {
            "name": "definition",
            "params": {"path": "src/lib.rs", "line": 0, "character": 3}
        }
    }));
    let read_file = child.read_frame_bounded();
    assert_eq!(read_file["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"].clone(),
        "result": "fn first_query_document() {}\n"
    }));

    let definition = child.read_frame_bounded();
    assert_eq!(definition["id"], 705);
    assert!(
        definition.to_string().contains("first_query_document.rs"),
        "first lazy semantic query ran before its document was synchronized: {definition}"
    );
    assert!(
        backend_log.exists(),
        "the first semantic query must activate the configured language server"
    );
}

// ── Push-response frame hazard regression ─────────────────────────────────────

/// Regression: KNOWN PROTOCOL HAZARDS §1 — push-response frames injected during
/// idle must NOT cause the next `invokeTool` call to receive a spurious
/// `ProtocolError`.
///
/// This test drives the binary at the raw JSON-RPC level so it can inject a
/// response frame (simulating the host's ACK to `notify/resourceUpdated`)
/// during idle, then verify the next invokeTool returns a valid result.
#[test]
fn lsp_process_push_response_frame_during_idle_is_discarded() {
    use extension_protocol::PROTOCOL_VERSION;

    let bin = lsp_extension_bin();

    let mut child = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("lsp_extension must spawn");

    let mut child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(child_stdout);

    // ── Helper closures ──

    let send = |stdin: &mut std::process::ChildStdin, frame: serde_json::Value| {
        let s = serde_json::to_string(&frame).unwrap();
        stdin.write_all(s.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };

    let recv = |reader: &mut BufReader<std::process::ChildStdout>| -> serde_json::Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).expect("valid JSON from extension")
    };

    // ── 1. initialize handshake ──────────────────────────────────────────────
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocol_version": PROTOCOL_VERSION,
                "client_info": "test-host/0.1.0",
                "extension_config": empty_lsp_initialize_payload(),
            }
        }),
    );

    // Read the Initialized response.
    let init_resp = recv(&mut reader);
    assert!(
        init_resp.get("result").is_some(),
        "initialize must return a result; got: {init_resp}"
    );
    let tools = &init_resp["result"]["data"]["tools"];
    let tool_names = tools
        .as_array()
        .expect("tools must be an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"_tower_catch_up_fence"));
    let public_tool_count = tool_names
        .iter()
        .filter(|name| **name != "_tower_catch_up_fence")
        .count();
    assert_eq!(
        public_tool_count, 9,
        "must declare 9 public tools plus the internal fence; got: {tool_names:?} (full resp: {init_resp})"
    );
    assert!(tool_names.contains(&"implementations"));
    assert!(tool_names.contains(&"type_hierarchy"));
    assert!(tool_names.contains(&"rename"));
    assert!(tool_names.contains(&"plan_path_rename"));
    assert!(tool_names.contains(&"plan_safe_delete"));

    // ── 2. Inject a push-response frame (simulates host ACK of notify/resourceUpdated) ──
    //
    // The push thread would have sent a notify/resourceUpdated request.
    // and the host would ACK with {"jsonrpc":"2.0","result":true,"id":20000}.
    // We inject this ACK directly into the extension's stdin while it is in idle.
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 20000,
            "result": true
        }),
    );

    // ── 3. Now send an invokeTool — must NOT get a spurious ProtocolError ─────
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "invokeTool",
            "params": {
                "name": "diagnostics",
                "params": { "path": "src/lib.rs" }
            }
        }),
    );

    // The extension needs to readFile — handle that HostCall.
    // (The pool.serves() check returns false for unconfigured .rs, so actually
    // there may be no HostCall. But if there is one, we respond to it.)
    let mut response_frame: Option<serde_json::Value> = None;
    for _ in 0..5 {
        let frame = recv(&mut reader);
        if frame.get("method").is_some() {
            // It's a HostCall — respond success.
            let call_id = frame["id"].clone();
            send(
                &mut child_stdin,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": call_id,
                    "result": "" // empty file content
                }),
            );
        } else {
            // It's the invokeTool response.
            response_frame = Some(frame);
            break;
        }
    }

    let resp = response_frame.expect("must have received invokeTool response within 5 frames");

    // The response MUST be a result, NOT an error triggered by the injected
    // push-response frame being misinterpreted as a request.
    assert!(
        resp.get("result").is_some(),
        "invokeTool after push-response injection must return a result, not an error; got: {resp}"
    );
    assert!(
        resp.get("error").is_none(),
        "invokeTool must not return an error; got: {resp}"
    );
    // The id must match the invokeTool request (id=1), not the push ACK (id=20000).
    assert_eq!(
        resp["id"],
        serde_json::json!(1),
        "response id must match invokeTool request id=1; got: {resp}"
    );

    // ── 4. shutdown ──────────────────────────────────────────────────────────
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": {}
        }),
    );
    let shutdown_resp = recv(&mut reader);
    assert!(
        shutdown_resp.get("result").is_some(),
        "shutdown must return a result; got: {shutdown_resp}"
    );

    let _ = child.wait();
}

// ── Concurrency stress (KNOWN PROTOCOL HAZARDS mandate: ≥ 20 parallel) ────────

/// KNOWN PROTOCOL HAZARDS §1 mandate: spawn ≥ 20 `lsp_extension` instances in
/// parallel, each running initialize → invokeTool(diagnostics) → shutdown.
/// Any race, deadlock, or protocol corruption surfaces as a timeout or panic.
#[test]
fn lsp_process_concurrent_spawn_stress_20_parallel() {
    const N: usize = 20;

    let bin = lsp_extension_bin();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let bin = bin.clone();
            std::thread::spawn(move || {
                let manifest = lsp_manifest(&bin);
                let deps = make_deps();

                let mut adapter = SidecarHostAdapter::spawn(
                    manifest,
                    deps,
                    TEST_TIMEOUT,
                    Some(empty_lsp_initialize_payload()),
                )
                .unwrap_or_else(|e| panic!("spawn #{i} failed: {e:?}"));

                // Call a tool that completes without a real LSP.
                let result = adapter
                    .call_tool("diagnostics", serde_json::json!({ "path": "src/main.rs" }))
                    .unwrap_or_else(|e| panic!("call_tool #{i} failed: {e:?}"));

                // Verify the result is sane (supported=false — no LSP configured).
                assert!(
                    result.is_object(),
                    "#{i}: result must be an object; got: {result}"
                );

                adapter.shutdown();
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join().unwrap_or_else(|_| panic!("thread #{i} panicked"));
    }
}

/// A 20-iteration parallel sidecar stress test covers LSP HostCalls made while
/// inbound frames may be queued.
#[test]
fn f007_t015_lsp_rename_hostcalls_queue_inbound_frames_under_20_iteration_parallel_stress() {
    const N: usize = 20;

    let handles = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let workspace = fake_lsp_workspace("ok");
                let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
                initialize_raw_lsp_child(&mut child);

                child.write_frame(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "invokeTool",
                    "params": {
                        "name": "rename",
                        "params": {
                            "path": "src/lib.rs",
                            "line": 0,
                            "character": 3,
                            "new_name": "new_name"
                        }
                    }
                }));

                let read = child.read_frame();
                assert_eq!(
                    read["method"], "workspace/readFile",
                    "#{i}: rename must request file content before LSP rename; got {read}"
                );
                child.write_frame(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "shutdown",
                    "params": {}
                }));
                child.write_frame(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": read["id"].clone(),
                    "result": "fn old_name() {}\n"
                }));

                respond_to_apply_edits(&mut child, false);

                let rename_response = child.read_frame();
                assert_eq!(
                    rename_response["id"],
                    serde_json::json!(1),
                    "#{i}: rename response must precede queued shutdown; got {rename_response}"
                );
                assert!(
                    rename_response.get("error").is_none(),
                    "#{i}: rename must not fail under queued-frame stress; got {rename_response}"
                );

                let shutdown_response = child.read_frame();
                assert_eq!(
                    shutdown_response["id"],
                    serde_json::json!(2),
                    "#{i}: queued shutdown must run after rename; got {shutdown_response}"
                );
            })
        })
        .collect::<Vec<_>>();

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("stress iteration #{i} panicked"));
    }
}

/// Same stress test but also injects push-response frames concurrently on each
/// instance to validate the hazard fix under load.
#[test]
fn lsp_process_concurrent_spawn_stress_20_with_push_response_injection() {
    use extension_protocol::PROTOCOL_VERSION;

    const N: usize = 20;

    let bin = lsp_extension_bin();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let bin = bin.clone();
            std::thread::spawn(move || {
                let mut child = Command::new(&bin)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap_or_else(|e| panic!("#{i}: spawn failed: {e}"));

                let mut stdin = child.stdin.take().unwrap();
                let stdout = child.stdout.take().unwrap();
                let mut reader = BufReader::new(stdout);

                let write_frame = |stdin: &mut std::process::ChildStdin,
                                   frame: serde_json::Value| {
                    let s = serde_json::to_string(&frame).unwrap();
                    stdin.write_all(s.as_bytes()).unwrap();
                    stdin.write_all(b"\n").unwrap();
                    stdin.flush().unwrap();
                };

                let read_frame =
                    |r: &mut BufReader<std::process::ChildStdout>| -> serde_json::Value {
                        let mut line = String::new();
                        r.read_line(&mut line).unwrap();
                        serde_json::from_str(line.trim()).expect("valid JSON")
                    };

                // initialize
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 0,
                        "method": "initialize",
                        "params": {
                            "protocol_version": PROTOCOL_VERSION,
                            "client_info": "stress-test/0.1.0",
                            "extension_config": empty_lsp_initialize_payload(),
                        }
                    }),
                );
                let init = read_frame(&mut reader);
                assert!(
                    init.get("result").is_some(),
                    "#{i}: initialize must succeed; got: {init}"
                );

                // Inject push-response frame (simulates notify/resourceUpdated ACK).
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 20000_u64 + i as u64,
                        "result": true
                    }),
                );

                // invokeTool(diagnostics) — must succeed despite the injected frame.
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "method": "invokeTool",
                        "params": { "name": "diagnostics", "params": { "path": "src/main.rs" } }
                    }),
                );

                // drain any HostCalls and collect the invokeTool response
                let mut invoke_resp: Option<serde_json::Value> = None;
                for _ in 0..5 {
                    let frame = read_frame(&mut reader);
                    if frame.get("method").is_some() {
                        let call_id = frame["id"].clone();
                        write_frame(
                            &mut stdin,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": call_id,
                                "result": ""
                            }),
                        );
                    } else {
                        invoke_resp = Some(frame);
                        break;
                    }
                }
                let resp = invoke_resp.unwrap_or_else(|| panic!("#{i}: no invokeTool response"));
                assert!(
                    resp.get("error").is_none(),
                    "#{i}: invokeTool must not error after push-response injection; got: {resp}"
                );
                assert_eq!(
                    resp["id"],
                    serde_json::json!(1),
                    "#{i}: response id must be 1; got: {resp}"
                );

                // shutdown
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 2,
                        "method": "shutdown",
                        "params": {}
                    }),
                );
                let shutdown = read_frame(&mut reader);
                assert!(
                    shutdown.get("result").is_some(),
                    "#{i}: shutdown must succeed; got: {shutdown}"
                );

                let _ = child.wait();
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join()
            .unwrap_or_else(|_| panic!("stress thread #{i} panicked"));
    }
}

#[test]
fn safe_delete_without_cleanup_completes_only_for_wholly_internal_references() {
    let consumer = "old_name();\n";
    for (mode, content, expected) in [
        (
            "safe_delete_no_cleanup_internal",
            "fn old_name() { old_name(); }\n",
            "complete",
        ),
        (
            "safe_delete_no_cleanup_internal",
            "fn old_name() { /*😀*/ old_name(); }\r\n",
            "complete",
        ),
        (
            "safe_delete_no_cleanup_mixed",
            "fn old_name() { old_name(); }\n",
            "incomplete",
        ),
        (
            "safe_delete_no_cleanup_external_same_file",
            "fn old_name() {}\nfn keep() { old_name(); }\n",
            "incomplete",
        ),
        (
            "safe_delete_no_cleanup_crossing",
            "fn old_name() {}\nfn keep() {}\n",
            "incomplete",
        ),
    ] {
        let workspace = fake_lsp_workspace(mode);
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let mut request = safe_delete_request(content);
        request["declaration"]["deletion_span"]["end_byte"] =
            serde_json::json!(content.find('\n').unwrap() + 1);
        let (response, host_calls) = invoke_safe_delete(
            &mut child,
            5201,
            request,
            &[("src/lib.rs", content), ("src/consumer.rs", consumer)],
        );
        let data = safe_delete_data(&response);
        assert_eq!(data["status"], expected, "{mode}: {data}");
        assert_eq!(data["plan"]["coverage"], expected);
        assert!(!data["plan"]["references"].as_array().unwrap().is_empty());
        assert!(data["plan"]["cleanup_edits"].as_array().unwrap().is_empty());
        assert_no_mutation_host_calls(&host_calls);
    }
}

#[test]
fn safe_delete_rejects_invalid_deletion_span_boundaries_before_backend_lookup() {
    let content = "fn old_name() { /*😀*/ old_name(); }\n";
    let split = content.find('😀').unwrap() + 1;
    for (start, end) in [(0, content.len() + 1), (split, content.len()), (0, split)] {
        let workspace = fake_lsp_workspace("safe_delete_no_cleanup_internal");
        let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
        initialize_raw_lsp_child_bounded(&mut child);
        let mut request = safe_delete_request(content);
        request["declaration"]["deletion_span"] =
            serde_json::json!({"start_byte": start, "end_byte": end});
        let (response, host_calls) =
            invoke_safe_delete(&mut child, 5202, request, &[("src/lib.rs", content)]);
        assert_eq!(
            safe_delete_data(&response)["status"],
            "invalid_location",
            "{response}"
        );
        assert_no_mutation_host_calls(&host_calls);
        let log =
            fs::read_to_string(workspace.path().join(".tower/fake_lsp.log")).unwrap_or_default();
        assert!(
            !log.contains("textDocument/references"),
            "invalid span reached backend: {log}"
        );
    }
}

#[test]
fn safe_delete_tool_names_use_snake_case_and_match_the_static_manifests() {
    let mut child = RawLspChild::spawn();
    let initialized = initialized_raw_lsp_child(&mut child);
    let runtime: Vec<extension_protocol::ToolDecl> =
        serde_json::from_value(initialized["result"]["data"]["tools"].clone()).unwrap();
    let mut runtime_names = runtime
        .iter()
        .filter(|tool| tool.name != "_tower_catch_up_fence")
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    runtime_names.sort();
    for extension in ["ast", "lsp"] {
        let source = fs::read_to_string(
            workspace_root().join(format!("extensions/{extension}/extension.toml")),
        )
        .unwrap();
        let manifest: ExtensionManifest = toml::from_str(&source).unwrap();
        for tool in &manifest.tools {
            assert!(
                !tool.name.is_empty()
                    && tool.name.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                "{extension} tool must use snake_case: {}",
                tool.name
            );
        }
        if extension == "lsp" {
            let mut declared_names = manifest
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>();
            declared_names.sort();
            assert_eq!(
                runtime_names, declared_names,
                "static and runtime LSP names must agree"
            );
            for name in ["plan_path_rename", "plan_safe_delete"] {
                let declared = manifest
                    .tools
                    .iter()
                    .find(|tool| tool.name == name)
                    .unwrap();
                let actual = runtime.iter().find(|tool| tool.name == name).unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&actual.schema_json).unwrap(),
                    serde_json::from_str::<serde_json::Value>(&declared.schema_json).unwrap()
                );
            }
        }
    }
}
