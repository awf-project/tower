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
use std::time::Duration;

use serde::Deserialize;

/// One language server entry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
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
/// # Note on integration with `load()`
///
/// `TowerConfig` (used by `load()`) uses `#[serde(deny_unknown_fields)]`, which
/// means a real `.tower/config.toml` that contains an `[lsp]` table will cause
/// `load()` to fail until the wiring task adds `lsp: LspConfig` to `TowerConfig`.
/// This function is the standalone testable entry point; wiring is Task 8's concern.
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
