#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::fs;
use std::time::Duration;

mod lint_support;

use lint_support::{
    SEVERITY_CODE_GENERIC_REGEX, TestWorkspace, block_on, configure_lint_timeout_and_host_budget,
    handler, handler_with_timeout, handler_with_timeout_and_host_call_log, start_server,
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
fn lint_fix_non_timeout_errors_map_to_null_timeout_detail() {
    for name in ["missing", "unparseable", "nonzero", "invalid_config"] {
        let workspace = TestWorkspace::new();
        let script = workspace.script(
            "error.sh",
            match name {
                "unparseable" => "#!/bin/sh\nprintf 'not a lint diagnostic\\n'\n",
                "nonzero" => "#!/bin/sh\nexit 1\n",
                _ => "#!/bin/sh\nprintf 'src/main.rs:1:1: warning: T001: harmless\\n'\n",
            },
        );
        if name == "missing" {
            workspace.write_lint_config_with_format(
                &workspace.root().join("missing-linter"),
                &["txt"],
                "rustc-json",
                "append",
                None,
            );
        } else if name == "invalid_config" {
            workspace.write_invalid_generic_lint_config_without_regex(&script);
        } else {
            workspace.write_lint_config_with_format(
                &script,
                &["txt"],
                "rustc-json",
                "append",
                None,
            );
        }
        workspace.write_file("src/main.txt", "nope\n");

        let payload = block_on(async {
            let client = start_server(handler(&workspace, &["src/main.txt"])).await;
            let result = call_lint_fix(&client, json!({ "path": "src/main.txt" })).await;
            let payload = result_json(&result);
            client.cancel().await.expect("cancel client");
            payload
        });

        assert_eq!(
            payload["error"]["code"], "lint_fix_unavailable",
            "{name} error code"
        );
        assert_eq!(payload["error"]["detail"], Value::Null, "{name} detail");
    }
}

#[test]
fn tower_lint_fix_uses_one_explicit_timeout_for_fix_and_verification() {
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
  sleep 0.7
  printf '%s\n' '{{"reason":"compiler-message","message":{{"message":"replace nope","level":"warning","code":{{"code":"fixture::replace"}},"spans":[{{"file_name":"src/main.rs","is_primary":true,"line_start":1,"column_start":1,"line_end":1,"column_end":5,"byte_start":0,"byte_end":4,"suggested_replacement":"yep","applicability":"MachineApplicable"}}]}}}}'
else
  sleep 0.7
  printf '%s\n' '{{"reason":"compiler-message","message":{{"message":"still noisy","level":"warning","code":{{"code":"fixture::remaining"}},"spans":[{{"file_name":"src/main.rs","is_primary":true,"line_start":1,"column_start":1,"line_end":1,"column_end":4}}]}}}}'
fi
"#
        ),
    );
    workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
    let host_timeout = configure_lint_timeout_and_host_budget(&workspace, Some(1));
    assert_eq!(host_timeout, Duration::from_secs(3));
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

    assert_eq!(
        payload,
        json!({
            "error": {
                "code": "lint_timeout",
                "message": "lint command timed out",
                "detail": {"operation": "fix", "layer": "subprocess", "budget_ms": 1000}
            }
        }),
        "verification must consume only the deadline remaining after extraction"
    );
    assert_eq!(
        fs::read_to_string(counter).expect("read invocation count"),
        "2"
    );
}

#[test]
fn tower_lint_fix_configured_timeout_reports_operation_layer_budget_and_reaps_process_tree() {
    for timeout_stage in ["extraction", "verification"] {
        let workspace = TestWorkspace::new();
        let count_path = workspace.root().join(".tower/fix-count");
        let pid_path = workspace.root().join(".tower/fix-descendant.pid");
        let script = workspace.script(
            "fix-timeout.sh",
            &format!(
                r#"#!/bin/sh
count=0
if [ -f "{}" ]; then count=$(cat "{}"); fi
count=$((count + 1))
printf '%s' "$count" > "{}"
printf '%s\n' "$count" >> "{}/.tower/fix-stages"
printf 'STDOUT_SENTINEL STDERR_SENTINEL DIAGNOSTICS_SENTINEL ARGV_SENTINEL ENVIRONMENT_SENTINEL\n' >&2
if {{ [ "{}" = extraction ] && [ "$count" = 1 ]; }} || {{ [ "{}" = verification ] && [ "$count" = 2 ]; }}; then
  sh -c 'trap "" TERM; while :; do sleep 1; done' &
  echo $! > "{}"
  wait
fi
if [ $((count % 2)) = 1 ]; then
  printf '%s\n' '{{"reason":"compiler-message","message":{{"message":"STDOUT_SENTINEL","level":"warning","code":{{"code":"fixture::replace"}},"spans":[{{"file_name":"src/main.rs","is_primary":true,"line_start":1,"column_start":1,"line_end":1,"column_end":5,"byte_start":0,"byte_end":4,"suggested_replacement":"yep","applicability":"MachineApplicable"}}]}}}}'
fi
"#,
                count_path.display(),
                count_path.display(),
                count_path.display(),
                workspace.root().display(),
                timeout_stage,
                timeout_stage,
                pid_path.display(),
            ),
        );
        workspace.write_lint_config_with_format(&script, &["rs"], "rustc-json", "append", None);
        let host_timeout = configure_lint_timeout_and_host_budget(&workspace, Some(1));
        workspace.write_file(
            "src/main.rs",
            "nope /* SOURCE_SENTINEL STDIN_SENTINEL DIAGNOSTICS_SENTINEL ARGV_SENTINEL ENVIRONMENT_SENTINEL */\n",
        );

        let host_call_log = lint_support::HostCallLog::default();
        let (timeout_payload, recovery_payload) = block_on(async {
            let client = start_server(handler_with_timeout_and_host_call_log(
                &workspace,
                &["src/main.rs"],
                host_timeout,
                host_call_log.clone(),
            ))
            .await;
            let timeout =
                result_json(&call_lint_fix(&client, json!({ "path": "src/main.rs" })).await);
            let recovery =
                result_json(&call_lint_fix(&client, json!({ "path": "src/main.rs" })).await);
            let descendant = fs::read_to_string(&pid_path).expect("read descendant pid");
            assert!(
                !std::process::Command::new("kill")
                    .args(["-0", descendant.trim()])
                    .status()
                    .is_ok_and(|status| status.success()),
                "timed-out descendant must be dead before sidecar shutdown"
            );
            client.cancel().await.expect("cancel client");
            (timeout, recovery)
        });

        assert_eq!(
            timeout_payload,
            json!({
                "error": {
                    "code": "lint_timeout",
                    "message": "lint command timed out",
                    "detail": {"operation": "fix", "layer": "subprocess", "budget_ms": 1000}
                }
            }),
            "{timeout_stage} timeout payload"
        );
        let public_payload = timeout_payload.to_string();
        for sentinel in [
            "SOURCE_SENTINEL",
            "STDIN_SENTINEL",
            "DIAGNOSTICS_SENTINEL",
            "ARGV_SENTINEL",
            "ENVIRONMENT_SENTINEL",
            "STDOUT_SENTINEL",
            "STDERR_SENTINEL",
        ] {
            assert!(
                !public_payload.contains(sentinel),
                "public timeout payload leaked {sentinel}: {public_payload}"
            );
            let host_calls = host_call_log.joined();
            assert!(
                !host_calls.contains(sentinel),
                "captured HostCall log leaked {sentinel}: {host_calls}"
            );
        }
        assert!(
            recovery_payload.get("error").is_none(),
            "same-sidecar recovery"
        );
        let stages = fs::read_to_string(workspace.root().join(".tower/fix-stages"))
            .expect("read ordered stage evidence");
        if timeout_stage == "verification" {
            assert!(stages.starts_with("1\n2\n"));
        } else {
            assert!(stages.starts_with("1\n"));
        }
    }
}
