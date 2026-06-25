# Extensions

Tower's extensibility model is an **out-of-process native extension** system. An extension is a
standalone native binary (a *sidecar*) that the host spawns as a child process and talks to over a
tower-specific **JSON-RPC 2.0 protocol on stdio**. Each extension contributes MCP tools (surfaced to
clients as `tower_<ext>_<tool>`), subscribes to workspace events, and reaches the workspace only
through host capability callbacks.

> **Previously WASM; now native sidecars.** Earlier versions sandboxed plugins as `wasm32-wasip1`
> modules inside an embedded `wasmtime` runtime. That model was replaced (spec 20) because tower wants
> an *extensibility* unit, not a *security sandbox*: rich extensions need subprocesses, sockets,
> long-lived state, and threads — exactly what WASI removes. Isolation is now the **OS process
> boundary**. The external MCP tool contract is unchanged: clients still see `tower_<ext>_<tool>`.

The engine remains a **single static binary** — no WASM, no WASI SDK, no JVM, no Node, no container at
runtime. Extensions are separate native executables discovered at startup.

---

## What an extension is

An extension is:

- a **native executable** (any language that can speak JSON-RPC 2.0 over stdio; the reference
  extensions are Rust binaries in `extensions/*`);
- described by an **`extension.toml` manifest** (identity, spawn command, activation, tools, event
  subscriptions, required capabilities);
- spawned as a **child process** of the host and isolated by the OS process boundary;
- a contributor of **MCP tools** that are merged into the host registry under the
  `tower_<ext>_<tool>` namespace;
- a **subscriber** to workspace events (`fileIndexed` / `fileChanged` / `fileDeleted`);
- a **consumer** of host capabilities (read a file, list files, read/write the AST index, request a
  format, log) — and *only* the capabilities it declares.

The shared wire contract lives in the `extension_protocol` crate (`crates/extension_protocol/`),
which contains types and serde (de)serialization only — no process, filesystem, or transport code.

---

## Wire protocol

The protocol is JSON-RPC 2.0 over stdio. Each message is a newline-delimited JSON object conforming to
the JSON-RPC 2.0 envelope:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{ ... }}
```

When `id` is present the message is a request; when absent it is a notification (used for event
delivery). The current protocol version is **`1`** (`extension_protocol::PROTOCOL_VERSION`); the host
sends it in `initialize` and the extension must echo it back so version mismatches are caught early.

### Lifecycle

```text
host  ──initialize──►  extension       (host sends protocol_version + client_info)
      ◄──Initialized─                  (extension declares tools, events, capabilities)
      ──invokeTool───►                 (host routes a tool call)
      ◄──ToolResult──
      ──deliverEvent─►                 (notification — no id)
      ◄──Ack─────────
      ──shutdown─────►                 (graceful stop)
      ◄──Ack─────────
```

| Method         | Direction        | Purpose                                                              |
|----------------|------------------|---------------------------------------------------------------------|
| `initialize`   | host → extension | Sent once after spawn. Carries `protocol_version` + `client_info`. The extension replies with its declared `tools`, subscribed `events`, and required `capabilities`. |
| `invokeTool`   | host → extension | Invoke a declared tool by `name` with a JSON `params` object. Reply is `ToolResult` (a JSON value). |
| `deliverEvent` | host → extension | Deliver a workspace event. Sent as a notification (no `id`); the extension replies `Ack`. |
| `shutdown`     | host → extension | Ask the extension to flush and exit. Reply `Ack`. |

Requests and responses are serialized with adjacent tagging
(`{"type":"<Variant>","data":<payload>}`; unit variants such as `Shutdown` emit no `data` key). Events
and host calls use internal tagging (`{"type":"<Variant>", ...}`).

Protocol-level failures come back as `Response::Error` carrying a JSON-RPC error object with the
standard codes (`-32700` parse, `-32600` invalid request, `-32601` method not found, `-32602` invalid
params, `-32603` internal).

---

## Events the host pushes

The host fans out workspace events to extensions that subscribe to them. Subscriptions are declared in
the manifest's `[events] subscribe` list and re-asserted in the `Initialized` response.

| Event method          | Payload                | Fired when |
|-----------------------|------------------------|------------|
| `event/fileIndexed`   | `{ file_id, path }`    | A file was indexed (or re-indexed) in the VFS. |
| `event/fileChanged`   | `{ file_id, path }`    | A file's content changed on disk and the VFS was updated. |
| `event/fileDeleted`   | `{ path }`             | A file was deleted from the workspace. |

These map to `ExtensionHostPort::on_file_indexed` / `on_file_changed` / `on_file_deleted` on the host
side and to the `Event::FileIndexed` / `FileChanged` / `FileDeleted` variants on the wire. `file_id`
is the numeric VFS file identifier; `path` is workspace-relative.

> Event delivery is **per-extension isolated**: a fault in one subscribed extension never blocks
> delivery to the others. The host does **not** buffer or replay history — an extension that needs
> events must use **eager** activation (see below), because a lazily-spawned extension would miss
> everything emitted before its first activation. The discovery loader rejects any `lazy` extension
> that declares event subscriptions.

---

## Capability callbacks

Extensions reach the workspace **only** through host capability callbacks (host calls). Each callback
is routed by the sidecar adapter to an existing outbound port — there is no privileged back-channel.
An extension may only use a capability it **declares** in its manifest; an undeclared `HostCall` is
rejected with a protocol error.

| Capability (manifest)  | Wire method                | Backed by port      | Effect |
|------------------------|----------------------------|---------------------|--------|
| `read_file`            | `workspace/readFile`       | `FileSystemPort`    | Read a workspace-relative file's bytes. |
| `list_files`           | `workspace/listFiles`      | `FileSystemPort`    | Enumerate indexed workspace files. |
| `index_get`            | `index/get`                | `AstIndexPort`      | Read a value from the AST index cache (e.g. key `ast/<path>`). |
| `index_put`            | `index/put`                | `AstIndexPort`      | Write bytes to the AST index cache. |
| `request_apply_edits`  | `workspace/applyEdits`     | `ApplyEditsHostPort`| Apply one or more byte-range edits across one or more workspace files through the host's CAS-guarded atomic mutation path, or preview them when `dry_run:true` is set. |
| `request_format`       | `workspace/requestFormat`  | `FormatQueuePort`   | Enqueue a workspace file for formatting. |
| `notify`               | `notify/resourceUpdated`   | MCP push channel    | Push a `notifications/resources/updated` event to subscribed MCP clients (best-effort, no round trip). |
| `log`                  | `log`                      | host logging        | Emit a log line (`trace`/`debug`/`info`/`warn`/`error`) through the host. |

### Capability security

- **Capability gating**: only the capabilities listed in `[capabilities] required` are reachable. The
  sidecar adapter rejects any host call whose capability was not declared.
- **Path validation**: every path argument passed to a workspace capability is validated before use.
  Empty paths, absolute paths, and any path containing a `..` traversal component (`..`, leading
  `../`, embedded `/../`, trailing `/..`) are rejected with a JSON-RPC error. Extensions cannot escape
  the workspace root.
- **No ambient access**: an extension has no special filesystem, network, or storage-cache access. The
  only way into the workspace is through the declared capability callbacks above.

---

## The `extension.toml` manifest

An `extension.toml` at the root of an extension's directory declares its identity, the process to
spawn, when to activate it, the tools it contributes, the events it subscribes to, and the
capabilities it requires.

```toml
name    = "ast"
version = "0.1.0"
# Command to spawn (argv[0] + args). Resolved relative to the extension directory at runtime.
command    = ["ast_extension"]
# "eager" (spawn at startup) or "lazy" (spawn on first invocation). Default: "lazy".
activation = "eager"

# Events to subscribe to. Eager activation is required if any are listed.
[events]
subscribe = ["event/fileIndexed", "event/fileChanged"]

# Host capabilities the extension is allowed to call.
[capabilities]
required = ["read_file", "list_files", "index_get", "index_put", "log"]

# Tools contributed to the MCP registry. Each is namespaced as tower_<name>_<tool>.
[[tools]]
name        = "get_outline"
description = "Return a structural outline for a workspace-relative source file."
schema_json = '{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}'
```

| Field                    | Meaning |
|--------------------------|---------|
| `name`                   | Unique extension identifier (snake_case recommended). Used as the `tower_<name>_*` tool namespace and matched against the disable list. |
| `version`                | SemVer string. |
| `command`                | Argv to spawn the child process. |
| `activation`             | `eager` (spawn at startup) or `lazy` (spawn on first tool call / event). Defaults to `lazy`. |
| `[events] subscribe`     | List of event methods (`event/fileIndexed`, `event/fileChanged`, `event/fileDeleted`). Requires `eager` activation. |
| `[capabilities] required`| List of capability names the extension may call (see the table above). |
| `[[tools]]`              | One block per tool: `name`, `description`, and `schema_json` (a JSON Schema string for the tool's input). Tools are merged into the MCP registry as `tower_<name>_<tool>`. |

---

## Discovery and activation

At startup the host discovers extensions by reading `<dir>/<name>/extension.toml` manifests from the
resolved search path.

### Search path (highest precedence first)

1. `--extensions-dir <path>` CLI flag — **replaces** the entire search path with that one directory.
2. `$TOWER_EXTENSIONS_DIR` environment variable — same replace semantics (a blank value is ignored).
3. Default multi-scope path:
   - **Global** — the XDG data directory (`<xdg-data>/tower/extensions/`), scanned **first** (lower
     precedence; shared across projects).
   - **Local** — `<workspace>/.tower/extensions/`, scanned **last** (wins on a name collision; this
     project only).

### Activation

| Activation | Behavior |
|------------|----------|
| `eager`    | The child process is spawned immediately at startup (via `SidecarHostAdapter::spawn`). Required for any extension that subscribes to events. |
| `lazy`     | A supervisor is created but the child is **not** spawned until the first tool invocation. Lazy extensions must not subscribe to events (the host does not replay history). |

Failures are non-fatal to the host: a `lazy + events` manifest is skipped with a stderr warning, and a
failed eager spawn is skipped with a stderr warning while startup continues. An extension listed in the
config disable list (see below) is never spawned.

---

## Supervision and fault model

The host supervises each extension and isolates faults so a misbehaving extension can never take down
the host or sever the MCP link.

### Fault kinds

| Fault           | Cause |
|-----------------|-------|
| `Timeout`       | The extension did not respond within the configured per-call deadline. The child is killed. |
| `Crashed`       | The child process exited unexpectedly (carries the OS exit code when available). |
| `ProtocolError` | The extension sent or received a message that violated the protocol. |
| `Quarantined`   | The extension was disabled after too many consecutive faults (see below). |

### Timeout

Every `invokeTool` and `deliverEvent` is bounded by `request_timeout_secs` (default **30 s**, set in
`[extensions]` config). Exceeding the deadline kills the child and returns `Timeout` to the caller.

### Restart with backoff

The supervisor respawns a crashed or timed-out **lazy** child on the *next* call, after an exponential
backoff: `min(2^n · 100 ms, 30 s)`. A successful call clears the backoff state. The supervisor itself
does not quarantine — it keeps offering to respawn as long as the registry keeps asking.

### Quarantine

The domain registry tracks consecutive faults per extension. After **3** consecutive faults
(`MAX_CONSECUTIVE_FAILURES`) the extension is marked `Quarantined`: subsequent tool invocations and
event deliveries return `Quarantined` immediately without contacting the instance. A single successful
call before the threshold resets the counter to zero.

This is a **runtime-agnostic policy** that lives in the domain (`ExtensionRegistry`), while the actual
process spawn/kill/respawn lives in the adapter (`SidecarHostAdapter` + `ExtensionSupervisor`).

---

## Configuration

Per-project extension settings live in `<workspace>/.tower/config.toml` under the `[extensions]`
table:

```toml
[extensions]
# Wall-clock deadline for a single tool call or event delivery, in seconds (default 30).
request_timeout_secs = 30
# Extension manifest names to skip loading entirely (matched on `name`, not path).
disabled = ["lsp"]
```

A disabled extension is never spawned — the check happens before any process starts. An absent config
file means defaults (30 s timeout, nothing disabled); a malformed config file fails startup (exit 1).

### Opt-in debug configuration

The `debug` extension is available only when at least one `[debug.<language>]` entry exists in
`<workspace>/.tower/config.toml` and the extension is not listed in `[extensions].disabled`.
The host parses this configuration strictly at startup and passes it to the sidecar during
`initialize`; malformed debug config fails startup instead of exposing a partially configured
debugger.

```toml
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
args = ["--stdio"]
adapter_type = "lldb"
launch = { request = "launch", program = "target/debug/app" }
default_timeout_secs = 15
idle_ttl_secs = 300

[debug.record]
backend = "rr"
trace_dir = ".tower/traces"
ttl_secs = 86400
max_traces = 20
record_timeout_secs = 60
```

| Field | Meaning |
|-------|---------|
| `extensions` | File extensions, without leading `.`, associated with this debug language. Must not be empty. |
| `command` | Debug adapter executable. Must not be empty. |
| `args` | Optional adapter arguments. |
| `adapter_type` | DAP adapter identifier sent during initialize, for example `lldb`, `go`, or `python`. |
| `launch` | Adapter-specific default launch arguments. Tool-level `launch_overrides` are merged into this object. |
| `default_timeout_secs` | Positive per-operation timeout used by launch, resume, inspect, and cleanup calls unless a tool overrides it. |
| `idle_ttl_secs` | Positive idle lifetime for an inactive debug session before the sidecar terminates and reaps it. |

Add `[debug.record] backend = "rr"` only when rr record/replay tools should be exposed. The host
validates this section at startup and forwards it through the sidecar initialize payload; the sidecar
does not reread `.tower/config.toml`.

| Field | Meaning |
|-------|---------|
| `backend` | Required record backend. The only supported value is `"rr"`. |
| `trace_dir` | Optional workspace-relative trace root. Defaults to `.tower/traces`; absolute paths and `..` traversal are rejected. |
| `ttl_secs` | Optional positive trace TTL in seconds. Omit for no TTL expiry. |
| `max_traces` | Optional positive retained trace limit. Defaults to `20`; pruning still applies without TTL. |
| `record_timeout_secs` | Optional positive default record timeout. Defaults to `60`; tool calls may pass `timeout_ms`. |

The debug sidecar declares no workspace mutation capabilities. A discovered extension named `debug`
takes priority over the bundled fallback. It owns adapter and debuggee process lifecycle inside the
extension process and removes ephemeral sessions on terminate, disconnect, shutdown, quarantine, or
idle expiry. See [debug sessions](user-guide/debug-sessions.md) for the operator workflow and
[MCP tool reference](mcp-tools.md#debug-tools) for the tool contract.

---

## Authoring an extension

The reference extensions live in `extensions/`:

| Extension          | Activation | Highlights |
|--------------------|------------|------------|
| `extensions/hello` | lazy       | Minimal example — a single `greet` tool, no events, no capabilities. The smallest possible extension. |
| `extensions/ast`   | eager      | Tree-sitter AST analysis plus anchored symbol edits. Subscribes to `fileIndexed`/`fileChanged`, uses `read_file`/`list_files`/`index_get`/`index_put`/`request_apply_edits`/`log`. Tools: `get_outline`, `find_symbols`, `search_symbols`, `reindex`, `read_symbol`, `replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`, `delete_symbol`. |
| `extensions/lsp`   | eager      | Language-server bridge. Subscribes to `fileChanged`/`fileDeleted`, uses `read_file`/`notify`/`log`/`request_apply_edits`. Tools: `diagnostics`, `definition`, `references`, `hover`, `implementations`, `rename`. |
| `extensions/lint`  | lazy       | Standalone linter runner. Uses `read_file`/`list_files`/`request_apply_edits`/`log`, runs configured external linters read-only, returns LSP-shaped diagnostics, and applies structured fixes through the host. Tools: `check`, `fix`. |

The `lint` extension is configured from `<workspace>/.tower/config.toml` under `[lint.<language>]`.
Each entry declares a command, file extensions, parser format, and target mode:

```toml
[lint.rust]
command = "cargo"
args = ["clippy", "--message-format=json"]
extensions = ["rs"]
format = "rustc-json"
target = "none"
```

See [mcp-tools.md](mcp-tools.md#standalone-lint-tools) for the `tower_lint_check` and
`tower_lint_fix` wire contracts and the full lint configuration reference.

### Minimal walkthrough (`hello`)

1. Write a native binary that:
   - reads newline-delimited JSON-RPC requests from stdin and writes responses to stdout;
   - on `initialize`, replies with its `tools` (the `greet` tool), an empty `events` list, and an
     empty `capabilities` list;
   - on `invokeTool` with `name = "greet"`, returns a greeting JSON value;
   - on `shutdown`, flushes and exits.
2. Write `extension.toml`:

   ```toml
   name       = "hello"
   version    = "0.1.0"
   command    = ["hello_extension"]
   activation = "lazy"

   [[tools]]
   name        = "greet"
   description = "Return a greeting string."
   schema_json = '{"type":"object","properties":{"name":{"type":"string"}}}'
   ```
3. Drop the binary + manifest into an extension scope (global XDG dir or `<workspace>/.tower/extensions/<name>/`)
   and restart `tower`. The tool appears in `tools/list` as `tower_hello_greet`.

For an extension that needs workspace access (like `ast`), declare the capabilities you call in
`[capabilities] required` and use the corresponding host-call methods; for one that reacts to file
changes (like `ast`/`lsp`), declare your event subscriptions in `[events] subscribe` and choose
`eager` activation if you subscribe to events.

The shared protocol types (`Request`, `Response`, `Event`, `HostCall`, `ExtensionManifest`,
`Capability`, `ToolDecl`, `PROTOCOL_VERSION`) are available from the `extension_protocol` crate if you
write the extension in Rust.

---

## Related pages

- [architecture.md](architecture.md) — the hexagon, ports & adapters, the extension host runtime
- [mcp-tools.md](mcp-tools.md) — the native `tower_*` tools and the extension tools in full detail
- [getting-started.md](getting-started.md) — build, run, and configure extensions for a workspace
- [development.md](development.md) — quality gate, CI, test strategy, contributing
