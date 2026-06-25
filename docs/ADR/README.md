# Architecture Decision Records

This directory contains the Architecture Decision Records (ADRs) for this project.

## Format

Each ADR follows this structure:

```markdown
# NNNN: Title

**Status**: Proposed | Accepted | Superseded | Deprecated
**Date**: YYYY-MM-DD

## Context       — What is the issue motivating this decision?
## Candidates    — Options considered with trade-offs
## Decision      — What we chose and why
## Consequences  — What becomes easier/harder
## Constitution Compliance — Mapping to project principles
```

## Numbering Convention

ADRs are numbered sequentially: `0001`, `0002`, etc.
Numbers are never reused. If a decision is reversed, the original ADR is marked "Superseded" and a new ADR is created with a reference.

## Index

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-domain-hexagon-boundary.md) | Keep the Domain Behind Hexagonal Ports | Proposed |
| [0002](0002-sled-vfs-index-source.md) | Use Sled as the Persisted VFS Index Source | Proposed |
| [0003](0003-cas-shadow-file-mutations.md) | Use CAS-Guarded Shadow-File Mutations | Proposed |
| [0004](0004-workspace-daemon-mcp-clients.md) | Use One Shared Workspace Daemon for MCP Clients | Proposed |
| [0005](0005-native-sidecar-extensions.md) | Use Native Out-of-Process Sidecar Extensions | Proposed |
| [0006](0006-extension-registry-read-paths.md) | Keep Extension Registry Writes Out of MCP Serving Paths | Proposed |
| [0007](0007-sidecar-multiplexed-frame-queue.md) | Queue Inbound Sidecar Frames During HostCalls | Proposed |
| [0008](0008-mcp-mutations-real-extension-host.md) | Notify the Real Extension Host After MCP Mutations | Proposed |
| [0009](0009-defer-apply-edits-extension-callbacks.md) | Defer Extension Callbacks After Workspace Apply-Edits | Proposed |

<!--
  Update this table as ADRs are added. Format:
  | [0001](0001-short-name.md) | Decision Title | Accepted |
-->

## Creating a New ADR

1. Find the next number: `ls docs/ADR/ | grep -oP '^\d+' | sort -n | tail -1` + 1
2. Copy the template: `cp docs/ADR/.template.md docs/ADR/NNNN-short-name.md`
3. Fill in all sections
4. Update this index
5. Submit for review

## Pre-Merge Checklist

Before merging any new or modified ADR:

- [ ] **Cross-references**: All `[ADR-NNNN]` links resolve to existing files
- [ ] **Supersession**: If changing a prior decision, both ADRs have `Supersedes`/`Superseded by` metadata
- [ ] **Constitution**: Compliance section maps to current constitution version
- [ ] **Candidates**: At least 2 alternatives documented with trade-offs
