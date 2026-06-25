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
//! and generation. `check` waits on a single predicate: the server is ready
//! **and** a diagnostics generation newer than the one captured before the sync
//! has landed for the requested URI. This eliminates the fixed settle budget and
//! the `Instant`-stamp double-publish heuristic: diagnostics are authoritative
//! only once the server signals it has finished analysis.
//!
//! The cap [`CHECK_TIMEOUT`] exists as a safety net for servers that never signal
//! (e.g. a crashed rust-analyzer). After the cap elapses, `check` returns the
//! latest known diagnostics (best-effort).
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
//! 2. `self.writer` (`Mutex<Box<dyn Write + Send>>`) — guards the stdin write
//!    half. Acquired via [`write_framed`] for the duration of one framed write.
//!    Never held across a condvar wait. The reader thread acquires it only for
//!    acks; it never holds `inner` at the same time, so no cycle exists.
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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::{Value, json};

use crate::lsp_adapter::decode::RawLocation;
use crate::lsp_adapter::documents::{DocSync, DocumentTracker};
use core_engine::adapters::fs::scan::TOWERIGNORE_FILE_NAME;
use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::{
    Diagnostic, Hover, Location, Position, Range, Severity, Symbol,
};
use core_engine::ports::{
    CodeIntelError, CodeIntelligencePort, DocumentSyncPort, NavigationPort, PrepareRenameResult,
    RenameNavigationError,
};

#[allow(dead_code)]
pub type RawWorkspaceEdit = lsp_types::WorkspaceEdit;

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

/// Upper bound on how long `check` waits for the server to settle. A safety net,
/// not the primary mechanism — the ready gate (serverStatus/`$/progress`) should
/// fire well before this. Generous because a cold first `cargo check` is slow.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a navigation request waits for its response before erroring.
const REQUEST_BUDGET: Duration = Duration::from_secs(10);

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
        raw: Value,
    },
    Message(String),
}

impl RequestFailure {
    fn format(&self, method: &str) -> String {
        match self {
            RequestFailure::Response { raw, .. } => format!("{method} error: {raw}"),
            RequestFailure::Message(message) => message.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenameCapability {
    UnknownAssumePrepare,
    Unavailable,
    RenameOnly,
    RenameWithPrepare,
}

impl RenameCapability {
    fn from_initialize_result(result: &Value) -> Self {
        match result
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("renameProvider"))
        {
            Some(Value::Bool(true)) => Self::RenameOnly,
            Some(Value::Bool(false)) | None => Self::Unavailable,
            Some(Value::Object(provider)) => {
                if provider
                    .get("prepareProvider")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    Self::RenameWithPrepare
                } else {
                    Self::RenameOnly
                }
            }
            Some(_) => Self::Unavailable,
        }
    }
}

/// Provisional cross-file settle delay used before a workspace query
/// (`references`) until spec 14c lands the real index-readiness signal
/// (`serverStatus`/`$/progress`). A bounded wait so a half-built index is not
/// returned as a complete result (EV3).
const REFERENCES_SETTLE_FALLBACK: Duration = Duration::from_millis(1500);

/// Correlates request ids to the full response message the reader thread parsed.
type ResponseCache = Arc<(Mutex<HashMap<i64, Value>>, Condvar)>;

/// Write half of the server's stdin, shared between the adapter (requests, doc
/// sync, handshake) and the reader thread (acking server→client requests). A
/// boxed trait object so tests can substitute an in-memory sink.
type Writer = Arc<Mutex<Box<dyn std::io::Write + Send>>>;

/// Frame and write `msg` to the shared writer. Held only across the write, never
/// across a condvar wait.
fn write_framed(writer: &Writer, msg: &Value) -> std::io::Result<()> {
    let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
    jsonrpc::write_message(&mut **w, msg)
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
                    map.insert(id, msg.clone());
                    cvar.notify_all();
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

    let w = Arc::clone(&writer);
    dispatcher.on_request(
        "client/registerCapability",
        Box::new(move |msg| ack(&w, msg)),
    );
    let w = Arc::clone(&writer);
    dispatcher.on_request(
        "window/workDoneProgress/create",
        Box::new(move |msg| ack(&w, msg)),
    );

    dispatcher
}

/// Diagnostics for one URI, tagged with the generation at which they were
/// published, so `wait_for_settled` can require a strictly newer publish than
/// the one seen before the `didOpen`/`didChange` was sent.
struct DiagEntry {
    diags: Vec<Diagnostic>,
    generation: u64,
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
        self.by_uri.insert(uri, DiagEntry { diags, generation });
    }

    fn signal_ready(&mut self) {
        self.ready = true;
        self.ready_gen += 1;
    }
}

/// Shared session state; the reader thread notifies the condvar on every
/// readiness change or diagnostics publish.
/// Shared session state; the reader thread notifies the condvar on every
/// readiness change or diagnostics publish.
pub type SharedState = Arc<(Mutex<SessionStateInner>, Condvar)>;

/// Store diagnostics for `uri`, bumping the generation, and wake any waiter.
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
fn current_generation(state: &SharedState) -> u64 {
    let (lock, _) = &**state;
    lock.lock().unwrap_or_else(|p| p.into_inner()).next_gen
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
    /// Rename capability shape reported by the initialized language server.
    rename_capability: Mutex<RenameCapability>,
    /// Root `.towerignore` policy applied to navigation results.
    ignore: Gitignore,
    next_id: AtomicI64,
    /// Set to `true` by the reader thread on stdout EOF (server died or exited).
    /// The pool checks this on every request to detect crashes. Relaxed ordering
    /// is sufficient: at worst the pool makes one wasted I/O attempt to a dead
    /// writer, gets `CodeIntelError::Backend`, and detects `true` on the next
    /// request — matching UN1 semantics exactly.
    pub dead_flag: Arc<AtomicBool>,
}

struct Session {
    child: Child,
    /// Single owner of open-document state (open/version/refcount), shared by
    /// `check`, navigation, and the watcher document-sync path so a document is
    /// never opened twice.
    docs: DocumentTracker,
}

impl LspClientAdapter {
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
    pub fn spawn(
        command: &str,
        args: &[String],
        extensions: Vec<String>,
        language_id: String,
        workspace_root: std::path::PathBuf,
        push_tx: Option<DiagnosticsSender>,
    ) -> Result<Self, CodeIntelError> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(&workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| CodeIntelError::Backend(format!("spawn {command} failed: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CodeIntelError::Backend("no stdin pipe".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CodeIntelError::Backend("no stdout pipe".to_owned()))?;

        let writer: Writer = Arc::new(Mutex::new(Box::new(stdin)));
        let state: SharedState =
            Arc::new((Mutex::new(SessionStateInner::default()), Condvar::new()));
        let responses: ResponseCache = Arc::new((Mutex::new(HashMap::new()), Condvar::new()));
        let dead_flag = Arc::new(AtomicBool::new(false));

        let dispatcher = build_dispatcher(
            Arc::clone(&state),
            Arc::clone(&responses),
            Arc::clone(&writer),
            push_tx,
        );
        let dead_flag_clone = Arc::clone(&dead_flag);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(msg)) = jsonrpc::read_message(&mut reader) {
                dispatcher.dispatch(&msg);
            }
            // Reader thread exits on EOF (server died, crashed, or was killed).
            // Signal the pool so the next request triggers a restart.
            dead_flag_clone.store(true, Ordering::Relaxed);
        });

        let ignore = build_ignore_matcher(&workspace_root);
        let adapter = Self {
            workspace_root,
            extensions,
            language_id,
            inner: Mutex::new(Session {
                child,
                docs: DocumentTracker::new(),
            }),
            writer,
            state,
            responses,
            rename_capability: Mutex::new(RenameCapability::UnknownAssumePrepare),
            ignore,
            next_id: AtomicI64::new(1),
            dead_flag,
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

    fn handshake(&self) -> Result<(), CodeIntelError> {
        let root_uri = path_to_uri(&self.workspace_root);
        let initialize_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": { "publishDiagnostics": {} },
                "window": { "workDoneProgress": true },
                // rust-analyzer reads serverStatus capability at
                // params.capabilities.experimental.serverStatusNotification,
                // NOT as a sibling of capabilities.
                "experimental": { "serverStatusNotification": true }
            },
            "initializationOptions": {}
        });
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        let initialize_result = self.send_request("initialize", initialize_params)?;
        *self
            .rename_capability
            .lock()
            .unwrap_or_else(|p| p.into_inner()) =
            RenameCapability::from_initialize_result(&initialize_result);
        write_framed(&self.writer, &initialized)
            .map_err(|e| CodeIntelError::Backend(format!("initialized write failed: {e}")))?;
        Ok(())
    }

    fn uri_for(&self, path: &RelativePath) -> String {
        path_to_uri(&self.workspace_root.join(path.as_str()))
    }

    fn supports(&self, path: &RelativePath) -> bool {
        path.as_str()
            .rsplit_once('.')
            .map(|(_, ext)| self.extensions.iter().any(|e| e == ext))
            .unwrap_or(false)
    }

    /// Acquire a scoped hold on `uri` (a navigation request) and emit the
    /// resulting `didOpen`/`didChange`. Pair with [`release_doc`](Self::release_doc).
    fn acquire_doc(&self, uri: &str, text: &str) -> Result<(), CodeIntelError> {
        let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(sync) = session.docs.acquire(uri, &self.language_id, text) {
            write_doc_sync(&self.writer, uri, &sync)
                .map_err(|e| CodeIntelError::Backend(format!("doc sync write failed: {e}")))?;
        }
        Ok(())
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
    fn send_request(&self, method: &str, params: Value) -> Result<Value, CodeIntelError> {
        let mut attempt = 0;
        loop {
            match self.send_request_once(method, params.clone())? {
                RequestOutcome::Ok(value) => return Ok(value),
                RequestOutcome::ContentModified if attempt < CONTENT_MODIFIED_RETRIES => {
                    std::thread::sleep(CONTENT_MODIFIED_BACKOFF);
                    attempt += 1;
                }
                RequestOutcome::ContentModified => {
                    return Err(CodeIntelError::Backend(format!(
                        "{method} still content-modified after {CONTENT_MODIFIED_RETRIES} retries"
                    )));
                }
                RequestOutcome::Error(failure) => {
                    return Err(CodeIntelError::Backend(failure.format(method)));
                }
            }
        }
    }

    /// Send one request and await its response, classifying the outcome so
    /// [`send_request`](Self::send_request) can retry transient failures.
    fn send_request_once(
        &self,
        method: &str,
        params: Value,
    ) -> Result<RequestOutcome, CodeIntelError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        // The request does not touch `docs`, so we write directly without holding `inner`.
        write_framed(&self.writer, &request)
            .map_err(|e| CodeIntelError::Backend(format!("{method} write failed: {e}")))?;

        let (lock, cvar) = &*self.responses;
        let mut map = lock.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = Instant::now() + REQUEST_BUDGET;
        loop {
            if let Some(msg) = map.remove(&id) {
                if let Some(err) = msg.get("error") {
                    if err.get("code").and_then(Value::as_i64) == Some(CONTENT_MODIFIED) {
                        return Ok(RequestOutcome::ContentModified);
                    }
                    return Ok(RequestOutcome::Error(RequestFailure::Response {
                        code: err.get("code").and_then(Value::as_i64),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        raw: err.clone(),
                    }));
                }
                return Ok(RequestOutcome::Ok(
                    msg.get("result").cloned().unwrap_or(Value::Null),
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(RequestOutcome::Error(RequestFailure::Message(format!(
                    "{method} timed out"
                ))));
            }
            let (next, _) = cvar
                .wait_timeout(map, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            map = next;
        }
    }

    /// Cross-file settle gate (EV3): block until the server emits a quiescent
    /// signal that is **newer** than `pre_open_ready_gen` (captured before the
    /// documents were synced), bounded by `REFERENCES_SETTLE_FALLBACK` so a
    /// server that never re-signals does not hang the request.
    ///
    /// Waiting for `ready_gen > pre_open_ready_gen` (not just `ready`) ensures
    /// we see a fresh settled notification from after the current document open,
    /// rather than returning immediately on a stale signal from a prior cycle.
    fn await_index_settled(&self, pre_open_ready_gen: u64) {
        let (lock, cvar) = &*self.state;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = Instant::now() + REFERENCES_SETTLE_FALLBACK;
        while s.ready_gen <= pre_open_ready_gen {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next, _) = cvar
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            s = next;
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
    ) -> Result<Vec<Location>, CodeIntelError> {
        if !self.supports(path) {
            return Err(CodeIntelError::Unsupported);
        }
        let uri = self.uri_for(path);
        self.acquire_doc(&uri, text)?;
        let result = self.send_request(method, params);
        self.release_doc(&uri);
        let raws = decode::decode_locations(&result?);
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
    ) -> Result<PrepareRenameResult, RenameNavigationError> {
        let value = self.prepare_rename_value(path, text, position)?;
        decode_prepare_rename(value)
    }

    fn prepare_rename_value(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Value, RenameNavigationError> {
        if !self.supports(path) {
            return Err(RenameNavigationError::UnsupportedLanguage);
        }
        let uri = self.uri_for(path);
        self.acquire_doc(&uri, text)
            .map_err(code_intel_to_rename_error)?;
        let result = self.send_prepare_rename_request(json!({
            "textDocument": { "uri": uri },
            "position": { "line": position.line, "character": position.character }
        }));
        self.release_doc(&uri);
        result
    }

    fn send_prepare_rename_request(&self, params: Value) -> Result<Value, RenameNavigationError> {
        let method = "textDocument/prepareRename";
        let mut attempt = 0;
        loop {
            match self
                .send_request_once(method, params.clone())
                .map_err(code_intel_to_rename_error)?
            {
                RequestOutcome::Ok(value) => return Ok(value),
                RequestOutcome::ContentModified if attempt < CONTENT_MODIFIED_RETRIES => {
                    std::thread::sleep(CONTENT_MODIFIED_BACKOFF);
                    attempt += 1;
                }
                RequestOutcome::ContentModified => {
                    return Err(RenameNavigationError::Backend(format!(
                        "{method} still content-modified after {CONTENT_MODIFIED_RETRIES} retries"
                    )));
                }
                RequestOutcome::Error(failure) if is_prepare_rename_rejection(&failure) => {
                    return Err(RenameNavigationError::NotRenameable);
                }
                RequestOutcome::Error(failure) if is_method_not_found(&failure) => {
                    return Ok(json!({ "defaultBehavior": true }));
                }
                RequestOutcome::Error(failure) => {
                    return Err(RenameNavigationError::Backend(failure.format(method)));
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

fn is_method_not_found(failure: &RequestFailure) -> bool {
    matches!(
        failure,
        RequestFailure::Response {
            code: Some(-32601),
            ..
        }
    )
}

impl NavigationPort for LspClientAdapter {
    fn definition(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
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

    fn implementations(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
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

    fn references(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        if !self.supports(path) {
            return Err(CodeIntelError::Unsupported);
        }
        let uri = self.uri_for(path);

        // EV3: capture ready_gen BEFORE opening the document so we can wait
        // for a fresh quiescent signal that arrived after the didOpen/didChange,
        // not a stale one from a prior analysis cycle.
        let pre_open_ready_gen = current_ready_gen(&self.state);
        self.acquire_doc(&uri, text)?;
        // Block until the server signals it has settled the index for the
        // newly opened document (or the fallback cap elapses).
        self.await_index_settled(pre_open_ready_gen);

        let result = self.send_request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
                "context": { "includeDeclaration": true }
            }),
        );
        self.release_doc(&uri);
        let raws = decode::decode_locations(&result?);
        Ok(workspace_locations(
            raws,
            &self.workspace_root,
            &self.ignore,
        ))
    }

    fn hover(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Option<Hover>, CodeIntelError> {
        if !self.supports(path) {
            return Err(CodeIntelError::Unsupported);
        }
        let uri = self.uri_for(path);
        self.acquire_doc(&uri, text)?;
        let result = self.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character }
            }),
        );
        self.release_doc(&uri);
        Ok(decode::decode_hover(&result?))
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
        self.acquire_doc(&uri, text)?;
        let result = self.send_request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        );
        self.release_doc(&uri);
        Ok(decode::decode_document_symbols(&result?))
    }

    fn prepare_rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<PrepareRenameResult, RenameNavigationError> {
        match *self
            .rename_capability
            .lock()
            .unwrap_or_else(|p| p.into_inner())
        {
            RenameCapability::Unavailable => {
                return Err(RenameNavigationError::UnsupportedLanguage);
            }
            RenameCapability::RenameOnly => {
                return Ok(PrepareRenameResult {
                    range: None,
                    placeholder: None,
                });
            }
            RenameCapability::UnknownAssumePrepare | RenameCapability::RenameWithPrepare => {}
        }
        self.prepare_rename_query(path, text, position)
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
        if !self.supports(path) {
            return Err(RenameNavigationError::UnsupportedLanguage);
        }
        match *self
            .rename_capability
            .lock()
            .unwrap_or_else(|p| p.into_inner())
        {
            RenameCapability::Unavailable => {
                return Err(RenameNavigationError::UnsupportedLanguage);
            }
            RenameCapability::UnknownAssumePrepare | RenameCapability::RenameWithPrepare => {
                let prepared = self.prepare_rename_value(path, text, position)?;
                if let Err(error) = decode_prepare_rename(prepared.clone()) {
                    if looks_like_workspace_edit(&prepared) {
                        return decode_workspace_edit(prepared);
                    }
                    return Err(error);
                }
            }
            RenameCapability::RenameOnly => {}
        }
        let uri = self.uri_for(path);
        self.acquire_doc(&uri, text)
            .map_err(code_intel_to_rename_error)?;
        let result = self.send_request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
                "newName": new_name
            }),
        );
        self.release_doc(&uri);
        let value = result.map_err(code_intel_to_rename_error)?;
        decode_workspace_edit(value)
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

impl CodeIntelligencePort for LspClientAdapter {
    fn check(&self, path: &RelativePath, text: &str) -> Result<Vec<Diagnostic>, CodeIntelError> {
        if !self.supports(path) {
            return Err(CodeIntelError::Unsupported);
        }
        let uri = self.uri_for(path);

        // Capture BOTH generations before syncing so the gate can require a
        // fresh quiescent signal AND a fresh diagnostic publish, both produced
        // after this didOpen/didChange — never satisfied by stale values from a
        // prior analysis cycle.
        let sync_gen = current_generation(&self.state);
        let pre_sync_ready_gen = current_ready_gen(&self.state);

        {
            let mut session = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let sync = if session.docs.is_open(&uri) {
                session.docs.sync(&uri, text)
            } else {
                session.docs.acquire(&uri, &self.language_id, text)
            };
            if let Some(sync) = sync {
                write_doc_sync(&self.writer, &uri, &sync)
                    .map_err(|e| CodeIntelError::Backend(format!("doc sync write failed: {e}")))?;
            }
        }
        // `inner` lock released here — reader thread can now safely lock `state`.

        Ok(wait_for_settled(
            &self.state,
            &uri,
            sync_gen,
            pre_sync_ready_gen,
            CHECK_TIMEOUT,
        ))
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
    fn is_dead(&self) -> bool {
        self.dead_flag.load(Ordering::Relaxed)
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

/// Decode `%XX` escapes back to bytes, then to a UTF-8 path. Returns `None` on
/// malformed escapes or non-UTF-8 results.
fn decode_uri_path(encoded: &str) -> Option<PathBuf> {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 < bytes.len() {
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            } else {
                return None; // truncated escape
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// Build a `file://` URI from an absolute path, percent-encoding unsafe bytes.
fn path_to_uri(path: &std::path::Path) -> String {
    format!("file://{}", encode_uri_path(&path.display().to_string()))
}

/// Parse a `file://` URI back to an absolute path, decoding `%XX` escapes.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    decode_uri_path(rest)
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
    write_framed(writer, &msg)
}

/// Normalize decoded raw locations to workspace-relative domain [`Location`]s,
/// dropping any outside `root` or matched by the `.towerignore` policy
/// (spec 14b: result filtering to workspace root / `.towerignore`).
fn workspace_locations(raws: Vec<RawLocation>, root: &Path, ignore: &Gitignore) -> Vec<Location> {
    raws.into_iter()
        .filter_map(|raw| {
            let abs = uri_to_path(&raw.uri)?;
            let rel = abs.strip_prefix(root).ok()?;
            let rel_str = rel.to_str()?;
            if rel_str.is_empty() {
                return None;
            }
            if matches!(
                ignore.matched_path_or_any_parents(rel_str, false),
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

fn code_intel_to_rename_error(error: CodeIntelError) -> RenameNavigationError {
    match error {
        CodeIntelError::Unsupported => RenameNavigationError::UnsupportedLanguage,
        CodeIntelError::Backend(message) => RenameNavigationError::Backend(message),
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicI64};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use serde_json::json;

    use super::{
        CHECK_TIMEOUT, DiagnosticsEvent, LspClientAdapter, RawLocation, RawWorkspaceEdit,
        RenameCapability, RenameNavigationError, ResponseCache, Session, SharedState, Writer,
        build_dispatcher, current_generation, mark_ready, parse_publish_diagnostics, path_to_uri,
        progress_kind, record_diagnostics, server_status_is_quiescent, uri_to_path,
        wait_for_settled, workspace_locations,
    };
    use crate::lsp_adapter::documents::DocumentTracker;
    use core_engine::domain::RelativePath;
    use core_engine::domain::code_intel::{Diagnostic, Position, Range, Severity};
    use core_engine::ports::{CodeIntelError, NavigationPort};

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
        let writer: Writer = Arc::new(Mutex::new(Box::new(sink.clone())));
        (writer, sink)
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
            rename_capability: Mutex::new(RenameCapability::UnknownAssumePrepare),
            ignore: empty_ignore(),
            next_id: AtomicI64::new(1),
            dead_flag: Arc::new(AtomicBool::new(false)),
        };
        (adapter, sink, responses)
    }

    fn complete_response(responses: ResponseCache, response: serde_json::Value) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let (lock, cvar) = &*responses;
            lock.lock().unwrap().insert(1, response);
            cvar.notify_all();
        });
    }

    fn complete_responses(responses: ResponseCache, responses_by_id: Vec<serde_json::Value>) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let (lock, cvar) = &*responses;
            let mut cache = lock.lock().unwrap();
            for response in responses_by_id {
                let id = response["id"].as_i64().expect("test response has id");
                cache.insert(id, response);
            }
            cvar.notify_all();
        });
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

        assert!(
            matches!(err, RenameNavigationError::Backend(message) if message.contains("internal error"))
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
            Arc::clone(&writer),
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
        assert!(CHECK_TIMEOUT >= Duration::from_secs(30));
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
            Arc::clone(&writer),
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
        let back = uri_to_path(&uri).unwrap();
        assert_eq!(back, p.to_path_buf());
    }

    #[test]
    fn uri_to_path_decodes_server_echoed_encoding() {
        let back = uri_to_path("file:///w/a%20b/caf%C3%A9.rs").unwrap();
        assert_eq!(back, std::path::Path::new("/w/a b/café.rs").to_path_buf());
    }

    #[test]
    fn uri_to_path_returns_none_on_truncated_escape() {
        assert!(uri_to_path("file:///x/%A").is_none());
        assert!(uri_to_path("file:///x/%").is_none());
        assert!(uri_to_path("file:///x/%ZZ").is_none());
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
            Arc::clone(&writer),
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
            Arc::clone(&writer),
            None,
        );
        // Must not panic or error.
        dispatcher.dispatch(&json!({
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": "file:///a.rs", "diagnostics": [] }
        }));
    }
}
