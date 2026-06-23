//! Engine-builder smoke test (Task 6).
#![forbid(unsafe_code)]

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::TowerConfig;
use core_engine::adapters::daemon::engine::build_engine;
use tempfile::tempdir;

#[test]
fn build_engine_indexes_a_fresh_workspace() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"pub fn a() {}").unwrap();
    std::fs::write(dir.path().join("b.rs"), b"pub fn b() {}").unwrap();

    let opts = GlobalOpts {
        workspace_dir: Some(dir.path().to_path_buf()),
        extensions_dir: None,
    };
    let handle = build_engine(&opts, TowerConfig::default()).expect("engine builds");

    let ws = handle.state.read().unwrap();
    let count = ws.workspace_arc().read().unwrap().all_file_ids().len();
    assert_eq!(count, 2, "two source files indexed");
}
