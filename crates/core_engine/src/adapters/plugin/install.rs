//! Global plugin management — the filesystem half of `tower plugin <cmd>`.
//!
//! Backs the `install` / `list` / `remove` subcommands. These operate on plugin
//! **files** in the XDG global scope (and, for `list`, the project-local scope):
//! they copy/enumerate/delete `*.wasm` by file name. They deliberately do **not**
//! load wasm or read manifests — keeping the CLI path free of the wasmtime engine.
//!
//! # Scopes
//!
//! - **Global**: [`global_plugins_dir`] → `$XDG_DATA_HOME/tower/plugins`
//!   (fallback `~/.local/share/tower/plugins`), shared across every project.
//! - **Local**: `<workspace>/.tower/plugins`, per project (see [`super::runtime`]).
//!
//! Identity at *serve* time is the plugin's manifest name, not its file name; these
//! helpers manage files only, so `remove` takes a file name (stem, `.wasm`
//! optional). The serve-time loader still resolves name collisions (local wins).

use std::fmt;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The XDG global plugins directory, or `None` when no base directory can be
/// determined (e.g. a headless environment with no `HOME`).
///
/// Linux: `$XDG_DATA_HOME/tower/plugins`, else `~/.local/share/tower/plugins`.
/// The `directories` crate applies the XDG spec, including ignoring a relative
/// `$XDG_DATA_HOME`.
pub fn global_plugins_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "tower").map(|dirs| dirs.data_dir().join("plugins"))
}

/// Which scope an installed plugin file lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Local,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Global => f.write_str("global"),
            Scope::Local => f.write_str("local"),
        }
    }
}

/// A plugin `.wasm` file discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub scope: Scope,
    pub file_name: String,
    pub path: PathBuf,
}

/// Copy a `.wasm` plugin into `global_dir`, creating the directory if needed.
/// Returns the destination path.
///
/// Errors if `src` is not a `*.wasm` file, has no file name, or the copy fails.
pub fn install(global_dir: &Path, src: &Path) -> io::Result<PathBuf> {
    if src.extension().and_then(|e| e.to_str()) != Some("wasm") {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("not a .wasm file: {}", src.display()),
        ));
    }
    let file_name = src.file_name().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("source has no file name: {}", src.display()),
        )
    })?;

    fs::create_dir_all(global_dir)?;
    let dest = global_dir.join(file_name);
    fs::copy(src, &dest)?;
    Ok(dest)
}

/// Enumerate installed `*.wasm` files in the given scopes (those that are `Some`
/// and exist), global first then local. A missing or unreadable directory
/// contributes no entries (it is not an error).
pub fn list(global_dir: Option<&Path>, local_dir: Option<&Path>) -> Vec<InstalledPlugin> {
    let mut out = Vec::new();
    for (scope, dir) in [(Scope::Global, global_dir), (Scope::Local, local_dir)] {
        let Some(dir) = dir else { continue };
        let mut found = collect(dir, scope);
        found.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        out.extend(found);
    }
    out
}

/// Remove `<global_dir>/<name>` where `name` is a file name with or without the
/// `.wasm` extension. Returns `true` if a file was removed, `false` if absent.
pub fn remove(global_dir: &Path, name: &str) -> io::Result<bool> {
    let file = if name.ends_with(".wasm") {
        name.to_string()
    } else {
        format!("{name}.wasm")
    };
    match fs::remove_file(global_dir.join(file)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Collect the `*.wasm` files directly inside `dir` (non-recursive). A read error
/// yields an empty list — `list` treats missing/unreadable scopes as empty.
fn collect(dir: &Path, scope: Scope) -> Vec<InstalledPlugin> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("wasm"))
        .map(|path| InstalledPlugin {
            scope,
            file_name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            path,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wasm(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"\0asm").expect("write fixture");
        p
    }

    #[test]
    fn install_copies_into_global_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = write_wasm(tmp.path(), "my_plugin.wasm");
        let global = tmp.path().join("xdg/tower/plugins");

        let dest = install(&global, &src).expect("install");
        assert_eq!(dest, global.join("my_plugin.wasm"));
        assert!(dest.exists(), "destination wasm must exist");
    }

    #[test]
    fn install_rejects_non_wasm() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("notes.txt");
        fs::write(&src, b"hello").expect("write");
        let global = tmp.path().join("g");

        let err = install(&global, &src).expect_err("must reject non-wasm");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn list_reports_global_and_local_scopes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        let local = tmp.path().join("local");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&local).unwrap();
        write_wasm(&global, "a.wasm");
        write_wasm(&local, "b.wasm");
        write_wasm(&local, "ignore.txt.wasm"); // still .wasm → counted

        let listed = list(Some(&global), Some(&local));
        let globals: Vec<_> = listed.iter().filter(|p| p.scope == Scope::Global).collect();
        let locals: Vec<_> = listed.iter().filter(|p| p.scope == Scope::Local).collect();
        assert_eq!(globals.len(), 1, "one global: {listed:?}");
        assert_eq!(locals.len(), 2, "two local: {listed:?}");
        assert_eq!(globals[0].file_name, "a.wasm");
    }

    #[test]
    fn list_skips_missing_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("nope");
        assert!(list(Some(&missing), None).is_empty());
    }

    #[test]
    fn remove_deletes_by_name_with_or_without_extension() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().to_path_buf();
        write_wasm(&global, "gone.wasm");

        assert!(remove(&global, "gone").expect("remove by stem"));
        assert!(!global.join("gone.wasm").exists());
        // Second remove → false (already absent), not an error.
        assert!(!remove(&global, "gone.wasm").expect("idempotent remove"));
    }
}
