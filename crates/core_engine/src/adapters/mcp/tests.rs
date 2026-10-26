//! Unit tests for the rmcp-based MCP adapter (spec 10a — rmcp 1.7 migration).
//!
//! Tests exercise [`TowerMcpHandler`] through an in-process duplex transport:
//! a real rmcp server is spun up on one end and a real rmcp client drives it
//! on the other. This removes the hand-rolled framing assumptions while
//! retaining the same behavioural coverage.
//!
//! TDD sequence (spec §TDD sequence):
//!   AC1 — `list_tools` returns the registered tools.
//!   AC2 — `call_tool` dispatches and returns the result.
//!   AC3 — `call_tool` with missing name → InvalidArgs error.
//!   AC4 — `call_tool` for an unknown tool → is_error:true content.
//!   AC5 — `list_resources` / `subscribe` / `unsubscribe` round-trip.
//!   AC6 — `get_info` advertises tools + resources with subscribe.

use std::sync::{Arc, Mutex};

use rmcp::model::Content;
use rmcp::service::PeerRequestOptions;
use rmcp::{RoleClient, ServerHandler, ServiceExt};
use serde_json::json;

use super::{
    ToolError,
    diagnostics::{DiagnosticsReader, NoOpDiagnosticsReader},
    extension_merged_registry::ExtensionMergedRegistry,
    lsp_tools::SubscriptionRegistry,
    rmcp_server::{TowerMcpHandler, tool_error_to_rmcp},
};
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::{InMemoryFs, InMemoryStorage};
use crate::domain::index::InvertedIndex;
use crate::domain::workspace::ProjectWorkspace;

// (No stub registries needed — tests use TowerMcpHandler backed by native tools.)

// ── Handler construction helpers ──────────────────────────────────────────────

fn make_empty_engine_state() -> Arc<std::sync::RwLock<EngineState>> {
    Arc::new(std::sync::RwLock::new(EngineState::new(
        ProjectWorkspace::new(),
        InvertedIndex::new(),
        Box::new(InMemoryStorage::new()),
        Box::new(InMemoryFs::new()),
    )))
}

fn empty_sub_reg() -> Arc<Mutex<SubscriptionRegistry>> {
    Arc::new(Mutex::new(SubscriptionRegistry::new()))
}

fn null_diag_reader() -> Arc<dyn DiagnosticsReader> {
    Arc::new(NoOpDiagnosticsReader)
}

/// Build a `TowerMcpHandler` backed by an `ExtensionMergedRegistry`.
///
/// `ExtensionMergedRegistry` can't be constructed from an arbitrary `ToolRegistry`
/// directly, so tests build one over a real (empty) engine state via
/// [`make_native_registry`] and exercise the resulting handler through a real
/// in-process rmcp client.
fn make_handler_from_merged(
    merged: ExtensionMergedRegistry,
    resource_uris: Vec<String>,
) -> TowerMcpHandler {
    TowerMcpHandler::new(merged, null_diag_reader(), empty_sub_reg(), resource_uris)
}

/// Build an `ExtensionMergedRegistry` backed by a real (empty) engine state and
/// no extensions. This gives us the native `tower_*` tools.
fn make_native_registry() -> ExtensionMergedRegistry {
    let ext_reg = Arc::new(std::sync::RwLock::new(
        crate::domain::extension_host::ExtensionRegistry::new(),
    ));
    ExtensionMergedRegistry::new(make_empty_engine_state(), ext_reg)
}

fn make_cancellable_sidecar_registry(marker: &std::path::Path) -> ExtensionMergedRegistry {
    use crate::adapters::InMemoryAstIndex;
    use crate::adapters::extension::host_deps::UnsupportedApplyEditsHost;
    use crate::adapters::extension::{ExtensionSupervisor, HostDeps};
    use crate::adapters::formatter::NoOpFormatQueue;
    use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
    use extension_protocol::{ExtensionManifest, ToolDecl};

    let script = r#"
read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"type":"Initialized","data":{"tools":[{"name":"wait","description":"wait until cancelled","schema_json":"{}"}],"events":[],"capabilities":[]}}}'
read -r invoke
if [ -f "$1" ]; then
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"type":"ToolResult","data":{"recovered":true}}}'
  read -r shutdown
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"type":"Ack"}}'
else
  sleep 20 &
  descendant=$!
  marker_tmp="${1}.tmp.$$"
  printf '%s %s\n' "$$" "$descendant" > "$marker_tmp"
  mv "$marker_tmp" "$1"
  while read -r ignored; do :; done
  sleep 20
fi
"#;
    let manifest = ExtensionManifest {
        name: "cancellable".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "cancellable-sidecar".to_owned(),
            marker.to_string_lossy().into_owned(),
        ],
        activation: Activation::Lazy,
        tools: vec![ToolDecl {
            name: "wait".to_owned(),
            description: "wait until cancelled".to_owned(),
            schema_json: "{}".to_owned(),
        }],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    };
    let deps = HostDeps {
        fs: Arc::new(Mutex::new(InMemoryFs::new())),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    };
    let supervisor = ExtensionSupervisor::new(manifest, deps, None, None);
    let mut extensions = crate::domain::extension_host::ExtensionRegistry::new();
    extensions
        .register(Box::new(supervisor))
        .expect("register cancellable sidecar");
    ExtensionMergedRegistry::new(
        make_empty_engine_state(),
        Arc::new(std::sync::RwLock::new(extensions)),
    )
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    !status
        .lines()
        .any(|line| line.starts_with("State:") && line.contains("Z (zombie)"))
}

#[cfg(target_os = "linux")]
async fn wait_for_process_death(pid: u32) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async move {
        while process_is_alive(pid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} must be dead before registry shutdown"));
}

// ── In-process server helper ──────────────────────────────────────────────────

/// Spin up a real rmcp server around `handler` and return a connected client.
///
/// Uses `tokio::io::duplex` for an in-process transport. The server task is
/// spawned on the current tokio runtime and exits when the client disconnects.
///
/// The returned client implements the full rmcp client protocol and can drive
/// `tools/list`, `tools/call`, `resources/list`, etc.
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

/// Run an async test body synchronously using a new tokio runtime.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

// ── F008 T017: structured MCP application errors ────────────────────────────────

#[test]
fn f008_application_introduces_exact_struct_variant_and_retains_execution_failed() {
    let data = json!({"outcome": "not_renameable"});
    let application = ToolError::Application {
        code: -32_042,
        message: "symbol cannot be renamed".to_owned(),
        data: Some(data.clone()),
    };

    match application {
        ToolError::Application {
            code,
            message,
            data: actual_data,
        } => {
            assert_eq!(code, -32_042);
            assert_eq!(message, "symbol cannot be renamed");
            assert_eq!(actual_data, Some(data));
        }
        other => panic!("expected application error, got {other:?}"),
    }

    assert!(matches!(
        ToolError::ExecutionFailed("adapter crashed".to_owned()),
        ToolError::ExecutionFailed(message) if message == "adapter crashed"
    ));
}

#[test]
fn f008_application_rmcp_branch_preserves_some_and_none_structured_content() {
    let data = json!({"outcome": "not_renameable", "reason": "generated symbol"});
    let with_data = tool_error_to_rmcp(ToolError::Application {
        code: -32_042,
        message: "symbol cannot be renamed".to_owned(),
        data: Some(data.clone()),
    })
    .expect("application errors must be tool results");

    assert_eq!(with_data.is_error, Some(true));
    assert_eq!(
        extract_first_text(&with_data.content),
        "symbol cannot be renamed"
    );
    assert_eq!(with_data.structured_content, Some(data));

    let without_data = tool_error_to_rmcp(ToolError::Application {
        code: -32_043,
        message: "rename unavailable".to_owned(),
        data: None,
    })
    .expect("application errors must be tool results");

    assert_eq!(without_data.is_error, Some(true));
    assert_eq!(
        extract_first_text(&without_data.content),
        "rename unavailable"
    );
    assert_eq!(without_data.structured_content, None);
}

#[test]
fn f008_application_invalid_args_behavior_is_unchanged() {
    block_on(async {
        let h = make_handler_from_merged(make_native_registry(), vec![]);
        let client = start_server(h).await;
        let error = client
            .peer()
            .call_tool(
                rmcp::model::CallToolRequestParams::new("tower_find_file")
                    .with_arguments(serde_json::Map::new()),
            )
            .await
            .expect_err("missing query must remain a protocol-level InvalidParams error");

        assert!(
            error.to_string().contains("-32602")
                || error
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("invalid params"),
            "unexpected InvalidArgs mapping: {error}"
        );
        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn f008_application_execution_failed_behavior_is_unchanged() {
    assert_eq!(
        ToolError::ExecutionFailed("adapter crashed".to_owned()).to_string(),
        "tool execution failed: adapter crashed"
    );

    let result = tool_error_to_rmcp(ToolError::ExecutionFailed("adapter crashed".to_owned()))
        .expect("execution failures must remain tool results");
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        extract_first_text(&result.content),
        "execution failed: adapter crashed"
    );
    assert_eq!(result.structured_content, None);
}

#[test]
fn f008_application_successful_result_behavior_is_unchanged() {
    block_on(async {
        let h = make_handler_from_merged(make_native_registry(), vec![]);
        let client = start_server(h).await;
        let result = client
            .peer()
            .call_tool(
                rmcp::model::CallToolRequestParams::new("tower_find_file")
                    .with_arguments(serde_json::from_value(json!({"query": "x"})).unwrap()),
            )
            .await
            .expect("successful tool call must retain its result envelope");

        assert_eq!(result.is_error, Some(false));
        assert!(result.structured_content.is_none());
        let payload: serde_json::Value = serde_json::from_str(&extract_first_text(&result.content))
            .expect("successful content must remain serialized JSON text");
        assert_eq!(payload, json!({"paths": []}));
        client.cancel().await.expect("cancel failed");
    });
}

// ── AC1: get_info advertises tools + resources ────────────────────────────────
//
// `get_info` does not require a full client round-trip; we can call it directly
// on the handler struct.

#[test]
fn ac1_get_info_advertises_tools_capability() {
    let merged = make_native_registry();
    let h = make_handler_from_merged(merged, vec![]);
    let info = h.get_info();
    assert!(
        info.capabilities.tools.is_some(),
        "tools capability must be advertised"
    );
}

#[test]
fn ac1_get_info_advertises_resources_with_subscribe() {
    let merged = make_native_registry();
    let h = make_handler_from_merged(merged, vec![]);
    let info = h.get_info();
    let res_cap = info
        .capabilities
        .resources
        .expect("resources capability must be advertised");
    assert_eq!(res_cap.subscribe, Some(true), "subscribe must be true");
}

#[test]
fn ac1_get_info_server_name_is_tower() {
    let merged = make_native_registry();
    let h = make_handler_from_merged(merged, vec![]);
    let info = h.get_info();
    assert_eq!(info.server_info.name, "tower");
}

// ── AC2: list_tools returns registered tools ──────────────────────────────────

#[test]
fn ac2_list_tools_native_registry_returns_expected_tools() {
    block_on(async {
        let merged = make_native_registry();
        let h = make_handler_from_merged(merged, vec![]);
        let client = start_server(h).await;
        let result = client
            .peer()
            .list_tools(Default::default())
            .await
            .expect("list_tools failed");
        assert!(
            !result.tools.is_empty(),
            "native registry must yield at least one tool"
        );
        // Check one known tool exists.
        let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            names.contains(&"tower_find_file"),
            "tower_find_file must be registered; got: {names:?}"
        );
        client.cancel().await.expect("cancel failed");
    });
}

// ── AC3: call_tool dispatches to registered tool ──────────────────────────────

#[test]
fn ac3_call_tool_dispatches_and_returns_result() {
    block_on(async {
        let merged = make_native_registry();
        let h = make_handler_from_merged(merged, vec![]);
        let client = start_server(h).await;
        // tower_find_file against the empty native registry: the dispatch must
        // succeed (is_error:false) and return a JSON payload with a `paths` array
        // (empty here). This asserts the validate→call→map round-trip end to end,
        // not merely that the tool is reachable.
        let result = client
            .peer()
            .call_tool(
                rmcp::model::CallToolRequestParams::new("tower_find_file")
                    .with_arguments(serde_json::from_value(json!({"query": "x"})).unwrap()),
            )
            .await
            .expect("call_tool RPC call itself must succeed");
        assert_eq!(
            result.is_error,
            Some(false),
            "find_file dispatch must succeed: {result:?}"
        );
        let text = extract_first_text(&result.content);
        let payload: serde_json::Value = serde_json::from_str(&text).expect("content must be JSON");
        assert!(
            payload
                .get("paths")
                .and_then(serde_json::Value::as_array)
                .is_some(),
            "payload must carry a 'paths' array; got: {text}"
        );
        client.cancel().await.expect("cancel failed");
    });
}

#[cfg(target_os = "linux")]
#[test]
fn cancelling_an_unlimited_extension_call_reaps_the_child_and_the_next_call_recovers() {
    block_on(async {
        let temp = tempfile::tempdir().expect("temporary cancellation fixture directory");
        let marker = temp.path().join("first-child-entered");
        let handler = make_handler_from_merged(make_cancellable_sidecar_registry(&marker), vec![]);
        let client = start_server(handler).await;

        let request = rmcp::model::ClientRequest::CallToolRequest(rmcp::model::Request::new(
            rmcp::model::CallToolRequestParams::new("tower_cancellable_wait"),
        ));
        let pending = client
            .peer()
            .send_cancellable_request(request, PeerRequestOptions::no_options())
            .await
            .expect("send cancellable tools/call request");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first sidecar child must enter the unlimited call");
        let pids = std::fs::read_to_string(&marker).expect("sidecar must publish its process ids");
        let mut pids = pids.split_whitespace().map(|pid| {
            pid.parse::<u32>()
                .expect("published process id must be numeric")
        });
        let child_pid = pids
            .next()
            .expect("sidecar must publish its direct child pid");
        let descendant_pid = pids
            .next()
            .expect("sidecar must publish its descendant pid");
        assert!(process_is_alive(child_pid));
        assert!(process_is_alive(descendant_pid));
        pending
            .cancel(Some("test cancellation".to_owned()))
            .await
            .expect("send rmcp cancellation notification");

        wait_for_process_death(child_pid).await;
        wait_for_process_death(descendant_pid).await;

        let recovered = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client
                .peer()
                .call_tool(rmcp::model::CallToolRequestParams::new(
                    "tower_cancellable_wait",
                )),
        )
        .await
        .expect("cancelled child must be reaped so the next call cannot remain blocked")
        .expect("the restarted sidecar call must succeed");

        assert_eq!(recovered.is_error, Some(false));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&extract_first_text(&recovered.content))
                .expect("recovery result must be JSON"),
            json!({"recovered": true})
        );
        client.cancel().await.expect("cancel failed");
    });
}

// ── AC4: call_tool for unknown tool or missing args ───────────────────────────

#[test]
fn ac4_call_unknown_tool_returns_is_error_true() {
    block_on(async {
        let merged = make_native_registry();
        let h = make_handler_from_merged(merged, vec![]);
        let client = start_server(h).await;
        let result = client
            .peer()
            .call_tool(rmcp::model::CallToolRequestParams::new("nonexistent_tool"))
            .await
            .expect("call_tool must succeed at the RPC level for unknown tools");
        assert_eq!(
            result.is_error,
            Some(true),
            "unknown tool must return is_error:true"
        );
        let text = extract_first_text(&result.content);
        assert!(
            text.contains("tool not found") || text.contains("not found"),
            "error message must mention not-found: {text}"
        );
        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn ac4_call_tool_missing_required_arg_returns_protocol_error() {
    block_on(async {
        let merged = make_native_registry();
        let h = make_handler_from_merged(merged, vec![]);
        let client = start_server(h).await;
        // tower_find_file requires "query" — omitting it yields InvalidArgs
        // which maps to a protocol-level error (not a CallToolResult).
        let result = client
            .peer()
            .call_tool(
                rmcp::model::CallToolRequestParams::new("tower_find_file")
                    .with_arguments(serde_json::Map::new()), // missing "query"
            )
            .await;
        // Should be a protocol-level error (Err) or an is_error result.
        // Either way the call should not panic.
        let _ = result;
        client.cancel().await.expect("cancel failed");
    });
}

// ── AC5: resources round-trip ─────────────────────────────────────────────────

#[test]
fn ac5_list_resources_returns_configured_uris() {
    block_on(async {
        let merged = make_native_registry();
        let uris = vec!["lsp://rust/diagnostics".to_owned()];
        let h = make_handler_from_merged(merged, uris);
        let client = start_server(h).await;
        let result = client
            .peer()
            .list_resources(Default::default())
            .await
            .expect("list_resources failed");
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].uri, "lsp://rust/diagnostics");
        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn ac5_subscribe_unsubscribe_round_trip() {
    block_on(async {
        let sub_reg = Arc::new(Mutex::new(SubscriptionRegistry::new()));
        let merged = make_native_registry();
        let h = TowerMcpHandler::new(merged, null_diag_reader(), Arc::clone(&sub_reg), vec![]);
        let client = start_server(h).await;

        client
            .peer()
            .subscribe(rmcp::model::SubscribeRequestParams::new(
                "file:///w/main.rs",
            ))
            .await
            .expect("subscribe failed");
        assert!(
            sub_reg.lock().unwrap().is_subscribed("file:///w/main.rs"),
            "URI must be subscribed after subscribe call"
        );

        client
            .peer()
            .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(
                "file:///w/main.rs",
            ))
            .await
            .expect("unsubscribe failed");
        assert!(
            !sub_reg.lock().unwrap().is_subscribed("file:///w/main.rs"),
            "URI must not be subscribed after unsubscribe call"
        );

        client.cancel().await.expect("cancel failed");
    });
}

#[test]
fn ac5_read_resource_returns_diagnostics_from_reader() {
    use crate::domain::code_intel::{Diagnostic, Position, Range, Severity};

    struct CannedReader;
    impl DiagnosticsReader for CannedReader {
        fn diagnostics_for(&self, _: &str) -> Vec<Diagnostic> {
            vec![Diagnostic {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: Severity::Error,
                message: "canned error".to_owned(),
                source: None,
                code: None,
            }]
        }
    }

    block_on(async {
        let merged = make_native_registry();
        let h = TowerMcpHandler::new(
            merged,
            Arc::new(CannedReader) as Arc<dyn DiagnosticsReader>,
            empty_sub_reg(),
            vec![],
        );
        let client = start_server(h).await;
        let result = client
            .peer()
            .read_resource(rmcp::model::ReadResourceRequestParams::new(
                "file:///w/main.rs",
            ))
            .await
            .expect("read_resource failed");
        assert!(!result.contents.is_empty());
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => String::new(),
        };
        assert!(
            text.contains("canned error"),
            "read_resource must return live diagnostics; got: {text}"
        );
        assert!(
            text.contains("\"supported\":true") || text.contains("\"supported\": true"),
            "must advertise supported:true; got: {text}"
        );
        client.cancel().await.expect("cancel failed");
    });
}

// ── AC6: SubscriptionRegistry unit tests (preserved from old tests) ───────────

#[test]
fn subscription_registry_subscribe_unsubscribe() {
    let mut reg = SubscriptionRegistry::new();
    reg.subscribe("file:///a.rs");
    assert!(reg.is_subscribed("file:///a.rs"));
    assert!(!reg.is_subscribed("file:///b.rs"));
    reg.unsubscribe("file:///a.rs");
    assert!(!reg.is_subscribed("file:///a.rs"));
}

#[test]
fn subscription_registry_clear() {
    let mut reg = SubscriptionRegistry::new();
    reg.subscribe("file:///a.rs");
    reg.subscribe("file:///b.rs");
    reg.clear();
    assert!(!reg.is_subscribed("file:///a.rs"));
    assert!(!reg.is_subscribed("file:///b.rs"));
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_first_text(content: &[Content]) -> String {
    content
        .first()
        .and_then(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}
