//! Domain layer — pure business logic, the Ubiquitous Language as Rust types.
//!
//! Invariant: no `sled`, `fs`, or `notify` imports here. Everything
//! in this module is constructible and assertable without any I/O (spec U3/AC4).
#![forbid(unsafe_code)]

pub mod code_intel;
pub mod error;
/// Extension host registry — runtime-agnostic registry implementing
/// `ExtensionHostPort` (spec 22). Stores `ExtensionInstance` trait objects,
/// fans out workspace events, routes tool calls, and enforces quarantine policy.
pub mod extension_host;
pub mod file_id;
pub mod grep;
pub mod index;
pub mod mutation;
pub mod refactor;
pub mod token;
pub mod virtual_file;
pub mod workspace;

pub use error::DomainError;
pub use extension_host::{ExtensionId, ExtensionInstance, ExtensionRegistry, InvokeError};
pub use file_id::FileId;
pub use virtual_file::{ContentHash, FileMetadata, RelativePath, Timestamp, VirtualFile};
pub use workspace::{ProjectWorkspace, WorkspaceSnapshot};
