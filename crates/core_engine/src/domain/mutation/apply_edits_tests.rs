//! F003 T010 contract tests for host-owned apply-edits DTOs.
#![forbid(unsafe_code)]

use crate::adapters::{InMemoryFs, InMemoryStorage};
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::{FileMutationService, compute_content_version};
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{ContentHash, DomainError, FileId, RelativePath, VirtualFile};
use crate::ports::inbound::{
    ApplyEditsFileResult, ApplyEditsPreview, ApplyEditsRequest, FileMutationUseCase,
    PerFileEditResult, SkippedEdit, SkippedEditReason, TextEdit, WorkspaceApplyEditsErrorCode,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult, WorkspaceEditSpan,
};
use crate::ports::{
    ExtensionHostPort, FileSystemPort, NoOpExtensionHost, PortError, ReconciliationDelta,
    StoragePort,
};
use extension_protocol::messages::{
    SafeDeleteCleanupEdit, SafeDeleteFileError, SafeDeleteFileOutcomeStatus,
    SafeDeleteMutationRequest, SafeDeleteMutationStatus, SafeDeleteRejectionCode,
};
use std::sync::{Arc, Mutex};

fn assert_file_mutation_use_case_apply_edits_contract<T: FileMutationUseCase + ?Sized>() {
    let _: fn(&mut T, ApplyEditsRequest) -> Result<ApplyEditsFileResult, DomainError> =
        T::apply_edits_cas;
    let _: fn(&T, ApplyEditsRequest) -> Result<ApplyEditsFileResult, DomainError> =
        T::apply_edits_dry_run;
    let _: fn(
        &mut T,
        WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> = T::apply_batch_edits;
}

fn workspace_span(
    path: &RelativePath,
    start_byte: usize,
    end_byte: usize,
    replacement: &str,
    base_hash: Option<String>,
) -> WorkspaceEditSpan {
    WorkspaceEditSpan {
        path: path.clone(),
        start_byte,
        end_byte,
        replacement: replacement.to_owned(),
        base_hash,
    }
}

fn per_file<'a>(
    result: &'a WorkspaceApplyEditsResult,
    path: &RelativePath,
) -> &'a PerFileEditResult {
    result
        .per_file
        .iter()
        .find(|file| file.path == *path)
        .unwrap_or_else(|| panic!("missing result for {}", path.as_str()))
}

fn make_state_with_file(
    path: &RelativePath,
    content: &[u8],
) -> (InMemoryFs, ProjectWorkspace, InvertedIndex, InMemoryStorage) {
    let mut fs = InMemoryFs::new();
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service.create_file(path.clone(), content.to_vec()).unwrap();
    }
    (fs, workspace, index, storage)
}

fn make_fs_only_file(path: &RelativePath, content: &[u8]) -> InMemoryFs {
    let mut fs = InMemoryFs::new();
    fs.write(path.clone(), content.to_vec()).unwrap();
    fs
}

#[derive(Default)]
struct RecordingHost {
    changed: Mutex<Vec<String>>,
    order: Option<Arc<Mutex<Vec<String>>>>,
}

impl RecordingHost {
    fn ordered(order: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            changed: Mutex::new(Vec::new()),
            order: Some(order),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileProjection {
    bytes: Vec<u8>,
    version: String,
    vfs_record: VirtualFile,
    storage_record: VirtualFile,
    index_matches: Vec<RelativePath>,
    storage_matches: Vec<FileId>,
}

fn snapshot_file_projection(
    fs: &InMemoryFs,
    workspace: &ProjectWorkspace,
    index: &InvertedIndex,
    storage: &InMemoryStorage,
    path: &RelativePath,
    search_query: &str,
) -> FileProjection {
    use crate::domain::index::FileSearch;
    use crate::ports::inbound::SearchUseCase as _;

    let id = workspace
        .get_by_path(path)
        .expect("fixture file must be tracked");
    let bytes = fs.read(path).expect("fixture file must be readable");
    FileProjection {
        version: compute_content_version(&bytes),
        bytes,
        vfs_record: workspace.get(id).unwrap().clone(),
        storage_record: storage.get(id).unwrap(),
        index_matches: FileSearch::new(index, workspace)
            .find_file(search_query)
            .unwrap(),
        storage_matches: storage.search_ids_for_test(search_query),
    }
}

struct OrderedFs {
    inner: InMemoryFs,
    order: Arc<Mutex<Vec<String>>>,
}

impl FileSystemPort for OrderedFs {
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        self.inner.read(path)
    }

    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.write(path, bytes)
    }

    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        let result = self.inner.rename(from, to.clone());
        if result.is_ok() && !to.as_str().ends_with(".tmp_write") {
            self.order
                .lock()
                .unwrap()
                .push(format!("filesystem:{}", to.as_str()));
        }
        result
    }

    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(path)
    }

    fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
        self.inner.mkdir(path)
    }

    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
}

struct OrderedStorage {
    inner: InMemoryStorage,
    order: Arc<Mutex<Vec<String>>>,
}

impl StoragePort for OrderedStorage {
    fn apply_reconciliation_delta(&mut self, delta: &ReconciliationDelta) -> Result<(), PortError> {
        self.inner.apply_reconciliation_delta(delta)
    }

    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.inner.get(id)
    }

    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        let path = file.path.clone();
        self.inner.put(file)?;
        self.order
            .lock()
            .unwrap()
            .push(format!("storage:{}", path.as_str()));
        Ok(())
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

fn safe_delete_edit(
    path: &str,
    content: &[u8],
    start_byte: usize,
    end_byte: usize,
    replacement: &str,
) -> SafeDeleteCleanupEdit {
    SafeDeleteCleanupEdit {
        path: path.to_owned(),
        start_byte,
        end_byte,
        replacement: replacement.to_owned(),
        base_hash: compute_content_version(content),
    }
}

fn make_state_with_files(
    files: &[(&str, &[u8])],
) -> (InMemoryFs, ProjectWorkspace, InvertedIndex, InMemoryStorage) {
    let mut fs = InMemoryFs::new();
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        for (path, content) in files {
            service
                .create_file(RelativePath::new(*path), content.to_vec())
                .unwrap();
        }
    }
    (fs, workspace, index, storage)
}

struct FailNthRenameFs {
    inner: InMemoryFs,
    fail_on: usize,
    renames: usize,
}

struct FailReadFs {
    inner: InMemoryFs,
    fail_path: RelativePath,
}

impl FileSystemPort for FailReadFs {
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        if path == &self.fail_path {
            return Err(PortError::ReadFailed("injected read failure".to_owned()));
        }
        self.inner.read(path)
    }

    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.write(path, bytes)
    }

    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        self.inner.rename(from, to)
    }

    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(path)
    }

    fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
        self.inner.mkdir(path)
    }

    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
}

impl FileSystemPort for FailNthRenameFs {
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        self.inner.read(path)
    }

    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.inner.write(path, bytes)
    }

    fn rename(&mut self, from: &RelativePath, to: RelativePath) -> Result<(), PortError> {
        self.renames += 1;
        if self.renames == self.fail_on {
            return Err(PortError::WriteFailed("injected rename failure".to_owned()));
        }
        self.inner.rename(from, to)
    }

    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.inner.delete(path)
    }

    fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
        self.inner.mkdir(path)
    }

    fn scan(&self) -> Vec<RelativePath> {
        self.inner.scan()
    }
}

struct FailNthPutStorage {
    inner: InMemoryStorage,
    fail_on: usize,
    puts: usize,
}

impl StoragePort for FailNthPutStorage {
    fn apply_reconciliation_delta(&mut self, delta: &ReconciliationDelta) -> Result<(), PortError> {
        self.inner.apply_reconciliation_delta(delta)
    }

    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.inner.get(id)
    }

    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        self.puts += 1;
        if self.puts == self.fail_on {
            return Err(PortError::WriteFailed(
                "injected persistence failure".to_owned(),
            ));
        }
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

impl ExtensionHostPort for RecordingHost {
    fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

    fn on_file_changed(&self, _id: FileId, path: &RelativePath) {
        self.changed.lock().unwrap().push(path.as_str().to_owned());
        if let Some(order) = &self.order {
            order
                .lock()
                .unwrap()
                .push(format!("callback:{}", path.as_str()));
        }
    }

    fn on_file_deleted(&self, _path: &RelativePath) {}

    fn declared_tools(&self) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
        Vec::new()
    }

    fn invoke(
        &self,
        tool_name: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, crate::domain::InvokeError> {
        Err(crate::domain::InvokeError::ToolNotFound(
            tool_name.to_owned(),
        ))
    }
}

#[test]
fn apply_edits_contract_types() {
    assert_file_mutation_use_case_apply_edits_contract::<dyn FileMutationUseCase>();

    let edit = TextEdit {
        start_byte: 3,
        end_byte: 7,
        replacement: "run".to_owned(),
    };
    let skipped = SkippedEdit {
        edit: edit.clone(),
        reason: SkippedEditReason::Conflict,
    };
    let preview = ApplyEditsPreview {
        path: RelativePath::new("src/main.rs"),
        edits: vec![edit.clone()],
        skipped: vec![skipped.clone()],
        preview_content: "fn run() {}".to_owned(),
    };
    let result = ApplyEditsFileResult {
        path: RelativePath::new("src/main.rs"),
        applied: vec![edit.clone()],
        skipped: vec![skipped],
        new_version: Some("v2".to_owned()),
        preview: Some(preview),
    };
    let request = ApplyEditsRequest {
        path: result.path.clone(),
        expected_version: "v1".to_owned(),
        edits: vec![edit],
    };

    let request_json = serde_json::to_string(&request).unwrap();
    let request_roundtrip: ApplyEditsRequest = serde_json::from_str(&request_json).unwrap();
    assert_eq!(request_roundtrip.expected_version, "v1");
    assert_eq!(request_roundtrip.path.as_str(), "src/main.rs");
    assert_eq!(request_roundtrip.edits[0].replacement, "run");

    let result_json = serde_json::to_string(&result).unwrap();
    let result_roundtrip: ApplyEditsFileResult = serde_json::from_str(&result_json).unwrap();
    assert_eq!(result_roundtrip.new_version.as_deref(), Some("v2"));
    assert_eq!(
        result_roundtrip.skipped[0].reason,
        SkippedEditReason::Conflict
    );
    assert_eq!(
        result_roundtrip.preview.as_ref().unwrap().preview_content,
        "fn run() {}"
    );

    let missing_version = serde_json::json!({
        "path": "src/main.rs",
        "edits": [{
            "start_byte": 3,
            "end_byte": 7,
            "replacement": "run"
        }]
    });
    let missing_version_error =
        serde_json::from_value::<ApplyEditsRequest>(missing_version).unwrap_err();
    assert!(
        missing_version_error
            .to_string()
            .contains("missing field `expected_version`"),
        "expected missing expected_version error, got {missing_version_error}"
    );

    let invalid_skip_reason = serde_json::json!({
        "path": "src/main.rs",
        "applied": [],
        "skipped": [{
            "edit": {
                "start_byte": 3,
                "end_byte": 7,
                "replacement": "run"
            },
            "reason": "Overlapped"
        }],
        "new_version": null,
        "preview": null
    });
    let invalid_skip_reason_error =
        serde_json::from_value::<ApplyEditsFileResult>(invalid_skip_reason).unwrap_err();
    assert_eq!(
        invalid_skip_reason_error.to_string(),
        "unknown variant `Overlapped`, expected `Conflict`"
    );
}

#[test]
fn safe_non_overlapping_edits_commit_once() {
    let path = RelativePath::new("src/main.rs");
    let original = b"alpha beta gamma\n";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let expected_version = compute_content_version(original);
    let beta = TextEdit {
        start_byte: 6,
        end_byte: 10,
        replacement: "BETA".to_owned(),
    };
    let gamma = TextEdit {
        start_byte: 11,
        end_byte: 16,
        replacement: "GAMMA".to_owned(),
    };
    let host = RecordingHost::default();

    let mut service =
        FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
    let result = service
        .apply_edits_cas(ApplyEditsRequest {
            path: path.clone(),
            expected_version,
            edits: vec![gamma.clone(), beta.clone()],
        })
        .unwrap();

    assert_eq!(fs.read(&path).unwrap(), b"alpha BETA GAMMA\n");
    assert_eq!(result.path, path);
    assert_eq!(result.applied, vec![beta, gamma]);
    assert!(result.skipped.is_empty());
    assert_eq!(
        result.new_version.as_deref(),
        Some(compute_content_version(b"alpha BETA GAMMA\n").as_str())
    );
    assert!(result.preview.is_none());
    assert_eq!(
        host.changed.lock().unwrap().as_slice(),
        ["src/main.rs"],
        "apply_edits_cas must commit through the single indexed-write path once"
    );
}

#[test]
fn apply_batch_edits_dry_run_allows_readable_file_missing_from_workspace_index() {
    let path = RelativePath::new("extensions/hello/src/main.rs");
    let original = b"fn send_response() {}\n";
    let mut fs = make_fs_only_file(&path, original);
    let mut workspace = ProjectWorkspace::new();
    let mut index = InvertedIndex::new();
    let mut storage = InMemoryStorage::new();
    let host = RecordingHost::default();
    let expected_version = compute_content_version(original);

    let mut service =
        FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![workspace_span(
                &path,
                0,
                0,
                "// preview\n",
                Some(expected_version),
            )],
            dry_run: Some(true),
        })
        .unwrap();

    let file = per_file(&result, &path);
    assert!(!file.applied);
    assert_eq!(file.edits_applied, 1);
    assert_eq!(file.edits_skipped, 0);
    assert_eq!(file.error, None);
    assert_eq!(
        file.preview.as_deref(),
        Some("// preview\nfn send_response() {}\n")
    );
    assert_eq!(fs.read(&path).unwrap(), original);
    assert!(workspace.get_by_path(&path).is_none());
    assert!(host.changed.lock().unwrap().is_empty());
}

#[test]
fn overlapping_edits_skip_conflict_deterministically() {
    let path = RelativePath::new("src/conflict.rs");
    let original = b"0123456789";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let first = TextEdit {
        start_byte: 1,
        end_byte: 5,
        replacement: "AAAA".to_owned(),
    };
    let conflicting = TextEdit {
        start_byte: 3,
        end_byte: 7,
        replacement: "BBBB".to_owned(),
    };
    let later = TextEdit {
        start_byte: 7,
        end_byte: 10,
        replacement: "CCC".to_owned(),
    };

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_edits_cas(ApplyEditsRequest {
            path: path.clone(),
            expected_version: compute_content_version(original),
            edits: vec![conflicting.clone(), later.clone(), first.clone()],
        })
        .unwrap();

    assert_eq!(fs.read(&path).unwrap(), b"0AAAA56CCC");
    assert_eq!(result.applied, vec![first, later]);
    assert_eq!(
        result.skipped,
        vec![SkippedEdit {
            edit: conflicting,
            reason: SkippedEditReason::Conflict
        }]
    );
    assert!(result.new_version.is_some());
    assert!(result.preview.is_none());
}

#[test]
fn same_position_insertions_skip_later_conflicts() {
    let path = RelativePath::new("src/insertions.rs");
    let original = b"abc";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let first = TextEdit {
        start_byte: 1,
        end_byte: 1,
        replacement: "X".to_owned(),
    };
    let second = TextEdit {
        start_byte: 1,
        end_byte: 1,
        replacement: "Y".to_owned(),
    };

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_edits_cas(ApplyEditsRequest {
            path,
            expected_version: compute_content_version(original),
            edits: vec![first.clone(), second.clone()],
        })
        .unwrap();

    assert_eq!(
        fs.read(&RelativePath::new("src/insertions.rs")).unwrap(),
        b"aXbc"
    );
    assert_eq!(result.applied, vec![first]);
    assert_eq!(
        result.skipped,
        vec![SkippedEdit {
            edit: second,
            reason: SkippedEditReason::Conflict
        }]
    );
}

#[test]
fn all_conflicting_edits_return_valid_result_without_writing() {
    let path = RelativePath::new("src/all_conflict.rs");
    let original = b"0123456789";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let first = TextEdit {
        start_byte: 2,
        end_byte: 6,
        replacement: "AAAA".to_owned(),
    };
    let second = TextEdit {
        start_byte: 4,
        end_byte: 8,
        replacement: "BBBB".to_owned(),
    };
    let host = RecordingHost::default();

    let mut service =
        FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
    let result = service
        .apply_edits_cas(ApplyEditsRequest {
            path: path.clone(),
            expected_version: compute_content_version(original),
            edits: vec![second.clone(), first.clone()],
        })
        .unwrap();

    assert_eq!(fs.read(&path).unwrap(), original);
    assert_eq!(result.path, path);
    assert!(result.applied.is_empty());
    assert_eq!(
        result.skipped,
        vec![
            SkippedEdit {
                edit: first,
                reason: SkippedEditReason::Conflict
            },
            SkippedEdit {
                edit: second,
                reason: SkippedEditReason::Conflict
            }
        ]
    );
    assert_eq!(result.new_version, None);
    assert!(result.preview.is_none());
    assert!(
        host.changed.lock().unwrap().is_empty(),
        "apply_edits_cas must not commit or broadcast when every edit is skipped"
    );
}

#[test]
fn stale_expected_version_rejects_without_writing() {
    let path = RelativePath::new("src/stale.rs");
    let original = b"fn original() {}";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let stale_version = compute_content_version(original);

    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .create_file(path.clone(), b"fn changed() {}".to_vec())
            .unwrap();
    }

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let error = service
        .apply_edits_cas(ApplyEditsRequest {
            path: path.clone(),
            expected_version: stale_version.clone(),
            edits: vec![TextEdit {
                start_byte: 3,
                end_byte: 10,
                replacement: "rewritten".to_owned(),
            }],
        })
        .unwrap_err();

    match error {
        DomainError::VersionConflict { expected, actual } => {
            assert_eq!(expected, stale_version);
            assert_eq!(actual, compute_content_version(b"fn changed() {}"));
        }
        other => panic!("expected VersionConflict, got {other:?}"),
    }
    assert_eq!(fs.read(&path).unwrap(), b"fn changed() {}");
}

#[test]
fn dry_run_returns_preview_without_writing() {
    let path = RelativePath::new("src/main.rs");
    let original = b"fn main() {}\n";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let original_version = compute_content_version(original);
    let edit = TextEdit {
        start_byte: 3,
        end_byte: 7,
        replacement: "run".to_owned(),
    };

    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_edits_dry_run(ApplyEditsRequest {
            path: path.clone(),
            expected_version: original_version.clone(),
            edits: vec![edit.clone()],
        })
        .unwrap();

    assert_eq!(fs.read(&path).unwrap(), original);
    assert_eq!(
        compute_content_version(&fs.read(&path).unwrap()),
        original_version
    );
    assert_eq!(result.applied, vec![edit.clone()]);
    assert!(result.skipped.is_empty());
    assert_eq!(result.new_version, None);
    assert_eq!(
        result.preview,
        Some(ApplyEditsPreview {
            path: path.clone(),
            edits: vec![edit],
            skipped: vec![],
            preview_content: "fn run() {}\n".to_owned()
        })
    );
}

#[test]
fn invalid_utf8_boundary_rejects_without_writing() {
    let path = RelativePath::new("src/utf8.rs");
    let original = "héllo".as_bytes();
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let error = service
        .apply_edits_cas(ApplyEditsRequest {
            path: path.clone(),
            expected_version: compute_content_version(original),
            edits: vec![
                TextEdit {
                    start_byte: 0,
                    end_byte: 1,
                    replacement: "H".to_owned(),
                },
                TextEdit {
                    start_byte: 2,
                    end_byte: 3,
                    replacement: "e".to_owned(),
                },
            ],
        })
        .unwrap_err();

    assert!(
        matches!(error, DomainError::InvalidRange(_)),
        "invalid UTF-8 boundary must return InvalidRange; got {error:?}"
    );
    assert_eq!(fs.read(&path).unwrap(), original);
}

#[test]
fn apply_batch_edits_returns_files_changed_equal_to_files_actually_committed() {
    let path_a = RelativePath::new("src/a.rs");
    let path_b = RelativePath::new("src/b.rs");
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path_a, b"alpha");
    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .create_file(path_b.clone(), b"beta".to_vec())
            .unwrap();
    }

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(
                    &path_a,
                    0,
                    5,
                    "ALPHA",
                    Some(compute_content_version(b"alpha")),
                ),
                workspace_span(&path_b, 0, 4, "BETA", Some("stale".to_owned())),
            ],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 1);
    assert_eq!(fs.read(&path_a).unwrap(), b"ALPHA");
    assert_eq!(fs.read(&path_b).unwrap(), b"beta");
    assert!(per_file(&result, &path_a).applied);
    assert_eq!(per_file(&result, &path_a).edits_applied, 1);
    assert!(!per_file(&result, &path_b).applied);
    assert_eq!(
        per_file(&result, &path_b)
            .error
            .as_ref()
            .map(|err| &err.code),
        Some(&WorkspaceApplyEditsErrorCode::Conflict)
    );
}

#[test]
fn apply_batch_edits_rejects_an_empty_edits_list_with_empty_edits_and_commits_no_files() {
    let path = RelativePath::new("src/empty.rs");
    let original = b"unchanged";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 0);
    assert_eq!(fs.read(&path).unwrap(), original);
    assert_eq!(result.per_file.len(), 1);
    let file = &result.per_file[0];
    assert_eq!(file.path, RelativePath::new(""));
    assert!(!file.applied);
    assert_eq!(file.edits_applied, 0);
    assert_eq!(file.edits_skipped, 0);
    assert_eq!(
        file.error.as_ref().map(|error| &error.code),
        Some(&WorkspaceApplyEditsErrorCode::EmptyEdits)
    );
}

#[test]
fn apply_batch_edits_groups_spans_by_path_and_preserves_best_effort_cross_file_behavior() {
    let path_a = RelativePath::new("src/group_a.rs");
    let path_b = RelativePath::new("src/group_b.rs");
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path_a, b"one two");
    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .create_file(path_b.clone(), b"three four".to_vec())
            .unwrap();
    }

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(
                    &path_a,
                    0,
                    3,
                    "ONE",
                    Some(compute_content_version(b"one two")),
                ),
                workspace_span(&path_b, 0, 5, "THREE", Some("stale".to_owned())),
                workspace_span(
                    &path_a,
                    4,
                    7,
                    "TWO",
                    Some(compute_content_version(b"one two")),
                ),
            ],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 1);
    assert_eq!(fs.read(&path_a).unwrap(), b"ONE TWO");
    assert_eq!(fs.read(&path_b).unwrap(), b"three four");
    assert_eq!(per_file(&result, &path_a).edits_applied, 2);
    assert!(!per_file(&result, &path_b).applied);
    assert_eq!(
        per_file(&result, &path_b)
            .error
            .as_ref()
            .map(|err| &err.code),
        Some(&WorkspaceApplyEditsErrorCode::Conflict)
    );
}

#[test]
fn apply_batch_edits_sorts_multiple_spans_in_one_file_by_descending_start_byte() {
    let path = RelativePath::new("src/descending.rs");
    let original = b"abc def ghi";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(&path, 0, 3, "ABC", Some(compute_content_version(original))),
                workspace_span(&path, 8, 11, "GHI", Some(compute_content_version(original))),
                workspace_span(&path, 4, 7, "DEF", Some(compute_content_version(original))),
            ],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 1);
    assert_eq!(fs.read(&path).unwrap(), b"ABC DEF GHI");
    assert_eq!(per_file(&result, &path).edits_applied, 3);
    assert_eq!(per_file(&result, &path).edits_skipped, 0);
}

#[test]
fn apply_batch_edits_marks_one_file_as_not_applied_for_overlapping_spans_and_does_not_mutate_it() {
    let path = RelativePath::new("src/overlap.rs");
    let original = b"0123456789";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(&path, 1, 5, "AAAA", Some(compute_content_version(original))),
                workspace_span(&path, 3, 7, "BBBB", Some(compute_content_version(original))),
            ],
            dry_run: None,
        })
        .unwrap();

    let file = per_file(&result, &path);
    assert_eq!(result.files_changed, 0);
    assert!(!file.applied);
    assert_eq!(file.edits_applied, 0);
    assert_eq!(
        file.error.as_ref().map(|err| &err.code),
        Some(&WorkspaceApplyEditsErrorCode::OverlappingSpans)
    );
    assert_eq!(fs.read(&path).unwrap(), original);
}

#[test]
fn apply_batch_edits_marks_stale_base_hash_as_conflict_and_reports_non_conflicting_outcomes() {
    let stale_path = RelativePath::new("src/stale_batch.rs");
    let ok_path = RelativePath::new("src/ok_batch.rs");
    let (mut fs, mut workspace, mut index, mut storage) =
        make_state_with_file(&stale_path, b"current");
    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .create_file(ok_path.clone(), b"fresh".to_vec())
            .unwrap();
    }

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(&stale_path, 0, 7, "changed", Some("stale".to_owned())),
                workspace_span(
                    &ok_path,
                    0,
                    5,
                    "FRESH",
                    Some(compute_content_version(b"fresh")),
                ),
            ],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 1);
    assert_eq!(fs.read(&stale_path).unwrap(), b"current");
    assert_eq!(fs.read(&ok_path).unwrap(), b"FRESH");
    assert!(per_file(&result, &ok_path).applied);
    assert_eq!(
        per_file(&result, &stale_path)
            .error
            .as_ref()
            .map(|err| &err.code),
        Some(&WorkspaceApplyEditsErrorCode::Conflict)
    );
}

#[test]
fn apply_batch_edits_checks_every_base_hash_in_a_file_group_before_mutating() {
    let path = RelativePath::new("src/mixed_hashes.rs");
    let original = b"alpha beta";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(
                    &path,
                    0,
                    5,
                    "ALPHA",
                    Some(compute_content_version(original)),
                ),
                workspace_span(&path, 6, 10, "BETA", Some("stale".to_owned())),
            ],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 0);
    assert_eq!(fs.read(&path).unwrap(), original);
    let file = per_file(&result, &path);
    assert!(!file.applied);
    assert_eq!(file.edits_applied, 0);
    assert_eq!(file.edits_skipped, 2);
    assert_eq!(
        file.error.as_ref().map(|error| &error.code),
        Some(&WorkspaceApplyEditsErrorCode::Conflict)
    );
}

#[test]
fn apply_batch_edits_requires_every_mutating_edit_to_supply_base_hash() {
    let path = RelativePath::new("src/missing_hash.rs");
    let original = b"alpha beta";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(
                    &path,
                    0,
                    5,
                    "ALPHA",
                    Some(compute_content_version(original)),
                ),
                workspace_span(&path, 6, 10, "BETA", None),
            ],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 0);
    assert_eq!(fs.read(&path).unwrap(), original);
    let file = per_file(&result, &path);
    assert!(!file.applied);
    assert_eq!(file.edits_applied, 0);
    assert_eq!(file.edits_skipped, 2);
    assert_eq!(
        file.error.as_ref().map(|error| &error.code),
        Some(&WorkspaceApplyEditsErrorCode::Conflict)
    );
}

#[test]
fn apply_batch_edits_reports_invalid_range_for_the_affected_file_and_leaves_it_unchanged() {
    let invalid_path = RelativePath::new("src/invalid_batch.rs");
    let ordering_path = RelativePath::new("src/invalid_order_batch.rs");
    let ok_path = RelativePath::new("src/valid_batch.rs");
    let invalid_original = "héllo".as_bytes();
    let ordering_original = b"order";
    let (mut fs, mut workspace, mut index, mut storage) =
        make_state_with_file(&invalid_path, invalid_original);
    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .create_file(ok_path.clone(), b"plain".to_vec())
            .unwrap();
        service
            .create_file(ordering_path.clone(), ordering_original.to_vec())
            .unwrap();
    }

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![
                workspace_span(
                    &invalid_path,
                    2,
                    3,
                    "e",
                    Some(compute_content_version(invalid_original)),
                ),
                workspace_span(
                    &ordering_path,
                    4,
                    2,
                    "x",
                    Some(compute_content_version(ordering_original)),
                ),
                workspace_span(
                    &ok_path,
                    0,
                    5,
                    "PLAIN",
                    Some(compute_content_version(b"plain")),
                ),
            ],
            dry_run: None,
        })
        .unwrap();

    assert_eq!(result.files_changed, 1);
    assert_eq!(fs.read(&invalid_path).unwrap(), invalid_original);
    assert_eq!(fs.read(&ordering_path).unwrap(), ordering_original);
    assert_eq!(fs.read(&ok_path).unwrap(), b"PLAIN");
    assert_eq!(
        per_file(&result, &invalid_path)
            .error
            .as_ref()
            .map(|err| &err.code),
        Some(&WorkspaceApplyEditsErrorCode::InvalidRange)
    );
    assert_eq!(
        per_file(&result, &ordering_path)
            .error
            .as_ref()
            .map(|err| &err.code),
        Some(&WorkspaceApplyEditsErrorCode::InvalidRange)
    );
}

#[test]
fn apply_batch_edits_with_dry_run_returns_preview_data_and_changes_no_contents_indexes_or_versions()
{
    use crate::domain::index::FileSearch;
    use crate::ports::inbound::SearchUseCase as _;

    let path = RelativePath::new("src/dry_batch.rs");
    let original = b"fn old() {}\n";
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let original_version = compute_content_version(original);

    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let result = service
        .apply_batch_edits(WorkspaceApplyEditsRequest {
            edits: vec![workspace_span(
                &path,
                3,
                6,
                "new",
                Some(original_version.clone()),
            )],
            dry_run: Some(true),
        })
        .unwrap();

    assert_eq!(result.files_changed, 0);
    assert_eq!(fs.read(&path).unwrap(), original);
    assert_eq!(
        compute_content_version(&fs.read(&path).unwrap()),
        original_version
    );

    let file = per_file(&result, &path);
    assert!(!file.applied);
    assert_eq!(file.edits_applied, 1);
    assert_eq!(file.new_version, None);
    assert_eq!(file.preview.as_deref(), Some("fn new() {}\n"));

    let results = FileSearch::new(&index, &workspace)
        .search_text("new")
        .unwrap();
    assert!(
        results.is_empty(),
        "dry-run preview content must not update the text index"
    );
}

#[test]
fn safe_delete_edits_whole_plan_success_preserves_file_order_and_commits_before_notifications() {
    use crate::ports::inbound::SearchUseCase as _;

    let files: &[(&str, &[u8])] = &[("src/b.rs", b"abcdef"), ("src/a.rs", b"uvwxyz")];
    let (fs, mut workspace, mut index, storage) = make_state_with_files(files);
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut fs = OrderedFs {
        inner: fs,
        order: Arc::clone(&order),
    };
    let mut storage = OrderedStorage {
        inner: storage,
        order: Arc::clone(&order),
    };
    let host = RecordingHost::ordered(Arc::clone(&order));
    let result = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![
                    safe_delete_edit("src/b.rs", b"abcdef", 4, 6, ""),
                    safe_delete_edit("src/a.rs", b"uvwxyz", 1, 3, "XY"),
                    safe_delete_edit("src/b.rs", b"abcdef", 0, 2, "AB"),
                ],
                dry_run: false,
            })
            .unwrap()
    };

    assert_eq!(result.status, SafeDeleteMutationStatus::Applied);
    assert_eq!(result.rejection, None);
    assert_eq!(
        result
            .per_file
            .iter()
            .map(|file| (file.path.as_str(), file.outcome))
            .collect::<Vec<_>>(),
        vec![
            ("src/b.rs", SafeDeleteFileOutcomeStatus::Committed),
            ("src/a.rs", SafeDeleteFileOutcomeStatus::Committed),
        ]
    );
    let b_path = RelativePath::new("src/b.rs");
    let a_path = RelativePath::new("src/a.rs");
    assert_eq!(fs.read(&b_path).unwrap(), b"ABcd");
    assert_eq!(fs.read(&a_path).unwrap(), b"uXYxyz");
    for path in [&b_path, &a_path] {
        let id = workspace.get_by_path(path).unwrap();
        assert_eq!(
            storage.inner.get(id).unwrap(),
            workspace.get(id).unwrap().clone()
        );
        assert!(
            crate::domain::index::FileSearch::new(&index, &workspace)
                .find_file(path.as_str())
                .unwrap()
                .contains(path)
        );
    }
    assert_eq!(
        order.lock().unwrap().as_slice(),
        [
            "filesystem:src/b.rs",
            "storage:src/b.rs",
            "callback:src/b.rs",
            "filesystem:src/a.rs",
            "storage:src/a.rs",
            "callback:src/a.rs",
        ]
    );
    assert_eq!(
        host.changed.lock().unwrap().as_slice(),
        ["src/b.rs", "src/a.rs"]
    );
}

#[test]
fn safe_delete_edits_stale_version_rejects_complete_plan_without_partial_commits() {
    let files: &[(&str, &[u8])] = &[("src/first.rs", b"first"), ("src/stale.rs", b"stale")];
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_files(files);
    let first_path = RelativePath::new("src/first.rs");
    let stale_path = RelativePath::new("src/stale.rs");
    let first_before =
        snapshot_file_projection(&fs, &workspace, &index, &storage, &first_path, "first");
    let stale_before =
        snapshot_file_projection(&fs, &workspace, &index, &storage, &stale_path, "stale");
    let host = RecordingHost::default();
    let mut stale = safe_delete_edit("src/stale.rs", b"stale", 0, 5, "fresh");
    stale.base_hash = compute_content_version(b"older");
    let result = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![
                    safe_delete_edit("src/first.rs", b"first", 0, 5, "FIRST"),
                    stale,
                ],
                dry_run: false,
            })
            .unwrap()
    };

    assert_eq!(result.status, SafeDeleteMutationStatus::Rejected);
    assert_eq!(result.rejection, Some(SafeDeleteRejectionCode::CasConflict));
    assert!(
        result
            .per_file
            .iter()
            .all(|file| file.outcome == SafeDeleteFileOutcomeStatus::Unattempted)
    );
    assert_eq!(
        snapshot_file_projection(&fs, &workspace, &index, &storage, &first_path, "first"),
        first_before
    );
    assert_eq!(
        snapshot_file_projection(&fs, &workspace, &index, &storage, &stale_path, "stale"),
        stale_before
    );
    assert!(host.changed.lock().unwrap().is_empty());
}

#[test]
fn safe_delete_edits_invalid_path_rejects_complete_plan_without_mutation() {
    let files: &[(&str, &[u8])] = &[("src/valid.rs", b"valid")];
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_files(files);
    let valid_path = RelativePath::new("src/valid.rs");
    let valid_before =
        snapshot_file_projection(&fs, &workspace, &index, &storage, &valid_path, "valid");
    let host = RecordingHost::default();
    let result = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![
                    safe_delete_edit("src/valid.rs", b"valid", 0, 5, "VALID"),
                    safe_delete_edit("../outside.rs", b"outside", 0, 7, "OUTSIDE"),
                ],
                dry_run: false,
            })
            .unwrap()
    };

    assert_eq!(result.status, SafeDeleteMutationStatus::Rejected);
    assert_eq!(result.rejection, Some(SafeDeleteRejectionCode::InvalidPath));
    assert!(
        result
            .per_file
            .iter()
            .all(|file| file.outcome == SafeDeleteFileOutcomeStatus::Unattempted)
    );
    assert_eq!(
        snapshot_file_projection(&fs, &workspace, &index, &storage, &valid_path, "valid"),
        valid_before
    );
    assert!(host.changed.lock().unwrap().is_empty());
}

#[test]
fn safe_delete_edits_rejects_non_canonical_lexical_paths_with_matching_vfs_entries() {
    for invalid_path in ["C:foo", "src\\file.rs", "src//file.rs", "src/./file.rs"] {
        let files: &[(&str, &[u8])] = &[(invalid_path, b"content")];
        let (mut fs, mut workspace, mut index, mut storage) = make_state_with_files(files);
        let host = RecordingHost::default();
        let result = {
            let mut service =
                FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
            service
                .apply_safe_delete_edits(SafeDeleteMutationRequest {
                    edits: vec![safe_delete_edit(invalid_path, b"content", 0, 7, "changed")],
                    dry_run: false,
                })
                .unwrap()
        };

        assert_eq!(result.status, SafeDeleteMutationStatus::Rejected);
        assert_eq!(result.rejection, Some(SafeDeleteRejectionCode::InvalidPath));
        assert_eq!(
            fs.read(&RelativePath::new(invalid_path)).unwrap(),
            b"content"
        );
        assert!(host.changed.lock().unwrap().is_empty());
    }
}

#[test]
fn safe_delete_edits_propagates_non_not_found_preflight_read_failures() {
    let path = RelativePath::new("src/unreadable.rs");
    let (fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, b"content");
    let mut fs = FailReadFs {
        inner: fs,
        fail_path: path,
    };
    let host = RecordingHost::default();
    let error = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![safe_delete_edit(
                    "src/unreadable.rs",
                    b"content",
                    0,
                    7,
                    "changed",
                )],
                dry_run: false,
            })
            .unwrap_err()
    };

    assert_eq!(
        error,
        DomainError::IoError("injected read failure".to_owned())
    );
    assert!(host.changed.lock().unwrap().is_empty());
}

#[test]
fn safe_delete_edits_invalid_and_overlapping_ranges_return_exact_rejections() {
    let path = RelativePath::new("src/ranges.rs");
    let original = "héllo".as_bytes();
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_file(&path, original);
    let host = RecordingHost::default();

    for (edits, expected) in [
        (
            vec![safe_delete_edit("src/ranges.rs", original, 2, 1, "x")],
            SafeDeleteRejectionCode::InvalidRange,
        ),
        (
            vec![safe_delete_edit("src/ranges.rs", original, 1, 2, "x")],
            SafeDeleteRejectionCode::InvalidRange,
        ),
        (
            vec![
                safe_delete_edit("src/ranges.rs", original, 0, 3, "x"),
                safe_delete_edit("src/ranges.rs", original, 2, 4, "y"),
            ],
            SafeDeleteRejectionCode::OverlappingEdits,
        ),
    ] {
        let result = {
            let mut service =
                FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
            service
                .apply_safe_delete_edits(SafeDeleteMutationRequest {
                    edits,
                    dry_run: false,
                })
                .unwrap()
        };
        assert_eq!(result.status, SafeDeleteMutationStatus::Rejected);
        assert_eq!(result.rejection, Some(expected));
        assert_eq!(fs.read(&path).unwrap(), original);
        assert!(host.changed.lock().unwrap().is_empty());
    }
}

#[test]
fn safe_delete_edits_later_invalid_or_overlapping_range_leaves_first_file_unchanged() {
    for (invalid_edits, expected) in [
        (
            vec![safe_delete_edit("src/bad.rs", b"second", 4, 2, "x")],
            SafeDeleteRejectionCode::InvalidRange,
        ),
        (
            vec![
                safe_delete_edit("src/bad.rs", b"second", 0, 4, "x"),
                safe_delete_edit("src/bad.rs", b"second", 3, 6, "y"),
            ],
            SafeDeleteRejectionCode::OverlappingEdits,
        ),
    ] {
        let files: &[(&str, &[u8])] = &[("src/first.rs", b"first"), ("src/bad.rs", b"second")];
        let (mut fs, mut workspace, mut index, mut storage) = make_state_with_files(files);
        let first_path = RelativePath::new("src/first.rs");
        let bad_path = RelativePath::new("src/bad.rs");
        let first_before =
            snapshot_file_projection(&fs, &workspace, &index, &storage, &first_path, "first");
        let bad_before =
            snapshot_file_projection(&fs, &workspace, &index, &storage, &bad_path, "second");
        let host = RecordingHost::default();
        let mut edits = vec![safe_delete_edit("src/first.rs", b"first", 0, 5, "FIRST")];
        edits.extend(invalid_edits);

        let result = {
            let mut service =
                FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
            service
                .apply_safe_delete_edits(SafeDeleteMutationRequest {
                    edits,
                    dry_run: false,
                })
                .unwrap()
        };

        assert_eq!(result.status, SafeDeleteMutationStatus::Rejected);
        assert_eq!(result.rejection, Some(expected));
        assert!(
            result
                .per_file
                .iter()
                .all(|file| file.outcome == SafeDeleteFileOutcomeStatus::Unattempted)
        );
        assert_eq!(
            snapshot_file_projection(&fs, &workspace, &index, &storage, &first_path, "first"),
            first_before
        );
        assert_eq!(
            snapshot_file_projection(&fs, &workspace, &index, &storage, &bad_path, "second"),
            bad_before
        );
        assert!(host.changed.lock().unwrap().is_empty());
    }
}

#[test]
fn safe_delete_edits_dry_run_previews_without_changes_and_matches_rejection_validation() {
    let files: &[(&str, &[u8])] = &[("src/a.rs", b"alpha"), ("src/b.rs", b"beta")];
    let (mut fs, mut workspace, mut index, mut storage) = make_state_with_files(files);
    let host = RecordingHost::default();
    let a_path = RelativePath::new("src/a.rs");
    let b_path = RelativePath::new("src/b.rs");
    let a_before = snapshot_file_projection(&fs, &workspace, &index, &storage, &a_path, "a");
    let b_before = snapshot_file_projection(&fs, &workspace, &index, &storage, &b_path, "b");
    let valid_edits = vec![
        safe_delete_edit("src/a.rs", b"alpha", 0, 5, "ALPHA"),
        safe_delete_edit("src/b.rs", b"beta", 0, 4, "BETA"),
    ];
    let preview = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: valid_edits.clone(),
                dry_run: true,
            })
            .unwrap()
    };
    let mut stale = safe_delete_edit("src/b.rs", b"beta", 0, 4, "BETA");
    stale.base_hash = compute_content_version(b"stale");
    let rejection = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![
                    safe_delete_edit("src/a.rs", b"alpha", 0, 5, "ALPHA"),
                    stale.clone(),
                ],
                dry_run: true,
            })
            .unwrap()
    };
    let stale_apply = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![safe_delete_edit("src/a.rs", b"alpha", 0, 5, "ALPHA"), stale],
                dry_run: false,
            })
            .unwrap()
    };

    assert_eq!(preview.status, SafeDeleteMutationStatus::Previewed);
    assert_eq!(preview.rejection, None);
    assert!(
        preview
            .per_file
            .iter()
            .all(|file| file.outcome == SafeDeleteFileOutcomeStatus::Unattempted)
    );
    assert_eq!(rejection.status, SafeDeleteMutationStatus::Rejected);
    assert_eq!(
        rejection.rejection,
        Some(SafeDeleteRejectionCode::CasConflict)
    );
    assert_eq!(rejection, stale_apply);
    let invalid_edits = vec![safe_delete_edit("src/a.rs", b"alpha", 2, 1, "x")];
    let invalid_preview = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: invalid_edits.clone(),
                dry_run: true,
            })
            .unwrap()
    };
    let invalid_apply = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: invalid_edits,
                dry_run: false,
            })
            .unwrap()
    };
    assert_eq!(invalid_preview, invalid_apply);
    assert_eq!(invalid_preview.status, SafeDeleteMutationStatus::Rejected);
    assert_eq!(
        invalid_preview.rejection,
        Some(SafeDeleteRejectionCode::InvalidRange)
    );
    assert_eq!(
        snapshot_file_projection(&fs, &workspace, &index, &storage, &a_path, "a"),
        a_before
    );
    assert_eq!(
        snapshot_file_projection(&fs, &workspace, &index, &storage, &b_path, "b"),
        b_before
    );

    fs.write(a_path.clone(), b"alpha changed".to_vec()).unwrap();
    let callbacks_before_apply = host.changed.lock().unwrap().clone();
    let after_preview_conflict = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: valid_edits,
                dry_run: false,
            })
            .unwrap()
    };
    assert_eq!(
        after_preview_conflict.status,
        SafeDeleteMutationStatus::Rejected
    );
    assert_eq!(
        after_preview_conflict.rejection,
        Some(SafeDeleteRejectionCode::CasConflict)
    );
    assert!(
        after_preview_conflict
            .per_file
            .iter()
            .all(|file| file.outcome == SafeDeleteFileOutcomeStatus::Unattempted)
    );
    assert_eq!(fs.read(&a_path).unwrap(), b"alpha changed");
    assert_eq!(fs.read(&b_path).unwrap(), b"beta");
    assert_eq!(
        host.changed.lock().unwrap().as_slice(),
        callbacks_before_apply
    );
    assert!(host.changed.lock().unwrap().is_empty());
}

#[test]
fn safe_delete_edits_first_file_commit_failure_stops_every_later_file() {
    let files: &[(&str, &[u8])] = &[("src/a.rs", b"alpha"), ("src/b.rs", b"beta")];
    let (fs, mut workspace, mut index, mut storage) = make_state_with_files(files);
    let mut fs = FailNthRenameFs {
        inner: fs,
        fail_on: 1,
        renames: 0,
    };
    let host = RecordingHost::default();
    let result = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![
                    safe_delete_edit("src/a.rs", b"alpha", 0, 5, "ALPHA"),
                    safe_delete_edit("src/b.rs", b"beta", 0, 4, "BETA"),
                ],
                dry_run: false,
            })
            .unwrap()
    };

    assert_eq!(result.status, SafeDeleteMutationStatus::PartialCommit);
    assert_eq!(
        result.per_file[0].outcome,
        SafeDeleteFileOutcomeStatus::Failed
    );
    assert_eq!(
        result.per_file[0].error,
        Some(SafeDeleteFileError::FilesystemError)
    );
    assert_eq!(
        result.per_file[1].outcome,
        SafeDeleteFileOutcomeStatus::Unattempted
    );
    assert_eq!(fs.read(&RelativePath::new("src/a.rs")).unwrap(), b"alpha");
    assert_eq!(fs.read(&RelativePath::new("src/b.rs")).unwrap(), b"beta");
    assert!(host.changed.lock().unwrap().is_empty());
}

#[test]
fn safe_delete_edits_commit_time_partial_failure_reports_committed_failed_and_unattempted_paths() {
    let files: &[(&str, &[u8])] = &[
        ("src/a.rs", b"alpha"),
        ("src/b.rs", b"beta"),
        ("src/c.rs", b"gamma"),
    ];
    let (mut fs, mut workspace, mut index, storage) = make_state_with_files(files);
    let b_path = RelativePath::new("src/b.rs");
    let b_before = snapshot_file_projection(&fs, &workspace, &index, &storage, &b_path, "b");
    let mut storage = FailNthPutStorage {
        inner: storage,
        fail_on: 2,
        puts: 0,
    };
    let host = RecordingHost::default();
    let result = {
        let mut service =
            FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
        service
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: vec![
                    safe_delete_edit("src/a.rs", b"alpha", 0, 5, "ALPHA"),
                    safe_delete_edit("src/b.rs", b"beta", 0, 4, "BETA"),
                    safe_delete_edit("src/c.rs", b"gamma", 0, 5, "GAMMA"),
                ],
                dry_run: false,
            })
            .unwrap()
    };

    assert_eq!(result.status, SafeDeleteMutationStatus::PartialCommit);
    assert_eq!(result.rejection, None);
    assert_eq!(
        result
            .per_file
            .iter()
            .map(|file| (file.path.as_str(), file.outcome, file.error))
            .collect::<Vec<_>>(),
        vec![
            ("src/a.rs", SafeDeleteFileOutcomeStatus::Committed, None),
            (
                "src/b.rs",
                SafeDeleteFileOutcomeStatus::Failed,
                Some(SafeDeleteFileError::PersistenceError),
            ),
            ("src/c.rs", SafeDeleteFileOutcomeStatus::Unattempted, None),
        ]
    );
    assert_eq!(fs.read(&RelativePath::new("src/a.rs")).unwrap(), b"ALPHA");
    assert_eq!(fs.read(&b_path).unwrap(), b"BETA");
    let b_id = workspace.get_by_path(&b_path).unwrap();
    assert_eq!(workspace.get(b_id).unwrap(), &b_before.vfs_record);
    assert_eq!(storage.inner.get(b_id).unwrap(), b_before.storage_record);
    use crate::ports::inbound::SearchUseCase as _;
    assert_eq!(
        crate::domain::index::FileSearch::new(&index, &workspace)
            .find_file("b")
            .unwrap(),
        b_before.index_matches
    );
    assert_eq!(
        storage.inner.search_ids_for_test("b"),
        b_before.storage_matches
    );
    assert_eq!(fs.read(&RelativePath::new("src/c.rs")).unwrap(), b"gamma");
    assert_eq!(host.changed.lock().unwrap().as_slice(), ["src/a.rs"]);
}
