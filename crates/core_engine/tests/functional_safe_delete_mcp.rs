#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

// Feature: F012

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::TowerConfig;
use core_engine::adapters::daemon::engine::{EngineHandle, build_engine};
use core_engine::adapters::fs::RealFs;
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::domain::extension_host::{ExtensionId, ExtensionRegistry, InvokeError};
use core_engine::domain::{FileId, RelativePath};
use core_engine::ports::{ExtensionHostPort, FileSystemPort, PortError};
use extension_protocol::{ExtensionFault, ToolDecl};
use serde_json::{Value, json};

const DECLARATION: &str = "pub fn target() {}\n";
const CONSUMER: &str = "fn consumer(){target();}\n";

struct CapturingHost {
    registry: Arc<RwLock<ExtensionRegistry>>,
    changed: Mutex<Vec<String>>,
}

impl CapturingHost {
    fn take_changed(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .changed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }
}

impl ExtensionHostPort for CapturingHost {
    fn on_file_indexed(&self, id: FileId, path: &RelativePath) {
        self.registry
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .on_file_indexed(id, path);
    }

    fn on_file_changed(&self, id: FileId, path: &RelativePath) {
        self.changed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(path.as_str().to_owned());
        self.registry
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .on_file_changed(id, path);
    }

    fn on_file_deleted(&self, path: &RelativePath) {
        self.registry
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .on_file_deleted(path);
    }

    fn declared_tools(&self) -> Vec<(ExtensionId, ToolDecl)> {
        self.registry
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .declared_tools()
    }

    fn invoke(&self, tool_name: &str, params: Value) -> Result<Value, InvokeError> {
        self.registry
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .invoke(tool_name, params)
    }

    fn invoke_extension(
        &self,
        extension_id: &ExtensionId,
        tool_name: &str,
        params: Value,
    ) -> Result<Value, InvokeError> {
        self.registry
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .invoke_extension(extension_id, tool_name, params)
    }
}

struct SafeDeleteHarness {
    workspace: tempfile::TempDir,
    registry: ExtensionMergedRegistry,
    host: Arc<CapturingHost>,
    fixture_log: PathBuf,
    handle: EngineHandle,
}

#[derive(Clone, Copy)]
enum DeclarationFailure {
    Unavailable,
    Timeout,
    Crashed,
    Malformed,
}

struct DeclarationFailureHost {
    inner: Arc<CapturingHost>,
    failure: DeclarationFailure,
}

impl ExtensionHostPort for DeclarationFailureHost {
    fn on_file_indexed(&self, id: FileId, path: &RelativePath) {
        self.inner.on_file_indexed(id, path);
    }

    fn on_file_changed(&self, id: FileId, path: &RelativePath) {
        self.inner.on_file_changed(id, path);
    }

    fn on_file_deleted(&self, path: &RelativePath) {
        self.inner.on_file_deleted(path);
    }

    fn declared_tools(&self) -> Vec<(ExtensionId, ToolDecl)> {
        self.inner.declared_tools()
    }

    fn invoke(&self, tool_name: &str, params: Value) -> Result<Value, InvokeError> {
        self.inner.invoke(tool_name, params)
    }

    fn invoke_extension(
        &self,
        extension_id: &ExtensionId,
        tool_name: &str,
        params: Value,
    ) -> Result<Value, InvokeError> {
        if extension_id.as_str() == "ast" && tool_name == "plan_safe_delete_declaration" {
            return match self.failure {
                DeclarationFailure::Unavailable => Err(InvokeError::ToolNotFound(tool_name.into())),
                DeclarationFailure::Timeout => Err(InvokeError::Fault(ExtensionFault::Timeout)),
                DeclarationFailure::Crashed => Err(InvokeError::Fault(ExtensionFault::Crashed {
                    code: Some(17),
                })),
                DeclarationFailure::Malformed => Ok(json!({"private": "RAW_DECLARATION_SENTINEL"})),
            };
        }
        self.inner.invoke_extension(extension_id, tool_name, params)
    }
}

impl SafeDeleteHarness {
    fn new(mode: &str, files: &[(&str, &str)]) -> Self {
        Self::with_host_capture(mode, files, true)
    }

    fn with_host_capture(mode: &str, files: &[(&str, &str)], capture: bool) -> Self {
        let workspace = tempfile::tempdir().expect("create safe-delete workspace");
        fs::create_dir_all(workspace.path().join(".tower")).expect("create .tower");
        install_extension(workspace.path(), "ast", "ast_extension");
        install_extension(workspace.path(), "lsp", "lsp_extension");
        let fixture_log = workspace.path().join(".tower/safe-delete-lsp.log");
        let config_source = format!(
            r#"
[extensions]
request_timeout_secs = 30

[lsp.rust]
command = {}
args = [{}, {}]
extensions = ["rs"]
"#,
            serde_json::to_string(&fixture_lsp_server::binary_path()).unwrap(),
            serde_json::to_string(mode).unwrap(),
            serde_json::to_string(&fixture_log).unwrap(),
        );
        fs::write(workspace.path().join(".tower/config.toml"), &config_source)
            .expect("write fixture config");
        let config: TowerConfig = toml::from_str(&config_source).expect("parse fixture config");
        let opts = GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: Some(workspace.path().join(".tower/extensions")),
        };
        let handle = build_engine(&opts, config).expect("build safe-delete engine");
        let host = Arc::new(CapturingHost {
            registry: Arc::clone(&handle.ext_registry),
            changed: Mutex::new(Vec::new()),
        });
        if capture {
            handle
                .state
                .write()
                .unwrap_or_else(|poison| poison.into_inner())
                .set_plugin_host(host.clone());
        }
        let mut registry = ExtensionMergedRegistry::new(
            Arc::clone(&handle.state),
            Arc::clone(&handle.ext_registry),
        );
        for (path, content) in files {
            registry
                .call(
                    "tower_create_file",
                    json!({"path": path, "content": content}),
                )
                .expect("seed through the public mutation boundary");
        }
        registry
            .call("tower_ast_reindex", json!({}))
            .expect("seed the already-active AST observer");
        host.take_changed();

        Self {
            workspace,
            registry,
            host,
            fixture_log,
            handle,
        }
    }

    fn safe_delete(&mut self, path: &str, dry_run: bool) -> Value {
        self.safe_delete_named(path, "target", dry_run)
    }

    fn safe_delete_named(&mut self, path: &str, symbol_name: &str, dry_run: bool) -> Value {
        self.registry
            .call(
                "tower_safe_delete_symbol",
                json!({
                    "path": path,
                    "symbol_name": symbol_name,
                    "kind": "function",
                    "dry_run": dry_run
                }),
            )
            .expect("safe delete must return a structured public result")
    }

    fn read(&mut self, path: &str) -> Value {
        self.registry
            .call(
                "tower_read_file",
                json!({"path": path, "with_version": true}),
            )
            .expect("read public workspace state")
    }

    fn public_snapshot(&mut self, paths: &[&str]) -> Value {
        let reads = paths
            .iter()
            .map(|path| {
                self.registry
                    .call(
                        "tower_read_file",
                        json!({"path": path, "with_version": true}),
                    )
                    .ok()
            })
            .collect::<Vec<_>>();
        json!({
            "reads": reads,
            "listing": self.registry.call("tower_list_dir", json!({"path": ""})).unwrap(),
            "target_search": self.registry.call("tower_search_text", json!({"pattern": "target"})).unwrap(),
            "storage": self.storage_records(paths),
        })
    }

    fn fixture_trace(&self) -> String {
        fs::read_to_string(PathBuf::from(format!(
            "{}.trace",
            self.fixture_log.display()
        )))
        .unwrap_or_default()
    }

    /// Observe the actual engine storage port, rather than inferring durable
    /// state from a read served by the filesystem adapter.
    fn storage_records(&self, paths: &[&str]) -> Vec<Value> {
        let state = self
            .handle
            .state
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        let workspace = state
            .workspace
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        paths
            .iter()
            .map(|path| {
                let relative = RelativePath::new(*path);
                let id = workspace
                    .get_by_path(&relative)
                    .expect("seeded path remains present in VFS");
                serde_json::to_value(
                    state
                        .storage
                        .get(id)
                        .expect("VFS file must have a durable storage record"),
                )
                .expect("serialize durable record")
            })
            .collect()
    }

    fn replace_fs(&mut self, fs: Box<dyn FileSystemPort + Send + Sync>) {
        self.handle
            .state
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .fs = fs;
    }

    fn fail_declaration_with(&mut self, failure: DeclarationFailure) {
        self.handle
            .state
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .set_plugin_host(Arc::new(DeclarationFailureHost {
                inner: Arc::clone(&self.host),
                failure,
            }));
    }
}

struct FailWriteFs {
    inner: RealFs,
    fail_path: RelativePath,
    armed: bool,
}

struct DriftingReadFs {
    inner: RealFs,
    path: RelativePath,
    reads: AtomicUsize,
}

/// Fails the read after the AST declaration planner consumed the original
/// content. This exercises snapshot acquisition in the public orchestration
/// path without fabricating a planner result.
struct FailSecondReadFs {
    inner: RealFs,
    path: RelativePath,
    reads: AtomicUsize,
}

impl FileSystemPort for DriftingReadFs {
    fn exists(&self, path: &RelativePath) -> bool {
        self.inner.exists(path)
    }
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        if path == &self.path && self.reads.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(b"fn consumer(){changed();}\n".to_vec());
        }
        self.inner.read(path)
    }
    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.write(path, bytes)
    }
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        self.inner.rename(from, to)
    }
    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(path)
    }
    fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
        self.inner.mkdir(path)
    }
    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
    fn workspace_root(&self) -> Option<PathBuf> {
        self.inner.workspace_root()
    }
}

impl FileSystemPort for FailSecondReadFs {
    fn exists(&self, path: &RelativePath) -> bool {
        self.inner.exists(path)
    }
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        if path == &self.path && self.reads.fetch_add(1, Ordering::SeqCst) > 0 {
            return Err(PortError::ReadFailed(
                "injected snapshot failure".to_owned(),
            ));
        }
        self.inner.read(path)
    }
    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.write(path, bytes)
    }
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        self.inner.rename(from, to)
    }
    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(path)
    }
    fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
        self.inner.mkdir(path)
    }
    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
    fn workspace_root(&self) -> Option<PathBuf> {
        self.inner.workspace_root()
    }
}

impl FileSystemPort for FailWriteFs {
    fn exists(&self, path: &RelativePath) -> bool {
        self.inner.exists(path)
    }
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        self.inner.read(path)
    }
    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        if self.armed && path == self.fail_path {
            self.armed = false;
            return Err(PortError::WriteFailed("injected commit failure".to_owned()));
        }
        self.inner.write(path, bytes)
    }
    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        if self.armed && to == self.fail_path {
            self.armed = false;
            return Err(PortError::WriteFailed("injected commit failure".to_owned()));
        }
        self.inner.rename(from, to)
    }
    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(path)
    }
    fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
        self.inner.mkdir(path)
    }
    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
    fn workspace_root(&self) -> Option<PathBuf> {
        self.inner.workspace_root()
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn install_extension(workspace: &Path, name: &str, binary_name: &str) {
    let extension_dir = workspace.join(".tower/extensions").join(name);
    fs::create_dir_all(&extension_dir).expect("create extension directory");
    let shipped = fs::read_to_string(
        workspace_root()
            .join("extensions")
            .join(name)
            .join("extension.toml"),
    )
    .expect("read shipped extension manifest");
    let binary = workspace_root()
        .join("target/debug")
        .join(binary_name)
        .to_string_lossy()
        .replace('\\', "\\\\");
    let manifest = shipped
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("command") {
                format!("command = [\"{binary}\"]")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(extension_dir.join("extension.toml"), manifest).expect("write test manifest");
}

fn assert_blocked(result: &Value, reason: &str) {
    assert_eq!(result["status"], "blocked", "safe-delete payload: {result}");
    assert_eq!(
        result["blocking_reason"], reason,
        "safe-delete payload: {result}"
    );
}

fn scenario_public_tool_registers_the_canonical_schema_and_preserves_span_delete_compatibility() {
    let mut harness =
        SafeDeleteHarness::new("safe_delete_reference_free", &[("src/lib.rs", DECLARATION)]);
    let tool = harness
        .registry
        .list()
        .into_iter()
        .find(|tool| tool.name == "tower_safe_delete_symbol")
        .expect("canonical native tool must be registered");
    assert_eq!(
        tool.input_schema["required"],
        json!(["path", "symbol_name"])
    );
    assert_eq!(
        tool.input_schema["properties"]
            .as_object()
            .expect("schema properties")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["dry_run", "kind", "path", "symbol_name"]
    );

    let legacy = harness
        .registry
        .call(
            "tower_ast_delete_symbol",
            json!({"path": "src/lib.rs", "symbol_name": "target", "kind": "function"}),
        )
        .expect("legacy span-delete remains callable");
    assert_eq!(legacy["applied"], true, "legacy delete payload: {legacy}");
}

fn scenario_live_references_without_complete_cleanup_block_every_state_change() {
    let mut harness = SafeDeleteHarness::new(
        "safe_delete_no_cleanup_with_references",
        &[("src/lib.rs", DECLARATION), ("src/consumer.rs", CONSUMER)],
    );
    let before = harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]);

    let result = harness.safe_delete("src/lib.rs", false);

    assert_blocked(&result, "cleanup_incomplete");
    assert!(!result["references"].as_array().unwrap().is_empty());
    assert_eq!(
        harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]),
        before
    );
    assert!(harness.host.take_changed().is_empty());
}

#[test]
fn public_tool_preserves_legacy_delete_and_removes_an_unused_symbol_from_the_active_observer() {
    scenario_public_tool_registers_the_canonical_schema_and_preserves_span_delete_compatibility();

    let mut harness =
        SafeDeleteHarness::new("safe_delete_reference_free", &[("src/lib.rs", DECLARATION)]);
    let before = harness
        .registry
        .call(
            "tower_ast_search_symbols",
            json!({"name": "target", "kind": "function"}),
        )
        .expect("search active AST observer before mutation");
    assert!(before.to_string().contains("target"), "{before}");

    let result = harness.safe_delete("src/lib.rs", false);

    assert_eq!(result["status"], "applied", "safe-delete payload: {result}");
    assert!(
        !harness.read("src/lib.rs")["content"]
            .as_str()
            .unwrap()
            .contains("target")
    );
    assert_eq!(harness.host.take_changed(), ["src/lib.rs"]);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let search = harness
            .registry
            .call(
                "tower_ast_search_symbols",
                json!({"name": "target", "kind": "function"}),
            )
            .expect("search active AST observer after mutation");
        if !search.to_string().contains("target") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "observer retained target: {search}"
        );
        std::thread::yield_now();
    }
}

#[test]
fn complete_cleanup_and_dry_run_share_the_same_multi_file_plan() {
    let mut harness = SafeDeleteHarness::new(
        "safe_delete_complete",
        &[("src/lib.rs", DECLARATION), ("src/consumer.rs", CONSUMER)],
    );

    let result = harness.safe_delete("src/lib.rs", false);

    assert_eq!(result["status"], "applied", "safe-delete payload: {result}");
    assert_eq!(
        result["affected_files"],
        json!(["src/consumer.rs", "src/lib.rs"])
    );
    assert_eq!(harness.read("src/lib.rs")["content"], "");
    assert_eq!(
        harness.read("src/consumer.rs")["content"],
        "fn consumer(){;}\n"
    );
    let mut changed = harness.host.take_changed();
    changed.sort();
    assert_eq!(changed, ["src/consumer.rs", "src/lib.rs"]);

    scenario_dry_run_matches_apply_plan_and_preflight_while_writing_and_notifying_nothing();
}

fn scenario_stale_or_invalid_later_target_rejects_the_whole_plan_without_callbacks() {
    for (mode, dry_run) in [
        ("safe_delete_complete", false),
        ("safe_delete_invalid_location", false),
        ("safe_delete_complete", true),
        ("safe_delete_invalid_location", true),
    ] {
        let mut harness = SafeDeleteHarness::new(
            mode,
            &[("src/lib.rs", DECLARATION), ("src/consumer.rs", CONSUMER)],
        );
        let before = harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]);
        if mode == "safe_delete_complete" {
            harness.replace_fs(Box::new(DriftingReadFs {
                inner: RealFs::new(harness.workspace.path()),
                path: RelativePath::new("src/consumer.rs"),
                reads: AtomicUsize::new(0),
            }));
        }

        let result = harness.safe_delete("src/lib.rs", dry_run);

        assert_blocked(
            &result,
            if mode == "safe_delete_complete" {
                "cas_conflict"
            } else {
                "invalid_cleanup"
            },
        );
        if mode == "safe_delete_complete" {
            harness.replace_fs(Box::new(RealFs::new(harness.workspace.path())));
        }
        assert_eq!(
            harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]),
            before
        );
        assert!(harness.host.take_changed().is_empty());
    }
}

#[test]
fn mutation_failures_distinguish_preflight_rejection_from_partial_commit() {
    scenario_stale_or_invalid_later_target_rejects_the_whole_plan_without_callbacks();

    let mut harness = SafeDeleteHarness::new(
        "safe_delete_complete_triple",
        &[
            ("src/a.rs", DECLARATION),
            ("src/b.rs", CONSUMER),
            ("src/c.rs", "fn another(){target();}\n"),
        ],
    );
    let before = harness.public_snapshot(&["src/a.rs", "src/b.rs", "src/c.rs"]);
    harness.replace_fs(Box::new(FailWriteFs {
        inner: RealFs::new(harness.workspace.path()),
        fail_path: RelativePath::new("src/b.rs"),
        armed: true,
    }));

    let result = harness.safe_delete("src/a.rs", false);

    assert_eq!(
        result["status"], "partial_commit",
        "safe-delete payload: {result}"
    );
    assert_eq!(
        result["mutation_result"]["per_file"],
        json!([
            {"path": "src/a.rs", "outcome": "committed"},
            {"path": "src/b.rs", "outcome": "failed", "error": "filesystem_error"},
            {"path": "src/c.rs", "outcome": "unattempted"}
        ])
    );
    assert_eq!(harness.read("src/a.rs")["content"], "");
    assert_eq!(harness.read("src/b.rs")["content"], CONSUMER);
    assert_eq!(
        harness.read("src/c.rs")["content"],
        "fn another(){target();}\n"
    );
    assert_eq!(
        harness
            .registry
            .call("tower_search_text", json!({"pattern": "target"}))
            .expect("partial commit leaves the public search index usable"),
        json!({"matches": [
            {"path": "src/b.rs", "line_number": 1, "line_content": "fn consumer(){target();}"},
            {"path": "src/c.rs", "line_number": 1, "line_content": "fn another(){target();}"}
        ]})
    );
    assert_eq!(
        harness
            .storage_records(&["src/a.rs", "src/b.rs", "src/c.rs"])
            .len(),
        3
    );
    assert_eq!(harness.host.take_changed(), ["src/a.rs"]);
    let after = harness.public_snapshot(&["src/a.rs", "src/b.rs", "src/c.rs"]);
    assert_ne!(after["reads"][0]["version"], before["reads"][0]["version"]);
    assert_eq!(after["reads"][1], before["reads"][1]);
    assert_eq!(after["reads"][2], before["reads"][2]);
    // A committed edit preserves identity while persisting the new content's
    // metadata. Failed/unattempted targets retain their complete old records.
    assert_eq!(after["storage"][0]["id"], before["storage"][0]["id"]);
    assert_eq!(after["storage"][0]["path"], before["storage"][0]["path"]);
    assert_eq!(after["storage"][0]["size"], 0);
    use sha2::Digest as _;
    assert_eq!(
        after["storage"][0]["content_hash"],
        json!(sha2::Sha256::digest(b"").to_vec())
    );
    assert_eq!(after["storage"][1], before["storage"][1]);
    assert_eq!(after["storage"][2], before["storage"][2]);
    assert_ne!(after["target_search"], before["target_search"]);
    assert_eq!(after["listing"], before["listing"]);
}

fn scenario_dry_run_matches_apply_plan_and_preflight_while_writing_and_notifying_nothing() {
    let files = [("src/lib.rs", DECLARATION), ("src/consumer.rs", CONSUMER)];
    let mut preview = SafeDeleteHarness::new("safe_delete_complete", &files);
    let before = preview.public_snapshot(&["src/lib.rs", "src/consumer.rs"]);

    let preview_result = preview.safe_delete("src/lib.rs", true);

    assert_eq!(preview_result["status"], "previewed", "{preview_result}");
    assert_eq!(
        preview.public_snapshot(&["src/lib.rs", "src/consumer.rs"]),
        before
    );
    assert!(preview.host.take_changed().is_empty());
    let mut apply = SafeDeleteHarness::new("safe_delete_complete", &files);
    let apply_result = apply.safe_delete("src/lib.rs", false);
    for field in [
        "declaration",
        "references",
        "proposed_edits",
        "affected_files",
    ] {
        assert_eq!(preview_result[field], apply_result[field], "field {field}");
    }
}

fn scenario_snapshot_failure_and_declaration_staleness_block_before_semantic_lookup() {
    let mut snapshot_failure = SafeDeleteHarness::new(
        "safe_delete_complete",
        &[("src/lib.rs", DECLARATION), ("src/consumer.rs", CONSUMER)],
    );
    let before = snapshot_failure.public_snapshot(&["src/lib.rs", "src/consumer.rs"]);
    snapshot_failure.replace_fs(Box::new(FailSecondReadFs {
        inner: RealFs::new(snapshot_failure.workspace.path()),
        path: RelativePath::new("src/lib.rs"),
        reads: AtomicUsize::new(0),
    }));

    let result = snapshot_failure.safe_delete("src/lib.rs", false);

    assert_blocked(&result, "references_unresolved");
    snapshot_failure.replace_fs(Box::new(RealFs::new(snapshot_failure.workspace.path())));
    assert_eq!(
        snapshot_failure.public_snapshot(&["src/lib.rs", "src/consumer.rs"]),
        before
    );
    assert!(snapshot_failure.host.take_changed().is_empty());

    let mut stale = SafeDeleteHarness::new(
        "safe_delete_capability_unavailable",
        &[("src/lib.rs", DECLARATION)],
    );
    let before = stale.public_snapshot(&["src/lib.rs"]);
    stale.replace_fs(Box::new(DriftingReadFs {
        inner: RealFs::new(stale.workspace.path()),
        path: RelativePath::new("src/lib.rs"),
        reads: AtomicUsize::new(0),
    }));

    let result = stale.safe_delete("src/lib.rs", false);

    assert_blocked(&result, "cas_conflict");
    stale.replace_fs(Box::new(RealFs::new(stale.workspace.path())));
    assert_eq!(stale.public_snapshot(&["src/lib.rs"]), before);
    assert!(stale.host.take_changed().is_empty());
    assert!(
        !stale.fixture_trace().contains("textDocument/references"),
        "stale declaration must win before semantic lookup: {}",
        stale.fixture_trace()
    );
}

fn scenario_declaration_rejection_short_circuits_before_semantic_lookup() {
    let mut harness = SafeDeleteHarness::new(
        "safe_delete_complete",
        &[("src/lib.rs", DECLARATION), ("src/consumer.rs", CONSUMER)],
    );
    let before = harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]);

    let result = harness
        .registry
        .call(
            "tower_safe_delete_symbol",
            json!({"path": "src/lib.rs", "symbol_name": "missing", "kind": "function", "dry_run": false}),
        )
        .expect("declaration failure is a structured public result");

    assert_blocked(&result, "not_found");
    assert_eq!(
        harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]),
        before
    );
    assert!(harness.host.take_changed().is_empty());
    assert!(
        !harness.fixture_trace().contains("textDocument/references"),
        "declaration rejection must not invoke semantic lookup: {}",
        harness.fixture_trace()
    );
}

#[test]
fn planning_refusals_are_non_mutating_and_short_circuit_semantic_lookup() {
    scenario_live_references_without_complete_cleanup_block_every_state_change();
    scenario_snapshot_failure_and_declaration_staleness_block_before_semantic_lookup();
    scenario_declaration_rejection_short_circuits_before_semantic_lookup();
}

fn scenario_backend_mappings_are_exact_session_safe_and_private_at_the_public_boundary() {
    let cases = [
        (
            "safe_delete_capability_unavailable",
            "capability_unavailable",
        ),
        ("safe_delete_incomplete", "cleanup_incomplete"),
        ("safe_delete_invalid_location", "invalid_cleanup"),
        ("safe_delete_workspace_external", "invalid_cleanup"),
        ("safe_delete_backend_timeout", "backend_error"),
        ("safe_delete_backend_crashed", "backend_error"),
        ("safe_delete_malformed_response", "backend_error"),
    ];
    for (mode, reason) in cases {
        let mut harness = SafeDeleteHarness::new(
            mode,
            &[("src/lib.rs", DECLARATION), ("src/consumer.rs", CONSUMER)],
        );
        let before = harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]);

        let result = harness.safe_delete("src/lib.rs", false);

        assert_blocked(&result, reason);
        assert_eq!(
            harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]),
            before
        );
        assert!(harness.host.take_changed().is_empty());
        assert!(
            harness
                .registry
                .call("tower_list_dir", json!({"path": ""}))
                .is_ok()
        );
        // These values are produced by the sidecar response or public request,
        // then cross the real native-to-sidecar boundary. The fixture trace is
        // deliberately not used as a logging assertion: it is LSP traffic,
        // not native orchestration logging.
        let observed = result.to_string();
        for private in [
            "BACKEND_TEXT_SENTINEL_0a42",
            "/private/ABSOLUTE_PATH_SENTINEL_5f14.rs",
            "file:///external/URI_SENTINEL_39c1.rs",
            DECLARATION,
            CONSUMER,
        ] {
            assert!(
                !observed.contains(private),
                "mode {mode} leaked {private}: {observed}"
            );
        }
    }
}

fn scenario_declaration_backend_failures_are_structured_private_and_non_mutating() {
    for (failure, reason) in [
        (DeclarationFailure::Unavailable, "capability_unavailable"),
        (DeclarationFailure::Timeout, "backend_error"),
        (DeclarationFailure::Crashed, "backend_error"),
        (DeclarationFailure::Malformed, "backend_error"),
    ] {
        let mut harness =
            SafeDeleteHarness::new("safe_delete_complete", &[("src/lib.rs", DECLARATION)]);
        let before = harness.public_snapshot(&["src/lib.rs"]);
        harness.fail_declaration_with(failure);

        let result = harness.safe_delete("src/lib.rs", false);

        assert_blocked(&result, reason);
        assert_eq!(harness.public_snapshot(&["src/lib.rs"]), before);
        assert!(harness.host.take_changed().is_empty());
        assert!(!result.to_string().contains("RAW_DECLARATION_SENTINEL"));
        assert!(
            harness
                .registry
                .call("tower_list_dir", json!({"path": ""}))
                .is_ok(),
            "the public MCP registry must remain usable after declaration failure"
        );
    }
}

#[test]
fn backend_failures_map_to_private_structured_results_and_keep_the_session_usable() {
    scenario_backend_mappings_are_exact_session_safe_and_private_at_the_public_boundary();
    scenario_declaration_backend_failures_are_structured_private_and_non_mutating();
}

#[test]
fn private_request_source_replacement_uri_credential_and_raw_error_never_escape_results() {
    const CHILD_ENV: &str = "TOWER_SAFE_DELETE_LOG_CAPTURE_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = Command::new(std::env::current_exe().expect("resolve current test binary"))
            .args([
                "--exact",
                "private_request_source_replacement_uri_credential_and_raw_error_never_escape_results",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("run confidentiality scenario behind a captured process boundary");
        assert!(
            output.status.success(),
            "captured child failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        for private in [
            "SOURCE_SENTINEL_29bc",
            "REPLACEMENT_SENTINEL_fa81",
            "CREDENTIAL_SENTINEL_7d91",
            "RAW_SIDECAR_ERROR_SENTINEL",
            "/private/ABSOLUTE_PATH_SENTINEL_5f14.rs",
            "file:///external/URI_SENTINEL_39c1.rs",
            "target_REQUEST_SENTINEL_4e0a",
        ] {
            assert!(
                !stderr.contains(private),
                "native stderr leaked {private}: {stderr}"
            );
        }
        assert!(
            !stderr.contains("safe-delete planning failed"),
            "expected blocked outcomes must not log: {stderr}"
        );
        return;
    }

    const PRIVATE_SOURCE: &str = "pub fn target() {} // SOURCE_SENTINEL_29bc\n";
    let cases = [
        (
            "safe_delete_backend_credential_sentinel",
            DECLARATION,
            "target",
            "backend_error",
            vec!["CREDENTIAL_SENTINEL_7d91", "RAW_SIDECAR_ERROR_SENTINEL"],
        ),
        (
            "safe_delete_private_replacement",
            DECLARATION,
            "target",
            "cleanup_incomplete",
            vec!["REPLACEMENT_SENTINEL_fa81"],
        ),
        (
            "safe_delete_absolute_path_sentinel",
            PRIVATE_SOURCE,
            "target",
            "invalid_cleanup",
            vec![
                "SOURCE_SENTINEL_29bc",
                "/private/ABSOLUTE_PATH_SENTINEL_5f14.rs",
            ],
        ),
        (
            "safe_delete_external_uri_sentinel",
            PRIVATE_SOURCE,
            "target",
            "invalid_cleanup",
            vec![
                "SOURCE_SENTINEL_29bc",
                "file:///external/URI_SENTINEL_39c1.rs",
            ],
        ),
        (
            "safe_delete_complete",
            PRIVATE_SOURCE,
            "target_REQUEST_SENTINEL_4e0a",
            "not_found",
            vec!["target_REQUEST_SENTINEL_4e0a", "SOURCE_SENTINEL_29bc"],
        ),
    ];
    for (mode, declaration, symbol_name, reason, private) in cases {
        let mut harness = SafeDeleteHarness::new(
            mode,
            &[("src/lib.rs", declaration), ("src/consumer.rs", CONSUMER)],
        );
        let before = harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]);

        let result = harness.safe_delete_named("src/lib.rs", symbol_name, false);

        assert_blocked(&result, reason);
        assert_eq!(
            harness.public_snapshot(&["src/lib.rs", "src/consumer.rs"]),
            before,
            "private producer {mode} must not mutate state"
        );
        assert!(harness.host.take_changed().is_empty());
        for sentinel in private {
            assert!(
                !result.to_string().contains(sentinel),
                "public payload leaked {sentinel} for {mode}: {result}"
            );
        }
    }
}

#[test]
fn safe_delete_uses_the_production_host_for_preview_and_apply() {
    let mut harness = SafeDeleteHarness::with_host_capture(
        "safe_delete_reference_free",
        &[("src/lib.rs", DECLARATION)],
        false,
    );
    let preview = harness.safe_delete("src/lib.rs", true);
    assert_eq!(preview["status"], "previewed", "{preview}");
    assert_eq!(
        fs::read_to_string(harness.workspace.path().join("src/lib.rs")).unwrap(),
        DECLARATION
    );
    let applied = harness.safe_delete("src/lib.rs", false);
    assert_eq!(applied["status"], "applied", "{applied}");
    assert_eq!(applied["proposed_edits"], preview["proposed_edits"]);
    assert_eq!(
        fs::read_to_string(harness.workspace.path().join("src/lib.rs")).unwrap(),
        ""
    );
}

#[test]
fn recursive_symbol_without_external_callers_previews_and_applies_through_the_production_host() {
    let source = "pub fn old_name() { old_name(); }\n";
    let mut harness = SafeDeleteHarness::with_host_capture(
        "safe_delete_no_cleanup_internal",
        &[("src/lib.rs", source)],
        false,
    );
    let before = harness.read("src/lib.rs");
    let preview = harness.safe_delete_named("src/lib.rs", "old_name", true);
    assert_eq!(preview["status"], "previewed", "{preview}");
    assert_eq!(preview["references"], json!([]));
    assert_eq!(harness.read("src/lib.rs"), before);
    let applied = harness.safe_delete_named("src/lib.rs", "old_name", false);
    assert_eq!(applied["status"], "applied", "{applied}");
    assert_eq!(applied["proposed_edits"], preview["proposed_edits"]);
    assert_eq!(harness.read("src/lib.rs")["content"], "");
    let symbols = harness
        .registry
        .call(
            "tower_ast_search_symbols",
            json!({"name": "old_name", "kind": "function"}),
        )
        .unwrap();
    assert!(!symbols.to_string().contains("old_name"), "{symbols}");
}
