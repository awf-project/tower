//! Thin client: connect-or-spawn the daemon, then relay stdio over the socket.
//! Also the `status`/`shutdown` control callers.
#![forbid(unsafe_code)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::adapters::cli::{GlobalOpts, resolve_workspace_root};
use crate::adapters::config;
use crate::adapters::config::{ProjectIdentity, QueryableProject};
use crate::adapters::daemon::socket::{
    OwnerLiveness, OwnershipError, lock_path, log_path, probe_owner, socket_path, try_connect,
};
use crate::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, read_line_capped, write_line,
};
use crate::domain::CancellationSignal;

#[derive(Debug)]
pub enum ExternalDaemonError {
    AvailabilityTimeout {
        project: ProjectIdentity,
    },
    Cancelled {
        project: ProjectIdentity,
        operation: String,
    },
    Handshake {
        project: ProjectIdentity,
        source: std::io::Error,
    },
    Initialize {
        project: ProjectIdentity,
        message: String,
    },
    TargetCall {
        project: ProjectIdentity,
        operation: String,
        message: String,
    },
    Decode {
        project: ProjectIdentity,
        operation: String,
        message: String,
    },
}

impl std::fmt::Display for ExternalDaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let project = match self {
            Self::AvailabilityTimeout { project }
            | Self::Cancelled { project, .. }
            | Self::Handshake { project, .. }
            | Self::Initialize { project, .. }
            | Self::TargetCall { project, .. }
            | Self::Decode { project, .. } => project,
        };
        let operation = match self {
            Self::Cancelled { operation, .. }
            | Self::TargetCall { operation, .. }
            | Self::Decode { operation, .. } => Some(operation),
            Self::AvailabilityTimeout { .. } | Self::Handshake { .. } | Self::Initialize { .. } => {
                None
            }
        };
        let failure = match self {
            Self::AvailabilityTimeout { .. } => "availability timed out",
            Self::Cancelled { .. } => "request cancelled",
            Self::Handshake { .. } => "handshake failed",
            Self::Initialize { .. } => "initialization failed",
            Self::TargetCall { .. } => "target call failed",
            Self::Decode { .. } => "target response decoding failed",
        };

        write!(
            formatter,
            "external daemon {failure} for project {} ({})",
            project.id.as_str(),
            project.name
        )?;
        if let Some(operation) = operation {
            write!(formatter, " during {operation}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ExternalDaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Handshake { source, .. } => Some(source),
            Self::AvailabilityTimeout { .. }
            | Self::Cancelled { .. }
            | Self::Initialize { .. }
            | Self::TargetCall { .. }
            | Self::Decode { .. } => None,
        }
    }
}

pub trait ExternalProjectConnector: Send + Sync {
    fn call(
        &self,
        project: &QueryableProject,
        operation: &str,
        arguments: Value,
        availability_timeout: Duration,
        cancellation: &dyn CancellationSignal,
    ) -> Result<Value, ExternalDaemonError>;
}

pub struct ExternalDaemonClient;

impl ExternalDaemonClient {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for ExternalDaemonClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalProjectConnector for ExternalDaemonClient {
    fn call(
        &self,
        project: &QueryableProject,
        operation: &str,
        arguments: Value,
        availability_timeout: Duration,
        cancellation: &dyn CancellationSignal,
    ) -> Result<Value, ExternalDaemonError> {
        let identity = ProjectIdentity {
            id: project.id.clone(),
            name: project.name.clone(),
        };
        let Some(workspace_root) = project.canonical_path.as_deref() else {
            return Err(ExternalDaemonError::AvailabilityTimeout { project: identity });
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_error| ExternalDaemonError::Initialize {
                project: identity.clone(),
                message: "client runtime initialization failed".to_owned(),
            })?;
        runtime.block_on(call_external_daemon(
            workspace_root,
            &identity,
            operation,
            arguments,
            availability_timeout,
            cancellation,
        ))
    }
}

async fn call_external_daemon(
    workspace_root: &Path,
    project: &ProjectIdentity,
    operation: &str,
    arguments: Value,
    availability_timeout: Duration,
    cancellation: &dyn CancellationSignal,
) -> Result<Value, ExternalDaemonError> {
    let deadline = tokio::time::Instant::now()
        .checked_add(availability_timeout)
        .ok_or_else(|| ExternalDaemonError::AvailabilityTimeout {
            project: project.clone(),
        })?;
    let client =
        initialize_external_daemon(workspace_root, project, operation, deadline, cancellation)
            .await?;
    let result = invoke_external_tool(client, project, operation, arguments, cancellation).await?;
    decode_external_result(result, project, operation)
}

async fn initialize_external_daemon(
    workspace_root: &Path,
    project: &ProjectIdentity,
    operation: &str,
    deadline: tokio::time::Instant,
    cancellation: &dyn CancellationSignal,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, ExternalDaemonError> {
    let mut stream =
        connect_or_spawn_with_deadline(workspace_root, project, operation, deadline, cancellation)
            .await?;

    run_during_availability(deadline, cancellation, async {
        #[cfg(test)]
        test_stage_pause(workspace_root, "handshake").await;
        #[cfg(test)]
        if workspace_root.join(".tower/fail-handshake").exists() {
            return Err(std::io::Error::other("RAW_HANDSHAKE_SECRET"));
        }
        stream
            .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
            .await?;
        stream.flush().await
    })
    .await
    .map_err(|error| match error {
        AvailabilityWaitError::Timeout => ExternalDaemonError::AvailabilityTimeout {
            project: project.clone(),
        },
        AvailabilityWaitError::Cancelled => ExternalDaemonError::Cancelled {
            project: project.clone(),
            operation: operation.to_owned(),
        },
        AvailabilityWaitError::Inner(source) => ExternalDaemonError::Handshake {
            project: project.clone(),
            source,
        },
    })?;

    let initialize = async {
        #[cfg(test)]
        test_stage_pause(workspace_root, "initialize").await;
        <() as ServiceExt<rmcp::RoleClient>>::serve((), stream).await
    };
    run_during_availability(deadline, cancellation, initialize)
        .await
        .map_err(|error| match error {
            AvailabilityWaitError::Timeout => ExternalDaemonError::AvailabilityTimeout {
                project: project.clone(),
            },
            AvailabilityWaitError::Cancelled => ExternalDaemonError::Cancelled {
                project: project.clone(),
                operation: operation.to_owned(),
            },
            AvailabilityWaitError::Inner(_source) => ExternalDaemonError::Initialize {
                project: project.clone(),
                message: "rmcp client initialization failed".to_owned(),
            },
        })
}

async fn invoke_external_tool(
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    project: &ProjectIdentity,
    operation: &str,
    arguments: Value,
    cancellation: &dyn CancellationSignal,
) -> Result<CallToolResult, ExternalDaemonError> {
    let arguments = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| ExternalDaemonError::Decode {
            project: project.clone(),
            operation: operation.to_owned(),
            message: "tool arguments must be an object".to_owned(),
        })?;
    let result = {
        let call = client
            .peer()
            .call_tool(CallToolRequestParams::new(operation.to_owned()).with_arguments(arguments));
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => break Some(result),
                () = cancellation_tick() => {
                    if cancellation.is_cancelled() {
                        break None;
                    }
                }
            }
        }
    };
    let Some(result) = result else {
        let _ = client.cancel().await;
        return Err(ExternalDaemonError::Cancelled {
            project: project.clone(),
            operation: operation.to_owned(),
        });
    };
    result.map_err(|error| match error {
        rmcp::service::ServiceError::UnexpectedResponse => ExternalDaemonError::Decode {
            project: project.clone(),
            operation: operation.to_owned(),
            message: "target returned an unexpected response".to_owned(),
        },
        _other => ExternalDaemonError::TargetCall {
            project: project.clone(),
            operation: operation.to_owned(),
            message: "target request failed".to_owned(),
        },
    })
}

fn decode_external_result(
    result: CallToolResult,
    project: &ProjectIdentity,
    operation: &str,
) -> Result<Value, ExternalDaemonError> {
    if result.is_error == Some(true) {
        return Err(ExternalDaemonError::TargetCall {
            project: project.clone(),
            operation: operation.to_owned(),
            message: "target returned an error result".to_owned(),
        });
    }
    let text = result
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .map(|content| content.text.as_str())
        .ok_or_else(|| ExternalDaemonError::Decode {
            project: project.clone(),
            operation: operation.to_owned(),
            message: "target result did not contain text content".to_owned(),
        })?;
    serde_json::from_str(text).map_err(|_error| ExternalDaemonError::Decode {
        project: project.clone(),
        operation: operation.to_owned(),
        message: "target result was not valid JSON".to_owned(),
    })
}

enum AvailabilityWaitError<E> {
    Timeout,
    Cancelled,
    Inner(E),
}

async fn run_during_availability<T, E>(
    deadline: tokio::time::Instant,
    cancellation: &dyn CancellationSignal,
    future: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, AvailabilityWaitError<E>> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result.map_err(AvailabilityWaitError::Inner),
            () = tokio::time::sleep_until(deadline) => return Err(AvailabilityWaitError::Timeout),
            () = cancellation_tick() => {
                if cancellation.is_cancelled() {
                    return Err(AvailabilityWaitError::Cancelled);
                }
            }
        }
    }
}

async fn cancellation_tick() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

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
fn spawn_daemon(workspace_root: &Path) -> std::io::Result<std::process::Child> {
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
        .spawn()
}

pub(crate) async fn connect_or_spawn_with_deadline(
    workspace_root: &Path,
    project: &ProjectIdentity,
    operation: &str,
    deadline: tokio::time::Instant,
    cancellation: &dyn CancellationSignal,
) -> Result<UnixStream, ExternalDaemonError> {
    if cancellation.is_cancelled() {
        return Err(map_availability_io_error(
            AvailabilityWaitError::Cancelled,
            project,
            operation,
        ));
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(map_availability_io_error(
            AvailabilityWaitError::Timeout,
            project,
            operation,
        ));
    }
    acquire_daemon_socket(
        workspace_root,
        ConnectionWait::External {
            deadline,
            cancellation,
        },
    )
    .await
    .map_err(|error| map_availability_io_error(error, project, operation))
}

/// Ordinary connections retain their optional local budget and I/O error contract.
pub async fn connect_or_spawn(workspace_root: &Path) -> std::io::Result<UnixStream> {
    let timeout = config::load(workspace_root)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?
        .daemon
        .connection_timeout();
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    acquire_daemon_socket(workspace_root, ConnectionWait::Local { deadline })
        .await
        .map_err(|error| match error {
            AvailabilityWaitError::Inner(source) => source,
            AvailabilityWaitError::Timeout => connection_timeout_error(),
            AvailabilityWaitError::Cancelled => {
                unreachable!("local connections have no cancellation signal")
            }
        })
}

/// Only the external path bounds each I/O stage; local retry timing stays compatible.
#[derive(Clone, Copy)]
enum ConnectionWait<'a> {
    Local {
        deadline: Option<tokio::time::Instant>,
    },
    External {
        deadline: tokio::time::Instant,
        cancellation: &'a dyn CancellationSignal,
    },
}

impl ConnectionWait<'_> {
    async fn io<T>(
        self,
        future: impl std::future::Future<Output = std::io::Result<T>>,
    ) -> Result<T, AvailabilityWaitError<std::io::Error>> {
        match self {
            Self::Local { .. } => future.await.map_err(AvailabilityWaitError::Inner),
            Self::External {
                deadline,
                cancellation,
            } => run_during_availability(deadline, cancellation, future).await,
        }
    }

    async fn retry(self) -> Result<(), AvailabilityWaitError<std::io::Error>> {
        match self {
            Self::Local { deadline } => {
                if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                    return Err(AvailabilityWaitError::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(())
            }
            Self::External { .. } => {
                self.io(async {
                    cancellation_tick().await;
                    Ok(())
                })
                .await
            }
        }
    }

    async fn probe(
        self,
        workspace_root: &Path,
    ) -> Result<Option<UnixStream>, AvailabilityWaitError<std::io::Error>> {
        self.io(async {
            #[cfg(test)]
            if matches!(self, Self::External { .. }) {
                test_stage_pause(workspace_root, "socket").await;
            }
            try_connect(&socket_path(workspace_root)).await
        })
        .await
    }

    async fn spawn(
        self,
        workspace_root: &Path,
    ) -> Result<std::process::Child, AvailabilityWaitError<std::io::Error>> {
        self.io(async {
            match self {
                Self::Local { .. } => spawn_daemon(workspace_root),
                Self::External { .. } => {
                    #[cfg(test)]
                    test_stage_pause(workspace_root, "spawn").await;
                    let root = workspace_root.to_path_buf();
                    tokio::task::spawn_blocking(move || spawn_daemon(&root))
                        .await
                        .map_err(std::io::Error::other)?
                }
            }
        })
        .await
    }
}

async fn acquire_daemon_socket(
    workspace_root: &Path,
    wait: ConnectionWait<'_>,
) -> Result<UnixStream, AvailabilityWaitError<std::io::Error>> {
    if let Some(stream) = wait.probe(workspace_root).await? {
        return Ok(stream);
    }
    let guard = loop {
        match SpawnAttemptGuard::acquire(workspace_root) {
            Ok(guard) => break guard,
            Err(OwnershipError::Lock(error))
                if error.raw_os_error() == Some(rustix::io::Errno::WOULDBLOCK.raw_os_error()) => {}
            Err(error) => return Err(AvailabilityWaitError::Inner(std::io::Error::other(error))),
        }
        #[cfg(test)]
        wait.io(async {
            test_stage_pause(workspace_root, "contention").await;
            Ok(())
        })
        .await?;
        wait.retry().await?;
    };
    // Another client may have published its socket while we waited for ownership.
    let stream = if let Some(stream) = wait.probe(workspace_root).await? {
        stream
    } else {
        let mut child = wait.spawn(workspace_root).await?;
        let mut child_exit = None;
        loop {
            if let Some(stream) = wait.probe(workspace_root).await? {
                break stream;
            }
            if child_exit.is_none() {
                child_exit = child.try_wait().map_err(AvailabilityWaitError::Inner)?;
            }
            if let Some(status) = child_exit
                && probe_owner(workspace_root)
                    .map_err(|error| AvailabilityWaitError::Inner(std::io::Error::other(error)))?
                    == OwnerLiveness::Dead
            {
                return Err(AvailabilityWaitError::Inner(std::io::Error::other(
                    format!("workspace daemon exited before publishing its socket ({status})"),
                )));
            }
            wait.retry().await?;
        }
    };
    drop(guard);
    Ok(stream)
}

#[cfg(test)]
async fn test_stage_pause(workspace_root: &Path, stage: &str) {
    let marker = workspace_root.join(".tower").join(format!("hold-{stage}"));
    if marker.exists() {
        std::fs::write(marker.with_extension("entered"), "entered")
            .expect("report entered acquisition stage");
        while marker.exists() {
            tokio::task::yield_now().await;
        }
    }
}

fn map_availability_io_error(
    error: AvailabilityWaitError<std::io::Error>,
    project: &ProjectIdentity,
    operation: &str,
) -> ExternalDaemonError {
    match error {
        AvailabilityWaitError::Timeout => ExternalDaemonError::AvailabilityTimeout {
            project: project.clone(),
        },
        AvailabilityWaitError::Cancelled => ExternalDaemonError::Cancelled {
            project: project.clone(),
            operation: operation.to_owned(),
        },
        AvailabilityWaitError::Inner(source) => ExternalDaemonError::Handshake {
            project: project.clone(),
            source,
        },
    }
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

#[cfg(test)]
mod external_query_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::{
        ExternalDaemonClient, ExternalDaemonError, ExternalProjectConnector, SpawnAttemptGuard,
        connect_or_spawn_with_deadline,
    };
    use crate::adapters::config::{ProjectId, ProjectIdentity, QueryableProject};
    use crate::domain::{CancellationSignal, NeverCancelled};

    #[derive(Clone)]
    struct ToggleCancellation(Arc<AtomicBool>);

    impl CancellationSignal for ToggleCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    enum FixtureReply {
        Result(Value),
        InitializeError,
        TargetError,
        TargetTimeout,
        MalformedResult,
        HoldUntilReleased,
    }

    /// A real daemon-socket fixture.  It deliberately speaks the daemon
    /// handshake plus JSON-RPC over Unix sockets instead of replacing the
    /// connector: GREEN must exercise the same wire boundary as production.
    struct ExternalDaemonFixture {
        _workspace: tempfile::TempDir,
        call_started: Receiver<()>,
        release: Option<Sender<()>>,
        worker: thread::JoinHandle<()>,
    }

    impl ExternalDaemonFixture {
        fn start(reply: FixtureReply) -> Self {
            let workspace = tempfile::tempdir().expect("temporary daemon fixture workspace");
            let tower_dir = workspace.path().join(".tower");
            std::fs::create_dir(&tower_dir).expect("create fixture .tower directory");
            std::fs::write(tower_dir.join("daemon.log"), "fixture log capture\n")
                .expect("initialize captured daemon log");
            let listener = UnixListener::bind(tower_dir.join("daemon.sock"))
                .expect("bind fixture daemon socket");
            let (call_started_tx, call_started) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let release = matches!(reply, FixtureReply::HoldUntilReleased).then_some(release_tx);
            let worker = thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept external daemon client");
                let mut reader = BufReader::new(stream.try_clone().expect("clone fixture stream"));
                let mut writer = stream;
                let mut line = String::new();
                if reader.read_line(&mut line).expect("read daemon handshake") == 0 {
                    return;
                }
                assert!(
                    line.contains("\"role\":\"mcp\""),
                    "expected MCP daemon handshake"
                );

                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => return,
                        Ok(_) => {}
                        // A client cancelling or rejecting initialization may
                        // reset the socket. Only an empty frame is clean EOF;
                        // truncated requests and all other failures still fail.
                        Err(error)
                            if error.kind() == std::io::ErrorKind::ConnectionReset
                                && line.is_empty() =>
                        {
                            return;
                        }
                        Err(error) => {
                            panic!("read JSON-RPC frame: {error}; partial frame: {line:?}")
                        }
                    }
                    let frame: Value = serde_json::from_str(&line).expect("valid JSON-RPC request");
                    let Some(method) = frame.get("method").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(id) = frame.get("id").cloned() else {
                        continue;
                    };
                    let response = match method {
                        "initialize" if matches!(reply, FixtureReply::InitializeError) => json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32603, "message": "RAW_INITIALIZATION_SECRET"}
                        }),
                        "initialize" => json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "external-fixture", "version": "1"}
                            }
                        }),
                        "tools/call" => {
                            call_started_tx
                                .send(())
                                .expect("report admitted target call");
                            match &reply {
                                FixtureReply::Result(value) => json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "result": {"content": [{"type": "text", "text": value.to_string()}]}
                                }),
                                FixtureReply::TargetTimeout => json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "result": {"content": [{"type": "text", "text": json!({"code": "operation_timeout", "detail": "RAW_TARGET_SECRET"}).to_string()}], "isError": true}
                                }),
                                FixtureReply::TargetError => json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "error": {"code": -32603, "message": "RAW_TARGET_CALL_SECRET"}
                                }),
                                FixtureReply::MalformedResult => json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "result": {"content": [{"type": "text", "text": "RAW_DECODE_SECRET:not-json"}]}
                                }),
                                FixtureReply::HoldUntilReleased => {
                                    release_rx.recv().expect("release held target call");
                                    json!({
                                        "jsonrpc": "2.0", "id": id,
                                        "result": {"content": [{"type": "text", "text": json!({"content": "released after acquisition deadline"}).to_string()}]}
                                    })
                                }
                                FixtureReply::InitializeError => {
                                    json!({
                                        "jsonrpc": "2.0", "id": id,
                                        "error": {"code": -32603, "message": "fixture admitted a call after failed initialization"}
                                    })
                                }
                            }
                        }
                        _ => continue,
                    };
                    if writeln!(writer, "{response}").is_err() || writer.flush().is_err() {
                        return;
                    }
                }
            });
            Self {
                _workspace: workspace,
                call_started,
                release,
                worker,
            }
        }

        fn target(&self) -> QueryableProject {
            project(Some(self._workspace.path().to_path_buf()))
        }

        fn captured_logs(&self) -> String {
            std::fs::read_to_string(self._workspace.path().join(".tower/daemon.log"))
                .expect("read captured daemon log")
        }

        fn wait_for_call(&self) {
            self.call_started
                .recv_timeout(Duration::from_secs(2))
                .expect("fixture must observe exactly one admitted tools/call");
        }

        fn release(&mut self) {
            self.release
                .take()
                .expect("fixture has a releasable call")
                .send(())
                .expect("release held target call");
        }

        fn join(self) {
            self.worker
                .join()
                .expect("fixture worker must finish cleanly");
        }
    }

    fn identity() -> ProjectIdentity {
        ProjectIdentity {
            id: ProjectId::new("external".to_owned()).expect("valid fixture project id"),
            name: "External fixture".to_owned(),
        }
    }

    fn project(canonical_path: Option<PathBuf>) -> QueryableProject {
        let configured_path = PathBuf::from("configured-relative-path");
        QueryableProject {
            id: identity().id,
            name: identity().name,
            resolved_path: PathBuf::from("/resolved/path/must/not/be/probed"),
            configured_path,
            canonical_path,
        }
    }

    fn assert_availability_timeout(error: ExternalDaemonError) {
        assert!(
            matches!(
                error,
                ExternalDaemonError::AvailabilityTimeout { ref project }
                    if *project == identity()
            ),
            "expected the configured project identity on availability timeout, got {error:?}"
        );
    }

    fn hold_stage(workspace: &Path, stage: &str) -> PathBuf {
        let marker = workspace.join(".tower").join(format!("hold-{stage}"));
        std::fs::create_dir_all(marker.parent().expect("stage marker parent"))
            .expect("create stage marker parent");
        std::fs::write(&marker, "held").expect("create stage marker");
        marker
    }

    async fn poll_once<F: std::future::Future>(
        mut future: std::pin::Pin<&mut F>,
    ) -> std::task::Poll<F::Output> {
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
    }

    // Yield to the real Unix-socket fixture while keeping virtual time fixed.
    // The wall-clock guard detects deadlocks without becoming a query budget.
    async fn drive_until<F: std::future::Future>(
        mut future: std::pin::Pin<&mut F>,
        mut observed: impl FnMut() -> bool,
    ) where
        F::Output: std::fmt::Debug,
    {
        let guard = std::time::Instant::now();
        loop {
            assert!(
                guard.elapsed() < Duration::from_secs(5),
                "fixture stage was not observed"
            );
            let outcome = poll_once(future.as_mut()).await;
            assert!(
                outcome.is_pending(),
                "operation completed before fixture stage: {outcome:?}"
            );
            if observed() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    async fn finish<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) -> F::Output {
        let guard = std::time::Instant::now();
        loop {
            assert!(
                guard.elapsed() < Duration::from_secs(5),
                "operation did not finish after fixture release"
            );
            if let std::task::Poll::Ready(result) = poll_once(future.as_mut()).await {
                return result;
            }
            tokio::task::yield_now().await;
        }
    }

    async fn assert_stage_times_out(workspace: &Path, stage: &str) {
        let marker = hold_stage(workspace, stage);
        let identity = identity();
        let call = super::call_external_daemon(
            workspace,
            &identity,
            "tower_read_file",
            json!({"path": "README.md"}),
            Duration::from_secs(10),
            &NeverCancelled,
        );
        tokio::pin!(call);
        drive_until(call.as_mut(), || marker.with_extension("entered").exists()).await;
        tokio::time::advance(Duration::from_secs(11)).await;
        let error = finish(call.as_mut()).await.expect_err(
            "held stage must exhaust 10-second acquisition budget after advancing 11 seconds",
        );
        std::fs::remove_file(marker).expect("release stage marker");
        assert_availability_timeout(error);
    }

    #[tokio::test(start_paused = true)]
    async fn external_daemon_client_bounds_target_initialization() {
        let workspace = tempfile::tempdir().expect("temporary external workspace");
        let error = connect_or_spawn_with_deadline(
            workspace.path(),
            &identity(),
            "tower_read_file",
            tokio::time::Instant::now(),
            &NeverCancelled,
        )
        .await
        .expect_err("an expired acquisition deadline must not probe, contend, or spawn");
        assert_availability_timeout(error);

        let stalled_socket = tempfile::tempdir().expect("stalled socket workspace");
        assert_stage_times_out(stalled_socket.path(), "socket").await;

        // A missing daemon timeout must not quietly acquire a finite default in
        // the legacy, otherwise-unbounded connect-or-spawn API.
        assert!(
            crate::adapters::config::load(workspace.path())
                .expect("default configuration loads")
                .daemon
                .connection_timeout()
                .is_none()
        );

        // Spawn-owner contention is an acquisition stage.  The eventual GREEN
        // implementation must observe this held lock under the same deadline.
        let guarded = tempfile::tempdir().expect("workspace with held spawn lock");
        let guard = SpawnAttemptGuard::acquire(guarded.path()).expect("hold spawn ownership");
        let contention = connect_or_spawn_with_deadline(
            guarded.path(),
            &identity(),
            "tower_read_file",
            tokio::time::Instant::now() + Duration::from_millis(25),
            &NeverCancelled,
        )
        .await
        .expect_err("held spawn ownership must consume the shared deadline");
        drop(guard);
        assert_availability_timeout(contention);

        let stalled_spawn = tempfile::tempdir().expect("stalled spawn workspace");
        assert_stage_times_out(stalled_spawn.path(), "spawn").await;

        let unavailable = project(None);
        let error = ExternalDaemonClient::new()
            .call(
                &unavailable,
                "tower_read_file",
                json!({"path": "README.md"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect_err("a snapshot without a canonical path is immediately unavailable");
        assert_availability_timeout(error);
        assert!(
            !unavailable.configured_path.exists(),
            "unavailable snapshot must not probe configured path"
        );
        assert!(
            !unavailable.resolved_path.exists(),
            "unavailable snapshot must not probe resolved path"
        );
        std::fs::create_dir_all(&unavailable.configured_path)
            .expect("a later directory appearance must not alter the snapshot");
        let stale_error = ExternalDaemonClient::new()
            .call(
                &unavailable,
                "tower_read_file",
                json!({"path": "README.md"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect_err("stale snapshot without canonical path remains unavailable");
        assert_availability_timeout(stale_error);
        assert!(
            !unavailable
                .configured_path
                .join(".tower/daemon.log")
                .exists(),
            "stale snapshot must not spawn"
        );
        std::fs::remove_dir(&unavailable.configured_path)
            .expect("remove configured-path appearance created by this test");

        for stage in ["handshake", "initialize"] {
            let fixture =
                ExternalDaemonFixture::start(FixtureReply::Result(json!({"unused": true})));
            assert_stage_times_out(fixture._workspace.path(), stage).await;
            fixture.join();
        }

        // A live target proves socket acquisition, handshake, and RMCP
        // initialization all complete within the same availability budget.
        let fixture =
            ExternalDaemonFixture::start(FixtureReply::Result(json!({"content": "ready"})));
        let target = fixture.target();
        let response = tokio::task::spawn_blocking(move || {
            ExternalDaemonClient::new().call(
                &target,
                "tower_read_file",
                json!({"path": "README.md"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
        })
        .await
        .expect("external connector worker must not panic")
        .expect("available target must initialize and answer within the shared deadline");
        fixture.join();
        assert_eq!(response, json!({"content": "ready"}));
    }

    #[tokio::test(start_paused = true)]
    async fn external_read_operation_preserves_absent_budget() {
        let mut fixture = ExternalDaemonFixture::start(FixtureReply::HoldUntilReleased);
        let root = fixture._workspace.path().to_path_buf();
        let identity = identity();
        let call = super::call_external_daemon(
            &root,
            &identity,
            "tower_read_file",
            json!({"path": "held-until-released.txt"}),
            Duration::from_secs(10),
            &NeverCancelled,
        );
        tokio::pin!(call);
        drive_until(call.as_mut(), || fixture.call_started.try_recv().is_ok()).await;
        // Admission proves initialization completed. Advance from this barrier,
        // so even a wrongly reset 10-second execution budget must expire.
        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(
            poll_once(call.as_mut()).await.is_pending(),
            "admitted call must remain pending after advancing 11 seconds beyond a 10-second acquisition budget"
        );
        fixture.release();
        let result = finish(call.as_mut())
            .await
            .expect("released unlimited target call succeeds");
        assert_eq!(
            result,
            json!({"content": "released after acquisition deadline"})
        );
        tokio::task::spawn_blocking(move || fixture.join())
            .await
            .expect("join fixture without blocking asynchronous RMCP cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn external_acquisition_shares_deadline_across_socket_handshake_and_initialize() {
        for final_stage in ["handshake", "initialize"] {
            let fixture =
                ExternalDaemonFixture::start(FixtureReply::Result(json!({"unused": true})));
            let root = fixture._workspace.path().to_path_buf();
            let socket = hold_stage(&root, "socket");
            let handshake = hold_stage(&root, "handshake");
            let initialize = hold_stage(&root, "initialize");
            let identity = identity();
            let call = super::call_external_daemon(
                &root,
                &identity,
                "tower_read_file",
                json!({"path": "README.md"}),
                Duration::from_secs(10),
                &NeverCancelled,
            );
            tokio::pin!(call);
            drive_until(call.as_mut(), || socket.with_extension("entered").exists()).await;
            tokio::time::advance(Duration::from_secs(6)).await;
            std::fs::remove_file(socket).unwrap();
            drive_until(call.as_mut(), || {
                handshake.with_extension("entered").exists()
            })
            .await;
            if final_stage == "initialize" {
                tokio::time::advance(Duration::from_secs(2)).await;
                std::fs::remove_file(handshake).unwrap();
                drive_until(call.as_mut(), || {
                    initialize.with_extension("entered").exists()
                })
                .await;
                tokio::time::advance(Duration::from_secs(3)).await;
            } else {
                tokio::time::advance(Duration::from_secs(5)).await;
            }
            // The total is 11 seconds; the final phase alone consumed only 3/5.
            // A per-phase reset would leave the future pending here.
            let outcome = poll_once(call.as_mut()).await;
            assert!(
                outcome.is_ready(),
                "shared 10-second budget reset at {final_stage} after advancing 11 seconds: {outcome:?}"
            );
            let std::task::Poll::Ready(result) = outcome else {
                unreachable!()
            };
            assert_availability_timeout(result.unwrap_err());
            fixture.join();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_connection_without_budget_waits_for_spawn_ownership() {
        let workspace = tempfile::tempdir().unwrap();
        let guard = SpawnAttemptGuard::acquire(workspace.path()).unwrap();
        let marker = hold_stage(workspace.path(), "contention");
        let call = super::connect_or_spawn(workspace.path());
        tokio::pin!(call);
        // Complete the absent-socket probe and reach the ownership retry loop.
        drive_until(call.as_mut(), || marker.with_extension("entered").exists()).await;
        std::fs::remove_file(marker).unwrap();
        assert!(
            poll_once(call.as_mut()).await.is_pending(),
            "ownership retry must begin before advancing virtual time"
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(
            poll_once(call.as_mut()).await.is_pending(),
            "ordinary acquisition must remain unlimited after 60 virtual seconds"
        );
        let listener =
            tokio::net::UnixListener::bind(workspace.path().join(".tower/daemon.sock")).unwrap();
        drop(guard);
        tokio::time::advance(Duration::from_millis(25)).await;
        let stream = finish(call.as_mut())
            .await
            .expect("ordinary connection recovers after ownership release");
        drop(stream);
        drop(listener);
    }

    #[test]
    fn external_daemon_client_preserves_target_errors_and_cancellation() {
        let cancelled = ToggleCancellation(Arc::new(AtomicBool::new(true)));
        let error = ExternalDaemonClient::new()
            .call(
                &project(Some(PathBuf::from("/must/not/be/probed/when/cancelled"))),
                "tower_read_file",
                json!({"path": "cancelled.txt"}),
                Duration::from_secs(1),
                &cancelled,
            )
            .expect_err("caller cancellation must interrupt external acquisition or execution");

        assert!(matches!(
            error,
            ExternalDaemonError::Cancelled { project, operation }
                if project == identity() && operation == "tower_read_file"
        ));

        // Closing immediately after accept deterministically forces the
        // daemon-wire handshake producer (rather than an RMCP response) to
        // fail.  The source error may contain the sentinel path; Display may
        // not expose it to the routing layer.
        let broken_workspace = tempfile::tempdir().expect("broken handshake workspace");
        let tower_dir = broken_workspace.path().join(".tower");
        std::fs::create_dir(&tower_dir).expect("create broken fixture directory");
        let listener = UnixListener::bind(tower_dir.join("daemon.sock"))
            .expect("bind broken handshake socket");
        std::fs::write(tower_dir.join("fail-handshake"), "fail")
            .expect("install deterministic handshake failure");
        std::fs::write(tower_dir.join("daemon.log"), "fixture log capture\n")
            .expect("initialize captured handshake log");
        let peer = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept broken handshake client");
            stream
                .shutdown(std::net::Shutdown::Both)
                .expect("close broken handshake socket");
        });
        let error = ExternalDaemonClient::new()
            .call(
                &project(Some(broken_workspace.path().to_path_buf())),
                "tower_read_file",
                json!({"path": "RAW_PATH_SECRET"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect_err("closed daemon socket must be a typed handshake failure");
        peer.join().expect("broken handshake peer must finish");
        assert!(
            matches!(error, ExternalDaemonError::Handshake { ref project, .. } if *project == identity())
        );
        let logs = std::fs::read_to_string(tower_dir.join("daemon.log"))
            .expect("read captured handshake log");
        assert!(!error.to_string().contains("RAW_HANDSHAKE_SECRET"));
        assert!(!error.to_string().contains("RAW_PATH_SECRET"));
        assert!(!logs.contains("RAW_HANDSHAKE_SECRET"));
        assert!(!logs.contains("RAW_PATH_SECRET"));

        let fixture = ExternalDaemonFixture::start(FixtureReply::TargetTimeout);
        let target = fixture.target();
        let error = ExternalDaemonClient::new()
            .call(
                &target,
                "tower_read_file",
                json!({"path": "RAW_PATH_SECRET"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect_err("target is_error response must retain typed target failure");
        let logs = fixture.captured_logs();
        fixture.join();
        assert!(
            matches!(error, ExternalDaemonError::TargetCall { ref project, ref operation, ref message }
            if *project == identity()
                && operation == "tower_read_file"
                && !message.contains("RAW_TARGET_SECRET")
                && !message.contains("RAW_PATH_SECRET"))
        );
        assert!(!error.to_string().contains("RAW_TARGET_SECRET"));
        assert!(!error.to_string().contains("RAW_PATH_SECRET"));
        assert!(!format!("{error:?}").contains("RAW_TARGET_SECRET"));
        assert!(!format!("{error:?}").contains("RAW_PATH_SECRET"));
        assert!(!logs.contains("RAW_TARGET_SECRET"));
        assert!(!logs.contains("RAW_PATH_SECRET"));

        let fixture = ExternalDaemonFixture::start(FixtureReply::TargetError);
        let target = fixture.target();
        let error = ExternalDaemonClient::new()
            .call(
                &target,
                "tower_read_file",
                json!({"path": "RAW_PATH_SECRET"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect_err("target JSON-RPC failure must retain a typed target-call failure");
        let logs = fixture.captured_logs();
        fixture.join();
        assert!(
            matches!(error, ExternalDaemonError::TargetCall { ref project, ref operation, ref message }
                if *project == identity()
                    && operation == "tower_read_file"
                    && !message.contains("RAW_TARGET_CALL_SECRET")
                    && !message.contains("RAW_PATH_SECRET"))
        );
        assert!(!error.to_string().contains("RAW_TARGET_CALL_SECRET"));
        assert!(!format!("{error:?}").contains("RAW_TARGET_CALL_SECRET"));
        assert!(!logs.contains("RAW_TARGET_CALL_SECRET"));

        let fixture = ExternalDaemonFixture::start(FixtureReply::InitializeError);
        let target = fixture.target();
        let error = ExternalDaemonClient::new()
            .call(
                &target,
                "tower_read_file",
                json!({"path": "RAW_PATH_SECRET"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect_err("initialization JSON-RPC error must retain typed initialization failure");
        let logs = fixture.captured_logs();
        fixture.join();
        assert!(
            matches!(error, ExternalDaemonError::Initialize { ref project, ref message }
                if *project == identity()
                    && !message.contains("RAW_INITIALIZATION_SECRET")
                    && !message.contains("RAW_PATH_SECRET"))
        );
        assert!(!error.to_string().contains("RAW_INITIALIZATION_SECRET"));
        assert!(!error.to_string().contains("RAW_PATH_SECRET"));
        assert!(!format!("{error:?}").contains("RAW_INITIALIZATION_SECRET"));
        assert!(!format!("{error:?}").contains("RAW_PATH_SECRET"));
        assert!(!logs.contains("RAW_INITIALIZATION_SECRET"));
        assert!(!logs.contains("RAW_PATH_SECRET"));

        let fixture = ExternalDaemonFixture::start(FixtureReply::MalformedResult);
        let target = fixture.target();
        let error = ExternalDaemonClient::new()
            .call(
                &target,
                "tower_read_file",
                json!({"path": "RAW_PATH_SECRET"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect_err("malformed target response must retain typed decode failure");
        let logs = fixture.captured_logs();
        fixture.join();
        assert!(
            matches!(error, ExternalDaemonError::Decode { ref project, ref operation, .. }
            if *project == identity() && operation == "tower_read_file"),
            "expected decode error, got {error:?}"
        );
        assert!(!error.to_string().contains("RAW_PATH_SECRET"));
        assert!(!error.to_string().contains("RAW_DECODE_SECRET"));
        assert!(!logs.contains("RAW_DECODE_SECRET"));
        assert!(!logs.contains("RAW_PATH_SECRET"));

        // Cancellation after tools/call admission must stop the source wait,
        // and a later connection must remain usable rather than inheriting
        // cancellation or poisoned transport state.
        let mut held = ExternalDaemonFixture::start(FixtureReply::HoldUntilReleased);
        let target = held.target();
        let cancellation = ToggleCancellation(Arc::new(AtomicBool::new(false)));
        let cancel_from_test = cancellation.clone();
        let (outcome_tx, outcome_rx) = mpsc::channel();
        thread::spawn(move || {
            outcome_tx
                .send(ExternalDaemonClient::new().call(
                    &target,
                    "tower_read_file",
                    json!({"path": "RAW_CANCELLED_PATH_SECRET"}),
                    Duration::from_secs(1),
                    &cancellation,
                ))
                .expect("report admitted cancellation outcome");
        });
        held.wait_for_call();
        cancel_from_test.0.store(true, Ordering::SeqCst);
        let error = outcome_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("admitted call must observe cancellation")
            .expect_err("admitted call must return Cancelled");
        assert!(matches!(
            error,
            ExternalDaemonError::Cancelled { ref project, ref operation }
                if *project == identity() && operation == "tower_read_file"
        ));
        assert!(!error.to_string().contains("RAW_CANCELLED_PATH_SECRET"));
        held.release();
        held.join();

        let recovered =
            ExternalDaemonFixture::start(FixtureReply::Result(json!({"recovered": true})));
        let target = recovered.target();
        let response = ExternalDaemonClient::new()
            .call(
                &target,
                "tower_read_file",
                json!({"path": "recovery.txt"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect("a subsequent external call must recover after cancellation");
        recovered.join();
        assert_eq!(response, json!({"recovered": true}));
    }

    #[test]
    fn external_daemon_client_does_not_cap_target_response() {
        let payload = "external-response".repeat(128 * 1024);
        let expected = json!({"content": payload});
        let fixture = ExternalDaemonFixture::start(FixtureReply::Result(expected.clone()));
        let target = fixture.target();
        let response = ExternalDaemonClient::new()
            .call(
                &target,
                "tower_read_file",
                json!({"path": "large.txt"}),
                Duration::from_secs(1),
                &NeverCancelled,
            )
            .expect("a valid large target response must be forwarded unchanged");
        fixture.join();

        assert_eq!(response, expected);
        assert!(matches!(response, Value::Object(_)));
        let encoded =
            serde_json::to_vec(&response).expect("response serializes for exact comparison");
        assert_eq!(
            encoded.len(),
            serde_json::to_vec(&expected)
                .expect("expected serializes")
                .len()
        );
        assert_eq!(
            Sha256::digest(&encoded),
            Sha256::digest(serde_json::to_vec(&expected).expect("expected serializes"))
        );
    }
}
