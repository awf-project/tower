//! Shared text extraction helper for the AST extension.
//!
//! Identical logic to `plugins/ast/src/text.rs` but without the `plugin_sdk`
//! dependency — this module uses only `tree_sitter` and std.

use tree_sitter::Node;

/// Extract the source text covered by `node`, trimming to a 256-byte cap.
///
/// Returns `None` if the byte range is out of bounds or the bytes are not
/// valid UTF-8 (should not happen for well-formed tree-sitter output).
pub(crate) fn extract_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    let bytes = source.get(node.start_byte()..node.end_byte())?;
    let text = std::str::from_utf8(bytes).ok()?;
    // Cap at 256 bytes: names longer than this are pathological / generated code.
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
