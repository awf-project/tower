//! Per-language workspace-root discovery (spec 14d EV1/AC2).
//!
//! Walks from a file's directory upward to `workspace_root`, looking for the
//! first ancestor that contains any of the provided manifest file names.
//! Falls back to `workspace_root` when no manifest is found.
//!
//! # Search direction
//!
//! Upward only: from `file_dir` to `workspace_root` (inclusive), stopping at
//! the first directory that contains a manifest. No recursion into children.
//! This covers the "nested crate" case (AC2) without exploding on monorepos.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// Walk from `file_dir` upward to `workspace_root` (inclusive), returning the
/// first ancestor directory that contains any file named in `manifest_names`.
///
/// Falls back to `workspace_root` when:
/// - No manifest is found in any ancestor.
/// - A filesystem metadata call fails (permission error, broken symlink): the
///   error is silently ignored and the walk continues to the next ancestor.
///
/// `file_dir` need not be a descendant of `workspace_root`; if it is not,
/// the walk starts from `file_dir` but stops when it can no longer move toward
/// `workspace_root` without overshooting — effectively returning `workspace_root`
/// immediately.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use core_engine::adapters::lsp::discovery::discover_workspace_root;
///
/// let root = discover_workspace_root(
///     Path::new("/repo/crate/src"),
///     Path::new("/repo"),
///     &["Cargo.toml"],
/// );
/// // Returns "/repo/crate" if Cargo.toml exists there, else "/repo".
/// ```
#[allow(dead_code)]
pub fn discover_workspace_root(
    file_dir: &Path,
    workspace_root: &Path,
    manifest_names: &[&str],
) -> PathBuf {
    let mut current = file_dir.to_path_buf();
    loop {
        for name in manifest_names {
            if std::fs::metadata(current.join(name)).is_ok() {
                return current;
            }
        }

        // Do not walk above workspace_root.
        if current == workspace_root {
            break;
        }

        match current.parent() {
            Some(p) => current = p.to_path_buf(),
            None => break,
        }
    }

    workspace_root.to_path_buf()
}

/// Returns the canonical manifest file names for a given language name as
/// configured in `[lsp]`. Unknown languages return an empty slice, which
/// causes `discover_workspace_root` to fall back to the workspace root
/// immediately.
///
/// This is the single place that owns the language-to-manifest mapping. It is
/// called at session-spawn time with the language key from `LspConfig.servers`.
///
/// # Examples
///
/// ```
/// use core_engine::adapters::lsp::discovery::manifest_names_for_language;
///
/// assert_eq!(manifest_names_for_language("rust"), &["Cargo.toml"]);
/// assert!(manifest_names_for_language("brainfuck").is_empty());
/// ```
#[allow(dead_code)]
pub fn manifest_names_for_language(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &["Cargo.toml"],
        "go" => &["go.mod"],
        "typescript" | "javascript" => &["tsconfig.json", "package.json"],
        "php" => &["composer.json"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a temp tree from a list of relative paths (files and their parent dirs).
    fn make_tree(entries: &[&str]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for entry in entries {
            let path = dir.path().join(entry);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, b"").unwrap();
        }
        dir
    }

    #[test]
    fn finds_nested_cargo_toml() {
        // crate/Cargo.toml exists — must return crate/, not repo root.
        let tree = make_tree(&["Cargo.toml", "crate/Cargo.toml", "crate/src/main.rs"]);
        let workspace = tree.path().to_path_buf();
        let file_dir = workspace.join("crate/src");
        let root = discover_workspace_root(&file_dir, &workspace, &["Cargo.toml"]);
        assert_eq!(root, workspace.join("crate"));
    }

    #[test]
    fn falls_back_to_workspace_root_when_no_manifest() {
        let tree = make_tree(&["src/main.rs"]);
        let workspace = tree.path().to_path_buf();
        let file_dir = workspace.join("src");
        let root = discover_workspace_root(&file_dir, &workspace, &["Cargo.toml"]);
        assert_eq!(root, workspace);
    }

    #[test]
    fn go_mod_at_repo_root() {
        let tree = make_tree(&["go.mod", "pkg/foo.go"]);
        let workspace = tree.path().to_path_buf();
        let file_dir = workspace.join("pkg");
        let root = discover_workspace_root(&file_dir, &workspace, &["go.mod"]);
        assert_eq!(root, workspace);
    }

    #[test]
    fn does_not_walk_above_workspace_root() {
        // A Cargo.toml exists ABOVE workspace_root — must NOT be returned.
        let outer = TempDir::new().unwrap();
        fs::write(outer.path().join("Cargo.toml"), b"").unwrap();
        let inner = outer.path().join("project");
        fs::create_dir_all(inner.join("src")).unwrap();
        let workspace = inner.clone();
        let file_dir = inner.join("src");
        let root = discover_workspace_root(&file_dir, &workspace, &["Cargo.toml"]);
        // Must fall back to workspace (inner), not outer.
        assert_eq!(root, workspace);
    }

    #[test]
    fn empty_manifest_list_falls_back_immediately() {
        let tree = make_tree(&["Cargo.toml", "src/main.rs"]);
        let workspace = tree.path().to_path_buf();
        let file_dir = workspace.join("src");
        let root = discover_workspace_root(&file_dir, &workspace, &[]);
        assert_eq!(root, workspace);
    }

    #[test]
    fn manifest_names_for_rust() {
        assert_eq!(manifest_names_for_language("rust"), &["Cargo.toml"]);
    }

    #[test]
    fn manifest_names_for_go() {
        assert_eq!(manifest_names_for_language("go"), &["go.mod"]);
    }

    #[test]
    fn manifest_names_for_typescript() {
        assert_eq!(
            manifest_names_for_language("typescript"),
            &["tsconfig.json", "package.json"]
        );
    }

    #[test]
    fn manifest_names_for_unknown_is_empty() {
        assert!(manifest_names_for_language("brainfuck").is_empty());
    }

    #[test]
    fn file_dir_not_descendant_of_workspace_returns_workspace() {
        // file_dir is completely outside the workspace — walk terminates at
        // filesystem root without finding workspace_root, returns workspace_root.
        let tree = make_tree(&["Cargo.toml"]);
        let workspace = tree.path().to_path_buf();
        let file_dir = Path::new("/tmp");
        let root = discover_workspace_root(file_dir, &workspace, &["Cargo.toml"]);
        assert_eq!(root, workspace);
    }
}
