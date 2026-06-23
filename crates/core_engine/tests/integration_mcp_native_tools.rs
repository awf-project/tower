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

use rmcp::model::CallToolRequestParams;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

use core_engine::adapters::mcp::diagnostics::NoOpDiagnosticsReader;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::lsp_tools::SubscriptionRegistry;
use core_engine::adapters::mcp::native_tools::{
    EXPECTED_NATIVE_TOOL_NAMES, EngineState, NativeToolRegistry,
};
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::mcp::rmcp_server::TowerMcpHandler;
use core_engine::adapters::{InMemoryFs, InMemoryStorage};
use core_engine::domain::RelativePath;
use core_engine::domain::extension_host::ExtensionRegistry;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::token::tokenize;
use core_engine::domain::virtual_file::FileMetadata;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::FileSystemPort;

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
