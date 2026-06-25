# 0009: Defer Extension Callbacks After Workspace Apply-Edits

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

Extensions can request `workspace/applyEdits` to perform CAS-guarded edits through the host. Some extensions also subscribe to file change events. If the host delivers `fileChanged` callbacks inline while still holding engine, workspace, index, or mutation locks, a sidecar can re-enter the same extension instance and deadlock the original HostCall.

Project feedback identified this as a high-priority pitfall for lint and LSP edit flows.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| Collect extension notifications during apply-edits and fan out after locks are released | Avoids reentrant sidecar deadlocks while preserving notifications | Requires deferred notification plumbing |
| Deliver callbacks inline from the mutation service | Straightforward control flow | Can re-enter the same sidecar and deadlock |
| Suppress callbacks for extension-originated edits | Avoids reentrancy | Leaves other extensions and indexes stale |

## Decision

We will defer extension callbacks produced by `workspace/applyEdits` until after the host releases engine state, workspace, index, and mutation locks. The mutation path records changed files, commits the edit, releases locks, and only then fans out `fileChanged` notifications through the extension host.

The same rule applies to future host-owned batch mutation capabilities that can be invoked from a sidecar.

## Consequences

**What becomes easier:**
- Sidecars can request edits without deadlocking on their own change callbacks.
- Other extensions still receive coherent post-commit notifications.
- Lock ownership remains local to mutation commit logic.

**What becomes harder:**
- Notification ordering must be documented and tested.
- Deferred callback buffers must preserve enough file identity and path information after locks are released.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Zero lock contention | Compliant | Callbacks run after mutation locks are released. |
| Extension fault isolation | Compliant | Reentrant sidecar deadlocks are avoided. |
| Atomic mutations | Compliant | Notifications are emitted only after successful commits. |
