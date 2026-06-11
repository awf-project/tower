//! Tree-sitter symbol finder for Rust, Go, and PHP source files.
//!
//! This module is **host-testable**: it uses the `tree-sitter` + grammar crates
//! which compile fine on the host target (no WASI SDK needed). The wasm export
//! surface in `lib.rs` calls into this module, keeping the core logic out of
//! the wasm ABI layer.
//!
//! # Design
//!
//! ```text
//! find_symbols(source_bytes, language_hint, symbol_name, kind)
//!   → SymbolResult::Unsupported           (OP1/AC3: language not supported)
//!   → SymbolResult::NotApplicable          (OP1/AC3: kind not applicable to language)
//!   → SymbolResult::Found(Vec<SymbolMatch>) (EV1/AC1: definition locations)
//! ```
//!
//! ## False-positive exclusion (U2/AC1)
//!
//! The grammar distinguishes definition nodes from comments, string literals,
//! and identifiers used in non-definition positions. We only walk nodes whose
//! tree-sitter kind is a definition node (e.g., `function_item`, `function_declaration`),
//! then match the `name` field text. A same-named occurrence inside a comment
//! or string literal is a different node kind entirely and is never visited.
//!
//! ## Not-applicable-kind (OP1/AC3)
//!
//! `SymbolKind::Method` is not applicable to Go (no method definitions in the
//! top-level sense — Go uses `method_declaration` which maps to `Method`).
//! `SymbolKind::Impl` is not applicable to Go or PHP.
//! When a kind cannot occur in the target language, `SymbolResult::NotApplicable`
//! is returned (empty result, no error).
//!
//! ## Error-tolerant trees (UN1/AC4)
//!
//! Tree-sitter always produces a tree from partial input. The walker skips
//! `ERROR` nodes and returns whatever well-formed definitions it found.

use tree_sitter::{Language, Node, Parser};

// ── Public types ──────────────────────────────────────────────────────────────

/// The kind of symbol to search for.
///
/// Not every kind is applicable to every language — see [`find_symbols`] for the
/// per-language applicability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A free function (Rust `fn foo`, Go `func Foo`, PHP `function foo`).
    Function,
    /// A struct or record type (Rust `struct Foo`, Go struct `type Foo struct`).
    Struct,
    /// An enum type (Rust `enum Foo`). Not applicable in Go or PHP.
    Enum,
    /// A trait (Rust `trait Foo`) or interface (Go `interface Foo`).
    Trait,
    /// An impl block (Rust `impl Foo`). Not applicable in Go or PHP.
    Impl,
    /// A method inside a type's implementation.
    Method,
    /// A module (Rust `mod foo`). Not applicable in Go or PHP.
    Module,
    /// A type alias (Rust `type Foo = …`, Go `type Foo = Bar`).
    TypeAlias,
    /// A constant (Rust `const FOO: …`, Go `const Foo`).
    Const,
    /// A static variable (Rust `static FOO: …`). Not applicable in Go or PHP.
    Static,
    /// A macro definition (Rust `macro_rules! foo`). Not applicable in Go or PHP.
    MacroDef,
    /// A class (PHP `class Foo`). Not applicable in Rust or Go.
    Class,
}

impl SymbolKind {
    /// Parse a kind from the string label used in the MCP wire format.
    ///
    /// Returns `None` for unrecognised strings.
    ///
    /// Named `parse_kind` rather than `from_str` to avoid confusion with
    /// the `std::str::FromStr` trait (clippy::should_implement_trait).
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

    /// Return the string label used in the MCP wire format.
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

/// A single symbol definition found in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMatch {
    /// The kind of the definition node.
    pub kind: SymbolKind,
    /// The matched symbol name.
    pub name: String,
    /// Byte-offset and line/col span of the definition node.
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

impl SymbolMatch {
    /// Convert to a [`plugin_sdk::Value::Map`] for the MCP wire format.
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

/// The result of a symbol search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolResult {
    /// The language is not supported.
    Unsupported { language: String },
    /// The requested kind is not applicable to this language (OP1/AC3).
    ///
    /// Returned as an empty list — not an error. The caller should treat
    /// this the same as `Found(vec![])`.
    NotApplicable,
    /// Zero or more symbol definitions found (EV1/UN1).
    Found(Vec<SymbolMatch>),
}

// ── Language enum (dispatch) ──────────────────────────────────────────────────

/// Derive a display label from a language hint (file path or bare language id).
///
/// If `hint` contains a `.`, the substring after the **last** `.` is returned,
/// lowercased (i.e. the file extension).  Otherwise the hint is returned
/// unchanged so that bare language ids like `"typescript"` pass through as-is.
///
/// # Examples
///
/// ```
/// use ast::symbols::language_label;
/// assert_eq!(language_label("foo.py"),        "py");
/// assert_eq!(language_label("lab/notes.md"),  "md");
/// assert_eq!(language_label("typescript"),    "typescript");
/// assert_eq!(language_label("UPPER.PY"),      "py");
/// ```
#[must_use]
pub fn language_label(hint: &str) -> String {
    match hint.rfind('.') {
        Some(pos) => hint[pos + 1..].to_lowercase(),
        None => hint.to_owned(),
    }
}

/// Supported languages for AST operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Go,
    Php,
}

impl SupportedLanguage {
    /// Detect the language from a file path / language hint.
    ///
    /// Accepts:
    /// - `.rs` extension or `"rust"` (case-insensitive) → Rust
    /// - `.go` extension or `"go"` (case-insensitive) → Go
    /// - `.php` extension or `"php"` (case-insensitive) → PHP
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
    ///
    /// Used to implement OP1/AC3: a kind that does not apply to the language
    /// returns `NotApplicable` (empty) rather than an error.
    #[must_use]
    pub fn kind_applicable(self, kind: SymbolKind) -> bool {
        match self {
            Self::Rust => true, // all SymbolKind variants exist in Rust
            Self::Go => matches!(
                kind,
                SymbolKind::Function
                    | SymbolKind::Struct
                    | SymbolKind::Trait   // mapped to interface
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

// ── Entry point ───────────────────────────────────────────────────────────────

/// Search `source` for definitions of `symbol_name` with the given `kind`.
///
/// `language_hint` is either a file path (extension used) or a language name.
///
/// Returns [`SymbolResult::Unsupported`] for unknown languages,
/// [`SymbolResult::NotApplicable`] for kinds that cannot occur in the language
/// (OP1/AC3), and [`SymbolResult::Found`] with zero or more matches otherwise.
///
/// The function is infallible — tree-sitter's error-tolerant parser always
/// produces a partial tree for malformed input (UN1/AC4).
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

    // OP1/AC3: kind not applicable to this language → empty result.
    if !lang.kind_applicable(kind) {
        return SymbolResult::NotApplicable;
    }

    let ts_language: Language = match lang {
        SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        SupportedLanguage::Php => tree_sitter_php::LANGUAGE_PHP.into(),
    };

    let mut parser = Parser::new();
    parser
        .set_language(&ts_language)
        .expect("tree-sitter language version mismatch — check Cargo.toml pins");

    // Tree-sitter always returns a tree even for invalid input (UN1/AC4).
    let tree = parser
        .parse(source, None)
        .unwrap_or_else(|| parser.parse(b"", None).expect("empty source must parse"));

    let root = tree.root_node();
    let mut matches = Vec::new();

    match lang {
        SupportedLanguage::Rust => walk_rust_symbols(root, source, symbol_name, kind, &mut matches),
        SupportedLanguage::Go => walk_go_symbols(root, source, symbol_name, kind, &mut matches),
        SupportedLanguage::Php => walk_php_symbols(root, source, symbol_name, kind, &mut matches),
    }

    SymbolResult::Found(matches)
}

// ── Rust walker ───────────────────────────────────────────────────────────────

/// Walk the Rust syntax tree looking for definitions of `symbol_name` with
/// the given `kind`. Only definition nodes are visited — comments and string
/// literals are different node kinds and are never matched (U2/AC1).
fn walk_rust_symbols(
    root: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match (kind, child.kind()) {
            (SymbolKind::Function, "function_item")
            | (SymbolKind::Struct, "struct_item")
            | (SymbolKind::Enum, "enum_item")
            | (SymbolKind::Trait, "trait_item")
            | (SymbolKind::Module, "mod_item")
            | (SymbolKind::TypeAlias, "type_item")
            | (SymbolKind::Const, "const_item")
            | (SymbolKind::Static, "static_item")
            | (SymbolKind::MacroDef, "macro_definition") => {
                if let Some(m) = match_named_item(child, source, symbol_name, kind) {
                    matches.push(m);
                }
            }
            (SymbolKind::Impl, "impl_item") => {
                // impl name = the type name (child_by_field_name("type")).
                if let Some(name_text) = impl_name(child, source)
                    && name_text == symbol_name
                {
                    matches.push(node_to_match(child, name_text, kind));
                }
            }
            (SymbolKind::Method, "impl_item") => {
                // Recurse into impl body looking for function_item (methods).
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
}

/// Build the impl name text from the `type` field (e.g. `"MyStruct"`).
fn impl_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.child_by_field_name("type")
        .and_then(|n| extract_text(n, source))
}

// ── Go walker ─────────────────────────────────────────────────────────────────

/// Walk the Go syntax tree looking for definitions of `symbol_name` with
/// the given `kind`.
///
/// Go node kinds:
/// - `function_declaration` → Function
/// - `method_declaration`   → Method
/// - `type_declaration`     → container; children are `type_spec`
///   - `type_spec` with struct body → Struct
///   - `type_spec` with interface body → Trait (mapped: Go `interface` ≈ Rust `trait`)
///   - `type_spec` with alias (`=`) → TypeAlias
/// - `const_declaration`    → Const (children are `const_spec`)
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

/// Walk `type_declaration` children (`type_spec` nodes) for Go.
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
        // Determine what this type_spec defines.
        let detected_kind = go_type_spec_kind(child, source);
        if detected_kind == Some(kind)
            && let Some(m) = match_named_item(child, source, symbol_name, kind)
        {
            matches.push(m);
        }
    }
}

/// Determine the `SymbolKind` for a Go `type_spec` node.
///
/// - Has a `type` child of kind `struct_type`   → `Struct`
/// - Has a `type` child of kind `interface_type` → `Trait` (Go interface mapped)
/// - Has an `=` sibling token in its children    → `TypeAlias`
/// - Otherwise: `Struct` (opaque type definition)
fn go_type_spec_kind(node: Node<'_>, _source: &[u8]) -> Option<SymbolKind> {
    let type_child = node.child_by_field_name("type")?;
    match type_child.kind() {
        "struct_type" => Some(SymbolKind::Struct),
        "interface_type" => Some(SymbolKind::Trait),
        _ => {
            // Check for alias (`type Foo = Bar`): presence of a `=` literal sibling.
            let is_alias = node
                .children(&mut node.walk())
                .any(|c| c.kind() == "=" && !c.is_named());
            if is_alias {
                Some(SymbolKind::TypeAlias)
            } else {
                // Opaque type definition (e.g. `type Duration int`) — classify as Struct
                // because it is a concrete type introduction, not an alias.
                Some(SymbolKind::Struct)
            }
        }
    }
}

/// Walk `const_declaration` → `const_spec` children for Go.
///
/// # Go grammar note
///
/// `const_spec` has a `"name"` field marked `"multiple": true` in
/// node-types.json, meaning a single `const_spec` can declare several
/// identifiers (e.g. `const A, B = 1, 2`).  `child_by_field_name` returns
/// only the **first** occurrence; we must use `children_by_field_name` to
/// iterate all of them.
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
        // Iterate ALL name-field children (handles multi-name specs like
        // `const A, B = 1, 2`).
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

/// Walk the PHP syntax tree looking for definitions of `symbol_name` with
/// the given `kind`.
///
/// PHP node kinds (php grammar):
/// - `function_definition`    → Function
/// - `class_declaration`      → Class (body contains `method_declaration`)
/// - `interface_declaration`  → Trait (PHP interface ≈ Rust trait)
/// - `trait_declaration`      → Trait (PHP trait ≈ Rust trait)
/// - `method_declaration`     → Method (inside class/interface/trait bodies)
/// - `const_declaration`      → Const
fn walk_php_symbols(
    root: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    kind: SymbolKind,
    matches: &mut Vec<SymbolMatch>,
) {
    walk_php_node(root, source, symbol_name, kind, matches);
}

/// Recursive walk for PHP (handles nested class/interface/trait bodies).
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
                // Recurse into class body for method_declaration.
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
                // Recurse into interface/trait body for method_declaration.
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
                // Recurse into declaration_list bodies (class/interface/trait bodies).
                walk_php_node(child, source, symbol_name, kind, matches);
            }
            "const_declaration" if matches!(kind, SymbolKind::Const) => {
                // PHP const_declaration contains const_element children.
                walk_php_const(child, source, symbol_name, matches);
            }
            _ => {}
        }
    }
}

/// Walk PHP `const_declaration` for `const_element` children.
///
/// # PHP grammar note
///
/// `const_element` in the tree-sitter-php grammar has **no named fields** —
/// `fields: {}` in node-types.json. `child_by_field_name("name")` always returns
/// `None`. We must iterate the element's children and find the first named child
/// whose kind is `"name"` (the grammar emits a `name` node as the first named
/// child of `const_element`, e.g. `const BAR = 1` → name="BAR").
fn walk_php_const(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    matches: &mut Vec<SymbolMatch>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "const_element" {
            continue;
        }
        if child.is_error() {
            continue;
        }
        // const_element has no named fields; find the "name" child by kind.
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

/// Try to match a named definition node.
///
/// Returns `Some(SymbolMatch)` if the node has a `name` field whose text
/// equals `symbol_name`. Returns `None` for ERROR nodes or name mismatches.
///
/// This is the core false-positive exclusion mechanism (U2/AC1): we only
/// visit nodes whose kind is a definition node kind. String literals and
/// comments in tree-sitter have entirely different node kinds (`string`,
/// `comment`, etc.) and are never visited.
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

/// Build a [`SymbolMatch`] from a tree-sitter node.
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

/// Extract the source text covered by `node` (capped at 256 bytes).
fn extract_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    let bytes = source.get(node.start_byte()..node.end_byte())?;
    let text = std::str::from_utf8(bytes).ok()?;
    let capped = if text.len() > 256 {
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TDD step 1 (RED → GREEN): Rust find_symbols — AC1 ────────────────────

    const RUST_FIXTURE: &[u8] = br#"
// This is a comment mentioning find_me
pub struct FindMe {
    value: u32,
}

/// find_me is also in a doc comment
impl FindMe {
    pub fn find_me() -> Self {
        // also in body comment: find_me
        Self { value: 0 }
    }

    pub fn other_fn(&self) {}
}

pub fn find_me() -> u32 { 42 }

pub enum FindMe2 {
    A,
}

const FIND_ME_CONST: u32 = 0;
static FIND_ME_STATIC: &str = "find_me is in string too";
"#;

    /// AC1: struct definition found; comment and string occurrences excluded.
    #[test]
    fn ac1_rust_finds_struct_definition_not_comment() {
        let result = find_symbols(RUST_FIXTURE, "test.rs", "FindMe", SymbolKind::Struct);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC1: exactly one FindMe struct definition, got: {matches:?}"
        );
        assert_eq!(matches[0].name, "FindMe");
        assert_eq!(matches[0].kind, SymbolKind::Struct);
        // Comment at line 1 and doc comment at line 6 must NOT produce matches.
    }

    /// AC1: function definition found, not comment/string occurrences.
    #[test]
    fn ac1_rust_finds_function_definition_not_false_positives() {
        let result = find_symbols(RUST_FIXTURE, "test.rs", "find_me", SymbolKind::Function);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        // Only the free function `pub fn find_me()` — not the method (different kind).
        assert_eq!(
            matches.len(),
            1,
            "AC1: exactly one find_me function, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Function);
    }

    /// AC1: method search finds method, not free function.
    #[test]
    fn ac1_rust_finds_method_not_free_function() {
        let result = find_symbols(RUST_FIXTURE, "test.rs", "find_me", SymbolKind::Method);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC1: exactly one find_me method, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Method);
    }

    /// AC1: const search finds const, not string literal containing the name.
    #[test]
    fn ac1_rust_const_not_string_literal() {
        let result = find_symbols(RUST_FIXTURE, "test.rs", "FIND_ME_CONST", SymbolKind::Const);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(matches.len(), 1, "AC1: one const, got: {matches:?}");
        assert_eq!(matches[0].kind, SymbolKind::Const);
    }

    /// AC1: span fields are valid.
    #[test]
    fn ac1_rust_match_span_is_valid() {
        let result = find_symbols(RUST_FIXTURE, "test.rs", "FindMe", SymbolKind::Struct);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        let m = &matches[0];
        assert!(m.start_byte < m.end_byte, "span start < end");
        assert!(m.end_byte <= RUST_FIXTURE.len(), "span within source");
    }

    /// AC1: unknown symbol → empty Found (not an error).
    #[test]
    fn ac1_rust_unknown_symbol_returns_empty() {
        let result = find_symbols(
            RUST_FIXTURE,
            "test.rs",
            "NoSuchSymbol",
            SymbolKind::Function,
        );
        assert!(
            matches!(result, SymbolResult::Found(ref m) if m.is_empty()),
            "unknown symbol must return empty Found, got {result:?}"
        );
    }

    // ── TDD step 2 (RED → GREEN): Go grammar ─────────────────────────────────

    const GO_FIXTURE: &[u8] = br#"package main

import "fmt"

// find_me is also in a comment
func FindMe(a, b int) int {
    // find_me in body comment
    return a + b
}

func other() {}

type MyStruct struct {
    Field int
}

type MyInterface interface {
    Method() error
}

const FindMeConst = 42

func (s MyStruct) FindMe() {}
"#;

    /// AC2: Go function definition found.
    #[test]
    fn ac2_go_finds_function_definition() {
        let result = find_symbols(GO_FIXTURE, "main.go", "FindMe", SymbolKind::Function);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one FindMe function in Go, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Function);
    }

    /// AC2: Go method definition found.
    #[test]
    fn ac2_go_finds_method_definition() {
        let result = find_symbols(GO_FIXTURE, "main.go", "FindMe", SymbolKind::Method);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one FindMe method in Go, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Method);
    }

    /// AC2: Go struct type found.
    #[test]
    fn ac2_go_finds_struct_type() {
        let result = find_symbols(GO_FIXTURE, "main.go", "MyStruct", SymbolKind::Struct);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one MyStruct in Go, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Struct);
    }

    /// AC2: Go interface found (mapped as Trait).
    #[test]
    fn ac2_go_finds_interface_as_trait() {
        let result = find_symbols(GO_FIXTURE, "main.go", "MyInterface", SymbolKind::Trait);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one MyInterface (trait) in Go, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Trait);
    }

    /// AC2: Go const found.
    #[test]
    fn ac2_go_finds_const() {
        let result = find_symbols(GO_FIXTURE, "main.go", "FindMeConst", SymbolKind::Const);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one FindMeConst in Go, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Const);
    }

    // ── TDD step 3 (RED → GREEN): PHP grammar ────────────────────────────────

    const PHP_FIXTURE: &[u8] = br#"<?php

// FindMe is also in a comment
class FindMe {
    // FindMe in body
    public function findMethod() {}
    public function other() {}
    const FIND_CONST = 1;
}

interface FindMeInterface {
    public function findMethod();
}

trait FindMeTrait {
    public function findMethod() {}
}

function findFunction() {}
"#;

    /// AC2: PHP class definition found.
    #[test]
    fn ac2_php_finds_class_definition() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "FindMe", SymbolKind::Class);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one FindMe class in PHP, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Class);
    }

    /// AC2: PHP function definition found.
    #[test]
    fn ac2_php_finds_function_definition() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "findFunction", SymbolKind::Function);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one findFunction in PHP, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Function);
    }

    /// AC2: PHP method found (inside class, interface, trait bodies).
    #[test]
    fn ac2_php_finds_method_across_class_interface_trait() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "findMethod", SymbolKind::Method);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        // Three `findMethod` definitions: in class FindMe, interface FindMeInterface,
        // and trait FindMeTrait.
        assert_eq!(
            matches.len(),
            3,
            "AC2: three findMethod definitions across class/interface/trait, got: {matches:?}"
        );
        for m in &matches {
            assert_eq!(m.kind, SymbolKind::Method);
        }
    }

    /// AC2: PHP interface found (mapped as Trait).
    #[test]
    fn ac2_php_finds_interface_as_trait() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "FindMeInterface", SymbolKind::Trait);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "AC2: one FindMeInterface as trait, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Trait);
    }

    /// AC2: PHP trait found.
    #[test]
    fn ac2_php_finds_trait_declaration() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "FindMeTrait", SymbolKind::Trait);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(matches.len(), 1, "AC2: one FindMeTrait, got: {matches:?}");
        assert_eq!(matches[0].kind, SymbolKind::Trait);
    }

    /// Regression: PHP const_element has no named fields — must scan children by kind.
    /// find_symbols(PHP_FIXTURE, "app.php", "FIND_CONST", Const) previously returned
    /// Found([]) because const_element.child_by_field_name("name") is always None.
    #[test]
    fn php_const_inside_class_is_found() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "FIND_CONST", SymbolKind::Const);
        let matches = match result {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "PHP const inside class must be found, got: {matches:?}"
        );
        assert_eq!(matches[0].kind, SymbolKind::Const);
        assert_eq!(matches[0].name, "FIND_CONST");
    }

    /// Regression: Go const_spec with multiple names — child_by_field_name returns
    /// only the first; children_by_field_name is required to reach subsequent names.
    #[test]
    fn go_multi_name_const_both_names_findable() {
        let source = b"package main\nconst A, B = 1, 2\n";
        let result_a = find_symbols(source, "file.go", "A", SymbolKind::Const);
        let result_b = find_symbols(source, "file.go", "B", SymbolKind::Const);

        let matches_a = match result_a {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found for A, got {other:?}"),
        };
        let matches_b = match result_b {
            SymbolResult::Found(m) => m,
            other => panic!("expected Found for B, got {other:?}"),
        };

        assert_eq!(
            matches_a.len(),
            1,
            "Go multi-name const A must be found, got: {matches_a:?}"
        );
        assert_eq!(matches_a[0].name, "A");
        assert_eq!(
            matches_b.len(),
            1,
            "Go multi-name const B must be found (was silently dropped), got: {matches_b:?}"
        );
        assert_eq!(matches_b[0].name, "B");
    }

    // ── TDD step 4 (RED → GREEN): not-applicable-kind (OP1/AC3) ─────────────

    /// AC3: Go — kind not applicable → NotApplicable (not an error).
    #[test]
    fn ac3_go_enum_kind_not_applicable_returns_empty() {
        // Go has no enum keyword; SymbolKind::Enum is not applicable.
        let result = find_symbols(GO_FIXTURE, "main.go", "anything", SymbolKind::Enum);
        assert!(
            matches!(result, SymbolResult::NotApplicable),
            "AC3: Enum is not applicable in Go, expected NotApplicable, got {result:?}"
        );
    }

    /// AC3: Go — Impl kind not applicable.
    #[test]
    fn ac3_go_impl_kind_not_applicable() {
        let result = find_symbols(GO_FIXTURE, "main.go", "anything", SymbolKind::Impl);
        assert!(
            matches!(result, SymbolResult::NotApplicable),
            "AC3: Impl is not applicable in Go, got {result:?}"
        );
    }

    /// AC3: PHP — Impl kind not applicable.
    #[test]
    fn ac3_php_impl_kind_not_applicable() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "anything", SymbolKind::Impl);
        assert!(
            matches!(result, SymbolResult::NotApplicable),
            "AC3: Impl is not applicable in PHP, got {result:?}"
        );
    }

    /// AC3: PHP — Struct kind not applicable.
    #[test]
    fn ac3_php_struct_kind_not_applicable() {
        let result = find_symbols(PHP_FIXTURE, "app.php", "anything", SymbolKind::Struct);
        assert!(
            matches!(result, SymbolResult::NotApplicable),
            "AC3: Struct is not applicable in PHP, got {result:?}"
        );
    }

    /// AC3: Unsupported language → Unsupported (not NotApplicable).
    #[test]
    fn ac3_unsupported_language_returns_unsupported() {
        let result = find_symbols(b"", "hello.py", "anything", SymbolKind::Function);
        assert!(
            matches!(result, SymbolResult::Unsupported { .. }),
            "AC3: unknown language must return Unsupported, got {result:?}"
        );
    }

    // ── MIN-01: language_label — extension extraction ─────────────────────────

    /// MIN-01: path hint with directory component produces the extension as label.
    #[test]
    fn min01_unsupported_path_with_directory_uses_extension() {
        let result = find_symbols(
            b"# heading",
            "lab/notes.md",
            "anything",
            SymbolKind::Function,
        );
        assert!(
            matches!(result, SymbolResult::Unsupported { ref language } if language == "md"),
            "MIN-01: lab/notes.md must produce language=\"md\", got {result:?}"
        );
    }

    /// MIN-01: bare extension path (no directory) produces the extension.
    #[test]
    fn min01_unsupported_simple_path_uses_extension() {
        let result = find_symbols(b"", "foo.py", "anything", SymbolKind::Function);
        assert!(
            matches!(result, SymbolResult::Unsupported { ref language } if language == "py"),
            "MIN-01: foo.py must produce language=\"py\", got {result:?}"
        );
    }

    /// MIN-01: dotless hint (bare language id) passes through unchanged.
    #[test]
    fn min01_unsupported_dotless_hint_passthrough() {
        let result = find_symbols(b"", "typescript", "anything", SymbolKind::Function);
        assert!(
            matches!(result, SymbolResult::Unsupported { ref language } if language == "typescript"),
            "MIN-01: dotless hint \"typescript\" must produce language=\"typescript\", got {result:?}"
        );
    }

    /// MIN-01: language_label is directly testable as a public helper.
    #[test]
    fn min01_language_label_unit() {
        assert_eq!(language_label("foo.py"), "py");
        assert_eq!(language_label("lab/notes.md"), "md");
        assert_eq!(language_label("typescript"), "typescript");
        assert_eq!(language_label("src/main.rs"), "rs");
        assert_eq!(language_label("UPPER.PY"), "py");
    }

    // ── TDD step 5 (RED → GREEN): malformed input (UN1/AC4) ──────────────────

    /// AC4: malformed Rust — partial results, no crash.
    #[test]
    fn ac4_malformed_rust_no_crash() {
        let broken = b"pub struct Good {} fn broken( { pub fn FindMe() {} ";
        let result = find_symbols(broken, "broken.rs", "Good", SymbolKind::Struct);
        // Must return Found (possibly empty or partial), not panic.
        assert!(
            matches!(result, SymbolResult::Found(_)),
            "AC4: malformed Rust must return Found, got {result:?}"
        );
    }

    /// AC4: malformed Go — partial results, no crash.
    #[test]
    fn ac4_malformed_go_no_crash() {
        let broken = b"package main\nfunc FindMe( {\n";
        let result = find_symbols(broken, "broken.go", "FindMe", SymbolKind::Function);
        assert!(
            matches!(result, SymbolResult::Found(_)),
            "AC4: malformed Go must return Found, got {result:?}"
        );
    }

    /// AC4: malformed PHP — partial results, no crash.
    #[test]
    fn ac4_malformed_php_no_crash() {
        let broken = b"<?php\nclass FindMe {\n";
        let result = find_symbols(broken, "broken.php", "FindMe", SymbolKind::Class);
        assert!(
            matches!(result, SymbolResult::Found(_)),
            "AC4: malformed PHP must return Found, got {result:?}"
        );
    }

    // ── Language hint detection ───────────────────────────────────────────────

    #[test]
    fn language_hint_rust_extensions() {
        assert_eq!(
            SupportedLanguage::from_hint("src/main.rs"),
            Some(SupportedLanguage::Rust)
        );
        assert_eq!(
            SupportedLanguage::from_hint("rust"),
            Some(SupportedLanguage::Rust)
        );
        assert_eq!(
            SupportedLanguage::from_hint("RUST"),
            Some(SupportedLanguage::Rust)
        );
    }

    #[test]
    fn language_hint_go_extensions() {
        assert_eq!(
            SupportedLanguage::from_hint("main.go"),
            Some(SupportedLanguage::Go)
        );
        assert_eq!(
            SupportedLanguage::from_hint("go"),
            Some(SupportedLanguage::Go)
        );
    }

    #[test]
    fn language_hint_php_extensions() {
        assert_eq!(
            SupportedLanguage::from_hint("app.php"),
            Some(SupportedLanguage::Php)
        );
        assert_eq!(
            SupportedLanguage::from_hint("php"),
            Some(SupportedLanguage::Php)
        );
    }

    #[test]
    fn language_hint_unknown_returns_none() {
        assert_eq!(SupportedLanguage::from_hint("main.py"), None);
        assert_eq!(SupportedLanguage::from_hint("hello.js"), None);
    }

    // ── SymbolKind round-trip ─────────────────────────────────────────────────

    #[test]
    fn symbol_kind_round_trip() {
        for s in &[
            "function",
            "struct",
            "enum",
            "trait",
            "impl",
            "method",
            "module",
            "type_alias",
            "const",
            "static",
            "macro_def",
            "class",
        ] {
            let kind = SymbolKind::parse_kind(s).unwrap_or_else(|| panic!("must parse '{s}'"));
            assert_eq!(kind.as_str(), *s, "round-trip for '{s}'");
        }
    }

    // ── to_sdk_value ──────────────────────────────────────────────────────────

    #[test]
    fn symbol_match_to_sdk_value_has_expected_keys() {
        use plugin_sdk::Value;

        let m = SymbolMatch {
            kind: SymbolKind::Function,
            name: "foo".to_owned(),
            start_byte: 0,
            end_byte: 20,
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 5,
        };

        let value = m.to_sdk_value();
        let Value::Map(pairs) = &value else {
            panic!("expected Map, got {value:?}");
        };
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        for expected in &["kind", "name", "start_byte", "end_byte", "start_row"] {
            assert!(
                keys.contains(expected),
                "must have key '{expected}', got {keys:?}"
            );
        }
    }
}
