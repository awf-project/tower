# 0001: Keep the Domain Behind Hexagonal Ports

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

Tower is organized around DDD, Hexagonal Architecture, and a microkernel extension model. The domain owns VFS metadata, mutation rules, indexing behavior, extension routing contracts, and user-facing tool semantics. Infrastructure concerns include Sled persistence, real filesystem access, OS file watching, MCP transport, and sidecar process management.

The core constraint is that domain behavior must remain testable with in-memory doubles. If infrastructure leaks into `domain/`, domain tests become slower, less deterministic, and harder to reason about. It also weakens the port contracts that let real adapters and fakes share behavior.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| Keep strict hexagonal boundaries | Domain remains deterministic, portable, and easy to test with fakes | Requires explicit ports and adapter wiring |
| Allow pragmatic infrastructure imports in domain modules | Shorter implementation path for some features | Couples business rules to storage, transport, and OS behavior |
| Split the domain into pure and impure subdomains | Makes exceptions explicit | Creates a second boundary that future changes can misunderstand |

## Decision

We will keep `domain/` pure. Domain modules must not import Sled, `std::fs`, `notify`, MCP transport, process management, or sidecar implementation details. Domain behavior depends on port traits, and infrastructure is wired through adapters.

Inbound ports define use cases. Outbound ports define dependencies. Real adapters and in-memory fakes must implement the same contracts.

## Consequences

**What becomes easier:**
- Domain unit tests stay fast and deterministic.
- Storage, filesystem, watcher, MCP, and sidecar implementations can evolve independently.
- Adapter contract tests can verify real implementations against the same behavior as fakes.

**What becomes harder:**
- New features need explicit port design before implementation.
- Some simple-looking changes require adapter wiring and test doubles.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Domain purity | Compliant | The decision keeps infrastructure out of `domain/`. |
| Testability | Compliant | Domain behavior remains testable with in-memory doubles. |
| Maintainability | Compliant | Boundaries make ownership and dependency direction explicit. |
