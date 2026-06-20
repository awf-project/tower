//! JSON-RPC 2.0 wire helpers for the `lsp` extension.
//!
//! Mirrors `extensions/ast/src/protocol.rs` but operates on a shared
//! `Mutex<Stdout>` so the push forwarder thread can also write frames without
//! interleaving.

#![forbid(unsafe_code)]

use std::io::Write;
use std::sync::{Arc, Mutex};

use extension_protocol::{HostCall, Response};

// ── Outbound helpers ──────────────────────────────────────────────────────────

/// Send a JSON-RPC success response to the host.
pub fn send_response(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<serde_json::Value>,
    resp: &Response,
) {
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
    if let Ok(mut guard) = out.lock() {
        let _ = writeln!(guard, "{s}");
        let _ = guard.flush();
    }
}

/// Send a JSON-RPC error response to the host.
pub fn send_error(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<serde_json::Value>,
    code: i32,
    msg: &str,
) {
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
    if let Ok(mut guard) = out.lock() {
        let _ = writeln!(guard, "{s}");
        let _ = guard.flush();
    }
}

/// Send a host capability call request frame.
///
/// Returns the numeric id used, so the caller can match the response.
pub fn send_host_call(
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    method: &str,
    call: &HostCall,
) -> u64 {
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
    if let Ok(mut guard) = out.lock() {
        let _ = writeln!(guard, "{s}");
        let _ = guard.flush();
    }
    id
}

/// Read a host response from a line iterator, returning `Ok(result_value)` or
/// `Err(error_message)`.
///
/// Frames that do not match `expected_id` are pushed onto `deferred` for later
/// processing by the main loop (avoids the "discarded request" deadlock hazard).
pub fn read_host_response<R>(
    lines: &mut R,
    expected_id: u64,
    deferred: &mut std::collections::VecDeque<(
        Option<serde_json::Value>,
        String,
        serde_json::Value,
    )>,
) -> Result<serde_json::Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
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

        let envelope: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => return Err(format!("parse error: {e}")),
        };

        let frame_id = envelope.get("id");
        let expected_id_val = serde_json::json!(expected_id);

        // Check if this is the response we're waiting for.
        if frame_id == Some(&expected_id_val) {
            if let Some(result) = envelope.get("result") {
                return Ok(result.clone());
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

        // Not our response. Classify the frame:
        // - If it has result/error but no method, it is a response to a
        //   different outstanding HostCall (e.g. a push-thread notify/resourceUpdated
        //   acknowledgement). These are fire-and-forget — discard silently.
        // - If it has a method, it is a host request that arrived while we were
        //   blocked; queue it so the main loop can process it after we return.
        let has_method = envelope.get("method").is_some();
        let is_response =
            !has_method && (envelope.get("result").is_some() || envelope.get("error").is_some());
        if is_response {
            continue;
        }
        let id = envelope.get("id").cloned();
        let method = envelope
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let params = envelope
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        deferred.push_back((id, method, params));
    }
}

/// Perform a typed host capability call and wait for the response.
///
/// `deferred` receives any non-matching frames encountered while waiting.
pub fn host_call<R>(
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    next_id: &mut u64,
    method: &str,
    call: &HostCall,
    deferred: &mut std::collections::VecDeque<(
        Option<serde_json::Value>,
        String,
        serde_json::Value,
    )>,
) -> Result<serde_json::Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let id = send_host_call(out, next_id, method, call);
    read_host_response(lines, id, deferred)
}

/// Call `log` capability — non-fatal if it fails.
#[allow(dead_code)]
pub fn host_call_log<R>(
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    next_id: &mut u64,
    level: &str,
    msg: &str,
    deferred: &mut std::collections::VecDeque<(
        Option<serde_json::Value>,
        String,
        serde_json::Value,
    )>,
) where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let call = HostCall::Log {
        level: level.to_owned(),
        msg: msg.to_owned(),
    };
    let _ = host_call(out, lines, next_id, "log", &call, deferred);
}

/// Send a `notify/resourceUpdated` HostCall from the push thread.
///
/// This variant takes a plain `impl Write` because the push thread owns the
/// already-locked guard. Returns the id sent.
pub fn send_notify_resource_updated(out: &mut impl Write, next_id: &mut u64, uri: &str) {
    let id = *next_id;
    *next_id += 1;
    let id_val = serde_json::json!(id);
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_val,
        "method": "notify/resourceUpdated",
        "params": { "uri": uri },
    });
    let s = serde_json::to_string(&envelope).expect("serialize notify/resourceUpdated");
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}
