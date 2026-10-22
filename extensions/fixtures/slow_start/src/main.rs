//! Deterministic slow-start sidecar used by daemon readiness process tests.

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};

use extension_protocol::{Event, InitParams, InitResult, PROTOCOL_VERSION, Response, ToolDecl};
use serde::{Deserialize, Serialize};

const PROBE_SCHEMA: &str =
    r#"{"type":"object","properties":{"release":{"type":"boolean"}},"additionalProperties":false}"#;
const FENCE_SCHEMA: &str = r#"{"type":"object","properties":{"epoch":{"type":"integer"},"delivered_events":{"type":"integer"}},"required":["epoch","delivered_events"],"additionalProperties":false}"#;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlowStartMode {
    Block,
    Resume,
    Fail,
    Crash,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SlowStartConfig {
    mode: SlowStartMode,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProbeCatchUpParams {
    pub release: bool,
}

impl Default for ProbeCatchUpParams {
    fn default() -> Self {
        Self { release: true }
    }
}

#[derive(Serialize)]
pub struct ProbeCatchUpResult {
    pub processed_events: u64,
    pub processed_ordinals: Vec<u64>,
    pub released: bool,
    pub barrier_observed: bool,
    pub pending_fences: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatchUpFenceParams {
    pub epoch: u64,
    pub delivered_events: u64,
}

#[derive(Serialize)]
pub struct CatchUpFenceResult {
    pub epoch: u64,
    pub processed_events: u64,
    pub ready: bool,
}

pub struct SlowStartFixture {
    mode: SlowStartMode,
    accepted_events: u64,
    pending_events: u64,
    pending_ordinals: VecDeque<u64>,
    processed_events: u64,
    processed_ordinals: VecDeque<u64>,
    released: bool,
    barrier_observed: bool,
    pending_fences: u64,
}

pub struct InvalidInitialization;

impl SlowStartFixture {
    pub fn initialize(params: InitParams) -> Result<Self, InvalidInitialization> {
        let config: SlowStartConfig =
            serde_json::from_value(params.extension_config.ok_or(InvalidInitialization)?)
                .map_err(|_| InvalidInitialization)?;
        Ok(Self {
            mode: config.mode,
            accepted_events: 0,
            pending_events: 0,
            pending_ordinals: VecDeque::new(),
            processed_events: 0,
            processed_ordinals: VecDeque::new(),
            released: matches!(config.mode, SlowStartMode::Resume),
            barrier_observed: false,
            pending_fences: 0,
        })
    }

    fn accept_event(&mut self, _event: Event) -> Result<(), EventFailure> {
        self.accepted_events += 1;
        let ordinal = self.accepted_events;
        match self.mode {
            SlowStartMode::Block if !self.released => {
                self.pending_events += 1;
                Self::record_ordinal(&mut self.pending_ordinals, ordinal);
            }
            SlowStartMode::Block | SlowStartMode::Resume => self.record_processed(ordinal),
            SlowStartMode::Fail => return Err(EventFailure::Planned),
            SlowStartMode::Crash => return Err(EventFailure::Crash),
        }
        Ok(())
    }

    pub fn probe_catch_up(&mut self, params: ProbeCatchUpParams) -> ProbeCatchUpResult {
        if params.release && !self.released {
            self.released = true;
            self.processed_events += self.pending_events;
            self.pending_events = 0;
            self.processed_ordinals = std::mem::take(&mut self.pending_ordinals);
        }
        ProbeCatchUpResult {
            processed_events: self.processed_events,
            processed_ordinals: self.processed_ordinals.iter().copied().collect(),
            released: self.released,
            barrier_observed: self.barrier_observed,
            pending_fences: self.pending_fences,
        }
    }

    pub fn catch_up_fence(&mut self, params: CatchUpFenceParams) -> CatchUpFenceResult {
        let ready = self.released && self.processed_events >= params.delivered_events;
        if ready {
            self.barrier_observed = true;
        } else {
            self.pending_fences += 1;
        }
        CatchUpFenceResult {
            epoch: params.epoch,
            processed_events: self.processed_events,
            ready,
        }
    }

    fn record_processed(&mut self, ordinal: u64) {
        self.processed_events += 1;
        Self::record_ordinal(&mut self.processed_ordinals, ordinal);
    }

    fn record_ordinal(ordinals: &mut VecDeque<u64>, ordinal: u64) {
        if ordinals.len() == 64 {
            ordinals.pop_front();
        }
        ordinals.push_back(ordinal);
    }
}

enum EventFailure {
    Planned,
    Crash,
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut fixture = None;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = envelope.get("id").cloned();
        let method = envelope
            .get("method")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let params = envelope
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match method {
            "initialize" => initialize(&mut output, id, params, &mut fixture),
            "deliverEvent" => deliver_event(&mut output, id, params, &mut fixture),
            "invokeTool" => invoke_tool(&mut output, id, params, &mut fixture),
            "shutdown" => {
                send_response(&mut output, id, Response::Ack);
                break;
            }
            _ => send_error(&mut output, id, -32601, "unknown method"),
        }
    }
}

fn initialize(
    output: &mut impl Write,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    fixture: &mut Option<SlowStartFixture>,
) {
    let Ok(init_params) = serde_json::from_value::<InitParams>(params) else {
        send_error(output, id, -32602, "invalid slow_start initialization");
        return;
    };
    if init_params.protocol_version != PROTOCOL_VERSION {
        send_error(output, id, -32600, "protocol version mismatch");
        return;
    }
    let Ok(initialized) = SlowStartFixture::initialize(init_params) else {
        send_error(output, id, -32602, "invalid slow_start initialization");
        return;
    };
    *fixture = Some(initialized);
    send_response(
        output,
        id,
        Response::Initialized(InitResult {
            tools: vec![
                ToolDecl {
                    name: "probe_catch_up".to_owned(),
                    description: "Release or observe deterministic slow-start catch-up.".to_owned(),
                    schema_json: PROBE_SCHEMA.to_owned(),
                },
                ToolDecl {
                    name: "_tower_catch_up_fence".to_owned(),
                    description: "Host-only catch-up ordering barrier.".to_owned(),
                    schema_json: FENCE_SCHEMA.to_owned(),
                },
            ],
            events: vec![
                "event/fileIndexed".to_owned(),
                "event/fileChanged".to_owned(),
                "event/fileDeleted".to_owned(),
            ],
            capabilities: Vec::new(),
        }),
    );
}

fn deliver_event(
    output: &mut impl Write,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    fixture: &mut Option<SlowStartFixture>,
) {
    let Some(fixture) = fixture.as_mut() else {
        send_error(output, id, -32002, "extension not initialized");
        return;
    };
    let Ok(event) = serde_json::from_value::<Event>(params) else {
        send_error(output, id, -32602, "invalid deliverEvent parameters");
        return;
    };
    match fixture.accept_event(event) {
        Ok(()) => send_response(output, id, Response::Ack),
        Err(EventFailure::Planned) => send_error(output, id, -32000, "planned replay failure"),
        Err(EventFailure::Crash) => std::process::exit(42),
    }
}

fn invoke_tool(
    output: &mut impl Write,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    fixture: &mut Option<SlowStartFixture>,
) {
    let Some(fixture) = fixture.as_mut() else {
        send_error(output, id, -32002, "extension not initialized");
        return;
    };
    let name = params
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let tool_params = params
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match name {
        "probe_catch_up" => {
            let Ok(tool_params) = serde_json::from_value::<ProbeCatchUpParams>(tool_params) else {
                send_error(output, id, -32602, "invalid probe_catch_up parameters");
                return;
            };
            send_tool_result(output, id, fixture.probe_catch_up(tool_params));
        }
        "_tower_catch_up_fence" => {
            let Ok(tool_params) = serde_json::from_value::<CatchUpFenceParams>(tool_params) else {
                send_error(output, id, -32602, "invalid catch-up fence parameters");
                return;
            };
            send_tool_result(output, id, fixture.catch_up_fence(tool_params));
        }
        _ => send_error(output, id, -32601, "unknown tool"),
    }
}

fn send_tool_result(
    output: &mut impl Write,
    id: Option<serde_json::Value>,
    result: impl Serialize,
) {
    send_response(
        output,
        id,
        Response::ToolResult(serde_json::to_value(result).expect("serialize fixture result")),
    );
}

fn send_response(output: &mut impl Write, id: Option<serde_json::Value>, response: Response) {
    let result = serde_json::to_value(response).expect("serialize protocol response");
    write_envelope(
        output,
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn send_error(output: &mut impl Write, id: Option<serde_json::Value>, code: i32, message: &str) {
    write_envelope(
        output,
        serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    );
}

fn write_envelope(output: &mut impl Write, envelope: serde_json::Value) {
    writeln!(output, "{envelope}").expect("write protocol response");
    output.flush().expect("flush protocol response");
}
