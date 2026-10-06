//! `[lsp]` config table — maps a language to its language-server command.
//!
//! Shape in `.tower/config.toml`:
//!
//! ```toml
//! [lsp.rust]
//! command = "rust-analyzer"
//! extensions = ["rs"]
//! args = []          # optional
//! ```
//!
//! Absent `[lsp]` table → empty config (no servers). Malformed → the caller
//! (startup) treats a parse error as fatal, matching the existing config policy.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LspInitializeError {
    MissingConfiguration,
    InvalidConfiguration,
}

pub fn lsp_initialize_payload(config: &LspConfig) -> serde_json::Value {
    serde_json::to_value(LspInitializePayload {
        servers: &config.servers,
        idle_timeout_secs: config.idle_timeout.map(|timeout| timeout.as_secs()),
    })
    .expect("LSP initialization configuration contains only serializable values")
}

pub fn decode_lsp_initialize_config(
    payload: Option<&serde_json::Value>,
) -> Result<LspConfig, LspInitializeError> {
    let payload = payload.ok_or(LspInitializeError::MissingConfiguration)?;
    let payload: OwnedLspInitializePayload = serde_json::from_value(payload.clone())
        .map_err(|_| LspInitializeError::InvalidConfiguration)?;

    Ok(LspConfig {
        servers: payload.servers,
        idle_timeout: payload.idle_timeout_secs.map(Duration::from_secs),
    })
}

#[derive(Serialize)]
struct LspInitializePayload<'a> {
    servers: &'a BTreeMap<String, LspServerConfig>,
    idle_timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct OwnedLspInitializePayload {
    servers: BTreeMap<String, LspServerConfig>,
    idle_timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLspCommand {
    pub executable: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LspCommandError {
    InvalidCommand,
    ServerMissing,
    ServerNotExecutable,
    ServerLaunchFailed,
}

pub fn resolve_lsp_command(
    command: &str,
    workspace_root: &Path,
    search_path: Option<&OsStr>,
) -> Result<ResolvedLspCommand, LspCommandError> {
    if command.trim().is_empty() {
        return Err(LspCommandError::InvalidCommand);
    }

    let command_path = Path::new(command);
    if is_explicit_path(command, command_path) {
        let candidate = if command_path.is_absolute() {
            command_path.to_path_buf()
        } else {
            workspace_root.join(command_path)
        };
        return validate_executable(&candidate);
    }

    let Some(search_path) = search_path else {
        return Err(LspCommandError::ServerMissing);
    };

    let mut detected_error = None;
    for directory in std::env::split_paths(search_path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            workspace_root.join(directory)
        };
        for candidate in executable_candidates(command_path) {
            match validate_executable(&directory.join(candidate)) {
                Ok(resolved) => return Ok(resolved),
                Err(LspCommandError::ServerMissing) => {}
                Err(error) => {
                    detected_error.get_or_insert(error);
                }
            }
        }
    }

    Err(detected_error.unwrap_or(LspCommandError::ServerMissing))
}

pub fn validate_lsp_commands(
    config: &LspConfig,
    workspace_root: &Path,
    search_path: Option<&OsStr>,
) -> Vec<extension_protocol::LspOutcome> {
    config
        .servers
        .iter()
        .filter_map(|(language, server)| {
            let error = resolve_lsp_command(&server.command, workspace_root, search_path).err()?;
            let (code, message) = match error {
                LspCommandError::InvalidCommand => (
                    extension_protocol::LspOutcomeCode::InvalidCommand,
                    format!("Configured language server command for {language} is empty."),
                ),
                LspCommandError::ServerMissing => (
                    extension_protocol::LspOutcomeCode::ServerMissing,
                    format!(
                        "Configured language server {} for {language} was not found.",
                        server.command
                    ),
                ),
                LspCommandError::ServerNotExecutable => (
                    extension_protocol::LspOutcomeCode::ServerNotExecutable,
                    format!(
                        "Configured language server {} for {language} is not executable.",
                        server.command
                    ),
                ),
                LspCommandError::ServerLaunchFailed => (
                    extension_protocol::LspOutcomeCode::ServerLaunchFailed,
                    format!(
                        "Configured language server {} for {language} could not be validated.",
                        server.command
                    ),
                ),
            };

            Some(extension_protocol::LspOutcome {
                status: extension_protocol::LspOutcomeStatus::Error,
                code,
                language: Some(language.clone()),
                command: Some(server.command.clone()),
                operation: None,
                path: None,
                phase: extension_protocol::LspOutcomePhase::Startup,
                message,
            })
        })
        .collect()
}

fn is_explicit_path(command: &str, path: &Path) -> bool {
    path.is_absolute()
        || path.components().count() > 1
        || matches!(command, "." | "..")
        || command.chars().any(std::path::is_separator)
}

#[cfg(not(windows))]
fn executable_candidates(command: &Path) -> Vec<PathBuf> {
    vec![command.to_path_buf()]
}

#[cfg(windows)]
fn executable_candidates(command: &Path) -> Vec<PathBuf> {
    if command.extension().is_some() {
        return vec![command.to_path_buf()];
    }

    let extensions = std::env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| [".COM", ".EXE", ".BAT", ".CMD"].map(str::to_owned).to_vec());

    std::iter::once(command.to_path_buf())
        .chain(extensions.into_iter().map(|extension| {
            let mut candidate = command.as_os_str().to_os_string();
            candidate.push(extension);
            PathBuf::from(candidate)
        }))
        .collect()
}

fn validate_executable(path: &Path) -> Result<ResolvedLspCommand, LspCommandError> {
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(LspCommandError::ServerMissing);
        }
        Err(_) => return Err(LspCommandError::ServerLaunchFailed),
    };

    if !metadata.is_file() || !has_execute_permission(&metadata) {
        return Err(LspCommandError::ServerNotExecutable);
    }

    match probe_executable_format(path) {
        Ok(true) => {}
        Ok(false) => return Err(LspCommandError::ServerNotExecutable),
        Err(error) if probe_failure_allows_execution(&error) => {}
        Err(_) => return Err(LspCommandError::ServerLaunchFailed),
    }

    let executable = path
        .canonicalize()
        .map_err(|_| LspCommandError::ServerLaunchFailed)?;
    Ok(ResolvedLspCommand { executable })
}

#[cfg(unix)]
fn has_execute_permission(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn has_execute_permission(_metadata: &std::fs::Metadata) -> bool {
    true
}

fn probe_executable_format(path: &Path) -> std::io::Result<bool> {
    let mut header = [0_u8; 4];
    let mut file = File::open(path)?;
    let read = file.read(&mut header)?;

    Ok(read >= 2
        && (header.starts_with(b"#!")
            || header.starts_with(b"MZ")
            || (read == header.len()
                && matches!(
                    header,
                    [0x7f, b'E', b'L', b'F']
                        | [0xfe, 0xed, 0xfa, 0xce]
                        | [0xfe, 0xed, 0xfa, 0xcf]
                        | [0xce, 0xfa, 0xed, 0xfe]
                        | [0xcf, 0xfa, 0xed, 0xfe]
                        | [0xca, 0xfe, 0xba, 0xbe]
                        | [0xbe, 0xba, 0xfe, 0xca]
                ))))
}

#[cfg(unix)]
fn probe_failure_allows_execution(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(not(unix))]
fn probe_failure_allows_execution(_error: &std::io::Error) -> bool {
    false
}

/// One language server entry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspServerConfig {
    /// The server binary, e.g. `"rust-analyzer"`.
    pub command: String,
    /// File extensions (without dot) this server handles, e.g. `["rs"]`.
    pub extensions: Vec<String>,
    /// Extra CLI args passed to the server. Defaults to empty.
    #[serde(default)]
    pub args: Vec<String>,
}

/// The parsed `[lsp]` table.
///
/// TOML shape:
/// ```toml
/// [lsp]
/// idle_timeout_secs = 300   # optional; absent = sessions stay resident
///
/// [lsp.rust]
/// command = "rust-analyzer"
/// extensions = ["rs"]
/// ```
///
/// # Manual `Deserialize`
///
/// `LspConfig` was previously `#[serde(transparent)]` over
/// `BTreeMap<String, LspServerConfig>`, which cannot carry a sibling scalar
/// field. The manual impl peels `idle_timeout_secs` from the raw TOML map and
/// treats every remaining sub-table as a language entry. Unknown bare keys
/// (e.g. a mistyped `idle_timeoutt_secs = 10`) will fail at
/// `LspServerConfig::deserialize` with "expected a map" — clear enough for
/// the startup-error-and-exit policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LspConfig {
    /// One entry per language; key is the language name (e.g. `"rust"`).
    pub servers: BTreeMap<String, LspServerConfig>,
    /// How long a session may be idle before the pool shuts it down.
    /// `None` means sessions stay resident indefinitely.
    pub idle_timeout: Option<Duration>,
}

impl<'de> serde::Deserialize<'de> for LspConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let mut map = BTreeMap::<String, toml::Value>::deserialize(d)?;

        let idle_timeout = match map.remove("idle_timeout_secs") {
            None => None,
            Some(v) => {
                let secs = v.as_integer().ok_or_else(|| {
                    D::Error::custom("idle_timeout_secs must be a non-negative integer")
                })?;
                let secs_u64 = u64::try_from(secs)
                    .map_err(|_| D::Error::custom("idle_timeout_secs must be >= 0"))?;
                Some(Duration::from_secs(secs_u64))
            }
        };

        let servers = map
            .into_iter()
            .map(|(lang, val)| {
                let server = LspServerConfig::deserialize(val).map_err(D::Error::custom)?;
                Ok((lang, server))
            })
            .collect::<Result<BTreeMap<_, _>, D::Error>>()?;

        Ok(LspConfig {
            servers,
            idle_timeout,
        })
    }
}

impl LspConfig {
    /// Resolve the server config for a file extension (without dot), if any.
    #[must_use]
    pub fn for_extension(&self, ext: &str) -> Option<&LspServerConfig> {
        self.servers
            .values()
            .find(|s| s.extensions.iter().any(|e| e == ext))
    }
}

/// Parse the `[lsp]` sub-table out of a full `.tower/config.toml` string.
///
/// Returns an empty `LspConfig` when the `[lsp]` table is absent.
///
/// # Errors
///
/// Returns the `toml` error string if the `[lsp]` table is present but malformed.
///
pub fn parse_lsp_config(toml_src: &str) -> Result<LspConfig, String> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        lsp: LspConfig,
    }
    let root: Root = toml::from_str(toml_src).map_err(|e| e.to_string())?;
    Ok(root.lsp)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn f008_initialize_codec_implements_the_exact_codec_without_filesystem_access() {
        let config = LspConfig {
            servers: BTreeMap::from([(
                "rust".to_owned(),
                LspServerConfig {
                    command: String::new(),
                    extensions: vec!["rs".to_owned()],
                    args: Vec::new(),
                },
            )]),
            idle_timeout: None,
        };

        let payload = lsp_initialize_payload(&config);

        assert_eq!(
            payload,
            serde_json::json!({
                "servers": {
                    "rust": {
                        "command": "",
                        "extensions": ["rs"],
                        "args": [],
                    }
                },
                "idle_timeout_secs": null,
            })
        );
        assert_eq!(decode_lsp_initialize_config(Some(&payload)), Ok(config));
    }

    #[test]
    fn f008_initialize_codec_round_trips_languages_arguments_and_timeout_and_rejects_invalid_payloads()
     {
        let config = LspConfig {
            servers: BTreeMap::from([
                (
                    "go".to_owned(),
                    LspServerConfig {
                        command: "gopls".to_owned(),
                        extensions: vec!["go".to_owned()],
                        args: vec!["serve".to_owned()],
                    },
                ),
                (
                    "rust".to_owned(),
                    LspServerConfig {
                        command: "rust-analyzer".to_owned(),
                        extensions: vec!["rs".to_owned()],
                        args: vec!["--log-file".to_owned(), "/tmp/log with spaces".to_owned()],
                    },
                ),
            ]),
            idle_timeout: Some(Duration::from_secs(300)),
        };

        let payload = lsp_initialize_payload(&config);
        assert_eq!(decode_lsp_initialize_config(Some(&payload)), Ok(config));

        let empty = serde_json::json!({"servers": {}, "idle_timeout_secs": null});
        assert_eq!(
            decode_lsp_initialize_config(Some(&empty)),
            Ok(LspConfig::default())
        );

        let timeout_omitted = serde_json::json!({
            "servers": {
                "rust": {
                    "command": "rust-analyzer",
                    "extensions": ["rs"],
                }
            }
        });
        assert_eq!(
            decode_lsp_initialize_config(Some(&timeout_omitted)),
            Ok(LspConfig {
                servers: BTreeMap::from([(
                    "rust".to_owned(),
                    LspServerConfig {
                        command: "rust-analyzer".to_owned(),
                        extensions: vec!["rs".to_owned()],
                        args: Vec::new(),
                    },
                )]),
                idle_timeout: None,
            }),
            "an absent idle_timeout_secs must mean no idle timeout"
        );

        let invalid_payloads = [
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"servers": []}),
            serde_json::json!({"servers": {"rust": {"extensions": ["rs"]}}}),
            serde_json::json!({"servers": {"rust": {"command": 7, "extensions": ["rs"]}}}),
            serde_json::json!({"servers": {"rust": {"command": "ra"}}}),
            serde_json::json!({"servers": {"rust": {"command": "ra", "extensions": "rs"}}}),
            serde_json::json!({"servers": {"rust": {"command": "ra", "extensions": ["rs"], "args": [7]}}}),
            serde_json::json!({"servers": {"rust": {"command": "ra", "extensions": [7]}}}),
            serde_json::json!({"servers": {}, "idle_timeout_secs": -1}),
            serde_json::json!({"servers": {}, "idle_timeout_secs": 1.5}),
            serde_json::json!({"servers": {}, "idle_timeout_secs": "300"}),
            serde_json::json!({"servers": {}, "idle_timeout_secs": 1e30}),
        ];

        assert_eq!(
            decode_lsp_initialize_config(None),
            Err(LspInitializeError::MissingConfiguration)
        );
        for payload in invalid_payloads {
            assert_eq!(
                decode_lsp_initialize_config(Some(&payload)),
                Err(LspInitializeError::InvalidConfiguration),
                "payload {payload}"
            );
        }
    }

    #[test]
    fn f008_initialize_codec_keeps_errors_unit_like_and_uses_one_additive_schema() {
        fn assert_unit_like(error: LspInitializeError) {
            match error {
                LspInitializeError::MissingConfiguration
                | LspInitializeError::InvalidConfiguration => {}
            }
        }

        assert_unit_like(LspInitializeError::MissingConfiguration);
        assert_unit_like(LspInitializeError::InvalidConfiguration);

        let payload = serde_json::json!({
            "servers": {
                "rust": {
                    "command": "rust-analyzer",
                    "extensions": ["rs"],
                    "future_server_option": true,
                }
            },
            "idle_timeout_secs": null,
            "future_top_level_option": {"enabled": true},
        });
        let decoded = decode_lsp_initialize_config(Some(&payload)).unwrap();
        let produced = lsp_initialize_payload(&decoded);

        assert_eq!(
            produced,
            serde_json::json!({
                "servers": {
                    "rust": {
                        "command": "rust-analyzer",
                        "extensions": ["rs"],
                        "args": [],
                    }
                },
                "idle_timeout_secs": null,
            })
        );
    }

    #[cfg(unix)]
    fn write_executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn write_invalid_executable(path: &Path) {
        std::fs::write(path, "not a real executable format").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn f008_resolver_inspects_detectable_prerequisites_without_launching_a_process() {
        let workspace = tempfile::tempdir().unwrap();
        let invalid_executable = workspace.path().join("server");
        write_invalid_executable(&invalid_executable);
        let non_executable = workspace.path().join("disabled-server");
        std::fs::write(&non_executable, "server").unwrap();
        std::fs::set_permissions(&non_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
        let directory = workspace.path().join("server-directory");
        std::fs::create_dir(&directory).unwrap();
        let trailing_directory = format!("server-directory{}", std::path::MAIN_SEPARATOR);

        let cases = [
            ("".to_owned(), LspCommandError::InvalidCommand),
            ("  \t\n".to_owned(), LspCommandError::InvalidCommand),
            ("missing/server".to_owned(), LspCommandError::ServerMissing),
            ("./server".to_owned(), LspCommandError::ServerNotExecutable),
            (".".to_owned(), LspCommandError::ServerNotExecutable),
            (trailing_directory, LspCommandError::ServerNotExecutable),
            (
                "./server-directory".to_owned(),
                LspCommandError::ServerNotExecutable,
            ),
            (
                "./disabled-server".to_owned(),
                LspCommandError::ServerNotExecutable,
            ),
        ];

        for (command, expected) in cases {
            assert_eq!(
                resolve_lsp_command(&command, workspace.path(), None),
                Err(expected),
                "command {command:?}"
            );
        }

        assert_eq!(
            resolve_lsp_command("server", workspace.path(), None),
            Err(LspCommandError::ServerMissing)
        );
    }

    #[test]
    #[cfg(unix)]
    fn f008_resolver_accepts_an_execute_only_native_file() {
        let workspace = tempfile::tempdir().unwrap();
        let executable = workspace.path().join("execute-only-server");
        write_executable(&executable);
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o111)).unwrap();

        assert_eq!(
            resolve_lsp_command("./execute-only-server", workspace.path(), None).unwrap(),
            ResolvedLspCommand {
                executable: executable.canonicalize().unwrap(),
            }
        );
    }

    #[test]
    #[cfg(windows)]
    fn f008_resolver_expands_windows_executable_suffixes_for_bare_names() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = workspace.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let executable = bin.join("language-server.exe");
        std::fs::write(&executable, b"MZ").unwrap();
        let search_path = std::env::join_paths([&bin]).unwrap();

        assert_eq!(
            resolve_lsp_command("language-server", workspace.path(), Some(&search_path)).unwrap(),
            ResolvedLspCommand {
                executable: executable.canonicalize().unwrap(),
            }
        );
    }

    #[test]
    #[cfg(unix)]
    fn f008_resolver_resolves_workspace_relative_absolute_and_space_containing_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let relative = Path::new("tools with spaces").join("language server");
        let executable = workspace.path().join(&relative);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        write_executable(&executable);

        for command in [relative.as_path(), executable.as_path()] {
            let command = command.to_str().unwrap();
            assert_eq!(
                resolve_lsp_command(command, workspace.path(), None).unwrap(),
                ResolvedLspCommand {
                    executable: executable.canonicalize().unwrap(),
                },
                "command {command:?}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn f008_resolver_uses_only_the_supplied_search_path_for_bare_names() {
        let workspace = tempfile::tempdir().unwrap();
        let relative_bin = workspace.path().join("relative bin");
        let absolute_bin = workspace.path().join("absolute bin");
        std::fs::create_dir_all(&relative_bin).unwrap();
        std::fs::create_dir_all(&absolute_bin).unwrap();
        let relative_server = relative_bin.join("relative-server");
        let absolute_server = absolute_bin.join("absolute-server");
        write_executable(&relative_server);
        write_executable(&absolute_server);

        let search_path = std::env::join_paths([Path::new("relative bin"), &absolute_bin]).unwrap();
        let cases = [
            ("relative-server", &relative_server),
            ("absolute-server", &absolute_server),
        ];

        for (command, expected) in cases {
            assert_eq!(
                resolve_lsp_command(command, workspace.path(), Some(&search_path)).unwrap(),
                ResolvedLspCommand {
                    executable: expected.canonicalize().unwrap(),
                },
                "command {command:?}"
            );
        }

        assert_eq!(
            resolve_lsp_command("relative-server", workspace.path(), None),
            Err(LspCommandError::ServerMissing)
        );
    }

    #[test]
    fn absent_lsp_table_yields_empty_config() {
        let cfg = parse_lsp_config("[plugins]\ndisabled = []\n").unwrap();
        assert!(cfg.servers.is_empty());
        assert!(cfg.for_extension("rs").is_none());
    }

    #[test]
    fn absent_idle_timeout_yields_none() {
        let src = r#"
            [lsp.rust]
            command = "rust-analyzer"
            extensions = ["rs"]
        "#;
        let cfg = parse_lsp_config(src).unwrap();
        assert!(cfg.idle_timeout.is_none());
        assert!(cfg.servers.contains_key("rust"));
    }

    #[test]
    fn idle_timeout_secs_parses_to_duration() {
        let src = r#"
            [lsp]
            idle_timeout_secs = 300

            [lsp.rust]
            command = "rust-analyzer"
            extensions = ["rs"]
        "#;
        let cfg = parse_lsp_config(src).unwrap();
        assert_eq!(cfg.idle_timeout, Some(std::time::Duration::from_secs(300)));
        assert!(cfg.servers.contains_key("rust"));
    }

    #[test]
    fn negative_idle_timeout_is_error() {
        // TOML integers are i64; negative values are syntactically valid TOML
        // but semantically invalid for a duration.
        let src =
            "[lsp]\nidle_timeout_secs = -1\n[lsp.rust]\ncommand = \"ra\"\nextensions = [\"rs\"]\n";
        assert!(
            parse_lsp_config(src).is_err(),
            "negative idle_timeout_secs must be rejected"
        );
    }

    #[test]
    fn non_integer_idle_timeout_is_error() {
        let src = "[lsp]\nidle_timeout_secs = \"five\"\n[lsp.rust]\ncommand = \"ra\"\nextensions = [\"rs\"]\n";
        assert!(parse_lsp_config(src).is_err());
    }

    #[test]
    fn existing_lsp_rust_table_still_parses_after_transparent_removal() {
        // Backward-compat guard: the #[serde(transparent)] removal must not
        // break existing configs that have no idle_timeout_secs field.
        let src = r#"
            [lsp.rust]
            command = "rust-analyzer"
            extensions = ["rs"]
            args = ["--log-file", "/tmp/ra.log"]
        "#;
        let cfg = parse_lsp_config(src).unwrap();
        let server = cfg.for_extension("rs").expect("rs must resolve");
        assert_eq!(server.command, "rust-analyzer");
        assert_eq!(server.args, vec!["--log-file", "/tmp/ra.log"]);
        assert!(cfg.idle_timeout.is_none());
    }

    #[test]
    fn multi_language_config_with_idle_timeout() {
        let src = r#"
            [lsp]
            idle_timeout_secs = 600

            [lsp.rust]
            command = "rust-analyzer"
            extensions = ["rs"]

            [lsp.go]
            command = "gopls"
            extensions = ["go"]
        "#;
        let cfg = parse_lsp_config(src).unwrap();
        assert_eq!(cfg.idle_timeout, Some(std::time::Duration::from_secs(600)));
        assert!(cfg.servers.contains_key("rust"));
        assert!(cfg.servers.contains_key("go"));
    }

    #[test]
    fn parses_rust_server_entry() {
        let src = r#"
            [lsp.rust]
            command = "rust-analyzer"
            extensions = ["rs"]
        "#;
        let cfg = parse_lsp_config(src).unwrap();
        let server = cfg.for_extension("rs").expect("rs must resolve");
        assert_eq!(server.command, "rust-analyzer");
        assert!(server.args.is_empty());
    }

    #[test]
    fn parses_optional_args() {
        let src = r#"
            [lsp.typescript]
            command = "typescript-language-server"
            extensions = ["ts", "tsx"]
            args = ["--stdio"]
        "#;
        let cfg = parse_lsp_config(src).unwrap();
        let server = cfg.for_extension("tsx").unwrap();
        assert_eq!(server.args, vec!["--stdio".to_owned()]);
    }

    #[test]
    fn malformed_lsp_table_is_error() {
        // `command` must be a string, not an integer.
        let src = "[lsp.rust]\ncommand = 42\nextensions = [\"rs\"]\n";
        assert!(parse_lsp_config(src).is_err());
    }
}
