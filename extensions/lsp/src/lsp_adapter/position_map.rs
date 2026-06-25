//! `PositionMap` — byte ↔ UTF-16 position conversion at the LSP boundary.
//!
//! LSP positions are `(line, character)` where `character` counts **UTF-16 code
//! units** (the protocol default). Tower addresses content by **UTF-8 byte
//! offsets**. The two coincide for ASCII but diverge for any multi-byte scalar:
//! `é` (U+00E9) is 2 UTF-8 bytes but 1 UTF-16 unit; `😀` (U+1F600) is 4 UTF-8
//! bytes but 2 UTF-16 units (a surrogate pair). This map converts both
//! directions over a borrowed document so navigation requests/results land on
//! the correct column for non-ASCII lines.
//!
//! Lines are split on `\n`; the `character` count excludes the line terminator.

#![forbid(unsafe_code)]

use core_engine::domain::code_intel::Position;

use crate::lsp_adapter::decode::WorkspaceEditDecodeError;

/// Bidirectional byte ↔ (line, UTF-16) converter over a borrowed UTF-8 document.
#[allow(dead_code)]
pub struct PositionMap<'a> {
    text: &'a str,
}

#[allow(dead_code)]
impl<'a> PositionMap<'a> {
    /// Borrow `text` for conversion. No copy; the map is `O(1)` to build and
    /// scans only as far as needed per query.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        Self { text }
    }

    /// Convert an absolute UTF-8 byte offset to an LSP `(line, UTF-16)` position.
    ///
    /// A `byte` past the end of the document clamps to the final position; a
    /// `byte` that lands inside a multi-byte scalar snaps down to the enclosing
    /// char boundary.
    #[must_use]
    pub fn byte_to_position(&self, byte: usize) -> Position {
        // Clamp past-end, then snap down to a char boundary so the slice below
        // never splits a multi-byte scalar.
        let mut byte = byte.min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }

        // Walk to the line containing `byte`, tracking the line's start offset.
        let mut line = 0u32;
        let mut line_start = 0usize;
        for (i, b) in self.text.bytes().enumerate() {
            if i >= byte {
                break;
            }
            if b == b'\n' {
                line += 1;
                line_start = i + 1;
            }
        }

        // UTF-16 column = code units between the line start and `byte`.
        let character = self.text[line_start..byte]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();
        Position { line, character }
    }

    /// Convert an LSP `(line, UTF-16)` position to an absolute UTF-8 byte offset.
    ///
    /// Returns `None` if `line` is beyond the document. A `character` past the
    /// line's end clamps to the line-terminating byte; one that lands inside a
    /// surrogate pair rounds forward to the next char boundary.
    #[must_use]
    pub fn position_to_byte(&self, pos: Position) -> Option<usize> {
        // Locate the start byte of `pos.line`. Line 0 starts at 0; line N starts
        // after the N-th `\n`. A line index past the last newline is out of range.
        let mut line_start = 0usize;
        if pos.line > 0 {
            let mut seen = 0u32;
            let mut found = false;
            for (i, b) in self.text.bytes().enumerate() {
                if b == b'\n' {
                    seen += 1;
                    if seen == pos.line {
                        line_start = i + 1;
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                return None;
            }
        }

        // Advance `pos.character` UTF-16 units from the line start, stopping at
        // the line terminator (clamp) or once the column is reached.
        let mut utf16 = 0u32;
        let mut byte = line_start;
        for c in self.text[line_start..].chars() {
            if c == '\n' || utf16 >= pos.character {
                break;
            }
            utf16 += c.len_utf16() as u32;
            byte += c.len_utf8();
        }
        Some(byte)
    }

    pub fn lsp_range_to_byte_range(
        &self,
        path: &str,
        range: lsp_types::Range,
    ) -> Result<std::ops::Range<usize>, WorkspaceEditDecodeError> {
        let start = self.exact_position_to_byte(range.start).ok_or_else(|| {
            WorkspaceEditDecodeError::InvalidRange {
                path: path.to_owned(),
                message: format!(
                    "invalid LSP range start {}:{}",
                    range.start.line, range.start.character
                ),
            }
        })?;
        let end = self.exact_position_to_byte(range.end).ok_or_else(|| {
            WorkspaceEditDecodeError::InvalidRange {
                path: path.to_owned(),
                message: format!(
                    "invalid LSP range end {}:{}",
                    range.end.line, range.end.character
                ),
            }
        })?;
        if start > end {
            return Err(WorkspaceEditDecodeError::InvalidRange {
                path: path.to_owned(),
                message: "range start is after range end".to_owned(),
            });
        }
        Ok(start..end)
    }

    fn exact_position_to_byte(&self, pos: lsp_types::Position) -> Option<usize> {
        let mut line_start = 0usize;
        if pos.line > 0 {
            let mut seen = 0u32;
            let mut found = false;
            for (i, b) in self.text.bytes().enumerate() {
                if b == b'\n' {
                    seen += 1;
                    if seen == pos.line {
                        line_start = i + 1;
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                return None;
            }
        }

        let mut utf16 = 0u32;
        let mut byte = line_start;
        for c in self.text[line_start..].chars() {
            if c == '\n' {
                break;
            }
            if utf16 == pos.character {
                return Some(byte);
            }
            let next_utf16 = utf16 + c.len_utf16() as u32;
            if pos.character < next_utf16 {
                return None;
            }
            utf16 = next_utf16;
            byte += c.len_utf8();
        }
        (utf16 == pos.character).then_some(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position as LspPosition, Range as LspRange};

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
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

    #[test]
    fn ascii_byte_equals_character() {
        let text = "let x = 1;";
        let map = PositionMap::new(text);
        // `=` is at byte 6 and UTF-16 column 6 (ASCII: byte == char).
        assert_eq!(map.byte_to_position(6), pos(0, 6));
        assert_eq!(map.position_to_byte(pos(0, 6)), Some(6));
    }

    /// AC8: a non-ASCII scalar before the position shifts the byte offset past
    /// the UTF-16 column. `let café = 1;` — `é` is 2 bytes / 1 UTF-16 unit, so
    /// the `=` is at UTF-16 column 9 but byte offset 10.
    #[test]
    fn non_ascii_before_position_maps_column() {
        let text = "let café = 1; // error";
        let map = PositionMap::new(text);
        assert_eq!(map.position_to_byte(pos(0, 9)), Some(10));
        assert_eq!(map.byte_to_position(10), pos(0, 9));
    }

    /// A surrogate-pair scalar (emoji) counts as 2 UTF-16 units / 4 bytes.
    #[test]
    fn surrogate_pair_emoji_roundtrips() {
        let text = "😀 = x";
        let map = PositionMap::new(text);
        // The space after the emoji: byte 4, UTF-16 column 2.
        assert_eq!(map.byte_to_position(4), pos(0, 2));
        assert_eq!(map.position_to_byte(pos(0, 2)), Some(4));
    }

    #[test]
    fn multiline_offsets_resolve_per_line() {
        let text = "ab\ncd";
        let map = PositionMap::new(text);
        // byte 3 is the start of line 1 ('c').
        assert_eq!(map.byte_to_position(3), pos(1, 0));
        assert_eq!(map.position_to_byte(pos(1, 1)), Some(4));
    }

    #[test]
    fn line_beyond_document_is_none() {
        let map = PositionMap::new("ab");
        assert_eq!(map.position_to_byte(pos(5, 0)), None);
    }

    #[test]
    fn byte_past_end_clamps_to_final_position() {
        let map = PositionMap::new("ab");
        assert_eq!(map.byte_to_position(999), pos(0, 2));
    }

    #[test]
    fn character_past_line_end_clamps_before_terminator() {
        let map = PositionMap::new("ab\ncd");
        // Column 99 on line 0 clamps to byte 2 (just before '\n').
        assert_eq!(map.position_to_byte(pos(0, 99)), Some(2));
    }

    #[test]
    fn lsp_range_to_byte_range_converts_utf16_positions_for_ascii_accented_and_surrogate_pairs() {
        let text = "alpha\nlet café = \"😀\";\n";
        let map = PositionMap::new(text);

        assert_eq!(
            map.lsp_range_to_byte_range("src/main.rs", lsp_range(0, 0, 0, 5))
                .unwrap(),
            0..5
        );
        assert_eq!(
            map.lsp_range_to_byte_range("src/main.rs", lsp_range(1, 4, 1, 8))
                .unwrap(),
            10..15
        );
        assert_eq!(
            map.lsp_range_to_byte_range("src/main.rs", lsp_range(1, 12, 1, 14))
                .unwrap(),
            19..23
        );
    }

    #[test]
    fn lsp_range_to_byte_range_invalid_utf16_position_returns_invalid_range_safely() {
        let map = PositionMap::new("let café = 1;");

        let error = map
            .lsp_range_to_byte_range("src/main.rs", lsp_range(7, 0, 7, 1))
            .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceEditDecodeError::InvalidRange { path, .. } if path == "src/main.rs"
        ));
    }
}
