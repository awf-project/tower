# Query another configured project

Use Tower's cross-project tools when an MCP client needs read-only information
from a second workspace without changing the active workspace or discovering
untrusted directories.

## Configure an external project

Add an explicit entry to the active workspace's `.tower/config.toml`:

```toml
[projects]
external_availability_timeout_secs = 10

[[projects.queryable]]
id = "shared-lib"
name = "Shared library"
path = "../shared-lib"
```

The `id` is the stable machine-facing selector. `name` is the human-facing
selector, and `path` is the configured workspace path. Every entry must have a
unique ID, name, and workspace identity. Tower does not scan for other projects.
The timeout bounds locating, spawning, and initializing the target daemon; it
must be a positive integer and defaults to 10 seconds.

Restart the active daemon after changing configuration. List the
configured targets with:

```json
{
  "name": "tower_list_projects",
  "arguments": {}
}
```

The response has this shape:

```json
{
  "projects": [
    { "id": "shared-lib", "name": "Shared library", "path": "../shared-lib" }
  ]
}
```

## Run a read-only query

Select one project and forward one of `tower_find_file`, `tower_list_dir`,
`tower_search_text`, or `tower_read_file`. For example, search
the target's indexed files:

```json
{
  "name": "tower_query_project",
  "arguments": {
    "project": { "id": "shared-lib" },
    "tool": "tower_search_text",
    "arguments": { "pattern": "ConnectionPool" }
  }
}
```

Successful results identify the source project and preserve paths relative to
that project's workspace:

```json
{
  "project": { "id": "shared-lib", "name": "Shared library" },
  "result": {
    "matches": [
      {
        "path": "src/pool.rs",
        "line_number": 12,
        "line_content": "struct ConnectionPool;"
      }
    ]
  }
}
```

Use exactly one selector field: `id`, `name`, or `path`. A valid query with no
matches returns an empty result with the same project identity. The active
workspace remains the caller's context, so paths in the result are not rewritten
to absolute paths.

## Understand the safety boundary

Only explicitly classified read-only native tools can be routed. File creation,
editing, deletion, formatting, lint fixes, debugging, extension tools, unknown
tools, and ambiguous operations are rejected before target connection or
dispatch. A configured target that is temporarily unavailable produces a
bounded error, while local tools remain usable.
