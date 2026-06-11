//! `ast` — reference Tree-sitter AST plugin (spec 12c/12d).
//!
//! Compiles to `wasm32-wasip1` using the `#[plugin_main]` macro from
//! `plugin_sdk`. Declares two tools: `get_outline` and `find_symbols`.
//!
//! # Architecture
//!
//! ```text
//! lib.rs (thin wasm export surface)
//!   #[plugin_main]  AstPlugin
//!   call_tool("get_outline", args)
//!     → extract path from args
//!     → host::read_file(path)        ← host capability (U2, never raw fs)
//!     → outline::parse_outline(bytes, path)   [Rust + Go + PHP]
//!     → OutlineResult::Unsupported   → Value::Map { "unsupported": true, ... }
//!     → OutlineResult::Parsed(o)     → o.to_sdk_value()
//!
//!   call_tool("find_symbols", args)
//!     → extract path, symbol_name, kind from args
//!     → host::read_file(path)        ← host capability (U2, never raw fs)
//!     → symbols::find_symbols(bytes, path, name, kind)  [Rust + Go + PHP]
//!     → SymbolResult::Unsupported    → Value::Map { "unsupported": true, ... }
//!     → SymbolResult::NotApplicable  → Value::Map { "matches": [] }
//!     → SymbolResult::Found(m)       → Value::Map { "matches": [...] }
//!
//! outline.rs (host-testable logic)
//!   parse_outline(source, hint)
//!   walk_top_level / walk_go_top_level / walk_php_top_level  ← multi-language AST walker
//!   Outline { items: [OutlineItem { kind, name, span }] }
//!
//! symbols.rs (host-testable logic)
//!   find_symbols(source, hint, name, kind)
//!   walk_rust_symbols / walk_go_symbols / walk_php_symbols
//!   SymbolResult::Found([SymbolMatch { kind, name, span }])
//! ```
//!
//! # Compilation targets
//!
//! - **Host (`x86_64-unknown-linux-gnu`)**: `cargo test -p ast` runs the
//!   `outline::tests` and `symbols::tests` suites with native tree-sitter — no WASI SDK needed.
//! - **Wasm (`wasm32-wasip1`)**: requires `CC_wasm32_wasip1` and `AR_wasm32_wasip1`
//!   pointing at the WASI SDK clang (see `docs/spikes/12a-tree-sitter-wasm-feasibility.md`).
//!
//! # WASI SDK env vars (from 12a recipe)
//!
//! ```text
//! CC_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang
//! AR_wasm32_wasip1=~/.cache/tree-sitter/wasi-sdk/bin/llvm-ar
//! cargo build -p ast --target wasm32-wasip1
//! ```

pub mod outline;
pub mod symbols;

use plugin_sdk::{
    ABI_VERSION, HookKind, HookPayload, Plugin, PluginManifest, SdkError, ToolDesc, Value,
    plugin_main,
};

// ── Plugin struct ─────────────────────────────────────────────────────────────

/// The AST plugin: exposes `get_outline` for Tree-sitter parsing.
///
/// Reads file content via the host capability (`plugin_sdk::host::read_file`),
/// parses it with Tree-sitter, and returns a structural outline.
///
/// Apply `#[plugin_main]` to generate the four wasm export symbols automatically.
#[plugin_main]
pub struct AstPlugin;

// ── Plugin trait ──────────────────────────────────────────────────────────────

impl Plugin for AstPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name: "ast".to_owned(),
            version: "0.2.0".to_owned(),
            abi: ABI_VERSION,
            tools: vec![
                ToolDesc {
                    name: "get_outline".to_owned(),
                    description: "Return a structural outline (functions, structs, methods, \
                                  traits, enums, impl blocks) for a workspace-relative source \
                                  file. Supports Rust (.rs), Go (.go), and PHP (.php). \
                                  Returns an unsupported-language result for other file types."
                        .to_owned(),
                    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to the source file"}},"required":["path"]}"#.to_owned(),
                },
                ToolDesc {
                    name: "find_symbols".to_owned(),
                    description: "Find precise definition locations for a named symbol of a \
                                  given kind in a workspace-relative source file. Uses the \
                                  error-tolerant syntax tree to exclude false positives in \
                                  comments and string literals. Supports Rust (.rs), Go (.go), \
                                  and PHP (.php). Returns an empty result for kinds not \
                                  applicable to the language (e.g. enum in Go)."
                        .to_owned(),
                    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to the source file"},"symbol_name":{"type":"string","description":"Name of the symbol to search for"},"kind":{"type":"string","description":"Symbol kind: function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"}},"required":["path","symbol_name","kind"]}"#.to_owned(),
                },
            ],
            hooks: vec![],
        }
    }

    fn call_tool(name: &str, args: Value) -> Result<Value, SdkError> {
        match name {
            "get_outline" => ast_get_outline_impl(args),
            "find_symbols" => ast_find_symbols_impl(args),
            other => Err(SdkError::ToolNotFound(other.to_owned())),
        }
    }

    fn on_hook(_kind: HookKind, _payload: HookPayload) {
        // No hooks subscribed.
    }
}

// ── Tool implementation ───────────────────────────────────────────────────────

/// Implementation of the `get_outline` tool.
///
/// Extracts the `path` argument, reads the file via the host capability,
/// parses with Tree-sitter, and returns the outline as a [`Value::Map`].
///
/// # Return value
///
/// On success (Rust file):
/// ```json
/// { "items": [ { "kind": "function", "name": "my_fn", "start_byte": 0, ... }, ... ] }
/// ```
///
/// On unsupported language (non-Rust file):
/// ```json
/// { "unsupported": true, "language": "foo.py" }
/// ```
///
/// # Errors
///
/// Returns `SdkError::InvalidArgs` if `path` is missing or not a text value.
/// Returns `SdkError::CallFailed` if the host cannot read the file.
fn ast_get_outline_impl(args: Value) -> Result<Value, SdkError> {
    // Extract workspace-relative path from args.
    let path = extract_text_field(&args, "path")?;

    // Read file content through the host capability (U2 — never raw fs).
    let content = plugin_sdk::host::read_file(path)
        .ok_or_else(|| SdkError::CallFailed(format!("host could not read file: {path}")))?;

    // Parse the outline. Language hint = file path (extension used by outline::is_rust_hint).
    let result = outline::parse_outline(&content, path);

    let value = match result {
        outline::OutlineResult::Unsupported { language } => {
            // OP1/AC2: return a typed unsupported-language result, not an error.
            Value::Map(vec![
                ("unsupported".to_owned(), Value::Bool(true)),
                ("language".to_owned(), Value::Text(language)),
            ])
        }
        outline::OutlineResult::Parsed(o) => {
            // EV1/AC1: full or partial outline (UN1/AC3: tree-sitter is error-tolerant).
            o.to_sdk_value()
        }
    };

    Ok(value)
}

/// Implementation of the `find_symbols` tool.
///
/// Extracts `path`, `symbol_name`, and `kind` arguments, reads the file via the
/// host capability, and calls [`symbols::find_symbols`].
///
/// # Return value
///
/// On success:
/// ```json
/// { "matches": [ { "kind": "function", "name": "foo", "start_byte": 0, ... }, ... ] }
/// ```
///
/// On unsupported language:
/// ```json
/// { "unsupported": true, "language": "foo.py" }
/// ```
///
/// On kind not applicable to the language (OP1/AC3):
/// ```json
/// { "matches": [] }
/// ```
///
/// # Errors
///
/// Returns `SdkError::InvalidArgs` if any required field is missing/wrong type.
/// Returns `SdkError::CallFailed` if the host cannot read the file.
fn ast_find_symbols_impl(args: Value) -> Result<Value, SdkError> {
    let path = extract_text_field(&args, "path")?;
    let symbol_name = extract_text_field(&args, "symbol_name")?;
    let kind_str = extract_text_field(&args, "kind")?;

    let kind = symbols::SymbolKind::parse_kind(kind_str).ok_or_else(|| {
        SdkError::InvalidArgs(format!(
            "unknown symbol kind '{kind_str}'; valid: function|struct|enum|trait|impl|method|\
             module|type_alias|const|static|macro_def|class"
        ))
    })?;

    let content = plugin_sdk::host::read_file(path)
        .ok_or_else(|| SdkError::CallFailed(format!("host could not read file: {path}")))?;

    let result = symbols::find_symbols(&content, path, symbol_name, kind);

    let value = match result {
        symbols::SymbolResult::Unsupported { language } => {
            // OP1: unsupported language returns typed result, not error.
            Value::Map(vec![
                ("unsupported".to_owned(), Value::Bool(true)),
                ("language".to_owned(), Value::Text(language)),
            ])
        }
        symbols::SymbolResult::NotApplicable => {
            // OP1/AC3: kind not applicable → empty matches, not error.
            Value::Map(vec![("matches".to_owned(), Value::List(vec![]))])
        }
        symbols::SymbolResult::Found(matches) => {
            let items: Vec<Value> = matches
                .iter()
                .map(symbols::SymbolMatch::to_sdk_value)
                .collect();
            Value::Map(vec![("matches".to_owned(), Value::List(items))])
        }
    };

    Ok(value)
}

/// Extract a `&str`-valued field from a `Value::Map`.
fn extract_text_field<'a>(args: &'a Value, field: &str) -> Result<&'a str, SdkError> {
    match args {
        Value::Map(pairs) => {
            for (key, val) in pairs {
                if key == field {
                    return match val {
                        Value::Text(s) => Ok(s.as_str()),
                        _ => Err(SdkError::InvalidArgs(format!(
                            "field '{field}' must be a string"
                        ))),
                    };
                }
            }
            Err(SdkError::InvalidArgs(format!(
                "missing required field '{field}'"
            )))
        }
        _ => Err(SdkError::InvalidArgs(
            "args must be a map object".to_owned(),
        )),
    }
}

// ── Host-side tests (lib-level: AstPlugin API + call_tool dispatch) ───────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_declares_both_ast_tools() {
        let manifest = AstPlugin::init();
        assert_eq!(manifest.name, "ast");
        assert_eq!(manifest.abi, ABI_VERSION);
        assert_eq!(
            manifest.tools.len(),
            2,
            "manifest must have exactly 2 tools"
        );
        let tool_names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            tool_names.contains(&"get_outline"),
            "manifest must have get_outline, got: {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"find_symbols"),
            "manifest must have find_symbols, got: {tool_names:?}"
        );
        assert!(manifest.hooks.is_empty());
    }

    #[test]
    fn call_tool_unknown_returns_not_found() {
        let result = AstPlugin::call_tool("unknown_tool", Value::Null);
        assert!(
            matches!(result, Err(SdkError::ToolNotFound(_))),
            "expected ToolNotFound, got {result:?}"
        );
    }

    #[test]
    fn call_tool_missing_path_returns_invalid_args() {
        let args = Value::Map(vec![]);
        let result = AstPlugin::call_tool("get_outline", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "expected InvalidArgs for missing path, got {result:?}"
        );
    }

    #[test]
    fn call_tool_non_map_args_returns_invalid_args() {
        let result = AstPlugin::call_tool("get_outline", Value::Null);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "expected InvalidArgs for non-map args, got {result:?}"
        );
    }

    #[test]
    fn call_tool_on_host_read_file_returns_call_failed() {
        // On the host target, host::read_file is a stub returning None.
        // get_outline must return CallFailed, not panic.
        let args = Value::Map(vec![(
            "path".to_owned(),
            Value::Text("src/main.rs".to_owned()),
        )]);
        let result = AstPlugin::call_tool("get_outline", args);
        assert!(
            matches!(result, Err(SdkError::CallFailed(_))),
            "expected CallFailed on host (read_file stub returns None), got {result:?}"
        );
    }

    #[test]
    fn call_tool_find_symbols_missing_args_returns_invalid_args() {
        // Missing symbol_name and kind.
        let args = Value::Map(vec![(
            "path".to_owned(),
            Value::Text("src/main.rs".to_owned()),
        )]);
        let result = AstPlugin::call_tool("find_symbols", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "missing symbol_name must return InvalidArgs, got {result:?}"
        );
    }

    #[test]
    fn call_tool_find_symbols_invalid_kind_returns_invalid_args() {
        let args = Value::Map(vec![
            ("path".to_owned(), Value::Text("src/main.rs".to_owned())),
            ("symbol_name".to_owned(), Value::Text("Foo".to_owned())),
            ("kind".to_owned(), Value::Text("not_a_kind".to_owned())),
        ]);
        let result = AstPlugin::call_tool("find_symbols", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "invalid kind must return InvalidArgs, got {result:?}"
        );
    }

    #[test]
    fn call_tool_find_symbols_host_read_fail_returns_call_failed() {
        let args = Value::Map(vec![
            ("path".to_owned(), Value::Text("src/main.rs".to_owned())),
            ("symbol_name".to_owned(), Value::Text("Foo".to_owned())),
            ("kind".to_owned(), Value::Text("struct".to_owned())),
        ]);
        let result = AstPlugin::call_tool("find_symbols", args);
        assert!(
            matches!(result, Err(SdkError::CallFailed(_))),
            "host read failure must return CallFailed, got {result:?}"
        );
    }
}
