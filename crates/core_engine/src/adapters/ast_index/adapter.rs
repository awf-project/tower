//! `XdgAstIndexAdapter` — filesystem-backed [`AstIndexPort`] implementation.
//!
//! # Storage layout
//!
//! ```text
//! <base_dir>/          ← caller supplies this (e.g. …/workspace/<key>/ast)
//! ├── symbols.bin
//! ├── outline.bin
//! └── …
//! ```
//!
//! Each key maps to `<base_dir>/<key>.bin`.
//!
//! # Atomic writes (shadow-file pattern)
//!
//! [`XdgAstIndexAdapter::put`] writes via the same shadow-file pattern used by
//! [`RealFs::write`] (spec 08):
//!
//! 1. Write bytes to `<key>.bin.tmp` (sibling of the destination, same volume).
//! 2. Call `File::sync_all()` to flush to durable storage.
//! 3. `std::fs::rename` the temp file over `<key>.bin` (POSIX `rename(2)` is
//!    atomic and silently overwrites an existing destination).
//!
//! A crash between steps 2 and 3 leaves an orphaned `.tmp` file but the
//! destination is untouched — no window of partial content is ever observable.
//!
//! # Interior mutability
//!
//! [`AstIndexPort`] requires `&self` so the adapter can live behind an `Arc`.
//! The adapter is stateless beyond `base_dir` (a `PathBuf`); all filesystem
//! operations are inherently serialised by the OS.  No application-level lock
//! is needed.
//!
//! # Dead code note
//!
//! `XdgAstIndexAdapter` is not yet wired in `main.rs` — that wiring lands in
//! Stage 3b.

use std::io::Write as _;
use std::path::PathBuf;

use crate::ports::ast_index::validate_key;
use crate::ports::{AstIndexPort, PortError};

/// Filesystem-backed implementation of [`AstIndexPort`].
///
/// Stores each key as `<base_dir>/<key>.bin` using an atomic shadow-file write
/// (`<key>.bin.tmp` → `fsync` → `rename`).
///
/// Construct via [`XdgAstIndexAdapter::new`], supplying the already-resolved
/// `base_dir` (e.g. the result of
/// [`workspace_ast_dir`](super::workspace_ast_dir)).  The adapter does **not**
/// compute XDG paths itself — that separation keeps it a dumb keyed store that
/// is easy to test in isolation with a `TempDir`.
///
/// # Example
///
/// ```rust,no_run
/// use core_engine::adapters::ast_index::XdgAstIndexAdapter;
/// use core_engine::ports::AstIndexPort;
///
/// let dir = tempfile::tempdir().unwrap();
/// let store = XdgAstIndexAdapter::new(dir.path().to_path_buf());
/// store.put("symbols", b"serialised index bytes").unwrap();
/// let bytes = store.get("symbols").unwrap();
/// assert_eq!(bytes, Some(b"serialised index bytes".to_vec()));
/// ```
#[derive(Debug)]
pub struct XdgAstIndexAdapter {
    base_dir: PathBuf,
}

impl XdgAstIndexAdapter {
    /// Create a new adapter rooted at `base_dir`.
    ///
    /// `base_dir` is the per-workspace AST directory
    /// (`…/workspace/<key>/ast`). The directory is created on the first `put`;
    /// it need not exist at construction time.
    #[must_use]
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Resolve `key` to the canonical `.bin` destination path.
    fn bin_path(&self, key: &str) -> PathBuf {
        self.base_dir.join(format!("{key}.bin"))
    }

    /// Resolve `key` to the shadow temp path used during atomic writes.
    fn tmp_path(&self, key: &str) -> PathBuf {
        self.base_dir.join(format!("{key}.bin.tmp"))
    }
}

impl AstIndexPort for XdgAstIndexAdapter {
    /// Store `bytes` under `key` using an atomic shadow-file write.
    ///
    /// Creates `base_dir` (and all parents) on the first write. Existing
    /// `.tmp` artifacts from a previous crash are silently overwritten by the
    /// new temp file.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), PortError> {
        validate_key(key)?;

        // Create the base directory tree on first use.
        std::fs::create_dir_all(&self.base_dir)
            .map_err(|e| PortError::WriteFailed(format!("create_dir_all: {e}")))?;

        let tmp = self.tmp_path(key);
        let dst = self.bin_path(key);

        // Step 1+2: write to temp file and fsync.
        {
            let mut file = std::fs::File::create(&tmp)
                .map_err(|e| PortError::WriteFailed(format!("create tmp: {e}")))?;
            file.write_all(bytes)
                .map_err(|e| PortError::WriteFailed(format!("write_all: {e}")))?;
            file.sync_all()
                .map_err(|e| PortError::WriteFailed(format!("sync_all: {e}")))?;
        }

        // Step 3: atomic rename over the destination.
        std::fs::rename(&tmp, &dst).map_err(|e| PortError::WriteFailed(format!("rename: {e}")))?;

        Ok(())
    }

    /// Read the bytes stored under `key`, or `Ok(None)` if absent.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, PortError> {
        validate_key(key)?;

        let path = self.bin_path(key);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(PortError::ReadFailed(format!(
                "read '{}': {e}",
                path.display()
            ))),
        }
    }

    /// Remove `<key>.bin`. Idempotent: returns `Ok(())` if already absent.
    fn delete(&self, key: &str) -> Result<(), PortError> {
        validate_key(key)?;

        let path = self.bin_path(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PortError::WriteFailed(format!(
                "remove '{}': {e}",
                path.display()
            ))),
        }
    }

    /// Enumerate all `*.bin` basenames in `base_dir`, stripping the extension.
    ///
    /// Returns an empty `Vec` when `base_dir` does not exist.
    fn list(&self) -> Result<Vec<String>, PortError> {
        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(PortError::ReadFailed(format!(
                    "read_dir '{}': {e}",
                    self.base_dir.display()
                )));
            }
        };

        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| PortError::ReadFailed(format!("dir entry: {e}")))?;
            let path = entry.path();
            // Only enumerate *.bin files; skip *.bin.tmp and other artifacts.
            if path.extension().and_then(|e| e.to_str()) == Some("bin") {
                let stem = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned);
                if let Some(key) = stem {
                    keys.push(key);
                }
            }
        }
        Ok(keys)
    }
}
