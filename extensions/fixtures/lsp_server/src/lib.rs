//! Shared test support for locating the prebuilt native LSP fixture.

use std::path::PathBuf;

/// Locate the fixture beside workspace binaries, including custom target dirs.
///
/// # Panics
/// Panics if the executable cannot be located or the fixture has not been built.
pub fn binary_path() -> PathBuf {
    let executable = std::env::current_exe().expect("locate test executable");
    let directory = executable.parent().expect("test executable directory");
    let directory = if directory.file_name().is_some_and(|name| name == "deps") {
        directory.parent().expect("Cargo profile directory")
    } else {
        directory
    };
    let binary = directory.join(format!(
        "fixture_lsp_server{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        binary.is_file(),
        "build native LSP fixture first: cargo build -p fixture_lsp_server --bins ({})",
        binary.display()
    );
    binary
}
