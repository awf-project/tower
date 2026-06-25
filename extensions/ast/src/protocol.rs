//! JSON-RPC 2.0 wire helpers for the `ast` extension.
//!
//! Encapsulates framing, host-call dispatch, and response parsing so the
//! main loop and tool handlers stay focused on business logic.

use std::io::Write;

use extension_protocol::{HostCall, Response};

// ── Outbound helpers ──────────────────────────────────────────────────────────

/// Send a JSON-RPC success response to the host.
pub fn send_response(out: &mut impl Write, id: &Option<serde_json::Value>, resp: &Response) {
    let result = serde_json::to_value(resp).expect("serialize Response");
    let envelope = if let Some(id_val) = id {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": result,
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "result": result,
        })
    };
    let s = serde_json::to_string(&envelope).expect("serialize envelope");
    writeln!(out, "{s}").unwrap();
}

/// Send a JSON-RPC error response to the host.
pub fn send_error(out: &mut impl Write, id: &Option<serde_json::Value>, code: i32, msg: &str) {
    let envelope = if let Some(id_val) = id {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "error": {"code": code, "message": msg},
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "error": {"code": code, "message": msg},
        })
    };
    let s = serde_json::to_string(&envelope).expect("serialize error");
    writeln!(out, "{s}").unwrap();
}

/// Send a host capability call and wait for the response.
///
/// Returns `Ok(result_value)` or `Err(error_message)`.
pub fn host_call<R>(
    out: &mut impl Write,
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    next_id: &mut u64,
    method: &str,
    call: &HostCall,
) -> Result<R, String>
where
    R: serde::de::DeserializeOwned,
{
    let id = *next_id;
    *next_id += 1;
    let id_val = serde_json::json!(id);

    let params = serde_json::to_value(call).expect("serialize HostCall");
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_val,
        "method": method,
        "params": params,
    });
    let s = serde_json::to_string(&envelope).expect("serialize host call");
    writeln!(out, "{s}").unwrap();
    out.flush().map_err(|e| format!("flush error: {e}"))?;

    read_host_response(lines, &id_val)
}

pub fn host_call_value(
    out: &mut impl Write,
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    next_id: &mut u64,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = *next_id;
    *next_id += 1;
    let id_val = serde_json::json!(id);

    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_val,
        "method": method,
        "params": params,
    });
    let s = serde_json::to_string(&envelope).expect("serialize host call");
    writeln!(out, "{s}").unwrap();
    out.flush().map_err(|e| format!("flush error: {e}"))?;

    read_host_response(lines, &id_val)
}

/// Read a single host response frame matching `expected_id`.
fn read_host_response<R>(
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    expected_id: &serde_json::Value,
) -> Result<R, String>
where
    R: serde::de::DeserializeOwned,
{
    loop {
        let line = match lines.next() {
            Some(Ok(l)) => l,
            Some(Err(e)) => return Err(format!("stdin error: {e}")),
            None => return Err("stdin closed while waiting for host response".to_owned()),
        };
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }

        let envelope: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| format!("parse error: {e}"))?;

        if envelope.get("id") != Some(expected_id) {
            // Wrong id — skip (should not happen in the serial protocol).
            continue;
        }

        if let Some(result) = envelope.get("result") {
            return serde_json::from_value(result.clone())
                .map_err(|e| format!("deserialize result: {e}"));
        }

        if let Some(error) = envelope.get("error") {
            let msg = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(msg.to_owned());
        }

        return Err("malformed host response (no result or error)".to_owned());
    }
}

// ── Typed host-call helpers ───────────────────────────────────────────────────

/// Call `workspace/readFile` and return the raw bytes.
///
/// The host encodes file content as a UTF-8 string in the JSON response
/// (`Value::String`), so we deserialize as `String` and convert to bytes.
pub fn host_call_read_file(
    out: &mut impl Write,
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    next_id: &mut u64,
    path: &str,
) -> Result<Vec<u8>, String> {
    let call = HostCall::ReadFile {
        path: path.to_owned(),
    };
    host_call::<String>(out, lines, next_id, "workspace/readFile", &call).map(String::into_bytes)
}

/// Call `workspace/listFiles` and return the list of relative paths.
pub fn host_call_list_files(
    out: &mut impl Write,
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    next_id: &mut u64,
) -> Result<Vec<String>, String> {
    let call = HostCall::ListFiles;
    host_call::<Vec<String>>(out, lines, next_id, "workspace/listFiles", &call)
}

/// Call `index/get` and return the raw bytes.
pub fn host_call_index_get(
    out: &mut impl Write,
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    next_id: &mut u64,
    key: &str,
) -> Result<Vec<u8>, String> {
    let call = HostCall::IndexGet {
        key: key.to_owned(),
    };
    host_call::<Vec<u8>>(out, lines, next_id, "index/get", &call)
}

/// Call `index/put` to store bytes under a key.
pub fn host_call_index_put(
    out: &mut impl Write,
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    next_id: &mut u64,
    key: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let call = HostCall::IndexPut {
        key: key.to_owned(),
        bytes,
    };
    host_call::<serde_json::Value>(out, lines, next_id, "index/put", &call).map(|_| ())
}

/// Emit a log message through the host's logging subsystem.
///
/// Non-fatal: if the log call fails, the error is silently ignored.
pub fn host_call_log(
    out: &mut impl Write,
    lines: &mut impl Iterator<Item = Result<String, std::io::Error>>,
    next_id: &mut u64,
    level: &str,
    msg: &str,
) -> Result<(), String> {
    let call = HostCall::Log {
        level: level.to_owned(),
        msg: msg.to_owned(),
    };
    host_call::<serde_json::Value>(out, lines, next_id, "log", &call).map(|_| ())
}
