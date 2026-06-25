# 0003: Use CAS-Guarded Shadow-File Mutations

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

Tower exposes file mutation tools to multiple MCP clients and extensions. Writes must avoid torn files, lost updates, and inconsistent VFS/index/storage state. The filesystem watcher also observes writes after the mutation path has already updated in-memory state, so watcher events must be idempotent.

Mutation tools accept optional content-version guards. The content version is a SHA-256 token returned by reads that request `with_version: true`.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| CAS-guarded mutations using shadow files and atomic rename | Prevents lost updates and torn writes | More implementation work and careful error handling |
| Direct in-place writes | Simpler implementation | Can tear files and exposes partial writes on crash |
| Lock all MCP clients globally during writes | Reduces concurrent conflicts | Hurts multi-agent workflows and still needs crash-safe filesystem writes |

## Decision

We will perform file mutations through the domain mutation service using optional CAS guards and the shadow-file pattern. The write path creates a temporary sibling ending in `.tmp_write`, flushes it durably, atomically renames it into place, and then updates VFS, index, and storage state.

Watcher and scanner paths must ignore `.tmp_write` artifacts. Watcher events caused by Tower's own writes must be idempotent and must not duplicate VFS records or change stable file identities.

## Consequences

**What becomes easier:**
- MCP clients can avoid overwriting each other's changes.
- Crashes do not expose partially written destination files.
- VFS, index, and storage updates remain centralized.

**What becomes harder:**
- Mutation errors must preserve enough detail for clients to distinguish conflicts, invalid ranges, and port failures.
- Watcher logic must handle echoed create/modify/delete events carefully.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Atomic mutations | Compliant | The decision requires shadow-file writes and atomic rename. |
| Multi-agent safety | Compliant | CAS guards provide optimistic concurrency control. |
| Index consistency | Compliant | Mutations update VFS, index, and storage together. |
