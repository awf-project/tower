//! Plugin runtime bootstrap — discovery, load, and registration glue (drop & play).
//!
//! # Wireframe
//!
//! ```text
//!  main.rs startup
//!    resolve_plugin_dirs(--plugins-dir | $TOWER_PLUGINS_DIR | [global, local])
//!      explicit flag/env → single dir (replaces the search path)
//!      default           → [<xdg-data>/tower/plugins, <ws>/.tower/plugins]
//!                          (global FIRST, local LAST → local wins on collision)
//!    IsolationEngine::new()                      ← fuel + epoch + ticker (11d)
//!    load_plugins_into_registry(dirs, engine, deps, config, disabled)
//!      phase 1: for each dir, for each *.wasm (sorted):
//!        file stem in `disabled` → eprintln info + SKIP (never instantiated)
//!        IsolatedSandbox::load(engine, path, deps, config)   ← 11c/11d path
//!          Err(load)    → eprintln warning + SKIP (never abort)
//!      phase 2: decide_shadowing (local overrides global by name) then:
//!        Keep         → registry.register(Box::new(sandbox))    ← 11b
//!        Overridden   → eprintln info  + SKIP (shadowed by local)
//!        Duplicate    → eprintln warn  + SKIP (dup name in same scope)
//!        register Err → eprintln warn  + SKIP (reserved / abi)
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
//! serves exactly the 7 native `tower_*` tools — identical to pre-plugin behaviour.
//! A single plugin that fails to load (malformed wasm, ABI mismatch, forbidden
//! import, duplicate name) logs a warning to stderr and is skipped; startup never
//! aborts because of one bad plugin.

use std::path::{Path, PathBuf};

use crate::domain::PluginInstance;
use crate::domain::plugin_host::PluginHostRegistry;

use super::isolation::{DEFAULT_FUEL_BUDGET, IsolatedSandbox, IsolationConfig, IsolationEngine};
use super::loader::HostDeps;

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

/// Resolve the ordered list of plugin directories to scan — **global first, local
/// last** — so a project-local plugin shadows a global one of the same name.
///
/// Precedence:
/// 1. `cli_arg` (`--plugins-dir`) non-empty → `[cli_arg]`. An explicit override
///    **replaces** the search path (a single directory, no scopes). Stop.
/// 2. `env_var` (`$TOWER_PLUGINS_DIR`) non-empty → `[env_var]`. Same replace
///    semantics. Stop.
/// 3. Otherwise the multi-scope default `[global?, local]`:
///    - `global_dir` is the XDG data dir (`Some` unless unavailable, e.g. no HOME),
///      scanned **first**;
///    - local is `<workspace_root>/.tower/plugins`, scanned **last** (wins on a
///      name collision).
///
/// Empty `cli_arg` / `env_var` strings are treated as absent (matching
/// [`resolve_plugins_dir`]). Pure (no env/filesystem access): the binary resolves
/// `global_dir` via the `directories` crate and passes it in.
pub fn resolve_plugin_dirs(
    cli_arg: Option<&str>,
    env_var: Option<&str>,
    global_dir: Option<PathBuf>,
    workspace_root: &Path,
) -> Vec<PathBuf> {
    if let Some(path) = cli_arg.filter(|s| !s.is_empty()) {
        return vec![PathBuf::from(path)];
    }
    if let Some(path) = env_var.filter(|s| !s.is_empty()) {
        return vec![PathBuf::from(path)];
    }
    let local = workspace_root.join(DEFAULT_PLUGINS_SUBDIR);
    match global_dir {
        Some(global) => vec![global, local],
        None => vec![local],
    }
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

/// Discover `*.wasm` files across one or more scopes (in `dirs` order), load each
/// through the 11c/11d isolated sandbox path, resolve cross-scope name shadowing,
/// and register the survivors into a fresh [`PluginHostRegistry`].
///
/// `dirs` is the ordered scope list from [`resolve_plugin_dirs`] — earlier dirs are
/// lower precedence (global), the last dir is highest (local). `engine` supplies
/// the shared fuel+epoch [`Engine`]; `deps` bundles the filesystem port, AST index,
/// and workspace that each plugin receives as its capability surface; `config` is
/// the per-call compute bound (see [`production_isolation_config`]).
///
/// # Shadowing (local overrides global)
///
/// When the same plugin **name** appears in more than one scope, the one from the
/// highest-precedence (latest) dir wins; the others are skipped with a stderr
/// `info` line. A same-name duplicate **within one scope** is dropped as before.
/// The domain [`PluginHostRegistry`] never sees a cross-scope duplicate, so it
/// stays strict and unchanged.
///
/// # Graceful degradation
///
/// - A missing dir (or read error) skips that scope only — remaining scopes still
///   load. Missing is silent; other read errors warn. No scopes at all → empty
///   registry (host serves only the native tools).
/// - A single plugin that fails to load (malformed wasm, ABI mismatch, forbidden
///   import) or to register (reserved prefix) logs a warning and is skipped. One
///   bad plugin never aborts startup.
///
/// Within each scope, files are processed in sorted path order for deterministic
/// registration.
pub fn load_plugins_into_registry(
    dirs: &[PathBuf],
    engine: &IsolationEngine,
    deps: HostDeps,
    config: IsolationConfig,
    disabled: &[String],
) -> PluginHostRegistry {
    let mut registry = PluginHostRegistry::new();

    // ── Phase 1: scan every scope (in order) and load each *.wasm. ──────────────
    let mut loaded: Vec<(usize, PathBuf, IsolatedSandbox)> = Vec::new();
    for (dir_idx, dir) in dirs.iter().enumerate() {
        let mut wasm_paths = match collect_wasm_files(dir) {
            Ok(paths) => paths,
            Err(e) => {
                // A missing scope is the normal drop & play default — stay quiet.
                // Any other I/O error (permissions, etc.) is worth a warning.
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "tower: warning — cannot read plugins dir {}: {e}",
                        dir.display()
                    );
                }
                continue;
            }
        };
        wasm_paths.sort();

        for path in wasm_paths {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && disabled.iter().any(|d| d == stem)
            {
                eprintln!("tower: info — plugin '{stem}' disabled by .tower/config.toml");
                continue;
            }
            match IsolatedSandbox::load(engine.engine(), &path, deps.clone(), config.clone()) {
                Ok(sandbox) => loaded.push((dir_idx, path, sandbox)),
                Err(e) => {
                    eprintln!("tower: warning — skipping plugin {}: {e}", path.display());
                }
            }
        }
    }

    // ── Phase 2: resolve shadowing (local overrides global) then register. ──────
    let entries: Vec<(usize, String)> = loaded
        .iter()
        .map(|(idx, _, sandbox)| (*idx, sandbox.manifest().name.clone()))
        .collect();
    let decisions = decide_shadowing(&entries);

    for ((_, path, sandbox), decision) in loaded.into_iter().zip(decisions) {
        let name = sandbox.manifest().name.clone();
        match decision {
            Shadow::Keep => {
                if let Err(e) = registry.register(Box::new(sandbox)) {
                    eprintln!(
                        "tower: warning — skipping plugin '{name}' from {}: {e}",
                        path.display()
                    );
                }
            }
            Shadow::Overridden => {
                eprintln!(
                    "tower: info — plugin '{name}' from {} overridden by a higher-precedence (local) scope",
                    path.display()
                );
            }
            Shadow::DuplicateInScope => {
                eprintln!(
                    "tower: warning — skipping plugin '{name}' from {}: duplicate name in the same scope",
                    path.display()
                );
            }
        }
    }

    registry
}

/// Per-plugin shadowing decision produced by [`decide_shadowing`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shadow {
    /// Register this plugin.
    Keep,
    /// Shadowed by a higher-precedence (later-dir) plugin of the same name.
    Overridden,
    /// A same-name plugin was already kept from the **same** directory; drop it.
    DuplicateInScope,
}

/// Decide which discovered plugins to register when scanning multiple scopes.
///
/// `entries` are `(dir_index, name)` in scan order (global dirs first, local last).
/// The winner for a name is the entry with the **highest `dir_index`**; on a tie
/// (same scope) the **first** occurrence wins. Returns a `Vec<Shadow>` parallel to
/// `entries`:
/// - the winner → [`Shadow::Keep`];
/// - a lower-`dir_index` same-name entry → [`Shadow::Overridden`] (cross-scope);
/// - a same-`dir_index` non-winner → [`Shadow::DuplicateInScope`] (in-scope dup).
fn decide_shadowing(entries: &[(usize, String)]) -> Vec<Shadow> {
    use std::collections::HashMap;

    // name → input index of the current winner (highest dir_index, earliest on tie).
    let mut winner: HashMap<&str, usize> = HashMap::new();
    for (i, (dir_idx, name)) in entries.iter().enumerate() {
        match winner.get(name.as_str()) {
            None => {
                winner.insert(name, i);
            }
            Some(&w) if *dir_idx > entries[w].0 => {
                winner.insert(name, i);
            }
            Some(_) => {}
        }
    }

    entries
        .iter()
        .enumerate()
        .map(|(i, (dir_idx, name))| {
            let w = winner[name.as_str()];
            if i == w {
                Shadow::Keep
            } else if *dir_idx == entries[w].0 {
                Shadow::DuplicateInScope
            } else {
                Shadow::Overridden
            }
        })
        .collect()
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

    // ── resolve_plugin_dirs (multi-scope) ───────────────────────────────────────

    #[test]
    fn cli_arg_replaces_search_path_single_dir() {
        let dirs = resolve_plugin_dirs(
            Some("/cli"),
            Some("/env"),
            Some(PathBuf::from("/xdg/tower/plugins")),
            Path::new("/workspace"),
        );
        assert_eq!(dirs, vec![PathBuf::from("/cli")]);
    }

    #[test]
    fn env_var_replaces_search_path_single_dir() {
        let dirs = resolve_plugin_dirs(
            None,
            Some("/env"),
            Some(PathBuf::from("/xdg/tower/plugins")),
            Path::new("/workspace"),
        );
        assert_eq!(dirs, vec![PathBuf::from("/env")]);
    }

    #[test]
    fn global_first_then_local_when_no_overrides() {
        let dirs = resolve_plugin_dirs(
            None,
            None,
            Some(PathBuf::from("/xdg/tower/plugins")),
            Path::new("/workspace"),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/xdg/tower/plugins"),
                PathBuf::from("/workspace/.tower/plugins"),
            ],
            "global must be scanned before local so local shadows global"
        );
    }

    #[test]
    fn local_only_when_global_unavailable() {
        let dirs = resolve_plugin_dirs(None, None, None, Path::new("/workspace"));
        assert_eq!(dirs, vec![PathBuf::from("/workspace/.tower/plugins")]);
    }

    #[test]
    fn empty_overrides_fall_through_to_scopes() {
        let dirs = resolve_plugin_dirs(
            Some(""),
            Some(""),
            Some(PathBuf::from("/xdg/tower/plugins")),
            Path::new("/workspace"),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/xdg/tower/plugins"),
                PathBuf::from("/workspace/.tower/plugins"),
            ]
        );
    }

    // ── decide_shadowing (cross-scope precedence) ───────────────────────────────

    fn entries(pairs: &[(usize, &str)]) -> Vec<(usize, String)> {
        pairs.iter().map(|(i, n)| (*i, n.to_string())).collect()
    }

    #[test]
    fn distinct_names_all_kept() {
        let d = decide_shadowing(&entries(&[(0, "a"), (1, "b")]));
        assert_eq!(d, vec![Shadow::Keep, Shadow::Keep]);
    }

    #[test]
    fn local_overrides_global_same_name() {
        // dir 0 = global, dir 1 = local; same name "ast" → local (idx 1) wins.
        let d = decide_shadowing(&entries(&[(0, "ast"), (1, "ast")]));
        assert_eq!(d, vec![Shadow::Overridden, Shadow::Keep]);
    }

    #[test]
    fn duplicate_within_same_scope_first_wins() {
        // Two "dup" in the SAME dir (0): first kept, second dropped as in-scope dup.
        let d = decide_shadowing(&entries(&[(0, "dup"), (0, "dup")]));
        assert_eq!(d, vec![Shadow::Keep, Shadow::DuplicateInScope]);
    }

    #[test]
    fn mixed_scopes_and_duplicates() {
        // global: a, b, a(dup) ; local: a(override), c
        let d = decide_shadowing(&entries(&[
            (0, "a"),
            (0, "b"),
            (0, "a"),
            (1, "a"),
            (1, "c"),
        ]));
        assert_eq!(
            d,
            vec![
                Shadow::Overridden, // global a, beaten by local a
                Shadow::Keep,       // global b, unique
                Shadow::Overridden, // 2nd global a — also beaten by the local a (higher scope)
                Shadow::Keep,       // local a wins
                Shadow::Keep,       // local c, unique
            ]
        );
    }
}
