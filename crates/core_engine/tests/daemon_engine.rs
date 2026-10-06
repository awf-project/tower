//! Engine-builder smoke test (Task 6).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, Command, Stdio};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::lsp::validate_lsp_commands;
use core_engine::adapters::config::{LspConfig, LspServerConfig, TowerConfig};
use core_engine::adapters::daemon::engine::build_engine;
use core_engine::adapters::daemon::socket::socket_path;
use core_engine::adapters::daemon::wire::{ClientRole, Handshake};
use extension_protocol::{LspOutcomeCode, LspOutcomePhase, LspOutcomeStatus};
use tempfile::tempdir;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn build_engine_indexes_a_fresh_workspace() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"pub fn a() {}").unwrap();
    std::fs::write(dir.path().join("b.rs"), b"pub fn b() {}").unwrap();

    let opts = GlobalOpts {
        workspace_dir: Some(dir.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, TowerConfig::default()).expect("engine builds");

    let ws = handle.state.read().unwrap();
    let count = ws.workspace_arc().read().unwrap().all_file_ids().len();
    assert_eq!(count, 2, "two source files indexed");
}

#[test]
fn f008_startup_validation_validates_all_language_entries_in_key_order_including_duplicate_extension_mappings()
 {
    let dir = tempdir().unwrap();
    let launched_marker = dir.path().join("valid-server-was-launched");
    let valid_server = dir.path().join("valid-f008-server");
    std::fs::write(
        &valid_server,
        format!("#!/bin/sh\ntouch '{}'\n", launched_marker.display()),
    )
    .expect("write valid LSP server fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(&valid_server, std::fs::Permissions::from_mode(0o755))
            .expect("make valid LSP server executable");
    }
    let config = LspConfig {
        servers: BTreeMap::from([
            (
                "zeta".to_owned(),
                LspServerConfig {
                    command: "./missing-zeta".to_owned(),
                    extensions: vec!["rs".to_owned()],
                    args: Vec::new(),
                },
            ),
            (
                "beta".to_owned(),
                LspServerConfig {
                    command: "./valid-f008-server".to_owned(),
                    extensions: vec!["rs".to_owned()],
                    args: Vec::new(),
                },
            ),
            (
                "alpha".to_owned(),
                LspServerConfig {
                    command: "   ".to_owned(),
                    extensions: vec!["rs".to_owned()],
                    args: Vec::new(),
                },
            ),
        ]),
        idle_timeout: None,
    };

    let outcomes = validate_lsp_commands(&config, dir.path(), None);

    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].language.as_deref(), Some("alpha"));
    assert_eq!(outcomes[0].command.as_deref(), Some("   "));
    assert_eq!(outcomes[0].code, LspOutcomeCode::InvalidCommand);
    assert_eq!(outcomes[1].language.as_deref(), Some("zeta"));
    assert_eq!(outcomes[1].command.as_deref(), Some("./missing-zeta"));
    assert_eq!(outcomes[1].code, LspOutcomeCode::ServerMissing);
    assert!(
        !launched_marker.exists(),
        "startup validation must resolve a valid command without launching its server"
    );
    for outcome in outcomes {
        assert_eq!(outcome.status, LspOutcomeStatus::Error);
        assert_eq!(outcome.phase, LspOutcomePhase::Startup);
        assert_eq!(outcome.operation, None);
        assert_eq!(outcome.path, None);
    }
}

#[cfg(unix)]
#[test]
fn f008_startup_validation_hands_an_explicit_empty_servers_map_to_the_lsp_extension() {
    let workspace = tempdir().expect("create workspace");
    let extension_dir = workspace.path().join("extensions/lsp");
    std::fs::create_dir_all(&extension_dir).expect("create LSP extension directory");
    let received_initialize = workspace.path().join("received-initialize.json");
    let script = extension_dir.join("capture-initialize.sh");
    std::fs::write(
        &script,
        format!(
            "read -r line\nprintf '%s' \"$line\" > '{}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"type\":\"Initialized\",\"data\":{{\"tools\":[],\"events\":[],\"capabilities\":[]}}}}}}'\n",
            received_initialize.display()
        ),
    )
    .expect("write LSP extension fixture");
    std::fs::write(
        extension_dir.join("extension.toml"),
        format!(
            "name = \"lsp\"\nversion = \"0.1.0\"\ncommand = [\"/bin/sh\", \"{}\"]\nactivation = \"eager\"\n",
            script.display()
        ),
    )
    .expect("write LSP extension manifest");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: Some(workspace.path().join("extensions")),
    };
    let _engine =
        build_engine(&opts, TowerConfig::default()).expect("build engine with LSP extension");

    let initialize: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&received_initialize)
            .expect("eager LSP extension receives initialize request"),
    )
    .expect("captured initialize request is JSON");
    assert_eq!(
        initialize["params"]["extension_config"],
        serde_json::json!({
            "servers": {},
            "idle_timeout_secs": null,
        }),
        "daemon must hand the explicit empty LSP configuration to its lsp extension"
    );
}

#[cfg(unix)]
#[test]
fn f008_startup_validation_emits_sanitized_diagnostics_before_a_tool_call_and_keeps_native_requests_available()
 {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    std::fs::write(dir.path().join("hello.txt"), "still serving").unwrap();
    std::fs::write(
        dir.path().join(".tower/config.toml"),
        r#"
[daemon]
idle_timeout_secs = 300

[lsp.rust]
command = "./missing-f008-server"
extensions = ["rs"]
args = ["F008_SENTINEL_ARGUMENT"]
"#,
    )
    .unwrap();

    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_tower"))
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(dir.path())
            .env("PATH", "F008_SENTINEL_PATH")
            .env("F008_SENTINEL_ENVIRONMENT", "F008_SENTINEL_ENV_VALUE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tower daemon"),
    );
    let stderr = child.0.stderr.take().expect("daemon stderr");
    let mut startup_lines = Vec::new();
    for line in BufReader::new(stderr).lines() {
        let line = line.expect("read daemon stderr");
        let listening = line.contains("daemon listening on");
        startup_lines.push(line);
        if listening {
            break;
        }
    }

    let diagnostic_index = startup_lines
        .iter()
        .position(|line| line.contains("\"outcome\""))
        .expect("startup must emit a structured LSP outcome");
    let listening_index = startup_lines
        .iter()
        .position(|line| line.contains("daemon listening on"))
        .expect("daemon must continue to serving state");
    assert!(diagnostic_index < listening_index);

    let diagnostic = &startup_lines[diagnostic_index];
    let json_start = diagnostic.find('{').expect("diagnostic JSON object");
    let payload: serde_json::Value =
        serde_json::from_str(&diagnostic[json_start..]).expect("structured startup diagnostic");
    assert_eq!(
        payload["outcome"],
        serde_json::json!({
            "status": "error",
            "code": "server_missing",
            "language": "rust",
            "command": "./missing-f008-server",
            "operation": null,
            "path": null,
            "phase": "startup",
            "message": payload["outcome"]["message"].clone(),
        })
    );
    for secret in [
        "F008_SENTINEL_ARGUMENT",
        "F008_SENTINEL_ENVIRONMENT",
        "F008_SENTINEL_ENV_VALUE",
        "F008_SENTINEL_PATH",
    ] {
        assert!(!diagnostic.contains(secret), "diagnostic leaked {secret}");
    }

    let mut stream = std::os::unix::net::UnixStream::connect(socket_path(dir.path()))
        .expect("connect to daemon socket");
    stream
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .unwrap();
    stream
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"f008-test\",\"version\":\"1\"}}}\n",
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    assert!(
        response.contains("\"result\""),
        "initialize failed: {response}"
    );

    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .unwrap();
    stream
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"tower_read_file\",\"arguments\":{\"path\":\"hello.txt\"}}}\n",
        )
        .unwrap();
    response.clear();
    reader.read_line(&mut response).unwrap();
    assert!(
        response.contains("still serving"),
        "native request failed: {response}"
    );
}
