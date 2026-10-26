#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

// Feature: F011

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::TowerConfig;
use core_engine::adapters::config::lsp::{
    LspConfig, LspServerConfig, lsp_initialize_payload_with_diagnostics_timeout,
};
use core_engine::adapters::daemon::engine::{EngineHandle, build_engine};
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::registry::ToolRegistry;
use serde_json::{Value, json};

const PRIVATE_SENTINELS: [&str; 5] = [
    "/absolute/PRIVATE_ABSOLUTE_PATH_SENTINEL",
    "file:///PRIVATE_EXTERNAL_URI_SENTINEL",
    "PRIVATE_REPLACEMENT_CONTENT_SENTINEL",
    "PRIVATE_WORKSPACE_EXTERNAL_CONTENT_SENTINEL",
    "PRIVATE_RAW_BACKEND_SENTINEL",
];

struct PathRenameHarness {
    _sandbox: tempfile::TempDir,
    workspace: tempfile::TempDir,
    registry: ExtensionMergedRegistry,
    fixture_log: PathBuf,
    _handle: EngineHandle,
}

/// Drives the sidecar protocol directly only to prove the public JSON-RPC
/// queueing contract while a `workspace/readFile` HostCall is outstanding.
struct RawLspSidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RawLspSidecar {
    fn spawn() -> Self {
        let mut child = Command::new(workspace_root().join("target/debug/lsp_extension"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn native lsp sidecar");
        Self {
            stdin: child.stdin.take().expect("sidecar stdin"),
            stdout: BufReader::new(child.stdout.take().expect("sidecar stdout")),
            child,
        }
    }

    fn send(&mut self, frame: Value) {
        writeln!(
            self.stdin,
            "{}",
            serde_json::to_string(&frame).expect("serialize frame")
        )
        .expect("write sidecar frame");
        self.stdin.flush().expect("flush sidecar frame");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read sidecar frame");
        serde_json::from_str(line.trim()).expect("valid sidecar frame")
    }
}

impl Drop for RawLspSidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl PathRenameHarness {
    fn new(mode: &str, files: &[(&str, &str)], ignore: Option<&str>) -> Self {
        // Keep the workspace's parent private to this harness. External-path
        // snapshots can then prove this request made no sibling entry without
        // observing unrelated tests creating their own /tmp directories.
        let sandbox = tempfile::tempdir().expect("create path-rename sandbox");
        let workspace = tempfile::tempdir_in(sandbox.path()).expect("create path-rename workspace");
        fs::create_dir_all(workspace.path().join(".tower")).expect("create .tower");
        for (path, content) in files {
            write_file(workspace.path(), path, content);
        }
        if let Some(ignore) = ignore {
            write_file(workspace.path(), ".towerignore", ignore);
        }
        install_lsp_extension(workspace.path());

        let fixture_log = workspace.path().join(".tower/fixture-lsp.log");
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
        let handle = build_engine(&opts, config).expect("build path-rename engine");
        let registry = ExtensionMergedRegistry::with_rename_coordinator(
            Arc::clone(&handle.state),
            handle.ext_registry.clone(),
            Arc::clone(&handle.rename_coordinator),
        );

        Self {
            _sandbox: sandbox,
            workspace,
            registry,
            fixture_log,
            _handle: handle,
        }
    }

    fn rename(&mut self, source: &str, destination: &str, policy: &str, dry_run: bool) -> Value {
        self.registry
            .call(
                "tower_rename_path",
                json!({
                    "source": source,
                    "destination": destination,
                    "dry_run": dry_run,
                    "semantic_policy": policy,
                    "expected_versions": {}
                }),
                &core_engine::domain::NeverCancelled,
            )
            .expect("tower_rename_path must return a structured result")
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        self.registry
            .call(name, args, &core_engine::domain::NeverCancelled)
            .expect("public tool call")
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.workspace.path().join(path)).ok()
    }

    fn snapshot(&mut self) -> Value {
        let files = sorted_workspace_files(self.workspace.path());
        let listing = self.call("tower_list_dir", json!({"path": ""}));
        let old_search = self.call("tower_search_text", json!({"pattern": "old"}));
        let new_search = self.call("tower_search_text", json!({"pattern": "new"}));
        let versions = ["src/old.rs", "src/new.rs", "src/consumer.rs"]
            .into_iter()
            .map(|path| {
                self.registry
                    .call(
                        "tower_read_file",
                        json!({"path": path, "with_version": true}),
                        &core_engine::domain::NeverCancelled,
                    )
                    .ok()
            })
            .collect::<Vec<_>>();
        json!({
            "files": files,
            "listing": listing,
            "old_search": old_search,
            "new_search": new_search,
            "versions": versions
        })
    }

    fn fixture_trace(&self) -> String {
        fs::read_to_string(&self.fixture_log).unwrap_or_default()
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

fn install_lsp_extension(workspace: &Path) {
    let extension_dir = workspace.join(".tower/extensions/lsp");
    fs::create_dir_all(&extension_dir).expect("create extension directory");
    let shipped = fs::read_to_string(workspace_root().join("extensions/lsp/extension.toml"))
        .expect("read shipped LSP manifest");
    let binary = workspace_root()
        .join("target/debug/lsp_extension")
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

fn write_file(root: &Path, path: &str, content: &str) {
    let destination = root.join(path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).expect("create file parent");
    }
    fs::write(destination, content).expect("write fixture file");
}

fn sorted_workspace_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(current).expect("read workspace directory") {
            let entry = entry.expect("read workspace entry");
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == ".tower") {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root)
                        .expect("workspace-relative path")
                        .to_string_lossy()
                        .replace('\\', "/"),
                    fs::read(path).expect("read workspace file"),
                ));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn external_destination_snapshot(workspace: &Path, destination: &str) -> Value {
    let destination = workspace.join(destination);
    let parent = destination
        .parent()
        .expect("external destination has parent");
    json!({
        "destination_exists": destination.exists(),
        "destination_bytes": fs::read(&destination).ok(),
        "parent_exists": parent.exists(),
        "parent_entries": if parent.exists() {
            let mut entries = fs::read_dir(parent)
                .expect("read external destination parent")
                .map(|entry| entry.expect("read external destination entry").file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            entries.sort();
            Some(entries)
        } else {
            None
        },
    })
}

fn assert_applied(result: &Value) {
    assert_eq!(result["status"], "applied", "rename payload: {result}");
    assert_eq!(result["errors"], json!([]), "rename payload: {result}");
}

#[test]
fn native_file_rename_updates_disk_and_public_indexes_without_stale_source_paths() {
    let mut harness = PathRenameHarness::new(
        "path_rename_supported",
        &[("src/old.rs", "pub fn old_value() {}\n")],
        None,
    );

    let result = harness.rename("src/old.rs", "src/new.rs", "best_effort", false);

    assert_applied(&result);
    assert_eq!(
        harness.read("src/new.rs").unwrap(),
        b"pub fn old_value() {}\n"
    );
    assert_eq!(harness.read("src/old.rs"), None);
    let listing = harness.call("tower_list_dir", json!({"path": "src"}));
    assert!(listing.to_string().contains("src/new.rs"), "{listing}");
    assert!(!listing.to_string().contains("src/old.rs"), "{listing}");
    let search = harness.call("tower_search_text", json!({"pattern": "old_value"}));
    assert!(search.to_string().contains("src/new.rs"), "{search}");
    assert!(!search.to_string().contains("src/old.rs"), "{search}");
    assert!(
        harness
            .fixture_trace()
            .contains("workspace/willRenameFiles")
    );

    fs::remove_file(harness.workspace.path().join("src/new.rs")).expect("external delete");
    write_file(
        harness.workspace.path(),
        "src/old.rs",
        "pub fn externally_recreated() {}\n",
    );
    harness.call("tower_reindex", json!({}));
    let listing = harness.call("tower_list_dir", json!({"path": "src"}));
    assert!(listing.to_string().contains("src/old.rs"), "{listing}");
    assert!(!listing.to_string().contains("src/new.rs"), "{listing}");
}

#[test]
fn native_directory_rename_preserves_descendants_and_removes_stale_source_listing_and_search_hits()
{
    let mut harness = PathRenameHarness::new(
        "path_rename_supported",
        &[
            ("src/old/a.rs", "pub fn old_alpha() {}\n"),
            ("src/old/nested/b.rs", "pub fn old_beta() {}\n"),
        ],
        None,
    );

    let result = harness.rename("src/old", "src/new", "best_effort", false);

    assert_applied(&result);
    assert!(harness.read("src/new/a.rs").is_some());
    assert!(harness.read("src/new/nested/b.rs").is_some());
    assert!(!harness.workspace.path().join("src/old").exists());
    let listing = harness.call("tower_list_dir", json!({"path": "src"}));
    assert!(!listing.to_string().contains("src/old"), "{listing}");
    let search = harness.call("tower_search_text", json!({"pattern": "old_beta"}));
    assert!(!search.to_string().contains("src/old"), "{search}");
}

#[test]
fn semantic_import_update_applies_fixture_edit_and_records_the_exact_rename_pair() {
    let mut harness = PathRenameHarness::new(
        "path_rename_semantic",
        &[
            ("src/old.rs", "pub fn value() {}\n"),
            ("src/consumer.rs", "use crate::old;\n"),
        ],
        None,
    );

    let result = harness.rename("src/old.rs", "src/new.rs", "required", false);

    assert_applied(&result);
    assert_eq!(
        harness.read("src/consumer.rs").unwrap(),
        b"use crate::new;\n"
    );
    let trace = harness.fixture_trace();
    assert!(trace.contains("workspace/willRenameFiles"), "{trace}");
    assert!(trace.contains("src/old.rs"), "{trace}");
    assert!(trace.contains("src/new.rs"), "{trace}");
}

#[test]
fn required_unsupported_returns_structured_unsupported_and_mutates_nothing() {
    let mut harness = PathRenameHarness::new(
        "path_rename_unsupported",
        &[("src/old.rs", "pub fn old_value() {}\n")],
        None,
    );
    let before = harness.snapshot();

    let result = harness.rename("src/old.rs", "src/new.rs", "required", false);

    assert_eq!(result["status"], "unsupported", "rename payload: {result}");
    assert_eq!(result["errors"][0]["code"], "semantic_unsupported");
    assert_eq!(harness.snapshot(), before);
}

#[test]
fn dry_run_preserves_paths_bytes_versions_and_search_visibility() {
    let mut harness = PathRenameHarness::new(
        "path_rename_supported",
        &[("src/old.rs", "pub fn old_value() {}\n")],
        None,
    );
    let before = harness.snapshot();

    let result = harness.rename("src/old.rs", "src/new.rs", "best_effort", true);

    assert_eq!(result["status"], "preview", "rename payload: {result}");
    assert_eq!(result["plan"]["moves"][0]["source"], "src/old.rs");
    assert_eq!(result["plan"]["moves"][0]["destination"], "src/new.rs");
    assert_eq!(harness.snapshot(), before);
}

#[test]
fn ignored_and_workspace_external_destinations_preserve_all_public_and_disk_state() {
    for (destination, ignore, expected_code) in [
        ("ignored/new.rs", Some("ignored/\n"), "destination_excluded"),
        ("../outside.rs", None, "workspace_escape"),
    ] {
        let mut harness = PathRenameHarness::new(
            "path_rename_supported",
            &[("src/old.rs", "pub fn old_value() {}\n")],
            ignore,
        );
        let before = harness.snapshot();
        let external_before = (destination.starts_with("../"))
            .then(|| external_destination_snapshot(harness.workspace.path(), destination));

        let result = harness.rename("src/old.rs", destination, "none", false);

        assert_eq!(result["status"], "rejected", "rename payload: {result}");
        assert_eq!(
            result["errors"][0]["code"], expected_code,
            "destination {destination}: {result}"
        );
        assert_eq!(harness.snapshot(), before, "destination: {destination}");
        assert_eq!(
            (destination.starts_with("../"))
                .then(|| external_destination_snapshot(harness.workspace.path(), destination)),
            external_before,
            "external destination or its parent changed: {destination}"
        );
    }
}

#[test]
fn public_failures_and_process_logs_redact_every_private_sentinel() {
    let mut semantic_harness = PathRenameHarness::new(
        "path_rename_semantic",
        &[
            ("src/old.rs", "pub fn old_value() {}\n"),
            (
                "src/consumer.rs",
                "x\n// PRIVATE_WORKSPACE_EXTERNAL_CONTENT_SENTINEL\n",
            ),
        ],
        None,
    );
    let semantic_result = semantic_harness.rename("src/old.rs", "src/new.rs", "required", false);
    let semantic_observed = format!("{semantic_result}\n{}", semantic_harness.fixture_trace());

    assert_eq!(semantic_result["status"], "rejected", "{semantic_result}");
    assert_eq!(
        semantic_result["errors"][0]["code"], "semantic_backend",
        "{semantic_result}"
    );
    assert!(
        semantic_harness
            .fixture_trace()
            .contains("workspace/willRenameFiles"),
        "semantic fixture must reach edit resolution: {semantic_observed}"
    );
    assert!(
        !semantic_observed.contains(PRIVATE_SENTINELS[3]),
        "workspace content leaked: {semantic_observed}"
    );

    for mode in [
        "path_rename_malformed",
        "path_rename_timeout",
        "path_rename_crash",
    ] {
        let mut harness =
            PathRenameHarness::new(mode, &[("src/old.rs", "pub fn old_value() {}\n")], None);

        let result = harness.rename("src/old.rs", "src/new.rs", "required", false);
        let observed = format!("{result}\n{}", harness.fixture_trace());

        assert_eq!(result["status"], "rejected", "mode {mode}: {result}");
        assert_eq!(
            result["errors"][0]["code"], "semantic_backend",
            "mode {mode}: {result}"
        );
        assert_eq!(
            result["errors"][0]["message"], "Semantic planning failed",
            "mode {mode}: {result}"
        );
        assert!(
            harness
                .fixture_trace()
                .contains(&format!("privacy_fixture_exercised:{mode}")),
            "mode {mode} must prove the sentinel-producing fixture branch ran: {observed}"
        );
        for sentinel in PRIVATE_SENTINELS
            .into_iter()
            .filter(|sentinel| *sentinel != PRIVATE_SENTINELS[3])
        {
            assert!(
                !observed.contains(sentinel),
                "mode {mode} leaked {sentinel}: {observed}"
            );
        }
    }
}

#[test]
fn path_rename_fault_fixture_keeps_its_optional_log_contract() {
    let mut child = Command::new(fixture_lsp_server::binary_path())
        .arg("path_rename_malformed")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fixture without optional log argument");
    let mut stdin = child.stdin.take().expect("fixture stdin");
    for message in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "workspace/willRenameFiles",
            "params": {"files": [{"oldUri": "file:///src/old.rs", "newUri": "file:///src/new.rs"}]}
        }),
    ] {
        let body = serde_json::to_vec(&message).expect("serialize fixture request");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write fixture header");
        stdin.write_all(&body).expect("write fixture request");
    }
    drop(stdin);

    let output = child.wait_with_output().expect("wait for fixture");
    assert!(
        output.status.success(),
        "optional log omission must not change fault behavior: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("PRIVATE_RAW_BACKEND_SENTINEL"),
        "malformed mode must still emit its selected response"
    );
}

#[test]
fn concurrent_spawn_and_request_handles_queued_frames_twenty_times_with_fresh_native_binaries() {
    const RUNS: usize = 20;
    let workers = (0..RUNS)
        .map(|iteration| {
            thread::spawn(move || {
                let workspace = tempfile::tempdir().expect("create raw sidecar workspace");
                let config = LspConfig {
                    servers: std::collections::BTreeMap::from([(
                        "rust".to_owned(),
                        LspServerConfig {
                            command: fixture_lsp_server::binary_path()
                                .to_string_lossy()
                                .into_owned(),
                            args: vec!["path_rename_semantic".to_owned()],
                            extensions: vec!["rs".to_owned()],
                        },
                    )]),
                    idle_timeout: None,
                };
                let mut sidecar = RawLspSidecar::spawn();
                sidecar.send(json!({
                    "jsonrpc": "2.0", "id": 0, "method": "initialize",
                    "params": {
                        "protocol_version": extension_protocol::PROTOCOL_VERSION,
                        "client_info": "path-rename-queue-test/0.1.0",
                        "extension_config": lsp_initialize_payload_with_diagnostics_timeout(
                            &config,
                            Some(Duration::from_secs(29)),
                        )
                    }
                }));
                assert!(
                    sidecar.receive().get("result").is_some(),
                    "iteration {iteration}: initialization must succeed"
                );
                sidecar.send(json!({
                    "jsonrpc": "2.0", "id": 1, "method": "invokeTool",
                    "params": {"name": "plan_path_rename", "params": {
                        "moves": [{"source": "src/old.rs", "destination": "src/new.rs", "kind": "file"}],
                        "semantic_policy": "required"
                    }}
                }));

                // The semantic fixture produces an edit, so the sidecar must await this
                // real HostCall.  Inject the next host request before answering it.
                let read = sidecar.receive();
                assert_eq!(read["method"], "workspace/readFile", "iteration {iteration}: {read}");
                sidecar.send(json!({
                    "jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": {}
                }));
                sidecar.send(json!({
                    "jsonrpc": "2.0", "id": read["id"].clone(),
                    "result": "use crate::old;\n"
                }));

                let first = sidecar.receive();
                assert_eq!(first["id"], 1, "iteration {iteration}: queued request overtook rename: {first}");
                assert!(first.get("result").is_some(), "iteration {iteration}: rename failed: {first}");
                let second = sidecar.receive();
                assert_eq!(second["id"], 2, "iteration {iteration}: queued shutdown was not replayed FIFO: {second}");
                drop(workspace);
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        worker.join().expect("concurrent path-rename request");
    }
}

struct BenchmarkWorkspace {
    path: PathBuf,
}

impl BenchmarkWorkspace {
    fn create(operator_root: &Path) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("TOWER_RENAME_PERF_ROOT prerequisite failed: system clock precedes Unix epoch")
            .as_nanos();
        let path = operator_root.join(format!("tower-rename-perf-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap_or_else(|error| {
            panic!(
                "TOWER_RENAME_PERF_ROOT prerequisite failed: cannot create benchmark child {}: {error}",
                path.display()
            )
        });
        Self { path }
    }
}

impl Drop for BenchmarkWorkspace {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "failed to clean benchmark-owned child {}: {error}",
                self.path.display()
            );
        }
    }
}

fn validated_performance_root() -> (PathBuf, String) {
    let configured = std::env::var_os("TOWER_RENAME_PERF_ROOT").unwrap_or_else(|| {
        panic!(
            "TOWER_RENAME_PERF_ROOT prerequisite failed: set it to an existing persistent-disk directory"
        )
    });
    let root = PathBuf::from(configured);
    let canonical_root = root.canonicalize().unwrap_or_else(|error| {
        panic!(
            "TOWER_RENAME_PERF_ROOT prerequisite failed: cannot canonicalize {}: {error}",
            root.display()
        )
    });
    assert!(
        canonical_root.is_dir(),
        "TOWER_RENAME_PERF_ROOT prerequisite failed: {} is not a directory",
        canonical_root.display()
    );
    let canonical_temp = std::env::temp_dir().canonicalize().unwrap_or_else(|error| {
        panic!("TOWER_RENAME_PERF_ROOT prerequisite failed: cannot canonicalize temp dir: {error}")
    });
    assert!(
        !canonical_root.starts_with(&canonical_temp),
        "TOWER_RENAME_PERF_ROOT prerequisite failed: {} is inside temporary directory {}",
        canonical_root.display(),
        canonical_temp.display()
    );

    let filesystem = identify_persistent_filesystem(&canonical_root);
    (canonical_root, filesystem)
}

#[cfg(target_os = "linux")]
fn identify_persistent_filesystem(root: &Path) -> String {
    let output = Command::new("stat")
        .args(["-f", "-c", "%T", "--"])
        .arg(root)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "TOWER_RENAME_PERF_ROOT prerequisite failed: could not execute stat for {}: {error}",
                root.display()
            )
        });
    assert!(
        output.status.success(),
        "TOWER_RENAME_PERF_ROOT prerequisite failed: stat failed for {}: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let filesystem = String::from_utf8(output.stdout)
        .expect("TOWER_RENAME_PERF_ROOT prerequisite failed: stat output was not UTF-8")
        .trim()
        .to_owned();
    assert!(
        matches!(filesystem.as_str(), "btrfs" | "ext2/ext3" | "xfs" | "zfs"),
        "TOWER_RENAME_PERF_ROOT prerequisite failed: filesystem {filesystem:?} is not an allowed persistent type (btrfs, ext2/ext3, xfs, zfs)"
    );
    filesystem
}

#[cfg(not(target_os = "linux"))]
fn identify_persistent_filesystem(root: &Path) -> String {
    panic!(
        "TOWER_RENAME_PERF_ROOT prerequisite failed: filesystem identification for {} requires Linux stat",
        root.display()
    )
}

fn seed_performance_workspace(root: &Path, path_count: usize) {
    for shard in 0..100 {
        let directory = root.join(format!("indexed/{shard:03}"));
        fs::create_dir_all(&directory).expect("create benchmark shard");
        for entry in 0..(path_count / 100) {
            let ordinal = shard * (path_count / 100) + entry;
            fs::write(
                directory.join(format!("path-{ordinal:06}.rs")),
                format!("pub const VALUE_{ordinal}: usize = {ordinal};\n"),
            )
            .expect("seed benchmark path");
        }
    }
}

#[test]
#[ignore]
fn native_rename_records_real_disk_100k_path_p95() {
    const PATH_COUNT: usize = 100_000;
    const WARM_UP_COUNT: usize = 10;
    const MEASURED_COUNT: usize = 30;

    let (operator_root, filesystem) = validated_performance_root();
    let workspace = BenchmarkWorkspace::create(&operator_root);
    seed_performance_workspace(&workspace.path, PATH_COUNT);
    let extensions_dir = workspace.path.join(".tower/perf-extensions");
    fs::create_dir_all(&extensions_dir).expect("create empty benchmark extension directory");
    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path.clone()),
        extensions_dir: Some(extensions_dir),
    };
    let handle = build_engine(&opts, TowerConfig::default()).expect("build benchmark engine");
    let mut registry = ExtensionMergedRegistry::with_rename_coordinator(
        Arc::clone(&handle.state),
        handle.ext_registry.clone(),
        Arc::clone(&handle.rename_coordinator),
    );
    let indexed = registry
        .call(
            "tower_list_dir",
            json!({"path": "", "recursive": true}),
            &core_engine::domain::NeverCancelled,
        )
        .expect("list indexed benchmark paths");
    let indexed_file_count = indexed["entries"]
        .as_array()
        .expect("list entries array")
        .iter()
        .filter(|entry| entry["kind"] == "file")
        .count();
    assert_eq!(indexed_file_count, PATH_COUNT);

    for ordinal in 0..WARM_UP_COUNT {
        let source = format!("indexed/000/path-{ordinal:06}.rs");
        let destination = format!("indexed/000/warm-{ordinal:06}.rs");
        let result = registry
            .call(
                "tower_rename_path",
                json!({
                    "source": source,
                    "destination": destination,
                    "semantic_policy": "none",
                    "expected_versions": {}
                }),
                &core_engine::domain::NeverCancelled,
            )
            .expect("warm-up public native rename dispatch");
        assert_applied(&result);
    }

    let mut samples = Vec::with_capacity(MEASURED_COUNT);
    for ordinal in WARM_UP_COUNT..(WARM_UP_COUNT + MEASURED_COUNT) {
        let source = format!("indexed/000/path-{ordinal:06}.rs");
        let destination = format!("indexed/000/measured-{ordinal:06}.rs");
        let started = Instant::now();
        let result = registry
            .call(
                "tower_rename_path",
                json!({
                    "source": source,
                    "destination": destination,
                    "semantic_policy": "none",
                    "expected_versions": {}
                }),
                &core_engine::domain::NeverCancelled,
            )
            .expect("measured public native rename dispatch");
        let elapsed_micros = u64::try_from(started.elapsed().as_micros())
            .expect("rename duration fits in integer microseconds");
        assert_applied(&result);
        samples.push(elapsed_micros);
    }

    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let minimum = sorted[0];
    let median = (sorted[MEASURED_COUNT / 2 - 1] + sorted[MEASURED_COUNT / 2]) / 2;
    let p95_index = (95 * MEASURED_COUNT).div_ceil(100) - 1;
    let p95 = sorted[p95_index];

    println!("benchmark_root={}", operator_root.display());
    println!("filesystem_type={filesystem}");
    println!("path_count={PATH_COUNT}");
    println!("warm_up_count={WARM_UP_COUNT}");
    println!("measured_count={MEASURED_COUNT}");
    println!("raw_microseconds={samples:?}");
    println!("min_microseconds={minimum}");
    println!("median_microseconds={median}");
    println!("p95_microseconds={p95}");

    assert_eq!(samples.len(), MEASURED_COUNT);
    assert!(
        p95 <= 500_000,
        "native rename p95 {p95}us exceeded 500000us; raw samples: {samples:?}"
    );
}
