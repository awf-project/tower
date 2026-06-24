#![forbid(unsafe_code)]

use extension_protocol::HostCall;
use extension_sidecar_harness::jsonrpc::HarnessError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

pub type HostCallIdAllocator = extension_sidecar_harness::HostCallIdAllocator;
pub type QueuedFrame = extension_sidecar_harness::QueuedFrame;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CheckRequest {
    pub path: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct FixRequest {
    pub path: Option<String>,
    #[serde(default, rename = "unsafe")]
    pub unsafe_fixes: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CheckResult {
    pub supported: bool,
    pub diagnostics: Vec<LintDiagnosticDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<LintToolErrorResponse>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LintToolErrorResponse {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct FixResult {
    pub files_changed: usize,
    pub fixes_applied: usize,
    pub fixes_skipped: Vec<SkippedFixDto>,
    pub remaining_diagnostics: Vec<LintDiagnosticDto>,
    pub previews: Vec<FixPreviewDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SkippedFixDto {
    pub path: String,
    pub reason: SkippedFixReason,
    pub diagnostic: Option<LintDiagnosticDto>,
    pub supported_fix: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkippedFixReason {
    Conflict,
    Unsafe,
    Unsupported,
    CasConflict,
    InvalidRange,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct FixPreviewDto {
    pub path: String,
    pub edits: Vec<FixPreviewEditDto>,
    pub preview_content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct FixPreviewEditDto {
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct FixToolErrorResponse {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LintDiagnosticDto {
    pub path: String,
    pub line: u32,
    pub character: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    #[serde(rename = "endCharacter")]
    pub end_character: u32,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub fn host_call<R>(
    out: &std::sync::Arc<std::sync::Mutex<impl std::io::Write>>,
    lines: &mut R,
    next_id: &mut u64,
    method: &str,
    call: &HostCall,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    extension_sidecar_harness::write_envelope(
        out,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": serde_json::to_value(call).map_err(|error| error.to_string())?,
        }),
    )
    .map_err(harness_error_message)?;
    read_host_response(lines, id, queued)
}

pub fn read_host_response<R>(
    lines: &mut R,
    expected_id: u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Value, String>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    extension_sidecar_harness::read_host_response(lines, expected_id, queued)
        .map_err(harness_error_message)
}

fn harness_error_message(error: HarnessError) -> String {
    match error {
        HarnessError::HostError(value) => value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
        other => other.to_string(),
    }
}
