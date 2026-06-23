//! Daemon end-to-end tests (Tasks 7–8).
#![forbid(unsafe_code)]

use std::time::Duration;

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::{DaemonConfig, TowerConfig};
use core_engine::adapters::daemon::server::run_daemon;
use core_engine::adapters::daemon::socket::{socket_path, wait_for_socket};
use core_engine::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, read_line_capped, write_line,
};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    // Daemon is up...
    let _ = wait_for_socket(&sock, Duration::from_secs(5))
        .await
        .unwrap();
    // ...no keep-alive clients connected; after ~idle it should exit and the
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
