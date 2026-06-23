//! `tower` binary — MCP stdio server (spec 10b, closes Milestone 3).
//!
//! # Startup sequence
//!
//! 1. Resolve workspace root: `--workspace-dir <path>` flag or `$TOWER_WORKSPACE`
//!    env var, falling back to the current working directory.
//! 2. Open the sled database from `<root>/.tower/db`.
//!    `SledStorageAdapter::open` reconstructs the [`ProjectWorkspace`] and
//!    [`InvertedIndex`] from persisted state (spec 04a/04b).
//! 3. Run the initial workspace scan if one has never completed
//!    (`StoragePort::is_scan_complete`).
//! 4. Wrap all state in an `Arc<RwLock<EngineState>>` for deadlock-free sharing
//!    with any future background watcher thread (spec 06 lock discipline).
//! 5. Discover and load sidecar extensions (spec 25/28): resolve the ordered
//!    extension dirs (`--extensions-dir` / `$TOWER_EXTENSIONS_DIR` override or
//!    XDG global then workspace-local, local wins on name collision), spawn each
//!    extension binary via `SidecarHostAdapter`, register survivors. A missing or
//!    empty scope yields no extensions; a single bad extension is skipped with a
//!    stderr warning and never aborts startup.
//! 6. Serve the native `tower_*` tools PLUS any extension tools (namespaced
//!    `tower_<ext>_<tool>`) over real `stdin` / `stdout` via an
//!    `ExtensionMergedRegistry`.
//!
//! # Wiring decision: `Arc<RwLock<EngineState>>`
//!
//! The spec requires that tool handlers and the filesystem watcher (spec 06)
//! share workspace/index/storage/fs without copying. `Arc` provides shared
//! ownership; `RwLock` allows concurrent readers (e.g. simultaneous
//! `tower_find_file` and `tower_search_text`) with exclusive mutation for writers
//! (create/delete/global_replace). Short critical sections only — no blocking
//! I/O is performed while holding the lock.
//!
//! # Error handling
//!
//! Startup failures print a human-readable message to stderr and exit with
//! code 1. The serve loop only returns on unrecoverable I/O (broken pipe);
//! malformed frames and tool errors are returned as JSON-RPC error responses
//! and the loop continues.

use clap::Parser;
use core_engine::adapters::cli::{Cli, Command, resolve_workspace_root};
use core_engine::adapters::config;
use core_engine::adapters::fs::scan::{init_towerignore, warn_if_towerignore_absent};

fn main() {
    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Mcp) {
        Command::Init => run_init(&cli.global_opts),
        Command::Mcp => core_engine::adapters::daemon::client::run_mcp_client(&cli.global_opts),
        Command::Daemon { detach } => {
            let workspace_root = resolve_workspace_root(&cli.global_opts);
            let cfg_result = config::load(&workspace_root).map_err(|e| e.to_string());
            match cfg_result {
                Err(e) => Err(e),
                Ok(mut cfg) => {
                    for warning in config::apply_backcompat(&mut cfg) {
                        eprintln!("{warning}");
                    }
                    warn_if_towerignore_absent(&workspace_root);
                    core_engine::adapters::daemon::server::run_daemon(&cli.global_opts, cfg, detach)
                }
            }
        }
        Command::Status => {
            use core_engine::adapters::daemon::wire::{ControlRequest, ControlResponse};
            match core_engine::adapters::daemon::client::send_control(
                &cli.global_opts,
                ControlRequest::Status,
            ) {
                Ok(ControlResponse::Status(s)) => {
                    println!(
                        "tower daemon: up {}s, {} client(s), {} file(s) indexed, {} extension tool(s)",
                        s.uptime_secs,
                        s.mcp_clients,
                        s.indexed_files,
                        s.extensions.len()
                    );
                    Ok(())
                }
                Ok(_) => Err("unexpected control response".to_string()),
                Err(e) => Err(e),
            }
        }
        Command::Shutdown => {
            use core_engine::adapters::daemon::wire::ControlRequest;
            match core_engine::adapters::daemon::client::send_control(
                &cli.global_opts,
                ControlRequest::Shutdown,
            ) {
                Ok(_) => {
                    println!("tower daemon: shutdown requested");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    };
    if let Err(e) = result {
        eprintln!("tower: {e}");
        std::process::exit(1);
    }
}

/// Handle `tower init`: scaffold a default `.towerignore` and `.tower/config.toml`
/// at the workspace root.
///
/// tower's file walker is authoritative and independent of git: it consults only
/// `.towerignore` (never `.gitignore`). `tower init` writes a sensible default.
/// It refuses to overwrite an existing `.towerignore` (returns an error; the
/// caller exits non-zero) so user edits are never clobbered. The `config.toml`
/// seed (default formatter tools) is best-effort: a pre-existing config is left
/// untouched rather than failing the command.
fn run_init(opts: &core_engine::adapters::cli::GlobalOpts) -> Result<(), String> {
    let root = resolve_workspace_root(opts);
    let ignore = match init_towerignore(&root) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(format!("{e}")),
        Err(e) => return Err(format!("failed to write .towerignore: {e}")),
    };
    println!("created {}", ignore.display());

    // Seed `.tower/config.toml` with default formatter tools. A pre-existing
    // config is left untouched (note, not an error) so re-running `tower init`
    // after a `.towerignore` was removed still behaves predictably.
    match config::init_config(&root) {
        Ok(path) => println!("created {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => println!("note: {e}"),
        Err(e) => return Err(format!("failed to write .tower/config.toml: {e}")),
    }
    Ok(())
}
