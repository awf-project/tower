//! JSON-RPC 2.0 wire helpers for the `fmt` extension.
//!
//! Mirrors `extensions/lsp/src/protocol.rs`: uses a shared `Mutex<Stdout>` for
//! thread-safe writes and a `VecDeque` deferred queue to avoid the
//! "discarded-request" deadlock (KNOWN PROTOCOL HAZARDS §1).

#![forbid(unsafe_code)]

use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex};

use extension_protocol::{HostCall, Response};

/// A response to a host capability call.
#[derive(Debug, PartialEq)]
pub enum HostCallError {
    Host {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },
    Transport(String),
    Parse(String),
    Closed,
    MalformedResponse,
}

impl fmt::Display for HostCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host { message, .. } => f.write_str(message),
            Self::Transport(message) => write!(f, "stdin error: {message}"),
            Self::Parse(message) => write!(f, "parse error: {message}"),
            Self::Closed => f.write_str("stdin closed while waiting for host response"),
            Self::MalformedResponse => f.write_str("malformed host response (no result or error)"),
        }
    }
}

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
/// Returns the numeric id used so the caller can match the response.
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
) -> Result<serde_json::Value, HostCallError>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    loop {
        let line = match lines.next() {
            Some(Ok(l)) => l,
            Some(Err(e)) => return Err(HostCallError::Transport(e.to_string())),
            None => return Err(HostCallError::Closed),
        };
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }

        let envelope: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => return Err(HostCallError::Parse(e.to_string())),
        };

        let frame_id = envelope.get("id");
        let expected_id_val = serde_json::json!(expected_id);

        // Check if this is the response we're waiting for.
        if frame_id == Some(&expected_id_val) {
            if let Some(result) = envelope.get("result") {
                return Ok(result.clone());
            }
            if let Some(error) = envelope.get("error") {
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
                    .to_owned();
                let code = error
                    .get("code")
                    .and_then(|v| v.as_i64())
                    .unwrap_or_default();
                let data = error.get("data").cloned();
                return Err(HostCallError::Host {
                    code,
                    message,
                    data,
                });
            }
            return Err(HostCallError::MalformedResponse);
        }

        // Not our response. Classify the frame:
        // - If it has result/error but no method, it is a response to a different
        //   outstanding HostCall (fire-and-forget). Discard silently.
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
) -> Result<serde_json::Value, HostCallError>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let id = send_host_call(out, next_id, method, call);
    read_host_response(lines, id, deferred)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;

    use super::{HostCallError, read_host_response};

    #[test]
    fn format_host_call_errors_preserve_validation_code_and_message() {
        let mut lines = vec![Ok::<_, io::Error>(
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"dependency locator is not a workspace path","data":{"kind":"validation_data_sentinel"}}}"#.to_owned(),
        )]
        .into_iter();
        let error = read_host_response(&mut lines, 7, &mut VecDeque::new())
            .expect_err("host validation error must remain an error");
        assert_eq!(
            error,
            HostCallError::Host {
                code: -32602,
                message: "dependency locator is not a workspace path".to_owned(),
                data: Some(serde_json::json!({"kind":"validation_data_sentinel"})),
            }
        );
    }

    #[test]
    fn format_host_call_transport_errors_remain_distinct() {
        let mut lines = vec![Err::<String, _>(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "transport sentinel",
        ))]
        .into_iter();
        let error = read_host_response(&mut lines, 8, &mut VecDeque::new())
            .expect_err("transport failure must remain an error");
        assert_eq!(
            error,
            HostCallError::Transport("transport sentinel".to_owned())
        );
    }
}
