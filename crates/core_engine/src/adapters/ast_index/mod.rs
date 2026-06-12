//! XDG-backed AST index adapter and workspace path helpers (Stage 3a).
//!
//! # Module layout
//!
//! ```text
//! ast_index/
//! ├── mod.rs       — public re-exports and workspace path helpers
//! └── adapter.rs   — XdgAstIndexAdapter + AstIndexPort impl
//! ```
//!
//! # Path helpers
//!
//! [`global_ast_workspace_dir`] resolves the XDG base workspace root
//! (`$XDG_DATA_HOME/tower/workspace`) using the same `directories` crate
//! pattern as [`crate::adapters::plugin::install::global_plugins_dir`].
//!
//! [`workspace_ast_dir`] combines the workspace root with a per-project key
//! (`…/workspace/<key>/ast`). The key is produced by [`compute_workspace_key`].
//!
//! [`compute_workspace_key`] canonicalises the workspace root path and produces
//! a stable, filesystem-safe string of the form `<basename>_<16-hex-chars>`.
//! It uses only `std` — no new hash dependency is introduced.
//!
//! # Wiring note (Stage 3b)
//!
//! `XdgAstIndexAdapter` is not yet constructed in `main.rs`; that wiring lands
//! in Stage 3b. The adapter is fully functional and tested here.

mod adapter;

pub use adapter::XdgAstIndexAdapter;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The XDG global workspace root directory, or `None` when no base directory
/// can be determined (e.g. a headless environment with no `HOME`).
///
/// Linux: `$XDG_DATA_HOME/tower/workspace`,
/// else `~/.local/share/tower/workspace`.
///
/// The `directories` crate applies the XDG specification, including ignoring a
/// relative `$XDG_DATA_HOME`.
pub fn global_ast_workspace_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "tower").map(|dirs| dirs.data_dir().join("workspace"))
}

/// Return the per-project AST blob directory.
///
/// Combines `global_ast_workspace_dir()` with the per-project `key`:
/// `…/workspace/<key>/ast`.
///
/// This function performs no I/O.
#[must_use]
pub fn workspace_ast_dir(key: &str) -> PathBuf {
    // Fallback to a sensible relative path when XDG resolution fails (e.g. in
    // tests or headless CI). Callers that need a real path should check
    // `global_ast_workspace_dir()` first.
    let base =
        global_ast_workspace_dir().unwrap_or_else(|| PathBuf::from(".local/share/tower/workspace"));
    base.join(key).join("ast")
}

/// Compute a stable, filesystem-safe workspace key for `root`.
///
/// # Format
///
/// `<basename>_<16 hex digits>`
///
/// where:
/// - `basename` is the last path component of the canonicalised root
///   (`root.file_name()`), falling back to `"unnamed"` when unavailable.
/// - the hex digits are the little-endian representation of a 64-bit
///   [`DefaultHasher`] hash of the canonical path bytes.
///
/// # Canonicalisation
///
/// `std::fs::canonicalize` is called to resolve symlinks so that two
/// different syntactic paths pointing to the same directory produce the
/// same key. If canonicalisation fails (e.g. the path does not exist yet, or
/// permissions deny it) the raw path is used instead and a warning is written
/// to `stderr`. This matches the project's convention: `RealFs` deliberately
/// does *not* canonicalise the workspace root to preserve symlink-friendly
/// test handles — canonicalisation here is scoped only to key computation.
///
/// # Stability
///
/// The key is stable across calls for the same path on the same machine.
/// It is **not** stable across machines (the canonical absolute path differs).
/// That is intentional: the XDG data dir is machine-local anyway.
///
/// # Collision resistance
///
/// A 64-bit hash is sufficient for a user-local cache directory that holds
/// at most dozens of distinct workspaces. A collision would cause two
/// different workspaces to share the same AST cache, which is inconvenient
/// but not a security issue (the data is not sensitive).
///
/// # Stability
///
/// The key is stable within a given binary build, but `DefaultHasher` does not
/// guarantee the same output across Rust toolchain upgrades. A `rustup update`
/// may therefore produce a different key for the same workspace root, causing a
/// one-time cache miss (the plugin re-indexes from scratch) — never corruption.
pub fn compute_workspace_key(root: &Path) -> String {
    let canonical = match std::fs::canonicalize(root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "tower: warning: could not canonicalise workspace root '{}': {e}; \
                 using raw path for AST index key",
                root.display()
            );
            root.to_path_buf()
        }
    };

    let basename = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed")
        .to_owned();

    // Hash the full canonical path for uniqueness.
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let digest = hasher.finish();

    format!("{basename}_{digest:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn compute_workspace_key_is_stable_across_calls() {
        let tmp = TempDir::new().unwrap();
        let key1 = compute_workspace_key(tmp.path());
        let key2 = compute_workspace_key(tmp.path());
        assert_eq!(key1, key2, "key must be deterministic");
    }

    #[test]
    fn compute_workspace_key_differs_for_different_roots() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let key1 = compute_workspace_key(tmp1.path());
        let key2 = compute_workspace_key(tmp2.path());
        assert_ne!(key1, key2, "distinct roots must produce distinct keys");
    }

    #[test]
    fn compute_workspace_key_contains_basename() {
        let tmp = TempDir::new().unwrap();
        let key = compute_workspace_key(tmp.path());
        // The key must contain the basename of the canonical tempdir path.
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        let basename = canonical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unnamed");
        assert!(
            key.starts_with(basename),
            "key '{key}' must start with basename '{basename}'"
        );
    }

    #[test]
    fn compute_workspace_key_has_hex_suffix() {
        let tmp = TempDir::new().unwrap();
        let key = compute_workspace_key(tmp.path());
        // Format: <basename>_<16 hex digits>
        let parts: Vec<&str> = key.rsplitn(2, '_').collect();
        assert_eq!(parts.len(), 2, "key must contain an underscore separator");
        let hex = parts[0];
        assert_eq!(hex.len(), 16, "hex suffix must be 16 characters");
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "hex suffix must be hex digits: '{hex}'"
        );
    }

    #[test]
    fn compute_workspace_key_handles_nonexistent_path() {
        // A path that does not exist → canonicalize fails → raw path used.
        let key = compute_workspace_key(Path::new("/nonexistent/tower/workspace/myproject"));
        assert!(
            !key.is_empty(),
            "key must not be empty even for a nonexistent path"
        );
    }

    #[test]
    fn workspace_ast_dir_structure() {
        // workspace_ast_dir returns a path ending in "<key>/ast".
        let dir = workspace_ast_dir("myproject_aabbccdd11223344");
        let components: Vec<_> = dir.components().collect();
        let last = components.last().unwrap();
        assert_eq!(
            last.as_os_str(),
            "ast",
            "last component must be 'ast': {}",
            dir.display()
        );
        let second_last = components[components.len() - 2];
        assert_eq!(
            second_last.as_os_str(),
            "myproject_aabbccdd11223344",
            "second-to-last must be the key: {}",
            dir.display()
        );
    }

    /// Verify that a symlinked TempDir and the real path beneath it produce
    /// the same key (canonicalize resolves symlinks).
    #[test]
    fn compute_workspace_key_resolves_symlinks() {
        let real_dir = TempDir::new().unwrap();
        let link_path = real_dir.path().join("symlinked_workspace");

        // Create a subdirectory to link to.
        let target = real_dir.path().join("real_workspace");
        std::fs::create_dir_all(&target).unwrap();

        // Create a symlink pointing to the real dir.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link_path).unwrap();
        #[cfg(not(unix))]
        {
            // Skip symlink test on non-Unix where symlink creation may require
            // elevated privileges.
            return;
        }

        let key_via_real = compute_workspace_key(&target);
        let key_via_link = compute_workspace_key(&link_path);
        assert_eq!(
            key_via_real, key_via_link,
            "symlinked and real paths must produce the same key"
        );
    }
}
