#![forbid(unsafe_code)]

pub mod dap;
pub mod process;
mod protocol;
pub mod session;
pub mod tools;
pub mod types;

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dap::{DapClient, DapError, DapEvent, DapResponse, DapTransport, read_frame, write_frame};
use extension_protocol::{
    HostCall, InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response,
};
use extension_sidecar_harness::jsonrpc::HarnessError;
use extension_sidecar_harness::{
    HostCallIdAllocator, QueuedFrame, frame_from_envelope, host_call, send_error, send_response,
};
use protocol::{
    DebugAdapterConfig, DebugInitializeConfig, debug_not_initialized_result,
    debug_tool_declarations, debug_tool_unavailable_result,
};
use serde_json::{Map, Value, json};
use session::{DebugAdapterFactory, DebugAdapterSession, LaunchRequest, SessionManager};
use types::{
    DebugBreakpoint, DebugRuntimeError, DebugScope, DebugSessionState, DebugStackFrame, DebugStop,
    DebugThread, DebugVariable,
};

fn main() {
    serve_debug();
}

fn serve_debug() {
    let out = Arc::new(Mutex::new(io::stdout()));
    let mut lines = BufReader::new(io::stdin()).lines();
    let mut queued = VecDeque::new();
    let mut host_call_ids = HostCallIdAllocator::new(10_000);
    let mut config: Option<DebugInitializeConfig> = None;
    let mut initialized = false;
    let mut sessions: Option<SessionManager> = None;

    while let Some(frame) = next_frame(&mut lines, &mut queued) {
        match frame {
            QueuedFrame::Notification { .. } => {}
            QueuedFrame::Request { id, method, params } => match method.as_str() {
                "initialize" => {
                    handle_initialize(
                        &out,
                        &id,
                        params,
                        &mut config,
                        &mut sessions,
                        &mut initialized,
                    );
                }
                "invokeTool" => {
                    let mut io = DebugProtocolIo {
                        out: &out,
                        lines: &mut lines,
                        host_call_ids: &mut host_call_ids,
                        queued: &mut queued,
                    };
                    handle_invoke_tool(
                        &mut io,
                        &id,
                        params,
                        config.as_ref(),
                        sessions.as_ref(),
                        initialized,
                    );
                }
                "deliverEvent" => {
                    let _ = send_response(&out, &id, &Response::Ack);
                }
                "shutdown" => {
                    if let Some(sessions) = sessions.as_ref() {
                        sessions.shutdown_all();
                    }
                    let _ = send_response(&out, &id, &Response::Ack);
                    if let Ok(mut out) = out.lock() {
                        let _ = out.flush();
                    }
                    break;
                }
                other => {
                    let _ = send_error(&out, &id, -32601, &format!("unknown method: {other}"));
                }
            },
        }
    }
}

fn next_frame<R>(lines: &mut R, queued: &mut VecDeque<QueuedFrame>) -> Option<QueuedFrame>
where
    R: Iterator<Item = Result<String, io::Error>>,
{
    if let Some(frame) = queued.pop_front() {
        return Some(frame);
    }

    loop {
        let line = match lines.next()? {
            Ok(line) => line,
            Err(_) => return None,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let envelope: Value = match serde_json::from_str(line) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        let has_method = envelope.get("method").is_some();
        let is_response =
            !has_method && (envelope.get("result").is_some() || envelope.get("error").is_some());
        if is_response {
            continue;
        }
        if let Some(frame) = frame_from_envelope(envelope) {
            return Some(frame);
        }
    }
}

fn handle_initialize(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<Value>,
    params: Value,
    config: &mut Option<DebugInitializeConfig>,
    sessions: &mut Option<SessionManager>,
    initialized: &mut bool,
) {
    let init_params: InitParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            let _ = send_error(out, id, -32602, &format!("bad InitParams: {error}"));
            return;
        }
    };

    if init_params.protocol_version != PROTOCOL_VERSION {
        let _ = send_error(
            out,
            id,
            -32600,
            &format!(
                "protocol version mismatch: host={} extension={}",
                init_params.protocol_version, PROTOCOL_VERSION
            ),
        );
        return;
    }

    match DebugInitializeConfig::from_init_payload(init_params.extension_config) {
        Ok(parsed_config) => {
            *sessions = parsed_config.clone().map(|config| {
                SessionManager::new(config.clone(), Arc::new(RealDebugAdapterFactory { config }))
            });
            *config = parsed_config;
            *initialized = true;
            let result = InitResult {
                tools: debug_tool_declarations(config.as_ref()),
                events: Vec::new(),
                capabilities: Vec::new(),
            };
            let _ = send_response(out, id, &Response::Initialized(result));
        }
        Err(error) => {
            let _ = send_error(out, id, error.jsonrpc_code(), &error.jsonrpc_message());
        }
    }
}

fn handle_invoke_tool(
    io: &mut DebugProtocolIo<'_, impl Iterator<Item = Result<String, io::Error>>, impl Write>,
    id: &Option<Value>,
    params: Value,
    config: Option<&DebugInitializeConfig>,
    sessions: Option<&SessionManager>,
    initialized: bool,
) {
    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let tool_params = params.get("params").cloned().unwrap_or(Value::Null);

    if !initialized {
        let _ = send_response(
            io.out,
            id,
            &Response::ToolResult(debug_not_initialized_result()),
        );
        return;
    }

    let declared = debug_tool_declarations(config);
    if declared.iter().all(|tool| tool.name != tool_name) {
        let _ = send_response(
            io.out,
            id,
            &Response::Error(ProtocolError {
                code: -32601,
                message: format!("unknown tool: {tool_name}"),
                data: None,
            }),
        );
        return;
    }

    let _ = debug_host_call(
        io.out,
        io.lines,
        io.host_call_ids,
        "log",
        &HostCall::Log {
            level: "debug".to_owned(),
            msg: format!("debug tool {tool_name} entered dispatch"),
        },
        io.queued,
    );

    let result = sessions
        .map(|sessions| dispatch_debug_tool(tool_name, tool_params, sessions))
        .unwrap_or_else(|| Ok(debug_tool_unavailable_result(tool_name)));

    match result {
        Ok(result) => {
            let _ = send_response(io.out, id, &Response::ToolResult(result));
        }
        Err(error) => {
            let code = serde_json::to_value(&error.code)
                .ok()
                .and_then(|value| value.as_i64())
                .and_then(|code| i32::try_from(code).ok())
                .unwrap_or(-32603);
            let _ = send_error(io.out, id, code, &error.message);
        }
    }
}

fn dispatch_debug_tool(
    tool_name: &str,
    params: Value,
    sessions: &SessionManager,
) -> Result<Value, protocol::DebugToolError> {
    match tool_name {
        "launch" => tools::tower_debug_launch(params, sessions),
        "set_breakpoints" => tools::tower_debug_set_breakpoints(params, sessions),
        "continue" => tools::tower_debug_continue(params, sessions),
        "step" => tools::tower_debug_step(params, sessions),
        "pause" => tools::tower_debug_pause(params, sessions),
        "threads" => tools::tower_debug_threads(params, sessions),
        "stack" => tools::tower_debug_stack(params, sessions),
        "variables" => tools::tower_debug_variables(params, sessions),
        "evaluate" => tools::tower_debug_evaluate(params, sessions),
        "terminate" => tools::tower_debug_terminate(params, sessions),
        "disconnect" => tools::tower_debug_disconnect(params, sessions),
        "sessions" => tools::tower_debug_sessions(params, sessions),
        _ => Ok(debug_tool_unavailable_result(tool_name)),
    }
}

struct RealDebugAdapterFactory {
    config: DebugInitializeConfig,
}

impl DebugAdapterFactory for RealDebugAdapterFactory {
    fn start(
        &self,
        request: &LaunchRequest,
    ) -> Result<Box<dyn DebugAdapterSession>, DebugRuntimeError> {
        let adapter_config = self
            .config
            .languages
            .get(&request.language)
            .ok_or_else(|| {
                DebugRuntimeError::LaunchFailed(format!(
                    "no debug adapter configured for language {}",
                    request.language
                ))
            })?;
        let transport = ProcessDapTransport::spawn(adapter_config).map_err(|error| {
            DebugRuntimeError::LaunchFailed(format!(
                "failed to start debug adapter for language {}: {error}",
                request.language
            ))
        })?;
        Ok(Box::new(ProcessDebugAdapterSession {
            adapter_type: adapter_config.adapter_type.clone(),
            launch_defaults: adapter_config.launch.clone(),
            client: DapClient::new(Box::new(transport)),
            next_output_sequence: 1,
        }))
    }
}

struct ProcessDapTransport {
    stdin: ChildStdin,
    frames: Receiver<Result<Option<Value>, DapError>>,
    child: Child,
}

impl ProcessDapTransport {
    fn spawn(config: &DebugAdapterConfig) -> Result<Self, io::Error> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("debug adapter stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("debug adapter stdout unavailable"))?;
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_frame(&mut reader) {
                    Ok(Some(frame)) => {
                        if sender.send(Ok(Some(frame))).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = sender.send(Ok(None));
                        break;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            stdin,
            frames,
            child,
        })
    }
}

impl DapTransport for ProcessDapTransport {
    fn send(&mut self, message: &Value) -> Result<(), DapError> {
        write_frame(&mut self.stdin, message)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Value>, DapError> {
        self.frames
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => DapError::Timeout {
                    command: "recv".to_owned(),
                },
                mpsc::RecvTimeoutError::Disconnected => DapError::AdapterExited,
            })?
    }
}

impl Drop for ProcessDapTransport {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            terminate_child(&mut self.child);
            let _ = self.child.wait();
        }
    }
}

struct ProcessDebugAdapterSession {
    adapter_type: String,
    launch_defaults: Map<String, Value>,
    client: DapClient,
    next_output_sequence: u64,
}

impl DebugAdapterSession for ProcessDebugAdapterSession {
    fn initialize(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError> {
        let response = self.client.request(
            "initialize",
            json!({
                "adapterID": self.adapter_type,
                "pathFormat": "path",
                "linesStartAt1": true,
                "columnsStartAt1": true
            }),
            timeout,
        )?;
        ensure_success(response)
    }

    fn launch(
        &mut self,
        request: &LaunchRequest,
        timeout: Duration,
    ) -> Result<(), DebugRuntimeError> {
        let mut arguments = self.launch_defaults.clone();
        arguments.insert("program".to_owned(), Value::String(request.program.clone()));
        if let Some(cwd) = &request.cwd {
            arguments.insert("cwd".to_owned(), Value::String(cwd.clone()));
        }
        if !request.args.is_empty() {
            arguments.insert(
                "args".to_owned(),
                Value::Array(request.args.iter().cloned().map(Value::String).collect()),
            );
        }
        if !request.env.is_empty() {
            arguments.insert(
                "env".to_owned(),
                serde_json::to_value(&request.env).map_err(|error| {
                    DebugRuntimeError::LaunchFailed(format!(
                        "failed to serialize debug launch env: {error}"
                    ))
                })?,
            );
        }
        for (key, value) in &request.launch_overrides {
            arguments.insert(key.clone(), value.clone());
        }

        let response = self
            .client
            .request("launch", Value::Object(arguments), timeout)?;
        ensure_success(response)
    }

    fn set_breakpoints(
        &mut self,
        breakpoints: &[DebugBreakpoint],
        timeout: Duration,
    ) -> Result<Vec<DebugBreakpoint>, DebugRuntimeError> {
        if breakpoints.is_empty() {
            let response = self
                .client
                .request("configurationDone", json!({}), timeout)?;
            ensure_success(response)?;
            return Ok(Vec::new());
        }

        let mut by_source: BTreeMap<String, Vec<&DebugBreakpoint>> = BTreeMap::new();
        for breakpoint in breakpoints {
            by_source
                .entry(breakpoint.path.clone())
                .or_default()
                .push(breakpoint);
        }

        let mut verified = Vec::new();
        for (path, source_breakpoints) in by_source {
            let dap_breakpoints: Vec<Value> = source_breakpoints
                .iter()
                .map(|breakpoint| {
                    json!({
                        "line": breakpoint.line,
                        "condition": breakpoint.condition,
                        "hitCondition": breakpoint.hit_condition
                    })
                })
                .collect();
            let response = self.client.request(
                "setBreakpoints",
                json!({
                    "source": { "path": path },
                    "breakpoints": dap_breakpoints
                }),
                timeout,
            )?;
            ensure_success(response.clone())?;
            let response_breakpoints = response
                .body
                .get("breakpoints")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (index, breakpoint) in source_breakpoints.into_iter().enumerate() {
                let response_breakpoint = response_breakpoints.get(index).unwrap_or(&Value::Null);
                verified.push(DebugBreakpoint {
                    path: breakpoint.path.clone(),
                    line: breakpoint.line,
                    condition: breakpoint.condition.clone(),
                    hit_condition: breakpoint.hit_condition.clone(),
                    verified: response_breakpoint
                        .get("verified")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    verified_id: response_breakpoint.get("id").and_then(Value::as_u64),
                });
            }
        }
        Ok(verified)
    }

    fn continue_session(&mut self, timeout: Duration) -> Result<DebugStop, DebugRuntimeError> {
        self.resume("continue", json!({}), timeout)
    }

    fn step(
        &mut self,
        thread_id: Option<u64>,
        timeout: Duration,
    ) -> Result<DebugStop, DebugRuntimeError> {
        self.resume(
            "next",
            json!({ "threadId": thread_id.unwrap_or(1) }),
            timeout,
        )
    }

    fn pause(
        &mut self,
        thread_id: Option<u64>,
        timeout: Duration,
    ) -> Result<DebugStop, DebugRuntimeError> {
        self.resume(
            "pause",
            json!({ "threadId": thread_id.unwrap_or(1) }),
            timeout,
        )
    }

    fn threads(&mut self, timeout: Duration) -> Result<Vec<DebugThread>, DebugRuntimeError> {
        let response = self.client.request("threads", json!({}), timeout)?;
        ensure_success(response.clone())?;
        Ok(response
            .body
            .get("threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|thread| {
                Some(DebugThread {
                    id: thread.get("id")?.as_u64()?,
                    name: thread.get("name")?.as_str()?.to_owned(),
                })
            })
            .collect())
    }

    fn stack(
        &mut self,
        thread_id: u64,
        timeout: Duration,
    ) -> Result<Vec<DebugStackFrame>, DebugRuntimeError> {
        let response =
            self.client
                .request("stackTrace", json!({ "threadId": thread_id }), timeout)?;
        ensure_success(response.clone())?;
        Ok(response
            .body
            .get("stackFrames")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(stack_frame_from_value)
            .collect())
    }

    fn scopes(
        &mut self,
        frame_id: u64,
        timeout: Duration,
    ) -> Result<Vec<DebugScope>, DebugRuntimeError> {
        let response = self
            .client
            .request("scopes", json!({ "frameId": frame_id }), timeout)?;
        ensure_success(response.clone())?;
        Ok(response
            .body
            .get("scopes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|scope| {
                Some(DebugScope {
                    name: scope.get("name")?.as_str()?.to_owned(),
                    variables_reference: scope.get("variablesReference")?.as_u64()?,
                    expensive: scope
                        .get("expensive")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect())
    }

    fn variables(
        &mut self,
        variables_reference: u64,
        timeout: Duration,
    ) -> Result<Vec<DebugVariable>, DebugRuntimeError> {
        let response = self.client.request(
            "variables",
            json!({ "variablesReference": variables_reference }),
            timeout,
        )?;
        ensure_success(response.clone())?;
        Ok(response
            .body
            .get("variables")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(variable_from_value)
            .collect())
    }

    fn evaluate(
        &mut self,
        frame_id: u64,
        expression: &str,
        timeout: Duration,
    ) -> Result<DebugVariable, DebugRuntimeError> {
        let response = self.client.request(
            "evaluate",
            json!({ "frameId": frame_id, "expression": expression }),
            timeout,
        )?;
        ensure_success(response.clone())?;
        variable_from_value(&response.body).ok_or_else(|| {
            DebugRuntimeError::LaunchFailed(
                "debug adapter returned malformed evaluate result".into(),
            )
        })
    }

    fn terminate(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError> {
        let response = self.client.request("terminate", json!({}), timeout)?;
        ensure_success(response)
    }

    fn disconnect(&mut self, timeout: Duration) -> Result<(), DebugRuntimeError> {
        let response = self.client.request("disconnect", json!({}), timeout)?;
        ensure_success(response)
    }
}

impl ProcessDebugAdapterSession {
    fn resume(
        &mut self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<DebugStop, DebugRuntimeError> {
        let response = self.client.request(command, arguments, timeout)?;
        ensure_success(response)?;
        if let Some(event) = self
            .client
            .wait_for_event(&["stopped", "terminated"], timeout)?
        {
            return Ok(self.stop_from_event(event, timeout));
        }
        Err(DebugRuntimeError::DebugTimeout(format!(
            "debug adapter did not report a stop before {timeout:?}"
        )))
    }

    fn stop_from_event(&mut self, event: DapEvent, timeout: Duration) -> DebugStop {
        let output_since = self.drain_output_events();
        match event.event.as_str() {
            "stopped" => {
                let thread_id = event
                    .body
                    .get("threadId")
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                let reason = event
                    .body
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let top_frame = self
                    .stack(thread_id, timeout)
                    .ok()
                    .and_then(|mut frames| frames.drain(..).next());
                DebugStop {
                    state: DebugSessionState::Stopped,
                    reason,
                    thread_id: Some(thread_id),
                    top_frame,
                    hit_breakpoint_ids: Vec::new(),
                    timed_out: false,
                    output_since,
                }
            }
            "terminated" => DebugStop {
                state: DebugSessionState::Terminated,
                reason: Some("terminated".to_owned()),
                thread_id: None,
                top_frame: None,
                hit_breakpoint_ids: Vec::new(),
                timed_out: false,
                output_since,
            },
            _ => DebugStop {
                state: DebugSessionState::Running,
                reason: None,
                thread_id: None,
                top_frame: None,
                hit_breakpoint_ids: Vec::new(),
                timed_out: true,
                output_since,
            },
        }
    }

    fn drain_output_events(&mut self) -> Vec<types::DebugOutput> {
        self.client
            .drain_events()
            .into_iter()
            .filter(|event| event.event == "output")
            .filter_map(|event| {
                let text = event.body.get("output")?.as_str()?.to_owned();
                let output = types::DebugOutput {
                    sequence: self.next_output_sequence,
                    category: event
                        .body
                        .get("category")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    text,
                };
                self.next_output_sequence = self.next_output_sequence.saturating_add(1);
                Some(output)
            })
            .collect()
    }
}

fn ensure_success(response: DapResponse) -> Result<(), DebugRuntimeError> {
    if response.success {
        return Ok(());
    }
    Err(DebugRuntimeError::LaunchFailed(
        response
            .message
            .unwrap_or_else(|| format!("debug adapter command {} failed", response.command)),
    ))
}

fn stack_frame_from_value(frame: &Value) -> Option<DebugStackFrame> {
    Some(DebugStackFrame {
        id: frame.get("id")?.as_u64()?,
        name: frame.get("name")?.as_str()?.to_owned(),
        path: frame
            .get("source")
            .and_then(|source| source.get("path"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        line: frame.get("line")?.as_u64()?,
        column: frame.get("column")?.as_u64()?,
    })
}

fn variable_from_value(variable: &Value) -> Option<DebugVariable> {
    Some(DebugVariable {
        name: variable.get("name")?.as_str()?.to_owned(),
        value: variable.get("value")?.as_str()?.to_owned(),
        r#type: variable
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        variables_reference: variable
            .get("variablesReference")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

impl From<DapError> for DebugRuntimeError {
    fn from(error: DapError) -> Self {
        match error {
            DapError::Io(error) => DebugRuntimeError::LaunchFailed(error.to_string()),
            DapError::MalformedFrame(error) | DapError::MalformedMessage(error) => {
                DebugRuntimeError::LaunchFailed(error)
            }
            DapError::Timeout { command } => {
                DebugRuntimeError::DebugTimeout(format!("debug adapter timed out during {command}"))
            }
            DapError::AdapterExited => {
                DebugRuntimeError::AdapterExited("debug adapter exited".to_owned())
            }
        }
    }
}

#[cfg(unix)]
fn terminate_child(child: &mut Child) {
    let process_group_id = format!("-{}", child.id());
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg("--")
        .arg(&process_group_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::thread::sleep(Duration::from_millis(50));
    if matches!(child.try_wait(), Ok(None)) {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(&process_group_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
}

#[cfg(not(unix))]
fn terminate_child(child: &mut Child) {
    let _ = child.kill();
}

struct DebugProtocolIo<'a, R, W>
where
    R: Iterator<Item = Result<String, io::Error>>,
    W: Write,
{
    out: &'a Arc<Mutex<W>>,
    lines: &'a mut R,
    host_call_ids: &'a mut HostCallIdAllocator,
    queued: &'a mut VecDeque<QueuedFrame>,
}

fn debug_host_call<R>(
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    host_call_ids: &mut HostCallIdAllocator,
    method: &str,
    call: &HostCall,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Value, HarnessError>
where
    R: Iterator<Item = Result<String, io::Error>>,
{
    host_call(out, lines, host_call_ids, method, call, queued)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{
        DapClient, DapError, DapTransport, DebugAdapterSession, DebugBreakpoint,
        ProcessDebugAdapterSession,
    };

    struct ScriptedTransport {
        frames: VecDeque<Result<Option<Value>, DapError>>,
        sent: Arc<Mutex<Vec<Value>>>,
    }

    impl ScriptedTransport {
        fn new(frames: Vec<Result<Option<Value>, DapError>>) -> Self {
            Self {
                frames: frames.into(),
                sent: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn sent_messages(&self) -> Arc<Mutex<Vec<Value>>> {
            Arc::clone(&self.sent)
        }
    }

    impl DapTransport for ScriptedTransport {
        fn send(&mut self, message: &Value) -> Result<(), DapError> {
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Value>, DapError> {
            self.frames.pop_front().unwrap_or(Ok(None))
        }
    }

    #[test]
    fn process_debug_adapter_sets_all_breakpoints_for_a_source_in_one_dap_request() {
        let transport = ScriptedTransport::new(vec![Ok(Some(json!({
            "seq": 10,
            "type": "response",
            "request_seq": 1,
            "command": "setBreakpoints",
            "success": true,
            "body": {
                "breakpoints": [
                    { "verified": true, "id": 7 },
                    { "verified": true, "id": 8 }
                ]
            }
        })))]);
        let sent = transport.sent_messages();
        let mut session = process_session(transport);

        let breakpoints = session
            .set_breakpoints(
                &[
                    source_breakpoint("src/main.rs", 12),
                    source_breakpoint("src/main.rs", 24),
                ],
                Duration::from_millis(50),
            )
            .expect("setBreakpoints should succeed");

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["command"], "setBreakpoints");
        assert_eq!(sent[0]["arguments"]["source"]["path"], "src/main.rs");
        assert_eq!(
            sent[0]["arguments"]["breakpoints"],
            json!([
                { "line": 12, "condition": null, "hitCondition": null },
                { "line": 24, "condition": null, "hitCondition": null }
            ])
        );
        assert_eq!(
            breakpoints
                .iter()
                .map(|breakpoint| breakpoint.verified_id)
                .collect::<Vec<_>>(),
            vec![Some(7), Some(8)]
        );
    }

    #[test]
    fn process_debug_adapter_maps_output_events_into_stop_output_since() {
        let transport = ScriptedTransport::new(vec![
            Ok(Some(json!({
                "seq": 10,
                "type": "response",
                "request_seq": 1,
                "command": "continue",
                "success": true,
                "body": {}
            }))),
            Ok(Some(json!({
                "seq": 11,
                "type": "event",
                "event": "output",
                "body": { "category": "stdout", "output": "ready\n" }
            }))),
            Ok(Some(json!({
                "seq": 12,
                "type": "event",
                "event": "stopped",
                "body": { "reason": "breakpoint", "threadId": 1 }
            }))),
            Ok(Some(json!({
                "seq": 13,
                "type": "response",
                "request_seq": 2,
                "command": "stackTrace",
                "success": true,
                "body": {
                    "stackFrames": [{
                        "id": 99,
                        "name": "main",
                        "source": { "path": "src/main.rs" },
                        "line": 12,
                        "column": 5
                    }]
                }
            }))),
        ]);
        let mut session = process_session(transport);

        let stop = session
            .continue_session(Duration::from_millis(50))
            .expect("continue should stop after output event");

        assert_eq!(stop.output_since.len(), 1);
        assert_eq!(stop.output_since[0].sequence, 1);
        assert_eq!(stop.output_since[0].category.as_deref(), Some("stdout"));
        assert_eq!(stop.output_since[0].text, "ready\n");
        assert_eq!(stop.top_frame.expect("top frame").id, 99);
    }

    fn process_session(transport: ScriptedTransport) -> ProcessDebugAdapterSession {
        ProcessDebugAdapterSession {
            adapter_type: "fixture".to_owned(),
            launch_defaults: serde_json::Map::new(),
            client: DapClient::new(Box::new(transport)),
            next_output_sequence: 1,
        }
    }

    fn source_breakpoint(path: &str, line: u64) -> DebugBreakpoint {
        DebugBreakpoint {
            path: path.to_owned(),
            line,
            condition: None,
            hit_condition: None,
            verified: false,
            verified_id: None,
        }
    }
}
