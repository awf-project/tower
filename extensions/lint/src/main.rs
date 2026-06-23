#![forbid(unsafe_code)]

mod protocol;

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_engine::adapters::config::{self, LintConfig};
use extension_protocol::{
    Capability, HostCall, InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response,
    ToolDecl,
};
use lint_extension::config::RunnerLintConfig;
use lint_extension::diagnostics::{LintDiagnostic, severity_json};
use lint_extension::runner::{LintToolError, RunOutcome, RunRequest, run_linter};
use protocol::{CheckRequest, CheckResult, LintDiagnosticDto, LintToolErrorResponse, QueuedFrame};
use serde_json::Value;

const HOST_CALL_START_ID: u64 = 10_000;
const LINT_TIMEOUT: Duration = Duration::from_secs(2);

fn main() {
    serve_lint_check();
}

fn serve_lint_check() {
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let mut lines = BufReader::new(io::stdin()).lines();
    let mut next_hcall_id = HOST_CALL_START_ID;
    let mut queued = VecDeque::new();
    let workspace_root = workspace_root();

    while let Some(frame) = next_frame(&mut lines, &mut queued) {
        match frame {
            QueuedFrame::Notification { method, .. } if method.is_empty() => {}
            QueuedFrame::Notification { .. } => {}
            QueuedFrame::Request { id, method, params } => match method.as_str() {
                "initialize" => handle_initialize(&stdout, &id, params),
                "invokeTool" => handle_invoke_tool(
                    &stdout,
                    &id,
                    params,
                    &workspace_root,
                    &mut lines,
                    &mut next_hcall_id,
                    &mut queued,
                ),
                "deliverEvent" => {
                    protocol::send_response(&stdout, &id, &Response::Ack);
                }
                "shutdown" => {
                    protocol::send_response(&stdout, &id, &Response::Ack);
                    if let Ok(mut out) = stdout.lock() {
                        let _ = out.flush();
                    }
                    break;
                }
                other => {
                    protocol::send_error(&stdout, &id, -32601, &format!("unknown method: {other}"));
                }
            },
        }
    }
}

fn lint_init_result() -> InitResult {
    InitResult {
        tools: vec![ToolDecl {
            name: "check".to_owned(),
            description: "Run configured lint commands for one file or the indexed workspace."
                .to_owned(),
            schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to lint; omit to lint all configured files"}},"additionalProperties":false}"#.to_owned(),
        }],
        events: Vec::new(),
        capabilities: vec![Capability::ReadFile, Capability::ListFiles, Capability::Log],
    }
}

fn check_lint<R>(
    request: CheckRequest,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<CheckResult, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let config = config::load(workspace_root)
        .map_err(|error| format!("failed to load lint config: {error}"))?
        .lint;

    if config.is_empty() {
        return Ok(unsupported_result());
    }

    if let Some(path) = request.path {
        check_one(&path, &config, workspace_root, lines, out, next_id, queued)
    } else {
        check_workspace(&config, workspace_root, lines, out, next_id, queued)
    }
}

fn check_result_from_run_outcome(outcome: RunOutcome) -> CheckResult {
    let mut diagnostics = outcome
        .diagnostics
        .into_iter()
        .map(diagnostic_to_dto)
        .collect::<Vec<_>>();
    sort_diagnostics(&mut diagnostics);
    CheckResult {
        supported: outcome.supported,
        diagnostics,
        error: None,
    }
}

fn diagnostic_to_dto(diagnostic: LintDiagnostic) -> LintDiagnosticDto {
    let range = diagnostic.diagnostic.range;
    LintDiagnosticDto {
        path: diagnostic.path,
        line: range.start.line,
        character: range.start.character,
        end_line: range.end.line,
        end_character: range.end.character,
        severity: severity_json(diagnostic.diagnostic.severity).to_owned(),
        code: diagnostic.diagnostic.code,
        message: diagnostic.diagnostic.message,
        source: diagnostic.diagnostic.source,
    }
}

fn lint_tool_error_response(error: LintToolError) -> LintToolErrorResponse {
    let code = error.code().to_owned();
    let message = match error {
        LintToolError::MissingBinary { .. } => "lint command is unavailable".to_owned(),
        LintToolError::UnparseableOutput => "lint output could not be parsed".to_owned(),
        LintToolError::NonzeroExit { .. } => "lint command exited with errors".to_owned(),
        LintToolError::Timeout => "lint command timed out".to_owned(),
        LintToolError::InvalidConfig(message) => message,
    };
    LintToolErrorResponse { code, message }
}

fn next_frame<R>(lines: &mut R, queued: &mut VecDeque<QueuedFrame>) -> Option<QueuedFrame>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
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

        let envelope = match serde_json::from_str(line) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        if let Some(frame) = protocol::frame_from_envelope(envelope) {
            return Some(frame);
        }
    }
}

fn handle_initialize(out: &Arc<Mutex<impl Write>>, id: &Option<Value>, params: Value) {
    let init_params: InitParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            protocol::send_error(out, id, -32602, &format!("bad InitParams: {error}"));
            return;
        }
    };

    if init_params.protocol_version != PROTOCOL_VERSION {
        protocol::send_error(
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

    protocol::send_response(out, id, &Response::Initialized(lint_init_result()));
}

fn handle_invoke_tool<R>(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<Value>,
    params: Value,
    workspace_root: &Path,
    lines: &mut R,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let tool_params = params.get("params").cloned().unwrap_or(Value::Null);

    if tool_name != "check" {
        protocol::send_response(
            out,
            id,
            &Response::Error(ProtocolError {
                code: -32601,
                message: format!("unknown tool: {tool_name}"),
                data: None,
            }),
        );
        return;
    }

    let request: CheckRequest = match serde_json::from_value(tool_params) {
        Ok(request) => request,
        Err(error) => {
            protocol::send_response(
                out,
                id,
                &Response::Error(ProtocolError {
                    code: -32602,
                    message: format!("bad CheckRequest: {error}"),
                    data: None,
                }),
            );
            return;
        }
    };

    match check_lint(request, workspace_root, lines, out, next_id, queued) {
        Ok(result) => protocol::send_response(
            out,
            id,
            &Response::ToolResult(serde_json::to_value(result).expect("serialize CheckResult")),
        ),
        Err(message) => protocol::send_response(
            out,
            id,
            &Response::Error(ProtocolError {
                code: -32000,
                message,
                data: None,
            }),
        ),
    }
}

fn check_one<R>(
    path: &str,
    config: &LintConfig,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<CheckResult, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let Some((_, command)) = config.command_for_path(path) else {
        return Ok(unsupported_result());
    };
    let runner_config = RunnerLintConfig::from(command);
    let stdin_content = read_file(path, lines, out, next_id, queued)?;
    run_configured_linter(&runner_config, workspace_root, path, Some(&stdin_content))
}

fn check_workspace<R>(
    config: &LintConfig,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<CheckResult, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let mut paths = list_files(lines, out, next_id, queued)?;
    paths.sort();
    let mut diagnostics = Vec::new();
    let mut ran_linter = false;

    for path in paths {
        let Some((_, command)) = config.command_for_path(&path) else {
            continue;
        };
        let runner_config = RunnerLintConfig::from(command);
        let stdin_content = read_file(&path, lines, out, next_id, queued)?;
        let result =
            run_configured_linter(&runner_config, workspace_root, &path, Some(&stdin_content))?;
        if result.error.is_some() {
            return Ok(result);
        }
        ran_linter = ran_linter || result.supported;
        diagnostics.extend(result.diagnostics);
    }

    sort_diagnostics(&mut diagnostics);
    Ok(CheckResult {
        supported: ran_linter,
        diagnostics,
        error: None,
    })
}

fn run_configured_linter(
    config: &RunnerLintConfig,
    workspace_root: &Path,
    target_path: &str,
    stdin_content: Option<&str>,
) -> Result<CheckResult, String> {
    match run_linter(RunRequest {
        config,
        workspace_root,
        target_path: Some(target_path),
        stdin_content,
        timeout: LINT_TIMEOUT,
    }) {
        Ok(outcome) => Ok(check_result_from_run_outcome(outcome)),
        Err(error) => Ok(CheckResult {
            supported: false,
            diagnostics: Vec::new(),
            error: Some(lint_tool_error_response(error)),
        }),
    }
}

fn read_file<R>(
    path: &str,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<String, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let value = protocol::host_call(
        out,
        lines,
        next_id,
        "workspace/readFile",
        &HostCall::ReadFile {
            path: path.to_owned(),
        },
        queued,
    )?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("unexpected readFile response: {value}"))
}

fn list_files<R>(
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Vec<String>, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let value = protocol::host_call(
        out,
        lines,
        next_id,
        "workspace/listFiles",
        &HostCall::ListFiles,
        queued,
    )?;
    let Some(files) = value.as_array() else {
        return Err(format!("unexpected listFiles response: {value}"));
    };

    Ok(files
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

fn unsupported_result() -> CheckResult {
    CheckResult {
        supported: false,
        diagnostics: Vec::new(),
        error: None,
    }
}

fn sort_diagnostics(diagnostics: &mut [LintDiagnosticDto]) {
    diagnostics.sort_by(|left, right| {
        (
            left.path.as_str(),
            left.line,
            left.character,
            left.message.as_str(),
        )
            .cmp(&(
                right.path.as_str(),
                right.line,
                right.character,
                right.message.as_str(),
            ))
    });
}

fn workspace_root() -> PathBuf {
    std::env::var_os("TOWER_WORKSPACE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}
