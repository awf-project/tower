//! Tool dispatch for the `ast` extension.
//!
//! Each function corresponds to one of the five declared tools:
//!   `get_outline`, `find_symbols`, `search_symbols`, `reindex`, `read_symbol`.
//!
//! All functions follow the same signature contract: they receive the JSON
//! params, the in-process shared state, and I/O handles for host capability
//! calls (workspace/readFile, index/put, etc.).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::sync::Mutex;

use crate::index::SymbolIndex;
use crate::outline::{self, OutlineResult};
use crate::protocol;
use crate::symbols::{self, SupportedLanguage, SymbolKind, SymbolResult};

// ── Dispatch entry point ──────────────────────────────────────────────────────

/// Route `tool_name` to the appropriate handler.
///
/// Returns `Ok(serde_json::Value)` on success or `Err(error_message)` on
/// failure.  The caller sends the result back to the host as a `ToolResult`
/// or `Error` response.
pub fn dispatch<W, R>(
    tool_name: &str,
    params: serde_json::Value,
    index: &Mutex<SymbolIndex>,
    content_hashes: &Mutex<HashMap<String, u64>>,
    out: &mut W,
    lines: &mut R,
    next_id: &mut u64,
) -> Result<serde_json::Value, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    match tool_name {
        "get_outline" => tool_get_outline(params, index, content_hashes, out, lines, next_id),
        "find_symbols" => tool_find_symbols(params, index, content_hashes, out, lines, next_id),
        "search_symbols" => tool_search_symbols(params, index),
        "reindex" => tool_reindex(index, content_hashes, out, lines, next_id),
        "read_symbol" => tool_read_symbol(params, out, lines, next_id),
        other => Err(format!("unknown tool: {other}")),
    }
}

// ── Tool: get_outline ─────────────────────────────────────────────────────────

fn tool_get_outline<W, R>(
    params: serde_json::Value,
    index: &Mutex<SymbolIndex>,
    content_hashes: &Mutex<HashMap<String, u64>>,
    out: &mut W,
    lines: &mut R,
    next_id: &mut u64,
) -> Result<serde_json::Value, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path = extract_str(&params, "path")?;
    let content = protocol::host_call_read_file(out, lines, next_id, path)?;

    let result = outline::parse_outline(&content, path);
    match result {
        OutlineResult::Unsupported { language } => Ok(serde_json::json!({
            "unsupported": true,
            "language": language,
        })),
        OutlineResult::Parsed(ref o) => {
            // Index-as-you-go: update the in-memory index with the parsed outline.
            let new_hash = hash_bytes(&content);
            {
                let mut hashes = content_hashes.lock().unwrap();
                hashes.insert(path.to_owned(), new_hash);
            }
            {
                let mut idx = index.lock().unwrap();
                idx.index_file(path, o);
                // Persist after indexing.
                let bytes = idx.to_bytes();
                drop(idx); // release lock before host call
                let _ = protocol::host_call_index_put(out, lines, next_id, "ast/symbols", bytes);
            }
            Ok(o.to_json())
        }
    }
}

// ── Tool: find_symbols ────────────────────────────────────────────────────────

fn tool_find_symbols<W, R>(
    params: serde_json::Value,
    index: &Mutex<SymbolIndex>,
    content_hashes: &Mutex<HashMap<String, u64>>,
    out: &mut W,
    lines: &mut R,
    next_id: &mut u64,
) -> Result<serde_json::Value, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path = extract_str(&params, "path")?;
    let symbol_name = extract_str(&params, "symbol_name")?;
    let kind_str = extract_str(&params, "kind")?;

    let kind = SymbolKind::parse_kind(kind_str).ok_or_else(|| {
        format!(
            "unknown symbol kind '{kind_str}'; valid: \
             function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"
        )
    })?;

    let content = protocol::host_call_read_file(out, lines, next_id, path)?;

    // Index-as-you-go.
    if let OutlineResult::Parsed(ref o) = outline::parse_outline(&content, path) {
        let new_hash = hash_bytes(&content);
        let mut hashes = content_hashes.lock().unwrap();
        hashes.insert(path.to_owned(), new_hash);
        drop(hashes);
        let mut idx = index.lock().unwrap();
        idx.index_file(path, o);
        let bytes = idx.to_bytes();
        drop(idx);
        let _ = protocol::host_call_index_put(out, lines, next_id, "ast/symbols", bytes);
    }

    let result = symbols::find_symbols(&content, path, symbol_name, kind);
    match result {
        SymbolResult::Unsupported { language } => Ok(serde_json::json!({
            "unsupported": true,
            "language": language,
        })),
        SymbolResult::NotApplicable => Ok(serde_json::json!({ "matches": [] })),
        SymbolResult::Found(matches) => {
            let items: Vec<serde_json::Value> = matches
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "kind": m.kind.as_str(),
                        "name": m.name,
                        "start_byte": m.start_byte,
                        "end_byte": m.end_byte,
                        "start_row": m.start_row,
                        "start_col": m.start_col,
                        "end_row": m.end_row,
                        "end_col": m.end_col,
                    })
                })
                .collect();
            Ok(serde_json::json!({ "matches": items }))
        }
    }
}

// ── Tool: search_symbols ──────────────────────────────────────────────────────

fn tool_search_symbols(
    params: serde_json::Value,
    index: &Mutex<SymbolIndex>,
) -> Result<serde_json::Value, String> {
    let name = extract_str(&params, "name")?;
    // kind is optional; wrong type → error.
    let kind_opt = extract_optional_str(&params, "kind")?;

    let idx = index.lock().unwrap();
    let hits: Vec<serde_json::Value> = idx
        .search(name, kind_opt.as_deref())
        .iter()
        .map(|e| e.to_json())
        .collect();
    Ok(serde_json::json!({ "matches": hits }))
}

// ── Tool: reindex ─────────────────────────────────────────────────────────────

fn tool_reindex<W, R>(
    index: &Mutex<SymbolIndex>,
    content_hashes: &Mutex<HashMap<String, u64>>,
    out: &mut W,
    lines: &mut R,
    next_id: &mut u64,
) -> Result<serde_json::Value, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let paths = protocol::host_call_list_files(out, lines, next_id)?;

    let mut fresh_index = SymbolIndex::default();
    let mut fresh_hashes: HashMap<String, u64> = HashMap::new();
    let mut indexed_files = 0i64;

    for path in &paths {
        let bytes = match protocol::host_call_read_file(out, lines, next_id, path) {
            Ok(b) => b,
            Err(_) => continue, // file may have vanished between list and read
        };
        if let OutlineResult::Parsed(ref o) = outline::parse_outline(&bytes, path) {
            fresh_index.index_file(path, o);
            fresh_hashes.insert(path.clone(), hash_bytes(&bytes));
            indexed_files += 1;
        }
    }

    let total_symbols: i64 = fresh_index
        .by_path
        .values()
        .map(|entries| entries.len() as i64)
        .sum();

    // Persist the fresh index.
    let persist_bytes = fresh_index.to_bytes();
    let _ = protocol::host_call_index_put(out, lines, next_id, "ast/symbols", persist_bytes);

    // Replace in-process state atomically.
    *index.lock().unwrap() = fresh_index;
    *content_hashes.lock().unwrap() = fresh_hashes;

    Ok(serde_json::json!({
        "indexed_files": indexed_files,
        "symbols": total_symbols,
    }))
}

// ── Tool: read_symbol ─────────────────────────────────────────────────────────

fn tool_read_symbol<W, R>(
    params: serde_json::Value,
    out: &mut W,
    lines: &mut R,
    next_id: &mut u64,
) -> Result<serde_json::Value, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let path = extract_str(&params, "path")?;
    let symbol_name = extract_str(&params, "symbol_name")?;

    // kind is optional.
    let kind_opt = match extract_optional_str(&params, "kind")? {
        Some(k_str) => {
            let k = SymbolKind::parse_kind(&k_str).ok_or_else(|| {
                format!(
                    "unknown symbol kind '{k_str}'; valid: \
                     function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"
                )
            })?;
            Some(k)
        }
        None => None,
    };

    let content = protocol::host_call_read_file(out, lines, next_id, path)?;

    let result = symbols::resolve_symbol_spans(&content, path, symbol_name, kind_opt);

    let matches_vec = match result {
        SymbolResult::Unsupported { language } => {
            return Ok(serde_json::json!({ "unsupported": true, "language": language }));
        }
        SymbolResult::NotApplicable => {
            return Err("kind is not applicable to this file's language".to_string());
        }
        SymbolResult::Found(ms) => ms,
    };

    if matches_vec.is_empty() {
        return Err(format!("symbol not found: {symbol_name}"));
    }

    let mut items: Vec<serde_json::Value> = Vec::with_capacity(matches_vec.len());
    for m in &matches_vec {
        let slice = content.get(m.start_byte..m.end_byte).ok_or_else(|| {
            format!(
                "symbol span [{}, {}) out of range for file: {path}",
                m.start_byte, m.end_byte
            )
        })?;
        let content_str = std::str::from_utf8(slice)
            .map_err(|e| format!("symbol span for '{symbol_name}' contains invalid UTF-8: {e}"))?;
        items.push(serde_json::json!({
            "kind":       m.kind.as_str(),
            "name":       m.name,
            "start_byte": m.start_byte,
            "end_byte":   m.end_byte,
            "start_row":  m.start_row,
            "end_row":    m.end_row,
            "content":    content_str,
        }));
    }

    Ok(serde_json::json!({ "matches": items }))
}

// ── Event reindex (called from deliverEvent) ──────────────────────────────────

/// Re-index a single `path` after a `fileIndexed` or `fileChanged` event.
///
/// Uses content-hash dedup: if the content is identical to the last indexed
/// version the index is not updated (no redundant persist).
///
/// UN1: if the file cannot be read, the failure is recorded in the index as
/// a removal (file deleted) and the function returns `Ok(())`.
pub fn reindex_single_path<W, R>(
    path: &str,
    index: &Mutex<SymbolIndex>,
    content_hashes: &Mutex<HashMap<String, u64>>,
    out: &mut W,
    lines: &mut R,
    next_id: &mut u64,
) -> Result<(), String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    match protocol::host_call_read_file(out, lines, next_id, path) {
        Ok(bytes) => {
            let new_hash = hash_bytes(&bytes);
            // Content-hash dedup: skip if unchanged.
            let already_current = {
                let hashes = content_hashes.lock().unwrap();
                hashes.get(path).copied() == Some(new_hash)
            };
            if already_current {
                return Ok(());
            }

            match outline::parse_outline(&bytes, path) {
                OutlineResult::Parsed(ref o) => {
                    {
                        let mut hashes = content_hashes.lock().unwrap();
                        hashes.insert(path.to_owned(), new_hash);
                    }
                    let persist_bytes = {
                        let mut idx = index.lock().unwrap();
                        idx.index_file(path, o);
                        idx.to_bytes()
                    };
                    let _ = protocol::host_call_index_put(
                        out,
                        lines,
                        next_id,
                        "ast/symbols",
                        persist_bytes,
                    );
                }
                OutlineResult::Unsupported { .. } => {
                    // Unsupported language: remove stale entry if present.
                    let was_indexed = {
                        let idx = index.lock().unwrap();
                        idx.by_path.contains_key(path)
                    };
                    if was_indexed {
                        let persist_bytes = {
                            let mut idx = index.lock().unwrap();
                            idx.remove_file(path);
                            idx.to_bytes()
                        };
                        content_hashes.lock().unwrap().remove(path);
                        let _ = protocol::host_call_index_put(
                            out,
                            lines,
                            next_id,
                            "ast/symbols",
                            persist_bytes,
                        );
                    }
                }
            }
        }
        Err(_) => {
            // File not found / read error → treat as deletion.
            let was_present = {
                let idx = index.lock().unwrap();
                idx.by_path.contains_key(path)
            };
            content_hashes.lock().unwrap().remove(path);
            if was_present {
                let persist_bytes = {
                    let mut idx = index.lock().unwrap();
                    idx.remove_file(path);
                    idx.to_bytes()
                };
                let _ = protocol::host_call_index_put(
                    out,
                    lines,
                    next_id,
                    "ast/symbols",
                    persist_bytes,
                );
            }
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract a required string field from a JSON object.
fn extract_str<'a>(params: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    params
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing or non-string required field '{field}'"))
}

/// Extract an optional string field; returns `Err` if present but not a string.
fn extract_optional_str(params: &serde_json::Value, field: &str) -> Result<Option<String>, String> {
    match params.get(field) {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("field '{field}' must be a string")),
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

// ── Conversion helpers ────────────────────────────────────────────────────────

/// Check whether the language for `path` is supported.
///
/// Used by `reindex_single_path` to decide whether to try parsing.
#[allow(dead_code)]
pub fn is_supported(path: &str) -> bool {
    SupportedLanguage::from_hint(path).is_some()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_str_missing_field_returns_err() {
        let params = serde_json::json!({});
        assert!(extract_str(&params, "path").is_err());
    }

    #[test]
    fn extract_str_present_returns_value() {
        let params = serde_json::json!({"path": "src/lib.rs"});
        assert_eq!(extract_str(&params, "path").unwrap(), "src/lib.rs");
    }

    #[test]
    fn extract_optional_str_absent_returns_none() {
        let params = serde_json::json!({"name": "foo"});
        assert_eq!(extract_optional_str(&params, "kind").unwrap(), None);
    }

    #[test]
    fn extract_optional_str_present_returns_some() {
        let params = serde_json::json!({"kind": "function"});
        assert_eq!(
            extract_optional_str(&params, "kind").unwrap(),
            Some("function".to_owned())
        );
    }

    #[test]
    fn extract_optional_str_wrong_type_returns_err() {
        let params = serde_json::json!({"kind": 42});
        assert!(extract_optional_str(&params, "kind").is_err());
    }

    #[test]
    fn search_symbols_empty_index_returns_empty_matches() {
        let index = Mutex::new(SymbolIndex::default());
        let params = serde_json::json!({"name": "anything"});
        let result = tool_search_symbols(params, &index).unwrap();
        let matches = result.get("matches").unwrap().as_array().unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn search_symbols_wrong_type_kind_returns_err() {
        let index = Mutex::new(SymbolIndex::default());
        let params = serde_json::json!({"name": "foo", "kind": 42});
        assert!(tool_search_symbols(params, &index).is_err());
    }
}
