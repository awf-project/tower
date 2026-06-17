//! Integration tests for the production plugin runtime wiring (drop & play).
//!
//! These verify the glue that the `tower` binary uses at startup:
//! discover `*.wasm` in a plugins directory, load each through the 11c/11d
//! isolated-sandbox path, register them in a [`PluginHostRegistry`], and serve
//! the result through a [`MergedRegistry`] alongside the 8 native `tower_*` tools.
//!
//! # What is exercised
//!
//! - A real `ast.wasm` dropped in the dir → `tools/list` exposes the 8
//!   native tools PLUS `tower_ast_get_outline` and `tower_ast_find_symbols`, and a
//!   `tools/call` to `tower_ast_get_outline` round-trips.
//! - A malformed `.wasm` and an ABI-mismatched `.wasm` are skipped; startup
//!   succeeds and still serves the rest.
//! - No plugins dir / empty plugins dir → exactly the 8 native tools.
//!
//! # Hermetic file reads
//!
//! The plugin reads source through the injected `FileSystemPort`. We inject an
//! `InMemoryFs` populated with the fixture so the test does not touch real disk
//! for plugin reads. The `.wasm` artefacts themselves are copied into a
//! `tempfile::TempDir` (real disk, but isolated and auto-cleaned).
//!
//! # Fixture paths (set by build.rs)
//!
//! `PLUGIN_AST_WASM` and `ABI_MISMATCH_WASM` point at artefacts built for
//! `wasm32-wasip1` before `cargo test` runs.

use std::path::Path;
use std::sync::{Arc, RwLock};

use core_engine::adapters::mcp::merged_registry::MergedRegistry;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::plugin::IsolationEngine;
use core_engine::adapters::plugin::runtime::{
    load_plugins_into_registry, production_isolation_config,
};
use core_engine::adapters::{HostDeps, InMemoryAstIndex, InMemoryFs, InMemoryStorage};
use core_engine::domain::RelativePath;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::plugin_host::PluginHostRegistry;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::FileSystemPort;

// ── Fixture paths ───────────────────────────────────────────────────────────────

fn ast_wasm() -> &'static str {
    env!("PLUGIN_AST_WASM")
}

fn abi_mismatch_wasm() -> &'static str {
    env!("ABI_MISMATCH_WASM")
}

/// Rust source the AST plugin will parse via `host::read_file`.
const RUST_SOURCE: &[u8] = br#"
pub struct Counter {
    value: u32,
}

pub fn make_counter() -> Counter {
    Counter { value: 0 }
}
"#;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Filesystem the plugin reads through: one Rust file at `src/lib.rs`.
fn fs_with_rust_source() -> Arc<dyn FileSystemPort + Send + Sync> {
    let mut fs = InMemoryFs::new();
    fs.write(RelativePath::new("src/lib.rs"), RUST_SOURCE.to_vec())
        .expect("write must succeed");
    Arc::new(fs)
}

/// Empty native engine state — gives the 8 native tools without indexed files.
fn empty_engine_state() -> Arc<RwLock<EngineState>> {
    Arc::new(RwLock::new(EngineState::new(
        ProjectWorkspace::new(),
        InvertedIndex::new(),
        Box::new(InMemoryStorage::new()),
        Box::new(InMemoryFs::new()),
    )))
}

/// Build a `MergedRegistry` from a populated plugin registry.
fn merged(registry: PluginHostRegistry) -> MergedRegistry {
    MergedRegistry::new(empty_engine_state(), Arc::new(RwLock::new(registry)))
}

/// Copy a built `.wasm` artefact into `dir` under `file_name`.
fn drop_wasm(dir: &Path, file_name: &str, src: &str) {
    std::fs::copy(src, dir.join(file_name))
        .unwrap_or_else(|e| panic!("copy {src} -> {file_name}: {e}"));
}

fn names(registry: &MergedRegistry) -> Vec<String> {
    registry.list().into_iter().map(|t| t.name).collect()
}

fn empty_deps() -> HostDeps {
    HostDeps {
        fs: Arc::new(InMemoryFs::new()),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        workspace: Arc::new(RwLock::new(ProjectWorkspace::new())),
        format_queue: Arc::new(core_engine::adapters::formatter::NoOpFormatQueue),
    }
}

fn deps_with_rust_source() -> HostDeps {
    HostDeps {
        fs: fs_with_rust_source(),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        workspace: Arc::new(RwLock::new(ProjectWorkspace::new())),
        format_queue: Arc::new(core_engine::adapters::formatter::NoOpFormatQueue),
    }
}

// ── AC: ast dropped in the dir → native + ast tools listed ───────────────

#[test]
fn ast_plugin_dir_exposes_native_and_ast_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "ast.wasm", ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[tmp.path().to_path_buf()],
        &engine,
        deps_with_rust_source(),
        production_isolation_config(),
        &[],
    );
    let merged = merged(registry);
    let listed = names(&merged);

    for native in &[
        "tower_find_file",
        "tower_search_text",
        "tower_read_file",
        "tower_create_file",
        "tower_create_directory",
        "tower_delete_file",
        "tower_global_replace",
    ] {
        assert!(
            listed.iter().any(|n| n == native),
            "missing native {native}: {listed:?}"
        );
    }
    assert!(
        listed.iter().any(|n| n == "tower_ast_get_outline"),
        "missing tower_ast_get_outline: {listed:?}"
    );
    assert!(
        listed.iter().any(|n| n == "tower_ast_find_symbols"),
        "missing tower_ast_find_symbols: {listed:?}"
    );
}

#[test]
fn ast_get_outline_round_trips_through_merged_registry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "ast.wasm", ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[tmp.path().to_path_buf()],
        &engine,
        deps_with_rust_source(),
        production_isolation_config(),
        &[],
    );
    let mut merged = merged(registry);

    let result = merged
        .call(
            "tower_ast_get_outline",
            serde_json::json!({ "path": "src/lib.rs" }),
        )
        .expect("get_outline must succeed");
    let text = result.to_string();
    assert!(text.contains("\"items\""), "expected outline items: {text}");
    assert!(
        text.contains("\"Counter\""),
        "expected Counter symbol: {text}"
    );
    assert!(
        text.contains("\"make_counter\""),
        "expected make_counter symbol: {text}"
    );
}

// ── AC: malformed wasm is skipped; the rest is still served ──────────────────────

#[test]
fn malformed_wasm_is_skipped_rest_served() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Garbage bytes with a .wasm extension — fails Module::from_file.
    std::fs::write(tmp.path().join("broken.wasm"), b"this is not wasm at all")
        .expect("write broken wasm");
    drop_wasm(tmp.path(), "ast.wasm", ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[tmp.path().to_path_buf()],
        &engine,
        deps_with_rust_source(),
        production_isolation_config(),
        &[],
    );
    let merged = merged(registry);
    let listed = names(&merged);

    // The good plugin survived; the broken one was skipped (8 native + 5 ast).
    assert_eq!(
        listed.len(),
        13,
        "expected 8 native + 5 ast tools after skipping broken wasm: {listed:?}"
    );
    assert!(listed.iter().any(|n| n == "tower_ast_get_outline"));
}

// ── AC: ABI-mismatched wasm is skipped; the rest is still served ─────────────────

#[test]
fn abi_mismatch_wasm_is_skipped_rest_served() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "abi_mismatch.wasm", abi_mismatch_wasm());
    drop_wasm(tmp.path(), "ast.wasm", ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[tmp.path().to_path_buf()],
        &engine,
        deps_with_rust_source(),
        production_isolation_config(),
        &[],
    );
    let merged = merged(registry);
    let listed = names(&merged);

    assert!(
        listed.iter().any(|n| n == "tower_ast_get_outline"),
        "good plugin must survive abi-mismatch sibling: {listed:?}"
    );
    // No tool from the abi-mismatch fixture leaked in (its name is not "ast").
    assert_eq!(
        listed.len(),
        13,
        "expected 8 native + 5 ast tools; abi-mismatch must be skipped: {listed:?}"
    );
}

// ── AC: no plugins dir / empty dir → exactly the 8 native tools ──────────────────

#[test]
fn missing_plugins_dir_serves_only_native_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("does-not-exist");

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[missing],
        &engine,
        empty_deps(),
        production_isolation_config(),
        &[],
    );
    let merged = merged(registry);
    assert_eq!(merged.list().len(), 8, "missing dir → 8 native tools only");
}

#[test]
fn empty_plugins_dir_serves_only_native_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[tmp.path().to_path_buf()],
        &engine,
        empty_deps(),
        production_isolation_config(),
        &[],
    );
    let merged = merged(registry);
    assert_eq!(merged.list().len(), 8, "empty dir → 8 native tools only");
}

// ── AC: global scope reachable from a project with an empty local scope ──────────

#[test]
fn global_plugin_reachable_with_empty_local_scope() {
    let global = tempfile::tempdir().expect("global tempdir");
    let local = tempfile::tempdir().expect("local tempdir");
    drop_wasm(global.path(), "ast.wasm", ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    // dirs order = [global, local]; local is empty.
    let registry = load_plugins_into_registry(
        &[global.path().to_path_buf(), local.path().to_path_buf()],
        &engine,
        deps_with_rust_source(),
        production_isolation_config(),
        &[],
    );
    let merged = merged(registry);
    let listed = names(&merged);

    assert!(
        listed.iter().any(|n| n == "tower_ast_get_outline"),
        "a globally-installed plugin must be usable from a project: {listed:?}"
    );
    assert_eq!(listed.len(), 13, "8 native + 5 ast (global): {listed:?}");
}

// ── AC: same name in both scopes → local overrides global (collapses to one) ─────

#[test]
fn local_scope_overrides_global_same_name() {
    let global = tempfile::tempdir().expect("global tempdir");
    let local = tempfile::tempdir().expect("local tempdir");
    // Same plugin (manifest name "ast") present in BOTH scopes.
    drop_wasm(global.path(), "ast.wasm", ast_wasm());
    drop_wasm(local.path(), "ast.wasm", ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[global.path().to_path_buf(), local.path().to_path_buf()],
        &engine,
        deps_with_rust_source(),
        production_isolation_config(),
        &[],
    );
    let merged = merged(registry);
    let listed = names(&merged);

    // Shadowing collapsed the duplicate to a single "ast" plugin: 8 native + 5 ast,
    // NOT 8 + 10. The local copy won (verified deterministically by decide_shadowing
    // unit tests in runtime.rs).
    assert_eq!(
        listed.len(),
        13,
        "duplicate name across scopes must collapse to one plugin: {listed:?}"
    );
    assert_eq!(
        listed
            .iter()
            .filter(|n| n.starts_with("tower_ast_"))
            .count(),
        5,
        "exactly the 5 ast tools, not duplicated: {listed:?}"
    );
}

// ── AC: a plugin whose file stem is in `disabled` is never loaded ─────────

#[test]
fn disabled_plugin_contributes_no_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "hello.wasm", env!("HELLO_PLUGIN_WASM"));

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[tmp.path().to_path_buf()],
        &engine,
        empty_deps(),
        production_isolation_config(),
        &["hello".to_string()],
    );

    assert!(
        registry.declared_tools().is_empty(),
        "a plugin listed in `disabled` must contribute no tools"
    );
}

#[test]
fn non_disabled_plugin_loads_normally() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "hello.wasm", env!("HELLO_PLUGIN_WASM"));

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[tmp.path().to_path_buf()],
        &engine,
        empty_deps(),
        production_isolation_config(),
        &[],
    );

    assert!(
        !registry.declared_tools().is_empty(),
        "an enabled plugin must contribute at least one tool"
    );
}

/// The disabled filter applies in EVERY scope, not just the local one: a plugin
/// living in the higher-precedence-ordered (global) scope is still skipped when
/// its stem is in `disabled`. This guards the spec's "a project can disable a
/// globally-installed plugin" claim.
#[test]
fn disabled_plugin_skipped_in_non_local_scope() {
    let global = tempfile::tempdir().expect("global tempdir");
    let local = tempfile::tempdir().expect("local tempdir");
    // hello lives in the FIRST (global) scope; local is empty.
    drop_wasm(global.path(), "hello.wasm", env!("HELLO_PLUGIN_WASM"));

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &[global.path().to_path_buf(), local.path().to_path_buf()],
        &engine,
        empty_deps(),
        production_isolation_config(),
        &["hello".to_string()],
    );

    assert!(
        registry.declared_tools().is_empty(),
        "a disabled plugin must be skipped even when it lives in a non-local scope"
    );
}
