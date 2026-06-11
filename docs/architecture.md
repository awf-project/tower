# Architecture

Tower is a Rust binary (`tower`) exposing a virtual file system, text processing, mass-refactoring,
and AST analysis over a JSON-RPC 2.0 stdio interface (MCP). Its internal structure follows
**Domain-Driven Design + Hexagonal (Ports and Adapters) + Microkernel**, where the plugin runtime
is the kernel extension point.

## Layer overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  INBOUND DRIVERS  (stdin / future: CLI, LSP)                                │
│                                                                             │
│   MCP transport (JSON-RPC 2.0, newline-delimited, no Content-Length)        │
│   serve(reader, writer, &mut dyn ToolRegistry)                              │
└──────────────────────────────┬──────────────────────────────────────────────┘
                               │  calls ToolRegistry::call / list
┌──────────────────────────────▼──────────────────────────────────────────────┐
│  ADAPTERS LAYER  (adapters/)                                                │
│                                                                             │
│   MCP / Native           Plugin                  Storage     FS     Watcher │
│   ─────────────────      ───────────────────     ───────     ────   ─────── │
│   NativeToolRegistry     WasmtimeHost (loader)   Sled        RealFs Notify  │
│   MergedRegistry         IsolatedSandbox (11d)   Adapter            Adapter │
│   serve() transport      IsolationEngine                                    │
└──────────┬───────────────────────┬───────────────────┬──────────────────────┘
           │ SearchUseCase         │ PluginHostPort     │ StoragePort
           │ FileMutationUseCase   │                    │ FileSystemPort
┌──────────▼───────────────────────▼───────────────────▼──────────────────────┐
│  PORTS (traits)                                                             │
│                                                                             │
│   Inbound (driving)          Outbound (driven)                              │
│   ─────────────────          ─────────────────                              │
│   SearchUseCase              StoragePort      (get/put/put_batch/delete/     │
│   FileMutationUseCase         blobs/scan-complete marker)                   │
│                              FileSystemPort   (read/write/rename/delete/    │
│                               mkdir/scan)                                   │
│                              PluginHostPort   (on_file_indexed/             │
│                               on_file_changed)                              │
└──────────────────────────────┬──────────────────────────────────────────────┘
                               │ no I/O imports — only port traits
┌──────────────────────────────▼──────────────────────────────────────────────┐
│  DOMAIN (domain/)   #![forbid(unsafe_code)]                                 │
│                                                                             │
│   ProjectWorkspace   FileId (generational)   VirtualFile   RelativePath     │
│   InvertedIndex      SearchService           FileMutationService            │
│   GlobalReplaceService   PluginHostRegistry  PluginInstance (trait)         │
└─────────────────────────────────────────────────────────────────────────────┘
```

## The golden rule

The `domain/` module contains `#![forbid(unsafe_code)]` and imports **zero** infrastructure
crates. There is no `sled`, `wasmtime`, `notify`, or `std::fs` anywhere under `domain/`.
Domain services receive port trait objects through their constructors and talk exclusively
through those interfaces. Violation = hexagonal boundary crossed.

```
// Enforced in domain/mod.rs:
//! Invariant: no `sled`, `fs`, `wasmtime`, or `notify` imports here. Everything
//! in this module is constructible and assertable without any I/O (spec U3/AC4).
```

## Crate layout

```
crates/
├── core_engine/           Host binary (tower) + lib
│   └── src/
│       ├── domain/        Pure business logic; no I/O
│       ├── ports/         Trait contracts (inbound + outbound)
│       └── adapters/      Concrete infrastructure wiring
│           ├── fs/        RealFs (std::fs), workspace_scan (ignore crate)
│           ├── storage/   SledStorageAdapter (sled 0.34)
│           ├── watcher/   NotifyWatcherAdapter (notify 6.1)
│           ├── mcp/       JSON-RPC transport, NativeToolRegistry, MergedRegistry
│           └── plugin/    WasmtimeHost, IsolatedSandbox, IsolationEngine
│
├── plugin_sdk/            Distributable SDK for plugin authors
│   │                      (PluginManifest, ToolDesc, HookKind, HookPayload,
│   │                       Value, SdkError, CallRequest, CallResponse, ABI_VERSION=2)
│   └── plugin_sdk_macros/ proc-macro crate: #[plugin_main], #[plugin_export]
│
├── plugin_ast/            Reference AST plugin → wasm32-wasip1 (~1.2 MB release)
│                          Tools: ast_get_outline, ast_find_symbols
│                          Languages: Rust (.rs), Go (.go), PHP (.php)
│
├── hello_plugin/          Minimal example plugin (cdylib)
├── fixture_abi_mismatch/  Test fixture: wrong ABI version
├── fixture_panic_plugin/  Test fixture: panicking guest
├── fixture_loop_plugin/   Test fixture: infinite-loop guest (fuel test)
└── fixture_loop_hook_plugin/ Test fixture: infinite-loop in hook handler
```

`default-members` excludes the wasm crates so `cargo build` on the host does not
attempt cross-compilation automatically.

## Ports in detail

### Inbound ports (driving — callers inject the domain)

Defined in `crates/core_engine/src/ports/inbound.rs`.

```rust
pub trait SearchUseCase {
    fn find_file(&self, query: &str) -> Result<Vec<RelativePath>, DomainError>;
    fn search_text(&self, pattern: &str) -> Result<Vec<Match>, DomainError>;
    fn search_text_capped(&self, pattern: &str, cap: usize) -> Result<Vec<Match>, DomainError>;
}

pub trait FileMutationUseCase {
    fn create_file(&mut self, path: RelativePath, content: Vec<u8>) -> Result<(), DomainError>;
    fn create_directory(&mut self, path: RelativePath) -> Result<(), DomainError>;
    fn delete_file(&mut self, path: &RelativePath) -> Result<(), DomainError>;
    fn global_replace(&mut self, target: &str, replacement: &str) -> Result<TxReport, DomainError>;
    fn global_replace_dry_run(&mut self, target: &str, replacement: &str) -> Result<TxReport, DomainError>;
}
```

Both traits are object-safe by design so adapters can hold `&dyn SearchUseCase` without
monomorphisation.

### Outbound ports (driven — domain requires these)

**`StoragePort`** (`ports/storage.rs`): persistence for `VirtualFile` entities and raw content
blobs. Key contract guarantees: `put_batch` is all-or-nothing; `mark_scan_complete` / `is_scan_complete`
survive process restart on sled. Implemented by `SledStorageAdapter` (production) and
`InMemoryStorage` (tests).

**`FileSystemPort`** (`ports/filesystem.rs`): raw byte I/O — `read`, `write`, `rename`, `delete`,
`mkdir`, `scan`. `rename` must be atomic with respect to concurrent readers (shadow-file invariant).
Implemented by `RealFs` (production) and `InMemoryFs` (tests).

**`PluginHostPort`** (`ports/plugin.rs`): lifecycle hooks — `on_file_indexed` and `on_file_changed`.
The signature takes `&self` (object-safe); interior mutability bridges to the `&mut self` that
wasm instances require. `NoOpPluginHost` satisfies the trait with empty bodies for configurations
without plugins.

Every real adapter passes the same contract test suite as its in-memory fake, enforcing behavioral
equivalence at the port boundary.

## Data flow

```
Startup
───────
workspace root
  └─ SledStorageAdapter::open(.tower/db)
       ├─ reconstructs ProjectWorkspace + InvertedIndex from sled
       └─ if not scan_complete:
            workspace_scan(root, &mut storage, &mut workspace, &mut index)
              uses ignore crate (respects .gitignore)
              put_batch → mark_scan_complete

Runtime (Arc<RwLock<EngineState>> shared by all handlers)
──────────────────────────────────────────────────────────

stdin (newline-delimited JSON-RPC 2.0)
  │
  ▼ serve() [transport.rs]
  │  parse JSON → JsonRpcRequest
  │  dispatch on method:
  │    "initialize"  → { protocolVersion, serverInfo.name="tower", capabilities }
  │    "tools/list"  → MergedRegistry::list()
  │    "tools/call"  → MergedRegistry::call(name, args)
  │  notification (no id) → silently dropped, no response
  │  malformed frame      → -32700 ParseError, loop continues
  │
  ├─ native tool path (vfs_* names):
  │    NativeToolRegistry::call(name, args)
  │      acquires RwLock::write (mutations) or RwLock::read (reads)
  │      delegates to SearchUseCase / FileMutationUseCase
  │        FileMutationService: write → <path>.tmp_write → flush → fs::rename
  │        GlobalReplaceService: parallel (Rayon) per-file rewrite → TxReport
  │        SearchService: inverted index lookup (find_file) or parallel grep (search_text)
  │      StoragePort::put / put_batch → sled
  │      PluginHostPort::on_file_indexed / on_file_changed → plugin fan-out
  │
  └─ plugin tool path ("<plugin_id>/<tool_name>" names):
       MergedRegistry::call → host.read() [RwLock read guard only]
         PluginHostRegistry::call_instance_tool(plugin_id, tool_name, args)
           Mutex<Box<dyn PluginInstance>>::lock [per-slot, not global]
             IsolatedSandbox::call_tool
               apply_compute_bounds (fuel + epoch)
               WasmInstance::call_tool (wasmtime TypedFunc::call)
               PluginHostError::PluginFault → ToolError::ExecutionFailed → -32603
               host process and MCP link survive any fault
  │
stdout (newline-delimited JSON-RPC 2.0)

Watcher (background, spec 06)
──────────────────────────────
OS inotify/kqueue/FSEvents
  └─ notify::recommended_watcher
       └─ debounce_events (coalesce bursts)
            └─ Sender<WatchEvent> ──channel──► worker thread
                                                EventProcessor::process_event
                                                  acquires RwLock::write briefly
                                                  re-indexes changed file
                                                  calls PluginHostPort::on_file_changed
```

## Plugin runtime (Microkernel)

The plugin system is a "drop and play" microkernel: place a `.wasm` file in the plugin directory
and it is loaded, sandboxed, and its tools appear in `tools/list` under `<plugin_name>/<tool_name>`.

```
Plugin SDK (crates/plugin_sdk/)
  Plugin trait + PluginManifest (name, version, abi, tools[], hooks[])
  ABI_VERSION = 2 (u32)
  Wire format: postcard binary, 4-byte LE u32 length header
  4 required wasm exports (generated by #[plugin_main]):
    __plugin_init()       → *mut u8  (postcard PluginManifest)
    __plugin_call_tool()  → *mut u8  (postcard CallResponse)
    __plugin_on_hook()              (no return)
    __plugin_free()                 (host frees guest heap buffers)

Loader (adapters/plugin/loader.rs — WasmtimeHost)
  1. Engine::new(config)           [consume_fuel + epoch_interruption]
  2. Module::from_file(engine, path)
  3. Store<WasmStoreData>
     WasiCtxBuilder::new()         [zero capability WASI]
       no preopened dirs → path_open returns ENOENT
       no env vars, no network, no stdio
       clocks + RNG remain (required by Rust std)
  4. Linker::new(engine)
     + p1::add_to_linker_sync      [links WASI symbols]
     + func_wrap("tower_host","host_log",...)       [max 4096 bytes to host stderr]
     + func_wrap("tower_host","host_read_file",...) [delegates to FileSystemPort]
     any other tower_host import → LinkError (instantiation rejected)
  5. linker.instantiate → call __plugin_init → verify manifest.abi == ABI_VERSION
     ABI mismatch → PluginLoadError::AbiMismatch

Registry (domain/plugin_host/ — PluginHostRegistry)
  stores: Vec<Mutex<Box<dyn PluginInstance>>>
  register(instance):
    rejects manifest.abi != ABI_VERSION  → RegistrationError::AbiMismatch
    rejects duplicate manifest.name      → RegistrationError::DuplicateName
  on_file_indexed / on_file_changed:
    fan-out to subscribed instances only (per manifest.hooks)
    per-plugin errors logged to stderr, fan-out continues (isolation)
  declared_tools() → Vec<(PluginId, ToolDesc)>
  call_instance_tool(plugin_id, tool_name, args) → Result<Value, PluginHostError>

Fault Isolation (adapters/plugin/isolation.rs — IsolatedSandbox)
  SandboxState: Ready(WasmInstance) | Failed{consecutive_failures} | Quarantined
  MAX_CONSECUTIVE_FAILURES = 3
  DEFAULT_FUEL_BUDGET      = 100_000_000 fuel units per call
  IsolationEngine: background thread "tower-epoch-ticker" every 10 ms
  guarded_call():
    Quarantined → PluginFault::Quarantined (no further attempts)
    Failed      → try_recreate() [lazy, on next call]
                  if failures >= MAX → Quarantined
    Ready       → apply_compute_bounds(fuel + epoch)
                  wasmtime trap / fuel-exhausted / epoch-exceeded
                  → state = Failed{0}; return PluginFault::*
  All PluginFault variants map to ToolError::ExecutionFailed → -32603
  MCP link is never severed by a plugin fault

MCP tool merging (adapters/mcp/merged_registry.rs — MergedRegistry)
  list()  = NativeToolRegistry::list() ++ plugin tools namespaced as "<id>/<name>"
  call(name):
    contains '/'? → split → host.read() [RwLock read, not write] → call_instance_tool
    else          → NativeToolRegistry::call(name, args)
  Plugin tools cannot claim un-namespaced names; no collision possible.
```

Hook kinds (ABI v2): `BeforeToolCall`, `AfterToolCall`, `FileIndexed`, `FileChanged`.
Unsubscribed plugins incur zero overhead. A delivery error from one plugin does not
block others.

## Non-negotiable invariants

| Invariant | Where enforced |
|-----------|----------------|
| **Domain purity**: `domain/` imports no sled, wasmtime, notify, or std::fs | `#![forbid(unsafe_code)]` + module-level doc comment; compile-time |
| **Generational FileId**: `struct FileId { index: u32, generation: u32 }`. A reused slot bumps generation; stale id never silently resolves to a different file | `domain/file_id.rs`; only `ProjectWorkspace` mints ids |
| **Atomic file writes**: write to `<path>.tmp_write` → flush → `fs::rename`. No torn files on crash. `.tmp_write` files are never indexed | `FileMutationService`, `GlobalReplaceService`; `FileSystemPort::rename` contract |
| **Zero lock contention**: `EngineState` behind `Arc<RwLock<EngineState>>`; critical sections are short and contain no blocking I/O; watcher (writer) must not starve MCP handlers (readers) | `main.rs` wiring; `MergedRegistry::call` uses `host.read()` not `host.write()` |
| **Plugin fault isolation**: trap / panic / fuel-exhaustion / epoch-timeout must not crash the host or sever the MCP link | `IsolatedSandbox::guarded_call` catches all wasmtime errors; maps to `ToolError::ExecutionFailed` |
| **Capability security**: WASM guests reach the workspace only via `tower_host::host_log` and `tower_host::host_read_file`; any other `tower_host` import causes `LinkError` at instantiation | `WasmtimeHost` linker setup; `WasiCtxBuilder::new()` zero-capability |
| **ABI version guard**: plugins where `manifest.abi != ABI_VERSION` (currently 2) are rejected | `PluginHostRegistry::register` and `WasmtimeHost::load` |
| **Unique plugin names**: duplicate `manifest.name` returns `RegistrationError::DuplicateName` | `PluginHostRegistry::register` |
| **Plugin tool namespacing**: `MergedRegistry` unconditionally uses `<plugin_name>/<tool_name>`; no code path allows an un-namespaced plugin tool | `MergedRegistry::list` |
| **Single static binary**: `cargo build -p core_engine` produces a self-contained `tower` binary; no JVM, Node, or container required | Cargo workspace; no runtime dynamic linking |

## JSON-RPC error codes

| Code | Constant | When |
|------|----------|------|
| -32700 | ParseError | Malformed JSON or invalid UTF-8 frame |
| -32600 | InvalidRequest | `jsonrpc` field is not `"2.0"` |
| -32601 | MethodNotFound | Unknown RPC method name |
| -32602 | InvalidParams | Missing required field in tool arguments |
| -32603 | InternalError | Tool execution failed (including plugin faults) |
| -32001 | ToolNotFound | Named tool not in registry |
| -32002 | ResourceNotFound | Domain entity not found |

Success shape:

```json
{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"<json-string>"}]},"id":1}
```

## Key design decisions

**`Arc<RwLock<EngineState>>`** — wiring choice at startup. `Arc` provides shared ownership
between the MCP handler and the future watcher thread without copying. `RwLock` allows
concurrent `vfs_find_file` / `vfs_search_text` readers while serialising mutating calls
(`create_file`, `delete_file`, `global_replace`). Critical sections hold no blocking I/O.

**`host.read()` not `host.write()` in `MergedRegistry::call`** — plugin tool dispatch acquires
only a read guard on the `Arc<RwLock<PluginHostRegistry>>`. Per-instance exclusivity is
delegated to the `Mutex<Box<dyn PluginInstance>>` inside the registry. Using `write()` would
block `tools/list` for the entire duration of a plugin invocation, violating the zero-lock-
contention mandate.

**Lazy sandbox recreate** — `IsolatedSandbox` recreates the wasm instance on the next call
after a failure, not asynchronously. The recreate is fast (~1–2 ms) and callers already
handle `Err(PluginFault)` on the failing call. A background thread would add cross-thread
state sharing complexity disproportionate to the benefit.

**postcard over JSON for plugin ABI** — the `plugin_sdk` wire format is postcard (binary,
alloc-compatible), not JSON. This is important for `no_std`-compatible plugins and keeps
the binary wire size small. JSON appears only at the MCP adapter boundary when converting
`plugin_sdk::Value` to `serde_json::Value`.

**`Mutex` per plugin slot, not one global lock** — `PluginHostRegistry` stores
`Vec<Mutex<Box<dyn PluginInstance>>>`. `PluginHostPort::on_file_indexed` takes `&self` for
object safety, while `PluginInstance::deliver_hook` requires `&mut self` for exclusive wasm
instance access. A `Mutex` per slot bridges these. A single-threaded `RefCell` would not
satisfy `Sync`, which is required because `EventProcessor` stores `Box<dyn PluginHostPort + Send + Sync>`.

## Contract testing

Every real adapter shares the same behavioral contract test suite as its in-memory fake
(defined in `adapters/contract_tests.rs`). This ensures that swapping `InMemoryStorage`
for `SledStorageAdapter` in production does not change observable semantics. Domain unit
tests use only in-memory doubles — zero disk I/O.

## Related pages

- [getting-started.md](getting-started.md) — prerequisites, build commands, first run
- [mcp-tools.md](mcp-tools.md) — the 7 native VFS tools and 2 AST plugin tools
- [plugins.md](plugins.md) — writing a plugin, the ABI, SDK, and fault isolation
- [development.md](development.md) — quality gate, CI, test strategy, contributing
