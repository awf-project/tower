# tower

Core engine for a high-performance productivity tool: a virtual file system with a persistent
inverted index, parallel content search, safe mass text refactoring, and a "Drop & Play" WASM
plugin architecture — all exposed over a JSON-RPC 2.0 stdio interface following the
[Model Context Protocol (MCP)](https://modelcontextprotocol.io).

Architecture: **Domain-Driven Design + Hexagonal (Ports & Adapters) + Microkernel**.
See [`project-brief.md`](project-brief.md) for the full vision.

---

## Features

| Capability | Description |
|---|---|
| **Virtual file system** | Workspace-scoped VFS with a persistent `sled` inverted index; sub-millisecond file lookup |
| **Parallel content search** | Rayon-backed grep across all indexed files |
| **Safe file mutations** | Shadow-file pattern (`<path>.tmp_write` → flush → atomic `fs::rename`); crash-safe |
| **Mass refactoring** | Parallel global find-and-replace with per-file atomic rewrites and a `TxReport` |
| **MCP server** | JSON-RPC 2.0 over stdin/stdout; 7 native `vfs_*` tools always available |
| **WASM plugin host** | `wasmtime` sandbox with fuel + epoch compute bounds and automatic fault isolation |
| **AST analysis** | `plugin_ast` — Tree-sitter outline and symbol search for Rust, Go, PHP |
| **Single static binary** | No JVM, Node, or container required at runtime |

---

## 60-second quick start

### Prerequisites

- Rust toolchain — pinned by `rust-toolchain.toml`; `rustup` installs it automatically:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- `cargo-deny` for the license/advisory gate:

  ```bash
  cargo install cargo-deny --locked
  ```

### Build

```bash
git clone <repository-url> tower
cd tower
cargo build -p core_engine        # produces target/debug/tower
```

### Run the MCP server

```bash
# Workspace root = current directory (or set --workspace-dir / $TOWER_WORKSPACE)
cargo run -p core_engine
# stderr: tower: initial scan complete — N files indexed
```

### Drive it over a pipe

```bash
# Handshake
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | cargo run -p core_engine -q

# List tools (7 native vfs_* tools)
echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' | cargo run -p core_engine -q

# Find a file
echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vfs_find_file","arguments":{"query":"main.rs"}}}' \
  | cargo run -p core_engine -q
```

Each request is a single newline-delimited JSON object on stdin; responses arrive on stdout.
No `Content-Length` header — unlike LSP.

---

## Quality gate

Run these checks in order before every merge (mirrors CI):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# Build wasm fixtures BEFORE cargo test (build.rs sets env paths to these .wasm files)
cargo build \
  -p hello_plugin \
  -p fixture_abi_mismatch \
  -p fixture_panic_plugin \
  -p fixture_loop_plugin \
  -p fixture_loop_hook_plugin \
  --target wasm32-wasip1

# Build the AST plugin (requires CC_wasm32_wasip1 / AR_wasm32_wasip1 — see getting-started.md)
cargo build -p plugin_ast --target wasm32-wasip1

# Host-side tests (wasm crates excluded from default-members)
cargo test --workspace \
  --exclude hello_plugin \
  --exclude plugin_ast \
  --exclude fixture_abi_mismatch \
  --exclude fixture_panic_plugin \
  --exclude fixture_loop_plugin \
  --exclude fixture_loop_hook_plugin

# plugin_ast host-side tests (Tree-sitter compiles natively, no WASI SDK needed)
cargo test -p plugin_ast

cargo deny check
```

---

## Workspace layout

```
crates/
├── core_engine/          Host binary (tower) + lib; domain / ports / adapters
├── plugin_sdk/           Distributable SDK: ABI types, Plugin trait, proc-macros
├── plugin_sdk_macros/    Proc-macro crate: #[plugin_main], #[plugin_export]
├── plugin_ast/           Reference AST plugin → wasm32-wasip1 (~1.2 MB release)
├── hello_plugin/         Minimal example plugin (cdylib + rlib)
├── fixture_abi_mismatch/ Test fixture: wrong ABI version
├── fixture_panic_plugin/ Test fixture: panicking guest
├── fixture_loop_plugin/  Test fixture: infinite-loop guest (fuel test)
└── fixture_loop_hook_plugin/ Test fixture: infinite-loop in hook handler
```

`default-members` covers `core_engine`, `plugin_sdk`, and `plugin_sdk_macros` only — the wasm
crates are built explicitly with `--target wasm32-wasip1`.

---

## Documentation

| Page | Contents |
|---|---|
| [`docs/getting-started.md`](docs/getting-started.md) | Prerequisites, build, quality gate, first MCP session |
| [`docs/architecture.md`](docs/architecture.md) | Hexagonal boundary, crate layout, ports, data flow, design decisions |
| [`docs/mcp-tools.md`](docs/mcp-tools.md) | Full MCP tool reference — wire protocol, all 7 native tools, AST plugin tools, error codes |
| [`docs/plugins.md`](docs/plugins.md) | Plugin authoring guide — SDK, ABI, build, fault isolation |
| [`docs/development.md`](docs/development.md) | Contributing, TDD workflow, CI pipeline, test conventions |
| [`docs/ADR/`](docs/ADR/) | Architecture Decision Records |
| [`docs/spikes/12a-tree-sitter-wasm-feasibility.md`](docs/spikes/12a-tree-sitter-wasm-feasibility.md) | WASI SDK recipe and Tree-sitter wasm feasibility investigation |
| [`project-brief.md`](project-brief.md) | Vision, objectives, functional scope |

---

## License

[EUPL-1.2](LICENSE).
