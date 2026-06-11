//! Plugin runtime bootstrap — discovery, load, and registration glue (drop & play).
//!
//! # Wireframe
//!
//! ```text
//!  main.rs startup
//!    resolve_plugins_dir(--plugins-dir | $TOWER_PLUGINS_DIR | <ws>/.tower/plugins)
//!    IsolationEngine::new()                      ← fuel + epoch + ticker (11d)
//!    load_plugins_into_registry(dir, engine, fs_port, config)
//!      for each *.wasm (sorted):
//!        IsolatedSandbox::load(engine, path, fs_port, config)   ← 11c/11d path
//!          Ok(sandbox)  → registry.register(Box::new(sandbox))  ← 11b
//!          Err(load)    → eprintln warning + SKIP (never abort)
//!        register Err   → eprintln warning + SKIP (dup name / abi)
//!    → PluginHostRegistry (may be empty)
//! ```
//!
//! # Why this lives in the adapter layer
//!
//! Directory scanning, `IsolatedSandbox` (wasmtime), and `IsolationEngine` are
//! infrastructure. The domain (`plugin_host`) stays infra-free: it only receives
//! the already-built `Box<dyn PluginInstance>` trait objects via `register`.
//! This module reuses 11c/11d/11b verbatim — it implements no loading, dispatch,
//! fault isolation, or trap catching of its own.
//!
//! # Graceful degradation (drop & play)
//!
//! A missing or empty plugins directory yields an empty registry, so the host
//! serves exactly the 7 native `vfs_*` tools — identical to pre-plugin behaviour.
//! A single plugin that fails to load (malformed wasm, ABI mismatch, forbidden
//! import, duplicate name) logs a warning to stderr and is skipped; startup never
//! aborts because of one bad plugin.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::plugin_host::PluginHostRegistry;
use crate::domain::PluginInstance;
use crate::ports::FileSystemPort;

use super::isolation::{IsolatedSandbox, IsolationConfig, IsolationEngine, DEFAULT_FUEL_BUDGET};

/// Default plugins directory, relative to the workspace root.
pub const DEFAULT_PLUGINS_SUBDIR: &str = ".tower/plugins";

/// Per-call epoch deadline in ticks. The [`IsolationEngine`] ticker advances the
/// epoch every 10 ms, so 3000 ticks ≈ 30 s of wall-clock per guest call.
///
/// Decision: 30 s. Generous enough never to interrupt legitimate AST parsing on
/// a slow host, while still bounding a runaway plugin by wall-clock as a
/// defense-in-depth complement to the deterministic fuel budget.
const PLUGIN_EPOCH_DEADLINE_TICKS: u64 = 3000;

// ── Plugins directory resolution ────────────────────────────────────────────────

/// Resolve the plugins directory.
///
/// Priority order (highest first):
/// 1. `cli_arg` — the value of the `--plugins-dir <path>` flag.
/// 2. `env_var` — the value of `$TOWER_PLUGINS_DIR`.
/// 3. `<workspace_root>/.tower/plugins` — the default.
///
/// Empty strings for `cli_arg` / `env_var` are treated as absent so a blank
/// environment variable does not silently redirect plugin discovery.
///
/// This function is pure (no environment or filesystem access) so the resolution
/// rules are unit-testable; the binary reads the real flag/env and passes them in.
pub fn resolve_plugins_dir(
    cli_arg: Option<&str>,
    env_var: Option<&str>,
    workspace_root: &Path,
) -> PathBuf {
    if let Some(path) = cli_arg.filter(|s| !s.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = env_var.filter(|s| !s.is_empty()) {
        return PathBuf::from(path);
    }
    workspace_root.join(DEFAULT_PLUGINS_SUBDIR)
}

// ── Isolation policy ────────────────────────────────────────────────────────────

/// The compute-bound policy applied to every production plugin sandbox.
///
/// Fuel (deterministic instruction budget) AND epoch (wall-clock deadline via the
/// [`IsolationEngine`] ticker) are both enabled, matching the AGENTS.md "fuel +
/// epoch interruption" fault-isolation mandate.
pub fn production_isolation_config() -> IsolationConfig {
    IsolationConfig {
        fuel_budget: Some(DEFAULT_FUEL_BUDGET),
        epoch_deadline_ticks: Some(PLUGIN_EPOCH_DEADLINE_TICKS),
    }
}

// ── Discovery + load + register ─────────────────────────────────────────────────

/// Discover `*.wasm` files in `dir`, load each through the 11c/11d isolated
/// sandbox path, and register every successful load into a fresh
/// [`PluginHostRegistry`].
///
/// `engine` supplies the shared fuel+epoch [`Engine`]; `fs_port` is the workspace
/// filesystem each plugin reads through (`host_read_file`); `config` is the
/// per-call compute bound (see [`production_isolation_config`]).
///
/// # Graceful degradation
///
/// - A missing `dir` (or any read error) yields an **empty** registry — the host
///   then serves only the native tools. Missing is silent; other read errors warn.
/// - A single plugin that fails to load (malformed wasm, ABI mismatch, forbidden
///   import) or to register (duplicate name) logs a warning to stderr and is
///   skipped. One bad plugin never aborts startup.
///
/// Files are processed in sorted path order for deterministic registration.
pub fn load_plugins_into_registry(
    dir: &Path,
    engine: &IsolationEngine,
    fs_port: Arc<dyn FileSystemPort + Send + Sync>,
    config: IsolationConfig,
) -> PluginHostRegistry {
    let mut registry = PluginHostRegistry::new();

    let mut wasm_paths = match collect_wasm_files(dir) {
        Ok(paths) => paths,
        Err(e) => {
            // A missing plugins dir is the normal drop & play default — stay quiet.
            // Any other I/O error (permissions, etc.) is worth a warning.
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "tower: warning — cannot read plugins dir {}: {e}",
                    dir.display()
                );
            }
            return registry;
        }
    };
    wasm_paths.sort();

    for path in wasm_paths {
        match IsolatedSandbox::load(engine.engine(), &path, Arc::clone(&fs_port), config.clone()) {
            Ok(sandbox) => {
                let name = sandbox.manifest().name.clone();
                if let Err(e) = registry.register(Box::new(sandbox)) {
                    eprintln!(
                        "tower: warning — skipping plugin '{name}' from {}: {e}",
                        path.display()
                    );
                }
            }
            Err(e) => {
                eprintln!("tower: warning — skipping plugin {}: {e}", path.display());
            }
        }
    }

    registry
}

/// Collect the `*.wasm` files directly inside `dir` (non-recursive).
///
/// Returns an `Err` (propagated from [`std::fs::read_dir`]) when the directory
/// cannot be read; the caller distinguishes `NotFound` from real errors.
fn collect_wasm_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("wasm") {
            paths.push(path);
        }
    }
    Ok(paths)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_arg_takes_precedence_over_everything() {
        let dir = resolve_plugins_dir(
            Some("/explicit/cli/path"),
            Some("/env/path"),
            Path::new("/workspace"),
        );
        assert_eq!(dir, PathBuf::from("/explicit/cli/path"));
    }

    #[test]
    fn env_var_used_when_no_cli_arg() {
        let dir = resolve_plugins_dir(None, Some("/env/path"), Path::new("/workspace"));
        assert_eq!(dir, PathBuf::from("/env/path"));
    }

    #[test]
    fn defaults_to_workspace_dot_tower_plugins() {
        let dir = resolve_plugins_dir(None, None, Path::new("/workspace"));
        assert_eq!(dir, PathBuf::from("/workspace/.tower/plugins"));
    }

    #[test]
    fn empty_strings_are_ignored_and_fall_through() {
        // Empty flag and empty env var must not be treated as a real path.
        let dir = resolve_plugins_dir(Some(""), Some(""), Path::new("/workspace"));
        assert_eq!(dir, PathBuf::from("/workspace/.tower/plugins"));
    }
}
