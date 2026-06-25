# 0002: Use Sled as the Persisted VFS Index Source

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

Tower keeps a workspace VFS, file metadata, and text-search index available to MCP clients and extensions. The daemon is long-lived, but it can be stopped while files are changed externally. On startup, the persisted index must be restored quickly without trusting stale records forever.

Prior project feedback identified a stale-index hazard: loading persisted Sled records verbatim can keep deleted files visible as permanent ghosts.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| Persist VFS/index state in Sled and reconcile paths on startup | Fast restart, durable index, removes stale deleted paths | Requires startup reconciliation logic |
| Always rebuild the full index from disk on startup | Simple correctness model | Slow on large workspaces and duplicates persisted index work |
| Treat Sled as advisory cache only | Avoids trusting persisted state | Weakens daemon restart performance and complicates source-of-truth semantics |

## Decision

We will use Sled as the persisted source of truth for loaded VFS and index state, then reconcile that state against the current filesystem during startup. Reconciliation is path-based and ignore-aware: paths absent from the filesystem walk are pruned, but file contents are not re-read solely for reconciliation.

Full reindex remains available through `tower_reindex` for explicit recovery or large external changes.

## Consequences

**What becomes easier:**
- Daemon startup can reuse persisted index state.
- Deleted files changed while Tower was offline are removed deterministically.
- Large repositories avoid unnecessary content hashing during normal startup.

**What becomes harder:**
- Startup has a required reconciliation phase.
- The persisted index and filesystem walker must apply the same ignore rules.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Index source of truth | Compliant | Sled remains the persisted source for loaded index state. |
| Correctness | Compliant | Startup reconciliation removes stale paths. |
| Performance | Compliant | Reconciliation avoids re-reading and hashing all file contents. |
