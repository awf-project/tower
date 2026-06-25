//! JSON-RPC 2.0 wire helpers for the `lsp` extension.
//!
//! Mirrors `extensions/ast/src/protocol.rs` but operates on a shared
//! `Mutex<Stdout>` so the push forwarder thread can also write frames without
//! interleaving.

#![forbid(unsafe_code)]

use std::io::Write;
use std::sync::{Arc, Mutex};

use extension_protocol::{HostCall, Response};
use extension_sidecar_harness::jsonrpc::HarnessError;
use serde_json::Value;

pub use extension_sidecar_harness::{HostCallIdAllocator, QueuedFrame, frame_from_envelope};

// ── Outbound helpers ──────────────────────────────────────────────────────────

/// Send a JSON-RPC success response to the host.
pub fn send_response(out: &Arc<Mutex<impl Write>>, id: &Option<Value>, resp: &Response) {
    let _ = extension_sidecar_harness::send_response(out, id, resp);
}

/// Send a JSON-RPC error response to the host.
pub fn send_error(out: &Arc<Mutex<impl Write>>, id: &Option<Value>, code: i32, msg: &str) {
    let _ = extension_sidecar_harness::send_error(out, id, code, msg);
}

/// Perform a typed host capability call and wait for the response.
///
/// `deferred` receives any non-matching frames encountered while waiting.
pub fn host_call<R>(
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    ids: &mut HostCallIdAllocator,
    method: &str,
    call: &HostCall,
    queued: &mut std::collections::VecDeque<QueuedFrame>,
) -> Result<Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    extension_sidecar_harness::host_call(out, lines, ids, method, call, queued)
        .map_err(harness_error_message)
}

pub fn host_call_value<R>(
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    ids: &mut HostCallIdAllocator,
    method: &str,
    params: Value,
    queued: &mut std::collections::VecDeque<QueuedFrame>,
) -> Result<Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let id = ids.next_id();
    extension_sidecar_harness::write_envelope(
        out,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    )
    .map_err(harness_error_message)?;
    extension_sidecar_harness::read_host_response(lines, id, queued).map_err(harness_error_message)
}

/// Send a `notify/resourceUpdated` HostCall from the push thread.
///
/// This variant takes a plain `impl Write` because the push thread owns the
/// already-locked guard. Returns the id sent.
pub fn send_notify_resource_updated(out: &mut impl Write, next_id: &mut u64, uri: &str) {
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "notify/resourceUpdated",
        "params": HostCall::NotifyResourceUpdated {
            uri: uri.to_owned(),
        },
    });
    if let Ok(line) = serde_json::to_string(&envelope) {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

fn harness_error_message(error: HarnessError) -> String {
    match error {
        HarnessError::HostError(value) => value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
        other => other.to_string(),
    }
}
