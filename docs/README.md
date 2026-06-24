# Documentation index

| Document | Purpose |
|---|---|
| [getting-started.md](getting-started.md) | Prerequisites, build commands, quality gate, first MCP session with copy-paste examples |
| [user-guide/debug-sessions.md](user-guide/debug-sessions.md) | How to configure and drive interactive Debug Adapter Protocol sessions and one-shot `tower_debug_eval_at` probes |
| [user-guide/lint-fixes.md](user-guide/lint-fixes.md) | How to preview and apply structured linter fixes safely with `tower_lint_fix` |
| [architecture.md](architecture.md) | Hexagonal boundary, crate layout, port signatures, data flow (startup → serve loop → watcher), extension host runtime, invariants table, design decisions |
| [mcp-tools.md](mcp-tools.md) | Wire protocol, session lifecycle, the native `tower_*` tools, extension tools (`tower_ast_get_outline`, `tower_lint_check`, `tower_lint_fix`, `tower_debug_launch`, `tower_debug_eval_at`, ...), JSON-RPC error codes |
| [towerignore.md](towerignore.md) | `.towerignore` index ignore source (git-independent), syntax, `tower init` scaffold, default template, BREAKING change for `.gitignore`-reliant workspaces |
| [extensions.md](extensions.md) | Extension authoring guide: out-of-process native sidecars, the JSON-RPC 2.0 protocol & lifecycle, capability callbacks, the `extension.toml` manifest, discovery/activation, supervision/fault model, worked examples |
| [development.md](development.md) | Spec-driven workflow, CI pipeline, testing conventions, hexagonal boundary rules, invariants |
| [ADR/](ADR/) | Architecture Decision Records (none yet; template and numbering convention in `ADR/README.md`) |

Related root-level files: [`project-brief.md`](../project-brief.md) (vision + functional scope),
[`AGENTS.md`](../AGENTS.md) (project guide for automated agents),
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) (canonical CI definition).
