// Feature: F004
// Feature: F005

#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    DebugInitError, DebugInitializeConfig, DebugRecordConfig, DebugToolError, DebugToolErrorCode,
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

fn rr_fixture_tower_config() -> TowerConfig {
    rr_fixture_tower_config_with_args(&[])
}

fn rr_fixture_tower_config_with_args(args: &[&str]) -> TowerConfig {
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
request_timeout_secs = 5

[debug.rust]
extensions = ["rs"]
command = {command}
args = [{args}]
adapter_type = "fixture"
default_timeout_secs = 5
idle_ttl_secs = 300

[debug.record]
backend = "rr"
trace_dir = ".tower/traces"
ttl_secs = 86400
max_traces = 25
record_timeout_secs = 30
"#
    ))
    .expect("native fixture debug config with rr record backend must parse")
}

fn real_rr_native_fixture_tower_config() -> TowerConfig {
    let command = serde_json::to_string(
        fixture_debug_adapter_bin()
            .to_str()
            .expect("fixture path must be utf8"),
    )
    .expect("fixture path must serialize");
    toml::from_str(&format!(
        r#"
[extensions]
request_timeout_secs = 10

[debug.rust]
extensions = ["rs"]
command = {command}
args = []
adapter_type = "fixture"
default_timeout_secs = 5
idle_ttl_secs = 300

[debug.record]
backend = "rr"
trace_dir = ".tower/traces"
ttl_secs = 86400
max_traces = 25
record_timeout_secs = 30
"#
    ))
    .expect("real rr native fixture config must parse")
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

fn expected_rr_specific_manifest_tool_names() -> Vec<&'static str> {
    vec![
        "record",
        "replay",
        "reverse_continue",
        "step_back",
        "watchpoint",
        "traces",
        "delete_trace",
        "find_origin",
        "record_and_find_origin",
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

fn valid_rr_debug_init_payload() -> serde_json::Value {
    valid_rr_debug_init_payload_with_trace_dir(".tower/traces")
}

fn valid_rr_debug_init_payload_with_trace_dir(trace_dir: &str) -> serde_json::Value {
    let mut payload = valid_debug_init_payload();
    payload["record"] = json!({
        "backend": "rr",
        "trace_dir": trace_dir,
        "ttl_secs": 86400,
        "max_traces": 25,
        "record_timeout_secs": 30
    });
    payload
}

fn unique_relative_trace_dir(label: &str) -> String {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    format!(".tower/test-traces-{label}-{}-{unique}", std::process::id())
}

fn reverse_debug_fixture_init_payload(token: &str) -> serde_json::Value {
    let command = fixture_debug_adapter_bin();
    let command = command
        .to_str()
        .expect("fixture path must be utf8")
        .to_owned();
    json!({
        "languages": {
            "rust": {
                "extensions": ["rs"],
                "command": command,
                "args": ["--token", token],
                "adapter_type": "fixture",
                "launch": {
                    "request": "launch"
                },
                "default_timeout_secs": 5,
                "idle_ttl_secs": 300
            }
        },
        "record": {
            "backend": "rr",
            "trace_dir": ".tower/traces",
            "ttl_secs": 86400,
            "max_traces": 25,
            "record_timeout_secs": 30,
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

fn reverse_debug_fixture_registry(
    token: &str,
    fixture_args: &[&str],
) -> (tempfile::TempDir, ExtensionMergedRegistry) {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let mut args = vec!["--token".to_owned(), token.to_owned()];
    args.extend(
        fixture_args
            .iter()
            .copied()
            .filter(|arg| *arg != "--scenario" && !arg.starts_with("--scenario="))
            .map(str::to_owned),
    );
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, rr_fixture_tower_config_with_args(&arg_refs))
        .expect("engine builds with fixture-backed reverse debug config");
    (
        workspace,
        ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry),
    )
}

fn reverse_debug_record_request(token: &str, scenario: &str) -> serde_json::Value {
    json!({
        "language": "rust",
        "program": "fixture-program",
        "args": [scenario],
        "cwd": null,
        "env": {
            "TOWER_DEBUG_FIXTURE_CLEANUP_TOKEN": token,
            "TOWER_DEBUG_FIXTURE_SCENARIO": scenario
        },
        "timeout_ms": 5_000
    })
}

fn reverse_debug_record_trace(
    merged: &mut ExtensionMergedRegistry,
    token: &str,
    scenario: &str,
) -> String {
    let record = merged
        .call(
            "tower_debug_record",
            reverse_debug_record_request(token, scenario),
        )
        .expect("tower_debug_record returns a structured fixture result");
    assert_eq!(record["recordable"], true, "record payload: {record}");
    record["trace_id"]
        .as_str()
        .expect("record result includes trace_id")
        .to_owned()
}

fn reverse_debug_cleanup_record_request(token: &str, scenario: &str) -> serde_json::Value {
    json!({
        "language": "rust",
        "program": fixture_debug_adapter_bin(),
        "args": ["--scenario", scenario],
        "cwd": null,
        "env": {
            "TOWER_DEBUG_FIXTURE_CLEANUP_TOKEN": token
        },
        "timeout_ms": 5_000
    })
}

fn assert_cleanup_event_emitted_exactly_once(record_result: &Value, token: &str) {
    let cleanup_count = record_result["output"]
        .as_array()
        .expect("record result includes captured output array")
        .iter()
        .flat_map(|output| output["text"].as_str().unwrap_or_default().lines())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event"] == "cleanup" && event["token"] == token)
        .count();
    assert_eq!(
        cleanup_count, 1,
        "cleanup token {token} must emit exactly one cleanup event in record output: {record_result}"
    );
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

fn invoke_debug_tool_raw(
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
    response
}

fn shutdown_debug_child(child: &mut RawDebugChild, id: u64) {
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "shutdown",
        "params": {}
    }));
    let shutdown = child.read_frame();
    assert_eq!(shutdown["id"], id);
    assert_eq!(shutdown["result"]["type"], "Ack");
}

fn manifest_tool_names() -> Vec<String> {
    let manifest = fs::read_to_string(workspace_root().join("extensions/debug/extension.toml"))
        .expect("read debug manifest");
    let value: toml::Value = toml::from_str(&manifest).expect("debug manifest parses as TOML");
    value["tools"]
        .as_array()
        .expect("debug manifest tools array")
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("manifest tool name")
                .to_owned()
        })
        .collect()
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

    let record_without_languages = DebugInitializeConfig::from_init_payload(Some(json!({
        "record": {
            "backend": "rr",
            "trace_dir": ".tower/traces",
            "ttl_secs": 86400,
            "max_traces": 25,
            "record_timeout_secs": 30
        }
    })));
    assert!(
        matches!(
            record_without_languages,
            Err(DebugInitError::InvalidConfig(_))
        ),
        "present payload with record but without languages must be rejected as DebugInitError::InvalidConfig; got {record_without_languages:?}"
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
fn debug_tool_declarations_existing_debug_tools_remain_absent_when_debug_language_config_is_absent_or_empty()
 {
    let record_only_config = DebugInitializeConfig::from_init_payload(Some(json!({
        "languages": {},
        "record": {
            "backend": "rr",
            "trace_dir": ".tower/traces",
            "ttl_secs": 86400,
            "max_traces": 25,
            "record_timeout_secs": 30
        }
    })))
    .expect("record-only initialize config parses")
    .expect("present record-only config");
    let empty_config = DebugInitializeConfig::from_init_payload(Some(json!({ "languages": {} })))
        .expect("empty map is a valid config")
        .expect("empty map still yields config");

    for config in [None, Some(&empty_config), Some(&record_only_config)] {
        let declarations = debug_tool_declarations(config);
        for name in expected_manifest_tool_names() {
            assert!(
                declarations.iter().all(|tool| tool.name != name),
                "{name} must be absent when debug language config is absent or empty; got {declarations:?}"
            );
        }
    }
}

#[test]
fn debug_tool_declarations_rr_specific_tools_remain_absent_when_debug_config_exists_but_record_backend_rr_is_absent()
 {
    let mut no_record = DebugInitializeConfig::from_init_payload(Some(valid_debug_init_payload()))
        .expect("valid debug config")
        .expect("present config");
    no_record.record = None;
    let non_rr_record = DebugInitializeConfig {
        record: Some(DebugRecordConfig {
            backend: "gdb".to_owned(),
            trace_dir: None,
            ttl_secs: None,
            max_traces: None,
            record_timeout_secs: None,
        }),
        ..no_record.clone()
    };

    for config in [&no_record, &non_rr_record] {
        let declarations = debug_tool_declarations(Some(config));
        for name in expected_rr_specific_manifest_tool_names() {
            assert!(
                declarations.iter().all(|tool| tool.name != name),
                "{name} must be absent when [debug.record] backend = \"rr\" is absent; got {declarations:?}"
            );
        }
    }
}

#[test]
fn debug_tool_declarations_rr_specific_tools_appear_when_record_backend_rr_is_configured() {
    let mut payload = valid_debug_init_payload();
    payload["record"] = json!({
        "backend": "rr",
        "trace_dir": ".tower/traces",
        "ttl_secs": 86400,
        "max_traces": 25,
        "record_timeout_secs": 30
    });
    let config = DebugInitializeConfig::from_init_payload(Some(payload))
        .expect("valid rr record config")
        .expect("present config");

    let declarations = debug_tool_declarations(Some(&config));
    let tool_names = declarations
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();

    for name in expected_manifest_tool_names()
        .into_iter()
        .chain(expected_rr_specific_manifest_tool_names())
    {
        assert!(
            tool_names.contains(&name),
            "{name} must be declared when debug languages and record.backend = \"rr\" are configured; got {tool_names:?}"
        );
    }
}

#[test]
fn debug_initialize_config_rejects_invalid_record_payloads() {
    for (record, expected_message) in [
        (
            json!({
                "backend": "gdb",
                "trace_dir": ".tower/traces",
                "ttl_secs": 86400,
                "max_traces": 25,
                "record_timeout_secs": 30
            }),
            "debug.record.backend",
        ),
        (
            json!({
                "backend": "rr",
                "trace_dir": "/tmp/traces",
                "ttl_secs": 86400,
                "max_traces": 25,
                "record_timeout_secs": 30
            }),
            "debug.record.trace_dir",
        ),
        (
            json!({
                "backend": "rr",
                "trace_dir": ".tower/traces",
                "ttl_secs": 0,
                "max_traces": 25,
                "record_timeout_secs": 30
            }),
            "debug.record.ttl_secs",
        ),
        (
            json!({
                "backend": "rr",
                "trace_dir": ".tower/traces",
                "ttl_secs": 86400,
                "max_traces": 0,
                "record_timeout_secs": 30
            }),
            "debug.record.max_traces",
        ),
        (
            json!({
                "backend": "rr",
                "trace_dir": ".tower/traces",
                "ttl_secs": 86400,
                "max_traces": 25,
                "record_timeout_secs": 0
            }),
            "debug.record.record_timeout_secs",
        ),
    ] {
        let mut payload = valid_debug_init_payload();
        payload["record"] = record;
        let error = DebugInitializeConfig::from_init_payload(Some(payload))
            .expect_err("invalid record initialize payload must fail closed");
        assert!(
            error.jsonrpc_message().contains(expected_message),
            "expected {expected_message} in error message; got {error}"
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
fn debug_process_with_rr_record_backend_declares_manifest_parity_for_rr_tool_surface() {
    let mut child = RawDebugChild::spawn();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 33,
        "method": "initialize",
        "params": {
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "debug-e2e/0.1.0",
            "extension_config": valid_rr_debug_init_payload()
        }
    }));

    let initialized = child.read_frame();
    assert_eq!(initialized["id"], 33);
    let runtime_names = initialized["result"]["data"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect::<Vec<_>>();
    let manifest_names = manifest_tool_names();
    let expected = expected_manifest_tool_names()
        .into_iter()
        .chain(expected_rr_specific_manifest_tool_names())
        .map(str::to_owned)
        .collect::<Vec<_>>();

    assert_eq!(
        runtime_names, expected,
        "rr runtime declarations must expose the exact debug plus rr tool surface"
    );
    for name in &expected {
        assert!(
            manifest_names.contains(name),
            "debug manifest must ship tool {name}; manifest names: {manifest_names:?}"
        );
    }
}

#[test]
fn debug_process_declared_rr_handlers_return_their_public_dtos() {
    let mut child = RawDebugChild::spawn();
    initialize_debug_child(
        &mut child,
        34,
        valid_rr_debug_init_payload_with_trace_dir(&unique_relative_trace_dir("public-dtos")),
    );

    let traces = invoke_debug_tool_raw(&mut child, 35, "traces", json!({}));
    assert_eq!(traces["result"]["data"], json!({ "traces": [] }));

    let delete_trace = invoke_debug_tool_raw(
        &mut child,
        36,
        "delete_trace",
        json!({ "trace_id": "missing-trace" }),
    );
    assert_eq!(delete_trace["result"]["data"]["deleted"], false);
    assert_eq!(
        delete_trace["result"]["data"]["error"]["code"],
        "trace_not_found"
    );

    let find_origin = invoke_debug_tool_raw(
        &mut child,
        37,
        "find_origin",
        json!({
            "trace_id": "trace-origin",
            "language": "rust",
            "watch": "answer",
            "at": { "kind": "crash" }
        }),
    );
    assert_eq!(find_origin["result"]["data"]["found"], false);
    assert_eq!(find_origin["result"]["data"]["reason"], "trace_not_found");

    let record_and_find_origin = invoke_debug_tool_raw(
        &mut child,
        38,
        "record_and_find_origin",
        json!({
            "record": {
                "language": "rust",
                "program": "target/debug/app",
                "args": [],
                "cwd": null,
                "env": {},
                "timeout_ms": 1000
            },
            "origin": {
                "language": "rust",
                "watch": "answer",
                "at": { "kind": "end" },
                "timeout_secs": 1,
                "max_depth": 2,
                "max_children": 4
            }
        }),
    );
    assert!(record_and_find_origin["result"]["data"]["record"].is_object());
    assert!(
        record_and_find_origin["result"]["data"]["origin"].is_null()
            || record_and_find_origin["result"]["data"]["origin"].is_object(),
        "origin must be null when recording is unsupported or a structured result when recording succeeds: {record_and_find_origin}"
    );
    assert_eq!(
        record_and_find_origin["result"]["data"]["error"],
        Value::Null
    );
}

#[test]
fn debug_process_record_rejects_fields_outside_public_record_params_contract() {
    let mut child = RawDebugChild::spawn();
    initialize_debug_child(&mut child, 39, valid_rr_debug_init_payload());

    let response = invoke_debug_tool_raw(
        &mut child,
        40,
        "record",
        json!({
            "language": "rust",
            "program": "target/debug/app",
            "launch_overrides": {}
        }),
    );

    assert_eq!(response["error"]["code"], -32602);
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
    for name in expected_rr_specific_manifest_tool_names() {
        let mcp_name = format!("tower_debug_{name}");
        assert!(
            present_names.iter().all(|tool| tool != &mcp_name),
            "{mcp_name} must be omitted from normal MCP discovery when [debug.record] backend = \"rr\" is absent; got {present_names:?}"
        );
    }
}

#[test]
fn rr_specific_tools_appear_in_merged_registry_only_when_record_backend_rr_is_configured() {
    let names = merged_tool_names(rr_fixture_tower_config());

    for name in expected_rr_specific_manifest_tool_names() {
        let mcp_name = format!("tower_debug_{name}");
        assert!(
            names.iter().any(|tool| tool == &mcp_name),
            "{mcp_name} must be present in normal MCP discovery when [debug.record] backend = \"rr\" is configured; got {names:?}"
        );
    }
}

#[test]
fn reverse_debug_rr_tools_are_absent_without_record_backend_rr_and_present_with_valid_debug_plus_record_config()
 {
    let names_without_record = merged_tool_names(native_fixture_tower_config(&[], 5));
    for name in expected_rr_specific_manifest_tool_names() {
        let mcp_name = format!("tower_debug_{name}");
        assert!(
            names_without_record.iter().all(|tool| tool != &mcp_name),
            "{mcp_name} must be absent without [debug.record] backend = \"rr\"; got {names_without_record:?}"
        );
    }

    let names_with_record = merged_tool_names(rr_fixture_tower_config());
    for name in expected_rr_specific_manifest_tool_names() {
        let mcp_name = format!("tower_debug_{name}");
        assert!(
            names_with_record.iter().any(|tool| tool == &mcp_name),
            "{mcp_name} must be present with valid debug plus rr record config; got {names_with_record:?}"
        );
    }
}

#[test]
fn reverse_debug_fixture_backed_e2e_covers_record_replay_reverse_continue_step_back_watchpoint_traces_and_delete_trace()
 {
    let token = format!("reverse-debug-tool-surface-{}", std::process::id());
    let (_workspace, mut merged) =
        reverse_debug_fixture_registry(&token, &["--scenario=record_ok"]);

    let record = merged
        .call(
            "tower_debug_record",
            reverse_debug_record_request(&token, "record_ok"),
        )
        .expect("tower_debug_record returns fixture-backed record result");
    assert_eq!(record["recordable"], true, "record payload: {record}");
    let trace_id = record["trace_id"]
        .as_str()
        .expect("record returns trace_id")
        .to_owned();

    let traces = merged
        .call("tower_debug_traces", json!({}))
        .expect("tower_debug_traces returns recorded fixture trace");
    assert!(
        traces["traces"]
            .as_array()
            .expect("traces array")
            .iter()
            .any(|trace| trace["trace_id"] == trace_id),
        "traces should include {trace_id}; got {traces}"
    );

    let replay = merged
        .call(
            "tower_debug_replay",
            json!({ "trace_id": trace_id, "language": "rust", "timeout_secs": 5 }),
        )
        .expect("tower_debug_replay opens fixture replay");
    assert_eq!(replay["state"], "stopped", "replay payload: {replay}");
    assert_eq!(replay["supportsStepBack"], true, "replay payload: {replay}");
    let session_id = replay["session_id"]
        .as_str()
        .expect("replay returns session_id")
        .to_owned();

    let reverse = merged
        .call(
            "tower_debug_reverse_continue",
            json!({ "session_id": session_id, "thread_id": 1, "timeout_secs": 5 }),
        )
        .expect("tower_debug_reverse_continue returns a deterministic stop");
    assert_eq!(reverse["state"], "stopped", "reverse payload: {reverse}");
    assert_eq!(
        reverse["reason"], "watchpoint",
        "reverse payload: {reverse}"
    );

    let stepped = merged
        .call(
            "tower_debug_step_back",
            json!({ "session_id": session_id, "thread_id": 1, "granularity": "line", "timeout_secs": 5 }),
        )
        .expect("tower_debug_step_back returns a deterministic line stop");
    assert_eq!(stepped["state"], "stopped", "step_back payload: {stepped}");
    assert_eq!(
        stepped["top_frame"]["line"], 11,
        "step_back payload: {stepped}"
    );

    let watchpoint = merged
        .call(
            "tower_debug_watchpoint",
            json!({ "session_id": session_id, "expression": "answer", "address": null, "kind": "write", "enabled": true }),
        )
        .expect("tower_debug_watchpoint sets a replay watchpoint");
    assert_eq!(watchpoint["ok"], true, "watchpoint payload: {watchpoint}");
    assert_eq!(
        watchpoint["watchpoint"]["expression"], "answer",
        "watchpoint payload: {watchpoint}"
    );

    let deleted = merged
        .call("tower_debug_delete_trace", json!({ "trace_id": trace_id }))
        .expect("tower_debug_delete_trace removes the fixture trace");
    assert_eq!(deleted["deleted"], true, "delete payload: {deleted}");
    let terminated = merged
        .call("tower_debug_terminate", json!({ "session_id": session_id }))
        .expect("tower_debug_terminate cleans up the replay session");
    assert_eq!(terminated["ok"], true, "terminate payload: {terminated}");
    assert_fixture_processes_gone(&token);
}

#[test]
fn reverse_debug_fixture_backed_e2e_find_origin_returns_found_true_with_frame_and_value_evidence() {
    let token = format!("reverse-debug-origin-found-{}", std::process::id());
    let (_workspace, mut merged) =
        reverse_debug_fixture_registry(&token, &["--scenario=watchpoint_stop"]);
    let trace_id = reverse_debug_record_trace(&mut merged, &token, "watchpoint_stop");

    let origin = merged
        .call(
            "tower_debug_find_origin",
            json!({
                "trace_id": trace_id,
                "language": "rust",
                "watch": "answer",
                "at": { "kind": "end" },
                "timeout_secs": 5,
                "max_depth": 8,
                "max_children": 8
            }),
        )
        .expect("tower_debug_find_origin returns fixture origin result");

    assert_eq!(origin["found"], true, "origin payload: {origin}");
    assert_eq!(origin["reason"], Value::Null, "origin payload: {origin}");
    assert_eq!(
        origin["write_frame"]["name"], "main",
        "origin payload: {origin}"
    );
    assert_eq!(
        origin["value"]["name"], "answer",
        "origin payload: {origin}"
    );
    assert_eq!(origin["value"]["value"], "42", "origin payload: {origin}");
    assert!(
        origin["stack"]
            .as_array()
            .is_some_and(|stack| !stack.is_empty()),
        "origin should include stack evidence; got {origin}"
    );
    assert_fixture_processes_gone(&token);
}

#[test]
fn reverse_debug_fixture_backed_e2e_find_origin_returns_found_false_with_reason_no_prior_write_reached()
 {
    let token = format!("reverse-debug-origin-none-{}", std::process::id());
    let (_workspace, mut merged) =
        reverse_debug_fixture_registry(&token, &["--scenario=no_prior_write"]);
    let trace_id = reverse_debug_record_trace(&mut merged, &token, "no_prior_write");

    let origin = merged
        .call(
            "tower_debug_find_origin",
            json!({
                "trace_id": trace_id,
                "language": "rust",
                "watch": "answer",
                "at": { "kind": "end" },
                "timeout_secs": 5,
                "max_depth": 8,
                "max_children": 8
            }),
        )
        .expect("tower_debug_find_origin returns fixture no-prior-write result");

    assert_eq!(origin["found"], false, "origin payload: {origin}");
    assert_eq!(
        origin["reason"], "no_prior_write_reached",
        "origin payload: {origin}"
    );
    assert_eq!(origin["error"], Value::Null, "origin payload: {origin}");
    assert_fixture_processes_gone(&token);
}

#[test]
fn reverse_debug_fixture_backed_e2e_record_and_find_origin_success_and_recording_success_origin_failure_cleanup()
 {
    for (scenario, expected_found, expected_reason, expected_error_code) in [
        ("watchpoint_stop", true, Value::Null, Value::Null),
        (
            "no_prior_write",
            false,
            json!("no_prior_write_reached"),
            Value::Null,
        ),
        (
            "adapter_exited",
            false,
            Value::Null,
            json!("adapter_exited"),
        ),
    ] {
        let token = format!("reverse-debug-combined-{scenario}-{}", std::process::id());
        let (_workspace, mut merged) =
            reverse_debug_fixture_registry(&token, &[&format!("--scenario={scenario}")]);
        let result = merged
            .call(
                "tower_debug_record_and_find_origin",
                json!({
                    "record": reverse_debug_record_request(&token, scenario),
                    "origin": {
                        "language": "rust",
                        "watch": "answer",
                        "at": { "kind": "end" },
                        "timeout_secs": 5,
                        "max_depth": 8,
                        "max_children": 8
                    }
                }),
            )
            .expect("tower_debug_record_and_find_origin returns fixture result");

        assert_eq!(
            result["record"]["recordable"], true,
            "combined payload: {result}"
        );
        assert_eq!(
            result["origin"]["found"], expected_found,
            "combined payload: {result}"
        );
        assert_eq!(
            result["origin"]["reason"], expected_reason,
            "combined payload: {result}"
        );
        assert_eq!(
            result["origin"]["error"]["code"], expected_error_code,
            "combined payload: {result}"
        );
        assert_fixture_processes_gone(&token);
    }
}

#[test]
fn reverse_debug_cleanup_token_assertions_prove_record_and_replay_process_trees_are_reaped_after_success_timeout_no_prior_write_adapter_exit_and_recipe_failure()
 {
    for (case, scenario) in [
        ("record", "record_ok"),
        ("replay", "replay_open"),
        ("timeout", "timeout"),
        ("no_prior_write", "no_prior_write"),
        ("adapter_exit", "adapter_exited"),
        ("recipe_failure", "adapter_exited"),
    ] {
        let replay_token = format!("reverse-debug-cleanup-replay-{case}-{}", std::process::id());
        let record_token = format!("reverse-debug-cleanup-record-{case}-{}", std::process::id());
        let (_workspace, mut merged) =
            reverse_debug_fixture_registry(&replay_token, &[&format!("--scenario={scenario}")]);
        let result = merged.call(
            "tower_debug_record_and_find_origin",
            json!({
                "record": reverse_debug_cleanup_record_request(&record_token, scenario),
                "origin": {
                    "language": "rust",
                    "watch": "answer",
                    "at": { "kind": "end" },
                    "timeout_secs": 1,
                    "max_depth": 4,
                    "max_children": 4
                }
            }),
        );
        assert!(
            result.is_ok(),
            "scenario {scenario} must return a structured result before cleanup assertion: {result:?}"
        );
        let result = result.expect("structured cleanup result");
        assert_cleanup_event_emitted_exactly_once(&result["record"], &record_token);
        assert_cleanup_event_emitted_exactly_once(&result["origin"], &replay_token);
        assert_fixture_processes_gone(&record_token);
        assert_fixture_processes_gone(&replay_token);
    }
}

#[test]
fn reverse_debug_gated_real_rr_native_fixture_records_and_replays_tiny_program_when_preflight_passes_or_skips_cleanly()
 {
    let preflight = Command::new("rr").arg("--version").output();
    let Ok(preflight) = preflight else {
        println!("SKIP reverse_debug real rr native fixture: rr binary is missing");
        return;
    };
    if !preflight.status.success() {
        println!(
            "SKIP reverse_debug real rr native fixture: rr preflight failed: {}",
            String::from_utf8_lossy(&preflight.stderr)
        );
        return;
    }

    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, real_rr_native_fixture_tower_config())
        .expect("engine builds with real rr native fixture config");
    let mut merged = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);
    let fixture_path = fixture_debug_adapter_bin()
        .to_str()
        .expect("fixture path must be utf8")
        .to_owned();
    let record = merged
        .call(
            "tower_debug_record",
            json!({
                "language": "rust",
                "program": fixture_path,
                "args": [],
                "cwd": null,
                "env": {},
                "timeout_ms": 30_000
            }),
        )
        .expect("real rr native fixture record returns structured result");
    assert_eq!(record["recordable"], true, "record payload: {record}");
    assert_eq!(
        record["trace"]["program"], fixture_path,
        "real rr record must trace the native fixture binary, not the scripted scenario fixture: {record}"
    );
    let trace_id = record["trace_id"]
        .as_str()
        .expect("real rr record returns trace_id")
        .to_owned();
    let replay = merged
        .call(
            "tower_debug_replay",
            json!({ "trace_id": trace_id, "language": "rust", "timeout_secs": 5 }),
        )
        .expect("real-rr gated fixture replay opens");
    assert_eq!(replay["state"], "stopped", "replay payload: {replay}");
}

#[test]
fn reverse_debug_twenty_parallel_initialize_record_or_replay_cleanup_shutdown_stress_test_completes_without_deadlock()
 {
    let handles = (0..20)
        .map(|index| {
            std::thread::spawn(move || {
                let scenario = if index % 2 == 0 {
                    "record_ok"
                } else {
                    "replay_open"
                };
                let token = format!("reverse-debug-stress-{index}-{}", std::process::id());
                let mut child = RawDebugChild::spawn();
                initialize_debug_child(&mut child, 1, reverse_debug_fixture_init_payload(&token));
                let record = invoke_debug_tool(
                    &mut child,
                    2,
                    "record",
                    reverse_debug_record_request(&token, scenario),
                );
                assert_eq!(
                    record["recordable"], true,
                    "stress record payload: {record}"
                );
                assert_cleanup_event_emitted_exactly_once(&record, &token);
                if index % 2 == 1 {
                    let trace_id = record["trace_id"]
                        .as_str()
                        .expect("stress record returns trace id")
                        .to_owned();
                    let replay = invoke_debug_tool(
                        &mut child,
                        3,
                        "replay",
                        json!({ "trace_id": trace_id, "language": "rust", "timeout_secs": 5 }),
                    );
                    let session_id = replay["session_id"]
                        .as_str()
                        .expect("stress replay returns session")
                        .to_owned();
                    let terminated = invoke_debug_tool(
                        &mut child,
                        4,
                        "terminate",
                        json!({ "session_id": session_id }),
                    );
                    assert_eq!(terminated["ok"], true);
                }
                shutdown_debug_child(&mut child, 5);
                assert_fixture_processes_gone(&token);
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle
            .join()
            .expect("reverse debug stress worker should not panic");
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
