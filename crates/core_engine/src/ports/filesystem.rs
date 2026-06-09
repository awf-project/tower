//! `FileSystemPort` — outbound port for raw file-system operations.
//!
//! # Contract
//!
//! Implementors must satisfy the behavioral contract checked by
//! [`crate::test_support::filesystem_contract`] tests:
//!
//! - **write → read round-trip**: bytes written to a path are returned verbatim
//!   by `read`.
//! - **rename atomicity** (U3 / AC2): after `rename(from, to)`, `to` holds the
//!   bytes that were in `from`, and `from` no longer exists. No intermediate
//!   state is observable between the two.
//! - **scan**: only paths currently present are yielded; removed or renamed
//!   paths are not included.
//! - **read on unknown path**: returns `PortError::NotFound`.
//!
//! # Why atomic rename matters
//!
//! The shadow-file pattern (spec 08) writes to a `.tmp` path then renames it
//! into place so readers never observe a partial write. This port must
//! guarantee that semantics.

use crate::domain::RelativePath;
use crate::ports::PortError;

/// Raw file I/O contract used by the domain for content operations.
///
/// All paths are workspace-relative [`RelativePath`] values so the domain
/// never constructs OS-level absolute paths (U1).
pub trait FileSystemPort {
    /// Read the full content of the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::NotFound`] if `path` does not exist.
    /// Returns [`PortError::ReadFailed`] on other I/O errors.
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError>;

    /// Write `bytes` to `path`, creating or overwriting the file.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::WriteFailed`] on failure.
    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError>;

    /// Atomically move the file at `from` to `to` (U3).
    ///
    /// After a successful call:
    /// - `read(to)` returns the bytes that were in `from`.
    /// - `read(from)` returns `PortError::NotFound`.
    ///
    /// The operation is atomic with respect to concurrent readers: no
    /// intermediate state where both or neither path exists is observable.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::NotFound`] if `from` does not exist.
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError>;

    /// Remove the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::NotFound`] if `path` does not exist.
    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError>;

    /// Return all paths currently tracked by this file system.
    ///
    /// The order is unspecified; callers must not rely on insertion order.
    fn scan(&self) -> Vec<RelativePath>;
}
