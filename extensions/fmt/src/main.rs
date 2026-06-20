//! `fmt` sidecar extension.
//!
//! Ports the removed WASM `fmt` plugin to the native extension model.
//!
//! # Responsibilities
//!
//! 1. **Change-driven formatting** (`event/fileChanged`): when a file changes,
//!    skip paths containing `.tmp_write` (shadow-file artifacts), then forward
//!    all other paths to the host via `workspace/requestFormat`. Treats both
//!    `"accepted"` and `"dropped"` results as success — the queue is best-effort
//!    and the extension does not error the event on a full queue.
//!
//! 2. **On-demand `format` tool**: with `{"path":"x"}` enqueues exactly that path;
//!    with `{}` calls `workspace/listFiles` and enqueues every file returned.
//!    Returns `{"requested":N,"accepted":A,"dropped":D}` promptly after enqueuing
//!    (does not wait for formatting to complete).
//!
//! # Protocol hazard mitigation
//!
//! This extension makes host-calls (`requestFormat`, `listFiles`) while the host
//! may concurrently deliver other events or requests on stdin. The
//! `read_host_response` helper uses a `VecDeque` deferred queue: any non-matching
//! inbound frames (host requests) are queued for the main loop to process after
//! the pending host call returns. This avoids the "discarded-request" deadlock
//! described in the KNOWN PROTOCOL HAZARDS (spec 27).

#![forbid(unsafe_code)]

mod protocol;

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

use extension_protocol::{
    Capability, HostCall, InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response,
    ToolDecl,
};
use serde_json::Value;

fn main() {
    // Wrap Stdout in a Mutex so it is Send + 'static if we ever need to share
    // it. For this extension there is no push thread, but the pattern matches
    // the LSP extension for consistency and future-proofing.
    let stdout_lock: Arc<Mutex<io::Stdout>> = Arc::new(Mutex::new(io::stdout()));

    let mut lines = BufReader::new(io::stdin()).lines();

    // Host-call id counter — start away from any host request ids.
    let mut next_hcall_id: u64 = 10_000;

    // Pending frames that arrived while waiting for a HostCall response.
    let mut deferred: VecDeque<(Option<Value>, String, Value)> = VecDeque::new();

    loop {
        // Drain deferred queue before reading a new frame from stdin.
        let frame_opt = if let Some(deferred_frame) = deferred.pop_front() {
            Some(deferred_frame)
        } else {
            match lines.next() {
                Some(Ok(line)) => {
                    let line = line.trim().to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    let envelope: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // Discard response frames (result/error without method) that
                    // arrive during idle — these are stale host ACKs.
                    let has_method = envelope.get("method").is_some();
                    let is_response = !has_method
                        && (envelope.get("result").is_some() || envelope.get("error").is_some());
                    if is_response {
                        continue;
                    }
                    let id = envelope.get("id").cloned();
                    let method = envelope
                        .get("method")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let params = envelope.get("params").cloned().unwrap_or(Value::Null);
                    Some((id, method, params))
                }
                Some(Err(_)) | None => break,
            }
        };

        let Some((id, method, params_val)) = frame_opt else {
            break;
        };

        match method.as_str() {
            "initialize" => {
                let init_params: InitParams = match serde_json::from_value(params_val) {
                    Ok(p) => p,
                    Err(e) => {
                        protocol::send_error(
                            &stdout_lock,
                            &id,
                            -32602,
                            &format!("bad InitParams: {e}"),
                        );
                        continue;
                    }
                };

                if init_params.protocol_version != PROTOCOL_VERSION {
                    protocol::send_error(
                        &stdout_lock,
                        &id,
                        -32600,
                        &format!(
                            "protocol version mismatch: host={} extension={}",
                            init_params.protocol_version, PROTOCOL_VERSION
                        ),
                    );
                    continue;
                }

                // No init-time host-calls needed for the fmt extension.
                // Send Initialized only after any init-time host-calls (ordering rule).
                let result = InitResult {
                    tools: vec![ToolDecl {
                        name: "format".to_owned(),
                        description: "Enqueue format jobs for one file or the entire workspace. \
                            Pass {\"path\":\"<ws-relative>\"} to format a single file, \
                            or {} to format all indexed files. \
                            Returns {\"requested\":N,\"accepted\":A,\"dropped\":D} immediately \
                            after enqueuing — does not wait for formatting to complete."
                            .to_owned(),
                        schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path of the file to format (omit to format all indexed files)"}},"additionalProperties":false}"#.to_owned(),
                    }],
                    events: vec!["event/fileChanged".to_owned()],
                    capabilities: vec![
                        Capability::RequestFormat,
                        Capability::ListFiles,
                        Capability::Log,
                    ],
                };

                protocol::send_response(&stdout_lock, &id, &Response::Initialized(result));
            }

            "invokeTool" => {
                let tool_name = params_val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let tool_params = params_val.get("params").cloned().unwrap_or(Value::Null);

                match tool_name.as_str() {
                    "format" => {
                        let result = handle_format_tool(
                            tool_params,
                            &stdout_lock,
                            &mut lines,
                            &mut next_hcall_id,
                            &mut deferred,
                        );
                        match result {
                            Ok(value) => {
                                protocol::send_response(
                                    &stdout_lock,
                                    &id,
                                    &Response::ToolResult(value),
                                );
                            }
                            Err(err_msg) => {
                                protocol::send_response(
                                    &stdout_lock,
                                    &id,
                                    &Response::Error(ProtocolError {
                                        code: -32000,
                                        message: err_msg,
                                        data: None,
                                    }),
                                );
                            }
                        }
                    }
                    other => {
                        protocol::send_response(
                            &stdout_lock,
                            &id,
                            &Response::Error(ProtocolError {
                                code: -32601,
                                message: format!("unknown tool: {other}"),
                                data: None,
                            }),
                        );
                    }
                }
            }

            "deliverEvent" => {
                // Extract the event data from the adjacently-tagged envelope.
                let event_data = params_val
                    .get("data")
                    .cloned()
                    .unwrap_or_else(|| params_val.clone());

                let event_type = event_data
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let path = event_data
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();

                if event_type == "FileChanged" && !path.is_empty() {
                    // Skip shadow-file artifacts — the only guest-side filter.
                    if !path.contains(".tmp_write") {
                        let call = HostCall::RequestFormat { path };
                        // Treat accepted and dropped both as success — best-effort queue.
                        let _ = protocol::host_call(
                            &stdout_lock,
                            &mut lines,
                            &mut next_hcall_id,
                            "workspace/requestFormat",
                            &call,
                            &mut deferred,
                        );
                    }
                }
                // Always ack the event regardless of the requestFormat result.
                protocol::send_response(&stdout_lock, &id, &Response::Ack);
            }

            "shutdown" => {
                protocol::send_response(&stdout_lock, &id, &Response::Ack);
                if let Ok(mut out) = stdout_lock.lock() {
                    let _ = out.flush();
                }
                break;
            }

            other => {
                protocol::send_error(
                    &stdout_lock,
                    &id,
                    -32601,
                    &format!("unknown method: {other}"),
                );
            }
        }
    }
}

/// Handle the `format` tool invocation.
///
/// - `{"path":"x"}` → enqueue exactly one path.
/// - `{}` (no path) → `workspace/listFiles` then enqueue each returned path.
///
/// Returns `{"requested":N,"accepted":A,"dropped":D}`.
fn handle_format_tool<R>(
    params: Value,
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    next_id: &mut u64,
    deferred: &mut VecDeque<(Option<Value>, String, Value)>,
) -> Result<Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path_opt = params
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let paths: Vec<String> = if let Some(p) = path_opt {
        // Single-file mode.
        vec![p]
    } else {
        // Format-all mode: enumerate workspace files.
        let resp = protocol::host_call(
            out,
            lines,
            next_id,
            "workspace/listFiles",
            &HostCall::ListFiles,
            deferred,
        )?;
        // Response is a JSON array of path strings.
        match resp {
            Value::Array(arr) => arr
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => return Err(format!("unexpected listFiles response: {resp}")),
        }
    };

    let requested = paths.len() as u64;
    let mut accepted: u64 = 0;
    let mut dropped: u64 = 0;

    for path in paths {
        let call = HostCall::RequestFormat { path };
        match protocol::host_call(
            out,
            lines,
            next_id,
            "workspace/requestFormat",
            &call,
            deferred,
        ) {
            Ok(Value::String(s)) if s == "accepted" => accepted += 1,
            Ok(_) => dropped += 1,
            Err(_) => dropped += 1,
        }
    }

    Ok(serde_json::json!({
        "requested": requested,
        "accepted": accepted,
        "dropped": dropped,
    }))
}
