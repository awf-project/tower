# Type Hierarchy

Use `tower_lsp_type_hierarchy` to inspect inheritance relationships reported by a configured
language server. The query is read-only and returns supertypes, subtypes, or both for a symbol at a
source position.

## Configure a language server

Add a binding under `.tower/config.toml` for the file extension you want to inspect:

```toml
[lsp.rust]
command = "rust-analyzer"
args = []
extensions = ["rs"]
```

The language server must support the LSP type-hierarchy methods. Restart the Tower daemon after
changing its configuration.

## Query a hierarchy

Call the namespaced MCP tool with a workspace-relative path. `line` and `character` are zero-based;
`character` is a UTF-16 code-unit offset, matching LSP:

```json
{
  "name": "tower_lsp_type_hierarchy",
  "arguments": {
    "path": "src/lib.rs",
    "line": 12,
    "character": 7,
    "direction": "both"
  }
}
```

Use `supertypes` to find parent types, `subtypes` to find derived types, or `both` to receive both
sets in separate arrays.

## Interpret the result

A supported response has this shape:

```json
{
  "supported": true,
  "supertypes": [],
  "subtypes": []
}
```

Each item preserves the language server's name, kind, optional detail, source `range`, and
`selectionRange`. Items inside the workspace use a relative `path` and `external:false`:

```json
{
  "name": "Derived",
  "kind": "class",
  "detail": null,
  "range": { "line": 20, "character": 6, "endLine": 20, "endCharacter": 13 },
  "selectionRange": {
    "line": 20,
    "character": 6,
    "endLine": 20,
    "endCharacter": 13
  },
  "path": "src/derived.rs",
  "externalUri": null,
  "external": false
}
```

Results outside the workspace are retained so dependency and platform relationships remain visible,
but they are explicitly read-only:

```json
{
  "path": null,
  "externalUri": "file:///external/library.rs",
  "external": true
}
```

Do not pass an `externalUri` to a workspace mutation tool. Tower never treats external hierarchy
results as writable workspace paths.

## Handle unsupported servers

If no language server is configured, or the server does not advertise type hierarchy, Tower returns a
successful response with `supported:false` and empty arrays. Check `supported` before consuming the
arrays, then fall back to AST navigation or text search when appropriate.

Malformed arguments return a JSON-RPC error. A backend failure is reported using the standard LSP
read-only outcome envelope; it does not modify workspace files.

## Related pages

- [MCP tool reference](../mcp-tools.md#tower_lsp_type_hierarchy)
- [Semantic edits](semantic-edits.md)
- [Workspace configuration](configuration.md)
