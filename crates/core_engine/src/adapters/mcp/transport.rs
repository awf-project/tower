//! JSON-RPC 2.0 over stdio — the MCP transport loop (spec 10a).
//!
//! # Framing
//!
//! Newline-delimited: each message is one JSON object followed by `\n`. The
//! reader calls `BufRead::lines()` so no partial-read buffering is needed.
//!
//! # Testability
//!
//! `serve` is generic over `R: BufRead` and `W: Write`. Tests drive it with
//! in-memory `Cursor` / `Vec<u8>` buffers; the production entry passes real
//! `stdin()`/`stdout()`. No `stdin`/`stdout` references inside this function.
//!
//! # Loop resilience (spec UN2/AC5)
//!
//! A malformed JSON line returns a `ParseError` response and the loop continues
//! reading the next line. Only an end-of-stream (EOF) or unrecoverable I/O
//! error terminates the loop.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{
    registry::ToolRegistry,
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId, ToolError},
};
use crate::adapters::lsp::pool::DiagnosticsReader;
use crate::adapters::mcp::lsp_tools::SubscriptionRegistry;

// ── Push types ────────────────────────────────────────────────────────────────

/// A server-initiated diagnostics push event, bridged from the LSP layer via
/// `mpsc`. Carries only the URI and generation — no MCP-specific types, so this
/// struct lives cleanly in the transport without importing LSP internals.
pub struct PushEvent {
    pub uri: String,
    pub generation: u64,
}

/// Unified serve-loop message: a stdin line or a push event.
///
/// Decision 3: one blocking `mpsc` channel replaces the `try_recv + sleep(1ms)`
/// poll. A stdin-reader thread sends `Stdin` lines; the push forwarder sends
/// `Push` events. The serve loop does a blocking `recv()` — no polling, no
/// latency floor. `Disconnected` → clean shutdown.
enum ServeInput {
    Stdin(String),
    Push(PushEvent),
}

// ── MCP capability shape (spec EV1/AC1) ──────────────────────────────────────

/// Server name advertised in the `initialize` response.
const SERVER_NAME: &str = "tower";
/// Server version advertised in the `initialize` response.
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the JSON-RPC 2.0 read/dispatch loop until EOF or an I/O error.
///
/// # Parameters
///
/// - `reader`   — source of newline-delimited JSON frames (stdin in production).
/// - `writer`   — sink for JSON responses (stdout in production).
/// - `registry` — tool registry consulted for `tools/list` and `tools/call`.
///
/// # Errors
///
/// Returns `Err` only if an unrecoverable I/O error occurs on `writer`.
/// Malformed frames and unknown tools are handled gracefully (JSON-RPC errors
/// are written to `writer` and the loop continues).
///
/// # Examples
///
/// ```rust,no_run
/// use std::io::{BufReader, BufWriter};
/// use core_engine::adapters::mcp::{serve, ToolRegistry, ToolDesc, ToolError};
/// use serde_json::Value;
///
/// struct EmptyRegistry;
/// impl ToolRegistry for EmptyRegistry {
///     fn list(&self) -> Vec<ToolDesc> { vec![] }
///     fn call(&mut self, name: &str, _: Value) -> Result<Value, ToolError> {
///         Err(ToolError::NotFound(name.to_owned()))
///     }
/// }
///
/// let stdin = std::io::stdin();
/// let stdout = std::io::stdout();
/// serve(
///     BufReader::new(stdin.lock()),
///     BufWriter::new(stdout.lock()),
///     &mut EmptyRegistry,
/// ).expect("serve failed");
/// ```
pub fn serve<R, W>(
    reader: R,
    mut writer: W,
    registry: &mut dyn ToolRegistry,
) -> Result<(), std::io::Error>
where
    R: BufRead,
    W: Write,
{
    for line_result in reader.lines() {
        // Treat invalid UTF-8 as a malformed frame (UN2/AC5): write a parse
        // error and keep the loop alive. Only true I/O errors propagate out.
        let line = match line_result {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                write_error(&mut writer, JsonRpcError::parse_error(None))?;
                continue;
            }
            Err(e) => return Err(e),
        };

        // Skip empty lines silently (common after a well-formed message).
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<JsonRpcRequest>(&line) {
            Err(_) => {
                // Malformed JSON or missing required fields — return parse error
                // and keep reading (AC5).
                // JSON-RPC 2.0 §5.1: use the id from the frame when it is
                // detectable, so clients can correlate the error with their
                // in-flight request (Finding 3).
                let id = try_extract_id(&line);
                write_error(&mut writer, JsonRpcError::parse_error(id))?;
            }
            Ok(req) => {
                let id = req.id.clone();
                let response = dispatch(req, registry);
                match response {
                    DispatchResult::Ok(r) => write_response(&mut writer, r)?,
                    DispatchResult::Err(e) => write_error(&mut writer, e)?,
                    DispatchResult::Notification => {}
                }
                let _ = id; // id is already moved into the response
            }
        }
    }

    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

enum DispatchResult {
    Ok(JsonRpcResponse),
    Err(JsonRpcError),
    /// Notifications do not need a response.
    Notification,
}

fn dispatch(req: JsonRpcRequest, registry: &mut dyn ToolRegistry) -> DispatchResult {
    let id = req.id.clone();

    // JSON-RPC 2.0 §4: a Request without an `id` member is a Notification.
    // The server MUST NOT reply to any notification, regardless of method name.
    // This covers all current and future notification methods without an allow-list.
    if id.is_none() {
        return DispatchResult::Notification;
    }

    // JSON-RPC 2.0 §4: the `jsonrpc` member MUST be exactly "2.0".
    // Reject version-mismatched frames with -32600 InvalidRequest (Finding 4).
    if req.jsonrpc != "2.0" {
        return DispatchResult::Err(JsonRpcError::invalid_request(id, "jsonrpc must be \"2.0\""));
    }

    match req.method.as_str() {
        "initialize" => handle_initialize(id),
        "tools/list" => handle_tools_list(id, registry),
        "tools/call" => handle_tools_call(id, req.params, registry),
        _ => DispatchResult::Err(JsonRpcError::method_not_found(id, &req.method)),
    }
}

/// Handle the `initialize` handshake (spec EV1/AC1).
///
/// Responds with server info and capability advertisement. We advertise:
/// - `tools`: listing and calling tools.
/// - `resources`: subscribable resource endpoints (`subscribe: true`).
///   `listChanged: false` — the resource list is static (one entry per
///   configured language) and never changes at runtime.
fn handle_initialize(id: Option<RequestId>) -> DispatchResult {
    let result = json!({
        "protocolVersion": "2024-11-05",
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
        },
        "capabilities": {
            "tools": {},
            "resources": { "subscribe": true, "listChanged": false }
        }
    });
    DispatchResult::Ok(JsonRpcResponse::ok(id, result))
}

/// Handle `tools/list` — delegate entirely to the registry (spec EV2/AC2).
fn handle_tools_list(id: Option<RequestId>, registry: &dyn ToolRegistry) -> DispatchResult {
    let tools = registry.list();
    // Serialise to a JSON array. `unwrap` is safe: `ToolDesc` derives Serialize
    // with only primitive / JSON-native field types.
    let tools_json = serde_json::to_value(tools).unwrap_or(Value::Array(vec![]));
    DispatchResult::Ok(JsonRpcResponse::ok(id, json!({ "tools": tools_json })))
}

/// Handle `tools/call` — look up the named tool and dispatch (spec EV3/AC3/AC4).
fn handle_tools_call(
    id: Option<RequestId>,
    params: Value,
    registry: &mut dyn ToolRegistry,
) -> DispatchResult {
    // Extract `name` and `arguments` from the params object.
    let tool_name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n.to_owned(),
        None => {
            return DispatchResult::Err(JsonRpcError::invalid_params(
                id,
                "missing required field 'name'",
            ));
        }
    };

    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    match registry.call(&tool_name, args) {
        Ok(result) => DispatchResult::Ok(JsonRpcResponse::ok(
            id,
            json!({ "content": [{ "type": "text", "text": result.to_string() }] }),
        )),
        Err(ToolError::NotFound(name)) => {
            DispatchResult::Err(JsonRpcError::tool_not_found(id, &name))
        }
        Err(ToolError::InvalidArgs(msg)) => {
            DispatchResult::Err(JsonRpcError::invalid_params(id, &msg))
        }
        Err(ToolError::ExecutionFailed(msg)) => {
            DispatchResult::Err(JsonRpcError::internal_error(id, &msg))
        }
        Err(ToolError::ResourceNotFound(msg)) => {
            DispatchResult::Err(JsonRpcError::resource_not_found(id, &msg))
        }
    }
}

// ── Resource handlers ─────────────────────────────────────────────────────────

/// Handle `resources/list` — one static entry per configured language.
///
/// URIs come from `SessionPool::resource_uris()`, computed at construction time.
/// No pool lock is needed here; the list never changes at runtime.
fn handle_resources_list(id: Option<RequestId>, resource_uris: &[String]) -> DispatchResult {
    let resources: Vec<Value> = resource_uris
        .iter()
        .map(|uri| {
            json!({
                "uri": uri,
                "name": uri,
                "mimeType": "application/json"
            })
        })
        .collect();
    DispatchResult::Ok(JsonRpcResponse::ok(id, json!({ "resources": resources })))
}

/// Handle `resources/read` — fully wired via `DiagnosticsReader` (Decision 4).
///
/// Returns the last published diagnostics from `SharedState` without re-running
/// `check`. Never spawns; never blocks. Always returns `supported: true`; an
/// unconfigured extension simply yields an empty `diagnostics` list.
fn handle_resources_read(
    id: Option<RequestId>,
    params: Value,
    diag_reader: &Arc<dyn DiagnosticsReader>,
) -> DispatchResult {
    let uri = match params.get("uri").and_then(Value::as_str) {
        Some(u) => u.to_owned(),
        None => {
            return DispatchResult::Err(JsonRpcError::invalid_params(id, "missing uri"));
        }
    };
    let diags = diag_reader.diagnostics_for(&uri);
    // Diagnostic does not derive Serialize; use the same manual serialiser as
    // lsp_tools::diagnostic_to_json so the JSON shapes are identical (AC6).
    let diags_json: Vec<Value> = diags
        .iter()
        .map(crate::adapters::mcp::lsp_tools::diagnostic_to_json)
        .collect();
    DispatchResult::Ok(JsonRpcResponse::ok(
        id,
        json!({
            "supported": true,
            "uri": uri,
            "diagnostics": diags_json
        }),
    ))
}

/// Handle `resources/subscribe` — record the URI in the subscription registry.
fn handle_resources_subscribe(
    id: Option<RequestId>,
    params: Value,
    sub_reg: &Arc<Mutex<SubscriptionRegistry>>,
) -> DispatchResult {
    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
        sub_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribe(uri);
    }
    DispatchResult::Ok(JsonRpcResponse::ok(id, Value::Null))
}

/// Handle `resources/unsubscribe` — remove the URI from the subscription registry.
fn handle_resources_unsubscribe(
    id: Option<RequestId>,
    params: Value,
    sub_reg: &Arc<Mutex<SubscriptionRegistry>>,
) -> DispatchResult {
    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
        sub_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unsubscribe(uri);
    }
    DispatchResult::Ok(JsonRpcResponse::ok(id, Value::Null))
}

/// Dispatch a request handling both tool and resource methods.
fn dispatch_with_resources(
    req: JsonRpcRequest,
    registry: &mut dyn ToolRegistry,
    sub_reg: &Arc<Mutex<SubscriptionRegistry>>,
    diag_reader: &Arc<dyn DiagnosticsReader>,
    resource_uris: &[String],
) -> DispatchResult {
    let id = req.id.clone();

    if id.is_none() {
        return DispatchResult::Notification;
    }

    if req.jsonrpc != "2.0" {
        return DispatchResult::Err(JsonRpcError::invalid_request(id, "jsonrpc must be \"2.0\""));
    }

    match req.method.as_str() {
        "initialize" => handle_initialize(id),
        "tools/list" => handle_tools_list(id, registry),
        "tools/call" => handle_tools_call(id, req.params, registry),
        "resources/list" => handle_resources_list(id, resource_uris),
        "resources/read" => handle_resources_read(id, req.params, diag_reader),
        "resources/subscribe" => handle_resources_subscribe(id, req.params, sub_reg),
        "resources/unsubscribe" => handle_resources_unsubscribe(id, req.params, sub_reg),
        _ => DispatchResult::Err(JsonRpcError::method_not_found(id, &req.method)),
    }
}

/// Serve MCP requests from `reader` and emit push notifications from `push_rx`.
///
/// # Architecture (Decision 3 — unified blocking channel)
///
/// A dedicated stdin-reader thread sends `ServeInput::Stdin(line)` lines.
/// The push forwarder (in `main.rs`) sends `ServeInput::Push(event)` via
/// `push_rx`. The serve loop does a blocking `recv()` on a single `mpsc`
/// channel — no `try_recv`, no `sleep`, no latency floor.
/// `Disconnected` → clean shutdown.
///
/// # resources/read (Decision 4)
///
/// Fully wired via `diag_reader`: returns the last published diagnostics
/// from `SharedState` without re-running `check`. No "supported:false" stub.
///
/// # Parameters
///
/// - `push_rx`: when `None`, push events are never sent (stdin-only mode).
/// - `resource_uris`: static list from `SessionPool::resource_uris()`.
///
/// # Errors
///
/// Returns `Err` only on an unrecoverable write I/O error.
pub fn serve_with_push<R, W>(
    reader: R,
    mut writer: W,
    registry: &mut dyn ToolRegistry,
    sub_reg: Arc<Mutex<SubscriptionRegistry>>,
    diag_reader: Arc<dyn DiagnosticsReader>,
    resource_uris: Vec<String>,
    push_rx: Option<std::sync::mpsc::Receiver<PushEvent>>,
) -> Result<(), std::io::Error>
where
    R: std::io::Read + Send + 'static,
    W: Write,
{
    use std::sync::mpsc;

    // Unified channel: carries both stdin lines and push events.
    let (serve_tx, serve_rx) = mpsc::channel::<ServeInput>();

    // Stdin-reader thread: wraps `reader` in a BufReader and sends each line.
    let stdin_tx = serve_tx.clone();
    std::thread::spawn(move || {
        let buf = std::io::BufReader::new(reader);
        for line_result in buf.lines() {
            match line_result {
                Ok(line) => {
                    if stdin_tx.send(ServeInput::Stdin(line)).is_err() {
                        break; // Serve loop exited.
                    }
                }
                Err(_) => break, // EOF or I/O error.
            }
        }
        // Dropping stdin_tx contributes to disconnecting serve_rx.
    });

    // Push forwarder thread: bridges `push_rx` → `ServeInput::Push`.
    // Only spawned when push_rx is Some (production). Skipped in tests that
    // supply only stdin events.
    if let Some(push_rx) = push_rx {
        let push_tx = serve_tx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = push_rx.recv() {
                if push_tx.send(ServeInput::Push(event)).is_err() {
                    break; // Serve loop exited.
                }
            }
            // Dropping push_tx contributes to disconnecting serve_rx.
        });
    }

    // Drop the original sender so serve_rx disconnects when both feeder
    // threads exit (no dangling sender keeping the channel alive).
    drop(serve_tx);

    // Serve loop: blocking recv — wakes immediately on any input.
    loop {
        match serve_rx.recv() {
            Ok(ServeInput::Stdin(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<JsonRpcRequest>(&line) {
                    Err(_) => {
                        let id = try_extract_id(&line);
                        write_error(&mut writer, JsonRpcError::parse_error(id))?;
                    }
                    Ok(req) => {
                        let response = dispatch_with_resources(
                            req,
                            registry,
                            &sub_reg,
                            &diag_reader,
                            &resource_uris,
                        );
                        match response {
                            DispatchResult::Ok(r) => write_response(&mut writer, r)?,
                            DispatchResult::Err(e) => write_error(&mut writer, e)?,
                            DispatchResult::Notification => {}
                        }
                    }
                }
            }
            Ok(ServeInput::Push(event)) => {
                let subscribed = sub_reg
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_subscribed(&event.uri);
                if subscribed {
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/resources/updated",
                        "params": {
                            "uri": event.uri,
                            "generation": event.generation
                        }
                    });
                    let line = serde_json::to_string(&notification).unwrap_or_default();
                    writeln!(writer, "{line}")?;
                    writer.flush()?;
                }
            }
            // Both feeder threads exited: EOF from client + push channel closed.
            Err(mpsc::RecvError) => {
                sub_reg.lock().unwrap_or_else(|p| p.into_inner()).clear();
                return Ok(());
            }
        }
    }
}

/// Attempt to extract the `id` field from a raw JSON line that failed full
/// [`JsonRpcRequest`] deserialization.
///
/// JSON-RPC 2.0 §5.1 requires the error `id` to be Null *only* when the id
/// itself is undetectable. When the frame is valid JSON (but structurally
/// incomplete — e.g. missing `method`), the id is readable and must be echoed.
///
/// Returns `None` when the line is not valid JSON or contains no `id` field.
fn try_extract_id(line: &str) -> Option<RequestId> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    match value.get("id")? {
        serde_json::Value::String(s) => Some(RequestId::Str(s.clone())),
        serde_json::Value::Number(n) => n.as_i64().map(RequestId::Num),
        serde_json::Value::Null => None,
        _ => None,
    }
}

fn write_response<W: Write>(writer: &mut W, r: JsonRpcResponse) -> Result<(), std::io::Error> {
    let line = serde_json::to_string(&r)
        .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"serialisation failed"},"id":null}"#.to_owned());
    writeln!(writer, "{line}")?;
    writer.flush()
}

fn write_error<W: Write>(writer: &mut W, e: JsonRpcError) -> Result<(), std::io::Error> {
    let line = serde_json::to_string(&e)
        .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"serialisation failed"},"id":null}"#.to_owned());
    writeln!(writer, "{line}")?;
    writer.flush()
}
