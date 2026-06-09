//! Sled-backed storage adapter (specs 04a + 04b).
//!
//! # Module layout
//!
//! ```text
//! storage/
//! ├── mod.rs          — public re-exports
//! ├── adapter.rs      — SledStorageAdapter + StoragePort impl
//! ├── error.rs        — StorageError (lifecycle errors, mapped to PortError)
//! └── keys.rs         — deterministic key encoding/decoding helpers
//! ```
//!
//! # Invariants
//!
//! - No `unsafe` code.
//! - Domain types (`VirtualFile`, `FileId`, …) are serialised with `postcard`.
//! - `sled` is never imported in `domain/` (verified by code review and the
//!   absence of `sled` in `domain/`'s import graph).
//! - All `put`/`delete` mutations touch `files`, `paths`, `meta`, and `index`
//!   in ONE atomic Sled transaction (spec 04a EV2 + spec 04b EV2/UN1).

mod adapter;
mod error;
mod keys;

pub use adapter::{SledStorageAdapter, SCHEMA_VERSION};
pub use error::StorageError;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_index;
