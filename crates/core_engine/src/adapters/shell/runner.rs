use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::adapters::config::ShellConfig;
use crate::adapters::config::shell::valid_command_name;
use crate::adapters::process::{
    BoundedOutput, ProcessError, ProcessRequest, TerminationStatus, run_supervised_process,
};
use crate::domain::{CancellationSignal, RelativePath};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellCommandRequest {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) cwd: Option<RelativePath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ShellCommandResult {
    pub(crate) status: TerminationStatus,
    pub(crate) stdout: ShellOutput,
    pub(crate) stderr: ShellOutput,
    pub(crate) timed_out: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ShellOutput {
    Text {
        text: String,
        byte_count: u64,
        truncated: bool,
    },
    Bytes {
        base64: String,
        byte_count: u64,
        truncated: bool,
    },
}

impl From<BoundedOutput> for ShellOutput {
    fn from(output: BoundedOutput) -> Self {
        let byte_count = output.bytes.len() as u64;
        match String::from_utf8(output.bytes) {
            Ok(text) => Self::Text {
                text,
                byte_count,
                truncated: output.truncated,
            },
            Err(error) => Self::Bytes {
                base64: encode_base64(error.as_bytes()),
                byte_count,
                truncated: output.truncated,
            },
        }
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }

    encoded
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellRunnerError {
    InvalidCommand,
    CommandNotAllowed,
    InvalidWorkingDirectory,
    Process(ProcessError),
    Cancelled,
}

impl fmt::Display for ShellRunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidCommand => "invalid command",
            Self::CommandNotAllowed => "command is not allowed",
            Self::InvalidWorkingDirectory => {
                "working directory must be an existing directory within the workspace"
            }
            Self::Process(error) => return fmt::Display::fmt(error, formatter),
            Self::Cancelled => "command execution cancelled",
        };

        formatter.write_str(message)
    }
}

impl std::error::Error for ShellRunnerError {}

pub(crate) struct ShellCommandRunner {
    policy: Arc<ShellConfig>,
    workspace_root: PathBuf,
}

impl ShellCommandRunner {
    pub(crate) fn new(policy: Arc<ShellConfig>, workspace_root: PathBuf) -> Self {
        Self {
            policy,
            workspace_root,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.policy.enabled
    }

    pub(crate) fn run(
        &self,
        request: ShellCommandRequest,
        cancellation: &dyn CancellationSignal,
    ) -> Result<ShellCommandResult, ShellRunnerError> {
        if !valid_command_name(&request.command)
            || request.args.iter().any(|argument| argument.contains('\0'))
        {
            return Err(ShellRunnerError::InvalidCommand);
        }

        let command = self
            .policy
            .commands
            .iter()
            .find(|command| command.name == request.command)
            .ok_or(ShellRunnerError::CommandNotAllowed)?;
        let cwd = resolve_working_directory(&self.workspace_root, request.cwd.as_ref())?;
        let timeout = Duration::from_secs(self.policy.timeout_secs);
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(ShellRunnerError::InvalidCommand)?;
        let stdout_limit = usize::try_from(self.policy.stdout_max_bytes)
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or(ShellRunnerError::InvalidCommand)?;
        let stderr_limit = usize::try_from(self.policy.stderr_max_bytes)
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or(ShellRunnerError::InvalidCommand)?;
        let mut argv = Vec::with_capacity(command.argv.len() + request.args.len());
        argv.extend(command.argv.iter().cloned());
        argv.extend(request.args);

        let outcome = run_supervised_process(
            ProcessRequest {
                argv,
                cwd,
                deadline: Some(deadline),
                stdin: None,
                stdout_limit: Some(stdout_limit),
                stderr_limit: Some(stderr_limit),
                cleanup_budget: Duration::from_secs(2),
            },
            cancellation,
        )
        .map_err(|error| match error {
            ProcessError::Cancelled => ShellRunnerError::Cancelled,
            error => ShellRunnerError::Process(error),
        })?;

        Ok(ShellCommandResult {
            status: outcome.status,
            stdout: outcome.stdout.into(),
            stderr: outcome.stderr.into(),
            timed_out: outcome.timed_out,
        })
    }
}

fn resolve_working_directory(
    workspace_root: &Path,
    requested: Option<&RelativePath>,
) -> Result<PathBuf, ShellRunnerError> {
    let canonical_root = workspace_root
        .canonicalize()
        .map_err(|_| ShellRunnerError::InvalidWorkingDirectory)?;
    let relative = requested.map_or_else(|| Path::new(""), |path| Path::new(path.as_str()));
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ShellRunnerError::InvalidWorkingDirectory);
    }

    let resolved = canonical_root
        .join(relative)
        .canonicalize()
        .map_err(|_| ShellRunnerError::InvalidWorkingDirectory)?;
    if !resolved.is_dir() || !resolved.starts_with(&canonical_root) {
        return Err(ShellRunnerError::InvalidWorkingDirectory);
    }
    Ok(resolved)
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::adapters::config::AllowedCommand;
    use crate::adapters::process::test_support::{executable, process_is_alive};
    use crate::domain::NeverCancelled;

    use crate::adapters::process::process_fixture_lock;

    struct ToggleCancellation(AtomicBool);

    impl ToggleCancellation {
        fn new() -> Self {
            Self(AtomicBool::new(false))
        }

        fn cancel(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    impl CancellationSignal for ToggleCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    fn policy(commands: Vec<AllowedCommand>, stdout: u64, stderr: u64) -> Arc<ShellConfig> {
        Arc::new(ShellConfig {
            enabled: true,
            commands,
            timeout_secs: 1,
            stdout_max_bytes: stdout,
            stderr_max_bytes: stderr,
        })
    }

    fn allowed(name: &str, argv: Vec<String>) -> AllowedCommand {
        AllowedCommand {
            name: name.to_owned(),
            argv,
        }
    }

    fn request(command: &str, args: &[&str], cwd: Option<&str>) -> ShellCommandRequest {
        ShellCommandRequest {
            command: command.to_owned(),
            args: args.iter().map(|argument| (*argument).to_owned()).collect(),
            cwd: cwd.map(RelativePath::new),
        }
    }

    fn runner_with_script(body: &str, stdout: u64, stderr: u64) -> (TempDir, ShellCommandRunner) {
        let workspace = tempdir().unwrap();
        let script = workspace.path().join("fixture");
        executable(&script, body);
        let runner = ShellCommandRunner::new(
            policy(
                vec![allowed(
                    "fixture",
                    vec![script.to_string_lossy().into_owned()],
                )],
                stdout,
                stderr,
            ),
            workspace.path().to_path_buf(),
        );
        (workspace, runner)
    }

    #[test]
    fn shell_result_serialization_matches_the_wire_contract() {
        let _fixture_lock = process_fixture_lock();
        for (status, expected) in [
            (
                TerminationStatus::Exited { code: 0 },
                serde_json::json!({"kind": "exited", "code": 0}),
            ),
            (
                TerminationStatus::Signalled,
                serde_json::json!({"kind": "signalled"}),
            ),
            (
                TerminationStatus::OtherFailure,
                serde_json::json!({"kind": "other_failure"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), expected);
        }

        let text = ShellOutput::Text {
            text: "ok".to_owned(),
            byte_count: 2,
            truncated: false,
        };
        assert_eq!(
            serde_json::to_value(&text).unwrap(),
            serde_json::json!({
                "kind": "text", "text": "ok", "byte_count": 2, "truncated": false,
            })
        );

        let bytes = ShellOutput::Bytes {
            base64: "/wA=".to_owned(),
            byte_count: 2,
            truncated: true,
        };
        let encoded = serde_json::to_value(&bytes).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "kind": "bytes", "base64": "/wA=", "byte_count": 2, "truncated": true,
            })
        );
        assert_eq!(
            serde_json::from_value::<ShellOutput>(encoded).unwrap(),
            bytes,
            "binary output must retain its standard padded base64 bytes and raw byte count"
        );

        let result = ShellCommandResult {
            status: TerminationStatus::Signalled,
            stdout: ShellOutput::Text {
                text: String::new(),
                byte_count: 0,
                truncated: false,
            },
            stderr: ShellOutput::Text {
                text: String::new(),
                byte_count: 0,
                truncated: false,
            },
            timed_out: true,
        };
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "status": {"kind": "signalled"},
                "stdout": {"kind": "text", "text": "", "byte_count": 0, "truncated": false},
                "stderr": {"kind": "text", "text": "", "byte_count": 0, "truncated": false},
                "timed_out": true,
            })
        );
    }

    #[test]
    fn supervisor_errors_are_mapped_at_the_runner_entry_point() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let cwd = workspace.path().join("cwd-sentinel");
        fs::create_dir(&cwd).unwrap();
        let runner = ShellCommandRunner::new(
            policy(
                vec![allowed(
                    "fixture",
                    vec!["missing-configured-executable-sentinel".to_owned()],
                )],
                32,
                32,
            ),
            workspace.path().to_path_buf(),
        );

        let error = runner
            .run(
                request(
                    "fixture",
                    &["request-argument-sentinel"],
                    Some("cwd-sentinel"),
                ),
                &NeverCancelled,
            )
            .expect_err("a supervisor spawn failure must remain a runner process error");

        assert_eq!(error, ShellRunnerError::Process(ProcessError::SpawnFailed));
        assert_eq!(error.to_string(), "process spawn failed");
        let rendered = format!("{error:?} {error}");
        for sentinel in [
            "missing-configured-executable-sentinel",
            "request-argument-sentinel",
            "cwd-sentinel",
        ] {
            assert!(!rendered.contains(sentinel));
        }
    }

    #[test]
    fn cancellation_maps_to_a_sanitized_runner_error_after_sensitive_producers_run() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let cwd = workspace.path().join("cwd-sentinel");
        fs::create_dir(&cwd).unwrap();
        let marker = workspace.path().join("producer-ran");
        let script = workspace.path().join("configured-executable-sentinel");
        executable(
            &script,
            "#!/bin/sh\nprintf '%s|%s|%s' \"$1\" \"$3\" \"$PWD\" > \"$2\"\nprintf 'stream-producer-sentinel'\nprintf 'stream-producer-sentinel' >&2\nwhile :; do sleep 1; done\n",
        );
        let runner = ShellCommandRunner::new(
            Arc::new(ShellConfig {
                enabled: true,
                commands: vec![allowed(
                    "fixture",
                    vec![
                        script.to_string_lossy().into_owned(),
                        "configured-argv-sentinel".to_owned(),
                        marker.to_string_lossy().into_owned(),
                    ],
                )],
                // Keep the cancellation scenario isolated from the independent
                // operation-timeout path when the full suite is under load.
                timeout_secs: 30,
                stdout_max_bytes: 64,
                stderr_max_bytes: 64,
            }),
            workspace.path().to_path_buf(),
        );
        let cancellation = Arc::new(ToggleCancellation::new());
        let cancellation_for_runner = Arc::clone(&cancellation);
        let runner_thread = thread::spawn(move || {
            runner.run(
                request(
                    "fixture",
                    &["request-argument-sentinel"],
                    Some("cwd-sentinel"),
                ),
                cancellation_for_runner.as_ref(),
            )
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while fs::read_to_string(&marker).map_or(true, |contents| {
            !contents.contains("request-argument-sentinel")
        }) && Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert!(
            marker.exists(),
            "the configured argv, cwd, and stream producers must run"
        );
        assert!(
            fs::read_to_string(&marker)
                .unwrap()
                .contains("configured-argv-sentinel")
        );
        assert!(
            fs::read_to_string(&marker)
                .unwrap()
                .contains("request-argument-sentinel")
        );
        assert!(
            fs::read_to_string(&marker)
                .unwrap()
                .contains("cwd-sentinel")
        );
        cancellation.cancel();

        let error = runner_thread
            .join()
            .unwrap()
            .expect_err("cancellation must be mapped");
        assert_eq!(error, ShellRunnerError::Cancelled);
        assert_eq!(error.to_string(), "command execution cancelled");
        let rendered = format!("{error:?} {error}");
        for sentinel in [
            "configured-executable-sentinel",
            "configured-argv-sentinel",
            "request-argument-sentinel",
            "cwd-sentinel",
            "stream-producer-sentinel",
        ] {
            assert!(!rendered.contains(sentinel));
        }
    }

    #[test]
    fn disallowed_command_is_rejected_before_spawn() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let marker = workspace.path().join("spawned");
        let script = workspace.path().join("fixture");
        executable(
            &script,
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let runner = ShellCommandRunner::new(
            policy(
                vec![allowed(
                    "allowed",
                    vec![script.to_string_lossy().into_owned()],
                )],
                32,
                32,
            ),
            workspace.path().to_path_buf(),
        );

        for (candidate, expected) in [
            ("", ShellRunnerError::InvalidCommand),
            ("bad/name", ShellRunnerError::InvalidCommand),
            ("allowed-prefix", ShellRunnerError::CommandNotAllowed),
        ] {
            assert_eq!(
                runner.run(request(candidate, &[], None), &NeverCancelled),
                Err(expected)
            );
            assert!(!marker.exists());
        }
        assert_eq!(
            runner.run(
                request("allowed", &["nul\0argument"], None),
                &NeverCancelled
            ),
            Err(ShellRunnerError::InvalidCommand)
        );
        assert!(!marker.exists());

        assert_eq!(
            ShellRunnerError::InvalidCommand.to_string(),
            "invalid command"
        );
        assert_eq!(
            ShellRunnerError::CommandNotAllowed.to_string(),
            "command is not allowed"
        );
        assert_eq!(
            ShellRunnerError::InvalidWorkingDirectory.to_string(),
            "working directory must be an existing directory within the workspace"
        );
        assert_eq!(
            ShellRunnerError::Process(ProcessError::SpawnFailed).to_string(),
            "process spawn failed"
        );
        assert_eq!(
            ShellRunnerError::Cancelled.to_string(),
            "command execution cancelled"
        );

        let defaults: ShellCommandRequest =
            serde_json::from_str(r#"{"command":"allowed"}"#).unwrap();
        assert!(defaults.args.is_empty());
        assert_eq!(defaults.cwd, None);
        assert!(
            serde_json::from_str::<ShellCommandRequest>(
                r#"{"command":"allowed","unexpected":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn working_directory_must_resolve_within_workspace() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let contained = workspace.path().join("contained");
        fs::create_dir(&contained).unwrap();
        fs::write(workspace.path().join("file"), b"not a directory").unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("escape")).unwrap();
        let marker = workspace.path().join("spawned");
        let script = workspace.path().join("fixture");
        executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$PWD\" >> '{}'\n",
                marker.display()
            ),
        );
        let runner = ShellCommandRunner::new(
            policy(
                vec![allowed(
                    "fixture",
                    vec![script.to_string_lossy().into_owned()],
                )],
                512,
                32,
            ),
            workspace.path().to_path_buf(),
        );

        runner
            .run(request("fixture", &[], None), &NeverCancelled)
            .unwrap();
        runner
            .run(request("fixture", &[], Some("contained")), &NeverCancelled)
            .unwrap();
        let observed = fs::read_to_string(&marker).unwrap();
        assert_eq!(
            observed.lines().collect::<Vec<_>>(),
            vec![
                workspace.path().canonicalize().unwrap().to_string_lossy(),
                contained.canonicalize().unwrap().to_string_lossy(),
            ]
        );

        fs::remove_file(&marker).unwrap();
        for cwd in [
            "/tmp",
            "../outside",
            "contained/../contained",
            "missing",
            "file",
            "escape",
        ] {
            assert_eq!(
                runner.run(request("fixture", &[], Some(cwd)), &NeverCancelled),
                Err(ShellRunnerError::InvalidWorkingDirectory),
                "cwd {cwd:?} must be rejected"
            );
            assert!(!marker.exists(), "cwd {cwd:?} spawned a process");
        }
    }

    #[test]
    fn allowed_command_appends_literal_arguments_without_shell_interpretation() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let capture = workspace.path().join("capture");
        let wrong = workspace.path().join("wrong");
        executable(
            &capture,
            "#!/bin/sh\nprintf 'capture\\n'; printf '<%s>\\n' \"$@\"\n",
        );
        executable(&wrong, "#!/bin/sh\nprintf 'wrong\\n'\n");
        let runner = ShellCommandRunner::new(
            policy(
                vec![
                    allowed("capture", vec![capture.to_string_lossy().into_owned()]),
                    allowed("capture-prefix", vec![wrong.to_string_lossy().into_owned()]),
                ],
                512,
                32,
            ),
            workspace.path().to_path_buf(),
        );

        let result = runner
            .run(
                request(
                    "capture",
                    &["; touch injected", "two words", "", "capture-prefix"],
                    None,
                ),
                &NeverCancelled,
            )
            .unwrap();

        assert_eq!(result.status, TerminationStatus::Exited { code: 0 });
        assert_eq!(
            result.stdout,
            ShellOutput::Text {
                text: "capture\n<; touch injected>\n<two words>\n<>\n<capture-prefix>\n".to_owned(),
                byte_count: 59,
                truncated: false,
            }
        );
        assert!(!workspace.path().join("injected").exists());
    }

    #[test]
    fn command_output_is_bounded() {
        let _fixture_lock = process_fixture_lock();
        let (_workspace, runner) = runner_with_script(
            "#!/bin/sh\nprintf 'abc\\342\\202\\254zz'; printf 'abcdefghijk' >&2\n",
            4,
            7,
        );

        let result = runner
            .run(request("fixture", &[], None), &NeverCancelled)
            .unwrap();

        assert_eq!(
            result.stdout,
            ShellOutput::Bytes {
                base64: "YWJj4g==".to_owned(),
                byte_count: 4,
                truncated: true,
            }
        );
        assert_eq!(
            result.stderr,
            ShellOutput::Text {
                text: "abcdefg".to_owned(),
                byte_count: 7,
                truncated: true,
            }
        );
        assert!(!result.timed_out);
    }

    #[test]
    fn non_utf8_command_output_is_returned_as_bytes() {
        let _fixture_lock = process_fixture_lock();
        let (_workspace, runner) = runner_with_script("#!/bin/sh\nprintf '\\377\\000'\n", 16, 16);

        let result = runner
            .run(request("fixture", &[], None), &NeverCancelled)
            .unwrap();

        assert_eq!(
            result.stdout,
            ShellOutput::Bytes {
                base64: "/wA=".to_owned(),
                byte_count: 2,
                truncated: false,
            }
        );
    }

    #[test]
    fn unsuccessful_command_preserves_exit_status_and_output() {
        let _fixture_lock = process_fixture_lock();
        let (_workspace, runner) = runner_with_script(
            "#!/bin/sh\nprintf stdout-value; printf stderr-value >&2; exit 17\n",
            64,
            64,
        );

        let result = runner
            .run(request("fixture", &[], None), &NeverCancelled)
            .unwrap();

        assert_eq!(result.status, TerminationStatus::Exited { code: 17 });
        assert_eq!(
            result.stdout,
            ShellOutput::Text {
                text: "stdout-value".to_owned(),
                byte_count: 12,
                truncated: false,
            }
        );
        assert_eq!(
            result.stderr,
            ShellOutput::Text {
                text: "stderr-value".to_owned(),
                byte_count: 12,
                truncated: false,
            }
        );
        assert!(!result.timed_out);
    }

    #[test]
    fn timed_out_shell_command_reports_after_tree_cleanup() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let pid_path = workspace.path().join("descendant.pid");
        let script = workspace.path().join("fixture");
        executable(
            &script,
            "#!/bin/sh\nsh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" > \"$1\"; while :; do sleep 1; done' -- \"$1\" &\nwhile [ ! -s \"$1\" ]; do :; done\nwait\n",
        );
        let runner = ShellCommandRunner::new(
            policy(
                vec![allowed(
                    "fixture",
                    vec![
                        script.to_string_lossy().into_owned(),
                        pid_path.to_string_lossy().into_owned(),
                    ],
                )],
                32,
                32,
            ),
            workspace.path().to_path_buf(),
        );
        let started = Instant::now();

        let result = runner
            .run(request("fixture", &[], None), &NeverCancelled)
            .unwrap();

        assert!(result.timed_out);
        assert!(matches!(
            result.status,
            TerminationStatus::Exited { .. }
                | TerminationStatus::Signalled
                | TerminationStatus::OtherFailure
        ));
        assert!(started.elapsed() <= Duration::from_secs(3));
        let pid = fs::read_to_string(&pid_path).unwrap();
        assert!(
            !process_is_alive(pid.trim()),
            "runner returned before descendant cleanup"
        );
    }

    #[test]
    fn concurrent_commands_keep_output_and_deadlines_isolated() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let script = workspace.path().join("fixture");
        executable(
            &script,
            "#!/bin/sh\nprintf '%s' \"$1\"; printf 'err-%s' \"$1\" >&2\nprintf '%s' \"$$\" > \"$1.pid\"\nwhile [ ! -f release ]; do :; done\n",
        );
        let commands = vec![allowed(
            "fixture",
            vec![script.to_string_lossy().into_owned()],
        )];
        // Keep the survivor alive until the short-deadline invocation has been
        // fully cleaned up. Separate deadlines make cross-invocation kills visible.
        let mut survivor_policy = (*policy(commands.clone(), 32, 32)).clone();
        survivor_policy.timeout_secs = 30;
        let survivor_runner =
            ShellCommandRunner::new(Arc::new(survivor_policy), workspace.path().to_path_buf());
        let survivor = thread::spawn(move || {
            survivor_runner.run(request("fixture", &["survivor"], None), &NeverCancelled)
        });
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        let survivor_pid = loop {
            if let Ok(pid) = fs::read_to_string(workspace.path().join("survivor.pid"))
                && !pid.is_empty()
            {
                break pid;
            }
            assert!(Instant::now() < ready_deadline, "survivor did not start");
            thread::yield_now();
        };
        let runner =
            ShellCommandRunner::new(policy(commands, 32, 32), workspace.path().to_path_buf());
        let slow = runner
            .run(request("fixture", &["slow"], None), &NeverCancelled)
            .unwrap();
        let survivor_alive = process_is_alive(survivor_pid.trim());
        // Release and join before asserting so a regression cannot leave a child
        // waiting on the fixture gate.
        fs::write(workspace.path().join("release"), b"").unwrap();
        let survivor = survivor.join().unwrap().unwrap();

        assert!(slow.timed_out);
        assert!(
            survivor_alive,
            "timeout cleanup killed the other invocation"
        );
        assert!(!survivor.timed_out);
        assert_eq!(survivor.status, TerminationStatus::Exited { code: 0 });
        for (result, name) in [(&slow, "slow"), (&survivor, "survivor")] {
            assert_eq!(
                result.stdout,
                ShellOutput::Text {
                    text: name.to_owned(),
                    byte_count: name.len() as u64,
                    truncated: false,
                }
            );
            let stderr = format!("err-{name}");
            assert_eq!(
                result.stderr,
                ShellOutput::Text {
                    byte_count: stderr.len() as u64,
                    text: stderr,
                    truncated: false,
                }
            );
        }
    }
}
