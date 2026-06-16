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
}
