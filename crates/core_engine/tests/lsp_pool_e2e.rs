//! Gated multi-server pool e2e (spec 14d AC1, AC4).
//!
//! Tests in this file require real language server binaries on PATH.
//! Each test checks for its binary and returns early (vacuous pass) when absent,
//! keeping `cargo test --workspace` green on machines without a full toolchain.
//!
//! AC1: two languages, two independent sessions, each routes correctly.
//! AC4: crash a real session via `kill_for_test`; next `check` restarts it;
//!      the host (SessionPool) and the test process both survive.

use core_engine::adapters::config::lsp::{LspConfig, LspServerConfig};
use core_engine::adapters::lsp::pool::SessionPool;
use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::Severity;
use core_engine::ports::CodeIntelligencePort;
use std::collections::BTreeMap;

// ── Binary presence probes ────────────────────────────────────────────────────

fn binary_present(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn rust_analyzer_present() -> bool {
    binary_present("rust-analyzer")
}

fn gopls_present() -> bool {
    binary_present("gopls")
}

// ── Project scaffolding ───────────────────────────────────────────────────────

fn scaffold_rust_project(broken: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"e2e_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let body = if broken {
        // Type error: cannot assign &str to u8
        "fn main() { let _x: u8 = \"type error\"; }\n"
    } else {
        "fn main() {}\n"
    };
    std::fs::write(dir.path().join("src/main.rs"), body).unwrap();
    dir
}

fn scaffold_go_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // Minimal go module
    std::fs::write(dir.path().join("go.mod"), "module e2e_probe\n\ngo 1.21\n").unwrap();
    // Clean Go file (no deliberate error: gopls is slow to start on CI and
    // returning empty diagnostics is sufficient to prove the session was spawned)
    std::fs::write(
        dir.path().join("main.go"),
        "package main\n\nfunc main() {}\n",
    )
    .unwrap();
    dir
}

// ── Config builders ───────────────────────────────────────────────────────────

fn make_config(entries: &[(&str, &str, &[&str])]) -> LspConfig {
    let mut servers = BTreeMap::new();
    for &(lang, cmd, exts) in entries {
        servers.insert(
            lang.to_owned(),
            LspServerConfig {
                command: cmd.to_owned(),
                extensions: exts.iter().map(|e| (*e).to_owned()).collect(),
                args: vec![],
            },
        );
    }
    LspConfig {
        servers,
        idle_timeout: None,
    }
}

// ── AC1: concurrent sessions for two languages ────────────────────────────────
//
// When both rust-analyzer and gopls are present, the pool spawns two independent
// sessions and routes by extension. When only one is present, the test still
// exercises AC1 for the available language (the other side is vacuously skipped).

#[test]
fn two_languages_route_to_independent_sessions() {
    // This test exercises multi-language routing. It requires at least
    // rust-analyzer to be meaningful. When both are present the full AC1 path
    // runs; when only one is present we test with a single language pool.
    if !rust_analyzer_present() {
        eprintln!(
            "skipping two_languages_route_to_independent_sessions: rust-analyzer not on PATH"
        );
        return;
    }

    let rust_dir = scaffold_rust_project(true);

    if gopls_present() {
        // Full AC1: both languages in one pool, one workspace dir (use rust dir
        // as the pool root; Go files sit outside Cargo.toml scan scope).
        let go_dir = scaffold_go_project();

        let config = make_config(&[("rust", "rust-analyzer", &["rs"]), ("go", "gopls", &["go"])]);
        // Use the rust project root so rust-analyzer has a valid Cargo.toml.
        // The go session spawns with the same pool root (gopls scans from there).
        let pool = SessionPool::new(config, rust_dir.path().to_path_buf());

        // Rust session: broken file must yield an error diagnostic.
        let rs_rel = RelativePath::new("src/main.rs");
        let rs_text = std::fs::read_to_string(rust_dir.path().join("src/main.rs")).unwrap();
        let rs_diags = pool
            .check(&rs_rel, &rs_text)
            .expect("rust check must not error");
        assert!(
            rs_diags
                .iter()
                .any(|d| matches!(d.severity, Severity::Error)),
            "expected an error diagnostic from rust-analyzer; got {rs_diags:?}"
        );

        // Go session: route a .go path — spawns gopls; may return empty diags
        // for a clean file. What matters is no error from the pool.
        // We point at a file path relative to the workspace root — gopls won't
        // find go.mod there, but the session still spawns and returns (possibly
        // empty) diagnostics without panicking.
        let go_rel = RelativePath::new("main.go");
        let go_text = std::fs::read_to_string(go_dir.path().join("main.go")).unwrap();
        let _go_diags = pool
            .check(&go_rel, &go_text)
            .expect("go check must not error");

        // Confirm each language was spawned independently via serves().
        assert!(
            pool.serves(&RelativePath::new("lib.rs")),
            ".rs must be served"
        );
        assert!(
            pool.serves(&RelativePath::new("lib.go")),
            ".go must be served"
        );
        assert!(
            !pool.serves(&RelativePath::new("index.ts")),
            ".ts must not be served when unconfigured"
        );
    } else {
        // Rust-only path: still tests AC1 routing for the configured language.
        eprintln!("gopls not on PATH; running AC1 with rust-only config");

        let config = make_config(&[("rust", "rust-analyzer", &["rs"])]);
        let pool = SessionPool::new(config, rust_dir.path().to_path_buf());

        let rel = RelativePath::new("src/main.rs");
        let text = std::fs::read_to_string(rust_dir.path().join("src/main.rs")).unwrap();
        let diags = pool.check(&rel, &text).expect("check must not error");
        assert!(
            diags.iter().any(|d| matches!(d.severity, Severity::Error)),
            "expected an error diagnostic from rust-analyzer; got {diags:?}"
        );

        assert!(pool.serves(&RelativePath::new("lib.rs")));
        assert!(!pool.serves(&RelativePath::new("lib.go")));
    }
}

// ── AC4: crash restart — host survives ────────────────────────────────────────
//
// Simulate a crash by calling `kill_for_test()` on the live session. The next
// `pool.check()` must detect `is_dead() == true`, evict the entry, spawn a
// fresh session, and return without error.

#[test]
fn crash_restart_host_stays_up() {
    if !rust_analyzer_present() {
        eprintln!("skipping crash_restart_host_stays_up: rust-analyzer not on PATH");
        return;
    }

    let dir = scaffold_rust_project(false);
    let config = make_config(&[("rust", "rust-analyzer", &["rs"])]);
    let pool = SessionPool::new(config, dir.path().to_path_buf());

    let rel = RelativePath::new("src/main.rs");
    let text = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();

    // First check: spawns the rust-analyzer session.
    let _ = pool.check(&rel, &text).expect("first check must succeed");

    // Kill the REAL OS process (not a flag flip): this closes its stdout, so the
    // session's reader thread hits EOF and sets dead_flag through the production
    // crash-*detection* path — exactly what AC4's "process killed externally" means.
    let found = pool.kill_session_for_test("rust");
    assert!(found, "rust session must be present after first check");

    // Wait for the reader-thread EOF → dead_flag chain to mark the session dead.
    // This asserts the detection itself works, not merely the restart-on-flag.
    let mut detected = false;
    for _ in 0..50 {
        if pool.session_is_dead_for_test("rust") == Some(true) {
            detected = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        detected,
        "reader thread must detect process death and set dead_flag within ~5s"
    );

    // Next check: pool detects is_dead() == true, evicts, re-spawns.
    // The host (SessionPool) must survive and return Ok.
    let result = pool.check(&rel, &text);
    assert!(
        result.is_ok(),
        "after crash restart, check must succeed; got {result:?}"
    );
}
