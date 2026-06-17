//! `ast` — reference Tree-sitter AST plugin (spec 12c/12d).
//!
//! Compiles to `wasm32-wasip1` using the `#[plugin_main]` macro from
//! `plugin_sdk`. Declares five tools: `get_outline`, `find_symbols`,
//! `search_symbols`, `reindex`, and `read_symbol`.
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
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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

// ── Content-hash dedup (REQ-2) ────────────────────────────────────────────────
//
// Maps workspace-relative path → hash of the last indexed byte slice.
//
// Decision: separate thread_local, NOT a field on SymbolIndex.
// Why: DefaultHasher is per-process randomized (std guarantee since Rust 1.36).
//      A persisted hash would never match after a warm restart, making dedup
//      silently dead on every startup. Keeping it in-memory (non-persisted) is
//      the only correct design.
// Trade-off: the dedup is lost on restart; that is acceptable — the cost of one
//            extra parse+persist per path after restart is negligible compared to
//            the savings within a session (e.g. formatter-echo FileChanged loops).
thread_local! {
    static CONTENT_HASHES: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
}

/// Hash a byte slice using the same `DefaultHasher` approach used in
/// `crates/core_engine/src/adapters/lsp/documents.rs`.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
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
    // Test instrumentation: count how many times persist is called.
    // Gated behind cfg(test) — zero cost in production builds.
    #[cfg(test)]
    PERSIST_COUNT.with(|c| c.set(c.get() + 1));

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

/// Re-index `path` with already-read content bytes, applying content-hash dedup.
///
/// # REQ-2: content-hash dedup
///
/// If `content` is `Some(bytes)` and the hash of `bytes` matches the hash stored
/// in `CONTENT_HASHES` for this path, the call is a no-op: no parse, no persist.
/// This suppresses redundant work for formatter-echo FileChanged loops.
///
/// If `content` is `None` (file deleted), the path is removed from the index and
/// the hash entry is cleared — but only if the path is actually present in the
/// index (avoids a spurious persist for a deletion of a never-indexed file).
///
/// # REQ-1: no double-parse
///
/// When content is new/changed, parse once here then hand the `Outline` to
/// `reindex_with_preparsed_outline` which does NOT parse again.
fn reindex_with_content(path: &str, content: Option<&[u8]>) {
    match content {
        Some(bytes) => {
            let new_hash = hash_bytes(bytes);

            // Short-circuit: same content as last index → nothing to do.
            let already_current =
                CONTENT_HASHES.with(|m| m.borrow().get(path).copied() == Some(new_hash));
            if already_current {
                return;
            }

            match outline::parse_outline(bytes, path) {
                outline::OutlineResult::Parsed(o) => {
                    // Store hash before indexing so re-entrant calls (if any) are safe.
                    CONTENT_HASHES.with(|m| {
                        m.borrow_mut().insert(path.to_owned(), new_hash);
                    });
                    reindex_with_preparsed_outline(path, &o);
                }
                outline::OutlineResult::Unsupported { .. } => {
                    // Unsupported language: remove stale entries only if present.
                    let was_indexed = with_index(|idx| {
                        idx.by_path.contains_key(path).then(|| {
                            idx.remove_file(path);
                        })
                    })
                    .is_some();
                    if was_indexed {
                        // Remove the stale hash too.
                        CONTENT_HASHES.with(|m| {
                            m.borrow_mut().remove(path);
                        });
                        with_index(|idx| persist_index(idx));
                    }
                }
            }
        }
        None => {
            // File deleted: remove entries only if the path is present.
            // Decision: guard the persist behind a contains_key check so that
            // deleting a never-indexed path does not trigger a spurious persist.
            let was_present = with_index(|idx| {
                if idx.by_path.contains_key(path) {
                    idx.remove_file(path);
                    true
                } else {
                    false
                }
            });
            // Always clear the hash for the path (even if not in the index) so
            // a subsequent re-create of the file is not suppressed by a stale hash.
            CONTENT_HASHES.with(|m| {
                m.borrow_mut().remove(path);
            });
            if was_present {
                with_index(|idx| persist_index(idx));
            }
        }
    }
}

/// Index `path` using an already-parsed `Outline` — no parse call made here.
///
/// # REQ-1
///
/// Callers that already hold a parsed `Outline` (e.g. `get_outline`,
/// `find_symbols`) must use this function instead of `reindex_with_content` to
/// avoid a second tree-sitter parse of the same bytes.
///
/// The content-hash dedup in `CONTENT_HASHES` is intentionally NOT checked here:
/// the caller has already parsed the file for its own return value, so the index
/// should always be brought up to date with the outline in hand.
fn reindex_with_preparsed_outline(path: &str, outline: &outline::Outline) {
    with_index(|idx| {
        idx.index_file(path, outline);
        persist_index(idx);
    });
}

/// Build a fresh symbol index and content-hash map from a set of `(path, bytes)`.
///
/// Used by the `reindex` tool to rebuild from the canonical file list. Returns
/// both the index and the per-path content hashes so the caller can REPLACE the
/// `CONTENT_HASHES` dedup map atomically. Repopulating it (rather than merely
/// clearing it) means the first FileChanged for an unchanged file after a full
/// reindex is correctly deduped (REQ-2) instead of triggering a redundant
/// parse+persist. Files whose language is unsupported are skipped — they get no
/// index entry and no hash entry, which also prunes stale hashes for paths that
/// are no longer indexable.
///
/// Pure (no host calls) so it is exercised by host unit tests; the wasm-only
/// `reindex` wiring just feeds it the bytes read via the host capability.
#[cfg(any(target_arch = "wasm32", test))]
fn build_fresh_index(files: &[(String, Vec<u8>)]) -> (SymbolIndex, HashMap<String, u64>) {
    let mut index = SymbolIndex::default();
    let mut hashes = HashMap::new();
    for (path, bytes) in files {
        if let outline::OutlineResult::Parsed(o) = outline::parse_outline(bytes, path) {
            index.index_file(path, &o);
            hashes.insert(path.clone(), hash_bytes(bytes));
        }
    }
    (index, hashes)
}

// ── Plugin struct ─────────────────────────────────────────────────────────────

/// The AST plugin: exposes `get_outline`, `find_symbols`, `search_symbols`,
/// `reindex`, and `read_symbol` with a persistent cross-file symbol index for
/// the workspace.
///
/// Apply `#[plugin_main]` to generate the five wasm export symbols automatically.
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
                ToolDesc {
                    name: "read_symbol".to_owned(),
                    description: "Read only a named symbol's source span from a file — not the \
                                  whole file. Returns the exact byte slice `[start_byte, \
                                  end_byte)` plus kind and start/end rows for each match, \
                                  ordered by start_byte. Supports optional kind filter to \
                                  disambiguate same-named symbols of different kinds. One \
                                  MCP round-trip: resolve + slice happen guest-side."
                        .to_owned(),
                    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to the source file"},"symbol_name":{"type":"string","description":"Name of the symbol to read"},"kind":{"type":"string","description":"Optional symbol kind filter: function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"}},"required":["path","symbol_name"]}"#.to_owned(),
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
            "read_symbol" => ast_read_symbol_impl(args),
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

    // Parse once — the result is both returned to the caller and handed to the
    // index, eliminating the double-parse that existed when reindex_with_content
    // was called with raw bytes (REQ-1).
    let result = outline::parse_outline(&content, path);

    let value = match result {
        outline::OutlineResult::Unsupported { language } => Value::Map(vec![
            ("unsupported".to_owned(), Value::Bool(true)),
            ("language".to_owned(), Value::Text(language)),
        ]),
        outline::OutlineResult::Parsed(ref o) => {
            // Index-as-you-go: reuse the outline we already parsed (REQ-1).
            // Also updates CONTENT_HASHES so a subsequent FileChanged for the
            // same bytes is a no-op (REQ-2).
            CONTENT_HASHES.with(|m| {
                m.borrow_mut().insert(path.to_owned(), hash_bytes(&content));
            });
            reindex_with_preparsed_outline(path, o);
            o.to_sdk_value()
        }
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

    // Index-as-you-go: parse the outline once for indexing, then let
    // find_symbols do its own independent parse for symbol matching (REQ-1).
    // Avoids the extra parse that reindex_with_content(path, Some(&content))
    // would trigger internally.
    if let outline::OutlineResult::Parsed(ref o) = outline::parse_outline(&content, path) {
        CONTENT_HASHES.with(|m| {
            m.borrow_mut().insert(path.to_owned(), hash_bytes(&content));
        });
        reindex_with_preparsed_outline(path, o);
    }

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
    let kind_opt = extract_optional_text_field(&args, "kind")?;

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

        // Read each listed file, skipping any that vanished between list and read.
        let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(paths.len());
        for path in &paths {
            if let Some(bytes) = plugin_sdk::host::read_file(path) {
                files.push((path.clone(), bytes));
            }
        }

        // Build a fresh index + content-hash map from the canonical file list so
        // that entries for deleted files are automatically pruned. `indexed_files`
        // is the count of files actually indexed — equal to the hash-map size,
        // since a hash entry is inserted for exactly each Parsed (indexable) file.
        let (fresh_index, fresh_hashes) = build_fresh_index(&files);
        let indexed_files = fresh_hashes.len() as i64;
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

        // Replace (not merely clear) the content-hash dedup map: repopulating it
        // with the freshly indexed files means a subsequent identical FileChanged
        // is correctly deduped (REQ-2), while stale entries for deleted/unindexable
        // files are dropped (they are simply absent from fresh_hashes).
        CONTENT_HASHES.with(|m| *m.borrow_mut() = fresh_hashes);

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

/// Implementation of the `read_symbol` tool.
///
/// Returns only the named symbol's source span — not the whole file. One MCP
/// round-trip: resolve + slice happen guest-side via the existing `host_read_file`
/// capability and `symbols::resolve_symbol_spans`.
///
/// # Arguments (in `args`)
///
/// - `path` (required): workspace-relative path to the source file.
/// - `symbol_name` (required): the symbol name to look up.
/// - `kind` (optional): restrict to a specific kind; if absent all applicable
///   kinds are searched and all matches are returned ordered by `start_byte`.
///
/// # Return value
///
/// ```json
/// {
///   "matches": [
///     {
///       "kind": "function",
///       "name": "handle",
///       "start_byte": 100, "end_byte": 200,
///       "start_row": 5,    "end_row": 12,
///       "content": "pub fn handle() {\n    ...\n}"
///     }
///   ]
/// }
/// ```
///
/// # Errors
///
/// - `SdkError::InvalidArgs` if required fields are missing/wrong type or an
///   unrecognised `kind` string is supplied.
/// - `SdkError::CallFailed` if the host cannot read the file, the symbol is
///   not found (UN1 — names the symbol), or a span lies outside the file
///   content (UN2 — stale span guard).
fn ast_read_symbol_impl(args: Value) -> Result<Value, SdkError> {
    let path = extract_text_field(&args, "path")?;
    let symbol_name = extract_text_field(&args, "symbol_name")?;

    // kind is OPTIONAL. If provided but wrong type → InvalidArgs (strict).
    // If provided but unrecognised string → InvalidArgs.
    let kind_opt = match extract_optional_text_field(&args, "kind")? {
        Some(k_str) => {
            let k = symbols::SymbolKind::parse_kind(&k_str).ok_or_else(|| {
                SdkError::InvalidArgs(format!(
                    "unknown symbol kind '{k_str}'; valid: function|struct|enum|trait|impl|\
                     method|module|type_alias|const|static|macro_def|class"
                ))
            })?;
            Some((k, k_str))
        }
        None => None,
    };

    let content = plugin_sdk::host::read_file(path)
        .ok_or_else(|| SdkError::CallFailed(format!("host could not read file: {path}")))?;

    // Destructure into the SymbolKind (for the resolver) and the original string
    // (for the NotApplicable error message below).
    let (kind_enum_opt, kind_str_opt) = match kind_opt {
        Some((k, s)) => (Some(k), Some(s)),
        None => (None, None),
    };

    let result = symbols::resolve_symbol_spans(&content, path, symbol_name, kind_enum_opt);

    let matches_vec = match result {
        symbols::SymbolResult::Unsupported { language } => {
            // Mirror find_symbols convention: return { unsupported: true, language }.
            return Ok(Value::Map(vec![
                ("unsupported".to_owned(), Value::Bool(true)),
                ("language".to_owned(), Value::Text(language)),
            ]));
        }
        symbols::SymbolResult::NotApplicable => {
            // NotApplicable only occurs when kind=Some(k) and that kind cannot
            // exist in the file's language (e.g. kind:"impl" on a Go file).
            // Returning "symbol not found" here is misleading — the symbol
            // could exist under a different kind. Return a clear error instead.
            let kind_str = kind_str_opt.as_deref().unwrap_or("<unknown>");
            return Err(SdkError::InvalidArgs(format!(
                "kind '{kind_str}' is not applicable to this file's language"
            )));
        }
        symbols::SymbolResult::Found(ms) => ms,
    };

    // UN1: symbol not found → error naming the symbol.
    if matches_vec.is_empty() {
        return Err(SdkError::CallFailed(format!(
            "symbol not found: {symbol_name}"
        )));
    }

    // Slice each match from the file content and build the response.
    let mut items: Vec<Value> = Vec::with_capacity(matches_vec.len());
    for m in &matches_vec {
        // UN2: stale-span guard — bounds-check before slicing.
        let slice = content.get(m.start_byte..m.end_byte).ok_or_else(|| {
            SdkError::CallFailed(format!(
                "symbol span [{}, {}) out of range for file: {path}",
                m.start_byte, m.end_byte
            ))
        })?;

        // U3: byte-exact slice → UTF-8 text (source files must be UTF-8).
        let content_str = std::str::from_utf8(slice).map_err(|e| {
            SdkError::CallFailed(format!(
                "symbol span for '{symbol_name}' contains invalid UTF-8: {e}"
            ))
        })?;

        items.push(Value::Map(vec![
            ("kind".to_owned(), Value::Text(m.kind.as_str().to_owned())),
            ("name".to_owned(), Value::Text(m.name.clone())),
            ("start_byte".to_owned(), Value::Integer(m.start_byte as i64)),
            ("end_byte".to_owned(), Value::Integer(m.end_byte as i64)),
            ("start_row".to_owned(), Value::Integer(m.start_row as i64)),
            ("end_row".to_owned(), Value::Integer(m.end_row as i64)),
            ("content".to_owned(), Value::Text(content_str.to_owned())),
        ]));
    }

    Ok(Value::Map(vec![("matches".to_owned(), Value::List(items))]))
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
/// - Field absent → `Ok(None)`.
/// - Field present and `Value::Text(s)` → `Ok(Some(s))`.
/// - Field present but NOT a text value → `Err(SdkError::InvalidArgs(...))`.
///
/// The strict rejection of a wrong-typed value (e.g. `"kind": 42`) prevents a
/// client bug from being silently treated as "no filter supplied", which would
/// turn a targeted search into a broad all-kinds search.
fn extract_optional_text_field(args: &Value, field: &str) -> Result<Option<String>, SdkError> {
    match args {
        Value::Map(pairs) => match pairs.iter().find(|(k, _)| k == field) {
            None => Ok(None),
            Some((_, Value::Text(s))) => Ok(Some(s.clone())),
            Some(_) => Err(SdkError::InvalidArgs(format!(
                "field '{field}' must be a string"
            ))),
        },
        _ => Ok(None),
    }
}

// ── Test-only instrumentation ─────────────────────────────────────────────────
//
// Thread-local counters gated behind `#[cfg(test)]` so they carry zero cost
// in production (wasm32 or host release) builds.  The `persist_index` function
// increments PERSIST_COUNT; `reindex_with_content` increments it via the same
// call.  Tests reset both counters before each assertion.

#[cfg(test)]
thread_local! {
    pub(crate) static PERSIST_COUNT: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
}

// ── Host-side tests (lib-level: AstPlugin API + call_tool dispatch) ───────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_declares_five_ast_tools() {
        let manifest = AstPlugin::init();
        assert_eq!(manifest.name, "ast");
        assert_eq!(manifest.abi, ABI_VERSION);
        assert_eq!(
            manifest.tools.len(),
            5,
            "manifest must have exactly 5 tools"
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
        assert!(
            tool_names.contains(&"read_symbol"),
            "manifest must have read_symbol, got: {tool_names:?}"
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
        assert_eq!(
            extract_optional_text_field(&args, "kind"),
            Ok(None),
            "absent field must return Ok(None)"
        );
    }

    #[test]
    fn extract_optional_text_field_present_returns_value() {
        let args = Value::Map(vec![
            ("name".to_owned(), Value::Text("foo".to_owned())),
            ("kind".to_owned(), Value::Text("function".to_owned())),
        ]);
        assert_eq!(
            extract_optional_text_field(&args, "kind"),
            Ok(Some("function".to_owned())),
            "present text field must return Ok(Some(s))"
        );
    }

    /// Fix 1 (RED-first): a present field with a non-string value (e.g. integer)
    /// must return `Err(InvalidArgs)`, not silently treat it as absent.
    ///
    /// Before this fix the function returned `Option<String>` and `None` for
    /// wrong-typed values, turning `"kind": 42` into a broad all-kinds search.
    #[test]
    fn extract_optional_text_field_wrong_type_returns_invalid_args() {
        let args = Value::Map(vec![
            ("name".to_owned(), Value::Text("foo".to_owned())),
            // Integer where a string is required.
            ("kind".to_owned(), Value::Integer(42)),
        ]);
        let result = extract_optional_text_field(&args, "kind");
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(ref msg)) if msg.contains("kind")),
            "present non-string field must return Err(InvalidArgs), got {result:?}"
        );
    }

    /// Fix 1: search_symbols with a wrong-typed `kind` field (e.g. integer) must
    /// return InvalidArgs — not silently drop the filter and search all kinds.
    #[test]
    fn call_tool_search_symbols_wrong_type_kind_returns_invalid_args() {
        INDEX.with(|c| *c.borrow_mut() = Some(SymbolIndex::default()));

        let args = Value::Map(vec![
            ("name".to_owned(), Value::Text("foo".to_owned())),
            ("kind".to_owned(), Value::Integer(42)),
        ]);
        let result = AstPlugin::call_tool("search_symbols", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "wrong-typed kind must return InvalidArgs for search_symbols, got {result:?}"
        );
    }

    /// Fix 2 (RED-first): `read_symbol` with a `kind` inapplicable to the file's
    /// language must return a clear `InvalidArgs` error mentioning applicability,
    /// NOT the misleading "symbol not found" message.
    ///
    /// This is a host-side test using `ast_read_symbol_impl` directly; the wasm
    /// path for the same behaviour is exercised by the e2e test
    /// `read_symbol_inapplicable_kind_returns_applicability_error` in
    /// `plugin_ast_e2e.rs`.
    ///
    /// On the host target `host::read_file` stubs to `None`, so this test cannot
    /// reach the `resolve_symbol_spans` call. The full language-mismatch path
    /// (kind=Some(k) → NotApplicable) is exercised end-to-end in the wasm sandbox.
    /// This unit test covers the InvalidArgs path for wrong-typed `kind`.
    #[test]
    fn call_tool_read_symbol_wrong_type_kind_returns_invalid_args() {
        let args = Value::Map(vec![
            ("path".to_owned(), Value::Text("src/main.go".to_owned())),
            ("symbol_name".to_owned(), Value::Text("MyFunc".to_owned())),
            // Integer where a string is required — Fix 1 must catch this.
            ("kind".to_owned(), Value::Integer(99)),
        ]);
        let result = AstPlugin::call_tool("read_symbol", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "wrong-typed kind must return InvalidArgs for read_symbol, got {result:?}"
        );
    }

    // ── RED tests: REQ-1 (no double-parse) ───────────────────────────────────
    //
    // Prove the new API exists: `reindex_with_preparsed_outline(path, &Outline)`.
    // Today this FAILS TO COMPILE because the function does not exist.
    // After REQ-1 is implemented it compiles and runs without re-parsing.

    #[test]
    fn req1_reindex_accepts_preparsed_outline() {
        // RED: reindex_with_preparsed_outline does not exist yet → compile error.
        // GREEN: this function exists and does NOT call parse_outline internally.
        let source = b"pub fn foo() {}";
        let outline = match outline::parse_outline(source, "t.rs") {
            outline::OutlineResult::Parsed(o) => o,
            outline::OutlineResult::Unsupported { language } => {
                panic!("expected Parsed, got Unsupported({language})")
            }
        };
        INDEX.with(|c| *c.borrow_mut() = Some(SymbolIndex::default()));

        // REQ-1 regression guard: reset the parse counter AFTER the setup parse
        // above, then assert that handing an already-parsed outline to the index
        // does NOT trigger a second tree-sitter parse. Without this guard, a
        // regression that re-parses internally would pass silently.
        outline::PARSE_COUNT.with(|c| c.set(0));
        reindex_with_preparsed_outline("t.rs", &outline);
        let parses = outline::PARSE_COUNT.with(|c| c.get());
        assert_eq!(
            parses, 0,
            "REQ-1: reindex_with_preparsed_outline must not call parse_outline; got {parses}"
        );

        // Verify the index was updated (the symbol is now searchable).
        with_index(|idx| {
            let hits = idx.search("foo", Some("function"));
            assert_eq!(
                hits.len(),
                1,
                "REQ-1: indexed symbol must be findable after reindex_with_preparsed_outline"
            );
        });
    }

    // ── REQ-2: full reindex repopulates (not just clears) the hash map ───────

    #[test]
    fn reindex_builds_fresh_hashes_and_prunes_unsupported() {
        // build_fresh_index must populate a hash entry for every indexable file
        // (so a post-reindex identical FileChanged is deduped, REQ-2) and must
        // omit unsupported files (pruning stale hashes for non-indexable paths).
        let files = vec![
            ("src/a.rs".to_owned(), b"pub fn alpha() {}".to_vec()),
            ("notes.md".to_owned(), b"# not indexable".to_vec()),
        ];
        let (index, hashes) = build_fresh_index(&files);

        assert!(
            hashes.contains_key("src/a.rs"),
            "REQ-2: indexable file must have a content-hash entry after reindex"
        );
        assert!(
            !hashes.contains_key("notes.md"),
            "unsupported file must not get a hash entry (stale-hash pruning)"
        );
        assert_eq!(
            index.search("alpha", Some("function")).len(),
            1,
            "fresh index must contain the indexed symbol"
        );
    }

    // ── RED tests: REQ-2 (no persist on unchanged content) ───────────────────
    //
    // Prove that a second identical FileChanged does NOT call persist_index.
    // Today reindex_with_content ALWAYS persists → PERSIST_COUNT == 1 on second
    // call → assertion fails.  After REQ-2 the hash map dedup skips persist.

    #[test]
    fn req2_second_identical_content_does_not_persist() {
        INDEX.with(|c| *c.borrow_mut() = Some(SymbolIndex::default()));
        PERSIST_COUNT.with(|c| c.set(0));

        let source = b"pub fn bar() {}";

        // First call: must index and persist (acceptable).
        reindex_with_content("src/bar.rs", Some(source));
        // (we do not assert the first count — only the second matters)

        // Second call with IDENTICAL bytes: must NOT persist.
        PERSIST_COUNT.with(|c| c.set(0));
        reindex_with_content("src/bar.rs", Some(source));
        let count = PERSIST_COUNT.with(|c| c.get());

        assert_eq!(
            count, 0,
            "REQ-2: identical content must skip persist_index; got {count} persist calls"
        );
        // TODAY: count == 1 → test FAILS (no hash dedup exists yet).
    }

    #[test]
    fn req2_delete_absent_path_does_not_persist() {
        INDEX.with(|c| *c.borrow_mut() = Some(SymbolIndex::default()));
        PERSIST_COUNT.with(|c| c.set(0));

        // None on a path that was never indexed → must not persist.
        reindex_with_content("src/never_seen.rs", None);
        let count = PERSIST_COUNT.with(|c| c.get());

        assert_eq!(
            count, 0,
            "REQ-2: deleting an absent path must not call persist_index; got {count} persist calls"
        );
        // TODAY: count == 1 → test FAILS (None branch always persists).
    }

    #[test]
    fn req2_content_hash_map_is_populated_after_first_index() {
        // RED: CONTENT_HASHES thread_local does not exist yet → compile error.
        // GREEN: after first reindex_with_content the hash map contains an entry
        //        for the path so the second call can short-circuit.
        INDEX.with(|c| *c.borrow_mut() = Some(SymbolIndex::default()));
        CONTENT_HASHES.with(|m| m.borrow_mut().clear());

        let source = b"pub fn baz() {}";
        reindex_with_content("src/baz.rs", Some(source));

        let has_entry = CONTENT_HASHES.with(|m| m.borrow().contains_key("src/baz.rs"));
        assert!(
            has_entry,
            "REQ-2: CONTENT_HASHES must contain an entry for the path after reindex_with_content"
        );
    }

    // ── Host-side dispatch tests for read_symbol ──────────────────────────────
    //
    // On the host target host::read_file is a stub returning None, so all
    // read-path tests assert CallFailed. Pure-logic tests for resolve_symbol_spans
    // live in symbols::tests.

    #[test]
    fn call_tool_read_symbol_missing_path_returns_invalid_args() {
        let args = Value::Map(vec![(
            "symbol_name".to_owned(),
            Value::Text("foo".to_owned()),
        )]);
        let result = AstPlugin::call_tool("read_symbol", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "missing path must return InvalidArgs, got {result:?}"
        );
    }

    #[test]
    fn call_tool_read_symbol_missing_symbol_name_returns_invalid_args() {
        let args = Value::Map(vec![(
            "path".to_owned(),
            Value::Text("src/main.rs".to_owned()),
        )]);
        let result = AstPlugin::call_tool("read_symbol", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "missing symbol_name must return InvalidArgs, got {result:?}"
        );
    }

    #[test]
    fn call_tool_read_symbol_invalid_kind_returns_invalid_args() {
        let args = Value::Map(vec![
            ("path".to_owned(), Value::Text("src/main.rs".to_owned())),
            ("symbol_name".to_owned(), Value::Text("Foo".to_owned())),
            ("kind".to_owned(), Value::Text("not_a_kind".to_owned())),
        ]);
        let result = AstPlugin::call_tool("read_symbol", args);
        assert!(
            matches!(result, Err(SdkError::InvalidArgs(_))),
            "unrecognised kind must return InvalidArgs, got {result:?}"
        );
    }

    #[test]
    fn call_tool_read_symbol_host_read_fail_returns_call_failed() {
        // On the host target, host::read_file is a stub returning None.
        // read_symbol must return CallFailed, not panic.
        let args = Value::Map(vec![
            ("path".to_owned(), Value::Text("src/main.rs".to_owned())),
            ("symbol_name".to_owned(), Value::Text("Foo".to_owned())),
        ]);
        let result = AstPlugin::call_tool("read_symbol", args);
        assert!(
            matches!(result, Err(SdkError::CallFailed(_))),
            "host read failure must return CallFailed, got {result:?}"
        );
    }

    #[test]
    fn call_tool_read_symbol_absent_optional_kind_is_ok_up_to_read() {
        // kind is absent — must not fail with InvalidArgs (kind is optional).
        // Still gets CallFailed from the host stub, but NOT InvalidArgs.
        let args = Value::Map(vec![
            ("path".to_owned(), Value::Text("src/main.rs".to_owned())),
            ("symbol_name".to_owned(), Value::Text("Foo".to_owned())),
        ]);
        let result = AstPlugin::call_tool("read_symbol", args);
        assert!(
            !matches!(result, Err(SdkError::InvalidArgs(_))),
            "absent kind must not produce InvalidArgs: {result:?}"
        );
    }
}
