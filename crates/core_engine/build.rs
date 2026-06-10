//! Build script for core_engine.
//!
//! Sets `cargo:rustc-env` variables pointing to wasm test fixtures so the
//! integration tests can locate them via `env!("...")`.
//!
//! # Decision: do NOT spawn `cargo build` from here
//!
//! Spawning `cargo build` inside a `build.rs` script causes a build-lock
//! deadlock: the outer `cargo build -p core_engine` holds the workspace build
//! lock; the inner `cargo build -p hello_plugin` tries to acquire the same
//! lock — and blocks forever.
//!
//! # How to build fixtures
//!
//! Run this once before `cargo test`:
//!
//! ```sh
//! cargo build -p hello_plugin -p fixture_abi_mismatch --target wasm32-wasip1
//! ```
//!
//! The test CI workflow in `.github/` runs this before the main `cargo test`.
//!
//! # Fixture paths
//!
//! - `HELLO_PLUGIN_WASM`          → `target/wasm32-wasip1/debug/hello_plugin.wasm`
//! - `ABI_MISMATCH_WASM`          → `target/wasm32-wasip1/debug/fixture_abi_mismatch.wasm`
//! - `FORBIDDEN_IMPORT_WASM`      → `tests/fixtures/forbidden_import.wasm` (committed)
//! - `FORBIDDEN_HOST_IMPORT_WASM` → `tests/fixtures/forbidden_host_import.wat` (committed)
//!
//! `FORBIDDEN_HOST_IMPORT_WASM` points at a `.wat` text file. wasmtime's default
//! features include the `wat` crate, so `Module::from_file` assembles it
//! transparently — no separate assembly step needed.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace root
        .expect("could not locate workspace root");

    let wasm_profile_dir = workspace_root
        .join("target")
        .join("wasm32-wasip1")
        .join("debug");

    println!(
        "cargo:rustc-env=HELLO_PLUGIN_WASM={}",
        wasm_profile_dir.join("hello_plugin.wasm").display()
    );

    println!(
        "cargo:rustc-env=ABI_MISMATCH_WASM={}",
        wasm_profile_dir.join("fixture_abi_mismatch.wasm").display()
    );

    println!(
        "cargo:rustc-env=FORBIDDEN_IMPORT_WASM={}",
        manifest_dir
            .join("tests")
            .join("fixtures")
            .join("forbidden_import.wasm")
            .display()
    );

    println!(
        "cargo:rustc-env=FORBIDDEN_HOST_IMPORT_WASM={}",
        manifest_dir
            .join("tests")
            .join("fixtures")
            .join("forbidden_host_import.wat")
            .display()
    );

    // Re-run if fixture sources change (for incremental correctness in IDE).
    println!("cargo:rerun-if-changed=../hello_plugin/src/lib.rs");
    println!("cargo:rerun-if-changed=../fixture_abi_mismatch/src/lib.rs");
    println!("cargo:rerun-if-changed=../plugin_sdk/src/lib.rs");
    println!("cargo:rerun-if-changed=../plugin_sdk/src/__private.rs");
    println!("cargo:rerun-if-changed=tests/fixtures/forbidden_import.wasm");
    println!("cargo:rerun-if-changed=tests/fixtures/forbidden_host_import.wat");
}
