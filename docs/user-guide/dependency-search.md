# Search configured dependency roots

Use dependency search when a workspace needs read-only visibility into one or more
trusted source trees, such as a sibling library or vendored source directory.
Tower never discovers dependency roots automatically: configure them explicitly,
then opt in per search request.

## Configure a root

Add this to the active workspace's `.tower/config.toml`:

```toml
[dependencies]
ignore = ["target/", "*.generated.rs"]
max_files = 10000
max_file_bytes = 1048576
max_total_bytes = 67108864

[[dependencies.roots]]
id = "library"
path = "../library-src"
```

`path` is resolved relative to the workspace. `id` is the stable public name used
in search results and locators. All limits are positive aggregate bounds except
`max_file_bytes`, which is applied per file. Omitted values use the defaults shown
above. Ignore rules apply before file and byte accounting.

Restart the daemon after changing `.tower/config.toml`. A missing or unavailable
root does not disable workspace search; its issue is included in the dependency
report. Invalid configuration fails startup without exposing private filesystem
paths or rejected input.

## Search dependencies

Pass an explicit `scope` to either native search tool:

```json
{
  "name": "tower_search_text",
  "arguments": {
    "pattern": "ConnectionPool",
    "scope": { "workspace": true, "dependencies": true }
  }
}
```

For filename search, use the same shape with `query`:

```json
{
  "name": "tower_find_file",
  "arguments": {
    "query": "pool",
    "scope": { "workspace": false, "dependencies": true }
  }
}
```

The two scope flags are required whenever `scope` is present. Omitting `scope`
preserves the workspace-only response and its legacy shape. Setting both flags to
`false` returns no hits but still returns the current dependency report.

## Interpret origins and locators

Scoped results identify ownership explicitly:

```json
{
  "locator": "dependency://library/src/pool.rs",
  "path": "src/pool.rs",
  "origin": "dependency",
  "root_id": "library"
}
```

Dependency locators are canonical and read-only. Mutating tools and formatter
requests reject them before accessing writable storage or a formatter queue. A
workspace result has `origin: "workspace"` and a normal workspace-relative
`locator`.

## Refresh the snapshot

Dependency roots are scanned during daemon preparation. Run `tower_reindex` after
large external changes to refresh the workspace and dependency snapshots:

```json
{
  "name": "tower_reindex",
  "arguments": {}
}
```

The completed dependency snapshot is published atomically, so a concurrent search
sees either the previous complete snapshot or the new complete snapshot. Scan
limits and recoverable root/read failures are reported in `dependency_report`.
