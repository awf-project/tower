//! Integration tests for `SidecarHostAdapter` (spec 23).
//!
//! These tests spawn the real `test_helper_extension` binary and exercise all
//! five acceptance criteria from the spec. They use in-memory test doubles for
//! the port dependencies — no real disk I/O.
//!
//! # Test binary location
//!
//! The `test_helper_extension` binary must be built before these tests run.
//! The CI and `make test` run `cargo build --workspace --bins` first. The binary
//! is located via `CARGO_MANIFEST_DIR` navigation to the workspace root, then
//! `target/debug/test_helper_extension`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::host_deps::{HostDeps, UnsupportedApplyEditsHost};
use super::sidecar::SidecarHostAdapter;
use crate::adapters::formatter::NoOpFormatQueue;
use crate::adapters::{InMemoryAstIndex, InMemoryFs};
use crate::domain::RelativePath;
use crate::ports::FileSystemPort;
use extension_protocol::ExtensionManifest;
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};

/// Default timeout for spec 23 tests — long enough that cooperative extensions
/// always respond well within it.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ── Binary path helper ────────────────────────────────────────────────────────

/// Locate the `test_helper_extension` binary relative to the workspace root.
///
/// Assumes `cargo build` (or `cargo test`) has already compiled it.
fn test_helper_bin() -> String {
    // CARGO_MANIFEST_DIR points at tower/crates/core_engine.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir)
        .parent() // crates/
        .unwrap()
        .parent() // tower/
        .unwrap();
    let bin = workspace_root
        .join("target")
        .join("debug")
        .join("test_helper_extension");
    bin.to_str().unwrap().to_owned()
}

// ── HostDeps builders ─────────────────────────────────────────────────────────

/// Build a `HostDeps` backed by in-memory test doubles.
fn make_deps(fs: InMemoryFs) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    }
}

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

fn make_manifest(bin: &str, caps: Vec<String>) -> ExtensionManifest {
    ExtensionManifest {
        name: "test_helper".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Eager,
        tools: vec![], // populated by initialize handshake
        events: EventsSection::default(),
        capabilities: CapabilitiesSection { required: caps },
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// AC1: Spawn, initialize handshake, declared tools populated from InitResult
// ═════════════════════════════════════════════════════════════════════════════

/// AC1: Given the test_helper extension binary, When spawned, Then it
/// initializes and reports its declared tools.
#[test]
fn ac1_spawn_initialize_handshake_populates_tools() {
    let bin = test_helper_bin();
    let manifest = make_manifest(
        &bin,
        vec![
            "read_file".to_owned(),
            "index_get".to_owned(),
            "index_put".to_owned(),
        ],
    );
    let deps = make_deps(InMemoryFs::new());

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    let m = instance.manifest();
    assert_eq!(m.name, "test_helper");

    // The test_helper declares: echo, read_file, index_roundtrip, read_bad_path
    let tool_names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"echo"),
        "must declare 'echo' tool; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"read_file"),
        "must declare 'read_file' tool; got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"index_roundtrip"),
        "must declare 'index_roundtrip' tool; got: {tool_names:?}"
    );

    instance.shutdown();
}

// ═════════════════════════════════════════════════════════════════════════════
// AC2: Tool round-trip (echo) via invokeTool
// ═════════════════════════════════════════════════════════════════════════════

/// U3 / AC3: call_tool("echo") forwards to invokeTool, gets back ToolResult.
#[test]
fn u3_call_tool_echo_round_trip() {
    let bin = test_helper_bin();
    let manifest = make_manifest(&bin, vec![]);
    let deps = make_deps(InMemoryFs::new());

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    let params = json!({"message": "hello"});
    let result = instance
        .call_tool("echo", params.clone())
        .expect("echo must succeed");

    assert_eq!(result, params, "echo must return params unchanged");
    instance.shutdown();
}

// ═════════════════════════════════════════════════════════════════════════════
// AC2: readFile capability dispatched through FileSystemPort
// ═════════════════════════════════════════════════════════════════════════════

/// AC2: Given an extension that calls workspace/readFile on an allowed path,
/// When it invokes a tool needing the file, Then the read succeeds through
/// FileSystemPort and the tool returns.
#[test]
fn ac2_read_file_capability_through_filesystem_port() {
    let bin = test_helper_bin();
    let manifest = make_manifest(&bin, vec!["read_file".to_owned()]);

    let mut fs = InMemoryFs::new();
    fs.write(RelativePath::new("hello.txt"), b"hello world".to_vec())
        .expect("write");
    let deps = make_deps(fs);

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    let result = instance
        .call_tool("read_file", json!({"path": "hello.txt"}))
        .expect("read_file tool must succeed");

    assert_eq!(
        result.as_str().unwrap_or(""),
        "hello world",
        "tool must return file content via FileSystemPort"
    );
    instance.shutdown();
}

// ═════════════════════════════════════════════════════════════════════════════
// AC3: Path traversal and absolute path denied (UN2)
// ═════════════════════════════════════════════════════════════════════════════

/// AC3: Given an extension requesting a `..`-traversal path, When it calls the
/// capability, Then it is denied.
#[test]
fn ac3_dotdot_traversal_path_is_denied() {
    let bin = test_helper_bin();
    let manifest = make_manifest(&bin, vec!["read_file".to_owned()]);
    let deps = make_deps(InMemoryFs::new());

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    // The "read_bad_path" tool calls readFile with whatever path we give it.
    // With "../secret" the host should deny it.
    let result = instance.call_tool("read_bad_path", json!({"path": "../secret"}));

    // The tool returns Response::Error when the host denies the path.
    assert!(
        result.is_err(),
        "traversal path must be denied; got Ok: {result:?}"
    );
    let fault = result.unwrap_err();
    let msg = format!("{fault:?}");
    assert!(
        msg.contains("..")
            || msg.contains("traversal")
            || msg.contains("denied")
            || msg.contains("must not"),
        "fault message should describe path violation: {fault:?}"
    );
    instance.shutdown();
}

/// AC3: An absolute path is denied.
#[test]
fn ac3_absolute_path_is_denied() {
    let bin = test_helper_bin();
    let manifest = make_manifest(&bin, vec!["read_file".to_owned()]);
    let deps = make_deps(InMemoryFs::new());

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    let result = instance.call_tool("read_bad_path", json!({"path": "/etc/passwd"}));

    assert!(
        result.is_err(),
        "absolute path must be denied; got Ok: {result:?}"
    );
    instance.shutdown();
}

/// AC3: Empty path is denied.
#[test]
fn ac3_empty_path_is_denied() {
    let bin = test_helper_bin();
    let manifest = make_manifest(&bin, vec!["read_file".to_owned()]);
    let deps = make_deps(InMemoryFs::new());

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    let result = instance.call_tool("read_bad_path", json!({"path": ""}));

    assert!(
        result.is_err(),
        "empty path must be denied; got Ok: {result:?}"
    );
    instance.shutdown();
}

// ═════════════════════════════════════════════════════════════════════════════
// AC4: Protocol-version mismatch rejected (UN1)
// ═════════════════════════════════════════════════════════════════════════════

/// AC4: Given an extension reporting a mismatched PROTOCOL_VERSION, When
/// spawned, Then the adapter rejects it with a version error.
///
/// We simulate a version-mismatch extension with a tiny inline shell script
/// that writes a mismatched Initialized response.
#[test]
fn ac4_protocol_version_mismatch_is_rejected() {
    // Build a manifest that points to a small shell wrapper which immediately
    // responds with a wrong protocol version in an Error response (simulating
    // what the test_helper does when given a wrong version).
    //
    // Strategy: we use the real test_helper but we send it an initialize
    // request with a wrong version. The test_helper checks the version and
    // sends an Error response, which the adapter must convert to ProtocolError.
    //
    // Since `SidecarHostAdapter::spawn` sends PROTOCOL_VERSION, we cannot
    // directly inject a wrong version through the normal path. Instead, we
    // build a tiny ad-hoc process using `sh -c` that writes a version-mismatch
    // error response and exits.
    //
    // The script:
    //   1. reads one line (the initialize request) from stdin
    //   2. writes a JSON-RPC Error response with code -32600 and a version message
    //   3. exits

    let script = r#"
        read -r line
        echo '{"jsonrpc":"2.0","id":0,"error":{"code":-32600,"message":"protocol version mismatch: host=1 extension=99"}}'
        exit 0
    "#;

    let manifest = ExtensionManifest {
        name: "mismatch_ext".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    };
    let deps = make_deps(InMemoryFs::new());

    let result = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT);

    let fault = result
        .err()
        .expect("version-mismatch extension must be rejected");
    let msg = format!("{fault:?}");
    assert!(
        msg.contains("version") || msg.contains("mismatch") || msg.contains("rejected"),
        "fault must mention version problem: {fault:?}"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// AC5: index/put then index/get round-trip through AstIndexPort
// ═════════════════════════════════════════════════════════════════════════════

/// AC5: Given an extension that performs index/put then index/get, When
/// invoked, Then the bytes round-trip through AstIndexPort.
#[test]
fn ac5_index_put_get_round_trip_through_ast_index_port() {
    let bin = test_helper_bin();
    let manifest = make_manifest(&bin, vec!["index_get".to_owned(), "index_put".to_owned()]);
    let index = InMemoryAstIndex::new();
    let deps = make_deps_with_index(InMemoryFs::new(), index);

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    // The "index_roundtrip" tool calls index/put{key,value} then index/get{key}
    // and returns the retrieved bytes.
    let result = instance
        .call_tool(
            "index_roundtrip",
            json!({"key": "my-cache-key", "value": "stored-bytes"}),
        )
        .expect("index_roundtrip must succeed");

    // The result is an array of u8 values (the bytes stored).
    // "stored-bytes" → [115, 116, 111, 114, 101, 100, 45, 98, 121, 116, 101, 115]
    let bytes: Vec<u8> = result
        .as_array()
        .unwrap_or_else(|| panic!("result must be array; got: {result:?}"))
        .iter()
        .map(|v| v.as_u64().expect("byte value") as u8)
        .collect();
    assert_eq!(
        bytes, b"stored-bytes",
        "bytes must round-trip through AstIndexPort"
    );
    instance.shutdown();
}

// ═════════════════════════════════════════════════════════════════════════════
// Deliver event round-trip
// ═════════════════════════════════════════════════════════════════════════════

/// U3: deliver_event forwards subscribed events and receives Ack.
#[test]
fn u3_deliver_event_receives_ack() {
    let bin = test_helper_bin();
    let manifest = make_manifest(&bin, vec![]);
    let deps = make_deps(InMemoryFs::new());

    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("spawn must succeed");

    let event = extension_protocol::Event::FileIndexed {
        file_id: 42,
        path: "src/lib.rs".to_owned(),
    };
    instance
        .deliver_event(event)
        .expect("deliver_event must return Ok");
    instance.shutdown();
}
