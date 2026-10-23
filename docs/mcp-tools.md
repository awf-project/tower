# MCP Tool Reference

tower exposes its workspace capabilities over a JSON-RPC 2.0 stdio interface
following the Model Context Protocol (MCP). This page covers the wire protocol,
the full tool catalogue, and concrete copy-paste examples.

---

## Wire protocol

```
stdin  ──► one JSON object per line ──► tower dispatcher ──► stdout
```

- **Framing**: newline-delimited JSON. Each message is a single JSON object
  terminated by `\n`. There are no Content-Length headers (unlike LSP).
- **Version**: JSON-RPC 2.0. The `"jsonrpc": "2.0"` field is required on every
  request.
- **Notifications**: requests with no `id` field receive no response and are
  silently dropped.
- **Resilience**: a malformed line returns a `ParseError` response; the loop
  continues reading the next line.

---

## Launching the server

```bash
# Default: workspace = current working directory
cargo run -p core_engine

# Explicit workspace root via flag
./tower --workspace-dir /path/to/project

# Explicit workspace root via environment variable
TOWER_WORKSPACE=/path/to/project ./tower
```

Priority order for workspace root resolution:

1. `--workspace-dir <path>` command-line flag
2. `$TOWER_WORKSPACE` environment variable
3. Current working directory

The binary name is `tower` (from `[[bin]] name = "tower"` in
`crates/core_engine/Cargo.toml`).

---

## Session lifecycle

### Daemon startup and readiness

`tower mcp` connects to (or starts) one daemon for the workspace. The daemon keeps its workspace
ownership for its full lifetime and binds the client transport before storage reconciliation and
extension catch-up. Consequently, MCP `initialize` can complete while preparation is still in
progress, and a client-side connection timeout does not release ownership or cause a competing
daemon to open the same Sled database.

Readiness is tracked independently for the workspace, AST, and LSP families. `tower status` reports
the daemon generation, lifecycle, each family’s state, and completed startup instrumentation. The
possible lifecycle states are `starting`, `ready`, `degraded`, `failed`, and `stopping`; a capability
can additionally be `preparing`, `ready`, `degraded`, `failed`, `unavailable`, or `stopping`.

Native control and tool-description requests remain available during preparation. Workspace, AST,
and LSP operations are admitted only when their dependencies are ready, or return a bounded
wait/structured readiness result. Clients must not interpret a tool inventory during catch-up as
proof that every extension result is exhaustive.

### 1. initialize

Send one `initialize` request at session start. tower responds with protocol
version and capability advertisement.

**Request**

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
```

**Response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "protocolVersion": "2024-11-05",
    "serverInfo": {
      "name": "tower",
      "version": "0.1.0"
    },
    "capabilities": {
      "tools": {}
    }
  },
  "id": 1
}
```

### 2. tools/list

Enumerate every available tool. The response lists the native `tower_*` tools
plus any namespaced tools contributed by discovered extensions (e.g.
`"tower_ast_get_outline"` from the `ast` extension, `"tower_lsp_diagnostics"` from
the `lsp` extension, `"tower_debug_launch"` from the `debug` extension, or
`"tower_lint_check"` from the `lint` extension).

**Request**

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

**Response (excerpt)**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "tools": [
      {
        "name": "tower_find_file",
        "description": "Find files in the workspace whose path matches the query string.",
        "inputSchema": {
          "type": "object",
          "properties": {
            "query": { "type": "string", "description": "Substring or fuzzy query to match against file paths." }
          },
          "required": ["query"]
        }
      },
      {
        "name": "tower_ast_get_outline",
        "description": "Return a structural outline ...",
        "inputSchema": { "..." : "..." }
      }
    ]
  },
  "id": 2
}
```

### 3. tools/call

Call a tool by name. The `params` object must contain `"name"` (the tool name)
and optionally `"arguments"` (a JSON object of tool-specific arguments; defaults
to `{}` when absent).

**Request shape**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "<tool-name>",
    "arguments": { "<arg>": "<value>" }
  }
}
```

**Success response shape**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [
      { "type": "text", "text": "<json-encoded result string>" }
    ]
  },
  "id": 3
}
```

The `text` field contains the tool result JSON-encoded as a string. Clients must
parse it with a second `JSON.parse` (or equivalent) to obtain the structured
result object.

---

## Error codes

| Code    | Name             | Meaning                                                                 |
|---------|------------------|-------------------------------------------------------------------------|
| -32700  | ParseError       | Malformed JSON or invalid UTF-8 in the request frame                   |
| -32600  | InvalidRequest   | `jsonrpc` field is not exactly `"2.0"`                                  |
| -32601  | MethodNotFound   | The JSON-RPC method name (e.g. `"tools/call"`) is unknown               |
| -32602  | InvalidParams    | A required tool argument is missing or has the wrong type               |
| -32603  | InternalError    | Tool execution failed at runtime (I/O error, extension fault, etc.)     |
| -32001  | ToolNotFound     | The named tool does not exist in the registry                           |
| -32002  | ResourceNotFound | The workspace entity targeted by the tool was not found (stable code — clients may branch on this without parsing the message) |
| -32004  | InitializationPending | The required capability is still preparing; `data` identifies the dependency and phase. |
| -32005  | CapabilityInitializationFailed | The required capability failed; `data` includes its failure class. |
| -32006  | DaemonStopping | The daemon is stopping and will not admit new work. |
| -32007  | CapabilityUnavailable | The requested capability is not configured or was disabled. |

Codes `-32001`, `-32002`, and `-32004` through `-32007` are in the JSON-RPC 2.0
server-defined range (`-32000` to `-32099`). `-32001` is for an unknown **tool name** inside
`tools/call`; `-32601` is reserved for an unknown **RPC method** name.

**Error response shape**

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32602,
    "message": "Invalid params: required field 'query' is missing or not a string"
  },
  "id": 3
}
```

---

## Native tools

The native tools are always available, regardless of which extensions are loaded. Their
names are undecorated (no namespace prefix).

### tower_safe_delete_symbol

Safely delete one declaration after resolving its references and optional backend cleanup edits.
This native tool does not replace `tower_ast_delete_symbol`, which remains the explicit span-only
deletion operation.

| Field         | Type    | Required | Description                                             |
| ------------- | ------- | -------- | ------------------------------------------------------- |
| `path`        | string  | yes      | Workspace-relative file containing the declaration     |
| `symbol_name` | string  | yes      | Nonempty declaration name                               |
| `kind`        | string  | no       | Declaration kind filter; omit to match any kind         |
| `dry_run`     | boolean | no       | Preview without mutation; defaults to `false`           |

Valid `kind` values are `function`, `struct`, `enum`, `trait`, `impl`, `method`, `module`,
`type_alias`, `const`, `static`, `macro_def`, and `class`.

Example request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "tower_safe_delete_symbol",
    "arguments": {
      "path": "src/unused.rs",
      "symbol_name": "obsolete_helper",
      "kind": "function",
      "dry_run": true
    }
  }
}
```

The result contains `status`, optional `blocking_reason`, the resolved `declaration` when
available, all `references`, proposed cleanup `proposed_edits`, ordered `affected_files`, and an
optional `mutation_result`. A `previewed` status means the dry-run plan passed validation;
`applied` means the requested mutation completed. A `blocked` status includes a
`blocking_reason`, such as `live_references`, `cleanup_incomplete`, `cas_conflict`, or
`backend_error`. Safe delete fails closed when references remain without complete backend
cleanup, or when analysis is unavailable, unresolved, malformed, stale, overlapping, or outside
the workspace. `dry_run:true` does not change file contents, versions, or index state.

`status` is one of `blocked`, `previewed`, `applied`, or `partial_commit`. A blocked report may
use one of these stable `blocking_reason` values: `not_found`, `ambiguous_symbol`,
`invalid_declaration`, `capability_unavailable`, `references_unresolved`, `live_references`,
`cleanup_incomplete`, `invalid_cleanup`, `cas_conflict`, or `backend_error`.

Validation and CAS failures occur before the first write; a later filesystem or persistence failure
can return `partial_commit` with per-file outcomes identifying committed, failed, and unattempted
files. Inspect `status` before treating the operation as complete.

### tower_find_file

Find workspace files whose path matches a query string via the inverted index.

| Field   | Type   | Required | Description                                      |
|---------|--------|----------|--------------------------------------------------|
| `query` | string | yes      | Substring or fuzzy query matched against paths   |

**Returns** `{"paths": [string]}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "tools/call",
  "params": {
    "name": "tower_find_file",
    "arguments": { "query": "client" }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"paths\":[\"src/client.rs\"]}" }]
  },
  "id": 10
}
```

Empty result when nothing matches: `{"paths": []}`.

---

### tower_list_dir

List indexed files and synthesized directories under a workspace-relative path.
The listing is derived from the tracked path set, not from a live filesystem
walk, so it inherits the current index and ignore rules. Empty directories are
not tracked.

| Field       | Type    | Required | Description                                                        |
|-------------|---------|----------|--------------------------------------------------------------------|
| `path`      | string  | yes      | Workspace-relative directory path to list; `""` and `"."` mean root |
| `recursive` | boolean | no       | Include descendants instead of only direct children                 |
| `max_depth` | integer | no       | Positive recursion depth limit; valid only when `recursive` is true |

**Returns** `{"entries": [{"path": string, "name": string, "kind": "file"|"dir"}]}`

Entries are sorted by workspace-relative path. Directory entries are synthesized
from tracked file prefixes and de-duplicated.

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "method": "tools/call",
  "params": {
    "name": "tower_list_dir",
    "arguments": { "path": "src", "recursive": true, "max_depth": 1 }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"entries\":[{\"path\":\"src/lib.rs\",\"name\":\"lib.rs\",\"kind\":\"file\"},{\"path\":\"src/net\",\"name\":\"net\",\"kind\":\"dir\"}]}" }]
  },
  "id": 11
}
```

Missing or untracked prefixes return an empty success payload:
`{"entries": []}`. A `path` that names a tracked file returns `-32602
InvalidParams` with a not-a-directory message. Supplying `max_depth` without
`recursive: true`, or supplying `max_depth: 0`, also returns `-32602`.

---

### tower_search_text

Parallel grep across all indexed file contents.

| Field     | Type   | Required | Description                                      |
|-----------|--------|----------|--------------------------------------------------|
| `pattern` | string | yes      | Text pattern searched across all indexed files   |

**Returns** `{"matches": [{"path": string, "line_number": uint, "line_content": string}]}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "method": "tools/call",
  "params": {
    "name": "tower_search_text",
    "arguments": { "pattern": "fn client" }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"matches\":[{\"path\":\"src/client.rs\",\"line_number\":1,\"line_content\":\"fn client() {}\"}]}" }]
  },
  "id": 12
}
```

---

### tower_read_file

Read the raw UTF-8 content of a workspace-relative file. Uses `FileSystemPort`
directly (no domain logic wrapper); returns `-32002` if the path does not exist.

| Field  | Type   | Required | Description                            |
|--------|--------|----------|----------------------------------------|
| `path` | string | yes      | Workspace-relative path to the file    |

**Returns** `{"content": string}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 13,
  "method": "tools/call",
  "params": {
    "name": "tower_read_file",
    "arguments": { "path": "src/main.rs" }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"content\":\"fn main() {}\\n\"}" }]
  },
  "id": 13
}
```

**Error: file not found**

```json
{
  "jsonrpc": "2.0",
  "error": { "code": -32002, "message": "Resource not found: path or entity not found in workspace" },
  "id": 13
}
```

---

### tower_create_file

Create or overwrite a file using the shadow-file pattern (`.tmp_write` sibling
→ flush → atomic `fs::rename`). The file is indexed immediately.

| Field     | Type   | Required | Description                                    |
|-----------|--------|----------|------------------------------------------------|
| `path`    | string | yes      | Workspace-relative path for the new file       |
| `content` | string | yes      | UTF-8 content to write                         |

**Returns** `{"created": true}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 14,
  "method": "tools/call",
  "params": {
    "name": "tower_create_file",
    "arguments": { "path": "src/widget.rs", "content": "pub struct Widget;" }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"created\":true}" }]
  },
  "id": 14
}
```

---

### tower_create_directory

Create a directory (recursive `mkdir_all`). Does not index any files; directories
become visible to subsequent file operations.

| Field  | Type   | Required | Description                                    |
|--------|--------|----------|------------------------------------------------|
| `path` | string | yes      | Workspace-relative path of the directory       |

**Returns** `{"created": true}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 15,
  "method": "tools/call",
  "params": {
    "name": "tower_create_directory",
    "arguments": { "path": "src/handlers" }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"created\":true}" }]
  },
  "id": 15
}
```

---

### tower_delete_file

Remove a file from the workspace and the VFS index. Returns `-32002` if the
path is not found.

| Field  | Type   | Required | Description                              |
|--------|--------|----------|------------------------------------------|
| `path` | string | yes      | Workspace-relative path of the file      |

**Returns** `{"deleted": true}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 16,
  "method": "tools/call",
  "params": {
    "name": "tower_delete_file",
    "arguments": { "path": "src/old.rs" }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"deleted\":true}" }]
  },
  "id": 16
}
```

---

### tower_global_replace

Parallel mass find-and-replace across every indexed file. Each file is rewritten
atomically via the shadow-file pattern. Partial failures (per-file I/O errors)
are reported in `errors` without aborting the remaining files.

**Warning — purely textual, not AST-aware**

`tower_global_replace` rewrites every literal byte-for-byte occurrence of the
search string across **all indexed files** regardless of file type or syntactic
context. It has no understanding of the language structure around a match. This
means:

- Occurrences inside **code comments** are replaced.
- Occurrences inside **string literals** are replaced.
- Occurrences in **Markdown prose**, **plain-text files**, and any other
  non-code format are replaced.
- A match that spans a rename in one file will be applied identically in every
  other file that happens to contain the same byte sequence.

**Observed example.** Renaming `compute_area` → `calculate_area` affected
**6 files / 10 occurrences**, touching: a function definition, its call-sites,
a `// compute_area: legacy alias` comment, a `docs/api.md` prose paragraph, and
a `CHANGELOG.txt` entry — all rewritten unconditionally.

**Recommended workflow — always check blast radius first:**

1. Run `tower_search_text` with your `target` string to review every occurrence
   and its surrounding context before committing to the replacement.
2. Inspect the results: if any hit is inside a comment, a string literal, or a
   prose file that should not change, consider a targeted edit instead.
3. Only then call `tower_global_replace`.

| Field         | Type   | Required | Description                                    |
|---------------|--------|----------|------------------------------------------------|
| `target`      | string | yes      | The string to search for                       |
| `replacement` | string | yes      | The string to substitute in every occurrence   |

**Returns** `{"files_changed": uint, "replacements": uint, "errors": [{"path": string, "reason": string}]}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 17,
  "method": "tools/call",
  "params": {
    "name": "tower_global_replace",
    "arguments": { "target": "OldName", "replacement": "NewName" }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"files_changed\":3,\"replacements\":7,\"errors\":[]}" }]
  },
  "id": 17
}
```

---

### tower_reindex

Force a full rebuild of the VFS and the inverted text-search index from the
current filesystem. Use after large external changes or to correct drift. Takes
no arguments.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| _(none)_ | — | — | This tool takes no arguments |

**Returns** `{"files_indexed": uint}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 18,
  "method": "tools/call",
  "params": { "name": "tower_reindex", "arguments": {} }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"files_indexed\":128}" }]
  },
  "id": 18
}
```

---

### tower_edit_range

Surgical byte-range edit of an **existing** file: splice
`[start_byte, end_byte) := replacement` into the current content and commit the
full result through the same atomic shadow-file path as `tower_create_file`. This
is the precise alternative to a whole-file `tower_create_file` rewrite or an
ambiguous `tower_global_replace`. Pairs naturally with `tower_ast_read_symbol`:
read a symbol's `start_byte`/`end_byte`, then edit exactly that span.

The replacement is applied **byte-exact** — no normalization and no implicit
formatting.

| Field         | Type    | Required | Description                                                        |
|---------------|---------|----------|--------------------------------------------------------------------|
| `path`        | string  | yes      | Workspace-relative path of the existing file to edit               |
| `start_byte`  | integer | yes      | Start of the replaced range (inclusive), `≥ 0`                     |
| `end_byte`    | integer | yes      | End of the replaced range (exclusive), `≥ start_byte`             |
| `replacement` | string  | yes      | UTF-8 text spliced in place of `[start_byte, end_byte)`            |

**Returns** `{"files_changed": uint, "replacements": uint, "errors": [{"path": string, "reason": string}]}`
— on success, `{"files_changed": 1, "replacements": 1, "errors": []}`.

**Validation (no write on failure)**

- The range must satisfy `0 ≤ start_byte ≤ end_byte ≤ file_length`, and both
  offsets must fall on UTF-8 character boundaries. A bad range (out of bounds,
  `start > end`, mid-codepoint split, or a non-UTF-8 target file) returns
  `-32602 InvalidParams` and the file is left untouched.
- A missing file returns `-32002 ResourceNotFound`. `tower_edit_range` edits
  existing files only; it never creates (use `tower_create_file` for that).
- An empty `replacement` deletes the span; `start_byte == end_byte` inserts
  without deleting (a pure insertion, including append at end-of-file).

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 19,
  "method": "tools/call",
  "params": {
    "name": "tower_edit_range",
    "arguments": {
      "path": "src/lib.rs",
      "start_byte": 100,
      "end_byte": 160,
      "replacement": "pub fn handle() -> Result<()> { Ok(()) }"
    }
  }
}
```

**Success response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "content": [{ "type": "text", "text": "{\"files_changed\":1,\"replacements\":1,\"errors\":[]}" }]
  },
  "id": 19
}
```

### tower_rename_path

Rename a workspace-relative file or directory after validating the complete
move against workspace boundaries, exclusions, conflicts, and optimistic
concurrency checks. With a configured language server, the tool can also plan
and apply import/reference edits through the existing workspace-edit path.

| Field | Type | Required | Description |
|---|---|---:|---|
| `source` | string | yes | Workspace-relative file or directory to move |
| `destination` | string | yes | Workspace-relative destination; it must not already exist or be inside the source |
| `dry_run` | boolean | no | Return the plan without changing disk, contents, versions, or indexes; defaults to `false` |
| `semantic_policy` | string | yes | `none`, `best_effort`, or `required` |
| `expected_versions` | object | no | Map of workspace-relative paths to 64-character SHA-256 CAS tokens; defaults to `{}` |

`none` performs a native-only move. `best_effort` uses semantic edits when
available and reports backend failures in the result. `required` returns an
`unsupported` result without mutation when the backend cannot provide the
requested rename support.

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "method": "tools/call",
  "params": {
    "name": "tower_rename_path",
    "arguments": {
      "source": "src/old.rs",
      "destination": "src/new.rs",
      "semantic_policy": "none"
    }
  }
}
```

**Result**

```json
{
  "status": "applied",
  "plan": {
    "moves": [
      { "source": "src/old.rs", "destination": "src/new.rs", "kind": "file" }
    ],
    "edits": [],
    "semantic_policy": "none"
  },
  "errors": []
}
```

Possible statuses are `preview`, `applied`, `partial`, `unsupported`, and
`rejected`. Errors use stable codes such as `source_not_found`,
`destination_exists`, `destination_excluded`, `workspace_escape`,
`semantic_unsupported`, `filesystem_error`, `persistence_error`, and
`cas_conflict`. All returned paths are workspace-relative. A `partial` result
means the native move committed but persistence or later semantic edits failed;
cross-file rename work is intentionally not a single transaction. See
[Rename a File or Directory](user-guide/path-rename.md) for preview and recovery steps.

---

## Code-intelligence tools (LSP)

The declared planning tools `tower_lsp_plan_path_rename` and
`tower_lsp_plan_safe_delete` return semantic plans without applying mutations.
The native `tower_rename_path` and `tower_safe_delete_symbol` tools compose those
plans with host validation and application. All local and MCP-facing tool names
use snake_case, including planning operations.

The `tower_lsp_*` tools surface language-server intelligence over the same
`tools/call` interface. They are contributed by the `lsp` extension (manifest
`name = "lsp"`) and appear in `tools/list` whenever that extension is discovered.
For read-only queries, when there is no backend or the file's language is not
supported by the configured server, the tool returns a normal result with
`"supported": false` (and an empty payload) rather than an error — so an agent can branch on that flag
and fall back to the structural `tower_ast_*` tools.

Positions use **zero-based** `line` and `character`, where `character` is a
**UTF-16 code-unit** column (LSP convention).

### tower_lsp_diagnostics

Run the configured language server over a file's current content and return
compiler/linter diagnostics. Use after editing a file to verify the change.

| Field  | Type   | Required | Description                                |
|--------|--------|----------|--------------------------------------------|
| `path` | string | yes      | Workspace-relative path of the file        |

**Returns**

```json
{
  "supported": true,
  "diagnostics": [
    {
      "line": 4, "character": 8, "endLine": 4, "endCharacter": 15,
      "severity": "error",
      "message": "cannot find value `foo` in this scope",
      "source": "rustc",
      "code": "E0425"
    }
  ]
}
```

`severity` is one of `error`, `warning`, `info`, `hint`. No backend or
unsupported language → `{"supported": false, "diagnostics": []}`.

### tower_lsp_definition

Resolve the definition site(s) of the symbol at a position.

| Field       | Type    | Required | Description                                   |
|-------------|---------|----------|-----------------------------------------------|
| `path`      | string  | yes      | Workspace-relative path of the file           |
| `line`      | integer | yes      | Zero-based line number                        |
| `character` | integer | yes      | Zero-based UTF-16 code-unit column            |

**Returns**

```json
{
  "supported": true,
  "locations": [
    { "path": "src/lib.rs", "line": 10, "character": 7, "endLine": 10, "endCharacter": 13 }
  ]
}
```

No backend / unsupported → `{"supported": false, "locations": []}`.

### tower_lsp_references

Find all reference sites of the symbol at a position. Same arguments as
`tower_lsp_definition`. Returns the locations under a `"references"` key:
`{"supported": true, "references": [ {path, line, character, endLine, endCharacter}, … ]}`.
No backend / unsupported → `{"supported": false, "references": []}`.

### tower_lsp_hover

Get hover information (type/doc) for the symbol at a position. Same arguments as
`tower_lsp_definition`.

**Returns**

```json
{
  "supported": true,
  "hover": {
    "contents": "fn handle() -> Result<()>",
    "line": 10, "character": 7, "endLine": 10, "endCharacter": 13
  }
}
```

The range fields are present only when the server reports one. No symbol under the
cursor → `{"supported": true, "hover": null}`. No backend / unsupported →
`{"supported": false, "hover": null}`.

### tower_lsp_implementations

Find implementation sites for the symbol at a position. Same arguments as
`tower_lsp_definition`.

**Returns**

```json
{
  "supported": true,
  "locations": [
    { "path": "src/lib.rs", "line": 10, "character": 7, "endLine": 10, "endCharacter": 13 },
    { "path": "src/impls.rs", "line": 4, "character": 6, "endLine": 4, "endCharacter": 18 }
  ]
}
```

If the configured server or language does not support implementation lookup, the
tool returns `{"supported": false, "locations": []}`.

### tower_lsp_type_hierarchy

Query the supertypes, subtypes, or both for the symbol at a position. The tool uses the configured
language server's type-hierarchy capability and is read-only.

| Field       | Type    | Required | Description                                  |
|-------------|---------|----------|----------------------------------------------|
| `path`      | string  | yes      | Workspace-relative path of the file          |
| `line`      | integer | yes      | Zero-based line number                       |
| `character` | integer | yes      | Zero-based UTF-16 code-unit column           |
| `direction` | string  | yes      | `supertypes`, `subtypes`, or `both`          |

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "method": "tools/call",
  "params": {
    "name": "tower_lsp_type_hierarchy",
    "arguments": {
      "path": "src/lib.rs",
      "line": 12,
      "character": 7,
      "direction": "both"
    }
  }
}
```

**Returns**

```json
{
  "supported": true,
  "supertypes": [
    {
      "name": "Base",
      "kind": "class",
      "detail": null,
      "range": { "line": 2, "character": 6, "endLine": 2, "endCharacter": 10 },
      "selectionRange": { "line": 2, "character": 6, "endLine": 2, "endCharacter": 10 },
      "path": "src/base.rs",
      "externalUri": null,
      "external": false
    }
  ],
  "subtypes": []
}
```

Workspace items contain a workspace-relative `path` and set `external:false`. Items outside the
workspace are retained as read-only results with `path:null`, their `externalUri`, and
`external:true`; they cannot be used as writable workspace paths. `range` and `selectionRange`
use zero-based UTF-16 coordinates.

When no language server is configured or the server does not advertise type hierarchy, the call
still succeeds with `supported:false`, empty `supertypes` and `subtypes`, and an `outcome` describing
the unsupported reason. Invalid arguments return a JSON-RPC error; backend failures use the standard
read-only LSP outcome envelope described in [Error codes](#error-codes).

### tower_lsp_rename

Rename the symbol at a position through the configured language server, decode
the resulting text edits into byte spans, and apply them through the privileged
`workspace/applyEdits` host capability. The `lsp` sidecar never writes workspace
files directly. Both `changes` and text-only `documentChanges` WorkspaceEdit
payloads are accepted, including versioned `TextDocumentEdit` entries; file
create, rename, and delete resource operations are rejected before any file is
changed.

| Field       | Type    | Required | Description                                   |
|-------------|---------|----------|-----------------------------------------------|
| `path`      | string  | yes      | Workspace-relative path of the file           |
| `line`      | integer | yes      | Zero-based line number                        |
| `character` | integer | yes      | Zero-based UTF-16 code-unit column            |
| `new_name`  | string  | yes      | Replacement symbol name                       |
| `dry_run`   | boolean | no       | Return preview data without mutating files    |

Before issuing the rename, Tower uses `prepareRename` when the server supports
it. A non-renameable position returns a structured tool payload instead of
partially editing files.

**Returns (applied rename)**

```json
{
  "applied": true,
  "files_changed": 1,
  "spans": [
    {
      "path": "src/lib.rs",
      "start_byte": 3,
      "end_byte": 11,
      "replacement": "new_name",
      "base_hash": "..."
    }
  ],
  "preview": null,
  "per_file": [
    {
      "path": "src/lib.rs",
      "applied": true,
      "edits_applied": 1,
      "edits_skipped": 0,
      "new_version": "..."
    }
  ]
}
```

**Returns (`dry_run:true`)**

```json
{
  "spans": [
    {
      "path": "src/lib.rs",
      "start_byte": 3,
      "end_byte": 11,
      "replacement": "new_name",
      "base_hash": "..."
    }
  ],
  "preview": "fn new_name() {}\n",
  "per_file": [
    {
      "path": "src/lib.rs",
      "applied": false,
      "edits_applied": 1,
      "edits_skipped": 0,
      "preview": "fn new_name() {}\n"
    }
  ]
}
```

**Structured error results**

```json
{ "code": "not_renameable", "message": "the symbol at this position cannot be renamed" }
```

Other stable `code` values are `unsupported_workspace_edit`,
`unsupported_language`, `invalid_range`, and `backend_error`. Host-side apply
failures surface inside `per_file[*].error`, for example `cas_conflict`.

---

## Standalone lint tools

The `lint` extension (manifest `name = "lint"`) runs configured external linters on demand and
returns diagnostics using the same response vocabulary as `tower_lsp_diagnostics`. It can also
apply structured fixes through the manifest-gated `workspace/applyEdits` HostCall. Linter binaries
are not allowed to mutate workspace files directly; Tower owns CAS validation, overlap handling,
atomic writes, and index refresh.

Configure linters in `<workspace>/.tower/config.toml`:

```toml
[lint.rust]
command = "cargo"
args = ["clippy", "--message-format=json"]
extensions = ["rs"]
format = "rustc-json"
target = "none"

[lint.javascript]
command = "eslint"
args = ["--format", "json"]
timeout_secs = 10
extensions = ["js", "jsx", "ts", "tsx"]
format = "eslint-json"
target = "append"
```

`timeout_secs` is an optional positive integer in seconds and defaults to `2`. It applies to each
lint command, including fix preparation and the follow-up check after edits. Tower derives a
lint-specific host budget so this inner deadline can elapse without being pre-empted by the base
extension deadline. Invalid values are configuration errors naming
`lint.<language>.timeout_secs`.

Supported `format` values are `rustc-json`, `eslint-json`, and `generic-regex`. Supported `target`
values are:

| Value | Behavior |
|-------|----------|
| `append` | Append the target path to the configured command arguments |
| `stdin` | Read the target file through Tower and pass its content to command stdin |
| `none` | Run the command without a per-file path; the linter output must identify files |

For `generic-regex`, set `regex` to a pattern with named capture groups. Required groups are
`file`, `line`, `col`, and `message`; optional groups include `endLine`, `endCol`, `severity`, and
`code`. `source` may be set to override the diagnostic source string.

### tower_lint_check

Run configured standalone linters for one file or for every indexed file with a matching
configuration.

| Field  | Type   | Required | Description                                                |
|--------|--------|----------|------------------------------------------------------------|
| `path` | string | no       | Workspace-relative file path; omit to lint supported files |

**Single-file request**

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "method": "tools/call",
  "params": {
    "name": "tower_lint_check",
    "arguments": { "path": "src/main.rs" }
  }
}
```

**Workspace request**

```json
{
  "jsonrpc": "2.0",
  "id": 31,
  "method": "tools/call",
  "params": {
    "name": "tower_lint_check",
    "arguments": {}
  }
}
```

**Returns**

```json
{
  "supported": true,
  "diagnostics": [
    {
      "path": "src/main.rs",
      "line": 9,
      "character": 14,
      "endLine": 9,
      "endCharacter": 17,
      "severity": "error",
      "message": "cannot find value `foo`",
      "source": "rustc",
      "code": "E0425"
    }
  ]
}
```

`line`, `character`, `endLine`, and `endCharacter` are zero-based. `severity` is one of `error`,
`warning`, `info`, or `hint`. Workspace lint results are sorted deterministically by path and then
position.

A file with no matching `[lint.<language>]` configuration returns a successful unsupported result:

```json
{ "supported": false, "diagnostics": [] }
```

Runner failures are also returned as successful tool results with a stable lint error object, so MCP
clients can branch without treating expected lint-runner failures as transport failures:

```json
{
  "supported": false,
  "diagnostics": [],
  "error": {
    "code": "lint_missing_binary",
    "message": "lint command is unavailable"
  }
}
```

Stable lint error codes are `lint_missing_binary`, `lint_unparseable_output`, `lint_nonzero_exit`,
`lint_timeout`, and `lint_invalid_config`.

When a lint command reaches its configured deadline, Tower terminates and reaps the command's
process tree and returns `lint_timeout` in the successful tool payload. The timeout is not a JSON-RPC
transport error.

### tower_lint_fix

Apply or preview structured fixes emitted by configured linters. `rustc-json` fixes are extracted
from suggestion spans; `MachineApplicable` suggestions are applied by default, while
`MaybeIncorrect` or unknown applicability is skipped unless `unsafe` is true. `eslint-json` fixes
are extracted from `messages[].fix.range` and `messages[].fix.text`. `generic-regex` and fix-less
diagnostics are reported as skipped unsupported fixes, not tool failures.

| Field     | Type    | Required | Description                                                     |
|-----------|---------|----------|-----------------------------------------------------------------|
| `path`    | string  | no       | Workspace-relative file path; omit to inspect supported files   |
| `unsafe`  | boolean | no       | Apply fixes with unsafe or unknown applicability                |
| `dry_run` | boolean | no       | Return previews without writing files                           |

**Dry-run request**

```json
{
  "jsonrpc": "2.0",
  "id": 32,
  "method": "tools/call",
  "params": {
    "name": "tower_lint_fix",
    "arguments": { "path": "src/main.rs", "dry_run": true }
  }
}
```

**Apply request**

```json
{
  "jsonrpc": "2.0",
  "id": 33,
  "method": "tools/call",
  "params": {
    "name": "tower_lint_fix",
    "arguments": { "path": "src/main.rs" }
  }
}
```

**Returns**

```json
{
  "files_changed": 1,
  "fixes_applied": 1,
  "fixes_skipped": [
    {
      "path": "src/main.rs",
      "reason": "unsafe",
      "diagnostic": {
        "path": "src/main.rs",
        "line": 9,
        "character": 14,
        "endLine": 9,
        "endCharacter": 17,
        "severity": "warning",
        "message": "suggestion is not machine-applicable",
        "source": "rustc"
      },
      "supported_fix": true
    }
  ],
  "remaining_diagnostics": [],
  "previews": []
}
```

`files_changed` counts distinct files for which Tower committed a non-dry-run edit. `fixes_applied`
counts accepted fix records. `fixes_skipped` uses stable reasons: `conflict`, `unsafe`,
`unsupported`, `cas_conflict`, and `invalid_range`. `remaining_diagnostics` is produced by one
follow-up lint check after successful writes; `tower_lint_fix` does not run an iterative fix loop.

Fix preparation timeout returns `lint_fix_unavailable` without changing files. If the follow-up
check times out after edits were committed, the result uses `lint_fix_apply_failed`, retains the
timeout cause, and reports that edits were applied rather than returning an empty successful
diagnostic list.

Dry-run responses keep `files_changed` at `0` and include preview entries:

```json
{
  "files_changed": 0,
  "fixes_applied": 1,
  "fixes_skipped": [],
  "remaining_diagnostics": [],
  "previews": [
    {
      "path": "src/main.rs",
      "edits": [
        { "start_byte": 6, "end_byte": 10, "replacement": "fixed" }
      ],
      "preview_content": "let fixed = true;\n"
    }
  ]
}
```

Fix-tool failures return a stable error object in the tool payload rather than a JSON-RPC transport
error when the request reached the extension:

```json
{
  "error": {
    "code": "lint_fix_apply_failed",
    "message": "failed to apply lint fixes"
  }
}
```

Stable fix error codes are `lint_fix_unavailable`, `lint_fix_apply_failed`, and
`lint_fix_invalid_request`.

---

## Debug tools

The `debug` extension (manifest `name = "debug"`) bridges configured Debug Adapter Protocol
adapters, provides a stateless one-shot eval-at probe, and can expose rr-backed record/replay
reverse debugging. Its live-session tools are lazy extension tools and appear only when all of the
following are true:

- the bundled debug sidecar binary is available next to `tower`, or a `debug` extension is discovered
  from an extension scope;
- the workspace has at least one valid `[debug.<language>]` entry in `.tower/config.toml`;
- the `debug` extension is not listed in `[extensions].disabled`.

Configure one language entry per adapter:

```toml
[debug.rust]
extensions = ["rs"]
command = "lldb-dap"
args = ["--stdio"]
adapter_type = "lldb"
launch = { request = "launch", program = "target/debug/app" }
default_timeout_secs = 15
idle_ttl_secs = 300
```

Enable rr-backed record/replay tools with an explicit record section:

```toml
[debug.record]
backend = "rr"
trace_dir = ".tower/traces"
ttl_secs = 86400
max_traces = 20
record_timeout_secs = 60
```

`trace_dir` is workspace-relative and defaults to `.tower/traces`. `ttl_secs`,
`max_traces`, and `record_timeout_secs` must be positive when present; the defaults are no TTL,
`20` retained traces, and `60` seconds.

Record/replay tools are omitted from `tools/list` unless `[debug.record] backend = "rr"` is present.
rr itself is preflighted at record time, so unsupported hosts return a structured payload instead of
crashing the extension.

The debug extension declares no workspace mutation capabilities. Sessions are ephemeral and owned by
the sidecar; terminate, disconnect, shutdown, quarantine, and idle TTL expiry clean up the adapter and
debuggee process tree.

### Runtime result shape

Expected runtime failures are successful tool payloads with stable error codes, not JSON-RPC transport
errors:

```json
{
  "ok": false,
  "error": {
    "code": "session-not-found",
    "message": "debug session missing",
    "data": null
  }
}
```

Stable debug error codes include `session-not-found`, `not-stopped`, `debug-timeout`,
`adapter-exited`, `launch-failed`, and `reverse_unsupported`. rr record failures use stable payload
codes such as `rr_unsupported`, `record_timeout`, and `record_failed`. Origin-finding failures use
stable reasons such as `no_prior_write_reached`, `trace_not_found`, `watch_evaluation_failed`,
`replay_open_failed`, `origin_timeout`, `record_failed`, and `capture_failed`. Malformed tool parameters still return
protocol-level invalid-params errors.

### tower_debug_launch

Launch a configured adapter session.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `language` | string | yes | Configured `[debug.<language>]` key |
| `program` | string | yes | Program path or adapter-specific program value |
| `cwd` | string or null | no | Working directory passed to the adapter |
| `args` | string array | no | Program arguments |
| `env` | object | no | Environment variables passed to launch |
| `launch_overrides` | object | yes | Adapter-specific launch arguments merged over configured defaults; use `{}` when no override is needed |

**Returns**

```json
{
  "session_id": "debug-1",
  "state": "stopped",
  "stop": {
    "state": "stopped",
    "reason": "entry",
    "thread_id": 1,
    "top_frame": { "id": 7, "name": "main", "path": "src/main.rs", "line": 1, "column": 1 },
    "hit_breakpoint_ids": [],
    "timed_out": false,
    "output_since": []
  }
}
```

### tower_debug_set_breakpoints

Replace the session's breakpoints for one source file.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |
| `path` | string | yes | Workspace-relative source path |
| `breakpoints` | array | yes | Breakpoints with `line`, optional `condition`, optional `hit_condition` |

**Returns** `{"breakpoints": [{"path": string, "line": uint, "condition": string|null, "hit_condition": string|null, "verified": bool, "verified_id": uint|null}]}`

### tower_debug_continue

Resume a session until it stops, terminates, or times out.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |
| `thread_id` | integer or null | no | Adapter thread to resume; omit for adapter default |
| `timeout_secs` | integer or null | no | Override the configured `default_timeout_secs` for this call |

**Returns** a stop object with `state`, `reason`, `thread_id`, `top_frame`, `hit_breakpoint_ids`,
`timed_out`, and `output_since`. A timeout returns `state:"running"` with `timed_out:true`; the
session remains available for `tower_debug_pause` or cleanup.

### tower_debug_step

Step a session until it stops, terminates, or times out.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |
| `thread_id` | integer or null | no | Adapter thread to step |
| `timeout_secs` | integer or null | no | Override the configured `default_timeout_secs` for this call |

**Returns** the same stop object as `tower_debug_continue`.

### tower_debug_pause

Pause a running session.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |
| `thread_id` | integer or null | no | Adapter thread to pause |

**Returns** the same stop object as `tower_debug_continue`.

### tower_debug_threads

List session threads.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |

**Returns** `{"threads": [{"id": uint, "name": string}]}`

### tower_debug_stack

Read stack frames for a stopped thread.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |
| `thread_id` | integer | yes | Thread id returned by `tower_debug_threads` or a stop result |

**Returns** `{"frames": [{"id": uint, "name": string, "path": string|null, "line": uint, "column": uint}]}`

### tower_debug_variables

Read variables from a DAP `variables_reference`.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |
| `variables_reference` | integer | yes | Adapter variable reference returned by prior debug data |

**Returns** `{"variables": [{"name": string, "value": string, "type": string|null, "variables_reference": uint}]}`

### tower_debug_evaluate

Evaluate an expression in a stopped frame.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |
| `frame_id` | integer | yes | Frame id returned by `tower_debug_stack` |
| `expression` | string | yes | Expression passed to the adapter |

**Returns** `{"result": {"name": string, "value": string, "type": string|null, "variables_reference": uint}}`

### tower_debug_eval_at

Run a stateless one-shot debug probe. The tool launches a configured program, sets one optional
breakpoint, continues until stop, exit, timeout, termination, or adapter exit, captures requested
evidence, and always tears down the internal session before returning. It does not expose a
`session_id`.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `lang` | string | yes | Configured `[debug.<language>]` key |
| `program` | string | yes | Program path or adapter-specific program value; the program must already be built |
| `args` | string array | no | Program arguments; defaults to `[]` |
| `cwd` | string or null | no | Working directory passed to the adapter |
| `env` | string map | no | Environment variables passed to launch; defaults to `{}` |
| `breakpoint` | object or null | no | Optional single breakpoint with `path`, `line`, and optional `condition` |
| `expressions` | string array | no | Expressions to evaluate at each captured hit; defaults to `[]` |
| `capture` | object | no | Booleans `stack`, `locals`, and `args`; each defaults to `true` |
| `on_hit` | `"first"` or `"all"` | no | Capture only the first hit or continue until `max_hits`; defaults to `"first"` |
| `max_hits` | integer | no | Positive hit bound; defaults to `1` |
| `max_depth` | integer | no | Recursive variable expansion depth; defaults to `2` |
| `max_children` | integer | no | Maximum expanded children per node or scope; defaults to `50` |
| `timeout_ms` | integer or null | no | Probe continue timeout in milliseconds; omit to use the configured default |

`breakpoint` shape:

```json
{ "path": "src/main.rs", "line": 42, "condition": "answer == 42" }
```

**Returns**

```json
{
  "hit": true,
  "hits": [
    {
      "thread_id": 1,
      "frame": { "id": 7, "name": "main", "path": "src/main.rs", "line": 42, "column": 1 },
      "stack": [
        { "id": 7, "name": "main", "path": "src/main.rs", "line": 42, "column": 1 }
      ],
      "locals": [
        {
          "name": "answer",
          "value": "42",
          "type": "i32",
          "children": [],
          "truncated": false
        }
      ],
      "args": [],
      "evaluated": {
        "answer": { "value": "42", "type": "i32" },
        "missing": { "error": "evaluation failed" }
      }
    }
  ],
  "output": [],
  "finished": "stopped",
  "exit_code": null,
  "condition_unsupported": null
}
```

`finished` is one of `"stopped"`, `"exited"`, `"timeout"`, `"terminated"`, or
`"adapter_exited"`. A no-hit normal exit returns `hit:false`, `finished:"exited"`, optional
`exit_code`, captured `output`, and an empty `hits` array. A timeout returns
`finished:"timeout"` after cleanup. If a requested breakpoint condition cannot be honored, the result
sets `condition_unsupported:true` instead of silently ignoring the condition.

### tower_debug_record

Record one native execution through rr and register the resulting trace. This tool is present only
when `[debug.record] backend = "rr"` is configured.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `language` | string | yes | Configured `[debug.<language>]` key |
| `program` | string | yes | Program path to run under `rr record`; the program must already be built |
| `args` | string array | no | Program arguments; defaults to `[]` |
| `cwd` | string or null | no | Working directory for the recorded process |
| `env` | string map | no | Environment variables for the recorded process; defaults to `{}` |
| `timeout_ms` | integer or null | no | Record timeout in milliseconds; omit to use `debug.record.record_timeout_secs` |

**Returns** `{"recordable": bool, "reason": string|null, "trace_id": string|null, "trace": object|null, "exit_code": integer|null, "output": array, "output_truncated": bool, "error": object|null}`.

Unsupported rr environments return `recordable:false`, `reason:"rr_unsupported"`, and
`error.data.unsupported_reason` such as `rr_missing`, `non_linux_host`,
`unsupported_cpu`, `unsupported_perf_counters`, or `rr_unsupported`. Record output is bounded across
stdout and stderr; `output_truncated:true` signals that the cap was reached.

### tower_debug_replay

Open a replay-backed debug session for a recorded trace. Replay sessions reuse the normal session
tools (`tower_debug_continue`, `tower_debug_step`, `tower_debug_threads`, `tower_debug_stack`,
`tower_debug_variables`, `tower_debug_evaluate`, `tower_debug_terminate`, and
`tower_debug_disconnect`) and advertise reverse execution support.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `trace_id` | string | yes | Trace returned by `tower_debug_record` or `tower_debug_traces` |
| `language` | string | yes | Configured `[debug.<language>]` key |
| `timeout_secs` | integer or null | no | Override the configured debug timeout for replay startup |

**Returns** `{"session_id": string, "trace_id": string, "state": "stopped", "stop": object|null, "supportsStepBack": bool}`.

### tower_debug_reverse_continue

Resume a replay session backward until the previous matching stop, breakpoint, watchpoint, start of
trace, or timeout.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Replay session returned by `tower_debug_replay` |
| `thread_id` | integer or null | no | Adapter thread to reverse-continue |
| `timeout_secs` | integer or null | no | Override the configured timeout for this call |

**Returns** the same stop object as `tower_debug_continue`. Live non-replay sessions return a
structured `reverse_unsupported` payload.

### tower_debug_step_back

Step a replay session backward.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Replay session returned by `tower_debug_replay` |
| `thread_id` | integer or null | no | Adapter thread to step backward |
| `granularity` | `"line"`, `"instruction"`, or `"over"` | no | Backward step granularity; defaults to `"line"` |
| `timeout_secs` | integer or null | no | Override the configured timeout for this call |

**Returns** the same stop object as `tower_debug_continue`. Live non-replay sessions return a
structured `reverse_unsupported` payload.

### tower_debug_watchpoint

Set a replay-session watchpoint by expression or address.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Replay session returned by `tower_debug_replay` |
| `expression` | string or null | no | Expression to watch, for example a variable name |
| `address` | string or null | no | Address to watch when expression-based watchpoints are not desired |
| `kind` | `"write"`, `"read"`, or `"access"` | yes | Watchpoint access kind |
| `enabled` | boolean or null | no | Defaults to `true` |
| `timeout_secs` | integer or null | no | Reserved timeout override for adapters that need it |

**Returns** `{"ok": bool, "watchpoint": {"watchpoint_id": string, "expression": string|null, "address": string|null, "kind": string, "enabled": bool, "verified": bool}|null, "error": object|null}`.

### tower_debug_traces

List recorded traces known to the debug sidecar.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| _(none)_ | — | — | This tool takes no arguments |

**Returns** `{"traces": [{"trace_id": string, "path": string, "created_unix_secs": uint, "program": string, "args_summary": array, "exit_code": integer|null, "output_summary": array, "output_truncated": bool, "expires_unix_secs": uint|null, "ttl_secs": uint|null, "prune_generation": uint}]}`.

### tower_debug_delete_trace

Delete one recorded trace by id. Deletion is contained to the configured trace root.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `trace_id` | string | yes | Trace id returned by `tower_debug_record` or `tower_debug_traces` |

**Returns** `{"deleted": bool, "trace_id": string, "error": object|null}`.

### tower_debug_find_origin

Open replay, seek to a target point, set a write watchpoint for a watched value, reverse-continue to
the last write, capture bounded evidence, and clean up the replay session before returning.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `trace_id` | string | yes | Trace to inspect |
| `language` | string | yes | Configured `[debug.<language>]` key |
| `at` | object | yes | Target point: `{"kind":"crash"}`, `{"kind":"end"}`, or `{"kind":"source","path":"src/main.rs","line":42,"column":1}` |
| `watch` | string | yes | Watched expression or value location |
| `timeout_secs` | integer or null | no | Timeout for replay seek and reverse operations |
| `max_depth` | integer or null | no | Recursive variable expansion depth |
| `max_children` | integer or null | no | Maximum expanded children per node or scope |

**Returns** `{"found": bool, "reason": string|null, "trace_id": string|null, "write_frame": object|null, "stack": array, "value": object|null, "locals": array, "args": array, "output": array, "truncated": bool, "error": object|null}`.

When reverse execution reaches the start of the trace without finding a prior write, the result has
`found:false` and `reason:"no_prior_write_reached"` rather than a transport error.

### tower_debug_record_and_find_origin

Record a program and immediately run the origin-finding recipe against the new trace.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `record` | object | yes | Same shape as `tower_debug_record` arguments |
| `origin` | object | yes | Origin request without `trace_id`: `language`, `at`, `watch`, `timeout_secs`, `max_depth`, and `max_children` |

**Returns** `{"record": <tower_debug_record result>, "origin": <tower_debug_find_origin result>|null}`. If recording is unsupported, `origin` is `null`; if recording succeeds but origin search fails, both the trace record result and origin failure are returned.

### tower_debug_terminate

Terminate the debuggee and adapter process tree, then remove the session.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |

**Returns** `{"ok": true}`

### tower_debug_disconnect

Ask the adapter to disconnect, then remove the session.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | string | yes | Session returned by `tower_debug_launch` |

**Returns** `{"ok": true}`

### tower_debug_sessions

List live ephemeral sessions.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| _(none)_ | — | — | This tool takes no arguments |

**Returns** `{"sessions": [{"session_id": string, "language": string, "state": "initializing"|"stopped"|"running"|"terminated", "last_stop": object|null}]}`

See [debug sessions and probes](user-guide/debug-sessions.md) for task-oriented workflows.

---

## Extension tools and namespacing

Extension tools are surfaced through the same `tools/list` / `tools/call` interface
as native tools, with one invariant: **extension tool names are always namespaced as
`"tower_<ext>_<tool_name>"`**.

- An extension with manifest `name = "ast"` that declares `get_outline` appears
  in `tools/list` as `"tower_ast_get_outline"`.
- Native tools carry the bare `tower_` prefix (`tower_find_file`, etc.). To
  guarantee extension tools never collide with native ones, **an extension name must
  not begin with `tower`** — that prefix is reserved for host tools.
- The namespacing is unconditional: there is no code path that allows an extension
  to claim an un-namespaced name.

Extension tools are dispatched through `ExtensionMergedRegistry`, which routes the
call to the owning extension via `ExtensionHostPort::invoke_extension`, preserving
the extension identity and requiring the tool to be declared by that extension. An extension fault
(timeout, crash, protocol error, or quarantine) maps to `-32603` and does not affect
the host process or the MCP link. (Previously these were WASM plugin tools dispatched
to an in-process sandbox; they are now out-of-process native sidecars — see
[extensions.md](extensions.md). The tool names and wire shape are unchanged.)

---

## AST extension tools

The `ast` extension (manifest `name = "ast"`) provides Tree-sitter-based
structural analysis for Rust, Go, and PHP. It also contributes AST-anchored write
tools that resolve exactly one symbol and request host-applied edits through
`workspace/applyEdits`. Its native sidecar binary must be discovered from an
extension scope for the tools to appear.

The declared `tower_ast_plan_safe_delete_declaration` planning tool accepts `path`,
`symbol_name`, and optional `kind`. It returns a `planned` declaration with its
workspace-relative path, symbol identity, UTF-16 position, deletion byte span, and
base content hash, or `not_found`, `ambiguous_symbol`, or `invalid_declaration`.
It reads fresh content and never applies edits. The native
[`tower_safe_delete_symbol`](#tower_safe_delete_symbol) composes this declaration
plan with semantic reference analysis and host preflight before applying changes.

Supported languages (detected by file extension or language ID):

| Extension / ID           | Language |
|--------------------------|----------|
| `.rs`, `rust`, `RUST`    | Rust     |
| `.go`, `go`              | Go       |
| `.php`, `php`            | PHP      |

Any other extension or language ID returns `{"unsupported": true, "language": "<hint>"}`.

### tower_ast_get_outline

Return the structural skeleton of a source file: functions, structs, enums,
traits, impl blocks, methods, modules, type aliases, constants, statics, macro
definitions, and PHP classes.

MCP name: `"tower_ast_get_outline"`

| Field  | Type   | Required | Description                            |
|--------|--------|----------|----------------------------------------|
| `path` | string | yes      | Workspace-relative path to source file |

**Returns (supported language)**

```json
{
  "items": [
    {
      "kind": "struct",
      "name": "MyStruct",
      "start_byte": 42,
      "end_byte": 87,
      "start_row": 3,
      "start_col": 0,
      "end_row": 5,
      "end_col": 1
    },
    {
      "kind": "impl",
      "name": "MyStruct",
      "start_byte": 89,
      "end_byte": 180,
      "start_row": 7,
      "start_col": 0,
      "end_row": 12,
      "end_col": 1
    },
    {
      "kind": "method",
      "name": "new",
      "start_byte": 110,
      "end_byte": 160,
      "start_row": 8,
      "start_col": 4,
      "end_row": 11,
      "end_col": 5
    }
  ]
}
```

All span fields are byte offsets and zero-based row/column positions into the
source file. Names are capped at 256 bytes. Anonymous items (some `impl` blocks
where the type cannot be resolved) use `"<anonymous>"`.

`impl Trait for Type` appears as `{"kind": "impl", "name": "Trait for Type"}`.
`impl !Send for Foo` appears as `{"kind": "impl", "name": "!Send for Foo"}`.

**Returns (unsupported language)**

```json
{ "unsupported": true, "language": "foo.py" }
```

**Kind values that may appear in `items`**

`function`, `struct`, `enum`, `trait`, `impl`, `method`, `module`,
`type_alias`, `const`, `static`, `macro_def`, `class`

Note: `class` is emitted for PHP class declarations (not `struct`), ensuring
round-trip consistency with `find_symbols kind=class`.

**Request example**

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "tools/call",
  "params": {
    "name": "tower_ast_get_outline",
    "arguments": { "path": "src/lib.rs" }
  }
}
```

---

### tower_ast_find_symbols

Find precise definition locations for a named symbol of a given kind. Uses the
error-tolerant Tree-sitter parse tree to exclude false positives in comments and
string literals.

MCP name: `"tower_ast_find_symbols"`

| Field         | Type   | Required | Description                                          |
|---------------|--------|----------|------------------------------------------------------|
| `path`        | string | yes      | Workspace-relative path to source file               |
| `symbol_name` | string | yes      | Name of the symbol to search for                     |
| `kind`        | string | yes      | Symbol kind (see table below)                        |

**Valid `kind` values**

`function`, `struct`, `enum`, `trait`, `impl`, `method`, `module`,
`type_alias`, `const`, `static`, `macro_def`, `class`

An unrecognised `kind` string returns `-32602 InvalidParams`.

**Language applicability**

Not every kind is valid for every language. Sending a kind that cannot exist in
the target language returns `{"matches": []}` (not an error), preserving the
"kind not applicable" contract.

| Kind        | Rust | Go | PHP |
|-------------|------|----|-----|
| function    | yes  | yes| yes |
| struct      | yes  | yes| no  |
| enum        | yes  | no | no  |
| trait       | yes  | yes| yes |
| impl        | yes  | no | no  |
| method      | yes  | yes| yes |
| module      | yes  | no | no  |
| type_alias  | yes  | yes| no  |
| const       | yes  | yes| no  |
| static      | yes  | no | no  |
| macro_def   | yes  | no | no  |
| class       | no   | no | yes |

**Returns (symbol found)**

```json
{
  "matches": [
    {
      "kind": "function",
      "name": "parse",
      "start_byte": 103,
      "end_byte": 241,
      "start_row": 7,
      "start_col": 0,
      "end_row": 14,
      "end_col": 1
    }
  ]
}
```

**Returns (kind not applicable to language)**

```json
{ "matches": [] }
```

**Returns (unsupported language)**

```json
{ "unsupported": true, "language": "main.ts" }
```

**Request example**

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "tools/call",
  "params": {
    "name": "tower_ast_find_symbols",
    "arguments": {
      "path": "src/lib.rs",
      "symbol_name": "parse",
      "kind": "function"
    }
  }
}
```

---

### tower_ast_search_symbols

Search the **cross-file** in-memory symbol index for symbols matching a name and
optional kind. Unlike `tower_ast_find_symbols` (which parses one named file), this
queries the workspace-wide index that the `ast` extension builds incrementally as
files are indexed (index-as-you-go) and keeps current via `fileIndexed` /
`fileChanged` events. Returns matches across **all** indexed files, each with its
`path`.

MCP name: `"tower_ast_search_symbols"`

| Field  | Type   | Required | Description                                                      |
|--------|--------|----------|------------------------------------------------------------------|
| `name` | string | yes      | Exact symbol name to search for                                  |
| `kind` | string | no       | Optional kind filter (same kind values as `find_symbols`)        |

**Returns**

```json
{
  "matches": [
    {
      "path": "src/lib.rs",
      "kind": "function",
      "name": "my_fn",
      "start_byte": 10, "end_byte": 50,
      "start_row": 2, "start_col": 0,
      "end_row": 4, "end_col": 1
    }
  ]
}
```

Because the index is built as files are read, results reflect only files that have
been visited (or indexed via `tower_ast_reindex`). Use `tower_ast_reindex` to force
a full cold build.

---

### tower_ast_reindex

Rebuild the whole-project symbol index by enumerating every workspace file and
parsing each. Use after large external changes or on a cold cache. Takes no
arguments.

MCP name: `"tower_ast_reindex"`

| Field    | Type | Required | Description                          |
|----------|------|----------|--------------------------------------|
| _(none)_ | —    | —        | This tool takes no arguments         |

**Returns** `{"indexed_files": uint, "symbols": uint}` — the number of files
indexed and the total number of symbols discovered across them.

---

### tower_ast_read_symbol

Read **only** a named symbol's source span — not the whole file. Resolves the
symbol and returns the exact byte slice `[start_byte, end_byte)` plus `kind` and
start/end rows for each match, ordered by `start_byte`. Resolution and slicing
happen guest-side in a single MCP round-trip. Pairs with `tower_edit_range`: read
a symbol's span, then write that span. (Spec 16.)

MCP name: `"tower_ast_read_symbol"`

| Field         | Type   | Required | Description                                                       |
|---------------|--------|----------|-------------------------------------------------------------------|
| `path`        | string | yes      | Workspace-relative path to the source file                        |
| `symbol_name` | string | yes      | Name of the symbol to read                                        |
| `kind`        | string | no       | Optional kind filter to disambiguate same-named symbols           |

**Returns**

```json
{
  "matches": [
    {
      "kind": "function",
      "name": "handle",
      "start_byte": 100, "end_byte": 200,
      "start_row": 5, "end_row": 12,
      "content": "pub fn handle() {\n    ...\n}"
    }
  ]
}
```

A missing file, an unknown symbol (the error names the symbol), or a stale span
maps to `-32603`. An unrecognised `kind` returns `-32602 InvalidParams`.

### tower_ast_replace_symbol_body

Replace the body of one uniquely resolved symbol. Tower resolves the symbol span
through the AST index, computes the body-only edit span, and submits one
host-owned `workspace/applyEdits` request.

| Field         | Type   | Required | Description                                              |
|---------------|--------|----------|----------------------------------------------------------|
| `path`        | string | yes      | Workspace-relative path to the source file               |
| `symbol_name` | string | yes      | Name of the symbol to edit                               |
| `kind`        | string | no       | Optional kind filter                                     |
| `replacement` | string | yes      | Replacement text for the symbol body                     |
| `dry_run`     | bool   | no       | Return preview data without mutating files               |

**Returns**

```json
{
  "applied": true,
  "files_changed": 1,
  "span": {
    "path": "src/lib.rs",
    "start_byte": 22,
    "end_byte": 27,
    "replacement": "\n    2\n",
    "base_hash": "..."
  },
  "per_file": [
    {
      "path": "src/lib.rs",
      "applied": true,
      "edits_applied": 1,
      "edits_skipped": 0,
      "new_version": "..."
    }
  ]
}
```

For `dry_run:true`, the same shape is returned with `applied:false`,
`files_changed:0`, and `preview` / `per_file[*].preview` populated.

### tower_ast_insert_before_symbol

Insert text immediately before one uniquely resolved symbol.

Arguments are the same as `tower_ast_replace_symbol_body`.

### tower_ast_insert_after_symbol

Insert text immediately after one uniquely resolved symbol.

Arguments are the same as `tower_ast_replace_symbol_body`.

### tower_ast_delete_symbol

Delete one uniquely resolved declaration span. When the symbol owns a trailing
newline, Tower removes that newline too, but it does not chase references or call
sites elsewhere in the workspace.

| Field         | Type   | Required | Description                                |
|---------------|--------|----------|--------------------------------------------|
| `path`        | string | yes      | Workspace-relative path to the source file |
| `symbol_name` | string | yes      | Name of the symbol to delete               |
| `kind`        | string | no       | Optional kind filter                       |
| `dry_run`     | bool   | no       | Return preview data without mutating files |

All four AST write tools return the same `AnchoredSymbolEditResult` shape on
success and stable structured error payloads on resolution failures:

```json
{
  "code": "ambiguous_symbol",
  "message": "symbol name matched multiple candidates",
  "candidates": [
    {
      "path": "src/lib.rs",
      "kind": "function",
      "name": "duplicate",
      "start_byte": 8,
      "end_byte": 28,
      "start_row": 0,
      "end_row": 0
    }
  ]
}
```

Other stable `code` values are `not_found`, `unsupported_language`,
`invalid_range`, and `backend_error`.


---

## Formatting

Formatting is no longer exposed as a standalone MCP tool. In the WASM era a `fmt`
plugin contributed a `tower_fmt_format` tool; that tool was removed with the WASM
plugin system. Formatting is now a **host capability** (`workspace/requestFormat`):
an extension enqueues a format job by calling that capability (backed by the host's
`FormatQueuePort`), and the host runs the configured external formatters
asynchronously. See [extensions.md](extensions.md#capability-callbacks) for the
capability contract.

---

## Connecting an MCP client

Any process that can write to stdin and read from stdout of the tower binary can
act as an MCP client. There is no authentication, no TLS, and no HTTP — it is
a direct child-process pipe.

**Minimal shell session** (for manual testing)

```bash
# Start tower, pipe manually
cargo run -p core_engine &
PID=$!

# Send initialize
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | ...
```

Because tower wraps `stdin().lock()` and `stdout().lock()` for the duration of
the serve loop, the simplest integration is to spawn it as a child process and
communicate over its stdio pipes. Here is a minimal example using Python:

```python
import subprocess, json

proc = subprocess.Popen(
    ["./tower", "--workspace-dir", "/path/to/project"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
)

def call(req):
    proc.stdin.write(json.dumps(req).encode() + b"\n")
    proc.stdin.flush()
    return json.loads(proc.stdout.readline())

# Handshake
call({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})

# List tools
tools = call({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})

# Find a file
result = call({
    "jsonrpc": "2.0",
    "id": 3,
    "method": "tools/call",
    "params": {"name": "tower_find_file", "arguments": {"query": "main"}}
})
# result["result"]["content"][0]["text"] is a JSON string — parse it again:
payload = json.loads(result["result"]["content"][0]["text"])
print(payload["paths"])  # ["src/main.rs", ...]
```

**Important**: the `text` field inside `content[0]` is the tool result encoded
as a JSON string. Always parse it a second time to get the structured object.

---

## Behaviour guarantees

- **MCP link survives extension faults.** An extension timeout, crash, protocol
  error, or quarantine maps to `-32603` and the serve loop continues.
- **`-32002` is stable.** Clients may branch on this code to detect "resource
  not found" without parsing the error message string.
- **`tools/list` is always fresh.** There is no cache; adding or removing an
  extension is reflected in the next `tools/list` call.
- **Empty lines are silently skipped.** Lines that are blank after trimming
  produce no response.
- **Notifications produce no response.** Requests with no `id` field are
  accepted but never replied to.
