//! Pure decoders: LSP wire JSON → domain navigation value objects.
//!
//! Kept separate from the transport so the tricky shapes — `Location` **vs**
//! `LocationLink` (U4/AC7), hierarchical `DocumentSymbol` **vs** flat
//! `SymbolInformation`, and the several `Hover.contents` encodings — are
//! unit-tested without a running language server. Positions stay in the LSP
//! UTF-16 convention (the domain's `Position`); workspace-root filtering and
//! uri→path stripping happen in the adapter after decoding.

#![forbid(unsafe_code)]

use serde_json::Value;

use crate::domain::code_intel::{Hover, Position, Range, Symbol};

/// A decoded location still addressed by its absolute `file://` uri. The adapter
/// strips the workspace root and drops out-of-root results to produce a domain
/// [`Location`](crate::domain::code_intel::Location).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawLocation {
    pub uri: String,
    pub range: Range,
}

fn decode_position(value: &Value) -> Option<Position> {
    Some(Position {
        line: u32::try_from(value.get("line")?.as_u64()?).ok()?,
        character: u32::try_from(value.get("character")?.as_u64()?).ok()?,
    })
}

fn decode_range(value: &Value) -> Option<Range> {
    Some(Range {
        start: decode_position(value.get("start")?)?,
        end: decode_position(value.get("end")?)?,
    })
}

/// Decode a `textDocument/definition` or `references` result, which may be
/// `null`, a single `Location`, an array of `Location`, a single `LocationLink`,
/// or an array of `LocationLink`. Both shapes normalize to [`RawLocation`].
pub fn decode_locations(value: &Value) -> Vec<RawLocation> {
    match value {
        Value::Array(items) => items.iter().filter_map(decode_one_location).collect(),
        Value::Object(_) => decode_one_location(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Decode one entry, accepting either a `Location` (`uri` + `range`) or a
/// `LocationLink` (`targetUri` + `targetSelectionRange`, falling back to
/// `targetRange`).
fn decode_one_location(value: &Value) -> Option<RawLocation> {
    if let Some(uri) = value.get("uri").and_then(Value::as_str) {
        // Location shape.
        return Some(RawLocation {
            uri: uri.to_owned(),
            range: decode_range(value.get("range")?)?,
        });
    }
    // LocationLink shape: prefer the selection range, else the full target range.
    let uri = value.get("targetUri").and_then(Value::as_str)?;
    let range_val = value
        .get("targetSelectionRange")
        .or_else(|| value.get("targetRange"))?;
    Some(RawLocation {
        uri: uri.to_owned(),
        range: decode_range(range_val)?,
    })
}

/// Decode a `textDocument/hover` result (`null`, `{contents, range?}` where
/// `contents` is `MarkupContent`, `MarkedString`, or an array of `MarkedString`).
pub fn decode_hover(value: &Value) -> Option<Hover> {
    let contents = render_hover_contents(value.get("contents")?);
    if contents.is_empty() {
        return None;
    }
    let range = value.get("range").and_then(decode_range);
    Some(Hover { contents, range })
}

/// Flatten any of the `Hover.contents` encodings into plain text:
/// `MarkupContent {kind,value}`, a plain `MarkedString` (string), a
/// `{language,value}` MarkedString, or an array of those.
fn render_hover_contents(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(render_hover_contents)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => value
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

/// Decode a `textDocument/documentSymbol` result: hierarchical `DocumentSymbol[]`
/// (flattened depth-first) or flat `SymbolInformation[]`.
pub fn decode_document_symbols(value: &Value) -> Vec<Symbol> {
    let Value::Array(items) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        push_symbol(item, &mut out);
    }
    out
}

/// Decode one symbol entry into `out`. Handles both the hierarchical
/// `DocumentSymbol` (recurse into `children`) and the flat `SymbolInformation`
/// (range lives under `location.range`).
fn push_symbol(value: &Value, out: &mut Vec<Symbol>) {
    let Some(name) = value.get("name").and_then(Value::as_str) else {
        return;
    };
    let kind = value
        .get("kind")
        .and_then(Value::as_u64)
        .map(symbol_kind_name)
        .unwrap_or_default();

    // DocumentSymbol carries `range`; SymbolInformation nests it in `location`.
    let range = value
        .get("range")
        .or_else(|| value.get("location").and_then(|l| l.get("range")))
        .and_then(decode_range);
    let Some(range) = range else {
        return;
    };
    let selection_range = value
        .get("selectionRange")
        .and_then(decode_range)
        .unwrap_or(range);

    out.push(Symbol {
        name: name.to_owned(),
        kind,
        range,
        selection_range,
    });

    // Hierarchical children, depth-first.
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            push_symbol(child, out);
        }
    }
}

/// Map an LSP numeric `SymbolKind` to a lowercase kind name (best-effort
/// alignment with the AST plugin vocabulary).
pub fn symbol_kind_name(kind: u64) -> String {
    let name = match kind {
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "symbol",
    };
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rng(l0: u32, c0: u32, l1: u32, c1: u32) -> Range {
        Range {
            start: Position {
                line: l0,
                character: c0,
            },
            end: Position {
                line: l1,
                character: c1,
            },
        }
    }

    #[test]
    fn decodes_single_location_object() {
        let v = json!({
            "uri": "file:///w/src/a.rs",
            "range": { "start": {"line":1,"character":2}, "end": {"line":1,"character":5} }
        });
        assert_eq!(
            decode_locations(&v),
            vec![RawLocation {
                uri: "file:///w/src/a.rs".to_owned(),
                range: rng(1, 2, 1, 5),
            }]
        );
    }

    #[test]
    fn decodes_array_of_locations() {
        let v = json!([
            { "uri": "file:///w/a.rs", "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}} },
            { "uri": "file:///w/b.rs", "range": {"start":{"line":2,"character":0},"end":{"line":2,"character":3}} }
        ]);
        let locs = decode_locations(&v);
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[1].uri, "file:///w/b.rs");
    }

    #[test]
    fn null_result_decodes_to_empty() {
        assert!(decode_locations(&Value::Null).is_empty());
    }

    /// AC7: a `LocationLink` (targetUri + targetSelectionRange) normalizes to a
    /// `RawLocation` using the selection range.
    #[test]
    fn decodes_location_link_using_selection_range() {
        let v = json!([{
            "targetUri": "file:///w/src/def.rs",
            "targetRange": { "start": {"line":10,"character":0}, "end": {"line":14,"character":1} },
            "targetSelectionRange": { "start": {"line":10,"character":3}, "end": {"line":10,"character":8} }
        }]);
        assert_eq!(
            decode_locations(&v),
            vec![RawLocation {
                uri: "file:///w/src/def.rs".to_owned(),
                range: rng(10, 3, 10, 8),
            }]
        );
    }

    #[test]
    fn location_link_without_selection_falls_back_to_target_range() {
        let v = json!([{
            "targetUri": "file:///w/src/def.rs",
            "targetRange": { "start": {"line":10,"character":0}, "end": {"line":14,"character":1} }
        }]);
        assert_eq!(decode_locations(&v)[0].range, rng(10, 0, 14, 1));
    }

    #[test]
    fn decodes_hover_markup_content() {
        let v = json!({
            "contents": { "kind": "markdown", "value": "```rust\nfn foo()\n```" },
            "range": { "start": {"line":1,"character":0}, "end": {"line":1,"character":3} }
        });
        let hover = decode_hover(&v).unwrap();
        assert!(hover.contents.contains("fn foo()"));
        assert_eq!(hover.range, Some(rng(1, 0, 1, 3)));
    }

    #[test]
    fn decodes_hover_plain_marked_string() {
        let v = json!({ "contents": "a tooltip" });
        let hover = decode_hover(&v).unwrap();
        assert_eq!(hover.contents, "a tooltip");
        assert_eq!(hover.range, None);
    }

    #[test]
    fn hover_null_is_none() {
        assert_eq!(decode_hover(&Value::Null), None);
    }

    #[test]
    fn decodes_hierarchical_document_symbols_flattened() {
        let v = json!([{
            "name": "Outer",
            "kind": 23,
            "range": { "start": {"line":0,"character":0}, "end": {"line":9,"character":1} },
            "selectionRange": { "start": {"line":0,"character":7}, "end": {"line":0,"character":12} },
            "children": [{
                "name": "method_a",
                "kind": 6,
                "range": { "start": {"line":2,"character":4}, "end": {"line":4,"character":5} },
                "selectionRange": { "start": {"line":2,"character":7}, "end": {"line":2,"character":15} }
            }]
        }]);
        let syms = decode_document_symbols(&v);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Outer", "method_a"]);
        assert_eq!(syms[0].kind, "struct");
        assert_eq!(syms[1].kind, "method");
        assert_eq!(syms[1].selection_range, rng(2, 7, 2, 15));
    }

    #[test]
    fn decodes_flat_symbol_information() {
        let v = json!([{
            "name": "top_fn",
            "kind": 12,
            "location": {
                "uri": "file:///w/a.rs",
                "range": { "start": {"line":3,"character":0}, "end": {"line":5,"character":1} }
            }
        }]);
        let syms = decode_document_symbols(&v);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "top_fn");
        assert_eq!(syms[0].kind, "function");
        assert_eq!(syms[0].range, rng(3, 0, 5, 1));
    }

    #[test]
    fn symbol_kind_name_maps_known_kinds() {
        assert_eq!(symbol_kind_name(12), "function");
        assert_eq!(symbol_kind_name(23), "struct");
        assert_eq!(symbol_kind_name(6), "method");
    }
}
