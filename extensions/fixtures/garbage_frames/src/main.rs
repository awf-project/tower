//! Spec 24 fault fixture: garbage-frames.
//!
//! Completes the `initialize` handshake (so `spawn` succeeds), then responds to
//! every subsequent request with a non-JSON garbage line.  This causes the host
//! to receive an undecodable frame and return `ExtensionFault::ProtocolError`
//! (AC3).

use std::io::{self, BufRead, Write};

use extension_protocol::{
    Capability, InitParams, InitResult, PROTOCOL_VERSION, Response, ToolDecl,
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
                    send_error(&mut out, &id, -32600, "protocol version mismatch");
                    continue;
                }
                let result = InitResult {
                    tools: vec![ToolDecl {
                        name: "run".to_owned(),
                        description: "Emits garbage when called".to_owned(),
                        schema_json: "{}".to_owned(),
                    }],
                    events: vec![],
                    capabilities: vec![Capability::Log],
                };
                let resp = Response::Initialized(result);
                send_response(&mut out, &id, &resp);
                out.flush().unwrap();
            }
            "invokeTool" | "deliverEvent" | "shutdown" => {
                // Emit pure garbage — not valid JSON, not a valid frame.
                writeln!(out, "THIS IS NOT JSON !!@#$%^&*()").unwrap();
                out.flush().unwrap();
                // Then exit so the host's reader thread sees EOF.
                break;
            }
            _ => {
                send_error(&mut out, &id, -32601, "unknown method");
            }
        }
        out.flush().unwrap();
    }
}

fn send_response(out: &mut impl Write, id: &Option<serde_json::Value>, resp: &Response) {
    let result = serde_json::to_value(resp).expect("serialize Response");
    let envelope = if let Some(id_val) = id {
        serde_json::json!({"jsonrpc": "2.0", "id": id_val, "result": result})
    } else {
        serde_json::json!({"jsonrpc": "2.0", "result": result})
    };
    writeln!(out, "{}", serde_json::to_string(&envelope).unwrap()).unwrap();
}

fn send_error(out: &mut impl Write, id: &Option<serde_json::Value>, code: i32, msg: &str) {
    let envelope = if let Some(id_val) = id {
        serde_json::json!({"jsonrpc": "2.0", "id": id_val, "error": {"code": code, "message": msg}})
    } else {
        serde_json::json!({"jsonrpc": "2.0", "error": {"code": code, "message": msg}})
    };
    writeln!(out, "{}", serde_json::to_string(&envelope).unwrap()).unwrap();
}
