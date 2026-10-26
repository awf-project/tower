//! End-to-end integration tests for the native `tower_*` MCP tools (spec 10b).
//!
//! All tests use an in-process rmcp server driven via `tokio::io::duplex`.
//! No spawned processes, no real stdio.
//!
//! # TDD sequence (spec 10b §TDD sequence)
//!
//! 1. AC1 — `tools/list` returns the native `tower_*` tools with schemas.
//! 2. AC2 — `tower_find_file` round-trip.
//! 3. AC3 — `tower_create_file` then `tower_find_file` finds it.
//! 4. AC4 — malformed args → `invalid-params` protocol error, no state change.
//! 5. AC5 — `tower_delete_file` on missing file → is_error:true result.

// Feature: F001

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, RwLock};

use extension_protocol::messages::{
    PATH_RENAME_REQUEST_SCHEMA_JSON, PathRenameError, PathRenameErrorCode,
    PathRenameRequest as WirePathRenameRequest, PathRenameResult as WirePathRenameResult,
};
use rmcp::model::CallToolRequestParams;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

use core_engine::adapters::fs::WorkspacePathExclusion;
use core_engine::adapters::mcp::diagnostics::NoOpDiagnosticsReader;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::lsp_tools::SubscriptionRegistry;
use core_engine::adapters::mcp::native_tools::{
    EXPECTED_NATIVE_TOOL_NAMES, EngineState, NativeToolRegistry,
};
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::mcp::rmcp_server::TowerMcpHandler;
use core_engine::adapters::{InMemoryFs, InMemoryStorage, RealFs};
use core_engine::domain::RelativePath;
use core_engine::domain::extension_host::ExtensionRegistry;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::mutation::FileMutationService;
use core_engine::domain::refactor::{
    PathRenameError as DomainPathRenameError, PathRenameRequest, PathRenameSemanticPolicy,
};
use core_engine::domain::token::tokenize;
use core_engine::domain::virtual_file::FileMetadata;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::{
    FileMutationUseCase, FileSystemPort, NoOpExtensionHost, PortError, ReconciliationDelta,
    StoragePort,
};

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Empty engine state — nothing indexed.
fn empty_state() -> Arc<RwLock<EngineState>> {
    Arc::new(RwLock::new(EngineState::new(
        ProjectWorkspace::new(),
        InvertedIndex::new(),
        Box::new(InMemoryStorage::new()),
        Box::new(InMemoryFs::new()),
    )))
}

/// State with one indexed file `src/client.rs` containing `"fn client() {}"`.
fn state_with_client_file() -> Arc<RwLock<EngineState>> {
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut fs = InMemoryFs::new();
    let storage = InMemoryStorage::new();

    let path = RelativePath::new("src/client.rs");
    let id = workspace
        .insert(path.clone(), FileMetadata::default())
        .unwrap();
    index.insert(id, &tokenize("src/client.rs"));
    fs.write(path, b"fn client() {}".to_vec()).unwrap();

    Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(storage),
        Box::new(fs),
    )))
}

fn state_with_list_dir_files() -> Arc<RwLock<EngineState>> {
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut fs = InMemoryFs::new();
    let storage = InMemoryStorage::new();

    for (path, content) in [
        ("src/a.rs", "fn a() {}"),
        ("src/b.rs", "fn b() {}"),
        ("src/net/c.rs", "fn c() {}"),
    ] {
        let path = RelativePath::new(path);
        let id = workspace
            .insert(path.clone(), FileMetadata::default())
            .unwrap();
        index.insert(id, &tokenize(path.as_str()));
        fs.write(path, content.as_bytes().to_vec()).unwrap();
    }

    Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(storage),
        Box::new(fs),
    )))
}

/// Build a `TowerMcpHandler` backed by the given engine state and no extensions.
fn make_handler(state: Arc<RwLock<EngineState>>) -> TowerMcpHandler {
    let ext_reg = Arc::new(RwLock::new(ExtensionRegistry::new()));
    let merged = ExtensionMergedRegistry::new(state, ext_reg);
    let sub_reg = Arc::new(Mutex::new(SubscriptionRegistry::new()));
    let diag_reader = Arc::new(NoOpDiagnosticsReader);
    TowerMcpHandler::new(merged, diag_reader, sub_reg, vec![])
}

/// Spin up a real rmcp server around `handler` and return a connected client.
async fn start_server(handler: TowerMcpHandler) -> rmcp::service::RunningService<RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    tokio::spawn(async move {
        if let Ok(server) = handler.serve(server_transport).await {
            let _ = server.waiting().await;
        }
    });
    ().serve(client_transport)
        .await
        .expect("client connect failed")
}

/// Run an async test body synchronously using a new tokio runtime.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

/// Extract the text payload from a `CallToolResult`'s first content item.
fn first_text(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .first()
        .and_then(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

async fn call_list_dir(
    client: &rmcp::service::RunningService<RoleClient, ()>,
    args: Value,
) -> Result<rmcp::model::CallToolResult, rmcp::ServiceError> {
    client
        .peer()
        .call_tool(
            CallToolRequestParams::new("tower_list_dir")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
}

fn list_dir_entries(result: &rmcp::model::CallToolResult) -> Vec<(String, String, String)> {
    let text = first_text(result);
    let payload: Value = serde_json::from_str(text).expect("content must be JSON");
    payload["entries"]
        .as_array()
        .expect("payload must contain entries")
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().unwrap().to_owned(),
                entry["name"].as_str().unwrap().to_owned(),
                entry["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn invalid_params_error(result: Result<rmcp::model::CallToolResult, rmcp::ServiceError>) -> String {
    match result.expect_err("call must return a protocol error") {
        rmcp::ServiceError::McpError(e) => {
            assert_eq!(e.code.0, -32602, "must be -32602 (InvalidParams)");
            e.message.to_string()
        }
        other => panic!("expected McpError, got: {other:?}"),
    }
}

fn native_registry_tool_names(state: Arc<RwLock<EngineState>>) -> Vec<String> {
    NativeToolRegistry::new(state)
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

// ── AC1: tools/list returns native tower_* tools with schemas ────────────────

#[test]
fn tools_list_returns_native_tools_with_schemas() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .list_tools(Default::default())
            .await
            .expect("list_tools failed");

        assert_eq!(
            result.tools.len(),
            EXPECTED_NATIVE_TOOL_NAMES.len(),
            "unexpected tower_* tool count; got {}; names: {:?}",
            result.tools.len(),
            result.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );

        let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
        let registry_names = native_registry_tool_names(empty_state());
        let registry_actual: Vec<&str> = registry_names.iter().map(String::as_str).collect();
        assert_eq!(
            names, registry_actual,
            "integration tools/list must publish the same ordered names as NativeToolRegistry::list"
        );

        for expected_name in EXPECTED_NATIVE_TOOL_NAMES {
            assert!(
                names.contains(&expected_name),
                "missing tool '{expected_name}' in tools/list; got {names:?}"
            );
        }

        client.cancel().await.expect("cancel failed");
    });
}

// ── T008: tower_list_dir MCP integration coverage ───────────────────────────

#[test]
fn list_dir_tools_list_exposes_object_schema() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .list_tools(Default::default())
            .await
            .expect("list_tools failed");
        let tool = result
            .tools
            .iter()
            .find(|tool| tool.name == "tower_list_dir")
            .expect("tools/list must expose tower_list_dir");

        assert_eq!(tool.input_schema["type"], "object");
        assert!(
            tool.input_schema["properties"].is_object(),
            "tower_list_dir must expose object input properties"
        );

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn list_dir_returns_direct_and_recursive_entries() {
    block_on(async {
        let state = state_with_list_dir_files();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = call_list_dir(&client, json!({ "path": "src" }))
            .await
            .expect("tower_list_dir must not return a protocol error");

        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            list_dir_entries(&result),
            vec![
                ("src/a.rs".to_owned(), "a.rs".to_owned(), "file".to_owned()),
                ("src/b.rs".to_owned(), "b.rs".to_owned(), "file".to_owned()),
                ("src/net".to_owned(), "net".to_owned(), "dir".to_owned()),
            ]
        );

        let text = first_text(&result);
        let payload: Value = serde_json::from_str(text).expect("content must be JSON");
        let entries = payload["entries"]
            .as_array()
            .expect("entries must be array");

        for entry in entries {
            assert!(entry.get("path").and_then(Value::as_str).is_some());
            assert!(entry.get("name").and_then(Value::as_str).is_some());
            assert!(entry.get("kind").and_then(Value::as_str).is_some());
        }

        let recursive_result = call_list_dir(&client, json!({ "path": "src", "recursive": true }))
            .await
            .expect("tower_list_dir must not return a protocol error");
        let recursive_entries = list_dir_entries(&recursive_result);

        assert!(
            recursive_entries
                .iter()
                .any(|(path, name, kind)| path == "src/net/c.rs"
                    && name == "c.rs"
                    && kind == "file"),
            "recursive listing must include src/net/c.rs; got {recursive_entries:?}"
        );

        let depth_limited_result = call_list_dir(
            &client,
            json!({ "path": "src", "recursive": true, "max_depth": 1 }),
        )
        .await
        .expect("tower_list_dir must not return a protocol error");
        let depth_limited_entries = list_dir_entries(&depth_limited_result);

        assert!(
            depth_limited_entries
                .iter()
                .any(|(path, name, kind)| path == "src/net" && name == "net" && kind == "dir"),
            "max_depth:1 listing must include src/net; got {depth_limited_entries:?}"
        );
        assert!(
            !depth_limited_entries
                .iter()
                .any(|(path, _, _)| path == "src/net/c.rs"),
            "max_depth:1 listing must exclude src/net/c.rs; got {depth_limited_entries:?}"
        );

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn list_dir_missing_prefix_returns_empty_entries() {
    block_on(async {
        let state = state_with_list_dir_files();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = call_list_dir(&client, json!({ "path": "missing" }))
            .await
            .expect("tower_list_dir must not return a protocol error");

        assert_eq!(result.is_error, Some(false));
        assert!(
            list_dir_entries(&result).is_empty(),
            "missing prefix must return an empty entries array"
        );

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn list_dir_rejects_invalid_depth_options() {
    block_on(async {
        let state = state_with_list_dir_files();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let message = invalid_params_error(
            call_list_dir(&client, json!({ "path": "src", "max_depth": 1 })).await,
        );

        assert!(
            message.contains("max_depth"),
            "InvalidParams message must mention max_depth; got {message:?}"
        );

        let message = invalid_params_error(
            call_list_dir(
                &client,
                json!({ "path": "src", "recursive": true, "max_depth": 0 }),
            )
            .await,
        );

        assert!(
            message.contains("max_depth"),
            "InvalidParams message must mention max_depth; got {message:?}"
        );

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn list_dir_tracked_file_path_returns_invalid_params_without_entries_payload() {
    block_on(async {
        let state = state_with_list_dir_files();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let message =
            invalid_params_error(call_list_dir(&client, json!({ "path": "src/a.rs" })).await);

        assert!(
            message.contains("src/a.rs"),
            "NotADirectory InvalidParams message must include requested path; got {message:?}"
        );

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn list_dir_after_create_file_shows_newly_tracked_file() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let create_result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_create_file").with_arguments(
                    json!({"path": "src/generated.rs", "content": "pub fn generated() {}"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("create_file call must not return protocol error");
        assert_eq!(create_result.is_error, Some(false));

        let result = call_list_dir(&client, json!({ "path": "src" }))
            .await
            .expect("tower_list_dir must not return a protocol error");
        let entries = list_dir_entries(&result);

        assert!(
            entries.iter().any(|(path, name, kind)| {
                path == "src/generated.rs" && name == "generated.rs" && kind == "file"
            }),
            "list_dir must include newly created file; got {entries:?}"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── AC2: tower_find_file round-trip ───────────────────────────────────────────

#[test]
fn ac2_find_file_returns_matching_paths() {
    block_on(async {
        let state = state_with_client_file();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_find_file")
                    .with_arguments(json!({"query": "client"}).as_object().unwrap().clone()),
            )
            .await
            .expect("call_tool must not return a protocol error");

        assert_eq!(
            result.is_error,
            Some(false),
            "find_file must succeed: {result:?}"
        );

        let text = first_text(&result);
        let payload: Value = serde_json::from_str(text).expect("content must be JSON");
        let paths = payload["paths"]
            .as_array()
            .expect("payload must have 'paths'");
        let path_strings: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();

        assert!(
            path_strings.contains(&"src/client.rs"),
            "tower_find_file must return src/client.rs; got {path_strings:?}"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── AC3: tower_create_file then tower_find_file finds it ─────────────────────

#[test]
fn ac3_create_file_then_find_file_locates_new_file() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        // Step 1: create.
        let create_result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_create_file").with_arguments(
                    json!({"path": "src/widget.rs", "content": "pub struct Widget;"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("create_file call must not return protocol error");

        assert_eq!(
            create_result.is_error,
            Some(false),
            "create_file must succeed: {create_result:?}"
        );

        // Step 2: find.
        let find_result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_find_file")
                    .with_arguments(json!({"query": "widget"}).as_object().unwrap().clone()),
            )
            .await
            .expect("find_file call must not return protocol error");

        assert_eq!(
            find_result.is_error,
            Some(false),
            "find_file must succeed: {find_result:?}"
        );

        let text = first_text(&find_result);
        let payload: Value = serde_json::from_str(text).unwrap();
        let paths = payload["paths"].as_array().unwrap();
        let path_strings: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
        assert!(
            path_strings.contains(&"src/widget.rs"),
            "newly created file must be findable via tower_find_file; got {path_strings:?}"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── AC4: malformed args → protocol-level InvalidParams error ─────────────────

#[test]
fn ac4_missing_query_returns_invalid_params() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        // Call tower_find_file without the required 'query' field.
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_find_file")
                    .with_arguments(serde_json::Map::new()), // missing "query"
            )
            .await;

        // InvalidArgs → rmcp protocol error (Err).
        assert!(
            result.is_err(),
            "missing 'query' must return a protocol error (InvalidParams)"
        );
        let svc_err = result.unwrap_err();
        let code = match svc_err {
            rmcp::ServiceError::McpError(e) => e.code.0,
            other => panic!("expected McpError, got: {other:?}"),
        };
        assert_eq!(code, -32602, "must be -32602 (InvalidParams)");

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn ac4_missing_content_for_create_file_returns_invalid_params() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_create_file")
                    .with_arguments(json!({"path": "ghost.rs"}).as_object().unwrap().clone()),
            )
            .await;

        assert!(
            result.is_err(),
            "missing 'content' must return a protocol error"
        );
        let code = match result.unwrap_err() {
            rmcp::ServiceError::McpError(e) => e.code.0,
            other => panic!("expected McpError, got: {other:?}"),
        };
        assert_eq!(code, -32602);

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn ac4_invalid_args_cause_no_state_change() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        // Try to create with missing 'content' — must fail without touching state.
        let _ = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_create_file")
                    .with_arguments(json!({"path": "ghost.rs"}).as_object().unwrap().clone()),
            )
            .await; // error expected; we don't assert here

        // find_file must return nothing — ghost.rs was not created.
        let find_result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_find_file")
                    .with_arguments(json!({"query": "ghost"}).as_object().unwrap().clone()),
            )
            .await
            .expect("find_file must not return protocol error");

        let text = first_text(&find_result);
        let payload: Value = serde_json::from_str(text).unwrap();
        let paths = payload["paths"].as_array().unwrap();
        assert!(
            paths.is_empty(),
            "failed create_file must leave workspace unchanged; ghost.rs must not be findable"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── AC5: tower_delete_file on missing file → is_error:true ───────────────────
//
// In the rmcp migration, tool-execution errors are returned as CallToolResult
// with is_error:true (per MCP spec). ResourceNotFound is a tool execution error,
// not a protocol error, so it comes back as Ok(is_error:true) rather than
// a JSON-RPC error response.

#[test]
fn ac5_delete_missing_file_returns_is_error_true() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_delete_file").with_arguments(
                    json!({"path": "does_not_exist.rs"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("delete_file must succeed at the RPC level");

        assert_eq!(
            result.is_error,
            Some(true),
            "delete of missing file must return is_error:true"
        );

        let text = first_text(&result);
        assert!(
            text.to_lowercase().contains("not found") || text.to_lowercase().contains("resource"),
            "error message must mention not-found; got: {text}"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── Multi-tool sequence: create → search → delete ─────────────────────────────

#[test]
fn full_sequence_create_search_delete() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        // 1. Create a file.
        let r1 = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_create_file").with_arguments(
                    json!({"path": "src/engine.rs", "content": "pub fn run() {}"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("create must not return protocol error");
        assert_eq!(r1.is_error, Some(false), "create must succeed: {r1:?}");

        // 2. Search for content inside the file.
        let r2 = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_search_text")
                    .with_arguments(json!({"pattern": "run"}).as_object().unwrap().clone()),
            )
            .await
            .expect("search must not return protocol error");
        assert_eq!(r2.is_error, Some(false), "search must succeed: {r2:?}");
        let text2 = first_text(&r2);
        let payload2: Value = serde_json::from_str(text2).unwrap();
        let matches = payload2["matches"].as_array().unwrap();
        assert!(!matches.is_empty(), "search_text must find 'run'");

        // 3. Delete the file.
        let r3 = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_delete_file").with_arguments(
                    json!({"path": "src/engine.rs"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("delete must not return protocol error");
        assert_eq!(r3.is_error, Some(false), "delete must succeed: {r3:?}");

        // 4. Find should now return nothing.
        let r4 = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_find_file")
                    .with_arguments(json!({"query": "engine"}).as_object().unwrap().clone()),
            )
            .await
            .expect("find_file must not return protocol error");
        let text4 = first_text(&r4);
        let payload4: Value = serde_json::from_str(text4).unwrap();
        assert!(
            payload4["paths"].as_array().unwrap().is_empty(),
            "deleted file must not be findable"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── tower_global_replace ──────────────────────────────────────────────────────

#[test]
fn global_replace_returns_files_changed_count() {
    block_on(async {
        let state = state_with_client_file();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_global_replace").with_arguments(
                    json!({"target": "client", "replacement": "server"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("global_replace must not return protocol error");

        assert_eq!(
            result.is_error,
            Some(false),
            "global_replace must succeed: {result:?}"
        );

        let text = first_text(&result);
        let payload: Value = serde_json::from_str(text).unwrap();
        assert!(
            payload["files_changed"].as_u64().unwrap_or(0) >= 1,
            "global_replace must report files_changed >= 1"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── tower_read_file ───────────────────────────────────────────────────────────

#[test]
fn read_file_returns_content() {
    block_on(async {
        let state = state_with_client_file();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_read_file").with_arguments(
                    json!({"path": "src/client.rs"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("read_file must not return protocol error");

        assert_eq!(
            result.is_error,
            Some(false),
            "read_file must succeed: {result:?}"
        );

        let text = first_text(&result);
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            payload["content"].as_str().unwrap(),
            "fn client() {}",
            "read_file must return exact content"
        );

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn read_file_on_missing_path_returns_is_error_true() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_read_file")
                    .with_arguments(json!({"path": "missing.rs"}).as_object().unwrap().clone()),
            )
            .await
            .expect("read_file must succeed at RPC level");

        assert_eq!(
            result.is_error,
            Some(true),
            "read_file on missing path must return is_error:true"
        );

        client.cancel().await.expect("cancel failed");
    });
}

// ── tower_create_directory ────────────────────────────────────────────────────

#[test]
fn create_directory_succeeds() {
    block_on(async {
        let state = empty_state();
        let handler = make_handler(state);
        let client = start_server(handler).await;

        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_create_directory")
                    .with_arguments(json!({"path": "a/b/c"}).as_object().unwrap().clone()),
            )
            .await
            .expect("create_directory must not return protocol error");

        assert_eq!(
            result.is_error,
            Some(false),
            "create_directory must succeed: {result:?}"
        );

        let text = first_text(&result);
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["created"], true);

        client.cancel().await.expect("cancel failed");
    });
}

// ── tower_rename_path ───────────────────────────────────────────────────────

fn call_native_rename(
    state: Arc<RwLock<EngineState>>,
    args: Value,
) -> Result<Value, core_engine::adapters::mcp::ToolError> {
    NativeToolRegistry::new(state).call(
        "tower_rename_path",
        args,
        &core_engine::domain::NeverCancelled,
    )
}

/// Canonical public examples.  Keep these literals independent of the
/// serializer under test so a wire-format regression cannot update both sides
/// of an assertion at once.
fn rename_result_fixture(status: &str) -> Value {
    let plan = json!({
        "moves": [{"source": "src/old.rs", "destination": "src/new.rs", "kind": "file"}],
        "edits": [],
        "semantic_policy": "none"
    });

    match status {
        "preview" => json!({"status": "preview", "plan": plan, "errors": []}),
        "applied" => json!({"status": "applied", "plan": plan, "errors": []}),
        "unsupported" => json!({
            "status": "unsupported",
            "plan": plan,
            "errors": [{"code": "semantic_unsupported", "message": "Semantic rename is unavailable"}]
        }),
        "rejected" => json!({
            "status": "rejected",
            "plan": {"moves": [], "edits": [], "semantic_policy": "none"},
            "errors": [{"code": "source_not_found", "path": "missing.rs", "message": "Source path was not found"}]
        }),
        "partial" => json!({
            "status": "partial",
            "plan": plan,
            "errors": [{"code": "persistence_error", "message": "Persistence operation failed"}]
        }),
        other => panic!("unknown path-rename fixture status: {other}"),
    }
}

fn typed_rename_result_fixture(status: &str) -> WirePathRenameResult {
    serde_json::from_value(rename_result_fixture(status)).unwrap_or_else(|error| {
        panic!("{status} fixture must deserialize as PathRenameResult: {error}")
    })
}

fn state_with_paths(paths: &[(&str, &str)]) -> Arc<RwLock<EngineState>> {
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut fs = InMemoryFs::new();

    for (path, content) in paths {
        let path = RelativePath::new(*path);
        let id = workspace
            .insert(path.clone(), FileMetadata::default())
            .unwrap();
        index.insert(id, &tokenize(path.as_str()));
        fs.write(path, content.as_bytes().to_vec()).unwrap();
    }

    Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(InMemoryStorage::new()),
        Box::new(fs),
    )))
}

#[derive(Default)]
struct RenamePersistenceFailure {
    inner: InMemoryStorage,
}

impl StoragePort for RenamePersistenceFailure {
    fn apply_reconciliation_delta(&mut self, _: &ReconciliationDelta) -> Result<(), PortError> {
        Err(PortError::WriteFailed(
            "PRIVATE_PERSISTENCE_SENTINEL".to_owned(),
        ))
    }

    fn get(
        &self,
        id: core_engine::domain::FileId,
    ) -> Result<core_engine::domain::VirtualFile, PortError> {
        self.inner.get(id)
    }

    fn put(&mut self, file: core_engine::domain::VirtualFile) -> Result<(), PortError> {
        self.inner.put(file)
    }

    fn put_batch(&mut self, files: &[core_engine::domain::VirtualFile]) -> Result<(), PortError> {
        self.inner.put_batch(files)
    }

    fn delete(&mut self, id: core_engine::domain::FileId) -> Result<(), PortError> {
        self.inner.delete(id)
    }

    fn put_blob(
        &mut self,
        hash: core_engine::domain::ContentHash,
        bytes: Vec<u8>,
    ) -> Result<(), PortError> {
        self.inner.put_blob(hash, bytes)
    }

    fn get_blob(&self, hash: &core_engine::domain::ContentHash) -> Result<Vec<u8>, PortError> {
        self.inner.get_blob(hash)
    }

    fn mark_scan_complete(&mut self) -> Result<(), PortError> {
        self.inner.mark_scan_complete()
    }

    fn is_scan_complete(&self) -> Result<bool, PortError> {
        self.inner.is_scan_complete()
    }
}

#[test]
fn rename_path_is_registered_with_the_exact_public_schema_and_dispatch() {
    let state = empty_state();
    let mut registry = NativeToolRegistry::new(state);
    let descriptor = registry
        .list()
        .into_iter()
        .find(|tool| tool.name == "tower_rename_path")
        .expect("tower_rename_path must be listed");

    assert_eq!(
        descriptor.input_schema,
        json!({
            "type": "object",
            "properties": {
                "source": {"type": "string"},
                "destination": {"type": "string"},
                "dry_run": {"type": "boolean", "default": false},
                "semantic_policy": {"type": "string", "enum": ["none", "best_effort", "required"]},
                "expected_versions": {
                    "type": "object",
                    "additionalProperties": {"type": "string", "pattern": "^[0-9A-Fa-f]{64}$"},
                    "default": {}
                }
            },
            "required": ["source", "destination", "semantic_policy"],
            "additionalProperties": false
        })
    );
    assert_eq!(
        descriptor.input_schema,
        serde_json::from_str::<Value>(PATH_RENAME_REQUEST_SCHEMA_JSON)
            .expect("canonical request schema must be valid JSON"),
        "the registered descriptor must remain independently schema-compatible"
    );

    let request_fixture = json!({
        "source": "src/old.rs",
        "destination": "src/new.rs",
        "dry_run": false,
        "semantic_policy": "none",
        "expected_versions": {"src/old.rs": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
    });
    let typed_request: WirePathRenameRequest = serde_json::from_value(request_fixture.clone())
        .expect("canonical request fixture must deserialize through the public DTO");
    assert_eq!(
        serde_json::to_value(typed_request).expect("request fixture must serialize"),
        request_fixture,
        "typed request serialization must retain the exact public fixture"
    );

    for status in ["preview", "applied", "unsupported", "rejected", "partial"] {
        assert_eq!(
            serde_json::to_value(typed_rename_result_fixture(status))
                .expect("result fixture must serialize"),
            rename_result_fixture(status),
            "typed {status} result serialization must retain the entire public fixture"
        );
    }

    let error = registry
        .call(
            "tower_rename_path",
            json!({"source": "missing.rs", "destination": "renamed.rs", "semantic_policy": "none"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("domain rejection must be a successful structured payload");
    assert_eq!(error, rename_result_fixture("rejected"));
}

#[test]
fn rename_path_validates_all_public_fields_and_malformed_cas_fails_closed() {
    let malformed = [
        json!({"source": 7, "destination": "b.rs", "semantic_policy": "none"}),
        json!({"source": "a.rs", "destination": false, "semantic_policy": "none"}),
        json!({"source": "a.rs", "destination": "b.rs", "dry_run": "yes", "semantic_policy": "none"}),
        json!({"source": "a.rs", "destination": "b.rs", "semantic_policy": "sometimes"}),
        json!({"source": "a.rs", "destination": "b.rs", "semantic_policy": "none", "expected_versions": []}),
        json!({"source": "a.rs", "destination": "b.rs", "semantic_policy": "none", "expected_versions": {"a.rs": 42}}),
        json!({"source": "a.rs", "destination": "b.rs", "semantic_policy": "none", "expected_versions": {"a.rs": "abc"}}),
        json!({"source": "a.rs", "destination": "b.rs", "semantic_policy": "none", "unknown": true}),
    ];

    for args in malformed {
        let error =
            call_native_rename(empty_state(), args).expect_err("malformed request must fail");
        assert!(
            matches!(error, core_engine::adapters::mcp::ToolError::InvalidArgs(_)),
            "malformed request must map to invalid arguments: {error:?}"
        );
    }
}

#[test]
fn rename_path_error_code_fixtures_are_one_to_one_and_use_public_messages() {
    let cases = [
        (
            PathRenameErrorCode::SourceNotFound,
            "source_not_found",
            "Source path was not found",
        ),
        (
            PathRenameErrorCode::DestinationExists,
            "destination_exists",
            "Destination path already exists",
        ),
        (
            PathRenameErrorCode::IdenticalPaths,
            "identical_paths",
            "Source and destination paths are identical",
        ),
        (
            PathRenameErrorCode::DestinationInsideSource,
            "destination_inside_source",
            "Destination is inside the source path",
        ),
        (
            PathRenameErrorCode::DestinationExcluded,
            "destination_excluded",
            "Destination path is excluded",
        ),
        (
            PathRenameErrorCode::WorkspaceEscape,
            "workspace_escape",
            "Path is outside the workspace",
        ),
        (
            PathRenameErrorCode::DuplicateDestination,
            "duplicate_destination",
            "Multiple moves have the same destination",
        ),
        (
            PathRenameErrorCode::InvalidExpectedVersion,
            "invalid_expected_version",
            "Expected version is invalid",
        ),
        (
            PathRenameErrorCode::SemanticUnsupported,
            "semantic_unsupported",
            "Semantic rename is unavailable",
        ),
        (
            PathRenameErrorCode::SemanticBackend,
            "semantic_backend",
            "Semantic planning failed",
        ),
        (
            PathRenameErrorCode::FilesystemError,
            "filesystem_error",
            "Filesystem operation failed",
        ),
        (
            PathRenameErrorCode::CasConflict,
            "cas_conflict",
            "File changed before semantic edits were applied",
        ),
        (
            PathRenameErrorCode::PersistenceError,
            "persistence_error",
            "Persistence operation failed",
        ),
    ];

    for (code, expected_code, expected_message) in cases {
        let error = PathRenameError {
            code,
            path: Some("src/public.rs".to_owned()),
            // The serializer must discard this backend string, not preserve it.
            message: "PRIVATE_RAW_BACKEND_SENTINEL".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(error).expect("public error fixture must serialize"),
            json!({"code": expected_code, "path": "src/public.rs", "message": expected_message}),
            "{expected_code} must have its own stable public code and message"
        );
    }
}

#[test]
fn rename_path_error_serialization_redacts_all_forbidden_boundary_sentinels() {
    let sentinels = [
        "/absolute/PRIVATE_ABSOLUTE_PATH_SENTINEL",
        "file:///PRIVATE_EXTERNAL_URI_SENTINEL",
        "PRIVATE_REPLACEMENT_CONTENT_SENTINEL",
        "PRIVATE_WORKSPACE_EXTERNAL_CONTENT_SENTINEL",
        "PRIVATE_RAW_BACKEND_SENTINEL",
    ];
    let error = PathRenameError {
        code: PathRenameErrorCode::FilesystemError,
        path: Some(sentinels[0].to_owned()),
        message: sentinels.join(" "),
    };
    let serialized = serde_json::to_string(&error).expect("public error must serialize");

    for sentinel in sentinels {
        assert!(
            !serialized.contains(sentinel),
            "the public error/message boundary must redact {sentinel}"
        );
    }
    assert_eq!(
        serde_json::from_str::<Value>(&serialized).expect("error JSON"),
        json!({"code": "filesystem_error", "message": "Filesystem operation failed"})
    );

    let preview = json!({
        "status": "preview",
        "plan": {
            "moves": [{"source": "src/old.rs", "destination": "src/new.rs", "kind": "file"}],
            "edits": [{
                "path": "src/old.rs",
                "start_byte": 0,
                "end_byte": 2,
                "replacement": "PRIVATE_REPLACEMENT_CONTENT_SENTINEL",
                "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }],
            "semantic_policy": "best_effort"
        },
        "errors": []
    });
    let typed_preview: WirePathRenameResult = serde_json::from_value(preview.clone())
        .expect("successful preview fixture must be a typed public result");
    assert_eq!(
        serde_json::to_value(typed_preview).expect("preview serialization"),
        preview,
        "replacement content is intentionally retained in successful preview edits"
    );
}

#[test]
fn rename_path_rejects_an_ignored_destination_via_the_real_exclusion_adapter_without_mutation() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(root.path().join(".towerignore"), b"ignored.rs\n")
        .expect("write real ignore policy");
    std::fs::create_dir_all(root.path().join("src")).expect("create source directory");
    std::fs::write(root.path().join("src/old.rs"), b"fn old() {}").expect("write source file");

    let source = RelativePath::new("src/old.rs");
    let mut workspace = ProjectWorkspace::new();
    let id = workspace
        .insert(source.clone(), FileMetadata::default())
        .expect("seed workspace source");
    let before_workspace: Vec<_> = workspace
        .snapshot()
        .files
        .into_iter()
        .map(|file| (file.path, file.id))
        .collect();
    let mut index = InvertedIndex::new();
    index.insert(id, &tokenize(source.as_str()));
    let mut storage = InMemoryStorage::new();
    let mut filesystem = RealFs::new(root.path());
    let exclusions = WorkspacePathExclusion::new(root.path().to_path_buf())
        .expect("real workspace exclusion adapter");

    let error = FileMutationService::new(
        &mut filesystem,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    )
    .plan_path_rename(
        PathRenameRequest {
            source: source.clone(),
            destination: RelativePath::new("ignored.rs"),
            dry_run: false,
            semantic_policy: PathRenameSemanticPolicy::None,
            expected_versions: Default::default(),
        },
        &exclusions,
    )
    .expect_err("a real .towerignore match must reject a native path rename");

    assert!(matches!(
        error,
        DomainPathRenameError::DestinationExcluded { .. }
    ));
    let after_workspace: Vec<_> = workspace
        .snapshot()
        .files
        .into_iter()
        .map(|file| (file.path, file.id))
        .collect();
    assert_eq!(
        after_workspace, before_workspace,
        "workspace must not change"
    );
    assert!(
        filesystem.exists(&source),
        "source file must remain on disk"
    );
    assert!(
        !filesystem.exists(&RelativePath::new("ignored.rs")),
        "destination must not be created"
    );
    assert!(
        storage.get(id).is_err(),
        "rejected planning must not persist a record"
    );
}

#[test]
fn rename_path_applies_native_file_move_and_serializes_the_exact_result() {
    let state = state_with_paths(&[("src/old.rs", "fn old() {}")]);
    let result = call_native_rename(
        Arc::clone(&state),
        json!({"source": "src/old.rs", "destination": "src/new.rs", "semantic_policy": "none"}),
    )
    .unwrap();

    assert_eq!(result, rename_result_fixture("applied"));
    let guard = state.read().unwrap();
    let workspace = guard.workspace.read().unwrap();
    assert!(
        workspace
            .get_by_path(&RelativePath::new("src/old.rs"))
            .is_none()
    );
    assert!(
        workspace
            .get_by_path(&RelativePath::new("src/new.rs"))
            .is_some()
    );
}

#[test]
fn rename_path_applies_native_directory_move_with_one_root_filesystem_mutation() {
    let state = state_with_paths(&[("src/a.rs", "a"), ("src/nested/b.rs", "b")]);
    let result = call_native_rename(
        Arc::clone(&state),
        json!({"source": "src", "destination": "lib", "semantic_policy": "none"}),
    )
    .unwrap();

    assert_eq!(result["status"], "applied");
    assert_eq!(
        result["plan"]["moves"],
        json!([
            {"source": "src", "destination": "lib", "kind": "directory"},
            {"source": "src/a.rs", "destination": "lib/a.rs", "kind": "file"},
            {"source": "src/nested/b.rs", "destination": "lib/nested/b.rs", "kind": "file"}
        ])
    );
    let guard = state.read().unwrap();
    let workspace = guard.workspace.read().unwrap();
    assert!(
        workspace
            .get_by_path(&RelativePath::new("lib/a.rs"))
            .is_some()
    );
    assert!(
        workspace
            .get_by_path(&RelativePath::new("lib/nested/b.rs"))
            .is_some()
    );
}

#[test]
fn rename_path_rejects_missing_and_workspace_external_sources_without_mutation() {
    for (source, expected_code) in [
        ("missing.rs", "source_not_found"),
        ("../PRIVATE_EXTERNAL_SENTINEL.rs", "workspace_escape"),
    ] {
        let state = state_with_paths(&[("src/kept.rs", "kept")]);
        let before = state
            .read()
            .unwrap()
            .workspace
            .read()
            .unwrap()
            .snapshot()
            .files;
        let result = call_native_rename(
            Arc::clone(&state),
            json!({"source": source, "destination": "src/moved.rs", "semantic_policy": "none"}),
        )
        .expect("domain rejection must be returned as a structured result");

        assert_eq!(result["status"], "rejected");
        assert_eq!(result["errors"][0]["code"], expected_code);
        assert!(
            result["errors"][0]["message"].is_string(),
            "{expected_code} must include a stable public error message"
        );
        assert_eq!(
            state
                .read()
                .unwrap()
                .workspace
                .read()
                .unwrap()
                .snapshot()
                .files,
            before
        );
        assert!(!result.to_string().contains("PRIVATE_EXTERNAL_SENTINEL"));
    }
}

#[test]
fn rename_path_required_semantics_without_lsp_returns_structured_unsupported() {
    let result = call_native_rename(
        state_with_paths(&[("src/old.rs", "fn old() {}")]),
        json!({"source": "src/old.rs", "destination": "src/new.rs", "semantic_policy": "required"}),
    )
    .expect("unsupported semantic planning is a successful structured result");

    let mut expected = rename_result_fixture("unsupported");
    expected["plan"]["semantic_policy"] = json!("required");
    assert_eq!(result, expected);
}

#[test]
fn rename_path_dry_run_returns_exact_preview_and_does_not_mutate() {
    let state = state_with_paths(&[("src/old.rs", "fn old() {}")]);
    let result = call_native_rename(
        Arc::clone(&state),
        json!({"source": "src/old.rs", "destination": "src/new.rs", "dry_run": true, "semantic_policy": "none"}),
    )
    .unwrap();

    assert_eq!(result, rename_result_fixture("preview"));
    let guard = state.read().unwrap();
    let workspace = guard.workspace.read().unwrap();
    assert!(
        workspace
            .get_by_path(&RelativePath::new("src/old.rs"))
            .is_some()
    );
    assert!(
        workspace
            .get_by_path(&RelativePath::new("src/new.rs"))
            .is_none()
    );
}

#[test]
fn rename_path_persistence_failure_returns_sanitized_structured_partial() {
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut fs = InMemoryFs::new();
    let source = RelativePath::new("src/old.rs");
    let id = workspace
        .insert(source.clone(), FileMetadata::default())
        .unwrap();
    index.insert(id, &tokenize(source.as_str()));
    fs.write(source, b"fn old() {}".to_vec()).unwrap();
    let state = Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(RenamePersistenceFailure::default()),
        Box::new(fs),
    )));

    let result = call_native_rename(
        state,
        json!({"source": "src/old.rs", "destination": "src/new.rs", "semantic_policy": "none"}),
    )
    .expect("post-move persistence failure must remain a successful structured payload");

    assert_eq!(result, rename_result_fixture("partial"));
    assert!(!result.to_string().contains("PRIVATE_PERSISTENCE_SENTINEL"));
}
