//! `tower` binary — MCP stdio server (spec 10b, closes Milestone 3).
//!
//! # Startup sequence
//!
//! 1. Resolve workspace root: `--workspace-dir <path>` flag or `$TOWER_WORKSPACE`
//!    env var, falling back to the current working directory.
//! 2. Open the sled database from `<root>/.tower/db`.
//!    `SledStorageAdapter::open` reconstructs the [`ProjectWorkspace`] and
//!    [`InvertedIndex`] from persisted state (spec 04a/04b).
//! 3. Run the initial workspace scan if one has never completed
//!    (`StoragePort::is_scan_complete`).
//! 4. Wrap all state in an `Arc<RwLock<EngineState>>` for deadlock-free sharing
//!    with any future background watcher thread (spec 06 lock discipline).
//! 5. Discover and load sidecar extensions (spec 25/28): resolve the ordered
//!    extension dirs (`--extensions-dir` / `$TOWER_EXTENSIONS_DIR` override or
//!    XDG global then workspace-local, local wins on name collision), spawn each
//!    extension binary via `SidecarHostAdapter`, register survivors. A missing or
//!    empty scope yields no extensions; a single bad extension is skipped with a
//!    stderr warning and never aborts startup.
//! 6. Serve the 9 native `tower_*` tools PLUS any extension tools (namespaced
//!    `tower_<ext>_<tool>`) over real `stdin` / `stdout` via an
//!    `ExtensionMergedRegistry`.
//!
//! # Wiring decision: `Arc<RwLock<EngineState>>`
//!
//! The spec requires that tool handlers and the filesystem watcher (spec 06)
//! share workspace/index/storage/fs without copying. `Arc` provides shared
//! ownership; `RwLock` allows concurrent readers (e.g. simultaneous
//! `tower_find_file` and `tower_search_text`) with exclusive mutation for writers
//! (create/delete/global_replace). Short critical sections only — no blocking
//! I/O is performed while holding the lock.
//!
//! # Error handling
//!
//! Startup failures print a human-readable message to stderr and exit with
//! code 1. The serve loop only returns on unrecoverable I/O (broken pipe);
//! malformed frames and tool errors are returned as JSON-RPC error responses
//! and the loop continues.

use std::env;
use std::sync::{Arc, RwLock};

use core_engine::adapters::SledStorageAdapter;
use core_engine::adapters::ast_index::{
    XdgAstIndexAdapter, compute_workspace_key, global_ast_workspace_dir, workspace_ast_dir,
};
use core_engine::adapters::config;
use core_engine::adapters::extension::{
    HostDeps as ExtensionHostDeps, global_extensions_dir, load_extensions_into_registry,
    resolve_extension_dirs,
};
use core_engine::adapters::fs::scan::{
    init_towerignore, reconcile_pruned, warn_if_towerignore_absent,
};
use core_engine::adapters::fs::{RealFs, workspace_scan};
use core_engine::adapters::mcp::lsp_tools::SubscriptionRegistry;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::{
    ExtensionMergedRegistry, PushEvent, TowerMcpHandler, spawn_push_task,
};
use core_engine::adapters::watcher::NotifyWatcherAdapter;
use core_engine::domain::extension_host::ExtensionRegistry;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::AstIndexPort;
use core_engine::ports::{ExtensionHostPort, StoragePort};

// ── SharedExtensionHost ───────────────────────────────────────────────────────

/// Adapter that bridges the watcher's [`ExtensionHostPort`] interface to the
/// [`ExtensionRegistry`] through an `Arc<RwLock<_>>`.
///
/// # Safety
///
/// The `RwLock` read guard is acquired and released within each call — no lock
/// is held across the extension delivery, so concurrent MCP reads are never
/// starved.
struct SharedExtensionHost(Arc<RwLock<ExtensionRegistry>>);

impl ExtensionHostPort for SharedExtensionHost {
    fn on_file_indexed(
        &self,
        id: core_engine::domain::FileId,
        path: &core_engine::domain::RelativePath,
    ) {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_indexed(id, path);
    }

    fn on_file_changed(
        &self,
        id: core_engine::domain::FileId,
        path: &core_engine::domain::RelativePath,
    ) {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_changed(id, path);
    }

    fn on_file_deleted(&self, path: &core_engine::domain::RelativePath) {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_deleted(path);
    }

    fn declared_tools(
        &self,
    ) -> Vec<(
        core_engine::domain::ExtensionId,
        extension_protocol::ToolDecl,
    )> {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.declared_tools()
    }

    fn invoke(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, core_engine::domain::InvokeError> {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.invoke(tool_name, params)
    }
}

fn main() {
    // `init` may appear after global flags (e.g. `tower --workspace-dir <p> init`),
    // mirroring how resolve_workspace_root scans all args, so detect it anywhere.
    let args: Vec<String> = env::args().collect();
    if args.iter().skip(1).any(|a| a == "init") {
        if let Err(e) = run_init() {
            eprintln!("tower: {e}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(e) = run() {
        eprintln!("tower: {e}");
        std::process::exit(1);
    }
}

/// Handle `tower init`: scaffold a default `.towerignore` and `.tower/config.toml`
/// at the workspace root.
///
/// tower's file walker is authoritative and independent of git: it consults only
/// `.towerignore` (never `.gitignore`). `tower init` writes a sensible default.
/// It refuses to overwrite an existing `.towerignore` (returns an error; the
/// caller exits non-zero) so user edits are never clobbered. The `config.toml`
/// seed (default formatter tools) is best-effort: a pre-existing config is left
/// untouched rather than failing the command.
fn run_init() -> Result<(), String> {
    let root = resolve_workspace_root();
    let ignore = match init_towerignore(&root) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(format!("{e}")),
        Err(e) => return Err(format!("failed to write .towerignore: {e}")),
    };
    println!("created {}", ignore.display());

    // Seed `.tower/config.toml` with default formatter tools. A pre-existing
    // config is left untouched (note, not an error) so re-running `tower init`
    // after a `.towerignore` was removed still behaves predictably.
    match config::init_config(&root) {
        Ok(path) => println!("created {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => println!("note: {e}"),
        Err(e) => return Err(format!("failed to write .tower/config.toml: {e}")),
    }
    Ok(())
}

fn run() -> Result<(), String> {
    // ── Step 1: resolve workspace root ────────────────────────────────────────
    let workspace_root = resolve_workspace_root();

    // Load the local project config (.tower/config.toml) early so a malformed
    // file fails fast before any DB or watcher work. Absent file → defaults.
    let mut tower_config = config::load(&workspace_root).map_err(|e| e.to_string())?;

    // Apply backward-compatibility migrations from the legacy [plugins] section
    // → [extensions] section (spec 28 O1), emitting deprecation warnings.
    for warning in config::apply_backcompat(&mut tower_config) {
        eprintln!("{warning}");
    }

    // Warn on EVERY boot when the sole ignore source is absent — not only on a
    // fresh scan. The scan's restart guard short-circuits subsequent boots, so a
    // warning placed inside the scan path would never fire after the first run,
    // hiding the security-relevant "indexing everything" default. Emitted here
    // (independent of the is_scan_complete guard) so the operator is reminded
    // each time the watcher will live-index all non-hidden files.
    warn_if_towerignore_absent(&workspace_root);

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
    let format_queue: Arc<dyn core_engine::adapters::formatter::FormatQueuePort + Send + Sync> =
        Arc::new(core_engine::adapters::formatter::FormatQueue::new(
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
    let sub_reg = Arc::new(std::sync::Mutex::new(SubscriptionRegistry::new()));

    // ── Step 5b: discover and load sidecar extensions (spec 28 EV1) ──────────
    //
    // Resolution order:
    //   1. --extensions-dir <path> / $TOWER_EXTENSIONS_DIR (explicit override).
    //   2. Default: XDG global → <workspace>/.tower/extensions (local wins).
    //   3. Back-compat fallback: if .tower/extensions/ absent but .tower/plugins/
    //      exists, use the plugins dir and warn (spec 28 O1).
    let extensions_dir_local = workspace_root.join(".tower/extensions");
    let plugins_dir_legacy = workspace_root.join(".tower/plugins");

    let mut extension_dirs = resolve_extension_dirs(
        extensions_dir_arg().as_deref(),
        env::var("TOWER_EXTENSIONS_DIR").ok().as_deref(),
        global_extensions_dir(),
        &workspace_root,
    );

    // Spec 28 O1: legacy .tower/plugins/ fallback — warn and substitute.
    if let Some((fallback_dir, warning)) =
        config::legacy_plugins_dir_fallback(&extensions_dir_local, &plugins_dir_legacy)
    {
        eprintln!("{warning}");
        // Replace the local scope entry with the legacy plugins dir.
        // `resolve_extension_dirs` with no override produces [global?, local].
        // If the extensions_dir_arg / env override was set, extension_dirs has
        // a single explicit entry and no local-scope fallback is needed.
        if extensions_dir_arg().is_none()
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
    let ext_fs: Arc<dyn core_engine::adapters::extension::host_deps::FsAdapter + Send + Sync> =
        Arc::new(std::sync::Mutex::new(RealFs::new(&workspace_root)));

    let ext_deps = ExtensionHostDeps {
        fs: ext_fs,
        // Cast Arc<dyn AstIndexPort + Send + Sync> → Arc<dyn AstIndexPort>:
        // HostDeps fields do not carry the Send + Sync bounds so we upcast here.
        ast_index: Arc::clone(&ext_ast_index) as Arc<dyn AstIndexPort>,
        format_queue: Arc::clone(&format_queue)
            as Arc<dyn core_engine::adapters::formatter::FormatQueuePort>,
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
    use core_engine::ports::NoOpDocumentSync;
    let doc_sync: Arc<dyn core_engine::ports::DocumentSyncPort + Send + Sync> =
        Arc::new(NoOpDocumentSync);

    // `_watcher` is intentionally bound (not `_`) so the Drop impl runs at the
    // end of `run()` — dropping it earlier would stop live sync.
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
        let replay: Vec<(
            core_engine::domain::FileId,
            core_engine::domain::RelativePath,
        )> = {
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

    // ── Step 6: serve native + extension tools over real stdin/stdout ─────────
    //
    // ExtensionMergedRegistry combines the 9 native tower_* tools with any
    // extension tools namespaced tower_<ext>_<tool>.
    let served_registry = ExtensionMergedRegistry::new(Arc::clone(&state), ext_registry);

    // resource_uris: empty in the extension model — resources/subscribe is driven
    // by extensions through their notify/resourceUpdated host-call.
    let resource_uris: Vec<String> = Vec::new();

    // DiagnosticsReader: no-op in the extension model (diagnostics are pushed
    // via the extension's notify/resourceUpdated, not polled through the pool).
    use core_engine::adapters::mcp::diagnostics::NoOpDiagnosticsReader;
    let diag_reader: Arc<dyn core_engine::adapters::mcp::diagnostics::DiagnosticsReader> =
        Arc::new(NoOpDiagnosticsReader);

    // Build the rmcp handler and serve over stdio.
    let handler = TowerMcpHandler::new(
        served_registry,
        diag_reader,
        Arc::clone(&sub_reg),
        resource_uris,
    );

    // Build a tokio runtime for the rmcp async serve loop.
    // `run()` is synchronous (spawns the watcher and other sync threads before
    // reaching here) so we create a dedicated runtime rather than making `run`
    // async — this avoids restructuring the entire startup path.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("mcp-runtime")
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    rt.block_on(async {
        let running = rmcp::serve_server(handler, rmcp::transport::io::stdio())
            .await
            .map_err(|e| format!("MCP server initialization error: {e}"))?;

        // Spawn the push-forwarding thread now that we have the peer handle.
        spawn_push_task(running.peer().clone(), sub_reg, push_rx);

        // Wait for the client to disconnect (EOF on stdin) or an error.
        running
            .waiting()
            .await
            .map(|_| ())
            .map_err(|e| format!("serve loop I/O error: {e}"))
    })
}

/// Resolve the workspace root directory.
///
/// Priority order (highest first):
/// 1. `--workspace-dir <path>` command-line argument.
/// 2. `TOWER_WORKSPACE` environment variable.
/// 3. Current working directory.
fn resolve_workspace_root() -> std::path::PathBuf {
    // Check for --workspace-dir <path> flag.
    let args: Vec<String> = env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--workspace-dir" {
            return std::path::PathBuf::from(&pair[1]);
        }
    }

    // Fall back to env var.
    if let Ok(val) = env::var("TOWER_WORKSPACE")
        && !val.is_empty()
    {
        return std::path::PathBuf::from(val);
    }

    // Default: current working directory.
    env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Extract the value of the `--extensions-dir <path>` command-line flag, if present.
///
/// Returns `None` when the flag is absent; resolution then falls back to
/// `$TOWER_EXTENSIONS_DIR` and finally the workspace default
/// (`<ws>/.tower/extensions`).
fn extensions_dir_arg() -> Option<String> {
    let args: Vec<String> = env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--extensions-dir" {
            return Some(pair[1].clone());
        }
    }
    None
}
