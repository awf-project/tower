//! Tree-sitter outline walker for Rust, Go, and PHP source files.
//!
//! Identical logic to `plugins/ast/src/outline.rs` but without the
//! `plugin_sdk` dependency. Output types are plain Rust structs.

use tree_sitter::{Language, Node, Parser};

use crate::symbols::SupportedLanguage;
use crate::text::extract_text;

// ── Public types ──────────────────────────────────────────────────────────────

/// The kind of a structural outline item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutlineKind {
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

impl OutlineKind {
    /// Return the string label used in the JSON wire format.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
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

/// Byte-offset span of an outline item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

/// A single structural item in the file outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineItem {
    pub kind: OutlineKind,
    pub name: String,
    pub span: Span,
}

/// The full outline of a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    pub items: Vec<OutlineItem>,
}

/// The result of an outline extraction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutlineResult {
    Unsupported { language: String },
    Parsed(Outline),
}

// ── Language label helper ─────────────────────────────────────────────────────

fn language_label(hint: &str) -> String {
    match hint.rfind('.') {
        Some(pos) => hint[pos + 1..].to_lowercase(),
        None => hint.to_owned(),
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Parse `source` and extract a structural outline.
///
/// `language_hint` is a file path or language name. Supported: Rust, Go, PHP.
pub fn parse_outline(source: &[u8], language_hint: &str) -> OutlineResult {
    let lang = match SupportedLanguage::from_hint(language_hint) {
        Some(l) => l,
        None => {
            return OutlineResult::Unsupported {
                language: language_label(language_hint),
            };
        }
    };

    let ts_language: Language = match lang {
        SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        SupportedLanguage::Php => tree_sitter_php::LANGUAGE_PHP.into(),
    };

    let mut parser = Parser::new();
    parser
        .set_language(&ts_language)
        .expect("tree-sitter language version mismatch");

    let tree = parser
        .parse(source, None)
        .unwrap_or_else(|| parser.parse(b"", None).expect("empty source must parse"));

    let root = tree.root_node();
    let mut items = Vec::new();

    match lang {
        SupportedLanguage::Rust => walk_top_level(root, source, &mut items),
        SupportedLanguage::Go => walk_go_top_level(root, source, &mut items),
        SupportedLanguage::Php => walk_php_top_level(root, source, &mut items),
    }

    OutlineResult::Parsed(Outline { items })
}

// ── Rust walker ───────────────────────────────────────────────────────────────

fn walk_top_level(node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Function) {
                    items.push(item);
                }
            }
            "struct_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Struct) {
                    items.push(item);
                }
            }
            "enum_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Enum) {
                    items.push(item);
                }
            }
            "trait_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Trait) {
                    items.push(item);
                }
            }
            "impl_item" => {
                let impl_item = extract_impl_item(child, source);
                items.push(impl_item);
                walk_impl_body(child, source, items);
            }
            "mod_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Module) {
                    items.push(item);
                }
                walk_mod_body(child, source, items);
            }
            "type_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::TypeAlias) {
                    items.push(item);
                }
            }
            "const_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Const) {
                    items.push(item);
                }
            }
            "static_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Static) {
                    items.push(item);
                }
            }
            "macro_definition" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::MacroDef) {
                    items.push(item);
                }
            }
            _ => {}
        }
    }
}

fn walk_mod_body(mod_node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    let body = mod_node
        .children(&mut mod_node.walk())
        .find(|n| n.kind() == "declaration_list");
    let Some(body) = body else { return };
    walk_top_level(body, source, items);
}

fn walk_impl_body(impl_node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    let body = impl_node
        .children(&mut impl_node.walk())
        .find(|n| n.kind() == "declaration_list");
    let Some(body) = body else { return };

    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "function_item"
            && let Some(item) = extract_named_item(child, source, OutlineKind::Method)
        {
            items.push(item);
        }
    }
}

fn extract_named_item(node: Node<'_>, source: &[u8], kind: OutlineKind) -> Option<OutlineItem> {
    if node.is_error() {
        return None;
    }
    let name = node
        .child_by_field_name("name")
        .and_then(|n| extract_text(n, source))
        .unwrap_or_else(|| "<anonymous>".to_owned());
    Some(OutlineItem {
        kind,
        name,
        span: node_span(node),
    })
}

fn extract_impl_item(node: Node<'_>, source: &[u8]) -> OutlineItem {
    let type_text = node
        .child_by_field_name("type")
        .and_then(|n| extract_text(n, source));

    let trait_text = node.child_by_field_name("trait").and_then(|trait_node| {
        let base = extract_text(trait_node, source)?;
        let is_negative = trait_node
            .prev_sibling()
            .is_some_and(|prev| prev.kind() == "!");
        if is_negative {
            Some(format!("!{base}"))
        } else {
            Some(base)
        }
    });

    let name = match (trait_text, type_text) {
        (Some(tr), Some(ty)) => format!("{tr} for {ty}"),
        (None, Some(ty)) => ty,
        (Some(tr), None) => tr,
        (None, None) => "<anonymous>".to_owned(),
    };

    OutlineItem {
        kind: OutlineKind::Impl,
        name,
        span: node_span(node),
    }
}

fn node_span(node: Node<'_>) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_row: start.row,
        start_col: start.column,
        end_row: end.row,
        end_col: end.column,
    }
}

// ── Go walker ─────────────────────────────────────────────────────────────────

fn walk_go_top_level(node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Function) {
                    items.push(item);
                }
            }
            "method_declaration" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Method) {
                    items.push(item);
                }
            }
            "type_declaration" => {
                walk_go_type_decl(child, source, items);
            }
            _ => {}
        }
    }
}

fn walk_go_type_decl(node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "type_spec" || child.is_error() {
            continue;
        }
        let type_field = child.child_by_field_name("type");
        let kind = match type_field.as_ref().map(Node::kind) {
            Some("interface_type") => OutlineKind::Trait,
            _ => OutlineKind::Struct,
        };
        if let Some(item) = extract_named_item(child, source, kind) {
            items.push(item);
        }
    }
}

// ── PHP walker ────────────────────────────────────────────────────────────────

fn walk_php_top_level(node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    walk_php_node_outline(node, source, items);
}

fn walk_php_node_outline(node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Function) {
                    items.push(item);
                }
            }
            "class_declaration" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Class) {
                    items.push(item);
                }
                walk_php_node_outline(child, source, items);
            }
            "interface_declaration" | "trait_declaration" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Trait) {
                    items.push(item);
                }
                walk_php_node_outline(child, source, items);
            }
            "method_declaration" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Method) {
                    items.push(item);
                }
            }
            "declaration_list" => {
                walk_php_node_outline(child, source, items);
            }
            _ => {}
        }
    }
}

// ── Conversion: OutlineItem → serde_json::Value ───────────────────────────────

impl OutlineItem {
    /// Convert to a `serde_json::Value` map for the MCP wire format.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.as_str(),
            "name": self.name,
            "start_byte": self.span.start_byte,
            "end_byte": self.span.end_byte,
            "start_row": self.span.start_row,
            "start_col": self.span.start_col,
            "end_row": self.span.end_row,
            "end_col": self.span.end_col,
        })
    }
}

impl Outline {
    /// Convert to a `serde_json::Value` map: `{ "items": [...] }`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let items: Vec<serde_json::Value> = self.items.iter().map(OutlineItem::to_json).collect();
        serde_json::json!({ "items": items })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_FIXTURE: &[u8] = br#"
pub struct MyStruct { field: u32 }
impl MyStruct {
    pub fn new() -> Self { Self { field: 0 } }
}
pub fn standalone_fn() {}
pub trait MyTrait { fn do_thing(&self); }
pub enum Color { Red, Green }
"#;

    #[test]
    fn rust_outline_returns_struct_impl_methods_fn_trait_enum() {
        let result = parse_outline(RUST_FIXTURE, "test.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { language } => {
                panic!("expected Parsed, got Unsupported({language})")
            }
        };
        let items: Vec<(&str, &str)> = outline
            .items
            .iter()
            .map(|i| (i.kind.as_str(), i.name.as_str()))
            .collect();

        assert!(items.iter().any(|&(k, n)| k == "struct" && n == "MyStruct"));
        assert!(items.iter().any(|&(k, n)| k == "impl" && n == "MyStruct"));
        assert!(items.iter().any(|&(k, n)| k == "method" && n == "new"));
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "function" && n == "standalone_fn")
        );
        assert!(items.iter().any(|&(k, n)| k == "trait" && n == "MyTrait"));
        assert!(items.iter().any(|&(k, n)| k == "enum" && n == "Color"));
    }

    #[test]
    fn unsupported_language_returns_unsupported() {
        let result = parse_outline(b"print('hello')", "test.py");
        assert!(
            matches!(result, OutlineResult::Unsupported { .. }),
            "Python must be Unsupported"
        );
    }

    #[test]
    fn outline_to_json_has_items_key() {
        let result = parse_outline(RUST_FIXTURE, "test.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            _ => panic!("expected Parsed"),
        };
        let json = outline.to_json();
        assert!(json.get("items").is_some(), "must have 'items' key");
    }
}
