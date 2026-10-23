#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

mod lint_support;

use core_engine::adapters::config;
use lint_support::{
    SEVERITY_CODE_GENERIC_REGEX, TestWorkspace, block_on, configure_lint_timeout_and_host_budget,
    handler, handler_with_timeout, lint_extension_bin, start_server,
};
use rmcp::RoleClient;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

async fn call_lint_check(
    client: &rmcp::service::RunningService<RoleClient, ()>,
    args: Value,
) -> rmcp::model::CallToolResult {
    client
        .peer()
        .call_tool(
            CallToolRequestParams::new("tower_lint_check")
                .with_arguments(args.as_object().expect("object args").clone()),
        )
        .await
        .expect("tower_lint_check call must succeed")
}

fn first_text(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .first()
        .and_then(|content| match &content.raw {
            rmcp::model::RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .expect("tool result must include text content")
}

fn result_json(result: &rmcp::model::CallToolResult) -> Value {
    assert!(
        !result.is_error.unwrap_or(false),
        "lint tool must return success-result payloads for supported user-facing outcomes"
    );
    serde_json::from_str(first_text(result)).expect("tool text must be JSON")
}

#[test]
fn lint_check_returns_diagnostics_for_single_configured_file_through_mcp() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "lint.sh",
        "#!/bin/sh\nprintf '%s:3:5: warning: T100: avoid placeholder text\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("src/main.txt", "one\ntwo\nplaceholder\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.txt"])).await;
        let result = call_lint_check(&client, json!({ "path": "src/main.txt" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], true);
    assert_eq!(
        payload["diagnostics"],
        json!([
            {
                "path": "src/main.txt",
                "line": 2,
                "character": 4,
                "endLine": 2,
                "endCharacter": 4,
                "severity": "warning",
                "code": "T100",
                "message": "avoid placeholder text",
                "source": "fixture-lint"
            }
        ])
    );
}

#[test]
fn lint_check_serializes_information_diagnostic_as_info_through_mcp() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "lint-info.sh",
        "#!/bin/sh\nprintf '%s:1:1: info: I100: informational diagnostic\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("src/info.txt", "content\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/info.txt"])).await;
        let result = call_lint_check(&client, json!({ "path": "src/info.txt" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], true);
    assert_eq!(payload["diagnostics"][0]["severity"], "info");
}

#[test]
fn lint_check_without_path_returns_sorted_workspace_diagnostics_through_mcp() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "lint-all.sh",
        r#"#!/bin/sh
case "$1" in
  "zeta.txt") printf 'zeta.txt:6:1: warning: Z001: final file\n' ;;
  "alpha.txt") printf 'alpha.txt:2:8: error: A001: first file\n' ;;
esac
"#,
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("zeta.txt", "z");
    workspace.write_file("alpha.txt", "a");
    workspace.write_file("README.md", "# unsupported");

    let payload = block_on(async {
        let client =
            start_server(handler(&workspace, &["zeta.txt", "README.md", "alpha.txt"])).await;
        let result = call_lint_check(&client, json!({})).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], true);
    assert_eq!(
        payload["diagnostics"],
        json!([
            {
                "path": "alpha.txt",
                "line": 1,
                "character": 7,
                "endLine": 1,
                "endCharacter": 7,
                "severity": "error",
                "code": "A001",
                "message": "first file",
                "source": "fixture-lint"
            },
            {
                "path": "zeta.txt",
                "line": 5,
                "character": 0,
                "endLine": 5,
                "endCharacter": 0,
                "severity": "warning",
                "code": "Z001",
                "message": "final file",
                "source": "fixture-lint"
            }
        ])
    );
}

#[test]
fn lint_check_returns_structured_error_for_missing_linter_without_mcp_error() {
    let workspace = TestWorkspace::new();
    workspace.write_lint_config_with_regex(
        &workspace.root().join("missing-linter"),
        &["txt"],
        SEVERITY_CODE_GENERIC_REGEX,
    );
    workspace.write_file("src/main.txt", "plain text");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.txt"])).await;
        let result = call_lint_check(&client, json!({ "path": "src/main.txt" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], false);
    assert_eq!(payload["diagnostics"], json!([]));
    assert_eq!(payload["error"]["code"], "lint_missing_binary");
}

#[test]
fn tower_lint_check_honors_configured_command_timeout() {
    const RUNNER_TIMEOUT: Duration = Duration::from_secs(6);
    const END_TO_END_LIMIT: Duration = Duration::from_secs(8);
    const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(18);

    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "configured-timeout-check.sh",
        "#!/bin/sh\nsleep 3\nprintf '%s:2:3: warning: T300: completed after configured wait\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    let host_timeout = configure_lint_timeout_and_host_budget(&workspace, Some(6));
    workspace.write_file("src/main.txt", "one\ntwo\n");

    assert_eq!(host_timeout, Duration::from_secs(13));
    let (payload, elapsed) = block_on(async {
        let client = start_server(handler_with_timeout(
            &workspace,
            &["src/main.txt"],
            host_timeout,
        ))
        .await;
        let started = Instant::now();
        let result = tokio::time::timeout(
            MCP_REQUEST_TIMEOUT,
            call_lint_check(&client, json!({ "path": "src/main.txt" })),
        )
        .await
        .expect("lint MCP request must finish within its harness budget");
        let elapsed = started.elapsed();
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        (payload, elapsed)
    });

    eprintln!(
        "configured lint timing: elapsed_ms={}, runner={}s, host={}s, mcp={}s, end_to_end_allowance=2s",
        elapsed.as_millis(),
        RUNNER_TIMEOUT.as_secs(),
        host_timeout.as_secs(),
        MCP_REQUEST_TIMEOUT.as_secs(),
    );
    assert!(elapsed >= Duration::from_secs(3));
    assert!(elapsed < END_TO_END_LIMIT);
    assert!(elapsed < host_timeout);
    assert!(elapsed < MCP_REQUEST_TIMEOUT);
    assert_eq!(payload["supported"], true);
    assert_eq!(payload["diagnostics"][0]["code"], "T300");
    assert_eq!(
        payload["diagnostics"][0]["message"],
        "completed after configured wait"
    );
}

#[test]
fn tower_lint_check_preserves_omitted_timeout_behavior() {
    const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "omitted-timeout-check.sh",
        "#!/bin/sh\nprintf '%s:1:1: warning: D200: default timeout remains usable\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    let host_timeout = configure_lint_timeout_and_host_budget(&workspace, None);
    let config_contents =
        std::fs::read_to_string(workspace.root().join(".tower/config.toml")).expect("read config");
    let lint_section = config_contents
        .split("[extensions]")
        .next()
        .expect("lint section");
    assert!(!lint_section.contains("timeout_secs"));
    assert_eq!(host_timeout, Duration::from_secs(5));
    workspace.write_file("src/default.txt", "content\n");

    let payload = block_on(async {
        let client = start_server(handler_with_timeout(
            &workspace,
            &["src/default.txt"],
            host_timeout,
        ))
        .await;
        let result = tokio::time::timeout(
            MCP_REQUEST_TIMEOUT,
            call_lint_check(&client, json!({ "path": "src/default.txt" })),
        )
        .await
        .expect("default-timeout lint request must finish within its harness budget");
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], true);
    assert_eq!(payload["diagnostics"][0]["code"], "D200");
    assert_eq!(
        payload["diagnostics"][0]["message"],
        "default timeout remains usable"
    );
}

const GOLANGCI_PROBE: &str =
    "package lintprobe\n\nimport \"fmt\"\n\nfunc main() { fmt.Printf(\"%d\", \"not-a-number\") }\n";

fn prerequisite_path(variable: &str) -> PathBuf {
    let value =
        std::env::var_os(variable).unwrap_or_else(|| panic!("{variable} prerequisite is not set"));
    fs::canonicalize(&value)
        .unwrap_or_else(|error| panic!("{variable} prerequisite cannot be canonicalized: {error}"))
}

fn prerequisite_executable(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| panic!("required executable `{name}` was not found on PATH"));
    fs::canonicalize(&path)
        .unwrap_or_else(|error| panic!("cannot canonicalize `{name}` executable: {error}"))
}

fn output_or_prerequisite_failure(
    command: &mut Command,
    purpose: &str,
    timeout: Duration,
) -> String {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|_| panic!("{purpose} prerequisite could not run"));
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "{purpose} prerequisite exceeded its {}ms budget",
                    timeout.as_millis()
                );
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{purpose} prerequisite could not be polled");
            }
        }
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|_| panic!("{purpose} prerequisite could not be reaped"));
    assert!(
        output.status.success(),
        "{purpose} prerequisite failed (status {})",
        output.status
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|_| panic!("{purpose} prerequisite returned non-UTF-8 output"))
}

#[cfg(unix)]
#[test]
fn prerequisite_command_is_bounded_by_its_timeout() {
    let started = Instant::now();
    let panic = std::panic::catch_unwind(|| {
        output_or_prerequisite_failure(
            Command::new("sh").args(["-c", "sleep 2"]),
            "deliberately slow prerequisite",
            Duration::from_millis(100),
        );
    })
    .expect_err("slow prerequisite must time out");

    assert!(started.elapsed() < Duration::from_secs(1));
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("timeout panic message");
    assert!(message.contains("deliberately slow prerequisite"));
    assert!(message.contains("exceeded its 100ms budget"));
}

fn copy_checkout_entry(source_root: &Path, destination_root: &Path, relative: &Path) {
    if matches!(
        relative.file_name().and_then(|name| name.to_str()),
        Some(".git" | ".tower")
    ) {
        return;
    }
    let source = source_root.join(relative);
    let destination = destination_root.join(relative);
    let metadata = fs::symlink_metadata(&source).expect("read checkout entry metadata");

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&source).expect("read checkout symlink");
        let Ok(resolved) = source
            .parent()
            .expect("checkout entry parent")
            .join(&target)
            .canonicalize()
        else {
            return;
        };
        if !resolved.starts_with(source_root) {
            return;
        }
        #[cfg(unix)]
        {
            let remapped_target = if target.is_absolute() {
                destination_root.join(
                    resolved
                        .strip_prefix(source_root)
                        .expect("resolved symlink is within checkout"),
                )
            } else {
                target
            };
            std::os::unix::fs::symlink(remapped_target, destination)
                .expect("copy checkout symlink");
        }
        return;
    }

    if metadata.is_dir() {
        fs::create_dir_all(&destination).expect("create private checkout directory");
        for entry in fs::read_dir(&source).expect("read checkout directory") {
            let entry = entry.expect("read checkout directory entry");
            copy_checkout_entry(
                source_root,
                destination_root,
                &relative.join(entry.file_name()),
            );
        }
    } else if metadata.is_file() {
        fs::copy(source, destination).expect("copy checkout file");
    }
}

fn assert_checkout_copy_is_isolated(source_root: &Path, copied_root: &Path, relative: &Path) {
    assert!(
        !matches!(
            relative.file_name().and_then(|name| name.to_str()),
            Some(".git" | ".tower")
        ),
        "private checkout copy must not contain Git or Tower metadata"
    );
    let copied = copied_root.join(relative);
    let metadata = fs::symlink_metadata(&copied).expect("read copied checkout entry metadata");

    if metadata.file_type().is_symlink() {
        let resolved = copied
            .canonicalize()
            .expect("copied checkout symlink must resolve");
        assert!(
            resolved.starts_with(copied_root),
            "copied checkout symlink must remain inside the private copy"
        );
        assert!(
            !resolved.starts_with(source_root),
            "copied checkout symlink must not traverse into the source checkout"
        );
        return;
    }

    if metadata.is_dir() {
        for entry in fs::read_dir(copied).expect("read copied checkout directory") {
            let entry = entry.expect("read copied checkout directory entry");
            assert_checkout_copy_is_isolated(
                source_root,
                copied_root,
                &relative.join(entry.file_name()),
            );
        }
    }
}

#[test]
fn checkout_copy_excludes_nested_vcs_and_tower_metadata() {
    let source = TestWorkspace::new();
    let destination = TestWorkspace::new();
    fs::create_dir_all(source.root().join("nested/.git")).expect("create nested Git metadata");
    fs::create_dir_all(source.root().join("nested/.tower")).expect("create nested Tower metadata");
    fs::write(source.root().join("nested/.git/config"), "metadata")
        .expect("write nested Git metadata");
    fs::write(source.root().join("nested/.tower/state"), "metadata")
        .expect("write nested Tower metadata");
    fs::write(source.root().join("nested/kept.txt"), "kept").expect("write retained file");

    copy_checkout_entry(source.root(), destination.root(), Path::new(""));

    assert!(destination.root().join("nested/kept.txt").is_file());
    assert!(!destination.root().join("nested/.git").exists());
    assert!(!destination.root().join("nested/.tower").exists());
    assert_checkout_copy_is_isolated(source.root(), destination.root(), Path::new(""));
}

fn sha256(path: &Path) -> String {
    sha256_bytes(&fs::read(path).expect("read provenance artifact"))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_supported_golangci_major(version: &str) -> u64 {
    let token = version
        .split_whitespace()
        .skip_while(|token| *token != "version")
        .nth(1)
        .unwrap_or_else(|| {
            panic!(
                "cannot parse golangci-lint version from `{}`",
                version.trim()
            )
        });
    let numeric = token.strip_prefix('v').unwrap_or(token);
    let major = numeric
        .split('.')
        .next()
        .expect("version major")
        .parse::<u64>()
        .expect("numeric golangci-lint major version");
    assert_eq!(
        major, 2,
        "unsupported golangci-lint major {major}; this operational gate supports major 2"
    );
    major
}

struct OperationalGolangci {
    workspace: TestWorkspace,
    golangci_lint: PathBuf,
    golangci_major: u64,
    host_timeout: Duration,
    client_timeout: Duration,
    config_sha256: String,
}

impl OperationalGolangci {
    const INTERNAL_TIMEOUT_SECS: u64 = 180;

    fn prepare() -> Self {
        let internal_timeout_secs = Self::INTERNAL_TIMEOUT_SECS;
        const CLIENT_TIMEOUT_ALLOWANCE: Duration = Duration::from_secs(30);
        const CONFIG: &str = r#"version: "2"
linters:
  default: none
  enable:
    - govet
  settings:
    govet:
      enable:
        - printf
  exclusions:
    generated: disable
    presets: []
    rules: []
    paths: []
issues:
  max-issues-per-linter: 0
  max-same-issues: 0
output:
  formats:
    text:
      path: stdout
      print-linter-name: true
      print-issued-lines: false
      colors: false
"#;

        let source_root = prerequisite_path("TOWER_AWF_CLI_ROOT");
        assert!(
            source_root.is_dir(),
            "TOWER_AWF_CLI_ROOT is not a directory"
        );
        assert!(
            !source_root.join("lint_probe").exists(),
            "TOWER_AWF_CLI_ROOT already contains lint_probe; refusing to overwrite it"
        );
        let go_mod = fs::read_to_string(source_root.join("go.mod"))
            .expect("TOWER_AWF_CLI_ROOT must contain a readable go.mod");
        assert!(
            go_mod
                .lines()
                .any(|line| line == "module github.com/awf-project/cli"),
            "TOWER_AWF_CLI_ROOT go.mod must declare module github.com/awf-project/cli"
        );
        assert!(
            source_root.join(".golangci.yml").is_file(),
            "TOWER_AWF_CLI_ROOT must contain .golangci.yml"
        );

        let golangci_lint = prerequisite_executable("golangci-lint");
        let go = prerequisite_executable("go");
        let env = prerequisite_executable("env");
        let version = output_or_prerequisite_failure(
            Command::new(&golangci_lint).arg("version"),
            "golangci-lint version",
            Duration::from_secs(10),
        );
        let golangci_major = parse_supported_golangci_major(&version);

        let workspace = TestWorkspace::new();
        copy_checkout_entry(&source_root, workspace.root(), Path::new(""));
        assert_checkout_copy_is_isolated(&source_root, workspace.root(), Path::new(""));
        output_or_prerequisite_failure(
            Command::new(&go)
                .args(["list", "-buildvcs=false", "-deps", "./..."])
                .current_dir(workspace.root()),
            "Go module dependencies",
            Duration::from_secs(120),
        );
        let probe_path = workspace.root().join("lint_probe/main.go");
        fs::create_dir_all(probe_path.parent().expect("probe parent")).expect("create lint probe");
        fs::write(&probe_path, GOLANGCI_PROBE).expect("write lint probe");
        assert_eq!(
            fs::read_to_string(&probe_path).expect("read lint probe"),
            GOLANGCI_PROBE
        );

        let test_config = workspace.root().join("operational-golangci.yml");
        fs::write(&test_config, CONFIG).expect("write operational golangci-lint config");
        output_or_prerequisite_failure(
            Command::new(&golangci_lint)
                .args(["config", "verify", "--config"])
                .arg(&test_config)
                .current_dir(workspace.root()),
            "golangci-lint v2 operational configuration",
            Duration::from_secs(10),
        );

        let tower_dir = workspace.root().join(".tower");
        fs::create_dir_all(&tower_dir).expect("create private Tower config directory");
        let escaped_env = env.to_string_lossy().replace('\\', "\\\\");
        let escaped_command = golangci_lint.to_string_lossy().replace('\\', "\\\\");
        let escaped_config = test_config.to_string_lossy().replace('\\', "\\\\");
        fs::write(
        tower_dir.join("config.toml"),
        format!(
            r#"[lint.golangci]
command = "{escaped_env}"
args = ["GOFLAGS=-buildvcs=false", "{escaped_command}", "run", "--config", "{escaped_config}", "./..."]
extensions = ["go"]
format = "generic-regex"
target = "none"
regex = '^(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)$'
source = "golangci-lint"
timeout_secs = {internal_timeout_secs}
"#,
        ),
    )
    .expect("write private Tower lint config");
        let host_timeout = config::load(workspace.root())
            .expect("load operational lint config")
            .lint_request_timeout()
            .expect("derive operational host deadline");
        let client_timeout = host_timeout + CLIENT_TIMEOUT_ALLOWANCE;
        assert!(Duration::from_secs(Self::INTERNAL_TIMEOUT_SECS) < host_timeout);
        assert!(host_timeout < client_timeout);

        Self {
            workspace,
            golangci_lint,
            golangci_major,
            host_timeout,
            client_timeout,
            config_sha256: sha256_bytes(CONFIG.as_bytes()),
        }
    }

    fn print_evidence(&self, payload: &Value, elapsed: Duration) {
        assert_eq!(
            payload["supported"], true,
            "operational lint must produce a supported result"
        );
        assert!(
            payload["error"].is_null(),
            "operational lint must not report an in-band error"
        );
        let diagnostic = payload["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .find(|diagnostic| diagnostic["path"] == "lint_probe/main.go")
            .expect("govet diagnostic for lint_probe/main.go");
        let message = diagnostic["message"].as_str().expect("diagnostic message");
        let source = diagnostic["source"].as_str().expect("diagnostic source");
        assert!(!message.is_empty());
        assert!(!source.is_empty());
        assert!(
            message.to_ascii_lowercase().contains("govet") || source == "govet",
            "probe diagnostic must identify govet"
        );

        let lint_extension = fs::canonicalize(lint_extension_bin())
            .expect("target/debug/lint_extension must be built before this gate");
        let test_executable = std::env::current_exe()
            .expect("test executable path")
            .canonicalize()
            .expect("canonical test executable path");
        println!(
            "{}",
            json!({
                "event": "golangci_lint_operational_evidence",
                "elapsed_ms": elapsed.as_millis(),
                "budgets_secs": {
                    "internal": Self::INTERNAL_TIMEOUT_SECS,
                    "host": self.host_timeout.as_secs(),
                    "client": self.client_timeout.as_secs(),
                },
                "golangci_major": self.golangci_major,
                "config_sha256": self.config_sha256,
                "artifacts": [
                    { "path": self.golangci_lint, "sha256": sha256(&self.golangci_lint) },
                    { "path": lint_extension, "sha256": sha256(&lint_extension) },
                    { "path": test_executable, "sha256": sha256(&test_executable) },
                ],
                "diagnostic": {
                    "path": "lint_probe/main.go",
                    "source": source,
                    "message_non_empty": true,
                }
            })
        );
    }
}

#[test]
#[ignore]
fn tower_lint_check_parses_real_golangci_diagnostics_from_awf_cli() {
    let fixture = OperationalGolangci::prepare();

    let started = Instant::now();
    let payload = block_on(async {
        let client = start_server(handler_with_timeout(
            &fixture.workspace,
            &["lint_probe/main.go"],
            fixture.host_timeout,
        ))
        .await;
        // target=none already runs ./...; select one file to avoid rerunning the
        // whole-project command once for every Go file returned by listFiles.
        let result = tokio::time::timeout(
            fixture.client_timeout,
            call_lint_check(&client, json!({ "path": "lint_probe/main.go" })),
        )
        .await
        .expect("real golangci-lint request exceeded the harness client budget");
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });
    let elapsed = started.elapsed();

    fixture.print_evidence(&payload, elapsed);
}
