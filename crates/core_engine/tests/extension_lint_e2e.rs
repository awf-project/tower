#![allow(clippy::pedantic)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

mod lint_support;

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::TowerConfig;
use core_engine::adapters::daemon::engine::build_engine;
use core_engine::adapters::extension::{SidecarHostAdapter, load_extensions_into_registry};
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::native_tools::{
    EXPECTED_NATIVE_TOOL_NAMES, EngineState, NativeToolRegistry,
};
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::{InMemoryStorage, RealFs};
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::mutation::compute_content_version;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::domain::{DomainError, RelativePath};
use core_engine::ports::FileSystemPort;
use core_engine::ports::inbound::{ApplyEditsRequest, TextEdit};
use extension_protocol::{ExtensionManifest, PROTOCOL_VERSION};
use lint_support::{
    SEVERITY_CODE_GENERIC_REGEX, TestWorkspace, host_deps, lint_empty_manifest, lint_extension_bin,
    lint_fix_manifest, workspace_root,
};
use serde_json::{Value, json};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

#[path = "../../../extensions/lint/src/protocol.rs"]
mod lint_protocol;

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

fn write_file(fs: &mut RealFs, path: &str, content: &[u8]) {
    fs.write(RelativePath::new(path), content.to_vec())
        .expect("write workspace file");
}

fn write_apply_edits_extension(workspace: &TestWorkspace, expected_version: &str) {
    let ext_dir = workspace.root().join("runtime_extensions/apply");
    fs::create_dir_all(&ext_dir).expect("create apply extension dir");
    fs::write(
        ext_dir.join("extension.toml"),
        r#"
name = "apply"
version = "0.1.0"
command = ["./apply_host.sh"]
activation = "lazy"

[[tools]]
name = "replace"
description = "Request one workspace/applyEdits host call."
schema_json = "{}"

[capabilities]
required = ["request_apply_edits"]
"#,
    )
    .expect("write apply extension manifest");

    let script = format!(
        r#"#!/bin/sh
IFS= read -r _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"type":"Initialized","data":{{"tools":[{{"name":"replace","description":"Request one workspace/applyEdits host call.","schema_json":"{{}}"}}],"events":[],"capabilities":["request_apply_edits"]}}}}}}'
IFS= read -r _invoke
printf '%s\n' '{{"jsonrpc":"2.0","id":99,"method":"workspace/applyEdits","params":{{"path":"src/lint_target.txt","expected_version":"{expected_version}","edits":[{{"start_byte":6,"end_byte":10,"replacement":"fixed"}}],"dry_run":false}}}}'
IFS= read -r host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"type":"ToolResult","data":{{"host_response":'"$host_response"'}}}}}}'
IFS= read -r _shutdown
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"type":"Ack"}}}}'
"#
    );
    let script_path = ext_dir.join("apply_host.sh");
    fs::write(&script_path, script).expect("write apply extension script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&script_path)
            .expect("apply extension script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod apply extension script");
    }
}

fn write_subscribed_apply_edits_extension(workspace: &TestWorkspace, expected_version: &str) {
    let ext_dir = workspace.root().join("runtime_extensions/apply_subscribed");
    fs::create_dir_all(&ext_dir).expect("create subscribed apply extension dir");
    fs::write(
        ext_dir.join("extension.toml"),
        r#"
name = "apply_subscribed"
version = "0.1.0"
command = ["./apply_host.sh"]
activation = "eager"

[[tools]]
name = "replace"
description = "Request one workspace/applyEdits host call while subscribed to fileChanged."
schema_json = "{}"

[events]
subscribe = ["event/fileChanged"]

[capabilities]
required = ["request_apply_edits"]
"#,
    )
    .expect("write subscribed apply extension manifest");

    let script = format!(
        r#"#!/bin/sh
IFS= read -r _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"type":"Initialized","data":{{"tools":[{{"name":"replace","description":"Request one workspace/applyEdits host call while subscribed to fileChanged.","schema_json":"{{}}"}}],"events":["event/fileChanged"],"capabilities":["request_apply_edits"]}}}}}}'
IFS= read -r _invoke
printf '%s\n' '{{"jsonrpc":"2.0","id":99,"method":"workspace/applyEdits","params":{{"path":"src/lint_target.txt","expected_version":"{expected_version}","edits":[{{"start_byte":6,"end_byte":10,"replacement":"fixed"}}],"dry_run":false}}}}'
IFS= read -r host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"type":"ToolResult","data":{{"host_response":'"$host_response"'}}}}}}'
IFS= read -r _event
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"type":"Ack"}}}}'
IFS= read -r _shutdown
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"type":"Ack"}}}}'
"#
    );
    let script_path = ext_dir.join("apply_host.sh");
    fs::write(&script_path, script).expect("write subscribed apply extension script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&script_path)
            .expect("subscribed apply extension script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)
            .expect("chmod subscribed apply extension script");
    }
}

fn write_reentrant_file_changed_apply_edits_extension(
    workspace: &TestWorkspace,
    first_expected_version: &str,
    second_expected_version: &str,
) {
    let ext_dir = workspace.root().join("runtime_extensions/apply_reentrant");
    fs::create_dir_all(&ext_dir).expect("create reentrant apply extension dir");
    fs::write(
        ext_dir.join("extension.toml"),
        r#"
name = "apply_reentrant"
version = "0.1.0"
command = ["./apply_host.sh"]
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
    .expect("write reentrant apply extension manifest");

    let script = format!(
        r#"#!/bin/sh
IFS= read -r _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"type":"Initialized","data":{{"tools":[{{"name":"replace","description":"Request workspace/applyEdits and request another edit from fileChanged.","schema_json":"{{}}"}}],"events":["event/fileChanged"],"capabilities":["request_apply_edits"]}}}}}}'
IFS= read -r _invoke
printf '%s\n' '{{"jsonrpc":"2.0","id":99,"method":"workspace/applyEdits","params":{{"path":"src/lint_target.txt","expected_version":"{first_expected_version}","edits":[{{"start_byte":6,"end_byte":10,"replacement":"fixed"}}],"dry_run":false}}}}'
IFS= read -r _first_host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"type":"ToolResult","data":{{"ok":true}}}}}}'
IFS= read -r _event
printf '%s\n' '{{"jsonrpc":"2.0","id":100,"method":"workspace/applyEdits","params":{{"path":"src/lint_target.txt","expected_version":"{second_expected_version}","edits":[{{"start_byte":0,"end_byte":5,"replacement":"omega"}}],"dry_run":false}}}}'
IFS= read -r _second_host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"type":"Ack"}}}}'
IFS= read -r _shutdown
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"type":"Ack"}}}}'
"#
    );
    let script_path = ext_dir.join("apply_host.sh");
    fs::write(&script_path, script).expect("write reentrant apply extension script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&script_path)
            .expect("reentrant apply extension script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions)
            .expect("chmod reentrant apply extension script");
    }
}

fn write_eager_observer_and_initialize_apply_extensions(
    workspace: &TestWorkspace,
    expected_version: &str,
) {
    let observer_dir = workspace.root().join("runtime_extensions/a_observer");
    fs::create_dir_all(&observer_dir).expect("create observer extension dir");
    fs::write(
        observer_dir.join("extension.toml"),
        r#"
name = "observer"
version = "0.1.0"
command = ["./observer.sh"]
activation = "eager"

[events]
subscribe = ["event/fileChanged"]
"#,
    )
    .expect("write observer manifest");
    let observer_script = workspace
        .root()
        .join("runtime_extensions/a_observer/observer.sh");
    fs::write(
        &observer_script,
        r#"#!/bin/sh
IFS= read -r _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"type":"Initialized","data":{"tools":[],"events":["event/fileChanged"],"capabilities":[]}}}'
IFS= read -r event
printf '%s\n' "$event" > observed_event.json
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"type":"Ack"}}'
IFS= read -r _shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"type":"Ack"}}'
"#,
    )
    .expect("write observer script");

    let apply_dir = workspace.root().join("runtime_extensions/b_apply");
    fs::create_dir_all(&apply_dir).expect("create eager apply extension dir");
    fs::write(
        apply_dir.join("extension.toml"),
        r#"
name = "apply"
version = "0.1.0"
command = ["./apply_init.sh"]
activation = "eager"

[capabilities]
required = ["request_apply_edits"]
"#,
    )
    .expect("write eager apply manifest");
    let apply_script = format!(
        r#"#!/bin/sh
IFS= read -r _initialize
printf '%s\n' '{{"jsonrpc":"2.0","id":99,"method":"workspace/applyEdits","params":{{"path":"src/lint_target.txt","expected_version":"{expected_version}","edits":[{{"start_byte":6,"end_byte":10,"replacement":"fixed"}}],"dry_run":false}}}}'
IFS= read -r _host_response
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"type":"Initialized","data":{{"tools":[],"events":[],"capabilities":["request_apply_edits"]}}}}}}'
IFS= read -r _shutdown
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"type":"Ack"}}}}'
"#
    );
    let apply_script_path = apply_dir.join("apply_init.sh");
    fs::write(&apply_script_path, apply_script).expect("write eager apply script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for path in [&observer_script, &apply_script_path] {
            let mut permissions = fs::metadata(path)
                .expect("extension script metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("chmod extension script");
        }
    }
}

fn assert_success_check_result(value: &Value) {
    assert!(
        value.get("error").is_none(),
        "stable lint tool failures must not appear for this case; got: {value}"
    );
}

fn rustc_fix_script(
    workspace: &TestWorkspace,
    name: &str,
    applicability: &str,
) -> std::path::PathBuf {
    workspace.script(
        name,
        &format!(
            r#"#!/bin/sh
printf '%s\n' '{{"reason":"compiler-message","message":{{"message":"replace nope","level":"warning","code":{{"code":"fixture::replace"}},"spans":[{{"file_name":"src/main.rs","is_primary":true,"line_start":1,"column_start":1,"line_end":1,"column_end":5,"byte_start":0,"byte_end":4,"suggested_replacement":"yep","applicability":"{applicability}"}}]}}}}'
"#
        ),
    )
}

fn rustc_message(message: &str, code: &str, spans_json: &str) -> String {
    format!(
        r#"{{"reason":"compiler-message","message":{{"message":"{message}","level":"warning","code":{{"code":"{code}"}},"spans":{spans_json}}}}}"#
    )
}

#[allow(clippy::too_many_arguments)]
fn rustc_span(
    path: &str,
    line_start: u32,
    column_start: u32,
    line_end: u32,
    column_end: u32,
    byte_start: usize,
    byte_end: usize,
    replacement: &str,
) -> String {
    format!(
        r#"{{"file_name":"{path}","is_primary":true,"line_start":{line_start},"column_start":{column_start},"line_end":{line_end},"column_end":{column_end},"byte_start":{byte_start},"byte_end":{byte_end},"suggested_replacement":"{replacement}","applicability":"MachineApplicable"}}"#
    )
}

fn rustc_fix_case_script(
    workspace: &TestWorkspace,
    name: &str,
    cases: &[(&str, Vec<String>)],
) -> std::path::PathBuf {
    let mut body = String::from("#!/bin/sh\ncase \"$1\" in\n");
    for (path, messages) in cases {
        body.push_str(&format!("  \"{path}\")\n"));
        for message in messages {
            body.push_str(&format!("    printf '%s\\n' '{message}'\n"));
        }
        body.push_str("    ;;\n");
    }
    body.push_str("esac\n");
    workspace.script(name, &body)
}

fn spawn_lint_fix_adapter(
    workspace: &TestWorkspace,
) -> Box<dyn core_engine::domain::ExtensionInstance> {
    SidecarHostAdapter::spawn(
        lint_fix_manifest(&lint_extension_bin()),
        host_deps(workspace.real_fs()),
        TEST_TIMEOUT,
    )
    .expect("lint extension must spawn")
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
fn lint_fix_manifest_declares_local_tool_fix_and_request_apply_edits_capability() {
    let manifest = shipped_lint_manifest();
    let tool = manifest
        .tools
        .iter()
        .find(|tool| tool.name == "fix")
        .expect("lint manifest must declare local tool fix");
    let schema: Value =
        serde_json::from_str(&tool.schema_json).expect("fix tool schema_json must be valid JSON");

    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["path"]["type"], "string");
    assert_eq!(schema["properties"]["unsafe"]["type"], "boolean");
    assert_eq!(schema["properties"]["dry_run"]["type"], "boolean");
    assert!(
        manifest
            .capabilities
            .required
            .iter()
            .any(|capability| capability == "request_apply_edits"),
        "fix requires request_apply_edits in the shipped manifest"
    );
}

#[test]
fn lint_manifest_declares_read_file_list_files_log_and_request_apply_edits_capabilities() {
    let manifest = shipped_lint_manifest();

    assert_eq!(
        manifest.capabilities.required,
        vec!["read_file", "list_files", "log", "request_apply_edits"],
        "lint must request read/list/log plus request_apply_edits for future fix orchestration"
    );
}

#[test]
fn lint_fix_protocol_dtos_serialize_stable_request_result_skip_preview_and_error_fields() {
    let request: lint_protocol::FixRequest = serde_json::from_value(json!({
        "path": "src/main.rs",
        "unsafe": true,
        "dry_run": true
    }))
    .expect("FixRequest must accept external unsafe field");
    assert_eq!(request.path.as_deref(), Some("src/main.rs"));
    assert!(request.unsafe_fixes);
    assert!(request.dry_run);

    let result = lint_protocol::FixResult {
        files_changed: 1,
        fixes_applied: 1,
        fixes_skipped: vec![lint_protocol::SkippedFixDto {
            path: "src/main.rs".to_owned(),
            reason: lint_protocol::SkippedFixReason::CasConflict,
            diagnostic: None,
            supported_fix: true,
        }],
        remaining_diagnostics: Vec::new(),
        previews: vec![lint_protocol::FixPreviewDto {
            path: "src/main.rs".to_owned(),
            edits: vec![lint_protocol::FixPreviewEditDto {
                start_byte: 0,
                end_byte: 4,
                replacement: "yep".to_owned(),
            }],
            preview_content: "yep\n".to_owned(),
        }],
    };

    assert_eq!(
        serde_json::to_value(result).expect("serialize FixResult"),
        json!({
            "files_changed": 1,
            "fixes_applied": 1,
            "fixes_skipped": [{
                "path": "src/main.rs",
                "reason": "cas_conflict",
                "diagnostic": null,
                "supported_fix": true
            }],
            "remaining_diagnostics": [],
            "previews": [{
                "path": "src/main.rs",
                "edits": [{
                    "start_byte": 0,
                    "end_byte": 4,
                    "replacement": "yep"
                }],
                "preview_content": "yep\n"
            }]
        })
    );

    for reason in [
        lint_protocol::SkippedFixReason::Conflict,
        lint_protocol::SkippedFixReason::Unsafe,
        lint_protocol::SkippedFixReason::Unsupported,
        lint_protocol::SkippedFixReason::CasConflict,
        lint_protocol::SkippedFixReason::InvalidRange,
    ] {
        let value = serde_json::to_value(reason).expect("serialize skipped reason");
        assert!(matches!(
            value.as_str(),
            Some("conflict" | "unsafe" | "unsupported" | "cas_conflict" | "invalid_range")
        ));
    }

    let error_codes = [
        "lint_fix_unavailable",
        "lint_fix_apply_failed",
        "lint_fix_invalid_request",
    ];
    for code in error_codes {
        let error = lint_protocol::FixToolErrorResponse {
            code: code.to_owned(),
            message: "stable error".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(error).expect("serialize FixToolErrorResponse")["code"],
            code
        );
    }
}

#[test]
fn lint_support_rs_can_build_lint_extension_fixtures_with_the_new_apply_edits_dependency_available()
{
    let workspace = TestWorkspace::new();
    let deps = host_deps(workspace.real_fs());

    let result = deps.apply_edits.apply_edits_dry_run(ApplyEditsRequest {
        path: RelativePath::new("src/main.txt"),
        expected_version: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_owned(),
        edits: vec![TextEdit {
            start_byte: 0,
            end_byte: 0,
            replacement: "fixed".to_owned(),
        }],
    });

    assert!(
        !matches!(result, Err(DomainError::UnsupportedOperation(_))),
        "lint_support host_deps must provide an apply-edits-capable fixture dependency"
    );
}

#[test]
fn lint_manifest_declares_focused_verification_filter_covers_the_lint_manifest_contract() {
    let manifest = shipped_lint_manifest();
    let check_tool = manifest
        .tools
        .iter()
        .find(|tool| tool.name == "check")
        .expect("focused lint_manifest_declares filter must cover the check tool contract");

    assert_eq!(manifest.name, "lint");
    assert_eq!(check_tool.name, "check");
    assert_eq!(
        manifest.capabilities.required,
        vec!["read_file", "list_files", "log", "request_apply_edits"],
        "focused lint_manifest_declares verification must exercise the required capability contract"
    );
}

#[test]
fn runtime_test_proves_apply_edits_writes_are_visible_through_an_indexed_read_search_path_after_mutation()
 {
    let workspace = TestWorkspace::new();
    workspace.write_file("src/lint_target.txt", "alpha beta\n");
    let expected_version = compute_content_version(b"alpha beta\n");
    write_apply_edits_extension(&workspace, &expected_version);

    let mut config = TowerConfig::default();
    config.extensions.request_timeout_secs = 1;
    let handle = build_engine(
        &GlobalOpts {
            workspace_dir: Some(workspace.root().to_path_buf()),
            extensions_dir: Some(workspace.root().join("runtime_extensions")),
        },
        config,
    )
    .expect("build engine with apply-edits test extension");
    let mut registry = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    registry
        .call("tower_apply_replace", json!({}))
        .expect("apply extension must request workspace/applyEdits successfully");

    let read = registry
        .call("tower_read_file", json!({ "path": "src/lint_target.txt" }))
        .expect("read mutated file through shared engine state");
    assert_eq!(read["content"], "alpha fixed\n");

    let matches = registry
        .call("tower_search_text", json!({ "pattern": "fixed" }))
        .expect("search mutated content through shared index");
    assert!(
        matches["matches"]
            .as_array()
            .expect("search result must include a matches array")
            .iter()
            .any(|hit| hit["path"] == "src/lint_target.txt"),
        "apply-edits write must be visible through indexed search; got {matches}"
    );
}

#[test]
fn apply_edits_from_file_changed_subscriber_does_not_deadlock_its_own_tool_call() {
    let workspace = TestWorkspace::new();
    workspace.write_file("src/lint_target.txt", "alpha beta\n");
    let expected_version = compute_content_version(b"alpha beta\n");
    write_subscribed_apply_edits_extension(&workspace, &expected_version);

    let mut config = TowerConfig::default();
    config.extensions.request_timeout_secs = 2;
    let handle = build_engine(
        &GlobalOpts {
            workspace_dir: Some(workspace.root().to_path_buf()),
            extensions_dir: Some(workspace.root().join("runtime_extensions")),
        },
        config,
    )
    .expect("build engine with subscribed apply-edits test extension");
    let mut registry = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    registry
        .call("tower_apply_subscribed_replace", json!({}))
        .expect("subscribed apply extension must not deadlock its own apply-edits HostCall");

    let read = registry
        .call("tower_read_file", json!({ "path": "src/lint_target.txt" }))
        .expect("read mutated file through shared engine state");
    assert_eq!(read["content"], "alpha fixed\n");
}

#[test]
fn apply_edits_from_file_changed_handler_does_not_deadlock_original_apply() {
    let workspace = TestWorkspace::new();
    workspace.write_file("src/lint_target.txt", "alpha beta\n");
    let first_expected_version = compute_content_version(b"alpha beta\n");
    let second_expected_version = compute_content_version(b"alpha fixed\n");
    write_reentrant_file_changed_apply_edits_extension(
        &workspace,
        &first_expected_version,
        &second_expected_version,
    );

    let mut config = TowerConfig::default();
    config.extensions.request_timeout_secs = 2;
    let handle = build_engine(
        &GlobalOpts {
            workspace_dir: Some(workspace.root().to_path_buf()),
            extensions_dir: Some(workspace.root().join("runtime_extensions")),
        },
        config,
    )
    .expect("build engine with reentrant apply-edits test extension");
    let mut registry = ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry);

    registry
        .call("tower_apply_reentrant_replace", json!({}))
        .expect("fileChanged apply-edits HostCall must not deadlock the original apply");

    let read = registry
        .call("tower_read_file", json!({ "path": "src/lint_target.txt" }))
        .expect("read twice-mutated file through shared engine state");
    assert_eq!(read["content"], "omega fixed\n");
}

#[test]
fn eager_initialize_time_apply_edits_uses_real_extension_host_for_file_changed_broadcasts() {
    let workspace = TestWorkspace::new();
    workspace.write_file("src/lint_target.txt", "alpha beta\n");
    let expected_version = compute_content_version(b"alpha beta\n");
    write_eager_observer_and_initialize_apply_extensions(&workspace, &expected_version);

    let mut config = TowerConfig::default();
    config.extensions.request_timeout_secs = 2;
    let _handle = build_engine(
        &GlobalOpts {
            workspace_dir: Some(workspace.root().to_path_buf()),
            extensions_dir: Some(workspace.root().join("runtime_extensions")),
        },
        config,
    )
    .expect("build engine with eager initialize-time apply-edits extension");

    let observed = fs::read_to_string(workspace.root().join("observed_event.json"))
        .expect("observer must receive fileChanged from initialize-time apply-edits");
    let observed: Value = serde_json::from_str(&observed).expect("observed event must be JSON");
    assert_eq!(observed["method"], "deliverEvent");
    assert_eq!(observed["params"]["path"], "src/lint_target.txt");
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
fn lint_fix_tool_appears_in_merged_registry_and_is_not_registered_as_native_mcp_tool() {
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
    let extension_names = merged
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    let native_names = NativeToolRegistry::new(empty_engine_state(&workspace))
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();

    assert!(extension_names.iter().any(|name| name == "tower_lint_fix"));
    assert!(!EXPECTED_NATIVE_TOOL_NAMES.contains(&"tower_lint_fix"));
    assert!(!native_names.iter().any(|name| name == "tower_lint_fix"));
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
fn main_rs_initializes_the_extension_and_declares_check_and_fix_with_required_capabilities() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("ok-lint.sh", "#!/bin/sh\nexit 0\n");
    workspace.write_lint_config(&script);

    let mut child = ProtocolChild::spawn(&workspace);
    let response = child.initialize_response();
    let result = &response["result"]["data"];
    let tool_names = result["tools"]
        .as_array()
        .expect("InitResult tools must be an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    let capability_names = result["capabilities"]
        .as_array()
        .expect("InitResult capabilities must be an array")
        .iter()
        .map(|capability| capability.as_str().expect("capability name"))
        .collect::<Vec<_>>();
    assert_eq!(response["result"]["type"], "Initialized");
    assert_eq!(tool_names, vec!["check", "fix"]);
    assert_eq!(
        capability_names,
        vec!["read_file", "list_files", "request_apply_edits", "log"]
    );
}

#[test]
fn lint_fix_path_reads_file_hashes_content_applies_one_safe_fix_and_reports_one_applied_fix() {
    let workspace = TestWorkspace::new();
    let script = rustc_fix_script(&workspace, "safe-fix.sh", "MachineApplicable");
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "nope\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "path": "src/main.rs" }))
        .expect("fix must return a tool result");
    adapter.shutdown();

    assert_eq!(result["files_changed"], 1);
    assert_eq!(result["fixes_applied"], 1);
    assert_eq!(result["fixes_skipped"], json!([]));
    assert_eq!(result["previews"], json!([]));
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).expect("read fixed file"),
        "yep\n"
    );
}

#[test]
fn lint_fix_dry_run_returns_would_apply_preview_and_leaves_file_hash_unchanged() {
    let workspace = TestWorkspace::new();
    let script = rustc_fix_script(&workspace, "dry-run-fix.sh", "MachineApplicable");
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "nope\n");
    let before = fs::read(workspace.root().join("src/main.rs")).expect("read original file");
    let before_hash = compute_content_version(&before);

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "path": "src/main.rs", "dry_run": true }))
        .expect("dry-run fix must return a tool result");
    adapter.shutdown();

    let after = fs::read(workspace.root().join("src/main.rs")).expect("read unchanged file");
    assert_eq!(compute_content_version(&after), before_hash);
    assert_eq!(result["files_changed"], 0);
    assert_eq!(result["fixes_applied"], 1);
    assert_eq!(result["previews"][0]["path"], "src/main.rs");
    assert_eq!(result["previews"][0]["preview_content"], "yep\n");
}

#[test]
fn lint_fix_counts_distinct_changed_files_and_accepted_lint_fix_records_not_byte_edits() {
    let workspace = TestWorkspace::new();
    let a_first = rustc_span("src/a.rs", 1, 1, 1, 4, 0, 3, "one");
    let a_second = rustc_span("src/a.rs", 1, 5, 1, 8, 4, 7, "two");
    let b_only = rustc_span("src/b.rs", 1, 1, 1, 4, 0, 3, "yes");
    let script = rustc_fix_case_script(
        &workspace,
        "multi-file-fix.sh",
        &[
            (
                "src/a.rs",
                vec![rustc_message(
                    "replace two ranges in one diagnostic",
                    "fixture::two_edits_one_fix",
                    &format!("[{a_first},{a_second}]"),
                )],
            ),
            (
                "src/b.rs",
                vec![rustc_message(
                    "replace one range",
                    "fixture::one_edit_one_fix",
                    &format!("[{b_only}]"),
                )],
            ),
        ],
    );
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/a.rs", "bad bad\n");
    workspace.write_file("src/b.rs", "bad\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({}))
        .expect("workspace fix must return a tool result");
    adapter.shutdown();

    assert_eq!(
        result["files_changed"], 2,
        "files_changed counts distinct files with new_version: Some(_)"
    );
    assert_eq!(
        result["fixes_applied"], 2,
        "fixes_applied counts accepted LintFix records, not the three byte edits"
    );
    assert_eq!(result["fixes_skipped"], json!([]));
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/a.rs")).expect("read fixed a.rs"),
        "one two\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/b.rs")).expect("read fixed b.rs"),
        "yes\n"
    );
}

#[test]
fn lint_fix_conflicting_multi_edit_skips_entire_fix_without_applying_any_edit() {
    let workspace = TestWorkspace::new();
    let accepted = rustc_span("src/main.rs", 1, 1, 1, 6, 0, 5, "alpha");
    let conflicting = rustc_span("src/main.rs", 1, 4, 1, 9, 3, 8, "beta");
    let script = rustc_fix_case_script(
        &workspace,
        "partial-conflict-fix.sh",
        &[(
            "src/main.rs",
            vec![rustc_message(
                "one range accepted one range conflicts",
                "fixture::partial_conflict",
                &format!("[{accepted},{conflicting}]"),
            )],
        )],
    );
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "abcdefgh\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "path": "src/main.rs" }))
        .expect("partial conflict fix must return a tool result");
    adapter.shutdown();

    assert_eq!(result["files_changed"], 0);
    assert_eq!(
        result["fixes_applied"], 0,
        "a multi-edit LintFix with any conflicting edit must not be partially applied"
    );
    assert_eq!(result["fixes_skipped"].as_array().expect("skips").len(), 1);
    assert_eq!(result["fixes_skipped"][0]["path"], "src/main.rs");
    assert_eq!(result["fixes_skipped"][0]["reason"], "conflict");
    assert_eq!(result["fixes_skipped"][0]["supported_fix"], true);
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).expect("read unchanged file"),
        "abcdefgh\n"
    );
}

#[test]
fn lint_fix_overlapping_fixture_edits_skip_the_entire_conflicting_fix() {
    let workspace = TestWorkspace::new();
    let first = rustc_span("src/main.rs", 1, 1, 1, 3, 0, 2, "AA");
    let overlapping = rustc_span("src/main.rs", 1, 2, 1, 4, 1, 3, "BB");
    let later = rustc_span("src/main.rs", 1, 5, 1, 7, 4, 6, "CC");
    let script = rustc_fix_case_script(
        &workspace,
        "overlap-deterministic-fix.sh",
        &[(
            "src/main.rs",
            vec![rustc_message(
                "overlap plus independent edit",
                "fixture::overlap_deterministic",
                &format!("[{first},{overlapping},{later}]"),
            )],
        )],
    );
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "abcdef\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "path": "src/main.rs" }))
        .expect("overlapping fix must return a tool result");
    adapter.shutdown();

    assert_eq!(result["files_changed"], 0);
    assert_eq!(result["fixes_applied"], 0);
    assert_eq!(result["fixes_skipped"].as_array().expect("skips").len(), 1);
    assert_eq!(result["fixes_skipped"][0]["reason"], "conflict");
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs"))
            .expect("read unchanged conflicting file"),
        "abcdef\n",
        "a conflicting LintFix must be skipped atomically"
    );
}

#[test]
fn lint_fix_skips_structured_fixes_for_a_different_file_than_the_current_plan() {
    let workspace = TestWorkspace::new();
    let foreign = rustc_span("src/b.rs", 1, 1, 1, 4, 0, 3, "bbb");
    let script = rustc_fix_case_script(
        &workspace,
        "cross-file-fix.sh",
        &[(
            "src/a.rs",
            vec![rustc_message(
                "structured fix belongs to another file",
                "fixture::cross_file",
                &format!("[{foreign}]"),
            )],
        )],
    );
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/a.rs", "aaa\n");
    workspace.write_file("src/b.rs", "bad\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "path": "src/a.rs" }))
        .expect("cross-file fix must return a tool result");
    adapter.shutdown();

    assert_eq!(result["files_changed"], 0);
    assert_eq!(result["fixes_applied"], 0);
    assert_eq!(result["fixes_skipped"].as_array().expect("skips").len(), 1);
    assert_eq!(result["fixes_skipped"][0]["path"], "src/b.rs");
    assert_eq!(result["fixes_skipped"][0]["reason"], "unsupported");
    assert_eq!(result["fixes_skipped"][0]["supported_fix"], true);
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/a.rs")).expect("read a.rs"),
        "aaa\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/b.rs")).expect("read b.rs"),
        "bad\n"
    );
}

#[test]
fn lint_fix_unsafe_false_skips_maybe_incorrect_but_unsafe_true_applies_it() {
    let workspace = TestWorkspace::new();
    let script = rustc_fix_script(&workspace, "unsafe-fix.sh", "MaybeIncorrect");
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "nope\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let safe_default = adapter
        .call_tool("fix", json!({ "path": "src/main.rs" }))
        .expect("unsafe default fix must return a tool result");
    adapter.shutdown();

    assert_eq!(safe_default["files_changed"], 0);
    assert_eq!(safe_default["fixes_applied"], 0);
    assert_eq!(safe_default["fixes_skipped"][0]["reason"], "unsafe");
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).expect("read skipped file"),
        "nope\n"
    );

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let unsafe_allowed = adapter
        .call_tool("fix", json!({ "path": "src/main.rs", "unsafe": true }))
        .expect("unsafe opt-in fix must return a tool result");
    adapter.shutdown();

    assert_eq!(unsafe_allowed["files_changed"], 1);
    assert_eq!(unsafe_allowed["fixes_applied"], 1);
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).expect("read fixed file"),
        "yep\n"
    );
}

#[test]
fn lint_fix_generic_regex_fixless_diagnostics_return_unsupported_skips_without_protocol_error() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "generic-lint.sh",
        "#!/bin/sh\nprintf '%s:1:1: warning: G001: generic issue\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("src/main.txt", "plain text\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "path": "src/main.txt" }))
        .expect("unsupported fixes must remain on the success-result path");
    adapter.shutdown();

    assert_eq!(result["files_changed"], 0);
    assert_eq!(result["fixes_applied"], 0);
    assert_eq!(result["fixes_skipped"][0]["reason"], "unsupported");
    assert_eq!(result["fixes_skipped"][0]["supported_fix"], false);
}

#[test]
fn lint_fix_maps_host_precondition_failed_and_invalid_range_to_nonfatal_skipped_fixes() {
    let workspace = TestWorkspace::new();
    let cas_span = rustc_span("src/cas.rs", 1, 1, 1, 4, 0, 3, "ok");
    let range_span = rustc_span("src/range.rs", 1, 1, 1, 4, 0, 3, "ok");
    let script = rustc_fix_case_script(
        &workspace,
        "host-error-mapping-fix.sh",
        &[
            (
                "src/cas.rs",
                vec![rustc_message(
                    "stale write",
                    "fixture::cas",
                    &format!("[{cas_span}]"),
                )],
            ),
            (
                "src/range.rs",
                vec![rustc_message(
                    "bad range",
                    "fixture::range",
                    &format!("[{range_span}]"),
                )],
            ),
        ],
    );
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);

    let mut child = ProtocolChild::spawn(&workspace);
    child.initialize();
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "invokeTool",
        "params": { "name": "fix", "params": {} }
    }));

    let list_files = child.read_frame();
    assert_eq!(list_files["method"], "workspace/listFiles");
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": list_files["id"].clone(),
        "result": ["src/cas.rs", "src/range.rs"]
    }));

    let read_cas = child.read_frame();
    assert_eq!(read_cas["method"], "workspace/readFile");
    assert_eq!(read_cas["params"]["path"], "src/cas.rs");
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": read_cas["id"].clone(),
        "result": "bad\n"
    }));

    let apply_cas = child.read_frame();
    assert_eq!(apply_cas["method"], "workspace/applyEdits");
    assert_eq!(apply_cas["params"]["path"], "src/cas.rs");
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": apply_cas["id"].clone(),
        "error": { "code": -32000, "message": "precondition_failed: stale content version" }
    }));

    let read_range = child.read_frame();
    assert_eq!(read_range["method"], "workspace/readFile");
    assert_eq!(read_range["params"]["path"], "src/range.rs");
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": read_range["id"].clone(),
        "result": "bad\n"
    }));

    let apply_range = child.read_frame();
    assert_eq!(apply_range["method"], "workspace/applyEdits");
    assert_eq!(apply_range["params"]["path"], "src/range.rs");
    child.write_frame(json!({
        "jsonrpc": "2.0",
        "id": apply_range["id"].clone(),
        "error": { "code": -32000, "message": "invalid_range: edit range is outside file bounds" }
    }));

    let response = child.read_frame();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["type"], "ToolResult");
    let result = &response["result"]["data"];
    assert_eq!(result["files_changed"], 0);
    assert_eq!(result["fixes_applied"], 0);
    assert_eq!(
        result["fixes_skipped"],
        json!([
            {
                "path": "src/cas.rs",
                "reason": "cas_conflict",
                "diagnostic": {
                    "path": "src/cas.rs",
                    "line": 0,
                    "character": 0,
                    "endLine": 0,
                    "endCharacter": 3,
                    "severity": "warning",
                    "code": "fixture::cas",
                    "message": "stale write",
                    "source": "fixture-lint"
                },
                "supported_fix": true
            },
            {
                "path": "src/range.rs",
                "reason": "invalid_range",
                "diagnostic": {
                    "path": "src/range.rs",
                    "line": 0,
                    "character": 0,
                    "endLine": 0,
                    "endCharacter": 3,
                    "severity": "warning",
                    "code": "fixture::range",
                    "message": "bad range",
                    "source": "fixture-lint"
                },
                "supported_fix": true
            }
        ])
    );
}

#[test]
fn lint_fix_successful_write_runs_exactly_one_follow_up_check_for_remaining_diagnostics() {
    let workspace = TestWorkspace::new();
    let counter = workspace.root().join("lint-count.txt");
    let counter_path = counter.to_string_lossy().replace('\\', "\\\\");
    let script = workspace.script(
        "follow-up-once-fix.sh",
        &format!(
            r#"#!/bin/sh
count=0
if [ -f "{counter_path}" ]; then
  count=$(cat "{counter_path}")
fi
count=$((count + 1))
printf '%s' "$count" > "{counter_path}"
if [ "$count" = 1 ]; then
  printf '%s\n' '{{"reason":"compiler-message","message":{{"message":"replace nope","level":"warning","code":{{"code":"fixture::replace"}},"spans":[{{"file_name":"src/main.rs","is_primary":true,"line_start":1,"column_start":1,"line_end":1,"column_end":5,"byte_start":0,"byte_end":4,"suggested_replacement":"yep","applicability":"MachineApplicable"}}]}}}}'
else
  printf '%s\n' '{{"reason":"compiler-message","message":{{"message":"still noisy","level":"warning","code":{{"code":"fixture::remaining"}},"spans":[{{"file_name":"src/main.rs","is_primary":true,"line_start":1,"column_start":1,"line_end":1,"column_end":4}}]}}}}'
fi
"#
        ),
    );
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "nope\n");

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "path": "src/main.rs" }))
        .expect("fix with follow-up check must return a tool result");
    adapter.shutdown();

    assert_eq!(result["files_changed"], 1);
    assert_eq!(result["fixes_applied"], 1);
    assert_eq!(
        result["remaining_diagnostics"]
            .as_array()
            .expect("remaining diagnostics")
            .len(),
        1
    );
    assert_eq!(
        result["remaining_diagnostics"][0]["code"],
        "fixture::remaining"
    );
    assert_eq!(
        fs::read_to_string(counter).expect("read linter invocation counter"),
        "2",
        "successful writes must trigger exactly one follow-up check and no iterative fix loop"
    );
}

#[test]
fn lint_fix_invalid_request_returns_stable_lint_fix_invalid_request_error_code() {
    let workspace = TestWorkspace::new();
    let script = workspace.script("ok-lint.sh", "#!/bin/sh\nexit 0\n");
    workspace.write_lint_config(&script);

    let mut adapter = spawn_lint_fix_adapter(&workspace);
    let result = adapter
        .call_tool("fix", json!({ "unsafe": "yes" }))
        .expect("invalid fix params must return a structured tool result");
    adapter.shutdown();

    assert_eq!(result["error"]["code"], "lint_fix_invalid_request");
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

    fn initialize_response(&mut self) -> Value {
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
        response
    }

    fn initialize(&mut self) {
        let _ = self.initialize_response();
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
