//! Thin client: connect-or-spawn the daemon, then relay stdio over the socket.
//! Also the `status`/`shutdown` control callers.
#![forbid(unsafe_code)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::adapters::cli::{GlobalOpts, resolve_workspace_root};
use crate::adapters::config;
use crate::adapters::daemon::socket::{
    OwnershipError, lock_path, log_path, socket_path, try_connect,
};
use crate::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, read_line_capped, write_line,
};

/// Override storage for the daemon executable path, used only in tests.
/// Production code never sets this; `daemon_exe()` falls through to `current_exe()`.
static TEST_DAEMON_EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

#[derive(Debug)]
pub struct SpawnAttemptGuard {
    _lock: std::fs::File,
}

impl SpawnAttemptGuard {
    pub fn acquire(workspace_root: &Path) -> Result<Self, OwnershipError> {
        let path = lock_path(workspace_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(OwnershipError::Lock)?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(OwnershipError::Lock)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| OwnershipError::Lock(std::io::Error::from_raw_os_error(error.raw_os_error())),
        )?;
        Ok(Self { _lock: lock })
    }
}

impl Drop for SpawnAttemptGuard {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self._lock, rustix::fs::FlockOperation::Unlock);
    }
}

/// Allow tests to register the `tower` binary path without touching the process
/// environment (avoids the `unsafe` `set_var` requirement in `#![forbid(unsafe_code)]`
/// test files). Only the first call takes effect; subsequent calls are ignored.
pub fn register_test_daemon_exe(path: &std::path::Path) {
    let _ = TEST_DAEMON_EXE.set(path.to_path_buf());
}

/// The `tower` executable to (re)spawn as a daemon.
///
/// Resolution order:
/// 1. [`TEST_DAEMON_EXE`] — set by integration tests via [`register_test_daemon_exe`]
///    so the test binary can distinguish itself from the real `tower` binary.
/// 2. `TOWER_TEST_BIN` env var — legacy test override via environment.
/// 3. `current_exe()` — production path: the running binary IS `tower`.
fn daemon_exe() -> std::io::Result<std::path::PathBuf> {
    if let Some(p) = TEST_DAEMON_EXE.get() {
        return Ok(p.clone());
    }
    if let Ok(p) = std::env::var("TOWER_TEST_BIN")
        && !p.is_empty()
    {
        return Ok(std::path::PathBuf::from(p));
    }
    std::env::current_exe()
}

/// Spawn a detached `tower daemon --detach` for `workspace_root`.
fn spawn_daemon(workspace_root: &Path) -> std::io::Result<()> {
    let exe = daemon_exe()?;
    let log_path = log_path(workspace_root);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let log2 = log.try_clone()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("--detach")
        .arg("--workspace-dir")
        .arg(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()?;
    Ok(())
}

/// Connect to the daemon, spawning one (under a flock) if absent/stale.
pub async fn connect_or_spawn(workspace_root: &Path) -> std::io::Result<UnixStream> {
    let timeout = config::load(workspace_root)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?
        .daemon
        .connection_timeout();
    let deadline = tokio::time::Instant::now() + timeout;
    let sock = socket_path(workspace_root);
    if let Some(s) = try_connect(&sock).await? {
        return Ok(s);
    }

    let guard = loop {
        match SpawnAttemptGuard::acquire(workspace_root) {
            Ok(guard) => break guard,
            Err(OwnershipError::Lock(error))
                if error.raw_os_error() == Some(rustix::io::Errno::WOULDBLOCK.raw_os_error()) => {}
            Err(error) => return Err(std::io::Error::other(error)),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(connection_timeout_error());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    // Re-check: another client may have spawned while we waited for the lock.
    let stream = if let Some(s) = try_connect(&sock).await? {
        s
    } else {
        spawn_daemon(workspace_root)?;
        loop {
            if let Some(stream) = try_connect(&sock).await? {
                break stream;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(connection_timeout_error());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    drop(guard);
    Ok(stream)
}

fn connection_timeout_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "timed out connecting to workspace daemon",
    )
}

/// Relay this process's stdin/stdout to the connected socket, after sending the
/// `mcp` handshake. Returns when either side reaches EOF.
async fn relay_mcp(stream: UnixStream) -> std::io::Result<()> {
    let (mut sr, mut sw) = stream.into_split();
    sw.write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .await?;
    sw.flush().await?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        tokio::io::copy(&mut stdin, &mut sw).await?;
        sw.shutdown().await
    };
    let down = async {
        tokio::io::copy(&mut sr, &mut stdout).await?;
        stdout.flush().await
    };

    tokio::pin!(up);
    tokio::pin!(down);
    tokio::select! {
        r = &mut up => {
            r?;
            down.await
        }
        r = &mut down => r,
    }
}

/// Blocking entrypoint for `tower mcp`.
pub fn run_mcp_client(opts: &GlobalOpts) -> Result<(), String> {
    let workspace_root = resolve_workspace_root(opts);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("mcp-client")
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;
    rt.block_on(async {
        let stream = connect_or_spawn(&workspace_root)
            .await
            .map_err(|e| format!("failed to reach tower daemon: {e}"))?;
        relay_mcp(stream)
            .await
            .map_err(|e| format!("relay error: {e}"))
    })
}

/// Async core of [`send_control`]: send one control request and return the response.
pub async fn send_control_async(
    opts: &GlobalOpts,
    req: ControlRequest,
) -> Result<ControlResponse, String> {
    let workspace_root = resolve_workspace_root(opts);
    let sock = socket_path(&workspace_root);
    let mut conn = try_connect(&sock)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no running tower daemon for this workspace".to_string())?;
    conn.write_all(Handshake::new(ClientRole::Control).to_line().as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    write_line(&mut conn, &serde_json::to_string(&req).unwrap())
        .await
        .map_err(|e| e.to_string())?;
    let line = read_line_capped(&mut conn, 1 << 20)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "daemon closed without responding".to_string())?;
    serde_json::from_str::<ControlResponse>(&line).map_err(|e| e.to_string())
}

/// Send a one-shot control request to the daemon (no spawn). Blocking wrapper
/// for use from synchronous contexts (CLI `tower status` / `tower shutdown`).
///
/// For async callers (e.g. integration tests), use [`send_control_async`] instead
/// to avoid creating a nested runtime.
pub fn send_control(opts: &GlobalOpts, req: ControlRequest) -> Result<ControlResponse, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;
    rt.block_on(send_control_async(opts, req))
}

#[cfg(test)]
mod connection_error_tests {
    #[test]
    fn connection_timeout_has_stable_kind_and_message() {
        let error = super::connection_timeout_error();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "timed out connecting to workspace daemon"
        );
    }

    #[tokio::test]
    async fn invalid_socket_probe_does_not_spawn_a_daemon() {
        // The invalid name cannot denote an absent/refused Unix socket. A
        // spawn attempt would create .tower/daemon.log under this workspace.
        let workspace = tempfile::tempdir().unwrap();
        let invalid = workspace.path().join("x".repeat(200));
        assert!(super::connect_or_spawn(&invalid).await.is_err());
        assert!(!invalid.join(".tower/daemon.log").exists());
        assert!(!invalid.join(".tower/daemon.lock").exists());
    }
}
