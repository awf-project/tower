use std::collections::HashMap;

use extension_protocol::messages::{
    CleanupCoverage, SafeDeleteBlockingReason, SafeDeleteByteSpan, SafeDeleteCleanupEdit,
    SafeDeleteDeclarationOutcome, SafeDeleteDeclarationPlan, SafeDeleteFileError,
    SafeDeleteFileOutcome, SafeDeleteFileOutcomeStatus, SafeDeleteMutationRequest,
    SafeDeleteMutationResult, SafeDeleteMutationStatus, SafeDeletePosition, SafeDeleteReference,
    SafeDeleteRejectionCode, SafeDeleteRequest, SafeDeleteSemanticOutcome, SafeDeleteSemanticPlan,
    SafeDeleteStatus,
};
use sha2::{Digest, Sha256};

use super::{
    ResolvedDeclaration, SafeDeleteAnalysis, SafeDeletePlan, SafeDeleteService, SafeDeleteSnapshot,
};
use crate::domain::refactor::{
    PathRenameError, PathRenamePlan, PathRenameRequest, PathRenameResult,
};
use crate::domain::{DomainError, RelativePath};
use crate::ports::PathExclusionPort;
use crate::ports::inbound::{
    ApplyEditsFileResult, ApplyEditsRequest, FileMutationUseCase, SafeDeleteUseCase, TxReport,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult, WorkspaceEditSpan,
};

const DECLARATION_PATH: &str = "src/declaration.rs";
const DECLARATION_CONTENT: &str = "fn gone() {}\nfn keep() { gone(); }\n";

#[derive(Default)]
struct RecordingMutation {
    calls: Vec<SafeDeleteMutationRequest>,
    response: Option<SafeDeleteMutationResult>,
    snapshots: HashMap<String, String>,
    side_effects: usize,
}

impl RecordingMutation {
    fn returning(response: SafeDeleteMutationResult) -> Self {
        Self {
            calls: Vec::new(),
            response: Some(response),
            snapshots: HashMap::new(),
            side_effects: 0,
        }
    }

    fn load_snapshots(&mut self, snapshots: &[SafeDeleteSnapshot]) {
        self.snapshots = snapshots
            .iter()
            .map(|snapshot| (snapshot.path.as_str().to_owned(), snapshot.content.clone()))
            .collect();
    }

    fn preflight(&self, request: &SafeDeleteMutationRequest) -> Option<SafeDeleteRejectionCode> {
        if request.edits.is_empty()
            || request.edits.windows(2).any(|pair| {
                (
                    &pair[0].path,
                    pair[0].start_byte,
                    pair[0].end_byte,
                    &pair[0].replacement,
                ) > (
                    &pair[1].path,
                    pair[1].start_byte,
                    pair[1].end_byte,
                    &pair[1].replacement,
                )
            })
        {
            return Some(SafeDeleteRejectionCode::InvalidRange);
        }

        for edit in &request.edits {
            let Some(content) = self.snapshots.get(&edit.path) else {
                return Some(SafeDeleteRejectionCode::InvalidPath);
            };
            if edit.base_hash != hash(content)
                || edit.start_byte > edit.end_byte
                || edit.end_byte > content.len()
                || !content.is_char_boundary(edit.start_byte)
                || !content.is_char_boundary(edit.end_byte)
            {
                return Some(SafeDeleteRejectionCode::InvalidRange);
            }
        }

        for (index, edit) in request.edits.iter().enumerate() {
            if request.edits[index + 1..].iter().any(|other| {
                edit.path == other.path
                    && edit.start_byte < other.end_byte
                    && other.start_byte < edit.end_byte
            }) {
                return Some(SafeDeleteRejectionCode::OverlappingEdits);
            }
        }

        None
    }
}

impl FileMutationUseCase for RecordingMutation {
    fn plan_path_rename(
        &self,
        _request: PathRenameRequest,
        _exclusions: &dyn PathExclusionPort,
    ) -> Result<PathRenamePlan, PathRenameError> {
        unreachable!("safe-delete policy must not plan path renames")
    }

    fn complete_path_rename_plan(
        &self,
        _plan: PathRenamePlan,
        _semantic: Result<Vec<WorkspaceEditSpan>, PathRenameError>,
    ) -> Result<PathRenamePlan, PathRenameError> {
        unreachable!("safe-delete policy must not complete path renames")
    }

    fn apply_path_rename(
        &mut self,
        _plan: PathRenamePlan,
        _exclusions: &dyn PathExclusionPort,
    ) -> Result<PathRenameResult, PathRenameError> {
        unreachable!("safe-delete policy must not apply path renames")
    }

    fn create_file(&mut self, _path: RelativePath, _content: Vec<u8>) -> Result<(), DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn create_directory(&mut self, _path: RelativePath) -> Result<(), DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn delete_file(&mut self, _path: &RelativePath) -> Result<(), DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn global_replace(
        &mut self,
        _target: &str,
        _replacement: &str,
    ) -> Result<TxReport, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn global_replace_dry_run(
        &mut self,
        _target: &str,
        _replacement: &str,
    ) -> Result<TxReport, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn create_file_cas(
        &mut self,
        _path: RelativePath,
        _content: Vec<u8>,
        _expected_version: Option<String>,
    ) -> Result<(), DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn edit_range(
        &mut self,
        _path: &RelativePath,
        _start_byte: usize,
        _end_byte: usize,
        _replacement: &str,
    ) -> Result<TxReport, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn edit_range_cas(
        &mut self,
        _path: &RelativePath,
        _start_byte: usize,
        _end_byte: usize,
        _replacement: &str,
        _expected_version: Option<String>,
    ) -> Result<TxReport, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn apply_edits_cas(
        &mut self,
        _request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn apply_edits_dry_run(
        &self,
        _request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn apply_batch_edits(
        &mut self,
        _request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }

    fn apply_safe_delete_edits(
        &mut self,
        request: SafeDeleteMutationRequest,
    ) -> Result<SafeDeleteMutationResult, DomainError> {
        let rejection = self.preflight(&request);
        self.calls.push(request);
        if let Some(rejection) = rejection {
            return Ok(SafeDeleteMutationResult {
                status: SafeDeleteMutationStatus::Rejected,
                rejection: Some(rejection),
                per_file: Vec::new(),
            });
        }

        let dry_run = self
            .calls
            .last()
            .expect("the request is recorded before a successful preflight")
            .dry_run;
        if !dry_run {
            self.side_effects += 1;
        }
        Ok(self.response.clone().unwrap_or_else(|| {
            if dry_run {
                SafeDeleteMutationResult {
                    status: SafeDeleteMutationStatus::Previewed,
                    rejection: None,
                    per_file: Vec::new(),
                }
            } else {
                applied_result()
            }
        }))
    }

    fn global_replace_cas(
        &mut self,
        _target: &str,
        _replacement: &str,
        _expected_versions: HashMap<RelativePath, String>,
    ) -> Result<TxReport, DomainError> {
        unreachable!("safe-delete policy must use its dedicated mutation operation")
    }
}

fn hash(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn request(dry_run: bool) -> SafeDeleteRequest {
    SafeDeleteRequest {
        path: DECLARATION_PATH.to_owned(),
        symbol_name: "gone".to_owned(),
        kind: Some("function".to_owned()),
        dry_run,
    }
}

fn declaration() -> SafeDeleteDeclarationPlan {
    SafeDeleteDeclarationPlan {
        path: DECLARATION_PATH.to_owned(),
        symbol_name: "gone".to_owned(),
        kind: "function".to_owned(),
        position: position(0, 3),
        deletion_span: SafeDeleteByteSpan {
            start_byte: 0,
            end_byte: 13,
        },
        base_hash: hash(DECLARATION_CONTENT),
    }
}

fn position(line: u32, character: u32) -> SafeDeletePosition {
    SafeDeletePosition { line, character }
}

fn reference(path: &str, start: (u32, u32), end: (u32, u32), content: &str) -> SafeDeleteReference {
    SafeDeleteReference {
        path: path.to_owned(),
        start: position(start.0, start.1),
        end: position(end.0, end.1),
        base_hash: hash(content),
    }
}

fn edit(
    path: &str,
    start_byte: usize,
    end_byte: usize,
    replacement: &str,
    content: &str,
) -> SafeDeleteCleanupEdit {
    SafeDeleteCleanupEdit {
        path: path.to_owned(),
        start_byte,
        end_byte,
        replacement: replacement.to_owned(),
        base_hash: hash(content),
    }
}

fn snapshot(path: &str, content: &str) -> SafeDeleteSnapshot {
    SafeDeleteSnapshot {
        path: RelativePath::new(path),
        content: content.to_owned(),
    }
}

fn complete(
    references: Vec<SafeDeleteReference>,
    cleanup_edits: Vec<SafeDeleteCleanupEdit>,
) -> SafeDeleteSemanticOutcome {
    SafeDeleteSemanticOutcome::Complete {
        plan: SafeDeleteSemanticPlan {
            references,
            cleanup_edits,
            coverage: CleanupCoverage::Complete,
        },
    }
}

fn incomplete(references: Vec<SafeDeleteReference>) -> SafeDeleteSemanticOutcome {
    SafeDeleteSemanticOutcome::Incomplete {
        plan: SafeDeleteSemanticPlan {
            references,
            cleanup_edits: Vec::new(),
            coverage: CleanupCoverage::Incomplete,
        },
    }
}

fn applied_result() -> SafeDeleteMutationResult {
    SafeDeleteMutationResult {
        status: SafeDeleteMutationStatus::Applied,
        rejection: None,
        per_file: vec![SafeDeleteFileOutcome {
            path: DECLARATION_PATH.to_owned(),
            outcome: SafeDeleteFileOutcomeStatus::Committed,
            error: None,
        }],
    }
}

fn run(
    mutation: &mut RecordingMutation,
    dry_run: bool,
    declaration_outcome: SafeDeleteDeclarationOutcome,
    semantic: SafeDeleteSemanticOutcome,
    snapshots: Vec<SafeDeleteSnapshot>,
) -> extension_protocol::messages::SafeDeleteReport {
    mutation.load_snapshots(&snapshots);
    SafeDeleteService::new(mutation)
        .safe_delete(request(dry_run), declaration_outcome, semantic, snapshots)
        .expect("policy decisions are represented by a safe-delete report")
}

#[test]
fn unique_unused_declaration_produces_validated_deletion_plan_and_applies_once() {
    let mut mutation = RecordingMutation::default();

    let report = run(
        &mut mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(Vec::new(), Vec::new()),
        vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
    );

    assert_eq!(report.status, SafeDeleteStatus::Applied);
    assert_eq!(report.blocking_reason, None);
    assert_eq!(mutation.calls.len(), 1);
    assert_eq!(
        mutation.calls[0],
        SafeDeleteMutationRequest {
            edits: vec![edit(DECLARATION_PATH, 0, 13, "", DECLARATION_CONTENT)],
            dry_run: false,
        }
    );
}

#[test]
fn dry_run_uses_same_port_once_and_returns_read_only_preflight_result() {
    let snapshots = vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)];
    let mut apply_mutation = RecordingMutation::default();
    let apply_report = run(
        &mut apply_mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(Vec::new(), Vec::new()),
        snapshots.clone(),
    );
    let mut preview_mutation = RecordingMutation::default();
    let preview_report = run(
        &mut preview_mutation,
        true,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(Vec::new(), Vec::new()),
        snapshots,
    );

    assert_eq!(apply_report.status, SafeDeleteStatus::Applied);
    assert_eq!(preview_report.status, SafeDeleteStatus::Previewed);
    assert_eq!(apply_mutation.calls.len(), 1);
    assert_eq!(preview_mutation.calls.len(), 1);
    assert_eq!(
        apply_mutation.calls[0].edits,
        preview_mutation.calls[0].edits
    );
    assert!(!apply_mutation.calls[0].dry_run);
    assert!(preview_mutation.calls[0].dry_run);
    assert_eq!(apply_mutation.side_effects, 1);
    assert_eq!(preview_mutation.side_effects, 0);
}

#[test]
fn snapshot_hashes_are_validated_before_semantic_outcomes_and_coordinates() {
    let reference_content = "gone();\n";
    let edit_content = "gone();\n";
    let cases = vec![
        (
            incomplete(Vec::new()),
            vec![snapshot(DECLARATION_PATH, "fn changed() {}\n")],
            SafeDeleteBlockingReason::CasConflict,
        ),
        (
            complete(
                vec![reference(
                    "src/reference.rs",
                    (0, 0),
                    (0, 4),
                    reference_content,
                )],
                Vec::new(),
            ),
            vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
            SafeDeleteBlockingReason::ReferencesUnresolved,
        ),
        (
            complete(
                Vec::new(),
                vec![edit("src/edit.rs", 0, 4, "", edit_content)],
            ),
            vec![
                snapshot(DECLARATION_PATH, DECLARATION_CONTENT),
                snapshot("src/edit.rs", edit_content),
                snapshot("src/edit.rs", edit_content),
            ],
            SafeDeleteBlockingReason::ReferencesUnresolved,
        ),
        (
            complete(Vec::new(), Vec::new()),
            vec![snapshot(DECLARATION_PATH, "fn gone() {}\n")],
            SafeDeleteBlockingReason::CasConflict,
        ),
    ];

    for (semantic, snapshots, expected_reason) in cases {
        let mut mutation = RecordingMutation::default();
        let report = run(
            &mut mutation,
            false,
            SafeDeleteDeclarationOutcome::Planned {
                plan: declaration(),
            },
            semantic,
            snapshots,
        );
        assert_eq!(report.status, SafeDeleteStatus::Blocked);
        assert_eq!(report.blocking_reason, Some(expected_reason));
        assert!(mutation.calls.is_empty());
    }
}

#[test]
fn only_complete_declaration_internal_references_are_filtered() {
    let internal = reference(DECLARATION_PATH, (0, 3), (0, 7), DECLARATION_CONTENT);
    let crossing = reference(DECLARATION_PATH, (0, 3), (1, 4), DECLARATION_CONTENT);
    let split_surrogate = reference("src/unicode.rs", (0, 1), (0, 2), "😀\r\ngone();\r\n");

    let mut mutation = RecordingMutation::default();
    let internal_report = run(
        &mut mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(vec![internal], Vec::new()),
        vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
    );
    assert_eq!(internal_report.status, SafeDeleteStatus::Applied);
    assert!(internal_report.references.is_empty());

    for (reference, snapshots) in [
        (
            crossing,
            vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
        ),
        (
            split_surrogate,
            vec![
                snapshot(DECLARATION_PATH, DECLARATION_CONTENT),
                snapshot("src/unicode.rs", "😀\r\ngone();\r\n"),
            ],
        ),
    ] {
        let mut mutation = RecordingMutation::default();
        let report = run(
            &mut mutation,
            false,
            SafeDeleteDeclarationOutcome::Planned {
                plan: declaration(),
            },
            complete(vec![reference], Vec::new()),
            snapshots,
        );
        assert_eq!(report.status, SafeDeleteStatus::Blocked);
        assert_eq!(
            report.blocking_reason,
            Some(SafeDeleteBlockingReason::InvalidCleanup)
        );
        assert!(mutation.calls.is_empty());
    }
}

#[test]
fn coordinates_accept_utf16_and_crlf_and_reject_out_of_bounds_locations() {
    let unicode_content = "😀gone\r\n";
    let crlf_content = "gone\r\n";
    let valid_cases = [
        (
            "src/unicode.rs",
            unicode_content,
            reference("src/unicode.rs", (0, 2), (0, 6), unicode_content),
            edit("src/unicode.rs", 4, 8, "", unicode_content),
        ),
        (
            "src/crlf.rs",
            crlf_content,
            reference("src/crlf.rs", (0, 0), (0, 4), crlf_content),
            edit("src/crlf.rs", 0, 4, "", crlf_content),
        ),
    ];
    for (path, content, live_reference, cleanup_edit) in valid_cases {
        let mut mutation = RecordingMutation::default();
        let report = run(
            &mut mutation,
            false,
            SafeDeleteDeclarationOutcome::Planned {
                plan: declaration(),
            },
            complete(vec![live_reference], vec![cleanup_edit]),
            vec![
                snapshot(DECLARATION_PATH, DECLARATION_CONTENT),
                snapshot(path, content),
            ],
        );
        assert_eq!(report.status, SafeDeleteStatus::Applied);
        assert_eq!(mutation.calls.len(), 1);
    }

    let exact_boundary = reference(DECLARATION_PATH, (0, 0), (1, 0), DECLARATION_CONTENT);
    let mut mutation = RecordingMutation::default();
    let report = run(
        &mut mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(vec![exact_boundary], Vec::new()),
        vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
    );
    assert_eq!(report.status, SafeDeleteStatus::Applied);
    assert!(report.references.is_empty());

    let mut mutation = RecordingMutation::default();
    let report = run(
        &mut mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(
            vec![reference("src/out_of_bounds.rs", (2, 0), (2, 1), "gone\n")],
            Vec::new(),
        ),
        vec![
            snapshot(DECLARATION_PATH, DECLARATION_CONTENT),
            snapshot("src/out_of_bounds.rs", "gone\n"),
        ],
    );
    assert_eq!(report.status, SafeDeleteStatus::Blocked);
    assert_eq!(
        report.blocking_reason,
        Some(SafeDeleteBlockingReason::InvalidCleanup)
    );
    assert!(mutation.calls.is_empty());
}

#[test]
fn coverage_attestation_blocks_unhandled_or_incomplete_live_references() {
    let live = reference(DECLARATION_PATH, (1, 12), (1, 16), DECLARATION_CONTENT);
    let cases = [
        (
            complete(vec![live.clone()], Vec::new()),
            SafeDeleteBlockingReason::LiveReferences,
        ),
        (
            incomplete(Vec::new()),
            SafeDeleteBlockingReason::CleanupIncomplete,
        ),
        (
            incomplete(vec![live]),
            SafeDeleteBlockingReason::CleanupIncomplete,
        ),
        (
            incomplete(vec![reference(
                DECLARATION_PATH,
                (0, 3),
                (0, 7),
                DECLARATION_CONTENT,
            )]),
            SafeDeleteBlockingReason::CleanupIncomplete,
        ),
    ];

    for (semantic, reason) in cases {
        let mut mutation = RecordingMutation::default();
        let report = run(
            &mut mutation,
            false,
            SafeDeleteDeclarationOutcome::Planned {
                plan: declaration(),
            },
            semantic,
            vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
        );
        assert_eq!(report.blocking_reason, Some(reason));
        assert!(mutation.calls.is_empty());
    }
}

#[test]
fn typed_complete_outcome_with_incomplete_plan_coverage_fails_closed() {
    let semantic = SafeDeleteSemanticOutcome::Complete {
        plan: SafeDeleteSemanticPlan {
            references: Vec::new(),
            cleanup_edits: Vec::new(),
            coverage: CleanupCoverage::Incomplete,
        },
    };
    let mut mutation = RecordingMutation::default();

    let report = run(
        &mut mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        semantic,
        vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
    );

    assert_eq!(report.status, SafeDeleteStatus::Blocked);
    assert_eq!(
        report.blocking_reason,
        Some(SafeDeleteBlockingReason::CleanupIncomplete)
    );
    assert!(mutation.calls.is_empty());
}

#[test]
fn exact_declaration_and_backend_failures_map_to_blocking_reasons() {
    let cases = [
        (
            SafeDeleteDeclarationOutcome::NotFound,
            SafeDeleteSemanticOutcome::BackendTimeout,
            SafeDeleteBlockingReason::NotFound,
        ),
        (
            SafeDeleteDeclarationOutcome::AmbiguousSymbol,
            SafeDeleteSemanticOutcome::BackendTimeout,
            SafeDeleteBlockingReason::AmbiguousSymbol,
        ),
        (
            SafeDeleteDeclarationOutcome::InvalidDeclaration,
            SafeDeleteSemanticOutcome::BackendTimeout,
            SafeDeleteBlockingReason::InvalidDeclaration,
        ),
    ];
    for (declaration_outcome, semantic, reason) in cases {
        let mut mutation = RecordingMutation::default();
        let report = run(
            &mut mutation,
            false,
            declaration_outcome,
            semantic,
            Vec::new(),
        );
        assert_eq!(report.blocking_reason, Some(reason));
        assert!(mutation.calls.is_empty());
    }

    for (semantic, reason) in [
        (
            SafeDeleteSemanticOutcome::CapabilityUnavailable,
            SafeDeleteBlockingReason::CapabilityUnavailable,
        ),
        (
            SafeDeleteSemanticOutcome::InvalidLocation,
            SafeDeleteBlockingReason::InvalidCleanup,
        ),
        (
            SafeDeleteSemanticOutcome::WorkspaceExternal,
            SafeDeleteBlockingReason::InvalidCleanup,
        ),
        (
            SafeDeleteSemanticOutcome::BackendTimeout,
            SafeDeleteBlockingReason::BackendError,
        ),
        (
            SafeDeleteSemanticOutcome::BackendCrashed,
            SafeDeleteBlockingReason::BackendError,
        ),
        (
            SafeDeleteSemanticOutcome::MalformedResponse,
            SafeDeleteBlockingReason::BackendError,
        ),
    ] {
        let mut mutation = RecordingMutation::default();
        let report = run(
            &mut mutation,
            false,
            SafeDeleteDeclarationOutcome::Planned {
                plan: declaration(),
            },
            semantic,
            vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
        );
        assert_eq!(report.blocking_reason, Some(reason));
        assert!(mutation.calls.is_empty());
    }
}

#[test]
fn complete_plan_is_sorted_once_and_partial_commit_order_is_preserved() {
    let a_content = "gone();\n";
    let z_content = "gone(); gone();\n";
    let a_ref = reference("src/a.rs", (0, 0), (0, 4), a_content);
    let z_first_ref = reference("src/z.rs", (0, 0), (0, 4), z_content);
    let z_ref = reference("src/z.rs", (0, 8), (0, 12), z_content);
    let a_edit = edit("src/a.rs", 0, 7, "", a_content);
    let declaration_edit = edit(DECLARATION_PATH, 0, 13, "", DECLARATION_CONTENT);
    let z_first_edit = edit("src/z.rs", 0, 7, "", z_content);
    let z_second_edit = edit("src/z.rs", 8, 15, "", z_content);
    let partial = SafeDeleteMutationResult {
        status: SafeDeleteMutationStatus::PartialCommit,
        rejection: None,
        per_file: vec![
            SafeDeleteFileOutcome {
                path: "src/a.rs".to_owned(),
                outcome: SafeDeleteFileOutcomeStatus::Committed,
                error: None,
            },
            SafeDeleteFileOutcome {
                path: "src/z.rs".to_owned(),
                outcome: SafeDeleteFileOutcomeStatus::Failed,
                error: Some(SafeDeleteFileError::PersistenceError),
            },
            SafeDeleteFileOutcome {
                path: DECLARATION_PATH.to_owned(),
                outcome: SafeDeleteFileOutcomeStatus::Unattempted,
                error: None,
            },
        ],
    };
    let mut mutation = RecordingMutation::returning(partial.clone());

    let report = run(
        &mut mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(
            vec![z_ref.clone(), a_ref.clone(), z_first_ref.clone()],
            vec![z_second_edit.clone(), a_edit.clone(), z_first_edit.clone()],
        ),
        vec![
            snapshot("src/z.rs", z_content),
            snapshot(DECLARATION_PATH, DECLARATION_CONTENT),
            snapshot("src/a.rs", a_content),
        ],
    );

    assert_eq!(report.status, SafeDeleteStatus::PartialCommit);
    assert_eq!(report.references, vec![a_ref.clone(), z_first_ref, z_ref]);
    assert_eq!(
        report.affected_files,
        vec!["src/a.rs", "src/declaration.rs", "src/z.rs"]
    );
    assert_eq!(report.mutation_result, Some(partial));
    assert_eq!(mutation.calls.len(), 1);
    assert_eq!(
        mutation.calls[0].edits,
        vec![a_edit, declaration_edit, z_first_edit, z_second_edit]
    );
}

#[test]
fn invalid_overlapping_and_mutation_rejected_plans_map_exactly_without_side_effects() {
    let overlapping = vec![
        edit(DECLARATION_PATH, 0, 8, "", DECLARATION_CONTENT),
        edit(DECLARATION_PATH, 4, 10, "", DECLARATION_CONTENT),
    ];
    let mut mutation = RecordingMutation::default();
    let report = run(
        &mut mutation,
        false,
        SafeDeleteDeclarationOutcome::Planned {
            plan: declaration(),
        },
        complete(Vec::new(), overlapping),
        vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
    );
    assert_eq!(
        report.blocking_reason,
        Some(SafeDeleteBlockingReason::InvalidCleanup)
    );
    assert!(mutation.calls.is_empty());

    for (rejection, reason) in [
        (
            SafeDeleteRejectionCode::CasConflict,
            SafeDeleteBlockingReason::CasConflict,
        ),
        (
            SafeDeleteRejectionCode::InvalidRange,
            SafeDeleteBlockingReason::InvalidCleanup,
        ),
        (
            SafeDeleteRejectionCode::InvalidPath,
            SafeDeleteBlockingReason::InvalidCleanup,
        ),
        (
            SafeDeleteRejectionCode::OverlappingEdits,
            SafeDeleteBlockingReason::InvalidCleanup,
        ),
    ] {
        let mut mutation = RecordingMutation::returning(SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Rejected,
            rejection: Some(rejection),
            per_file: Vec::new(),
        });
        let report = run(
            &mut mutation,
            true,
            SafeDeleteDeclarationOutcome::Planned {
                plan: declaration(),
            },
            complete(Vec::new(), Vec::new()),
            vec![snapshot(DECLARATION_PATH, DECLARATION_CONTENT)],
        );
        assert_eq!(report.blocking_reason, Some(reason));
        assert_eq!(mutation.calls.len(), 1);
        assert!(mutation.calls[0].dry_run);
        assert_eq!(mutation.side_effects, 0);
    }

    let model = ResolvedDeclaration {
        plan: declaration(),
    };
    let _analysis = SafeDeleteAnalysis {
        declaration: model.clone(),
        references: Vec::new(),
        live_references: Vec::new(),
        cleanup_edits: Vec::new(),
        coverage: CleanupCoverage::Complete,
    };
    let _plan = SafeDeletePlan {
        declaration: model,
        references: Vec::new(),
        edits: Vec::new(),
        affected_files: vec![RelativePath::new(DECLARATION_PATH)],
        dry_run: true,
    };
}

#[test]
fn invalid_reference_keeps_every_later_resolved_live_reference_in_the_report() {
    let z_content = "gone();\n";
    let valid = reference("src/z.rs", (0, 0), (0, 4), z_content);
    for invalid in [
        reference(DECLARATION_PATH, (0, 3), (1, 4), DECLARATION_CONTENT),
        reference(DECLARATION_PATH, (99, 0), (99, 4), DECLARATION_CONTENT),
    ] {
        for dry_run in [false, true] {
            let mut mutation = RecordingMutation::default();
            let report = run(
                &mut mutation,
                dry_run,
                SafeDeleteDeclarationOutcome::Planned {
                    plan: declaration(),
                },
                complete(vec![valid.clone(), invalid.clone()], Vec::new()),
                vec![
                    snapshot(DECLARATION_PATH, DECLARATION_CONTENT),
                    snapshot("src/z.rs", z_content),
                ],
            );
            assert_eq!(
                report.blocking_reason,
                Some(SafeDeleteBlockingReason::InvalidCleanup)
            );
            assert_eq!(report.references, vec![invalid.clone(), valid.clone()]);
            assert!(mutation.calls.is_empty());
        }
    }
}
