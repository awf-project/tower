//! `domain/refactor` — transactional refactoring operations (spec 09).
//!
//! # Scope
//!
//! This module implements `FileMutationUseCase::global_replace`: a parallel
//! find-and-replace across the entire workspace, composing spec-07 grep
//! (read + search) and spec-08 atomic shadow-file writes.
//!
//! # Design
//!
//! ```text
//!  global_replace(target, replacement):
//!    Phase 1 (parallel, read-only):
//!      for each file: read → count occurrences → apply replacement → FilePatch
//!    Phase 2 (sequential, mutable):
//!      for each FilePatch: atomic shadow-file write → upsert VFS metadata
//!      put_batch(updated VirtualFiles) → storage
//!      return TxReport { files_changed, replacements, errors }
//! ```
//!
//! See [`global_replace::GlobalReplaceService`] for the detailed architecture.
#![forbid(unsafe_code)]

mod global_replace;
mod path_rename;
#[cfg(test)]
mod path_rename_tests;
mod safe_delete;
#[cfg(test)]
mod safe_delete_tests;

pub use global_replace::GlobalReplaceService;
pub use path_rename::{
    PathMove, PathRenameError, PathRenamePlan, PathRenameRequest, PathRenameResult,
    PathRenameSemanticOutcome, PathRenameSemanticPolicy, PathRenameStatus,
};
pub use safe_delete::{
    ResolvedDeclaration, SafeDeleteAnalysis, SafeDeletePlan, SafeDeleteService, SafeDeleteSnapshot,
};

#[cfg(test)]
mod tests;
