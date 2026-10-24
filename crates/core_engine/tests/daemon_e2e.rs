#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::{DaemonConfig, TowerConfig};
use core_engine::adapters::daemon::server::run_daemon;
use core_engine::adapters::daemon::socket::{WorkspaceDaemonOwner, socket_path, wait_for_socket};
use core_engine::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, read_line_capped, write_line,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use extension_protocol::{Event, InitParams, PROTOCOL_VERSION, Request, Response};

#[path = "support/process_binary.rs"]
mod process_binary;

#[path = "support/reconciliation_process.rs"]
mod reconciliation_process;

fn config_with_idle(secs: u64) -> TowerConfig {
    TowerConfig {
        daemon: DaemonConfig {
            idle_timeout_secs: secs,
            connection_timeout_secs: 10,
            preparation_timeout_secs: 10,
        },
        ..Default::default()
    }
}

async fn read_raw_control_status(workspace: &Path) -> String {
    let mut connection = wait_for_socket(&socket_path(workspace), Duration::from_secs(5))
        .await
        .expect("connect control client for raw status");
    connection
        .write_all(Handshake::new(ClientRole::Control).to_line().as_bytes())
        .await
        .expect("send control handshake");
    write_line(
        &mut connection,
        &serde_json::to_string(&ControlRequest::Status).expect("serialize status request"),
    )
    .await
    .expect("send status request");
    read_line_capped(&mut connection, 1 << 20)
        .await
        .expect("read raw status response")
        .expect("daemon returned a raw status response")
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

fn slow_start_process_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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

struct ProductionDaemonGuard {
    child: Option<Child>,
    binary: PathBuf,
    workspace: PathBuf,
}

impl ProductionDaemonGuard {
    fn new(child: Child, binary: &Path, workspace: &Path) -> Self {
        Self {
            child: Some(child),
            binary: binary.to_path_buf(),
            workspace: workspace.to_path_buf(),
        }
    }

    fn wait_and_disarm(&mut self) -> std::process::ExitStatus {
        let status = self
            .child
            .as_mut()
            .expect("daemon guard is armed")
            .wait()
            .expect("wait for lint daemon shutdown");
        self.child = None;
        status
    }
}

impl Drop for ProductionDaemonGuard {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };

        let _ = Command::new(&self.binary)
            .arg("shutdown")
            .arg("--workspace-dir")
            .arg(&self.workspace)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
#[test]
fn production_daemon_guard_reaps_child_on_failure_path() {
    let child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .spawn()
        .expect("spawn guarded process");
    let pid = child.id().to_string();
    let guard = ProductionDaemonGuard::new(child, Path::new("/bin/true"), Path::new("/tmp"));

    drop(guard);

    let status = Command::new("kill")
        .args(["-0", &pid])
        .stderr(Stdio::inherit())
        .status()
        .expect("probe guarded process");
    assert!(!status.success(), "guarded process {pid} was not reaped");
}

fn selected_binary(selector: &str) -> PathBuf {
    process_binary::selected_binary(selector)
}

fn install_ast_extension(workspace: &Path) {
    let extension_dir = workspace.join(".tower/extensions/ast");
    fs::create_dir_all(&extension_dir).expect("create AST extension directory");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root");
    let binary = root
        .join("target/debug/ast_extension")
        .to_string_lossy()
        .replace('\\', "\\\\");
    let manifest = fs::read_to_string(root.join("extensions/ast/extension.toml"))
        .expect("read shipped AST manifest")
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("command") {
                format!("command = [\"{binary}\"]")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(extension_dir.join("extension.toml"), manifest)
        .expect("write workspace AST manifest");
}

fn install_lint_extension(workspace: &Path) {
    let extension_dir = workspace.join(".tower/extensions/lint");
    fs::create_dir_all(&extension_dir).expect("create lint extension directory");
    let binary = workspace_root()
        .join("target/debug/lint_extension")
        .to_string_lossy()
        .replace('\\', "\\\\");
    let manifest = fs::read_to_string(workspace_root().join("extensions/lint/extension.toml"))
        .expect("read shipped lint manifest")
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("command") {
                format!("command = [\"{binary}\"]")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(extension_dir.join("extension.toml"), manifest)
        .expect("write workspace lint manifest");
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn slow_start_extension_bin() -> PathBuf {
    workspace_root().join("target/debug/slow_start_extension")
}

struct SlowStartChild {
    child: Child,
    stdin: std::process::ChildStdin,
    frames: std::sync::mpsc::Receiver<Value>,
}

impl SlowStartChild {
    fn spawn() -> Self {
        let mut child = Command::new(slow_start_extension_bin())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn slow-start fixture; build workspace binaries first");
        let stdin = child.stdin.take().expect("slow-start stdin");
        let stdout = child.stdout.take().expect("slow-start stdout");
        let (sender, frames) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(frame) = serde_json::from_str(&line) else {
                    break;
                };
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            frames,
        }
    }

    fn send(&mut self, frame: Value) {
        serde_json::to_writer(&mut self.stdin, &frame).expect("serialize fixture request");
        self.stdin.write_all(b"\n").expect("write fixture newline");
        self.stdin.flush().expect("flush fixture request");
    }

    fn recv(&self) -> Value {
        self.frames
            .recv_timeout(Duration::from_secs(2))
            .expect("fixture response within the extension operation budget")
    }

    fn initialize(&mut self, id: u64, mode: Option<Value>) -> Value {
        let mut params = json!({
            "protocol_version": PROTOCOL_VERSION,
            "client_info": "daemon-e2e",
        });
        if let Some(mode) = mode {
            params["extension_config"] = mode;
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": params,
        }));
        self.recv()
    }

    fn deliver_event(&mut self, id: u64, event: Event) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "deliverEvent",
            "params": serde_json::to_value(event).expect("serialize fixture event"),
        }));
        self.recv()
    }

    fn invoke_tool(&mut self, id: u64, name: &str, params: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "invokeTool",
            "params": {"name": name, "params": params},
        }));
        self.recv()
    }
}

impl Drop for SlowStartChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn install_slow_start_extension(workspace: &Path) {
    let extension_dir = workspace.join(".tower/extensions/slow_start");
    fs::create_dir_all(&extension_dir).expect("create slow-start extension directory");
    let binary = slow_start_extension_bin()
        .to_string_lossy()
        .replace('\\', "\\\\");
    let manifest =
        fs::read_to_string(workspace_root().join("extensions/fixtures/slow_start/extension.toml"))
            .expect("read slow-start manifest")
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("command") {
                    format!("command = [\"{binary}\"]")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    fs::write(extension_dir.join("extension.toml"), manifest).expect("write slow-start manifest");
}

fn spawn_daemon_at_pre_bind(binary: &Path, workspace: &Path) -> Child {
    std::fs::create_dir_all(workspace.join(".tower")).unwrap();
    std::fs::write(
        workspace.join(".tower/config.toml"),
        "[daemon]\nidle_timeout_secs = 300\nconnection_timeout_secs = 1\n",
    )
    .unwrap();
    let mut child = Command::new(binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .env("TOWER_TEST_PRE_BIND_HANDSHAKE", "stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start daemon with pre-bind milestone");
    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut ready = String::new();
        let result = BufReader::new(stdout).read_line(&mut ready).map(|_| ready);
        let _ = ready_tx.send(result);
    });
    let ready = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(ready)) => ready,
        outcome => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pre-bind milestone unavailable: {outcome:?}");
        }
    };
    if ready != "ownership-acquired\n" {
        let _ = child.kill();
        let _ = child.wait();
        panic!("unexpected pre-bind milestone: {ready:?}");
    }
    child
}

fn release_pre_bind(child: &mut Child) {
    let mut stdin = child.stdin.take().expect("hooked daemon stdin");
    stdin.write_all(b"continue\n").unwrap();
    stdin.flush().unwrap();
}

fn spawn_daemon_at_preparation_barrier(binary: &Path, workspace: &Path, barrier: &str) -> Child {
    spawn_daemon_at_preparation_barrier_with_config(binary, workspace, barrier, "")
}

fn spawn_daemon_at_preparation_barrier_with_config(
    binary: &Path,
    workspace: &Path,
    barrier: &str,
    extra_config: &str,
) -> Child {
    std::fs::create_dir_all(workspace.join(".tower")).unwrap();
    std::fs::write(
        workspace.join(".tower/config.toml"),
        format!("[daemon]\nidle_timeout_secs = 300\nconnection_timeout_secs = 2\npreparation_timeout_secs = 2\n{extra_config}"),
    )
    .unwrap();
    let mut child = Command::new(binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .env("TOWER_TEST_PREPARATION_HANDSHAKE", "stdio")
        .env("TOWER_TEST_PREPARATION_BARRIER", barrier)
        .env(
            "TOWER_TEST_CLEANUP_TRACE",
            workspace.join(".tower/preparation-cleanup.json"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start daemon at preparation barrier");
    let stdout = child.stdout.take().unwrap();
    let (milestone_tx, milestone_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut milestone = String::new();
        let result = BufReader::new(stdout)
            .read_line(&mut milestone)
            .map(|_| milestone);
        let _ = milestone_tx.send(result);
    });
    let milestone = match milestone_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(milestone)) => milestone,
        outcome => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("preparation milestone unavailable: {outcome:?}");
        }
    };
    if milestone != format!("transport-bound:{barrier}\n") {
        let _ = child.kill();
        let _ = child.wait();
        panic!("unexpected preparation milestone: {milestone:?}");
    }
    child
}

fn release_preparation_barrier(child: &mut Child) {
    let stdin = child.stdin.as_mut().expect("hooked daemon stdin");
    stdin.write_all(b"continue\n").unwrap();
    stdin.flush().unwrap();
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

async fn await_watcher_path(
    connection: &mut UnixStream,
    id: u64,
    path: &str,
    present: bool,
    deadline: Instant,
) {
    loop {
        write_line(
            connection,
            &json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": "tower_find_file", "arguments": {"query": path}}
            })
            .to_string(),
        )
        .await
        .expect("observe watcher index through MCP");
        let response: Value = serde_json::from_str(
            &read_line_capped(connection, 1 << 20)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let data: Value = serde_json::from_str(
            response["result"]["content"][0]["text"]
                .as_str()
                .expect("indexed path response"),
        )
        .unwrap();
        let indexed = data["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == path);
        if indexed == present {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "watcher did not publish expected indexed state for {path}: {response}"
        );
        tokio::task::yield_now().await;
    }
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
    let runtime = tokio::runtime::Runtime::new().expect("formatter status runtime");
    let opts = GlobalOpts {
        workspace_dir: Some(workspace.to_path_buf()),
        extensions_dir: None,
    };
    wait_until(Duration::from_secs(10), || {
        matches!(runtime.block_on(core_engine::adapters::daemon::client::send_control_async(&opts, ControlRequest::Status)),
            Ok(ControlResponse::Status(snapshot)) if snapshot.lifecycle == core_engine::adapters::daemon::DaemonLifecycle::Ready)
    });
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

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let line = read_line_capped(&mut conn, 65536).await.unwrap().unwrap();
        let resp: ControlResponse = serde_json::from_str(&line).unwrap();
        match resp {
            ControlResponse::Status(s) if s.indexed_files == 1 => {
                assert!(s.generation > 0);
                assert!(matches!(
                    s.lifecycle,
                    core_engine::adapters::daemon::DaemonLifecycle::Starting { .. }
                        | core_engine::adapters::daemon::DaemonLifecycle::Ready
                        | core_engine::adapters::daemon::DaemonLifecycle::Degraded { .. }
                ));
                assert_eq!(
                    s.workspace,
                    core_engine::adapters::daemon::CapabilityReadiness::Ready
                );
                assert!(matches!(
                    s.lsp,
                    core_engine::adapters::daemon::CapabilityReadiness::Preparing { .. }
                        | core_engine::adapters::daemon::CapabilityReadiness::Ready
                        | core_engine::adapters::daemon::CapabilityReadiness::Unavailable
                ));
                break;
            }
            ControlResponse::Status(_) if Instant::now() < deadline => {
                conn = wait_for_socket(&sock, Duration::from_secs(1))
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
                tokio::task::yield_now().await;
            }
            _ => panic!("expected indexed status snapshot before deadline"),
        }
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
            .unwrap()
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

#[cfg(unix)]
#[tokio::test]
async fn daemon_lint_default_timeout_precedes_small_global_deadline() {
    for explicit_timeout in [None, Some(2_u64)] {
        let payload = daemon_lint_timeout_payload(explicit_timeout).await;
        assert_eq!(payload["error"]["code"], "lint_timeout");
        assert_eq!(payload["supported"], false);
        assert_eq!(payload["diagnostics"], json!([]));
    }
}

#[cfg(unix)]
async fn daemon_lint_timeout_payload(explicit_timeout: Option<u64>) -> Value {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempdir().expect("create lint deadline workspace");
    install_lint_extension(workspace.path());
    fs::create_dir_all(workspace.path().join("src")).expect("create source directory");
    fs::write(workspace.path().join("src/main.txt"), "lint me\n").expect("seed source");
    let command = workspace.path().join("hang-lint.sh");
    fs::write(&command, "#!/bin/sh\nsleep 60\n").expect("write hanging linter");
    let mut permissions = fs::metadata(&command).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&command, permissions).expect("make hanging linter executable");
    let timeout = explicit_timeout
        .map(|seconds| format!("timeout_secs = {seconds}\n"))
        .unwrap_or_default();
    fs::write(
        workspace.path().join(".tower/config.toml"),
        format!(
            "[daemon]\nidle_timeout_secs = 300\n\
             [extensions]\nrequest_timeout_secs = 1\n\
             [lint.fixture]\ncommand = {:?}\n{}extensions = [\"txt\"]\nformat = \"generic-regex\"\ntarget = \"append\"\nregex = \"^(?P<message>.*)$\"\nsource = \"fixture-lint\"\n",
            command.to_string_lossy(),
            timeout
        ),
    )
    .expect("write lint configuration");

    let production_binary = selected_production_binary();
    let daemon = Command::new(&production_binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn production daemon");
    let mut daemon = ProductionDaemonGuard::new(daemon, &production_binary, workspace.path());
    let mut connection = wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("production daemon binds its public socket");
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .expect("send MCP handshake");
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"lint-deadline","version":"1"}}}"#,
    )
    .await
    .expect("send MCP initialize");
    read_line_capped(&mut connection, 1 << 20)
        .await
        .expect("read MCP initialize")
        .expect("MCP initialize response");

    let discovery_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let raw_status = read_raw_control_status(workspace.path()).await;
        let status: Value = serde_json::from_str(&raw_status).expect("parse daemon status");
        if status["extensions"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool == "check"))
        {
            break;
        }
        if Instant::now() >= discovery_deadline {
            panic!("lint extension was not discovered through the production daemon: {raw_status}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tower_lint_check","arguments":{"path":"src/main.txt"}}}"#,
    )
    .await
    .expect("request lint check");
    let raw = tokio::time::timeout(
        Duration::from_secs(7),
        read_line_capped(&mut connection, 1 << 20),
    )
    .await
    .expect("client budget above five seconds must not expire")
    .expect("read lint response")
    .expect("lint response frame");
    let response: Value = serde_json::from_str(&raw).expect("parse lint response");
    let payload = response["result"]["content"][0]["text"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());

    drop(connection);
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
    let status = daemon.wait_and_disarm();
    assert!(status.success(), "daemon exited with {status}");

    payload.unwrap_or_else(|| {
        panic!("lint tool must return a structured success-result payload: {raw}")
    })
}

#[tokio::test]
async fn cold_start_initializes_before_extension_catch_up() {
    use core_engine::adapters::daemon::CapabilityReadiness;
    use core_engine::adapters::daemon::client::connect_or_spawn;
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };

    let mut recovery = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Block);
    recovery.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    recovery
        .await_milestone(StartupMilestone::TransportBound, deadline)
        .await;
    recovery
        .await_milestone(StartupMilestone::WorkspaceReady, deadline)
        .await;
    recovery
        .await_milestone(StartupMilestone::CatchUpBlocked, deadline)
        .await;
    recovery.join_generation().await;

    let preparing = recovery.status().await;
    assert_eq!(preparing.workspace, CapabilityReadiness::Ready);
    assert!(matches!(
        preparing.ast,
        CapabilityReadiness::Preparing { .. }
    ));
    let blocked = recovery.observe_probe().await;
    assert!(!blocked.released);
    assert!(blocked.pending_fences > 0);

    let released = recovery.release_probe().await;
    assert!(released.released);
    recovery
        .await_milestone(StartupMilestone::CatchUpReleased, deadline)
        .await;
    let ready = recovery.status().await;
    assert_eq!(ready.ast, CapabilityReadiness::Ready);
    let converged = recovery.observe_probe().await;
    assert!(converged.barrier_observed);
    recovery.shutdown_and_await_release(deadline).await;

    let dir = tempdir().unwrap();
    let production = std::path::Path::new(env!("CARGO_BIN_EXE_tower"));
    let hooks = selected_binary("TOWER_TEST_BIN");
    core_engine::adapters::daemon::client::register_test_daemon_exe(production);
    let mut daemon = spawn_daemon_at_pre_bind(&hooks, dir.path());

    let mut handles = Vec::new();
    for _ in 0..8 {
        let p = dir.path().to_path_buf();
        handles.push(tokio::spawn(async move { connect_or_spawn(&p).await }));
    }
    release_pre_bind(&mut daemon);

    for h in handles {
        h.await.unwrap().expect("client joins starting daemon");
    }

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
        assert!(s.generation > 0);
    } else {
        panic!("expected status snapshot");
    }
    assert!(
        sled::open(dir.path().join(".tower/db")).is_err(),
        "the running daemon must be the sole Sled opener"
    );
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
    let status = daemon.wait().expect("wait for daemon shutdown");
    assert!(status.success(), "daemon exited with {status}");
}

#[tokio::test]
async fn first_client_disconnect_preserves_single_daemon_owner() {
    use core_engine::adapters::daemon::client::connect_or_spawn;
    use core_engine::adapters::daemon::socket::{OwnerLiveness, probe_owner};

    let dir = tempdir().unwrap();
    let production = std::path::Path::new(env!("CARGO_BIN_EXE_tower"));
    let hooks = selected_binary("TOWER_TEST_BIN");
    core_engine::adapters::daemon::client::register_test_daemon_exe(production);
    let mut daemon = spawn_daemon_at_pre_bind(&hooks, dir.path());

    let started = Instant::now();
    let error = connect_or_spawn(dir.path())
        .await
        .expect_err("client must exhaust its connection deadline before bind");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Live);

    release_pre_bind(&mut daemon);
    wait_for_socket(&socket_path(dir.path()), Duration::from_secs(5))
        .await
        .expect("same owner binds after timed-out client disconnects");
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
    let status = daemon.wait().expect("wait for daemon shutdown");
    assert!(status.success(), "daemon exited with {status}");
}

#[tokio::test]
async fn dead_owner_allows_clean_restart() {
    use core_engine::adapters::daemon::client::connect_or_spawn;
    use core_engine::adapters::daemon::socket::{OwnerLiveness, probe_owner};

    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    std::fs::write(
        dir.path().join(".tower/config.toml"),
        "[daemon]\nidle_timeout_secs = 300\n",
    )
    .unwrap();
    let production = std::path::Path::new(env!("CARGO_BIN_EXE_tower"));
    core_engine::adapters::daemon::client::register_test_daemon_exe(production);
    let mut first = Command::new(production)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start first daemon");
    wait_for_socket(&socket_path(dir.path()), Duration::from_secs(5))
        .await
        .expect("first daemon socket");
    let first_generation = match core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Status,
    )
    .await
    .unwrap()
    {
        ControlResponse::Status(snapshot) => snapshot.generation,
        response => panic!("expected status snapshot, got {response:?}"),
    };

    first.kill().expect("terminate first daemon");
    first.wait().expect("reap first daemon");
    assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Dead);

    connect_or_spawn(dir.path())
        .await
        .expect("restart after dead owner");
    let successor = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(dir.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Status,
    )
    .await
    .unwrap();
    match successor {
        ControlResponse::Status(snapshot) => assert!(snapshot.generation > first_generation),
        response => panic!("expected status snapshot, got {response:?}"),
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

#[tokio::test]
async fn malformed_tool_parameters_remain_invalid_params() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    spawn_daemon_thread(dir.path().to_path_buf(), config_with_idle(300));
    let mut connection = wait_for_socket(&socket_path(dir.path()), Duration::from_secs(5))
        .await
        .unwrap();
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .unwrap();
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"daemon-e2e","version":"1"}}}"#,
    )
    .await
    .unwrap();
    let initialized = read_line_capped(&mut connection, 1 << 20)
        .await
        .unwrap()
        .unwrap();
    assert!(serde_json::from_str::<Value>(&initialized).unwrap()["result"].is_object());
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await
    .unwrap();

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tower_read_file","arguments":{}}}"#,
    )
    .await
    .unwrap();
    let response = read_line_capped(&mut connection, 1 << 20)
        .await
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();

    assert_eq!(response["error"]["code"], -32602);
    assert_ne!(response["error"]["code"], -32004);
    assert_ne!(response["error"]["code"], -32005);
    assert_ne!(response["error"]["code"], -32006);

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
async fn mutations_wait_until_workspace_reconciliation_is_ready() {
    let workspace = tempdir().expect("create workspace");
    let hooks = selected_binary("TOWER_TEST_BIN");
    let mut daemon = spawn_daemon_at_preparation_barrier(
        &hooks,
        workspace.path(),
        "before-reconciliation-commit",
    );
    let mut connection = wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("early transport accepts MCP clients");
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .unwrap();
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"preparation-gate","version":"1"}}}"#,
    )
    .await
    .unwrap();
    let initialized: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("initialize remains available during preparation"),
    )
    .expect("initialize response is JSON");
    assert!(
        initialized["result"].is_object(),
        "early MCP initialize must succeed without an EngineState: {initialized}"
    );
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":11,"method":"tools/list","params":{}}"#,
    )
    .await
    .unwrap();
    let early_tools: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("native declarations remain available during preparation"),
    )
    .expect("early tools/list response is JSON");
    let early_tool_names = early_tools["result"]["tools"]
        .as_array()
        .expect("early tools/list result contains the stable native declarations")
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("tool declaration name")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(
        early_tool_names
            .iter()
            .any(|name| name == "tower_read_file"),
        "native declarations must be stable before workspace readiness"
    );

    let early_status = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Status,
    )
    .await
    .expect("control status remains available before workspace readiness");
    assert!(matches!(
        early_status,
        ControlResponse::Status(ref snapshot)
            if matches!(snapshot.workspace, core_engine::adapters::daemon::CapabilityReadiness::Preparing { .. })
    ));

    let mut read_connection =
        wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
            .await
            .expect("a second early MCP client can connect");
    read_connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .unwrap();
    write_line(
        &mut read_connection,
        r#"{"jsonrpc":"2.0","id":12,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"preparation-read-gate","version":"1"}}}"#,
    )
    .await
    .unwrap();
    read_line_capped(&mut read_connection, 1 << 20)
        .await
        .unwrap()
        .expect("second early initialize succeeds");
    write_line(
        &mut read_connection,
        r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"tower_read_file","arguments":{"path":"not-yet-readable.rs"}}}"#,
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            read_line_capped(&mut read_connection, 1 << 20)
        )
        .await
        .is_err(),
        "a native read must not obtain EngineState before workspace readiness"
    );

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tower_create_file","arguments":{"path":"gated.rs","content":"pub fn gated() {}\n"}}}"#,
    )
    .await
    .unwrap();

    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            read_line_capped(&mut connection, 1 << 20)
        )
        .await
        .is_err(),
        "mutation completed before durable reconciliation"
    );
    assert!(!workspace.path().join("gated.rs").exists());

    release_preparation_barrier(&mut daemon);
    let response = read_line_capped(&mut connection, 1 << 20)
        .await
        .unwrap()
        .expect("mutation completes after workspace publication");
    assert!(serde_json::from_str::<Value>(&response).unwrap()["result"].is_object());
    assert!(workspace.path().join("gated.rs").exists());

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":14,"method":"tools/list","params":{}}"#,
    )
    .await
    .unwrap();
    let ready_tools: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("tools/list succeeds after workspace readiness"),
    )
    .expect("ready tools/list response is JSON");
    let ready_tool_names = ready_tools["result"]["tools"]
        .as_array()
        .expect("ready tools/list result contains declarations")
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("tool declaration name")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(early_tool_names, ready_tool_names);

    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
    assert!(daemon.wait().expect("join daemon").success());
}

#[tokio::test]
async fn capabilities_become_ready_independently() {
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };

    // Each result below comes from a configured real process dependency. This
    // makes the admission contract observable without peeking at tool lists.
    let mut harness = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Block);
    harness.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    harness
        .await_milestone(StartupMilestone::CatchUpBlocked, deadline)
        .await;
    assert!(
        harness
            .call_tool(
                21,
                "tower_create_file",
                json!({"path":"blocked.rs","content":"pub fn blocked() {}\n"}),
            )
            .await["result"]
            .is_object(),
        "workspace mutations remain independent of AST catch-up"
    );
    harness.release_probe().await;
    harness
        .await_milestone(StartupMilestone::CatchUpReleased, deadline)
        .await;
    assert!(
        harness
            .call_tool(24, "tower_ast_get_outline", json!({"path":"ready.rs"}))
            .await["result"]
            .is_object(),
        "AST dispatch begins only after its host-only fence"
    );
    harness.shutdown_and_await_release(deadline).await;

    let workspace = tempdir().expect("create workspace");
    std::fs::write(workspace.path().join("ready.rs"), "pub fn ready() {}\n").unwrap();
    install_ast_extension(workspace.path());
    let hooks = selected_binary("TOWER_TEST_BIN");
    let mut daemon = spawn_daemon_at_preparation_barrier(
        &hooks,
        workspace.path(),
        "capability-catch-up-blocked",
    );
    let mut connection = wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("connect while capability catch-up is blocked");
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .unwrap();
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"readiness-independent-gates","version":"1"}}}"#,
    )
    .await
    .unwrap();
    read_line_capped(&mut connection, 1 << 20)
        .await
        .unwrap()
        .expect("initialize response");

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tower_read_file","arguments":{"path":"ready.rs"}}}"#,
    )
    .await
    .unwrap();
    let workspace_read: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("workspace-ready native read must complete"),
    )
    .unwrap();
    assert!(workspace_read["result"].is_object());

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"tower_create_file","arguments":{"path":"mutation.rs","content":"pub fn mutation() {}\n"}}}"#,
    )
    .await
    .unwrap();
    let mutation: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("workspace-ready mutation must complete"),
    )
    .unwrap();
    assert!(mutation["result"].is_object());

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#,
    )
    .await
    .unwrap();
    let blocked_list: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("tools/list remains callable"),
    )
    .unwrap();
    let blocked_names = blocked_list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert!(blocked_names.contains(&"tower_read_file"));
    assert!(
        !blocked_names
            .iter()
            .any(|name| name.starts_with("tower_ast_"))
    );
    assert!(
        !blocked_names
            .iter()
            .any(|name| name.starts_with("tower_lsp_"))
    );

    let status = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Status,
    )
    .await
    .expect("status remains callable during capability catch-up");
    assert!(matches!(
        status,
        ControlResponse::Status(ref snapshot)
            if snapshot.workspace == core_engine::adapters::daemon::CapabilityReadiness::Ready
                && !matches!(snapshot.ast, core_engine::adapters::daemon::CapabilityReadiness::Ready)
                && !matches!(snapshot.lsp, core_engine::adapters::daemon::CapabilityReadiness::Ready)
    ));

    release_preparation_barrier(&mut daemon);
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"tower_ast_get_outline","arguments":{"path":"ready.rs"}}}"#,
    )
    .await
    .unwrap();
    let ast: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("published AST tool returns its outline"),
    )
    .unwrap();
    assert_eq!(ast["result"]["content"][0]["type"], "text");
    assert!(
        ast["result"]["content"][0]["text"]
            .as_str()
            .expect("AST tool text")
            .contains("ready")
    );
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
    assert!(daemon.wait().expect("join daemon").success());
}

#[tokio::test]
async fn readiness_list_and_call_remain_coherent_during_transition() {
    let workspace = tempdir().expect("create workspace");
    std::fs::write(workspace.path().join("ready.rs"), "pub fn ready() {}\n").unwrap();
    install_ast_extension(workspace.path());
    let hooks = selected_binary("TOWER_TEST_BIN");
    let mut daemon = spawn_daemon_at_preparation_barrier(
        &hooks,
        workspace.path(),
        "capability-catch-up-blocked",
    );
    let mut connection = wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("connect during capability transition");
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .unwrap();
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"readiness-transition","version":"1"}}}"#,
    )
    .await
    .unwrap();
    read_line_capped(&mut connection, 1 << 20)
        .await
        .unwrap()
        .expect("initialize response");
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )
    .await
    .unwrap();
    let before: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("pre-transition list"),
    )
    .unwrap();
    assert!(
        before["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().starts_with("tower_ast_"))
    );

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tower_ast_get_outline","arguments":{"path":"ready.rs"}}}"#,
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            read_line_capped(&mut connection, 1 << 20)
        )
        .await
        .is_err(),
        "an exhaustive AST call must not complete while its declaration is hidden"
    );

    release_preparation_barrier(&mut daemon);
    let response: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("admitted AST call completes after publication"),
    )
    .unwrap();
    assert_eq!(response["result"]["content"][0]["type"], "text");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("AST result text")
            .contains("ready")
    );

    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#,
    )
    .await
    .unwrap();
    let after: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .unwrap()
            .expect("post-transition list"),
    )
    .unwrap();
    assert!(
        after["result"]["tools"]
            .as_array()
            .expect("tool declarations")
            .iter()
            .any(|tool| tool["name"] == "tower_ast_get_outline")
    );

    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
    assert!(daemon.wait().expect("join daemon").success());
}

#[tokio::test]
async fn failed_startup_restarts_in_same_workspace() {
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };

    let mut recovery = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Fail);
    recovery.seed_file("failed-generation.rs", "pub fn failed_generation() {}\n");
    recovery.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    recovery
        .await_milestone(StartupMilestone::PreparationCancelled, deadline)
        .await;
    let failed_generation = recovery.generation().await;
    recovery.shutdown_and_await_release(deadline).await;
    recovery
        .await_milestone(StartupMilestone::ResourcesReleased, deadline)
        .await;
    assert_eq!(
        recovery.released_resources().await,
        reconciliation_process::ReleasedResources {
            sled: true,
            watcher: true,
            socket: true,
            sidecars: true,
            ownership: true,
        },
        "failed generation must release every owned public resource before reuse"
    );
    recovery.restart().await;
    recovery
        .await_milestone(StartupMilestone::TransportBound, deadline)
        .await;
    assert!(
        recovery.generation().await > failed_generation,
        "same-workspace restart must acquire a strictly newer generation"
    );

    let workspace = tempdir().expect("create workspace");
    let hooks = selected_binary("TOWER_TEST_BIN");
    let mut daemon =
        spawn_daemon_at_preparation_barrier(&hooks, workspace.path(), "fail-reconciliation");
    wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("transport is bound before preparation failure");
    release_preparation_barrier(&mut daemon);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = core_engine::adapters::daemon::client::send_control_async(
            &GlobalOpts {
                workspace_dir: Some(workspace.path().to_path_buf()),
                extensions_dir: None,
            },
            ControlRequest::Status,
        )
        .await
        .expect("failed preparation remains observable through control status");
        if matches!(
            response,
            ControlResponse::Status(ref snapshot)
                if snapshot.lifecycle == core_engine::adapters::daemon::DaemonLifecycle::Failed {
                    failure_class: core_engine::adapters::daemon::StartupFailureClass::Reconciliation,
                }
        ) {
            break;
        }
        assert!(Instant::now() < deadline, "failure was not published");
        tokio::task::yield_now().await;
    }

    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
    assert!(daemon.wait().expect("join failed daemon").success());

    spawn_daemon_thread(workspace.path().to_path_buf(), config_with_idle(300));
    wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("same persisted workspace restarts cleanly");
    let status = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Status,
    )
    .await
    .expect("restarted daemon status");
    assert!(!matches!(
        status,
        ControlResponse::Status(ref snapshot)
            if matches!(snapshot.lifecycle, core_engine::adapters::daemon::DaemonLifecycle::Failed { .. })
    ));
    let _ = core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await;
}

#[tokio::test]
async fn shutdown_cancels_preparation_and_releases_resources() {
    use core_engine::adapters::daemon::socket::{OwnerLiveness, probe_owner};
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };

    let mut recovery = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Block);
    recovery.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    recovery
        .await_milestone(StartupMilestone::CatchUpBlocked, deadline)
        .await;
    recovery.observe_probe().await;
    recovery.shutdown_and_await_release(deadline).await;
    recovery
        .await_milestone(StartupMilestone::PreparationCancelled, deadline)
        .await;
    recovery
        .await_milestone(StartupMilestone::ResourcesReleased, deadline)
        .await;
    assert_eq!(
        recovery.released_resources().await,
        reconciliation_process::ReleasedResources {
            sled: true,
            watcher: true,
            socket: true,
            sidecars: true,
            ownership: true,
        }
    );
    // Hold a real storage handle throughout the quiet window rather than
    // reopening it repeatedly (Sled's background drop can transiently retain a lock).
    let storage = recovery.hold_released_storage(deadline).await;
    let quiet_logs = recovery.captured_logs().await;
    let quiet_until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < quiet_until {
        recovery.assert_stopped_workers_remain_absent();
        assert_eq!(
            recovery.captured_logs().await,
            quiet_logs,
            "the durable output channel must show no later worker effects"
        );
        tokio::task::yield_now().await;
    }

    storage
        .flush()
        .expect("the independently held released storage remains usable");
    drop(storage);

    let workspace = tempdir().expect("create workspace");
    let extensions = workspace.path().join(".tower/extensions");
    let sidecar = extensions.join("cleanup-sentinel");
    std::fs::create_dir_all(&sidecar).expect("create cleanup sentinel extension");
    let launched = workspace.path().join("sidecar-launched");
    let cleaned = workspace.path().join("sidecar-cleaned");
    let script = sidecar.join("sentinel.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf launched > '{}'\ntrap 'printf cleaned > \"{}\"' EXIT\nwhile IFS= read -r request; do\n  case \"$request\" in\n    *initialize*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"type\":\"Initialized\",\"data\":{{\"tools\":[],\"events\":[],\"capabilities\":[]}}}}}}' ;;\n    *shutdown*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"type\":\"Ack\"}}}}'; exit 0 ;;\n  esac\ndone\n",
            launched.display(),
            cleaned.display(),
        ),
    )
    .expect("write cleanup sentinel extension");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make cleanup sentinel executable");
    }
    std::fs::write(
        sidecar.join("extension.toml"),
        format!(
            "name = \"cleanup-sentinel\"\nversion = \"0.1.0\"\ncommand = [\"{}\"]\nactivation = \"eager\"\n[capabilities]\nrequired = []\n",
            script.display()
        ),
    )
    .expect("write cleanup sentinel manifest");
    let hooks = selected_binary("TOWER_TEST_BIN");
    let mut daemon = spawn_daemon_at_preparation_barrier(
        &hooks,
        workspace.path(),
        "capability-catch-up-blocked",
    );
    wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("early transport accepts shutdown");

    core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await
    .expect("shutdown cancels blocked preparation");
    assert!(daemon.wait().expect("joined shutdown").success());
    assert!(
        !socket_path(workspace.path()).exists(),
        "joined shutdown releases the daemon socket"
    );
    assert_eq!(probe_owner(workspace.path()).unwrap(), OwnerLiveness::Dead);
    sled::open(workspace.path().join(".tower/db")).expect("shutdown releases Sled");
    WorkspaceDaemonOwner::acquire(workspace.path()).expect("shutdown releases daemon ownership");
    assert!(
        launched.exists(),
        "the sidecar reached capability preparation"
    );
    assert!(
        cleaned.exists(),
        "joined shutdown terminates the sidecar process"
    );
    let cleanup: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(workspace.path().join(".tower/preparation-cleanup.json"))
            .expect("startup hook recorded joined resource cleanup"),
    )
    .expect("cleanup trace is JSON");
    assert_eq!(
        cleanup,
        [
            "watcher",
            "sidecars",
            "push_fanout",
            "sled",
            "socket",
            "ownership"
        ],
        "joined cancellation must release every owned resource in shutdown order"
    );
}

#[test]
fn slow_extension_protocol_examples_match_literal_json() {
    let initialize = Request::Initialize(InitParams {
        protocol_version: PROTOCOL_VERSION,
        client_info: "daemon-e2e".to_owned(),
        extension_config: Some(json!({"mode": "block"})),
    });
    assert_eq!(
        serde_json::to_value(initialize).unwrap(),
        json!({
            "type": "Initialize",
            "data": {
                "protocol_version": PROTOCOL_VERSION,
                "client_info": "daemon-e2e",
                "extension_config": {"mode": "block"}
            }
        })
    );
    assert_eq!(
        serde_json::to_value(Response::Ack).unwrap(),
        json!({"type": "Ack"})
    );
    assert_eq!(
        serde_json::to_value(Request::InvokeTool {
            name: "probe_catch_up".to_owned(),
            params: json!({"release": false}),
        })
        .unwrap(),
        json!({
            "type": "InvokeTool",
            "data": {"name": "probe_catch_up", "params": {"release": false}}
        })
    );
    assert_eq!(
        serde_json::to_value(Request::InvokeTool {
            name: "_tower_catch_up_fence".to_owned(),
            params: json!({"epoch": 7, "delivered_events": 1}),
        })
        .unwrap(),
        json!({
            "type": "InvokeTool",
            "data": {
                "name": "_tower_catch_up_fence",
                "params": {"epoch": 7, "delivered_events": 1}
            }
        })
    );

    let mut child = SlowStartChild::spawn();
    let initialized = child.initialize(1, Some(json!({"mode": "block"})));
    assert_eq!(
        initialized,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "type": "Initialized",
                "data": {
                    "tools": [
                        {
                            "name": "probe_catch_up",
                            "description": "Release or observe deterministic slow-start catch-up.",
                            "schema_json": "{\"type\":\"object\",\"properties\":{\"release\":{\"type\":\"boolean\"}},\"additionalProperties\":false}"
                        },
                        {
                            "name": "_tower_catch_up_fence",
                            "description": "Host-only catch-up ordering barrier.",
                            "schema_json": "{\"type\":\"object\",\"properties\":{\"epoch\":{\"type\":\"integer\"},\"delivered_events\":{\"type\":\"integer\"}},\"required\":[\"epoch\",\"delivered_events\"],\"additionalProperties\":false}"
                        }
                    ],
                    "events": ["event/fileIndexed", "event/fileChanged", "event/fileDeleted"],
                    "capabilities": []
                }
            }
        })
    );

    child.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "deliverEvent",
        "params": serde_json::to_value(Event::FileIndexed {
            file_id: 1,
            path: "private.rs".to_owned(),
        }).unwrap(),
    }));
    assert_eq!(
        child.recv(),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"type": "Ack"}})
    );

    child.send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "invokeTool", "params": {
            "name": "probe_catch_up", "params": {"release": false}
        }}),
    );
    assert_eq!(
        child.recv()["result"],
        json!({"type": "ToolResult", "data": {
            "processed_events": 0, "processed_ordinals": [], "released": false,
            "barrier_observed": false, "pending_fences": 0
        }})
    );

    child.send(
        json!({"jsonrpc": "2.0", "id": 4, "method": "invokeTool", "params": {
            "name": "_tower_catch_up_fence", "params": {"epoch": 7, "delivered_events": 1}
        }}),
    );
    assert_eq!(
        child.recv()["result"],
        json!({"type": "ToolResult", "data": {
            "epoch": 7, "processed_events": 0, "ready": false
        }})
    );

    child.send(
        json!({"jsonrpc": "2.0", "id": 5, "method": "invokeTool", "params": {
            "name": "probe_catch_up", "params": {}
        }}),
    );
    assert_eq!(
        child.recv()["result"],
        json!({"type": "ToolResult", "data": {
            "processed_events": 1, "processed_ordinals": [1], "released": true,
            "barrier_observed": false, "pending_fences": 1
        }})
    );

    child.send(
        json!({"jsonrpc": "2.0", "id": 6, "method": "invokeTool", "params": {
            "name": "_tower_catch_up_fence", "params": {"epoch": 7, "delivered_events": 1}
        }}),
    );
    assert_eq!(
        child.recv()["result"],
        json!({"type": "ToolResult", "data": {
            "epoch": 7, "processed_events": 1, "ready": true
        }})
    );

    let unknown_probe_field = child.invoke_tool(
        7,
        "probe_catch_up",
        json!({"release": false, "unknown": true}),
    );
    assert_eq!(
        unknown_probe_field,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "error": {"code": -32602, "message": "invalid probe_catch_up parameters"}
        })
    );

    // A release is a one-way state transition: later releases observe the same snapshot.
    assert_eq!(
        child.invoke_tool(8, "probe_catch_up", json!({}))["result"],
        json!({"type": "ToolResult", "data": {
            "processed_events": 1, "processed_ordinals": [1], "released": true,
            "barrier_observed": true, "pending_fences": 1
        }})
    );

    let mut resume = SlowStartChild::spawn();
    assert!(resume.initialize(20, Some(json!({"mode": "resume"})))["result"].is_object());
    assert_eq!(
        resume.deliver_event(
            21,
            Event::FileChanged {
                file_id: 2,
                path: "immediate.rs".to_owned()
            },
        ),
        json!({"jsonrpc": "2.0", "id": 21, "result": {"type": "Ack"}})
    );
    assert_eq!(
        resume.invoke_tool(22, "probe_catch_up", json!({"release": false}))["result"]["data"]["processed_events"],
        1,
        "resume mode applies accepted replay events without a release probe"
    );

    let mut overflow = SlowStartChild::spawn();
    assert!(overflow.initialize(30, Some(json!({"mode": "block"})))["result"].is_object());
    for ordinal in 1..=65_u64 {
        assert_eq!(
            overflow.deliver_event(
                30 + ordinal,
                Event::FileChanged {
                    file_id: ordinal,
                    path: format!("event-{ordinal}.rs")
                },
            )["result"],
            json!({"type": "Ack"})
        );
    }
    assert_eq!(
        overflow.invoke_tool(96, "probe_catch_up", json!({}))["result"]["data"],
        json!({
            "processed_events": 65,
            "processed_ordinals": (2_u64..=65).collect::<Vec<_>>(),
            "released": true,
            "barrier_observed": false,
            "pending_fences": 0
        }),
        "bounded evidence retains exactly the newest 64 ordinals while the total remains exact"
    );

    let mut malformed = SlowStartChild::spawn();
    assert!(malformed.initialize(100, Some(json!({"mode": "block"})))["result"].is_object());
    malformed.send(json!({
        "jsonrpc": "2.0", "id": 101, "method": "deliverEvent", "params": null
    }));
    assert_eq!(
        malformed.recv(),
        json!({
            "jsonrpc": "2.0", "id": 101,
            "error": {"code": -32602, "message": "invalid deliverEvent parameters"}
        })
    );
    assert_eq!(
        malformed.invoke_tool(102, "probe_catch_up", json!({}))["result"]["data"],
        json!({
            "processed_events": 0, "processed_ordinals": [], "released": true,
            "barrier_observed": false, "pending_fences": 0
        }),
        "a malformed event must not advance accepted or processed state"
    );
}

#[tokio::test]
async fn slow_extension_rejects_invalid_initialization_modes() {
    let invalid = [
        None,
        Some(json!({})),
        Some(json!({"mode": "unknown"})),
        Some(json!({"mode": "block", "unknown": true})),
    ];
    for (index, config) in invalid.into_iter().enumerate() {
        let mut child = SlowStartChild::spawn();
        let response = child.initialize(index as u64 + 1, config);
        assert_eq!(
            response,
            json!({
                "jsonrpc": "2.0",
                "id": index as u64 + 1,
                "error": {"code": -32602, "message": "invalid slow_start initialization"}
            }),
            "initialization failures expose one fixed protocol envelope"
        );
    }

    // The same strict validation must be reached through manifest discovery;
    // an invalid fixture is isolated rather than preventing the MCP daemon
    // from serving its native public surface.
    let workspace = tempdir().expect("create invalid-discovery workspace");
    install_slow_start_extension(workspace.path());
    fs::write(
        workspace.path().join(".tower/config.toml"),
        "[daemon]\nidle_timeout_secs = 300\nconnection_timeout_secs = 2\npreparation_timeout_secs = 2\n\
         [extensions]\nrequest_timeout_secs = 2\n\
         [extensions.readiness]\nslow_start = \"ast\"\n\
         [extensions.readiness_controls]\nslow_start = [\"probe_catch_up\"]\n\
         [extensions.initialize.slow_start]\nmode = \"not-a-mode\"\n",
    )
    .expect("configure invalid slow-start manifest discovery");
    let mut daemon = Command::new(selected_production_binary())
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon with discovered invalid slow-start fixture");
    let mut connection = wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .expect("invalid extension initialization remains isolated from MCP startup");
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .expect("send MCP handshake");
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"invalid-slow-start","version":"1"}}}"#,
    )
    .await
    .expect("send MCP initialize");
    let initialized: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .expect("read MCP initialize")
            .expect("MCP initialize response"),
    )
    .expect("parse MCP initialize response");
    assert!(
        initialized["result"].is_object(),
        "invalid fixture is isolated: {initialized}"
    );
    write_line(
        &mut connection,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )
    .await
    .expect("list tools after invalid fixture initialization");
    let tools: Value = serde_json::from_str(
        &read_line_capped(&mut connection, 1 << 20)
            .await
            .expect("read tools/list")
            .expect("tools/list response"),
    )
    .expect("parse tools/list response");
    assert!(
        !tools["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .any(|tool| { tool["name"] == "tower_slow_start_probe_catch_up" }),
        "an extension rejected during manifest discovery cannot publish its control: {tools}"
    );
    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let _ =
        core_engine::adapters::daemon::client::send_control_async(&opts, ControlRequest::Shutdown)
            .await;
    let _ = daemon.wait();
}

#[tokio::test]
async fn slow_extension_fail_and_crash_modes_are_deterministic() {
    let _slow_start_guard = slow_start_process_lock().lock().await;
    let event = serde_json::to_value(Event::FileChanged {
        file_id: 9,
        path: "producer-secret.rs".to_owned(),
    })
    .unwrap();

    let mut failing = SlowStartChild::spawn();
    assert!(failing.initialize(1, Some(json!({"mode": "fail"})))["result"].is_object());
    failing.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "deliverEvent", "params": event
    }));
    let failure = failing.recv();
    assert_eq!(
        failure,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {"code": -32000, "message": "planned replay failure"}
        })
    );
    assert!(!failure.to_string().contains("producer-secret"));

    let mut crashing = SlowStartChild::spawn();
    assert!(crashing.initialize(3, Some(json!({"mode": "crash"})))["result"].is_object());
    crashing.send(json!({
        "jsonrpc": "2.0", "id": 4, "method": "deliverEvent", "params": {
            "type": "FileDeleted", "path": "producer-secret.rs"
        }
    }));
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = crashing.child.try_wait().expect("poll crashing fixture") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "crash mode did not exit within its operation budget"
        );
        std::thread::yield_now();
    };
    assert!(!status.success());

    // These are discovery scenarios, not direct-child substitutes: each
    // declared mode is forwarded by the daemon to the manifest-discovered
    // fixture. The direct checks above pin the fixture's exact wire behavior.
    for mode in ["resume", "fail", "crash"] {
        let workspace = tempdir().expect("create mode-discovery workspace");
        fs::write(
            workspace.path().join("replayed.rs"),
            "pub fn replayed() {}\n",
        )
        .expect("seed replay event");
        install_slow_start_extension(workspace.path());
        fs::write(
            workspace.path().join(".tower/config.toml"),
            format!(
                "[daemon]\nidle_timeout_secs = 300\nconnection_timeout_secs = 2\npreparation_timeout_secs = 2\n\
                 [extensions]\nrequest_timeout_secs = 2\n\
                 [extensions.readiness]\nslow_start = \"ast\"\n\
                 [extensions.readiness_controls]\nslow_start = [\"probe_catch_up\"]\n\
                 [extensions.initialize.slow_start]\nmode = \"{mode}\"\n"
            ),
        )
        .expect("configure discovered fixture mode");
        let mut daemon = Command::new(selected_production_binary())
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(workspace.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", workspace.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon with discovered slow-start mode");
        let mut connection =
            wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
                .await
                .expect("manifest-discovered extension does not block MCP startup");
        connection
            .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
            .await
            .expect("send MCP handshake");
        write_line(
            &mut connection,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"slow-mode-discovery","version":"1"}}}"#,
        )
        .await
        .expect("send MCP initialize");
        let initialized: Value = serde_json::from_str(
            &read_line_capped(&mut connection, 1 << 20)
                .await
                .expect("read MCP initialize")
                .expect("MCP initialize response"),
        )
        .expect("parse MCP initialize response");
        assert!(
            initialized["result"].is_object(),
            "mode {mode} is delivered through discovery: {initialized}"
        );

        let opts = GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = core_engine::adapters::daemon::client::send_control_async(
                &opts,
                ControlRequest::Status,
            )
            .await
            .expect("read mode-specific readiness");
            let observed = match status {
                ControlResponse::Status(snapshot) if mode == "resume" => {
                    snapshot.ast == core_engine::adapters::daemon::CapabilityReadiness::Ready
                }
                ControlResponse::Status(snapshot) => matches!(
                    snapshot.ast,
                    core_engine::adapters::daemon::CapabilityReadiness::Failed { .. }
                        | core_engine::adapters::daemon::CapabilityReadiness::Unavailable
                ),
                _ => false,
            };
            if observed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mode {mode} did not produce its public readiness outcome"
            );
            tokio::task::yield_now().await;
        }
        let _ = core_engine::adapters::daemon::client::send_control_async(
            &opts,
            ControlRequest::Shutdown,
        )
        .await;
        let _ = daemon.wait();
    }
}

#[tokio::test]
async fn changes_during_catch_up_converge_before_ready() {
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };
    let mut harness = ReconciliationProcessHarness::new_with_files(
        SlowStartInitializeConfiguration::Block,
        &[
            ("replayed-a.rs", "pub fn replayed_a() {}\n"),
            ("replayed-b.rs", "pub fn replayed_b() {}\n"),
            ("replayed-c.rs", "pub fn replayed_c() {}\n"),
        ],
    );
    harness.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    harness
        .await_milestone(StartupMilestone::CatchUpBlocked, deadline)
        .await;
    assert!(!harness.observe_probe().await.released);

    // Five watcher transitions; the indexed marker fences modify before delete.
    let workspace = harness.workspace();
    fs::write(
        workspace.join("watcher-live.rs"),
        "pub fn watcher_created() {}\n",
    )
    .unwrap();
    harness
        .await_indexed_path("watcher-live.rs", true, deadline)
        .await;
    fs::write(
        workspace.join("watcher-live.rs"),
        "pub fn watcher_changed() {}\n",
    )
    .unwrap();
    fs::write(
        workspace.join("watcher-marker.rs"),
        "// watcher queue marker\n",
    )
    .unwrap();
    harness
        .await_indexed_path("watcher-marker.rs", true, deadline)
        .await;
    fs::remove_file(workspace.join("watcher-live.rs")).unwrap();
    harness
        .await_indexed_path("watcher-live.rs", false, deadline)
        .await;
    fs::remove_file(workspace.join("watcher-marker.rs")).unwrap();
    harness
        .await_indexed_path("watcher-marker.rs", false, deadline)
        .await;
    for (id, name, arguments) in [
        (
            101,
            "tower_create_file",
            json!({"path":"mcp-live.rs","content":"pub fn mcp_created() {}\n"}),
        ),
        (
            102,
            "tower_edit_range",
            json!({"path":"mcp-live.rs","start_byte":7,"end_byte":18,"replacement":"mcp_changed"}),
        ),
        (103, "tower_delete_file", json!({"path":"mcp-live.rs"})),
    ] {
        let response = harness.call_tool(id, name, arguments).await;
        assert!(
            response.get("error").is_none(),
            "mutation failed: {response}"
        );
        assert!(
            response["result"].is_object(),
            "mutation response: {response}"
        );
        assert_ne!(
            response["result"]["isError"], true,
            "mutation failed: {response}"
        );
    }
    let release = harness.release_probe().await;
    assert!(release.released);
    assert!(
        !release.barrier_observed,
        "release returns before the future ready fence"
    );
    harness
        .await_milestone(StartupMilestone::CatchUpReleased, deadline)
        .await;
    let final_probe = harness.observe_probe().await;
    // 3 snapshot entries + 5 watcher transitions + 3 successful MCP commits.
    assert_eq!(final_probe.processed_events, 11);
    assert_eq!(final_probe.processed_ordinals, (1..=11).collect::<Vec<_>>());
    assert!(final_probe.barrier_observed);
    assert_eq!(
        harness.status().await.ast,
        core_engine::adapters::daemon::CapabilityReadiness::Ready
    );
    harness.shutdown_and_await_release(deadline).await;
}

#[tokio::test]
async fn startup_outputs_exclude_confidential_producer_values() {
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };

    let path_sentinel = "PRIVATE_STARTUP_PATH.rs";
    let content_sentinel = "PRIVATE_STARTUP_CONTENT";
    let backend_sentinel = "planned replay failure";
    let mut privacy = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Fail);
    privacy.seed_file(
        path_sentinel,
        &format!("pub const SECRET: &str = {content_sentinel:?};"),
    );
    privacy.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let failed_response = privacy
        .call_tool(
            90,
            "tower_slow_start_probe_catch_up",
            json!({"release": true}),
        )
        .await;
    privacy
        .await_milestone(StartupMilestone::PreparationCancelled, deadline)
        .await;
    let status = serde_json::to_string(&privacy.status().await).expect("encode public status");
    let logs = privacy.captured_logs().await;
    for raw_output in [
        status,
        failed_response.to_string(),
        logs.daemon,
        logs.sidecars,
    ] {
        for sentinel in [path_sentinel, content_sentinel, backend_sentinel] {
            assert!(
                !raw_output.contains(sentinel),
                "public startup output leaked confidential producer value {sentinel}: {raw_output}"
            );
        }
    }

    let cases = [
        ("block", 2_u64, Value::Null),
        ("fail", 2_u64, json!("extension_protocol")),
    ];

    for (mode, expected_event_count, expected_failure_class) in cases {
        let workspace = tempdir().expect("create instrumented startup workspace");
        let path_sentinel = "PRIVATE_STARTUP_PATH.rs";
        let content_sentinel = "PRIVATE_STARTUP_CONTENT";
        let backend_sentinel = "planned replay failure";
        fs::write(
            workspace.path().join(path_sentinel),
            format!("pub const SECRET: &str = \"{content_sentinel}\";\n"),
        )
        .expect("seed confidential replay input");
        fs::write(workspace.path().join("second.rs"), "pub fn second() {}\n")
            .expect("seed second replay input");
        install_slow_start_extension(workspace.path());
        fs::write(
            workspace.path().join(".tower/config.toml"),
            format!(
                "[daemon]\nidle_timeout_secs = 300\nconnection_timeout_secs = 2\npreparation_timeout_secs = 2\n\
                 [extensions]\nrequest_timeout_secs = 2\n\
                 [extensions.readiness]\nslow_start = \"ast\"\n\
                 [extensions.readiness_controls]\nslow_start = [\"probe_catch_up\"]\n\
                 [extensions.initialize.slow_start]\nmode = \"{mode}\"\n"
            ),
        )
        .expect("configure instrumented slow-start fixture");

        let mut daemon = Command::new(selected_production_binary())
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(workspace.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", workspace.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn instrumented production daemon");
        wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
            .await
            .expect("instrumented daemon binds its public socket");
        let opts = GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: None,
        };

        if mode == "block" {
            let mut connection =
                wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
                    .await
                    .expect("connect MCP client while startup replay is active");
            connection
                .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
                .await
                .expect("send MCP handshake");
            write_line(
                &mut connection,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"startup-instrumentation","version":"1"}}}"#,
            )
            .await
            .expect("send MCP initialize");
            read_line_capped(&mut connection, 1 << 20)
                .await
                .expect("read MCP initialize")
                .expect("MCP initialize response");

            let probe = |id| {
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {
                        "name": "tower_slow_start_probe_catch_up",
                        "arguments": {"release": false}
                    }
                })
                .to_string()
            };
            let probe_deadline = Instant::now() + Duration::from_secs(3);
            let mut probe_id = 2;
            loop {
                write_line(&mut connection, &probe(probe_id))
                    .await
                    .expect("request deterministic replay observation");
                let response: Value = serde_json::from_str(
                    &read_line_capped(&mut connection, 1 << 20)
                        .await
                        .expect("read replay observation")
                        .expect("replay observation response"),
                )
                .expect("parse replay observation");
                let text = response["result"]["content"][0]["text"]
                    .as_str()
                    .expect("replay observation data");
                if let Ok(observed) = serde_json::from_str::<Value>(text)
                    && observed["pending_fences"].as_u64().unwrap_or(0) >= 1
                {
                    assert_eq!(observed["released"], false);
                    break;
                }
                assert!(
                    Instant::now() < probe_deadline,
                    "fixture did not confirm an active replay fence before release"
                );
                probe_id += 1;
                tokio::task::yield_now().await;
            }

            let active_raw_status = read_raw_control_status(workspace.path()).await;
            for sentinel in [path_sentinel, content_sentinel, backend_sentinel] {
                assert!(
                    !active_raw_status.contains(sentinel),
                    "active public startup status leaked confidential producer data"
                );
            }
            let active_status: Value =
                serde_json::from_str(&active_raw_status).expect("parse active raw status JSON");
            let active_instrumentation = &active_status["instrumentation"];
            let active_phases = active_instrumentation["phases"]
                .as_array()
                .expect("active completed phase projection");
            assert!(
                active_phases
                    .iter()
                    .any(|timing| timing["phase"] == "extensions_discovering"),
                "completed phases remain visible while replay is active"
            );
            assert_eq!(
                active_instrumentation["extension_replays"],
                json!([]),
                "only the active replay record is omitted until finalized"
            );

            probe_id += 1;
            write_line(
                &mut connection,
                &json!({
                    "jsonrpc": "2.0", "id": probe_id, "method": "tools/call",
                    "params": {
                        "name": "tower_slow_start_probe_catch_up",
                        "arguments": {"release": true}
                    }
                })
                .to_string(),
            )
            .await
            .expect("release active replay");
            read_line_capped(&mut connection, 1 << 20)
                .await
                .expect("read replay release")
                .expect("replay release response");
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let expected_phase_count = if mode == "block" { 7 } else { 6 };
        let (raw_status, snapshot) = loop {
            let raw_status = read_raw_control_status(workspace.path()).await;
            for sentinel in [path_sentinel, content_sentinel, backend_sentinel] {
                assert!(
                    !raw_status.contains(sentinel),
                    "public startup status leaked confidential producer data"
                );
            }
            let ControlResponse::Status(snapshot) = serde_json::from_str(&raw_status)
                .expect("decode raw status response after confidentiality checks")
            else {
                panic!("status command returned a non-status response");
            };
            if !snapshot.instrumentation.extension_replays.is_empty()
                && snapshot.instrumentation.phases.len() == expected_phase_count
            {
                break (raw_status, snapshot);
            }
            assert!(
                Instant::now() < deadline,
                "startup replay metrics were not finalized for {mode}"
            );
            tokio::task::yield_now().await;
        };

        let expected_phases = if mode == "block" {
            vec![
                "ownership_acquired",
                "transport_bound",
                "storage_opening",
                "workspace_reconciling",
                "watcher_catching_up",
                "extensions_discovering",
                "capabilities_catching_up",
            ]
        } else {
            vec![
                "ownership_acquired",
                "transport_bound",
                "storage_opening",
                "workspace_reconciling",
                "watcher_catching_up",
                "extensions_discovering",
            ]
        };
        let phase_json = expected_phases
            .iter()
            .zip(&snapshot.instrumentation.phases)
            .map(|(phase, timing)| {
                json!({
                    "phase": phase,
                    "duration_ms": timing.duration_ms,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            snapshot.instrumentation.phases.len(),
            expected_phases.len(),
            "every completed authoritative phase is projected exactly once"
        );

        let replay = &snapshot.instrumentation.extension_replays[0];
        let expected_instrumentation = json!({
            "phases": phase_json,
            "extension_replays": [{
                "extension_id": "slow_start",
                "event_count": expected_event_count,
                "duration_ms": replay.duration_ms,
                "failure_class": expected_failure_class,
            }],
        });
        let raw_status_value: Value =
            serde_json::from_str(&raw_status).expect("parse final raw status JSON");
        let actual_instrumentation = raw_status_value["instrumentation"].clone();
        assert_eq!(actual_instrumentation, expected_instrumentation);

        let _ = core_engine::adapters::daemon::client::send_control_async(
            &opts,
            ControlRequest::Shutdown,
        )
        .await;
        let _ = daemon.wait();
    }
}

#[tokio::test]
#[ignore = "requires explicitly selected real-disk TMPDIR"]
async fn slow_extension_replay_metrics_are_reproducible_on_real_disk() {
    use reconciliation_process::{
        ReconciliationProcessHarness, ReleasedResources, SlowStartInitializeConfiguration,
        StartupEvidenceArtifact, StartupEvidenceRun,
    };

    const INDEXED_FILE_COUNT: u64 = 1_396;
    const PATH_SENTINEL: &str = "PRIVATE_REAL_DISK_PATH";
    const CONTENT_SENTINEL: &str = "PRIVATE_REAL_DISK_CONTENT";
    const BACKEND_SENTINEL: &str = "PRIVATE_REAL_DISK_BACKEND_TEXT";

    let disk = ReconciliationProcessHarness::verified_real_disk_from_tmpdir();
    assert!(disk.tmpdir.is_absolute());
    assert_ne!(disk.target, Path::new("/tmp"));
    assert!(!matches!(disk.filesystem.as_str(), "tmpfs" | "ramfs"));

    let binaries = ReconciliationProcessHarness::binary_provenance();
    for (path, digest) in [
        (&binaries.tower_path, &binaries.tower_sha256),
        (
            &binaries.slow_start_extension_path,
            &binaries.slow_start_extension_sha256,
        ),
    ] {
        assert!(path.is_absolute());
        assert_eq!(
            path,
            &path.canonicalize().expect("canonical evidence binary")
        );
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    assert_eq!(
        binaries.tower_path.parent(),
        binaries.slow_start_extension_path.parent(),
        "both binaries must come from the same current-checkout target directory"
    );

    let mut harness = ReconciliationProcessHarness::new_on_verified_disk(
        &disk,
        binaries.clone(),
        SlowStartInitializeConfiguration::Block,
    );
    for ordinal in 0..INDEXED_FILE_COUNT {
        let path = if ordinal == 0 {
            format!("{PATH_SENTINEL}-{ordinal:04}.rs")
        } else {
            format!("indexed-{ordinal:04}.rs")
        };
        harness.seed_file(
            &path,
            &format!(
                "pub const ITEM_{ordinal}: (&str, &str) = ({CONTENT_SENTINEL:?}, {BACKEND_SENTINEL:?});\n"
            ),
        );
    }

    let mut runs: Vec<StartupEvidenceRun> = Vec::with_capacity(3);
    let mut logs_by_run = Vec::with_capacity(3);
    for expected_run in 0..3 {
        assert_eq!(
            ReconciliationProcessHarness::binary_provenance(),
            binaries,
            "binary paths and digests must remain stable across evidence runs"
        );
        let started = Instant::now();
        harness.start_generation().await;
        let reindex = harness
            .call_tool(20_000 + expected_run, "tower_reindex", json!({}))
            .await;
        assert!(
            reindex.get("result").is_some(),
            "public full reindex must complete before measuring run {expected_run}: {reindex}"
        );
        let indexed = loop {
            let status = harness.status().await;
            if status.indexed_files == INDEXED_FILE_COUNT as usize {
                break status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "public status did not reach 1,396 indexed files before the bounded deadline: {status:?}"
            );
            tokio::task::yield_now().await;
        };
        assert_eq!(
            indexed.indexed_files, INDEXED_FILE_COUNT as usize,
            "public status must report exactly the 1,396 generated regular files before run {expected_run}"
        );

        let blocked = loop {
            let probe = harness.observe_probe().await;
            if probe.pending_fences > 0 {
                break probe;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the internal catch-up fence was not published before the bounded deadline"
            );
            tokio::task::yield_now().await;
        };
        assert!(
            !blocked.released,
            "catch-up must still be blocked before release"
        );
        assert!(
            blocked.pending_fences > 0,
            "the sidecar's internal catch-up fence must be observable through the public probe"
        );
        assert_eq!(
            blocked.processed_events, 0,
            "no replay event may be processed before the nonblocking release"
        );
        let released = harness.release_probe().await;
        assert!(released.released, "public probe must release catch-up");
        harness
            .await_milestone(
                reconciliation_process::StartupMilestone::CatchUpReleased,
                Instant::now() + Duration::from_secs(5),
            )
            .await;
        let after_fence = harness.observe_probe().await;
        assert!(
            after_fence.barrier_observed,
            "internal fence must complete after release"
        );
        assert!(
            after_fence.pending_fences >= blocked.pending_fences,
            "pending attempts may advance until the release command is processed"
        );
        assert_eq!(
            harness.observe_probe().await.pending_fences,
            after_fence.pending_fences,
            "no pending attempt may follow public readiness"
        );
        let ready = harness.status().await;
        assert_eq!(
            ready.lifecycle,
            core_engine::adapters::daemon::DaemonLifecycle::Ready,
            "daemon readiness must follow the observed fence"
        );
        assert_eq!(
            ready.ast,
            core_engine::adapters::daemon::CapabilityReadiness::Ready,
            "the configured AST capability must be ready only after the fence"
        );
        let catching_up = ready
            .instrumentation
            .phases
            .iter()
            .find(|phase| {
                phase.phase == core_engine::adapters::daemon::StartupPhase::CapabilitiesCatchingUp
            })
            .expect("public status must include completed capabilities-catching-up timing");
        let run = StartupEvidenceRun {
            connection_deadline_ms: 2_000,
            initialize_elapsed_ms: harness.initialize_elapsed_ms(),
            catch_up_release_elapsed_ms: harness
                .catch_up_release_elapsed_ms()
                .expect("catch-up release milestone"),
            generation: ready.generation,
            capabilities_catching_up_ms: catching_up.duration_ms,
            instrumentation: ready.instrumentation,
        };
        assert!(run.initialize_elapsed_ms < run.connection_deadline_ms);
        assert!(run.initialize_elapsed_ms <= run.catch_up_release_elapsed_ms);

        let matching_replays = run
            .instrumentation
            .extension_replays
            .iter()
            .filter(|replay| replay.extension_id.as_str() == "slow_start")
            .collect::<Vec<_>>();
        assert_eq!(matching_replays.len(), 1);
        assert_eq!(matching_replays[0].event_count, INDEXED_FILE_COUNT);
        assert!(
            serde_json::to_value(matching_replays[0])
                .expect("serialize replay evidence")
                .get("duration_ms")
                .is_some()
        );
        assert!(matching_replays[0].failure_class.is_none());
        assert!(run.instrumentation.phases.iter().any(|phase| phase.phase
            == core_engine::adapters::daemon::StartupPhase::CapabilitiesCatchingUp));

        // Send the privacy sentinel through the real sidecar protocol boundary,
        // rather than merely putting it in an indexed file.  Its invalid-field
        // response is intentionally public and must not leak the field name.
        let sidecar_boundary = harness
            .call_tool(
                30_000 + expected_run,
                "tower_slow_start_probe_catch_up",
                json!({BACKEND_SENTINEL: true}),
            )
            .await;
        assert!(
            !sidecar_boundary.to_string().contains(BACKEND_SENTINEL),
            "sidecar output must not reflect private backend input"
        );

        harness
            .shutdown_and_await_release(Instant::now() + Duration::from_secs(5))
            .await;
        let logs = harness.captured_logs().await;
        logs_by_run.push(logs);
        assert_eq!(
            harness.released_resources().await,
            ReleasedResources {
                sled: true,
                watcher: true,
                socket: true,
                sidecars: true,
                ownership: true,
            },
            "cold-start run {expected_run} must fully release before reacquisition"
        );
        if let Some(previous) = runs.last() {
            assert!(
                run.generation > previous.generation,
                "same-workspace cold starts require increasing generations"
            );
        }
        runs.push(run);
        harness.prepare_next_cold_start();
    }
    let runs: [_; 3] = runs.try_into().expect("exactly three raw evidence runs");
    let artifact_path = harness.write_startup_evidence(&StartupEvidenceArtifact {
        disk: disk.clone(),
        binaries: binaries.clone(),
        runs: runs.clone(),
    });
    let evidence_root = artifact_path
        .parent()
        .expect("evidence artifact has a parent")
        .canonicalize()
        .expect("canonical evidence directory");
    let workspace_evidence_root = harness.evidence_root();
    assert!(
        evidence_root.starts_with(&workspace_evidence_root),
        "evidence artifact must be contained by this harness workspace's .tower/perf-evidence: {}",
        evidence_root.display()
    );
    let artifact = fs::read_to_string(&artifact_path).expect("read startup evidence artifact");
    assert!(
        artifact.len() <= 64 * 1024,
        "evidence artifact must be bounded"
    );
    for provenance in [
        disk.source,
        disk.target.display().to_string(),
        disk.filesystem,
        binaries.tower_path.display().to_string(),
        binaries.tower_sha256,
        binaries.slow_start_extension_path.display().to_string(),
        binaries.slow_start_extension_sha256,
    ] {
        assert!(artifact.contains(&provenance));
    }
    let artifact_json: Value = serde_json::from_str(&artifact)
        .expect("evidence artifact must be structurally parseable JSON");
    let raw_runs = artifact_json["raw_runs"]
        .as_array()
        .expect("artifact must expose raw_runs");
    assert_eq!(
        raw_runs.len(),
        3,
        "artifact must contain exactly three raw records"
    );
    for (ordinal, (record, run)) in raw_runs.iter().zip(runs.iter()).enumerate() {
        let replay = run
            .instrumentation
            .extension_replays
            .iter()
            .find(|replay| replay.extension_id.as_str() == "slow_start")
            .expect("selected slow-start replay");
        assert_eq!(record["ordinal"], ordinal + 1);
        assert_eq!(record["generation"], run.generation);
        assert_eq!(record["connection_deadline_ms"], run.connection_deadline_ms);
        assert_eq!(record["initialize_elapsed_ms"], run.initialize_elapsed_ms);
        assert_eq!(
            record["catch_up_release_elapsed_ms"],
            run.catch_up_release_elapsed_ms
        );
        assert_eq!(
            record["capabilities_catching_up_ms"],
            run.capabilities_catching_up_ms
        );
        assert_eq!(record["event_count"], replay.event_count);
        assert_eq!(record["duration_ms"], replay.duration_ms);
    }
    for sentinel in [PATH_SENTINEL, CONTENT_SENTINEL, BACKEND_SENTINEL] {
        assert!(!artifact.contains(sentinel));
    }

    assert_eq!(logs_by_run.len(), 3, "capture logs for each cold start");
    for logs in logs_by_run {
        assert_eq!(
            logs.sidecars, logs.daemon,
            "sidecar stderr is inherited into and captured by the daemon stderr aggregate"
        );
        for output in [logs.daemon, logs.sidecars] {
            for sentinel in [PATH_SENTINEL, CONTENT_SENTINEL, BACKEND_SENTINEL] {
                assert!(!output.contains(sentinel));
            }
        }
    }
    let retained = workspace_root().join(".tower/perf-evidence/B001-startup.json");
    fs::create_dir_all(retained.parent().unwrap()).expect("create retained evidence directory");
    fs::write(&retained, &artifact).expect("retain validated three-run startup evidence");
    println!("validated startup evidence: {}", retained.display());
}

#[test]
fn startup_evidence_validators_reject_each_untrusted_disk_and_binary_input() {
    use reconciliation_process::{
        BinaryCandidate, BinaryProvenanceError, RealDiskCandidate, RealDiskValidationError,
        validate_binary_provenance, validate_real_disk_candidate,
    };

    let valid_disk = || RealDiskCandidate {
        tmpdir: PathBuf::from("/real-disk/evidence"),
        source: "/dev/nvme0n1p2".to_owned(),
        target: PathBuf::from("/real-disk"),
        filesystem: "ext4".to_owned(),
        writable: true,
    };
    assert_eq!(
        validate_real_disk_candidate(RealDiskCandidate {
            tmpdir: PathBuf::from("relative"),
            ..valid_disk()
        }),
        Err(RealDiskValidationError::RelativeTmpdir)
    );
    assert_eq!(
        validate_real_disk_candidate(RealDiskCandidate {
            target: PathBuf::from("/tmp"),
            ..valid_disk()
        }),
        Err(RealDiskValidationError::TmpDirectory)
    );
    for filesystem in ["tmpfs", "ramfs"] {
        assert_eq!(
            validate_real_disk_candidate(RealDiskCandidate {
                filesystem: filesystem.to_owned(),
                ..valid_disk()
            }),
            Err(RealDiskValidationError::MemoryFilesystem(
                filesystem.to_owned()
            ))
        );
    }
    assert_eq!(
        validate_real_disk_candidate(RealDiskCandidate {
            writable: false,
            ..valid_disk()
        }),
        Err(RealDiskValidationError::Unwritable(PathBuf::from(
            "/real-disk/evidence"
        )))
    );

    let valid_binary = || BinaryCandidate {
        tower_path: PathBuf::from("/checkout/target/debug/tower"),
        tower_sha256: "a".repeat(64),
        slow_start_extension_path: PathBuf::from("/checkout/target/debug/slow_start_extension"),
        slow_start_extension_sha256: "b".repeat(64),
        target_dir: PathBuf::from("/checkout/target/debug"),
    };
    let expected = valid_binary();
    assert_eq!(
        validate_binary_provenance(
            &expected,
            BinaryCandidate {
                tower_sha256: "c".repeat(64),
                ..valid_binary()
            },
        ),
        Err(BinaryProvenanceError::DigestChanged { binary: "tower" })
    );
    assert_eq!(
        validate_binary_provenance(
            &expected,
            BinaryCandidate {
                slow_start_extension_sha256: "d".repeat(64),
                ..valid_binary()
            },
        ),
        Err(BinaryProvenanceError::DigestChanged {
            binary: "slow_start_extension"
        })
    );
    assert_eq!(
        validate_binary_provenance(
            &expected,
            BinaryCandidate {
                tower_path: PathBuf::from("/foreign/target/debug/tower"),
                ..valid_binary()
            },
        ),
        Err(BinaryProvenanceError::ForeignTargetDirectory(
            PathBuf::from("/foreign/target/debug/tower")
        ))
    );
}

// Feature: B001
#[tokio::test]
async fn control_call_before_extension_discovery_does_not_wait_for_catch_up() {
    struct DaemonGuard(Child);
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("ready.rs"), "pub fn ready() {}\n").unwrap();
    install_slow_start_extension(workspace.path());
    let hooks = selected_binary("TOWER_TEST_BIN");
    let mut daemon = DaemonGuard(spawn_daemon_at_preparation_barrier_with_config(
        &hooks,
        workspace.path(),
        "before-extension-discovery",
        "[extensions.readiness]\nslow_start = \"ast\"\n[extensions.readiness_controls]\nslow_start = [\"probe_catch_up\"]\n[extensions.initialize.slow_start]\nmode = \"block\"\n",
    ));
    let mut connection = wait_for_socket(&socket_path(workspace.path()), Duration::from_secs(5))
        .await
        .unwrap();
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .unwrap();
    write_line(&mut connection, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"early-control","version":"1"}}}"#).await.unwrap();
    read_line_capped(&mut connection, 1 << 20)
        .await
        .unwrap()
        .unwrap();
    let request = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tower_slow_start_probe_catch_up","arguments":{"release":false}}}"#;
    write_line(&mut connection, request).await.unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        read_line_capped(&mut connection, 1 << 20),
    )
    .await
    .expect("undeclared tool must not wait for discovery")
    .unwrap()
    .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tool not found")
    );
    release_preparation_barrier(&mut daemon.0);
    // Retry only after a prompt NotFound: initialized controls remain usable
    // while exhaustive tools still wait for catch-up.
    let deadline = Instant::now() + Duration::from_secs(3);
    let response = loop {
        write_line(&mut connection, request).await.unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            read_line_capped(&mut connection, 1 << 20),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        if response["result"]["isError"] == false {
            break response;
        }
        assert!(Instant::now() < deadline, "{response}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(response["result"]["isError"], false, "{response}");
    let probe: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(probe["released"], false);
    assert_eq!(probe["barrier_observed"], false);
}

#[tokio::test]
async fn slow_extension_probe_resumes_blocked_replay_after_initialization() {
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };
    let mut harness = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Block);
    harness.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    harness
        .await_milestone(StartupMilestone::CatchUpBlocked, deadline)
        .await;
    let blocked = harness.observe_probe().await;
    assert!(!blocked.released);
    assert!(blocked.pending_fences > 0);
    assert_eq!(
        harness.status().await.ast,
        core_engine::adapters::daemon::CapabilityReadiness::Preparing {
            phase: core_engine::adapters::daemon::StartupPhase::CapabilitiesCatchingUp,
        }
    );
    let release = harness.release_probe().await;
    assert!(release.released);
    assert!(!release.barrier_observed);
    harness
        .await_milestone(StartupMilestone::CatchUpReleased, deadline)
        .await;
    let final_probe = harness.observe_probe().await;
    assert_eq!(final_probe.processed_events, 1);
    assert_eq!(final_probe.processed_ordinals, vec![1]);
    assert!(final_probe.barrier_observed);
    harness.shutdown_and_await_release(deadline).await;
}
#[tokio::test]
async fn slow_extension_pending_fence_expires_as_extension_timeout() {
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };
    let mut harness = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Block);
    harness.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    harness
        .await_milestone(StartupMilestone::CatchUpBlocked, deadline)
        .await;
    loop {
        let status = harness.status().await;
        if let core_engine::adapters::daemon::CapabilityReadiness::Failed { failure_class } =
            status.ast
        {
            assert_eq!(
                failure_class,
                core_engine::adapters::daemon::StartupFailureClass::ExtensionTimeout
            );
            assert_eq!(
                status.workspace,
                core_engine::adapters::daemon::CapabilityReadiness::Ready
            );
            assert_eq!(
                status.instrumentation.extension_replays[0].failure_class,
                Some(failure_class)
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "valid pending fence never expired"
        );
        tokio::task::yield_now().await;
    }
    harness.shutdown_and_await_release(deadline).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_failure_is_observable_and_restartable() {
    let workspace = tempdir().unwrap();
    fs::create_dir_all(workspace.path().join(".tower")).unwrap();
    // Deterministic storage failure without hooks or permissions assumptions.
    fs::write(workspace.path().join(".tower/db"), "not a directory").unwrap();
    let root = workspace.path().to_path_buf();
    let mut config = config_with_idle(300);
    config.daemon.connection_timeout_secs = 1;
    let daemon = tokio::task::spawn_blocking(move || {
        run_daemon(
            &GlobalOpts {
                workspace_dir: Some(root),
                extensions_dir: None,
            },
            config,
            false,
        )
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    let raw = loop {
        let raw = read_raw_control_status(workspace.path()).await;
        if raw.contains("failed") {
            break raw;
        }
        assert!(Instant::now() < deadline, "{raw}");
        tokio::task::yield_now().await;
    };
    // Even an open control connection cannot retain fatal resources forever.
    let mut control = UnixStream::connect(socket_path(workspace.path()))
        .await
        .unwrap();
    control
        .write_all(Handshake::new(ClientRole::Control).to_line().as_bytes())
        .await
        .unwrap();
    assert!(raw.contains("failed"), "{raw}");
    assert!(
        tokio::time::timeout(Duration::from_secs(3), daemon)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(!socket_path(workspace.path()).exists());
    let owner = WorkspaceDaemonOwner::acquire(workspace.path())
        .expect("failure released lifetime ownership");
    drop(owner);
    drop(control);
    fs::remove_file(workspace.path().join(".tower/db")).unwrap();
    let root = workspace.path().to_path_buf();
    spawn_daemon_thread(root, config_with_idle(1));
    let status = read_raw_control_status(workspace.path()).await;
    assert!(!status.is_empty(), "fresh generation serves control");
    // Explicit shutdown avoids a detached daemon outliving its test workspace.
    let mut connection = UnixStream::connect(socket_path(workspace.path()))
        .await
        .unwrap();
    connection
        .write_all(Handshake::new(ClientRole::Control).to_line().as_bytes())
        .await
        .unwrap();
    write_line(
        &mut connection,
        &serde_json::to_string(&ControlRequest::Shutdown).unwrap(),
    )
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket_path(workspace.path()).exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn rename_optional_lsp_fallback_does_not_wait_for_unrelated_ast_catch_up() {
    use reconciliation_process::{
        ReconciliationProcessHarness, SlowStartInitializeConfiguration, StartupMilestone,
    };
    let mut harness = ReconciliationProcessHarness::new(SlowStartInitializeConfiguration::Block);
    harness.start_generation().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    harness
        .await_milestone(StartupMilestone::CatchUpBlocked, deadline)
        .await;
    let status = harness.status().await;
    assert_eq!(
        status.lsp,
        core_engine::adapters::daemon::CapabilityReadiness::Unavailable
    );
    assert!(matches!(
        status.ast,
        core_engine::adapters::daemon::CapabilityReadiness::Preparing { .. }
    ));
    let response = tokio::time::timeout(Duration::from_secs(1), harness.call_tool(91, "tower_rename_path", json!({"source":"ready.rs","destination":"renamed.rs","semantic_policy":"best_effort","dry_run":true}))).await.expect("absent LSP must not wait for AST catch-up");
    assert_eq!(response["result"]["isError"], false, "{response}");
    harness.shutdown_and_await_release(deadline).await;
}

#[tokio::test]
async fn tower_lsp_diagnostics_returns_fixture_results_through_the_daemon() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempdir().expect("create fixture diagnostics workspace");
    let tower_dir = workspace.path().join(".tower");
    let source_dir = workspace.path().join("src");
    fs::create_dir_all(&tower_dir).expect("create tower directory");
    fs::create_dir_all(&source_dir).expect("create source directory");
    fs::write(source_dir.join("valid.rs"), "fn valid() {}\n").expect("write valid fixture");
    fs::write(source_dir.join("invalid.rs"), "fixture_invalid\n").expect("write invalid fixture");
    fs::write(source_dir.join("timeout.rs"), "fixture_invalid\n").expect("write timeout fixture");

    let sidecar_pid_log = tower_dir.join("lsp-sidecar.pids");
    let sidecar_launcher = tower_dir.join("launch-lsp-sidecar.sh");
    fs::write(
        &sidecar_launcher,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> {}\nexec {}\n",
            shell_quote(&sidecar_pid_log),
            shell_quote(&workspace_root().join("target/debug/lsp_extension")),
        ),
    )
    .expect("write sidecar launcher");
    let mut permissions = fs::metadata(&sidecar_launcher).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&sidecar_launcher, permissions).expect("make launcher executable");
    install_lsp_extension_with_command(workspace.path(), &sidecar_launcher);

    let fixture_log = tower_dir.join("fixture-lsp.log");
    let fixture_pid_log = tower_dir.join("fixture-lsp.pids");
    fs::write(
        tower_dir.join("config.toml"),
        format!(
            "[daemon]\nidle_timeout_secs = 300\n[extensions]\nrequest_timeout_secs = 3\n\
             [lsp.rust]\ncommand = {}\nargs = [\"diagnostics_by_content\", {}, {}]\nextensions = [\"rs\"]\n",
            toml_string(&workspace_root().join("target/debug/fixture_lsp_server")),
            toml_string(&fixture_log),
            toml_string(&fixture_pid_log),
        ),
    )
    .expect("write fixture diagnostics configuration");

    let (mut daemon, mut connection) = start_lsp_daemon(workspace.path()).await;
    await_public_lsp_diagnostics(&mut connection, Duration::from_secs(15)).await;

    let valid = call_public_diagnostics(&mut connection, 100, "src/valid.rs", 5).await;
    assert_eq!(valid["result"]["isError"], false, "{valid}");
    assert_eq!(
        tool_payload(&valid),
        json!({"supported": true, "diagnostics": []})
    );

    let invalid = call_public_diagnostics(&mut connection, 101, "src/invalid.rs", 5).await;
    assert_eq!(invalid["result"]["isError"], false, "{invalid}");
    assert_eq!(
        tool_payload(&invalid),
        json!({
            "supported": true,
            "diagnostics": [{
                "line": 0,
                "character": 0,
                "endLine": 0,
                "endCharacter": 1,
                "severity": "error",
                "message": "fixture syntax error",
                "source": "fixture-lsp",
                "code": "fixture-syntax",
            }],
        }),
        "{invalid}"
    );

    let sidecar_pid_before = only_recorded_pid(&sidecar_pid_log, "LSP sidecar");
    let fixture_pid_before = only_recorded_pid(&fixture_pid_log, "fixture language server");
    let timeout = call_public_diagnostics(&mut connection, 102, "src/timeout.rs", 5).await;
    assert_eq!(timeout["result"]["isError"], true, "{timeout}");
    assert_eq!(
        timeout["result"]["structuredContent"]["outcome"],
        json!({
            "status": "error",
            "code": "server_timeout",
            "language": "rust",
            "command": workspace_root().join("target/debug/fixture_lsp_server"),
            "operation": "diagnostics",
            "path": "src/timeout.rs",
            "phase": "runtime",
            "message": "language server request failed",
        }),
        "{timeout}"
    );

    let recovered = call_public_diagnostics(&mut connection, 103, "src/valid.rs", 5).await;
    assert_eq!(recovered["result"]["isError"], false, "{recovered}");
    assert_eq!(
        tool_payload(&recovered),
        json!({"supported": true, "diagnostics": []})
    );
    assert_eq!(
        only_recorded_pid(&sidecar_pid_log, "LSP sidecar"),
        sidecar_pid_before
    );
    assert_eq!(
        only_recorded_pid(&fixture_pid_log, "fixture language server"),
        fixture_pid_before
    );

    stop_lsp_daemon(workspace.path(), &mut daemon, connection).await;
}

#[tokio::test]
#[ignore = "requires explicitly provisioned real LSP servers"]
async fn configured_javascript_yaml_json_and_css_servers_return_diagnostics() {
    let bin_dir = std::env::var_os("TOWER_REAL_LSP_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!("TOWER_REAL_LSP_BIN_DIR must point to provisioned real LSP binaries")
        });
    let bin_dir = bin_dir.canonicalize().unwrap_or_else(|error| {
        panic!(
            "TOWER_REAL_LSP_BIN_DIR {} cannot be canonicalized: {error}",
            bin_dir.display()
        )
    });
    let servers: [(&str, &[&str], &str); 4] = [
        ("javascript", &["js"], "typescript-language-server"),
        ("yaml", &["yaml", "yml"], "yaml-language-server"),
        ("json", &["json"], "vscode-json-language-server"),
        ("css", &["css"], "vscode-css-language-server"),
    ];
    let commands: Vec<_> = servers
        .iter()
        .map(|(_, _, executable)| {
            let path = bin_dir.join(executable);
            let canonical = path.canonicalize().unwrap_or_else(|error| {
                panic!(
                    "required provisioned LSP binary {} is unavailable: {error}",
                    path.display()
                )
            });
            assert!(
                canonical.is_file(),
                "required provisioned LSP binary is not a file: {}",
                canonical.display()
            );
            canonical
        })
        .collect();

    let workspace = tempdir().expect("create real-server diagnostics workspace");
    fs::create_dir_all(workspace.path().join("src")).expect("create source directory");
    fs::create_dir_all(workspace.path().join(".tower")).expect("create tower directory");
    install_lsp_extension_with_command(
        workspace.path(),
        &workspace_root().join("target/debug/lsp_extension"),
    );
    let documents = [
        (
            "src/valid.js",
            "const value = 1;\n",
            "src/invalid.js",
            "const value = ;\n",
            14,
            15,
        ),
        (
            "src/valid.yaml",
            "value: 1\n",
            "src/invalid.yaml",
            "value: [1, 2\n",
            7,
            12,
        ),
        (
            "src/valid.json",
            "{\"value\": 1}\n",
            "src/invalid.json",
            "{\"value\": }\n",
            10,
            11,
        ),
        (
            "src/valid.css",
            "a { color: red; }\n",
            "src/invalid.css",
            "a { color red; }\n",
            4,
            14,
        ),
    ];
    for (valid_path, valid, invalid_path, invalid, _, _) in documents {
        fs::write(workspace.path().join(valid_path), valid).expect("write valid real-server input");
        fs::write(workspace.path().join(invalid_path), invalid)
            .expect("write invalid real-server input");
    }
    let mut config = String::from(
        "[daemon]\nidle_timeout_secs = 300\n[extensions]\nrequest_timeout_secs = 30\n",
    );
    for ((language, extensions, _), command) in servers.iter().zip(&commands) {
        config.push_str(&format!(
            "[lsp.{language}]\ncommand = {}\nargs = [\"--stdio\"]\nextensions = {}\n",
            toml_string(command),
            serde_json::to_string(extensions).expect("serialize LSP extensions")
        ));
    }
    fs::write(workspace.path().join(".tower/config.toml"), config)
        .expect("write real-server configuration");

    let (mut daemon, mut connection) = start_lsp_daemon(workspace.path()).await;
    await_public_lsp_diagnostics(&mut connection, Duration::from_secs(45)).await;
    let mut successful_calls = 0_u8;
    for (index, (valid_path, _, invalid_path, invalid_source, expected_start, expected_end)) in
        documents.iter().enumerate()
    {
        let valid =
            call_public_diagnostics(&mut connection, 200 + index as u64 * 2, valid_path, 40).await;
        assert_eq!(valid["result"]["isError"], false, "{valid_path}: {valid}");
        let valid_payload = tool_payload(&valid);
        assert_eq!(
            valid_payload["supported"], true,
            "{valid_path}: {valid_payload}"
        );
        assert_eq!(
            valid_payload["diagnostics"],
            json!([]),
            "{valid_path}: {valid_payload}"
        );
        successful_calls += 1;

        let invalid =
            call_public_diagnostics(&mut connection, 201 + index as u64 * 2, invalid_path, 40)
                .await;
        assert_eq!(
            invalid["result"]["isError"], false,
            "{invalid_path}: {invalid}"
        );
        let invalid_payload = tool_payload(&invalid);
        assert_eq!(
            invalid_payload["supported"], true,
            "{invalid_path}: {invalid_payload}"
        );
        let diagnostics = invalid_payload["diagnostics"]
            .as_array()
            .unwrap_or_else(|| {
                panic!("{invalid_path} must return a diagnostics array: {invalid_payload}")
            });
        assert_invalid_syntax_diagnostic(
            servers[index].0,
            invalid_path,
            invalid_source,
            *expected_start,
            *expected_end,
            diagnostics,
        );
        successful_calls += 1;
    }
    assert_eq!(
        successful_calls, 8,
        "the real-server matrix must make exactly eight successful calls"
    );
    stop_lsp_daemon(workspace.path(), &mut daemon, connection).await;
}

fn assert_invalid_syntax_diagnostic(
    language: &str,
    path: &str,
    source: &str,
    expected_start: u64,
    expected_end: u64,
    diagnostics: &[Value],
) {
    let line_lengths: Vec<u64> = source
        .split('\n')
        .map(|line| {
            line.encode_utf16()
                .count()
                .try_into()
                .expect("source line length fits in u64")
        })
        .collect();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            let (Some(line), Some(character), Some(end_line), Some(end_character)) = (
                diagnostic["line"].as_u64(),
                diagnostic["character"].as_u64(),
                diagnostic["endLine"].as_u64(),
                diagnostic["endCharacter"].as_u64(),
            ) else {
                return false;
            };
            let range_is_bounded = line_lengths
                .get(line as usize)
                .is_some_and(|length| character <= *length)
                && line_lengths
                    .get(end_line as usize)
                    .is_some_and(|length| end_character <= *length)
                && (line, character) <= (end_line, end_character);
            let identifies_expected_syntax = if language == "yaml" {
                diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.to_ascii_lowercase().contains("flow sequence"))
                    && ((line == 0
                        && end_line == 0
                        && character <= expected_end
                        && end_character >= expected_start)
                        || (line == 1 && character == 0 && end_line == 1 && end_character == 0))
            } else {
                line == 0
                    && end_line == 0
                    && character <= expected_end
                    && end_character >= expected_start
            };
            diagnostic["severity"] == "error" && range_is_bounded && identifies_expected_syntax
        }),
        "{language} invalid document {path} must report an error with numeric, document-bounded \
         range on the expected syntax location {expected_start}..={expected_end}: {diagnostics:?}"
    );
}

fn toml_string(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("serialize TOML-compatible path string")
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn install_lsp_extension_with_command(workspace: &Path, command: &Path) {
    let extension_dir = workspace.join(".tower/extensions/lsp");
    fs::create_dir_all(&extension_dir).expect("create workspace LSP extension directory");
    let manifest = fs::read_to_string(workspace_root().join("extensions/lsp/extension.toml"))
        .expect("read shipped LSP manifest")
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("command") {
                format!("command = [{}]", toml_string(command))
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(extension_dir.join("extension.toml"), manifest)
        .expect("write workspace LSP manifest");
}

async fn start_lsp_daemon(workspace: &Path) -> (ProductionDaemonGuard, UnixStream) {
    let binary = selected_production_binary();
    let child = Command::new(&binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn production daemon");
    let guard = ProductionDaemonGuard::new(child, &binary, workspace);
    let mut connection = wait_for_socket(&socket_path(workspace), Duration::from_secs(10))
        .await
        .expect("daemon binds public socket");
    connection
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await
        .expect("send MCP handshake");
    write_line(&mut connection, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"lsp-diagnostics-e2e","version":"1"}}}"#).await.expect("initialize MCP session");
    let initialized = read_mcp_response(&mut connection, 1, Duration::from_secs(10)).await;
    assert!(initialized["result"].is_object(), "{initialized}");
    (guard, connection)
}

async fn read_mcp_response(connection: &mut UnixStream, id: u64, timeout: Duration) -> Value {
    tokio::time::timeout(timeout, async {
        loop {
            let line = read_line_capped(connection, 1 << 20)
                .await
                .expect("read MCP frame")
                .expect("MCP connection closed");
            let frame: Value = serde_json::from_str(&line).expect("parse MCP frame");
            if frame["id"] == id {
                return frame;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("MCP response {id} exceeded {timeout:?}"))
}

async fn await_public_lsp_diagnostics(connection: &mut UnixStream, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut id = 10_u64;
    loop {
        write_line(
            connection,
            &json!({"jsonrpc":"2.0", "id":id, "method":"tools/list", "params":{}}).to_string(),
        )
        .await
        .expect("request tools/list");
        let response = read_mcp_response(connection, id, Duration::from_secs(5)).await;
        if response["result"]["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["name"] == "tower_lsp_diagnostics")
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tower_lsp_diagnostics was not published: {response}"
        );
        id += 1;
        tokio::task::yield_now().await;
    }
}

async fn call_public_diagnostics(
    connection: &mut UnixStream,
    id: u64,
    path: &str,
    timeout_secs: u64,
) -> Value {
    write_line(
        connection,
        &json!({
            "jsonrpc":"2.0", "id":id, "method":"tools/call",
            "params":{"name":"tower_lsp_diagnostics", "arguments":{"path":path}}
        })
        .to_string(),
    )
    .await
    .expect("invoke public diagnostics tool");
    read_mcp_response(connection, id, Duration::from_secs(timeout_secs)).await
}

fn tool_payload(response: &Value) -> Value {
    serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool result lacks JSON text content: {response}")),
    )
    .unwrap_or_else(|error| panic!("tool result text is not JSON: {error}: {response}"))
}

fn only_recorded_pid(path: &Path, process: &str) -> u32 {
    let pids: Vec<u32> = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {process} PID log {}: {error}", path.display()))
        .lines()
        .map(|line| {
            line.parse()
                .unwrap_or_else(|error| panic!("invalid {process} PID {line}: {error}"))
        })
        .collect();
    assert_eq!(pids.len(), 1, "{process} must start exactly once: {pids:?}");
    pids[0]
}

async fn stop_lsp_daemon(
    workspace: &Path,
    daemon: &mut ProductionDaemonGuard,
    connection: UnixStream,
) {
    drop(connection);
    core_engine::adapters::daemon::client::send_control_async(
        &GlobalOpts {
            workspace_dir: Some(workspace.to_path_buf()),
            extensions_dir: None,
        },
        ControlRequest::Shutdown,
    )
    .await
    .expect("request daemon shutdown");
    let status = daemon.wait_and_disarm();
    assert!(status.success(), "daemon exited with {status}");
}
