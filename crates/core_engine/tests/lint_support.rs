#![allow(clippy::pedantic)]
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use core_engine::adapters::extension::HostDeps;
use core_engine::adapters::extension::host_deps::{ApplyEditsHostPort, FsAdapter};
use core_engine::adapters::formatter::NoOpFormatQueue;
use core_engine::adapters::{InMemoryAstIndex, RealFs};
use core_engine::domain::mutation::compute_content_version;
use core_engine::domain::{DomainError, RelativePath};
use core_engine::ports::inbound::{
    ApplyEditsFileResult, ApplyEditsPreview, ApplyEditsRequest, TextEdit,
};
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
        self.write_lint_config_with_format(
            command,
            extensions,
            "generic-regex",
            "append",
            Some(regex),
        );
    }

    pub fn write_lint_config_with_format(
        &self,
        command: &Path,
        extensions: &[&str],
        format_name: &str,
        target: &str,
        regex: Option<&str>,
    ) {
        let tower_dir = self.root().join(".tower");
        fs::create_dir_all(&tower_dir).expect("create .tower");
        let command = command.to_string_lossy().replace('\\', "\\\\");
        let extensions = extensions
            .iter()
            .map(|extension| format!(r#""{extension}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let regex = regex
            .map(|regex| format!("regex = '{regex}'\n"))
            .unwrap_or_default();
        fs::write(
            tower_dir.join("config.toml"),
            format!(
                r#"
[lint.fixture]
command = "{command}"
extensions = [{extensions}]
format = "{format_name}"
target = "{target}"
{regex}source = "fixture-lint"
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
                "request_apply_edits".to_owned(),
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

pub fn lint_fix_manifest(bin: &str) -> ExtensionManifest {
    lint_manifest(
        bin,
        vec![
            ToolDecl {
                name: "check".to_owned(),
                description: "Run configured lint commands for one file or the indexed workspace."
                    .to_owned(),
                schema_json: r#"{"type":"object","properties":{"path":{"type":"string"}}}"#.to_owned(),
            },
            ToolDecl {
                name: "fix".to_owned(),
                description: "Apply structured lint fixes for one file or the indexed workspace."
                    .to_owned(),
                schema_json: r#"{"type":"object","properties":{"path":{"type":"string"},"unsafe":{"type":"boolean"},"dry_run":{"type":"boolean"}}}"#.to_owned(),
            },
        ],
    )
}

pub fn host_deps(fs: RealFs) -> HostDeps {
    let fs = Arc::new(Mutex::new(fs));
    HostDeps {
        fs: fs.clone(),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(FixtureApplyEditsHost { fs }),
        push_tx: None,
    }
}

struct FixtureApplyEditsHost {
    fs: Arc<Mutex<RealFs>>,
}

impl FixtureApplyEditsHost {
    fn read_checked(&self, request: &ApplyEditsRequest) -> Result<Vec<u8>, DomainError> {
        let bytes = self
            .fs
            .read_file(request.path.as_str())
            .map_err(|message| {
                if message.contains("not found") {
                    DomainError::NotFound
                } else {
                    DomainError::IoError(message)
                }
            })?;
        let actual = compute_content_version(&bytes);
        if actual != request.expected_version {
            return Err(DomainError::VersionConflict {
                expected: request.expected_version.clone(),
                actual,
            });
        }
        Ok(bytes)
    }

    fn plan(&self, request: &ApplyEditsRequest) -> Result<String, DomainError> {
        let bytes = self.read_checked(request)?;
        let mut content = String::from_utf8(bytes).map_err(|e| {
            DomainError::InvalidRange(format!(
                "{} is not UTF-8 text; apply_edits only edits text files ({e})",
                request.path.as_str()
            ))
        })?;
        let mut edits = request.edits.clone();
        edits.sort_by(|a, b| {
            b.start_byte
                .cmp(&a.start_byte)
                .then_with(|| b.end_byte.cmp(&a.end_byte))
        });
        for edit in edits {
            validate_edit(&request.path, &content, &edit)?;
            content.replace_range(edit.start_byte..edit.end_byte, &edit.replacement);
        }
        Ok(content)
    }
}

impl ApplyEditsHostPort for FixtureApplyEditsHost {
    fn apply_edits_cas(
        &self,
        request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        let content = self.plan(&request)?;
        let new_version = compute_content_version(content.as_bytes());
        let mut fs = self
            .fs
            .lock()
            .map_err(|e| DomainError::IoError(format!("fs mutex poisoned: {e}")))?;
        core_engine::ports::FileSystemPort::write(
            &mut *fs,
            request.path.clone(),
            content.into_bytes(),
        )
        .map_err(|e| DomainError::IoError(e.to_string()))?;
        Ok(ApplyEditsFileResult {
            path: request.path,
            applied: request.edits,
            skipped: Vec::new(),
            new_version: Some(new_version),
            preview: None,
        })
    }

    fn apply_edits_dry_run(
        &self,
        request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        let content = self.plan(&request)?;
        Ok(ApplyEditsFileResult {
            path: request.path.clone(),
            applied: request.edits.clone(),
            skipped: Vec::new(),
            new_version: None,
            preview: Some(ApplyEditsPreview {
                path: request.path,
                edits: request.edits,
                skipped: Vec::new(),
                preview_content: content,
            }),
        })
    }
}

fn validate_edit(path: &RelativePath, content: &str, edit: &TextEdit) -> Result<(), DomainError> {
    if edit.start_byte > edit.end_byte {
        return Err(DomainError::InvalidRange(format!(
            "{} start_byte {} exceeds end_byte {}",
            path.as_str(),
            edit.start_byte,
            edit.end_byte
        )));
    }
    if edit.end_byte > content.len() {
        return Err(DomainError::InvalidRange(format!(
            "{} end_byte {} exceeds file length {}",
            path.as_str(),
            edit.end_byte,
            content.len()
        )));
    }
    if !content.is_char_boundary(edit.start_byte) || !content.is_char_boundary(edit.end_byte) {
        return Err(DomainError::InvalidRange(format!(
            "{} edit range {}..{} is not on UTF-8 boundaries",
            path.as_str(),
            edit.start_byte,
            edit.end_byte
        )));
    }
    Ok(())
}
