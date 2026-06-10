//! In-process integration tests for the MCP transport (spec 10a).
//!
//! All tests drive `serve()` with in-memory `Cursor`/`Vec<u8>` buffers and a
//! stub `ToolRegistry`. No real stdin/stdout, no spawned processes.
//!
//! TDD sequence (spec §TDD sequence):
//!   1. RED/GREEN: initialize handshake (AC1)
//!   2. RED/GREEN: tools/list from empty registry (AC2)
//!   3. RED/GREEN: tools/call dispatch to stub (AC3)
//!   4. RED/GREEN: unknown tool → JSON-RPC error (AC4)
//!   5. RED/GREEN: malformed frame → parse error, loop continues (AC5)

use std::io::Cursor;

use serde_json::{json, Value};

use super::{
    registry::ToolRegistry,
    transport::serve,
    types::{ToolDesc, ToolError},
};

// ── Stub registry helpers ─────────────────────────────────────────────────────

/// A registry with no tools. Used by AC2 and resilience tests.
struct EmptyRegistry;

impl ToolRegistry for EmptyRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        vec![]
    }

    fn call(&mut self, name: &str, _args: Value) -> Result<Value, ToolError> {
        Err(ToolError::NotFound(name.to_owned()))
    }
}

/// A registry with a single stub tool named `"echo"` that returns its args.
struct EchoRegistry;

impl ToolRegistry for EchoRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        vec![ToolDesc {
            name: "echo".to_owned(),
            description: "Echoes its input back as output.".to_owned(),
            input_schema: json!({ "type": "object" }),
        }]
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
        match name {
            "echo" => Ok(args),
            other => Err(ToolError::NotFound(other.to_owned())),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Feed `input` through `serve` and collect all response lines.
fn run_serve(input: &str, registry: &mut dyn ToolRegistry) -> Vec<Value> {
    let reader = Cursor::new(input.as_bytes().to_vec());
    let mut output: Vec<u8> = Vec::new();
    serve(reader, &mut output, registry).expect("serve returned an error");
    let text = String::from_utf8(output).expect("output is not valid UTF-8");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("response is not valid JSON"))
        .collect()
}

/// Build a minimal `initialize` request string.
fn initialize_request(id: i64) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.1" }
        },
        "id": id
    })
    .to_string()
        + "\n"
}

// ── AC1: initialize handshake ─────────────────────────────────────────────────

#[test]
fn initialize_returns_capabilities() {
    let input = initialize_request(1);
    let responses = run_serve(&input, &mut EmptyRegistry);

    assert_eq!(responses.len(), 1, "expected exactly one response");
    let resp = &responses[0];

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    // Must have a `result` (not `error`) — handshake succeeded.
    assert!(resp.get("result").is_some(), "expected result, got: {resp}");
    // Must advertise a `capabilities` object.
    assert!(
        resp["result"]["capabilities"].is_object(),
        "capabilities must be an object: {resp}"
    );
    // Must advertise the `tools` capability.
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "tools capability must be present: {resp}"
    );
    // Must include server info.
    assert!(
        resp["result"]["serverInfo"]["name"].is_string(),
        "serverInfo.name must be a string: {resp}"
    );
}

#[test]
fn initialize_echoes_request_id() {
    let input = initialize_request(42);
    let responses = run_serve(&input, &mut EmptyRegistry);
    assert_eq!(responses[0]["id"], 42);
}

// ── AC2: tools/list from empty registry ──────────────────────────────────────

#[test]
fn tools_list_empty_registry_returns_empty_list() {
    let input = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": 2
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EmptyRegistry);

    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    assert!(resp.get("result").is_some(), "expected result: {resp}");
    let tools = &resp["result"]["tools"];
    assert!(tools.is_array(), "tools must be an array: {resp}");
    assert_eq!(
        tools.as_array().unwrap().len(),
        0,
        "empty registry must return empty tool list"
    );
}

// ── AC3: tools/call dispatch to stub ─────────────────────────────────────────

#[test]
fn tools_call_dispatches_to_registered_tool() {
    let input = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "echo",
            "arguments": { "msg": "hello" }
        },
        "id": 3
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EchoRegistry);

    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    assert!(resp.get("result").is_some(), "expected result: {resp}");
    // Content array must be present.
    assert!(
        resp["result"]["content"].is_array(),
        "result.content must be array: {resp}"
    );
}

#[test]
fn tools_list_with_echo_registry_returns_one_tool() {
    let input = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": 4
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EchoRegistry);

    let tools = responses[0]["result"]["tools"]
        .as_array()
        .expect("tools must be array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");
}

// ── AC4: unknown tool → structured JSON-RPC error ────────────────────────────

#[test]
fn tools_call_unknown_tool_returns_tool_not_found_error() {
    let input = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "nonexistent_tool",
            "arguments": {}
        },
        "id": 5
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EmptyRegistry);

    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    // Must be an error response, not a result.
    assert!(resp.get("error").is_some(), "expected error: {resp}");
    assert!(resp.get("result").is_none(), "must not have result: {resp}");
    // Application-level ToolNotFound code (-32001), NOT -32601 (MethodNotFound).
    // -32601 would mean tools/call itself is unknown, which is incorrect.
    assert_eq!(resp["error"]["code"], -32001);
    assert_eq!(resp["id"], 5, "id must be echoed");
}

#[test]
fn tools_call_missing_name_returns_invalid_params() {
    let input = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "arguments": {} },
        "id": 6
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EmptyRegistry);

    let resp = &responses[0];
    assert!(resp.get("error").is_some(), "expected error: {resp}");
    assert_eq!(resp["error"]["code"], -32602);
}

// ── AC5: malformed frame → parse error, loop continues ───────────────────────

#[test]
fn malformed_frame_returns_parse_error_without_killing_loop() {
    // First line: garbage JSON. Second line: valid `tools/list`.
    let input = "not-valid-json\n".to_owned()
        + &json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "params": {},
            "id": 7
        })
        .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EmptyRegistry);

    // Two responses: one parse error + one tools/list result.
    assert_eq!(
        responses.len(),
        2,
        "expected 2 responses, got: {responses:?}"
    );

    // First response: parse error.
    assert_eq!(responses[0]["error"]["code"], -32700);
    // Second response: the tools/list result (loop survived).
    assert!(
        responses[1]["result"]["tools"].is_array(),
        "second response must be tools/list result"
    );
}

#[test]
fn multiple_malformed_frames_each_return_parse_error() {
    let input = "bad1\nbad2\nbad3\n";
    let responses = run_serve(input, &mut EmptyRegistry);
    assert_eq!(responses.len(), 3);
    for r in &responses {
        assert_eq!(r["error"]["code"], -32700);
    }
}

// ── Multi-turn: several requests in sequence ─────────────────────────────────

#[test]
fn multiple_requests_in_sequence_are_all_served() {
    let init = initialize_request(10);
    let list = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": 11
    })
    .to_string()
        + "\n";

    let input = init + &list;
    let responses = run_serve(&input, &mut EmptyRegistry);

    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 10);
    assert!(responses[0]["result"]["capabilities"].is_object());
    assert_eq!(responses[1]["id"], 11);
    assert!(responses[1]["result"]["tools"].is_array());
}

// ── Notifications: no response expected ──────────────────────────────────────

#[test]
fn notification_does_not_produce_a_response() {
    // `notifications/initialized` has no `id` — it is a notification.
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    })
    .to_string()
        + "\n";

    let responses = run_serve(&notification, &mut EmptyRegistry);
    assert_eq!(
        responses.len(),
        0,
        "notifications must not produce a response"
    );
}

// ── JSON-RPC 2.0 §4: ANY frame without id is a notification ──────────────────

#[test]
fn unknown_method_without_id_is_a_notification_not_an_error() {
    // JSON-RPC 2.0 §4: a Request without an `id` member is a Notification.
    // The server MUST NOT reply, regardless of method name.
    let frames = [
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}),
        json!({"jsonrpc":"2.0","method":"notifications/progress","params":{}}),
        json!({"jsonrpc":"2.0","method":"some/future/notification","params":{}}),
    ];
    for frame in &frames {
        let input = frame.to_string() + "\n";
        let responses = run_serve(&input, &mut EmptyRegistry);
        assert_eq!(
            responses.len(),
            0,
            "notification '{method}' must not produce a response; got: {responses:?}",
            method = frame["method"],
            responses = responses
        );
    }
}

// ── AC4: ToolError::NotFound maps to -32001 (not -32601) ─────────────────────

#[test]
fn tools_call_unknown_tool_returns_tool_not_found_code() {
    // -32601 means "the method tools/call does not exist" (wrong).
    // -32001 is the application-level "tool not found" code (correct).
    let input = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {"name": "nonexistent_tool", "arguments": {}},
        "id": 99
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EmptyRegistry);
    let resp = &responses[0];
    assert!(resp.get("error").is_some(), "expected error: {resp}");
    assert_eq!(
        resp["error"]["code"], -32001,
        "ToolError::NotFound must map to -32001, not -32601: {resp}"
    );
    assert_eq!(resp["id"], 99);
}

// ── Finding 3: parse_error carries recoverable id when JSON is valid but struct invalid ──

#[test]
fn parse_error_carries_request_id_when_json_is_parseable() {
    // Valid JSON, valid jsonrpc version, but missing required 'method' field.
    // JSON-RPC 2.0 §5.1: id MUST be Null only when the id itself is undetectable.
    // Here the id is fully readable so the error response should echo id:42.
    let input = json!({
        "jsonrpc": "2.0",
        "id": 42
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EmptyRegistry);
    assert_eq!(responses.len(), 1, "expected one error response");
    let resp = &responses[0];
    assert!(resp.get("error").is_some(), "expected error: {resp}");
    assert_eq!(
        resp["id"], 42,
        "id must be echoed from parseable JSON frame; got: {resp}"
    );
    assert_eq!(
        resp["error"]["code"], -32700,
        "must be parse error -32700: {resp}"
    );
}

// ── Finding 4: jsonrpc version validation ────────────────────────────────────

#[test]
fn wrong_jsonrpc_version_returns_invalid_request() {
    // JSON-RPC 2.0 §4 requires jsonrpc == "2.0". Any other value must produce
    // -32600 InvalidRequest, not a successful dispatch.
    let input = json!({
        "jsonrpc": "1.0",
        "method": "tools/list",
        "params": {},
        "id": 77
    })
    .to_string()
        + "\n";

    let responses = run_serve(&input, &mut EmptyRegistry);
    assert_eq!(responses.len(), 1, "expected one error response");
    let resp = &responses[0];
    assert!(resp.get("error").is_some(), "expected error: {resp}");
    assert_eq!(
        resp["error"]["code"], -32600,
        "wrong jsonrpc version must return -32600 InvalidRequest: {resp}"
    );
    assert_eq!(resp["id"], 77, "id must be echoed: {resp}");
}

// ── UN2/AC5: non-UTF-8 frame must not kill the serve loop ─────────────────────

/// Feed bytes directly to `serve` bypassing the UTF-8 `String` helper so we
/// can inject arbitrary byte sequences.
fn run_serve_raw(input_bytes: &[u8], registry: &mut dyn ToolRegistry) -> Vec<Value> {
    use std::io::Cursor;
    let reader = Cursor::new(input_bytes.to_vec());
    let mut output: Vec<u8> = Vec::new();
    serve(reader, &mut output, registry).expect("serve returned an I/O error");
    let text = String::from_utf8_lossy(&output);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("response is not valid JSON"))
        .collect()
}

#[test]
fn non_utf8_frame_returns_parse_error_and_loop_continues() {
    // A frame with a lone 0xFF byte (invalid UTF-8), followed by a valid
    // tools/list request. The server must survive the first frame and still
    // answer the second.
    let mut input: Vec<u8> = vec![0xFF, 0x00, b'\n'];
    let valid = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": 100
    })
    .to_string();
    input.extend_from_slice(valid.as_bytes());
    input.push(b'\n');

    let responses = run_serve_raw(&input, &mut EmptyRegistry);

    assert_eq!(
        responses.len(),
        2,
        "expected parse-error + tools/list result; got: {responses:?}"
    );
    assert_eq!(
        responses[0]["error"]["code"], -32700,
        "non-UTF-8 frame must return parse error -32700"
    );
    assert!(
        responses[1]["result"]["tools"].is_array(),
        "loop must survive the bad frame: {responses:?}"
    );
}
