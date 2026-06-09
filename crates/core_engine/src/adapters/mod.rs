//! Adapters layer — concrete implementations of port traits.
//!
//! In-memory test doubles are always compiled in (used by contract test
//! macros). Real infrastructure adapters (sled, std::fs, wasmtime, notify)
//! land in specs 04 / 05 / 11 and are conditionally compiled.

pub mod fs;
pub mod in_memory_fs;
pub mod in_memory_storage;
pub mod storage;

pub use fs::RealFs;
pub use in_memory_fs::InMemoryFs;
pub use in_memory_storage::InMemoryStorage;
pub use storage::SledStorageAdapter;

#[cfg(test)]
mod contract_tests;
