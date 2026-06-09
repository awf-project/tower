//! Ports layer — the hexagon boundary between domain and infrastructure.
//!
//! # Architecture
//!
//! ```text
//! INBOUND (Driving API — consumers call these)
//! ┌──────────────────────────────────────────────────────────────┐
//! │ trait SearchUseCase {                                         │
//! │   fn find_file(&self, query: &str)      -> Result<Vec<Path>> │
//! │   fn search_text(&self, pattern: &str)  -> Result<Vec<Match>>│
//! │ }                                                             │
//! │ trait FileMutationUseCase {                                   │
//! │   fn create_file(…) / create_directory(…) / delete_file(…)  │
//! │   fn global_replace(…)                  -> Result<TxReport>  │
//! │ }                                                             │
//! └──────────────────────────────────────────────────────────────┘
//! OUTBOUND (Driven SPI — domain requires these)
//! ┌──────────────────────────────────────────────────────────────┐
//! │ trait StoragePort    { get / put / delete VirtualFile + blobs}│
//! │ trait FileSystemPort { read / write / rename / scan bytes }  │
//! │ trait PluginHostPort { on_file_indexed / on_file_changed }   │
//! └──────────────────────────────────────────────────────────────┘
//! TEST DOUBLES (spec 02):
//!   InMemoryStorage : HashMap-backed StoragePort
//!   InMemoryFs      : HashMap<path,bytes> FileSystemPort (atomic rename)
//! ```
//!
//! # Dependency inversion (U1)
//!
//! The domain never imports a concrete adapter. It receives port implementations
//! through its constructor (or method parameters), ensuring that `domain/` stays
//! free of `sled`, `std::fs`, `wasmtime`, and `notify` (AC4).

pub mod error;
pub mod filesystem;
pub mod plugin;
pub mod storage;

// Inbound ports — use-case contracts called by external drivers.
pub mod inbound;

pub use error::PortError;
pub use filesystem::FileSystemPort;
pub use inbound::{FileMutationUseCase, Match, SearchUseCase, TxReport};
pub use plugin::{NoOpPluginHost, PluginHostPort};
pub use storage::StoragePort;
