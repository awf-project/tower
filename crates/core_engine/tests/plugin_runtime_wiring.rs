//! Integration tests for the production plugin runtime wiring (drop & play).
//!
//! These verify the glue that the `tower` binary uses at startup:
//! discover `*.wasm` in a plugins directory, load each through the 11c/11d
//! isolated-sandbox path, register them in a [`PluginHostRegistry`], and serve
//! the result through a [`MergedRegistry`] alongside the 7 native `vfs_*` tools.
//!
//! # What is exercised
//!
//! - A real `plugin_ast.wasm` dropped in the dir → `tools/list` exposes the 7
//!   native tools PLUS `ast/ast_get_outline` and `ast/ast_find_symbols`, and a
//!   `tools/call` to `ast/ast_get_outline` round-trips.
//! - A malformed `.wasm` and an ABI-mismatched `.wasm` are skipped; startup
//!   succeeds and still serves the rest.
//! - No plugins dir / empty plugins dir → exactly the 7 native tools.
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
use core_engine::adapters::plugin::runtime::{
    load_plugins_into_registry, production_isolation_config,
};
use core_engine::adapters::plugin::IsolationEngine;
use core_engine::adapters::{InMemoryFs, InMemoryStorage};
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::plugin_host::PluginHostRegistry;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::domain::RelativePath;
use core_engine::ports::FileSystemPort;

// ── Fixture paths ───────────────────────────────────────────────────────────────

fn plugin_ast_wasm() -> &'static str {
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

/// Empty native engine state — gives the 7 native tools without indexed files.
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

// ── AC: plugin_ast dropped in the dir → native + ast tools listed ───────────────

#[test]
fn ast_plugin_dir_exposes_native_and_ast_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "plugin_ast.wasm", plugin_ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        tmp.path(),
        &engine,
        fs_with_rust_source(),
        production_isolation_config(),
    );
    let merged = merged(registry);
    let listed = names(&merged);

    for native in &[
        "vfs_find_file",
        "vfs_search_text",
        "vfs_read_file",
        "vfs_create_file",
        "vfs_create_directory",
        "vfs_delete_file",
        "vfs_global_replace",
    ] {
        assert!(
            listed.iter().any(|n| n == native),
            "missing native {native}: {listed:?}"
        );
    }
    assert!(
        listed.iter().any(|n| n == "ast/ast_get_outline"),
        "missing ast/ast_get_outline: {listed:?}"
    );
    assert!(
        listed.iter().any(|n| n == "ast/ast_find_symbols"),
        "missing ast/ast_find_symbols: {listed:?}"
    );
}

#[test]
fn ast_get_outline_round_trips_through_merged_registry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "plugin_ast.wasm", plugin_ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        tmp.path(),
        &engine,
        fs_with_rust_source(),
        production_isolation_config(),
    );
    let mut merged = merged(registry);

    let result = merged
        .call(
            "ast/ast_get_outline",
            serde_json::json!({ "path": "src/lib.rs" }),
        )
        .expect("ast_get_outline must succeed");
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
    drop_wasm(tmp.path(), "plugin_ast.wasm", plugin_ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        tmp.path(),
        &engine,
        fs_with_rust_source(),
        production_isolation_config(),
    );
    let merged = merged(registry);
    let listed = names(&merged);

    // The good plugin survived; the broken one was skipped (7 native + 2 ast).
    assert_eq!(
        listed.len(),
        9,
        "expected 7 native + 2 ast tools after skipping broken wasm: {listed:?}"
    );
    assert!(listed.iter().any(|n| n == "ast/ast_get_outline"));
}

// ── AC: ABI-mismatched wasm is skipped; the rest is still served ─────────────────

#[test]
fn abi_mismatch_wasm_is_skipped_rest_served() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop_wasm(tmp.path(), "abi_mismatch.wasm", abi_mismatch_wasm());
    drop_wasm(tmp.path(), "plugin_ast.wasm", plugin_ast_wasm());

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        tmp.path(),
        &engine,
        fs_with_rust_source(),
        production_isolation_config(),
    );
    let merged = merged(registry);
    let listed = names(&merged);

    assert!(
        listed.iter().any(|n| n == "ast/ast_get_outline"),
        "good plugin must survive abi-mismatch sibling: {listed:?}"
    );
    // No tool from the abi-mismatch fixture leaked in (its name is not "ast").
    assert_eq!(
        listed.len(),
        9,
        "expected 7 native + 2 ast tools; abi-mismatch must be skipped: {listed:?}"
    );
}

// ── AC: no plugins dir / empty dir → exactly the 7 native tools ──────────────────

#[test]
fn missing_plugins_dir_serves_only_native_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("does-not-exist");

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        &missing,
        &engine,
        Arc::new(InMemoryFs::new()),
        production_isolation_config(),
    );
    let merged = merged(registry);
    assert_eq!(merged.list().len(), 7, "missing dir → 7 native tools only");
}

#[test]
fn empty_plugins_dir_serves_only_native_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = IsolationEngine::new().expect("isolation engine");
    let registry = load_plugins_into_registry(
        tmp.path(),
        &engine,
        Arc::new(InMemoryFs::new()),
        production_isolation_config(),
    );
    let merged = merged(registry);
    assert_eq!(merged.list().len(), 7, "empty dir → 7 native tools only");
}
