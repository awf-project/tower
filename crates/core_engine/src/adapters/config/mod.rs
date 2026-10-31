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

use crate::adapters::path_identity::lexical_normalize;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ProjectId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidProjectId;

impl std::fmt::Display for InvalidProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("project id must not be empty")
    }
}

impl std::error::Error for InvalidProjectId {}

impl ProjectId {
    pub fn new(value: String) -> Result<Self, InvalidProjectId> {
        let value = value.trim();
        if value.is_empty() {
            return Err(InvalidProjectId);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectIdentity {
    pub id: ProjectId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryableProject {
    pub id: ProjectId,
    pub name: String,
    pub configured_path: PathBuf,
    pub resolved_path: PathBuf,
    pub canonical_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryableProjects(Vec<QueryableProject>);

impl QueryableProjects {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &QueryableProject> {
        self.0.iter()
    }

    #[must_use]
    pub fn get_by_id(&self, id: &ProjectId) -> Option<&QueryableProject> {
        self.0.iter().find(|project| project.id == *id)
    }

    #[must_use]
    pub fn get_by_name(&self, name: &str) -> Option<&QueryableProject> {
        self.0.iter().find(|project| project.name == name)
    }

    #[must_use]
    pub fn get_by_configured_path(&self, path: &Path) -> Option<&QueryableProject> {
        self.0
            .iter()
            .find(|project| project.configured_path == path)
    }

    #[must_use]
    pub fn get_by_canonical_path(&self, path: &Path) -> Option<&QueryableProject> {
        self.0
            .iter()
            .find(|project| project.canonical_path.as_deref() == Some(path))
    }
}

#[derive(Debug, Clone)]
pub struct ProjectsConfig {
    queryable: QueryableProjects,
    external_availability_timeout: std::time::Duration,
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            queryable: QueryableProjects::default(),
            external_availability_timeout: std::time::Duration::from_secs(10),
        }
    }
}

impl ProjectsConfig {
    #[must_use]
    pub fn queryable(&self) -> &QueryableProjects {
        &self.queryable
    }

    #[must_use]
    pub fn external_availability_timeout(&self) -> std::time::Duration {
        self.external_availability_timeout
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryableProjectIdentityField {
    Id,
    Name,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryableProjectPathReason {
    Empty,
    Invalid,
    NotDirectory,
}

/// Parsed `.tower/config.toml`. Unknown top-level keys are rejected.
#[derive(Debug, Default)]
pub struct TowerConfig {
    pub formatter: FormatterConfig,
    /// `[lsp]` section — language-server bindings (LSP plan 1, Task 8).
    ///
    /// Absent → `LspConfig::default()` (empty; no servers). Populated by
    /// wiring in `main.rs`; production code reads `config.lsp` directly
    /// rather than calling `parse_lsp_config` again.
    pub lsp: LspConfig,
    pub lint: LintConfig,
    pub debug: DebugConfig,
    /// `[extensions]` section — sidecar extension runtime settings (spec 24).
    ///
    /// Absent → `ExtensionConfig::default()` (no request timeout).
    pub extensions: ExtensionConfig,
    /// `[daemon]` section — shared-daemon runtime settings.
    /// Absent → `DaemonConfig::default()` (30-second idle timeout).
    pub daemon: DaemonConfig,
    pub projects: ProjectsConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTowerConfig {
    #[serde(default)]
    formatter: FormatterConfig,
    #[serde(default)]
    lsp: LspConfig,
    #[serde(default)]
    lint: LintConfig,
    #[serde(default)]
    debug: DebugConfig,
    #[serde(default)]
    extensions: ExtensionConfig,
    #[serde(default)]
    daemon: DaemonConfig,
    #[serde(default)]
    projects: Option<toml::Value>,
}

impl<'de> Deserialize<'de> for TowerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = toml::Value::deserialize(deserializer)?;
        validate_timeout_values(&value).map_err(serde::de::Error::custom)?;
        let raw: RawTowerConfig = value.try_into().map_err(serde::de::Error::custom)?;
        let _ = raw.projects;
        Ok(Self {
            formatter: raw.formatter,
            lsp: raw.lsp,
            lint: raw.lint,
            debug: raw.debug,
            extensions: raw.extensions,
            daemon: raw.daemon,
            projects: ProjectsConfig::default(),
        })
    }
}

impl TowerConfig {
    pub fn lsp_budget_required(&self) -> bool {
        !self.lsp.servers.is_empty() && !self.extensions.disabled.iter().any(|name| name == "lsp")
    }

    pub fn lint_request_timeout(&self) -> Result<Option<std::time::Duration>, ConfigError> {
        let mut maximum = self.extensions.request_timeout();
        for (language, command) in &self.lint.commands {
            let Some(seconds) = command.timeout_secs else {
                continue;
            };
            let invalid_deadline = || ConfigError::InvalidLintDeadline {
                field: format!("lint.{language}.timeout_secs"),
                value: seconds,
            };
            let derived_seconds = seconds
                .checked_mul(2)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(invalid_deadline)?;
            validate_deadline_seconds(derived_seconds).ok_or_else(invalid_deadline)?;
            let derived = std::time::Duration::from_secs(derived_seconds);
            maximum = Some(maximum.map_or(derived, |current| current.max(derived)));
        }
        Ok(maximum)
    }
}

/// `[extensions]` section (spec 24 — supervision & fault model; spec 25 — disable list).
///
/// Controls per-request timeout, respawn backoff, and which extensions are
/// skipped at startup.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,

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
    pub const LSP_SUPERVISION_MARGIN: std::time::Duration = std::time::Duration::from_secs(1);

    #[must_use]
    pub fn request_timeout(&self) -> Option<std::time::Duration> {
        self.request_timeout_secs
            .map(std::time::Duration::from_secs)
    }

    pub fn lsp_diagnostics_timeout(&self) -> Result<Option<std::time::Duration>, ConfigError> {
        let Some(request_timeout_secs) = self.request_timeout_secs else {
            return Ok(None);
        };
        let timeout = std::time::Duration::from_secs(request_timeout_secs)
            .checked_sub(Self::LSP_SUPERVISION_MARGIN)
            .filter(|timeout| !timeout.is_zero())
            .ok_or(ConfigError::InvalidLspDiagnosticsDeadline {
                request_timeout_secs,
                required_margin_secs: Self::LSP_SUPERVISION_MARGIN.as_secs(),
            })?;
        Ok(Some(timeout))
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
    #[serde(default)]
    pub connection_timeout_secs: Option<u64>,
    #[serde(default)]
    pub preparation_timeout_secs: Option<u64>,
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

    #[must_use]
    pub fn connection_timeout(&self) -> Option<std::time::Duration> {
        self.connection_timeout_secs
            .map(std::time::Duration::from_secs)
    }

    #[must_use]
    pub fn preparation_timeout(&self) -> Option<std::time::Duration> {
        self.preparation_timeout_secs
            .map(std::time::Duration::from_secs)
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: Self::default_idle_timeout_secs(),
            connection_timeout_secs: None,
            preparation_timeout_secs: None,
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
    #[serde(default)]
    pub timeout_secs: Option<u64>,
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
    InvalidLspDiagnosticsDeadline {
        request_timeout_secs: u64,
        required_margin_secs: u64,
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
    InvalidQueryableProjectIdentity {
        index: usize,
        field: QueryableProjectIdentityField,
    },
    InvalidQueryableProjectPath {
        index: usize,
        reason: QueryableProjectPathReason,
    },
    DuplicateQueryableProjectId {
        id: ProjectId,
    },
    DuplicateQueryableProjectName {
        name: String,
    },
    DuplicateQueryableProjectPath {
        first_id: ProjectId,
        second_id: ProjectId,
    },
    InvalidExternalAvailabilityTimeout {
        value: i64,
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
            ConfigError::InvalidLspDiagnosticsDeadline {
                request_timeout_secs,
                required_margin_secs,
            } => write!(
                f,
                "invalid LSP diagnostics deadline {request_timeout_secs}: requires a {required_margin_secs}-second supervision margin"
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
            ConfigError::InvalidQueryableProjectIdentity { index, field } => {
                write!(
                    f,
                    "invalid queryable project identity at index {index}: {field:?}"
                )
            }
            ConfigError::InvalidQueryableProjectPath { index, reason } => {
                write!(
                    f,
                    "invalid queryable project path at index {index}: {reason:?}"
                )
            }
            ConfigError::DuplicateQueryableProjectId { id } => {
                write!(f, "duplicate queryable project id: {}", id.as_str())
            }
            ConfigError::DuplicateQueryableProjectName { name } => {
                write!(f, "duplicate queryable project name: {name}")
            }
            ConfigError::DuplicateQueryableProjectPath {
                first_id,
                second_id,
            } => write!(
                f,
                "duplicate queryable project path for {} and {}",
                first_id.as_str(),
                second_id.as_str()
            ),
            ConfigError::InvalidExternalAvailabilityTimeout { value } => write!(
                f,
                "invalid projects.external_availability_timeout_secs: {value}"
            ),
        }
    }
}

/// Load `<workspace_root>/.tower/config.toml`.
///
/// Missing file → `Ok(TowerConfig::default())`. Present but unreadable or
/// invalid → `Err`.
pub fn load(workspace_root: &Path) -> Result<TowerConfig, ConfigError> {
    load_with_path_canonicalizer(workspace_root, |path| fs::canonicalize(path))
}

/// Loads configuration with its external path-resolution boundary supplied by
/// the caller. Production uses [`fs::canonicalize`]; co-located tests can
/// deterministically exercise OS failures that cannot be made portable by
/// changing filesystem permissions.
fn load_with_path_canonicalizer(
    workspace_root: &Path,
    canonicalize_path: impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<TowerConfig, ConfigError> {
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
        source: timeout_parse_error(&contents, &source).unwrap_or(source),
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

    validate_timeout_values(&raw).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;
    let mut config: TowerConfig =
        toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
    config.projects = parse_projects_config(&raw, workspace_root, canonicalize_path).map_err(
        |error| match error {
            ProjectConfigLoadError::Parse(source) => ConfigError::Parse {
                path: path.clone(),
                source,
            },
            ProjectConfigLoadError::Config(error) => error,
        },
    )?;
    validate_daemon_deadlines(&config.daemon)?;
    validate_extension_configuration(&config.extensions)?;
    config.lint_request_timeout()?;
    if config.lsp_budget_required() {
        config.extensions.lsp_diagnostics_timeout()?;
    }
    Ok(config)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectsConfig {
    #[serde(default)]
    queryable: Vec<RawQueryableProject>,
    #[serde(default = "default_external_availability_timeout_secs")]
    external_availability_timeout_secs: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQueryableProject {
    id: String,
    name: String,
    path: String,
}

const fn default_external_availability_timeout_secs() -> i64 {
    10
}

enum ProjectConfigLoadError {
    Parse(toml::de::Error),
    Config(ConfigError),
}

fn parse_projects_config(
    raw: &toml::Value,
    workspace_root: &Path,
    canonicalize_path: impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<ProjectsConfig, ProjectConfigLoadError> {
    let raw_projects = raw
        .get("projects")
        .cloned()
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    let raw_projects: RawProjectsConfig = raw_projects
        .try_into()
        .map_err(ProjectConfigLoadError::Parse)?;
    if raw_projects.external_availability_timeout_secs <= 0 {
        return Err(ProjectConfigLoadError::Config(
            ConfigError::InvalidExternalAvailabilityTimeout {
                value: raw_projects.external_availability_timeout_secs,
            },
        ));
    }

    let absolute_workspace_root = if workspace_root.is_absolute() {
        lexical_normalize(workspace_root)
    } else {
        let current_dir = std::env::current_dir().map_err(|_| {
            ProjectConfigLoadError::Config(ConfigError::InvalidQueryableProjectPath {
                index: 0,
                reason: QueryableProjectPathReason::Invalid,
            })
        })?;
        lexical_normalize(&current_dir.join(workspace_root))
    };
    let mut projects = Vec::<QueryableProject>::with_capacity(raw_projects.queryable.len());

    for (index, raw_project) in raw_projects.queryable.into_iter().enumerate() {
        let (id, name) = validate_project_identity(raw_project.id, &raw_project.name, index)
            .map_err(ProjectConfigLoadError::Config)?;
        if projects.iter().any(|project| project.id == id) {
            return Err(ProjectConfigLoadError::Config(
                ConfigError::DuplicateQueryableProjectId { id },
            ));
        }
        if projects.iter().any(|project| project.name == name) {
            return Err(ProjectConfigLoadError::Config(
                ConfigError::DuplicateQueryableProjectName { name },
            ));
        }

        let (configured_path, resolved_path, canonical_path) = resolve_project_path(
            raw_project.path,
            &absolute_workspace_root,
            index,
            &canonicalize_path,
        )?;
        if let Some(first) = projects.iter().find(|project| {
            project.resolved_path == resolved_path
                || canonical_path
                    .as_ref()
                    .zip(project.canonical_path.as_ref())
                    .is_some_and(|(left, right)| left == right)
        }) {
            return Err(ProjectConfigLoadError::Config(
                ConfigError::DuplicateQueryableProjectPath {
                    first_id: first.id.clone(),
                    second_id: id,
                },
            ));
        }
        projects.push(QueryableProject {
            id,
            name,
            configured_path,
            resolved_path,
            canonical_path,
        });
    }

    Ok(ProjectsConfig {
        queryable: QueryableProjects(projects),
        external_availability_timeout: std::time::Duration::from_secs(
            raw_projects.external_availability_timeout_secs as u64,
        ),
    })
}

fn validate_project_identity(
    raw_id: String,
    raw_name: &str,
    index: usize,
) -> Result<(ProjectId, String), ConfigError> {
    let id = ProjectId::new(raw_id).map_err(|_| ConfigError::InvalidQueryableProjectIdentity {
        index,
        field: QueryableProjectIdentityField::Id,
    })?;
    let name = raw_name.trim().to_owned();
    if name.is_empty() {
        return Err(ConfigError::InvalidQueryableProjectIdentity {
            index,
            field: QueryableProjectIdentityField::Name,
        });
    }
    Ok((id, name))
}

fn resolve_project_path(
    raw_path: String,
    absolute_workspace_root: &Path,
    index: usize,
    canonicalize_path: &impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>), ProjectConfigLoadError> {
    let invalid = |reason| {
        ProjectConfigLoadError::Config(ConfigError::InvalidQueryableProjectPath { index, reason })
    };
    if raw_path.trim().is_empty() {
        return Err(invalid(QueryableProjectPathReason::Empty));
    }
    if raw_path.contains('\0') {
        return Err(invalid(QueryableProjectPathReason::Invalid));
    }
    let configured_path = PathBuf::from(raw_path);
    let absolute_path = if configured_path.is_absolute() {
        configured_path.clone()
    } else {
        absolute_workspace_root.join(&configured_path)
    };
    // Resolve the original spelling first: collapsing `..` earlier changes
    // filesystem traversal when an intermediate component is a symlink.
    let canonical_path = match canonicalize_path(&absolute_path) {
        Ok(path) => {
            let metadata =
                fs::metadata(&path).map_err(|_| invalid(QueryableProjectPathReason::Invalid))?;
            if !metadata.is_dir() {
                return Err(invalid(QueryableProjectPathReason::NotDirectory));
            }
            Some(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
            return Err(invalid(QueryableProjectPathReason::NotDirectory));
        }
        Err(_) => return Err(invalid(QueryableProjectPathReason::Invalid)),
    };
    let resolved_path = lexical_normalize(&absolute_path);
    if !resolved_path.is_absolute() {
        return Err(invalid(QueryableProjectPathReason::Invalid));
    }
    Ok((configured_path, resolved_path, canonical_path))
}

fn timeout_parse_error(contents: &str, source: &toml::de::Error) -> Option<toml::de::Error> {
    let offset = source.span()?.start.min(contents.len());
    let prefix = &contents[..offset];
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let line_end = contents[offset..]
        .find('\n')
        .map_or(contents.len(), |index| offset + index);
    let line = contents[line_start..line_end].trim();
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if !matches!(
        key,
        "request_timeout_secs" | "timeout_secs" | "default_timeout_secs" | "record_timeout_secs"
    ) {
        return None;
    }
    let table = prefix
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('[') && line.ends_with(']'))?
        .trim_matches(['[', ']']);
    let field = match (table, key) {
        ("extensions", "request_timeout_secs") => "extensions.request_timeout_secs".to_string(),
        (table, "timeout_secs") if table.starts_with("formatter.tools.") => {
            format!("{table}.timeout_secs")
        }
        (table, "timeout_secs") if table.starts_with("lint.") => {
            format!("{table}.timeout_secs")
        }
        (table, "default_timeout_secs") if table.starts_with("debug.") => {
            format!("{table}.default_timeout_secs")
        }
        ("debug.record", "record_timeout_secs") => "debug.record.record_timeout_secs".to_string(),
        _ => return None,
    };
    Some(<toml::de::Error as serde::de::Error>::custom(format!(
        "{field}: invalid positive deadline value {}",
        value.trim()
    )))
}

fn validate_deadline_seconds(seconds: u64) -> Option<()> {
    seconds.checked_mul(1_000)?;
    std::time::Instant::now().checked_add(std::time::Duration::from_secs(seconds))?;
    Some(())
}

fn validate_timeout_value(field: &str, value: &toml::Value) -> Result<(), toml::de::Error> {
    let valid = value
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .filter(|value| validate_deadline_seconds(*value).is_some())
        .is_some();
    if valid {
        return Ok(());
    }
    Err(<toml::de::Error as serde::de::Error>::custom(format!(
        "{field}: invalid positive deadline value {value}"
    )))
}

fn validate_timeout_values(raw: &toml::Value) -> Result<(), toml::de::Error> {
    if let Some(value) = raw
        .get("extensions")
        .and_then(|section| section.get("request_timeout_secs"))
    {
        validate_timeout_value("extensions.request_timeout_secs", value)?;
    }
    if let Some(tools) = raw
        .get("formatter")
        .and_then(|section| section.get("tools"))
        .and_then(toml::Value::as_table)
    {
        for (id, tool) in tools {
            if let Some(value) = tool.get("timeout_secs") {
                validate_timeout_value(&format!("formatter.tools.{id}.timeout_secs"), value)?;
            }
        }
    }
    if let Some(debug) = raw.get("debug").and_then(toml::Value::as_table) {
        for (language, config) in debug {
            let (field, value) = if language == "record" {
                (
                    "debug.record.record_timeout_secs".to_string(),
                    config.get("record_timeout_secs"),
                )
            } else {
                (
                    format!("debug.{language}.default_timeout_secs"),
                    config.get("default_timeout_secs"),
                )
            };
            if let Some(value) = value {
                validate_timeout_value(&field, value)?;
            }
        }
    }
    lint::validate_timeout_values(raw, validate_timeout_value)
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
        if let Some(value) = value
            && (value == 0 || validate_deadline_seconds(value).is_none())
        {
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

    fn assert_parse_error_names(body: &str, field: &str, value: &str) {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), body);
        let error = load(tmp.path()).expect_err("invalid timeout must be rejected");
        match error {
            ConfigError::Parse { source, .. } => {
                let rendered = source.to_string();
                assert!(rendered.contains(field), "{rendered}");
                assert!(rendered.contains(value), "{rendered}");
            }
            other => panic!("expected ConfigError::Parse, got {other:?}"),
        }
    }

    fn max_seconds_with_representable_millis() -> u64 {
        u64::MAX / 1_000
    }

    fn first_seconds_that_overflow_the_platform_deadline() -> u64 {
        let maximum = max_seconds_with_representable_millis();
        if std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(maximum))
            .is_some()
        {
            return maximum + 1;
        }

        let mut lower = 1_u64;
        let mut upper = maximum;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            if std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(middle))
                .is_some()
            {
                lower = middle + 1;
            } else {
                upper = middle;
            }
        }
        lower
    }

    #[test]
    fn direct_deserialization_rejects_invalid_explicit_timeout_deadlines() {
        let invalid = max_seconds_with_representable_millis() + 1;
        for (field, source) in [
            (
                "extensions.request_timeout_secs",
                format!("[extensions]\nrequest_timeout_secs = {invalid}\n"),
            ),
            (
                "formatter.tools.rustfmt.timeout_secs",
                format!(
                    "[formatter.tools.rustfmt]\nargv = [\"rustfmt\"]\nextensions = [\"rs\"]\ntimeout_secs = {invalid}\n"
                ),
            ),
            (
                "lint.rust.timeout_secs",
                format!(
                    "[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = {invalid}\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n"
                ),
            ),
            (
                "debug.rust.default_timeout_secs",
                format!(
                    "[debug.rust]\nextensions = [\"rs\"]\ncommand = \"lldb-dap\"\nadapter_type = \"lldb\"\ndefault_timeout_secs = {invalid}\nidle_ttl_secs = 300\n"
                ),
            ),
            (
                "debug.record.record_timeout_secs",
                format!("[debug.record]\nbackend = \"rr\"\nrecord_timeout_secs = {invalid}\n"),
            ),
        ] {
            let error = toml::from_str::<TowerConfig>(&source)
                .expect_err("an invalid explicit deadline must be rejected");
            assert!(error.to_string().contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn extension_request_timeout_absence_and_positive_value_are_preserved() {
        let absent = TowerConfig::default();
        assert_eq!(absent.extensions.request_timeout_secs, None);
        assert_eq!(absent.extensions.request_timeout(), None);

        let configured: TowerConfig = toml::from_str("[extensions]\nrequest_timeout_secs = 17\n")
            .expect("positive extension timeout parses");
        assert_eq!(configured.extensions.request_timeout_secs, Some(17));
        assert_eq!(
            configured.extensions.request_timeout(),
            Some(std::time::Duration::from_secs(17))
        );
    }

    #[test]
    fn extension_request_timeout_rejects_zero_negative_non_integer_and_u64_overflow() {
        for value in [
            "0",
            "-1",
            "\"2\"",
            "9223372036854775808",
            "9223372036854775807",
        ] {
            assert_parse_error_names(
                &format!("[extensions]\nrequest_timeout_secs = {value}\n"),
                "extensions.request_timeout_secs",
                value.trim_matches('"'),
            );
        }
    }

    #[test]
    fn extension_request_timeout_rejects_millisecond_and_platform_deadline_overflows() {
        let millisecond_overflow = max_seconds_with_representable_millis() + 1;
        let deadline_overflow = first_seconds_that_overflow_the_platform_deadline();

        for (name, seconds) in [
            ("millisecond representation", millisecond_overflow),
            ("platform or millisecond deadline", deadline_overflow),
        ] {
            assert_parse_error_names(
                &format!("[extensions]\nrequest_timeout_secs = {seconds}\n"),
                "extensions.request_timeout_secs",
                &seconds.to_string(),
            );
            assert_ne!(
                seconds,
                u64::MAX,
                "{name} must be a representable u64 input"
            );
        }
    }

    #[test]
    fn formatter_timeout_absence_and_positive_value_are_preserved() {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            "[formatter.tools.rustfmt]\nargv = [\"rustfmt\"]\nextensions = [\"rs\"]\n",
        );
        let absent = load(tmp.path()).expect("formatter timeout may be absent");
        assert_eq!(absent.formatter.tools["rustfmt"].timeout_secs, None);

        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            "[formatter.tools.rustfmt]\nargv = [\"rustfmt\"]\nextensions = [\"rs\"]\ntimeout_secs = 17\n",
        );
        let configured = load(tmp.path()).expect("positive formatter timeout parses");
        assert_eq!(configured.formatter.tools["rustfmt"].timeout_secs, Some(17));
    }

    #[test]
    fn formatter_timeout_rejects_zero_negative_non_integer_and_u64_overflow() {
        for value in [
            "0",
            "-1",
            "\"2\"",
            "9223372036854775808",
            "9223372036854775807",
        ] {
            assert_parse_error_names(
                &format!(
                    "[formatter.tools.rustfmt]\nargv = [\"rustfmt\"]\nextensions = [\"rs\"]\ntimeout_secs = {value}\n"
                ),
                "formatter.tools.rustfmt.timeout_secs",
                value.trim_matches('"'),
            );
        }
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
    fn lint_request_timeout_preserves_absence_and_derives_explicit_supervision_budgets() {
        for (host_secs, lint_entries, expected_secs) in [
            (None, "", None),
            (Some(1), "", Some(1)),
            (
                None,
                "[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = 2\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                Some(5),
            ),
            (
                Some(2),
                "[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = 2\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                Some(5),
            ),
            (
                Some(4),
                "[lint.go]\ncommand = \"golangci-lint\"\ntimeout_secs = 7\nextensions = [\"go\"]\nformat = \"generic-regex\"\ntarget = \"append\"\n[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = 3\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                Some(15),
            ),
            (
                Some(30),
                "[lint.rust]\ncommand = \"cargo\"\ntimeout_secs = 7\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
                Some(30),
            ),
        ] {
            let host = host_secs
                .map(|seconds| format!("[extensions]\nrequest_timeout_secs = {seconds}\n"))
                .unwrap_or_default();
            let source = format!("{host}{lint_entries}");
            let config: TowerConfig = toml::from_str(&source).expect("valid timeout fixture");

            assert_eq!(
                config
                    .lint_request_timeout()
                    .expect("representable lint deadline"),
                expected_secs.map(std::time::Duration::from_secs),
                "host={host_secs:?}, lint={lint_entries:?}"
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
            .timeout_secs = Some(u64::MAX);
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

        let millisecond_overflow = max_seconds_with_representable_millis() / 2 + 1;
        let mut unrepresentable_deadline: TowerConfig = toml::from_str(
            "[lint.go]\ncommand = \"golangci-lint\"\nextensions = [\"go\"]\nformat = \"generic-regex\"\ntarget = \"append\"\n[lint.rust]\ncommand = \"cargo\"\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
        )
        .unwrap();
        for command in unrepresentable_deadline.lint.commands.values_mut() {
            command.timeout_secs = Some(millisecond_overflow);
        }
        assert!(matches!(
            unrepresentable_deadline.lint_request_timeout(),
            Err(ConfigError::InvalidLintDeadline { field, value })
                if field == "lint.go.timeout_secs" && value == millisecond_overflow
        ));

        let deadline_overflow = first_seconds_that_overflow_the_platform_deadline();
        let lint_seconds = deadline_overflow.div_ceil(2);
        let mut platform_deadline_overflow: TowerConfig = toml::from_str(
            "[lint.rust]\ncommand = \"cargo\"\nextensions = [\"rs\"]\nformat = \"rustc-json\"\ntarget = \"append\"\n",
        )
        .unwrap();
        platform_deadline_overflow
            .lint
            .commands
            .get_mut("rust")
            .unwrap()
            .timeout_secs = Some(lint_seconds);
        assert!(matches!(
            platform_deadline_overflow.lint_request_timeout(),
            Err(ConfigError::InvalidLintDeadline { field, value })
                if field == "lint.rust.timeout_secs" && value == lint_seconds
        ));
    }

    #[test]
    fn lsp_diagnostics_timeout_preserves_margin_and_exempts_inactive_lsp() {
        assert_eq!(
            ExtensionConfig::default()
                .lsp_diagnostics_timeout()
                .expect("absent host budget is unlimited"),
            None
        );

        let configured = ExtensionConfig {
            request_timeout_secs: Some(17),
            ..ExtensionConfig::default()
        };
        assert_eq!(
            configured
                .lsp_diagnostics_timeout()
                .expect("positive host budget leaves the LSP supervision margin"),
            Some(std::time::Duration::from_secs(16))
        );

        {
            let request_timeout_secs = 1;
            let tmp = tempdir().unwrap();
            write_config(
                tmp.path(),
                &format!(
                    "[extensions]\nrequest_timeout_secs = {request_timeout_secs}\n\
                     [lsp.rust]\ncommand = \"rust-analyzer\"\nextensions = [\"rs\"]\n"
                ),
            );

            assert!(matches!(
                load(tmp.path()),
                Err(ConfigError::InvalidLspDiagnosticsDeadline {
                    request_timeout_secs: actual,
                    required_margin_secs: 1,
                }) if actual == request_timeout_secs
            ));
        }

        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[extensions]\nrequest_timeout_secs = 1\n");
        load(tmp.path()).expect("an absent LSP configuration must not require its margin");

        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            "[extensions]\nrequest_timeout_secs = 1\ndisabled = [\"lsp\"]\n\
             [lsp.rust]\ncommand = \"rust-analyzer\"\nextensions = [\"rs\"]\n",
        );
        load(tmp.path()).expect("a disabled LSP configuration must not require its margin");
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

    /// `[extensions] request_timeout = 5` is parsed correctly.
    #[test]
    fn parses_extensions_request_timeout() {
        let tmp = tempdir().unwrap();
        write_config(tmp.path(), "[extensions]\nrequest_timeout_secs = 5\n");
        let cfg = load(tmp.path()).expect("valid extensions config");
        assert_eq!(cfg.extensions.request_timeout_secs, Some(5));
        assert_eq!(
            cfg.extensions.request_timeout(),
            Some(std::time::Duration::from_secs(5))
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
        assert_eq!(cfg.extensions.request_timeout_secs, Some(60));
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
    fn daemon_connection_and_preparation_deadlines_are_unlimited_when_absent() {
        let cfg = TowerConfig::default();

        assert_eq!(cfg.daemon.connection_timeout_secs, None);
        assert_eq!(cfg.daemon.preparation_timeout_secs, None);
        assert_eq!(cfg.daemon.connection_timeout(), None);
        assert_eq!(cfg.daemon.preparation_timeout(), None);
    }

    #[test]
    fn daemon_config_deadline_accessors_use_independent_values() {
        let cfg: TowerConfig = toml::from_str(
            "[daemon]\nconnection_timeout_secs = 3\npreparation_timeout_secs = 17\n",
        )
        .unwrap();

        assert_eq!(
            cfg.daemon.connection_timeout(),
            Some(std::time::Duration::from_secs(3))
        );
        assert_eq!(
            cfg.daemon.preparation_timeout(),
            Some(std::time::Duration::from_secs(17))
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
    fn daemon_config_deadlines_reject_platform_deadline_overflow() {
        let value = first_seconds_that_overflow_the_platform_deadline();
        for field in ["connection_timeout_secs", "preparation_timeout_secs"] {
            let tmp = tempdir().unwrap();
            write_config(tmp.path(), &format!("[daemon]\n{field} = {value}\n"));
            assert!(
                matches!(
                    load(tmp.path()),
                    Err(ConfigError::InvalidDaemonDeadline {
                        field: actual_field,
                        value: actual_value,
                    }) if actual_field == field && actual_value == value
                ),
                "{field} must reject an absolute deadline that the platform cannot represent"
            );
        }
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

    #[test]
    fn configured_queryable_projects_accept_distinct_canonical_workspaces() {
        let tmp = tempfile::Builder::new()
            .prefix("queryable-projects-")
            .tempdir_in(".")
            .unwrap();
        let workspace_root = fs::canonicalize(tmp.path()).unwrap();
        let alpha = workspace_root.join("alpha");
        let beta = workspace_root.join("beta");
        fs::create_dir_all(&alpha).unwrap();
        fs::create_dir_all(&beta).unwrap();

        write_config(
            &workspace_root,
            &format!(
                r#"
[[projects.queryable]]
id = "  alpha-id  "
name = "  Alpha  "
path = "alpha"

[[projects.queryable]]
id = "beta-id"
name = "Beta"
path = "{}"
"#,
                beta.display()
            ),
        );

        let config = load(&workspace_root).expect("distinct queryable projects must load");
        let projects = config.projects.queryable();
        assert_eq!(projects.iter().len(), 2);
        let configured: Vec<_> = projects.iter().collect();
        assert_eq!(configured[0].id.as_str(), "alpha-id");
        assert_eq!(configured[0].name, "Alpha");
        assert_eq!(configured[0].configured_path, PathBuf::from("alpha"));
        assert_eq!(configured[0].resolved_path, alpha);
        assert_eq!(configured[0].canonical_path, Some(alpha.clone()));
        assert_eq!(configured[1].id.as_str(), "beta-id");
        assert_eq!(configured[1].name, "Beta");
        assert_eq!(configured[1].configured_path, beta);
        assert_eq!(configured[1].resolved_path, beta);
        assert_eq!(configured[1].canonical_path, Some(beta.clone()));

        let alpha_id = ProjectId::new("alpha-id".to_owned()).unwrap();
        assert_eq!(projects.get_by_id(&alpha_id), Some(configured[0]));
        assert_eq!(projects.get_by_name("Alpha"), Some(configured[0]));
        assert_eq!(
            projects.get_by_configured_path(Path::new("alpha")),
            Some(configured[0])
        );
        assert_eq!(projects.get_by_canonical_path(&alpha), Some(configured[0]));
        assert_eq!(projects.get_by_name("alpha"), None);
        assert_eq!(projects.get_by_configured_path(&alpha), None);
        assert_eq!(projects.get_by_canonical_path(Path::new("alpha")), None);

        assert_eq!(
            serde_json::to_value(&alpha_id).unwrap(),
            serde_json::json!("alpha-id")
        );
        assert_eq!(
            serde_json::to_value(ProjectIdentity {
                id: alpha_id,
                name: "Alpha".to_owned(),
            })
            .unwrap(),
            serde_json::json!({"id": "alpha-id", "name": "Alpha"})
        );
        assert_eq!(ProjectId::new("   ".to_owned()), Err(InvalidProjectId));
        assert_eq!(InvalidProjectId.to_string(), "project id must not be empty");

        #[cfg(unix)]
        {
            let symlink_root = workspace_root.join("symlink-root");
            let real_root = workspace_root.join("real-root");
            let real_target = real_root.join("target");
            fs::create_dir_all(&real_target).unwrap();
            fs::create_dir_all(symlink_root.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&real_root, &symlink_root).unwrap();
            write_config(
                &real_target,
                "[[projects.queryable]]\nid = \"symlinked\"\nname = \"Symlinked\"\npath = \"../../symlink-root/target/../target\"\n",
            );
            let config = load(&real_target).unwrap();
            let project = config.projects.queryable().iter().next().unwrap();
            assert_eq!(
                project.configured_path,
                Path::new("../../symlink-root/target/../target")
            );
            assert_eq!(project.resolved_path, symlink_root.join("target"));
            assert_eq!(project.canonical_path, Some(real_target));
        }
    }

    #[test]
    fn configured_queryable_projects_reject_duplicate_id_name_or_resolved_canonical_path() {
        #[derive(Clone, Copy)]
        enum ExpectedError {
            Identity(usize, QueryableProjectIdentityField),
            DuplicateId(&'static str),
            DuplicateName(&'static str),
            DuplicatePath(&'static str, &'static str),
            Path(usize, QueryableProjectPathReason),
        }

        let tmp = tempdir().unwrap();
        let existing = tmp.path().join("existing");
        let other = tmp.path().join("other");
        let regular_file = tmp.path().join("not-a-directory");
        let traversal_through_file = regular_file.join("descendant");
        let privacy_sentinel = tmp.path().join("unrelated-private-queryable-project-root");
        fs::create_dir_all(&existing).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(&regular_file, "file").unwrap();

        let cases = [
            (
                "empty id",
                format!("[[projects.queryable]]\nid = \"  \"\nname = \"Name\"\npath = \"{}\"\n", existing.display()),
                ExpectedError::Identity(0, QueryableProjectIdentityField::Id),
            ),
            (
                "empty name is contextualized with its zero-based index",
                format!("[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \"{}\"\n[[projects.queryable]]\nid = \"two\"\nname = \" \\t \"\npath = \"{}\"\n", existing.display(), other.display()),
                ExpectedError::Identity(1, QueryableProjectIdentityField::Name),
            ),
            (
                "trimmed duplicate id",
                format!("[[projects.queryable]]\nid = \"same\"\nname = \"One\"\npath = \"{}\"\n[[projects.queryable]]\nid = \" same \"\nname = \"Two\"\npath = \"{}\"\n", existing.display(), other.display()),
                ExpectedError::DuplicateId("same"),
            ),
            (
                "trimmed duplicate name",
                format!("[[projects.queryable]]\nid = \"one\"\nname = \"Same\"\npath = \"{}\"\n[[projects.queryable]]\nid = \"two\"\nname = \" Same \"\npath = \"{}\"\n", existing.display(), other.display()),
                ExpectedError::DuplicateName("Same"),
            ),
            (
                "unresolved lexical alias",
                "[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \"missing/../missing\"\n[[projects.queryable]]\nid = \"two\"\nname = \"Two\"\npath = \"missing\"\n".to_owned(),
                ExpectedError::DuplicatePath("one", "two"),
            ),
            (
                "empty path",
                "[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \" \\t \"\n".to_owned(),
                ExpectedError::Path(0, QueryableProjectPathReason::Empty),
            ),
            (
                "embedded nul",
                "[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \"bad\\u0000path\"\n".to_owned(),
                ExpectedError::Path(0, QueryableProjectPathReason::Invalid),
            ),
            (
                "file root",
                format!("[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \"{}\"\n", regular_file.display()),
                ExpectedError::Path(0, QueryableProjectPathReason::NotDirectory),
            ),
            (
                "traversal through a file",
                format!("[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \"{}\"\n", traversal_through_file.display()),
                ExpectedError::Path(0, QueryableProjectPathReason::NotDirectory),
            ),
        ];

        for (name, body, expected) in cases {
            // Every rejected path is accompanied by an unrelated raw path in
            // the parser input. This proves both public renderings redact
            // unrelated configured paths, including for failures before the
            // resolver reaches the offending entry (such as an empty path).
            write_config(
                tmp.path(),
                &format!(
                    "{body}[[projects.queryable]]\nid = \"privacy-sentinel\"\nname = \"Privacy sentinel\"\npath = \"{}\"\n",
                    privacy_sentinel.display()
                ),
            );
            let error = load(tmp.path()).expect_err(name);
            match (expected, &error) {
                (
                    ExpectedError::Identity(expected_index, expected_field),
                    ConfigError::InvalidQueryableProjectIdentity { index, field },
                ) => {
                    assert_eq!((*index, *field), (expected_index, expected_field), "{name}");
                }
                (
                    ExpectedError::DuplicateId(expected_id),
                    ConfigError::DuplicateQueryableProjectId { id },
                ) => assert_eq!(id.as_str(), expected_id, "{name}"),
                (
                    ExpectedError::DuplicateName(expected_name),
                    ConfigError::DuplicateQueryableProjectName { name: actual_name },
                ) => assert_eq!(actual_name, expected_name, "{name}"),
                (
                    ExpectedError::DuplicatePath(expected_first, expected_second),
                    ConfigError::DuplicateQueryableProjectPath {
                        first_id,
                        second_id,
                    },
                ) => assert_eq!(
                    (first_id.as_str(), second_id.as_str()),
                    (expected_first, expected_second),
                    "{name}"
                ),
                (
                    ExpectedError::Path(expected_index, expected_reason),
                    ConfigError::InvalidQueryableProjectPath { index, reason },
                ) => assert_eq!(
                    (*index, *reason),
                    (expected_index, expected_reason),
                    "{name}"
                ),
                _ => panic!("{name}: unexpected error {error:?}"),
            }
            let display = error.to_string();
            let debug = format!("{error:?}");
            assert!(
                !display.contains(privacy_sentinel.to_str().unwrap()),
                "{name}: display leaked the unrelated configured path: {display}"
            );
            assert!(
                !debug.contains(privacy_sentinel.to_str().unwrap()),
                "{name}: debug leaked the unrelated configured path: {debug}"
            );
            fs::remove_file(tmp.path().join(".tower/config.toml")).unwrap();
        }

        let permission_denied_path = tmp.path().join("permission-denied-private-root");
        write_config(
            tmp.path(),
            &format!(
                "[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \"{}\"\n\
                 [[projects.queryable]]\nid = \"privacy-sentinel\"\nname = \"Privacy sentinel\"\npath = \"{}\"\n",
                permission_denied_path.display(),
                privacy_sentinel.display(),
            ),
        );
        let error = load_with_path_canonicalizer(tmp.path(), |path| {
            if path == permission_denied_path {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                fs::canonicalize(path)
            }
        })
        .expect_err("permission denial must be mapped through the real configuration loader");
        assert!(matches!(
            error,
            ConfigError::InvalidQueryableProjectPath {
                index: 0,
                reason: QueryableProjectPathReason::Invalid,
            }
        ));
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(
            !display.contains(privacy_sentinel.to_str().unwrap()),
            "permission denial display leaked the unrelated configured path: {display}"
        );
        assert!(
            !debug.contains(privacy_sentinel.to_str().unwrap()),
            "permission denial debug leaked the unrelated configured path: {debug}"
        );
        fs::remove_file(tmp.path().join(".tower/config.toml")).unwrap();

        #[cfg(unix)]
        {
            let alias = tmp.path().join("alias");
            std::os::unix::fs::symlink(&existing, &alias).unwrap();
            write_config(
                tmp.path(),
                &format!(
                    "[[projects.queryable]]\nid = \"one\"\nname = \"One\"\npath = \"{}\"\n[[projects.queryable]]\nid = \"two\"\nname = \"Two\"\npath = \"{}\"\n",
                    existing.display(),
                    alias.display()
                ),
            );
            assert!(matches!(
                load(tmp.path()),
                Err(ConfigError::DuplicateQueryableProjectPath { first_id, second_id })
                    if first_id.as_str() == "one" && second_id.as_str() == "two"
            ));
            fs::remove_file(tmp.path().join(".tower/config.toml")).unwrap();

            let loop_path = tmp.path().join("loop");
            std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
            write_config(
                tmp.path(),
                &format!(
                    "[[projects.queryable]]\nid = \"loop\"\nname = \"Loop\"\npath = \"{}\"\n\
                     [[projects.queryable]]\nid = \"privacy-sentinel\"\nname = \"Privacy sentinel\"\npath = \"{}\"\n",
                    loop_path.display(),
                    privacy_sentinel.display(),
                ),
            );
            let error = load(tmp.path()).expect_err("symlink loop must be invalid");
            assert!(matches!(
                error,
                ConfigError::InvalidQueryableProjectPath {
                    index: 0,
                    reason: QueryableProjectPathReason::Invalid,
                }
            ));
            let display = error.to_string();
            let debug = format!("{error:?}");
            assert!(
                !display.contains(privacy_sentinel.to_str().unwrap()),
                "symlink loop display leaked the unrelated configured path: {display}"
            );
            assert!(
                !debug.contains(privacy_sentinel.to_str().unwrap()),
                "symlink loop debug leaked the unrelated configured path: {debug}"
            );
        }
    }

    #[test]
    fn configured_queryable_projects_preserve_temporarily_unavailable_root() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("not-yet-mounted");
        write_config(
            tmp.path(),
            "[[projects.queryable]]\nid = \"offline\"\nname = \"Offline\"\npath = \"not-yet-mounted\"\n",
        );

        let config = load(tmp.path()).expect("an unavailable root remains configured");
        let project = config.projects.queryable().iter().next().unwrap();
        assert_eq!(project.id.as_str(), "offline");
        assert_eq!(project.name, "Offline");
        assert_eq!(project.configured_path, Path::new("not-yet-mounted"));
        assert_eq!(project.resolved_path, missing);
        assert_eq!(project.canonical_path, None);
        assert!(!missing.exists());
        assert_eq!(
            config
                .projects
                .queryable()
                .get_by_configured_path(Path::new("not-yet-mounted")),
            Some(project)
        );
        assert_eq!(
            config.projects.queryable().get_by_canonical_path(&missing),
            None
        );

        #[cfg(unix)]
        {
            fs::remove_file(tmp.path().join(".tower/config.toml")).unwrap();
            let dangling = tmp.path().join("dangling");
            std::os::unix::fs::symlink(tmp.path().join("absent-target"), &dangling).unwrap();
            write_config(
                tmp.path(),
                "[[projects.queryable]]\nid = \"dangling\"\nname = \"Dangling\"\npath = \"dangling\"\n",
            );
            let config = load(tmp.path()).expect("a dangling symlink remains configured");
            let project = config.projects.queryable().iter().next().unwrap();
            assert_eq!(project.resolved_path, dangling);
            assert_eq!(project.canonical_path, None);
        }
    }

    #[test]
    fn queryable_project_availability_timeout_defaults_to_ten_seconds_and_rejects_non_positive_values()
     {
        let absent = tempdir().unwrap();
        let config = load(absent.path()).unwrap();
        assert_eq!(config.projects.queryable().iter().len(), 0);
        assert_eq!(
            config.projects.external_availability_timeout(),
            std::time::Duration::from_secs(10)
        );

        let positive = tempdir().unwrap();
        write_config(
            positive.path(),
            "[projects]\nexternal_availability_timeout_secs = 37\n[daemon]\nconnection_timeout_secs = 3\n",
        );
        let config = load(positive.path()).unwrap();
        assert_eq!(
            config.projects.external_availability_timeout(),
            std::time::Duration::from_secs(37)
        );
        assert_eq!(
            config.daemon.connection_timeout(),
            Some(std::time::Duration::from_secs(3))
        );
        assert_eq!(config.daemon.preparation_timeout(), None);
        assert_eq!(config.extensions.request_timeout(), None);

        for value in [0, -1] {
            let tmp = tempdir().unwrap();
            write_config(
                tmp.path(),
                &format!("[projects]\nexternal_availability_timeout_secs = {value}\n"),
            );
            assert!(matches!(
                load(tmp.path()),
                Err(ConfigError::InvalidExternalAvailabilityTimeout { value: actual })
                    if actual == value
            ));
        }

        let non_integer = tempdir().unwrap();
        write_config(
            non_integer.path(),
            "[projects]\nexternal_availability_timeout_secs = \"ten\"\n",
        );
        assert!(matches!(
            load(non_integer.path()),
            Err(ConfigError::Parse { .. })
        ));
    }
}
