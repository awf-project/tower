//! `ast` native sidecar extension (spec 26).
//!
//! Reimplements the `ast` WASM plugin as a native tree-sitter sidecar over the
//! Tower JSON-RPC 2.0 extension protocol. No WASM target, no WASI SDK required.
//!
//! # Protocol lifecycle
//!
//! ```text
//! host ──initialize──► ast_extension
//!      ◄──Initialized── (tools: find_symbols, search_symbols, get_outline, read_symbol, reindex)
//!                       (events: event/fileIndexed, event/fileChanged)
//!      ──invokeTool───► (tool_name, params)
//!      ◄──ToolResult──
//!      ──deliverEvent─► (FileIndexed | FileChanged)
//!      ◄──Ack──────────
//!      ──shutdown─────►
//!      ◄──Ack──────────
//! ```
//!
//! # Design
//!
//! - One in-memory [`SymbolIndex`] and content-hash dedup map, guarded by
//!   `std::sync::Mutex` (the protocol is serial per-extension; no concurrency needed).
//! - `fileIndexed`/`fileChanged`: host_call `workspace/readFile` → tree-sitter parse
//!   → update index via `index/put`.
//! - Tools: read from the in-process `SymbolIndex`; `get_outline`/`find_symbols`/
//!   `read_symbol` call `workspace/readFile` for per-file operations.
//! - `reindex`: calls `workspace/listFiles` + per-file `workspace/readFile`, rebuilds
//!   the index from scratch, then persists via `index/put`.
//!
//! # Host capabilities used
//!
//! `workspace/readFile`, `workspace/listFiles`, `index/get`, `index/put`, `log`.
//! All capability calls follow the host-call → host-response interleave pattern:
//! the extension sends a JSON-RPC request (with `id`) to stdout, then reads
//! the response from stdin before continuing.

mod index;
mod outline;
mod protocol;
mod symbols;
mod text;
mod tools;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::Mutex;

use extension_protocol::{
    Capability, InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response, ToolDecl,
};

use index::SymbolIndex;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Extension-wide mutable state (serial protocol, no concurrency).
    let index: Mutex<SymbolIndex> = Mutex::new(SymbolIndex::default());
    let content_hashes: Mutex<HashMap<String, u64>> = Mutex::new(HashMap::new());

    let mut lines = stdin.lock().lines();

    // Incremental host-call id counter (must not collide with request ids from host).
    // We use ids starting at 10_000 to be well clear of low host request ids.
    let mut next_hcall_id: u64 = 10_000;

    while let Some(Ok(line)) = lines.next() {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }

        let envelope: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = envelope.get("id").cloned();
        let method = envelope
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let params_val = envelope
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        match method.as_str() {
            "initialize" => {
                let init_params: InitParams = match serde_json::from_value(params_val) {
                    Ok(p) => p,
                    Err(e) => {
                        protocol::send_error(
                            &mut out,
                            &id,
                            -32602,
                            &format!("bad InitParams: {e}"),
                        );
                        continue;
                    }
                };

                if init_params.protocol_version != PROTOCOL_VERSION {
                    protocol::send_error(
                        &mut out,
                        &id,
                        -32600,
                        &format!(
                            "protocol version mismatch: host={} extension={}",
                            init_params.protocol_version, PROTOCOL_VERSION
                        ),
                    );
                    continue;
                }

                let result = InitResult {
                    tools: vec![
                        ToolDecl {
                            name: "get_outline".to_owned(),
                            description: "Return a structural outline (functions, structs, \
                                          methods, traits, enums, impl blocks) for a \
                                          workspace-relative source file. Supports Rust (.rs), \
                                          Go (.go), and PHP (.php). Returns an \
                                          unsupported-language result for other file types."
                                .to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to the source file"}},"required":["path"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "find_symbols".to_owned(),
                            description: "Find precise definition locations for a named symbol \
                                          of a given kind in a workspace-relative source file. \
                                          Uses the error-tolerant syntax tree to exclude false \
                                          positives in comments and string literals. Supports \
                                          Rust (.rs), Go (.go), and PHP (.php)."
                                .to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to the source file"},"symbol_name":{"type":"string","description":"Name of the symbol to search for"},"kind":{"type":"string","description":"Symbol kind: function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"}},"required":["path","symbol_name","kind"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "search_symbols".to_owned(),
                            description: "Search the in-memory cross-file symbol index for \
                                          symbols matching a given name and optional kind. The \
                                          index is built incrementally via fileIndexed/fileChanged \
                                          events and updated by reindex."
                                .to_owned(),
                            schema_json: r#"{"type":"object","properties":{"name":{"type":"string","description":"Exact symbol name to search for"},"kind":{"type":"string","description":"Optional symbol kind filter: function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"}},"required":["name"]}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "reindex".to_owned(),
                            description: "Rebuild the whole-project symbol index by enumerating \
                                          all workspace files and parsing each. Forces a full \
                                          reindex; use after large external changes or on a cold \
                                          cache."
                                .to_owned(),
                            schema_json: r#"{"type":"object","properties":{},"additionalProperties":false}"#.to_owned(),
                        },
                        ToolDecl {
                            name: "read_symbol".to_owned(),
                            description: "Read only a named symbol source span from a file — not \
                                          the whole file. Returns the exact byte slice \
                                          [start_byte, end_byte) plus kind and start/end rows for \
                                          each match, ordered by start_byte."
                                .to_owned(),
                            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to the source file"},"symbol_name":{"type":"string","description":"Name of the symbol to read"},"kind":{"type":"string","description":"Optional symbol kind filter: function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"}},"required":["path","symbol_name"]}"#.to_owned(),
                        },
                    ],
                    events: vec![
                        "event/fileIndexed".to_owned(),
                        "event/fileChanged".to_owned(),
                    ],
                    capabilities: vec![
                        Capability::ReadFile,
                        Capability::ListFiles,
                        Capability::IndexGet,
                        Capability::IndexPut,
                        Capability::Log,
                    ],
                };

                // Warm-start: load existing index from the host AST store BEFORE
                // sending the Initialized response.
                //
                // This ordering is critical for protocol correctness. If we sent
                // Initialized first and then made a HostCall (index/get), the host
                // would see Initialized and return from spawn(). The test thread
                // could then immediately write a new request (e.g. deliverEvent) to
                // the child's stdin. That frame would arrive while the child is
                // inside read_host_response waiting for the index/get reply (id=10000).
                // read_host_response silently skips frames whose id does not match,
                // so the deliverEvent frame (id=1) would be consumed and discarded.
                // Result: the child never sends an Ack for deliverEvent, the host
                // times out, and the test fails with ExtensionFault::Timeout.
                //
                // By performing all HostCalls before Initialized, the host's
                // wait_response_timed cannot return until those HostCalls are fully
                // dispatched and answered. The child is back in its outer request loop
                // before the host hands control to the test thread.
                let loaded = protocol::host_call_index_get(
                    &mut out,
                    &mut lines,
                    &mut next_hcall_id,
                    "ast/symbols",
                );
                if let Ok(bytes) = loaded
                    && let Some(idx) = SymbolIndex::from_bytes(&bytes)
                {
                    *index.lock().unwrap() = idx;
                }

                let resp = Response::Initialized(result);
                protocol::send_response(&mut out, &id, &resp);
            }

            "invokeTool" => {
                let tool_name = params_val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let tool_params = params_val
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let result = tools::dispatch(
                    &tool_name,
                    tool_params,
                    &index,
                    &content_hashes,
                    &mut out,
                    &mut lines,
                    &mut next_hcall_id,
                );

                match result {
                    Ok(value) => {
                        protocol::send_response(&mut out, &id, &Response::ToolResult(value));
                    }
                    Err(err_msg) => {
                        protocol::send_response(
                            &mut out,
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

            "deliverEvent" => {
                // params is the Event struct (or just path-containing JSON).
                // The host sends event as {"type":"FileIndexed","file_id":..,"path":..}
                // or wrapped in Request::DeliverEvent envelope.
                let path = extract_event_path(&params_val);

                if let Some(path_str) = path {
                    // Re-index this file: readFile → parse → update index → persist.
                    let reindex_result = tools::reindex_single_path(
                        &path_str,
                        &index,
                        &content_hashes,
                        &mut out,
                        &mut lines,
                        &mut next_hcall_id,
                    );
                    if let Err(e) = reindex_result {
                        // UN1: log but do not crash — continue serving other files.
                        let _ = protocol::host_call_log(
                            &mut out,
                            &mut lines,
                            &mut next_hcall_id,
                            "warn",
                            &format!("deliverEvent reindex_single_path failed: {e}"),
                        );
                    }
                }

                protocol::send_response(&mut out, &id, &Response::Ack);
            }

            "shutdown" => {
                protocol::send_response(&mut out, &id, &Response::Ack);
                let _ = out.flush();
                break;
            }

            other => {
                protocol::send_error(&mut out, &id, -32601, &format!("unknown method: {other}"));
            }
        }

        let _ = out.flush();
    }
}

/// Extract the `path` field from a `deliverEvent` params value.
///
/// The host sends params as either:
/// - A `Request::DeliverEvent(Event)` adjacently-tagged value:
///   `{"type":"DeliverEvent","data":{"type":"FileIndexed","file_id":1,"path":"..."}}`
/// - Or just the Event directly:
///   `{"type":"FileIndexed","file_id":1,"path":"..."}`
///
/// We try both shapes.
fn extract_event_path(params: &serde_json::Value) -> Option<String> {
    // Shape 1: adjacently-tagged Request::DeliverEvent
    if let Some(data) = params.get("data")
        && let Some(path) = data.get("path").and_then(|v| v.as_str())
    {
        return Some(path.to_owned());
    }
    // Shape 2: bare Event
    if let Some(path) = params.get("path").and_then(|v| v.as_str()) {
        return Some(path.to_owned());
    }
    None
}
