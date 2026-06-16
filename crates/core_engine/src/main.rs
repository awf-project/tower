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
//! 5. Discover and load WASM plugins (drop & play) across two scopes: resolve the
//!    ordered plugin dirs (`--plugins-dir` / `$TOWER_PLUGINS_DIR` replace the path;
//!    otherwise the XDG global `~/.local/share/tower/plugins` then the project-local
//!    `<root>/.tower/plugins`, local winning on a name collision), load each
//!    `*.wasm` through the isolated-sandbox path (11c/11d) injecting the workspace
//!    `FileSystemPort`, and register the survivors. A missing or empty scope simply
//!    yields no plugins; a single bad plugin is skipped with a stderr warning and
//!    never aborts startup.
//!
//! The binary also exposes a `tower plugin <install|list|remove>` subcommand to
//! manage installed plugins in the local or global scope (`--local` / `--global`,
//! default local); when `argv[1] == "plugin"` it runs that instead of the server.
//! 6. Serve the 7 native `tower_*` tools PLUS any plugin tools (namespaced
//!    `<plugin>/<tool>`) over real `stdin` / `stdout` via a `MergedRegistry`.
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
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use core_engine::adapters::SledStorageAdapter;
use core_engine::adapters::ast_index::{
    XdgAstIndexAdapter, compute_workspace_key, global_ast_workspace_dir, workspace_ast_dir,
};
use core_engine::adapters::config;
use core_engine::adapters::fs::scan::{
    init_towerignore, reconcile_pruned, warn_if_towerignore_absent,
};
use core_engine::adapters::fs::{RealFs, workspace_scan};
use core_engine::adapters::lsp::pool::SessionPool;
use core_engine::adapters::mcp::chain_registry::ChainRegistry;
use core_engine::adapters::mcp::lsp_tools::{LspToolRegistry, SubscriptionRegistry};
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::nav_tools::NavToolRegistry;
use core_engine::adapters::mcp::{MergedRegistry, PushEvent, serve_with_push};
use core_engine::adapters::plugin::{
    DEFAULT_PLUGINS_SUBDIR, HostDeps, IsolationEngine, Scope, global_plugins_dir, install,
    load_plugins_into_registry, production_isolation_config, resolve_plugin_dirs,
};
use core_engine::adapters::watcher::NotifyWatcherAdapter;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::plugin_host::PluginHostRegistry;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::domain::{FileId, RelativePath};
use core_engine::ports::AstIndexPort;
use core_engine::ports::CodeIntelligencePort;
use core_engine::ports::{FileSystemPort, PluginHostPort, StoragePort};

// ── SharedPluginHost ──────────────────────────────────────────────────────────

/// Adapter that delegates [`PluginHostPort`] calls to the shared
/// [`PluginHostRegistry`] through an `Arc<RwLock<_>>`.
///
/// # Decision
///
/// The watcher needs `Box<dyn PluginHostPort + Send + Sync>`, while the MCP
/// serve loop holds `Arc<RwLock<PluginHostRegistry>>`. Rather than cloning the
/// registry (which would break the shared-identity requirement — watcher would
/// fire hooks into a dead copy), we wrap the `Arc` in a thin newtype that
/// delegates through the lock.
///
/// # Trade-off
///
/// Each `on_file_changed` / `on_file_indexed` call takes the registry `RwLock`
/// **read** guard and enqueues onto every subscribed plugin's bounded mailbox;
/// the actual work runs on each plugin's own worker thread. The mailbox send is
/// a blocking, backpressuring send, but since every hot-path consumer also takes
/// a read guard, concurrent readers never contend — MCP reads are never starved.
/// The only thing a full mailbox could block is a future `register()` writer,
/// which is startup-only. Safe to call from the watcher thread.
struct SharedPluginHost(Arc<RwLock<PluginHostRegistry>>);

impl PluginHostPort for SharedPluginHost {
    fn on_file_indexed(&self, id: FileId, path: &RelativePath) {
        // Recover a poisoned lock (wasm trap inside a hook could poison it).
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_indexed(id, path);
    }

    fn on_file_changed(&self, id: FileId, path: &RelativePath) {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.on_file_changed(id, path);
    }
}

fn main() {
    // Subcommand dispatch: `tower plugin <install|list|remove> ...` manages
    // installed plugins instead of starting the MCP server.
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("plugin") {
        if let Err(e) = run_plugin_cli(&args[2..]) {
            eprintln!("tower: {e}");
            std::process::exit(1);
        }
        return;
    }
    // `init` may appear after global flags (e.g. `tower --workspace-dir <p> init`),
    // mirroring how resolve_workspace_root scans all args, so detect it anywhere.
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

/// Handle `tower plugin <install <path> | list | remove <name>> [--local|--global]`.
///
/// `install`/`remove` act on the scope chosen by `--local` / `--global`,
/// defaulting to **local** (`<workspace>/.tower/plugins`); `list` always shows
/// both scopes. These manage plugin **files** by name — see
/// [`core_engine::adapters::plugin::install`].
fn run_plugin_cli(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("install") => {
            let src = first_positional(&args[1..])
                .ok_or("usage: tower plugin install <path-to.wasm> [--local|--global]")?;
            let scope = install::scope_from_flags(args, Scope::Local)?;
            let dir = scope_dir(scope)?;
            let dest = install::install(&dir, Path::new(src))
                .map_err(|e| format!("install failed: {e}"))?;
            println!("installed {src} -> {} ({scope})", dest.display());
            Ok(())
        }
        Some("list") => {
            let global = global_plugins_dir();
            let local = resolve_workspace_root().join(DEFAULT_PLUGINS_SUBDIR);
            let listed = install::list(global.as_deref(), Some(&local));
            if listed.is_empty() {
                println!("no plugins installed");
            } else {
                for p in listed {
                    println!("{:<6} {}", p.scope.to_string(), p.file_name);
                }
            }
            Ok(())
        }
        Some("remove") => {
            let name = first_positional(&args[1..])
                .ok_or("usage: tower plugin remove <name> [--local|--global]")?;
            let scope = install::scope_from_flags(args, Scope::Local)?;
            let dir = scope_dir(scope)?;
            if install::remove(&dir, name).map_err(|e| format!("remove failed: {e}"))? {
                println!("removed '{name}' from {} ({scope})", dir.display());
            } else {
                println!("no such {scope} plugin: '{name}'");
            }
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown plugin subcommand '{other}' (expected: install | list | remove)"
        )),
        None => Err(
            "usage: tower plugin <install <path> | list | remove <name>> [--local|--global]"
                .to_string(),
        ),
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

/// The directory backing a [`Scope`]: local is `<workspace>/.tower/plugins`,
/// global is the XDG data dir (absent only without HOME/XDG base dirs).
fn scope_dir(scope: Scope) -> Result<PathBuf, String> {
    match scope {
        Scope::Local => Ok(resolve_workspace_root().join(DEFAULT_PLUGINS_SUBDIR)),
        Scope::Global => global_plugins_dir().ok_or_else(|| {
            "cannot determine the global plugins directory (no HOME/XDG base dir)".to_string()
        }),
    }
}

/// First non-flag argument (an argument not starting with `--`), so the path /
/// name can appear before or after the scope flag.
fn first_positional(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
}

fn run() -> Result<(), String> {
    // ── Step 1: resolve workspace root ────────────────────────────────────────
    let workspace_root = resolve_workspace_root();

    // Load the local project config (.tower/config.toml) early so a malformed
    // file fails fast before any DB or watcher work. Absent file → defaults.
    let tower_config = config::load(&workspace_root).map_err(|e| e.to_string())?;

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

    // ── Step 5: discover and load WASM plugins (global + local, drop & play) ──
    // Explicit --plugins-dir / $TOWER_PLUGINS_DIR replace the search path; otherwise
    // the XDG global scope is scanned first and the project-local scope last, so a
    // local plugin shadows a global one of the same name.
    let plugin_dirs = resolve_plugin_dirs(
        plugins_dir_arg().as_deref(),
        env::var("TOWER_PLUGINS_DIR").ok().as_deref(),
        global_plugins_dir(),
        &workspace_root,
    );

    // The IsolationEngine owns the background epoch ticker; it must outlive the
    // serve loop so per-call epoch deadlines keep firing. Held in `run`'s scope.
    let isolation_engine = IsolationEngine::new()
        .map_err(|e| format!("failed to initialise plugin isolation engine: {e}"))?;

    // Plugins read the real workspace through their own FileSystemPort (the same
    // capability the native tools use), satisfying host_read_file.
    let plugin_fs: Arc<dyn FileSystemPort + Send + Sync> = Arc::new(RealFs::new(&workspace_root));

    // ── AST index: resolve the per-workspace XDG data directory ──────────────
    //
    // Decision: fall back to `<workspace_root>/.tower/ast` when
    // `global_ast_workspace_dir()` returns None (no HOME/XDG_DATA_HOME, e.g.
    // a headless CI container without $HOME set).
    //
    // Why: silently falling back to a workspace-local directory keeps the
    // binary usable in constrained environments. The data is a pure cache
    // (re-derivable by the plugin); losing XDG isolation in those environments
    // is acceptable.
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
    let plugin_ast_index: Arc<dyn AstIndexPort + Send + Sync> =
        Arc::new(XdgAstIndexAdapter::new(ast_base_dir));

    // Extract the workspace Arc from EngineState so plugins see the same live
    // workspace as the MCP handlers and the watcher (same Arc, same RwLock).
    let plugin_workspace = state
        .read()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .workspace_arc();

    // Real format queue: workers run the external formatters declared in
    // .tower/config.toml and share their echo-suppression set with the registry
    // (loop break). No threads start until host_request_format enqueues a job.
    let format_queue: Arc<dyn core_engine::adapters::formatter::FormatQueuePort + Send + Sync> =
        Arc::new(core_engine::adapters::formatter::FormatQueue::new(
            tower_config.plugins.formatter.clone(),
            workspace_root.clone(),
        ));

    // Capture the formatter echo set before `format_queue` is moved into the
    // plugin deps. Shared with the watcher so formatter-induced writes do not
    // trigger a spurious `didChange` to the language server (spec 14b UN1).
    let echo_set = format_queue
        .shared_echo_set()
        .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())));

    let plugin_deps = HostDeps {
        fs: plugin_fs,
        ast_index: plugin_ast_index,
        workspace: plugin_workspace,
        format_queue,
    };

    let plugin_host = load_plugins_into_registry(
        &plugin_dirs,
        &isolation_engine,
        plugin_deps,
        production_isolation_config(),
        &tower_config.plugins.disabled,
    );
    let plugin_count = plugin_host.declared_tools().len();
    if plugin_count > 0 {
        let scopes: Vec<String> = plugin_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        eprintln!(
            "tower: loaded {plugin_count} plugin tool(s) from [{}]",
            scopes.join(", ")
        );
    }
    let plugin_host = Arc::new(RwLock::new(plugin_host));

    // Inject the real plugin host into the shared EngineState so MCP-driven
    // mutations (create/delete/global_replace) broadcast `on_file_changed` to
    // plugins. Without this the handlers would keep their no-op default and the
    // AST plugin's cross-file index would go stale after MCP deletes/edits.
    {
        let host: Arc<dyn PluginHostPort + Send + Sync> =
            Arc::new(SharedPluginHost(Arc::clone(&plugin_host)));
        state
            .write()
            .map_err(|_| "engine state lock poisoned".to_string())?
            .set_plugin_host(host);
    }

    // ── Step 5a-bis: push bridge + session pool ────────────────────────────────
    //
    // Decision 3: unified blocking ServeInput channel. Architecture:
    //
    //   LspClientAdapter dispatcher
    //     --DiagnosticsEvent(mpsc::Sender)--> diag_rx
    //       --PushEvent--> serve_with_push unified channel (internal)
    //
    // The forwarder thread (diag_rx → push_tx) is spawned here. serve_with_push
    // receives push_rx and internally bridges Push events into the unified channel.
    use core_engine::adapters::lsp::DiagnosticsEvent;
    use core_engine::adapters::lsp::pool::RealSpawner;

    let (diag_tx, diag_rx) = std::sync::mpsc::channel::<DiagnosticsEvent>();
    let (push_tx, push_rx) = std::sync::mpsc::channel::<PushEvent>();
    let sub_reg = Arc::new(std::sync::Mutex::new(SubscriptionRegistry::new()));

    // Forwarder: DiagnosticsEvent → PushEvent.
    // Exits cleanly when diag_rx disconnects (SessionPool dropped or all sessions evicted).
    std::thread::spawn(move || {
        while let Ok(event) = diag_rx.recv() {
            // Ignore send error: MCP serve loop may have already exited.
            let _ = push_tx.send(PushEvent {
                uri: event.uri,
                generation: event.generation,
            });
        }
    });

    // Build the pool with push_tx so each spawned session gets a clone of the
    // diagnostics sender wired at spawn time (RealSpawner → LspClientAdapter::spawn).
    let lsp_pool = Arc::new(SessionPool::with_spawner(
        tower_config.lsp.clone(),
        workspace_root.clone(),
        Arc::new(RealSpawner),
        Some(diag_tx),
    ));

    // ── Step 5b: spawn the filesystem watcher for live VFS sync ───────────────
    //
    // Decision: always-on, fail-fast at startup.
    // Why: live VFS sync is now a core feature (spec 06), not optional. A watcher
    // that fails to initialise means FS events will never reach the index —
    // silently serving stale data is worse than a clear startup error.
    //
    // Trade-off: if the host OS notify backend is unavailable (e.g. certain
    // container environments with no inotify), the server will refuse to start.
    // An operator that truly wants a read-only / scan-only mode can be added
    // later with a `--no-watch` flag; for now the binary makes the correct
    // default: always live.
    //
    // Watcher state sharing:
    //   workspace + index — Arc clones from EngineState (same instances as MCP)
    //   storage          — try_clone() of the sled adapter (same Db, no re-open)
    //   plugin_host      — SharedPluginHost wrapping Arc<RwLock<PluginHostRegistry>>
    let watcher_workspace = state
        .read()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .workspace_arc();
    let watcher_index = state
        .read()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .index_arc();
    let watcher_plugin_host = Box::new(SharedPluginHost(Arc::clone(&plugin_host)));

    // 14b: the watcher mirrors live file edits into the language server. The
    // pool's DocumentSyncPort impl routes events to the appropriate per-language
    // session, and the formatter echo set prevents formatter writes from
    // churning diagnostics.
    let doc_sync: Arc<dyn core_engine::ports::DocumentSyncPort + Send + Sync> =
        Arc::clone(&lsp_pool) as _;

    // `_watcher` is intentionally bound (not `_`) so the Drop impl runs at the
    // end of `run()` — dropping it earlier would stop live sync.
    let _watcher = NotifyWatcherAdapter::with_document_sync(
        workspace_root.clone(),
        watcher_workspace,
        watcher_index,
        Box::new(storage_for_watcher),
        watcher_plugin_host,
        doc_sync,
        Arc::clone(&echo_set),
    )
    .map_err(|e| format!("failed to start filesystem watcher: {e}"))?;

    eprintln!(
        "tower: filesystem watcher active on {}",
        workspace_root.display()
    );

    // ── Step 6: serve native + plugin tools over real stdin/stdout ────────────
    // MergedRegistry exposes the 7 native tower_* tools plus namespaced plugin
    // tools. A missing/empty plugins dir leaves it serving exactly the natives.
    // Lock stdin/stdout for the duration of the serve loop.
    // BufReader/BufWriter ensure line-oriented I/O matches the framing spec.
    let merged_registry = MergedRegistry::new(Arc::clone(&state), plugin_host);

    // ── Step 6b: code-intelligence + navigation tools from the session pool ───
    //
    // The pool implements all three port traits. When no language is configured,
    // or when an extension is not handled, it returns Unsupported — identical
    // behaviour to the old None/InMemoryCodeIntel path.
    let code_intel: Arc<dyn CodeIntelligencePort> = Arc::clone(&lsp_pool) as _;
    let nav: Option<Arc<dyn core_engine::ports::NavigationPort>> = Some(Arc::clone(&lsp_pool) as _);

    let lsp_registry = LspToolRegistry::new(Arc::clone(&state), code_intel);
    let nav_registry = NavToolRegistry::new(Arc::clone(&state), nav);

    // Compose: ChainRegistry tries merged → tower_lsp_diagnostics → navigation.
    // `list` concatenates all surfaces; `call` routes by first-non-NotFound.
    let mut served_registry = ChainRegistry::new(vec![
        Box::new(merged_registry),
        Box::new(lsp_registry),
        Box::new(nav_registry),
    ]);

    // resource_uris: static list from config (one entry per language).
    // diag_reader: pulls last-published diagnostics for resources/read (AC6).
    let resource_uris: Vec<String> = lsp_pool.resource_uris();
    let diag_reader: Arc<dyn core_engine::adapters::lsp::pool::DiagnosticsReader> =
        Arc::clone(&lsp_pool) as _;

    // serve_with_push requires R: Send + 'static (the reader moves into a thread).
    // BufReader<Stdin> satisfies both; StdinLock<'_> does not (lifetime).
    let stdout = std::io::stdout();
    serve_with_push(
        BufReader::new(std::io::stdin()),
        BufWriter::new(stdout.lock()),
        &mut served_registry,
        sub_reg,
        diag_reader,
        resource_uris,
        Some(push_rx),
    )
    .map_err(|e| format!("serve loop I/O error: {e}"))
}

/// Resolve the workspace root directory.
///
/// Priority order (highest first):
/// 1. `--workspace-dir <path>` command-line argument.
/// 2. `TOWER_WORKSPACE` environment variable.
/// 3. Current working directory.
fn resolve_workspace_root() -> PathBuf {
    // Check for --workspace-dir <path> flag.
    let args: Vec<String> = env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--workspace-dir" {
            return PathBuf::from(&pair[1]);
        }
    }

    // Fall back to env var.
    if let Ok(val) = env::var("TOWER_WORKSPACE")
        && !val.is_empty()
    {
        return PathBuf::from(val);
    }

    // Default: current working directory.
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Extract the value of the `--plugins-dir <path>` command-line flag, if present.
///
/// Returns `None` when the flag is absent; resolution then falls back to
/// `$TOWER_PLUGINS_DIR` and finally the workspace default (see
/// [`resolve_plugins_dir`]).
fn plugins_dir_arg() -> Option<String> {
    let args: Vec<String> = env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--plugins-dir" {
            return Some(pair[1].clone());
        }
    }
    None
}
