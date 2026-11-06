# Project Agent Memory

The first-party `agent_memory` sidecar stores explicit, structured project knowledge. It does not
capture terminal transcripts or conversation history automatically. Install the sidecar with the
other Tower extensions, then use the `tower_memory_*` tools through any MCP client.

## Configure persistence and privacy

The default configuration uses durable, project-scoped storage. Choose a fallback policy when the
durable store is unavailable:

```toml
[agent_memory.storage]
startup_policy = "read_only" # fail, read_only, or in_memory

[[agent_memory.privacy.rules]]
id = "credential"
pattern = "(?i)api[_-]?key\\s*[:=]\\s*\\S+"
action = "reject"
# For action = "redact", also set: replacement = "[REDACTED]"
```

`fail` stops startup when the configured store cannot be opened. `read_only` preserves retrieval
when a compatible store is available but rejects mutations. `in_memory` allows volatile operation
and reports that durable data is unavailable. Runtime failures do not silently change the selected
mode. Every response reports `persistence_mode`; mutation responses report `durable: true` only
after a durable acknowledgement.

Privacy rules are applied before persistence, indexing, logs, and optional provider requests.
Changing a rule is prospective: it does not scrub existing records or external backups. Forgetting
a record is logical deletion, not a forensic-erasure guarantee.

## Remember and retrieve a record

Records have one of these types: `observation`, `decision`, `bugfix`, `review_rule`, `task_note`,
or `session_summary`. Include provenance such as paths, symbols, tags, branch, and source agent
when it helps future searches.

Use the compact-first workflow:

1. Call `tower_memory_search` or `tower_memory_context_for_task` to receive bounded hits without
   full bodies.
2. Call `tower_memory_get` with the returned IDs to fetch complete records in request order.
3. Use `tower_memory_timeline` with a record, query, or time-window anchor when chronological
   context is useful.

`tower_memory_remember` returns a stable project-scoped ID and revision. Use that revision with
`tower_memory_update` or `tower_memory_forget`; mixed batches return independent per-item outcomes,
including stable `not_found` results for unknown IDs. An idempotency key lets a retry reuse the
original record instead of creating a duplicate.

## Tool overview

| Tool                            | Purpose                                               |
| ------------------------------- | ----------------------------------------------------- |
| `tower_memory_remember`         | Create one structured record with an idempotency key. |
| `tower_memory_update`           | Update records with expected-revision checks.         |
| `tower_memory_forget`           | Logically delete records.                             |
| `tower_memory_search`           | Return compact lexical and metadata-filtered hits.    |
| `tower_memory_timeline`         | Return bounded chronological events around an anchor. |
| `tower_memory_get`              | Fetch complete records by ID in request order.        |
| `tower_memory_context_for_task` | Return bounded compact context relevant to a task.    |

Lexical and metadata retrieval works without an LLM or semantic provider. Optional enrichment is
bounded and cannot block baseline memory operations indefinitely. See the [MCP tool reference](../mcp-tools.md#agent-memory-extension-tools)
for request fields, limits, stable outcomes, and error handling.

## Optional enrichment lifecycle

Without a configured provider, memory operations create no enrichment worker or provider request.
A configured provider uses one worker with one queued request and one queued result. Saturation
skips optional enrichment while the acknowledged record and lexical projection remain available.
The protocol thread publishes completed results when it next handles a frame, checking the record
revision before writing. Shutdown cancels outstanding provider HTTP work and joins the worker.
