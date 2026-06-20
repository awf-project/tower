//! MCP transport adapter (spec 10a).
//!
//! JSON-RPC 2.0 over stdio, newline-delimited framing. This module contains
//! only the **protocol plumbing** — it is deliberately tool-agnostic.
//!
//! # Wireframe
//!
//! ```text
//!  stdin ─► frame parser ─► JsonRpcRequest ─► Dispatcher
//!                                              │ registry.lookup(tool_name)
//!                                              ▼
//!                            ToolRegistry (trait) ─► result ─► JsonRpcResponse ─► stdout
//!  trait ToolRegistry { list() -> Vec<ToolDesc>; call(name, args) -> Result<Value, ToolError> }
//!    (native tools register here in 10b; extension tools appended in 28 — same surface)
//! ```
//!
//! # What is NOT here
//!
//! - The 9 native tool handlers — those are spec 10b.
//! - Extension tool merging — spec 28 (`ExtensionMergedRegistry`).
//! - `std::io::stdin()` / `std::io::stdout()` — the binary entry wires those
//!   in spec 10b. The `serve` function is generic over `R: BufRead, W: Write`.
//!
//! # Registry seam
//!
//! The [`ToolRegistry`] trait is the single surface shared by all tool sources:
//!
//! | Spec | Implementor | What it adds |
//! |------|-------------|--------------|
//! | 10b  | `NativeToolRegistry` | 9 workspace tools |
//! | 28   | `ExtensionMergedRegistry` | wraps 10b + extension tools |
//!
//! The transport only calls `registry.list()` and `registry.call()` — it never
//! needs to be changed when new tools arrive.

pub mod chain_registry;
pub mod extension_merged_registry;
pub mod lsp_support;
pub mod lsp_tools;
pub mod native_tools;
pub mod nav_tools;
pub mod registry;
pub mod transport;
pub mod types;

pub use extension_merged_registry::ExtensionMergedRegistry;
pub use native_tools::{EngineState, NativeToolRegistry};
pub use registry::ToolRegistry;
pub use transport::{PushEvent, serve, serve_with_push};
pub use types::{ToolDesc, ToolError};

#[cfg(test)]
mod tests;
