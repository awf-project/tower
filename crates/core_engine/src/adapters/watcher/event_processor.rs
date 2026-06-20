//! `EventProcessor` — the testable core of the watcher adapter (spec 06).
//!
//! # Architecture
//!
//! ```text
//!  WatchEvent (injectable) ──channel──► EventProcessor::process_event
//!                                              │
//!         Filter (parity with scan walker):    │
//!           tmp-artifact? → drop                │
//!           is root .towerignore? → rebuild matcher, drop
//!           hidden (any '.'-component)? → drop  │
//!           ignored (root .towerignore)? → drop │
//!                                              │
//!               ┌───────────────────────────────┼────────────────────────┐
//!               │ Create  → ws.insert + idx.insert + storage.put         │
//!               │ Modify  → ws.update (in-place) + delta reindex + put   │
//!               │ Delete  → ws.remove + idx.remove + storage.delete      │
//!               │ Rename  → single-lock (remove+insert) + storage ops    │
//!               └───────────────────────────────┬────────────────────────┘
//!                                               ▼ broadcast on_file_changed
//!               Shared state: Arc<RwLock<ProjectWorkspace>>
//!                             Arc<RwLock<InvertedIndex>>
//!               Lock order: workspace first, then index (anti-deadlock)
//! ```
//!
//! # Testability
//!
//! `EventProcessor` is constructed with injected `Arc<RwLock<_>>` state and a
//! `Box<dyn StoragePort>` so tests can drive it deterministically with synthetic
//! `WatchEvent` values, no wall-clock sleeps, and no OS notify subscription.
//!
//! # Lock acquisition order
//!
//! To prevent deadlock the processor ALWAYS acquires locks in this order:
//! 1. `workspace` write lock
//! 2. `index` write lock
//!
//! Read-only callers (MCP find/search) take read locks in the same order.
//! This total order ensures no two threads can be waiting on each other.
//!
//! # Critical section minimality (ST1)
//!
//! Write locks are held for the **domain mutation only** (workspace + index
//! update). The storage `put`/`delete` call — which may do I/O — happens
//! *outside* the lock so readers are not blocked during disk I/O.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::adapters::fs::scan::TOWERIGNORE_FILE_NAME;
use crate::domain::index::InvertedIndex;
use crate::domain::mutation::is_tmp_artifact;
use crate::domain::token::tokenize;
use crate::domain::virtual_file::{FileMetadata, RelativePath, Timestamp};
use crate::domain::workspace::ProjectWorkspace;
use crate::ports::{DocumentSyncPort, ExtensionHostPort, NoOpDocumentSync, StoragePort};

// ── WatchEvent ────────────────────────────────────────────────────────────────

/// An event that the watcher adapter emits after debouncing OS filesystem
/// events.
///
/// This is the canonical shape that crosses the producer/consumer boundary.
/// Tests inject these directly into [`EventProcessor::process_event`] without
/// any OS machinery (spec NOTE: injectable channel, no wall-clock sleeps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A new file appeared at `path` (absolute).
    Create(PathBuf),
    /// An existing file's content or metadata changed (absolute path).
    Modify(PathBuf),
    /// A file was removed (absolute path).
    Delete(PathBuf),
    /// A file was moved from `from` to `to` (both absolute).
    Rename { from: PathBuf, to: PathBuf },
}

// ── EventProcessor ────────────────────────────────────────────────────────────

/// Processes [`WatchEvent`]s, keeping workspace + index + storage in sync.
///
/// Constructed once and shared with the watcher thread. Tests drive it
/// directly by calling [`EventProcessor::process_event`] with synthetic events.
pub struct EventProcessor {
    /// Absolute workspace root; events outside it are silently ignored (UN1/AC5).
    root: PathBuf,
    workspace: Arc<RwLock<ProjectWorkspace>>,
    index: Arc<RwLock<InvertedIndex>>,
    storage: Box<dyn StoragePort + Send>,
    extension_host: Box<dyn ExtensionHostPort + Send + Sync>,
    /// Compiled matcher for the ROOT `.towerignore` (the canonical location
    /// `tower init` writes). Rebuilt whenever an event touches that file so
    /// subsequent events honor the latest rules. Falls back to
    /// [`Gitignore::empty`] when the file is absent or fails to parse.
    ignore_matcher: Gitignore,
    /// Mirrors file lifecycle into a resident language server (spec 14b).
    /// Defaults to [`NoOpDocumentSync`] when no server is configured.
    doc_sync: Arc<dyn DocumentSyncPort + Send + Sync>,
    /// Formatter echo set (spec 13a), shared with the `FormatQueue`. A Modify
    /// for a path present here is a formatter-induced write — its `didChange`
    /// is suppressed (UN1) so formatter output never churns diagnostics. Peeked
    /// here (non-consuming); the plugin host's `fan_out` consumes the entry.
    echo_set: Arc<Mutex<HashMap<String, ()>>>,
}

impl std::fmt::Debug for EventProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventProcessor")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl EventProcessor {
    /// Create a new processor.
    ///
    /// # Arguments
    ///
    /// - `root`         — absolute workspace root directory.
    /// - `workspace`    — shared workspace aggregate.
    /// - `index`        — shared inverted index.
    /// - `storage`      — outbound storage port (sled or in-memory fake).
    /// - `extension_host` — lifecycle hook receiver; use `NoOpExtensionHost` when
    ///   none is registered.
    pub fn new(
        root: PathBuf,
        workspace: Arc<RwLock<ProjectWorkspace>>,
        index: Arc<RwLock<InvertedIndex>>,
        storage: Box<dyn StoragePort + Send>,
        extension_host: Box<dyn ExtensionHostPort + Send + Sync>,
    ) -> Self {
        let ignore_matcher = build_ignore_matcher(&root);
        Self {
            root,
            workspace,
            index,
            storage,
            extension_host,
            ignore_matcher,
            doc_sync: Arc::new(NoOpDocumentSync),
            echo_set: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attach a language-server document-sync sink and the shared formatter
    /// `echo_set` (spec 14b). Without this the processor uses
    /// [`NoOpDocumentSync`] and an empty echo set (no LSP sync, no suppression).
    #[must_use]
    pub fn with_document_sync(
        mut self,
        doc_sync: Arc<dyn DocumentSyncPort + Send + Sync>,
        echo_set: Arc<Mutex<HashMap<String, ()>>>,
    ) -> Self {
        self.doc_sync = doc_sync;
        self.echo_set = echo_set;
        self
    }

    /// `true` if `rel_path` is a formatter-induced write (present in the shared
    /// `echo_set`). Non-consuming peek — the plugin host's `fan_out` removes the
    /// entry, so the peek must precede that call within an event (UN1 ordering).
    fn is_echo(&self, rel_path: &RelativePath) -> bool {
        self.echo_set
            .lock()
            .map(|set| set.contains_key(rel_path.as_str()))
            .unwrap_or(false)
    }

    /// Process a single [`WatchEvent`].
    ///
    /// - Paths outside `root` are silently ignored (UN1/AC5).
    /// - Lock acquisition order: workspace → index (anti-deadlock invariant).
    /// - Storage I/O occurs **outside** the write lock (ST1 — short critical
    ///   sections).
    /// - `on_file_changed` is broadcast **after** the VFS update commits (OP1).
    ///
    /// Errors from storage are returned but do not corrupt the in-memory state
    /// (the domain mutation has already committed).
    pub fn process_event(&mut self, event: WatchEvent) -> Result<(), crate::ports::PortError> {
        match event {
            WatchEvent::Create(path) => self.handle_create(path),
            WatchEvent::Modify(path) => self.handle_modify(path),
            WatchEvent::Delete(path) => self.handle_delete(path),
            WatchEvent::Rename { from, to } => self.handle_rename(from, to),
        }
    }

    // ── Event handlers ────────────────────────────────────────────────────────

    /// EV1: Create → insert VirtualFile + index + persist.
    fn handle_create(&mut self, abs_path: PathBuf) -> Result<(), crate::ports::PortError> {
        let Some(rel_path) = self.to_relative(&abs_path) else {
            // UN1/AC5: outside root — ignore.
            return Ok(());
        };

        // UN2/AC5: never index shadow-file temp artifacts as real user files.
        if is_tmp_artifact(&rel_path) {
            return Ok(());
        }

        // An event on the root .towerignore rebuilds the matcher so subsequent
        // events honor the new rules. The file itself is hidden — never indexed.
        if Self::is_root_towerignore(&rel_path) {
            self.rebuild_ignore_matcher();
            return Ok(());
        }

        // Parity with the scan walker: skip hidden paths and .towerignore matches.
        if self.is_excluded(&rel_path) {
            return Ok(());
        }

        let metadata = read_metadata(&abs_path);
        let tokens = tokenize(rel_path.as_str());

        // ── Write lock: minimal critical section ──────────────────────────────
        // Acquire workspace lock first, then index lock (anti-deadlock order).
        // If the path is already tracked (e.g. duplicate event), skip silently.
        let (file_id, virtual_file) = {
            let mut ws = self
                .workspace
                .write()
                .expect("workspace lock poisoned — unrecoverable");

            // Idempotency: if already tracked, do not re-insert.
            if ws.get_by_path(&rel_path).is_some() {
                return Ok(());
            }

            let file_id = match ws.insert(rel_path.clone(), metadata) {
                Ok(id) => id,
                Err(crate::domain::DomainError::DuplicatePath) => {
                    // Concurrent event already inserted it.
                    return Ok(());
                }
                Err(e) => {
                    return Err(crate::ports::PortError::WriteFailed(e.to_string()));
                }
            };

            let virtual_file = ws
                .get(file_id)
                .expect("just inserted — slot must be live")
                .clone();

            // Acquire index lock while still holding workspace write lock.
            // Order: workspace → index (total order invariant).
            {
                let mut idx = self
                    .index
                    .write()
                    .expect("index lock poisoned — unrecoverable");
                idx.insert(file_id, &tokens);
            }

            (file_id, virtual_file)
        }; // workspace write lock released here

        // Storage I/O outside the lock (ST1).
        self.storage.put(virtual_file)?;

        // 14b EV1: open the document on the resident language server.
        if self.doc_sync.serves(&rel_path)
            && let Ok(text) = std::fs::read_to_string(&abs_path)
        {
            self.doc_sync.document_opened(&rel_path, &text);
        }

        // OP1: broadcast after VFS update commits.
        self.extension_host.on_file_changed(file_id, &rel_path);

        Ok(())
    }

    /// EV2: Modify → update metadata/hash in place, preserving the `FileId`.
    ///
    /// Uses `ProjectWorkspace::update` to mutate the existing slot rather than
    /// remove + re-insert, so existing `FileId` handles remain valid after a
    /// Modify event (generational arena stability invariant).
    ///
    /// The index delta-reindex is still performed (remove old id, re-insert same
    /// id) to keep the index consistent; because the id is unchanged, this is
    /// effectively a no-op at the posting-list level but handles any future case
    /// where token computation could change.
    fn handle_modify(&mut self, abs_path: PathBuf) -> Result<(), crate::ports::PortError> {
        let Some(rel_path) = self.to_relative(&abs_path) else {
            return Ok(());
        };

        // UN2/AC5: never process shadow-file temp artifacts.
        if is_tmp_artifact(&rel_path) {
            return Ok(());
        }

        // An event on the root .towerignore rebuilds the matcher so subsequent
        // events honor the new rules. The file itself is hidden — never indexed.
        if Self::is_root_towerignore(&rel_path) {
            self.rebuild_ignore_matcher();
            return Ok(());
        }

        // Parity with the scan walker: a path that is hidden or now matches an
        // ignore rule must not be indexed. If it was previously tracked (e.g. a
        // later .towerignore edit now covers it), remove it instead of updating.
        if self.is_excluded(&rel_path) {
            return self.handle_delete(abs_path);
        }

        let metadata = read_metadata(&abs_path);
        let new_tokens = tokenize(rel_path.as_str());

        // ── Write lock ────────────────────────────────────────────────────────
        let (file_id, virtual_file) = {
            let mut ws = self
                .workspace
                .write()
                .expect("workspace lock poisoned — unrecoverable");

            let Some(file_id) = ws.get_by_path(&rel_path) else {
                // Not tracked — treat as a late Create.
                drop(ws);
                return self.handle_create(abs_path);
            };

            // Update metadata in-place; FileId and path are preserved.
            ws.update(file_id, metadata)
                .expect("get_by_path returned a valid id — slot must be live");

            let virtual_file = ws
                .get(file_id)
                .expect("just updated — slot is live")
                .clone();

            // Delta reindex with the same file_id so posting lists stay coherent.
            {
                let mut idx = self
                    .index
                    .write()
                    .expect("index lock poisoned — unrecoverable");
                idx.remove(file_id);
                idx.insert(file_id, &new_tokens);
            }

            (file_id, virtual_file)
        };

        // Storage I/O outside the lock (ST1).
        self.storage.put(virtual_file)?;

        // 14b EV1/UN1: push the new content to the language server, unless this
        // Modify is a formatter echo. The echo peek precedes `on_file_changed`
        // below (which consumes the echo entry for the fmt plugin).
        if self.doc_sync.serves(&rel_path)
            && !self.is_echo(&rel_path)
            && let Ok(text) = std::fs::read_to_string(&abs_path)
        {
            self.doc_sync.document_changed(&rel_path, &text);
        }

        // OP1: broadcast after VFS update commits.
        self.extension_host.on_file_changed(file_id, &rel_path);

        Ok(())
    }

    /// EV3: Delete → remove from workspace + index + persist, free FileId.
    fn handle_delete(&mut self, abs_path: PathBuf) -> Result<(), crate::ports::PortError> {
        let Some(rel_path) = self.to_relative(&abs_path) else {
            return Ok(());
        };

        // Deleting the root .towerignore rebuilds the matcher (it now matches
        // nothing). The file is hidden, so it was never tracked — nothing to
        // remove from the VFS.
        if Self::is_root_towerignore(&rel_path) {
            self.rebuild_ignore_matcher();
            return Ok(());
        }

        // ── Write lock ────────────────────────────────────────────────────────
        // `rel_path` is cloned before the lock is taken so it is available for
        // the post-lock extension_host call (OP1) without holding the lock during
        // the notification (ST1 — short critical sections).
        let file_id = {
            let mut ws = self
                .workspace
                .write()
                .expect("workspace lock poisoned — unrecoverable");

            let Some(file_id) = ws.get_by_path(&rel_path) else {
                // Already removed or never tracked — idempotent no-op.
                return Ok(());
            };

            ws.remove(file_id).expect("get_by_path returned a valid id");

            {
                let mut idx = self
                    .index
                    .write()
                    .expect("index lock poisoned — unrecoverable");
                idx.remove(file_id);
            }

            file_id
        };

        // Storage I/O outside the lock (ST1).
        // Ignore NotFound from storage — the file may never have been persisted
        // (e.g. it was created and deleted between flush cycles).
        match self.storage.delete(file_id) {
            Ok(()) | Err(crate::ports::PortError::NotFound) => {}
            Err(e) => return Err(e),
        }

        // 14b: close the document on the language server.
        if self.doc_sync.serves(&rel_path) {
            self.doc_sync.document_closed(&rel_path);
        }

        // OP1: broadcast after VFS update commits (delete side).
        self.extension_host.on_file_changed(file_id, &rel_path);

        Ok(())
    }

    /// EV4/AC3: Rename → remap path while preserving path↔FileId bijection.
    ///
    /// The delete of `from` and the insert of `to` are executed inside a
    /// **single** write-lock critical section so concurrent readers never observe
    /// a state where neither path exists (atomicity gap). After the lock is
    /// released two storage operations (delete old, put new) happen outside the
    /// lock (ST1 — short critical sections). Two `on_file_changed` calls follow:
    /// one for the deleted path and one for the created path (OP1).
    ///
    /// # Tmp-artifact guard (UN2/AC5)
    ///
    /// If `to` resolves to a `.tmp_write` or `.~tmp` path the destination is a
    /// temp artifact and must never be indexed. The rename is treated as a plain
    /// delete of the source (mirroring the `RealFs::write` internal `.~tmp →
    /// <dst>` pattern in reverse). This prevents external agents that rename a
    /// real file to a tmp-suffixed name from polluting the VFS.
    fn handle_rename(&mut self, from: PathBuf, to: PathBuf) -> Result<(), crate::ports::PortError> {
        let from_rel = self.to_relative(&from);
        let to_rel = self.to_relative(&to);

        // If the source is the root .towerignore, its rules are moving away —
        // rebuild the matcher from disk so subsequent events stop honoring the
        // removed file. Destination handling below still applies to `to`.
        if from_rel.as_ref().is_some_and(Self::is_root_towerignore) {
            self.rebuild_ignore_matcher();
        }

        // If the destination is outside the root, fall back to a plain delete.
        let Some(to_rel_path) = to_rel else {
            if from_rel.is_some() {
                return self.handle_delete(from);
            }
            return Ok(());
        };

        // UN2/AC5: if the destination is a tmp artifact, never insert it into
        // the VFS. Treat it as a delete of the source (the real file moved away).
        if is_tmp_artifact(&to_rel_path) {
            if from_rel.is_some() {
                return self.handle_delete(from);
            }
            return Ok(());
        }

        // If the destination is the root .towerignore, rebuild the matcher and
        // treat the source as moved away (delete it if it was tracked). The
        // .towerignore file itself is hidden and never indexed.
        if Self::is_root_towerignore(&to_rel_path) {
            self.rebuild_ignore_matcher();
            if from_rel.is_some() {
                return self.handle_delete(from);
            }
            return Ok(());
        }

        // Parity with the scan walker: if the destination is hidden or matches an
        // ignore rule, never insert it. Treat it as a delete of the source.
        if self.is_excluded(&to_rel_path) {
            if from_rel.is_some() {
                return self.handle_delete(from);
            }
            return Ok(());
        }

        // If the source is outside the root, treat as a plain create.
        let Some(from_rel_path) = from_rel else {
            return self.handle_create(to);
        };

        let to_metadata = read_metadata(&to);
        let to_tokens = tokenize(to_rel_path.as_str());

        // ── Single write-lock critical section ────────────────────────────────
        // Both the remove(from) and insert(to) happen atomically from the
        // perspective of concurrent readers. No gap where neither path exists.
        let (from_file_id, to_file_id, to_virtual_file) = {
            let mut ws = self
                .workspace
                .write()
                .expect("workspace lock poisoned — unrecoverable");

            // Remove the source entry (if tracked).
            let from_file_id = ws.get_by_path(&from_rel_path);
            if let Some(fid) = from_file_id {
                ws.remove(fid).expect("get_by_path returned a valid id");
            }

            // Insert the destination entry (idempotent if already present).
            let to_file_id = match ws.insert(to_rel_path.clone(), to_metadata) {
                Ok(id) => id,
                Err(crate::domain::DomainError::DuplicatePath) => {
                    // Destination already tracked — treat as a no-op for insert.
                    ws.get_by_path(&to_rel_path)
                        .expect("DuplicatePath implies get_by_path succeeds")
                }
                Err(e) => return Err(crate::ports::PortError::WriteFailed(e.to_string())),
            };

            let to_virtual_file = ws
                .get(to_file_id)
                .expect("just inserted — slot must be live")
                .clone();

            // Update the index under the same lock scope.
            {
                let mut idx = self
                    .index
                    .write()
                    .expect("index lock poisoned — unrecoverable");
                if let Some(fid) = from_file_id {
                    idx.remove(fid);
                }
                idx.insert(to_file_id, &to_tokens);
            }

            (from_file_id, to_file_id, to_virtual_file)
        }; // both locks released here — no gap visible to readers

        // Storage I/O outside the lock (ST1).
        if let Some(fid) = from_file_id {
            match self.storage.delete(fid) {
                Ok(()) | Err(crate::ports::PortError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        self.storage.put(to_virtual_file)?;

        // 14b: mirror the rename to the language server — close the source
        // document, open the destination with its current content.
        if self.doc_sync.serves(&from_rel_path) {
            self.doc_sync.document_closed(&from_rel_path);
        }
        if self.doc_sync.serves(&to_rel_path)
            && let Ok(text) = std::fs::read_to_string(&to)
        {
            self.doc_sync.document_opened(&to_rel_path, &text);
        }

        // OP1: broadcast after VFS update commits.
        if let Some(fid) = from_file_id {
            self.extension_host.on_file_changed(fid, &from_rel_path);
        }
        self.extension_host
            .on_file_changed(to_file_id, &to_rel_path);

        Ok(())
    }

    // ── Ignore / hidden filtering (parity with the scan walker) ────────────────

    /// `true` if `rel_path` is the root `.towerignore` file itself.
    fn is_root_towerignore(rel_path: &RelativePath) -> bool {
        rel_path.as_str() == TOWERIGNORE_FILE_NAME
    }

    /// Rebuild the ignore matcher from the current on-disk root `.towerignore`.
    fn rebuild_ignore_matcher(&mut self) {
        self.ignore_matcher = build_ignore_matcher(&self.root);
    }

    /// `true` if any component of `rel_path` starts with '.', mirroring the
    /// scan walker's `hidden(true)`. Covers `.git/`, dotfiles, `.env`/`.env.*`
    /// and the `.towerignore` file itself.
    fn is_hidden(rel_path: &RelativePath) -> bool {
        rel_path
            .as_str()
            .split('/')
            .any(|component| component.starts_with('.'))
    }

    /// `true` if `rel_path` is excluded by the root `.towerignore`, honoring
    /// `!` negation. A file under an ignored directory is caught via
    /// `matched_path_or_any_parents`. Un-anchored globs (e.g. `*.key`) match at
    /// any depth, which is why a root-only matcher closes the secret hole.
    fn is_ignored(&self, rel_path: &RelativePath) -> bool {
        match self
            .ignore_matcher
            .matched_path_or_any_parents(rel_path.as_str(), false)
        {
            Match::Ignore(_) => true,
            Match::Whitelist(_) | Match::None => false,
        }
    }

    /// `true` if `rel_path` must be excluded from the index (hidden OR ignored).
    fn is_excluded(&self, rel_path: &RelativePath) -> bool {
        Self::is_hidden(rel_path) || self.is_ignored(rel_path)
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Convert an absolute path to a workspace-relative [`RelativePath`].
    ///
    /// Returns `None` if the path is outside the workspace root (UN1/AC5).
    fn to_relative(&self, abs: &Path) -> Option<RelativePath> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let rel_str = rel.to_str()?;
        // Normalise path separators to forward-slash on all platforms.
        #[cfg(not(windows))]
        let normalised: &str = rel_str;
        #[cfg(windows)]
        let normalised: String = rel_str.replace('\\', "/");
        if normalised.is_empty() {
            return None;
        }
        Some(RelativePath::new(normalised))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a [`Gitignore`] matcher from the ROOT `.towerignore` only — the
/// canonical location `tower init` writes.
///
/// Falls back to [`Gitignore::empty`] when the file is absent or fails to
/// parse, so a malformed ignore file never causes the watcher to index nothing
/// (or to panic). Nested per-directory `.towerignore` files are intentionally
/// out of scope (the un-anchored-glob behaviour makes a root-only matcher
/// sufficient to close the secret hole).
fn build_ignore_matcher(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    // `add` returns Some(err) on a parse error; the file simply contributes no
    // globs in that case. A None return means success (or absent file).
    let _ = builder.add(root.join(TOWERIGNORE_FILE_NAME));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Read filesystem metadata for an absolute path.
///
/// Falls back to default metadata (size=0, mtime=0) on any I/O error so that
/// a race between the OS event and the metadata read never crashes the watcher.
fn read_metadata(abs: &Path) -> FileMetadata {
    match std::fs::metadata(abs) {
        Ok(m) => {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| Timestamp(d.as_secs()))
                .unwrap_or(Timestamp(0));
            FileMetadata {
                size: m.len(),
                modified,
                content_hash: None,
            }
        }
        Err(_) => FileMetadata::default(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use super::*;
    use crate::adapters::in_memory_storage::InMemoryStorage;
    use crate::domain::index::InvertedIndex;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::ports::NoOpExtensionHost;

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// An `ExtensionHostPort` that records calls to `on_file_changed`.
    #[derive(Debug, Default)]
    struct RecordingExtensionHost {
        calls: std::sync::Mutex<Vec<(crate::domain::FileId, RelativePath)>>,
    }

    impl ExtensionHostPort for RecordingExtensionHost {
        fn on_file_indexed(&self, _id: crate::domain::FileId, _path: &RelativePath) {}
        fn on_file_changed(&self, id: crate::domain::FileId, path: &RelativePath) {
            self.calls.lock().unwrap().push((id, path.clone()));
        }
        fn on_file_deleted(&self, _path: &RelativePath) {}
        fn declared_tools(
            &self,
        ) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
            Vec::new()
        }
        fn invoke(
            &self,
            tool_name: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, crate::domain::InvokeError> {
            Err(crate::domain::InvokeError::ToolNotFound(
                tool_name.to_owned(),
            ))
        }
    }

    fn make_processor_with_extension_host(
        root: PathBuf,
        host: Box<dyn ExtensionHostPort + Send + Sync>,
    ) -> (
        EventProcessor,
        Arc<RwLock<ProjectWorkspace>>,
        Arc<RwLock<InvertedIndex>>,
    ) {
        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        let storage = Box::new(InMemoryStorage::new());
        let processor = EventProcessor::new(
            root,
            Arc::clone(&workspace),
            Arc::clone(&index),
            storage,
            host,
        );
        (processor, workspace, index)
    }

    fn make_processor(
        root: PathBuf,
    ) -> (
        EventProcessor,
        Arc<RwLock<ProjectWorkspace>>,
        Arc<RwLock<InvertedIndex>>,
    ) {
        make_processor_with_extension_host(root, Box::new(NoOpExtensionHost))
    }

    // ── Document-sync wiring (spec 14b) ───────────────────────────────────────

    /// Records the document-sync calls the processor makes, for `.rs` files only.
    #[derive(Default)]
    struct RecordingDocSync {
        opened: Mutex<Vec<String>>,
        changed: Mutex<Vec<String>>,
        closed: Mutex<Vec<String>>,
    }

    impl DocumentSyncPort for RecordingDocSync {
        fn serves(&self, path: &RelativePath) -> bool {
            path.as_str().ends_with(".rs")
        }
        fn document_opened(&self, path: &RelativePath, _text: &str) {
            self.opened.lock().unwrap().push(path.as_str().to_owned());
        }
        fn document_changed(&self, path: &RelativePath, _text: &str) {
            self.changed.lock().unwrap().push(path.as_str().to_owned());
        }
        fn document_closed(&self, path: &RelativePath) {
            self.closed.lock().unwrap().push(path.as_str().to_owned());
        }
    }

    fn make_processor_with_doc_sync(
        root: PathBuf,
        doc_sync: Arc<dyn DocumentSyncPort + Send + Sync>,
        echo_set: Arc<Mutex<HashMap<String, ()>>>,
    ) -> EventProcessor {
        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        let storage = Box::new(InMemoryStorage::new());
        EventProcessor::new(root, workspace, index, storage, Box::new(NoOpExtensionHost))
            .with_document_sync(doc_sync, echo_set)
    }

    /// EV1: a Create for a served file opens it on the server.
    #[test]
    fn create_opens_document_on_server() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file = root.join("a.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();

        let sync = Arc::new(RecordingDocSync::default());
        let echo: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut proc = make_processor_with_doc_sync(root, Arc::clone(&sync) as _, echo);

        proc.process_event(WatchEvent::Create(file)).unwrap();

        assert_eq!(*sync.opened.lock().unwrap(), vec!["a.rs".to_owned()]);
    }

    /// AC5/EV1: a Modify pushes the new content as `didChange`.
    #[test]
    fn modify_syncs_changed_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file = root.join("a.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();

        let sync = Arc::new(RecordingDocSync::default());
        let echo: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut proc = make_processor_with_doc_sync(root, Arc::clone(&sync) as _, echo);

        proc.process_event(WatchEvent::Create(file.clone()))
            .unwrap();
        std::fs::write(&file, b"fn main() { let x = 1; }").unwrap();
        proc.process_event(WatchEvent::Modify(file)).unwrap();

        assert_eq!(*sync.changed.lock().unwrap(), vec!["a.rs".to_owned()]);
    }

    /// AC6/UN1: a Modify for a path in the echo set is NOT synced.
    #[test]
    fn echo_suppressed_modify_does_not_sync() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file = root.join("a.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();

        let sync = Arc::new(RecordingDocSync::default());
        let echo: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));
        echo.lock().unwrap().insert("a.rs".to_owned(), ());
        let mut proc = make_processor_with_doc_sync(root, Arc::clone(&sync) as _, echo);

        proc.process_event(WatchEvent::Create(file.clone()))
            .unwrap();
        std::fs::write(&file, b"fn main() { let x = 1; }").unwrap();
        proc.process_event(WatchEvent::Modify(file)).unwrap();

        assert!(
            sync.changed.lock().unwrap().is_empty(),
            "formatter-echo Modify must not emit a document_changed"
        );
    }

    /// A Delete closes the document on the server.
    #[test]
    fn delete_closes_document_on_server() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file = root.join("a.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();

        let sync = Arc::new(RecordingDocSync::default());
        let echo: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut proc = make_processor_with_doc_sync(root, Arc::clone(&sync) as _, echo);

        proc.process_event(WatchEvent::Create(file.clone()))
            .unwrap();
        std::fs::remove_file(&file).unwrap();
        proc.process_event(WatchEvent::Delete(file)).unwrap();

        assert_eq!(*sync.closed.lock().unwrap(), vec!["a.rs".to_owned()]);
    }

    /// A file whose language has no server (`serves` false) is never synced.
    #[test]
    fn unserved_extension_is_not_synced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file = root.join("notes.md");
        std::fs::write(&file, b"# hi").unwrap();

        let sync = Arc::new(RecordingDocSync::default());
        let echo: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut proc = make_processor_with_doc_sync(root, Arc::clone(&sync) as _, echo);

        proc.process_event(WatchEvent::Create(file)).unwrap();

        assert!(sync.opened.lock().unwrap().is_empty());
    }

    /// Helper: run `find_file` against current index+workspace state.
    fn find_file(
        ws: &Arc<RwLock<ProjectWorkspace>>,
        idx: &Arc<RwLock<InvertedIndex>>,
        query: &str,
    ) -> Vec<RelativePath> {
        use crate::domain::index::FileSearch;
        use crate::ports::inbound::SearchUseCase as _;
        let ws_guard = ws.read().unwrap();
        let idx_guard = idx.read().unwrap();
        FileSearch::new(&idx_guard, &ws_guard)
            .find_file(query)
            .unwrap()
    }

    // ── TDD step 1/2: Create → searchable ─────────────────────────────────────

    /// AC1: After a Create event, find_file returns the new file.
    #[test]
    fn create_event_makes_file_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create an actual file so read_metadata does not fall back to default.
        let file_path = root.join("src").join("http_client.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, b"// content").unwrap();

        let (mut processor, ws, idx) = make_processor(root);

        processor
            .process_event(WatchEvent::Create(file_path))
            .unwrap();

        let results = find_file(&ws, &idx, "client");
        let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();
        assert!(
            paths.contains(&"src/http_client.rs"),
            "expected src/http_client.rs in {paths:?}"
        );
    }

    /// Create event on a path already tracked is idempotent (no DuplicatePath error).
    #[test]
    fn duplicate_create_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("file.rs");
        std::fs::write(&file_path, b"x").unwrap();

        let (mut processor, ws, _idx) = make_processor(root);

        processor
            .process_event(WatchEvent::Create(file_path.clone()))
            .unwrap();
        // Second create must not panic or error.
        processor
            .process_event(WatchEvent::Create(file_path))
            .unwrap();

        let ws_guard = ws.read().unwrap();
        let count = ws_guard.snapshot().files.len();
        assert_eq!(count, 1, "idempotent create must not insert a duplicate");
    }

    // ── TDD step 3/4: Delete + Rename consistency ──────────────────────────────

    /// AC2: After a Delete event, the file disappears from search.
    #[test]
    fn delete_event_removes_file_from_search() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("unique_module.rs");
        std::fs::write(&file_path, b"x").unwrap();

        let (mut processor, ws, idx) = make_processor(root);

        processor
            .process_event(WatchEvent::Create(file_path.clone()))
            .unwrap();

        // Verify it was indexed.
        let before = find_file(&ws, &idx, "unique");
        assert!(
            !before.is_empty(),
            "file must be searchable after create: {before:?}"
        );

        // Delete on disk then send Delete event.
        std::fs::remove_file(&file_path).unwrap();
        processor
            .process_event(WatchEvent::Delete(file_path))
            .unwrap();

        let after = find_file(&ws, &idx, "unique");
        assert!(
            after.is_empty(),
            "file must be gone after delete: {after:?}"
        );

        // FileId must be freed (workspace should have no live files).
        let ws_guard = ws.read().unwrap();
        assert_eq!(
            ws_guard.snapshot().files.len(),
            0,
            "workspace must be empty"
        );
    }

    /// Delete on an unknown path is a silent no-op (not an error).
    #[test]
    fn delete_unknown_path_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (mut processor, _, _) = make_processor(root.clone());

        let result = processor.process_event(WatchEvent::Delete(root.join("ghost.rs")));
        assert!(result.is_ok(), "delete on unknown path must not error");
    }

    /// AC3: Rename a.rs → b.rs: find_file("b") hits, find_file("a") misses.
    #[test]
    fn rename_event_remaps_path_bijection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let a = root.join("alpha_service.rs");
        let b = root.join("beta_service.rs");
        std::fs::write(&a, b"x").unwrap();

        let (mut processor, ws, idx) = make_processor(root);

        // Insert the original file.
        processor
            .process_event(WatchEvent::Create(a.clone()))
            .unwrap();

        // Simulate the rename on disk.
        std::fs::rename(&a, &b).unwrap();

        processor
            .process_event(WatchEvent::Rename {
                from: a.clone(),
                to: b.clone(),
            })
            .unwrap();

        let hits_beta = find_file(&ws, &idx, "beta");
        let paths_beta: Vec<&str> = hits_beta.iter().map(|p| p.as_str()).collect();
        assert!(
            paths_beta.contains(&"beta_service.rs"),
            "beta_service.rs must be searchable after rename; got {paths_beta:?}"
        );

        // "alpha" must no longer resolve to anything unique.
        let hits_alpha = find_file(&ws, &idx, "alpha");
        let paths_alpha: Vec<&str> = hits_alpha
            .iter()
            .filter(|p| p.as_str() == "alpha_service.rs")
            .map(|p| p.as_str())
            .collect();
        assert!(
            paths_alpha.is_empty(),
            "alpha_service.rs must be gone after rename; got {paths_alpha:?}"
        );

        // Bijection: only one file in workspace.
        let ws_guard = ws.read().unwrap();
        assert_eq!(
            ws_guard.snapshot().files.len(),
            1,
            "exactly one file after rename"
        );
        assert_eq!(
            ws_guard.snapshot().files[0].path.as_str(),
            "beta_service.rs",
            "remaining file must be beta_service.rs"
        );
    }

    // ── TDD step 5/6: out-of-root ignore + Modify ─────────────────────────────

    /// AC5/UN1: Events outside the workspace root are silently ignored.
    #[test]
    fn event_outside_root_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let outside = std::env::temp_dir().join("outside_file.rs");

        let (mut processor, ws, _idx) = make_processor(root);

        // All three event types must be no-ops when outside root.
        processor
            .process_event(WatchEvent::Create(outside.clone()))
            .unwrap();
        processor
            .process_event(WatchEvent::Modify(outside.clone()))
            .unwrap();
        processor
            .process_event(WatchEvent::Delete(outside.clone()))
            .unwrap();
        processor
            .process_event(WatchEvent::Rename {
                from: outside.clone(),
                to: outside,
            })
            .unwrap();

        let ws_guard = ws.read().unwrap();
        assert_eq!(
            ws_guard.snapshot().files.len(),
            0,
            "no files must be added for out-of-root events"
        );
    }

    /// EV2/Modify: metadata is updated after a Modify event.
    #[test]
    fn modify_event_updates_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("config.rs");
        std::fs::write(&file_path, b"v1").unwrap();

        let (mut processor, ws, idx) = make_processor(root);

        processor
            .process_event(WatchEvent::Create(file_path.clone()))
            .unwrap();

        // Update file content and send Modify.
        std::fs::write(&file_path, b"v2 with more content").unwrap();
        processor
            .process_event(WatchEvent::Modify(file_path))
            .unwrap();

        // File must still be searchable after modify.
        let results = find_file(&ws, &idx, "config");
        assert!(
            !results.is_empty(),
            "file must remain searchable after modify"
        );

        // Only one file in workspace (not duplicated by modify).
        let ws_guard = ws.read().unwrap();
        assert_eq!(
            ws_guard.snapshot().files.len(),
            1,
            "no duplicate after modify"
        );
    }

    /// EV2: Modify must not change the FileId — callers holding a FileId from
    /// an earlier Create or search must still resolve to the same file after Modify.
    #[test]
    fn modify_preserves_file_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("stable.rs");
        std::fs::write(&file_path, b"v1").unwrap();

        let (mut processor, ws, _idx) = make_processor(root);

        processor
            .process_event(WatchEvent::Create(file_path.clone()))
            .unwrap();

        // Capture the FileId after Create.
        let rel = crate::domain::virtual_file::RelativePath::new("stable.rs");
        let id_before = ws
            .read()
            .unwrap()
            .get_by_path(&rel)
            .expect("must be tracked");

        // Modify the file and send a Modify event.
        std::fs::write(&file_path, b"v2 different content").unwrap();
        processor
            .process_event(WatchEvent::Modify(file_path))
            .unwrap();

        // FileId must be identical — the slot was updated, not reallocated.
        let id_after = ws.read().unwrap().get_by_path(&rel).expect("still tracked");
        assert_eq!(
            id_before, id_after,
            "Modify must not reallocate the FileId (generational handle stability)"
        );
    }

    /// Modify on an untracked path behaves as a late Create (EV2 fallback).
    #[test]
    fn modify_on_untracked_path_acts_as_create() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("new_file.rs");
        std::fs::write(&file_path, b"x").unwrap();

        let (mut processor, ws, idx) = make_processor(root);

        processor
            .process_event(WatchEvent::Modify(file_path))
            .unwrap();

        let results = find_file(&ws, &idx, "new");
        assert!(!results.is_empty(), "Modify on untracked acts as Create");

        let ws_guard = ws.read().unwrap();
        assert_eq!(ws_guard.snapshot().files.len(), 1);
    }

    // ── TDD step 7/8: ExtensionHost broadcast ─────────────────────────────────

    /// Helper wrapper to allow Arc<RecordingExtensionHost> to satisfy Box<dyn ExtensionHostPort>.
    struct ArcExtHost(Arc<RecordingExtensionHost>);

    impl ExtensionHostPort for ArcExtHost {
        fn on_file_indexed(&self, id: crate::domain::FileId, path: &RelativePath) {
            self.0.on_file_indexed(id, path);
        }
        fn on_file_changed(&self, id: crate::domain::FileId, path: &RelativePath) {
            self.0.on_file_changed(id, path);
        }
        fn on_file_deleted(&self, path: &RelativePath) {
            self.0.on_file_deleted(path);
        }
        fn declared_tools(
            &self,
        ) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
            Vec::new()
        }
        fn invoke(
            &self,
            tool_name: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, crate::domain::InvokeError> {
            self.0.invoke(tool_name, params)
        }
    }

    /// OP1: on_file_changed is broadcast after Create and Modify.
    #[test]
    fn extension_host_notified_after_create_and_modify() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("service.rs");
        std::fs::write(&file_path, b"v1").unwrap();

        let recording = Arc::new(RecordingExtensionHost::default());
        let boxed_host: Box<dyn ExtensionHostPort + Send + Sync> =
            Box::new(ArcExtHost(Arc::clone(&recording)));

        let (mut processor, _, _) = make_processor_with_extension_host(root, boxed_host);

        processor
            .process_event(WatchEvent::Create(file_path.clone()))
            .unwrap();

        std::fs::write(&file_path, b"v2").unwrap();
        processor
            .process_event(WatchEvent::Modify(file_path))
            .unwrap();

        let calls = recording.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "on_file_changed must be called twice; got {}",
            calls.len()
        );
    }

    /// OP1: on_file_changed is broadcast after Delete.
    #[test]
    fn extension_host_notified_after_delete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("soon_deleted.rs");
        std::fs::write(&file_path, b"x").unwrap();

        let recording = Arc::new(RecordingExtensionHost::default());
        let boxed_host: Box<dyn ExtensionHostPort + Send + Sync> =
            Box::new(ArcExtHost(Arc::clone(&recording)));

        let (mut processor, _, _) = make_processor_with_extension_host(root, boxed_host);

        // Create then delete — we want the delete notification.
        processor
            .process_event(WatchEvent::Create(file_path.clone()))
            .unwrap();
        std::fs::remove_file(&file_path).unwrap();
        processor
            .process_event(WatchEvent::Delete(file_path))
            .unwrap();

        let calls = recording.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "on_file_changed must be called for both Create and Delete; got {} call(s)",
            calls.len()
        );
    }

    /// AC3: Rename must appear atomic to concurrent readers — the file must be
    /// visible under exactly one name at all observable times.
    ///
    /// We verify this by running many rename events and checking the workspace
    /// never contains zero files or two files between the delete and create legs.
    /// A concurrent reader polls the workspace count in a tight loop throughout.
    #[test]
    fn rename_is_atomic_to_concurrent_readers() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let a = root.join("alpha_atomic.rs");
        let b = root.join("beta_atomic.rs");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();

        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        let storage = Box::new(InMemoryStorage::new());
        let mut processor = EventProcessor::new(
            root.clone(),
            Arc::clone(&workspace),
            Arc::clone(&index),
            storage,
            Box::new(NoOpExtensionHost),
        );

        // Pre-insert alpha so the rename has a file to delete.
        processor
            .process_event(WatchEvent::Create(a.clone()))
            .unwrap();

        let saw_gap = Arc::new(AtomicBool::new(false));
        let saw_gap_clone = Arc::clone(&saw_gap);
        let ws_for_reader = Arc::clone(&workspace);
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);

        // Reader thread: continuously checks that the workspace always contains
        // exactly one file (never 0, never 2).
        let reader = thread::spawn(move || {
            while !done_clone.load(Ordering::Relaxed) {
                let count = ws_for_reader.read().unwrap().snapshot().files.len();
                if count != 1 {
                    saw_gap_clone.store(true, Ordering::Relaxed);
                }
            }
        });

        // Perform the rename — this must appear atomic to the reader above.
        processor
            .process_event(WatchEvent::Rename {
                from: a.clone(),
                to: b.clone(),
            })
            .unwrap();

        done.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        assert!(
            !saw_gap.load(Ordering::Relaxed),
            "reader observed a gap (0 or 2 files) during rename — atomicity violated"
        );
    }

    // ── TDD step 7/8: concurrent read+write no-deadlock ───────────────────────

    // ── TDD: handle_rename tmp artifact guard (UN2/AC5) ──────────────────────

    /// AC5: Rename from a real file to a .tmp_write path must NOT insert the
    /// tmp artifact into the VFS. The source file must be treated as deleted.
    #[test]
    fn rename_to_tmp_write_path_treats_source_as_delete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let real_file = root.join("foo.rs");
        std::fs::write(&real_file, b"x").unwrap();

        let (mut processor, ws, _idx) = make_processor(root.clone());

        // Insert the real file first.
        processor
            .process_event(WatchEvent::Create(real_file.clone()))
            .unwrap();
        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            1,
            "setup: one real file in VFS"
        );

        // Simulate an external agent renaming foo.rs → foo.rs.tmp_write.
        processor
            .process_event(WatchEvent::Rename {
                from: real_file,
                to: root.join("foo.rs.tmp_write"),
            })
            .unwrap();

        let ws_guard = ws.read().unwrap();
        // The tmp artifact must NOT be in the VFS.
        let tmp_rel = RelativePath::new("foo.rs.tmp_write");
        assert!(
            ws_guard.get_by_path(&tmp_rel).is_none(),
            ".tmp_write must never be inserted into the VFS on Rename"
        );
        // The original file was removed (rename = move away).
        assert_eq!(
            ws_guard.snapshot().files.len(),
            0,
            "source file must be removed when renamed to a tmp artifact path"
        );
    }

    /// AC5: Rename whose destination is a .~tmp path is also silently suppressed.
    #[test]
    fn rename_to_tilde_tmp_path_is_suppressed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let real_file = root.join("bar.rs");
        std::fs::write(&real_file, b"x").unwrap();

        let (mut processor, ws, _idx) = make_processor(root.clone());
        processor
            .process_event(WatchEvent::Create(real_file.clone()))
            .unwrap();

        processor
            .process_event(WatchEvent::Rename {
                from: real_file,
                to: root.join("bar.rs.~tmp"),
            })
            .unwrap();

        let ws_guard = ws.read().unwrap();
        let tmp_rel = RelativePath::new("bar.rs.~tmp");
        assert!(
            ws_guard.get_by_path(&tmp_rel).is_none(),
            ".~tmp must never be inserted into the VFS on Rename"
        );
    }

    /// Rename whose destination matches an ignore rule must delete the source and
    /// never insert the destination (rename-into-secret regression net).
    #[test]
    fn rename_into_ignored_path_deletes_source_and_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "*.key\n");

        let src = root.join("foo.rs");
        std::fs::write(&src, b"x").unwrap();

        let (mut processor, ws, _idx) = make_processor(root.clone());
        processor
            .process_event(WatchEvent::Create(src.clone()))
            .unwrap();
        assert_eq!(ws.read().unwrap().snapshot().files.len(), 1);

        let dst = root.join("src").join("deploy.key");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        processor
            .process_event(WatchEvent::Rename { from: src, to: dst })
            .unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            0,
            "rename onto an ignored path must delete the source and not index the destination"
        );
    }

    /// Renaming the root `.towerignore` *away* must rebuild the matcher so its
    /// rules stop applying to subsequent events (source-side rebuild).
    #[test]
    fn rename_towerignore_away_rebuilds_matcher() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "*.key\n");

        let (mut processor, ws, _idx) = make_processor(root.clone());

        // While .towerignore exists, a *.key is excluded.
        let key = root.join("a.key");
        std::fs::write(&key, b"k").unwrap();
        processor.process_event(WatchEvent::Create(key)).unwrap();
        assert_eq!(ws.read().unwrap().snapshot().files.len(), 0);

        // Rename .towerignore away (rules move off-tree). The fs change precedes
        // the event, mirroring the real watcher's ordering.
        let ti = root.join(".towerignore");
        let backup = root.join("backup.txt");
        std::fs::rename(&ti, &backup).unwrap();
        processor
            .process_event(WatchEvent::Rename {
                from: ti,
                to: backup,
            })
            .unwrap();

        // Now a *.key must be indexed — the matcher was rebuilt to empty.
        let key2 = root.join("b.key");
        std::fs::write(&key2, b"k").unwrap();
        processor.process_event(WatchEvent::Create(key2)).unwrap();

        // backup.txt (the rename destination) + b.key.
        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            2,
            "after .towerignore is renamed away, its rules must no longer apply"
        );
    }

    // ── .towerignore / hidden-file filtering (security hole closure) ──────────

    /// Helper: write a `.towerignore` at `root` with the given content.
    fn write_towerignore(root: &Path, content: &str) {
        std::fs::write(root.join(".towerignore"), content).unwrap();
    }

    /// A file under a directory matched by `.towerignore` (via a parent match)
    /// must NOT be indexed by the watcher.
    #[test]
    fn create_secret_under_ignored_dir_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "secrets/\n");

        let secret = root.join("secrets").join("api.key");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, b"TOPSECRET").unwrap();

        let (mut processor, ws, _idx) = make_processor(root);
        processor.process_event(WatchEvent::Create(secret)).unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            0,
            "secret under an ignored directory must never be indexed"
        );
    }

    /// An un-anchored glob like `*.key` must match at ANY depth — proving a
    /// root-only matcher closes the secret hole without nested .towerignore.
    #[test]
    fn create_key_file_at_depth_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "*.key\n");

        let nested = root.join("src").join("deploy.key");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, b"KEYDATA").unwrap();

        let (mut processor, ws, _idx) = make_processor(root);
        processor.process_event(WatchEvent::Create(nested)).unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            0,
            "un-anchored *.key must match at depth and never be indexed"
        );
    }

    /// Hidden files/dirs (any component starting with '.') must never be indexed,
    /// matching the scan's `hidden(true)` — covers `.env` and `.git/config`.
    #[test]
    fn create_hidden_paths_are_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let dotenv = root.join(".env");
        std::fs::write(&dotenv, b"SECRET=1").unwrap();
        let gitcfg = root.join(".git").join("config");
        std::fs::create_dir_all(gitcfg.parent().unwrap()).unwrap();
        std::fs::write(&gitcfg, b"[core]").unwrap();

        let (mut processor, ws, _idx) = make_processor(root);
        processor.process_event(WatchEvent::Create(dotenv)).unwrap();
        processor.process_event(WatchEvent::Create(gitcfg)).unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            0,
            "hidden paths (.env, .git/config) must never be indexed"
        );
    }

    /// Control: a normal, non-ignored, non-hidden file IS indexed.
    #[test]
    fn create_normal_file_is_indexed_with_filters_active() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "secrets/\n*.key\n");

        let main = root.join("src").join("main.rs");
        std::fs::create_dir_all(main.parent().unwrap()).unwrap();
        std::fs::write(&main, b"fn main(){}").unwrap();

        let (mut processor, ws, _idx) = make_processor(root);
        processor.process_event(WatchEvent::Create(main)).unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            1,
            "a normal file must still be indexed when filters are active"
        );
    }

    /// Negation: `!logs/keep.log` re-includes a file otherwise excluded by
    /// `logs/*`. The kept file is indexed; the debug log is not.
    #[test]
    fn negation_reincludes_whitelisted_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "logs/*\n!logs/keep.log\n");

        let logs = root.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let keep = logs.join("keep.log");
        let debug = logs.join("debug.log");
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::write(&debug, b"debug").unwrap();

        let (mut processor, ws, _idx) = make_processor(root);
        processor.process_event(WatchEvent::Create(keep)).unwrap();
        processor.process_event(WatchEvent::Create(debug)).unwrap();

        let ws_guard = ws.read().unwrap();
        let snapshot = ws_guard.snapshot();
        let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"logs/keep.log"),
            "whitelisted logs/keep.log must be indexed; got {paths:?}"
        );
        assert!(
            !paths.contains(&"logs/debug.log"),
            "logs/debug.log must remain ignored; got {paths:?}"
        );
    }

    /// Modify-becomes-ignored: a tracked file that, after a `.towerignore` edit,
    /// now matches an ignore rule must be REMOVED on the next Modify event.
    #[test]
    fn modify_becomes_ignored_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // No matching rule initially.
        write_towerignore(&root, "# empty\n");

        let file = root.join("a.rs");
        std::fs::write(&file, b"v1").unwrap();

        let (mut processor, ws, _idx) = make_processor(root.clone());
        processor
            .process_event(WatchEvent::Create(file.clone()))
            .unwrap();
        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            1,
            "setup: a.rs indexed"
        );

        // Edit .towerignore so a.rs is now ignored, then rebuild via the
        // .towerignore event, then Modify a.rs.
        write_towerignore(&root, "a.rs\n");
        processor
            .process_event(WatchEvent::Modify(root.join(".towerignore")))
            .unwrap();
        std::fs::write(&file, b"v2").unwrap();
        processor.process_event(WatchEvent::Modify(file)).unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            0,
            "a.rs now matching ignore must be removed on Modify"
        );
    }

    /// Matcher rebuild: after a Modify on the root `.towerignore` adds `build/`,
    /// a subsequent Create under `build/` is NOT indexed.
    #[test]
    fn towerignore_event_rebuilds_matcher() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "# empty\n");

        let (mut processor, ws, _idx) = make_processor(root.clone());

        // Rewrite .towerignore to add build/ and fire the rebuild event.
        write_towerignore(&root, "build/\n");
        processor
            .process_event(WatchEvent::Modify(root.join(".towerignore")))
            .unwrap();

        let built = root.join("build").join("x.rs");
        std::fs::create_dir_all(built.parent().unwrap()).unwrap();
        std::fs::write(&built, b"x").unwrap();
        processor.process_event(WatchEvent::Create(built)).unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            0,
            "after rebuild, files under newly-ignored build/ must not be indexed"
        );
    }

    /// The `.towerignore` file itself must never be indexed (it is hidden).
    #[test]
    fn towerignore_file_itself_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_towerignore(&root, "# empty\n");

        let (mut processor, ws, _idx) = make_processor(root.clone());
        processor
            .process_event(WatchEvent::Create(root.join(".towerignore")))
            .unwrap();

        assert_eq!(
            ws.read().unwrap().snapshot().files.len(),
            0,
            ".towerignore is hidden and must never be indexed"
        );
    }

    /// Concurrent reads do not deadlock with concurrent writes.
    ///
    /// 4 writer threads each send 25 Create events.
    /// 4 reader threads each call find_file 50 times.
    /// The test would hang under a deadlock (the test timeout would fire).
    #[test]
    fn concurrent_reads_and_writes_do_not_deadlock() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let workspace = Arc::new(RwLock::new(ProjectWorkspace::new()));
        let index = Arc::new(RwLock::new(InvertedIndex::new()));

        // Pre-create directories to avoid metadata I/O from racing with readers.
        let files_per_thread = 25usize;
        let writer_threads = 4usize;

        for t in 0..writer_threads {
            for i in 0..files_per_thread {
                let path = root.join(format!("thread{t}_file{i}.rs"));
                std::fs::write(&path, b"x").unwrap();
            }
        }

        let ws_for_readers = Arc::clone(&workspace);
        let idx_for_readers = Arc::clone(&index);

        // Spawn reader threads first.
        let readers: Vec<thread::JoinHandle<()>> = (0..4)
            .map(|_| {
                let ws = Arc::clone(&ws_for_readers);
                let idx = Arc::clone(&idx_for_readers);
                thread::spawn(move || {
                    use crate::domain::index::FileSearch;
                    use crate::ports::inbound::SearchUseCase as _;
                    for _ in 0..50 {
                        let ws_g = ws.read().unwrap();
                        let idx_g = idx.read().unwrap();
                        let _ = FileSearch::new(&idx_g, &ws_g).find_file("file");
                    }
                })
            })
            .collect();

        // Spawn writer threads.
        let writers: Vec<thread::JoinHandle<()>> = (0..writer_threads)
            .map(|t| {
                let root_clone = root.clone();
                let ws = Arc::clone(&workspace);
                let idx = Arc::clone(&index);
                thread::spawn(move || {
                    let storage = Box::new(InMemoryStorage::new());
                    let mut processor = EventProcessor::new(
                        root_clone.clone(),
                        ws,
                        idx,
                        storage,
                        Box::new(NoOpExtensionHost),
                    );
                    for i in 0..files_per_thread {
                        let path = root_clone.join(format!("thread{t}_file{i}.rs"));
                        processor
                            .process_event(WatchEvent::Create(path))
                            .unwrap_or(()); // ignore storage failures in test
                    }
                })
            })
            .collect();

        for handle in writers {
            handle.join().expect("writer thread panicked");
        }
        for handle in readers {
            handle.join().expect("reader thread panicked");
        }
        // If we reach here without a hang, no deadlock occurred.
    }
}
