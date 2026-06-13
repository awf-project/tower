//! Local project configuration: `<workspace>/.tower/config.toml`.
//!
//! Infrastructure concern (reads `std::fs`, parses `toml`). Lives in `adapters/`;
//! the domain never imports it. First feature: disabling plugins by file stem.
//!
//! Absent file → `TowerConfig::default()` (silent). Present but unreadable or
//! invalid (bad TOML / unknown key) → `Err` → the binary fails fast (exit 1).

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
}

/// `[plugins]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    /// Plugin file stems (no `.wasm`) to skip loading, e.g. `["ast"]`.
    #[serde(default)]
    pub disabled: Vec<String>,
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
}
