# 0005: Use Native Out-of-Process Sidecar Extensions

**Status**: Proposed
**Date**: 2026-06-25
**Supersedes**: N/A
**Superseded by**: N/A

## Context

Tower extensions provide AST, LSP, lint, formatting, and debug capabilities. Extensions can fail, hang, or be developed independently from the host. They also need constrained access to workspace capabilities such as read file, list files, apply edits, request format, and log.

The project does not want to require a runtime VM, JVM, Node runtime, or container at runtime.

## Candidates

| Option | Pros | Cons |
|--------|------|------|
| Native sidecars over JSON-RPC 2.0 stdio | Fault isolation, no runtime VM, language-independent wire contract | Requires process supervision and protocol discipline |
| In-process plugins | Low overhead and simple calls | Extension faults can take down the host |
| WASM sandbox | Strong sandboxing model | Adds runtime complexity and a runtime VM dependency |

## Decision

We will run extensions as native out-of-process sidecars communicating with the host over JSON-RPC 2.0 on stdio. Each extension declares tools, event subscriptions, and host capabilities in its manifest. The host supervises sidecars with timeouts, restart behavior, shutdown, and quarantine.

Extensions may access workspace state only through declared HostCalls routed by the sidecar adapter.

## Consequences

**What becomes easier:**
- Extension crashes and hangs do not sever the MCP host.
- Extension binaries can be built as part of the Rust workspace and shipped without an external runtime.
- Capability declarations provide an explicit security boundary.

**What becomes harder:**
- The sidecar protocol must be carefully versioned and tested.
- Long-running extension processes need lifecycle supervision and cleanup.

## Constitution Compliance

| Principle | Status | Justification |
|-----------|--------|---------------|
| Extension fault isolation | Compliant | Extensions run outside the host process. |
| Capability security | Compliant | Host access is limited to declared HostCalls. |
| No runtime VM | Compliant | Native sidecars avoid a required external VM/runtime. |
