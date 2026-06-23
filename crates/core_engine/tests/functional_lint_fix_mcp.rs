// Feature: F003

#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::fs;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

mod lint_support;

use core_engine::adapters::InMemoryStorage;
use core_engine::adapters::extension::SidecarHostAdapter;
use core_engine::adapters::mcp::diagnostics::NoOpDiagnosticsReader;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::lsp_tools::SubscriptionRegistry;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::rmcp_server::TowerMcpHandler;
use core_engine::domain::RelativePath;
use core_engine::domain::extension_host::ExtensionRegistry;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::token::tokenize;
use core_engine::domain::virtual_file::FileMetadata;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::FileSystemPort;
use lint_support::{
    SEVERITY_CODE_GENERIC_REGEX, TestWorkspace, host_deps, lint_extension_bin, lint_fix_manifest,
};
use rmcp::model::CallToolRequestParams;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

fn engine_state(workspace: &TestWorkspace, indexed_paths: &[&str]) -> Arc<RwLock<EngineState>> {
    let mut project = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut fs = workspace.real_fs();
    let storage = InMemoryStorage::new();

    for path in indexed_paths {
        let relative_path = RelativePath::new(*path);
        let id = project
            .insert(relative_path.clone(), FileMetadata::default())
            .expect("index workspace file");
        index.insert(id, &tokenize(relative_path.as_str()));
        let content = fs
            .read(&relative_path)
            .expect("indexed file must exist on disk");
        fs.write(relative_path, content)
            .expect("write indexed file");
    }

    Arc::new(RwLock::new(EngineState::new(
        project,
        index,
        Box::new(storage),
        Box::new(fs),
    )))
}

fn handler(workspace: &TestWorkspace, indexed_paths: &[&str]) -> TowerMcpHandler {
    let mut extension_registry = ExtensionRegistry::new();
    let lint_extension = SidecarHostAdapter::spawn(
        lint_fix_manifest(&lint_extension_bin()),
        host_deps(workspace.real_fs()),
        TEST_TIMEOUT,
    )
    .expect("spawn lint extension");
    extension_registry
        .register(lint_extension)
        .expect("register lint extension");

    let merged = ExtensionMergedRegistry::new(
        engine_state(workspace, indexed_paths),
        Arc::new(RwLock::new(extension_registry)),
    );
    TowerMcpHandler::new(
        merged,
        Arc::new(NoOpDiagnosticsReader),
        Arc::new(Mutex::new(SubscriptionRegistry::new())),
        vec![],
    )
}

async fn start_server(handler: TowerMcpHandler) -> rmcp::service::RunningService<RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    tokio::spawn(async move {
        if let Ok(server) = handler.serve(server_transport).await {
            let _ = server.waiting().await;
        }
    });
    ().serve(client_transport)
        .await
        .expect("client connect failed")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

async fn call_lint_fix(
    client: &rmcp::service::RunningService<RoleClient, ()>,
    args: Value,
) -> rmcp::model::CallToolResult {
    client
        .peer()
        .call_tool(
            CallToolRequestParams::new("tower_lint_fix")
                .with_arguments(args.as_object().expect("object args").clone()),
        )
        .await
        .expect("tower_lint_fix call must succeed")
}

fn first_text(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .first()
        .and_then(|content| match &content.raw {
            rmcp::model::RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .expect("tool result must include text content")
}

fn result_json(result: &rmcp::model::CallToolResult) -> Value {
    assert!(
        !result.is_error.unwrap_or(false),
        "lint fix must return success-result payloads for supported user-facing outcomes"
    );
    serde_json::from_str(first_text(result)).expect("tool text must be JSON")
}

fn write_rustc_fix_script(workspace: &TestWorkspace, name: &str) -> std::path::PathBuf {
    workspace.script(
        name,
        r#"#!/bin/sh
printf '%s\n' '{"reason":"compiler-message","message":{"message":"replace nope","level":"warning","code":{"code":"fixture::replace"},"spans":[{"file_name":"src/main.rs","is_primary":true,"line_start":1,"column_start":1,"line_end":1,"column_end":5,"byte_start":0,"byte_end":4,"suggested_replacement":"yep","applicability":"MachineApplicable"}]}}'
"#,
    )
}

#[test]
fn lint_fix_applies_structured_fix_through_mcp_merged_extension_surface() {
    let workspace = TestWorkspace::new();
    let script = write_rustc_fix_script(&workspace, "fix.sh");
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "nope\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.rs"])).await;
        let result = call_lint_fix(&client, json!({ "path": "src/main.rs" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["files_changed"], 1);
    assert_eq!(payload["fixes_applied"], 1);
    assert_eq!(payload["fixes_skipped"], json!([]));
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).expect("read fixed file"),
        "yep\n"
    );
}

#[test]
fn lint_fix_dry_run_returns_preview_without_writing_file_through_mcp() {
    let workspace = TestWorkspace::new();
    let script = write_rustc_fix_script(&workspace, "dry-run-fix.sh");
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    workspace.write_file("src/main.rs", "nope\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.rs"])).await;
        let result =
            call_lint_fix(&client, json!({ "path": "src/main.rs", "dry_run": true })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["files_changed"], 0);
    assert_eq!(payload["fixes_applied"], 1);
    assert_eq!(payload["previews"][0]["path"], "src/main.rs");
    assert_eq!(payload["previews"][0]["preview_content"], "yep\n");
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).expect("read unchanged file"),
        "nope\n"
    );
}

#[test]
fn lint_fix_reports_unsupported_diagnostic_without_mcp_error() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "generic-lint.sh",
        "#!/bin/sh\nprintf '%s:1:1: warning: G001: generic issue\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("src/main.txt", "plain text\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.txt"])).await;
        let result = call_lint_fix(&client, json!({ "path": "src/main.txt" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["files_changed"], 0);
    assert_eq!(payload["fixes_applied"], 0);
    assert_eq!(payload["fixes_skipped"][0]["reason"], "unsupported");
    assert_eq!(payload["fixes_skipped"][0]["supported_fix"], false);
}

#[test]
fn lint_fix_returns_structured_error_for_missing_linter_without_mcp_error() {
    let workspace = TestWorkspace::new();
    workspace.write_lint_config_with_format(
        &workspace.root().join("missing-linter"),
        &["rs"],
        "rustc-json",
        "append",
        None,
    );
    workspace.write_file("src/main.rs", "nope\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.rs"])).await;
        let result = call_lint_fix(&client, json!({ "path": "src/main.rs" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["error"]["code"], "lint_fix_unavailable");
}
