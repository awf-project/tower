//! Unix socket paths and lifecycle helpers (connect / bind / spawn-lock).
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

/// `<workspace>/.tower/daemon.sock`.
#[must_use]
pub fn socket_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.sock")
}

/// `<workspace>/.tower/daemon.lock` — flock target for the spawn race.
#[must_use]
pub fn lock_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.lock")
}

/// `<workspace>/.tower/daemon.log` — detached daemon's stdout/stderr.
#[must_use]
pub fn log_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.log")
}

/// Connect to the daemon socket. `None` when no daemon is listening
/// (file missing, or present-but-refused = stale after a crash).
pub async fn try_connect(path: &Path) -> Option<UnixStream> {
    match UnixStream::connect(path).await {
        Ok(s) => Some(s),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            None
        }
        Err(_) => None,
    }
}

/// Bind the daemon socket, cleaning a stale file first.
///
/// - No file → bind.
/// - File present + a live listener answers a probe connect → `AddrInUse`.
/// - File present + probe refused (stale) → unlink + bind.
pub fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        // Probe with a blocking std connect; refused ⇒ stale.
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "a live daemon already owns this socket",
                ));
            }
            Err(_) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    UnixListener::bind(path)
}

/// Acquire the exclusive spawn lock (blocking `flock`). The returned `File`
/// holds the lock; dropping it releases. Caller runs this on a blocking thread.
pub fn acquire_spawn_lock(workspace_root: &Path) -> std::io::Result<std::fs::File> {
    let path = lock_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    Ok(file)
}

/// Poll `try_connect` until it succeeds or `timeout` elapses.
pub async fn wait_for_socket(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(s) = try_connect(path).await {
            return Ok(s);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for daemon socket",
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
