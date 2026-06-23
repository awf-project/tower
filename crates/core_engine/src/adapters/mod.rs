//! Adapters layer — concrete implementations of port traits.
//!
//! In-memory test doubles are always compiled in (used by contract test
//! macros). Real infrastructure adapters (sled, std::fs, notify)
//! land in specs 04 / 05 and are conditionally compiled.

/// XDG-backed AST index adapter and workspace path helpers (Stage 3a).
///
/// Outbound port: [`crate::ports::AstIndexPort`].
/// Real adapter: [`ast_index::XdgAstIndexAdapter`].
/// In-memory fake: [`InMemoryAstIndex`].
pub mod ast_index;
/// Binary CLI surface (clap, derive).
pub mod cli;
/// Local project configuration (`.tower/config.toml`).
///
/// Infra: reads `std::fs` and parses `toml`. The domain never sees it.
pub mod config;
/// Shared-daemon hub: one daemon per workspace owns the engine; `tower mcp`
/// clients relay stdio over a Unix socket. Pure infrastructure (sockets,
/// processes, framing) — the `domain/` crate is never imported here.
pub mod daemon;
/// Sidecar extension adapter — `SidecarHostAdapter` implementing
/// `ExtensionInstance` over a child process with JSON-RPC 2.0 over stdio (spec 23).
///
/// Capability callbacks are dispatched to `FileSystemPort`, `AstIndexPort`, and
/// `FormatQueuePort` — no privileged path, golden rule preserved.
pub mod extension;
/// Host-side format queue and worker pool (spec 13a).
///
/// Outbound capability surface: `host_request_format(path)` → `Accepted | Dropped`.
/// Adapter only (imports `std::fs`, `std::process::Command`); never imported by domain.
pub mod formatter;
pub mod fs;
pub mod in_memory_ast_index;
pub mod in_memory_code_intel;
pub mod in_memory_fs;
pub mod in_memory_storage;
/// MCP JSON-RPC 2.0 stdio transport (spec 10a).
///
/// Protocol plumbing only — tool handlers live in spec 10b, extension tool
/// merging in spec 28. The [`mcp::ToolRegistry`] trait is the seam.
pub mod mcp;
pub mod storage;
pub mod watcher;

pub use ast_index::XdgAstIndexAdapter;
pub use extension::{HostDeps as ExtensionHostDeps, SidecarHostAdapter};
pub use fs::RealFs;
pub use in_memory_ast_index::InMemoryAstIndex;
pub use in_memory_code_intel::InMemoryCodeIntel;
pub use in_memory_fs::InMemoryFs;
pub use in_memory_storage::InMemoryStorage;
pub use storage::SledStorageAdapter;
pub use watcher::NotifyWatcherAdapter;

#[cfg(test)]
mod contract_tests;
