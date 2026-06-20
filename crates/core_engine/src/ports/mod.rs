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
//! │ trait StoragePort      { get / put / delete VirtualFile + blobs }│
//! │ trait FileSystemPort   { read / write / rename / scan bytes }    │
//! │ trait ExtensionHostPort{ on_file_indexed / on_file_changed }     │
//! │ trait AstIndexPort     { put / get / delete / list blobs }       │
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
//! free of `sled`, `std::fs`, and `notify` (AC4).

pub mod ast_index;
pub mod code_intel;
pub mod document_sync;
pub mod error;
pub mod extension_host;
pub mod filesystem;
pub mod navigation;
pub mod storage;

// Inbound ports — use-case contracts called by external drivers.
pub mod inbound;

pub use ast_index::{AstIndexPort, validate_key as validate_ast_index_key};
pub use code_intel::{CodeIntelError, CodeIntelligencePort};
pub use document_sync::{DocumentSyncPort, NoOpDocumentSync};
pub use error::PortError;
pub use extension_host::{ExtensionHostPort, NoOpExtensionHost};
pub use filesystem::FileSystemPort;
pub use inbound::{FileMutationUseCase, FileReplaceError, Match, SearchUseCase, TxReport};
pub use navigation::NavigationPort;
pub use storage::StoragePort;
