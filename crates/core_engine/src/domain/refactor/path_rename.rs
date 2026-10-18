use std::collections::HashMap;

use crate::domain::{FileId, RelativePath};
use crate::ports::inbound::{DirectoryEntryKind, WorkspaceApplyEditsResult, WorkspaceEditSpan};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRenameRequest {
    pub source: RelativePath,
    pub destination: RelativePath,
    pub dry_run: bool,
    pub semantic_policy: PathRenameSemanticPolicy,
    pub expected_versions: HashMap<RelativePath, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMove {
    pub source: RelativePath,
    pub destination: RelativePath,
    pub kind: DirectoryEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRenameSemanticPolicy {
    None,
    BestEffort,
    Required,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRenameSemanticOutcome {
    Pending,
    NotRequested,
    Planned,
    Unsupported,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRenameStatus {
    Preview,
    Applied,
    Partial,
    Unsupported,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathRenameError {
    SourceNotFound {
        path: RelativePath,
    },
    DestinationExists {
        path: RelativePath,
    },
    IdenticalPaths {
        path: RelativePath,
    },
    DestinationInsideSource {
        source: RelativePath,
        destination: RelativePath,
    },
    DestinationExcluded {
        path: RelativePath,
        reason: String,
    },
    WorkspaceEscape {
        path: RelativePath,
    },
    DuplicateDestination {
        path: RelativePath,
    },
    InvalidExpectedVersion {
        path: RelativePath,
    },
    SemanticUnsupported,
    SemanticBackend,
    Filesystem,
    Persistence,
    VersionConflict {
        path: RelativePath,
        expected: String,
        actual: String,
    },
}

impl core::fmt::Display for PathRenameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            Self::SourceNotFound { .. } => "path rename source was not found",
            Self::DestinationExists { .. } => "path rename destination already exists",
            Self::IdenticalPaths { .. } => "path rename source and destination are identical",
            Self::DestinationInsideSource { .. } => "path rename destination is inside its source",
            Self::DestinationExcluded { .. } => "path rename destination is excluded",
            Self::WorkspaceEscape { .. } => "path rename escapes the workspace",
            Self::DuplicateDestination { .. } => "path rename contains a duplicate destination",
            Self::InvalidExpectedVersion { .. } => {
                "path rename contains an invalid expected version"
            }
            Self::SemanticUnsupported => "semantic path rename is unsupported",
            Self::SemanticBackend => "semantic path rename planning failed",
            Self::Filesystem => "path rename filesystem operation failed",
            Self::Persistence => "path rename persistence failed",
            Self::VersionConflict { .. } => "path rename version conflict",
        };
        f.write_str(message)
    }
}

impl core::error::Error for PathRenameError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRenamePlan {
    moves: Vec<PathMove>,
    edits: Vec<WorkspaceEditSpan>,
    semantic_policy: PathRenameSemanticPolicy,
    dry_run: bool,
    expected_versions: HashMap<RelativePath, String>,
    semantic_outcome: PathRenameSemanticOutcome,
    semantic_errors: Vec<PathRenameError>,
    moved_identities: Vec<(RelativePath, FileId)>,
}

impl PathRenamePlan {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        mut moves: Vec<PathMove>,
        edits: Vec<WorkspaceEditSpan>,
        semantic_policy: PathRenameSemanticPolicy,
        dry_run: bool,
        expected_versions: HashMap<RelativePath, String>,
        semantic_outcome: PathRenameSemanticOutcome,
        semantic_errors: Vec<PathRenameError>,
        moved_identities: Vec<(RelativePath, FileId)>,
    ) -> Result<Self, PathRenameError> {
        validate_moves(&mut moves)?;
        validate_semantic_state(semantic_policy, semantic_outcome, &edits, &semantic_errors)?;
        validate_expected_versions(&moves, &edits, semantic_outcome, &expected_versions)?;

        Ok(Self {
            moves,
            edits,
            semantic_policy,
            dry_run,
            expected_versions,
            semantic_outcome,
            semantic_errors,
            moved_identities,
        })
    }

    pub fn moves(&self) -> &[PathMove] {
        &self.moves
    }

    pub fn edits(&self) -> &[WorkspaceEditSpan] {
        &self.edits
    }

    pub fn semantic_policy(&self) -> PathRenameSemanticPolicy {
        self.semantic_policy
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn expected_versions(&self) -> &HashMap<RelativePath, String> {
        &self.expected_versions
    }

    pub fn semantic_outcome(&self) -> PathRenameSemanticOutcome {
        self.semantic_outcome
    }

    pub fn semantic_errors(&self) -> &[PathRenameError] {
        &self.semantic_errors
    }

    pub(crate) fn moved_identities(&self) -> &[(RelativePath, FileId)] {
        &self.moved_identities
    }
}

fn validate_moves(moves: &mut [PathMove]) -> Result<(), PathRenameError> {
    let mut destinations = std::collections::HashSet::with_capacity(moves.len());
    for path_move in moves.iter() {
        if path_move.source == path_move.destination {
            return Err(PathRenameError::IdenticalPaths {
                path: path_move.source.clone(),
            });
        }
        if path_move.kind == DirectoryEntryKind::Dir
            && is_descendant(&path_move.destination, &path_move.source)
        {
            return Err(PathRenameError::DestinationInsideSource {
                source: path_move.source.clone(),
                destination: path_move.destination.clone(),
            });
        }
        if !destinations.insert(path_move.destination.clone()) {
            return Err(PathRenameError::DuplicateDestination {
                path: path_move.destination.clone(),
            });
        }
    }

    if let Some(root_index) = moves
        .iter()
        .position(|path_move| path_move.kind == DirectoryEntryKind::Dir)
    {
        moves.swap(0, root_index);
        let root = &moves[0];
        let valid_descendants = moves[1..].iter().all(|path_move| {
            if path_move.kind != DirectoryEntryKind::File {
                return false;
            }

            let Some(source_suffix) = descendant_suffix(&path_move.source, &root.source) else {
                return false;
            };
            let Some(destination_suffix) =
                descendant_suffix(&path_move.destination, &root.destination)
            else {
                return false;
            };

            source_suffix == destination_suffix
        });
        if !valid_descendants {
            return Err(PathRenameError::SemanticBackend);
        }
        moves[1..].sort_by(|left, right| left.source.cmp(&right.source));
    }

    Ok(())
}

fn is_descendant(path: &RelativePath, parent: &RelativePath) -> bool {
    descendant_suffix(path, parent).is_some()
}

fn descendant_suffix<'a>(path: &'a RelativePath, parent: &RelativePath) -> Option<&'a str> {
    path.as_str()
        .strip_prefix(parent.as_str())
        .filter(|suffix| suffix.starts_with('/'))
}

fn validate_semantic_state(
    policy: PathRenameSemanticPolicy,
    outcome: PathRenameSemanticOutcome,
    edits: &[WorkspaceEditSpan],
    errors: &[PathRenameError],
) -> Result<(), PathRenameError> {
    let valid = match (policy, outcome) {
        (PathRenameSemanticPolicy::None, PathRenameSemanticOutcome::NotRequested) => {
            edits.is_empty() && errors.is_empty()
        }
        (
            PathRenameSemanticPolicy::BestEffort | PathRenameSemanticPolicy::Required,
            PathRenameSemanticOutcome::Pending,
        ) => edits.is_empty() && errors.is_empty(),
        (
            PathRenameSemanticPolicy::BestEffort | PathRenameSemanticPolicy::Required,
            PathRenameSemanticOutcome::Planned,
        ) => errors.is_empty(),
        (PathRenameSemanticPolicy::BestEffort, PathRenameSemanticOutcome::Unsupported) => {
            edits.is_empty() && errors == [PathRenameError::SemanticUnsupported]
        }
        (PathRenameSemanticPolicy::BestEffort, PathRenameSemanticOutcome::Failed) => {
            edits.is_empty()
                && !errors.is_empty()
                && errors.iter().all(|error| {
                    matches!(
                        error,
                        PathRenameError::SemanticBackend | PathRenameError::WorkspaceEscape { .. }
                    )
                })
        }
        _ => false,
    };

    if valid {
        Ok(())
    } else {
        Err(PathRenameError::SemanticBackend)
    }
}

fn validate_expected_versions(
    moves: &[PathMove],
    edits: &[WorkspaceEditSpan],
    semantic_outcome: PathRenameSemanticOutcome,
    expected_versions: &HashMap<RelativePath, String>,
) -> Result<(), PathRenameError> {
    for (path, version) in expected_versions {
        let valid_hash = version.len() == 64
            && version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let moved_file = moves.iter().any(|path_move| {
            path_move.kind == DirectoryEntryKind::File && path_move.source == *path
        });
        let known_non_file = moves.iter().any(|path_move| {
            (path_move.kind == DirectoryEntryKind::Dir && path_move.source == *path)
                || (path_move.destination == *path && !moved_file)
        });
        let semantic_target = edits.iter().any(|edit| edit.path == *path);
        let membership_deferred = semantic_outcome == PathRenameSemanticOutcome::Pending;

        if !valid_hash
            || known_non_file
            || (!moved_file && !semantic_target && !membership_deferred)
        {
            return Err(PathRenameError::InvalidExpectedVersion { path: path.clone() });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRenameResult {
    pub status: PathRenameStatus,
    pub plan: PathRenamePlan,
    pub edit_result: Option<WorkspaceApplyEditsResult>,
    pub errors: Vec<PathRenameError>,
}
