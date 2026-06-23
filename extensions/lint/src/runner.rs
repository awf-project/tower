#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use core_engine::adapters::config::lint::{ParserFormat, TargetMode};

use crate::config::RunnerLintConfig;
use crate::diagnostics::LintDiagnostic;
use crate::fixes::LintFix;
use crate::parsers::{ParserError, extract_linter_fixes, parse_linter_output};

const OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

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

    let args = command_args(request)?;
    let mut child = spawn_command(request, &args)?;

    let stdout = child.stdout.take().map(read_output);
    let stderr = child.stderr.take().map(read_output);
    let stdin = if let TargetMode::Stdin = request.config.target {
        let content = request
            .stdin_content
            .ok_or_else(|| LintToolError::InvalidConfig("missing stdin_content".to_owned()))?
            .to_owned();
        Some(write_stdin(
            child
                .stdin
                .take()
                .ok_or_else(|| LintToolError::InvalidConfig("missing stdin pipe".to_owned()))?,
            content,
        ))
    } else {
        None
    };
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let status = wait_with_timeout(&mut child, request.timeout);
        let _ = tx.send(status);
    });

    let status = rx
        .recv()
        .map_err(|_| LintToolError::InvalidConfig("failed to wait for lint command".to_owned()))?;
    let status = match status {
        Ok(status) => {
            join_stdin_writer(stdin)?;
            status
        }
        Err(error) => return Err(error),
    };

    let mut output = Vec::new();
    append_reader_output(&mut output, stdout)?;
    append_reader_output(&mut output, stderr)?;
    let output = String::from_utf8_lossy(&output);
    Ok((output.into_owned(), status.code(), status.success()))
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
) -> Result<std::process::Child, LintToolError> {
    let deadline = std::time::Instant::now() + Duration::from_millis(100);

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

        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(LintToolError::MissingBinary {
                    command: request.config.command.clone(),
                });
            }
            Err(error) if is_executable_busy(&error) && std::time::Instant::now() < deadline => {
                thread::sleep(WAIT_POLL_INTERVAL);
            }
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

fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, LintToolError> {
    let start = std::time::Instant::now();

    loop {
        if let Some(status) = child.try_wait().map_err(|_| {
            LintToolError::InvalidConfig("failed to wait for lint command".to_owned())
        })? {
            return Ok(status);
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(LintToolError::Timeout);
        }

        thread::sleep(WAIT_POLL_INTERVAL.min(timeout.saturating_sub(start.elapsed())));
    }
}

fn read_output<R>(mut reader: R) -> thread::JoinHandle<Result<Vec<u8>, LintToolError>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
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

fn write_stdin(
    mut stdin: ChildStdin,
    content: String,
) -> thread::JoinHandle<Result<(), LintToolError>> {
    thread::spawn(move || {
        stdin
            .write_all(content.as_bytes())
            .map_err(|_| LintToolError::InvalidConfig("failed to write stdin_content".to_owned()))
    })
}

fn join_stdin_writer(
    handle: Option<thread::JoinHandle<Result<(), LintToolError>>>,
) -> Result<(), LintToolError> {
    if let Some(handle) = handle {
        handle.join().map_err(|_| {
            LintToolError::InvalidConfig("stdin writer thread panicked".to_owned())
        })??;
    }
    Ok(())
}

fn append_reader_output(
    output: &mut Vec<u8>,
    handle: Option<thread::JoinHandle<Result<Vec<u8>, LintToolError>>>,
) -> Result<(), LintToolError> {
    if let Some(handle) = handle {
        let mut stream = handle
            .join()
            .map_err(|_| LintToolError::UnparseableOutput)??;
        output.append(&mut stream);
    }
    Ok(())
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

    fn regex_config(command: String, target: TargetMode) -> RunnerLintConfig {
        RunnerLintConfig {
            command,
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
    fn config_rs_adapts_host_lint_config_into_extension_runner_configuration_without_diverging() {
        let host_config = LintCommandConfig {
            command: "cargo".to_owned(),
            args: vec!["check".to_owned(), "--message-format=json".to_owned()],
            extensions: vec!["rs".to_owned()],
            format: ParserFormat::RustcJson,
            target: TargetMode::Append,
            regex: None,
            source: Some("cargo-check".to_owned()),
        };

        let runner_config = RunnerLintConfig::from(&host_config);

        assert_eq!(runner_config.command, host_config.command);
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
        let mut request = request(&config, &workspace, None, None);
        request.timeout = Duration::from_millis(50);

        let error = run_linter(request).assert_err();

        assert_eq!(error, LintToolError::Timeout);
        assert_eq!(error.code(), "lint_timeout");
    }

    #[test]
    fn stdin_mode_timeout_supervises_blocked_stdin_writes() {
        let workspace = TestWorkspace::new();
        let command = workspace.command("ignore_stdin.sh", "#!/bin/sh\nsleep 5\n");
        let config = regex_config(command, TargetMode::Stdin);
        let stdin_content = "x".repeat(OUTPUT_LIMIT_BYTES * 2);
        let mut request = request(
            &config,
            &workspace,
            Some("src/lib.rs"),
            Some(stdin_content.as_str()),
        );
        request.timeout = Duration::from_millis(50);

        let started = Instant::now();
        let error = run_linter(request).assert_err();

        assert_eq!(error, LintToolError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
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
