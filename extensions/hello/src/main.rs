//! `hello` — minimal lazy sidecar extension (spec 26).
//!
//! This is the reference "hello world" example for the Tower extension system.
//! It demonstrates:
//!   - A `lazy` activation extension (spawned only on first tool invocation).
//!   - A single tool `greet` with one optional parameter `name`.
//!   - No event subscriptions (tool-only).
//!   - No host capabilities (no file reads, no index access).
//!
//! # Protocol
//!
//! ```text
//! host ──initialize──► hello_extension
//!      ◄──Initialized── (tools: [greet], events: [], capabilities: [])
//!      ──invokeTool(greet, {name?})──►
//!      ◄──ToolResult("Hello, <name>!")──
//!      ──shutdown──►
//!      ◄──Ack──────────
//! ```

use std::io::{self, BufRead, Write};

use extension_protocol::{
    InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response, ToolDecl,
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
                        send_error(&mut out, &id, -32602, &format!("bad InitParams: {e}"));
                        continue;
                    }
                };

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
                    tools: vec![ToolDecl {
                        name: "greet".to_owned(),
                        description: "Return a greeting string. Pass 'name' to personalize."
                            .to_owned(),
                        schema_json: r#"{"type":"object","properties":{"name":{"type":"string","description":"Name to greet (optional, defaults to World)"}}}"#.to_owned(),
                    }],
                    events: vec![],
                    capabilities: vec![],
                };
                send_response(&mut out, &id, &Response::Initialized(result));
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
                    "greet" => {
                        let name = tool_params
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("World");
                        let greeting = format!("Hello, {name}!");
                        send_response(
                            &mut out,
                            &id,
                            &Response::ToolResult(serde_json::Value::String(greeting)),
                        );
                    }
                    other => {
                        send_response(
                            &mut out,
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
                // hello does not subscribe to events; ack anyway.
                send_response(&mut out, &id, &Response::Ack);
            }

            "shutdown" => {
                send_response(&mut out, &id, &Response::Ack);
                let _ = out.flush();
                break;
            }

            other => {
                send_error(&mut out, &id, -32601, &format!("unknown method: {other}"));
            }
        }

        let _ = out.flush();
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
