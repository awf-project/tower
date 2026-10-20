# Semantic Edits

Use Tower's semantic edit tools when you want the sidecars to find the right
symbol or reference set, but you still want the host to own validation, compare-
and-swap checks, atomic writes, and index refresh.

Tower supports three refactoring workflows:

- `tower_lsp_rename` for language-server-backed rename across one or more files
- `tower_rename_path` for safe file or directory moves, with optional language-server reference updates
- `tower_ast_*` anchored symbol edits for exact one-symbol body replacement,
  insert-before, insert-after, and delete

`tower_rename_path` is host-owned: Tower validates the complete move, keeps the
filesystem and index in sync, and applies any returned text edits through the
existing CAS-guarded workspace-edit path. The sidecar never moves files itself.

## Before You Start

For rename, implementation lookup, and reference-aware safe delete, configure a language server in
`<workspace>/.tower/config.toml`:

```toml
[lsp.rust]
command = "rust-analyzer"
args = []
extensions = ["rs"]
```

Safe delete requires a backend that can resolve references. If no suitable backend is available,
Tower returns a blocked report and leaves the declaration unchanged; it never silently falls back
to span-only deletion.

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
{
  "code": "not_renameable",
  "message": "the symbol at this position cannot be renamed"
}
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

## Safe-delete a symbol

Use `tower_safe_delete_symbol` when deleting a declaration must account for references. The native
tool resolves the declaration from fresh file content, asks the configured semantic backend for
references and optional cleanup edits, and applies the complete validated plan through the host's
CAS-guarded mutation path.

```json
{
  "jsonrpc": "2.0",
  "id": 22,
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

Set `dry_run` to `true` to receive the declaration, resolved references, proposed cleanup edits,
affected files, and whether the operation is blocked without mutating contents, versions, or the
index. A live reference without complete backend cleanup blocks the operation; unresolved,
unavailable, timed-out, malformed, stale, overlapping, or workspace-external plans also fail closed.

A successful complete reference lookup also permits deleting a recursive function
when all its usages are wholly inside the declaration being removed. This needs
no backend cleanup edits. A usage elsewhere, even in the same file, still blocks
when the backend cannot provide complete cleanup; uncertain or explicitly
incomplete analysis always blocks.

If the result has `status: "previewed"`, inspect `proposed_edits` and `affected_files`, then repeat
the request with `dry_run: false` to apply it. The tool defaults `dry_run` to `false` when omitted.
A `blocked` result includes `blocking_reason`; resolve the reported references or backend issue
before retrying. A dry run does not reserve file versions, so a later apply can still report
`cas_conflict` if a target file changes.

Successful application reports `status: "applied"`. A commit-time filesystem or persistence failure
may report `status: "partial_commit"` with per-file outcomes; validation and CAS failures are
rejected before the first write. The public blocking reasons include `live_references`,
`references_unresolved`, `cleanup_incomplete`, `cas_conflict`, and `backend_error`.

Other stable blocking reasons are `not_found`, `ambiguous_symbol`, `invalid_declaration`,
`capability_unavailable`, and `invalid_cleanup`. These values are safe for agents to branch on;
backend diagnostics and external paths are not exposed in the public report.

Use `tower_ast_delete_symbol` only when declaration-bounded, span-only deletion is intentional. It
remains available for compatibility and does not remove references or call sites.

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
- Use `tower_rename_path` when you want to move a file or directory and optionally
  update imports and references to its path. See [Rename a path](path-rename.md).
- Use `tower_ast_replace_symbol_body` when you want to rewrite one declaration's
  body and leave surrounding bytes unchanged.
- Use `tower_ast_insert_before_symbol` or `tower_ast_insert_after_symbol` when
  you want structural insertion without hand-computing offsets.
- Use `tower_ast_delete_symbol` when you want declaration-bounded deletion only.
  It does not remove references or call sites.
- Use `tower_safe_delete_symbol` when deletion must be reference-aware and fail closed when the
  semantic backend cannot account for remaining usages.

## Related Pages

- [`../mcp-tools.md`](../mcp-tools.md) for the full wire contracts
- [`../extensions.md`](../extensions.md) for the capability model
- [`../getting-started.md`](../getting-started.md) for initial setup
