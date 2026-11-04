# Retrieve Semantic Code Context

Use the `semantic_context` sidecar to find code by meaning, discover related files, and collect
snippets for a task within a token budget. It indexes eligible workspace source files; the index is
derived data stored separately from Tower's VFS index.

## Configure a local embedding service

Install the `semantic_context_extension` binary with the other Tower extensions, then make an
OpenAI-compatible `/v1/embeddings` service and its model available. For a local Ollama service,
add this to the workspace's `.tower/config.toml`:

```toml
[semantic_context]
enabled = true

[semantic_context.embedding]
provider = "openai_compatible"
base_url = "http://127.0.0.1:11434/v1"
model = "nomic-embed-text"
```

Restart the workspace daemon after changing the configuration. Tower does not install or start the
embedding service or download the model. The four semantic tools remain discoverable when the
section is absent or disabled, but return `status: "unsupported"` and
`reason: "provider_not_configured"`. A valid configuration with an unavailable service still lets
Tower start; the affected calls return a structured provider error.

For a non-loopback service, set an explicit HTTPS `base_url` and `allow_remote = true`. That
explicitly permits sending workspace source snippets to the service. If authentication is needed,
set `api_key_env` to the name of an environment variable containing the token. Keep the token out
of the configuration file. See [workspace configuration](configuration.md) for chunking, timeout,
and storage settings.

## Build or refresh the index

Call the semantic rebuild tool through your MCP client:

```json
{
  "name": "tower_semantic_reindex",
  "arguments": {}
}
```

An `ok` result reports `indexed_files`, `indexed_chunks`, and `generation`. After startup catch-up,
Tower applies indexed, changed, and deleted file events to the affected path. Rebuild after changing
the embedding model, vector dimensions, or chunking settings, or when the tool reports
`status: "reindex_required"`. The index can be recreated from the eligible workspace files.

## Search by meaning

```json
{
  "name": "tower_semantic_search",
  "arguments": {
    "query": "where are deleted files removed from the index?",
    "limit": 5,
    "languages": ["rust"],
    "lexical_rerank": true
  }
}
```

Each hit includes a workspace-relative `path`, UTF-8 `start_byte` and `end_byte`, `score`,
`language`, `snippet`, and `reason`. `lexical_rerank` is optional and defaults to `false`.
Use `path_prefixes` for a workspace-relative prefix such as `"crates/"`; omitted filters include
all indexed files. An unmatched prefix returns no hits. Empty queries and a zero `limit` return
empty results. A limit cannot exceed 1,000.

## Find related files

```json
{
  "name": "tower_semantic_related_files",
  "arguments": {
    "path": "crates/core_engine/src/adapters/daemon/lifecycle.rs",
    "limit": 5
  }
}
```

The response has deduplicated `files` with `path`, `score`, and `reason`. The source path must be
workspace-relative. `path_prefixes` and `languages` can narrow the result.

## Assemble task context

```json
{
  "name": "tower_semantic_context_for_task",
  "arguments": {
    "task": "explain how the watcher updates the index",
    "token_budget": 1200,
    "limit": 8,
    "path_prefixes": ["crates/"]
  }
}
```

The result contains diversified `chunks`, `estimated_tokens`, `truncated`, and
`eligible_unfitted`. The estimate is deterministic and conservative, based on UTF-8 bytes plus
separator cost; it is not a tokenizer-specific count. The tool stops before adding a chunk that
would exceed the requested budget. A zero budget yields no chunks and reports eligible context
that could not fit. `token_budget` cannot exceed 1,000,000 and `limit` cannot exceed 1,000.

See the [MCP tool reference](../mcp-tools.md#semantic-context-extension-tools) for response
shapes, filters, and error results.
