# 0010: Stage Daemon Startup and Capability Readiness

**Status**: Proposed
**Date**: 2026-10-21
**Supersedes**: N/A
**Superseded by**: N/A

## Context

Tower uses one shared daemon per workspace. A cold start currently couples the client connection path to workspace and extension preparation. Slow Sled opening, reconciliation, watcher catch-up, or extension initialization can therefore exhaust the client timeout before MCP initialization completes. A timed-out client may then trigger another launch attempt while the first process is still opening the same persisted state.

One global readiness flag would avoid some races but would hide which capabilities are usable and unnecessarily block native workspace tools when only AST or LSP preparation is incomplete. Extension state also needs a precise transition from a reconciled workspace snapshot to ordered live events; otherwise a write during startup can be omitted or delivered out of order.

This decision complements ADR 0004. ADR 0004 selects one shared workspace daemon; this ADR defines that daemon's ownership, startup, readiness, and catch-up model.

## Candidates

| Option                                                                            | Pros                                                                                              | Cons                                                                                                 |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Increase the client socket timeout                                                | Small implementation change                                                                       | Masks latency, does not prevent competing owners, and provides no truthful readiness model           |
| Bind transport early behind one global ready flag                                 | MCP initialization can complete promptly                                                          | Blocks otherwise usable capabilities and cannot represent isolated AST or LSP failure                |
| Hold lifetime ownership, serve transport early, and publish independent readiness | Prevents competing Sled owners, keeps initialization prompt, and gates each capability truthfully | Adds lifecycle coordination, ordered publication, catch-up fences, and cancellation responsibilities |

## Decision

We will separate daemon lifetime ownership from client spawn serialization. The daemon acquires an exclusive workspace owner lock before stale-socket cleanup or Sled access and holds it until serving, preparation, workers, extensions, storage, and the socket have shut down. A distinct short-lived guard serializes client launch attempts. Foreground and detached startup use the same ownership path.

The daemon will bind and accept its transport before opening storage, reconciling the workspace, or preparing extensions. MCP initialization, status, shutdown, and tool descriptions remain available without a prepared engine. Workspace-dependent calls wait on workspace readiness within their own bounded preparation budget; a request timeout does not cancel daemon preparation.

Workspace, AST, and LSP readiness will be published independently by adapter-owned lifecycle state. Tool inventory and dispatch will use the same readiness snapshot. Unconfigured capability families are reported as unavailable, failures remain scoped when possible, and shutdown publishes a terminal stopping state. Domain and port contracts remain unchanged.

Committed file events from every authoritative producer will enter one ordered adapter-owned publication journal. Extension startup installs a subscriber while capturing a reconciled snapshot and watermark under the same publication ordering boundary. It replays that snapshot, drains committed events through a finite cut, and invokes a reserved host-only fence through the existing extension invocation path before atomically switching the extension to ready live delivery. Child I/O and callbacks never run while authoritative or publication locks are held.

## Consequences

**What becomes easier:**

- Clients can initialize promptly while slow workspace or extension preparation continues.
- Only one daemon generation can own Sled and workspace resources, including after client timeouts.
- Native workspace, AST, and LSP tools expose truthful, independently gated availability and failure states.
- Snapshot replay and concurrent mutations have an explicit ordering boundary with no event-loss window.
- Startup failure, cancellation, shutdown, and observability have one adapter-owned lifecycle model.

**What becomes harder:**

- The daemon must coordinate lifecycle transitions, bounded waits, worker cancellation, and owner-last shutdown.
- Every committed mutation producer must publish through the shared ordering boundary.
- Readiness-aware extensions must implement the reserved catch-up fence and preserve queue ordering.
- Compatibility requires conservative status decoding and stable structured readiness errors.
- Concurrency and recovery tests must cover ownership races, event cuts, delayed fences, and partial capability failure.

## Constitution Compliance

| Principle                 | Status    | Justification                                                                                                                    |
| ------------------------- | --------- | -------------------------------------------------------------------------------------------------------------------------------- |
| Domain purity             | Compliant | Ownership, readiness, publication, and instrumentation remain in adapters; domain and port signatures do not change.             |
| Multi-agent model         | Compliant | A lifetime workspace owner prevents concurrent daemon generations from opening the same Sled state.                              |
| Zero lock contention      | Compliant | Publication locks order in-memory commit records only; sidecar calls, disk replay, and callbacks occur after locks are released. |
| Extension fault isolation | Compliant | AST and LSP readiness and failures are isolated, bounded, and supervised per capability family.                                  |
| Index consistency         | Compliant | Snapshot watermarks, ordered committed events, and host-only fences close the startup-to-live-delivery gap.                      |
| Verification              | Compliant | The design requires process, concurrency, recovery, ordering, and full workspace completion tests.                               |
