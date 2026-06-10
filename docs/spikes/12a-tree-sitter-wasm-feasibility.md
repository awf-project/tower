# Spike 12a — Tree-sitter → wasm32-wasip1 Feasibility

| Field     | Value                        |
|-----------|------------------------------|
| Status    | SUCCESS — exit criterion met |
| Date      | 2026-06-11                   |
| Spec      | 12a                          |
| Delivers  | AC1 (parse + symbol), AC2 (recipe for 12c) |

## Decision

**Approach A wins.** Vendor the tree-sitter C grammar sources via the `tree-sitter` + `tree-sitter-rust`
Rust crates and compile them to `wasm32-wasip1` using the WASI SDK clang as the C toolchain. The cc
crate handles C compilation automatically when two environment variables point it at the WASI SDK.

```
// Decision: Approach A (vendor C + compile via cc crate + WASI SDK)
// Why: only working path; Approach C3 is identical in mechanism; Approach B is a dead end
// Trade-off: build-time dependency on WASI SDK (installed once, reusable across machines)
```

## Exit criterion

Parsed `"fn hello() {}"` inside `wasm32-wasip1`, emitted the symbol `hello` to stdout via
`wasmtime run`. **Met by Approaches A and C3 (C3 is mechanically identical to A).**

---

## Approach Results

### A — Vendor C sources, compile via cc crate + WASI SDK clang

**Result: SUCCESS. Exit criterion met.**

- `tree-sitter = "0.25"` (resolved `0.25.10`) + `tree-sitter-rust = "0.23"` (resolved `0.23.3`)
- Set `CC_wasm32_wasip1` and `AR_wasm32_wasip1` to WASI SDK toolchain; `cargo build --release --target wasm32-wasip1` compiles cleanly.
- `wasmtime run spike.wasm` outputs `hello`.
- Module size: 1.2 MB release (`opt-level = "s"`).

**Why it works:** The tree-sitter crates vendor their C parser sources and compile them via the `cc`
crate. The Rust toolchain's `wasm32-wasip1` target component contains Rust std but **no C sysroot**
(`stdlib.h` is absent). WASI SDK 25+ provides both a wasm-targeting clang and the full `wasi-sysroot`
that satisfies `#include <stdlib.h>`. The cc crate honours the
`CC_<target>` / `AR_<target>` env-var pattern (with hyphens replaced by underscores) to select a
per-target compiler.

### B — Pure-Rust tree-sitter bindings (no C compilation in guest)

**Result: BLOCKED. Ruled out.**

`tree-sitter-c2rust-core v0.20.9` (the runtime transpiled to Rust) compiles to `wasm32-wasip1`
successfully, but no pure-Rust tree-sitter **grammar** for the Rust language exists anywhere on
crates.io or GitHub. The only grammar crate, `tree-sitter-rust`, unconditionally compiles `parser.c`
+ `scanner.c` via the cc crate. Without a WASI sysroot those builds fail:

```
fatal error: 'stdlib.h' file not found
error: failed to run custom build command for `tree-sitter-rust v0.24.2`
```

`tree-sitter-c2rust` v0.25.2 has an API mismatch on Rust 1.96.0 (`stop_printing_dot_graphs` not
found). No wasm32-wasip1-compatible grammar exists. **Approach B is a dead end for any grammar.**

### C — Precompiled grammar wasm loaded inside the guest

**Result: BLOCKED (pure C variants). C3 sub-variant = equivalent to A.**

Three sub-variants assessed:

- **C1 (nested wasm instantiation):** wasm32-wasip1 has no WASI API for wasm module instantiation.
  The grammar `.wasm` produced by `tree-sitter build --wasm` is an emscripten PIC module importing
  from `"env"` (`calloc`, `free`, `iswspace`, `iswalpha`, `memory`, `__indirect_function_table`,
  `__memory_base`, `__table_base`) and requires a JS runtime host. Cannot be loaded inside a wasip1
  guest.
- **C2 (pure-Rust parse-table interpreter):** no such crate exists on crates.io.
- **C3 (grammar C sources compiled together with guest via cc + WASI SDK):** succeeds. This is
  mechanically identical to Approach A — same toolchain, same env vars, same outcome. Used
  `tree-sitter = "0.26.9"` + `tree-sitter-rust = "0.23.3"` with the wasi-sdk cached automatically
  by `tree-sitter build --wasm` at `~/.cache/tree-sitter/wasi-sdk` (version 29.0, llvm 21.1.4).
  Module size: 1.3 MB release.

---

## Independent Verification (decision agent)

Re-run in a clean `/tmp/verify-12a` scratch directory (not part of the tower workspace):

**Cargo.toml**
```toml
[dependencies]
tree-sitter = "0.25"
tree-sitter-rust = "0.23"

[profile.release]
opt-level = "s"
```

**src/main.rs** — parse `b"fn hello() {}"`, walk tree for `identifier` child of `function_item`,
print the byte slice.

**Build**
```
CC_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang \
AR_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/llvm-ar \
cargo build --release --target wasm32-wasip1
```

**Output**
```
   Compiling tree-sitter-rust v0.23.3
   Compiling tree-sitter v0.25.10
   Compiling verify-12a v0.1.0 (/tmp/verify-12a)
    Finished `release` profile [optimized] target(s) in 4.39s
```

**Run**
```
$ ~/.cargo/bin/wasmtime run /tmp/verify-12a/target/wasm32-wasip1/release/verify-12a.wasm
hello
```

Module size: 1.2 MB (`wasm32-wasip1` release, `opt-level = "s"`).  
wasmtime version: 45.0.1  
WASI SDK: clang 21.1.4-wasi-sdk (from `~/.cache/tree-sitter/wasi-sdk`, installed by tree-sitter CLI)

---

## Recipe for Spec 12c

### Prerequisites (one-time machine setup)

```bash
# 1. Rust target
rustup target add wasm32-wasip1

# 2. WASI SDK — two equivalent sources:
#    Option A: use what tree-sitter CLI already downloaded (zero extra work)
#      ls ~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang
#    Option B: download WASI SDK 25+ explicitly
#      curl -sL https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-25/\
#        wasi-sdk-25.0-x86_64-linux.tar.gz | tar -xz -C /opt/wasi-sdk --strip-components=1
#      export CC_wasm32_wasip1=/opt/wasi-sdk/bin/wasm32-wasip1-clang
#      export AR_wasm32_wasip1=/opt/wasi-sdk/bin/llvm-ar

# 3. wasmtime (runtime, dev/test only)
cargo install wasmtime-cli --locked
```

### Cargo.toml dependencies for plugin_ast

```toml
[dependencies]
tree-sitter = "0.25"          # resolved: 0.25.10 — change only after re-testing
tree-sitter-rust = "0.23"     # resolved: 0.23.3  — same caveat
```

No additional crates required. Both crates vendor their C sources; the cc build dependency is
transitive.

### Build command (and CI env)

```bash
CC_wasm32_wasip1=/path/to/wasi-sdk/bin/wasm32-wasip1-clang \
AR_wasm32_wasip1=/path/to/wasi-sdk/bin/llvm-ar \
cargo build --release --target wasm32-wasip1 -p plugin_ast
```

The env vars follow the cc crate convention: `CC_<target>` / `AR_<target>` with non-alphanumeric
chars replaced by `_`. These must be set in every CI runner that builds the wasm target.

### Why these two env vars and nothing else

The Rust wasm32-wasip1 toolchain component ships only `libstd` — it has no C sysroot. The WASI SDK
clang carries `--sysroot` pointing at `wasi-sysroot` inside its `.cfg` wrapper, so `stdlib.h` and
friends resolve automatically once `CC_wasm32_wasip1` is set. `AR_wasm32_wasip1` is required because
the default `ar` on the host produces non-wasm archives.

### Version pinning caveat

`tree-sitter-rust 0.23.x` targets `tree-sitter 0.25.x`. Upgrading either together is safe; crossing
major/minor boundary (e.g., `tree-sitter-rust 0.24.x` with `tree-sitter 0.25.x`) causes API
mismatches. Lock both explicitly in `Cargo.toml` and test any upgrade.

### Module size baseline

- Release (`opt-level = "s"`): ~1.2 MB
- The grammar C sources (parser.c + scanner.c) account for most of the size; this is expected for
  a tree-sitter grammar embedded in wasm. `wasm-opt -Os` can reduce this further if needed.

### What spec 12c does NOT need to re-derive

- How to get a C sysroot for wasm32-wasip1 (WASI SDK, as above).
- Why pure-Rust grammars don't exist (Approach B exhausted).
- Why pre-compiled grammar wasm can't be loaded inside a guest (Approach C1 exhausted).
- The correct cc crate env var naming convention.

---

## Caveats and risks for 12c

1. **WASI SDK availability in CI.** The env vars must be set in every runner building the wasm
   target. Consider a `.cargo/config.toml` `[target.wasm32-wasip1]` `linker` entry or a
   `build.rs` that auto-detects the WASI SDK path and emits `cargo:rustc-env=CC_wasm32_wasip1=…`
   to avoid per-machine manual setup.

2. **Grammar version drift.** tree-sitter updates grammar crates frequently. The build will break
   silently on upgrade if the C sysroot environment is missing on a new CI runner. Pin versions in
   `Cargo.lock` and test upgrades explicitly.

3. **Module size.** At 1.2 MB the grammar+runtime is well within typical wasm limits, but 12c
   should track size as additional grammars are added (`wasm-opt`, `strip`, `opt-level = "z"`).

4. **Host-side fuel / epoch.** The wasm module runs inside wasmtime with fuel + epoch interruption
   (spec 11d). The tree-sitter grammar C code contains tight loops; ensure the fuel budget is
   generous enough for large files. Benchmark with a 10 kLOC input before finalising limits.

5. **No pure-Rust fallback.** Approach B is definitively blocked. If a platform arises where the
   WASI SDK clang cannot be run (e.g., non-x86_64 CI without WASI SDK binaries), a cross-compilation
   workaround is needed. Document this as a known limitation; do not silently weaken the Tree-sitter
   requirement (UN1).
