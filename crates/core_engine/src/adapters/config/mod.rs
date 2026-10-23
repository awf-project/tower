//! Local project configuration: `<workspace>/.tower/config.toml`.
//!
//! Infrastructure concern (reads `std::fs`, parses `toml`). Lives in `adapters/`;
//! the domain never imports it. First feature: disabling extensions by manifest name.
//! Second feature (spec 13a): `[formatter.tools.<id>]` for external formatters.
//! Third feature (LSP plan 1, Task 6): `[lsp]` table parsed via `parse_lsp_config`.
//!
//! Absent file → `TowerConfig::default()` (silent). Present but unreadable or
//! invalid (bad TOML / unknown key) → `Err` → the binary fails fast (exit 1).

pub mod debug;
pub mod lint;
pub mod lsp;
pub use debug::{DebugConfig, DebugLanguageConfig};
pub use lint::{LintCommandConfig, LintConfig, ParserFormat, TargetMode};
pub use lsp::{LspConfig, LspServerConfig, parse_lsp_config};

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReadinessFamily {
    Ast,
    Lsp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionConfigurationReason {
    ConflictingBuiltinFamily,
    InvalidControlName,
    DuplicateControl,
    BuiltinExhaustiveControl,
    ReservedInitializeIdentity,
    NonJsonInitializeValue,
}

/// Parsed `.tower/config.toml`. Unknown top-level keys are rejected.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TowerConfig {
    #[serde(default)]
    pub formatter: FormatterConfig,
    /// `[lsp]` section — language-server bindings (LSP plan 1, Task 8).
    ///
    /// Absent → `LspConfig::default()` (empty; no servers). Populated by
    /// wiring in `main.rs`; production code reads `config.lsp` directly
    /// rather than calling `parse_lsp_config` again.
    #[serde(default)]
    pub lsp: LspConfig,
    #[serde(default)]
    pub lint: LintConfig,
    #[serde(default)]
    pub debug: DebugConfig,
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

impl TowerConfig {
    pub fn lint_request_timeout(&self) -> Result<std::time::Duration, ConfigError> {
        let maximum: Option<(&String, &LintCommandConfig)> =
            self.lint
                .commands
                .iter()
                .fold(None, |maximum, entry| match maximum {
                    Some((_, current)) if current.timeout_secs >= entry.1.timeout_secs => maximum,
                    _ => Some(entry),
                });
        let Some((language, command)) = maximum else {
            return Ok(self.extensions.request_timeout());
        };

        let invalid_deadline = || ConfigError::InvalidLintDeadline {
            field: format!("lint.{language}.timeout_secs"),
            value: command.timeout_secs,
        };
        let lint_secs = command
            .timeout_secs
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(invalid_deadline)?;
        let lint_timeout = std::time::Duration::from_secs(lint_secs);
        std::time::Instant::now()
            .checked_add(lint_timeout)
            .ok_or_else(invalid_deadline)?;

        Ok(self.extensions.request_timeout().max(lint_timeout))
    }
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
    #[serde(default)]
    pub disabled: Vec<String>,

    #[serde(default)]
    pub readiness: BTreeMap<String, ReadinessFamily>,

    #[serde(default)]
    pub readiness_controls: BTreeMap<String, Vec<String>>,

    #[serde(default)]
    pub initialize: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
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
            readiness: BTreeMap::new(),
            readiness_controls: BTreeMap::new(),
            initialize: BTreeMap::new(),
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
    #[serde(default = "DaemonConfig::default_connection_timeout_secs")]
    pub connection_timeout_secs: u64,
    #[serde(default = "DaemonConfig::default_preparation_timeout_secs")]
    pub preparation_timeout_secs: u64,
}

impl DaemonConfig {
    const fn default_idle_timeout_secs() -> u64 {
        30
    }

    const fn default_connection_timeout_secs() -> u64 {
        10
    }

    const fn default_preparation_timeout_secs() -> u64 {
        10
    }

    /// Idle timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_secs)
    }

    #[must_use]
    pub fn connection_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.connection_timeout_secs)
    }

    #[must_use]
    pub fn preparation_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.preparation_timeout_secs)
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: Self::default_idle_timeout_secs(),
            connection_timeout_secs: Self::default_connection_timeout_secs(),
            preparation_timeout_secs: Self::default_preparation_timeout_secs(),
        }
    }
}

/// `[formatter]` section (spec 13a).
///
/// # Adding new optional fields
///
/// This struct uses `#[serde(deny_unknown_fields)]`. Adding a new optional
/// field in a future version is a **breaking config change** for users who have
/// an existing `[formatter]` section: any unrecognised field causes startup to
/// exit 1 (consistent with the strict configuration policy). Document new
/// fields prominently in the changelog.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatterConfig {
    /// Tool definitions keyed by a human-readable ID (e.g. `"rustfmt"`).
    ///
    /// The key is the TOML table name under `[formatter.tools.<id>]`.
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

/// `[formatter.tools.<id>]` section — one external formatter tool.
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
/// intentionally strict (consistent with the top-level configuration policy) so typos in
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
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// The file is not valid TOML, or contains an unknown key.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    InvalidDaemonDeadline {
        field: &'static str,
        value: u64,
    },
    InvalidLintDeadline {
        field: String,
        value: u64,
    },
    InvalidExtensionConfiguration {
        field: &'static str,
        reason: ExtensionConfigurationReason,
    },
    LegacySection {
        path: PathBuf,
        section: &'static str,
        replacements: Vec<&'static str>,
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
            ConfigError::InvalidDaemonDeadline { field, value } => {
                write!(f, "invalid daemon deadline {field}: {value}")
            }
            ConfigError::InvalidLintDeadline { field, value } => write!(
                f,
                "invalid lint deadline {field}: {value} cannot produce 2 * timeout_secs + 1"
            ),
            ConfigError::InvalidExtensionConfiguration { field, reason } => {
                write!(f, "invalid extension configuration {field}: {reason:?}")
            }
            ConfigError::LegacySection {
                path,
                section,
                replacements,
            } => write!(
                f,
                "legacy section {section} in {} is unsupported; replace it with {}",
                path.display(),
                replacements.join(", ")
            ),
        }
    }
}

/// Load `<workspace_root>/.tower/config.toml`.
///
/// Missing file → `Ok(TowerConfig::default())`. Present but unreadable or
/// invalid → `Err`.
pub fn load(workspace_root: &Path) -> Result<TowerConfig, ConfigError> {
    let path = workspace_root.join(".tower").join("config.toml");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(TowerConfig::default());
        }
        Err(source) => return Err(ConfigError::Io { path, source }),
    };

    let raw: toml::Value = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;

    if let Some(plugins) = raw.get("plugins") {
        let table = plugins.as_table();
        let has_disabled = table.is_some_and(|plugins| plugins.contains_key("disabled"));
        let has_formatter = table.is_some_and(|plugins| plugins.contains_key("formatter"));
        let replacements = match (has_disabled, has_formatter) {
            (true, false) => vec!["[extensions] disabled"],
            (false, true) => vec!["[formatter.tools.<id>]"],
            _ => vec!["[extensions] disabled", "[formatter.tools.<id>]"],
        };

        return Err(ConfigError::LegacySection {
            path,
            section: "[plugins]",
            replacements,
        });
    }

    if raw
        .get("extensions")
        .and_then(|extensions| extensions.get("initialize"))
        .is_some_and(contains_non_json_toml_value)
    {
        return Err(ConfigError::InvalidExtensionConfiguration {
            field: "initialize",
            reason: ExtensionConfigurationReason::NonJsonInitializeValue,
        });
    }

    lint::validate_timeout_values(&raw).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;
    let config: TowerConfig =
        toml::from_str(&contents).map_err(|source| ConfigError::Parse { path, source })?;
    validate_daemon_deadlines(&config.daemon)?;
    validate_extension_configuration(&config.extensions)?;
    config.lint_request_timeout()?;
    Ok(config)
}

fn contains_non_json_toml_value(value: &toml::Value) -> bool {
    match value {
        toml::Value::Float(value) => !value.is_finite(),
        toml::Value::Datetime(_) => true,
        toml::Value::Array(values) => values.iter().any(contains_non_json_toml_value),
        toml::Value::Table(values) => values.values().any(contains_non_json_toml_value),
        toml::Value::String(_) | toml::Value::Integer(_) | toml::Value::Boolean(_) => false,
    }
}

fn validate_daemon_deadlines(config: &DaemonConfig) -> Result<(), ConfigError> {
    for (field, value) in [
        ("connection_timeout_secs", config.connection_timeout_secs),
        ("preparation_timeout_secs", config.preparation_timeout_secs),
    ] {
        if value == 0 {
            return Err(ConfigError::InvalidDaemonDeadline { field, value });
        }
    }
    Ok(())
}

fn validate_extension_configuration(config: &ExtensionConfig) -> Result<(), ConfigError> {
    for (identity, expected_family) in
        [("ast", ReadinessFamily::Ast), ("lsp", ReadinessFamily::Lsp)]
    {
        if config
            .readiness
            .get(identity)
            .is_some_and(|family| *family != expected_family)
        {
            return Err(ConfigError::InvalidExtensionConfiguration {
                field: "readiness",
                reason: ExtensionConfigurationReason::ConflictingBuiltinFamily,
            });
        }
    }

    for (identity, controls) in &config.readiness_controls {
        if identity == "ast" || identity == "lsp" {
            return Err(ConfigError::InvalidExtensionConfiguration {
                field: "readiness_controls",
                reason: ExtensionConfigurationReason::BuiltinExhaustiveControl,
            });
        }
        if !config.readiness.contains_key(identity)
            || controls.is_empty()
            || controls.iter().any(|control| control.trim().is_empty())
        {
            return Err(ConfigError::InvalidExtensionConfiguration {
                field: "readiness_controls",
                reason: ExtensionConfigurationReason::InvalidControlName,
            });
        }

        let mut unique = std::collections::BTreeSet::new();
        if controls.iter().any(|control| !unique.insert(control)) {
            return Err(ConfigError::InvalidExtensionConfiguration {
                field: "readiness_controls",
                reason: ExtensionConfigurationReason::DuplicateControl,
            });
        }
    }

    if config
        .initialize
        .keys()
        .any(|identity| identity == "lsp" || identity == "debug")
    {
        return Err(ConfigError::InvalidExtensionConfiguration {
            field: "initialize",
            reason: ExtensionConfigurationReason::ReservedInitializeIdentity,
        });
    }

    Ok(())
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
# [extensions] configures native out-of-process sidecars by manifest name.
# Add names to `disabled` to prevent those sidecars from being spawned.
[extensions]
disabled = []
#
# [formatter.tools.<id>] entries are commands run by the host format queue when
# a file changes. mode = \"filter\" (default, stdin->stdout) or \"inplace\".
# Uninstalled tools are skipped, so you can safely keep entries you do not use.

[formatter.tools.rustfmt]
argv = [\"rustfmt\", \"--edition\", \"2021\"]
extensions = [\"rs\"]

[formatter.tools.gofmt]
argv = [\"gofmt\"]
extensions = [\"go\"]

[formatter.tools.zigfmt]
argv = [\"zig\", \"fmt\", \"--stdin\"]
extensions = [\"zig\"]

[formatter.tools.prettier]
argv = [\"prettier\", \"--stdin-filepath\", \"{path}\"]
extensions = [\"ts\", \"tsx\", \"js\", \"jsx\"]

[formatter.tools.php-cs-fixer]
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
    fn canonical_default_config_is_parseable_and_contains_no_legacy_examples() {
        let tmp = tempdir().unwrap();
        let path = init_config(tmp.path()).expect("first init must succeed");
        assert!(path.ends_with("config.toml"));
        let generated = fs::read_to_string(&path).unwrap();
        let config = load(tmp.path()).expect("generated config must parse");

        assert!(generated.contains("[extensions]"));
        assert!(generated.contains("[formatter.tools.rustfmt]"));
        assert!(generated.contains("native out-of-process sidecar"));
        assert!(generated.contains("host format queue"));
        for forbidden in ["[plugins]", "plugin_fmt", ".wasm", "apply_backcompat"] {
            assert!(
                !generated.contains(forbidden),
                "generated config contains forbidden token {forbidden}"
            );
        }

        let expected = [
            (
                "rustfmt",
                FormatterMode::Filter,
                &["rustfmt", "--edition", "2021"][..],
                &["rs"][..],
            ),
            ("gofmt", FormatterMode::Filter, &["gofmt"][..], &["go"][..]),
            (
                "zigfmt",
                FormatterMode::Filter,
                &["zig", "fmt", "--stdin"][..],
                &["zig"][..],
            ),
            (
                "prettier",
                FormatterMode::Filter,
                &["prettier", "--stdin-filepath", "{path}"][..],
                &["ts", "tsx", "js", "jsx"][..],
            ),
            (
                "php-cs-fixer",
                FormatterMode::Inplace,
                &["php-cs-fixer", "fix", "{path}"][..],
                &["php"][..],
            ),
        ];
        assert_eq!(config.formatter.tools.len(), expected.len());
        for (name, mode, argv, extensions) in expected {
            let tool = config
                .formatter
                .tools
                .get(name)
                .unwrap_or_else(|| panic!("missing formatter default {name}"));
            assert_eq!(tool.mode, mode, "mode for {name}");
            assert_eq!(tool.argv, argv, "command and argv for {name}");
            assert_eq!(tool.extensions, extensions, "extensions for {name}");
        }
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
        assert!(cfg.extensions.disabled.is_empty());
        assert!(cfg.formatter.tools.is_empty());
    }

    #[test]
    fn empty_file_yields_default() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "");
        let cfg = load(tmp.path()).expect("empty config is valid");
        assert!(cfg.extensions.disabled.is_empty());
        assert!(cfg.formatter.tools.is_empty());
    }

    #[test]
    fn lint_timeout_derives_sufficient_host_budget() {
        for (host_secs, lint_entries, expected_secs) in [
            (1, "", 1),
            (
                1,
                "[lint.rust]\ncommand = \"cargo\"\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                5,
            ),
            (
                2,
                "[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = 2\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                5,
            ),
            (
                4,
                "[lint.go]\ncommand = \"golangci-lint\"\ntimeout_secs = 7\nextensions = [\"go\"]\nformat = \"generic-regex\"\ntarget = \"append\"\n[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = 3\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                15,
            ),
            (
                30,
                "[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = 7\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                30,
            ),
        ] {
            let source =
                format!("[extensions]\nrequest_timeout_secs = {host_secs}\n{lint_entries}");
            let config: TowerConfig = toml::from_str(&source).expect("valid timeout fixture");

            assert_eq!(
                config
                    .lint_request_timeout()
                    .expect("representable lint deadline"),
                std::time::Duration::from_secs(expected_secs),
                "host={host_secs}, lint={lint_entries:?}"
            );
            assert_eq!(config.extensions.request_timeout_secs, host_secs);
        }

        let mut arithmetic_overflow: TowerConfig = toml::from_str(
            "[lint.rust]\ncommand = \"cargo\"\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
        )
        .unwrap();
        arithmetic_overflow
            .lint
            .commands
            .get_mut("rust")
            .unwrap()
            .timeout_secs = u64::MAX;
        let err = arithmetic_overflow
            .lint_request_timeout()
            .expect_err("u64 arithmetic overflow must be rejected");
        assert!(matches!(
            err,
            ConfigError::InvalidLintDeadline { ref field, value }
                if field == "lint.rust.timeout_secs" && value == u64::MAX
        ));
        assert_eq!(
            err.to_string(),
            format!(
                "invalid lint deadline lint.rust.timeout_secs: {} cannot produce 2 * timeout_secs + 1",
                u64::MAX
            )
        );

        let instant_overflow = u64::MAX / 2;
        let mut unrepresentable_deadline: TowerConfig = toml::from_str(
            "[lint.go]\ncommand = \"golangci-lint\"\nextensions = [\"go\"]\nformat = \"generic-regex\"\ntarget = \"append\"\n[lint.rust]\ncommand = \"cargo\"\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
        )
        .unwrap();
        for command in unrepresentable_deadline.lint.commands.values_mut() {
            command.timeout_secs = instant_overflow;
        }
        assert!(matches!(
            unrepresentable_deadline.lint_request_timeout(),
            Err(ConfigError::InvalidLintDeadline { field, value })
                if field == "lint.go.timeout_secs" && value == instant_overflow
        ));
    }

    #[test]
    fn parses_disabled_list() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[extensions]\ndisabled = [\"ast\"]\n");
        let cfg = load(tmp.path()).expect("valid config");
        assert_eq!(cfg.extensions.disabled, vec!["ast".to_string()]);
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
        write_config(tmp.path(), "[extensions]\nenabled = [\"ast\"]\n");
        let err = load(tmp.path()).expect_err("unknown key must error via deny_unknown_fields");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // ── RED-9 → GREEN-9: [formatter.tools.*] parsing (spec 13a) ──────────────

    /// GREEN-9 / AC8 (config): absent [formatter] yields empty FormatterConfig.
    #[test]
    fn absent_formatter_section_yields_empty_tools() {
        let tmp = tempdir().unwrap();
        let cfg = load(tmp.path()).expect("absent config is ok");
        assert!(
            cfg.formatter.tools.is_empty(),
            "absent formatter section must yield empty tools map"
        );
    }

    /// GREEN-9: [formatter.tools.rustfmt] with filter mode (default).
    #[test]
    fn parses_filter_mode_tool() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[formatter.tools.rustfmt]
argv = ["rustfmt", "--edition", "2021"]
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("valid formatter config");
        let tool = cfg
            .formatter
            .tools
            .get("rustfmt")
            .expect("rustfmt tool must be present");
        assert_eq!(tool.argv, vec!["rustfmt", "--edition", "2021"]);
        assert_eq!(tool.extensions, vec!["rs"]);
        assert_eq!(tool.mode, FormatterMode::Filter, "default mode is filter");
    }

    /// GREEN-9: [formatter.tools.php-cs-fixer] with inplace mode.
    #[test]
    fn parses_inplace_mode_tool() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[formatter.tools.php-cs-fixer]
mode = "inplace"
argv = ["php-cs-fixer", "fix", "{path}"]
extensions = ["php"]
"#,
        );
        let cfg = load(tmp.path()).expect("valid inplace config");
        let tool = cfg
            .formatter
            .tools
            .get("php-cs-fixer")
            .expect("php-cs-fixer must be present");
        assert_eq!(tool.mode, FormatterMode::Inplace);
        assert_eq!(tool.extensions, vec!["php"]);
        assert!(tool.argv.contains(&"{path}".to_owned()));
    }

    /// GREEN-9 (error): unknown key in [formatter.tools.rustfmt] → ConfigError::Parse.
    #[test]
    fn unknown_key_in_tool_section_is_error() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[formatter.tools.rustfmt]
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
[formatter.tools.rustfmt]
argv = ["rustfmt"]
extensions = ["rs"]

[formatter.tools.prettier]
argv = ["prettier", "--stdin-filepath", "{path}"]
extensions = ["ts", "js"]
"#,
        );
        let cfg = load(tmp.path()).expect("valid config");
        let fmt = &cfg.formatter;

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
[extensions]
disabled = ["ast"]

[formatter.tools.rustfmt]
argv = ["rustfmt"]
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("combined config is valid");
        assert_eq!(cfg.extensions.disabled, vec!["ast"]);
        assert!(cfg.formatter.tools.contains_key("rustfmt"));
    }

    // ── LSP section coexists with canonical sections (Task 8 / Step B) ──────

    /// A config file that contains BOTH an `[extensions]` section AND
    /// `[lsp.rust]` must load without a `deny_unknown_fields` error now that
    /// `TowerConfig` has an `lsp: LspConfig` field.
    #[test]
    fn lsp_and_extensions_coexist_in_tower_config() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[extensions]
disabled = ["ast"]

[lsp.rust]
command = "rust-analyzer"
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("[lsp] section must not cause deny_unknown_fields error");
        // Existing section still parses.
        assert_eq!(cfg.extensions.disabled, vec!["ast"]);
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
        write_config(tmp.path(), "[extensions]\ndisabled = []\n");
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

    /// `[extensions]`, `[formatter]`, and `[lsp]` all coexist.
    #[test]
    fn extensions_formatter_lsp_coexist() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[extensions]
request_timeout_secs = 60
disabled = ["lsp"]

[formatter.tools.rustfmt]
argv = ["rustfmt"]
extensions = ["rs"]

[lsp.rust]
command = "rust-analyzer"
extensions = ["rs"]
"#,
        );
        let cfg = load(tmp.path()).expect("combined config must load");
        assert_eq!(cfg.extensions.request_timeout_secs, 60);
        assert_eq!(cfg.extensions.disabled, vec!["lsp"]);
        assert!(cfg.formatter.tools.contains_key("rustfmt"));
        assert!(cfg.lsp.for_extension("rs").is_some());
    }

    #[test]
    fn legacy_plugins_sections_are_rejected_with_manual_replacements() {
        let cases = [
            (
                "empty",
                "[plugins]\n",
                vec!["[extensions] disabled", "[formatter.tools.<id>]"],
            ),
            (
                "disabling",
                "[plugins]\ndisabled = [\"private-extension\"]\n",
                vec!["[extensions] disabled"],
            ),
            (
                "formatter-bearing",
                "[plugins.formatter.tools.secret-tool]\nargv = [\"secret-command\"]\n",
                vec!["[formatter.tools.<id>]"],
            ),
            (
                "mixed legacy and canonical",
                "[plugins]\ndisabled = [\"private-extension\"]\n\
                 [plugins.formatter.tools.secret-tool]\nargv = [\"secret-command\"]\n\
                 [extensions]\ndisabled = []\n\
                 [formatter]\n",
                vec!["[extensions] disabled", "[formatter.tools.<id>]"],
            ),
        ];

        for (name, body, expected_replacements) in cases {
            let tmp = tempdir().unwrap();
            write_config(tmp.path(), body);
            let config_path = tmp.path().join(".tower/config.toml");

            let err = load(tmp.path()).expect_err(name);
            let rendered = err.to_string();
            match err {
                ConfigError::LegacySection {
                    path,
                    section,
                    replacements,
                } => {
                    assert_eq!(path, config_path, "{name}");
                    assert_eq!(section, "[plugins]", "{name}");
                    assert_eq!(replacements, expected_replacements, "{name}");
                    assert_eq!(
                        rendered,
                        format!(
                            "legacy section [plugins] in {} is unsupported; replace it with {}",
                            config_path.display(),
                            expected_replacements.join(", ")
                        ),
                        "{name}"
                    );
                    assert!(!rendered.contains("private-extension"), "{name}");
                    assert!(!rendered.contains("secret-command"), "{name}");
                    assert!(!rendered.contains("secret-tool"), "{name}");
                }
                other => panic!("{name}: expected LegacySection, got {other:?}"),
            }
            assert_eq!(fs::read_to_string(config_path).unwrap(), body, "{name}");
        }
    }

    #[test]
    fn malformed_toml_and_unknown_canonical_keys_fail_closed_without_rewriting() {
        for (name, body) in [
            ("malformed", "[formatter\nsecret = \"unchanged\"\n"),
            (
                "unknown canonical key",
                "[formatter]\nsecret = \"unchanged\"\n",
            ),
        ] {
            let tmp = tempdir().unwrap();
            write_config(tmp.path(), body);
            let config_path = tmp.path().join(".tower/config.toml");

            assert!(
                matches!(load(tmp.path()), Err(ConfigError::Parse { .. })),
                "{name}"
            );
            assert_eq!(fs::read_to_string(config_path).unwrap(), body, "{name}");
        }
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

    #[test]
    fn daemon_config_deadline_defaults_are_ten_seconds() {
        let cfg = TowerConfig::default();

        assert_eq!(cfg.daemon.connection_timeout_secs, 10);
        assert_eq!(cfg.daemon.preparation_timeout_secs, 10);
        assert_eq!(
            cfg.daemon.connection_timeout(),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            cfg.daemon.preparation_timeout(),
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn daemon_config_deadline_accessors_use_independent_values() {
        let cfg: TowerConfig = toml::from_str(
            "[daemon]\nconnection_timeout_secs = 3\npreparation_timeout_secs = 17\n",
        )
        .unwrap();

        assert_eq!(
            cfg.daemon.connection_timeout(),
            std::time::Duration::from_secs(3)
        );
        assert_eq!(
            cfg.daemon.preparation_timeout(),
            std::time::Duration::from_secs(17)
        );
    }

    #[test]
    fn daemon_config_deadline_rejects_zero_connection_timeout() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[daemon]\nconnection_timeout_secs = 0\n");

        assert!(matches!(
            load(tmp.path()),
            Err(ConfigError::InvalidDaemonDeadline {
                field: "connection_timeout_secs",
                value: 0,
            })
        ));
    }

    #[test]
    fn daemon_config_deadline_rejects_zero_preparation_timeout() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[daemon]\npreparation_timeout_secs = 0\n");

        assert!(matches!(
            load(tmp.path()),
            Err(ConfigError::InvalidDaemonDeadline {
                field: "preparation_timeout_secs",
                value: 0,
            })
        ));
    }

    #[test]
    fn daemon_config_deadline_preserves_strict_parse_failures() {
        for body in [
            "[daemon]\nconnection_timeout_secs = \"ten\"\n",
            "[daemon]\npreparation_timeout_sec = 10\n",
        ] {
            let tmp = tempdir().unwrap();
            write_config(tmp.path(), body);
            assert!(matches!(load(tmp.path()), Err(ConfigError::Parse { .. })));
        }
    }

    #[test]
    fn extension_readiness_mapping_is_strict_and_preserves_builtin_families() {
        let default = TowerConfig::default();
        assert!(default.extensions.readiness.is_empty());
        assert!(default.extensions.readiness_controls.is_empty());

        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            "[extensions]\ndisabled = [\"slow_start\"]\n[extensions.readiness]\nslow_start = \"ast\"\nsemantic = \"lsp\"\n",
        );
        let cfg = load(tmp.path()).unwrap();
        assert_eq!(cfg.extensions.readiness["slow_start"], ReadinessFamily::Ast);
        assert_eq!(cfg.extensions.readiness["semantic"], ReadinessFamily::Lsp);
        assert!(
            cfg.extensions
                .disabled
                .iter()
                .any(|name| name == "slow_start")
        );

        for body in [
            "[extensions.readiness]\nslow_start = \"workspace\"\n",
            "[extensions.readiness]\nslow_start = 1\n",
        ] {
            let tmp = tempdir().unwrap();
            write_config(tmp.path(), body);
            assert!(matches!(load(tmp.path()), Err(ConfigError::Parse { .. })));
        }

        for (identity, family) in [("ast", "lsp"), ("lsp", "ast")] {
            let tmp = tempdir().unwrap();
            write_config(
                tmp.path(),
                &format!("[extensions.readiness]\n{identity} = \"{family}\"\n"),
            );
            assert!(matches!(
                load(tmp.path()),
                Err(ConfigError::InvalidExtensionConfiguration {
                    field: "readiness",
                    reason: ExtensionConfigurationReason::ConflictingBuiltinFamily,
                })
            ));
        }
    }

    #[test]
    fn extension_initialize_configuration_is_identity_scoped_and_json_compatible() {
        assert!(TowerConfig::default().extensions.initialize.is_empty());
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            "[extensions.initialize.alpha]\nmode = \"block\"\nnested = { enabled = true, count = 2 }\n[extensions.initialize.beta]\nmode = \"resume\"\n",
        );
        let cfg = load(tmp.path()).unwrap();
        assert_eq!(
            cfg.extensions.initialize["alpha"],
            serde_json::Map::from_iter([
                ("mode".to_owned(), serde_json::json!("block")),
                (
                    "nested".to_owned(),
                    serde_json::json!({"enabled": true, "count": 2})
                ),
            ])
        );
        assert_eq!(cfg.extensions.initialize["beta"]["mode"], "resume");

        for body in [
            "[extensions.initialize]\nalpha = \"not-an-object\"\n",
            "[extensions.initialize.alpha]\nvalue = nan\n",
        ] {
            let tmp = tempdir().unwrap();
            write_config(tmp.path(), body);
            assert!(load(tmp.path()).is_err());
        }

        for identity in ["lsp", "debug"] {
            let tmp = tempdir().unwrap();
            write_config(
                tmp.path(),
                &format!("[extensions.initialize.{identity}]\nmode = \"secret\"\n"),
            );
            assert!(matches!(
                load(tmp.path()),
                Err(ConfigError::InvalidExtensionConfiguration {
                    field: "initialize",
                    reason: ExtensionConfigurationReason::ReservedInitializeIdentity,
                })
            ));
        }

        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            "[lsp.rust]\ncommand = \"rust-analyzer\"\nextensions = [\"rs\"]\n[debug.rust]\ncommand = \"lldb-dap\"\nextensions = [\"rs\"]\nadapter_type = \"lldb\"\ndefault_timeout_secs = 10\nidle_ttl_secs = 30\n",
        );
        let cfg = load(tmp.path()).unwrap();
        assert!(cfg.lsp.for_extension("rs").is_some());
        assert!(cfg.debug.languages.contains_key("rust"));
    }

    #[test]
    fn extension_readiness_configuration_reports_fixed_validation_reasons() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            "[extensions.readiness]\nslow = \"ast\"\n[extensions.readiness_controls]\nslow = [\"probe\"]\n",
        );
        let cfg = load(tmp.path()).expect("valid identity-scoped controls must be preserved");
        assert_eq!(cfg.extensions.readiness_controls["slow"], ["probe"]);

        let cases = [
            (
                "[extensions.readiness]\nast = \"lsp\"\n",
                "readiness",
                ExtensionConfigurationReason::ConflictingBuiltinFamily,
            ),
            (
                "[extensions.readiness_controls]\nmissing = [\"probe\"]\n",
                "readiness_controls",
                ExtensionConfigurationReason::InvalidControlName,
            ),
            (
                "[extensions.readiness]\nslow = \"ast\"\n[extensions.readiness_controls]\nslow = []\n",
                "readiness_controls",
                ExtensionConfigurationReason::InvalidControlName,
            ),
            (
                "[extensions.readiness]\nslow = \"ast\"\n[extensions.readiness_controls]\nslow = [\"\"]\n",
                "readiness_controls",
                ExtensionConfigurationReason::InvalidControlName,
            ),
            (
                "[extensions.readiness]\nslow = \"ast\"\n[extensions.readiness_controls]\nslow = [\"probe\", \"probe\"]\n",
                "readiness_controls",
                ExtensionConfigurationReason::DuplicateControl,
            ),
            (
                "[extensions.readiness_controls]\nast = [\"tower_ast_get_outline\"]\n",
                "readiness_controls",
                ExtensionConfigurationReason::BuiltinExhaustiveControl,
            ),
            (
                "[extensions.initialize.lsp]\nmode = \"override\"\n",
                "initialize",
                ExtensionConfigurationReason::ReservedInitializeIdentity,
            ),
            (
                "[extensions.initialize.slow]\nvalue = nan\n",
                "initialize",
                ExtensionConfigurationReason::NonJsonInitializeValue,
            ),
        ];

        for (body, field, reason) in cases {
            let tmp = tempdir().unwrap();
            write_config(tmp.path(), body);
            assert!(matches!(
                load(tmp.path()),
                Err(ConfigError::InvalidExtensionConfiguration {
                    field: actual_field,
                    reason: actual_reason,
                }) if actual_field == field && actual_reason == reason
            ));
        }
    }
}
