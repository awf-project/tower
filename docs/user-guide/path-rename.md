# Rename a File or Directory

Use `tower_rename_path` to move an indexed file or directory within a workspace.
Tower validates the complete move before changing the filesystem, then updates
the index while preserving moved files' identities. A directory result lists its
root move and every indexed descendant move.

## Preview the move

Send workspace-relative source and destination paths. Start with `dry_run` to
inspect the proposed moves and text edits without changing disk, file versions,
or the index:

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
      "semantic_policy": "none",
      "dry_run": true
    }
  }
}
```

A path-only preview has `status: "preview"`, a `plan.moves` entry for the file,
an empty `plan.edits` array, and no `edit_result`. Use the same request with
`dry_run: false` to apply it; Tower checks the paths and versions again, so a
preview does not reserve the destination.

## Choose how to update references

Set `semantic_policy` on every request:

| Value         | Behavior                                                                                                                                                |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `none`        | Move the path without asking a language server for text edits.                                                                                          |
| `best_effort` | Ask a configured language server for import and reference edits; move the path even if semantic planning is unsupported or fails, and inspect `errors`. |
| `required`    | Move only when the language server supplies a valid complete edit plan. Unsupported or failed planning leaves the workspace unchanged.                  |

For semantic updates, configure a language server for the affected language as
described in [Semantic Edits](semantic-edits.md#before-you-start). The server must
support `workspace/willRenameFiles`. Tower applies its text edits through the
existing workspace edit path after the native move. The move and text edits do
not form one multi-file transaction.

## Apply with version checks

To protect a file against intervening edits, read it with `tower_read_file` and
`with_version: true`, then pass its SHA-256 `version` under its **pre-move** path
in `expected_versions`. The map is optional; its keys must name moved files or
semantic edit targets, and its values must be 64-character hexadecimal hashes.
For a directory move, guard individual files rather than the directory path.

```json
{
  "jsonrpc": "2.0",
  "id": 31,
  "method": "tools/call",
  "params": {
    "name": "tower_rename_path",
    "arguments": {
      "source": "src/old.rs",
      "destination": "src/new.rs",
      "semantic_policy": "required",
      "expected_versions": {
        "src/old.rs": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
      }
    }
  }
}
```

Replace the sample hash with the actual version from `tower_read_file`. The
request guard is checked before the move; each planned text edit also carries
its own content hash for a per-file check when edits are applied.

## Read the result and recover

Check `status`, `plan`, `edit_result`, and `errors` before treating the refactor
as complete:

| Status        | Meaning                                                                                                                            |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `preview`     | Complete proposed `plan.moves` and `plan.edits`; no mutation.                                                                      |
| `applied`     | Native move committed and all planned text edits succeeded; `errors` may still contain a `best_effort` planning warning.           |
| `partial`     | Native move committed, but persistence or one or more text edits failed. Inspect `errors` and `edit_result.per_file` when present. |
| `unsupported` | Required semantic support was unavailable; no mutation.                                                                            |
| `rejected`    | Validation, required semantic planning, or filesystem movement failed before a committed move; no indexed rename was published.    |

If `partial` reports `persistence_error`, run `tower_reindex` or restart Tower
to reconcile the persisted index with the filesystem. If text edits failed,
inspect `edit_result.per_file` and repair those files at their destination paths.
Do not repeat the original rename request after a partial result: the source
path may already be gone.

Tower rejects an existing or excluded destination, a path outside the
workspace, and a directory destination inside its own source. Rejections and
previews leave the workspace unchanged. Malformed request fields produce a
JSON-RPC invalid-params error; ordinary rename failures use the structured
`status` and `errors` result described in the [MCP tool reference](../mcp-tools.md#tower_rename_path).
