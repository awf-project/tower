//! `CodeIntelligencePort` — outbound port for language-aware analysis.
//!
//! # Contract (checked by `crate::test_support::codeintel_contract`)
//!
//! - **clean text → no diagnostics**: `check` on syntactically/semantically
//!   valid content returns an empty `Vec`.
//! - **broken text → diagnostics**: `check` on content the backend rejects
//!   returns at least one `Diagnostic` whose `severity` is `Error`.
//! - **unsupported extension → `Unsupported`**: `check` on a path whose
//!   extension has no configured backend returns `Err(CodeIntelError::Unsupported)`.
//! - **idempotent re-check**: calling `check` twice on the same `(path, text)`
//!   returns equal diagnostics (the backend reflects the latest content, not an
//!   accumulation).
//!
//! The single symmetric `check` method is what lets the in-memory fake and the
//! real rust-analyzer adapter satisfy the SAME contract suite (spec 02): both
//! map `(path, text)` to diagnostics, one via a built-in marker analyzer, the
//! other by driving the language server and awaiting `publishDiagnostics`.

use crate::domain::RelativePath;
use crate::domain::code_intel::Diagnostic;

/// Failure modes of a code-intelligence backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeIntelError {
    /// No backend is configured for this file's extension.
    Unsupported,
    /// The backend is configured but failed (spawn error, crash, protocol error).
    Backend(String),
}

impl core::fmt::Display for CodeIntelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "no code-intelligence backend for this file type"),
            Self::Backend(msg) => write!(f, "code-intelligence backend error: {msg}"),
        }
    }
}

impl core::error::Error for CodeIntelError {}

/// Language-aware analysis of file content.
///
/// # Object safety
///
/// Object-safe (`&self`, owned returns) so it can be wired as
/// `Arc<dyn CodeIntelligencePort>` at runtime.
pub trait CodeIntelligencePort: Send + Sync {
    /// Analyze `text` as the current content of `path` and return diagnostics.
    ///
    /// An empty `Vec` means "analyzed, no problems". The content is treated as
    /// the document's latest full text (the backend syncs to it before
    /// reporting).
    ///
    /// # Errors
    ///
    /// - [`CodeIntelError::Unsupported`] — no backend for this extension.
    /// - [`CodeIntelError::Backend`] — the backend errored.
    fn check(&self, path: &RelativePath, text: &str) -> Result<Vec<Diagnostic>, CodeIntelError>;
}
