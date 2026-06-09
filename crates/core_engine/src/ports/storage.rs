//! `StoragePort` — outbound port for persisting domain entities and raw blobs.
//!
//! # Contract
//!
//! Implementors must satisfy the behavioral contract checked by
//! [`crate::test_support::storage_contract`] tests:
//!
//! - **put → get round-trip**: a file inserted with `put` is returned verbatim
//!   by `get` using the same `FileId`.
//! - **delete removes**: after `delete`, `get` returns `PortError::NotFound`.
//! - **get on unknown key**: returns `PortError::NotFound`.
//! - **blob round-trip**: bytes stored via `put_blob` are returned verbatim by
//!   `get_blob` for the same `ContentHash`.
//!
//! The domain never sees a concrete backend; it receives an `&dyn StoragePort`
//! (or `&mut dyn StoragePort` for mutation) and talks exclusively through this
//! interface (U1 / EV1).

use crate::domain::{ContentHash, FileId, VirtualFile};
use crate::ports::PortError;

/// Persistence contract for `VirtualFile` entities and raw content blobs.
///
/// # Object safety
///
/// This trait is object-safe so it can be used as `dyn StoragePort` where
/// dynamic dispatch is needed (e.g. wiring domain services at runtime).
pub trait StoragePort {
    /// Retrieve a `VirtualFile` by its `FileId`.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::NotFound`] if no entry exists for `id`.
    fn get(&self, id: FileId) -> Result<VirtualFile, PortError>;

    /// Persist a `VirtualFile`. Overwrites any existing entry for the same id.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::WriteFailed`] if the backing store rejects the
    /// write (e.g. serialization failure, out of space).
    fn put(&mut self, file: VirtualFile) -> Result<(), PortError>;

    /// Remove the entry for `id`.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::NotFound`] if `id` was not present.
    fn delete(&mut self, id: FileId) -> Result<(), PortError>;

    /// Store a raw content blob indexed by its `ContentHash`.
    ///
    /// Implementations are free to deduplicate blobs (content-addressed
    /// storage).
    ///
    /// # Errors
    ///
    /// Returns [`PortError::WriteFailed`] on failure.
    fn put_blob(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), PortError>;

    /// Retrieve a raw content blob by its `ContentHash`.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::NotFound`] if no blob was stored for `hash`.
    fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, PortError>;
}
