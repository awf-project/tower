//! Domain layer — pure business logic, the Ubiquitous Language as Rust types.
//!
//! Invariant: no `sled`, `fs`, `wasmtime`, or `notify` imports here. Everything
//! in this module is constructible and assertable without any I/O (spec U3/AC4).
#![forbid(unsafe_code)]

pub mod error;
pub mod file_id;
pub mod grep;
pub mod index;
pub mod mutation;
/// Plugin host registry — runtime-agnostic registry implementing `PluginHostPort`
/// (spec 11b). Stores `PluginInstance` trait objects, fans out lifecycle hooks,
/// and aggregates declared tools for MCP merge (12b).
pub mod plugin_host;
pub mod refactor;
pub mod token;
pub mod virtual_file;
pub mod workspace;

pub use error::DomainError;
pub use file_id::FileId;
pub use plugin_host::{
    PluginFaultKind, PluginHostError, PluginHostRegistry, PluginId, PluginInstance,
    RegistrationError,
};
pub use virtual_file::{ContentHash, FileMetadata, RelativePath, Timestamp, VirtualFile};
pub use workspace::{ProjectWorkspace, WorkspaceSnapshot};
