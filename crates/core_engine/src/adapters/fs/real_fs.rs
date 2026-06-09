//! `RealFs` implementation — see parent module for design notes.

use std::collections::HashSet;
use std::io;
use std::path::{Component, PathBuf};

use crate::domain::RelativePath;
use crate::ports::{FileSystemPort, PortError};

/// Real filesystem adapter backed by `std::fs`.
///
/// Every [`FileSystemPort`] operation resolves the [`RelativePath`] argument
/// against the `root` directory and delegates to the corresponding `std::fs`
/// function.  See the module-level documentation for the path-traversal policy,
/// the durable-write strategy, and the `scan()` semantics.
///
/// # Example
///
/// ```rust,no_run
/// use core_engine::adapters::fs::RealFs;
/// use core_engine::domain::RelativePath;
/// use core_engine::ports::FileSystemPort;
///
/// let tmp = std::env::temp_dir().join("my-workspace");
/// std::fs::create_dir_all(&tmp).unwrap();
/// let mut fs = RealFs::new(tmp);
///
/// let path = RelativePath::new("hello.txt");
/// fs.write(path.clone(), b"hello".to_vec()).unwrap();
/// assert_eq!(fs.read(&path).unwrap(), b"hello");
/// ```
#[derive(Debug)]
pub struct RealFs {
    /// Absolute root directory.  All relative paths are resolved against this.
    root: PathBuf,
    /// Paths that have been written or renamed *into* via this adapter instance
    /// and have not since been deleted or renamed *away*.  Used by `scan()`.
    known_paths: HashSet<RelativePath>,
}

impl RealFs {
    /// Create a new `RealFs` rooted at `root`.
    ///
    /// `root` must be an existing directory on the host filesystem.  The path
    /// is stored as-is (not canonicalised) so that tests may use `TempDir`
    /// handles whose paths contain symlinks without losing the temp-dir lifetime
    /// guard.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            known_paths: HashSet::new(),
        }
    }

    /// Resolve `rel` against `self.root`, returning an absolute `PathBuf`.
    ///
    /// Returns `PortError::ReadFailed` (read context) or `PortError::WriteFailed`
    /// (write context) if `rel` would escape the root via `..` components, or if
    /// `rel` is an absolute path (starts with `/` on POSIX or a drive prefix on
    /// Windows).
    /// The caller chooses the error context via the `is_write` flag so the
    /// returned variant matches the operation semantics.
    ///
    /// # Path traversal check
    ///
    /// The algorithm walks the `Component` values of `rel.as_str()` and
    /// maintains a nesting depth counter.  A `ParentDir` (`..`) that would
    /// take the depth below zero is rejected immediately.  `RootDir` and
    /// `Prefix` components are also rejected immediately: on POSIX,
    /// `PathBuf::join("/etc/passwd")` discards the base path entirely, so
    /// allowing such a component would silently escape the sandbox.
    /// This is O(n) and never touches the filesystem.
    fn resolve(&self, rel: &RelativePath, is_write: bool) -> Result<PathBuf, PortError> {
        let rel_str = rel.as_str();

        let make_traversal_err = |msg: String| -> PortError {
            if is_write {
                PortError::WriteFailed(msg)
            } else {
                PortError::ReadFailed(msg)
            }
        };

        // Walk components and count nesting depth to detect escape attempts.
        let mut depth: i32 = 0;
        for component in PathBuf::from(rel_str).components() {
            match component {
                // Absolute paths must be rejected: PathBuf::join replaces the
                // entire base when the argument is absolute, escaping the root.
                Component::RootDir | Component::Prefix(_) => {
                    return Err(make_traversal_err(format!(
                        "path traversal detected in: {rel_str}"
                    )));
                }
                Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(make_traversal_err(format!(
                            "path traversal detected in: {rel_str}"
                        )));
                    }
                }
                Component::Normal(_) => depth += 1,
                // CurDir ('.') is a no-op for depth.
                Component::CurDir => {}
            }
        }
        Ok(self.root.join(rel_str))
    }
}

/// Map a `std::io::Error` to a [`PortError`].
///
/// `ErrorKind::NotFound` becomes `PortError::NotFound`.  All other errors
/// become `ReadFailed` or `WriteFailed` according to `is_write`.
fn map_io_error(err: io::Error, is_write: bool) -> PortError {
    if err.kind() == io::ErrorKind::NotFound {
        PortError::NotFound
    } else if is_write {
        PortError::WriteFailed(err.to_string())
    } else {
        PortError::ReadFailed(err.to_string())
    }
}

impl FileSystemPort for RealFs {
    /// Read the full content of `path` as raw bytes.
    ///
    /// Binary and non-UTF-8 files are returned verbatim — no decoding is
    /// attempted (UN1/AC3).
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        let abs = self.resolve(path, false)?;
        std::fs::read(&abs).map_err(|e| map_io_error(e, false))
    }

    /// Write `bytes` to `path` durably (temp-file + `sync_all` + atomic rename).
    ///
    /// Parent directories are created automatically so that paths like
    /// `src/lib.rs` work without a prior `mkdir` call.
    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        let abs = self.resolve(&path, true)?;

        // Ensure parent directory exists.
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| map_io_error(e, true))?;
        }

        // Step 1: write to a sibling temp file in the same directory.
        // Using a fixed suffix keeps the temp file on the same volume.
        let tmp_path = {
            let file_name = abs
                .file_name()
                .map(|n| {
                    let mut s = n.to_os_string();
                    s.push(".~tmp");
                    s
                })
                .ok_or_else(|| {
                    PortError::WriteFailed(format!("invalid path: {}", path.as_str()))
                })?;
            abs.with_file_name(file_name)
        };

        // Step 2: write all bytes and fsync.
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp_path).map_err(|e| map_io_error(e, true))?;
            file.write_all(&bytes).map_err(|e| map_io_error(e, true))?;
            // Flush kernel buffers to durable storage before the rename (U3).
            file.sync_all().map_err(|e| map_io_error(e, true))?;
        }

        // Step 3: atomic rename over the destination (POSIX rename(2)).
        std::fs::rename(&tmp_path, &abs).map_err(|e| map_io_error(e, true))?;

        self.known_paths.insert(path);
        Ok(())
    }

    /// Atomically rename `from` to `to` (U2/AC2).
    ///
    /// Uses `std::fs::rename` which maps to `rename(2)` on POSIX systems,
    /// providing atomic same-volume semantics.  If `to` already exists it is
    /// silently overwritten (POSIX `rename(2)` behaviour, required by the
    /// shadow-file pattern).
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        let abs_from = self.resolve(from, false)?;
        let abs_to = self.resolve(&to, true)?;

        // Ensure the source actually exists; rename(2) would succeed even on a
        // missing source on some platforms (POSIX requires source to exist).
        if !abs_from.exists() {
            return Err(PortError::NotFound);
        }

        // Ensure destination parent directory exists.
        if let Some(parent) = abs_to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| map_io_error(e, true))?;
        }

        std::fs::rename(&abs_from, &abs_to).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PortError::NotFound
            } else {
                PortError::WriteFailed(e.to_string())
            }
        })?;

        // Update tracked paths: source is gone, destination is live.
        self.known_paths.remove(from);
        self.known_paths.insert(to);
        Ok(())
    }

    /// Remove the file at `path`.
    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        let abs = self.resolve(path, true)?;
        std::fs::remove_file(&abs).map_err(|e| map_io_error(e, true))?;
        self.known_paths.remove(path);
        Ok(())
    }

    /// Return all paths written (or renamed into) via this adapter instance
    /// that have not since been deleted or renamed away.
    ///
    /// This does *not* perform a recursive directory walk — that is spec 05b.
    fn scan(&self) -> Vec<RelativePath> {
        self.known_paths.iter().cloned().collect()
    }
}
