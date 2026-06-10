//! `tower` binary — MCP stdio server (spec 10b, closes Milestone 3).
//!
//! # Startup sequence
//!
//! 1. Resolve workspace root: `--workspace-dir <path>` flag or `$TOWER_WORKSPACE`
//!    env var, falling back to the current working directory.
//! 2. Open the sled database from `<root>/.tower/db`.
//!    `SledStorageAdapter::open` reconstructs the [`ProjectWorkspace`] and
//!    [`InvertedIndex`] from persisted state (spec 04a/04b).
//! 3. Run the initial workspace scan if one has never completed
//!    (`StoragePort::is_scan_complete`).
//! 4. Wrap all state in an `Arc<RwLock<EngineState>>` for deadlock-free sharing
//!    with any future background watcher thread (spec 06 lock discipline).
//! 5. Register the 7 native `vfs_*` tools into a `NativeToolRegistry`.
//! 6. Start the 10a `serve` loop over real `stdin` / `stdout`.
//!
//! # Wiring decision: `Arc<RwLock<EngineState>>`
//!
//! The spec requires that tool handlers and the filesystem watcher (spec 06)
//! share workspace/index/storage/fs without copying. `Arc` provides shared
//! ownership; `RwLock` allows concurrent readers (e.g. simultaneous
//! `vfs_find_file` and `vfs_search_text`) with exclusive mutation for writers
//! (create/delete/global_replace). Short critical sections only — no blocking
//! I/O is performed while holding the lock.
//!
//! # Error handling
//!
//! Startup failures print a human-readable message to stderr and exit with
//! code 1. The serve loop only returns on unrecoverable I/O (broken pipe);
//! malformed frames and tool errors are returned as JSON-RPC error responses
//! and the loop continues.

use std::env;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use core_engine::adapters::fs::{workspace_scan, RealFs};
use core_engine::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
use core_engine::adapters::mcp::serve;
use core_engine::adapters::SledStorageAdapter;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::StoragePort;

fn main() {
    if let Err(e) = run() {
        eprintln!("tower: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    // ── Step 1: resolve workspace root ────────────────────────────────────────
    let workspace_root = resolve_workspace_root();

    // ── Step 2: open sled database ────────────────────────────────────────────
    let db_path = workspace_root.join(".tower").join("db");
    std::fs::create_dir_all(&db_path)
        .map_err(|e| format!("failed to create db dir {}: {e}", db_path.display()))?;

    let (mut storage, workspace, index) = SledStorageAdapter::open(&db_path)
        .map_err(|e| format!("failed to open sled database at {}: {e}", db_path.display()))?;

    // ── Step 3: initial workspace scan if never completed ─────────────────────
    let (workspace, index) = if !storage.is_scan_complete().unwrap_or(false) {
        let mut fresh_workspace = ProjectWorkspace::new();
        let mut fresh_index = InvertedIndex::new();
        match workspace_scan(
            &workspace_root,
            &mut storage,
            &mut fresh_workspace,
            &mut fresh_index,
        ) {
            Ok(report) => {
                eprintln!(
                    "tower: initial scan complete — {} files indexed",
                    report.indexed
                );
            }
            Err(e) => {
                eprintln!("tower: warning — initial scan failed: {e}");
            }
        }
        (fresh_workspace, fresh_index)
    } else {
        (workspace, index)
    };

    // ── Step 4: build shared engine state ─────────────────────────────────────
    let fs = RealFs::new(&workspace_root);
    let state = Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        Box::new(storage),
        Box::new(fs),
    )));

    // ── Step 5: register native tools ─────────────────────────────────────────
    let mut registry = NativeToolRegistry::new(Arc::clone(&state));

    // ── Step 6: serve loop over real stdin/stdout ──────────────────────────────
    // Lock stdin/stdout for the duration of the serve loop.
    // BufReader/BufWriter ensure line-oriented I/O matches the framing spec.
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(
        BufReader::new(stdin.lock()),
        BufWriter::new(stdout.lock()),
        &mut registry,
    )
    .map_err(|e| format!("serve loop I/O error: {e}"))
}

/// Resolve the workspace root directory.
///
/// Priority order (highest first):
/// 1. `--workspace-dir <path>` command-line argument.
/// 2. `TOWER_WORKSPACE` environment variable.
/// 3. Current working directory.
fn resolve_workspace_root() -> PathBuf {
    // Check for --workspace-dir <path> flag.
    let args: Vec<String> = env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--workspace-dir" {
            return PathBuf::from(&pair[1]);
        }
    }

    // Fall back to env var.
    if let Ok(val) = env::var("TOWER_WORKSPACE") {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }

    // Default: current working directory.
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
