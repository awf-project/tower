#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::fs;
use std::time::Duration;

mod lint_support;

use lint_support::{
    SEVERITY_CODE_GENERIC_REGEX, TestWorkspace, block_on, configure_lint_timeout_and_host_budget,
    handler, handler_with_timeout, start_server,
};
use rmcp::RoleClient;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

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

#[test]
fn tower_lint_fix_honors_configured_timeout_for_fix_and_verification() {
    const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(14);

    let workspace = TestWorkspace::new();
    let counter = workspace.root().join("configured-fix-count.txt");
    let counter_path = counter.to_string_lossy().replace('\\', "\\\\");
    let script = workspace.script(
        "configured-timeout-fix.sh",
        &format!(
            r#"#!/bin/sh
count=0
if [ -f "{counter_path}" ]; then count=$(cat "{counter_path}"); fi
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
    let host_timeout = configure_lint_timeout_and_host_budget(&workspace, Some(4));
    assert_eq!(host_timeout, Duration::from_secs(9));
    workspace.write_file("src/main.rs", "nope\n");

    let payload = block_on(async {
        let client = start_server(handler_with_timeout(
            &workspace,
            &["src/main.rs"],
            host_timeout,
        ))
        .await;
        let result = tokio::time::timeout(
            MCP_REQUEST_TIMEOUT,
            call_lint_fix(&client, json!({ "path": "src/main.rs" })),
        )
        .await
        .expect("lint fix MCP request must finish within its harness budget");
        let payload = result_json(&result);
        client.cancel().await.expect("cancel client");
        payload
    });

    assert_eq!(payload["files_changed"], 1);
    assert_eq!(payload["fixes_applied"], 1);
    assert_eq!(
        payload["remaining_diagnostics"],
        json!([{
            "path": "src/main.rs",
            "line": 0,
            "character": 0,
            "endLine": 0,
            "endCharacter": 3,
            "severity": "warning",
            "code": "fixture::remaining",
            "message": "still noisy",
            "source": "fixture-lint"
        }])
    );
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).expect("read fixed file"),
        "yep\n"
    );
    assert_eq!(
        fs::read_to_string(counter).expect("read invocation count"),
        "2"
    );
}
