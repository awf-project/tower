//! `SidecarHostAdapter` — `ExtensionInstance` over a real child process.
//!
//! See the module-level doc in `mod.rs` for the architecture overview.
//!
//! # Spec 24 additions (fault model)
//!
//! - **Timeout**: every `send_request` call is bounded by a wall-clock deadline
//!   (`request_timeout`). On timeout the child is killed via `SIGKILL` (Unix) /
//!   `TerminateProcess` (Windows) and `ExtensionFault::Timeout` is returned.
//! - **Crash detection**: when the reader loop reaches EOF (stdout closed) it
//!   stores `SlotState::ChildExited` in the pending slot and notifies the condvar
//!   so the waiting `send_request` wakes up and returns
//!   `ExtensionFault::Crashed{code}`.
//! - **Garbage frames**: undecodable lines in the reader loop cause an error
//!   response to be written into the pending slot so the caller gets
//!   `ExtensionFault::ProtocolError` (existing behaviour, now explicit).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use extension_protocol::envelope::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use extension_protocol::{
    Capability, ExtensionFault, ExtensionManifest, InitParams, InitResult, PROTOCOL_VERSION,
    Response,
};
use serde_json::Value;

use super::host_deps::HostDeps;
use super::path_validation::validate_capability_path;
use crate::adapters::mcp::PushEvent;
use crate::domain::{DomainError, ExtensionInstance, RelativePath};
use crate::ports::inbound::{
    PerFileEditResult, WorkspaceApplyEditsError, WorkspaceApplyEditsErrorCode,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult, WorkspaceEditSpan,
};

// ── Wire helpers ──────────────────────────────────────────────────────────────

/// Serialize a JSON-RPC request to a newline-terminated JSON frame.
fn encode_request(id: u64, method: &str, params: Value) -> String {
    let envelope = JsonRpcRequest {
        jsonrpc: "2.0".to_owned(),
        id: Some(Value::Number(id.into())),
        method: method.to_owned(),
        params: Some(params),
    };
    let mut s = serde_json::to_string(&envelope).expect("serialize JsonRpcRequest");
    s.push('\n');
    s
}

/// Serialize a JSON-RPC success response to a newline-terminated JSON frame.
fn encode_response_ok(id: Value, result: Value) -> String {
    let envelope = JsonRpcResponse {
        jsonrpc: "2.0".to_owned(),
        id,
        result: Some(result),
        error: None,
    };
    let mut s = serde_json::to_string(&envelope).expect("serialize JsonRpcResponse");
    s.push('\n');
    s
}

/// Serialize a JSON-RPC error response to a newline-terminated JSON frame.
fn encode_response_err(id: Value, code: i32, message: String) -> String {
    let envelope = JsonRpcResponse {
        jsonrpc: "2.0".to_owned(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data: None,
        }),
    };
    let mut s = serde_json::to_string(&envelope).expect("serialize JsonRpcResponse error");
    s.push('\n');
    s
}

// ── Pending response slot ─────────────────────────────────────────────────────

/// State of the one-shot pending-response slot (spec 24 — adds `ChildExited`).
#[derive(Debug, Default)]
enum SlotState {
    /// The adapter is waiting for a response.
    #[default]
    Waiting,
    /// The extension replied (success or protocol-level error).
    Response(Result<Value, String>),
    /// The child process exited before responding.
    ///
    /// Carries the exit code if the OS could retrieve it.
    ChildExited(Option<i32>),
}

impl SlotState {
    /// True while the adapter should keep waiting (i.e. `Waiting`).
    fn is_waiting(&self) -> bool {
        matches!(self, SlotState::Waiting)
    }
}

/// One-shot response slot for the synchronous request/response protocol.
///
/// The adapter sets the state to `Waiting` before sending a request. The reader
/// thread transitions it to `Response` or `ChildExited` and notifies the condvar.
#[derive(Debug, Default)]
struct PendingSlot {
    state: SlotState,
}

struct InitializeError {
    fault: ExtensionFault,
    reader_thread: Option<JoinHandle<()>>,
}

impl InitializeError {
    fn before_reader(fault: ExtensionFault) -> Self {
        Self {
            fault,
            reader_thread: None,
        }
    }

    fn after_reader(fault: ExtensionFault, reader_thread: JoinHandle<()>) -> Self {
        Self {
            fault,
            reader_thread: Some(reader_thread),
        }
    }

    fn into_parts(self) -> (ExtensionFault, Option<JoinHandle<()>>) {
        (self.fault, self.reader_thread)
    }
}

// ── SidecarHostAdapter ────────────────────────────────────────────────────────

/// Implements [`ExtensionInstance`] over a child process speaking JSON-RPC 2.0
/// over stdio (spec 23/24).
///
/// # Spawning
///
/// Use [`SidecarHostAdapter::spawn`] to start the process and complete the
/// `initialize` handshake. The returned adapter is ready for `call_tool` /
/// `deliver_event` / `shutdown`.
///
/// # Fault model (spec 24)
///
/// - **Timeout** — wall-clock deadline exceeded → child is killed →
///   [`ExtensionFault::Timeout`].
/// - **Child crashed** — EOF/exit during a call →
///   [`ExtensionFault::Crashed`].
/// - **Bad response** → [`ExtensionFault::ProtocolError`].
/// - **Protocol mismatch** → [`ExtensionFault::ProtocolError`] from `spawn`.
pub struct SidecarHostAdapter {
    /// Cached manifest returned by the extension at `initialize` time.
    manifest: ExtensionManifest,
    /// Serial request counter. Always incremented before sending.
    next_id: u64,
    /// Shared write end of child stdin — also used by the reader thread for
    /// capability responses. Protected by a mutex to serialize all writes.
    stdin: Arc<Mutex<ChildStdin>>,
    /// Response slot shared with the reader thread.
    pending: Arc<(Mutex<PendingSlot>, Condvar)>,
    /// Child process handle (kept alive until `shutdown`).
    child: Child,
    /// Reader thread handle. Joined in `shutdown`.
    reader_thread: Option<JoinHandle<()>>,
    /// Per-request wall-clock deadline (spec 24 U1).
    request_timeout: Duration,
}

impl std::fmt::Debug for SidecarHostAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarHostAdapter")
            .field("manifest_name", &self.manifest.name)
            .field("next_id", &self.next_id)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl SidecarHostAdapter {
    /// Spawn the extension described by `manifest` and complete the `initialize`
    /// handshake.
    ///
    /// # Errors
    ///
    /// - `ExtensionFault::ProtocolError` — spawn failed, handshake failed, or
    ///   the extension's `PROTOCOL_VERSION` does not match (UN1).
    /// - `ExtensionFault::Crashed` — the process exited before responding.
    ///
    /// # Arguments
    ///
    /// - `manifest` — the extension's manifest; `manifest.command` is the argv.
    /// - `deps` — existing outbound ports for capability dispatch.
    /// - `request_timeout` — wall-clock deadline for each request (spec 24 U1).
    ///   Pass `Duration::from_secs(30)` for the default.
    pub fn spawn(
        manifest: ExtensionManifest,
        deps: HostDeps,
        request_timeout: Duration,
        extension_config: Option<Value>,
    ) -> Result<Box<dyn ExtensionInstance>, ExtensionFault> {
        Self::spawn_with_config(manifest, deps, request_timeout, extension_config)
    }

    /// Spawn the extension and include host-provided extension configuration in
    /// the `initialize` request.
    pub fn spawn_with_config(
        manifest: ExtensionManifest,
        deps: HostDeps,
        request_timeout: Duration,
        extension_config: Option<Value>,
    ) -> Result<Box<dyn ExtensionInstance>, ExtensionFault> {
        let argv = &manifest.command;
        if argv.is_empty() {
            return Err(ExtensionFault::ProtocolError {
                message: "manifest.command is empty".to_owned(),
            });
        }

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        // Tell the extension where the workspace is. The host knows the workspace
        // root (--workspace-dir) but the extension is a separate process: without
        // this it would fall back to its own current_dir, which is wherever the
        // host happened to be launched — so e.g. the LSP extension would read the
        // wrong `.tower/config.toml`, find no language servers, and report every
        // request unsupported. We export `TOWER_WORKSPACE` (the extension's
        // primary source) and also set the child cwd for any cwd-relative fallback.
        // `argv[0]` is already an absolute path (resolved against the extension
        // dir in discovery), so changing cwd does not affect locating the binary.
        if let Some(ws) = deps.fs.workspace_root() {
            cmd.env("TOWER_WORKSPACE", &ws).current_dir(&ws);
        }

        let mut child = cmd.spawn().map_err(|e| ExtensionFault::ProtocolError {
            message: format!("failed to spawn '{}': {e}", argv[0]),
        })?;

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<(Mutex<PendingSlot>, Condvar)> =
            Arc::new((Mutex::new(PendingSlot::default()), Condvar::new()));

        // Perform the initialize handshake before starting the reader loop.
        // We read directly from stdout here (single-threaded, before reader starts).
        let (init_result, reader_thread) = match Self::do_initialize(
            &stdin,
            stdout,
            Arc::clone(&pending),
            deps,
            &manifest,
            request_timeout,
            extension_config,
        ) {
            Ok(initialized) => initialized,
            Err(err) => {
                let (fault, reader_thread) = err.into_parts();
                Self::cleanup_failed_initialize(&mut child, reader_thread);
                return Err(fault);
            }
        };

        // Reconstruct the manifest using the extension's declared tools/events/capabilities.
        let final_manifest = ExtensionManifest {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            command: manifest.command.clone(),
            activation: manifest.activation.clone(),
            tools: init_result.tools,
            events: extension_protocol::manifest::EventsSection {
                subscribe: init_result.events,
            },
            capabilities: extension_protocol::manifest::CapabilitiesSection {
                required: init_result
                    .capabilities
                    .iter()
                    .filter(|capability| capability_allowed_for_manifest(&manifest, capability))
                    .map(capability_to_str)
                    .map(str::to_owned)
                    .collect(),
            },
        };

        Ok(Box::new(Self {
            manifest: final_manifest,
            next_id: 1,
            stdin,
            pending,
            child,
            reader_thread: Some(reader_thread),
            request_timeout,
        }))
    }

    /// Perform the `initialize` handshake synchronously.
    ///
    /// Sends `InitParams`, starts the reader thread (which will serve capability
    /// callbacks inline), and waits for the `Initialized` response.
    ///
    /// Returns the [`InitResult`] together with the reader thread's
    /// [`JoinHandle`] so the caller can store it for proper shutdown.
    #[allow(clippy::too_many_arguments)]
    fn do_initialize(
        stdin: &Arc<Mutex<ChildStdin>>,
        stdout: ChildStdout,
        pending: Arc<(Mutex<PendingSlot>, Condvar)>,
        deps: HostDeps,
        manifest: &ExtensionManifest,
        request_timeout: Duration,
        extension_config: Option<Value>,
    ) -> Result<(InitResult, JoinHandle<()>), InitializeError> {
        // Send the initialize request (id = 0).
        let params = serde_json::to_value(InitParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: "tower-host/0.1.0".to_owned(),
            extension_config,
        })
        .map_err(|e| {
            InitializeError::before_reader(ExtensionFault::ProtocolError {
                message: format!("serialize InitParams: {e}"),
            })
        })?;

        {
            let frame = encode_request(0, "initialize", params);
            let mut guard = stdin.lock().map_err(|_| {
                InitializeError::before_reader(ExtensionFault::ProtocolError {
                    message: "stdin mutex poisoned".to_owned(),
                })
            })?;
            guard.write_all(frame.as_bytes()).map_err(|e| {
                InitializeError::before_reader(ExtensionFault::ProtocolError {
                    message: format!("write initialize: {e}"),
                })
            })?;
        }

        // Set up the pending slot for id=0 before starting the reader.
        {
            let (lock, _) = &*pending;
            let mut slot = lock.lock().unwrap_or_else(|p| p.into_inner());
            slot.state = SlotState::Waiting;
        }

        // Start the reader thread. It will run the capability dispatch loop.
        let stdin_clone = Arc::clone(stdin);
        let pending_clone = Arc::clone(&pending);
        let allowed_caps = allowed_capabilities_for_manifest(manifest);

        let reader = thread::spawn(move || {
            reader_loop(
                BufReader::new(stdout),
                stdin_clone,
                pending_clone,
                Arc::new(deps),
                allowed_caps,
            );
        });

        // Wait for the Initialized response (id=0) — with timeout (spec 24 U1).
        // We do NOT have a `Child` handle here so we cannot kill on timeout during
        // initialize; instead we return `Timeout` and the caller must drop/kill.
        let raw = match Self::wait_response_timed(&pending, request_timeout, None) {
            Ok(raw) => raw,
            Err(fault) => return Err(InitializeError::after_reader(fault, reader)),
        };

        // Parse the Response from the result value.
        let response: Response = match serde_json::from_value(raw) {
            Ok(response) => response,
            Err(e) => {
                return Err(InitializeError::after_reader(
                    ExtensionFault::ProtocolError {
                        message: format!("parse Initialized response: {e}"),
                    },
                    reader,
                ));
            }
        };

        match response {
            Response::Initialized(init_result) => {
                // UN1: The extension validated PROTOCOL_VERSION on its side and
                // would have sent Response::Error if mismatched; we catch that
                // in the arm below. The host's assertion is implicit: only an
                // extension that accepted our advertised PROTOCOL_VERSION reaches
                // this arm.
                Ok((init_result, reader))
            }
            Response::Error(err) => Err(InitializeError::after_reader(
                ExtensionFault::ProtocolError {
                    message: format!("initialize rejected: {} (code {})", err.message, err.code),
                },
                reader,
            )),
            other => Err(InitializeError::after_reader(
                ExtensionFault::ProtocolError {
                    message: format!("expected Initialized, got: {other:?}"),
                },
                reader,
            )),
        }
    }

    fn cleanup_failed_initialize(child: &mut Child, reader_thread: Option<JoinHandle<()>>) {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(handle) = reader_thread {
            let _ = handle.join();
        }
    }

    /// Send a JSON-RPC request and wait for the response.
    ///
    /// On timeout the child is killed and `ExtensionFault::Timeout` is returned
    /// (spec 24 UN1). On EOF/exit `ExtensionFault::Crashed` is returned (UN2).
    fn send_request(&mut self, method: &str, params: Value) -> Result<Value, ExtensionFault> {
        let id = self.next_id;
        self.next_id += 1;

        // Clear the pending slot before sending.
        {
            let (lock, _) = &*self.pending;
            let mut slot = lock.lock().unwrap_or_else(|p| p.into_inner());
            slot.state = SlotState::Waiting;
        }

        let frame = encode_request(id, method, params);
        {
            let mut guard = self
                .stdin
                .lock()
                .map_err(|_| ExtensionFault::ProtocolError {
                    message: "stdin mutex poisoned".to_owned(),
                })?;
            guard
                .write_all(frame.as_bytes())
                .map_err(|_| ExtensionFault::Crashed { code: None })?;
        }

        Self::wait_response_timed(&self.pending, self.request_timeout, Some(&mut self.child))
    }

    /// Block until the pending slot contains a response (or deadline exceeded).
    ///
    /// # Timeout (spec 24 UN1)
    ///
    /// Uses `Condvar::wait_timeout_while` so the wait is bounded. If the
    /// deadline is reached before a response arrives:
    ///   1. The child is killed via `child.kill()`.
    ///   2. `ExtensionFault::Timeout` is returned.
    ///
    /// # Crash (spec 24 UN2)
    ///
    /// If the reader thread signals `SlotState::ChildExited`, the exit code is
    /// extracted and `ExtensionFault::Crashed { code }` is returned.
    ///
    /// # `child` parameter
    ///
    /// Pass `Some(&mut child)` to enable kill-on-timeout. Pass `None` during
    /// `do_initialize` where the `Child` handle is not yet stored in `self`.
    fn wait_response_timed(
        pending: &Arc<(Mutex<PendingSlot>, Condvar)>,
        timeout: Duration,
        child: Option<&mut Child>,
    ) -> Result<Value, ExtensionFault> {
        let (lock, cvar) = &**pending;
        let guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        let (mut guard, timed_out) = cvar
            .wait_timeout_while(guard, timeout, |slot| slot.state.is_waiting())
            .unwrap_or_else(|p| p.into_inner());

        if timed_out.timed_out() {
            // Kill the child to reclaim its resources (spec 24 UN1).
            if let Some(c) = child {
                let _ = c.kill();
                let _ = c.wait(); // reap zombie
            }
            return Err(ExtensionFault::Timeout);
        }

        // Extract the state.
        match std::mem::replace(&mut guard.state, SlotState::Waiting) {
            SlotState::Response(Ok(v)) => Ok(v),
            SlotState::Response(Err(msg)) => Err(ExtensionFault::ProtocolError { message: msg }),
            SlotState::ChildExited(code) => Err(ExtensionFault::Crashed { code }),
            SlotState::Waiting => {
                // Should not happen: condvar woke but state is still Waiting.
                Err(ExtensionFault::ProtocolError {
                    message: "condvar woke spuriously with Waiting state".to_owned(),
                })
            }
        }
    }
}

impl ExtensionInstance for SidecarHostAdapter {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn call_tool(&mut self, name: &str, params: Value) -> Result<Value, ExtensionFault> {
        // Build the flat wire params with the tool name and original params.
        let flat_params = serde_json::json!({"name": name, "params": params});

        let raw = self.send_request("invokeTool", flat_params)?;

        let response: Response =
            serde_json::from_value(raw).map_err(|e| ExtensionFault::ProtocolError {
                message: format!("parse ToolResult: {e}"),
            })?;

        match response {
            Response::ToolResult(v) => Ok(v),
            Response::Error(err) => Err(ExtensionFault::ProtocolError {
                message: format!("tool error: {} (code {})", err.message, err.code),
            }),
            other => Err(ExtensionFault::ProtocolError {
                message: format!("expected ToolResult, got: {other:?}"),
            }),
        }
    }

    fn deliver_event(&mut self, event: extension_protocol::Event) -> Result<(), ExtensionFault> {
        let params = serde_json::to_value(&event).map_err(|e| ExtensionFault::ProtocolError {
            message: format!("serialize Event: {e}"),
        })?;

        let raw = self.send_request("deliverEvent", params)?;

        let response: Response =
            serde_json::from_value(raw).map_err(|e| ExtensionFault::ProtocolError {
                message: format!("parse deliverEvent response: {e}"),
            })?;

        match response {
            Response::Ack => Ok(()),
            Response::Error(err) => Err(ExtensionFault::ProtocolError {
                message: format!("deliverEvent error: {}", err.message),
            }),
            other => Err(ExtensionFault::ProtocolError {
                message: format!("expected Ack, got: {other:?}"),
            }),
        }
    }

    fn shutdown(&mut self) {
        // ── Step 1: send `shutdown` JSON-RPC request (best-effort). ─────────
        // The process may have already exited — all write errors are ignored.
        let id = self.next_id;
        self.next_id += 1;
        let frame = encode_request(id, "shutdown", Value::Null);

        if let Ok(mut guard) = self.stdin.lock() {
            let _ = guard.write_all(frame.as_bytes());
            // Drop the lock so the reader thread can unblock.
        }

        // ── Step 2: wait up to SHUTDOWN_GRACE_MS for the child to exit. ─────
        // We approximate a timed join: poll child.try_wait() in a loop.
        // Once the child exits, the reader thread will exit too (EOF on stdout).
        const SHUTDOWN_GRACE_MS: u64 = 2_000;
        let deadline = std::time::Instant::now() + Duration::from_millis(SHUTDOWN_GRACE_MS);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break, // Child exited cleanly.
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => break, // Deadline elapsed or OS error — escalate.
            }
        }

        // ── Step 3: SIGTERM — if the child is still running. ─────────────────
        // On Unix, send SIGTERM via the platform `kill` utility (safe, no
        // `unsafe` code needed). On non-Unix, skip straight to SIGKILL.
        if matches!(self.child.try_wait(), Ok(None)) {
            send_sigterm(self.child.id());

            // Give SIGTERM another grace period before SIGKILL.
            const SIGTERM_GRACE_MS: u64 = 2_000;
            let sigterm_deadline =
                std::time::Instant::now() + Duration::from_millis(SIGTERM_GRACE_MS);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < sigterm_deadline => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    _ => break,
                }
            }
        }

        // ── Step 4: SIGKILL — unconditional reap. ────────────────────────────
        // `Child::kill()` sends SIGKILL (Unix) / TerminateProcess (Windows).
        // If the child has already exited this is a harmless no-op.
        let _ = self.child.kill();
        let _ = self.child.wait();

        // ── Step 5: join the reader thread — only now that the child is dead. ─
        // The child has been reaped, so its stdout write-end is closed; the
        // reader loop observes EOF and terminates, making this join guaranteed
        // finite. Joining BEFORE the kill escalation would deadlock: a child
        // that ignores `shutdown` keeps stdout open, the reader blocks forever
        // in `read`, and `join()` never returns (the kill steps above are then
        // never reached). Regression: `shutdown_reaps_extension_that_ignores_shutdown_request`.
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

// ── Signal helpers ────────────────────────────────────────────────────────────

/// Send SIGTERM to the process with the given PID (spec 25 EV2 escalation).
///
/// On Unix we invoke the platform `kill -TERM <pid>` command — this avoids any
/// `unsafe` code while still delivering the correct signal. The exit status of
/// `kill` is ignored because the process may have already exited between the
/// `try_wait` check and this call (harmless race).
///
/// On non-Unix platforms (Windows) this is a no-op: the caller will proceed to
/// `Child::kill()` (SIGKILL / `TerminateProcess`) immediately.
fn send_sigterm(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
    // non-Unix: no-op — caller escalates to SIGKILL directly.
    #[cfg(not(unix))]
    let _ = pid;
}

// ── Reader loop ───────────────────────────────────────────────────────────────

/// Background reader thread: dispatches incoming frames from the extension.
///
/// Frames are JSON-RPC envelopes. Two kinds are possible:
///
/// 1. **Response** (the extension is answering a host request): has `result` or
///    `error`. Matched by `id` and stored in the `PendingSlot`.
/// 2. **HostCall** (the extension is requesting a host capability): has `method`
///    and `id`. Dispatched to ports; the response is written back to child stdin.
///
/// # EOF / crash (spec 24 UN2)
///
/// When the loop exits (EOF on stdout or I/O error) it stores
/// `SlotState::ChildExited` in the slot and notifies the condvar so any
/// thread waiting in `wait_response_timed` wakes immediately with a
/// `Crashed` fault instead of blocking until the timeout.
///
/// # Garbage frames (spec 24 UN3)
///
/// Non-JSON lines are logged to stderr. They do NOT signal `ChildExited`
/// because the process may still be running — the caller gets a
/// `ProtocolError` via the normal error path when the pending slot is
/// eventually fulfilled by the next valid (error) response. However, if
/// a garbage frame is the only response to an outstanding request (the
/// process then exits), the `ChildExited` path catches it.
fn reader_loop(
    reader: BufReader<ChildStdout>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<(Mutex<PendingSlot>, Condvar)>,
    deps: Arc<HostDeps>,
    allowed_caps: Vec<Capability>,
) {
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }

        let envelope: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // Garbage frame — log and store a ProtocolError in the slot so
                // the waiting caller wakes up rather than blocking to timeout.
                eprintln!("[tower/sidecar] parse error: {e}: {line}");
                let (lock, cvar) = &*pending;
                let mut slot = lock.lock().unwrap_or_else(|p| p.into_inner());
                if slot.state.is_waiting() {
                    slot.state = SlotState::Response(Err(format!(
                        "undecodable frame from extension: {line}"
                    )));
                    cvar.notify_one();
                }
                continue;
            }
        };

        let has_result = envelope.get("result").is_some();
        let has_error = envelope.get("error").is_some();
        let has_method = envelope.get("method").is_some();

        if has_result || has_error {
            // This is a response to a host-initiated request.
            let result = if has_result {
                Ok(envelope["result"].clone())
            } else {
                let err_msg = envelope["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_owned();
                Err(err_msg)
            };

            let (lock, cvar) = &*pending;
            let mut slot = lock.lock().unwrap_or_else(|p| p.into_inner());
            slot.state = SlotState::Response(result);
            cvar.notify_one();
        } else if has_method {
            // This is a HostCall request from the extension.
            let method = envelope["method"].as_str().unwrap_or("").to_owned();
            let call_id = envelope.get("id").cloned().unwrap_or(Value::Null);
            let params = envelope.get("params").cloned().unwrap_or(Value::Null);

            let response_frame = dispatch_host_call(&method, call_id, params, &deps, &allowed_caps);
            if let Ok(mut guard) = stdin.lock() {
                let _ = guard.write_all(response_frame.as_bytes());
            }
        }
    }

    // EOF or I/O error — signal the waiter so it wakes as Crashed rather than
    // spinning until the deadline (spec 24 UN2 fast path).
    let (lock, cvar) = &*pending;
    let mut slot = lock.lock().unwrap_or_else(|p| p.into_inner());
    if slot.state.is_waiting() {
        slot.state = SlotState::ChildExited(None);
        cvar.notify_one();
    }
}

// ── Capability dispatcher ─────────────────────────────────────────────────────

/// Dispatch a `HostCall` from the extension to the appropriate port.
///
/// Returns a newline-terminated JSON-RPC response frame to write back to the
/// extension's stdin.
///
/// Path validation (UN2) is enforced here for `readFile` and `requestFormat`.
fn dispatch_host_call(
    method: &str,
    call_id: Value,
    params: Value,
    deps: &HostDeps,
    allowed_caps: &[Capability],
) -> String {
    match method {
        "workspace/readFile" => {
            if !allowed_caps.contains(&Capability::ReadFile) {
                return encode_response_err(
                    call_id,
                    -32603,
                    "capability 'read_file' not declared".to_owned(),
                );
            }
            let path = params["path"].as_str().unwrap_or("").to_owned();
            match validate_capability_path(&path) {
                Err(e) => encode_response_err(call_id, -32602, e),
                Ok(()) => match deps.fs.read_file(&path) {
                    Ok(bytes) => {
                        // Return bytes as a UTF-8 string (lossy) for JSON transport.
                        let content = String::from_utf8_lossy(&bytes).into_owned();
                        encode_response_ok(call_id, Value::String(content))
                    }
                    Err(e) => encode_response_err(call_id, -32603, e),
                },
            }
        }
        "workspace/listFiles" => {
            if !allowed_caps.contains(&Capability::ListFiles) {
                return encode_response_err(
                    call_id,
                    -32603,
                    "capability 'list_files' not declared".to_owned(),
                );
            }
            let files = deps.fs.list_files();
            let arr: Vec<Value> = files.into_iter().map(Value::String).collect();
            encode_response_ok(call_id, Value::Array(arr))
        }
        "index/get" => {
            if !allowed_caps.contains(&Capability::IndexGet) {
                return encode_response_err(
                    call_id,
                    -32603,
                    "capability 'index_get' not declared".to_owned(),
                );
            }
            let key = params["key"].as_str().unwrap_or("").to_owned();
            match deps.ast_index.get(&key) {
                Ok(Some(bytes)) => {
                    let arr: Vec<Value> =
                        bytes.into_iter().map(|b| Value::Number(b.into())).collect();
                    encode_response_ok(call_id, Value::Array(arr))
                }
                Ok(None) => encode_response_ok(call_id, Value::Null),
                Err(e) => encode_response_err(call_id, -32603, format!("index/get: {e}")),
            }
        }
        "index/put" => {
            if !allowed_caps.contains(&Capability::IndexPut) {
                return encode_response_err(
                    call_id,
                    -32603,
                    "capability 'index_put' not declared".to_owned(),
                );
            }
            let key = params["key"].as_str().unwrap_or("").to_owned();
            // Bytes may arrive as an array of numbers or a base64 string.
            // For simplicity, we support an array of u8 values.
            let bytes: Vec<u8> = if let Some(arr) = params["bytes"].as_array() {
                arr.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect()
            } else {
                Vec::new()
            };
            match deps.ast_index.put(&key, &bytes) {
                Ok(()) => encode_response_ok(call_id, Value::Bool(true)),
                Err(e) => encode_response_err(call_id, -32603, format!("index/put: {e}")),
            }
        }
        "workspace/requestFormat" => {
            if !allowed_caps.contains(&Capability::RequestFormat) {
                return encode_response_err(
                    call_id,
                    -32603,
                    "capability 'request_format' not declared".to_owned(),
                );
            }
            let path = params["path"].as_str().unwrap_or("").to_owned();
            match validate_capability_path(&path) {
                Err(e) => encode_response_err(call_id, -32602, e),
                Ok(()) => {
                    use crate::adapters::formatter::EnqueueResult;
                    match deps.format_queue.enqueue(path.clone()) {
                        EnqueueResult::Accepted => {
                            encode_response_ok(call_id, Value::String("accepted".to_owned()))
                        }
                        EnqueueResult::Dropped => {
                            encode_response_ok(call_id, Value::String("dropped".to_owned()))
                        }
                    }
                }
            }
        }
        "workspace/applyEdits" => {
            if !allowed_caps.contains(&Capability::RequestApplyEdits) {
                return encode_response_err(
                    call_id,
                    -32603,
                    "capability 'request_apply_edits' not declared".to_owned(),
                );
            }
            let decoded = match decode_workspace_apply_edits_request(params) {
                Ok(decoded) => decoded,
                Err(e) => return encode_response_err(call_id, -32602, e),
            };
            let (request, invalid_path_results) =
                split_invalid_path_apply_edits_results(decoded.request);
            if request.edits.is_empty() && !invalid_path_results.is_empty() {
                let result = WorkspaceApplyEditsResult {
                    files_changed: 0,
                    per_file: invalid_path_results,
                };
                return match serde_json::to_value(result) {
                    Ok(value) => encode_response_ok(call_id, value),
                    Err(e) => encode_response_err(
                        call_id,
                        -32603,
                        format!("apply_edits serialization failed: {e}"),
                    ),
                };
            }

            match deps.apply_edits.apply_batch_edits(request.clone()) {
                Ok(mut result) => {
                    result.per_file.extend(invalid_path_results);
                    match encode_workspace_apply_edits_result(
                        &request,
                        result,
                        decoded.legacy_path.as_deref(),
                    ) {
                        Ok(value) => encode_response_ok(call_id, value),
                        Err(e) => encode_response_err(
                            call_id,
                            -32603,
                            format!("apply_edits serialization failed: {e}"),
                        ),
                    }
                }
                Err(e) => encode_apply_edits_error(call_id, e),
            }
        }
        "log" => {
            // Log capability: emit to stderr (host-side, structured).
            // No capability check — log is always allowed.
            let level = params["level"].as_str().unwrap_or("info");
            let msg = params["msg"].as_str().unwrap_or("");
            eprintln!("[tower/ext] [{level}] {msg}");
            encode_response_ok(call_id, Value::Bool(true))
        }
        "notify/resourceUpdated" => {
            // Push path (spec 27 O1): forward to the MCP transport's push channel.
            // No round-trip blocking — the extension sends this and continues.
            // `Notify` capability must be declared; otherwise reject.
            if !allowed_caps.contains(&Capability::Notify) {
                return encode_response_err(
                    call_id,
                    -32603,
                    "capability 'notify' not declared".to_owned(),
                );
            }
            let uri = params["uri"].as_str().unwrap_or("").to_owned();
            if let Some(ref tx) = deps.push_tx {
                // generation: 0 means "unknown" (the extension doesn't track it).
                let _ = tx.send(PushEvent { uri, generation: 0 });
            }
            encode_response_ok(call_id, Value::Bool(true))
        }
        other => encode_response_err(
            call_id,
            -32601,
            format!("unknown host capability method: {other}"),
        ),
    }
}

struct DecodedWorkspaceApplyEditsRequest {
    request: WorkspaceApplyEditsRequest,
    legacy_path: Option<String>,
}

fn decode_workspace_apply_edits_request(
    params: Value,
) -> Result<DecodedWorkspaceApplyEditsRequest, String> {
    validate_raw_base_hash_fields(&params)?;
    let legacy_path = params
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let params = normalize_workspace_apply_edits_params(params)?;
    let request: extension_protocol::WorkspaceApplyEditsRequest = serde_json::from_value(params)
        .map_err(|e| format!("invalid_params: workspace/applyEdits params are malformed: {e}"))?;
    validate_required_base_hashes(&request)?;
    let edits = request
        .edits
        .into_iter()
        .map(|edit| {
            validate_base_hash(edit.base_hash.as_deref())?;
            Ok(WorkspaceEditSpan {
                path: RelativePath::new(edit.path),
                start_byte: edit.start_byte,
                end_byte: edit.end_byte,
                replacement: edit.replacement,
                base_hash: edit.base_hash,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(DecodedWorkspaceApplyEditsRequest {
        request: WorkspaceApplyEditsRequest {
            edits,
            dry_run: request.dry_run,
        },
        legacy_path,
    })
}

fn encode_workspace_apply_edits_result(
    request: &WorkspaceApplyEditsRequest,
    result: WorkspaceApplyEditsResult,
    legacy_path: Option<&str>,
) -> Result<Value, serde_json::Error> {
    let Some(legacy_path) = legacy_path else {
        return serde_json::to_value(result);
    };
    let Some(file) = result
        .per_file
        .iter()
        .find(|file| file.path.as_str() == legacy_path)
    else {
        return serde_json::to_value(result);
    };

    let edits = request
        .edits
        .iter()
        .filter(|edit| edit.path.as_str() == legacy_path)
        .map(|edit| {
            serde_json::json!({
                "start_byte": edit.start_byte,
                "end_byte": edit.end_byte,
                "replacement": edit.replacement,
            })
        })
        .collect::<Vec<_>>();
    let applied = if file.applied {
        edits.clone()
    } else {
        Vec::new()
    };
    let skipped = if file.error.is_some() {
        edits
            .iter()
            .map(|edit| serde_json::json!({ "edit": edit }))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let preview = file.preview.as_ref().map(|preview_content| {
        serde_json::json!({
            "edits": edits,
            "preview_content": preview_content,
        })
    });

    serde_json::to_value(serde_json::json!({
        "applied": applied,
        "skipped": skipped,
        "new_version": file.new_version,
        "preview": preview,
    }))
}

fn normalize_workspace_apply_edits_params(mut params: Value) -> Result<Value, String> {
    let Value::Object(object) = &mut params else {
        return Err("invalid_params: workspace/applyEdits params must be an object".to_owned());
    };
    let path = object.get("path").cloned();
    let expected_version = object.get("expected_version").cloned();

    if path.is_none() && expected_version.is_none() {
        return Ok(params);
    }

    let Some(Value::Array(edits)) = object.get_mut("edits") else {
        return Ok(params);
    };

    for edit in edits {
        let Value::Object(edit_object) = edit else {
            continue;
        };
        if !edit_object.contains_key("path")
            && let Some(path) = path.clone()
        {
            edit_object.insert("path".to_owned(), path);
        }
        if !edit_object.contains_key("base_hash")
            && let Some(expected_version) = expected_version.clone()
        {
            edit_object.insert("base_hash".to_owned(), expected_version);
        }
    }

    Ok(params)
}

fn split_invalid_path_apply_edits_results(
    request: WorkspaceApplyEditsRequest,
) -> (WorkspaceApplyEditsRequest, Vec<PerFileEditResult>) {
    let mut valid_edits = Vec::new();
    let mut invalid_results = Vec::new();

    for edit in request.edits {
        match validate_capability_path(edit.path.as_str()) {
            Ok(()) => valid_edits.push(edit),
            Err(message) => invalid_results.push(PerFileEditResult {
                path: edit.path.clone(),
                applied: false,
                edits_applied: 0,
                edits_skipped: 1,
                new_version: None,
                preview: None,
                error: Some(WorkspaceApplyEditsError {
                    code: WorkspaceApplyEditsErrorCode::InvalidPath,
                    message,
                    path: Some(edit.path),
                }),
            }),
        }
    }

    (
        WorkspaceApplyEditsRequest {
            edits: valid_edits,
            dry_run: request.dry_run,
        },
        invalid_results,
    )
}

fn encode_apply_edits_error(call_id: Value, error: DomainError) -> String {
    match error {
        DomainError::VersionConflict { .. } => {
            encode_response_err(call_id, -32009, format!("precondition_failed: {error}"))
        }
        DomainError::InvalidRange(_) => {
            encode_response_err(call_id, -32602, format!("invalid_range: {error}"))
        }
        other => encode_response_err(call_id, -32603, format!("apply_edits: {other}")),
    }
}

fn validate_raw_base_hash_fields(params: &Value) -> Result<(), String> {
    let object = match params {
        Value::Object(object) => object,
        _ => {
            return Err("invalid_params: workspace/applyEdits params must be an object".to_owned());
        }
    };
    let Some(Value::Array(edits)) = object.get("edits") else {
        return Ok(());
    };
    if let Some(expected_version) = object.get("expected_version")
        && !expected_version.is_string()
    {
        return Err("invalid_params: base_hash must be a string when present".to_owned());
    }
    for edit in edits {
        let Some(base_hash) = edit.get("base_hash") else {
            continue;
        };
        if !base_hash.is_string() {
            return Err("invalid_params: base_hash must be a string when present".to_owned());
        }
    }
    Ok(())
}

fn validate_base_hash(base_hash: Option<&str>) -> Result<(), String> {
    let Some(base_hash) = base_hash else {
        return Ok(());
    };
    if base_hash.len() != 64 || !base_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(
            "invalid_params: base_hash must be a 64-character SHA-256 hex string".to_owned(),
        );
    }
    Ok(())
}

fn validate_required_base_hashes(
    request: &extension_protocol::WorkspaceApplyEditsRequest,
) -> Result<(), String> {
    if request.dry_run == Some(true) {
        return Ok(());
    }
    if request.edits.iter().any(|edit| edit.base_hash.is_none()) {
        return Err(
            "invalid_params: base_hash is required for every mutating workspace/applyEdits edit"
                .to_owned(),
        );
    }
    Ok(())
}

// ── Capability string helpers ─────────────────────────────────────────────────

fn capability_to_str(cap: &Capability) -> &'static str {
    match cap {
        Capability::ReadFile => "read_file",
        Capability::ListFiles => "list_files",
        Capability::IndexGet => "index_get",
        Capability::IndexPut => "index_put",
        Capability::RequestFormat => "request_format",
        Capability::RequestApplyEdits => "request_apply_edits",
        Capability::Log => "log",
        Capability::Notify => "notify",
    }
}

fn str_to_capability(s: &str) -> Option<Capability> {
    match s {
        "read_file" => Some(Capability::ReadFile),
        "list_files" => Some(Capability::ListFiles),
        "index_get" => Some(Capability::IndexGet),
        "index_put" => Some(Capability::IndexPut),
        "request_format" => Some(Capability::RequestFormat),
        "request_apply_edits" => Some(Capability::RequestApplyEdits),
        "log" => Some(Capability::Log),
        "notify" => Some(Capability::Notify),
        _ => None,
    }
}

fn allowed_capabilities_for_manifest(manifest: &ExtensionManifest) -> Vec<Capability> {
    manifest
        .capabilities
        .required
        .iter()
        .filter_map(|s| str_to_capability(s))
        .filter(|capability| capability_allowed_for_manifest(manifest, capability))
        .collect()
}

fn capability_allowed_for_manifest(manifest: &ExtensionManifest, capability: &Capability) -> bool {
    !matches!(capability, Capability::RequestApplyEdits)
        || is_trusted_apply_edits_extension(manifest)
}

fn is_trusted_apply_edits_extension(manifest: &ExtensionManifest) -> bool {
    let expected_binary = match manifest.name.as_str() {
        "ast" => "ast_extension",
        "lsp" => "lsp_extension",
        "lint" => "lint_extension",
        _ => return false,
    };
    #[cfg(test)]
    if manifest
        .command
        .first()
        .is_some_and(|command| command == "python3")
    {
        return true;
    }
    manifest
        .command
        .first()
        .and_then(|command| Path::new(command).file_name())
        .and_then(|name| name.to_str())
        == Some(expected_binary)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::mpsc;

    use serde_json::Value;
    use serde_json::json;

    use super::{HostDeps, dispatch_host_call};
    use crate::adapters::extension::host_deps::ApplyEditsHostPort;
    use crate::adapters::formatter::NoOpFormatQueue;
    use crate::adapters::mcp::PushEvent;
    use crate::adapters::{InMemoryAstIndex, InMemoryFs};
    use crate::domain::{DomainError, RelativePath};
    use crate::ports::inbound::{
        PerFileEditResult, WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult, WorkspaceEditSpan,
    };
    use extension_protocol::Capability;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum RecordedApplyEditsCall {
        ApplyBatch(WorkspaceApplyEditsRequest),
    }

    struct RecordingApplyEditsHost {
        calls: Mutex<Vec<RecordedApplyEditsCall>>,
        result: WorkspaceApplyEditsResult,
    }

    impl RecordingApplyEditsHost {
        fn new(result: WorkspaceApplyEditsResult) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result,
            }
        }

        fn calls(&self) -> Vec<RecordedApplyEditsCall> {
            self.calls.lock().expect("call recorder lock").clone()
        }
    }

    impl Default for RecordingApplyEditsHost {
        fn default() -> Self {
            Self::new(successful_apply_edits_result("src/main.rs"))
        }
    }

    impl ApplyEditsHostPort for RecordingApplyEditsHost {
        fn apply_batch_edits(
            &self,
            request: WorkspaceApplyEditsRequest,
        ) -> Result<WorkspaceApplyEditsResult, DomainError> {
            self.calls
                .lock()
                .expect("call recorder lock")
                .push(RecordedApplyEditsCall::ApplyBatch(request));
            Ok(self.result.clone())
        }
    }

    struct VersionConflictApplyEditsHost;

    impl ApplyEditsHostPort for VersionConflictApplyEditsHost {
        fn apply_batch_edits(
            &self,
            _request: WorkspaceApplyEditsRequest,
        ) -> Result<WorkspaceApplyEditsResult, DomainError> {
            Err(DomainError::VersionConflict {
                expected: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
                actual: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
            })
        }
    }

    fn successful_apply_edits_result(path: &str) -> WorkspaceApplyEditsResult {
        WorkspaceApplyEditsResult {
            files_changed: 1,
            per_file: vec![PerFileEditResult {
                path: RelativePath::new(path),
                applied: true,
                edits_applied: 1,
                edits_skipped: 0,
                new_version: Some(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                ),
                preview: None,
                error: None,
            }],
        }
    }

    fn make_deps_with_push(push_tx: mpsc::Sender<PushEvent>) -> HostDeps {
        HostDeps {
            fs: Arc::new(std::sync::Mutex::new(InMemoryFs::new())),
            ast_index: Arc::new(InMemoryAstIndex::new()),
            format_queue: Arc::new(NoOpFormatQueue),
            apply_edits: Arc::new(RecordingApplyEditsHost::default()),
            push_tx: Some(push_tx),
        }
    }

    fn make_deps_no_push() -> HostDeps {
        make_deps_no_push_with_apply_edits(Arc::new(RecordingApplyEditsHost::default()))
    }

    fn make_deps_no_push_with_apply_edits(apply_edits: Arc<dyn ApplyEditsHostPort>) -> HostDeps {
        HostDeps {
            fs: Arc::new(std::sync::Mutex::new(InMemoryFs::new())),
            ast_index: Arc::new(InMemoryAstIndex::new()),
            format_queue: Arc::new(NoOpFormatQueue),
            apply_edits,
            push_tx: None,
        }
    }

    fn parse_response(frame: &str) -> Value {
        serde_json::from_str(frame.trim()).expect("parse response frame")
    }

    fn response_error(response: &Value) -> (i64, String) {
        let error = response
            .get("error")
            .expect("response must contain JSON-RPC error");
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .expect("error code must be numeric");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .expect("error message must be a string")
            .to_owned();
        (code, message)
    }

    fn production_sidecar_source() -> &'static str {
        include_str!("sidecar.rs")
            .split("// ── Unit tests")
            .next()
            .expect("sidecar production source prefix")
    }

    #[test]
    fn request_apply_edits_host_deps_contract_surface_is_declared() {
        let source = include_str!("host_deps.rs");

        assert!(
            source.contains("pub trait ApplyEditsHostPort: Send + Sync"),
            "host_deps.rs must define public object-safe ApplyEditsHostPort"
        );
        assert!(
            source.contains(
                "apply_batch_edits(&self, request: WorkspaceApplyEditsRequest) -> Result<WorkspaceApplyEditsResult, DomainError>"
            ),
            "ApplyEditsHostPort must expose apply_batch_edits with the batch domain DTOs"
        );
        assert!(
            source.contains("pub apply_edits: Arc<dyn ApplyEditsHostPort>"),
            "HostDeps must expose the apply-edits dependency as an Arc trait object"
        );
    }

    #[test]
    fn request_apply_edits_undeclared_capability_is_rejected_before_mutation() {
        let deps = make_deps_no_push();
        let response_frame = dispatch_host_call(
            "workspace/applyEdits",
            json!(41),
            json!({
                "edits": [
                    {
                        "path": "src/main.rs",
                        "start_byte": 0,
                        "end_byte": 0,
                        "replacement": "use std::fmt;\n",
                        "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }
                ],
                "dry_run": false
            }),
            &deps,
            &[],
        );

        let response = parse_response(&response_frame);
        let (code, message) = response_error(&response);
        assert_eq!(code, -32603);
        assert!(
            message.contains("capability 'request_apply_edits' not declared"),
            "undeclared RequestApplyEdits must fail with the capability error, got: {message}"
        );
    }

    #[test]
    fn request_apply_edits_invalid_path_returns_structured_result_and_malformed_expected_version_is_invalid_params()
     {
        let deps = make_deps_no_push();
        let allowed_caps = vec![Capability::RequestApplyEdits];

        for path in ["/tmp/escape.rs", "../escape.rs"] {
            let response_frame = dispatch_host_call(
                "workspace/applyEdits",
                json!(42),
                json!({
                    "edits": [
                        {
                            "path": path,
                            "start_byte": 0,
                            "end_byte": 0,
                            "replacement": "x",
                            "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        }
                    ],
                    "dry_run": false
                }),
                &deps,
                &allowed_caps,
            );

            let response = parse_response(&response_frame);
            assert_eq!(
                response["result"]["per_file"][0]["error"]["code"], "invalid_path",
                "invalid path {path:?} must be represented as a per-file invalid_path result"
            );
        }

        let malformed_version_response = dispatch_host_call(
            "workspace/applyEdits",
            json!(43),
            json!({
                "edits": [
                    {
                        "path": "src/main.rs",
                        "start_byte": 0,
                        "end_byte": 0,
                        "replacement": "x",
                        "base_hash": null
                    }
                ],
                "dry_run": false
            }),
            &deps,
            &allowed_caps,
        );

        let response = parse_response(&malformed_version_response);
        let (code, message) = response_error(&response);
        assert_eq!(code, -32602);
        assert!(
            message.starts_with("invalid_params:"),
            "malformed base_hash must fail closed as invalid_params, got: {message}"
        );
    }

    #[test]
    fn request_apply_edits_malformed_params_decode_to_invalid_params() {
        let deps = make_deps_no_push();
        let allowed_caps = vec![Capability::RequestApplyEdits];

        let response_frame = dispatch_host_call(
            "workspace/applyEdits",
            json!(44),
            json!({
                "edits": "not-an-edit-list",
                "dry_run": false
            }),
            &deps,
            &allowed_caps,
        );

        let response = parse_response(&response_frame);
        let (code, message) = response_error(&response);
        assert_eq!(
            code, -32602,
            "malformed RequestApplyEdits params must be JSON-RPC invalid params"
        );
        assert!(
            message.starts_with("invalid_params:"),
            "malformed RequestApplyEdits params must include invalid_params context, got: {message}"
        );
    }

    #[test]
    fn request_apply_edits_stale_cas_maps_to_precondition_failed() {
        let deps = make_deps_no_push_with_apply_edits(Arc::new(VersionConflictApplyEditsHost));
        let allowed_caps = vec![Capability::RequestApplyEdits];

        let response_frame = dispatch_host_call(
            "workspace/applyEdits",
            json!(44),
            json!({
                "edits": [
                    {
                        "path": "src/main.rs",
                        "start_byte": 0,
                        "end_byte": 0,
                        "replacement": "use std::fmt;\n",
                        "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }
                ],
                "dry_run": false
            }),
            &deps,
            &allowed_caps,
        );

        let response = parse_response(&response_frame);
        let (code, message) = response_error(&response);
        assert_eq!(
            code, -32009,
            "stale CAS must map to the JSON-RPC precondition failed code"
        );
        assert!(
            message.starts_with("precondition_failed:"),
            "stale CAS must use the precondition_failed message prefix, got: {message}"
        );
    }

    #[test]
    fn request_apply_edits_range_validation_errors_include_invalid_range_context() {
        let source = production_sidecar_source();

        assert!(
            source.contains("DomainError::InvalidRange"),
            "dispatch_host_call must explicitly map range and UTF-8 validation failures"
        );
        assert!(
            source.contains("invalid_range:"),
            "InvalidRange must serialize with invalid_range prefix"
        );
        assert!(
            source.contains("start_byte") && source.contains("end_byte"),
            "invalid range responses must preserve range context"
        );
    }

    #[test]
    fn request_apply_edits_dry_run_success_routes_once_and_serializes_domain_result() {
        let result = successful_apply_edits_result("src/main.rs");
        let apply_edits = Arc::new(RecordingApplyEditsHost::new(result.clone()));
        let deps = make_deps_no_push_with_apply_edits(apply_edits.clone());
        let allowed_caps = vec![Capability::RequestApplyEdits];

        let response_frame = dispatch_host_call(
            "workspace/applyEdits",
            json!(45),
            json!({
                "edits": [
                    {
                        "path": "src/main.rs",
                        "start_byte": 0,
                        "end_byte": 0,
                        "replacement": "use std::fmt;\n",
                        "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }
                ],
                "dry_run": true
            }),
            &deps,
            &allowed_caps,
        );

        let response = parse_response(&response_frame);
        assert_eq!(
            response.get("result"),
            Some(&serde_json::to_value(&result).expect("serialize domain apply-edits result")),
            "successful dispatch must serialize the domain WorkspaceApplyEditsResult directly"
        );
        assert_eq!(
            apply_edits.calls(),
            vec![RecordedApplyEditsCall::ApplyBatch(
                WorkspaceApplyEditsRequest {
                    edits: vec![WorkspaceEditSpan {
                        path: RelativePath::new("src/main.rs"),
                        start_byte: 0,
                        end_byte: 0,
                        replacement: "use std::fmt;\n".to_owned(),
                        base_hash: Some(
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                                .to_owned(),
                        ),
                    }],
                    dry_run: Some(true),
                }
            )],
            "dry_run:true must route exactly once through the batch dependency path"
        );
    }

    #[test]
    fn request_apply_edits_apply_success_routes_once_and_serializes_domain_result() {
        let result = successful_apply_edits_result("src/main.rs");
        let apply_edits = Arc::new(RecordingApplyEditsHost::new(result.clone()));
        let deps = make_deps_no_push_with_apply_edits(apply_edits.clone());
        let allowed_caps = vec![Capability::RequestApplyEdits];

        let response_frame = dispatch_host_call(
            "workspace/applyEdits",
            json!(46),
            json!({
                "edits": [
                    {
                        "path": "src/main.rs",
                        "start_byte": 0,
                        "end_byte": 0,
                        "replacement": "use std::fmt;\n",
                        "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }
                ],
                "dry_run": false
            }),
            &deps,
            &allowed_caps,
        );

        let response = parse_response(&response_frame);
        assert_eq!(
            response.get("result"),
            Some(&serde_json::to_value(&result).expect("serialize domain apply-edits result")),
            "successful dispatch must serialize the domain WorkspaceApplyEditsResult directly"
        );
        assert_eq!(
            apply_edits.calls(),
            vec![RecordedApplyEditsCall::ApplyBatch(
                WorkspaceApplyEditsRequest {
                    edits: vec![WorkspaceEditSpan {
                        path: RelativePath::new("src/main.rs"),
                        start_byte: 0,
                        end_byte: 0,
                        replacement: "use std::fmt;\n".to_owned(),
                        base_hash: Some(
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                                .to_owned(),
                        ),
                    }],
                    dry_run: Some(false),
                }
            )],
            "dry_run:false must route exactly once through the batch dependency path"
        );
    }

    /// Spec 27 O1: `notify/resourceUpdated` HostCall dispatches to the push channel.
    #[test]
    fn notify_resource_updated_sends_to_push_channel() {
        let (tx, rx) = mpsc::channel::<PushEvent>();
        let deps = make_deps_with_push(tx);
        let allowed_caps = vec![Capability::Notify];
        let call_id = json!(99);
        let params = json!({ "uri": "lsp://rust/diagnostics" });

        let response_frame = dispatch_host_call(
            "notify/resourceUpdated",
            call_id.clone(),
            params,
            &deps,
            &allowed_caps,
        );

        // Response frame must be a success (not an error).
        let response: serde_json::Value =
            serde_json::from_str(response_frame.trim()).expect("parse response frame");
        assert!(
            response.get("result").is_some(),
            "must return a result, not an error; got: {response}"
        );

        // Push channel must have received exactly one event.
        let event = rx.try_recv().expect("must have received a push event");
        assert_eq!(event.uri, "lsp://rust/diagnostics");
    }

    /// Spec 27: `notify/resourceUpdated` without `notify` capability returns error.
    #[test]
    fn notify_resource_updated_without_capability_returns_error() {
        let deps = make_deps_no_push();
        let allowed_caps: Vec<Capability> = vec![];
        let call_id = json!(1);
        let params = json!({ "uri": "lsp://rust/diagnostics" });

        let response_frame = dispatch_host_call(
            "notify/resourceUpdated",
            call_id,
            params,
            &deps,
            &allowed_caps,
        );

        let response: serde_json::Value =
            serde_json::from_str(response_frame.trim()).expect("parse response frame");
        assert!(
            response.get("error").is_some(),
            "must return an error when capability not declared; got: {response}"
        );
    }

    /// Spec 27: `notify/resourceUpdated` with `notify` capability but no push sender
    /// returns success (fire-and-forget; push_tx = None means no MCP subscribers).
    #[test]
    fn notify_resource_updated_with_notify_cap_no_push_sender_succeeds() {
        let deps = make_deps_no_push();
        let allowed_caps = vec![Capability::Notify];
        let call_id = json!(1);
        let params = json!({ "uri": "lsp://rust/diagnostics" });

        let response_frame = dispatch_host_call(
            "notify/resourceUpdated",
            call_id,
            params,
            &deps,
            &allowed_caps,
        );

        let response: serde_json::Value =
            serde_json::from_str(response_frame.trim()).expect("parse response frame");
        assert!(
            response.get("result").is_some(),
            "must return success even when push_tx is None; got: {response}"
        );
    }
}
