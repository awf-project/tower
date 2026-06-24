# Debug Sessions And Probes

This guide shows how to configure the `debug` extension, drive a live Debug Adapter Protocol
session, run a stateless one-shot probe, and use rr-backed replay workflows through
`tower_debug_*` MCP tools.

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
`tower_debug_continue`, `tower_debug_stack`, `tower_debug_variables`, `tower_debug_evaluate`,
`tower_debug_eval_at`, and `tower_debug_terminate`.

## Enable rr Record/Replay

Add `[debug.record] backend = "rr"` when you want time-travel debugging tools. Without this section,
the rr-specific tools are absent from `tools/list` even if ordinary debug tools are enabled.

```toml
[debug.record]
backend = "rr"
trace_dir = ".tower/traces"
ttl_secs = 86400
max_traces = 20
record_timeout_secs = 60
```

`trace_dir` must stay inside the workspace. `ttl_secs`, `max_traces`, and
`record_timeout_secs` are optional positive values; omit `ttl_secs` for no TTL expiry. rr host
support is checked when recording. Missing rr, non-Linux hosts, unsupported CPUs, or unsupported
perf-counter settings return `recordable:false` with `reason:"rr_unsupported"` instead of crashing
the extension.

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

## Run A One-Shot Probe

Use `tower_debug_eval_at` when you need breakpoint evidence without keeping an interactive session
alive. The probe launches the configured program, sets one optional breakpoint, continues until a
hit, normal exit, timeout, or adapter exit, captures requested evidence, and then terminates the
session internally. The response never contains a `session_id`.

```json
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"tower_debug_eval_at","arguments":{"lang":"rust","program":"target/debug/app","breakpoint":{"path":"src/main.rs","line":42,"condition":null},"expressions":["answer"],"capture":{"stack":true,"locals":true,"args":true},"on_hit":"first","max_depth":2,"max_children":50,"timeout_ms":5000}}}
```

On a hit, the payload includes `hit:true`, `finished:"stopped"`, captured stack/frame data,
expanded locals and args according to the requested bounds, evaluated expression results, and
captured output:

```json
{
  "hit": true,
  "hits": [
    {
      "thread_id": 1,
      "frame": { "id": 7, "name": "main", "path": "src/main.rs", "line": 42, "column": 1 },
      "stack": [],
      "locals": [],
      "args": [],
      "evaluated": {
        "answer": { "value": "42", "type": "i32" }
      }
    }
  ],
  "output": [],
  "finished": "stopped",
  "exit_code": null,
  "condition_unsupported": null
}
```

If the breakpoint is not reached before normal process exit, the payload has `hit:false`,
`finished:"exited"`, and an `exit_code` when the adapter provides one. A timeout returns
`finished:"timeout"` after teardown. Individual expression failures appear under that expression as
`{"error":"..."}` without failing the whole probe.

## Record and Replay

Record a built native program:

```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"tower_debug_record","arguments":{"language":"rust","program":"target/debug/app","args":[],"cwd":".","env":{},"timeout_ms":60000}}}
```

A successful result includes `recordable:true`, `trace_id`, trace metadata, the recorded program
`exit_code`, bounded `output`, and `output_truncated`. An unsupported host returns
`recordable:false` and an `error.data.unsupported_reason` such as `rr_missing` or
`non_linux_host`.

Open a replay session from the trace:

```json
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"tower_debug_replay","arguments":{"trace_id":"trace-1","language":"rust","timeout_secs":15}}}
```

Replay returns a normal `session_id` plus `supportsStepBack:true`. Use the same inspection tools as
live sessions, then call `tower_debug_terminate` or `tower_debug_disconnect` when done.

## Navigate Backward

Step backward by line, instruction, or "over":

```json
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"tower_debug_step_back","arguments":{"session_id":"debug-1","thread_id":1,"granularity":"over","timeout_secs":15}}}
```

Set a write watchpoint and reverse-continue to the previous write or stop:

```json
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"tower_debug_watchpoint","arguments":{"session_id":"debug-1","expression":"x","address":null,"kind":"write","enabled":true,"timeout_secs":15}}}
```

```json
{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"tower_debug_reverse_continue","arguments":{"session_id":"debug-1","thread_id":1,"timeout_secs":15}}}
```

Reverse operations on live non-replay sessions return a structured payload error with code
`reverse_unsupported`.

## Find an Origin

Use `tower_debug_find_origin` when you already have a trace and need the last write to a watched
value. The tool opens replay, seeks to the requested target, sets a write watchpoint, runs backward,
captures bounded stack/value evidence, and cleans up the replay session before returning.

```json
{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"tower_debug_find_origin","arguments":{"trace_id":"trace-1","language":"rust","at":{"kind":"crash"},"watch":"x","timeout_secs":15,"max_depth":2,"max_children":50}}}
```

Targets are `{"kind":"crash"}`, `{"kind":"end"}`, or
`{"kind":"source","path":"src/main.rs","line":42,"column":1}`. A found result includes
`found:true`, `write_frame`, `stack`, `value`, `locals`, `args`, `output`, and `truncated`. If replay
reaches the beginning of the trace without a prior write, the result is `found:false` with
`reason:"no_prior_write_reached"`.

Use `tower_debug_record_and_find_origin` to combine recording and origin search:

```json
{"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"tower_debug_record_and_find_origin","arguments":{"record":{"language":"rust","program":"target/debug/app","args":[],"cwd":".","env":{},"timeout_ms":60000},"origin":{"language":"rust","at":{"kind":"crash"},"watch":"x","timeout_secs":15,"max_depth":2,"max_children":50}}}}
```

The response always includes the `record` result. `origin` is `null` when recording is unsupported;
otherwise it contains the same shape as `tower_debug_find_origin`.

## Manage Traces

List traces:

```json
{"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"tower_debug_traces","arguments":{}}}
```

Delete a trace:

```json
{"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"tower_debug_delete_trace","arguments":{"trace_id":"trace-1"}}}
```

New recordings prune expired traces and oldest traces above `max_traces`. Trace ids are restricted
to ASCII letters, digits, `.`, `_`, and `-`; invalid, missing, deleted, or expired trace ids return a
structured payload error instead of escaping the configured trace root.

## Cleanup

Terminate the debuggee and adapter process tree:

```json
{"jsonrpc":"2.0","id":19,"method":"tools/call","params":{"name":"tower_debug_terminate","arguments":{"session_id":"debug-1"}}}
```

Use `tower_debug_disconnect` when the adapter should disconnect instead of terminate:

```json
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"tower_debug_disconnect","arguments":{"session_id":"debug-1"}}}
```

List live sessions at any time:

```json
{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"tower_debug_sessions","arguments":{}}}
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

Stable codes include `session-not-found`, `not-stopped`, `debug-timeout`, `adapter-exited`,
`launch-failed`, `reverse_unsupported`, `rr_unsupported`, `record_timeout`, and `record_failed`.
Malformed tool arguments still use protocol-level invalid-params errors.
