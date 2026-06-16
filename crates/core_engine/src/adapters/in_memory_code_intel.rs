//! `InMemoryCodeIntel` — a deterministic test double for `CodeIntelligencePort`.
//!
//! It is NOT a language server. It implements the port's behavioural contract
//! with a trivial built-in analyzer so domain/tool tests run with zero I/O and
//! no external binary:
//!
//! - a line containing the marker `//!ERR` yields one `Error` diagnostic on
//!   that line, spanning the marker;
//! - any other content yields no diagnostics;
//! - a path whose extension is not in `supported` yields `Unsupported`.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use crate::domain::RelativePath;
use crate::domain::code_intel::{Diagnostic, Hover, Location, Position, Range, Severity, Symbol};
use crate::ports::{CodeIntelError, CodeIntelligencePort, NavigationPort};

/// The marker the fake analyzer treats as an error.
const ERROR_MARKER: &str = "//!ERR";

/// Deterministic in-memory `CodeIntelligencePort`.
pub struct InMemoryCodeIntel {
    /// File extensions (without dot) this fake "supports".
    supported: HashSet<String>,
}

impl InMemoryCodeIntel {
    /// A fake that supports only `.rs` files (matches the MVP single language).
    #[must_use]
    pub fn new() -> Self {
        let mut supported = HashSet::new();
        supported.insert("rs".to_owned());
        Self { supported }
    }

    fn extension(path: &RelativePath) -> Option<String> {
        path.as_str()
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
    }
}

impl Default for InMemoryCodeIntel {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeIntelligencePort for InMemoryCodeIntel {
    fn check(&self, path: &RelativePath, text: &str) -> Result<Vec<Diagnostic>, CodeIntelError> {
        let ext = Self::extension(path).ok_or(CodeIntelError::Unsupported)?;
        if !self.supported.contains(&ext) {
            return Err(CodeIntelError::Unsupported);
        }

        let mut diags = Vec::new();
        for (line_idx, line) in text.lines().enumerate() {
            if let Some(col) = line.find(ERROR_MARKER) {
                let start = u32::try_from(col).unwrap_or(0);
                let end = start + u32::try_from(ERROR_MARKER.len()).unwrap_or(0);
                let line_no = u32::try_from(line_idx).unwrap_or(0);
                diags.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line: line_no,
                            character: start,
                        },
                        end: Position {
                            line: line_no,
                            character: end,
                        },
                    },
                    severity: Severity::Error,
                    message: "marker-triggered error".to_owned(),
                    source: Some("in-memory-fake".to_owned()),
                    code: None,
                });
            }
        }
        Ok(diags)
    }
}

impl InMemoryCodeIntel {
    fn is_supported(&self, path: &RelativePath) -> bool {
        Self::extension(path).is_some_and(|ext| self.supported.contains(&ext))
    }
}

/// A zero-width range at `pos` (used by the fake to echo a queried position).
fn point(pos: Position) -> Range {
    Range {
        start: pos,
        end: pos,
    }
}

impl NavigationPort for InMemoryCodeIntel {
    /// Echoes the queried position back as a single same-file location, so tests
    /// can assert the position flowed through the port unchanged.
    fn definition(
        &self,
        path: &RelativePath,
        _text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        if !self.is_supported(path) {
            return Err(CodeIntelError::Unsupported);
        }
        Ok(vec![Location {
            path: path.clone(),
            range: point(position),
        }])
    }

    /// Two synthetic, distinct reference sites (the queried line and the next).
    fn references(
        &self,
        path: &RelativePath,
        _text: &str,
        position: Position,
    ) -> Result<Vec<Location>, CodeIntelError> {
        if !self.is_supported(path) {
            return Err(CodeIntelError::Unsupported);
        }
        let next = Position {
            line: position.line + 1,
            character: position.character,
        };
        Ok(vec![
            Location {
                path: path.clone(),
                range: point(position),
            },
            Location {
                path: path.clone(),
                range: point(next),
            },
        ])
    }

    fn hover(
        &self,
        path: &RelativePath,
        _text: &str,
        position: Position,
    ) -> Result<Option<Hover>, CodeIntelError> {
        if !self.is_supported(path) {
            return Err(CodeIntelError::Unsupported);
        }
        Ok(Some(Hover {
            contents: format!("symbol at {}:{}", position.line, position.character),
            range: Some(point(position)),
        }))
    }

    /// One `function` symbol per line whose trimmed text starts with `fn `.
    fn document_symbols(
        &self,
        path: &RelativePath,
        text: &str,
    ) -> Result<Vec<Symbol>, CodeIntelError> {
        if !self.is_supported(path) {
            return Err(CodeIntelError::Unsupported);
        }
        let mut symbols = Vec::new();
        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("fn ") {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let line_no = u32::try_from(line_idx).unwrap_or(0);
                let len = u32::try_from(line.len()).unwrap_or(0);
                let full = Range {
                    start: Position {
                        line: line_no,
                        character: 0,
                    },
                    end: Position {
                        line: line_no,
                        character: len,
                    },
                };
                symbols.push(Symbol {
                    name,
                    kind: "function".to_owned(),
                    range: full,
                    selection_range: full,
                });
            }
        }
        Ok(symbols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_line_yields_one_error() {
        let ci = InMemoryCodeIntel::new();
        let diags = ci
            .check(&RelativePath::new("src/a.rs"), "ok\nlet x = 1; //!ERR\nok")
            .unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].range.start.line, 1);
    }

    #[test]
    fn clean_text_yields_nothing() {
        let ci = InMemoryCodeIntel::new();
        let diags = ci
            .check(&RelativePath::new("src/a.rs"), "fn main() {}")
            .unwrap();
        assert!(diags.is_empty());
    }

    #[test]
    fn unsupported_extension_errors() {
        let ci = InMemoryCodeIntel::new();
        let err = ci
            .check(&RelativePath::new("notes.txt"), "//!ERR")
            .unwrap_err();
        assert_eq!(err, CodeIntelError::Unsupported);
    }

    // ── NavigationPort (deterministic fake behaviour) ─────────────────────────

    #[test]
    fn definition_echoes_the_queried_position() {
        let ci = InMemoryCodeIntel::new();
        let pos = Position {
            line: 3,
            character: 7,
        };
        let locs = ci
            .definition(&RelativePath::new("src/a.rs"), "fn main() {}", pos)
            .unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path.as_str(), "src/a.rs");
        assert_eq!(locs[0].range.start, pos);
    }

    #[test]
    fn references_returns_two_distinct_sites() {
        let ci = InMemoryCodeIntel::new();
        let pos = Position {
            line: 2,
            character: 0,
        };
        let refs = ci
            .references(&RelativePath::new("src/a.rs"), "fn main() {}", pos)
            .unwrap();
        assert_eq!(refs.len(), 2);
        assert_ne!(refs[0].range.start.line, refs[1].range.start.line);
    }

    #[test]
    fn hover_returns_some_contents() {
        let ci = InMemoryCodeIntel::new();
        let hover = ci
            .hover(
                &RelativePath::new("src/a.rs"),
                "fn main() {}",
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .unwrap();
        assert!(hover.is_some());
        assert!(!hover.unwrap().contents.is_empty());
    }

    #[test]
    fn document_symbols_lists_fn_declarations() {
        let ci = InMemoryCodeIntel::new();
        let text = "fn alpha() {}\nlet x = 1;\nfn beta() {}\n";
        let symbols = ci
            .document_symbols(&RelativePath::new("src/a.rs"), text)
            .unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(symbols.iter().all(|s| s.kind == "function"));
    }

    #[test]
    fn navigation_on_unsupported_extension_errors() {
        let ci = InMemoryCodeIntel::new();
        let txt = RelativePath::new("notes.txt");
        let pos = Position {
            line: 0,
            character: 0,
        };
        assert_eq!(
            ci.definition(&txt, "x", pos).unwrap_err(),
            CodeIntelError::Unsupported
        );
        assert_eq!(
            ci.document_symbols(&txt, "x").unwrap_err(),
            CodeIntelError::Unsupported
        );
    }
}
