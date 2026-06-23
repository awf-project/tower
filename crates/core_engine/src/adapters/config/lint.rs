#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct LintConfig {
    pub commands: BTreeMap<String, LintCommandConfig>,
}

impl LintConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    #[must_use]
    pub fn command_for_extension(&self, extension: &str) -> Option<(&str, &LintCommandConfig)> {
        let extension = extension.strip_prefix('.').unwrap_or(extension);

        self.commands
            .iter()
            .find(|(_, command)| {
                command
                    .extensions
                    .iter()
                    .any(|candidate| candidate == extension)
            })
            .map(|(language, command)| (language.as_str(), command))
    }

    #[must_use]
    pub fn command_for_path(&self, path: &str) -> Option<(&str, &LintCommandConfig)> {
        std::path::Path::new(path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .and_then(|extension| self.command_for_extension(extension))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintCommandConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    pub format: ParserFormat,
    pub target: TargetMode,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParserFormat {
    RustcJson,
    EslintJson,
    GenericRegex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetMode {
    Append,
    Stdin,
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::{ConfigError, TowerConfig, load};
    use serde::Deserialize;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[derive(Deserialize)]
    struct ParserFormatValue {
        value: ParserFormat,
    }

    #[derive(Deserialize)]
    struct TargetModeValue {
        value: TargetMode,
    }

    fn parse_tower_config(src: &str) -> Result<TowerConfig, toml::de::Error> {
        toml::from_str(src)
    }

    fn write_config(root: &Path, body: &str) {
        let dir = root.join(".tower");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), body).unwrap();
    }

    #[test]
    fn tower_config_exposes_lint_field_that_is_empty_when_config_has_no_lint_table() {
        let cfg = parse_tower_config("[plugins]\ndisabled = []\n").expect("config must parse");

        assert!(cfg.lint.is_empty());
    }

    #[test]
    fn lint_config_parses_multiple_language_entries_under_lint_lang_and_keeps_each_language_key_available_for_lookup()
     {
        let cfg = parse_tower_config(
            r#"
[lint.rust]
command = "cargo"
args = ["check", "--message-format=json"]
extensions = ["rs"]
format = "rustc-json"
target = "append"

[lint.javascript]
command = "eslint"
extensions = ["js", "jsx"]
format = "eslint-json"
target = "append"
"#,
        )
        .expect("multi-language lint config must parse");

        assert!(cfg.lint.commands.contains_key("rust"));
        assert!(cfg.lint.commands.contains_key("javascript"));
        assert_eq!(cfg.lint.commands["rust"].command, "cargo");
        assert_eq!(cfg.lint.commands["javascript"].command, "eslint");
    }

    #[test]
    fn lint_config_exposes_is_empty_command_for_extension_and_command_for_path() {
        let cfg = parse_tower_config(
            r#"
[lint.rust]
command = "cargo"
args = ["check", "--message-format=json"]
extensions = ["rs"]
format = "rustc-json"
target = "append"
"#,
        )
        .expect("lint config must parse");

        assert!(!cfg.lint.is_empty());

        let (extension_lang, extension_command) = cfg
            .lint
            .command_for_extension("rs")
            .expect("rs extension must resolve");
        assert_eq!(extension_lang, "rust");
        assert_eq!(extension_command.command, "cargo");

        let (path_lang, path_command) = cfg
            .lint
            .command_for_path("src/main.rs")
            .expect("rs path must resolve");
        assert_eq!(path_lang, "rust");
        assert_eq!(path_command.command, "cargo");
    }

    #[test]
    fn lint_command_config_is_public_host_side_config_contract_with_exact_fields_and_serde_defaults()
     {
        let command: LintCommandConfig = toml::from_str(
            r#"
command = "eslint"
extensions = ["js"]
format = "eslint-json"
target = "stdin"
"#,
        )
        .expect("required fields plus defaulted optional fields must parse");

        assert_eq!(command.command, "eslint");
        assert!(command.args.is_empty());
        assert_eq!(command.extensions, vec!["js"]);
        assert_eq!(command.format, ParserFormat::EslintJson);
        assert_eq!(command.target, TargetMode::Stdin);
        assert_eq!(command.regex, None);
        assert_eq!(command.source, None);

        let with_optional_fields: LintCommandConfig = toml::from_str(
            r#"
command = "lint"
args = ["--json"]
extensions = ["txt"]
format = "generic-regex"
target = "none"
regex = "^(?<message>.*)$"
source = "custom-lint"
"#,
        )
        .expect("all contract fields must parse");

        assert_eq!(with_optional_fields.args, vec!["--json"]);
        assert_eq!(
            with_optional_fields.regex.as_deref(),
            Some("^(?<message>.*)$")
        );
        assert_eq!(with_optional_fields.source.as_deref(), Some("custom-lint"));
    }

    #[test]
    fn lint_command_config_rejects_unknown_fields_and_only_defaults_args_regex_and_source() {
        let unknown_field = toml::from_str::<LintCommandConfig>(
            r#"
command = "cargo"
extensions = ["rs"]
format = "rustc-json"
target = "append"
unexpected = true
"#,
        );
        assert!(unknown_field.is_err());

        let missing_extensions = toml::from_str::<LintCommandConfig>(
            r#"
command = "cargo"
format = "rustc-json"
target = "append"
"#,
        );
        assert!(missing_extensions.is_err());

        let missing_command = toml::from_str::<LintCommandConfig>(
            r#"
extensions = ["rs"]
format = "rustc-json"
target = "append"
"#,
        );
        assert!(missing_command.is_err());

        let missing_format = toml::from_str::<LintCommandConfig>(
            r#"
command = "cargo"
extensions = ["rs"]
target = "append"
"#,
        );
        assert!(missing_format.is_err());

        let missing_target = toml::from_str::<LintCommandConfig>(
            r#"
command = "cargo"
extensions = ["rs"]
format = "rustc-json"
"#,
        );
        assert!(missing_target.is_err());
    }

    #[test]
    fn parser_format_accepts_exactly_rustc_json_eslint_json_and_generic_regex() {
        assert_eq!(
            toml::from_str::<ParserFormatValue>("value = \"rustc-json\"")
                .unwrap()
                .value,
            ParserFormat::RustcJson
        );
        assert_eq!(
            toml::from_str::<ParserFormatValue>("value = \"eslint-json\"")
                .unwrap()
                .value,
            ParserFormat::EslintJson
        );
        assert_eq!(
            toml::from_str::<ParserFormatValue>("value = \"generic-regex\"")
                .unwrap()
                .value,
            ParserFormat::GenericRegex
        );
        assert!(toml::from_str::<ParserFormatValue>("value = \"rustc\"").is_err());
    }

    #[test]
    fn target_mode_accepts_exactly_append_stdin_and_none() {
        assert_eq!(
            toml::from_str::<TargetModeValue>("value = \"append\"")
                .unwrap()
                .value,
            TargetMode::Append
        );
        assert_eq!(
            toml::from_str::<TargetModeValue>("value = \"stdin\"")
                .unwrap()
                .value,
            TargetMode::Stdin
        );
        assert_eq!(
            toml::from_str::<TargetModeValue>("value = \"none\"")
                .unwrap()
                .value,
            TargetMode::None
        );
        assert!(toml::from_str::<TargetModeValue>("value = \"file\"").is_err());
    }

    #[test]
    fn malformed_lint_lang_values_return_existing_config_parse_failure_path_from_tower_config_loading()
     {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[lint.rust]
command = 42
extensions = ["rs"]
format = "rustc-json"
target = "append"
"#,
        );

        let err =
            load(tmp.path()).expect_err("malformed lint config must fail startup config load");

        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unknown_keys_under_lint_lang_return_existing_config_parse_failure_path_instead_of_being_ignored()
     {
        let tmp = tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"
[lint.rust]
command = "cargo"
extensions = ["rs"]
format = "rustc-json"
target = "append"
unknown_key = true
"#,
        );

        let err = load(tmp.path()).expect_err("unknown lint key must fail startup config load");

        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn command_for_extension_strips_optional_leading_dot_matches_extensions_deterministically_and_returns_none_for_unmatched_extensions()
     {
        let cfg = parse_tower_config(
            r#"
[lint.javascript]
command = "eslint"
extensions = ["js", "jsx"]
format = "eslint-json"
target = "append"

[lint.rust]
command = "cargo"
extensions = ["rs"]
format = "rustc-json"
target = "append"

[lint.typescript]
command = "tsc"
extensions = ["js", "ts"]
format = "generic-regex"
target = "none"
"#,
        )
        .expect("lint config must parse");

        let (lang, command) = cfg
            .lint
            .command_for_extension(".rs")
            .expect("leading dot must be stripped before matching");
        assert_eq!(lang, "rust");
        assert_eq!(command.command, "cargo");

        let (lang, command) = cfg
            .lint
            .command_for_extension("js")
            .expect("shared extension must resolve deterministically");
        assert_eq!(lang, "javascript");
        assert_eq!(command.command, "eslint");

        assert!(cfg.lint.command_for_extension("py").is_none());
    }

    #[test]
    fn command_for_path_derives_final_path_extension_delegates_to_command_for_extension_and_returns_none_for_paths_without_matching_extension()
     {
        let cfg = parse_tower_config(
            r#"
[lint.rust]
command = "cargo"
extensions = ["rs"]
format = "rustc-json"
target = "append"
"#,
        )
        .expect("lint config must parse");

        let (lang, command) = cfg
            .lint
            .command_for_path("crates/core_engine/src/lib.rs")
            .expect("final path extension must resolve");
        assert_eq!(lang, "rust");
        assert_eq!(command.command, "cargo");

        assert!(
            cfg.lint
                .command_for_path("crates/core_engine/src/lib")
                .is_none()
        );
        assert!(cfg.lint.command_for_path("README.md").is_none());
    }
}
