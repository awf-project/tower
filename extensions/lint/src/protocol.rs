#![forbid(unsafe_code)]
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use extension_protocol::{HostCall, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CheckRequest {
    pub path: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CheckResult {
    pub supported: bool,
    pub diagnostics: Vec<LintDiagnosticDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<LintToolErrorResponse>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LintToolErrorResponse {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LintDiagnosticDto {
    pub path: String,
    pub line: u32,
    pub character: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    #[serde(rename = "endCharacter")]
    pub end_character: u32,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum QueuedFrame {
    Request {
        id: Option<Value>,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

pub fn send_response(out: &Arc<Mutex<impl Write>>, id: &Option<Value>, response: &Response) {
    let result = serde_json::to_value(response).expect("serialize Response");
    let envelope = if let Some(id) = id {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "result": result,
        })
    };
    write_envelope(out, envelope);
}

pub fn send_error(out: &Arc<Mutex<impl Write>>, id: &Option<Value>, code: i32, message: &str) {
    let envelope = if let Some(id) = id {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "error": { "code": code, "message": message },
        })
    };
    write_envelope(out, envelope);
}

pub fn host_call<R>(
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    next_id: &mut u64,
    method: &str,
    call: &HostCall,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let id = *next_id;
    *next_id += 1;

    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": serde_json::to_value(call).expect("serialize HostCall"),
    });
    write_envelope(out, envelope);
    read_host_response(lines, id, queued)
}

pub fn frame_from_envelope(envelope: Value) -> Option<QueuedFrame> {
    let has_method = envelope.get("method").is_some();
    let is_response =
        !has_method && (envelope.get("result").is_some() || envelope.get("error").is_some());
    if is_response {
        return None;
    }

    let method = envelope
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = envelope.get("params").cloned().unwrap_or(Value::Null);
    let id = envelope.get("id").cloned();

    if id.is_some() {
        Some(QueuedFrame::Request { id, method, params })
    } else {
        Some(QueuedFrame::Notification { method, params })
    }
}

fn read_host_response<R>(
    lines: &mut R,
    expected_id: u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let expected = serde_json::json!(expected_id);

    loop {
        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(error)) => return Err(format!("stdin error: {error}")),
            None => return Err("stdin closed while waiting for host response".to_owned()),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let envelope: Value =
            serde_json::from_str(line).map_err(|error| format!("parse error: {error}"))?;
        if envelope.get("id") == Some(&expected) {
            if let Some(result) = envelope.get("result") {
                return Ok(result.clone());
            }
            if let Some(error) = envelope.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown host error");
                return Err(message.to_owned());
            }
            return Err("malformed host response".to_owned());
        }

        if let Some(frame) = frame_from_envelope(envelope) {
            queued.push_back(frame);
        }
    }
}

fn write_envelope(out: &Arc<Mutex<impl Write>>, envelope: Value) {
    let line = serde_json::to_string(&envelope).expect("serialize envelope");
    if let Ok(mut out) = out.lock() {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}
