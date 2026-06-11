//! End-to-end integration test for the `plugin_ast` wasm plugin (specs 12c + 12d).
//!
//! # What this verifies
//!
//! **Spec 12c (outline)**
//! - The `plugin_ast.wasm` built from the `plugin_ast` crate loads via the 11c
//!   wasmtime loader.
//! - The manifest declares both `ast_get_outline` and `ast_find_symbols` as tools.
//! - `ast_get_outline` through `MergedRegistry` returns structural items for Rust.
//! - `ast_get_outline` for a non-Rust file returns a typed unsupported-language result.
//! - `ast_get_outline` for a malformed Rust file returns a partial outline (no crash).
//!
//! **Spec 12d (symbols + multi-language — Drop & Play gate, AC5)**
//! - Dropping the rebuilt `.wasm` with no host recompile: `tools/list` exposes
//!   `ast/ast_get_outline` AND `ast/ast_find_symbols`.
//! - `ast_find_symbols` round-trips through the MCP sandbox for Rust, Go, and PHP:
//!   definition found, comment/string false-positives excluded.
//! - A `kind` not applicable to the language returns an empty `matches` list (OP1/AC3).
//! - A malformed file returns partial results without crashing (UN1/AC4).
//!
//! # Drop & Play proof (AC5)
//!
//! The test binary is compiled against the `core_engine` **host** (no plugin source
//! dependency). The `PLUGIN_AST_WASM` env var points to the built `.wasm` artefact
//! which is loaded at runtime. The host is never recompiled when only the plugin
//! changes — the assertions below verify both tools appear in `tools/list` and both
//! produce correct results purely via the wasm ABI.
//!
//! # Fixture path
//!
//! `PLUGIN_AST_WASM` is set by `core_engine`'s `build.rs`. The CI workflow builds
//! `plugin_ast --target wasm32-wasip1` with the WASI SDK env vars before running
//! `cargo test`, so the `.wasm` is always present when this test runs.
//!
//! # File reads through the host capability
//!
//! The plugin calls `host::read_file(path)`. We inject an `InMemoryFs` populated
//! with the fixture content so the test is deterministic and hermetic (no real disk).

use std::sync::{Arc, RwLock};

use core_engine::adapters::mcp::merged_registry::MergedRegistry;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::adapters::mcp::registry::ToolRegistry;
use core_engine::adapters::{InMemoryFs, InMemoryStorage, WasmtimeHost};
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::plugin_host::PluginHostRegistry;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::domain::RelativePath;
use core_engine::ports::FileSystemPort;

// ── Fixture path (set by build.rs) ────────────────────────────────────────────

fn plugin_ast_wasm() -> &'static str {
    env!("PLUGIN_AST_WASM")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Rust source fixture exercising functions, structs, impls, and traits.
const RUST_SOURCE: &[u8] = br#"
use std::fmt;

pub struct Counter {
    value: u32,
}

impl Counter {
    pub fn new() -> Self {
        Self { value: 0 }
    }

    pub fn increment(&mut self) {
        self.value += 1;
    }

    pub fn get(&self) -> u32 {
        self.value
    }
}

pub trait Resetable {
    fn reset(&mut self);
}

impl Resetable for Counter {
    fn reset(&mut self) {
        self.value = 0;
    }
}

pub fn make_counter() -> Counter {
    Counter::new()
}
"#;

/// Helper: populate an `InMemoryFs` with a file at `path` and return it wrapped in Arc.
fn fs_with(path: &str, content: &[u8]) -> Arc<InMemoryFs> {
    let mut fs = InMemoryFs::new();
    fs.write(RelativePath::new(path), content.to_vec())
        .expect("write must succeed");
    Arc::new(fs)
}

/// Build a `MergedRegistry` with the `plugin_ast` plugin registered.
///
/// `fs` is the filesystem the plugin will use for `host::read_file` calls.
///
/// A second empty `EngineState` is wired for the native tool side (not used by
/// these tests — we only exercise the plugin tool path).
fn merged_registry_with_plugin_ast(fs: Arc<InMemoryFs>) -> MergedRegistry {
    let fs_port: Arc<dyn core_engine::ports::FileSystemPort + Send + Sync> = fs;
    let instance = WasmtimeHost::load(plugin_ast_wasm(), fs_port).expect("plugin_ast must load");

    let mut plugin_registry = PluginHostRegistry::new();
    plugin_registry
        .register(instance)
        .expect("ast plugin must register");

    // Native tool side: empty state (not exercised by these plugin-only tests).
    let engine_state = Arc::new(RwLock::new(EngineState::new(
        ProjectWorkspace::new(),
        InvertedIndex::new(),
        Box::new(InMemoryStorage::new()),
        Box::new(InMemoryFs::new()),
    )));
    MergedRegistry::new(engine_state, Arc::new(RwLock::new(plugin_registry)))
}

// ── AC4/AC5: plugin loads and declares both ast tools ────────────────────────

/// AC4/AC5 (Drop & Play): Given the built `plugin_ast.wasm`, When loaded via
/// `WasmtimeHost::load` with no host recompile, Then the manifest declares
/// both `ast_get_outline` and `ast_find_symbols` as tools.
///
/// This is the Drop & Play gate for spec 12d (AC5): the host binary is not
/// recompiled; only the `.wasm` artefact is swapped. Both tools appear in the
/// manifest, proving the plugin ABI surface is forward-compatible.
#[test]
fn ac4_ac5_plugin_ast_loads_and_declares_both_tools() {
    let instance =
        WasmtimeHost::load(plugin_ast_wasm(), Arc::new(InMemoryFs::new())).expect("must load");

    let manifest = instance.manifest();
    assert_eq!(manifest.name, "ast", "plugin name must be 'ast'");
    assert_eq!(manifest.abi, plugin_sdk::ABI_VERSION, "ABI must match");
    assert_eq!(
        manifest.tools.len(),
        2,
        "manifest must declare exactly 2 tools"
    );

    let tool_names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"ast_get_outline"),
        "manifest must have ast_get_outline, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"ast_find_symbols"),
        "manifest must have ast_find_symbols, got: {tool_names:?}"
    );
    assert!(manifest.hooks.is_empty(), "no hooks declared");
}

/// AC5 (Drop & Play): `tools/list` via `MergedRegistry` exposes both namespaced
/// AST tools without any host recompile — only the `.wasm` is swapped.
#[test]
fn ac5_tools_list_exposes_both_ast_tools() {
    let fs = Arc::new(InMemoryFs::new());
    let registry = merged_registry_with_plugin_ast(fs);

    let tools = registry.list();
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    assert!(
        tool_names.contains(&"ast/ast_get_outline"),
        "AC5: tools/list must contain ast/ast_get_outline, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"ast/ast_find_symbols"),
        "AC5: tools/list must contain ast/ast_find_symbols, got: {tool_names:?}"
    );
}

// ── AC1: outline returns structural items for a Rust file ────────────────────

/// AC1: Given a Rust file with functions/structs/impls/traits, When
/// `ast/ast_get_outline` is called via the MergedRegistry, Then each item is
/// listed with its kind, name, and location, and no raw body text is returned.
#[test]
fn ac1_ast_get_outline_rust_file_returns_structural_items() {
    let fs = fs_with("src/counter.rs", RUST_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({ "path": "src/counter.rs" });
    let result = registry
        .call("ast/ast_get_outline", args)
        .expect("ac1: ast_get_outline must succeed");

    // The result is serde_json::Value. Convert to string to inspect.
    let result_str = result.to_string();

    // Verify key structural items appear.
    assert!(
        result_str.contains("\"Counter\""),
        "AC1: Counter struct must appear, got: {result_str}"
    );
    assert!(
        result_str.contains("\"make_counter\""),
        "AC1: make_counter function must appear, got: {result_str}"
    );
    assert!(
        result_str.contains("\"Resetable\""),
        "AC1: Resetable trait must appear, got: {result_str}"
    );

    // Verify 'items' key exists (expected shape: { "items": [...] }).
    assert!(
        result_str.contains("\"items\""),
        "AC1: result must have 'items' key, got: {result_str}"
    );

    // U1: no raw body text — check that multi-line content does not appear as a name.
    // A body like "{ self.value += 1; }" should not be a name value.
    assert!(
        !result_str.contains("self.value += 1"),
        "U1: raw body text must not appear in outline names"
    );
}

/// AC1 extension: verify span fields are present in each item.
#[test]
fn ac1_outline_items_have_span_fields() {
    let fs = fs_with("src/lib.rs", RUST_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({ "path": "src/lib.rs" });
    let result = registry
        .call("ast/ast_get_outline", args)
        .expect("ac1: ast_get_outline must succeed");

    let result_str = result.to_string();

    // Each item must have span fields.
    for key in &["start_byte", "end_byte", "start_row"] {
        assert!(
            result_str.contains(format!("\"{key}\"").as_str()),
            "AC1: span field '{key}' must be in result, got: {result_str}"
        );
    }
}

// ── AC2: unsupported language returns typed result ───────────────────────────

/// AC2: Given a non-Rust file, When `ast/ast_get_outline` is called, Then a
/// typed unsupported-language result is returned (not an error).
#[test]
fn ac2_non_rust_file_returns_unsupported_result() {
    let fs = fs_with("hello.py", b"def hello(): pass");
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({ "path": "hello.py" });
    // Must succeed (not error): returns unsupported result, not SdkError.
    let result = registry
        .call("ast/ast_get_outline", args)
        .expect("AC2: call must succeed even for unsupported language");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"unsupported\""),
        "AC2: unsupported-language result must have 'unsupported' key, got: {result_str}"
    );
    assert!(
        result_str.contains("\"py\""),
        "AC2: unsupported result must include the language extension, got: {result_str}"
    );
}

// ── AC3: malformed Rust file yields partial outline ───────────────────────────

/// AC3: Given a syntactically broken Rust file, When `ast/ast_get_outline` is
/// called, Then a partial outline returns without crash.
#[test]
fn ac3_malformed_rust_file_returns_partial_outline_no_crash() {
    // A file with valid items BEFORE the break survives error recovery.
    let broken_source = b"
pub fn good_fn() {}
fn broken_fn( {
";
    let fs = fs_with("broken.rs", broken_source);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({ "path": "broken.rs" });
    let result = registry
        .call("ast/ast_get_outline", args)
        .expect("AC3: malformed file must not return error — partial outline expected");

    let result_str = result.to_string();
    // Must have 'items' key (partial outline), not 'unsupported'.
    assert!(
        result_str.contains("\"items\""),
        "AC3: partial outline must have 'items' key, got: {result_str}"
    );
    assert!(
        !result_str.contains("\"unsupported\""),
        "AC3: broken Rust must not be treated as unsupported language"
    );
    // The good_fn before the break must appear.
    assert!(
        result_str.contains("\"good_fn\""),
        "AC3: good_fn before broken syntax must survive, got: {result_str}"
    );
}

// ── U2: plugin reads only through the host capability ────────────────────────

/// U2: The plugin reads file content only through the injected `InMemoryFs`
/// (the host capability) — not raw `std::fs`.
///
/// Verified implicitly by all tests above: they inject an `InMemoryFs` with
/// only the specific file content, so a raw-fs read would fail or return
/// different data. This test makes the property explicit: an empty `InMemoryFs`
/// causes `host_read_file` to return not_found, and the tool returns
/// `SdkError::CallFailed` (mapped to a ToolError by MergedRegistry).
#[test]
fn u2_empty_fs_causes_call_failed_not_raw_fs_read() {
    // Empty InMemoryFs: the plugin cannot read any file.
    let mut registry = merged_registry_with_plugin_ast(Arc::new(InMemoryFs::new()));

    let args = serde_json::json!({ "path": "src/lib.rs" });
    let result = registry.call("ast/ast_get_outline", args);

    // Must be an error (CallFailed path), not a valid outline read from the host FS.
    assert!(
        result.is_err(),
        "U2: empty InMemoryFs must cause an error, not read from host filesystem"
    );
}

// ── Spec 12d: ast_find_symbols e2e (AC1/AC2/AC3/AC4/AC5) ────────────────────
//
// All tests below exercise ast_find_symbols via the wasm sandbox through
// MergedRegistry — this is the Drop & Play gate (AC5): only the .wasm changes,
// the host binary is never recompiled.

/// Rust source for ast_find_symbols tests: includes definition + comment/string
/// occurrences of the same name to prove false-positive exclusion (AC1/U2).
const RUST_SYMBOLS_SOURCE: &[u8] = br#"
// FindMe is mentioned in this comment
pub struct FindMe {
    value: u32,
}

/// FindMe also in a doc comment
impl FindMe {
    /// find_me_method also in this doc
    pub fn find_me_method() -> Self {
        // find_me_method in body comment
        Self { value: 0 }
    }
}

pub fn find_me_fn() -> u32 {
    // find_me_fn mentioned in body comment
    42
}

static FIND_ME_STATIC: &str = "FindMe is in string too";
"#;

/// Go source for multi-language e2e tests (AC2).
const GO_SYMBOLS_SOURCE: &[u8] = br#"package main

// FindMe is in a comment
func FindMe(x int) int {
    // FindMe in body comment
    return x
}

func (s MyStruct) FindMeMethod() {}

type MyStruct struct {
    Field int
}

type MyInterface interface {
    Method() error
}
"#;

/// PHP source for multi-language e2e tests (AC2).
const PHP_SYMBOLS_SOURCE: &[u8] = br#"<?php

// FindMe is in a comment
class FindMe {
    // FindMe in body
    public function findMeMethod() {}
}

interface FindMeInterface {}

function findMeFn() {}
"#;

/// AC1 (12d): Rust struct definition found through wasm sandbox; comment and
/// string occurrences do NOT produce additional matches (false-positive exclusion).
#[test]
fn ac1_12d_rust_find_symbols_struct_no_false_positives() {
    let fs = fs_with("src/lib.rs", RUST_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "src/lib.rs",
        "symbol_name": "FindMe",
        "kind": "struct"
    });
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC1: ast_find_symbols must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"matches\""),
        "AC1: result must have 'matches' key, got: {result_str}"
    );

    // Deserialize to count matches precisely.
    let matches = result["matches"]
        .as_array()
        .expect("AC1: 'matches' must be an array");
    assert_eq!(
        matches.len(),
        1,
        "AC1: exactly one FindMe struct definition; comment/string must not match. got: {result_str}"
    );
    assert_eq!(
        matches[0]["kind"].as_str().unwrap_or(""),
        "struct",
        "AC1: match kind must be 'struct'"
    );
    assert_eq!(
        matches[0]["name"].as_str().unwrap_or(""),
        "FindMe",
        "AC1: match name must be 'FindMe'"
    );
}

/// AC1 (12d): Rust method found; free function with same root name not matched.
#[test]
fn ac1_12d_rust_find_symbols_method_not_free_function() {
    let fs = fs_with("src/lib.rs", RUST_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "src/lib.rs",
        "symbol_name": "find_me_method",
        "kind": "method"
    });
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC1: ast_find_symbols method must succeed");

    let matches = result["matches"]
        .as_array()
        .expect("'matches' must be array");
    assert_eq!(
        matches.len(),
        1,
        "AC1: exactly one find_me_method, got: {result:?}"
    );
    assert_eq!(matches[0]["kind"].as_str().unwrap_or(""), "method");
}

/// AC1 (12d): span fields are present in symbol matches.
#[test]
fn ac1_12d_rust_find_symbols_match_has_span_fields() {
    let fs = fs_with("src/lib.rs", RUST_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "src/lib.rs",
        "symbol_name": "FindMe",
        "kind": "struct"
    });
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("must succeed");

    let result_str = result.to_string();
    for key in &["start_byte", "end_byte", "start_row"] {
        assert!(
            result_str.contains(format!("\"{key}\"").as_str()),
            "AC1: span field '{key}' must be present, got: {result_str}"
        );
    }
}

/// AC2 (12d): Go function definition found through wasm sandbox.
#[test]
fn ac2_12d_go_find_symbols_function_definition() {
    let fs = fs_with("main.go", GO_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "main.go",
        "symbol_name": "FindMe",
        "kind": "function"
    });
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC2: Go find_symbols must succeed");

    let matches = result["matches"]
        .as_array()
        .expect("'matches' must be array");
    assert_eq!(
        matches.len(),
        1,
        "AC2: one FindMe function in Go, got: {result:?}"
    );
    assert_eq!(matches[0]["kind"].as_str().unwrap_or(""), "function");
    assert_eq!(matches[0]["name"].as_str().unwrap_or(""), "FindMe");
}

/// AC2 (12d): Go method definition found.
#[test]
fn ac2_12d_go_find_symbols_method_definition() {
    let fs = fs_with("main.go", GO_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "main.go",
        "symbol_name": "FindMeMethod",
        "kind": "method"
    });
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC2: Go method must succeed");

    let matches = result["matches"]
        .as_array()
        .expect("'matches' must be array");
    assert_eq!(
        matches.len(),
        1,
        "AC2: one FindMeMethod in Go, got: {result:?}"
    );
    assert_eq!(matches[0]["kind"].as_str().unwrap_or(""), "method");
}

/// AC2 (12d): Go struct type found.
#[test]
fn ac2_12d_go_find_symbols_struct_type() {
    let fs = fs_with("main.go", GO_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "main.go",
        "symbol_name": "MyStruct",
        "kind": "struct"
    });
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC2: Go struct must succeed");

    let matches = result["matches"]
        .as_array()
        .expect("'matches' must be array");
    assert_eq!(matches.len(), 1, "AC2: one MyStruct in Go, got: {result:?}");
}

/// AC2 (12d): Go outline (ast_get_outline) returns functions, methods, structs,
/// and interfaces for a Go file — multi-language outline support (U1).
#[test]
fn ac2_12d_go_outline_returns_structural_items() {
    let fs = fs_with("main.go", GO_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({ "path": "main.go" });
    let result = registry
        .call("ast/ast_get_outline", args)
        .expect("AC2: Go outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"items\""),
        "AC2: Go outline must have 'items', got: {result_str}"
    );
    assert!(
        result_str.contains("\"FindMe\""),
        "AC2: Go outline must contain FindMe, got: {result_str}"
    );
    assert!(
        result_str.contains("\"MyStruct\""),
        "AC2: Go outline must contain MyStruct, got: {result_str}"
    );
}

/// AC2 (12d): PHP class definition found through wasm sandbox.
#[test]
fn ac2_12d_php_find_symbols_class_definition() {
    let fs = fs_with("app.php", PHP_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "app.php",
        "symbol_name": "FindMe",
        "kind": "class"
    });
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC2: PHP class find must succeed");

    let matches = result["matches"]
        .as_array()
        .expect("'matches' must be array");
    assert_eq!(
        matches.len(),
        1,
        "AC2: one FindMe class in PHP, got: {result:?}"
    );
    assert_eq!(matches[0]["kind"].as_str().unwrap_or(""), "class");
    assert_eq!(matches[0]["name"].as_str().unwrap_or(""), "FindMe");
}

/// AC2 (12d): PHP outline (ast_get_outline) returns class, methods, and function
/// for a PHP file — multi-language outline support (U1).
#[test]
fn ac2_12d_php_outline_returns_structural_items() {
    let fs = fs_with("app.php", PHP_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({ "path": "app.php" });
    let result = registry
        .call("ast/ast_get_outline", args)
        .expect("AC2: PHP outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"items\""),
        "AC2: PHP outline must have 'items', got: {result_str}"
    );
    assert!(
        result_str.contains("\"FindMe\""),
        "AC2: PHP outline must contain FindMe class, got: {result_str}"
    );
    assert!(
        result_str.contains("\"findMeMethod\""),
        "AC2: PHP outline must contain findMeMethod, got: {result_str}"
    );
}

/// AC3 (12d): kind not applicable to Go returns `{ "matches": [] }` — not an error.
///
/// OP1: `enum` does not exist in Go; the tool returns empty matches, no error.
#[test]
fn ac3_12d_go_not_applicable_kind_returns_empty_matches() {
    let fs = fs_with("main.go", GO_SYMBOLS_SOURCE);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "main.go",
        "symbol_name": "anything",
        "kind": "enum"   // enum is not applicable in Go
    });
    // Must succeed (OP1/AC3 — not an error).
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC3: not-applicable kind must not return an error");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"matches\""),
        "AC3: result must have 'matches' key, got: {result_str}"
    );
    let matches = result["matches"]
        .as_array()
        .expect("'matches' must be array");
    assert!(
        matches.is_empty(),
        "AC3: not-applicable kind must return empty matches in Go, got: {result_str}"
    );
}

/// AC4 (12d): malformed Rust file returns partial symbol results without crashing.
#[test]
fn ac4_12d_malformed_rust_returns_partial_without_crash() {
    let broken = b"pub struct Good {} fn broken( { ";
    let fs = fs_with("broken.rs", broken);
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({
        "path": "broken.rs",
        "symbol_name": "Good",
        "kind": "struct"
    });
    // Must not crash or return a system error — partial results are acceptable.
    let result = registry
        .call("ast/ast_find_symbols", args)
        .expect("AC4: malformed file must not crash the sandbox");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"matches\""),
        "AC4: malformed file must return 'matches', got: {result_str}"
    );
}

// ── BUG-01: zero-byte source file must not trap ───────────────────────────────

/// BUG-01 regression: A zero-byte file with a supported extension (.rs) must
/// return `{ "items": [] }` from `ast/ast_get_outline`, NOT a -32603 MCP error
/// (wasm trap). Root cause was the guest SDK treating rc=0 + null ptr as a
/// missing-file error regardless of out_len; an empty file legitimately returns
/// rc=0, out_len=0, and a null pointer. Fixed in `plugin_sdk::host::interpret_read`.
#[test]
fn bug01_zero_byte_rust_file_returns_empty_items_not_trap() {
    // A zero-byte .rs file: empty content, supported extension.
    let fs = fs_with("empty.rs", b"");
    let mut registry = merged_registry_with_plugin_ast(fs);

    let args = serde_json::json!({ "path": "empty.rs" });
    let result = registry
        .call("ast/ast_get_outline", args)
        .expect("BUG-01: zero-byte file must not produce a wasm trap / MCP -32603");

    let result_str = result.to_string();

    // Must return the outline shape with an empty items list — not an error.
    assert!(
        result_str.contains("\"items\""),
        "BUG-01: zero-byte file must return 'items' key, got: {result_str}"
    );

    let items = result["items"]
        .as_array()
        .expect("BUG-01: 'items' must be a JSON array");
    assert!(
        items.is_empty(),
        "BUG-01: zero-byte file must yield empty items list, got: {result_str}"
    );
}
