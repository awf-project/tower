// Feature: F009
// Feature: B001
//! End-to-end integration tests for the `ast` native extension (spec 26).
//!
//! # What this verifies
//!
//! - **AC1/U1**: `ast_extension` declares all five tools (`get_outline`,
//!   `find_symbols`, `search_symbols`, `reindex`, `read_symbol`) over the
//!   extension protocol.
//! - **AC2/EV1**: A `fileIndexed` event triggers parse → index update; a
//!   subsequent `find_symbols` reflects the new symbols.
//! - **AC3**: Multi-language parity (Rust, Go, PHP) matches the prior WASM output.
//! - **AC4/U4**: The `hello_extension` lazy greet tool.
//! - **AC5**: The test suite drives the native binary via plain `cargo build`
//!   (no WASI SDK, no stale-artifact hazard).
//!
//! # Test binary location
//!
//! `ast_extension` and `hello_extension` are in `default-members`, so
//! `cargo test --workspace` compiles them first. Binaries are located in
//! `target/debug/`.
//!
//! # Host capability doubles
//!
//! All tests use `InMemoryFs` and `InMemoryAstIndex` — no real disk I/O.

#[path = "support/process_binary.rs"]
mod process_binary;

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{Event, ExtensionManifest};

use core_engine::adapters::extension::host_deps::ApplyEditsHostPort;
use core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost;
use core_engine::adapters::extension::{HostDeps, SidecarHostAdapter};
use core_engine::adapters::formatter::NoOpFormatQueue;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::{InMemoryAstIndex, InMemoryFs, InMemoryStorage};
use core_engine::domain::extension_host::{ExtensionInstance, ExtensionRegistry};
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::mutation::compute_content_version;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::domain::{DomainError, RelativePath};
use core_engine::ports::FileSystemPort;
use core_engine::ports::inbound::{
    PerFileEditResult, WorkspaceApplyEditsError, WorkspaceApplyEditsErrorCode,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult,
};
use tempfile::tempdir;

/// Default timeout: long enough for cooperative extensions in CI.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

// ── Binary path helpers ───────────────────────────────────────────────────────

fn workspace_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR points at tower/crates/core_engine.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent() // crates/
        .unwrap()
        .parent() // tower/
        .unwrap()
        .to_path_buf()
}

fn ast_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("ast_extension")
        .to_str()
        .unwrap()
        .to_owned()
}

/// A direct protocol host used only where the test must observe every HostCall
/// emitted by the AST sidecar. `SidecarHostAdapter` intentionally consumes log
/// HostCalls, so it cannot prove the sidecar did not emit one.
struct RawAstChild {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<serde_json::Value>,
}

impl RawAstChild {
    fn spawn() -> Self {
        let mut child = Command::new(ast_extension_bin())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn AST extension");
        let stdin = child.stdin.take().expect("AST stdin");
        let stdout = child.stdout.take().expect("AST stdout");
        let (frame_tx, frames) = mpsc::channel();
        thread::spawn(move || collect_ast_frames(stdout, frame_tx));

        Self {
            child,
            stdin,
            frames,
        }
    }

    fn write_frame(&mut self, frame: serde_json::Value) {
        writeln!(self.stdin, "{frame}").expect("write AST protocol frame");
        self.stdin.flush().expect("flush AST protocol frame");
    }

    fn read_frame(&self) -> serde_json::Value {
        self.frames
            .recv_timeout(TEST_TIMEOUT)
            .expect("AST sidecar must emit a protocol frame")
    }

    fn assert_no_additional_host_call(&self) {
        assert!(
            self.frames
                .recv_timeout(Duration::from_millis(150))
                .is_err(),
            "planner emitted an unexpected HostCall after its terminal response"
        );
    }

    fn initialize(&mut self) -> serde_json::Value {
        self.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": extension_protocol::PROTOCOL_VERSION,
                "client_info": "safe-delete-log-test"
            }
        }));
        let warm_start = self.read_frame();
        assert_eq!(warm_start["method"], "index/get");
        self.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": warm_start["id"].clone(),
            "result": []
        }));
        let initialized = self.read_frame();
        assert_eq!(initialized["id"], 1);
        assert_eq!(initialized["result"]["type"], "Initialized");
        initialized
    }

    fn invoke_planner_with_content(
        &mut self,
        request_id: u64,
        content: &[u8],
        symbol_name: &str,
    ) -> serde_json::Value {
        self.invoke_planner(request_id, symbol_name);
        let read_file = self.read_frame();
        assert_eq!(read_file["method"], "workspace/readFile");
        self.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": read_file["id"].clone(),
            "result": std::str::from_utf8(content).expect("test content is UTF-8")
        }));
        let response = self.read_frame();
        assert_eq!(response["id"], request_id, "planner must not log normally");
        assert_eq!(response["result"]["type"], "ToolResult");
        self.assert_no_additional_host_call();
        response["result"]["data"].clone()
    }

    fn invoke_planner(&mut self, request_id: u64, symbol_name: &str) {
        self.write_frame(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "invokeTool",
            "params": {
                "name": "plan_safe_delete_declaration",
                "params": {
                    "path": "src/lib.rs",
                    "symbol_name": symbol_name,
                    "kind": "function"
                }
            }
        }));
    }
}

fn collect_ast_frames(stdout: ChildStdout, frame_tx: mpsc::Sender<serde_json::Value>) {
    let mut stdout = BufReader::new(stdout);
    loop {
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let frame = serde_json::from_str(line.trim()).expect("AST frame is valid JSON");
                if frame_tx.send(frame).is_err() {
                    break;
                }
            }
        }
    }
}

impl Drop for RawAstChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn hello_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("hello_extension")
        .to_str()
        .unwrap()
        .to_owned()
}

struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn production_tower_bin() -> PathBuf {
    process_binary::selected_binary("TOWER_PRODUCTION_BIN")
}

fn run_daemon_to_idle(binary: &Path, workspace: &Path) {
    let output = Command::new(binary)
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .output()
        .expect("run baseline daemon");
    assert!(
        output.status.success(),
        "baseline daemon failed: {output:?}"
    );
    assert!(output.stdout.is_empty(), "daemon polluted stdout");
}

fn install_ast_extension(workspace: &Path) -> PathBuf {
    let extension = workspace.join("extensions/ast");
    fs::create_dir_all(&extension).unwrap();
    fs::copy(
        workspace_root().join("extensions/ast/extension.toml"),
        extension.join("extension.toml"),
    )
    .unwrap();
    let binary = extension.join("ast_extension");
    fs::copy(ast_extension_bin(), &binary).unwrap();
    binary
}

fn spawn_ast_daemon(binary: &Path, workspace: &Path) -> DaemonChild {
    DaemonChild(
        Command::new(binary)
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(workspace)
            .arg("--extensions-dir")
            .arg(workspace.join("extensions"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn AST daemon"),
    )
}

#[cfg(unix)]
fn ast_symbol_search(workspace: &Path, symbol: &str) -> serde_json::Value {
    use core_engine::adapters::daemon::socket::socket_path;
    use core_engine::adapters::daemon::wire::{ClientRole, Handshake};
    use std::os::unix::net::UnixStream;

    let socket = socket_path(workspace);
    let mut stream = (0..400)
        .find_map(|_| match UnixStream::connect(&socket) {
            Ok(stream) => Some(stream),
            Err(_) => {
                thread::sleep(Duration::from_millis(25));
                None
            }
        })
        .expect("daemon socket becomes available");
    stream
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    stream.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"reconciliation-test\",\"version\":\"1\"}}}\n").unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let init: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(init.get("result").is_some(), "initialize failed: {init}");
    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    // MCP initialization is independent of discovery. Wait for the public
    // exhaustive declaration before asserting semantic freshness.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        writeln!(
            stream,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}})
        )
        .unwrap();
        let inventory = loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == 3 {
                break frame;
            }
        };
        if inventory["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "tower_ast_search_symbols")
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{inventory}");
        thread::sleep(Duration::from_millis(10));
    }
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "tower_ast_search_symbols",
            "arguments": { "name": symbol, "kind": "function" }
        }
    });
    writeln!(stream, "{request}").unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
#[cfg(unix)]
fn repaired_files_reach_ast_once_after_parallel_startup() {
    let workspace = tempdir().unwrap();
    fs::create_dir_all(workspace.path().join(".tower")).unwrap();
    fs::write(
        workspace.path().join(".tower/config.toml"),
        "[daemon]\nidle_timeout_secs = 1\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join("baseline.rs"),
        "pub fn baseline() {}\n",
    )
    .unwrap();
    let tower = production_tower_bin();
    run_daemon_to_idle(&tower, workspace.path());

    fs::write(
        workspace.path().join("repaired.rs"),
        "pub fn repaired_parallel_startup_symbol() {}\n",
    )
    .unwrap();
    install_ast_extension(workspace.path());

    for _ in 0..2 {
        let mut daemon = spawn_ast_daemon(&tower, workspace.path());
        let response = ast_symbol_search(workspace.path(), "repaired_parallel_startup_symbol");
        let rendered = response.to_string();
        assert_eq!(
            rendered
                .matches("\\\"name\\\":\\\"repaired_parallel_startup_symbol\\\"")
                .count(),
            1,
            "repaired symbol must be visible exactly once after restart: {response}"
        );
        daemon.0.kill().unwrap();
        daemon.0.wait().unwrap();
    }
}

// ── HostDeps builders ─────────────────────────────────────────────────────────

fn make_deps(fs: InMemoryFs) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    }
}

fn empty_engine_state() -> Arc<std::sync::RwLock<EngineState>> {
    Arc::new(std::sync::RwLock::new(EngineState::new(
        ProjectWorkspace::new(),
        InvertedIndex::new(),
        Box::new(InMemoryStorage::new()),
        Box::new(InMemoryFs::new()),
    )))
}

fn make_deps_with_apply_edits(fs: Arc<Mutex<InMemoryFs>>) -> HostDeps {
    HostDeps {
        fs: fs.clone(),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(InMemoryApplyEditsHost { fs }),
        push_tx: None,
    }
}

struct InMemoryApplyEditsHost {
    fs: Arc<Mutex<InMemoryFs>>,
}

struct CountingApplyEditsHost {
    calls: Arc<AtomicUsize>,
}

impl ApplyEditsHostPort for CountingApplyEditsHost {
    fn apply_batch_edits(
        &self,
        _request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(DomainError::UnsupportedOperation(
            "safe-delete declaration planning must not apply edits".to_owned(),
        ))
    }
}

fn safe_delete_planner(
    content: &[u8],
) -> (
    Box<dyn ExtensionInstance>,
    Arc<Mutex<InMemoryFs>>,
    Arc<AtomicUsize>,
) {
    let mut initial_fs = InMemoryFs::new();
    initial_fs
        .write(RelativePath::new("src/lib.rs"), content.to_vec())
        .expect("write planner fixture");
    let fs = Arc::new(Mutex::new(initial_fs));
    let apply_calls = Arc::new(AtomicUsize::new(0));
    let deps = HostDeps {
        fs: fs.clone(),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(CountingApplyEditsHost {
            calls: apply_calls.clone(),
        }),
        push_tx: None,
    };
    let mut adapter =
        SidecarHostAdapter::spawn(ast_manifest(&ast_extension_bin()), deps, TEST_TIMEOUT, None)
            .expect("ast must spawn");
    adapter
        .deliver_event(Event::FileIndexed {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileIndexed must seed AST index");
    (adapter, fs, apply_calls)
}

#[test]
fn catch_up_ast_fence_observes_updated_symbol_index() {
    let (mut adapter, fs, _) = safe_delete_planner(b"pub fn before_catch_up() {}\n");
    fs.lock()
        .expect("lock AST fixture filesystem")
        .write(
            RelativePath::new("src/lib.rs"),
            b"pub fn after_catch_up() {}\n".to_vec(),
        )
        .expect("commit changed source");
    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("deliver committed change");

    let fence = adapter
        .call_tool(
            "_tower_catch_up_fence",
            serde_json::json!({"epoch": 41, "delivered_events": 2}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("AST catch-up fence is declared and handled");
    assert_eq!(
        fence,
        serde_json::json!({"epoch": 41, "processed_events": 2, "ready": true})
    );

    let symbols = adapter
        .call_tool(
            "search_symbols",
            serde_json::json!({"query": "after_catch_up", "kind": "function"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("query AST index after ready fence");
    assert!(
        symbols.to_string().contains("after_catch_up"),
        "ready fence acknowledged before the changed symbol was searchable: {symbols}"
    );
    assert!(!symbols.to_string().contains("before_catch_up"));

    // Validate the fence at its actual protocol boundary as well: a sidecar
    // must advertise the private control, and an epoch/count that was never
    // delivered cannot turn the capability ready.
    let mut raw = RawAstChild::spawn();
    let initialized = raw.initialize();
    assert!(
        initialized["result"]["data"]["tools"]
            .as_array()
            .expect("AST initialized tools must be an array")
            .iter()
            .any(|tool| tool["name"] == "_tower_catch_up_fence"),
        "the AST sidecar must declare its private catch-up fence"
    );
    raw.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "invokeTool",
        "params": {
            "name": "_tower_catch_up_fence",
            "params": {"epoch": 42, "delivered_events": 1}
        }
    }));
    let rejected = raw.read_frame();
    assert_eq!(rejected["id"], 42);
    assert!(
        rejected.get("error").is_some()
            || rejected["result"]["data"]["ready"] == serde_json::json!(false),
        "AST must reject a fence whose delivered count is not its processed count: {rejected}"
    );
}

fn plan_safe_delete(
    adapter: &mut Box<dyn ExtensionInstance>,
    symbol_name: &str,
) -> serde_json::Value {
    adapter
        .call_tool(
            "plan_safe_delete_declaration",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": symbol_name,
                "kind": "function"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("safe-delete declaration planning must return a structured result")
}

impl ApplyEditsHostPort for InMemoryApplyEditsHost {
    fn apply_batch_edits(
        &self,
        request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        let dry_run = request.dry_run.unwrap_or(false);
        let mut files_changed = 0;
        let mut per_file = Vec::new();

        for span in request.edits {
            let mut fs = self
                .fs
                .lock()
                .map_err(|error| DomainError::IoError(format!("fs mutex poisoned: {error}")))?;
            let bytes = fs.read(&span.path).map_err(|error| {
                DomainError::IoError(format!("read {} failed: {error}", span.path.as_str()))
            })?;
            let actual_hash = compute_content_version(&bytes);
            if span.base_hash.as_deref() != Some(actual_hash.as_str()) {
                per_file.push(PerFileEditResult {
                    path: span.path,
                    applied: false,
                    edits_applied: 0,
                    edits_skipped: 1,
                    new_version: None,
                    preview: None,
                    error: Some(WorkspaceApplyEditsError {
                        code: WorkspaceApplyEditsErrorCode::Conflict,
                        message: "base_hash did not match file version".to_owned(),
                        path: None,
                    }),
                });
                continue;
            }

            let mut content = String::from_utf8(bytes).map_err(|error| {
                DomainError::InvalidRange(format!(
                    "{} is not UTF-8 text: {error}",
                    span.path.as_str()
                ))
            })?;
            if span.start_byte > span.end_byte || span.end_byte > content.len() {
                per_file.push(PerFileEditResult {
                    path: span.path,
                    applied: false,
                    edits_applied: 0,
                    edits_skipped: 1,
                    new_version: None,
                    preview: None,
                    error: Some(WorkspaceApplyEditsError {
                        code: WorkspaceApplyEditsErrorCode::InvalidRange,
                        message: "edit range is outside file bounds".to_owned(),
                        path: None,
                    }),
                });
                continue;
            }

            content.replace_range(span.start_byte..span.end_byte, &span.replacement);
            let new_version = compute_content_version(content.as_bytes());
            if dry_run {
                per_file.push(PerFileEditResult {
                    path: span.path,
                    applied: false,
                    edits_applied: 0,
                    edits_skipped: 0,
                    new_version: None,
                    preview: Some(content),
                    error: None,
                });
            } else {
                fs.write(span.path.clone(), content.into_bytes())
                    .map_err(|error| {
                        DomainError::IoError(format!(
                            "write {} failed: {error}",
                            span.path.as_str()
                        ))
                    })?;
                files_changed += 1;
                per_file.push(PerFileEditResult {
                    path: span.path,
                    applied: true,
                    edits_applied: 1,
                    edits_skipped: 0,
                    new_version: Some(new_version),
                    preview: None,
                    error: None,
                });
            }
        }

        Ok(WorkspaceApplyEditsResult {
            files_changed,
            per_file,
        })
    }
}

#[allow(dead_code)]
fn make_deps_with_index(fs: InMemoryFs, index: InMemoryAstIndex) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(index),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    }
}

// ── Manifest builders ─────────────────────────────────────────────────────────

fn ast_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "ast".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection {
            subscribe: vec![
                "event/fileIndexed".to_owned(),
                "event/fileChanged".to_owned(),
            ],
        },
        capabilities: CapabilitiesSection {
            required: vec![
                "read_file".to_owned(),
                "list_files".to_owned(),
                "index_get".to_owned(),
                "index_put".to_owned(),
                "request_apply_edits".to_owned(),
                "log".to_owned(),
            ],
        },
    }
}

fn hello_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "hello".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Lazy,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    }
}

// ── Rust source fixture ───────────────────────────────────────────────────────

const RUST_SOURCE: &[u8] = br#"
use std::fmt;

pub struct Counter {
    value: u32,
}

impl Counter {
    pub fn new() -> Self {
        Self { value: 0 }
    }

    pub fn increment(&mut self) {
        self.value += 1;
    }

    pub fn get(&self) -> u32 {
        self.value
    }
}

pub trait Resetable {
    fn reset(&mut self);
}

impl Resetable for Counter {
    fn reset(&mut self) {
        self.value = 0;
    }
}

pub fn make_counter() -> Counter {
    Counter::new()
}
"#;

const GO_SOURCE: &[u8] = br#"package main

import "fmt"

func HelloWorld(name string) string {
    return fmt.Sprintf("Hello, %s", name)
}

type MyStruct struct {
    value int
}

type MyInterface interface {
    Method() error
}
"#;

const PHP_SOURCE: &[u8] = br#"<?php

class MyClass {
    public function myMethod() {}
}

interface MyInterface {
    public function interfaceMethod();
}

function topLevelFn() {}
"#;

#[test]
fn safe_delete_declaration_declares_and_dispatches_the_exact_internal_tool() {
    let (mut adapter, _fs, apply_calls) = safe_delete_planner(b"pub fn target() {}\n");
    assert!(
        adapter
            .manifest()
            .tools
            .iter()
            .any(|tool| tool.name == "plan_safe_delete_declaration"),
        "the planner must declare its ownership for targeted orchestration"
    );

    let result = plan_safe_delete(&mut adapter, "target");
    assert_eq!(result["status"], "planned");
    assert_eq!(apply_calls.load(Ordering::SeqCst), 0);
    adapter.shutdown();
}

#[test]
fn safe_delete_declaration_unique_resolution_returns_the_exact_plan_payload() {
    let content = b"pub fn target() {}\n";
    let mut child = RawAstChild::spawn();
    child.initialize();

    let result = child.invoke_planner_with_content(2, content, "target");

    assert_eq!(
        result,
        serde_json::json!({
            "status": "planned",
            "plan": {
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "position": { "line": 0, "character": 7 },
                "deletion_span": { "start_byte": 0, "end_byte": 19 },
                "base_hash": compute_content_version(content)
            }
        })
    );
}

#[test]
fn safe_delete_declaration_uses_the_declaration_name_node_for_position() {
    let content = b"pub(in crate::target_scope) fn target() {}\n";
    let mut child = RawAstChild::spawn();
    child.initialize();

    let result = child.invoke_planner_with_content(2, content, "target");

    assert_eq!(result["status"], "planned");
    assert_eq!(
        result["plan"]["position"],
        serde_json::json!({ "line": 0, "character": 31 })
    );
}

#[test]
fn safe_delete_declaration_missing_target_returns_not_found_without_mutation() {
    let mut child = RawAstChild::spawn();
    child.initialize();

    let result = child.invoke_planner_with_content(2, b"pub fn present() {}\n", "missing");

    assert_eq!(result, serde_json::json!({ "status": "not_found" }));
}

#[test]
fn safe_delete_declaration_multiple_matches_return_ambiguous_symbol_without_mutation() {
    let content = b"pub fn duplicate() {}\nmod inner { pub fn duplicate() {} }\n";
    let mut child = RawAstChild::spawn();
    child.initialize();

    let result = child.invoke_planner_with_content(2, content, "duplicate");

    assert_eq!(result, serde_json::json!({ "status": "ambiguous_symbol" }));
}

#[test]
fn safe_delete_declaration_invalid_source_span_returns_invalid_declaration_without_mutation() {
    let mut child = RawAstChild::spawn();
    child.initialize();

    let result = child.invoke_planner_with_content(2, b"pub fn target() {", "target");

    assert_eq!(
        result,
        serde_json::json!({ "status": "invalid_declaration" })
    );

    // A real HostCall error can contain source, paths, hashes, and backend
    // details. The sidecar's public log payload must be the fixed safe string.
    const SECRET_SOURCE: &str = "source=top-secret-safe-delete-source";
    const SECRET_PATH: &str = "path=/private/safe-delete.rs";
    const SECRET_HASH: &str = "hash=deadbeef-safe-delete";
    const BACKEND_DETAIL: &str = "backend=fixture-read-failure";
    let mut faulting_child = RawAstChild::spawn();
    faulting_child.initialize();
    faulting_child.invoke_planner(2, "target");
    let read_file = faulting_child.read_frame();
    assert_eq!(read_file["method"], "workspace/readFile");
    let fault = format!("{SECRET_SOURCE}; {SECRET_PATH}; {SECRET_HASH}; {BACKEND_DETAIL}");
    faulting_child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read_file["id"].clone(),
        "error": { "code": -32603, "message": fault }
    }));
    let log = faulting_child.read_frame();
    assert_eq!(log["method"], "log");
    assert_eq!(
        log["params"]["msg"],
        "safe-delete declaration planning failed"
    );
    let log_json = log.to_string();
    for secret in [SECRET_SOURCE, SECRET_PATH, SECRET_HASH, BACKEND_DETAIL] {
        assert!(
            !log_json.contains(secret),
            "safe planner log must not disclose {secret}: {log_json}"
        );
    }
    faulting_child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": log["id"].clone(),
        "result": true
    }));
    let terminal = faulting_child.read_frame();
    assert_eq!(terminal["id"], 2);
    assert_eq!(
        terminal["result"]["data"]["message"],
        "safe-delete declaration planning failed"
    );
    let terminal_json = terminal.to_string();
    for secret in [SECRET_SOURCE, SECRET_PATH, SECRET_HASH, BACKEND_DETAIL] {
        assert!(
            !terminal_json.contains(secret),
            "safe planner terminal response must not disclose {secret}: {terminal_json}"
        );
    }
}

#[test]
fn safe_delete_declaration_resolves_span_position_and_hash_from_fresh_content() {
    let stale = b"pub fn target() {}\n";
    let fresh = "// shifted 🦀\npub fn target() -> u8 { 1 }\n".as_bytes();
    let (mut adapter, fs, apply_calls) = safe_delete_planner(stale);
    fs.lock()
        .expect("fs lock")
        .write(RelativePath::new("src/lib.rs"), fresh.to_vec())
        .expect("replace indexed content");

    let result = plan_safe_delete(&mut adapter, "target");

    let start = fresh
        .windows(b"pub fn target".len())
        .position(|window| window == b"pub fn target")
        .expect("fresh declaration start");
    assert_eq!(result["status"], "planned");
    assert_eq!(
        result["plan"]["position"],
        serde_json::json!({"line": 1, "character": 7})
    );
    assert_eq!(result["plan"]["deletion_span"]["start_byte"], start);
    assert_eq!(result["plan"]["deletion_span"]["end_byte"], fresh.len());
    assert_eq!(result["plan"]["base_hash"], compute_content_version(fresh));
    assert_eq!(apply_calls.load(Ordering::SeqCst), 0);
    adapter.shutdown();
}

#[test]
fn safe_delete_declaration_owns_only_one_immediately_trailing_newline() {
    let fixtures: &[(&[u8], usize)] = &[
        (b"pub fn target() {}\n", 19),
        (b"pub fn target() {}\r\n", 20),
        (b"pub fn target() {}", 18),
        (b"pub fn target() {}\npub fn next() {}\n", 19),
        (b"pub fn target() {}\n\npub fn next() {}\n", 19),
        (b"pub fn target() {} // trailing\n", 18),
    ];

    for &(content, expected_end) in fixtures {
        let (mut adapter, _fs, apply_calls) = safe_delete_planner(content);
        let result = plan_safe_delete(&mut adapter, "target");
        assert_eq!(result["status"], "planned", "fixture: {content:?}");
        assert_eq!(
            result["plan"]["deletion_span"],
            serde_json::json!({ "start_byte": 0, "end_byte": expected_end }),
            "fixture: {content:?}"
        );
        assert_eq!(apply_calls.load(Ordering::SeqCst), 0);
        adapter.shutdown();
    }
}

#[test]
fn safe_delete_declaration_preserves_existing_delete_symbol_process_behavior() {
    let content = b"pub fn delete_me() {}\npub fn keep_me() {}\n";
    let mut initial_fs = InMemoryFs::new();
    initial_fs
        .write(RelativePath::new("src/lib.rs"), content.to_vec())
        .expect("write fixture");
    let fs = Arc::new(Mutex::new(initial_fs));
    let mut adapter = SidecarHostAdapter::spawn(
        ast_manifest(&ast_extension_bin()),
        make_deps_with_apply_edits(fs.clone()),
        TEST_TIMEOUT,
        None,
    )
    .expect("ast must spawn");
    adapter
        .deliver_event(Event::FileIndexed {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileIndexed must seed AST index");

    let plan = plan_safe_delete(&mut adapter, "delete_me");
    assert_eq!(plan["status"], "planned");
    assert_eq!(
        fs.lock()
            .expect("fs lock")
            .read(&RelativePath::new("src/lib.rs"))
            .expect("read fixture"),
        content
    );

    let delete = adapter
        .call_tool(
            "delete_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "delete_me",
                "kind": "function"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("existing delete_symbol process contract must still succeed");
    assert_eq!(delete["applied"], true);
    assert_eq!(
        fs.lock()
            .expect("fs lock")
            .read(&RelativePath::new("src/lib.rs"))
            .expect("read edited fixture"),
        b"pub fn keep_me() {}\n"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// F007/T016: AST anchored symbol edit tools
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn t016_runtime_init_advertises_bare_ast_write_tools_and_request_apply_edits() {
    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());
    let manifest_toml =
        std::fs::read_to_string(workspace_root().join("extensions/ast/extension.toml"))
            .expect("ast extension manifest must be readable");
    let static_manifest: ExtensionManifest =
        toml::from_str(&manifest_toml).expect("manifest must parse");

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let initialized = adapter.manifest();
    let mut runtime_tools = initialized.tools.clone();
    assert!(
        runtime_tools
            .iter()
            .any(|tool| tool.name == "_tower_catch_up_fence"),
        "initialized host dispatch must retain fence ownership"
    );
    runtime_tools.retain(|tool| tool.name != "_tower_catch_up_fence");
    runtime_tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut manifest_tools = static_manifest.tools;
    manifest_tools.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(
        runtime_tools
            .iter()
            .any(|tool| tool.name == "plan_safe_delete_declaration"),
        "InitResult must advertise the declared planning tool"
    );
    assert_eq!(
        runtime_tools, manifest_tools,
        "public InitResult tool metadata must match extension.toml"
    );
    let tool_names: Vec<&str> = runtime_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    for expected in [
        "replace_symbol_body",
        "insert_before_symbol",
        "insert_after_symbol",
        "delete_symbol",
    ] {
        assert!(
            tool_names.contains(&expected),
            "InitResult must advertise {expected}, got {tool_names:?}"
        );
    }
    assert!(
        initialized
            .capabilities
            .required
            .contains(&"request_apply_edits".to_owned()),
        "InitResult must request request_apply_edits, got {:?}",
        initialized.capabilities.required
    );
    adapter.shutdown();
}

#[test]
fn t016_mcp_tool_discovery_exposes_prefixed_ast_write_tools_through_merge_layer() {
    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");
    let ext_registry = Arc::new(std::sync::RwLock::new(ExtensionRegistry::new()));
    ext_registry
        .write()
        .expect("extension registry lock")
        .register(adapter)
        .expect("register ast extension");

    let merged = ExtensionMergedRegistry::new(empty_engine_state(), ext_registry);
    let names = merged
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();

    assert!(
        names
            .iter()
            .any(|name| name == "tower_ast_plan_safe_delete_declaration"),
        "tools/list must expose the declared safe-delete planner; got {names:?}"
    );

    for expected in [
        "tower_ast_replace_symbol_body",
        "tower_ast_insert_before_symbol",
        "tower_ast_insert_after_symbol",
        "tower_ast_delete_symbol",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "tools/list must expose {expected} through the merge layer; got {names:?}"
        );
    }
}

#[test]
fn t016_extension_toml_declares_same_four_bare_ast_write_tools_and_request_apply_edits() {
    let toml = std::fs::read_to_string(workspace_root().join("extensions/ast/extension.toml"))
        .expect("ast extension manifest must be readable");
    let manifest: ExtensionManifest = toml::from_str(&toml).expect("manifest must parse");
    let tool_names: Vec<&str> = manifest
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();

    for expected in [
        "replace_symbol_body",
        "insert_before_symbol",
        "insert_after_symbol",
        "delete_symbol",
    ] {
        assert!(
            tool_names.contains(&expected),
            "extension.toml must declare {expected}, got {tool_names:?}"
        );
    }
    assert!(
        manifest
            .capabilities
            .required
            .contains(&"request_apply_edits".to_owned()),
        "extension.toml must declare request_apply_edits, got {:?}",
        manifest.capabilities.required
    );
}

#[test]
fn t016_real_ast_sidecar_applies_each_anchored_edit_and_refreshes_symbol_index_after_file_changed()
{
    let mut initial_fs = InMemoryFs::new();
    initial_fs
        .write(
            RelativePath::new("src/lib.rs"),
            b"pub fn remove_me() {}\n\npub fn target() -> u8 {\n    1\n}\n".to_vec(),
        )
        .expect("write must succeed");
    let fs = Arc::new(Mutex::new(initial_fs));

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps_with_apply_edits(fs.clone());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");
    adapter
        .deliver_event(Event::FileIndexed {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileIndexed must seed AST index");

    let replace = adapter
        .call_tool(
            "replace_symbol_body",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n    2\n"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("replace_symbol_body must succeed");
    assert_eq!(replace["applied"], true);
    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileChanged after replace must refresh AST index");

    let insert_before = adapter
        .call_tool(
            "insert_before_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "#[allow(dead_code)]\n"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("insert_before_symbol must succeed");
    assert_eq!(insert_before["applied"], true);
    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileChanged after insert-before must refresh AST index");

    let insert_after = adapter
        .call_tool(
            "insert_after_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n\npub fn added_after() -> u8 { 3 }\n"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("insert_after_symbol must succeed");
    assert_eq!(insert_after["applied"], true);
    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileChanged after insert-after must refresh AST index");

    let delete = adapter
        .call_tool(
            "delete_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "remove_me",
                "kind": "function"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("delete_symbol must succeed");
    assert_eq!(delete["applied"], true);

    let edited = {
        let fs = fs.lock().expect("fs lock");
        String::from_utf8(
            fs.read(&RelativePath::new("src/lib.rs"))
                .expect("read edited file"),
        )
        .expect("edited file must be UTF-8")
    };
    assert!(!edited.contains("remove_me"));
    assert!(edited.contains("#[allow(dead_code)]\npub fn target() -> u8 {\n    2\n}"));
    assert!(edited.contains("pub fn added_after() -> u8 { 3 }"));

    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileChanged must refresh AST index");
    let search = adapter
        .call_tool(
            "search_symbols",
            serde_json::json!({"name": "added_after"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("search_symbols must succeed after fileChanged");
    assert!(
        search.to_string().contains("added_after"),
        "symbol index must include post-edit symbol, got {search}"
    );
    adapter.shutdown();
}

#[test]
fn t016_dry_run_returns_preview_without_changing_file() {
    let original = b"pub fn target() {}\n".to_vec();
    let mut initial_fs = InMemoryFs::new();
    initial_fs
        .write(RelativePath::new("src/lib.rs"), original.clone())
        .expect("write must succeed");
    let fs = Arc::new(Mutex::new(initial_fs));

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps_with_apply_edits(fs.clone());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");
    adapter
        .deliver_event(Event::FileIndexed {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("fileIndexed must seed AST index");

    let result = adapter
        .call_tool(
            "insert_before_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "// preview\n",
                "dry_run": true
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("dry-run insert_before_symbol must succeed");

    assert_eq!(result["applied"], false);
    assert_eq!(result["files_changed"], 0);
    assert_eq!(result["preview"], "// preview\npub fn target() {}\n");
    let after = fs
        .lock()
        .expect("fs lock")
        .read(&RelativePath::new("src/lib.rs"))
        .expect("read file after dry-run");
    assert_eq!(after, original);
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC1 / U1: ast_extension declares all five tools
// ─────────────────────────────────────────────────────────────────────────────

/// AC1/U1: Given the `ast_extension` binary, When spawned, Then it initializes
/// and declares all five AST tools with the same names as the WASM plugin.
#[test]
fn ac1_ast_extension_declares_five_tools() {
    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let manifest = adapter.manifest();
    assert_eq!(manifest.name, "ast");

    let tool_names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"get_outline"),
        "must have get_outline, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"find_symbols"),
        "must have find_symbols, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"search_symbols"),
        "must have search_symbols, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"reindex"),
        "must have reindex, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"read_symbol"),
        "must have read_symbol, got: {tool_names:?}"
    );
    adapter.shutdown();
}

/// AC1: ast subscribes to both event/fileIndexed and event/fileChanged.
#[test]
fn ac1_ast_extension_subscribes_to_both_events() {
    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let m = adapter.manifest();
    assert!(
        m.events.subscribe.contains(&"event/fileIndexed".to_owned()),
        "must subscribe to event/fileIndexed, got: {:?}",
        m.events.subscribe
    );
    assert!(
        m.events.subscribe.contains(&"event/fileChanged".to_owned()),
        "must subscribe to event/fileChanged, got: {:?}",
        m.events.subscribe
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC1 parity: get_outline returns structural items for Rust (U1)
// ─────────────────────────────────────────────────────────────────────────────

/// AC1 parity: get_outline on a Rust file returns struct/impl/method/fn/trait items.
#[test]
fn ac1_get_outline_rust_returns_structural_items() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/counter.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "get_outline",
            serde_json::json!({"path": "src/counter.rs"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("get_outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"Counter\""),
        "must contain Counter struct, got: {result_str}"
    );
    assert!(
        result_str.contains("\"make_counter\""),
        "must contain make_counter fn, got: {result_str}"
    );
    assert!(
        result_str.contains("\"Resetable\""),
        "must contain Resetable trait, got: {result_str}"
    );
    // Structural items must have kind/name/start_byte fields.
    assert!(
        result_str.contains("\"start_byte\""),
        "items must have start_byte, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC1: get_outline on a non-Rust file returns unsupported result.
#[test]
fn ac1_get_outline_unsupported_language_returns_unsupported() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("README.md"),
        b"# Heading".to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "get_outline",
            serde_json::json!({"path": "README.md"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("get_outline must succeed (even for unsupported language)");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"unsupported\""),
        "non-Rust file must return unsupported result, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC2 / EV1: fileIndexed event → index update → find_symbols reflects it
// ─────────────────────────────────────────────────────────────────────────────

/// AC2/EV1: When fileIndexed is delivered, ast reparses the file and a
/// subsequent search_symbols call reflects the new symbols.
#[test]
fn ac2_file_indexed_event_updates_index() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    // Deliver fileIndexed event for the file.
    adapter
        .deliver_event(Event::FileIndexed {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("deliverEvent must succeed");

    // Search for a symbol that should be in the index.
    let result = adapter
        .call_tool(
            "search_symbols",
            serde_json::json!({"name": "make_counter"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("search_symbols must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"make_counter\""),
        "search_symbols must find make_counter after fileIndexed, got: {result_str}"
    );
    adapter.shutdown();
}

#[test]
fn ast_public_event_uses_unlimited_host_budget() {
    let mut fs = InMemoryFs::new();
    fs.write(RelativePath::new("src/lib.rs"), RUST_SOURCE.to_vec())
        .expect("write AST fixture");
    let mut adapter = SidecarHostAdapter::spawn(
        ast_manifest(&ast_extension_bin()),
        make_deps(fs),
        None,
        None,
    )
    .expect("AST must initialize without a host request deadline");

    adapter
        .deliver_event(Event::FileIndexed {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("AST fileIndexed must complete without a host request deadline");
    let result = adapter
        .call_tool(
            "search_symbols",
            serde_json::json!({"name": "make_counter"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("search must order after fileIndexed processing");
    assert!(result.to_string().contains("make_counter"));
    adapter.shutdown();
}

/// AC2: fileChanged event triggers reparse; subsequent find_symbols reflects
/// the updated symbols.
#[test]
fn ac2_file_changed_event_updates_index() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    // Deliver fileChanged event.
    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("deliverEvent must succeed");

    // Subsequent find_symbols must reflect the indexed content.
    let result = adapter
        .call_tool(
            "find_symbols",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "Counter",
                "kind": "struct"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("find_symbols must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"Counter\""),
        "find_symbols must find Counter after fileChanged, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC3: Multi-language parity (Go and PHP)
// ─────────────────────────────────────────────────────────────────────────────

/// AC3: get_outline on a Go file returns Go-specific structural items.
#[test]
fn ac3_get_outline_go_returns_structural_items() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("main.go"),
        GO_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "get_outline",
            serde_json::json!({"path": "main.go"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("get_outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"HelloWorld\""),
        "Go outline must contain HelloWorld fn, got: {result_str}"
    );
    assert!(
        result_str.contains("\"MyStruct\""),
        "Go outline must contain MyStruct, got: {result_str}"
    );
    assert!(
        result_str.contains("\"MyInterface\""),
        "Go outline must contain MyInterface, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC3: get_outline on a PHP file returns PHP-specific structural items.
#[test]
fn ac3_get_outline_php_returns_structural_items() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("app.php"),
        PHP_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "get_outline",
            serde_json::json!({"path": "app.php"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("get_outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"MyClass\""),
        "PHP outline must contain MyClass, got: {result_str}"
    );
    assert!(
        result_str.contains("\"MyInterface\""),
        "PHP outline must contain MyInterface, got: {result_str}"
    );
    assert!(
        result_str.contains("\"topLevelFn\""),
        "PHP outline must contain topLevelFn, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC3: find_symbols on a Go file finds a Go function.
#[test]
fn ac3_find_symbols_go_finds_function() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("main.go"),
        GO_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "find_symbols",
            serde_json::json!({
                "path": "main.go",
                "symbol_name": "HelloWorld",
                "kind": "function"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("find_symbols must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"HelloWorld\""),
        "find_symbols must find HelloWorld in Go, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC3: find_symbols with kind=impl on Go returns not-applicable (empty matches).
#[test]
fn ac3_find_symbols_go_inapplicable_kind_returns_empty() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("main.go"),
        GO_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "find_symbols",
            serde_json::json!({
                "path": "main.go",
                "symbol_name": "Foo",
                "kind": "impl"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("find_symbols with inapplicable kind must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"matches\""),
        "inapplicable kind must return matches key, got: {result_str}"
    );
    // matches must be empty
    let matches = result
        .get("matches")
        .and_then(|v| v.as_array())
        .expect("matches must be an array");
    assert!(
        matches.is_empty(),
        "inapplicable kind must return empty matches, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// U1: read_symbol returns symbol content
// ─────────────────────────────────────────────────────────────────────────────

/// U1: read_symbol returns the source content for a named symbol.
#[test]
fn u1_read_symbol_returns_symbol_content() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/counter.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "read_symbol",
            serde_json::json!({
                "path": "src/counter.rs",
                "symbol_name": "make_counter"
            }),
            &core_engine::domain::NeverCancelled,
        )
        .expect("read_symbol must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"make_counter\""),
        "read_symbol must return the symbol name, got: {result_str}"
    );
    assert!(
        result_str.contains("\"content\""),
        "read_symbol must return content field, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// UN1: parse failure on a file does not crash the extension
// ─────────────────────────────────────────────────────────────────────────────

/// UN1: If a file fails to parse (malformed Rust), ast records the partial
/// outline and continues serving others without crashing.
#[test]
fn un1_malformed_rust_file_does_not_crash_extension() {
    let malformed = b"fn broken( { struct // broken\n";
    let good = b"pub fn good_fn() {}";

    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/broken.rs"),
        malformed.to_vec(),
    )
    .expect("write must succeed");
    fs.write(
        core_engine::domain::RelativePath::new("src/good.rs"),
        good.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    // Outline the broken file — must not crash, must return Parsed (partial).
    let result = adapter
        .call_tool(
            "get_outline",
            serde_json::json!({"path": "src/broken.rs"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("get_outline must not crash on malformed input");
    // Result could be items:[] or items with partial content — not an error.
    assert!(
        result.get("items").is_some() || result.get("unsupported").is_some(),
        "malformed file must return outline or unsupported, got: {result:?}"
    );

    // The extension must still serve the good file correctly after the failure.
    let good_result = adapter
        .call_tool(
            "get_outline",
            serde_json::json!({"path": "src/good.rs"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("get_outline on good file must succeed after malformed parse");
    let good_str = good_result.to_string();
    assert!(
        good_str.contains("\"good_fn\""),
        "good file must still parse after malformed file, got: {good_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC4 / U4: hello extension — lazy greet tool
// ─────────────────────────────────────────────────────────────────────────────

/// AC4/U4: Given the `hello_extension`, When `greet` is invoked, Then it
/// returns a greeting; it is launched lazily on that first call.
#[test]
fn ac4_hello_extension_greet_returns_greeting() {
    let bin = hello_extension_bin();
    let manifest = hello_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("hello must spawn");

    let result = adapter
        .call_tool(
            "greet",
            serde_json::json!({"name": "Tower"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("greet must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("Tower"),
        "greet must include the name, got: {result_str}"
    );
    assert!(
        result_str.contains("Hello"),
        "greet must say Hello, got: {result_str}"
    );
    adapter.shutdown();
}

#[test]
fn hello_public_request_uses_unlimited_host_budget() {
    let mut adapter = SidecarHostAdapter::spawn(
        hello_manifest(&hello_extension_bin()),
        make_deps(InMemoryFs::new()),
        None,
        None,
    )
    .expect("hello must initialize without a host request deadline");

    let result = adapter
        .call_tool(
            "greet",
            serde_json::json!({"name": "Tower"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("hello request must complete without a host request deadline");
    assert!(result.to_string().contains("Hello, Tower"));
    adapter.shutdown();
}

/// AC4: hello extension declares a single `greet` tool, no events.
#[test]
fn ac4_hello_extension_declares_greet_no_events() {
    let bin = hello_extension_bin();
    let manifest = hello_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("hello must spawn");

    let m = adapter.manifest();
    let tool_names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"greet"),
        "hello must declare greet tool, got: {tool_names:?}"
    );
    assert!(
        m.events.subscribe.is_empty(),
        "hello must not subscribe to events (lazy, U4), got: {:?}",
        m.events.subscribe
    );
    adapter.shutdown();
}

/// AC4: hello with no name parameter returns a default greeting.
#[test]
fn ac4_hello_greet_default_name() {
    let bin = hello_extension_bin();
    let manifest = hello_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("hello must spawn");

    let result = adapter
        .call_tool(
            "greet",
            serde_json::json!({}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("greet with no name must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("Hello"),
        "default greet must say Hello, got: {result_str}"
    );
    // Default is "World"
    assert!(
        result_str.contains("World"),
        "default greet must use 'World' as name, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC5: ast_extension uses ExtensionRegistry — build via plain cargo
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Regression — bug #2: spawn→immediate-request deadlock
//
// Root cause: ast/main.rs performed a warm-start `index/get` HostCall AFTER
// sending the `Initialized` response. `spawn()` returned as soon as the host
// saw `Initialized`. The test thread then immediately wrote a new request frame
// (e.g. `invokeTool`) to the child's stdin. The child was still inside
// `read_host_response` for `index/get` (expecting id=10000). It read the
// incoming `invokeTool` frame (id=1), found id≠10000, and silently skipped
// (consumed) it. After receiving the `index/get` response the child looped
// back to `lines.next()`, but the `invokeTool` frame was gone — permanent
// deadlock, manifesting as `ExtensionFault::Timeout` after 15 s.
//
// Fix: all HostCalls inside the `initialize` handler now complete BEFORE the
// `Initialized` response is sent. `spawn()` therefore cannot return until the
// child is fully back in its outer request loop, so no subsequent write races
// with an outstanding `read_host_response`.
//
// Regression test: spawn `ast_extension` and IMMEDIATELY call a tool in a
// tight loop without any sleep. Under a parallel test run with 16+ cores,
// OS scheduling pressure is sufficient to trigger the old bug with very high
// probability within a few hundred iterations. We run 50 sequential
// spawn→call→shutdown cycles; each must complete without Timeout.
// ─────────────────────────────────────────────────────────────────────────────

/// REG/bug#2: spawn followed immediately by a tool call must never deadlock.
///
/// Each iteration spawns `ast_extension`, calls `search_symbols` without any
/// deliberate delay, and shuts down. The old code deadlocked here ~20% of the
/// time under parallel test execution because the warm-start `index/get`
/// HostCall in the `initialize` handler raced with the immediately-following
/// tool request. 50 consecutive clean iterations without `Timeout` confirms
/// the fix.
#[test]
fn reg_spawn_then_immediate_tool_call_never_deadlocks() {
    let bin = ast_extension_bin();

    for iteration in 0..50 {
        let manifest = ast_manifest(&bin);
        let deps = make_deps(InMemoryFs::new());

        let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
            .unwrap_or_else(|e| panic!("iteration {iteration}: spawn must succeed, got: {e:?}"));

        // Call a zero-I/O tool (search_symbols with empty index) immediately after
        // spawn, without any yield/sleep, to maximise scheduling interleave.
        let result = adapter.call_tool(
            "search_symbols",
            serde_json::json!({"name": "anything"}),
            &core_engine::domain::NeverCancelled,
        );
        assert!(
            result.is_ok(),
            "iteration {iteration}: call_tool must not deadlock/timeout, got: {:?}",
            result.err()
        );

        adapter.shutdown();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AC5: ast_extension uses ExtensionRegistry — build via plain cargo
// ─────────────────────────────────────────────────────────────────────────────

/// AC5: ast_extension works through ExtensionRegistry (the domain boundary).
/// This test proves the native binary integrates with the registry the same
/// way as any fake `ExtensionInstance`.
#[test]
fn ac5_ast_extension_via_registry() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("ast must spawn");

    let mut registry = ExtensionRegistry::new();
    registry.register(adapter).expect("must register ast");

    // Invoke get_outline through the registry (routes by tool name).
    let result = registry
        .invoke(
            "get_outline",
            serde_json::json!({"path": "src/lib.rs"}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("registry invoke must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"Counter\""),
        "registry invoke must return Counter outline, got: {result_str}"
    );
}
