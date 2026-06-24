#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::traces::{TraceId, TraceMetadata, TracePolicy, TraceStore};
use crate::types::{DebugOutput, RuntimeFailure};

pub const RR_OUTPUT_MAX_BYTES: usize = 65_536;
pub const RR_UNSUPPORTED: &str = "rr_unsupported";
pub const RECORD_TIMEOUT: &str = "record_timeout";
pub const RECORD_FAILED: &str = "record_failed";

pub trait RrPreflight {
    fn check(&self) -> RrPreflightStatus;
}

#[derive(Clone, Debug, Default)]
pub struct RealRrPreflight;

impl RealRrPreflight {
    pub fn new() -> Self {
        Self
    }
}

impl RrPreflight for RealRrPreflight {
    fn check(&self) -> RrPreflightStatus {
        if !cfg!(target_os = "linux") {
            return RrPreflightStatus::Unsupported {
                reason: RrUnsupportedReason::NonLinuxHost,
                message: "rr recording is supported only on Linux hosts".to_owned(),
            };
        }

        match Command::new("rr")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => RrPreflightStatus::Supported,
            Ok(_) => RrPreflightStatus::Unsupported {
                reason: RrUnsupportedReason::RrUnsupported,
                message: "rr preflight command reported unsupported host configuration".to_owned(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                RrPreflightStatus::Unsupported {
                    reason: RrUnsupportedReason::RrMissing,
                    message: "rr binary is not available on PATH".to_owned(),
                }
            }
            Err(_) => RrPreflightStatus::Unsupported {
                reason: RrUnsupportedReason::RrUnsupported,
                message: "rr preflight command could not verify host support".to_owned(),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct FakeRrPreflight {
    status: RrPreflightStatus,
}

impl FakeRrPreflight {
    pub fn new(status: RrPreflightStatus) -> Self {
        Self { status }
    }
}

impl RrPreflight for FakeRrPreflight {
    fn check(&self) -> RrPreflightStatus {
        self.status.clone()
    }
}

pub trait RrRecorder {
    fn record(&self, request: RrRecordRequest, store: &mut TraceStore) -> RrRecordResult;
}

#[derive(Clone, Debug, Default)]
pub struct RealRrRecorder;

impl RealRrRecorder {
    pub fn new() -> Self {
        Self
    }
}

impl RrRecorder for RealRrRecorder {
    fn record(&self, request: RrRecordRequest, store: &mut TraceStore) -> RrRecordResult {
        if request
            .trace_policy
            .trace_root
            .metadata()
            .is_ok_and(|metadata| !metadata.is_dir())
        {
            return record_failed("trace_register", "trace root is not a directory");
        }

        let now_unix_secs = current_unix_secs();
        let allocation = match store.allocate_trace(&request.program, now_unix_secs) {
            Ok(allocation) => allocation,
            Err(_) => return record_failed("trace_register", "trace allocation failed"),
        };

        let mut command = Command::new("rr");
        command
            .arg("record")
            .arg("-o")
            .arg(&allocation.path)
            .arg("--")
            .arg(&request.program)
            .args(&request.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_request_env(&mut command, &request.env);
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = store.abort_trace(&allocation);
                return rr_unsupported(
                    RrUnsupportedReason::RrMissing,
                    "rr binary is not available on PATH",
                );
            }
            Err(_) => {
                let _ = store.abort_trace(&allocation);
                return record_failed("spawn", "failed to spawn rr record");
            }
        };

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate_child(&mut child);
                let _ = store.abort_trace(&allocation);
                return record_failed("output", "rr stdout was unavailable");
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate_child(&mut child);
                let _ = store.abort_trace(&allocation);
                return record_failed("output", "rr stderr was unavailable");
            }
        };

        let output_capture = OutputCapture::new();
        let stdout_reader = output_capture.spawn_reader("stdout", stdout);
        let stderr_reader = output_capture.spawn_reader("stderr", stderr);

        let status = match wait_with_timeout(
            &mut child,
            Duration::from_millis(request.effective_timeout_ms()),
        ) {
            WaitOutcome::Exited(status) => status,
            WaitOutcome::TimedOut => {
                terminate_child(&mut child);
                let _ = store.abort_trace(&allocation);
                return record_timeout(request.effective_timeout_ms());
            }
            WaitOutcome::WaitFailed => {
                terminate_child(&mut child);
                let _ = store.abort_trace(&allocation);
                return record_failed("wait", "failed while waiting for rr record");
            }
        };

        if let Err(_error) = collect_reader(stdout_reader) {
            let _ = store.abort_trace(&allocation);
            return record_failed("output", "failed to capture rr stdout");
        }
        if let Err(_error) = collect_reader(stderr_reader) {
            let _ = store.abort_trace(&allocation);
            return record_failed("output", "failed to capture rr stderr");
        }

        let (chunks, readers_truncated) = match output_capture.finish() {
            Ok(output) => output,
            Err(_) => {
                let _ = store.abort_trace(&allocation);
                return record_failed("output", "failed to capture rr stdout");
            }
        };

        if status.code().is_none() {
            let _ = store.abort_trace(&allocation);
            return record_failed("wait", "rr record exited without a process status code");
        }
        if looks_like_rr_output_failure(&chunks, status.code()) {
            let _ = store.abort_trace(&allocation);
            return record_failed("output", "rr record output was malformed");
        }

        let (output, output_truncated) = bounded_output_events(chunks, readers_truncated);
        let output_summary = output
            .iter()
            .map(|output| output.text.clone())
            .collect::<Vec<_>>();
        let exit_code = status.code().map(i64::from);
        let trace = match store.register_completed(
            allocation.clone(),
            crate::traces::TraceCompletion {
                program: request.program.clone(),
                args_summary: request.args.clone(),
                exit_code,
                output_summary,
                output_truncated,
            },
            now_unix_secs,
        ) {
            Ok(trace) => trace,
            Err(_) => {
                let _ = store.abort_trace(&allocation);
                return record_failed("trace_register", "trace registration failed");
            }
        };

        if store.prune(now_unix_secs).is_err() {
            return record_failed("trace_register", "trace pruning failed");
        }

        RrRecordResult {
            recordable: true,
            reason: None,
            trace_id: Some(trace.trace_id.clone()),
            trace: Some(trace),
            exit_code,
            output,
            output_truncated,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FixtureRrRecorder;

impl FixtureRrRecorder {
    pub fn new() -> Self {
        Self
    }
}

impl RrRecorder for FixtureRrRecorder {
    fn record(&self, request: RrRecordRequest, store: &mut TraceStore) -> RrRecordResult {
        record_fixture_scenario(request, store)
    }
}

#[derive(Clone, Debug)]
pub struct FakeRrRecorder {
    result: RrRecordResult,
}

impl FakeRrRecorder {
    pub fn new(result: RrRecordResult) -> Self {
        Self { result }
    }
}

impl RrRecorder for FakeRrRecorder {
    fn record(&self, _request: RrRecordRequest, _store: &mut TraceStore) -> RrRecordResult {
        self.result.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RrPreflightStatus {
    Supported,
    Unsupported {
        reason: RrUnsupportedReason,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RrUnsupportedReason {
    RrMissing,
    NonLinuxHost,
    UnsupportedCpu,
    UnsupportedPerfCounters,
    RrUnsupported,
}

impl RrUnsupportedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RrMissing => "rr_missing",
            Self::NonLinuxHost => "non_linux_host",
            Self::UnsupportedCpu => "unsupported_cpu",
            Self::UnsupportedPerfCounters => "unsupported_perf_counters",
            Self::RrUnsupported => RR_UNSUPPORTED,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RrRecordRequest {
    pub language: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub trace_policy: TracePolicy,
}

impl RrRecordRequest {
    pub fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms
            .unwrap_or(self.trace_policy.record_timeout_secs * 1000)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RrRecordResult {
    pub recordable: bool,
    pub reason: Option<String>,
    pub trace_id: Option<TraceId>,
    pub trace: Option<TraceMetadata>,
    pub exit_code: Option<i64>,
    pub output: Vec<DebugOutput>,
    pub output_truncated: bool,
    pub error: Option<RuntimeFailure>,
}

pub struct RrRuntime {
    pub preflight: Box<dyn RrPreflight>,
    pub recorder: Box<dyn RrRecorder>,
    pub store: TraceStore,
}

impl RrRuntime {
    pub fn new(store: TraceStore) -> Self {
        Self {
            preflight: Box::new(RealRrPreflight::new()),
            recorder: Box::new(RealRrRecorder::new()),
            store,
        }
    }

    pub fn new_fixture(store: TraceStore) -> Self {
        Self {
            preflight: Box::new(FakeRrPreflight::new(RrPreflightStatus::Supported)),
            recorder: Box::new(FixtureRrRecorder::new()),
            store,
        }
    }

    pub fn with_parts(
        preflight: Box<dyn RrPreflight>,
        recorder: Box<dyn RrRecorder>,
        store: TraceStore,
    ) -> Self {
        Self {
            preflight,
            recorder,
            store,
        }
    }

    pub fn record(&mut self, request: RrRecordRequest) -> RrRecordResult {
        record_with_preflight(
            self.preflight.as_ref(),
            self.recorder.as_ref(),
            request,
            &mut self.store,
        )
    }
}

enum WaitOutcome {
    Exited(std::process::ExitStatus),
    TimedOut,
    WaitFailed,
}

#[derive(Debug)]
struct CapturedChunk {
    sequence: u64,
    category: &'static str,
    bytes: Vec<u8>,
}

struct OutputCapture {
    receiver: mpsc::Receiver<CapturedChunk>,
    sender: Option<mpsc::Sender<CapturedChunk>>,
    next_sequence: Arc<AtomicU64>,
    remaining_bytes: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
}

impl OutputCapture {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            receiver,
            sender: Some(sender),
            next_sequence: Arc::new(AtomicU64::new(1)),
            remaining_bytes: Arc::new(AtomicUsize::new(RR_OUTPUT_MAX_BYTES)),
            truncated: Arc::new(AtomicBool::new(false)),
        }
    }

    fn spawn_reader<R>(
        &self,
        category: &'static str,
        mut reader: R,
    ) -> std::thread::JoinHandle<std::io::Result<()>>
    where
        R: Read + Send + 'static,
    {
        let sender = self
            .sender
            .as_ref()
            .expect("output capture sender must exist while spawning readers")
            .clone();
        let next_sequence = Arc::clone(&self.next_sequence);
        let remaining_bytes = Arc::clone(&self.remaining_bytes);
        let truncated = Arc::clone(&self.truncated);

        std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }

                let allowed = reserve_output_bytes(&remaining_bytes, read);
                if allowed < read {
                    truncated.store(true, Ordering::Relaxed);
                }
                if allowed == 0 {
                    continue;
                }

                let sequence = next_sequence.fetch_add(1, Ordering::Relaxed);
                if sender
                    .send(CapturedChunk {
                        sequence,
                        category,
                        bytes: buffer[..allowed].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(())
        })
    }

    fn finish(mut self) -> std::io::Result<(Vec<CapturedChunk>, bool)> {
        drop(self.sender.take());
        let mut chunks = self.receiver.into_iter().collect::<Vec<_>>();
        chunks.sort_by_key(|chunk| chunk.sequence);
        Ok((chunks, self.truncated.load(Ordering::Relaxed)))
    }
}

fn reserve_output_bytes(remaining_bytes: &AtomicUsize, requested: usize) -> usize {
    let mut current = remaining_bytes.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return 0;
        }
        let allowed = current.min(requested);
        match remaining_bytes.compare_exchange_weak(
            current,
            current - allowed,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return allowed,
            Err(actual) => current = actual,
        }
    }
}

pub fn record_with_preflight(
    preflight: &dyn RrPreflight,
    recorder: &dyn RrRecorder,
    request: RrRecordRequest,
    store: &mut TraceStore,
) -> RrRecordResult {
    match preflight.check() {
        RrPreflightStatus::Supported => recorder.record(request, store),
        RrPreflightStatus::Unsupported { reason, message } => rr_unsupported(reason, &message),
    }
}

fn record_fixture_scenario(request: RrRecordRequest, store: &mut TraceStore) -> RrRecordResult {
    if request
        .trace_policy
        .trace_root
        .metadata()
        .is_ok_and(|metadata| !metadata.is_dir())
    {
        return record_failed("trace_register", "trace root is not a directory");
    }

    let now_unix_secs = current_unix_secs();
    let trace_program = fixture_trace_program(&request);
    let allocation = match store.allocate_trace(&trace_program, now_unix_secs) {
        Ok(allocation) => allocation,
        Err(_) => return record_failed("trace_register", "trace allocation failed"),
    };

    let program = fixture_program_for_request(&request);
    let mut command = Command::new(program);
    command
        .args(fixture_args_for_request(&request))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_request_env(&mut command, &request.env);
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return record_failed("spawn", "failed to spawn fixture record"),
    };

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child);
            return record_failed("output", "fixture stdout was unavailable");
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut child);
            return record_failed("output", "fixture stderr was unavailable");
        }
    };

    let output_capture = OutputCapture::new();
    let stdout_reader = output_capture.spawn_reader("stdout", stdout);
    let stderr_reader = output_capture.spawn_reader("stderr", stderr);

    let status = match wait_with_timeout(
        &mut child,
        Duration::from_millis(request.effective_timeout_ms()),
    ) {
        WaitOutcome::Exited(status) => status,
        WaitOutcome::TimedOut => {
            terminate_child(&mut child);
            return record_timeout(request.effective_timeout_ms());
        }
        WaitOutcome::WaitFailed => {
            terminate_child(&mut child);
            return record_failed("wait", "failed while waiting for fixture record");
        }
    };

    if collect_reader(stdout_reader).is_err() || collect_reader(stderr_reader).is_err() {
        return record_failed("output", "failed to capture fixture output");
    }

    let (chunks, readers_truncated) = match output_capture.finish() {
        Ok(output) => output,
        Err(_) => return record_failed("output", "failed to capture fixture output"),
    };
    let (output, output_truncated) = bounded_output_events(chunks, readers_truncated);
    let output_summary = output
        .iter()
        .map(|output| output.text.clone())
        .collect::<Vec<_>>();
    let exit_code = status.code().map(i64::from);
    let trace = match store.register_completed(
        allocation,
        crate::traces::TraceCompletion {
            program: trace_program,
            args_summary: request.args.clone(),
            exit_code,
            output_summary,
            output_truncated,
        },
        now_unix_secs,
    ) {
        Ok(trace) => trace,
        Err(_) => return record_failed("trace_register", "trace registration failed"),
    };

    if store.prune(now_unix_secs).is_err() {
        return record_failed("trace_register", "trace pruning failed");
    }

    RrRecordResult {
        recordable: status.success(),
        reason: (!status.success()).then(|| RECORD_FAILED.to_owned()),
        trace_id: Some(trace.trace_id.clone()),
        trace: Some(trace),
        exit_code,
        output,
        output_truncated,
        error: (!status.success()).then(|| RuntimeFailure {
            code: RECORD_FAILED.to_owned(),
            message: "fixture record exited with failure".to_owned(),
            data: Some(serde_json::json!({ "stage": "fixture" })),
        }),
    }
}

fn fixture_program_for_request(request: &RrRecordRequest) -> PathBuf {
    if request.program == "fixture-program" {
        return std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.parent()
                    .map(|parent| parent.join("fixture_debug_adapter"))
            })
            .unwrap_or_else(|| PathBuf::from("fixture_debug_adapter"));
    }
    PathBuf::from(&request.program)
}

fn fixture_args_for_request(request: &RrRecordRequest) -> Vec<String> {
    if request
        .args
        .iter()
        .any(|arg| arg == "--scenario" || arg.starts_with("--scenario="))
    {
        return request.args.clone();
    }
    if let Some(scenario) = request
        .env
        .get("TOWER_DEBUG_FIXTURE_SCENARIO")
        .or_else(|| request.args.first())
    {
        return vec!["--scenario".to_owned(), scenario.clone()];
    }
    request.args.clone()
}

fn fixture_trace_program(request: &RrRecordRequest) -> String {
    let scenario = request
        .env
        .get("TOWER_DEBUG_FIXTURE_SCENARIO")
        .map(String::as_str)
        .or_else(|| {
            request.args.windows(2).find_map(|args| {
                (args.first().is_some_and(|arg| arg == "--scenario")).then(|| args[1].as_str())
            })
        })
        .or_else(|| {
            request
                .args
                .iter()
                .find_map(|arg| arg.strip_prefix("--scenario="))
        })
        .or_else(|| request.args.first().map(String::as_str))
        .unwrap_or(request.program.as_str());
    scenario.to_owned()
}

fn rr_unsupported(reason: RrUnsupportedReason, message: &str) -> RrRecordResult {
    RrRecordResult {
        recordable: false,
        reason: Some(RR_UNSUPPORTED.to_owned()),
        trace_id: None,
        trace: None,
        exit_code: None,
        output: Vec::new(),
        output_truncated: false,
        error: Some(RuntimeFailure {
            code: RR_UNSUPPORTED.to_owned(),
            message: message.to_owned(),
            data: Some(serde_json::json!({ "unsupported_reason": reason.as_str() })),
        }),
    }
}

fn record_timeout(timeout_ms: u64) -> RrRecordResult {
    RrRecordResult {
        recordable: false,
        reason: Some(RECORD_TIMEOUT.to_owned()),
        trace_id: None,
        trace: None,
        exit_code: None,
        output: Vec::new(),
        output_truncated: false,
        error: Some(RuntimeFailure {
            code: RECORD_TIMEOUT.to_owned(),
            message: "rr recording timed out".to_owned(),
            data: Some(serde_json::json!({ "timeout_ms": timeout_ms })),
        }),
    }
}

fn record_failed(stage: &str, message: &str) -> RrRecordResult {
    RrRecordResult {
        recordable: false,
        reason: Some(RECORD_FAILED.to_owned()),
        trace_id: None,
        trace: None,
        exit_code: None,
        output: Vec::new(),
        output_truncated: false,
        error: Some(RuntimeFailure {
            code: RECORD_FAILED.to_owned(),
            message: message.to_owned(),
            data: Some(serde_json::json!({ "stage": stage })),
        }),
    }
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn apply_request_env(command: &mut Command, env: &BTreeMap<String, String>) {
    for (key, value) in env {
        command.env(key, value);
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> WaitOutcome {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return WaitOutcome::Exited(status),
            Ok(None) if started.elapsed() >= timeout => return WaitOutcome::TimedOut,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return WaitOutcome::WaitFailed,
        }
    }
}

fn collect_reader(handle: std::thread::JoinHandle<std::io::Result<()>>) -> std::io::Result<()> {
    handle
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("output reader thread panicked")))
}

fn bounded_output_events(
    chunks: Vec<CapturedChunk>,
    readers_truncated: bool,
) -> (Vec<DebugOutput>, bool) {
    let mut output = Vec::new();
    let mut output_truncated = readers_truncated;
    for chunk in chunks {
        let mut text = String::from_utf8_lossy(&chunk.bytes).into_owned();
        if text.len() > chunk.bytes.len() {
            truncate_string_to_bytes(&mut text, chunk.bytes.len());
            output_truncated = true;
        }
        if text.is_empty() {
            continue;
        }
        output.push(DebugOutput {
            sequence: output.len() as u64 + 1,
            category: Some(chunk.category.to_owned()),
            text,
        });
    }

    (output, output_truncated)
}

fn truncate_string_to_bytes(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
}

fn looks_like_rr_output_failure(chunks: &[CapturedChunk], exit_code: Option<i32>) -> bool {
    let stdout_is_empty = chunks
        .iter()
        .filter(|chunk| chunk.category == "stdout")
        .all(|chunk| chunk.bytes.is_empty());
    if exit_code != Some(2) || !stdout_is_empty {
        return false;
    }
    let stderr_text = chunks
        .iter()
        .filter(|chunk| chunk.category == "stderr")
        .flat_map(|chunk| chunk.bytes.iter().copied())
        .collect::<Vec<_>>();
    let stderr_text = String::from_utf8_lossy(&stderr_text).to_lowercase();
    stderr_text.contains("syntax error")
        || stderr_text.contains("unterminated")
        || stderr_text.contains("unexpected eof")
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

    let _ = Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .arg(&process_group_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    kill_remaining_process_group_members(child.id());

    for _ in 0..20 {
        if !matches!(child.try_wait(), Ok(None)) {
            let _ = child.wait();
            wait_for_process_group_to_empty(child.id());
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let _ = child.kill();
    let _ = child.wait();
    wait_for_process_group_to_empty(child.id());
}

#[cfg(unix)]
fn kill_remaining_process_group_members(process_group_id: u32) {
    for pid in process_group_members(process_group_id) {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(unix)]
fn wait_for_process_group_to_empty(process_group_id: u32) {
    for _ in 0..50 {
        if process_group_members(process_group_id).is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn process_group_members(process_group_id: u32) -> Vec<u32> {
    let output = Command::new("pgrep")
        .arg("-g")
        .arg(process_group_id.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(not(unix))]
fn terminate_child(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{Value, json};

    use crate::traces::{AllocatedTrace, TraceCompletion, TraceStoreError};
    use crate::types::{RuntimeFailure, RuntimeFailureResult};

    use super::*;

    fn temp_trace_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tower-rr-{name}-{unique}"));
        fs::create_dir_all(&root).expect("create temp trace root");
        root
    }

    fn trace_policy(name: &str) -> TracePolicy {
        TracePolicy {
            trace_root: temp_trace_root(name),
            ttl_secs: Some(120),
            max_traces: 20,
            record_timeout_secs: 60,
        }
    }

    fn request(name: &str) -> RrRecordRequest {
        RrRecordRequest {
            language: "rust".to_owned(),
            program: "/bin/echo".to_owned(),
            args: vec!["hello".to_owned()],
            cwd: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            trace_policy: trace_policy(name),
        }
    }

    fn debug_output(sequence: u64, category: &str, text: &str) -> DebugOutput {
        DebugOutput {
            sequence,
            category: Some(category.to_owned()),
            text: text.to_owned(),
        }
    }

    fn write_executable(path: PathBuf, script: &str) -> PathBuf {
        fs::write(&path, script).expect("write test executable");
        let mut permissions = fs::metadata(&path)
            .expect("test executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("mark test executable");
        path
    }

    fn fake_rr_bin(name: &str, script: &str) -> (PathBuf, PathBuf) {
        let bin_dir = temp_trace_root(name).join("bin");
        fs::create_dir_all(&bin_dir).expect("create fake rr bin dir");
        let rr_path = write_executable(bin_dir.join("rr"), script);
        (bin_dir, rr_path)
    }

    fn runtime_failure(code: &str, data: Value) -> RuntimeFailure {
        RuntimeFailure {
            code: code.to_owned(),
            message: format!("{code} occurred"),
            data: Some(data),
        }
    }

    fn successful_result(store: &mut TraceStore) -> RrRecordResult {
        let allocation = store
            .allocate_trace("/bin/echo", 1_000)
            .expect("trace allocation");
        let trace_id = allocation.trace_id.clone();
        let trace = store
            .register_completed(
                allocation,
                TraceCompletion {
                    program: "/bin/echo".to_owned(),
                    args_summary: vec!["hello".to_owned()],
                    exit_code: Some(0),
                    output_summary: vec!["hello\n".to_owned()],
                    output_truncated: false,
                },
                1_000,
            )
            .expect("trace registration");

        RrRecordResult {
            recordable: true,
            reason: None,
            trace_id: Some(trace_id),
            trace: Some(trace),
            exit_code: Some(0),
            output: vec![debug_output(1, "stdout", "hello\n")],
            output_truncated: false,
            error: None,
        }
    }

    #[test]
    fn rr_runtime_failure_and_runtime_failure_result_move_to_types_preserving_code_message_and_data_fields()
     {
        let types_src = include_str!("types.rs");
        let tools_src = include_str!("tools.rs");
        let rr_src = include_str!("rr.rs");
        assert!(
            types_src.contains("pub struct RuntimeFailureResult")
                && types_src.contains("pub struct RuntimeFailure"),
            "RuntimeFailure DTOs must live in types.rs"
        );
        assert!(
            tools_src.contains("use crate::types::{")
                && tools_src.contains("RuntimeFailure, RuntimeFailureResult"),
            "tools.rs must import RuntimeFailure DTOs from types.rs"
        );
        assert!(
            rr_src.contains("use crate::types::{DebugOutput, RuntimeFailure};"),
            "rr.rs must import RuntimeFailure from types.rs"
        );
        let origin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/origin.rs");
        if origin_path.exists() {
            let origin_src = fs::read_to_string(origin_path).expect("read origin.rs");
            assert!(
                origin_src.contains("use crate::types::") && origin_src.contains("RuntimeFailure"),
                "origin.rs must import RuntimeFailure DTOs from types.rs when added"
            );
        }

        let result = RuntimeFailureResult {
            ok: false,
            error: RuntimeFailure {
                code: RR_UNSUPPORTED.to_owned(),
                message: "rr is unavailable".to_owned(),
                data: Some(json!({ "unsupported_reason": "rr_missing" })),
            },
        };

        assert_eq!(
            serde_json::to_value(result).expect("runtime failure result serializes"),
            json!({
                "ok": false,
                "error": {
                    "code": "rr_unsupported",
                    "message": "rr is unavailable",
                    "data": { "unsupported_reason": "rr_missing" }
                }
            })
        );
    }

    #[test]
    fn rr_main_wiring_constructs_trace_store_runtime_and_passes_it_to_tool_dispatch() {
        let main_src = include_str!("main.rs");

        assert!(
            main_src.contains("let mut rr_runtime: Option<RrRuntime> = None;"),
            "main.rs must keep rr runtime state beside debug sessions"
        );
        assert!(
            main_src.contains(".map(TracePolicy::from_record_config)")
                && main_src.contains("TraceStore::open(policy)")
                && main_src.contains("*rr_runtime = parsed_traces.map(|store|")
                && main_src.contains("RrRuntime::new_fixture(store)")
                && main_src.contains("RrRuntime::new(store)"),
            "initialize must build TracePolicy -> TraceStore -> RrRuntime from debug.record config"
        );
        assert!(
            main_src.contains("rr_runtime.as_mut()"),
            "invokeTool dispatch must receive mutable rr runtime state"
        );
    }

    #[test]
    fn rr_preflight_exists_as_the_fakeable_preflight_trait_with_real_and_fake_implementations() {
        fn check_with_trait(preflight: &dyn RrPreflight) -> RrPreflightStatus {
            preflight.check()
        }

        let fake = FakeRrPreflight::new(RrPreflightStatus::Supported);
        let real = RealRrPreflight::new();

        assert_eq!(check_with_trait(&fake), RrPreflightStatus::Supported);
        match check_with_trait(&real) {
            RrPreflightStatus::Supported | RrPreflightStatus::Unsupported { .. } => {}
        }
    }

    #[test]
    fn rr_recorder_exists_as_the_fakeable_record_trait_with_real_and_fake_implementations() {
        fn record_with_trait(
            recorder: &dyn RrRecorder,
            request: RrRecordRequest,
            store: &mut TraceStore,
        ) -> RrRecordResult {
            recorder.record(request, store)
        }

        let mut store = TraceStore::new(trace_policy("fake-recorder"));
        let fake_result = successful_result(&mut store);
        let fake = FakeRrRecorder::new(fake_result.clone());
        let _real = RealRrRecorder::new();

        assert_eq!(
            record_with_trait(&fake, request("fake-recorder-request"), &mut store),
            fake_result
        );
    }

    #[test]
    fn rr_preflight_status_exists_with_supported_and_unsupported_reason_message_variants() {
        let status = RrPreflightStatus::Unsupported {
            reason: RrUnsupportedReason::UnsupportedCpu,
            message: "CPU lacks required rr support".to_owned(),
        };

        assert_eq!(
            status,
            RrPreflightStatus::Unsupported {
                reason: RrUnsupportedReason::UnsupportedCpu,
                message: "CPU lacks required rr support".to_owned(),
            }
        );
        assert_eq!(RrPreflightStatus::Supported, RrPreflightStatus::Supported);
    }

    #[test]
    fn rr_unsupported_reason_exists_with_exact_serde_and_string_values() {
        let cases = [
            (RrUnsupportedReason::RrMissing, "rr_missing"),
            (RrUnsupportedReason::NonLinuxHost, "non_linux_host"),
            (RrUnsupportedReason::UnsupportedCpu, "unsupported_cpu"),
            (
                RrUnsupportedReason::UnsupportedPerfCounters,
                "unsupported_perf_counters",
            ),
            (RrUnsupportedReason::RrUnsupported, "rr_unsupported"),
        ];

        for (reason, expected) in cases {
            assert_eq!(reason.as_str(), expected);
            assert_eq!(
                serde_json::to_value(&reason).expect("reason serializes"),
                json!(expected)
            );
            assert_eq!(
                serde_json::from_value::<RrUnsupportedReason>(json!(expected))
                    .expect("reason deserializes"),
                reason
            );
        }
    }

    #[test]
    fn rr_record_request_contains_the_public_record_fields_and_effective_timeout_default() {
        let trace_policy = trace_policy("request-default-timeout");
        let request = RrRecordRequest {
            language: "rust".to_owned(),
            program: "target/debug/app".to_owned(),
            args: vec!["--case".to_owned(), "smoke".to_owned()],
            cwd: Some("fixtures".to_owned()),
            env: BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())]),
            timeout_ms: None,
            trace_policy,
        };

        assert_eq!(request.language, "rust");
        assert_eq!(request.program, "target/debug/app");
        assert_eq!(request.args, ["--case", "smoke"]);
        assert_eq!(request.cwd.as_deref(), Some("fixtures"));
        assert_eq!(request.env["RUST_LOG"], "debug");
        assert_eq!(request.effective_timeout_ms(), 60_000);
    }

    #[test]
    fn rr_record_request_effective_timeout_ms_returns_explicit_timeout_when_present() {
        let mut request = request("request-explicit-timeout");
        request.timeout_ms = Some(1_500);

        assert_eq!(request.effective_timeout_ms(), 1_500);
    }

    #[test]
    fn rr_record_result_is_the_stable_tool_facing_result_dto_with_expected_fields() {
        let mut store = TraceStore::new(trace_policy("record-result-dto"));
        let result = successful_result(&mut store);
        let serialized = serde_json::to_value(&result).expect("record result serializes");

        assert_eq!(serialized["recordable"], true);
        assert_eq!(serialized["reason"], Value::Null);
        assert!(serialized.get("trace_id").is_some());
        assert!(serialized.get("trace").is_some());
        assert_eq!(serialized["exit_code"], 0);
        assert_eq!(serialized["output"][0]["text"], "hello\n");
        assert_eq!(serialized["output_truncated"], false);
        assert_eq!(serialized["error"], Value::Null);
    }

    #[test]
    fn rr_successful_completed_runs_return_recordable_trace_metadata_exit_code_output_and_no_error()
    {
        let mut store = TraceStore::new(trace_policy("successful-run"));
        let result = successful_result(&mut store);

        assert!(result.recordable);
        assert_eq!(result.reason, None);
        assert_eq!(
            result.trace_id,
            result.trace.as_ref().map(|trace| trace.trace_id.clone())
        );
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output, vec![debug_output(1, "stdout", "hello\n")]);
        assert!(!result.output_truncated);
        assert_eq!(result.error, None);
    }

    #[test]
    fn rr_missing_binary_returns_unrecordable_rr_unsupported_with_rr_missing_error_data() {
        let missing_path = temp_trace_root("rr-missing-path");
        let request = RrRecordRequest {
            env: BTreeMap::from([("PATH".to_owned(), missing_path.display().to_string())]),
            ..request("rr-missing-request")
        };
        let mut store = TraceStore::new(request.trace_policy.clone());
        let result = RealRrRecorder::new().record(request, &mut store);

        assert!(!result.recordable);
        assert_eq!(result.reason.as_deref(), Some(RR_UNSUPPORTED));
        let error = result.error.expect("unsupported result includes error");
        assert_eq!(error.code, RR_UNSUPPORTED);
        assert_eq!(
            error.data,
            Some(json!({ "unsupported_reason": "rr_missing" }))
        );
    }

    #[test]
    fn rr_preflight_unsupported_returns_recordable_false_with_stable_rr_unsupported_data() {
        for (reason, unsupported_reason) in [
            (RrUnsupportedReason::NonLinuxHost, "non_linux_host"),
            (RrUnsupportedReason::UnsupportedCpu, "unsupported_cpu"),
            (
                RrUnsupportedReason::UnsupportedPerfCounters,
                "unsupported_perf_counters",
            ),
            (RrUnsupportedReason::RrUnsupported, "rr_unsupported"),
        ] {
            let preflight = FakeRrPreflight::new(RrPreflightStatus::Unsupported {
                reason: reason.clone(),
                message: format!("{unsupported_reason} is unsupported"),
            });
            let status = preflight.check();

            match status {
                RrPreflightStatus::Unsupported { reason, message } => {
                    assert_eq!(reason.as_str(), unsupported_reason);
                    assert_eq!(message, format!("{unsupported_reason} is unsupported"));
                }
                RrPreflightStatus::Supported => {
                    panic!("unsupported preflight case unexpectedly returned Supported")
                }
            }

            let recorder = FakeRrRecorder::new(successful_result(&mut TraceStore::new(
                trace_policy("unused-preflight-recorder-result"),
            )));
            let mut store = TraceStore::new(trace_policy("preflight-unsupported-result"));
            let result = record_with_preflight(
                &preflight,
                &recorder,
                request("preflight-unsupported-request"),
                &mut store,
            );

            assert!(!result.recordable);
            assert_eq!(result.reason.as_deref(), Some(RR_UNSUPPORTED));
            assert_eq!(result.trace_id, None);
            assert_eq!(result.trace, None);
            assert_eq!(result.output, Vec::new());
            assert!(!result.output_truncated);
            let error = result.error.expect("unsupported result includes error");
            assert_eq!(error.code, RR_UNSUPPORTED);
            assert_eq!(
                error.data,
                Some(json!({ "unsupported_reason": unsupported_reason }))
            );
        }
    }

    #[test]
    fn fixture_shaped_record_requests_do_not_bypass_rr_preflight() {
        let preflight = FakeRrPreflight::new(RrPreflightStatus::Unsupported {
            reason: RrUnsupportedReason::RrMissing,
            message: "rr missing for production recorder".to_owned(),
        });
        let recorder = FakeRrRecorder::new(successful_result(&mut TraceStore::new(trace_policy(
            "unused-fixture-shaped-recorder-result",
        ))));
        let mut request = request("fixture-shaped-preflight");
        request.program = "fixture-program".to_owned();
        request.env.insert(
            "TOWER_DEBUG_FIXTURE_CLEANUP_TOKEN".to_owned(),
            "fixture-shaped-token".to_owned(),
        );
        request.env.insert(
            "TOWER_DEBUG_FIXTURE_SCENARIO".to_owned(),
            "record_ok".to_owned(),
        );
        let mut store = TraceStore::new(trace_policy("fixture-shaped-preflight-store"));

        let result = record_with_preflight(&preflight, &recorder, request, &mut store);

        assert!(!result.recordable);
        assert_eq!(result.reason.as_deref(), Some(RR_UNSUPPORTED));
        assert_eq!(result.trace_id, None);
        let error = result.error.expect("unsupported result includes error");
        assert_eq!(error.code, RR_UNSUPPORTED);
        assert_eq!(
            error.data,
            Some(json!({ "unsupported_reason": "rr_missing" }))
        );
    }

    #[test]
    fn rr_timeout_while_target_is_running_returns_record_timeout_with_effective_timeout_data() {
        let timeout_probe = temp_trace_root("timeout-reaping").join("child.pid");
        let script = format!(
            "#!/bin/sh\n\
             shift 4\n\
             \"$@\" &\n\
             echo \"$!\" > {}\n\
             wait \"$!\"\n",
            timeout_probe.display()
        );
        let (fake_rr_dir, _rr_path) = fake_rr_bin("timeout-reaping", &script);
        let request = RrRecordRequest {
            timeout_ms: Some(250),
            env: BTreeMap::from([("PATH".to_owned(), fake_rr_dir.display().to_string())]),
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "/bin/sleep 30".to_owned()],
            ..request("timeout-result")
        };
        let mut store = TraceStore::new(request.trace_policy.clone());
        let result = RealRrRecorder::new().record(request, &mut store);

        assert!(!result.recordable);
        assert_eq!(result.reason.as_deref(), Some(RECORD_TIMEOUT));
        let error = result.error.expect("timeout result includes error");
        assert_eq!(error.code, RECORD_TIMEOUT);
        assert_eq!(error.data, Some(json!({ "timeout_ms": 250 })));
        let child_pid = fs::read_to_string(&timeout_probe)
            .expect("timeout path records child pid")
            .trim()
            .to_owned();
        let status = Command::new("kill")
            .arg("-0")
            .arg(&child_pid)
            .status()
            .expect("probe child process liveness");
        assert!(
            !status.success(),
            "timeout cleanup must reap the target process tree; child pid {child_pid} is still alive"
        );
    }

    #[test]
    fn rr_spawn_wait_output_and_trace_register_failures_return_record_failed_with_stage_data() {
        let fake_rr_failures = [
            (
                "spawn",
                {
                    let bin_dir = temp_trace_root("spawn-failure").join("bin");
                    fs::create_dir_all(&bin_dir).expect("create fake rr bin dir");
                    fs::write(bin_dir.join("rr"), "#!/bin/sh\nexit 0\n")
                        .expect("write non-executable fake rr");
                    RrRecordRequest {
                        env: BTreeMap::from([("PATH".to_owned(), bin_dir.display().to_string())]),
                        ..request("spawn-failure")
                    }
                },
                TraceStore::new(trace_policy("spawn-failure-store")),
            ),
            (
                "wait",
                {
                    let (fake_rr_dir, _rr_path) =
                        fake_rr_bin("wait-failure", "#!/bin/sh\nkill -9 $$\n");
                    RrRecordRequest {
                        env: BTreeMap::from([(
                            "PATH".to_owned(),
                            fake_rr_dir.display().to_string(),
                        )]),
                        ..request("wait-failure")
                    }
                },
                TraceStore::new(trace_policy("wait-failure-store")),
            ),
            (
                "output",
                {
                    let (fake_rr_dir, _rr_path) =
                        fake_rr_bin("output-failure", "#!/bin/sh\nprintf 'unterminated");
                    RrRecordRequest {
                        env: BTreeMap::from([(
                            "PATH".to_owned(),
                            fake_rr_dir.display().to_string(),
                        )]),
                        ..request("output-failure")
                    }
                },
                TraceStore::new(trace_policy("output-failure-store")),
            ),
            (
                "trace_register",
                {
                    let (fake_rr_dir, _rr_path) =
                        fake_rr_bin("trace-register-failure", "#!/bin/sh\nshift 4\n\"$@\"\n");
                    let trace_root_file = temp_trace_root("trace-register-root").join("file");
                    fs::write(&trace_root_file, "not a directory").expect("write trace root file");
                    RrRecordRequest {
                        env: BTreeMap::from([(
                            "PATH".to_owned(),
                            fake_rr_dir.display().to_string(),
                        )]),
                        trace_policy: TracePolicy {
                            trace_root: trace_root_file,
                            ttl_secs: Some(120),
                            max_traces: 20,
                            record_timeout_secs: 60,
                        },
                        ..request("trace-register-failure")
                    }
                },
                TraceStore::new(trace_policy("trace-register-failure-store")),
            ),
        ];

        for (stage, request, mut store) in fake_rr_failures {
            let result = RealRrRecorder::new().record(request, &mut store);

            assert!(
                !result.recordable,
                "stage {stage} unexpectedly recorded successfully: {result:?}"
            );
            assert_eq!(
                result.reason.as_deref(),
                Some(RECORD_FAILED),
                "stage {stage} returned unexpected result: {result:?}"
            );
            let error = result.error.expect("record failure result includes error");
            assert_eq!(error.code, RECORD_FAILED);
            assert_eq!(
                error.data,
                Some(json!({ "stage": stage })),
                "stage {stage} returned unexpected error"
            );
        }
    }

    #[test]
    fn rr_target_nonzero_exit_is_preserved_as_exit_code_and_not_infrastructure_failure() {
        let (fake_rr_dir, _rr_path) = fake_rr_bin("target-nonzero", "#!/bin/sh\nshift 4\n\"$@\"\n");
        let request = RrRecordRequest {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "echo failed >&2; exit 7".to_owned()],
            env: BTreeMap::from([("PATH".to_owned(), fake_rr_dir.display().to_string())]),
            ..request("target-nonzero")
        };
        let mut store = TraceStore::new(request.trace_policy.clone());
        let result = RealRrRecorder::new().record(request, &mut store);

        assert!(result.recordable);
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.reason, None);
        assert_eq!(result.error, None);
        assert_eq!(result.output, vec![debug_output(1, "stderr", "failed\n")]);
    }

    #[test]
    fn rr_output_capture_is_bounded_to_max_bytes_preserves_order_and_marks_truncated() {
        let output_capture = OutputCapture::new();
        let reader = output_capture.spawn_reader(
            "stdout",
            std::io::Cursor::new(vec![b'x'; RR_OUTPUT_MAX_BYTES + 1024]),
        );
        collect_reader(reader).expect("memory output reader");
        let (chunks, readers_truncated) = output_capture.finish().expect("finish output capture");
        let (output, output_truncated) = bounded_output_events(chunks, readers_truncated);

        assert_eq!(RR_OUTPUT_MAX_BYTES, 65_536);
        assert_eq!(
            output.first().and_then(|output| output.category.as_deref()),
            Some("stdout")
        );
        let captured_bytes: usize = output.iter().map(|output| output.text.len()).sum();
        assert_eq!(captured_bytes, RR_OUTPUT_MAX_BYTES);
        assert!(output_truncated);
    }

    #[test]
    fn rr_output_capture_preserves_stderr_before_stdout_interleaving() {
        let (fake_rr_dir, _rr_path) =
            fake_rr_bin("interleaved-output", "#!/bin/sh\nshift 4\n\"$@\"\n");
        let request = RrRecordRequest {
            program: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf stderr-first >&2; /bin/sleep 0.05; printf stdout-second".to_owned(),
            ],
            env: BTreeMap::from([("PATH".to_owned(), fake_rr_dir.display().to_string())]),
            ..request("interleaved-output")
        };
        let mut store = TraceStore::new(request.trace_policy.clone());
        let result = RealRrRecorder::new().record(request, &mut store);

        assert!(result.recordable);
        assert_eq!(
            result
                .output
                .iter()
                .map(|output| (
                    output.category.as_deref().unwrap_or_default().to_owned(),
                    output.text.clone()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("stderr".to_owned(), "stderr-first".to_owned()),
                ("stdout".to_owned(), "stdout-second".to_owned()),
            ]
        );
        assert!(!result.output_truncated);
    }

    #[test]
    fn rr_runtime_failure_data_contains_only_exact_structured_fields_for_supported_error_shapes() {
        let cases = [
            runtime_failure(
                RR_UNSUPPORTED,
                json!({ "unsupported_reason": "unsupported_cpu" }),
            ),
            runtime_failure(RECORD_TIMEOUT, json!({ "timeout_ms": 500 })),
            runtime_failure(RECORD_FAILED, json!({ "stage": "spawn" })),
        ];

        for error in cases {
            let keys = error
                .data
                .as_ref()
                .and_then(Value::as_object)
                .expect("error data is an object")
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            assert!(
                matches!(
                    keys.as_slice(),
                    [only]
                        if only == "unsupported_reason" || only == "timeout_ms" || only == "stage"
                ),
                "unexpected runtime failure data keys: {keys:?}"
            );
        }
    }

    #[test]
    fn rr_successful_recording_allocates_registers_and_prunes_trace_store_metadata() {
        let (fake_rr_dir, _rr_path) = fake_rr_bin(
            "successful-command",
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > \"$RR_TEST_ARGV_CAPTURE\"\n\
             shift 4\n\
             \"$@\"\n",
        );
        let argv_capture = temp_trace_root("successful-command-capture").join("argv");
        let mut store = TraceStore::new(TracePolicy {
            max_traces: 1,
            ..trace_policy("allocate-register-prune")
        });
        let old = store
            .allocate_trace("old", 1)
            .expect("old trace allocation");
        store
            .register_completed(
                old,
                TraceCompletion {
                    program: "old".to_owned(),
                    args_summary: Vec::new(),
                    exit_code: Some(0),
                    output_summary: Vec::new(),
                    output_truncated: false,
                },
                1,
            )
            .expect("old trace registration");

        let request = RrRecordRequest {
            program: "/bin/echo".to_owned(),
            args: vec!["--ok".to_owned()],
            env: BTreeMap::from([
                ("PATH".to_owned(), fake_rr_dir.display().to_string()),
                (
                    "RR_TEST_ARGV_CAPTURE".to_owned(),
                    argv_capture.display().to_string(),
                ),
            ]),
            trace_policy: store.policy().clone(),
            ..request("successful-command")
        };
        let result = RealRrRecorder::new().record(request, &mut store);

        assert!(result.recordable);
        let metadata = result.trace.expect("successful record returns metadata");
        assert_eq!(result.trace_id.as_ref(), Some(&metadata.trace_id));
        assert_eq!(metadata.program, "/bin/echo");
        assert_eq!(metadata.args_summary, vec!["--ok"]);
        assert_eq!(metadata.exit_code, Some(0));
        assert!(
            fs::read_to_string(argv_capture)
                .expect("fake rr argv capture")
                .contains("record\n-o\n"),
            "recorder must invoke `rr record -o <allocated.path> -- <program> <args>`"
        );
        assert_eq!(
            store.list_traces().expect("trace list")[0].trace_id,
            metadata.trace_id
        );
        assert_eq!(
            store.list_traces().expect("trace list").len(),
            1,
            "successful recording must prune after registering the new trace"
        );
    }

    #[test]
    fn rr_recording_failure_does_not_leave_partial_trace_metadata_listable_as_completed_trace() {
        let bin_dir = temp_trace_root("failed-recording").join("bin");
        fs::create_dir_all(&bin_dir).expect("create fake rr bin dir");
        fs::write(bin_dir.join("rr"), "#!/bin/sh\nexit 0\n").expect("write non-executable fake rr");
        let request = RrRecordRequest {
            env: BTreeMap::from([("PATH".to_owned(), bin_dir.display().to_string())]),
            ..request("failed-recording")
        };
        let trace_root = request.trace_policy.trace_root.clone();
        let mut store = TraceStore::new(request.trace_policy.clone());
        let result = RealRrRecorder::new().record(request, &mut store);

        assert!(!result.recordable);
        assert_eq!(result.reason.as_deref(), Some(RECORD_FAILED));
        let error = result.error.expect("record failure result includes error");
        assert_eq!(error.data, Some(json!({ "stage": "spawn" })));
        assert_eq!(store.list_traces().expect("trace list"), Vec::new());
        let remaining_entries = fs::read_dir(trace_root).expect("trace root exists").count();
        assert_eq!(
            remaining_entries, 0,
            "failed recordings must remove the allocated trace directory"
        );
    }

    #[test]
    fn rr_trace_register_failure_is_reported_as_record_failed_and_unregistered() {
        let mut store = TraceStore::new(trace_policy("trace-register-failure"));
        let escaped = AllocatedTrace {
            trace_id: TraceId::new("escaped").expect("valid trace id"),
            path: temp_trace_root("outside-trace-root").join("escaped"),
        };
        fs::create_dir_all(&escaped.path).expect("create escaped path");
        let error = store
            .register_completed(
                escaped,
                TraceCompletion {
                    program: "escaped".to_owned(),
                    args_summary: Vec::new(),
                    exit_code: Some(0),
                    output_summary: Vec::new(),
                    output_truncated: false,
                },
                1,
            )
            .expect_err("escaped trace registration should fail");

        assert!(matches!(error, TraceStoreError::TracePathEscaped { .. }));
        assert_eq!(store.list_traces().expect("trace list"), Vec::new());
    }
}
