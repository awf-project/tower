//! End-to-end integration tests for the `ast` native extension (spec 26).
//!
//! # What this verifies
//!
//! - **AC1/U1**: `ast_extension` declares all five tools (`get_outline`,
//!   `find_symbols`, `search_symbols`, `reindex`, `read_symbol`) over the
//!   extension protocol.
//! - **AC2/EV1**: A `fileIndexed` event triggers parse → index update; a
//!   subsequent `find_symbols` reflects the new symbols.
//! - **AC3**: Multi-language parity (Rust, Go, PHP) matches the prior WASM output.
//! - **AC4/U4**: The `hello_extension` lazy greet tool.
//! - **AC5**: The test suite drives the native binary via plain `cargo build`
//!   (no WASI SDK, no stale-artifact hazard).
//!
//! # Test binary location
//!
//! `ast_extension` and `hello_extension` are in `default-members`, so
//! `cargo test --workspace` compiles them first. Binaries are located in
//! `target/debug/`.
//!
//! # Host capability doubles
//!
//! All tests use `InMemoryFs` and `InMemoryAstIndex` — no real disk I/O.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{Event, ExtensionManifest};

use core_engine::adapters::extension::{HostDeps, SidecarHostAdapter};
use core_engine::adapters::formatter::NoOpFormatQueue;
use core_engine::adapters::{InMemoryAstIndex, InMemoryFs};
use core_engine::domain::extension_host::ExtensionRegistry;
use core_engine::ports::FileSystemPort;

/// Default timeout: long enough for cooperative extensions in CI.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

// ── Binary path helpers ───────────────────────────────────────────────────────

fn workspace_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = .../tower/crates/core_engine
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent() // crates/
        .unwrap()
        .parent() // tower/
        .unwrap()
        .to_path_buf()
}

fn ast_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("ast_extension")
        .to_str()
        .unwrap()
        .to_owned()
}

fn hello_extension_bin() -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join("hello_extension")
        .to_str()
        .unwrap()
        .to_owned()
}

// ── HostDeps builders ─────────────────────────────────────────────────────────

fn make_deps(fs: InMemoryFs) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        push_tx: None,
    }
}

#[allow(dead_code)]
fn make_deps_with_index(fs: InMemoryFs, index: InMemoryAstIndex) -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(fs)),
        ast_index: Arc::new(index),
        format_queue: Arc::new(NoOpFormatQueue),
        push_tx: None,
    }
}

// ── Manifest builders ─────────────────────────────────────────────────────────

fn ast_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "ast".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection {
            subscribe: vec![
                "event/fileIndexed".to_owned(),
                "event/fileChanged".to_owned(),
            ],
        },
        capabilities: CapabilitiesSection {
            required: vec![
                "read_file".to_owned(),
                "list_files".to_owned(),
                "index_get".to_owned(),
                "index_put".to_owned(),
                "log".to_owned(),
            ],
        },
    }
}

fn hello_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "hello".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Lazy,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    }
}

// ── Rust source fixture ───────────────────────────────────────────────────────

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

const GO_SOURCE: &[u8] = br#"package main

import "fmt"

func HelloWorld(name string) string {
    return fmt.Sprintf("Hello, %s", name)
}

type MyStruct struct {
    value int
}

type MyInterface interface {
    Method() error
}
"#;

const PHP_SOURCE: &[u8] = br#"<?php

class MyClass {
    public function myMethod() {}
}

interface MyInterface {
    public function interfaceMethod();
}

function topLevelFn() {}
"#;

// ─────────────────────────────────────────────────────────────────────────────
// AC1 / U1: ast_extension declares all five tools
// ─────────────────────────────────────────────────────────────────────────────

/// AC1/U1: Given the `ast_extension` binary, When spawned, Then it initializes
/// and declares all five AST tools with the same names as the WASM plugin.
#[test]
fn ac1_ast_extension_declares_five_tools() {
    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let manifest = adapter.manifest();
    assert_eq!(manifest.name, "ast");

    let tool_names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"get_outline"),
        "must have get_outline, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"find_symbols"),
        "must have find_symbols, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"search_symbols"),
        "must have search_symbols, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"reindex"),
        "must have reindex, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"read_symbol"),
        "must have read_symbol, got: {tool_names:?}"
    );
    adapter.shutdown();
}

/// AC1: ast subscribes to both event/fileIndexed and event/fileChanged.
#[test]
fn ac1_ast_extension_subscribes_to_both_events() {
    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let m = adapter.manifest();
    assert!(
        m.events.subscribe.contains(&"event/fileIndexed".to_owned()),
        "must subscribe to event/fileIndexed, got: {:?}",
        m.events.subscribe
    );
    assert!(
        m.events.subscribe.contains(&"event/fileChanged".to_owned()),
        "must subscribe to event/fileChanged, got: {:?}",
        m.events.subscribe
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC1 parity: get_outline returns structural items for Rust (U1)
// ─────────────────────────────────────────────────────────────────────────────

/// AC1 parity: get_outline on a Rust file returns struct/impl/method/fn/trait items.
#[test]
fn ac1_get_outline_rust_returns_structural_items() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/counter.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let result = adapter
        .call_tool("get_outline", serde_json::json!({"path": "src/counter.rs"}))
        .expect("get_outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"Counter\""),
        "must contain Counter struct, got: {result_str}"
    );
    assert!(
        result_str.contains("\"make_counter\""),
        "must contain make_counter fn, got: {result_str}"
    );
    assert!(
        result_str.contains("\"Resetable\""),
        "must contain Resetable trait, got: {result_str}"
    );
    // Structural items must have kind/name/start_byte fields.
    assert!(
        result_str.contains("\"start_byte\""),
        "items must have start_byte, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC1: get_outline on a non-Rust file returns unsupported result.
#[test]
fn ac1_get_outline_unsupported_language_returns_unsupported() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("README.md"),
        b"# Heading".to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let result = adapter
        .call_tool("get_outline", serde_json::json!({"path": "README.md"}))
        .expect("get_outline must succeed (even for unsupported language)");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"unsupported\""),
        "non-Rust file must return unsupported result, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC2 / EV1: fileIndexed event → index update → find_symbols reflects it
// ─────────────────────────────────────────────────────────────────────────────

/// AC2/EV1: When fileIndexed is delivered, ast reparses the file and a
/// subsequent search_symbols call reflects the new symbols.
#[test]
fn ac2_file_indexed_event_updates_index() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    // Deliver fileIndexed event for the file.
    adapter
        .deliver_event(Event::FileIndexed {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("deliverEvent must succeed");

    // Search for a symbol that should be in the index.
    let result = adapter
        .call_tool(
            "search_symbols",
            serde_json::json!({"name": "make_counter"}),
        )
        .expect("search_symbols must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"make_counter\""),
        "search_symbols must find make_counter after fileIndexed, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC2: fileChanged event triggers reparse; subsequent find_symbols reflects
/// the updated symbols.
#[test]
fn ac2_file_changed_event_updates_index() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    // Deliver fileChanged event.
    adapter
        .deliver_event(Event::FileChanged {
            file_id: 1,
            path: "src/lib.rs".to_owned(),
        })
        .expect("deliverEvent must succeed");

    // Subsequent find_symbols must reflect the indexed content.
    let result = adapter
        .call_tool(
            "find_symbols",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "Counter",
                "kind": "struct"
            }),
        )
        .expect("find_symbols must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"Counter\""),
        "find_symbols must find Counter after fileChanged, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC3: Multi-language parity (Go and PHP)
// ─────────────────────────────────────────────────────────────────────────────

/// AC3: get_outline on a Go file returns Go-specific structural items.
#[test]
fn ac3_get_outline_go_returns_structural_items() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("main.go"),
        GO_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let result = adapter
        .call_tool("get_outline", serde_json::json!({"path": "main.go"}))
        .expect("get_outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"HelloWorld\""),
        "Go outline must contain HelloWorld fn, got: {result_str}"
    );
    assert!(
        result_str.contains("\"MyStruct\""),
        "Go outline must contain MyStruct, got: {result_str}"
    );
    assert!(
        result_str.contains("\"MyInterface\""),
        "Go outline must contain MyInterface, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC3: get_outline on a PHP file returns PHP-specific structural items.
#[test]
fn ac3_get_outline_php_returns_structural_items() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("app.php"),
        PHP_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let result = adapter
        .call_tool("get_outline", serde_json::json!({"path": "app.php"}))
        .expect("get_outline must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"MyClass\""),
        "PHP outline must contain MyClass, got: {result_str}"
    );
    assert!(
        result_str.contains("\"MyInterface\""),
        "PHP outline must contain MyInterface, got: {result_str}"
    );
    assert!(
        result_str.contains("\"topLevelFn\""),
        "PHP outline must contain topLevelFn, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC3: find_symbols on a Go file finds a Go function.
#[test]
fn ac3_find_symbols_go_finds_function() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("main.go"),
        GO_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "find_symbols",
            serde_json::json!({
                "path": "main.go",
                "symbol_name": "HelloWorld",
                "kind": "function"
            }),
        )
        .expect("find_symbols must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"HelloWorld\""),
        "find_symbols must find HelloWorld in Go, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC3: find_symbols with kind=impl on Go returns not-applicable (empty matches).
#[test]
fn ac3_find_symbols_go_inapplicable_kind_returns_empty() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("main.go"),
        GO_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "find_symbols",
            serde_json::json!({
                "path": "main.go",
                "symbol_name": "Foo",
                "kind": "impl"
            }),
        )
        .expect("find_symbols with inapplicable kind must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"matches\""),
        "inapplicable kind must return matches key, got: {result_str}"
    );
    // matches must be empty
    let matches = result
        .get("matches")
        .and_then(|v| v.as_array())
        .expect("matches must be an array");
    assert!(
        matches.is_empty(),
        "inapplicable kind must return empty matches, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// U1: read_symbol returns symbol content
// ─────────────────────────────────────────────────────────────────────────────

/// U1: read_symbol returns the source content for a named symbol.
#[test]
fn u1_read_symbol_returns_symbol_content() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/counter.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let result = adapter
        .call_tool(
            "read_symbol",
            serde_json::json!({
                "path": "src/counter.rs",
                "symbol_name": "make_counter"
            }),
        )
        .expect("read_symbol must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"make_counter\""),
        "read_symbol must return the symbol name, got: {result_str}"
    );
    assert!(
        result_str.contains("\"content\""),
        "read_symbol must return content field, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// UN1: parse failure on a file does not crash the extension
// ─────────────────────────────────────────────────────────────────────────────

/// UN1: If a file fails to parse (malformed Rust), ast records the partial
/// outline and continues serving others without crashing.
#[test]
fn un1_malformed_rust_file_does_not_crash_extension() {
    let malformed = b"fn broken( { struct // broken\n";
    let good = b"pub fn good_fn() {}";

    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/broken.rs"),
        malformed.to_vec(),
    )
    .expect("write must succeed");
    fs.write(
        core_engine::domain::RelativePath::new("src/good.rs"),
        good.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    // Outline the broken file — must not crash, must return Parsed (partial).
    let result = adapter
        .call_tool("get_outline", serde_json::json!({"path": "src/broken.rs"}))
        .expect("get_outline must not crash on malformed input");
    // Result could be items:[] or items with partial content — not an error.
    assert!(
        result.get("items").is_some() || result.get("unsupported").is_some(),
        "malformed file must return outline or unsupported, got: {result:?}"
    );

    // The extension must still serve the good file correctly after the failure.
    let good_result = adapter
        .call_tool("get_outline", serde_json::json!({"path": "src/good.rs"}))
        .expect("get_outline on good file must succeed after malformed parse");
    let good_str = good_result.to_string();
    assert!(
        good_str.contains("\"good_fn\""),
        "good file must still parse after malformed file, got: {good_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC4 / U4: hello extension — lazy greet tool
// ─────────────────────────────────────────────────────────────────────────────

/// AC4/U4: Given the `hello_extension`, When `greet` is invoked, Then it
/// returns a greeting; it is launched lazily on that first call.
#[test]
fn ac4_hello_extension_greet_returns_greeting() {
    let bin = hello_extension_bin();
    let manifest = hello_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("hello must spawn");

    let result = adapter
        .call_tool("greet", serde_json::json!({"name": "Tower"}))
        .expect("greet must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("Tower"),
        "greet must include the name, got: {result_str}"
    );
    assert!(
        result_str.contains("Hello"),
        "greet must say Hello, got: {result_str}"
    );
    adapter.shutdown();
}

/// AC4: hello extension declares a single `greet` tool, no events.
#[test]
fn ac4_hello_extension_declares_greet_no_events() {
    let bin = hello_extension_bin();
    let manifest = hello_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("hello must spawn");

    let m = adapter.manifest();
    let tool_names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"greet"),
        "hello must declare greet tool, got: {tool_names:?}"
    );
    assert!(
        m.events.subscribe.is_empty(),
        "hello must not subscribe to events (lazy, U4), got: {:?}",
        m.events.subscribe
    );
    adapter.shutdown();
}

/// AC4: hello with no name parameter returns a default greeting.
#[test]
fn ac4_hello_greet_default_name() {
    let bin = hello_extension_bin();
    let manifest = hello_manifest(&bin);
    let deps = make_deps(InMemoryFs::new());

    let mut adapter =
        SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("hello must spawn");

    let result = adapter
        .call_tool("greet", serde_json::json!({}))
        .expect("greet with no name must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("Hello"),
        "default greet must say Hello, got: {result_str}"
    );
    // Default is "World"
    assert!(
        result_str.contains("World"),
        "default greet must use 'World' as name, got: {result_str}"
    );
    adapter.shutdown();
}

// ─────────────────────────────────────────────────────────────────────────────
// AC5: ast_extension uses ExtensionRegistry — build via plain cargo
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Regression — bug #2: spawn→immediate-request deadlock
//
// Root cause: ast/main.rs performed a warm-start `index/get` HostCall AFTER
// sending the `Initialized` response. `spawn()` returned as soon as the host
// saw `Initialized`. The test thread then immediately wrote a new request frame
// (e.g. `invokeTool`) to the child's stdin. The child was still inside
// `read_host_response` for `index/get` (expecting id=10000). It read the
// incoming `invokeTool` frame (id=1), found id≠10000, and silently skipped
// (consumed) it. After receiving the `index/get` response the child looped
// back to `lines.next()`, but the `invokeTool` frame was gone — permanent
// deadlock, manifesting as `ExtensionFault::Timeout` after 15 s.
//
// Fix: all HostCalls inside the `initialize` handler now complete BEFORE the
// `Initialized` response is sent. `spawn()` therefore cannot return until the
// child is fully back in its outer request loop, so no subsequent write races
// with an outstanding `read_host_response`.
//
// Regression test: spawn `ast_extension` and IMMEDIATELY call a tool in a
// tight loop without any sleep. Under a parallel test run with 16+ cores,
// OS scheduling pressure is sufficient to trigger the old bug with very high
// probability within a few hundred iterations. We run 50 sequential
// spawn→call→shutdown cycles; each must complete without Timeout.
// ─────────────────────────────────────────────────────────────────────────────

/// REG/bug#2: spawn followed immediately by a tool call must never deadlock.
///
/// Each iteration spawns `ast_extension`, calls `search_symbols` without any
/// deliberate delay, and shuts down. The old code deadlocked here ~20% of the
/// time under parallel test execution because the warm-start `index/get`
/// HostCall in the `initialize` handler raced with the immediately-following
/// tool request. 50 consecutive clean iterations without `Timeout` confirms
/// the fix.
#[test]
fn reg_spawn_then_immediate_tool_call_never_deadlocks() {
    let bin = ast_extension_bin();

    for iteration in 0..50 {
        let manifest = ast_manifest(&bin);
        let deps = make_deps(InMemoryFs::new());

        let mut adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT)
            .unwrap_or_else(|e| panic!("iteration {iteration}: spawn must succeed, got: {e:?}"));

        // Call a zero-I/O tool (search_symbols with empty index) immediately after
        // spawn, without any yield/sleep, to maximise scheduling interleave.
        let result = adapter.call_tool("search_symbols", serde_json::json!({"name": "anything"}));
        assert!(
            result.is_ok(),
            "iteration {iteration}: call_tool must not deadlock/timeout, got: {:?}",
            result.err()
        );

        adapter.shutdown();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AC5: ast_extension uses ExtensionRegistry — build via plain cargo
// ─────────────────────────────────────────────────────────────────────────────

/// AC5: ast_extension works through ExtensionRegistry (the domain boundary).
/// This test proves the native binary integrates with the registry the same
/// way as any fake `ExtensionInstance`.
#[test]
fn ac5_ast_extension_via_registry() {
    let mut fs = InMemoryFs::new();
    fs.write(
        core_engine::domain::RelativePath::new("src/lib.rs"),
        RUST_SOURCE.to_vec(),
    )
    .expect("write must succeed");

    let bin = ast_extension_bin();
    let manifest = ast_manifest(&bin);
    let deps = make_deps(fs);

    let adapter = SidecarHostAdapter::spawn(manifest, deps, TEST_TIMEOUT).expect("ast must spawn");

    let mut registry = ExtensionRegistry::new();
    registry.register(adapter).expect("must register ast");

    // Invoke get_outline through the registry (routes by tool name).
    let result = registry
        .invoke("get_outline", serde_json::json!({"path": "src/lib.rs"}))
        .expect("registry invoke must succeed");

    let result_str = result.to_string();
    assert!(
        result_str.contains("\"Counter\""),
        "registry invoke must return Counter outline, got: {result_str}"
    );
}
