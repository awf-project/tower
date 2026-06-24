//! Spec 27 — LSP extension **process-level** e2e tests.
//!
//! These tests spawn the actual `lsp_extension` binary via stdio and drive it
//! through the JSON-RPC protocol. They do NOT require a real language server.
//!
//! # What this verifies
//!
//! - **AC1/U1**: `lsp_extension` spawns, initialises, and declares exactly four
//!   tools (`diagnostics`, `definition`, `references`, `hover`) over the protocol.
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
use core_engine::ports::FileSystemPort;
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{Event, ExtensionManifest};

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

impl Drop for RawLspChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── AC1/U1: spawn and initialize ─────────────────────────────────────────────

/// AC1/U1: `lsp_extension` spawns, completes the initialize handshake, and
/// declares exactly four tools with the required names.
#[test]
fn lsp_process_spawns_and_declares_four_tools() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    let tool_names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tool_names.len(),
        4,
        "must declare exactly 4 tools; got: {tool_names:?}"
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

    adapter.shutdown();
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

/// AC1: `lsp_extension` declares the `notify` capability.
#[test]
fn lsp_process_declares_notify_capability() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None)
        .expect("lsp_extension must spawn");

    let m = adapter.manifest();
    assert!(
        m.capabilities.required.contains(&"notify".to_owned()),
        "must declare 'notify' capability; got: {:?}",
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

#[test]
fn lsp_sidecar_loop_uses_shared_harness_queue_and_id_allocator_directly() {
    let root = workspace_root();
    let main_rs =
        fs::read_to_string(root.join("extensions/lsp/src/main.rs")).expect("read lsp main.rs");
    let protocol_rs = fs::read_to_string(root.join("extensions/lsp/src/protocol.rs"))
        .expect("read lsp protocol.rs");

    assert!(
        main_rs.contains("HostCallIdAllocator::new(10_000)"),
        "LSP main loop must allocate host-call ids through the shared harness allocator"
    );
    assert!(
        main_rs.contains("VecDeque<QueuedFrame>"),
        "LSP main loop must queue harness QueuedFrame values, not tuple compatibility frames"
    );
    assert!(
        main_rs.contains("protocol::frame_from_envelope(envelope)"),
        "LSP idle loop must classify inbound frames through the harness frame parser"
    );
    assert!(
        !main_rs.contains("next_hcall_id: u64"),
        "LSP main loop must not keep a local raw u64 host-call counter"
    );
    assert!(
        !main_rs.contains("VecDeque<(Option<Value>, String, Value)>"),
        "LSP main loop must not retain tuple-shaped deferred frames"
    );
    assert!(
        !protocol_rs.contains("DeferredFrame"),
        "LSP protocol layer must not convert harness QueuedFrame values back into local tuples"
    );
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
    // Response carries Initialized data with the declared tools array.
    let tools = &init_resp["result"]["data"]["tools"];
    assert!(
        tools.is_array() && tools.as_array().unwrap().len() == 4,
        "must declare 4 tools; got: {tools} (full resp: {init_resp})"
    );

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
