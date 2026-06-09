# tower

Core Engine for a high-performance productivity tool: a virtual filesystem, safe
mass text refactoring, an MCP (Model Context Protocol) interface, and a
"Drop & Play" WASM plugin architecture. Built with Domain-Driven Design and a
Hexagonal (ports & adapters) layout.

See [`project-brief.md`](project-brief.md) for the vision and
[`.agent/todo/`](.agent/todo/) for the spec roadmap.

## Workspace layout

```text
crates/
├── core_engine/   # bin (`tower`) + lib; domain / ports / adapters
├── plugin_sdk/    # lib: macros & host bindings for WASM plugins
└── plugin_ast/    # reference Tree-sitter plugin, built for wasm32-wasip1
```

## Prerequisites

The toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml); `rustup`
installs the right channel, components, and the `wasm32-wasip1` target
automatically. The license/advisory gate needs `cargo-deny`:

```sh
cargo install cargo-deny --locked
```

## Quality gate

Run the same checks CI enforces, in order:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p plugin_ast --target wasm32-wasip1
cargo deny check
```

## License

[EUPL-1.2](LICENSE).
