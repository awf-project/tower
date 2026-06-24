use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn main() {
    if let Err(err) = DebugAdapterFixture::new(io::stdin(), io::stdout()).run() {
        eprintln!("fixture_debug_adapter failed: {err}");
        std::process::exit(1);
    }
}

struct DebugAdapterFixture<R, W> {
    input: R,
    output: W,
    next_seq: u64,
    continue_count: u64,
    suppress_continue_response: bool,
    continue_event_delay: Duration,
}

impl<R, W> DebugAdapterFixture<R, W>
where
    R: Read,
    W: Write,
{
    fn new(input: R, output: W) -> Self {
        let continue_delay = continue_delay_from_args();
        Self {
            input,
            output,
            next_seq: 1,
            continue_count: 0,
            suppress_continue_response: continue_delay > Duration::ZERO,
            continue_event_delay: continue_event_delay_from_args(),
        }
    }

    fn run(&mut self) -> io::Result<()> {
        while let Some(request) = read_frame(&mut self.input)? {
            let Some(response) = self.dispatch(request)? else {
                continue;
            };
            write_frame(&mut self.output, &response)?;
        }
        Ok(())
    }

    fn dispatch(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        match request.command {
            DapCommand::Initialize => self.initialize(request),
            DapCommand::Launch => self.launch(request),
            DapCommand::SetBreakpoints => self.set_breakpoints(request),
            DapCommand::ConfigurationDone => self.configuration_done(request),
            DapCommand::Continue => self.continue_execution(request),
            DapCommand::Next => self.next(request),
            DapCommand::Step => self.step(request),
            DapCommand::Pause => self.pause(request),
            DapCommand::Threads => self.threads(request),
            DapCommand::StackTrace => self.stack_trace(request),
            DapCommand::Scopes => self.scopes(request),
            DapCommand::Variables => self.variables(request),
            DapCommand::Evaluate => self.evaluate(request),
            DapCommand::Terminate => self.terminate(request),
            DapCommand::Disconnect => self.disconnect(request),
        }
    }

    fn initialize(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.response(
            request,
            Some(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsTerminateRequest": true
            })),
        )))
    }

    fn launch(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.empty_response(request)))
    }

    fn set_breakpoints(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        let breakpoints = request
            .arguments
            .get("breakpoints")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, breakpoint)| {
                json!({
                    "id": index + 1,
                    "verified": true,
                    "line": breakpoint.get("line").and_then(Value::as_u64).unwrap_or(1)
                })
            })
            .collect::<Vec<_>>();
        Ok(Some(self.response(
            request,
            Some(json!({ "breakpoints": breakpoints })),
        )))
    }

    fn configuration_done(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.empty_response(request)))
    }

    fn continue_execution(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        self.continue_count += 1;
        if self.suppress_continue_response {
            return Ok(None);
        }

        let (event, body) = if self.continue_count == 1 {
            ("stopped", json!({ "reason": "breakpoint", "threadId": 1 }))
        } else {
            ("terminated", json!({}))
        };
        let response = self.response(request, Some(json!({ "allThreadsContinued": true })));

        if self.continue_event_delay > Duration::ZERO {
            write_frame(&mut self.output, &response)?;
            thread::sleep(self.continue_event_delay);
            self.write_event(event, body)?;
            return Ok(None);
        }

        self.write_event(event, body)?;
        Ok(Some(response))
    }

    fn next(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        self.write_event("stopped", json!({ "reason": "step", "threadId": 1 }))?;
        Ok(Some(self.empty_response(request)))
    }

    fn step(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        self.write_event("stopped", json!({ "reason": "step", "threadId": 1 }))?;
        Ok(Some(self.empty_response(request)))
    }

    fn pause(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        self.write_event("stopped", json!({ "reason": "pause", "threadId": 1 }))?;
        Ok(Some(self.empty_response(request)))
    }

    fn threads(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.response(
            request,
            Some(json!({ "threads": [{ "id": 1, "name": "main" }] })),
        )))
    }

    fn stack_trace(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.response(
            request,
            Some(json!({
                "stackFrames": [{
                    "id": 1,
                    "name": "main",
                    "source": { "path": "src/main.rs" },
                    "line": 12,
                    "column": 5
                }]
            })),
        )))
    }

    fn scopes(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.response(
            request,
            Some(json!({
                "scopes": [{
                    "name": "Locals",
                    "variablesReference": 100,
                    "expensive": false
                }]
            })),
        )))
    }

    fn variables(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.response(
            request,
            Some(json!({
                "variables": [{
                    "name": "answer",
                    "value": "42",
                    "type": "i32",
                    "variablesReference": 0
                }]
            })),
        )))
    }

    fn evaluate(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        let name = request
            .arguments
            .get("expression")
            .and_then(Value::as_str)
            .unwrap_or("answer")
            .to_owned();
        Ok(Some(self.response(
            request,
            Some(json!({
                "name": name,
                "value": "42",
                "type": "i32",
                "variablesReference": 0
            })),
        )))
    }

    fn terminate(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.empty_response(request)))
    }

    fn disconnect(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.empty_response(request)))
    }

    fn empty_response(&mut self, request: DapRequest) -> DapResponse {
        self.response(request, Some(json!({})))
    }

    fn response(&mut self, request: DapRequest, body: Option<Value>) -> DapResponse {
        let seq = self.next_seq;
        self.next_seq += 1;
        DapResponse {
            seq,
            message_type: "response",
            request_seq: request.seq,
            success: true,
            command: request.command,
            body,
        }
    }

    fn write_event(&mut self, event: &str, body: Value) -> io::Result<()> {
        let seq = self.next_seq;
        self.next_seq += 1;
        write_frame(
            &mut self.output,
            &DapEvent {
                seq,
                message_type: "event",
                event,
                body: Some(body),
            },
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DapRequest {
    seq: u64,
    command: DapCommand,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
enum DapCommand {
    Initialize,
    Launch,
    SetBreakpoints,
    ConfigurationDone,
    Continue,
    Next,
    Step,
    Pause,
    Threads,
    StackTrace,
    Scopes,
    Variables,
    Evaluate,
    Terminate,
    Disconnect,
}

#[derive(Debug, Clone, Serialize)]
struct DapResponse {
    seq: u64,
    #[serde(rename = "type")]
    message_type: &'static str,
    request_seq: u64,
    success: bool,
    command: DapCommand,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
struct DapEvent<'a> {
    seq: u64,
    #[serde(rename = "type")]
    message_type: &'static str,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<Value>,
}

impl Serialize for DapCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            DapCommand::Initialize => "initialize",
            DapCommand::Launch => "launch",
            DapCommand::SetBreakpoints => "setBreakpoints",
            DapCommand::ConfigurationDone => "configurationDone",
            DapCommand::Continue => "continue",
            DapCommand::Next => "next",
            DapCommand::Step => "step",
            DapCommand::Pause => "pause",
            DapCommand::Threads => "threads",
            DapCommand::StackTrace => "stackTrace",
            DapCommand::Scopes => "scopes",
            DapCommand::Variables => "variables",
            DapCommand::Evaluate => "evaluate",
            DapCommand::Terminate => "terminate",
            DapCommand::Disconnect => "disconnect",
        })
    }
}

fn continue_delay_from_args() -> Duration {
    delay_from_named_arg("--continue-delay-ms")
}

fn continue_event_delay_from_args() -> Duration {
    delay_from_named_arg("--continue-event-delay-ms")
}

fn delay_from_named_arg(name: &str) -> Duration {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
            return delay_from_millis_arg(value);
        }
        if arg == name
            && let Some(value) = args.next()
        {
            return delay_from_millis_arg(&value);
        }
    }
    Duration::ZERO
}

fn delay_from_millis_arg(value: &str) -> Duration {
    value
        .parse::<u64>()
        .map(Duration::from_millis)
        .unwrap_or(Duration::ZERO)
}

fn write_frame(writer: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn read_frame(reader: &mut impl Read) -> io::Result<Option<DapRequest>> {
    let Some(header) = read_header(reader)? else {
        return Ok(None);
    };
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

fn read_header(reader: &mut impl Read) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte)? {
            0 if bytes.is_empty() => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial DAP header",
                ));
            }
            _ => {
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n") {
                    return String::from_utf8(bytes).map(Some).map_err(io::Error::other);
                }
            }
        }
    }
}
