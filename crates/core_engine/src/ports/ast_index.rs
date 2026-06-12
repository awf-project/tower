//! `AstIndexPort` — outbound port for per-workspace, persistent AST index blobs.
//!
//! # Purpose
//!
//! A future in-plugin AST index (Stage 4) will serialise a per-project symbol
//! index to raw bytes and call a host capability to persist it. The plugin
//! supplies only an opaque string key; the host maps that key to a path under
//! the per-workspace XDG data directory:
//!
//! ```text
//! $XDG_DATA_HOME/tower/workspace/<workspace_key>/ast/<key>.bin
//! ```
//!
//! The WASM guest never sees absolute machine paths — only the opaque `key`.
//!
//! # Contract
//!
//! Implementors must satisfy the behavioural contract verified by
//! [`crate::ast_index_contract_tests!`]:
//!
//! - **put → get round-trip**: bytes stored with `put` are returned verbatim by
//!   `get` for the same key.
//! - **get-miss** → `Ok(None)` (not `PortError::NotFound`).
//! - **overwrite** (put twice): the second value wins.
//! - **delete then get** → `Ok(None)`.
//! - **delete missing** → `Ok(())` (idempotent).
//! - **list** reflects the current set of keys after puts and deletes.
//! - **key validation**: empty key, keys containing `/`, `..`, or absolute
//!   markers return `PortError::InvalidArgs`.
//! - **binary safety**: values may contain any byte value including `0x00` and
//!   non-UTF-8 sequences.
//!
//! The domain never sees a concrete backend; it receives an `Arc<dyn AstIndexPort>`
//! and talks exclusively through this interface.

use crate::ports::PortError;

/// Persistence contract for opaque AST index blobs, keyed by a caller-supplied
/// string within a single workspace partition.
///
/// All methods take `&self` (not `&mut self`) so implementations can be shared
/// behind an `Arc` and called from concurrent contexts without external locking.
///
/// # Object safety
///
/// This trait is object-safe so it can be used as `dyn AstIndexPort` where
/// dynamic dispatch is needed.
///
/// # Key rules
///
/// Keys must be non-empty, must not contain `/`, and must not equal `..` or
/// start with `../`. Violations return [`PortError::InvalidArgs`].
pub trait AstIndexPort: Send + Sync {
    /// Store `bytes` under `key`, creating or overwriting any existing value.
    ///
    /// # Errors
    ///
    /// - [`PortError::InvalidArgs`] if `key` violates the key rules above.
    /// - [`PortError::WriteFailed`] if the backing store rejects the write.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), PortError>;

    /// Retrieve the bytes stored under `key`, or `Ok(None)` if absent.
    ///
    /// Returns `Ok(None)` — **not** `Err(PortError::NotFound)` — when the key
    /// is not present. Distinguishing "absent" from "I/O failure" is important
    /// for the cache-miss path.
    ///
    /// # Errors
    ///
    /// - [`PortError::InvalidArgs`] if `key` violates the key rules above.
    /// - [`PortError::ReadFailed`] on backing-store I/O errors.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, PortError>;

    /// Remove the entry for `key`.
    ///
    /// Idempotent: if the key was not present, returns `Ok(())` without error.
    ///
    /// # Errors
    ///
    /// - [`PortError::InvalidArgs`] if `key` violates the key rules above.
    /// - [`PortError::WriteFailed`] if the backing store rejects the removal.
    fn delete(&self, key: &str) -> Result<(), PortError>;

    /// Return all keys currently present in this partition.
    ///
    /// The order is unspecified; callers must not rely on insertion order.
    /// Returns an empty `Vec` when no keys are present.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::ReadFailed`] if the backing store cannot be read.
    fn list(&self) -> Result<Vec<String>, PortError>;
}

/// Validate that `key` satisfies the port's key rules.
///
/// Returns `Ok(())` when valid, `Err(PortError::InvalidArgs(_))` otherwise.
///
/// # Rules
///
/// - Must be non-empty.
/// - Must not contain a NUL byte (rejected by the OS at the filesystem layer;
///   caught here so it surfaces as `InvalidArgs`, not an opaque write error).
/// - Must not contain `/` (no path separators — keys are flat).
/// - Must not equal `..` exactly.
/// - Must not start with `../` (would escape a flat namespace).
///
/// These rules are enforced by both the real adapter and the in-memory fake so
/// they are tested uniformly by the contract suite.
pub fn validate_key(key: &str) -> Result<(), PortError> {
    if key.is_empty() {
        return Err(PortError::InvalidArgs("key must not be empty".to_owned()));
    }
    if key.contains('\0') {
        return Err(PortError::InvalidArgs(
            "key must not contain null bytes".to_owned(),
        ));
    }
    if key.contains('/') {
        return Err(PortError::InvalidArgs(
            "key must not contain '/' (keys are flat)".to_owned(),
        ));
    }
    if key == ".." {
        return Err(PortError::InvalidArgs("key must not be '..'".to_owned()));
    }
    if key.starts_with("../") {
        return Err(PortError::InvalidArgs(
            "key must not start with '../'".to_owned(),
        ));
    }
    Ok(())
}
