//! `lsp` sidecar extension (spec 27).
//!
//! Absorbs `adapters/lsp/*` logic into an out-of-process sidecar. Exposes the
//! LSP tools (`tower_lsp_diagnostics`, `tower_lsp_definition`,
//! `tower_lsp_references`, `tower_lsp_hover`, `tower_lsp_implementations`,
//! `tower_lsp_rename`) over the extension protocol and
//! subscribes to `event/fileChanged` and `event/fileDeleted` for document sync.
//!
//! # Push path (spec 27 O1)
//!
//! When diagnostics arrive from a language server (`publishDiagnostics`), the
//! sidecar sends a `notify/resourceUpdated` HostCall to the host, which re-emits
//! it as MCP `notifications/resources/updated` to subscribed clients. The pull
//! tool (`tower_lsp_diagnostics`) remains available regardless.
//!
//! # Concurrency model
//!
//! The extension uses **two threads**:
//!
//! 1. The main loop reads JSON-RPC frames from stdin and handles host requests
//!    (`initialize`, `invokeTool`, `deliverEvent`, `shutdown`).
//! 2. The push thread watches the mpsc channel from `SessionPool` and writes
//!    `notify/resourceUpdated` HostCalls to stdout when diagnostics are published.
//!
//! Both threads share a `Mutex<Stdout>` so their writes do not interleave.
//!
//! # Protocol hazard mitigation (spec 27 known hazard)
//!
//! The push thread only sends HostCall *requests* (not responses). The main
//! loop's `host_call` helper waits for a response by reading the next frame
//! whose `id` matches. If a push-thread HostCall response arrives while the
//! main loop is blocked in `host_call`, the push thread handles it correctly
//! because the push HostCall `id` is well outside the main-loop's id space
//! (push ids start at 20_000; main-loop ids start at 10_000).
//!
//! To avoid the "discarded request" deadlock described in the KNOWN PROTOCOL
//! HAZARDS, the main loop's `read_response` loops until it sees its expected `id`
//! and **queues** any unmatched frames (whether push HostCall responses or host
//! requests). Unmatched frames that look like host requests are re-processed after
//! the pending host call returns. This is safe because the main loop is the only
//! reader of stdin.

#![forbid(unsafe_code)]

mod config;
mod lsp_adapter;
mod protocol;
mod push;
mod session;
mod tools;

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use extension_protocol::{
    Capability, InitParams, InitResult, PROTOCOL_VERSION, Response, ToolDecl,
};
use protocol::{HostCallIdAllocator, QueuedFrame};
use serde_json::Value;

use session::LspSessionPool;

fn main() {
    // Wrap Stdout in a Mutex so it is Send + 'static and can be shared with the
    // push forwarder thread without holding a lifetime-bounded lock.
    let stdout_lock: Arc<Mutex<io::Stdout>> = Arc::new(Mutex::new(io::stdout()));

    let mut lines = BufReader::new(io::stdin()).lines();

    // Incremental host-call id counter — start well away from any host request ids.
    let mut host_call_ids = HostCallIdAllocator::new(10_000);

    // Pending frames that arrived while waiting for a HostCall response.
    let mut deferred: VecDeque<QueuedFrame> = VecDeque::new();

    // Session pool — populated after initialize.
    let mut pool: Option<LspSessionPool> = None;

    // Workspace root — set at initialize time from config.
    let mut workspace_root: PathBuf = PathBuf::from(".");

    loop {
        // Process any deferred frames before reading a new one.
        let frame_opt = if let Some(deferred_frame) = deferred.pop_front() {
            Some(deferred_frame)
        } else {
            // Read from stdin.
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
                    // arrive during idle spins — these are host acknowledgements of
                    // fire-and-forget push HostCalls (e.g. notify/resourceUpdated).
                    // Acting on them (sending a spurious error reply) would corrupt
                    // the host's pending-slot condvar and cause false ProtocolErrors.
                    let has_method = envelope.get("method").is_some();
                    let is_response = !has_method
                        && (envelope.get("result").is_some() || envelope.get("error").is_some());
                    if is_response {
                        continue;
                    }
                    protocol::frame_from_envelope(envelope)
                }
                Some(Err(_)) | None => break,
            }
        };

        let Some(frame) = frame_opt else {
            break;
        };

        let QueuedFrame::Request {
            id,
            method,
            params: params_val,
        } = frame
        else {
            continue;
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

                // Read workspace root from environment or default to CWD.
                workspace_root = std::env::var("TOWER_WORKSPACE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    });

                // Load LSP config from .tower/config.toml.
                let lsp_config = config::load_lsp_config(&workspace_root);

                // Build the push channel: LSP adapter sends DiagnosticsEvent → we
                // forward as `notify/resourceUpdated` HostCall.
                let (push_tx, push_rx) =
                    std::sync::mpsc::channel::<crate::lsp_adapter::DiagnosticsEvent>();

                // Spawn the push forwarder thread.
                let stdout_clone = Arc::clone(&stdout_lock);
                std::thread::spawn(move || {
                    push::run_push_forwarder(push_rx, stdout_clone);
                });

                // Build the session pool with push wired in.
                pool = Some(LspSessionPool::new(
                    lsp_config,
                    workspace_root.clone(),
                    Some(push_tx),
                ));

                let result = InitResult {
                    tools: vec![
                        ToolDecl {
                            name: "diagnostics".to_owned(),
                            description: "Run the configured language server over a file's current content and return compiler/linter diagnostics (errors and warnings). Use after editing a file to verify the change did not break it.".to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path of the file to analyze"}},"required":["path"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "definition".to_owned(),
                            description: "Go to the definition of the symbol at the given position in a workspace-relative source file.".to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path"},"line":{"type":"integer","description":"0-based line"},"character":{"type":"integer","description":"0-based UTF-16 character offset"}},"required":["path","line","character"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "references".to_owned(),
                            description: "Find all references to the symbol at the given position in a workspace-relative source file.".to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path"},"line":{"type":"integer","description":"0-based line"},"character":{"type":"integer","description":"0-based UTF-16 character offset"}},"required":["path","line","character"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "hover".to_owned(),
                            description: "Return hover information for the symbol at the given position in a workspace-relative source file.".to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path"},"line":{"type":"integer","description":"0-based line"},"character":{"type":"integer","description":"0-based UTF-16 character offset"}},"required":["path","line","character"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "implementations".to_owned(),
                            description: "Find implementations for the symbol at the given position in a workspace-relative source file.".to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path"},"line":{"type":"integer","description":"0-based line"},"character":{"type":"integer","description":"0-based UTF-16 character offset"}},"required":["path","line","character"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "rename".to_owned(),
                            description: "Rename the symbol at the given position using the configured language server and workspace apply-edits host capability.".to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path"},"line":{"type":"integer","description":"0-based line"},"character":{"type":"integer","description":"0-based UTF-16 character offset"},"new_name":{"type":"string","description":"New symbol name"},"dry_run":{"type":"boolean","description":"Return host preview data without mutating files"}},"required":["path","line","character","new_name"]}"#.to_owned(),
                        },
                    ],
                    events: vec![
                        "event/fileChanged".to_owned(),
                        "event/fileDeleted".to_owned(),
                    ],
                    capabilities: vec![
                        Capability::ReadFile,
                        Capability::Notify,
                        Capability::Log,
                        Capability::RequestApplyEdits,
                    ],
                };

                let resp = Response::Initialized(result);
                protocol::send_response(&stdout_lock, &id, &resp);
            }

            "invokeTool" => {
                let tool_name = params_val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let tool_params = params_val.get("params").cloned().unwrap_or(Value::Null);

                let result = if let Some(ref mut p) = pool {
                    tools::dispatch(
                        &tool_name,
                        tool_params,
                        p,
                        &workspace_root,
                        &stdout_lock,
                        &mut lines,
                        &mut host_call_ids,
                        &mut deferred,
                    )
                } else {
                    Err("LSP extension not initialized".to_owned())
                };

                match result {
                    Ok(value) => {
                        protocol::send_response(&stdout_lock, &id, &Response::ToolResult(value));
                    }
                    Err(err_msg) => {
                        protocol::send_error(&stdout_lock, &id, -32000, &err_msg);
                    }
                }
            }

            "deliverEvent" => {
                // The host delivers fileChanged or fileDeleted events.
                if let Some(ref mut p) = pool {
                    handle_event(params_val, p, &workspace_root);
                }
                protocol::send_response(&stdout_lock, &id, &Response::Ack);
            }

            "shutdown" => {
                if let Some(mut p) = pool.take() {
                    p.shutdown_all();
                }
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

/// Handle a `deliverEvent` frame: extract path and dispatch document sync.
fn handle_event(params: Value, pool: &mut LspSessionPool, workspace_root: &std::path::Path) {
    // Shape 1: adjacently-tagged Request::DeliverEvent wrapper.
    let event_data = if let Some(data) = params.get("data") {
        data.clone()
    } else {
        params
    };

    let event_type = event_data
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = event_data
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if path.is_empty() {
        return;
    }

    match event_type {
        "FileChanged" => {
            // Update the document in any session that handles this file's extension.
            pool.on_file_changed(path, workspace_root);
        }
        "FileDeleted" => {
            // Close the document in any session that handles this file's extension.
            pool.on_file_deleted(path, workspace_root);
        }
        _ => {}
    }
}
