// Feature: F006

#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::TowerConfig;
use core_engine::adapters::daemon::engine::build_engine;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::registry::ToolRegistry;
use serde_json::{Value, json};

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn fixture_debug_adapter_bin() -> std::path::PathBuf {
    workspace_root()
        .join("target")
        .join("debug")
        .join("fixture_debug_adapter")
}

fn rr_fixture_tower_config(token: &str) -> TowerConfig {
    let command = serde_json::to_string(
        fixture_debug_adapter_bin()
            .to_str()
            .expect("fixture path must be utf8"),
    )
    .expect("fixture path must serialize");
    let token = serde_json::to_string(token).expect("token must serialize");

    toml::from_str(&format!(
        r#"
[extensions]
request_timeout_secs = 5

[debug.rust]
extensions = ["rs"]
command = {command}
args = ["--token", {token}]
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
    .expect("fixture-backed rr config must parse")
}

fn reverse_debug_registry(token: &str) -> (tempfile::TempDir, ExtensionMergedRegistry) {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").expect("write source");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, rr_fixture_tower_config(token))
        .expect("engine builds with debug sidecar");

    (
        workspace,
        ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry),
    )
}

fn record_request(token: &str, scenario: &str) -> Value {
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

fn cleanup_side_channel_path(token: &str) -> std::path::PathBuf {
    let sanitized = token
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect::<String>();
    std::env::temp_dir().join(format!("tower-debug-fixture-cleanup-{sanitized}.jsonl"))
}

fn remove_cleanup_side_channel(token: &str) {
    let _ = std::fs::remove_file(cleanup_side_channel_path(token));
}

fn assert_record_cleanup_once(record_result: &Value, token: &str) {
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
        "record cleanup token {token} must appear exactly once in {record_result}"
    );
}

fn assert_replay_cleanup_once(token: &str) {
    let path = cleanup_side_channel_path(token);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture cleanup side channel {path:?}: {error}"));
    let cleanup_count = content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event"] == "cleanup" && event["token"] == token)
        .count();

    assert_eq!(
        cleanup_count, 1,
        "replay cleanup token {token} must appear exactly once in {path:?}: {content}"
    );
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

#[test]
fn reverse_debug_records_replays_and_finds_origin_through_public_tools() {
    let replay_token = format!("functional-reverse-debug-replay-{}", std::process::id());
    remove_cleanup_side_channel(&replay_token);
    let (_workspace, mut registry) = reverse_debug_registry(&replay_token);

    let record = registry
        .call(
            "tower_debug_record",
            record_request(&replay_token, "watchpoint_stop"),
        )
        .expect("tower_debug_record returns a fixture-backed trace");
    assert_eq!(record["recordable"], true, "record payload: {record}");
    assert_record_cleanup_once(&record, &replay_token);
    let trace_id = record["trace_id"]
        .as_str()
        .expect("record returns trace_id")
        .to_owned();

    let replay = registry
        .call(
            "tower_debug_replay",
            json!({ "trace_id": trace_id, "language": "rust", "timeout_secs": 5 }),
        )
        .expect("tower_debug_replay opens the recorded trace");
    assert_eq!(replay["state"], "stopped", "replay payload: {replay}");
    assert_eq!(replay["supportsStepBack"], true, "replay payload: {replay}");
    let session_id = replay["session_id"]
        .as_str()
        .expect("replay returns session_id")
        .to_owned();

    let reverse_stop = registry
        .call(
            "tower_debug_reverse_continue",
            json!({ "session_id": session_id, "thread_id": 1, "timeout_secs": 5 }),
        )
        .expect("tower_debug_reverse_continue returns a stop");
    assert_eq!(
        reverse_stop["reason"], "watchpoint",
        "reverse payload: {reverse_stop}"
    );

    let terminated = registry
        .call("tower_debug_terminate", json!({ "session_id": session_id }))
        .expect("tower_debug_terminate cleans up replay session");
    assert_eq!(terminated["ok"], true, "terminate payload: {terminated}");
    assert_replay_cleanup_once(&replay_token);
    assert_fixture_processes_gone(&replay_token);

    let origin_token = format!("functional-reverse-debug-origin-{}", std::process::id());
    remove_cleanup_side_channel(&origin_token);
    let (_workspace, mut registry) = reverse_debug_registry(&origin_token);

    let record = registry
        .call(
            "tower_debug_record",
            record_request(&origin_token, "watchpoint_stop"),
        )
        .expect("tower_debug_record returns a fixture-backed trace");
    assert_eq!(record["recordable"], true, "record payload: {record}");
    assert_record_cleanup_once(&record, &origin_token);
    let trace_id = record["trace_id"]
        .as_str()
        .expect("record returns trace_id")
        .to_owned();

    let origin = registry
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
        .expect("tower_debug_find_origin returns origin evidence");
    assert_eq!(origin["found"], true, "origin payload: {origin}");
    assert_eq!(
        origin["write_frame"]["name"], "main",
        "origin payload: {origin}"
    );
    assert_eq!(
        origin["value"]["name"], "answer",
        "origin payload: {origin}"
    );
    assert_eq!(origin["value"]["value"], "42", "origin payload: {origin}");
    assert_replay_cleanup_once(&origin_token);
    assert_fixture_processes_gone(&origin_token);
}

#[test]
fn reverse_debug_reports_no_prior_write_as_structured_tool_result() {
    let token = format!("functional-reverse-debug-no-prior-{}", std::process::id());
    remove_cleanup_side_channel(&token);
    let (_workspace, mut registry) = reverse_debug_registry(&token);

    let result = registry
        .call(
            "tower_debug_record_and_find_origin",
            json!({
                "record": record_request(&token, "no_prior_write"),
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
        .expect("expected no-prior-write returns a structured successful payload");

    assert_eq!(result["record"]["recordable"], true, "payload: {result}");
    assert_record_cleanup_once(&result["record"], &token);
    assert_eq!(result["origin"]["found"], false, "payload: {result}");
    assert_eq!(
        result["origin"]["reason"], "no_prior_write_reached",
        "payload: {result}"
    );
    assert_eq!(result["origin"]["error"], Value::Null, "payload: {result}");
    assert_replay_cleanup_once(&token);
    assert_fixture_processes_gone(&token);
}
