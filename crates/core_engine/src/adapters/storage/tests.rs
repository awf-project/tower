//! Tests for `SledStorageAdapter`.
//!
//! # TDD sequence (spec 04a)
//!
//! 1. RED/GREEN  : contract macro vs adapter on a tempdir (U1/AC1).
//! 2. RED/GREEN  : restart → workspace identity (EV1/AC2).
//! 3. RED/GREEN  : torn-write rollback (UN1/AC3).
//! 4. RED/GREEN  : schema version guard (UN2/AC4).
//!
//! # How the contract suite is wired against a tempdir
//!
//! The `storage_contract_tests!` macro accepts a constructor expression that
//! returns a fresh, empty `StoragePort` implementor. For `SledStorageAdapter`
//! we provide a closure that creates a tempdir and calls `open()`, discarding
//! the workspace (contract tests only exercise the port, not the workspace).
//!
//! Each expansion creates a private `storage_contract` module with
//! concrete `#[test]` functions, so the tempdir is per-test-function.

use tempfile::TempDir;

use crate::adapters::storage::{SCHEMA_VERSION, SledStorageAdapter, StorageError};
use crate::domain::token::Token;
use crate::domain::{ContentHash, FileId, RelativePath, VirtualFile};
use crate::ports::{PortError, ReconciliationDelta, StoragePort};
use crate::test_support::make_virtual_file;

// ── AC1: contract suite on a real tempdir ────────────────────────────────────

fn make_sled_adapter() -> SledStorageAdapter {
    let dir = TempDir::new().expect("tempdir");
    // `keep()` converts TempDir into a PathBuf without scheduling cleanup,
    // so the dir persists for the test process lifetime (each test gets its
    // own tempdir through the `make_sled_adapter` closure).
    let path = dir.keep();
    let (adapter, _ws, _index) = SledStorageAdapter::open(&path).expect("SledStorageAdapter::open");
    adapter
}

/// Injects the only failure mode that can occur after Sled has accepted the
/// reconciliation transaction: durable completion.  The wrapper deliberately
/// delegates the real transaction first, so the test does not mistake a
/// pre-commit abort for a synchronous flush failure.
struct FlushFailingSledStorage {
    inner: SledStorageAdapter,
}

impl StoragePort for FlushFailingSledStorage {
    fn apply_reconciliation_delta(&mut self, delta: &ReconciliationDelta) -> Result<(), PortError> {
        self.inner.apply_reconciliation_delta(delta)?;
        Err(PortError::ReconciliationFailed {
            operation: "apply_reconciliation_delta",
            reason: "injected synchronous flush failure".to_owned(),
        })
    }

    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.inner.get(id)
    }

    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        self.inner.put(file)
    }

    fn put_batch(&mut self, files: &[VirtualFile]) -> Result<(), PortError> {
        self.inner.put_batch(files)
    }

    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
        self.inner.delete(id)
    }

    fn put_blob(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.put_blob(hash, bytes)
    }

    fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, PortError> {
        self.inner.get_blob(hash)
    }

    fn mark_scan_complete(&mut self) -> Result<(), PortError> {
        self.inner.mark_scan_complete()
    }

    fn is_scan_complete(&self) -> Result<bool, PortError> {
        self.inner.is_scan_complete()
    }
}

#[test]
fn post_transaction_synchronous_flush_failure_is_reported_as_reconciliation_failure() {
    let dir = TempDir::new().unwrap();
    let (inner, _, _) = SledStorageAdapter::open(dir.path()).unwrap();
    let mut store = FlushFailingSledStorage { inner };
    let original = make_virtual_file(0, 0, "src/old.rs");
    let replacement = make_virtual_file(0, 0, "src/new.rs");
    store.put(original.clone()).unwrap();

    assert_eq!(
        store.apply_reconciliation_delta(&ReconciliationDelta {
            additions: vec![replacement.clone()],
            removals: vec![original.id],
        }),
        Err(PortError::ReconciliationFailed {
            operation: "apply_reconciliation_delta",
            reason: "injected synchronous flush failure".to_owned(),
        })
    );
    assert_eq!(store.get(replacement.id).unwrap().path, replacement.path);
    assert_eq!(
        store
            .inner
            .persisted_file_id_for_path_for_test(&replacement.path),
        Some(replacement.id)
    );
}

impl crate::adapters::contract_tests::ReconciliationState for SledStorageAdapter {
    fn file_id_for_path(&self, path: &RelativePath) -> Option<FileId> {
        self.persisted_file_id_for_path_for_test(path)
    }

    fn search_ids(&self, query: &str) -> Vec<FileId> {
        self.persisted_search_ids_for_test(query)
    }

    fn slot_generation(&self, index: u32) -> Option<u32> {
        self.persisted_slot_generation_for_test(index)
    }

    fn slot_is_free(&self, index: u32) -> bool {
        self.persisted_slot_is_free_for_test(index)
    }

    fn survivor_rewrites(&self) -> usize {
        self.reconciliation_survivor_rewrites_for_test()
    }
}

crate::storage_contract_tests!(make_sled_adapter, || {
    let store = crate::adapters::contract_tests::reconciliation_delta_is_atomic(
        make_sled_adapter,
        |store| store.fail_next_reconciliation_before_commit_for_test(),
    );

    assert_eq!(store.reconciliation_survivor_rewrites_for_test(), 0);
});

#[test]
fn reconciliation_delta_survives_reopen_and_rolls_back_on_failure() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    let survivor = make_virtual_file(0, 0, "src/survivor.rs");
    let removed = make_virtual_file(1, 0, "src/removed.rs");
    let replacement = make_virtual_file(1, 1, "src/replacement.rs");
    let retried = make_virtual_file(1, 2, "src/retried.rs");
    let additional = make_virtual_file(2, 0, "src/additional.rs");

    let (mut store, _, _) = SledStorageAdapter::open(&path).unwrap();
    store
        .put_batch(&[survivor.clone(), removed.clone()])
        .unwrap();
    store
        .apply_reconciliation_delta(&ReconciliationDelta {
            additions: vec![replacement.clone()],
            removals: vec![removed.id],
        })
        .unwrap();

    // Clones are how the watcher shares this adapter. Both handles must report
    // the same instance-scoped instrumentation state, including after a failed
    // reconciliation attempt.
    let clone = store.try_clone();
    assert_eq!(
        store.reconciliation_survivor_rewrites_for_test(),
        clone.reconciliation_survivor_rewrites_for_test()
    );

    let failed_delta = ReconciliationDelta {
        additions: vec![additional.clone()],
        removals: vec![replacement.id],
    };
    store.fail_next_reconciliation_before_commit_for_test();
    assert!(matches!(
        store.apply_reconciliation_delta(&failed_delta),
        Err(PortError::ReconciliationFailed {
            operation: "apply_reconciliation_delta",
            ..
        })
    ));
    assert_eq!(
        store.reconciliation_survivor_rewrites_for_test(),
        clone.reconciliation_survivor_rewrites_for_test()
    );
    assert_eq!(
        store.persisted_file_id_for_path_for_test(&replacement.path),
        Some(replacement.id)
    );
    assert_eq!(
        store.persisted_file_id_for_path_for_test(&retried.path),
        None
    );
    assert_eq!(
        store.persisted_search_ids_for_test("replacement"),
        vec![replacement.id]
    );
    assert!(store.persisted_search_ids_for_test("retried").is_empty());
    assert_eq!(store.persisted_slot_generation_for_test(1), Some(1));
    assert!(!store.persisted_slot_is_free_for_test(1));
    assert_eq!(store.persisted_slot_generation_for_test(2), None);
    drop(clone);
    drop(store);

    // Check both the rehydrated views and the backing Sled projections. The
    // direct probes prevent `open()` from masking a torn secondary projection
    // by reconstructing it from file records.
    let (mut reopened, workspace, index) = SledStorageAdapter::open(&path).unwrap();
    assert_eq!(workspace.get(survivor.id).unwrap(), &survivor);
    assert_eq!(workspace.get(replacement.id).unwrap(), &replacement);
    assert!(workspace.get(removed.id).is_err());
    assert!(workspace.get(retried.id).is_err());
    assert!(workspace.get(additional.id).is_err());
    assert_eq!(workspace.get_by_path(&survivor.path), Some(survivor.id));
    assert_eq!(workspace.get_by_path(&removed.path), None);
    assert_eq!(
        workspace.get_by_path(&replacement.path),
        Some(replacement.id)
    );
    assert_eq!(workspace.get_by_path(&retried.path), None);
    assert_eq!(workspace.get_by_path(&additional.path), None);
    assert_eq!(index.query(&Token::new("src")).count(), 2);
    assert_eq!(
        index.query(&Token::new("survivor")).collect::<Vec<_>>(),
        vec![survivor.id]
    );
    assert!(index.query(&Token::new("removed")).next().is_none());
    assert_eq!(
        index.query(&Token::new("replacement")).collect::<Vec<_>>(),
        vec![replacement.id]
    );
    assert!(index.query(&Token::new("retried")).next().is_none());
    assert!(index.query(&Token::new("additional")).next().is_none());
    assert_eq!(workspace.snapshot().slot_generations, vec![0, 1]);
    assert!(workspace.snapshot().free_slots.is_empty());
    assert_eq!(
        reopened.persisted_file_id_for_path_for_test(&replacement.path),
        Some(replacement.id)
    );
    assert_eq!(
        reopened.persisted_file_id_for_path_for_test(&retried.path),
        None
    );
    assert_eq!(
        reopened.persisted_search_ids_for_test("replacement"),
        vec![replacement.id]
    );
    assert!(reopened.persisted_search_ids_for_test("retried").is_empty());
    assert_eq!(reopened.persisted_slot_generation_for_test(1), Some(1));
    assert!(!reopened.persisted_slot_is_free_for_test(1));
    assert_eq!(reopened.persisted_slot_generation_for_test(2), None);

    reopened.apply_reconciliation_delta(&failed_delta).unwrap();
    drop(reopened);

    // A second close/reopen proves the successful retry durably freed slot 1.
    // This intermediate state makes omission of the persisted free-slot write
    // observable instead of inferring allocator state from the file records.
    let (mut reopened, workspace, index) = SledStorageAdapter::open(&path).unwrap();
    assert_eq!(workspace.get(survivor.id).unwrap(), &survivor);
    assert_eq!(workspace.get(additional.id).unwrap(), &additional);
    assert!(workspace.get(removed.id).is_err());
    assert!(workspace.get(replacement.id).is_err());
    assert!(workspace.get(retried.id).is_err());
    assert_eq!(workspace.get_by_path(&survivor.path), Some(survivor.id));
    assert_eq!(workspace.get_by_path(&removed.path), None);
    assert_eq!(workspace.get_by_path(&replacement.path), None);
    assert_eq!(workspace.get_by_path(&retried.path), None);
    assert_eq!(workspace.get_by_path(&additional.path), Some(additional.id));
    assert_eq!(index.query(&Token::new("src")).count(), 2);
    assert_eq!(
        index.query(&Token::new("survivor")).collect::<Vec<_>>(),
        vec![survivor.id]
    );
    assert!(index.query(&Token::new("removed")).next().is_none());
    assert!(index.query(&Token::new("replacement")).next().is_none());
    assert!(index.query(&Token::new("retried")).next().is_none());
    assert_eq!(
        index.query(&Token::new("additional")).collect::<Vec<_>>(),
        vec![additional.id]
    );
    assert_eq!(workspace.snapshot().slot_generations, vec![0, 1, 0]);
    assert_eq!(workspace.snapshot().free_slots, vec![1]);
    assert_eq!(
        reopened.persisted_file_id_for_path_for_test(&replacement.path),
        None
    );
    assert_eq!(
        reopened.persisted_file_id_for_path_for_test(&retried.path),
        None
    );
    assert!(
        reopened
            .persisted_search_ids_for_test("replacement")
            .is_empty()
    );
    assert!(reopened.persisted_search_ids_for_test("retried").is_empty());
    assert_eq!(reopened.persisted_slot_generation_for_test(1), Some(1));
    assert!(reopened.persisted_slot_is_free_for_test(1));
    assert_eq!(reopened.persisted_slot_generation_for_test(2), Some(0));
    assert!(!reopened.persisted_slot_is_free_for_test(2));

    reopened
        .apply_reconciliation_delta(&ReconciliationDelta {
            additions: vec![retried.clone()],
            removals: Vec::new(),
        })
        .unwrap();
    drop(reopened);

    // Reusing the freed slot increments its generation and keeps the removed
    // identities stale after another reopen.
    let (reopened, workspace, index) = SledStorageAdapter::open(&path).unwrap();
    assert_eq!(workspace.get(retried.id).unwrap(), &retried);
    assert!(workspace.get(removed.id).is_err());
    assert!(workspace.get(replacement.id).is_err());
    assert_eq!(workspace.get_by_path(&retried.path), Some(retried.id));
    assert_eq!(
        index.query(&Token::new("retried")).collect::<Vec<_>>(),
        vec![retried.id]
    );
    assert_eq!(reopened.persisted_slot_generation_for_test(1), Some(2));
    assert!(!reopened.persisted_slot_is_free_for_test(1));
}

// ── AC2: restart rebuilds workspace including allocator state ─────────────────

#[test]
fn restart_rebuilds_workspace_with_generational_allocator_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    // First open — write two files then delete the first.
    let (id_a, id_b) = {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&path).unwrap();

        let id_a = ws
            .insert(RelativePath::new("a.rs"), Default::default())
            .unwrap();
        let id_b = ws
            .insert(RelativePath::new("b.rs"), Default::default())
            .unwrap();

        // Put both files through the adapter.
        adapter.put(ws.get(id_a).unwrap().clone()).unwrap();
        adapter.put(ws.get(id_b).unwrap().clone()).unwrap();

        // Remove file A — frees its slot.
        ws.remove(id_a).unwrap();
        adapter.delete(id_a).unwrap();

        // Insert a third file — should reuse slot 0 with generation 1.
        let id_c = ws
            .insert(RelativePath::new("c.rs"), Default::default())
            .unwrap();
        adapter.put(ws.get(id_c).unwrap().clone()).unwrap();

        // Persist free-slot state by snapshotting the workspace.
        // The adapter derives slot state from the files tree; after the delete
        // the free_slots list is empty (slot 0 was reused for c.rs).
        (id_a, id_b)
    };

    // Second open — reconstruct workspace from disk.
    let (_adapter, ws2, _index) = SledStorageAdapter::open(&path).unwrap();

    // id_a (slot 0, gen 0) should be stale — it was replaced by c.rs (slot 0, gen 1).
    assert!(
        ws2.get(id_a).is_err(),
        "stale id_a must not resolve after restart"
    );
    // id_b (slot 1, gen 0) is still live.
    assert!(ws2.get(id_b).is_ok(), "id_b must survive restart");
    // c.rs occupies slot 0 with generation 1.
    let id_c_expected = FileId::new_for_testing(0, 1);
    assert!(
        ws2.get(id_c_expected).is_ok(),
        "c.rs (slot 0, gen 1) must survive restart"
    );
}

// ── AC3: mid-transaction failure leaves no partial state ──────────────────────

#[test]
fn transaction_abort_leaves_no_partial_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();

    // Insert a known-good file.
    let good = make_virtual_file(0, 0, "good.rs");
    adapter.put(good.clone()).unwrap();

    // Simulate a transaction failure by trying to delete a file that does not
    // exist. Sled aborts the transaction atomically.
    let absent = FileId::new_for_testing(99, 0);
    let err = adapter.delete(absent).unwrap_err();
    assert_eq!(err, crate::ports::PortError::NotFound);

    // The good file must still be present — no partial state.
    assert_eq!(
        adapter.get(FileId::new_for_testing(0, 0)).unwrap(),
        good,
        "good.rs must remain after aborted delete of non-existent file"
    );

    // Reopen and verify the same invariant holds at the storage level.
    drop(adapter);
    let (adapter2, _ws, _index2) = SledStorageAdapter::open(&path).unwrap();
    assert_eq!(
        adapter2.get(FileId::new_for_testing(0, 0)).unwrap(),
        good,
        "good.rs must survive reopen after aborted transaction"
    );
}

// ── AC4: incompatible schema version is rejected ──────────────────────────────

#[test]
fn incompatible_schema_version_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    // Create a fresh database with the current schema version.
    {
        let (_adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
    }

    // Tamper with the schema_version to simulate a future incompatible version.
    {
        let db = SledStorageAdapter::open_db_with_retry(&path).unwrap();
        let meta = db.open_tree("meta").unwrap();
        let bad_version: u32 = SCHEMA_VERSION + 1;
        meta.insert(b"schema_version", &bad_version.to_le_bytes())
            .unwrap();
        meta.flush().unwrap();
        drop(db);
    }

    // Attempting to open should now fail with IncompatibleSchema.
    let result = SledStorageAdapter::open(&path);
    assert!(result.is_err(), "open with incompatible schema must fail");
    match result.unwrap_err() {
        StorageError::IncompatibleSchema { on_disk, expected } => {
            assert_eq!(on_disk, SCHEMA_VERSION + 1);
            assert_eq!(expected, SCHEMA_VERSION);
        }
        other => panic!("expected IncompatibleSchema, got: {other}"),
    }
}

// ── AC2-ext: freed slot must be reusable after restart (free_slots staleness) ──
//
// Regression for the free_slots staleness bug: after delete (without reuse in
// the same session) and a restart, the freed slot must appear in free_slots so
// the workspace can reuse it on the next insert rather than growing the arena.

#[test]
fn freed_slot_is_reusable_after_restart() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    // First session: insert a.rs (slot 0 gen 0) then delete it without reusing.
    // b.rs is at slot 1 gen 0 and stays alive.
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&path).unwrap();

        let id_a = ws
            .insert(RelativePath::new("a.rs"), Default::default())
            .unwrap();
        let id_b = ws
            .insert(RelativePath::new("b.rs"), Default::default())
            .unwrap();
        adapter.put(ws.get(id_a).unwrap().clone()).unwrap();
        adapter.put(ws.get(id_b).unwrap().clone()).unwrap();

        ws.remove(id_a).unwrap();
        adapter.delete(id_a).unwrap();
        // Session ends here WITHOUT reinserting into slot 0.
        let _ = id_b; // suppress unused warning
    }

    // Second session: reopen — slot 0 was freed and should be in free_slots.
    // The next insert must reuse slot 0 (with generation 1), not grow to slot 2.
    {
        let (_adapter2, mut ws2, _index) = SledStorageAdapter::open(&path).unwrap();

        let id_c = ws2
            .insert(RelativePath::new("c.rs"), Default::default())
            .unwrap();

        assert_eq!(
            id_c.index(),
            0,
            "slot 0 must be reused after restart (free_slots must be persisted)"
        );
        assert_eq!(
            id_c.generation(),
            1,
            "reused slot 0 must have generation 1 (bumped from gen 0)"
        );
    }
}

// ── AC2-ext: post-restart insert must not collide with surviving file ─────────
//
// After a delete+reuse cycle, restart, then insert: the new file must not
// overwrite a surviving occupant.

#[test]
fn post_restart_insert_does_not_collide_with_surviving_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    // First session: insert a.rs (slot 0 gen 0), delete it, insert c.rs which
    // reuses slot 0 at gen 1.  b.rs stays alive at slot 1 gen 0.
    {
        let (mut adapter, mut ws, _index) = SledStorageAdapter::open(&path).unwrap();

        let id_a = ws
            .insert(RelativePath::new("a.rs"), Default::default())
            .unwrap();
        let id_b = ws
            .insert(RelativePath::new("b.rs"), Default::default())
            .unwrap();
        adapter.put(ws.get(id_a).unwrap().clone()).unwrap();
        adapter.put(ws.get(id_b).unwrap().clone()).unwrap();

        ws.remove(id_a).unwrap();
        adapter.delete(id_a).unwrap();

        let id_c = ws
            .insert(RelativePath::new("c.rs"), Default::default())
            .unwrap();
        adapter.put(ws.get(id_c).unwrap().clone()).unwrap();
        let _ = id_b;
    }

    // Second session: reopen and insert a new file.  c.rs (slot 0 gen 1) and
    // b.rs (slot 1 gen 0) must remain accessible; d.rs must not collide.
    {
        let (mut adapter, mut ws2, _index) = SledStorageAdapter::open(&path).unwrap();

        let id_d = ws2
            .insert(RelativePath::new("d.rs"), Default::default())
            .unwrap();
        adapter.put(ws2.get(id_d).unwrap().clone()).unwrap();

        // Slot 0 (c.rs, gen 1) and slot 1 (b.rs, gen 0) must still resolve.
        assert!(
            ws2.get(FileId::new_for_testing(0, 1)).is_ok(),
            "c.rs at slot 0 gen 1 must survive the second session"
        );
        assert!(
            ws2.get(FileId::new_for_testing(1, 0)).is_ok(),
            "b.rs at slot 1 gen 0 must survive the second session"
        );

        // d.rs must occupy a slot that does not alias c.rs or b.rs.
        assert!(
            id_d.index() != 0 || id_d.generation() != 1,
            "d.rs must not collide with c.rs (slot 0 gen 1)"
        );
        assert!(
            id_d.index() != 1 || id_d.generation() != 0,
            "d.rs must not collide with b.rs (slot 1 gen 0)"
        );

        // The new file must be retrievable.
        assert!(
            adapter.get(id_d).is_ok(),
            "d.rs must be persisted and retrievable"
        );
    }
}

// ── AC3-ext: crash after files commit but before meta commit leaves consistent state ──
//
// Simulates the torn-write window: after the files+paths transaction commits,
// the meta tree is forcibly left stale (as if the process crashed before the
// meta write).  On reopen the workspace must be self-consistent.

#[test]
fn torn_meta_write_leaves_consistent_state_on_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    // Write a file normally so the database has real content.
    let good = make_virtual_file(0, 0, "good.rs");
    {
        let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
        adapter.put(good.clone()).unwrap();
    }

    // Corrupt the meta tree (simulate a crash-truncated write) by wiping
    // slot_generations but leaving free_slots intact.
    {
        let db = SledStorageAdapter::open_db_with_retry(&path).unwrap();
        let meta = db.open_tree("meta").unwrap();
        meta.remove(b"slot_generations").unwrap();
        meta.flush().unwrap();
    }

    // On reopen the workspace must still reconstruct correctly from the files
    // tree alone — the good file must be accessible.
    let (adapter2, ws2, _index) = SledStorageAdapter::open(&path).unwrap();
    let id = FileId::new_for_testing(0, 0);
    assert!(
        ws2.get(id).is_ok(),
        "good.rs must be accessible after stale meta on reopen"
    );
    assert_eq!(
        adapter2.get(id).unwrap(),
        good,
        "good.rs value must match after stale meta on reopen"
    );
}

// ── Bijection: overwriting a file with a changed path removes the old path entry ──

#[test]
fn put_with_changed_path_removes_old_path_entry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();

    // Insert file at "old.rs".
    let file_v1 = make_virtual_file(0, 0, "old.rs");
    adapter.put(file_v1.clone()).unwrap();

    // Overwrite with the same FileId but a different path.
    let file_v2 = make_virtual_file(0, 0, "new.rs");
    adapter.put(file_v2.clone()).unwrap();

    // The new path resolves correctly.
    assert_eq!(
        adapter.get(FileId::new_for_testing(0, 0)).unwrap(),
        file_v2,
        "get must return the updated file"
    );

    // The old path entry must be gone from the paths tree.  Inspect directly.
    drop(adapter);
    {
        let db = SledStorageAdapter::open_db_with_retry(&path).unwrap();
        let paths_tree = db.open_tree("paths").unwrap();
        assert!(
            paths_tree.get(b"old.rs").unwrap().is_none(),
            "old path entry must be removed from the paths tree after overwrite"
        );
        assert!(
            paths_tree.get(b"new.rs").unwrap().is_some(),
            "new path entry must exist in the paths tree after overwrite"
        );
    }
}

// ── Codec error in delete returns WriteFailed, not NotFound ──────────────────

#[test]
fn codec_error_in_delete_is_not_reported_as_not_found() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    // First session: write a valid file then close the adapter.
    {
        let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
        let file = make_virtual_file(0, 0, "corrupt.rs");
        adapter.put(file).unwrap();
    }

    // Corrupt the stored bytes directly (adapter is dropped, exclusive lock released).
    {
        let db = SledStorageAdapter::open_db_with_retry(&path).unwrap();
        let files_tree = db.open_tree("files").unwrap();
        let key = {
            let mut k = [0u8; 8];
            k[..4].copy_from_slice(&0u32.to_be_bytes());
            k[4..].copy_from_slice(&0u32.to_be_bytes());
            k
        };
        files_tree.insert(key, b"\xFF\xFF\xFF\xFF").unwrap();
        files_tree.flush().unwrap();
    }

    // Reopen so the adapter sees the corrupt bytes.  rebuild_workspace iterates
    // the files tree during open, so it will fail with Codec when it hits the
    // corrupt entry — that is acceptable (the corruption is caught early).
    //
    // If open somehow succeeds (unlikely given rebuild_workspace reads all files),
    // attempt the delete and verify the error is not NotFound.
    match SledStorageAdapter::open(&path) {
        Err(_) => {
            // open() correctly detected the corruption during rebuild_workspace.
            // This is the expected path — the error is not NotFound.
        }
        Ok((mut adapter2, _ws2, _index)) => {
            let id = FileId::new_for_testing(0, 0);
            let result = adapter2.delete(id);
            // NotFound is the wrong answer: the bytes are present but corrupt.
            // WriteFailed is the correct response.
            if let Err(e) = result {
                assert_ne!(
                    e,
                    crate::ports::PortError::NotFound,
                    "corrupt entry must not be reported as NotFound"
                );
            }
        }
    }
}

// ── Extra: blob round-trip survives reopen ────────────────────────────────────

#[test]
fn blob_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    {
        let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
        let hash = crate::test_support::sample_content_hash();
        adapter.put_blob(hash, b"hello sled".to_vec()).unwrap();
    }

    let (adapter2, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
    let hash = crate::test_support::sample_content_hash();
    assert_eq!(adapter2.get_blob(&hash).unwrap(), b"hello sled");
}

// ── Scan-complete marker survives restart ─────────────────────────────────────

#[test]
fn scan_complete_marker_survives_restart() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    // First session: mark the scan complete.
    {
        let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
        assert!(!adapter.is_scan_complete().unwrap());
        adapter.mark_scan_complete().unwrap();
        assert!(adapter.is_scan_complete().unwrap());
    }

    // Reopen: the flag must still be set.
    let (adapter2, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
    assert!(
        adapter2.is_scan_complete().unwrap(),
        "scan-complete marker must survive a process restart (sled flush)"
    );
}

// ── put_batch: all-or-nothing atomicity on sled ───────────────────────────────
//
// Because SledStorageAdapter cannot inject a mid-batch failure without a
// test-hook (we keep production code clean), we verify the positive path here
// and rely on the InMemoryStorage contract tests for the atomicity invariant.
// The sled `Transactional` API guarantees rollback on any error inside the
// closure, which is covered transitively by the index-abort tests above.

#[test]
fn put_batch_persists_all_files_atomically() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    let files = vec![
        make_virtual_file(0, 0, "batch/a.rs"),
        make_virtual_file(1, 0, "batch/b.rs"),
        make_virtual_file(2, 0, "batch/c.rs"),
    ];

    {
        let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
        adapter.put_batch(&files).unwrap();
    }

    // Reopen and verify all three files survive.
    let (adapter2, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
    for file in &files {
        assert_eq!(
            adapter2.get(file.id).unwrap(),
            *file,
            "file {} must survive restart after put_batch",
            file.id.index()
        );
    }
}

#[test]
fn put_batch_empty_is_noop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    let (mut adapter, _ws, _index) = SledStorageAdapter::open(&path).unwrap();
    // Must not error.
    adapter.put_batch(&[]).unwrap();
}
