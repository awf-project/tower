use crate::domain::RelativePath;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathExclusionDecision {
    Allowed,
    Excluded(PathExclusionReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathExclusionReason {
    IgnoreRule,
    Hidden,
    GitMetadata,
    AtomicWriteArtifact,
    FormatterTemporary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathExclusionError {
    OutsideWorkspace,
    SymlinkEscape,
    PolicyEvaluation,
}

pub trait PathExclusionPort: Send + Sync {
    fn classify(&self, path: &RelativePath) -> Result<PathExclusionDecision, PathExclusionError>;
}
