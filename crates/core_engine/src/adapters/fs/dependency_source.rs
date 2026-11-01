use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::adapters::config::ResolvedDependencyRoot;
use crate::domain::RelativePath;
use crate::domain::search::{
    DependencyIndexCandidate, DependencyIndexIssue, DependencyIndexIssueReason,
    DependencyIndexLimits, DependencyIndexSnapshot, DependencyPath, DependencyRootId,
    IndexedDependencyFile,
};
use crate::ports::{
    DependencyCandidate, DependencyRead, DependencySourceError, DependencySourcePort,
    PathExclusionDecision, PathExclusionPort,
};

use super::WorkspacePathExclusion;

pub(crate) trait DependencyFileReader: Send + Sync {
    /// Resolves a candidate for the scanner's root-containment check.
    ///
    /// Keeping this at the filesystem boundary makes it possible to exercise
    /// containment failures on platforms without symlink fixtures.
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        path.canonicalize()
    }

    /// Records a candidate that survived enumeration and ignore filtering.
    fn candidate_seen(&self) {}

    fn metadata(&self, path: &Path) -> std::io::Result<std::fs::Metadata>;

    fn read_bounded(&self, path: &Path, max_bytes: NonZeroU64) -> std::io::Result<Vec<u8>>;

    fn read_confined(
        &self,
        root: &Path,
        relative: &Path,
        max_bytes: NonZeroU64,
    ) -> Result<Vec<u8>, ConfinedReadError> {
        let absolute = root.join(relative);
        let canonical = self.canonicalize(&absolute).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ConfinedReadError::Changed
            } else {
                ConfinedReadError::BoundaryViolation
            }
        })?;
        if !canonical.starts_with(root) {
            return Err(ConfinedReadError::BoundaryViolation);
        }
        let before = self.metadata(&absolute).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ConfinedReadError::Changed
            } else {
                ConfinedReadError::Unreadable
            }
        })?;
        if before.len() > max_bytes.get() {
            return Err(ConfinedReadError::LimitExceeded);
        }
        let bytes = self.read_bounded(&absolute, max_bytes).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ConfinedReadError::Changed
            } else {
                ConfinedReadError::Unreadable
            }
        })?;
        let after = self
            .metadata(&absolute)
            .map_err(|_| ConfinedReadError::Changed)?;
        if bytes.len() as u64 != before.len()
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Err(ConfinedReadError::Changed);
        }
        Ok(bytes)
    }

    fn probe_confined(
        &self,
        root: &Path,
        relative: &Path,
        max_bytes: NonZeroU64,
    ) -> Result<(), ConfinedReadError> {
        let absolute = root.join(relative);
        let canonical = self
            .canonicalize(&absolute)
            .map_err(|_| ConfinedReadError::BoundaryViolation)?;
        if !canonical.starts_with(root) {
            return Err(ConfinedReadError::BoundaryViolation);
        }
        self.read_bounded(&absolute, max_bytes)
            .map(|_| ())
            .map_err(|_| ConfinedReadError::Unreadable)
    }

    /// Records content retained by the completed candidate snapshot.
    fn accepted(&self, _bytes: usize) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfinedReadError {
    BoundaryViolation,
    Changed,
    Unreadable,
    LimitExceeded,
}

enum CandidateLoad {
    Indexed {
        file: IndexedDependencyFile,
        size: u64,
    },
    Limited(DependencyCandidate),
    Issue(DependencyIndexIssue),
}

enum EntryClassification {
    Skip,
    Candidate {
        candidate: DependencyCandidate,
        identity: PathBuf,
    },
    Failure(DependencyCandidate),
}

pub(crate) struct RealDependencyFileReader;

impl DependencyFileReader for RealDependencyFileReader {
    fn metadata(&self, path: &Path) -> std::io::Result<fs::Metadata> {
        fs::metadata(path)
    }

    fn read_bounded(&self, path: &Path, max_bytes: NonZeroU64) -> std::io::Result<Vec<u8>> {
        let file = File::open(path)?;
        let capacity =
            usize::try_from(file.metadata()?.len().min(max_bytes.get())).unwrap_or(usize::MAX);
        let mut bytes = Vec::new();
        bytes.try_reserve(capacity).map_err(std::io::Error::other)?;
        file.take(max_bytes.get()).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    #[cfg(target_os = "linux")]
    fn read_confined(
        &self,
        root: &Path,
        relative: &Path,
        max_bytes: NonZeroU64,
    ) -> Result<Vec<u8>, ConfinedReadError> {
        use rustix::fs::{Mode, OFlags, ResolveFlags, open, openat2};

        let root = open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| ConfinedReadError::Unreadable)?;
        let file = openat2(
            &root,
            relative,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|error| match error {
            rustix::io::Errno::NOENT => ConfinedReadError::Changed,
            rustix::io::Errno::LOOP | rustix::io::Errno::XDEV => {
                ConfinedReadError::BoundaryViolation
            }
            _ => ConfinedReadError::Unreadable,
        })?;
        let mut file = File::from(file);
        let before = file.metadata().map_err(|_| ConfinedReadError::Unreadable)?;
        if before.len() > max_bytes.get() {
            return Err(ConfinedReadError::LimitExceeded);
        }
        let capacity = usize::try_from(before.len()).unwrap_or(usize::MAX);
        let mut bytes = Vec::new();
        bytes
            .try_reserve(capacity)
            .map_err(|_| ConfinedReadError::Unreadable)?;
        file.by_ref()
            .take(max_bytes.get())
            .read_to_end(&mut bytes)
            .map_err(|_| ConfinedReadError::Unreadable)?;
        let after = file.metadata().map_err(|_| ConfinedReadError::Changed)?;
        if bytes.len() as u64 != before.len()
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Err(ConfinedReadError::Changed);
        }
        Ok(bytes)
    }

    #[cfg(target_os = "linux")]
    fn probe_confined(
        &self,
        root: &Path,
        relative: &Path,
        max_bytes: NonZeroU64,
    ) -> Result<(), ConfinedReadError> {
        use rustix::fs::{Mode, OFlags, ResolveFlags, open, openat2};

        let root = open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| ConfinedReadError::Unreadable)?;
        let file = openat2(
            &root,
            relative,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|error| match error {
            rustix::io::Errno::NOENT => ConfinedReadError::Changed,
            rustix::io::Errno::LOOP | rustix::io::Errno::XDEV => {
                ConfinedReadError::BoundaryViolation
            }
            _ => ConfinedReadError::Unreadable,
        })?;
        let mut file = File::from(file);
        std::io::copy(
            &mut file.by_ref().take(max_bytes.get()),
            &mut std::io::sink(),
        )
        .map(|_| ())
        .map_err(|_| ConfinedReadError::Unreadable)
    }
}

pub struct FsDependencySource {
    roots: Vec<ResolvedDependencyRoot>,
    workspace_root: PathBuf,
    ignore: Vec<String>,
    reader: Arc<dyn DependencyFileReader>,
}

#[derive(Debug)]
pub enum DependencyScanError {
    Source(DependencySourceError),
    InvariantViolation,
}

impl std::fmt::Display for DependencyScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(source) => write!(formatter, "dependency source failure: {source}"),
            Self::InvariantViolation => formatter.write_str("dependency scan invariant violation"),
        }
    }
}

impl std::error::Error for DependencyScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::InvariantViolation => None,
        }
    }
}

impl FsDependencySource {
    pub fn new(
        roots: Vec<ResolvedDependencyRoot>,
        workspace_root: PathBuf,
        ignore: Vec<String>,
    ) -> Result<Self, DependencySourceError> {
        Self::from_reader(
            roots,
            workspace_root,
            ignore,
            Arc::new(RealDependencyFileReader),
        )
    }

    pub fn build_snapshot(
        &self,
        limits: &DependencyIndexLimits,
    ) -> Result<DependencyIndexSnapshot, DependencyScanError> {
        let mut issues = Vec::new();
        let mut candidates = Vec::new();
        let mut roots = self.available_deduplicated_roots(&mut issues);
        roots.sort_by(|left, right| left.id.cmp(&right.id));

        let mut enumerated = Vec::new();
        for root in roots {
            match self.enumerate(&root.id) {
                Ok(entries) => enumerated.extend(entries),
                Err(error) => issues.push(issue_from_error(error)?),
            }
        }
        enumerated.sort_by(|left, right| left.path.cmp(&right.path));
        enumerated.dedup_by(|left, right| left.path == right.path);

        let max_files = limits.max_files().get();
        let max_file_bytes = limits.max_file_bytes();
        let max_total_bytes = limits.max_total_bytes().get();
        let mut accepted_files = 0_usize;
        let mut accepted_bytes = 0_u64;

        for candidate in enumerated {
            if candidate.size > max_file_bytes.get() || accepted_files == max_files {
                candidates.push(limit_candidate(candidate));
                continue;
            }

            let remaining = max_total_bytes.saturating_sub(accepted_bytes);
            if remaining == 0 {
                candidates.push(limit_candidate(candidate));
                continue;
            }
            let requested = NonZeroU64::new(max_file_bytes.get().min(remaining))
                .ok_or(DependencyScanError::InvariantViolation)?;
            if candidate.size > remaining {
                match self.probe_bounded(&candidate.path, requested) {
                    Ok(()) | Err(ConfinedReadError::LimitExceeded) => {
                        candidates.push(limit_candidate(candidate));
                    }
                    Err(error) => issues.push(issue_for_path(
                        candidate.path,
                        match error {
                            ConfinedReadError::BoundaryViolation => {
                                DependencyIndexIssueReason::BoundaryViolation
                            }
                            ConfinedReadError::Changed => {
                                DependencyIndexIssueReason::ChangedDuringRead
                            }
                            ConfinedReadError::Unreadable => {
                                DependencyIndexIssueReason::EntryUnreadable
                            }
                            ConfinedReadError::LimitExceeded => unreachable!("handled above"),
                        },
                    )),
                }
                continue;
            }

            match self.load_candidate(candidate, requested)? {
                CandidateLoad::Indexed { file, size } => {
                    accepted_files += 1;
                    accepted_bytes += size;
                    candidates.push(DependencyIndexCandidate::Eligible(file));
                }
                CandidateLoad::Limited(candidate) => {
                    candidates.push(limit_candidate(candidate));
                }
                CandidateLoad::Issue(issue) => issues.push(issue),
            }
        }

        Ok(DependencyIndexSnapshot::from_candidates(
            candidates, *limits, issues,
        ))
    }

    #[cfg(test)]
    pub(crate) fn with_reader(
        roots: Vec<ResolvedDependencyRoot>,
        workspace_root: PathBuf,
        ignore: Vec<String>,
        reader: Arc<dyn DependencyFileReader>,
    ) -> Result<Self, DependencySourceError> {
        Self::from_reader(roots, workspace_root, ignore, reader)
    }

    fn from_reader(
        roots: Vec<ResolvedDependencyRoot>,
        workspace_root: PathBuf,
        ignore: Vec<String>,
        reader: Arc<dyn DependencyFileReader>,
    ) -> Result<Self, DependencySourceError> {
        let _ = build_ignore_matcher(Path::new("."), &ignore);
        Ok(Self {
            roots,
            workspace_root,
            ignore,
            reader,
        })
    }

    fn root(&self, root_id: &DependencyRootId) -> Option<&ResolvedDependencyRoot> {
        self.roots.iter().find(|root| &root.id == root_id)
    }

    fn available_deduplicated_roots(
        &self,
        issues: &mut Vec<DependencyIndexIssue>,
    ) -> Vec<ResolvedDependencyRoot> {
        let mut available = Vec::new();
        for root in &self.roots {
            match self.reader.metadata(&root.path) {
                Err(_) => issues.push(DependencyIndexIssue {
                    root_id: root.id.clone(),
                    path: None,
                    reason: DependencyIndexIssueReason::RootUnavailable,
                }),
                Ok(metadata) if !metadata.is_dir() => issues.push(DependencyIndexIssue {
                    root_id: root.id.clone(),
                    path: None,
                    reason: DependencyIndexIssueReason::RootNotDirectory,
                }),
                Ok(_) => match self.reader.canonicalize(&root.path) {
                    Ok(path) => available.push((path, root.clone())),
                    Err(_) => issues.push(DependencyIndexIssue {
                        root_id: root.id.clone(),
                        path: None,
                        reason: DependencyIndexIssueReason::RootUnavailable,
                    }),
                },
            }
        }
        available.sort_by(|(left_path, left), (right_path, right)| {
            left_path
                .components()
                .count()
                .cmp(&right_path.components().count())
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut retained = Vec::<(PathBuf, ResolvedDependencyRoot)>::new();
        for (path, mut root) in available {
            if retained.iter().any(|(outer, _)| path.starts_with(outer)) {
                continue;
            }
            root.path = path.clone();
            retained.push((path, root));
        }
        retained.into_iter().map(|(_, root)| root).collect()
    }

    fn probe_bounded(
        &self,
        path: &DependencyPath,
        max_bytes: NonZeroU64,
    ) -> Result<(), ConfinedReadError> {
        let root = self
            .root(path.root_id())
            .ok_or(ConfinedReadError::Changed)?;
        let canonical_root = self
            .reader
            .canonicalize(&root.path)
            .map_err(|_| ConfinedReadError::Changed)?;
        self.reader.probe_confined(
            &canonical_root,
            Path::new(path.relative_path().as_str()),
            max_bytes,
        )
    }

    fn load_candidate(
        &self,
        candidate: DependencyCandidate,
        max_bytes: NonZeroU64,
    ) -> Result<CandidateLoad, DependencyScanError> {
        match self.read_bounded(&candidate.path, max_bytes) {
            Ok(read) => match String::from_utf8(read.bytes) {
                Ok(content) => {
                    self.reader.accepted(content.len());
                    Ok(CandidateLoad::Indexed {
                        size: candidate.size,
                        file: IndexedDependencyFile::new(candidate.path, candidate.size, content),
                    })
                }
                Err(_) => Ok(CandidateLoad::Issue(issue_for_path(
                    candidate.path,
                    DependencyIndexIssueReason::InvalidEncoding,
                ))),
            },
            Err(DependencySourceError::LimitExceeded { .. }) => {
                Ok(CandidateLoad::Limited(candidate))
            }
            Err(error) => Ok(CandidateLoad::Issue(issue_from_error(error)?)),
        }
    }
}

impl DependencySourcePort for FsDependencySource {
    fn enumerate(
        &self,
        root_id: &DependencyRootId,
    ) -> Result<Vec<DependencyCandidate>, DependencySourceError> {
        let root = self
            .root(root_id)
            .ok_or_else(|| DependencySourceError::RootUnavailable {
                root_id: root_id.clone(),
            })?;
        let metadata = self.reader.metadata(&root.path).map_err(|_| {
            DependencySourceError::RootUnavailable {
                root_id: root_id.clone(),
            }
        })?;
        if !metadata.is_dir() {
            return Err(DependencySourceError::RootNotDirectory {
                root_id: root_id.clone(),
            });
        }
        let canonical_root = self.reader.canonicalize(&root.path).map_err(|_| {
            DependencySourceError::RootUnavailable {
                root_id: root_id.clone(),
            }
        })?;
        let policy = WorkspacePathExclusion::new(canonical_root.clone()).map_err(|_| {
            DependencySourceError::RootUnavailable {
                root_id: root_id.clone(),
            }
        })?;
        let configured_ignore = build_ignore_matcher(&canonical_root, &self.ignore);
        let workspace = self.workspace_root.canonicalize().ok();
        let mut entries = Vec::new();
        let mut identities = BTreeSet::new();

        for walked in WorkspacePathExclusion::walker(&canonical_root) {
            let entry = match walked {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if entry.path() == canonical_root || entry.file_type().is_some_and(|kind| kind.is_dir())
            {
                continue;
            }
            match self.classify_entry(
                root_id,
                &canonical_root,
                &policy,
                &configured_ignore,
                workspace.as_deref(),
                entry.path(),
            )? {
                EntryClassification::Skip => {}
                EntryClassification::Failure(candidate) => entries.push(candidate),
                EntryClassification::Candidate {
                    candidate,
                    identity,
                } => {
                    if identities.insert(identity) {
                        entries.push(candidate);
                    }
                }
            }
        }
        entries.sort();
        Ok(entries)
    }

    fn read_bounded(
        &self,
        path: &DependencyPath,
        max_bytes: NonZeroU64,
    ) -> Result<DependencyRead, DependencySourceError> {
        let root =
            self.root(path.root_id())
                .ok_or_else(|| DependencySourceError::RootUnavailable {
                    root_id: path.root_id().clone(),
                })?;
        let canonical_root = self.reader.canonicalize(&root.path).map_err(|_| {
            DependencySourceError::RootUnavailable {
                root_id: path.root_id().clone(),
            }
        })?;
        let bytes = self
            .reader
            .read_confined(
                &canonical_root,
                Path::new(path.relative_path().as_str()),
                max_bytes,
            )
            .map_err(|error| match error {
                ConfinedReadError::BoundaryViolation => {
                    DependencySourceError::BoundaryViolation { path: path.clone() }
                }
                ConfinedReadError::Changed => {
                    DependencySourceError::ChangedDuringRead { path: path.clone() }
                }
                ConfinedReadError::Unreadable => {
                    DependencySourceError::EntryUnreadable { path: path.clone() }
                }
                ConfinedReadError::LimitExceeded => DependencySourceError::LimitExceeded {
                    path: path.clone(),
                    max_bytes,
                },
            })?;
        if std::str::from_utf8(&bytes).is_err() {
            return Err(DependencySourceError::InvalidEncoding { path: path.clone() });
        }
        Ok(DependencyRead {
            path: path.clone(),
            bytes,
        })
    }
}

impl FsDependencySource {
    fn classify_entry(
        &self,
        root_id: &DependencyRootId,
        canonical_root: &Path,
        policy: &WorkspacePathExclusion,
        configured_ignore: &Gitignore,
        workspace: Option<&Path>,
        absolute: &Path,
    ) -> Result<EntryClassification, DependencySourceError> {
        let Some(relative) = absolute.strip_prefix(canonical_root).ok() else {
            return Ok(EntryClassification::Skip);
        };
        let Some(relative) = relative.to_str() else {
            return Ok(EntryClassification::Skip);
        };
        let relative = RelativePath::new(relative.replace('\\', "/"));
        let path = DependencyPath::new(root_id.clone(), relative.clone()).map_err(|_| {
            DependencySourceError::BoundaryViolation {
                path: fallback_path(root_id),
            }
        })?;
        match policy.classify(&relative) {
            Ok(PathExclusionDecision::Allowed) => {}
            Ok(PathExclusionDecision::Excluded(_)) => return Ok(EntryClassification::Skip),
            Err(_) => {
                return Ok(EntryClassification::Failure(DependencyCandidate {
                    path,
                    size: 0,
                }));
            }
        }
        if configured_path_is_ignored(configured_ignore, canonical_root, absolute) {
            return Ok(EntryClassification::Skip);
        }
        self.reader.candidate_seen();
        let canonical = match self.reader.canonicalize(absolute) {
            Ok(canonical) if canonical.starts_with(canonical_root) => canonical,
            Ok(_) | Err(_) => {
                return Ok(EntryClassification::Failure(DependencyCandidate {
                    path,
                    size: 0,
                }));
            }
        };
        if workspace.is_some_and(|root| canonical.starts_with(root)) {
            return Ok(EntryClassification::Skip);
        }
        let size = self
            .reader
            .metadata(absolute)
            .map_err(|_| DependencySourceError::EntryUnreadable { path: path.clone() })?
            .len();
        Ok(EntryClassification::Candidate {
            candidate: DependencyCandidate { path, size },
            identity: canonical,
        })
    }
}

fn build_ignore_matcher(root: &Path, patterns: &[String]) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        let _ = builder.add_line(None, pattern);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn configured_path_is_ignored(matcher: &Gitignore, root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let mut ancestor = relative.parent();
    while let Some(directory) = ancestor {
        if directory.as_os_str().is_empty() {
            break;
        }
        if matcher.matched(root.join(directory), true).is_ignore() {
            return true;
        }
        ancestor = directory.parent();
    }
    matcher.matched_path_or_any_parents(path, false).is_ignore()
}

fn fallback_path(root_id: &DependencyRootId) -> DependencyPath {
    DependencyPath::new(root_id.clone(), RelativePath::new("invalid"))
        .expect("static dependency path is valid")
}

fn limit_candidate(candidate: DependencyCandidate) -> DependencyIndexCandidate {
    DependencyIndexCandidate::Eligible(IndexedDependencyFile::new(
        candidate.path,
        candidate.size,
        String::new(),
    ))
}

fn issue_from_error(
    error: DependencySourceError,
) -> Result<DependencyIndexIssue, DependencyScanError> {
    Ok(match error {
        DependencySourceError::RootUnavailable { root_id } => DependencyIndexIssue {
            root_id,
            path: None,
            reason: DependencyIndexIssueReason::RootUnavailable,
        },
        DependencySourceError::RootNotDirectory { root_id } => DependencyIndexIssue {
            root_id,
            path: None,
            reason: DependencyIndexIssueReason::RootNotDirectory,
        },
        DependencySourceError::EntryUnreadable { path } => {
            issue_for_path(path, DependencyIndexIssueReason::EntryUnreadable)
        }
        DependencySourceError::ChangedDuringRead { path } => {
            issue_for_path(path, DependencyIndexIssueReason::ChangedDuringRead)
        }
        DependencySourceError::InvalidEncoding { path } => {
            issue_for_path(path, DependencyIndexIssueReason::InvalidEncoding)
        }
        DependencySourceError::BoundaryViolation { path } => {
            issue_for_path(path, DependencyIndexIssueReason::BoundaryViolation)
        }
        error @ DependencySourceError::LimitExceeded { .. } => {
            return Err(DependencyScanError::Source(error));
        }
    })
}

fn issue_for_path(
    path: DependencyPath,
    reason: DependencyIndexIssueReason,
) -> DependencyIndexIssue {
    DependencyIndexIssue {
        root_id: path.root_id().clone(),
        path: Some(path.relative_path().clone()),
        reason,
    }
}
