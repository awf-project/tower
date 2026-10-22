//! Engine assembly: open Sled, scan, build shared state, load extensions,
//! start the watcher. Extracted from the old `main.rs::run` so both the daemon
//! and (Phase A) the stdio path share one setup. Serving is the caller's job.
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use extension_protocol::ExtensionFault;

use super::catch_up::{CatchUpError, CatchUpFailure};
use crate::adapters::SledStorageAdapter;
use crate::adapters::ast_index::{
    XdgAstIndexAdapter, compute_workspace_key, global_ast_workspace_dir, workspace_ast_dir,
};
use crate::adapters::cli::{GlobalOpts, resolve_extensions_dir_arg, resolve_workspace_root};
use crate::adapters::config::TowerConfig;
use crate::adapters::config::lsp::{lsp_initialize_payload, validate_lsp_commands};
use crate::adapters::daemon::instrumentation::StartupInstrumentation;
use crate::adapters::daemon::lifecycle::{
    CapabilityDependency, CapabilityReadiness, DaemonLifecycle, ReadinessController,
    ReadinessWaitError, StartupFailureClass, StartupPhase,
};
use crate::adapters::daemon::socket::WorkspaceDaemonOwner;
use crate::adapters::daemon::wire::StartupInstrumentationSnapshot;
use crate::adapters::daemon::{CommittedEventPublisher, ExtensionDependencyPolicy, SnapshotEntry};
use crate::adapters::extension::host_deps::ApplyEditsHostPort;
use crate::adapters::extension::{
    ExtensionInitConfigMap, ExtensionSupervisor, HostDeps as ExtensionHostDeps,
    global_extensions_dir, resolve_extension_dirs,
};
use crate::adapters::fs::RealFs;
#[cfg(any(test, feature = "startup-test-hooks"))]
use crate::adapters::fs::scan::ReconciliationMetrics;
#[cfg(feature = "startup-test-hooks")]
use crate::adapters::fs::scan::collect_reconciliation_metrics;
use crate::adapters::fs::scan::{ReconciliationReport, ScanError};
use crate::adapters::mcp::PushEvent;
use crate::adapters::mcp::diagnostics::{DiagnosticsReader, NoOpDiagnosticsReader};
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::watcher::{NotifyWatcherAdapter, RenameCoordinator, WatcherError};
use crate::domain::DomainError;
use crate::domain::extension_host::{
    ExtensionId, ExtensionRegistry, InvokeError, RegistrationError,
};
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::FileMutationService;
use crate::domain::workspace::ProjectWorkspace;
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
#[derive(Debug)]
pub enum PreparationError {
    Cancelled,
    Storage,
    Reconciliation,
    WatcherRegistration,
    WatcherCatchUp,
    WatcherBarrier,
    ExtensionDiscovery,
}

impl std::fmt::Display for PreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "workspace preparation was cancelled",
            Self::Storage => "workspace storage preparation failed",
            Self::Reconciliation => "workspace reconciliation failed",
            Self::WatcherRegistration => "workspace watcher registration failed",
            Self::WatcherCatchUp => "workspace watcher catch-up failed",
            Self::WatcherBarrier => "workspace watcher barrier failed",
            Self::ExtensionDiscovery => "workspace extension discovery failed",
        })
    }
}

impl std::error::Error for PreparationError {}

pub struct PreparedEngine {
    handle: Mutex<Option<EngineHandle>>,
}

impl PreparedEngine {
    fn new(handle: EngineHandle) -> Self {
        Self {
            handle: Mutex::new(Some(handle)),
        }
    }

    pub fn state(&self) -> Arc<RwLock<EngineState>> {
        Arc::clone(
            &self
                .handle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .expect("prepared engine resources remain owned")
                .state,
        )
    }

    pub fn extension_registry(&self) -> Arc<RwLock<ExtensionRegistry>> {
        Arc::clone(
            &self
                .handle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .expect("prepared engine resources remain owned")
                .ext_registry,
        )
    }

    pub fn rename_coordinator(&self) -> Arc<RenameCoordinator> {
        Arc::clone(
            &self
                .handle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .expect("prepared engine resources remain owned")
                .rename_coordinator,
        )
    }

    pub fn diagnostics_reader(&self) -> Arc<dyn DiagnosticsReader> {
        Arc::clone(
            &self
                .handle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .expect("prepared engine resources remain owned")
                .diag_reader,
        )
    }

    pub fn take_push_receiver(&self) -> Option<mpsc::Receiver<PushEvent>> {
        self.handle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_mut()
            .and_then(|handle| handle.push_rx.take())
    }

    fn install_push_receiver(&self, receiver: mpsc::Receiver<PushEvent>) {
        if let Some(handle) = self
            .handle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_mut()
        {
            handle.push_rx = Some(receiver);
        }
    }
}

#[derive(Clone)]
pub struct PreparationCancellation {
    cancelled: Arc<AtomicBool>,
}

impl PreparationCancellation {
    fn check(&self) -> Result<(), PreparationError> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(PreparationError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct PreparationShared {
    published: Mutex<Option<Arc<PreparedEngine>>>,
    complete: Mutex<bool>,
    changed: Condvar,
    inventory_complete: Mutex<bool>,
    inventory_changed: Condvar,
}

pub struct EarlyDaemonContext {
    readiness: Arc<ReadinessController>,
    instrumentation: Arc<StartupInstrumentation>,
    opts: Mutex<Option<GlobalOpts>>,
    config: Mutex<Option<TowerConfig>>,
    shared: Arc<PreparationShared>,
    preparation_timeout: Duration,
    dependency_policy: Arc<ExtensionDependencyPolicy>,
}

impl EarlyDaemonContext {
    pub fn readiness(&self) -> Arc<ReadinessController> {
        Arc::clone(&self.readiness)
    }

    pub fn instrumentation_snapshot(&self) -> StartupInstrumentationSnapshot {
        self.instrumentation.snapshot()
    }

    pub fn preparation_timeout(&self) -> Duration {
        self.preparation_timeout
    }

    pub fn dependency_policy(&self) -> Arc<ExtensionDependencyPolicy> {
        Arc::clone(&self.dependency_policy)
    }

    pub fn prepared_if_available(&self) -> Option<Arc<PreparedEngine>> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .cloned()
    }

    pub fn prepared_if_complete(&self) -> Option<Arc<PreparedEngine>> {
        if !*self
            .shared
            .complete
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
        {
            return None;
        }
        self.shared
            .published
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .cloned()
    }

    pub fn wait_prepared(
        &self,
        deadline: Duration,
    ) -> Result<Arc<PreparedEngine>, ReadinessWaitError> {
        self.readiness
            .wait_until_ready(CapabilityDependency::Workspace, deadline)?;
        self.shared
            .published
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .cloned()
            .ok_or(ReadinessWaitError::Failed {
                dependency: CapabilityDependency::Workspace,
                failure_class: StartupFailureClass::Cancelled,
            })
    }

    pub fn wait_preparation_complete(
        &self,
        deadline: Duration,
    ) -> Result<Arc<PreparedEngine>, ReadinessWaitError> {
        let complete = self
            .shared
            .complete
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (complete, timeout) = self
            .shared
            .changed
            .wait_timeout_while(complete, deadline, |complete| !*complete)
            .unwrap_or_else(|poison| poison.into_inner());
        if !*complete && timeout.timed_out() {
            return Err(ReadinessWaitError::Timeout {
                dependency: CapabilityDependency::Workspace,
                phase: StartupPhase::ExtensionsDiscovering,
            });
        }
        drop(complete);
        self.wait_prepared(Duration::ZERO)
    }

    pub fn wait_extension_inventory(
        &self,
        deadline: Duration,
    ) -> Result<Arc<PreparedEngine>, ReadinessWaitError> {
        let complete = self
            .shared
            .inventory_complete
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (complete, timeout) = self
            .shared
            .inventory_changed
            .wait_timeout_while(complete, deadline, |complete| !*complete)
            .unwrap_or_else(|poison| poison.into_inner());
        if !*complete && timeout.timed_out() {
            return Err(ReadinessWaitError::Timeout {
                dependency: CapabilityDependency::Workspace,
                phase: StartupPhase::ExtensionsDiscovering,
            });
        }
        drop(complete);
        self.wait_prepared(Duration::ZERO)
    }

    pub fn release_prepared(&self) {
        self.shared
            .published
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
    }
}

pub struct PreparationHandle {
    cancellation: PreparationCancellation,
    readiness: Arc<ReadinessController>,
    shared: Arc<PreparationShared>,
    worker: Option<JoinHandle<Result<Arc<PreparedEngine>, PreparationError>>>,
}

impl PreparationHandle {
    pub fn cancel(&self) {
        self.cancellation.cancel();
        let prepared = self
            .shared
            .published
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(prepared) = prepared
            && let Some(publisher) = &prepared
                .state()
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .publication
        {
            publisher.cancel_deliveries();
        }
    }

    pub fn wait_workspace_ready(
        &self,
        deadline: Duration,
    ) -> Result<Arc<PreparedEngine>, ReadinessWaitError> {
        self.readiness
            .wait_until_ready(CapabilityDependency::Workspace, deadline)?;
        self.shared
            .published
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .cloned()
            .ok_or(ReadinessWaitError::Failed {
                dependency: CapabilityDependency::Workspace,
                failure_class: StartupFailureClass::Cancelled,
            })
    }

    pub fn join(mut self) -> Result<Arc<PreparedEngine>, PreparationError> {
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<Arc<PreparedEngine>, PreparationError> {
        let Some(worker) = self.worker.take() else {
            return self
                .shared
                .published
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .cloned()
                .ok_or(PreparationError::Cancelled);
        };
        // Never detach a thread holding storage or extension resources. Reload,
        // scan, watcher waits and replay now observe this handle's cancellation;
        // an already entered OS syscall/durable commit must finish before join.
        worker.join().unwrap_or(Err(PreparationError::Cancelled))
    }
}

impl Drop for PreparationHandle {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.cancel();
            let _ = self.join_worker();
        }
    }
}

pub fn build_early_context(
    opts: &GlobalOpts,
    config: TowerConfig,
    owner: &WorkspaceDaemonOwner,
) -> Result<EarlyDaemonContext, PreparationError> {
    let preparation_timeout = config.daemon.preparation_timeout();
    let dependency_policy = Arc::new(extension_dependency_policy(&config));
    let workspace_root = resolve_workspace_root(opts);
    emit_lsp_startup_diagnostics(&config, &workspace_root);
    let readiness = Arc::new(ReadinessController::new(owner.generation()));
    let instrumentation = Arc::new(StartupInstrumentation::new());
    instrumentation
        .record_phase(StartupPhase::OwnershipAcquired, Duration::ZERO)
        .map_err(|_| PreparationError::Storage)?;
    readiness
        .transition_daemon(DaemonLifecycle::Starting {
            phase: StartupPhase::TransportBound,
        })
        .map_err(|_| PreparationError::Storage)?;
    instrumentation
        .record_phase(StartupPhase::TransportBound, Duration::ZERO)
        .map_err(|_| PreparationError::Storage)?;
    Ok(EarlyDaemonContext {
        readiness,
        instrumentation,
        opts: Mutex::new(Some(GlobalOpts {
            workspace_dir: opts.workspace_dir.clone(),
            extensions_dir: opts.extensions_dir.clone(),
        })),
        config: Mutex::new(Some(config)),
        shared: Arc::new(PreparationShared {
            published: Mutex::new(None),
            complete: Mutex::new(false),
            changed: Condvar::new(),
            inventory_complete: Mutex::new(false),
            inventory_changed: Condvar::new(),
        }),
        preparation_timeout,
        dependency_policy,
    })
}

pub fn start_workspace_preparation(
    context: Arc<EarlyDaemonContext>,
) -> Result<PreparationHandle, PreparationError> {
    let cancellation = PreparationCancellation {
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let worker_cancellation = cancellation.clone();
    let readiness = context.readiness();
    let worker_readiness = Arc::clone(&readiness);
    let shared = Arc::clone(&context.shared);
    let worker_shared = Arc::clone(&shared);
    let opts = context
        .opts
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
        .ok_or(PreparationError::Storage)?;
    let config = context
        .config
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
        .ok_or(PreparationError::Storage)?;
    let instrumentation = Arc::clone(&context.instrumentation);
    let worker = std::thread::Builder::new()
        .name("workspace-preparation".to_owned())
        .spawn(move || {
            let result = run_workspace_preparation(
                &opts,
                config,
                &worker_readiness,
                &worker_shared,
                &worker_cancellation,
                &instrumentation,
            );
            if let Err(error) = &result {
                publish_preparation_failure(&worker_readiness, failure_class(error));
            }
            *worker_shared
                .complete
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = true;
            worker_shared.changed.notify_all();
            *worker_shared
                .inventory_complete
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = true;
            worker_shared.inventory_changed.notify_all();
            result
        })
        .map_err(|_| PreparationError::Storage)?;
    Ok(PreparationHandle {
        cancellation,
        readiness,
        shared,
        worker: Some(worker),
    })
}

struct WorkspacePreparation {
    handle: EngineHandle,
    ext_ast_index: Arc<dyn AstIndexPort + Send + Sync>,
    format_queue: SharedFormatQueue,
}

/// Own persisted workspace assembly through the watcher barrier. Extension
/// discovery and capability publication remain in the startup coordinator.
fn prepare_workspace_resources(
    opts: &GlobalOpts,
    config: &TowerConfig,
    readiness: &ReadinessController,
    cancellation: &PreparationCancellation,
    instrumentation: &StartupInstrumentation,
    publisher: Arc<CommittedEventPublisher>,
) -> Result<WorkspacePreparation, PreparationError> {
    readiness
        .transition_daemon(DaemonLifecycle::Starting {
            phase: StartupPhase::StorageOpening,
        })
        .map_err(|_| PreparationError::Storage)?;
    let phase_started = Instant::now();
    let workspace_root = resolve_workspace_root(opts);
    #[cfg(feature = "startup-test-hooks")]
    let startup_test_hooks = StartupTestHooks::for_workspace(&workspace_root);
    let storage_result = SledStorageAdapter::open_cancellable(
        &workspace_root.join(".tower/db"),
        &cancellation.cancelled,
    );
    cancellation.check()?;
    let (mut storage, workspace, index) = storage_result.map_err(|_| PreparationError::Storage)?;
    cancellation.check()?;
    instrumentation
        .record_phase(StartupPhase::StorageOpening, phase_started.elapsed())
        .map_err(|_| PreparationError::Storage)?;
    #[cfg(feature = "startup-test-hooks")]
    let rewrite_counter = storage.reconciliation_rewrite_counter();
    #[cfg(feature = "startup-test-hooks")]
    let rewrite_count_before_prepare = rewrite_counter.snapshot();
    let ext_ast_index = ast_index_for_workspace(&workspace_root);
    let (format_queue, echo_set) = format_queue_for_workspace(&workspace_root, config);
    let ext_registry = Arc::new(RwLock::new(ExtensionRegistry::new()));
    readiness
        .transition_daemon(DaemonLifecycle::Starting {
            phase: StartupPhase::WorkspaceReconciling,
        })
        .map_err(|_| PreparationError::Reconciliation)?;
    let phase_started = Instant::now();
    #[cfg(feature = "startup-test-hooks")]
    let (workspace_load, mut reconciliation_metrics) = collect_reconciliation_metrics(|| {
        load_workspace_index(
            &workspace_root,
            &mut storage,
            workspace,
            index,
            Some(&cancellation.cancelled),
            &startup_test_hooks,
        )
    });
    #[cfg(not(feature = "startup-test-hooks"))]
    let workspace_load = load_workspace_index(
        &workspace_root,
        &mut storage,
        workspace,
        index,
        Some(&cancellation.cancelled),
    );
    let WorkspaceLoadResult {
        workspace,
        index,
        reconciliation,
    } = {
        cancellation.check()?;
        workspace_load
    }
    .map_err(|_| PreparationError::Reconciliation)?;
    cancellation.check()?;
    instrumentation
        .record_phase(StartupPhase::WorkspaceReconciling, phase_started.elapsed())
        .map_err(|_| PreparationError::Reconciliation)?;
    let _ = writeln!(
        std::io::stderr(),
        "tower: reconciled index — {} added, {} removed, {} unchanged",
        reconciliation.added,
        reconciliation.removed,
        reconciliation.unchanged
    );
    let storage_for_watcher = storage.try_clone();
    let state = Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(storage),
        Box::new(RealFs::new(&workspace_root)),
    )));
    state
        .write()
        .map_err(|_| PreparationError::Storage)?
        .publication = Some(Arc::clone(&publisher));
    inject_extension_host(&state, &ext_registry).map_err(|_| PreparationError::Storage)?;
    let rename_coordinator = Arc::new(RenameCoordinator::default());
    let watcher = start_watcher(
        &workspace_root,
        &state,
        storage_for_watcher,
        &ext_registry,
        echo_set,
        Arc::clone(&rename_coordinator),
        Some(Arc::clone(&cancellation.cancelled)),
    )
    .map_err(|_| PreparationError::WatcherRegistration)?;
    readiness
        .transition_daemon(DaemonLifecycle::Starting {
            phase: StartupPhase::WatcherCatchingUp,
        })
        .map_err(|_| PreparationError::WatcherCatchUp)?;
    let phase_started = Instant::now();
    cancellation.check()?;
    let catch_up = watcher.request_catch_up();
    cancellation.check()?;
    catch_up.map_err(|_| PreparationError::WatcherCatchUp)?;
    let barrier = watcher.barrier();
    cancellation.check()?;
    barrier.map_err(|_| PreparationError::WatcherBarrier)?;
    cancellation.check()?;
    instrumentation
        .record_phase(StartupPhase::WatcherCatchingUp, phase_started.elapsed())
        .map_err(|_| PreparationError::WatcherBarrier)?;
    #[cfg(feature = "startup-test-hooks")]
    if startup_test_hooks.emit_reconciliation_metrics {
        reconciliation_metrics = with_storage_rewrite_delta(
            reconciliation_metrics,
            rewrite_count_before_prepare,
            rewrite_counter.snapshot(),
        );
        emit_reconciliation_metrics(reconciliation_metrics);
    }
    let handle = EngineHandle {
        state: Arc::clone(&state),
        ext_registry: Arc::clone(&ext_registry),
        push_rx: None,
        diag_reader: Arc::new(NoOpDiagnosticsReader),
        rename_coordinator,
        _watcher: watcher,
    };
    Ok(WorkspacePreparation {
        handle,
        ext_ast_index,
        format_queue,
    })
}

fn run_workspace_preparation(
    opts: &GlobalOpts,
    config: TowerConfig,
    readiness: &Arc<ReadinessController>,
    shared: &Arc<PreparationShared>,
    cancellation: &PreparationCancellation,
    instrumentation: &StartupInstrumentation,
) -> Result<Arc<PreparedEngine>, PreparationError> {
    // Every authoritative producer and per-extension FIFO owner shares this
    // generation-local publication boundary.
    let publisher = Arc::new(CommittedEventPublisher::new(
        readiness.snapshot().generation,
    ));
    if cancellation.cancelled.load(Ordering::Acquire) {
        publish_preparation_failure(readiness, StartupFailureClass::Cancelled);
        return Err(PreparationError::Cancelled);
    }
    if let Err(error) = preparation_test_barrier(cancellation, "before-reconciliation-commit") {
        publish_preparation_failure(readiness, failure_class(&error));
        return Err(error);
    }
    if let Some(error) = injected_preparation_failure() {
        publish_preparation_failure(readiness, failure_class(&error));
        return Err(error);
    }
    let WorkspacePreparation {
        handle,
        ext_ast_index,
        format_queue,
    } = prepare_workspace_resources(
        opts,
        &config,
        readiness,
        cancellation,
        instrumentation,
        Arc::clone(&publisher),
    )?;
    let workspace_root = resolve_workspace_root(opts);
    let state = Arc::clone(&handle.state);
    let ext_registry = Arc::clone(&handle.ext_registry);
    if cancellation.cancelled.load(Ordering::Acquire) {
        drop(handle);
        publish_preparation_failure(readiness, StartupFailureClass::Cancelled);
        return Err(PreparationError::Cancelled);
    }
    let prepared = Arc::new(PreparedEngine::new(handle));
    *shared
        .published
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(Arc::clone(&prepared));
    shared.changed.notify_all();
    readiness
        .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
        .map_err(|_| PreparationError::WatcherBarrier)?;
    readiness
        .transition_daemon(DaemonLifecycle::Starting {
            phase: StartupPhase::ExtensionsDiscovering,
        })
        .map_err(|_| PreparationError::ExtensionDiscovery)?;
    let phase_started = Instant::now();
    preparation_test_barrier(cancellation, "before-extension-discovery")?;
    let push_rx = load_extension_registry(
        opts,
        &workspace_root,
        &config,
        &state,
        &ext_registry,
        ext_ast_index,
        format_queue,
        Some(&cancellation.cancelled),
    )
    .map_err(|_| PreparationError::ExtensionDiscovery)?;
    cancellation.check()?;
    prepared.install_push_receiver(push_rx);
    instrumentation
        .record_phase(StartupPhase::ExtensionsDiscovering, phase_started.elapsed())
        .map_err(|_| PreparationError::ExtensionDiscovery)?;
    register_extension_readiness(&config, readiness, &ext_registry, &publisher)
        .map_err(|_| PreparationError::ExtensionDiscovery)?;
    readiness
        .transition_daemon(DaemonLifecycle::Starting {
            phase: StartupPhase::CapabilitiesCatchingUp,
        })
        .map_err(|_| PreparationError::ExtensionDiscovery)?;
    // Discovery is complete independently of replay. Resolve absent families
    // before inventory waiters classify optional semantic dependencies.
    readiness
        .close_discovery()
        .map_err(|_| PreparationError::ExtensionDiscovery)?;
    *shared
        .inventory_complete
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = true;
    shared.inventory_changed.notify_all();
    if let Err(error) = preparation_test_barrier(cancellation, "capability-catch-up-blocked") {
        publish_preparation_failure(readiness, failure_class(&error));
        return Err(error);
    }
    let phase_started = Instant::now();
    let capabilities_ready = drive_extension_catch_up(
        &config,
        readiness,
        &publisher,
        &ext_registry,
        cancellation,
        instrumentation,
    )
    .map_err(|_| PreparationError::ExtensionDiscovery)?;
    if capabilities_ready {
        instrumentation
            .record_phase(
                StartupPhase::CapabilitiesCatchingUp,
                phase_started.elapsed(),
            )
            .map_err(|_| PreparationError::ExtensionDiscovery)?;
    }
    let _ = readiness.close_discovery();
    record_preparation_stage_trace(opts);
    Ok(prepared)
}

fn extension_dependency_policy(config: &TowerConfig) -> ExtensionDependencyPolicy {
    let mut dependencies: HashMap<ExtensionId, CapabilityDependency> = [
        (ExtensionId::new("ast"), CapabilityDependency::Ast),
        (ExtensionId::new("lsp"), CapabilityDependency::Lsp),
    ]
    .into_iter()
    .collect();
    dependencies.extend(
        config
            .extensions
            .readiness
            .iter()
            .map(|(identity, family)| {
                let dependency = match family {
                    crate::adapters::config::ReadinessFamily::Ast => CapabilityDependency::Ast,
                    crate::adapters::config::ReadinessFamily::Lsp => CapabilityDependency::Lsp,
                };
                (ExtensionId::new(identity), dependency)
            }),
    );
    let readiness_controls = config
        .extensions
        .readiness_controls
        .iter()
        .map(|(identity, controls)| {
            let mut controls: HashSet<String> = controls.iter().cloned().collect();
            controls.insert("_tower_catch_up_fence".to_owned());
            (ExtensionId::new(identity), controls)
        })
        .collect();
    ExtensionDependencyPolicy::new(dependencies, readiness_controls)
}

fn loaded_extension_ids(registry: &Arc<RwLock<ExtensionRegistry>>) -> HashSet<ExtensionId> {
    registry
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .declared_tools()
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

fn register_extension_readiness(
    config: &TowerConfig,
    readiness: &ReadinessController,
    registry: &Arc<RwLock<ExtensionRegistry>>,
    publisher: &CommittedEventPublisher,
) -> Result<(), ()> {
    let mut loaded = loaded_extension_ids(registry);
    // A successfully initialized extension can declare zero tools. Its delivery
    // owner still establishes its identity and must not bypass control validation.
    loaded.extend(
        extension_dependency_bindings(config)
            .into_keys()
            .filter(|id| publisher.delivery(id).is_some()),
    );
    for (extension_id, dependency) in extension_dependency_bindings(config) {
        if !loaded.contains(&extension_id) {
            continue;
        }
        readiness
            .register_extension(extension_id.clone(), dependency)
            .map_err(|_| ())?;
        let declarations = registry
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .declared_tools();
        let controls_valid = config
            .extensions
            .readiness_controls
            .get(extension_id.as_str())
            .is_none_or(|controls| {
                controls.iter().all(|control| {
                    declarations
                        .iter()
                        .any(|(id, tool)| id == &extension_id && tool.name == *control)
                })
            });
        readiness
            .transition_extension(
                &extension_id,
                if controls_valid {
                    CapabilityReadiness::Preparing {
                        phase: StartupPhase::CapabilitiesCatchingUp,
                    }
                } else {
                    CapabilityReadiness::Failed {
                        failure_class: StartupFailureClass::ExtensionProtocol,
                    }
                },
            )
            .map_err(|_| ())?;
    }
    Ok(())
}

fn extension_dependency_bindings(
    config: &TowerConfig,
) -> HashMap<ExtensionId, CapabilityDependency> {
    let mut bindings = HashMap::from([
        (ExtensionId::new("ast"), CapabilityDependency::Ast),
        (ExtensionId::new("lsp"), CapabilityDependency::Lsp),
    ]);
    if config.lsp.servers.is_empty() {
        bindings.remove(&ExtensionId::new("lsp"));
    }
    for (identity, family) in &config.extensions.readiness {
        bindings.insert(
            ExtensionId::new(identity),
            match family {
                crate::adapters::config::ReadinessFamily::Ast => CapabilityDependency::Ast,
                crate::adapters::config::ReadinessFamily::Lsp => CapabilityDependency::Lsp,
            },
        );
    }
    bindings
}

fn drive_extension_catch_up(
    config: &TowerConfig,
    readiness: &ReadinessController,
    publisher: &Arc<CommittedEventPublisher>,
    registry: &Arc<RwLock<ExtensionRegistry>>,
    cancellation: &PreparationCancellation,
    instrumentation: &StartupInstrumentation,
) -> Result<bool, ()> {
    // Every epoch has its own FIFO owner and deadline. A blocked AST fence
    // cannot prevent a ready LSP epoch (or vice versa) from being published.
    std::thread::scope(|scope| {
        let workers: Vec<_> = extension_dependency_bindings(config).into_keys()
            .filter(|id| publisher.delivery(id).is_some())
            .map(|id| scope.spawn(move || {
                let delivery = publisher.require_delivery(&id).map_err(|_| ())?;
                if readiness.snapshot().extensions.iter().any(|extension| extension.extension_id == id
                    && matches!(extension.readiness, CapabilityReadiness::Failed { .. })) {
                    delivery.cancel();
                    return Ok(false);
                }
                let started = Instant::now();
                let target = {
                    let _commit = publisher.commit_lock();
                    delivery.cut()
                };
                let result = loop {
                    if cancellation.cancelled.load(Ordering::Acquire) {
                        break Err((0, CatchUpFailure::protocol(id.clone(), CatchUpError::Cancelled { extension_id: id.clone() })));
                    }
                    match target.recv_timeout(Duration::from_millis(10)) {
                        Ok(result) => break result,
                        Err(mpsc::RecvTimeoutError::Timeout) => {},
                        Err(mpsc::RecvTimeoutError::Disconnected) => break Err((0, CatchUpFailure::protocol(id.clone(), CatchUpError::BarrierClosed { extension_id: id.clone() }))),
                    }
                };
                    let (delivered_events, mut failure) = match result {
                        Ok(count) => (count, None),
                        Err((count, fault)) => (count, Some(fault)),
                    };
                let deadline = Instant::now() + config.extensions.request_timeout();
                while failure.is_none() {
                    if cancellation.cancelled.load(Ordering::Acquire) {
                        failure = Some(CatchUpFailure::protocol(id.clone(), CatchUpError::Cancelled { extension_id: id.clone() }));
                        break;
                    }
                    let response = registry.read().unwrap_or_else(|p| p.into_inner())
                        .invoke_extension(&id, "_tower_catch_up_fence", serde_json::json!({"epoch": 1, "delivered_events": delivered_events}));
                    match classify_runtime_fence(&id, response, delivered_events, Instant::now() >= deadline) {
                        Ok(true) => break,
                        Ok(false) => std::thread::park_timeout(Duration::from_millis(10)),
                        Err(class) => failure = Some(class),
                    }
                }
                instrumentation.record_extension_replay(id.clone(), delivered_events, started.elapsed(), failure.as_ref().map(|failure| failure.failure_class)).map_err(|_| ())?;
                {
                    let _commit = publisher.commit_lock();
                    match &failure {
                        None => {
                            delivery.activate();
                            readiness.transition_extension(&id, CapabilityReadiness::Ready).map_err(|_| ())?;
                        }
                        Some(failure) => {
                            delivery.cancel();
                            let _ = readiness.transition_extension(&id, CapabilityReadiness::Failed { failure_class: failure.failure_class });
                        }
                    }
                }

                Ok::<bool, ()>(failure.is_none())
            })).collect();
        let mut all_ready = true;
        for worker in workers {
            all_ready &= worker.join().map_err(|_| ())??;
        }
        Ok(all_ready)
    })
}

fn classify_runtime_fence(
    id: &ExtensionId,
    response: Result<serde_json::Value, InvokeError>,
    target: u64,
    expired: bool,
) -> Result<bool, CatchUpFailure> {
    let value = match response {
        Ok(value) => value,
        Err(InvokeError::ToolNotFound(_)) => {
            return Err(CatchUpFailure::protocol(
                id.clone(),
                CatchUpError::FenceUnsupported {
                    extension_id: id.clone(),
                },
            ));
        }
        Err(InvokeError::Fault(fault)) => return Err(CatchUpFailure::delivery(id.clone(), &fault)),
        Err(InvokeError::Application(_)) => {
            return Err(CatchUpFailure::protocol(
                id.clone(),
                CatchUpError::FenceMismatch {
                    extension_id: id.clone(),
                },
            ));
        }
    };
    classify_fence_response(Ok(value), target, expired).map_err(|class| {
        if class == StartupFailureClass::ExtensionTimeout {
            CatchUpFailure::delivery(id.clone(), &ExtensionFault::Timeout)
        } else {
            CatchUpFailure::protocol(
                id.clone(),
                CatchUpError::FenceMismatch {
                    extension_id: id.clone(),
                },
            )
        }
    })
}

fn classify_fence_response(
    response: Result<serde_json::Value, InvokeError>,
    target: u64,
    expired: bool,
) -> Result<bool, StartupFailureClass> {
    let response = response.map_err(|error| catch_up_failure_class(Some(&error)))?;
    let epoch = response.get("epoch").and_then(serde_json::Value::as_u64);
    let count = response
        .get("processed_events")
        .and_then(serde_json::Value::as_u64);
    if epoch != Some(1) {
        return Err(StartupFailureClass::ExtensionProtocol);
    }
    match (
        response.get("ready").and_then(serde_json::Value::as_bool),
        count,
    ) {
        (Some(true), Some(count)) if count == target => Ok(true),
        (Some(false), Some(count)) if count <= target => {
            if expired {
                Err(StartupFailureClass::ExtensionTimeout)
            } else {
                Ok(false)
            }
        }
        _ => Err(StartupFailureClass::ExtensionProtocol),
    }
}

fn catch_up_failure_class(error: Option<&InvokeError>) -> StartupFailureClass {
    match error {
        Some(InvokeError::Fault(ExtensionFault::Timeout)) => StartupFailureClass::ExtensionTimeout,
        Some(InvokeError::Fault(ExtensionFault::Crashed { .. })) => {
            StartupFailureClass::ExtensionCrash
        }
        Some(InvokeError::Fault(
            ExtensionFault::ProtocolError { .. } | ExtensionFault::Quarantined,
        ))
        | Some(InvokeError::Application(_) | InvokeError::ToolNotFound(_))
        | None => StartupFailureClass::ExtensionProtocol,
    }
}

fn preparation_test_barrier(
    cancellation: &PreparationCancellation,
    expected_barrier: &str,
) -> Result<(), PreparationError> {
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = (cancellation, expected_barrier);
    #[cfg(feature = "startup-test-hooks")]
    {
        use std::io::{BufRead as _, Write as _};
        let Ok(barrier) = env::var("TOWER_TEST_PREPARATION_BARRIER") else {
            return Ok(());
        };
        if barrier != expected_barrier && barrier != "fail-reconciliation" {
            return Ok(());
        }
        if barrier == "fail-reconciliation" && expected_barrier != "before-reconciliation-commit" {
            return Ok(());
        }
        if env::var("TOWER_TEST_PREPARATION_HANDSHAKE").as_deref() != Ok("stdio") {
            return Ok(());
        }
        println!("transport-bound:{barrier}");
        let _ = std::io::stdout().flush();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut command = String::new();
            let _ = std::io::stdin().lock().read_line(&mut command);
            let _ = tx.send(command);
        });
        while !cancellation.cancelled.load(Ordering::Acquire) {
            if rx.recv_timeout(Duration::from_millis(25)).is_ok() {
                return if barrier == "fail-reconciliation" {
                    Err(PreparationError::Reconciliation)
                } else {
                    Ok(())
                };
            }
        }
        Err(PreparationError::Cancelled)
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    Ok(())
}

fn publish_preparation_failure(readiness: &ReadinessController, class: StartupFailureClass) {
    let _ = readiness.transition_capability(
        CapabilityDependency::Workspace,
        CapabilityReadiness::Failed {
            failure_class: class,
        },
    );
}

fn failure_class(error: &PreparationError) -> StartupFailureClass {
    match error {
        PreparationError::Cancelled => StartupFailureClass::Cancelled,
        PreparationError::Storage => StartupFailureClass::Storage,
        PreparationError::Reconciliation => StartupFailureClass::Reconciliation,
        PreparationError::WatcherRegistration
        | PreparationError::WatcherCatchUp
        | PreparationError::WatcherBarrier => StartupFailureClass::Watcher,
        PreparationError::ExtensionDiscovery => StartupFailureClass::ExtensionProtocol,
    }
}

fn injected_preparation_failure() -> Option<PreparationError> {
    // A second, test-only sentinel prevents production processes from honoring
    // an accidentally inherited failure selector.
    env::var_os("TOWER_TEST_EXPECTED_PREPARATION_ERROR")?;
    match env::var("TOWER_TEST_PREPARATION_FAILURE").ok()?.as_str() {
        "cancelled" => Some(PreparationError::Cancelled),
        "storage" => Some(PreparationError::Storage),
        "reconciliation" => Some(PreparationError::Reconciliation),
        "watcher_registration" => Some(PreparationError::WatcherRegistration),
        "watcher_catch_up" => Some(PreparationError::WatcherCatchUp),
        "watcher_barrier" => Some(PreparationError::WatcherBarrier),
        "extension_discovery" => Some(PreparationError::ExtensionDiscovery),
        _ => None,
    }
}

fn record_preparation_stage_trace(opts: &GlobalOpts) {
    let Some(path) = env::var_os("TOWER_TEST_PREPARATION_STAGE_TRACE") else {
        return;
    };
    if env::var_os("TOWER_TEST_WORKSPACE").as_deref()
        != opts.workspace_dir.as_deref().map(Path::as_os_str)
    {
        return;
    }
    let stages = [
        "storage_open_reload",
        "durable_reconciliation",
        "watcher_registration",
        "watcher_catch_up",
        "watcher_barrier",
        "extension_discovery",
        "capability_catch_up",
    ];
    let _ = std::fs::write(path, serde_json::to_vec(&stages).unwrap_or_default());
}

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
    // The engine owns its extension registry. Host dependencies must not close
    // an Arc cycle back to the engine (or tear it down on its own child worker).
    state: std::sync::Weak<RwLock<EngineState>>,
}

impl EngineApplyEditsHost {
    fn new(state: Arc<RwLock<EngineState>>) -> Self {
        Self {
            state: Arc::downgrade(&state),
        }
    }
    fn with_mutation_service<T>(
        &self,
        f: impl FnOnce(&mut FileMutationService<'_>) -> Result<T, DomainError>,
    ) -> Result<T, DomainError> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| DomainError::IoError("workspace engine is stopping".to_owned()))?;
        crate::adapters::mcp::native_tools::with_mutation_service(&state, |service| Ok(f(service)))
            .map_err(|_| DomainError::IoError("workspace edit commit failed".to_owned()))?
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

    fn invoke_extension(
        &self,
        extension_id: &crate::domain::ExtensionId,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, crate::domain::InvokeError> {
        let guard = self.0.read().unwrap_or_else(|p| p.into_inner());
        guard.invoke_extension(extension_id, tool_name, params)
    }
}

// ── EngineHandle ──────────────────────────────────────────────────────────────

/// Everything the serve loop needs, with the watcher kept alive by ownership.
pub struct EngineHandle {
    pub state: Arc<RwLock<EngineState>>,
    pub ext_registry: Arc<RwLock<ExtensionRegistry>>,
    pub push_rx: Option<std::sync::mpsc::Receiver<PushEvent>>,
    pub diag_reader: Arc<dyn DiagnosticsReader>,
    /// Shared ordering boundary for native renames and filesystem events.
    pub rename_coordinator: Arc<RenameCoordinator>,
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
    cancelled: Option<&AtomicBool>,
    #[cfg(feature = "startup-test-hooks")] hooks: &StartupTestHooks,
) -> Result<WorkspaceLoadResult, WorkspaceLoadError> {
    let mut workspace = workspace;
    let mut index = index;
    let never_cancelled = AtomicBool::new(false);
    let cancelled = cancelled.unwrap_or(&never_cancelled);
    #[cfg(not(feature = "startup-test-hooks"))]
    let reconciliation = crate::adapters::fs::scan::reconcile_workspace_cancellable(
        workspace_root,
        storage,
        &mut workspace,
        &mut index,
        cancelled,
        || Ok(()),
    );
    #[cfg(feature = "startup-test-hooks")]
    let reconciliation = crate::adapters::fs::scan::reconcile_workspace_cancellable(
        workspace_root,
        storage,
        &mut workspace,
        &mut index,
        cancelled,
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
            let _ = writeln!(
                std::io::stderr(),
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

#[allow(clippy::too_many_arguments)] // Composition boundary: existing resources plus cancellation.
fn load_extension_registry(
    opts: &GlobalOpts,
    workspace_root: &Path,
    tower_config: &TowerConfig,
    state: &Arc<RwLock<EngineState>>,
    ext_registry: &Arc<RwLock<ExtensionRegistry>>,
    ext_ast_index: Arc<dyn AstIndexPort + Send + Sync>,
    format_queue: Arc<dyn crate::adapters::formatter::FormatQueuePort + Send + Sync>,
    cancelled: Option<&AtomicBool>,
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

    let publication = state
        .read()
        .map_err(|_| "engine state lock poisoned")?
        .publication
        .clone();
    let mut registration_error = None;
    crate::adapters::extension::discovery::load_extensions_with_register_cancellable(
        &extension_dirs,
        ext_deps.clone(),
        tower_config.extensions.request_timeout(),
        &tower_config.extensions.disabled,
        &init_configs,
        cancelled,
        |instance, name, _| {
            let instance = if let Some(publisher) = &publication {
                let _commit = publisher.commit_lock();
                let snapshot = state
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .workspace_arc()
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .snapshot()
                    .files
                    .into_iter()
                    .map(|file| SnapshotEntry {
                        file_id: file.id,
                        path: file.path,
                    })
                    .collect();
                let id = ExtensionId::new(name);
                let preparing = extension_dependency_bindings(tower_config).contains_key(&id);
                match super::delivery::OrderedExtension::spawn(
                    instance,
                    Arc::clone(publisher),
                    snapshot,
                    preparing,
                ) {
                    Ok((instance, delivery)) => {
                        let subscribed = publisher.subscribe(id, delivery);
                        // Subscription and snapshot are now installed. Release
                        // the cut before registration: rejection drops and joins
                        // the worker, which must be able to acquire this gate.
                        drop(_commit);
                        if subscribed.is_err() {
                            registration_error =
                                Some("duplicate or closed extension epoch".to_owned());
                            return;
                        }
                        if ext_registry
                            .write()
                            .unwrap_or_else(|p| p.into_inner())
                            .register(instance)
                            .is_err()
                        {
                            registration_error = Some("duplicate extension identity".to_owned());
                        }
                    }
                    Err(_) => {
                        registration_error = Some("extension delivery worker failed".to_owned())
                    }
                }
                return;
            } else {
                instance
            };
            if ext_registry
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .register(instance)
                .is_err()
            {
                registration_error = Some("duplicate extension identity".to_owned());
            }
        },
    );
    if let Some(error) = registration_error {
        return Err(error);
    }

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
        let _ = writeln!(
            std::io::stderr(),
            "tower: loaded {ext_tool_count} extension tool(s) from [{}]",
            scopes.join(", ")
        );
    }
    Ok(push_rx)
}

fn extension_init_configs(tower_config: &TowerConfig) -> ExtensionInitConfigMap {
    let mut init_configs: ExtensionInitConfigMap = tower_config
        .extensions
        .initialize
        .iter()
        .map(|(identity, config)| (identity.clone(), serde_json::Value::Object(config.clone())))
        .collect();
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
    rename_coordinator: Arc<RenameCoordinator>,
    cancelled: Option<Arc<AtomicBool>>,
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

    let publication = state
        .read()
        .map_err(|_| WatcherError::ChannelClosed)?
        .publication
        .clone();
    let watcher = NotifyWatcherAdapter::with_publication(
        workspace_root.to_path_buf(),
        watcher_workspace,
        watcher_index,
        Box::new(storage_for_watcher),
        watcher_ext_host,
        doc_sync,
        echo_set,
        rename_coordinator,
        cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
        publication,
    )?;

    let _ = writeln!(
        std::io::stderr(),
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

    let _ = writeln!(
        std::io::stderr(),
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
    emit_lsp_startup_diagnostics(&tower_config, &workspace_root);
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
            None,
            #[cfg(feature = "startup-test-hooks")]
            &startup_test_hooks,
        )
        .map_err(|error| format!("{error} at {}", workspace_root.display()))?;
        let _ = writeln!(
            std::io::stderr(),
            "tower: reconciled index — {} added, {} removed, {} unchanged",
            reconciliation.added,
            reconciliation.removed,
            reconciliation.unchanged
        );
        let storage_for_watcher = storage.try_clone();
        let state = Arc::new(RwLock::new(EngineState::new(
            workspace,
            index,
            Box::new(storage),
            Box::new(RealFs::new(&workspace_root)),
        )));
        inject_extension_host(&state, &ext_registry)?;
        let rename_coordinator = Arc::new(RenameCoordinator::default());
        let watcher = start_watcher(
            &workspace_root,
            &state,
            storage_for_watcher,
            &ext_registry,
            echo_set,
            Arc::clone(&rename_coordinator),
            None,
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
        Ok((state, watcher, rename_coordinator))
    };
    #[cfg(feature = "startup-test-hooks")]
    let (prepared, mut reconciliation_metrics) = collect_reconciliation_metrics(prepare);
    #[cfg(not(feature = "startup-test-hooks"))]
    let prepared = prepare();
    let (state, _watcher, rename_coordinator) = prepared?;
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
        None,
    )?;
    replay_initial_index_to_extensions(&state, &ext_registry)?;
    let diag_reader: Arc<dyn DiagnosticsReader> = Arc::new(NoOpDiagnosticsReader);

    Ok(EngineHandle {
        state,
        ext_registry,
        push_rx: Some(push_rx),
        diag_reader,
        rename_coordinator,
        _watcher,
    })
}

fn emit_lsp_startup_diagnostics(tower_config: &TowerConfig, workspace_root: &Path) {
    for outcome in validate_lsp_commands(
        &tower_config.lsp,
        workspace_root,
        std::env::var_os("PATH").as_deref(),
    ) {
        let diagnostic = serde_json::json!({ "outcome": outcome });
        let _ = writeln!(
            std::io::stderr(),
            "tower: LSP startup diagnostic {diagnostic}"
        );
    }
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
    let _ = writeln!(
        std::io::stderr(),
        "tower: reconciliation-metrics {{\"content_reads\":{},\"content_hashes\":{},\"file_record_rewrites\":{}}}",
        metrics.content_reads,
        metrics.content_hashes,
        metrics.file_record_rewrites
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

#[cfg(test)]
mod fence_contract_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn production_fence_errors_preserve_typed_cause_identity_and_fixed_class() {
        let id = ExtensionId::new("configured");
        let unsupported = classify_runtime_fence(
            &id,
            Err(InvokeError::ToolNotFound("private backend text".into())),
            1,
            false,
        )
        .unwrap_err();
        assert!(matches!(
            unsupported.error,
            CatchUpError::FenceUnsupported { .. }
        ));
        let mismatch = classify_runtime_fence(
            &id,
            Ok(json!({"epoch":9,"processed_events":1,"ready":true})),
            1,
            false,
        )
        .unwrap_err();
        assert!(matches!(mismatch.error, CatchUpError::FenceMismatch { .. }));
        let timeout = classify_runtime_fence(
            &id,
            Err(InvokeError::Fault(ExtensionFault::Timeout)),
            1,
            false,
        )
        .unwrap_err();
        assert!(matches!(timeout.error, CatchUpError::DeliveryFailed { .. }));
        assert_eq!(timeout.failure_class, StartupFailureClass::ExtensionTimeout);
        for failure in [unsupported, mismatch, timeout] {
            assert_eq!(failure.extension_id, id);
            assert!(!format!("{failure:?}").contains("private backend"));
        }
    }

    #[test]
    fn unknown_readiness_controls_fail_the_owning_family_registration() {
        struct Initialized(extension_protocol::ExtensionManifest);
        impl crate::domain::extension_host::ExtensionInstance for Initialized {
            fn manifest(&self) -> &extension_protocol::ExtensionManifest {
                &self.0
            }
            fn call_tool(
                &mut self,
                _: &str,
                _: serde_json::Value,
            ) -> Result<serde_json::Value, crate::domain::extension_host::ExtensionCallError>
            {
                panic!("registration must not invoke tools")
            }
            fn deliver_event(
                &mut self,
                _: extension_protocol::Event,
            ) -> Result<(), extension_protocol::ExtensionFault> {
                Ok(())
            }
            fn shutdown(&mut self) {}
        }
        for (control, valid, empty) in [
            ("probe", true, false),
            ("undeclared", false, false),
            ("probe", false, true),
        ] {
            let mut manifest = toml::from_str::<extension_protocol::ExtensionManifest>(
                r#"
name = "configured"
version = "0.1.0"
command = ["unused"]
activation = "eager"
[[tools]]
name = "probe"
description = "initialized control"
schema_json = '{}'
"#,
            )
            .unwrap();
            if empty {
                manifest.tools.clear();
            }
            let publisher = Arc::new(CommittedEventPublisher::new(1));
            let (instance, delivery) = super::super::delivery::OrderedExtension::spawn(
                Box::new(Initialized(manifest)),
                Arc::clone(&publisher),
                vec![],
                true,
            )
            .unwrap();
            publisher
                .subscribe(ExtensionId::new("configured"), delivery)
                .unwrap();
            let mut registry = ExtensionRegistry::new();
            registry.register(instance).unwrap();
            let registry = Arc::new(RwLock::new(registry));
            let mut config = TowerConfig::default();
            config.extensions.readiness.insert(
                "configured".into(),
                crate::adapters::config::ReadinessFamily::Ast,
            );
            config
                .extensions
                .readiness_controls
                .insert("configured".into(), vec![control.into()]);
            let readiness = ReadinessController::new(1);
            readiness
                .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
                .unwrap();
            register_extension_readiness(&config, &readiness, &registry, &publisher).unwrap();
            let snapshot = readiness.snapshot();
            let member = snapshot
                .extensions
                .iter()
                .find(|e| e.extension_id.as_str() == "configured")
                .unwrap();
            if valid {
                assert!(matches!(
                    member.readiness,
                    CapabilityReadiness::Preparing { .. }
                ));
            } else {
                assert_eq!(
                    member.readiness,
                    CapabilityReadiness::Failed {
                        failure_class: StartupFailureClass::ExtensionProtocol
                    }
                );
            }
        }
    }

    #[test]
    fn actual_hostcall_edits_share_the_commit_journal_and_reject_stale_versions() {
        use crate::adapters::mcp::native_tools::NativeToolRegistry;
        use crate::adapters::mcp::registry::ToolRegistry;
        use crate::adapters::{InMemoryFs, InMemoryStorage};
        let publisher = Arc::new(CommittedEventPublisher::new(12));
        let mut engine = EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(InMemoryFs::new()),
        );
        engine.publication = Some(Arc::clone(&publisher));
        let state = Arc::new(RwLock::new(engine));
        let mut native = NativeToolRegistry::new(Arc::clone(&state));
        native
            .call(
                "tower_create_file",
                json!({"path":"host.rs","content":"before"}),
            )
            .unwrap();
        let (mut extension, delivery) = {
            let _commit = publisher.commit_lock();
            let (extension, delivery) = super::super::delivery::OrderedExtension::spawn(
                Box::new(super::super::delivery::tests::Recorder::new(false)),
                Arc::clone(&publisher),
                Vec::new(),
                true,
            )
            .unwrap();
            publisher
                .subscribe(ExtensionId::new("recorder"), Arc::clone(&delivery))
                .unwrap();
            (extension, delivery)
        };
        let host = EngineApplyEditsHost::new(state);
        let request = |base_hash| WorkspaceApplyEditsRequest {
            edits: vec![crate::ports::inbound::WorkspaceEditSpan {
                path: crate::domain::RelativePath::new("host.rs"),
                start_byte: 0,
                end_byte: 6,
                replacement: "after".into(),
                base_hash: Some(base_hash),
            }],
            dry_run: Some(false),
        };
        let hash = crate::domain::mutation::compute_content_version(b"before");
        assert_eq!(
            host.apply_batch_edits(request(hash.clone()))
                .unwrap()
                .files_changed,
            1
        );
        assert_eq!(
            host.apply_batch_edits(request(hash)).unwrap().files_changed,
            0
        );
        let target = {
            let _commit = publisher.commit_lock();
            delivery.cut()
        };
        assert_eq!(target.recv_timeout(Duration::from_secs(2)).unwrap(), Ok(1));
        assert_eq!(publisher.capture_cut().unwrap().sequence, 2);
        assert_eq!(
            extension
                .call_tool("query", json!({}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        extension.shutdown();
    }

    #[test]
    fn actual_mcp_watcher_and_hostcall_commits_reach_one_fifo_delivery_owner() {
        use crate::adapters::mcp::native_tools::NativeToolRegistry;
        use crate::adapters::mcp::registry::ToolRegistry;
        use crate::adapters::watcher::{EventProcessor, WatchEvent};
        use crate::ports::NoOpExtensionHost;
        let root = tempfile::tempdir().unwrap();
        let (storage, workspace, index) = open_storage(root.path()).unwrap();
        let watcher_storage = storage.try_clone();
        let publisher = Arc::new(CommittedEventPublisher::new(13));
        let mut engine = EngineState::new(
            workspace,
            index,
            Box::new(storage),
            Box::new(RealFs::new(root.path())),
        );
        engine.publication = Some(Arc::clone(&publisher));
        let state = Arc::new(RwLock::new(engine));
        let mut watcher = EventProcessor::new(
            root.path().to_path_buf(),
            state.read().unwrap().workspace_arc(),
            state.read().unwrap().index_arc(),
            Box::new(watcher_storage),
            Box::new(NoOpExtensionHost),
        )
        .with_publication(Some(Arc::clone(&publisher)));
        let (mut extension, delivery) = {
            let _commit = publisher.commit_lock();
            let (extension, delivery) = super::super::delivery::OrderedExtension::spawn(
                Box::new(super::super::delivery::tests::Recorder::new(false)),
                Arc::clone(&publisher),
                Vec::new(),
                true,
            )
            .unwrap();
            publisher
                .subscribe(ExtensionId::new("recorder"), Arc::clone(&delivery))
                .unwrap();
            (extension, delivery)
        };
        let mut native = NativeToolRegistry::new(Arc::clone(&state));
        native
            .call(
                "tower_create_file",
                json!({"path":"mcp.rs","content":"first"}),
            )
            .unwrap();
        std::fs::write(root.path().join("watcher.rs"), "watcher").unwrap();
        watcher
            .process_event(WatchEvent::Create(root.path().join("watcher.rs")))
            .unwrap();
        let host = EngineApplyEditsHost::new(Arc::clone(&state));
        let result = host
            .apply_batch_edits(WorkspaceApplyEditsRequest {
                edits: vec![crate::ports::inbound::WorkspaceEditSpan {
                    path: crate::domain::RelativePath::new("mcp.rs"),
                    start_byte: 0,
                    end_byte: 5,
                    replacement: "hostcall".into(),
                    base_hash: Some(crate::domain::mutation::compute_content_version(b"first")),
                }],
                dry_run: Some(false),
            })
            .unwrap();
        assert_eq!(result.files_changed, 1);
        native.call("tower_reindex", json!({})).unwrap();
        native
            .call("tower_delete_file", json!({"path":"watcher.rs"}))
            .unwrap();
        let target = {
            let _commit = publisher.commit_lock();
            delivery.cut()
        };
        assert_eq!(target.recv_timeout(Duration::from_secs(2)).unwrap(), Ok(5));
        let events = extension.call_tool("query", json!({})).unwrap();
        let paths: Vec<_> = events
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["path"].as_str().unwrap())
            .collect();
        // Reindex refreshes mcp.rs' filesystem metadata after the HostCall.
        assert_eq!(
            paths,
            ["mcp.rs", "watcher.rs", "mcp.rs", "mcp.rs", "watcher.rs"]
        );
        assert_eq!(events[4]["type"], "FileDeleted");
        extension.shutdown();
    }

    #[test]
    fn pending_fence_expiry_is_timeout_and_malformed_evidence_is_protocol_failure() {
        let pending = json!({"epoch":1,"processed_events":2,"ready":false});
        assert_eq!(
            classify_fence_response(Ok(pending.clone()), 3, false),
            Ok(false)
        );
        assert_eq!(
            classify_fence_response(Ok(pending), 3, true),
            Err(StartupFailureClass::ExtensionTimeout)
        );
        for response in [
            json!({"epoch":2,"processed_events":3,"ready":true}),
            json!({"epoch":1,"processed_events":4,"ready":true}),
            json!({"epoch":1,"processed_events":4,"ready":false}),
            json!({"ready":false}),
        ] {
            assert_eq!(
                classify_fence_response(Ok(response), 3, true),
                Err(StartupFailureClass::ExtensionProtocol)
            );
        }
        assert_eq!(
            classify_fence_response(
                Ok(json!({"epoch":1,"processed_events":3,"ready":true})),
                3,
                false
            ),
            Ok(true)
        );
    }

    #[test]
    fn catch_up_failures_preserve_timeout_crash_and_protocol_classes() {
        let timeout = InvokeError::Fault(ExtensionFault::Timeout);
        let crashed = InvokeError::Fault(ExtensionFault::Crashed { code: Some(17) });
        assert_eq!(
            catch_up_failure_class(Some(&timeout)),
            StartupFailureClass::ExtensionTimeout
        );
        assert_eq!(
            catch_up_failure_class(Some(&crashed)),
            StartupFailureClass::ExtensionCrash
        );
        for error in [
            InvokeError::Fault(ExtensionFault::ProtocolError {
                message: "private".into(),
            }),
            InvokeError::Fault(ExtensionFault::Quarantined),
            InvokeError::ToolNotFound("private".into()),
            InvokeError::Application(crate::domain::extension_host::ExtensionApplicationError {
                code: -1,
                message: "private".into(),
                data: None,
            }),
        ] {
            assert_eq!(
                catch_up_failure_class(Some(&error)),
                StartupFailureClass::ExtensionProtocol
            );
        }
        assert_eq!(
            catch_up_failure_class(None),
            StartupFailureClass::ExtensionProtocol
        );
    }
}
