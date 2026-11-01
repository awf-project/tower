use std::num::NonZeroU64;

use crate::domain::search::{DependencyPath, DependencyRootId};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DependencyCandidate {
    pub path: DependencyPath,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyRead {
    pub path: DependencyPath,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DependencySourceError {
    RootUnavailable {
        root_id: DependencyRootId,
    },
    RootNotDirectory {
        root_id: DependencyRootId,
    },
    EntryUnreadable {
        path: DependencyPath,
    },
    ChangedDuringRead {
        path: DependencyPath,
    },
    LimitExceeded {
        path: DependencyPath,
        max_bytes: NonZeroU64,
    },
    InvalidEncoding {
        path: DependencyPath,
    },
    BoundaryViolation {
        path: DependencyPath,
    },
}

impl core::fmt::Display for DependencySourceError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RootUnavailable { root_id } => {
                write!(formatter, "dependency root {root_id:?} is unavailable")
            }
            Self::RootNotDirectory { root_id } => {
                write!(formatter, "dependency root {root_id:?} is not a directory")
            }
            Self::EntryUnreadable { path } => write!(
                formatter,
                "dependency entry {} is unreadable",
                path.relative_path().as_str()
            ),
            Self::ChangedDuringRead { path } => write!(
                formatter,
                "dependency entry {} changed during read",
                path.relative_path().as_str()
            ),
            Self::LimitExceeded { path, max_bytes } => write!(
                formatter,
                "dependency entry {} exceeds the {max_bytes}-byte limit",
                path.relative_path().as_str()
            ),
            Self::InvalidEncoding { path } => write!(
                formatter,
                "dependency entry {} has invalid encoding",
                path.relative_path().as_str()
            ),
            Self::BoundaryViolation { path } => write!(
                formatter,
                "dependency entry {} violates its root boundary",
                path.relative_path().as_str()
            ),
        }
    }
}

impl core::error::Error for DependencySourceError {}

pub trait DependencySourcePort: Send + Sync {
    fn enumerate(
        &self,
        root_id: &DependencyRootId,
    ) -> Result<Vec<DependencyCandidate>, DependencySourceError>;

    fn read_bounded(
        &self,
        path: &DependencyPath,
        max_bytes: NonZeroU64,
    ) -> Result<DependencyRead, DependencySourceError>;
}
