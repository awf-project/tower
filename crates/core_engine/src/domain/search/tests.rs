use super::*;
use crate::domain::FileId;

fn root(value: &str) -> DependencyRootId {
    DependencyRootId::new(value.to_owned()).expect("valid dependency root")
}

fn dependency_path(root_id: &str, path: &str) -> DependencyPath {
    DependencyPath::new(root(root_id), RelativePath::new(path)).expect("valid dependency path")
}

fn file(root_id: &str, path: &str, content: &str) -> IndexedDependencyFile {
    IndexedDependencyFile::new(
        dependency_path(root_id, path),
        content.len() as u64,
        content.to_owned(),
    )
}

fn eligible(root_id: &str, path: &str, content: &str) -> DependencyIndexCandidate {
    DependencyIndexCandidate::Eligible(file(root_id, path, content))
}

fn report(
    indexed_files: usize,
    indexed_bytes: u64,
    skipped_files: usize,
    limit_reasons: Vec<DependencyLimitReason>,
) -> DependencyIndexReport {
    DependencyIndexReport {
        indexed_files,
        indexed_bytes,
        skipped_files,
        truncated: !limit_reasons.is_empty(),
        limit_reasons,
        issues: Vec::new(),
    }
}

fn snapshot(files: Vec<IndexedDependencyFile>) -> DependencyIndexSnapshot {
    let indexed_bytes = files.iter().map(|entry| entry.size).sum();
    DependencyIndexSnapshot::new(files, report(0, indexed_bytes, 0, Vec::new()))
}

#[test]
fn dependency_index_limits_reject_zero_values() {
    assert_eq!(
        DependencyIndexLimits::new(0, 1, 1),
        Err(DependencyModelError::ZeroMaxFiles)
    );
    assert_eq!(
        DependencyIndexLimits::new(1, 0, 1),
        Err(DependencyModelError::ZeroMaxFileBytes)
    );
    assert_eq!(
        DependencyIndexLimits::new(1, 1, 0),
        Err(DependencyModelError::ZeroMaxTotalBytes)
    );

    let limits = DependencyIndexLimits::new(2, 3, 4).expect("positive limits");
    assert_eq!(limits.max_files.get(), 2);
    assert_eq!(limits.max_file_bytes.get(), 3);
    assert_eq!(limits.max_total_bytes.get(), 4);
}

#[test]
fn dependency_root_identity_rejects_invalid_labels() {
    assert_eq!(
        DependencyRootId::new(String::new()),
        Err(DependencyModelError::EmptyRootId)
    );

    for invalid in [".", "..", "a/b", "a\\b", "line\nbreak", "nul\0byte"] {
        assert_eq!(
            DependencyRootId::new(invalid.to_owned()),
            Err(DependencyModelError::InvalidRootId),
            "label {invalid:?} must be rejected"
        );
    }
}

#[test]
fn dependency_path_rejects_non_relative_or_ambiguous_components() {
    for invalid in [
        "",
        "/src/lib.rs",
        "C:/src/lib.rs",
        "src\\lib.rs",
        "./lib.rs",
        "src/./lib.rs",
        "../lib.rs",
        "src/../lib.rs",
        "src//lib.rs",
        "src/line\nbreak.rs",
    ] {
        assert_eq!(
            DependencyPath::new(root("library"), RelativePath::new(invalid)),
            Err(DependencyModelError::InvalidDependencyPath),
            "path {invalid:?} must be rejected"
        );
    }
}

#[test]
fn dependency_index_reports_file_count_truncation() {
    let limits = DependencyIndexLimits::new(2, 100, 100).expect("valid limits");
    let snapshot = DependencyIndexSnapshot::from_candidates(
        vec![
            eligible("library", "a.rs", "a"),
            DependencyIndexCandidate::Ignored,
            DependencyIndexCandidate::WorkspaceOwned,
            eligible("library", "b.rs", "b"),
            eligible("library", "c.rs", "c"),
        ],
        limits,
        vec![DependencyIndexIssue {
            root_id: root("unavailable"),
            path: None,
            reason: DependencyIndexIssueReason::RootUnavailable,
        }],
    );

    assert_eq!(snapshot.files().len(), limits.max_files.get());
    assert_eq!(
        snapshot
            .files()
            .iter()
            .map(|entry| entry.path.relative_path().as_str())
            .collect::<Vec<_>>(),
        vec!["a.rs", "b.rs"]
    );
    assert_eq!(snapshot.report().indexed_files, limits.max_files.get());
    assert_eq!(snapshot.report().indexed_bytes, 2);
    assert_eq!(snapshot.report().skipped_files, 1);
    assert!(snapshot.report().truncated);
    assert_eq!(
        snapshot.report().limit_reasons,
        vec![DependencyLimitReason::FileCount]
    );
    assert_eq!(snapshot.report().issues.len(), 1);
}

#[test]
fn dependency_index_excludes_files_over_size_limit() {
    let limits = DependencyIndexLimits::new(2, 4, 8).expect("valid limits");
    let snapshot = DependencyIndexSnapshot::from_candidates(
        vec![
            eligible("library", "exact.rs", "1234"),
            eligible("library", "oversize.rs", "12345"),
            eligible("library", "later.rs", "x"),
        ],
        limits,
        Vec::new(),
    );

    assert_eq!(snapshot.files()[0].size, limits.max_file_bytes.get());
    assert_eq!(snapshot.report().indexed_bytes, 5);
    assert_eq!(snapshot.report().skipped_files, 1);
    assert_eq!(snapshot.files().len(), 2);
    assert_eq!(
        snapshot.files()[1].path.relative_path().as_str(),
        "later.rs"
    );
    assert!(snapshot.report().truncated);
    assert_eq!(
        snapshot.report().limit_reasons,
        vec![DependencyLimitReason::FileSize]
    );
}

#[test]
fn dependency_index_reports_total_byte_truncation() {
    let limits = DependencyIndexLimits::new(3, 5, 5).expect("valid limits");
    let snapshot = DependencyIndexSnapshot::from_candidates(
        vec![
            eligible("library", "exact.rs", "12345"),
            eligible("library", "over-total.rs", "x"),
        ],
        limits,
        Vec::new(),
    );

    assert_eq!(
        snapshot.report().indexed_bytes,
        limits.max_total_bytes.get()
    );
    assert!(
        snapshot.files().iter().map(|entry| entry.size).sum::<u64>()
            <= limits.max_total_bytes.get()
    );
    assert_eq!(snapshot.report().skipped_files, 1);
    assert!(snapshot.report().truncated);
    assert_eq!(
        snapshot.report().limit_reasons,
        vec![DependencyLimitReason::TotalBytes]
    );
}

#[test]
fn dependency_results_follow_root_then_relative_path_order() {
    let snapshot = snapshot(vec![
        file("zeta", "b.rs", "needle"),
        file("alpha", "z.rs", "needle"),
        file("alpha", "a.rs", "needle"),
    ]);

    let filenames = compose_filename_results(
        vec![
            RelativePath::new("z.rs"),
            RelativePath::new("a.rs"),
            RelativePath::new("z.rs"),
        ],
        &snapshot,
        "rs",
        SearchScope::all(),
    );
    let filename_locators: Vec<_> = filenames
        .results
        .into_iter()
        .map(|result| result.locator)
        .collect();
    assert_eq!(
        filename_locators,
        vec![
            SearchResultLocator::Workspace(RelativePath::new("z.rs")),
            SearchResultLocator::Workspace(RelativePath::new("a.rs")),
            SearchResultLocator::Dependency(dependency_path("alpha", "a.rs")),
            SearchResultLocator::Dependency(dependency_path("alpha", "z.rs")),
            SearchResultLocator::Dependency(dependency_path("zeta", "b.rs")),
        ]
    );

    let text = compose_text_results(
        vec![
            Match {
                path: RelativePath::new("z.rs"),
                line_number: 9,
                line_content: "needle z".to_owned(),
                file_id: FileId::new_for_testing(0, 0),
            },
            Match {
                path: RelativePath::new("a.rs"),
                line_number: 2,
                line_content: "needle a".to_owned(),
                file_id: FileId::new_for_testing(1, 0),
            },
        ],
        &snapshot,
        "needle",
        SearchScope::all(),
    );
    let text_locators: Vec<_> = text
        .matches
        .into_iter()
        .map(|result| result.locator)
        .collect();
    assert_eq!(
        text_locators,
        vec![
            SearchResultLocator::Workspace(RelativePath::new("z.rs")),
            SearchResultLocator::Workspace(RelativePath::new("a.rs")),
            SearchResultLocator::Dependency(dependency_path("alpha", "a.rs")),
            SearchResultLocator::Dependency(dependency_path("alpha", "z.rs")),
            SearchResultLocator::Dependency(dependency_path("zeta", "b.rs")),
        ]
    );
}

#[test]
fn dependency_results_suppress_duplicate_root_paths() {
    let duplicate = file("library", "src/lib.rs", "needle\nneedle\nneedle");
    let snapshot = snapshot(vec![duplicate.clone(), duplicate]);

    let filenames = compose_filename_results(
        Vec::new(),
        &snapshot,
        "lib",
        SearchScope::dependencies_only(),
    );
    assert_eq!(filenames.results.len(), 1);

    let text = compose_text_results(
        Vec::new(),
        &snapshot,
        "needle",
        SearchScope::dependencies_only(),
    );
    assert_eq!(text.matches.len(), 3);
    assert_eq!(
        text.matches
            .iter()
            .map(|entry| entry.line_number)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn composition_keeps_same_suffix_workspace_and_dependency_files_distinct() {
    let path = RelativePath::new("src/lib.rs");
    let snapshot = snapshot(vec![file("library", path.as_str(), "needle")]);

    let results =
        compose_filename_results(vec![path.clone()], &snapshot, "lib", SearchScope::all());
    assert_eq!(results.results.len(), 2);
    assert_eq!(
        results.results[0].locator,
        SearchResultLocator::Workspace(path)
    );
    assert_eq!(
        results.results[1].locator,
        SearchResultLocator::Dependency(dependency_path("library", "src/lib.rs"))
    );
}

#[test]
fn filename_composition_preserves_empty_pattern_behavior() {
    let snapshot = snapshot(vec![file("library", "src/lib.rs", "contents")]);
    let result = compose_filename_results(
        vec![RelativePath::new("src/main.rs")],
        &snapshot,
        "",
        SearchScope::all(),
    );

    assert!(result.results.is_empty());
    assert_eq!(result.dependency_report, snapshot.report().clone());
}

#[test]
fn text_composition_preserves_empty_pattern_behavior() {
    let snapshot = snapshot(vec![file("library", "src/lib.rs", "first\nsecond\n")]);
    let workspace_match = Match {
        path: RelativePath::new("src/main.rs"),
        line_number: 1,
        line_content: "workspace".to_owned(),
        file_id: FileId::new_for_testing(0, 0),
    };
    let result = compose_text_results(vec![workspace_match], &snapshot, "", SearchScope::all());

    assert_eq!(result.matches.len(), 3);
    assert_eq!(result.matches[0].origin, SearchResultOrigin::Workspace);
    assert_eq!(result.matches[1].line_number, 1);
    assert_eq!(result.matches[1].line_content, "first");
    assert_eq!(result.matches[2].line_number, 2);
    assert_eq!(result.matches[2].line_content, "second");
}

#[test]
fn dependency_issues_expose_only_root_relative_context() {
    let issue = DependencyIndexIssue {
        root_id: root("library"),
        path: Some(RelativePath::new("src/lib.rs")),
        reason: DependencyIndexIssueReason::EntryUnreadable,
    };
    let snapshot = DependencyIndexSnapshot::new(
        Vec::new(),
        DependencyIndexReport {
            indexed_files: 0,
            indexed_bytes: 0,
            skipped_files: 1,
            truncated: false,
            limit_reasons: Vec::new(),
            issues: vec![issue.clone()],
        },
    );

    assert_eq!(snapshot.report().issues, vec![issue]);
    let diagnostic = format!("{:?}", snapshot.report().issues[0]);
    assert!(diagnostic.contains("library"));
    assert!(diagnostic.contains("src/lib.rs"));
    assert!(!diagnostic.contains("/home/"));
    assert!(!diagnostic.contains("os error"));
}

#[test]
fn dependency_snapshot_replacement_preserves_immutable_prior_generation() {
    let old = snapshot(vec![file("library", "old.rs", "old")]);
    let new = snapshot(vec![file("library", "new.rs", "new")]);

    assert_eq!(old.files()[0].path.relative_path().as_str(), "old.rs");
    assert_eq!(old.files()[0].content, "old");
    assert_eq!(new.files()[0].path.relative_path().as_str(), "new.rs");
    assert_eq!(new.files()[0].content, "new");
}

#[test]
fn search_scope_and_origin_preserve_dependency_identity() {
    assert_eq!(
        SearchScope::workspace_only(),
        SearchScope {
            workspace: true,
            dependencies: false,
        }
    );
    assert_eq!(
        SearchScope::dependencies_only(),
        SearchScope {
            workspace: false,
            dependencies: true,
        }
    );
    assert_eq!(
        SearchScope::all(),
        SearchScope {
            workspace: true,
            dependencies: true,
        }
    );
    assert_eq!(
        SearchScope::none(),
        SearchScope {
            workspace: false,
            dependencies: false,
        }
    );

    let path = dependency_path("library", "src/lib.rs");
    assert_eq!(path.root_id(), &root("library"));
    assert_eq!(path.relative_path(), &RelativePath::new("src/lib.rs"));
    assert_eq!(
        SearchResultOrigin::Dependency {
            root_id: path.root_id().clone(),
        },
        SearchResultOrigin::Dependency {
            root_id: root("library"),
        }
    );

    let snapshot = snapshot(vec![file("library", "src/lib.rs", "needle")]);
    let workspace_file = RelativePath::new("src/main.rs");
    let workspace_match = Match {
        path: workspace_file.clone(),
        line_number: 1,
        line_content: "needle".to_owned(),
        file_id: FileId::new_for_testing(0, 0),
    };

    for (scope, expected_files, expected_text) in [
        (SearchScope::workspace_only(), 1, 1),
        (SearchScope::dependencies_only(), 1, 1),
        (SearchScope::all(), 2, 2),
        (SearchScope::none(), 0, 0),
    ] {
        let filenames =
            compose_filename_results(vec![workspace_file.clone()], &snapshot, "src", scope);
        let text = compose_text_results(vec![workspace_match.clone()], &snapshot, "needle", scope);

        assert_eq!(filenames.results.len(), expected_files);
        assert_eq!(text.matches.len(), expected_text);
        assert_eq!(filenames.dependency_report, snapshot.report().clone());
        assert_eq!(text.dependency_report, snapshot.report().clone());
    }
}
