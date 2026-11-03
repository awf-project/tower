# Run Configured Commands

Tower can expose a constrained, host-owned `tower_run_command` MCP tool for
clients that do not have another command-execution facility. It is disabled by
default and is never available to sidecar extensions or configured external
projects.

## Enable the tool

Add an explicit allowlist to `<workspace>/.tower/config.toml`:

```toml
[shell]
enabled = true
timeout_secs = 30
stdout_max_bytes = 65536
stderr_max_bytes = 65536

[[shell.commands]]
name = "check"
argv = ["cargo", "check", "--workspace"]
```

The `argv` array is executed directly. Tower does not invoke a shell, so shell
expressions, pipelines, redirection, and background jobs are not interpreted.
The request supplies the configured command name and may append string
arguments:

```json
{
  "name": "tower_run_command",
  "arguments": {
    "command": "check",
    "args": []
  }
}
```

After changing configuration, restart the workspace daemon so the capability is
discovered from the new policy.

## Restrict the working directory

Omit `cwd` to run from the workspace root, or provide an existing
workspace-relative directory:

```json
{
  "name": "tower_run_command",
  "arguments": {
    "command": "check",
    "cwd": "crates/core_engine"
  }
}
```

Absolute paths, `..` traversal, missing paths, files, and symlinks resolving
outside the workspace are rejected before a process is spawned.

## Interpret the result

Successful execution returns a structured result containing `status`, `stdout`,
`stderr`, and `timed_out`:

```json
{
  "status": { "kind": "exited", "code": 0 },
  "stdout": {
    "kind": "text",
    "text": "Finished check\n",
    "byte_count": 15,
    "truncated": false
  },
  "stderr": {
    "kind": "text",
    "text": "",
    "byte_count": 0,
    "truncated": false
  },
  "timed_out": false
}
```

Each stream is capped independently. Invalid UTF-8 is returned as
`{"kind":"bytes","base64":"...",...}` rather than being replaced or lost.
A timeout reports `timed_out: true` after Tower terminates and reaps the full
process tree. A normal nonzero exit remains a structured result with
`timed_out: false` and its exit code; it is not converted into a transport
error.

## Safety boundaries

- The tool is absent from `tools/list` unless `[shell].enabled = true`.
- Only named configured commands can run; request input cannot replace the configured executable.
- Command arguments, output, executable paths, and raw operating-system errors are not logged by default.
- Caller cancellation terminates and reaps the process tree and returns a cancellation error.

For the complete wire schema and stable error mappings, see the
[MCP tool reference](../mcp-tools.md#tower_run_command).
