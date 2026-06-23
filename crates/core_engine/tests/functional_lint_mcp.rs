// Feature: F002

#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

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
    SEVERITY_CODE_GENERIC_REGEX, TestWorkspace, host_deps, lint_check_manifest, lint_extension_bin,
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
        lint_check_manifest(&lint_extension_bin()),
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

async fn call_lint_check(
    client: &rmcp::service::RunningService<RoleClient, ()>,
    args: Value,
) -> rmcp::model::CallToolResult {
    client
        .peer()
        .call_tool(
            CallToolRequestParams::new("tower_lint_check")
                .with_arguments(args.as_object().expect("object args").clone()),
        )
        .await
        .expect("tower_lint_check call must succeed")
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
        "lint tool must return success-result payloads for supported user-facing outcomes"
    );
    serde_json::from_str(first_text(result)).expect("tool text must be JSON")
}

#[test]
fn lint_check_returns_diagnostics_for_single_configured_file_through_mcp() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "lint.sh",
        "#!/bin/sh\nprintf '%s:3:5: warning: T100: avoid placeholder text\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("src/main.txt", "one\ntwo\nTODO\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.txt"])).await;
        let result = call_lint_check(&client, json!({ "path": "src/main.txt" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], true);
    assert_eq!(
        payload["diagnostics"],
        json!([
            {
                "path": "src/main.txt",
                "line": 2,
                "character": 4,
                "endLine": 2,
                "endCharacter": 4,
                "severity": "warning",
                "code": "T100",
                "message": "avoid placeholder text",
                "source": "fixture-lint"
            }
        ])
    );
}

#[test]
fn lint_check_serializes_information_diagnostic_as_info_through_mcp() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "lint-info.sh",
        "#!/bin/sh\nprintf '%s:1:1: info: I100: informational diagnostic\\n' \"$1\"\n",
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("src/info.txt", "content\n");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/info.txt"])).await;
        let result = call_lint_check(&client, json!({ "path": "src/info.txt" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], true);
    assert_eq!(payload["diagnostics"][0]["severity"], "info");
}

#[test]
fn lint_check_without_path_returns_sorted_workspace_diagnostics_through_mcp() {
    let workspace = TestWorkspace::new();
    let script = workspace.script(
        "lint-all.sh",
        r#"#!/bin/sh
case "$1" in
  "zeta.txt") printf 'zeta.txt:6:1: warning: Z001: final file\n' ;;
  "alpha.txt") printf 'alpha.txt:2:8: error: A001: first file\n' ;;
esac
"#,
    );
    workspace.write_lint_config_with_regex(&script, &["txt"], SEVERITY_CODE_GENERIC_REGEX);
    workspace.write_file("zeta.txt", "z");
    workspace.write_file("alpha.txt", "a");
    workspace.write_file("README.md", "# unsupported");

    let payload = block_on(async {
        let client =
            start_server(handler(&workspace, &["zeta.txt", "README.md", "alpha.txt"])).await;
        let result = call_lint_check(&client, json!({})).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], true);
    assert_eq!(
        payload["diagnostics"],
        json!([
            {
                "path": "alpha.txt",
                "line": 1,
                "character": 7,
                "endLine": 1,
                "endCharacter": 7,
                "severity": "error",
                "code": "A001",
                "message": "first file",
                "source": "fixture-lint"
            },
            {
                "path": "zeta.txt",
                "line": 5,
                "character": 0,
                "endLine": 5,
                "endCharacter": 0,
                "severity": "warning",
                "code": "Z001",
                "message": "final file",
                "source": "fixture-lint"
            }
        ])
    );
}

#[test]
fn lint_check_returns_structured_error_for_missing_linter_without_mcp_error() {
    let workspace = TestWorkspace::new();
    workspace.write_lint_config_with_regex(
        &workspace.root().join("missing-linter"),
        &["txt"],
        SEVERITY_CODE_GENERIC_REGEX,
    );
    workspace.write_file("src/main.txt", "plain text");

    let payload = block_on(async {
        let client = start_server(handler(&workspace, &["src/main.txt"])).await;
        let result = call_lint_check(&client, json!({ "path": "src/main.txt" })).await;
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["supported"], false);
    assert_eq!(payload["diagnostics"], json!([]));
    assert_eq!(payload["error"]["code"], "lint_missing_binary");
}
