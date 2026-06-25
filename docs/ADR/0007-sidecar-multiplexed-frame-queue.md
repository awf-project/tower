# 0007: Queue Inbound Sidecar Frames During HostCalls

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

The sidecar protocol multiplexes two flows on the child's stdin: host requests to the extension and host responses to the extension's HostCalls. A sidecar that waits for a HostCall response by matching only an id can accidentally discard host requests received in the same window.

This failure mode causes permanent deadlock: the host waits for a response to a request that the child already discarded.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| Queue non-matching inbound frames while awaiting HostCall responses | Preserves protocol ordering and avoids discarded host requests | Requires shared sidecar harness behavior and tests |
| Forbid HostCalls outside initialize | Simpler runtime loop | Too restrictive for LSP, lint, AST edits, and formatting |
| Use separate pipes for HostCalls and host requests | Clear transport separation | Breaks the current protocol shape and process harness |

## Decision

We will require sidecars that make HostCalls to queue inbound host requests encountered while awaiting a HostCall response. Queued frames are replayed FIFO after the HostCall response is handled.

Initialize-time HostCalls should complete before the extension sends `Initialized`, so the host does not hand normal control to a child that is not yet back in its main read loop. New long-lived HostCall paths must include parallel stress tests with at least 20 concurrent iterations.

## Consequences

**What becomes easier:**
- Sidecars can safely perform HostCalls during tool execution and event handling.
- Concurrent spawn and push-response races are covered by regression tests.
- The protocol hazard is documented close to the architectural decision.

**What becomes harder:**
- Sidecar loops need queueing logic instead of simple id-only response waits.
- Tests must cover concurrency windows that are otherwise rare.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Extension fault isolation | Compliant | Protocol stalls are handled as sidecar concerns, not host failures. |
| Correctness | Compliant | Host requests are not silently discarded. |
| Verification | Compliant | The decision requires stress coverage for new HostCall paths. |
