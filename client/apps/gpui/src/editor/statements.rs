//! Statement geometry for the gutter play glyph, the current-statement frame
//! and the result-source highlight. Every span comes from
//! `mutation_review::statement_span_at_for` so this module can never disagree
//! with what Run executes.

use sqlly_lang::sqlmodel::Dialect;

use crate::mutation_review::{
    legacy_statement_tiles, span_in_tiles, statement_span_at_for, tiled_statement_spans,
};

/// Byte span of one statement (same contract as `statement_span_at_for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementSpan {
    pub start: usize,
    pub end: usize,
}

/// All statements in `sql`, in order, skipping spans that are blank or
/// comment-only. Walks `statement_span_at_for` from 0, jumping to `end` each time;
/// terminates because `end > cursor` for non-empty text, and falls back to
/// one whole-buffer span when there are no delimiters.
pub fn statement_spans_for(sql: &str, dialect: Dialect) -> Vec<StatementSpan> {
    if sql.is_empty() {
        return Vec::new();
    }
    // Tile the buffer once, then walk the tiles exactly as
    // `statement_span_at_for` would pick them. Calling it per statement
    // re-scanned the whole buffer each time — quadratic, and ~0.7 s per
    // keystroke on a 0.5 MB import script. Oracle-family dialects use the
    // legacy scanner's tiles, as `statement_span_at_for` does.
    let tiled = if dialect == Dialect::Oracle {
        legacy_statement_tiles(sql)
    } else {
        tiled_statement_spans(sql, dialect)
    };
    let span_at = |cursor: usize| {
        if tiled.is_empty() {
            statement_span_at_for(sql, cursor, dialect)
        } else {
            span_in_tiles(&tiled, sql.len(), cursor)
        }
    };
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < sql.len() {
        let (start, end) = span_at(cursor);
        if end <= cursor {
            break;
        }
        if !is_skippable(&sql[start..end]) {
            out.push(StatementSpan { start, end });
        }
        cursor = end;
    }
    out
}

/// T-SQL default for callers without an engine context.
pub fn statement_spans(sql: &str) -> Vec<StatementSpan> {
    statement_spans_for(sql, Dialect::SqlServer)
}

/// Index into `statement_spans` of the statement containing `offset`.
pub fn statement_index_at(spans: &[StatementSpan], offset: usize) -> Option<usize> {
    if spans.is_empty() {
        return None;
    }
    for (i, span) in spans.iter().enumerate() {
        if offset >= span.start && offset < span.end {
            return Some(i);
        }
    }
    let last = spans.last()?;
    if offset >= last.end {
        Some(spans.len() - 1)
    } else {
        None
    }
}

/// First non-blank byte of `span` (the offset a gutter click should run from).
pub fn span_content_start(sql: &str, span: StatementSpan) -> usize {
    first_non_blank(sql, span.start, span.end)
}

/// First and last buffer lines of a span, using the FIRST NON-BLANK byte of
/// the span for the start line (a span that begins right after `;` on the
/// previous line must not frame that previous line) and the last non-blank
/// byte for the end line. `line_of` maps a byte offset to its 0-based line.
pub fn span_lines(
    sql: &str,
    span: StatementSpan,
    line_of: impl Fn(usize) -> usize,
) -> (usize, usize) {
    let start = first_non_blank(sql, span.start, span.end);
    let end = last_non_blank(sql, span.start, span.end);
    (line_of(start), line_of(end))
}

/// Geometry of the current-statement frame in editor-content coordinates.
/// `first_display`/`last_display` are display (post-fold) lines. The frame is
/// inset 1px from the text-area left edge so it never touches the gutter
/// hairline (DESIGN.md: never two hairlines on one edge).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub fn frame_rect(
    first_display: usize,
    last_display: usize,
    line_height: f32,
    gutter_width: f32,
    text_area_width: f32,
) -> FrameRect {
    let first = first_display.min(last_display);
    let last = first_display.max(last_display);
    FrameRect {
        x: gutter_width + 1.0,
        y: first as f32 * line_height,
        width: (text_area_width - 2.0).max(0.0),
        height: (last - first + 1) as f32 * line_height,
    }
}

/// Width of the play-glyph column added to the left of the line numbers.
pub const PLAY_COLUMN_WIDTH: f32 = 14.0;

/// Which display line a pointer at (`x`, `y`) in editor-content coordinates is
/// over, if the pointer is inside the play column. `None` outside the column.
pub fn play_glyph_hit(
    x: f32,
    y: f32,
    line_height: f32,
    display_line_count: usize,
) -> Option<usize> {
    if x < 0.0 || x >= PLAY_COLUMN_WIDTH || y < 0.0 || line_height <= 0.0 || display_line_count == 0
    {
        return None;
    }
    let line = (y / line_height) as usize;
    (line < display_line_count).then_some(line)
}

/// When the executed span contains N statements and the result view reports
/// exactly N result sets, highlight statement `active_index`; otherwise keep
/// the whole executed span rather than guess.
pub fn narrow_result_source(
    span: (usize, usize),
    buffer: &str,
    result_count: usize,
    active_index: usize,
    dialect: Dialect,
) -> (usize, usize) {
    let start = span.0.min(buffer.len());
    let end = span.1.min(buffer.len()).max(start);
    let inner = statement_spans_for(&buffer[start..end], dialect);
    if inner.len() == result_count
        && result_count > 0
        && let Some(st) = inner.get(active_index)
    {
        return (start + st.start, start + st.end);
    }
    (start, end)
}

fn first_non_blank(sql: &str, start: usize, end: usize) -> usize {
    let end = end.min(sql.len());
    let start = start.min(end);
    sql[start..end]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| start + i)
        .unwrap_or(start)
}

fn last_non_blank(sql: &str, start: usize, end: usize) -> usize {
    let end = end.min(sql.len());
    let start = start.min(end);
    sql[start..end]
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| start + i)
        .unwrap_or(start)
}

fn is_skippable(text: &str) -> bool {
    let mut t = text.trim();
    if let Some(stripped) = t.strip_suffix(';') {
        t = stripped.trim();
    }
    if t.is_empty() {
        return true;
    }
    if is_go_separator_line(t) {
        return true;
    }
    is_comment_only(t)
}

fn is_go_separator_line(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 2 || !t.is_char_boundary(2) || !t[..2].eq_ignore_ascii_case("go") {
        return false;
    }
    t[2..]
        .chars()
        .all(|c| matches!(c, ' ' | '\t' | '-' | '/' | '*'))
}

fn is_comment_only(trimmed: &str) -> bool {
    let t = trimmed.trim();
    if t.starts_with("/*") && t.ends_with("*/") {
        return true;
    }
    t.lines().all(|line| {
        let line = line.trim();
        line.is_empty() || line.starts_with("--")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-optimization walk: one `statement_span_at_for` per statement.
    fn spans_by_cursor_walk(sql: &str, dialect: Dialect) -> Vec<StatementSpan> {
        let mut out = Vec::new();
        let mut cursor = 0;
        while cursor < sql.len() {
            let (start, end) = statement_span_at_for(sql, cursor, dialect);
            if end <= cursor {
                break;
            }
            if !is_skippable(&sql[start..end]) {
                out.push(StatementSpan { start, end });
            }
            cursor = end;
        }
        out
    }

    #[test]
    fn listing_statements_splits_the_buffer_once() {
        use crate::mutation_review::WHOLE_BUFFER_SPLITS;
        let sql = "SELECT 1;\nSELECT 2;\n".repeat(200);
        for dialect in Dialect::ALL {
            WHOLE_BUFFER_SPLITS.with(|n| n.set(0));
            let spans = statement_spans_for(&sql, dialect);
            assert!(spans.len() >= 200, "{dialect:?}: {}", spans.len());
            assert_eq!(
                WHOLE_BUFFER_SPLITS.with(|n| n.get()),
                1,
                "{dialect:?} must split once, not once per statement"
            );
        }
    }

    #[test]
    fn single_tiling_matches_the_per_statement_walk() {
        let samples = [
            String::new(),
            "   \n-- only a comment\n".to_string(),
            "SELECT 1".to_string(),
            "SELECT 1; SELECT 2;\n\n-- trailing note\n".to_string(),
            "SELECT 1\nGO\nSELECT 2\nGO\n".to_string(),
            "BEGIN TRAN;\nUPDATE t SET a = 'x;y';\n/* ; */ COMMIT;".to_string(),
            "SELECT 1;\nSELECT 2;\n".repeat(50),
            {
                // A slice of the import fixture: the old walk is quadratic,
                // too slow for the whole thing in a debug test run.
                let full = crate::editor::perf_fixture::import_script();
                let cut = full[..40_000].rfind('\n').unwrap_or(0);
                full[..cut].to_string()
            },
        ];
        let dialects = [
            Dialect::SqlServer,
            Dialect::Postgres,
            Dialect::MySql,
            Dialect::Sqlite,
            Dialect::Oracle,
        ];
        for sql in &samples {
            for dialect in dialects {
                assert_eq!(
                    statement_spans_for(sql, dialect),
                    spans_by_cursor_walk(sql, dialect),
                    "{dialect:?}: {:?}",
                    &sql[..sql.len().min(60)]
                );
            }
        }
    }

    fn line_of(sql: &str, offset: usize) -> usize {
        sql[..offset.min(sql.len())]
            .bytes()
            .filter(|&b| b == b'\n')
            .count()
    }

    fn first_lines(sql: &str) -> Vec<usize> {
        statement_spans(sql)
            .into_iter()
            .map(|span| span_lines(sql, span, |o| line_of(sql, o)).0)
            .collect()
    }

    #[test]
    fn spans_split_on_semicolons() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3";
        let spans = statement_spans(sql);
        assert_eq!(spans.len(), 3);
        assert_eq!(first_lines(sql), vec![0, 1, 2]);
    }

    #[test]
    fn spans_split_on_go_lines() {
        let sql = "SELECT 1\nGO\nSELECT 2";
        let spans = statement_spans(sql);
        assert_eq!(spans.len(), 2);
        assert_eq!(first_lines(sql), vec![0, 2]);
    }

    #[test]
    fn span_after_semicolon_starts_on_next_line() {
        let sql = "SELECT 1;\n\nSELECT 2";
        let spans = statement_spans(sql);
        assert_eq!(spans.len(), 2);
        assert_eq!(span_lines(sql, spans[1], |o| line_of(sql, o)).0, 2);
    }

    #[test]
    fn undelimited_buffer_is_one_span() {
        let sql = "SELECT 1\nFROM t";
        let spans = statement_spans(sql);
        assert_eq!(spans.len(), 1);
        assert_eq!(span_lines(sql, spans[0], |o| line_of(sql, o)), (0, 1));
    }

    #[test]
    fn blank_and_comment_only_spans_are_skipped() {
        let sql = "SELECT 1;\n-- note\n;\nSELECT 2";
        assert_eq!(statement_spans(sql).len(), 2);
    }

    #[test]
    fn statement_index_at_boundaries() {
        let sql = "SELECT 1;\nSELECT 2";
        let spans = statement_spans(sql);
        let semi = sql.find(';').expect("semicolon");
        assert_eq!(statement_index_at(&spans, semi), Some(0));
        assert_eq!(statement_index_at(&spans, sql.len()), Some(spans.len() - 1));
    }

    #[test]
    fn frame_rect_covers_inclusive_lines() {
        let r = frame_rect(2, 4, 20.0, 40.0, 600.0);
        assert_eq!(r.y, 40.0);
        assert_eq!(r.height, 60.0);
        assert_eq!(r.x, 41.0);
        assert_eq!(r.width, 600.0 - 2.0);
    }

    #[test]
    fn play_glyph_hit_inside_and_outside_column() {
        assert_eq!(play_glyph_hit(5.0, 25.0, 20.0, 3), Some(1));
        assert_eq!(play_glyph_hit(20.0, 25.0, 20.0, 3), None);
        assert_eq!(play_glyph_hit(5.0, 1000.0, 20.0, 3), None);
    }

    #[test]
    fn terminates_on_pathological_input() {
        assert!(statement_spans(";;;;").len() < 8);
        assert!(statement_spans("").is_empty());
    }

    #[test]
    fn unicode_buffer_does_not_panic() {
        assert!(!statement_spans("€ SELECT 1;\nSELECT 2").is_empty());
        assert_eq!(statement_spans("か").len(), 1);
    }

    #[test]
    fn narrow_result_source_matches_count() {
        let buffer = "SELECT 1;\nSELECT 2;";
        let spans = statement_spans(buffer);
        assert_eq!(spans.len(), 2);
        let whole = (0, buffer.len());
        let narrowed = narrow_result_source(whole, buffer, 2, 1, Dialect::SqlServer);
        assert_eq!(narrowed, (spans[1].start, spans[1].end));
    }

    #[test]
    fn narrow_result_source_falls_back_when_counts_differ() {
        let buffer = "SELECT 1;\nSELECT 2;";
        let whole = (0, buffer.len());
        assert_eq!(
            narrow_result_source(whole, buffer, 1, 0, Dialect::SqlServer),
            whole
        );
    }
}
