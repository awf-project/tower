#![allow(clippy::pedantic)]
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use core_engine::adapters::extension::HostDeps;
use core_engine::adapters::formatter::NoOpFormatQueue;
use core_engine::adapters::{InMemoryAstIndex, RealFs};
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{ExtensionManifest, ToolDecl};

pub const SIMPLE_GENERIC_REGEX: &str =
    r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";
pub const SEVERITY_CODE_GENERIC_REGEX: &str = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<severity>warning|error|info|hint): (?P<code>[^:]+): (?P<message>.+)";

pub struct TestWorkspace {
    temp: tempfile::TempDir,
}

impl TestWorkspace {
    pub fn new() -> Self {
        Self {
            temp: tempfile::tempdir().expect("create temp workspace"),
        }
    }

    pub fn root(&self) -> &Path {
        self.temp.path()
    }

    pub fn real_fs(&self) -> RealFs {
        RealFs::new(self.root())
    }

    pub fn write_file(&self, path: &str, content: &str) {
        let path = self.root().join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, content).expect("write workspace file");
    }

    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root().join(name);
        fs::write(&path, body).expect("write lint script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&path).expect("script metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("chmod lint script");
        }

        path
    }

    pub fn write_lint_config(&self, command: &Path) {
        self.write_lint_config_with_regex(command, &["txt"], SIMPLE_GENERIC_REGEX);
    }

    pub fn write_lint_config_for_extensions(&self, command: &Path, extensions: &[&str]) {
        self.write_lint_config_with_regex(command, extensions, SIMPLE_GENERIC_REGEX);
    }

    pub fn write_lint_config_with_regex(&self, command: &Path, extensions: &[&str], regex: &str) {
        let tower_dir = self.root().join(".tower");
        fs::create_dir_all(&tower_dir).expect("create .tower");
        let command = command.to_string_lossy().replace('\\', "\\\\");
        let extensions = extensions
            .iter()
            .map(|extension| format!(r#""{extension}""#))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            tower_dir.join("config.toml"),
            format!(
                r#"
[lint.fixture]
command = "{command}"
extensions = [{extensions}]
format = "generic-regex"
target = "append"
regex = '{regex}'
source = "fixture-lint"
"#
            ),
        )
        .expect("write lint config");
    }

    pub fn write_invalid_generic_lint_config_without_regex(&self, command: &Path) {
        let tower_dir = self.root().join(".tower");
        fs::create_dir_all(&tower_dir).expect("create .tower");
        let command = command.to_string_lossy().replace('\\', "\\\\");
        fs::write(
            tower_dir.join("config.toml"),
            format!(
                r#"
[lint.fixture]
command = "{command}"
extensions = ["txt"]
format = "generic-regex"
target = "append"
source = "fixture-lint"
"#
            ),
        )
        .expect("write invalid lint config");
    }
}

impl Default for TestWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

pub fn workspace_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

pub fn lint_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("lint_extension")
        .to_string_lossy()
        .into_owned()
}

pub fn lint_manifest(bin: &str, tools: Vec<ToolDecl>) -> ExtensionManifest {
    ExtensionManifest {
        name: "lint".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Eager,
        tools,
        events: EventsSection::default(),
        capabilities: CapabilitiesSection {
            required: vec![
                "read_file".to_owned(),
                "list_files".to_owned(),
                "log".to_owned(),
            ],
        },
    }
}

pub fn lint_empty_manifest(bin: &str) -> ExtensionManifest {
    lint_manifest(bin, Vec::new())
}

pub fn lint_check_manifest(bin: &str) -> ExtensionManifest {
    lint_manifest(
        bin,
        vec![ToolDecl {
            name: "check".to_owned(),
            description: "Run configured lint commands for one file or the indexed workspace."
                .to_owned(),
            schema_json: r#"{"type":"object","properties":{"path":{"type":"string"}}}"#.to_owned(),
        }],
    )
}

pub fn host_deps(fs: RealFs) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        push_tx: None,
    }
}
