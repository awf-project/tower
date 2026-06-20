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
profile  = "minimal"
```

`rustup` reads this file automatically on first use and installs the exact channel and components.
No extra build target is required: the engine and the native extensions all build for the host target.
(The previous WASM model needed `wasm32-wasip1` and a WASI SDK; neither is required any more.)

If you do not have `rustup`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### cargo-deny

Used to enforce license and advisory policy (`deny.toml` at the workspace root).

```bash
cargo install cargo-deny --locked
```

---

## Clone and build

```bash
git clone <repository-url> tower
cd tower

# Build the host binary (tower) and every native extension.
cargo build            # or: make build
```

The host binary produced is `target/debug/tower`; the reference extension binaries
(`ast_extension`, `hello_extension`, `lsp_extension`) are also under `target/debug/`.

---

## Quality gate

Run these four checks in order before every merge. The CI pipeline (`ci.yml`) executes them in exactly
this sequence; a step must pass before the next runs. `make gate` runs the whole sequence.

### 1. Format check

```bash
cargo fmt --all --check          # make fmt-check
```

### 2. Clippy (warnings are errors)

```bash
cargo clippy --workspace --all-targets -- -D warnings    # make clippy
```

### 3. Run the test suite

`cargo test --workspace` compiles every native extension binary too, so the host
integration tests can locate them under `target/debug/`. There is no WASM build
step and no WASI SDK.

```bash
cargo test --workspace           # make test
```

### 4. License and advisory policy

```bash
cargo deny check                 # make deny
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

The response lists the native `tower_*` tools plus any tools contributed by discovered extensions
(e.g. `tower_ast_get_outline`, `tower_ast_find_symbols` when the `ast` extension is present).
Extension tools are always namespaced as `tower_<ext>_<tool_name>`.

Extensions are native sidecar binaries discovered from an extension scope by their `extension.toml`
manifest. Install one (its binary + manifest) into a scope and restart `tower` — no recompile of the
host:

- **Global** (`<xdg-data>/tower/extensions/<name>/`) — usable by every project; scanned first.
- **Local** (`<workspace>/.tower/extensions/<name>/`) — this project only; wins on a name collision.
  An explicit `--extensions-dir <path>` / `$TOWER_EXTENSIONS_DIR` replaces both scopes with one dir.

```bash
# Local (this project only): place the binary + manifest in its own directory.
mkdir -p .tower/extensions/ast
cp target/debug/ast_extension     .tower/extensions/ast/
cp extensions/ast/extension.toml  .tower/extensions/ast/

cargo run -p core_engine     # tower_ast_* tools now appear in tools/list
```

No extensions in any scope simply serves the native tools; an extension that fails to spawn (or one
listed in the config disable list) is skipped with a stderr warning and never blocks startup. When the
same extension name appears in both scopes, the local copy wins. See [`extensions.md`](extensions.md)
for the manifest schema, capabilities, activation, and the supervision/fault model.

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
- [mcp-tools.md](mcp-tools.md) — the native tools and the extension tools in full detail
- [extensions.md](extensions.md) — authoring a native extension: protocol, capabilities, fault model
- [development.md](development.md) — contribution guide, TDD workflow, benchmark targets
