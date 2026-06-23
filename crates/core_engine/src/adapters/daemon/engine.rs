//! Engine assembly: open Sled, scan, build shared state, load extensions,
//! start the watcher. Extracted from the old `main.rs::run` so both the daemon
//! and (Phase A) the stdio path share one setup. Serving is the caller's job.
#![forbid(unsafe_code)]

use std::env;
use std::sync::{Arc, RwLock};

use crate::adapters::SledStorageAdapter;
use crate::adapters::ast_index::{
    XdgAstIndexAdapter, compute_workspace_key, global_ast_workspace_dir, workspace_ast_dir,
};
use crate::adapters::cli::{GlobalOpts, resolve_extensions_dir_arg, resolve_workspace_root};
use crate::adapters::config::{TowerConfig, legacy_plugins_dir_fallback};
use crate::adapters::extension::{
    HostDeps as ExtensionHostDeps, global_extensions_dir, load_extensions_into_registry,
    resolve_extension_dirs,
};
use crate::adapters::fs::scan::reconcile_pruned;
use crate::adapters::fs::{RealFs, workspace_scan};
use crate::adapters::mcp::PushEvent;
use crate::adapters::mcp::diagnostics::{DiagnosticsReader, NoOpDiagnosticsReader};
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::watcher::NotifyWatcherAdapter;
use crate::domain::extension_host::ExtensionRegistry;
use crate::domain::index::InvertedIndex;
use crate::domain::workspace::ProjectWorkspace;
use crate::ports::{AstIndexPort, ExtensionHostPort, NoOpDocumentSync, StoragePort};

// ── SharedExtensionHost ───────────────────────────────────────────────────────

/// Adapter that bridges the watcher's [`ExtensionHostPort`] interface to the
/// [`ExtensionRegistry`] through an `Arc<RwLock<_>>`.
///
/// # Safety
///
/// The `RwLock` read guard is acquired and released within each call — no lock
/// is held across the extension delivery, so concurrent MCP reads are never
/// starved.
pub(crate) struct SharedExtensionHost(pub(crate) Arc<RwLock<ExtensionRegistry>>);

impl ExtensionHostPort for SharedExtensionHost {
    fn on_file_indexed(&self, id: crate::domain::FileId, path: &crate::domain::RelativePath) {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_indexed(id, path);
    }

    fn on_file_changed(&self, id: crate::domain::FileId, path: &crate::domain::RelativePath) {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_changed(id, path);
    }

    fn on_file_deleted(&self, path: &crate::domain::RelativePath) {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_deleted(path);
    }

    fn declared_tools(&self) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.declared_tools()
    }

    fn invoke(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, crate::domain::InvokeError> {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.invoke(tool_name, params)
    }
}

// ── EngineHandle ──────────────────────────────────────────────────────────────

/// Everything the serve loop needs, with the watcher kept alive by ownership.
pub struct EngineHandle {
    pub state: Arc<RwLock<EngineState>>,
    pub ext_registry: Arc<RwLock<ExtensionRegistry>>,
    pub push_rx: std::sync::mpsc::Receiver<PushEvent>,
    pub diag_reader: Arc<dyn DiagnosticsReader>,
    /// Kept alive: dropping the watcher stops live sync.
    pub _watcher: NotifyWatcherAdapter,
}

// ── build_engine ──────────────────────────────────────────────────────────────

/// Assemble the engine from a workspace root and a loaded config.
///
/// Opens Sled, runs the initial workspace scan (or reconcile if already done),
/// builds shared state, loads sidecar extensions, and starts the filesystem
/// watcher. Serving (stdio or socket) is the caller's responsibility.
///
/// The `tower_config` is passed in ready-to-use; callers are responsible for
/// loading it via `config::load` and running `config::apply_backcompat` before
/// calling this function.
pub fn build_engine(opts: &GlobalOpts, tower_config: TowerConfig) -> Result<EngineHandle, String> {
    let workspace_root = resolve_workspace_root(opts);

    // ── Step 2: open sled database ────────────────────────────────────────────
    let db_path = workspace_root.join(".tower").join("db");
    std::fs::create_dir_all(&db_path)
        .map_err(|e| format!("failed to create db dir {}: {e}", db_path.display()))?;

    let (mut storage, workspace, index) = SledStorageAdapter::open(&db_path)
        .map_err(|e| format!("failed to open sled database at {}: {e}", db_path.display()))?;

    // ── Step 3: initial workspace scan if never completed ─────────────────────
    let (workspace, index) = if !storage.is_scan_complete().unwrap_or(false) {
        let mut fresh_workspace = ProjectWorkspace::new();
        let mut fresh_index = InvertedIndex::new();
        match workspace_scan(
            &workspace_root,
            &mut storage,
            &mut fresh_workspace,
            &mut fresh_index,
        ) {
            Ok(report) => {
                eprintln!(
                    "tower: initial scan complete — {} files indexed",
                    report.indexed
                );
            }
            Err(e) => {
                eprintln!("tower: warning — initial scan failed: {e}");
            }
        }
        (fresh_workspace, fresh_index)
    } else {
        // Scan already complete: sled state is loaded verbatim. Reconcile it
        // against the current filesystem so ghosts (files deleted while down,
        // entries indexed before an ignore rule existed, foreign temp files)
        // are pruned. Path-only walk — does not read or re-hash file contents,
        // keeping startup cheap on large repos. Additions are out of scope (the
        // live watcher and tower_reindex cover offline-added files).
        let mut workspace = workspace;
        let mut index = index;
        let pruned = reconcile_pruned(&workspace_root, &mut workspace, &mut index, &mut storage);
        if pruned > 0 {
            eprintln!("tower: reconciled index — {pruned} stale entries pruned");
        }
        (workspace, index)
    };

    // ── Step 4: build shared engine state ─────────────────────────────────────
    //
    // Clone the sled adapter before moving `storage` into `EngineState`.
    // `SledStorageAdapter::try_clone` shares the same underlying sled trees —
    // no second `sled::open`, no second file lock. The clone is kept here
    // and moved into `NotifyWatcherAdapter` below.
    let storage_for_watcher = storage.try_clone();
    let fs = RealFs::new(&workspace_root);
    let state = Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(storage),
        Box::new(fs),
    )));

    // ── AST index: resolve the per-workspace XDG data directory ──────────────
    //
    // Decision: fall back to `<workspace_root>/.tower/ast` when
    // `global_ast_workspace_dir()` returns None (no HOME/XDG_DATA_HOME, e.g.
    // a headless CI container without $HOME set).
    //
    // Why: silently falling back to a workspace-local directory keeps the
    // binary usable in constrained environments. The data is a pure cache
    // (re-derivable by the extension); losing XDG isolation in those
    // environments is acceptable.
    //
    // Trade-off: the local `.tower/ast` directory is committed-adjacent and
    // could appear in version control if the user forgets to gitignore it.
    // A future improvement could warn and suggest adding `.tower/ast/` to
    // `.gitignore`, but that is out of scope here.
    let ast_base_dir = match global_ast_workspace_dir() {
        Some(_) => workspace_ast_dir(&compute_workspace_key(&workspace_root)),
        None => {
            eprintln!(
                "tower: warning — XDG data directory unavailable; \
                 AST index will be stored in <workspace>/.tower/ast"
            );
            workspace_root.join(".tower").join("ast")
        }
    };
    let ext_ast_index: Arc<dyn AstIndexPort + Send + Sync> =
        Arc::new(XdgAstIndexAdapter::new(ast_base_dir));

    // Real format queue: workers run the external formatters declared in
    // .tower/config.toml and share their echo-suppression set with the registry
    // (loop break). No threads start until host_request_format enqueues a job.
    let format_queue: Arc<dyn crate::adapters::formatter::FormatQueuePort + Send + Sync> =
        Arc::new(crate::adapters::formatter::FormatQueue::new(
            tower_config.plugins.formatter.clone(),
            workspace_root.clone(),
        ));

    // Capture the formatter echo set before `format_queue` is moved into the
    // extension deps. Shared with the watcher so formatter-induced writes do not
    // trigger a spurious `didChange` to the language server (spec 14b UN1).
    let echo_set = format_queue
        .shared_echo_set()
        .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())));

    // ── Step 5a: push bridge (for extension notify/resourceUpdated) ───────────
    //
    // The push channel carries `PushEvent`s from extensions (via their
    // `notify/resourceUpdated` host-call) to the MCP serve loop. The
    // `spawn_push_task` function bridges these into MCP push notifications.
    let (push_tx, push_rx) = std::sync::mpsc::channel::<PushEvent>();

    // ── Step 5b: discover and load sidecar extensions (spec 28 EV1) ──────────
    //
    // Resolution order:
    //   1. --extensions-dir <path> / $TOWER_EXTENSIONS_DIR (explicit override).
    //   2. Default: XDG global → <workspace>/.tower/extensions (local wins).
    //   3. Back-compat fallback: if .tower/extensions/ absent but .tower/plugins/
    //      exists, use the plugins dir and warn (spec 28 O1).
    let extensions_dir_local = workspace_root.join(".tower/extensions");
    let plugins_dir_legacy = workspace_root.join(".tower/plugins");

    let ext_dir_arg = resolve_extensions_dir_arg(opts);

    let mut extension_dirs = resolve_extension_dirs(
        ext_dir_arg.as_deref(),
        env::var("TOWER_EXTENSIONS_DIR").ok().as_deref(),
        global_extensions_dir(),
        &workspace_root,
    );

    // Spec 28 O1: legacy .tower/plugins/ fallback — warn and substitute.
    if let Some((fallback_dir, warning)) =
        legacy_plugins_dir_fallback(&extensions_dir_local, &plugins_dir_legacy)
    {
        eprintln!("{warning}");
        // Replace the local scope entry with the legacy plugins dir.
        // `resolve_extension_dirs` with no override produces [global?, local].
        // If the extensions_dir_arg / env override was set, extension_dirs has
        // a single explicit entry and no local-scope fallback is needed.
        if ext_dir_arg.is_none()
            && env::var("TOWER_EXTENSIONS_DIR")
                .unwrap_or_default()
                .is_empty()
        {
            // Replace any occurrence of extensions_dir_local with the fallback.
            for dir in &mut extension_dirs {
                if dir == &extensions_dir_local {
                    *dir = fallback_dir.clone();
                }
            }
        }
    }

    // Extensions read the real workspace through their FileSystemPort capability.
    let ext_fs: Arc<dyn crate::adapters::extension::host_deps::FsAdapter + Send + Sync> =
        Arc::new(std::sync::Mutex::new(RealFs::new(&workspace_root)));

    let ext_deps = ExtensionHostDeps {
        fs: ext_fs,
        // Cast Arc<dyn AstIndexPort + Send + Sync> → Arc<dyn AstIndexPort>:
        // HostDeps fields do not carry the Send + Sync bounds so we upcast here.
        ast_index: Arc::clone(&ext_ast_index) as Arc<dyn AstIndexPort>,
        format_queue: Arc::clone(&format_queue)
            as Arc<dyn crate::adapters::formatter::FormatQueuePort>,
        push_tx: Some(push_tx),
    };

    let ext_registry = load_extensions_into_registry(
        &extension_dirs,
        ext_deps,
        tower_config.extensions.request_timeout(),
        &tower_config.extensions.disabled,
    );
    let ext_tool_count = ext_registry.declared_tools().len();
    if ext_tool_count > 0 {
        let scopes: Vec<String> = extension_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        eprintln!(
            "tower: loaded {ext_tool_count} extension tool(s) from [{}]",
            scopes.join(", ")
        );
    }
    let ext_registry = Arc::new(RwLock::new(ext_registry));

    // Inject the extension host into the shared EngineState so MCP-driven
    // mutations (create/delete/global_replace) broadcast `on_file_changed` to
    // extensions. Without this the handlers would keep their no-op default and
    // the AST extension's cross-file index would go stale after MCP deletes/edits.
    {
        let host: Arc<dyn ExtensionHostPort + Send + Sync> =
            Arc::new(SharedExtensionHost(Arc::clone(&ext_registry)));
        state
            .write()
            .map_err(|_| "engine state lock poisoned".to_string())?
            .set_plugin_host(host);
    }

    // ── Step 5c: spawn the filesystem watcher for live VFS sync ──────────────
    //
    // Decision: always-on, fail-fast at startup.
    // Why: live VFS sync is now a core feature (spec 06), not optional.
    //
    // Watcher state sharing:
    //   workspace + index — Arc clones from EngineState (same instances as MCP)
    //   storage          — try_clone() of the sled adapter (same Db, no re-open)
    //   extension_host   — SharedExtensionHost wrapping Arc<RwLock<ExtensionRegistry>>
    let watcher_workspace = state
        .read()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .workspace_arc();
    let watcher_index = state
        .read()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .index_arc();
    let watcher_ext_host: Box<dyn ExtensionHostPort + Send + Sync> =
        Box::new(SharedExtensionHost(Arc::clone(&ext_registry)));

    // LSP doc-sync is now provided by the LSP extension (spec 27).
    // The hard-wired SessionPool / doc_sync adapter is no longer wired into the
    // watcher; use the NoOp stub so the watcher no longer drives language-server
    // document events directly.
    let doc_sync: Arc<dyn crate::ports::DocumentSyncPort + Send + Sync> =
        Arc::new(NoOpDocumentSync);

    // `_watcher` is intentionally bound (not `_`) so the Drop impl runs at the
    // end of the caller's scope — dropping it earlier would stop live sync.
    let _watcher = NotifyWatcherAdapter::with_document_sync(
        workspace_root.clone(),
        watcher_workspace,
        watcher_index,
        Box::new(storage_for_watcher),
        watcher_ext_host,
        doc_sync,
        Arc::clone(&echo_set),
    )
    .map_err(|e| format!("failed to start filesystem watcher: {e}"))?;

    eprintln!(
        "tower: filesystem watcher active on {}",
        workspace_root.display()
    );

    // ── Step 5d: replay the initial scan to extensions ───────────────────────
    //
    // The workspace scan (step 3) ran before extensions were loaded (step 5b),
    // so event subscribers — notably the AST extension's cross-file symbol
    // index — never saw those files, leaving `search_symbols` empty until a
    // manual `reindex`. Deliver the already-indexed files as `fileIndexed` now,
    // in a background thread so startup is not blocked (delivery is one
    // round-trip per file per subscriber). The watcher (step 5c) covers
    // everything that changes from here on; re-delivering a file the watcher
    // also reports is harmless (the AST index is idempotent per path).
    {
        let replay: Vec<(crate::domain::FileId, crate::domain::RelativePath)> = {
            let ws_arc = state
                .read()
                .map_err(|_| "engine state lock poisoned".to_string())?
                .workspace_arc();
            let ws = ws_arc.read().unwrap_or_else(|p| p.into_inner());
            ws.all_file_ids()
                .into_iter()
                .filter_map(|id| ws.get(id).ok().map(|vf| (id, vf.path.clone())))
                .collect()
        };
        if !replay.is_empty() {
            eprintln!(
                "tower: delivering {} indexed file(s) to extensions (background)",
                replay.len()
            );
            let host = SharedExtensionHost(Arc::clone(&ext_registry));
            std::thread::Builder::new()
                .name("ext-initial-index".to_owned())
                .spawn(move || {
                    for (id, path) in replay {
                        host.on_file_indexed(id, &path);
                    }
                })
                .map_err(|e| format!("failed to spawn initial-index thread: {e}"))?;
        }
    }

    let diag_reader: Arc<dyn DiagnosticsReader> = Arc::new(NoOpDiagnosticsReader);

    Ok(EngineHandle {
        state,
        ext_registry,
        push_rx,
        diag_reader,
        _watcher,
    })
}
