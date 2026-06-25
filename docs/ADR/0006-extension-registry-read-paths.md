# 0006: Keep Extension Registry Writes Out of MCP Serving Paths

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

The daemon stores the extension registry behind a shared `RwLock`. MCP requests list and invoke extension tools while the watcher can deliver file events to the same registry. Watcher delivery uses blocking backpressure into extension mailboxes.

Project feedback identified a deadlock risk: a serving path that takes a registry write lock can block behind watcher backpressure while other serving paths and watcher callbacks need concurrent read access.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| Register extensions only during startup; serving paths use read locks | Allows concurrent reads and avoids writer starvation hazards | Dynamic runtime registration is not supported |
| Allow registry mutation during serving | Enables dynamic extension changes | Introduces lock contention and deadlock risk |
| Replace `RwLock` with a custom actor | Serializes access explicitly | Larger runtime redesign and message-passing complexity |

## Decision

We will treat extension registration as a startup-only operation. MCP serving paths and watcher event delivery must not take a write guard on the shared extension registry. Hot-path operations such as declared tool listing, tool invocation, and hook delivery use read access.

If runtime extension installation is needed later, it must be designed as a separate lifecycle with explicit quiescence or registry swapping.

## Consequences

**What becomes easier:**
- MCP serving paths can run concurrently with watcher hook delivery.
- The registry lock model stays simple and auditable.
- Backpressure from extension mailboxes does not introduce writer-lock deadlocks.

**What becomes harder:**
- Runtime extension registration or reload is deferred.
- Future registry mutation features need a deliberate lifecycle design.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Zero lock contention | Compliant | Hot paths avoid registry write locks. |
| Extension fault isolation | Compliant | Backpressure remains contained to extension delivery. |
| Maintainability | Compliant | The lock contract is explicit and narrow. |
