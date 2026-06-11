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

Enumerate every available tool. The response lists all 7 native `vfs_*` tools
plus any namespaced plugin tools (e.g. `"ast/ast_get_outline"`) that are loaded.

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
        "name": "vfs_find_file",
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
        "name": "ast/ast_get_outline",
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
| -32603  | InternalError    | Tool execution failed at runtime (I/O error, plugin fault, etc.)        |
| -32001  | ToolNotFound     | The named tool does not exist in the registry                           |
| -32002  | ResourceNotFound | The workspace entity targeted by the tool was not found (stable code — clients may branch on this without parsing the message) |

Codes `-32001` and `-32002` are in the JSON-RPC 2.0 server-defined range
(`-32000` to `-32099`). `-32001` is for an unknown **tool name** inside
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

## Native VFS tools

Seven tools are always available, regardless of which plugins are loaded. Their
names are undecorated (no namespace prefix).

### vfs_find_file

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
    "name": "vfs_find_file",
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

### vfs_search_text

Parallel grep across all indexed file contents.

| Field     | Type   | Required | Description                                      |
|-----------|--------|----------|--------------------------------------------------|
| `pattern` | string | yes      | Text pattern searched across all indexed files   |

**Returns** `{"matches": [{"path": string, "line_number": uint, "line_content": string}]}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "method": "tools/call",
  "params": {
    "name": "vfs_search_text",
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
  "id": 11
}
```

---

### vfs_read_file

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
  "id": 12,
  "method": "tools/call",
  "params": {
    "name": "vfs_read_file",
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
  "id": 12
}
```

**Error: file not found**

```json
{
  "jsonrpc": "2.0",
  "error": { "code": -32002, "message": "Resource not found: path or entity not found in workspace" },
  "id": 12
}
```

---

### vfs_create_file

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
  "id": 13,
  "method": "tools/call",
  "params": {
    "name": "vfs_create_file",
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
  "id": 13
}
```

---

### vfs_create_directory

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
  "id": 14,
  "method": "tools/call",
  "params": {
    "name": "vfs_create_directory",
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
  "id": 14
}
```

---

### vfs_delete_file

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
  "id": 15,
  "method": "tools/call",
  "params": {
    "name": "vfs_delete_file",
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
  "id": 15
}
```

---

### vfs_global_replace

Parallel mass find-and-replace across every indexed file. Each file is rewritten
atomically via the shadow-file pattern. Partial failures (per-file I/O errors)
are reported in `errors` without aborting the remaining files.

| Field         | Type   | Required | Description                                    |
|---------------|--------|----------|------------------------------------------------|
| `target`      | string | yes      | The string to search for                       |
| `replacement` | string | yes      | The string to substitute in every occurrence   |

**Returns** `{"files_changed": uint, "replacements": uint, "errors": [{"path": string, "reason": string}]}`

**Request**

```json
{
  "jsonrpc": "2.0",
  "id": 16,
  "method": "tools/call",
  "params": {
    "name": "vfs_global_replace",
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
  "id": 16
}
```

---

## Plugin tools and namespacing

Plugin tools are surfaced through the same `tools/list` / `tools/call` interface
as native tools, with one invariant: **plugin tool names are always namespaced as
`"<plugin_name>/<tool_name>"`**.

- A plugin with manifest `name = "ast"` that declares `ast_get_outline` appears
  in `tools/list` as `"ast/ast_get_outline"`.
- Native tools keep their plain names (`vfs_find_file`, etc.). No collision is
  possible regardless of what a plugin declares.
- The namespacing is unconditional: there is no code path that allows a plugin
  to claim an un-namespaced name.

Plugin tools are dispatched through `MergedRegistry`, which routes the call to
the appropriate `IsolatedSandbox`. A plugin fault (trap, fuel exhaustion, epoch
timeout, or quarantine) maps to `-32603` and does not affect the host process or
the MCP link.

---

## AST plugin tools

The `plugin_ast` crate (manifest `name = "ast"`) provides Tree-sitter-based
structural analysis for Rust, Go, and PHP. It must be compiled to
`wasm32-wasip1` and placed in the plugin directory for the tools to appear.

Supported languages (detected by file extension or language ID):

| Extension / ID           | Language |
|--------------------------|----------|
| `.rs`, `rust`, `RUST`    | Rust     |
| `.go`, `go`              | Go       |
| `.php`, `php`            | PHP      |

Any other extension or language ID returns `{"unsupported": true, "language": "<hint>"}`.

### ast/ast_get_outline

Return the structural skeleton of a source file: functions, structs, enums,
traits, impl blocks, methods, modules, type aliases, constants, statics, macro
definitions, and PHP classes.

MCP name: `"ast/ast_get_outline"`

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
round-trip consistency with `ast_find_symbols kind=class`.

**Request example**

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "tools/call",
  "params": {
    "name": "ast/ast_get_outline",
    "arguments": { "path": "src/lib.rs" }
  }
}
```

---

### ast/ast_find_symbols

Find precise definition locations for a named symbol of a given kind. Uses the
error-tolerant Tree-sitter parse tree to exclude false positives in comments and
string literals.

MCP name: `"ast/ast_find_symbols"`

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
    "name": "ast/ast_find_symbols",
    "arguments": {
      "path": "src/lib.rs",
      "symbol_name": "parse",
      "kind": "function"
    }
  }
}
```

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
    "params": {"name": "vfs_find_file", "arguments": {"query": "main"}}
})
# result["result"]["content"][0]["text"] is a JSON string — parse it again:
payload = json.loads(result["result"]["content"][0]["text"])
print(payload["paths"])  # ["src/main.rs", ...]
```

**Important**: the `text` field inside `content[0]` is the tool result encoded
as a JSON string. Always parse it a second time to get the structured object.

---

## Behaviour guarantees

- **MCP link survives plugin faults.** A plugin trap, fuel exhaustion, epoch
  timeout, or quarantine maps to `-32603` and the serve loop continues.
- **`-32002` is stable.** Clients may branch on this code to detect "resource
  not found" without parsing the error message string.
- **`tools/list` is always fresh.** There is no cache; adding or removing a
  plugin is reflected in the next `tools/list` call.
- **Empty lines are silently skipped.** Lines that are blank after trimming
  produce no response.
- **Notifications produce no response.** Requests with no `id` field are
  accepted but never replied to.
