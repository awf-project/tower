#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use core_engine::adapters::extension::discovery::read_manifests_from_dir;

const CURRENT_BINARY: &[u8] = b"current lint extension\n";
const STALE_MANIFEST: &[u8] = b"stale manifest\n";
const STALE_BINARY: &[u8] = b"stale binary\n";

struct MakeInstallFixture {
    sandbox: TempDir,
    working_tree: PathBuf,
    bin_dir: PathBuf,
    daemon_marker: PathBuf,
    manifest: Vec<u8>,
}

impl MakeInstallFixture {
    fn new() -> Self {
        let sandbox = tempfile::tempdir().expect("create installation sandbox");
        let working_tree = sandbox.path().join("working tree");
        let bin_dir = sandbox.path().join("fixture bin");
        let daemon_marker = sandbox.path().join("tower-invocations");
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("core_engine crate belongs to the workspace");
        let manifest = fs::read(repo_root.join("extensions/lint/extension.toml"))
            .expect("read repository lint manifest");

        fs::create_dir_all(working_tree.join("extensions/lint"))
            .expect("create fixture extension directory");
        fs::create_dir_all(&bin_dir).expect("create fixture binary directory");
        fs::copy(repo_root.join("Makefile"), working_tree.join("Makefile"))
            .expect("copy repository Makefile");
        fs::write(
            working_tree.join("extensions/lint/extension.toml"),
            &manifest,
        )
        .expect("copy lint manifest");

        write_executable(
            &bin_dir.join("cargo"),
            "#!/bin/sh\nset -eu\nmkdir -p target/release\nprintf 'current lint extension\\n' > target/release/lint_extension\n",
        );
        write_executable(
            &bin_dir.join("tower"),
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> \"$TOWER_SENTINEL_MARKER\"\nexit 97\n",
        );

        let fixture = Self {
            sandbox,
            working_tree,
            bin_dir,
            daemon_marker,
            manifest,
        };
        fixture.verify_tower_sentinel();
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.sandbox.path().join(name)
    }

    fn local_destination(&self) -> PathBuf {
        self.working_tree.join(".tower/extensions")
    }

    fn install(
        &self,
        home: &Path,
        xdg_data_home: Option<&Path>,
        global: bool,
        explicit_destination: Option<&Path>,
    ) -> Output {
        let mut command = self.make_command(home, xdg_data_home);
        command.arg("EXTENSIONS=lint");
        if global {
            command.arg("GLOBAL=1");
        }
        if let Some(destination) = explicit_destination {
            command.arg(format!("EXT_DEST={}", destination.display()));
        }
        command
            .arg("install-extensions")
            .output()
            .expect("run make install-extensions")
    }

    fn help(&self, home: &Path) -> Output {
        self.make_command(home, None)
            .arg("help")
            .output()
            .expect("run make help")
    }

    fn make_command(&self, home: &Path, xdg_data_home: Option<&Path>) -> Command {
        let mut command = Command::new("make");
        command
            .current_dir(&self.working_tree)
            .env_clear()
            .env("HOME", home)
            .env("PATH", self.fixture_path())
            .env("TOWER_SENTINEL_MARKER", &self.daemon_marker);
        if let Some(path) = xdg_data_home {
            command.env("XDG_DATA_HOME", path);
        }
        command
    }

    fn fixture_path(&self) -> std::ffi::OsString {
        std::env::join_paths([
            self.bin_dir.clone(),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .expect("construct fixture PATH")
    }

    fn verify_tower_sentinel(&self) {
        let status = Command::new(self.bin_dir.join("tower"))
            .arg("harmless-fixture-probe")
            .env("TOWER_SENTINEL_MARKER", &self.daemon_marker)
            .status()
            .expect("invoke tower sentinel directly");
        assert_eq!(status.code(), Some(97));
        assert_eq!(
            fs::read_to_string(&self.daemon_marker).expect("read tower marker"),
            "harmless-fixture-probe\n"
        );
        fs::remove_file(&self.daemon_marker).expect("clear tower setup marker");
    }

    fn seed_stale_extension(&self, destination: &Path) {
        let extension_dir = destination.join("lint");
        fs::create_dir_all(&extension_dir).expect("create stale extension directory");
        fs::write(extension_dir.join("extension.toml"), STALE_MANIFEST)
            .expect("seed stale manifest");
        fs::write(extension_dir.join("lint_extension"), STALE_BINARY).expect("seed stale binary");
    }

    fn assert_installed(&self, destination: &Path) {
        let extension_dir = destination.join("lint");
        assert_eq!(
            fs::read(extension_dir.join("extension.toml")).expect("read installed manifest"),
            self.manifest
        );
        let binary = extension_dir.join("lint_extension");
        assert_eq!(
            fs::read(&binary).expect("read installed binary"),
            CURRENT_BINARY
        );
        assert_ne!(
            fs::metadata(binary)
                .expect("inspect installed binary")
                .permissions()
                .mode()
                & 0o111,
            0,
            "installed extension binary must be executable"
        );
    }

    fn assert_destination_empty(&self, destination: &Path) {
        if destination.exists() {
            assert_eq!(
                fs::read_dir(destination)
                    .expect("read competing destination")
                    .count(),
                0,
                "competing destination {} must remain empty",
                destination.display()
            );
        }
    }

    fn assert_daemon_untouched(&self) {
        assert!(
            !self.daemon_marker.exists(),
            "install-extensions invoked tower: {}",
            fs::read_to_string(&self.daemon_marker).unwrap_or_default()
        );
    }
}

fn write_executable(path: &Path, content: &str) {
    // Open the writable executable only in a child process. Otherwise a
    // concurrent test's fork can inherit that descriptor until exec, causing
    // ETXTBSY when this test launches its newly written script.
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg("printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"")
        .arg("write-fixture-executable")
        .arg(path)
        .arg(content)
        .status()
        .expect("spawn fixture executable writer");
    assert!(status.success(), "write fixture executable: {status}");
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

struct InstallationCase {
    name: &'static str,
    global: bool,
    use_xdg: bool,
    explicit: bool,
}

fn run_installation_case(case: InstallationCase) {
    let fixture = MakeInstallFixture::new();
    let home = fixture.path("home");
    let xdg = fixture.path("xdg data");
    let explicit = fixture.path("explicit destination");
    let local = fixture.local_destination();
    let home_global = home.join(".local/share/tower/extensions");
    let xdg_global = xdg.join("tower/extensions");
    let effective = if case.explicit {
        &explicit
    } else if !case.global {
        &local
    } else if case.use_xdg {
        &xdg_global
    } else {
        &home_global
    };
    let reported_destination = if case.global || case.explicit {
        effective.display().to_string()
    } else {
        ".tower/extensions".to_owned()
    };
    let competitors = [&local, &xdg_global, &home_global, &explicit];
    fixture.seed_stale_extension(effective);

    let output = fixture.install(
        &home,
        case.use_xdg.then_some(xdg.as_path()),
        case.global,
        case.explicit.then_some(explicit.as_path()),
    );

    assert_success(&output, case.name);
    assert!(
        output.stderr.is_empty(),
        "{} wrote stderr:\n{}",
        case.name,
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_installed(effective);
    for destination in competitors.iter().filter(|path| *path != &effective) {
        fixture.assert_destination_empty(destination);
    }
    fixture.assert_daemon_untouched();

    let stdout = String::from_utf8(output.stdout).expect("installation stdout is UTF-8");
    let lines: Vec<_> = stdout.lines().collect();
    let copy_line = lines
        .iter()
        .position(|line| line.contains("installed extension 'lint'"))
        .expect("installation reports the lint copy");
    assert!(
        lines[..copy_line]
            .iter()
            .any(|line| *line == format!("installing extensions into {reported_destination}")),
        "{} must print effective root {} before copying; stdout:\n{}",
        case.name,
        reported_destination,
        stdout
    );
    assert!(
        lines[copy_line + 1..].iter().any(|line| {
            line.contains("reload the relevant shared daemon")
                && line.contains("reconnect its clients")
        }),
        "installation must report reload and reconnect guidance after copying; stdout:\n{stdout}"
    );
}

#[test]
fn extension_installation_populates_and_reports_effective_scope() {
    for case in [
        InstallationCase {
            name: "local default",
            global: false,
            use_xdg: true,
            explicit: false,
        },
        InstallationCase {
            name: "XDG global",
            global: true,
            use_xdg: true,
            explicit: false,
        },
        InstallationCase {
            name: "HOME fallback global",
            global: true,
            use_xdg: false,
            explicit: false,
        },
        InstallationCase {
            name: "explicit destination",
            global: true,
            use_xdg: true,
            explicit: true,
        },
    ] {
        run_installation_case(case);
    }
}

#[test]
fn extension_installation_help_describes_global_scope() {
    let fixture = MakeInstallFixture::new();
    let output = fixture.help(&fixture.path("home"));

    assert_success(&output, "make help");
    assert!(output.stderr.is_empty(), "make help must not write stderr");
    fixture.assert_daemon_untouched();
    let help = String::from_utf8(output.stdout).expect("make help output is UTF-8");
    for expected in [
        "default: .tower/extensions",
        "GLOBAL=1",
        "XDG_DATA_HOME/tower/extensions, falling back to HOME/.local/share/tower/extensions",
        "EXT_DEST overrides those defaults",
        "install updates only the server, not extensions",
    ] {
        assert!(
            help.contains(expected),
            "make help must describe {expected:?}; output:\n{help}"
        );
    }
}

#[test]
fn extension_installation_reports_copy_failure() {
    let fixture = MakeInstallFixture::new();
    fs::remove_file(fixture.working_tree.join("extensions/lint/extension.toml"))
        .expect("remove source manifest");

    let output = fixture.install(&fixture.path("home"), None, false, None);

    assert!(
        !output.status.success(),
        "missing manifest must fail installation"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("installed extension 'lint'"));
    assert!(!stdout.contains("reload the relevant shared daemon"));
    assert!(
        !fixture
            .local_destination()
            .join("lint/lint_extension")
            .exists()
    );
    fixture.assert_daemon_untouched();
}

#[test]
fn packaged_extensions_are_workspace_built_shipped_manifest_valid_and_discoverable() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("core_engine crate belongs to the workspace");
    let workspace_manifest =
        fs::read_to_string(repo_root.join("Cargo.toml")).expect("read workspace manifest");
    let workspace: toml::Value =
        toml::from_str(&workspace_manifest).expect("parse workspace manifest");
    let workspace = workspace
        .get("workspace")
        .and_then(toml::Value::as_table)
        .expect("workspace table");
    let values = |key: &str| {
        workspace[key]
            .as_array()
            .expect("workspace member list")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>()
    };

    let packaged = [
        ("semantic_context", "semantic_context_extension"),
        ("agent_memory", "agent_memory_extension"),
    ];
    for (extension, binary) in packaged {
        let member = format!("extensions/{extension}");
        assert!(values("members").contains(&member.as_str()));
        assert!(
            values("default-members").contains(&member.as_str()),
            "the native {extension} sidecar must be built by default workspace commands"
        );
        let package_manifest = fs::read_to_string(repo_root.join(&member).join("Cargo.toml"))
            .unwrap_or_else(|error| panic!("read {extension} package manifest: {error}"));
        let package: toml::Value = toml::from_str(&package_manifest)
            .unwrap_or_else(|error| panic!("parse {extension} package manifest: {error}"));
        assert_eq!(package["package"]["name"].as_str(), Some(binary));
        assert!(repo_root.join(&member).join("src/main.rs").is_file());
    }

    let makefile = fs::read_to_string(repo_root.join("Makefile")).expect("read Makefile");
    assert!(
        makefile.contains("semantic-index-durability-real-disk:"),
        "the durability check needs a dedicated local target"
    );
    assert!(makefile.contains(
        "cargo test -p semantic_context_extension --test semantic_index_benchmark --release -- --ignored --exact semantic_index_durability_on_real_disk"
    ));
    let default_test_recipe = makefile
        .split(".PHONY: test\n")
        .nth(1)
        .and_then(|tail| tail.split(".PHONY:").next())
        .expect("default test recipe");
    assert!(!default_test_recipe.contains("semantic_index_durability_on_real_disk"));
    let gate_recipe = makefile
        .split(".PHONY: gate\n")
        .nth(1)
        .and_then(|tail| tail.split(".PHONY:").next())
        .expect("gate recipe");
    assert!(!gate_recipe.contains("semantic_index_durability_on_real_disk"));
    let default_extensions = makefile
        .lines()
        .find_map(|line| line.strip_prefix("EXTENSIONS   ?= "))
        .expect("default shipped extension list");
    for (extension, _) in packaged {
        assert!(
            default_extensions
                .split_whitespace()
                .any(|item| item == extension),
            "the default shipped extension list includes {extension}"
        );
    }
    let sandbox = tempfile::tempdir().expect("create extension packaging sandbox");
    let working_tree = sandbox.path().join("working tree");
    let bin_dir = sandbox.path().join("bin");
    let destination = sandbox.path().join("extensions");
    fs::create_dir_all(&working_tree).expect("create packaging working tree");
    fs::create_dir_all(&bin_dir).expect("create fixture bin directory");
    fs::copy(repo_root.join("Makefile"), working_tree.join("Makefile"))
        .expect("copy repository Makefile");
    for (extension, _) in packaged {
        let fixture_dir = working_tree.join("extensions").join(extension);
        fs::create_dir_all(&fixture_dir).expect("create packaged extension fixture");
        fs::copy(
            repo_root
                .join("extensions")
                .join(extension)
                .join("extension.toml"),
            fixture_dir.join("extension.toml"),
        )
        .unwrap_or_else(|error| panic!("copy {extension} manifest: {error}"));
    }
    write_executable(
        &bin_dir.join("cargo"),
        "#!/bin/sh\nset -eu\nmkdir -p target/release\nfor binary in semantic_context_extension agent_memory_extension; do\n  printf '#!/bin/sh\\nexit 0\\n' > \"target/release/$binary\"\n  chmod 755 \"target/release/$binary\"\ndone\n",
    );

    let output = Command::new("make")
        .current_dir(&working_tree)
        .env_clear()
        .env("HOME", sandbox.path().join("home"))
        .env(
            "PATH",
            std::env::join_paths([bin_dir, PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
                .expect("construct fixture PATH"),
        )
        .arg("EXTENSIONS=semantic_context agent_memory")
        .arg(format!("EXT_DEST={}", destination.display()))
        .arg("install-extensions")
        .output()
        .expect("install packaged extensions");
    assert_success(&output, "packaged extension installation");

    let mut errors = Vec::new();
    let discovered = read_manifests_from_dir(&destination, &mut errors);
    assert!(errors.is_empty(), "packaged manifest errors: {errors:?}");
    assert_eq!(discovered.len(), packaged.len());
    for (extension, binary) in packaged {
        let extension_dir = destination.join(extension);
        assert!(extension_dir.join("extension.toml").is_file());
        assert!(extension_dir.join(binary).is_file());
        let (_, manifest) = discovered
            .iter()
            .find(|(directory, _)| directory == &extension_dir)
            .unwrap_or_else(|| panic!("discover packaged {extension} manifest"));
        assert_eq!(manifest.name, extension);
        assert_eq!(manifest.command, [binary]);
    }
}
