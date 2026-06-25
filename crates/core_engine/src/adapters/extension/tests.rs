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

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{
    fs,
    process::{Command, Stdio},
};

use serde_json::json;

use super::host_deps::{ApplyEditsHostPort, HostDeps, UnsupportedApplyEditsHost};
use super::sidecar::SidecarHostAdapter;
use crate::adapters::cli::GlobalOpts;
use crate::adapters::config::TowerConfig;
use crate::adapters::daemon::engine::build_engine;
use crate::adapters::formatter::NoOpFormatQueue;
use crate::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::{InMemoryAstIndex, InMemoryFs};
use crate::domain::mutation::compute_content_version;
use crate::domain::{DomainError, RelativePath};
use crate::ports::FileSystemPort;
use crate::ports::inbound::{
    PerFileEditResult, WorkspaceApplyEditsError, WorkspaceApplyEditsErrorCode,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult,
};
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{ExtensionFault, ExtensionManifest};

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

fn make_deps_with_apply_edits(apply_edits: Arc<dyn ApplyEditsHostPort>) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(InMemoryFs::new())),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits,
        push_tx: None,
    }
}

#[derive(Debug)]
enum ApplyEditsHostBehavior {
    Result(WorkspaceApplyEditsResult),
    VersionConflict,
}

#[derive(Debug)]
struct RecordingBatchApplyEditsHost {
    calls: Mutex<Vec<WorkspaceApplyEditsRequest>>,
    behavior: ApplyEditsHostBehavior,
    write_attempts: AtomicUsize,
    file_changed_fanout_attempts: AtomicUsize,
}

impl RecordingBatchApplyEditsHost {
    fn returning(result: WorkspaceApplyEditsResult) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            behavior: ApplyEditsHostBehavior::Result(result),
            write_attempts: AtomicUsize::new(0),
            file_changed_fanout_attempts: AtomicUsize::new(0),
        }
    }

    fn version_conflict() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            behavior: ApplyEditsHostBehavior::VersionConflict,
            write_attempts: AtomicUsize::new(0),
            file_changed_fanout_attempts: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> Vec<WorkspaceApplyEditsRequest> {
        self.calls.lock().expect("call recorder lock").clone()
    }

    fn write_attempts(&self) -> usize {
        self.write_attempts.load(Ordering::SeqCst)
    }

    fn file_changed_fanout_attempts(&self) -> usize {
        self.file_changed_fanout_attempts.load(Ordering::SeqCst)
    }
}

impl ApplyEditsHostPort for RecordingBatchApplyEditsHost {
    fn apply_batch_edits(
        &self,
        request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        let dry_run = request.dry_run == Some(true);
        self.calls.lock().expect("call recorder lock").push(request);
        match &self.behavior {
            ApplyEditsHostBehavior::Result(result) => {
                if !dry_run {
                    let changed = result.per_file.iter().filter(|entry| entry.applied).count();
                    self.write_attempts.fetch_add(changed, Ordering::SeqCst);
                    self.file_changed_fanout_attempts
                        .fetch_add(changed, Ordering::SeqCst);
                }
                Ok(result.clone())
            }
            ApplyEditsHostBehavior::VersionConflict => Err(DomainError::VersionConflict {
                expected: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
                actual: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
            }),
        }
    }
}

fn apply_edits_result(per_file: Vec<PerFileEditResult>) -> WorkspaceApplyEditsResult {
    WorkspaceApplyEditsResult {
        files_changed: per_file.iter().filter(|entry| entry.applied).count(),
        per_file,
    }
}

fn applied_file(path: &str, preview: Option<&str>) -> PerFileEditResult {
    PerFileEditResult {
        path: RelativePath::new(path),
        applied: true,
        edits_applied: 1,
        edits_skipped: 0,
        new_version: Some(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
        ),
        preview: preview.map(str::to_owned),
        error: None,
    }
}

fn failed_file(path: &str, code: WorkspaceApplyEditsErrorCode, message: &str) -> PerFileEditResult {
    PerFileEditResult {
        path: RelativePath::new(path),
        applied: false,
        edits_applied: 0,
        edits_skipped: 1,
        new_version: None,
        preview: None,
        error: Some(WorkspaceApplyEditsError {
            code,
            message: message.to_owned(),
            path: Some(RelativePath::new(path)),
        }),
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

fn make_apply_edits_script_manifest(name: &str, caps: Vec<String>) -> ExtensionManifest {
    let script = r#"
import json
import sys

init = json.loads(sys.stdin.readline())
print(json.dumps({
    "jsonrpc": "2.0",
    "id": init.get("id"),
    "result": {
        "type": "Initialized",
        "data": {
            "tools": [{"name": "apply_edits", "description": "Call workspace/applyEdits", "schema_json": "{}"}],
            "events": [],
            "capabilities": []
        }
    }
}), flush=True)

request = json.loads(sys.stdin.readline())
params = request.get("params", {}).get("params", {})
print(json.dumps({
    "jsonrpc": "2.0",
    "id": 7001,
    "method": "workspace/applyEdits",
    "params": params
}), flush=True)
host_response = json.loads(sys.stdin.readline())
print(json.dumps({
    "jsonrpc": "2.0",
    "id": request.get("id"),
    "result": {"type": "ToolResult", "data": host_response}
}), flush=True)

shutdown = sys.stdin.readline()
if shutdown:
    shutdown_request = json.loads(shutdown)
    print(json.dumps({
        "jsonrpc": "2.0",
        "id": shutdown_request.get("id"),
        "result": {"type": "Ack"}
    }), flush=True)
"#;

    ExtensionManifest {
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        command: vec!["python3".to_owned(), "-c".to_owned(), script.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection { required: caps },
    }
}

fn invoke_apply_edits_host_call(
    caps: Vec<String>,
    deps: HostDeps,
    params: serde_json::Value,
) -> serde_json::Value {
    invoke_apply_edits_host_call_as("ast", caps, deps, params)
}

fn invoke_apply_edits_host_call_as(
    manifest_name: &str,
    caps: Vec<String>,
    deps: HostDeps,
    params: serde_json::Value,
) -> serde_json::Value {
    let manifest = make_apply_edits_script_manifest(manifest_name, caps);
    let mut instance =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");
    let result = instance
        .call_tool("apply_edits", params)
        .expect("apply_edits helper must return the host response");
    instance.shutdown();
    result
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .expect("extension script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod extension script");
    }
}

fn write_batch_apply_edits_extension(ext_root: &Path, first_file_version: &str) {
    let ext_dir = ext_root.join("apply_batch");
    fs::create_dir_all(&ext_dir).expect("create batch apply extension dir");
    fs::write(
        ext_dir.join("extension.toml"),
        r#"
name = "ast"
version = "0.1.0"
command = ["./ast_extension"]
activation = "lazy"

[[tools]]
name = "replace"
description = "Request a multi-file workspace/applyEdits host call."
schema_json = "{}"

[capabilities]
required = ["request_apply_edits"]
"#,
    )
    .expect("write batch apply manifest");

    let script = format!(
        r#"#!/bin/sh
IFS= read -r _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"type":"Initialized","data":{{"tools":[{{"name":"replace","description":"Request a multi-file workspace/applyEdits host call.","schema_json":"{{}}"}}],"events":[],"capabilities":["request_apply_edits"]}}}}}}'
IFS= read -r _invoke
printf '%s\n' '{{"jsonrpc":"2.0","id":99,"method":"workspace/applyEdits","params":{{"edits":[{{"path":"src/a.txt","start_byte":0,"end_byte":5,"replacement":"ALPHA","base_hash":"{first_file_version}"}},{{"path":"src/missing.txt","start_byte":0,"end_byte":1,"replacement":"x","base_hash":"{first_file_version}"}}],"dry_run":false}}}}'
IFS= read -r host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"type":"ToolResult","data":{{"host_response":'"$host_response"'}}}}}}'
IFS= read -r _shutdown
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"type":"Ack"}}}}'
"#
    );
    let script_path = ext_dir.join("ast_extension");
    fs::write(&script_path, script).expect("write batch apply script");
    make_executable(&script_path);
}

fn write_reentrant_file_changed_apply_edits_extension(
    ext_root: &Path,
    first_expected_version: &str,
    second_expected_version: &str,
) {
    let ext_dir = ext_root.join("apply_reentrant");
    fs::create_dir_all(&ext_dir).expect("create reentrant apply extension dir");
    fs::write(
        ext_dir.join("extension.toml"),
        r#"
name = "ast"
version = "0.1.0"
command = ["./ast_extension"]
activation = "eager"

[[tools]]
name = "replace"
description = "Request workspace/applyEdits and request another edit from fileChanged."
schema_json = "{}"

[events]
subscribe = ["event/fileChanged"]

[capabilities]
required = ["request_apply_edits"]
"#,
    )
    .expect("write reentrant apply manifest");

    let script = format!(
        r#"#!/bin/sh
IFS= read -r _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"type":"Initialized","data":{{"tools":[{{"name":"replace","description":"Request workspace/applyEdits and request another edit from fileChanged.","schema_json":"{{}}"}}],"events":["event/fileChanged"],"capabilities":["request_apply_edits"]}}}}}}'
IFS= read -r _invoke
printf '%s\n' '{{"jsonrpc":"2.0","id":99,"method":"workspace/applyEdits","params":{{"edits":[{{"path":"src/lint_target.txt","start_byte":6,"end_byte":10,"replacement":"fixed","base_hash":"{first_expected_version}"}}],"dry_run":false}}}}'
IFS= read -r _first_host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"type":"ToolResult","data":{{"ok":true}}}}}}'
IFS= read -r _event
printf '%s\n' '{{"jsonrpc":"2.0","id":100,"method":"workspace/applyEdits","params":{{"edits":[{{"path":"src/lint_target.txt","start_byte":0,"end_byte":5,"replacement":"omega","base_hash":"{second_expected_version}"}}],"dry_run":false}}}}'
IFS= read -r _second_host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"type":"Ack"}}}}'
IFS= read -r _shutdown
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"type":"Ack"}}}}'
"#
    );
    let script_path = ext_dir.join("ast_extension");
    fs::write(&script_path, script).expect("write reentrant apply script");
    make_executable(&script_path);
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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

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

#[test]
fn sidecar_host_adapter_do_initialize_can_populate_the_new_field_without_changing_protocol_version_semantics()
 {
    let script = r#"
        read -r line
        ok=1
        case "$line" in *'"method":"initialize"'*) ;; *) ok=0 ;; esac
        case "$line" in *'"protocol_version":1'*) ;; *) ok=0 ;; esac
        case "$line" in *'"extension_config"'*) ;; *) ok=0 ;; esac
        case "$line" in *'"lldb-dap"'*) ;; *) ok=0 ;; esac
        if [ "$ok" = 1 ]; then
            echo '{"jsonrpc":"2.0","id":0,"result":{"type":"Initialized","data":{"tools":[],"events":[],"capabilities":[]}}}'
        else
            echo '{"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"initialize config missing or protocol version changed"}}'
        fi
    "#;
    let manifest = ExtensionManifest {
        name: "captures_initialize_config".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    };
    let deps = make_deps(InMemoryFs::new());

    let instance = SidecarHostAdapter::spawn_with_config(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(json!({
            "languages": {
                "rust": {
                    "command": "lldb-dap"
                }
            }
        })),
    )
    .expect("initialize request must include extension_config without changing protocol version");

    assert!(instance.manifest().tools.is_empty());
}

#[test]
fn sidecar_host_adapter_initialize_surfaces_extension_config_validation_errors() {
    let script = r#"
        read -r line
        case "$line" in
            *'"extension_config":["not-object"]'*)
                echo '{"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"extension_config must be an object"}}'
                ;;
            *)
                echo '{"jsonrpc":"2.0","id":0,"result":{"type":"Initialized","data":{"tools":[],"events":[],"capabilities":[]}}}'
                ;;
        esac
    "#;
    let manifest = ExtensionManifest {
        name: "rejects_bad_initialize_config".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    };
    let deps = make_deps(InMemoryFs::new());

    let result = SidecarHostAdapter::spawn_with_config(
        manifest,
        deps,
        TEST_TIMEOUT,
        Some(json!(["not-object"])),
    );
    let fault = match result {
        Ok(mut instance) => {
            instance.shutdown();
            panic!("malformed extension_config must be rejected by the initializing extension");
        }
        Err(fault) => fault,
    };

    match fault {
        ExtensionFault::ProtocolError { message } => {
            assert!(
                message.contains("extension_config"),
                "fault must identify the invalid initialize config field: {message}"
            );
            assert!(
                message.contains("object"),
                "fault must preserve the extension's validation message: {message}"
            );
        }
        other => panic!("expected protocol error for invalid extension_config, got {other:?}"),
    }
}

#[test]
fn spawn_with_config_reaps_child_when_initialize_is_rejected() {
    let pid_file = tempfile::NamedTempFile::new().expect("pid file must be creatable");
    let pid_path = pid_file.path().to_owned();
    let script = r#"
        printf '%s\n' "$$" > "$1"
        read -r _line
        echo '{"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"extension_config must be an object"}}'
        while true; do sleep 1; done
    "#;
    let manifest = ExtensionManifest {
        name: "rejects_initialize_and_keeps_running".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "rejects_initialize_and_keeps_running".to_owned(),
            pid_path.to_string_lossy().into_owned(),
        ],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    };
    let deps = make_deps(InMemoryFs::new());

    let result = SidecarHostAdapter::spawn_with_config(
        manifest,
        deps,
        Duration::from_secs(2),
        Some(json!(["not-object"])),
    );
    match result {
        Err(ExtensionFault::ProtocolError { .. }) => {}
        Err(other) => {
            panic!("initialize rejection must surface as a protocol error, got {other:?}")
        }
        Ok(mut instance) => {
            instance.shutdown();
            panic!("initialize rejection must not produce a live extension instance");
        }
    }

    let pid_text = fs::read_to_string(&pid_path).expect("sidecar must record its pid");
    let pid = pid_text
        .trim()
        .parse::<u32>()
        .expect("pid file must contain a process id");
    for _ in 0..20 {
        if !process_is_running(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    terminate_process(pid);
    panic!("initialize rejection must kill and reap the sidecar process {pid}");
}

fn process_is_running(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn terminate_process(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status();
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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

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

    let result = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None);

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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

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
// T012: workspace/applyEdits HostCall wiring
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn sidecar_host_adapter_dispatches_workspace_apply_edits_to_batch_mutation_dependency_when_and_only_when_request_apply_edits_is_declared()
 {
    let expected = apply_edits_result(vec![applied_file("src/main.rs", None)]);
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(expected.clone()));
    let deps = make_deps_with_apply_edits(apply_edits.clone());

    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [{
                "path": "src/main.rs",
                "start_byte": 0,
                "end_byte": 0,
                "replacement": "use std::fmt;\n",
                "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }],
            "dry_run": false
        }),
    );

    assert_eq!(response["result"], serde_json::to_value(expected).unwrap());
    assert_eq!(apply_edits.calls().len(), 1);
    assert_eq!(
        apply_edits.calls()[0].edits[0].base_hash.as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
}

#[test]
fn caller_without_request_apply_edits_receives_capability_error_and_no_mutation_occurs() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![applied_file("src/main.rs", None)],
    )));
    let deps = make_deps_with_apply_edits(apply_edits.clone());

    let response = invoke_apply_edits_host_call(
        Vec::new(),
        deps,
        json!({
            "edits": [{
                "path": "src/main.rs",
                "start_byte": 0,
                "end_byte": 0,
                "replacement": "x"
            }]
        }),
    );

    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("capability 'request_apply_edits' not declared")
    );
    assert!(apply_edits.calls().is_empty());
}

#[test]
fn third_party_manifest_cannot_self_declare_request_apply_edits_to_mutate_workspace() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![applied_file("src/main.rs", None)],
    )));
    let deps = make_deps_with_apply_edits(apply_edits.clone());

    let response = invoke_apply_edits_host_call_as(
        "third_party_refactor",
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [{
                "path": "src/main.rs",
                "start_byte": 0,
                "end_byte": 0,
                "replacement": "x"
            }]
        }),
    );

    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("capability 'request_apply_edits' not declared")
    );
    assert!(
        apply_edits.calls().is_empty(),
        "untrusted extensions must be rejected before the mutation host is invoked"
    );
}

#[test]
fn workspace_apply_edits_rejects_empty_absolute_and_parent_traversal_paths_as_invalid_path() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![failed_file(
            "",
            WorkspaceApplyEditsErrorCode::InvalidPath,
            "path must be workspace-relative",
        )],
    )));

    for path in ["", "/tmp/escape.rs", "../escape.rs"] {
        let deps = make_deps_with_apply_edits(apply_edits.clone());
        let response = invoke_apply_edits_host_call(
            vec!["request_apply_edits".to_owned()],
            deps,
            json!({
                "edits": [{
                    "path": path,
                    "start_byte": 0,
                    "end_byte": 0,
                    "replacement": "x",
                    "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }]
            }),
        );

        assert_eq!(
            response["result"]["per_file"][0]["error"]["code"], "invalid_path",
            "path {path:?} must be represented as WorkspaceApplyEditsErrorCode::InvalidPath"
        );
    }
}

#[test]
fn mixed_valid_and_invalid_apply_edits_batch_applies_valid_targets_and_reports_invalid_paths() {
    let expected = apply_edits_result(vec![applied_file("src/a.rs", None)]);
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(expected));
    let deps = make_deps_with_apply_edits(apply_edits.clone());

    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [
                {"path": "src/a.rs", "start_byte": 0, "end_byte": 0, "replacement": "a", "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                {"path": "../escape.rs", "start_byte": 0, "end_byte": 0, "replacement": "b", "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
            ]
        }),
    );

    let calls = apply_edits.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]
            .edits
            .iter()
            .map(|edit| edit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.rs"],
        "only valid targets should be delegated to the mutation host"
    );
    assert_eq!(response["result"]["files_changed"], 1);
    assert_eq!(response["result"]["per_file"].as_array().unwrap().len(), 2);
    assert_eq!(response["result"]["per_file"][0]["path"], "src/a.rs");
    assert_eq!(response["result"]["per_file"][1]["path"], "../escape.rs");
    assert_eq!(
        response["result"]["per_file"][1]["error"]["code"],
        "invalid_path"
    );
}

#[test]
fn present_malformed_base_hash_or_non_string_cas_is_invalid_params_and_never_unconditional_write() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![applied_file("src/main.rs", None)],
    )));

    for base_hash in [json!("not-a-sha256"), json!(1234)] {
        let deps = make_deps_with_apply_edits(apply_edits.clone());
        let response = invoke_apply_edits_host_call(
            vec!["request_apply_edits".to_owned()],
            deps,
            json!({
                "edits": [{
                    "path": "src/main.rs",
                    "start_byte": 0,
                    "end_byte": 0,
                    "replacement": "x",
                    "base_hash": base_hash
                }]
            }),
        );

        assert_eq!(response["error"]["code"], -32602);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with("invalid_params: base_hash")
        );
    }
    assert!(apply_edits.calls().is_empty());
}

#[test]
fn missing_base_hash_for_mutating_apply_edits_is_invalid_params_and_never_unconditional_write() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![applied_file("src/main.rs", None)],
    )));
    let deps = make_deps_with_apply_edits(apply_edits.clone());
    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [{
                "path": "src/main.rs",
                "start_byte": 0,
                "end_byte": 0,
                "replacement": "x"
            }],
            "dry_run": false
        }),
    );

    assert_eq!(response["error"]["code"], -32602);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("base_hash is required")
    );
    assert!(apply_edits.calls().is_empty());
}

#[test]
fn valid_base_hash_values_map_to_the_existing_optimistic_cas_mechanism_for_each_affected_file() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![applied_file("src/main.rs", None)],
    )));
    let deps = make_deps_with_apply_edits(apply_edits.clone());

    let _response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [
                {
                    "path": "src/main.rs",
                    "start_byte": 0,
                    "end_byte": 0,
                    "replacement": "a",
                    "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                },
                {
                    "path": "src/lib.rs",
                    "start_byte": 0,
                    "end_byte": 0,
                    "replacement": "b",
                    "base_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            ]
        }),
    );

    let calls = apply_edits.calls();
    assert_eq!(calls.len(), 1);
    let hashes: Vec<_> = calls[0]
        .edits
        .iter()
        .map(|edit| edit.base_hash.as_deref())
        .collect();
    assert_eq!(
        hashes,
        vec![
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        ]
    );
}

#[test]
fn engine_apply_edits_host_returns_one_per_file_result_for_every_targeted_file() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let src_dir = workspace.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("a.txt"), b"alpha\n").expect("write source file");
    let extensions_dir = workspace.path().join("runtime_extensions");
    let first_file_version = compute_content_version(b"alpha\n");
    write_batch_apply_edits_extension(&extensions_dir, &first_file_version);

    let mut config = TowerConfig::default();
    config.extensions.request_timeout_secs = 2;
    let handle = build_engine(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: Some(extensions_dir),
        },
        config,
    )
    .expect("build engine with batch apply-edits extension");
    let mut registry = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    let result = registry
        .call("tower_ast_replace", json!({}))
        .expect("daemon apply-edits host dependency must return a tool result");
    let per_file = result["host_response"]["result"]["per_file"]
        .as_array()
        .unwrap_or_else(|| panic!("host response must include per-file results: {result}"));

    assert_eq!(
        per_file.len(),
        2,
        "one PerFileEditResult must be returned for every targeted file, including failures"
    );
    assert_eq!(per_file[0]["path"], "src/a.txt");
    assert_eq!(per_file[1]["path"], "src/missing.txt");
    assert!(
        per_file[1]["error"].is_object(),
        "failed targeted files must be represented as per-file errors: {result}"
    );
}

#[test]
fn stale_cas_maps_to_workspace_apply_edits_conflict_serialized_as_cas_conflict() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![failed_file(
            "src/main.rs",
            WorkspaceApplyEditsErrorCode::Conflict,
            "base_hash does not match current version",
        )],
    )));
    let deps = make_deps_with_apply_edits(apply_edits);

    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [{
                "path": "src/main.rs",
                "start_byte": 0,
                "end_byte": 0,
                "replacement": "x",
                "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }),
    );

    assert_eq!(
        response["result"]["per_file"][0]["error"]["code"],
        "cas_conflict"
    );
}

#[test]
fn stale_cas_surfaced_as_hostcall_transport_error_remains_precondition_failed() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::version_conflict());
    let deps = make_deps_with_apply_edits(apply_edits);

    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [{
                "path": "src/main.rs",
                "start_byte": 0,
                "end_byte": 0,
                "replacement": "x",
                "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }),
    );

    assert_eq!(response["error"]["code"], -32009);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("precondition_failed")
    );
}

#[test]
fn successful_file_mutations_fan_out_filechanged_callbacks_only_after_locks_are_released() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let src_dir = workspace.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("lint_target.txt"), b"alpha beta\n").expect("write source file");
    let first_expected_version = compute_content_version(b"alpha beta\n");
    let second_expected_version = compute_content_version(b"alpha fixed\n");
    let extensions_dir = workspace.path().join("runtime_extensions");
    write_reentrant_file_changed_apply_edits_extension(
        &extensions_dir,
        &first_expected_version,
        &second_expected_version,
    );

    let mut config = TowerConfig::default();
    config.extensions.request_timeout_secs = 2;
    let handle = build_engine(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: Some(extensions_dir),
        },
        config,
    )
    .expect("build engine with reentrant apply-edits extension");
    let mut registry = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    registry
        .call("tower_ast_replace", json!({}))
        .expect("fileChanged-triggered apply-edits HostCall must not deadlock original apply");
    let read = registry
        .call("tower_read_file", json!({ "path": "src/lint_target.txt" }))
        .expect("read twice-mutated file through shared engine state");

    assert_eq!(
        read["content"], "omega fixed\n",
        "fileChanged callback must run after mutation locks are released so reentrant edits can complete"
    );
}

#[test]
fn workspace_apply_edits_dry_run_returns_preview_per_file_data_and_performs_zero_writes_and_zero_filechanged_fanout()
 {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(
        WorkspaceApplyEditsResult {
            files_changed: 0,
            per_file: vec![applied_file("src/main.rs", Some("fn preview() {}\n"))],
        },
    ));
    let deps = make_deps_with_apply_edits(apply_edits.clone());

    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [{
                "path": "src/main.rs",
                "start_byte": 0,
                "end_byte": 0,
                "replacement": "fn preview() {}\n"
            }],
            "dry_run": true
        }),
    );

    assert_eq!(
        response["result"]["per_file"][0]["preview"],
        "fn preview() {}\n"
    );
    assert_eq!(apply_edits.calls()[0].dry_run, Some(true));
    assert_eq!(response["result"]["files_changed"], 0);
    assert_eq!(
        apply_edits.write_attempts(),
        0,
        "dry-run workspace/applyEdits must not attempt writes"
    );
    assert_eq!(
        apply_edits.file_changed_fanout_attempts(),
        0,
        "dry-run workspace/applyEdits must not fan out fileChanged callbacks"
    );
}

#[test]
fn adapter_tests_cover_partial_multi_file_reporting_with_a_behavioral_fake_mutation_host() {
    let expected = apply_edits_result(vec![
        applied_file("src/a.rs", None),
        failed_file(
            "src/b.rs",
            WorkspaceApplyEditsErrorCode::Conflict,
            "base_hash does not match current version",
        ),
    ]);
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(expected));
    let deps = make_deps_with_apply_edits(apply_edits);

    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": [
                {"path": "src/a.rs", "start_byte": 0, "end_byte": 0, "replacement": "a", "base_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                {"path": "src/b.rs", "start_byte": 0, "end_byte": 0, "replacement": "b", "base_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
            ]
        }),
    );

    assert_eq!(response["result"]["per_file"].as_array().unwrap().len(), 2);
    assert_eq!(response["result"]["per_file"][0]["path"], "src/a.rs");
    assert_eq!(response["result"]["per_file"][1]["path"], "src/b.rs");
    assert_eq!(
        response["result"]["per_file"][1]["error"]["code"],
        "cas_conflict"
    );
}

#[test]
fn adapter_tests_cover_empty_edit_list_as_a_batch_result_error() {
    let apply_edits = Arc::new(RecordingBatchApplyEditsHost::returning(apply_edits_result(
        vec![failed_file(
            "",
            WorkspaceApplyEditsErrorCode::EmptyEdits,
            "workspace/applyEdits requires at least one edit",
        )],
    )));
    let deps = make_deps_with_apply_edits(apply_edits);

    let response = invoke_apply_edits_host_call(
        vec!["request_apply_edits".to_owned()],
        deps,
        json!({
            "edits": []
        }),
    );

    assert_eq!(
        response["result"]["per_file"][0]["error"]["code"],
        "empty_edit_list"
    );
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
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT, None).expect("spawn must succeed");

    let event = extension_protocol::Event::FileIndexed {
        file_id: 42,
        path: "src/lib.rs".to_owned(),
    };
    instance
        .deliver_event(event)
        .expect("deliver_event must return Ok");
    instance.shutdown();
}
