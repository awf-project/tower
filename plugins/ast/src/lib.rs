//! `ast` — reference Tree-sitter AST plugin (spec 12c/12d).
//!
//! Compiles to `wasm32-wasip1` using the `#[plugin_main]` macro from
//! `plugin_sdk`. Declares four tools: `get_outline`, `find_symbols`,
//! `search_symbols`, and `reindex`.
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
//!     → reindex_path(path, &outline) ← update in-memory index (index-as-you-go)
//!     → OutlineResult::Unsupported   → Value::Map { "unsupported": true, ... }
//!     → OutlineResult::Parsed(o)     → o.to_sdk_value()
//!
//!   call_tool("find_symbols", args)
//!     → extract path, symbol_name, kind from args
//!     → host::read_file(path)        ← host capability (U2, never raw fs)
//!     → symbols::find_symbols(bytes, path, name, kind)  [Rust + Go + PHP]
//!     → reindex_path_from_content(path, bytes) ← update index with content already read
//!     → SymbolResult::Unsupported    → Value::Map { "unsupported": true, ... }
//!     → SymbolResult::NotApplicable  → Value::Map { "matches": [] }
//!     → SymbolResult::Found(m)       → Value::Map { "matches": [...] }
//!
//!   call_tool("search_symbols", args)
//!     → extract name, optional kind from args
//!     → with_index(|idx| idx.search(name, kind))
//!     → Value::Map { "matches": [...per-entry Map with path/kind/name/spans...] }
//!
//!   call_tool("reindex", {})
//!     → host::list_files()                  ← enumerate all workspace-relative paths
//!     → build fresh SymbolIndex from scratch
//!     → for each path: host::read_file(path) → parse_outline → index_file
//!     → replace thread_local INDEX, persist once
//!     → Value::Map { "indexed_files": n, "symbols": m }
//!
//!   on_hook(FileChanged, { path })
//!     → reindex_path(&path)    ← read_file + parse_outline + index_file / remove_file
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
//!
//! index.rs (host-testable logic)
//!   SymbolIndex { by_path: BTreeMap<String, Vec<SymbolEntry>> }
//!   index_file, remove_file, search, to_bytes, from_bytes
//! ```
//!
//! # Compilation targets
//!
//! - **Host (`x86_64-unknown-linux-gnu`)**: `cargo test -p ast` runs the
//!   `outline::tests`, `symbols::tests`, and `index::tests` suites with native
//!   tree-sitter — no WASI SDK needed.
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

pub mod index;
pub mod outline;
pub mod symbols;
pub(crate) mod text;

use std::cell::RefCell;

use plugin_sdk::{
    ABI_VERSION, HookKind, HookPayload, Plugin, PluginManifest, SdkError, ToolDesc, Value,
    plugin_main,
};

use crate::index::SymbolIndex;

// ── Thread-local symbol index ─────────────────────────────────────────────────
//
// WASM is single-threaded; thread_local! is the correct pattern for per-instance
// mutable state. `Option<SymbolIndex>` = None until first use (lazy-loaded from
// the host store on first access).
thread_local! {
    static INDEX: RefCell<Option<SymbolIndex>> = const { RefCell::new(None) };
}

/// Access (and lazy-load) the symbol index.
///
/// On first call the index is loaded from `host::ast_store_get("symbols")` via
/// `SymbolIndex::from_bytes`. If the key is absent or the bytes are corrupt,
/// `SymbolIndex::default()` is used (empty index). On the host target
/// `ast_store_get` panics, so the `#[cfg]` gate ensures the store call is
/// wasm32-only; on host the index always starts empty.
///
/// The closure `f` receives a mutable reference to the loaded `SymbolIndex`.
fn with_index<F, R>(f: F) -> R
where
    F: FnOnce(&mut SymbolIndex) -> R,
{
    INDEX.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            *opt = Some(load_index_from_store());
        }
        f(opt.as_mut().expect("index always Some after init"))
    })
}

/// Load the symbol index from the host store, or return a default empty index.
///
/// Only calls `host::ast_store_get` on `wasm32` — the stub panics on host.
/// On non-wasm targets returns `SymbolIndex::default()` (the index is purely
/// in-memory; persistence only runs inside the wasm guest).
fn load_index_from_store() -> SymbolIndex {
    #[cfg(target_arch = "wasm32")]
    {
        plugin_sdk::host::ast_store_get("symbols")
            .and_then(|bytes| SymbolIndex::from_bytes(&bytes))
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        SymbolIndex::default()
    }
}

/// Persist the current index state to the host store.
///
/// Only calls `host::ast_store_put` on `wasm32`; no-op on host.
///
/// # Decision: persist-whole-blob-per-change
/// Why: the index is small (workspace of source files, entries are compact),
///      and a single postcard blob keeps the implementation trivial — no
///      partial-update protocol needed against AstIndexPort.
/// Trade-off: O(n) serialisation cost on every mutation. For a large workspace
///            (thousands of files) consider a write-behind cache or delta protocol.
fn persist_index(idx: &SymbolIndex) {
    #[cfg(target_arch = "wasm32")]
    {
        let bytes = idx.to_bytes();
        // Ignore errors: a failed persist degrades to a warm-restart rebuild,
        // it does not corrupt the in-memory state the caller already updated.
        let _ = plugin_sdk::host::ast_store_put("symbols", &bytes);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = idx; // no-op: host tests exercise pure in-memory index logic
    }
}

/// Re-index `path` by reading its content and parsing an outline.
///
/// If `host::read_file` returns `None` (file deleted or not found) the path is
/// removed from the index instead. Persists the updated index blob to the host
/// store after every mutation.
///
/// Called from `on_hook(FileChanged)` and as index-as-you-go from tool impls.
fn reindex_path(path: &str) {
    let bytes_opt = plugin_sdk::host::read_file(path);
    reindex_with_content(path, bytes_opt.as_deref());
}

/// Re-index `path` with already-read content bytes.
///
/// Avoids a second `read_file` call when the tool impl has already read the
/// file. If `content` is `None` the path is removed from the index.
fn reindex_with_content(path: &str, content: Option<&[u8]>) {
    match content {
        Some(bytes) => {
            let outline = match outline::parse_outline(bytes, path) {
                outline::OutlineResult::Parsed(o) => o,
                // Unsupported language: nothing to index; remove stale entries.
                outline::OutlineResult::Unsupported { .. } => {
                    with_index(|idx| {
                        idx.remove_file(path);
                        persist_index(idx);
                    });
                    return;
                }
            };
            with_index(|idx| {
                idx.index_file(path, &outline);
                persist_index(idx);
            });
        }
        None => {
            // File not found or deleted: remove its entries.
            with_index(|idx| {
                idx.remove_file(path);
                persist_index(idx);
            });
        }
    }
}

// ── Plugin struct ─────────────────────────────────────────────────────────────

/// The AST plugin: exposes `get_outline`, `find_symbols`, `search_symbols`, and
/// `reindex` with a persistent cross-file symbol index for the workspace.
///
/// Apply `#[plugin_main]` to generate the four wasm export symbols automatically.
#[plugin_main]
pub struct AstPlugin;

// ── Plugin trait ──────────────────────────────────────────────────────────────

impl Plugin for AstPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name: "ast".to_owned(),
            version: "0.3.0".to_owned(),
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
                ToolDesc {
                    name: "search_symbols".to_owned(),
                    description: "Search the in-memory cross-file symbol index for symbols \
                                  matching a given name and optional kind. The index is built \
                                  incrementally as files are read (index-as-you-go) and updated \
                                  via FileChanged hooks. Returns all matching symbols across all \
                                  indexed files with their path and location."
                        .to_owned(),
                    schema_json: r#"{"type":"object","properties":{"name":{"type":"string","description":"Exact symbol name to search for"},"kind":{"type":"string","description":"Optional symbol kind filter: function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"}},"required":["name"]}"#.to_owned(),
                },
                ToolDesc {
                    name: "reindex".to_owned(),
                    description: "Rebuild the whole-project symbol index by enumerating all \
                                  workspace files and parsing each. Forces a full reindex; use \
                                  after large external changes or on a cold cache."
                        .to_owned(),
                    schema_json: r#"{"type":"object","properties":{},"additionalProperties":false}"#.to_owned(),
                },
            ],
            hooks: vec![HookKind::FileChanged],
        }
    }

    fn call_tool(name: &str, args: Value) -> Result<Value, SdkError> {
        match name {
            "get_outline" => ast_get_outline_impl(args),
            "find_symbols" => ast_find_symbols_impl(args),
            "search_symbols" => ast_search_symbols_impl(args),
            "reindex" => ast_reindex_impl(),
            other => Err(SdkError::ToolNotFound(other.to_owned())),
        }
    }

    fn on_hook(kind: HookKind, payload: HookPayload) {
        if let (HookKind::FileChanged, HookPayload::FileChanged { path }) = (kind, payload) {
            reindex_path(&path);
        }
        // All other hook kinds are ignored (not subscribed).
    }
}

// ── Tool implementation ───────────────────────────────────────────────────────

/// Implementation of the `get_outline` tool.
///
/// Extracts the `path` argument, reads the file via the host capability,
/// parses with Tree-sitter, updates the in-memory symbol index (index-as-you-go),
/// and returns the outline as a [`Value::Map`].
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
    let path = extract_text_field(&args, "path")?;

    let content = plugin_sdk::host::read_file(path)
        .ok_or_else(|| SdkError::CallFailed(format!("host could not read file: {path}")))?;

    let result = outline::parse_outline(&content, path);

    // Index-as-you-go: update the index with the content we already read.
    // On the host target (tests) reindex_with_content is a no-op for persist.
    reindex_with_content(path, Some(&content));

    let value = match result {
        outline::OutlineResult::Unsupported { language } => Value::Map(vec![
            ("unsupported".to_owned(), Value::Bool(true)),
            ("language".to_owned(), Value::Text(language)),
        ]),
        outline::OutlineResult::Parsed(o) => o.to_sdk_value(),
    };

    Ok(value)
}

/// Implementation of the `find_symbols` tool.
///
/// Extracts `path`, `symbol_name`, and `kind` arguments, reads the file via the
/// host capability, updates the in-memory symbol index (index-as-you-go), and
/// calls [`symbols::find_symbols`].
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

    // Index-as-you-go: reuse the content already read.
    reindex_with_content(path, Some(&content));

    let result = symbols::find_symbols(&content, path, symbol_name, kind);

    let value = match result {
        symbols::SymbolResult::Unsupported { language } => Value::Map(vec![
            ("unsupported".to_owned(), Value::Bool(true)),
            ("language".to_owned(), Value::Text(language)),
        ]),
        symbols::SymbolResult::NotApplicable => {
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

/// Implementation of the `search_symbols` tool.
///
/// Searches the in-memory symbol index for all symbols matching `name` (exact
/// match) and an optional `kind` filter across all indexed files.
///
/// # Return value
///
/// ```json
/// {
///   "matches": [
///     {
///       "path":       "src/lib.rs",
///       "kind":       "function",
///       "name":       "my_fn",
///       "start_byte": 10,
///       "end_byte":   50,
///       "start_row":  2,
///       "start_col":  0,
///       "end_row":    4,
///       "end_col":    1
///     }
///   ]
/// }
/// ```
///
/// # Errors
///
/// Returns `SdkError::InvalidArgs` if `name` is missing or not a string.
fn ast_search_symbols_impl(args: Value) -> Result<Value, SdkError> {
    let name = extract_text_field(&args, "name")?;
    let kind_opt = extract_optional_text_field(&args, "kind");

    let items: Vec<Value> = with_index(|idx| {
        idx.search(name, kind_opt.as_deref())
            .iter()
            .map(|e| e.to_sdk_value())
            .collect()
    });

    Ok(Value::Map(vec![("matches".to_owned(), Value::List(items))]))
}

/// Implementation of the `reindex` tool.
///
/// Enumerates every workspace file via `host::list_files()`, parses each
/// through `outline::parse_outline`, and builds a **fresh** `SymbolIndex`
/// from scratch. The fresh build naturally prunes any files that no longer
/// exist in the workspace — there is no need for an explicit deletion pass.
///
/// The rebuilt index replaces the `thread_local` state and is persisted once
/// via `host::ast_store_put("symbols", …)`.
///
/// # Decision: fresh-rebuild over merge
///
/// Why: rebuilding from the canonical file list removes stale entries for
///      deleted files automatically; merging would require a diff against the
///      previous index.
/// Trade-off: O(n) parse cost over the whole workspace. Acceptable for the
///            force-reindex use case (large external change / cold cache).
///
/// # cfg-gating
///
/// The body that calls `host::list_files()` and `host::read_file()` is
/// wasm32-only because both stubs panic on the host target. On the host target
/// the function returns a zero-count result map without calling any host fn,
/// keeping host-side `call_tool` dispatch tests safe.
///
/// # Return value
///
/// ```json
/// { "indexed_files": 3, "symbols": 42 }
/// ```
fn ast_reindex_impl() -> Result<Value, SdkError> {
    #[cfg(target_arch = "wasm32")]
    {
        let paths = plugin_sdk::host::list_files();

        // Build a fresh index — do not start from the existing thread-local so
        // that entries for deleted files are automatically pruned.
        let mut fresh_index = SymbolIndex::default();
        let mut indexed_files: i64 = 0;

        for path in &paths {
            let Some(bytes) = plugin_sdk::host::read_file(path) else {
                // File disappeared between list_files and read_file — skip it.
                continue;
            };
            if let outline::OutlineResult::Parsed(o) = outline::parse_outline(&bytes, path) {
                fresh_index.index_file(path, &o);
                indexed_files += 1;
            }
            // Unsupported language: skip (not indexable).
        }

        let total_symbols: i64 = fresh_index
            .by_path
            .values()
            .map(|entries| entries.len() as i64)
            .sum();

        // Replace the thread-local index and persist once.
        INDEX.with(|cell| {
            *cell.borrow_mut() = Some(fresh_index.clone());
        });
        persist_index(&fresh_index);

        Ok(Value::Map(vec![
            ("indexed_files".to_owned(), Value::Integer(indexed_files)),
            ("symbols".to_owned(), Value::Integer(total_symbols)),
        ]))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // On the host target the list_files / read_file stubs panic; return a
        // no-op result so host-side call_tool dispatch tests remain safe.
        Ok(Value::Map(vec![
            ("indexed_files".to_owned(), Value::Integer(0)),
            ("symbols".to_owned(), Value::Integer(0)),
        ]))
    }
}

// ── Field extraction helpers ──────────────────────────────────────────────────

/// Extract a required `&str`-valued field from a `Value::Map`.
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

/// Extract an optional `String`-valued field from a `Value::Map`.
///
/// Returns `None` if the field is absent (not an error); returns `None`
/// silently if the value is not a string (lenient — treat as absent).
fn extract_optional_text_field(args: &Value, field: &str) -> Option<String> {
    match args {
        Value::Map(pairs) => pairs.iter().find(|(k, _)| k == field).and_then(|(_, v)| {
            if let Value::Text(s) = v {
                Some(s.clone())
            } else {
                None
            }
        }),
        _ => None,
    }
}

// ── Host-side tests (lib-level: AstPlugin API + call_tool dispatch) ───────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_declares_four_ast_tools() {
        let manifest = AstPlugin::init();
        assert_eq!(manifest.name, "ast");
        assert_eq!(manifest.abi, ABI_VERSION);
        assert_eq!(
            manifest.tools.len(),
            4,
            "manifest must have exactly 4 tools"
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
        assert!(
            tool_names.contains(&"search_symbols"),
            "manifest must have search_symbols, got: {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"reindex"),
            "manifest must have reindex, got: {tool_names:?}"
        );
    }

    #[test]
    fn manifest_subscribes_file_changed_hook() {
        let manifest = AstPlugin::init();
        assert_eq!(
            manifest.hooks,
            vec![HookKind::FileChanged],
            "manifest must subscribe to FileChanged hook"
        );
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

    #[test]
    fn call_tool_search_symbols_missing_name_returns_invalid_args() {
        let args = Value::Map(vec![]);
        let result = AstPlugin::call_tool("search_symbols", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "missing name must return InvalidArgs, got {result:?}"
        );
    }

    #[test]
    fn call_tool_search_symbols_empty_index_returns_empty_matches() {
        // On host: index is always empty (no wasm store). Must return empty list.
        // Flush the thread-local so this test is not affected by test ordering.
        INDEX.with(|c| *c.borrow_mut() = Some(SymbolIndex::default()));

        let args = Value::Map(vec![(
            "name".to_owned(),
            Value::Text("anything".to_owned()),
        )]);
        let result = AstPlugin::call_tool("search_symbols", args).expect("must succeed");
        let Value::Map(pairs) = &result else {
            panic!("expected Map, got {result:?}");
        };
        let (_, matches_val) = pairs
            .iter()
            .find(|(k, _)| k == "matches")
            .expect("must have 'matches' key");
        assert!(
            matches!(matches_val, Value::List(v) if v.is_empty()),
            "empty index must return empty matches list, got {result:?}"
        );
    }

    #[test]
    fn call_tool_search_symbols_optional_kind_absent_is_ok() {
        // search_symbols with no kind field must succeed (kind is optional).
        INDEX.with(|c| *c.borrow_mut() = Some(SymbolIndex::default()));

        let args = Value::Map(vec![("name".to_owned(), Value::Text("my_fn".to_owned()))]);
        let result = AstPlugin::call_tool("search_symbols", args);
        assert!(
            result.is_ok(),
            "absent optional kind must not error: {result:?}"
        );
    }

    #[test]
    fn call_tool_reindex_on_host_returns_zero_counts_without_panic() {
        // On the host target list_files / read_file stubs panic; reindex must
        // return a zero-count result map without calling any host fn.
        let result = AstPlugin::call_tool("reindex", Value::Map(vec![]))
            .expect("reindex must succeed on host target");
        let Value::Map(pairs) = &result else {
            panic!("expected Map, got {result:?}");
        };
        let indexed = pairs
            .iter()
            .find(|(k, _)| k == "indexed_files")
            .map(|(_, v)| v)
            .expect("must have 'indexed_files' key");
        assert!(
            matches!(indexed, Value::Integer(0)),
            "host target must return indexed_files=0, got {result:?}"
        );
        let symbols = pairs
            .iter()
            .find(|(k, _)| k == "symbols")
            .map(|(_, v)| v)
            .expect("must have 'symbols' key");
        assert!(
            matches!(symbols, Value::Integer(0)),
            "host target must return symbols=0, got {result:?}"
        );
    }

    #[test]
    fn extract_optional_text_field_absent_returns_none() {
        let args = Value::Map(vec![("name".to_owned(), Value::Text("foo".to_owned()))]);
        assert_eq!(extract_optional_text_field(&args, "kind"), None);
    }

    #[test]
    fn extract_optional_text_field_present_returns_value() {
        let args = Value::Map(vec![
            ("name".to_owned(), Value::Text("foo".to_owned())),
            ("kind".to_owned(), Value::Text("function".to_owned())),
        ]);
        assert_eq!(
            extract_optional_text_field(&args, "kind"),
            Some("function".to_owned())
        );
    }
}
