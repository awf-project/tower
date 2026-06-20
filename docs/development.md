# Development & Contributing

This document covers: spec-driven workflow, quality gate, testing conventions,
CI pipeline, hexagonal boundary rules, and the TDD-based build process used to
construct tower.

> Since the extension-system migration (spec 20) the engine is a **single static
> binary** with out-of-process **native** sidecar extensions (`extensions/*`).
> There is **no WASM build step and no WASI SDK** — `cargo test --workspace`
> compiles every native extension binary and the host locates them under
> `target/debug/`. The author-facing extension guide is [extensions.md](extensions.md).

---

## Spec-driven workflow

All features were built from a backlog of single-responsibility specifications
in `.agent/todo/` (git-ignored; local working artifacts only). Each spec is
written in **EARS + Given/When/Then** format and includes an explicit TDD
sequence. The ordering follows a dependency DAG documented in
`.agent/todo/README.md`, which also holds the authoritative ASCII architecture
wireframe.

The contract for merging any spec:

> **Every spec must leave the tree in a stable state** — all quality gates green
> before moving to the next spec.

Specs are not versioned in the repository. The source of intent is
`project-brief.md`. If you need to understand why a decision was made, check the
ADR directory at `docs/ADR/` and the decision comments embedded in the relevant
source files.

---

## Quality gate

Every commit — and every spec — must pass these four checks in this exact order.
The `Makefile` mirrors them; `make gate` runs the whole sequence.

```sh
# 1. Formatting
cargo fmt --all --check            # make fmt-check

# 2. Linting (warnings are hard errors)
cargo clippy --workspace --all-targets -- -D warnings    # make clippy

# 3. Run the full workspace test suite. This compiles every native extension
#    binary (extensions/*) too, so the host integration tests can locate them
#    under target/debug/. No WASM build, no WASI SDK.
cargo test --workspace             # make test

# 4. License and advisory policy
cargo deny check                   # make deny
```

`cargo-deny` must be installed once: `cargo install cargo-deny --locked`.

### Makefile targets

The `Makefile` is the developer task runner; `make help` lists everything.

| Target           | Command |
|------------------|---------|
| `make build`     | `cargo build --workspace` (host + native extensions, debug) |
| `make release`   | `cargo build --release -p core_engine` |
| `make run`       | `cargo run -p core_engine` (MCP server over stdio) |
| `make fmt`       | `cargo fmt --all` |
| `make fmt-check` | `cargo fmt --all --check` (gate step 1) |
| `make clippy`    | `cargo clippy --workspace --all-targets -- -D warnings` (gate step 2) |
| `make test`      | `cargo test --workspace` (gate step 3) |
| `make deny`      | `cargo deny check` (gate step 4) |
| `make gate`      | full quality gate: fmt-check + clippy + test + deny |
| `make dist`      | package a release tarball + sha256 for the host target |
| `make install`   | build release and install `tower` to `$INSTALL_DIR` (default `~/.local/bin`) |
| `make clean`     | `cargo clean` + remove `dist/` |

---

## Toolchain and prerequisites

The toolchain is pinned in `rust-toolchain.toml` at the workspace root:

```toml
[toolchain]
channel = "1.96.0"
components = ["rustfmt", "clippy"]
profile = "minimal"
```

`rustup` installs this automatically on first use. No extra build target is
required: the engine and the native extensions all build for the host target.
(The previous WASM model pinned `wasm32-wasip1` and required a WASI SDK; neither
is needed any more.)

---

## CI pipeline

The single `quality` job in `.github/workflows/ci.yml` runs on every push and
pull request on `ubuntu-latest`. Steps in order:

1. `actions/checkout@v4`
2. `actions-rust-lang/setup-rust-toolchain@v1` — reads `rust-toolchain.toml`
3. `Swatinem/rust-cache@v2` — Cargo build cache
4. `cargo fmt --all --check`
5. `cargo clippy --workspace --all-targets -- -D warnings`
6. `cargo test --workspace` (also compiles every native extension binary)
7. `EmbarkStudios/cargo-deny-action@v2`

There is no WASI SDK step and no separate WASM build step: native extensions are
ordinary workspace members compiled by `cargo test --workspace`.

---

## Hexagonal boundary rules

The architecture enforces a hard boundary between the domain and infrastructure.
Violating it is not a style issue — it breaks testability and couples business
logic to runtime choices.

**The domain imports no infrastructure.** Code under `crates/core_engine/src/domain/`
carries `#![forbid(unsafe_code)]` and must not import `sled`, `std::fs`,
`std::process`, `notify`, or any transport. The domain depends only on port traits
defined in `crates/core_engine/src/ports/` and the pure `extension_protocol` types.

```
domain/            # Pure business logic. No I/O. #![forbid(unsafe_code)].
  workspace.rs     # ProjectWorkspace — the aggregate root
  file_id.rs       # FileId { index: u32, generation: u32 } — generational
  extension_host/  # ExtensionRegistry, ExtensionInstance trait, quarantine policy
  grep/            # Text search
  mutation/        # File write operations
  refactor/        # Global replace
  ...

ports/             # Trait definitions only. No concrete types.
  inbound.rs       # SearchUseCase, FileMutationUseCase (driving ports)
  storage.rs       # StoragePort (driven)
  filesystem.rs    # FileSystemPort (driven)
  ast_index.rs     # AstIndexPort (driven)
  extension_host.rs # ExtensionHostPort (driven)

adapters/          # Concrete implementations. Imports sled, std::process, notify, std::fs.
  storage/         # SledStorageAdapter
  fs/              # RealFs
  watcher/         # NotifyWatcherAdapter
  mcp/             # JSON-RPC 2.0 stdio transport, ExtensionMergedRegistry
  config/          # .tower/config.toml parser
  extension/       # SidecarHostAdapter, ExtensionSupervisor, discovery,
                   #   host_deps, path_validation — the only std::process code
  in_memory_fs.rs       # Test double for FileSystemPort
  in_memory_storage.rs  # Test double for StoragePort
```

If you reach for an infrastructure crate inside `domain/`, stop — you are
crossing the boundary.

---

## Testing conventions

### Domain unit tests

Domain tests live inside their modules under `#[cfg(test)]` and use only
in-memory test doubles. No real disk, no sled, no subprocess.

```rust
// Correct: wire up fakes through trait objects
let mut storage = InMemoryStorage::new();
let mut fs = InMemoryFs::new();
index_file(&mut storage as &mut dyn StoragePort, &mut fs as &mut dyn FileSystemPort, ...);
```

`InMemoryFs` provides atomic rename (single HashMap operation, no observable gap)
and filters `.tmp_write` artifacts from `scan`, matching the contract of `RealFs`.

### Contract tests for adapters

Every outbound port has a reusable contract test suite defined as a declarative
macro in `crates/core_engine/src/test_support/`. Any new adapter must be wired
through the same macro to guarantee behavioral equivalence with the in-memory
fake:

```rust
// crates/core_engine/src/adapters/contract_tests.rs
storage_contract_tests!(InMemoryStorage::new);
filesystem_contract_tests!(InMemoryFs::new);
```

When adding `SledStorageAdapter` or `RealFs` tests, expand the same macros
against those concrete types. This is enforced by the spec: "every real adapter
must pass the same contract test suite as its in-memory fake."

### Integration tests

Integration tests live in `crates/core_engine/tests/` and exercise full
adapter-wired scenarios:

| File | Covers |
|------|--------|
| `integration_mcp_native_tools.rs` | JSON-RPC over stdio, the native VFS tools |
| `integration_global_replace.rs` | Parallel mass find-and-replace, error cases |
| `integration_mutation.rs` | Shadow-file atomic writes |
| `ast_e2e.rs` | AST outline + symbol search via MCP, end-to-end through the `ast` extension |

Native extension binaries are located through `CARGO_BIN_EXE_<name>` env vars that
Cargo sets for workspace binary crates (e.g. `CARGO_BIN_EXE_ast_extension`), so
`cargo test --workspace` builds and wires them automatically — no separate build
step and no `build.rs` artifact-locating dance.

### Extension logic tested on the host

The `ast` extension's outline/symbol parsing is plain native Rust, so its unit
tests run as ordinary `cargo test` — no sandbox, no WASI SDK. This keeps the
Tree-sitter logic fast to iterate on, and the extension binary is exercised
end-to-end via `ast_e2e.rs`.

### Fault isolation tests

Extension fault isolation is exercised through the sidecar adapter and supervisor
(`adapters/extension/`): a child that crashes, hangs past the per-call timeout, or
violates the protocol yields the corresponding `ExtensionFault`
(`Crashed` / `Timeout` / `ProtocolError`), and repeated faults drive the
`ExtensionRegistry` quarantine policy (`MAX_CONSECUTIVE_FAILURES = 3`). The
test-only fault fixtures live under `extensions/fixtures/`.

---

## Non-negotiable invariants

These hold at every commit:

- **Domain purity**: `domain/` is 100% testable with in-memory doubles. No real
  disk, DB, subprocess, or runtime in domain unit tests.
- **`FileId` is generational**: `struct FileId { index: u32, generation: u32 }`.
  A reused slot increments `generation` so a stale `FileId` can never silently
  resolve to a different file.
- **Atomic file writes**: all content writes use the shadow-file pattern —
  write to `<path>.tmp_write`, flush, then OS-atomic `fs::rename`. No torn
  writes on crash. `.tmp_write` artifacts are never indexed by the VFS.
- **Zero lock contention**: `EngineState` is behind `Arc<RwLock<EngineState>>`;
  critical sections are short and contain no blocking I/O. The fs watcher
  (writer) must not starve MCP handlers (readers).
- **Extension fault isolation**: a child crash, hang (per-call timeout), or
  protocol violation must not crash the host process or sever the MCP link.
  Enforced by the OS process boundary plus `SidecarHostAdapter` (timeout/kill)
  mapping faults to `ExtensionFault`.
- **Capability security**: extensions reach the workspace only through declared
  capability callbacks routed to outbound ports; an undeclared `HostCall` is
  rejected, and path arguments are validated (no `..` traversal, no absolute or
  empty paths).
- **Protocol version guard**: the host rejects an extension whose
  `protocol_version` does not match `extension_protocol::PROTOCOL_VERSION`
  (currently `1`) at the `initialize` handshake.
- **Quarantine**: an extension is quarantined after `MAX_CONSECUTIVE_FAILURES`
  (= 3) consecutive faults; a single success resets the counter.
- **No panic in `ToolRegistry` implementations**: all runtime failures must be
  returned as `ToolError::ExecutionFailed`. Panics inside tool handlers would
  cross the fault isolation boundary.

---

## How the project was built

Tower was constructed through an orchestrated TDD workflow applied spec by spec:

1. **Implement (TDD)**: RED — write the test. GREEN — make it pass with the
   minimal implementation. REFACTOR — clean up without breaking the gate.
2. **Adversarial review**: each implementation was reviewed through multiple
   lenses (architecture, security, performance, correctness) before the next
   spec began.
3. **Remediate**: review findings were addressed before the spec was considered
   done.
4. **Gate check**: `fmt + clippy + test` green — only then move to the next spec.

No spec left the tree in a broken state: the full quality gate
(`fmt + clippy + test + deny`) was green at every step.

The dependency order of the specs is preserved in `.agent/todo/README.md`.
The hexagonal architecture ensured that specs could be developed in isolation:
domain specs never touched adapters, and adapter specs never touched domain logic
beyond port traits. The most recent slice (specs 20–29) replaced the embedded WASM
plugin host with the out-of-process native extension model described in
[extensions.md](extensions.md), without changing the external MCP tool contract.

---

## Related docs

- [docs/ADR/](ADR/) — architectural decision records
- [project-brief.md](../project-brief.md) — the original product intent
- [AGENTS.md](../AGENTS.md) — crate layout, port/adapter names, invariants (the authoritative quick-reference for contributors)
- [.github/workflows/ci.yml](../.github/workflows/ci.yml) — canonical CI definition
