# Semantic Edits

Use Tower's semantic edit tools when you want the sidecars to find the right
symbol or reference set, but you still want the host to own validation, compare-
and-swap checks, atomic writes, and index refresh.

F007 adds two workflows:

- `tower_lsp_rename` for language-server-backed rename across one or more files
- `tower_ast_*` anchored symbol edits for exact one-symbol body replacement,
  insert-before, insert-after, and delete

## Before You Start

For rename and implementation lookup, configure a language server in
`<workspace>/.tower/config.toml`:

```toml
[lsp.rust]
command = "rust-analyzer"
args = []
extensions = ["rs"]
```

For AST edits, build or install the `ast` extension and make sure the file has
been indexed. If you changed many files outside Tower, run `tower_ast_reindex`
before relying on symbol lookup.

## Preview a Rename

Start with `dry_run:true`. Rename dry-runs compute the same spans as a real
rename, but the host returns preview data and does not rewrite files.

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "tools/call",
  "params": {
    "name": "tower_lsp_rename",
    "arguments": {
      "path": "src/lib.rs",
      "line": 0,
      "character": 3,
      "new_name": "new_name",
      "dry_run": true
    }
  }
}
```

Typical preview:

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

If the position is not renameable, the tool returns a structured payload such as:

```json
{ "code": "not_renameable", "message": "the symbol at this position cannot be renamed" }
```

## Apply a Rename

Omit `dry_run` to apply the rename through one host-owned `workspace/applyEdits`
request:

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "tools/call",
  "params": {
    "name": "tower_lsp_rename",
    "arguments": {
      "path": "src/lib.rs",
      "line": 0,
      "character": 3,
      "new_name": "new_name"
    }
  }
}
```

Inspect `per_file` even on successful calls. It carries the final per-file apply
status, optional `new_version`, and any host-side error such as `cas_conflict`.

## Preview an AST-Anchored Edit

AST edits are useful when you know the symbol name and want one precise
declaration change without asking the language server to compute references.

Example: insert text immediately before one function:

```json
{
  "jsonrpc": "2.0",
  "id": 22,
  "method": "tools/call",
  "params": {
    "name": "tower_ast_insert_before_symbol",
    "arguments": {
      "path": "src/lib.rs",
      "symbol_name": "target",
      "kind": "function",
      "replacement": "// preview\n",
      "dry_run": true
    }
  }
}
```

Typical result:

```json
{
  "applied": false,
  "files_changed": 0,
  "span": {
    "path": "src/lib.rs",
    "start_byte": 0,
    "end_byte": 0,
    "replacement": "// preview\n",
    "base_hash": "..."
  },
  "preview": "// preview\npub fn target() {}\n",
  "per_file": [
    {
      "path": "src/lib.rs",
      "applied": false,
      "edits_applied": 1,
      "edits_skipped": 0,
      "preview": "// preview\npub fn target() {}\n"
    }
  ]
}
```

The four AST write tools are:

- `tower_ast_replace_symbol_body`
- `tower_ast_insert_before_symbol`
- `tower_ast_insert_after_symbol`
- `tower_ast_delete_symbol`

## Handle Resolution Failures

AST edits require exactly one resolved symbol. Two common payload errors are:

```json
{ "code": "not_found", "message": "no matching symbol found" }
```

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

Use `kind` whenever the same symbol name appears in multiple declaration kinds or
scopes.

## Choose the Right Tool

- Use `tower_lsp_rename` when you want reference-aware renaming across files.
- Use `tower_ast_replace_symbol_body` when you want to rewrite one declaration's
  body and leave surrounding bytes unchanged.
- Use `tower_ast_insert_before_symbol` or `tower_ast_insert_after_symbol` when
  you want structural insertion without hand-computing offsets.
- Use `tower_ast_delete_symbol` when you want declaration-bounded deletion only.
  It does not remove references or call sites.

## Related Pages

- [`../mcp-tools.md`](../mcp-tools.md) for the full wire contracts
- [`../extensions.md`](../extensions.md) for the capability model
- [`../getting-started.md`](../getting-started.md) for initial setup
