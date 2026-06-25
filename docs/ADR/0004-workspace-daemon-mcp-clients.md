# 0004: Use One Shared Workspace Daemon for MCP Clients

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

Tower is used by multiple agents and clients in the same workspace. Each client needs MCP over stdio, but the expensive and stateful parts of the engine are the VFS, Sled index, watcher, extension registry, and sidecar processes.

Running a full independent engine per client would duplicate watchers and extension hosts, amplify filesystem contention, and create divergent indexes.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| One daemon per workspace with thin `tower mcp` clients | Shared state, single watcher, consistent extension state | Requires socket lifecycle and idle management |
| One engine per MCP client | Simple stdio lifecycle | Duplicates state and can create inconsistent indexing |
| One global daemon for all workspaces | Centralized management | Harder isolation, workspace-specific config, and cleanup |

## Decision

We will run one shared daemon per workspace. `tower mcp` is a thin stdio client that connects to or spawns the daemon. The daemon owns the Sled index, watcher, extension registry, and sidecar processes, and listens on `<workspace>/.tower/daemon.sock`.

The daemon serves one MCP session per connected client over shared state. It self-terminates after the configured idle timeout when no clients remain.

## Consequences

**What becomes easier:**
- Multiple agents observe one coherent workspace state.
- Only one watcher and extension registry operate per workspace.
- Sidecar lifecycle is centralized.

**What becomes harder:**
- The daemon must handle concurrent clients safely.
- Socket cleanup, idle timeout, and connect-or-spawn behavior become part of the runtime contract.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Multi-agent model | Compliant | The decision makes the daemon the shared workspace authority. |
| Low contention | Compliant | Shared state avoids duplicate watchers and indexes. |
| Operational clarity | Compliant | Runtime ownership lives in one foreground daemon process. |
