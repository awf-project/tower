use std::collections::HashMap;

use crate::domain::{FileId, RelativePath};
use crate::ports::inbound::{DirectoryEntryKind, WorkspaceEditSpan};

use super::{
    PathMove, PathRenameError, PathRenamePlan, PathRenameRequest, PathRenameResult,
    PathRenameSemanticOutcome, PathRenameSemanticPolicy, PathRenameStatus,
};

fn path(value: &str) -> RelativePath {
    RelativePath::new(value)
}

fn file_move(source: &str, destination: &str) -> PathMove {
    PathMove {
        source: path(source),
        destination: path(destination),
        kind: DirectoryEntryKind::File,
    }
}

fn directory_move(source: &str, destination: &str) -> PathMove {
    PathMove {
        source: path(source),
        destination: path(destination),
        kind: DirectoryEntryKind::Dir,
    }
}

#[test]
fn public_path_rename_types_retain_the_exact_immutable_plan_contract() {
    let source = path("src/old.rs");
    let destination = path("src/new.rs");
    let mut expected_versions = HashMap::new();
    expected_versions.insert(source.clone(), "a".repeat(64));
    let request = PathRenameRequest {
        source: source.clone(),
        destination: destination.clone(),
        dry_run: true,
        semantic_policy: PathRenameSemanticPolicy::BestEffort,
        expected_versions: expected_versions.clone(),
    };
    let moves = vec![file_move("src/old.rs", "src/new.rs")];
    let edits = vec![WorkspaceEditSpan {
        path: path("src/consumer.rs"),
        start_byte: 4,
        end_byte: 7,
        replacement: "new".to_owned(),
        base_hash: Some("b".repeat(64)),
    }];
    let identity = FileId::new_for_testing(3, 2);

    let plan = PathRenamePlan::new(
        moves.clone(),
        edits.clone(),
        request.semantic_policy,
        request.dry_run,
        request.expected_versions.clone(),
        PathRenameSemanticOutcome::Planned,
        Vec::new(),
        vec![(source, identity)],
    )
    .expect("a complete path-rename plan should be valid");
    let result = PathRenameResult {
        status: PathRenameStatus::Preview,
        plan: plan.clone(),
        edit_result: None,
        errors: Vec::new(),
    };

    assert_eq!(plan.moves(), moves);
    assert_eq!(plan.edits(), edits);
    assert_eq!(plan.semantic_policy(), request.semantic_policy);
    assert!(plan.dry_run());
    assert_eq!(plan.expected_versions(), &expected_versions);
    assert_eq!(plan.semantic_outcome(), PathRenameSemanticOutcome::Planned);
    assert!(plan.semantic_errors().is_empty());
    assert_eq!(plan.moved_identities(), &[(path("src/old.rs"), identity)]);
    assert_eq!(result.status, PathRenameStatus::Preview);
}

#[test]
fn path_rename_result_retains_every_execution_status() {
    let plan = PathRenamePlan::new(
        vec![file_move("old.rs", "new.rs")],
        Vec::new(),
        PathRenameSemanticPolicy::None,
        false,
        HashMap::new(),
        PathRenameSemanticOutcome::NotRequested,
        Vec::new(),
        vec![(path("old.rs"), FileId::new_for_testing(0, 0))],
    )
    .expect("a valid plan is required to verify each result status");

    for status in [
        PathRenameStatus::Preview,
        PathRenameStatus::Applied,
        PathRenameStatus::Partial,
        PathRenameStatus::Unsupported,
        PathRenameStatus::Rejected,
    ] {
        let result = PathRenameResult {
            status,
            plan: plan.clone(),
            edit_result: None,
            errors: Vec::new(),
        };

        assert_eq!(result.status, status, "result must retain {status:?}");
    }
}

#[test]
fn path_rename_error_contract_exposes_every_named_failure() {
    let errors = [
        (
            PathRenameError::SourceNotFound {
                path: path("missing.rs"),
            },
            "path rename source was not found",
        ),
        (
            PathRenameError::DestinationExists {
                path: path("existing.rs"),
            },
            "path rename destination already exists",
        ),
        (
            PathRenameError::IdenticalPaths {
                path: path("same.rs"),
            },
            "path rename source and destination are identical",
        ),
        (
            PathRenameError::DestinationInsideSource {
                source: path("old"),
                destination: path("old/nested"),
            },
            "path rename destination is inside its source",
        ),
        (
            PathRenameError::DestinationExcluded {
                path: path("ignored.rs"),
                reason: "matched .towerignore".to_owned(),
            },
            "path rename destination is excluded",
        ),
        (
            PathRenameError::WorkspaceEscape {
                path: path("../outside.rs"),
            },
            "path rename escapes the workspace",
        ),
        (
            PathRenameError::DuplicateDestination {
                path: path("new/same.rs"),
            },
            "path rename contains a duplicate destination",
        ),
        (
            PathRenameError::InvalidExpectedVersion {
                path: path("guarded.rs"),
            },
            "path rename contains an invalid expected version",
        ),
        (
            PathRenameError::SemanticUnsupported,
            "semantic path rename is unsupported",
        ),
        (
            PathRenameError::SemanticBackend,
            "semantic path rename planning failed",
        ),
        (
            PathRenameError::Filesystem,
            "path rename filesystem operation failed",
        ),
        (
            PathRenameError::Persistence,
            "path rename persistence failed",
        ),
        (
            PathRenameError::VersionConflict {
                path: path("stale.rs"),
                expected: "a".repeat(64),
                actual: "b".repeat(64),
            },
            "path rename version conflict",
        ),
    ];

    for (error, message) in errors {
        assert_eq!(
            error.to_string(),
            message,
            "exact error message for {error:?}"
        );
    }
}

#[test]
fn file_plan_and_semantic_edits_preserve_planner_order() {
    let moves = vec![
        file_move("src/z.rs", "renamed/z.rs"),
        file_move("src/a.rs", "renamed/a.rs"),
        file_move("src/m.rs", "renamed/m.rs"),
    ];
    let edits = vec![
        WorkspaceEditSpan {
            path: path("src/consumer_b.rs"),
            start_byte: 8,
            end_byte: 11,
            replacement: "new_b".to_owned(),
            base_hash: Some("b".repeat(64)),
        },
        WorkspaceEditSpan {
            path: path("src/consumer_a.rs"),
            start_byte: 2,
            end_byte: 5,
            replacement: "new_a".to_owned(),
            base_hash: Some("a".repeat(64)),
        },
    ];

    let plan = PathRenamePlan::new(
        moves.clone(),
        edits.clone(),
        PathRenameSemanticPolicy::BestEffort,
        false,
        HashMap::new(),
        PathRenameSemanticOutcome::Planned,
        Vec::new(),
        vec![
            (path("src/z.rs"), FileId::new_for_testing(2, 0)),
            (path("src/a.rs"), FileId::new_for_testing(4, 0)),
            (path("src/m.rs"), FileId::new_for_testing(6, 0)),
        ],
    )
    .expect("a multi-file planner result with semantic edits should be valid");

    assert_eq!(
        plan.moves(),
        moves,
        "file move order must remain planner order"
    );
    assert_eq!(
        plan.edits(),
        edits,
        "semantic edit order must remain planner order"
    );
}

#[test]
fn semantic_policy_retains_each_supported_policy_without_weakening() {
    let cases = [
        (
            PathRenameSemanticPolicy::None,
            PathRenameSemanticOutcome::NotRequested,
        ),
        (
            PathRenameSemanticPolicy::BestEffort,
            PathRenameSemanticOutcome::Pending,
        ),
        (
            PathRenameSemanticPolicy::Required,
            PathRenameSemanticOutcome::Pending,
        ),
    ];

    for (policy, outcome) in cases {
        let plan = PathRenamePlan::new(
            vec![file_move("old.rs", "new.rs")],
            Vec::new(),
            policy,
            false,
            HashMap::new(),
            outcome,
            Vec::new(),
            vec![(path("old.rs"), FileId::new_for_testing(0, 0))],
        )
        .expect("each exact semantic policy should retain its valid readiness state");

        assert_eq!(plan.semantic_policy(), policy);
        assert_eq!(plan.semantic_outcome(), outcome);
    }

    let invalid_readiness = PathRenamePlan::new(
        vec![file_move("old.rs", "new.rs")],
        Vec::new(),
        PathRenameSemanticPolicy::None,
        false,
        HashMap::new(),
        PathRenameSemanticOutcome::Pending,
        Vec::new(),
        vec![(path("old.rs"), FileId::new_for_testing(0, 0))],
    );
    assert_eq!(invalid_readiness, Err(PathRenameError::SemanticBackend));

    // Unknown policy strings are rejected at the public wire parsing boundary
    // introduced by T008. The T001 domain boundary accepts the closed Rust enum,
    // so no invalid string can be constructed here without testing a fake parser.
}

#[test]
fn path_rename_plan_rejects_the_validation_failures_owned_by_t001() {
    let base = || {
        (
            Vec::new(),
            Vec::new(),
            PathRenameSemanticPolicy::None,
            false,
            HashMap::new(),
            PathRenameSemanticOutcome::NotRequested,
            Vec::new(),
            Vec::new(),
        )
    };

    let (mut moves, edits, policy, dry_run, versions, outcome, errors, identities) = base();
    moves.push(file_move("same.rs", "same.rs"));
    assert_eq!(
        PathRenamePlan::new(
            moves, edits, policy, dry_run, versions, outcome, errors, identities,
        ),
        Err(PathRenameError::IdenticalPaths {
            path: path("same.rs")
        })
    );

    let (mut moves, edits, policy, dry_run, versions, outcome, errors, identities) = base();
    moves.push(PathMove {
        source: path("old"),
        destination: path("old/nested"),
        kind: DirectoryEntryKind::Dir,
    });
    assert_eq!(
        PathRenamePlan::new(
            moves, edits, policy, dry_run, versions, outcome, errors, identities,
        ),
        Err(PathRenameError::DestinationInsideSource {
            source: path("old"),
            destination: path("old/nested"),
        })
    );

    let (mut moves, edits, policy, dry_run, versions, outcome, errors, identities) = base();
    moves.extend([
        file_move("old/a.rs", "new/same.rs"),
        file_move("old/b.rs", "new/same.rs"),
    ]);
    assert_eq!(
        PathRenamePlan::new(
            moves, edits, policy, dry_run, versions, outcome, errors, identities,
        ),
        Err(PathRenameError::DuplicateDestination {
            path: path("new/same.rs")
        })
    );
}

#[test]
fn path_rename_errors_retain_sanitized_infrastructure_variants() {
    let secret = "/private/workspace/backend-token";
    let errors = [
        PathRenameError::SemanticBackend,
        PathRenameError::WorkspaceEscape {
            path: path("external.rs"),
        },
    ];

    let plan = PathRenamePlan::new(
        vec![file_move("old", "new")],
        Vec::new(),
        PathRenameSemanticPolicy::BestEffort,
        false,
        HashMap::new(),
        PathRenameSemanticOutcome::Failed,
        errors.to_vec(),
        vec![(path("old"), FileId::new_for_testing(1, 0))],
    )
    .expect("best-effort plans should retain structured sanitized warnings");

    assert_eq!(plan.semantic_errors(), errors);
    assert!(!PathRenameError::Filesystem.to_string().contains(secret));
    assert!(
        !PathRenameError::SemanticBackend
            .to_string()
            .contains(secret)
    );
}

#[test]
fn best_effort_failed_plan_rejects_non_semantic_errors() {
    let invalid_errors = [
        PathRenameError::Filesystem,
        PathRenameError::Persistence,
        PathRenameError::VersionConflict {
            path: path("stale.rs"),
            expected: "a".repeat(64),
            actual: "b".repeat(64),
        },
        PathRenameError::InvalidExpectedVersion {
            path: path("guarded.rs"),
        },
        PathRenameError::SemanticUnsupported,
    ];

    for error in invalid_errors {
        let result = PathRenamePlan::new(
            vec![file_move("old.rs", "new.rs")],
            Vec::new(),
            PathRenameSemanticPolicy::BestEffort,
            false,
            HashMap::new(),
            PathRenameSemanticOutcome::Failed,
            vec![error],
            vec![(path("old.rs"), FileId::new_for_testing(1, 0))],
        );

        assert_eq!(result, Err(PathRenameError::SemanticBackend));
    }
}

#[test]
fn directory_plan_keeps_root_then_descendants_in_lexical_source_order() {
    let moves = vec![
        file_move("old/nested/b.rs", "new/nested/b.rs"),
        PathMove {
            source: path("old"),
            destination: path("new"),
            kind: DirectoryEntryKind::Dir,
        },
        file_move("old/a.rs", "new/a.rs"),
    ];
    let identities = vec![
        (path("old/a.rs"), FileId::new_for_testing(4, 1)),
        (path("old/nested/b.rs"), FileId::new_for_testing(8, 3)),
    ];

    let plan = PathRenamePlan::new(
        moves.clone(),
        Vec::new(),
        PathRenameSemanticPolicy::None,
        false,
        HashMap::new(),
        PathRenameSemanticOutcome::NotRequested,
        Vec::new(),
        identities.clone(),
    )
    .expect("the directory planner should normalize root and descendants");

    assert_eq!(
        plan.moves(),
        [
            PathMove {
                source: path("old"),
                destination: path("new"),
                kind: DirectoryEntryKind::Dir,
            },
            file_move("old/a.rs", "new/a.rs"),
            file_move("old/nested/b.rs", "new/nested/b.rs"),
        ]
    );
    assert_eq!(plan.moved_identities(), identities);

    let empty_directory = PathRenamePlan::new(
        vec![PathMove {
            source: path("empty"),
            destination: path("renamed"),
            kind: DirectoryEntryKind::Dir,
        }],
        Vec::new(),
        PathRenameSemanticPolicy::None,
        false,
        HashMap::new(),
        PathRenameSemanticOutcome::NotRequested,
        Vec::new(),
        Vec::new(),
    )
    .expect("an empty directory should need only its root summary");
    assert_eq!(empty_directory.moves().len(), 1);
    assert!(empty_directory.moved_identities().is_empty());
}

#[test]
fn directory_plan_rejects_malformed_descendant_topology() {
    let malformed_move_sets = [
        vec![
            directory_move("old", "new"),
            directory_move("old/nested", "new/nested"),
        ],
        vec![
            directory_move("old", "new"),
            file_move("unrelated.rs", "new/unrelated.rs"),
        ],
        vec![
            directory_move("old", "new"),
            file_move("old/file.rs", "new/different.rs"),
        ],
    ];

    for moves in malformed_move_sets {
        let result = PathRenamePlan::new(
            moves,
            Vec::new(),
            PathRenameSemanticPolicy::None,
            false,
            HashMap::new(),
            PathRenameSemanticOutcome::NotRequested,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(result, Err(PathRenameError::SemanticBackend));
    }
}

#[test]
fn duplicate_destination_is_rejected_before_a_plan_is_created() {
    let result = PathRenamePlan::new(
        vec![
            file_move("old/a.rs", "new/same.rs"),
            file_move("old/b.rs", "new/same.rs"),
        ],
        Vec::new(),
        PathRenameSemanticPolicy::None,
        false,
        HashMap::new(),
        PathRenameSemanticOutcome::NotRequested,
        Vec::new(),
        vec![
            (path("old/a.rs"), FileId::new_for_testing(1, 0)),
            (path("old/b.rs"), FileId::new_for_testing(2, 0)),
        ],
    );

    assert_eq!(
        result,
        Err(PathRenameError::DuplicateDestination {
            path: path("new/same.rs")
        })
    );
}

#[test]
fn pending_plan_rejects_known_non_file_cas_keys_before_semantic_finalization() {
    for invalid_path in [path("old"), path("new")] {
        let result = PathRenamePlan::new(
            vec![
                directory_move("old", "new"),
                file_move("old/file.rs", "new/file.rs"),
            ],
            Vec::new(),
            PathRenameSemanticPolicy::BestEffort,
            false,
            HashMap::from([(invalid_path.clone(), "a".repeat(64))]),
            PathRenameSemanticOutcome::Pending,
            Vec::new(),
            vec![(path("old/file.rs"), FileId::new_for_testing(1, 0))],
        );

        assert_eq!(
            result,
            Err(PathRenameError::InvalidExpectedVersion { path: invalid_path })
        );
    }
}

#[test]
fn completed_plan_rejects_expected_version_for_an_unrelated_path() {
    let unrelated = path("unrelated.rs");
    let result = PathRenamePlan::new(
        vec![file_move("old.rs", "new.rs")],
        Vec::new(),
        PathRenameSemanticPolicy::BestEffort,
        false,
        HashMap::from([(unrelated.clone(), "a".repeat(64))]),
        PathRenameSemanticOutcome::Planned,
        Vec::new(),
        vec![(path("old.rs"), FileId::new_for_testing(1, 0))],
    );

    assert_eq!(
        result,
        Err(PathRenameError::InvalidExpectedVersion { path: unrelated })
    );
}
