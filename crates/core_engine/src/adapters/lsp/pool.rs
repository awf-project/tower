//! `SessionPool` — one resident LSP session per configured language, lazy-spawned
//! and crash-restartable (spec 14d).
//!
//! # Lifecycle
//!
//! - **Lazy spawn**: the first request for a language calls `spawner.spawn()`.
//!   Subsequent requests reuse the `Arc<dyn PooledSession>`.
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

use crate::adapters::config::lsp::{LspConfig, LspServerConfig};
use crate::adapters::lsp::{DiagnosticsSender, SharedState};
use crate::domain::RelativePath;
use crate::domain::code_intel::Diagnostic;
use crate::ports::{CodeIntelError, CodeIntelligencePort, DocumentSyncPort, NavigationPort};

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
    fn shared_state(&self) -> SharedState;

    /// Return the last published diagnostics for `uri` from `SharedState::by_uri`,
    /// without re-running `check`. Returns `[]` when the URI has never published.
    /// Called by `SessionPool::diagnostics_for` to serve `resources/read` (AC6 pull).
    fn diagnostics_for(&self, uri: &str) -> Vec<Diagnostic>;

    /// Test/`testing`-only: force this session into the "dead" state.
    ///
    /// `LspClientAdapter` kills its real OS process so the reader thread observes
    /// stdout EOF and sets `dead_flag` through the production detection path — the
    /// same sequence an external crash triggers. `FakeSession` flips its in-memory
    /// flag. No default body: every session decides how it dies.
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
        push_tx: Option<DiagnosticsSender>,
    ) -> Result<Arc<dyn PooledSession>, CodeIntelError>;
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
        push_tx: Option<DiagnosticsSender>,
    ) -> Result<Arc<dyn PooledSession>, CodeIntelError> {
        use crate::adapters::lsp::LspClientAdapter;
        let adapter = LspClientAdapter::spawn(
            &cfg.command,
            &cfg.args,
            cfg.extensions.clone(),
            language_id.to_owned(),
            root,
            push_tx,
        )?;
        Ok(Arc::new(adapter) as Arc<dyn PooledSession>)
    }
}

// ── DiagnosticsReader trait ───────────────────────────────────────────────────

/// One-method read-only trait threaded into `transport.rs` for `resources/read`.
/// `SessionPool` implements it; `FakePool` implements it in transport tests.
/// Keeps the transport testable without depending on `SessionPool` concretely.
pub trait DiagnosticsReader: Send + Sync {
    /// Return the last published diagnostics for the given LSP URI, or `[]`
    /// when no live session exists for that URI's extension.
    fn diagnostics_for(&self, uri: &str) -> Vec<Diagnostic>;
}

// ── PoolEntry ─────────────────────────────────────────────────────────────────

struct PoolEntry {
    session: Arc<dyn PooledSession>,
    last_used: Instant,
}

// ── SessionPool ───────────────────────────────────────────────────────────────

/// Manages one resident LSP session per configured language.
///
/// Constructed with `new` (production) or `with_spawner` (test seam).
pub struct SessionPool {
    /// Language name → live session entry.
    sessions: Mutex<HashMap<String, PoolEntry>>,
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
    fn get_or_spawn(&self, lang: &str) -> Result<Arc<dyn PooledSession>, CodeIntelError> {
        // Phase 1: look up under lock, evict if dead/idle.
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = sessions.get(lang) {
                let dead = entry.session.is_dead();
                let idle = self
                    .idle_timeout
                    .is_some_and(|t| entry.last_used.elapsed() > t);
                if !dead && !idle {
                    // Fast path: reuse the live session.
                    let session = Arc::clone(&entry.session);
                    // Update last_used while we still hold the lock.
                    sessions.get_mut(lang).unwrap().last_used = Instant::now();
                    return Ok(session);
                }
                // Evict dead or idle entry.
                sessions.remove(lang);
            }
        }
        // Phase 2: spawn outside the lock (may block on process I/O).
        let cfg = self
            .lang_index
            .get(lang)
            .ok_or(CodeIntelError::Unsupported)?;
        let session =
            self.spawner
                .spawn(cfg, lang, self.workspace_root.clone(), self.push_tx.clone())?;

        // Phase 3: re-acquire to insert; discard concurrent loser.
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions
            .entry(lang.to_owned())
            .or_insert_with(|| PoolEntry {
                session: Arc::clone(&session),
                last_used: Instant::now(),
            });
        Ok(session)
    }

    /// Return the last published diagnostics for the given LSP URI.
    /// Routes the URI's extension to the (already-spawned) session via
    /// `session.diagnostics_for(uri)`. Returns `[]` when no live session or URI.
    pub fn diagnostics_for_uri(&self, uri: &str) -> Vec<Diagnostic> {
        // Extract extension from URI path (e.g. "file:///w/src/main.rs" → "rs").
        let ext = uri.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let lang = match self.ext_index.get(ext) {
            Some(l) => l.as_str(),
            None => return vec![],
        };
        let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        match sessions.get(lang) {
            Some(entry) if !entry.session.is_dead() => entry.session.diagnostics_for(uri),
            _ => vec![],
        }
    }

    /// Whether the pool is configured to handle files with this path's extension.
    /// O(1), no lock — reads the immutable `ext_index`.
    #[must_use]
    pub fn serves(&self, path: &RelativePath) -> bool {
        self.lang_for_path(path).is_some()
    }

    /// Return the static resource URI list: one `lsp://<lang>/diagnostics` entry
    /// per configured language. Called at startup by `main.rs` and passed to
    /// `serve_with_push` as the `resource_uris` list for `resources/list`.
    /// No pool lock — reads the immutable `lang_index`.
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
        if let Some(entry) = sessions.get_mut(lang) {
            entry.last_used -= by;
        }
    }

    /// Test-only: simulate a crash for the session serving `lang` by calling
    /// `kill_for_test()` on it. The pool's next `get_or_spawn` for that language
    /// will detect `is_dead() == true` and re-spawn a fresh session (AC4).
    ///
    /// Returns `true` when the session was found and signalled; `false` when no
    /// live session exists for `lang` (e.g. first request has not happened yet).
    #[cfg(any(test, feature = "testing"))]
    pub fn kill_session_for_test(&self, lang: &str) -> bool {
        // Clone the Arc and drop the pool lock before `kill_for_test`: the real
        // session's `kill_for_test` locks `inner`, so we must not hold the pool
        // (tier-0) lock across it (lock-ordering discipline).
        let session = {
            let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            sessions.get(lang).map(|entry| Arc::clone(&entry.session))
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
    #[cfg(any(test, feature = "testing"))]
    pub fn session_is_dead_for_test(&self, lang: &str) -> Option<bool> {
        let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions.get(lang).map(|entry| entry.session.is_dead())
    }
}

impl DiagnosticsReader for SessionPool {
    fn diagnostics_for(&self, uri: &str) -> Vec<Diagnostic> {
        self.diagnostics_for_uri(uri)
    }
}

// ── Port impls on SessionPool ─────────────────────────────────────────────────
//
// Each port impl routes by extension → language → get_or_spawn → session method.
// The pool lock is never held while doing session I/O.

impl CodeIntelligencePort for SessionPool {
    fn check(&self, path: &RelativePath, text: &str) -> Result<Vec<Diagnostic>, CodeIntelError> {
        let lang = self
            .lang_for_path(path)
            .ok_or(CodeIntelError::Unsupported)?
            .to_owned();
        let session = self.get_or_spawn(&lang)?;
        session.check(path, text)
    }
}

impl NavigationPort for SessionPool {
    fn definition(
        &self,
        path: &RelativePath,
        text: &str,
        position: crate::domain::code_intel::Position,
    ) -> Result<Vec<crate::domain::code_intel::Location>, CodeIntelError> {
        let lang = self
            .lang_for_path(path)
            .ok_or(CodeIntelError::Unsupported)?
            .to_owned();
        let session = self.get_or_spawn(&lang)?;
        session.definition(path, text, position)
    }

    fn references(
        &self,
        path: &RelativePath,
        text: &str,
        position: crate::domain::code_intel::Position,
    ) -> Result<Vec<crate::domain::code_intel::Location>, CodeIntelError> {
        let lang = self
            .lang_for_path(path)
            .ok_or(CodeIntelError::Unsupported)?
            .to_owned();
        let session = self.get_or_spawn(&lang)?;
        session.references(path, text, position)
    }

    fn hover(
        &self,
        path: &RelativePath,
        text: &str,
        position: crate::domain::code_intel::Position,
    ) -> Result<Option<crate::domain::code_intel::Hover>, CodeIntelError> {
        let lang = self
            .lang_for_path(path)
            .ok_or(CodeIntelError::Unsupported)?
            .to_owned();
        let session = self.get_or_spawn(&lang)?;
        session.hover(path, text, position)
    }

    fn document_symbols(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<crate::domain::code_intel::Symbol>, CodeIntelError> {
        let lang = self
            .lang_for_path(path)
            .ok_or(CodeIntelError::Unsupported)?
            .to_owned();
        let session = self.get_or_spawn(&lang)?;
        session.document_symbols(path, text)
    }
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
        if let Ok(session) = self.get_or_spawn(&lang) {
            session.document_opened(path, text);
        }
    }

    fn document_changed(&self, path: &RelativePath, text: &str) {
        let Some(lang) = self.lang_for_path(path).map(str::to_owned) else {
            return;
        };
        if let Ok(session) = self.get_or_spawn(&lang) {
            session.document_changed(path, text);
        }
    }

    fn document_closed(&self, path: &RelativePath) {
        let Some(lang) = self.lang_for_path(path).map(str::to_owned) else {
            return;
        };
        if let Ok(session) = self.get_or_spawn(&lang) {
            session.document_closed(path);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::lsp::{LspConfig, LspServerConfig};
    use crate::domain::RelativePath;
    use crate::ports::CodeIntelError;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // ── FakeSession ───────────────────────────────────────────────────────────

    struct FakeSession {
        dead: AtomicBool,
        check_calls: AtomicUsize,
        canned_diags: Vec<Diagnostic>,
    }

    impl FakeSession {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                dead: AtomicBool::new(false),
                check_calls: AtomicUsize::new(0),
                canned_diags: vec![],
            })
        }

        fn set_dead(&self) {
            self.dead.store(true, Ordering::Relaxed);
        }

        fn calls(&self) -> usize {
            self.check_calls.load(Ordering::Relaxed)
        }
    }

    impl PooledSession for FakeSession {
        fn is_dead(&self) -> bool {
            self.dead.load(Ordering::Relaxed)
        }

        fn shared_state(&self) -> SharedState {
            Arc::new((
                Mutex::new(crate::adapters::lsp::SessionStateInner::default()),
                std::sync::Condvar::new(),
            ))
        }

        fn diagnostics_for(&self, _uri: &str) -> Vec<Diagnostic> {
            self.canned_diags.clone()
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
            _: crate::domain::code_intel::Position,
        ) -> Result<Vec<crate::domain::code_intel::Location>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }

        fn references(
            &self,
            _: &RelativePath,
            _: &str,
            _: crate::domain::code_intel::Position,
        ) -> Result<Vec<crate::domain::code_intel::Location>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }

        fn hover(
            &self,
            _: &RelativePath,
            _: &str,
            _: crate::domain::code_intel::Position,
        ) -> Result<Option<crate::domain::code_intel::Hover>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
        }

        fn document_symbols(
            &self,
            _: &RelativePath,
            _: &str,
        ) -> Result<Vec<crate::domain::code_intel::Symbol>, CodeIntelError> {
            Err(CodeIntelError::Unsupported)
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

    struct FakeSpawner {
        /// language → (session_to_return, spawn_count)
        per_lang: Mutex<HashMap<String, (Arc<FakeSession>, usize)>>,
    }

    impl FakeSpawner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                per_lang: Mutex::new(HashMap::new()),
            })
        }

        fn register(&self, lang: &str, session: Arc<FakeSession>) {
            self.per_lang
                .lock()
                .unwrap()
                .insert(lang.to_owned(), (session, 0));
        }

        fn spawn_count(&self, lang: &str) -> usize {
            self.per_lang
                .lock()
                .unwrap()
                .get(lang)
                .map(|(_, c)| *c)
                .unwrap_or(0)
        }
    }

    impl SessionSpawner for FakeSpawner {
        fn spawn(
            &self,
            _cfg: &LspServerConfig,
            language_id: &str,
            _root: PathBuf,
            _push_tx: Option<DiagnosticsSender>,
        ) -> Result<Arc<dyn PooledSession>, CodeIntelError> {
            let mut map = self.per_lang.lock().unwrap();
            match map.get_mut(language_id) {
                Some((session, count)) => {
                    *count += 1;
                    Ok(Arc::clone(session) as Arc<dyn PooledSession>)
                }
                None => Err(CodeIntelError::Backend(format!(
                    "FakeSpawner: no session registered for {language_id}"
                ))),
            }
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
                _push_tx: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, CodeIntelError> {
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
                    .and_then(|q| q.pop_front())
                    .map(|s| s as Arc<dyn PooledSession>)
                    .ok_or_else(|| CodeIntelError::Backend("queue empty".into()))
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
                _: Option<DiagnosticsSender>,
            ) -> Result<Arc<dyn PooledSession>, CodeIntelError> {
                self.count.fetch_add(1, Ordering::Relaxed);
                self.queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .map(|s| s as Arc<dyn PooledSession>)
                    .ok_or_else(|| CodeIntelError::Backend("queue empty".into()))
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
}
