//! `plugin_ast` — reference Tree-sitter plugin, compiled to wasm32-wasip1.
//!
//! Stub entrypoint until spec 12c implements `ast_get_outline`. Target-agnostic
//! so it builds for both the host (spec AC1/AC2) and wasm32-wasip1 (spec AC4).

fn main() {}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_smoke() {
        // Proves the test harness compiles and runs for this crate (host target).
        assert_eq!(ast_smoke(), 4);
    }

    fn ast_smoke() -> u32 {
        2 + 2
    }
}
