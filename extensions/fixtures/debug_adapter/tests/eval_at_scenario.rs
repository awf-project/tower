use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};

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

fn write_frame(mut writer: impl Write, value: &Value) {
    let body = serde_json::to_vec(value).expect("serialize DAP frame");
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).expect("write DAP header");
    writer.write_all(&body).expect("write DAP body");
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
