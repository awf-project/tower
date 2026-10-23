use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const PRINT_TARGET: &str = "__print_extension_destination";

struct MakeFixture {
    sandbox: TempDir,
    repo_root: PathBuf,
    probe_makefile: PathBuf,
}

impl MakeFixture {
    fn new() -> Self {
        let sandbox = tempfile::tempdir().expect("create Make contract sandbox");
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("core_engine crate belongs to the workspace")
            .to_path_buf();
        let probe_makefile = sandbox.path().join("print-extension-destination.mk");

        fs::write(
            &probe_makefile,
            format!(".PHONY: {PRINT_TARGET}\n{PRINT_TARGET}:\n\t@printf '%s\\n' \"$(EXT_DEST)\"\n"),
        )
        .expect("write destination probe Makefile");

        Self {
            sandbox,
            repo_root,
            probe_makefile,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.sandbox.path().join(name)
    }

    fn resolve_destination(
        &self,
        home: &Path,
        xdg_data_home: Option<&Path>,
        global: bool,
        explicit_destination: Option<&Path>,
    ) -> String {
        let mut command = self.make_command(home, xdg_data_home);
        command
            .arg("-f")
            .arg(self.repo_root.join("Makefile"))
            .arg("-f")
            .arg(&self.probe_makefile);

        if global {
            command.arg("GLOBAL=1");
        }
        if let Some(destination) = explicit_destination {
            command.arg(format!("EXT_DEST={}", destination.display()));
        }

        let output = command
            .arg(PRINT_TARGET)
            .output()
            .expect("run Make destination probe");
        assert_success(&output, "destination probe");
        String::from_utf8(output.stdout)
            .expect("Make destination is UTF-8")
            .trim_end_matches(['\r', '\n'])
            .to_owned()
    }

    fn help(&self, home: &Path) -> String {
        let output = self
            .make_command(home, None)
            .arg("help")
            .output()
            .expect("run make help");
        assert_success(&output, "make help");
        String::from_utf8(output.stdout).expect("Make help is UTF-8")
    }

    fn make_command(&self, home: &Path, xdg_data_home: Option<&Path>) -> Command {
        let mut command = Command::new("make");
        command
            .current_dir(&self.repo_root)
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin");

        if let Some(path) = xdg_data_home {
            command.env("XDG_DATA_HOME", path);
        }

        command
    }
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn extension_destination_defaults_to_workspace_scope() {
    let fixture = MakeFixture::new();
    let home = fixture.path("home");

    let destination = fixture.resolve_destination(&home, None, false, None);

    assert_eq!(destination, ".tower/extensions");

    let help = fixture.help(&home);
    for expected in [
        "install-extensions",
        "default: .tower/extensions",
        "GLOBAL=1 selects XDG_DATA_HOME/tower/extensions, falling back to HOME/.local/share/tower/extensions",
        "EXT_DEST overrides those defaults",
    ] {
        assert!(
            help.contains(expected),
            "make help must describe {expected:?}; output:\n{help}"
        );
    }
}

#[test]
fn global_extension_destination_uses_xdg_data_home() {
    let fixture = MakeFixture::new();
    let home = fixture.path("home");
    let xdg_data_home = fixture.path("xdg-data");

    let destination = fixture.resolve_destination(&home, Some(&xdg_data_home), true, None);

    assert_eq!(
        destination,
        xdg_data_home.join("tower/extensions").display().to_string()
    );
}

#[test]
fn global_extension_destination_falls_back_to_home_local_share() {
    let fixture = MakeFixture::new();
    let home = fixture.path("home");

    let absent_destination = fixture.resolve_destination(&home, None, true, None);
    assert_eq!(
        absent_destination,
        home.join(".local/share/tower/extensions")
            .display()
            .to_string()
    );

    let empty_xdg = Path::new("");
    let empty_destination = fixture.resolve_destination(&home, Some(empty_xdg), true, None);
    assert_eq!(empty_destination, absent_destination);
}

#[test]
fn explicit_extension_destination_overrides_global_selection() {
    let fixture = MakeFixture::new();
    let home = fixture.path("fallback home");
    let xdg_data_home = fixture.path("ignored xdg data");
    let explicit_destination = fixture.path("explicit extension destination with spaces");

    let destination = fixture.resolve_destination(
        &home,
        Some(&xdg_data_home),
        true,
        Some(&explicit_destination),
    );

    assert_eq!(destination, explicit_destination.display().to_string());
}
