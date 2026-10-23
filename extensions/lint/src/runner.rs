#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(unix))]
compile_error!("lint command process-tree supervision requires a Unix host");

use core_engine::adapters::config::lint::{ParserFormat, TargetMode};
use rustix::process::{Pid, Signal, kill_process_group};

use crate::config::RunnerLintConfig;
use crate::diagnostics::LintDiagnostic;
use crate::fixes::LintFix;
use crate::parsers::{ParserError, extract_linter_fixes, parse_linter_output};

const OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const TERMINATE_GRACE: Duration = Duration::from_millis(100);
const CLEANUP_ALLOWANCE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug)]
pub struct RunRequest<'a> {
    pub config: &'a RunnerLintConfig,
    pub workspace_root: &'a Path,
    pub target_path: Option<&'a str>,
    pub stdin_content: Option<&'a str>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    pub supported: bool,
    pub diagnostics: Vec<LintDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFixOutcome {
    pub supported: bool,
    pub fixes: Vec<LintFix>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LintToolError {
    MissingBinary { command: String },
    UnparseableOutput,
    NonzeroExit { code: Option<i32> },
    Timeout,
    InvalidConfig(String),
}

impl LintToolError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingBinary { .. } => "lint_missing_binary",
            Self::UnparseableOutput => "lint_unparseable_output",
            Self::NonzeroExit { .. } => "lint_nonzero_exit",
            Self::Timeout => "lint_timeout",
            Self::InvalidConfig(_) => "lint_invalid_config",
        }
    }
}

pub fn run_linter(request: RunRequest<'_>) -> Result<RunOutcome, LintToolError> {
    let (output, status_code, status_success) = run_linter_output(request)?;
    if status_success && output.trim().is_empty() {
        return Ok(RunOutcome {
            supported: true,
            diagnostics: Vec::new(),
        });
    }

    match parse_linter_output(
        request.config.format,
        &output,
        request.workspace_root,
        request.config.regex.as_deref(),
        request.config.source.as_deref(),
    ) {
        Ok(diagnostics) => Ok(RunOutcome {
            supported: true,
            diagnostics,
        }),
        Err(ParserError::NoDiagnostics) if status_success => Ok(RunOutcome {
            supported: true,
            diagnostics: Vec::new(),
        }),
        Err(error) => Err(map_parser_error(error, status_code)),
    }
}

pub fn run_linter_fixes(request: RunRequest<'_>) -> Result<RunFixOutcome, LintToolError> {
    let (output, status_code, status_success) = run_linter_output(request)?;
    if status_success && output.trim().is_empty() {
        return Ok(RunFixOutcome {
            supported: true,
            fixes: Vec::new(),
        });
    }

    match extract_linter_fixes(
        request.config.format,
        &output,
        request.workspace_root,
        request.config.regex.as_deref(),
        request.config.source.as_deref(),
    ) {
        Ok(fixes) => Ok(RunFixOutcome {
            supported: true,
            fixes,
        }),
        Err(ParserError::NoDiagnostics) if status_success => Ok(RunFixOutcome {
            supported: true,
            fixes: Vec::new(),
        }),
        Err(error) => Err(map_parser_error(error, status_code)),
    }
}

fn run_linter_output(
    request: RunRequest<'_>,
) -> Result<(String, Option<i32>, bool), LintToolError> {
    validate_config(request.config)?;

    let deadline = Instant::now() + request.timeout;
    let args = command_args(request)?;
    let child = spawn_command(request, &args, deadline)?;
    let mut process = RunningProcess::new(child);

    process.stdout = process.child.stdout.take().map(read_output);
    process.stderr = process.child.stderr.take().map(read_output);
    process.stdin = if let TargetMode::Stdin = request.config.target {
        let content = request
            .stdin_content
            .ok_or_else(|| LintToolError::InvalidConfig("missing stdin_content".to_owned()))?
            .to_owned();
        Some(write_stdin(
            process
                .child
                .stdin
                .take()
                .ok_or_else(|| LintToolError::InvalidConfig("missing stdin pipe".to_owned()))?,
            content,
        ))
    } else {
        None
    };
    let mut status = None;
    let mut stdin_done = process.stdin.is_none();
    let mut stdout_output = None;
    let mut stderr_output = None;

    loop {
        if status.is_none() {
            status = process.child.try_wait().map_err(|_| {
                LintToolError::InvalidConfig("failed to wait for lint command".to_owned())
            })?;
        }
        poll_unit_worker(&mut process.stdin, &mut stdin_done)?;
        poll_output_worker(&mut process.stdout, &mut stdout_output)?;
        poll_output_worker(&mut process.stderr, &mut stderr_output)?;

        if let Some(status) = status
            && stdin_done
            && stdout_output.is_some()
            && stderr_output.is_some()
        {
            process.disarm();
            let mut output = stdout_output.unwrap_or_default();
            output.extend(stderr_output.unwrap_or_default());
            let output = String::from_utf8_lossy(&output);
            return Ok((output.into_owned(), status.code(), status.success()));
        }

        if Instant::now() >= deadline {
            process.cleanup()?;
            return Err(LintToolError::Timeout);
        }

        thread::sleep(WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn validate_config(config: &RunnerLintConfig) -> Result<(), LintToolError> {
    if config.command.is_empty() {
        return Err(LintToolError::InvalidConfig("missing command".to_owned()));
    }

    if matches!(config.format, ParserFormat::GenericRegex) && config.regex.is_none() {
        return Err(LintToolError::InvalidConfig(
            "missing regex for generic-regex parser".to_owned(),
        ));
    }

    Ok(())
}

fn command_args(request: RunRequest<'_>) -> Result<Vec<String>, LintToolError> {
    let mut args = request.config.args.clone();

    match request.config.target {
        TargetMode::Append => {
            let target_path = request
                .target_path
                .ok_or_else(|| LintToolError::InvalidConfig("missing target_path".to_owned()))?;
            args.push(target_path.to_owned());
        }
        TargetMode::Stdin => {
            if request.target_path.is_none() {
                return Err(LintToolError::InvalidConfig(
                    "missing target_path".to_owned(),
                ));
            }
            if request.stdin_content.is_none() {
                return Err(LintToolError::InvalidConfig(
                    "missing stdin_content".to_owned(),
                ));
            }
        }
        TargetMode::None => {}
    }

    Ok(args)
}

fn spawn_command(
    request: RunRequest<'_>,
    args: &[String],
    deadline: Instant,
) -> Result<Child, LintToolError> {
    loop {
        let mut command = Command::new(&request.config.command);
        command
            .args(args)
            .current_dir(request.workspace_root)
            .stdin(if matches!(request.config.target, TargetMode::Stdin) {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        use std::os::unix::process::CommandExt;
        command.process_group(0);

        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(LintToolError::MissingBinary {
                    command: request.config.command.clone(),
                });
            }
            Err(error) if is_executable_busy(&error) && Instant::now() < deadline => {
                thread::sleep(WAIT_POLL_INTERVAL);
            }
            Err(error) if is_executable_busy(&error) => return Err(LintToolError::Timeout),
            Err(_) => {
                return Err(LintToolError::InvalidConfig(
                    "failed to spawn lint command".to_owned(),
                ));
            }
        }
    }
}

fn is_executable_busy(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(26)
}

struct Worker<T> {
    receiver: mpsc::Receiver<Result<T, LintToolError>>,
    handle: Option<thread::JoinHandle<()>>,
}

fn read_output<R>(mut reader: R) -> Worker<Vec<u8>>
where
    R: Read + Send + 'static,
{
    spawn_worker(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];

        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|_| LintToolError::UnparseableOutput)?;
            if read == 0 {
                return Ok(output);
            }

            let remaining = OUTPUT_LIMIT_BYTES.saturating_sub(output.len());
            if remaining > 0 {
                output.extend_from_slice(&buffer[..read.min(remaining)]);
            }
        }
    })
}

fn write_stdin(mut stdin: ChildStdin, content: String) -> Worker<()> {
    spawn_worker(move || {
        stdin
            .write_all(content.as_bytes())
            .map_err(|_| LintToolError::InvalidConfig("failed to write stdin_content".to_owned()))
    })
}

fn spawn_worker<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, LintToolError> + Send + 'static,
) -> Worker<T> {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _ = sender.send(work());
    });
    Worker {
        receiver,
        handle: Some(handle),
    }
}

impl<T> Worker<T> {
    fn join(&mut self) -> Result<(), LintToolError> {
        self.handle
            .take()
            .expect("worker handle must exist while worker is active")
            .join()
            .map_err(|_| LintToolError::InvalidConfig("lint pipe worker panicked".to_owned()))
    }
}

fn poll_unit_worker(worker: &mut Option<Worker<()>>, done: &mut bool) -> Result<(), LintToolError> {
    let Some(active) = worker.as_ref() else {
        return Ok(());
    };
    match active.receiver.try_recv() {
        Ok(result) => {
            let join_result = worker.as_mut().expect("active worker").join();
            *done = true;
            *worker = None;
            result?;
            join_result?;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            let join_result = worker.as_mut().expect("active worker").join();
            *worker = None;
            join_result?;
            return Err(LintToolError::InvalidConfig(
                "stdin writer thread stopped unexpectedly".to_owned(),
            ));
        }
    }
    Ok(())
}

fn poll_output_worker(
    worker: &mut Option<Worker<Vec<u8>>>,
    output: &mut Option<Vec<u8>>,
) -> Result<(), LintToolError> {
    let Some(active) = worker.as_ref() else {
        return Ok(());
    };
    match active.receiver.try_recv() {
        Ok(result) => {
            let join_result = worker.as_mut().expect("active worker").join();
            *worker = None;
            *output = Some(result?);
            join_result?;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            let join_result = worker.as_mut().expect("active worker").join();
            *worker = None;
            join_result?;
            return Err(LintToolError::UnparseableOutput);
        }
    }
    Ok(())
}

struct RunningProcess {
    child: Child,
    process_group: Pid,
    stdin: Option<Worker<()>>,
    stdout: Option<Worker<Vec<u8>>>,
    stderr: Option<Worker<Vec<u8>>>,
    armed: bool,
}

impl RunningProcess {
    fn new(child: Child) -> Self {
        let process_group = Pid::from_raw(child.id() as i32)
            .expect("a spawned child always has a positive process id");
        Self {
            child,
            process_group,
            stdin: None,
            stdout: None,
            stderr: None,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cleanup(&mut self) -> Result<(), LintToolError> {
        if !self.armed {
            return Ok(());
        }

        let cleanup_deadline = Instant::now() + CLEANUP_ALLOWANCE;
        let terminate_deadline = (Instant::now() + TERMINATE_GRACE).min(cleanup_deadline);
        let mut signal_error = signal_group_if_alive(self.process_group, Signal::TERM).err();

        while Instant::now() < terminate_deadline {
            self.drain_cleanup();
            thread::sleep(
                WAIT_POLL_INTERVAL
                    .min(terminate_deadline.saturating_duration_since(Instant::now())),
            );
        }

        // Signal the saved group even when the direct child exited: descendants may
        // still own inherited pipes or deliberately ignore SIGTERM.
        if let Err(error) = signal_group_if_alive(self.process_group, Signal::KILL) {
            signal_error = Some(error);
        }
        let _ = self.child.kill();
        while Instant::now() < cleanup_deadline {
            self.drain_cleanup();
            if self.child.try_wait().ok().flatten().is_some()
                && self.stdin.is_none()
                && self.stdout.is_none()
                && self.stderr.is_none()
            {
                self.armed = false;
                return signal_error.map_or(Ok(()), Err);
            }
            thread::sleep(
                WAIT_POLL_INTERVAL.min(cleanup_deadline.saturating_duration_since(Instant::now())),
            );
        }

        Err(LintToolError::InvalidConfig(
            "failed to clean up lint command process tree".to_owned(),
        ))
    }

    fn drain_cleanup(&mut self) {
        let _ = self.child.try_wait();
        let mut stdin_done = self.stdin.is_none();
        let mut ignored = None;
        let _ = poll_unit_worker(&mut self.stdin, &mut stdin_done);
        let _ = poll_output_worker(&mut self.stdout, &mut ignored);
        ignored = None;
        let _ = poll_output_worker(&mut self.stderr, &mut ignored);
    }
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn signal_group_if_alive(process_group: Pid, signal: Signal) -> Result<(), LintToolError> {
    match kill_process_group(process_group, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(_) => Err(LintToolError::InvalidConfig(
            "failed to signal lint command process tree".to_owned(),
        )),
    }
}

fn map_parser_error(error: ParserError, exit_code: Option<i32>) -> LintToolError {
    match error {
        ParserError::NoDiagnostics => LintToolError::NonzeroExit { code: exit_code },
        ParserError::InvalidRegex => LintToolError::InvalidConfig("invalid regex".to_owned()),
        ParserError::MissingCaptureGroup(group) => {
            LintToolError::InvalidConfig(format!("missing capture group: {group}"))
        }
        ParserError::InvalidJson | ParserError::UnsafePath(_) => LintToolError::UnparseableOutput,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use core_engine::adapters::config::lint::{LintCommandConfig, ParserFormat, TargetMode};
    use core_engine::domain::code_intel::Severity;

    static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    struct TestWorkspace {
        root: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("tower-lint-runner-test-{nanos}-{id}"));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn command(&self, name: &str, body: &str) -> String {
            let path = self.root.join(name);
            fs::write(&path, body).unwrap();

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let mut permissions = fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&path, permissions).unwrap();
            }

            path.to_string_lossy().into_owned()
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.root.join(name)).unwrap()
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    struct ProcessCleanup {
        pids: Vec<String>,
    }

    #[cfg(unix)]
    impl ProcessCleanup {
        fn new() -> Self {
            Self { pids: Vec::new() }
        }

        fn retain_until_test_end(&mut self, pid: String) {
            self.pids.push(pid);
        }
    }

    #[cfg(unix)]
    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            for pid in &self.pids {
                let _ = Command::new("kill").args(["-KILL", pid]).status();
            }
        }
    }

    #[cfg(unix)]
    fn process_is_alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .status()
            .is_ok_and(|status| status.success())
    }

    fn regex_config(command: String, target: TargetMode) -> RunnerLintConfig {
        RunnerLintConfig {
            command,
            timeout_secs: 2,
            args: Vec::new(),
            extensions: vec!["rs".to_owned()],
            format: ParserFormat::GenericRegex,
            target,
            regex: Some(r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)".to_owned()),
            source: Some("fixture-lint".to_owned()),
        }
    }

    fn request<'a>(
        config: &'a RunnerLintConfig,
        workspace: &'a TestWorkspace,
        target_path: Option<&'a str>,
        stdin_content: Option<&'a str>,
    ) -> RunRequest<'a> {
        RunRequest {
            config,
            workspace_root: &workspace.root,
            target_path,
            stdin_content,
            timeout: Duration::from_secs(2),
        }
    }

    trait RunnerResultExt<T> {
        fn assert_ok(self) -> T;
        fn assert_err(self) -> LintToolError;
    }

    impl<T> RunnerResultExt<T> for Result<T, LintToolError> {
        fn assert_ok(self) -> T {
            match self {
                Ok(value) => value,
                Err(error) => panic!("expected Ok(_), got Err({error:?})"),
            }
        }

        fn assert_err(self) -> LintToolError {
            match self {
                Ok(_) => panic!("expected Err(_), got Ok(_)"),
                Err(error) => error,
            }
        }
    }

    #[test]
    fn host_lint_configuration_preserves_all_runner_settings() {
        let host_config = LintCommandConfig {
            command: "cargo".to_owned(),
            timeout_secs: 2,
            args: vec!["check".to_owned(), "--message-format=json".to_owned()],
            extensions: vec!["rs".to_owned()],
            format: ParserFormat::RustcJson,
            target: TargetMode::Append,
            regex: None,
            source: Some("cargo-check".to_owned()),
        };

        let runner_config = RunnerLintConfig::from(&host_config);

        assert_eq!(runner_config.command, host_config.command);
        assert_eq!(runner_config.timeout_secs, host_config.timeout_secs);
        assert_eq!(runner_config.args, host_config.args);
        assert_eq!(runner_config.extensions, host_config.extensions);
        assert_eq!(runner_config.format, host_config.format);
        assert_eq!(runner_config.target, host_config.target);
        assert_eq!(runner_config.regex, host_config.regex);
        assert_eq!(runner_config.source, host_config.source);
    }

    #[test]
    fn lint_tool_error_code_returns_exact_stable_error_codes() {
        assert_eq!(
            LintToolError::MissingBinary {
                command: "missing-lint".to_owned()
            }
            .code(),
            "lint_missing_binary"
        );
        assert_eq!(
            LintToolError::UnparseableOutput.code(),
            "lint_unparseable_output"
        );
        assert_eq!(
            LintToolError::NonzeroExit { code: Some(1) }.code(),
            "lint_nonzero_exit"
        );
        assert_eq!(LintToolError::Timeout.code(), "lint_timeout");
        assert_eq!(
            LintToolError::InvalidConfig("bad".to_owned()).code(),
            "lint_invalid_config"
        );
    }

    #[test]
    fn target_mode_append_appends_workspace_relative_target_path_to_configured_command_arguments() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "append_args.sh",
            "#!/bin/sh\ndir=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$dir/args.txt\"\nprintf 'src/main.rs:1:1: appended\\n'\n",
        );
        let mut config = regex_config(command, TargetMode::Append);
        config.args = vec!["--json".to_owned()];

        let outcome =
            run_linter(request(&config, &workspace, Some("src/main.rs"), None)).assert_ok();

        assert!(outcome.supported);
        assert_eq!(workspace.read("args.txt"), "--json\nsrc/main.rs\n");
        assert_eq!(outcome.diagnostics[0].path, "src/main.rs");
    }

    #[test]
    fn target_mode_stdin_requires_stdin_content_and_writes_it_without_appending_path() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "stdin_args.sh",
            "#!/bin/sh\ndir=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$dir/args.txt\"\ncat > \"$dir/stdin.txt\"\nprintf 'src/lib.rs:2:3: stdin diagnostic\\n'\n",
        );
        let mut config = regex_config(command, TargetMode::Stdin);
        config.args = vec!["--stdin".to_owned()];

        let missing =
            run_linter(request(&config, &workspace, Some("src/lib.rs"), None)).assert_err();
        match missing {
            LintToolError::InvalidConfig(message) => {
                assert!(message.contains("stdin_content"));
            }
            other => panic!("expected InvalidConfig for missing stdin_content, got {other:?}"),
        }

        let outcome = run_linter(request(
            &config,
            &workspace,
            Some("src/lib.rs"),
            Some("fn lib() {}\n"),
        ))
        .assert_ok();

        assert!(outcome.supported);
        assert_eq!(workspace.read("args.txt"), "--stdin\n");
        assert_eq!(workspace.read("stdin.txt"), "fn lib() {}\n");
        assert_eq!(outcome.diagnostics[0].path, "src/lib.rs");
    }

    #[test]
    fn target_mode_none_runs_command_without_target_path_for_whole_project_tools() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "none_args.sh",
            "#!/bin/sh\ndir=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$dir/args.txt\"\nprintf 'src/main.rs:1:1: whole project\\n'\n",
        );
        let mut config = regex_config(command, TargetMode::None);
        config.args = vec!["--workspace".to_owned()];

        let outcome = run_linter(request(&config, &workspace, None, None)).assert_ok();

        assert!(outcome.supported);
        assert_eq!(workspace.read("args.txt"), "--workspace\n");
        assert_eq!(outcome.diagnostics[0].path, "src/main.rs");
    }

    #[test]
    fn target_mode_append_and_stdin_require_target_path_before_spawning_command() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "should_not_spawn.sh",
            "#!/bin/sh\ndir=$(dirname \"$0\")\nprintf spawned > \"$dir/spawned.txt\"\nprintf 'src/main.rs:1:1: should not run\\n'\n",
        );

        for target in [TargetMode::Append, TargetMode::Stdin] {
            let config = regex_config(command.clone(), target);
            let error =
                run_linter(request(&config, &workspace, None, Some("content"))).assert_err();

            match error {
                LintToolError::InvalidConfig(message) => {
                    assert!(message.contains("target_path"));
                }
                other => panic!("expected InvalidConfig for missing target_path, got {other:?}"),
            }
            assert!(!workspace.root.join("spawned.txt").exists());
        }
    }

    #[test]
    fn target_mode_none_ignores_target_path_and_stdin_content_for_command_construction() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "none_ignores.sh",
            "#!/bin/sh\ndir=$(dirname \"$0\")\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done > \"$dir/args.txt\"\ncat > \"$dir/stdin.txt\"\nprintf 'src/main.rs:1:1: ignored extras\\n'\n",
        );
        let config = regex_config(command, TargetMode::None);

        let outcome = run_linter(request(
            &config,
            &workspace,
            Some("src/ignored.rs"),
            Some("ignored stdin"),
        ))
        .assert_ok();

        assert!(outcome.supported);
        assert_eq!(workspace.read("args.txt"), "");
        assert_eq!(workspace.read("stdin.txt"), "");
    }

    #[test]
    fn runner_combines_stdout_and_stderr_before_parser_invocation() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "stdout_stderr.sh",
            "#!/bin/sh\nprintf 'src/stdout.rs:1:1: stdout diagnostic\\n'\nprintf 'src/stderr.rs:2:1: stderr diagnostic\\n' >&2\n",
        );
        let config = regex_config(command, TargetMode::None);

        let outcome = run_linter(request(&config, &workspace, None, None)).assert_ok();

        let paths: Vec<_> = outcome
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.path.as_str())
            .collect();
        assert_eq!(paths, vec!["src/stderr.rs", "src/stdout.rs"]);
    }

    #[test]
    fn missing_command_returns_missing_binary_and_code_lint_missing_binary() {
        let workspace = TestWorkspace::new();
        let config = regex_config(
            workspace
                .root
                .join("does-not-exist")
                .to_string_lossy()
                .into_owned(),
            TargetMode::None,
        );

        let error = run_linter(request(&config, &workspace, None, None)).assert_err();

        assert_eq!(
            error,
            LintToolError::MissingBinary {
                command: config.command.clone()
            }
        );
        assert_eq!(error.code(), "lint_missing_binary");
    }

    #[test]
    fn timeout_or_hanging_command_returns_timeout_without_blocking_indefinitely() {
        let workspace = TestWorkspace::new();
        let command = workspace.command("hang.sh", "#!/bin/sh\nsleep 5\n");
        let config = regex_config(command, TargetMode::None);
        let mut run_request = request(&config, &workspace, None, None);
        run_request.timeout = Duration::from_millis(50);

        assert_timeout(|| run_linter(run_request));
    }

    #[test]
    fn blocked_stdin_writer_obeys_command_timeout() {
        let workspace = TestWorkspace::new();
        let command = workspace.command("ignore_stdin.sh", "#!/bin/sh\nsleep 5\n");
        let config = regex_config(command, TargetMode::Stdin);
        let stdin_content = "x".repeat(OUTPUT_LIMIT_BYTES * 2);
        let mut run_request = request(
            &config,
            &workspace,
            Some("src/lib.rs"),
            Some(stdin_content.as_str()),
        );
        run_request.timeout = Duration::from_millis(50);

        assert_timeout(|| run_linter(run_request));
    }

    #[test]
    fn fix_extraction_obeys_command_timeout() {
        let workspace = TestWorkspace::new();
        let command = workspace.command("fix_hang.sh", "#!/bin/sh\nsleep 5\n");
        let config = regex_config(command, TargetMode::None);
        let mut run_request = request(&config, &workspace, None, None);
        run_request.timeout = Duration::from_millis(50);

        assert_timeout(|| run_linter_fixes(run_request));
    }

    fn assert_timeout<T>(run: impl FnOnce() -> Result<T, LintToolError>) {
        let started = Instant::now();
        let error = run().assert_err();
        assert_eq!(error, LintToolError::Timeout);
        assert_eq!(error.code(), "lint_timeout");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_linter_terminates_its_process_tree_on_supported_unix_hosts() {
        // The 250 ms allowance covers CI scheduling jitter; the production contract
        // permits only 500 ms beyond the 50 ms execution deadline.
        const EXECUTION_DEADLINE: Duration = Duration::from_millis(50);
        const CLEANUP_ALLOWANCE: Duration = Duration::from_millis(500);
        const SCHEDULING_TOLERANCE: Duration = Duration::from_millis(250);
        let maximum_return_time = EXECUTION_DEADLINE + CLEANUP_ALLOWANCE + SCHEDULING_TOLERANCE;
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "descendant_ignores_term.sh",
            "#!/bin/sh\necho $$ > direct-child.pid\nsh -c 'trap \"\" TERM; echo $$ > descendant.pid; while :; do sleep 1; done' &\nwhile [ ! -f descendant.pid ]; do :; done\nwait\n",
        );
        let config = regex_config(command, TargetMode::None);
        let mut run_request = request(&config, &workspace, None, None);
        run_request.timeout = EXECUTION_DEADLINE;

        let started = Instant::now();
        let error = run_linter(run_request).assert_err();
        let elapsed = started.elapsed();
        let direct_child_pid = workspace.read("direct-child.pid").trim().to_owned();
        let descendant_pid = workspace.read("descendant.pid").trim().to_owned();
        let mut cleanup = ProcessCleanup::new();
        cleanup.retain_until_test_end(descendant_pid.clone());

        assert_eq!(error, LintToolError::Timeout);
        assert_eq!(error.code(), "lint_timeout");
        assert!(
            elapsed <= maximum_return_time,
            "timeout cleanup took {elapsed:?}; allowed {maximum_return_time:?} (50 ms deadline + 500 ms cleanup + 250 ms scheduling tolerance)"
        );
        assert!(
            !process_is_alive(&direct_child_pid),
            "the direct child ({direct_child_pid}) was not reaped before the timeout result"
        );

        assert!(
            !process_is_alive(&descendant_pid),
            "timed-out descendant ({descendant_pid}) remained alive"
        );

        let command = workspace.command(
            "exited_parent_leaves_pipes.sh",
            "#!/bin/sh\nsh -c 'trap \"\" TERM; echo $$ > pipe_holder.pid; sleep 5' &\nexit 0\n",
        );
        let config = regex_config(command, TargetMode::None);
        let mut run_request = request(&config, &workspace, None, None);
        run_request.timeout = EXECUTION_DEADLINE;

        let started = Instant::now();
        let error = run_linter(run_request).assert_err();
        let elapsed = started.elapsed();
        let pipe_holder_pid = workspace.read("pipe_holder.pid").trim().to_owned();
        cleanup.retain_until_test_end(pipe_holder_pid.clone());

        assert_eq!(error, LintToolError::Timeout);
        assert_eq!(error.code(), "lint_timeout");
        assert!(
            elapsed <= maximum_return_time,
            "pipe cleanup took {elapsed:?}; allowed {maximum_return_time:?} (50 ms deadline + 500 ms cleanup + 250 ms scheduling tolerance)"
        );

        assert!(
            !process_is_alive(&pipe_holder_pid),
            "pipe-holding descendant ({pipe_holder_pid}) remained alive"
        );
    }

    #[test]
    fn non_zero_exit_with_parseable_diagnostics_returns_supported_true_diagnostics_instead_of_tool_error()
     {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "nonzero_with_diagnostics.sh",
            "#!/bin/sh\nprintf 'src/main.rs:3:4: failing lint\\n'\nexit 7\n",
        );
        let config = regex_config(command, TargetMode::None);

        let outcome = run_linter(request(&config, &workspace, None, None)).assert_ok();

        assert!(outcome.supported);
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(outcome.diagnostics[0].path, "src/main.rs");
        assert_eq!(outcome.diagnostics[0].diagnostic.severity, Severity::Error);
    }

    #[test]
    fn run_linter_keeps_structured_fix_payloads_diagnostic_only_until_fix_orchestration() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "rustc_fix_payload.sh",
            "#!/bin/sh\nprintf '%s\\n' '{\"reason\":\"compiler-message\",\"message\":{\"message\":\"use `is_empty`\",\"level\":\"warning\",\"code\":{\"code\":\"clippy::len_zero\"},\"spans\":[{\"file_name\":\"src/lib.rs\",\"is_primary\":true,\"line_start\":4,\"column_start\":8,\"line_end\":4,\"column_end\":17,\"byte_start\":42,\"byte_end\":51,\"suggested_replacement\":\"items.is_empty()\",\"applicability\":\"MachineApplicable\"}]}}'\n",
        );
        let mut config = regex_config(command, TargetMode::None);
        config.format = ParserFormat::RustcJson;
        config.regex = None;

        let outcome = run_linter(request(&config, &workspace, None, None)).assert_ok();

        assert!(outcome.supported);
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(outcome.diagnostics[0].path, "src/lib.rs");
        assert_eq!(outcome.diagnostics[0].diagnostic.message, "use `is_empty`");
        assert_eq!(
            outcome.diagnostics[0].diagnostic.code.as_deref(),
            Some("clippy::len_zero")
        );
    }

    #[test]
    fn non_zero_exit_without_parseable_diagnostics_returns_nonzero_exit_and_code_lint_nonzero_exit()
    {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "nonzero_without_diagnostics.sh",
            "#!/bin/sh\nprintf 'no diagnostics here\\n'\nexit 9\n",
        );
        let config = regex_config(command, TargetMode::None);

        let error = run_linter(request(&config, &workspace, None, None)).assert_err();

        assert_eq!(error, LintToolError::NonzeroExit { code: Some(9) });
        assert_eq!(error.code(), "lint_nonzero_exit");
    }

    #[test]
    fn parser_no_diagnostics_on_zero_exit_returns_supported_true_with_no_diagnostics() {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "zero_without_diagnostics.sh",
            "#!/bin/sh\nprintf 'no diagnostics here\\n'\n",
        );
        let config = regex_config(command, TargetMode::None);

        let outcome = run_linter(request(&config, &workspace, None, None)).assert_ok();

        assert!(outcome.supported);
        assert!(outcome.diagnostics.is_empty());
    }

    #[test]
    fn runtime_config_inconsistencies_including_generic_regex_without_regex_return_invalid_config()
    {
        let workspace = TestWorkspace::new();
        let command = workspace.command(
            "generic_without_regex.sh",
            "#!/bin/sh\nprintf 'src/main.rs:1:1: diagnostic\\n'\n",
        );
        let mut config = regex_config(command, TargetMode::None);
        config.regex = None;

        let error = run_linter(request(&config, &workspace, None, None)).assert_err();

        match error {
            LintToolError::InvalidConfig(message) => {
                assert!(message.contains("regex"));
            }
            other => panic!("expected InvalidConfig for missing generic regex, got {other:?}"),
        }
    }

    #[test]
    fn parser_errors_map_to_lint_tool_errors_by_exit_status_and_parser_error_kind() {
        let workspace = TestWorkspace::new();
        let command = workspace.command("invalid_json.sh", "#!/bin/sh\nprintf '{not json\\n'\n");
        let mut config = regex_config(command, TargetMode::None);
        config.format = ParserFormat::RustcJson;
        config.regex = None;

        let zero_exit = run_linter(request(&config, &workspace, None, None)).assert_err();
        assert_eq!(zero_exit, LintToolError::UnparseableOutput);

        let command = workspace.command(
            "invalid_regex.sh",
            "#!/bin/sh\nprintf 'src/main.rs:1:1: diagnostic\\n'\n",
        );
        let mut config = regex_config(command, TargetMode::None);
        config.regex = Some("(".to_owned());

        let invalid_config = run_linter(request(&config, &workspace, None, None)).assert_err();
        match invalid_config {
            LintToolError::InvalidConfig(message) => {
                assert!(message.contains("regex"));
            }
            other => panic!("expected InvalidConfig for invalid regex, got {other:?}"),
        }

        let command = workspace.command(
            "missing_capture_group.sh",
            "#!/bin/sh\nprintf 'src/main.rs:1:1: diagnostic\\n'\n",
        );
        let mut config = regex_config(command, TargetMode::None);
        config.regex = Some(r"(?P<line>\d+):(?P<col>\d+): (?P<message>.+)".to_owned());

        let missing_capture = run_linter(request(&config, &workspace, None, None)).assert_err();
        match missing_capture {
            LintToolError::InvalidConfig(message) => {
                assert!(message.contains("file"));
            }
            other => panic!("expected InvalidConfig for missing capture group, got {other:?}"),
        }

        let command = workspace.command(
            "unsafe_path.sh",
            "#!/bin/sh\nprintf '../outside.rs:1:1: escaped workspace\\n'\n",
        );
        let config = regex_config(command, TargetMode::None);

        let unsafe_path = run_linter(request(&config, &workspace, None, None)).assert_err();
        assert_eq!(unsafe_path, LintToolError::UnparseableOutput);
    }
}
