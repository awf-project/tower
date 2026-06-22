//! rmcp [`ServerHandler`] implementation — replaces the hand-rolled JSON-RPC
//! transport loop (spec 10a migration to rmcp 1.7).
//!
//! # Design
//!
//! [`TowerMcpHandler`] wraps the existing [`ExtensionMergedRegistry`] (which
//! provides native + sidecar-extension tools) behind a `Mutex` so the async
//! handler methods can mutably borrow it without holding a synchronous lock
//! across await points. All registry calls are short-lived synchronous CPU
//! work — no blocking I/O — so wrapping in a plain `std::sync::Mutex` (and
//! taking the lock only for the duration of the registry call) keeps the
//! critical section small and avoids the overhead of a tokio Mutex.
//!
//! # Resource subscription
//!
//! Subscription state lives in [`SubscriptionRegistry`] (unchanged from the
//! old transport) and is shared behind an `Arc<Mutex<…>>`. When a
//! `PushEvent` arrives on the mpsc channel the push-forwarding task calls
//! `peer.notify_resource_updated` — rmcp handles the framing and
//! serialisation.
//!
//! # Hexagonal boundary
//!
//! This module is an **inbound adapter**: it imports rmcp types and the
//! existing registry / subscription types that also live in the `adapters`
//! layer. The `domain/` package is never imported here.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use rmcp::ErrorData as McpError;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, RawResource, ReadResourceRequestParams,
    ReadResourceResult, Resource, ResourceContents, ResourceUpdatedNotificationParam,
    ResourcesCapability, ServerCapabilities, ServerInfo, SubscribeRequestParams, Tool,
    UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{Peer, ServerHandler};
use serde_json::Value;

use crate::adapters::mcp::diagnostics::{DiagnosticsReader, PushEvent};
use crate::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use crate::adapters::mcp::lsp_tools::SubscriptionRegistry;
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};

// ── TowerMcpHandler ───────────────────────────────────────────────────────────

/// rmcp [`ServerHandler`] for the Tower MCP server.
///
/// Wraps the merged tool registry (native + sidecar-extension tools), the
/// diagnostics reader, the resource subscription registry, and the static
/// list of resource URIs advertised to clients.
///
/// # Thread safety
///
/// The struct is `Send + Sync` (required by `ServerHandler`). All mutable
/// state is accessed through `Mutex` guards whose critical sections contain
/// only non-blocking CPU work.
pub struct TowerMcpHandler {
    /// The unified tool registry (native tower_* + sidecar extension tools).
    /// Wrapped in a plain Mutex: registry calls are sync/non-blocking.
    registry: Mutex<ExtensionMergedRegistry>,

    /// Diagnostics provider for resources/read. No-op in the extension model.
    diag_reader: Arc<dyn DiagnosticsReader>,

    /// Subscription state: URIs the client has subscribed to.
    sub_reg: Arc<Mutex<SubscriptionRegistry>>,

    /// Static resource URIs advertised via resources/list.
    /// Empty in the extension model (resources are pushed by extensions).
    resource_uris: Vec<String>,
}

impl TowerMcpHandler {
    /// Construct a new handler.
    ///
    /// - `registry`      — the merged tool registry; moves in.
    /// - `diag_reader`   — for `resources/read`; typically a no-op.
    /// - `sub_reg`       — shared subscription state.
    /// - `resource_uris` — static list of resource URIs for `resources/list`.
    #[must_use]
    pub fn new(
        registry: ExtensionMergedRegistry,
        diag_reader: Arc<dyn DiagnosticsReader>,
        sub_reg: Arc<Mutex<SubscriptionRegistry>>,
        resource_uris: Vec<String>,
    ) -> Self {
        Self {
            registry: Mutex::new(registry),
            diag_reader,
            sub_reg,
            resource_uris,
        }
    }
}

// ── ServerHandler impl ────────────────────────────────────────────────────────

impl ServerHandler for TowerMcpHandler {
    /// Advertise server identity and capabilities.
    ///
    /// Tools and resources (with subscribe enabled) are advertised. rmcp
    /// handles protocol-version negotiation — we never need to echo it back.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources_with(ResourcesCapability {
                    subscribe: Some(true),
                    list_changed: Some(false),
                })
                .build(),
        )
        .with_server_info(Implementation::new("tower", env!("CARGO_PKG_VERSION")))
    }

    /// `tools/list` — delegate to the merged registry.
    async fn list_tools(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .list();
        let rmcp_tools: Vec<Tool> = tools.into_iter().map(tool_desc_to_rmcp).collect();
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: rmcp_tools,
        })
    }

    /// `tools/call` — dispatch to the registry and map errors.
    ///
    /// # Error strategy
    ///
    /// The MCP spec distinguishes protocol errors (bad request, unknown method)
    /// from tool execution errors. Tool execution errors are returned as
    /// `CallToolResult` with `is_error: true`; only `InvalidArgs` maps to a
    /// protocol-level `-32602 InvalidParams` error because it represents a
    /// malformed MCP request (not a tool fault).
    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = req.name.as_ref().to_owned();
        // arguments may be absent; default to empty object per MCP spec.
        let args: Value = req
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Object(serde_json::Map::new()));

        let result = self
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .call(&name, args);

        match result {
            Ok(value) => {
                // Preserve the existing wire shape: text content wrapping the
                // serialised JSON value (matches the pre-migration behaviour
                // and what Codex and other clients expect).
                Ok(CallToolResult::success(vec![Content::text(
                    value.to_string(),
                )]))
            }
            Err(ToolError::NotFound(msg)) => {
                // Application-level "tool not found". Encode as is_error:true
                // per MCP spec (tool-level error, not a protocol error).
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "tool not found: {msg}"
                ))]))
            }
            Err(ToolError::InvalidArgs(msg)) => {
                // Bad request structure — map to protocol-level invalid params.
                Err(McpError::invalid_params(msg, None))
            }
            Err(ToolError::ExecutionFailed(msg)) => Ok(CallToolResult::error(vec![Content::text(
                format!("execution failed: {msg}"),
            )])),
            Err(ToolError::ResourceNotFound(msg)) => {
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "resource not found: {msg}"
                ))]))
            }
            Err(ToolError::PreconditionFailed(msg)) => {
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "precondition failed: {msg}"
                ))]))
            }
        }
    }

    /// `resources/list` — return the static list of configured resource URIs.
    async fn list_resources(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let resources: Vec<Resource> = self
            .resource_uris
            .iter()
            .map(|uri| {
                Resource::new(
                    RawResource::new(uri.clone(), uri.clone()).with_mime_type("application/json"),
                    None,
                )
            })
            .collect();
        Ok(ListResourcesResult {
            meta: None,
            next_cursor: None,
            resources,
        })
    }

    /// `resources/read` — return diagnostics for the requested URI.
    ///
    /// The diagnostics are serialised as a JSON text payload. No-op
    /// `DiagnosticsReader` (the default in the extension model) always returns
    /// an empty list, so the response is `{supported:true, diagnostics:[]}`.
    async fn read_resource(
        &self,
        req: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = req.uri.clone();
        let diags = self.diag_reader.diagnostics_for(&uri);
        let diags_json: Vec<Value> = diags
            .iter()
            .map(crate::adapters::mcp::lsp_tools::diagnostic_to_json)
            .collect();
        let payload = serde_json::json!({
            "supported": true,
            "uri": uri,
            "diagnostics": diags_json
        });
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            payload.to_string(),
            uri,
        )]))
    }

    /// `resources/subscribe` — record the URI in the subscription registry.
    async fn subscribe(
        &self,
        req: SubscribeRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.sub_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribe(&req.uri);
        Ok(())
    }

    /// `resources/unsubscribe` — remove the URI from the subscription registry.
    async fn unsubscribe(
        &self,
        req: UnsubscribeRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.sub_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unsubscribe(&req.uri);
        Ok(())
    }
}

// ── Conversion helpers ────────────────────────────────────────────────────────

/// Convert a [`ToolDesc`] (Tower's domain-facing descriptor) to rmcp's [`Tool`].
///
/// The `input_schema` on [`ToolDesc`] is a [`serde_json::Value`] expected to be
/// an object; we extract the inner map into the `Arc<JsonObject>` rmcp requires.
/// If the value is not an object (defensive), we fall back to an empty map.
fn tool_desc_to_rmcp(desc: ToolDesc) -> Tool {
    let schema_map = match desc.input_schema {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    Tool::new(desc.name, desc.description, Arc::new(schema_map))
}

// ── Push-forwarding task ──────────────────────────────────────────────────────

/// Spawn a background thread that forwards [`PushEvent`]s to the MCP client.
///
/// Each event is checked against the [`SubscriptionRegistry`]; only subscribed
/// URIs produce a `notifications/resources/updated` push. rmcp's
/// `Peer::notify_resource_updated` handles all framing.
///
/// The thread exits when `push_rx` is closed (sender dropped) or when the peer
/// disconnects (notification returns an error).
///
/// Decision: uses a plain OS thread rather than a `tokio::spawn` task because
/// `std::sync::mpsc::Receiver` is more naturally driven by a blocking `recv()`.
/// `tokio::runtime::Handle::block_on` bridges the async `notify_*` call from
/// the sync context without spinning up a second runtime.
pub fn spawn_push_task(
    peer: Peer<RoleServer>,
    sub_reg: Arc<Mutex<SubscriptionRegistry>>,
    push_rx: std::sync::mpsc::Receiver<PushEvent>,
) {
    let rt = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("mcp-push-forwarder".to_owned())
        .spawn(move || {
            while let Ok(event) = push_rx.recv() {
                let subscribed = sub_reg
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_subscribed(&event.uri);
                if subscribed {
                    let params = ResourceUpdatedNotificationParam::new(event.uri);
                    if rt.block_on(peer.notify_resource_updated(params)).is_err() {
                        // Peer disconnected — stop forwarding.
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn mcp-push-forwarder thread");
}
