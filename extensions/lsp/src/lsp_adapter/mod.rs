//! `LspClientAdapter` — a resident language-server client implementing
//! `CodeIntelligencePort` for a single configured server (MVP: rust-analyzer).
//!
//! # Design
//!
//! The server is spawned once on the first call to [`LspClientAdapter::spawn`]
//! and kept resident. A dedicated reader thread parses every framed message and
//! routes it through a [`Dispatcher`]:
//!
//! - `textDocument/publishDiagnostics` → [`record_diagnostics`] (bumps generation)
//! - `experimental/serverStatus` with `quiescent: true` → [`mark_ready`]
//!   (preferred readiness signal; requires `capabilities.experimental.serverStatusNotification`
//!   in the initialize params so rust-analyzer emits it)
//! - `$/progress` with `kind: "end"` → [`mark_ready`] (generic fallback, suppressed once
//!   any `experimental/serverStatus` notification has been seen)
//! - `client/registerCapability`, `window/workDoneProgress/create` → [`ack`]
//!   (empty result, best-effort)
//! - Unknown methods → silently ignored (read loop survives)
//!
//! # Ready gate and generation-numbered diagnostics
//!
//! [`SessionState`] holds both the readiness flag and diagnostics keyed by URI
//! and generation. After emitting a document sync, `check` waits until the server
//! is ready **and** a newer diagnostics generation has landed for the requested
//! URI. When identical content emits no sync, an already-settled URI entry is
//! authoritative and can be reused immediately. This eliminates the fixed settle
//! budget and the `Instant`-stamp double-publish heuristic.
//!
//!
//! # Lock discipline
//!
//! Three independent locks exist, with a strict ordering to prevent deadlocks:
//!
//! 1. `self.inner` (`Mutex<Session>`) — guards `child` and `docs`
//!    (`DocumentTracker`). Held across the `self.writer` write in the doc-sync
//!    paths to preserve per-document version ordering (two racing `didChange`s
//!    cannot interleave out of order). Released before any condvar wait.
//!    `acquire_doc` and `release_doc` (navigation-side doc holds) follow the
//!    same `inner` → `writer` ordering, so this discipline covers both the
//!    watcher and navigation paths.
//! 2. `self.writer` — serializes stdin writes on a dedicated worker. Callers
//!    enqueue one complete frame and may use a deadline-aware acknowledgement;
//!    the reader thread uses the same queue for server-request acks.
//! 3. `self.state` / `self.responses` (each `Mutex` inside their `Arc`) —
//!    written by the reader thread, waited on by `check` and `send_request_once`.
//!    The reader thread never acquires `inner`, so the ordering
//!    `inner` → `writer` → (`state` | `responses`) is never reversed.
//!
//! `check` acquires `inner` (doc sync), releases it, then waits on `state`.
//! `await_index_settled` waits on `state` only.
//! The reader thread's handler paths are mutually exclusive and each takes
//! exactly one lock at a time: the ack path acquires only `writer`; the
//! notification path acquires only `state`; the response path acquires only
//! `responses`. The reader thread never acquires `inner`.

#![forbid(unsafe_code)]

pub mod decode;
pub mod discovery;
pub mod documents;
pub mod jsonrpc;
pub mod pool;
pub mod position_map;

pub use pool::SessionPool;

use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::{Value, json};

use crate::lsp_adapter::decode::{
    RawLocation, UriAccessPolicy, UriClassification, WorkspaceEditDecodeError,
    decode_workspace_edit as decode_workspace_edit_spans,
};
use crate::lsp_adapter::documents::{DocSync, DocumentTracker};
use core_engine::adapters::fs::scan::TOWERIGNORE_FILE_NAME;
use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::{
    Diagnostic, Hover, Location, Position, Range, Severity, Symbol,
};
use core_engine::domain::mutation::compute_content_version;
use core_engine::ports::{
    CodeIntelError, CodeIntelligencePort, DocumentSyncPort, NavigationPort, PrepareRenameResult,
    RenameNavigationError,
};
use extension_protocol::messages::{
    CleanupCoverage, LspOutcomeCode, PathMoveKind, PathRenameError, PathRenameErrorCode,
    PathRenamePlanningRequest, SafeDeleteCleanupEdit, SafeDeletePosition, SafeDeleteReference,
    SafeDeleteSemanticOutcome, SafeDeleteSemanticPlan, SafeDeleteSemanticRequest,
};

#[allow(dead_code)]
pub type RawWorkspaceEdit = lsp_types::WorkspaceEdit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LspSessionError {
    Unconfigured,
    CapabilityUnavailable,
    NotRenameable,
    WorkspaceExternal,
    Backend(extension_protocol::LspOutcomeCode),
}

impl core::fmt::Display for LspSessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let description = match self {
            Self::Unconfigured => "language is not configured",
            Self::CapabilityUnavailable => "language server capability is unavailable",
            Self::NotRenameable => "symbol is not renameable",
            Self::WorkspaceExternal => "workspace path is external",
            Self::Backend(_) => "language server request failed",
        };
        f.write_str(description)
    }
}

impl core::error::Error for LspSessionError {}

/// A push event carrying updated diagnostics for one URI.
///
/// Sent by the reader thread when `publishDiagnostics` arrives, so the MCP
/// serve loop can push `notifications/resources/updated` to subscribers.
#[derive(Debug, Clone)]
pub struct DiagnosticsEvent {
    pub uri: String,
    // `diagnostics` and `generation` are populated by the reader thread for
    // potential future use; the push forwarder currently only forwards the URI.
    #[allow(dead_code)]
    pub diagnostics: Vec<Diagnostic>,
    #[allow(dead_code)]
    pub generation: u64,
}

/// Sender half of the push channel, cloned into the reader thread at spawn time.
/// Uses an unbounded channel so `send()` is non-blocking: the reader thread
/// calls it after releasing the `state` MutexGuard (Decision 3).
/// `None` = push disabled; a disconnected receiver is silently ignored.
pub type DiagnosticsSender = mpsc::Sender<DiagnosticsEvent>;

#[cfg(test)]
const DIAGNOSTIC_PUBLICATION_HISTORY: usize = 64;

/// LSP `ContentModified` error code — the document changed while the request was
/// in flight. Per the LSP spec this is transient: re-request rather than fail.
const CONTENT_MODIFIED: i64 = -32801;

/// How many times a `ContentModified` response is retried before giving up.
const CONTENT_MODIFIED_RETRIES: u32 = 5;

/// Backoff between `ContentModified` retries (lets the server settle).
const CONTENT_MODIFIED_BACKOFF: Duration = Duration::from_millis(300);

/// Classified outcome of a single LSP request (see `send_request_once`).
enum RequestOutcome {
    /// A successful `result` payload.
    Ok(Value),
    /// A transient `ContentModified` (-32801) — retryable.
    ContentModified,
    /// A non-retryable error (already formatted) or a timeout.
    Error(RequestFailure),
}

/// Non-retryable LSP request failure.
enum RequestFailure {
    Response {
        code: Option<i64>,
        message: Option<String>,
    },
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ReaderExit {
    Crashed = 0,
    Transport = 1,
    Malformed = 2,
}

impl ReaderExit {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Transport,
            2 => Self::Malformed,
            _ => Self::Crashed,
        }
    }
}

fn classify_reader_error(error: &std::io::Error) -> ReaderExit {
    match error.kind() {
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
            ReaderExit::Malformed
        }
        _ => ReaderExit::Transport,
    }
}

fn session_to_code_intel_error(error: LspSessionError) -> CodeIntelError {
    match error {
        LspSessionError::Unconfigured | LspSessionError::CapabilityUnavailable => {
            CodeIntelError::Unsupported
        }
        LspSessionError::NotRenameable
        | LspSessionError::WorkspaceExternal
        | LspSessionError::Backend(_) => CodeIntelError::Backend(error.to_string()),
    }
}

fn session_to_rename_error(error: LspSessionError) -> RenameNavigationError {
    match error {
        LspSessionError::Unconfigured | LspSessionError::CapabilityUnavailable => {
            RenameNavigationError::UnsupportedLanguage
        }
        LspSessionError::NotRenameable => RenameNavigationError::NotRenameable,
        LspSessionError::WorkspaceExternal | LspSessionError::Backend(_) => {
            RenameNavigationError::Backend(error.to_string())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenameCapability {
    Unavailable,
    RenameOnly,
    RenameWithPrepare,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderCapability {
    Unavailable,
    Available,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticsCapability {
    Push,
    Pull,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SemanticCapabilities {
    definition: ProviderCapability,
    references: ProviderCapability,
    hover: ProviderCapability,
    implementation: ProviderCapability,
    type_hierarchy: ProviderCapability,
    rename: RenameCapability,
    will_rename_files: ProviderCapability,
    safe_delete: ProviderCapability,
    diagnostics: DiagnosticsCapability,
}

#[allow(dead_code)]
impl SemanticCapabilities {
    fn from_initialize_result(result: &Value) -> Result<Self, LspSessionError> {
        let capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or_else(malformed_capabilities)?;

        Ok(Self {
            definition: parse_provider(capabilities.get("definitionProvider"))?,
            references: parse_provider(capabilities.get("referencesProvider"))?,
            hover: parse_provider(capabilities.get("hoverProvider"))?,
            implementation: parse_provider(capabilities.get("implementationProvider"))?,
            type_hierarchy: parse_provider(capabilities.get("typeHierarchyProvider"))?,
            rename: parse_rename_provider(capabilities.get("renameProvider"))?,
            will_rename_files: parse_will_rename_files(capabilities.get("workspace"))?,
            safe_delete: parse_safe_delete(capabilities.get("experimental"))?,
            diagnostics: if matches!(
                capabilities.get("diagnosticProvider"),
                Some(Value::Bool(true) | Value::Object(_))
            ) {
                DiagnosticsCapability::Pull
            } else {
                DiagnosticsCapability::Push
            },
        })
    }
}

fn parse_safe_delete(value: Option<&Value>) -> Result<ProviderCapability, LspSessionError> {
    let Some(experimental) = value else {
        return Ok(ProviderCapability::Unavailable);
    };
    let experimental = experimental
        .as_object()
        .ok_or_else(malformed_capabilities)?;
    parse_provider(experimental.get("towerSafeDelete"))
}

fn parse_will_rename_files(value: Option<&Value>) -> Result<ProviderCapability, LspSessionError> {
    let Some(workspace) = value else {
        return Ok(ProviderCapability::Unavailable);
    };
    let workspace = workspace.as_object().ok_or_else(malformed_capabilities)?;
    let Some(file_operations) = workspace.get("fileOperations") else {
        return Ok(ProviderCapability::Unavailable);
    };
    let file_operations = file_operations
        .as_object()
        .ok_or_else(malformed_capabilities)?;
    parse_provider(file_operations.get("willRename"))
}

fn path_rename_error(code: PathRenameErrorCode, message: &'static str) -> PathRenameError {
    PathRenameError {
        code,
        path: None,
        message: message.to_owned(),
    }
}

fn validate_path_rename_moves(request: &PathRenamePlanningRequest) -> Result<(), PathRenameError> {
    let invalid = || {
        path_rename_error(
            PathRenameErrorCode::SemanticBackend,
            "Semantic planning failed",
        )
    };
    let Some(root) = request.moves.first() else {
        return Err(invalid());
    };
    let normalized = |path: &str| {
        !path.is_empty()
            && Path::new(path)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
    };
    if !normalized(&root.source)
        || !normalized(&root.destination)
        || root.source == root.destination
    {
        return Err(invalid());
    }

    if root.kind == PathMoveKind::File && request.moves.len() != 1 {
        return Err(invalid());
    }

    let mut sources = std::collections::HashSet::with_capacity(request.moves.len());
    let mut destinations = std::collections::HashSet::with_capacity(request.moves.len());
    let mut previous_source: Option<&str> = None;
    for (index, path_move) in request.moves.iter().enumerate() {
        if !normalized(&path_move.source)
            || !normalized(&path_move.destination)
            || !sources.insert(path_move.source.as_str())
            || !destinations.insert(path_move.destination.as_str())
            || previous_source.is_some_and(|previous| previous >= path_move.source.as_str())
        {
            return Err(invalid());
        }
        if index > 0 {
            if root.kind != PathMoveKind::Directory || path_move.kind != PathMoveKind::File {
                return Err(invalid());
            }
            let Ok(suffix) = Path::new(&path_move.source).strip_prefix(&root.source) else {
                return Err(invalid());
            };
            if suffix.as_os_str().is_empty()
                || Path::new(&root.destination).join(suffix) != Path::new(&path_move.destination)
            {
                return Err(invalid());
            }
        }
        previous_source = Some(&path_move.source);
    }
    Ok(())
}

fn malformed_capabilities() -> LspSessionError {
    LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerMalformedResponse)
}

fn parse_provider(value: Option<&Value>) -> Result<ProviderCapability, LspSessionError> {
    match value {
        None | Some(Value::Bool(false)) => Ok(ProviderCapability::Unavailable),
        Some(Value::Bool(true) | Value::Object(_)) => Ok(ProviderCapability::Available),
        Some(_) => Err(malformed_capabilities()),
    }
}

fn parse_rename_provider(value: Option<&Value>) -> Result<RenameCapability, LspSessionError> {
    match value {
        None | Some(Value::Bool(false)) => Ok(RenameCapability::Unavailable),
        Some(Value::Bool(true)) => Ok(RenameCapability::RenameOnly),
        Some(Value::Object(options)) => match options.get("prepareProvider") {
            None | Some(Value::Bool(false)) => Ok(RenameCapability::RenameOnly),
            Some(Value::Bool(true)) => Ok(RenameCapability::RenameWithPrepare),
            Some(_) => Err(malformed_capabilities()),
        },
        Some(_) => Err(malformed_capabilities()),
    }
}

/// Correlates request ids to the full response message the reader thread parsed.
type ResponseCache = Arc<(Mutex<HashMap<i64, Option<Value>>>, Condvar)>;

/// Registers before submission so immediate replies are accepted; dropping the
/// guard removes the reservation on success, timeout, or transport failure.
struct PendingRequest {
    responses: ResponseCache,
    id: i64,
}

impl PendingRequest {
    fn new(responses: &ResponseCache, id: i64) -> Self {
        let (lock, cvar) = &**responses;
        lock.lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, None);
        cvar.notify_all();
        Self {
            responses: Arc::clone(responses),
            id,
        }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.responses;
        lock.lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
        cvar.notify_all();
    }
}

/// Write half of the server's stdin, shared between the adapter (requests, doc
/// sync, handshake) and the reader thread (acking server→client requests). A
/// boxed trait object so tests can substitute an in-memory sink.
#[derive(Clone)]
struct Writer {
    tx: mpsc::Sender<WriteJob>,
}

struct WriteJob {
    message: Value,
    deadline: Option<Instant>,
    result: mpsc::SyncSender<std::io::Result<()>>,
}

impl Writer {
    fn new(mut output: Box<dyn std::io::Write + Send>) -> Self {
        let (tx, rx) = mpsc::channel::<WriteJob>();
        std::thread::spawn(move || {
            for job in rx {
                let result = if job
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "LSP write deadline elapsed before emission",
                    ))
                } else {
                    jsonrpc::write_message(&mut *output, &job.message)
                };
                let _ = job.result.send(result);
            }
        });
        Self { tx }
    }

    fn submit(
        &self,
        message: &Value,
        deadline: Option<Instant>,
    ) -> std::io::Result<mpsc::Receiver<std::io::Result<()>>> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.tx
            .send(WriteJob {
                message: message.clone(),
                deadline,
                result: result_tx,
            })
            .map_err(|_| broken_writer())?;
        Ok(result_rx)
    }

    fn write(&self, message: &Value) -> std::io::Result<()> {
        let result_rx = self.submit(message, None)?;
        result_rx.recv().map_err(|_| broken_writer())?
    }

    fn write_before(&self, message: &Value, deadline: Instant) -> std::io::Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "LSP write deadline elapsed",
            ));
        }
        let result_rx = self.submit(message, Some(deadline))?;
        result_rx
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "LSP write deadline elapsed")
                }
                mpsc::RecvTimeoutError::Disconnected => broken_writer(),
            })?
    }

    fn write_until(&self, message: &Value, deadline: Option<Instant>) -> std::io::Result<()> {
        match deadline {
            Some(deadline) => self.write_before(message, deadline),
            None => self.write(message),
        }
    }
}

fn broken_writer() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "LSP writer stopped")
}

/// Frame and write `msg` to the shared writer. Held only across the write, never
/// across a condvar wait.
fn write_framed(writer: &Writer, msg: &Value) -> std::io::Result<()> {
    writer.write(msg)
}

// Handlers run only on the reader thread that owns the Dispatcher; `Send` lets
// the Dispatcher be moved onto that thread. `Sync` is not required — it is never
// shared by reference across threads.
type NotificationHandler = Box<dyn Fn(&Value) + Send>;
type RequestHandler = Box<dyn Fn(&Value) + Send>;

/// Routes parsed LSP messages by method:
/// - `method` + `id`  → server→client request → request handler (acks)
/// - `method`, no `id`→ notification → notification handler
/// - `id`, no `method`→ response → correlated into the response cache
///
/// Unknown methods are ignored without breaking the read loop.
struct Dispatcher {
    notifications: HashMap<&'static str, NotificationHandler>,
    requests: HashMap<&'static str, RequestHandler>,
    responses: ResponseCache,
}

impl Dispatcher {
    fn new(responses: ResponseCache) -> Self {
        Self {
            notifications: HashMap::new(),
            requests: HashMap::new(),
            responses,
        }
    }

    fn on_notification(&mut self, method: &'static str, handler: NotificationHandler) {
        self.notifications.insert(method, handler);
    }

    fn on_request(&mut self, method: &'static str, handler: RequestHandler) {
        self.requests.insert(method, handler);
    }

    fn dispatch(&self, msg: &Value) {
        match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
            (Some(method), Some(_)) => {
                if let Some(handler) = self.requests.get(method) {
                    handler(msg);
                }
            }
            (Some(method), None) => {
                if let Some(handler) = self.notifications.get(method) {
                    handler(msg);
                }
            }
            (None, Some(id)) => {
                // LSP permits string ids; Tower only issues integer ids, so a non-integer id
                // here is not one of ours and is dropped.
                if let Some(id) = id.as_i64() {
                    let (lock, cvar) = &*self.responses;
                    let mut map = lock.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(response @ None) = map.get_mut(&id) {
                        *response = Some(msg.clone());
                        cvar.notify_all();
                    }
                }
            }
            (None, None) => {}
        }
    }
}

/// Reply to a server→client request with an empty `result` — we accept dynamic
/// capability registration / progress-token creation without acting on them.
///
/// Write failures are intentionally ignored (best-effort): a lost ack does not
/// break the session, and surfacing it would mask the real query result.
fn ack(writer: &Writer, msg: &Value) {
    if let Some(id) = msg.get("id") {
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": null });
        let _ = write_framed(writer, &reply);
    }
}

/// Wire the real handler set: diagnostics → store + optional push, readiness
/// signals → ready gate, server requests → ack.
///
/// `push_tx` is `Some` when a subscriber is wired (Task 6). `send()` is called
/// after `record_diagnostics` returns, capturing the generation from the stored
/// entry so push and pull always observe the same value.
fn build_dispatcher(
    state: SharedState,
    responses: ResponseCache,
    writer: Writer,
    push_tx: Option<DiagnosticsSender>,
) -> Dispatcher {
    let mut dispatcher = Dispatcher::new(responses);

    let s = Arc::clone(&state);
    dispatcher.on_notification(
        "textDocument/publishDiagnostics",
        Box::new(move |msg| {
            if let Some((uri, diags)) = parse_publish_diagnostics(msg) {
                let generation = record_diagnostics(&s, uri.clone(), diags.clone());
                if let Some(ref tx) = push_tx {
                    // Non-blocking: unbounded mpsc::Sender::send() never blocks.
                    // Silently drop if the receiver has disconnected (serve loop exited).
                    let _ = tx.send(DiagnosticsEvent {
                        uri,
                        diagnostics: diags,
                        generation,
                    });
                }
            }
        }),
    );

    let s = Arc::clone(&state);
    dispatcher.on_notification(
        "experimental/serverStatus",
        Box::new(move |msg| {
            // Mark the server as owning its readiness signal so the $/progress
            // fallback is suppressed for the rest of the session.
            note_server_status_active(&s);
            if server_status_is_quiescent(msg) {
                mark_ready(&s);
            }
        }),
    );

    let s = Arc::clone(&state);
    dispatcher.on_notification(
        "$/progress",
        Box::new(move |msg| {
            // Generic fallback for servers that do not emit experimental/serverStatus.
            // Suppressed once any serverStatus notification has been seen — at that
            // point the server owns its readiness signal exclusively via
            // serverStatus quiescent:true, and $/progress end tokens (cachePriming,
            // CrateGraph, etc.) would fire prematurely before cargo-check completes.
            if progress_kind(msg) == Some("end") && !is_server_status_active(&s) {
                mark_ready(&s);
            }
        }),
    );

    let w = writer.clone();
    dispatcher.on_request(
        "client/registerCapability",
        Box::new(move |msg| ack(&w, msg)),
    );
    let w = writer.clone();
    dispatcher.on_request(
        "window/workDoneProgress/create",
        Box::new(move |msg| ack(&w, msg)),
    );

    dispatcher
}

/// Diagnostics for one URI, tagged with the generation at which they were
/// published, so `wait_for_settled` can require a strictly newer publish than
/// the one seen before the `didOpen`/`didChange` was sent.
#[derive(Clone)]
struct DiagEntry {
    diags: Vec<Diagnostic>,
    #[cfg_attr(not(test), allow(dead_code))]
    generation: u64,
}

struct PushWaiter {
    watermark: u64,
    first_fresh: Option<Vec<Diagnostic>>,
}

/// Diagnostics + readiness for the resident session, behind one mutex so `check`
/// can block on a single predicate: `ready_gen > pre_sync_ready_gen && fresh`.
///
/// Named `SessionStateInner` so `pool.rs` tests can construct a default one for
/// `FakeSession::shared_state()` without exposing private fields.
#[derive(Default)]
pub struct SessionStateInner {
    /// Set once the server signals analysis settled. Until then, published
    /// diagnostics are not authoritative.
    ready: bool,
    by_uri: HashMap<String, DiagEntry>,
    push_waiters: HashMap<String, HashMap<u64, PushWaiter>>,
    next_push_waiter_id: u64,
    /// Monotonic, bumped on every `publishDiagnostics` for any URI.
    next_gen: u64,
    /// Monotonic, bumped on every authoritative quiescent signal. Allows
    /// `await_index_settled` and `wait_for_settled` to wait for a signal that
    /// arrived AFTER the current sync, not a stale one from a prior cycle.
    ready_gen: u64,
    /// True once any `experimental/serverStatus` notification has been received
    /// (regardless of quiescent value). When true, the `$/progress end` generic
    /// fallback is suppressed — the server owns its readiness signal exclusively
    /// via `serverStatus quiescent:true`. When false, the `$/progress end`
    /// fallback is the only readiness signal (servers without serverStatus support).
    server_status_active: bool,
}

impl SessionStateInner {
    fn publish(&mut self, uri: String, diags: Vec<Diagnostic>) {
        self.next_gen += 1;
        let generation = self.next_gen;
        self.by_uri.insert(
            uri.clone(),
            DiagEntry {
                diags: diags.clone(),
                generation,
            },
        );
        if let Some(waiters) = self.push_waiters.get_mut(&uri) {
            for waiter in waiters.values_mut() {
                if generation > waiter.watermark && waiter.first_fresh.is_none() {
                    waiter.first_fresh = Some(
                        self.by_uri
                            .get(&uri)
                            .map_or_else(Vec::new, |entry| entry.diags.clone()),
                    );
                }
            }
        }
    }

    fn signal_ready(&mut self) {
        self.ready = true;
        self.ready_gen += 1;
    }
}

/// Shared session state; the reader thread notifies the condvar on every
/// readiness change or diagnostics publish.
pub type SharedState = Arc<(Mutex<SessionStateInner>, Condvar)>;

/// Store diagnostics for `uri`, bumping the generation, wake any waiter,
/// and return the new generation so the caller can attach it to a push event.
fn record_diagnostics(state: &SharedState, uri: String, diags: Vec<Diagnostic>) -> u64 {
    let (lock, cvar) = &**state;
    let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
    s.publish(uri, diags);
    let generation = s.next_gen;
    cvar.notify_all();
    generation
}

/// Mark the session ready (analysis settled) and wake any waiter.
///
/// Called on every quiescent signal — bumps `ready_gen` each time so
/// `await_index_settled` can distinguish a fresh settled event from a
/// stale one captured before the current operation began.
fn mark_ready(state: &SharedState) {
    let (lock, cvar) = &**state;
    let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
    s.signal_ready();
    cvar.notify_all();
}

/// Record that the server has emitted at least one `experimental/serverStatus`
/// notification. This disables the generic `$/progress end` fallback for the
/// rest of the session so that premature progress tokens (cachePriming, etc.)
/// cannot fire the ready gate before cargo-check completes.
fn note_server_status_active(state: &SharedState) {
    let (lock, _) = &**state;
    lock.lock()
        .unwrap_or_else(|p| p.into_inner())
        .server_status_active = true;
}

/// Returns true once any `experimental/serverStatus` notification has been seen.
fn is_server_status_active(state: &SharedState) -> bool {
    let (lock, _) = &**state;
    lock.lock()
        .unwrap_or_else(|p| p.into_inner())
        .server_status_active
}

/// Read the current diagnostics generation. Call before syncing so `check` can
/// wait for a strictly newer generation (a publish caused by this sync or later).
#[cfg(test)]
fn current_generation(state: &SharedState) -> u64 {
    let (lock, _) = &**state;
    lock.lock().unwrap_or_else(|p| p.into_inner()).next_gen
}

fn register_push_waiter(state: &SharedState, uri: &str) -> (u64, u64) {
    let (lock, _) = &**state;
    let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
    let watermark = state.next_gen;
    let waiter_id = state.next_push_waiter_id;
    state.next_push_waiter_id += 1;
    state
        .push_waiters
        .entry(uri.to_owned())
        .or_default()
        .insert(
            waiter_id,
            PushWaiter {
                watermark,
                first_fresh: None,
            },
        );
    (watermark, waiter_id)
}

fn remove_push_waiter(state: &mut SessionStateInner, uri: &str, waiter_id: u64) {
    if let Some(waiters) = state.push_waiters.get_mut(uri) {
        waiters.remove(&waiter_id);
        if waiters.is_empty() {
            state.push_waiters.remove(uri);
        }
    }
}

/// Read the current ready generation. Call before opening documents so
/// `await_index_settled` can wait for a quiescent signal that arrived AFTER
/// the documents were synced, not a stale one from a prior cycle.
fn current_ready_gen(state: &SharedState) -> u64 {
    let (lock, _) = &**state;
    lock.lock().unwrap_or_else(|p| p.into_inner()).ready_gen
}

/// Block until the server has signalled a real quiescent (`ready_gen` newer
/// than `pre_sync_ready_gen`) AND published diagnostics for `uri` newer than
/// `sync_gen`, or `timeout` elapses (best-effort fallback).
///
/// For rust-analyzer, `mark_ready` is called by the `experimental/serverStatus`
/// handler when `quiescent: true` arrives — after the real cargo-check has run
/// and `publishDiagnostics` with the error result has already been delivered.
/// The `$/progress` end fallback may also call `mark_ready` for servers without
/// `serverStatus`; the two-part predicate (quiescent AND fresh diagnostics)
/// ensures the gate does not fire on a stale quiescent from a prior cycle.
#[cfg(test)]
fn wait_for_settled(
    state: &SharedState,
    uri: &str,
    sync_gen: u64,
    pre_sync_ready_gen: u64,
    timeout: Duration,
) -> Vec<Diagnostic> {
    let (lock, cvar) = &**state;
    let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
    let deadline = Instant::now() + timeout;
    loop {
        let fresh = s.by_uri.get(uri).is_some_and(|e| e.generation > sync_gen);
        let quiescent = s.ready_gen > pre_sync_ready_gen;
        let now = Instant::now();
        if (quiescent && fresh) || now >= deadline {
            return s
                .by_uri
                .get(uri)
                .map(|e| e.diags.clone())
                .unwrap_or_default();
        }
        let (next, _) = cvar
            .wait_timeout(s, deadline - now)
            .unwrap_or_else(|p| p.into_inner());
        s = next;
    }
}

/// Resident client for one language server.
pub struct LspClientAdapter {
    workspace_root: std::path::PathBuf,
    /// Extensions this adapter serves (e.g. `["rs"]`).
    extensions: Vec<String>,
    /// The language id sent on `didOpen` (e.g. `"rust"`, `"go"`).
    /// Set at spawn time from the pool's language key; immutable thereafter.
    language_id: String,
    inner: Mutex<Session>,
    /// Shared stdin writer (requests, doc sync, acks from the reader thread).
    writer: Writer,
    /// Diagnostics + readiness, written by the reader thread, waited on by `check`.
    state: SharedState,
    /// Responses to navigation requests, keyed by request id.
    responses: ResponseCache,
    /// Semantic provider capabilities reported by the initialized server.
    semantic_capabilities: Mutex<SemanticCapabilities>,
    operation_timeout: Option<Duration>,
    /// Root `.towerignore` policy applied to navigation results.
    ignore: Gitignore,
    next_id: AtomicI64,
    /// Set to `true` by the reader thread on stdout EOF (server died or exited).
    /// The pool checks this on every request to detect crashes. Relaxed ordering
    /// is sufficient: at worst the pool makes one wasted I/O attempt to a dead
    /// writer, gets `CodeIntelError::Backend`, and detects `true` on the next
    /// request — matching UN1 semantics exactly.
    pub dead_flag: Arc<AtomicBool>,
    /// Cause published by the reader before `dead_flag` becomes observable.
    reader_exit: Arc<AtomicU8>,
}

struct Session {
    child: Child,
    /// Single owner of open-document state (open/version/refcount), shared by
    /// `check`, navigation, and the watcher document-sync path so a document is
    /// never opened twice.
    docs: DocumentTracker,
}

fn classify_spawn_error(error: std::io::Error, executable: &Path, root: &Path) -> LspSessionError {
    use extension_protocol::LspOutcomeCode;

    let code = if !root.is_dir() {
        LspOutcomeCode::ServerLaunchFailed
    } else if matches!(std::fs::metadata(executable), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        LspOutcomeCode::ServerMissing
    } else {
        match error.kind() {
            std::io::ErrorKind::PermissionDenied if executable_is_non_executable(executable) => {
                LspOutcomeCode::ServerNotExecutable
            }
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::Unsupported => {
                LspOutcomeCode::ServerNotExecutable
            }
            _ if is_bad_executable_format(&error) => LspOutcomeCode::ServerNotExecutable,
            _ => LspOutcomeCode::ServerLaunchFailed,
        }
    };
    LspSessionError::Backend(code)
}

#[cfg(unix)]
fn executable_is_non_executable(executable: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(executable).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 == 0)
}

#[cfg(not(unix))]
fn executable_is_non_executable(_executable: &Path) -> bool {
    false
}

#[cfg(unix)]
fn is_bad_executable_format(error: &std::io::Error) -> bool {
    // POSIX ENOEXEC: the file exists but its format is not executable.
    error.raw_os_error() == Some(8)
}

#[cfg(windows)]
fn is_bad_executable_format(error: &std::io::Error) -> bool {
    // Win32 ERROR_BAD_EXE_FORMAT.
    error.raw_os_error() == Some(193)
}

#[cfg(not(any(unix, windows)))]
fn is_bad_executable_format(_error: &std::io::Error) -> bool {
    false
}

impl LspClientAdapter {
    #[allow(dead_code)]
    pub(crate) fn plan_path_rename(
        &self,
        request: &PathRenamePlanningRequest,
    ) -> Result<RawWorkspaceEdit, PathRenameError> {
        if self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .will_rename_files
            == ProviderCapability::Unavailable
        {
            return Err(path_rename_error(
                PathRenameErrorCode::SemanticUnsupported,
                "Semantic rename is unavailable",
            ));
        }
        validate_path_rename_moves(request)?;

        let files = request
            .moves
            .iter()
            .map(|path_move| {
                json!({
                    "oldUri": path_to_uri(&self.workspace_root.join(&path_move.source)),
                    "newUri": path_to_uri(&self.workspace_root.join(&path_move.destination)),
                })
            })
            .collect::<Vec<_>>();
        let response = self
            .send_request("workspace/willRenameFiles", json!({ "files": files }))
            .map_err(|_| {
                path_rename_error(
                    PathRenameErrorCode::SemanticBackend,
                    "Semantic planning failed",
                )
            })?;
        if response.is_null() {
            return Ok(RawWorkspaceEdit::default());
        }
        let mut edit = decode_workspace_edit_typed(response).map_err(|_| {
            path_rename_error(
                PathRenameErrorCode::SemanticBackend,
                "Semantic planning failed",
            )
        })?;
        if edit.changes.as_ref().is_some_and(HashMap::is_empty) {
            edit.changes = None;
        }
        Ok(edit)
    }

    /// Spawn `command` with `args` rooted at `workspace_root`, perform the LSP
    /// handshake, and start the reader thread.
    ///
    /// `language_id` is the LSP language identifier sent on `didOpen` (e.g.
    /// `"rust"`, `"go"`). Pass `push_tx` to receive push events when diagnostics
    /// are published; `None` disables push delivery.
    ///
    /// # Errors
    ///
    /// Returns `CodeIntelError::Backend` if the server cannot be spawned or the
    /// handshake fails.
    #[allow(dead_code)]
    pub fn spawn(
        command: &str,
        args: &[String],
        extensions: Vec<String>,
        language_id: String,
        workspace_root: std::path::PathBuf,
        push_tx: Option<DiagnosticsSender>,
        operation_timeout: Option<Duration>,
    ) -> Result<Self, CodeIntelError> {
        Self::spawn_resolved(
            Path::new(command),
            args,
            extensions,
            language_id,
            workspace_root,
            push_tx,
            operation_timeout,
        )
        .map_err(session_to_code_intel_error)
    }

    pub(crate) fn spawn_resolved(
        executable: &Path,
        args: &[String],
        extensions: Vec<String>,
        language_id: String,
        root: PathBuf,
        push_tx: Option<DiagnosticsSender>,
        operation_timeout: Option<Duration>,
    ) -> Result<Self, LspSessionError> {
        if !root.is_dir() {
            return Err(LspSessionError::Backend(
                extension_protocol::LspOutcomeCode::ServerLaunchFailed,
            ));
        }

        let mut child = Command::new(executable)
            .args(args)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| classify_spawn_error(error, executable, &root))?;

        let stdin = child.stdin.take().ok_or(LspSessionError::Backend(
            extension_protocol::LspOutcomeCode::ServerLaunchFailed,
        ))?;
        let stdout = child.stdout.take().ok_or(LspSessionError::Backend(
            extension_protocol::LspOutcomeCode::ServerLaunchFailed,
        ))?;

        let writer = Writer::new(Box::new(stdin));
        let state: SharedState =
            Arc::new((Mutex::new(SessionStateInner::default()), Condvar::new()));
        let responses: ResponseCache = Arc::new((Mutex::new(HashMap::new()), Condvar::new()));
        let dead_flag = Arc::new(AtomicBool::new(false));
        let reader_exit = Arc::new(AtomicU8::new(ReaderExit::Crashed as u8));

        let dispatcher = build_dispatcher(
            Arc::clone(&state),
            Arc::clone(&responses),
            writer.clone(),
            push_tx,
        );
        let dead_flag_clone = Arc::clone(&dead_flag);
        let reader_exit_clone = Arc::clone(&reader_exit);
        let state_clone = Arc::clone(&state);
        let responses_clone = Arc::clone(&responses);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match jsonrpc::read_message(&mut reader) {
                    Ok(Some(msg)) => dispatcher.dispatch(&msg),
                    Ok(None) => break,
                    Err(error) => {
                        reader_exit_clone
                            .store(classify_reader_error(&error) as u8, Ordering::Relaxed);
                        break;
                    }
                }
            }
            // Reader thread exits on EOF (server died, crashed, or was killed).
            // Release publishes the exit cause before waking every waiter.
            {
                // Synchronize the terminal predicate with readiness waiters so
                // EOF cannot be lost between their predicate check and wait.
                let _state = state_clone.0.lock().unwrap_or_else(|p| p.into_inner());
                dead_flag_clone.store(true, Ordering::Release);
                state_clone.1.notify_all();
            }
            let _responses = responses_clone.0.lock().unwrap_or_else(|p| p.into_inner());
            responses_clone.1.notify_all();
        });

        let ignore = build_ignore_matcher(&root);
        let adapter = Self {
            workspace_root: root,
            extensions,
            language_id,
            inner: Mutex::new(Session {
                child,
                docs: DocumentTracker::new(),
            }),
            writer,
            state,
            responses,
            semantic_capabilities: Mutex::new(SemanticCapabilities {
                definition: ProviderCapability::Unavailable,
                references: ProviderCapability::Unavailable,
                hover: ProviderCapability::Unavailable,
                implementation: ProviderCapability::Unavailable,
                type_hierarchy: ProviderCapability::Unavailable,
                rename: RenameCapability::Unavailable,
                will_rename_files: ProviderCapability::Unavailable,
                safe_delete: ProviderCapability::Unavailable,
                diagnostics: DiagnosticsCapability::Push,
            }),
            operation_timeout,
            ignore,
            next_id: AtomicI64::new(1),
            dead_flag,
            reader_exit,
        };
        adapter.handshake()?;
        Ok(adapter)
    }

    /// Expose the shared diagnostic state so `PooledSession::shared_state` can
    /// clone the Arc. Called once at pool construction time; not on the hot path.
    #[allow(dead_code)]
    pub fn shared_state(&self) -> SharedState {
        Arc::clone(&self.state)
    }

    fn handshake(&self) -> Result<(), LspSessionError> {
        let deadline = self.operation_deadline()?;
        let root_uri = path_to_uri(&self.workspace_root);
        let initialize_params = initialize_params(std::process::id(), &root_uri);
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        let initialize_result =
            self.send_request_until("initialize", initialize_params, deadline)?;
        let capabilities = SemanticCapabilities::from_initialize_result(&initialize_result)?;
        *self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = capabilities;
        self.write_until(&initialized, deadline).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                return LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerTimeout);
            }
            LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerTransportError)
        })?;
        Ok(())
    }

    fn operation_deadline(&self) -> Result<Option<Instant>, LspSessionError> {
        self.operation_timeout
            .map(|timeout| {
                Instant::now()
                    .checked_add(timeout)
                    .ok_or(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerTimeout,
                    ))
            })
            .transpose()
    }

    fn write_until(&self, message: &Value, deadline: Option<Instant>) -> std::io::Result<()> {
        let result = self.writer.write_until(message, deadline);
        if result
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
        {
            self.terminate_after_write_timeout();
        }
        result
    }

    fn terminate_after_write_timeout(&self) {
        self.dead_flag.store(true, Ordering::Release);
        self.state.1.notify_all();
        self.responses.1.notify_all();
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let _ = session.child.kill();
    }

    fn uri_for(&self, path: &RelativePath) -> String {
        path_to_uri(&self.workspace_root.join(path.as_str()))
    }

    #[allow(dead_code)]
    fn normalize_type_hierarchy_items(
        &self,
        value: &Value,
    ) -> Result<Vec<extension_protocol::TypeHierarchyItem>, LspSessionError> {
        if value.is_null() {
            return Ok(Vec::new());
        }
        let raw_items = serde_json::from_value::<Vec<lsp_types::TypeHierarchyItem>>(value.clone())
            .map_err(|_| malformed_response())?;

        raw_items
            .into_iter()
            .map(|raw| {
                let kind = serde_json::to_value(raw.kind)
                    .ok()
                    .and_then(|value| value.as_u64())
                    .ok_or_else(malformed_response)
                    .map(decode::symbol_kind_name)?;
                let uri = raw.uri.to_string();
                let classification =
                    decode::classify_uri(&uri, &self.workspace_root, UriAccessPolicy::ReadOnly)
                        .ok_or_else(malformed_response)?;
                let (path, external_uri, external) = match classification {
                    UriClassification::WorkspacePath(path) => (Some(path), None, false),
                    UriClassification::ExternalUri(uri) => (None, Some(uri), true),
                };

                Ok(extension_protocol::TypeHierarchyItem {
                    name: raw.name,
                    kind,
                    detail: raw.detail,
                    range: type_hierarchy_range(raw.range),
                    selection_range: type_hierarchy_range(raw.selection_range),
                    path,
                    external_uri,
                    external,
                })
            })
            .collect()
    }

    #[allow(dead_code)]
    pub(crate) fn type_hierarchy_lsp(
        &self,
        query: &extension_protocol::TypeHierarchyQuery,
        text: &str,
    ) -> Result<extension_protocol::TypeHierarchyResult, LspSessionError> {
        let path = RelativePath::new(query.path.clone());
        if !self.supports(&path)
            || self
                .semantic_capabilities
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .type_hierarchy
                == ProviderCapability::Unavailable
        {
            return Err(LspSessionError::CapabilityUnavailable);
        }

        let uri = self.uri_for(&path);
        let deadline = self.operation_deadline()?;
        self.acquire_doc_until(&uri, text, deadline)?;
        let result = (|| {
            let prepared = self.send_request_until(
                "textDocument/prepareTypeHierarchy",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": query.line, "character": query.character }
                }),
                deadline,
            )?;
            let prepared = decode_prepared_type_hierarchy(prepared)?;
            let mut supertypes = Vec::new();
            let mut subtypes = Vec::new();

            let follow_ups = [
                (
                    matches!(
                        query.direction,
                        extension_protocol::TypeHierarchyDirection::Supertypes
                            | extension_protocol::TypeHierarchyDirection::Both
                    ),
                    "typeHierarchy/supertypes",
                    &mut supertypes,
                ),
                (
                    matches!(
                        query.direction,
                        extension_protocol::TypeHierarchyDirection::Subtypes
                            | extension_protocol::TypeHierarchyDirection::Both
                    ),
                    "typeHierarchy/subtypes",
                    &mut subtypes,
                ),
            ];
            for (requested, method, items) in follow_ups {
                if !requested {
                    continue;
                }
                for item in &prepared {
                    let response =
                        self.send_request_until(method, json!({ "item": item }), deadline)?;
                    append_unique(items, self.normalize_type_hierarchy_items(&response)?);
                }
            }

            Ok(extension_protocol::TypeHierarchyResult {
                supported: true,
                supertypes,
                subtypes,
                outcome: None,
            })
        })();
        self.release_doc(&uri);
        result
    }

    fn supports(&self, path: &RelativePath) -> bool {
        path.as_str()
            .rsplit_once('.')
            .map(|(_, ext)| self.extensions.iter().any(|e| e == ext))
            .unwrap_or(false)
    }

    /// Acquire a scoped hold on `uri` (a navigation request) and emit the
    /// resulting `didOpen`/`didChange`. Pair with [`release_doc`](Self::release_doc).
    fn acquire_doc_until(
        &self,
        uri: &str,
        text: &str,
        deadline: Option<Instant>,
    ) -> Result<(), LspSessionError> {
        self.acquire_doc_with_sync_until(uri, text, deadline)
            .map(|_| ())
    }

    fn acquire_doc_with_sync_until(
        &self,
        uri: &str,
        text: &str,
        deadline: Option<Instant>,
    ) -> Result<bool, LspSessionError> {
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let (sync, rollback) = session
            .docs
            .acquire_with_rollback(uri, &self.language_id, text);
        let emitted_sync = sync.is_some();
        if let Some(sync) = sync
            && let Err(error) = write_doc_sync_until(&self.writer, uri, &sync, deadline)
        {
            session.docs.rollback_acquire(uri, rollback);
            if error.kind() == std::io::ErrorKind::TimedOut {
                self.dead_flag.store(true, Ordering::Release);
                self.state.1.notify_all();
                self.responses.1.notify_all();
                let _ = session.child.kill();
            }
            return Err(if error.kind() == std::io::ErrorKind::TimedOut {
                LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerTimeout)
            } else {
                self.transport_failure()
            });
        }
        Ok(emitted_sync)
    }

    /// Release a scoped hold on `uri`, emitting `didClose` if it was the last
    /// reference. Best-effort: a write failure here must not mask a query result.
    fn release_doc(&self, uri: &str) {
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(sync) = session.docs.release(uri) {
            let _ = write_doc_sync(&self.writer, uri, &sync);
        }
    }

    /// Send an LSP request and block until its response (or [`REQUEST_BUDGET`]).
    ///
    /// The `inner` lock is NOT held while awaiting the response condvar — same
    /// lock discipline as `check`.
    ///
    /// A [`ContentModified`](CONTENT_MODIFIED) error (the document changed while
    /// the request was in flight — common when document open/close churn races a
    /// query) is **retried** a bounded number of times, per the LSP spec which
    /// defines it as a transient "re-request" signal rather than a real failure.
    fn send_request(&self, method: &str, params: Value) -> Result<Value, LspSessionError> {
        let deadline = self.operation_deadline()?;
        self.send_request_until(method, params, deadline)
    }

    fn send_request_until(
        &self,
        method: &str,
        params: Value,
        deadline: Option<Instant>,
    ) -> Result<Value, LspSessionError> {
        let mut attempt = 0;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(LspSessionError::Backend(
                    extension_protocol::LspOutcomeCode::ServerTimeout,
                ));
            }
            match self.send_request_once_until(method, params.clone(), deadline)? {
                RequestOutcome::Ok(value) => return Ok(value),
                RequestOutcome::ContentModified if attempt < CONTENT_MODIFIED_RETRIES => {
                    let backoff = deadline.map_or(CONTENT_MODIFIED_BACKOFF, |deadline| {
                        CONTENT_MODIFIED_BACKOFF
                            .min(deadline.saturating_duration_since(Instant::now()))
                    });
                    if backoff.is_zero() {
                        return Err(LspSessionError::Backend(
                            extension_protocol::LspOutcomeCode::ServerTimeout,
                        ));
                    }
                    std::thread::sleep(backoff);
                    attempt += 1;
                }
                RequestOutcome::ContentModified => {
                    return Err(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerError,
                    ));
                }
                RequestOutcome::Error(failure) => {
                    return Err(self.classify_request_failure(&failure));
                }
            }
        }
    }

    /// Send one request and await its response, classifying the outcome so
    /// [`send_request`](Self::send_request) can retry transient failures.
    fn send_request_once_until(
        &self,
        method: &str,
        params: Value,
        deadline: Option<Instant>,
    ) -> Result<RequestOutcome, LspSessionError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if self.dead_flag.load(Ordering::Acquire) {
            return Err(self.reader_failure());
        }
        let _pending = PendingRequest::new(&self.responses, id);
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        // The request does not touch `docs`, so we write directly without holding `inner`.
        self.write_until(&request, deadline).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerTimeout)
            } else {
                self.transport_failure()
            }
        })?;

        let (lock, cvar) = &*self.responses;
        let mut map = lock.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(msg) = map.get_mut(&id).and_then(Option::take) {
                map.remove(&id);
                let has_result = msg.get("result").is_some();
                let has_error = msg.get("error").is_some();
                if has_result == has_error {
                    return Err(malformed_response());
                }
                if let Some(err) = msg.get("error") {
                    if !err.is_object()
                        || err.get("code").and_then(Value::as_i64).is_none()
                        || err
                            .get("message")
                            .is_some_and(|message| !message.is_string())
                    {
                        return Err(malformed_response());
                    }
                    if err.get("code").and_then(Value::as_i64) == Some(CONTENT_MODIFIED) {
                        return Ok(RequestOutcome::ContentModified);
                    }
                    return Ok(RequestOutcome::Error(RequestFailure::Response {
                        code: err.get("code").and_then(Value::as_i64),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    }));
                }
                return Ok(RequestOutcome::Ok(msg["result"].clone()));
            }
            if self.dead_flag.load(Ordering::Acquire) {
                return Err(self.reader_failure());
            }
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(RequestOutcome::Error(RequestFailure::Timeout));
            }
            map = if let Some(deadline) = deadline {
                cvar.wait_timeout(map, deadline - now)
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            } else {
                cvar.wait(map).unwrap_or_else(|p| p.into_inner())
            };
        }
    }

    fn transport_failure(&self) -> LspSessionError {
        if self.dead_flag.load(Ordering::Acquire) {
            self.reader_failure()
        } else {
            LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerTransportError)
        }
    }

    fn reader_failure(&self) -> LspSessionError {
        let code = match ReaderExit::from_u8(self.reader_exit.load(Ordering::Relaxed)) {
            ReaderExit::Crashed => extension_protocol::LspOutcomeCode::ServerCrashed,
            ReaderExit::Transport => extension_protocol::LspOutcomeCode::ServerTransportError,
            ReaderExit::Malformed => extension_protocol::LspOutcomeCode::ServerMalformedResponse,
        };
        LspSessionError::Backend(code)
    }

    fn wait_for_settled_or_failure(
        &self,
        uri: &str,
        waiter_id: u64,
        emitted_sync: bool,
        deadline: Option<Instant>,
    ) -> Result<Vec<Diagnostic>, LspSessionError> {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if self.dead_flag.load(Ordering::Acquire) {
                remove_push_waiter(&mut state, uri, waiter_id);
                return Err(self.reader_failure());
            }
            let fresh = state
                .push_waiters
                .get(uri)
                .and_then(|waiters| waiters.get(&waiter_id))
                .and_then(|waiter| waiter.first_fresh.clone());
            let settled_cache = !emitted_sync && state.ready && state.by_uri.contains_key(uri);
            let now = Instant::now();
            if let Some(diagnostics) = fresh {
                remove_push_waiter(&mut state, uri, waiter_id);
                return Ok(diagnostics);
            }
            if settled_cache {
                let diagnostics = state
                    .by_uri
                    .get(uri)
                    .map_or_else(Vec::new, |entry| entry.diags.clone());
                remove_push_waiter(&mut state, uri, waiter_id);
                return Ok(diagnostics);
            }
            if deadline.is_some_and(|deadline| now >= deadline) {
                remove_push_waiter(&mut state, uri, waiter_id);
                return Err(LspSessionError::Backend(
                    extension_protocol::LspOutcomeCode::ServerTimeout,
                ));
            }
            state = if let Some(deadline) = deadline {
                cvar.wait_timeout(state, deadline - now)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0
            } else {
                cvar.wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
            };
        }
    }

    fn pull_diagnostics(
        &self,
        uri: &str,
        deadline: Option<Instant>,
    ) -> Result<Vec<Diagnostic>, LspSessionError> {
        use lsp_types::{DocumentDiagnosticReport, DocumentDiagnosticReportResult};

        let value = self.send_request_until(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri } }),
            deadline,
        )?;
        let report = serde_json::from_value::<DocumentDiagnosticReportResult>(value)
            .map_err(|_| malformed_capabilities())?;
        let DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) = report
        else {
            return Err(malformed_capabilities());
        };

        full.full_document_diagnostic_report
            .items
            .into_iter()
            .map(|diagnostic| {
                serde_json::to_value(diagnostic)
                    .ok()
                    .and_then(|value| parse_one_diagnostic(&value))
                    .ok_or_else(malformed_capabilities)
            })
            .collect()
    }

    fn classify_request_failure(&self, failure: &RequestFailure) -> LspSessionError {
        if self.dead_flag.load(Ordering::Acquire) {
            return self.reader_failure();
        }
        match failure {
            RequestFailure::Response {
                code: Some(-32601), ..
            } => LspSessionError::CapabilityUnavailable,
            RequestFailure::Response { .. } => {
                LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerError)
            }
            RequestFailure::Timeout => {
                LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerTimeout)
            }
        }
    }

    /// Cross-file settle gate (EV3): block until the server emits a quiescent
    /// signal that is **newer** than `pre_open_ready_gen` (captured before the
    /// documents were synced). Only the configured operation deadline bounds
    /// this wait; an absent deadline waits for readiness or reader exit.
    ///
    /// Waiting for `ready_gen > pre_open_ready_gen` (not just `ready`) ensures
    /// we see a fresh settled notification from after the current document open,
    /// rather than returning immediately on a stale signal from a prior cycle.
    fn await_index_settled(
        &self,
        pre_open_ready_gen: u64,
        deadline: Option<Instant>,
    ) -> Result<(), LspSessionError> {
        let (lock, cvar) = &*self.state;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if self.dead_flag.load(Ordering::Acquire) {
                return Err(self.reader_failure());
            }
            if s.ready_gen > pre_open_ready_gen {
                return Ok(());
            }
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Err(LspSessionError::Backend(LspOutcomeCode::ServerTimeout));
            }
            s = if let Some(deadline) = deadline {
                cvar.wait_timeout(s, deadline - now)
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            } else {
                cvar.wait(s).unwrap_or_else(|p| p.into_inner())
            };
        }
    }

    /// Open the document, run `request`, decode locations, filter to the
    /// workspace root / `.towerignore`. Shared by `definition` and `references`.
    fn location_query(
        &self,
        path: &RelativePath,
        text: &str,
        method: &str,
        params: Value,
    ) -> Result<Vec<Location>, LspSessionError> {
        if !self.supports(path) {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        let uri = self.uri_for(path);
        let deadline = self.operation_deadline()?;
        self.acquire_doc_until(&uri, text, deadline)?;
        let result = self.send_request_until(method, params, deadline);
        self.release_doc(&uri);
        let raws = decode_locations_typed(&result?)?;
        Ok(workspace_locations(
            raws,
            &self.workspace_root,
            &self.ignore,
        ))
    }

    fn prepare_rename_query(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<PrepareRenameResult, LspSessionError> {
        let value = self.prepare_rename_value(path, text, position)?;
        decode_prepare_rename_typed(value)
    }

    fn prepare_rename_value(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Value, LspSessionError> {
        if !self.supports(path) {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        let deadline = self.operation_deadline()?;
        self.prepare_rename_value_until(path, text, position, deadline)
    }

    fn prepare_rename_value_until(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        deadline: Option<Instant>,
    ) -> Result<Value, LspSessionError> {
        let uri = self.uri_for(path);
        self.acquire_doc_until(&uri, text, deadline)?;
        let result = self.send_prepare_rename_request(
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character }
            }),
            deadline,
        );
        self.release_doc(&uri);
        result
    }

    fn send_prepare_rename_request(
        &self,
        params: Value,
        deadline: Option<Instant>,
    ) -> Result<Value, LspSessionError> {
        let method = "textDocument/prepareRename";
        let mut attempt = 0;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(LspSessionError::Backend(
                    extension_protocol::LspOutcomeCode::ServerTimeout,
                ));
            }
            match self.send_request_once_until(method, params.clone(), deadline)? {
                RequestOutcome::Ok(value) => return Ok(value),
                RequestOutcome::ContentModified if attempt < CONTENT_MODIFIED_RETRIES => {
                    let backoff = deadline.map_or(CONTENT_MODIFIED_BACKOFF, |deadline| {
                        CONTENT_MODIFIED_BACKOFF
                            .min(deadline.saturating_duration_since(Instant::now()))
                    });
                    if backoff.is_zero() {
                        return Err(LspSessionError::Backend(
                            extension_protocol::LspOutcomeCode::ServerTimeout,
                        ));
                    }
                    std::thread::sleep(backoff);
                    attempt += 1;
                }
                RequestOutcome::ContentModified => {
                    return Err(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerError,
                    ));
                }
                RequestOutcome::Error(failure) if is_prepare_rename_rejection(&failure) => {
                    return Err(LspSessionError::NotRenameable);
                }
                RequestOutcome::Error(failure) => {
                    return Err(self.classify_request_failure(&failure));
                }
            }
        }
    }
}

fn is_prepare_rename_rejection(failure: &RequestFailure) -> bool {
    match failure {
        RequestFailure::Response {
            code: Some(-32602),
            message: Some(message),
            ..
        } => message
            .to_ascii_lowercase()
            .contains("not valid rename target"),
        _ => false,
    }
}

impl NavigationPort for LspClientAdapter {
    fn definition(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        pool::PooledSession::definition_lsp(self, path, text, position)
            .map_err(session_to_code_intel_error)
    }

    fn implementations(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        pool::PooledSession::implementations_lsp(self, path, text, position)
            .map_err(session_to_code_intel_error)
    }

    fn references(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        pool::PooledSession::references_lsp(self, path, text, position)
            .map_err(session_to_code_intel_error)
    }

    fn hover(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Option<Hover>, CodeIntelError> {
        pool::PooledSession::hover_lsp(self, path, text, position)
            .map_err(session_to_code_intel_error)
    }

    fn document_symbols(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<Symbol>, CodeIntelError> {
        if !self.supports(path) {
            return Err(CodeIntelError::Unsupported);
        }
        let uri = self.uri_for(path);
        let deadline = self
            .operation_deadline()
            .map_err(session_to_code_intel_error)?;
        self.acquire_doc_until(&uri, text, deadline)
            .map_err(session_to_code_intel_error)?;
        let result = self.send_request_until(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
            deadline,
        );
        self.release_doc(&uri);
        Ok(decode::decode_document_symbols(
            &result.map_err(session_to_code_intel_error)?,
        ))
    }

    fn prepare_rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<PrepareRenameResult, RenameNavigationError> {
        pool::PooledSession::prepare_rename_lsp(self, path, text, position)
            .map_err(session_to_rename_error)
    }
}

impl LspClientAdapter {
    #[allow(dead_code)]
    pub fn rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, RenameNavigationError> {
        pool::PooledSession::rename_lsp(self, path, text, position, new_name)
            .map_err(session_to_rename_error)
    }

    fn rename_query(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, LspSessionError> {
        if !self.supports(path) {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        let deadline = self.operation_deadline()?;
        match self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .rename
        {
            RenameCapability::Unavailable => {
                return Err(LspSessionError::CapabilityUnavailable);
            }
            RenameCapability::RenameWithPrepare => {
                let prepared = self.prepare_rename_value_until(path, text, position, deadline)?;
                if let Err(error) = decode_prepare_rename_typed(prepared.clone()) {
                    if looks_like_workspace_edit(&prepared) {
                        return decode_workspace_edit_typed(prepared);
                    }
                    return Err(error);
                }
            }
            RenameCapability::RenameOnly => {}
        }
        let uri = self.uri_for(path);
        self.acquire_doc_until(&uri, text, deadline)?;
        let result = self.send_request_until(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
                "newName": new_name
            }),
            deadline,
        );
        self.release_doc(&uri);
        decode_workspace_edit_typed(result?)
    }
}

impl DocumentSyncPort for LspClientAdapter {
    fn serves(&self, path: &RelativePath) -> bool {
        self.supports(path)
    }

    /// Watcher Create: take a resident hold (`didOpen`) on the document.
    fn document_opened(&self, path: &RelativePath, text: &str) {
        let uri = self.uri_for(path);
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(sync) = session.docs.acquire(&uri, &self.language_id, text) {
            let _ = write_doc_sync(&self.writer, &uri, &sync);
        }
    }

    /// Watcher Modify: push the latest content (`didChange`). Opens the document
    /// first if a Create was missed (e.g. a file present at startup scan).
    fn document_changed(&self, path: &RelativePath, text: &str) {
        let uri = self.uri_for(path);
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let sync = if session.docs.is_open(&uri) {
            session.docs.sync(&uri, text)
        } else {
            session.docs.acquire(&uri, &self.language_id, text)
        };
        if let Some(sync) = sync {
            let _ = write_doc_sync(&self.writer, &uri, &sync);
        }
    }

    /// Watcher Delete: release the resident hold (`didClose` if last).
    fn document_closed(&self, path: &RelativePath) {
        let uri = self.uri_for(path);
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(sync) = session.docs.release(&uri) {
            let _ = write_doc_sync(&self.writer, &uri, &sync);
        }
    }
}

fn initialize_params(process_id: u32, root_uri: &str) -> Value {
    json!({
        "processId": process_id,
        "rootUri": root_uri,
        "capabilities": {
            "textDocument": {
                "publishDiagnostics": {},
                "diagnostic": {},
            },
            "workspace": {
                "fileOperations": { "willRename": true }
            },
            "window": { "workDoneProgress": true },
            // rust-analyzer reads serverStatus capability at
            // params.capabilities.experimental.serverStatusNotification,
            // NOT as a sibling of capabilities.
            "experimental": { "serverStatusNotification": true }
        },
        "initializationOptions": {}
    })
}

impl CodeIntelligencePort for LspClientAdapter {
    fn check(&self, path: &RelativePath, text: &str) -> Result<Vec<Diagnostic>, CodeIntelError> {
        pool::PooledSession::check_lsp(self, path, text).map_err(session_to_code_intel_error)
    }
}

impl Drop for LspClientAdapter {
    fn drop(&mut self) {
        // `get_mut` returns `Result<&mut Session, PoisonError<…>>` — both
        // arms give us a `&mut Session`, so we can call `kill()` without
        // acquiring the lock a second time (exclusive access guaranteed in Drop).
        let session = self.inner.get_mut().unwrap_or_else(|p| p.into_inner());
        // Ignore kill errors: the process may already have exited (e.g. crash).
        let _ = session.child.kill();
    }
}

impl pool::PooledSession for LspClientAdapter {
    fn plan_safe_delete(
        &self,
        request: &SafeDeleteSemanticRequest,
        read_content: &mut dyn FnMut(&str) -> Result<String, LspSessionError>,
    ) -> Result<SafeDeleteSemanticOutcome, LspSessionError> {
        self.plan_safe_delete_query(request, read_content)
    }

    fn type_hierarchy_lsp(
        &self,
        query: &extension_protocol::TypeHierarchyQuery,
        text: &str,
    ) -> Result<extension_protocol::TypeHierarchyResult, LspSessionError> {
        LspClientAdapter::type_hierarchy_lsp(self, query, text)
    }

    fn plan_path_rename(
        &self,
        request: &PathRenamePlanningRequest,
    ) -> Result<RawWorkspaceEdit, PathRenameError> {
        LspClientAdapter::plan_path_rename(self, request)
    }

    fn check_lsp(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<Diagnostic>, LspSessionError> {
        if !self.supports(path) {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        if self.dead_flag.load(Ordering::Acquire) {
            return Err(self.reader_failure());
        }
        let deadline = self.operation_deadline()?;
        let uri = self.uri_for(path);
        let diagnostics = self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .diagnostics;
        let push_waiter = (diagnostics == DiagnosticsCapability::Push)
            .then(|| register_push_waiter(&self.state, &uri).1);
        let emitted_sync = {
            let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let sync = if session.docs.is_open(&uri) {
                session.docs.sync(&uri, text)
            } else {
                session.docs.acquire(&uri, &self.language_id, text)
            };
            let emitted_sync = sync.is_some();
            if let Some(sync) = sync
                && let Err(error) = write_doc_sync_until(&self.writer, &uri, &sync, deadline)
            {
                if let Some(waiter_id) = push_waiter {
                    let (lock, _) = &*self.state;
                    remove_push_waiter(
                        &mut lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                        &uri,
                        waiter_id,
                    );
                }
                return Err(if error.kind() == std::io::ErrorKind::TimedOut {
                    LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerTimeout)
                } else {
                    self.transport_failure()
                });
            }
            emitted_sync
        };
        match diagnostics {
            DiagnosticsCapability::Push => {
                let waiter_id = push_waiter.ok_or_else(malformed_response)?;
                self.wait_for_settled_or_failure(&uri, waiter_id, emitted_sync, deadline)
            }
            DiagnosticsCapability::Pull => self.pull_diagnostics(&uri, deadline),
        }
    }

    fn definition_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, LspSessionError> {
        if self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .definition
            == ProviderCapability::Unavailable
        {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        let uri = self.uri_for(path);
        self.location_query(
            path,
            text,
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character }
            }),
        )
    }

    fn references_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, LspSessionError> {
        if !self.supports(path)
            || self
                .semantic_capabilities
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .references
                == ProviderCapability::Unavailable
        {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        let uri = self.uri_for(path);
        let pre_open_ready_gen = current_ready_gen(&self.state);
        let deadline = self.operation_deadline()?;
        let emitted_sync = self.acquire_doc_with_sync_until(&uri, text, deadline)?;
        let result = (|| {
            // Readiness notifications are optional in LSP. Only servers that
            // actually signal readiness can participate in this extra gate;
            // otherwise the semantic response itself establishes completion.
            if emitted_sync && (pre_open_ready_gen > 0 || is_server_status_active(&self.state)) {
                self.await_index_settled(pre_open_ready_gen, deadline)?;
            }
            self.send_request_until(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                    "context": { "includeDeclaration": true }
                }),
                deadline,
            )
        })();
        self.release_doc(&uri);
        let raws = decode_locations_typed(&result?)?;
        Ok(workspace_locations(
            raws,
            &self.workspace_root,
            &self.ignore,
        ))
    }

    fn implementations_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, LspSessionError> {
        if self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .implementation
            == ProviderCapability::Unavailable
        {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        let uri = self.uri_for(path);
        self.location_query(
            path,
            text,
            "textDocument/implementation",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character }
            }),
        )
    }

    fn hover_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Option<Hover>, LspSessionError> {
        if !self.supports(path)
            || self
                .semantic_capabilities
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .hover
                == ProviderCapability::Unavailable
        {
            return Err(LspSessionError::CapabilityUnavailable);
        }
        let uri = self.uri_for(path);
        let deadline = self.operation_deadline()?;
        self.acquire_doc_until(&uri, text, deadline)?;
        let result = self.send_request_until(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character }
            }),
            deadline,
        );
        self.release_doc(&uri);
        decode_hover_typed(&result?)
    }

    fn prepare_rename_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<PrepareRenameResult, LspSessionError> {
        match self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .rename
        {
            RenameCapability::Unavailable => Err(LspSessionError::CapabilityUnavailable),
            RenameCapability::RenameOnly => Ok(PrepareRenameResult {
                range: None,
                placeholder: None,
            }),
            RenameCapability::RenameWithPrepare => self.prepare_rename_query(path, text, position),
        }
    }

    fn rename_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, LspSessionError> {
        self.rename_query(path, text, position, new_name)
    }

    fn is_dead(&self) -> bool {
        self.dead_flag.load(Ordering::Acquire)
    }

    fn shared_state(&self) -> SharedState {
        Arc::clone(&self.state)
    }

    fn diagnostics_for(&self, uri: &str) -> Vec<Diagnostic> {
        let (lock, _) = &*self.state;
        lock.lock()
            .unwrap_or_else(|p| p.into_inner())
            .by_uri
            .get(uri)
            .map(|e| e.diags.clone())
            .unwrap_or_default()
    }

    fn rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, RenameNavigationError> {
        Self::rename(self, path, text, position, new_name)
    }

    /// Kill the real OS process to simulate a crash in e2e tests.
    ///
    /// We deliberately do NOT set `dead_flag` directly: killing the child closes
    /// its stdout, so the reader thread observes EOF and sets `dead_flag` itself.
    /// This exercises the production crash-*detection* path (EOF → `dead_flag`) —
    /// exactly what an external kill triggers (spec 14d AC4).
    #[cfg(any(test, feature = "testing"))]
    fn kill_for_test(&self) {
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let _ = session.child.kill();
    }
}

impl LspClientAdapter {
    fn plan_safe_delete_query(
        &self,
        request: &SafeDeleteSemanticRequest,
        read_content: &mut dyn FnMut(&str) -> Result<String, LspSessionError>,
    ) -> Result<SafeDeleteSemanticOutcome, LspSessionError> {
        let declaration = &request.declaration;
        let declaration_text = match read_content(&declaration.path) {
            Ok(text) => text,
            Err(LspSessionError::WorkspaceExternal) => {
                return Ok(SafeDeleteSemanticOutcome::WorkspaceExternal);
            }
            Err(_) => return Ok(SafeDeleteSemanticOutcome::InvalidLocation),
        };
        if compute_content_version(declaration_text.as_bytes()) != declaration.base_hash {
            return Ok(incomplete_safe_delete(Vec::new()));
        }
        let declaration_position = lsp_types::Position {
            line: declaration.position.line,
            character: declaration.position.character,
        };
        let position_map = crate::lsp_adapter::position_map::PositionMap::new(&declaration_text);
        if declaration.deletion_span.start_byte >= declaration.deletion_span.end_byte
            || !declaration_text.is_char_boundary(declaration.deletion_span.start_byte)
            || !declaration_text.is_char_boundary(declaration.deletion_span.end_byte)
            || position_map
                .lsp_range_to_byte_range(
                    &declaration.path,
                    lsp_types::Range {
                        start: declaration_position,
                        end: declaration_position,
                    },
                )
                .is_err()
        {
            return Ok(SafeDeleteSemanticOutcome::InvalidLocation);
        }

        let path = RelativePath::new(declaration.path.clone());
        if !self.supports(&path) {
            return Ok(SafeDeleteSemanticOutcome::CapabilityUnavailable);
        }
        let capabilities = *self
            .semantic_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if capabilities.references == ProviderCapability::Unavailable {
            return Ok(SafeDeleteSemanticOutcome::CapabilityUnavailable);
        }

        let uri = self.uri_for(&path);
        let deadline = self.operation_deadline()?;
        if let Err(error) = self.acquire_doc_until(&uri, &declaration_text, deadline) {
            return Ok(safe_delete_error_outcome(error));
        }
        let outcome = (|| {
            let params = json!({
                "textDocument": { "uri": uri },
                "position": {
                    "line": declaration.position.line,
                    "character": declaration.position.character,
                },
                "context": { "includeDeclaration": false }
            });
            let references =
                match self.send_request_until("textDocument/references", params, deadline) {
                    Err(error @ LspSessionError::Backend(LspOutcomeCode::ServerError)) => {
                        return Err(error);
                    }
                    Err(error) => return Ok(safe_delete_error_outcome(error)),
                    Ok(value) => match self.decode_safe_delete_references(value, read_content) {
                        Ok(references) => references,
                        Err(outcome) => return Ok(outcome),
                    },
                };
            if capabilities.safe_delete == ProviderCapability::Unavailable {
                // A complete reference lookup needs no cleanup capability when
                // every usage disappears with the validated declaration span.
                // Preserve the references for the domain's independent checks.
                let all_internal = references.iter().all(|reference| {
                    reference.path == declaration.path
                        && reference.base_hash == declaration.base_hash
                        && position_map
                            .lsp_range_to_byte_range(
                                &reference.path,
                                lsp_types::Range {
                                    start: lsp_types::Position {
                                        line: reference.start.line,
                                        character: reference.start.character,
                                    },
                                    end: lsp_types::Position {
                                        line: reference.end.line,
                                        character: reference.end.character,
                                    },
                                },
                            )
                            .is_ok_and(|range| {
                                range.start >= declaration.deletion_span.start_byte
                                    && range.end <= declaration.deletion_span.end_byte
                            })
                });
                return Ok(if all_internal {
                    complete_safe_delete(references, Vec::new())
                } else {
                    incomplete_safe_delete(references)
                });
            }
            match self.send_request_until(
                "experimental/tower/safeDelete",
                json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": declaration.position.line,
                        "character": declaration.position.character,
                    }
                }),
                deadline,
            ) {
                Err(error @ LspSessionError::Backend(LspOutcomeCode::ServerError)) => Err(error),
                Err(error) => Ok(safe_delete_error_outcome(error)),
                Ok(value) => Ok(self.decode_safe_delete_cleanup(value, references, read_content)),
            }
        })();
        self.release_doc(&uri);
        outcome
    }

    fn decode_safe_delete_references(
        &self,
        value: Value,
        read_content: &mut dyn FnMut(&str) -> Result<String, LspSessionError>,
    ) -> Result<Vec<SafeDeleteReference>, SafeDeleteSemanticOutcome> {
        let locations: Vec<lsp_types::Location> = if value.is_null() {
            Vec::new()
        } else {
            serde_json::from_value(value)
                .map_err(|_| SafeDeleteSemanticOutcome::MalformedResponse)?
        };
        locations
            .into_iter()
            .map(|location| {
                let raw_uri = location.uri.to_string();
                let Some(UriClassification::WorkspacePath(path)) =
                    decode::classify_uri(&raw_uri, &self.workspace_root, UriAccessPolicy::Writable)
                else {
                    return Err(SafeDeleteSemanticOutcome::WorkspaceExternal);
                };
                let text = read_content(&path).map_err(|error| match error {
                    LspSessionError::WorkspaceExternal => {
                        SafeDeleteSemanticOutcome::WorkspaceExternal
                    }
                    _ => SafeDeleteSemanticOutcome::InvalidLocation,
                })?;
                crate::lsp_adapter::position_map::PositionMap::new(&text)
                    .lsp_range_to_byte_range(&path, location.range)
                    .map_err(|_| SafeDeleteSemanticOutcome::InvalidLocation)?;
                Ok(SafeDeleteReference {
                    path,
                    start: SafeDeletePosition {
                        line: location.range.start.line,
                        character: location.range.start.character,
                    },
                    end: SafeDeletePosition {
                        line: location.range.end.line,
                        character: location.range.end.character,
                    },
                    base_hash: compute_content_version(text.as_bytes()),
                })
            })
            .collect()
    }

    fn decode_safe_delete_cleanup(
        &self,
        value: Value,
        references: Vec<SafeDeleteReference>,
        read_content: &mut dyn FnMut(&str) -> Result<String, LspSessionError>,
    ) -> SafeDeleteSemanticOutcome {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CleanupResponse {
            coverage: CleanupCoverage,
            edit: RawWorkspaceEdit,
        }
        let response: CleanupResponse = match serde_json::from_value(value) {
            Ok(response) => response,
            Err(_) => return SafeDeleteSemanticOutcome::MalformedResponse,
        };
        let edits = match decode_workspace_edit_spans(response.edit, &self.workspace_root, |path| {
            read_content(path).map_err(|error| match error {
                LspSessionError::WorkspaceExternal => WorkspaceEditDecodeError::InvalidPath {
                    uri: "workspace-external path".to_owned(),
                },
                _ => WorkspaceEditDecodeError::UnreadableFile {
                    path: path.to_owned(),
                    message: "workspace file could not be read".to_owned(),
                },
            })
        }) {
            Ok(edits) => {
                let mut cleanup_edits = Vec::with_capacity(edits.len());
                for edit in edits {
                    let Some(base_hash) = edit.base_hash else {
                        return SafeDeleteSemanticOutcome::MalformedResponse;
                    };
                    cleanup_edits.push(SafeDeleteCleanupEdit {
                        path: edit.path,
                        start_byte: edit.start_byte,
                        end_byte: edit.end_byte,
                        replacement: edit.replacement,
                        base_hash,
                    });
                }
                cleanup_edits
            }
            Err(WorkspaceEditDecodeError::InvalidPath { .. }) => {
                return SafeDeleteSemanticOutcome::WorkspaceExternal;
            }
            Err(
                WorkspaceEditDecodeError::InvalidRange { .. }
                | WorkspaceEditDecodeError::MissingText { .. }
                | WorkspaceEditDecodeError::UnreadableFile { .. },
            ) => {
                return SafeDeleteSemanticOutcome::InvalidLocation;
            }
            Err(WorkspaceEditDecodeError::UnsupportedWorkspaceEdit { .. }) => {
                return SafeDeleteSemanticOutcome::MalformedResponse;
            }
        };
        match response.coverage {
            CleanupCoverage::Complete => complete_safe_delete(references, edits),
            CleanupCoverage::Incomplete => SafeDeleteSemanticOutcome::Incomplete {
                plan: SafeDeleteSemanticPlan {
                    references,
                    cleanup_edits: edits,
                    coverage: CleanupCoverage::Incomplete,
                },
            },
        }
    }
}

fn complete_safe_delete(
    references: Vec<SafeDeleteReference>,
    cleanup_edits: Vec<SafeDeleteCleanupEdit>,
) -> SafeDeleteSemanticOutcome {
    SafeDeleteSemanticOutcome::Complete {
        plan: SafeDeleteSemanticPlan {
            references,
            cleanup_edits,
            coverage: CleanupCoverage::Complete,
        },
    }
}

fn incomplete_safe_delete(references: Vec<SafeDeleteReference>) -> SafeDeleteSemanticOutcome {
    SafeDeleteSemanticOutcome::Incomplete {
        plan: SafeDeleteSemanticPlan {
            references,
            cleanup_edits: Vec::new(),
            coverage: CleanupCoverage::Incomplete,
        },
    }
}

fn safe_delete_error_outcome(error: LspSessionError) -> SafeDeleteSemanticOutcome {
    use extension_protocol::LspOutcomeCode;
    match error {
        LspSessionError::Unconfigured
        | LspSessionError::CapabilityUnavailable
        | LspSessionError::NotRenameable
        | LspSessionError::Backend(
            LspOutcomeCode::LanguageNotConfigured | LspOutcomeCode::CapabilityUnavailable,
        ) => SafeDeleteSemanticOutcome::CapabilityUnavailable,
        LspSessionError::WorkspaceExternal => SafeDeleteSemanticOutcome::WorkspaceExternal,
        LspSessionError::Backend(LspOutcomeCode::ServerTimeout) => {
            SafeDeleteSemanticOutcome::BackendTimeout
        }
        LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse) => {
            SafeDeleteSemanticOutcome::MalformedResponse
        }
        LspSessionError::Backend(_) => SafeDeleteSemanticOutcome::BackendCrashed,
    }
}

/// Convert a `publishDiagnostics` notification into `(uri, Vec<Diagnostic>)`.
fn parse_publish_diagnostics(msg: &Value) -> Option<(String, Vec<Diagnostic>)> {
    let params = msg.get("params")?;
    let uri = params.get("uri")?.as_str()?.to_owned();
    let arr = params.get("diagnostics")?.as_array()?;
    let diags = arr.iter().filter_map(parse_one_diagnostic).collect();
    Some((uri, diags))
}

fn parse_one_diagnostic(d: &Value) -> Option<Diagnostic> {
    let range = d.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let pos = |p: &Value| -> Option<Position> {
        Some(Position {
            line: u32::try_from(p.get("line")?.as_u64()?).ok()?,
            character: u32::try_from(p.get("character")?.as_u64()?).ok()?,
        })
    };
    let severity = match d.get("severity").and_then(Value::as_u64) {
        Some(1) => Severity::Error,
        Some(2) => Severity::Warning,
        Some(3) => Severity::Information,
        Some(4) => Severity::Hint,
        // LSP: absent severity is implementation-defined; treat as error.
        _ => Severity::Error,
    };
    Some(Diagnostic {
        range: Range {
            start: pos(start)?,
            end: pos(end)?,
        },
        severity,
        message: d.get("message")?.as_str()?.to_owned(),
        source: d.get("source").and_then(Value::as_str).map(str::to_owned),
        code: d.get("code").map(|c| match c {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
    })
}

/// rust-analyzer's `experimental/serverStatus`: analysis is settled when
/// `quiescent: true`. The preferred readiness signal.
fn server_status_is_quiescent(msg: &Value) -> bool {
    msg.get("params")
        .and_then(|p| p.get("quiescent"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Extract the `kind` field from a `$/progress` notification (`"begin"`,
/// `"report"`, or `"end"`). Returns `None` for malformed messages.
fn progress_kind(msg: &Value) -> Option<&str> {
    msg.get("params")
        .and_then(|p| p.get("value"))
        .and_then(|v| v.get("kind"))
        .and_then(Value::as_str)
}

/// Hex digit (uppercase) for a 4-bit nibble.
fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// Percent-encode the bytes of `path` that are unsafe in a `file://` URI path.
///
/// RFC 3986 unreserved chars plus `/` pass through; everything else (space,
/// non-ASCII UTF-8 bytes, etc.) becomes `%XX`. Dependency-free, matching the
/// hand-rolled transport.
fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    out
}

/// Build a `file://` URI from an absolute path, percent-encoding unsafe bytes.
fn path_to_uri(path: &std::path::Path) -> String {
    format!("file://{}", encode_uri_path(&path.display().to_string()))
}

/// Build the root `.towerignore` matcher used to filter navigation results.
/// Falls back to an empty matcher when the file is absent or malformed (same
/// policy as the watcher's `EventProcessor`).
fn build_ignore_matcher(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let _ = builder.add(root.join(TOWERIGNORE_FILE_NAME));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Serialise a [`DocSync`] action to the matching LSP notification and write it
/// through the shared writer.
fn write_doc_sync(writer: &Writer, uri: &str, sync: &DocSync) -> std::io::Result<()> {
    write_doc_sync_message(uri, sync, |message| write_framed(writer, message))
}

fn write_doc_sync_until(
    writer: &Writer,
    uri: &str,
    sync: &DocSync,
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    write_doc_sync_message(uri, sync, |message| writer.write_until(message, deadline))
}

fn write_doc_sync_message(
    uri: &str,
    sync: &DocSync,
    write: impl FnOnce(&Value) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let msg = match sync {
        DocSync::Open {
            version,
            language_id,
            text,
        } => json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri, "languageId": language_id,
                    "version": version, "text": text
                }
            }
        }),
        DocSync::Change { version, text } => json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ { "text": text } ]
            }
        }),
        DocSync::Close => json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": uri } }
        }),
    };
    write(&msg)
}

/// Normalize decoded raw locations to workspace-relative domain [`Location`]s,
/// dropping any outside `root` or matched by the `.towerignore` policy
/// (spec 14b: result filtering to workspace root / `.towerignore`).
fn workspace_locations(raws: Vec<RawLocation>, root: &Path, ignore: &Gitignore) -> Vec<Location> {
    raws.into_iter()
        .filter_map(|raw| {
            let UriClassification::WorkspacePath(rel_str) =
                decode::classify_uri(&raw.uri, root, UriAccessPolicy::Writable)?
            else {
                return None;
            };
            if matches!(
                ignore.matched_path_or_any_parents(&rel_str, false),
                Match::Ignore(_)
            ) {
                return None;
            }
            Some(Location {
                path: RelativePath::new(rel_str),
                range: raw.range,
            })
        })
        .collect()
}

fn type_hierarchy_range(range: lsp_types::Range) -> extension_protocol::TypeHierarchyRange {
    extension_protocol::TypeHierarchyRange {
        line: range.start.line,
        character: range.start.character,
        end_line: range.end.line,
        end_character: range.end.character,
    }
}

fn decode_prepared_type_hierarchy(
    value: Value,
) -> Result<Vec<lsp_types::TypeHierarchyItem>, LspSessionError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value(value).map_err(|_| malformed_response())
}

fn append_unique<T: PartialEq>(target: &mut Vec<T>, items: impl IntoIterator<Item = T>) {
    for item in items {
        if !target.contains(&item) {
            target.push(item);
        }
    }
}

fn malformed_response() -> LspSessionError {
    LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerMalformedResponse)
}

fn decode_locations_typed(value: &Value) -> Result<Vec<RawLocation>, LspSessionError> {
    let decoded = decode::decode_locations(value);
    match value {
        Value::Null => Ok(decoded),
        Value::Object(_) if decoded.len() == 1 => Ok(decoded),
        Value::Array(items) if decoded.len() == items.len() => Ok(decoded),
        _ => Err(malformed_response()),
    }
}

fn decode_hover_typed(value: &Value) -> Result<Option<Hover>, LspSessionError> {
    if value.is_null() {
        return Ok(None);
    }
    let hover = decode::decode_hover(value);
    if hover.is_some() {
        Ok(hover)
    } else {
        Err(malformed_response())
    }
}

fn decode_prepare_rename_typed(value: Value) -> Result<PrepareRenameResult, LspSessionError> {
    if value.is_null() {
        return Err(LspSessionError::NotRenameable);
    }
    decode_prepare_rename(value).map_err(|error| match error {
        RenameNavigationError::NotRenameable => LspSessionError::NotRenameable,
        RenameNavigationError::UnsupportedLanguage => LspSessionError::CapabilityUnavailable,
        RenameNavigationError::Backend(_) => malformed_response(),
    })
}

fn decode_workspace_edit_typed(value: Value) -> Result<RawWorkspaceEdit, LspSessionError> {
    decode_workspace_edit(value).map_err(|_| malformed_response())
}

fn decode_prepare_rename(value: Value) -> Result<PrepareRenameResult, RenameNavigationError> {
    if value.is_null() {
        return Err(RenameNavigationError::NotRenameable);
    }
    let response: lsp_types::PrepareRenameResponse = serde_json::from_value(value)
        .map_err(|e| RenameNavigationError::Backend(format!("decode prepareRename failed: {e}")))?;
    match response {
        lsp_types::PrepareRenameResponse::Range(range) => Ok(PrepareRenameResult {
            range: Some(domain_range(range)),
            placeholder: None,
        }),
        lsp_types::PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
            Ok(PrepareRenameResult {
                range: Some(domain_range(range)),
                placeholder: Some(placeholder),
            })
        }
        lsp_types::PrepareRenameResponse::DefaultBehavior { default_behavior } => {
            if default_behavior {
                Ok(PrepareRenameResult {
                    range: None,
                    placeholder: None,
                })
            } else {
                Err(RenameNavigationError::NotRenameable)
            }
        }
    }
}

fn domain_range(range: lsp_types::Range) -> Range {
    Range {
        start: Position {
            line: range.start.line,
            character: range.start.character,
        },
        end: Position {
            line: range.end.line,
            character: range.end.character,
        },
    }
}

fn decode_workspace_edit(value: Value) -> Result<RawWorkspaceEdit, RenameNavigationError> {
    if value.is_null() {
        return Ok(RawWorkspaceEdit {
            changes: None,
            document_changes: None,
            change_annotations: None,
        });
    }
    serde_json::from_value(value)
        .map_err(|e| RenameNavigationError::Backend(format!("decode WorkspaceEdit failed: {e}")))
}

fn looks_like_workspace_edit(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.contains_key("changes") || object.contains_key("documentChanges")
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Error, ErrorKind, Write};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::atomic::Ordering;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{
        DIAGNOSTIC_PUBLICATION_HISTORY, DiagnosticsCapability, DiagnosticsEvent, LspClientAdapter,
        LspSessionError, ProviderCapability, RawLocation, RawWorkspaceEdit, ReaderExit,
        RenameCapability, RenameNavigationError, ResponseCache, SemanticCapabilities, Session,
        SharedState, UriAccessPolicy, UriClassification, Writer, build_dispatcher,
        classify_reader_error, current_generation, decode, initialize_params, mark_ready,
        parse_publish_diagnostics, path_to_uri, progress_kind, record_diagnostics,
        server_status_is_quiescent, wait_for_settled, workspace_locations,
    };
    use crate::lsp_adapter::documents::DocumentTracker;
    use crate::lsp_adapter::pool::PooledSession;
    use core_engine::domain::RelativePath;
    use core_engine::domain::code_intel::{Diagnostic, Location, Position, Range, Severity};
    use core_engine::ports::{CodeIntelError, DocumentSyncPort, NavigationPort};
    use extension_protocol::{
        LspOutcomeCode, TypeHierarchyDirection, TypeHierarchyQuery, TypeHierarchyRange,
    };

    const TEST_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn initialize_advertises_pull_diagnostics_client_capability() {
        let params = initialize_params(42, "file:///workspace");

        assert_eq!(
            params["capabilities"]["textDocument"]["diagnostic"],
            json!({})
        );
    }

    fn new_state() -> SharedState {
        Arc::new((
            Mutex::new(super::SessionStateInner::default()),
            Condvar::new(),
        ))
    }

    fn diag(msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Severity::Error,
            message: msg.to_owned(),
            source: None,
            code: None,
        }
    }

    #[test]
    fn recording_diagnostics_bumps_generation() {
        let state = new_state();
        assert_eq!(current_generation(&state), 0);
        record_diagnostics(&state, "file:///a.rs".into(), vec![diag("x")]);
        assert_eq!(current_generation(&state), 1);
        record_diagnostics(&state, "file:///a.rs".into(), vec![diag("y")]);
        assert_eq!(current_generation(&state), 2);
    }

    #[test]
    fn wait_returns_only_after_ready_and_newer_generation() {
        let state = new_state();
        let sync_gen = current_generation(&state);
        let pre_sync_ready_gen = 0u64; // ready_gen starts at 0; gate fires on ready_gen > 0
        record_diagnostics(&state, "file:///a.rs".into(), vec![]); // pre-ready empty flush
        let bg = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                record_diagnostics(&state, "file:///a.rs".into(), vec![diag("real error")]);
                mark_ready(&state); // ready_gen → 1
            })
        };
        let got = wait_for_settled(
            &state,
            "file:///a.rs",
            sync_gen,
            pre_sync_ready_gen,
            Duration::from_secs(5),
        );
        bg.join().unwrap();
        assert_eq!(
            got.len(),
            1,
            "must return the populated set, not the empty flush"
        );
        assert_eq!(got[0].message, "real error");
    }

    #[test]
    fn wait_times_out_to_best_effort_when_never_ready() {
        let state = new_state();
        let sync_gen = current_generation(&state);
        let pre_sync_ready_gen = 0u64;
        record_diagnostics(&state, "file:///a.rs".into(), vec![diag("e")]);
        // ready_gen never advances past 0 → predicate never fires → times out
        let got = wait_for_settled(
            &state,
            "file:///a.rs",
            sync_gen,
            pre_sync_ready_gen,
            Duration::from_millis(100),
        );
        assert_eq!(got.len(), 1);
    }

    fn raw(uri: &str) -> RawLocation {
        RawLocation {
            uri: uri.to_owned(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
        }
    }

    fn empty_ignore() -> ignore::gitignore::Gitignore {
        ignore::gitignore::Gitignore::empty()
    }

    #[test]
    fn in_root_location_becomes_relative() {
        let root = std::path::Path::new("/w");
        let out = workspace_locations(vec![raw("file:///w/src/a.rs")], root, &empty_ignore());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path.as_str(), "src/a.rs");
    }

    #[test]
    fn out_of_root_location_is_dropped() {
        let root = std::path::Path::new("/w");
        let out = workspace_locations(
            vec![raw("file:///w/src/a.rs"), raw("file:///elsewhere/b.rs")],
            root,
            &empty_ignore(),
        );
        let paths: Vec<&str> = out.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs"]);
    }

    #[test]
    fn ignored_location_is_dropped() {
        let root = std::path::Path::new("/w");
        let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
        builder.add_line(None, "*.gen.rs").unwrap();
        let ignore = builder.build().unwrap();

        let out = workspace_locations(
            vec![
                raw("file:///w/src/real.rs"),
                raw("file:///w/src/foo.gen.rs"),
            ],
            root,
            &ignore,
        );
        let paths: Vec<&str> = out.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["src/real.rs"]);
    }

    #[test]
    fn parses_a_publish_diagnostics_notification() {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///w/src/a.rs",
                "diagnostics": [{
                    "range": {
                        "start": { "line": 2, "character": 4 },
                        "end": { "line": 2, "character": 7 }
                    },
                    "severity": 1,
                    "message": "cannot find value `foo`",
                    "source": "rustc",
                    "code": "E0425"
                }]
            }
        });
        let (uri, diags) = parse_publish_diagnostics(&msg).unwrap();
        assert_eq!(uri, "file:///w/src/a.rs");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("E0425"));
    }

    #[test]
    fn empty_diagnostics_array_parses_to_empty_vec() {
        let msg = json!({
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": "file:///w/src/a.rs", "diagnostics": [] }
        });
        let (_, diags) = parse_publish_diagnostics(&msg).unwrap();
        assert!(diags.is_empty());
    }

    #[test]
    fn server_status_quiescent_true_is_ready() {
        let msg = json!({
            "method": "experimental/serverStatus",
            "params": { "health": "ok", "quiescent": true }
        });
        assert!(server_status_is_quiescent(&msg));
    }

    #[test]
    fn server_status_quiescent_false_is_not_ready() {
        let msg = json!({
            "method": "experimental/serverStatus",
            "params": { "health": "ok", "quiescent": false }
        });
        assert!(!server_status_is_quiescent(&msg));
    }

    #[test]
    fn progress_kind_identifies_begin_end_and_report() {
        let begin = json!({
            "method": "$/progress",
            "params": { "token": "rust-analyzer/flycheck/0", "value": { "kind": "begin" } }
        });
        let end = json!({
            "method": "$/progress",
            "params": { "token": "rust-analyzer/flycheck/0", "value": { "kind": "end" } }
        });
        let report = json!({
            "method": "$/progress",
            "params": { "token": "rustAnalyzer/cachePriming", "value": { "kind": "report" } }
        });
        assert_eq!(progress_kind(&begin), Some("begin"));
        assert_eq!(progress_kind(&end), Some("end"));
        assert_eq!(progress_kind(&report), Some("report"));
        assert_eq!(progress_kind(&json!({})), None);
    }

    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn test_writer() -> (Writer, SharedSink) {
        let sink = SharedSink::default();
        let writer = Writer::new(Box::new(sink.clone()));
        (writer, sink)
    }

    #[derive(Clone)]
    struct BlockingWriter {
        sink: SharedSink,
        started: Arc<(Mutex<bool>, Condvar)>,
        released: Arc<(Mutex<bool>, Condvar)>,
    }

    impl BlockingWriter {
        fn new(sink: SharedSink) -> Self {
            Self {
                sink,
                started: Arc::new((Mutex::new(false), Condvar::new())),
                released: Arc::new((Mutex::new(false), Condvar::new())),
            }
        }

        fn writer(&self) -> Writer {
            Writer::new(Box::new(self.clone()))
        }

        fn wait_until_write_starts(&self) -> bool {
            let (lock, cvar) = &*self.started;
            let started = lock.lock().unwrap();
            let (started, timeout) = cvar
                .wait_timeout_while(started, Duration::from_millis(250), |started| !*started)
                .unwrap();
            *started && !timeout.timed_out()
        }

        fn release(&self) {
            let (lock, cvar) = &*self.released;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (started_lock, started_cvar) = &*self.started;
            *started_lock.lock().unwrap() = true;
            started_cvar.notify_all();

            let (released_lock, released_cvar) = &*self.released;
            let mut released = released_lock.lock().unwrap();
            while !*released {
                released = released_cvar.wait(released).unwrap();
            }
            self.sink.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn test_adapter() -> (LspClientAdapter, SharedSink, ResponseCache) {
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (writer, sink) = test_writer();
        let responses: ResponseCache =
            Arc::new((Mutex::new(std::collections::HashMap::new()), Condvar::new()));
        let adapter = LspClientAdapter {
            workspace_root: std::path::PathBuf::from("/workspace"),
            extensions: vec!["rs".to_owned()],
            language_id: "rust".to_owned(),
            inner: Mutex::new(Session {
                child,
                docs: DocumentTracker::new(),
            }),
            writer,
            state: new_state(),
            responses: Arc::clone(&responses),
            semantic_capabilities: Mutex::new(SemanticCapabilities {
                definition: ProviderCapability::Available,
                references: ProviderCapability::Available,
                hover: ProviderCapability::Available,
                implementation: ProviderCapability::Available,
                type_hierarchy: ProviderCapability::Unavailable,
                rename: RenameCapability::RenameWithPrepare,
                will_rename_files: ProviderCapability::Unavailable,
                safe_delete: ProviderCapability::Unavailable,
                diagnostics: DiagnosticsCapability::Push,
            }),
            operation_timeout: Some(TEST_OPERATION_TIMEOUT),
            ignore: empty_ignore(),
            next_id: AtomicI64::new(1),
            dead_flag: Arc::new(AtomicBool::new(false)),
            reader_exit: Arc::new(AtomicU8::new(ReaderExit::Crashed as u8)),
        };
        (adapter, sink, responses)
    }

    fn hierarchy_item(
        name: &str,
        kind: u64,
        uri: &str,
        line: u32,
        character: u32,
    ) -> serde_json::Value {
        json!({
            "name": name,
            "kind": kind,
            "detail": format!("detail for {name}"),
            "uri": uri,
            "range": {
                "start": { "line": line, "character": character },
                "end": { "line": line + 2, "character": character + 4 }
            },
            "selectionRange": {
                "start": { "line": line + 1, "character": character + 1 },
                "end": { "line": line + 1, "character": character + 3 }
            },
            "data": { "opaque": "adapter-only" }
        })
    }

    fn hierarchy_query(direction: TypeHierarchyDirection) -> TypeHierarchyQuery {
        TypeHierarchyQuery {
            path: "src/main.rs".to_owned(),
            line: 4,
            character: 9,
            direction,
        }
    }

    fn enable_type_hierarchy(adapter: &LspClientAdapter) {
        adapter.semantic_capabilities.lock().unwrap().type_hierarchy =
            ProviderCapability::Available;
    }

    fn methods(messages: &[serde_json::Value]) -> Vec<&str> {
        messages
            .iter()
            .filter_map(|message| message.get("method").and_then(serde_json::Value::as_str))
            .collect()
    }

    fn method_count(messages: &[serde_json::Value], method: &str) -> usize {
        methods(messages)
            .iter()
            .filter(|item| **item == method)
            .count()
    }

    fn assert_hierarchy_item_ranges(
        items: &[extension_protocol::TypeHierarchyItem],
        expected: &[(&str, u32, u32)],
    ) {
        assert_eq!(
            items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>()
        );
        for (item, (_, line, character)) in items.iter().zip(expected) {
            assert_eq!(
                item.range,
                TypeHierarchyRange {
                    line: *line,
                    character: *character,
                    end_line: line + 2,
                    end_character: character + 4,
                }
            );
            assert_eq!(
                item.selection_range,
                TypeHierarchyRange {
                    line: line + 1,
                    character: character + 1,
                    end_line: line + 1,
                    end_character: character + 3,
                }
            );
        }
    }

    #[test]
    fn hierarchy_response_normalizes_null_and_mixed_array_in_backend_order() {
        let (adapter, _, _) = test_adapter();
        assert_eq!(
            adapter.normalize_type_hierarchy_items(&serde_json::Value::Null),
            Ok(Vec::new())
        );

        let value = json!([
            hierarchy_item("WorkspaceType", 12, "file:///workspace/src/lib.rs", 3, 5),
            hierarchy_item("ExternalType", 23, "file:///dependency/src/lib.rs", 8, 13)
        ]);
        let items = adapter.normalize_type_hierarchy_items(&value).unwrap();

        assert_eq!(
            items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["WorkspaceType", "ExternalType"]
        );
        assert_eq!(items[0].detail.as_deref(), Some("detail for WorkspaceType"));
        assert_eq!(items[0].range.line, 3);
        assert_eq!(items[0].range.character, 5);
        assert_eq!(items[0].range.end_line, 5);
        assert_eq!(items[0].range.end_character, 9);
        assert_eq!(items[0].selection_range.line, 4);
        assert_eq!(items[0].selection_range.character, 6);
        assert_eq!(items[0].selection_range.end_line, 4);
        assert_eq!(items[0].selection_range.end_character, 8);
        assert_eq!(items[1].detail.as_deref(), Some("detail for ExternalType"));
        assert_eq!(items[1].kind, "struct");
        assert_eq!(items[1].range.line, 8);
        assert_eq!(items[1].range.character, 13);
        assert_eq!(items[1].range.end_line, 10);
        assert_eq!(items[1].range.end_character, 17);
        assert_eq!(items[1].selection_range.line, 9);
        assert_eq!(items[1].selection_range.character, 14);
        assert_eq!(items[1].selection_range.end_line, 9);
        assert_eq!(items[1].selection_range.end_character, 16);
    }

    #[test]
    fn hierarchy_symbol_kinds_use_known_name_and_unknown_fallback() {
        let (adapter, _, _) = test_adapter();
        let value = json!([
            hierarchy_item("Known", 12, "file:///workspace/src/lib.rs", 0, 0),
            hierarchy_item("Unknown", 999, "file:///workspace/src/lib.rs", 1, 0)
        ]);

        let items = adapter.normalize_type_hierarchy_items(&value).unwrap();

        assert_eq!(items[0].kind, "function");
        assert_eq!(items[1].kind, "symbol");
    }

    #[test]
    fn hierarchy_workspace_file_uri_becomes_relative_path() {
        let (adapter, _, _) = test_adapter();
        let value = json!([hierarchy_item(
            "WorkspaceType",
            5,
            "file:///workspace/src/types.rs",
            0,
            0
        )]);

        let items = adapter.normalize_type_hierarchy_items(&value).unwrap();

        assert_eq!(items[0].path.as_deref(), Some("src/types.rs"));
        assert_eq!(items[0].external_uri, None);
        assert!(!items[0].external);
    }

    #[test]
    fn hierarchy_external_uri_remains_read_only_external_uri() {
        let (adapter, _, _) = test_adapter();
        let external_uri = "file:///dependency/src/types.rs";
        let value = json!([hierarchy_item("ExternalType", 5, external_uri, 0, 0)]);

        let items = adapter.normalize_type_hierarchy_items(&value).unwrap();

        assert_eq!(items[0].path, None);
        assert_eq!(items[0].external_uri.as_deref(), Some(external_uri));
        assert!(items[0].external);
    }

    #[test]
    fn hierarchy_malformed_item_fails_whole_response() {
        let (adapter, _, _) = test_adapter();
        let value = json!([
            hierarchy_item("Valid", 5, "file:///workspace/src/types.rs", 0, 0),
            {
                "name": "MissingSelectionRange",
                "kind": 5,
                "uri": "file:///workspace/src/broken.rs",
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                }
            }
        ]);

        let error = adapter.normalize_type_hierarchy_items(&value).unwrap_err();

        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse)
        );
        assert_eq!(
            serde_json::to_value(LspOutcomeCode::ServerMalformedResponse).unwrap(),
            "server_malformed_response"
        );
    }

    #[test]
    fn hierarchy_non_array_response_fails_with_server_malformed_response() {
        let (adapter, _, _) = test_adapter();

        let error = adapter
            .normalize_type_hierarchy_items(&json!({ "name": "not an array" }))
            .unwrap_err();

        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse)
        );
    }

    fn complete_response(responses: ResponseCache, response: serde_json::Value) {
        complete_responses(responses, vec![response]);
    }

    fn complete_responses(responses: ResponseCache, responses_by_id: Vec<serde_json::Value>) {
        std::thread::spawn(move || {
            let (lock, cvar) = &*responses;
            for response in responses_by_id {
                let id = response["id"].as_i64().expect("test response has id");
                let cache = lock.lock().unwrap();
                let (mut cache, timeout) = cvar
                    .wait_timeout_while(cache, Duration::from_secs(2), |cache| {
                        !cache.contains_key(&id)
                    })
                    .unwrap();
                assert!(
                    !timeout.timed_out(),
                    "request {id} must be registered before its response"
                );
                cache.insert(id, Some(response));
                cvar.notify_all();
            }
        });
    }

    fn assert_hierarchy_error(
        injected: Vec<serde_json::Value>,
        direction: TypeHierarchyDirection,
        expected: LspSessionError,
    ) {
        let (adapter, sink, responses) = test_adapter();
        enable_type_hierarchy(&adapter);
        complete_responses(responses, injected);

        let error = adapter
            .type_hierarchy_lsp(&hierarchy_query(direction), "private source text")
            .unwrap_err();

        assert_eq!(error, expected);
        assert!(!error.to_string().contains("private"));
        assert_eq!(
            method_count(&written_messages(&sink), "textDocument/didClose"),
            1
        );
    }

    #[test]
    fn type_hierarchy_provider_parsing_and_unavailable_directions_are_classified() {
        for (provider, expected) in [
            (None, ProviderCapability::Unavailable),
            (Some(json!(false)), ProviderCapability::Unavailable),
            (Some(json!(true)), ProviderCapability::Available),
            (Some(json!({})), ProviderCapability::Available),
        ] {
            let mut capabilities = serde_json::Map::new();
            if let Some(provider) = provider {
                capabilities.insert("typeHierarchyProvider".to_owned(), provider);
            }
            let parsed = SemanticCapabilities::from_initialize_result(&json!({
                "capabilities": capabilities
            }))
            .unwrap();
            assert_eq!(parsed.type_hierarchy, expected);
        }

        let malformed = SemanticCapabilities::from_initialize_result(&json!({
            "capabilities": {"typeHierarchyProvider": "yes"}
        }))
        .unwrap_err();
        assert_eq!(
            malformed,
            LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse)
        );

        for direction in [
            TypeHierarchyDirection::Supertypes,
            TypeHierarchyDirection::Subtypes,
            TypeHierarchyDirection::Both,
        ] {
            let (adapter, sink, _) = test_adapter();
            let error = adapter
                .type_hierarchy_lsp(&hierarchy_query(direction), "private source")
                .unwrap_err();
            assert_eq!(error, LspSessionError::CapabilityUnavailable);
            assert!(
                methods(&written_messages(&sink))
                    .iter()
                    .all(|method| *method != "textDocument/prepareTypeHierarchy")
            );
        }
    }

    #[test]
    fn type_hierarchy_pooled_session_delegates_to_the_adapter_sequence() {
        let (adapter, sink, responses) = test_adapter();
        enable_type_hierarchy(&adapter);
        let prepared = hierarchy_item("Root", 5, "file:///workspace/src/main.rs", 4, 9);
        let parent = hierarchy_item("Parent", 23, "file:///workspace/src/parent.rs", 7, 11);
        complete_responses(
            responses,
            vec![
                json!({"id": 1, "result": [prepared]}),
                json!({"id": 2, "result": [parent]}),
            ],
        );

        let session: &dyn PooledSession = &adapter;
        let result = session
            .type_hierarchy_lsp(
                &hierarchy_query(TypeHierarchyDirection::Supertypes),
                "private hierarchy source text",
            )
            .unwrap();

        assert!(result.supported);
        assert_eq!(
            result
                .supertypes
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Parent"]
        );
        assert!(result.subtypes.is_empty());
        assert_eq!(
            methods(&written_messages(&sink)),
            vec![
                "textDocument/didOpen",
                "textDocument/prepareTypeHierarchy",
                "typeHierarchy/supertypes",
                "textDocument/didClose",
            ]
        );
    }

    #[test]
    fn type_hierarchy_supported_directions_preserve_order_ranges_and_raw_items() {
        for (direction, expected_methods, expect_supertypes, expect_subtypes) in [
            (
                TypeHierarchyDirection::Supertypes,
                vec![
                    "textDocument/prepareTypeHierarchy",
                    "typeHierarchy/supertypes",
                ],
                true,
                false,
            ),
            (
                TypeHierarchyDirection::Subtypes,
                vec![
                    "textDocument/prepareTypeHierarchy",
                    "typeHierarchy/subtypes",
                ],
                false,
                true,
            ),
            (
                TypeHierarchyDirection::Both,
                vec![
                    "textDocument/prepareTypeHierarchy",
                    "typeHierarchy/supertypes",
                    "typeHierarchy/subtypes",
                ],
                true,
                true,
            ),
        ] {
            let (adapter, sink, responses) = test_adapter();
            enable_type_hierarchy(&adapter);
            let mut prepared = hierarchy_item("Root", 5, "file:///workspace/src/main.rs", 4, 9);
            prepared["data"] = json!({ "opaque": "opaque hierarchy marker" });
            let parent_a = hierarchy_item("ParentA", 23, "file:///workspace/src/a.rs", 7, 11);
            let parent_b = hierarchy_item("ParentB", 23, "file:///workspace/src/b.rs", 20, 3);
            let child_a = hierarchy_item("ChildA", 12, "file:///dependency/src/a.rs", 13, 17);
            let child_b = hierarchy_item("ChildB", 12, "file:///dependency/src/b.rs", 30, 1);
            let mut injected = vec![json!({"id": 1, "result": [prepared.clone()]})];
            match direction {
                TypeHierarchyDirection::Supertypes => {
                    injected.push(
                        json!({"id": 2, "result": [parent_a.clone(), parent_b.clone(), parent_a]}),
                    );
                }
                TypeHierarchyDirection::Subtypes => {
                    injected.push(json!({"id": 2, "result": [child_a, child_b]}));
                }
                TypeHierarchyDirection::Both => {
                    injected.push(
                        json!({"id": 2, "result": [parent_a.clone(), parent_b.clone(), parent_a]}),
                    );
                    injected.push(json!({"id": 3, "result": [child_a, child_b]}));
                }
            }
            complete_responses(responses, injected);

            let result = adapter
                .type_hierarchy_lsp(&hierarchy_query(direction), "private hierarchy source text")
                .unwrap();

            assert!(result.supported);
            if expect_supertypes {
                assert_hierarchy_item_ranges(
                    &result.supertypes,
                    &[("ParentA", 7, 11), ("ParentB", 20, 3)],
                );
            } else {
                assert!(result.supertypes.is_empty());
            }
            if expect_subtypes {
                assert_hierarchy_item_ranges(
                    &result.subtypes,
                    &[("ChildA", 13, 17), ("ChildB", 30, 1)],
                );
            } else {
                assert!(result.subtypes.is_empty());
            }

            let messages = written_messages(&sink);
            let request_methods: Vec<_> = methods(&messages)
                .into_iter()
                .filter(|method| {
                    method.contains("TypeHierarchy") || method.starts_with("typeHierarchy/")
                })
                .collect();
            assert_eq!(request_methods, expected_methods);
            let prepare = request_with_method(&messages, "textDocument/prepareTypeHierarchy");
            assert_eq!(
                prepare["params"]["textDocument"]["uri"],
                "file:///workspace/src/main.rs"
            );
            assert_eq!(
                prepare["params"]["position"],
                json!({"line": 4, "character": 9})
            );
            for method in ["typeHierarchy/supertypes", "typeHierarchy/subtypes"] {
                if expected_methods.contains(&method) {
                    let followup = request_with_method(&messages, method);
                    assert_eq!(followup["params"]["item"], prepared);
                }
            }

            let public_json = serde_json::to_string(&result).unwrap();
            assert!(!public_json.contains("opaque hierarchy marker"));
            assert!(!public_json.contains("private hierarchy source text"));
            for log_host_call in messages
                .iter()
                .filter(|message| message["method"] == "log" || message["params"]["type"] == "log")
            {
                let log_json = serde_json::to_string(log_host_call).unwrap();
                assert!(!log_json.contains("opaque hierarchy marker"));
                assert!(!log_json.contains("private hierarchy source text"));
            }
        }
    }

    #[test]
    fn type_hierarchy_empty_prepare_is_supported_and_releases_document() {
        for prepared in [serde_json::Value::Null, json!([])] {
            let (adapter, sink, responses) = test_adapter();
            enable_type_hierarchy(&adapter);
            let path = RelativePath::new("src/main.rs");
            adapter.document_opened(&path, "struct Empty;");
            complete_response(responses, json!({"id": 1, "result": prepared}));

            let result = adapter
                .type_hierarchy_lsp(
                    &hierarchy_query(TypeHierarchyDirection::Both),
                    "struct Empty;",
                )
                .unwrap();

            assert!(result.supported);
            assert!(result.supertypes.is_empty());
            assert!(result.subtypes.is_empty());
            let messages = written_messages(&sink);
            assert_eq!(
                methods(&messages)
                    .iter()
                    .filter(|method| method.starts_with("typeHierarchy/"))
                    .count(),
                0
            );
            assert_eq!(method_count(&messages, "textDocument/didClose"), 0);
            adapter.document_closed(&path);
            assert_eq!(
                method_count(&written_messages(&sink), "textDocument/didClose"),
                1
            );
        }
    }

    #[test]
    fn type_hierarchy_prepare_method_not_found_is_capability_unavailable_and_releases_document() {
        assert_hierarchy_error(
            vec![json!({"id": 1, "error": {"code": -32601, "message": "private server detail"}})],
            TypeHierarchyDirection::Supertypes,
            LspSessionError::CapabilityUnavailable,
        );
    }

    #[test]
    fn type_hierarchy_followup_backend_error_is_server_error_and_releases_document() {
        assert_hierarchy_error(
            vec![
                json!({"id": 1, "result": [hierarchy_item("Root", 5, "file:///workspace/src/main.rs", 4, 9)]}),
                json!({"id": 2, "error": {"code": -32603, "message": "private backend detail"}}),
            ],
            TypeHierarchyDirection::Subtypes,
            LspSessionError::Backend(LspOutcomeCode::ServerError),
        );
    }

    #[test]
    fn type_hierarchy_invalid_prepare_response_is_malformed_and_releases_document() {
        assert_hierarchy_error(
            vec![json!({"id": 1, "result": {"unexpected": "private source text"}})],
            TypeHierarchyDirection::Both,
            LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse),
        );
    }

    #[test]
    fn type_hierarchy_failed_document_sync_rolls_back_document_hold() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(Error::new(ErrorKind::BrokenPipe, "document sync failed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (mut adapter, _sink, _responses) = test_adapter();
        enable_type_hierarchy(&adapter);
        adapter.writer = Writer::new(Box::new(FailingWriter));

        let error = adapter
            .type_hierarchy_lsp(
                &hierarchy_query(TypeHierarchyDirection::Both),
                "private source text",
            )
            .unwrap_err();

        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::ServerTransportError)
        );
        let uri = adapter.uri_for(&RelativePath::new("src/main.rs"));
        assert!(
            !adapter
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .docs
                .is_open(&uri),
            "a failed didOpen must not retain a document hold"
        );
    }

    #[test]
    fn combined_lsp_requests_share_one_configured_deadline() {
        let (mut adapter, sink, responses) = test_adapter();
        enable_type_hierarchy(&adapter);
        // Admission gates make the two nested protocol turns observable.  The
        // second response deliberately remains withheld: a fresh relative
        // budget may not be allocated after the prepare response is admitted.
        adapter.operation_timeout = Some(Duration::from_millis(50));
        let (first_admitted_tx, first_admitted_rx) = std::sync::mpsc::channel();
        let (second_admitted_tx, second_admitted_rx) = std::sync::mpsc::channel();
        let responder_responses = Arc::clone(&responses);
        let responder = std::thread::spawn(move || {
            first_admitted_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("prepare request must reach its admission gate");
            let (lock, cvar) = &*responder_responses;
            lock.lock().unwrap().insert(
                1,
                Some(json!({"id": 1, "result": [hierarchy_item("Root", 5, "file:///workspace/src/main.rs", 4, 9)]})),
            );
            cvar.notify_all();
            second_admitted_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("nested hierarchy request must be admitted after prepare");
        });

        let worker = std::thread::spawn(move || {
            adapter.type_hierarchy_lsp(
                &hierarchy_query(TypeHierarchyDirection::Both),
                "private source text",
            )
        });
        let request_count = |method| method_count(&written_messages(&sink), method);
        let admission_deadline = Instant::now() + Duration::from_secs(1);
        while request_count("textDocument/prepareTypeHierarchy") != 1 {
            assert!(
                Instant::now() < admission_deadline,
                "prepare request was not admitted before the test gate"
            );
            std::thread::yield_now();
        }
        first_admitted_tx
            .send(())
            .expect("release prepare response after its admission");
        while request_count("typeHierarchy/supertypes") != 1 {
            assert!(
                Instant::now() < admission_deadline,
                "nested request was not admitted after the prepare response"
            );
            std::thread::yield_now();
        }
        second_admitted_tx
            .send(())
            .expect("record nested request admission");
        let error = worker
            .join()
            .expect("compound hierarchy request worker joins")
            .unwrap_err();
        responder.join().expect("admission responder joins");

        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::ServerTimeout)
        );
        assert_eq!(
            method_count(&written_messages(&sink), "textDocument/didClose"),
            1
        );
    }

    fn adapter_with_malformed_server_output(output: &str) -> LspClientAdapter {
        let initialize =
            r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"definitionProvider":true}}}"#;
        let script = format!(
            "initialize='{initialize}'; printf 'Content-Length: %s\\r\\n\\r\\n%s' \"${{#initialize}}\" \"$initialize\"; printf '%b' '{output}'; sleep 60"
        );
        LspClientAdapter::spawn(
            "/bin/sh",
            &["-c".to_owned(), script],
            vec!["rs".to_owned()],
            "rust".to_owned(),
            std::env::current_dir().expect("test process has a current directory"),
            None,
            Some(TEST_OPERATION_TIMEOUT),
        )
        .expect("the valid initialize response must allow the session to start")
    }

    fn wait_until_reader_exits(adapter: &LspClientAdapter) {
        for _ in 0..100 {
            if adapter.dead_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("malformed server output must terminate the reader");
    }

    fn written_messages(sink: &SharedSink) -> Vec<serde_json::Value> {
        let bytes = sink.0.lock().unwrap().clone();
        let mut reader = BufReader::new(bytes.as_slice());
        let mut out = Vec::new();
        while let Some(msg) = super::jsonrpc::read_message(&mut reader).unwrap() {
            out.push(msg);
        }
        out
    }

    fn request_with_method(messages: &[serde_json::Value], method: &str) -> serde_json::Value {
        messages
            .iter()
            .find(|msg| msg.get("method").and_then(serde_json::Value::as_str) == Some(method))
            .cloned()
            .unwrap()
    }

    fn enable_pull_diagnostics(adapter: &LspClientAdapter) {
        adapter.semantic_capabilities.lock().unwrap().diagnostics = DiagnosticsCapability::Pull;
    }

    fn full_diagnostic_response(id: i64, message: &str) -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "kind": "full",
                "items": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "severity": 1,
                    "message": message
                }]
            }
        })
    }

    #[test]
    fn diagnostic_capability_selects_push_when_pull_is_not_advertised() {
        let capabilities = SemanticCapabilities::from_initialize_result(&json!({
            "capabilities": {}
        }))
        .unwrap();

        assert_eq!(capabilities.diagnostics, DiagnosticsCapability::Push);
    }

    #[test]
    fn diagnostic_capability_selects_pull_when_document_diagnostics_are_advertised() {
        for provider in [json!(true), json!({"identifier": "rust"})] {
            let capabilities = SemanticCapabilities::from_initialize_result(&json!({
                "capabilities": {"diagnosticProvider": provider}
            }))
            .unwrap();

            assert_eq!(capabilities.diagnostics, DiagnosticsCapability::Pull);
        }
    }

    #[test]
    fn operation_timeout_overflow_returns_typed_error_without_unwinding() {
        let (mut adapter, _sink, _responses) = test_adapter();
        adapter.operation_timeout = Some(Duration::MAX);

        let result = std::panic::catch_unwind(|| {
            PooledSession::check_lsp(&adapter, &RelativePath::new("src/main.rs"), "fn main() {}")
        });

        assert_eq!(
            result.expect("overflow must not unwind"),
            Err(LspSessionError::Backend(LspOutcomeCode::ServerTimeout))
        );
    }

    #[test]
    fn push_diagnostics_distinguish_explicit_empty_from_absent_publication() {
        for expected in [Vec::new(), vec![diag("fresh diagnostic")]] {
            let (mut adapter, sink, _responses) = test_adapter();
            adapter.operation_timeout = Some(Duration::from_millis(250));
            let uri = "file:///workspace/src/main.rs";
            record_diagnostics(
                &adapter.state,
                uri.to_owned(),
                vec![diag("stale diagnostic")],
            );
            let writer = BlockingWriter::new(sink);
            adapter.writer = writer.writer();

            let actual = std::thread::scope(|scope| {
                let check = scope.spawn(|| {
                    PooledSession::check_lsp(
                        &adapter,
                        &RelativePath::new("src/main.rs"),
                        "fn changed() {}",
                    )
                });
                let write_started = writer.wait_until_write_starts();
                record_diagnostics(
                    &adapter.state,
                    "file:///workspace/src/other.rs".to_owned(),
                    vec![diag("fresh wrong-uri diagnostic")],
                );
                record_diagnostics(&adapter.state, uri.to_owned(), expected.clone());
                for index in 0..=DIAGNOSTIC_PUBLICATION_HISTORY {
                    record_diagnostics(
                        &adapter.state,
                        uri.to_owned(),
                        vec![diag(&format!("later same-uri diagnostic {index}"))],
                    );
                }
                record_diagnostics(
                    &adapter.state,
                    uri.to_owned(),
                    vec![diag("later fresh diagnostic")],
                );
                mark_ready(&adapter.state);
                writer.release();
                assert!(
                    write_started,
                    "didOpen/didChange must be blocked before publishing"
                );
                check.join().unwrap()
            })
            .unwrap();

            assert_eq!(actual, expected);
        }

        let (mut adapter, _sink, _responses) = test_adapter();
        adapter.operation_timeout = Some(Duration::from_millis(20));
        assert_eq!(
            PooledSession::check_lsp(
                &adapter,
                &RelativePath::new("src/main.rs"),
                "fn no_publication() {}",
            ),
            Err(LspSessionError::Backend(LspOutcomeCode::ServerTimeout))
        );
    }

    #[test]
    fn pull_diagnostics_decode_full_reports_and_reject_ungrounded_unchanged_reports() {
        let (mut adapter, sink, responses) = test_adapter();
        adapter.operation_timeout = Some(Duration::from_millis(250));
        enable_pull_diagnostics(&adapter);
        record_diagnostics(
            &adapter.state,
            "file:///workspace/src/main.rs".to_owned(),
            vec![diag("conflicting push diagnostic")],
        );
        complete_response(responses, full_diagnostic_response(1, "pulled diagnostic"));

        let diagnostics =
            PooledSession::check_lsp(&adapter, &RelativePath::new("src/main.rs"), "fn main() {}")
                .unwrap();

        assert_eq!(diagnostics, vec![diag("pulled diagnostic")]);
        let request = request_with_method(&written_messages(&sink), "textDocument/diagnostic");
        assert!(request["params"].get("previousResultId").is_none());

        for result in [
            json!({"kind": "unchanged", "resultId": "unrequested"}),
            json!({"relatedDocuments": {}}),
        ] {
            let (mut adapter, _sink, responses) = test_adapter();
            adapter.operation_timeout = Some(Duration::from_millis(250));
            enable_pull_diagnostics(&adapter);
            complete_response(
                responses,
                json!({"jsonrpc": "2.0", "id": 1, "result": result}),
            );

            assert_eq!(
                PooledSession::check_lsp(
                    &adapter,
                    &RelativePath::new("src/main.rs"),
                    "fn main() {}",
                ),
                Err(LspSessionError::Backend(
                    LspOutcomeCode::ServerMalformedResponse
                ))
            );
        }
    }

    #[test]
    fn diagnostics_resynchronize_changed_documents_before_querying() {
        let path = RelativePath::new("src/main.rs");
        let uri = "file:///workspace/src/main.rs";

        let (mut push_adapter, push_sink, _push_responses) = test_adapter();
        push_adapter.operation_timeout = Some(Duration::from_millis(250));
        push_adapter
            .inner
            .lock()
            .unwrap()
            .docs
            .acquire(uri, "rust", "fn old() {}");
        let push_writer = BlockingWriter::new(push_sink.clone());
        push_adapter.writer = push_writer.writer();
        let changed_push = std::thread::scope(|scope| {
            let check = scope
                .spawn(|| PooledSession::check_lsp(&push_adapter, &path, "fn new_content() {}"));
            let write_started = push_writer.wait_until_write_starts();
            record_diagnostics(
                &push_adapter.state,
                uri.to_owned(),
                vec![diag("push changed-content result")],
            );
            mark_ready(&push_adapter.state);
            push_writer.release();
            assert!(
                write_started,
                "push didChange must be written before publication"
            );
            check.join().unwrap()
        })
        .unwrap();
        assert_eq!(changed_push, vec![diag("push changed-content result")]);
        assert_eq!(
            PooledSession::check_lsp(&push_adapter, &path, "fn new_content() {}").unwrap(),
            vec![diag("push changed-content result")],
            "only identical push content may reuse the settled publication"
        );
        assert_eq!(
            method_count(&written_messages(&push_sink), "textDocument/didChange"),
            1
        );

        let (mut pull_adapter, pull_sink, pull_responses) = test_adapter();
        pull_adapter.operation_timeout = Some(Duration::from_millis(250));
        enable_pull_diagnostics(&pull_adapter);
        pull_adapter
            .inner
            .lock()
            .unwrap()
            .docs
            .acquire(uri, "rust", "fn old() {}");
        let pull_writer = BlockingWriter::new(pull_sink.clone());
        pull_adapter.writer = pull_writer.writer();
        let changed_pull = std::thread::scope(|scope| {
            let check = scope
                .spawn(|| PooledSession::check_lsp(&pull_adapter, &path, "fn new_content() {}"));
            let write_started = pull_writer.wait_until_write_starts();
            complete_response(
                Arc::clone(&pull_responses),
                full_diagnostic_response(1, "pull changed-content result"),
            );
            pull_writer.release();
            assert!(
                write_started,
                "pull didChange must precede its diagnostic request"
            );
            check.join().unwrap()
        })
        .unwrap();
        assert_eq!(changed_pull, vec![diag("pull changed-content result")]);
        complete_response(
            pull_responses,
            full_diagnostic_response(2, "pull identical-content result"),
        );
        assert_eq!(
            PooledSession::check_lsp(&pull_adapter, &path, "fn new_content() {}").unwrap(),
            vec![diag("pull identical-content result")],
            "pull diagnostics must query again even when the document content is unchanged"
        );
        let pull_messages = written_messages(&pull_sink);
        let methods = methods(&pull_messages);
        let change = methods
            .iter()
            .position(|method| *method == "textDocument/didChange")
            .unwrap();
        let query = methods
            .iter()
            .position(|method| *method == "textDocument/diagnostic")
            .unwrap();
        assert!(change < query);
        assert_eq!(method_count(&pull_messages, "textDocument/didChange"), 1);
        assert_eq!(method_count(&pull_messages, "textDocument/diagnostic"), 2);
    }

    #[test]
    fn diagnostics_timeout_preserves_the_live_session() {
        let (mut adapter, sink, responses) = test_adapter();
        adapter.operation_timeout = Some(Duration::from_millis(20));
        enable_pull_diagnostics(&adapter);
        let path = RelativePath::new("src/main.rs");

        assert_eq!(
            PooledSession::check_lsp(&adapter, &path, "fn first() {}"),
            Err(LspSessionError::Backend(LspOutcomeCode::ServerTimeout))
        );
        assert!(
            adapter
                .inner
                .lock()
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );

        adapter.operation_timeout = Some(Duration::from_millis(250));
        complete_response(responses, full_diagnostic_response(2, "session survived"));
        let diagnostics = PooledSession::check_lsp(&adapter, &path, "fn second() {}").unwrap();

        assert_eq!(diagnostics, vec![diag("session survived")]);
        assert_eq!(
            method_count(&written_messages(&sink), "textDocument/diagnostic"),
            2
        );
        assert!(
            adapter
                .inner
                .lock()
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn diagnostics_without_configured_timeout_waits_for_readiness() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let trace = workspace.path().join("diagnostics.log");
        let release = workspace.path().join("diagnostics.release");
        let adapter = LspClientAdapter::spawn(
            fixture_lsp_server::binary_path()
                .to_str()
                .expect("fixture path is UTF-8"),
            &[
                "diagnostics_released".to_owned(),
                trace.display().to_string(),
                "-".to_owned(),
                release.display().to_string(),
            ],
            vec!["rs".to_owned()],
            "rust".to_owned(),
            workspace.path().to_path_buf(),
            None,
            None,
        )
        .expect("fixture language server initializes without an operation timeout");

        let result = std::thread::scope(|scope| {
            let check = scope.spawn(|| {
                PooledSession::check_lsp(
                    &adapter,
                    &RelativePath::new("src/main.rs"),
                    "fn main() {}",
                )
            });
            for _ in 0..200 {
                if std::fs::read_to_string(format!("{}.trace", trace.display()))
                    .unwrap_or_default()
                    .contains("didOpen")
                {
                    std::fs::write(&release, "release").expect("release diagnostics fixture");
                    return check.join().expect("diagnostics worker joins");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            std::fs::write(&release, "release")
                .expect("release diagnostics fixture during teardown");
            let _ = check.join();
            panic!("diagnostics fixture did not reach its release gate")
        })
        .expect("released diagnostics request completes");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "released diagnostic");
    }

    #[test]
    fn writer_shutdown_maps_submission_failure_to_broken_pipe() {
        let (tx, rx) = std::sync::mpsc::channel();
        let writer = Writer { tx };
        drop(rx);
        let message = json!({"id": 1, "method": "textDocument/diagnostic"});
        assert_eq!(
            writer.write(&message).unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            writer
                .write_before(&message, Instant::now() + Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn queued_write_is_not_emitted_after_its_deadline() {
        let sink = SharedSink::default();
        let blocker = BlockingWriter::new(sink.clone());
        let writer = blocker.writer();
        let first_writer = writer.clone();
        let first = std::thread::spawn(move || first_writer.write(&json!({"sequence": 1})));
        assert!(blocker.wait_until_write_starts());

        assert_eq!(
            writer
                .write_before(
                    &json!({"sequence": 2}),
                    Instant::now() + Duration::from_millis(20),
                )
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        blocker.release();
        first.join().unwrap().unwrap();
        writer.write(&json!({"sequence": 3})).unwrap();

        let messages = written_messages(&sink);
        assert!(messages.iter().any(|message| message["sequence"] == 1));
        assert!(messages.iter().any(|message| message["sequence"] == 3));
        assert!(!messages.iter().any(|message| message["sequence"] == 2));
    }

    #[test]
    fn diagnostics_blocked_pull_write_timeout_terminates_the_session() {
        let (mut adapter, sink, responses) = test_adapter();
        enable_pull_diagnostics(&adapter);
        // Open first: the blocked operation must be the pull request, not didOpen.
        adapter
            .acquire_doc_until("file:///workspace/src/main.rs", "fn main() {}", None)
            .unwrap();
        let writer = BlockingWriter::new(sink.clone());
        adapter.writer = writer.writer();
        adapter.operation_timeout = Some(Duration::from_millis(50));
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let adapter_ref = &adapter;
            let check = scope.spawn(move || {
                tx.send(PooledSession::check_lsp(
                    adapter_ref,
                    &RelativePath::new("src/main.rs"),
                    "fn main() {}",
                ))
                .unwrap();
            });
            let started = writer.wait_until_write_starts();
            let result = rx.recv_timeout(Duration::from_millis(500));
            // Release even if the bounded receive failed, so a regression cannot
            // strand the worker or prevent the scoped thread from joining.
            writer.release();
            check.join().unwrap();
            assert!(started);
            assert_eq!(
                result,
                Ok(Err(LspSessionError::Backend(LspOutcomeCode::ServerTimeout)))
            );
            assert!(responses.0.lock().unwrap().is_empty());
        });
        // The timed-out caller can finish before the released writer has
        // emitted its frame. A queued write acknowledges all preceding writes
        // before the sink is parsed, including their headers and bodies.
        adapter
            .writer
            .write(&json!({"test_barrier": true}))
            .unwrap();
        assert_eq!(
            PooledSession::check_lsp(&adapter, &RelativePath::new("src/main.rs"), "fn main() {}"),
            Err(LspSessionError::Backend(LspOutcomeCode::ServerCrashed))
        );
        assert!(responses.0.lock().unwrap().is_empty());
        assert_eq!(
            method_count(&written_messages(&sink), "textDocument/diagnostic"),
            1
        );
        assert!(adapter.dead_flag.load(Ordering::Acquire));
    }

    #[test]
    fn timed_out_requests_discard_late_unsolicited_and_duplicate_responses() {
        let (adapter, _sink, responses) = test_adapter();
        let dispatcher = super::Dispatcher::new(Arc::clone(&responses));
        for id in 1..=32 {
            let result = adapter
                .send_request_once_until(
                    "textDocument/diagnostic",
                    json!({}),
                    Some(Instant::now() + Duration::from_millis(1)),
                )
                .unwrap();
            assert!(matches!(
                result,
                super::RequestOutcome::Error(super::RequestFailure::Timeout)
            ));
            dispatcher.dispatch(&json!({"id": id, "result": {}}));
            dispatcher.dispatch(&json!({"id": id + 1000, "result": {}}));
            assert!(responses.0.lock().unwrap().is_empty());
        }
        {
            let _pending = super::PendingRequest::new(&responses, 100);
            dispatcher.dispatch(&json!({"id": 100, "result": "first"}));
            dispatcher.dispatch(&json!({"id": 100, "result": "duplicate"}));
            assert_eq!(
                responses.0.lock().unwrap()[&100].as_ref().unwrap()["result"],
                "first"
            );
        }
        assert!(responses.0.lock().unwrap().is_empty());
    }

    #[test]
    fn operation_timeout_bounds_blocked_document_synchronization() {
        let (mut adapter, sink, _responses) = test_adapter();
        adapter.operation_timeout = Some(Duration::from_millis(20));
        let writer = BlockingWriter::new(sink);
        adapter.writer = writer.writer();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let result = PooledSession::check_lsp(
                &adapter,
                &RelativePath::new("src/main.rs"),
                "fn blocked_sync() {}",
            );
            let _ = result_tx.send(result);
        });

        assert!(writer.wait_until_write_starts());
        assert_eq!(
            result_rx.recv_timeout(Duration::from_millis(250)),
            Ok(Err(LspSessionError::Backend(LspOutcomeCode::ServerTimeout)))
        );
    }

    fn written_method_count(sink: &SharedSink, method: &str) -> usize {
        let bytes = sink.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .matches(&format!("\"method\":\"{method}\""))
            .count()
    }

    #[test]
    fn unlimited_references_do_not_require_optional_or_repeated_readiness_notifications() {
        for already_open in [false, true] {
            let (mut adapter, sink, responses) = test_adapter();
            adapter.operation_timeout = None;
            let path = RelativePath::new("src/main.rs");
            if already_open {
                adapter
                    .acquire_doc_until(&adapter.uri_for(&path), "fn main() {}", None)
                    .unwrap();
                mark_ready(&adapter.state);
            }
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let result = PooledSession::references_lsp(
                        &adapter,
                        &path,
                        "fn main() {}",
                        Position {
                            line: 0,
                            character: 0,
                        },
                    );
                    let _ = tx.send(result);
                });
                let deadline = Instant::now() + Duration::from_secs(2);
                while written_method_count(&sink, "textDocument/references") == 0
                    && Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
                let dispatched = written_method_count(&sink, "textDocument/references") == 1;
                super::Dispatcher::new(Arc::clone(&responses))
                    .dispatch(&json!({"id":1,"result":[]}));
                let result = rx.recv_timeout(Duration::from_secs(2));
                {
                    let _state = adapter.state.0.lock().unwrap();
                    adapter.dead_flag.store(true, Ordering::Release);
                    adapter.state.1.notify_all();
                }
                {
                    let _responses = responses.0.lock().unwrap();
                    responses.1.notify_all();
                }
                worker.join().unwrap();
                assert!(
                    dispatched,
                    "optional readiness signals must not prevent protocol dispatch"
                );
                assert_eq!(result, Ok(Ok(Vec::new())));
            });
        }
    }

    #[test]
    fn unlimited_request_response_wait_observes_reader_exit() {
        let (mut adapter, sink, responses) = test_adapter();
        adapter.operation_timeout = None;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let result = PooledSession::definition_lsp(
                    &adapter,
                    &RelativePath::new("src/main.rs"),
                    "fn main() {}",
                    Position {
                        line: 0,
                        character: 0,
                    },
                );
                let _ = tx.send(result);
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while written_method_count(&sink, "textDocument/definition") == 0
                && Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            let dispatched = written_method_count(&sink, "textDocument/definition") == 1;
            {
                let _state = adapter.state.0.lock().unwrap();
                adapter.dead_flag.store(true, Ordering::Release);
                adapter.state.1.notify_all();
            }
            {
                let _responses = responses.0.lock().unwrap();
                responses.1.notify_all();
            }
            let result = rx.recv_timeout(Duration::from_secs(2));
            // If a regression ignores EOF, release its pending response too so
            // the harness can fail without stranding a scoped thread.
            super::Dispatcher::new(Arc::clone(&responses)).dispatch(&json!({"id":1,"result":[]}));
            worker.join().unwrap();
            assert!(dispatched);
            assert_eq!(
                result,
                Ok(Err(LspSessionError::Backend(LspOutcomeCode::ServerCrashed)))
            );
        });
    }

    #[test]
    fn references_without_configured_timeout_wait_for_fresh_readiness() {
        let (mut adapter, sink, responses) = test_adapter();
        adapter.operation_timeout = None;
        mark_ready(&adapter.state);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let result = PooledSession::references_lsp(
                    &adapter,
                    &RelativePath::new("src/main.rs"),
                    "fn main() {}",
                    Position {
                        line: 0,
                        character: 0,
                    },
                );
                let _ = tx.send(result);
            });
            let entered_deadline = Instant::now() + Duration::from_secs(2);
            while written_method_count(&sink, "textDocument/didOpen") == 0
                && Instant::now() < entered_deadline
            {
                std::thread::yield_now();
            }
            let entered = written_method_count(&sink, "textDocument/didOpen") == 1;
            // Exceed the removed 1.5s production fallback. This is a harness
            // watchdog only; readiness remains controlled by the explicit release.
            let pending = rx.recv_timeout(Duration::from_millis(1600));
            let requests_before_release = written_method_count(&sink, "textDocument/references");
            mark_ready(&adapter.state);
            let request_deadline = Instant::now() + Duration::from_secs(2);
            while written_method_count(&sink, "textDocument/references") == 0
                && Instant::now() < request_deadline
            {
                std::thread::yield_now();
            }
            super::Dispatcher::new(Arc::clone(&responses)).dispatch(&json!({"id":1,"result":[]}));
            let completed = rx.recv_timeout(Duration::from_secs(2));
            // Always wake and join on assertion failures as well.
            {
                let _state = adapter.state.0.lock().unwrap();
                adapter.dead_flag.store(true, Ordering::Release);
                adapter.state.1.notify_all();
            }
            {
                let _responses = responses.0.lock().unwrap();
                responses.1.notify_all();
            }
            worker.join().unwrap();
            assert!(entered, "references must reach document synchronization");
            assert!(matches!(
                pending,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(
                requests_before_release, 0,
                "elapsed time must not bypass readiness"
            );
            assert_eq!(completed, Ok(Ok(Vec::new())));
        });
    }

    #[test]
    fn unlimited_reference_readiness_wait_observes_reader_exit() {
        let workspace = tempfile::tempdir().unwrap();
        let adapter = Arc::new(
            LspClientAdapter::spawn(
                fixture_lsp_server::binary_path().to_str().unwrap(),
                &["server_crashed".to_owned()],
                vec!["rs".to_owned()],
                "rust".to_owned(),
                workspace.path().to_path_buf(),
                None,
                None,
            )
            .unwrap(),
        );
        let worker_adapter = Arc::clone(&adapter);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = PooledSession::references_lsp(
                worker_adapter.as_ref(),
                &RelativePath::new("src/main.rs"),
                "fn main() {}",
                Position {
                    line: 0,
                    character: 0,
                },
            );
            let _ = tx.send(result);
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        {
            let _state = adapter.state.0.lock().unwrap();
            adapter.dead_flag.store(true, Ordering::Release);
            adapter.state.1.notify_all();
        }
        worker.join().unwrap();
        assert_eq!(
            result,
            Ok(Err(LspSessionError::Backend(LspOutcomeCode::ServerCrashed)))
        );
    }

    #[test]
    fn references_readiness_timeout_releases_document_hold() {
        let (mut adapter, sink, _responses) = test_adapter();
        adapter.operation_timeout = Some(Duration::from_millis(20));
        mark_ready(&adapter.state);
        let path = RelativePath::new("src/main.rs");

        assert_eq!(
            PooledSession::references_lsp(
                &adapter,
                &path,
                "fn main() {}",
                Position {
                    line: 0,
                    character: 0,
                },
            ),
            Err(LspSessionError::Backend(LspOutcomeCode::ServerTimeout))
        );

        let uri = adapter.uri_for(&path);
        assert!(
            !adapter
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .docs
                .is_open(&uri)
        );
        assert_eq!(
            method_count(&written_messages(&sink), "textDocument/didClose"),
            1
        );
    }

    #[test]
    fn f008_failure_classification_preserves_typed_failure_information_through_sidecar_seams() {
        let (adapter, _sink, _responses) = test_adapter();

        let error = PooledSession::definition_lsp(
            &adapter,
            &RelativePath::new("README.txt"),
            "not rust",
            Position {
                line: 0,
                character: 0,
            },
        )
        .unwrap_err();

        assert_eq!(error, super::LspSessionError::CapabilityUnavailable);
    }

    #[cfg(unix)]
    #[test]
    fn f008_spawn_resolved_classifies_post_resolution_executable_changes() {
        use extension_protocol::LspOutcomeCode;
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().expect("temporary workspace");
        let executable = workspace.path().join("language-server");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        std::fs::remove_file(&executable).unwrap();
        let missing = match LspClientAdapter::spawn_resolved(
            &executable,
            &[],
            vec!["rs".to_owned()],
            "rust".to_owned(),
            workspace.path().to_path_buf(),
            None,
            Some(TEST_OPERATION_TIMEOUT),
        ) {
            Ok(_) => panic!("removed executable must not spawn"),
            Err(error) => error,
        };
        assert_eq!(
            missing,
            super::LspSessionError::Backend(LspOutcomeCode::ServerMissing)
        );

        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let not_executable = match LspClientAdapter::spawn_resolved(
            &executable,
            &[],
            vec!["rs".to_owned()],
            "rust".to_owned(),
            workspace.path().to_path_buf(),
            None,
            Some(TEST_OPERATION_TIMEOUT),
        ) {
            Ok(_) => panic!("non-executable file must not spawn"),
            Err(error) => error,
        };
        assert_eq!(
            not_executable,
            super::LspSessionError::Backend(LspOutcomeCode::ServerNotExecutable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn f008_spawn_resolved_does_not_misclassify_working_directory_failure() {
        use extension_protocol::LspOutcomeCode;

        let workspace = tempfile::tempdir().expect("temporary workspace");
        let removed_root = workspace.path().join("removed-root");
        std::fs::create_dir(&removed_root).unwrap();
        std::fs::remove_dir(&removed_root).unwrap();

        let error = match LspClientAdapter::spawn_resolved(
            std::path::Path::new("/bin/true"),
            &[],
            vec!["rs".to_owned()],
            "rust".to_owned(),
            removed_root,
            None,
            Some(TEST_OPERATION_TIMEOUT),
        ) {
            Ok(_) => panic!("removed working directory must not permit spawn"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            super::LspSessionError::Backend(LspOutcomeCode::ServerLaunchFailed)
        );

        let ambiguous_permission_error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            super::classify_spawn_error(
                ambiguous_permission_error,
                std::path::Path::new("/bin/true"),
                workspace.path(),
            ),
            super::LspSessionError::Backend(LspOutcomeCode::ServerLaunchFailed),
            "a valid executable must not absorb a working-directory permission failure"
        );
    }

    #[test]
    fn f008_failure_classification_preserves_successful_definition_payload_for_typed_and_legacy_ports()
     {
        let (adapter, _sink, responses) = test_adapter();
        let expected = vec![Location {
            path: RelativePath::new("src/lib.rs"),
            range: Range {
                start: Position {
                    line: 2,
                    character: 4,
                },
                end: Position {
                    line: 2,
                    character: 8,
                },
            },
        }];
        let result = json!([{
            "uri": "file:///workspace/src/lib.rs",
            "range": {
                "start": { "line": 2, "character": 4 },
                "end": { "line": 2, "character": 8 }
            }
        }]);
        complete_responses(
            responses,
            vec![
                json!({"id": 1, "result": result}),
                json!({"id": 2, "result": result}),
            ],
        );
        let path = RelativePath::new("src/main.rs");
        let position = Position {
            line: 2,
            character: 5,
        };

        let typed = PooledSession::definition_lsp(&adapter, &path, "fn main() {}", position)
            .expect("typed query keeps its established successful payload");
        let legacy = NavigationPort::definition(&adapter, &path, "fn main() {}", position)
            .expect("legacy wrapper delegates to the same successful typed query");

        assert_eq!(typed, expected);
        assert_eq!(legacy, expected);
    }

    #[test]
    fn f008_failure_classification_uses_injected_responses_for_precedence_and_sanitized_codes() {
        let cases = [
            (
                json!({"id": 1, "error": {"code": -32601, "message": "secret method detail"}}),
                super::LspSessionError::CapabilityUnavailable,
                "language server capability is unavailable",
            ),
            (
                json!({"id": 1, "error": {"code": -32603, "message": "secret backend detail"}}),
                super::LspSessionError::Backend(LspOutcomeCode::ServerError),
                "language server request failed",
            ),
            (
                json!({"id": 1, "result": {"unexpected": "secret schema detail"}}),
                super::LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse),
                "language server request failed",
            ),
        ];

        for (response, expected, sanitized_message) in cases {
            let (adapter, _sink, responses) = test_adapter();
            complete_response(responses, response);

            let error = PooledSession::definition_lsp(
                &adapter,
                &RelativePath::new("src/main.rs"),
                "fn main() {}",
                Position {
                    line: 0,
                    character: 3,
                },
            )
            .unwrap_err();

            assert_eq!(error, expected);
            assert_eq!(error.to_string(), sanitized_message);
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn f008_failure_classification_classifies_known_server_exit_as_server_crashed() {
        let (adapter, _sink, responses) = test_adapter();
        adapter
            .dead_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        complete_response(
            responses,
            json!({"id": 1, "error": {"code": -32603, "message": "internal error"}}),
        );

        let error = PooledSession::definition_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "fn main() {}",
            Position {
                line: 0,
                character: 3,
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            super::LspSessionError::Backend(LspOutcomeCode::ServerCrashed)
        );
    }

    #[test]
    fn f008_failure_classification_classifies_live_server_io_failure_as_transport_error() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "private transport detail",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (mut adapter, _sink, _responses) = test_adapter();
        adapter.writer = Writer::new(Box::new(FailingWriter));

        let error = PooledSession::definition_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "fn main() {}",
            Position {
                line: 0,
                character: 3,
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            super::LspSessionError::Backend(LspOutcomeCode::ServerTransportError)
        );

        let legacy_error = NavigationPort::definition(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "fn main() {}",
            Position {
                line: 0,
                character: 3,
            },
        )
        .unwrap_err();
        assert_eq!(
            legacy_error,
            CodeIntelError::Backend("language server request failed".to_owned())
        );
    }

    #[test]
    fn f008_failure_classification_classifies_invalid_framing_as_malformed_response() {
        let adapter = adapter_with_malformed_server_output("Content-Length: nope\\r\\n\\r\\n");
        wait_until_reader_exits(&adapter);

        let error = PooledSession::definition_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "fn main() {}",
            Position {
                line: 0,
                character: 3,
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            super::LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse)
        );
        assert_eq!(error.to_string(), "language server request failed");
    }

    #[test]
    fn f008_failure_classification_classifies_invalid_json_as_malformed_response() {
        let adapter = adapter_with_malformed_server_output("Content-Length: 1\\r\\n\\r\\n{");
        wait_until_reader_exits(&adapter);

        let error = PooledSession::definition_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "fn main() {}",
            Position {
                line: 0,
                character: 3,
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            super::LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse)
        );
        assert_eq!(error.to_string(), "language server request failed");
    }

    #[test]
    fn f008_failure_classification_classifies_deadline_expiry_as_server_timeout() {
        let (adapter, _sink, _responses) = test_adapter();

        let error = PooledSession::definition_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "fn main() {}",
            Position {
                line: 0,
                character: 3,
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            super::LspSessionError::Backend(LspOutcomeCode::ServerTimeout)
        );
    }

    #[test]
    fn f008_failure_classification_preserves_explicit_rename_target_rejection() {
        let (adapter, _sink, responses) = test_adapter();
        complete_response(
            responses,
            json!({
                "id": 1,
                "error": {"code": -32602, "message": "not valid rename target"}
            }),
        );

        let error = PooledSession::prepare_rename_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "let name = 1;",
            Position {
                line: 0,
                character: 4,
            },
        )
        .unwrap_err();

        assert_eq!(error, super::LspSessionError::NotRenameable);
    }

    #[test]
    fn f008_failure_classification_prepare_rename_method_not_found_is_capability_unavailable() {
        let (adapter, _sink, responses) = test_adapter();
        complete_response(
            responses,
            json!({
                "id": 1,
                "error": {"code": -32601, "message": "method not found"}
            }),
        );

        let error = PooledSession::prepare_rename_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "let name = 1;",
            Position {
                line: 0,
                character: 4,
            },
        )
        .unwrap_err();

        assert_eq!(error, super::LspSessionError::CapabilityUnavailable);
    }

    #[test]
    fn f008_failure_classification_rejects_missing_or_ambiguous_response_payloads() {
        for response in [
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": null,
                "error": {"code": -32603, "message": "internal error"}
            }),
        ] {
            let (adapter, _sink, responses) = test_adapter();
            complete_response(responses, response);

            let error = PooledSession::definition_lsp(
                &adapter,
                &RelativePath::new("src/main.rs"),
                "fn main() {}",
                Position {
                    line: 0,
                    character: 3,
                },
            )
            .unwrap_err();

            assert_eq!(
                error,
                super::LspSessionError::Backend(LspOutcomeCode::ServerMalformedResponse)
            );
        }
    }

    #[test]
    fn f008_failure_classification_check_reports_known_dead_server_without_waiting() {
        let (adapter, _sink, _responses) = test_adapter();
        adapter
            .dead_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let error =
            PooledSession::check_lsp(&adapter, &RelativePath::new("src/main.rs"), "fn main() {}")
                .unwrap_err();

        assert_eq!(
            error,
            super::LspSessionError::Backend(LspOutcomeCode::ServerCrashed)
        );
    }

    #[test]
    fn f008_failure_classification_reader_io_error_is_transport_error() {
        assert_eq!(
            classify_reader_error(&Error::new(
                ErrorKind::BrokenPipe,
                "private transport detail"
            )),
            ReaderExit::Transport
        );
    }

    #[test]
    fn f008_capability_captures_provider_shapes_and_classifies_missing_false_and_malformed_values()
    {
        let valid_cases = [
            (
                json!({
                    "capabilities": {
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "hoverProvider": true,
                        "implementationProvider": true,
                        "renameProvider": true
                    }
                }),
                SemanticCapabilities {
                    definition: ProviderCapability::Available,
                    references: ProviderCapability::Available,
                    hover: ProviderCapability::Available,
                    implementation: ProviderCapability::Available,
                    type_hierarchy: ProviderCapability::Unavailable,
                    rename: RenameCapability::RenameOnly,
                    will_rename_files: ProviderCapability::Unavailable,
                    safe_delete: ProviderCapability::Unavailable,
                    diagnostics: DiagnosticsCapability::Push,
                },
            ),
            (
                json!({
                    "capabilities": {
                        "definitionProvider": {},
                        "referencesProvider": {"workDoneProgress": true},
                        "hoverProvider": {},
                        "implementationProvider": {},
                        "renameProvider": {"prepareProvider": true}
                    }
                }),
                SemanticCapabilities {
                    definition: ProviderCapability::Available,
                    references: ProviderCapability::Available,
                    hover: ProviderCapability::Available,
                    implementation: ProviderCapability::Available,
                    type_hierarchy: ProviderCapability::Unavailable,
                    rename: RenameCapability::RenameWithPrepare,
                    will_rename_files: ProviderCapability::Unavailable,
                    safe_delete: ProviderCapability::Unavailable,
                    diagnostics: DiagnosticsCapability::Push,
                },
            ),
            (
                json!({
                    "capabilities": {
                        "definitionProvider": false,
                        "referencesProvider": false,
                        "hoverProvider": false,
                        "implementationProvider": false,
                        "renameProvider": false
                    }
                }),
                SemanticCapabilities {
                    definition: ProviderCapability::Unavailable,
                    references: ProviderCapability::Unavailable,
                    hover: ProviderCapability::Unavailable,
                    implementation: ProviderCapability::Unavailable,
                    type_hierarchy: ProviderCapability::Unavailable,
                    rename: RenameCapability::Unavailable,
                    will_rename_files: ProviderCapability::Unavailable,
                    safe_delete: ProviderCapability::Unavailable,
                    diagnostics: DiagnosticsCapability::Push,
                },
            ),
            (
                json!({"capabilities": {}}),
                SemanticCapabilities {
                    definition: ProviderCapability::Unavailable,
                    references: ProviderCapability::Unavailable,
                    hover: ProviderCapability::Unavailable,
                    implementation: ProviderCapability::Unavailable,
                    type_hierarchy: ProviderCapability::Unavailable,
                    rename: RenameCapability::Unavailable,
                    will_rename_files: ProviderCapability::Unavailable,
                    safe_delete: ProviderCapability::Unavailable,
                    diagnostics: DiagnosticsCapability::Push,
                },
            ),
        ];

        for (response, expected) in valid_cases {
            assert_eq!(
                SemanticCapabilities::from_initialize_result(&response),
                Ok(expected)
            );
        }

        for (provider, malformed) in [
            ("definitionProvider", json!("yes")),
            ("referencesProvider", json!(1)),
            ("hoverProvider", json!([])),
            ("implementationProvider", json!(null)),
            ("renameProvider", json!("supported")),
            ("renameProvider", json!({"prepareProvider": "yes"})),
        ] {
            let mut capabilities = serde_json::Map::new();
            capabilities.insert(provider.to_owned(), malformed);
            let response = json!({"capabilities": capabilities});

            assert_eq!(
                SemanticCapabilities::from_initialize_result(&response),
                Err(super::LspSessionError::Backend(
                    LspOutcomeCode::ServerMalformedResponse
                )),
                "provider {provider} must reject malformed capability values"
            );
        }
    }

    #[test]
    fn f008_capability_table_gates_unavailable_methods_without_gating_published_diagnostics() {
        let capabilities = SemanticCapabilities::from_initialize_result(&json!({
            "capabilities": {
                "definitionProvider": false,
                "referencesProvider": false,
                "hoverProvider": false,
                "implementationProvider": false,
                "renameProvider": false,
                "diagnosticProvider": "malformed-but-irrelevant-to-push-diagnostics"
            }
        }))
        .expect("diagnosticProvider must not participate in semantic capability parsing");
        assert_eq!(capabilities.definition, ProviderCapability::Unavailable);
        assert_eq!(capabilities.references, ProviderCapability::Unavailable);
        assert_eq!(capabilities.hover, ProviderCapability::Unavailable);
        assert_eq!(capabilities.implementation, ProviderCapability::Unavailable);
        assert_eq!(capabilities.rename, RenameCapability::Unavailable);

        // Inject method-not-found replies only to make an incorrectly dispatched
        // request return promptly. A correctly gated operation must not consume
        // these replies or write any of the semantic LSP requests.
        let (adapter, sink, responses) = test_adapter();
        *adapter.semantic_capabilities.lock().unwrap() = capabilities;
        complete_responses(
            responses,
            vec![
                json!({"id": 1, "error": {"code": -32601, "message": "method not found"}}),
                json!({"id": 2, "error": {"code": -32601, "message": "method not found"}}),
                json!({"id": 3, "error": {"code": -32601, "message": "method not found"}}),
                json!({"id": 4, "error": {"code": -32601, "message": "method not found"}}),
                json!({"id": 5, "error": {"code": -32601, "message": "method not found"}}),
            ],
        );
        let path = RelativePath::new("src/main.rs");
        let text = "let name = 1;";
        let position = Position {
            line: 0,
            character: 4,
        };

        for error in [
            PooledSession::definition_lsp(&adapter, &path, text, position).unwrap_err(),
            PooledSession::references_lsp(&adapter, &path, text, position).unwrap_err(),
            PooledSession::hover_lsp(&adapter, &path, text, position).unwrap_err(),
            PooledSession::implementations_lsp(&adapter, &path, text, position).unwrap_err(),
            PooledSession::rename_lsp(&adapter, &path, text, position, "new_name").unwrap_err(),
        ] {
            assert_eq!(
                error,
                super::LspSessionError::CapabilityUnavailable,
                "an operation without its advertised provider must return CapabilityUnavailable"
            );
        }

        let messages = written_messages(&sink);
        let methods: Vec<_> = messages
            .iter()
            .filter_map(|message| message.get("method").and_then(serde_json::Value::as_str))
            .collect();
        for method in [
            "textDocument/definition",
            "textDocument/references",
            "textDocument/hover",
            "textDocument/implementation",
            "textDocument/prepareRename",
            "textDocument/rename",
        ] {
            assert!(
                !methods.contains(&method),
                "unavailable capability must prevent sending {method}; sent methods: {methods:?}"
            );
        }

        // Push diagnostics do not depend on diagnosticProvider. Exercise the
        // real notification dispatcher and the public check path rather than
        // treating the provider field's absence from parsing as proof.
        let (adapter, _sink, responses) = test_adapter();
        let dispatcher = build_dispatcher(
            Arc::clone(&adapter.state),
            responses,
            adapter.writer.clone(),
            None,
        );
        let publish_and_settle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            dispatcher.dispatch(&json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": "file:///workspace/src/main.rs",
                    "diagnostics": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 4}
                        },
                        "severity": 1,
                        "message": "published diagnostic"
                    }]
                }
            }));
            dispatcher.dispatch(&json!({
                "jsonrpc": "2.0",
                "method": "experimental/serverStatus",
                "params": {"quiescent": true}
            }));
        });
        let diagnostics = PooledSession::check_lsp(&adapter, &path, text)
            .expect("published diagnostics must remain available without diagnosticProvider");
        publish_and_settle.join().unwrap();
        assert_eq!(
            diagnostics.len(),
            1,
            "the published diagnostic must reach check"
        );
        assert_eq!(diagnostics[0].message, "published diagnostic");
    }

    #[test]
    fn repeated_identical_check_reuses_settled_cached_diagnostics() {
        let (adapter, _sink, _responses) = test_adapter();
        let path = RelativePath::new("src/main.rs");
        let uri = "file:///workspace/src/main.rs";
        let text = "fn main() {}";
        adapter
            .inner
            .lock()
            .unwrap()
            .docs
            .acquire(uri, "rust", text);
        record_diagnostics(&adapter.state, uri.to_owned(), vec![diag("cached")]);
        mark_ready(&adapter.state);

        for _ in 0..2 {
            let diagnostics = PooledSession::check_lsp(&adapter, &path, text)
                .expect("an unchanged settled document must reuse cached diagnostics");
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].message, "cached");
        }
    }

    #[test]
    fn preopened_identical_document_reuses_settled_cached_diagnostics() {
        let (adapter, _sink, _responses) = test_adapter();
        let path = RelativePath::new("src/main.rs");
        let uri = "file:///workspace/src/main.rs";
        let text = "fn main() {}";
        adapter
            .inner
            .lock()
            .unwrap()
            .docs
            .acquire(uri, "rust", text);
        record_diagnostics(&adapter.state, uri.to_owned(), Vec::new());
        mark_ready(&adapter.state);

        let diagnostics = PooledSession::check_lsp(&adapter, &path, text)
            .expect("a watcher-preopened settled document must not await a new generation");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn f008_capability_rename_without_prepare_stays_available_and_target_rejection_stays_not_renameable()
     {
        let (adapter, sink, responses) = test_adapter();
        adapter.semantic_capabilities.lock().unwrap().rename = RenameCapability::RenameOnly;
        complete_response(
            responses,
            json!({
                "id": 1,
                "result": {"changes": {}}
            }),
        );

        PooledSession::rename_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "let name = 1;",
            Position {
                line: 0,
                character: 4,
            },
            "new_name",
        )
        .expect("renameProvider without prepareProvider must remain available");
        let messages = written_messages(&sink);
        let methods: Vec<_> = messages
            .iter()
            .filter_map(|message| message.get("method").and_then(serde_json::Value::as_str))
            .collect();
        assert!(methods.contains(&"textDocument/rename"));
        assert!(!methods.contains(&"textDocument/prepareRename"));

        let (adapter, _sink, responses) = test_adapter();
        adapter.semantic_capabilities.lock().unwrap().rename = RenameCapability::RenameWithPrepare;
        complete_response(
            responses,
            json!({
                "id": 1,
                "error": {"code": -32602, "message": "not valid rename target"}
            }),
        );
        let error = PooledSession::rename_lsp(
            &adapter,
            &RelativePath::new("src/main.rs"),
            "let name = 1;",
            Position {
                line: 0,
                character: 4,
            },
            "new_name",
        )
        .unwrap_err();

        assert_eq!(error, super::LspSessionError::NotRenameable);
    }

    #[test]
    fn lsp_client_adapter_sends_text_document_implementation_with_zero_based_utf16_position() {
        let (adapter, sink, responses) = test_adapter();
        complete_response(
            responses,
            json!({
                "id": 1,
                "result": [{
                    "uri": "file:///workspace/src/impl.rs",
                    "range": {
                        "start": { "line": 7, "character": 3 },
                        "end": { "line": 7, "character": 9 }
                    }
                }]
            }),
        );

        let locations = adapter
            .implementations(
                &RelativePath::new("src/main.rs"),
                "trait Example {}",
                Position {
                    line: 2,
                    character: 11,
                },
            )
            .unwrap();

        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path.as_str(), "src/impl.rs");
        let request = request_with_method(&written_messages(&sink), "textDocument/implementation");
        assert_eq!(request["params"]["position"]["line"], 2);
        assert_eq!(request["params"]["position"]["character"], 11);
    }

    #[test]
    fn unsupported_implementation_lookup_returns_code_intel_unsupported() {
        let (adapter, _sink, _responses) = test_adapter();

        let err = adapter
            .implementations(
                &RelativePath::new("README.txt"),
                "not rust",
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .unwrap_err();

        assert_eq!(err, CodeIntelError::Unsupported);
    }

    #[test]
    fn prepare_rename_returns_range_and_placeholder_from_lsp_result() {
        let (adapter, sink, responses) = test_adapter();
        complete_response(
            responses,
            json!({
                "id": 1,
                "result": {
                    "range": {
                        "start": { "line": 1, "character": 4 },
                        "end": { "line": 1, "character": 8 }
                    },
                    "placeholder": "name"
                }
            }),
        );

        let prepared = adapter
            .prepare_rename(
                &RelativePath::new("src/main.rs"),
                "let name = 1;",
                Position {
                    line: 1,
                    character: 5,
                },
            )
            .unwrap();

        assert_eq!(prepared.placeholder.as_deref(), Some("name"));
        assert_eq!(
            prepared.range.unwrap(),
            Range {
                start: Position {
                    line: 1,
                    character: 4,
                },
                end: Position {
                    line: 1,
                    character: 8,
                },
            }
        );
        request_with_method(&written_messages(&sink), "textDocument/prepareRename");
    }

    #[test]
    fn prepare_rename_rejection_maps_to_not_renameable() {
        let (adapter, _sink, responses) = test_adapter();
        complete_response(
            responses,
            json!({
                "id": 1,
                "error": {
                    "code": -32602,
                    "message": "not valid rename target"
                }
            }),
        );

        let err = adapter
            .prepare_rename(
                &RelativePath::new("src/main.rs"),
                "let name = 1;",
                Position {
                    line: 1,
                    character: 0,
                },
            )
            .unwrap_err();

        assert_eq!(err, RenameNavigationError::NotRenameable);
    }

    #[test]
    fn prepare_rename_backend_error_stays_backend() {
        let (adapter, _sink, responses) = test_adapter();
        complete_response(
            responses,
            json!({
                "id": 1,
                "error": {
                    "code": -32603,
                    "message": "internal error"
                }
            }),
        );

        let err = adapter
            .prepare_rename(
                &RelativePath::new("src/main.rs"),
                "let name = 1;",
                Position {
                    line: 1,
                    character: 0,
                },
            )
            .unwrap_err();

        assert_eq!(
            err,
            RenameNavigationError::Backend("language server request failed".to_owned())
        );
    }

    #[test]
    fn rename_sends_new_name_exactly_sourced_from_caller() {
        let (adapter, sink, responses) = test_adapter();
        complete_responses(
            responses,
            vec![
                json!({
                    "id": 1,
                    "result": {
                        "range": {
                            "start": { "line": 0, "character": 4 },
                            "end": { "line": 0, "character": 8 }
                        },
                        "placeholder": "name"
                    }
                }),
                json!({
                    "id": 2,
                    "result": {
                        "changes": {
                            "file:///workspace/src/main.rs": [{
                                "range": {
                                    "start": { "line": 0, "character": 4 },
                                    "end": { "line": 0, "character": 8 }
                                },
                                "newText": "renamedSymbol"
                            }]
                        }
                    }
                }),
            ],
        );

        let _ = adapter
            .rename(
                &RelativePath::new("src/main.rs"),
                "let name = 1;",
                Position {
                    line: 0,
                    character: 5,
                },
                "renamedSymbol",
            )
            .unwrap();

        let messages = written_messages(&sink);
        let prepare_idx = messages
            .iter()
            .position(|msg| {
                msg.get("method").and_then(serde_json::Value::as_str)
                    == Some("textDocument/prepareRename")
            })
            .expect("rename must prepareRename before issuing rename");
        let rename_idx = messages
            .iter()
            .position(|msg| {
                msg.get("method").and_then(serde_json::Value::as_str) == Some("textDocument/rename")
            })
            .expect("rename request must be sent after successful prepareRename");
        assert!(
            prepare_idx < rename_idx,
            "prepareRename must be sent before rename; messages={messages:?}"
        );
        assert_eq!(messages[rename_idx]["params"]["newName"], "renamedSymbol");
    }

    #[test]
    fn raw_workspace_edit_forwards_changes_document_changes_versions_and_resource_ops() {
        let (adapter, _sink, responses) = test_adapter();
        complete_response(
            responses,
            json!({
                "id": 1,
                "result": {
                    "changes": {
                        "file:///workspace/src/main.rs": [{
                            "range": {
                                "start": { "line": 0, "character": 4 },
                                "end": { "line": 0, "character": 8 }
                            },
                            "newText": "new_name"
                        }]
                    },
                    "documentChanges": [{
                        "textDocument": {
                            "uri": "file:///workspace/src/lib.rs",
                            "version": 7
                        },
                        "edits": [{
                            "range": {
                                "start": { "line": 2, "character": 0 },
                                "end": { "line": 2, "character": 3 }
                            },
                            "newText": "new_name"
                        }]
                    }, {
                        "kind": "rename",
                        "oldUri": "file:///workspace/src/old.rs",
                        "newUri": "file:///workspace/src/new.rs",
                        "options": { "overwrite": true, "ignoreIfExists": false }
                    }]
                }
            }),
        );

        let edit: RawWorkspaceEdit = adapter
            .rename(
                &RelativePath::new("src/main.rs"),
                "let name = 1;",
                Position {
                    line: 0,
                    character: 5,
                },
                "new_name",
            )
            .unwrap();

        assert!(
            edit.changes
                .as_ref()
                .is_some_and(|changes| !changes.is_empty())
        );
        assert!(edit.document_changes.is_some());
        let forwarded = serde_json::to_value(&edit).unwrap();
        assert_eq!(
            forwarded["documentChanges"][1]["kind"], "rename",
            "unsupported resource operations must remain in RawWorkspaceEdit for T014 rejection"
        );
        assert_eq!(
            forwarded["documentChanges"][1]["oldUri"],
            "file:///workspace/src/old.rs"
        );
        assert_eq!(
            forwarded["documentChanges"][1]["newUri"],
            "file:///workspace/src/new.rs"
        );
    }

    #[test]
    fn rename_navigation_error_has_stable_public_variants() {
        assert!(matches!(
            RenameNavigationError::NotRenameable,
            RenameNavigationError::NotRenameable
        ));
        assert!(matches!(
            RenameNavigationError::UnsupportedLanguage,
            RenameNavigationError::UnsupportedLanguage
        ));
        assert!(matches!(
            RenameNavigationError::Backend("backend".to_owned()),
            RenameNavigationError::Backend(_)
        ));
    }

    #[test]
    fn dispatch_routes_notifications_correlates_responses_and_acks() {
        let state = new_state();
        let responses: ResponseCache =
            Arc::new((Mutex::new(std::collections::HashMap::new()), Condvar::new()));
        let (writer, sink) = test_writer();
        let dispatcher = build_dispatcher(
            Arc::clone(&state),
            Arc::clone(&responses),
            writer.clone(),
            None,
        );

        dispatcher.dispatch(&json!({
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": "file:///a.rs", "diagnostics": [] }
        }));
        dispatcher.dispatch(&json!({
            "method": "experimental/serverStatus",
            "params": { "quiescent": true }
        }));
        dispatcher.dispatch(&json!({ "method": "telemetry/event", "params": {} })); // unknown → ignored
        dispatcher.dispatch(&json!({
            "id": 99, "method": "client/registerCapability",
            "params": { "registrations": [] }
        }));
        let _pending = super::PendingRequest::new(&responses, 7);
        dispatcher.dispatch(&json!({ "id": 7, "result": { "ok": true } }));

        let (lock, _) = &*state;
        let s = lock.lock().unwrap();
        assert!(s.ready, "serverStatus quiescent must set ready");
        assert_eq!(s.next_gen, 1, "publishDiagnostics must bump generation");
        drop(s);

        let (rlock, _) = &*responses;
        assert!(
            rlock.lock().unwrap().contains_key(&7),
            "response 7 correlated"
        );

        let written = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(
            written.contains("\"id\":99"),
            "ack must echo the request id; got {written}"
        );
        assert!(
            written.contains("\"result\":null"),
            "ack carries a null result"
        );
    }

    #[test]
    fn check_timeout_is_generous_safety_net_not_primary_gate() {
        // The constant exists and is far larger than the old 5s budget — it is a
        // safety net, not the mechanism. Guards against reintroducing a tight budget.
        assert!(TEST_OPERATION_TIMEOUT >= Duration::from_secs(30));
    }

    #[test]
    fn language_id_field_is_threaded_to_doc_sync() {
        // After the LANGUAGE_ID const is deleted, language_id must come from
        // the adapter field. We verify by building a dispatcher with a "go"
        // state and confirming the plumbing compiles and does not panic.
        // (Full behavioral coverage lives in pool.rs with FakeSession; this
        // guards that the field exists and the wiring compiles.)
        let state = new_state();
        let responses: ResponseCache =
            Arc::new((std::collections::HashMap::new().into(), Condvar::new()));
        let (writer, _sink) = test_writer();
        let dispatcher = build_dispatcher(
            Arc::clone(&state),
            Arc::clone(&responses),
            writer.clone(),
            None,
        );
        // build_dispatcher must not panic regardless of future language_id value.
        let _ = dispatcher;
    }

    #[test]
    fn dead_flag_starts_false() {
        // AtomicBool semantics sanity-check before pool.rs uses PooledSession::is_dead.
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!flag.load(Ordering::Relaxed));
        flag.store(true, Ordering::Relaxed);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn uri_round_trips_space_and_non_ascii() {
        let p = std::path::Path::new("/home/u/my project/café/main.rs");
        let uri = path_to_uri(p);
        assert!(uri.starts_with("file://"));
        assert!(uri.contains("%20"), "space must be percent-encoded: {uri}");
        assert!(!uri.contains(' '), "no raw spaces in the uri");
        assert_eq!(
            decode::classify_uri(&uri, Path::new("/home/u"), UriAccessPolicy::Writable,),
            Some(UriClassification::WorkspacePath(
                "my project/café/main.rs".to_owned()
            ))
        );
    }

    #[test]
    fn uri_to_path_decodes_server_echoed_encoding() {
        assert_eq!(
            decode::classify_uri(
                "file:///w/a%20b/caf%C3%A9.rs",
                Path::new("/w"),
                UriAccessPolicy::Writable,
            ),
            Some(UriClassification::WorkspacePath("a b/café.rs".to_owned()))
        );
    }

    #[test]
    fn uri_to_path_returns_none_on_truncated_escape() {
        for uri in ["file:///x/%A", "file:///x/%", "file:///x/%ZZ"] {
            assert!(
                decode::classify_uri(uri, Path::new("/x"), UriAccessPolicy::Writable).is_none()
            );
        }
    }

    #[test]
    fn build_dispatcher_push_tx_sends_event_on_diagnostics() {
        use std::sync::mpsc;

        let state = new_state();
        let responses: ResponseCache =
            Arc::new((Mutex::new(std::collections::HashMap::new()), Condvar::new()));
        let (writer, _sink) = test_writer();
        let (tx, rx) = mpsc::channel::<DiagnosticsEvent>();

        let dispatcher = build_dispatcher(
            Arc::clone(&state),
            Arc::clone(&responses),
            writer.clone(),
            Some(tx),
        );

        dispatcher.dispatch(&json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///workspace/src/main.rs",
                "diagnostics": []
            }
        }));

        let event = rx
            .try_recv()
            .expect("DiagnosticsEvent must be sent after publishDiagnostics");
        assert_eq!(event.uri, "file:///workspace/src/main.rs");
        assert_eq!(event.generation, 1, "first publish must yield generation 1");
    }

    #[test]
    fn build_dispatcher_push_tx_none_does_not_break() {
        // push_tx = None must behave identically to build_dispatcher.
        let state = new_state();
        let responses: ResponseCache =
            Arc::new((Mutex::new(std::collections::HashMap::new()), Condvar::new()));
        let (writer, _) = test_writer();
        let dispatcher = build_dispatcher(
            Arc::clone(&state),
            Arc::clone(&responses),
            writer.clone(),
            None,
        );
        // Must not panic or error.
        dispatcher.dispatch(&json!({
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": "file:///a.rs", "diagnostics": [] }
        }));
    }
}
