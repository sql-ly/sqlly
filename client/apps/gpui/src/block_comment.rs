//! Toggle Block Comment: wrap a span of SQL in `/* … */`, or take the
//! markers off when the span already is one block comment.
//!
//! Pure text transform; the editor applies the returned splice as a single
//! undo step.

use std::ops::Range;

/// One splice the editor applies: replace `replace` in the original text with
/// `text`, then select `selection` (offsets into the edited text; an empty
/// range is a plain caret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockCommentEdit {
    pub replace: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

/// Toggle a block comment for the selection `selection` in `doc`.
///
/// With a selection, the selected text (minus surrounding whitespace) is
/// wrapped, or unwrapped when it is already exactly one block comment or the
/// markers sit right outside it. With an empty selection the caret's line is
/// used the same way, and an empty line gets an empty `/*  */` with the caret
/// inside. Returns `None` when there is nothing to do.
pub(crate) fn toggle_block_comment(doc: &str, selection: Range<usize>) -> Option<BlockCommentEdit> {
    let start = selection.start.min(doc.len());
    let end = selection.end.min(doc.len()).max(start);
    if start == end {
        return toggle_at_caret(doc, start);
    }
    let core = trim_range(doc, start..end);
    if core.is_empty() {
        return None;
    }
    let spans = block_comment_spans(doc);
    Some(match enclosing_comment(doc, &spans, &core) {
        Some(span) => {
            let (text, _) = unwrapped(doc, &span);
            let selection = span.start..span.start + text.len();
            BlockCommentEdit {
                replace: span,
                text,
                selection,
            }
        }
        None => {
            let text = format!("/* {} */", &doc[core.clone()]);
            let selection = core.start..core.start + text.len();
            BlockCommentEdit {
                replace: core,
                text,
                selection,
            }
        }
    })
}

/// No selection: work on the caret's line, keeping the caret on the same
/// character of text.
fn toggle_at_caret(doc: &str, caret: usize) -> Option<BlockCommentEdit> {
    let line_start = doc[..caret].rfind('\n').map_or(0, |i| i + 1);
    let line_end = doc[caret..].find('\n').map_or(doc.len(), |i| caret + i);
    let core = trim_range(doc, line_start..line_end);
    if core.is_empty() {
        return Some(BlockCommentEdit {
            replace: caret..caret,
            text: "/*  */".to_string(),
            selection: caret + 3..caret + 3,
        });
    }
    let spans = block_comment_spans(doc);
    let (replace, text, caret) = match enclosing_comment(doc, &spans, &core) {
        Some(span) => {
            let (text, opener_len) = unwrapped(doc, &span);
            let removed = span.len() - text.len();
            let caret = if caret <= span.start {
                caret
            } else if caret >= span.end {
                caret - removed
            } else {
                caret
                    .saturating_sub(opener_len)
                    .clamp(span.start, span.start + text.len())
            };
            (span, text, caret)
        }
        None => {
            let text = format!("/* {} */", &doc[core.clone()]);
            let caret = if caret < core.start {
                caret
            } else if caret >= core.end {
                caret + 6
            } else {
                caret + 3
            };
            (core, text, caret)
        }
    };
    Some(BlockCommentEdit {
        replace,
        text,
        selection: caret..caret,
    })
}

/// `range` shrunk to exclude leading and trailing whitespace.
fn trim_range(doc: &str, range: Range<usize>) -> Range<usize> {
    let s = &doc[range.clone()];
    let start = range.start + (s.len() - s.trim_start().len());
    start..start + s.trim().len()
}

/// The body of the block comment `span` with its markers (and one space just
/// inside each, if present) removed, plus how many bytes came off the front.
fn unwrapped(doc: &str, span: &Range<usize>) -> (String, usize) {
    let inner = &doc[span.start + 2..span.end - 2];
    let lead = usize::from(inner.starts_with(' '));
    let rest = &inner[lead..];
    let rest = rest.strip_suffix(' ').unwrap_or(rest);
    (rest.to_string(), 2 + lead)
}

/// The block comment to unwrap for `core`: one that is exactly `core`, or
/// else the innermost one whose markers sit right outside it with only
/// whitespace between.
fn enclosing_comment(
    doc: &str,
    spans: &[Range<usize>],
    core: &Range<usize>,
) -> Option<Range<usize>> {
    if let Some(exact) = spans.iter().find(|span| *span == core) {
        return Some(exact.clone());
    }
    spans
        .iter()
        .filter(|span| {
            span.start + 2 <= core.start
                && core.end + 2 <= span.end
                && doc[span.start + 2..core.start].trim().is_empty()
                && doc[core.end..span.end - 2].trim().is_empty()
        })
        .min_by_key(|span| span.len())
        .cloned()
}

/// Every closed `/* … */` comment in `doc`, nested ones included, skipping
/// markers inside string literals, quoted identifiers and `--` comments.
/// Nesting follows SQL Server and PostgreSQL: each `/*` needs its own `*/`.
fn block_comment_spans(doc: &str) -> Vec<Range<usize>> {
    let bytes = doc.as_bytes();
    let mut spans = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let next = bytes.get(i + 1).copied();
        if !open.is_empty() {
            match (bytes[i], next) {
                (b'/', Some(b'*')) => {
                    open.push(i);
                    i += 2;
                }
                (b'*', Some(b'/')) => {
                    let start = open.pop().expect("inside a comment");
                    spans.push(start..i + 2);
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        match (bytes[i], next) {
            (b'/', Some(b'*')) => {
                open.push(i);
                i += 2;
            }
            (b'-', Some(b'-')) => {
                i = doc[i..].find('\n').map_or(bytes.len(), |n| i + n + 1);
            }
            (b'\'', _) => i = quoted_end(bytes, i, b'\''),
            (b'"', _) => i = quoted_end(bytes, i, b'"'),
            (b'`', _) => i = quoted_end(bytes, i, b'`'),
            (b'[', _) => i = quoted_end(bytes, i, b']'),
            _ => i += 1,
        }
    }
    spans
}

/// Index just past the quoted run opened at `start`, where a doubled closing
/// character is an escaped one.
fn quoted_end(bytes: &[u8], start: usize, close: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == close {
            if bytes.get(i + 1) == Some(&close) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply the toggle and return the new document plus the selected text.
    fn run(doc: &str, sel: Range<usize>) -> (String, Range<usize>) {
        let edit = toggle_block_comment(doc, sel).expect("an edit");
        let mut out = String::new();
        out.push_str(&doc[..edit.replace.start]);
        out.push_str(&edit.text);
        out.push_str(&doc[edit.replace.end..]);
        (out, edit.selection)
    }

    fn sel_of(doc: &str, needle: &str) -> Range<usize> {
        let start = doc.find(needle).expect("needle in doc");
        start..start + needle.len()
    }

    #[test]
    fn wraps_a_partial_line_selection() {
        let doc = "SELECT a, b FROM t";
        let (out, sel) = run(doc, sel_of(doc, "a, b"));
        assert_eq!(out, "SELECT /* a, b */ FROM t");
        assert_eq!(&out[sel], "/* a, b */");
    }

    #[test]
    fn unwraps_a_selection_that_is_one_block_comment() {
        let doc = "SELECT /* a, b */ FROM t";
        let (out, sel) = run(doc, sel_of(doc, "/* a, b */"));
        assert_eq!(out, "SELECT a, b FROM t");
        assert_eq!(&out[sel], "a, b");
    }

    #[test]
    fn toggling_twice_restores_the_original_text() {
        let doc = "SELECT a\nFROM t\nWHERE x = 1";
        let (wrapped, sel) = run(doc, sel_of(doc, "FROM t\nWHERE x = 1"));
        assert_eq!(wrapped, "SELECT a\n/* FROM t\nWHERE x = 1 */");
        let (restored, sel) = run(&wrapped, sel);
        assert_eq!(restored, doc);
        assert_eq!(&restored[sel], "FROM t\nWHERE x = 1");
    }

    #[test]
    fn unwraps_when_the_markers_sit_just_outside_the_selection() {
        let doc = "SELECT /* a, b */ FROM t";
        let (out, sel) = run(doc, sel_of(doc, "a, b"));
        assert_eq!(out, "SELECT a, b FROM t");
        assert_eq!(&out[sel], "a, b");
    }

    #[test]
    fn multi_line_selection_keeps_trailing_newline_outside_the_comment() {
        let doc = "SELECT 1;\nSELECT 2;\nSELECT 3;\n";
        // Whole-line selection of the first two lines, newline included.
        let (out, sel) = run(doc, 0..doc.find("SELECT 3").unwrap());
        assert_eq!(out, "/* SELECT 1;\nSELECT 2; */\nSELECT 3;\n");
        assert_eq!(&out[sel], "/* SELECT 1;\nSELECT 2; */");
    }

    #[test]
    fn unwraps_a_multi_line_comment_without_inner_spaces() {
        let doc = "/*\nSELECT 1;\n*/\nSELECT 2;";
        let (out, _) = run(doc, sel_of(doc, "/*\nSELECT 1;\n*/"));
        assert_eq!(out, "\nSELECT 1;\n\nSELECT 2;");
    }

    #[test]
    fn wrapping_a_selection_that_contains_a_comment_nests_it() {
        let doc = "SELECT a /* note */, b FROM t";
        let (out, sel) = run(doc, sel_of(doc, "a /* note */, b"));
        assert_eq!(out, "SELECT /* a /* note */, b */ FROM t");
        // The outer comment is matched as a whole, so it unwraps back.
        let (restored, _) = run(&out, sel);
        assert_eq!(restored, doc);
    }

    #[test]
    fn two_separate_comments_are_wrapped_not_unwrapped() {
        // Starts with /* and ends with */, but they are two comments.
        let doc = "/* a */ b /* c */";
        let (out, _) = run(doc, 0..doc.len());
        assert_eq!(out, "/* /* a */ b /* c */ */");
    }

    #[test]
    fn markers_inside_a_string_are_not_mistaken_for_a_comment() {
        let doc = "SELECT '/* x */' AS a";
        let (out, _) = run(doc, sel_of(doc, "x"));
        assert_eq!(out, "SELECT '/* /* x */ */' AS a");
        let (out, _) = run(doc, sel_of(doc, "/* x */"));
        assert_eq!(out, "SELECT '/* /* x */ */' AS a");
    }

    #[test]
    fn no_selection_wraps_the_current_line_and_keeps_the_caret_on_its_text() {
        let doc = "SELECT 1;\n    SELECT 2;\nSELECT 3;";
        let caret = doc.find("2;").unwrap();
        let (out, sel) = run(doc, caret..caret);
        assert_eq!(out, "SELECT 1;\n    /* SELECT 2; */\nSELECT 3;");
        assert!(sel.is_empty());
        assert_eq!(&out[sel.start..sel.start + 2], "2;");
    }

    #[test]
    fn no_selection_unwraps_a_commented_line() {
        let doc = "SELECT 1;\n    /* SELECT 2; */\nSELECT 3;";
        let caret = doc.find("2;").unwrap();
        let (out, sel) = run(doc, caret..caret);
        assert_eq!(out, "SELECT 1;\n    SELECT 2;\nSELECT 3;");
        assert_eq!(&out[sel.start..sel.start + 2], "2;");
    }

    #[test]
    fn no_selection_on_a_blank_line_inserts_an_empty_comment() {
        let doc = "SELECT 1;\n\nSELECT 2;";
        let caret = doc.find("\n\n").unwrap() + 1;
        let (out, sel) = run(doc, caret..caret);
        assert_eq!(out, "SELECT 1;\n/*  */\nSELECT 2;");
        assert_eq!(sel, caret + 3..caret + 3);
        // And toggling again from inside takes it back out.
        let (restored, _) = run(&out, sel);
        assert_eq!(restored, doc);
    }

    #[test]
    fn whitespace_only_selection_on_a_text_line_is_a_no_op() {
        let doc = "SELECT 1;   \nSELECT 2;";
        let start = doc.find("   ").unwrap();
        assert_eq!(toggle_block_comment(doc, start..start + 3), None);
    }

    #[test]
    fn handles_multibyte_text() {
        let doc = "SELECT 'é' AS ü";
        let (out, sel) = run(doc, sel_of(doc, "'é' AS ü"));
        assert_eq!(out, "SELECT /* 'é' AS ü */");
        let (restored, _) = run(&out, sel);
        assert_eq!(restored, doc);
    }
}
