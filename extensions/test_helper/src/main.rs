//! Minimal test-only sidecar extension for spec 23 integration tests.
//!
//! Implements the full Tower JSON-RPC 2.0 extension protocol over stdio:
//!   host → initialize  → this binary → Initialized{tools,events,capabilities}
//!   host → invokeTool  → this binary → ToolResult(...)
//!   host → deliverEvent → this binary → Ack
//!   host → shutdown    → this binary → Ack (then exit)
//!
//! The extension exercises the following capabilities, which the host must serve:
//!   - workspace/readFile
//!   - index/get, index/put
//!
//! Tool "echo": returns the params unchanged.
//! Tool "read_file": calls workspace/readFile{path} from params, returns the bytes as a string.
//! Tool "index_roundtrip": calls index/put then index/get, returns the retrieved bytes.
//! Tool "read_bad_path": calls workspace/readFile with the path from params (used to test
//!   denial of traversal/absolute paths — expects the host to return an error).

use std::io::{self, BufRead, Write};

use extension_protocol::{
    Capability, HostCall, InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response,
    ToolDecl,
};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut lines = stdin.lock().lines();

    while let Some(Ok(line)) = lines.next() {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }

        // Parse the JSON-RPC envelope to get `id` and `method`.
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

        // Parse typed request from `params` (our messages use the typed Request enum
        // embedded in the params field, or directly as the whole payload).
        // The host sends Request as the JSON-RPC params value.
        // The wire shape for Request is adjacently tagged: {"type":"...","data":{...}}
        // We receive the full envelope; params contains the Request payload.
        let params_val = envelope
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        match method.as_str() {
            "initialize" => {
                // params is InitParams directly (not wrapped in Request enum)
                let init_params: InitParams = match serde_json::from_value(params_val) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error(&mut out, &id, -32602, &format!("bad InitParams: {e}"));
                        continue;
                    }
                };

                // Protocol version check (UN1): reject mismatched version.
                if init_params.protocol_version != PROTOCOL_VERSION {
                    send_error(
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
                            name: "echo".to_owned(),
                            description: "Echo params back".to_owned(),
                            schema_json: "{}".to_owned(),
                        },
                        ToolDecl {
                            name: "read_file".to_owned(),
                            description: "Call workspace/readFile with path from params".to_owned(),
                            schema_json: "{}".to_owned(),
                        },
                        ToolDecl {
                            name: "index_roundtrip".to_owned(),
                            description: "Put then get from AST index".to_owned(),
                            schema_json: "{}".to_owned(),
                        },
                        ToolDecl {
                            name: "read_bad_path".to_owned(),
                            description: "Attempt to read a bad path (for denial tests)".to_owned(),
                            schema_json: "{}".to_owned(),
                        },
                    ],
                    events: vec![
                        "event/fileIndexed".to_owned(),
                        "event/fileChanged".to_owned(),
                    ],
                    capabilities: vec![
                        Capability::ReadFile,
                        Capability::IndexGet,
                        Capability::IndexPut,
                        Capability::Log,
                    ],
                };

                let resp = Response::Initialized(result);
                send_response(&mut out, &id, &resp);
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

                match tool_name.as_str() {
                    "echo" => {
                        let resp = Response::ToolResult(tool_params);
                        send_response(&mut out, &id, &resp);
                    }
                    "read_file" => {
                        let path = tool_params
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();

                        // Make a host call: workspace/readFile
                        let host_call = HostCall::ReadFile { path: path.clone() };
                        let call_id = serde_json::json!(9001_u64);
                        send_host_call(&mut out, &call_id, "workspace/readFile", &host_call);
                        out.flush().unwrap();

                        // Read host response
                        match read_host_response(&mut lines, &call_id) {
                            Ok(result_val) => {
                                let resp = Response::ToolResult(result_val);
                                send_response(&mut out, &id, &resp);
                            }
                            Err(err_msg) => {
                                let resp = Response::Error(ProtocolError {
                                    code: -32000,
                                    message: err_msg,
                                    data: None,
                                });
                                send_response(&mut out, &id, &resp);
                            }
                        }
                    }
                    "index_roundtrip" => {
                        let key = tool_params
                            .get("key")
                            .and_then(|v| v.as_str())
                            .unwrap_or("test-key")
                            .to_owned();
                        let value = tool_params
                            .get("value")
                            .and_then(|v| v.as_str())
                            .unwrap_or("test-bytes")
                            .to_owned();

                        // index/put
                        let put_call = HostCall::IndexPut {
                            key: key.clone(),
                            bytes: value.as_bytes().to_vec(),
                        };
                        let put_id = serde_json::json!(9002_u64);
                        send_host_call(&mut out, &put_id, "index/put", &put_call);
                        out.flush().unwrap();

                        match read_host_response(&mut lines, &put_id) {
                            Ok(_) => {}
                            Err(err_msg) => {
                                let resp = Response::Error(ProtocolError {
                                    code: -32000,
                                    message: format!("index/put failed: {err_msg}"),
                                    data: None,
                                });
                                send_response(&mut out, &id, &resp);
                                continue;
                            }
                        }

                        // index/get
                        let get_call = HostCall::IndexGet { key: key.clone() };
                        let get_id = serde_json::json!(9003_u64);
                        send_host_call(&mut out, &get_id, "index/get", &get_call);
                        out.flush().unwrap();

                        match read_host_response(&mut lines, &get_id) {
                            Ok(result_val) => {
                                let resp = Response::ToolResult(result_val);
                                send_response(&mut out, &id, &resp);
                            }
                            Err(err_msg) => {
                                let resp = Response::Error(ProtocolError {
                                    code: -32000,
                                    message: format!("index/get failed: {err_msg}"),
                                    data: None,
                                });
                                send_response(&mut out, &id, &resp);
                            }
                        }
                    }
                    "read_bad_path" => {
                        let path = tool_params
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();

                        let host_call = HostCall::ReadFile { path };
                        let call_id = serde_json::json!(9004_u64);
                        send_host_call(&mut out, &call_id, "workspace/readFile", &host_call);
                        out.flush().unwrap();

                        // Expect this to come back as an error from the host.
                        match read_host_response(&mut lines, &call_id) {
                            Ok(v) => {
                                // Unexpectedly succeeded — return the value
                                let resp = Response::ToolResult(v);
                                send_response(&mut out, &id, &resp);
                            }
                            Err(err_msg) => {
                                // Return the denial message so tests can assert on it
                                let resp = Response::Error(ProtocolError {
                                    code: -32000,
                                    message: err_msg,
                                    data: None,
                                });
                                send_response(&mut out, &id, &resp);
                            }
                        }
                    }
                    other => {
                        send_error(&mut out, &id, -32601, &format!("unknown tool: {other}"));
                    }
                }
            }
            "deliverEvent" => {
                // Acknowledge event delivery
                let resp = Response::Ack;
                send_response(&mut out, &id, &resp);
            }
            "shutdown" => {
                let resp = Response::Ack;
                send_response(&mut out, &id, &resp);
                out.flush().unwrap();
                break;
            }
            other => {
                send_error(&mut out, &id, -32601, &format!("unknown method: {other}"));
            }
        }
        out.flush().unwrap();
    }
}

fn send_response(out: &mut impl Write, id: &Option<serde_json::Value>, resp: &Response) {
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

fn send_error(out: &mut impl Write, id: &Option<serde_json::Value>, code: i32, msg: &str) {
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

/// Send a host capability call (the extension is the requester here).
fn send_host_call(out: &mut impl Write, id: &serde_json::Value, method: &str, call: &HostCall) {
    let params = serde_json::to_value(call).expect("serialize HostCall");
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let s = serde_json::to_string(&envelope).expect("serialize host call");
    writeln!(out, "{s}").unwrap();
}

/// Read the next non-blank line from stdin and parse it as a host response matching `expected_id`.
fn read_host_response(
    lines: &mut impl Iterator<Item = Result<String, io::Error>>,
    expected_id: &serde_json::Value,
) -> Result<serde_json::Value, String> {
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

        // Check this is the response for our call.
        if envelope.get("id") != Some(expected_id) {
            // Unexpected: skip for now (serial protocol, should not happen in tests).
            continue;
        }

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
}
