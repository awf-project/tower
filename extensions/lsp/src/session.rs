//! `LspSessionPool` — thin wrapper around `core_engine::adapters::lsp::SessionPool`
//! that adds document-sync helpers needed by the sidecar extension (spec 27 EV1).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::lsp_adapter::decode::{WorkspaceEditDecodeError, decode_workspace_edit};
use crate::lsp_adapter::{DiagnosticsEvent, RawWorkspaceEdit, SessionPool};
use core_engine::adapters::config::lsp::LspConfig;
use core_engine::domain::RelativePath;
use core_engine::domain::code_intel::{Diagnostic, Hover, Location, Position};
use core_engine::ports::{
    CodeIntelError, CodeIntelligencePort, DocumentSyncPort, NavigationPort, RenameNavigationError,
};
use extension_protocol::WorkspaceEditSpan;

/// The sidecar extension's LSP session manager.
///
/// Wraps `SessionPool` and exposes document-sync helpers driven by the
/// extension's `event/fileChanged` and `event/fileDeleted` subscriptions.
pub struct LspSessionPool {
    inner: SessionPool,
    // Retained for future use (e.g. absolute path resolution in tool handlers).
    #[allow(dead_code)]
    workspace_root: PathBuf,
}

impl LspSessionPool {
    /// Build the pool from the parsed `[lsp.*]` config.
    ///
    /// Pass `push_tx` to enable the push path (spec 27 O1): diagnostics events
    /// from the language server will be sent on this channel.
    #[must_use]
    pub fn new(
        config: LspConfig,
        workspace_root: PathBuf,
        push_tx: Option<Sender<DiagnosticsEvent>>,
    ) -> Self {
        use std::sync::Arc;
        let spawner = Arc::new(crate::lsp_adapter::pool::RealSpawner);
        let pool = SessionPool::with_spawner(config, workspace_root.clone(), spawner, push_tx);
        Self {
            inner: pool,
            workspace_root,
        }
    }

    /// Check diagnostics for the file at `path` (workspace-relative).
    ///
    /// Reads the file content from `text` (already resolved by the caller).
    pub fn check(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<Diagnostic>, CodeIntelError> {
        self.inner.check(path, text)
    }

    /// Go to definition at `position` in `path`.
    pub fn definition(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        self.inner.definition(path, text, position)
    }

    /// Find references at `position` in `path`.
    pub fn references(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        self.inner.references(path, text, position)
    }

    pub fn implementations(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        self.inner.implementations(path, text, position)
    }

    /// Hover information at `position` in `path`.
    pub fn hover(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Option<Hover>, CodeIntelError> {
        self.inner.hover(path, text, position)
    }

    pub fn rename(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<RawWorkspaceEdit, RenameNavigationError> {
        self.inner.rename(path, text, position, new_name)
    }

    /// Whether this pool is configured to handle the given path.
    pub fn serves(&self, path: &RelativePath) -> bool {
        self.inner.serves(path)
    }

    #[allow(dead_code)]
    pub fn decode_rename_workspace_edit<F>(
        &self,
        raw: RawWorkspaceEdit,
        resolver: F,
    ) -> Result<Vec<WorkspaceEditSpan>, WorkspaceEditDecodeError>
    where
        F: FnMut(&str) -> Result<String, WorkspaceEditDecodeError>,
    {
        decode_workspace_edit(raw, &self.workspace_root, resolver)
    }

    /// Called when `event/fileChanged` is received.
    ///
    /// Reads the file content from disk (absolute path resolved against
    /// `workspace_root`) and pushes `document_changed` to the session.
    pub fn on_file_changed(&self, rel_path: &str, workspace_root: &std::path::Path) {
        let path = RelativePath::new(rel_path);
        if !self.inner.serves(&path) {
            return;
        }
        let abs = workspace_root.join(rel_path);
        match std::fs::read_to_string(&abs) {
            Ok(text) => {
                self.inner.document_changed(&path, &text);
            }
            Err(e) => {
                eprintln!("[tower/lsp-ext] on_file_changed read error for {rel_path}: {e}");
            }
        }
    }

    /// Called when `event/fileDeleted` is received.
    ///
    /// Issues `document_closed` to any open session handling this file's extension.
    pub fn on_file_deleted(&self, rel_path: &str, _workspace_root: &std::path::Path) {
        let path = RelativePath::new(rel_path);
        if !self.inner.serves(&path) {
            return;
        }
        self.inner.document_closed(&path);
    }

    /// Resolve a workspace-relative path to an absolute path.
    #[allow(dead_code)]
    pub fn abs_path(&self, rel: &str) -> PathBuf {
        self.workspace_root.join(rel)
    }

    /// Shutdown all resident language server sessions gracefully.
    pub fn shutdown_all(&mut self) {
        // The `SessionPool` drops all sessions when it is dropped.
        // We take no explicit action here; on drop the pool kills its children.
    }
}

#[cfg(test)]
#[allow(clippy::mutable_key_type)]
mod tests {
    use super::*;
    use core_engine::domain::mutation::compute_content_version;
    use std::collections::HashMap;
    use std::str::FromStr;

    use lsp_types::{Position as LspPosition, Range as LspRange, TextEdit, Uri, WorkspaceEdit};

    fn uri(value: &str) -> Uri {
        Uri::from_str(value).unwrap()
    }

    fn text_edit(l0: u32, c0: u32, l1: u32, c1: u32, replacement: &str) -> TextEdit {
        TextEdit::new(
            LspRange {
                start: LspPosition {
                    line: l0,
                    character: c0,
                },
                end: LspPosition {
                    line: l1,
                    character: c1,
                },
            },
            replacement.to_owned(),
        )
    }

    #[test]
    fn session_exposes_decoded_rename_edit_data_without_performing_hostcall() {
        let workspace = tempfile::tempdir().unwrap();
        let pool = LspSessionPool::new(LspConfig::default(), workspace.path().to_path_buf(), None);
        let uri_path = workspace.path().join("src/main.rs");
        let mut changes = HashMap::new();
        changes.insert(
            uri(&format!("file://{}", uri_path.display())),
            vec![text_edit(0, 4, 0, 12, "new_name")],
        );

        let spans = pool
            .decode_rename_workspace_edit(WorkspaceEdit::new(changes), |path| {
                assert_eq!(path, "src/main.rs");
                Ok("let old_name = 1;\n".to_owned())
            })
            .unwrap();

        assert_eq!(
            spans,
            vec![WorkspaceEditSpan {
                path: "src/main.rs".to_owned(),
                start_byte: 4,
                end_byte: 12,
                replacement: "new_name".to_owned(),
                base_hash: Some(compute_content_version(b"let old_name = 1;\n")),
            }]
        );
    }
}
