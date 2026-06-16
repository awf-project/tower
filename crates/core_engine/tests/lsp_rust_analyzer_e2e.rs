//! End-to-end: drive the real rust-analyzer through `LspClientAdapter`.
//!
//! GATED: skipped (passes vacuously) when `rust-analyzer` is not on PATH, so
//! `cargo test --workspace` stays green on machines without it. Mirrors the
//! ast_e2e gating on a present wasm artifact.

use std::path::PathBuf;

use core_engine::adapters::lsp::LspClientAdapter;
use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::Position;
use core_engine::ports::{CodeIntelligencePort, NavigationPort};

fn rust_analyzer_present() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A minimal cargo project on disk so rust-analyzer has a workspace to load.
fn scaffold_project(broken: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let body = if broken {
        "fn main() { let _ = does_not_exist(); }\n"
    } else {
        "fn main() {}\n"
    };
    std::fs::write(dir.path().join("src/main.rs"), body).unwrap();
    dir
}

#[test]
fn broken_rust_reports_an_error_diagnostic() {
    if !rust_analyzer_present() {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    }
    let dir = scaffold_project(true);
    let adapter = LspClientAdapter::spawn(
        "rust-analyzer",
        &[],
        vec!["rs".to_owned()],
        "rust".to_owned(),
        PathBuf::from(dir.path()),
        None,
    )
    .expect("spawn rust-analyzer");

    let text = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    // rust-analyzer needs time for its first cargo check; re-poll a few times.
    let mut diags = Vec::new();
    for _ in 0..6 {
        diags = adapter
            .check(&RelativePath::new("src/main.rs"), &text)
            .expect("check must not error");
        if !diags.is_empty() {
            break;
        }
    }
    assert!(
        diags
            .iter()
            .any(|d| { matches!(d.severity, core_engine::domain::code_intel::Severity::Error) }),
        "expected at least one error diagnostic from rust-analyzer; got {diags:?}"
    );
}

/// AC1: with the readiness gate, a single `check` (no manual re-poll) blocks
/// until rust-analyzer signals quiescent, then returns the error — proving the
/// fixed settle budget is gone.
#[test]
fn slow_first_check_returns_on_quiescent_without_repoll() {
    if !rust_analyzer_present() {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    }
    let dir = scaffold_project(true);
    let adapter = LspClientAdapter::spawn(
        "rust-analyzer",
        &[],
        vec!["rs".to_owned()],
        "rust".to_owned(),
        PathBuf::from(dir.path()),
        None,
    )
    .expect("spawn rust-analyzer");

    let text = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    // Single call — no for-loop re-poll. The ready gate must hold the call open
    // until diagnostics are authoritative.
    let diags = adapter
        .check(&RelativePath::new("src/main.rs"), &text)
        .expect("check must not error");
    assert!(
        diags
            .iter()
            .any(|d| matches!(d.severity, core_engine::domain::code_intel::Severity::Error)),
        "expected an error diagnostic on the first (gated) check; got {diags:?}"
    );
}

/// A library crate whose `greet()` is defined in `src/thing.rs` and called from
/// `src/lib.rs`, giving rust-analyzer a genuine cross-file resolution to make.
fn scaffold_cross_file_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"navprobe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    // lib.rs: line 3 char 12 sits on the `greet` identifier of the call site.
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "mod thing;\n\npub fn run() {\n    thing::greet();\n}\n",
    )
    .unwrap();
    // thing.rs: line 0 char 8 sits on the `greet` identifier of the definition.
    std::fs::write(
        dir.path().join("src/thing.rs"),
        "/// Greets.\npub fn greet() {}\n",
    )
    .unwrap();
    dir
}

fn spawn_nav(dir: &tempfile::TempDir) -> LspClientAdapter {
    LspClientAdapter::spawn(
        "rust-analyzer",
        &[],
        vec!["rs".to_owned()],
        "rust".to_owned(),
        PathBuf::from(dir.path()),
        None,
    )
    .expect("spawn rust-analyzer")
}

/// AC1: go-to-definition from a use site in `lib.rs` resolves to `thing.rs`.
#[test]
fn definition_resolves_across_files() {
    if !rust_analyzer_present() {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    }
    let dir = scaffold_cross_file_project();
    let adapter = spawn_nav(&dir);
    let lib = RelativePath::new("src/lib.rs");
    let text = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    let call_site = Position {
        line: 3,
        character: 12,
    };

    let mut locs = Vec::new();
    for _ in 0..10 {
        locs = adapter
            .definition(&lib, &text, call_site)
            .expect("definition");
        if !locs.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(
        locs.iter().any(|l| l.path.as_str() == "src/thing.rs"),
        "definition must resolve to src/thing.rs (cross-file); got {locs:?}"
    );
}

/// AC2: find-references on the definition returns the call site in `lib.rs`.
#[test]
fn references_include_cross_file_use_site() {
    if !rust_analyzer_present() {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    }
    let dir = scaffold_cross_file_project();
    let adapter = spawn_nav(&dir);
    let thing = RelativePath::new("src/thing.rs");
    let text = std::fs::read_to_string(dir.path().join("src/thing.rs")).unwrap();
    let def_site = Position {
        line: 1,
        character: 8,
    };

    let mut refs = Vec::new();
    for _ in 0..10 {
        refs = adapter
            .references(&thing, &text, def_site)
            .expect("references");
        if refs.iter().any(|l| l.path.as_str() == "src/lib.rs") {
            break;
        }
    }
    assert!(
        refs.iter().any(|l| l.path.as_str() == "src/lib.rs"),
        "references must include the use site in src/lib.rs; got {refs:?}"
    );
}

/// AC3: hover on the call site returns non-empty contents.
#[test]
fn hover_returns_contents() {
    if !rust_analyzer_present() {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    }
    let dir = scaffold_cross_file_project();
    let adapter = spawn_nav(&dir);
    let lib = RelativePath::new("src/lib.rs");
    let text = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    let call_site = Position {
        line: 3,
        character: 12,
    };

    let mut hover = None;
    for _ in 0..10 {
        hover = adapter.hover(&lib, &text, call_site).expect("hover");
        if hover.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let hover = hover.expect("hover must return contents for a resolved symbol");
    assert!(
        !hover.contents.is_empty(),
        "hover contents must be non-empty"
    );
}
