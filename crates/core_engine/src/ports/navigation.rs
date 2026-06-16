//! `NavigationPort` — outbound port for semantic navigation.
//!
//! Sibling to [`CodeIntelligencePort`](super::CodeIntelligencePort): `check`
//! answers "is this file broken?", `NavigationPort` answers "where is this
//! symbol / what is it?". They are separated (ISP) because an implementor may
//! support diagnostics without navigation (or vice-versa) — the in-memory fake
//! and a real language server each implement whichever they serve.
//!
//! # Contract (checked by `crate::test_support::navigation_contract`)
//!
//! - **unsupported extension → `Unsupported`**: every method on a path whose
//!   extension has no configured backend returns `Err(CodeIntelError::Unsupported)`.
//! - **supported file → never errors on shape**: a supported file yields `Ok`
//!   (possibly empty / `None`), never a `Backend` error for a well-formed query.
//!
//! Positions are UTF-16 code units (domain convention, matching the LSP wire
//! format); byte↔UTF-16 conversion for the tree-sitter fallback happens in the
//! adapter via `PositionMap`. Each method receives the file's current `text` so
//! the adapter can sync the document before querying (mirroring `check`).

use crate::domain::RelativePath;
use crate::domain::code_intel::{Hover, Location, Position, Symbol};
use crate::ports::CodeIntelError;

/// Semantic navigation over file content.
///
/// Object-safe (`&self`, owned returns) so it can be wired as
/// `Arc<dyn NavigationPort>` at runtime.
pub trait NavigationPort: Send + Sync {
    /// Resolve the definition(s) of the symbol at `position` in `path`.
    ///
    /// Returns every definition site as a workspace-relative [`Location`]
    /// (usually one, but a symbol may have multiple). Empty when nothing
    /// resolves.
    ///
    /// # Errors
    /// - [`CodeIntelError::Unsupported`] — no backend for this extension.
    /// - [`CodeIntelError::Backend`] — the backend errored.
    fn definition(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError>;

    /// Resolve all reference sites of the symbol at `position` in `path`.
    ///
    /// # Errors
    /// As [`definition`](Self::definition).
    fn references(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError>;

    /// Hover information (type/doc) for the symbol at `position` in `path`.
    ///
    /// `Ok(None)` when there is nothing to show.
    ///
    /// # Errors
    /// As [`definition`](Self::definition).
    fn hover(
        &self,
        path: &RelativePath,
        text: &str,
        position: Position,
    ) -> Result<Option<Hover>, CodeIntelError>;

    /// The symbol outline of `path`.
    ///
    /// # Errors
    /// As [`definition`](Self::definition).
    fn document_symbols(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<Symbol>, CodeIntelError>;
}
