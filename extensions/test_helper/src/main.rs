//! Minimal test-only sidecar extension for spec 23 integration tests.
//!
//! Implements the full Tower JSON-RPC 2.0 extension protocol over stdio:
//!   host → initialize  → this binary → Initialized{tools,events,capabilities}
//!   host → invokeTool  → this binary → ToolResult payload
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

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::thread;

use extension_protocol::{
    Capability, HostCall, InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response,
    ToolDecl,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatePhase {
    Initialize,
    InvokeTool,
    DeliverEvent,
}

impl GatePhase {
    fn environment_value(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::InvokeTool => "invokeTool",
            Self::DeliverEvent => "deliverEvent",
        }
    }

    fn markers(self) -> (&'static str, &'static str) {
        match self {
            Self::Initialize => ("initialize_entered", "initialize_release"),
            Self::InvokeTool => ("invoke_tool_entered", "invoke_tool_release"),
            Self::DeliverEvent => ("deliver_event_entered", "deliver_event_release"),
        }
    }
}

#[derive(Debug)]
struct TestHelperGate {
    phase: GatePhase,
    control_dir: PathBuf,
}

impl TestHelperGate {
    fn from_environment(phase: GatePhase) -> Result<Option<Self>, ProtocolError> {
        if env::var("TOWER_TEST_HELPER_GATE").ok().as_deref() != Some(phase.environment_value()) {
            return Ok(None);
        }

        let control_dir = env::var_os("TOWER_TEST_HELPER_CONTROL_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_dir())
            .ok_or_else(invalid_gate_control_dir)?;
        Ok(Some(Self { phase, control_dir }))
    }

    fn wait_for_release(&self) -> Result<(), ProtocolError> {
        let (entered_name, release_name) = self.phase.markers();
        let entered = self.control_dir.join(entered_name);
        let staged = self.control_dir.join(format!(".{entered_name}.tmp"));
        fs::write(&staged, []).map_err(|_| invalid_gate_control_dir())?;
        fs::rename(&staged, &entered).map_err(|_| invalid_gate_control_dir())?;

        let release = self.control_dir.join(release_name);
        while !release.is_file() {
            if !self.control_dir.is_dir() {
                return Err(invalid_gate_control_dir());
            }
            thread::yield_now();
        }
        Ok(())
    }
}

fn invalid_gate_control_dir() -> ProtocolError {
    ProtocolError {
        code: -32602,
        message: "invalid test-helper gate control directory".to_owned(),
        data: None,
    }
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut lines = stdin.lock().lines();
    let mut queued = VecDeque::new();

    loop {
        let envelope = match next_envelope(&mut lines, &mut queued) {
            Ok(Some(envelope)) => envelope,
            Ok(None) => break,
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
        // Request uses the adjacent `type` plus `data` tagging defined by the protocol crate.
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

                if let Err(error) = wait_for_gate(GatePhase::Initialize) {
                    send_error(&mut out, &id, error.code, &error.message);
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
                if let Err(error) = wait_for_gate(GatePhase::InvokeTool) {
                    send_error(&mut out, &id, error.code, &error.message);
                    continue;
                }
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
                        match read_host_response(&mut lines, &mut queued, &call_id) {
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

                        match read_host_response(&mut lines, &mut queued, &put_id) {
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

                        match read_host_response(&mut lines, &mut queued, &get_id) {
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
                        match read_host_response(&mut lines, &mut queued, &call_id) {
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
                if let Err(error) = wait_for_gate(GatePhase::DeliverEvent) {
                    send_error(&mut out, &id, error.code, &error.message);
                    continue;
                }
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

fn wait_for_gate(phase: GatePhase) -> Result<(), ProtocolError> {
    if let Some(gate) = TestHelperGate::from_environment(phase)? {
        gate.wait_for_release()?;
    }
    Ok(())
}

fn next_envelope(
    lines: &mut impl Iterator<Item = Result<String, io::Error>>,
    queued: &mut VecDeque<serde_json::Value>,
) -> Result<Option<serde_json::Value>, String> {
    if let Some(envelope) = queued.pop_front() {
        return Ok(Some(envelope));
    }

    loop {
        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(error)) => return Err(format!("stdin error: {error}")),
            None => return Ok(None),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        return serde_json::from_str(line)
            .map(Some)
            .map_err(|error| format!("parse error: {error}"));
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
    queued: &mut VecDeque<serde_json::Value>,
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

        // Request and response IDs are independent in each direction. Always
        // preserve inbound requests, even when the host happens to reuse the
        // fixture's outstanding HostCall ID.
        if envelope.get("method").is_some() {
            queued.push_back(envelope);
            continue;
        }

        // Check this is the response for our call.
        if envelope.get("id") != Some(expected_id) {
            queued.push_back(envelope);
            continue;
        }

        let result = envelope.get("result");
        let error = envelope.get("error");
        return match (result, error) {
            (Some(result), None) => Ok(result.clone()),
            (None, Some(error)) => Err(error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_owned()),
            _ => {
                Err("malformed host response (expected exactly one of result or error)".to_owned())
            }
        };
    }
}
