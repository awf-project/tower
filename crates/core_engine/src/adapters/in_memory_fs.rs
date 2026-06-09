//! `InMemoryFs` — HashMap-backed [`FileSystemPort`] test double (spec 02 / AC1 / AC2).
//!
//! Emulates a real file system in memory. The `rename` operation is atomic in
//! the single-threaded sense: after the call returns, the destination holds the
//! source bytes and the source is gone — no intermediate state is observable
//! by any caller of `read` or `scan` (AC2 / U3).

use std::collections::HashMap;

use crate::domain::RelativePath;
use crate::ports::{FileSystemPort, PortError};

/// A purely in-memory implementation of [`FileSystemPort`].
///
/// Backed by a `HashMap<String, Vec<u8>>` keyed by the string form of each
/// [`RelativePath`]. All operations are O(1) average.
///
/// # Rename atomicity
///
/// [`InMemoryFs::rename`] removes the source key and inserts the destination
/// key in a single HashMap operation sequence with no observable gap (AC2).
///
/// # Example
///
/// ```rust
/// use core_engine::adapters::InMemoryFs;
/// use core_engine::domain::RelativePath;
/// use core_engine::ports::FileSystemPort;
///
/// let mut fs = InMemoryFs::default();
/// let tmp = RelativePath::new("src/main.rs.tmp");
/// let dst = RelativePath::new("src/main.rs");
///
/// fs.write(tmp.clone(), b"fn main() {}".to_vec()).unwrap();
/// fs.rename(&tmp, dst.clone()).unwrap();
///
/// assert_eq!(fs.read(&dst).unwrap(), b"fn main() {}");
/// assert!(fs.read(&tmp).is_err()); // tmp is gone
/// ```
#[derive(Debug, Default)]
pub struct InMemoryFs {
    files: HashMap<String, Vec<u8>>,
}

impl InMemoryFs {
    /// Create an empty file system.
    pub fn new() -> Self {
        Self::default()
    }
}

impl FileSystemPort for InMemoryFs {
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        self.files
            .get(path.as_str())
            .cloned()
            .ok_or(PortError::NotFound)
    }

    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.files.insert(path.as_str().to_owned(), bytes);
        Ok(())
    }

    /// Atomically move `from` to `to` (U3 / AC2).
    ///
    /// Removes the source entry and inserts it under the destination key.
    /// After this method returns the caller cannot observe any intermediate
    /// state where `from` still exists alongside `to`.
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        let bytes = self
            .files
            .remove(from.as_str())
            .ok_or(PortError::NotFound)?;
        self.files.insert(to.as_str().to_owned(), bytes);
        Ok(())
    }

    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.files
            .remove(path.as_str())
            .map(|_| ())
            .ok_or(PortError::NotFound)
    }

    fn scan(&self) -> Vec<RelativePath> {
        self.files
            .keys()
            .map(|s| RelativePath::new(s.as_str()))
            .collect()
    }
}
