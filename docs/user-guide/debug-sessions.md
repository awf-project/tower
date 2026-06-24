# Interactive Debug Sessions

This guide shows how to configure the `debug` extension and drive a live Debug Adapter Protocol
session through `tower_debug_*` MCP tools.

Debugging is opt-in. The tools are absent from `tools/list` unless the workspace has a valid
`[debug.<language>]` config entry and the `debug` extension is enabled. A discovered `debug`
extension takes priority; otherwise the bundled sidecar is used when its binary is available next to
`tower`.

## Configure an Adapter

Add a language entry to `<workspace>/.tower/config.toml`:

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

`command` and `adapter_type` must be non-empty. `extensions` must contain extension names without a
leading dot. `default_timeout_secs` and `idle_ttl_secs` must be positive integers. A malformed debug
section fails startup so a client does not see a half-configured debugger.

Build the workspace so `debug_extension` is available next to `tower`, or install a compatible
`debug` extension in an extension scope, then restart `tower`. The debug tools appear as
extension-contributed MCP tools:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}
```

Expected tool names include `tower_debug_launch`, `tower_debug_set_breakpoints`,
`tower_debug_continue`, `tower_debug_stack`, `tower_debug_variables`, `tower_debug_evaluate`, and
`tower_debug_terminate`.

## Launch and Stop

Start a session for a configured language. `launch_overrides` is merged into the configured `launch`
object and wins for duplicate keys.

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tower_debug_launch","arguments":{"language":"rust","program":"target/debug/app","cwd":".","args":[],"env":{},"launch_overrides":{}}}}
```

The result includes a `session_id`, top-level `state`, and a nested initial `stop` object when the
adapter reports an entry stop. Keep the `session_id` for every later call.

Set breakpoints with workspace-relative source paths:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tower_debug_set_breakpoints","arguments":{"session_id":"debug-1","path":"src/main.rs","breakpoints":[{"line":42,"condition":null,"hit_condition":null}]}}}
```

Resume until the adapter reports a stop, termination, or timeout:

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tower_debug_continue","arguments":{"session_id":"debug-1","thread_id":null,"timeout_secs":15}}}
```

A stopped response has `state:"stopped"`, `reason`, `thread_id`, `top_frame`,
`hit_breakpoint_ids`, `timed_out:false`, and `output_since`. If the timeout elapses, the response has
`state:"running"` and `timed_out:true`; the session remains controllable with `tower_debug_pause` or
`tower_debug_terminate`.

## Inspect Runtime State

List threads:

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"tower_debug_threads","arguments":{"session_id":"debug-1"}}}
```

Read the stack for a stopped thread:

```json
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"tower_debug_stack","arguments":{"session_id":"debug-1","thread_id":1}}}
```

Read variables from a scope or expandable variable reference:

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"tower_debug_variables","arguments":{"session_id":"debug-1","variables_reference":100}}}
```

Evaluate an expression in a frame:

```json
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"tower_debug_evaluate","arguments":{"session_id":"debug-1","frame_id":7,"expression":"answer"}}}
```

Stack, variables, and evaluate calls require a stopped session. A running session returns a stable
payload error with code `not-stopped`.

## Cleanup

Terminate the debuggee and adapter process tree:

```json
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"tower_debug_terminate","arguments":{"session_id":"debug-1"}}}
```

Use `tower_debug_disconnect` when the adapter should disconnect instead of terminate:

```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"tower_debug_disconnect","arguments":{"session_id":"debug-1"}}}
```

List live sessions at any time:

```json
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"tower_debug_sessions","arguments":{}}}
```

Sessions are ephemeral and are not restored after extension restart. Unknown, ended, expired, or lost
session ids return a stable payload error with code `session-not-found`.

## Error Handling

Debug runtime failures are returned as successful tool payloads so clients can branch on stable
codes:

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

Stable codes are `session-not-found`, `not-stopped`, `debug-timeout`, `adapter-exited`, and
`launch-failed`. Malformed tool arguments still use protocol-level invalid-params errors.
