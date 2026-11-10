//! Document text maintenance (`didChange` application) and the byte-offset ↔
//! LSP-position conversions. Positions are 0-based line + UTF-16 code-unit
//! column, per the LSP spec's default position encoding. All conversions clamp
//! malformed input instead of panicking — a confused client must not kill the
//! server.

use crate::types::{Position, Range, TextDocumentContentChangeEvent};

/// Apply a whole `didChange` notification to the held text: every content
/// change in order, unless the incoming version is OLDER than what we hold
/// (an out-of-order notification from a confused client), in which case
/// `None` — the held text must not be overwritten by stale edits.
pub fn apply_did_change(
    mut text: String,
    held_version: Option<i32>,
    incoming_version: i32,
    changes: &[TextDocumentContentChangeEvent],
) -> Option<String> {
    if held_version.is_some_and(|held| incoming_version < held) {
        return None;
    }
    for change in changes {
        apply_content_change(&mut text, change);
    }
    Some(text)
}

/// Apply one LSP content change to `text`: a ranged (incremental, UTF-16)
/// edit when `range` is present, a full replacement otherwise. Out-of-bounds
/// ranges clamp rather than panic — a confused client must not kill the
/// server.
pub fn apply_content_change(text: &mut String, change: &TextDocumentContentChangeEvent) {
    match &change.range {
        Some(range) => {
            let start = position_to_offset(text, range.start);
            let end = position_to_offset(text, range.end).max(start);
            text.replace_range(start.min(text.len())..end.min(text.len()), &change.text);
        }
        None => *text = change.text.clone(),
    }
}

/// Convert an LSP (line, character) position to a byte offset in `text`.
/// Lines are 0-based; characters are UTF-16 code units (LSP default).
pub fn position_to_offset(text: &str, pos: Position) -> usize {
    let mut offset = 0usize;
    for _ in 0..pos.line {
        match text[offset..].find('\n') {
            Some(nl) => offset += nl + 1,
            None => return text.len().min(offset),
        }
    }
    // Now on the target line: advance by UTF-16 code units.
    let line_start = offset;
    let mut utf16_remaining = pos.character as usize;
    for (byte_idx, ch) in text[line_start..].char_indices() {
        if ch == '\n' || utf16_remaining == 0 {
            return line_start + byte_idx;
        }
        utf16_remaining = utf16_remaining.saturating_sub(ch.len_utf16());
    }
    text.len()
}

/// The inverse of [`position_to_offset`]: convert a byte `offset` in `text` to
/// an LSP (line, character) position, with `character` in UTF-16 code units
/// (LSP default). `offset` is clamped to `text.len()`.
pub fn offset_to_position(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, ch) in text.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let character = text[line_start..offset].encode_utf16().count() as u32;
    Position { line, character }
}

/// Convert a byte-offset span into an LSP `Range` (0-based line + UTF-16
/// code-unit column, per the LSP spec's default position encoding). Offsets
/// are clamped to the document and snapped forward off any mid-character
/// byte, so malformed spans can never panic.
pub(super) fn byte_span_to_range(text: &str, start: usize, end: usize) -> Range {
    Range {
        start: byte_offset_to_position(text, start),
        end: byte_offset_to_position(text, end.max(start)),
    }
}

pub(super) fn byte_offset_to_position(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let mut line: u32 = 0;
    let mut character: u32 = 0;
    for (i, ch) in text.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }
    Position { line, character }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::server::{semantic_token_legend, tokenize_semantic_for};

    #[test]
    fn crlf_documents_edit_correctly() {
        // CRLF line endings: positions are line/UTF-16-column; the \r must
        // not shift columns or break line indexing.
        let mut text = "SELECT 1\r\nFROM users\r\nWHERE id = 1".to_string();
        // Replace `users` on line 1 (cols 5..10).
        apply_content_change(
            &mut text,
            &TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 1,
                        character: 5,
                    },
                    end: Position {
                        line: 1,
                        character: 10,
                    },
                }),
                text: "orders".to_string(),
            },
        );
        assert_eq!(text, "SELECT 1\r\nFROM orders\r\nWHERE id = 1");
    }

    #[test]
    fn astral_characters_count_as_two_utf16_units() {
        // 😀 (U+1F600) is a surrogate pair: 2 UTF-16 units, 4 UTF-8 bytes.
        // Editing after it must honor UTF-16 columns, not chars or bytes.
        let mut text = "-- 😀 note\nSELECT x".to_string();
        // Line 0: `-- ` = 3 units, 😀 = 2 units, ` note` starts at unit 5.
        // Replace `note` (units 6..10) with `emoji`.
        apply_content_change(
            &mut text,
            &TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 6,
                    },
                    end: Position {
                        line: 0,
                        character: 10,
                    },
                }),
                text: "emoji".to_string(),
            },
        );
        assert_eq!(text, "-- 😀 emoji\nSELECT x");
        // Semantic tokens over astral content keep UTF-16 columns too.
        let legend = semantic_token_legend();
        let data = tokenize_semantic_for(
            "SELECT '😀' FROM t",
            &legend,
            sqlly_lang::sqlmodel::Dialect::SqlServer,
        );
        let string_type = legend.iter().position(|t| t == "string").unwrap() as u32;
        let string_entry = data
            .chunks(5)
            .find(|c| c[3] == string_type)
            .expect("string token present");
        // '😀' = quote + 2 units + quote = 4 UTF-16 units long.
        assert_eq!(string_entry[2], 4, "{data:?}");
    }

    #[test]
    fn multiple_content_changes_apply_in_order() {
        // One didChange carrying two sequential ranged edits (the LSP allows
        // batches; each range addresses the text AFTER the previous change).
        let text = "SELECT a FROM t".to_string();
        let changes = vec![
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 7,
                    },
                    end: Position {
                        line: 0,
                        character: 8,
                    },
                }),
                text: "col".to_string(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 16,
                    },
                    end: Position {
                        line: 0,
                        character: 17,
                    },
                }),
                text: "users".to_string(),
            },
        ];
        let out = apply_did_change(text, Some(1), 2, &changes).expect("newer version applies");
        assert_eq!(out, "SELECT col FROM users");
    }

    #[test]
    fn stale_didchange_versions_are_rejected() {
        let text = "SELECT 2".to_string();
        let stale = apply_did_change(
            text.clone(),
            Some(5),
            3,
            &[TextDocumentContentChangeEvent {
                range: None,
                text: "SELECT old".to_string(),
            }],
        );
        assert!(stale.is_none(), "older version must not overwrite");
        // Equal and newer versions apply (idempotent redelivery is fine).
        assert!(apply_did_change(text.clone(), Some(5), 5, &[]).is_some());
        assert!(apply_did_change(text, Some(5), 6, &[]).is_some());
    }

    #[test]
    fn ranged_content_changes_apply_with_utf16_positions() {
        let mut text = "SELECT 'é' AS x\nFROM t".to_string();
        // Replace `x` (line 0; é is one UTF-16 unit) with `col`.
        apply_content_change(
            &mut text,
            &TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 14,
                    },
                    end: Position {
                        line: 0,
                        character: 15,
                    },
                }),
                text: "col".to_string(),
            },
        );
        assert_eq!(text, "SELECT 'é' AS col\nFROM t");
        // A rangeless change replaces everything.
        apply_content_change(
            &mut text,
            &TextDocumentContentChangeEvent {
                range: None,
                text: "SELECT 1".to_string(),
            },
        );
        assert_eq!(text, "SELECT 1");
    }

    /// Byte spans map to 0-based line + UTF-16 column across newlines and
    /// multibyte characters, and malformed offsets clamp instead of panic.
    #[test]
    fn byte_spans_convert_to_utf16_positions() {
        let text = "SELECT ż\nFROM 𝄞tbl";
        // 'ż' is 2 UTF-8 bytes / 1 UTF-16 unit; '𝄞' is 4 UTF-8 bytes / 2
        // UTF-16 units.
        let from_start = text.find("FROM").unwrap();
        let r = byte_span_to_range(text, from_start, from_start + 4);
        assert_eq!((r.start.line, r.start.character), (1, 0));
        assert_eq!((r.end.line, r.end.character), (1, 4));

        let tbl_start = text.find("tbl").unwrap();
        let r = byte_span_to_range(text, tbl_start, tbl_start + 3);
        assert_eq!(
            (r.start.line, r.start.character),
            (1, 7),
            "after 4-byte 𝄞 (2 UTF-16 units)"
        );

        // Clamped: past-the-end and inverted spans never panic.
        let r = byte_span_to_range(text, 10_000, 5);
        assert_eq!(r.start.line, 1);
        assert!(r.end.character >= r.start.character || r.end.line >= r.start.line);
    }

    /// `position_to_offset` and `offset_to_position` are inverses on valid
    /// char boundaries, honoring UTF-16 columns across multibyte characters
    /// and CRLF newlines.
    #[test]
    fn position_offset_round_trip() {
        let text = "SELECT ż, 𝄞col\r\nFROM t\nWHERE x = 'é'";
        for (offset, _) in text.char_indices() {
            let pos = offset_to_position(text, offset);
            assert_eq!(
                position_to_offset(text, pos),
                offset,
                "round trip at byte {offset} ({pos:?})"
            );
        }
        // End of text round-trips too.
        let end = offset_to_position(text, text.len());
        assert_eq!(position_to_offset(text, end), text.len());
    }

    /// Out-of-range positions and offsets clamp to the document instead of
    /// panicking.
    #[test]
    fn position_offset_conversions_clamp() {
        let text = "SELECT 1\nFROM t";
        // A line past the end of the document clamps to the start of the
        // text after the final newline instead of panicking.
        let past_line = Position {
            line: 99,
            character: 0,
        };
        assert_eq!(position_to_offset(text, past_line), 9);
        // A column past the end of its line stops at the newline.
        let past_col = Position {
            line: 0,
            character: 99,
        };
        assert_eq!(position_to_offset(text, past_col), 8);
        // An offset past the end clamps to the final position.
        let pos = offset_to_position(text, 10_000);
        assert_eq!((pos.line, pos.character), (1, 6));
    }
}
