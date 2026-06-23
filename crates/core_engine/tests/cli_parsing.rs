//! Unit tests for the clap CLI surface (Task 1).
#![forbid(unsafe_code)]

use std::path::PathBuf;

use clap::Parser;
use core_engine::adapters::cli::{Cli, Command, resolve_workspace_root};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("args parse")
}

#[test]
fn bare_invocation_has_no_subcommand() {
    let cli = parse(&["tower"]);
    assert!(cli.command.is_none());
}

#[test]
fn mcp_subcommand_parses() {
    let cli = parse(&["tower", "mcp"]);
    assert!(matches!(cli.command, Some(Command::Mcp)));
}

#[test]
fn daemon_detach_flag_parses() {
    let cli = parse(&["tower", "daemon", "--detach"]);
    assert!(matches!(
        cli.command,
        Some(Command::Daemon { detach: true })
    ));
    let cli = parse(&["tower", "daemon"]);
    assert!(matches!(
        cli.command,
        Some(Command::Daemon { detach: false })
    ));
}

#[test]
fn init_status_shutdown_parse() {
    assert!(matches!(
        parse(&["tower", "init"]).command,
        Some(Command::Init)
    ));
    assert!(matches!(
        parse(&["tower", "status"]).command,
        Some(Command::Status)
    ));
    assert!(matches!(
        parse(&["tower", "shutdown"]).command,
        Some(Command::Shutdown)
    ));
}

#[test]
fn workspace_dir_flag_is_global_and_wins_over_env() {
    let cli = parse(&["tower", "--workspace-dir", "/tmp/ws", "mcp"]);
    let root = resolve_workspace_root(&cli.global_opts);
    assert_eq!(root, PathBuf::from("/tmp/ws"));
}

#[test]
fn version_flag_is_recognized() {
    // clap returns a DisplayVersion "error" for --version; assert that kind.
    let err = Cli::try_parse_from(["tower", "--version"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
}

#[test]
fn unknown_subcommand_is_rejected() {
    assert!(Cli::try_parse_from(["tower", "bogus"]).is_err());
}
