//! Load `[lsp.*]` config from `.tower/config.toml` (spec 27 U3).
//!
//! The extension reads the same config format as the host's `adapters/config/lsp`
//! module. Absent file → empty config (no servers). Malformed file → empty config
//! with a stderr warning (the extension stays up but cannot serve any language).

#![forbid(unsafe_code)]

use std::path::Path;

use core_engine::adapters::config::lsp::LspConfig;

/// Load `LspConfig` from `<workspace>/.tower/config.toml`.
///
/// Returns an empty config on any I/O or parse error (logs to stderr).
pub fn load_lsp_config(workspace_root: &Path) -> LspConfig {
    let config_path = workspace_root.join(".tower").join("config.toml");
    let src = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LspConfig::default();
        }
        Err(e) => {
            eprintln!("[tower/lsp-ext] config read error: {e}");
            return LspConfig::default();
        }
    };
    match core_engine::adapters::config::lsp::parse_lsp_config(&src) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[tower/lsp-ext] config parse error: {e}");
            LspConfig::default()
        }
    }
}
