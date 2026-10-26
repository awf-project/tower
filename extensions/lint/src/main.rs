#![forbid(unsafe_code)]

use std::collections::{BTreeSet, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core_engine::adapters::config::{self, LintConfig};
use core_engine::domain::mutation::compute_content_version;
use extension_protocol::messages::ApplyEditsHostCallTextEdit;
use extension_protocol::{
    Capability, HostCall, InitParams, InitResult, PROTOCOL_VERSION, ProtocolError, Response,
    ToolDecl,
};
use extension_sidecar_harness::{frame_from_envelope, send_error, send_response};
use lint_extension::config::RunnerLintConfig;
use lint_extension::diagnostics::{LintDiagnostic, severity_json};
use lint_extension::fixes::{FixApplicability, LintByteEdit, LintFix};
use lint_extension::protocol;
use lint_extension::runner::{LintToolError, RunOutcome, RunRequest, run_linter, run_linter_fixes};
use protocol::{
    CheckRequest, CheckResult, FixPreviewDto, FixPreviewEditDto, FixRequest, FixResult,
    FixToolErrorResponse, LintDiagnosticDto, LintToolErrorResponse, QueuedFrame, SkippedFixDto,
    SkippedFixReason,
};
use serde::Deserialize;
use serde_json::{Value, json};

const HOST_CALL_START_ID: u64 = 10_000;

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
                    let _ = send_response(&stdout, &id, &Response::Ack);
                }
                "shutdown" => {
                    let _ = send_response(&stdout, &id, &Response::Ack);
                    if let Ok(mut out) = stdout.lock() {
                        let _ = out.flush();
                    }
                    break;
                }
                other => {
                    let _ = send_error(&stdout, &id, -32601, &format!("unknown method: {other}"));
                }
            },
        }
    }
}

fn lint_init_result() -> InitResult {
    InitResult {
        tools: vec![
            ToolDecl {
                name: "check".to_owned(),
                description: "Run configured lint commands for one file or the indexed workspace."
                    .to_owned(),
                schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to lint; omit to lint all configured files"}},"additionalProperties":false}"#.to_owned(),
            },
            ToolDecl {
                name: "fix".to_owned(),
                description: "Apply structured lint fixes for one file or the indexed workspace."
                    .to_owned(),
                schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to fix; omit to inspect all configured files"},"unsafe":{"type":"boolean","description":"Apply fixes with unsafe or unknown applicability"},"dry_run":{"type":"boolean","description":"Preview fixes without writing files"}},"additionalProperties":false}"#.to_owned(),
            },
        ],
        events: Vec::new(),
        capabilities: vec![
            Capability::ReadFile,
            Capability::ListFiles,
            Capability::RequestApplyEdits,
            Capability::Log,
        ],
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

    let timeout_secs = if let Some(path) = request.path.as_deref() {
        config
            .command_for_path(path)
            .and_then(|(_, command)| command.timeout_secs)
    } else {
        config
            .commands
            .values()
            .filter_map(|command| command.timeout_secs)
            .min()
    };
    let deadline = match OperationDeadline::from_timeout_secs(timeout_secs) {
        Ok(deadline) => deadline,
        Err(error) => {
            return Ok(CheckResult {
                supported: false,
                diagnostics: Vec::new(),
                error: Some(LintToolErrorResponse {
                    code: "lint_invalid_config".to_owned(),
                    message: error.message,
                    detail: None,
                }),
            });
        }
    };

    if let Some(path) = request.path {
        check_one_with_deadline(
            &path,
            &config,
            workspace_root,
            lines,
            out,
            next_id,
            queued,
            deadline,
        )
    } else {
        check_workspace_with_deadline(
            &config,
            workspace_root,
            lines,
            out,
            next_id,
            queued,
            deadline,
        )
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
    let (message, detail) = match error {
        LintToolError::MissingBinary { .. } => ("lint command is unavailable".to_owned(), None),
        LintToolError::UnparseableOutput => ("lint output could not be parsed".to_owned(), None),
        LintToolError::NonzeroExit { .. } => ("lint command exited with errors".to_owned(), None),
        LintToolError::Timeout { detail } => ("lint command timed out".to_owned(), Some(detail)),
        LintToolError::InvalidConfig(message) => (message, None),
    };
    LintToolErrorResponse {
        code,
        message,
        detail,
    }
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
        if let Some(frame) = frame_from_envelope(envelope) {
            return Some(frame);
        }
    }
}

fn handle_initialize(out: &Arc<Mutex<impl Write>>, id: &Option<Value>, params: Value) {
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

    let _ = send_response(out, id, &Response::Initialized(lint_init_result()));
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

    match tool_name {
        "check" => invoke_check_tool(out, id, tool_params, workspace_root, lines, next_id, queued),
        "fix" => invoke_fix_tool(out, id, tool_params, workspace_root, lines, next_id, queued),
        _ => {
            let _ = send_response(
                out,
                id,
                &Response::Error(ProtocolError {
                    code: -32601,
                    message: format!("unknown tool: {tool_name}"),
                    data: None,
                }),
            );
        }
    }
}

fn invoke_check_tool<R>(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<Value>,
    tool_params: Value,
    workspace_root: &Path,
    lines: &mut R,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let request: CheckRequest = match serde_json::from_value(tool_params) {
        Ok(request) => request,
        Err(error) => {
            let _ = send_response(
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
        Ok(result) => {
            let _ = send_response(
                out,
                id,
                &Response::ToolResult(serde_json::to_value(result).expect("serialize CheckResult")),
            );
        }
        Err(message) => {
            let _ = send_response(
                out,
                id,
                &Response::Error(ProtocolError {
                    code: -32000,
                    message,
                    data: None,
                }),
            );
        }
    }
}

fn invoke_fix_tool<R>(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<Value>,
    tool_params: Value,
    workspace_root: &Path,
    lines: &mut R,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let request: FixRequest = match serde_json::from_value(tool_params) {
        Ok(request) => request,
        Err(error) => {
            let _ = send_response(
                out,
                id,
                &Response::ToolResult(fix_error_result(
                    "lint_fix_invalid_request",
                    format!("bad FixRequest: {error}"),
                    None,
                )),
            );
            return;
        }
    };

    match fix_lint(request, workspace_root, lines, out, next_id, queued) {
        Ok(result) => {
            let _ = send_response(
                out,
                id,
                &Response::ToolResult(serde_json::to_value(result).expect("serialize FixResult")),
            );
        }
        Err(error) => {
            let _ = send_response(
                out,
                id,
                &Response::ToolResult(fix_error_result(error.code, error.message, error.detail)),
            );
        }
    }
}

#[derive(Clone, Debug)]
struct FixFailure {
    code: &'static str,
    message: String,
    detail: Option<extension_protocol::TimeoutDetail>,
}

#[derive(Clone, Copy, Debug)]
struct OperationDeadline {
    deadline: Option<Instant>,
    budget_ms: Option<u64>,
}

impl OperationDeadline {
    fn from_timeout_secs(timeout_secs: Option<u64>) -> Result<Self, FixFailure> {
        let Some(seconds) = timeout_secs else {
            return Ok(Self {
                deadline: None,
                budget_ms: None,
            });
        };
        let timeout = Duration::from_secs(seconds);
        let budget_ms = seconds
            .checked_mul(1_000)
            .ok_or_else(|| fix_unavailable("lint timeout is too large"))?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| fix_unavailable("lint timeout deadline is not representable"))?;
        Ok(Self {
            deadline: Some(deadline),
            budget_ms: Some(budget_ms),
        })
    }

    fn remaining(self, operation: &'static str) -> Result<Option<Duration>, FixFailure> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(self.timeout_failure(operation));
        }
        Ok(Some(remaining))
    }

    fn map_runner_error(self, operation: &'static str, error: LintToolError) -> FixFailure {
        if matches!(error, LintToolError::Timeout { .. }) {
            self.timeout_failure(operation)
        } else {
            fix_unavailable(lint_fix_unavailable_message(error))
        }
    }

    fn timeout_failure(self, operation: &'static str) -> FixFailure {
        FixFailure {
            code: "lint_timeout",
            message: "lint command timed out".to_owned(),
            detail: Some(extension_protocol::TimeoutDetail {
                operation: operation.to_owned(),
                layer: "subprocess".to_owned(),
                budget_ms: self
                    .budget_ms
                    .expect("an expired fix deadline has an original budget"),
            }),
        }
    }
}

#[derive(Clone, Debug)]
struct FileFixPlan {
    path: String,
    expected_version: String,
    fixes: Vec<LintFix>,
}

#[derive(Clone, Debug)]
struct PreparedFix {
    fix: LintFix,
    accepted_edits: Vec<LintByteEdit>,
}

#[derive(Clone, Copy, Debug)]
struct FixApplyOptions {
    unsafe_fixes: bool,
    dry_run: bool,
}

struct FixApplyAccum<'a> {
    result: &'a mut FixResult,
    changed_files: &'a mut BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ApplyEditsResultDto {
    applied: Vec<FixPreviewEditDto>,
    skipped: Vec<SkippedEditDto>,
    new_version: Option<String>,
    preview: Option<ApplyEditsPreviewDto>,
}

#[derive(Clone, Debug, Deserialize)]
struct ApplyEditsPreviewDto {
    edits: Vec<FixPreviewEditDto>,
    preview_content: String,
}

#[derive(Clone, Debug, Deserialize)]
struct SkippedEditDto {
    edit: FixPreviewEditDto,
}

fn fix_lint<R>(
    request: FixRequest,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<FixResult, FixFailure>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    if let Some(path) = request.path.as_deref() {
        validate_fix_path(path)?;
    }

    let config = config::load(workspace_root)
        .map_err(|error| fix_unavailable(format!("failed to load lint config: {error}")))?
        .lint;
    if config.is_empty() {
        return Err(fix_unavailable("missing lint configuration"));
    }

    let fix_deadline = if let Some(path) = request.path.as_deref() {
        config
            .command_for_path(path)
            .and_then(|(_, command)| command.timeout_secs)
    } else {
        config
            .commands
            .values()
            .filter_map(|command| command.timeout_secs)
            .min()
    };
    let fix_deadline = OperationDeadline::from_timeout_secs(fix_deadline)?;

    let mut result = FixResult::default();
    let mut changed_files = BTreeSet::new();

    // Preparation failures must precede every write, including workspace batches.
    // Applying the prepared plans still uses per-file CAS rather than a batch transaction.
    let mut plans = Vec::new();
    for path in fix_target_paths(&request, lines, out, next_id, queued)? {
        if let Some(plan) = collect_fix_plan(
            path,
            &config,
            workspace_root,
            lines,
            out,
            next_id,
            queued,
            fix_deadline,
        )? {
            plans.push(plan);
        }
    }
    for plan in plans {
        apply_fix_plan(
            plan,
            FixApplyOptions {
                unsafe_fixes: request.unsafe_fixes,
                dry_run: request.dry_run,
            },
            lines,
            out,
            next_id,
            queued,
            FixApplyAccum {
                result: &mut result,
                changed_files: &mut changed_files,
            },
        )?;
    }

    result.files_changed = changed_files.len();
    if !request.dry_run && result.files_changed > 0 {
        result.remaining_diagnostics = run_follow_up_check(
            request.path,
            &config,
            workspace_root,
            lines,
            out,
            next_id,
            queued,
            fix_deadline,
        )?;
    }
    sort_diagnostics(&mut result.remaining_diagnostics);
    Ok(result)
}

fn fix_target_paths<R>(
    request: &FixRequest,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Vec<String>, FixFailure>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    if let Some(path) = request.path.as_ref() {
        return Ok(vec![path.clone()]);
    }

    let mut paths = list_files(lines, out, next_id, queued).map_err(fix_apply_failed)?;
    paths.sort();
    Ok(paths)
}

#[allow(clippy::too_many_arguments)]
fn collect_fix_plan<R>(
    path: String,
    config: &LintConfig,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
    deadline: OperationDeadline,
) -> Result<Option<FileFixPlan>, FixFailure>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let Some((_, command)) = config.command_for_path(&path) else {
        return Ok(None);
    };
    let content = read_file(&path, lines, out, next_id, queued).map_err(fix_apply_failed)?;
    let runner_config = RunnerLintConfig::from(command);
    let outcome = run_linter_fixes(RunRequest {
        config: &runner_config,
        workspace_root,
        target_path: Some(&path),
        stdin_content: Some(&content),
        timeout: deadline.remaining("fix")?,
    })
    .map_err(|error| deadline.map_runner_error("fix", error))?;

    Ok(outcome.supported.then(|| FileFixPlan {
        path,
        expected_version: compute_content_version(content.as_bytes()),
        fixes: outcome.fixes,
    }))
}

fn apply_fix_plan<R>(
    plan: FileFixPlan,
    options: FixApplyOptions,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
    accum: FixApplyAccum<'_>,
) -> Result<(), FixFailure>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let (prepared, mut skipped) = prepare_fixes(plan.fixes, options.unsafe_fixes, &plan.path);
    accum.result.fixes_skipped.append(&mut skipped);
    if prepared.is_empty() {
        return Ok(());
    }

    let edits = prepared
        .iter()
        .flat_map(|prepared| prepared.accepted_edits.iter())
        .map(|edit| ApplyEditsHostCallTextEdit {
            start_byte: edit.start_byte,
            end_byte: edit.end_byte,
            replacement: edit.replacement.clone(),
        })
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return Ok(());
    }

    let value = match protocol::host_call(
        out,
        lines,
        next_id,
        "workspace/applyEdits",
        &HostCall::RequestApplyEdits {
            path: plan.path.clone(),
            expected_version: plan.expected_version,
            edits,
            dry_run: options.dry_run,
        },
        queued,
    ) {
        Ok(value) => value,
        Err(message) if message.starts_with("precondition_failed:") => {
            skip_prepared(
                &mut accum.result.fixes_skipped,
                &prepared,
                SkippedFixReason::CasConflict,
            );
            return Ok(());
        }
        Err(message) if message.starts_with("invalid_range:") => {
            skip_prepared(
                &mut accum.result.fixes_skipped,
                &prepared,
                SkippedFixReason::InvalidRange,
            );
            return Ok(());
        }
        Err(message) => return Err(fix_apply_failed(message)),
    };

    let applied = serde_json::from_value::<ApplyEditsResultDto>(value).map_err(|error| {
        fix_apply_failed(format!("malformed workspace/applyEdits response: {error}"))
    })?;

    if applied.new_version.is_some() {
        accum.changed_files.insert(plan.path.clone());
    }
    count_applied_fixes(&mut accum.result.fixes_applied, &prepared, &applied.applied);
    append_host_skips(&mut accum.result.fixes_skipped, &prepared, &applied.skipped);

    if options.dry_run
        && let Some(preview) = applied.preview
    {
        accum.result.previews.push(FixPreviewDto {
            path: plan.path,
            edits: preview.edits,
            preview_content: preview.preview_content,
        });
    }

    Ok(())
}

fn prepare_fixes(
    fixes: Vec<LintFix>,
    unsafe_fixes: bool,
    target_path: &str,
) -> (Vec<PreparedFix>, Vec<SkippedFixDto>) {
    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    let mut accepted: Vec<LintByteEdit> = Vec::new();

    for fix in fixes {
        if fix.path != target_path {
            skipped.push(skipped_fix(&fix, SkippedFixReason::Unsupported, true));
            continue;
        }

        match fix.applicability {
            FixApplicability::Unsupported => {
                skipped.push(skipped_fix(&fix, SkippedFixReason::Unsupported, false));
            }
            FixApplicability::Unsafe if !unsafe_fixes => {
                skipped.push(skipped_fix(&fix, SkippedFixReason::Unsafe, true));
            }
            FixApplicability::Safe | FixApplicability::Unsafe => {
                let conflicts_with_accepted = fix.edits.iter().any(|edit| {
                    accepted
                        .iter()
                        .any(|existing| edits_conflict(existing, edit))
                });
                let conflicts_within_fix = fix.edits.iter().enumerate().any(|(index, edit)| {
                    fix.edits
                        .iter()
                        .skip(index + 1)
                        .any(|other| edits_conflict(edit, other))
                });

                if conflicts_with_accepted || conflicts_within_fix {
                    skipped.push(skipped_fix(&fix, SkippedFixReason::Conflict, true));
                } else if !fix.edits.is_empty() {
                    let accepted_edits = fix.edits.clone();
                    accepted.extend(accepted_edits.clone());
                    prepared.push(PreparedFix {
                        fix,
                        accepted_edits,
                    });
                }
            }
        }
    }

    (prepared, skipped)
}

fn edits_conflict(left: &LintByteEdit, right: &LintByteEdit) -> bool {
    left.start_byte < right.end_byte && right.start_byte < left.end_byte
}

fn count_applied_fixes(
    fixes_applied: &mut usize,
    prepared: &[PreparedFix],
    applied: &[FixPreviewEditDto],
) {
    for prepared in prepared {
        if prepared.accepted_edits.iter().any(|edit| {
            applied
                .iter()
                .any(|applied| preview_edit_matches(edit, applied))
        }) {
            *fixes_applied += 1;
        }
    }
}

fn append_host_skips(
    fixes_skipped: &mut Vec<SkippedFixDto>,
    prepared: &[PreparedFix],
    skipped: &[SkippedEditDto],
) {
    for skipped_edit in skipped {
        let Some(prepared) = prepared.iter().find(|prepared| {
            prepared
                .accepted_edits
                .iter()
                .any(|edit| preview_edit_matches(edit, &skipped_edit.edit))
        }) else {
            continue;
        };
        fixes_skipped.push(skipped_fix(&prepared.fix, SkippedFixReason::Conflict, true));
    }
}

fn skip_prepared(
    fixes_skipped: &mut Vec<SkippedFixDto>,
    prepared: &[PreparedFix],
    reason: SkippedFixReason,
) {
    for prepared in prepared {
        fixes_skipped.push(skipped_fix(&prepared.fix, reason.clone(), true));
    }
}

fn preview_edit_matches(edit: &LintByteEdit, preview: &FixPreviewEditDto) -> bool {
    edit.start_byte == preview.start_byte
        && edit.end_byte == preview.end_byte
        && edit.replacement == preview.replacement
}

fn skipped_fix(fix: &LintFix, reason: SkippedFixReason, supported_fix: bool) -> SkippedFixDto {
    SkippedFixDto {
        path: fix.path.clone(),
        reason,
        diagnostic: Some(diagnostic_to_dto(fix.diagnostic.clone())),
        supported_fix,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_follow_up_check<R>(
    path: Option<String>,
    config: &LintConfig,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
    deadline: OperationDeadline,
) -> Result<Vec<LintDiagnosticDto>, FixFailure>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let check = if let Some(path) = path {
        check_one_with_timeout(
            &path,
            config,
            workspace_root,
            lines,
            out,
            next_id,
            queued,
            deadline.remaining("fix")?,
        )
    } else {
        check_workspace_with_deadline(
            config,
            workspace_root,
            lines,
            out,
            next_id,
            queued,
            deadline,
        )
    };

    map_follow_up_check(check).map_err(|failure| {
        if failure.code == "lint_timeout" {
            deadline.timeout_failure("fix")
        } else {
            failure
        }
    })
}

fn map_follow_up_check(
    check: Result<CheckResult, String>,
) -> Result<Vec<LintDiagnosticDto>, FixFailure> {
    let check = check.map_err(follow_up_failed)?;
    if let Some(error) = check.error {
        if error.code == "lint_timeout" && error.detail.is_some() {
            return Err(FixFailure {
                code: "lint_timeout",
                message: "lint command timed out".to_owned(),
                detail: error.detail,
            });
        }
        return Err(follow_up_failed(format!(
            "{}: {}",
            error.code, error.message
        )));
    }

    Ok(check.diagnostics)
}

fn follow_up_failed(cause: impl Into<String>) -> FixFailure {
    fix_apply_failed(format!(
        "{}; edits were applied before the follow-up lint check failed",
        cause.into()
    ))
}

fn validate_fix_path(path: &str) -> Result<(), FixFailure> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(fix_invalid_request("invalid path"));
    }
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(fix_invalid_request("invalid path"));
    }
    Ok(())
}

fn lint_fix_unavailable_message(error: LintToolError) -> String {
    match error {
        LintToolError::MissingBinary { .. } => "lint command is unavailable".to_owned(),
        LintToolError::UnparseableOutput => "lint output could not be parsed".to_owned(),
        LintToolError::NonzeroExit { .. } => "lint command exited with errors".to_owned(),
        LintToolError::Timeout { .. } => "lint command timed out".to_owned(),
        LintToolError::InvalidConfig(message) => message,
    }
}

fn fix_invalid_request(message: impl Into<String>) -> FixFailure {
    FixFailure {
        code: "lint_fix_invalid_request",
        message: message.into(),
        detail: None,
    }
}

fn fix_unavailable(message: impl Into<String>) -> FixFailure {
    FixFailure {
        code: "lint_fix_unavailable",
        message: message.into(),
        detail: None,
    }
}

fn fix_apply_failed(message: impl Into<String>) -> FixFailure {
    FixFailure {
        code: "lint_fix_apply_failed",
        message: message.into(),
        detail: None,
    }
}

fn fix_error_result(
    code: &'static str,
    message: impl Into<String>,
    detail: Option<extension_protocol::TimeoutDetail>,
) -> Value {
    let error = FixToolErrorResponse {
        code: code.to_owned(),
        message: message.into(),
        detail,
    };
    json!({ "error": error })
}

#[allow(clippy::too_many_arguments)]
fn check_one_with_deadline<R>(
    path: &str,
    config: &LintConfig,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
    deadline: OperationDeadline,
) -> Result<CheckResult, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let timeout = match deadline.remaining("check") {
        Ok(timeout) => timeout,
        Err(failure) => return Ok(check_timeout_result(failure)),
    };
    let mut result = check_one_with_timeout(
        path,
        config,
        workspace_root,
        lines,
        out,
        next_id,
        queued,
        timeout,
    )?;
    normalize_check_timeout(&mut result, deadline);
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn check_one_with_timeout<R>(
    path: &str,
    config: &LintConfig,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
    timeout_override: Option<Duration>,
) -> Result<CheckResult, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let Some((_, command)) = config.command_for_path(path) else {
        return Ok(unsupported_result());
    };
    let runner_config = RunnerLintConfig::from(command);
    let stdin_content = read_file(path, lines, out, next_id, queued)?;
    run_configured_linter(
        &runner_config,
        workspace_root,
        path,
        Some(&stdin_content),
        timeout_override.or_else(|| runner_config.timeout_secs.map(Duration::from_secs)),
    )
}

#[allow(clippy::too_many_arguments)]
fn check_workspace_with_deadline<R>(
    config: &LintConfig,
    workspace_root: &Path,
    lines: &mut R,
    out: &Arc<Mutex<impl Write>>,
    next_id: &mut u64,
    queued: &mut VecDeque<QueuedFrame>,
    deadline: OperationDeadline,
) -> Result<CheckResult, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let mut paths = list_files(lines, out, next_id, queued)?;
    paths.sort();
    let mut diagnostics = Vec::new();
    let mut ran_linter = false;

    for path in paths {
        if config.command_for_path(&path).is_none() {
            continue;
        }
        let result = check_one_with_deadline(
            &path,
            config,
            workspace_root,
            lines,
            out,
            next_id,
            queued,
            deadline,
        )?;
        if result.error.is_some() {
            return Ok(result);
        }
        ran_linter |= result.supported;
        diagnostics.extend(result.diagnostics);
    }

    sort_diagnostics(&mut diagnostics);
    Ok(CheckResult {
        supported: ran_linter,
        diagnostics,
        error: None,
    })
}

fn normalize_check_timeout(result: &mut CheckResult, deadline: OperationDeadline) {
    if result
        .error
        .as_ref()
        .is_some_and(|error| error.code == "lint_timeout")
    {
        *result = check_timeout_result(deadline.timeout_failure("check"));
    }
}

fn check_timeout_result(failure: FixFailure) -> CheckResult {
    CheckResult {
        supported: false,
        diagnostics: Vec::new(),
        error: Some(LintToolErrorResponse {
            code: failure.code.to_owned(),
            message: failure.message,
            detail: failure.detail,
        }),
    }
}

fn run_configured_linter(
    config: &RunnerLintConfig,
    workspace_root: &Path,
    target_path: &str,
    stdin_content: Option<&str>,
    timeout: Option<Duration>,
) -> Result<CheckResult, String> {
    match run_linter(RunRequest {
        config,
        workspace_root,
        target_path: Some(target_path),
        stdin_content,
        timeout,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(message: &str) -> LintDiagnosticDto {
        LintDiagnosticDto {
            path: "src/lib.rs".to_owned(),
            line: 1,
            character: 2,
            end_line: 1,
            end_character: 3,
            severity: "warning".to_owned(),
            code: Some("fixture".to_owned()),
            message: message.to_owned(),
            source: Some("fixture-lint".to_owned()),
        }
    }

    #[test]
    fn follow_up_check_preserves_successful_diagnostics() {
        let expected = vec![diagnostic("remaining diagnostic")];
        let actual = map_follow_up_check(Ok(CheckResult {
            supported: true,
            diagnostics: expected.clone(),
            error: None,
        }))
        .expect("successful follow-up should preserve diagnostics");
        assert_eq!(actual, expected);

        let unsupported = map_follow_up_check(Ok(unsupported_result()))
            .expect("unsupported follow-up should be empty success");
        assert!(unsupported.is_empty());
    }

    #[test]
    fn follow_up_check_maps_tool_errors_to_apply_failed() {
        for (code, message) in [
            ("lint_timeout", "lint command timed out"),
            ("lint_missing_binary", "lint command is unavailable"),
        ] {
            let failure = map_follow_up_check(Ok(CheckResult {
                supported: false,
                diagnostics: vec![diagnostic("must not mask the error")],
                error: Some(LintToolErrorResponse {
                    code: code.to_owned(),
                    message: message.to_owned(),
                    detail: None,
                }),
            }))
            .expect_err("tool error must fail after edits");

            assert_eq!(failure.code, "lint_fix_apply_failed");
            assert!(failure.message.contains(code));
            assert!(failure.message.contains(message));
            assert!(failure.message.contains("edits were applied"));
        }
    }

    #[test]
    fn follow_up_check_maps_transport_failure_to_apply_failed() {
        let failure = map_follow_up_check(Err("sidecar response stream closed".to_owned()))
            .expect_err("transport error must fail after edits");

        assert_eq!(failure.code, "lint_fix_apply_failed");
        assert!(failure.message.contains("sidecar response stream closed"));
        assert!(failure.message.contains("edits were applied"));
    }
}
