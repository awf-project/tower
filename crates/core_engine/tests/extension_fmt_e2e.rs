//! End-to-end integration tests for the `fmt` native extension.
//!
//! # What this verifies
//!
//! - **EV1**: `event/fileChanged` delivers → `requestFormat` called with that path.
//! - **EV2**: A `.tmp_write` path is NOT forwarded to `requestFormat`.
//! - **T1**: `format` tool with `{"path":"x"}` → a one-file request count.
//! - **T2**: `format` tool with `{}` → lists files and enqueues each, returns
//!   aggregated counts.
//! - **Stress**: Concurrent-spawn stress (≥ 20 iterations) to prove no
//!   hazard-#2 deadlock.
//!
//! # Host capability doubles
//!
//! A `RecordingFormatQueue` is injected via `HostDeps` in the
//! `SidecarHostAdapter`-driven tests. It records every enqueued path and always
//! returns `Accepted`.
//!
//! The raw-protocol (JSON-RPC level) tests drive the binary directly so they
//! can control both the inbound frames and the host-call responses.

#![allow(clippy::pedantic)]

use std::fs;
use std::io::{BufRead, BufReader, Read as _, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost;
use core_engine::adapters::extension::{HostDeps, SidecarHostAdapter};
use core_engine::adapters::formatter::{EnqueueResult, FormatQueuePort, NoOpFormatQueue};
use core_engine::adapters::mcp::diagnostics::NoOpDiagnosticsReader;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::lsp_tools::SubscriptionRegistry;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::rmcp_server::TowerMcpHandler;
use core_engine::adapters::{InMemoryAstIndex, InMemoryFs, InMemoryStorage};
use core_engine::domain::extension_host::ExtensionRegistry;
use core_engine::ports::FileSystemPort;
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{Event, ExtensionManifest};
use rmcp::model::CallToolRequestParams;

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

#[path = "support/mcp_server.rs"]
mod mcp_server;
use mcp_server::start_server;
#[path = "support/dependency_search.rs"]
mod dependency_search;
use dependency_search::dependency_search_state_with_storage;

// ── Recording test double ─────────────────────────────────────────────────────

/// A `FormatQueuePort` that records every enqueued path and always returns
/// `Accepted`. Used to assert that the `fmt` extension forwards paths correctly.
#[derive(Clone, Default)]
struct RecordingFormatQueue {
    paths: Arc<Mutex<Vec<String>>>,
}

impl RecordingFormatQueue {
    fn new() -> Self {
        Self::default()
    }

    fn recorded(&self) -> Vec<String> {
        self.paths.lock().expect("lock").clone()
    }
}

impl FormatQueuePort for RecordingFormatQueue {
    fn enqueue(&self, path: String) -> EnqueueResult {
        self.paths.lock().expect("lock").push(path);
        EnqueueResult::Accepted
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent() // crates/
        .unwrap()
        .parent() // tower/
        .unwrap()
        .to_path_buf()
}

fn fmt_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("fmt_extension")
        .to_str()
        .unwrap()
        .to_owned()
}

fn fmt_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "fmt".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection {
            subscribe: vec!["event/fileChanged".to_owned()],
        },
        capabilities: CapabilitiesSection {
            required: vec![
                "request_format".to_owned(),
                "list_files".to_owned(),
                "log".to_owned(),
            ],
        },
    }
}

fn make_deps_with_recording(fq: RecordingFormatQueue) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(InMemoryFs::new())),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(fq),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
        agent_memory_storage: None,
        agent_memory_namespace: None,
    }
}

fn make_deps_with_fs_and_recording(fs: InMemoryFs, fq: RecordingFormatQueue) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(fq),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
        agent_memory_storage: None,
        agent_memory_namespace: None,
    }
}

fn make_deps() -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(InMemoryFs::new())),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
        agent_memory_storage: None,
        agent_memory_namespace: None,
    }
}

fn write_dependency_config(
    workspace: &std::path::Path,
    dependency: &std::path::Path,
    root_id: &str,
) {
    fs::create_dir_all(workspace.join(".tower")).unwrap();
    fs::write(
        workspace.join(".tower/config.toml"),
        format!(
            "[dependencies]\nmax_files = 10\nmax_file_bytes = 1024\nmax_total_bytes = 4096\nignore = []\n\n[[dependencies.roots]]\nid = {:?}\npath = {:?}\n",
            root_id, dependency
        ),
    )
    .unwrap();
}

fn dependency_search_state(workspace_root: &std::path::Path) -> Arc<RwLock<EngineState>> {
    dependency_search_state_with_storage(workspace_root, Box::new(InMemoryStorage::new()))
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

fn first_text(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .first()
        .and_then(|content| match &content.raw {
            rmcp::model::RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

// ── AC1/U1: spawn and initialize ─────────────────────────────────────────────

/// `fmt_extension` spawns, completes the initialize handshake, and declares
/// exactly one tool (`format`).
#[test]
fn fmt_process_spawns_and_declares_format_tool() {
    let bin = fmt_extension_bin();
    let manifest = fmt_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("fmt_extension must spawn");

    let m = adapter.manifest();
    let tool_names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tool_names.len(),
        1,
        "must declare exactly 1 tool; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"format"),
        "must declare 'format'; got: {tool_names:?}"
    );

    adapter.shutdown();
}

/// `fmt_extension` subscribes to `event/fileChanged`.
#[test]
fn fmt_process_subscribes_to_file_changed() {
    let bin = fmt_extension_bin();
    let manifest = fmt_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("fmt_extension must spawn");

    let m = adapter.manifest();
    assert!(
        m.events.subscribe.contains(&"event/fileChanged".to_owned()),
        "must subscribe to event/fileChanged; got: {:?}",
        m.events.subscribe
    );

    adapter.shutdown();
}

// ── EV1: FileChanged forwards path to requestFormat ──────────────────────────

/// EV1: A `fileChanged` event triggers `requestFormat` with the file path.
/// Verified via the `RecordingFormatQueue` double injected into `HostDeps`.
#[test]
fn fmt_process_file_changed_event_enqueues_path() {
    let bin = fmt_extension_bin();
    let manifest = fmt_manifest(&bin);
    let recording = RecordingFormatQueue::new();
    let deps = make_deps_with_recording(recording.clone());

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("fmt_extension must spawn");

    let event = Event::FileChanged {
        file_id: 1,
        path: "src/main.rs".to_owned(),
    };
    adapter
        .deliver_event(event)
        .expect("fileChanged must succeed");

    // Give the host adapter time to process the host-call before we check.
    adapter.shutdown();

    let recorded = recording.recorded();
    assert_eq!(
        recorded,
        vec!["src/main.rs"],
        "requestFormat must be called with the changed path; got: {recorded:?}"
    );
}

#[test]
fn fmt_public_event_uses_unlimited_host_budget() {
    let recording = RecordingFormatQueue::new();
    let mut adapter = SidecarHostAdapter::spawn(
        fmt_manifest(&fmt_extension_bin()),
        make_deps_with_recording(recording.clone()),
        None,
        None,
    )
    .expect("fmt must initialize without a host request deadline");

    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/main.rs".to_owned(),
        })
        .expect("fmt fileChanged must complete without a host request deadline");
    adapter.shutdown();
    assert_eq!(recording.recorded(), vec!["src/main.rs"]);
}

// ── EV2: .tmp_write paths are filtered ───────────────────────────────────────

/// EV2: A `.tmp_write` path is NOT forwarded to `requestFormat`.
#[test]
fn fmt_process_tmp_write_path_not_forwarded() {
    let bin = fmt_extension_bin();
    let manifest = fmt_manifest(&bin);
    let recording = RecordingFormatQueue::new();
    let deps = make_deps_with_recording(recording.clone());

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("fmt_extension must spawn");

    let event = Event::FileChanged {
        file_id: 2,
        path: "src/main.rs.tmp_write".to_owned(),
    };
    adapter
        .deliver_event(event)
        .expect("fileChanged must succeed (even for filtered paths)");

    adapter.shutdown();

    let recorded = recording.recorded();
    assert!(
        recorded.is_empty(),
        ".tmp_write path must NOT be forwarded to requestFormat; got: {recorded:?}"
    );
}

// ── T1: format tool with single path ─────────────────────────────────────────

/// T1: `format` tool with `{"path":"src/lib.rs"}` reports one request and
/// enqueues exactly that path.
#[test]
fn fmt_tool_with_path_enqueues_one_file() {
    let bin = fmt_extension_bin();
    let manifest = fmt_manifest(&bin);
    let recording = RecordingFormatQueue::new();
    let deps = make_deps_with_recording(recording.clone());

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("fmt_extension must spawn");

    let result = adapter
        .call_tool(
            "format",
            serde_json::json!({ "path": "src/lib.rs" }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("format tool must not error");

    adapter.shutdown();

    assert_eq!(
        result["requested"].as_u64(),
        Some(1),
        "requested must be 1; got: {result}"
    );
    assert_eq!(
        result["accepted"].as_u64(),
        Some(1),
        "accepted must be 1; got: {result}"
    );
    assert_eq!(
        result["dropped"].as_u64(),
        Some(0),
        "dropped must be 0; got: {result}"
    );

    let recorded = recording.recorded();
    assert_eq!(
        recorded,
        vec!["src/lib.rs"],
        "requestFormat must be called with exactly that path; got: {recorded:?}"
    );
}

// ── T2: format tool with no path (format-all) ────────────────────────────────

/// T2: `format` tool with `{}` → `workspace/listFiles` then enqueues each file.
/// Verified using a populated `InMemoryFs` so listFiles returns known paths.
#[test]
fn fmt_tool_without_path_enqueues_all_files() {
    let bin = fmt_extension_bin();
    let manifest = fmt_manifest(&bin);

    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/main.rs"),
        b"fn main() {}".to_vec(),
    )
    .expect("write");
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        b"pub fn foo() {}".to_vec(),
    )
    .expect("write");

    let recording = RecordingFormatQueue::new();
    let deps = make_deps_with_fs_and_recording(fs, recording.clone());

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("fmt_extension must spawn");

    let result = adapter
        .call_tool(
            "format",
            serde_json::json!({}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("format tool must not error");

    adapter.shutdown();

    let requested = result["requested"].as_u64().expect("requested is u64");
    let accepted = result["accepted"].as_u64().expect("accepted is u64");
    let dropped = result["dropped"].as_u64().expect("dropped is u64");

    assert_eq!(requested, 2, "must request 2 files; got: {result}");
    assert_eq!(accepted, 2, "must accept 2 files; got: {result}");
    assert_eq!(dropped, 0, "must drop 0; got: {result}");
    assert_eq!(
        accepted + dropped,
        requested,
        "accepted + dropped must equal requested"
    );

    let mut recorded = recording.recorded();
    recorded.sort();
    assert_eq!(
        recorded,
        vec!["src/lib.rs", "src/main.rs"],
        "requestFormat must be called for each file; got: {recorded:?}"
    );
}

#[test]
fn formatting_rejects_dependency_paths_before_enqueue() {
    block_on(async {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let dependency = fixture
            .path()
            .join("PRIVATE_DEPENDENCY_ABSOLUTE_PATH_SENTINEL");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(dependency.join("src")).unwrap();
        fs::write(workspace.join("workspace.rs"), "workspace_anchor\n").unwrap();
        let dependency_file = dependency.join("src/shared.rs");
        fs::write(&dependency_file, "PRIVATE_DEPENDENCY_CONTENT_SENTINEL\n").unwrap();
        write_dependency_config(&workspace, &dependency, "library");

        let mut stderr = gag::BufferRedirect::stderr().expect("stderr capture must install");
        let state = dependency_search_state(&workspace);
        let recording = RecordingFormatQueue::new();
        let adapter = SidecarHostAdapter::spawn(
            fmt_manifest(&fmt_extension_bin()),
            make_deps_with_recording(recording.clone()),
            TEST_TIMEOUT,
            None,
        )
        .expect("real fmt sidecar must initialize");
        let extensions = Arc::new(RwLock::new(ExtensionRegistry::new()));
        extensions
            .write()
            .unwrap()
            .register(adapter)
            .expect("fmt sidecar must register");
        let merged = ExtensionMergedRegistry::new(state, extensions);
        let handler = TowerMcpHandler::new(
            merged,
            Arc::new(NoOpDiagnosticsReader),
            Arc::new(Mutex::new(SubscriptionRegistry::new())),
            vec![],
        );
        let client = start_server(handler).await;

        let search = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("tower_find_file").with_arguments(
                    serde_json::json!({
                        "query": "shared.rs",
                        "scope": {"workspace": true, "dependencies": true}
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            )
            .await
            .expect("public dependency search must succeed");
        let payload: serde_json::Value =
            serde_json::from_str(first_text(&search)).expect("search payload must be JSON");
        let locator = payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|result| result["origin"] == "dependency")
            .and_then(|result| result["locator"].as_str())
            .expect("public search must return a dependency locator")
            .to_owned();
        assert_eq!(locator, "dependency://library/src/shared.rs");
        let bytes_before = fs::read(&dependency_file).unwrap();

        for (path, expected_message) in [
            (locator, "dependency locator is not a workspace path"),
            (
                "dependency://library/src/shared.rs.tmp_write".to_owned(),
                "dependency locator is not a workspace path",
            ),
            (
                "dependency://library/.tmp_write/..".to_owned(),
                "invalid dependency locator",
            ),
        ] {
            let result = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new("tower_fmt_format").with_arguments(
                        serde_json::json!({"path": path})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;
            let error = match result.expect_err("formatter locator must be invalid params") {
                rmcp::ServiceError::McpError(error) => {
                    assert_eq!(error.code.0, -32602);
                    error.message.to_string()
                }
                other => panic!("expected MCP invalid-params error, got {other:?}"),
            };
            assert_eq!(error, expected_message);
            assert!(!error.contains("PRIVATE_DEPENDENCY_ABSOLUTE_PATH_SENTINEL"));
            assert!(!error.contains("PRIVATE_DEPENDENCY_CONTENT_SENTINEL"));
            assert!(recording.recorded().is_empty());
            assert_eq!(fs::read(&dependency_file).unwrap(), bytes_before);
        }
        let mut captured_logs = String::new();
        stderr
            .read_to_string(&mut captured_logs)
            .expect("captured logs must be readable");
        assert!(!captured_logs.contains("PRIVATE_DEPENDENCY_ABSOLUTE_PATH_SENTINEL"));
        assert!(!captured_logs.contains("PRIVATE_DEPENDENCY_CONTENT_SENTINEL"));
        client.cancel().await.unwrap();
    });
}

// ── Concurrency stress: no deadlock ──────────────────────────────────────────

/// Spawn ≥ 20 instances concurrently and drive each through
/// initialize → invokeTool(format with path) → deliverEvent(fileChanged) → shutdown.
/// All must complete without deadlock, panic, or error.
///
/// This validates that the deferred-queue hazard mitigation (HAZARD #2) is
/// correct under concurrent load.
#[test]
fn fmt_process_concurrent_spawn_stress_20() {
    use extension_protocol::PROTOCOL_VERSION;

    const N: usize = 20;

    let bin = fmt_extension_bin();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let bin = bin.clone();
            std::thread::spawn(move || {
                let mut child = Command::new(&bin)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap_or_else(|e| panic!("#{i}: spawn failed: {e}"));

                let mut stdin = child.stdin.take().unwrap();
                let stdout = child.stdout.take().unwrap();
                let mut reader = BufReader::new(stdout);

                let write_frame = |stdin: &mut std::process::ChildStdin,
                                   frame: serde_json::Value| {
                    let s = serde_json::to_string(&frame).unwrap();
                    stdin.write_all(s.as_bytes()).unwrap();
                    stdin.write_all(b"\n").unwrap();
                    stdin.flush().unwrap();
                };

                let read_frame =
                    |r: &mut BufReader<std::process::ChildStdout>| -> serde_json::Value {
                        let mut line = String::new();
                        r.read_line(&mut line).unwrap();
                        serde_json::from_str(line.trim()).expect("valid JSON")
                    };

                // ── initialize ──
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 0,
                        "method": "initialize",
                        "params": {
                            "protocol_version": PROTOCOL_VERSION,
                            "client_info": "stress-test/0.1.0"
                        }
                    }),
                );
                let init = read_frame(&mut reader);
                assert!(
                    init.get("result").is_some(),
                    "#{i}: initialize must succeed; got: {init}"
                );

                // ── invokeTool(format, {path}) ──
                // The extension will make a workspace/requestFormat host-call.
                // We respond with "accepted".
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "method": "invokeTool",
                        "params": {
                            "name": "format",
                            "params": { "path": format!("src/file_{i}.rs") }
                        }
                    }),
                );

                // Drain host-calls and collect the invokeTool response.
                let mut invoke_resp: Option<serde_json::Value> = None;
                for _ in 0..10 {
                    let frame = read_frame(&mut reader);
                    if frame.get("method").is_some() {
                        // Host-call from extension — respond with "accepted".
                        let call_id = frame["id"].clone();
                        write_frame(
                            &mut stdin,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": call_id,
                                "result": "accepted"
                            }),
                        );
                    } else {
                        invoke_resp = Some(frame);
                        break;
                    }
                }
                let resp = invoke_resp.unwrap_or_else(|| panic!("#{i}: no invokeTool response"));
                assert!(
                    resp.get("result").is_some(),
                    "#{i}: invokeTool must succeed; got: {resp}"
                );

                // ── deliverEvent(fileChanged) ──
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 2,
                        "method": "deliverEvent",
                        "params": {
                            "type": "DeliverEvent",
                            "data": {
                                "type": "FileChanged",
                                "file_id": i as u64,
                                "path": format!("src/changed_{i}.rs")
                            }
                        }
                    }),
                );

                // Extension will make a requestFormat host-call then send Ack.
                let mut event_ack: Option<serde_json::Value> = None;
                for _ in 0..10 {
                    let frame = read_frame(&mut reader);
                    if frame.get("method").is_some() {
                        let call_id = frame["id"].clone();
                        write_frame(
                            &mut stdin,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": call_id,
                                "result": "accepted"
                            }),
                        );
                    } else {
                        event_ack = Some(frame);
                        break;
                    }
                }
                let ack = event_ack.unwrap_or_else(|| panic!("#{i}: no deliverEvent ack"));
                assert!(
                    ack.get("result").is_some(),
                    "#{i}: deliverEvent must ack; got: {ack}"
                );

                // ── shutdown ──
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 3,
                        "method": "shutdown"
                    }),
                );
                let shutdown_ack = read_frame(&mut reader);
                assert!(
                    shutdown_ack.get("result").is_some(),
                    "#{i}: shutdown must ack; got: {shutdown_ack}"
                );

                child
                    .wait()
                    .unwrap_or_else(|e| panic!("#{i}: wait failed: {e}"));
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join().unwrap_or_else(|_| panic!("thread #{i} panicked"));
    }
}
