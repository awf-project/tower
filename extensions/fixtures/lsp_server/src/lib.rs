//! Shared test support for locating the prebuilt native LSP fixture.

use std::path::PathBuf;

/// Locate the fixture beside workspace binaries, including custom target dirs.
///
/// # Panics
/// Panics if the executable cannot be located or the fixture has not been built.
pub fn binary_path() -> PathBuf {
    let executable = std::env::current_exe().expect("locate test executable");
    let directory = executable.parent().expect("test executable directory");
    let name = format!("fixture_lsp_server{}", std::env::consts::EXE_SUFFIX);
    // Cargo runs test executables below the profile's build/ (or deps/ on
    // older toolchains). The prebuilt fixture lives at the profile root.
    let binary = directory
        .ancestors()
        .filter(|dir| {
            dir.file_name()
                .is_some_and(|part| part == "build" || part == "deps")
        })
        .filter_map(|dir| dir.parent())
        .map(|profile| profile.join(&name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| directory.join(name));
    assert!(
        binary.is_file(),
        "build native LSP fixture first: cargo build -p fixture_lsp_server --bins ({})",
        binary.display()
    );
    binary
}
