//! In-memory symbol index for the AST plugin (spec 12d).
//!
//! # Design
//!
//! ```text
//! SymbolIndex { by_path: BTreeMap<String, Vec<SymbolEntry>> }
//!   index_file(path, outline)   → replace all entries for path
//!   remove_file(path)           → drop all entries for path
//!   search(name, kind?)         → cross-file exact-name match, optional kind filter
//!   to_bytes() / from_bytes()   → postcard round-trip for persistence
//! ```
//!
//! Persistence is via `host::ast_store_put/get("symbols")`. The full blob is
//! rewritten on every change — acceptable for v1 where the index is small.
//!
//! Host-side unit tests cover all pure methods; no WASI runtime is required.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::outline::{Outline, OutlineItem};

// ── SymbolEntry ───────────────────────────────────────────────────────────────

/// A single symbol stored in the cross-file index.
///
/// Fields mirror [`crate::symbols::SymbolMatch`] plus a `path` so callers
/// can locate the file that contains the symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    /// Workspace-relative path of the file containing this symbol.
    pub path: String,
    /// Symbol kind string (e.g. `"function"`, `"struct"`) — same labels as
    /// [`crate::outline::OutlineKind::as_str`].
    pub kind: String,
    /// Symbol name.
    pub name: String,
    /// Inclusive start byte offset in the source file.
    pub start_byte: usize,
    /// Exclusive end byte offset in the source file.
    pub end_byte: usize,
    /// Zero-based start row.
    pub start_row: usize,
    /// Zero-based start column.
    pub start_col: usize,
    /// Zero-based end row.
    pub end_row: usize,
    /// Zero-based end column.
    pub end_col: usize,
}

impl SymbolEntry {
    /// Convert to a [`plugin_sdk::Value::Map`] for the MCP wire format.
    ///
    /// Includes the `path` field so callers know which file to open.
    #[must_use]
    pub fn to_sdk_value(&self) -> plugin_sdk::Value {
        plugin_sdk::Value::Map(vec![
            (
                "path".to_owned(),
                plugin_sdk::Value::Text(self.path.clone()),
            ),
            (
                "kind".to_owned(),
                plugin_sdk::Value::Text(self.kind.clone()),
            ),
            (
                "name".to_owned(),
                plugin_sdk::Value::Text(self.name.clone()),
            ),
            (
                "start_byte".to_owned(),
                plugin_sdk::Value::Integer(self.start_byte as i64),
            ),
            (
                "end_byte".to_owned(),
                plugin_sdk::Value::Integer(self.end_byte as i64),
            ),
            (
                "start_row".to_owned(),
                plugin_sdk::Value::Integer(self.start_row as i64),
            ),
            (
                "start_col".to_owned(),
                plugin_sdk::Value::Integer(self.start_col as i64),
            ),
            (
                "end_row".to_owned(),
                plugin_sdk::Value::Integer(self.end_row as i64),
            ),
            (
                "end_col".to_owned(),
                plugin_sdk::Value::Integer(self.end_col as i64),
            ),
        ])
    }
}

// ── SymbolIndex ───────────────────────────────────────────────────────────────

/// Cross-file symbol index backed by a `BTreeMap<path, entries>`.
///
/// Keyed by path so updating or removing a single file is O(log n) on the
/// number of distinct paths. The index is persisted as a postcard blob via
/// `host::ast_store_put("symbols")` after every mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SymbolIndex {
    /// Per-file symbol lists. `BTreeMap` gives deterministic iteration order
    /// which keeps postcard blobs stable across identical input sequences.
    pub by_path: BTreeMap<String, Vec<SymbolEntry>>,
}

impl SymbolIndex {
    /// Replace all index entries for `path` with the symbols in `outline`.
    ///
    /// If the file was previously indexed, the old entries are discarded
    /// before the new ones are inserted — no duplication across re-indexes.
    pub fn index_file(&mut self, path: &str, outline: &Outline) {
        let entries: Vec<SymbolEntry> = outline
            .items
            .iter()
            .map(|item| entry_from_item(path, item))
            .collect();
        self.by_path.insert(path.to_owned(), entries);
    }

    /// Remove all index entries for `path` (e.g. file deleted / not found).
    ///
    /// No-op if `path` was not in the index.
    pub fn remove_file(&mut self, path: &str) {
        self.by_path.remove(path);
    }

    /// Search all indexed files for symbols whose name exactly matches `name`.
    ///
    /// If `kind` is `Some`, only entries whose `kind` field equals the given
    /// string are returned. If `kind` is `None`, all kinds are returned.
    ///
    /// # Performance
    ///
    /// // perf: O(n) scan across all entries, acceptable for v1.
    /// A future revision can add a `by_name` secondary index if the workspace
    /// grows large enough to make scanning a bottleneck.
    #[must_use]
    pub fn search<'a>(&'a self, name: &str, kind: Option<&str>) -> Vec<&'a SymbolEntry> {
        self.by_path
            .values()
            .flatten()
            .filter(|e| e.name == name && kind.is_none_or(|k| e.kind == k))
            .collect()
    }

    /// Serialise the index to a postcard byte buffer.
    ///
    /// Returns an empty `Vec` if serialisation fails (should not happen for
    /// well-formed data — postcard infallible for serde-derived structs).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).unwrap_or_default()
    }

    /// Deserialise a `SymbolIndex` from a postcard byte buffer.
    ///
    /// Returns `None` if the bytes are corrupt or were produced by an
    /// incompatible serialisation layout (e.g. after a schema change). The
    /// caller should fall back to `SymbolIndex::default()` in that case.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Build a [`SymbolEntry`] from an [`OutlineItem`] + the file path.
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
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::{OutlineKind, Span};

    // Helper: build a minimal Outline with one item for unit tests.
    fn single_item_outline(kind: OutlineKind, name: &str) -> Outline {
        Outline {
            items: vec![OutlineItem {
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
            }],
        }
    }

    // ── index_file / search ───────────────────────────────────────────────────

    #[test]
    fn index_file_then_search_finds_entry() {
        let mut idx = SymbolIndex::default();
        let outline = single_item_outline(OutlineKind::Function, "my_fn");
        idx.index_file("src/lib.rs", &outline);

        let results = idx.search("my_fn", None);
        assert_eq!(results.len(), 1, "must find exactly one entry");
        assert_eq!(results[0].name, "my_fn");
        assert_eq!(results[0].path, "src/lib.rs");
        assert_eq!(results[0].kind, "function");
    }

    #[test]
    fn search_with_kind_filter_finds_matching_kind() {
        let mut idx = SymbolIndex::default();
        let fn_outline = single_item_outline(OutlineKind::Function, "foo");
        let struct_outline = single_item_outline(OutlineKind::Struct, "foo");
        idx.index_file("a.rs", &fn_outline);
        idx.index_file("b.rs", &struct_outline);

        let fns = idx.search("foo", Some("function"));
        assert_eq!(fns.len(), 1, "kind filter must exclude struct");
        assert_eq!(fns[0].kind, "function");

        let structs = idx.search("foo", Some("struct"));
        assert_eq!(structs.len(), 1, "kind filter must exclude function");
        assert_eq!(structs[0].kind, "struct");

        let all = idx.search("foo", None);
        assert_eq!(all.len(), 2, "no kind filter returns both");
    }

    #[test]
    fn search_absent_name_returns_empty() {
        let mut idx = SymbolIndex::default();
        idx.index_file(
            "src/lib.rs",
            &single_item_outline(OutlineKind::Struct, "Bar"),
        );

        let results = idx.search("NoSuch", None);
        assert!(results.is_empty(), "absent name must return empty");
    }

    // ── remove_file ───────────────────────────────────────────────────────────

    #[test]
    fn remove_file_drops_entries() {
        let mut idx = SymbolIndex::default();
        idx.index_file(
            "src/lib.rs",
            &single_item_outline(OutlineKind::Struct, "Foo"),
        );
        idx.remove_file("src/lib.rs");

        let results = idx.search("Foo", None);
        assert!(results.is_empty(), "entries must be gone after remove_file");
    }

    #[test]
    fn remove_file_noop_for_unknown_path() {
        let mut idx = SymbolIndex::default();
        // Must not panic.
        idx.remove_file("does/not/exist.rs");
        assert!(idx.by_path.is_empty());
    }

    // ── re-index replaces (no duplication) ───────────────────────────────────

    #[test]
    fn reindex_same_path_replaces_not_duplicates() {
        let mut idx = SymbolIndex::default();
        idx.index_file(
            "src/lib.rs",
            &single_item_outline(OutlineKind::Function, "old_fn"),
        );
        idx.index_file(
            "src/lib.rs",
            &single_item_outline(OutlineKind::Function, "new_fn"),
        );

        // old_fn must be gone; new_fn must be present; exactly one entry total.
        assert!(
            idx.search("old_fn", None).is_empty(),
            "old entries must be replaced"
        );
        let new_results = idx.search("new_fn", None);
        assert_eq!(new_results.len(), 1, "new entry must appear exactly once");

        let all: Vec<&SymbolEntry> = idx.by_path.values().flatten().collect();
        assert_eq!(all.len(), 1, "no duplicate entries after re-index");
    }

    // ── postcard round-trip ───────────────────────────────────────────────────

    #[test]
    fn to_bytes_from_bytes_round_trip() {
        let mut idx = SymbolIndex::default();
        idx.index_file(
            "src/lib.rs",
            &single_item_outline(OutlineKind::Trait, "MyTrait"),
        );
        idx.index_file(
            "src/other.rs",
            &single_item_outline(OutlineKind::Struct, "MyStruct"),
        );

        let bytes = idx.to_bytes();
        assert!(!bytes.is_empty(), "serialised index must not be empty");

        let restored = SymbolIndex::from_bytes(&bytes).expect("round-trip must succeed");
        assert_eq!(restored, idx, "round-trip must preserve all entries");
    }

    #[test]
    fn from_bytes_corrupt_returns_none() {
        let result = SymbolIndex::from_bytes(b"not valid postcard");
        assert!(result.is_none(), "corrupt bytes must return None");
    }

    #[test]
    fn empty_index_round_trip() {
        let idx = SymbolIndex::default();
        let bytes = idx.to_bytes();
        let restored = SymbolIndex::from_bytes(&bytes).expect("empty index must round-trip");
        assert!(restored.by_path.is_empty());
    }

    // ── end-to-end with real parse_outline output ─────────────────────────────

    #[test]
    fn index_from_real_parse_outline_then_search() {
        // Use a real Rust fixture through parse_outline to exercise the full
        // pure path: tree-sitter parse → Outline → SymbolIndex → search.
        let source = br#"
pub struct RealStruct {
    x: u32,
}
impl RealStruct {
    pub fn real_method(&self) {}
}
pub fn real_fn() {}
pub trait RealTrait {}
"#;

        let outline = match crate::outline::parse_outline(source, "fixture.rs") {
            crate::outline::OutlineResult::Parsed(o) => o,
            crate::outline::OutlineResult::Unsupported { language } => {
                panic!("unexpected Unsupported({language})")
            }
        };

        let mut idx = SymbolIndex::default();
        idx.index_file("fixture.rs", &outline);

        // Each symbol kind must be findable.
        assert!(
            !idx.search("RealStruct", Some("struct")).is_empty(),
            "RealStruct struct must be indexed"
        );
        assert!(
            !idx.search("real_fn", Some("function")).is_empty(),
            "real_fn function must be indexed"
        );
        assert!(
            !idx.search("real_method", Some("method")).is_empty(),
            "real_method method must be indexed"
        );
        assert!(
            !idx.search("RealTrait", Some("trait")).is_empty(),
            "RealTrait trait must be indexed"
        );

        // kind filter must work: RealStruct with wrong kind returns nothing.
        assert!(
            idx.search("RealStruct", Some("function")).is_empty(),
            "wrong kind filter must return empty"
        );

        // Absent name returns empty.
        assert!(
            idx.search("NoSuchSymbol", None).is_empty(),
            "absent symbol must not be found"
        );
    }
}
