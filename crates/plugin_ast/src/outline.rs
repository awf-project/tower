//! Tree-sitter outline walker for Rust, Go, and PHP source files.
//!
//! This module is **host-testable**: it uses the `tree-sitter` + grammar crates
//! which compile fine on the host target (no WASI SDK needed). The wasm
//! export surface in `lib.rs` calls into this module, keeping the core logic
//! out of the wasm ABI layer.
//!
//! # Design
//!
//! ```text
//! parse_outline(source_bytes, language_hint)
//!   → OutlineResult::Unsupported          (OP1/AC2: unsupported language hint)
//!   → OutlineResult::Outline(Outline)     (EV1/AC1: supported language, partial ok for AC3)
//! ```
//!
//! Supported languages and structural items (EV1/U1):
//!
//! **Rust** (`.rs`):
//! - `function_item` → `OutlineKind::Function`
//! - `struct_item`   → `OutlineKind::Struct`
//! - `enum_item`     → `OutlineKind::Enum`
//! - `trait_item`    → `OutlineKind::Trait`
//! - `impl_item`     → `OutlineKind::Impl`
//! - methods inside an `impl_item` block → `OutlineKind::Method`
//! - `mod_item`      → `OutlineKind::Module`
//!
//! **Go** (`.go`):
//! - `function_declaration` → `OutlineKind::Function`
//! - `method_declaration`   → `OutlineKind::Method`
//! - `type_spec` (struct body)    → `OutlineKind::Struct`
//! - `type_spec` (interface body) → `OutlineKind::Trait`
//!
//! **PHP** (`.php`):
//! - `function_definition`   → `OutlineKind::Function`
//! - `class_declaration`     → `OutlineKind::Class`
//! - `interface_declaration` → `OutlineKind::Trait`
//! - `trait_declaration`     → `OutlineKind::Trait`
//! - `method_declaration`    → `OutlineKind::Method`
//!
//! Raw body text is never included — only kind, name (or a "<anonymous>" fallback
//! for items whose name cannot be resolved), and byte-offset span (AC1/EV1).

use tree_sitter::{Language, Node, Parser};

use crate::symbols::{language_label, SupportedLanguage};

// ── Public types ──────────────────────────────────────────────────────────────

/// The kind of a structural outline item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutlineKind {
    /// A `fn foo()` free function.
    Function,
    /// A `struct Foo`.
    Struct,
    /// An `enum Foo`.
    Enum,
    /// A `trait Foo`.
    Trait,
    /// An `impl Foo` or `impl Trait for Foo`.
    Impl,
    /// A method inside an `impl` block.
    Method,
    /// A `mod foo` module declaration.
    Module,
    /// A `type Foo = …` type alias.
    TypeAlias,
    /// A `const FOO: …` constant.
    Const,
    /// A `static FOO: …` static.
    Static,
    /// A `macro_rules! foo` macro definition.
    MacroDef,
    /// A PHP `class Foo` declaration.
    ///
    /// Distinct from `Struct` so that consumers can use `kind=class` in
    /// `find_symbols` after `get_outline` reports a PHP class (round-trip
    /// consistency).
    Class,
}

impl OutlineKind {
    /// Return the string label used in the [`plugin_sdk::Value`] representation.
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

/// Byte-offset span of an outline item in the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start_byte: usize,
    /// Exclusive end byte offset.
    pub end_byte: usize,
    /// Zero-based start row.
    pub start_row: usize,
    /// Zero-based start column.
    pub start_col: usize,
    /// Zero-based end row (inclusive line of last byte).
    pub end_row: usize,
    /// Zero-based end column.
    pub end_col: usize,
}

/// A single structural item in the file outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineItem {
    /// The kind of structural construct.
    pub kind: OutlineKind,
    /// Name of the item. `"<anonymous>"` for nameless items (some `impl` blocks).
    pub name: String,
    /// Byte-offset and line/column span.
    pub span: Span,
}

/// The full outline of a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    /// Structural items in source order.
    pub items: Vec<OutlineItem>,
}

/// The result of an outline extraction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutlineResult {
    /// The language is not supported (OP1/AC2).
    ///
    /// Returned for any language hint other than `"rust"` (case-insensitive),
    /// or when the file extension is not `.rs`.
    Unsupported { language: String },
    /// A (possibly partial) outline was extracted (EV1/UN1/AC1/AC3).
    ///
    /// Tree-sitter is error-tolerant: even a malformed file produces a partial
    /// tree. The outline may be empty if the file has no recognisable items.
    Parsed(Outline),
}

// ── Language hint ─────────────────────────────────────────────────────────────

/// Return `true` if the hint (a file path or language identifier) refers to Rust.
///
/// Accepts:
/// - Language identifier strings: `"rust"`, `"Rust"`, `"RUST"` (case-insensitive)
/// - File paths ending in `.rs`
///
/// Kept for backward compatibility with existing tests.
pub fn is_rust_hint(hint: &str) -> bool {
    SupportedLanguage::from_hint(hint) == Some(SupportedLanguage::Rust)
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Parse `source` and extract a structural outline.
///
/// `language_hint` is either a file path (extension used) or a language name.
/// Supported languages: Rust (`.rs`), Go (`.go`), PHP (`.php`).
/// For any unsupported hint, returns [`OutlineResult::Unsupported`].
///
/// # Errors (none returned)
///
/// This function is infallible. Tree-sitter's error-tolerant parser always
/// produces a tree (UN1/AC3); the walker skips nodes it cannot interpret.
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
        // The FFI call is inside the grammar crate; we have no unsafe here.
        // Pinned versions (tree-sitter 0.25 + matching grammar crates) are
        // known-compatible (12a recipe). A version mismatch is a compile-time
        // packaging error, not a runtime user error — hence the unwrap below.
        SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        SupportedLanguage::Php => tree_sitter_php::LANGUAGE_PHP.into(),
    };

    let mut parser = Parser::new();
    parser
        .set_language(&ts_language)
        .expect("tree-sitter language version mismatch — check Cargo.toml pins");

    // Tree-sitter always returns a tree even for invalid input (UN1/AC3).
    let tree = parser.parse(source, None).unwrap_or_else(|| {
        // parse() returns None only if no language is set or parsing was cancelled.
        // Neither can happen here (language is set above, no timeout/callback set).
        parser.parse(b"", None).expect("empty source must parse")
    });

    let root = tree.root_node();
    let mut items = Vec::new();

    match lang {
        SupportedLanguage::Rust => walk_top_level(root, source, &mut items),
        SupportedLanguage::Go => walk_go_top_level(root, source, &mut items),
        SupportedLanguage::Php => walk_php_top_level(root, source, &mut items),
    }

    OutlineResult::Parsed(Outline { items })
}

// ── AST walker ────────────────────────────────────────────────────────────────

/// Walk the direct children of the source_file root and collect outline items.
///
/// Recurses into `impl_item` blocks to collect methods (spec EV1).
/// Does NOT recurse into function bodies, struct fields, etc. — only the
/// structural skeleton is collected (U1).
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
                // impl items: extract the impl header then recurse for methods.
                let impl_item = extract_impl_item(child, source);
                items.push(impl_item);
                walk_impl_body(child, source, items);
            }
            "mod_item" => {
                if let Some(item) = extract_named_item(child, source, OutlineKind::Module) {
                    items.push(item);
                }
                // Don't recurse into mod bodies — they're separate compilation units.
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
                // macro_rules! foo { ... }
                if let Some(item) = extract_named_item(child, source, OutlineKind::MacroDef) {
                    items.push(item);
                }
            }
            // attribute_item, use_declaration, extern_crate_declaration, etc.: skip.
            _ => {}
        }
    }
}

/// Walk methods inside an `impl_item` declaration block.
fn walk_impl_body(impl_node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    // The declaration_list (body) is the last child of impl_item.
    let body = impl_node
        .children(&mut impl_node.walk())
        .find(|n| n.kind() == "declaration_list");

    let Some(body) = body else { return };

    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "function_item" {
            if let Some(item) = extract_named_item(child, source, OutlineKind::Method) {
                items.push(item);
            }
        }
    }
}

/// Extract a named structural item (function, struct, enum, trait, mod, etc.).
///
/// Returns `None` only if tree-sitter produced an ERROR node for this item
/// (severely malformed). The name child is searched by field name `"name"`;
/// if absent (should not happen for well-formed items), `"<anonymous>"` is used.
fn extract_named_item(node: Node<'_>, source: &[u8], kind: OutlineKind) -> Option<OutlineItem> {
    // Skip pure ERROR nodes — they have no meaningful structure.
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

/// Extract the `impl_item` as an outline item.
///
/// For `impl Foo`, the name is derived from the `type` child.
/// For `impl Trait for Foo`, the name is `"Trait for Foo"`.
/// For `impl !Trait for Foo` (negative impl), the name is `"!Trait for Foo"`.
/// Falls back to `"<anonymous>"`.
fn extract_impl_item(node: Node<'_>, source: &[u8]) -> OutlineItem {
    // Decision: build a descriptive name from the impl's type/trait fields.
    // tree-sitter-rust 0.23 grammar fields for impl_item:
    //   trait:     the trait identifier (optional; present for `impl Trait for Type`)
    //   type:      the concrete type
    //
    // For negative impls (`impl !Send for T`), the `!` token is a plain STRING
    // sibling at child index 1 — it is NOT part of the `trait` field value.
    // Verified in grammar.json: SEQ [ "impl", opt("!"), FIELD("trait"), "for",
    // FIELD("type"), FIELD("body") ].  We detect it via prev_sibling of the
    // trait node (kind == "!") and prepend it to the trait name.
    let type_text = node
        .child_by_field_name("type")
        .and_then(|n| extract_text(n, source));

    let trait_text = node.child_by_field_name("trait").and_then(|trait_node| {
        let base = extract_text(trait_node, source)?;
        // Check whether the immediately-preceding sibling is the `!` token.
        // This is the only way to distinguish negative from positive impls
        // without re-scanning all children.
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

/// Extract the source text covered by `node`, trimming to a reasonable length.
///
/// Used for names and type fragments — never for body text (U1 enforced by
/// only calling this on name/type nodes, not body nodes).
fn extract_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    let bytes = source.get(node.start_byte()..node.end_byte())?;
    let text = std::str::from_utf8(bytes).ok()?;
    // Cap at 256 bytes: names longer than this are pathological / generated code.
    // Prevents a DoS if a malformed file has a huge "name" span.
    let capped = if text.len() > 256 {
        // Trim to last valid UTF-8 char boundary.
        let mut end = 256;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };
    Some(capped.to_owned())
}

/// Build a [`Span`] from a tree-sitter node's position information.
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

// ── Go AST walker ─────────────────────────────────────────────────────────────

/// Walk Go top-level declarations and collect outline items.
///
/// Go node kinds mapped:
/// - `function_declaration` → `Function`
/// - `method_declaration`   → `Method`
/// - `type_declaration`     → recurse into `type_spec` children
///   - struct body  → `Struct`
///   - interface body → `Trait`
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

/// Walk a Go `type_declaration` node for `type_spec` children.
fn walk_go_type_decl(node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "type_spec" || child.is_error() {
            continue;
        }
        let type_field = child.child_by_field_name("type");
        let kind = match type_field.as_ref().map(Node::kind) {
            Some("interface_type") => OutlineKind::Trait,
            _ => OutlineKind::Struct, // struct_type, or opaque type
        };
        if let Some(item) = extract_named_item(child, source, kind) {
            items.push(item);
        }
    }
}

// ── PHP AST walker ────────────────────────────────────────────────────────────

/// Walk PHP top-level declarations and collect outline items.
///
/// PHP node kinds mapped:
/// - `function_definition`   → `Function`
/// - `class_declaration`     → `Class`
/// - `interface_declaration` → `Trait`
/// - `trait_declaration`     → `Trait`
/// - Methods inside class/interface/trait bodies → `Method`
///
/// # Round-trip consistency
///
/// `class_declaration` maps to `OutlineKind::Class` (not `Struct`) so that a
/// consumer using `get_outline` on a PHP file can pass `kind=class` to
/// `find_symbols` and get a result — using `kind=struct` would trigger
/// `NotApplicable` because `Struct` is not in PHP's `kind_applicable` set.
fn walk_php_top_level(node: Node<'_>, source: &[u8], items: &mut Vec<OutlineItem>) {
    walk_php_node_outline(node, source, items);
}

/// Recursive PHP outline walk (handles nested class/interface/trait bodies).
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
                // Decision: emit OutlineKind::Class (not Struct) for PHP class declarations.
                // Why: round-trip consistency — get_outline → find_symbols with
                //      kind=class works; kind=struct would return NotApplicable for PHP.
                // Trade-off: OutlineKind gains a Class variant (one extra arm everywhere).
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

// ── Conversion helpers ────────────────────────────────────────────────────────

impl OutlineItem {
    /// Convert to a [`plugin_sdk::Value::Map`] for the MCP wire format.
    ///
    /// Shape:
    /// ```json
    /// {
    ///   "kind":       "function",
    ///   "name":       "my_fn",
    ///   "start_byte": 0,
    ///   "end_byte":   42,
    ///   "start_row":  0,
    ///   "start_col":  0,
    ///   "end_row":    2,
    ///   "end_col":    1
    /// }
    /// ```
    #[must_use]
    pub fn to_sdk_value(&self) -> plugin_sdk::Value {
        plugin_sdk::Value::Map(vec![
            (
                "kind".to_owned(),
                plugin_sdk::Value::Text(self.kind.as_str().to_owned()),
            ),
            (
                "name".to_owned(),
                plugin_sdk::Value::Text(self.name.clone()),
            ),
            (
                "start_byte".to_owned(),
                plugin_sdk::Value::Integer(self.span.start_byte as i64),
            ),
            (
                "end_byte".to_owned(),
                plugin_sdk::Value::Integer(self.span.end_byte as i64),
            ),
            (
                "start_row".to_owned(),
                plugin_sdk::Value::Integer(self.span.start_row as i64),
            ),
            (
                "start_col".to_owned(),
                plugin_sdk::Value::Integer(self.span.start_col as i64),
            ),
            (
                "end_row".to_owned(),
                plugin_sdk::Value::Integer(self.span.end_row as i64),
            ),
            (
                "end_col".to_owned(),
                plugin_sdk::Value::Integer(self.span.end_col as i64),
            ),
        ])
    }
}

impl Outline {
    /// Convert to a [`plugin_sdk::Value::Map`] for the MCP wire format.
    ///
    /// Shape: `{ "items": [ ...OutlineItem maps... ] }`
    #[must_use]
    pub fn to_sdk_value(&self) -> plugin_sdk::Value {
        let items: Vec<plugin_sdk::Value> =
            self.items.iter().map(OutlineItem::to_sdk_value).collect();
        plugin_sdk::Value::Map(vec![("items".to_owned(), plugin_sdk::Value::List(items))])
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TDD step 1: AC1 — outline on a Rust fixture returns kinds/names/spans ──

    const RUST_FIXTURE: &[u8] = br#"
use std::collections::HashMap;

/// A struct with a doc comment.
pub struct MyStruct {
    field: u32,
}

impl MyStruct {
    pub fn new(value: u32) -> Self {
        Self { field: value }
    }

    fn get(&self) -> u32 {
        self.field
    }
}

pub fn standalone_fn(x: i32) -> i32 {
    x + 1
}

pub trait MyTrait {
    fn do_thing(&self);
}

impl MyTrait for MyStruct {
    fn do_thing(&self) {}
}

pub enum Color {
    Red,
    Green,
    Blue,
}
"#;

    #[test]
    fn ac1_rust_fixture_returns_struct_impl_methods_fn_trait_enum() {
        let result = parse_outline(RUST_FIXTURE, "test.rs");

        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { language } => {
                panic!("AC1: expected Parsed, got Unsupported({language})")
            }
        };

        // Collect (kind, name) pairs for easy assertion.
        let items: Vec<(&str, &str)> = outline
            .items
            .iter()
            .map(|i| (i.kind.as_str(), i.name.as_str()))
            .collect();

        // Struct must appear.
        assert!(
            items.iter().any(|&(k, n)| k == "struct" && n == "MyStruct"),
            "AC1: MyStruct must be in outline, got: {items:?}"
        );

        // Impl block must appear.
        assert!(
            items.iter().any(|&(k, n)| k == "impl" && n == "MyStruct"),
            "AC1: impl MyStruct must be in outline, got: {items:?}"
        );

        // Methods must appear.
        assert!(
            items.iter().any(|&(k, n)| k == "method" && n == "new"),
            "AC1: method 'new' must be in outline, got: {items:?}"
        );
        assert!(
            items.iter().any(|&(k, n)| k == "method" && n == "get"),
            "AC1: method 'get' must be in outline, got: {items:?}"
        );

        // Free function must appear.
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "function" && n == "standalone_fn"),
            "AC1: standalone_fn must be in outline, got: {items:?}"
        );

        // Trait must appear.
        assert!(
            items.iter().any(|&(k, n)| k == "trait" && n == "MyTrait"),
            "AC1: MyTrait must be in outline, got: {items:?}"
        );

        // impl Trait for Type must appear.
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "impl" && n == "MyTrait for MyStruct"),
            "AC1: impl MyTrait for MyStruct must be in outline, got: {items:?}"
        );

        // Enum must appear.
        assert!(
            items.iter().any(|&(k, n)| k == "enum" && n == "Color"),
            "AC1: Color enum must be in outline, got: {items:?}"
        );
    }

    #[test]
    fn ac1_no_raw_body_text_in_items() {
        // U1: no raw body text — only kind, name, span.
        let result = parse_outline(RUST_FIXTURE, "test.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { .. } => panic!("expected Parsed"),
        };

        // All names should be short identifiers, not multi-line bodies.
        for item in &outline.items {
            assert!(
                !item.name.contains('\n'),
                "U1: name must not contain newlines (raw body), got: {:?}",
                item.name
            );
            assert!(
                item.name.len() <= 256,
                "U1: name must be capped at 256 bytes, got length {}",
                item.name.len()
            );
        }
    }

    #[test]
    fn ac1_spans_are_valid_byte_offsets() {
        let result = parse_outline(RUST_FIXTURE, "test.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { .. } => panic!("expected Parsed"),
        };

        for item in &outline.items {
            assert!(
                item.span.start_byte < item.span.end_byte,
                "AC1: span start must be before end for {:?}",
                item
            );
            assert!(
                item.span.end_byte <= RUST_FIXTURE.len(),
                "AC1: span end must be within source bounds for {:?}",
                item
            );
        }
    }

    // ── TDD step 2: AC2 — unsupported language guard ──────────────────────────

    #[test]
    fn ac2_unsupported_language_returns_unsupported() {
        // MIN-01: language field must carry the extension, not the raw path.
        let result = parse_outline(b"class Foo {}", "foo.py");
        assert!(
            matches!(result, OutlineResult::Unsupported { ref language } if language == "py"),
            "AC2: Python hint must return Unsupported with language=\"py\", got: {result:?}"
        );
    }

    #[test]
    fn ac2_unsupported_path_with_directory_uses_extension() {
        // MIN-01: a path like "lab/notes.md" must produce language="md".
        let result = parse_outline(b"# heading", "lab/notes.md");
        assert!(
            matches!(result, OutlineResult::Unsupported { ref language } if language == "md"),
            "AC2: lab/notes.md hint must return language=\"md\", got: {result:?}"
        );
    }

    #[test]
    fn ac2_unsupported_dotless_hint_passthrough() {
        // MIN-01: a bare language id with no dot passes through unchanged.
        let result = parse_outline(b"const x: number = 1;", "typescript");
        assert!(
            matches!(result, OutlineResult::Unsupported { ref language } if language == "typescript"),
            "AC2: dotless hint \"typescript\" must produce language=\"typescript\", got: {result:?}"
        );
    }

    #[test]
    fn ac2_unsupported_javascript_returns_unsupported() {
        let result = parse_outline(b"function hello() {}", "hello.js");
        assert!(
            matches!(result, OutlineResult::Unsupported { .. }),
            "AC2: JS hint must return Unsupported, got: {result:?}"
        );
    }

    #[test]
    fn ac2_unsupported_language_id_returns_unsupported() {
        // Decision: Go and PHP are now supported (spec 12d).
        // Use "typescript" as an example of a truly unsupported language id.
        let result = parse_outline(b"const x: number = 1;", "typescript");
        assert!(
            matches!(result, OutlineResult::Unsupported { .. }),
            "AC2: TypeScript language id must return Unsupported, got: {result:?}"
        );
    }

    #[test]
    fn ac2_rust_language_id_case_insensitive() {
        // Language id "RUST" (all caps) must be accepted.
        let result = parse_outline(b"fn hello() {}", "RUST");
        assert!(
            matches!(result, OutlineResult::Parsed(_)),
            "AC2: RUST language id must be accepted, got: {result:?}"
        );
    }

    #[test]
    fn ac2_rs_extension_is_accepted() {
        let result = parse_outline(b"fn hello() {}", "src/main.rs");
        assert!(
            matches!(result, OutlineResult::Parsed(_)),
            "AC2: .rs extension must be accepted as Rust, got: {result:?}"
        );
    }

    // ── TDD step 3: AC3 — malformed input yields partial outline ─────────────

    #[test]
    fn ac3_malformed_input_does_not_crash() {
        // UN1: tree-sitter is error-tolerant — broken syntax yields partial outline.
        let broken = b"fn broken( { struct // missing brace\npub fn good_fn() {}";
        let result = parse_outline(broken, "broken.rs");
        // Must return Parsed (not panic, not Unsupported).
        assert!(
            matches!(result, OutlineResult::Parsed(_)),
            "AC3: malformed Rust must return Parsed (partial), got: {result:?}"
        );
    }

    #[test]
    fn ac3_partial_outline_has_no_crash_for_empty_source() {
        let result = parse_outline(b"", "empty.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { .. } => panic!("expected Parsed for .rs extension"),
        };
        // Empty source yields empty outline — no crash.
        assert!(
            outline.items.is_empty(),
            "AC3: empty source must yield empty outline, got: {:?}",
            outline.items
        );
    }

    #[test]
    fn ac3_mixed_valid_and_broken_extracts_valid_items() {
        // A file with some broken syntax still yields the well-formed items
        // that appear BEFORE the error region.
        //
        // Decision: tree-sitter's error recovery wraps the broken node AND any
        // immediately following nodes in the same ERROR token. Items declared
        // after a broken statement may be consumed by the error node. This is
        // the expected tree-sitter behavior and NOT a bug in the walker.
        // The contract (UN1/AC3) is: no crash + at least the items BEFORE the
        // break are extracted.
        let mixed = b"
pub fn valid_fn() -> u32 { 42 }
pub struct GoodStruct {}
fn broken( {
";
        let result = parse_outline(mixed, "mixed.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { .. } => panic!("expected Parsed"),
        };
        // Items before the break must survive.
        let items: Vec<(&str, &str)> = outline
            .items
            .iter()
            .map(|i| (i.kind.as_str(), i.name.as_str()))
            .collect();
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "function" && n == "valid_fn"),
            "AC3: valid_fn must survive broken syntax, got: {items:?}"
        );
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "struct" && n == "GoodStruct"),
            "AC3: GoodStruct must survive broken syntax, got: {items:?}"
        );
    }

    // ── is_rust_hint ─────────────────────────────────────────────────────────

    #[test]
    fn is_rust_hint_returns_true_for_rs_extension() {
        assert!(is_rust_hint("main.rs"));
        assert!(is_rust_hint("src/lib.rs"));
        assert!(is_rust_hint("path/to/file.rs"));
    }

    #[test]
    fn is_rust_hint_returns_true_for_rust_language_id() {
        assert!(is_rust_hint("rust"));
        assert!(is_rust_hint("Rust"));
        assert!(is_rust_hint("RUST"));
    }

    #[test]
    fn is_rust_hint_returns_false_for_other_extensions() {
        assert!(!is_rust_hint("main.py"));
        assert!(!is_rust_hint("main.go"));
        assert!(!is_rust_hint("main.ts"));
        assert!(!is_rust_hint("main.cpp"));
    }

    // ── to_sdk_value ──────────────────────────────────────────────────────────

    #[test]
    fn outline_item_to_sdk_value_has_expected_keys() {
        use plugin_sdk::Value;

        let item = OutlineItem {
            kind: OutlineKind::Function,
            name: "my_fn".to_owned(),
            span: Span {
                start_byte: 0,
                end_byte: 20,
                start_row: 0,
                start_col: 0,
                end_row: 2,
                end_col: 1,
            },
        };

        let value = item.to_sdk_value();
        let Value::Map(pairs) = &value else {
            panic!("expected Map, got {value:?}");
        };

        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        for expected_key in &[
            "kind",
            "name",
            "start_byte",
            "end_byte",
            "start_row",
            "end_row",
        ] {
            assert!(
                keys.contains(expected_key),
                "to_sdk_value must have key '{expected_key}', got: {keys:?}"
            );
        }

        // kind must be "function"
        let kind_val = pairs
            .iter()
            .find(|(k, _)| k == "kind")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(*kind_val, Value::Text("function".to_owned()));
    }

    #[test]
    fn outline_to_sdk_value_wraps_items_in_map() {
        use plugin_sdk::Value;

        let outline = Outline {
            items: vec![OutlineItem {
                kind: OutlineKind::Struct,
                name: "Foo".to_owned(),
                span: Span {
                    start_byte: 0,
                    end_byte: 15,
                    start_row: 0,
                    start_col: 0,
                    end_row: 0,
                    end_col: 15,
                },
            }],
        };

        let value = outline.to_sdk_value();
        let Value::Map(pairs) = &value else {
            panic!("expected Map, got {value:?}");
        };
        let items_val = pairs
            .iter()
            .find(|(k, _)| k == "items")
            .map(|(_, v)| v)
            .expect("must have 'items' key");

        assert!(matches!(items_val, Value::List(_)), "items must be a List");
    }

    // ── TDD: negative trait impl — `!` marker must not be dropped ────────────

    /// `impl !Send for Foo {}` — the `!` is a sibling of the `trait` field in the
    /// tree-sitter-rust 0.23 grammar (child index 1, kind `"!"`), NOT part of the
    /// field value.  The outline name must be `"!Send for Foo"`, not `"Send for Foo"`.
    #[test]
    fn negative_impl_name_includes_bang() {
        let source = b"impl !Send for Foo {}";
        let result = parse_outline(source, "test.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { language } => {
                panic!("expected Parsed, got Unsupported({language})")
            }
        };
        let impl_item = outline
            .items
            .iter()
            .find(|i| i.kind == OutlineKind::Impl)
            .expect("impl item must be present");
        assert_eq!(
            impl_item.name, "!Send for Foo",
            "negative impl must include '!' prefix: got {:?}",
            impl_item.name
        );
    }

    /// Positive impl must be unaffected — no spurious `!` prefix added.
    #[test]
    fn positive_impl_name_has_no_bang() {
        let source = b"impl Send for Foo {}";
        let result = parse_outline(source, "test.rs");
        let outline = match result {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { language } => {
                panic!("expected Parsed, got Unsupported({language})")
            }
        };
        let impl_item = outline
            .items
            .iter()
            .find(|i| i.kind == OutlineKind::Impl)
            .expect("impl item must be present");
        assert_eq!(
            impl_item.name, "Send for Foo",
            "positive impl must not gain '!' prefix: got {:?}",
            impl_item.name
        );
    }

    // ── Go outline tests (AC2 spec 12d) ──────────────────────────────────────

    const GO_FIXTURE: &[u8] = br#"package main

import "fmt"

func HelloWorld(name string) string {
    return fmt.Sprintf("Hello, %s", name)
}

func (s MyStruct) GetValue() int {
    return s.value
}

type MyStruct struct {
    value int
}

type MyInterface interface {
    Method() error
}
"#;

    #[test]
    fn go_outline_functions_and_methods() {
        let result = parse_outline(GO_FIXTURE, "main.go");
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

        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "function" && n == "HelloWorld"),
            "Go function HelloWorld must be in outline, got: {items:?}"
        );
        assert!(
            items.iter().any(|&(k, n)| k == "method" && n == "GetValue"),
            "Go method GetValue must be in outline, got: {items:?}"
        );
        assert!(
            items.iter().any(|&(k, n)| k == "struct" && n == "MyStruct"),
            "Go struct MyStruct must be in outline, got: {items:?}"
        );
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "trait" && n == "MyInterface"),
            "Go interface MyInterface must be outline as trait, got: {items:?}"
        );
    }

    #[test]
    fn go_unsupported_hint_returns_unsupported() {
        // Go hint before 12d was returned as Unsupported — now it must parse.
        let result = parse_outline(b"package main", "main.go");
        assert!(
            matches!(result, OutlineResult::Parsed(_)),
            "Go file must return Parsed after 12d, got: {result:?}"
        );
    }

    #[test]
    fn go_language_id_accepted() {
        let result = parse_outline(b"package main\nfunc Foo() {}", "go");
        assert!(
            matches!(result, OutlineResult::Parsed(_)),
            "Go language id must be accepted, got: {result:?}"
        );
    }

    // ── PHP outline tests (AC2 spec 12d) ─────────────────────────────────────

    const PHP_FIXTURE: &[u8] = br#"<?php

class MyClass {
    public function myMethod() {}
}

interface MyInterface {
    public function interfaceMethod();
}

trait MyTrait {
    public function traitMethod() {}
}

function topLevelFn() {}
"#;

    #[test]
    fn php_outline_class_interface_trait_function() {
        let result = parse_outline(PHP_FIXTURE, "app.php");
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

        assert!(
            items.iter().any(|&(k, n)| k == "class" && n == "MyClass"),
            "PHP class MyClass must be in outline as class (not struct), got: {items:?}"
        );
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "trait" && n == "MyInterface"),
            "PHP interface MyInterface must be in outline as trait, got: {items:?}"
        );
        assert!(
            items.iter().any(|&(k, n)| k == "trait" && n == "MyTrait"),
            "PHP trait MyTrait must be in outline as trait, got: {items:?}"
        );
        assert!(
            items
                .iter()
                .any(|&(k, n)| k == "function" && n == "topLevelFn"),
            "PHP function topLevelFn must be in outline, got: {items:?}"
        );
        assert!(
            items.iter().any(|&(k, n)| k == "method" && n == "myMethod"),
            "PHP method myMethod must be in outline, got: {items:?}"
        );
    }

    #[test]
    fn php_language_id_accepted() {
        let result = parse_outline(b"<?php\nfunction foo() {}", "php");
        assert!(
            matches!(result, OutlineResult::Parsed(_)),
            "PHP language id must be accepted, got: {result:?}"
        );
    }

    #[test]
    fn php_extension_accepted() {
        let result = parse_outline(b"<?php\nclass A {}", "src/A.php");
        assert!(
            matches!(result, OutlineResult::Parsed(_)),
            "PHP .php extension must be accepted, got: {result:?}"
        );
    }

    /// Round-trip: get_outline emits kind=class for PHP class declarations;
    /// a caller using that kind in find_symbols must get a result (not
    /// NotApplicable). Previously outline emitted kind=struct, but Struct is not
    /// in PHP's kind_applicable() set, making the round-trip silently broken.
    #[test]
    fn php_class_outline_kind_matches_find_symbols_kind() {
        use crate::symbols::{find_symbols, SymbolKind, SymbolResult};

        let source = br#"<?php
class MyClass {
    public function myMethod() {}
}
"#;

        // Step 1: outline must emit kind=class for the class declaration.
        let outline = match parse_outline(source, "test.php") {
            OutlineResult::Parsed(o) => o,
            OutlineResult::Unsupported { language } => {
                panic!("expected Parsed, got Unsupported({language})")
            }
        };
        let class_item = outline
            .items
            .iter()
            .find(|i| i.name == "MyClass")
            .expect("MyClass must appear in outline");
        assert_eq!(
            class_item.kind,
            OutlineKind::Class,
            "PHP class must have OutlineKind::Class in outline, got {:?}",
            class_item.kind
        );
        let kind_str = class_item.kind.as_str();
        assert_eq!(
            kind_str, "class",
            "OutlineKind::Class must serialize as 'class'"
        );

        // Step 2: find_symbols with kind=class (the kind emitted by outline) must find the class.
        let sym_kind = SymbolKind::parse_kind(kind_str)
            .expect("outline kind string must be parseable by SymbolKind::parse_kind");
        let result = find_symbols(source, "test.php", "MyClass", sym_kind);
        let sym_matches = match result {
            SymbolResult::Found(m) => m,
            SymbolResult::NotApplicable => {
                panic!(
                    "round-trip broken: find_symbols returned NotApplicable for kind=class in PHP"
                )
            }
            SymbolResult::Unsupported { language } => {
                panic!("unexpected Unsupported({language})")
            }
        };
        assert_eq!(
            sym_matches.len(),
            1,
            "round-trip: find_symbols must find MyClass with kind=class, got: {sym_matches:?}"
        );
    }
}
