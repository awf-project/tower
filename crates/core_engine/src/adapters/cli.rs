//! Binary CLI surface (clap, derive).
//!
//! Lives in `adapters/` (infrastructure): it parses process arguments and
//! resolves filesystem paths. The `domain/` crate never imports it.
#![forbid(unsafe_code)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// `tower` command-line interface.
#[derive(Debug, Parser)]
#[command(name = "tower", version, about = "Tower core engine", long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub global_opts: GlobalOpts,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Flags valid on every subcommand. Workspace resolution stays homogeneous.
#[derive(Debug, clap::Args)]
pub struct GlobalOpts {
    /// Workspace root. Overrides `$TOWER_WORKSPACE`; defaults to the cwd.
    #[arg(long, global = true, value_name = "PATH")]
    pub workspace_dir: Option<PathBuf>,

    /// Extension search dir. Overrides `$TOWER_EXTENSIONS_DIR`.
    #[arg(long, global = true, value_name = "PATH")]
    pub extensions_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run an MCP stdio client: connect-or-spawn the daemon and relay stdio.
    Mcp,
    /// Run the daemon in the foreground (auto-spawn target; systemd-friendly).
    Daemon {
        /// Set by the auto-spawn path: detach into a new session (setsid).
        #[arg(long, hide = true)]
        detach: bool,
    },
    /// Scaffold `.towerignore` and `.tower/config.toml`.
    Init,
    /// Print the running daemon's status snapshot.
    Status,
    /// Ask the running daemon to shut down.
    Shutdown,
}

/// Resolve the workspace root: `--workspace-dir` > `$TOWER_WORKSPACE` > cwd.
#[must_use]
pub fn resolve_workspace_root(opts: &GlobalOpts) -> PathBuf {
    if let Some(p) = &opts.workspace_dir {
        return p.clone();
    }
    if let Ok(val) = std::env::var("TOWER_WORKSPACE")
        && !val.is_empty()
    {
        return PathBuf::from(val);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve the `--extensions-dir` override as a string, if present.
/// Falls back (in the caller) to `$TOWER_EXTENSIONS_DIR` then the XDG default.
#[must_use]
pub fn resolve_extensions_dir_arg(opts: &GlobalOpts) -> Option<String> {
    opts.extensions_dir
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
}
