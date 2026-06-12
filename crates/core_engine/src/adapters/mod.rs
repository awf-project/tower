//! Adapters layer — concrete implementations of port traits.
//!
//! In-memory test doubles are always compiled in (used by contract test
//! macros). Real infrastructure adapters (sled, std::fs, wasmtime, notify)
//! land in specs 04 / 05 / 11 and are conditionally compiled.

/// XDG-backed AST index adapter and workspace path helpers (Stage 3a).
///
/// Outbound port: [`crate::ports::AstIndexPort`].
/// Real adapter: [`ast_index::XdgAstIndexAdapter`].
/// In-memory fake: [`InMemoryAstIndex`].
pub mod ast_index;
pub mod fs;
pub mod in_memory_ast_index;
pub mod in_memory_fs;
pub mod in_memory_storage;
/// MCP JSON-RPC 2.0 stdio transport (spec 10a).
///
/// Protocol plumbing only — tool handlers live in spec 10b, plugin tool
/// merging in spec 12b. The [`mcp::ToolRegistry`] trait is the seam.
pub mod mcp;
/// Wasmtime plugin loader and capability linker (spec 11c).
///
/// Security boundary: exposes only the explicit `tower_host` allow-list to
/// wasm guests. WASI is linked with a zero-capability context (no preopened
/// dirs, no network). All other host imports cause a link error.
pub mod plugin;
pub mod storage;
pub mod watcher;

pub use ast_index::XdgAstIndexAdapter;
pub use fs::RealFs;
pub use in_memory_ast_index::InMemoryAstIndex;
pub use in_memory_fs::InMemoryFs;
pub use in_memory_storage::InMemoryStorage;
pub use plugin::{
    HostDeps, IsolatedSandbox, IsolationConfig, IsolationEngine, MAX_CONSECUTIVE_FAILURES,
    PluginLoadError, WasmtimeHost,
};
// Note: WasmInstance is intentionally not re-exported — it is pub(crate) to
// keep the concrete wasmtime runtime type behind the hexagonal adapter boundary.
pub use storage::SledStorageAdapter;
pub use watcher::NotifyWatcherAdapter;

#[cfg(test)]
mod contract_tests;
