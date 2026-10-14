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
use crate::adapters::config::TowerConfig;
use crate::adapters::config::lsp::{lsp_initialize_payload, validate_lsp_commands};
use crate::adapters::extension::host_deps::ApplyEditsHostPort;
use crate::adapters::extension::{
    ExtensionInitConfigMap, ExtensionSupervisor, HostDeps as ExtensionHostDeps,
    global_extensions_dir, load_extensions_into_shared_registry, resolve_extension_dirs,
};
use crate::adapters::fs::RealFs;
#[cfg(any(test, feature = "startup-test-hooks"))]
use crate::adapters::fs::scan::ReconciliationMetrics;
#[cfg(not(feature = "startup-test-hooks"))]
use crate::adapters::fs::scan::reconcile_workspace;
use crate::adapters::fs::scan::{ReconciliationReport, ScanError};
#[cfg(feature = "startup-test-hooks")]
use crate::adapters::fs::scan::{
    collect_reconciliation_metrics, reconcile_workspace_before_commit,
};
use crate::adapters::mcp::PushEvent;
use crate::adapters::mcp::diagnostics::{DiagnosticsReader, NoOpDiagnosticsReader};
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::watcher::{NotifyWatcherAdapter, WatcherError};
use crate::domain::DomainError;
use crate::domain::extension_host::{ExtensionRegistry, RegistrationError};
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::FileMutationService;
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{FileId, RelativePath};
#[cfg(feature = "startup-test-hooks")]
use crate::ports::PortError;
use crate::ports::inbound::{
    FileMutationUseCase, WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult,
};
use crate::ports::{AstIndexPort, ExtensionHostPort, NoOpDocumentSync};
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

#[cfg(feature = "startup-test-hooks")]
#[derive(Debug, Default)]
struct StartupTestHooks {
    fail_initial_reconciliation_before_commit: bool,
    emit_reconciliation_metrics: bool,
}

#[cfg(feature = "startup-test-hooks")]
impl StartupTestHooks {
    fn for_workspace(workspace_root: &Path) -> Self {
        let Some(selector) = env::var_os("TOWER_TEST_WORKSPACE") else {
            return Self::default();
        };
        let Ok(selector) = Path::new(&selector).canonicalize() else {
            return Self::default();
        };
        let Ok(workspace_root) = workspace_root.canonicalize() else {
            return Self::default();
        };
        if selector != workspace_root {
            return Self::default();
        }

        Self {
            fail_initial_reconciliation_before_commit: env::var("TOWER_TEST_FAIL_RECONCILIATION")
                .is_ok_and(|value| value == "before_commit"),
            emit_reconciliation_metrics: env::var("TOWER_TEST_METRICS")
                .is_ok_and(|value| value == "1"),
        }
    }
}

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

struct WorkspaceLoadResult {
    workspace: ProjectWorkspace,
    index: InvertedIndex,
    reconciliation: ReconciliationReport,
}

#[derive(Debug)]
enum WorkspaceLoadError {
    Reconciliation(ScanError),
    Watcher {
        operation: &'static str,
        source: WatcherError,
    },
}

impl WorkspaceLoadError {
    fn operation(&self) -> &'static str {
        match self {
            Self::Reconciliation(ScanError::MarkerRead { .. }) => "marker_read",
            Self::Reconciliation(ScanError::Enumeration { .. }) => "enumeration",
            Self::Reconciliation(ScanError::Metadata { .. }) => "metadata_read",
            Self::Reconciliation(ScanError::ContentRead { .. }) => "content_read",
            Self::Reconciliation(ScanError::Persistence { operation, .. }) => operation,
            Self::Watcher { operation, .. } => operation,
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Self::Reconciliation(
                ScanError::Enumeration { path, .. }
                | ScanError::Metadata { path, .. }
                | ScanError::ContentRead { path, .. },
            ) => Some(path),
            Self::Reconciliation(ScanError::MarkerRead { .. } | ScanError::Persistence { .. })
            | Self::Watcher { .. } => None,
        }
    }

    fn redacted_reason(&self) -> &'static str {
        match self {
            Self::Reconciliation(ScanError::MarkerRead { .. }) => "scan marker read failed",
            Self::Reconciliation(ScanError::Enumeration { .. }) => "workspace enumeration failed",
            Self::Reconciliation(ScanError::Metadata { .. }) => "file metadata read failed",
            Self::Reconciliation(ScanError::ContentRead { .. }) => "file content read failed",
            Self::Reconciliation(ScanError::Persistence { .. }) => {
                "reconciliation persistence failed"
            }
            Self::Watcher {
                source: WatcherError::ChannelClosed,
                ..
            } => "watcher channel closed",
            Self::Watcher {
                source: WatcherError::NotifyInit(_),
                ..
            } => "watcher initialization failed",
            Self::Watcher {
                source: WatcherError::NotifyWatch(_),
                ..
            } => "watcher registration failed",
            Self::Watcher {
                source: WatcherError::Storage(_),
                ..
            } => "watcher storage operation failed",
            Self::Watcher {
                source: WatcherError::SpawnFailed(_),
                ..
            } => "watcher worker startup failed",
        }
    }
}

impl std::fmt::Display for WorkspaceLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tower: workspace reconciliation failed during {}",
            self.operation()
        )?;
        if let Some(path) = self.path() {
            write!(formatter, " at {}", path.display())?;
        }
        write!(formatter, ": {}", self.redacted_reason())
    }
}

impl std::error::Error for WorkspaceLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reconciliation(source) => Some(source),
            Self::Watcher { source, .. } => Some(source),
        }
    }
}

fn load_workspace_index(
    workspace_root: &Path,
    storage: &mut SledStorageAdapter,
    workspace: ProjectWorkspace,
    index: InvertedIndex,
    #[cfg(feature = "startup-test-hooks")] hooks: &StartupTestHooks,
) -> Result<WorkspaceLoadResult, WorkspaceLoadError> {
    let mut workspace = workspace;
    let mut index = index;
    #[cfg(not(feature = "startup-test-hooks"))]
    let reconciliation = reconcile_workspace(workspace_root, storage, &mut workspace, &mut index);
    #[cfg(feature = "startup-test-hooks")]
    let reconciliation = reconcile_workspace_before_commit(
        workspace_root,
        storage,
        &mut workspace,
        &mut index,
        || {
            if hooks.fail_initial_reconciliation_before_commit {
                Err(ScanError::Persistence {
                    operation: "apply_reconciliation_delta",
                    source: PortError::ReconciliationFailed {
                        operation: "apply_reconciliation_delta",
                        reason: "injected startup reconciliation failure".to_owned(),
                    },
                })
            } else {
                Ok(())
            }
        },
    );
    let reconciliation = reconciliation.map_err(WorkspaceLoadError::Reconciliation)?;
    Ok(WorkspaceLoadResult {
        workspace,
        index,
        reconciliation,
    })
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
        tower_config.formatter.clone(),
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
    let ext_dir_arg = resolve_extensions_dir_arg(opts);

    let extension_dirs = resolve_extension_dirs(
        ext_dir_arg.as_deref(),
        env::var("TOWER_EXTENSIONS_DIR").ok().as_deref(),
        global_extensions_dir(),
        workspace_root,
    );

    let ext_fs: Arc<dyn crate::adapters::extension::host_deps::FsAdapter + Send + Sync> =
        Arc::new(Mutex::new(RealFs::new(workspace_root)));

    let ext_deps = ExtensionHostDeps {
        fs: ext_fs,
        ast_index: ext_ast_index as Arc<dyn AstIndexPort>,
        format_queue: format_queue as Arc<dyn crate::adapters::formatter::FormatQueuePort>,
        apply_edits: Arc::new(EngineApplyEditsHost::new(Arc::clone(state))),
        push_tx: Some(push_tx),
    };

    let init_configs = extension_init_configs(tower_config);

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

fn extension_init_configs(tower_config: &TowerConfig) -> ExtensionInitConfigMap {
    let mut init_configs = ExtensionInitConfigMap::new();
    init_configs.insert("lsp".to_owned(), lsp_initialize_payload(&tower_config.lsp));
    if let Some(debug_config) = tower_config.debug.for_extension_initialize() {
        init_configs.insert("debug".to_owned(), debug_config);
    }
    init_configs
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
) -> Result<NotifyWatcherAdapter, WatcherError> {
    let watcher_workspace = state
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .workspace_arc();
    let watcher_index = state
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
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
    )?;

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
        "tower: delivering {} indexed file(s) to extensions",
        replay.len()
    );
    let host = SharedExtensionHost(Arc::clone(ext_registry));
    for (id, path) in replay {
        host.on_file_indexed(id, &path);
    }
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
/// loading it via `config::load` before calling this function.
pub fn build_engine(opts: &GlobalOpts, tower_config: TowerConfig) -> Result<EngineHandle, String> {
    let workspace_root = resolve_workspace_root(opts);
    #[cfg(feature = "startup-test-hooks")]
    let startup_test_hooks = StartupTestHooks::for_workspace(&workspace_root);
    for outcome in validate_lsp_commands(
        &tower_config.lsp,
        &workspace_root,
        std::env::var_os("PATH").as_deref(),
    ) {
        let diagnostic = serde_json::json!({ "outcome": outcome });
        eprintln!("tower: LSP startup diagnostic {diagnostic}");
    }
    let (mut storage, workspace, index) = open_storage(&workspace_root)?;
    #[cfg(feature = "startup-test-hooks")]
    let rewrite_counter = storage.reconciliation_rewrite_counter();
    #[cfg(feature = "startup-test-hooks")]
    let rewrite_count_before_prepare = rewrite_counter.snapshot();
    let ext_ast_index = ast_index_for_workspace(&workspace_root);
    let (format_queue, echo_set) = format_queue_for_workspace(&workspace_root, &tower_config);
    let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
    let prepare = || -> Result<_, String> {
        let WorkspaceLoadResult {
            workspace,
            index,
            reconciliation,
        } = load_workspace_index(
            &workspace_root,
            &mut storage,
            workspace,
            index,
            #[cfg(feature = "startup-test-hooks")]
            &startup_test_hooks,
        )
        .map_err(|error| format!("{error} at {}", workspace_root.display()))?;
        eprintln!(
            "tower: reconciled index — {} added, {} removed, {} unchanged",
            reconciliation.added, reconciliation.removed, reconciliation.unchanged
        );
        let storage_for_watcher = storage.try_clone();
        let state = Arc::new(RwLock::new(EngineState::new(
            workspace,
            index,
            Box::new(storage),
            Box::new(RealFs::new(&workspace_root)),
        )));
        inject_extension_host(&state, &ext_registry)?;
        let watcher = start_watcher(
            &workspace_root,
            &state,
            storage_for_watcher,
            &ext_registry,
            echo_set,
        )
        .map_err(|source| WorkspaceLoadError::Watcher {
            operation: "watcher_registration",
            source,
        })
        .map_err(|error| error.to_string())?;
        watcher
            .request_catch_up()
            .map_err(|source| WorkspaceLoadError::Watcher {
                operation: "watcher_catch_up",
                source,
            })
            .map_err(|error| error.to_string())?;
        watcher
            .barrier()
            .map_err(|source| WorkspaceLoadError::Watcher {
                operation: "watcher_barrier",
                source,
            })
            .map_err(|error| error.to_string())?;
        Ok((state, watcher))
    };
    #[cfg(feature = "startup-test-hooks")]
    let (prepared, mut reconciliation_metrics) = collect_reconciliation_metrics(prepare);
    #[cfg(not(feature = "startup-test-hooks"))]
    let prepared = prepare();
    let (state, _watcher) = prepared?;
    #[cfg(feature = "startup-test-hooks")]
    if startup_test_hooks.emit_reconciliation_metrics {
        reconciliation_metrics = with_storage_rewrite_delta(
            reconciliation_metrics,
            rewrite_count_before_prepare,
            rewrite_counter.snapshot(),
        );
        emit_reconciliation_metrics(reconciliation_metrics);
    }
    let push_rx = load_extension_registry(
        opts,
        &workspace_root,
        &tower_config,
        &state,
        &ext_registry,
        ext_ast_index,
        format_queue,
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

#[cfg(any(test, feature = "startup-test-hooks"))]
fn with_storage_rewrite_delta(
    mut metrics: ReconciliationMetrics,
    before: usize,
    after: usize,
) -> ReconciliationMetrics {
    metrics.file_record_rewrites += after.saturating_sub(before);
    metrics
}

#[cfg(feature = "startup-test-hooks")]
fn emit_reconciliation_metrics(metrics: ReconciliationMetrics) {
    eprintln!(
        "tower: reconciliation-metrics {{\"content_reads\":{},\"content_hashes\":{},\"file_record_rewrites\":{}}}",
        metrics.content_reads, metrics.content_hashes, metrics.file_record_rewrites
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::adapters::config::{LspConfig, LspServerConfig};

    #[test]
    fn storage_rewrite_delta_is_added_to_collected_reconciliation_metrics() {
        let metrics = ReconciliationMetrics {
            content_reads: 2,
            content_hashes: 1,
            file_record_rewrites: 0,
        };

        assert_eq!(
            with_storage_rewrite_delta(metrics, 7, 9).file_record_rewrites,
            2
        );
    }

    #[test]
    fn f008_daemon_hands_lsp_configuration_to_extension_discovery() {
        let tower_config = TowerConfig {
            lsp: LspConfig {
                servers: BTreeMap::from([(
                    "rust".to_owned(),
                    LspServerConfig {
                        command: "rust-analyzer".to_owned(),
                        extensions: vec!["rs".to_owned()],
                        args: vec!["--log-file".to_owned(), "log with spaces".to_owned()],
                    },
                )]),
                idle_timeout: Some(std::time::Duration::from_secs(45)),
            },
            ..TowerConfig::default()
        };

        let init_configs = extension_init_configs(&tower_config);

        assert_eq!(
            init_configs.get("lsp"),
            Some(&serde_json::json!({
                "servers": {
                    "rust": {
                        "command": "rust-analyzer",
                        "extensions": ["rs"],
                        "args": ["--log-file", "log with spaces"],
                    },
                },
                "idle_timeout_secs": 45,
            }))
        );
    }
}
