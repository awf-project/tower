//! Tool dispatch for the `ast` extension.
//!
//! Each function corresponds to one of the declared tools:
//!   `get_outline`, `find_symbols`, `search_symbols`, `reindex`, `read_symbol`,
//!   `replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`,
//!   `delete_symbol`.
//!
//! All functions follow the same signature contract: they receive the JSON
//! params, the in-process shared state, and I/O handles for host capability
//! calls (workspace/readFile, index/put, etc.).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::sync::Mutex;

use crate::index::{SymbolEntry, SymbolIndex};
use crate::outline::{self, OutlineResult};
use crate::protocol;
use crate::symbols::{self, SupportedLanguage, SymbolKind, SymbolMatch, SymbolResult};
use extension_protocol::{
    AnchoredSymbolEditError, AnchoredSymbolEditErrorCode, AnchoredSymbolEditRequest,
    AnchoredSymbolEditResult, SymbolCandidate, WorkspaceApplyEditsRequest,
    WorkspaceApplyEditsResult, WorkspaceEditSpan,
};
use sha2::{Digest, Sha256};

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
        "replace_symbol_body" => {
            tool_replace_symbol_body(params, index, content_hashes, out, lines, next_id)
        }
        "insert_before_symbol" => {
            tool_insert_before_symbol(params, index, content_hashes, out, lines, next_id)
        }
        "insert_after_symbol" => {
            tool_insert_after_symbol(params, index, content_hashes, out, lines, next_id)
        }
        "delete_symbol" => tool_delete_symbol(params, index, content_hashes, out, lines, next_id),
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

// ── Tool: anchored symbol edits ───────────────────────────────────────────────

fn tool_replace_symbol_body<W, R>(
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
    let request = anchored_symbol_edit_request(params, true)?;
    apply_anchored_symbol_edit(
        request,
        AnchoredEditKind::ReplaceBody,
        index,
        content_hashes,
        out,
        lines,
        next_id,
    )
}

fn tool_insert_before_symbol<W, R>(
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
    let request = anchored_symbol_edit_request(params, true)?;
    apply_anchored_symbol_edit(
        request,
        AnchoredEditKind::InsertBefore,
        index,
        content_hashes,
        out,
        lines,
        next_id,
    )
}

fn tool_insert_after_symbol<W, R>(
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
    let request = anchored_symbol_edit_request(params, true)?;
    apply_anchored_symbol_edit(
        request,
        AnchoredEditKind::InsertAfter,
        index,
        content_hashes,
        out,
        lines,
        next_id,
    )
}

fn tool_delete_symbol<W, R>(
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
    let request = anchored_symbol_edit_request(params, false)?;
    apply_anchored_symbol_edit(
        request,
        AnchoredEditKind::DeleteDeclaration,
        index,
        content_hashes,
        out,
        lines,
        next_id,
    )
}

#[derive(Clone, Copy)]
enum AnchoredEditKind {
    ReplaceBody,
    InsertBefore,
    InsertAfter,
    DeleteDeclaration,
}

struct IndexedSymbolMatch {
    symbol: SymbolMatch,
    body_span: Option<(usize, usize)>,
}

fn apply_anchored_symbol_edit<W, R>(
    request: AnchoredSymbolEditRequest,
    edit_kind: AnchoredEditKind,
    index: &Mutex<SymbolIndex>,
    _content_hashes: &Mutex<HashMap<String, u64>>,
    out: &mut W,
    lines: &mut R,
    next_id: &mut u64,
) -> Result<serde_json::Value, String>
where
    W: Write,
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let kind = parse_optional_kind(request.kind.as_deref())?;
    let indexed_matches =
        resolve_indexed_symbol_matches(index, &request.path, &request.symbol_name, kind)?;

    let indexed_symbol = match indexed_matches.as_slice() {
        [] => {
            return anchored_edit_error_value(
                AnchoredSymbolEditErrorCode::NotFound,
                format!("symbol not found: {}", request.symbol_name),
                Some(request.path),
                None,
            );
        }
        [symbol] => symbol,
        many => {
            let candidates = many
                .iter()
                .map(|m| symbol_candidate(&request.path, &m.symbol))
                .collect();
            return anchored_edit_error_value(
                AnchoredSymbolEditErrorCode::AmbiguousSymbol,
                format!("symbol '{}' is ambiguous", request.symbol_name),
                Some(request.path),
                Some(candidates),
            );
        }
    };
    if matches!(edit_kind, AnchoredEditKind::ReplaceBody) && indexed_symbol.body_span.is_none() {
        return anchored_edit_error_value(
            AnchoredSymbolEditErrorCode::BackendError,
            format!(
                "symbol '{}' has no replaceable body span",
                indexed_symbol.symbol.name
            ),
            Some(request.path),
            None,
        );
    }

    let content = protocol::host_call_read_file(out, lines, next_id, &request.path)?;
    let current_matches =
        resolve_current_symbol_matches(&content, &request.path, &request.symbol_name, kind)?;
    let current_symbol = match current_matches.as_slice() {
        [] => {
            return anchored_edit_error_value(
                AnchoredSymbolEditErrorCode::NotFound,
                format!("symbol not found: {}", request.symbol_name),
                Some(request.path),
                None,
            );
        }
        [symbol] => symbol,
        many => {
            let candidates = many
                .iter()
                .map(|m| symbol_candidate(&request.path, &m.symbol))
                .collect();
            return anchored_edit_error_value(
                AnchoredSymbolEditErrorCode::AmbiguousSymbol,
                format!("symbol '{}' is ambiguous", request.symbol_name),
                Some(request.path),
                Some(candidates),
            );
        }
    };
    let symbol = &current_symbol.symbol;

    let replacement = request.replacement.unwrap_or_default();
    let (start_byte, end_byte) = match edit_kind {
        AnchoredEditKind::ReplaceBody => match current_symbol.body_span {
            Some(span) => span,
            None => {
                return anchored_edit_error_value(
                    AnchoredSymbolEditErrorCode::BackendError,
                    format!("symbol '{}' has no replaceable body span", symbol.name),
                    Some(request.path),
                    None,
                );
            }
        },
        AnchoredEditKind::InsertBefore => (symbol.start_byte, symbol.start_byte),
        AnchoredEditKind::InsertAfter => (symbol.end_byte, symbol.end_byte),
        AnchoredEditKind::DeleteDeclaration => (symbol.start_byte, symbol.end_byte),
    };

    let (start_byte, end_byte) = match edit_kind {
        AnchoredEditKind::DeleteDeclaration => (
            start_byte,
            declaration_end_with_owned_newline(&content, end_byte),
        ),
        _ => (start_byte, end_byte),
    };

    if start_byte > end_byte || end_byte > content.len() {
        return anchored_edit_error_value(
            AnchoredSymbolEditErrorCode::InvalidRange,
            format!("resolved edit span [{start_byte}, {end_byte}) is outside the file"),
            Some(request.path),
            None,
        );
    }

    let span = WorkspaceEditSpan {
        path: request.path.clone(),
        start_byte,
        end_byte,
        replacement,
        base_hash: Some(sha256_hex(&content)),
    };
    let apply_request = WorkspaceApplyEditsRequest {
        edits: vec![span.clone()],
        dry_run: Some(request.dry_run.unwrap_or(false)),
    };
    let value = protocol::host_call_value(
        out,
        lines,
        next_id,
        "workspace/applyEdits",
        serde_json::to_value(apply_request)
            .map_err(|e| format!("serialize WorkspaceApplyEditsRequest failed: {e}"))?,
    )?;
    let apply_result = serde_json::from_value::<WorkspaceApplyEditsResult>(value)
        .map_err(|e| format!("malformed workspace/applyEdits response: {e}"))?;
    let result = AnchoredSymbolEditResult {
        applied: apply_result.per_file.iter().any(|file| file.applied),
        files_changed: apply_result.files_changed,
        span: Some(span),
        preview: combined_preview(&apply_result),
        per_file: apply_result.per_file,
    };

    serde_json::to_value(result)
        .map_err(|e| format!("serialize AnchoredSymbolEditResult failed: {e}"))
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

fn anchored_symbol_edit_request(
    params: serde_json::Value,
    replacement_required: bool,
) -> Result<AnchoredSymbolEditRequest, String> {
    let request: AnchoredSymbolEditRequest =
        serde_json::from_value(params).map_err(|error| format!("invalid params: {error}"))?;
    if replacement_required && request.replacement.is_none() {
        return Err("missing required field 'replacement'".to_owned());
    }
    Ok(request)
}

fn parse_optional_kind(kind: Option<&str>) -> Result<Option<SymbolKind>, String> {
    kind.map(|kind_str| {
        SymbolKind::parse_kind(kind_str).ok_or_else(|| {
            format!(
                "unknown symbol kind '{kind_str}'; valid: \
                 function|struct|enum|trait|impl|method|module|type_alias|const|static|macro_def|class"
            )
        })
    })
    .transpose()
}

fn anchored_edit_error_value(
    code: AnchoredSymbolEditErrorCode,
    message: impl Into<String>,
    path: Option<String>,
    candidates: Option<Vec<SymbolCandidate>>,
) -> Result<serde_json::Value, String> {
    serde_json::to_value(AnchoredSymbolEditError {
        code,
        message: message.into(),
        path,
        candidates,
    })
    .map_err(|e| format!("serialize AnchoredSymbolEditError failed: {e}"))
}

fn symbol_candidate(path: &str, symbol: &SymbolMatch) -> SymbolCandidate {
    SymbolCandidate {
        path: path.to_owned(),
        kind: symbol.kind.as_str().to_owned(),
        name: symbol.name.clone(),
        start_byte: symbol.start_byte,
        end_byte: symbol.end_byte,
        start_row: symbol.start_row,
        end_row: symbol.end_row,
    }
}

fn resolve_current_symbol_matches(
    content: &[u8],
    path: &str,
    symbol_name: &str,
    kind: Option<SymbolKind>,
) -> Result<Vec<IndexedSymbolMatch>, String> {
    let outline = match outline::parse_outline(content, path) {
        OutlineResult::Parsed(outline) => outline,
        OutlineResult::Unsupported { language } => {
            return Err(format!("unsupported language for {path}: {language}"));
        }
    };
    let kind_label = kind.map(SymbolKind::as_str);
    Ok(outline
        .items
        .iter()
        .filter(|item| item.name == symbol_name)
        .filter(|item| kind_label.is_none_or(|kind| item.kind.as_str() == kind))
        .map(outline_item_to_match)
        .collect())
}

fn resolve_indexed_symbol_matches(
    index: &Mutex<SymbolIndex>,
    path: &str,
    symbol_name: &str,
    kind: Option<SymbolKind>,
) -> Result<Vec<IndexedSymbolMatch>, String> {
    let kind_label = kind.map(SymbolKind::as_str);
    let matches = {
        let idx = index.lock().unwrap();
        idx.search(symbol_name, kind_label)
            .into_iter()
            .filter(|entry| entry.path == path)
            .map(symbol_entry_to_match)
            .collect()
    };
    Ok(matches)
}

fn symbol_entry_to_match(entry: &SymbolEntry) -> IndexedSymbolMatch {
    IndexedSymbolMatch {
        symbol: SymbolMatch {
            kind: SymbolKind::parse_kind(&entry.kind).unwrap_or(SymbolKind::Function),
            name: entry.name.clone(),
            start_byte: entry.start_byte,
            end_byte: entry.end_byte,
            start_row: entry.start_row,
            start_col: entry.start_col,
            end_row: entry.end_row,
            end_col: entry.end_col,
        },
        body_span: entry.body_start_byte.zip(entry.body_end_byte),
    }
}

fn outline_item_to_match(item: &outline::OutlineItem) -> IndexedSymbolMatch {
    IndexedSymbolMatch {
        symbol: SymbolMatch {
            kind: SymbolKind::parse_kind(item.kind.as_str()).unwrap_or(SymbolKind::Function),
            name: item.name.clone(),
            start_byte: item.span.start_byte,
            end_byte: item.span.end_byte,
            start_row: item.span.start_row,
            start_col: item.span.start_col,
            end_row: item.span.end_row,
            end_col: item.span.end_col,
        },
        body_span: item
            .body_span
            .as_ref()
            .map(|span| (span.start_byte, span.end_byte)),
    }
}

fn declaration_end_with_owned_newline(content: &[u8], end_byte: usize) -> usize {
    if content.get(end_byte) == Some(&b'\r') && content.get(end_byte + 1) == Some(&b'\n') {
        end_byte + 2
    } else if content.get(end_byte) == Some(&b'\n') {
        end_byte + 1
    } else {
        end_byte
    }
}

fn combined_preview(result: &WorkspaceApplyEditsResult) -> Option<String> {
    let previews: Vec<&str> = result
        .per_file
        .iter()
        .filter_map(|file| file.preview.as_deref())
        .collect();
    if previews.is_empty() {
        None
    } else {
        Some(previews.join("\n"))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
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
    use extension_protocol::{
        AnchoredSymbolEditError, AnchoredSymbolEditErrorCode, AnchoredSymbolEditResult,
        PerFileEditResult, WorkspaceApplyEditsError, WorkspaceApplyEditsErrorCode,
        WorkspaceApplyEditsResult,
    };

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

    #[test]
    fn anchored_symbol_edit_request_fields_are_exercised_through_public_insert_after_tool_contract()
    {
        let content = b"pub fn rename_me() {}\n";
        let (result, frames) = dispatch_for_test(
            "insert_after_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "rename_me",
                "kind": "function",
                "replacement": "\nfn renamed() {}",
                "dry_run": true
            }),
            vec![
                host_result(10_000, String::from_utf8(content.to_vec()).unwrap()),
                host_result(10_001, one_file_apply_result(true)),
            ],
            Some(content),
        );

        let _: AnchoredSymbolEditResult =
            serde_json::from_value(result.expect("insert-after must succeed"))
                .expect("success result must match contract");
        let apply = frames
            .iter()
            .find(|frame| frame["method"] == "workspace/applyEdits")
            .expect("public insert-after tool must call workspace/applyEdits");
        assert_eq!(apply["params"]["edits"][0]["path"], "src/lib.rs");
        assert_eq!(
            apply["params"]["edits"][0]["replacement"],
            "\nfn renamed() {}"
        );
        assert_eq!(apply["params"]["dry_run"], true);
    }

    #[test]
    fn replace_and_insert_tools_reject_missing_replacement_through_existing_invalid_params_path() {
        for tool in [
            "replace_symbol_body",
            "insert_before_symbol",
            "insert_after_symbol",
        ] {
            let (result, frames) = dispatch_for_test(
                tool,
                serde_json::json!({
                    "path": "src/lib.rs",
                    "symbol_name": "target",
                    "kind": "function"
                }),
                vec![],
                None,
            );

            assert!(
                result
                    .expect_err("missing replacement must be rejected")
                    .contains("replacement")
            );
            assert!(
                frames.is_empty(),
                "invalid params must not emit HostCalls for {tool}: {frames:?}"
            );
        }
    }

    #[test]
    fn replace_symbol_body_resolves_exactly_one_symbol_and_replaces_only_that_symbols_body_span() {
        let content = b"pub fn keep() -> u8 { 1 }\n\npub fn target() -> u8 {\n    1\n}\n\npub fn after() -> u8 { 2 }\n";
        let result = call_edit_tool_success(
            "replace_symbol_body",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n    42\n"
            }),
            content,
            WorkspaceApplyEditsResult {
                files_changed: 1,
                per_file: vec![PerFileEditResult {
                    path: "src/lib.rs".to_owned(),
                    applied: true,
                    edits_applied: 1,
                    edits_skipped: 0,
                    new_version: Some(CHANGED_SHA256.to_owned()),
                    preview: None,
                    error: None,
                }],
            },
        );

        assert!(result.applied);
        assert_eq!(result.files_changed, 1);
        let span = result
            .span
            .expect("result must include the resolved edit span");
        assert_eq!(span.path, "src/lib.rs");
        assert_eq!(span.replacement, "\n    42\n");
        assert_eq!(
            &content[..span.start_byte],
            b"pub fn keep() -> u8 { 1 }\n\npub fn target() -> u8 {"
        );
        assert_eq!(
            &content[span.end_byte..],
            b"}\n\npub fn after() -> u8 { 2 }\n"
        );
    }

    #[test]
    fn insert_before_and_after_symbol_resolve_exactly_one_symbol_without_changing_symbol_bytes() {
        let content = b"pub fn first() {}\n\npub fn target() {}\n\npub fn last() {}\n";

        let before = call_edit_tool_success(
            "insert_before_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "#[allow(dead_code)]\n"
            }),
            content,
            one_file_apply_result(false),
        );
        let before_span = before.span.expect("insert-before span must be returned");
        assert_eq!(before_span.start_byte, before_span.end_byte);
        assert_eq!(
            &content[before_span.start_byte..before_span.start_byte + 18],
            b"pub fn target() {}"
        );

        let after = call_edit_tool_success(
            "insert_after_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n#[cfg(test)]"
            }),
            content,
            one_file_apply_result(false),
        );
        let after_span = after.span.expect("insert-after span must be returned");
        assert_eq!(after_span.start_byte, after_span.end_byte);
        assert_eq!(
            &content[..after_span.start_byte],
            b"pub fn first() {}\n\npub fn target() {}"
        );
    }

    #[test]
    fn delete_symbol_removes_declaration_span_plus_owned_trailing_newline_without_consuming_next_declaration()
     {
        let content = b"pub fn delete_me() {}\npub fn keep_me() {}\n";
        let result = call_edit_tool_success(
            "delete_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "delete_me",
                "kind": "function"
            }),
            content,
            one_file_apply_result(false),
        );

        let span = result.span.expect("delete span must be returned");
        assert_eq!(span.replacement, "");
        assert_eq!(
            &content[span.start_byte..span.end_byte],
            b"pub fn delete_me() {}\n"
        );
        assert_eq!(&content[span.end_byte..], b"pub fn keep_me() {}\n");
    }

    #[test]
    fn no_symbol_matches_returns_not_found_error_serialized_as_not_found_and_performs_no_hostcall_for_each_ast_write_tool()
     {
        let content = b"pub fn present() {}\n";
        for tool in AST_WRITE_TOOLS {
            let (result, frames) =
                dispatch_for_test(tool, edit_params("missing"), vec![], Some(content));

            let value =
                result.expect("semantic edit errors are returned as structured tool results");
            let error: AnchoredSymbolEditError =
                serde_json::from_value(value).expect("not-found error must match contract");
            assert_eq!(error.code, AnchoredSymbolEditErrorCode::NotFound);
            assert_eq!(
                serde_json::to_value(&error.code).expect("serialize code"),
                serde_json::json!("not_found")
            );
            assert_eq!(error.path.as_deref(), Some("src/lib.rs"));
            assert!(
                frames.is_empty(),
                "not-found {tool} edit must not emit HostCalls: {frames:?}"
            );
        }
    }

    #[test]
    fn multiple_symbol_matches_return_ambiguous_symbol_candidates_and_perform_no_hostcall_for_each_ast_write_tool()
     {
        let content = b"pub fn duplicate() {}\nmod inner { pub fn duplicate() {} }\n";
        for tool in AST_WRITE_TOOLS {
            let (result, frames) =
                dispatch_for_test(tool, edit_params("duplicate"), vec![], Some(content));

            let value = result.expect("ambiguous edit must return structured error payload");
            let error: AnchoredSymbolEditError =
                serde_json::from_value(value).expect("ambiguous error must match contract");
            assert_eq!(error.code, AnchoredSymbolEditErrorCode::AmbiguousSymbol);
            assert_eq!(
                serde_json::to_value(&error.code).expect("serialize code"),
                serde_json::json!("ambiguous_symbol")
            );
            assert_eq!(error.path.as_deref(), Some("src/lib.rs"));
            let candidates = error
                .candidates
                .expect("ambiguous error must include candidates");
            assert_eq!(candidates.len(), 2);
            for candidate in candidates {
                assert_eq!(candidate.path, "src/lib.rs");
                assert_eq!(candidate.kind, "function");
                assert_eq!(candidate.name, "duplicate");
                assert!(candidate.start_byte < candidate.end_byte);
                assert!(candidate.start_row <= candidate.end_row);
            }
            assert!(
                frames.is_empty(),
                "ambiguous {tool} edit must not emit HostCalls: {frames:?}"
            );
        }
    }

    #[test]
    fn replace_symbol_body_with_unique_bodyless_symbol_returns_backend_error_and_performs_no_hostcall()
     {
        let content = b"pub trait TargetTrait {\n    fn target(&self);\n}\n";
        let (result, frames) = dispatch_for_test(
            "replace_symbol_body",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "method",
                "replacement": "\n        Ok(())\n"
            }),
            vec![],
            Some(content),
        );

        let value = result.expect("bodyless symbol must return structured error payload");
        let error: AnchoredSymbolEditError =
            serde_json::from_value(value).expect("backend error must match contract");
        assert_eq!(error.code, AnchoredSymbolEditErrorCode::BackendError);
        assert_eq!(
            serde_json::to_value(&error.code).expect("serialize code"),
            serde_json::json!("backend_error")
        );
        assert_eq!(error.path.as_deref(), Some("src/lib.rs"));
        assert!(
            frames.is_empty(),
            "bodyless replace must not emit HostCalls: {frames:?}"
        );
    }

    #[test]
    fn replace_symbol_body_uses_tree_sitter_body_span_and_ignores_braces_inside_strings() {
        let content = br#"pub fn target() -> &'static str {
    let value = "}";
    value
}
"#;
        let result = call_edit_tool_success(
            "replace_symbol_body",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n    \"ok\"\n"
            }),
            content,
            one_file_apply_result(false),
        );

        let span = result.span.expect("replace-body span must be returned");
        assert_eq!(
            &content[..span.start_byte],
            b"pub fn target() -> &'static str {"
        );
        assert_eq!(&content[span.end_byte..], b"}\n");
    }

    #[test]
    fn non_dry_run_calls_workspace_apply_edits_with_byte_spans_and_resolve_time_sha256_base_hash() {
        let content = b"pub fn target() {}\n";
        let expected_hash = TARGET_FN_SHA256;
        let (result, frames) = dispatch_for_test(
            "insert_after_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n"
            }),
            vec![
                host_result(10_000, String::from_utf8(content.to_vec()).unwrap()),
                host_result(10_001, one_file_apply_result(false)),
            ],
            Some(content),
        );

        let _: AnchoredSymbolEditResult =
            serde_json::from_value(result.expect("insert-after must succeed"))
                .expect("success result must match contract");
        let apply = frames
            .iter()
            .find(|frame| frame["method"] == "workspace/applyEdits")
            .expect("edit must call workspace/applyEdits");
        assert_eq!(apply["params"]["edits"][0]["path"], "src/lib.rs");
        assert_eq!(apply["params"]["edits"][0]["base_hash"], expected_hash);
        assert!(apply["params"]["edits"][0]["start_byte"].is_u64());
        assert!(apply["params"]["edits"][0]["end_byte"].is_u64());
    }

    #[test]
    fn anchored_edit_resolves_spans_from_fresh_file_content_not_stale_index_offsets() {
        let stale_index_content = b"pub fn target() {}\n";
        let fresh_content = b"pub fn before() {}\n\npub fn target() {}\n";
        let (result, frames) = dispatch_for_test(
            "insert_after_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n"
            }),
            vec![
                host_result(10_000, String::from_utf8(fresh_content.to_vec()).unwrap()),
                host_result(10_001, one_file_apply_result(false)),
            ],
            Some(stale_index_content),
        );

        let _: AnchoredSymbolEditResult =
            serde_json::from_value(result.expect("insert-after must succeed"))
                .expect("success result must match contract");
        let apply = frames
            .iter()
            .find(|frame| frame["method"] == "workspace/applyEdits")
            .expect("edit must call workspace/applyEdits");
        let start = apply["params"]["edits"][0]["start_byte"]
            .as_u64()
            .expect("start_byte must be numeric") as usize;
        assert_eq!(
            &fresh_content[..start],
            b"pub fn before() {}\n\npub fn target() {}"
        );
        assert_eq!(
            apply["params"]["edits"][0]["base_hash"],
            sha256_hex(fresh_content)
        );
    }

    #[test]
    fn dry_run_calls_workspace_apply_edits_with_dry_run_true_and_returns_preview_without_applying()
    {
        let content = b"pub fn target() {}\n";
        let result = call_edit_tool_success(
            "insert_before_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "// inserted\n",
                "dry_run": true
            }),
            content,
            one_file_apply_result(true),
        );

        assert!(!result.applied);
        assert_eq!(result.files_changed, 0);
        assert_eq!(
            result.preview.as_deref(),
            Some("// inserted\npub fn target() {}\n")
        );
        assert_eq!(
            result.per_file[0].preview.as_deref(),
            Some("// inserted\npub fn target() {}\n")
        );
    }

    #[test]
    fn host_apply_failures_surface_in_per_file_error_with_exact_workspace_apply_edits_error_code() {
        let content = b"pub fn target() {}\n";
        let result = call_edit_tool_success(
            "insert_after_symbol",
            serde_json::json!({
                "path": "src/lib.rs",
                "symbol_name": "target",
                "kind": "function",
                "replacement": "\n"
            }),
            content,
            WorkspaceApplyEditsResult {
                files_changed: 0,
                per_file: vec![PerFileEditResult {
                    path: "src/lib.rs".to_owned(),
                    applied: false,
                    edits_applied: 0,
                    edits_skipped: 1,
                    new_version: None,
                    preview: None,
                    error: Some(WorkspaceApplyEditsError {
                        code: WorkspaceApplyEditsErrorCode::Conflict,
                        message: "CAS conflict".to_owned(),
                        path: Some("src/lib.rs".to_owned()),
                    }),
                }],
            },
        );

        let error = result.per_file[0]
            .error
            .as_ref()
            .expect("file error expected");
        assert_eq!(error.code, WorkspaceApplyEditsErrorCode::Conflict);
        assert_eq!(
            serde_json::to_value(&error.code).expect("serialize error code"),
            serde_json::json!("cas_conflict")
        );
    }

    fn call_edit_tool_success(
        tool: &str,
        params: serde_json::Value,
        content: &[u8],
        apply_result: WorkspaceApplyEditsResult,
    ) -> AnchoredSymbolEditResult {
        let (result, _) = dispatch_for_test(
            tool,
            params,
            vec![
                host_result(10_000, String::from_utf8(content.to_vec()).unwrap()),
                host_result(10_001, apply_result),
            ],
            Some(content),
        );
        serde_json::from_value(result.expect("edit tool must succeed"))
            .expect("edit result must match AnchoredSymbolEditResult")
    }

    fn dispatch_for_test(
        tool: &str,
        params: serde_json::Value,
        responses: Vec<String>,
        indexed_content: Option<&[u8]>,
    ) -> (Result<serde_json::Value, String>, Vec<serde_json::Value>) {
        let index = Mutex::new(SymbolIndex::default());
        if let Some(content) = indexed_content
            && let outline::OutlineResult::Parsed(ref outline) =
                outline::parse_outline(content, "src/lib.rs")
        {
            index.lock().unwrap().index_file("src/lib.rs", outline);
        }
        let content_hashes = Mutex::new(HashMap::new());
        let mut out = Vec::new();
        let mut next_id = 10_000;
        let mut lines = responses.into_iter().map(Ok);
        let result = dispatch(
            tool,
            params,
            &index,
            &content_hashes,
            &mut out,
            &mut lines,
            &mut next_id,
        );
        let frames = String::from_utf8(out)
            .expect("host call frames must be UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("host call frame must be JSON"))
            .collect();
        (result, frames)
    }

    fn host_result(id: u64, result: impl serde::Serialize) -> String {
        serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
        .expect("serialize host response")
    }

    fn one_file_apply_result(dry_run: bool) -> WorkspaceApplyEditsResult {
        WorkspaceApplyEditsResult {
            files_changed: usize::from(!dry_run),
            per_file: vec![PerFileEditResult {
                path: "src/lib.rs".to_owned(),
                applied: !dry_run,
                edits_applied: 1,
                edits_skipped: 0,
                new_version: (!dry_run).then(|| CHANGED_SHA256.to_owned()),
                preview: dry_run.then(|| "// inserted\npub fn target() {}\n".to_owned()),
                error: None,
            }],
        }
    }

    fn edit_params(symbol_name: &str) -> serde_json::Value {
        serde_json::json!({
            "path": "src/lib.rs",
            "symbol_name": symbol_name,
            "kind": "function",
            "replacement": "// replacement\n"
        })
    }

    const AST_WRITE_TOOLS: &[&str] = &[
        "replace_symbol_body",
        "insert_before_symbol",
        "insert_after_symbol",
        "delete_symbol",
    ];

    const TARGET_FN_SHA256: &str =
        "28c70ad37c9d1fb11b1d2d221fc016c9788028e35bb5a3cbacd740f24829f8e6";
    const CHANGED_SHA256: &str = "d67e2e944994496c8d8ec76eed0cf9f09679448d584b532bebf941852a37f5ed";
}
