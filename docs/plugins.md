# Plugin Authoring Guide — Drop & Play

Tower's plugin system lets third parties extend the tool surface exposed over MCP without modifying
or recompiling the host binary. A plugin is a single `.wasm` file compiled to `wasm32-wasip1`.

> **Status**: drop & play is live. The production `tower` binary scans a plugins directory at
> startup, loads every `*.wasm` through the isolated-sandbox path (specs 11c/11d), and serves their
> tools over MCP alongside the 7 native `tower_*` tools (spec 12b) — no host recompile required. Drop a
> `.wasm` in the plugins directory, restart `tower`, and its tools appear in `tools/list`.

This guide covers everything needed to write, build, and deploy a plugin.

```
┌──────────────────────────────────────────────────────────────┐
│  Plugin author's crate (wasm32-wasip1)                       │
│                                                              │
│   Cargo.toml: crate-type = ["cdylib", "rlib"]               │
│   plugin_sdk ─── ABI types, Plugin trait, macros            │
│                                                              │
│   #[plugin_main]                                             │
│   struct MyPlugin;          ← #[plugin_main] generates       │
│                               __plugin_init                  │
│   impl Plugin for MyPlugin    __plugin_call_tool             │
│     fn init() -> Manifest     __plugin_on_hook               │
│     fn call_tool(...)         __plugin_free                  │
│     fn on_hook(...)           __plugin_alloc                 │
│                                                              │
│   tower_host capability surface (two functions only):        │
│     host_log(ptr, len)                                       │
│     host_read_file(path_ptr, path_len, out_ptr, out_len) → u32│
└────────────────┬─────────────────────────────────────────────┘
                 │  .wasm file (postcard wire format)
┌────────────────▼─────────────────────────────────────────────┐
│  tower host (wasmtime)                                       │
│                                                              │
│  IsolationEngine ── fuel + epoch + background ticker        │
│  IsolatedSandbox ── per-call budget, trap catch, quarantine  │
│  PluginHostRegistry ── manifest.name as namespace key        │
│  MergedRegistry ── native tools + tower_<plugin>_<tool>      │
│                                                              │
│  MCP tools/list: "tower_hello_greet", "tower_ast_get_outline", … │
└──────────────────────────────────────────────────────────────┘
```

---

## Prerequisites

The toolchain is pinned in `rust-toolchain.toml` (channel `1.96.0`). `rustup` installs it
automatically on first use; it includes the `wasm32-wasip1` target.

```toml
# rust-toolchain.toml (already in the workspace — shown for reference)
[toolchain]
channel = "1.96.0"
components = ["rustfmt", "clippy"]
targets   = ["wasm32-wasip1"]
profile   = "minimal"
```

**Pure-Rust plugins** (no C dependencies) need no additional tools — `hello_plugin` falls in this
category.

**Plugins that vendor C sources** (e.g. tree-sitter grammars) require the WASI SDK. Two sources:

```bash
# Option A — use the SDK already cached by the tree-sitter CLI
ls ~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang

# Option B — download WASI SDK 25 explicitly
curl -sL https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-25/wasi-sdk-25.0-x86_64-linux.tar.gz \
  | tar -xz -C /opt/wasi-sdk --strip-components=1
```

---

## Step 1 — Create the crate

```bash
cargo new --lib my_plugin
cd my_plugin
```

`Cargo.toml` — the two mandatory items are `crate-type = ["cdylib", "rlib"]` and the `plugin_sdk`
dependency:

```toml
[package]
name    = "my_plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]
# cdylib produces the __plugin_* export symbols the host calls.
# rlib keeps host-side unit tests compilable without wasm.

[dependencies]
plugin_sdk = { path = "../plugin_sdk" }  # or a published version
```

`cdylib` is required. Without it the four `__plugin_*` symbols are not exported and the host cannot
load the plugin.

---

## Step 2 — Implement the `Plugin` trait

```rust
use plugin_sdk::{
    plugin_export, plugin_main,
    ABI_VERSION, HookKind, HookPayload, Plugin, PluginManifest, SdkError, ToolDesc, Value,
};

// #[plugin_main] generates the five wasm export symbols automatically.
// Place it on the struct, not the impl block.
#[plugin_main]
pub struct MyPlugin;

impl Plugin for MyPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name:    "my_plugin".to_owned(),   // becomes the MCP namespace
            version: "0.1.0".to_owned(),
            abi:     ABI_VERSION,              // must be ABI_VERSION — always
            tools: vec![
                ToolDesc {
                    name:        "greet".to_owned(),
                    description: "Return a greeting for the given name.".to_owned(),
                    schema_json: r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#.to_owned(),
                },
            ],
            hooks: vec![HookKind::BeforeToolCall],
        }
    }

    fn call_tool(name: &str, args: Value) -> Result<Value, SdkError> {
        match name {
            "greet" => greet(args),
            other   => Err(SdkError::ToolNotFound(other.to_owned())),
        }
    }

    fn on_hook(kind: HookKind, payload: HookPayload) {
        if let HookKind::BeforeToolCall = kind {
            if let HookPayload::BeforeToolCall { tool_name, .. } = payload {
                plugin_sdk::host::log(&format!("before: {tool_name}"));
            }
        }
    }
}

#[plugin_export]
fn greet(args: Value) -> Result<Value, SdkError> {
    let name = extract_string(&args, "name")?;
    Ok(Value::Text(format!("Hello, {name}!")))
}

fn extract_string<'a>(args: &'a Value, field: &str) -> Result<&'a str, SdkError> {
    match args {
        Value::Map(pairs) => {
            for (k, v) in pairs {
                if k == field {
                    return match v {
                        Value::Text(s) => Ok(s.as_str()),
                        _ => Err(SdkError::InvalidArgs(format!("'{field}' must be a string"))),
                    };
                }
            }
            Err(SdkError::InvalidArgs(format!("missing field '{field}'")))
        }
        _ => Err(SdkError::InvalidArgs("args must be a map".to_owned())),
    }
}
```

### The `Plugin` trait

| Method | Called when | Must return |
|--------|-------------|-------------|
| `fn init() -> PluginManifest` | Once at load time, via `__plugin_init` | Manifest with `abi: ABI_VERSION` |
| `fn call_tool(name, args) -> Result<Value, SdkError>` | On every tool invocation | `Ok(Value)` or `Err(SdkError)` |
| `fn on_hook(kind, payload)` | When a subscribed hook fires | nothing |

### `#[plugin_main]`

Annotate the plugin struct with `#[plugin_main]`. The macro generates five `extern "C"` export
symbols:

| Symbol | Signature | Purpose |
|--------|-----------|---------|
| `__plugin_init` | `() -> *mut u8` | Serialise manifest; return length-prefixed postcard buffer |
| `__plugin_call_tool` | `(*mut u8, usize) -> *mut u8` | Dispatch a tool call |
| `__plugin_on_hook` | `(*mut u8, usize)` | Dispatch a lifecycle hook |
| `__plugin_free` | `(*mut u8, usize)` | Free a buffer previously returned to the host |
| `__plugin_alloc` | `(u32) -> *mut u8` | Allocate guest memory for host-written arguments |

The macro takes no arguments. Do not place it on the `impl` block.

### `#[plugin_export]`

`#[plugin_export]` is a marker attribute for tool handler functions. It documents intent and will
drive automatic dispatch table generation in a future ABI revision. Currently it passes the function
through unchanged. Tool dispatch is explicit in `call_tool`.

### The manifest

```rust
PluginManifest {
    name:    String,        // routing namespace: tools appear as "tower_<name>_<tool_name>" in MCP; must not begin with "tower"
    version: String,        // semver string, informational
    abi:     u32,           // must equal ABI_VERSION (currently 2); any other value → load rejection
    tools:   Vec<ToolDesc>, // tools this plugin exports
    hooks:   Vec<HookKind>, // lifecycle hooks this plugin subscribes to
}
```

`ToolDesc.schema_json` is a JSON Schema string for the tool's input. Use `"{}"` for tools that take
no parameters.

---

## Step 3 — Host capabilities

Inside the wasm guest, exactly two host functions are reachable. Both live in `plugin_sdk::host`.

### `plugin_sdk::host::log`

```rust
plugin_sdk::host::log("plugin initialised");
```

Writes a UTF-8 message to the host's diagnostic output (stderr). Capped at 4096 bytes per call to
prevent log-flooding. On non-wasm targets (host unit tests) this is a no-op.

### `plugin_sdk::host::read_file`

```rust
if let Some(bytes) = plugin_sdk::host::read_file("src/main.rs") {
    let content = String::from_utf8_lossy(&bytes);
    plugin_sdk::host::log(&format!("read {} bytes", bytes.len()));
}
```

Reads a workspace-relative path through the host `FileSystemPort`. Returns `Some(Vec<u8>)` on
success, `None` if the file is not found or an I/O error occurs. The path must be relative (no
leading `/`) and must not contain `..`. On non-wasm targets this always returns `None`.

### Capability sandbox

The WASI context is built with `WasiCtxBuilder::new()` and no further configuration. Concretely:

| WASI syscall | Outcome |
|---|---|
| `path_open` / filesystem I/O | ENOENT — no preopened directories |
| Network sockets | Unavailable |
| Environment variables | None (no leakage) |
| stdin / stdout / stderr | No-op sink |
| Clocks (`clock_time_get`) | Functional — required by Rust std |
| RNG (`random_get`) | Functional — required by Rust std (HashMap seeding) |

Any `tower_host` import beyond `host_log` and `host_read_file` causes a `LinkError` at
instantiation — the plugin is rejected before any guest code runs.

---

## Step 4 — Lifecycle hooks

Declare the hooks the plugin wants in `PluginManifest::hooks`. Only declared hooks are delivered;
undeclared hooks incur zero overhead.

```rust
hooks: vec![HookKind::BeforeToolCall, HookKind::FileIndexed],
```

Available hook kinds (ABI version 2):

| `HookKind` | Fires | `HookPayload` variant |
|---|---|---|
| `BeforeToolCall` | Before any tool call (native or plugin) | `BeforeToolCall { tool_name, args }` |
| `AfterToolCall` | After a tool call completes | `AfterToolCall { tool_name, result }` |
| `FileIndexed` | After a file is indexed | `FileIndexed { path }` |
| `FileChanged` | After a file is re-indexed on change | `FileChanged { path }` |

Hook delivery errors from one plugin are logged to stderr and do not block delivery to other
plugins.

---

## Step 5 — Build to `wasm32-wasip1`

### Pure-Rust plugin (e.g. `hello_plugin`)

No WASI SDK needed:

```bash
cargo build -p my_plugin --target wasm32-wasip1
# Output: target/wasm32-wasip1/debug/my_plugin.wasm

cargo build -p my_plugin --target wasm32-wasip1 --release
# Output: target/wasm32-wasip1/release/my_plugin.wasm  (~small, opt-level="s")
```

### Plugin with C dependencies (e.g. `plugin_ast` — tree-sitter grammars)

The tree-sitter grammar crates vendor C sources compiled via the `cc` crate. The Rust
`wasm32-wasip1` toolchain component has no C sysroot, so WASI SDK must be pointed at via two
environment variables. The `cc` crate honours the `CC_<target>` / `AR_<target>` naming convention
(hyphens replaced by underscores):

```bash
# Using the SDK cached by the tree-sitter CLI (zero extra install):
export CC_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang
export AR_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/llvm-ar

cargo build -p plugin_ast --target wasm32-wasip1 --release
# Output: target/wasm32-wasip1/release/plugin_ast.wasm  (~1.2 MB)
```

Or using a manually downloaded WASI SDK 25:

```bash
export CC_wasm32_wasip1=/opt/wasi-sdk/bin/wasm32-wasip1-clang
export AR_wasm32_wasip1=/opt/wasi-sdk/bin/llvm-ar

cargo build -p plugin_ast --target wasm32-wasip1 --release
```

`CC_wasm32_wasip1` points the `cc` crate at the WASI SDK clang, which carries a `--sysroot`
pointing at `wasi-sysroot` — so `stdlib.h` and friends resolve automatically. `AR_wasm32_wasip1` is
required because the host `ar` produces non-wasm archives.

#### Tree-sitter version pinning

`tree-sitter-rust 0.23.x` targets `tree-sitter 0.25.x`. Crossing the minor boundary (e.g.
`tree-sitter-rust 0.24.x` with `tree-sitter 0.25.x`) causes API mismatches. Pin both in
`Cargo.toml` and test any upgrade explicitly:

```toml
tree-sitter      = "0.25"  # resolved: 0.25.10
tree-sitter-rust = "0.23"  # resolved: 0.23.3
```

---

## Step 6 — Drop into the host

Copy the built `.wasm` into the host's **plugins directory** and (re)start `tower`. That is the whole
deployment step — no host recompile, no config file.

```bash
# Default location: <workspace>/.tower/plugins/
mkdir -p .tower/plugins
cp target/wasm32-wasip1/release/my_plugin.wasm .tower/plugins/
cargo run -p core_engine            # or: tower
```

### Where the host looks

The plugins directory is resolved in this priority order (highest first):

| Source | Example |
|--------|---------|
| `--plugins-dir <path>` flag | `tower --plugins-dir /opt/tower/plugins` |
| `$TOWER_PLUGINS_DIR` env var | `TOWER_PLUGINS_DIR=/opt/tower/plugins tower` |
| Default | `<workspace>/.tower/plugins/` |

The workspace root itself follows `--workspace-dir` / `$TOWER_WORKSPACE` / the current directory.

### What happens at startup

For every `*.wasm` in the directory (processed in sorted order), the host:

1. Calls `__plugin_init` to read the `PluginManifest`.
2. Checks `manifest.abi == ABI_VERSION` (currently `2`). Mismatch → `PluginLoadError::AbiMismatch`.
3. Checks `manifest.name` is unique. Duplicate → `RegistrationError::DuplicateName`.
   Also checks `manifest.name` does not begin with `tower` (reserved for native host tools) →
   `RegistrationError::ReservedName`.
4. Wraps the instance in an `IsolatedSandbox` with fuel + epoch compute bounds (spec 11d), injecting
   the workspace `FileSystemPort` so `host::read_file` reads the real workspace.
5. Registers the tools in the `MergedRegistry` under `tower_<manifest.name>_<tool_name>` (spec 12b).

From this point the plugin's tools appear in the MCP `tools/list` response with no host recompile.

### Graceful degradation

- **No plugins directory / empty directory** → the host serves exactly the 7 native `tower_*` tools,
  identical to a build with no plugins.
- **A single bad plugin** (malformed wasm, ABI mismatch, forbidden import, duplicate name, or a name
  beginning with the reserved `tower` prefix) is logged
  to stderr as a warning and **skipped** — startup never aborts, and the remaining plugins still load.
- **A plugin that faults at call time** (trap, infinite loop, fuel/epoch exhaustion) is isolated by
  its sandbox: the call returns a tool error and the host plus MCP link survive (spec 11d).

### Try it with the reference plugin

```bash
# Build the reference tree-sitter plugin (needs the WASI SDK — see Prerequisites).
CC_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang \
AR_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/llvm-ar \
  cargo build -p plugin_ast --target wasm32-wasip1 --release

mkdir -p .tower/plugins
cp target/wasm32-wasip1/release/plugin_ast.wasm .tower/plugins/
cargo run -p core_engine            # tools/list now includes tower_ast_get_outline + tower_ast_find_symbols
```

---

## ABI version

`ABI_VERSION` is `2` (defined in `crates/plugin_sdk/src/lib.rs`).

Embed it in every manifest:

```rust
abi: ABI_VERSION,   // from plugin_sdk — never hard-code the integer
```

The host rejects plugins where `manifest.abi != ABI_VERSION`. When the SDK bumps `ABI_VERSION`,
recompile all plugins. Breaking ABI changes include:

- Export function signatures (`__plugin_init`, `__plugin_call_tool`, `__plugin_on_hook`,
  `__plugin_free`).
- Postcard layout of `PluginManifest`, `CallRequest`, `CallResponse`, or `HookEnvelope`.

---

## Wire format

All data crossing the host–guest boundary uses [postcard](https://docs.rs/postcard) (binary,
`no_std`+`alloc` compatible). Buffer layout for return values:

```
[ 4-byte LE u32 payload_length ][ payload_length bytes of postcard ]
```

JSON appears only at the MCP adapter boundary in the host. `plugin_sdk` never imports `serde_json`.

---

## Fault isolation and quarantine

Tower enforces per-call compute bounds on every plugin. A plugin fault never crashes the host
process or severs the MCP connection.

### Compute bounds

| Parameter | Default | Description |
|---|---|---|
| Fuel budget | `100_000_000` units | Wasmtime instruction budget per call |
| Epoch deadline | disabled by default | Wall-clock bound; opt in via `IsolationConfig` |

A background thread named `tower-epoch-ticker` increments the engine epoch every 10 ms when
`IsolationEngine` is in use.

### Sandbox lifecycle

```
Ready ──trap/fuel/epoch──▶ Failed ──next call, recreate──▶ Ready
                              │
                         after 3 consecutive
                         restart failures
                              │
                              ▼
                         Quarantined (all calls return PluginFault::Quarantined)
```

`MAX_CONSECUTIVE_FAILURES = 3`. After three consecutive restart failures the sandbox is permanently
quarantined. The manifest stays available for `tools/list` introspection even in the quarantined
state; all tool calls return a `-32603 InternalError` response.

Fault kinds surfaced to the host:

| `PluginFaultKind` | Cause |
|---|---|
| `Trapped(String)` | Guest `unreachable`, panic compiled to wasm trap |
| `FuelExhausted` | Instruction budget exceeded |
| `EpochDeadlineExceeded` | Wall-clock deadline exceeded |
| `Quarantined` | Sandbox permanently disabled |

All faults map to JSON-RPC `-32603 InternalError`. The MCP link is unaffected.

---

## Tool namespacing

Plugin tool names are always prefixed with `tower_<manifest.name>_` in MCP:

```
manifest.name = "ast"  →  MCP tool name: "tower_ast_get_outline"
manifest.name = "hello" → MCP tool name: "tower_hello_greet"
```

Native host tools (`tower_find_file`, etc.) carry the bare `tower_` prefix. To guarantee plugin
tools can never collide with native ones, **a plugin name must not begin with `tower`** — that
prefix is reserved for host tools. A plugin whose `manifest.name` starts with `tower` is rejected
at registration (`RegistrationError::ReservedName`). No collision is possible.

---

## Worked example: `hello_plugin`

`crates/hello_plugin` is the minimal reference plugin. It demonstrates the complete authoring
workflow with two tools (`greet`, `read_file_echo`) and one hook (`BeforeToolCall`).

```
crates/hello_plugin/
├── Cargo.toml   crate-type = ["cdylib", "rlib"]
└── src/lib.rs   HelloPlugin: #[plugin_main], impl Plugin
```

Build and run host-side tests (no wasm runtime needed):

```bash
# Host-side unit tests — no WASI SDK, no wasmtime
cargo test -p hello_plugin

# Build the wasm binary
cargo build -p hello_plugin --target wasm32-wasip1
```

The `greet` tool call from MCP:

```json
{"jsonrpc":"2.0","method":"tools/call","params":{"name":"tower_hello_greet","arguments":{"name":"Alice"}},"id":1}
```

Response:

```json
{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"{\"text\":\"Hello, Alice!\"}"}]},"id":1}
```

The `read_file_echo` tool demonstrates `host::read_file`:

```json
{"jsonrpc":"2.0","method":"tools/call","params":{"name":"tower_hello_read_file_echo","arguments":{"path":"Cargo.toml"}},"id":2}
```

---

## Worked example: `plugin_ast`

`crates/plugin_ast` is the reference Tree-sitter AST plugin (spec 12c/12d). It illustrates a
plugin with C grammar sources and two tools.

```
crates/plugin_ast/
├── Cargo.toml   tree-sitter = "0.25", tree-sitter-rust = "0.23", tree-sitter-go, tree-sitter-php
└── src/
    ├── lib.rs      AstPlugin: #[plugin_main], impl Plugin — tool dispatch
    ├── outline.rs  parse_outline() — host-testable Tree-sitter walker
    └── symbols.rs  find_symbols() — host-testable symbol search
```

Supported languages: `.rs` (Rust), `.go` (Go), `.php` (PHP). Other extensions return
`{"unsupported": true, "language": "<extension>"}` — not an error.

Build (requires WASI SDK for C grammar compilation):

```bash
export CC_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang
export AR_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/llvm-ar

cargo build -p plugin_ast --target wasm32-wasip1 --release
# Produces: target/wasm32-wasip1/release/plugin_ast.wasm  (~1.2 MB)
```

Host-side tests (no WASI SDK or wasmtime needed):

```bash
cargo test -p plugin_ast
```

MCP call examples once loaded:

```json
{"jsonrpc":"2.0","method":"tools/call","params":{"name":"tower_ast_get_outline","arguments":{"path":"src/lib.rs"}},"id":3}
```

```json
{"jsonrpc":"2.0","method":"tools/call","params":{"name":"tower_ast_find_symbols","arguments":{"path":"src/lib.rs","symbol_name":"MyStruct","kind":"struct"}},"id":4}
```

Valid `kind` values: `function`, `struct`, `enum`, `trait`, `impl`, `method`, `module`,
`type_alias`, `const`, `static`, `macro_def`, `class`.

A kind not applicable to the target language (e.g. `enum` in a `.go` file) returns
`{"matches": []}`, not an error.

---

## Quick reference checklist

- [ ] `crate-type = ["cdylib", "rlib"]` in `Cargo.toml`
- [ ] `plugin_sdk` as a dependency (brings `postcard`, `serde`, `plugin_sdk_macros`)
- [ ] Struct annotated with `#[plugin_main]`
- [ ] `impl Plugin` with `init()`, `call_tool()`, `on_hook()`
- [ ] `manifest.abi = ABI_VERSION` — never hard-code `2`
- [ ] `manifest.name` is unique within the deployment
- [ ] Tool handlers return `Err(SdkError::ToolNotFound)` for unknown names — no panic
- [ ] For C-dependency plugins: `CC_wasm32_wasip1` and `AR_wasm32_wasip1` set before building
- [ ] Host-side unit tests written under `#[cfg(test)]` using the `rlib` target

---

## Related documentation

- [architecture.md](architecture.md) — hexagonal boundary, domain invariants, crate layout
- [getting-started.md](getting-started.md) — workspace setup, first build
- [mcp-tools.md](mcp-tools.md) — MCP protocol reference, native tools, JSON-RPC error codes
- [development.md](development.md) — CI quality gate, full test command reference
- [docs/spikes/12a-tree-sitter-wasm-feasibility.md](spikes/12a-tree-sitter-wasm-feasibility.md) — detailed WASI SDK recipe and approach analysis
