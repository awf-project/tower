// Feature: F004
// Feature: F005

#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::TowerConfig;
use core_engine::adapters::daemon::engine::build_engine;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::native_tools::{EXPECTED_NATIVE_TOOL_NAMES, NativeToolRegistry};
use core_engine::adapters::mcp::registry::ToolRegistry;
use extension_protocol::PROTOCOL_VERSION;
use serde_json::{Value, json};

#[path = "../../../extensions/debug/src/protocol.rs"]
mod debug_protocol;

use debug_protocol::{
    DebugInitError, DebugInitializeConfig, DebugToolError, DebugToolErrorCode,
    debug_not_initialized_result, debug_tool_declarations, debug_tool_unavailable_result,
};

struct RawDebugChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RawDebugChild {
    fn spawn() -> Self {
        let mut child = Command::new(debug_extension_bin())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn debug extension");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn write_frame(&mut self, frame: serde_json::Value) {
        let line = serde_json::to_string(&frame).expect("serialize frame");
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    fn read_frame(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read frame");
        assert!(
            !line.is_empty(),
            "child stdout closed before a frame arrived"
        );
        serde_json::from_str(line.trim()).expect("valid JSON frame")
    }
}

impl Drop for RawDebugChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn debug_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("debug_extension")
        .to_str()
        .expect("debug extension bin path")
        .to_owned()
}

fn fixture_debug_adapter_bin() -> std::path::PathBuf {
    workspace_root()
        .join("target")
        .join("debug")
        .join("fixture_debug_adapter")
}

fn debug_config() -> TowerConfig {
    toml::from_str(
        r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
args = ["--quiet"]
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
    )
    .expect("debug config must parse")
}

fn native_fixture_tower_config(args: &[&str], extension_request_timeout_secs: u64) -> TowerConfig {
    let args = args
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("fixture arg must serialize"))
        .collect::<Vec<_>>()
        .join(", ");
    let command = serde_json::to_string(
        fixture_debug_adapter_bin()
            .to_str()
            .expect("fixture path must be utf8"),
    )
    .expect("fixture path must serialize");
    toml::from_str(&format!(
        r#"
[extensions]
request_timeout_secs = {extension_request_timeout_secs}

[debug.rust]
extensions = ["rs"]
command = {command}
args = [{args}]
adapter_type = "fixture"
default_timeout_secs = 5
idle_ttl_secs = 300
"#
    ))
    .expect("native fixture debug config must parse")
}

fn expected_debug_tool_names() -> Vec<&'static str> {
    vec![
        "tower_debug_launch",
        "tower_debug_set_breakpoints",
        "tower_debug_continue",
        "tower_debug_step",
        "tower_debug_pause",
        "tower_debug_threads",
        "tower_debug_stack",
        "tower_debug_variables",
        "tower_debug_evaluate",
        "tower_debug_eval_at",
        "tower_debug_terminate",
        "tower_debug_disconnect",
        "tower_debug_sessions",
    ]
}

fn expected_manifest_tool_names() -> Vec<&'static str> {
    vec![
        "launch",
        "set_breakpoints",
        "continue",
        "step",
        "pause",
        "threads",
        "stack",
        "variables",
        "evaluate",
        "eval_at",
        "terminate",
        "disconnect",
        "sessions",
    ]
}

fn valid_debug_init_payload() -> serde_json::Value {
    json!({
        "languages": {
            "rust": {
                "extensions": ["rs"],
                "command": "lldb-dap",
                "args": ["--quiet"],
                "adapter_type": "lldb",
                "launch": {
                    "program": "${workspaceFolder}/target/debug/app"
                },
                "default_timeout_secs": 15,
                "idle_ttl_secs": 300
            }
        }
    })
}

fn native_dap_fixture_init_payload(args: &[&str], timeout_secs: u64) -> serde_json::Value {
    json!({
        "languages": {
            "rust": {
                "extensions": ["rs"],
                "command": fixture_debug_adapter_bin().to_str().expect("fixture path must be utf8"),
                "args": args,
                "adapter_type": "fixture",
                "launch": {
                    "request": "launch"
                },
                "default_timeout_secs": timeout_secs,
                "idle_ttl_secs": 300
            }
        }
    })
}

fn dap_fixture_init_payload(script: &std::path::Path) -> serde_json::Value {
    json!({
        "languages": {
            "rust": {
                "extensions": ["rs"],
                "command": "python3",
                "args": [script.to_str().expect("fixture path must be utf8")],
                "adapter_type": "fixture",
                "launch": {
                    "request": "launch"
                },
                "default_timeout_secs": 5,
                "idle_ttl_secs": 300
            }
        }
    })
}

fn write_dap_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("dap_fixture.py");
    fs::write(
        &script,
        r#"
import json
import sys

seq = 1

def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        name, value = line.decode("ascii").split(":", 1)
        headers[name.lower()] = value.strip()
    length = int(headers["content-length"])
    return json.loads(sys.stdin.buffer.read(length))

def write_message(message):
    body = json.dumps(message, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    request = read_message()
    if request is None:
        break
    command = request.get("command")
    body = {}
    if command == "setBreakpoints":
        body = {"breakpoints": []}
    elif command == "stackTrace":
        body = {"stackFrames": [{"id": 1, "name": "main", "source": {"path": request.get("arguments", {}).get("source", {}).get("path", "main.rs")}, "line": 1, "column": 1}]}
    elif command == "threads":
        body = {"threads": [{"id": 1, "name": "main"}]}
    write_message({
        "seq": seq,
        "type": "response",
        "request_seq": request["seq"],
        "command": command,
        "success": True,
        "body": body,
    })
    seq += 1
"#,
    )
    .expect("write dap fixture");
    script
}

struct RawDapFixture {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RawDapFixture {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(fixture_debug_adapter_bin())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn fixture debug adapter");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn request(&mut self, seq: u64, command: &str, arguments: serde_json::Value) {
        write_dap_frame(
            &mut self.stdin,
            &json!({
                "seq": seq,
                "type": "request",
                "command": command,
                "arguments": arguments
            }),
        );
    }

    fn response_for(&mut self, seq: u64, command: &str) -> serde_json::Value {
        loop {
            let frame = read_dap_frame(&mut self.stdout);
            if frame["type"] == "response"
                && frame["request_seq"] == seq
                && frame["command"] == command
            {
                return frame;
            }
        }
    }
}

impl Drop for RawDapFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_dap_frame(writer: &mut impl Write, message: &serde_json::Value) {
    let body = serde_json::to_vec(message).expect("serialize DAP frame");
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).expect("write DAP header");
    writer.write_all(&body).expect("write DAP body");
    writer.flush().expect("flush DAP frame");
}

fn read_dap_frame(reader: &mut impl BufRead) -> serde_json::Value {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).expect("read DAP header");
        assert!(bytes > 0, "fixture closed stdout while reading DAP headers");
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>().expect("Content-Length usize"));
        }
    }
    let len = content_length.expect("DAP frame includes Content-Length");
    let mut body = vec![0; len];
    reader.read_exact(&mut body).expect("read DAP body");
    serde_json::from_slice(&body).expect("DAP JSON body")
}

fn merged_tool_names(config: TowerConfig) -> Vec<String> {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, config).expect("engine builds");

    let merged = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);
    merged
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>()
}

fn initialize_debug_child(child: &mut RawDebugChild, id: u64, extension_config: serde_json::Value) {
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "debug-e2e/0.1.0",
            "extension_config": extension_config
        }
    }));

    let initialized = child.read_frame();
    assert_eq!(initialized["id"], id);
    assert!(
        initialized.get("result").is_some(),
        "initialize must succeed; got {initialized}"
    );
}

fn invoke_debug_tool(
    child: &mut RawDebugChild,
    id: u64,
    name: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "invokeTool",
        "params": {
            "name": name,
            "params": params
        }
    }));
    let host_call = child.read_frame();
    assert_eq!(
        host_call["method"], "log",
        "{name} dispatch must perform the sidecar HostCall before returning; got {host_call}"
    );
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": host_call["id"].clone(),
        "result": true
    }));
    let response = child.read_frame();
    assert_eq!(
        response["id"], id,
        "{name} response id mismatch: {response}"
    );
    response["result"]["data"].clone()
}

fn launch_fixture_session(child: &mut RawDebugChild, id: u64) -> serde_json::Value {
    invoke_debug_tool(
        child,
        id,
        "launch",
        json!({
            "language": "rust",
            "program": "fixture-program",
            "cwd": null,
            "args": [],
            "env": {},
            "launch_overrides": {}
        }),
    )
}

fn fixture_process_count(token: &str) -> usize {
    let output = Command::new("ps")
        .args(["-eo", "pid=,args="])
        .output()
        .expect("ps command");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("fixture_debug_adapter") && line.contains(token))
        .count()
}

fn assert_fixture_processes_gone(token: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if fixture_process_count(token) == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        fixture_process_count(token),
        0,
        "fixture adapter process with token {token} should be cleaned up"
    );
}

fn assert_object_has_no_key_recursively(value: &Value, forbidden_key: &str) {
    match value {
        Value::Object(map) => {
            assert!(
                !map.contains_key(forbidden_key),
                "response must not contain {forbidden_key}; got {value}"
            );
            for child in map.values() {
                assert_object_has_no_key_recursively(child, forbidden_key);
            }
        }
        Value::Array(items) => {
            for child in items {
                assert_object_has_no_key_recursively(child, forbidden_key);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn eval_at_request(timeout_ms: u64) -> Value {
    json!({
        "lang": "rust",
        "program": "fixture-program",
        "breakpoint": { "path": "src/main.rs", "line": 12 },
        "expressions": ["answer"],
        "capture": { "stack": true, "locals": true, "args": true },
        "on_hit": "first",
        "timeout_ms": timeout_ms
    })
}

fn assert_quarantine_cleans_fixture_process(token: &str) {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(
        &opts,
        native_fixture_tower_config(&["--token", token, "--continue-delay-ms=5000"], 1),
    )
    .expect("engine builds with fixture debug config");
    let mut merged = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    let launch = merged
        .call(
            "tower_debug_launch",
            json!({
                "language": "rust",
                "program": "fixture-program",
                "cwd": null,
                "args": [],
                "env": {},
                "launch_overrides": {}
            }),
        )
        .expect("fixture launch succeeds before quarantine");
    let session_id = launch["session_id"]
        .as_str()
        .expect("launch returns session_id")
        .to_owned();

    let first_fault = merged
        .call(
            "tower_debug_continue",
            json!({ "session_id": session_id, "thread_id": 1, "timeout_secs": 5 }),
        )
        .expect_err("slow continue must fault through the sidecar request timeout");
    assert!(
        format!("{first_fault:?}").contains("Timeout"),
        "first quarantine-path fault must be a timeout; got {first_fault:?}"
    );

    let mut saw_quarantined = false;
    for _ in 0..4 {
        let fault = merged
            .call("tower_debug_sessions", json!({}))
            .expect_err("faulted debug extension must stop serving calls");
        if format!("{fault:?}").contains("Quarantined") {
            saw_quarantined = true;
            break;
        }
    }
    assert!(
        saw_quarantined,
        "debug extension must reach the public quarantine state after repeated faults"
    );
    assert_fixture_processes_gone(token);
}

#[test]
fn workspace_cargo_toml_includes_extensions_fixtures_debug_adapter() {
    let manifest = fs::read_to_string(workspace_root().join("Cargo.toml")).expect("read manifest");

    assert!(
        manifest.contains("\"extensions/fixtures/debug_adapter\""),
        "Workspace Cargo.toml includes extensions/fixtures/debug_adapter"
    );
}

#[test]
fn extensions_fixtures_debug_adapter_cargo_toml_defines_a_fixture_binary_usable_by_core_e2e_tests()
{
    let manifest =
        fs::read_to_string(workspace_root().join("extensions/fixtures/debug_adapter/Cargo.toml"))
            .expect("read fixture manifest");

    assert!(
        manifest.contains("name = \"fixture_debug_adapter\"")
            && manifest.contains("[[bin]]")
            && manifest.contains("path = \"src/main.rs\""),
        "extensions/fixtures/debug_adapter/Cargo.toml defines a fixture binary usable by core e2e tests"
    );
}

#[test]
fn fixture_handles_initialize_launch_setbreakpoints_configurationdone_continue_next_step_pause_threads_stacktrace_scopes_variables_evaluate_terminate_and_disconnect_requests_needed_by_tests()
 {
    let mut fixture = RawDapFixture::spawn(&[]);
    let commands = [
        (
            "initialize",
            json!({
                "adapterID": "fixture",
                "pathFormat": "path",
                "linesStartAt1": true,
                "columnsStartAt1": true
            }),
        ),
        ("launch", json!({ "program": "fixture-program" })),
        (
            "setBreakpoints",
            json!({
                "source": { "path": "src/main.rs" },
                "breakpoints": [{ "line": 12 }]
            }),
        ),
        ("configurationDone", json!({})),
        ("continue", json!({})),
        ("next", json!({ "threadId": 1 })),
        ("step", json!({ "threadId": 1 })),
        ("pause", json!({ "threadId": 1 })),
        ("threads", json!({})),
        ("stackTrace", json!({ "threadId": 1 })),
        ("scopes", json!({ "frameId": 1 })),
        ("variables", json!({ "variablesReference": 100 })),
        ("evaluate", json!({ "frameId": 1, "expression": "answer" })),
        ("terminate", json!({})),
        ("disconnect", json!({})),
    ];

    for (index, (command, arguments)) in commands.into_iter().enumerate() {
        let seq = u64::try_from(index + 1).expect("seq fits u64");
        fixture.request(seq, command, arguments);
        let response = fixture.response_for(seq, command);
        assert_eq!(
            response["success"], true,
            "fixture must return a successful DAP response for {command}; got {response}"
        );
    }
}

#[test]
fn fixture_can_emit_continue_response_before_stop_event_to_match_real_dap_ordering() {
    let mut fixture = RawDapFixture::spawn(&["--continue-event-delay-ms=50"]);

    fixture.request(1, "continue", json!({}));
    let first = read_dap_frame(&mut fixture.stdout);
    assert_eq!(
        first["type"], "response",
        "response-before-event fixture mode must send the continue response first; got {first}"
    );
    assert_eq!(first["request_seq"], 1);
    assert_eq!(first["command"], "continue");

    let second = read_dap_frame(&mut fixture.stdout);
    assert_eq!(
        second["type"], "event",
        "response-before-event fixture mode must send the stop event after the response; got {second}"
    );
    assert_eq!(second["event"], "stopped");
}

#[test]
fn extensions_debug_src_main_rs_starts_a_json_rpc_stdio_loop_using_the_shared_harness() {
    let mut child = RawDebugChild::spawn();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "debug-e2e/0.1.0"
        }
    }));

    let initialized = child.read_frame();
    assert_eq!(initialized["id"], 1);
    assert!(
        initialized.get("result").is_some(),
        "initialize must return a JSON-RPC result; got: {initialized}"
    );
    assert_eq!(
        initialized["result"]["type"], "Initialized",
        "debug sidecar must speak the extension protocol over stdio; got: {initialized}"
    );
    assert_eq!(
        initialized["result"]["data"]["tools"]
            .as_array()
            .expect("tools array")
            .len(),
        0,
        "absent debug config must initialize safely with no declared tools"
    );
}

#[test]
fn extensions_debug_src_protocol_rs_defines_public_debug_protocol_dtos_and_errors() {
    let payload = valid_debug_init_payload();
    let parsed = DebugInitializeConfig::from_init_payload(Some(payload))
        .expect("valid debug config must parse")
        .expect("valid debug config must be present");

    assert!(!parsed.is_empty());
    assert_eq!(parsed.languages["rust"].command, "lldb-dap");
    assert_eq!(parsed.languages["rust"].adapter_type, "lldb");
    assert_eq!(parsed.languages["rust"].extensions, vec!["rs"]);
    assert_eq!(parsed.languages["rust"].args, vec!["--quiet"]);
    assert_eq!(parsed.languages["rust"].default_timeout_secs, 15);
    assert_eq!(parsed.languages["rust"].idle_ttl_secs, 300);
    assert_eq!(
        parsed.languages["rust"].launch["program"],
        "${workspaceFolder}/target/debug/app"
    );

    let error = DebugInitError::InvalidConfig("bad config".to_owned());
    assert_eq!(error.jsonrpc_code(), -32602);

    assert_eq!(
        serde_json::to_value(DebugToolError {
            code: DebugToolErrorCode::InvalidParams,
            message: "InvalidParams".to_owned(),
        })
        .expect("serialize debug tool error"),
        json!({ "code": -32602, "message": "InvalidParams" })
    );
    assert_eq!(
        debug_not_initialized_result(),
        json!({
            "code": "debug-not-initialized",
            "message": "debug extension is not initialized"
        })
    );
    assert_eq!(
        debug_tool_unavailable_result("sessions"),
        json!({
            "code": format!("debug-not-{}", "implemented"),
            "message": "debug tool sessions is unavailable"
        })
    );
}

#[test]
fn debug_initialize_config_from_init_payload_accepts_absent_valid_non_empty_and_rejects_malformed_payloads()
 {
    assert_eq!(
        DebugInitializeConfig::from_init_payload(None).expect("absent payload is accepted"),
        None
    );

    let parsed = DebugInitializeConfig::from_init_payload(Some(valid_debug_init_payload()))
        .expect("valid payload is accepted")
        .expect("valid payload yields config");
    assert!(!parsed.is_empty());

    let missing_languages = DebugInitializeConfig::from_init_payload(Some(json!({})));
    assert!(
        matches!(missing_languages, Err(DebugInitError::InvalidConfig(_))),
        "present payload without languages must be rejected as DebugInitError::InvalidConfig; got {missing_languages:?}"
    );

    let malformed = DebugInitializeConfig::from_init_payload(Some(json!({
        "languages": {
            "rust": {
                "extensions": ["rs"],
                "command": "",
                "args": [],
                "adapter_type": "lldb",
                "launch": {},
                "default_timeout_secs": 15,
                "idle_ttl_secs": 300
            }
        }
    })));
    assert!(
        matches!(malformed, Err(DebugInitError::InvalidConfig(_))),
        "malformed payload must be rejected as DebugInitError::InvalidConfig; got {malformed:?}"
    );
}

#[test]
fn debug_tool_declarations_returns_empty_for_none_or_empty_config_and_complete_manifest_backed_tool_set_for_configured_debug_languages()
 {
    let empty_config = DebugInitializeConfig::from_init_payload(Some(json!({ "languages": {} })))
        .expect("empty map is a valid config")
        .expect("empty map still yields config");

    assert!(debug_tool_declarations(None).is_empty());
    assert!(debug_tool_declarations(Some(&empty_config)).is_empty());

    let configured = DebugInitializeConfig::from_init_payload(Some(valid_debug_init_payload()))
        .expect("valid config")
        .expect("present config");
    let declarations = debug_tool_declarations(Some(&configured));
    let tool_names = declarations
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_names, expected_manifest_tool_names());
    for tool in &declarations {
        let schema: serde_json::Value =
            serde_json::from_str(&tool.schema_json).expect("debug tool schema_json must parse");
        assert_eq!(
            schema["type"], "object",
            "{} schema must be an object",
            tool.name
        );
        if tool.name != "sessions" {
            assert_ne!(
                schema["properties"],
                json!({}),
                "{} schema must describe its real input contract, not the placeholder empty object",
                tool.name
            );
        }
    }
    let launch = declarations
        .iter()
        .find(|tool| tool.name == "launch")
        .expect("launch declaration exists");
    let launch_schema: serde_json::Value =
        serde_json::from_str(&launch.schema_json).expect("launch schema parses");
    assert_eq!(launch_schema["required"], json!(["language", "program"]));
    let step = declarations
        .iter()
        .find(|tool| tool.name == "step")
        .expect("step declaration exists");
    let step_schema: serde_json::Value =
        serde_json::from_str(&step.schema_json).expect("step schema parses");
    assert!(step_schema["properties"].get("granularity").is_none());

    let manifest = fs::read_to_string(workspace_root().join("extensions/debug/extension.toml"))
        .expect("read debug manifest");
    for name in expected_manifest_tool_names() {
        assert!(
            manifest.contains(&format!("name        = \"{name}\""))
                || manifest.contains(&format!("name = \"{name}\"")),
            "debug tool {name} must remain manifest-backed"
        );
    }
}

#[test]
fn debug_init_error_maps_malformed_initialize_config_to_json_rpc_minus_32602_with_stable_message_prefix()
 {
    let mut child = RawDebugChild::spawn();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": {
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "debug-e2e/0.1.0",
            "extension_config": {}
        }
    }));

    let error = child.read_frame();
    assert_eq!(error["id"], 2);
    assert_eq!(error["error"]["code"], -32602);
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("error message")
            .starts_with("debug_invalid_initialize_config"),
        "malformed initialize config must use the stable debug error prefix; got {error}"
    );
}

#[test]
fn debug_process_spawns_and_declares_configured_tools() {
    let mut child = RawDebugChild::spawn();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "initialize",
        "params": {
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "debug-e2e/0.1.0",
            "extension_config": valid_debug_init_payload()
        }
    }));

    let initialized = child.read_frame();
    assert_eq!(initialized["id"], 3);
    let tool_names = initialized["result"]["data"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(tool_names, expected_manifest_tool_names());
}

#[test]
fn debug_sidecar_launch_uses_configured_dap_adapter_and_creates_a_session() {
    let fixture_dir = tempfile::tempdir().expect("create fixture dir");
    let fixture = write_dap_fixture(fixture_dir.path());
    let mut child = RawDebugChild::spawn();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 30,
        "method": "initialize",
        "params": {
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "debug-e2e/0.1.0",
            "extension_config": dap_fixture_init_payload(&fixture)
        }
    }));

    let initialized = child.read_frame();
    assert_eq!(initialized["id"], 30);
    assert!(
        initialized.get("result").is_some(),
        "initialize with fixture debug config must succeed; got {initialized}"
    );

    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 31,
        "method": "invokeTool",
        "params": {
            "name": "launch",
            "params": {
                "language": "rust",
                "program": "fixture-program",
                "cwd": null,
                "args": [],
                "env": {},
                "launch_overrides": {}
            }
        }
    }));
    let host_call = child.read_frame();
    assert_eq!(
        host_call["method"], "log",
        "launch dispatch must still perform the sidecar HostCall; got {host_call}"
    );
    let host_call_id = host_call["id"].clone();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": host_call_id,
        "result": true
    }));

    let launch = child.read_frame();
    assert_eq!(launch["id"], 31);
    let result = &launch["result"]["data"];
    assert_eq!(
        result["state"], "stopped",
        "launch must return a real stopped debug session result; got {launch}"
    );
    assert!(
        result["session_id"]
            .as_str()
            .is_some_and(|session_id| session_id.starts_with("debug-")),
        "launch must include a generated session_id; got {launch}"
    );
    assert_ne!(
        result["code"],
        format!("debug-not-{}", "implemented"),
        "configured launch must not use the unavailable sidecar placeholder; got {launch}"
    );
}

#[test]
fn e2e_launch_flow_sets_a_breakpoint_continues_to_stopped_inspects_stack_variables_evaluate_and_continues_to_terminated()
 {
    let mut child = RawDebugChild::spawn();
    initialize_debug_child(
        &mut child,
        40,
        native_dap_fixture_init_payload(&["--continue-event-delay-ms=50"], 5),
    );

    let launch = launch_fixture_session(&mut child, 41);
    let session_id = launch["session_id"]
        .as_str()
        .expect("launch returns session_id")
        .to_owned();
    assert_eq!(launch["state"], "stopped", "launch should stop at entry");

    let breakpoints = invoke_debug_tool(
        &mut child,
        42,
        "set_breakpoints",
        json!({
            "session_id": session_id,
            "path": "src/main.rs",
            "breakpoints": [{ "line": 12, "condition": null, "hit_condition": null }]
        }),
    );
    assert_eq!(
        breakpoints["breakpoints"][0]["verified"], true,
        "set_breakpoints should return a verified fixture breakpoint; got {breakpoints}"
    );

    let stopped = invoke_debug_tool(
        &mut child,
        43,
        "continue",
        json!({ "session_id": session_id, "thread_id": 1, "timeout_secs": 5 }),
    );
    assert_eq!(
        stopped["state"], "stopped",
        "continue should stop; got {stopped}"
    );
    assert_eq!(stopped["timed_out"], false);

    let stack = invoke_debug_tool(
        &mut child,
        44,
        "stack",
        json!({ "session_id": session_id, "thread_id": 1 }),
    );
    let frame_id = stack["frames"][0]["id"].as_u64().expect("frame id");
    assert_eq!(stack["frames"][0]["name"], "main");

    let variables = invoke_debug_tool(
        &mut child,
        45,
        "variables",
        json!({ "session_id": session_id, "variables_reference": 100 }),
    );
    assert_eq!(variables["variables"][0]["name"], "answer");
    assert_eq!(variables["variables"][0]["value"], "42");

    let evaluated = invoke_debug_tool(
        &mut child,
        46,
        "evaluate",
        json!({ "session_id": session_id, "frame_id": frame_id, "expression": "answer" }),
    );
    assert_eq!(evaluated["result"]["value"], "42");

    let terminated = invoke_debug_tool(
        &mut child,
        47,
        "continue",
        json!({ "session_id": session_id, "thread_id": 1, "timeout_secs": 5 }),
    );
    assert_eq!(
        terminated["state"], "terminated",
        "second continue should run fixture to terminated; got {terminated}"
    );
}

#[test]
fn e2e_timeout_flow_returns_within_configured_timeout_plus_500_ms_and_leaves_the_session_controllable_by_pause_or_terminate()
 {
    let mut child = RawDebugChild::spawn();
    initialize_debug_child(
        &mut child,
        50,
        native_dap_fixture_init_payload(&["--continue-delay-ms=2000"], 1),
    );
    let launch = launch_fixture_session(&mut child, 51);
    let session_id = launch["session_id"]
        .as_str()
        .expect("launch returns session_id")
        .to_owned();

    let started = Instant::now();
    let timed_out = invoke_debug_tool(
        &mut child,
        52,
        "continue",
        json!({ "session_id": session_id, "thread_id": 1, "timeout_secs": 1 }),
    );
    assert!(
        started.elapsed() <= Duration::from_millis(1_500),
        "timeout flow must return within configured timeout plus 500 ms"
    );
    assert_eq!(timed_out["state"], "running");
    assert_eq!(timed_out["timed_out"], true);

    let paused = invoke_debug_tool(
        &mut child,
        53,
        "pause",
        json!({ "session_id": session_id, "thread_id": 1 }),
    );
    assert_eq!(
        paused["state"], "stopped",
        "session should remain controllable by pause after timeout; got {paused}"
    );

    let terminated = invoke_debug_tool(
        &mut child,
        54,
        "terminate",
        json!({ "session_id": session_id }),
    );
    assert_eq!(terminated["ok"], true);
}

#[test]
fn e2e_stale_id_flow_returns_stable_session_not_found_and_running_inspection_returns_stable_not_stopped()
 {
    let mut child = RawDebugChild::spawn();
    initialize_debug_child(
        &mut child,
        60,
        native_dap_fixture_init_payload(&["--continue-delay-ms=2000"], 1),
    );

    let stale = invoke_debug_tool(
        &mut child,
        61,
        "threads",
        json!({ "session_id": "debug-stale-session" }),
    );
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["error"]["code"], "session-not-found");

    let launch = launch_fixture_session(&mut child, 62);
    let session_id = launch["session_id"]
        .as_str()
        .expect("launch returns session_id")
        .to_owned();
    let running = invoke_debug_tool(
        &mut child,
        63,
        "continue",
        json!({ "session_id": session_id, "thread_id": 1, "timeout_secs": 1 }),
    );
    assert_eq!(running["state"], "running");

    let stack = invoke_debug_tool(
        &mut child,
        64,
        "stack",
        json!({ "session_id": session_id, "thread_id": 1 }),
    );
    assert_eq!(stack["ok"], false);
    assert_eq!(stack["error"]["code"], "not-stopped");

    let _ = invoke_debug_tool(
        &mut child,
        65,
        "terminate",
        json!({ "session_id": session_id }),
    );
}

#[test]
fn e2e_cleanup_flow_verifies_no_adapter_debuggee_fixture_child_remains_after_terminate_disconnect_shutdown_or_quarantine()
 {
    for operation in ["terminate", "disconnect", "shutdown", "quarantine"] {
        let token = format!("cleanup-{operation}-{}", std::process::id());
        if operation == "quarantine" {
            assert_quarantine_cleans_fixture_process(&token);
            continue;
        }

        let mut child = RawDebugChild::spawn();
        initialize_debug_child(
            &mut child,
            70,
            native_dap_fixture_init_payload(&["--token", &token], 5),
        );
        let launch = launch_fixture_session(&mut child, 71);
        let session_id = launch["session_id"]
            .as_str()
            .expect("launch returns session_id")
            .to_owned();

        match operation {
            "terminate" => {
                let result = invoke_debug_tool(
                    &mut child,
                    72,
                    "terminate",
                    json!({ "session_id": session_id }),
                );
                assert_eq!(result["ok"], true);
            }
            "disconnect" => {
                let result = invoke_debug_tool(
                    &mut child,
                    72,
                    "disconnect",
                    json!({ "session_id": session_id }),
                );
                assert_eq!(result["ok"], true);
            }
            "shutdown" => {
                child.write_frame(json!({
                    "jsonrpc": "2.0",
                    "id": 72,
                    "method": "shutdown",
                    "params": {}
                }));
                let shutdown = child.read_frame();
                assert_eq!(shutdown["id"], 72);
                assert_eq!(shutdown["result"]["type"], "Ack");
            }
            other => panic!("unsupported cleanup operation: {other}"),
        }
        drop(child);
        assert_fixture_processes_gone(&token);
    }
}

#[test]
fn twenty_iteration_parallel_initialize_launch_terminate_shutdown_stress_test_completes_without_deadlock()
 {
    let handles = (0..20)
        .map(|index| {
            std::thread::spawn(move || {
                let mut child = RawDebugChild::spawn();
                initialize_debug_child(
                    &mut child,
                    80,
                    native_dap_fixture_init_payload(&["--stress-index", &index.to_string()], 5),
                );
                let launch = launch_fixture_session(&mut child, 81);
                let session_id = launch["session_id"]
                    .as_str()
                    .expect("launch returns session_id")
                    .to_owned();
                let result = invoke_debug_tool(
                    &mut child,
                    82,
                    "terminate",
                    json!({ "session_id": session_id }),
                );
                assert_eq!(result["ok"], true);
                child.write_frame(json!({
                    "jsonrpc": "2.0",
                    "id": 83,
                    "method": "shutdown",
                    "params": {}
                }));
                let shutdown = child.read_frame();
                assert_eq!(shutdown["id"], 83);
                assert_eq!(shutdown["result"]["type"], "Ack");
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().expect("stress worker should not panic");
    }
}

#[test]
fn shutdown_after_initialize_returns_ack() {
    let mut child = RawDebugChild::spawn();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "initialize",
        "params": {
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "debug-e2e/0.1.0",
            "extension_config": valid_debug_init_payload()
        }
    }));
    assert!(
        child.read_frame().get("result").is_some(),
        "initialize must succeed before shutdown"
    );

    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "shutdown",
        "params": {}
    }));
    let shutdown = child.read_frame();
    assert_eq!(shutdown["id"], 7);
    assert_eq!(shutdown["result"]["type"], "Ack");
}

#[test]
fn debug_tools_absent_without_config_and_present_with_valid_debug_config() {
    let absent_names = merged_tool_names(TowerConfig::default());
    for name in expected_debug_tool_names() {
        assert!(
            absent_names.iter().all(|tool| tool != name),
            "{name} must be absent when no [debug.*] config exists; got {absent_names:?}"
        );
    }

    let present_names = merged_tool_names(debug_config());
    for name in expected_debug_tool_names() {
        assert!(
            present_names.iter().any(|tool| tool == name),
            "{name} must be present when valid [debug.rust] config exists; got {present_names:?}"
        );
    }
}

#[test]
fn debug_tools_absent_when_extension_disabled_even_with_valid_debug_config() {
    let mut config = debug_config();
    config.extensions.disabled = vec!["debug".to_owned()];

    let names = merged_tool_names(config);
    for name in expected_debug_tool_names() {
        assert!(
            names.iter().all(|tool| tool != name),
            "{name} must be absent when debug is disabled; got {names:?}"
        );
    }
}

#[test]
fn mcp_registry_exposes_debug_tools_as_extension_contributed_runtime_capabilities() {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, native_fixture_tower_config(&[], 5))
        .expect("engine builds with fixture debug config");

    let native_tools = NativeToolRegistry::new(Arc::clone(&handle.state))
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    for name in expected_debug_tool_names() {
        assert!(
            !EXPECTED_NATIVE_TOOL_NAMES.contains(&name),
            "{name} must not be added to the canonical native tool list"
        );
        assert!(
            !native_tools.iter().any(|tool| tool == name),
            "{name} must be extension-contributed, not registered as a native MCP tool"
        );
    }

    let mut merged = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);
    let merged_names = merged
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();

    for name in expected_debug_tool_names() {
        assert!(
            merged_names.iter().any(|tool| tool == name),
            "{name} must be present through extension discovery once debug config wiring lands; got {merged_names:?}"
        );
    }

    let initial_sessions = merged
        .call("tower_debug_sessions", json!({}))
        .expect("debug sessions tool returns structured runtime state");
    assert_eq!(
        initial_sessions["sessions"].as_array().map(Vec::len),
        Some(0),
        "new debug runtime should start with no sessions; got {initial_sessions}"
    );

    let launch = merged
        .call(
            "tower_debug_launch",
            json!({
                "language": "rust",
                "program": "fixture-program",
                "cwd": null,
                "args": [],
                "env": {},
                "launch_overrides": {}
            }),
        )
        .expect("debug launch works through the merged MCP registry");
    let session_id = launch["session_id"]
        .as_str()
        .expect("launch returns session_id")
        .to_owned();
    assert_eq!(launch["state"], "stopped");

    let sessions = merged
        .call("tower_debug_sessions", json!({}))
        .expect("launched session is visible through MCP sessions tool");
    assert!(
        sessions["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .any(|session| session["session_id"] == session_id),
        "sessions tool should include launched session {session_id}; got {sessions}"
    );

    let terminated = merged
        .call("tower_debug_terminate", json!({ "session_id": session_id }))
        .expect("debug terminate works through the merged MCP registry");
    assert_eq!(terminated["ok"], true);
}

#[test]
fn daemon_engine_wires_tower_config_debug_into_extension_loading_without_domain_debug_code() {
    let names_without_debug = merged_tool_names(TowerConfig::default());
    for name in expected_debug_tool_names() {
        assert!(
            names_without_debug.iter().all(|tool| tool != name),
            "{name} must be absent from daemon-built extension tools when TowerConfig.debug is empty; got {names_without_debug:?}"
        );
    }

    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle =
        build_engine(&opts, debug_config()).expect("engine builds with parsed debug config");

    let native_names = NativeToolRegistry::new(Arc::clone(&handle.state))
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    let merged_names = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry)
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();

    for name in expected_debug_tool_names() {
        assert!(
            native_names.iter().all(|tool| tool != name),
            "{name} must not be registered as a native daemon tool; got {native_names:?}"
        );
        assert!(
            merged_names.iter().any(|tool| tool == name),
            "{name} must be exposed through daemon-built extension loading when TowerConfig.debug is present; got {merged_names:?}"
        );
    }
}

#[test]
fn eval_at_absent_without_debug_config_or_with_empty_debug_config() {
    let absent_names = merged_tool_names(TowerConfig::default());
    assert!(
        absent_names
            .iter()
            .all(|tool| tool != "tower_debug_eval_at"),
        "tower_debug_eval_at must be absent when debug config is missing; got {absent_names:?}"
    );

    let empty_debug_config: TowerConfig =
        toml::from_str("[debug]\n").expect("empty debug config must parse");
    let empty_names = merged_tool_names(empty_debug_config);
    assert!(
        empty_names.iter().all(|tool| tool != "tower_debug_eval_at"),
        "tower_debug_eval_at must be absent when debug config is empty; got {empty_names:?}"
    );
}

#[test]
fn eval_at_present_in_merged_registry_with_fixture_debug_config() {
    let names = merged_tool_names(native_fixture_tower_config(&[], 5));

    assert!(
        names.iter().any(|tool| tool == "tower_debug_eval_at"),
        "tower_debug_eval_at must be present in the merged registry with fixture-backed debug config; got {names:?}"
    );
}

#[test]
fn eval_at_fixture_hit_returns_stack_output_expression_and_no_session_id() {
    let token = format!("eval-at-hit-{}", std::process::id());
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, native_fixture_tower_config(&["--token", &token], 5))
        .expect("engine builds with fixture debug config");
    let mut merged = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    let result = merged
        .call("tower_debug_eval_at", eval_at_request(5_000))
        .expect("fixture-backed eval-at hit returns a successful payload");

    assert_eq!(result["hit"], true, "eval-at hit payload: {result}");
    assert_eq!(
        result["finished"], "stopped",
        "eval-at hit payload: {result}"
    );
    assert!(
        result["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "eval-at hit must include at least one hit; got {result}"
    );
    assert_eq!(result["hits"][0]["frame"]["name"], "main");
    assert!(
        result["hits"][0]["stack"]
            .as_array()
            .is_some_and(|stack| !stack.is_empty()),
        "eval-at hit must include stack evidence; got {result}"
    );
    assert_eq!(result["hits"][0]["evaluated"]["answer"]["value"], "42");
    assert!(
        result["output"]
            .as_array()
            .is_some_and(|output| !output.is_empty()),
        "eval-at hit must include captured fixture output; got {result}"
    );
    assert_object_has_no_key_recursively(&result, "session_id");
    assert_fixture_processes_gone(&token);
}

#[test]
fn eval_at_fixture_no_hit_exit_returns_successful_exited_payload_and_no_leaked_process() {
    let token = format!("eval-at-no-hit-{}", std::process::id());
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(
        &opts,
        native_fixture_tower_config(&["--token", &token, "--eval-at-scenario=no-hit-exit"], 5),
    )
    .expect("engine builds with no-hit fixture debug config");
    let mut merged = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    let result = merged
        .call(
            "tower_debug_eval_at",
            json!({
                "lang": "rust",
                "program": "fixture-program",
                "expressions": ["answer"],
                "timeout_ms": 5_000
            }),
        )
        .expect("fixture-backed eval-at no-hit exit returns a successful payload");

    assert_eq!(result["hit"], false, "eval-at no-hit payload: {result}");
    assert_eq!(
        result["finished"], "exited",
        "eval-at no-hit payload: {result}"
    );
    assert_eq!(result["exit_code"], 0, "eval-at no-hit payload: {result}");
    assert!(
        result["output"].as_array().is_some(),
        "eval-at no-hit must include an output payload; got {result}"
    );
    assert_object_has_no_key_recursively(&result, "session_id");
    assert_fixture_processes_gone(&token);
}

#[test]
fn eval_at_fixture_timeout_returns_successful_timeout_payload_and_no_leaked_process() {
    let token = format!("eval-at-timeout-{}", std::process::id());
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(
        &opts,
        native_fixture_tower_config(&["--token", &token, "--continue-delay-ms=1500"], 5),
    )
    .expect("engine builds with timeout fixture debug config");
    let mut merged = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    let started = Instant::now();
    let result = merged
        .call("tower_debug_eval_at", eval_at_request(100))
        .expect("fixture-backed eval-at timeout returns a successful payload");

    assert!(
        started.elapsed() <= Duration::from_secs(2),
        "eval-at timeout must complete without hanging"
    );
    assert_eq!(result["hit"], false, "eval-at timeout payload: {result}");
    assert_eq!(
        result["finished"], "timeout",
        "eval-at timeout payload: {result}"
    );
    assert!(
        result["output"].as_array().is_some(),
        "eval-at timeout must include an output payload; got {result}"
    );
    assert_object_has_no_key_recursively(&result, "session_id");
    assert_fixture_processes_gone(&token);
}
