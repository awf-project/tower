# Architecture

Tower is a Rust binary (`tower`) exposing a virtual file system, text processing, mass-refactoring,
and AST analysis over a JSON-RPC 2.0 stdio interface (MCP). Its internal structure follows
**Domain-Driven Design + Hexagonal (Ports and Adapters) + Microkernel**, where the extension host
is the kernel extension point.

> **Extensibility model.** Tower extends the headless engine through **out-of-process native
> extensions** (sidecars): standalone binaries the host spawns as child processes and drives over a
> JSON-RPC 2.0 protocol on stdio. Isolation is the **OS process boundary**. (Previously this was an
> embedded `wasmtime` WASM sandbox; it was replaced in spec 20. See [extensions.md](extensions.md).)
> The engine itself is a single static binary — no WASM, no WASI SDK, no JVM, no Node.

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
│   MCP / Native           Extension              Storage     FS     Watcher  │
│   ─────────────────      ───────────────────    ───────     ────   ──────── │
│   NativeToolRegistry     SidecarHostAdapter     Sled        RealFs Notify   │
│   ExtensionMergedRegistry  ExtensionSupervisor  Adapter            Adapter  │
│   serve() transport      discovery / manifest                               │
└──────┬─────────────────────────┬───────────────────┬────────────────────────┘
       │ SearchUseCase           │ ExtensionHostPort  │ StoragePort
       │ FileMutationUseCase     │ FormatQueuePort    │ FileSystemPort
       │                         │ AstIndexPort       │
┌──────▼─────────────────────────▼───────────────────▼────────────────────────┐
│  PORTS (traits)                                                             │
│                                                                             │
│   Inbound (driving)          Outbound (driven)                              │
│   ─────────────────          ─────────────────                              │
│   SearchUseCase              StoragePort      (get/put/put_batch/delete/     │
│   FileMutationUseCase         blobs/scan-complete marker)                   │
│                              FileSystemPort   (read/write/rename/delete/    │
│                               mkdir/scan)                                   │
│                              AstIndexPort     (index cache get/put)         │
│                              FormatQueuePort  (enqueue format request)      │
│                              ExtensionHostPort (on_file_indexed/changed/    │
│                               deleted, declared_tools, invoke)              │
└──────────────────────────────┬──────────────────────────────────────────────┘
                               │ no I/O imports — only port traits
┌──────────────────────────────▼──────────────────────────────────────────────┐
│  DOMAIN (domain/)   #![forbid(unsafe_code)]                                 │
│                                                                             │
│   ProjectWorkspace   FileId (generational)   VirtualFile   RelativePath     │
│   InvertedIndex      SearchService           FileMutationService            │
│   GlobalReplaceService   ExtensionRegistry   ExtensionInstance (trait)      │
└─────────────────────────────────────────────────────────────────────────────┘
```

## The golden rule

The `domain/` module contains `#![forbid(unsafe_code)]` and imports **zero** infrastructure
crates. There is no `sled`, `std::process`, `notify`, or `std::fs` anywhere under `domain/`.
Domain services receive port trait objects through their constructors and talk exclusively
through those interfaces. Violation = hexagonal boundary crossed.

The extension host is part of this: `domain/extension_host/` (the `ExtensionRegistry`) holds the
routing, event-fan-out, and quarantine **policy** and imports only the pure `extension_protocol`
types — never `std::process`. The process spawn/stdio/kill lives in `adapters/extension/`.

```
// Enforced in domain/mod.rs:
//! Invariant: no `sled`, `fs`, `std::process`, or `notify` imports here. Everything
//! in this module is constructible and assertable without any I/O.
```

## Crate layout

```
crates/
├── core_engine/           Host binary (tower) + lib
│   └── src/
│       ├── domain/        Pure business logic; no I/O
│       │   └── extension_host/  ExtensionRegistry, ExtensionInstance trait (policy only)
│       ├── ports/         Trait contracts (inbound + outbound)
│       └── adapters/      Concrete infrastructure wiring
│           ├── fs/        RealFs (std::fs), workspace_scan (ignore crate)
│           ├── storage/   SledStorageAdapter (sled 0.34)
│           ├── watcher/   NotifyWatcherAdapter (notify 6.1)
│           ├── mcp/       JSON-RPC transport, NativeToolRegistry, ExtensionMergedRegistry
│           ├── config/    .tower/config.toml parser
│           └── extension/ SidecarHostAdapter, ExtensionSupervisor, discovery,
│                          host_deps, path_validation (the only std::process code)
│
└── extension_protocol/    Shared wire contract (types + serde only; no I/O)
                           Request/Response/Event/HostCall, ExtensionManifest,
                           Capability, ToolDecl, PROTOCOL_VERSION = 1

extensions/                Native sidecar extensions (separate binaries)
├── ast/                   Reference Tree-sitter AST extension (eager)
│                          Tools: get_outline, find_symbols, search_symbols,
│                                 reindex, read_symbol
│                          Languages: Rust (.rs), Go (.go), PHP (.php)
├── hello/                 Minimal example extension (lazy; greet tool)
├── lsp/                   Language-server bridge extension (lazy)
│                          Tools: diagnostics, definition, references, hover
└── fixtures/              Test-only fault-isolation fixtures
```

The reference extensions are ordinary native binaries. `cargo build --workspace --bins`
builds them under `target/debug/` before tests that spawn them. There is no WASM build
step and no WASI SDK.

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

**`AstIndexPort`** (`ports/ast_index.rs`): a small get/put cache for serialized AST artifacts, keyed
by string (e.g. `ast/<relative-path>`). Backs the `index/get` and `index/put` capability callbacks.

**`FormatQueuePort`** (`adapters/formatter`): enqueues a workspace file for formatting. Backs the
`workspace/requestFormat` capability callback.

**`ExtensionHostPort`** (`ports/extension_host.rs`): the bridge to the extension subsystem —
`on_file_indexed`, `on_file_changed`, `on_file_deleted` (event fan-out), `declared_tools` (the tools
each extension contributes, consumed by the MCP merge), and `invoke` (route a tool call to its owning
extension). The signatures take `&self` (object-safe); interior mutability bridges to the `&mut self`
that each `ExtensionInstance` requires for its subprocess I/O. A no-op default implementation satisfies
the trait for configurations with no extensions.

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
  │    "tools/list"  → ExtensionMergedRegistry::list()
  │    "tools/call"  → ExtensionMergedRegistry::call(name, args)
  │  notification (no id) → silently dropped, no response
  │  malformed frame      → -32700 ParseError, loop continues
  │
  ├─ native tool path (tower_* names):
  │    NativeToolRegistry::call(name, args)
  │      acquires RwLock::write (mutations) or RwLock::read (reads)
  │      delegates to SearchUseCase / FileMutationUseCase
  │        FileMutationService: write → <path>.tmp_write → flush → fs::rename
  │        GlobalReplaceService: parallel (Rayon) per-file rewrite → TxReport
  │        SearchService: inverted index lookup (find_file) or parallel grep (search_text)
  │      StoragePort::put / put_batch → sled
  │      ExtensionHostPort::on_file_indexed / on_file_changed → extension event fan-out
  │
  └─ extension tool path ("tower_<ext>_<tool_name>" names):
       ExtensionMergedRegistry::call → ExtensionHostPort::invoke(tool_name, args)
         ExtensionRegistry routes by tool name → owning instance
           Mutex<Box<dyn ExtensionInstance>>::lock [per-instance, not global]
             SidecarHostAdapter::call_tool
               JSON-RPC invokeTool over stdio to the child process
               per-call timeout; capability callbacks routed to outbound ports
               ExtensionFault (Timeout/Crashed/ProtocolError/Quarantined)
                 → InvokeError → -32603; host process and MCP link survive any fault
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
                                                  calls ExtensionHostPort::on_file_changed
```

## Extension host runtime (Microkernel)

Extensions are native sidecar binaries discovered at startup and spawned as child processes. Their
tools appear in `tools/list` under `tower_<ext>_<tool_name>`. The host ↔ extension wire contract is
JSON-RPC 2.0 over stdio (the `extension_protocol` crate). For the full author-facing description see
[extensions.md](extensions.md).

```
Protocol crate (crates/extension_protocol/)   [pure types, no I/O]
  Request  { Initialize, InvokeTool, DeliverEvent, Shutdown }
  Response { Initialized(tools/events/capabilities), ToolResult, Ack, Error }
  Event    { FileIndexed, FileChanged, FileDeleted }
  HostCall { ReadFile, ListFiles, IndexGet, IndexPut, RequestFormat, Log, NotifyResourceUpdated }
  ExtensionManifest (extension.toml), Capability, ToolDecl, PROTOCOL_VERSION = 1

Discovery (adapters/extension/discovery.rs)
  search path (resolve_extension_dirs):
    --extensions-dir / $TOWER_EXTENSIONS_DIR → single dir (replaces path)
    else: [global XDG <data>/tower/extensions, local <ws>/.tower/extensions]  (local wins)
  read <dir>/<name>/extension.toml manifests
  validate: activation=lazy + event subscriptions → rejected (no event replay)
  disabled (config [extensions] disabled) → never spawned
  activate:
    eager → SidecarHostAdapter::spawn now (required for event subscribers)
    lazy  → ExtensionSupervisor created; child spawned on first call

Sidecar adapter (adapters/extension/ — SidecarHostAdapter : ExtensionInstance)
  spawn child process; send Initialize, read Initialized (tools/events/capabilities)
  call_tool(name, params)  → JSON-RPC invokeTool over stdio, bounded by request_timeout
  deliver_event(Event)     → JSON-RPC deliverEvent notification
  capability callbacks (HostCall) routed to existing outbound ports — no privileged path:
    ReadFile / ListFiles    → FileSystemPort
    IndexGet / IndexPut     → AstIndexPort
    RequestFormat           → FormatQueuePort
    NotifyResourceUpdated   → MCP push channel
    Log                     → host logging
  capability gating: a HostCall not declared in the manifest → rejected (ProtocolError)
  path validation: empty / absolute / `..`-traversal paths → rejected

Supervisor (adapters/extension/supervisor.rs — ExtensionSupervisor)
  lazy respawn with exponential backoff: min(2^n · 100 ms, 30 s)
  Timeout / Crashed → drop instance, enter backoff; respawn on next call after delay
  success → clear backoff

Registry (domain/extension_host/ — ExtensionRegistry : ExtensionHostPort)  [policy only]
  stores: per-extension Mutex<Box<dyn ExtensionInstance>>
  on_file_indexed / on_file_changed / on_file_deleted:
    fan-out only to instances subscribed to that event kind
    per-extension error isolation: one bad extension blocks no others
  invoke(tool, params): route by tool name → owning instance; map fault → caller error
  declared_tools() → Vec<(ExtensionId, ToolDecl)>
  quarantine: per-ext consecutive-fault counter; ≥ MAX_CONSECUTIVE_FAILURES (=3)
    ⇒ Quarantined: stop routing/delivery, return Quarantined to callers (a success resets)

MCP tool merging (adapters/mcp/extension_merged_registry.rs — ExtensionMergedRegistry)
  list()  = NativeToolRegistry::list() ++ extension tools namespaced as "tower_<ext>_<name>"
  call(name):
    native name ("tower_find_file", …)? → NativeToolRegistry::call(name, args)
    else "tower_<ext>_<tool>"           → ExtensionHostPort::invoke(name, args)
  extension names beginning with "tower" are reserved for host tools; no collision possible.
```

Event kinds: `event/fileIndexed`, `event/fileChanged`, `event/fileDeleted`. Unsubscribed extensions
incur zero overhead. A delivery error from one extension does not block others. Isolation is the OS
process boundary — a crash, hang (timeout), or protocol violation in a child never crashes the host or
severs the MCP link.

## Non-negotiable invariants

| Invariant | Where enforced |
|-----------|----------------|
| **Domain purity**: `domain/` imports no sled, std::process, notify, or std::fs | `#![forbid(unsafe_code)]` + module-level doc comment; compile-time |
| **Generational FileId**: `struct FileId { index: u32, generation: u32 }`. A reused slot bumps generation; stale id never silently resolves to a different file | `domain/file_id.rs`; only `ProjectWorkspace` mints ids |
| **Atomic file writes**: write to `<path>.tmp_write` → flush → `fs::rename`. No torn files on crash. `.tmp_write` files are never indexed | `FileMutationService`, `GlobalReplaceService`; `FileSystemPort::rename` contract |
| **Zero lock contention**: `EngineState` behind `Arc<RwLock<EngineState>>`; critical sections are short and contain no blocking I/O; watcher (writer) must not starve MCP handlers (readers) | `main.rs` wiring; per-extension `Mutex` so one slow extension stalls only itself |
| **Extension fault isolation**: a child crash, hang (timeout), or protocol violation must not crash the host or sever the MCP link | OS process boundary + `SidecarHostAdapter` (per-call timeout/kill) → `ExtensionFault` → `-32603` |
| **Capability security**: extensions reach the workspace only via declared capability callbacks routed to outbound ports; an undeclared `HostCall` is rejected | `SidecarHostAdapter` capability gating + `path_validation` (rejects `..`/absolute/empty paths) |
| **Protocol version guard**: the host compares the extension's `protocol_version` against `PROTOCOL_VERSION` (currently 1) at `initialize` | `extension_protocol::PROTOCOL_VERSION`; sidecar `initialize` handshake |
| **Quarantine**: an extension is quarantined after `MAX_CONSECUTIVE_FAILURES` (=3) consecutive faults; a success resets the counter | `ExtensionRegistry` (domain policy) |
| **Extension tool namespacing**: tools are always merged as `tower_<ext>_<tool_name>`; no un-namespaced extension tool is possible | `ExtensionMergedRegistry::list` |
| **Reserved host prefix**: extension names beginning with `tower` are reserved for native host tools; no collision possible | `ExtensionMergedRegistry` |
| **Single static binary**: `cargo build -p core_engine` produces a self-contained `tower` binary; no WASM, WASI SDK, JVM, Node, or container required | Cargo workspace; no runtime dynamic linking |

## JSON-RPC error codes

| Code | Constant | When |
|------|----------|------|
| -32700 | ParseError | Malformed JSON or invalid UTF-8 frame |
| -32600 | InvalidRequest | `jsonrpc` field is not `"2.0"` |
| -32601 | MethodNotFound | Unknown RPC method name |
| -32602 | InvalidParams | Missing required field in tool arguments |
| -32603 | InternalError | Tool execution failed (including extension faults) |
| -32001 | ToolNotFound | Named tool not in registry |
| -32002 | ResourceNotFound | Domain entity not found |

Success shape:

```json
{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"<json-string>"}]},"id":1}
```

## Key design decisions

**`Arc<RwLock<EngineState>>`** — wiring choice at startup. `Arc` provides shared ownership
between the MCP handler and the future watcher thread without copying. `RwLock` allows
concurrent `tower_find_file` / `tower_search_text` readers while serialising mutating calls
(`create_file`, `delete_file`, `global_replace`). Critical sections hold no blocking I/O.

**Out-of-process sidecars over in-process WASM** — extensions are separate native binaries, not
embedded WASM modules. The earlier `wasmtime` model conflated a *security sandbox* with an
*extensibility unit*; tower wants the latter, and rich extensions need subprocesses, sockets,
long-lived state, and threads that WASI deliberately removes. The OS process boundary provides
isolation; native code regains full performance (notably for the Tree-sitter `ast` extension, which
no longer needs a WASI sysroot) and extensions can be authored in any language.

**JSON-RPC 2.0 over stdio, not MCP-as-protocol** — the host ↔ extension protocol is tower-specific
because the engine needs native event subscription and host capability callbacks that MCP's
client→server shape does not give cleanly. The *external* MCP contract is unchanged.

**`Mutex` per extension instance, not one global lock** — `ExtensionRegistry` wraps each instance in
`Mutex<Box<dyn ExtensionInstance>>`. `ExtensionHostPort` methods take `&self` for object safety, while
`ExtensionInstance` methods take `&mut self` for exclusive subprocess I/O. A `Mutex` per instance
bridges these and means a slow or blocked extension only stalls itself, never the others.

**Quarantine policy in the domain, process control in the adapter** — the consecutive-fault counter
and quarantine decision live in the runtime-agnostic `ExtensionRegistry` (domain), so they are
unit-testable with no I/O. The actual spawn/timeout/kill/respawn lives in `SidecarHostAdapter` +
`ExtensionSupervisor` (adapter). This keeps the golden rule intact.

## Contract testing

Every real adapter shares the same behavioral contract test suite as its in-memory fake
(defined in `adapters/contract_tests.rs`). This ensures that swapping `InMemoryStorage`
for `SledStorageAdapter` in production does not change observable semantics. Domain unit
tests use only in-memory doubles — zero disk I/O.

## Related pages

- [getting-started.md](getting-started.md) — prerequisites, build commands, first run
- [mcp-tools.md](mcp-tools.md) — the native `tower_*` tools and the extension tools
- [extensions.md](extensions.md) — authoring an extension, the protocol, capabilities, fault model
- [development.md](development.md) — quality gate, CI, test strategy, contributing
