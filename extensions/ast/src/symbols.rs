//! Tree-sitter symbol finder for Rust, Go, and PHP source files.
//!
//! Identical logic to `plugins/ast/src/symbols.rs` but without the
//! `plugin_sdk` dependency. Output is returned as plain Rust structs;
//! JSON conversion uses `serde_json::Value` directly in `tools.rs`.

use tree_sitter::{Language, Node, Parser, Tree};

use crate::text::extract_text;

// ── Public types ──────────────────────────────────────────────────────────────

/// The kind of symbol to search for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Method,
    Module,
    TypeAlias,
    Const,
    Static,
    MacroDef,
    Class,
}

impl SymbolKind {
    /// Parse a kind from the MCP wire-format string label.
    #[must_use]
    pub fn parse_kind(s: &str) -> Option<Self> {
        match s {
            "function" => Some(Self::Function),
            "struct" => Some(Self::Struct),
            "enum" => Some(Self::Enum),
            "trait" => Some(Self::Trait),
            "impl" => Some(Self::Impl),
            "method" => Some(Self::Method),
            "module" => Some(Self::Module),
            "type_alias" => Some(Self::TypeAlias),
            "const" => Some(Self::Const),
            "static" => Some(Self::Static),
            "macro_def" => Some(Self::MacroDef),
            "class" => Some(Self::Class),
            _ => None,
        }
    }

    /// Return the MCP wire-format string label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Method => "method",
            Self::Module => "module",
            Self::TypeAlias => "type_alias",
            Self::Const => "const",
            Self::Static => "static",
            Self::MacroDef => "macro_def",
            Self::Class => "class",
        }
    }
}

/// A single symbol definition found in source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMatch {
    pub kind: SymbolKind,
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

/// The result of a symbol search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolResult {
    Unsupported { language: String },
    NotApplicable,
    Found(Vec<SymbolMatch>),
}

// ── Language enum ─────────────────────────────────────────────────────────────

/// Derive a display label from a file path or bare language id.
#[must_use]
pub fn language_label(hint: &str) -> String {
    match hint.rfind('.') {
        Some(pos) => hint[pos + 1..].to_lowercase(),
        None => hint.to_owned(),
    }
}

/// Supported languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Go,
    Php,
}

impl SupportedLanguage {
    /// Detect language from file path / language hint.
    #[must_use]
    pub fn from_hint(hint: &str) -> Option<Self> {
        let lower = hint.to_lowercase();
        if lower == "rust" || lower.ends_with(".rs") {
            Some(Self::Rust)
        } else if lower == "go" || lower.ends_with(".go") {
            Some(Self::Go)
        } else if lower == "php" || lower.ends_with(".php") {
            Some(Self::Php)
        } else {
            None
        }
    }

    /// Return whether the given `kind` can be defined in this language.
    #[must_use]
    pub fn kind_applicable(self, kind: SymbolKind) -> bool {
        match self {
            Self::Rust => true,
            Self::Go => matches!(
                kind,
                SymbolKind::Function
                    | SymbolKind::Struct
                    | SymbolKind::Trait
                    | SymbolKind::Method
                    | SymbolKind::TypeAlias
                    | SymbolKind::Const
            ),
            Self::Php => matches!(
                kind,
                SymbolKind::Function
                    | SymbolKind::Class
                    | SymbolKind::Trait
                    | SymbolKind::Method
                    | SymbolKind::Const
            ),
        }
    }
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// Resolve symbol spans with an optional kind filter (for `read_symbol`).
pub fn resolve_symbol_spans(
    source: &[u8],
    hint: &str,
    name: &str,
    kind: Option<SymbolKind>,
) -> SymbolResult {
    match kind {
        Some(k) => find_symbols(source, hint, name, k),
        None => {
            let lang = match SupportedLanguage::from_hint(hint) {
                Some(l) => l,
                None => {
                    return SymbolResult::Unsupported {
                        language: language_label(hint),
                    };
                }
            };

            let all_kinds = [
                SymbolKind::Function,
                SymbolKind::Struct,
                SymbolKind::Enum,
                SymbolKind::Trait,
                SymbolKind::Impl,
                SymbolKind::Method,
                SymbolKind::Module,
                SymbolKind::TypeAlias,
                SymbolKind::Const,
                SymbolKind::Static,
                SymbolKind::MacroDef,
                SymbolKind::Class,
            ];

            // Parse ONCE and reuse the tree across every applicable kind. Calling
            // `find_symbols` per kind re-parsed the file ~12 times — and the parse
            // dominates the cost. The walk over an already-parsed tree is cheap, so
            // this collapses the no-`kind` path to a single parse. Results are
            // identical (same per-kind walks, same start_byte ordering).
            let tree = parse_source(lang, source);
            let root = tree.root_node();
            let mut merged: Vec<SymbolMatch> = Vec::new();
            for k in all_kinds {
                if lang.kind_applicable(k) {
                    walk_for_kind(lang, root, source, name, k, &mut merged);
                }
            }
            merged.sort_by_key(|m| m.start_byte);
            SymbolResult::Found(merged)
        }
    }
}

/// Search `source` for definitions of `symbol_name` with the given `kind`.
pub fn find_symbols(
    source: &[u8],
    language_hint: &str,
    symbol_name: &str,
    kind: SymbolKind,
) -> SymbolResult {
    let lang = match SupportedLanguage::from_hint(language_hint) {
        Some(l) => l,
        None => {
            return SymbolResult::Unsupported {
                language: language_label(language_hint),
            };
        }
    };

    if !lang.kind_applicable(kind) {
        return SymbolResult::NotApplicable;
    }

    let tree = parse_source(lang, source);
    let mut matches = Vec::new();
    walk_for_kind(
        lang,
        tree.root_node(),
        source,
        symbol_name,
        kind,
        &mut matches,
    );
    SymbolResult::Found(matches)
}

/// The tree-sitter [`Language`] for a supported language.
fn ts_language(lang: SupportedLanguage) -> Language {
    match lang {
        SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        SupportedLanguage::Php => tree_sitter_php::LANGUAGE_PHP.into(),
    }
}

/// Parse `source` for `lang`, recovering with an empty tree on parse failure.
fn parse_source(lang: SupportedLanguage, source: &[u8]) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&ts_language(lang))
        .expect("tree-sitter language version mismatch");
    parser
        .parse(source, None)
        .unwrap_or_else(|| parser.parse(b"", None).expect("empty source must parse"))
}

/// Walk an already-parsed `root`, collecting matches of `symbol_name` for a
/// single `kind`. Factored out so callers can parse once and walk many kinds.
fn walk_for_kind(
    lang: SupportedLanguage,
    root: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    match lang {
        SupportedLanguage::Rust => walk_rust_symbols(root, source, symbol_name, kind, matches),
        SupportedLanguage::Go => walk_go_symbols(root, source, symbol_name, kind, matches),
        SupportedLanguage::Php => walk_php_symbols(root, source, symbol_name, kind, matches),
    }
}

// ── Rust walker ───────────────────────────────────────────────────────────────

fn walk_rust_symbols(
    root: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_rust_node(child, source, symbol_name, kind, matches);
    }
}

fn visit_rust_node(
    child: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    match (kind, child.kind()) {
        (SymbolKind::Function, "function_item")
        | (SymbolKind::Struct, "struct_item")
        | (SymbolKind::Enum, "enum_item")
        | (SymbolKind::Trait, "trait_item")
        | (SymbolKind::TypeAlias, "type_item")
        | (SymbolKind::Const, "const_item")
        | (SymbolKind::Static, "static_item")
        | (SymbolKind::MacroDef, "macro_definition") => {
            if let Some(m) = match_named_item(child, source, symbol_name, kind) {
                matches.push(m);
            }
        }
        (SymbolKind::Module, "mod_item") => {
            if let Some(m) = match_named_item(child, source, symbol_name, kind) {
                matches.push(m);
            }
            walk_rust_mod_body(child, source, symbol_name, kind, matches);
        }
        (_, "mod_item") => {
            walk_rust_mod_body(child, source, symbol_name, kind, matches);
        }
        (SymbolKind::Impl, "impl_item") => {
            if let Some(name_text) = impl_name(child, source)
                && name_text == symbol_name
            {
                matches.push(node_to_match(child, name_text, kind));
            }
        }
        (SymbolKind::Method, "impl_item") => {
            let body = child
                .children(&mut child.walk())
                .find(|n| n.kind() == "declaration_list");
            if let Some(body) = body {
                let mut bcursor = body.walk();
                for method in body.children(&mut bcursor) {
                    if method.kind() == "function_item"
                        && let Some(m) =
                            match_named_item(method, source, symbol_name, SymbolKind::Method)
                    {
                        matches.push(m);
                    }
                }
            }
        }
        _ => {}
    }
}

fn walk_rust_mod_body(
    mod_node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    let body = mod_node
        .children(&mut mod_node.walk())
        .find(|n| n.kind() == "declaration_list");

    let Some(body) = body else { return };

    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        visit_rust_node(child, source, symbol_name, kind, matches);
    }
}

fn impl_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.child_by_field_name("type")
        .and_then(|n| extract_text(n, source))
}

// ── Go walker ─────────────────────────────────────────────────────────────────

fn walk_go_symbols(
    root: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_declaration" if matches!(kind, SymbolKind::Function) => {
                if let Some(m) = match_named_item(child, source, symbol_name, SymbolKind::Function)
                {
                    matches.push(m);
                }
            }
            "method_declaration" if matches!(kind, SymbolKind::Method) => {
                if let Some(m) = match_named_item(child, source, symbol_name, SymbolKind::Method) {
                    matches.push(m);
                }
            }
            "type_declaration" => {
                walk_go_type_decl(child, source, symbol_name, kind, matches);
            }
            "const_declaration" if matches!(kind, SymbolKind::Const) => {
                walk_go_const_decl(child, source, symbol_name, matches);
            }
            _ => {}
        }
    }
}

fn walk_go_type_decl(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "type_spec" {
            continue;
        }
        let detected_kind = go_type_spec_kind(child);
        if detected_kind == Some(kind)
            && let Some(m) = match_named_item(child, source, symbol_name, kind)
        {
            matches.push(m);
        }
    }
}

fn go_type_spec_kind(node: Node<'_>) -> Option<SymbolKind> {
    let type_child = node.child_by_field_name("type")?;
    match type_child.kind() {
        "struct_type" => Some(SymbolKind::Struct),
        "interface_type" => Some(SymbolKind::Trait),
        _ => {
            let is_alias = node
                .children(&mut node.walk())
                .any(|c| c.kind() == "=" && !c.is_named());
            if is_alias {
                Some(SymbolKind::TypeAlias)
            } else {
                Some(SymbolKind::Struct)
            }
        }
    }
}

fn walk_go_const_decl(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "const_spec" || child.is_error() {
            continue;
        }
        let mut name_cursor = child.walk();
        for name_node in child.children_by_field_name("name", &mut name_cursor) {
            let Some(name_text) = extract_text(name_node, source) else {
                continue;
            };
            if name_text == symbol_name {
                matches.push(node_to_match(child, name_text, SymbolKind::Const));
            }
        }
    }
}

// ── PHP walker ────────────────────────────────────────────────────────────────

fn walk_php_symbols(
    root: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    walk_php_node(root, source, symbol_name, kind, matches);
}

fn walk_php_node(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" if matches!(kind, SymbolKind::Function) => {
                if let Some(m) = match_named_item(child, source, symbol_name, SymbolKind::Function)
                {
                    matches.push(m);
                }
            }
            "class_declaration" => {
                if matches!(kind, SymbolKind::Class)
                    && let Some(m) = match_named_item(child, source, symbol_name, SymbolKind::Class)
                {
                    matches.push(m);
                }
                if matches!(kind, SymbolKind::Method | SymbolKind::Const) {
                    walk_php_node(child, source, symbol_name, kind, matches);
                }
            }
            "interface_declaration" | "trait_declaration" => {
                if matches!(kind, SymbolKind::Trait)
                    && let Some(m) = match_named_item(child, source, symbol_name, SymbolKind::Trait)
                {
                    matches.push(m);
                }
                if matches!(kind, SymbolKind::Method) {
                    walk_php_node(child, source, symbol_name, kind, matches);
                }
            }
            "method_declaration" if matches!(kind, SymbolKind::Method) => {
                if let Some(m) = match_named_item(child, source, symbol_name, SymbolKind::Method) {
                    matches.push(m);
                }
            }
            "declaration_list" => {
                walk_php_node(child, source, symbol_name, kind, matches);
            }
            "const_declaration" if matches!(kind, SymbolKind::Const) => {
                walk_php_const(child, source, symbol_name, matches);
            }
            _ => {}
        }
    }
}

fn walk_php_const(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "const_element" || child.is_error() {
            continue;
        }
        let mut elem_cursor = child.walk();
        let name_node = child
            .children(&mut elem_cursor)
            .find(|c| c.kind() == "name" && c.is_named());
        let Some(name_node) = name_node else {
            continue;
        };
        let Some(name_text) = extract_text(name_node, source) else {
            continue;
        };
        if name_text == symbol_name {
            matches.push(node_to_match(child, name_text, SymbolKind::Const));
        }
    }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn match_named_item(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
) -> Option<SymbolMatch> {
    if node.is_error() {
        return None;
    }
    let name_node = node.child_by_field_name("name")?;
    let name_text = extract_text(name_node, source)?;
    if name_text != symbol_name {
        return None;
    }
    Some(node_to_match(node, name_text, kind))
}

fn node_to_match(node: Node<'_>, name: String, kind: SymbolKind) -> SymbolMatch {
    let start = node.start_position();
    let end = node.end_position();
    SymbolMatch {
        kind,
        name,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_row: start.row,
        start_col: start.column,
        end_row: end.row,
        end_col: end.column,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_FIXTURE: &[u8] = br#"
pub struct FindMe { value: u32 }
impl FindMe {
    pub fn find_me() -> Self { Self { value: 0 } }
}
pub fn find_me() -> u32 { 42 }
"#;

    #[test]
    fn rust_finds_struct_definition() {
        let result = find_symbols(RUST_FIXTURE, "test.rs", "FindMe", SymbolKind::Struct);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "FindMe");
        assert_eq!(matches[0].kind, SymbolKind::Struct);
    }

    #[test]
    fn rust_finds_function_not_method() {
        let result = find_symbols(RUST_FIXTURE, "test.rs", "find_me", SymbolKind::Function);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, SymbolKind::Function);
    }

    #[test]
    fn unsupported_language_returns_unsupported() {
        let result = find_symbols(b"", "test.py", "foo", SymbolKind::Function);
        assert!(
            matches!(result, SymbolResult::Unsupported { .. }),
            "Python must be Unsupported"
        );
    }

    #[test]
    fn inapplicable_kind_returns_not_applicable() {
        // SymbolKind::Impl is not applicable in Go.
        let result = find_symbols(b"package main", "test.go", "Foo", SymbolKind::Impl);
        assert!(
            matches!(result, SymbolResult::NotApplicable),
            "Impl must be NotApplicable for Go"
        );
    }

    /// The no-`kind` path (parse-once, walk every kind) must return the union of
    /// the per-kind searches, ordered by start_byte. `find_me` exists in the
    /// fixture as BOTH a method and a free function, so both must surface.
    #[test]
    fn no_kind_resolve_merges_all_kinds_like_per_kind_union() {
        let merged = match resolve_symbol_spans(RUST_FIXTURE, "test.rs", "find_me", None) {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        let kinds: Vec<SymbolKind> = merged.iter().map(|m| m.kind).collect();
        assert!(
            kinds.contains(&SymbolKind::Method) && kinds.contains(&SymbolKind::Function),
            "no-kind resolve must surface both the method and the function, got {kinds:?}"
        );
        assert!(
            merged
                .windows(2)
                .all(|w| w[0].start_byte <= w[1].start_byte),
            "matches must be ordered by start_byte"
        );

        // Equivalence: the optimisation must not change results vs the explicit
        // per-kind union it replaced.
        let per_kind = |k| match find_symbols(RUST_FIXTURE, "test.rs", "find_me", k) {
            SymbolResult::Found(m) => m,
            _ => Vec::new(),
        };
        assert_eq!(
            merged.len(),
            per_kind(SymbolKind::Function).len() + per_kind(SymbolKind::Method).len()
        );
    }
}
