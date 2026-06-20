//! `ExtensionHostPort` — outbound port for the extension host registry.
//!
//! # Declaration (spec 22 scope)
//!
//! This trait is declared here so the domain can hold a `&dyn ExtensionHostPort`
//! and dispatch workspace events and tool calls without coupling to any concrete
//! sidecar adapter. The real sidecar adapter lands in spec 23.
//!
//! # No-op implementation (AC6 / OP1)
//!
//! [`NoOpExtensionHost`] satisfies the trait with empty bodies. It is wired when
//! no real extension host is active, making extension hooks truly optional.
//!
//! # Hexagonal boundary
//!
//! This file contains only trait declarations and a zero-cost no-op struct.
//! It imports nothing from infrastructure crates (`sled`, `std::fs`, `wasmtime`,
//! `notify`, `std::process`). The domain can depend on this safely.

use extension_protocol::ToolDecl;
use serde_json::Value;

use crate::domain::extension_host::{ExtensionId, InvokeError};
use crate::domain::{FileId, RelativePath};

/// Extension host lifecycle hook and tool-routing contract.
///
/// The domain calls these methods to:
/// - broadcast workspace events to subscribed extensions (EV2),
/// - aggregate declared tools for MCP merge (U1),
/// - route tool invocations to the owning extension (U2).
///
/// Implementations must be infallible from the domain's perspective for the
/// event delivery methods: faults are isolated per-extension internally. Tool
/// routing returns a `Result` so callers can surface errors to MCP clients.
///
/// # Object safety
///
/// The trait is object-safe so the domain can hold `&dyn ExtensionHostPort`
/// without monomorphisation.
pub trait ExtensionHostPort {
    /// Called after a file has been indexed (content hash computed and stored).
    ///
    /// The implementation fans out `event/fileIndexed` to all subscribed
    /// extension instances.
    fn on_file_indexed(&self, id: FileId, path: &RelativePath);

    /// Called after a file's content has changed (re-indexed after a write).
    ///
    /// The implementation fans out `event/fileChanged` to all subscribed
    /// extension instances.
    fn on_file_changed(&self, id: FileId, path: &RelativePath);

    /// Called after a file has been deleted from the workspace (spec 27 EV1).
    ///
    /// The implementation fans out `event/fileDeleted` to all subscribed
    /// extension instances. Extensions that track open documents (e.g. the LSP
    /// extension) issue `textDocument/didClose` to the appropriate language
    /// server on receipt.
    fn on_file_deleted(&self, path: &RelativePath);

    /// Return all tools declared by registered extensions, each tagged by
    /// extension identity (U1).
    ///
    /// The MCP merge layer (spec 28) consumes this to expose extension tools
    /// alongside native tools, prefixing each as `tower_<ext>_<tool>`.
    fn declared_tools(&self) -> Vec<(ExtensionId, ToolDecl)>;

    /// Invoke a named tool, routing to the owning extension instance (U2).
    ///
    /// `tool_name` is the bare tool name (without the `tower_<ext>_` prefix).
    ///
    /// # Errors
    ///
    /// - [`InvokeError::ToolNotFound`] — no extension owns `tool_name`.
    /// - [`InvokeError::Fault`] — the owning extension returned a fault or is quarantined.
    fn invoke(&self, tool_name: &str, params: Value) -> Result<Value, InvokeError>;
}

/// A no-op [`ExtensionHostPort`] that discards all calls (OP1 / AC6).
///
/// Wire this when no real extension runtime is active. Behaviour must be
/// identical to having no extensions registered (AC6).
///
/// # Example
///
/// ```rust
/// use core_engine::ports::NoOpExtensionHost;
/// use core_engine::ports::ExtensionHostPort;
///
/// let host = NoOpExtensionHost;
/// // All calls are silently discarded / return empty results.
/// assert!(host.declared_tools().is_empty());
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpExtensionHost;

impl ExtensionHostPort for NoOpExtensionHost {
    #[inline]
    fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

    #[inline]
    fn on_file_changed(&self, _id: FileId, _path: &RelativePath) {}

    #[inline]
    fn on_file_deleted(&self, _path: &RelativePath) {}

    #[inline]
    fn declared_tools(&self) -> Vec<(ExtensionId, ToolDecl)> {
        Vec::new()
    }

    #[inline]
    fn invoke(&self, tool_name: &str, _params: Value) -> Result<Value, InvokeError> {
        Err(InvokeError::ToolNotFound(tool_name.to_owned()))
    }
}
