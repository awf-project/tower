//! Errors specific to the Sled storage adapter.
//!
//! These are infrastructure-level errors that are mapped to [`PortError`]
//! at the [`StoragePort`] boundary. They are also used by the `open()` /
//! `start()` lifecycle methods which cannot return `PortError` (those are
//! not I/O operations on the port contract).
//!
//! [`StoragePort`]: crate::ports::StoragePort

use crate::ports::PortError;

/// Error returned by `SledStorageAdapter::open()` and internal operations.
#[derive(Debug)]
pub enum StorageError {
    /// The on-disk schema version is incompatible with this binary (spec UN2/AC4).
    IncompatibleSchema {
        /// Version found on disk.
        on_disk: u32,
        /// Version expected by this code.
        expected: u32,
    },
    /// A Sled operation failed.
    Sled(sled::Error),
    /// Serialisation or deserialisation failed.
    Codec(postcard::Error),
    /// The on-disk metadata is corrupt or missing required keys.
    CorruptMetadata(String),
}

impl core::fmt::Display for StorageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IncompatibleSchema { on_disk, expected } => write!(
                f,
                "incompatible schema version: on-disk={on_disk}, expected={expected}"
            ),
            Self::Sled(e) => write!(f, "sled error: {e}"),
            Self::Codec(e) => write!(f, "codec error: {e}"),
            Self::CorruptMetadata(msg) => write!(f, "corrupt metadata: {msg}"),
        }
    }
}

impl core::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Sled(e) => Some(e),
            Self::Codec(e) => Some(e),
            _ => None,
        }
    }
}

impl From<sled::Error> for StorageError {
    fn from(e: sled::Error) -> Self {
        Self::Sled(e)
    }
}

impl From<postcard::Error> for StorageError {
    fn from(e: postcard::Error) -> Self {
        Self::Codec(e)
    }
}

/// Map a [`StorageError`] to a [`PortError`] for the public StoragePort API.
///
/// `NotFound` is not reachable here — callers handle that case before
/// serialising.
pub(super) fn to_port_error(e: StorageError) -> PortError {
    PortError::WriteFailed(e.to_string())
}

/// Map a [`StorageError`] to a [`PortError::ReadFailed`].
pub(super) fn to_read_error(e: StorageError) -> PortError {
    PortError::ReadFailed(e.to_string())
}
