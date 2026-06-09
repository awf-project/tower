# Core Engine — Project Guide

Rust **Core Engine** for a high-performance editor/productivity tool: a Virtual File System (VFS),
text processing, an embedded **WASM plugin host** (`wasmtime`), and an **MCP** (JSON-RPC over stdio)
interface. Architecture: **DDD + Hexagonal (Ports & Adapters) + Microkernel**.

> This file holds project-specific facts. General engineering conventions (persona, git, code style,
> TDD discipline) live in the user-global `~/.claude/CLAUDE.md` and are not repeated here.

## Current state: specs-first

The repo is **not yet scaffolded** (no `Cargo.toml` until spec `00`). Work is driven by an iterative
spec backlog in **`.agent/todo/`** — read `.agent/todo/README.md` first (roadmap, dependency DAG,
architecture wireframe, milestones). The source of intent is **`project-brief.md`**.

- 23 single-responsibility specs, format **EARS + Given/When/Then**, each with a TDD sequence.
- **Build them in dependency order.** Every spec must leave the tree in a **stable state**: all gates
  green before moving on.
- `.agent/` is git-ignored — specs are local working artifacts, not versioned.

## The golden rule (hexagonal boundary)

**The core domain imports no infrastructure.** Code in `domain/` must NOT import `sled`, `std::fs`,
`wasmtime`, `notify`, or any transport. It depends only on **port traits**. Infrastructure is wired in
adapters. If you reach for an I/O crate inside the domain, stop — you're crossing the boundary.

- Inbound ports (driving): `SearchUseCase`, `FileMutationUseCase`.
- Outbound ports (driven): `StoragePort`, `FileSystemPort`, `PluginHostPort`.
- Adapters: `SledStorageAdapter`, `RealFs` + scan, `NotifyWatcherAdapter`, MCP transport,
  `WasmtimeHostAdapter`.
- Every real adapter must pass the **same contract test suite** as its in-memory fake (spec `02`).

## Crate layout (Cargo workspace)

```
crates/
├── core_engine/   # Host binary — domain/ ports/ adapters/   (specs 00–11d)
├── plugin_sdk/    # Distributable SDK: shared types, ABI, macros  (spec 11a)
└── plugin_ast/    # Reference Tree-sitter plugin → wasm32-wasip1  (specs 12a–12d)
```

## Commands (target — available after spec 00)

```bash
cargo test --workspace                       # domain unit tests use in-memory doubles, zero disk I/O
cargo clippy --workspace -- -D warnings      # warnings are errors
cargo fmt --check
cargo deny check                             # license/advisory policy
cargo build -p plugin_ast --target wasm32-wasip1   # the WASM reference plugin
cargo run -p core_engine                     # MCP server over stdio (after spec 10b)
```

**Definition of "stable" / done** for any change: `cargo fmt --check` + `cargo clippy -- -D warnings`
+ `cargo test --workspace` all green. State it with evidence, never assume.

## Non-negotiable invariants

- **Domain purity**: 100% of the domain testable with in-memory test doubles — no real disk/DB in
  domain unit tests.
- **`FileId` is generational** (`index: u32` + `generation: u32`): a reused slot bumps generation so a
  stale `FileId` never resolves to a different file.
- **Atomic mutations**: writes go through the shadow-file pattern (`.tmp_write` → durable flush →
  OS-atomic `fs::rename`). No torn files under crash. `.tmp_write` artifacts are never indexed.
- **Zero lock contention**: shared VFS state behind fine-grained, short-held `RwLock`; the watcher
  (writer) must not starve MCP (readers).
- **Plugin fault isolation**: a plugin panic/trap/infinite-loop must never take down the host or sever
  the MCP link — enforced via wasmtime **fuel + epoch interruption** and trap catching (spec 11d).
- **Capability security**: WASM guests reach the workspace only through explicit host functions; no
  net, raw fs, or storage-cache access.
- **Single static binary**: no JVM, no Node, no container required at runtime.

## Working conventions specific to this repo

- Per the global guide: write/update the **ASCII architecture wireframe** in `.agent/todo/README.md`
  when structure changes — keep it the single source of truth for the hexagon.
- Index source of truth = **Sled** (persisted + reloaded on start); index invalidation is transactional
  and joint with file-record writes (spec 04b).
- Tree-sitter-in-WASM is the #1 technical risk: it is gated behind the feasibility spike (`12a`) with an
  explicit escalation path. Do not silently weaken the requirement.

## ZPM Project Memory

This project uses ZPM memory segments as its knowledge base. **Query ZPM before starting work. Update ZPM as you learn.**

### Segments

| Segment | Purpose | Mount |
|---------|---------|-------|
| `default` | Project knowledge: ADRs, decisions, observations, conventions, architecture facts | auto |
| `feedback` | Learned rules from past implementations/reviews — queryable by file pattern | auto |
| `pr_<branch>` | PR tracking: TODOs, stubs, mocks, blocking issues, completion gate | per-implementation |

### Read before acting

```bash
# What decisions exist about this area?
zpm query-logic --goal "decision(Id, What, Why)" --memory default
# What feedback rules apply to files I'm touching?
zpm query-logic --goal "rule(Id, Cat, Desc, Prio, Src)" --memory feedback
# What rules apply specifically to a file/directory?
zpm query-logic --goal "applicable(RuleId, 'src/path/file.ext')" --memory feedback
# What ADRs are active?
zpm query-logic --goal "current_decision(Id, Decision)" --memory default
# Any architecture violations?
zpm query-logic --goal "integrity_violation(Kind, File)" --memory default
```

### Write as you work

```bash
# Observation: discovered something non-obvious
zpm remember-fact --fact "observation(id, Category, 'Description', 'YYYY-MM-DD')" --memory default
# Categories: pattern | convention | quirk | dependency | performance

# Decision: made an architectural choice
zpm remember-fact --fact "decision(id, 'What', 'Why', 'Trade-off')" --memory default

# Feedback rule: a mistake teaches a reusable lesson
zpm remember-fact --fact "rule(rule_id, Category, 'Imperative rule', Priority, Source)" --memory feedback
zpm remember-fact --fact "trigger(rule_id, 'pattern', Scope)" --memory feedback
# Categories: architecture | pitfall | test | review | style
# Priority: high | medium | low — Scope: file | directory | project
```

### What NOT to store

- File contents, git status, directory listings — ephemeral
- Anything already in code comments or documentation
- Duplicate of an existing fact — query first

### Architecture Rules

Query: `zpm query-logic --goal "rule(Id, architecture, Desc, Prio, Src)" --memory feedback`

### Test Conventions

Query: `zpm query-logic --goal "rule(Id, test, Desc, Prio, Src)" --memory feedback`

### Common Pitfalls

Query: `zpm query-logic --goal "rule(Id, pitfall, Desc, Prio, Src)" --memory feedback`

### Review Standards

Query: `zpm query-logic --goal "rule(Id, review, Desc, Prio, Src)" --memory feedback`
