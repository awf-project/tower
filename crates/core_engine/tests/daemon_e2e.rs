//! Daemon end-to-end tests (Tasks 7–8).
// Feature: F009
#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::{DaemonConfig, TowerConfig};
use core_engine::adapters::daemon::server::run_daemon;
use core_engine::adapters::daemon::socket::{socket_path, wait_for_socket};
use core_engine::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, read_line_capped, write_line,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[path = "support/process_binary.rs"]
mod process_binary;

#[path = "support/reconciliation_process.rs"]
mod reconciliation_process;

fn config_with_idle(secs: u64) -> TowerConfig {
    TowerConfig {
        daemon: DaemonConfig {
            idle_timeout_secs: secs,
        },
        ..Default::default()
    }
}

/// Spawn `run_daemon` on a blocking thread against a tempdir workspace.
fn spawn_daemon_thread(root: std::path::PathBuf, cfg: TowerConfig) {
    std::thread::spawn(move || {
        let opts = GlobalOpts {
            workspace_dir: Some(root),
            extensions_dir: None,
        };
        let _ = run_daemon(&opts, cfg, false);
    });
}

fn selected_production_binary() -> PathBuf {
    process_binary::selected_binary("TOWER_PRODUCTION_BIN")
}

fn run_production(binary: &Path, workspace: &Path, command: &str) -> Output {
    Command::new(binary)
        .arg(command)
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .output()
        .expect("run production tower binary")
}

fn selected_binary(selector: &str) -> PathBuf {
    process_binary::selected_binary(selector)
}

fn selected_existing_directory(selector: &str) -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os(selector).unwrap_or_else(|| panic!("{selector} must be set")),
    );
    assert!(path.is_dir(), "{selector} must name an existing directory");
    path.canonicalize().unwrap()
}

fn executable_sha256(path: &Path) -> String {
    Sha256::digest(fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn assert_no_deprecated_startup_artifacts(workspace: &Path) {
    let tower_dir = workspace.join(".tower");
    for relative in ["plugins", "plugin_fmt", "plugin_fmt.wasm", "formatter.toml"] {
        assert!(
            !tower_dir.join(relative).exists(),
            "daemon created deprecated artifact .tower/{relative}"
        );
    }

    let db = sled::open(tower_dir.join("db")).expect("open daemon Sled database");
    let meta = db.open_tree("meta").expect("open persisted metadata tree");
    assert!(
        meta.get(b"scan_complete").unwrap().is_none(),
        "daemon created deprecated scan-completion state"
    );
}

fn write_launch_sentinel(extension_dir: &Path, name: &str, marker: &Path) {
    let extension = extension_dir.join(name);
    std::fs::create_dir_all(&extension).unwrap();
    let script = extension.join("launch.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf launched >> '{}'\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        extension.join("extension.toml"),
        format!(
            "name = \"{name}\"\nversion = \"0.1.0\"\ncommand = [\"{}\"]\nactivation = \"eager\"\n[capabilities]\nrequired = []\n",
            script.display()
        ),
    )
    .unwrap();
}

fn assert_legacy_config_rejected_before_spawn(
    binary: &Path,
    workspace: &Path,
    config: &str,
    secrets: &[&str],
    launch_markers: &[&Path],
) {
    let config_path = workspace.join(".tower/config.toml");
    std::fs::write(&config_path, config).unwrap();
    let before = std::fs::read(&config_path).unwrap();

    let output = run_production(binary, workspace, "daemon");

    assert!(
        !output.status.success(),
        "legacy configuration was accepted"
    );
    assert!(output.stdout.is_empty(), "rejection leaked onto stdout");
    assert_eq!(std::fs::read(&config_path).unwrap(), before);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(".tower/config.toml"), "stderr: {stderr}");
    assert!(stderr.contains("[plugins]"), "stderr: {stderr}");
    assert!(stderr.contains("[extensions] disabled"), "stderr: {stderr}");
    assert!(
        stderr.contains("[formatter.tools.<id>]"),
        "stderr: {stderr}"
    );
    for secret in secrets {
        assert!(!stderr.contains(secret), "diagnostic disclosed {secret}");
    }
    for marker in launch_markers {
        assert!(!marker.exists(), "sidecar spawned before config rejection");
    }
}

fn write_formatter_fixture(path: &Path, log: &Path, inplace: bool) {
    let body = if inplace {
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$(basename \"$0\")\" \"$*\" \"$PWD\" >> '{}'\nprintf '%s-formatted\\n' \"$(basename \"$0\")\" > \"$2\"\n",
            log.display()
        )
    } else {
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$(basename \"$0\")\" \"$*\" \"$PWD\" >> '{}'\ncat >/dev/null\nprintf '%s-formatted\\n' \"$(basename \"$0\")\"\n",
            log.display()
        )
    };
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(predicate(), "condition not met within {timeout:?}");
}

fn indexed_paths_through_mcp(workspace: &Path) -> Result<Vec<String>, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create MCP client runtime: {error}"))?
        .block_on(async {
            let mut connection = UnixStream::connect(socket_path(workspace))
                .await
                .map_err(|error| format!("connect MCP client: {error}"))?;
            connection
                .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
                .await
                .map_err(|error| format!("write MCP handshake: {error}"))?;

            write_line(
                &mut connection,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"daemon-e2e","version":"1"}}}"#,
            )
            .await
            .map_err(|error| format!("initialize MCP client: {error}"))?;
            let initialized = read_line_capped(&mut connection, 1 << 20)
                .await
                .map_err(|error| format!("read MCP initialization: {error}"))?
                .ok_or_else(|| "daemon closed MCP connection during initialization".to_owned())?;
            let initialized: Value = serde_json::from_str(&initialized)
                .map_err(|error| format!("parse MCP initialization: {error}"))?;
            if initialized.get("result").is_none() {
                return Err(format!("MCP initialization failed: {initialized}"));
            }
            write_line(
                &mut connection,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            )
            .await
            .map_err(|error| format!("send MCP initialized notification: {error}"))?;

            write_line(
                &mut connection,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "tower_list_dir",
                        "arguments": {"path": "", "recursive": true},
                    },
                })
                .to_string(),
            )
            .await
            .map_err(|error| format!("call tower_list_dir: {error}"))?;
            let response = read_line_capped(&mut connection, 1 << 20)
                .await
                .map_err(|error| format!("read tower_list_dir response: {error}"))?
                .ok_or_else(|| "daemon closed MCP connection during tower_list_dir".to_owned())?;
            let response: Value = serde_json::from_str(&response)
                .map_err(|error| format!("parse tower_list_dir response: {error}"))?;
            let text = response["result"]["content"]
                .as_array()
                .and_then(|content| content.first())
                .and_then(|content| content["text"].as_str())
                .ok_or_else(|| format!("tower_list_dir did not return text content: {response}"))?;
            let payload: Value = serde_json::from_str(text)
                .map_err(|error| format!("parse tower_list_dir payload: {error}"))?;

            payload["entries"]
                .as_array()
                .ok_or_else(|| format!("tower_list_dir did not return entries: {payload}"))?
                .iter()
                .filter(|entry| entry["kind"] == "file")
                .map(|entry| {
                    entry["path"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("file entry lacks a path: {entry}"))
                })
                .collect()
        })
}

fn start_and_observe_inventory(binary: &Path, workspace: &Path, expected_count: usize) -> u128 {
    let empty_extensions = workspace.join("empty-extensions");
    fs::create_dir_all(&empty_extensions).unwrap();
    let started = Instant::now();
    let mut daemon = Command::new(binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .env("XDG_CONFIG_HOME", workspace.join("xdg-config"))
        .env("XDG_DATA_HOME", workspace.join("xdg-data"))
        .env("TOWER_EXTENSIONS_DIR", &empty_extensions)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start timed tower daemon");

    let mut observed = Err("daemon inventory was not queried".to_owned());
    wait_until(Duration::from_secs(30), || {
        if daemon.try_wait().unwrap().is_some() {
            return true;
        }
        if !socket_path(workspace).exists() {
            return false;
        }
        observed = indexed_paths_through_mcp(workspace);
        observed
            .as_ref()
            .is_ok_and(|paths| paths.len() == expected_count)
    });
    let elapsed = started.elapsed().as_micros();

    if !observed
        .as_ref()
        .is_ok_and(|paths| paths.len() == expected_count)
    {
        let output = daemon.wait_with_output().unwrap();
        panic!("daemon did not expose {expected_count} files: {observed:?}; output={output:?}");
    }

    let shutdown = run_production(binary, workspace, "shutdown");
    assert!(shutdown.status.success(), "shutdown failed: {shutdown:?}");
    let output = daemon.wait_with_output().unwrap();
    assert!(output.status.success(), "daemon failed: {output:?}");
    assert!(output.stdout.is_empty(), "daemon wrote protocol stdout");
    elapsed
}

fn write_representative_workspace(workspace: &Path) {
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join(".tower")).unwrap();
    fs::write(
        workspace.join(".tower/config.toml"),
        "[daemon]\nidle_timeout_secs = 1\n",
    )
    .unwrap();
    fs::write(workspace.join(".towerignore"), "ignored/**\n").unwrap();
    let content = "x".repeat(4096);
    for index in 0..1_000 {
        fs::write(workspace.join(format!("src/file_{index:04}.rs")), &content).unwrap();
    }
}

fn percentile_summary(samples: &[u128]) -> (u128, u128, u128) {
    assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let p95 = sorted[(sorted.len() * 95).div_ceil(100) - 1];
    (sorted[0], median, p95)
}

fn run_unchanged_metrics(binary: &Path, workspace: &Path) -> Value {
    let selector = workspace.canonicalize().unwrap();
    let output = Command::new(binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .env("XDG_CONFIG_HOME", workspace.join("xdg-config"))
        .env("XDG_DATA_HOME", workspace.join("xdg-data"))
        .env("TOWER_EXTENSIONS_DIR", workspace.join("empty-extensions"))
        .env("TOWER_TEST_WORKSPACE", selector)
        .env("TOWER_TEST_METRICS", "1")
        .output()
        .expect("run hooked tower daemon");
    assert!(output.status.success(), "hooked daemon failed: {output:?}");
    assert!(output.stdout.is_empty(), "telemetry leaked onto stdout");

    let prefix = "tower: reconciliation-metrics ";
    let lines = String::from_utf8(output.stderr).unwrap();
    let metrics = lines
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .collect::<Vec<_>>();
    assert_eq!(metrics.len(), 1, "missing or duplicate metrics: {lines}");
    serde_json::from_str(metrics[0]).expect("parse reconciliation metrics")
}

#[test]
fn reconciliation_failure_is_scoped_and_process_visible() {
    reconciliation_process::assert_reconciliation_failure_contract_from_env();
}

#[test]
fn canonical_configuration_drives_formatter_and_rejects_legacy_before_spawn() {
    let binary = selected_production_binary();
    assert_legacy_formatter_scenarios_rejected(&binary);
    assert_canonical_formatter_execution(&binary);
}

fn formatter_workspace_with_launch_sentinels() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let fixture = tempdir().unwrap();
    let tower_dir = fixture.path().join(".tower");
    let local_marker = fixture.path().join("local-sidecar-launched");
    let legacy_marker = fixture.path().join("legacy-sidecar-launched");
    write_launch_sentinel(&tower_dir.join("extensions"), "sentinel", &local_marker);
    write_launch_sentinel(&tower_dir.join("plugins"), "legacy", &legacy_marker);
    (fixture, local_marker, legacy_marker)
}

fn assert_legacy_formatter_scenarios_rejected(binary: &Path) {
    let secret = "legacy-command-secret-91dd";
    let legacy = format!(
        "[plugins]\ndisabled = [\"{secret}\"]\n\
         [plugins.formatter.tools.secret]\nargv = [\"{secret}\"]\nextensions = [\"rs\"]\n"
    );
    let mixed = format!(
        "[extensions]\ndisabled = [\"sentinel\"]\n\
         [formatter.tools.canonical]\nargv = [\"cat\"]\nextensions = [\"txt\"]\n\
         [plugins]\ndisabled = [\"{secret}\"]\n\
         [plugins.formatter.tools.secret]\nargv = [\"{secret}\"]\nextensions = [\"rs\"]\n"
    );
    for (name, config) in [("legacy-only", legacy), ("mixed", mixed)] {
        let (fixture, local_marker, legacy_marker) = formatter_workspace_with_launch_sentinels();
        let enabled_marker = fixture.path().join("enabled-sidecar-launched");
        write_launch_sentinel(
            &fixture.path().join(".tower/extensions"),
            "enabled-sentinel",
            &enabled_marker,
        );
        eprintln!("legacy formatter rejection scenario: {name}");
        assert_legacy_config_rejected_before_spawn(
            binary,
            fixture.path(),
            &config,
            &[secret],
            &[&local_marker, &enabled_marker, &legacy_marker],
        );
    }
}

fn install_formatter_extension(workspace: &Path) {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let fmt_binary = repo_root.join("target/debug/fmt_extension");
    assert!(fmt_binary.is_file(), "fmt_extension fixture must be built");
    let fmt_dir = workspace.join(".tower").join("extensions/fmt");
    std::fs::create_dir_all(&fmt_dir).unwrap();
    std::fs::write(
        fmt_dir.join("extension.toml"),
        format!(
            "name = \"fmt\"\nversion = \"0.1.0\"\ncommand = [\"{}\"]\nactivation = \"eager\"\n\
             [events]\nsubscribe = [\"event/fileChanged\"]\n\
             [capabilities]\nrequired = [\"request_format\", \"list_files\", \"log\"]\n\
             [[tools]]\nname = \"format\"\ndescription = \"format fixture\"\nschema_json = '{{\"type\":\"object\"}}'\n",
            fmt_binary.display()
        ),
    )
    .unwrap();
}

fn configure_formatter_commands(workspace: &Path) -> PathBuf {
    let tower_dir = workspace.join(".tower");
    let formatter_log = workspace.join("formatter.log");
    let tools_dir = workspace.join("formatter-tools");
    std::fs::create_dir_all(&tools_dir).unwrap();
    for tool in ["rustfmt", "gofmt", "zig", "prettier"] {
        write_formatter_fixture(&tools_dir.join(tool), &formatter_log, false);
    }
    write_formatter_fixture(&tools_dir.join("php-cs-fixer"), &formatter_log, true);

    let canonical = format!(
        "[daemon]\nidle_timeout_secs = 30\n\
         [extensions]\ndisabled = [\"sentinel\"]\n\
         [formatter.tools.rustfmt]\nargv = [\"{}\", \"--edition\", \"2021\"]\nextensions = [\"rs\"]\n\
         [formatter.tools.gofmt]\nargv = [\"{}\"]\nextensions = [\"go\"]\n\
         [formatter.tools.zigfmt]\nargv = [\"{}\", \"fmt\", \"--stdin\"]\nextensions = [\"zig\"]\n\
         [formatter.tools.prettier]\nargv = [\"{}\", \"--stdin-filepath\", \"{{path}}\"]\nextensions = [\"ts\"]\n\
         [formatter.tools.php-cs-fixer]\nmode = \"inplace\"\nargv = [\"{}\", \"fix\", \"{{path}}\"]\nextensions = [\"php\"]\n",
        tools_dir.join("rustfmt").display(),
        tools_dir.join("gofmt").display(),
        tools_dir.join("zig").display(),
        tools_dir.join("prettier").display(),
        tools_dir.join("php-cs-fixer").display(),
    );
    std::fs::write(tower_dir.join("config.toml"), canonical).unwrap();

    formatter_log
}

fn assert_formatter_lifecycle(
    binary: &Path,
    workspace: &Path,
    cases: &[(&str, &str)],
    local_marker: &Path,
    legacy_marker: &Path,
) {
    for &(path, _) in cases {
        std::fs::write(workspace.join(path), "before startup\n").unwrap();
    }

    let daemon = Command::new(binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start production daemon");

    wait_until(Duration::from_secs(10), || socket_path(workspace).exists());
    for &(path, _) in cases {
        std::fs::write(workspace.join(path), "format me\n").unwrap();
    }
    wait_until(Duration::from_secs(10), || {
        cases.iter().all(|(path, expected)| {
            std::fs::read_to_string(workspace.join(path))
                .is_ok_and(|contents| contents == *expected)
        })
    });

    let shutdown = run_production(binary, workspace, "shutdown");
    assert!(shutdown.status.success(), "shutdown failed: {shutdown:?}");
    let output = daemon.wait_with_output().expect("wait for daemon shutdown");
    assert!(output.status.success(), "daemon failed: {output:?}");
    assert!(output.stdout.is_empty(), "daemon wrote protocol stdout");
    assert!(!local_marker.exists(), "disabled extension was spawned");
    assert!(
        !legacy_marker.exists(),
        "obsolete .tower/plugins fallback spawned an extension"
    );
}

fn assert_formatter_invocations(workspace: &Path, log: &str) {
    for tool in ["rustfmt", "gofmt", "zig", "prettier", "php-cs-fixer"] {
        assert!(log.contains(&format!("{tool}|")), "log: {log}");
    }
    assert!(log.contains("rustfmt|--edition 2021|"), "log: {log}");
    assert!(
        log.contains(&format!("gofmt||{}", workspace.display())),
        "log: {log}"
    );
    assert!(log.contains("zig|fmt --stdin|"), "log: {log}");
    assert!(
        log.contains(&format!(
            "prettier|--stdin-filepath {}|",
            workspace.join("sample.ts").display()
        )),
        "log: {log}"
    );
    assert!(log.contains("php-cs-fixer|fix "), "log: {log}");
    assert!(
        log.contains(&format!(
            "php-cs-fixer|fix {}|",
            workspace.join("sample.fmt.tmp.php").display()
        )),
        "log: {log}"
    );
}

fn assert_canonical_formatter_execution(binary: &Path) {
    let (fixture, local_marker, legacy_marker) = formatter_workspace_with_launch_sentinels();
    let workspace = fixture.path();
    install_formatter_extension(workspace);
    let formatter_log = configure_formatter_commands(workspace);
    let cases = [
        ("sample.rs", "rustfmt-formatted\n"),
        ("sample.go", "gofmt-formatted\n"),
        ("sample.zig", "zig-formatted\n"),
        ("sample.ts", "prettier-formatted\n"),
        ("sample.php", "php-cs-fixer-formatted\n"),
    ];
    assert_formatter_lifecycle(binary, workspace, &cases, &local_marker, &legacy_marker);
    let log = std::fs::read_to_string(formatter_log).expect("formatter invocation log");
    assert_formatter_invocations(workspace, &log);
}

#[test]
fn fresh_init_then_daemon_startup_indexes_every_eligible_fixture_path() {
    let binary = selected_production_binary();
    let fixture = tempdir().unwrap();
    let workspace = fixture.path();
    std::fs::create_dir_all(workspace.join("nested/deeper")).unwrap();
    for path in ["root.rs", "nested/one.go", "nested/deeper/two.php"] {
        std::fs::write(workspace.join(path), path).unwrap();
    }

    let init = run_production(&binary, workspace, "init");
    assert!(
        init.status.success(),
        "init stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let mut daemon = Command::new(&binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start daemon after init");

    wait_until(Duration::from_secs(10), || socket_path(workspace).exists());
    let expected_paths = vec![
        "nested/deeper/two.php".to_owned(),
        "nested/one.go".to_owned(),
        "root.rs".to_owned(),
    ];
    let mut observed_paths = Err("tower_list_dir was not called".to_owned());
    wait_until(Duration::from_secs(10), || {
        observed_paths = indexed_paths_through_mcp(workspace);
        eprintln!("tower_list_dir result: {observed_paths:?}");
        observed_paths.as_ref() == Ok(&expected_paths)
    });
    assert_eq!(
        observed_paths.expect("query indexed paths through MCP"),
        expected_paths,
        "tower_list_dir must expose exactly the eligible fixture files"
    );

    let shutdown = run_production(&binary, workspace, "shutdown");
    assert!(shutdown.status.success());
    let status = daemon.wait().expect("wait for daemon");
    assert!(status.success(), "daemon exited with {status}");
}

#[test]
fn generated_config_is_canonical_and_reconciliation_failure_is_process_visible() {
    let production = selected_binary("TOWER_PRODUCTION_BIN");
    let test_hooks = selected_binary("TOWER_TEST_BIN");

    let legacy_fixture = tempdir().unwrap();
    let legacy_workspace = legacy_fixture.path();
    let legacy_marker = legacy_workspace.join("legacy-sidecar-launched");
    fs::create_dir_all(legacy_workspace.join(".tower")).unwrap();
    write_launch_sentinel(
        &legacy_workspace.join(".tower/extensions"),
        "legacy-rejection-sentinel",
        &legacy_marker,
    );
    assert_legacy_config_rejected_before_spawn(
        &production,
        legacy_workspace,
        "[plugins]\ndisabled = []\n[plugins.formatter.tools.legacy]\nargv = [\"cat\"]\nextensions = [\"rs\"]\n",
        &[],
        &[&legacy_marker],
    );

    let fixture = tempdir().unwrap();
    let workspace = fixture.path();
    fs::create_dir_all(workspace.join("nested/deeper")).unwrap();
    for path in ["root.rs", "nested/one.go", "nested/deeper/two.php"] {
        fs::write(workspace.join(path), path).unwrap();
    }

    let init = run_production(&production, workspace, "init");
    assert!(
        init.status.success(),
        "init stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let config = fs::read_to_string(workspace.join(".tower/config.toml")).unwrap();
    for forbidden in ["[plugins]", "plugin_fmt", ".wasm", "apply_backcompat"] {
        assert!(
            !config.contains(forbidden),
            "generated config contains {forbidden}"
        );
    }
    assert!(workspace.join(".towerignore").is_file());
    let mut generated = fs::read_dir(workspace.join(".tower"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    generated.sort();
    assert_eq!(generated, ["config.toml".to_owned()]);

    let daemon = Command::new(&production)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start daemon after canonical init");
    wait_until(Duration::from_secs(10), || socket_path(workspace).exists());
    let expected = vec![
        "nested/deeper/two.php".to_owned(),
        "nested/one.go".to_owned(),
        "root.rs".to_owned(),
    ];
    let mut observed = Err("tower_list_dir was not called".to_owned());
    wait_until(Duration::from_secs(10), || {
        observed = indexed_paths_through_mcp(workspace);
        observed.as_ref() == Ok(&expected)
    });
    assert_eq!(observed.unwrap(), expected);
    let shutdown = run_production(&production, workspace, "shutdown");
    assert!(shutdown.status.success(), "shutdown failed: {shutdown:?}");
    let output = daemon.wait_with_output().unwrap();
    assert!(output.status.success(), "daemon failed: {output:?}");
    assert!(output.stdout.is_empty(), "daemon wrote protocol stdout");
    assert_no_deprecated_startup_artifacts(workspace);

    reconciliation_process::assert_reconciliation_failure_contract(&production, &test_hooks);
}

#[test]
#[ignore = "manual real-disk startup benchmark; requires pinned baseline and explicit binaries"]
fn unchanged_workspace_startup_records_reproducible_real_disk_timing() {
    const BASELINE_COMMIT: &str = "c4716405d819fc7234674a466e99d8e87b0ccbeb";
    let perf_root = selected_existing_directory("TOWER_PERF_TMPDIR");
    let baseline = selected_binary("TOWER_BASELINE_BIN");
    let production = selected_binary("TOWER_PRODUCTION_BIN");
    let test_hooks = selected_binary("TOWER_TEST_BIN");

    let system_temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(
        !perf_root.starts_with(&system_temp),
        "TOWER_PERF_TMPDIR must resolve outside {}",
        system_temp.display()
    );
    assert_ne!(
        baseline.canonicalize().unwrap(),
        production.canonicalize().unwrap()
    );
    assert_ne!(
        baseline.canonicalize().unwrap(),
        test_hooks.canonicalize().unwrap()
    );
    let baseline_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join(".tower/perf-baseline")
        .canonicalize()
        .expect("prepare .tower/perf-baseline from the pinned commit first");
    let expected_baseline = baseline_root.join("target/debug/tower");
    assert_eq!(
        baseline.canonicalize().unwrap(),
        expected_baseline
            .canonicalize()
            .expect("build the pinned baseline at target/debug/tower"),
        "TOWER_BASELINE_BIN must select the pinned worktree's planned output"
    );
    let baseline_head = Command::new("git")
        .arg("-C")
        .arg(&baseline_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read pinned baseline worktree HEAD");
    assert!(
        baseline_head.status.success(),
        "baseline directory must be a readable pinned worktree"
    );
    assert_eq!(
        String::from_utf8(baseline_head.stdout).unwrap().trim(),
        BASELINE_COMMIT
    );
    assert_ne!(
        executable_sha256(&baseline),
        executable_sha256(&production),
        "baseline and production binaries must not be byte-identical"
    );

    let baseline_fixture = tempfile::Builder::new()
        .prefix("tower-baseline-")
        .tempdir_in(&perf_root)
        .unwrap();
    let production_fixture = tempfile::Builder::new()
        .prefix("tower-f009-")
        .tempdir_in(&perf_root)
        .unwrap();
    write_representative_workspace(baseline_fixture.path());
    write_representative_workspace(production_fixture.path());

    start_and_observe_inventory(&baseline, baseline_fixture.path(), 1_000);
    start_and_observe_inventory(&production, production_fixture.path(), 1_000);

    for _ in 0..10 {
        start_and_observe_inventory(&baseline, baseline_fixture.path(), 1_000);
        start_and_observe_inventory(&production, production_fixture.path(), 1_000);
    }

    let mut baseline_samples = Vec::with_capacity(30);
    let mut production_samples = Vec::with_capacity(30);
    for _ in 0..30 {
        baseline_samples.push(start_and_observe_inventory(
            &baseline,
            baseline_fixture.path(),
            1_000,
        ));
        production_samples.push(start_and_observe_inventory(
            &production,
            production_fixture.path(),
            1_000,
        ));
    }

    let expected_metrics = json!({
        "content_reads": 0,
        "content_hashes": 0,
        "file_record_rewrites": 0,
    });
    for _ in 0..3 {
        assert_eq!(
            run_unchanged_metrics(&test_hooks, production_fixture.path()),
            expected_metrics
        );
    }

    let baseline_summary = percentile_summary(&baseline_samples);
    let production_summary = percentile_summary(&production_samples);
    println!("baseline_commit={BASELINE_COMMIT}");
    println!(
        "rustc={}",
        String::from_utf8(
            Command::new("rustc")
                .arg("--version")
                .output()
                .unwrap()
                .stdout
        )
        .unwrap()
        .trim()
    );
    println!("filesystem={}", perf_root.display());
    println!("baseline_sha256={}", executable_sha256(&baseline));
    println!("production_sha256={}", executable_sha256(&production));
    println!("baseline_raw_us={baseline_samples:?}");
    println!("production_raw_us={production_samples:?}");
    println!(
        "baseline samples=30 min={} median={} p95={}",
        baseline_summary.0, baseline_summary.1, baseline_summary.2
    );
    println!(
        "production samples=30 min={} median={} p95={}",
        production_summary.0, production_summary.1, production_summary.2
    );
    println!("telemetry samples=3 content_reads=0 content_hashes=0 file_record_rewrites=0");
}

#[tokio::test]
async fn control_status_reports_a_snapshot() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    std::fs::write(dir.path().join("a.rs"), b"pub fn a() {}").unwrap();
    spawn_daemon_thread(dir.path().to_path_buf(), config_with_idle(300));

    let sock = socket_path(dir.path());
    let mut conn = wait_for_socket(&sock, Duration::from_secs(5))
        .await
        .unwrap();
    conn.write_all(Handshake::new(ClientRole::Control).to_line().as_bytes())
        .await
        .unwrap();
    write_line(
        &mut conn,
        &serde_json::to_string(&ControlRequest::Status).unwrap(),
    )
    .await
    .unwrap();

    let line = read_line_capped(&mut conn, 65536).await.unwrap().unwrap();
    let resp: ControlResponse = serde_json::from_str(&line).unwrap();
    match resp {
        ControlResponse::Status(s) => assert_eq!(s.indexed_files, 1),
        _ => panic!("expected status snapshot"),
    }
}

#[tokio::test]
async fn daemon_self_terminates_after_idle() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    spawn_daemon_thread(dir.path().to_path_buf(), config_with_idle(1)); // 1s idle

    let sock = socket_path(dir.path());
    // Daemon is up.
    let _ = wait_for_socket(&sock, Duration::from_secs(5))
        .await
        .unwrap();
    // With no keep-alive clients connected, it should exit after the idle timeout and the
    // socket should stop accepting.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(
        core_engine::adapters::daemon::socket::try_connect(&sock)
            .await
            .is_none(),
        "daemon should have shut down after idle"
    );
}

#[tokio::test]
async fn mcp_client_connect_or_spawn_then_initialize() {
    use core_engine::adapters::daemon::client::connect_or_spawn;
    use core_engine::adapters::daemon::wire::{ClientRole, Handshake};

    let dir = tempdir().unwrap();

    // No daemon yet: connect_or_spawn must launch one via the real binary.
    // Fresh workspaces do not have `.tower/` yet; the client/daemon path must
    // create its own runtime directory before locking, logging, and binding.
    // Register the tower binary path so daemon_exe() spawns the right binary
    // (not the test runner). CARGO_BIN_EXE_tower is only set when compiling
    // integration test binaries, not when compiling the library.
    core_engine::adapters::daemon::client::register_test_daemon_exe(std::path::Path::new(env!(
        "CARGO_BIN_EXE_tower"
    )));
    let mut conn = connect_or_spawn(dir.path())
        .await
        .expect("connect-or-spawn");

    conn.write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .unwrap();
    // A minimal rmcp initialize request must get a response from the shared engine.
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    conn.write_all(init).await.unwrap();
    conn.write_all(b"\n").await.unwrap();

    let line = core_engine::adapters::daemon::wire::read_line_capped(&mut conn, 1 << 20)
        .await
        .unwrap()
        .expect("initialize response");
    assert!(line.contains("\"result\""), "got: {line}");

    // Cleanup: ask the daemon to stop.
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
}

#[tokio::test]
async fn concurrent_clients_share_one_daemon() {
    use core_engine::adapters::daemon::client::connect_or_spawn;

    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();

    // Register the real tower binary so daemon_exe() spawns the right process.
    core_engine::adapters::daemon::client::register_test_daemon_exe(std::path::Path::new(env!(
        "CARGO_BIN_EXE_tower"
    )));

    // Launch K connect-or-spawn calls near-simultaneously.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let p = dir.path().to_path_buf();
        handles.push(tokio::spawn(
            async move { connect_or_spawn(&p).await.is_ok() },
        ));
    }
    let mut ok = 0;
    for h in handles {
        if h.await.unwrap() {
            ok += 1;
        }
    }
    assert_eq!(ok, 8, "all clients connect");

    // Exactly one daemon → exactly one Sled lock dir, one socket.
    let snap = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Status,
    )
    .await
    .expect("status");
    if let ControlResponse::Status(s) = snap {
        assert!(s.uptime_secs < 60); // single fresh daemon answered
    }
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
}

#[tokio::test]
async fn daemon_rejects_incompatible_protocol_handshake() {
    use core_engine::adapters::daemon::socket::{socket_path, wait_for_socket};
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    spawn_daemon_thread(dir.path().to_path_buf(), config_with_idle(300));
    let sock = socket_path(dir.path());
    let mut conn = wait_for_socket(&sock, Duration::from_secs(5))
        .await
        .unwrap();
    // Hand-write a future-protocol handshake; the daemon must drop the connection.
    conn.write_all(br#"{"role":"mcp","protocol":999}"#)
        .await
        .unwrap();
    conn.write_all(b"\n").await.unwrap();
    let mut buf = [0u8; 16];
    // Daemon closes the connection → read returns 0.
    let n = conn.read(&mut buf).await.unwrap_or(0);
    assert_eq!(n, 0, "incompatible handshake must be dropped");
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
}
