#![allow(clippy::pedantic)]
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use core_engine::adapters::extension::HostDeps;
use core_engine::adapters::extension::SidecarHostAdapter;
use core_engine::adapters::extension::host_deps::{ApplyEditsHostPort, FsAdapter};
use core_engine::adapters::formatter::NoOpFormatQueue;
use core_engine::adapters::mcp::diagnostics::NoOpDiagnosticsReader;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::lsp_tools::SubscriptionRegistry;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::rmcp_server::TowerMcpHandler;
use core_engine::adapters::{InMemoryAstIndex, RealFs};
use core_engine::adapters::{InMemoryStorage, config};
use core_engine::domain::extension_host::ExtensionRegistry;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::mutation::compute_content_version;
use core_engine::domain::token::tokenize;
use core_engine::domain::virtual_file::FileMetadata;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::domain::{DomainError, RelativePath};
use core_engine::ports::FileSystemPort;
use core_engine::ports::inbound::{
    ApplyEditsRequest, PerFileEditResult, TextEdit, WorkspaceApplyEditsError,
    WorkspaceApplyEditsErrorCode, WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult,
    WorkspaceEditSpan,
};
use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{ExtensionManifest, ToolDecl};
use rmcp::{RoleClient, ServiceExt};

pub const SIMPLE_GENERIC_REGEX: &str =
    r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";
pub const SEVERITY_CODE_GENERIC_REGEX: &str = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<severity>warning|error|info|hint): (?P<code>[^:]+): (?P<message>.+)";

#[derive(Clone, Default)]
pub struct HostCallLog(Arc<Mutex<Vec<String>>>);

impl HostCallLog {
    fn record(&self, method: &str, params: serde_json::Value) {
        self.0
            .lock()
            .expect("HostCall log mutex")
            .push(serde_json::json!({ "method": method, "params": params }).to_string());
    }

    pub fn joined(&self) -> String {
        self.0.lock().expect("HostCall log mutex").join("\n")
    }
}

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

fn recording_host_deps(fs: RealFs, log: HostCallLog) -> HostDeps {
    let fs = Arc::new(RecordingFs {
        inner: Arc::new(Mutex::new(fs)),
        log: log.clone(),
    });
    HostDeps {
        fs: fs.clone(),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(RecordingApplyEditsHost {
            inner: FixtureApplyEditsHost {
                fs: fs.inner.clone(),
            },
            log,
        }),
        push_tx: None,
    }
}

struct RecordingFs {
    inner: Arc<Mutex<RealFs>>,
    log: HostCallLog,
}

impl FsAdapter for RecordingFs {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.log
            .record("workspace/readFile", serde_json::json!({ "path": path }));
        self.inner.read_file(path)
    }

    fn list_files(&self) -> Vec<String> {
        self.log
            .record("workspace/listFiles", serde_json::json!({}));
        self.inner.list_files()
    }

    fn workspace_root(&self) -> Option<PathBuf> {
        self.inner.workspace_root()
    }
}

struct RecordingApplyEditsHost {
    inner: FixtureApplyEditsHost,
    log: HostCallLog,
}

impl ApplyEditsHostPort for RecordingApplyEditsHost {
    fn apply_batch_edits(
        &self,
        request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        self.log.record(
            "workspace/applyEdits",
            serde_json::to_value(&request).expect("serialize HostCall request"),
        );
        self.inner.apply_batch_edits(request)
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
    fn apply_batch_edits(
        &self,
        request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        if request.edits.is_empty() {
            return Ok(WorkspaceApplyEditsResult {
                files_changed: 0,
                per_file: vec![PerFileEditResult {
                    path: RelativePath::new(""),
                    applied: false,
                    edits_applied: 0,
                    edits_skipped: 0,
                    new_version: None,
                    preview: None,
                    error: Some(WorkspaceApplyEditsError {
                        code: WorkspaceApplyEditsErrorCode::EmptyEdits,
                        message: "workspace/applyEdits requires at least one edit".to_owned(),
                        path: None,
                    }),
                }],
            });
        }

        let dry_run = request.dry_run.unwrap_or(false);
        let mut groups: BTreeMap<RelativePath, Vec<WorkspaceEditSpan>> = BTreeMap::new();
        for edit in request.edits {
            groups.entry(edit.path.clone()).or_default().push(edit);
        }

        let mut files_changed = 0;
        let mut per_file = Vec::new();
        for (path, spans) in groups {
            let expected_version = match spans.iter().find_map(|span| span.base_hash.clone()) {
                Some(hash) => hash,
                None => {
                    let bytes = self
                        .fs
                        .read_file(path.as_str())
                        .map_err(DomainError::IoError)?;
                    compute_content_version(&bytes)
                }
            };
            let edits = spans
                .iter()
                .map(|span| TextEdit {
                    start_byte: span.start_byte,
                    end_byte: span.end_byte,
                    replacement: span.replacement.clone(),
                })
                .collect::<Vec<_>>();
            let single = ApplyEditsRequest {
                path: path.clone(),
                expected_version,
                edits,
            };
            let content = self.plan(&single)?;

            if dry_run {
                per_file.push(PerFileEditResult {
                    path,
                    applied: true,
                    edits_applied: single.edits.len(),
                    edits_skipped: 0,
                    new_version: None,
                    preview: Some(content),
                    error: None,
                });
                continue;
            }

            let new_version = compute_content_version(content.as_bytes());
            let mut fs = self
                .fs
                .lock()
                .map_err(|e| DomainError::IoError(format!("fs mutex poisoned: {e}")))?;
            core_engine::ports::FileSystemPort::write(&mut *fs, path.clone(), content.into_bytes())
                .map_err(|e| DomainError::IoError(e.to_string()))?;
            files_changed += 1;
            per_file.push(PerFileEditResult {
                path,
                applied: true,
                edits_applied: single.edits.len(),
                edits_skipped: 0,
                new_version: Some(new_version),
                preview: None,
                error: None,
            });
        }

        Ok(WorkspaceApplyEditsResult {
            files_changed,
            per_file,
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

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

fn engine_state(workspace: &TestWorkspace, indexed_paths: &[&str]) -> Arc<RwLock<EngineState>> {
    let mut project = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut fs = workspace.real_fs();
    let storage = InMemoryStorage::new();

    for path in indexed_paths {
        let relative_path = RelativePath::new(*path);
        let id = project
            .insert(relative_path.clone(), FileMetadata::default())
            .expect("index workspace file");
        index.insert(id, &tokenize(relative_path.as_str()));
        let content = fs
            .read(&relative_path)
            .expect("indexed file must exist on disk");
        fs.write(relative_path, content)
            .expect("write indexed file");
    }

    Arc::new(RwLock::new(EngineState::new(
        project,
        index,
        Box::new(storage),
        Box::new(fs),
    )))
}

pub fn handler_with_timeout(
    workspace: &TestWorkspace,
    indexed_paths: &[&str],
    request_timeout: Duration,
) -> TowerMcpHandler {
    let mut extension_registry = ExtensionRegistry::new();
    let lint_extension = SidecarHostAdapter::spawn(
        lint_fix_manifest(&lint_extension_bin()),
        host_deps(workspace.real_fs()),
        request_timeout,
        None,
    )
    .expect("spawn lint extension");
    extension_registry
        .register(lint_extension)
        .expect("register lint extension");

    let merged = ExtensionMergedRegistry::new(
        engine_state(workspace, indexed_paths),
        Arc::new(RwLock::new(extension_registry)),
    );
    TowerMcpHandler::new(
        merged,
        Arc::new(NoOpDiagnosticsReader),
        Arc::new(Mutex::new(SubscriptionRegistry::new())),
        vec![],
    )
}

pub fn handler_with_timeout_and_host_call_log(
    workspace: &TestWorkspace,
    indexed_paths: &[&str],
    request_timeout: Duration,
    log: HostCallLog,
) -> TowerMcpHandler {
    let mut extension_registry = ExtensionRegistry::new();
    let lint_extension = SidecarHostAdapter::spawn(
        lint_fix_manifest(&lint_extension_bin()),
        recording_host_deps(workspace.real_fs(), log),
        request_timeout,
        None,
    )
    .expect("spawn lint extension");
    extension_registry
        .register(lint_extension)
        .expect("register lint extension");

    let merged = ExtensionMergedRegistry::new(
        engine_state(workspace, indexed_paths),
        Arc::new(RwLock::new(extension_registry)),
    );
    TowerMcpHandler::new(
        merged,
        Arc::new(NoOpDiagnosticsReader),
        Arc::new(Mutex::new(SubscriptionRegistry::new())),
        vec![],
    )
}

pub fn handler(workspace: &TestWorkspace, indexed_paths: &[&str]) -> TowerMcpHandler {
    handler_with_timeout(workspace, indexed_paths, TEST_TIMEOUT)
}

pub fn configure_lint_timeout_and_host_budget(
    workspace: &TestWorkspace,
    timeout_secs: Option<u64>,
) -> Duration {
    let path = workspace.root().join(".tower/config.toml");
    let mut contents = std::fs::read_to_string(&path).expect("read lint config");
    if let Some(timeout_secs) = timeout_secs {
        contents.push_str(&format!("timeout_secs = {timeout_secs}\n"));
    }
    contents.push_str("\n[extensions]\nrequest_timeout_secs = 1\n");
    std::fs::write(&path, contents).expect("write lint and host timeouts");

    config::load(workspace.root())
        .expect("load lint config")
        .lint_request_timeout()
        .expect("derive lint request timeout")
        .expect("fixture configures a lint timeout")
}

pub async fn start_server(
    handler: TowerMcpHandler,
) -> rmcp::service::RunningService<RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    tokio::spawn(async move {
        if let Ok(server) = handler.serve(server_transport).await {
            let _ = server.waiting().await;
        }
    });
    ().serve(client_transport)
        .await
        .expect("client connect failed")
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}
