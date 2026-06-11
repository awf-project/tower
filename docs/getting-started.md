# Getting Started

This guide covers prerequisites, building the project, running the quality gate, and operating the
`tower` MCP server for the first time.

---

## Prerequisites

### Rust toolchain

The toolchain is pinned in `rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.96.0"
components = ["rustfmt", "clippy"]
targets  = ["wasm32-wasip1"]
profile  = "minimal"
```

`rustup` reads this file automatically on first use and installs the exact channel, components, and
the `wasm32-wasip1` target. No manual `rustup target add` is needed after a fresh clone.

If you do not have `rustup`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### cargo-deny

Used to enforce license and advisory policy (`deny.toml` at the workspace root).

```bash
cargo install cargo-deny --locked
```

### WASI SDK (required only for `plugin_ast` and future grammar plugins)

`plugin_ast` vendors tree-sitter C grammar sources that must be compiled to `wasm32-wasip1`. The
Rust wasm target ships no C sysroot, so WASI SDK provides the wasm-targeting clang and the
`wasi-sysroot`.

Pure-Rust wasm crates (`hello_plugin`, all `fixture_*`) compile without it.

**Option A — reuse what the tree-sitter CLI already downloaded (zero extra work)**

```bash
# After running `tree-sitter build --wasm` at least once the SDK is cached at:
ls ~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang

export CC_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang
export AR_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/llvm-ar
```

**Option B — download WASI SDK 25 explicitly**

```bash
WASI_SDK_DIR="$HOME/.local/wasi-sdk"
mkdir -p "$WASI_SDK_DIR"
curl -fsSL \
  https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-25/wasi-sdk-25.0-x86_64-linux.tar.gz \
  | tar -xz -C "$WASI_SDK_DIR" --strip-components=1

export CC_wasm32_wasip1="$WASI_SDK_DIR/bin/wasm32-wasip1-clang"
export AR_wasm32_wasip1="$WASI_SDK_DIR/bin/llvm-ar"
```

The env-var names follow the `cc` crate convention: `CC_<target>` / `AR_<target>` with hyphens
replaced by underscores. They must be visible in every shell session that builds the wasm target.
Add the `export` lines to your shell profile or a `.envrc` file to make them persistent.

> See [docs/spikes/12a-tree-sitter-wasm-feasibility.md](spikes/12a-tree-sitter-wasm-feasibility.md)
> for the full investigation and reasoning behind this approach.

---

## Clone and build

```bash
git clone <repository-url> tower
cd tower

# Build the host binary (tower) and all host-side crates.
# The wasm32-wasip1 crates are excluded from default-members and are not built here.
cargo build
```

The binary produced is `target/debug/tower`.

---

## Quality gate

Run these commands in order before every merge. The CI pipeline (`ci.yml`) executes them in exactly
this sequence; a step must pass before the next runs.

### 1. Format check

```bash
cargo fmt --all --check
```

### 2. Clippy (warnings are errors)

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

### 3. Build wasm fixtures

Integration tests locate fixture `.wasm` files through env vars set by `crates/core_engine/build.rs`.
Those vars point at `target/wasm32-wasip1/debug/*.wasm`, so the fixtures must be compiled before
`cargo test` is run. Skipping this step causes integration tests to fail at startup.

```bash
cargo build \
  -p hello_plugin \
  -p fixture_abi_mismatch \
  -p fixture_panic_plugin \
  -p fixture_loop_plugin \
  -p fixture_loop_hook_plugin \
  --target wasm32-wasip1
```

### 4. Build plugin_ast for wasm32-wasip1

Requires `CC_wasm32_wasip1` and `AR_wasm32_wasip1` to be set (see [Prerequisites](#prerequisites)).

```bash
cargo build -p plugin_ast --target wasm32-wasip1
```

### 5. Run host-side tests

```bash
cargo test --workspace \
  --exclude hello_plugin \
  --exclude plugin_ast \
  --exclude fixture_abi_mismatch \
  --exclude fixture_panic_plugin \
  --exclude fixture_loop_plugin \
  --exclude fixture_loop_hook_plugin
```

Expected: 423 tests pass, 6 ignored.

### 6. Run plugin_ast host-side tests

`plugin_ast` contains a native outline walker that is tested without WASI SDK (tree-sitter compiles
natively here).

```bash
cargo test -p plugin_ast
```

Expected: 65 tests pass.

### 7. License and advisory policy

```bash
cargo deny check
```

---

## Running the engine as an MCP server

`tower` is a JSON-RPC 2.0 server that speaks over `stdin`/`stdout` using newline-delimited JSON (no
`Content-Length` header). Start it with:

```bash
cargo run -p core_engine
```

Or, after a release build:

```bash
cargo build --release -p core_engine
./target/release/tower
```

### Workspace root resolution

The engine needs to know which directory to index. It resolves the workspace root in priority order:

| Priority | Mechanism | Example |
|----------|-----------|---------|
| 1 (highest) | `--workspace-dir <path>` CLI flag | `tower --workspace-dir /home/user/myproject` |
| 2 | `TOWER_WORKSPACE` environment variable | `TOWER_WORKSPACE=/home/user/myproject tower` |
| 3 (fallback) | Current working directory | `cd /home/user/myproject && tower` |

### Sled database

On startup, `tower` creates `.tower/db/` inside the workspace root and opens a `sled` embedded
database there. This directory is created automatically; no manual setup is required.

```
<workspace-root>/
└── .tower/
    └── db/       ← sled database (persisted across restarts)
```

### Initial scan

On the first run against a workspace, `tower` walks the directory tree (respecting `.gitignore`),
indexes every text file, and reports progress to `stderr`:

```
tower: initial scan complete — 312 files indexed
```

Subsequent starts reload the index from `sled` and skip the scan. The scan state is stored
transactionally; a crash during the initial scan causes a fresh scan on the next start.

---

## First session

The following is a minimal copy-paste session. Each line sent to `stdin` must be a single JSON
object terminated by a newline. Responses arrive on `stdout` the same way.

Start the server pointed at a project directory:

```bash
cd /path/to/your/project
cargo run -p core_engine
# stderr: tower: initial scan complete — N files indexed
```

In a second terminal (or pipe input to the process), send JSON-RPC messages.

### Handshake

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
```

Expected response:

```json
{"jsonrpc":"2.0","result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"tower","version":"0.1.0"}},"id":1}
```

### List available tools

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

The response lists 7 native `tower_*` tools plus any loaded plugin tools (e.g., `tower_ast_ast_get_outline`,
`tower_ast_ast_find_symbols` if `plugin_ast` is deployed). Plugin tools are always namespaced as
`tower_<plugin_name>_<tool_name>`.

To deploy a plugin, drop its `.wasm` into the plugins directory and restart `tower` — no recompile.
The directory is resolved from `--plugins-dir <path>`, then `$TOWER_PLUGINS_DIR`, then the default
`<workspace>/.tower/plugins/`:

```bash
mkdir -p .tower/plugins
cp target/wasm32-wasip1/release/plugin_ast.wasm .tower/plugins/
cargo run -p core_engine     # tower_ast_* tools now appear in tools/list
```

A missing or empty directory simply serves the 7 native tools; a malformed or ABI-mismatched
`.wasm` is skipped with a stderr warning and never blocks startup. See
[`plugins.md`](plugins.md) for the full deployment and fault-isolation details.

### Find a file

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tower_find_file","arguments":{"query":"main.rs"}}}
```

### Search for text

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tower_search_text","arguments":{"pattern":"fn main"}}}
```

### Read a file

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"tower_read_file","arguments":{"path":"src/main.rs"}}}
```

### Create a file

```json
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"tower_create_file","arguments":{"path":"notes.txt","content":"hello world\n"}}}
```

### Mass find-and-replace

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"tower_global_replace","arguments":{"target":"old_name","replacement":"new_name"}}}
```

Response shape (success):

```json
{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"{\"files_changed\":3,\"replacements\":7,\"errors\":[]}"}]},"id":7}
```

All tool responses follow the same envelope: `result.content[0].text` is a JSON string containing
the tool-specific payload.

### Error codes reference

| Code | Meaning |
|------|---------|
| -32700 | `ParseError` — malformed JSON or invalid UTF-8 |
| -32600 | `InvalidRequest` — wrong `jsonrpc` version |
| -32601 | `MethodNotFound` — unknown RPC method |
| -32602 | `InvalidParams` — missing required field in tool arguments |
| -32603 | `InternalError` — tool execution failed |
| -32001 | `ToolNotFound` — named tool not in the registry |
| -32002 | `ResourceNotFound` — domain entity not found |

---

## Next steps

- [architecture.md](architecture.md) — hexagonal boundary, domain model, adapter wiring
- [mcp-tools.md](mcp-tools.md) — all 7 native tools and the AST plugin tools in full detail
- [plugins.md](plugins.md) — authoring a Drop-and-Play wasm plugin, ABI, fault isolation
- [development.md](development.md) — contribution guide, TDD workflow, benchmark targets
