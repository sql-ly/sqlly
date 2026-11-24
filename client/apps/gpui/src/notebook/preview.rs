//! The cheap stand-in for a SQL cell that has no live editor: one styled text
//! run coloured by the app's own tokenizer, laid out at the editor's line
//! height so swapping it for the editor does not move the cell.

use std::ops::Range;

use gpui::{HighlightStyle, SharedString};
use sqlly_lang::sqlmodel::Dialect;

use crate::theme::Theme;

/// Lines a cell shows before its editor scrolls (and its preview clips).
pub const MAX_VISIBLE_LINES: usize = 24;
pub const MIN_VISIBLE_LINES: usize = 2;

#[derive(Clone)]
pub struct SqlPreview {
    /// Hash of the source this preview was built from.
    pub source_hash: u64,
    pub text: SharedString,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
}

pub fn source_hash(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

pub fn line_count(source: &str) -> usize {
    source.split('\n').count().max(1)
}

/// Rows the cell's SQL area takes: the source's lines clamped to the
/// visible range (the editor scrolls past the maximum).
pub fn visible_lines(source: &str) -> usize {
    line_count(source).clamp(MIN_VISIBLE_LINES, MAX_VISIBLE_LINES)
}

pub fn build(dialect: Dialect, source: &str) -> SqlPreview {
    let clipped_end = source
        .match_indices('\n')
        .nth(MAX_VISIBLE_LINES - 1)
        .map(|(i, _)| i)
        .unwrap_or(source.len());
    let text = &source[..clipped_end];
    let tokens = crate::sql_syntax::classify_for(dialect, text);
    let mut highlights = Vec::with_capacity(tokens.len());
    let mut cursor = 0usize;
    for token in tokens {
        let start = token.start.max(cursor);
        let end = token.end.min(text.len());
        if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        highlights.push((
            start..end,
            HighlightStyle {
                color: Some(Theme::syntax_color(token.kind)),
                ..Default::default()
            },
        ));
        cursor = end;
    }
    SqlPreview {
        source_hash: source_hash(source),
        text: SharedString::from(text.to_string()),
        highlights,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_are_sorted_disjoint_and_on_char_boundaries() {
        let src = "SELECT 'héllo', 42 -- note\nFROM dbo.T WHERE x = @p";
        let preview = build(Dialect::SqlServer, src);
        assert!(!preview.highlights.is_empty());
        let mut last = 0;
        for (range, _) in &preview.highlights {
            assert!(range.start >= last);
            assert!(src.is_char_boundary(range.start) && src.is_char_boundary(range.end));
            last = range.end;
        }
        assert_eq!(line_count(src), 2);
    }

    #[test]
    fn long_sources_are_clipped_to_the_visible_lines() {
        let src = (0..100)
            .map(|i| format!("SELECT {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = build(Dialect::Postgres, &src);
        assert_eq!(preview.text.lines().count(), MAX_VISIBLE_LINES);
        assert_eq!(line_count(&src), 100);
        assert_eq!(visible_lines(&src), MAX_VISIBLE_LINES);
        assert_eq!(visible_lines(""), MIN_VISIBLE_LINES);
    }
}
