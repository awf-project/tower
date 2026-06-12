//! Shared text extraction helper for the AST plugin.
//!
//! Both `outline` and `symbols` need to slice source bytes for a tree-sitter
//! node, validate UTF-8, and cap the result at 256 bytes to prevent DoS from
//! pathological name spans in generated code.  This module holds the single
//! canonical copy.

use tree_sitter::Node;

/// Extract the source text covered by `node`, trimming to a 256-byte cap.
///
/// Used for names and type fragments — never for body text (U1 enforced by
/// only calling this on name/type nodes, not body nodes).
///
/// Returns `None` if the byte range is out of bounds or the bytes are not
/// valid UTF-8 (should not happen for well-formed tree-sitter output).
pub(crate) fn extract_text(node: Node<'_>, source: &[u8]) -> Option<String> {
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
