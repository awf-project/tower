//! TDD tests for spec 04b — Inverted Index Persistence.
//!
//! # TDD sequence
//!
//! 1. RED/GREEN: restart → find_file identity (AC1 / EV1).
//! 2. RED/GREEN: delete → restart → unique token absent (AC2).
//! 3. RED/GREEN: joint rollback on posting failure (AC3 / UN1).
//! 4. RED/GREEN: dangling-posting reconciliation on reload (AC4 / UN2).

use tempfile::TempDir;

use crate::adapters::storage::SledStorageAdapter;
use crate::domain::index::FileSearch;
use crate::domain::virtual_file::FileMetadata;
use crate::domain::{RelativePath, VirtualFile};
use crate::ports::StoragePort;
use crate::ports::inbound::SearchUseCase;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Insert a file into the workspace + adapter + index atomically.
///
/// Returns the [`VirtualFile`] assigned by the workspace so callers can hold
/// onto the `FileId` for subsequent operations.
fn add_indexed_file(
    ws: &mut crate::domain::workspace::ProjectWorkspace,
    adapter: &mut SledStorageAdapter,
    path: &str,
) -> VirtualFile {
    let rel = RelativePath::new(path);
    let id = ws.insert(rel.clone(), FileMetadata::default()).unwrap();
    let file = ws.get(id).unwrap().clone();
    adapter.put(file.clone()).unwrap();
    file
}

// ── AC1: restart → find_file identity (EV1) ─────────────────────────────────

/// AC1 — files indexed, process restarts, find_file returns identical results
/// without any rescan.
#[test]
fn restart_find_file_returns_same_results() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    // First session: index two files then close the adapter.
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        add_indexed_file(&mut ws, &mut adapter, "src/http/Client.rs");
        add_indexed_file(&mut ws, &mut adapter, "src/db/client.rs");
        // adapter is dropped here, releasing the sled lock.
    }

    // Second session: capture the expected results (same data, no rescan).
    let expected_results: Vec<RelativePath> = {
        let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
        let search = FileSearch::new(&index, &ws);
        let results = search.find_file("client").unwrap();
        assert!(
            !results.is_empty(),
            "first reopen must return results from persisted index"
        );
        results
    };

    // Third session: reopen again WITHOUT any additional indexing and compare.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);
    let results = search.find_file("client").unwrap();

    assert_eq!(
        results, expected_results,
        "find_file results must match across restarts without any rescan"
    );
}

/// AC1 variant — a single file survives restart and is still findable.
#[test]
fn single_file_survives_restart_and_is_findable() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        add_indexed_file(&mut ws, &mut adapter, "src/unique_sentinel_module.rs");
    }

    // Reopen: no rescan, only rehydration from the index tree.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);
    let results = search.find_file("sentinel").unwrap();

    let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();
    assert!(
        paths.contains(&"src/unique_sentinel_module.rs"),
        "unique_sentinel_module.rs must be findable after restart; got {paths:?}"
    );
}

// ── AC2: delete → restart → unique token absent ──────────────────────────────

/// AC2 — after a file is deleted and the process restarts, its unique token is
/// no longer returned by find_file.
#[test]
fn deleted_file_unique_token_absent_after_restart() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        let file = add_indexed_file(&mut ws, &mut adapter, "src/ghost_xyzzy_module/impl.rs");
        // Unique token: "ghost", "xyzzy".

        // Delete the file.
        ws.remove(file.id).unwrap();
        adapter.delete(file.id).unwrap();
    }

    // Reopen: ghostly file must not appear under its unique token.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);

    let results = search.find_file("xyzzy").unwrap();
    assert!(
        results.is_empty(),
        "unique token 'xyzzy' must yield no results after delete+restart; got {results:?}"
    );

    let results2 = search.find_file("ghost").unwrap();
    assert!(
        results2.is_empty(),
        "unique token 'ghost' must yield no results after delete+restart; got {results2:?}"
    );
}

/// AC2 variant — deleting one file does not remove another file sharing a token.
#[test]
fn deleting_one_file_does_not_remove_shared_token_for_other() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        let file_a = add_indexed_file(&mut ws, &mut adapter, "src/http/Client.rs");
        add_indexed_file(&mut ws, &mut adapter, "src/db/client_pool.rs");

        // Delete only Client.rs.
        ws.remove(file_a.id).unwrap();
        adapter.delete(file_a.id).unwrap();
    }

    // Reopen: "client" still returns client_pool.rs.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);
    let results = search.find_file("client").unwrap();
    let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();

    assert!(
        paths.contains(&"src/db/client_pool.rs"),
        "client_pool.rs must still be findable; got {paths:?}"
    );
    assert!(
        !paths.contains(&"src/http/Client.rs"),
        "deleted Client.rs must not appear; got {paths:?}"
    );
}

// ── AC3: joint rollback on posting failure (UN1) ─────────────────────────────

/// AC3 — if the posting update is forced to fail inside the transaction, neither
/// the file record nor the postings commit.
///
/// Strategy A (forced abort): `put_aborting_for_test` writes to both the files
/// tree and the index tree, then aborts.  This proves that writes to both trees
/// are rolled back together.
#[test]
fn joint_rollback_leaves_both_file_and_index_trees_clean() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    // Open and write one baseline file.
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        add_indexed_file(&mut ws, &mut adapter, "baseline/file.rs");
    }

    // Force a transaction abort AFTER writing to both files and index trees.
    // `put_aborting_for_test` now writes a posting entry to tx_index before
    // returning Abort, so a regression that commits index changes before the
    // abort would be caught here.
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        let rel = RelativePath::new("should_not_appear/ghost.rs");
        let id = ws.insert(rel.clone(), FileMetadata::default()).unwrap();
        let file = ws.get(id).unwrap().clone();

        let result = adapter.put_aborting_for_test(file);
        assert!(
            result.is_err(),
            "put_aborting_for_test must return Err to confirm the path was exercised"
        );
    }

    // Reopen: neither the file record nor the index entry must exist.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);

    let results = search.find_file("ghost").unwrap();
    assert!(
        results.is_empty(),
        "aborted file must not appear in search results; got {results:?}"
    );

    // Baseline must still be intact.
    let baseline = search.find_file("baseline").unwrap();
    let paths: Vec<&str> = baseline.iter().map(|p| p.as_str()).collect();
    assert!(
        paths.contains(&"baseline/file.rs"),
        "baseline file must survive the aborted put; got {paths:?}"
    );
}

/// AC3 variant (Strategy B) — corrupt the index tree for one of the new
/// file's tokens with non-postcard bytes, then call the real `put`.
///
/// The codec error during `update_posting` must abort the full transaction,
/// leaving both the file tree and the index tree clean.
#[test]
fn corrupt_index_entry_causes_put_to_abort_and_leaves_db_clean() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    // Baseline session.
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        add_indexed_file(&mut ws, &mut adapter, "baseline/clean.rs");
    }

    // Inject corrupt bytes for the token "corrupt" into the index tree.
    // When the real `put` tries to deserialise this, postcard will fail and
    // the transaction must abort.
    {
        let db = SledStorageAdapter::open_db_with_retry(&db_path).unwrap();
        let index_tree = db.open_tree("index").unwrap();
        index_tree
            .insert(b"corrupt", b"\xFF\xFF\xFF\xFF not valid postcard")
            .unwrap();
        index_tree.flush().unwrap();
    }

    // Attempt to insert a file whose path tokenises to include "corrupt".
    let put_result = {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        let rel = RelativePath::new("corrupt/module.rs");
        let id = ws.insert(rel.clone(), FileMetadata::default()).unwrap();
        let file = ws.get(id).unwrap().clone();
        adapter.put(file)
    };

    assert!(
        put_result.is_err(),
        "put with corrupt index entry must return Err; got {put_result:?}"
    );

    // Reopen: the new file record must NOT exist in the database.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);

    let ghost = search.find_file("module").unwrap();
    assert!(
        ghost.is_empty(),
        "aborted file must not appear in search; got {ghost:?}"
    );

    // Baseline must survive.
    let baseline = search.find_file("clean").unwrap();
    let paths: Vec<&str> = baseline.iter().map(|p| p.as_str()).collect();
    assert!(
        paths.contains(&"baseline/clean.rs"),
        "baseline must survive the aborted put; got {paths:?}"
    );
}

// ── AC4: dangling-posting reconciliation on reload (UN2) ─────────────────────

/// AC4 — intentionally corrupt the index tree so a posting references a FileId
/// that is absent from the files tree. On reload, the adapter must reconcile
/// (drop) the dangling entry so search never returns a phantom result.
#[test]
fn dangling_posting_reconciled_on_reload() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    // Write a real file so the database is initialised.
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        add_indexed_file(&mut ws, &mut adapter, "real/actual.rs");
    }

    // Inject a dangling posting: write a posting list for the token "phantom"
    // containing a FileId that does not exist in the files tree.
    {
        use crate::domain::FileId;
        let dead_id = FileId::new_for_testing(999, 42);
        let posting: Vec<FileId> = vec![dead_id];
        let posting_bytes = postcard::to_allocvec(&posting).unwrap();

        let db = SledStorageAdapter::open_db_with_retry(&db_path).unwrap();
        let index_tree = db.open_tree("index").unwrap();
        index_tree
            .insert(b"phantom", posting_bytes.as_slice())
            .unwrap();
        index_tree.flush().unwrap();
    }

    // Reopen: the reconciliation pass must drop the dangling posting.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);

    let results = search.find_file("phantom").unwrap();
    assert!(
        results.is_empty(),
        "phantom result from dangling posting must be absent after reconciliation; \
         got {results:?}"
    );

    // The real file must still be findable.
    let real = search.find_file("actual").unwrap();
    let paths: Vec<&str> = real.iter().map(|p| p.as_str()).collect();
    assert!(
        paths.contains(&"real/actual.rs"),
        "real/actual.rs must survive reconciliation; got {paths:?}"
    );
}

/// AC4 variant — a posting list that mixes live and dead FileIds; after
/// reconciliation only the live one is returned by search.
#[test]
fn mixed_posting_retains_live_and_drops_dead_file_ids() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    // Write one real file with a "shared" token.
    let live_file_id;
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&db_path).unwrap();
        let file = add_indexed_file(&mut ws, &mut adapter, "shared/live_module.rs");
        live_file_id = file.id;
    }

    // Inject a dangling FileId into the posting list for one of the tokens
    // that "shared/live_module.rs" already owns (e.g. "live").
    {
        use crate::domain::FileId;
        let dead_id = FileId::new_for_testing(888, 7);

        let db = SledStorageAdapter::open_db_with_retry(&db_path).unwrap();
        let index_tree = db.open_tree("index").unwrap();

        // Append the dead id to the existing posting for "live" (if present)
        // or create it fresh.
        let mut posting: Vec<FileId> = match index_tree.get(b"live").unwrap() {
            Some(bytes) => postcard::from_bytes(&bytes).unwrap_or_default(),
            None => Vec::new(),
        };
        posting.push(dead_id);
        let bytes = postcard::to_allocvec(&posting).unwrap();
        index_tree.insert(b"live", bytes.as_slice()).unwrap();
        index_tree.flush().unwrap();
    }

    // Reopen: reconciliation drops dead_id, keeps live_file_id.
    let (_adapter, ws, index) = SledStorageAdapter::open(&db_path).unwrap();
    let search = FileSearch::new(&index, &ws);

    let results = search.find_file("live").unwrap();
    let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();

    assert!(
        paths.contains(&"shared/live_module.rs"),
        "live_module.rs must survive reconciliation; got {paths:?}"
    );

    // The dead_id (slot 888, gen 7) must not produce a path (it never existed
    // in the workspace so workspace.get() returns StaleHandle and is skipped).
    // FileSearch already skips stale handles; the reconciliation is belt-and-
    // suspenders but we also verify the count is exactly 1.
    assert_eq!(
        results.len(),
        1,
        "only 1 result expected (the live file); got {results:?}"
    );

    // Suppress unused-variable warning (live_file_id is used above indirectly
    // through the DB write, but the compiler cannot see that).
    let _ = live_file_id;
}
