#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DebugConfig {
    pub languages: BTreeMap<String, DebugLanguageConfig>,
}

impl DebugConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.languages.is_empty()
    }

    #[must_use]
    pub fn for_extension_initialize(&self) -> Option<serde_json::Value> {
        (!self.is_empty()).then(|| {
            serde_json::json!({
                "languages": self.languages,
            })
        })
    }
}

impl<'de> Deserialize<'de> for DebugConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = BTreeMap::<String, RawDebugLanguageConfig>::deserialize(deserializer)?;
        let mut languages = BTreeMap::new();

        for (language, config) in raw {
            if language.is_empty() {
                return Err(de::Error::custom("debug language key must not be empty"));
            }

            let config = config.validate(Some(&language))?;
            languages.insert(language, config);
        }

        Ok(Self { languages })
    }
}

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
    use super::DebugLanguageConfig;
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
                }
            }))
        );
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
