# Tower

Tower is a core engine for a high-performance productivity tool: a virtual file system with a persistent
inverted index, parallel content search, safe mass text refactoring, and an out-of-process **native
extension** architecture — all exposed over a JSON-RPC 2.0 stdio interface following the
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
| **MCP server** | JSON-RPC 2.0 over stdin/stdout; native `tower_*` tools plus any extension tools, served from one surface |
| **Native extension host** | Out-of-process native sidecar extensions over a JSON-RPC 2.0 protocol on stdio; OS-process isolation, per-call timeouts, restart/backoff, and quarantine |
| **AST analysis** | `ast` extension — Tree-sitter outline and symbol search for Rust, Go, PHP |
| **Code intelligence** | `lsp` extension — diagnostics, definition, references, hover via a language-server bridge |
| **Single static binary** | No WASM, WASI SDK, JVM, Node, or container required at runtime |

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

# List tools (native tower_* tools, plus tower_<ext>_<tool> for any loaded extension)
echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' | cargo run -p core_engine -q

# Find a file
echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tower_find_file","arguments":{"query":"main.rs"}}}' \
  | cargo run -p core_engine -q

# List indexed directory entries
echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tower_list_dir","arguments":{"path":"src","recursive":true,"max_depth":1}}}' \
  | cargo run -p core_engine -q
```

Each request is a single newline-delimited JSON object on stdin; responses arrive on stdout.
No `Content-Length` header — unlike LSP.

### Install an extension

Extensions are out-of-process **native sidecar** binaries described by an `extension.toml` manifest.
`tower` discovers them at startup (resolved from `--extensions-dir`, then `$TOWER_EXTENSIONS_DIR`,
then the global XDG dir and `<workspace>/.tower/extensions/`). Drop a binary + manifest into a scope,
restart, and its tools appear in `tools/list` — no host recompile:

```bash
mkdir -p .tower/extensions/ast
cp target/debug/ast_extension     .tower/extensions/ast/
cp extensions/ast/extension.toml  .tower/extensions/ast/
cargo run -p core_engine -q     # tools/list now also exposes tower_ast_get_outline, tower_ast_find_symbols, …
```

No extensions serves the native tools; an extension that fails to spawn is skipped with a
stderr warning and never aborts startup. See [`docs/extensions.md`](docs/extensions.md) for details.

---

## Quality gate

Run these checks in order before every merge (mirrors CI):

```bash
cargo fmt --all --check                                   # make fmt-check
cargo clippy --workspace --all-targets -- -D warnings     # make clippy

# Build native extension binaries first so host integration tests can locate
# them under target/debug/. No WASM, no WASI SDK.
cargo build --workspace --bins
cargo test --workspace                                    # make test

cargo deny check                                          # make deny
```

Or run the whole sequence with `make gate`.

---

## Workspace layout

```
crates/                   Engine + shared protocol (host-side)
├── core_engine/          Host binary (tower) + lib; domain / ports / adapters
└── extension_protocol/   Shared host ↔ extension wire contract (types + serde only)

extensions/               Native sidecar extensions (separate binaries)
├── ast/                  Reference Tree-sitter AST extension (eager)
├── hello/                Minimal example extension (lazy)
├── lsp/                  Language-server bridge extension (lazy)
└── fixtures/             Test-only fault-isolation fixtures (not shipped)
```

The reference extensions are ordinary native binaries. Build them with
`cargo build --workspace --bins` before `cargo test --workspace` so host integration
tests can locate sidecars under `target/debug/`. There is no WASM build step and no WASI SDK.

---

## Documentation

| Page | Contents |
|---|---|
| [`docs/getting-started.md`](docs/getting-started.md) | Prerequisites, build, quality gate, first MCP session |
| [`docs/architecture.md`](docs/architecture.md) | Hexagonal boundary, crate layout, ports, data flow, design decisions |
| [`docs/mcp-tools.md`](docs/mcp-tools.md) | Full MCP tool reference — wire protocol, the native tools, extension tools, error codes |
| [`docs/extensions.md`](docs/extensions.md) | Extension authoring guide — native sidecars, the JSON-RPC protocol, capabilities, manifest, fault model |
| [`docs/development.md`](docs/development.md) | Contributing, TDD workflow, CI pipeline, test conventions |
| [`docs/ADR/`](docs/ADR/) | Architecture Decision Records |
| [`project-brief.md`](project-brief.md) | Vision, objectives, functional scope |

---

## License

[EUPL-1.2](LICENSE).
