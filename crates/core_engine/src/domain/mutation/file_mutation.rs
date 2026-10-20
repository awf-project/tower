//! `FileMutationService` — concrete implementation of [`FileMutationUseCase`].
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use extension_protocol::messages::{
    SafeDeleteFileError, SafeDeleteFileOutcome, SafeDeleteFileOutcomeStatus,
    SafeDeleteMutationRequest, SafeDeleteMutationResult, SafeDeleteMutationStatus,
    SafeDeleteRejectionCode, is_safe_delete_path,
};

use crate::domain::index::InvertedIndex;
use crate::domain::mutation::compute_content_version;
use crate::domain::refactor::GlobalReplaceService;
use crate::domain::refactor::{
    PathMove, PathRenameError, PathRenamePlan, PathRenameRequest, PathRenameResult,
    PathRenameSemanticOutcome, PathRenameSemanticPolicy, PathRenameStatus,
};
use crate::domain::token::tokenize;
use crate::domain::virtual_file::{FileMetadata, Timestamp};
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, RelativePath};
use crate::ports::inbound::{
    ApplyEditsFileResult, ApplyEditsPreview, ApplyEditsRequest, DirectoryEntryKind,
    FileMutationUseCase, FileReplaceError, PerFileEditResult, SkippedEdit, SkippedEditReason,
    TextEdit, TxReport, WorkspaceApplyEditsError, WorkspaceApplyEditsErrorCode,
    WorkspaceApplyEditsRequest, WorkspaceApplyEditsResult, WorkspaceEditSpan,
};
use crate::ports::{
    ExtensionHostPort, FileSystemPort, PathExclusionDecision, PathExclusionError,
    PathExclusionPort, PortError, ReconciliationDelta, StoragePort,
};

// ── FileMutationService ───────────────────────────────────────────────────────

/// Implements [`FileMutationUseCase`]: create/overwrite files, create
/// directories, and delete files — keeping the VFS, inverted index, and
/// storage in sync.
///
/// # Ownership model
///
/// The service borrows all state mutably for the duration of a call sequence.
/// The [`FileSystemPort`] is borrowed rather than owned so the same FS
/// instance can be reused across multiple service method calls and shared
/// with other components (e.g. an `InMemoryFs` in tests).
///
/// # Example
///
/// ```rust
/// use core_engine::domain::mutation::FileMutationService;
/// use core_engine::domain::workspace::ProjectWorkspace;
/// use core_engine::domain::index::InvertedIndex;
/// use core_engine::domain::RelativePath;
/// use core_engine::adapters::{InMemoryFs, InMemoryStorage};
/// use core_engine::ports::inbound::FileMutationUseCase;
/// use core_engine::ports::NoOpExtensionHost;
///
/// let mut workspace = ProjectWorkspace::new();
/// let mut index = InvertedIndex::new();
/// let mut storage = InMemoryStorage::new();
/// let mut fs = InMemoryFs::new();
///
/// let mut svc = FileMutationService::new(
///     &mut fs,
///     &mut workspace,
///     &mut index,
///     &mut storage,
///     &NoOpExtensionHost,
/// );
///
/// let path = RelativePath::new("src/lib.rs");
/// svc.create_file(path.clone(), b"fn main() {}".to_vec()).unwrap();
///
/// // The file is now tracked in the VFS.
/// assert!(workspace.get_by_path(&path).is_some());
/// ```
pub struct FileMutationService<'ws> {
    fs: &'ws mut dyn FileSystemPort,
    workspace: &'ws mut ProjectWorkspace,
    index: &'ws mut InvertedIndex,
    storage: &'ws mut dyn StoragePort,
    extension_host: &'ws dyn ExtensionHostPort,
}

impl<'ws> FileMutationService<'ws> {
    fn validate_rename_path(
        path: &RelativePath,
        exclusions: &dyn PathExclusionPort,
    ) -> Result<(), PathRenameError> {
        if path.as_str().is_empty()
            || std::path::Path::new(path.as_str()).is_absolute()
            || std::path::Path::new(path.as_str())
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(PathRenameError::WorkspaceEscape { path: path.clone() });
        }
        match exclusions.classify(path) {
            Ok(PathExclusionDecision::Allowed) => Ok(()),
            Ok(PathExclusionDecision::Excluded(reason)) => {
                Err(PathRenameError::DestinationExcluded {
                    path: path.clone(),
                    reason: format!("{reason:?}"),
                })
            }
            Err(PathExclusionError::OutsideWorkspace | PathExclusionError::SymlinkEscape) => {
                Err(PathRenameError::WorkspaceEscape { path: path.clone() })
            }
            Err(PathExclusionError::PolicyEvaluation) => {
                Err(PathRenameError::DestinationExcluded {
                    path: path.clone(),
                    reason: "policy evaluation failed".to_owned(),
                })
            }
        }
    }

    fn current_version(&self, path: &RelativePath) -> Result<String, PathRenameError> {
        self.fs
            .read(path)
            .map(|bytes| compute_content_version(&bytes))
            .map_err(|error| match error {
                PortError::NotFound => PathRenameError::SourceNotFound { path: path.clone() },
                _ => PathRenameError::Filesystem,
            })
    }

    fn validate_caller_versions(&self, plan: &PathRenamePlan) -> Result<(), PathRenameError> {
        for (path, expected) in plan.expected_versions() {
            let actual = self.current_version(path)?;
            if &actual != expected {
                return Err(PathRenameError::VersionConflict {
                    path: path.clone(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }

    fn validate_destination_absent(&self, plan: &PathRenamePlan) -> Result<(), PathRenameError> {
        for path_move in plan.moves() {
            if self.fs.exists(&path_move.destination) {
                return Err(PathRenameError::DestinationExists {
                    path: path_move.destination.clone(),
                });
            }
        }
        Ok(())
    }

    fn validate_move_membership(&self, plan: &PathRenamePlan) -> Result<(), PathRenameError> {
        let root = &plan.moves()[0];
        let expected: HashSet<_> = plan
            .moved_identities()
            .iter()
            .map(|(path, id)| (path.clone(), *id))
            .collect();
        let actual: HashSet<_> = if root.kind == DirectoryEntryKind::Dir {
            let prefix = format!("{}/", root.source.as_str());
            self.workspace
                .snapshot()
                .files
                .into_iter()
                .filter(|file| file.path.as_str().starts_with(&prefix))
                .map(|file| (file.path, file.id))
                .collect()
        } else {
            self.workspace
                .get_by_path(&root.source)
                .map(|id| HashSet::from([(root.source.clone(), id)]))
                .unwrap_or_default()
        };
        if actual == expected {
            Ok(())
        } else {
            Err(PathRenameError::SourceNotFound {
                path: root.source.clone(),
            })
        }
    }

    /// Construct a new service.
    ///
    /// # Arguments
    ///
    /// - `fs`             — file-system port (borrowed mutably so the same
    ///   instance can be reused across multiple calls).
    /// - `workspace`      — mutable workspace aggregate.
    /// - `index`          — mutable inverted index.
    /// - `storage`        — mutable storage port.
    /// - `extension_host` — lifecycle hook receiver (`NoOpExtensionHost` when none).
    pub fn new(
        fs: &'ws mut dyn FileSystemPort,
        workspace: &'ws mut ProjectWorkspace,
        index: &'ws mut InvertedIndex,
        storage: &'ws mut dyn StoragePort,
        extension_host: &'ws dyn ExtensionHostPort,
    ) -> Self {
        Self {
            fs,
            workspace,
            index,
            storage,
            extension_host,
        }
    }

    /// Shared "commit an indexed full-file write" tail (spec 17 REFACTOR).
    ///
    /// Shared commit tail for a full-file rewrite of an **already-tracked**
    /// file. Called only by [`FileMutationUseCase::edit_range`], which resolves
    /// the live `file_id` before computing the spliced bytes. Performs:
    ///
    /// 1. Atomically write `content` to disk via the spec-08 shadow-file pattern.
    /// 2. Update VFS metadata in-place on the existing slot (stable `FileId`).
    /// 3. Delta-reindex (remove old tokens + insert new).
    /// 4. `storage.put` to persist the updated `VirtualFile` record.
    /// 5. Broadcast `on_file_changed` to the plugin host.
    ///
    /// [`FileMutationUseCase::create_file`] does **not** route through this
    /// helper: it has upsert semantics (insert-or-update via `workspace.insert`
    /// with a `DuplicatePath` fallback) and keeps its own inline commit sequence.
    /// This helper deliberately handles only the update-existing case, so it
    /// takes a pre-resolved `file_id` and calls `workspace.update` directly.
    ///
    /// # Arguments
    ///
    /// - `path`    — workspace-relative path (used for tokenisation + broadcast).
    /// - `file_id` — pre-resolved, live `FileId` for the existing VFS slot.
    /// - `content` — the full new file bytes to write and commit.
    ///
    /// # Errors
    ///
    /// Propagates `atomic_write` failures as `DomainError::IoError` and
    /// `storage.put` failures likewise.
    fn commit_indexed_write(
        &mut self,
        path: &RelativePath,
        file_id: crate::domain::FileId,
        content: Vec<u8>,
    ) -> Result<(), DomainError> {
        // Step A: shadow-file write + atomic rename.
        super::atomic_write(self.fs, path, content).map_err(DomainError::IoError)?;

        // Step B: VFS — update metadata in-place (stable FileId; the slot is
        // already live for both create_file's overwrite branch and edit_range).
        let metadata = FileMetadata {
            size: 0,
            modified: Timestamp(0),
            content_hash: None,
        };
        self.workspace
            .update(file_id, metadata)
            .expect("file_id is live — update must succeed");

        // Step C: delta-reindex.
        let tokens = tokenize(path.as_str());
        self.index.remove(file_id);
        self.index.insert(file_id, &tokens);

        // Step D: persist the updated VirtualFile record.
        let virtual_file = self
            .workspace
            .get(file_id)
            .expect("just updated — slot must be live")
            .clone();
        self.storage.put(virtual_file).map_err(port_err_to_domain)?;

        // Step E: broadcast.
        self.extension_host.on_file_changed(file_id, path);

        Ok(())
    }
}

impl<'ws> FileMutationUseCase for FileMutationService<'ws> {
    fn plan_path_rename(
        &self,
        request: PathRenameRequest,
        exclusions: &dyn PathExclusionPort,
    ) -> Result<PathRenamePlan, PathRenameError> {
        Self::validate_rename_path(&request.source, exclusions)?;
        Self::validate_rename_path(&request.destination, exclusions)?;
        if request.source == request.destination {
            return Err(PathRenameError::IdenticalPaths {
                path: request.source,
            });
        }

        let source_id = self.workspace.get_by_path(&request.source);
        let prefix = format!("{}/", request.source.as_str());
        let mut descendants: Vec<_> = self
            .workspace
            .snapshot()
            .files
            .into_iter()
            .filter(|file| file.path.as_str().starts_with(&prefix))
            .collect();
        descendants.sort_by(|left, right| left.path.cmp(&right.path));
        let is_directory = source_id.is_none() && !descendants.is_empty();
        if source_id.is_none() && !is_directory {
            return Err(PathRenameError::SourceNotFound {
                path: request.source,
            });
        }

        let mut moves = Vec::with_capacity(1 + descendants.len());
        let mut identities =
            Vec::with_capacity(descendants.len() + usize::from(source_id.is_some()));
        if let Some(id) = source_id {
            moves.push(PathMove {
                source: request.source.clone(),
                destination: request.destination.clone(),
                kind: DirectoryEntryKind::File,
            });
            identities.push((request.source.clone(), id));
        } else {
            moves.push(PathMove {
                source: request.source.clone(),
                destination: request.destination.clone(),
                kind: DirectoryEntryKind::Dir,
            });
            for file in descendants {
                let suffix = file
                    .path
                    .as_str()
                    .strip_prefix(request.source.as_str())
                    .unwrap();
                let destination =
                    RelativePath::new(format!("{}{suffix}", request.destination.as_str()));
                Self::validate_rename_path(&destination, exclusions)?;
                moves.push(PathMove {
                    source: file.path.clone(),
                    destination,
                    kind: DirectoryEntryKind::File,
                });
                identities.push((file.path, file.id));
            }
        }

        let mut staged = ProjectWorkspace::from_snapshot(self.workspace.snapshot())
            .map_err(|_| PathRenameError::SemanticBackend)?;
        staged.remap_paths(&moves)?;
        let outcome = match request.semantic_policy {
            PathRenameSemanticPolicy::None => PathRenameSemanticOutcome::NotRequested,
            _ => PathRenameSemanticOutcome::Pending,
        };
        let plan = PathRenamePlan::new(
            moves,
            Vec::new(),
            request.semantic_policy,
            request.dry_run,
            request.expected_versions,
            outcome,
            Vec::new(),
            identities,
        )?;
        self.validate_caller_versions(&plan)?;
        self.validate_destination_absent(&plan)?;
        Ok(plan)
    }

    fn complete_path_rename_plan(
        &self,
        plan: PathRenamePlan,
        semantic: Result<Vec<WorkspaceEditSpan>, PathRenameError>,
    ) -> Result<PathRenamePlan, PathRenameError> {
        if plan.semantic_policy() == PathRenameSemanticPolicy::None {
            return Ok(plan);
        }
        self.validate_caller_versions(&plan)?;
        let (mut edits, outcome, errors) = match semantic {
            Ok(edits) => (edits, PathRenameSemanticOutcome::Planned, Vec::new()),
            Err(error) if plan.semantic_policy() == PathRenameSemanticPolicy::Required => {
                return Err(error);
            }
            Err(PathRenameError::SemanticUnsupported) => (
                Vec::new(),
                PathRenameSemanticOutcome::Unsupported,
                vec![PathRenameError::SemanticUnsupported],
            ),
            Err(
                error
                @ (PathRenameError::SemanticBackend | PathRenameError::WorkspaceEscape { .. }),
            ) => (Vec::new(), PathRenameSemanticOutcome::Failed, vec![error]),
            Err(_) => return Err(PathRenameError::SemanticBackend),
        };
        let invalid_target = edits.iter().find_map(|edit| {
            let raw_path = std::path::Path::new(edit.path.as_str());
            let invalid_path = edit.path.as_str().is_empty()
                || raw_path.is_absolute()
                || raw_path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir));
            invalid_path
                .then(|| PathRenameError::WorkspaceEscape {
                    path: edit.path.clone(),
                })
                .or_else(|| {
                    let moved = plan.moves().iter().any(|path_move| {
                        path_move.kind == DirectoryEntryKind::File && path_move.source == edit.path
                    });
                    (!moved && self.workspace.get_by_path(&edit.path).is_none()).then(|| {
                        PathRenameError::WorkspaceEscape {
                            path: edit.path.clone(),
                        }
                    })
                })
        });
        if let Some(error) = invalid_target {
            if plan.semantic_policy() == PathRenameSemanticPolicy::Required {
                return Err(error);
            }
            edits.clear();
            return PathRenamePlan::new(
                plan.moves().to_vec(),
                edits,
                plan.semantic_policy(),
                plan.dry_run(),
                plan.expected_versions().clone(),
                PathRenameSemanticOutcome::Failed,
                vec![error],
                plan.moved_identities().to_vec(),
            );
        }
        for edit in &mut edits {
            if let Some(path_move) = plan.moves().iter().find(|path_move| {
                path_move.kind == DirectoryEntryKind::File && path_move.source == edit.path
            }) {
                edit.path = path_move.destination.clone();
            }
        }
        let ready = PathRenamePlan::new(
            plan.moves().to_vec(),
            edits,
            plan.semantic_policy(),
            plan.dry_run(),
            plan.expected_versions().clone(),
            outcome,
            errors,
            plan.moved_identities().to_vec(),
        )?;
        Ok(ready)
    }

    fn apply_path_rename(
        &mut self,
        plan: PathRenamePlan,
        exclusions: &dyn PathExclusionPort,
    ) -> Result<PathRenameResult, PathRenameError> {
        if plan.semantic_outcome() == PathRenameSemanticOutcome::Pending {
            return Err(PathRenameError::SemanticBackend);
        }
        for path_move in plan.moves() {
            Self::validate_rename_path(&path_move.destination, exclusions)?;
        }
        self.validate_caller_versions(&plan)?;
        self.validate_move_membership(&plan)?;
        self.validate_destination_absent(&plan)?;

        let current_identities: Vec<_> = plan
            .moved_identities()
            .iter()
            .map(
                |(path, expected_id)| match self.workspace.get_by_path(path) {
                    Some(id) if id == *expected_id => Ok((path.clone(), id)),
                    _ => Err(PathRenameError::SourceNotFound { path: path.clone() }),
                },
            )
            .collect::<Result<_, _>>()?;
        let mut staged = ProjectWorkspace::from_snapshot(self.workspace.snapshot())
            .map_err(|_| PathRenameError::SemanticBackend)?;
        staged.remap_paths(plan.moves())?;
        if plan.dry_run() {
            return Ok(PathRenameResult {
                status: PathRenameStatus::Preview,
                plan,
                edit_result: None,
                errors: Vec::new(),
            });
        }

        let root = &plan.moves()[0];
        if root.kind == DirectoryEntryKind::Dir {
            self.fs
                .rename(&root.source, root.destination.clone())
                .map_err(|_| PathRenameError::Filesystem)?;
        } else {
            self.fs
                .rename(&root.source, root.destination.clone())
                .map_err(|_| PathRenameError::Filesystem)?;
        }

        self.workspace
            .remap_paths(plan.moves())
            .expect("path rename was staged and validated before filesystem movement");
        let mut additions = Vec::with_capacity(current_identities.len());
        for (_, id) in &current_identities {
            self.index.remove(*id);
            let file = self
                .workspace
                .get(*id)
                .expect("moved identity remains live")
                .clone();
            self.index.insert(*id, &tokenize(file.path.as_str()));
            additions.push(file);
        }
        let delta = ReconciliationDelta {
            removals: current_identities.iter().map(|(_, id)| *id).collect(),
            additions,
        };
        let persistence_failed = self.storage.apply_reconciliation_delta(&delta).is_err();
        if persistence_failed {
            return Ok(PathRenameResult {
                status: PathRenameStatus::Partial,
                plan,
                edit_result: None,
                errors: vec![PathRenameError::Persistence],
            });
        }

        let mut errors = plan.semantic_errors().to_vec();
        let edit_result = if plan.edits().is_empty() {
            None
        } else {
            Some(
                self.apply_batch_edits(WorkspaceApplyEditsRequest {
                    edits: plan.edits().to_vec(),
                    dry_run: Some(false),
                })
                .map_err(|_| PathRenameError::SemanticBackend)?,
            )
        };
        let edits_failed = edit_result
            .as_ref()
            .is_some_and(|result| result.per_file.iter().any(|file| !file.applied));
        Ok(PathRenameResult {
            status: if edits_failed {
                PathRenameStatus::Partial
            } else {
                PathRenameStatus::Applied
            },
            plan,
            edit_result,
            errors: std::mem::take(&mut errors),
        })
    }

    /// Create or overwrite the file at `path` with `content`.
    ///
    /// Uses the shadow-file pattern:
    /// 1. Write `content` to `<path>.tmp_write` (durable flush by the port).
    /// 2. Atomically rename `<path>.tmp_write` → `path`.
    /// 3. Upsert VFS + index + storage.
    /// 4. Broadcast `on_file_changed`.
    ///
    /// # Errors
    ///
    /// Returns a domain error on port failure (WriteFailed from rename maps to
    /// `DomainError::IoError`). `DuplicatePath` is handled internally as an
    /// overwrite (upsert semantics).
    ///
    /// # Crash safety
    ///
    /// A crash after step 1 but before step 2 leaves `<path>.tmp_write` on
    /// disk; the original `path` is untouched. The stray `.tmp_write` is
    /// filtered out by the scanner and watcher (AC5). Steps 3–4 only execute
    /// if step 2 succeeds, so a crash mid-rename leaves the domain state clean.
    ///
    /// # Watcher idempotency
    ///
    /// After this call the OS watcher will emit a `Create`/`Modify` event for
    /// `path`. `EventProcessor::handle_create` is guarded by `ws.get_by_path`
    /// — a path already tracked is a silent no-op. `handle_modify` updates
    /// metadata in-place, preserving the `FileId`. No extra suppression needed.
    fn create_file(&mut self, path: RelativePath, content: Vec<u8>) -> Result<(), DomainError> {
        // ── Step 1 & 2: shadow-file write + atomic rename ─────────────────────
        //
        // Decision: delegate to the canonical `atomic_write` primitive in
        // `mutation/mod.rs` rather than inlining the tmp+rename logic here.
        //
        // Why: `InMemoryFs::write` is a direct insert (no internal
        // temp+rename). If we called `fs.write(dst)`, a crash between write
        // and index/storage update would leave the file on disk with no VFS
        // entry — observable partial state. With the domain-level `.tmp_write`
        // + rename, a crash before rename leaves `.tmp_write` (GC-able) and
        // the original dst untouched; a crash after rename but before VFS
        // update is not possible in single-threaded domain code.
        //
        // For `RealFs`, `fs.write(tmp)` also does an internal `.~tmp` +
        // fsync + rename. This "double-shadow" is intentional: the inner
        // `.~tmp` ensures the `.tmp_write` file itself is durably written
        // before the domain renames it over the destination.
        super::atomic_write(self.fs, &path, content).map_err(DomainError::IoError)?;

        // ── Step 3: VFS + index + storage ─────────────────────────────────────
        //
        // Size is derived from what the FS just accepted. We use 0 here because
        // the domain must not re-read the FS for metadata (that would introduce
        // an unnecessary round-trip and a potential TOCTOU race). The watcher
        // will update the size via a Modify event with real metadata.
        let metadata = FileMetadata {
            size: 0,
            modified: Timestamp(0),
            content_hash: None,
        };

        let tokens = tokenize(path.as_str());

        let file_id = match self.workspace.insert(path.clone(), metadata) {
            Ok(id) => {
                // New file: insert into index.
                self.index.insert(id, &tokens);
                id
            }
            Err(DomainError::DuplicatePath) => {
                // Overwrite path: update metadata in-place (stable FileId),
                // then delta-reindex.
                let id = self
                    .workspace
                    .get_by_path(&path)
                    .expect("DuplicatePath guarantees get_by_path succeeds");
                self.workspace
                    .update(id, metadata)
                    .expect("get_by_path returned a valid id — slot must be live");
                self.index.remove(id);
                self.index.insert(id, &tokens);
                id
            }
            Err(e) => return Err(e),
        };

        let virtual_file = self
            .workspace
            .get(file_id)
            .expect("just inserted/updated — slot must be live")
            .clone();

        self.storage.put(virtual_file).map_err(port_err_to_domain)?;

        // ── Step 4: broadcast ─────────────────────────────────────────────────
        self.extension_host.on_file_changed(file_id, &path);

        Ok(())
    }

    /// Create a directory at `path` recursively (EV4/AC4).
    ///
    /// Directories are not tracked in the VFS — only files are indexed.
    ///
    /// # Errors
    ///
    /// Returns a domain error if the port signals a write failure.
    fn create_directory(&mut self, path: RelativePath) -> Result<(), DomainError> {
        self.fs.mkdir(path).map_err(port_err_to_domain)
    }

    /// Delete the file at `path` (EV3/AC3).
    ///
    /// Order:
    /// 1. Resolve `FileId` from the VFS — `DomainError::NotFound` if absent.
    /// 2. Remove from workspace + index (in-memory state cleared).
    /// 3. Physically remove the file via the FS port.
    /// 4. Delete from storage (persist the removal).
    /// 5. Broadcast `on_file_changed`.
    ///
    /// # Crash-safe ordering
    ///
    /// Decision: physical delete (step 3) fires before storage delete (step 4).
    ///
    /// Why: a crash between step 3 and step 4 leaves the file gone from disk
    /// with a dangling sled entry. On restart the watcher fires a Delete event
    /// that cleans up the stale storage record — self-healing. The inverse
    /// ordering (storage first) would leave the file on disk with no domain
    /// record and no watcher event to repair it, making it permanently invisible
    /// to the domain API until a full rescan.
    ///
    /// Trade-off: a crash after fs.delete but before storage.delete leaves a
    /// dangling storage entry (minor leak). The watcher heals it. A crash after
    /// storage.delete but before fs.delete would leave the file permanently
    /// hidden from the domain API until a full rescan.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when `path` is not tracked.
    fn delete_file(&mut self, path: &RelativePath) -> Result<(), DomainError> {
        // Step 1: resolve the FileId from the VFS.
        let file_id = self
            .workspace
            .get_by_path(path)
            .ok_or(DomainError::NotFound)?;

        // Step 2: remove from VFS + index (in-memory; always succeeds).
        self.workspace
            .remove(file_id)
            .expect("get_by_path returned a valid id — slot must be live");
        self.index.remove(file_id);

        // Step 3: physical removal first (crash-safe ordering — see doc above).
        // A NotFound from the FS is tolerated — if the file was already removed
        // from disk we still want to clean up the domain state.
        match self.fs.delete(path) {
            Ok(()) | Err(PortError::NotFound) => {}
            Err(e) => return Err(port_err_to_domain(e)),
        }

        // Step 4: persist the removal.
        // Ignore NotFound from storage — the file may never have been persisted
        // (e.g. created and deleted between flush cycles).
        match self.storage.delete(file_id) {
            Ok(()) | Err(PortError::NotFound) => {}
            Err(e) => return Err(port_err_to_domain(e)),
        }

        // Step 5: broadcast.
        self.extension_host.on_file_changed(file_id, path);

        Ok(())
    }

    /// Global find-and-replace across all indexed files (spec 09).
    ///
    /// Composes spec-07 content reading and spec-08 atomic shadow-file writes.
    /// Parallelises the read+apply phase with Rayon; commits writes sequentially
    /// to avoid data races on shared mutable state (VFS + index + storage).
    ///
    /// Per-file failures (e.g. read-only files) are collected in
    /// `TxReport::errors`; the operation continues for all remaining files
    /// (partial-failure semantics, UN1/AC2).
    ///
    /// # Errors
    ///
    /// Returns `DomainError::IoError` only if the batch storage flush fails
    /// after all FS writes succeeded. Individual per-file errors are in
    /// `TxReport::errors`, not in the `Err` path.
    fn global_replace(&mut self, target: &str, replacement: &str) -> Result<TxReport, DomainError> {
        GlobalReplaceService::new(self.fs, self.workspace, self.storage).execute(
            target,
            replacement,
            false,
        )
    }

    /// Dry-run variant: compute the would-change report without writing any
    /// file or mutating any state (OP1/AC4).
    ///
    /// # Errors
    ///
    /// Returns `DomainError` only if the workspace cannot be read.
    fn global_replace_dry_run(
        &mut self,
        target: &str,
        replacement: &str,
    ) -> Result<TxReport, DomainError> {
        GlobalReplaceService::new(self.fs, self.workspace, self.storage).execute(
            target,
            replacement,
            true,
        )
    }

    /// Replace the byte range `[start_byte, end_byte)` with `replacement`
    /// (spec 17 — surgical range edit).
    ///
    /// # Algorithm
    ///
    /// 1. Read current bytes via `FileSystemPort::read`.
    /// 2. Validate range: `0 ≤ start ≤ end ≤ len` and both on UTF-8 char
    ///    boundaries — return [`DomainError::InvalidRange`] without touching
    ///    the file if validation fails.
    /// 3. Splice: `bytes[..start] ++ replacement ++ bytes[end..]`.
    /// 4. Atomic write via the spec-08 shadow-file primitive.
    /// 5. Commit: update VFS metadata + delta-reindex + `storage.put` +
    ///    broadcast `on_file_changed`.
    ///
    /// # Errors
    ///
    /// See [`FileMutationUseCase::edit_range`] for the full error table.
    fn edit_range(
        &mut self,
        path: &RelativePath,
        start_byte: usize,
        end_byte: usize,
        replacement: &str,
    ) -> Result<TxReport, DomainError> {
        // ── Step 1: read current bytes ─────────────────────────────────────────
        //
        // We read through the FS port rather than the VFS so we always have
        // the actual on-disk bytes (same source as `atomic_write` will target).
        // The FS port returns `PortError::NotFound` if the file does not exist
        // on disk; however, we want to surface the "not tracked in VFS" case
        // first (the spec's UN2 requirement states "edit_range edits existing
        // files ONLY; never creates"). Check the workspace before reading.
        let file_id = self
            .workspace
            .get_by_path(path)
            .ok_or(DomainError::NotFound)?;

        let bytes = self.fs.read(path).map_err(port_err_to_domain)?;

        // ── Step 2: validate range ─────────────────────────────────────────────
        //
        // Both `start_byte` and `end_byte` must satisfy:
        //   0 ≤ start ≤ end ≤ bytes.len()
        // AND the content must be valid UTF-8 with both positions on char boundaries.
        //
        // Decision: validate UTF-8 on the full file bytes first (a single pass),
        // then check char boundaries with `str::is_char_boundary`. This avoids
        // indexing into potentially invalid UTF-8 with raw byte offsets.
        //
        // Trade-off: if the stored file is not valid UTF-8 (e.g. a binary blob),
        // we return `InvalidRange` with a clear message rather than silently
        // corrupting the file. The spec states replacement is a UTF-8 string
        // and the result must stay valid UTF-8.
        let len = bytes.len();

        if start_byte > end_byte {
            return Err(DomainError::InvalidRange(format!(
                "start ({start_byte}) must be ≤ end ({end_byte})"
            )));
        }
        if end_byte > len {
            return Err(DomainError::InvalidRange(format!(
                "end ({end_byte}) exceeds file length ({len})"
            )));
        }

        // Validate UTF-8 and char boundaries in one step.
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            DomainError::InvalidRange(format!(
                "target file is not UTF-8 text; edit_range only edits text files ({e})"
            ))
        })?;

        if !text.is_char_boundary(start_byte) {
            return Err(DomainError::InvalidRange(format!(
                "start byte {start_byte} is not on a UTF-8 character boundary"
            )));
        }
        if !text.is_char_boundary(end_byte) {
            return Err(DomainError::InvalidRange(format!(
                "end byte {end_byte} is not on a UTF-8 character boundary"
            )));
        }

        // ── Step 3: splice ────────────────────────────────────────────────────
        //
        // Concatenate three segments: prefix | replacement | suffix.
        // All three are valid UTF-8 (prefix/suffix at char boundaries, replacement
        // from JSON string), so the result is guaranteed valid UTF-8.
        let mut spliced = Vec::with_capacity(start_byte + replacement.len() + (len - end_byte));
        spliced.extend_from_slice(&bytes[..start_byte]);
        spliced.extend_from_slice(replacement.as_bytes());
        spliced.extend_from_slice(&bytes[end_byte..]);

        // ── Step 4 & 5: atomic write + commit ────────────────────────────────
        self.commit_indexed_write(path, file_id, spliced)?;

        Ok(TxReport {
            files_changed: 1,
            replacements: 1,
            errors: vec![],
        })
    }

    fn create_file_cas(
        &mut self,
        path: RelativePath,
        content: Vec<u8>,
        expected_version: Option<String>,
    ) -> Result<(), DomainError> {
        if let Some(want) = expected_version {
            // Read the live bytes to compute the current version. A missing file
            // is itself a conflict: the caller expected a specific version but it
            // is gone. We report `actual = ""` — an unambiguous "absent" sentinel
            // (a real SHA-256 hash is always 64 hex chars) — rather than NotFound,
            // so the caller treats it uniformly as "re-read and retry".
            let actual = match self.fs.read(&path) {
                Ok(bytes) => compute_content_version(&bytes),
                Err(PortError::NotFound) => String::new(),
                Err(e) => return Err(port_err_to_domain(e)),
            };
            if actual != want {
                return Err(DomainError::VersionConflict {
                    expected: want,
                    actual,
                });
            }
        }
        self.create_file(path, content)
    }

    fn edit_range_cas(
        &mut self,
        path: &RelativePath,
        start_byte: usize,
        end_byte: usize,
        replacement: &str,
        expected_version: Option<String>,
    ) -> Result<TxReport, DomainError> {
        if let Some(want) = expected_version {
            // Check VFS first so we surface NotFound cleanly (same as edit_range).
            if self.workspace.get_by_path(path).is_none() {
                return Err(DomainError::NotFound);
            }
            let current_bytes = self.fs.read(path).map_err(port_err_to_domain)?;
            let actual = compute_content_version(&current_bytes);
            if actual != want {
                return Err(DomainError::VersionConflict {
                    expected: want,
                    actual,
                });
            }
        }
        self.edit_range(path, start_byte, end_byte, replacement)
    }

    fn apply_edits_cas(
        &mut self,
        request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        let file_id = self
            .workspace
            .get_by_path(&request.path)
            .ok_or(DomainError::NotFound)?;
        let bytes = self.fs.read(&request.path).map_err(port_err_to_domain)?;
        let actual = compute_content_version(&bytes);
        if actual != request.expected_version {
            return Err(DomainError::VersionConflict {
                expected: request.expected_version,
                actual,
            });
        }

        let plan = plan_apply_edits(&request.path, &bytes, request.edits)?;
        let new_version = if plan.applied.is_empty() {
            None
        } else {
            let version = compute_content_version(plan.content.as_bytes());
            self.commit_indexed_write(&request.path, file_id, plan.content.into_bytes())?;
            Some(version)
        };

        Ok(ApplyEditsFileResult {
            path: request.path,
            applied: plan.applied,
            skipped: plan.skipped,
            new_version,
            preview: None,
        })
    }

    fn apply_edits_dry_run(
        &self,
        request: ApplyEditsRequest,
    ) -> Result<ApplyEditsFileResult, DomainError> {
        let bytes = self.fs.read(&request.path).map_err(port_err_to_domain)?;
        let actual = compute_content_version(&bytes);
        if actual != request.expected_version {
            return Err(DomainError::VersionConflict {
                expected: request.expected_version,
                actual,
            });
        }

        let plan = plan_apply_edits(&request.path, &bytes, request.edits)?;
        let preview = ApplyEditsPreview {
            path: request.path.clone(),
            edits: plan.applied.clone(),
            skipped: plan.skipped.clone(),
            preview_content: plan.content,
        };

        Ok(ApplyEditsFileResult {
            path: request.path,
            applied: preview.edits.clone(),
            skipped: preview.skipped.clone(),
            new_version: None,
            preview: Some(preview),
        })
    }

    fn apply_batch_edits(
        &mut self,
        request: WorkspaceApplyEditsRequest,
    ) -> Result<WorkspaceApplyEditsResult, DomainError> {
        if request.edits.is_empty() {
            return Ok(WorkspaceApplyEditsResult {
                files_changed: 0,
                per_file: vec![batch_file_error(
                    RelativePath::new(""),
                    0,
                    WorkspaceApplyEditsError {
                        code: WorkspaceApplyEditsErrorCode::EmptyEdits,
                        message: "empty edit list".to_owned(),
                        path: None,
                    },
                )],
            });
        }

        let dry_run = request.dry_run.unwrap_or(false);
        let mut files_changed = 0;
        let mut per_file = Vec::new();

        for group in group_workspace_edit_spans(request.edits) {
            let file_result = self.apply_batch_edit_group(group, dry_run);
            if !dry_run && file_result.applied {
                files_changed += 1;
            }
            per_file.push(file_result);
        }

        Ok(WorkspaceApplyEditsResult {
            files_changed,
            per_file,
        })
    }

    fn apply_safe_delete_edits(
        &mut self,
        request: SafeDeleteMutationRequest,
    ) -> Result<SafeDeleteMutationResult, DomainError> {
        let groups = group_safe_delete_edits(request.edits);
        let unattempted =
            || safe_delete_outcomes(&groups, SafeDeleteFileOutcomeStatus::Unattempted);

        let mut plans = Vec::with_capacity(groups.len());
        for group in &groups {
            if !is_safe_delete_path(&group.path) {
                return Ok(safe_delete_rejection(
                    SafeDeleteRejectionCode::InvalidPath,
                    unattempted(),
                ));
            }

            let path = RelativePath::new(group.path.clone());
            let Some(file_id) = self.workspace.get_by_path(&path) else {
                return Ok(safe_delete_rejection(
                    SafeDeleteRejectionCode::InvalidPath,
                    unattempted(),
                ));
            };
            let bytes = match self.fs.read(&path) {
                Ok(bytes) => bytes,
                Err(PortError::NotFound) => {
                    return Ok(safe_delete_rejection(
                        SafeDeleteRejectionCode::InvalidPath,
                        unattempted(),
                    ));
                }
                Err(error) => return Err(port_err_to_domain(error)),
            };
            let actual = compute_content_version(&bytes);
            if group.edits.iter().any(|edit| edit.base_hash != actual) {
                return Ok(safe_delete_rejection(
                    SafeDeleteRejectionCode::CasConflict,
                    unattempted(),
                ));
            }

            let edits: Vec<TextEdit> = group
                .edits
                .iter()
                .map(|edit| TextEdit {
                    start_byte: edit.start_byte,
                    end_byte: edit.end_byte,
                    replacement: edit.replacement.clone(),
                })
                .collect();
            if has_overlapping_text_edits(&edits) {
                return Ok(safe_delete_rejection(
                    SafeDeleteRejectionCode::OverlappingEdits,
                    unattempted(),
                ));
            }
            let text = match validate_apply_edits(&path, &bytes, &edits) {
                Ok(text) => text,
                Err(_) => {
                    return Ok(safe_delete_rejection(
                        SafeDeleteRejectionCode::InvalidRange,
                        unattempted(),
                    ));
                }
            };
            let planned = edits
                .into_iter()
                .map(|edit| PlannedTextEdit { edit })
                .collect::<Vec<_>>();
            plans.push(SafeDeleteFilePlan {
                path,
                file_id,
                content: splice_text_edits(text, &planned).into_bytes(),
            });
        }

        if request.dry_run {
            return Ok(SafeDeleteMutationResult {
                status: SafeDeleteMutationStatus::Previewed,
                rejection: None,
                per_file: unattempted(),
            });
        }

        let mut per_file = unattempted();
        for (index, plan) in plans.into_iter().enumerate() {
            if super::atomic_write(self.fs, &plan.path, plan.content).is_err() {
                per_file[index].outcome = SafeDeleteFileOutcomeStatus::Failed;
                per_file[index].error = Some(SafeDeleteFileError::FilesystemError);
                return Ok(safe_delete_partial_commit(per_file));
            }

            let metadata = FileMetadata {
                size: 0,
                modified: Timestamp(0),
                content_hash: None,
            };
            self.workspace
                .update(plan.file_id, metadata)
                .expect("safe-delete preflight retained a live file identity");
            self.index.remove(plan.file_id);
            self.index
                .insert(plan.file_id, &tokenize(plan.path.as_str()));
            let virtual_file = self
                .workspace
                .get(plan.file_id)
                .expect("safe-delete update retained a live file identity")
                .clone();
            if self.storage.put(virtual_file).is_err() {
                per_file[index].outcome = SafeDeleteFileOutcomeStatus::Failed;
                per_file[index].error = Some(SafeDeleteFileError::PersistenceError);
                return Ok(safe_delete_partial_commit(per_file));
            }

            self.extension_host
                .on_file_changed(plan.file_id, &plan.path);
            per_file[index].outcome = SafeDeleteFileOutcomeStatus::Committed;
        }

        Ok(SafeDeleteMutationResult {
            status: SafeDeleteMutationStatus::Applied,
            rejection: None,
            per_file,
        })
    }

    fn global_replace_cas(
        &mut self,
        target: &str,
        replacement: &str,
        expected_versions: HashMap<RelativePath, String>,
    ) -> Result<TxReport, DomainError> {
        if expected_versions.is_empty() {
            return self.global_replace(target, replacement);
        }

        // Phase 1 (under the write-lock): check each guarded path's live version.
        // A drifted hash — or a guarded path we cannot read — is a conflict: it is
        // recorded in the report and excluded from the write phase. Untracked
        // paths are never written by global_replace, so a guard on one is vacuous
        // and is skipped without manufacturing an error.
        let mut cas_errors: Vec<FileReplaceError> = Vec::new();
        let mut conflicting: HashSet<RelativePath> = HashSet::new();
        for (guarded_path, want) in &expected_versions {
            if self.workspace.get_by_path(guarded_path).is_none() {
                continue;
            }
            match self.fs.read(guarded_path) {
                Ok(bytes) => {
                    let actual = compute_content_version(&bytes);
                    if &actual != want {
                        cas_errors.push(FileReplaceError {
                            path: guarded_path.clone(),
                            reason: format!("version conflict: expected {want}, actual {actual}"),
                        });
                        conflicting.insert(guarded_path.clone());
                    }
                }
                Err(e) => {
                    cas_errors.push(FileReplaceError {
                        path: guarded_path.clone(),
                        reason: format!("could not read file for CAS check: {e}"),
                    });
                    conflicting.insert(guarded_path.clone());
                }
            }
        }

        // Phase 2: delegate to the canonical replace engine, excluding conflicts.
        // Reuses its parallel read/apply, atomic shadow-file writes, and batched
        // storage flush rather than re-implementing them here (spec 18 REFACTOR).
        let mut report = GlobalReplaceService::new(self.fs, self.workspace, self.storage)
            .execute_excluding(target, replacement, false, &conflicting)?;

        // Merge the CAS conflicts into the report, preserving deterministic order.
        report.errors.extend(cas_errors);
        report.errors.sort();
        Ok(report)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl<'ws> FileMutationService<'ws> {
    fn apply_batch_edit_group(
        &mut self,
        group: WorkspaceEditGroup,
        dry_run: bool,
    ) -> PerFileEditResult {
        let path = group.path;
        let mut edits = group.edits;
        edits.sort_by(|left, right| {
            right
                .start_byte
                .cmp(&left.start_byte)
                .then_with(|| right.end_byte.cmp(&left.end_byte))
        });

        let bytes = match self.fs.read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return batch_file_error(
                    path,
                    edits.len(),
                    domain_error_to_batch_error(port_err_to_domain(error)),
                );
            }
        };

        let actual = compute_content_version(&bytes);
        if !dry_run && group.base_hashes.len() != edits.len() {
            return batch_file_error(
                path.clone(),
                edits.len(),
                WorkspaceApplyEditsError {
                    code: WorkspaceApplyEditsErrorCode::Conflict,
                    message: "version conflict: base_hash is required for every mutating edit"
                        .to_owned(),
                    path: Some(path),
                },
            );
        }
        if group
            .base_hashes
            .iter()
            .any(|base_hash| base_hash != &actual)
        {
            return batch_file_error(
                path.clone(),
                edits.len(),
                WorkspaceApplyEditsError {
                    code: WorkspaceApplyEditsErrorCode::Conflict,
                    message: format!(
                        "version conflict: one or more supplied base_hash values did not match actual {actual}"
                    ),
                    path: Some(path),
                },
            );
        }
        let expected_version = actual;

        if let Err(error) = validate_apply_edits(&path, &bytes, &edits) {
            return batch_file_error(path, edits.len(), domain_error_to_batch_error(error));
        }

        if has_overlapping_text_edits(&edits) {
            return batch_file_error(
                path.clone(),
                edits.len(),
                WorkspaceApplyEditsError {
                    code: WorkspaceApplyEditsErrorCode::OverlappingSpans,
                    message: "overlapping spans in one file are rejected by batch apply-edits"
                        .to_owned(),
                    path: Some(path),
                },
            );
        }

        let request = ApplyEditsRequest {
            path: path.clone(),
            expected_version,
            edits,
        };
        let file_result = if dry_run {
            self.apply_edits_dry_run(request)
        } else {
            self.apply_edits_cas(request)
        };

        match file_result {
            Ok(result) => batch_file_success(result, dry_run),
            Err(error) => {
                batch_file_error(path, group.edit_count, domain_error_to_batch_error(error))
            }
        }
    }
}

/// Map a [`PortError`] to a [`DomainError`].
///
/// The domain layer must not expose infrastructure error types in its return
/// values; this conversion preserves the cause information in a domain-safe way.
fn port_err_to_domain(err: PortError) -> DomainError {
    match err {
        PortError::NotFound => DomainError::NotFound,
        PortError::ReconciliationFailed { reason, .. }
        | PortError::WriteFailed(reason)
        | PortError::ReadFailed(reason)
        | PortError::InvalidArgs(reason) => DomainError::IoError(reason),
    }
}

struct WorkspaceEditGroup {
    path: RelativePath,
    base_hashes: Vec<String>,
    edits: Vec<TextEdit>,
    edit_count: usize,
}

struct SafeDeleteEditGroup {
    path: String,
    edits: Vec<extension_protocol::messages::SafeDeleteCleanupEdit>,
}

struct SafeDeleteFilePlan {
    path: RelativePath,
    file_id: crate::domain::FileId,
    content: Vec<u8>,
}

fn group_safe_delete_edits(
    edits: Vec<extension_protocol::messages::SafeDeleteCleanupEdit>,
) -> Vec<SafeDeleteEditGroup> {
    let mut groups: Vec<SafeDeleteEditGroup> = Vec::new();
    for edit in edits {
        if let Some(group) = groups.iter_mut().find(|group| group.path == edit.path) {
            group.edits.push(edit);
        } else {
            groups.push(SafeDeleteEditGroup {
                path: edit.path.clone(),
                edits: vec![edit],
            });
        }
    }
    groups
}

fn safe_delete_outcomes(
    groups: &[SafeDeleteEditGroup],
    outcome: SafeDeleteFileOutcomeStatus,
) -> Vec<SafeDeleteFileOutcome> {
    groups
        .iter()
        .map(|group| SafeDeleteFileOutcome {
            path: group.path.clone(),
            outcome,
            error: None,
        })
        .collect()
}

fn safe_delete_rejection(
    rejection: SafeDeleteRejectionCode,
    per_file: Vec<SafeDeleteFileOutcome>,
) -> SafeDeleteMutationResult {
    SafeDeleteMutationResult {
        status: SafeDeleteMutationStatus::Rejected,
        rejection: Some(rejection),
        per_file,
    }
}

fn safe_delete_partial_commit(per_file: Vec<SafeDeleteFileOutcome>) -> SafeDeleteMutationResult {
    SafeDeleteMutationResult {
        status: SafeDeleteMutationStatus::PartialCommit,
        rejection: None,
        per_file,
    }
}

fn group_workspace_edit_spans(spans: Vec<WorkspaceEditSpan>) -> Vec<WorkspaceEditGroup> {
    let mut groups: Vec<WorkspaceEditGroup> = Vec::new();

    for span in spans {
        let edit = TextEdit {
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            replacement: span.replacement,
        };

        if let Some(group) = groups.iter_mut().find(|group| group.path == span.path) {
            if let Some(base_hash) = span.base_hash {
                group.base_hashes.push(base_hash);
            }
            group.edits.push(edit);
            group.edit_count += 1;
        } else {
            let base_hashes = span.base_hash.into_iter().collect();
            groups.push(WorkspaceEditGroup {
                path: span.path,
                base_hashes,
                edits: vec![edit],
                edit_count: 1,
            });
        }
    }

    groups
}

fn has_overlapping_text_edits(edits: &[TextEdit]) -> bool {
    for (index, left) in edits.iter().enumerate() {
        if edits
            .iter()
            .skip(index + 1)
            .any(|right| text_edits_conflict(left, right))
        {
            return true;
        }
    }

    false
}

fn batch_file_success(result: ApplyEditsFileResult, dry_run: bool) -> PerFileEditResult {
    let preview = result.preview.map(|preview| preview.preview_content);
    let applied = !dry_run && result.new_version.is_some();
    PerFileEditResult {
        path: result.path,
        applied,
        edits_applied: result.applied.len(),
        edits_skipped: result.skipped.len(),
        new_version: result.new_version,
        preview,
        error: None,
    }
}

fn batch_file_error(
    path: RelativePath,
    edit_count: usize,
    error: WorkspaceApplyEditsError,
) -> PerFileEditResult {
    PerFileEditResult {
        path,
        applied: false,
        edits_applied: 0,
        edits_skipped: edit_count,
        new_version: None,
        preview: None,
        error: Some(error),
    }
}

fn domain_error_to_batch_error(error: DomainError) -> WorkspaceApplyEditsError {
    let code = match error {
        DomainError::InvalidRange(_) => WorkspaceApplyEditsErrorCode::InvalidRange,
        DomainError::VersionConflict { .. } => WorkspaceApplyEditsErrorCode::Conflict,
        DomainError::NotFound | DomainError::NotADirectory(_) => {
            WorkspaceApplyEditsErrorCode::InvalidPath
        }
        DomainError::UnsupportedOperation(_) => WorkspaceApplyEditsErrorCode::Unsupported,
        DomainError::StaleHandle | DomainError::DuplicatePath | DomainError::IoError(_) => {
            WorkspaceApplyEditsErrorCode::Internal
        }
    };

    WorkspaceApplyEditsError {
        code,
        message: error.to_string(),
        path: None,
    }
}

struct ApplyEditsPlan {
    applied: Vec<TextEdit>,
    skipped: Vec<SkippedEdit>,
    content: String,
}

#[derive(Clone)]
struct PlannedTextEdit {
    edit: TextEdit,
}

fn plan_apply_edits(
    path: &RelativePath,
    bytes: &[u8],
    edits: Vec<TextEdit>,
) -> Result<ApplyEditsPlan, DomainError> {
    let text = validate_apply_edits(path, bytes, &edits)?;
    let (planned, skipped) = filter_conflicting_edits(edits);
    let content = splice_text_edits(text, &planned);
    let applied = planned.into_iter().map(|planned| planned.edit).collect();

    Ok(ApplyEditsPlan {
        applied,
        skipped,
        content,
    })
}

fn validate_apply_edits<'a>(
    path: &RelativePath,
    bytes: &'a [u8],
    edits: &[TextEdit],
) -> Result<&'a str, DomainError> {
    let len = bytes.len();
    let text = std::str::from_utf8(bytes).map_err(|e| {
        DomainError::InvalidRange(format!(
            "{} is not UTF-8 text; apply_edits only edits text files ({e})",
            path.as_str()
        ))
    })?;

    for edit in edits {
        if edit.start_byte > edit.end_byte {
            return Err(DomainError::InvalidRange(format!(
                "{}: start ({}) must be ≤ end ({})",
                path.as_str(),
                edit.start_byte,
                edit.end_byte
            )));
        }
        if edit.end_byte > len {
            return Err(DomainError::InvalidRange(format!(
                "{}: end ({}) exceeds file length ({len})",
                path.as_str(),
                edit.end_byte
            )));
        }
        if !text.is_char_boundary(edit.start_byte) {
            return Err(DomainError::InvalidRange(format!(
                "{}: start byte {} is not on a UTF-8 character boundary",
                path.as_str(),
                edit.start_byte
            )));
        }
        if !text.is_char_boundary(edit.end_byte) {
            return Err(DomainError::InvalidRange(format!(
                "{}: end byte {} is not on a UTF-8 character boundary",
                path.as_str(),
                edit.end_byte
            )));
        }
    }

    Ok(text)
}

fn filter_conflicting_edits(edits: Vec<TextEdit>) -> (Vec<PlannedTextEdit>, Vec<SkippedEdit>) {
    let mut indexed: Vec<(usize, TextEdit)> = edits.into_iter().enumerate().collect();
    indexed.sort_by(|(left_index, left), (right_index, right)| {
        left.start_byte
            .cmp(&right.start_byte)
            .then_with(|| left.end_byte.cmp(&right.end_byte))
            .then_with(|| left_index.cmp(right_index))
    });

    let mut applied_indexed: Vec<(usize, PlannedTextEdit)> = Vec::new();
    let mut skipped_indexed: Vec<(usize, SkippedEdit)> = Vec::new();
    let mut component: Vec<(usize, TextEdit)> = Vec::new();
    let mut component_end: Option<usize> = None;

    for (index, edit) in indexed {
        match component_end {
            None => {
                component_end = Some(edit.end_byte);
                component.push((index, edit));
            }
            Some(end) if edit.start_byte <= end => {
                component_end = Some(end.max(edit.end_byte));
                component.push((index, edit));
            }
            Some(_) => {
                flush_conflict_component(
                    &mut component,
                    &mut applied_indexed,
                    &mut skipped_indexed,
                );
                component_end = Some(edit.end_byte);
                component.push((index, edit));
            }
        }
    }
    flush_conflict_component(&mut component, &mut applied_indexed, &mut skipped_indexed);

    applied_indexed.sort_by(|(left_index, left), (right_index, right)| {
        left.edit
            .start_byte
            .cmp(&right.edit.start_byte)
            .then_with(|| left.edit.end_byte.cmp(&right.edit.end_byte))
            .then_with(|| left_index.cmp(right_index))
    });
    skipped_indexed.sort_by(|(left_index, left), (right_index, right)| {
        left.edit
            .start_byte
            .cmp(&right.edit.start_byte)
            .then_with(|| left.edit.end_byte.cmp(&right.edit.end_byte))
            .then_with(|| left_index.cmp(right_index))
    });

    (
        applied_indexed.into_iter().map(|(_, edit)| edit).collect(),
        skipped_indexed
            .into_iter()
            .map(|(_, skipped)| skipped)
            .collect(),
    )
}

fn flush_conflict_component(
    component: &mut Vec<(usize, TextEdit)>,
    applied: &mut Vec<(usize, PlannedTextEdit)>,
    skipped: &mut Vec<(usize, SkippedEdit)>,
) {
    if component.is_empty() {
        return;
    }

    let mut accepted: Vec<(usize, TextEdit)> = Vec::new();
    let mut rejected: Vec<(usize, SkippedEdit)> = Vec::new();
    for (index, edit) in component.drain(..) {
        if accepted
            .iter()
            .all(|(_, accepted_edit)| !text_edits_conflict(accepted_edit, &edit))
        {
            accepted.push((index, edit));
        } else {
            rejected.push((
                index,
                SkippedEdit {
                    edit,
                    reason: SkippedEditReason::Conflict,
                },
            ));
        }
    }

    if !rejected.is_empty()
        && accepted.len() < 2
        && !same_position_empty_insertions(&accepted, &rejected)
    {
        skipped.extend(accepted.into_iter().map(|(index, edit)| {
            (
                index,
                SkippedEdit {
                    edit,
                    reason: SkippedEditReason::Conflict,
                },
            )
        }));
        skipped.extend(rejected);
    } else {
        applied.extend(
            accepted
                .into_iter()
                .map(|(index, edit)| (index, PlannedTextEdit { edit })),
        );
        skipped.extend(rejected);
    }
}

fn text_edits_conflict(left: &TextEdit, right: &TextEdit) -> bool {
    if left.start_byte == left.end_byte && right.start_byte == right.end_byte {
        return left.start_byte == right.start_byte;
    }

    left.start_byte < right.end_byte && right.start_byte < left.end_byte
}

fn same_position_empty_insertions(
    accepted: &[(usize, TextEdit)],
    rejected: &[(usize, SkippedEdit)],
) -> bool {
    let Some((_, accepted_edit)) = accepted.first() else {
        return false;
    };
    let is_empty_at = |edit: &TextEdit, byte| edit.start_byte == byte && edit.end_byte == byte;
    let byte = accepted_edit.start_byte;

    is_empty_at(accepted_edit, byte)
        && rejected
            .iter()
            .all(|(_, skipped)| is_empty_at(&skipped.edit, byte))
}

fn splice_text_edits(text: &str, applied: &[PlannedTextEdit]) -> String {
    let mut spliced = text.to_owned();
    let mut descending = applied.to_vec();
    descending.sort_by(|left, right| {
        right
            .edit
            .start_byte
            .cmp(&left.edit.start_byte)
            .then_with(|| right.edit.end_byte.cmp(&left.edit.end_byte))
    });

    for planned in descending {
        spliced.replace_range(
            planned.edit.start_byte..planned.edit.end_byte,
            &planned.edit.replacement,
        );
    }

    spliced
}
