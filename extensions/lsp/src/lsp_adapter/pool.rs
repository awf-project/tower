//! `SessionPool` — one resident LSP session per configured language and discovered
//! project root, lazy-spawned and crash-restartable (spec 14d).
//!
//! # Lifecycle
//!
//! - **Lazy spawn**: the first request for a language/project-root pair calls
//!   `spawner.spawn()`. Subsequent requests reuse the `Arc<dyn PooledSession>`.
//! - **Crash restart**: on every request, the pool checks `session.is_dead()`.
//!   If true, the entry is removed and a fresh session is spawned (UN1).
//! - **Idle shutdown**: when `LspConfig.idle_timeout` is set, `get_or_spawn`
//!   checks `last_used` on the entry. Idle-expired sessions are evicted and
//!   re-spawned on the next request. No background thread (YAGNI).
//!
//! # Lock ordering
//!
//! ```text
//! sessions  Mutex<HashMap>  (tier-0) — acquired and dropped BEFORE any
//!   PooledSession method  (tier-1+). Never held while doing I/O (spawn/write/wait).
//! lang_index and ext_index are immutable after construction; no lock needed.
//! ```
//!
//! # Watcher starvation prevention
//!
//! `DocumentSyncPort::serves()` reads only the immutable `ext_index` — O(1), no
//! lock. The pool lock is never held while doing session I/O.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::lsp_adapter::discovery::{discover_workspace_root, manifest_names_for_language};
use crate::lsp_adapter::{DiagnosticsSender, LspSessionError, RawWorkspaceEdit, SharedState};
use core_engine::adapters::config::lsp::{
    LspCommandError, LspConfig, LspServerConfig, resolve_lsp_command,
};
use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::{Diagnostic, Position};
use core_engine::ports::{
    CodeIntelError, CodeIntelligencePort, DocumentSyncPort, NavigationPort, PrepareRenameResult,
    RenameNavigationError,
};

// ── PooledSession trait ───────────────────────────────────────────────────────

/// A managed LSP session held by the pool.
///
/// Supertraits `CodeIntelligencePort + NavigationPort + DocumentSyncPort` let
/// `SessionPool` delegate all port calls to the underlying session via a single
/// `Arc<dyn PooledSession>`. The three lifecycle/read methods below are the pool's
/// own interface: `is_dead` for crash detection, `shared_state` for push wiring
/// (Task 6), and `diagnostics_for` for `resources/read` pull (AC6).
///
/// Object-safe: confirmed — 14c already uses `Arc<dyn CodeIntelligencePort>` in
/// `main.rs`, so adding three non-generic methods keeps the vtable valid.
pub trait PooledSession:
    CodeIntelligencePort + NavigationPort + DocumentSyncPort + Send + Sync
{
    /// True if the reader thread has set EOF (server died or exited).
    /// The pool checks this on every request to detect crashes.
    fn is_dead(&self) -> bool;

    /// Clone the shared diagnostic state `Arc` for push-wiring (Task 6).
    /// Called once at pool construction time; not on the hot path.
    #[allow(dead_code)]
    fn shared_state(&self) -> SharedState;

    /// Return the last published diagnostics for `uri` from `SharedState::by_uri`,
    /// without re-running `check`. Returns `[]` when the URI has never published.
    /// Called by `SessionPool::diagnostics_for` to serve `resources/read` (AC6 pull).
    #[allow(dead_code)]
    fn diagnostics_for(&self, uri: &str) -> Vec<Diagnostic>;

    fn check_lsp(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<Diagnostic>, LspSessionError> {
        self.check(path, text)
            .map_err(code_intel_to_lsp_session_error)
    }

    fn definition_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, LspSessionError> {
        self.definition(path, text, position)
            .map_err(code_intel_to_lsp_session_error)
    }

    fn references_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, LspSessionError> {
        self.references(path, text, position)
            .map_err(code_intel_to_lsp_session_error)
    }

    fn implementations_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, LspSessionError> {
        self.implementations(path, text, position)
            .map_err(code_intel_to_lsp_session_error)
    }

    fn hover_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Option<core_engine::domain::code_intel::Hover>, LspSessionError> {
        self.hover(path, text, position)
            .map_err(code_intel_to_lsp_session_error)
    }

    fn prepare_rename_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<PrepareRenameResult, LspSessionError> {
        self.prepare_rename(path, text, position)
            .map_err(rename_to_lsp_session_error)
    }

    fn rename_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, LspSessionError> {
        self.rename(path, text, position, new_name)
            .map_err(rename_to_lsp_session_error)
    }

    #[allow(dead_code)]
    fn rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, RenameNavigationError>;

    /// Test/`testing`-only: force this session into the "dead" state.
    ///
    /// `LspClientAdapter` kills its real OS process so the reader thread observes
    /// stdout EOF and sets `dead_flag` through the production detection path — the
    /// same sequence an external crash triggers. `FakeSession` flips its in-memory
    /// flag. No default body: every session decides how it dies.
    #[allow(dead_code)]
    #[cfg(any(test, feature = "testing"))]
    fn kill_for_test(&self);
}

// ── SessionSpawner trait ──────────────────────────────────────────────────────

/// Factory that the pool calls to create a new session. Production uses
/// `RealSpawner`; tests inject `FakeSpawner` via `SessionPool::with_spawner`.
pub trait SessionSpawner: Send + Sync {
    /// Spawn (or construct) a session for the given language config, rooted at
    /// `root`, optionally wired to `push_tx` for push delivery.
    fn spawn(
        &self,
        cfg: &LspServerConfig,
        language_id: &str,
        root: PathBuf,
        workspace_root: &std::path::Path,
        push_tx: Option<DiagnosticsSender>,
    ) -> Result<Arc<dyn PooledSession>, LspSessionError>;
}

// ── RealSpawner ───────────────────────────────────────────────────────────────

/// Production spawner: calls `LspClientAdapter::spawn` and boxes it as
/// `Arc<dyn PooledSession>`.
pub struct RealSpawner;

impl SessionSpawner for RealSpawner {
    fn spawn(
        &self,
        cfg: &LspServerConfig,
        language_id: &str,
        root: PathBuf,
        workspace_root: &std::path::Path,
        push_tx: Option<DiagnosticsSender>,
    ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
        use crate::lsp_adapter::LspClientAdapter;

        let resolved = resolve_lsp_command(
            &cfg.command,
            workspace_root,
            std::env::var_os("PATH").as_deref(),
        )
        .map_err(lsp_command_to_session_error)?;
        let adapter = LspClientAdapter::spawn_resolved(
            &resolved.executable,
            &cfg.args,
            cfg.extensions.clone(),
            language_id.to_owned(),
            root,
            push_tx,
        )?;
        Ok(Arc::new(adapter))
    }
}

// (DiagnosticsReader / NoOpDiagnosticsReader now live in
//  core_engine::adapters::mcp::diagnostics — the hand-rolled `transport` module
//  was removed in the rmcp migration.)

// ── PoolEntry ─────────────────────────────────────────────────────────────────

struct PoolEntry {
    session: Arc<dyn PooledSession>,
    last_used: Instant,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionKey {
    language: String,
    root: PathBuf,
}

// ── SessionPool ───────────────────────────────────────────────────────────────

/// Manages one resident LSP session per configured language and project root.
///
/// Constructed with `new` (production) or `with_spawner` (test seam).
pub struct SessionPool {
    /// Language and discovered project root → live session entry.
    sessions: Mutex<HashMap<SessionKey, PoolEntry>>,
    spawner: Arc<dyn SessionSpawner>,
    /// language name → `LspServerConfig` (immutable after construction).
    lang_index: HashMap<String, LspServerConfig>,
    /// file extension → language name (immutable after construction; O(1) lookup).
    ext_index: HashMap<String, String>,
    workspace_root: PathBuf,
    idle_timeout: Option<Duration>,
    push_tx: Option<DiagnosticsSender>,
}

impl SessionPool {
    /// Production constructor: uses `RealSpawner` and no push channel.
    #[allow(dead_code)]
    #[must_use]
    pub fn new(config: LspConfig, workspace_root: PathBuf) -> Self {
        Self::with_spawner(config, workspace_root, Arc::new(RealSpawner), None)
    }

    /// Test seam constructor: inject `spawner` and optionally a `push_tx`.
    #[must_use]
    pub fn with_spawner(
        config: LspConfig,
        workspace_root: PathBuf,
        spawner: Arc<dyn SessionSpawner>,
        push_tx: Option<DiagnosticsSender>,
    ) -> Self {
        let mut lang_index = HashMap::new();
        let mut ext_index = HashMap::new();
        for (lang, cfg) in &config.servers {
            for ext in &cfg.extensions {
                ext_index.insert(ext.clone(), lang.clone());
            }
            lang_index.insert(lang.clone(), cfg.clone());
        }
        Self {
            sessions: Mutex::new(HashMap::new()),
            spawner,
            lang_index,
            ext_index,
            workspace_root,
            idle_timeout: config.idle_timeout,
            push_tx,
        }
    }

    /// Resolve the language name for a relative path from its extension.
    /// O(1), no lock — reads the immutable `ext_index`.
    fn lang_for_path(&self, path: &RelativePath) -> Option<&str> {
        let ext = path.as_str().rsplit_once('.')?.1;
        self.ext_index.get(ext).map(String::as_str)
    }

    /// Get the live session for `lang`, spawning one if absent, dead, or idle.
    ///
    /// Lock discipline: the pool lock is acquired to look up or remove the entry,
    /// then DROPPED before calling `spawner.spawn()` (which may block on I/O),
    /// then re-acquired to insert the new session. A concurrent "loser" that also
    /// spawned will have its Arc dropped here → `Drop::drop` kills the child.
    fn get_or_spawn(
        &self,
        lang: &str,
        path: &RelativePath,
    ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
        let absolute_path = self.workspace_root.join(path.as_str());
        let file_dir = absolute_path.parent().unwrap_or(&self.workspace_root);
        let root = discover_workspace_root(
            file_dir,
            &self.workspace_root,
            manifest_names_for_language(lang),
        );
        let key = SessionKey {
            language: lang.to_owned(),
            root: root.clone(),
        };

        // Phase 1: look up under lock, evict if dead/idle.
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = sessions.get(&key) {
                let dead = entry.session.is_dead();
                let idle = self
                    .idle_timeout
                    .is_some_and(|t| entry.last_used.elapsed() > t);
                if !dead && !idle {
                    // Fast path: reuse the live session.
                    let session = Arc::clone(&entry.session);
                    // Update last_used while we still hold the lock.
                    sessions.get_mut(&key).unwrap().last_used = Instant::now();
                    return Ok(session);
                }
                // Evict dead or idle entry.
                sessions.remove(&key);
            }
        }
        // Phase 2: spawn outside the lock (may block on process I/O).
        let cfg = self
            .lang_index
            .get(lang)
            .ok_or(LspSessionError::Unconfigured)?;
        let session =
            self.spawner
                .spawn(cfg, lang, root, &self.workspace_root, self.push_tx.clone())?;

        // Phase 3: re-acquire to insert; discard concurrent loser.
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let (selected, loser) = match sessions.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().last_used = Instant::now();
                (Arc::clone(&entry.get().session), Some(session))
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(PoolEntry {
                    session: Arc::clone(&session),
                    last_used: Instant::now(),
                });
                (session, None)
            }
        };
        drop(sessions);
        drop(loser);
        Ok(selected)
    }

    /// Return the last published diagnostics for the given LSP URI.
    /// Routes the URI's extension to the (already-spawned) session via
    /// `session.diagnostics_for(uri)`. Returns `[]` when no live session or URI.
    #[allow(dead_code)]
    pub fn diagnostics_for_uri(&self, uri: &str) -> Vec<Diagnostic> {
        // Extract extension from URI path (e.g. "file:///w/src/main.rs" → "rs").
        let ext = uri.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let lang = match self.ext_index.get(ext) {
            Some(l) => l.as_str(),
            None => return vec![],
        };
        let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions
            .iter()
            .filter(|(key, entry)| key.language == lang && !entry.session.is_dead())
            .find_map(|(_, entry)| {
                let diagnostics = entry.session.diagnostics_for(uri);
                (!diagnostics.is_empty()).then_some(diagnostics)
            })
            .unwrap_or_default()
    }

    /// Whether the pool is configured to handle files with this path's extension.
    /// O(1), no lock — reads the immutable `ext_index`.
    #[must_use]
    pub fn serves(&self, path: &RelativePath) -> bool {
        self.lang_for_path(path).is_some()
    }

    #[must_use]
    pub fn binding_for(&self, path: &RelativePath) -> Option<(&str, &str)> {
        let language = self.lang_for_path(path)?;
        let command = self.lang_index.get(language)?.command.as_str();
        Some((language, command))
    }

    /// Return the static resource URI list: one `lsp://<lang>/diagnostics` entry
    /// per configured language. Called at startup by `main.rs` and passed to
    /// `serve_with_push` as the `resource_uris` list for `resources/list`.
    /// No pool lock — reads the immutable `lang_index`.
    #[allow(dead_code)]
    #[must_use]
    pub fn resource_uris(&self) -> Vec<String> {
        self.lang_index
            .keys()
            .map(|lang| format!("lsp://{lang}/diagnostics"))
            .collect()
    }

    /// Test-only: backdate `last_used` for `lang` by `by` so idle eviction fires
    /// on the next request without requiring a real `sleep`.
    #[cfg(test)]
    pub fn backdate_last_used_for_test(&self, lang: &str, by: Duration) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, entry)) = sessions.iter_mut().find(|(key, _)| key.language == lang) {
            entry.last_used -= by;
        }
    }

    /// Test-only: simulate a crash for the session serving `lang` by calling
    /// `kill_for_test()` on it. The pool's next `get_or_spawn` for that language
    /// will detect `is_dead() == true` and re-spawn a fresh session (AC4).
    ///
    /// Returns `true` when the session was found and signalled; `false` when no
    /// live session exists for `lang` (e.g. first request has not happened yet).
    #[allow(dead_code)]
    #[cfg(any(test, feature = "testing"))]
    pub fn kill_session_for_test(&self, lang: &str) -> bool {
        // Clone the Arc and drop the pool lock before `kill_for_test`: the real
        // session's `kill_for_test` locks `inner`, so we must not hold the pool
        // (tier-0) lock across it (lock-ordering discipline).
        let session = {
            let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            sessions
                .iter()
                .find(|(key, _)| key.language == lang)
                .map(|(_, entry)| Arc::clone(&entry.session))
        };
        match session {
            Some(session) => {
                session.kill_for_test();
                true
            }
            None => false,
        }
    }

    /// Test/`testing`-only: whether the session for `lang` is currently detected
    /// dead. The AC4 e2e polls this after a real process kill to confirm the
    /// reader-thread EOF → `dead_flag` detection chain fired, before asserting
    /// the pool re-spawns. `None` when no session exists for `lang`.
    #[allow(dead_code)]
    #[cfg(any(test, feature = "testing"))]
    pub fn session_is_dead_for_test(&self, lang: &str) -> Option<bool> {
        let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions
            .iter()
            .find(|(key, _)| key.language == lang)
            .map(|(_, entry)| entry.session.is_dead())
    }
}

// ── Port impls on SessionPool ─────────────────────────────────────────────────
//
// Each port impl routes by extension → language → get_or_spawn → session method.
// The pool lock is never held while doing session I/O.

impl CodeIntelligencePort for SessionPool {
    fn check(&self, path: &RelativePath, text: &str) -> Result<Vec<Diagnostic>, CodeIntelError> {
        self.check_lsp(path, text)
            .map_err(lsp_session_to_code_intel_error)
    }
}

impl NavigationPort for SessionPool {
    fn definition(
        &self,
        path: &RelativePath,
        text: &str,
        position: core_engine::domain::code_intel::Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, CodeIntelError> {
        self.definition_lsp(path, text, position)
            .map_err(lsp_session_to_code_intel_error)
    }

    fn implementations(
        &self,
        path: &RelativePath,
        text: &str,
        position: core_engine::domain::code_intel::Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, CodeIntelError> {
        self.implementations_lsp(path, text, position)
            .map_err(lsp_session_to_code_intel_error)
    }

    fn references(
        &self,
        path: &RelativePath,
        text: &str,
        position: core_engine::domain::code_intel::Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, CodeIntelError> {
        self.references_lsp(path, text, position)
            .map_err(lsp_session_to_code_intel_error)
    }

    fn hover(
        &self,
        path: &RelativePath,
        text: &str,
        position: core_engine::domain::code_intel::Position,
    ) -> Result<Option<core_engine::domain::code_intel::Hover>, CodeIntelError> {
        self.hover_lsp(path, text, position)
            .map_err(lsp_session_to_code_intel_error)
    }

    fn document_symbols(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<core_engine::domain::code_intel::Symbol>, CodeIntelError> {
        let lang = self
            .lang_for_path(path)
            .ok_or(CodeIntelError::Unsupported)?
            .to_owned();
        let session = self
            .get_or_spawn(&lang, path)
            .map_err(lsp_session_to_code_intel_error)?;
        session.document_symbols(path, text)
    }

    fn prepare_rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: core_engine::domain::code_intel::Position,
    ) -> Result<PrepareRenameResult, RenameNavigationError> {
        self.prepare_rename_lsp(path, text, position)
            .map_err(lsp_session_to_rename_error)
    }
}

impl SessionPool {
    pub fn check_lsp(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<Diagnostic>, LspSessionError> {
        self.session_for_lsp(path)?.check_lsp(path, text)
    }

    pub fn definition_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, LspSessionError> {
        self.session_for_lsp(path)?
            .definition_lsp(path, text, position)
    }

    pub fn references_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, LspSessionError> {
        self.session_for_lsp(path)?
            .references_lsp(path, text, position)
    }

    pub fn implementations_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<core_engine::domain::code_intel::Location>, LspSessionError> {
        self.session_for_lsp(path)?
            .implementations_lsp(path, text, position)
    }

    pub fn hover_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Option<core_engine::domain::code_intel::Hover>, LspSessionError> {
        self.session_for_lsp(path)?.hover_lsp(path, text, position)
    }

    pub fn prepare_rename_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<PrepareRenameResult, LspSessionError> {
        self.session_for_lsp(path)?
            .prepare_rename_lsp(path, text, position)
    }

    pub fn rename_lsp(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, LspSessionError> {
        self.session_for_lsp(path)?
            .rename_lsp(path, text, position, new_name)
    }

    fn session_for_lsp(
        &self,
        path: &RelativePath,
    ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
        let lang = self
            .lang_for_path(path)
            .ok_or(LspSessionError::Unconfigured)?
            .to_owned();
        self.get_or_spawn(&lang, path)
    }

    #[allow(dead_code)]
    pub fn rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, RenameNavigationError> {
        self.rename_lsp(path, text, position, new_name)
            .map_err(lsp_session_to_rename_error)
    }
}

fn lsp_session_to_rename_error(error: LspSessionError) -> RenameNavigationError {
    match error {
        LspSessionError::Unconfigured | LspSessionError::CapabilityUnavailable => {
            RenameNavigationError::UnsupportedLanguage
        }
        LspSessionError::NotRenameable => RenameNavigationError::NotRenameable,
        error @ LspSessionError::Backend(_) => RenameNavigationError::Backend(error.to_string()),
    }
}

fn lsp_session_to_code_intel_error(error: LspSessionError) -> CodeIntelError {
    match error {
        LspSessionError::Unconfigured | LspSessionError::CapabilityUnavailable => {
            CodeIntelError::Unsupported
        }
        error => CodeIntelError::Backend(error.to_string()),
    }
}

fn code_intel_to_lsp_session_error(error: CodeIntelError) -> LspSessionError {
    match error {
        CodeIntelError::Unsupported => LspSessionError::CapabilityUnavailable,
        CodeIntelError::Backend(_) => {
            LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerError)
        }
    }
}

fn rename_to_lsp_session_error(error: RenameNavigationError) -> LspSessionError {
    match error {
        RenameNavigationError::UnsupportedLanguage => LspSessionError::CapabilityUnavailable,
        RenameNavigationError::NotRenameable => LspSessionError::NotRenameable,
        RenameNavigationError::Backend(_) => {
            LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerError)
        }
    }
}

fn lsp_command_to_session_error(error: LspCommandError) -> LspSessionError {
    use extension_protocol::LspOutcomeCode;

    let code = match error {
        LspCommandError::InvalidCommand => LspOutcomeCode::InvalidCommand,
        LspCommandError::ServerMissing => LspOutcomeCode::ServerMissing,
        LspCommandError::ServerNotExecutable => LspOutcomeCode::ServerNotExecutable,
        LspCommandError::ServerLaunchFailed => LspOutcomeCode::ServerLaunchFailed,
    };
    LspSessionError::Backend(code)
}

impl DocumentSyncPort for SessionPool {
    /// O(1), no lock — reads the immutable `ext_index`.
    fn serves(&self, path: &RelativePath) -> bool {
        self.lang_for_path(path).is_some()
    }

    fn document_opened(&self, path: &RelativePath, text: &str) {
        let Some(lang) = self.lang_for_path(path).map(str::to_owned) else {
            return;
        };
        if let Ok(session) = self.get_or_spawn(&lang, path) {
            session.document_opened(path, text);
        }
    }

    fn document_changed(&self, path: &RelativePath, text: &str) {
        let Some(lang) = self.lang_for_path(path).map(str::to_owned) else {
            return;
        };
        if let Ok(session) = self.get_or_spawn(&lang, path) {
            session.document_changed(path, text);
        }
    }

    fn document_closed(&self, path: &RelativePath) {
        let Some(lang) = self.lang_for_path(path).map(str::to_owned) else {
            return;
        };
        if let Ok(session) = self.get_or_spawn(&lang, path) {
            session.document_closed(path);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use core_engine::adapters::config::lsp::{LspConfig, LspServerConfig};
    use core_engine::domain::RelativePath;
    use core_engine::ports::CodeIntelError;
    use std::collections::{BTreeMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, mpsc};
    use std::time::Duration;

    // ── FakeSession ───────────────────────────────────────────────────────────

    struct FakeSession {
        dead: AtomicBool,
        check_calls: AtomicUsize,
        implementation_calls: AtomicUsize,
        prepare_rename_calls: AtomicUsize,
        rename_calls: AtomicUsize,
        canned_diags: Vec<Diagnostic>,
        canned_implementations: Mutex<Vec<core_engine::domain::code_intel::Location>>,
        canned_prepare_rename: Mutex<Result<PrepareRenameResult, RenameNavigationError>>,
        canned_rename: Mutex<Result<RawWorkspaceEdit, RenameNavigationError>>,
    }

    impl FakeSession {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                dead: AtomicBool::new(false),
                check_calls: AtomicUsize::new(0),
                implementation_calls: AtomicUsize::new(0),
                prepare_rename_calls: AtomicUsize::new(0),
                rename_calls: AtomicUsize::new(0),
                canned_diags: vec![],
                canned_implementations: Mutex::new(vec![
                    core_engine::domain::code_intel::Location {
                        path: RelativePath::new("src/impl.rs"),
                        range: core_engine::domain::code_intel::Range {
                            start: Position {
                                line: 3,
                                character: 1,
                            },
                            end: Position {
                                line: 3,
                                character: 5,
                            },
                        },
                    },
                ]),
                canned_prepare_rename: Mutex::new(Ok(PrepareRenameResult {
                    range: Some(core_engine::domain::code_intel::Range {
                        start: Position {
                            line: 0,
                            character: 4,
                        },
                        end: Position {
                            line: 0,
                            character: 8,
                        },
                    }),
                    placeholder: Some("name".to_owned()),
                })),
                canned_rename: Mutex::new(Ok(RawWorkspaceEdit {
                    changes: Some(std::collections::HashMap::new()),
                    document_changes: None,
                    change_annotations: None,
                })),
            })
        }

        fn set_dead(&self) {
            self.dead.store(true, Ordering::Relaxed);
        }

        fn calls(&self) -> usize {
            self.check_calls.load(Ordering::Relaxed)
        }

        fn implementation_calls(&self) -> usize {
            self.implementation_calls.load(Ordering::Relaxed)
        }

        fn prepare_rename_calls(&self) -> usize {
            self.prepare_rename_calls.load(Ordering::Relaxed)
        }

        fn rename_calls(&self) -> usize {
            self.rename_calls.load(Ordering::Relaxed)
        }
    }

    impl PooledSession for FakeSession {
        fn is_dead(&self) -> bool {
            self.dead.load(Ordering::Relaxed)
        }

        fn shared_state(&self) -> SharedState {
            Arc::new((
                Mutex::new(crate::lsp_adapter::SessionStateInner::default()),
                std::sync::Condvar::new(),
            ))
        }

        fn diagnostics_for(&self, _uri: &str) -> Vec<Diagnostic> {
            self.canned_diags.clone()
        }

        fn rename(
            &self,
            _: &RelativePath,
            _: &str,
            _: Position,
            _: &str,
        ) -> Result<RawWorkspaceEdit, RenameNavigationError> {
            self.rename_calls.fetch_add(1, Ordering::Relaxed);
            self.canned_rename.lock().unwrap().clone()
        }

        fn kill_for_test(&self) {
            self.set_dead();
        }
    }

    impl CodeIntelligencePort for FakeSession {
        fn check(
            &self,
            _path: &RelativePath,
            _text: &str,
        ) -> Result<Vec<Diagnostic>, CodeIntelError> {
            self.check_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.canned_diags.clone())
        }
    }

    impl NavigationPort for FakeSession {
        fn definition(
            &self,
            _: &RelativePath,
            _: &str,
            _: core_engine::domain::code_intel::Position,
        ) -> Result<Vec<core_engine::domain::code_intel::Location>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }

        fn implementations(
            &self,
            _: &RelativePath,
            _: &str,
            _: core_engine::domain::code_intel::Position,
        ) -> Result<Vec<core_engine::domain::code_intel::Location>, CodeIntelError> {
            self.implementation_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.canned_implementations.lock().unwrap().clone())
        }

        fn references(
            &self,
            _: &RelativePath,
            _: &str,
            _: core_engine::domain::code_intel::Position,
        ) -> Result<Vec<core_engine::domain::code_intel::Location>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }

        fn hover(
            &self,
            _: &RelativePath,
            _: &str,
            _: core_engine::domain::code_intel::Position,
        ) -> Result<Option<core_engine::domain::code_intel::Hover>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }

        fn document_symbols(
            &self,
            _: &RelativePath,
            _: &str,
        ) -> Result<Vec<core_engine::domain::code_intel::Symbol>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }

        fn prepare_rename(
            &self,
            _: &RelativePath,
            _: &str,
            _: core_engine::domain::code_intel::Position,
        ) -> Result<PrepareRenameResult, RenameNavigationError> {
            self.prepare_rename_calls.fetch_add(1, Ordering::Relaxed);
            self.canned_prepare_rename.lock().unwrap().clone()
        }
    }

    impl DocumentSyncPort for FakeSession {
        fn serves(&self, _: &RelativePath) -> bool {
            true
        }

        fn document_opened(&self, _: &RelativePath, _: &str) {}
        fn document_changed(&self, _: &RelativePath, _: &str) {}
        fn document_closed(&self, _: &RelativePath) {}
    }

    // ── FakeSpawner ───────────────────────────────────────────────────────────

    type SessionQueue = (VecDeque<Arc<FakeSession>>, usize);

    struct FakeSpawner {
        /// language → (sessions returned in registration order, spawn_count)
        per_lang: Mutex<HashMap<String, SessionQueue>>,
        failures: Mutex<HashMap<String, VecDeque<LspSessionError>>>,
        spawn_barrier: Mutex<Option<Arc<Barrier>>>,
    }

    impl FakeSpawner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                per_lang: Mutex::new(HashMap::new()),
                failures: Mutex::new(HashMap::new()),
                spawn_barrier: Mutex::new(None),
            })
        }

        fn register(&self, lang: &str, session: Arc<FakeSession>) {
            self.per_lang
                .lock()
                .unwrap()
                .insert(lang.to_owned(), (VecDeque::from([session]), 0));
        }

        fn enqueue(&self, lang: &str, session: Arc<FakeSession>) {
            self.per_lang
                .lock()
                .unwrap()
                .get_mut(lang)
                .expect("language must be registered before enqueueing a session")
                .0
                .push_back(session);
        }

        fn spawn_count(&self, lang: &str) -> usize {
            self.per_lang
                .lock()
                .unwrap()
                .get(lang)
                .map(|(_, c)| *c)
                .unwrap_or(0)
        }

        fn fail_next(&self, lang: &str, error: LspSessionError) {
            self.failures
                .lock()
                .unwrap()
                .entry(lang.to_owned())
                .or_default()
                .push_back(error);
        }

        fn synchronize_next_spawns(&self, participants: usize) {
            *self.spawn_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(participants)));
        }
    }

    impl SessionSpawner for FakeSpawner {
        fn spawn(
            &self,
            _cfg: &LspServerConfig,
            language_id: &str,
            _root: PathBuf,
            _workspace_root: &std::path::Path,
            _push_tx: Option<DiagnosticsSender>,
        ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
            let session = match self.per_lang.lock().unwrap().get_mut(language_id) {
                Some((sessions, count)) => {
                    let session = sessions
                        .get(*count)
                        .or_else(|| sessions.back())
                        .expect("registered language must have a fake session");
                    *count += 1;
                    Arc::clone(session)
                }
                None => {
                    return Err(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerError,
                    ));
                }
            };
            let failure = self
                .failures
                .lock()
                .unwrap()
                .get_mut(language_id)
                .and_then(VecDeque::pop_front);
            let barrier = self.spawn_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.wait();
            }
            failure.map_or_else(|| Ok(session as Arc<dyn PooledSession>), Err)
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn rust_config() -> LspConfig {
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: "rust-analyzer".to_owned(),
                extensions: vec!["rs".to_owned()],
                args: vec![],
            },
        );
        LspConfig {
            servers,
            idle_timeout: None,
        }
    }

    fn multi_lang_config() -> LspConfig {
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: "rust-analyzer".to_owned(),
                extensions: vec!["rs".to_owned()],
                args: vec![],
            },
        );
        servers.insert(
            "go".to_owned(),
            LspServerConfig {
                command: "gopls".to_owned(),
                extensions: vec!["go".to_owned()],
                args: vec![],
            },
        );
        LspConfig {
            servers,
            idle_timeout: None,
        }
    }

    fn pool_with_spawner(config: LspConfig, spawner: Arc<dyn SessionSpawner>) -> SessionPool {
        SessionPool::with_spawner(config, PathBuf::from("/workspace"), spawner, None)
    }

    fn f008_lifecycle_config() -> LspConfig {
        let mut servers = BTreeMap::new();
        for (language, command, extensions) in [
            ("go", "gopls", vec!["go"]),
            ("php", "intelephense", vec!["php", "shared"]),
            ("python", "pyright-langserver", vec!["py"]),
            ("rust", "rust-analyzer", vec!["rs"]),
            (
                "typescript",
                "typescript-language-server",
                vec!["ts", "shared"],
            ),
        ] {
            servers.insert(
                language.to_owned(),
                LspServerConfig {
                    command: command.to_owned(),
                    extensions: extensions.into_iter().map(str::to_owned).collect(),
                    args: vec![],
                },
            );
        }
        LspConfig {
            servers,
            idle_timeout: None,
        }
    }

    #[test]
    fn f008_lifecycle_routes_five_languages_and_uses_lexicographically_last_duplicate_mapping() {
        let spawner = FakeSpawner::new();
        let sessions: HashMap<_, _> = ["rust", "go", "php", "typescript", "python"]
            .into_iter()
            .map(|language| (language, FakeSession::new()))
            .collect();
        for (language, session) in &sessions {
            spawner.register(language, Arc::clone(session));
        }
        let pool = pool_with_spawner(
            f008_lifecycle_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );

        for (path, expected_language) in [
            ("src/main.rs", "rust"),
            ("main.go", "go"),
            ("index.php", "php"),
            ("index.ts", "typescript"),
            ("app.py", "python"),
            ("duplicate.shared", "typescript"),
        ] {
            let calls_before = sessions[expected_language].calls();
            pool.check_lsp(&RelativePath::new(path), "")
                .expect("configured language must route to its fake session");
            assert_eq!(sessions[expected_language].calls(), calls_before + 1);
        }

        for language in ["rust", "go", "php", "typescript", "python"] {
            assert_eq!(spawner.spawn_count(language), 1, "{language} spawn count");
        }
        assert_eq!(sessions["php"].calls(), 1);
        assert_eq!(sessions["typescript"].calls(), 2);
    }

    #[test]
    fn f008_lifecycle_recovers_after_a_missing_command_without_caching_the_failure() {
        use extension_protocol::LspOutcomeCode;

        let spawner = FakeSpawner::new();
        let session = FakeSession::new();
        spawner.register("rust", Arc::clone(&session));
        spawner.fail_next(
            "rust",
            LspSessionError::Backend(LspOutcomeCode::ServerMissing),
        );
        let pool = pool_with_spawner(
            rust_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );
        let path = RelativePath::new("src/main.rs");

        assert_eq!(
            pool.check_lsp(&path, "").unwrap_err(),
            LspSessionError::Backend(LspOutcomeCode::ServerMissing)
        );
        pool.check_lsp(&path, "")
            .expect("a restored command must be retried successfully");

        assert_eq!(spawner.spawn_count("rust"), 2);
        assert_eq!(session.calls(), 1);
    }

    #[test]
    fn f008_lifecycle_restarts_a_dead_session_without_disturbing_a_healthy_language() {
        let spawner = FakeSpawner::new();
        let dead_rust = FakeSession::new();
        let replacement_rust = FakeSession::new();
        let go = FakeSession::new();
        spawner.register("rust", Arc::clone(&dead_rust));
        spawner.enqueue("rust", Arc::clone(&replacement_rust));
        spawner.register("go", Arc::clone(&go));
        let pool = pool_with_spawner(
            multi_lang_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );

        pool.check_lsp(&RelativePath::new("src/main.rs"), "")
            .unwrap();
        pool.check_lsp(&RelativePath::new("main.go"), "").unwrap();
        dead_rust.set_dead();
        pool.check_lsp(&RelativePath::new("src/main.rs"), "")
            .expect("dead Rust session must be restarted");
        pool.check_lsp(&RelativePath::new("src/main.rs"), "")
            .expect("healthy replacement session must be reused");
        pool.check_lsp(&RelativePath::new("main.go"), "")
            .expect("healthy Go session must remain usable");

        assert_eq!(spawner.spawn_count("rust"), 2);
        assert_eq!(spawner.spawn_count("go"), 1);
        assert_eq!(dead_rust.calls(), 1);
        assert_eq!(replacement_rust.calls(), 2);
        assert_eq!(go.calls(), 2);
    }

    #[test]
    fn f008_lifecycle_classifies_concurrent_equivalent_failures_identically() {
        use extension_protocol::LspOutcomeCode;

        let spawner = FakeSpawner::new();
        spawner.register("rust", FakeSession::new());
        for _ in 0..2 {
            spawner.fail_next(
                "rust",
                LspSessionError::Backend(LspOutcomeCode::ServerTransportError),
            );
        }
        spawner.synchronize_next_spawns(2);
        let pool = Arc::new(pool_with_spawner(
            rust_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        ));

        let requests: Vec<_> = (0..2)
            .map(|_| {
                let pool = Arc::clone(&pool);
                std::thread::spawn(move || pool.check_lsp(&RelativePath::new("src/main.rs"), ""))
            })
            .collect();
        let results: Vec<_> = requests
            .into_iter()
            .map(|request| request.join().expect("request thread must not panic"))
            .collect();

        assert_eq!(
            results,
            vec![
                Err(LspSessionError::Backend(
                    LspOutcomeCode::ServerTransportError
                )),
                Err(LspSessionError::Backend(
                    LspOutcomeCode::ServerTransportError
                )),
            ]
        );
        assert_eq!(spawner.spawn_count("rust"), 2);
    }

    #[cfg(unix)]
    #[test]
    fn f008_spawn_real_spawner_resolves_from_workspace_and_preserves_ordered_args() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().expect("temporary workspace");
        let project_root = workspace.path().join("crates/app");
        std::fs::create_dir_all(project_root.join("src")).expect("create project source directory");
        std::fs::write(project_root.join("Cargo.toml"), "").expect("write project manifest");

        let executable = workspace.path().join("server with spaces.sh");
        let args_file = workspace.path().join("received-args");
        let cwd_file = workspace.path().join("received-cwd");
        let initialize = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\npwd > '{}'\nprintf 'Content-Length: %s\\r\\n\\r\\n%s' '{}' '{}'\nexec sleep 60\n",
            args_file.display(),
            cwd_file.display(),
            initialize.len(),
            initialize,
        );
        std::fs::write(&executable, script).expect("write fake language server");
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let mut config = rust_config();
        let server = config.servers.get_mut("rust").unwrap();
        server.command = "./server with spaces.sh".to_owned();
        server.args = vec!["first argument".to_owned(), "--second=value".to_owned()];
        let pool = SessionPool::new(config, workspace.path().to_path_buf());

        pool.get_or_spawn("rust", &RelativePath::new("crates/app/src/main.rs"))
            .expect("resolved language server should spawn");

        assert_eq!(
            std::fs::read_to_string(args_file).unwrap(),
            "first argument\n--second=value\n"
        );
        assert_eq!(
            PathBuf::from(std::fs::read_to_string(cwd_file).unwrap().trim()),
            project_root
        );
    }

    #[cfg(unix)]
    #[test]
    fn f008_spawn_maps_resolution_and_actual_launch_failures_to_outcome_codes() {
        use extension_protocol::LspOutcomeCode;
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().expect("temporary workspace");
        let invalid_format = workspace.path().join("invalid-format");
        std::fs::write(&invalid_format, "not an executable format").unwrap();
        let mut permissions = std::fs::metadata(&invalid_format).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&invalid_format, permissions).unwrap();

        let mut config = rust_config();
        config.servers.get_mut("rust").unwrap().command = " ".to_owned();
        let error = SessionPool::new(config, workspace.path().to_path_buf())
            .check_lsp(&RelativePath::new("src/main.rs"), "")
            .unwrap_err();
        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::InvalidCommand)
        );

        let mut config = rust_config();
        config.servers.get_mut("rust").unwrap().command = "missing-server".to_owned();
        let error = SessionPool::new(config, workspace.path().to_path_buf())
            .check_lsp(&RelativePath::new("src/main.rs"), "")
            .unwrap_err();
        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::ServerMissing)
        );

        let mut config = rust_config();
        config.servers.get_mut("rust").unwrap().command = "./invalid-format".to_owned();
        let error = SessionPool::new(config, workspace.path().to_path_buf())
            .check_lsp(&RelativePath::new("src/main.rs"), "")
            .unwrap_err();
        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::ServerNotExecutable)
        );

        let launch_config = LspServerConfig {
            command: "/bin/true".to_owned(),
            extensions: vec!["rs".to_owned()],
            args: vec![],
        };
        let error = match RealSpawner.spawn(
            &launch_config,
            "rust",
            workspace.path().join("missing-working-directory"),
            workspace.path(),
            None,
        ) {
            Ok(_) => panic!("a missing working directory must fail process launch"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            LspSessionError::Backend(LspOutcomeCode::ServerLaunchFailed)
        );
    }

    #[test]
    fn f008_spawn_releases_pool_lock_before_blocking_spawn_io() {
        struct BlockingSpawner {
            started: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            session: Arc<FakeSession>,
        }

        impl SessionSpawner for BlockingSpawner {
            fn spawn(
                &self,
                _: &LspServerConfig,
                _: &str,
                _: PathBuf,
                _: &std::path::Path,
                _: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
                self.started.send(()).expect("report that spawn started");
                self.release
                    .lock()
                    .unwrap()
                    .recv()
                    .expect("allow blocked spawn to finish");
                Ok(Arc::clone(&self.session) as Arc<dyn PooledSession>)
            }
        }

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let spawner = Arc::new(BlockingSpawner {
            started: started_tx,
            release: Mutex::new(release_rx),
            session: FakeSession::new(),
        });
        let pool = Arc::new(SessionPool::with_spawner(
            rust_config(),
            PathBuf::from("/workspace"),
            spawner,
            None,
        ));
        let spawned_pool = Arc::clone(&pool);
        let spawned_request = std::thread::spawn(move || {
            spawned_pool.check_lsp(&RelativePath::new("src/main.rs"), "")
        });

        started_rx
            .recv()
            .expect("spawn must enter its blocking I/O phase");
        let lock_was_released = pool.sessions.try_lock().is_ok();
        release_tx.send(()).expect("release blocked spawn");

        assert!(
            lock_was_released,
            "the session-pool lock must be released before spawn I/O blocks"
        );
        spawned_request
            .join()
            .expect("spawn request thread must not panic")
            .expect("released spawn must service the request");
    }

    #[test]
    fn f008_spawn_reuses_successful_session_and_does_not_cache_spawn_failure() {
        struct RetrySpawner {
            attempts: AtomicUsize,
            session: Arc<FakeSession>,
        }

        impl SessionSpawner for RetrySpawner {
            fn spawn(
                &self,
                _: &LspServerConfig,
                _: &str,
                _: PathBuf,
                _: &std::path::Path,
                _: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
                if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerLaunchFailed,
                    ));
                }
                Ok(Arc::clone(&self.session) as Arc<dyn PooledSession>)
            }
        }

        let spawner = Arc::new(RetrySpawner {
            attempts: AtomicUsize::new(0),
            session: FakeSession::new(),
        });
        let pool = SessionPool::with_spawner(
            rust_config(),
            PathBuf::from("/workspace"),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
            None,
        );
        let path = RelativePath::new("src/main.rs");

        assert_eq!(
            pool.check_lsp(&path, "").unwrap_err(),
            LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerLaunchFailed)
        );
        pool.check_lsp(&path, "").expect("failed spawn is retried");
        pool.check_lsp(&path, "").expect("live session is reused");

        assert_eq!(spawner.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(spawner.session.calls(), 2);
    }

    #[test]
    fn f008_spawn_concurrent_loser_is_discarded_before_servicing_request() {
        struct RacingSpawner {
            barrier: Barrier,
            next: AtomicUsize,
            sessions: [Arc<FakeSession>; 2],
        }

        impl SessionSpawner for RacingSpawner {
            fn spawn(
                &self,
                _: &LspServerConfig,
                _: &str,
                _: PathBuf,
                _: &std::path::Path,
                _: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
                let index = self.next.fetch_add(1, Ordering::SeqCst);
                self.barrier.wait();
                Ok(Arc::clone(&self.sessions[index]) as Arc<dyn PooledSession>)
            }
        }

        let first = FakeSession::new();
        let second = FakeSession::new();
        let spawner = Arc::new(RacingSpawner {
            barrier: Barrier::new(2),
            next: AtomicUsize::new(0),
            sessions: [Arc::clone(&first), Arc::clone(&second)],
        });
        let pool = Arc::new(SessionPool::with_spawner(
            rust_config(),
            PathBuf::from("/workspace"),
            spawner,
            None,
        ));

        let requests: Vec<_> = (0..2)
            .map(|_| {
                let pool = Arc::clone(&pool);
                std::thread::spawn(move || pool.check_lsp(&RelativePath::new("src/main.rs"), ""))
            })
            .collect();
        for request in requests {
            request
                .join()
                .expect("concurrent request must not panic")
                .expect("concurrent request must use the pooled session");
        }

        let call_counts = [first.calls(), second.calls()];
        assert!(
            call_counts == [2, 0] || call_counts == [0, 2],
            "both requests must be serviced by the cached winner: {call_counts:?}"
        );
    }

    #[test]
    fn malformed_initialize_capability_survives_real_lazy_spawn() {
        let initialize = r#"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"capabilities\":{\"definitionProvider\":\"yes\"}}}"#;
        let script = format!(
            "initialize='{initialize}'; printf 'Content-Length: %s\\r\\n\\r\\n%s' \"${{#initialize}}\" \"$initialize\"; sleep 60"
        );
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            LspServerConfig {
                command: "/bin/sh".to_owned(),
                extensions: vec!["rs".to_owned()],
                args: vec!["-c".to_owned(), script],
            },
        );
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let pool = SessionPool::new(
            LspConfig {
                servers,
                idle_timeout: None,
            },
            workspace.path().to_path_buf(),
        );

        let error = pool
            .definition_lsp(
                &RelativePath::new("src/main.rs"),
                "fn main() {}",
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .expect_err("malformed initialize capability must reject lazy spawn");

        assert_eq!(
            error,
            LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerMalformedResponse)
        );
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    // AC5: empty config, no server configured.
    #[test]
    fn empty_config_returns_unsupported() {
        let pool = SessionPool::new(LspConfig::default(), PathBuf::from("/workspace"));
        let rel = RelativePath::new("src/main.rs");
        assert!(!pool.serves(&rel));
        let err = pool.check(&rel, "fn main() {}").unwrap_err();
        assert!(matches!(err, CodeIntelError::Unsupported));
    }

    // AC5: extension not in config.
    #[test]
    fn unconfigured_extension_returns_unsupported() {
        let spawner = FakeSpawner::new();
        let pool = pool_with_spawner(rust_config(), spawner);
        let rel = RelativePath::new("README.txt");
        assert!(!pool.serves(&rel));
        let err = pool.check(&rel, "hello").unwrap_err();
        assert!(matches!(err, CodeIntelError::Unsupported));
    }

    // serves() reads ext_index with zero lock — O(1).
    #[test]
    fn serves_reflects_configured_extensions() {
        let spawner = FakeSpawner::new();
        let pool = pool_with_spawner(rust_config(), spawner);
        assert!(pool.serves(&RelativePath::new("lib.rs")));
        assert!(!pool.serves(&RelativePath::new("lib.go")));
    }

    // Lazy-spawn-once: two check() calls on the same language spawn exactly once.
    #[test]
    fn lazy_spawn_once_on_first_request_only() {
        let spawner = FakeSpawner::new();
        let session = FakeSession::new();
        spawner.register("rust", Arc::clone(&session));
        let pool = pool_with_spawner(
            rust_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );

        let rel = RelativePath::new("src/main.rs");
        pool.check(&rel, "fn main() {}").unwrap();
        pool.check(&rel, "fn main() {}").unwrap();

        assert_eq!(
            spawner.spawn_count("rust"),
            1,
            "must spawn only once for two requests"
        );
        assert_eq!(session.calls(), 2, "session.check() must be called twice");
    }

    #[test]
    fn nested_projects_spawn_distinct_sessions_at_discovered_roots() {
        struct RootRecordingSpawner {
            session: Arc<FakeSession>,
            roots: Mutex<Vec<(PathBuf, PathBuf)>>,
        }

        impl SessionSpawner for RootRecordingSpawner {
            fn spawn(
                &self,
                _: &LspServerConfig,
                _: &str,
                root: PathBuf,
                workspace_root: &std::path::Path,
                _: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
                self.roots
                    .lock()
                    .unwrap()
                    .push((root, workspace_root.to_path_buf()));
                Ok(Arc::clone(&self.session) as Arc<dyn PooledSession>)
            }
        }

        let workspace = tempfile::tempdir().unwrap();
        for project in ["crates/alpha", "crates/beta"] {
            std::fs::create_dir_all(workspace.path().join(project).join("src")).unwrap();
            std::fs::write(workspace.path().join(project).join("Cargo.toml"), "").unwrap();
        }
        let spawner = Arc::new(RootRecordingSpawner {
            session: FakeSession::new(),
            roots: Mutex::new(Vec::new()),
        });
        let pool = SessionPool::with_spawner(
            rust_config(),
            workspace.path().to_path_buf(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
            None,
        );

        pool.check(&RelativePath::new("crates/alpha/src/lib.rs"), "")
            .unwrap();
        pool.check(&RelativePath::new("crates/beta/src/lib.rs"), "")
            .unwrap();

        assert_eq!(
            *spawner.roots.lock().unwrap(),
            vec![
                (
                    workspace.path().join("crates/alpha"),
                    workspace.path().to_path_buf(),
                ),
                (
                    workspace.path().join("crates/beta"),
                    workspace.path().to_path_buf(),
                ),
            ]
        );
    }

    // Crash restart: flip dead_flag, next request re-spawns (spawn count = 2).
    #[test]
    fn crash_restart_respawns_on_next_request() {
        use std::collections::VecDeque;

        struct QueueSpawner {
            queue: Mutex<HashMap<String, VecDeque<Arc<FakeSession>>>>,
            counts: Mutex<HashMap<String, usize>>,
        }

        impl QueueSpawner {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    queue: Mutex::new(HashMap::new()),
                    counts: Mutex::new(HashMap::new()),
                })
            }

            fn push(&self, lang: &str, s: Arc<FakeSession>) {
                self.queue
                    .lock()
                    .unwrap()
                    .entry(lang.to_owned())
                    .or_default()
                    .push_back(s);
            }

            fn spawn_count(&self, lang: &str) -> usize {
                *self.counts.lock().unwrap().get(lang).unwrap_or(&0)
            }
        }

        impl SessionSpawner for QueueSpawner {
            fn spawn(
                &self,
                _cfg: &LspServerConfig,
                lang: &str,
                _root: PathBuf,
                _workspace_root: &std::path::Path,
                _push_tx: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
                *self
                    .counts
                    .lock()
                    .unwrap()
                    .entry(lang.to_owned())
                    .or_insert(0) += 1;
                self.queue
                    .lock()
                    .unwrap()
                    .get_mut(lang)
                    .and_then(VecDeque::pop_front)
                    .map(|session| session as Arc<dyn PooledSession>)
                    .ok_or(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerError,
                    ))
            }
        }

        let first = FakeSession::new();
        let second = FakeSession::new();
        let spawner = QueueSpawner::new();
        spawner.push("rust", Arc::clone(&first));
        spawner.push("rust", Arc::clone(&second));

        let pool = SessionPool::with_spawner(
            rust_config(),
            PathBuf::from("/workspace"),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
            None,
        );
        let rel = RelativePath::new("src/main.rs");

        pool.check(&rel, "").unwrap();
        assert_eq!(spawner.spawn_count("rust"), 1);

        first.set_dead();

        pool.check(&rel, "").unwrap();
        assert_eq!(spawner.spawn_count("rust"), 2, "must re-spawn after crash");
        assert_eq!(
            second.calls(),
            1,
            "second session must handle the post-crash request"
        );
    }

    // Idle eviction: session idle longer than timeout is evicted and re-spawned.
    #[test]
    fn idle_eviction_respawns_after_timeout() {
        use std::collections::VecDeque;

        struct QueueSpawner {
            queue: Mutex<VecDeque<Arc<FakeSession>>>,
            count: AtomicUsize,
        }

        impl QueueSpawner {
            fn new(sessions: Vec<Arc<FakeSession>>) -> Arc<Self> {
                Arc::new(Self {
                    queue: Mutex::new(sessions.into_iter().collect()),
                    count: AtomicUsize::new(0),
                })
            }
        }

        impl SessionSpawner for QueueSpawner {
            fn spawn(
                &self,
                _: &LspServerConfig,
                _: &str,
                _: PathBuf,
                _: &std::path::Path,
                _: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
                self.count.fetch_add(1, Ordering::Relaxed);
                self.queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .map(|session| session as Arc<dyn PooledSession>)
                    .ok_or(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerError,
                    ))
            }
        }

        let first = FakeSession::new();
        let second = FakeSession::new();
        let spawner = QueueSpawner::new(vec![Arc::clone(&first), Arc::clone(&second)]);
        let spawner_arc = Arc::clone(&spawner) as Arc<dyn SessionSpawner>;

        let mut config = rust_config();
        config.idle_timeout = Some(Duration::from_millis(1));

        let pool =
            SessionPool::with_spawner(config, PathBuf::from("/workspace"), spawner_arc, None);
        let rel = RelativePath::new("lib.rs");

        pool.check(&rel, "").unwrap();
        assert_eq!(spawner.count.load(Ordering::Relaxed), 1);

        // Backdate so the entry appears idle.
        pool.backdate_last_used_for_test("rust", Duration::from_millis(10));

        pool.check(&rel, "").unwrap();
        assert_eq!(
            spawner.count.load(Ordering::Relaxed),
            2,
            "must re-spawn after idle timeout"
        );
        assert_eq!(
            second.calls(),
            1,
            "second session must handle the post-eviction request"
        );
    }

    // Multi-language routing.
    #[test]
    fn multi_language_routes_by_extension() {
        let spawner = FakeSpawner::new();
        let rust_session = FakeSession::new();
        let go_session = FakeSession::new();
        spawner.register("rust", Arc::clone(&rust_session));
        spawner.register("go", Arc::clone(&go_session));

        let pool = pool_with_spawner(
            multi_lang_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );

        pool.check(&RelativePath::new("main.rs"), "").unwrap();
        pool.check(&RelativePath::new("main.go"), "").unwrap();

        assert_eq!(spawner.spawn_count("rust"), 1, "rust spawned once");
        assert_eq!(spawner.spawn_count("go"), 1, "go spawned once");
        assert_eq!(rust_session.calls(), 1, "rust session handled .rs check");
        assert_eq!(go_session.calls(), 1, "go session handled .go check");
    }

    // Unsupported: no spawn occurs for an unconfigured extension.
    #[test]
    fn unsupported_extension_never_calls_spawner() {
        let spawner = FakeSpawner::new();
        let pool = pool_with_spawner(
            rust_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );

        let err = pool.check(&RelativePath::new("index.js"), "").unwrap_err();
        assert!(matches!(err, CodeIntelError::Unsupported));
        assert_eq!(
            spawner.spawn_count("rust"),
            0,
            "spawner must NOT be called for unconfigured ext"
        );
    }

    #[test]
    fn typed_session_seam_returns_successful_values() {
        let session = FakeSession::new();
        let path = RelativePath::new("src/main.rs");
        let position = Position {
            line: 0,
            character: 4,
        };

        assert_eq!(session.check_lsp(&path, "fn main() {}").unwrap(), vec![]);
        assert_eq!(
            session
                .implementations_lsp(&path, "trait Example {}", position)
                .unwrap()[0]
                .path
                .as_str(),
            "src/impl.rs"
        );
        assert_eq!(
            session
                .prepare_rename_lsp(&path, "let name = 1;", position)
                .unwrap()
                .placeholder
                .as_deref(),
            Some("name")
        );
        assert!(
            session
                .rename_lsp(&path, "let name = 1;", position, "new_name")
                .unwrap()
                .changes
                .is_some()
        );
    }

    #[test]
    fn typed_session_seam_returns_unconfigured_language() {
        let spawner = FakeSpawner::new();
        let pool = pool_with_spawner(
            rust_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );

        let error = pool
            .check_lsp(&RelativePath::new("index.js"), "")
            .unwrap_err();

        assert_eq!(error, LspSessionError::Unconfigured);
        assert_eq!(spawner.spawn_count("rust"), 0);
    }

    #[test]
    fn typed_session_seam_returns_not_renameable() {
        let session = FakeSession::new();
        *session.canned_prepare_rename.lock().unwrap() = Err(RenameNavigationError::NotRenameable);

        let error = session
            .prepare_rename_lsp(
                &RelativePath::new("src/main.rs"),
                "let value = 1;",
                Position {
                    line: 0,
                    character: 4,
                },
            )
            .unwrap_err();

        assert_eq!(error, LspSessionError::NotRenameable);
    }

    #[test]
    fn typed_session_seam_defaults_backend_failures_to_server_error() {
        let session = FakeSession::new();
        *session.canned_prepare_rename.lock().unwrap() = Err(RenameNavigationError::Backend(
            "private backend detail".to_owned(),
        ));

        let error = session
            .prepare_rename_lsp(
                &RelativePath::new("src/main.rs"),
                "let value = 1;",
                Position {
                    line: 0,
                    character: 4,
                },
            )
            .unwrap_err();

        assert_eq!(
            error,
            LspSessionError::Backend(extension_protocol::LspOutcomeCode::ServerError)
        );
        assert_eq!(error.to_string(), "language server request failed");
    }

    #[test]
    fn session_pool_routes_implementation_prepare_rename_and_rename_through_get_or_spawn() {
        let spawner = FakeSpawner::new();
        let session = FakeSession::new();
        spawner.register("rust", Arc::clone(&session));
        let pool = pool_with_spawner(
            rust_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );
        let rel = RelativePath::new("src/main.rs");
        let pos = Position {
            line: 0,
            character: 5,
        };

        let implementations = pool.implementations(&rel, "trait Example {}", pos).unwrap();
        let prepared = pool.prepare_rename(&rel, "let name = 1;", pos).unwrap();
        let edit = pool.rename(&rel, "let name = 1;", pos, "new_name").unwrap();

        assert_eq!(spawner.spawn_count("rust"), 1);
        assert_eq!(session.implementation_calls(), 1);
        assert_eq!(session.prepare_rename_calls(), 1);
        assert_eq!(session.rename_calls(), 1);
        assert_eq!(implementations[0].path.as_str(), "src/impl.rs");
        assert_eq!(prepared.placeholder.as_deref(), Some("name"));
        assert!(edit.changes.is_some());
    }

    #[test]
    fn session_pool_restarts_dead_session_for_implementation_prepare_rename_and_rename() {
        use std::collections::VecDeque;

        struct QueueSpawner {
            queue: Mutex<VecDeque<Arc<FakeSession>>>,
            count: AtomicUsize,
        }

        impl QueueSpawner {
            fn new(sessions: Vec<Arc<FakeSession>>) -> Arc<Self> {
                Arc::new(Self {
                    queue: Mutex::new(sessions.into_iter().collect()),
                    count: AtomicUsize::new(0),
                })
            }
        }

        impl SessionSpawner for QueueSpawner {
            fn spawn(
                &self,
                _: &LspServerConfig,
                _: &str,
                _: PathBuf,
                _: &std::path::Path,
                _: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, LspSessionError> {
                self.count.fetch_add(1, Ordering::Relaxed);
                self.queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .map(|session| session as Arc<dyn PooledSession>)
                    .ok_or(LspSessionError::Backend(
                        extension_protocol::LspOutcomeCode::ServerError,
                    ))
            }
        }

        let first = FakeSession::new();
        let second = FakeSession::new();
        let third = FakeSession::new();
        let fourth = FakeSession::new();
        let spawner = QueueSpawner::new(vec![
            Arc::clone(&first),
            Arc::clone(&second),
            Arc::clone(&third),
            Arc::clone(&fourth),
        ]);
        let pool = SessionPool::with_spawner(
            rust_config(),
            PathBuf::from("/workspace"),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
            None,
        );
        let rel = RelativePath::new("src/main.rs");
        let pos = Position {
            line: 0,
            character: 0,
        };

        pool.implementations(&rel, "", pos).unwrap();
        first.set_dead();
        pool.implementations(&rel, "", pos).unwrap();

        second.set_dead();
        pool.prepare_rename(&rel, "let name = 1;", pos).unwrap();

        third.set_dead();
        pool.rename(&rel, "let name = 1;", pos, "new_name").unwrap();

        assert_eq!(spawner.count.load(Ordering::Relaxed), 4);
        assert_eq!(second.implementation_calls(), 1);
        assert_eq!(third.prepare_rename_calls(), 1);
        assert_eq!(fourth.rename_calls(), 1);
    }

    #[test]
    fn session_pool_returns_unsupported_language_for_rename_on_unconfigured_extension() {
        let spawner = FakeSpawner::new();
        let pool = pool_with_spawner(
            rust_config(),
            Arc::clone(&spawner) as Arc<dyn SessionSpawner>,
        );

        let err = pool
            .rename(
                &RelativePath::new("index.js"),
                "",
                Position {
                    line: 0,
                    character: 0,
                },
                "renamed",
            )
            .unwrap_err();

        assert_eq!(err, RenameNavigationError::UnsupportedLanguage);
        assert_eq!(spawner.spawn_count("rust"), 0);
    }
}
