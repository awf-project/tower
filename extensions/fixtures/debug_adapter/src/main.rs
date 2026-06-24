use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn main() {
    match FixtureScenario::from_process_args_and_env(std::env::args().skip(1)) {
        Ok(Some(scenario)) => {
            emit_scripted_scenario(scenario, io::stdout()).unwrap_or_else(|err| {
                eprintln!("fixture_debug_adapter scenario failed: {err}");
                std::process::exit(1);
            });
            return;
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }

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
    next_stack_line: u64,
    suppress_continue_response: bool,
    continue_event_delay: Duration,
    eval_at_scenario: EvalAtScenario,
    replay_scenario: Option<FixtureScenario>,
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
            next_stack_line: 12,
            suppress_continue_response: continue_delay > Duration::ZERO,
            continue_event_delay: continue_event_delay_from_args(),
            eval_at_scenario: eval_at_scenario_from_args(),
            replay_scenario: None,
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
            DapCommand::ReverseContinue => self.reverse_continue(request),
            DapCommand::StepBack => self.step_back(request),
            DapCommand::SetDataBreakpoints => self.set_data_breakpoints(request),
            DapCommand::SeekReplay => self.seek_replay(request),
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
        self.replay_scenario = request
            .arguments
            .get("trace_id")
            .and_then(Value::as_str)
            .and_then(FixtureScenario::from_trace_id)
            .or_else(|| {
                request
                    .arguments
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .find_map(FixtureScenario::from_trace_id)
            });
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

        let response = self.response(request, Some(json!({ "allThreadsContinued": true })));

        if self.eval_at_scenario == EvalAtScenario::NoHitExit {
            if self.continue_event_delay > Duration::ZERO {
                write_frame(&mut self.output, &response)?;
                thread::sleep(self.continue_event_delay);
            }
            self.write_event("exited", json!({ "exitCode": 0 }))?;
            self.write_event("terminated", json!({}))?;
            return Ok((self.continue_event_delay == Duration::ZERO).then_some(response));
        }

        let (event, body) = if self.continue_count == 1 {
            ("stopped", json!({ "reason": "breakpoint", "threadId": 1 }))
        } else {
            ("terminated", json!({}))
        };
        let output = if self.continue_count == 1 {
            "fixture stopped at breakpoint with answer=42\n"
        } else {
            "fixture terminated after continue\n"
        };

        if self.continue_event_delay > Duration::ZERO {
            write_frame(&mut self.output, &response)?;
            thread::sleep(self.continue_event_delay);
            self.write_event(event, body)?;
            return Ok(None);
        }

        self.write_output_event(output)?;
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
                    "line": self.next_stack_line,
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

    fn reverse_continue(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        self.write_cleanup_output_event()?;
        if self.replay_scenario == Some(FixtureScenario::AdapterExited) {
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                std::process::exit(0);
            });
            return Ok(Some(self.empty_response(request)));
        }
        if self.replay_scenario == Some(FixtureScenario::NoPriorWrite) {
            self.write_event("terminated", json!({}))?;
            return Ok(Some(self.empty_response(request)));
        }
        self.next_stack_line = 12;
        self.write_event("stopped", json!({ "reason": "watchpoint", "threadId": 1 }))?;
        Ok(Some(self.empty_response(request)))
    }

    fn step_back(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        self.next_stack_line = 11;
        self.write_event("stopped", json!({ "reason": "step", "threadId": 1 }))?;
        Ok(Some(self.empty_response(request)))
    }

    fn set_data_breakpoints(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        Ok(Some(self.response(
            request,
            Some(json!({
                "breakpoints": [{
                    "id": "watch-1",
                    "verified": true
                }]
            })),
        )))
    }

    fn seek_replay(&mut self, request: DapRequest) -> io::Result<Option<DapResponse>> {
        self.next_stack_line = 12;
        self.write_event("stopped", json!({ "reason": "replay", "threadId": 1 }))?;
        Ok(Some(self.empty_response(request)))
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

    fn write_output_event(&mut self, output: &str) -> io::Result<()> {
        self.write_event("output", json!({ "category": "stdout", "output": output }))
    }

    fn write_cleanup_output_event(&mut self) -> io::Result<()> {
        let token = fixture_cleanup_token();
        if token.is_empty() {
            return Ok(());
        }
        let event = fixture_event("cleanup", &token, None, None, None, None);
        write_cleanup_side_channel(&event)?;
        let output = serde_json::to_string(&event).map_err(io::Error::other)?;
        self.write_output_event(&(output + "\n"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvalAtScenario {
    BreakpointThenTerminate,
    NoHitExit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixtureScenario {
    RecordOk,
    ReplayOpen,
    ReverseContinueStop,
    StepBackLine,
    WatchpointStop,
    NoPriorWrite,
    Timeout,
    AdapterExited,
}

impl FixtureScenario {
    fn from_process_args_and_env(
        args: impl IntoIterator<Item = String>,
    ) -> Result<Option<Self>, String> {
        scenario_name_from_args(args)
            .or_else(|| std::env::var("TOWER_DEBUG_FIXTURE_SCENARIO").ok())
            .map(|value| Self::from_name(&value))
            .transpose()
    }

    fn from_name(value: &str) -> Result<Self, String> {
        match value {
            "record_ok" => Ok(Self::RecordOk),
            "replay_open" => Ok(Self::ReplayOpen),
            "reverse_continue_stop" => Ok(Self::ReverseContinueStop),
            "step_back_line" => Ok(Self::StepBackLine),
            "watchpoint_stop" => Ok(Self::WatchpointStop),
            "no_prior_write" => Ok(Self::NoPriorWrite),
            "timeout" => Ok(Self::Timeout),
            "adapter_exited" => Ok(Self::AdapterExited),
            _ => Err(format!("unsupported fixture scenario: {value}")),
        }
    }

    fn from_trace_id(trace_id: &str) -> Option<Self> {
        let compact_trace_id = trace_id.replace(['_', '-'], "");
        [
            ("record_ok", Self::RecordOk),
            ("replay_open", Self::ReplayOpen),
            ("reverse_continue_stop", Self::ReverseContinueStop),
            ("step_back_line", Self::StepBackLine),
            ("watchpoint_stop", Self::WatchpointStop),
            ("no_prior_write", Self::NoPriorWrite),
            ("timeout", Self::Timeout),
            ("adapter_exited", Self::AdapterExited),
        ]
        .into_iter()
        .find_map(|(name, scenario)| {
            let compact_name = name.replace(['_', '-'], "");
            (trace_id.contains(name) || compact_trace_id.contains(&compact_name))
                .then_some(scenario)
        })
    }
}

#[derive(Debug, Clone, Serialize)]
struct FixtureEvent {
    event: String,
    token: String,
    session: Option<String>,
    trace: Option<String>,
    stop: Option<Value>,
    output: Option<String>,
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
    ReverseContinue,
    StepBack,
    SetDataBreakpoints,
    SeekReplay,
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
            DapCommand::ReverseContinue => "reverseContinue",
            DapCommand::StepBack => "stepBack",
            DapCommand::SetDataBreakpoints => "setDataBreakpoints",
            DapCommand::SeekReplay => "seekReplay",
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

fn eval_at_scenario_from_args() -> EvalAtScenario {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--eval-at-scenario=") {
            return eval_at_scenario_from_value(value);
        }
        if arg == "--eval-at-scenario"
            && let Some(value) = args.next()
        {
            return eval_at_scenario_from_value(&value);
        }
    }
    EvalAtScenario::BreakpointThenTerminate
}

fn eval_at_scenario_from_value(value: &str) -> EvalAtScenario {
    match value {
        "no-hit-exit" => EvalAtScenario::NoHitExit,
        _ => EvalAtScenario::BreakpointThenTerminate,
    }
}

fn scenario_name_from_args(args: impl IntoIterator<Item = String>) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--scenario=") {
            return Some(value.to_owned());
        }
        if arg == "--scenario" {
            return args.next();
        }
    }
    None
}

fn fixture_cleanup_token() -> String {
    std::env::var("TOWER_DEBUG_FIXTURE_CLEANUP_TOKEN")
        .ok()
        .or_else(|| token_from_args(std::env::args().skip(1)))
        .unwrap_or_default()
}

fn token_from_args(args: impl IntoIterator<Item = String>) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--token=") {
            return Some(value.to_owned());
        }
        if arg == "--token" {
            return args.next();
        }
    }
    None
}

fn emit_scripted_scenario(scenario: FixtureScenario, mut writer: impl Write) -> io::Result<()> {
    let token = fixture_cleanup_token();
    let mut events = match scenario {
        FixtureScenario::RecordOk => vec![fixture_event(
            "record",
            &token,
            None,
            Some("trace-record-ok"),
            None,
            Some("recorded fixture timeline"),
        )],
        FixtureScenario::ReplayOpen => vec![fixture_event(
            "replay",
            &token,
            Some("debug-fixture-replay"),
            Some("trace-replay-open"),
            Some(json!({ "sequence": 1, "reason": "replay", "line": 12 })),
            Some("opened replay"),
        )],
        FixtureScenario::ReverseContinueStop => vec![
            fixture_event(
                "stop",
                &token,
                Some("debug-fixture-replay"),
                Some("trace-reverse-continue"),
                Some(json!({ "sequence": 1, "reason": "replay", "line": 12 })),
                None,
            ),
            fixture_event(
                "stop",
                &token,
                Some("debug-fixture-replay"),
                Some("trace-reverse-continue"),
                Some(json!({ "sequence": 2, "reason": "watchpoint", "line": 12 })),
                None,
            ),
            fixture_event(
                "stop",
                &token,
                Some("debug-fixture-replay"),
                Some("trace-reverse-continue"),
                Some(json!({ "sequence": 3, "reason": "step", "line": 11 })),
                None,
            ),
        ],
        FixtureScenario::StepBackLine => vec![fixture_event(
            "stop",
            &token,
            Some("debug-fixture-replay"),
            Some("trace-step-back"),
            Some(json!({ "sequence": 1, "reason": "step", "line": 11 })),
            None,
        )],
        FixtureScenario::WatchpointStop => vec![fixture_event(
            "stop",
            &token,
            Some("debug-fixture-replay"),
            Some("trace-watchpoint"),
            Some(json!({ "sequence": 1, "reason": "watchpoint", "line": 12 })),
            Some("answer = 42"),
        )],
        FixtureScenario::NoPriorWrite => vec![fixture_event(
            "no_prior_write",
            &token,
            Some("debug-fixture-replay"),
            Some("trace-no-prior-write"),
            Some(json!({ "sequence": 1, "reason": "replay", "line": 12 })),
            Some("no prior write reached"),
        )],
        FixtureScenario::Timeout => vec![fixture_event(
            "timeout",
            &token,
            Some("debug-fixture-replay"),
            Some("trace-timeout"),
            None,
            Some("fixture timeout"),
        )],
        FixtureScenario::AdapterExited => vec![fixture_event(
            "adapter_exited",
            &token,
            Some("debug-fixture-replay"),
            Some("trace-adapter-exited"),
            None,
            Some("fixture adapter exited"),
        )],
    };
    events.push(fixture_event("cleanup", &token, None, None, None, None));

    for event in events {
        serde_json::to_writer(&mut writer, &event).map_err(io::Error::other)?;
        writeln!(writer)?;
    }
    writer.flush()
}

fn write_cleanup_side_channel(event: &FixtureEvent) -> io::Result<()> {
    if event.token.is_empty() {
        return Ok(());
    }
    let sanitized = event
        .token
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect::<String>();
    let path = std::env::temp_dir().join(format!("tower-debug-fixture-cleanup-{sanitized}.jsonl"));
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, event).map_err(io::Error::other)?;
    writeln!(file)
}

fn fixture_event(
    event: &str,
    token: &str,
    session: Option<&str>,
    trace: Option<&str>,
    stop: Option<Value>,
    output: Option<&str>,
) -> FixtureEvent {
    FixtureEvent {
        event: event.to_owned(),
        token: token.to_owned(),
        session: session.map(str::to_owned),
        trace: trace.map(str::to_owned),
        stop,
        output: output.map(str::to_owned),
    }
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
