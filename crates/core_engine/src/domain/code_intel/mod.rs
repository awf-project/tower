//! Code-intelligence domain value objects.
//!
//! Pure data describing a diagnostic (a problem reported about a file). No I/O,
//! no LSP types — the LSP wire format is an adapter concern. Positions are
//! UTF-16 code-unit offsets to match the LSP protocol the adapter speaks; the
//! adapter is responsible for any encoding conversion at the boundary.

#![forbid(unsafe_code)]

use crate::domain::RelativePath;

/// A zero-based position in a text document.
///
/// `character` counts UTF-16 code units (LSP default encoding), NOT bytes or
/// Unicode scalar values. The domain stores what the protocol uses; conversion
/// lives in the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A half-open range `[start, end)` within a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// Diagnostic severity, mirroring the four LSP levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

/// A single problem reported about a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub message: String,
    /// The producer, e.g. `"rustc"` or `"clippy"`. `None` if the backend omits it.
    pub source: Option<String>,
    /// A machine-readable code, e.g. `"E0425"`. `None` if absent.
    pub code: Option<String>,
}

/// A range within a specific workspace file — the result of a navigation query
/// (`definition`, `references`). The `path` is workspace-relative; the adapter
/// strips the workspace root and drops out-of-root results before constructing
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: RelativePath,
    pub range: Range,
}

/// Hover information for a position: human-readable contents (already rendered
/// to text by the adapter) and the optional range the hover describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hover {
    pub contents: String,
    pub range: Option<Range>,
}

/// One entry in a document's symbol outline (`document_symbols`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// Kind name, e.g. `"function"`/`"struct"`, matching the AST plugin
    /// vocabulary so the tree-sitter fallback and LSP path agree.
    pub kind: String,
    /// Full extent of the symbol (signature + body).
    pub range: Range,
    /// The identifier range alone (just the declared name).
    pub selection_range: Range,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_is_constructible_and_comparable() {
        let a = Diagnostic {
            range: Range {
                start: Position {
                    line: 3,
                    character: 4,
                },
                end: Position {
                    line: 3,
                    character: 9,
                },
            },
            severity: Severity::Error,
            message: "cannot find value `foo`".to_owned(),
            source: Some("rustc".to_owned()),
            code: Some("E0425".to_owned()),
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(a.range.start.line, 3);
        assert_eq!(a.severity, Severity::Error);
    }
}
