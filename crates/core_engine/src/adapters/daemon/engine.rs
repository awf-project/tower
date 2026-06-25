//! Engine assembly: open Sled, scan, build shared state, load extensions,
//! start the watcher. Extracted from the old `main.rs::run` so both the daemon
//! and (Phase A) the stdio path share one setup. Serving is the caller's job.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, mpsc};

use crate::adapters::SledStorageAdapter;
use crate::adapters::ast_index::{
    XdgAstIndexAdapter, compute_workspace_key, global_ast_workspace_dir, workspace_ast_dir,
};
use crate::adapters::cli::{GlobalOpts, resolve_extensions_dir_arg, resolve_workspace_root};
use crate::adapters::config::{TowerConfig, legacy_plugins_dir_fallback};
use crate::adapters::extension::host_deps::ApplyEditsHostPort;
use crate::adapters::extension::{
    ExtensionInitConfigMap, ExtensionSupervisor, HostDeps as ExtensionHostDeps,
    global_extensions_dir, load_extensions_into_shared_registry, resolve_extension_dirs,
};
use crate::adapters::fs::scan::reconcile_pruned;
use crate::adapters::fs::{RealFs, workspace_scan};
use crate::adapters::mcp::PushEvent;
use crate::adapters::mcp::diagnostics::{DiagnosticsReader, NoOpDiagnosticsReader};
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::watcher::NotifyWatcherAdapter;
use crate::domain::DomainError;
use crate::domain::extension_host::{ExtensionRegistry, RegistrationError};
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::FileMutationService;
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{FileId, RelativePath};
use crate::ports::inbound::{
    FileMutationUseCase, WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult,
};
use crate::ports::{AstIndexPort, ExtensionHostPort, NoOpDocumentSync, StoragePort};
use extension_protocol::ExtensionManifest;

type SharedFormatQueue = Arc<dyn crate::adapters::formatter::FormatQueuePort + Send + Sync>;
type FormatterEchoSet = Arc<Mutex<HashMap<String, ()>>>;
const BUNDLED_DEBUG_MANIFEST: &str = include_str!("../../../../../extensions/debug/extension.toml");
const RR_DEBUG_TOOL_NAMES: &[&str] = &[
    "record",
    "replay",
    "reverse_continue",
    "step_back",
    "watchpoint",
    "traces",
    "delete_trace",
    "find_origin",
    "record_and_find_origin",
];

struct EngineApplyEditsHost {
    state: Arc<RwLock<EngineState>>,
}

#[derive(Default)]
struct DeferredExtensionHost {
    changed: Mutex<Vec<(FileId, RelativePath)>>,
}

impl DeferredExtensionHost {
    fn drain_changed(&self) -> Vec<(FileId, RelativePath)> {
        self.changed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }
}

impl ExtensionHostPort for DeferredExtensionHost {
    fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

    fn on_file_changed(&self, id: FileId, path: &RelativePath) {
        self.changed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((id, path.clone()));
    }

    fn on_file_deleted(&self, _path: &RelativePath) {}

    fn declared_tools(
        &self,
    ) -> Vec<(
        crate::domain::extension_host::ExtensionId,
        extension_protocol::ToolDecl,
    )> {
        Vec::new()
    }

    fn invoke(
        &self,
        tool_name: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, crate::domain::extension_host::InvokeError> {
        Err(crate::domain::extension_host::InvokeError::ToolNotFound(
            tool_name.to_owned(),
        ))
    }
}

impl EngineApplyEditsHost {
    fn new(state: Arc<RwLock<EngineState>>) -> Self {
        Self { state }
    }

    fn with_mutation_service<T>(
        &self,
        f: impl FnOnce(&mut FileMutationService<'_>) -> Result<T, DomainError>,
    ) -> Result<T, DomainError> {
        let deferred_host = DeferredExtensionHost::default();
        let (result, extension_host) = {
            let mut guard = self
                .state
                .write()
                .map_err(|_| DomainError::IoError("engine state lock poisoned".to_owned()))?;
            let ws_arc = Arc::clone(&guard.workspace);
            let idx_arc = Arc::clone(&guard.index);
            let mut ws = ws_arc
                .write()
                .map_err(|_| DomainError::IoError("workspace lock poisoned".to_owned()))?;
            let mut idx = idx_arc
                .write()
                .map_err(|_| DomainError::IoError("index lock poisoned".to_owned()))?;
            let extension_host = guard.extension_host();
            let engine = &mut *guard;
            let mut svc = FileMutationService::new(
                engine.fs.as_mut(),
                &mut ws,
                &mut idx,
                engine.storage.as_mut(),
                &deferred_host,
            );
            (f(&mut svc), extension_host)
        };

        for (file_id, path) in deferred_host.drain_changed() {
            extension_host.on_file_changed(file_id, &path);
        }

        result
    }
}

impl ApplyEditsHostPort for EngineApplyEditsHost {
    fn apply_batch_edits(
        &self,
        request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        self.with_mutation_service(|svc| svc.apply_batch_edits(request))
    }
}

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

fn open_storage(
    workspace_root: &Path,
) -> Result<(SledStorageAdapter, ProjectWorkspace, InvertedIndex), String> {
    let db_path = workspace_root.join(".tower").join("db");
    std::fs::create_dir_all(&db_path)
        .map_err(|e| format!("failed to create db dir {}: {e}", db_path.display()))?;

    SledStorageAdapter::open(&db_path)
        .map_err(|e| format!("failed to open sled database at {}: {e}", db_path.display()))
}

fn load_workspace_index(
    workspace_root: &Path,
    storage: &mut SledStorageAdapter,
    workspace: ProjectWorkspace,
    index: InvertedIndex,
) -> (ProjectWorkspace, InvertedIndex) {
    if !storage.is_scan_complete().unwrap_or(false) {
        let mut fresh_workspace = ProjectWorkspace::new();
        let mut fresh_index = InvertedIndex::new();
        match workspace_scan(
            workspace_root,
            storage,
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
        return (fresh_workspace, fresh_index);
    }

    let mut workspace = workspace;
    let mut index = index;
    let pruned = reconcile_pruned(workspace_root, &mut workspace, &mut index, storage);
    if pruned > 0 {
        eprintln!("tower: reconciled index — {pruned} stale entries pruned");
    }
    (workspace, index)
}

fn ast_index_for_workspace(workspace_root: &Path) -> Arc<dyn AstIndexPort + Send + Sync> {
    let ast_base_dir = match global_ast_workspace_dir() {
        Some(_) => workspace_ast_dir(&compute_workspace_key(workspace_root)),
        None => {
            eprintln!(
                "tower: warning — XDG data directory unavailable; \
                 AST index will be stored in <workspace>/.tower/ast"
            );
            workspace_root.join(".tower").join("ast")
        }
    };
    Arc::new(XdgAstIndexAdapter::new(ast_base_dir))
}

fn format_queue_for_workspace(
    workspace_root: &Path,
    tower_config: &TowerConfig,
) -> (SharedFormatQueue, FormatterEchoSet) {
    let format_queue: SharedFormatQueue = Arc::new(crate::adapters::formatter::FormatQueue::new(
        tower_config.plugins.formatter.clone(),
        workspace_root.to_path_buf(),
    ));
    let echo_set = format_queue
        .shared_echo_set()
        .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new())));
    (format_queue, echo_set)
}

fn load_extension_registry(
    opts: &GlobalOpts,
    workspace_root: &Path,
    tower_config: &TowerConfig,
    state: &Arc<RwLock<EngineState>>,
    ext_registry: &Arc<RwLock<ExtensionRegistry>>,
    ext_ast_index: Arc<dyn AstIndexPort + Send + Sync>,
    format_queue: Arc<dyn crate::adapters::formatter::FormatQueuePort + Send + Sync>,
) -> Result<mpsc::Receiver<PushEvent>, String> {
    let (push_tx, push_rx) = mpsc::channel::<PushEvent>();
    let extensions_dir_local = workspace_root.join(".tower/extensions");
    let plugins_dir_legacy = workspace_root.join(".tower/plugins");
    let ext_dir_arg = resolve_extensions_dir_arg(opts);

    let mut extension_dirs = resolve_extension_dirs(
        ext_dir_arg.as_deref(),
        env::var("TOWER_EXTENSIONS_DIR").ok().as_deref(),
        global_extensions_dir(),
        workspace_root,
    );

    if let Some((fallback_dir, warning)) =
        legacy_plugins_dir_fallback(&extensions_dir_local, &plugins_dir_legacy)
    {
        eprintln!("{warning}");
        if ext_dir_arg.is_none()
            && env::var("TOWER_EXTENSIONS_DIR")
                .unwrap_or_default()
                .is_empty()
        {
            for dir in &mut extension_dirs {
                if dir == &extensions_dir_local {
                    *dir = fallback_dir.clone();
                }
            }
        }
    }

    let ext_fs: Arc<dyn crate::adapters::extension::host_deps::FsAdapter + Send + Sync> =
        Arc::new(Mutex::new(RealFs::new(workspace_root)));

    let ext_deps = ExtensionHostDeps {
        fs: ext_fs,
        ast_index: ext_ast_index as Arc<dyn AstIndexPort>,
        format_queue: format_queue as Arc<dyn crate::adapters::formatter::FormatQueuePort>,
        apply_edits: Arc::new(EngineApplyEditsHost::new(Arc::clone(state))),
        push_tx: Some(push_tx),
    };

    let mut init_configs = ExtensionInitConfigMap::new();
    if let Some(debug_config) = tower_config.debug.for_extension_initialize() {
        init_configs.insert("debug".to_owned(), debug_config);
    }

    load_extensions_into_shared_registry(
        ext_registry,
        &extension_dirs,
        ext_deps.clone(),
        tower_config.extensions.request_timeout(),
        &tower_config.extensions.disabled,
        &init_configs,
    );

    register_bundled_debug_extension(
        ext_registry,
        ext_deps,
        tower_config.extensions.request_timeout(),
        tower_config,
    )?;
    let ext_tool_count = ext_registry
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .declared_tools()
        .len();
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
    Ok(push_rx)
}

fn register_bundled_debug_extension(
    ext_registry: &Arc<RwLock<ExtensionRegistry>>,
    ext_deps: ExtensionHostDeps,
    request_timeout: std::time::Duration,
    tower_config: &TowerConfig,
) -> Result<(), String> {
    if tower_config.debug.is_empty()
        || tower_config
            .extensions
            .disabled
            .iter()
            .any(|name| name == "debug")
    {
        return Ok(());
    }

    let mut manifest: ExtensionManifest = match toml::from_str(BUNDLED_DEBUG_MANIFEST) {
        Ok(manifest) => manifest,
        Err(err) => {
            return Err(format!("invalid bundled debug extension manifest: {err}"));
        }
    };

    if let Some(command) = debug_extension_binary_path()
        && let Some(argv0) = manifest.command.first_mut()
    {
        *argv0 = command;
    }

    if tower_config
        .debug
        .record
        .as_ref()
        .is_none_or(|record| record.backend != "rr")
    {
        manifest
            .tools
            .retain(|tool| !RR_DEBUG_TOOL_NAMES.contains(&tool.name.as_str()));
    }

    let instance = Box::new(ExtensionSupervisor::new(
        manifest,
        ext_deps,
        request_timeout,
        tower_config.debug.for_extension_initialize(),
    ));
    let mut guard = ext_registry
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    match guard.register(instance) {
        Ok(()) | Err(RegistrationError::DuplicateName(_)) => Ok(()),
    }
}

fn debug_extension_binary_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let bin_dir = if exe_dir.file_name().is_some_and(|name| name == "deps") {
        exe_dir.parent()?
    } else {
        exe_dir
    };
    let bin = bin_dir.join(format!("debug_extension{}", std::env::consts::EXE_SUFFIX));
    bin.exists().then(|| bin.to_string_lossy().into_owned())
}

fn inject_extension_host(
    state: &Arc<RwLock<EngineState>>,
    ext_registry: &Arc<RwLock<ExtensionRegistry>>,
) -> Result<(), String> {
    let host: Arc<dyn ExtensionHostPort + Send + Sync> =
        Arc::new(SharedExtensionHost(Arc::clone(ext_registry)));
    state
        .write()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .set_plugin_host(host);
    Ok(())
}

fn start_watcher(
    workspace_root: &Path,
    state: &Arc<RwLock<EngineState>>,
    storage_for_watcher: SledStorageAdapter,
    ext_registry: &Arc<RwLock<ExtensionRegistry>>,
    echo_set: FormatterEchoSet,
) -> Result<NotifyWatcherAdapter, String> {
    let watcher_workspace = state
        .read()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .workspace_arc();
    let watcher_index = state
        .read()
        .map_err(|_| "engine state lock poisoned".to_string())?
        .index_arc();
    let watcher_ext_host: Box<dyn ExtensionHostPort + Send + Sync> =
        Box::new(SharedExtensionHost(Arc::clone(ext_registry)));
    let doc_sync: Arc<dyn crate::ports::DocumentSyncPort + Send + Sync> =
        Arc::new(NoOpDocumentSync);

    let watcher = NotifyWatcherAdapter::with_document_sync(
        workspace_root.to_path_buf(),
        watcher_workspace,
        watcher_index,
        Box::new(storage_for_watcher),
        watcher_ext_host,
        doc_sync,
        echo_set,
    )
    .map_err(|e| format!("failed to start filesystem watcher: {e}"))?;

    eprintln!(
        "tower: filesystem watcher active on {}",
        workspace_root.display()
    );
    Ok(watcher)
}

fn replay_initial_index_to_extensions(
    state: &Arc<RwLock<EngineState>>,
    ext_registry: &Arc<RwLock<ExtensionRegistry>>,
) -> Result<(), String> {
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
    if replay.is_empty() {
        return Ok(());
    }

    eprintln!(
        "tower: delivering {} indexed file(s) to extensions (background)",
        replay.len()
    );
    let host = SharedExtensionHost(Arc::clone(ext_registry));
    std::thread::Builder::new()
        .name("ext-initial-index".to_owned())
        .spawn(move || {
            for (id, path) in replay {
                host.on_file_indexed(id, &path);
            }
        })
        .map_err(|e| format!("failed to spawn initial-index thread: {e}"))?;
    Ok(())
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
    let (mut storage, workspace, index) = open_storage(&workspace_root)?;
    let (workspace, index) = load_workspace_index(&workspace_root, &mut storage, workspace, index);
    let storage_for_watcher = storage.try_clone();
    let state = Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(storage),
        Box::new(RealFs::new(&workspace_root)),
    )));

    let ext_ast_index = ast_index_for_workspace(&workspace_root);
    let (format_queue, echo_set) = format_queue_for_workspace(&workspace_root, &tower_config);
    let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
    inject_extension_host(&state, &ext_registry)?;
    let push_rx = load_extension_registry(
        opts,
        &workspace_root,
        &tower_config,
        &state,
        &ext_registry,
        ext_ast_index,
        format_queue,
    )?;
    let _watcher = start_watcher(
        &workspace_root,
        &state,
        storage_for_watcher,
        &ext_registry,
        echo_set,
    )?;
    replay_initial_index_to_extensions(&state, &ext_registry)?;
    let diag_reader: Arc<dyn DiagnosticsReader> = Arc::new(NoOpDiagnosticsReader);

    Ok(EngineHandle {
        state,
        ext_registry,
        push_rx,
        diag_reader,
        _watcher,
    })
}
