//! Spec 27 — LSP extension **process-level** e2e tests.
//!
//! These tests spawn the actual `lsp_extension` binary via stdio and drive it
//! through the JSON-RPC protocol. They do NOT require a real language server.
//!
//! # What this verifies
//!
//! - **AC1/U1**: `lsp_extension` spawns, initialises, and declares the LSP tools
//!   (`diagnostics`, `definition`, `references`, `hover`, `implementations`,
//!   `rename`) over the protocol.
//! - **EV1**: `deliverEvent` for `fileChanged` and `fileDeleted` completes cleanly.
//! - **Diagnostics unsupported**: with no LSP configured, `diagnostics` returns
//!   `{supported: false}` rather than an error.
//! - **Push-response hazard regression** (KNOWN PROTOCOL HAZARDS §1): a
//!   response frame that arrives on stdin during an idle main-loop spin (i.e. the
//!   push thread sent `notify/resourceUpdated` and the host ACKed it) is silently
//!   discarded. The subsequent `invokeTool` call must NOT receive a spurious
//!   `ProtocolError`.
//! - **Concurrency stress** (KNOWN PROTOCOL HAZARDS §1 mandate): ≥ 20 parallel
//!   spawns, each running initialize → invokeTool → shutdown, with no race or
//!   deadlock.
//!
//! # Binary location
//!
//! `lsp_extension` is a `default-members` binary, so `cargo test --workspace`
//! compiles it into `target/debug/` before running these tests.

#![allow(clippy::pedantic)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost;
use core_engine::adapters::extension::{HostDeps, SidecarHostAdapter};
use core_engine::adapters::formatter::NoOpFormatQueue;
use core_engine::adapters::{InMemoryAstIndex, InMemoryFs};
use core_engine::domain::mutation::compute_content_version;
use core_engine::ports::FileSystemPort;
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{Event, ExtensionManifest, InitResult};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

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

fn lsp_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("lsp_extension")
        .to_str()
        .unwrap()
        .to_owned()
}

fn lsp_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "lsp".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Lazy,
        tools: vec![],
        events: EventsSection {
            subscribe: vec![
                "event/fileChanged".to_owned(),
                "event/fileDeleted".to_owned(),
            ],
        },
        capabilities: CapabilitiesSection {
            required: vec![
                "read_file".to_owned(),
                "notify".to_owned(),
                "log".to_owned(),
                "request_apply_edits".to_owned(),
            ],
        },
    }
}

fn make_deps() -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(InMemoryFs::new())),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    }
}

struct RawLspChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RawLspChild {
    fn spawn() -> Self {
        Self::spawn_with_workspace(None)
    }

    fn spawn_with_workspace(workspace: Option<&std::path::Path>) -> Self {
        let mut command = Command::new(lsp_extension_bin());
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::inherit());
        if let Some(path) = workspace {
            command.env("TOWER_WORKSPACE", path);
        }

        let mut child = command.spawn().expect("spawn lsp extension");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn write_frame(&mut self, frame: serde_json::Value) {
        let line = serde_json::to_string(&frame).expect("serialize frame");
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    fn read_frame(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read frame");
        assert!(
            !line.is_empty(),
            "child stdout closed before a frame arrived"
        );
        serde_json::from_str(line.trim()).expect("valid JSON frame")
    }
}

fn fake_lsp_workspace(mode: &str) -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("create fake lsp workspace");
    fs::create_dir_all(workspace.path().join(".tower")).expect("create .tower");
    fs::create_dir_all(workspace.path().join("src")).expect("create src");
    fs::write(workspace.path().join("src/lib.rs"), "fn old_name() {}\n")
        .expect("write source file");

    let script = workspace.path().join("fake_lsp.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

MODE = sys.argv[1]

def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    body = sys.stdin.buffer.read(int(headers["content-length"]))
    return json.loads(body.decode("utf-8"))

def send_message(payload):
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    method = message.get("method")
    request_id = message.get("id")
    if request_id is None:
        continue
    params = message.get("params") or {}
    text_document = params.get("textDocument") or {}
    uri = text_document.get("uri") or "file:///workspace/src/lib.rs"

    if method == "initialize":
        send_message({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "implementationProvider": True,
                    "renameProvider": True if MODE == "no_prepare" else {"prepareProvider": True}
                }
            }
        })
    elif method == "textDocument/implementation":
        if MODE == "backend_error":
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32099, "message": "implementation backend exploded"}
            })
        else:
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": [{
                    "uri": uri,
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 11}
                    }
                }]
            })
    elif method == "textDocument/prepareRename":
        if MODE == "no_prepare":
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32603, "message": "prepareRename should not be called"}
            })
        elif MODE == "prepare_method_not_found":
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "method not found"}
            })
        elif MODE == "reject":
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32602, "message": "not valid rename target"}
            })
        elif MODE == "unsupported_edit":
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {"documentChanges": [{"kind": "create", "uri": uri}]}
            })
        else:
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 11}
                    },
                    "placeholder": "old_name"
                }
            })
    elif method == "textDocument/rename":
        new_name = params.get("newName", "new_name")
        text_edit = {
            "range": {
                "start": {"line": 0, "character": 3},
                "end": {"line": 0, "character": 11}
            },
            "newText": new_name
        }
        if MODE == "versioned_document_changes":
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "documentChanges": [{
                        "textDocument": {"uri": uri, "version": 7},
                        "edits": [text_edit]
                    }]
                }
            })
        else:
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "changes": {
                        uri: [text_edit]
                    }
                }
            })
    else:
        send_message({"jsonrpc": "2.0", "id": request_id, "result": None})
"#,
    )
    .expect("write fake lsp script");

    let config = format!(
        r#"
[lsp.rust]
command = "python3"
args = ["{}", "{}"]
extensions = ["rs"]
"#,
        script.display(),
        mode
    );
    fs::write(workspace.path().join(".tower/config.toml"), config).expect("write lsp config");

    workspace
}

fn initialize_raw_lsp_child(child: &mut RawLspChild) {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0"
        }
    }));
    let initialized = child.read_frame();
    assert!(
        initialized.get("result").is_some(),
        "initialize must succeed; got: {initialized}"
    );
}

fn initialized_raw_lsp_child(child: &mut RawLspChild) -> serde_json::Value {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0"
        }
    }));
    let initialized = child.read_frame();
    assert!(
        initialized.get("result").is_some(),
        "initialize must succeed; got: {initialized}"
    );
    initialized
}

impl Drop for RawLspChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── AC1/U1: spawn and initialize ─────────────────────────────────────────────

/// Runtime `InitResult` advertises bare tool names `implementations` and
/// `rename`; MCP tool discovery exposes them publicly as
/// `tower_lsp_implementations` and `tower_lsp_rename` through the merge-layer
/// prefix.
#[test]
fn f007_t015_lsp_process_spawns_and_declares_implementations_and_rename_tools() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    let tool_names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tool_names.len(),
        6,
        "must declare exactly 6 tools; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"diagnostics"),
        "must declare 'diagnostics'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"definition"),
        "must declare 'definition'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"references"),
        "must declare 'references'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"hover"),
        "must declare 'hover'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"implementations"),
        "must declare 'implementations'; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"rename"),
        "must declare 'rename'; got: {tool_names:?}"
    );

    adapter.shutdown();
}

/// E2E/protocol tests assert tool discovery parity between `extension.toml` and
/// runtime `InitResult` metadata.
#[test]
fn f007_t015_lsp_runtime_initresult_tool_metadata_matches_extension_toml() {
    let manifest_path = workspace_root().join("extensions/lsp/extension.toml");
    let toml = fs::read_to_string(&manifest_path).expect("read lsp extension.toml");
    let manifest: ExtensionManifest = toml::from_str(&toml).expect("manifest must parse");
    let mut manifest_tools = manifest.tools.clone();
    manifest_tools.sort_by(|a, b| a.name.cmp(&b.name));

    let mut child = RawLspChild::spawn();
    let initialized = initialized_raw_lsp_child(&mut child);
    assert_eq!(initialized["result"]["type"], "Initialized");
    let init: InitResult = serde_json::from_value(initialized["result"]["data"].clone())
        .expect("runtime InitResult must deserialize");
    let mut runtime_tools = init.tools;
    runtime_tools.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(
        runtime_tools, manifest_tools,
        "runtime InitResult tool metadata must stay in parity with extension.toml"
    );
    assert!(
        runtime_tools
            .iter()
            .any(|tool| tool.name == "implementations")
    );
    assert!(runtime_tools.iter().any(|tool| tool.name == "rename"));
}

/// AC1: `lsp_extension` subscribes to `event/fileChanged` and `event/fileDeleted`.
#[test]
fn lsp_process_subscribes_to_file_events() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    assert!(
        m.events.subscribe.contains(&"event/fileChanged".to_owned()),
        "must subscribe to event/fileChanged; got: {:?}",
        m.events.subscribe
    );
    assert!(
        m.events.subscribe.contains(&"event/fileDeleted".to_owned()),
        "must subscribe to event/fileDeleted; got: {:?}",
        m.events.subscribe
    );

    adapter.shutdown();
}

/// `extensions/lsp/extension.toml` declares bare tool names `implementations`
/// and `rename`, plus privileged capability `request_apply_edits`.
#[test]
fn f007_t015_lsp_process_declares_request_apply_edits_capability() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    assert!(
        m.capabilities
            .required
            .contains(&"request_apply_edits".to_owned()),
        "must declare 'request_apply_edits' capability; got: {:?}",
        m.capabilities.required
    );

    adapter.shutdown();
}

#[test]
fn lsp_process_queues_host_requests_seen_during_read_file_hostcall_and_discards_idle_responses() {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    let tower_dir = workspace.path().join(".tower");
    fs::create_dir_all(&tower_dir).expect("create .tower");
    fs::write(
        tower_dir.join("config.toml"),
        r#"
[lsp.rust]
command = "definitely-missing-tower-lsp-server"
extensions = ["rs"]
"#,
    )
    .expect("write lsp config");

    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION,
            "client_info": "lsp-e2e/0.1.0"
        }
    }));
    let initialized = child.read_frame();
    assert!(
        initialized.get("result").is_some(),
        "initialize must succeed; got: {initialized}"
    );

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "diagnostics",
            "params": { "path": "src/lib.rs" }
        }
    }));
    let first_read = child.read_frame();
    assert_eq!(
        first_read["method"], "workspace/readFile",
        "configured diagnostics must request file contents before answering; got: {first_read}"
    );
    let first_read_id = first_read["id"].clone();

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "invokeTool",
        "params": {
            "name": "diagnostics",
            "params": { "path": "src/lib.rs" }
        }
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 20_000,
        "result": true
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "shutdown",
        "params": {}
    }));
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": first_read_id,
        "result": "fn main() {}\n"
    }));

    let first_response = child.read_frame();
    assert_eq!(
        first_response["id"], 1,
        "original invokeTool response must be returned before queued frames; got: {first_response}"
    );

    let queued_read = child.read_frame();
    assert_eq!(
        queued_read["method"], "workspace/readFile",
        "queued invokeTool must replay before shutdown and issue its own readFile; got: {queued_read}"
    );
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": queued_read["id"].clone(),
        "result": "fn main() {}\n"
    }));

    let queued_response = child.read_frame();
    assert_eq!(
        queued_response["id"], 2,
        "queued invokeTool response must preserve FIFO order before shutdown; got: {queued_response}"
    );

    let shutdown = child.read_frame();
    assert_eq!(
        shutdown["id"], 3,
        "shutdown request queued during HostCall wait must run after earlier invokeTool; got: {shutdown}"
    );
    assert!(
        shutdown.get("result").is_some(),
        "shutdown must return a result; got: {shutdown}"
    );
}

#[test]
fn lsp_initialize_errors_preserve_json_rpc_codes_for_malformed_params_version_mismatch_and_unknown_methods()
 {
    let mut child = RawLspChild::spawn();

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "initialize",
        "params": { "protocol_version": extension_protocol::PROTOCOL_VERSION }
    }));
    let malformed = child.read_frame();
    assert_eq!(malformed["id"], 10);
    assert_eq!(malformed["error"]["code"], -32602);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "initialize",
        "params": {
            "protocol_version": extension_protocol::PROTOCOL_VERSION + 1,
            "client_info": "lsp-e2e/0.1.0"
        }
    }));
    let mismatch = child.read_frame();
    assert_eq!(mismatch["id"], 11);
    assert_eq!(mismatch["error"]["code"], -32600);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "definitelyUnknown",
        "params": null
    }));
    let unknown = child.read_frame();
    assert_eq!(unknown["id"], 12);
    assert_eq!(unknown["error"]["code"], -32601);
}

// ── Diagnostics unsupported path ──────────────────────────────────────────────

/// With no language servers configured, `tower_lsp_diagnostics` returns
/// `{supported: false}` (not an error) for any path.
#[test]
fn lsp_process_diagnostics_unsupported_when_no_lsp_configured() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    // Build InMemoryFs before wrapping so we can write to it.
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        b"fn main() {}".to_vec(),
    )
    .expect("write must succeed");
    let deps = HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    };

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let result = adapter
        .call_tool("diagnostics", serde_json::json!({ "path": "src/lib.rs" }))
        .expect("diagnostics must not error");

    // Without any LSP config, the pool does not serve .rs — returns unsupported.
    let supported = result.get("supported").and_then(|v| v.as_bool());
    assert_eq!(
        supported,
        Some(false),
        "must return {{supported: false}} when no LSP configured; got: {result}"
    );

    adapter.shutdown();
}

/// `tower_lsp_implementations` parses `LspImplementationRequest` fields `path`,
/// `line`, and `character`.
#[test]
fn f007_t015_tower_lsp_implementations_parses_lsp_implementation_request_fields() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let result = adapter
        .call_tool(
            "implementations",
            serde_json::json!({
                "path": "src/lib.rs",
                "line": 3,
                "character": 14
            }),
        )
        .expect("implementations request with path, line, and character must parse");

    assert!(
        result.is_object(),
        "implementations must return a structured result after parsing; got: {result}"
    );

    adapter.shutdown();
}

/// `tower_lsp_implementations` returns `LspImplementationResult { supported:
/// false, locations: [] }` when `NavigationPort::implementations` returns
/// `CodeIntelError::Unsupported`; backend failures still surface as the existing
/// sidecar tool error path.
#[test]
fn f007_t015_tower_lsp_implementations_returns_unsupported_result_for_unsupported_navigation() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let result = adapter
        .call_tool(
            "implementations",
            serde_json::json!({
                "path": "README.md",
                "line": 0,
                "character": 0
            }),
        )
        .expect("unsupported implementations lookup must return a result, not a sidecar error");

    assert_eq!(result["supported"], serde_json::json!(false));
    assert_eq!(result["locations"], serde_json::json!([]));

    adapter.shutdown();
}

/// `tower_lsp_implementations` surfaces backend failures through the existing
/// sidecar tool error path rather than converting them to unsupported results.
#[test]
fn f007_t015_tower_lsp_implementations_surfaces_backend_failures_as_sidecar_tool_errors() {
    let workspace = fake_lsp_workspace("backend_error");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "implementations",
            "params": { "path": "src/lib.rs", "line": 0, "character": 3 }
        }
    }));

    let read = child.read_frame();
    assert_eq!(
        read["method"],
        serde_json::json!("workspace/readFile"),
        "configured implementations lookup must request file contents before LSP query; got {read}"
    );
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["error"]["code"],
        serde_json::json!(-32000),
        "backend failure must use the sidecar tool error path; got {response}"
    );
    let message = response["error"]["message"]
        .as_str()
        .expect("sidecar error must include a message");
    assert!(
        message.contains("textDocument/implementation error"),
        "error message must name the failed LSP request; got {message:?}"
    );
    assert!(
        message.contains("implementation backend exploded"),
        "error message must preserve backend details; got {message:?}"
    );
}

/// `tower_lsp_implementations(path, line, character)` returns
/// `LspImplementationResult { supported: true, locations }`, with each location
/// in the same `Location` shape as existing `tower_lsp_definition` and
/// `tower_lsp_references`.
#[test]
fn f007_t015_tower_lsp_implementations_returns_supported_locations_in_existing_location_shape() {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "implementations",
            "params": { "path": "src/lib.rs", "line": 0, "character": 3 }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    let result = &response["result"]["data"];
    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(result["supported"], serde_json::json!(true));
    assert_eq!(
        result["locations"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(result["locations"][0]["line"], serde_json::json!(0));
    assert_eq!(result["locations"][0]["character"], serde_json::json!(3));
    assert_eq!(result["locations"][0]["endLine"], serde_json::json!(0));
    assert_eq!(
        result["locations"][0]["endCharacter"],
        serde_json::json!(11)
    );
}

/// `tower_lsp_rename(path, line, character, new_name, dry_run?)` parses
/// `RenameRequest`, calls prepareRename when available, and returns
/// `RenameError { code: RenameErrorCode::NotRenameable, ... }` serialized as
/// `not_renameable` without HostCall when prepareRename rejects the position.
#[test]
fn f007_t015_tower_lsp_rename_returns_not_renameable_without_apply_edits_hostcall_when_prepare_rename_rejects()
 {
    let workspace = fake_lsp_workspace("reject");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name"
            }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], serde_json::json!(1));
    assert!(
        response.get("method").is_none(),
        "prepareRename rejection must not issue workspace/applyEdits; got {response}"
    );
    assert_eq!(
        response["result"]["data"]["code"],
        serde_json::json!("not_renameable")
    );
}

/// `tower_lsp_rename` rejects unsupported WorkspaceEdit operations with
/// `RenameError { code: RenameErrorCode::UnsupportedWorkspaceEdit, ... }`
/// serialized as `unsupported_workspace_edit` before calling
/// `workspace/applyEdits`.
#[test]
fn f007_t015_tower_lsp_rename_rejects_unsupported_workspace_edit_before_apply_edits_hostcall() {
    let workspace = fake_lsp_workspace("unsupported_edit");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name"
            }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], serde_json::json!(1));
    assert!(
        response.get("method").is_none(),
        "unsupported WorkspaceEdit must not issue workspace/applyEdits; got {response}"
    );
    assert_eq!(
        response["result"]["data"]["code"],
        serde_json::json!("unsupported_workspace_edit")
    );
}

fn respond_to_apply_edits(child: &mut RawLspChild, dry_run: bool) -> serde_json::Value {
    let apply = child.read_frame();
    assert_eq!(apply["method"], "workspace/applyEdits");
    assert_eq!(apply["params"]["dry_run"], serde_json::json!(dry_run));
    assert_eq!(
        apply["params"]["edits"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(
        apply["params"]["edits"][0]["start_byte"],
        serde_json::json!(3)
    );
    assert_eq!(
        apply["params"]["edits"][0]["end_byte"],
        serde_json::json!(11)
    );
    assert_eq!(
        apply["params"]["edits"][0]["replacement"],
        serde_json::json!("new_name")
    );
    assert_eq!(
        apply["params"]["edits"][0]["base_hash"],
        serde_json::json!(compute_content_version(b"fn old_name() {}\n"))
    );
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": apply["id"].clone(),
        "result": {
            "files_changed": if dry_run { 0 } else { 1 },
            "per_file": [{
                "path": "src/lib.rs",
                "applied": !dry_run,
                "edits_applied": 1,
                "edits_skipped": 0,
                "new_version": if dry_run { serde_json::Value::Null } else { serde_json::json!("abc123") },
                "preview": "fn new_name() {}\n"
            }]
        }
    }));
    apply
}

fn invoke_supported_rename(child: &mut RawLspChild, dry_run: bool) -> serde_json::Value {
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name",
                "dry_run": dry_run
            }
        }
    }));

    let read = child.read_frame();
    assert_eq!(read["method"], "workspace/readFile");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));

    respond_to_apply_edits(child, dry_run);
    child.read_frame()
}

/// For non-dry-run rename with supported text edits, `tower_lsp_rename`
/// performs exactly one `workspace/applyEdits` HostCall containing all decoded
/// spans and `dry_run: false`.
#[test]
fn f007_t015_tower_lsp_rename_non_dry_run_performs_one_apply_edits_hostcall_with_decoded_spans_and_dry_run_false()
 {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, false);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["applied"],
        serde_json::json!(true)
    );
    assert_eq!(
        response["result"]["data"]["files_changed"],
        serde_json::json!(1)
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
}

#[test]
fn f007_t015_tower_lsp_rename_skips_prepare_when_server_does_not_advertise_prepare_provider() {
    let workspace = fake_lsp_workspace("no_prepare");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, false);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
}

#[test]
fn f007_t015_tower_lsp_rename_falls_back_to_rename_when_prepare_method_is_missing() {
    let workspace = fake_lsp_workspace("prepare_method_not_found");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, false);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
}

/// For `dry_run: true`, `tower_lsp_rename` performs exactly one
/// `workspace/applyEdits` HostCall containing all decoded spans and
/// `dry_run: true`; the host returns preview/per-file data and performs no
/// mutation.
#[test]
fn f007_t015_tower_lsp_rename_dry_run_performs_one_apply_edits_hostcall_with_decoded_spans_and_dry_run_true()
 {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, true);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["preview"],
        serde_json::json!("fn new_name() {}\n")
    );
    assert_eq!(
        response["result"]["data"]["per_file"][0]["applied"],
        serde_json::json!(false)
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
}

#[test]
fn f007_t015_tower_lsp_rename_accepts_versioned_document_changes_text_edits() {
    let workspace = fake_lsp_workspace("versioned_document_changes");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let response = invoke_supported_rename(&mut child, true);

    assert_eq!(response["id"], serde_json::json!(1));
    assert_eq!(
        response["result"]["data"]["spans"][0]["path"],
        serde_json::json!("src/lib.rs")
    );
    assert_eq!(
        response["result"]["data"]["spans"][0]["replacement"],
        serde_json::json!("new_name")
    );
    assert_eq!(
        response["result"]["data"]["preview"],
        serde_json::json!("fn new_name() {}\n")
    );
}

/// Rename success returns `RenameResult` with fields `applied`, `files_changed`,
/// `spans`, `preview`, and `per_file`; rename dry-run returns `RenamePreview`
/// with fields `spans`, `preview`, and `per_file`.
#[test]
fn f007_t015_tower_lsp_rename_success_and_dry_run_return_rename_result_and_rename_preview_fields() {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    let result_response = invoke_supported_rename(&mut child, false);
    let result = &result_response["result"]["data"];
    assert!(result.get("applied").is_some());
    assert!(result.get("files_changed").is_some());
    assert!(result.get("spans").is_some());
    assert!(result.get("preview").is_some());
    assert!(result.get("per_file").is_some());

    let preview_response = invoke_supported_rename(&mut child, true);
    let preview = &preview_response["result"]["data"];
    assert!(preview.get("spans").is_some());
    assert!(preview.get("preview").is_some());
    assert!(preview.get("per_file").is_some());
    assert!(preview.get("applied").is_none());
    assert!(preview.get("files_changed").is_none());
    assert_eq!(preview["spans"][0]["start_byte"], serde_json::json!(3));
    assert_eq!(preview["spans"][0]["end_byte"], serde_json::json!(11));
    assert_eq!(
        preview["spans"][0]["base_hash"],
        serde_json::json!(compute_content_version(b"fn old_name() {}\n"))
    );
}

/// Host apply failures are surfaced in `RenameResult.per_file[*].error` as
/// `WorkspaceApplyEditsError` with exact `WorkspaceApplyEditsErrorCode` values
/// from T010.
#[test]
fn f007_t015_tower_lsp_rename_surfaces_host_apply_failures_in_per_file_error_with_exact_workspace_apply_edits_error_code()
 {
    let workspace = fake_lsp_workspace("ok");
    let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
    initialize_raw_lsp_child(&mut child);

    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": {
            "name": "rename",
            "params": {
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name"
            }
        }
    }));
    let read = child.read_frame();
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": read["id"].clone(),
        "result": "fn old_name() {}\n"
    }));
    let apply = child.read_frame();
    assert_eq!(apply["method"], "workspace/applyEdits");
    child.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": apply["id"].clone(),
        "result": {
            "files_changed": 0,
            "per_file": [{
                "path": "src/lib.rs",
                "applied": false,
                "edits_applied": 0,
                "edits_skipped": 1,
                "error": {
                    "code": "cas_conflict",
                    "message": "stale base hash",
                    "path": "src/lib.rs"
                }
            }]
        }
    }));

    let response = child.read_frame();
    assert_eq!(
        response["result"]["data"]["per_file"][0]["error"]["code"],
        serde_json::json!("cas_conflict")
    );
}

// ── EV1: document sync events ─────────────────────────────────────────────────

/// EV1: `event/fileChanged` is delivered without error.
#[test]
fn lsp_process_file_changed_event_completes() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let event = Event::FileChanged {
        file_id: 1,
        path: "src/main.rs".to_owned(),
    };
    adapter
        .deliver_event(event)
        .expect("fileChanged must succeed");

    adapter.shutdown();
}

/// EV1: `event/fileDeleted` is delivered without error.
#[test]
fn lsp_process_file_deleted_event_completes() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let event = Event::FileDeleted {
        path: "src/lib.rs".to_owned(),
    };
    adapter
        .deliver_event(event)
        .expect("fileDeleted must succeed");

    adapter.shutdown();
}

// ── Push-response frame hazard regression ─────────────────────────────────────

/// Regression: KNOWN PROTOCOL HAZARDS §1 — push-response frames injected during
/// idle must NOT cause the next `invokeTool` call to receive a spurious
/// `ProtocolError`.
///
/// This test drives the binary at the raw JSON-RPC level so it can inject a
/// response frame (simulating the host's ACK to `notify/resourceUpdated`)
/// during idle, then verify the next invokeTool returns a valid result.
#[test]
fn lsp_process_push_response_frame_during_idle_is_discarded() {
    use extension_protocol::PROTOCOL_VERSION;

    let bin = lsp_extension_bin();

    let mut child = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("lsp_extension must spawn");

    let mut child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(child_stdout);

    // ── Helper closures ──

    let send = |stdin: &mut std::process::ChildStdin, frame: serde_json::Value| {
        let s = serde_json::to_string(&frame).unwrap();
        stdin.write_all(s.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };

    let recv = |reader: &mut BufReader<std::process::ChildStdout>| -> serde_json::Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).expect("valid JSON from extension")
    };

    // ── 1. initialize handshake ──────────────────────────────────────────────
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocol_version": PROTOCOL_VERSION,
                "client_info": "test-host/0.1.0"
            }
        }),
    );

    // Read the Initialized response.
    let init_resp = recv(&mut reader);
    assert!(
        init_resp.get("result").is_some(),
        "initialize must return a result; got: {init_resp}"
    );
    let tools = &init_resp["result"]["data"]["tools"];
    let tool_names = tools
        .as_array()
        .expect("tools must be an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(
        tool_names.len(),
        6,
        "must declare 6 tools; got: {tool_names:?} (full resp: {init_resp})"
    );
    assert!(tool_names.contains(&"implementations"));
    assert!(tool_names.contains(&"rename"));

    // ── 2. Inject a push-response frame (simulates host ACK of notify/resourceUpdated) ──
    //
    // The push thread would have sent a notify/resourceUpdated request.
    // and the host would ACK with {"jsonrpc":"2.0","result":true,"id":20000}.
    // We inject this ACK directly into the extension's stdin while it is in idle.
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 20000,
            "result": true
        }),
    );

    // ── 3. Now send an invokeTool — must NOT get a spurious ProtocolError ─────
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "invokeTool",
            "params": {
                "name": "diagnostics",
                "params": { "path": "src/lib.rs" }
            }
        }),
    );

    // The extension needs to readFile — handle that HostCall.
    // (The pool.serves() check returns false for unconfigured .rs, so actually
    // there may be no HostCall. But if there is one, we respond to it.)
    let mut response_frame: Option<serde_json::Value> = None;
    for _ in 0..5 {
        let frame = recv(&mut reader);
        if frame.get("method").is_some() {
            // It's a HostCall — respond success.
            let call_id = frame["id"].clone();
            send(
                &mut child_stdin,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": call_id,
                    "result": "" // empty file content
                }),
            );
        } else {
            // It's the invokeTool response.
            response_frame = Some(frame);
            break;
        }
    }

    let resp = response_frame.expect("must have received invokeTool response within 5 frames");

    // The response MUST be a result, NOT an error triggered by the injected
    // push-response frame being misinterpreted as a request.
    assert!(
        resp.get("result").is_some(),
        "invokeTool after push-response injection must return a result, not an error; got: {resp}"
    );
    assert!(
        resp.get("error").is_none(),
        "invokeTool must not return an error; got: {resp}"
    );
    // The id must match the invokeTool request (id=1), not the push ACK (id=20000).
    assert_eq!(
        resp["id"],
        serde_json::json!(1),
        "response id must match invokeTool request id=1; got: {resp}"
    );

    // ── 4. shutdown ──────────────────────────────────────────────────────────
    send(
        &mut child_stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": {}
        }),
    );
    let shutdown_resp = recv(&mut reader);
    assert!(
        shutdown_resp.get("result").is_some(),
        "shutdown must return a result; got: {shutdown_resp}"
    );

    let _ = child.wait();
}

// ── Concurrency stress (KNOWN PROTOCOL HAZARDS mandate: ≥ 20 parallel) ────────

/// KNOWN PROTOCOL HAZARDS §1 mandate: spawn ≥ 20 `lsp_extension` instances in
/// parallel, each running initialize → invokeTool(diagnostics) → shutdown.
/// Any race, deadlock, or protocol corruption surfaces as a timeout or panic.
#[test]
fn lsp_process_concurrent_spawn_stress_20_parallel() {
    const N: usize = 20;

    let bin = lsp_extension_bin();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let bin = bin.clone();
            std::thread::spawn(move || {
                let manifest = lsp_manifest(&bin);
                let deps = make_deps();

                let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
                    .unwrap_or_else(|e| panic!("spawn #{i} failed: {e:?}"));

                // Call a tool that completes without a real LSP.
                let result = adapter
                    .call_tool("diagnostics", serde_json::json!({ "path": "src/main.rs" }))
                    .unwrap_or_else(|e| panic!("call_tool #{i} failed: {e:?}"));

                // Verify the result is sane (supported=false — no LSP configured).
                assert!(
                    result.is_object(),
                    "#{i}: result must be an object; got: {result}"
                );

                adapter.shutdown();
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join().unwrap_or_else(|_| panic!("thread #{i} panicked"));
    }
}

/// A 20-iteration parallel sidecar stress test covers LSP HostCalls made while
/// inbound frames may be queued.
#[test]
fn f007_t015_lsp_rename_hostcalls_queue_inbound_frames_under_20_iteration_parallel_stress() {
    const N: usize = 20;

    let handles = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let workspace = fake_lsp_workspace("ok");
                let mut child = RawLspChild::spawn_with_workspace(Some(workspace.path()));
                initialize_raw_lsp_child(&mut child);

                child.write_frame(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "invokeTool",
                    "params": {
                        "name": "rename",
                        "params": {
                            "path": "src/lib.rs",
                            "line": 0,
                            "character": 3,
                            "new_name": "new_name"
                        }
                    }
                }));

                let read = child.read_frame();
                assert_eq!(
                    read["method"], "workspace/readFile",
                    "#{i}: rename must request file content before LSP rename; got {read}"
                );
                child.write_frame(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "shutdown",
                    "params": {}
                }));
                child.write_frame(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": read["id"].clone(),
                    "result": "fn old_name() {}\n"
                }));

                respond_to_apply_edits(&mut child, false);

                let rename_response = child.read_frame();
                assert_eq!(
                    rename_response["id"],
                    serde_json::json!(1),
                    "#{i}: rename response must precede queued shutdown; got {rename_response}"
                );
                assert!(
                    rename_response.get("error").is_none(),
                    "#{i}: rename must not fail under queued-frame stress; got {rename_response}"
                );

                let shutdown_response = child.read_frame();
                assert_eq!(
                    shutdown_response["id"],
                    serde_json::json!(2),
                    "#{i}: queued shutdown must run after rename; got {shutdown_response}"
                );
            })
        })
        .collect::<Vec<_>>();

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("stress iteration #{i} panicked"));
    }
}

/// Same stress test but also injects push-response frames concurrently on each
/// instance to validate the hazard fix under load.
#[test]
fn lsp_process_concurrent_spawn_stress_20_with_push_response_injection() {
    use extension_protocol::PROTOCOL_VERSION;

    const N: usize = 20;

    let bin = lsp_extension_bin();
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

                // initialize
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

                // Inject push-response frame (simulates notify/resourceUpdated ACK).
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 20000_u64 + i as u64,
                        "result": true
                    }),
                );

                // invokeTool(diagnostics) — must succeed despite the injected frame.
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "method": "invokeTool",
                        "params": { "name": "diagnostics", "params": { "path": "src/main.rs" } }
                    }),
                );

                // drain any HostCalls and collect the invokeTool response
                let mut invoke_resp: Option<serde_json::Value> = None;
                for _ in 0..5 {
                    let frame = read_frame(&mut reader);
                    if frame.get("method").is_some() {
                        let call_id = frame["id"].clone();
                        write_frame(
                            &mut stdin,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": call_id,
                                "result": ""
                            }),
                        );
                    } else {
                        invoke_resp = Some(frame);
                        break;
                    }
                }
                let resp = invoke_resp.unwrap_or_else(|| panic!("#{i}: no invokeTool response"));
                assert!(
                    resp.get("error").is_none(),
                    "#{i}: invokeTool must not error after push-response injection; got: {resp}"
                );
                assert_eq!(
                    resp["id"],
                    serde_json::json!(1),
                    "#{i}: response id must be 1; got: {resp}"
                );

                // shutdown
                write_frame(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 2,
                        "method": "shutdown",
                        "params": {}
                    }),
                );
                let shutdown = read_frame(&mut reader);
                assert!(
                    shutdown.get("result").is_some(),
                    "#{i}: shutdown must succeed; got: {shutdown}"
                );

                let _ = child.wait();
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join()
            .unwrap_or_else(|_| panic!("stress thread #{i} panicked"));
    }
}
