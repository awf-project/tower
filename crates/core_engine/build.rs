//! Build script for core_engine.
//!
//! Previously set `cargo:rustc-env` variables pointing to WASM test fixtures
//! (HELLO_PLUGIN_WASM, PLUGIN_AST_WASM, etc.) for the old wasmtime plugin host.
//! All WASM plugin machinery was removed in spec 29; the sidecar extension system
//! (spec 20–28) does not require any build-time artifact paths.

fn main() {}
