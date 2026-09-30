---
title: "Tower — Code Intelligence for AI Agents"
description: "A native Rust MCP server for workspace search, code navigation, safe edits, linting, and debugging through isolated extensions."
lead: "Give your AI coding agent the tools to search, understand, and safely edit your codebase through MCP."
date: 2026-03-14
draft: false
---

## Build Tower

Build the host and its native extensions with the repository’s pinned Rust toolchain:

```bash
git clone https://github.com/awf-project/tower.git
cd tower
cargo build --release --workspace --bins
```

## Explore your workspace

Point Tower at the project you want to index. It stores the index locally and watches for file changes.
Connect your MCP client to the built binary to search files, read source, and apply version-checked edits.
AST, language-server, linting, and debugging tools are provided by extensions; see the documentation for installation and configuration.
