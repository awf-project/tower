use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::adapters::config::ShellConfig;
use crate::adapters::mcp::{ToolDesc, ToolError, ToolRegistry};
use crate::adapters::process::ProcessError;
use crate::adapters::shell::{
    ShellCommandRequest, ShellCommandResult, ShellCommandRunner, ShellRunnerError,
};
use crate::domain::CancellationSignal;

const TOOL_NAME: &str = "tower_run_command";

pub struct ShellCommandRegistry {
    runner: Box<dyn ShellRunnerBackend + Send>,
}

trait ShellRunnerBackend {
    fn is_enabled(&self) -> bool;
    fn run(
        &self,
        request: ShellCommandRequest,
        cancellation: &dyn CancellationSignal,
    ) -> Result<ShellCommandResult, ShellRunnerError>;
}

impl ShellRunnerBackend for ShellCommandRunner {
    fn is_enabled(&self) -> bool {
        self.is_enabled()
    }

    fn run(
        &self,
        request: ShellCommandRequest,
        cancellation: &dyn CancellationSignal,
    ) -> Result<ShellCommandResult, ShellRunnerError> {
        self.run(request, cancellation)
    }
}

impl ShellCommandRegistry {
    pub fn new(policy: Arc<ShellConfig>, workspace_root: PathBuf) -> Self {
        Self {
            runner: Box::new(ShellCommandRunner::new(policy, workspace_root)),
        }
    }

    #[cfg(test)]
    fn with_runner(runner: impl ShellRunnerBackend + Send + 'static) -> Self {
        Self {
            runner: Box::new(runner),
        }
    }
}

impl ToolRegistry for ShellCommandRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        if !self.runner.is_enabled() {
            return Vec::new();
        }

        vec![ToolDesc {
            name: TOOL_NAME.to_owned(),
            description: "Run an explicitly allowed command within the workspace".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "default": []
                    },
                    "cwd": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }]
    }

    fn call(
        &mut self,
        name: &str,
        args: Value,
        cancellation: &dyn CancellationSignal,
    ) -> Result<Value, ToolError> {
        if name != TOOL_NAME || !self.runner.is_enabled() {
            return Err(ToolError::NotFound(name.to_owned()));
        }

        let request: ShellCommandRequest = serde_json::from_value(args).map_err(|_| {
            ToolError::InvalidArgs("invalid tower_run_command arguments".to_owned())
        })?;
        let result = self
            .runner
            .run(request, cancellation)
            .map_err(map_runner_error)?;
        serialize_result(result)
    }
}

fn serialize_result(result: ShellCommandResult) -> Result<Value, ToolError> {
    serde_json::to_value(result)
        .map_err(|_| ToolError::ExecutionFailed("command execution failed".to_owned()))
}

fn map_runner_error(error: ShellRunnerError) -> ToolError {
    match error {
        ShellRunnerError::InvalidCommand | ShellRunnerError::CommandNotAllowed => {
            ToolError::InvalidArgs("command is not allowed".to_owned())
        }
        ShellRunnerError::InvalidWorkingDirectory => ToolError::InvalidArgs(
            "working directory must be an existing directory within the workspace".to_owned(),
        ),
        ShellRunnerError::Process(ProcessError::SpawnFailed) => {
            ToolError::ExecutionFailed("command process could not be started".to_owned())
        }
        ShellRunnerError::Process(_) => {
            ToolError::ExecutionFailed("command execution failed".to_owned())
        }
        ShellRunnerError::Cancelled => ToolError::Cancelled,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use serde_json::json;
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::adapters::config::AllowedCommand;
    use crate::adapters::mcp::native_tools::{EXPECTED_NATIVE_TOOL_NAMES, NativeToolRegistry};
    use crate::adapters::process::test_support::{
        assert_recorded_process_is_dead, executable, wait_for_file,
    };
    use crate::adapters::process::{OutputStream, process_fixture_lock};
    use crate::domain::NeverCancelled;

    #[derive(Default)]
    struct ToggleCancellation(AtomicBool);

    impl ToggleCancellation {
        fn cancel(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    impl CancellationSignal for ToggleCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    struct FailingRunner(ShellRunnerError);

    impl ShellRunnerBackend for FailingRunner {
        fn is_enabled(&self) -> bool {
            true
        }

        fn run(
            &self,
            _request: ShellCommandRequest,
            _cancellation: &dyn CancellationSignal,
        ) -> Result<ShellCommandResult, ShellRunnerError> {
            Err(self.0)
        }
    }

    fn enabled_policy(name: &str, argv: Vec<String>, timeout_secs: u64) -> Arc<ShellConfig> {
        Arc::new(ShellConfig {
            enabled: true,
            commands: vec![AllowedCommand {
                name: name.to_owned(),
                argv,
            }],
            timeout_secs,
            stdout_max_bytes: 256,
            stderr_max_bytes: 256,
        })
    }

    fn disabled_policy() -> Arc<ShellConfig> {
        Arc::new(ShellConfig {
            enabled: false,
            commands: Vec::new(),
            timeout_secs: 1,
            stdout_max_bytes: 256,
            stderr_max_bytes: 256,
        })
    }

    fn registry_with_script(body: &str, timeout_secs: u64) -> (TempDir, ShellCommandRegistry) {
        let workspace = tempdir().unwrap();
        let script = workspace.path().join("fixture");
        executable(&script, body);
        let registry = ShellCommandRegistry::new(
            enabled_policy(
                "fixture",
                vec![script.to_string_lossy().into_owned()],
                timeout_secs,
            ),
            workspace.path().to_path_buf(),
        );
        (workspace, registry)
    }

    fn assert_invalid_args(result: Result<Value, ToolError>, expected: &str) {
        assert!(matches!(
            result,
            Err(ToolError::InvalidArgs(message)) if message == expected
        ));
    }

    #[test]
    fn shell_command_registry_describes_strict_schema() {
        let _fixture_lock = process_fixture_lock();
        let (workspace, mut registry) =
            registry_with_script("#!/bin/sh\nprintf 'ok:%s' \"$1\"\n", 2);

        let descriptors = registry.list();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].name, TOOL_NAME);
        assert_eq!(
            descriptors[0].input_schema,
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "default": []
                    },
                    "cwd": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            })
        );
        let descriptor_json = serde_json::to_string(&descriptors[0]).unwrap();
        assert!(!descriptor_json.contains(workspace.path().to_string_lossy().as_ref()));

        let result = registry
            .call(
                TOOL_NAME,
                json!({"command": "fixture", "args": ["value"]}),
                &NeverCancelled,
            )
            .unwrap();
        assert_eq!(result["status"], json!({"kind": "exited", "code": 0}));
        assert_eq!(result["stdout"]["text"], "ok:value");
        assert_eq!(result["timed_out"], false);
    }

    #[test]
    fn shell_command_registry_returns_structured_nonzero_and_signalled_outcomes() {
        let _fixture_lock = process_fixture_lock();
        let (_nonzero_workspace, mut nonzero_registry) =
            registry_with_script("#!/bin/sh\nprintf nonzero-output; exit 17\n", 2);

        let nonzero_result = nonzero_registry
            .call(TOOL_NAME, json!({"command": "fixture"}), &NeverCancelled)
            .expect("normal nonzero exits are structured command results");
        assert_eq!(
            nonzero_result["status"],
            json!({"kind": "exited", "code": 17})
        );
        assert_eq!(nonzero_result["stdout"]["text"], "nonzero-output");
        assert_eq!(nonzero_result["timed_out"], false);

        let (_signal_workspace, mut signal_registry) =
            registry_with_script("#!/bin/sh\nkill -TERM $$\n", 2);

        let signal_result = signal_registry
            .call(TOOL_NAME, json!({"command": "fixture"}), &NeverCancelled)
            .expect("independent signal termination is a structured command result");
        assert_eq!(signal_result["status"], json!({"kind": "signalled"}));
        assert_eq!(signal_result["timed_out"], false);
    }

    #[test]
    fn disabled_shell_command_registry_lists_and_dispatches_nothing() {
        let workspace = tempdir().unwrap();
        let mut registry =
            ShellCommandRegistry::new(disabled_policy(), workspace.path().to_path_buf());

        assert!(registry.list().is_empty());
        assert!(matches!(
            registry.call(
                TOOL_NAME,
                json!({"command": "anything"}),
                &NeverCancelled
            ),
            Err(ToolError::NotFound(name)) if name == TOOL_NAME
        ));
        assert!(!EXPECTED_NATIVE_TOOL_NAMES.contains(&TOOL_NAME));
        assert_eq!(NativeToolRegistry::external_access(TOOL_NAME), None);
    }

    #[test]
    fn shell_runner_failures_map_to_stable_public_errors() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let missing = workspace.path().join("private-missing-executable");
        let mut registry = ShellCommandRegistry::new(
            enabled_policy("fixture", vec![missing.to_string_lossy().into_owned()], 2),
            workspace.path().to_path_buf(),
        );

        assert_invalid_args(
            registry.call(TOOL_NAME, json!({"command": "bad/name"}), &NeverCancelled),
            "command is not allowed",
        );
        assert_invalid_args(
            registry.call(TOOL_NAME, json!({"command": "other"}), &NeverCancelled),
            "command is not allowed",
        );
        assert_invalid_args(
            registry.call(
                TOOL_NAME,
                json!({"command": "fixture", "cwd": "missing-directory"}),
                &NeverCancelled,
            ),
            "working directory must be an existing directory within the workspace",
        );
        assert!(matches!(
            registry.call(TOOL_NAME, json!({"command": "fixture"}), &NeverCancelled),
            Err(ToolError::ExecutionFailed(message))
                if message == "command process could not be started"
        ));
        for malformed in [
            Value::Null,
            json!({}),
            json!({"command": 7}),
            json!({"command": "fixture", "args": [7]}),
            json!({"command": "fixture", "unexpected": true}),
        ] {
            assert_invalid_args(
                registry.call(TOOL_NAME, malformed, &NeverCancelled),
                "invalid tower_run_command arguments",
            );
        }
        let rendered = format!(
            "{:?}",
            registry.call(TOOL_NAME, json!({"command": "fixture"}), &NeverCancelled)
        );
        assert!(!rendered.contains(missing.to_string_lossy().as_ref()));

        for process_error in [
            ProcessError::StdinWriteFailed,
            ProcessError::PipeReadFailed {
                stream: OutputStream::Stdout,
            },
            ProcessError::PipeReadFailed {
                stream: OutputStream::Stderr,
            },
            ProcessError::WaitFailed,
            ProcessError::CleanupFailed,
        ] {
            let mut registry = ShellCommandRegistry::with_runner(FailingRunner(
                ShellRunnerError::Process(process_error),
            ));
            assert!(matches!(
                registry.call(TOOL_NAME, json!({"command": "private-command"}), &NeverCancelled),
                Err(ToolError::ExecutionFailed(message))
                    if message == "command execution failed"
            ));
        }

        let mut cancelled =
            ShellCommandRegistry::with_runner(FailingRunner(ShellRunnerError::Cancelled));
        assert!(matches!(
            cancelled.call(
                TOOL_NAME,
                json!({"command": "private-command"}),
                &NeverCancelled
            ),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn shell_runner_live_failure_keeps_private_values_out_of_error_and_logs() {
        const HELPER_ENV: &str = "TOWER_SHELL_PRIVACY_FAILURE_HELPER";
        const EXECUTABLE_SENTINEL: &str = "PRIVATE_EXECUTABLE_PATH_SENTINEL";
        const ARGV_SENTINEL: &str = "PRIVATE_ARGV_SENTINEL";
        const OUTPUT_SENTINEL: &str = "PRIVATE_OUTPUT_SENTINEL";
        const OS_ERROR_SENTINEL: &str = "PRIVATE_OS_ERROR_SENTINEL";

        if std::env::var_os(HELPER_ENV).is_some() {
            let workspace = tempdir().unwrap();
            let missing = workspace
                .path()
                .join(format!("{EXECUTABLE_SENTINEL}-{OS_ERROR_SENTINEL}"));
            let mut registry = ShellCommandRegistry::new(
                enabled_policy(
                    "private-command",
                    vec![
                        missing.to_string_lossy().into_owned(),
                        ARGV_SENTINEL.to_owned(),
                        OUTPUT_SENTINEL.to_owned(),
                    ],
                    2,
                ),
                workspace.path().to_path_buf(),
            );
            let error = registry
                .call(
                    TOOL_NAME,
                    json!({"command": "private-command"}),
                    &NeverCancelled,
                )
                .expect_err("missing private executable must fail at the real runner boundary");
            assert!(matches!(
                error,
                ToolError::ExecutionFailed(ref message)
                    if message == "command process could not be started"
            ));
            eprintln!("public-error={error:?}");
            return;
        }

        let output = Command::new(std::env::current_exe().expect("resolve current test binary"))
            .arg("--exact")
            .arg("adapters::mcp::shell_command::tests::shell_runner_live_failure_keeps_private_values_out_of_error_and_logs")
            .arg("--nocapture")
            .env(HELPER_ENV, "1")
            .output()
            .expect("run privacy helper test process");
        assert!(
            output.status.success(),
            "privacy helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let captured = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(captured.contains("command process could not be started"));
        for sentinel in [
            EXECUTABLE_SENTINEL,
            ARGV_SENTINEL,
            OUTPUT_SENTINEL,
            OS_ERROR_SENTINEL,
        ] {
            assert!(
                !captured.contains(sentinel),
                "captured live runner logs leaked {sentinel}: {captured}"
            );
        }
    }

    #[test]
    fn cancelled_shell_call_terminates_and_reaps_process_tree() {
        let _fixture_lock = process_fixture_lock();
        let workspace = tempdir().unwrap();
        let leader_pid = workspace.path().join("leader.pid");
        let descendant_pid = workspace.path().join("descendant.pid");
        let script = workspace.path().join("fixture");
        executable(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$1\"\nsh -c 'while :; do sleep 1; done' &\nprintf '%s\\n' \"$!\" > \"$2\"\nwait\n",
        );
        let registry = ShellCommandRegistry::new(
            enabled_policy(
                "fixture",
                vec![
                    script.to_string_lossy().into_owned(),
                    leader_pid.to_string_lossy().into_owned(),
                    descendant_pid.to_string_lossy().into_owned(),
                ],
                30,
            ),
            workspace.path().to_path_buf(),
        );
        let cancellation = Arc::new(ToggleCancellation::default());
        let call_cancellation = Arc::clone(&cancellation);
        let call = thread::spawn(move || {
            let mut registry = registry;
            registry.call(
                TOOL_NAME,
                json!({"command": "fixture"}),
                call_cancellation.as_ref(),
            )
        });

        wait_for_file(&leader_pid);
        wait_for_file(&descendant_pid);
        cancellation.cancel();
        assert!(matches!(call.join().unwrap(), Err(ToolError::Cancelled)));
        assert_recorded_process_is_dead(&leader_pid);
        assert_recorded_process_is_dead(&descendant_pid);
    }

    #[test]
    fn request_cancellation_is_distinct_from_a_timeout_result() {
        let _fixture_lock = process_fixture_lock();
        let (_workspace, mut registry) =
            registry_with_script("#!/bin/sh\nwhile :; do sleep 1; done\n", 1);

        let result = registry
            .call(TOOL_NAME, json!({"command": "fixture"}), &NeverCancelled)
            .unwrap();

        assert_eq!(result["timed_out"], true);
        assert_eq!(result["status"], json!({"kind": "signalled"}));
    }
}
