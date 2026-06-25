//! Pure decoders: LSP wire JSON → domain navigation value objects.
//!
//! Kept separate from the transport so the tricky shapes — `Location` **vs**
//! `LocationLink` (U4/AC7), hierarchical `DocumentSymbol` **vs** flat
//! `SymbolInformation`, and the several `Hover.contents` encodings — are
//! unit-tested without a running language server. Positions stay in the LSP
//! UTF-16 convention (the domain's `Position`). WorkspaceEdit decoding is
//! workspace-root aware because apply-edits host calls require relative paths.

#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use core_engine::domain::code_intel::{Hover, Position, Range, Symbol};
use core_engine::domain::mutation::compute_content_version;
use extension_protocol::{RenameErrorCode, WorkspaceApplyEditsErrorCode, WorkspaceEditSpan};
use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, TextEdit, Uri};

use crate::lsp_adapter::RawWorkspaceEdit;
use crate::lsp_adapter::position_map::PositionMap;

/// A decoded location still addressed by its absolute `file://` uri. The adapter
/// strips the workspace root and drops out-of-root results to produce a domain
/// [`Location`](core_engine::domain::code_intel::Location).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawLocation {
    pub uri: String,
    pub range: Range,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceEditDecodeError {
    MissingText { path: String },
    UnreadableFile { path: String, message: String },
    UnsupportedWorkspaceEdit { message: String },
    InvalidRange { path: String, message: String },
    InvalidPath { uri: String },
}

#[allow(dead_code)]
impl WorkspaceEditDecodeError {
    #[must_use]
    pub fn rename_error_code(&self) -> RenameErrorCode {
        match self {
            Self::UnsupportedWorkspaceEdit { .. } => RenameErrorCode::UnsupportedWorkspaceEdit,
            Self::InvalidRange { .. } => RenameErrorCode::InvalidRange,
            Self::MissingText { .. } | Self::UnreadableFile { .. } | Self::InvalidPath { .. } => {
                RenameErrorCode::BackendError
            }
        }
    }

    #[must_use]
    pub fn workspace_apply_edits_error_code(&self) -> WorkspaceApplyEditsErrorCode {
        match self {
            Self::InvalidPath { .. } => WorkspaceApplyEditsErrorCode::InvalidPath,
            Self::InvalidRange { .. } => WorkspaceApplyEditsErrorCode::InvalidRange,
            Self::UnsupportedWorkspaceEdit { .. } => WorkspaceApplyEditsErrorCode::Unsupported,
            Self::MissingText { .. } | Self::UnreadableFile { .. } => {
                WorkspaceApplyEditsErrorCode::Internal
            }
        }
    }
}

#[allow(dead_code)]
pub fn decode_workspace_edit<F>(
    raw: RawWorkspaceEdit,
    workspace_root: &Path,
    mut resolver: F,
) -> Result<Vec<WorkspaceEditSpan>, WorkspaceEditDecodeError>
where
    F: FnMut(&str) -> Result<String, WorkspaceEditDecodeError>,
{
    let edits = workspace_text_edits(raw, workspace_root)?;
    let mut spans = Vec::with_capacity(edits.len());
    for (path, edit) in edits {
        let text = resolver(&path)?;
        let range = PositionMap::new(&text).lsp_range_to_byte_range(&path, edit.range)?;
        spans.push(WorkspaceEditSpan {
            path,
            start_byte: range.start,
            end_byte: range.end,
            replacement: edit.new_text,
            base_hash: Some(compute_content_version(text.as_bytes())),
        });
    }
    Ok(spans)
}

fn workspace_text_edits(
    raw: RawWorkspaceEdit,
    workspace_root: &Path,
) -> Result<Vec<(String, TextEdit)>, WorkspaceEditDecodeError> {
    if raw.document_changes.is_some() && raw.changes.is_some() {
        return Err(WorkspaceEditDecodeError::UnsupportedWorkspaceEdit {
            message: "WorkspaceEdit with both changes and documentChanges is not supported"
                .to_owned(),
        });
    }

    if let Some(document_changes) = raw.document_changes {
        return document_changes_text_edits(document_changes, workspace_root);
    }

    let Some(changes) = raw.changes else {
        return Ok(Vec::new());
    };

    let mut edits = Vec::new();
    for (uri, uri_edits) in changes {
        let path = workspace_path_from_uri(&uri, workspace_root)?;
        edits.extend(uri_edits.into_iter().map(|edit| (path.clone(), edit)));
    }
    Ok(edits)
}

fn document_changes_text_edits(
    document_changes: DocumentChanges,
    workspace_root: &Path,
) -> Result<Vec<(String, TextEdit)>, WorkspaceEditDecodeError> {
    match document_changes {
        DocumentChanges::Edits(edits) => {
            let mut out = Vec::new();
            for edit in edits {
                let path = workspace_path_from_uri(&edit.text_document.uri, workspace_root)?;
                for text_edit in edit.edits {
                    out.push((path.clone(), one_of_text_edit(text_edit)));
                }
            }
            Ok(out)
        }
        DocumentChanges::Operations(operations) => {
            let mut out = Vec::new();
            for operation in operations {
                match operation {
                    DocumentChangeOperation::Edit(edit) => {
                        let path =
                            workspace_path_from_uri(&edit.text_document.uri, workspace_root)?;
                        for text_edit in edit.edits {
                            out.push((path.clone(), one_of_text_edit(text_edit)));
                        }
                    }
                    DocumentChangeOperation::Op(_) => {
                        return Err(WorkspaceEditDecodeError::UnsupportedWorkspaceEdit {
                            message: "WorkspaceEdit resource operations are not supported"
                                .to_owned(),
                        });
                    }
                }
            }
            Ok(out)
        }
    }
}

fn one_of_text_edit(edit: OneOf<TextEdit, lsp_types::AnnotatedTextEdit>) -> TextEdit {
    match edit {
        OneOf::Left(edit) => edit,
        OneOf::Right(edit) => edit.text_edit,
    }
}

fn workspace_path_from_uri(
    uri: &Uri,
    workspace_root: &Path,
) -> Result<String, WorkspaceEditDecodeError> {
    let raw = uri.to_string();
    let Some(encoded_path) = raw.strip_prefix("file://") else {
        return Err(WorkspaceEditDecodeError::InvalidPath { uri: raw });
    };
    let decoded = decode_uri_path(encoded_path)
        .ok_or_else(|| WorkspaceEditDecodeError::InvalidPath { uri: raw.clone() })?;
    let abs_path = normalize_path(Path::new(&decoded));
    let root = absolute_normalized_root(workspace_root)
        .ok_or_else(|| WorkspaceEditDecodeError::InvalidPath { uri: raw.clone() })?;
    let rel = abs_path
        .strip_prefix(&root)
        .ok()
        .and_then(path_to_workspace_string)
        .ok_or_else(|| WorkspaceEditDecodeError::InvalidPath { uri: raw.clone() })?;
    Ok(rel)
}

fn absolute_normalized_root(root: &Path) -> Option<PathBuf> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(root)
    };
    Some(normalize_path(&absolute))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn path_to_workspace_string(path: &Path) -> Option<String> {
    let text = path.to_str()?.replace('\\', "/");
    (!text.is_empty() && text != ".").then_some(text)
}

fn decode_uri_path(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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
#[allow(clippy::mutable_key_type)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::str::FromStr;

    use lsp_types::{
        CreateFile, DeleteFile, DocumentChangeOperation, DocumentChanges, OneOf,
        OptionalVersionedTextDocumentIdentifier, Position as LspPosition, Range as LspRange,
        RenameFile, ResourceOp, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
    };
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

    fn uri(value: &str) -> Uri {
        Uri::from_str(value).unwrap()
    }

    fn lsp_range(l0: u32, c0: u32, l1: u32, c1: u32) -> LspRange {
        LspRange {
            start: LspPosition {
                line: l0,
                character: c0,
            },
            end: LspPosition {
                line: l1,
                character: c1,
            },
        }
    }

    fn text_edit(l0: u32, c0: u32, l1: u32, c1: u32, replacement: &str) -> TextEdit {
        TextEdit::new(lsp_range(l0, c0, l1, c1), replacement.to_owned())
    }

    fn resolver(path: &str) -> Result<String, WorkspaceEditDecodeError> {
        match path {
            "src/main.rs" => Ok("let old_name = 1;\nlet other = old_name;\n".to_owned()),
            "src/lib.rs" => Ok("pub fn old_name() {}\n".to_owned()),
            "src/unicode.rs" => Ok("let café = \"😀\";\n".to_owned()),
            _ => Err(WorkspaceEditDecodeError::MissingText {
                path: path.to_owned(),
            }),
        }
    }

    fn workspace_root() -> std::path::PathBuf {
        std::path::PathBuf::from("/workspace")
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

    #[test]
    fn decode_workspace_edit_accepts_raw_workspace_edit_plus_resolver_and_returns_spans() {
        let mut changes = HashMap::new();
        changes.insert(
            uri("file:///workspace/src/main.rs"),
            vec![text_edit(0, 4, 0, 12, "new_name")],
        );

        let spans = decode_workspace_edit(WorkspaceEdit::new(changes), &workspace_root(), resolver)
            .unwrap();

        assert_eq!(
            spans,
            vec![WorkspaceEditSpan {
                path: "src/main.rs".to_owned(),
                start_byte: 4,
                end_byte: 12,
                replacement: "new_name".to_owned(),
                base_hash: Some(compute_content_version(
                    b"let old_name = 1;\nlet other = old_name;\n",
                )),
            }]
        );
    }

    #[test]
    fn decode_workspace_edit_supports_ordinary_changes_text_edits_and_preserves_replacement_text() {
        let mut changes = HashMap::new();
        changes.insert(
            uri("file:///workspace/src/main.rs"),
            vec![
                text_edit(0, 4, 0, 12, "renamed"),
                text_edit(1, 12, 1, 20, "renamed"),
            ],
        );
        changes.insert(
            uri("file:///workspace/src/lib.rs"),
            vec![text_edit(0, 7, 0, 15, "renamed")],
        );

        let spans = decode_workspace_edit(WorkspaceEdit::new(changes), &workspace_root(), resolver)
            .unwrap();

        assert_eq!(spans.len(), 3);
        assert!(spans.iter().all(|span| span.replacement == "renamed"));
        assert!(spans.iter().any(|span| span.path == "src/main.rs"));
        assert!(spans.iter().any(|span| span.path == "src/lib.rs"));
    }

    #[test]
    fn decode_workspace_edit_rejects_resource_document_changes_with_unsupported_workspace_edit_error_code()
     {
        let unsupported_edits = [
            DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                uri: uri("file:///workspace/src/new.rs"),
                options: None,
                annotation_id: None,
            })),
            DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                old_uri: uri("file:///workspace/src/old.rs"),
                new_uri: uri("file:///workspace/src/new.rs"),
                options: None,
                annotation_id: None,
            })),
            DocumentChangeOperation::Op(ResourceOp::Delete(DeleteFile {
                uri: uri("file:///workspace/src/old.rs"),
                options: None,
            })),
        ];

        for document_change in unsupported_edits {
            let raw = WorkspaceEdit {
                changes: None,
                document_changes: Some(DocumentChanges::Operations(vec![document_change])),
                change_annotations: None,
            };

            let error = decode_workspace_edit(raw, &workspace_root(), resolver).unwrap_err();

            assert!(matches!(
                error,
                WorkspaceEditDecodeError::UnsupportedWorkspaceEdit { .. }
            ));
            assert_eq!(
                error.rename_error_code(),
                RenameErrorCode::UnsupportedWorkspaceEdit
            );
            assert_eq!(
                serde_json::to_value(error.rename_error_code()).unwrap(),
                "unsupported_workspace_edit"
            );
        }
    }

    #[test]
    fn decode_workspace_edit_accepts_versioned_text_document_edit_document_changes() {
        let raw_edits = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri("file:///workspace/src/main.rs"),
                    version: Some(7),
                },
                edits: vec![OneOf::Left(text_edit(0, 4, 0, 12, "new_name"))],
            }])),
            change_annotations: None,
        };

        let spans = decode_workspace_edit(raw_edits, &workspace_root(), resolver).unwrap();

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].path, "src/main.rs");
        assert_eq!(spans[0].replacement, "new_name");

        let raw_operations = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: uri("file:///workspace/src/lib.rs"),
                        version: Some(9),
                    },
                    edits: vec![OneOf::Left(text_edit(0, 4, 0, 12, "new_name"))],
                }),
            ])),
            change_annotations: None,
        };

        let spans = decode_workspace_edit(raw_operations, &workspace_root(), resolver).unwrap();

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].path, "src/lib.rs");
        assert_eq!(spans[0].replacement, "new_name");
    }

    #[test]
    fn decode_workspace_edit_validates_full_workspace_edit_before_returning_any_spans() {
        let valid = TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri("file:///workspace/src/main.rs"),
                version: None,
            },
            edits: vec![OneOf::Left(text_edit(0, 4, 0, 12, "new_name"))],
        };
        let raw = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Edit(valid),
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri("file:///workspace/src/new.rs"),
                    options: None,
                    annotation_id: None,
                })),
            ])),
            change_annotations: None,
        };

        let error = decode_workspace_edit(raw, &workspace_root(), resolver).unwrap_err();

        assert!(matches!(
            error,
            WorkspaceEditDecodeError::UnsupportedWorkspaceEdit { .. }
        ));
    }

    #[test]
    fn invalid_utf16_positions_return_invalid_range_and_serialize_invalid_range() {
        let mut changes = HashMap::new();
        changes.insert(
            uri("file:///workspace/src/main.rs"),
            vec![text_edit(99, 0, 99, 1, "new_name")],
        );

        let error = decode_workspace_edit(WorkspaceEdit::new(changes), &workspace_root(), resolver)
            .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceEditDecodeError::InvalidRange { .. }
        ));
        assert_eq!(error.rename_error_code(), RenameErrorCode::InvalidRange);
        assert_eq!(
            serde_json::to_value(error.rename_error_code()).unwrap(),
            "invalid_range"
        );
    }

    #[test]
    fn invalid_uri_path_conversion_returns_invalid_path_and_hostcall_invalid_path_code() {
        let mut changes = HashMap::new();
        changes.insert(
            uri("untitled:rename-buffer"),
            vec![text_edit(0, 0, 0, 1, "new_name")],
        );

        let error = decode_workspace_edit(WorkspaceEdit::new(changes), &workspace_root(), resolver)
            .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceEditDecodeError::InvalidPath { .. }
        ));
        assert_eq!(
            error.workspace_apply_edits_error_code(),
            WorkspaceApplyEditsErrorCode::InvalidPath
        );
        assert_eq!(
            serde_json::to_value(error.workspace_apply_edits_error_code()).unwrap(),
            "invalid_path"
        );
    }

    #[test]
    fn decoder_output_uses_workspace_relative_paths_not_raw_file_uris() {
        let mut changes = HashMap::new();
        changes.insert(
            uri("file:///workspace/src/main.rs"),
            vec![text_edit(0, 4, 0, 12, "new_name")],
        );

        let spans = decode_workspace_edit(WorkspaceEdit::new(changes), &workspace_root(), resolver)
            .unwrap();

        assert_eq!(spans[0].path, "src/main.rs");
        assert!(!spans[0].path.starts_with("file://"));
        assert!(!spans[0].path.starts_with("home/"));
    }
}
