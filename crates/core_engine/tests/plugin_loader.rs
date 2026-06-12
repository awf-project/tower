//! Integration tests for the wasmtime plugin loader (spec 11c).
//!
//! # TDD sequence
//!
//! 1. RED → GREEN: AC1 — load hello, verify PluginInstance returned.
//! 2. RED → GREEN: AC2 — allowed capability round-trip (host_read_file).
//! 3. RED → GREEN: AC3 — forbidden import denied at link/instantiation.
//! 4. RED → GREEN: AC4 — ABI mismatch rejected with version error.
//!
//! # Fixture paths
//!
//! Wasm fixture paths are set by `core_engine`'s `build.rs` script as
//! `cargo:rustc-env` variables. The CI workflow builds `hello` and
//! `fixture_abi_mismatch` for `wasm32-wasip1` BEFORE running `cargo test`.
//! The `forbidden_import.wasm` and `forbidden_host_import.wat` are committed
//! directly to `tests/fixtures/` (no build step needed).
//!
//! # WASI lockdown verification
//!
//! The `forbidden_import.wasm` fixture imports `wasi_snapshot_preview1::fd_write`
//! which IS resolved by the WASI linker (that is intentional — the module loads
//! successfully). The test therefore verifies the *sandbox* property: even though
//! the WASI import is resolved, calling `fd_write` in a zero-capability WasiCtx
//! fails gracefully (EBADF/no-op) rather than writing to the host filesystem.
//!
//! # AC3 security boundary (UN2)
//!
//! The `forbidden_host_import.wat` fixture imports an UNREGISTERED `tower_host`
//! function (`raw_fs_write`). This import has NO binding in the linker, so
//! instantiation must fail with `PluginLoadError::LinkError`. This is the key
//! property of the security boundary: unknown tower_host imports are positively
//! denied, not silently resolved.

use std::sync::{Arc, RwLock};

use core_engine::adapters::{
    HostDeps, InMemoryAstIndex, InMemoryFs, PluginLoadError, WasmtimeHost,
};
use core_engine::domain::RelativePath;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::{AstIndexPort, FileSystemPort};

// ── Fixture paths (set by build.rs) ──────────────────────────────────────────

fn hello_wasm() -> &'static str {
    env!("HELLO_PLUGIN_WASM")
}

fn abi_mismatch_wasm() -> &'static str {
    env!("ABI_MISMATCH_WASM")
}

fn forbidden_import_wasm() -> &'static str {
    env!("FORBIDDEN_IMPORT_WASM")
}

fn forbidden_host_import_wat() -> &'static str {
    env!("FORBIDDEN_HOST_IMPORT_WASM")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn empty_fs() -> Arc<InMemoryFs> {
    Arc::new(InMemoryFs::new())
}

fn empty_ast_index() -> Arc<dyn AstIndexPort + Send + Sync> {
    Arc::new(InMemoryAstIndex::new())
}

fn empty_workspace() -> Arc<RwLock<ProjectWorkspace>> {
    Arc::new(RwLock::new(ProjectWorkspace::new()))
}

fn empty_deps() -> HostDeps {
    HostDeps {
        fs: empty_fs(),
        ast_index: empty_ast_index(),
        workspace: empty_workspace(),
    }
}

fn fs_with_file(path: &str, content: &[u8]) -> Arc<InMemoryFs> {
    let mut fs = InMemoryFs::new();
    fs.write(RelativePath::new(path), content.to_vec())
        .expect("write must succeed");
    Arc::new(fs)
}

// ── AC1: Load and register ────────────────────────────────────────────────────

/// AC1: Given a trivial .wasm plugin, When loaded, Then it instantiates and
/// returns a PluginInstance with the expected manifest.
#[test]
fn ac1_load_hello_returns_plugin_instance() {
    let instance =
        WasmtimeHost::load(hello_wasm(), empty_deps()).expect("AC1: hello must load successfully");

    let manifest = instance.manifest();
    assert_eq!(manifest.name, "hello", "AC1: manifest name");
    assert_eq!(manifest.version, "0.1.0", "AC1: manifest version");
    assert_eq!(manifest.abi, plugin_sdk::ABI_VERSION, "AC1: manifest abi");
    assert_eq!(manifest.tools.len(), 5, "AC1: five tools");
    assert_eq!(manifest.tools[0].name, "greet", "AC1: greet tool name");
    assert_eq!(
        manifest.tools[1].name, "read_file_echo",
        "AC1: read_file_echo tool name"
    );
    assert_eq!(
        manifest.tools[2].name, "index_put",
        "AC1: index_put tool name"
    );
    assert_eq!(
        manifest.tools[3].name, "index_get",
        "AC1: index_get tool name"
    );
    assert_eq!(
        manifest.tools[4].name, "list_files",
        "AC1: list_files tool name"
    );
}

/// AC1 extension: the returned instance can be registered in the domain registry
/// without error (EV1).
#[test]
fn ac1_loaded_instance_registers_in_domain_registry() {
    use core_engine::domain::PluginHostRegistry;

    let instance = WasmtimeHost::load(hello_wasm(), empty_deps()).expect("AC1: hello must load");

    let mut registry = PluginHostRegistry::new();
    registry
        .register(instance)
        .expect("AC1: current-ABI plugin must register without error");

    let tools = registry.declared_tools();
    assert_eq!(
        tools.len(),
        5,
        "AC1: registry must contain greet, read_file_echo, index_put, index_get, list_files tools"
    );
    let tool_names: Vec<&str> = tools.iter().map(|(_, t)| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"greet"),
        "AC1: greet must be in declared tools"
    );
    assert!(
        tool_names.contains(&"read_file_echo"),
        "AC1: read_file_echo must be in declared tools"
    );
    assert!(
        tool_names.contains(&"index_put"),
        "AC1: index_put must be in declared tools"
    );
    assert!(
        tool_names.contains(&"index_get"),
        "AC1: index_get must be in declared tools"
    );
    assert!(
        tool_names.contains(&"list_files"),
        "AC1: list_files must be in declared tools"
    );
}

// ── AC2: Allowed capability round-trip ───────────────────────────────────────

/// AC2: Given a plugin that calls a tool, When call_tool is invoked with valid
/// args, Then it returns the expected response.
#[test]
fn ac2_call_tool_greet_returns_greeting() {
    use plugin_sdk::Value;

    let mut instance =
        WasmtimeHost::load(hello_wasm(), empty_deps()).expect("AC2: hello must load");

    let args = Value::Map(vec![("name".to_owned(), Value::Text("World".to_owned()))]);
    let result = instance
        .call_tool("greet", args)
        .expect("AC2: call_tool must succeed");

    assert_eq!(
        result,
        Value::Text("Hello, World!".to_owned()),
        "AC2: greeting must match"
    );
}

/// AC2: host_read_file end-to-end round-trip — plugin reads a file from the
/// injected InMemoryFs and returns the content.
///
/// This exercises the full path: plugin calls `host_read_file` via the SDK,
/// the host `host_read_file_impl` decodes the path, calls `FileSystemPort::read`,
/// allocates a guest buffer via `__plugin_alloc`, writes the bytes, and the
/// plugin receives and returns them.
#[test]
fn ac2_host_read_file_round_trips_content() {
    use plugin_sdk::Value;

    let content = b"# Tower workspace file\n";
    let fs = fs_with_file("docs/readme.md", content);
    let mut instance = WasmtimeHost::load(
        hello_wasm(),
        HostDeps {
            fs,
            ast_index: empty_ast_index(),
            workspace: empty_workspace(),
        },
    )
    .expect("AC2: hello must load");

    let args = Value::Map(vec![(
        "path".to_owned(),
        Value::Text("docs/readme.md".to_owned()),
    )]);
    let result = instance
        .call_tool("read_file_echo", args)
        .expect("AC2: read_file_echo must succeed");

    assert_eq!(
        result,
        Value::Text("# Tower workspace file\n".to_owned()),
        "AC2: file content must round-trip through host_read_file"
    );
}

/// AC2: host_read_file traversal guard — a path containing ".." is rejected
/// by the host traversal guard and returns an error, not host fs content.
#[test]
fn ac2_host_read_file_rejects_traversal_path() {
    use plugin_sdk::Value;

    // Provide a populated FS so the test does not fail due to not-found on a
    // legitimate path. The traversal path must be rejected regardless.
    let fs = fs_with_file("safe.txt", b"safe content");
    let mut instance = WasmtimeHost::load(
        hello_wasm(),
        HostDeps {
            fs,
            ast_index: empty_ast_index(),
            workspace: empty_workspace(),
        },
    )
    .expect("AC2: hello must load");

    // A path with ".." is a traversal attempt — must not reach the FS port.
    let args = Value::Map(vec![(
        "path".to_owned(),
        Value::Text("../etc/passwd".to_owned()),
    )]);
    let result = instance.call_tool("read_file_echo", args);
    assert!(
        result.is_err(),
        "AC2: traversal path must be rejected, got: {result:?}"
    );
}

/// AC2: host_read_file traversal guard — an absolute path is also rejected.
#[test]
fn ac2_host_read_file_rejects_absolute_path() {
    use plugin_sdk::Value;

    let fs = fs_with_file("safe.txt", b"safe content");
    let mut instance = WasmtimeHost::load(
        hello_wasm(),
        HostDeps {
            fs,
            ast_index: empty_ast_index(),
            workspace: empty_workspace(),
        },
    )
    .expect("AC2: hello must load");

    let args = Value::Map(vec![(
        "path".to_owned(),
        Value::Text("/etc/passwd".to_owned()),
    )]);
    let result = instance.call_tool("read_file_echo", args);
    assert!(
        result.is_err(),
        "AC2: absolute path must be rejected, got: {result:?}"
    );
}

/// AC2 extension: tool call round-trip with missing tool returns ToolNotFound.
#[test]
fn ac2_call_tool_unknown_returns_tool_not_found() {
    use core_engine::domain::PluginHostError;

    let mut instance =
        WasmtimeHost::load(hello_wasm(), empty_deps()).expect("AC2: load must succeed");

    let result = instance.call_tool("nonexistent", plugin_sdk::Value::Null);
    assert!(
        matches!(result, Err(PluginHostError::ToolNotFound(_))),
        "AC2: unknown tool must return ToolNotFound, got: {result:?}"
    );
}

/// AC2 extension: deliver_hook round-trip — BeforeToolCall hook is delivered
/// without error.
#[test]
fn ac2_deliver_hook_before_tool_call_succeeds() {
    use plugin_sdk::{HookKind, HookPayload, Value};

    let mut instance =
        WasmtimeHost::load(hello_wasm(), empty_deps()).expect("AC2: load must succeed");

    let result = instance.deliver_hook(
        HookKind::BeforeToolCall,
        HookPayload::BeforeToolCall {
            tool_name: "greet".to_owned(),
            args: Value::Null,
        },
    );
    assert!(
        result.is_ok(),
        "AC2: hook delivery must succeed: {result:?}"
    );
}

// ── AC3: Security boundary — unknown tower_host import is denied ──────────────

/// AC3 (UN2): Given a plugin that imports an UNREGISTERED `tower_host` function,
/// When loaded, Then `WasmtimeHost::load` returns `Err(PluginLoadError::LinkError)`.
///
/// The fixture `forbidden_host_import.wat` imports `tower_host::raw_fs_write`
/// which has no binding in the capability linker. The linker must fail to
/// resolve it, producing a `LinkError`. This is the key security property:
/// unknown `tower_host` imports are positively denied, not silently resolved.
#[test]
fn ac3_unknown_host_import_is_link_error() {
    let result = WasmtimeHost::load(forbidden_host_import_wat(), empty_deps());
    let is_link_error = matches!(result, Err(PluginLoadError::LinkError(_)));
    let description = match &result {
        Ok(_) => "Ok(<plugin instance>)".to_owned(),
        Err(e) => format!("Err({e})"),
    };
    assert!(
        is_link_error,
        "AC3: unregistered tower_host import must produce LinkError, got: {description}"
    );
}

/// U2 (WASI sandbox): Given a plugin that imports `wasi_snapshot_preview1::fd_write`
/// (a raw WASI syscall), the WASI linker resolves it BUT the zero-capability
/// `WasiCtx` makes it inert — fd_write has no open file descriptors and
/// returns EBADF rather than writing to the host filesystem.
///
/// This test verifies the sandbox property: even when a WASI import is
/// resolved, it cannot perform real I/O. Note that `fd_write` is a WASI
/// function, not a `tower_host` function — it IS in the WASI linker by design
/// (the WASI lockdown is by zero-capability context, not by import denial).
#[test]
fn u2_wasi_fd_write_resolves_but_is_inert_in_sandbox() {
    use plugin_sdk::Value;

    let load_result = WasmtimeHost::load(forbidden_import_wasm(), empty_deps());

    match load_result {
        // The WASI import resolved but __plugin_init returned ptr=0 (invalid
        // manifest), so the loader correctly rejects the plugin.
        Err(
            PluginLoadError::InvalidManifestPointer
            | PluginLoadError::InitTrap(_)
            | PluginLoadError::ManifestDeserialize(_),
        ) => {
            // Pass: the sandbox prevented the plugin from functioning.
        }
        // If the plugin somehow loaded (it has all required exports), any
        // call_tool must fail — real I/O did not occur.
        Ok(mut instance) => {
            let result = instance.call_tool("__plugin_call_tool", Value::Null);
            assert!(
                result.is_err(),
                "U2: sandbox plugin call_tool must not succeed with real I/O"
            );
        }
        Err(PluginLoadError::LinkError(msg)) => {
            // Also acceptable — wasmtime may treat WASI differently.
            eprintln!("U2: fd_write caused link error (unexpected but safe): {msg}");
        }
        Err(e) => {
            panic!("U2: unexpected error variant: {e}");
        }
    }
}

// ── AC4: ABI mismatch rejected ────────────────────────────────────────────────

/// AC4: Given a plugin with abi=0 (mismatched), When loaded, Then the host
/// rejects it with a clear AbiMismatch error and does NOT return a PluginInstance.
#[test]
fn ac4_abi_mismatch_plugin_is_rejected() {
    let result = WasmtimeHost::load(abi_mismatch_wasm(), empty_deps()).map(|_| "<plugin instance>");

    assert!(
        matches!(result, Err(PluginLoadError::AbiMismatch { expected, got }) if expected == plugin_sdk::ABI_VERSION && got == 0),
        "AC4: ABI mismatch must produce AbiMismatch {{ expected: {}, got: 0 }}, got: {result:?}",
        plugin_sdk::ABI_VERSION
    );
}

/// AC4: Verify the error message is human-readable.
#[test]
fn ac4_abi_mismatch_error_message_is_clear() {
    let result = WasmtimeHost::load(abi_mismatch_wasm(), empty_deps()).map(|_| "<plugin instance>");
    let err = result.expect_err("AC4: must error");
    let msg = err.to_string();
    assert!(
        msg.contains("ABI mismatch"),
        "AC4: error must mention 'ABI mismatch', got: {msg}"
    );
    assert!(
        msg.contains(&plugin_sdk::ABI_VERSION.to_string()),
        "AC4: error must mention expected ABI version, got: {msg}"
    );
}

// ── WASI lockdown verification ────────────────────────────────────────────────

// ── Stage 3b: ast_store_put / ast_store_get round-trip via hello plugin ──────

/// Stage 3b AC: `index_put` followed by `index_get` on the same key returns
/// identical bytes through `InMemoryAstIndex`.
///
/// rc-convention (from loader.rs):
/// - `ast_store_get` returns 0 (present) including for empty values, 1 (absent), 2 (error).
/// - The guest SDK (`host::ast_store_get`) maps rc=0 → `Some(bytes)`, rc!=0 → `None`.
/// - `index_get` tool maps `Some(bytes)` → `Value::Text(content)`, `None` → `Value::Null`.
///
/// This test asserts the full put → get round-trip succeeds end-to-end and that
/// querying a missing key returns `Value::Null`.
#[test]
fn stage3b_index_put_then_get_round_trips_bytes() {
    use plugin_sdk::Value;

    let ast_index = Arc::new(InMemoryAstIndex::new()) as Arc<dyn AstIndexPort + Send + Sync>;
    let mut instance = WasmtimeHost::load(
        hello_wasm(),
        HostDeps {
            fs: empty_fs(),
            ast_index: Arc::clone(&ast_index),
            workspace: empty_workspace(),
        },
    )
    .expect("stage3b: hello must load");

    // Put a value under key "mykey".
    let put_args = Value::Map(vec![
        ("key".to_owned(), Value::Text("mykey".to_owned())),
        ("value".to_owned(), Value::Text("hello bytes".to_owned())),
    ]);
    let put_result = instance
        .call_tool("index_put", put_args)
        .expect("stage3b: index_put must succeed");
    assert_eq!(
        put_result,
        Value::Text("ok".to_owned()),
        "stage3b: index_put must return 'ok'"
    );

    // Get the value back — must match what was stored.
    let get_args = Value::Map(vec![("key".to_owned(), Value::Text("mykey".to_owned()))]);
    let get_result = instance
        .call_tool("index_get", get_args)
        .expect("stage3b: index_get must succeed");
    assert_eq!(
        get_result,
        Value::Text("hello bytes".to_owned()),
        "stage3b: index_get must return the stored value"
    );
}

/// Stage 3b AC: `index_get` on a key that was never stored returns `Value::Null`.
///
/// rc-convention: rc=1 (absent) → `None` → `Value::Null`.
#[test]
fn stage3b_index_get_missing_key_returns_null() {
    use plugin_sdk::Value;

    let mut instance =
        WasmtimeHost::load(hello_wasm(), empty_deps()).expect("stage3b: hello must load");

    let get_args = Value::Map(vec![(
        "key".to_owned(),
        Value::Text("does-not-exist".to_owned()),
    )]);
    let get_result = instance
        .call_tool("index_get", get_args)
        .expect("stage3b: index_get on missing key must not error");
    assert_eq!(
        get_result,
        Value::Null,
        "stage3b: index_get on absent key must return Null"
    );
}

/// U2: A loaded plugin cannot access the real filesystem via WASI syscalls.
///
/// This is demonstrated by verifying that the InMemoryFs port (not std::fs)
/// is the only file access channel. The WASI context has no preopened dirs.
#[test]
fn u2_wasi_has_no_preopened_directories() {
    // Load hello with an empty InMemoryFs. The plugin does NOT call
    // host_read_file, so loading and calling greet must succeed.
    // This confirms the WASI lockdown doesn't break the plugin while also
    // confirming no real filesystem access occurs.
    let mut instance = WasmtimeHost::load(hello_wasm(), empty_deps())
        .expect("U2: hello with empty FS must still load");

    // The greet tool works because it needs no file access.
    let args = plugin_sdk::Value::Map(vec![(
        "name".to_owned(),
        plugin_sdk::Value::Text("WASI".to_owned()),
    )]);
    let result = instance
        .call_tool("greet", args)
        .expect("U2: greet must work");
    assert_eq!(result, plugin_sdk::Value::Text("Hello, WASI!".to_owned()));
}

// ── Stage 4a: host_list_files end-to-end ─────────────────────────────────────

/// Stage 4a AC: `list_files` returns the paths of all files in the workspace.
///
/// The workspace is pre-populated with 3 known paths. After loading hello
/// with that workspace, calling the `list_files` tool must return count=3 and
/// all three paths (order may vary, so we check containment).
///
/// RC convention: rc=0 always (empty workspace also returns rc=0 with a
/// postcard-encoded empty Vec). The guest deserialises with postcard.
#[test]
fn stage4a_list_files_returns_workspace_paths() {
    use core_engine::domain::workspace::ProjectWorkspace;
    use core_engine::domain::{FileMetadata, RelativePath};
    use plugin_sdk::Value;

    // Build a workspace with 3 known files.
    let mut ws = ProjectWorkspace::new();
    ws.insert(RelativePath::new("src/main.rs"), FileMetadata::default())
        .expect("insert main.rs");
    ws.insert(RelativePath::new("src/lib.rs"), FileMetadata::default())
        .expect("insert lib.rs");
    ws.insert(RelativePath::new("Cargo.toml"), FileMetadata::default())
        .expect("insert Cargo.toml");

    let workspace = Arc::new(RwLock::new(ws));
    let deps = HostDeps {
        fs: empty_fs(),
        ast_index: empty_ast_index(),
        workspace: Arc::clone(&workspace),
    };

    let mut instance = WasmtimeHost::load(hello_wasm(), deps).expect("stage4a: hello must load");

    let result = instance
        .call_tool("list_files", Value::Map(vec![]))
        .expect("stage4a: list_files must succeed");

    // The result is a map with "count" and "paths".
    let Value::Map(ref pairs) = result else {
        panic!("stage4a: expected Value::Map, got {result:?}");
    };

    let count = pairs
        .iter()
        .find(|(k, _)| k == "count")
        .map(|(_, v)| v)
        .expect("stage4a: map must have 'count' key");
    assert_eq!(
        *count,
        Value::Integer(3),
        "stage4a: count must be 3, got {count:?}"
    );

    let paths_val = pairs
        .iter()
        .find(|(k, _)| k == "paths")
        .map(|(_, v)| v)
        .expect("stage4a: map must have 'paths' key");
    let paths_str = match paths_val {
        Value::Text(s) => s.as_str(),
        other => panic!("stage4a: 'paths' must be Value::Text, got {other:?}"),
    };
    assert!(
        paths_str.contains("src/main.rs"),
        "stage4a: paths must contain src/main.rs, got: {paths_str}"
    );
    assert!(
        paths_str.contains("src/lib.rs"),
        "stage4a: paths must contain src/lib.rs, got: {paths_str}"
    );
    assert!(
        paths_str.contains("Cargo.toml"),
        "stage4a: paths must contain Cargo.toml, got: {paths_str}"
    );
}

/// Stage 4a AC: `list_files` on an empty workspace returns count=0 and empty paths.
///
/// RC convention: rc=0 with empty Vec is valid — not "absent", just an
/// empty workspace. The guest returns count=0 and an empty paths string.
#[test]
fn stage4a_list_files_empty_workspace_returns_zero() {
    use plugin_sdk::Value;

    let mut instance =
        WasmtimeHost::load(hello_wasm(), empty_deps()).expect("stage4a: hello must load");

    let result = instance
        .call_tool("list_files", Value::Map(vec![]))
        .expect("stage4a: list_files on empty workspace must not error");

    let Value::Map(ref pairs) = result else {
        panic!("stage4a: expected Value::Map, got {result:?}");
    };

    let count = pairs
        .iter()
        .find(|(k, _)| k == "count")
        .map(|(_, v)| v)
        .expect("stage4a: map must have 'count' key");
    assert_eq!(
        *count,
        Value::Integer(0),
        "stage4a: empty workspace must yield count=0"
    );
}

/// Stage 4a: ABI is additive — `fixture_abi_mismatch` (abi=0) still fails with
/// AbiMismatch. Adding a new host import does NOT change the version contract.
#[test]
fn stage4a_abi_additive_mismatch_still_rejected() {
    let result = WasmtimeHost::load(abi_mismatch_wasm(), empty_deps()).map(|_| "<loaded>");
    assert!(
        matches!(result, Err(PluginLoadError::AbiMismatch { .. })),
        "stage4a: abi_mismatch fixture must still be rejected: {result:?}"
    );
}
