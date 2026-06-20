//! Push forwarder thread (spec 27 O1).
//!
//! Watches the diagnostics channel from `SessionPool` and emits
//! `notify/resourceUpdated` HostCall frames to the host when diagnostics arrive.
//! The host re-emits them as MCP `notifications/resources/updated`.
//!
//! # Thread safety
//!
//! This module runs in a background thread. It shares `Mutex<Stdout>` with the
//! main loop so writes from both threads do not interleave.
//!
//! # Protocol hazard: push-thread HostCall responses
//!
//! When the push thread sends a `notify/resourceUpdated` HostCall, the host
//! replies with `{"result": true, "id": <id>}`. That response arrives on stdin,
//! where the main loop's `read_host_response` might be waiting for a different
//! id (e.g. a `workspace/readFile` response id=10000 while the push HostCall id
//! is 20000). The main loop's `read_host_response` in `protocol.rs` handles this
//! by queuing unmatched frames — so the push-thread response is queued and safely
//! ignored (it is not a host request and the push thread has already moved on).
//!
//! The push thread does NOT read stdin and does NOT need to process the response:
//! `notify/resourceUpdated` is fire-and-forget from the extension's perspective.

#![forbid(unsafe_code)]

use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::lsp_adapter::DiagnosticsEvent;

use crate::protocol;

/// Counter for push HostCall ids. Starts high to avoid collision with main-loop ids.
static PUSH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(20_000);

/// Run the push forwarder loop until the sender is dropped.
///
/// Receives `DiagnosticsEvent` values from the LSP adapter's push channel and
/// emits `notify/resourceUpdated` HostCall frames.
pub fn run_push_forwarder<W: Write + Send + 'static>(
    rx: std::sync::mpsc::Receiver<DiagnosticsEvent>,
    out: Arc<Mutex<W>>,
) {
    while let Ok(event) = rx.recv() {
        // Build the LSP diagnostics URI from the event URI.
        // The event URI is typically `file:///abs/path/file.rs`; the push
        // notification uses the same URI so MCP clients can correlate it.
        let uri = &event.uri;

        let id = PUSH_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut next_id = id;

        if let Ok(mut guard) = out.lock() {
            protocol::send_notify_resource_updated(&mut *guard, &mut next_id, uri);
        }
        // The response from the host (if any) arrives on stdin. The push thread
        // does NOT read stdin — the main loop's `read_host_response` will
        // encounter it as an unmatched frame and queue/discard it safely.
    }
    // rx dropped — SessionPool gone; push thread exits cleanly.
}
