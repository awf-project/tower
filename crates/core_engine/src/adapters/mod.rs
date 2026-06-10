//! Adapters layer — concrete implementations of port traits.
//!
//! In-memory test doubles are always compiled in (used by contract test
//! macros). Real infrastructure adapters (sled, std::fs, wasmtime, notify)
//! land in specs 04 / 05 / 11 and are conditionally compiled.

pub mod fs;
pub mod in_memory_fs;
pub mod in_memory_storage;
/// MCP JSON-RPC 2.0 stdio transport (spec 10a).
///
/// Protocol plumbing only — tool handlers live in spec 10b, plugin tool
/// merging in spec 12b. The [`mcp::ToolRegistry`] trait is the seam.
pub mod mcp;
pub mod storage;
pub mod watcher;

pub use fs::RealFs;
pub use in_memory_fs::InMemoryFs;
pub use in_memory_storage::InMemoryStorage;
pub use storage::SledStorageAdapter;
pub use watcher::NotifyWatcherAdapter;

#[cfg(test)]
mod contract_tests;
