//! F003 T010 contract tests for host-owned apply-edits DTOs.
#![forbid(unsafe_code)]

use crate::adapters::{InMemoryFs, InMemoryStorage};
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::{FileMutationService, compute_content_version};
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, FileId, RelativePath};
use crate::ports::inbound::{
    ApplyEditsFileResult, ApplyEditsPreview, ApplyEditsRequest, FileMutationUseCase, SkippedEdit,
    SkippedEditReason, TextEdit,
};
use crate::ports::{ExtensionHostPort, FileSystemPort, NoOpExtensionHost};
use std::sync::Mutex;

fn assert_file_mutation_use_case_apply_edits_contract<T: FileMutationUseCase + ?Sized>() {
    let _: fn(&mut T, ApplyEditsRequest) -> Result<ApplyEditsFileResult, DomainError> =
        T::apply_edits_cas;
    let _: fn(&T, ApplyEditsRequest) -> Result<ApplyEditsFileResult, DomainError> =
        T::apply_edits_dry_run;
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

#[derive(Default)]
struct RecordingHost {
    changed: Mutex<Vec<String>>,
}

impl ExtensionHostPort for RecordingHost {
    fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

    fn on_file_changed(&self, _id: FileId, path: &RelativePath) {
        self.changed.lock().unwrap().push(path.as_str().to_owned());
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
