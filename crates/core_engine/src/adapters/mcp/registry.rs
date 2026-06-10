//! `ToolRegistry` — the seam between the MCP transport and tool implementations.
//!
//! # Design (spec 10a U2)
//!
//! The trait is the **only** connection point between the JSON-RPC dispatcher
//! (this crate) and the tool business logic. Keeping it as a trait means:
//!
//! - **10b** can implement `ToolRegistry` with the 7 native workspace tools.
//! - **12b** can wrap a `Box<dyn ToolRegistry>` (or compose two registries)
//!   to merge plugin tools into the same surface.
//!
//! Neither the transport nor the domain need to change when new tools are added.
//!
//! # Object safety
//!
//! The trait is object-safe: `list` takes `&self`, `call` takes `&mut self`.
//! Both return owned values, so there are no lifetime or generic parameters on
//! the return types. A `Box<dyn ToolRegistry>` is a valid type.
//!
//! # Example (stub for tests)
//!
//! ```rust
//! use core_engine::adapters::mcp::{ToolRegistry, ToolDesc, ToolError};
//! use serde_json::Value;
//!
//! struct EmptyRegistry;
//!
//! impl ToolRegistry for EmptyRegistry {
//!     fn list(&self) -> Vec<ToolDesc> { vec![] }
//!     fn call(&mut self, name: &str, _args: Value) -> Result<Value, ToolError> {
//!         Err(ToolError::NotFound(name.to_owned()))
//!     }
//! }
//! ```

use serde_json::Value;

use super::types::{ToolDesc, ToolError};

/// The unified registry surface shared by native (10b) and plugin (12b) tools.
///
/// # Seam contract
///
/// - `list` — return every tool this registry knows about. The transport calls
///   this once per `tools/list` request; no caching is assumed.
/// - `call` — look up `name`, execute with `args`, return a JSON `Value` on
///   success or a [`ToolError`] variant on failure.
///
/// Implementors MUST NOT panic. Any runtime failure must be returned as
/// [`ToolError::ExecutionFailed`].
pub trait ToolRegistry {
    /// Return a description of every tool in this registry.
    ///
    /// An empty `Vec` is valid (AC2: empty registry returns empty list).
    fn list(&self) -> Vec<ToolDesc>;

    /// Dispatch `args` to the tool named `name`.
    ///
    /// # Errors
    ///
    /// - [`ToolError::NotFound`] — `name` is not registered here.
    /// - [`ToolError::InvalidArgs`] — `args` are structurally wrong.
    /// - [`ToolError::ExecutionFailed`] — tool ran but encountered a runtime error.
    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError>;
}
