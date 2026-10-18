#![forbid(unsafe_code)]

use std::collections::HashMap;

use crate::adapters::{InMemoryFs, InMemoryStorage};
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::{FileMutationService, compute_content_version};
use crate::domain::refactor::{
    PathRenameError, PathRenameRequest, PathRenameSemanticOutcome, PathRenameSemanticPolicy,
    PathRenameStatus,
};
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{ContentHash, FileId, RelativePath, VirtualFile};
use crate::ports::inbound::{FileMutationUseCase, WorkspaceApplyEditsErrorCode, WorkspaceEditSpan};
use crate::ports::{
    ExtensionHostPort, FileSystemPort, NoOpExtensionHost, PathExclusionDecision,
    PathExclusionError, PathExclusionPort, PortError, ReconciliationDelta, StoragePort,
};

struct RejectRenameFs(InMemoryFs);

impl FileSystemPort for RejectRenameFs {
    fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
        self.0.read(path)
    }
    fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
        self.0.write(path, bytes)
    }
    fn rename(&mut self, _: &RelativePath, _: RelativePath) -> Result<(), PortError> {
        Err(PortError::WriteFailed("rejected".to_owned()))
    }
    fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
        self.0.delete(path)
    }
    fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
        self.0.mkdir(path)
    }
    fn scan(&self) -> Vec<RelativePath> {
        self.0.scan()
    }
}

#[derive(Default)]
struct CountingHost(std::sync::atomic::AtomicUsize);

impl ExtensionHostPort for CountingHost {
    fn on_file_indexed(&self, _: FileId, _: &RelativePath) {}
    fn on_file_changed(&self, _: FileId, _: &RelativePath) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    fn on_file_deleted(&self, _: &RelativePath) {}
    fn declared_tools(
        &self,
    ) -> Vec<(
        crate::domain::extension_host::ExtensionId,
        extension_protocol::ToolDecl,
    )> {
        Vec::new()
    }
    fn invoke(
        &self,
        name: &str,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, crate::domain::extension_host::InvokeError> {
        Err(crate::domain::extension_host::InvokeError::ToolNotFound(
            name.to_owned(),
        ))
    }
}
use crate::test_support::{InMemoryPathExclusion, PathExclusionFixture};

fn path(value: &str) -> RelativePath {
    RelativePath::new(value)
}

fn exclusions() -> InMemoryPathExclusion {
    InMemoryPathExclusion::new(PathExclusionFixture::default())
}

fn request(
    source: &str,
    destination: &str,
    dry_run: bool,
    semantic_policy: PathRenameSemanticPolicy,
) -> PathRenameRequest {
    PathRenameRequest {
        source: path(source),
        destination: path(destination),
        dry_run,
        semantic_policy,
        expected_versions: HashMap::new(),
    }
}

fn seeded(
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
        for (file_path, content) in files {
            service
                .create_file(path(file_path), content.to_vec())
                .unwrap();
        }
    }
    (fs, workspace, index, storage)
}

#[test]
fn file_mutation_use_case_exposes_object_safe_plan_finalize_apply_and_preserves_constructor() {
    fn assert_object_safe(_: &mut dyn FileMutationUseCase) {}

    let (mut fs, mut workspace, mut index, mut storage) =
        seeded(&[("src/old.rs", b"pub fn old() {}")]);
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    assert_object_safe(&mut service);

    let plan = service
        .plan_path_rename(
            request(
                "src/old.rs",
                "src/new.rs",
                false,
                PathRenameSemanticPolicy::None,
            ),
            &exclusions(),
        )
        .unwrap();
    let result = service.apply_path_rename(plan, &exclusions()).unwrap();

    assert_eq!(result.status, PathRenameStatus::Applied);
}

#[test]
fn plan_path_rename_validates_complete_move_set_without_mutating_ports() {
    let original = b"pub fn old() {}";
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("src/old.rs", original)]);
    let before = workspace.snapshot();
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );

    let plan = service
        .plan_path_rename(
            request(
                "src/old.rs",
                "src/new.rs",
                false,
                PathRenameSemanticPolicy::None,
            ),
            &exclusions(),
        )
        .unwrap();

    assert_eq!(plan.moves().len(), 1);
    let after = workspace.snapshot();
    assert_eq!(after.files, before.files);
    assert_eq!(after.slot_generations, before.slot_generations);
    assert_eq!(after.free_slots, before.free_slots);
    assert_eq!(fs.read(&path("src/old.rs")).unwrap(), original);
    assert_eq!(fs.read(&path("src/new.rs")), Err(PortError::NotFound));
}

#[test]
fn semantic_edit_targets_beneath_directory_source_are_remapped_with_ranges_and_hashes() {
    let bytes = b"mod old;";
    let base_hash = compute_content_version(bytes);
    let (mut fs, mut workspace, mut index, mut storage) =
        seeded(&[("src/old/mod.rs", bytes), ("src/old/nested.rs", b"nested")]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "src/old",
                "src/new",
                false,
                PathRenameSemanticPolicy::BestEffort,
            ),
            &exclusions(),
        )
        .unwrap();
    let edit = WorkspaceEditSpan {
        path: path("src/old/mod.rs"),
        start_byte: 4,
        end_byte: 7,
        replacement: "new".to_owned(),
        base_hash: Some(base_hash.clone()),
    };

    let ready = service
        .complete_path_rename_plan(native, Ok(vec![edit]))
        .unwrap();

    assert_eq!(ready.semantic_outcome(), PathRenameSemanticOutcome::Planned);
    assert_eq!(ready.edits()[0].path, path("src/new/mod.rs"));
    assert_eq!(
        (ready.edits()[0].start_byte, ready.edits()[0].end_byte),
        (4, 7)
    );
    assert_eq!(
        ready.edits()[0].base_hash.as_deref(),
        Some(base_hash.as_str())
    );
    assert_eq!(ready.moves().len(), 3);
}

#[test]
fn move_is_published_with_same_identity_and_persisted_before_deferred_edits() {
    let original = b"mod old;";
    let (mut fs, mut workspace, mut index, mut storage) =
        seeded(&[("src/old/mod.rs", original), ("src/old/other.rs", b"other")]);
    let old_id = workspace.get_by_path(&path("src/old/mod.rs")).unwrap();
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "src/old",
                "src/new",
                false,
                PathRenameSemanticPolicy::BestEffort,
            ),
            &exclusions(),
        )
        .unwrap();
    let ready = service
        .complete_path_rename_plan(native, Ok(Vec::new()))
        .unwrap();
    let result = service.apply_path_rename(ready, &exclusions()).unwrap();

    assert_eq!(result.status, PathRenameStatus::Applied);
    assert_eq!(workspace.get_by_path(&path("src/new/mod.rs")), Some(old_id));
    assert_eq!(workspace.get_by_path(&path("src/old/mod.rs")), None);
    assert_eq!(storage.get(old_id).unwrap().path, path("src/new/mod.rs"));
    assert_eq!(fs.read(&path("src/new/mod.rs")).unwrap(), original);
}

#[test]
fn dry_run_revalidates_ready_plan_and_mutates_nothing() {
    let original = b"old";
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", original)]);
    let old_id = workspace.get_by_path(&path("old.rs")).unwrap();
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let plan = service
        .plan_path_rename(
            request("old.rs", "new.rs", true, PathRenameSemanticPolicy::None),
            &exclusions(),
        )
        .unwrap();
    let result = service.apply_path_rename(plan, &exclusions()).unwrap();

    assert_eq!(result.status, PathRenameStatus::Preview);
    assert_eq!(workspace.get_by_path(&path("old.rs")), Some(old_id));
    assert_eq!(workspace.get_by_path(&path("new.rs")), None);
    assert_eq!(storage.get(old_id).unwrap().path, path("old.rs"));
    assert_eq!(fs.read(&path("old.rs")).unwrap(), original);
    assert_eq!(fs.read(&path("new.rs")), Err(PortError::NotFound));
}

#[test]
fn unavailable_required_semantics_is_semantic_unsupported_before_mutation() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::Required,
            ),
            &exclusions(),
        )
        .unwrap();

    assert_eq!(
        service.complete_path_rename_plan(native, Err(PathRenameError::SemanticUnsupported)),
        Err(PathRenameError::SemanticUnsupported)
    );
    assert!(workspace.get_by_path(&path("old.rs")).is_some());
    assert_eq!(fs.read(&path("new.rs")), Err(PortError::NotFound));
}

#[test]
fn best_effort_semantic_unavailability_commits_the_path_only_plan_with_a_sanitized_warning() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::BestEffort,
            ),
            &exclusions(),
        )
        .unwrap();
    let ready = service
        .complete_path_rename_plan(native, Err(PathRenameError::SemanticUnsupported))
        .unwrap();

    assert_eq!(
        ready.semantic_outcome(),
        PathRenameSemanticOutcome::Unsupported
    );
    assert_eq!(
        ready.semantic_errors(),
        &[PathRenameError::SemanticUnsupported]
    );
    let result = service.apply_path_rename(ready, &exclusions()).unwrap();
    assert_eq!(result.status, PathRenameStatus::Applied);
    assert_eq!(result.errors, vec![PathRenameError::SemanticUnsupported]);
    assert_eq!(fs.read(&path("new.rs")).unwrap(), b"old");
}

#[test]
fn best_effort_semantic_backend_failure_commits_the_path_only_plan_with_a_sanitized_warning() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::BestEffort,
            ),
            &exclusions(),
        )
        .unwrap();
    let ready = service
        .complete_path_rename_plan(native, Err(PathRenameError::SemanticBackend))
        .unwrap();

    assert_eq!(ready.semantic_outcome(), PathRenameSemanticOutcome::Failed);
    assert_eq!(ready.semantic_errors(), &[PathRenameError::SemanticBackend]);
    let result = service.apply_path_rename(ready, &exclusions()).unwrap();
    assert_eq!(result.status, PathRenameStatus::Applied);
    assert_eq!(result.errors, vec![PathRenameError::SemanticBackend]);
    assert_eq!(fs.read(&path("new.rs")).unwrap(), b"old");
}

#[test]
fn best_effort_external_semantic_target_commits_the_path_only_plan_with_workspace_escape() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::BestEffort,
            ),
            &exclusions(),
        )
        .unwrap();
    let escape = PathRenameError::WorkspaceEscape {
        path: path("external://secret/replacement"),
    };
    let ready = service
        .complete_path_rename_plan(native, Err(escape.clone()))
        .unwrap();

    assert_eq!(ready.semantic_outcome(), PathRenameSemanticOutcome::Failed);
    assert_eq!(ready.semantic_errors(), &[escape.clone()]);
    let result = service.apply_path_rename(ready, &exclusions()).unwrap();
    assert_eq!(result.status, PathRenameStatus::Applied);
    assert_eq!(result.errors, vec![escape]);
    assert_eq!(fs.read(&path("new.rs")).unwrap(), b"old");
}

#[test]
fn required_semantic_backend_failure_rejects_without_mutating_the_native_path() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::Required,
            ),
            &exclusions(),
        )
        .unwrap();

    assert_eq!(
        service.complete_path_rename_plan(native, Err(PathRenameError::SemanticBackend)),
        Err(PathRenameError::SemanticBackend)
    );
    assert_eq!(fs.read(&path("old.rs")).unwrap(), b"old");
    assert_eq!(fs.read(&path("new.rs")), Err(PortError::NotFound));
}

#[test]
fn preflight_rejects_directory_self_nesting_without_mutating_any_descendant() {
    let (mut fs, mut workspace, mut index, mut storage) =
        seeded(&[("src/old/a.rs", b"a"), ("src/old/nested/b.rs", b"b")]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );

    assert_eq!(
        service.plan_path_rename(
            request(
                "src/old",
                "src/old/nested/renamed",
                false,
                PathRenameSemanticPolicy::None,
            ),
            &exclusions(),
        ),
        Err(PathRenameError::DestinationInsideSource {
            source: path("src/old"),
            destination: path("src/old/nested/renamed"),
        })
    );
    assert_eq!(fs.read(&path("src/old/a.rs")).unwrap(), b"a");
    assert_eq!(fs.read(&path("src/old/nested/b.rs")).unwrap(), b"b");
}

#[test]
fn preflight_rejects_a_conflict_in_the_complete_directory_descendant_move_set() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[
        ("src/old/a.rs", b"a"),
        ("src/old/b.rs", b"b"),
        ("src/new/b.rs", b"existing"),
    ]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );

    assert_eq!(
        service.plan_path_rename(
            request("src/old", "src/new", false, PathRenameSemanticPolicy::None,),
            &exclusions(),
        ),
        Err(PathRenameError::DestinationExists {
            path: path("src/new/b.rs"),
        })
    );
    assert_eq!(fs.read(&path("src/old/a.rs")).unwrap(), b"a");
    assert_eq!(fs.read(&path("src/old/b.rs")).unwrap(), b"b");
    assert_eq!(fs.read(&path("src/new/b.rs")).unwrap(), b"existing");
}

struct SymlinkEscapingExclusions;

impl PathExclusionPort for SymlinkEscapingExclusions {
    fn classify(&self, path: &RelativePath) -> Result<PathExclusionDecision, PathExclusionError> {
        if path == &RelativePath::new("new.rs") {
            Err(PathExclusionError::SymlinkEscape)
        } else {
            Ok(PathExclusionDecision::Allowed)
        }
    }
}

#[test]
fn preflight_maps_symlink_escape_to_workspace_escape_without_mutation() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );

    assert_eq!(
        service.plan_path_rename(
            request("old.rs", "new.rs", false, PathRenameSemanticPolicy::None),
            &SymlinkEscapingExclusions,
        ),
        Err(PathRenameError::WorkspaceEscape {
            path: path("new.rs"),
        })
    );
    assert_eq!(fs.read(&path("old.rs")).unwrap(), b"old");
    assert_eq!(fs.read(&path("new.rs")), Err(PortError::NotFound));
}

#[test]
fn stale_semantic_hash_is_per_file_conflict_after_committed_move() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let native = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::BestEffort,
            ),
            &exclusions(),
        )
        .unwrap();
    let ready = service
        .complete_path_rename_plan(
            native,
            Ok(vec![WorkspaceEditSpan {
                path: path("old.rs"),
                start_byte: 0,
                end_byte: 3,
                replacement: "new".to_owned(),
                base_hash: Some(compute_content_version(b"stale")),
            }]),
        )
        .unwrap();
    let result = service.apply_path_rename(ready, &exclusions()).unwrap();

    assert_eq!(result.status, PathRenameStatus::Partial);
    let edit_result = result
        .edit_result
        .expect("move committed before edit conflict");
    assert_eq!(edit_result.per_file[0].path, path("new.rs"));
    assert_eq!(
        edit_result.per_file[0].error.as_ref().unwrap().code,
        WorkspaceApplyEditsErrorCode::Conflict
    );
    assert!(workspace.get_by_path(&path("new.rs")).is_some());
}

#[test]
fn exact_error_mapping_distinguishes_preflight_filesystem_and_caller_cas_failures() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    assert!(matches!(
        service.plan_path_rename(
            request(
                "missing.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::None
            ),
            &exclusions(),
        ),
        Err(PathRenameError::SourceNotFound { .. })
    ));
    assert!(matches!(
        service.plan_path_rename(
            request(
                "old.rs",
                ".git/new.rs",
                false,
                PathRenameSemanticPolicy::None
            ),
            &exclusions(),
        ),
        Err(PathRenameError::DestinationExcluded { .. })
    ));
    assert!(matches!(
        service.plan_path_rename(
            request("../old.rs", "new.rs", false, PathRenameSemanticPolicy::None),
            &exclusions(),
        ),
        Err(PathRenameError::WorkspaceEscape { .. })
    ));

    let mut guarded = request("old.rs", "new.rs", false, PathRenameSemanticPolicy::None);
    guarded
        .expected_versions
        .insert(path("old.rs"), compute_content_version(b"before"));
    assert!(matches!(
        service.plan_path_rename(guarded, &exclusions()),
        Err(PathRenameError::VersionConflict { path: conflict_path, .. })
            if conflict_path == path("old.rs")
    ));

    let mut guarded = request("old.rs", "new.rs", false, PathRenameSemanticPolicy::None);
    guarded
        .expected_versions
        .insert(path("old.rs"), compute_content_version(b"old"));
    let plan = service.plan_path_rename(guarded, &exclusions()).unwrap();
    fs.write(path("old.rs"), b"changed".to_vec()).unwrap();
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    assert!(matches!(
        service.apply_path_rename(plan, &exclusions()),
        Err(PathRenameError::VersionConflict { path: conflict_path, .. })
            if conflict_path == path("old.rs")
    ));

    struct FailingRenameFs(InMemoryFs);
    impl FileSystemPort for FailingRenameFs {
        fn read(&self, path: &RelativePath) -> Result<Vec<u8>, PortError> {
            self.0.read(path)
        }
        fn write(&mut self, path: RelativePath, bytes: Vec<u8>) -> Result<(), PortError> {
            self.0.write(path, bytes)
        }
        fn rename(&mut self, _: &RelativePath, _: RelativePath) -> Result<(), PortError> {
            Err(PortError::WriteFailed("raw backend sentinel".to_owned()))
        }
        fn delete(&mut self, path: &RelativePath) -> Result<(), PortError> {
            self.0.delete(path)
        }
        fn mkdir(&mut self, path: RelativePath) -> Result<(), PortError> {
            self.0.mkdir(path)
        }
        fn scan(&self) -> Vec<RelativePath> {
            self.0.scan()
        }
    }

    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let plan = service
        .plan_path_rename(
            request("old.rs", "new.rs", false, PathRenameSemanticPolicy::None),
            &exclusions(),
        )
        .unwrap();
    let mut failing_fs = FailingRenameFs(fs);
    let mut service = FileMutationService::new(
        &mut failing_fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    assert_eq!(
        service.apply_path_rename(plan, &exclusions()),
        Err(PathRenameError::Filesystem)
    );
}

struct FailingReconciliationStorage(InMemoryStorage);

impl StoragePort for FailingReconciliationStorage {
    fn apply_reconciliation_delta(&mut self, _: &ReconciliationDelta) -> Result<(), PortError> {
        Err(PortError::ReconciliationFailed {
            operation: "apply_reconciliation_delta",
            reason: "backend diagnostic sentinel /absolute/private/path".to_owned(),
        })
    }

    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
        self.0.get(id)
    }
    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
        self.0.put(file)
    }
    fn put_batch(&mut self, files: &[VirtualFile]) -> Result<(), PortError> {
        self.0.put_batch(files)
    }
    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
        self.0.delete(id)
    }
    fn put_blob(&mut self, hash: ContentHash, bytes: Vec<u8>) -> Result<(), PortError> {
        self.0.put_blob(hash, bytes)
    }
    fn get_blob(&self, hash: &ContentHash) -> Result<Vec<u8>, PortError> {
        self.0.get_blob(hash)
    }
    fn mark_scan_complete(&mut self) -> Result<(), PortError> {
        self.0.mark_scan_complete()
    }
    fn is_scan_complete(&self) -> Result<bool, PortError> {
        self.0.is_scan_complete()
    }
}

#[test]
fn persistence_failure_keeps_destination_skips_edits_and_returns_sanitized_partial() {
    let (mut fs, mut workspace, mut index, storage) = seeded(&[("old.rs", b"old")]);
    let mut storage = FailingReconciliationStorage(storage);
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let plan = service
        .plan_path_rename(
            request("old.rs", "new.rs", false, PathRenameSemanticPolicy::None),
            &exclusions(),
        )
        .unwrap();
    let result = service.apply_path_rename(plan, &exclusions()).unwrap();

    assert_eq!(result.status, PathRenameStatus::Partial);
    assert_eq!(result.errors, vec![PathRenameError::Persistence]);
    assert!(result.edit_result.is_none());
    assert!(workspace.get_by_path(&path("new.rs")).is_some());
    let rendered = format!("{:?} {}", result.errors, result.errors[0]);
    assert!(!rendered.contains("sentinel"));
    assert!(!rendered.contains("/absolute/private/path"));
}

#[test]
fn semantic_completion_rejects_or_discards_an_invalid_target_as_one_batch() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    let invalid = WorkspaceEditSpan {
        path: path("../outside.rs"),
        start_byte: 0,
        end_byte: 0,
        replacement: "x".to_owned(),
        base_hash: None,
    };

    let best_effort = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::BestEffort,
            ),
            &exclusions(),
        )
        .unwrap();
    let ready = service
        .complete_path_rename_plan(best_effort, Ok(vec![invalid.clone()]))
        .unwrap();
    assert!(ready.edits().is_empty());
    assert!(matches!(
        ready.semantic_errors(),
        [PathRenameError::WorkspaceEscape { .. }]
    ));

    let required = service
        .plan_path_rename(
            request(
                "old.rs",
                "new.rs",
                false,
                PathRenameSemanticPolicy::Required,
            ),
            &exclusions(),
        )
        .unwrap();
    assert!(matches!(
        service.complete_path_rename_plan(required, Ok(vec![invalid])),
        Err(PathRenameError::WorkspaceEscape { .. })
    ));
}

#[test]
fn apply_rejects_intervening_directory_descendant_before_filesystem_movement() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old/a.rs", b"a")]);
    let plan = {
        let service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .plan_path_rename(
                request("old", "new", false, PathRenameSemanticPolicy::None),
                &exclusions(),
            )
            .unwrap()
    };
    {
        let mut service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .create_file(path("old/late.rs"), b"late".to_vec())
            .unwrap();
    }
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    assert!(matches!(
        service.apply_path_rename(plan, &exclusions()),
        Err(PathRenameError::SourceNotFound { .. })
    ));
    assert_eq!(fs.read(&path("old/a.rs")).unwrap(), b"a");
    assert_eq!(fs.read(&path("old/late.rs")).unwrap(), b"late");
}

#[test]
fn apply_rejects_an_unindexed_filesystem_destination_without_overwriting_it() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let plan = {
        let service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .plan_path_rename(
                request("old.rs", "new.rs", false, PathRenameSemanticPolicy::None),
                &exclusions(),
            )
            .unwrap()
    };
    fs.write(path("new.rs"), b"external".to_vec()).unwrap();
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    assert_eq!(
        service.apply_path_rename(plan, &exclusions()),
        Err(PathRenameError::DestinationExists {
            path: path("new.rs")
        })
    );
    assert_eq!(fs.read(&path("old.rs")).unwrap(), b"old");
    assert_eq!(fs.read(&path("new.rs")).unwrap(), b"external");
}

#[test]
fn failed_directory_root_rename_never_falls_back_to_partial_child_moves() {
    let (mut fs, mut workspace, mut index, mut storage) =
        seeded(&[("old/a.rs", b"a"), ("old/b.rs", b"b")]);
    let plan = {
        let service = FileMutationService::new(
            &mut fs,
            &mut workspace,
            &mut index,
            &mut storage,
            &NoOpExtensionHost,
        );
        service
            .plan_path_rename(
                request("old", "new", false, PathRenameSemanticPolicy::None),
                &exclusions(),
            )
            .unwrap()
    };
    let mut fs = RejectRenameFs(fs);
    let mut service = FileMutationService::new(
        &mut fs,
        &mut workspace,
        &mut index,
        &mut storage,
        &NoOpExtensionHost,
    );
    assert_eq!(
        service.apply_path_rename(plan, &exclusions()),
        Err(PathRenameError::Filesystem)
    );
    assert_eq!(fs.read(&path("old/a.rs")).unwrap(), b"a");
    assert_eq!(fs.read(&path("old/b.rs")).unwrap(), b"b");
    assert_eq!(fs.read(&path("new/a.rs")), Err(PortError::NotFound));
}

#[test]
fn path_rename_collects_no_inline_extension_notifications() {
    let (mut fs, mut workspace, mut index, mut storage) = seeded(&[("old.rs", b"old")]);
    let host = CountingHost::default();
    let mut service =
        FileMutationService::new(&mut fs, &mut workspace, &mut index, &mut storage, &host);
    let plan = service
        .plan_path_rename(
            request("old.rs", "new.rs", false, PathRenameSemanticPolicy::None),
            &exclusions(),
        )
        .unwrap();
    service.apply_path_rename(plan, &exclusions()).unwrap();
    assert_eq!(host.0.load(std::sync::atomic::Ordering::Relaxed), 0);
}
