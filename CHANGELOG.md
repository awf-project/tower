# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-11-26

### Fixed

- Linux release builds now package the Tower binary and bundled extensions on clean runners. Packaging errors stop publication.
- MCP clients can discover registered tools from extensions without a catch-up dependency, including formatting, linting and debugging.
- Language servers opened for nested projects now resolve document URIs and navigation results against the Tower workspace and apply nested `.towerignore` rules to navigation results.
- Rust debugging with GDB now completes the deferred DAP launch handshake and reports the observed entry stop or a running session when entry stops are disabled.
- One-shot debug probes skip GDB's temporary breakpoint at `main` when the requested breakpoint is elsewhere.
- LSP navigation evaluates `.towerignore` once per result path in each query, reducing the cost of finding many references in the same file.
- Continuing after an acknowledged GDB resume times out now waits for the running program's next stop or exit.
- rr recordings preserve the program exit code and register only finalized traces. GDB replay connects to the recorded process and supports reverse stepping and reverse continue.
- LSP navigation reports a workspace policy error when a result's exclusion rules cannot be evaluated.
- Ignored, untracked filesystem events no longer stage a copy of the workspace index before source file events.

## [0.1.0] - 2026-11-20
