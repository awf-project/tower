# Documentation index

| Document | Purpose |
|---|---|
| [getting-started.md](getting-started.md) | Prerequisites, build commands, quality gate, first MCP session with copy-paste examples |
| [architecture.md](architecture.md) | Hexagonal boundary, crate layout, port signatures, data flow (startup → serve loop → watcher), plugin runtime, invariants table, design decisions |
| [mcp-tools.md](mcp-tools.md) | Wire protocol, session lifecycle, all 7 native `tower_*` tools, AST plugin tools (`tower_ast_get_outline`, `tower_ast_find_symbols`), JSON-RPC error codes |
| [towerignore.md](towerignore.md) | `.towerignore` index ignore source (git-independent), syntax, `tower init` scaffold, default template, BREAKING change for `.gitignore`-reliant workspaces |
| [plugins.md](plugins.md) | Plugin authoring guide: SDK, `#[plugin_main]`, host capabilities, lifecycle hooks, build recipes, ABI, fault isolation, worked examples |
| [development.md](development.md) | Spec-driven workflow, CI pipeline, testing conventions, hexagonal boundary rules, invariants |
| [ADR/](ADR/) | Architecture Decision Records (none yet; template and numbering convention in `ADR/README.md`) |

Related root-level files: [`project-brief.md`](../project-brief.md) (vision + functional scope),
[`AGENTS.md`](../AGENTS.md) (project guide for automated agents),
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) (canonical CI definition).
