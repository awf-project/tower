//! Extension discovery, manifest loading, and activation — spec 25.
//!
//! # Responsibility
//!
//! This module turns a filesystem search path into a live [`ExtensionRegistry`]
//! ready to route tool calls and deliver events. It:
//!
//! 1. **Discovers** extension directories across one or more scopes
//!    (global XDG → local `<ws>/.tower/extensions/`), with `--extensions-dir` /
//!    `$TOWER_EXTENSIONS_DIR` as an override that replaces the search path.
//!
//! 2. **Reads** each `extension.toml` manifest statically — without launching the
//!    process — and skips malformed manifests with a warning (UN2 / AC5).
//!
//! 3. **Validates** that no `lazy` extension subscribes to events (UN1 / AC3):
//!    events require an already-running process; the host does not buffer history.
//!
//! 4. **Shadows** global extensions with local ones of the same name (AC1).
//!
//! 5. **Disables** extensions listed in `[extensions] disabled` (U3 / AC4).
//!
//! 6. **Activates** extensions per their `activation` field:
//!    - `eager` → [`SidecarHostAdapter::spawn`] is called immediately so the
//!      extension is running before the first file event flows (EV1 / AC2).
//!    - `lazy` → an [`ExtensionSupervisor`] is created; the child is only spawned
//!      on the first `call_tool` invocation (EV1 / AC2).
//!
//! Graceful shutdown (EV2 / AC6) is handled by [`ExtensionRegistry::drop`], which
//! sends a `shutdown` request to every registered instance. The `SidecarHostAdapter`
//! (spec 23/25) now escalates: `shutdown` request → SIGTERM → SIGKILL.
//!
//! # Hexagonal boundary
//!
//! This module lives in `adapters/extension/`. It imports `std::fs`, `std::path`,
//! and the port / adapter types. It must never be imported by `domain/`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use extension_protocol::{Activation, ExtensionManifest};

use super::host_deps::HostDeps;
use super::supervisor::ExtensionSupervisor;
use crate::domain::extension_host::{ExtensionInstance, ExtensionRegistry};

// ── Default directory constants ───────────────────────────────────────────────

/// Default extensions sub-directory relative to the workspace root.
pub const DEFAULT_EXTENSIONS_SUBDIR: &str = ".tower/extensions";

/// XDG data sub-path for the global extensions scope (appended to
/// `$XDG_DATA_HOME` or `~/.local/share`).
pub const XDG_EXTENSIONS_SUBDIR: &str = "tower/extensions";

// ── Error / warning types ─────────────────────────────────────────────────────

/// Why a single extension manifest could not be loaded.
///
/// Load errors are non-fatal: the engine logs a warning and continues (UN2 / AC5).
#[derive(Debug)]
pub enum ManifestLoadError {
    /// `extension.toml` could not be read (I/O error).
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `extension.toml` is not valid TOML or violates the manifest schema.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// The manifest is valid but violates the activation rule:
    /// a `lazy` extension cannot subscribe to events (AC3 / UN1).
    LazyWithEvents {
        /// Extension name from the manifest.
        name: String,
        /// The event subscriptions that triggered the error.
        events: Vec<String>,
    },
}

impl std::fmt::Display for ManifestLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "invalid manifest {}: {source}", path.display())
            }
            Self::LazyWithEvents { name, events } => write!(
                f,
                "extension '{name}' declares activation=lazy but subscribes to events {:?}; \
                 events require eager activation (the host does not buffer/replay history)",
                events
            ),
        }
    }
}

impl std::error::Error for ManifestLoadError {}

// ── Directory resolution ──────────────────────────────────────────────────────

/// Return the XDG global extensions directory, or `None` when no base directory
/// can be determined (e.g. a headless environment with no `HOME`).
///
/// Linux: `$XDG_DATA_HOME/tower/extensions`, else
/// `~/.local/share/tower/extensions`.
#[must_use]
pub fn global_extensions_dir() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "tower").map(|dirs| dirs.data_dir().join("extensions"))
}

/// Resolve the ordered list of extension directories to scan — **global first,
/// local last** — so a project-local extension shadows a global one of the same
/// name.
///
/// Precedence (highest to lowest):
/// 1. `cli_arg` (`--extensions-dir`) non-empty → `[cli_arg]`. Explicit override
///    **replaces** the entire search path (single directory). Stop.
/// 2. `env_var` (`$TOWER_EXTENSIONS_DIR`) non-empty → `[env_var]`. Same replace
///    semantics. Stop.
/// 3. Multi-scope default `[global?, local]`:
///    - `global_dir` (`Some`) — XDG data dir appended with [`XDG_EXTENSIONS_SUBDIR`],
///      scanned **first** (lower precedence).
///    - local — `<workspace_root>/.tower/extensions/`, scanned **last** (wins on
///      a name collision).
///
/// Empty `cli_arg` / `env_var` strings are treated as absent (a blank environment
/// variable does not silently redirect discovery).
///
/// This function is **pure** (no filesystem or environment access) so the
/// resolution rules are unit-testable in isolation.
///
/// # Examples
///
/// ```rust
/// use std::path::{Path, PathBuf};
/// use core_engine::adapters::extension::discovery::resolve_extension_dirs;
///
/// // Explicit override takes priority.
/// let dirs = resolve_extension_dirs(Some("/custom"), None, None, Path::new("/ws"));
/// assert_eq!(dirs, vec![PathBuf::from("/custom")]);
///
/// // Default: global (XDG) then local.
/// let dirs = resolve_extension_dirs(
///     None,
///     None,
///     Some(PathBuf::from("/xdg/tower/extensions")),
///     Path::new("/ws"),
/// );
/// assert_eq!(dirs[0], PathBuf::from("/xdg/tower/extensions"));
/// assert_eq!(dirs[1], Path::new("/ws/.tower/extensions"));
/// ```
#[must_use]
pub fn resolve_extension_dirs(
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
    let local = workspace_root.join(DEFAULT_EXTENSIONS_SUBDIR);
    match global_dir {
        Some(global) => vec![global, local],
        None => vec![local],
    }
}

// ── Manifest reading ──────────────────────────────────────────────────────────

/// Read all `<dir>/extensions/<name>/extension.toml` manifests from a single
/// directory scope.
///
/// Each direct sub-directory of `dir` that contains an `extension.toml` is
/// treated as one extension. The executable is **not launched**; only the
/// manifest is parsed (U1).
///
/// Malformed manifests are collected into `errors` and skipped (UN2 / AC5); the
/// caller logs the errors as warnings and continues.
///
/// Extensions with `activation = lazy` that also declare event subscriptions
/// are rejected into `errors` (UN1 / AC3).
///
/// # Missing directory
///
/// If `dir` does not exist or cannot be read, the function returns an empty
/// `Vec` without adding any errors (graceful degradation — a missing scope is
/// not an error, matching the plugin loader pattern).
pub fn read_manifests_from_dir(
    dir: &Path,
    errors: &mut Vec<(PathBuf, ManifestLoadError)>,
) -> Vec<(PathBuf, ExtensionManifest)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!(
                "tower: warning — cannot read extensions dir {}: {e}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut results = Vec::new();

    let mut subdirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();

    for ext_dir in subdirs {
        let manifest_path = ext_dir.join("extension.toml");

        let contents = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Sub-directory without extension.toml — not an extension, skip quietly.
                continue;
            }
            Err(source) => {
                errors.push((
                    ext_dir.clone(),
                    ManifestLoadError::Io {
                        path: manifest_path,
                        source,
                    },
                ));
                continue;
            }
        };

        let manifest: ExtensionManifest = match toml::from_str(&contents) {
            Ok(m) => m,
            Err(source) => {
                errors.push((
                    ext_dir.clone(),
                    ManifestLoadError::Parse {
                        path: manifest_path,
                        source,
                    },
                ));
                continue;
            }
        };

        // Validate: lazy + events → error (UN1 / AC3).
        if manifest.activation == Activation::Lazy && !manifest.events.subscribe.is_empty() {
            errors.push((
                ext_dir.clone(),
                ManifestLoadError::LazyWithEvents {
                    name: manifest.name.clone(),
                    events: manifest.events.subscribe.clone(),
                },
            ));
            continue;
        }

        results.push((ext_dir, manifest));
    }

    results
}

// ── Shadowing ─────────────────────────────────────────────────────────────────

/// Per-extension shadowing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shadow {
    /// Register this extension.
    Keep,
    /// Shadowed by a higher-precedence (later-scope) extension of the same name.
    Overridden,
    /// A same-name extension was already kept from the **same** scope.
    DuplicateInScope,
}

/// Decide which discovered extensions to register when scanning multiple scopes.
///
/// `entries` are `(scope_index, name)` in scan order (global scopes first, local
/// last). The **winner** for a name is the entry with the highest `scope_index`;
/// on a tie (same scope) the **first** occurrence wins. Returns a `Vec<Shadow>`
/// parallel to `entries`.
///
/// # Examples
///
/// ```rust
/// use core_engine::adapters::extension::discovery::{Shadow, decide_shadowing};
///
/// // Local (scope 1) shadows global (scope 0) with the same name.
/// let entries = vec![(0usize, "ast".to_owned()), (1, "ast".to_owned())];
/// let decisions = decide_shadowing(&entries);
/// assert_eq!(decisions, vec![Shadow::Overridden, Shadow::Keep]);
/// ```
#[must_use]
pub fn decide_shadowing(entries: &[(usize, String)]) -> Vec<Shadow> {
    use std::collections::HashMap;

    // name → input index of the current winner.
    let mut winner: HashMap<&str, usize> = HashMap::new();
    for (i, (scope_idx, name)) in entries.iter().enumerate() {
        match winner.get(name.as_str()) {
            None => {
                winner.insert(name, i);
            }
            Some(&w) if *scope_idx > entries[w].0 => {
                winner.insert(name, i);
            }
            Some(_) => {}
        }
    }

    entries
        .iter()
        .enumerate()
        .map(|(i, (scope_idx, name))| {
            let w = winner[name.as_str()];
            if i == w {
                Shadow::Keep
            } else if *scope_idx == entries[w].0 {
                Shadow::DuplicateInScope
            } else {
                Shadow::Overridden
            }
        })
        .collect()
}

// ── Full pipeline ─────────────────────────────────────────────────────────────

/// Discover all extensions across `dirs`, validate manifests, apply the disable
/// list, resolve shadowing, and register survivors into a fresh
/// [`ExtensionRegistry`].
///
/// # Activation policy (EV1 / AC2)
///
/// - `activation = eager` → the extension is spawned immediately via
///   [`super::sidecar::SidecarHostAdapter::spawn`].
/// - `activation = lazy` → an [`ExtensionSupervisor`] is created; the child
///   process is spawned on the first tool invocation (supervisor handles this
///   transparently).
///
/// # Graceful degradation
///
/// - Missing scope dir → skipped silently.
/// - Malformed `extension.toml` → skipped with a stderr warning (AC5).
/// - `lazy + events` manifest → skipped with a stderr warning (AC3).
/// - Disabled extension → skipped with a stderr info line (AC4).
/// - Failed `eager` spawn → skipped with a stderr warning; startup continues.
/// - Duplicate / shadowed name → logged and skipped (AC1).
///
/// The returned registry may be empty if no valid extensions are found.
pub fn load_extensions_into_registry(
    dirs: &[PathBuf],
    deps: HostDeps,
    request_timeout: Duration,
    disabled: &[String],
) -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::new();

    // ── Phase 1: scan every scope, read manifests. ────────────────────────────
    let mut all_entries: Vec<(usize, PathBuf, ExtensionManifest)> = Vec::new();
    let mut load_errors: Vec<(PathBuf, ManifestLoadError)> = Vec::new();

    for (scope_idx, dir) in dirs.iter().enumerate() {
        let entries = read_manifests_from_dir(dir, &mut load_errors);
        for (ext_dir, manifest) in entries {
            all_entries.push((scope_idx, ext_dir, manifest));
        }
    }

    // Log all manifest load errors as warnings (UN2 / AC5).
    for (_, err) in &load_errors {
        eprintln!("tower: warning — skipping extension: {err}");
    }

    // ── Phase 2: apply disable list. ─────────────────────────────────────────
    let mut after_disable: Vec<(usize, PathBuf, ExtensionManifest)> = Vec::new();
    for (scope_idx, ext_dir, manifest) in all_entries {
        if disabled.iter().any(|d| d == &manifest.name) {
            eprintln!(
                "tower: info — extension '{}' disabled by [extensions] disabled",
                manifest.name
            );
            continue;
        }
        after_disable.push((scope_idx, ext_dir, manifest));
    }

    // ── Phase 3: resolve shadowing (local overrides global). ─────────────────
    let shadow_input: Vec<(usize, String)> = after_disable
        .iter()
        .map(|(idx, _, m)| (*idx, m.name.clone()))
        .collect();
    let decisions = decide_shadowing(&shadow_input);

    // ── Phase 4: activate survivors and register. ─────────────────────────────
    for ((scope_idx, ext_dir, manifest), decision) in after_disable.into_iter().zip(decisions) {
        let name = manifest.name.clone();
        match decision {
            Shadow::Keep => {
                // Resolve the command: if the first element is a relative path
                // (not starting with '/'), resolve it relative to ext_dir.
                let manifest = resolve_command_paths(manifest, &ext_dir);

                let instance: Box<dyn ExtensionInstance> = match manifest.activation {
                    Activation::Eager => {
                        // Spawn immediately — must be running before events flow (EV1).
                        match super::sidecar::SidecarHostAdapter::spawn(
                            manifest.clone(),
                            clone_deps(&deps),
                            request_timeout,
                        ) {
                            Ok(inst) => inst,
                            Err(e) => {
                                eprintln!(
                                    "tower: warning — failed to spawn eager extension '{name}' from {}: {e}",
                                    ext_dir.display()
                                );
                                continue;
                            }
                        }
                    }
                    Activation::Lazy => {
                        // Wrap in supervisor — spawn deferred to first call.
                        Box::new(ExtensionSupervisor::new(
                            manifest,
                            clone_deps(&deps),
                            request_timeout,
                        ))
                    }
                };

                if let Err(e) = registry.register(instance) {
                    eprintln!(
                        "tower: warning — skipping extension '{name}' from {}: {e}",
                        ext_dir.display()
                    );
                }
            }
            Shadow::Overridden => {
                eprintln!(
                    "tower: info — extension '{name}' (scope {scope_idx}) overridden by a \
                     higher-precedence (local) scope"
                );
            }
            Shadow::DuplicateInScope => {
                eprintln!(
                    "tower: warning — skipping extension '{name}': duplicate name in the same scope"
                );
            }
        }
    }

    registry
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve relative command paths against the extension directory.
///
/// If `manifest.command[0]` does not start with `/` and is not empty, it is
/// treated as a path relative to `ext_dir` (e.g. `./my-ext` → `<ext_dir>/my-ext`).
/// Absolute paths and bare executable names (no directory separator) are left
/// unchanged so that executables on `$PATH` still resolve correctly.
fn resolve_command_paths(mut manifest: ExtensionManifest, ext_dir: &Path) -> ExtensionManifest {
    if let Some(cmd) = manifest.command.first_mut() {
        let p = Path::new(cmd.as_str());
        // Any RELATIVE command — including a bare binary name like `ast_extension`
        // or a nested `bin/my-ext` — is resolved relative to the extension
        // directory, because an extension ships its sidecar binary alongside its
        // `extension.toml` (this matches the manifest's documented contract).
        // An absolute command is used verbatim. A bare name is NOT a PATH lookup:
        // the extension's executable lives in its own directory, not on $PATH.
        if p.is_relative() {
            // Strip `CurDir` (`.`) components before joining so that `./my-ext`
            // resolved against `/dir` gives `/dir/my-ext` rather than
            // `/dir/./my-ext`.
            use std::path::Component;
            let normalized: PathBuf = p
                .components()
                .filter(|c| !matches!(c, Component::CurDir))
                .collect();
            let resolved = ext_dir.join(normalized);
            *cmd = resolved.to_string_lossy().into_owned();
        }
    }
    manifest
}

/// Clone a `HostDeps` (all fields are `Arc` — cheap; `push_tx` is cloned too).
fn clone_deps(deps: &HostDeps) -> HostDeps {
    HostDeps {
        fs: std::sync::Arc::clone(&deps.fs),
        ast_index: std::sync::Arc::clone(&deps.ast_index),
        format_queue: std::sync::Arc::clone(&deps.format_queue),
        push_tx: deps.push_tx.clone(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use extension_protocol::manifest::{CapabilitiesSection, EventsSection};
    use extension_protocol::{Activation, ExtensionManifest};
    use tempfile::TempDir;

    use super::{
        ManifestLoadError, Shadow, decide_shadowing, read_manifests_from_dir,
        resolve_extension_dirs,
    };

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn write_manifest(dir: &Path, name: &str, toml: &str) {
        let ext_dir = dir.join(name);
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("extension.toml"), toml).unwrap();
    }

    fn minimal_manifest_toml(name: &str, activation: &str) -> String {
        format!(
            r#"name = "{name}"
version = "0.1.0"
command = ["./{name}"]
activation = "{activation}"
"#
        )
    }

    fn manifest_with_events(name: &str, activation: &str) -> String {
        format!(
            r#"name = "{name}"
version = "0.1.0"
command = ["./{name}"]
activation = "{activation}"

[events]
subscribe = ["event/fileChanged"]
"#
        )
    }

    // ── RED-1 → GREEN-1: resolve_extension_dirs ───────────────────────────────

    /// CLI arg overrides everything (single dir, replaces search path).
    #[test]
    fn cli_arg_replaces_search_path() {
        let dirs = resolve_extension_dirs(
            Some("/cli/extensions"),
            Some("/env/extensions"),
            Some(PathBuf::from("/xdg/tower/extensions")),
            Path::new("/ws"),
        );
        assert_eq!(dirs, vec![PathBuf::from("/cli/extensions")]);
    }

    /// Env var used when no CLI arg.
    #[test]
    fn env_var_replaces_search_path_when_no_cli_arg() {
        let dirs = resolve_extension_dirs(
            None,
            Some("/env/extensions"),
            Some(PathBuf::from("/xdg/tower/extensions")),
            Path::new("/ws"),
        );
        assert_eq!(dirs, vec![PathBuf::from("/env/extensions")]);
    }

    /// Default: global XDG first, local last.
    #[test]
    fn default_global_first_then_local() {
        let dirs = resolve_extension_dirs(
            None,
            None,
            Some(PathBuf::from("/xdg/tower/extensions")),
            Path::new("/ws"),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/xdg/tower/extensions"),
                PathBuf::from("/ws/.tower/extensions"),
            ]
        );
    }

    /// Local-only when global unavailable.
    #[test]
    fn local_only_when_global_unavailable() {
        let dirs = resolve_extension_dirs(None, None, None, Path::new("/ws"));
        assert_eq!(dirs, vec![PathBuf::from("/ws/.tower/extensions")]);
    }

    /// Empty strings treated as absent.
    #[test]
    fn empty_strings_treated_as_absent() {
        let dirs = resolve_extension_dirs(
            Some(""),
            Some(""),
            Some(PathBuf::from("/xdg/tower/extensions")),
            Path::new("/ws"),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/xdg/tower/extensions"),
                PathBuf::from("/ws/.tower/extensions"),
            ]
        );
    }

    // ── RED-2 → GREEN-2: read_manifests_from_dir — happy path ────────────────

    /// Single valid manifest in a scope is returned.
    #[test]
    fn reads_single_valid_manifest() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "ast", &minimal_manifest_toml("ast", "eager"));

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert!(errors.is_empty(), "no errors expected: {errors:?}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.name, "ast");
    }

    /// Multiple valid manifests are all returned, sorted by directory name.
    #[test]
    fn reads_multiple_valid_manifests_sorted() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "zz_ext",
            &minimal_manifest_toml("zz_ext", "lazy"),
        );
        write_manifest(
            tmp.path(),
            "aa_ext",
            &minimal_manifest_toml("aa_ext", "eager"),
        );

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert!(errors.is_empty());
        assert_eq!(found.len(), 2);
        // Sorted alphabetically by directory name.
        assert_eq!(found[0].1.name, "aa_ext");
        assert_eq!(found[1].1.name, "zz_ext");
    }

    /// Missing scope directory returns empty without errors (graceful degradation).
    #[test]
    fn missing_scope_dir_returns_empty_no_error() {
        let mut errors = Vec::new();
        let found = read_manifests_from_dir(Path::new("/nonexistent/dir"), &mut errors);
        assert!(found.is_empty());
        assert!(errors.is_empty(), "missing dir must not produce errors");
    }

    // ── RED-3 → GREEN-3: static manifest read — no spawn ─────────────────────

    /// Manifest is read without touching the executable — the command binary
    /// doesn't need to exist for discovery to succeed.
    #[test]
    fn manifest_read_does_not_require_executable_to_exist() {
        let tmp = TempDir::new().unwrap();
        // Command points to a nonexistent binary.
        let toml = r#"
name = "ghost"
version = "0.1.0"
command = ["/nonexistent/ghost-binary"]
activation = "lazy"
"#;
        write_manifest(tmp.path(), "ghost", toml);

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert!(errors.is_empty());
        assert_eq!(
            found.len(),
            1,
            "manifest must load even if binary is absent"
        );
        assert_eq!(found[0].1.name, "ghost");
    }

    // ── RED-4 → GREEN-4: malformed manifest skipped with warning (AC5 / UN2) ─

    /// Malformed TOML is collected into errors and skipped.
    #[test]
    fn malformed_manifest_is_skipped_with_error() {
        let tmp = TempDir::new().unwrap();
        let ext_dir = tmp.path().join("bad");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("extension.toml"), "this = = not toml").unwrap();

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert_eq!(found.len(), 0, "malformed manifest must be skipped");
        assert_eq!(errors.len(), 1);
        assert!(
            matches!(errors[0].1, ManifestLoadError::Parse { .. }),
            "must be a Parse error: {:?}",
            errors[0].1
        );
    }

    /// A good extension alongside a malformed one — the good one still loads.
    #[test]
    fn good_extension_loads_despite_malformed_sibling() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "good", &minimal_manifest_toml("good", "eager"));
        let bad_dir = tmp.path().join("bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("extension.toml"), "broken = = =").unwrap();

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert_eq!(found.len(), 1, "good extension must still load");
        assert_eq!(found[0].1.name, "good");
        assert_eq!(errors.len(), 1, "exactly one error for the bad manifest");
    }

    // ── RED-5 → GREEN-5: lazy + events rejected (AC3 / UN1) ──────────────────

    /// A `lazy` extension that also subscribes to events is rejected with
    /// `LazyWithEvents` (AC3).
    #[test]
    fn lazy_extension_with_events_is_rejected() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "bad_lazy",
            &manifest_with_events("bad_lazy", "lazy"),
        );

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert_eq!(found.len(), 0, "lazy+events must be rejected");
        assert_eq!(errors.len(), 1);
        assert!(
            matches!(&errors[0].1, ManifestLoadError::LazyWithEvents { name, .. } if name == "bad_lazy"),
            "must be LazyWithEvents: {:?}",
            errors[0].1
        );
    }

    /// An `eager` extension that subscribes to events is accepted (EV1).
    #[test]
    fn eager_extension_with_events_is_accepted() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "watcher",
            &manifest_with_events("watcher", "eager"),
        );

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert!(errors.is_empty(), "eager+events must not error: {errors:?}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.name, "watcher");
    }

    /// A `lazy` extension with NO events is accepted (default case).
    #[test]
    fn lazy_extension_without_events_is_accepted() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "tool_only",
            &minimal_manifest_toml("tool_only", "lazy"),
        );

        let mut errors = Vec::new();
        let found = read_manifests_from_dir(tmp.path(), &mut errors);
        assert!(errors.is_empty());
        assert_eq!(found.len(), 1);
    }

    // ── RED-6 → GREEN-6: decide_shadowing (AC1) ───────────────────────────────

    /// Distinct names → all `Keep`.
    #[test]
    fn distinct_names_all_kept() {
        let entries = vec![(0usize, "ast".to_owned()), (0, "lsp".to_owned())];
        let d = decide_shadowing(&entries);
        assert_eq!(d, vec![Shadow::Keep, Shadow::Keep]);
    }

    /// Local (scope 1) shadows global (scope 0) with the same name (AC1).
    #[test]
    fn local_shadows_global_same_name() {
        let entries = vec![(0usize, "ast".to_owned()), (1, "ast".to_owned())];
        let d = decide_shadowing(&entries);
        assert_eq!(d, vec![Shadow::Overridden, Shadow::Keep]);
    }

    /// Duplicate in the same scope: first wins.
    #[test]
    fn duplicate_in_same_scope_first_wins() {
        let entries = vec![(0usize, "dup".to_owned()), (0, "dup".to_owned())];
        let d = decide_shadowing(&entries);
        assert_eq!(d, vec![Shadow::Keep, Shadow::DuplicateInScope]);
    }

    /// Mixed scopes and duplicates (mirrors the plugin runtime test).
    #[test]
    fn mixed_scopes_and_duplicates() {
        let entries = vec![
            (0usize, "a".to_owned()),
            (0, "b".to_owned()),
            (0, "a".to_owned()), // dup in scope 0
            (1, "a".to_owned()), // local override
            (1, "c".to_owned()),
        ];
        let d = decide_shadowing(&entries);
        assert_eq!(
            d,
            vec![
                Shadow::Overridden, // scope-0 "a" beaten by scope-1 "a"
                Shadow::Keep,       // scope-0 "b" unique
                Shadow::Overridden, // 2nd scope-0 "a" also beaten
                Shadow::Keep,       // scope-1 "a" wins
                Shadow::Keep,       // scope-1 "c" unique
            ]
        );
    }

    // ── RED-7 → GREEN-7: load_extensions_into_registry (integration) ─────────
    // These tests use only fake/in-memory instances, never real spawning.

    use std::sync::{Arc, Mutex};

    use super::super::host_deps::HostDeps;
    use crate::adapters::formatter::NoOpFormatQueue;
    use crate::adapters::{InMemoryAstIndex, InMemoryFs};

    fn make_host_deps() -> HostDeps {
        HostDeps {
            fs: Arc::new(Mutex::new(InMemoryFs::new())),
            ast_index: Arc::new(InMemoryAstIndex::new()),
            format_queue: Arc::new(NoOpFormatQueue),
            push_tx: None,
        }
    }

    /// AC4: disabled extension is not registered.
    #[test]
    fn disabled_extension_is_not_registered() {
        // Use manifests that declare tools so we can verify presence/absence via
        // declared_tools() — lazy extensions are wrapped in a supervisor (not
        // spawned), so this is safe in a pure unit test.
        let tmp = TempDir::new().unwrap();
        let toml_ast = r#"
name = "ast"
version = "0.1.0"
command = ["/nonexistent/ast-bin"]
activation = "lazy"

[[tools]]
name = "outline"
description = "Outline"
schema_json = "{}"
"#;
        let toml_lsp = r#"
name = "lsp"
version = "0.1.0"
command = ["/nonexistent/lsp-bin"]
activation = "lazy"

[[tools]]
name = "diagnostics"
description = "Diagnostics"
schema_json = "{}"
"#;
        write_manifest(tmp.path(), "ast", toml_ast);
        write_manifest(tmp.path(), "lsp", toml_lsp);

        let dirs = vec![tmp.path().to_owned()];
        let registry = super::load_extensions_into_registry(
            &dirs,
            make_host_deps(),
            std::time::Duration::from_secs(1),
            &["ast".to_owned()], // disable "ast"
        );

        let tools = registry.declared_tools();
        assert_eq!(tools.len(), 1, "only lsp should be registered");
        assert_eq!(tools[0].1.name, "diagnostics", "lsp's tool must be present");

        // Verify ast is not present.
        for (_, tool) in &tools {
            assert_ne!(tool.name, "outline", "ast must be disabled");
        }
    }

    /// AC1 (shadowing): local extension shadows global one of the same name.
    #[test]
    fn local_extension_shadows_global_same_name() {
        // global scope (scope 0): "ast" with tool "outline_v1"
        // local scope (scope 1): "ast" with tool "outline_v2"
        // Result: only "outline_v2" (local) is registered.
        let global_tmp = TempDir::new().unwrap();
        let local_tmp = TempDir::new().unwrap();

        let global_ast = r#"
name = "ast"
version = "0.1.0"
command = ["/nonexistent/ast-global"]
activation = "lazy"

[[tools]]
name = "outline_v1"
description = "Global outline"
schema_json = "{}"
"#;
        let local_ast = r#"
name = "ast"
version = "0.1.0"
command = ["/nonexistent/ast-local"]
activation = "lazy"

[[tools]]
name = "outline_v2"
description = "Local outline"
schema_json = "{}"
"#;
        write_manifest(global_tmp.path(), "ast", global_ast);
        write_manifest(local_tmp.path(), "ast", local_ast);

        let dirs = vec![global_tmp.path().to_owned(), local_tmp.path().to_owned()];
        let registry = super::load_extensions_into_registry(
            &dirs,
            make_host_deps(),
            std::time::Duration::from_secs(1),
            &[],
        );

        let tools = registry.declared_tools();
        assert_eq!(tools.len(), 1, "only local ast should be registered");
        assert_eq!(
            tools[0].1.name, "outline_v2",
            "local (higher-scope) extension must win"
        );
    }

    /// AC5 (malformed manifest skipped): good extensions still load despite malformed neighbor.
    #[test]
    fn malformed_manifest_skipped_others_load() {
        let tmp = TempDir::new().unwrap();
        // Good extension.
        let good_toml = r#"
name = "good"
version = "0.1.0"
command = ["/nonexistent/good"]
activation = "lazy"

[[tools]]
name = "do_thing"
description = "Does a thing"
schema_json = "{}"
"#;
        write_manifest(tmp.path(), "good", good_toml);
        // Malformed extension.
        let bad_dir = tmp.path().join("bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("extension.toml"), "broken = = =").unwrap();

        let dirs = vec![tmp.path().to_owned()];
        let registry = super::load_extensions_into_registry(
            &dirs,
            make_host_deps(),
            std::time::Duration::from_secs(1),
            &[],
        );

        let tools = registry.declared_tools();
        assert_eq!(
            tools.len(),
            1,
            "good extension must load despite malformed sibling"
        );
        assert_eq!(tools[0].1.name, "do_thing");
    }

    /// AC3: lazy+events extension is rejected (never registered).
    #[test]
    fn lazy_with_events_is_rejected_and_not_registered() {
        let tmp = TempDir::new().unwrap();
        let bad_toml = r#"
name = "bad_lazy"
version = "0.1.0"
command = ["/nonexistent/bad"]
activation = "lazy"

[[tools]]
name = "a_tool"
description = "A tool"
schema_json = "{}"

[events]
subscribe = ["event/fileChanged"]
"#;
        write_manifest(tmp.path(), "bad_lazy", bad_toml);

        let dirs = vec![tmp.path().to_owned()];
        let registry = super::load_extensions_into_registry(
            &dirs,
            make_host_deps(),
            std::time::Duration::from_secs(1),
            &[],
        );

        let tools = registry.declared_tools();
        assert!(
            tools.is_empty(),
            "lazy+events extension must not be registered: {tools:?}"
        );
    }

    /// Missing scope directory is silently skipped — registry is empty but no panic.
    #[test]
    fn missing_scope_dir_yields_empty_registry() {
        let dirs = vec![PathBuf::from("/nonexistent/extensions/dir")];
        let registry = super::load_extensions_into_registry(
            &dirs,
            make_host_deps(),
            std::time::Duration::from_secs(1),
            &[],
        );
        assert!(
            registry.declared_tools().is_empty(),
            "missing scope must yield empty registry"
        );
    }

    // ── resolve_command_paths helper ──────────────────────────────────────────

    #[test]
    fn relative_command_with_dir_component_is_resolved() {
        use super::resolve_command_paths;
        let manifest = ExtensionManifest {
            name: "test".to_owned(),
            version: "0.1.0".to_owned(),
            command: vec!["./test-bin".to_owned()],
            activation: Activation::Lazy,
            tools: vec![],
            events: EventsSection::default(),
            capabilities: CapabilitiesSection::default(),
        };
        let ext_dir = Path::new("/ws/.tower/extensions/test");
        let resolved = resolve_command_paths(manifest, ext_dir);
        assert_eq!(
            resolved.command[0], "/ws/.tower/extensions/test/test-bin",
            "relative command must be resolved against ext_dir"
        );
    }

    #[test]
    fn absolute_command_is_unchanged() {
        use super::resolve_command_paths;
        let manifest = ExtensionManifest {
            name: "test".to_owned(),
            version: "0.1.0".to_owned(),
            command: vec!["/usr/bin/my-ext".to_owned()],
            activation: Activation::Lazy,
            tools: vec![],
            events: EventsSection::default(),
            capabilities: CapabilitiesSection::default(),
        };
        let ext_dir = Path::new("/ws/.tower/extensions/test");
        let resolved = resolve_command_paths(manifest, ext_dir);
        assert_eq!(resolved.command[0], "/usr/bin/my-ext");
    }

    /// A bare binary name (no separator) is the extension's own sidecar binary
    /// shipped alongside its manifest, so it MUST resolve relative to the
    /// extension directory — not be left as a $PATH lookup. Regression: shipped
    /// `extension.toml` files use bare names (e.g. `ast_extension`) and failed to
    /// spawn ("No such file or directory") when bare names were not resolved.
    #[test]
    fn bare_executable_name_is_resolved_to_ext_dir() {
        use super::resolve_command_paths;
        let manifest = ExtensionManifest {
            name: "test".to_owned(),
            version: "0.1.0".to_owned(),
            command: vec!["ast_extension".to_owned()],
            activation: Activation::Eager,
            tools: vec![],
            events: EventsSection::default(),
            capabilities: CapabilitiesSection::default(),
        };
        let ext_dir = Path::new("/ws/.tower/extensions/ast");
        let resolved = resolve_command_paths(manifest, ext_dir);
        assert_eq!(
            resolved.command[0], "/ws/.tower/extensions/ast/ast_extension",
            "a bare sidecar binary name must resolve relative to the extension dir"
        );
    }

    /// Guard against the class of bug where a shipped `extension.toml` is not
    /// itself discovery-valid (the e2e suites build manifests in code and bypass
    /// the real files). Asserts each reference manifest parses and — per spec 25
    /// AC3 — declares `eager` activation whenever it subscribes to events.
    #[test]
    fn shipped_reference_manifests_are_discovery_valid() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let exts_dir = ws.join("extensions");
        for name in ["ast", "fmt", "hello", "lsp"] {
            let path = exts_dir.join(name).join("extension.toml");
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let manifest: ExtensionManifest = toml::from_str(&contents)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            assert!(!manifest.command.is_empty(), "{name}: empty command");
            if !manifest.events.subscribe.is_empty() {
                assert!(
                    matches!(manifest.activation, Activation::Eager),
                    "{name}: subscribes to events {:?} but activation is {:?} — \
                     event subscribers must be eager (spec 25 AC3)",
                    manifest.events.subscribe,
                    manifest.activation
                );
            }
        }
    }
}
