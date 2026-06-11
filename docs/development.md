# Development & Contributing

This document covers: spec-driven workflow, quality gate, testing conventions,
CI pipeline (including WASI SDK setup), hexagonal boundary rules, and the
TDD-based build process used to construct tower.

---

## Spec-driven workflow

All features were built from a backlog of 23 single-responsibility specifications
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

Every commit — and every spec — must pass all five checks in this exact order:

```sh
# 1. Formatting
cargo fmt --all --check

# 2. Linting (warnings are hard errors)
cargo clippy --workspace --all-targets -- -D warnings

# 3. Build wasm fixtures (MUST precede cargo test — see fixture ordering below)
cargo build \
  -p hello \
  -p fixture_abi_mismatch \
  -p fixture_panic_plugin \
  -p fixture_loop_plugin \
  -p fixture_loop_hook_plugin \
  --target wasm32-wasip1

# 4. Build the Tree-sitter AST plugin
cargo build -p ast --target wasm32-wasip1

# 5. Run host-side tests — default-members already scopes this to the host crates,
#    so the wasm-only crates are skipped without an --exclude list.
cargo test

# 6. Run ast host-side tests separately
cargo test -p ast

# 7. License and advisory policy
cargo deny check
```

`cargo-deny` must be installed once: `cargo install cargo-deny --locked`.

**Why the wasm builds must come before `cargo test`**: `crates/core_engine/build.rs`
sets `cargo:rustc-env` variables (e.g. `PANIC_PLUGIN_WASM`, `LOOP_PLUGIN_WASM`)
pointing at the compiled `.wasm` files. Integration tests retrieve these paths via
`env!("...")` at compile time. If the wasm binaries do not exist when `cargo test`
runs, the test binary will compile against stale paths or fail to locate fixtures
at runtime.

**Why you must not spawn `cargo build` from `build.rs`**: doing so would attempt
to acquire the workspace build lock that the outer `cargo build -p core_engine`
already holds, causing a deadlock. The CI workflow runs wasm builds as a separate
step before invoking `cargo test`.

---

## Toolchain and prerequisites

The toolchain is pinned in `rust-toolchain.toml` at the workspace root:

```toml
[toolchain]
channel = "1.96.0"
components = ["rustfmt", "clippy"]
targets = ["wasm32-wasip1"]
profile = "minimal"
```

`rustup` installs this automatically on first use. The `wasm32-wasip1` target is
included; no manual `rustup target add` is needed on a fresh clone.

### WASI SDK

The WASI SDK is required only to compile `ast` (and any future grammar
plugin that vendors C sources). Pure-Rust wasm crates (`hello`, all
fixtures) compile without it.

Two ways to obtain the SDK locally:

**Option A — tree-sitter CLI (auto-installed)**

```sh
# tree-sitter CLI populates ~/.cache/tree-sitter/wasi-sdk on first `build --wasm`
export CC_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang
export AR_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/llvm-ar
```

**Option B — manual download**

```sh
WASI_SDK_DIR=~/.local/wasi-sdk-25
mkdir -p "$WASI_SDK_DIR"
curl -fsSL \
  https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-25/wasi-sdk-25.0-x86_64-linux.tar.gz \
  | tar -xz -C "$WASI_SDK_DIR" --strip-components=1

export CC_wasm32_wasip1="$WASI_SDK_DIR/bin/wasm32-wasip1-clang"
export AR_wasm32_wasip1="$WASI_SDK_DIR/bin/llvm-ar"
```

The variable names follow the `cc` crate convention: `CC_<target>` and
`AR_<target>`, with non-alphanumeric characters replaced by underscores. Minimum
tested version: wasi-sdk-25.0. wasi-sdk-29.0 also works.

---

## CI pipeline

The single `quality` job in `.github/workflows/ci.yml` runs on every push and
pull request on `ubuntu-latest`. Steps in order:

1. `actions/checkout@v4`
2. `actions-rust-lang/setup-rust-toolchain@v1` — reads `rust-toolchain.toml`
3. `Swatinem/rust-cache@v2` — Cargo build cache
4. Cache WASI SDK (key: `wasi-sdk-25.0-x86_64-linux`, path: `$WASI_SDK_DIR`)
5. Download WASI SDK on cache miss (same `curl | tar` command as Option B above)
6. Verify WASI clang with `"$CC_wasm32_wasip1" --version`
7. `cargo fmt --all --check`
8. `cargo clippy --workspace --all-targets -- -D warnings`
9. Build wasm fixtures (all five fixture crates + `hello`, `--target wasm32-wasip1`)
10. Build `ast` for `wasm32-wasip1`
11. `cargo test --workspace --exclude ...` (all wasm crates excluded)
12. `cargo test -p ast` (host-side outline walker tests)
13. `EmbarkStudios/cargo-deny-action@v2`

The CI job env block sets `WASI_SDK_DIR`, `CC_wasm32_wasip1`, and
`AR_wasm32_wasip1` globally so all build steps inherit them automatically.

---

## Hexagonal boundary rules

The architecture enforces a hard boundary between the domain and infrastructure.
Violating it is not a style issue — it breaks testability and couples business
logic to runtime choices.

**The domain imports no infrastructure.** Code under `crates/core_engine/src/domain/`
carries `#![forbid(unsafe_code)]` and must not import `sled`, `std::fs`,
`wasmtime`, `notify`, or any transport. The domain depends only on port traits
defined in `crates/core_engine/src/ports/`.

```
domain/          # Pure business logic. No I/O. #![forbid(unsafe_code)].
  workspace.rs   # ProjectWorkspace — the aggregate root
  file_id.rs     # FileId { index: u32, generation: u32 } — generational
  plugin_host/   # PluginHostRegistry, PluginInstance trait, PluginFaultKind
  grep/          # Text search
  mutation/      # File write operations
  refactor/      # Global replace
  ...

ports/           # Trait definitions only. No concrete types.
  inbound.rs     # SearchUseCase, FileMutationUseCase (driving ports)
  storage.rs     # StoragePort (driven)
  filesystem.rs  # FileSystemPort (driven)
  plugin.rs      # PluginHostPort (driven), NoOpPluginHost

adapters/        # Concrete implementations. Imports sled, wasmtime, notify, std::fs.
  storage/       # SledStorageAdapter
  fs/            # RealFs
  watcher/       # NotifyWatcherAdapter
  mcp/           # JSON-RPC 2.0 stdio transport
  plugin/        # WasmtimeHost, IsolatedSandbox — the only wasmtime-importing code
  in_memory_fs.rs       # Test double for FileSystemPort
  in_memory_storage.rs  # Test double for StoragePort
```

If you reach for an infrastructure crate inside `domain/`, stop — you are
crossing the boundary.

---

## Testing conventions

### Domain unit tests

Domain tests live inside their modules under `#[cfg(test)]` and use only
in-memory test doubles. No real disk, no sled, no wasmtime.

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
| `integration_mcp_native_tools.rs` | JSON-RPC over stdio, all 7 native VFS tools |
| `integration_global_replace.rs` | Parallel mass find-and-replace, error cases |
| `integration_mutation.rs` | Shadow-file atomic writes |
| `plugin_loader.rs` | WasmtimeHost load, ABI guard, capability linker |
| `ast_e2e.rs` | AST outline + symbol search via MCP |
| `plugin_fault_isolation.rs` | Trap, fuel exhaustion, epoch timeout, quarantine |

Fixture `.wasm` paths are injected via `env!("...")` macros, set by
`crates/core_engine/build.rs`. Always build the wasm fixtures before running
these tests.

### Plugin logic tested on the host

`plugins/ast` exposes its outline parsing as a Rust library (`rlib`). Its
host-side unit tests run with `cargo test -p ast` — no wasm sandbox,
no WASI SDK needed. This makes the Tree-sitter logic fast to iterate on.

The wasm build (`cargo build -p ast --target wasm32-wasip1`) is a
separate gate that verifies the same code compiles for the target and produces
a correctly sized module (~1.2 MB release, ~opt-level=s).

### Fault isolation tests

`tests/plugin_fault_isolation.rs` uses deterministic fuel-based interruption
rather than wall-clock timers to avoid flakiness:

```rust
// Prefer a small fuel budget for tests — exhausted in microseconds, no sleep needed
let config = IsolationConfig {
    fuel_budget: Some(1_000),
    epoch_deadline_ticks: None,
};
```

Epoch interruption is also tested by calling `engine.increment_epoch()` directly
rather than relying on the background ticker thread.

### Wasm fixture crates

| Crate | Purpose |
|-------|---------|
| `hello` | Minimal example: `greet` tool + `BeforeToolCall` hook |
| `fixture_abi_mismatch` | Wrong `ABI_VERSION` → `PluginLoadError::AbiMismatch` |
| `fixture_panic_plugin` | Guest `panic!` → `PluginFaultKind::Trapped` |
| `fixture_loop_plugin` | Infinite loop in tool handler → `FuelExhausted` |
| `fixture_loop_hook_plugin` | Infinite loop in hook handler → `FuelExhausted` |

All five are `wasm32-wasip1` only and excluded from `default-members` in the
workspace `Cargo.toml`. Committed static fixtures (`forbidden_import.wasm`,
`forbidden_host_import.wat`) live in `crates/core_engine/tests/fixtures/`.

---

## Non-negotiable invariants

These hold at every commit:

- **Domain purity**: `domain/` is 100% testable with in-memory doubles. No real
  disk, DB, or runtime in domain unit tests.
- **`FileId` is generational**: `struct FileId { index: u32, generation: u32 }`.
  A reused slot increments `generation` so a stale `FileId` can never silently
  resolve to a different file.
- **Atomic file writes**: all content writes use the shadow-file pattern —
  write to `<path>.tmp_write`, flush, then OS-atomic `fs::rename`. No torn
  writes on crash. `.tmp_write` artifacts are never indexed by the VFS.
- **Zero lock contention**: `EngineState` is behind `Arc<RwLock<EngineState>>`;
  critical sections are short and contain no blocking I/O. The fs watcher
  (writer) must not starve MCP handlers (readers).
- **Plugin fault isolation**: any plugin trap, panic, fuel exhaustion, or epoch
  timeout must not crash the host process or sever the MCP link. Enforced by
  `IsolatedSandbox` catching all wasmtime errors and returning
  `PluginHostError::PluginFault`.
- **Capability security**: wasm guests reach the workspace only through
  `tower_host::host_log` and `tower_host::host_read_file`. Any other
  `tower_host` import causes a `LinkError` at instantiation.
- **ABI version guard**: plugins where `manifest.abi != ABI_VERSION` (currently
  `2`) are rejected at both load time and registration.
- **Unique plugin names**: duplicate `manifest.name` at registration returns
  `RegistrationError::DuplicateName`. The second plugin is never stored.
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

This cycle produced 423 passing host tests and 65 passing `ast` host-side
tests across 23 specs, with no spec leaving the tree in a broken state.

The dependency order of the 23 specs is preserved in `.agent/todo/README.md`.
The hexagonal architecture ensured that specs could be developed in isolation:
domain specs never touched adapters, and adapter specs never touched domain logic
beyond port traits.

---

## Related docs

- [docs/ADR/](ADR/) — architectural decision records
- [project-brief.md](../project-brief.md) — the original product intent
- [AGENTS.md](../AGENTS.md) — crate layout, port/adapter names, invariants (the authoritative quick-reference for contributors)
- [.github/workflows/ci.yml](../.github/workflows/ci.yml) — canonical CI definition
