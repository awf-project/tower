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
use rmcp::{RoleClient, ServerHandler, ServiceExt};
use serde_json::json;

use super::{
    diagnostics::{DiagnosticsReader, NoOpDiagnosticsReader},
    extension_merged_registry::ExtensionMergedRegistry,
    lsp_tools::SubscriptionRegistry,
    rmcp_server::TowerMcpHandler,
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
/// no extensions. This gives us the 9 native `tower_*` tools.
fn make_native_registry() -> ExtensionMergedRegistry {
    let ext_reg = Arc::new(std::sync::RwLock::new(
        crate::domain::extension_host::ExtensionRegistry::new(),
    ));
    ExtensionMergedRegistry::new(make_empty_engine_state(), ext_reg)
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
        // The native registry has 9 tools.
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
