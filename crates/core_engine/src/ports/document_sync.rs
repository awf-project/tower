//! `DocumentSyncPort` — outbound port the watcher uses to mirror live file
//! state into a resident language server.
//!
//! The watcher's `EventProcessor` is the single driver: it calls
//! [`document_opened`](DocumentSyncPort::document_opened) on Create,
//! [`document_changed`](DocumentSyncPort::document_changed) on Modify, and
//! [`document_closed`](DocumentSyncPort::document_closed) on Delete. The LSP
//! adapter implements this by driving its `DocumentTracker`
//! (`didOpen`/`didChange`/`didClose`). [`serves`](DocumentSyncPort::serves)
//! lets the processor skip reading file content for languages with no server.
//!
//! Calls are fire-and-forget (no `Result`): a language-server write failure must
//! never break the watcher's VFS bookkeeping.

use crate::domain::RelativePath;

/// Mirrors workspace file lifecycle into a language server.
pub trait DocumentSyncPort: Send + Sync {
    /// Whether a server is configured for this file's language. When `false`
    /// the processor skips reading content and the other methods are no-ops.
    fn serves(&self, path: &RelativePath) -> bool;

    /// A served file appeared (watcher Create): open it on the server.
    fn document_opened(&self, path: &RelativePath, text: &str);

    /// A served file changed (watcher Modify): push its latest content.
    fn document_changed(&self, path: &RelativePath, text: &str);

    /// A served file was removed (watcher Delete): close it on the server.
    fn document_closed(&self, path: &RelativePath);
}

/// No-op sink for deployments/tests without a language server.
#[derive(Debug, Default)]
pub struct NoOpDocumentSync;

impl DocumentSyncPort for NoOpDocumentSync {
    fn serves(&self, _path: &RelativePath) -> bool {
        false
    }
    fn document_opened(&self, _path: &RelativePath, _text: &str) {}
    fn document_changed(&self, _path: &RelativePath, _text: &str) {}
    fn document_closed(&self, _path: &RelativePath) {}
}
