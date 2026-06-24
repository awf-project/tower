#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DebugConfig {
    pub languages: BTreeMap<String, DebugLanguageConfig>,
    pub record: Option<DebugRecordConfig>,
}

impl DebugConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.languages.is_empty() && self.record.is_none()
    }

    #[must_use]
    pub fn for_extension_initialize(&self) -> Option<serde_json::Value> {
        (!self.is_empty()).then(|| {
            serde_json::json!({
                "languages": self.languages,
                "record": self.record,
            })
        })
    }
}

impl<'de> Deserialize<'de> for DebugConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
        let mut languages = BTreeMap::new();
        let mut record = None;

        for (key, value) in raw {
            if key == "record" {
                let raw_record: RawDebugRecordConfig =
                    value.try_into().map_err(de::Error::custom)?;
                record = Some(raw_record.validate().map_err(de::Error::custom)?);
                continue;
            }

            if key.is_empty() {
                return Err(de::Error::custom("debug language key must not be empty"));
            }

            let config: RawDebugLanguageConfig = value.try_into().map_err(de::Error::custom)?;
            let config = config.validate(Some(&key))?;
            languages.insert(key, config);
        }

        Ok(Self { languages, record })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugRecordConfig {
    pub backend: String,
    pub trace_dir: Option<PathBuf>,
    pub ttl_secs: Option<u64>,
    pub max_traces: Option<usize>,
    pub record_timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDebugRecordConfig {
    backend: Option<String>,
    #[serde(default)]
    trace_dir: Option<PathBuf>,
    #[serde(default)]
    ttl_secs: Option<toml::Value>,
    #[serde(default)]
    max_traces: Option<toml::Value>,
    #[serde(default)]
    record_timeout_secs: Option<toml::Value>,
}

impl RawDebugRecordConfig {
    pub fn validate(self) -> Result<DebugRecordConfig, DebugRecordConfigError> {
        let Self {
            backend,
            trace_dir,
            ttl_secs,
            max_traces,
            record_timeout_secs,
        } = self;

        let backend = backend.ok_or(DebugRecordConfigError::MissingBackend)?;
        if backend != "rr" {
            return Err(DebugRecordConfigError::UnsupportedBackend { backend });
        }

        if trace_dir
            .as_deref()
            .is_some_and(|path| !is_valid_relative_trace_dir(path))
        {
            return Err(DebugRecordConfigError::InvalidTraceDir {
                field: "debug.record.trace_dir",
            });
        }

        Ok(DebugRecordConfig {
            backend,
            trace_dir,
            ttl_secs: optional_positive_u64(
                ttl_secs.as_ref(),
                DebugRecordConfigError::InvalidTtlSecs {
                    field: "debug.record.ttl_secs",
                },
            )?,
            max_traces: optional_positive_usize(
                max_traces.as_ref(),
                DebugRecordConfigError::InvalidMaxTraces {
                    field: "debug.record.max_traces",
                },
            )?,
            record_timeout_secs: optional_positive_u64(
                record_timeout_secs.as_ref(),
                DebugRecordConfigError::InvalidRecordTimeoutSecs {
                    field: "debug.record.record_timeout_secs",
                },
            )?,
        })
    }
}

fn optional_positive_u64(
    value: Option<&toml::Value>,
    error: DebugRecordConfigError,
) -> Result<Option<u64>, DebugRecordConfigError> {
    value.map_or(Ok(None), |value| {
        value
            .as_integer()
            .and_then(|integer| u64::try_from(integer).ok())
            .filter(|integer| *integer > 0)
            .map(Some)
            .ok_or(error)
    })
}

fn optional_positive_usize(
    value: Option<&toml::Value>,
    error: DebugRecordConfigError,
) -> Result<Option<usize>, DebugRecordConfigError> {
    value.map_or(Ok(None), |value| {
        value
            .as_integer()
            .and_then(|integer| usize::try_from(integer).ok())
            .filter(|integer| *integer > 0)
            .map(Some)
            .ok_or(error)
    })
}

fn is_valid_relative_trace_dir(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugRecordConfigError {
    MissingBackend,
    UnsupportedBackend { backend: String },
    InvalidTtlSecs { field: &'static str },
    InvalidTraceDir { field: &'static str },
    InvalidMaxTraces { field: &'static str },
    InvalidRecordTimeoutSecs { field: &'static str },
}

impl fmt::Display for DebugRecordConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBackend => formatter.write_str("debug.record.backend is required"),
            Self::UnsupportedBackend { backend } => {
                write!(formatter, "unsupported debug.record.backend {backend:?}")
            }
            Self::InvalidTtlSecs { field }
            | Self::InvalidTraceDir { field }
            | Self::InvalidMaxTraces { field }
            | Self::InvalidRecordTimeoutSecs { field } => {
                write!(formatter, "{field} is invalid")
            }
        }
    }
}

impl std::error::Error for DebugRecordConfigError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDebugLanguageConfig {
    extensions: Vec<String>,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    adapter_type: String,
    #[serde(default)]
    launch: serde_json::Map<String, serde_json::Value>,
    default_timeout_secs: toml::Value,
    idle_ttl_secs: toml::Value,
}

impl RawDebugLanguageConfig {
    fn validate<E>(self, language: Option<&str>) -> Result<DebugLanguageConfig, E>
    where
        E: de::Error,
    {
        let field_name = |field: &str| match language {
            Some(language) => format!("debug.{language}.{field}"),
            None => field.to_string(),
        };

        if self.command.is_empty() {
            return Err(E::custom(format!(
                "{} must not be empty",
                field_name("command")
            )));
        }
        if self.adapter_type.is_empty() {
            return Err(E::custom(format!(
                "{} must not be empty",
                field_name("adapter_type")
            )));
        }
        if self.extensions.is_empty() {
            return Err(E::custom(format!(
                "{} must not be empty",
                field_name("extensions")
            )));
        }
        if let Some(extension) = self
            .extensions
            .iter()
            .find(|extension| extension.starts_with('.'))
        {
            return Err(E::custom(format!(
                "{} entry {extension:?} must not start with '.'",
                field_name("extensions")
            )));
        }

        Ok(DebugLanguageConfig {
            extensions: self.extensions,
            command: self.command,
            args: self.args,
            adapter_type: self.adapter_type,
            launch: self.launch,
            default_timeout_secs: positive_duration_secs(
                language,
                "default_timeout_secs",
                &self.default_timeout_secs,
            )?,
            idle_ttl_secs: positive_duration_secs(language, "idle_ttl_secs", &self.idle_ttl_secs)?,
        })
    }
}

fn positive_duration_secs<E>(
    language: Option<&str>,
    field: &str,
    value: &toml::Value,
) -> Result<u64, E>
where
    E: de::Error,
{
    let field_name = match language {
        Some(language) => format!("debug.{language}.{field}"),
        None => field.to_string(),
    };

    let Some(secs) = value.as_integer() else {
        return Err(E::custom(format!(
            "{field_name} must be a positive integer"
        )));
    };

    u64::try_from(secs)
        .ok()
        .filter(|secs| *secs > 0)
        .ok_or_else(|| E::custom(format!("{field_name} must be a positive integer")))
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DebugLanguageConfig {
    pub extensions: Vec<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub adapter_type: String,
    #[serde(default)]
    pub launch: serde_json::Map<String, serde_json::Value>,
    pub default_timeout_secs: u64,
    pub idle_ttl_secs: u64,
}

impl<'de> Deserialize<'de> for DebugLanguageConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawDebugLanguageConfig::deserialize(deserializer)?.validate(None)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        DebugLanguageConfig, DebugRecordConfig, DebugRecordConfigError, RawDebugRecordConfig,
    };
    use crate::adapters::config::TowerConfig;
    use serde_json::json;

    fn parse_tower_config(src: &str) -> Result<TowerConfig, toml::de::Error> {
        toml::from_str(src)
    }

    fn parse_debug_language_config(src: &str) -> Result<DebugLanguageConfig, toml::de::Error> {
        toml::from_str(src)
    }

    #[test]
    fn debug_config_parses_a_missing_debug_section_as_empty_and_exposes_is_empty_true() {
        let cfg = parse_tower_config("[plugins]\ndisabled = []\n").expect("config must parse");

        assert!(cfg.debug.is_empty());
        assert_eq!(cfg.debug.for_extension_initialize(), None);
    }

    #[test]
    fn debug_config_parses_valid_multi_language_entries_with_adapter_command_argv_adapter_type_launch_defaults_default_timeout_and_idle_ttl()
     {
        let cfg = parse_tower_config(
            r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
args = ["--stdio"]
adapter_type = "lldb"
launch = { request = "launch", program = "target/debug/app" }
default_timeout_secs = 15
idle_ttl_secs = 300

[debug.go]
extensions = ["go"]
command = "dlv"
args = ["dap"]
adapter_type = "go"
launch = { request = "launch", mode = "debug", program = "." }
default_timeout_secs = 20
idle_ttl_secs = 120
"#,
        )
        .expect("multi-language debug config must parse");

        let rust = cfg
            .debug
            .languages
            .get("rust")
            .expect("rust language config must be present");
        assert_eq!(rust.extensions, vec!["rs"]);
        assert_eq!(rust.command, "lldb-dap");
        assert_eq!(rust.args, vec!["--stdio"]);
        assert_eq!(rust.adapter_type, "lldb");
        assert_eq!(rust.launch["request"], json!("launch"));
        assert_eq!(rust.launch["program"], json!("target/debug/app"));
        assert_eq!(rust.default_timeout_secs, 15);
        assert_eq!(rust.idle_ttl_secs, 300);

        let go = cfg
            .debug
            .languages
            .get("go")
            .expect("go language config must be present");
        assert_eq!(go.extensions, vec!["go"]);
        assert_eq!(go.command, "dlv");
        assert_eq!(go.args, vec!["dap"]);
        assert_eq!(go.adapter_type, "go");
        assert_eq!(go.launch["mode"], json!("debug"));
        assert_eq!(go.default_timeout_secs, 20);
        assert_eq!(go.idle_ttl_secs, 120);

        assert_eq!(
            cfg.debug.for_extension_initialize(),
            Some(json!({
                "languages": {
                    "go": {
                        "extensions": ["go"],
                        "command": "dlv",
                        "args": ["dap"],
                        "adapter_type": "go",
                        "launch": { "request": "launch", "mode": "debug", "program": "." },
                        "default_timeout_secs": 20,
                        "idle_ttl_secs": 120
                    },
                    "rust": {
                        "extensions": ["rs"],
                        "command": "lldb-dap",
                        "args": ["--stdio"],
                        "adapter_type": "lldb",
                        "launch": { "request": "launch", "program": "target/debug/app" },
                        "default_timeout_secs": 15,
                        "idle_ttl_secs": 300
                    }
                },
                "record": null
            }))
        );
    }

    #[test]
    fn debug_config_parses_host_side_debug_record_config_with_exact_public_serde_fields() {
        let config = DebugRecordConfig {
            backend: "rr".to_owned(),
            trace_dir: Some(PathBuf::from(".tower/traces")),
            ttl_secs: Some(86_400),
            max_traces: Some(25),
            record_timeout_secs: Some(30),
        };

        let value = serde_json::to_value(&config).expect("record config serializes");

        assert_eq!(value["backend"], json!("rr"));
        assert_eq!(value["trace_dir"], json!(".tower/traces"));
        assert_eq!(value["ttl_secs"], json!(86_400));
        assert_eq!(value["max_traces"], json!(25));
        assert_eq!(value["record_timeout_secs"], json!(30));
    }

    #[test]
    fn debug_config_parses_debug_config_includes_record_without_changing_absent_empty_or_present_language_configs()
     {
        let absent = parse_tower_config("[plugins]\ndisabled = []\n").expect("absent debug parses");
        assert!(absent.debug.languages.is_empty());
        assert_eq!(absent.debug.record, None);

        let empty = parse_tower_config("[debug]\n").expect("empty debug parses");
        assert!(empty.debug.languages.is_empty());
        assert_eq!(empty.debug.record, None);

        let present = parse_tower_config(
            r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect("present language config parses");

        assert!(present.debug.languages.contains_key("rust"));
        assert_eq!(present.debug.record, None);
    }

    #[test]
    fn debug_config_record_serializes_through_extension_config_without_dropping_language_settings()
    {
        let cfg = parse_tower_config(
            r#"
[debug.record]
backend = "rr"
trace_dir = ".tower/traces"
ttl_secs = 86400
max_traces = 25
record_timeout_secs = 30

[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
args = ["--quiet"]
adapter_type = "lldb"
launch = { request = "launch", program = "target/debug/app" }
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect("record and language config parse together");

        assert_eq!(
            cfg.debug.for_extension_initialize(),
            Some(json!({
                "languages": {
                    "rust": {
                        "extensions": ["rs"],
                        "command": "lldb-dap",
                        "args": ["--quiet"],
                        "adapter_type": "lldb",
                        "launch": { "request": "launch", "program": "target/debug/app" },
                        "default_timeout_secs": 15,
                        "idle_ttl_secs": 300
                    }
                },
                "record": {
                    "backend": "rr",
                    "trace_dir": ".tower/traces",
                    "ttl_secs": 86400,
                    "max_traces": 25,
                    "record_timeout_secs": 30,
                }
            }))
        );
    }

    #[test]
    fn debug_config_parses_absent_debug_record_as_record_none() {
        let cfg = parse_tower_config(
            r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect("debug config without record parses");

        assert_eq!(cfg.debug.record, None);
    }

    #[test]
    fn debug_config_record_rr_backend_as_debug_record_config_and_preserves_retention_values() {
        let cfg = parse_tower_config(
            r#"
[debug.record]
backend = "rr"
trace_dir = ".tower/traces"
ttl_secs = 86400
max_traces = 25
record_timeout_secs = 30
"#,
        )
        .expect("rr record config parses");

        assert_eq!(
            cfg.debug.record,
            Some(DebugRecordConfig {
                backend: "rr".to_owned(),
                trace_dir: Some(PathBuf::from(".tower/traces")),
                ttl_secs: Some(86_400),
                max_traces: Some(25),
                record_timeout_secs: Some(30),
            })
        );
    }

    #[test]
    fn debug_config_record_raw_debug_record_config_validates_present_record_table_before_constructing_debug_record_config()
     {
        let raw = RawDebugRecordConfig {
            backend: Some("rr".to_owned()),
            trace_dir: Some(PathBuf::from(".tower/traces")),
            ttl_secs: Some(toml::Value::Integer(86_400)),
            max_traces: Some(toml::Value::Integer(25)),
            record_timeout_secs: Some(toml::Value::Integer(30)),
        };

        assert_eq!(
            raw.validate().expect("valid raw record config"),
            DebugRecordConfig {
                backend: "rr".to_owned(),
                trace_dir: Some(PathBuf::from(".tower/traces")),
                ttl_secs: Some(86_400),
                max_traces: Some(25),
                record_timeout_secs: Some(30),
            }
        );
    }

    #[test]
    fn debug_config_parses_record_config_validation_uses_named_debug_record_config_error_variants_and_renders_field_names()
     {
        let cases = [
            (
                DebugRecordConfigError::MissingBackend,
                "debug.record.backend",
            ),
            (
                DebugRecordConfigError::UnsupportedBackend {
                    backend: "gdb".to_owned(),
                },
                "debug.record.backend",
            ),
            (
                DebugRecordConfigError::InvalidTtlSecs {
                    field: "debug.record.ttl_secs",
                },
                "debug.record.ttl_secs",
            ),
            (
                DebugRecordConfigError::InvalidTraceDir {
                    field: "debug.record.trace_dir",
                },
                "debug.record.trace_dir",
            ),
            (
                DebugRecordConfigError::InvalidMaxTraces {
                    field: "debug.record.max_traces",
                },
                "debug.record.max_traces",
            ),
            (
                DebugRecordConfigError::InvalidRecordTimeoutSecs {
                    field: "debug.record.record_timeout_secs",
                },
                "debug.record.record_timeout_secs",
            ),
        ];

        for (error, field) in cases {
            let message = error.to_string();
            assert!(message.contains(field), "{message}");
        }
    }

    #[test]
    fn debug_config_record_unsupported_backend_values_return_debug_record_config_error_unsupported_backend()
     {
        let raw = RawDebugRecordConfig {
            backend: Some("gdb".to_owned()),
            trace_dir: None,
            ttl_secs: None,
            max_traces: None,
            record_timeout_secs: None,
        };

        assert_eq!(
            raw.validate().expect_err("unsupported backend is rejected"),
            DebugRecordConfigError::UnsupportedBackend {
                backend: "gdb".to_owned()
            }
        );
    }

    #[test]
    fn debug_config_record_invalid_values_return_exact_debug_record_config_error_field_names() {
        for (name, raw, expected) in [
            (
                "invalid ttl_secs",
                RawDebugRecordConfig {
                    backend: Some("rr".to_owned()),
                    trace_dir: None,
                    ttl_secs: Some(toml::Value::Integer(0)),
                    max_traces: None,
                    record_timeout_secs: None,
                },
                DebugRecordConfigError::InvalidTtlSecs {
                    field: "debug.record.ttl_secs",
                },
            ),
            (
                "invalid absolute trace_dir",
                RawDebugRecordConfig {
                    backend: Some("rr".to_owned()),
                    trace_dir: Some(PathBuf::from("/tmp/tower-traces")),
                    ttl_secs: None,
                    max_traces: None,
                    record_timeout_secs: None,
                },
                DebugRecordConfigError::InvalidTraceDir {
                    field: "debug.record.trace_dir",
                },
            ),
            (
                "invalid traversal trace_dir",
                RawDebugRecordConfig {
                    backend: Some("rr".to_owned()),
                    trace_dir: Some(PathBuf::from("../traces")),
                    ttl_secs: None,
                    max_traces: None,
                    record_timeout_secs: None,
                },
                DebugRecordConfigError::InvalidTraceDir {
                    field: "debug.record.trace_dir",
                },
            ),
            (
                "invalid max_traces",
                RawDebugRecordConfig {
                    backend: Some("rr".to_owned()),
                    trace_dir: None,
                    ttl_secs: None,
                    max_traces: Some(toml::Value::Integer(0)),
                    record_timeout_secs: None,
                },
                DebugRecordConfigError::InvalidMaxTraces {
                    field: "debug.record.max_traces",
                },
            ),
            (
                "invalid record_timeout_secs",
                RawDebugRecordConfig {
                    backend: Some("rr".to_owned()),
                    trace_dir: None,
                    ttl_secs: None,
                    max_traces: None,
                    record_timeout_secs: Some(toml::Value::Integer(0)),
                },
                DebugRecordConfigError::InvalidRecordTimeoutSecs {
                    field: "debug.record.record_timeout_secs",
                },
            ),
        ] {
            assert_eq!(raw.validate().expect_err(name), expected, "{name}");
        }
    }

    #[test]
    fn debug_language_config_rejects_an_empty_adapter_command_with_a_config_parse_error_that_names_the_language_key()
     {
        let err = parse_tower_config(
            r#"
[debug.rust]
extensions = ["rs"]
command = ""
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect_err("empty debug adapter command must be rejected");
        let message = err.to_string();

        assert!(message.contains("rust"), "{message}");
        assert!(message.contains("command"), "{message}");
    }

    #[test]
    fn debug_language_config_rejects_an_empty_adapter_type_with_a_config_parse_error_that_names_the_language_key()
     {
        let err = parse_tower_config(
            r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = ""
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect_err("empty debug adapter type must be rejected");
        let message = err.to_string();

        assert!(message.contains("rust"), "{message}");
        assert!(message.contains("adapter_type"), "{message}");
    }

    #[test]
    fn debug_language_config_rejects_empty_extensions_with_a_config_parse_error_that_names_the_language_key()
     {
        let err = parse_tower_config(
            r#"
[debug.rust]
extensions = []
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect_err("empty debug extensions list must be rejected");
        let message = err.to_string();

        assert!(message.contains("rust"), "{message}");
        assert!(message.contains("extensions"), "{message}");
    }

    #[test]
    fn debug_language_config_rejects_extension_entries_with_a_leading_dot_with_a_config_parse_error_that_names_the_language_key()
     {
        let err = parse_tower_config(
            r#"
[debug.rust]
extensions = [".rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect_err("debug extensions with a leading dot must be rejected");
        let message = err.to_string();

        assert!(message.contains("rust"), "{message}");
        assert!(message.contains("extensions"), "{message}");
        assert!(message.contains(".rs"), "{message}");
    }

    #[test]
    fn debug_config_rejects_empty_language_keys_with_a_config_parse_error() {
        let err = parse_tower_config(
            r#"
[debug.""]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
        )
        .expect_err("empty debug language key must be rejected");
        let message = err.to_string();

        assert!(message.contains("language"), "{message}");
        assert!(message.contains("empty"), "{message}");
    }

    #[test]
    fn unknown_keys_below_debug_lang_are_rejected_by_deny_unknown_fields() {
        let err = parse_tower_config(
            r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
launch = { request = "launch" }
default_timeout_secs = 15
idle_ttl_secs = 300
unexpected = true
"#,
        )
        .expect_err("unknown debug language keys must be rejected");
        let message = err.to_string();

        assert!(message.contains("unexpected"), "{message}");
    }

    #[test]
    fn negative_zero_where_forbidden_or_non_integer_duration_values_for_timeout_or_idle_ttl_return_a_config_parse_error()
     {
        for (name, src) in [
            (
                "negative default_timeout_secs",
                r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = -1
idle_ttl_secs = 300
"#,
            ),
            (
                "zero default_timeout_secs",
                r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 0
idle_ttl_secs = 300
"#,
            ),
            (
                "non-integer default_timeout_secs",
                r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = "15"
idle_ttl_secs = 300
"#,
            ),
            (
                "negative idle_ttl_secs",
                r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = -1
"#,
            ),
            (
                "zero idle_ttl_secs",
                r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 0
"#,
            ),
            (
                "non-integer idle_ttl_secs",
                r#"
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = "300"
"#,
            ),
        ] {
            let err = parse_tower_config(src).expect_err(name);
            let message = err.to_string();

            assert!(message.contains("rust"), "{name}: {message}");
            assert!(
                message.contains("default_timeout_secs") || message.contains("idle_ttl_secs"),
                "{name}: {message}"
            );
        }
    }

    #[test]
    fn tower_config_includes_a_public_debug_field_whose_value_is_empty_when_debug_is_absent() {
        let cfg = TowerConfig::default();

        assert!(cfg.debug.is_empty());
    }

    #[test]
    fn public_debug_language_config_deserialization_rejects_invalid_values() {
        for (name, src, expected_field) in [
            (
                "empty command",
                r#"
extensions = ["rs"]
command = ""
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
                "command",
            ),
            (
                "empty adapter_type",
                r#"
extensions = ["rs"]
command = "lldb-dap"
adapter_type = ""
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
                "adapter_type",
            ),
            (
                "empty extensions",
                r#"
extensions = []
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
                "extensions",
            ),
            (
                "leading-dot extension",
                r#"
extensions = [".rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 300
"#,
                "extensions",
            ),
            (
                "zero default_timeout_secs",
                r#"
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 0
idle_ttl_secs = 300
"#,
                "default_timeout_secs",
            ),
            (
                "zero idle_ttl_secs",
                r#"
extensions = ["rs"]
command = "lldb-dap"
adapter_type = "lldb"
default_timeout_secs = 15
idle_ttl_secs = 0
"#,
                "idle_ttl_secs",
            ),
        ] {
            let err = parse_debug_language_config(src).expect_err(name);
            let message = err.to_string();

            assert!(message.contains(expected_field), "{name}: {message}");
        }
    }
}
