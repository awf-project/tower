//! MCP inbound adapter.
//!
//! JSON-RPC 2.0 over stdio, served via the [`rmcp`] SDK. This module contains
//! the protocol plumbing and the tool-registry abstraction — it is
//! deliberately tool-agnostic.
//!
//! # Wireframe
//!
//! ```text
//!  stdin ─► rmcp transport ─► TowerMcpHandler
//!                                │ registry.list()  → tools/list response
//!                                │ registry.call()  → tools/call response
//!                                ▼
//!                 ToolRegistry (trait) ─► ExtensionMergedRegistry
//!                   (native tools in 10b; extension tools appended in 28)
//! ```
//!
//! # What is NOT here
//!
//! - The native tool handlers — those are spec 10b (`native_tools`).
//! - Extension tool merging — spec 28 (`ExtensionMergedRegistry`).
//! - `tokio::io::stdin()` / `tokio::io::stdout()` — the binary entry wires
//!   those in `main.rs`. The rmcp transport is generic over async I/O.
//!
//! # Registry seam
//!
//! The [`ToolRegistry`] trait is the single surface shared by all tool sources:

pub mod chain_registry;
pub mod diagnostics;
pub mod diagnostics_json;
pub mod extension_merged_registry;
pub mod lsp_support;
pub mod lsp_tools;
pub mod native_tools;
pub mod nav_tools;
pub mod project_query;
pub mod registry;
pub mod rmcp_server;
pub mod types;

pub use diagnostics::{DiagnosticsReader, NoOpDiagnosticsReader, PushEvent};
pub use extension_merged_registry::ExtensionMergedRegistry;
pub use native_tools::{EngineState, NativeToolRegistry};
pub use project_query::{
    EXTERNAL_PROJECT_UNAVAILABLE, EXTERNAL_QUERY_CANCELLED, EXTERNAL_READ_ONLY_BOUNDARY,
    EXTERNAL_TARGET_FAILURE, EXTERNAL_TARGET_RESPONSE_INVALID, ExternalQueryRequest,
    ExternalQueryResult, ProjectListResult, ProjectQueryRegistry, ProjectSelector,
    QueryableProjectListing,
};
pub use registry::ToolRegistry;
pub use rmcp_server::{TowerMcpHandler, spawn_push_task};
pub use types::{ToolDesc, ToolError};

#[cfg(test)]
mod tests;
