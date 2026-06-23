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

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
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

// ── AC1/U1: spawn and initialize ─────────────────────────────────────────────

/// AC1/U1: `lsp_extension` spawns, completes the initialize handshake, and
/// declares exactly four tools with the required names.
#[test]
fn lsp_process_spawns_and_declares_four_tools() {
    let bin = lsp_extension_bin();
    let manifest = lsp_manifest(&bin);
    let deps = make_deps();

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("lsp_extension must spawn");

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

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("lsp_extension must spawn");

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

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("lsp_extension must spawn");

    let m = adapter.manifest();
    assert!(
        m.capabilities.required.contains(&"notify".to_owned()),
        "must declare 'notify' capability; got: {:?}",
        m.capabilities.required
    );

    adapter.shutdown();
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

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("lsp_extension must spawn");

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

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("lsp_extension must spawn");

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

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("lsp_extension must spawn");

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

                let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT)
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
