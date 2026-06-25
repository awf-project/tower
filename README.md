# Tower

Tower is a Rust core engine that gives MCP clients a fast, shared view of a workspace:
indexed files, content search, safe edits, semantic tools, lint fixes, and debug workflows.

It runs as one daemon per workspace. Each MCP session launched through Tower's stdio entrypoint
connects to that daemon, so multiple agents can inspect and edit the same project through the same
persisted index.

Architecture: **Domain-Driven Design + Hexagonal Architecture + Microkernel**.

## Why Tower

| Capability | What it gives clients |
|---|---|
| **Persistent workspace index** | Fast file lookup and directory listing from a `sled`-backed VFS. |
| **Parallel text search** | Rayon-backed grep across indexed file contents. |
| **Safe mutations** | Atomic writes and CAS-style `expected_version` checks for agent-safe edits. |
| **Shared daemon** | One watcher, index, and extension registry per workspace; many MCP clients. |
| **Native sidecar extensions** | Out-of-process extension binaries that contribute `tower_<ext>_*` MCP tools. |
| **Semantic workflows** | AST outline/symbol tools, LSP diagnostics/navigation, lint fixes, and opt-in DAP debugging. |
| **No runtime VM** | Native binaries only; no JVM, Node, WASM runtime, or container required at runtime. |

## Quick Start

### Install

Download a prebuilt binary from GitHub Releases:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/awf-project/tower/main/scripts/install.sh | sh
```

The installer writes `tower` to `$HOME/.local/bin` by default. You can override the release or
install directory:

```bash
curl -fsSL https://raw.githubusercontent.com/awf-project/tower/main/scripts/install.sh \
  | TOWER_VERSION=dev TOWER_INSTALL_DIR="$HOME/bin" sh
```

To build from source instead:

```bash
git clone <repository-url> tower
cd tower
cargo build --workspace --bins
```

The main binary is `target/debug/tower`.

### Initialize a Workspace

Run this once at the project root you want Tower to index:

```bash
tower init
```

This creates:

| File | Purpose |
|---|---|
| `.towerignore` | Authoritative ignore rules for the Tower index, independent of `.gitignore`. |
| `.tower/config.toml` | Local Tower configuration, including formatter/linter/debug settings. |

Secrets and generated output should be excluded in `.towerignore` before exposing a workspace to an
agent. If `.towerignore` is missing, Tower warns and indexes all non-hidden files except `.git/`.

### Connect an MCP Client

Tower's MCP entrypoint is the `mcp` subcommand:

```bash
tower mcp
```

Most MCP clients do not store that as one shell string. Configure the executable and arguments in the
shape your client expects. In JSON-like configs, that usually means:

```json
{
  "command": "tower",
  "args": ["mcp"]
}
```

If the client provides a CLI for registering MCP servers, pass `tower` as the command and `mcp` as
its argument using that client's syntax.

The `mcp` subcommand connects to the workspace daemon, spawning it if needed. The daemon owns the
index, watcher, storage, and loaded extensions, then exits after its configured idle timeout when no
clients remain.

Useful local checks:

```bash
tower status
tower shutdown
```

Workspace resolution is consistent across commands:

| Priority | Mechanism |
|---|---|
| 1 | `--workspace-dir <path>` |
| 2 | `TOWER_WORKSPACE=/path/to/project` |
| 3 | current working directory |

## Core Concepts

**The index is Tower-owned.** Tower persists workspace metadata under `.tower/db/`, watches file
changes, and serves file/search tools from the current index.

**`.towerignore` is the source of truth.** Tower does not inherit `.gitignore`. This keeps the agent
visibility policy explicit and project-local.

**Edits are CAS-aware.** Mutating tools can use an `expected_version` token returned by
`tower_read_file` to reject writes when a file changed since it was read.

**Extensions are native sidecars.** Extension binaries are discovered at startup, supervised by the
host, and exposed as namespaced MCP tools such as `tower_ast_get_outline` or
`tower_lint_check`. A failed extension is skipped or quarantined without taking down the daemon.

## Tool Surface

Native tools are always available. Extension tools appear when their sidecars are installed and
enabled.

| Area | Examples |
|---|---|
| Files and search | `tower_read_file`, `tower_list_dir`, `tower_find_file`, `tower_search_text` |
| Safe mutations | `tower_create_file`, `tower_edit_range`, `tower_global_replace`, `tower_delete_file` |
| AST | `tower_ast_get_outline`, `tower_ast_find_symbols`, `tower_ast_read_symbol` |
| LSP | `tower_lsp_diagnostics`, `tower_lsp_definition`, `tower_lsp_references`, `tower_lsp_hover` |
| Lint | `tower_lint_check`, `tower_lint_fix` |
| Debug | `tower_debug_launch`, `tower_debug_eval_at`, `tower_debug_record_and_find_origin` |

See [docs/mcp-tools.md](docs/mcp-tools.md) for the full wire protocol, tool schemas, responses, and
stable error codes.

## Documentation

| Page | Contents |
|---|---|
| [docs/getting-started.md](docs/getting-started.md) | Build, first MCP session, quality gate, and copy-paste examples. |
| [docs/towerignore.md](docs/towerignore.md) | `.towerignore` behavior, defaults, migration notes, and watcher limits. |
| [docs/architecture.md](docs/architecture.md) | Hexagonal boundary, ports/adapters, daemon flow, and invariants. |
| [docs/extensions.md](docs/extensions.md) | Native sidecar extension protocol, manifests, capabilities, and supervision. |
| [docs/mcp-tools.md](docs/mcp-tools.md) | Complete MCP tool reference and JSON-RPC details. |
| [docs/user-guide/semantic-edits.md](docs/user-guide/semantic-edits.md) | Safe AST and LSP edit workflows. |
| [docs/user-guide/lint-fixes.md](docs/user-guide/lint-fixes.md) | Previewing and applying structured linter fixes. |
| [docs/user-guide/debug-sessions.md](docs/user-guide/debug-sessions.md) | Interactive DAP sessions and rr-backed reverse-debug workflows. |
| [docs/development.md](docs/development.md) | Contribution workflow, CI checks, tests, and project conventions. |

## Development

The merge gate is:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --bins
cargo test --workspace
cargo deny check
```

Or run the project wrapper:

```bash
make gate
```

## License

[EUPL-1.2](LICENSE).
