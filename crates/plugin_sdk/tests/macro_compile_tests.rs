//! Trybuild compile-pass / compile-fail tests for the plugin_sdk macros.
//!
//! These tests compile the files in `tests/compile_pass/` and
//! `tests/compile_fail/` using the full `plugin_sdk` crate as a dependency.
//! This proves the macro-generated code is syntactically and type-correct.

#[test]
fn macro_compile_tests() {
    let t = trybuild::TestCases::new();
    t.pass("tests/compile_pass/*.rs");
    t.compile_fail("tests/compile_fail/*.rs");
}
