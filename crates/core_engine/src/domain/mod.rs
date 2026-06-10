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
pub mod token;
pub mod virtual_file;
pub mod workspace;

pub use error::DomainError;
pub use file_id::FileId;
pub use virtual_file::{ContentHash, FileMetadata, RelativePath, Timestamp, VirtualFile};
pub use workspace::{ProjectWorkspace, WorkspaceSnapshot};
