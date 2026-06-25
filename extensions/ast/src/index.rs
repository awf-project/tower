//! In-memory symbol index for the `ast` extension (spec 26).
//!
//! Same design as `plugins/ast/src/index.rs` but without `plugin_sdk`.
//! Serialised to postcard for persistence via `index/put` / `index/get`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::outline::{Outline, OutlineItem};

// ── SymbolEntry ───────────────────────────────────────────────────────────────

/// A single symbol stored in the cross-file index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub path: String,
    pub kind: String,
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    #[serde(default)]
    pub body_start_byte: Option<usize>,
    #[serde(default)]
    pub body_end_byte: Option<usize>,
}

impl SymbolEntry {
    /// Convert to a `serde_json::Value` map for the MCP wire format.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path":       self.path,
            "kind":       self.kind,
            "name":       self.name,
            "start_byte": self.start_byte,
            "end_byte":   self.end_byte,
            "start_row":  self.start_row,
            "start_col":  self.start_col,
            "end_row":    self.end_row,
            "end_col":    self.end_col,
        })
    }
}

// ── SymbolIndex ───────────────────────────────────────────────────────────────

/// Cross-file symbol index backed by `BTreeMap<path, entries>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SymbolIndex {
    pub by_path: BTreeMap<String, Vec<SymbolEntry>>,
}

impl SymbolIndex {
    /// Replace all index entries for `path` with the symbols in `outline`.
    pub fn index_file(&mut self, path: &str, outline: &Outline) {
        let entries: Vec<SymbolEntry> = outline
            .items
            .iter()
            .map(|item| entry_from_item(path, item))
            .collect();
        self.by_path.insert(path.to_owned(), entries);
    }

    /// Remove all index entries for `path`.
    pub fn remove_file(&mut self, path: &str) {
        self.by_path.remove(path);
    }

    /// Search for symbols with `name` and optional `kind` filter.
    #[must_use]
    pub fn search<'a>(&'a self, name: &str, kind: Option<&str>) -> Vec<&'a SymbolEntry> {
        self.by_path
            .values()
            .flatten()
            .filter(|e| e.name == name && kind.is_none_or(|k| e.kind == k))
            .collect()
    }

    /// Serialize to postcard bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).unwrap_or_default()
    }

    /// Deserialize from postcard bytes; returns `None` on failure.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

// ── Private helper ────────────────────────────────────────────────────────────

fn entry_from_item(path: &str, item: &OutlineItem) -> SymbolEntry {
    SymbolEntry {
        path: path.to_owned(),
        kind: item.kind.as_str().to_owned(),
        name: item.name.clone(),
        start_byte: item.span.start_byte,
        end_byte: item.span.end_byte,
        start_row: item.span.start_row,
        start_col: item.span.start_col,
        end_row: item.span.end_row,
        end_col: item.span.end_col,
        body_start_byte: item.body_span.as_ref().map(|span| span.start_byte),
        body_end_byte: item.body_span.as_ref().map(|span| span.end_byte),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::{OutlineKind, Span};

    fn make_outline(items: Vec<(&str, &str)>) -> Outline {
        Outline {
            items: items
                .into_iter()
                .map(|(kind_str, name)| {
                    let kind = match kind_str {
                        "function" => OutlineKind::Function,
                        "struct" => OutlineKind::Struct,
                        "method" => OutlineKind::Method,
                        _ => OutlineKind::Function,
                    };
                    OutlineItem {
                        kind,
                        name: name.to_owned(),
                        span: Span {
                            start_byte: 0,
                            end_byte: 10,
                            start_row: 0,
                            start_col: 0,
                            end_row: 0,
                            end_col: 10,
                        },
                        body_span: None,
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn index_file_and_search() {
        let mut idx = SymbolIndex::default();
        let outline = make_outline(vec![("function", "my_fn"), ("struct", "MyStruct")]);
        idx.index_file("src/lib.rs", &outline);

        let hits = idx.search("my_fn", Some("function"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "my_fn");
        assert_eq!(hits[0].path, "src/lib.rs");
    }

    #[test]
    fn remove_file_clears_entries() {
        let mut idx = SymbolIndex::default();
        let outline = make_outline(vec![("function", "foo")]);
        idx.index_file("src/a.rs", &outline);
        idx.remove_file("src/a.rs");
        assert!(idx.search("foo", None).is_empty());
    }

    #[test]
    fn postcard_roundtrip() {
        let mut idx = SymbolIndex::default();
        let outline = make_outline(vec![("function", "bar")]);
        idx.index_file("src/b.rs", &outline);
        let bytes = idx.to_bytes();
        let restored = SymbolIndex::from_bytes(&bytes).expect("roundtrip must succeed");
        assert_eq!(idx, restored);
    }

    #[test]
    fn search_no_kind_filter_returns_all() {
        let mut idx = SymbolIndex::default();
        let outline = make_outline(vec![("function", "alpha"), ("struct", "alpha")]);
        idx.index_file("src/c.rs", &outline);
        let hits = idx.search("alpha", None);
        assert_eq!(hits.len(), 2);
    }
}
