use std::collections::BTreeSet;
use std::num::{NonZeroU64, NonZeroUsize};

use crate::domain::RelativePath;
use crate::domain::token::tokenize;
use crate::ports::inbound::Match;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DependencyRootId(String);

impl DependencyRootId {
    pub fn new(value: String) -> Result<Self, DependencyModelError> {
        if value.is_empty() {
            return Err(DependencyModelError::EmptyRootId);
        }
        if matches!(value.as_str(), "." | "..")
            || value.contains(['/', '\\'])
            || value.chars().any(char::is_control)
        {
            return Err(DependencyModelError::InvalidRootId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DependencyPath {
    root_id: DependencyRootId,
    relative_path: RelativePath,
}

impl DependencyPath {
    pub fn new(
        root_id: DependencyRootId,
        relative_path: RelativePath,
    ) -> Result<Self, DependencyModelError> {
        let path = relative_path.as_str();
        let has_drive_prefix = path.as_bytes().get(1) == Some(&b':');
        if path.is_empty()
            || path.starts_with('/')
            || has_drive_prefix
            || path.contains('\\')
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        {
            return Err(DependencyModelError::InvalidDependencyPath);
        }
        Ok(Self {
            root_id,
            relative_path,
        })
    }

    pub fn root_id(&self) -> &DependencyRootId {
        &self.root_id
    }

    pub fn relative_path(&self) -> &RelativePath {
        &self.relative_path
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DependencyModelError {
    EmptyRootId,
    InvalidRootId,
    InvalidDependencyPath,
    ZeroMaxFiles,
    ZeroMaxFileBytes,
    ZeroMaxTotalBytes,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DependencyIndexLimits {
    max_files: NonZeroUsize,
    max_file_bytes: NonZeroU64,
    max_total_bytes: NonZeroU64,
}

impl DependencyIndexLimits {
    pub fn new(
        max_files: usize,
        max_file_bytes: u64,
        max_total_bytes: u64,
    ) -> Result<Self, DependencyModelError> {
        let max_files = NonZeroUsize::new(max_files).ok_or(DependencyModelError::ZeroMaxFiles)?;
        let max_file_bytes =
            NonZeroU64::new(max_file_bytes).ok_or(DependencyModelError::ZeroMaxFileBytes)?;
        let max_total_bytes =
            NonZeroU64::new(max_total_bytes).ok_or(DependencyModelError::ZeroMaxTotalBytes)?;
        Ok(Self {
            max_files,
            max_file_bytes,
            max_total_bytes,
        })
    }

    pub fn max_files(&self) -> NonZeroUsize {
        self.max_files
    }

    pub fn max_file_bytes(&self) -> NonZeroU64 {
        self.max_file_bytes
    }

    pub fn max_total_bytes(&self) -> NonZeroU64 {
        self.max_total_bytes
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SearchScope {
    pub workspace: bool,
    pub dependencies: bool,
}

impl SearchScope {
    pub fn workspace_only() -> Self {
        Self {
            workspace: true,
            dependencies: false,
        }
    }

    pub fn dependencies_only() -> Self {
        Self {
            workspace: false,
            dependencies: true,
        }
    }

    pub fn all() -> Self {
        Self {
            workspace: true,
            dependencies: true,
        }
    }

    pub fn none() -> Self {
        Self {
            workspace: false,
            dependencies: false,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SearchResultOrigin {
    Workspace,
    Dependency { root_id: DependencyRootId },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexedDependencyFile {
    path: DependencyPath,
    size: u64,
    content: String,
}

impl IndexedDependencyFile {
    pub fn new(path: DependencyPath, size: u64, content: String) -> Self {
        Self {
            path,
            size,
            content,
        }
    }

    pub fn path(&self) -> &DependencyPath {
        &self.path
    }
}

/// A candidate considered by the pure dependency-index accounting boundary.
///
/// Adapters classify ignored and workspace-owned files before accounting. They
/// remain explicit here so callers can preserve the invariant that those
/// candidates do not consume either resource budget.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DependencyIndexCandidate {
    Eligible(IndexedDependencyFile),
    Ignored,
    WorkspaceOwned,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DependencyLimitReason {
    FileCount,
    FileSize,
    TotalBytes,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DependencyIndexIssueReason {
    RootUnavailable,
    RootNotDirectory,
    EntryUnreadable,
    ChangedDuringRead,
    InvalidEncoding,
    BoundaryViolation,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DependencyIndexIssue {
    pub root_id: DependencyRootId,
    pub path: Option<RelativePath>,
    pub reason: DependencyIndexIssueReason,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DependencyIndexReport {
    pub indexed_files: usize,
    pub indexed_bytes: u64,
    pub skipped_files: usize,
    pub truncated: bool,
    pub limit_reasons: Vec<DependencyLimitReason>,
    pub issues: Vec<DependencyIndexIssue>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DependencyIndexSnapshot {
    files: Vec<IndexedDependencyFile>,
    report: DependencyIndexReport,
}

impl DependencyIndexSnapshot {
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            report: DependencyIndexReport {
                indexed_files: 0,
                indexed_bytes: 0,
                skipped_files: 0,
                truncated: false,
                limit_reasons: Vec::new(),
                issues: Vec::new(),
            },
        }
    }

    pub fn new(files: Vec<IndexedDependencyFile>, report: DependencyIndexReport) -> Self {
        Self { files, report }
    }

    pub fn files(&self) -> &[IndexedDependencyFile] {
        &self.files
    }

    pub fn report(&self) -> &DependencyIndexReport {
        &self.report
    }

    /// Apply aggregate limits to already-classified candidates.
    ///
    /// This pure boundary owns retained-file and report accounting. Filesystem
    /// traversal, ownership detection, and issue collection remain adapter
    /// responsibilities.
    pub fn from_candidates(
        candidates: Vec<DependencyIndexCandidate>,
        limits: DependencyIndexLimits,
        mut issues: Vec<DependencyIndexIssue>,
    ) -> Self {
        let mut eligible = candidates
            .into_iter()
            .filter_map(|candidate| match candidate {
                DependencyIndexCandidate::Eligible(file) => Some(file),
                DependencyIndexCandidate::Ignored | DependencyIndexCandidate::WorkspaceOwned => {
                    None
                }
            })
            .collect::<Vec<_>>();
        eligible.sort_by(|left, right| left.path.cmp(&right.path));
        eligible.dedup_by(|left, right| left.path == right.path);

        let mut files = Vec::new();
        let mut indexed_bytes = 0_u64;
        issues.sort_by(|left, right| {
            (&left.root_id, &left.path, left.reason).cmp(&(
                &right.root_id,
                &right.path,
                right.reason,
            ))
        });
        let mut skipped_files = issues.iter().filter(|issue| issue.path.is_some()).count();
        let mut reasons = BTreeSet::new();
        let mut total_bytes_exhausted = false;
        for file in eligible {
            if file.size > limits.max_file_bytes.get() {
                skipped_files += 1;
                reasons.insert(DependencyLimitReason::FileSize);
                continue;
            }
            if files.len() == limits.max_files.get() {
                skipped_files += 1;
                reasons.insert(DependencyLimitReason::FileCount);
                continue;
            }
            if total_bytes_exhausted {
                skipped_files += 1;
                reasons.insert(DependencyLimitReason::TotalBytes);
                continue;
            }
            if file.size > limits.max_total_bytes.get() - indexed_bytes {
                skipped_files += 1;
                reasons.insert(DependencyLimitReason::TotalBytes);
                total_bytes_exhausted = true;
                continue;
            }
            indexed_bytes += file.size;
            files.push(file);
        }

        let limit_reasons = reasons.into_iter().collect::<Vec<_>>();
        let truncated = !limit_reasons.is_empty();
        let report = DependencyIndexReport {
            indexed_files: files.len(),
            indexed_bytes,
            skipped_files,
            truncated,
            limit_reasons,
            issues,
        };
        Self { files, report }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SearchResultLocator {
    Workspace(RelativePath),
    Dependency(DependencyPath),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScopedFileResult {
    pub locator: SearchResultLocator,
    pub path: RelativePath,
    pub origin: SearchResultOrigin,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScopedTextMatch {
    pub locator: SearchResultLocator,
    pub path: RelativePath,
    pub line_number: u32,
    pub line_content: String,
    pub origin: SearchResultOrigin,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScopedFileSearchResult {
    pub results: Vec<ScopedFileResult>,
    pub dependency_report: DependencyIndexReport,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScopedTextSearchResult {
    pub matches: Vec<ScopedTextMatch>,
    pub dependency_report: DependencyIndexReport,
}

pub fn compose_filename_results(
    workspace_hits: Vec<RelativePath>,
    snapshot: &DependencyIndexSnapshot,
    query: &str,
    scope: SearchScope,
) -> ScopedFileSearchResult {
    let mut results = Vec::new();
    let mut seen = BTreeSet::new();
    if !query.trim().is_empty() {
        if scope.workspace {
            for path in workspace_hits {
                let locator = SearchResultLocator::Workspace(path.clone());
                if seen.insert(locator.clone()) {
                    results.push(ScopedFileResult {
                        locator,
                        path,
                        origin: SearchResultOrigin::Workspace,
                    });
                }
            }
        }
        if scope.dependencies {
            let query_tokens = tokenize(query);
            let mut files = snapshot.files.iter().collect::<Vec<_>>();
            files.sort_by(|left, right| left.path.cmp(&right.path));
            for file in files {
                let file_tokens = tokenize(file.path.relative_path().as_str());
                if query_tokens.iter().any(|token| file_tokens.contains(token)) {
                    let locator = SearchResultLocator::Dependency(file.path.clone());
                    if seen.insert(locator.clone()) {
                        results.push(ScopedFileResult {
                            locator,
                            path: file.path.relative_path().clone(),
                            origin: SearchResultOrigin::Dependency {
                                root_id: file.path.root_id().clone(),
                            },
                        });
                    }
                }
            }
        }
    }
    ScopedFileSearchResult {
        results,
        dependency_report: snapshot.report.clone(),
    }
}

pub fn compose_text_results(
    workspace_hits: Vec<Match>,
    snapshot: &DependencyIndexSnapshot,
    pattern: &str,
    scope: SearchScope,
) -> ScopedTextSearchResult {
    let mut matches = Vec::new();
    let mut seen = BTreeSet::new();
    if scope.workspace {
        for hit in workspace_hits {
            let locator = SearchResultLocator::Workspace(hit.path.clone());
            if seen.insert((locator.clone(), hit.line_number)) {
                matches.push(ScopedTextMatch {
                    locator,
                    path: hit.path,
                    line_number: hit.line_number,
                    line_content: hit.line_content,
                    origin: SearchResultOrigin::Workspace,
                });
            }
        }
    }
    if scope.dependencies {
        let mut files = snapshot.files.iter().collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        for file in files {
            for (index, line) in file.content.lines().enumerate() {
                if !line.contains(pattern) {
                    continue;
                }
                let line_number = (index as u64 + 1).min(u64::from(u32::MAX)) as u32;
                let locator = SearchResultLocator::Dependency(file.path.clone());
                if seen.insert((locator.clone(), line_number)) {
                    matches.push(ScopedTextMatch {
                        locator,
                        path: file.path.relative_path().clone(),
                        line_number,
                        line_content: line.to_owned(),
                        origin: SearchResultOrigin::Dependency {
                            root_id: file.path.root_id().clone(),
                        },
                    });
                }
            }
        }
    }
    ScopedTextSearchResult {
        matches,
        dependency_report: snapshot.report.clone(),
    }
}

#[cfg(test)]
mod tests;
