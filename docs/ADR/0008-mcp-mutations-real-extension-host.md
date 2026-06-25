# 0008: Notify the Real Extension Host After MCP Mutations

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

MCP file mutation handlers update the shared VFS and index directly through the domain mutation service. Extensions such as AST and LSP maintain their own derived state from file events. The filesystem watcher cannot always compensate for missing notifications because it may observe an idempotent event after the MCP mutation has already updated the shared VFS.

Project feedback identified a concrete failure: using `NoOpPluginHost` in MCP mutation handlers can leave deleted file symbols in the AST index forever.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| MCP mutation handlers use the real extension host | Derived extension indexes stay coherent | Mutations must be wired after extension host injection |
| Rely on filesystem watcher events | Keeps mutation handlers simpler | Watcher idempotency can suppress the needed extension notification |
| Force full extension reindex after every mutation | Simple recovery model | Expensive and noisy for frequent edits |

## Decision

We will ensure MCP mutation handlers construct mutation services with the real extension host injected into engine state. Create, delete, edit-range, global-replace, and host-owned apply-edits paths must notify extensions through the same extension host contract used by watcher-driven changes.

`NoOpExtensionHost` is only valid before extension host injection or in tests that explicitly do not assert extension notifications.

## Consequences

**What becomes easier:**
- AST, LSP, lint, and formatter observers see mutations made through MCP tools.
- Deleted files do not leave permanent derived-index state.
- Watcher idempotency can remain focused on filesystem reconciliation.

**What becomes harder:**
- Engine assembly must inject the real extension host before serving mutating MCP tools.
- Tests need recording extension hosts to prove notifications are emitted.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Index consistency | Compliant | Derived extension indexes receive mutation notifications. |
| Extension host contract | Compliant | MCP mutations use the same event path as watcher updates. |
| Correctness | Compliant | The decision prevents stale AST/LSP state after mutations. |
