//! Unit tests for the clap CLI surface (Task 1).
// Feature: F009
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Output};

use clap::Parser;
use core_engine::adapters::cli::{Cli, Command, resolve_workspace_root};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("args parse")
}

fn run_init(workspace: &Path) -> Output {
    ProcessCommand::new(env!("CARGO_BIN_EXE_tower"))
        .arg("init")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .output()
        .expect("run tower init")
}

fn generated_files(root: &Path) -> Vec<PathBuf> {
    fn visit(root: &Path, current: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort();
    files
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

#[test]
fn init_preserves_all_config_and_ignore_existence_combinations() {
    let cases = [
        (false, false, true),
        (true, false, true),
        (false, true, false),
        (true, true, false),
    ];

    for (config_exists, ignore_exists, succeeds) in cases {
        let workspace = tempfile::tempdir().unwrap();
        let config_path = workspace.path().join(".tower/config.toml");
        let ignore_path = workspace.path().join(".towerignore");
        let config_bytes = b"custom-config-bytes\n";
        let ignore_bytes = b"custom-ignore-bytes\n";

        if config_exists {
            std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            std::fs::write(&config_path, config_bytes).unwrap();
        }
        if ignore_exists {
            std::fs::write(&ignore_path, ignore_bytes).unwrap();
        }

        let output = run_init(workspace.path());
        assert_eq!(
            output.status.success(),
            succeeds,
            "config_exists={config_exists}, ignore_exists={ignore_exists}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if succeeds {
            assert!(stderr.is_empty(), "successful init stderr: {stderr}");
            assert!(
                stdout.contains("created"),
                "successful init stdout: {stdout}"
            );
            if config_exists {
                assert!(stdout.contains("note:"), "existing config stdout: {stdout}");
            }
        } else {
            assert!(stdout.is_empty(), "refused init stdout: {stdout}");
            assert!(
                stderr.contains(".towerignore") && stderr.contains("refusing to overwrite"),
                "refused init stderr: {stderr}"
            );
        }

        if config_exists {
            assert_eq!(std::fs::read(&config_path).unwrap(), config_bytes);
        } else if ignore_exists {
            assert!(
                !config_path.exists(),
                "ignore refusal must precede config creation"
            );
        } else {
            assert!(config_path.is_file());
        }
        if ignore_exists {
            assert_eq!(std::fs::read(&ignore_path).unwrap(), ignore_bytes);
        } else {
            assert!(ignore_path.is_file());
        }

        let expected = if config_exists || !ignore_exists {
            vec![
                PathBuf::from(".tower/config.toml"),
                PathBuf::from(".towerignore"),
            ]
        } else {
            vec![PathBuf::from(".towerignore")]
        };
        assert_eq!(generated_files(workspace.path()), expected);
    }
}
