//! Engine-builder smoke test (Task 6).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::lsp::validate_lsp_commands;
use core_engine::adapters::config::{LspConfig, LspServerConfig, TowerConfig};
use core_engine::adapters::daemon::engine::build_engine;
use core_engine::adapters::daemon::socket::socket_path;
use core_engine::adapters::daemon::wire::{ClientRole, Handshake};
use core_engine::adapters::fs::workspace_scan;
use core_engine::adapters::watcher::WatcherError;
use core_engine::adapters::{InMemoryStorage, NotifyWatcherAdapter, SledStorageAdapter};
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::token::Token;
use core_engine::domain::{ProjectWorkspace, RelativePath};
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

#[cfg(unix)]
#[test]
fn repaired_additions_replay_after_commit_and_watcher_catch_up() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    let workspace = tempdir().expect("create workspace");
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
    let replay_marker = workspace.path().join("replay.json");
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
        }),
        "daemon must hand the explicit empty LSP configuration to its lsp extension"
    );
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
