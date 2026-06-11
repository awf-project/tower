# PROJECT BRIEF: Core Engine (VFS, Text Processing, Extensible WASM Plugin Architecture & MCP)

## 1. Vision & Objectives

The goal of this project is to develop the central infrastructure component (**Core Engine**) for a modern, high-performance productivity tool or code editor written in **Rust**.

This core delivers a minimal memory footprint, absolute resilience against file corruption during mass refactoring, and a standardized **MCP (Model Context Protocol)** interface. It features a **"Drop & Play" distributed extension architecture (Plugin SDK)** driven by an embedded **WebAssembly (WASM)** runtime. This allows third-party contributors to securely inject advanced semantic capabilities—such as multi-language parsing via **Tree-sitter**—and custom MCP tools without requiring global recompilation or complex local setups.

---

## 2. Architectural Manifesto: DDD, Clean, Hexagonal & Microkernel

To ensure long-term maintainability and bulletproof isolation, the Core Engine decouples its business logic from execution runtimes and third-party code by merging **Domain-Driven Design (DDD)**, **Hexagonal Architecture (Ports & Adapters)**, and the **Microkernel Pattern**:

### A. The Core Domain (Microkernel)

* **Absolute Independence:** The core domain encapsulates the pure business logic of the Virtual File System (VFS), inverted search indexes, and atomic text-processing operations. It has zero knowledge of the storage engine (`Sled`), the transport protocols (`MCP`), or the WebAssembly runtime.
* **Strongly-Typed Model:** Implementation of an *Ubiquitous Language* using explicit Rust types: `FileId` (Value Object), `VirtualFile` (Entity), and `ProjectWorkspace` (Aggregate Root).
* **Extension Hooks:** The domain exposes a `PluginHostPort` (Registry). It broadcasts life-cycle hooks (e.g., `on_file_indexed`, `on_file_changed`) to registered extensions without knowing their underlying technical implementation.

### B. Hexagonal Design (Ports & Adapters)

The domain interacts with the outside world strictly through abstract interfaces:

* **Inbound Ports (Driving API):** Traits defining capabilities exposed to consumers, such as `SearchUseCase` and `FileMutationUseCase`.
* **Outbound Ports (Driven SPI):** Traits defining system capabilities required by the domain, such as `StoragePort` and `FileSystemPort`.
* **Adapters (Infrastructure & Presentation):** Low-level implementations that satisfy the ports. Storage is handled by a `SledStorageAdapter`, file monitoring by a `NotifyWatcherAdapter`, and agent routing by a `McpJsonRpcAdapter`.

### C. "Drop & Play" WASM Plugin Runtime

To allow third-party contributors to scale the ecosystem effortlessly, plugins are decoupled from the host process:

* **The WASM Sandbox:** Plugins are distributed as single, pre-compiled `.wasm` binaries. The Core Host embeds **`wasmtime`** (Cranelift JIT compiler) to run these plugins in a secure, sandboxed environment with zero local system dependencies.
* **Capability-Based Security:** Plugins have no native access to the host's network, file system, or storage caches. They can only interact with the workspace via specific functions exposed through the Core's Outbound Ports (`Linker::func_wrap`).
* **Dynamic MCP Merging:** Upon initialization, loaded WASM plugins declare their custom tools to the host. The central MCP adapter dynamically unifies and registers these tools, exposing a single, cohesive interface to the AI client.

---

## 3. Functional Scope (MVP)

### A. Virtual File System (VFS) & Real-Time Tracking

* **Initial Workspace Scan:** Deep-scans and indexes the root project workspace at startup, assigning each file a unique `FileId` ($u32$) to save RAM and optimize CPU lookups.
* **Reactive File Monitoring:** Hooks into native OS filesystem events via `Notify` to instantly synchronize insertions, updates, and deletions with the VFS and the key-value database.

### B. Ultra-Fast File Search

* **Term Tokenization:** Automatically breaks down file paths and names into searchable tokens and sub-strings.
* **Sub-Millisecond Resolution:** Queries an automated inverted index to resolve file locations instantly, bypassing iterative disk lookups.

### C. Parallel Project Grep (Content Search)

* **Brute-Force CPU-Bound Search:** Scans text patterns across all files tracked by the VFS.
* **Work-Stealing Architecture:** Leverages a lightweight thread pool (`Rayon`) to map file chunks directly across all available CPU cores, maximizing SSD read performance without maintaining heavy text indices in the DB.

### D. Safe File Mutations (CRUD System)

* **Atomic File Writes:** Implements a *Shadow File* mutation pattern. Modified content is written to a temporary sibling file (`.tmp_write`), which then overwrites the original via an OS-level atomic `fs::rename`. This guarantees zero code corruption in the event of a crash or power failure.
* **Mass Refactoring:** Supports multi-threaded global find-and-replace queries across the workspace with automatic, transactional index invalidation.

---

## 4. Unified MCP API Specifications

The central `McpJsonRpcAdapter` unifies native Core utilities and third-party WASM tools into a single JSON-RPC schema over standard I/O (`stdin`/`stdout`):

### Core Native Tools

1. **`tower_find_file`**
* *Arguments:* `{ "query": "string" }`
* *Returns:* Array of matching file paths resolved via the inverted index.


2. **`tower_search_text`**
* *Arguments:* `{ "pattern": "string" }`
* *Returns:* Structured list of string matches including `FileId`, `path`, `line_number`, and `line_content`.


3. **`tower_read_file`**
* *Arguments:* `{ "path": "string" }`
* *Returns:* Raw text content of the target file.


4. **`tower_create_file`**
* *Arguments:* `{ "path": "string", "content": "string" }`
* *Returns:* Success confirmation following safe shadow-file creation.


5. **`tower_create_directory`**
* *Arguments:* `{ "path": "string" }`
* *Returns:* Recursive directory structure initialization confirmation.


6. **`tower_delete_file`**
* *Arguments:* `{ "path": "string" }`
* *Returns:* Confirmation of physical removal and immediate VFS index clearing.


7. **`tower_global_replace`**
* *Arguments:* `{ "target": "string", "replacement": "string" }`
* *Returns:* Transaction status of the mass-parallel refactoring job.



### First-Party Reference Plugin Tools (AST Multi-Language via Tree-sitter)

8. **`ast_get_outline`**
* *Arguments:* `{ "path": "string" }`
* *Returns:* A high-level semantic layout of the code (Classes, Functions, Methods, Scopes) extracted via Tree-sitter, allowing the AI to map out a file without pulling down thousands of raw text lines.


9. **`ast_find_symbols`**
* *Arguments:* `{ "symbol_name": "string", "kind": "string" }`
* *Returns:* Precise symbol definition locations across error-tolerant syntax trees, removing noise and false positives found in plain-text searches.



---

## 5. Directory Layout (Cargo Multi-Crate Workspace)

The codebase is organized as a Cargo Workspace to split the distributable SDK from the Core Host implementation:

```text
my_project_workspace/
├── Cargo.toml               # Workspace configuration file
├── crates/                  # Engine + SDK (host-side, default-members)
│   ├── core_engine/         # Core Host binary (Clean/Hexagonal)
│   │   └── src/
│   │       ├── domain/      # VFS logic, indexing, and PluginHost engine
│   │       ├── ports/       # Inbound (API) and Outbound (SPI) interfaces
│   │       └── adapters/    # Sled storage, Notify watcher, Wasmtime loader, MCP Server
│   │
│   ├── plugin_sdk/          # Shared SDK crate distributed to third-party developers
│   └── plugin_sdk_macros/   # Proc-macros: #[plugin_main], #[plugin_export]
│
└── plugins/                 # wasm32-wasip1 plugins (excluded from default-members)
    ├── ast/                 # Reference Tree-sitter WASM plugin (Go, PHP, Rust)
    ├── hello/               # Minimal example plugin
    └── fixtures/            # Test-only wasm fixtures (11c/11d)

```

---

## 6. Metrics of Success & Technical Constraints

* **Zero Lock Contention:** Concurrent access to the VFS (writes from the OS file watcher vs. reads from the AI client over MCP) must be managed using fine-grained synchronization primitives (`RwLock`) to prevent latency spikes during heavy indexing.
* **Fault Isolation (Plugin Host Resilience):** The crash, panic, or infinite loop of a third-party WASM plugin must never take down the Core Engine or sever the link with the parent MCP client. The host must trap exceptions and gracefully restart failing sandboxes in the background.
* **Platform Independence:** The Core Engine compiles down to a single, statically linked, zero-dependency native binary. No virtual machine (JVM), heavy containerization, or external runtime (Node.js) is required on the user's host machine.
* **Pure Testability:** 100% of the Core Domain and its pluggable interfaces must be fully testable using pure in-memory test doubles (Mocks), eliminating all dependencies on actual hardware disk-I/O during CI/CD test phases.
