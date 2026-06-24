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
cargo build --workspace --bins               # builds native sidecar binaries into target/debug/
cargo test --workspace                       # domain unit tests use in-memory doubles (zero disk I/O);
                                             # host e2e/integration tests spawn the prebuilt sidecars
cargo clippy --workspace --all-targets -- -D warnings  # warnings are errors
cargo fmt --check
cargo deny check                             # license/advisory policy
cargo run -p core_engine                     # MCP server over stdio
tower mcp                                    # MCP stdio client: connect-or-spawn the daemon, relay
tower daemon                                 # run the shared daemon in the foreground
tower status                                 # snapshot of the running daemon
tower shutdown                               # stop the running daemon
```

**Definition of "stable" / done** for any change: `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings`
+ `cargo build --workspace --bins` + `cargo test --workspace` all green. State it with evidence, never assume.

## MCP tool surface (JSON-RPC over stdio)

**Multi-agent model.** Each agent launches `tower mcp`, a thin stdio client that
connects to (or spawns) a single per-workspace **daemon** holding the Sled index,
watcher, and extensions. The daemon listens on `<workspace>/.tower/daemon.sock` and
serves one MCP session per client over shared state, so several agents can drive the
same workspace concurrently (writes stay safe via the existing `expected_version`
CAS). The daemon self-terminates after `[daemon] idle_timeout_secs` (default 30s)
with no connected clients. **Migration:** point MCP client configs at `tower mcp`
(was `tower`).

The `core_engine` binary exposes these tools to MCP clients. All `path` arguments are
**workspace-relative**. Mutating tools accept an optional `expected_version` (hex SHA-256 from a
prior `tower_read_file` with `with_version: true`) for **optimistic compare-and-swap** (spec 18): the
write is rejected with `PreconditionFailed` if the file changed since it was read.

### VFS — files & search (host-side)

| Tool | Purpose |
|------|---------|
| `tower_read_file` | Read raw UTF-8; `with_version: true` returns a `version` CAS token. |
| `tower_create_file` | Create/overwrite with `content`; CAS via `expected_version`. |
| `tower_edit_range` | Splice `replacement` into byte range `[start_byte, end_byte)` (UTF-8 boundaries; empty = delete; equal bytes = insert); atomic commit; CAS. |
| `tower_global_replace` | Replace every `target` with `replacement` across all indexed files; per-file CAS via `expected_versions` map (path → SHA-256) — conflicts land in `TxReport.errors`, non-conflicting files still commit. |
| `tower_create_directory` | Recursive `mkdir`. |
| `tower_delete_file` | Delete a file. |
| `tower_list_dir` | List indexed files and synthesized directories under a workspace-relative directory path. |
| `tower_find_file` | Match a substring/fuzzy `query` against file paths. |
| `tower_search_text` | Grep `pattern` across all indexed file contents. |
| `tower_reindex` | Full workspace re-scan; rebuild file + text-search indexes (reconciles external create/delete). |

### AST — structural navigation (`ast` extension, spec 26)

Tree-sitter backed; error-tolerant so comments/strings yield no false positives. Supports `.rs`, `.go`, `.php`.
Symbol kinds: `function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class`.

| Tool | Purpose |
|------|---------|
| `tower_ast_get_outline` | Structural outline (functions, structs, methods, traits, enums, impl blocks) of one file. |
| `tower_ast_find_symbols` | Precise definition locations of a named symbol of a given `kind` within one file. |
| `tower_ast_read_symbol` | Read only a named symbol's source span (byte slice + kind + start/end rows), not the whole file. |
| `tower_ast_search_symbols` | Cross-file lookup against the in-memory symbol index (kept fresh by `fileIndexed`/`fileChanged` events). |
| `tower_ast_reindex` | Force a full whole-project symbol reindex (cold cache / large external change). |

### LSP — semantic queries (`lsp` extension, spec 27)

Require a configured language server for the file extension (`[lsp.<lang>]` in `.tower/config.toml`).
Positions are **0-based** `line` + **UTF-16** `character` offset.

| Tool | Purpose |
|------|---------|
| `tower_lsp_definition` | Go to definition of the symbol at a position. |
| `tower_lsp_references` | Find all references to the symbol at a position. |
| `tower_lsp_hover` | Hover info for the symbol at a position. |
| `tower_lsp_diagnostics` | Errors/warnings for a file. |

### Debug — interactive sessions (`debug` extension, spec 33)

Opt-in; tools appear only when `[debug.<language>]` is configured and the `debug` extension is
enabled. A discovered `debug` extension takes priority; otherwise the bundled sidecar is used when
`debug_extension` is available next to `tower`.

| Tool | Purpose |
|------|---------|
| `tower_debug_launch` | Launch a configured Debug Adapter Protocol session; returns `session_id` and initial stop state. |
| `tower_debug_set_breakpoints` | Replace the breakpoint set for a source path within a session. |
| `tower_debug_continue` | Resume execution until stop, termination, or timeout. |
| `tower_debug_step` | Step execution for a session/thread until the adapter reports a stop. |
| `tower_debug_pause` | Pause a running session/thread. |
| `tower_debug_threads` | List threads for a debug session. |
| `tower_debug_stack` | Read stack frames for a thread. |
| `tower_debug_variables` | Read variables for a DAP variables reference. |
| `tower_debug_evaluate` | Evaluate an expression in a stack frame. |
| `tower_debug_terminate` | Terminate a debug session and clean up the adapter process. |
| `tower_debug_disconnect` | Disconnect from a debug session. |
| `tower_debug_sessions` | List active debug sessions and their last known state. |

### Linting (`lint` extension)

Runs configured linters from `.tower/config.toml`; unsupported paths return a successful unsupported
result rather than a transport error.

| Tool | Purpose |
|------|---------|
| `tower_lint_check` | Run configured linters for one workspace-relative path, or all supported indexed files when `path` is omitted. |
| `tower_lint_fix` | Apply structured lint fixes for one path or all supported indexed files; supports `dry_run` and `unsafe`. |

### Formatting

| Tool | Purpose |
|------|---------|
| `tower_fmt_format` | Enqueue format jobs; `{path}` formats one file, `{}` formats all indexed files. Returns `{requested, accepted, dropped}` immediately — does **not** wait for completion. |

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
- **No runtime VM**: ships as a self-contained native build — no JVM, no Node, no container required
  at runtime. (Third-party Rust crates, including the official MCP SDK `rmcp`, are linked in normally;
  the constraint is about not requiring an external runtime, not about avoiding dependencies.)

## Working conventions specific to this repo

- Per the global guide: write/update the **ASCII architecture wireframe** in `.agent/todo/README.md`
  when structure changes — keep it the single source of truth for the hexagon.
- Index source of truth = **Sled** (persisted + reloaded on start); index invalidation is transactional
  and joint with file-record writes (spec 04b).
- **Local config**: `<workspace>/.tower/config.toml` (TOML) holds per-project settings.
  `[extensions] disabled = ["<name>"]` skips loading the named extension by manifest name.
  `[lsp.<lang>]` configures a language-server binding for the `lsp_extension` sidecar.
  `[daemon] idle_timeout_secs` — seconds with no connected clients before the daemon exits (default 30).
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


## Tower MCP Server (code intelligence)

This project is indexed by the [tower](https://github.com/awf-project/tower) MCP server, which
continuously watches the workspace. **Prefer tower tools over raw file scans**: `tower_search_text`
instead of grepping the tree, `tower_ast_*` instead of reading whole files to find a symbol, and the
CAS-guarded mutators for safe edits. All `path` arguments are **workspace-relative**.

Mutating tools accept an optional `expected_version` (hex SHA-256 from a prior `tower_read_file` with
`with_version: true`) for **optimistic compare-and-swap**: the write is rejected with
`PreconditionFailed` if the file changed since it was read.

### VFS — files & search (host-side)

| Tool | Purpose |
|------|---------|
| `tower_read_file` | Read raw UTF-8; `with_version: true` returns a `version` CAS token. |
| `tower_create_file` | Create/overwrite with `content`; CAS via `expected_version`. |
| `tower_edit_range` | Splice `replacement` into byte range `[start_byte, end_byte)` (UTF-8 boundaries; empty = delete; equal bytes = insert); atomic commit; CAS. |
| `tower_global_replace` | Replace every `target` with `replacement` across all indexed files; per-file CAS via `expected_versions` map (path → SHA-256) — conflicts land in `TxReport.errors`, non-conflicting files still commit. |
| `tower_create_directory` | Recursive `mkdir`. |
| `tower_delete_file` | Delete a file. |
| `tower_list_dir` | List indexed files and synthesized directories under a workspace-relative directory path. |
| `tower_find_file` | Match a substring/fuzzy `query` against file paths. |
| `tower_search_text` | Grep `pattern` across all indexed file contents. |
| `tower_reindex` | Full workspace re-scan; rebuild file + text-search indexes (reconciles external create/delete). |

### AST — structural navigation (`ast` extension)

Tree-sitter backed; error-tolerant so comments/strings yield no false positives. Supports `.rs`, `.go`, `.php`.
Symbol kinds: `function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class`.

| Tool | Purpose |
|------|---------|
| `tower_ast_get_outline` | Structural outline (functions, structs, methods, traits, enums, impl blocks) of one file. |
| `tower_ast_find_symbols` | Precise definition locations of a named symbol of a given `kind` within one file. |
| `tower_ast_read_symbol` | Read only a named symbol's source span (byte slice + kind + start/end rows), not the whole file. |
| `tower_ast_search_symbols` | Cross-file lookup against the in-memory symbol index (kept fresh by `fileIndexed`/`fileChanged` events). |
| `tower_ast_reindex` | Force a full whole-project symbol reindex (cold cache / large external change). |

### LSP — semantic queries (`lsp` extension)

Require a configured language server for the file extension (`[lsp.<lang>]` in `.tower/config.toml`).
Positions are **0-based** `line` + **UTF-16** `character` offset.

| Tool | Purpose |
|------|---------|
| `tower_lsp_definition` | Go to definition of the symbol at a position. |
| `tower_lsp_references` | Find all references to the symbol at a position. |
| `tower_lsp_hover` | Hover info for the symbol at a position. |
| `tower_lsp_diagnostics` | Errors/warnings for a file. |

### Debug — interactive sessions (`debug` extension)

Opt-in; tools appear only when `[debug.<language>]` is configured and the `debug` extension is
enabled. A discovered `debug` extension takes priority; otherwise the bundled sidecar is used when
`debug_extension` is available next to `tower`.

| Tool | Purpose |
|------|---------|
| `tower_debug_launch` | Launch a configured Debug Adapter Protocol session; returns `session_id` and initial stop state. |
| `tower_debug_set_breakpoints` | Replace the breakpoint set for a source path within a session. |
| `tower_debug_continue` | Resume execution until stop, termination, or timeout. |
| `tower_debug_step` | Step execution for a session/thread until the adapter reports a stop. |
| `tower_debug_pause` | Pause a running session/thread. |
| `tower_debug_threads` | List threads for a debug session. |
| `tower_debug_stack` | Read stack frames for a thread. |
| `tower_debug_variables` | Read variables for a DAP variables reference. |
| `tower_debug_evaluate` | Evaluate an expression in a stack frame. |
| `tower_debug_terminate` | Terminate a debug session and clean up the adapter process. |
| `tower_debug_disconnect` | Disconnect from a debug session. |
| `tower_debug_sessions` | List active debug sessions and their last known state. |

### Linting (`lint` extension)

Runs configured linters from `.tower/config.toml`; unsupported paths return a successful unsupported
result rather than a transport error.

| Tool | Purpose |
|------|---------|
| `tower_lint_check` | Run configured linters for one workspace-relative path, or all supported indexed files when `path` is omitted. |
| `tower_lint_fix` | Apply structured lint fixes for one path or all supported indexed files; supports `dry_run` and `unsafe`. |

### Formatting

| Tool | Purpose |
|------|---------|
| `tower_fmt_format` | Enqueue format jobs; `{path}` formats one file, `{}` formats all indexed files. Returns `{requested, accepted, dropped}` immediately — does **not** wait for completion. |
