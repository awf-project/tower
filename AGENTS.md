# Core Engine — Project Guide

Rust **Core Engine** for a high-performance editor/productivity tool: a Virtual File System (VFS),
text processing, an **out-of-process sidecar extension host** (JSON-RPC 2.0 over stdio), and an
**MCP** (JSON-RPC over stdio) interface. Architecture: **DDD + Hexagonal (Ports & Adapters) +
Microkernel**.

> This file holds project-specific facts. General engineering conventions (persona, git, code style,
> TDD discipline) live in the user-global and are not repeated here.

## The golden rule (hexagonal boundary)

**The core domain imports no infrastructure.** Code in `domain/` must NOT import `sled`, `std::fs`,
`notify`, or any transport. It depends only on **port traits**. Infrastructure is wired in
adapters. If you reach for an I/O crate inside the domain, stop — you're crossing the boundary.

- Inbound ports (driving): `SearchUseCase`, `FileMutationUseCase`.
- Outbound ports (driven): `StoragePort`, `FileSystemPort`, `ExtensionHostPort`.
- Adapters: `SledStorageAdapter`, `RealFs` + scan, `NotifyWatcherAdapter`, MCP transport,
  `SidecarHostAdapter`.
- Every real adapter must pass the **same contract test suite** as its in-memory fake (spec `02`).

## Crate layout (Cargo workspace)

```
crates/                    # Engine + protocol (host-side, default-members)
├── core_engine/            # Host binary — domain/ ports/ adapters/   (specs 00–10b, 22–28)
└── extension_protocol/     # Shared wire contract: JSON-RPC 2.0 types + manifest schema (spec 21)
                              # types+serde only; no host/process/transport; used by host and extensions

extensions/                 # Out-of-process sidecar extensions (all are workspace + default-members)
├── ast/                    # AST + tree-sitter extension (specs 26)
├── lsp/                    # LSP bridge extension — absorbs adapters/lsp/* (spec 27)
├── hello/                  # Minimal example extension
├── test_helper/            # Contract test fixture extension (specs 23–24)
└── fixtures/               # Fault-isolation test fixtures (spec 24)
```

## Commands (available after spec 00)

```bash
cargo test --workspace                       # domain unit tests use in-memory doubles (zero disk I/O);
                                             # also compiles every native extension binary into target/debug/
                                             # so host e2e/integration tests can spawn the real sidecars
cargo clippy --workspace -- -D warnings      # warnings are errors
cargo fmt --check
cargo deny check                             # license/advisory policy
cargo run -p core_engine                     # MCP server over stdio
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
- **Extension fault isolation**: an extension panic/crash/infinite-loop must never take down the host
  or sever the MCP link — enforced via wall-clock timeout + restart + quarantine at the OS process
  boundary (spec 24). A child that ignores shutdown is killed (SIGTERM → SIGKILL).
- **Capability security**: sidecar extensions reach the workspace only through explicit HostCall
  dispatch in `SidecarHostAdapter`; no direct net, raw fs, or storage-cache access.
- **Single static binary**: no JVM, no Node, no container required at runtime.

## Working conventions specific to this repo

- Per the global guide: write/update the **ASCII architecture wireframe** in `.agent/todo/README.md`
  when structure changes — keep it the single source of truth for the hexagon.
- Index source of truth = **Sled** (persisted + reloaded on start); index invalidation is transactional
  and joint with file-record writes (spec 04b).
- **Local config**: `<workspace>/.tower/config.toml` (TOML) holds per-project settings.
  `[extensions] disabled = ["<name>"]` skips loading the named extension by manifest name.
  `[lsp.<lang>]` configures a language-server binding for the `lsp_extension` sidecar.
  Absent file → defaults; malformed file → startup fails (exit 1). Parser: `adapters/config`.

## KNOWN PROTOCOL HAZARDS (sidecar extension multiplexing)

The sidecar JSON-RPC protocol multiplexes, on the child's stdin, **both** host requests
(`initialize`/`invokeTool`/`deliverEvent`/`shutdown`) **and** host responses to the child's
capability HostCalls. Extensions that do a single-pass id-only match when waiting for a HostCall
response will silently discard host requests that arrive in the window — causing a permanent deadlock
(host times out waiting for its response).

**Required mitigations for any new extension that makes HostCalls:**

- (a) Perform all initialize-time HostCalls **before** sending the `Initialized` response, so
  `spawn()` cannot hand control to the host until the child is back in its main read loop.
- (b) For a long-lived extension that makes HostCalls outside a single request turn (e.g. the push
  forwarder thread in the LSP extension), **queue** inbound host requests encountered while awaiting
  a HostCall response rather than discarding non-matching frames.
- (c) Run any new extension's e2e suite in parallel ≥ 20 times to flush concurrent-spawn races.

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
