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

use serde_json::{Value, json};

use super::{
    registry::ToolRegistry,
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId, ToolError},
};

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
/// Responds with server info and capability advertisement. We advertise the
/// `tools` capability (listing and calling). Additional capabilities (e.g.
/// `resources`, `prompts`) are omitted until needed.
fn handle_initialize(id: Option<RequestId>) -> DispatchResult {
    let result = json!({
        "protocolVersion": "2024-11-05",
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
        },
        "capabilities": {
            "tools": {}
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
