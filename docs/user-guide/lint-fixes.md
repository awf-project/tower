# Apply Lint Fixes

Use `tower_lint_fix` when you want Tower to apply machine-applicable linter fixes without giving the
linter or the extension direct write access to workspace files. The `lint` extension extracts
structured fixes, then asks the host to apply them through Tower's existing atomic write and
compare-and-swap path.

---

## Before you start

Configure at least one linter in `<workspace>/.tower/config.toml`:

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
extensions = ["js", "jsx", "ts", "tsx"]
format = "eslint-json"
target = "append"
```

Supported fix extraction is built in for `rustc-json` and `eslint-json`. `generic-regex` diagnostics
can still be reported by `tower_lint_check`, but they are reported as unsupported by
`tower_lint_fix`.

---

## Preview fixes first

Use `dry_run:true` to inspect the edits and preview content without touching the file on disk:

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "tools/call",
  "params": {
    "name": "tower_lint_fix",
    "arguments": {
      "path": "src/lib.rs",
      "dry_run": true
    }
  }
}
```

Typical result:

```json
{
  "files_changed": 0,
  "fixes_applied": 1,
  "fixes_skipped": [],
  "remaining_diagnostics": [],
  "previews": [
    {
      "path": "src/lib.rs",
      "edits": [
        {
          "start_byte": 42,
          "end_byte": 51,
          "replacement": "items.is_empty()"
        }
      ],
      "preview_content": "..."
    }
  ]
}
```

---

## Apply safe fixes

Omit `dry_run` to apply fixes. By default, Tower applies only safe structured fixes:

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "tools/call",
  "params": {
    "name": "tower_lint_fix",
    "arguments": {
      "path": "src/lib.rs"
    }
  }
}
```

The response reports:

- `files_changed`: how many files were actually rewritten
- `fixes_applied`: how many extracted fixes landed
- `fixes_skipped`: fixes Tower refused or could not apply
- `remaining_diagnostics`: one follow-up lint pass after successful writes

When `path` is omitted, Tower walks indexed files, selects only files with a matching lint
configuration, and processes them in path order.

---

## Opt in to unsafe fixes

Some linter suggestions are structured but not safe-by-default, such as rustc/clippy suggestions with
non-machine-applicable confidence. To include those, set `unsafe:true`:

```json
{
  "jsonrpc": "2.0",
  "id": 22,
  "method": "tools/call",
  "params": {
    "name": "tower_lint_fix",
    "arguments": {
      "path": "src/lib.rs",
      "unsafe": true
    }
  }
}
```

Without `unsafe:true`, those fixes are returned in `fixes_skipped` with `reason:"unsafe"`.

---

## Understand skipped fixes

`fixes_skipped` is part of normal tool behavior. Common reasons are:

| Reason | Meaning |
|---|---|
| `unsafe` | The fix was structured but not safe-by-default, and `unsafe:true` was not set. |
| `unsupported` | The diagnostic had no supported structured fix payload for `tower_lint_fix`. |
| `conflict` | The fix overlapped another accepted edit or the host rejected an overlapping edit. |
| `cas_conflict` | The file changed between lint extraction and apply, so Tower refused the write. |
| `invalid_range` | The emitted byte range was not valid for the current file content. |

These are reported in-band so callers can branch on them without treating the tool call itself as a
transport failure.

---

## Related pages

- [`../mcp-tools.md`](../mcp-tools.md) for the full `tower_lint_fix` reference
- [`../extensions.md`](../extensions.md) for the extension capability model
- [`../getting-started.md`](../getting-started.md) for initial workspace setup
