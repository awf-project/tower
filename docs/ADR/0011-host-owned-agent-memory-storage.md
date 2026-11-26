# 0011: Use Host-Owned Namespaced Storage for Agent Memory

**Status**: Proposed
**Date**: 2026-11-04
**Amended**: 2026-11-26 (extension discovery and storage authorization)
**Supersedes**: N/A
**Superseded by**: N/A

## Context

F017 introduces durable agent memory implemented by a first-party native sidecar. Memory records,
privacy rules, retrieval behavior, migrations, and optional enrichment belong to the extension, but
durable persistence creates a new privileged boundary. Allowing the sidecar to open a database or
workspace files directly would bypass the host's capability model and make namespace isolation,
durability, and authorization extension-specific concerns.

The existing VFS `StoragePort` is not a suitable persistence contract for agent memory. It is shaped
around `FileId` and `VirtualFile`, while memory requires opaque keys, ordered bounded scans,
conditional multi-key transactions, synchronous durable acknowledgement, and private namespaces.
Widening the VFS contract would couple unrelated domains and expose storage implementation concerns
across the hexagonal boundary.

## Candidates

| Option                                                     | Pros                                                                                                           | Cons                                                                                            |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Let the agent-memory sidecar own its database              | Keeps persistence implementation local to the extension                                                        | Grants direct filesystem access, duplicates durability policy, and weakens capability isolation |
| Add a host-owned, capability-gated namespaced storage port | Preserves sidecar-owned memory rules while centralizing authorization, transactions, durability, and isolation | Adds a shared protocol surface, host port, adapters, and compatibility obligations              |
| Extend the VFS `StoragePort` for memory data               | Reuses an existing host abstraction                                                                            | Mixes unrelated schemas and forces memory semantics into file-specific types                    |

## Decision

We will keep agent-memory business rules in the `agent_memory` sidecar and place physical storage
behind a dedicated host-owned `AgentMemoryStoragePort`. The port exposes bounded opaque reads,
ordered scans, and conditional multi-key transactions without exposing Sled or filesystem types.
The host will provide both Sled-backed and in-memory adapters that pass the same contract suite.

The normal extension search path selects the active `agent_memory` manifest and resolves its
command. The host grants storage HostCalls to this extension when its manifest declares
`agent_memory_storage`. The host derives the namespace from the canonical workspace identity
and the selected extension name. This keeps existing project data accessible after moving the
extension or changing its installation scope.

A project-local `agent_memory` takes precedence over a global installation of the same name
and can read and write the same project's memory. Operators can select a specific installation
with `--extensions-dir` or `TOWER_EXTENSIONS_DIR`. This amends the earlier first-party executable
check, which rejected the distributed extension in its documented installation directory.

Durable mode acknowledges a mutation only after the transaction is synchronously flushed.
Expected conflicts and uncertain commit outcomes are represented by typed protocol results.

The existing queued sidecar HostCall transport, supervision, cancellation, and JSON-RPC envelope
remain unchanged. Memory record schemas, privacy behavior, retrieval rules, and provider
orchestration remain sidecar concerns and are not added to the core domain.

## Consequences

**What becomes easier:**

- Agent memory retains a private, transactional, crash-recoverable persistence contract.
- The host can enforce capability authorization and namespace isolation before adapter access.
- Durable and in-memory adapters can share one behavioral contract suite.
- Memory schemas can evolve without coupling the VFS domain to memory record types.
- Future implementations can replace the physical adapter without changing memory business rules.

**What becomes harder:**

- The extension protocol gains a privileged compatibility surface that must be versioned and tested.
- Host assembly must select and inject the configured durable, read-only, or in-memory adapter.
- Transaction limits, conflict outcomes, flush acknowledgement, and uncertain commits require exact
  cross-adapter semantics.
- A local replacement inherits access to the project memory associated with the selected extension name.

## Constitution Compliance

| Principle                 | Status    | Justification                                                                                                    |
| ------------------------- | --------- | ---------------------------------------------------------------------------------------------------------------- |
| Domain purity             | Compliant | Memory rules remain in the sidecar, and the core domain does not import Sled, filesystem, or transport concerns. |
| Hexagonal architecture    | Compliant | Physical persistence is hidden behind a dedicated port with real and in-memory adapters.                         |
| Capability security       | Compliant | The selected manifest must declare storage. The host selects the namespace for each project and extension.       |
| Extension fault isolation | Compliant | Storage uses the existing supervised out-of-process HostCall path.                                               |
| Testability               | Compliant | In-memory and Sled-backed implementations must pass the same adapter contract suite.                             |
| Durability                | Compliant | Durable success is acknowledged only after a conditional transaction is synchronously flushed.                   |
