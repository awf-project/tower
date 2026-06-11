//! End-to-end integration tests for the 7 native `tower_*` MCP tools (spec 10b).
//!
//! All tests use the 10a in-process transport: `serve()` is driven with
//! in-memory `Cursor`/`Vec<u8>` buffers. No spawned processes, no real stdio.
//!
//! # TDD sequence (spec 10b §TDD sequence)
//!
//! 1. AC1 — `tools/list` returns 7 `tower_*` tools with schemas.
//! 2. AC2 — `tower_find_file` round-trip.
//! 3. AC3 — `tower_create_file` then `tower_find_file` finds it.
//! 4. AC4 — malformed args → `invalid-params`, no state change.
//! 5. AC5 — `tower_delete_file` on missing file → stable-code error.

#![forbid(unsafe_code)]

use std::io::Cursor;
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};

use core_engine::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
use core_engine::adapters::mcp::transport::serve;
use core_engine::adapters::{InMemoryFs, InMemoryStorage};
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::token::tokenize;
use core_engine::domain::virtual_file::FileMetadata;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::domain::RelativePath;
use core_engine::ports::FileSystemPort;

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Drive `serve` with `input` and `registry`; return all parsed response lines.
fn run(input: &str, registry: &mut NativeToolRegistry) -> Vec<Value> {
    let reader = Cursor::new(input.as_bytes().to_vec());
    let mut output: Vec<u8> = Vec::new();
    serve(reader, &mut output, registry).expect("serve must not return an I/O error");
    let text = String::from_utf8(output).expect("output must be valid UTF-8");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each response line must be valid JSON"))
        .collect()
}

/// Build a `tools/call` JSON-RPC request string.
fn tools_call(id: i64, name: &str, arguments: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
        "id": id
    })
    .to_string()
        + "\n"
}

/// Build a `tools/list` request string.
fn tools_list(id: i64) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": id
    })
    .to_string()
        + "\n"
}

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

// ── AC1: tools/list returns 7 tower_* tools with schemas ─────────────────────

#[test]
fn ac1_tools_list_returns_seven_tower_tools_with_schemas() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let responses = run(&tools_list(1), &mut reg);

    assert_eq!(responses.len(), 1, "expected exactly one response");
    let resp = &responses[0];

    assert!(
        resp.get("result").is_some(),
        "expected result, not error: {resp}"
    );
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("result.tools must be an array");

    // Must have exactly 7 tools.
    assert_eq!(
        tools.len(),
        7,
        "expected 7 tower_* tools; got {}",
        tools.len()
    );

    // All 7 names must be present.
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    for expected_name in &[
        "tower_find_file",
        "tower_search_text",
        "tower_read_file",
        "tower_create_file",
        "tower_create_directory",
        "tower_delete_file",
        "tower_global_replace",
    ] {
        assert!(
            names.contains(expected_name),
            "missing tool '{expected_name}' in tools/list; got {names:?}"
        );
    }

    // Every tool must carry an inputSchema object.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unknown>");
        assert!(
            tool["inputSchema"].is_object(),
            "tool '{name}' must have an inputSchema object"
        );
    }
}

// ── AC2: tower_find_file round-trip ───────────────────────────────────────────

#[test]
fn ac2_find_file_returns_matching_paths_over_transport() {
    let state = state_with_client_file();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let input = tools_call(2, "tower_find_file", json!({ "query": "client" }));
    let responses = run(&input, &mut reg);

    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    assert_eq!(resp["id"], 2);
    assert!(resp.get("result").is_some(), "expected result: {resp}");

    // The content array from the transport wraps the JSON value as text.
    let content = resp["result"]["content"]
        .as_array()
        .expect("result.content must be array");
    assert!(!content.is_empty(), "content must be non-empty");

    // Parse the inner text payload to check paths.
    let text = content[0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    let payload: Value = serde_json::from_str(text).expect("text payload must be JSON");
    let paths = payload["paths"]
        .as_array()
        .expect("payload must have 'paths'");
    let path_strings: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();

    assert!(
        path_strings.contains(&"src/client.rs"),
        "tower_find_file must return src/client.rs; got {path_strings:?}"
    );
}

// ── AC3: tower_create_file then tower_find_file finds it ─────────────────────

#[test]
fn ac3_create_file_then_find_file_locates_new_file_over_transport() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    // Step 1: create.
    let create_input = tools_call(
        3,
        "tower_create_file",
        json!({ "path": "src/widget.rs", "content": "pub struct Widget;" }),
    );
    let create_responses = run(&create_input, &mut reg);
    assert_eq!(create_responses.len(), 1);
    assert!(
        create_responses[0].get("result").is_some(),
        "create_file must succeed: {:?}",
        create_responses[0]
    );

    // Step 2: find.
    let find_input = tools_call(4, "tower_find_file", json!({ "query": "widget" }));
    let find_responses = run(&find_input, &mut reg);
    assert_eq!(find_responses.len(), 1);
    let resp = &find_responses[0];
    assert!(
        resp.get("result").is_some(),
        "find_file must succeed: {resp}"
    );

    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be string");
    let payload: Value = serde_json::from_str(text).unwrap();
    let paths = payload["paths"].as_array().unwrap();
    let path_strings: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
    assert!(
        path_strings.contains(&"src/widget.rs"),
        "newly created file must be findable via tower_find_file; got {path_strings:?}"
    );
}

// ── AC4: malformed args → invalid-params, no state change ─────────────────────

#[test]
fn ac4_missing_query_returns_invalid_params_over_transport() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    // Call tower_find_file without the required 'query' field.
    let input = tools_call(5, "tower_find_file", json!({}));
    let responses = run(&input, &mut reg);

    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    // Must be an error response.
    assert!(resp.get("error").is_some(), "expected error: {resp}");
    assert!(resp.get("result").is_none(), "must not have result: {resp}");
    // JSON-RPC InvalidParams code.
    assert_eq!(
        resp["error"]["code"], -32602,
        "missing 'query' must return -32602 (InvalidParams): {resp}"
    );
    assert_eq!(resp["id"], 5, "id must be echoed");
}

#[test]
fn ac4_missing_content_for_create_file_returns_invalid_params() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let input = tools_call(6, "tower_create_file", json!({ "path": "ghost.rs" }));
    let responses = run(&input, &mut reg);
    let resp = &responses[0];
    assert!(resp.get("error").is_some(), "expected error: {resp}");
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn ac4_invalid_args_cause_no_state_change() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    // Try to create with missing 'content' — must fail without touching state.
    let bad_create = tools_call(7, "tower_create_file", json!({ "path": "ghost.rs" }));
    run(&bad_create, &mut reg); // response is an error; we don't assert here

    // Now find_file must return nothing — ghost.rs was not created.
    let find_input = tools_call(8, "tower_find_file", json!({ "query": "ghost" }));
    let find_responses = run(&find_input, &mut reg);
    let text = find_responses[0]["result"]["content"][0]["text"]
        .as_str()
        .expect("text payload");
    let payload: Value = serde_json::from_str(text).unwrap();
    let paths = payload["paths"].as_array().unwrap();
    assert!(
        paths.is_empty(),
        "failed create_file must leave workspace unchanged; ghost.rs must not be findable"
    );
}

// ── AC5: tower_delete_file on missing file → stable-code error ───────────────

#[test]
fn ac5_delete_missing_file_returns_stable_error_code_over_transport() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let input = tools_call(
        9,
        "tower_delete_file",
        json!({ "path": "does_not_exist.rs" }),
    );
    let responses = run(&input, &mut reg);

    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    // Must be an error.
    assert!(
        resp.get("error").is_some(),
        "expected error response: {resp}"
    );
    assert!(resp.get("result").is_none(), "must not have result: {resp}");
    assert_eq!(resp["id"], 9);

    // Domain NotFound maps to ToolError::ResourceNotFound which the transport
    // maps to -32002 (server-defined stable code, JSON-RPC 2.0 §5.1 range
    // -32000..=-32099). Clients branch on -32002 to show "not found" without
    // parsing the error string. This is the stable code contract (AC5).
    assert_eq!(
        resp["error"]["code"], -32002,
        "delete-missing must return -32002 (ResourceNotFound): {resp}"
    );
    // The error message must carry a recognisable not-found signal.
    let message = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.to_lowercase().contains("not found"),
        "error message must mention 'not found'; got: {message}"
    );
}

// ── Multi-tool sequence: create → search → delete ─────────────────────────────

#[test]
fn full_sequence_create_search_delete_over_transport() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    // 1. Create a file.
    let create = tools_call(
        10,
        "tower_create_file",
        json!({ "path": "src/engine.rs", "content": "pub fn run() {}" }),
    );
    let r1 = run(&create, &mut reg);
    assert!(
        r1[0].get("result").is_some(),
        "create must succeed: {:?}",
        r1[0]
    );

    // 2. Search for content inside the file.
    let search = tools_call(11, "tower_search_text", json!({ "pattern": "run" }));
    let r2 = run(&search, &mut reg);
    assert!(
        r2[0].get("result").is_some(),
        "search must succeed: {:?}",
        r2[0]
    );
    let text2 = r2[0]["result"]["content"][0]["text"].as_str().unwrap();
    let payload2: Value = serde_json::from_str(text2).unwrap();
    let matches = payload2["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "search_text must find 'run'");

    // 3. Delete the file.
    let delete = tools_call(12, "tower_delete_file", json!({ "path": "src/engine.rs" }));
    let r3 = run(&delete, &mut reg);
    assert!(
        r3[0].get("result").is_some(),
        "delete must succeed: {:?}",
        r3[0]
    );

    // 4. Find should now return nothing.
    let find = tools_call(13, "tower_find_file", json!({ "query": "engine" }));
    let r4 = run(&find, &mut reg);
    let text4 = r4[0]["result"]["content"][0]["text"].as_str().unwrap();
    let payload4: Value = serde_json::from_str(text4).unwrap();
    assert!(
        payload4["paths"].as_array().unwrap().is_empty(),
        "deleted file must not be findable"
    );
}

// ── tower_global_replace over transport ───────────────────────────────────────

#[test]
fn global_replace_returns_files_changed_count() {
    let state = state_with_client_file();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let input = tools_call(
        20,
        "tower_global_replace",
        json!({ "target": "client", "replacement": "server" }),
    );
    let responses = run(&input, &mut reg);
    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    assert!(
        resp.get("result").is_some(),
        "global_replace must succeed: {resp}"
    );

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let payload: Value = serde_json::from_str(text).unwrap();
    assert!(
        payload["files_changed"].as_u64().unwrap_or(0) >= 1,
        "global_replace must report files_changed >= 1"
    );
}

// ── tower_read_file over transport ────────────────────────────────────────────

#[test]
fn read_file_returns_content_over_transport() {
    let state = state_with_client_file();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let input = tools_call(30, "tower_read_file", json!({ "path": "src/client.rs" }));
    let responses = run(&input, &mut reg);
    let resp = &responses[0];
    assert!(
        resp.get("result").is_some(),
        "read_file must succeed: {resp}"
    );

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let payload: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        payload["content"].as_str().unwrap(),
        "fn client() {}",
        "read_file must return exact content"
    );
}

#[test]
fn read_file_on_missing_path_returns_error() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let input = tools_call(31, "tower_read_file", json!({ "path": "missing.rs" }));
    let responses = run(&input, &mut reg);
    let resp = &responses[0];
    assert!(
        resp.get("error").is_some(),
        "read_file on missing path must error: {resp}"
    );
    // PortError::NotFound must surface as the stable -32002 code (same contract
    // as tower_delete_file; both go through domain_err_to_tool_error → ResourceNotFound).
    assert_eq!(
        resp["error"]["code"], -32002,
        "read_file on missing path must return -32002: {resp}"
    );
}

// ── tower_create_directory over transport ─────────────────────────────────────

#[test]
fn create_directory_succeeds_over_transport() {
    let state = empty_state();
    let mut reg = NativeToolRegistry::new(Arc::clone(&state));

    let input = tools_call(40, "tower_create_directory", json!({ "path": "a/b/c" }));
    let responses = run(&input, &mut reg);
    let resp = &responses[0];
    assert!(
        resp.get("result").is_some(),
        "create_directory must succeed: {resp}"
    );

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let payload: Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["created"], true);
}
