//! Local project configuration: `<workspace>/.tower/config.toml`.
//!
//! Infrastructure concern (reads `std::fs`, parses `toml`). Lives in `adapters/`;
//! the domain never imports it. First feature: disabling extensions by manifest name.
//! Second feature (spec 13a): `[formatter.tools.<id>]` for external formatters.
//! Third feature (LSP plan 1, Task 6): `[lsp]` table parsed via `parse_lsp_config`.
//!
//! Absent file → `TowerConfig::default()` (silent). Present but unreadable or
//! invalid (bad TOML / unknown key) → `Err` → the binary fails fast (exit 1).

#[cfg(test)]
#[path = "tests.rs"]
mod agent_memory_tests;
pub mod debug;
pub mod lint;
pub mod lsp;
pub mod shell;
pub use debug::{DebugConfig, DebugLanguageConfig};
pub use lint::{LintCommandConfig, LintConfig, ParserFormat, TargetMode};
pub use lsp::{LspConfig, LspServerConfig, parse_lsp_config};
pub use shell::{AllowedCommand, ShellConfig, ShellConfigurationReason};

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::adapters::path_identity::lexical_normalize;
use crate::domain::search::{DependencyIndexLimits, DependencyRootId};

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum ReadinessFamily {
    #[serde(rename = "ast")]
    Ast,
    #[serde(rename = "lsp")]
    Lsp,
    #[serde(rename = "semantic_context")]
    SemanticContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticContextConfigurationReason {
    Empty,
    Zero,
    OutOfRange,
    OverlapNotLessThanTarget,
    UrlCredentialsForbidden,
    RemoteOptInRequired,
    RemoteHttpsRequired,
    UnsupportedProvider,
    UnsupportedStoreBackend,
    PathEscape,
    SymlinkEscape,
    InvalidEndpoint,
    InvalidEnvironmentVariable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMemoryConfigurationReason {
    Zero,
    OutOfRange,
    InconsistentBounds,
    InvalidPrivacyRule,
    InvalidProviderTimeout,
    IncompatibleStartupPolicy,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMemoryConfig {
    #[serde(default)]
    pub storage: AgentMemoryStorageConfig,
    #[serde(default)]
    pub limits: AgentMemoryLimitsConfig,
    #[serde(default)]
    pub privacy: AgentMemoryPrivacyConfig,
    #[serde(default)]
    pub provider: Option<AgentMemoryProviderConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMemoryStorageConfig {
    #[serde(default)]
    pub startup_policy: AgentMemoryStartupPolicy,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMemoryStartupPolicy {
    #[default]
    Fail,
    ReadOnly,
    InMemory,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMemoryLimitsConfig {
    #[serde(default = "agent_memory_default_max_title_bytes")]
    pub max_title_bytes: u32,
    #[serde(default = "agent_memory_default_max_body_bytes")]
    pub max_body_bytes: u32,
    #[serde(default = "agent_memory_default_max_metadata_items")]
    pub max_metadata_items: u32,
    #[serde(default = "agent_memory_default_max_metadata_string_bytes")]
    pub max_metadata_string_bytes: u32,
    #[serde(default = "agent_memory_default_max_batch_items")]
    pub max_batch_items: u32,
    #[serde(default = "agent_memory_default_results")]
    pub default_results: u32,
    #[serde(default = "agent_memory_default_max_results")]
    pub max_results: u32,
    #[serde(default = "agent_memory_default_response_bytes")]
    pub default_response_bytes: u32,
    #[serde(default = "agent_memory_default_max_response_bytes")]
    pub max_response_bytes: u32,
    #[serde(default = "agent_memory_default_max_query_bytes")]
    pub max_query_bytes: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMemoryPrivacyConfig {
    #[serde(default)]
    pub rules: Vec<AgentMemoryPrivacyRuleConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMemoryPrivacyRuleConfig {
    pub id: String,
    pub pattern: String,
    pub action: AgentMemoryPrivacyAction,
    #[serde(default)]
    pub replacement: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMemoryPrivacyAction {
    Reject,
    Redact,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMemoryProviderConfig {
    pub name: String,
    #[serde(default = "agent_memory_default_provider_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMemoryPersistenceMode {
    Durable,
    ReadOnly,
    InMemory,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMemoryInitConfig {
    pub project_key: String,
    pub persistence_mode: AgentMemoryPersistenceMode,
    pub durable_data_unavailable: bool,
    pub limits: AgentMemoryLimitsConfig,
    pub privacy: AgentMemoryPrivacyConfig,
    pub provider: Option<AgentMemoryProviderConfig>,
}

impl Default for AgentMemoryLimitsConfig {
    fn default() -> Self {
        Self {
            max_title_bytes: agent_memory_default_max_title_bytes(),
            max_body_bytes: agent_memory_default_max_body_bytes(),
            max_metadata_items: agent_memory_default_max_metadata_items(),
            max_metadata_string_bytes: agent_memory_default_max_metadata_string_bytes(),
            max_batch_items: agent_memory_default_max_batch_items(),
            default_results: agent_memory_default_results(),
            max_results: agent_memory_default_max_results(),
            default_response_bytes: agent_memory_default_response_bytes(),
            max_response_bytes: agent_memory_default_max_response_bytes(),
            max_query_bytes: agent_memory_default_max_query_bytes(),
        }
    }
}

const fn agent_memory_default_max_title_bytes() -> u32 {
    1_024
}
const fn agent_memory_default_max_body_bytes() -> u32 {
    65_536
}
const fn agent_memory_default_max_metadata_items() -> u32 {
    100
}
const fn agent_memory_default_max_metadata_string_bytes() -> u32 {
    1_024
}
const fn agent_memory_default_max_batch_items() -> u32 {
    100
}
const fn agent_memory_default_results() -> u32 {
    20
}
const fn agent_memory_default_max_results() -> u32 {
    100
}
const fn agent_memory_default_response_bytes() -> u32 {
    32_768
}
const fn agent_memory_default_max_response_bytes() -> u32 {
    131_072
}
const fn agent_memory_default_max_query_bytes() -> u32 {
    4_096
}
const fn agent_memory_default_provider_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticContextConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub embedding: SemanticEmbeddingConfig,
    #[serde(default)]
    pub timeouts: SemanticTimeoutConfig,
    #[serde(default)]
    pub chunking: SemanticChunkingConfig,
    #[serde(default)]
    pub store: SemanticStoreConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticEmbeddingConfig {
    #[serde(default = "semantic_default_provider")]
    pub provider: String,
    #[serde(default = "semantic_default_base_url")]
    pub base_url: String,
    #[serde(default = "semantic_default_model")]
    pub model: String,
    #[serde(default = "semantic_default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "semantic_default_batch_size")]
    pub batch_size: u64,
    #[serde(default)]
    pub dimensions: Option<u64>,
    #[serde(default)]
    pub allow_remote: bool,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticTimeoutConfig {
    #[serde(default = "semantic_default_timeout_ms")]
    pub search_ms: u64,
    #[serde(default)]
    pub reindex_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticChunkingConfig {
    #[serde(default = "semantic_default_target_bytes")]
    pub target_bytes: u64,
    #[serde(default = "semantic_default_overlap_bytes")]
    pub overlap_bytes: u64,
    #[serde(default = "semantic_default_max_file_bytes")]
    pub max_file_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticStoreConfig {
    #[serde(default = "semantic_default_store_backend")]
    pub backend: String,
}

impl Default for SemanticEmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: semantic_default_provider(),
            base_url: semantic_default_base_url(),
            model: semantic_default_model(),
            timeout_ms: semantic_default_timeout_ms(),
            batch_size: semantic_default_batch_size(),
            dimensions: None,
            allow_remote: false,
            api_key_env: None,
        }
    }
}

impl Default for SemanticTimeoutConfig {
    fn default() -> Self {
        Self {
            search_ms: semantic_default_timeout_ms(),
            reindex_ms: None,
        }
    }
}

impl Default for SemanticChunkingConfig {
    fn default() -> Self {
        Self {
            target_bytes: semantic_default_target_bytes(),
            overlap_bytes: semantic_default_overlap_bytes(),
            max_file_bytes: semantic_default_max_file_bytes(),
        }
    }
}

impl Default for SemanticStoreConfig {
    fn default() -> Self {
        Self {
            backend: semantic_default_store_backend(),
        }
    }
}

fn semantic_default_provider() -> String {
    "openai_compatible".to_owned()
}
fn semantic_default_base_url() -> String {
    "http://127.0.0.1:11434/v1".to_owned()
}
fn semantic_default_model() -> String {
    "nomic-embed-text".to_owned()
}
const fn semantic_default_timeout_ms() -> u64 {
    30_000
}
const fn semantic_default_batch_size() -> u64 {
    32
}
const fn semantic_default_target_bytes() -> u64 {
    4_096
}
const fn semantic_default_overlap_bytes() -> u64 {
    512
}
const fn semantic_default_max_file_bytes() -> u64 {
    2_097_152
}
fn semantic_default_store_backend() -> String {
    "sqlite".to_owned()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyRootPathReason {
    Empty,
    Unsafe,
    Prohibited,
    NotDirectory,
    InvalidId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDependencyRoot {
    pub id: DependencyRootId,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct DependenciesConfig {
    pub roots: Vec<ResolvedDependencyRoot>,
    pub ignore: Vec<String>,
    pub limits: DependencyIndexLimits,
}

impl Default for DependenciesConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            ignore: Vec::new(),
            limits: default_dependency_limits(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDependenciesConfig {
    #[serde(default)]
    pub roots: Vec<RawDependencyRoot>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default = "default_dependency_max_files")]
    pub max_files: u64,
    #[serde(default = "default_dependency_max_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default = "default_dependency_max_total_bytes")]
    pub max_total_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDependencyRoot {
    pub id: String,
    pub path: String,
}

const fn default_dependency_max_files() -> u64 {
    10_000
}

const fn default_dependency_max_file_bytes() -> u64 {
    1_048_576
}

const fn default_dependency_max_total_bytes() -> u64 {
    67_108_864
}

fn default_dependency_limits() -> DependencyIndexLimits {
    DependencyIndexLimits::new(
        default_dependency_max_files() as usize,
        default_dependency_max_file_bytes(),
        default_dependency_max_total_bytes(),
    )
    .expect("dependency defaults are positive")
}

/// Parsed `.tower/config.toml`. Unknown top-level keys are rejected.
#[derive(Debug, Default)]
pub struct TowerConfig {
    pub formatter: FormatterConfig,
    pub shell: ShellConfig,
    /// `[lsp]` section — language-server bindings (LSP plan 1, Task 8).
    ///
    /// Absent → `LspConfig::default()` (empty; no servers). Populated by
    /// wiring in `main.rs`; production code reads `config.lsp` directly
    /// rather than calling `parse_lsp_config` again.
    pub lsp: LspConfig,
    pub lint: LintConfig,
    pub debug: DebugConfig,
    pub semantic_context: SemanticContextConfig,
    pub agent_memory: AgentMemoryConfig,
    /// `[extensions]` section — sidecar extension runtime settings (spec 24).
    ///
    /// Absent → `ExtensionConfig::default()` (no request timeout).
    pub extensions: ExtensionConfig,
    /// `[daemon]` section — shared-daemon runtime settings.
    /// Absent → `DaemonConfig::default()` (30-second idle timeout).
    pub daemon: DaemonConfig,
    pub projects: ProjectsConfig,
    pub dependencies: DependenciesConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTowerConfig {
    #[serde(default)]
    formatter: FormatterConfig,
    #[serde(default)]
    shell: ShellConfig,
    #[serde(default)]
    lsp: LspConfig,
    #[serde(default)]
    lint: LintConfig,
    #[serde(default)]
    debug: DebugConfig,
    #[serde(default)]
    semantic_context: SemanticContextConfig,
    #[serde(default)]
    agent_memory: AgentMemoryConfig,
    #[serde(default)]
    extensions: ExtensionConfig,
    #[serde(default)]
    daemon: DaemonConfig,
    #[serde(default)]
    projects: Option<toml::Value>,
    #[serde(default)]
    dependencies: RawDependenciesConfig,
}

impl<'de> Deserialize<'de> for TowerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = toml::Value::deserialize(deserializer)?;
        validate_timeout_values(&value).map_err(serde::de::Error::custom)?;
        let raw: RawTowerConfig = value.try_into().map_err(serde::de::Error::custom)?;
        let _ = (raw.projects, raw.dependencies);
        Ok(Self {
            formatter: raw.formatter,
            shell: raw.shell,
            lsp: raw.lsp,
            lint: raw.lint,
            debug: raw.debug,
            semantic_context: raw.semantic_context,
            agent_memory: raw.agent_memory,
            extensions: raw.extensions,
            daemon: raw.daemon,
            projects: ProjectsConfig::default(),
            dependencies: DependenciesConfig::default(),
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
    InvalidShellConfiguration {
        field: &'static str,
        reason: ShellConfigurationReason,
    },
    InvalidSemanticContextConfiguration {
        field: &'static str,
        reason: SemanticContextConfigurationReason,
    },
    InvalidAgentMemoryConfiguration {
        field: &'static str,
        reason: AgentMemoryConfigurationReason,
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
    InvalidDependencyLimit {
        field: &'static str,
        value: u64,
    },
    InvalidDependencyRoot {
        index: usize,
        reason: DependencyRootPathReason,
    },
    DuplicateDependencyRootId {
        id: DependencyRootId,
    },
    InvalidDependencyIgnore {
        index: usize,
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
            ConfigError::InvalidShellConfiguration { field, reason } => {
                write!(f, "invalid shell configuration {field}: {reason:?}")
            }
            ConfigError::InvalidSemanticContextConfiguration { field, reason } => {
                write!(
                    f,
                    "invalid semantic context configuration {field}: {reason:?}"
                )
            }
            ConfigError::InvalidAgentMemoryConfiguration { field, reason } => {
                write!(f, "invalid agent memory configuration {field}: {reason:?}")
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
            ConfigError::InvalidDependencyLimit { field, value } => {
                write!(f, "invalid dependency limit {field}: {value}")
            }
            ConfigError::InvalidDependencyRoot { index, reason } => {
                write!(f, "invalid dependency root at index {index}: {reason:?}")
            }
            ConfigError::DuplicateDependencyRootId { .. } => {
                f.write_str("duplicate dependency root id")
            }
            ConfigError::InvalidDependencyIgnore { index } => {
                write!(f, "invalid dependency ignore rule at index {index}")
            }
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

    // TOML has no null value, but this field has a stricter public error
    // contract than a generic syntax failure. Classify the assignment before
    // parsing while accepting TOML-equivalent quoted keys and comments.
    if agent_memory_provider_timeout_is_null(&contents) {
        return Err(invalid_agent_memory(
            "agent_memory.provider.timeout_ms",
            AgentMemoryConfigurationReason::InvalidProviderTimeout,
        ));
    }

    let raw: toml::Value = toml::from_str(&contents).map_err(|source| {
        shell_parse_error(&contents, &source).unwrap_or_else(|| ConfigError::Parse {
            path: path.clone(),
            source: timeout_parse_error(&contents, &source).unwrap_or(source),
        })
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
    config.projects = parse_projects_config(&raw, workspace_root, &canonicalize_path).map_err(
        |error| match error {
            ProjectConfigLoadError::Parse(source) => ConfigError::Parse {
                path: path.clone(),
                source,
            },
            ProjectConfigLoadError::Config(error) => error,
        },
    )?;
    config.dependencies = parse_dependencies_config(&raw, workspace_root, &canonicalize_path)
        .map_err(|error| match error {
            DependencyConfigLoadError::Parse(source) => ConfigError::Parse {
                path: path.clone(),
                source,
            },
            DependencyConfigLoadError::Config(error) => error,
        })?;
    validate_daemon_deadlines(&config.daemon)?;
    validate_extension_configuration(&config.extensions)?;
    validate_agent_memory_configuration(&config.agent_memory)?;
    validate_semantic_context_configuration(
        &config.semantic_context,
        workspace_root,
        &canonicalize_path,
    )?;
    config.shell.validate()?;
    config.lint_request_timeout()?;
    if config.lsp_budget_required() {
        config.extensions.lsp_diagnostics_timeout()?;
    }
    Ok(config)
}

fn agent_memory_provider_timeout_is_null(contents: &str) -> bool {
    let mut in_provider = false;
    for line in contents.lines() {
        let statement = line.split('#').next().unwrap_or_default().trim();
        if statement.starts_with('[') {
            in_provider = statement == "[agent_memory.provider]";
            continue;
        }
        if !in_provider {
            continue;
        }
        let Some((key, value)) = statement.split_once('=') else {
            continue;
        };
        let key = key
            .trim()
            .trim_matches(|character| character == '"' || character == '\'');
        if key == "timeout_ms" && value.trim() == "null" {
            return true;
        }
    }
    false
}

fn invalid_agent_memory(
    field: &'static str,
    reason: AgentMemoryConfigurationReason,
) -> ConfigError {
    ConfigError::InvalidAgentMemoryConfiguration { field, reason }
}

fn validate_agent_memory_configuration(config: &AgentMemoryConfig) -> Result<(), ConfigError> {
    const MAX_LIMIT: u32 = 16_777_216;
    let limits = &config.limits;
    for (field, value) in [
        (
            "agent_memory.limits.max_title_bytes",
            limits.max_title_bytes,
        ),
        ("agent_memory.limits.max_body_bytes", limits.max_body_bytes),
        (
            "agent_memory.limits.max_metadata_items",
            limits.max_metadata_items,
        ),
        (
            "agent_memory.limits.max_metadata_string_bytes",
            limits.max_metadata_string_bytes,
        ),
        (
            "agent_memory.limits.max_batch_items",
            limits.max_batch_items,
        ),
        (
            "agent_memory.limits.default_results",
            limits.default_results,
        ),
        ("agent_memory.limits.max_results", limits.max_results),
        (
            "agent_memory.limits.default_response_bytes",
            limits.default_response_bytes,
        ),
        (
            "agent_memory.limits.max_response_bytes",
            limits.max_response_bytes,
        ),
        (
            "agent_memory.limits.max_query_bytes",
            limits.max_query_bytes,
        ),
    ] {
        if value == 0 {
            return Err(invalid_agent_memory(
                field,
                AgentMemoryConfigurationReason::Zero,
            ));
        }
        if value > MAX_LIMIT {
            return Err(invalid_agent_memory(
                field,
                AgentMemoryConfigurationReason::OutOfRange,
            ));
        }
    }
    if limits.default_results > limits.max_results {
        return Err(invalid_agent_memory(
            "agent_memory.limits.default_results",
            AgentMemoryConfigurationReason::InconsistentBounds,
        ));
    }
    if limits.max_results > limits.max_batch_items {
        return Err(invalid_agent_memory(
            "agent_memory.limits.max_results",
            AgentMemoryConfigurationReason::InconsistentBounds,
        ));
    }
    if limits.default_response_bytes > limits.max_response_bytes {
        return Err(invalid_agent_memory(
            "agent_memory.limits.default_response_bytes",
            AgentMemoryConfigurationReason::InconsistentBounds,
        ));
    }

    let mut rule_ids = std::collections::HashSet::new();
    for (index, rule) in config.privacy.rules.iter().enumerate() {
        let id_field = match index {
            0 => "agent_memory.privacy.rules[0].id",
            1 => "agent_memory.privacy.rules[1].id",
            _ => "agent_memory.privacy.rules[].id",
        };
        if rule.id.is_empty() || rule.id.len() > 128 || !rule_ids.insert(rule.id.as_str()) {
            return Err(invalid_agent_memory(
                id_field,
                AgentMemoryConfigurationReason::InvalidPrivacyRule,
            ));
        }
        if regex::Regex::new(&rule.pattern).is_err() {
            return Err(invalid_agent_memory(
                match index {
                    0 => "agent_memory.privacy.rules[0].pattern",
                    _ => "agent_memory.privacy.rules[].pattern",
                },
                AgentMemoryConfigurationReason::InvalidPrivacyRule,
            ));
        }
        let replacement_valid = match rule.action {
            AgentMemoryPrivacyAction::Reject => rule.replacement.is_none(),
            AgentMemoryPrivacyAction::Redact => rule.replacement.is_some(),
        };
        if !replacement_valid {
            return Err(invalid_agent_memory(
                match index {
                    0 => "agent_memory.privacy.rules[0].replacement",
                    _ => "agent_memory.privacy.rules[].replacement",
                },
                AgentMemoryConfigurationReason::InvalidPrivacyRule,
            ));
        }
    }
    if let Some(provider) = &config.provider {
        if provider.name.is_empty() || provider.name.len() > 128 {
            return Err(invalid_agent_memory(
                "agent_memory.provider.name",
                AgentMemoryConfigurationReason::InvalidProviderTimeout,
            ));
        }
        if !(1..=86_400_000).contains(&provider.timeout_ms) {
            return Err(invalid_agent_memory(
                "agent_memory.provider.timeout_ms",
                AgentMemoryConfigurationReason::InvalidProviderTimeout,
            ));
        }
    }
    Ok(())
}

fn invalid_semantic(
    field: &'static str,
    reason: SemanticContextConfigurationReason,
) -> ConfigError {
    ConfigError::InvalidSemanticContextConfiguration { field, reason }
}

fn validate_semantic_context_configuration(
    config: &SemanticContextConfig,
    workspace_root: &Path,
    canonicalize_path: &impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<(), ConfigError> {
    use SemanticContextConfigurationReason::{
        Empty, InvalidEnvironmentVariable, OutOfRange, OverlapNotLessThanTarget,
        RemoteHttpsRequired, RemoteOptInRequired, UnsupportedProvider, UnsupportedStoreBackend,
        UrlCredentialsForbidden,
    };

    let embedding = &config.embedding;
    if embedding.provider != "openai_compatible" {
        return Err(invalid_semantic(
            "semantic_context.embedding.provider",
            UnsupportedProvider,
        ));
    }
    if embedding.model.is_empty() {
        return Err(invalid_semantic("semantic_context.embedding.model", Empty));
    }
    validate_semantic_number(
        "semantic_context.embedding.timeout_ms",
        embedding.timeout_ms,
        3_600_000,
    )?;
    validate_semantic_number(
        "semantic_context.embedding.batch_size",
        embedding.batch_size,
        256,
    )?;
    if let Some(dimensions) = embedding.dimensions {
        validate_semantic_number("semantic_context.embedding.dimensions", dimensions, 65_536)?;
    }
    if let Some(name) = &embedding.api_key_env
        && !is_environment_variable_name(name)
    {
        return Err(invalid_semantic(
            "semantic_context.embedding.api_key_env",
            InvalidEnvironmentVariable,
        ));
    }

    let endpoint = parse_semantic_endpoint(&embedding.base_url)
        .map_err(|reason| invalid_semantic("semantic_context.embedding.base_url", reason))?;
    if endpoint.has_credentials {
        return Err(invalid_semantic(
            "semantic_context.embedding.base_url",
            UrlCredentialsForbidden,
        ));
    }
    if !endpoint.local && !embedding.allow_remote {
        return Err(invalid_semantic(
            "semantic_context.embedding.base_url",
            RemoteOptInRequired,
        ));
    }
    if !endpoint.local && endpoint.scheme != "https" {
        return Err(invalid_semantic(
            "semantic_context.embedding.base_url",
            RemoteHttpsRequired,
        ));
    }

    validate_semantic_number(
        "semantic_context.timeouts.search_ms",
        config.timeouts.search_ms,
        3_600_000,
    )?;
    if let Some(reindex_ms) = config.timeouts.reindex_ms {
        validate_semantic_number(
            "semantic_context.timeouts.reindex_ms",
            reindex_ms,
            86_400_000,
        )?;
    }
    if !(4..=1_048_576).contains(&config.chunking.target_bytes) {
        return Err(invalid_semantic(
            "semantic_context.chunking.target_bytes",
            OutOfRange,
        ));
    }
    if config.chunking.max_file_bytes < config.chunking.target_bytes
        || config.chunking.max_file_bytes > 67_108_864
    {
        return Err(invalid_semantic(
            "semantic_context.chunking.max_file_bytes",
            OutOfRange,
        ));
    }
    if config.chunking.overlap_bytes >= config.chunking.target_bytes {
        return Err(invalid_semantic(
            "semantic_context.chunking.overlap_bytes",
            OverlapNotLessThanTarget,
        ));
    }
    if config.store.backend != "sqlite" {
        return Err(invalid_semantic(
            "semantic_context.store.backend",
            UnsupportedStoreBackend,
        ));
    }
    if config.enabled {
        validate_semantic_store_path(workspace_root, canonicalize_path)?;
    }
    Ok(())
}

fn validate_semantic_number(
    field: &'static str,
    value: u64,
    maximum: u64,
) -> Result<(), ConfigError> {
    if value == 0 {
        return Err(invalid_semantic(
            field,
            SemanticContextConfigurationReason::Zero,
        ));
    }
    if value > maximum {
        return Err(invalid_semantic(
            field,
            SemanticContextConfigurationReason::OutOfRange,
        ));
    }
    Ok(())
}

struct SemanticEndpoint {
    scheme: String,
    local: bool,
    has_credentials: bool,
}

fn parse_semantic_endpoint(
    value: &str,
) -> Result<SemanticEndpoint, SemanticContextConfigurationReason> {
    use SemanticContextConfigurationReason::{Empty, InvalidEndpoint};
    if value.is_empty() {
        return Err(Empty);
    }
    let endpoint = url::Url::parse(value).map_err(|_| InvalidEndpoint)?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !has_valid_percent_encoding(endpoint.path())
    {
        return Err(InvalidEndpoint);
    }
    let host = endpoint.host_str().ok_or(InvalidEndpoint)?;
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    Ok(SemanticEndpoint {
        scheme: endpoint.scheme().to_owned(),
        local,
        has_credentials: !endpoint.username().is_empty() || endpoint.password().is_some(),
    })
}

fn has_valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn is_environment_variable_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn validate_semantic_store_path(
    workspace_root: &Path,
    canonicalize_path: &impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<(), ConfigError> {
    let field = "semantic_context.store.path";
    let workspace = canonicalize_path(workspace_root)
        .map_err(|_| invalid_semantic(field, SemanticContextConfigurationReason::PathEscape))?;
    let derived_directory = workspace_root.join(".tower/semantic_context");
    let existing_ancestor = derived_directory
        .ancestors()
        .find(|candidate| candidate.exists())
        .expect("semantic store path always has an existing workspace ancestor");
    let resolved_ancestor = canonicalize_path(existing_ancestor)
        .map_err(|_| invalid_semantic(field, SemanticContextConfigurationReason::PathEscape))?;
    if !resolved_ancestor.starts_with(&workspace) {
        return Err(invalid_semantic(
            field,
            SemanticContextConfigurationReason::SymlinkEscape,
        ));
    }
    fs::create_dir_all(&derived_directory)
        .map_err(|_| invalid_semantic(field, SemanticContextConfigurationReason::PathEscape))?;
    let resolved_directory = canonicalize_path(&derived_directory)
        .map_err(|_| invalid_semantic(field, SemanticContextConfigurationReason::PathEscape))?;
    if !resolved_directory.starts_with(&workspace) {
        return Err(invalid_semantic(
            field,
            SemanticContextConfigurationReason::SymlinkEscape,
        ));
    }

    let database = derived_directory.join("index.sqlite3");
    if fs::symlink_metadata(&database).is_ok() {
        let resolved_database = canonicalize_path(&database).map_err(|_| {
            invalid_semantic(field, SemanticContextConfigurationReason::SymlinkEscape)
        })?;
        if !resolved_database.starts_with(&workspace) {
            return Err(invalid_semantic(
                field,
                SemanticContextConfigurationReason::SymlinkEscape,
            ));
        }
    }
    Ok(())
}

enum DependencyConfigLoadError {
    Parse(toml::de::Error),
    Config(ConfigError),
}

fn parse_dependencies_config(
    raw: &toml::Value,
    workspace_root: &Path,
    canonicalize_path: &impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<DependenciesConfig, DependencyConfigLoadError> {
    let raw_dependencies = raw
        .get("dependencies")
        .cloned()
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    let raw_dependencies: RawDependenciesConfig = raw_dependencies
        .try_into()
        .map_err(DependencyConfigLoadError::Parse)?;

    let limits = parse_dependency_limits(&raw_dependencies)?;
    validate_dependency_ignores(&raw_dependencies.ignore, workspace_root)?;
    let roots =
        resolve_dependency_roots(raw_dependencies.roots, workspace_root, canonicalize_path)?;

    Ok(DependenciesConfig {
        roots,
        ignore: raw_dependencies.ignore,
        limits,
    })
}

fn parse_dependency_limits(
    raw: &RawDependenciesConfig,
) -> Result<DependencyIndexLimits, DependencyConfigLoadError> {
    let max_files = dependency_usize_limit("max_files", raw.max_files)
        .map_err(DependencyConfigLoadError::Config)?;
    validate_dependency_limit("max_file_bytes", raw.max_file_bytes)
        .map_err(DependencyConfigLoadError::Config)?;
    validate_dependency_limit("max_total_bytes", raw.max_total_bytes)
        .map_err(DependencyConfigLoadError::Config)?;
    Ok(
        DependencyIndexLimits::new(max_files, raw.max_file_bytes, raw.max_total_bytes)
            .expect("dependency limits were checked as positive"),
    )
}

fn validate_dependency_ignores(
    rules: &[String],
    workspace_root: &Path,
) -> Result<(), DependencyConfigLoadError> {
    for (index, rule) in rules.iter().enumerate() {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(workspace_root);
        if !dependency_ignore_syntax_is_valid(rule)
            || builder.add_line(None, rule).is_err()
            || builder.build().is_err()
        {
            return Err(DependencyConfigLoadError::Config(
                ConfigError::InvalidDependencyIgnore { index },
            ));
        }
    }
    Ok(())
}

fn resolve_dependency_roots(
    raw_roots: Vec<RawDependencyRoot>,
    workspace_root: &Path,
    canonicalize_path: &impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<Vec<ResolvedDependencyRoot>, DependencyConfigLoadError> {
    if raw_roots.is_empty() {
        return Ok(Vec::new());
    }

    let absolute_workspace = absolute_lexical_path(workspace_root).map_err(|()| {
        DependencyConfigLoadError::Config(ConfigError::InvalidDependencyRoot {
            index: 0,
            reason: DependencyRootPathReason::Unsafe,
        })
    })?;
    let canonical_workspace = canonicalize_path(&absolute_workspace)
        .map(|path| lexical_normalize(&path))
        .map_err(|_| {
            DependencyConfigLoadError::Config(ConfigError::InvalidDependencyRoot {
                index: 0,
                reason: DependencyRootPathReason::Unsafe,
            })
        })?;

    let mut identified = Vec::with_capacity(raw_roots.len());
    for (index, root) in raw_roots.into_iter().enumerate() {
        let id = DependencyRootId::new(root.id).map_err(|_| {
            DependencyConfigLoadError::Config(ConfigError::InvalidDependencyRoot {
                index,
                reason: DependencyRootPathReason::InvalidId,
            })
        })?;
        if identified
            .iter()
            .any(|(existing, _, _): &(DependencyRootId, String, usize)| existing == &id)
        {
            return Err(DependencyConfigLoadError::Config(
                ConfigError::DuplicateDependencyRootId { id },
            ));
        }
        identified.push((id, root.path, index));
    }

    let mut roots = Vec::<ResolvedDependencyRoot>::with_capacity(identified.len());
    for (id, raw_path, index) in identified {
        let path = resolve_dependency_root(
            raw_path,
            &absolute_workspace,
            &canonical_workspace,
            index,
            canonicalize_path,
        )
        .map_err(DependencyConfigLoadError::Config)?;

        if roots
            .iter()
            .any(|root| root.path == path || path.starts_with(&root.path))
        {
            continue;
        }
        roots.retain(|root| !root.path.starts_with(&path));
        roots.push(ResolvedDependencyRoot { id, path });
    }
    roots.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(roots)
}

fn dependency_ignore_syntax_is_valid(rule: &str) -> bool {
    if rule.is_empty() || rule.chars().any(char::is_control) {
        return false;
    }
    let mut escaped = false;
    let mut character_class_open = false;
    for character in rule.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' if !character_class_open => character_class_open = true,
            ']' if character_class_open => character_class_open = false,
            _ => {}
        }
    }
    !escaped && !character_class_open
}

fn dependency_usize_limit(field: &'static str, value: u64) -> Result<usize, ConfigError> {
    validate_dependency_limit(field, value)?;
    usize::try_from(value).map_err(|_| ConfigError::InvalidDependencyLimit { field, value })
}

fn validate_dependency_limit(field: &'static str, value: u64) -> Result<(), ConfigError> {
    if value == 0 {
        return Err(ConfigError::InvalidDependencyLimit { field, value });
    }
    Ok(())
}

fn absolute_lexical_path(path: &Path) -> Result<PathBuf, ()> {
    if path.is_absolute() {
        return Ok(lexical_normalize(path));
    }
    std::env::current_dir()
        .map(|current| lexical_normalize(&current.join(path)))
        .map_err(|_| ())
}

fn resolve_dependency_root(
    raw_path: String,
    absolute_workspace: &Path,
    canonical_workspace: &Path,
    index: usize,
    canonicalize_path: &impl Fn(&Path) -> io::Result<PathBuf>,
) -> Result<PathBuf, ConfigError> {
    let invalid = |reason| ConfigError::InvalidDependencyRoot { index, reason };
    if raw_path.is_empty() {
        return Err(invalid(DependencyRootPathReason::Empty));
    }
    if raw_path.contains('\0') || is_ambiguous_dependency_path(&raw_path) {
        return Err(invalid(DependencyRootPathReason::Unsafe));
    }
    let configured = PathBuf::from(raw_path);
    let absolute = if configured.is_absolute() {
        configured
    } else {
        absolute_workspace.join(configured)
    };
    let lexical = lexical_normalize(&absolute);
    if !lexical.is_absolute() || dependency_path_is_prohibited(&lexical, canonical_workspace) {
        return Err(invalid(if lexical.is_absolute() {
            DependencyRootPathReason::Prohibited
        } else {
            DependencyRootPathReason::Unsafe
        }));
    }

    let resolved = match canonicalize_path(&absolute) {
        Ok(path) => {
            let path = lexical_normalize(&path);
            let metadata =
                fs::metadata(&path).map_err(|_| invalid(DependencyRootPathReason::Unsafe))?;
            if !metadata.is_dir() {
                return Err(invalid(DependencyRootPathReason::NotDirectory));
            }
            path
        }
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
            return Err(invalid(DependencyRootPathReason::NotDirectory));
        }
        Err(error) => resolve_unavailable_dependency_root(
            &lexical,
            absolute_workspace,
            error.kind(),
            canonicalize_path,
        )
        .ok_or_else(|| invalid(DependencyRootPathReason::Unsafe))?,
    };

    if dependency_path_is_prohibited(&resolved, canonical_workspace) {
        return Err(invalid(DependencyRootPathReason::Prohibited));
    }
    Ok(resolved)
}

fn is_ambiguous_dependency_path(_raw_path: &str) -> bool {
    #[cfg(windows)]
    {
        use std::path::Component;
        if matches!(Path::new(_raw_path).components().next(), Some(Component::Prefix(prefix)) if !prefix.kind().is_verbatim() && !Path::new(_raw_path).is_absolute())
        {
            return true;
        }
    }
    false
}

fn resolve_unavailable_dependency_root(
    lexical: &Path,
    workspace: &Path,
    error_kind: io::ErrorKind,
    canonicalize_path: &impl Fn(&Path) -> io::Result<PathBuf>,
) -> Option<PathBuf> {
    let mut ancestor = lexical.parent()?;
    let mut suffix = Vec::new();
    suffix.push(lexical.file_name()?.to_owned());

    loop {
        if error_kind != io::ErrorKind::NotFound && ancestor == workspace {
            return None;
        }
        if let Ok(canonical) = canonicalize_path(ancestor) {
            let metadata = fs::metadata(&canonical).ok()?;
            if !metadata.is_dir() {
                return None;
            }
            let mut resolved = lexical_normalize(&canonical);
            for component in suffix.iter().rev() {
                resolved.push(component);
            }
            return Some(resolved);
        }
        suffix.push(ancestor.file_name()?.to_owned());
        ancestor = ancestor.parent()?;
    }
}

fn dependency_path_is_prohibited(path: &Path, workspace: &Path) -> bool {
    if path.parent().is_none() || (workspace.starts_with(path) && workspace != path) {
        return true;
    }

    let workspace_metadata = [workspace.join(".tower"), workspace.join(".git")];
    if workspace_metadata
        .iter()
        .any(|metadata| path.starts_with(metadata))
    {
        return true;
    }
    if path != workspace
        && [path.join(".tower"), path.join(".git")]
            .iter()
            .any(|metadata| metadata.exists())
    {
        return true;
    }

    #[cfg(unix)]
    if ["/proc", "/sys", "/dev", "/etc", "/boot", "/run"]
        .iter()
        .map(Path::new)
        .any(|system_tree| path.starts_with(system_tree) || system_tree.starts_with(path))
    {
        return true;
    }

    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        let system_root = lexical_normalize(Path::new(&system_root));
        if path.starts_with(&system_root) || system_root.starts_with(path) {
            return true;
        }
    }

    false
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

fn shell_parse_error(contents: &str, source: &toml::de::Error) -> Option<ConfigError> {
    let offset = source.span()?.start.min(contents.len());
    let prefix = &contents[..offset];
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let line_end = contents[offset..]
        .find('\n')
        .map_or(contents.len(), |index| offset + index);
    let (key, value) = contents[line_start..line_end].trim().split_once('=')?;
    let table = prefix
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('[') && line.ends_with(']'))?;
    if table != "[shell]" || key.trim() != "timeout_secs" {
        return None;
    }
    value.trim().parse::<u64>().ok()?;
    Some(ConfigError::InvalidShellConfiguration {
        field: "shell.timeout_secs",
        reason: ShellConfigurationReason::InvalidTimeout,
    })
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

    fn assert_invalid_semantic_context(
        body: &str,
        expected_field: &'static str,
        expected_reason: SemanticContextConfigurationReason,
    ) {
        let fixture = tempdir().unwrap();
        write_config(fixture.path(), body);
        match load(fixture.path()).expect_err("invalid semantic configuration must be rejected") {
            ConfigError::InvalidSemanticContextConfiguration { field, reason } => {
                assert_eq!(field, expected_field);
                assert_eq!(reason, expected_reason);
            }
            other => panic!("expected InvalidSemanticContextConfiguration, got {other:?}"),
        }
    }

    #[test]
    fn semantic_context_defaults_are_disabled_and_provider_free() {
        let fixture = tempdir().unwrap();
        let config = load(fixture.path()).expect("absent configuration uses inert defaults");
        let semantic = config.semantic_context;
        assert!(!semantic.enabled);
        assert_eq!(semantic.embedding.provider, "openai_compatible");
        assert_eq!(semantic.embedding.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(semantic.embedding.model, "nomic-embed-text");
        assert_eq!(semantic.embedding.timeout_ms, 30_000);
        assert_eq!(semantic.embedding.batch_size, 32);
        assert_eq!(semantic.embedding.dimensions, None);
        assert!(!semantic.embedding.allow_remote);
        assert_eq!(semantic.embedding.api_key_env, None);
        assert_eq!(semantic.timeouts.search_ms, 30_000);
        assert_eq!(semantic.timeouts.reindex_ms, None);
        assert_eq!(semantic.chunking.target_bytes, 4_096);
        assert_eq!(semantic.chunking.overlap_bytes, 512);
        assert_eq!(semantic.chunking.max_file_bytes, 2_097_152);
        assert_eq!(semantic.store.backend, "sqlite");
        assert!(!fixture.path().join(".tower/semantic_context").exists());
    }

    #[test]
    fn semantic_context_enabled_defaults_match_public_contract() {
        let fixture = tempdir().unwrap();
        write_config(fixture.path(), "[semantic_context]\nenabled = true\n");
        let semantic = load(fixture.path())
            .expect("enabled defaults are valid")
            .semantic_context;
        assert!(semantic.enabled);
        assert_eq!(semantic.embedding.timeout_ms, 30_000);
        assert_eq!(semantic.timeouts.search_ms, 30_000);
        assert_eq!(semantic.timeouts.reindex_ms, None);
        assert_eq!(semantic.embedding.batch_size, 32);
        assert_eq!(semantic.embedding.dimensions, None);
        assert_eq!(semantic.chunking.target_bytes, 4_096);
        assert_eq!(semantic.chunking.overlap_bytes, 512);
        assert_eq!(semantic.chunking.max_file_bytes, 2_097_152);
    }

    #[test]
    fn semantic_context_rejects_invalid_numeric_boundaries_before_spawn() {
        let valid = [
            "[semantic_context]\nenabled=true\n[semantic_context.embedding]\ntimeout_ms=1\nbatch_size=1\ndimensions=1\n[semantic_context.timeouts]\nsearch_ms=1\nreindex_ms=1\n[semantic_context.chunking]\ntarget_bytes=4\noverlap_bytes=0\nmax_file_bytes=4\n",
            "[semantic_context]\nenabled=true\n[semantic_context.embedding]\ntimeout_ms=3600000\nbatch_size=256\ndimensions=65536\n[semantic_context.timeouts]\nsearch_ms=3600000\nreindex_ms=86400000\n[semantic_context.chunking]\ntarget_bytes=1048576\noverlap_bytes=1048575\nmax_file_bytes=67108864\n",
        ];
        for body in valid {
            let fixture = tempdir().unwrap();
            write_config(fixture.path(), body);
            load(fixture.path()).expect("documented numeric boundary must be accepted");
        }

        let invalid = [
            (
                "[semantic_context.embedding]\ntimeout_ms=0\n",
                "semantic_context.embedding.timeout_ms",
                SemanticContextConfigurationReason::Zero,
            ),
            (
                "[semantic_context.embedding]\ntimeout_ms=3600001\n",
                "semantic_context.embedding.timeout_ms",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.embedding]\nbatch_size=0\n",
                "semantic_context.embedding.batch_size",
                SemanticContextConfigurationReason::Zero,
            ),
            (
                "[semantic_context.embedding]\nbatch_size=257\n",
                "semantic_context.embedding.batch_size",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.embedding]\ndimensions=0\n",
                "semantic_context.embedding.dimensions",
                SemanticContextConfigurationReason::Zero,
            ),
            (
                "[semantic_context.embedding]\ndimensions=65537\n",
                "semantic_context.embedding.dimensions",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.timeouts]\nsearch_ms=0\n",
                "semantic_context.timeouts.search_ms",
                SemanticContextConfigurationReason::Zero,
            ),
            (
                "[semantic_context.timeouts]\nsearch_ms=3600001\n",
                "semantic_context.timeouts.search_ms",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.timeouts]\nreindex_ms=0\n",
                "semantic_context.timeouts.reindex_ms",
                SemanticContextConfigurationReason::Zero,
            ),
            (
                "[semantic_context.timeouts]\nreindex_ms=86400001\n",
                "semantic_context.timeouts.reindex_ms",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.chunking]\ntarget_bytes=3\n",
                "semantic_context.chunking.target_bytes",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.chunking]\ntarget_bytes=1048577\n",
                "semantic_context.chunking.target_bytes",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.chunking]\ntarget_bytes=4\noverlap_bytes=4\n",
                "semantic_context.chunking.overlap_bytes",
                SemanticContextConfigurationReason::OverlapNotLessThanTarget,
            ),
            (
                "[semantic_context.chunking]\ntarget_bytes=4\nmax_file_bytes=3\n",
                "semantic_context.chunking.max_file_bytes",
                SemanticContextConfigurationReason::OutOfRange,
            ),
            (
                "[semantic_context.chunking]\nmax_file_bytes=67108865\n",
                "semantic_context.chunking.max_file_bytes",
                SemanticContextConfigurationReason::OutOfRange,
            ),
        ];
        for (fragment, field, reason) in invalid {
            assert_invalid_semantic_context(
                &format!("[semantic_context]\nenabled=true\n{fragment}"),
                field,
                reason,
            );
        }
    }

    #[test]
    fn semantic_context_rejects_remote_and_credentialed_endpoints_before_spawn() {
        let invalid = [
            (
                "provider=\"other\"",
                "semantic_context.embedding.provider",
                SemanticContextConfigurationReason::UnsupportedProvider,
            ),
            (
                "model=\"\"",
                "semantic_context.embedding.model",
                SemanticContextConfigurationReason::Empty,
            ),
            (
                "base_url=\"not-a-url\"",
                "semantic_context.embedding.base_url",
                SemanticContextConfigurationReason::InvalidEndpoint,
            ),
            (
                "base_url=\"ftp://localhost/v1\"",
                "semantic_context.embedding.base_url",
                SemanticContextConfigurationReason::InvalidEndpoint,
            ),
            (
                "base_url=\"http://localhost:70000/v1\"",
                "semantic_context.embedding.base_url",
                SemanticContextConfigurationReason::InvalidEndpoint,
            ),
            (
                "base_url=\"http://localhost/%zz\"",
                "semantic_context.embedding.base_url",
                SemanticContextConfigurationReason::InvalidEndpoint,
            ),
            (
                "base_url=\"http://user@localhost/v1\"",
                "semantic_context.embedding.base_url",
                SemanticContextConfigurationReason::UrlCredentialsForbidden,
            ),
            (
                "base_url=\"https://example.com/v1\"",
                "semantic_context.embedding.base_url",
                SemanticContextConfigurationReason::RemoteOptInRequired,
            ),
            (
                "base_url=\"http://example.com/v1\"\nallow_remote=true",
                "semantic_context.embedding.base_url",
                SemanticContextConfigurationReason::RemoteHttpsRequired,
            ),
            (
                "api_key_env=\"9INVALID\"",
                "semantic_context.embedding.api_key_env",
                SemanticContextConfigurationReason::InvalidEnvironmentVariable,
            ),
        ];
        for (fragment, field, reason) in invalid {
            assert_invalid_semantic_context(
                &format!(
                    "[semantic_context]\nenabled=true\n[semantic_context.embedding]\n{fragment}\n"
                ),
                field,
                reason,
            );
        }
        assert_invalid_semantic_context(
            "[semantic_context]\nenabled=true\n[semantic_context.store]\nbackend=\"memory\"\n",
            "semantic_context.store.backend",
            SemanticContextConfigurationReason::UnsupportedStoreBackend,
        );
    }

    #[cfg(unix)]
    #[test]
    fn semantic_context_store_path_is_symlink_confined_before_spawn() {
        use std::os::unix::fs::symlink;

        let fixture = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir_all(fixture.path().join(".tower")).unwrap();
        symlink(
            outside.path(),
            fixture.path().join(".tower/semantic_context"),
        )
        .unwrap();
        write_config(fixture.path(), "[semantic_context]\nenabled=true\n");
        match load(fixture.path()).expect_err("derived store symlink escape must be rejected") {
            ConfigError::InvalidSemanticContextConfiguration { field, reason } => {
                assert_eq!(field, "semantic_context.store.path");
                assert_eq!(reason, SemanticContextConfigurationReason::SymlinkEscape);
            }
            other => panic!("expected semantic store confinement error, got {other:?}"),
        }
        assert!(!outside.path().join("index.sqlite3").exists());

        let fixture = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir_all(fixture.path().join(".tower/semantic_context")).unwrap();
        symlink(
            outside.path().join("outside.sqlite3"),
            fixture.path().join(".tower/semantic_context/index.sqlite3"),
        )
        .unwrap();
        write_config(fixture.path(), "[semantic_context]\nenabled=true\n");
        match load(fixture.path()).expect_err("derived store leaf symlink escape must be rejected")
        {
            ConfigError::InvalidSemanticContextConfiguration { field, reason } => {
                assert_eq!(field, "semantic_context.store.path");
                assert_eq!(reason, SemanticContextConfigurationReason::SymlinkEscape);
            }
            other => panic!("expected semantic store confinement error, got {other:?}"),
        }
        assert!(!outside.path().join("outside.sqlite3").exists());
    }

    fn assert_invalid_shell_configuration(
        body: &str,
        expected_field: &'static str,
        expected_reason: ShellConfigurationReason,
    ) {
        let fixture = tempdir().unwrap();
        write_config(fixture.path(), body);

        match load(fixture.path()).expect_err("invalid shell policy must be rejected") {
            ConfigError::InvalidShellConfiguration { field, reason } => {
                assert_eq!(field, expected_field);
                assert_eq!(reason, expected_reason);
            }
            other => panic!("expected InvalidShellConfiguration, got {other:?}"),
        }
    }

    #[test]
    fn shell_config_absent_and_disabled_are_inert() {
        let absent = tempdir().unwrap();
        let absent_config = load(absent.path()).expect("absent configuration must use defaults");
        assert_eq!(absent_config.shell.enabled_policy(), None);
        assert!(!absent_config.shell.enabled);
        assert!(absent_config.shell.commands.is_empty());
        assert_eq!(absent_config.shell.timeout_secs, 30);
        assert_eq!(absent_config.shell.stdout_max_bytes, 65_536);
        assert_eq!(absent_config.shell.stderr_max_bytes, 65_536);

        let disabled = tempdir().unwrap();
        write_config(
            disabled.path(),
            "[shell]\nenabled = false\ntimeout_secs = 1\nstdout_max_bytes = 1\nstderr_max_bytes = 16777216\n",
        );
        let disabled_config = load(disabled.path()).expect("disabled policy must remain valid");
        assert_eq!(disabled_config.shell.enabled_policy(), None);
    }

    #[test]
    fn shell_config_preserves_named_direct_argv_and_limits() {
        let fixture = tempdir().unwrap();
        write_config(
            fixture.path(),
            "[shell]\nenabled = true\ntimeout_secs = 86400\nstdout_max_bytes = 16777216\nstderr_max_bytes = 7\n\
             [[shell.commands]]\nname = \"check\"\nargv = [\"cargo\", \"check\", \"\"]\n\
             [[shell.commands]]\nname = \"literal-shell-syntax\"\nargv = [\"printf\", \"a | b > c & d\"]\n",
        );

        let config = load(fixture.path()).expect("valid shell policy must load");
        let policy = config
            .shell
            .enabled_policy()
            .expect("enabled policy must be exposed");
        assert_eq!(
            policy.commands,
            [
                AllowedCommand {
                    name: "check".to_owned(),
                    argv: vec!["cargo".to_owned(), "check".to_owned(), String::new()],
                },
                AllowedCommand {
                    name: "literal-shell-syntax".to_owned(),
                    argv: vec!["printf".to_owned(), "a | b > c & d".to_owned()],
                },
            ]
        );
        assert_eq!(
            std::time::Duration::from_secs(policy.timeout_secs),
            std::time::Duration::from_secs(86_400)
        );
        assert_eq!(policy.stdout_max_bytes, 16_777_216);
        assert_eq!(policy.stderr_max_bytes, 7);
    }

    #[test]
    fn shell_config_rejects_invalid_policy() {
        let valid_limits =
            "timeout_secs = 30\nstdout_max_bytes = 65536\nstderr_max_bytes = 65536\n";
        let cases = [
            (
                format!("[shell]\nenabled = true\n{valid_limits}"),
                "shell.commands",
                ShellConfigurationReason::EmptyCommands,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"\"\nargv = [\"true\"]\n"),
                "shell.commands.name",
                ShellConfigurationReason::InvalidName,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"bad.name\"\nargv = [\"true\"]\n"),
                "shell.commands.name",
                ShellConfigurationReason::InvalidName,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"-leading-dash\"\nargv = [\"true\"]\n"),
                "shell.commands.name",
                ShellConfigurationReason::InvalidName,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"{}\"\nargv = [\"true\"]\n", "a".repeat(65)),
                "shell.commands.name",
                ShellConfigurationReason::InvalidName,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"same\"\nargv = [\"true\"]\n[[shell.commands]]\nname = \"same\"\nargv = [\"false\"]\n"),
                "shell.commands.name",
                ShellConfigurationReason::DuplicateName,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"empty\"\nargv = []\n"),
                "shell.commands.argv",
                ShellConfigurationReason::EmptyArgv,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"empty\"\nargv = [\"\", \"literal\"]\n"),
                "shell.commands.argv",
                ShellConfigurationReason::EmptyArgv,
            ),
            (
                format!("[shell]\nenabled = true\n{valid_limits}[[shell.commands]]\nname = \"nul\"\nargv = [\"true\", \"a\\u0000b\"]\n"),
                "shell.commands.argv",
                ShellConfigurationReason::InvalidArgv,
            ),
            (
                "[shell]\nenabled = true\ntimeout_secs = 0\nstdout_max_bytes = 65536\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"ok\"\nargv = [\"true\"]\n".to_owned(),
                "shell.timeout_secs",
                ShellConfigurationReason::InvalidTimeout,
            ),
            (
                "[shell]\nenabled = true\ntimeout_secs = 86401\nstdout_max_bytes = 65536\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"ok\"\nargv = [\"true\"]\n".to_owned(),
                "shell.timeout_secs",
                ShellConfigurationReason::InvalidTimeout,
            ),
            (
                "[shell]\nenabled = true\ntimeout_secs = 18446744073709551615\nstdout_max_bytes = 65536\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"ok\"\nargv = [\"true\"]\n".to_owned(),
                "shell.timeout_secs",
                ShellConfigurationReason::InvalidTimeout,
            ),
            (
                "[shell]\nenabled = true\ntimeout_secs = 30\nstdout_max_bytes = 0\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"ok\"\nargv = [\"true\"]\n".to_owned(),
                "shell.stdout_max_bytes",
                ShellConfigurationReason::InvalidOutputLimit,
            ),
            (
                "[shell]\nenabled = true\ntimeout_secs = 30\nstdout_max_bytes = 16777217\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"ok\"\nargv = [\"true\"]\n".to_owned(),
                "shell.stdout_max_bytes",
                ShellConfigurationReason::InvalidOutputLimit,
            ),
            (
                "[shell]\nenabled = true\ntimeout_secs = 30\nstdout_max_bytes = 65536\nstderr_max_bytes = 0\n[[shell.commands]]\nname = \"ok\"\nargv = [\"true\"]\n".to_owned(),
                "shell.stderr_max_bytes",
                ShellConfigurationReason::InvalidOutputLimit,
            ),
            (
                "[shell]\nenabled = true\ntimeout_secs = 30\nstdout_max_bytes = 65536\nstderr_max_bytes = 16777217\n[[shell.commands]]\nname = \"ok\"\nargv = [\"true\"]\n".to_owned(),
                "shell.stderr_max_bytes",
                ShellConfigurationReason::InvalidOutputLimit,
            ),
            (
                "[shell]\nenabled = false\ntimeout_secs = 30\nstdout_max_bytes = 65536\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"-disabled-invalid-name\"\nargv = [\"true\"]\n".to_owned(),
                "shell.commands.name",
                ShellConfigurationReason::InvalidName,
            ),
            (
                "[shell]\nenabled = false\ntimeout_secs = 30\nstdout_max_bytes = 0\nstderr_max_bytes = 65536\n".to_owned(),
                "shell.stdout_max_bytes",
                ShellConfigurationReason::InvalidOutputLimit,
            ),
        ];

        for (body, field, reason) in cases {
            assert_invalid_shell_configuration(&body, field, reason);
        }

        let valid_maximum_name = "a".repeat(64);
        let valid_name_fixture = tempdir().unwrap();
        write_config(
            valid_name_fixture.path(),
            &format!(
                "[shell]\nenabled = true\ntimeout_secs = 30\nstdout_max_bytes = 65536\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"{valid_maximum_name}\"\nargv = [\"true\"]\n"
            ),
        );
        let valid_name_config =
            load(valid_name_fixture.path()).expect("a 64-byte command name must load");
        assert_eq!(valid_name_config.shell.commands[0].name, valid_maximum_name);

        let rejected_value_sentinel = "shell-rejected-value-sentinel";
        let redaction_fixture = tempdir().unwrap();
        write_config(
            redaction_fixture.path(),
            &format!(
                "[shell]\nenabled = true\ntimeout_secs = 30\nstdout_max_bytes = 65536\nstderr_max_bytes = 65536\n[[shell.commands]]\nname = \"{rejected_value_sentinel}.\"\nargv = [\"true\"]\n"
            ),
        );
        let redaction_error =
            load(redaction_fixture.path()).expect_err("the sentinel command name must be rejected");
        assert!(matches!(
            &redaction_error,
            ConfigError::InvalidShellConfiguration {
                field: "shell.commands.name",
                reason: ShellConfigurationReason::InvalidName,
            }
        ));
        let rendered = redaction_error.to_string();
        assert!(rendered.contains("shell.commands.name"), "{rendered}");
        assert!(rendered.contains("InvalidName"), "{rendered}");
        assert!(!rendered.contains(rejected_value_sentinel), "{rendered}");

        let unknown = tempdir().unwrap();
        write_config(
            unknown.path(),
            "[shell]\nenabled = false\nunexpected = true\n",
        );
        assert!(matches!(
            load(unknown.path()),
            Err(ConfigError::Parse { .. })
        ));
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

    fn dependency_contract_example() -> &'static str {
        let guide = include_str!("../../../../../docs/user-guide/configuration.md");
        guide
            .split("```toml\n")
            .find(|block| block.starts_with("[dependencies]\n"))
            .expect("documented dependency TOML example")
            .split_once("\n```")
            .expect("dependency TOML closing fence")
            .0
    }

    fn dependency_root_ids(config: &TowerConfig) -> Vec<DependencyRootId> {
        config
            .dependencies
            .roots
            .iter()
            .map(|root| root.id.clone())
            .collect()
    }

    #[test]
    fn dependency_configuration_parses_documented_example_and_defaults() {
        let fixture = tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let sibling = fixture.path().join("library-src");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        write_config(&workspace, dependency_contract_example());

        let config = load(&workspace).expect("documented dependency configuration must load");

        assert_eq!(
            config.dependencies.roots,
            vec![ResolvedDependencyRoot {
                id: DependencyRootId::new("library".to_owned()).unwrap(),
                path: sibling.canonicalize().unwrap(),
            }]
        );
        assert_eq!(config.dependencies.ignore, ["target/", "*.generated.rs"]);
        assert_eq!(
            config.dependencies.limits,
            DependencyIndexLimits::new(10_000, 1_048_576, 67_108_864).unwrap()
        );

        write_config(
            &workspace,
            "[dependencies]\n\n[[dependencies.roots]]\nid = \"library\"\npath = \"../library-src\"\n",
        );
        let defaults = load(&workspace).expect("omitted dependency fields must use defaults");
        assert!(defaults.dependencies.ignore.is_empty());
        assert_eq!(defaults.dependencies.limits, config.dependencies.limits);
    }

    #[test]
    fn dependency_search_configuration_rejects_invalid_limits_and_roots() {
        let workspace = tempdir().unwrap();

        for field in ["max_files", "max_file_bytes", "max_total_bytes"] {
            write_config(workspace.path(), &format!("[dependencies]\n{field} = 0\n"));
            assert!(matches!(
                load(workspace.path()),
                Err(ConfigError::InvalidDependencyLimit {
                    field: actual,
                    value: 0,
                }) if actual == field
            ));
        }

        for body in [
            "[dependencies\n",
            "[dependencies]\nunknown = true\n",
            "[dependencies]\nmax_files = -1\n",
            "[[dependencies.roots]]\nid = \"missing-path\"\n",
        ] {
            write_config(workspace.path(), body);
            assert!(matches!(
                load(workspace.path()),
                Err(ConfigError::Parse { .. })
            ));
        }

        let cases = [
            ("", DependencyRootPathReason::Empty),
            ("unsafe\\u0000root", DependencyRootPathReason::Unsafe),
        ];
        for (configured_path, reason) in cases {
            write_config(
                workspace.path(),
                &format!("[[dependencies.roots]]\nid = \"root\"\npath = \"{configured_path}\"\n"),
            );
            let error = load(workspace.path()).expect_err("invalid root must be rejected");
            assert!(matches!(
                error,
                ConfigError::InvalidDependencyRoot {
                    index: 0,
                    reason: actual,
                } if actual == reason
            ));
        }

        let regular_file = workspace.path().join("dependency-file");
        fs::write(&regular_file, "private-content-sentinel").unwrap();
        write_config(
            workspace.path(),
            "[[dependencies.roots]]\nid = \"file\"\npath = \"dependency-file\"\n",
        );
        let error = load(workspace.path()).expect_err("regular file must be rejected");
        assert!(matches!(
            error,
            ConfigError::InvalidDependencyRoot {
                index: 0,
                reason: DependencyRootPathReason::NotDirectory,
            }
        ));
        let rendered = error.to_string();
        assert!(!rendered.contains("private-content-sentinel"));
        assert!(!rendered.contains(regular_file.to_string_lossy().as_ref()));

        let external_with_git = tempdir().unwrap();
        fs::create_dir_all(external_with_git.path().join(".git")).unwrap();
        let external_with_tower = tempdir().unwrap();
        fs::create_dir_all(external_with_tower.path().join(".tower")).unwrap();
        fs::create_dir_all(workspace.path().join(".git")).unwrap();
        fs::create_dir_all(workspace.path().join(".git/hooks")).unwrap();
        fs::create_dir_all(workspace.path().join(".tower/cache")).unwrap();
        let mut prohibited_paths = vec![
            workspace.path().parent().unwrap().to_path_buf(),
            workspace.path().join(".tower"),
            workspace.path().join(".tower/cache"),
            workspace.path().join(".git"),
            workspace.path().join(".git/hooks"),
            external_with_git.path().to_path_buf(),
            external_with_tower.path().to_path_buf(),
        ];
        #[cfg(unix)]
        prohibited_paths.extend(
            [
                "/",
                "/proc",
                "/proc/self",
                "/sys",
                "/sys/kernel",
                "/dev",
                "/dev/shm",
                "/etc",
                "/etc/..",
                "/boot",
                "/run",
            ]
            .into_iter()
            .map(PathBuf::from),
        );
        #[cfg(windows)]
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            prohibited_paths.push(PathBuf::from(system_root));
        }

        for prohibited in prohibited_paths {
            write_config(
                workspace.path(),
                &format!(
                    "[[dependencies.roots]]\nid = \"metadata\"\npath = {:?}\n",
                    prohibited.to_string_lossy()
                ),
            );
            let error = load(workspace.path()).expect_err("prohibited root must be rejected");
            assert!(matches!(
                error,
                ConfigError::InvalidDependencyRoot {
                    index: 0,
                    reason: DependencyRootPathReason::Prohibited,
                }
            ));
            assert!(
                !error
                    .to_string()
                    .contains(prohibited.to_string_lossy().as_ref())
            );
        }

        fs::create_dir_all(workspace.path().join("dependency")).unwrap();
        write_config(
            workspace.path(),
            "[[dependencies.roots]]\nid = \"duplicate\"\npath = \"dependency\"\n\n[[dependencies.roots]]\nid = \"duplicate\"\npath = \"dependency/.\"\n",
        );
        assert!(matches!(
            load(workspace.path()),
            Err(ConfigError::DuplicateDependencyRootId { id })
                if id == DependencyRootId::new("duplicate".to_owned()).unwrap()
        ));

        let raw_os_sentinel = "raw-os-error-sentinel";
        let private_path_sentinel = workspace.path().join("unrelated-private-path-sentinel");
        let unreadable = workspace.path().join("unreadable-root");
        write_config(
            workspace.path(),
            "[[dependencies.roots]]\nid = \"unreadable\"\npath = \"unreadable-root\"\n",
        );
        let error = load_with_path_canonicalizer(workspace.path(), |path| {
            if path == unreadable {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{raw_os_sentinel}: {}", private_path_sentinel.display()),
                ))
            } else {
                fs::canonicalize(path)
            }
        })
        .expect_err("canonicalizer failure must be sanitized through load");
        assert!(matches!(
            error,
            ConfigError::InvalidDependencyRoot {
                index: 0,
                reason: DependencyRootPathReason::Unsafe,
            }
        ));
        let rendered = error.to_string();
        assert!(!rendered.contains(raw_os_sentinel), "{rendered}");
        assert!(
            !rendered.contains(private_path_sentinel.to_string_lossy().as_ref()),
            "{rendered}"
        );
    }

    #[test]
    fn dependency_configuration_preserves_outermost_root_in_both_orders() {
        let workspace = tempdir().unwrap();
        let parent = workspace.path().join("dependency");
        let child = parent.join("nested");
        fs::create_dir_all(&child).unwrap();
        fs::write(parent.join("parent-only.rs"), "pub struct ParentOnly;").unwrap();

        let equal = "[[dependencies.roots]]\nid = \"z-first\"\npath = \"dependency\"\n\n[[dependencies.roots]]\nid = \"a-second\"\npath = \"dependency/.\"\n";
        write_config(workspace.path(), equal);
        let config = load(workspace.path()).expect("equivalent roots must deduplicate");
        assert_eq!(
            dependency_root_ids(&config),
            [DependencyRootId::new("z-first".to_owned()).unwrap()]
        );

        #[cfg(unix)]
        {
            let alias = workspace.path().join("dependency-alias");
            std::os::unix::fs::symlink(&parent, &alias).unwrap();
            write_config(
                workspace.path(),
                "[[dependencies.roots]]\nid = \"alias-first\"\npath = \"dependency-alias\"\n\n[[dependencies.roots]]\nid = \"original-second\"\npath = \"dependency\"\n",
            );
            let config = load(workspace.path()).expect("directory aliases must deduplicate");
            assert_eq!(
                config.dependencies.roots,
                [ResolvedDependencyRoot {
                    id: DependencyRootId::new("alias-first".to_owned()).unwrap(),
                    path: parent.canonicalize().unwrap(),
                }]
            );
        }

        for body in [
            "[[dependencies.roots]]\nid = \"z-parent\"\npath = \"dependency\"\n\n[[dependencies.roots]]\nid = \"a-child\"\npath = \"dependency/nested\"\n",
            "[[dependencies.roots]]\nid = \"a-child\"\npath = \"dependency/nested\"\n\n[[dependencies.roots]]\nid = \"z-parent\"\npath = \"dependency\"\n",
        ] {
            write_config(workspace.path(), body);
            let config = load(workspace.path()).expect("nested roots must deduplicate");
            assert_eq!(
                config.dependencies.roots,
                [ResolvedDependencyRoot {
                    id: DependencyRootId::new("z-parent".to_owned()).unwrap(),
                    path: parent.canonicalize().unwrap(),
                }]
            );
            assert!(
                config.dependencies.roots[0]
                    .path
                    .join("parent-only.rs")
                    .is_file()
            );
        }

        fs::create_dir_all(workspace.path().join("another")).unwrap();
        write_config(
            workspace.path(),
            "[[dependencies.roots]]\nid = \"z-root\"\npath = \"dependency\"\n\n[[dependencies.roots]]\nid = \"a-root\"\npath = \"another\"\n",
        );
        let sorted = load(workspace.path()).expect("distinct roots must load");
        assert_eq!(
            dependency_root_ids(&sorted),
            [
                DependencyRootId::new("a-root".to_owned()).unwrap(),
                DependencyRootId::new("z-root".to_owned()).unwrap(),
            ]
        );
    }

    #[test]
    fn dependency_configuration_accepts_missing_roots_for_scan_reporting() {
        let workspace = tempdir().unwrap();
        write_config(
            workspace.path(),
            "[[dependencies.roots]]\nid = \"missing\"\npath = \"vendor/not-yet-installed\"\n",
        );

        let config = load(workspace.path()).expect("safe missing root must remain configured");

        assert_eq!(
            config.dependencies.roots,
            [ResolvedDependencyRoot {
                id: DependencyRootId::new("missing".to_owned()).unwrap(),
                path: workspace
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("vendor/not-yet-installed"),
            }]
        );
        assert!(!config.dependencies.roots[0].path.exists());

        let unreadable = workspace.path().join("vendor/unreadable");
        fs::create_dir_all(unreadable.parent().unwrap()).unwrap();
        write_config(
            workspace.path(),
            "[[dependencies.roots]]\nid = \"unreadable\"\npath = \"vendor/unreadable\"\n",
        );
        let config = load_with_path_canonicalizer(workspace.path(), |path| {
            if path == unreadable {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                fs::canonicalize(path)
            }
        })
        .expect("a safely bounded unreadable root must remain configured");
        assert_eq!(
            config.dependencies.roots,
            [ResolvedDependencyRoot {
                id: DependencyRootId::new("unreadable".to_owned()).unwrap(),
                path: workspace
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("vendor/unreadable"),
            }]
        );
    }

    #[test]
    fn dependency_configuration_rejects_invalid_ids_and_ignore_rules() {
        let workspace = tempdir().unwrap();
        fs::create_dir_all(workspace.path().join("dependency")).unwrap();

        for invalid_id in ["", ".", "..", "with/slash", "with\\\\slash"] {
            write_config(
                workspace.path(),
                &format!(
                    "[[dependencies.roots]]\nid = {:?}\npath = \"dependency\"\n",
                    invalid_id
                ),
            );
            assert!(matches!(
                load(workspace.path()),
                Err(ConfigError::InvalidDependencyRoot {
                    index: 0,
                    reason: DependencyRootPathReason::InvalidId,
                })
            ));
        }

        write_config(workspace.path(), "[dependencies]\nignore = [\"[\"]\n");
        let error = load(workspace.path()).expect_err("invalid ignore rule must be rejected");
        assert!(matches!(
            error,
            ConfigError::InvalidDependencyIgnore { index: 0 }
        ));
        assert!(!error.to_string().contains('['));
    }

    #[test]
    fn dependency_configuration_preserves_workspace_source_ownership() {
        let workspace = tempdir().unwrap();
        let source = workspace.path().join("src");
        fs::create_dir_all(&source).unwrap();
        write_config(
            workspace.path(),
            "[[dependencies.roots]]\nid = \"workspace\"\npath = \".\"\n\n[[dependencies.roots]]\nid = \"source\"\npath = \"src\"\n",
        );

        let config = load(workspace.path()).expect("workspace source roots are valid");

        assert_eq!(
            config.dependencies.roots,
            [ResolvedDependencyRoot {
                id: DependencyRootId::new("workspace".to_owned()).unwrap(),
                path: workspace.path().canonicalize().unwrap(),
            }]
        );

        #[cfg(unix)]
        {
            let ordinary_etc = workspace.path().join("src/etc/dependency");
            fs::create_dir_all(&ordinary_etc).unwrap();
            write_config(
                workspace.path(),
                "[[dependencies.roots]]\nid = \"ordinary-etc\"\npath = \"src/etc/dependency\"\n",
            );

            let config = load(workspace.path())
                .expect("ordinary directories named like system roots must remain valid");
            assert_eq!(
                config.dependencies.roots,
                [ResolvedDependencyRoot {
                    id: DependencyRootId::new("ordinary-etc".to_owned()).unwrap(),
                    path: ordinary_etc.canonicalize().unwrap(),
                }]
            );
        }
    }

    #[test]
    fn dependency_search_configuration_absence_preserves_defaults() {
        let workspace = tempdir().unwrap();
        write_config(workspace.path(), "[extensions]\ndisabled = [\"ast\"]\n");

        let config = load_with_path_canonicalizer(workspace.path(), |_| {
            panic!("zero dependency roots must not invoke path canonicalization")
        })
        .expect("unrelated configuration must load without dependency discovery");

        assert!(config.dependencies.roots.is_empty());
        assert!(config.dependencies.ignore.is_empty());
        assert_eq!(
            config.dependencies.limits,
            DependenciesConfig::default().limits
        );
        assert_eq!(config.extensions.disabled, ["ast"]);
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
            "[extensions]\ndisabled = [\"slow_start\"]\n[extensions.readiness]\nslow_start = \"ast\"\nsemantic = \"lsp\"\nsemantic_search = \"semantic_context\"\n",
        );
        let cfg = load(tmp.path()).unwrap();
        assert_eq!(cfg.extensions.readiness["slow_start"], ReadinessFamily::Ast);
        assert_eq!(cfg.extensions.readiness["semantic"], ReadinessFamily::Lsp);
        assert_eq!(
            cfg.extensions.readiness["semantic_search"],
            ReadinessFamily::SemanticContext
        );
        assert_eq!(
            serde_json::to_value(ReadinessFamily::SemanticContext).unwrap(),
            serde_json::json!("semantic_context")
        );
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
