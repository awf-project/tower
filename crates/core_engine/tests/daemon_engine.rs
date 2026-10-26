//! Engine-builder smoke test (Task 6).
// Feature: B001
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::Duration;

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::lsp::validate_lsp_commands;
use core_engine::adapters::config::{LspConfig, LspServerConfig, TowerConfig};
use core_engine::adapters::daemon::engine::{
    PreparationError, build_early_context, build_engine, start_workspace_preparation,
};
use core_engine::adapters::daemon::socket::{WorkspaceDaemonOwner, socket_path};
use core_engine::adapters::daemon::wire::{ClientRole, Handshake};
use core_engine::adapters::daemon::{
    CapabilityDependency, CapabilityReadiness, CatchUpError, CommittedEventBatch,
    CommittedEventPublisher, CommittedFileEvent, DaemonLifecycle, ExtensionCatchUpCoordinator,
    ExtensionDependencyPolicy, PublicationError, ReadinessController, SnapshotEntry,
};
use core_engine::adapters::fs::workspace_scan;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::mcp::types::ToolError;
use core_engine::adapters::watcher::WatcherError;
use core_engine::adapters::{InMemoryStorage, NotifyWatcherAdapter, SledStorageAdapter};
use core_engine::domain::extension_host::ExtensionId;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::token::Token;
use core_engine::domain::{FileId, ProjectWorkspace, RelativePath};
use core_engine::ports::{ExtensionHostPort, NoOpExtensionHost, PortError};
use extension_protocol::{LspOutcomeCode, LspOutcomePhase, LspOutcomeStatus};
use tempfile::tempdir;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn early_context_fixture(
    workspace: &Path,
    config: TowerConfig,
) -> (
    WorkspaceDaemonOwner,
    Arc<core_engine::adapters::daemon::engine::EarlyDaemonContext>,
) {
    let owner = WorkspaceDaemonOwner::acquire(workspace).expect("acquire daemon ownership");
    let opts = GlobalOpts {
        workspace_dir: Some(workspace.to_path_buf()),
        extensions_dir: None,
    };
    let context = build_early_context(&opts, config, &owner).expect("build early daemon context");
    (owner, Arc::new(context))
}

#[test]
fn early_context_exposes_readiness_without_engine_state() {
    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());

    let snapshot = context.readiness().snapshot();
    assert_eq!(snapshot.generation, 1);
    assert!(matches!(
        snapshot.workspace,
        CapabilityReadiness::Preparing { .. }
    ));
}

#[test]
fn unlimited_preparation_wait_remains_pending_until_stopping() {
    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());
    let (result_tx, result_rx) = mpsc::channel();
    let waiting = Arc::clone(&context);

    let waiter = std::thread::spawn(move || {
        result_tx
            .send(waiting.wait_prepared(None))
            .expect("report unlimited wait result");
    });

    assert!(
        result_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "an absent preparation budget must not synthesize an expiry"
    );
    context
        .readiness()
        .transition_daemon(DaemonLifecycle::Stopping)
        .expect("stopping wakes a pending unlimited preparation wait");
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_secs(1)),
        Ok(Err(
            core_engine::adapters::daemon::ReadinessWaitError::Stopping { .. }
        ))
    ));
    waiter
        .join()
        .expect("unlimited waiter joins after stopping");
}

#[test]
fn finite_preparation_and_inventory_waits_report_timeout() {
    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());

    assert!(matches!(
        context.wait_preparation_complete(Some(Duration::ZERO)),
        Err(core_engine::adapters::daemon::ReadinessWaitError::Timeout { .. })
    ));
    assert!(matches!(
        context.wait_extension_inventory(Some(Duration::ZERO)),
        Err(core_engine::adapters::daemon::ReadinessWaitError::Timeout { .. })
    ));
}

#[test]
fn workspace_preparation_runs_authoritative_stages_in_order() {
    let workspace = tempdir().expect("create workspace");
    std::fs::write(workspace.path().join("indexed.rs"), "pub fn indexed() {}\n")
        .expect("seed workspace");
    let trace = workspace.path().join(".tower/preparation-stages.json");
    let output = Command::new(std::env::current_exe().expect("resolve test executable"))
        .args([
            "--ignored",
            "--exact",
            "workspace_preparation_stage_order_case",
            "--nocapture",
        ])
        // This hook is confined to the fixture subprocess.  The production
        // binary must ignore it, and parallel integration tests never share
        // its workspace or process environment.
        .env("TOWER_TEST_WORKSPACE", workspace.path())
        .env("TOWER_TEST_PREPARATION_STAGE_TRACE", &trace)
        .output()
        .expect("run isolated stage-order case");
    assert!(
        output.status.success(),
        "stage-order case failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "isolated subprocess fixture"]
fn workspace_preparation_stage_order_case() {
    let workspace = std::env::var_os("TOWER_TEST_WORKSPACE")
        .map(std::path::PathBuf::from)
        .expect("isolated workspace selector");
    let trace = std::env::var_os("TOWER_TEST_PREPARATION_STAGE_TRACE")
        .map(std::path::PathBuf::from)
        .expect("isolated stage trace path");
    let (_owner, context) = early_context_fixture(&workspace, TowerConfig::default());
    let readiness = context.readiness();

    let prepared = start_workspace_preparation(context)
        .expect("start workspace preparation")
        .join()
        .expect("complete workspace preparation");
    assert_eq!(readiness.snapshot().workspace, CapabilityReadiness::Ready);
    drop(prepared);

    let stages: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(trace).expect("startup hook recorded preparation stages"),
    )
    .expect("startup stage trace is JSON");
    assert_eq!(
        stages,
        [
            "storage_open_reload",
            "durable_reconciliation",
            "watcher_registration",
            "watcher_catch_up",
            "watcher_barrier",
            "extension_discovery",
            "capability_catch_up",
        ],
        "workspace readiness must follow the authoritative stage sequence"
    );
}

#[test]
fn workspace_preparation_wait_returns_prepared_engine_only_after_barrier() {
    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());
    let handle = start_workspace_preparation(context).expect("start workspace preparation");

    let published = handle
        .wait_workspace_ready(Duration::from_secs(5))
        .expect("watcher barrier publishes the prepared engine");
    let joined = handle.join().expect("preparation worker joins");
    assert!(Arc::ptr_eq(&published, &joined));
}

#[test]
fn workspace_preparation_wait_maps_timeout_failed_and_stopping() {
    for expected in ["timeout", "failed", "stopping"] {
        let output = Command::new(std::env::current_exe().expect("resolve test executable"))
            .args([
                "--ignored",
                "--exact",
                "workspace_preparation_wait_error_case",
                "--nocapture",
            ])
            .env("TOWER_TEST_PREPARATION_WAIT_ERROR", expected)
            .output()
            .expect("run isolated readiness wait case");
        assert!(
            output.status.success(),
            "{expected} readiness case failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "isolated subprocess fixture"]
fn workspace_preparation_wait_error_case() {
    let Some(expected) = std::env::var_os("TOWER_TEST_PREPARATION_WAIT_ERROR") else {
        return;
    };
    let expected = expected.to_string_lossy();
    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());
    let readiness = context.readiness();
    let handle = start_workspace_preparation(context).expect("start workspace preparation");

    match expected.as_ref() {
        "timeout" => assert!(matches!(
            handle.wait_workspace_ready(Duration::ZERO),
            Err(core_engine::adapters::daemon::ReadinessWaitError::Timeout {
                dependency: CapabilityDependency::Workspace,
                ..
            })
        )),
        "failed" => {
            readiness
                .transition_capability(
                    CapabilityDependency::Workspace,
                    CapabilityReadiness::Failed {
                        failure_class:
                            core_engine::adapters::daemon::StartupFailureClass::Reconciliation,
                    },
                )
                .expect("publish workspace failure");
            assert!(matches!(
                handle.wait_workspace_ready(Duration::from_secs(1)),
                Err(core_engine::adapters::daemon::ReadinessWaitError::Failed {
                    dependency: CapabilityDependency::Workspace,
                    failure_class:
                        core_engine::adapters::daemon::StartupFailureClass::Reconciliation,
                })
            ));
        }
        "stopping" => {
            readiness
                .transition_daemon(DaemonLifecycle::Stopping)
                .expect("publish stopping");
            assert!(matches!(
                handle.wait_workspace_ready(Duration::from_secs(1)),
                Err(
                    core_engine::adapters::daemon::ReadinessWaitError::Stopping {
                        dependency: CapabilityDependency::Workspace,
                    }
                )
            ));
        }
        other => panic!("unknown readiness case {other}"),
    }
}

#[test]
fn workspace_preparation_cancel_and_join_release_owned_resources() {
    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());
    let readiness = context.readiness();
    let handle = start_workspace_preparation(context).expect("start workspace preparation");

    handle.cancel();
    assert!(matches!(handle.join(), Err(PreparationError::Cancelled)));
    assert!(matches!(
        readiness.snapshot().workspace,
        CapabilityReadiness::Failed {
            failure_class: core_engine::adapters::daemon::StartupFailureClass::Cancelled
        } | CapabilityReadiness::Stopping
    ));
    sled::open(workspace.path().join(".tower/db")).expect("cancelled preparation releases Sled");
}

#[test]
fn dropping_workspace_preparation_cancels_and_joins_worker() {
    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());
    let handle = start_workspace_preparation(context).expect("start workspace preparation");

    drop(handle);
    sled::open(workspace.path().join(".tower/db")).expect("drop joins and releases Sled");
}

#[test]
fn workspace_preparation_reports_every_stage_failure_exactly() {
    for (hook, expected) in [
        ("cancelled", "Cancelled"),
        ("storage", "Storage"),
        ("reconciliation", "Reconciliation"),
        ("watcher_registration", "WatcherRegistration"),
        ("watcher_catch_up", "WatcherCatchUp"),
        ("watcher_barrier", "WatcherBarrier"),
        ("extension_discovery", "ExtensionDiscovery"),
    ] {
        let output = Command::new(std::env::current_exe().expect("resolve test executable"))
            .args([
                "--ignored",
                "--exact",
                "workspace_preparation_stage_failure_case",
                "--nocapture",
            ])
            .env("TOWER_TEST_PREPARATION_FAILURE", hook)
            .env("TOWER_TEST_EXPECTED_PREPARATION_ERROR", expected)
            .output()
            .expect("run isolated preparation failure case");
        assert!(
            output.status.success(),
            "{hook} did not produce {expected}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "isolated subprocess fixture"]
fn workspace_preparation_stage_failure_case() {
    let (Some(_hook), Some(expected)) = (
        std::env::var_os("TOWER_TEST_PREPARATION_FAILURE"),
        std::env::var_os("TOWER_TEST_EXPECTED_PREPARATION_ERROR"),
    ) else {
        return;
    };
    let workspace = tempdir().expect("create workspace");
    let owner = WorkspaceDaemonOwner::acquire(workspace.path()).expect("acquire daemon ownership");
    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: None,
    };
    let result = build_early_context(&opts, TowerConfig::default(), &owner)
        .and_then(|context| start_workspace_preparation(Arc::new(context)))
        .and_then(|handle| handle.join());
    let matches_expected = matches!(
        (expected.to_string_lossy().as_ref(), result),
        ("Cancelled", Err(PreparationError::Cancelled))
            | ("Storage", Err(PreparationError::Storage))
            | ("Reconciliation", Err(PreparationError::Reconciliation))
            | (
                "WatcherRegistration",
                Err(PreparationError::WatcherRegistration)
            )
            | ("WatcherCatchUp", Err(PreparationError::WatcherCatchUp))
            | ("WatcherBarrier", Err(PreparationError::WatcherBarrier))
            | (
                "ExtensionDiscovery",
                Err(PreparationError::ExtensionDiscovery)
            )
    );
    assert!(matches_expected, "unexpected preparation failure");
}

#[cfg(unix)]
#[test]
fn workspace_preparation_forwards_identity_scoped_initialize_configuration() {
    use std::os::unix::fs::PermissionsExt as _;

    let workspace = tempdir().expect("create workspace");
    let extensions = workspace.path().join("extensions");
    for identity in ["alpha", "beta", "without-config", "lsp", "debug"] {
        let directory = extensions.join(identity);
        std::fs::create_dir_all(&directory).expect("create extension directory");
        let marker = workspace.path().join(format!("{identity}-initialize.json"));
        let script = directory.join("capture.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nread -r initialize\nprintf '%s\\n' \"$initialize\" > '{}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"type\":\"Initialized\",\"data\":{{\"tools\":[],\"events\":[],\"capabilities\":[]}}}}}}'\nwhile read -r request; do\n  case \"$request\" in *shutdown*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"type\":\"Ack\"}}}}';; esac\ndone\n",
                marker.display()
            ),
        )
        .expect("write extension fixture");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make extension executable");
        std::fs::write(
            directory.join("extension.toml"),
            format!(
                "name = \"{identity}\"\nversion = \"0.1.0\"\ncommand = [\"{}\"]\nactivation = \"eager\"\n[capabilities]\nrequired = []\n",
                script.display()
            ),
        )
        .expect("write extension manifest");
    }
    let mut config: TowerConfig = toml::from_str(
        r#"
[lsp.rust]
command = "rust-analyzer"
extensions = ["rs"]
args = ["--stdio"]

[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
args = ["--stdio"]
adapter_type = "lldb"
launch = { request = "launch", program = "target/debug/app" }
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
    )
    .expect("parse retained LSP and debug initialization configuration");
    config.extensions.initialize.insert(
        "alpha".to_owned(),
        serde_json::Map::from_iter([("mode".to_owned(), serde_json::json!("one"))]),
    );
    config.extensions.initialize.insert(
        "beta".to_owned(),
        serde_json::Map::from_iter([("mode".to_owned(), serde_json::json!("two"))]),
    );
    let owner = WorkspaceDaemonOwner::acquire(workspace.path()).expect("acquire daemon ownership");
    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: Some(extensions),
    };
    let context = build_early_context(&opts, config, &owner).expect("build early context");
    start_workspace_preparation(Arc::new(context))
        .expect("start preparation")
        .join()
        .expect("complete extension discovery");

    for (identity, expected) in [("alpha", Some("one")), ("beta", Some("two"))] {
        let initialize: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(workspace.path().join(format!("{identity}-initialize.json")))
                .expect("extension captured initialize request"),
        )
        .expect("initialize request is JSON");
        assert_eq!(
            initialize["params"]["extension_config"]["mode"],
            serde_json::json!(expected.expect("configured identity"))
        );
    }
    let absent: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.path().join("without-config-initialize.json"))
            .expect("unconfigured extension captured initialize request"),
    )
    .expect("initialize request is JSON");
    assert!(absent["params"]["extension_config"].is_null());

    let lsp: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.path().join("lsp-initialize.json"))
            .expect("LSP extension captured initialize request"),
    )
    .expect("initialize request is JSON");
    assert_eq!(
        lsp["params"]["extension_config"],
        serde_json::json!({
            "servers": {
                "rust": {
                    "command": "rust-analyzer",
                    "extensions": ["rs"],
                    "args": ["--stdio"],
                }
            },
            "idle_timeout_secs": null,
            "diagnostics_timeout_secs": null,
        })
    );

    let debug: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.path().join("debug-initialize.json"))
            .expect("debug extension captured initialize request"),
    )
    .expect("initialize request is JSON");
    assert_eq!(
        debug["params"]["extension_config"],
        serde_json::json!({
            "languages": {
                "rust": {
                    "extensions": ["rs"],
                    "command": "lldb-dap",
                    "args": ["--stdio"],
                    "adapter_type": "lldb",
                    "launch": {"request": "launch", "program": "target/debug/app"},
                    "default_timeout_secs": 15,
                    "idle_ttl_secs": 300,
                }
            },
            "record": null,
        })
    );
}

/// Holds the watcher worker inside a real lifecycle callback.  This makes the
/// queue-order assertion deterministic without reaching into its private
/// message channel or relying on a wall-clock race.
struct BlockingExtensionHost {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    blocked_once: AtomicBool,
}

impl ExtensionHostPort for BlockingExtensionHost {
    fn on_file_indexed(&self, _id: core_engine::domain::FileId, _path: &RelativePath) {}

    fn on_file_changed(&self, _id: core_engine::domain::FileId, _path: &RelativePath) {
        if !self.blocked_once.swap(true, Ordering::AcqRel) {
            self.entered.send(()).expect("report blocked watcher event");
            self.release
                .lock()
                .expect("release channel lock")
                .recv()
                .expect("release blocked watcher event");
        }
    }

    fn on_file_deleted(&self, _path: &RelativePath) {}

    fn declared_tools(
        &self,
    ) -> Vec<(
        core_engine::domain::ExtensionId,
        extension_protocol::ToolDecl,
    )> {
        Vec::new()
    }

    fn invoke(
        &self,
        tool_name: &str,
        _params: serde_json::Value,
        _cancellation: &dyn core_engine::domain::CancellationSignal,
    ) -> Result<serde_json::Value, core_engine::domain::InvokeError> {
        Err(core_engine::domain::InvokeError::ToolNotFound(
            tool_name.to_owned(),
        ))
    }
}

#[test]
fn build_engine_indexes_a_fresh_workspace() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"pub fn a() {}").unwrap();
    std::fs::write(dir.path().join("b.rs"), b"pub fn b() {}").unwrap();

    let opts = GlobalOpts {
        workspace_dir: Some(dir.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, TowerConfig::default()).expect("engine builds");

    let ws = handle.state.read().unwrap();
    let count = ws.workspace_arc().read().unwrap().all_file_ids().len();
    assert_eq!(count, 2, "two source files indexed");
}

/// The daemon lifecycle admission boundary is public to daemon assembly code;
/// keep this contract in an integration crate so it cannot accidentally rely on
/// private lifecycle state or test-only helpers.
#[test]
fn daemon_engine_public_readiness_admission_reports_ready_and_stopping() {
    let controller = ReadinessController::new(1);
    controller
        .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
        .expect("workspace readiness is published through the daemon boundary");
    controller
        .close_discovery()
        .expect("discovery closure is published through the daemon boundary");
    controller
        .transition_daemon(DaemonLifecycle::Ready)
        .expect("ready daemon is admitted through the public daemon boundary");

    let permit = controller
        .admit_dispatch(
            &[CapabilityDependency::Workspace],
            std::time::Duration::ZERO,
        )
        .expect("ready workspace dispatch is admitted");
    assert_eq!(permit.snapshot().workspace, CapabilityReadiness::Ready);
    drop(permit);

    controller
        .transition_daemon(DaemonLifecycle::Stopping)
        .expect("stopping is published through the public daemon boundary");
    assert!(matches!(
        controller.admit_dispatch(
            &[CapabilityDependency::Workspace],
            std::time::Duration::ZERO
        ),
        Err(
            core_engine::adapters::daemon::ReadinessWaitError::Stopping {
                dependency: CapabilityDependency::Workspace,
            }
        )
    ));
}

#[cfg(unix)]
#[test]
fn repaired_additions_replay_after_commit_and_watcher_catch_up() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    let workspace = tempdir().expect("create workspace");
    let observations = tempdir().expect("create replay observation directory");
    std::fs::write(
        workspace.path().join("persisted.rs"),
        "pub fn persisted() {}\n",
    )
    .expect("seed persisted file");
    let db_path = workspace.path().join(".tower/db");
    std::fs::create_dir_all(&db_path).expect("create database directory");
    let (mut storage, mut initial_workspace, mut initial_index) =
        SledStorageAdapter::open(&db_path).expect("open initial storage");
    workspace_scan(
        workspace.path(),
        &mut storage,
        &mut initial_workspace,
        &mut initial_index,
    )
    .expect("persist initial workspace");
    drop(storage);

    std::fs::write(
        workspace.path().join("repaired.rs"),
        "pub fn repaired() {}\n",
    )
    .expect("create offline addition");
    let extension_dir = workspace.path().join("extensions/recorder");
    std::fs::create_dir_all(&extension_dir).expect("create recorder extension directory");
    // Keep observer writes outside the indexed tree: otherwise they wake the
    // watcher and can contend with the lock-availability assertions below.
    let replay_marker = observations.path().join("replay.json");
    let script = extension_dir.join("record-replay.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nread -r initialize\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"type\":\"Initialized\",\"data\":{{\"tools\":[],\"events\":[\"event/fileIndexed\"],\"capabilities\":[\"read_file\"]}}}}}}'\nwhile IFS= read -r event; do\n  request_id=$(printf '%s\\n' \"$event\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')\n  case \"$event\" in\n    *repaired.rs*)\n      printf '%s\\n' \"$event\" >> '{}'\n      printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":999,\"method\":\"workspace/readFile\",\"params\":{{\"path\":\"repaired.rs\"}}}}'\n      IFS= read -r response\n      printf '%s\\n' \"$response\" >> '{}'\n      ;;\n  esac\n  if [ -n \"$request_id\" ]; then\n    printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{{\"type\":\"Ack\"}}}}\\n' \"$request_id\"\n  fi\ndone\n",
            replay_marker.display(),
            replay_marker.display()
        ),
    )
    .expect("write recorder extension");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make recorder executable");
    std::fs::write(
        extension_dir.join("extension.toml"),
        format!(
            "name = \"recorder\"\nversion = \"0.1.0\"\ncommand = [\"{}\"]\nactivation = \"eager\"\n\n[events]\nsubscribe = [\"event/fileIndexed\"]\n\n[capabilities]\nrequired = [\"read_file\"]\n",
            script.display()
        ),
    )
    .expect("write recorder manifest");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: Some(workspace.path().join("extensions")),
    };
    let engine = build_engine(&opts, TowerConfig::default()).expect("repaired engine starts");
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(&replay_marker)
        .map(|replay| !replay.contains("\"result\""))
        .unwrap_or(true)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let replay = std::fs::read_to_string(&replay_marker)
        .expect("committed repaired addition is replayed exactly once");
    assert_eq!(replay.matches("\"method\":\"deliverEvent\"").count(), 1);
    assert!(replay.contains("repaired.rs"));
    assert!(
        replay.contains("\"result\""),
        "replay callback HostCall must complete: {replay}"
    );
    let state = engine
        .state
        .try_read()
        .expect("replay must not hold the engine-state lock");
    let workspace_state = state.workspace_arc();
    let index_state = state.index_arc();
    drop(state);
    let workspace_guard = workspace_state
        .try_read()
        .expect("replay must not hold the workspace lock");
    let repaired_id = workspace_guard
        .get_by_path(&RelativePath::new("repaired.rs"))
        .expect("repaired addition is visible before replay completes");
    drop(workspace_guard);
    assert_eq!(
        index_state
            .try_read()
            .expect("replay must not hold the index lock")
            .query(&Token::new("repaired"))
            .collect::<Vec<_>>(),
        vec![repaired_id]
    );
}

#[test]
fn watcher_catch_up_converges_create_delete_and_rename() {
    let dir = tempdir().expect("create workspace");
    let deleted = dir.path().join("deleted.rs");
    let renamed_from = dir.path().join("renamed_from.rs");
    let renamed_to = dir.path().join("renamed_to.rs");
    let created = dir.path().join("created.rs");
    std::fs::write(&deleted, "pub fn deleted() {}\n").expect("seed deleted file");
    std::fs::write(&renamed_from, "pub fn renamed() {}\n").expect("seed renamed file");

    let mut storage = InMemoryStorage::new();
    let mut initial_workspace = ProjectWorkspace::new();
    let mut initial_index = InvertedIndex::new();
    workspace_scan(
        dir.path(),
        &mut storage,
        &mut initial_workspace,
        &mut initial_index,
    )
    .expect("initial scan succeeds");
    let workspace = Arc::new(RwLock::new(initial_workspace));
    let index = Arc::new(RwLock::new(initial_index));

    let (mutate_tx, mutate_rx) = mpsc::channel();
    let (mutated_tx, mutated_rx) = mpsc::channel();
    let mutation = std::thread::spawn(move || {
        mutate_rx.recv().expect("mutation boundary opens");
        std::fs::write(created, "pub fn created() {}\n").expect("create file");
        std::fs::remove_file(deleted).expect("delete file");
        std::fs::rename(renamed_from, renamed_to).expect("rename file");
        mutated_tx.send(()).expect("report completed mutations");
    });

    mutate_tx.send(()).expect("open mutation boundary");
    mutated_rx.recv().expect("mutations complete");
    mutation.join().expect("mutation thread exits");

    let watcher = NotifyWatcherAdapter::new(
        dir.path().to_path_buf(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        Box::new(storage),
        Box::new(NoOpExtensionHost),
    )
    .expect("watcher registers");
    watcher.request_catch_up().expect("catch-up succeeds");
    watcher.barrier().expect("barrier succeeds");

    let workspace = workspace.read().expect("workspace lock");
    let index = index.read().expect("index lock");
    let created_id = workspace
        .get_by_path(&RelativePath::new("created.rs"))
        .expect("created file is indexed");
    let renamed_id = workspace
        .get_by_path(&RelativePath::new("renamed_to.rs"))
        .expect("renamed destination is indexed");

    assert_eq!(workspace.all_file_ids().len(), 2);
    assert!(
        workspace
            .get_by_path(&RelativePath::new("deleted.rs"))
            .is_none()
    );
    assert!(
        workspace
            .get_by_path(&RelativePath::new("renamed_from.rs"))
            .is_none()
    );
    assert_eq!(
        index.query(&Token::new("created")).collect::<Vec<_>>(),
        vec![created_id]
    );
    assert_eq!(
        index.query(&Token::new("renamed")).collect::<Vec<_>>(),
        vec![renamed_id]
    );
    assert_eq!(index.posting_count(&Token::new("deleted")), 0);
    assert_eq!(index.posting_count(&Token::new("from")), 0);
}

#[test]
fn watcher_catch_up_preserves_typed_failure_mapping_and_confidentiality() {
    let dir = tempdir().expect("create workspace");
    let secret_contents = "do-not-expose-catch-up-file-contents";
    std::fs::write(dir.path().join("tracked.rs"), secret_contents).expect("seed tracked file");

    let mut storage = InMemoryStorage::new();
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    workspace_scan(dir.path(), &mut storage, &mut workspace, &mut index)
        .expect("initial scan succeeds");
    storage.fail_next_reconciliation_before_commit_for_test();

    let watcher = NotifyWatcherAdapter::new(
        dir.path().to_path_buf(),
        Arc::new(RwLock::new(workspace)),
        Arc::new(RwLock::new(index)),
        Box::new(storage),
        Box::new(NoOpExtensionHost),
    )
    .expect("watcher registers");

    let error = watcher
        .request_catch_up()
        .expect_err("persistence failure is returned to the caller");
    assert!(matches!(
        error,
        WatcherError::Storage(PortError::ReconciliationFailed {
            operation: "apply_reconciliation_delta",
            ..
        })
    ));
    assert!(
        !error.to_string().contains(secret_contents),
        "catch-up errors must never include file contents"
    );
}

#[test]
fn watcher_catch_up_wraps_scan_errors_without_file_contents() {
    let dir = tempdir().expect("create workspace");
    let secret_contents = "do-not-expose-scan-file-contents";
    std::fs::write(dir.path().join("tracked.rs"), secret_contents).expect("seed tracked file");

    let watcher = NotifyWatcherAdapter::new(
        dir.path().to_path_buf(),
        Arc::new(RwLock::new(ProjectWorkspace::new())),
        Arc::new(RwLock::new(InvertedIndex::new())),
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpExtensionHost),
    )
    .expect("watcher registers");
    std::fs::remove_dir_all(dir.path()).expect("remove workspace before catch-up");

    let error = watcher
        .request_catch_up()
        .expect_err("scan failure is returned to the caller");
    let WatcherError::Storage(PortError::ReadFailed(message)) = error else {
        panic!("non-persistence scan errors must map to WatcherError::Storage(ReadFailed)");
    };
    assert!(message.starts_with("catch_up failed:"));
    assert!(
        !message.contains(secret_contents),
        "catch-up errors must never include file contents"
    );
}

#[test]
fn watcher_catch_up_maps_acknowledgement_and_enqueue_disconnects_to_channel_closed() {
    let dir = tempdir().expect("create workspace");
    let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let index = Arc::new(RwLock::new(InvertedIndex::new()));

    let poison_workspace = Arc::clone(&workspace);
    assert!(
        std::thread::spawn(move || {
            let _guard = poison_workspace.write().expect("workspace lock");
            panic!("poison workspace for worker-disconnect coverage");
        })
        .join()
        .is_err(),
        "test fixture must poison the worker's workspace lock"
    );

    let watcher = NotifyWatcherAdapter::new(
        dir.path().to_path_buf(),
        workspace,
        index,
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpExtensionHost),
    )
    .expect("watcher registers");

    assert!(matches!(
        watcher.request_catch_up(),
        Err(WatcherError::ChannelClosed)
    ));
    assert!(matches!(
        watcher.request_catch_up(),
        Err(WatcherError::ChannelClosed)
    ));
}

#[test]
fn watcher_startup_failure_prevents_extension_loading() {
    let failed_root = tempdir().expect("create failed startup workspace");
    let failed_workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let poison_workspace = Arc::clone(&failed_workspace);
    assert!(
        std::thread::spawn(move || {
            let _guard = poison_workspace.write().expect("workspace lock");
            panic!("terminate watcher worker during startup catch-up");
        })
        .join()
        .is_err()
    );
    let failed_watcher = NotifyWatcherAdapter::new(
        failed_root.path().to_path_buf(),
        failed_workspace,
        Arc::new(RwLock::new(InvertedIndex::new())),
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpExtensionHost),
    )
    .expect("watcher registers before startup catch-up");
    assert!(matches!(
        failed_watcher.request_catch_up(),
        Err(WatcherError::ChannelClosed)
    ));
    assert!(matches!(
        failed_watcher.barrier(),
        Err(WatcherError::ChannelClosed)
    ));

    let retry_root = tempdir().expect("create retry workspace");
    std::fs::write(
        retry_root.path().join("retry.rs"),
        "pub fn clean_retry() {}\n",
    )
    .expect("seed retry addition");
    let retry_workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let retry_watcher = NotifyWatcherAdapter::new(
        retry_root.path().to_path_buf(),
        Arc::clone(&retry_workspace),
        Arc::new(RwLock::new(InvertedIndex::new())),
        Box::new(InMemoryStorage::new()),
        Box::new(NoOpExtensionHost),
    )
    .expect("clean retry watcher registers");
    retry_watcher
        .request_catch_up()
        .expect("first clean retry catch-up succeeds");
    retry_watcher
        .barrier()
        .expect("first clean retry barrier succeeds");
    assert!(
        retry_workspace
            .read()
            .expect("retry workspace lock")
            .get_by_path(&RelativePath::new("retry.rs"))
            .is_some()
    );

    let opts = GlobalOpts {
        workspace_dir: Some(retry_root.path().to_path_buf()),
        extensions_dir: None,
    };
    let _engine = build_engine(&opts, TowerConfig::default())
        .expect("daemon startup succeeds after the channel-closed attempt");
}

#[test]
fn watcher_barrier_waits_for_earlier_worker_event() {
    let dir = tempdir().expect("create workspace");
    let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
    let index = Arc::new(RwLock::new(InvertedIndex::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let host = BlockingExtensionHost {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        blocked_once: AtomicBool::new(false),
    };
    let watcher = NotifyWatcherAdapter::new(
        dir.path().to_path_buf(),
        Arc::clone(&workspace),
        Arc::clone(&index),
        Box::new(InMemoryStorage::new()),
        Box::new(host),
    )
    .expect("watcher registers");

    std::fs::write(dir.path().join("queued.rs"), "pub fn queued() {}\n")
        .expect("create queued file");
    entered_rx
        .recv()
        .expect("watcher event reaches controlled callback");

    std::thread::scope(|scope| {
        let (started_tx, started_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let watcher_ref = &watcher;
        let barrier_thread = scope.spawn(move || {
            started_tx.send(()).expect("barrier call starts");
            completed_tx
                .send(watcher_ref.barrier())
                .expect("barrier result delivered");
        });
        started_rx.recv().expect("barrier is enqueued after event");
        assert!(
            completed_rx.try_recv().is_err(),
            "barrier must not acknowledge while the earlier event is pending"
        );

        release_tx.send(()).expect("release earlier event");
        assert!(
            completed_rx
                .recv()
                .expect("barrier result available after release")
                .is_ok(),
            "barrier acknowledges after earlier event completes"
        );
        barrier_thread.join().expect("barrier caller exits");
    });

    assert!(
        workspace
            .read()
            .expect("workspace lock")
            .get_by_path(&RelativePath::new("queued.rs"))
            .is_some(),
        "the event preceding the barrier completes before acknowledgement"
    );
}

#[test]
fn f008_startup_validation_validates_all_language_entries_in_key_order_including_duplicate_extension_mappings()
 {
    let dir = tempdir().unwrap();
    let launched_marker = dir.path().join("valid-server-was-launched");
    let valid_server = dir.path().join("valid-f008-server");
    std::fs::write(
        &valid_server,
        format!("#!/bin/sh\ntouch '{}'\n", launched_marker.display()),
    )
    .expect("write valid LSP server fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(&valid_server, std::fs::Permissions::from_mode(0o755))
            .expect("make valid LSP server executable");
    }
    let config = LspConfig {
        servers: BTreeMap::from([
            (
                "zeta".to_owned(),
                LspServerConfig {
                    command: "./missing-zeta".to_owned(),
                    extensions: vec!["rs".to_owned()],
                    args: Vec::new(),
                },
            ),
            (
                "beta".to_owned(),
                LspServerConfig {
                    command: "./valid-f008-server".to_owned(),
                    extensions: vec!["rs".to_owned()],
                    args: Vec::new(),
                },
            ),
            (
                "alpha".to_owned(),
                LspServerConfig {
                    command: "   ".to_owned(),
                    extensions: vec!["rs".to_owned()],
                    args: Vec::new(),
                },
            ),
        ]),
        idle_timeout: None,
    };

    let outcomes = validate_lsp_commands(&config, dir.path(), None);

    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].language.as_deref(), Some("alpha"));
    assert_eq!(outcomes[0].command.as_deref(), Some("   "));
    assert_eq!(outcomes[0].code, LspOutcomeCode::InvalidCommand);
    assert_eq!(outcomes[1].language.as_deref(), Some("zeta"));
    assert_eq!(outcomes[1].command.as_deref(), Some("./missing-zeta"));
    assert_eq!(outcomes[1].code, LspOutcomeCode::ServerMissing);
    assert!(
        !launched_marker.exists(),
        "startup validation must resolve a valid command without launching its server"
    );
    for outcome in outcomes {
        assert_eq!(outcome.status, LspOutcomeStatus::Error);
        assert_eq!(outcome.phase, LspOutcomePhase::Startup);
        assert_eq!(outcome.operation, None);
        assert_eq!(outcome.path, None);
    }
}

#[cfg(unix)]
#[test]
fn f008_startup_validation_hands_an_explicit_empty_servers_map_to_the_lsp_extension() {
    let workspace = tempdir().expect("create workspace");
    let extension_dir = workspace.path().join("extensions/lsp");
    std::fs::create_dir_all(&extension_dir).expect("create LSP extension directory");
    let received_initialize = workspace.path().join("received-initialize.json");
    let script = extension_dir.join("capture-initialize.sh");
    std::fs::write(
        &script,
        format!(
            "read -r line\nprintf '%s' \"$line\" > '{}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"type\":\"Initialized\",\"data\":{{\"tools\":[],\"events\":[],\"capabilities\":[]}}}}}}'\n",
            received_initialize.display()
        ),
    )
    .expect("write LSP extension fixture");
    std::fs::write(
        extension_dir.join("extension.toml"),
        format!(
            "name = \"lsp\"\nversion = \"0.1.0\"\ncommand = [\"/bin/sh\", \"{}\"]\nactivation = \"eager\"\n",
            script.display()
        ),
    )
    .expect("write LSP extension manifest");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: Some(workspace.path().join("extensions")),
    };
    let _engine =
        build_engine(&opts, TowerConfig::default()).expect("build engine with LSP extension");

    let initialize: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&received_initialize)
            .expect("eager LSP extension receives initialize request"),
    )
    .expect("captured initialize request is JSON");
    assert_eq!(
        initialize["params"]["extension_config"],
        serde_json::json!({
            "servers": {},
            "idle_timeout_secs": null,
            "diagnostics_timeout_secs": null,
        }),
        "daemon must hand the explicit empty LSP configuration to its lsp extension"
    );
}

#[test]
fn daemon_produced_lsp_timeout_reaches_the_real_sidecar_session_pool() {
    let workspace = tempdir().expect("create workspace");
    std::fs::create_dir_all(workspace.path().join("src")).expect("create source directory");
    std::fs::write(workspace.path().join("src/lib.rs"), "fn probe() {}\n")
        .expect("write source file");
    let extension_dir = workspace.path().join("extensions/lsp");
    std::fs::create_dir_all(&extension_dir).expect("create LSP extension directory");
    let lsp_binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .join("target/debug/lsp_extension");
    std::fs::write(
        extension_dir.join("extension.toml"),
        format!(
            "name = \"lsp\"\nversion = \"0.1.0\"\ncommand = [\"{}\"]\nactivation = \"eager\"\n\n[[tools]]\nname = \"diagnostics\"\ndescription = \"fixture diagnostics\"\nschema_json = '{{\"type\":\"object\"}}'\n\n[capabilities]\nrequired = [\"read_file\"]\n",
            lsp_binary.display()
        ),
    )
    .expect("write LSP extension manifest");
    let config_source = format!(
        "[extensions]\nrequest_timeout_secs = 3\n\n[lsp.rust]\ncommand = {}\nargs = [\"diagnostics_push_absent\", {}]\nextensions = [\"rs\"]\n",
        serde_json::to_string(&fixture_lsp_server::binary_path()).expect("serialize fixture path"),
        serde_json::to_string(&workspace.path().join(".tower/lsp.log"))
            .expect("serialize log path"),
    );
    let config: TowerConfig = toml::from_str(&config_source).expect("parse daemon configuration");
    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: Some(workspace.path().join("extensions")),
    };
    let handle = build_engine(&opts, config).expect("daemon builds the real LSP sidecar");
    let mut registry = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    let error = registry
        .call(
            "tower_lsp_diagnostics",
            serde_json::json!({"path": "src/lib.rs"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect_err("the fixture withholds diagnostics until the daemon-produced pool deadline");
    assert!(matches!(
        error,
        ToolError::Application {
            code: -32_000,
            ref message,
            data: Some(ref data),
        } if message == "language server request failed"
            && data["outcome"]["code"] == "server_timeout"
            && data["outcome"]["timeout"] == serde_json::json!({
                "operation": "diagnostics", "layer": "lsp", "budget_ms": 2_000
            })
    ));
}

#[cfg(unix)]
#[test]
fn f008_startup_validation_emits_sanitized_diagnostics_before_a_tool_call_and_keeps_native_requests_available()
 {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    std::fs::write(dir.path().join("hello.txt"), "still serving").unwrap();
    std::fs::write(
        dir.path().join(".tower/config.toml"),
        r#"
[daemon]
idle_timeout_secs = 300

[lsp.rust]
command = "./missing-f008-server"
extensions = ["rs"]
args = ["F008_SENTINEL_ARGUMENT"]
"#,
    )
    .unwrap();

    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_tower"))
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(dir.path())
            .env("PATH", "F008_SENTINEL_PATH")
            .env("F008_SENTINEL_ENVIRONMENT", "F008_SENTINEL_ENV_VALUE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tower daemon"),
    );
    let stderr = child.0.stderr.take().expect("daemon stderr");
    let mut startup_lines = Vec::new();
    for line in BufReader::new(stderr).lines() {
        let line = line.expect("read daemon stderr");
        let listening = line.contains("daemon listening on");
        startup_lines.push(line);
        if listening {
            break;
        }
    }

    let diagnostic_index = startup_lines
        .iter()
        .position(|line| line.contains("\"outcome\""))
        .expect("startup must emit a structured LSP outcome");
    let listening_index = startup_lines
        .iter()
        .position(|line| line.contains("daemon listening on"))
        .expect("daemon must continue to serving state");
    assert!(diagnostic_index < listening_index);

    let diagnostic = &startup_lines[diagnostic_index];
    let json_start = diagnostic.find('{').expect("diagnostic JSON object");
    let payload: serde_json::Value =
        serde_json::from_str(&diagnostic[json_start..]).expect("structured startup diagnostic");
    assert_eq!(
        payload["outcome"],
        serde_json::json!({
            "status": "error",
            "code": "server_missing",
            "language": "rust",
            "command": "./missing-f008-server",
            "operation": null,
            "path": null,
            "timeout": null,
            "phase": "startup",
            "message": payload["outcome"]["message"].clone(),
        })
    );
    for secret in [
        "F008_SENTINEL_ARGUMENT",
        "F008_SENTINEL_ENVIRONMENT",
        "F008_SENTINEL_ENV_VALUE",
        "F008_SENTINEL_PATH",
    ] {
        assert!(!diagnostic.contains(secret), "diagnostic leaked {secret}");
    }

    let mut stream = std::os::unix::net::UnixStream::connect(socket_path(dir.path()))
        .expect("connect to daemon socket");
    stream
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .unwrap();
    stream
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"f008-test\",\"version\":\"1\"}}}\n",
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    assert!(
        response.contains("\"result\""),
        "initialize failed: {response}"
    );

    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .unwrap();
    stream
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"tower_read_file\",\"arguments\":{\"path\":\"hello.txt\"}}}\n",
        )
        .unwrap();
    response.clear();
    reader.read_line(&mut response).unwrap();
    assert!(
        response.contains("still serving"),
        "native request failed: {response}"
    );
}

fn catch_up_fixture() -> (
    Arc<CommittedEventPublisher>,
    ExtensionCatchUpCoordinator,
    ExtensionId,
) {
    let extension_id = ExtensionId::new("ast");
    let policy = ExtensionDependencyPolicy::new(
        HashMap::from([(extension_id.clone(), CapabilityDependency::Ast)]),
        HashMap::from([(
            extension_id.clone(),
            HashSet::from(["probe_catch_up".to_owned()]),
        )]),
    );
    let publisher = Arc::new(CommittedEventPublisher::new(7));
    let coordinator = ExtensionCatchUpCoordinator::new(Arc::clone(&publisher), Arc::new(policy));
    (publisher, coordinator, extension_id)
}

fn catch_up_snapshot(path: &str, index: u32, generation: u32) -> SnapshotEntry {
    SnapshotEntry {
        file_id: FileId::new_for_testing(index, generation),
        path: RelativePath::new(path),
    }
}

#[test]
fn catch_up_begin_captures_subscriber_snapshot_and_watermark_atomically() {
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let sequence = publisher
        .reserve_batch(1)
        .expect("reserve concurrent commit");
    let epoch = coordinator
        .begin(
            extension_id.clone(),
            vec![catch_up_snapshot("src/existing.rs", 0, 1)],
        )
        .expect("begin catch-up epoch");
    assert_eq!(epoch.extension_id(), &extension_id);
    assert_eq!(
        epoch.epoch(),
        1,
        "the first extension epoch is generation-local"
    );
    assert_eq!(
        publisher
            .capture_cut()
            .expect("a reserved but uncommitted batch is not visible in a cut")
            .sequence,
        0,
        "watermarks advance only for committed publication"
    );
    let batch = publisher
        .publish(
            sequence,
            vec![CommittedFileEvent::Indexed {
                file_id: FileId::new_for_testing(1, 1),
                path: RelativePath::new("src/concurrent.rs"),
            }],
        )
        .expect("publish concurrent commit");
    coordinator
        .accept_committed_batch(batch)
        .expect("subscriber observes post-watermark commit");
    assert_eq!(
        publisher
            .capture_cut()
            .expect("published batch advances the cut")
            .sequence,
        sequence
    );

    assert!(matches!(
        coordinator.begin(extension_id.clone(), Vec::new()),
        Err(CatchUpError::DuplicateEpoch { extension_id: id }) if id == extension_id
    ));
    let unknown = ExtensionId::new("unknown");
    assert!(matches!(
        coordinator.begin(unknown.clone(), Vec::new()),
        Err(CatchUpError::UnknownExtension { extension_id: id }) if id == unknown
    ));
    coordinator.cancel(epoch).expect("close fixture epoch");
}

#[test]
fn catch_up_committed_batches_preserve_mcp_watcher_and_hostcall_order() {
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let epoch = coordinator
        .begin(extension_id, Vec::new())
        .expect("begin epoch");
    for (path, event) in [
        ("mcp.rs", "indexed"),
        ("watcher.rs", "changed"),
        ("hostcall.rs", "deleted"),
    ] {
        let sequence = publisher.reserve_batch(1).expect("reserve producer batch");
        let path = RelativePath::new(path);
        let event = match event {
            "indexed" => CommittedFileEvent::Indexed {
                file_id: FileId::new_for_testing(sequence as u32, 1),
                path,
            },
            "changed" => CommittedFileEvent::Changed {
                file_id: FileId::new_for_testing(sequence as u32, 1),
                path,
            },
            _ => CommittedFileEvent::Deleted { path },
        };
        let batch = publisher
            .publish(sequence, vec![event])
            .expect("publish producer batch");
        coordinator
            .accept_committed_batch(batch)
            .expect("accept producer batch in commit order");
    }
    let cut = publisher.capture_cut().expect("capture finite cut");
    coordinator
        .deliver_through(&epoch, cut)
        .expect("deliver every committed producer batch in order");
}

#[test]
fn catch_up_publication_overflow_rejects_before_commit() {
    let publisher = CommittedEventPublisher::new(u64::MAX);
    assert_eq!(
        publisher.reserve_batch(usize::MAX),
        Err(PublicationError::Overflow)
    );
    assert_eq!(
        publisher
            .capture_cut()
            .expect("overflow does not advance watermark")
            .sequence,
        0
    );
}

#[test]
fn catch_up_failed_reservation_does_not_block_later_commit() {
    let publisher = CommittedEventPublisher::new(7);
    let failed = publisher.reserve_batch(1).expect("reserve failed mutation");
    let committed = publisher
        .reserve_batch(1)
        .expect("reserve successful mutation");

    publisher
        .abort(failed)
        .expect("record failed mutation without publishing events");
    let batch = publisher
        .publish(
            committed,
            vec![CommittedFileEvent::Deleted {
                path: RelativePath::new("committed.rs"),
            }],
        )
        .expect("a failed predecessor must not wedge publication");

    assert_eq!(
        batch.sequence, 1,
        "failed reservations do not create journal gaps"
    );
    assert_eq!(publisher.capture_cut().unwrap().sequence, 1);
}

#[test]
fn catch_up_snapshot_omits_pre_watermark_duplicates_and_stale_ids() {
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let epoch = coordinator
        .begin(extension_id, vec![catch_up_snapshot("src/live.rs", 0, 2)])
        .expect("begin from authoritative surviving snapshot");
    let sequence = publisher
        .reserve_batch(1)
        .expect("reserve post-cut deletion");
    let batch = publisher
        .publish(
            sequence,
            vec![CommittedFileEvent::Deleted {
                path: RelativePath::new("src/stale.rs"),
            }],
        )
        .expect("publish deletion");
    coordinator
        .accept_committed_batch(batch)
        .expect("queue deletion");
    coordinator
        .deliver_through(&epoch, publisher.capture_cut().expect("capture cut"))
        .expect("stale generations remain absent after replay");
}

#[test]
fn catch_up_changes_converge_before_capability_becomes_ready() {
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let epoch = coordinator
        .begin(extension_id, vec![catch_up_snapshot("src/a.rs", 0, 1)])
        .expect("begin epoch");
    let sequence = publisher
        .reserve_batch(3)
        .expect("reserve converging batch");
    let batch = publisher
        .publish(
            sequence,
            vec![
                CommittedFileEvent::Changed {
                    file_id: FileId::new_for_testing(0, 1),
                    path: RelativePath::new("src/a.rs"),
                },
                CommittedFileEvent::Indexed {
                    file_id: FileId::new_for_testing(1, 1),
                    path: RelativePath::new("src/b.rs"),
                },
                CommittedFileEvent::Deleted {
                    path: RelativePath::new("src/a.rs"),
                },
            ],
        )
        .expect("publish converging batch");
    coordinator
        .accept_committed_batch(batch)
        .expect("accept changes");
    coordinator
        .deliver_through(&epoch, publisher.capture_cut().expect("capture cut"))
        .expect("deliver through convergence cut");
    coordinator
        .publish_ready(epoch)
        .expect("publish readiness only after convergence");
}

#[test]
fn catch_up_finite_cut_preserves_later_events_without_starvation() {
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let epoch = coordinator
        .begin(extension_id, Vec::new())
        .expect("begin epoch");
    let first = publisher.reserve_batch(1).expect("reserve pre-cut batch");
    coordinator
        .accept_committed_batch(
            publisher
                .publish(
                    first,
                    vec![CommittedFileEvent::Deleted {
                        path: RelativePath::new("before-cut.rs"),
                    }],
                )
                .expect("publish pre-cut batch"),
        )
        .expect("accept pre-cut batch");
    let cut = publisher.capture_cut().expect("capture finite cut");
    let later = publisher.reserve_batch(1).expect("reserve later batch");
    coordinator
        .accept_committed_batch(
            publisher
                .publish(
                    later,
                    vec![CommittedFileEvent::Deleted {
                        path: RelativePath::new("after-cut.rs"),
                    }],
                )
                .expect("publish later batch"),
        )
        .expect("accept later batch");
    coordinator
        .deliver_through(&epoch, cut)
        .expect("finite cut completes despite later writes");
    coordinator
        .publish_ready(epoch)
        .expect("handoff to live delivery");
}

#[test]
fn catch_up_fence_rejects_missing_support_wrong_epoch_and_wrong_count() {
    let (_publisher, coordinator, extension_id) = catch_up_fixture();
    let epoch = coordinator
        .begin(extension_id.clone(), Vec::new())
        .expect("begin epoch");
    assert!(matches!(
        coordinator.publish_ready(epoch),
        Err(CatchUpError::FenceUnsupported { extension_id: id }) if id == extension_id
    ));
}

#[test]
fn catch_up_cancellation_and_delivery_errors_are_isolated() {
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let epoch = coordinator
        .begin(extension_id.clone(), Vec::new())
        .expect("begin epoch");
    coordinator.cancel(epoch).expect("cancel active epoch");
    assert!(matches!(
        coordinator.begin(extension_id.clone(), Vec::new()),
        Err(CatchUpError::ClosedEpoch { extension_id: id }) if id == extension_id
    ));
    coordinator.close();
    assert!(matches!(
        coordinator.begin(extension_id.clone(), Vec::new()),
        Err(CatchUpError::BarrierClosed { extension_id: id }) if id == extension_id
    ));
    publisher.close();
    assert_eq!(publisher.capture_cut(), Err(PublicationError::Closed));

    // A malformed batch is a protocol failure, not a silent drop.  It must not
    // contaminate an independently active extension epoch.
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let healthy = coordinator
        .begin(extension_id.clone(), Vec::new())
        .expect("begin independent healthy epoch");
    let malformed = CommittedEventBatch {
        sequence: 0,
        events: vec![CommittedFileEvent::Deleted {
            path: RelativePath::new("invalid-sequence.rs"),
        }],
    };
    assert_eq!(
        coordinator.accept_committed_batch(malformed),
        Err(CatchUpError::InvalidSequence),
        "out-of-order publication must retain its exact typed failure"
    );
    coordinator
        .deliver_through(
            &healthy,
            publisher.capture_cut().expect("capture healthy cut"),
        )
        .expect("one extension's invalid batch must not poison another active epoch");
}

#[test]
fn ast_and_lsp_readiness_are_independent() {
    let ast = ExtensionId::new("ast");
    let lsp = ExtensionId::new("lsp");
    let policy = ExtensionDependencyPolicy::new(
        HashMap::from([
            (ast.clone(), CapabilityDependency::Ast),
            (lsp.clone(), CapabilityDependency::Lsp),
        ]),
        HashMap::new(),
    );
    assert_eq!(policy.dependency_for(&ast), Some(CapabilityDependency::Ast));
    assert_eq!(policy.dependency_for(&lsp), Some(CapabilityDependency::Lsp));
    assert_ne!(policy.dependency_for(&ast), policy.dependency_for(&lsp));
    let publisher = Arc::new(CommittedEventPublisher::new(9));
    let coordinator = ExtensionCatchUpCoordinator::new(publisher, Arc::new(policy));
    let ast_epoch = coordinator
        .begin(ast, Vec::new())
        .expect("AST begins independently");
    let _lsp_epoch = coordinator
        .begin(lsp, Vec::new())
        .expect("LSP remains independently preparing");
    coordinator
        .publish_ready(ast_epoch)
        .expect("AST can become ready without publishing LSP readiness");
}

#[test]
fn failed_extension_degrades_only_its_capability() {
    let readiness = ReadinessController::new(9);
    let ast = ExtensionId::new("ast");
    readiness
        .register_extension(ast.clone(), CapabilityDependency::Ast)
        .expect("register AST dependency");
    readiness
        .transition_capability(CapabilityDependency::Workspace, CapabilityReadiness::Ready)
        .expect("workspace ready");
    readiness
        .transition_extension(
            &ast,
            CapabilityReadiness::Degraded {
                failure_class:
                    core_engine::adapters::daemon::StartupFailureClass::ExtensionProtocol,
            },
        )
        .expect("AST degrades independently");
    let snapshot = readiness.snapshot();
    assert_eq!(snapshot.workspace, CapabilityReadiness::Ready);
    assert!(matches!(snapshot.ast, CapabilityReadiness::Degraded { .. }));
    assert!(matches!(
        snapshot.lsp,
        CapabilityReadiness::Preparing { .. }
    ));

    let lsp = ExtensionId::new("lsp");
    let policy = ExtensionDependencyPolicy::new(
        HashMap::from([
            (ast.clone(), CapabilityDependency::Ast),
            (lsp.clone(), CapabilityDependency::Lsp),
        ]),
        HashMap::new(),
    );
    let coordinator = ExtensionCatchUpCoordinator::new(
        Arc::new(CommittedEventPublisher::new(9)),
        Arc::new(policy),
    );
    let failed_epoch = coordinator.begin(ast, Vec::new()).expect("begin AST epoch");
    let _healthy_epoch = coordinator.begin(lsp, Vec::new()).expect("begin LSP epoch");
    coordinator
        .cancel(failed_epoch)
        .expect("isolating AST failure leaves LSP epoch active");
}

#[test]
fn catch_up_callbacks_run_without_authoritative_locks() {
    let (publisher, coordinator, extension_id) = catch_up_fixture();
    let epoch = coordinator
        .begin(extension_id, Vec::new())
        .expect("begin epoch");
    let authoritative_lock = Arc::new(Mutex::new(()));
    let guard = authoritative_lock.lock().expect("hold authoritative lock");
    drop(guard);
    let lock_for_callback = Arc::clone(&authoritative_lock);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let available = lock_for_callback.try_lock().is_ok();
        tx.send(available).expect("report callback lock state");
    })
    .join()
    .expect("callback probe joins");
    assert!(
        rx.recv().expect("receive callback observation"),
        "delivery callbacks must run only after authoritative guards are released"
    );
    coordinator
        .deliver_through(&epoch, publisher.capture_cut().expect("capture cut"))
        .expect("deliver without authoritative locks");
}

#[test]
fn unknown_tools_do_not_wait_for_workspace_preparation() {
    use core_engine::adapters::mcp::extension_merged_registry::ReadinessAwareRegistry;
    use core_engine::adapters::mcp::registry::ToolRegistry;
    use core_engine::adapters::mcp::types::ToolError;
    let workspace = tempdir().unwrap();
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());
    let mut registry = ReadinessAwareRegistry::new(context);
    // The engine has not been started: any attempted readiness wait would time out.
    let started = std::time::Instant::now();
    for name in ["does_not_exist", "tower_ast__tower_catch_up_fence"] {
        assert!(matches!(
            registry.call(
                name,
                serde_json::json!({}),
                &core_engine::domain::NeverCancelled
            ),
            Err(ToolError::NotFound(_))
        ));
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        registry.call(
            "tower_read_file",
            serde_json::json!({}),
            &core_engine::domain::NeverCancelled
        ),
        Err(ToolError::InvalidArgs(_))
    ));
}

#[test]
fn readiness_aware_registry_unlimited_call_wakes_only_when_stopping() {
    use core_engine::adapters::mcp::extension_merged_registry::ReadinessAwareRegistry;
    use core_engine::adapters::mcp::registry::ToolRegistry;
    use core_engine::adapters::mcp::types::ToolError;

    let workspace = tempdir().expect("create workspace");
    let (_owner, context) = early_context_fixture(workspace.path(), TowerConfig::default());
    let (result_tx, result_rx) = mpsc::channel();
    let waiting_context = Arc::clone(&context);
    let caller = std::thread::spawn(move || {
        let mut registry = ReadinessAwareRegistry::new(waiting_context);
        result_tx
            .send(registry.call(
                "tower_read_file",
                serde_json::json!({"path":"pending.rs"}),
                &core_engine::domain::NeverCancelled,
            ))
            .expect("report registry result");
    });

    assert!(
        result_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "a public native call must not invent a preparation deadline"
    );
    context
        .readiness()
        .transition_daemon(DaemonLifecycle::Stopping)
        .expect("stopping wakes registry dispatch");
    match result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("registry wake result")
        .expect_err("stopping rejects the pending public call")
    {
        ToolError::Application {
            code,
            message,
            data,
        } => {
            assert_eq!(code, -32_006);
            assert_eq!(message, "daemon is stopping");
            assert_eq!(data, Some(serde_json::json!({"dependency":"workspace"})));
        }
        other => panic!("expected stopping application error, got {other:?}"),
    }
    caller.join().expect("registry caller joins after stopping");
}
