#![allow(clippy::pedantic)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

mod lint_support;

use core_engine::adapters::extension::{SidecarHostAdapter, load_extensions_into_registry};
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::native_tools::{
    EXPECTED_NATIVE_TOOL_NAMES, EngineState, NativeToolRegistry,
};
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::{InMemoryStorage, RealFs};
use core_engine::domain::RelativePath;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::FileSystemPort;
use extension_protocol::{ExtensionManifest, PROTOCOL_VERSION};
use lint_support::{
    TestWorkspace, host_deps, lint_empty_manifest, lint_extension_bin, workspace_root,
};
use serde_json::{Value, json};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

fn shipped_lint_manifest() -> ExtensionManifest {
    let path = workspace_root().join("extensions/lint/extension.toml");
    let contents = fs::read_to_string(&path).expect("read shipped lint extension manifest");
    toml::from_str(&contents).expect("shipped lint extension manifest must parse")
}

fn workspace_cargo_toml() -> toml::Value {
    let path = workspace_root().join("Cargo.toml");
    let contents = fs::read_to_string(&path).expect("read workspace Cargo.toml");
    toml::from_str(&contents).expect("workspace Cargo.toml must parse")
}

fn toml_string_array<'a>(value: &'a toml::Value, key: &str) -> Vec<&'a str> {
    value
        .get("workspace")
        .and_then(|workspace| workspace.get(key))
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("workspace.{key} must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("workspace.{key} entries must be strings"))
        })
        .collect()
}

fn empty_engine_state(workspace: &TestWorkspace) -> Arc<std::sync::RwLock<EngineState>> {
    Arc::new(std::sync::RwLock::new(EngineState::new(
        ProjectWorkspace::new(),
        InvertedIndex::new(),
        Box::new(InMemoryStorage::new()),
        Box::new(workspace.real_fs()),
    )))
}

fn spawn_adapter(workspace: &TestWorkspace) -> Box<dyn core_engine::domain::ExtensionInstance> {
    SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(workspace.real_fs()),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn")
}

fn write_file(fs: &mut RealFs, path: &str, content: &[u8]) {
    fs.write(RelativePath::new(path), content.to_vec())
        .expect("write workspace file");
}

fn assert_success_check_result(value: &Value) {
    assert!(
        value.get("error").is_none(),
        "stable lint tool failures must not appear for this case; got: {value}"
    );
}

#[test]
fn workspace_cargo_toml_includes_extensions_lint_as_workspace_member_and_default_member_consistent_with_other_sidecar_extensions()
 {
    let cargo = workspace_cargo_toml();
    let members = toml_string_array(&cargo, "members");
    let default_members = toml_string_array(&cargo, "default-members");

    assert!(
        members.contains(&"extensions/lint"),
        "workspace members must include extensions/lint; got {members:?}"
    );
    assert!(
        default_members.contains(&"extensions/lint"),
        "workspace default-members must include extensions/lint so cargo build --workspace --bins builds lint_extension; got {default_members:?}"
    );
}

#[test]
fn extensions_lint_extension_toml_declares_extension_name_lint_and_binary_lint_extension() {
    let manifest = shipped_lint_manifest();

    assert_eq!(manifest.name, "lint");
    assert_eq!(manifest.command, vec!["lint_extension"]);
}

#[test]
fn lint_manifest_declares_local_tool_check_with_valid_input_schema_accepting_optional_path() {
    let manifest = shipped_lint_manifest();
    let tool = manifest
        .tools
        .iter()
        .find(|tool| tool.name == "check")
        .expect("lint manifest must declare local tool check");
    let schema: Value =
        serde_json::from_str(&tool.schema_json).expect("check tool schema_json must be valid JSON");

    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["path"]["type"], "string");
    assert!(
        schema
            .get("required")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty),
        "path must be optional, so required must be absent or empty; got {schema}"
    );
}

#[test]
fn lint_manifest_declares_read_file_list_files_and_log_capabilities_only() {
    let manifest = shipped_lint_manifest();

    assert_eq!(
        manifest.capabilities.required,
        vec!["read_file", "list_files", "log"],
        "lint must request only read/list/log host capabilities"
    );
}

#[test]
fn extension_tool_appears_in_list_with_tower_prefix() {
    let workspace = TestWorkspace::new();
    let disabled = vec![
        "ast".to_owned(),
        "fmt".to_owned(),
        "hello".to_owned(),
        "lsp".to_owned(),
    ];
    let extension_registry = load_extensions_into_registry(
        &[workspace_root().join("extensions")],
        host_deps(workspace.real_fs()),
        TEST_TIMEOUT,
        &disabled,
    );
    let merged = ExtensionMergedRegistry::new(
        empty_engine_state(&workspace),
        Arc::new(std::sync::RwLock::new(extension_registry)),
    );

    let tools = merged.list();
    let lint_tool = tools
        .iter()
        .find(|tool| tool.name == "tower_lint_check")
        .expect("tools/list through merged registry must include tower_lint_check");

    assert_eq!(lint_tool.input_schema["type"], "object");
    assert_eq!(
        lint_tool.input_schema["properties"]["path"]["type"],
        "string"
    );
}

#[test]
fn no_native_tool_registry_entry_named_tower_lint_check_is_added() {
    let workspace = TestWorkspace::new();
    let native = NativeToolRegistry::new(empty_engine_state(&workspace));
    let names = native
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();

    assert!(
        !EXPECTED_NATIVE_TOOL_NAMES.contains(&"tower_lint_check"),
        "tower_lint_check must not be added to the canonical native tool list"
    );
    assert!(
        !names.iter().any(|name| name == "tower_lint_check"),
        "tower_lint_check must come from extension discovery, not NativeToolRegistry; got {names:?}"
    );
}

#[test]
fn main_rs_initializes_the_extension_and_declares_exactly_one_tool_named_check() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("ok-lint.sh", "#!/bin/sh\nexit 0\n");
    workspace.write_lint_config(&script);

    let mut adapter = spawn_adapter(&workspace);

    let manifest = adapter.manifest();
    let tool_names = manifest
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["check"]);
    adapter.shutdown();
}

#[test]
fn the_check_tool_accepts_optional_path_and_with_a_path_lints_one_workspace_relative_file() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("one-file-lint.sh", "#!/bin/sh\nexit 0\n");
    workspace.write_lint_config(&script);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "src/main.txt", b"plain text");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({ "path": "src/main.txt" }))
        .expect("check must return a tool result");

    adapter.shutdown();

    assert_eq!(result["supported"], true);
    assert_eq!(result["diagnostics"], json!([]));
    assert_success_check_result(&result);
}

#[test]
fn calling_check_without_path_uses_workspace_list_files_and_sorts_workspace_diagnostics() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "sorted-lint.sh",
        r#"#!/bin/sh
case "$1" in
  "b.txt") printf 'b.txt:9:4: zebra\n' ;;
  "a.txt") printf 'a.txt:1:2: apple\n' ;;
esac
"#,
    );
    workspace.write_lint_config(&script);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "b.txt", b"b");
    write_file(&mut fs, "a.txt", b"a");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({}))
        .expect("check-all must return a tool result");

    adapter.shutdown();

    assert_eq!(result["supported"], true);
    let diagnostics = result["diagnostics"]
        .as_array()
        .expect("diagnostics must be an array");
    let ordering = diagnostics
        .iter()
        .map(|diagnostic| {
            (
                diagnostic["path"].as_str().expect("path"),
                diagnostic["line"].as_u64().expect("line"),
                diagnostic["character"].as_u64().expect("character"),
                diagnostic["message"].as_str().expect("message"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ordering,
        vec![("a.txt", 0, 1, "apple"), ("b.txt", 8, 3, "zebra")]
    );
    assert_success_check_result(&result);
}

#[test]
fn unsupported_file_extensions_return_supported_false_diagnostics_empty_and_no_protocol_error() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("txt-lint.sh", "#!/bin/sh\nexit 0\n");
    workspace.write_lint_config_for_extensions(&script, &["txt"]);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "README.md", b"# Readme");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({ "path": "README.md" }))
        .expect("unsupported path must still be a successful tool result");

    adapter.shutdown();

    assert_eq!(result, json!({ "supported": false, "diagnostics": [] }));
}

#[test]
fn main_rs_returns_runner_failures_as_tool_result_error_with_stable_missing_binary_code() {
    let workspace = TestWorkspace::new();
    let missing = workspace.root().join("definitely-missing-lint-command");
    workspace.write_lint_config(&missing);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "src/main.txt", b"plain text");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({ "path": "src/main.txt" }))
        .expect("missing binary must remain on the success-result path");

    adapter.shutdown();

    assert_eq!(result["supported"], false);
    assert_eq!(result["diagnostics"], json!([]));
    assert_eq!(result["error"]["code"], "lint_missing_binary");
}

#[test]
fn main_rs_returns_runner_failures_as_tool_result_error_with_stable_invalid_config_code() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("unused-lint.sh", "#!/bin/sh\nexit 0\n");
    workspace.write_invalid_generic_lint_config_without_regex(&script);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "src/main.txt", b"plain text");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({ "path": "src/main.txt" }))
        .expect("invalid config must remain on the success-result path");

    adapter.shutdown();

    assert_eq!(result["supported"], false);
    assert_eq!(result["diagnostics"], json!([]));
    assert_eq!(result["error"]["code"], "lint_invalid_config");
}

#[test]
fn unparseable_nonzero_output_preserves_success_result_path_with_stable_lint_error_code() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "bad-output-lint.sh",
        "#!/bin/sh\nprintf 'this is not a diagnostic\\n'\nexit 2\n",
    );
    workspace.write_lint_config(&script);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "src/main.txt", b"plain text");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({ "path": "src/main.txt" }))
        .expect("nonzero parser failure must remain on the success-result path");

    adapter.shutdown();

    assert_eq!(result["supported"], false);
    assert_eq!(result["diagnostics"], json!([]));
    assert_eq!(result["error"]["code"], "lint_nonzero_exit");
}

#[test]
fn unparseable_successful_output_preserves_success_result_path_with_stable_lint_error_code() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "successful-bad-output-lint.sh",
        "#!/bin/sh\nprintf '{not json\\n'\nexit 0\n",
    );
    let tower_dir = workspace.root().join(".tower");
    fs::create_dir_all(&tower_dir).expect("create .tower");
    let command = script.to_string_lossy().replace('\\', "\\\\");
    fs::write(
        tower_dir.join("config.toml"),
        format!(
            r#"
[lint.fixture]
command = "{command}"
extensions = ["txt"]
format = "rustc-json"
target = "append"
source = "fixture-lint"
"#
        ),
    )
    .expect("write rustc-json lint config");
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "src/main.txt", b"plain text");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({ "path": "src/main.txt" }))
        .expect("successful parser failure must remain on the success-result path");

    adapter.shutdown();

    assert_eq!(result["supported"], false);
    assert_eq!(result["diagnostics"], json!([]));
    assert_eq!(result["error"]["code"], "lint_unparseable_output");
}

#[test]
fn hanging_linter_preserves_success_result_path_with_stable_timeout_code() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("hanging-lint.sh", "#!/bin/sh\nsleep 60\n");
    workspace.write_lint_config(&script);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "src/main.txt", b"plain text");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({ "path": "src/main.txt" }))
        .expect("timeout must remain on the success-result path");

    adapter.shutdown();

    assert_eq!(result["supported"], false);
    assert_eq!(result["diagnostics"], json!([]));
    assert_eq!(result["error"]["code"], "lint_timeout");
}

#[test]
fn workspace_check_propagates_per_file_runner_error_with_stable_lint_error_code() {
    let workspace = TestWorkspace::new();
    let missing = workspace
        .root()
        .join("definitely-missing-workspace-lint-command");
    workspace.write_lint_config(&missing);
    let mut fs = workspace.real_fs();
    write_file(&mut fs, "src/main.txt", b"plain text");

    let mut adapter = SidecarHostAdapter::spawn(
        lint_empty_manifest(&lint_extension_bin()),
        host_deps(fs),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn");
    let result = adapter
        .call_tool("check", json!({}))
        .expect("workspace runner failure must remain on the success-result path");

    adapter.shutdown();

    assert_eq!(result["supported"], false);
    assert_eq!(result["diagnostics"], json!([]));
    assert_eq!(result["error"]["code"], "lint_missing_binary");
}

struct ProtocolChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ProtocolChild {
    fn spawn(workspace: &TestWorkspace) -> Self {
        let mut child = Command::new(lint_extension_bin())
            .current_dir(workspace.root())
            .env("TOWER_WORKSPACE", workspace.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn lint extension");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn write_frame(&mut self, frame: Value) {
        let line = serde_json::to_string(&frame).expect("serialize frame");
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read frame");
        assert!(
            !line.is_empty(),
            "child stdout closed before a frame arrived"
        );
        serde_json::from_str(line.trim()).expect("valid JSON frame")
    }

    fn initialize(&mut self) {
        self.write_frame(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocol_version": PROTOCOL_VERSION,
                "client_info": "lint-e2e/0.1.0"
            }
        }));
        let response = self.read_frame();
        assert!(
            response.get("result").is_some(),
            "initialize must succeed; got: {response}"
        );
    }
}

impl Drop for ProtocolChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn queued_frame_replay_preserves_invoke_tool_and_shutdown_fifo_order_after_hostcall_response() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("empty-lint.sh", "#!/bin/sh\nexit 0\n");
    workspace.write_lint_config(&script);

    let mut child = ProtocolChild::spawn(&workspace);
    child.initialize();

    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": { "name": "check", "params": {} }
    }));
    let host_call = child.read_frame();
    assert_eq!(host_call["method"], "workspace/listFiles");
    let host_call_id = host_call["id"].clone();

    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "invokeTool",
        "params": { "name": "check", "params": {} }
    }));
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "shutdown",
        "params": null
    }));
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": host_call_id,
        "result": []
    }));

    let check_response = child.read_frame();
    assert_eq!(
        check_response["id"], 1,
        "invokeTool response must be replayed before queued shutdown: {check_response}"
    );
    assert!(check_response.get("result").is_some());

    let queued_host_call = child.read_frame();
    assert_eq!(
        queued_host_call["method"], "workspace/listFiles",
        "queued invokeTool must be replayed before queued shutdown: {queued_host_call}"
    );
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": queued_host_call["id"].clone(),
        "result": []
    }));

    let queued_check_response = child.read_frame();
    assert_eq!(
        queued_check_response["id"], 2,
        "queued invokeTool response must retain original order before shutdown: {queued_check_response}"
    );
    assert!(queued_check_response.get("result").is_some());

    let shutdown_response = child.read_frame();
    assert_eq!(
        shutdown_response["id"], 3,
        "queued shutdown must retain original order: {shutdown_response}"
    );
    assert!(shutdown_response.get("result").is_some());
}

#[test]
fn twenty_way_parallel_e2e_stress_covers_initialize_check_and_shutdown_without_hostcall_deadlock() {
    const N: usize = 20;

    let handles = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let workspace = TestWorkspace::new();
                let script = workspace.script("empty-lint.sh", "#!/bin/sh\nexit 0\n");
                workspace.write_lint_config(&script);

                let mut child = ProtocolChild::spawn(&workspace);
                child.initialize();
                child.write_frame(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "invokeTool",
                    "params": { "name": "check", "params": {} }
                }));

                let host_call = child.read_frame();
                assert_eq!(
                    host_call["method"], "workspace/listFiles",
                    "#{i}: {host_call}"
                );
                child.write_frame(json!({
                    "jsonrpc": "2.0",
                    "id": host_call["id"].clone(),
                    "result": []
                }));

                let check_response = child.read_frame();
                assert!(
                    check_response.get("result").is_some(),
                    "#{i}: check must succeed; got: {check_response}"
                );

                child.write_frame(json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "shutdown"
                }));
                let shutdown_response = child.read_frame();
                assert!(
                    shutdown_response.get("result").is_some(),
                    "#{i}: shutdown must succeed; got: {shutdown_response}"
                );
            })
        })
        .collect::<Vec<_>>();

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("stress worker #{i} panicked"));
    }
}
