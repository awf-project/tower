use std::io::{BufReader, Read, Write};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

#[test]
fn eval_at_no_hit_exit_arg_emits_exited_then_terminated_without_stopped() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fixture_debug_adapter"))
        .arg("--eval-at-scenario=no-hit-exit")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn fixture debug adapter");

    let mut stdin = child.stdin.take().expect("fixture stdin");
    write_frame(
        &mut stdin,
        &json!({
            "seq": 1,
            "type": "request",
            "command": "initialize",
            "arguments": {}
        }),
    );
    write_frame(
        &mut stdin,
        &json!({
            "seq": 2,
            "type": "request",
            "command": "launch",
            "arguments": {}
        }),
    );
    write_frame(
        &mut stdin,
        &json!({
            "seq": 3,
            "type": "request",
            "command": "continue",
            "arguments": {}
        }),
    );
    drop(stdin);

    let mut stdout = child.stdout.take().expect("fixture stdout");
    let mut output = Vec::new();
    stdout
        .read_to_end(&mut output)
        .expect("read fixture stdout");

    let status = child.wait().expect("wait for fixture debug adapter");
    assert!(status.success(), "fixture exited with {status}");

    let frames = read_json_frames(&output);
    let events = frames
        .iter()
        .filter(|frame| frame["type"] == "event")
        .collect::<Vec<_>>();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event"], "exited");
    assert_eq!(events[0]["body"]["exitCode"], 0);
    assert_eq!(events[1]["event"], "terminated");
    assert!(events.iter().all(|event| event["event"] != "stopped"));
}

#[test]
fn scripted_record_replay_timeline_scenarios_are_selected_by_cli_flag_or_env_and_cli_wins() {
    let env_only = run_scripted_scenario(
        &[],
        Some(("TOWER_DEBUG_FIXTURE_SCENARIO", "record_ok")),
        "scripted-env-record",
    );
    assert!(
        env_only.status.success(),
        "env-selected scenario should succeed; stderr={}",
        String::from_utf8_lossy(&env_only.stderr)
    );
    let env_events = parse_json_lines(&env_only.stdout);
    assert_eq!(env_events[0]["event"], "record");
    assert_eq!(env_events[0]["trace"], "trace-record-ok");

    let cli_wins = run_scripted_scenario(
        &["--scenario", "replay_open"],
        Some(("TOWER_DEBUG_FIXTURE_SCENARIO", "record_ok")),
        "scripted-cli-replay",
    );
    assert!(
        cli_wins.status.success(),
        "CLI-selected scenario should succeed; stderr={}",
        String::from_utf8_lossy(&cli_wins.stderr)
    );
    let cli_events = parse_json_lines(&cli_wins.stdout);
    assert_eq!(cli_events[0]["event"], "replay");
    assert_eq!(cli_events[0]["trace"], "trace-replay-open");
}

#[test]
fn scripted_scenario_selection_rejects_unsupported_names() {
    for (args, env) in [
        (
            vec!["--scenario", "unsupported"],
            Some(("TOWER_DEBUG_FIXTURE_SCENARIO", "record_ok")),
        ),
        (
            Vec::new(),
            Some(("TOWER_DEBUG_FIXTURE_SCENARIO", "unknown")),
        ),
    ] {
        let output = run_scripted_scenario(&args, env, "scripted-unsupported");

        assert!(
            !output.status.success(),
            "unsupported scenario must fail; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unsupported fixture scenario"),
            "unsupported scenario should explain the failure; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn scripted_scenarios_emit_one_json_line_per_event_with_exact_public_fields_and_cleanup_event() {
    for scenario in [
        "record_ok",
        "replay_open",
        "reverse_continue_stop",
        "step_back_line",
        "watchpoint_stop",
        "no_prior_write",
        "timeout",
        "adapter_exited",
    ] {
        let token = format!("scripted-fields-{scenario}");
        let output = run_scripted_scenario(&["--scenario", scenario], None, &token);
        assert!(
            output.status.success(),
            "scenario {scenario} should succeed; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events = parse_json_lines(&output.stdout);
        assert!(!events.is_empty(), "scenario {scenario} must emit events");
        for event in &events {
            let object = event.as_object().expect("fixture event is an object");
            assert_eq!(
                object.len(),
                6,
                "fixture event must have exact fields: {event}"
            );
            for key in ["event", "token", "session", "trace", "stop", "output"] {
                assert!(
                    object.contains_key(key),
                    "scenario {scenario} event missing field {key}: {event}"
                );
            }
            assert!(
                object.keys().all(
                    |key| ["event", "token", "session", "trace", "stop", "output"]
                        .contains(&key.as_str())
                ),
                "scenario {scenario} emitted unexpected event fields: {event}"
            );
            assert!(
                event["event"].is_string(),
                "event name must be a string: {event}"
            );
            assert_eq!(event["token"], token);
            assert!(
                event["session"].is_null() || event["session"].is_string(),
                "session must be null or string: {event}"
            );
            assert!(
                event["trace"].is_null() || event["trace"].is_string(),
                "trace must be null or string: {event}"
            );
            assert!(
                event["stop"].is_null() || event["stop"].is_object(),
                "stop must be null or object: {event}"
            );
            assert!(
                event["output"].is_null() || event["output"].is_string(),
                "output must be null or string: {event}"
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "cleanup" && event["token"] == token)
                .count(),
            1,
            "scenario {scenario} must emit exactly one cleanup event"
        );
    }
}

#[test]
fn scripted_cleanup_tokens_are_opaque_ascii_and_each_required_failure_or_success_path_cleans_once()
{
    for scenario in [
        "record_ok",
        "replay_open",
        "timeout",
        "no_prior_write",
        "adapter_exited",
        "watchpoint_stop",
    ] {
        let token = format!("OpaqueCleanupToken-{scenario}-A19");
        assert!(token.is_ascii());
        let output = run_scripted_scenario(&["--scenario", scenario], None, &token);
        assert!(
            output.status.success(),
            "scenario {scenario} should succeed; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events = parse_json_lines(&output.stdout);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "cleanup" && event["token"] == token)
                .count(),
            1,
            "scenario {scenario} must clean exactly once for token {token}"
        );
    }
}

#[test]
fn scripted_stop_events_use_deterministic_stop_sequence_ordering_not_wall_clock_order() {
    let output = run_scripted_scenario(
        &["--scenario", "reverse_continue_stop"],
        None,
        "scripted-sequence-order",
    );
    assert!(
        output.status.success(),
        "reverse_continue_stop should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sequences = parse_json_lines(&output.stdout)
        .into_iter()
        .filter_map(|event| event["stop"]["sequence"].as_u64())
        .collect::<Vec<_>>();

    assert_eq!(
        sequences,
        [1, 2, 3],
        "tests assert scripted stop ordering by stop.sequence"
    );
}

fn write_frame(mut writer: impl Write, value: &Value) {
    let body = serde_json::to_vec(value).expect("serialize DAP frame");
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).expect("write DAP header");
    writer.write_all(&body).expect("write DAP body");
}

fn run_scripted_scenario(args: &[&str], env: Option<(&str, &str)>, cleanup_token: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fixture_debug_adapter"));
    command
        .args(args)
        .env("TOWER_DEBUG_FIXTURE_CLEANUP_TOKEN", cleanup_token)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((name, value)) = env {
        command.env(name, value);
    }
    command.output().expect("run fixture scripted scenario")
}

fn parse_json_lines(output: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("fixture event JSON line"))
        .collect()
}

fn read_json_frames(output: &[u8]) -> Vec<Value> {
    let mut reader = BufReader::new(output);
    let mut frames = Vec::new();

    while let Some(header) = read_header(&mut reader) {
        let content_length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .expect("DAP Content-Length header");
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).expect("read DAP body");
        frames.push(serde_json::from_slice(&body).expect("parse DAP frame"));
    }

    frames
}

fn read_header(reader: &mut impl Read) -> Option<String> {
    let mut bytes = Vec::new();
    let mut byte = [0; 1];

    loop {
        match reader.read(&mut byte).expect("read DAP header") {
            0 if bytes.is_empty() => return None,
            0 => panic!("partial DAP header"),
            _ => {
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n") {
                    return Some(String::from_utf8(bytes).expect("DAP header is UTF-8"));
                }
            }
        }
    }
}
