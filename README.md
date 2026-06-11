# Tower

Tower is a core engine for a high-performance productivity tool: a virtual file system with a persistent
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
| **MCP server** | JSON-RPC 2.0 over stdin/stdout; 7 native `tower_*` tools plus any plugin tools, served from one surface |
| **WASM plugin host** | Drop a `.wasm` in the plugins directory — `tower` auto-loads it at startup inside a `wasmtime` sandbox with fuel + epoch compute bounds and automatic fault isolation |
| **AST analysis** | `ast` — Tree-sitter outline and symbol search for Rust, Go, PHP |
| **Single static binary** | No JVM, Node, or container required at runtime |

---

## 60-second quick start

### Install (prebuilt binary)

No toolchain required — download a prebuilt binary from GitHub Releases:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/awf-project/tower/main/scripts/install.sh | sh
```

Installs the latest stable release to `$HOME/.local/bin/tower`. Override via environment:

| Variable | Default | Purpose |
|---|---|---|
| `TOWER_VERSION` | `latest` | `latest`/`stable`, `dev` (rolling prerelease), or a tag like `v0.1.0` |
| `TOWER_INSTALL_DIR` | `$HOME/.local/bin` | Where the binary is installed |
| `TOWER_TARGET` | auto-detected | Force a target triple instead of auto-detecting |

```bash
curl -fsSL .../install.sh | TOWER_VERSION=dev sh      # rolling dev build
curl -fsSL .../install.sh | TOWER_VERSION=v0.1.0 sh   # pinned version
```

The installer verifies the published `.sha256` checksum when present. To build from source instead, follow the steps below.

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

# List tools (7 native tower_* tools, plus <plugin>/<tool> for any loaded plugin)
echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' | cargo run -p core_engine -q

# Find a file
echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tower_find_file","arguments":{"query":"main.rs"}}}' \
  | cargo run -p core_engine -q
```

Each request is a single newline-delimited JSON object on stdin; responses arrive on stdout.
No `Content-Length` header — unlike LSP.

### Drop & Play a plugin

`tower` scans a plugins directory at startup (resolved from `--plugins-dir`, then
`$TOWER_PLUGINS_DIR`, then `<workspace>/.tower/plugins/`). Drop a `.wasm` in, restart, and its
tools appear in `tools/list` — no host recompile:

```bash
mkdir -p .tower/plugins
cp target/wasm32-wasip1/release/ast.wasm .tower/plugins/
cargo run -p core_engine -q     # tools/list now also exposes ast/ast_get_outline + ast/ast_find_symbols
```

A missing/empty directory serves exactly the 7 native tools; a bad plugin is skipped with a stderr
warning and never aborts startup. See [`docs/plugins.md`](docs/plugins.md) for details.

---

## Quality gate

Run these checks in order before every merge (mirrors CI):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# Build wasm fixtures BEFORE cargo test (build.rs sets env paths to these .wasm files)
cargo build \
  -p hello \
  -p fixture_abi_mismatch \
  -p fixture_panic_plugin \
  -p fixture_loop_plugin \
  -p fixture_loop_hook_plugin \
  --target wasm32-wasip1

# Build the AST plugin (requires CC_wasm32_wasip1 / AR_wasm32_wasip1 — see getting-started.md)
cargo build -p ast --target wasm32-wasip1

# Host-side tests — default-members already scopes this to the host crates,
# so the wasm crates are skipped without any --exclude list.
cargo test

# ast host-side tests (Tree-sitter compiles natively, no WASI SDK needed)
cargo test -p ast

cargo deny check
```

---

## Workspace layout

```
crates/                   Engine + SDK (host-side, default-members)
├── core_engine/          Host binary (tower) + lib; domain / ports / adapters
├── plugin_sdk/           Distributable SDK: ABI types, Plugin trait, proc-macros
└── plugin_sdk_macros/    Proc-macro crate: #[plugin_main], #[plugin_export]

plugins/                  wasm32-wasip1 plugins (excluded from default-members)
├── ast/                  Reference AST plugin → wasm32-wasip1 (~1.2 MB release)
├── hello/                Minimal example plugin (cdylib + rlib)
└── fixtures/             Test-only wasm fixtures (not shipped)
    ├── fixture_abi_mismatch/     wrong ABI version
    ├── fixture_panic_plugin/     panicking guest
    ├── fixture_loop_plugin/      infinite-loop guest (fuel test)
    └── fixture_loop_hook_plugin/ infinite-loop in hook handler
```

`default-members` covers `core_engine`, `plugin_sdk`, and `plugin_sdk_macros` only — the wasm
crates under `plugins/` are built explicitly with `--target wasm32-wasip1`.

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
| [`project-brief.md`](project-brief.md) | Vision, objectives, functional scope |

---

## License

[EUPL-1.2](LICENSE).
