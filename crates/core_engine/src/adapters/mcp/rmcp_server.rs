//! rmcp [`ServerHandler`] implementation — replaces the hand-rolled JSON-RPC
//! transport loop (spec 10a migration to rmcp).
//!
//! # Design
//!
//! [`TowerMcpHandler`] wraps the existing [`ExtensionMergedRegistry`] (which
//! provides native + sidecar-extension tools) behind a `Mutex` so the async
//! handler methods can mutably borrow it without holding a synchronous lock
//! across await points. Tool dispatch can block on sidecars, so dispatch and
//! background inventory observation run on the blocking pool to keep the async
//! runtime available for cancellation notifications.
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
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents,
    ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig, SubscribeRequestParams,
    Tool, UnsubscribeRequestParams,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::{Peer, ServerHandler};
use serde_json::Value;

use crate::adapters::mcp::diagnostics::{DiagnosticsReader, PushEvent};
use crate::adapters::mcp::diagnostics_json::{DiagnosticJson, diagnostics_response_json};
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
/// The struct is `Send + Sync` (required by `ServerHandler`). Registry dispatch
/// holds a synchronous mutex on the blocking pool; background inventory reads
/// use that same pool so waiting for dispatch cannot stall cancellation.
pub struct TowerMcpHandler {
    /// The unified tool registry (native tower_* + sidecar extension tools).
    /// Wrapped in a plain Mutex: registry calls are sync/non-blocking.
    registry: Arc<Mutex<Box<dyn ToolRegistry + Send>>>,

    tool_list_changes: Mutex<Vec<tokio::sync::watch::Receiver<()>>>,
    advertised_tools: Arc<Mutex<Vec<ToolDesc>>>,
    tool_list_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,

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
    pub fn new<R>(
        registry: R,
        diag_reader: Arc<dyn DiagnosticsReader>,
        sub_reg: Arc<Mutex<SubscriptionRegistry>>,
        resource_uris: Vec<String>,
    ) -> Self
    where
        R: ToolRegistry + Send + 'static,
    {
        let tool_list_changes = registry.tool_list_changes();
        let advertised_tools = registry.list();
        Self {
            registry: Arc::new(Mutex::new(Box::new(registry))),
            tool_list_changes: Mutex::new(tool_list_changes),
            advertised_tools: Arc::new(Mutex::new(advertised_tools)),
            tool_list_tasks: Mutex::new(Vec::new()),
            diag_reader,
            sub_reg,
            resource_uris,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_list_tools(&self) -> Vec<ToolDesc> {
        self.registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .list()
    }

    #[cfg(test)]
    pub(crate) fn test_call_tool(
        &self,
        name: &str,
        args: Value,
        cancellation: &dyn crate::domain::CancellationSignal,
    ) -> Result<Value, ToolError> {
        self.registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .call(name, args, cancellation)
    }
}

// ── ServerHandler impl ────────────────────────────────────────────────────────

impl ServerHandler for TowerMcpHandler {
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let changes = std::mem::take(
            &mut *self
                .tool_list_changes
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        for mut changes in changes {
            let registry = Arc::clone(&self.registry);
            let advertised_tools = Arc::clone(&self.advertised_tools);
            let peer = context.peer.clone();
            let task = tokio::spawn(async move {
                while changes.changed().await.is_ok() {
                    let registry = Arc::clone(&registry);
                    let advertised_tools = Arc::clone(&advertised_tools);
                    // Dispatch can hold the registry during an unlimited call.
                    // Waiting for it must not block rmcp cancellation processing.
                    let changed = tokio::task::spawn_blocking(move || {
                        // Match tools/list's lock order and compare the whole schema,
                        // not only names. Lifecycle progress alone needs no push.
                        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                        let tools = registry.list();
                        let mut advertised =
                            advertised_tools.lock().unwrap_or_else(|p| p.into_inner());
                        if *advertised == tools {
                            false
                        } else {
                            *advertised = tools;
                            true
                        }
                    })
                    .await;
                    let Ok(changed) = changed else {
                        break;
                    };
                    if changed && peer.notify_tool_list_changed().await.is_err() {
                        break;
                    }
                }
            });
            self.tool_list_tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(task);
        }
    }

    /// Advertise server identity and capabilities.
    ///
    /// Tools and resources (with subscribe enabled) are advertised. rmcp
    /// handles protocol-version negotiation — we never need to echo it back.
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_subscribe()
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
        let registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
        let tools = registry.list();
        *self
            .advertised_tools
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = tools.clone();
        let rmcp_tools: Vec<Tool> = tools.into_iter().map(tool_desc_to_rmcp).collect();
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: rmcp_tools,
            ..Default::default()
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
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = req.name.as_ref().to_owned();
        // arguments may be absent; default to empty object per MCP spec.
        let args: Value = req
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Object(serde_json::Map::new()));

        // Registry dispatch is synchronous and extension calls may be
        // unlimited. Keep it off the async runtime so rmcp can process the
        // matching cancellation notification and cancel `ctx.ct`.
        let registry = Arc::clone(&self.registry);
        let result = tokio::task::spawn_blocking(move || {
            registry.lock().unwrap_or_else(|p| p.into_inner()).call(
                &name,
                args,
                &RmcpCancellationSignal(ctx),
            )
        })
        .await
        .map_err(|error| {
            McpError::internal_error(format!("tool dispatch task failed: {error}"), None)
        })?;

        match result {
            Ok(value) => {
                // Preserve the existing wire shape: text content wrapping the
                // serialised JSON value (matches the pre-migration behaviour
                // and what Codex and other clients expect).
                Ok(CallToolResult::success(vec![ContentBlock::text(value.to_string())]).into())
            }
            Err(error) => tool_error_to_rmcp(error).map(Into::into),
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
            .map(|uri| Resource::new(uri.clone(), uri.clone()).with_mime_type("application/json"))
            .collect();
        Ok(ListResourcesResult {
            meta: None,
            next_cursor: None,
            resources,
            ..Default::default()
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
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = req.uri.clone();
        let diags = self.diag_reader.diagnostics_for(&uri);
        let diagnostics: Vec<DiagnosticJson<'_>> = diags
            .iter()
            .map(|diagnostic| DiagnosticJson {
                path: None,
                diagnostic,
            })
            .collect();
        let mut payload = diagnostics_response_json(true, &diagnostics);
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("uri".to_owned(), Value::String(uri.clone()));
        }
        Ok(ReadResourceResult::new(vec![ResourceContents::text(payload.to_string(), uri)]).into())
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

impl Drop for TowerMcpHandler {
    fn drop(&mut self) {
        for task in self
            .tool_list_tasks
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            task.abort();
        }
    }
}

/// Convert an internal tool failure to its MCP protocol or tool-result shape.
///
/// Kept crate-internal so the conversion can be tested without manufacturing
/// an extension registry failure that is not wired until the T034 migration.
pub(super) fn tool_error_to_rmcp(error: ToolError) -> Result<CallToolResult, McpError> {
    match error {
        ToolError::NotFound(msg) => {
            // Application-level "tool not found". Encode as is_error:true
            // per MCP spec (tool-level error, not a protocol error).
            Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "tool not found: {msg}"
            ))]))
        }
        ToolError::InvalidArgs(msg) => {
            // Bad request structure — map to protocol-level invalid params.
            Err(McpError::invalid_params(msg, None))
        }
        ToolError::ExecutionFailed(msg) => Ok(CallToolResult::error(vec![ContentBlock::text(
            format!("execution failed: {msg}"),
        )])),
        ToolError::Cancelled => Ok(CallToolResult::error(vec![ContentBlock::text(
            "tool invocation cancelled",
        )])),
        ToolError::Application {
            code: _,
            message,
            data,
        } => {
            let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
            result.structured_content = data;
            Ok(result)
        }
        ToolError::ResourceNotFound(msg) => Ok(CallToolResult::error(vec![ContentBlock::text(
            format!("resource not found: {msg}"),
        )])),
        ToolError::PreconditionFailed(msg) => Ok(CallToolResult::error(vec![ContentBlock::text(
            format!("precondition failed: {msg}"),
        )])),
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
struct RmcpCancellationSignal(RequestContext<RoleServer>);

impl crate::domain::CancellationSignal for RmcpCancellationSignal {
    fn is_cancelled(&self) -> bool {
        self.0.ct.is_cancelled()
    }
}
