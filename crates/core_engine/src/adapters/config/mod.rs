//! Local project configuration: `<workspace>/.tower/config.toml`.
//!
//! Infrastructure concern (reads `std::fs`, parses `toml`). Lives in `adapters/`;
//! the domain never imports it. First feature: disabling plugins by file stem.
//! Second feature (spec 13a): `[plugins.formatter.tools.<id>]` for external formatters.
//! Third feature (LSP plan 1, Task 6): `[lsp]` table parsed via `parse_lsp_config`.
//!
//! Absent file → `TowerConfig::default()` (silent). Present but unreadable or
//! invalid (bad TOML / unknown key) → `Err` → the binary fails fast (exit 1).

pub mod lsp;
pub use lsp::{LspConfig, LspServerConfig, parse_lsp_config};

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Parsed `.tower/config.toml`. Unknown top-level keys are rejected.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TowerConfig {
    #[serde(default)]
    pub plugins: PluginConfig,
    /// `[lsp]` section — language-server bindings (LSP plan 1, Task 8).
    ///
    /// Absent → `LspConfig::default()` (empty; no servers). Populated by
    /// wiring in `main.rs`; production code reads `config.lsp` directly
    /// rather than calling `parse_lsp_config` again.
    #[serde(default)]
    pub lsp: LspConfig,
    /// `[extensions]` section — sidecar extension runtime settings (spec 24).
    ///
    /// Absent → `ExtensionConfig::default()` (30-second timeout).
    #[serde(default)]
    pub extensions: ExtensionConfig,
    /// `[daemon]` section — shared-daemon runtime settings.
    /// Absent → `DaemonConfig::default()` (30-second idle timeout).
    #[serde(default)]
    pub daemon: DaemonConfig,
}

/// `[extensions]` section (spec 24 — supervision & fault model; spec 25 — disable list).
///
/// Controls per-request timeout, respawn backoff, and which extensions are
/// skipped at startup.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    /// Wall-clock deadline for a single extension tool call or event delivery,
    /// in seconds (default 30s, matching the old WASM epoch budget — spec 24 U1).
    ///
    /// If a call exceeds this deadline the child is killed and
    /// `ExtensionFault::Timeout` is returned to the caller.
    #[serde(default = "ExtensionConfig::default_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// Extension manifest names to skip loading, e.g. `["ast", "lsp"]`.
    ///
    /// Matched against `ExtensionManifest::name` (not the filesystem path).
    /// Disabled extensions are never spawned — the check happens before any
    /// process is started (spec 25 U3).
    ///
    /// Note for spec 28 migration: `[plugins] disabled` (WASM era) will be
    /// mapped here during the config migration step.
    #[serde(default)]
    pub disabled: Vec<String>,
}

impl ExtensionConfig {
    const fn default_request_timeout_secs() -> u64 {
        30
    }

    /// Return the `request_timeout` as a [`std::time::Duration`].
    #[must_use]
    pub fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout_secs)
    }
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: Self::default_request_timeout_secs(),
            disabled: Vec::new(),
        }
    }
}

/// `[daemon]` section — shared-daemon hub settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Seconds with zero keep-alive clients before the daemon self-terminates.
    /// Only `mcp`/`observer` connections count; `control` connections do not.
    #[serde(default = "DaemonConfig::default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

impl DaemonConfig {
    const fn default_idle_timeout_secs() -> u64 {
        30
    }

    /// Idle timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_secs)
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: Self::default_idle_timeout_secs(),
        }
    }
}

/// `[plugins]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    /// Plugin file stems (no `.wasm`) to skip loading, e.g. `["ast"]`.
    #[serde(default)]
    pub disabled: Vec<String>,
    /// `[plugins.formatter]` — external formatter tool definitions (spec 13a).
    ///
    /// Absent → `FormatterConfig::default()` (empty map; capability present but
    /// every request is a no-op because no tool matches any extension).
    #[serde(default)]
    pub formatter: FormatterConfig,
}

/// `[plugins.formatter]` section (spec 13a).
///
/// # Adding new optional fields
///
/// This struct uses `#[serde(deny_unknown_fields)]`. Adding a new optional
/// field in a future version is a **breaking config change** for users who have
/// an existing `[plugins.formatter]` section: any unrecognised field causes
/// startup to exit 1 (consistent with the `[plugins]` policy). Document new
/// fields prominently in the changelog.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatterConfig {
    /// Tool definitions keyed by a human-readable ID (e.g. `"rustfmt"`).
    ///
    /// The key is the TOML table name under `[plugins.formatter.tools.<id>]`.
    /// Duplicate keys within the TOML file are rejected by the TOML parser
    /// before we see them.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolConfig>,
}

impl FormatterConfig {
    /// Return the `ToolConfig` whose `extensions` list contains `ext`, or `None`.
    ///
    /// The first matching tool in iteration order (alphabetical by id, since
    /// `BTreeMap` is ordered) wins. This is deterministic for a given config.
    #[must_use]
    pub fn tool_for_extension(&self, ext: &str) -> Option<&ToolConfig> {
        self.tools
            .values()
            .find(|t| t.extensions.iter().any(|e| e == ext))
    }
}

/// `[plugins.formatter.tools.<id>]` section — one external formatter tool.
///
/// # Fields
///
/// - `mode`: `"filter"` (default) or `"inplace"`. See [`FormatterMode`].
/// - `argv`: command + arguments. For `filter` mode the tool reads stdin and
///   writes to stdout. For `inplace` mode `{path}` is substituted with the
///   host temp copy path.
/// - `extensions`: list of file extensions (without the leading `.`) for which
///   this tool is invoked. Example: `["rs"]` for `rustfmt`.
///
/// # Note on `deny_unknown_fields`
///
/// This struct rejects any unrecognised field in the TOML section. This is
/// intentionally strict (consistent with the `[plugins]` policy) so typos in
/// field names cause startup to fail visibly rather than silently being ignored.
/// Adding a new optional field (e.g. `timeout_secs`) in a future version is
/// therefore a breaking config change for users with existing tool entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    /// Invocation contract: `filter` (default) or `inplace`.
    #[serde(default)]
    pub mode: FormatterMode,
    /// The command and its arguments. `{path}` in any argument is substituted
    /// with the temp copy path in `inplace` mode, or the file path in `filter`
    /// mode (e.g. prettier's `--stdin-filepath {path}`).
    #[serde(default)]
    pub argv: Vec<String>,
    /// File extensions (without leading `.`) handled by this tool.
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// Invocation contract for a formatter tool.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatterMode {
    /// Read source bytes from stdin, write formatted bytes to stdout. `{path}`
    /// in argv is substituted with the file path (e.g. prettier's
    /// `--stdin-filepath {path}`), and cwd is the file's directory for config
    /// discovery. This is the default (e.g. `rustfmt`, `gofmt`, `zig fmt --stdin`).
    #[default]
    Filter,
    /// Run the tool against a host-managed temp copy in the file's directory.
    /// The `{path}` placeholder in `argv` is substituted with the temp path.
    /// Example: `php-cs-fixer fix {path}`.
    Inplace,
}

/// Why loading `.tower/config.toml` failed (absence is not an error).
#[derive(Debug)]
pub enum ConfigError {
    /// The file exists but could not be read (any IO error except NotFound).
    Io { path: PathBuf, source: io::Error },
    /// The file is not valid TOML, or contains an unknown key.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "cannot read config {}: {source}", path.display())
            }
            ConfigError::Parse { path, source } => {
                write!(f, "invalid config {}: {source}", path.display())
            }
        }
    }
}

/// Load `<workspace_root>/.tower/config.toml`.
///
/// Missing file → `Ok(TowerConfig::default())`. Present but unreadable or
/// invalid → `Err`.
pub fn load(workspace_root: &Path) -> Result<TowerConfig, ConfigError> {
    let path = workspace_root.join(".tower").join("config.toml");
    match fs::read_to_string(&path) {
        Ok(contents) => {
            toml::from_str(&contents).map_err(|source| ConfigError::Parse { path, source })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(TowerConfig::default()),
        Err(source) => Err(ConfigError::Io { path, source }),
    }
}

/// Apply backward-compatibility migrations to a loaded [`TowerConfig`] and
/// return any deprecation warnings to be emitted by the caller (spec 28 O1).
///
/// # Migrations
///
/// | Legacy key / path                   | New key / path               |
/// |-------------------------------------|------------------------------|
/// | `[plugins] disabled`                | `[extensions] disabled`      |
///
/// The migration is **additive only** — it copies the legacy value into the new
/// field when the new field is empty (the user hasn't set it yet). If the user
/// has explicitly set `[extensions] disabled`, no migration occurs (their intent
/// wins).
///
/// Callers should print each warning string to stderr exactly once at startup
/// so operators know to update their config file.
///
/// # Example
///
/// ```rust
/// use core_engine::adapters::config::{TowerConfig, PluginConfig, apply_backcompat};
///
/// let mut cfg = TowerConfig::default();
/// cfg.plugins.disabled = vec!["ast".to_owned()];
/// let warnings = apply_backcompat(&mut cfg);
/// assert_eq!(cfg.extensions.disabled, vec!["ast".to_owned()]);
/// assert!(!warnings.is_empty());
/// ```
pub fn apply_backcompat(cfg: &mut TowerConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    // [plugins] disabled → [extensions] disabled
    if !cfg.plugins.disabled.is_empty() && cfg.extensions.disabled.is_empty() {
        cfg.extensions.disabled = cfg.plugins.disabled.clone();
        warnings.push(
            "tower: deprecated — `[plugins] disabled` in .tower/config.toml is superseded by \
             `[extensions] disabled`. Please update your config file."
                .to_owned(),
        );
    }

    warnings
}

/// Resolve the legacy `.tower/plugins/` directory fallback (spec 28 O1).
///
/// Returns a deprecation warning string and the fallback path when:
/// - `extensions_dir` does not exist on disk, **and**
/// - the legacy `plugins_dir` exists on disk.
///
/// The caller should use the returned path as a fallback extension directory
/// and emit the warning to stderr.
///
/// Returns `None` when no fallback is needed (the extensions dir exists or
/// neither dir exists).
#[must_use]
pub fn legacy_plugins_dir_fallback(
    extensions_dir: &Path,
    plugins_dir: &Path,
) -> Option<(PathBuf, String)> {
    if !extensions_dir.exists() && plugins_dir.exists() {
        let warning = format!(
            "tower: deprecated — `.tower/plugins/` is superseded by `.tower/extensions/`. \
             Move your extensions to {} and update any tooling.",
            extensions_dir.display()
        );
        Some((plugins_dir.to_owned(), warning))
    } else {
        None
    }
}

/// Default `.tower/config.toml` body seeded by `tower init`.
///
/// Seeds one formatter per language in the common stack. `filter` tools read
/// stdin / write stdout; `{path}` (when present, e.g. prettier's
/// `--stdin-filepath`) is substituted with the file path. `inplace` tools
/// rewrite a host-managed temp copy. A tool whose executable is not installed
/// degrades to a silent no-op, so seeding all of them is safe.
pub const DEFAULT_CONFIG: &str = "\
# Tower local project configuration.
#
# [plugins]
# disabled = [\"ast\"]   # skip loading a plugin by its *.wasm file stem
#
# [plugins.formatter.tools.<id>] — external formatters run by plugin_fmt when a
# file changes. mode = \"filter\" (default, stdin->stdout) or \"inplace\".
# Uninstalled tools are skipped, so you can safely keep entries you do not use.

[plugins.formatter.tools.rustfmt]
argv = [\"rustfmt\", \"--edition\", \"2021\"]
extensions = [\"rs\"]

[plugins.formatter.tools.gofmt]
argv = [\"gofmt\"]
extensions = [\"go\"]

[plugins.formatter.tools.zigfmt]
argv = [\"zig\", \"fmt\", \"--stdin\"]
extensions = [\"zig\"]

[plugins.formatter.tools.prettier]
argv = [\"prettier\", \"--stdin-filepath\", \"{path}\"]
extensions = [\"ts\", \"tsx\", \"js\", \"jsx\"]

[plugins.formatter.tools.php-cs-fixer]
mode = \"inplace\"
argv = [\"php-cs-fixer\", \"fix\", \"{path}\"]
extensions = [\"php\"]
";

/// Scaffold `<root>/.tower/config.toml` with [`DEFAULT_CONFIG`].
///
/// Mirrors `init_towerignore`: creates the `.tower/` directory if needed and
/// refuses to overwrite an existing `config.toml` (returns `AlreadyExists`) so
/// user edits are never clobbered. Backs `tower init`.
pub fn init_config(root: &Path) -> io::Result<PathBuf> {
    let dir = root.join(".tower");
    let path = dir.join("config.toml");
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists; refusing to overwrite", path.display()),
        ));
    }
    fs::create_dir_all(&dir)?;
    fs::write(&path, DEFAULT_CONFIG)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_config(root: &Path, body: &str) {
        let dir = root.join(".tower");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), body).unwrap();
    }

    #[test]
    fn init_config_seeds_parseable_formatter_defaults() {
        let tmp = tempdir().unwrap();
        let path = init_config(tmp.path()).expect("first init must succeed");
        assert!(path.ends_with("config.toml"));

        let cfg = load(tmp.path()).expect("seeded config must parse");
        let tools = &cfg.plugins.formatter.tools;
        for id in ["rustfmt", "gofmt", "zigfmt", "prettier", "php-cs-fixer"] {
            assert!(tools.contains_key(id), "seed must define {id}");
        }
        assert_eq!(tools["rustfmt"].mode, FormatterMode::Filter);
        assert_eq!(tools["rustfmt"].extensions, vec!["rs".to_string()]);
        assert_eq!(tools["php-cs-fixer"].mode, FormatterMode::Inplace);
    }

    #[test]
    fn init_config_refuses_to_overwrite() {
        let tmp = tempdir().unwrap();
        init_config(tmp.path()).expect("first init must succeed");
        let err = init_config(tmp.path()).expect_err("second init must refuse");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn absent_file_yields_default() {
        let tmp = tempdir().unwrap();
        let cfg = load(tmp.path()).expect("absent config is not an error");
        assert!(cfg.plugins.disabled.is_empty());
    }

    #[test]
    fn empty_file_yields_default() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "");
        let cfg = load(tmp.path()).expect("empty config is valid");
        assert!(cfg.plugins.disabled.is_empty());
    }

    #[test]
    fn parses_disabled_list() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[plugins]\ndisabled = [\"ast\"]\n");
        let cfg = load(tmp.path()).expect("valid config");
        assert_eq!(cfg.plugins.disabled, vec!["ast".to_string()]);
    }

    #[test]
    fn invalid_toml_is_error() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "this = = not toml");
        let err = load(tmp.path()).expect_err("invalid toml must error");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unknown_key_is_error() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[plugins]\nenabled = [\"ast\"]\n");
        let err = load(tmp.path()).expect_err("unknown key must error via deny_unknown_fields");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // ── RED-9 → GREEN-9: [plugins.formatter.tools.*] parsing (spec 13a) ───────

    /// GREEN-9 / AC8 (config): absent [plugins.formatter] yields empty FormatterConfig.
    #[test]
    fn absent_formatter_section_yields_empty_tools() {
        let tmp = tempdir().unwrap();
        let cfg = load(tmp.path()).expect("absent config is ok");
        assert!(
            cfg.plugins.formatter.tools.is_empty(),
            "absent formatter section must yield empty tools map"
        );
    }

    /// GREEN-9: [plugins.formatter.tools.rustfmt] with filter mode (default).
    #[test]
    fn parses_filter_mode_tool() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[plugins.formatter.tools.rustfmt]
argv = ["rustfmt", "--edition", "2021"]
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("valid formatter config");
        let tool = cfg
            .plugins
            .formatter
            .tools
            .get("rustfmt")
            .expect("rustfmt tool must be present");
        assert_eq!(tool.argv, vec!["rustfmt", "--edition", "2021"]);
        assert_eq!(tool.extensions, vec!["rs"]);
        assert_eq!(tool.mode, FormatterMode::Filter, "default mode is filter");
    }

    /// GREEN-9: [plugins.formatter.tools.php-cs-fixer] with inplace mode.
    #[test]
    fn parses_inplace_mode_tool() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[plugins.formatter.tools.php-cs-fixer]
mode = "inplace"
argv = ["php-cs-fixer", "fix", "{path}"]
extensions = ["php"]
"#,
        );
        let cfg = load(tmp.path()).expect("valid inplace config");
        let tool = cfg
            .plugins
            .formatter
            .tools
            .get("php-cs-fixer")
            .expect("php-cs-fixer must be present");
        assert_eq!(tool.mode, FormatterMode::Inplace);
        assert_eq!(tool.extensions, vec!["php"]);
        assert!(tool.argv.contains(&"{path}".to_owned()));
    }

    /// GREEN-9 (error): unknown key in [plugins.formatter.tools.rustfmt] → ConfigError::Parse.
    #[test]
    fn unknown_key_in_tool_section_is_error() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[plugins.formatter.tools.rustfmt]
argv = ["rustfmt"]
extensions = ["rs"]
unknown_field = true
"#,
        );
        let err = load(tmp.path()).expect_err("unknown field in tool section must error");
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "must be a parse error: {err}"
        );
    }

    /// GREEN-9: tool_for_extension returns the correct tool.
    #[test]
    fn tool_for_extension_returns_matching_tool() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[plugins.formatter.tools.rustfmt]
argv = ["rustfmt"]
extensions = ["rs"]

[plugins.formatter.tools.prettier]
argv = ["prettier", "--stdin-filepath", "{path}"]
extensions = ["ts", "js"]
"#,
        );
        let cfg = load(tmp.path()).expect("valid config");
        let fmt = &cfg.plugins.formatter;

        let rs_tool = fmt.tool_for_extension("rs").expect("rs must match rustfmt");
        assert_eq!(rs_tool.argv, vec!["rustfmt"]);

        let ts_tool = fmt
            .tool_for_extension("ts")
            .expect("ts must match prettier");
        assert_eq!(ts_tool.argv[0], "prettier");

        assert!(
            fmt.tool_for_extension("py").is_none(),
            "py must not match any tool"
        );
    }

    /// GREEN-9: multiple tools coexist and both/disabled still work.
    #[test]
    fn formatter_and_disabled_coexist() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[plugins]
disabled = ["ast"]

[plugins.formatter.tools.rustfmt]
argv = ["rustfmt"]
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("combined config is valid");
        assert_eq!(cfg.plugins.disabled, vec!["ast"]);
        assert!(cfg.plugins.formatter.tools.contains_key("rustfmt"));
    }

    // ── LSP section coexists with plugins section (Task 8 / Step B) ─────────

    /// A config file that contains BOTH an existing `[plugins]` section AND
    /// `[lsp.rust]` must load without a `deny_unknown_fields` error now that
    /// `TowerConfig` has an `lsp: LspConfig` field.
    #[test]
    fn lsp_and_plugins_coexist_in_tower_config() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[plugins]
disabled = ["ast"]

[lsp.rust]
command = "rust-analyzer"
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("[lsp] section must not cause deny_unknown_fields error");
        // Existing section still parses.
        assert_eq!(cfg.plugins.disabled, vec!["ast"]);
        // LSP section is accessible through the loaded TowerConfig.
        let server = cfg
            .lsp
            .for_extension("rs")
            .expect("lsp.for_extension('rs') must resolve after load()");
        assert_eq!(server.command, "rust-analyzer");
        // New field: absent idle_timeout_secs yields None (backward-compat guard).
        assert!(cfg.lsp.idle_timeout.is_none());
    }

    /// Absent `[lsp]` section still produces an empty `LspConfig` (no regression).
    #[test]
    fn absent_lsp_section_yields_empty_lsp_config() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[plugins]\ndisabled = []\n");
        let cfg = load(tmp.path()).expect("config without [lsp] must load");
        assert!(
            cfg.lsp.servers.is_empty(),
            "absent [lsp] must yield empty LspConfig"
        );
    }

    // ── Spec 24: [extensions] section ────────────────────────────────────────

    /// Absent `[extensions]` section yields 30-second default timeout (spec 24 U1).
    #[test]
    fn absent_extensions_section_yields_default_timeout() {
        let tmp = tempdir().unwrap();
        let cfg = load(tmp.path()).expect("absent config is ok");
        assert_eq!(
            cfg.extensions.request_timeout_secs, 30,
            "default timeout must be 30s"
        );
        assert_eq!(
            cfg.extensions.request_timeout(),
            std::time::Duration::from_secs(30)
        );
    }

    /// `[extensions] request_timeout = 5` is parsed correctly.
    #[test]
    fn parses_extensions_request_timeout() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[extensions]\nrequest_timeout_secs = 5\n");
        let cfg = load(tmp.path()).expect("valid extensions config");
        assert_eq!(cfg.extensions.request_timeout_secs, 5);
        assert_eq!(
            cfg.extensions.request_timeout(),
            std::time::Duration::from_secs(5)
        );
    }

    /// Unknown key in `[extensions]` is rejected.
    #[test]
    fn unknown_key_in_extensions_section_is_error() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[extensions]\nunknown_key = true\n");
        let err = load(tmp.path()).expect_err("unknown field must error");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // ── Spec 25: [extensions] disabled list ──────────────────────────────────

    /// `[extensions] disabled = ["ast"]` is parsed correctly (spec 25 U3).
    #[test]
    fn parses_extensions_disabled_list() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[extensions]\ndisabled = [\"ast\"]\n");
        let cfg = load(tmp.path()).expect("valid extensions config with disabled");
        assert_eq!(cfg.extensions.disabled, vec!["ast".to_string()]);
    }

    /// Absent `disabled` key yields empty vec (backward-compat).
    #[test]
    fn absent_extensions_disabled_yields_empty_vec() {
        let tmp = tempdir().unwrap();
        let cfg = load(tmp.path()).expect("absent config ok");
        assert!(
            cfg.extensions.disabled.is_empty(),
            "absent disabled must be empty"
        );
    }

    /// `[extensions]` and `[plugins]` and `[lsp]` all coexist.
    #[test]
    fn extensions_plugins_lsp_coexist() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[plugins]
disabled = ["ast"]

[extensions]
request_timeout_secs = 60
disabled = ["lsp"]

[lsp.rust]
command = "rust-analyzer"
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("combined config must load");
        assert_eq!(cfg.plugins.disabled, vec!["ast"]);
        assert_eq!(cfg.extensions.request_timeout_secs, 60);
        assert_eq!(cfg.extensions.disabled, vec!["lsp"]);
        assert!(cfg.lsp.for_extension("rs").is_some());
    }

    // ── Spec 28: apply_backcompat + legacy_plugins_dir_fallback ──────────────

    /// AC4 (key migration): `[plugins] disabled` is copied to `[extensions]
    /// disabled` when the latter is empty, and a warning is returned.
    #[test]
    fn apply_backcompat_copies_plugins_disabled_to_extensions_disabled() {
        let mut cfg = TowerConfig::default();
        cfg.plugins.disabled = vec!["ast".to_owned(), "hello".to_owned()];

        let warnings = apply_backcompat(&mut cfg);

        assert_eq!(
            cfg.extensions.disabled,
            vec!["ast".to_owned(), "hello".to_owned()],
            "extensions.disabled must be populated from plugins.disabled"
        );
        assert!(
            !warnings.is_empty(),
            "a deprecation warning must be returned"
        );
        assert!(
            warnings[0].contains("deprecated"),
            "warning must mention deprecation: {}",
            warnings[0]
        );
    }

    /// AC4: If `[extensions] disabled` is already set, it wins — no migration.
    #[test]
    fn apply_backcompat_does_not_overwrite_existing_extensions_disabled() {
        let mut cfg = TowerConfig::default();
        cfg.plugins.disabled = vec!["ast".to_owned()];
        cfg.extensions.disabled = vec!["lsp".to_owned()];

        let warnings = apply_backcompat(&mut cfg);

        assert_eq!(
            cfg.extensions.disabled,
            vec!["lsp".to_owned()],
            "extensions.disabled must be preserved when already set"
        );
        assert!(
            warnings.is_empty(),
            "no warning when extensions.disabled is already set"
        );
    }

    /// AC4: If `[plugins] disabled` is empty, no migration occurs and no warning.
    #[test]
    fn apply_backcompat_noop_when_plugins_disabled_is_empty() {
        let mut cfg = TowerConfig::default();
        // Both empty — no-op.
        let warnings = apply_backcompat(&mut cfg);
        assert!(cfg.extensions.disabled.is_empty());
        assert!(warnings.is_empty());
    }

    /// AC4 (dir migration): `legacy_plugins_dir_fallback` returns the plugins
    /// dir + warning when extensions dir is absent but plugins dir exists.
    #[test]
    fn legacy_plugins_dir_fallback_returns_fallback_when_plugins_dir_exists() {
        let tmp = tempdir().unwrap();
        let extensions_dir = tmp.path().join(".tower/extensions");
        let plugins_dir = tmp.path().join(".tower/plugins");
        fs::create_dir_all(&plugins_dir).unwrap();
        // extensions dir does NOT exist.

        let result = legacy_plugins_dir_fallback(&extensions_dir, &plugins_dir);
        assert!(result.is_some(), "must return Some when plugins dir exists");
        let (path, warning) = result.unwrap();
        assert_eq!(path, plugins_dir);
        assert!(
            warning.contains("deprecated"),
            "warning must mention deprecation: {warning}"
        );
    }

    /// `legacy_plugins_dir_fallback` returns None when extensions dir exists.
    #[test]
    fn legacy_plugins_dir_fallback_returns_none_when_extensions_dir_exists() {
        let tmp = tempdir().unwrap();
        let extensions_dir = tmp.path().join(".tower/extensions");
        let plugins_dir = tmp.path().join(".tower/plugins");
        fs::create_dir_all(&extensions_dir).unwrap();
        fs::create_dir_all(&plugins_dir).unwrap();

        let result = legacy_plugins_dir_fallback(&extensions_dir, &plugins_dir);
        assert!(
            result.is_none(),
            "must return None when extensions dir exists"
        );
    }

    /// `legacy_plugins_dir_fallback` returns None when neither dir exists.
    #[test]
    fn legacy_plugins_dir_fallback_returns_none_when_neither_dir_exists() {
        let tmp = tempdir().unwrap();
        let extensions_dir = tmp.path().join(".tower/extensions");
        let plugins_dir = tmp.path().join(".tower/plugins");

        let result = legacy_plugins_dir_fallback(&extensions_dir, &plugins_dir);
        assert!(result.is_none(), "must return None when neither dir exists");
    }

    // ── Task 2: [daemon] config section ──────────────────────────────────────

    #[test]
    fn daemon_config_defaults_to_30s() {
        let cfg = TowerConfig::default();
        assert_eq!(cfg.daemon.idle_timeout_secs, 30);
        assert_eq!(
            cfg.daemon.idle_timeout(),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn daemon_idle_timeout_parses_from_toml() {
        let src = "[daemon]\nidle_timeout_secs = 5\n";
        let cfg: TowerConfig = toml::from_str(src).expect("parse");
        assert_eq!(cfg.daemon.idle_timeout(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn daemon_unknown_key_is_rejected() {
        let src = "[daemon]\nidle_timeoutt_secs = 5\n";
        assert!(toml::from_str::<TowerConfig>(src).is_err());
    }
}
