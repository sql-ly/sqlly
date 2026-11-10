//! Tab expansion for the SQL editor's display layer.
//!
//! GPUI's `shape_line` does not expand `\t` — on Windows (DirectWrite) a raw
//! tab shapes to a zero/undefined-width glyph, so pressing Tab looked like a
//! no-op and cursor math drifted on any line containing tabs. The editor
//! therefore shapes a *display* string in which each tab is replaced by the
//! spaces needed to reach the next tab stop, and uses a [`TabMap`] per line
//! to convert byte offsets between the content and display strings.
//!
//! Lines without tabs take a zero-allocation identity fast path.

use gpui::TextRun;

use crate::theme::Theme;

/// A dim label inserted into the display string immediately before content
/// byte `content_col`. `text` should already include any trailing spacing.
pub struct Inlay<'a> {
    pub content_col: usize,
    pub text: &'a str,
}

/// Byte-offset mapping between a content line and its tab-expanded display
/// form.
#[derive(Clone)]
pub struct TabMap {
    display: String,
    /// `c2d[i]` = display byte offset of content byte `i` (length
    /// `content.len() + 1`). `None` when the line contains no tabs and the
    /// mapping is the identity.
    c2d: Option<Vec<usize>>,
    /// Display byte ranges occupied by inlay text (empty when none).
    inlay_spans: Vec<(usize, usize)>,
}

impl TabMap {
    /// Expands tabs to spaces up to the next multiple-of-`tab_width` column
    /// (columns counted in characters, which matches the editor's monospace
    /// rendering).
    pub fn new(line: &str, tab_width: usize) -> Self {
        let tab_width = tab_width.max(1);
        if !line.contains('\t') {
            return Self {
                display: line.to_string(),
                c2d: None,
                inlay_spans: Vec::new(),
            };
        }

        let mut display = String::with_capacity(line.len() + tab_width * 4);
        let mut c2d = Vec::with_capacity(line.len() + 1);
        let mut display_chars = 0usize;
        for (byte_idx, ch) in line.char_indices() {
            // Keep the first byte at the character's leading edge. Map its
            // continuation bytes to the trailing edge so reverse lookups can
            // never select an interior UTF-8 byte as an editor cursor.
            c2d.push(display.len());
            let _ = byte_idx;
            if ch == '\t' {
                let spaces = tab_width - (display_chars % tab_width);
                for _ in 0..spaces {
                    display.push(' ');
                }
                display_chars += spaces;
            } else {
                display.push(ch);
                display_chars += 1;
            }
            for _ in 1..ch.len_utf8() {
                c2d.push(display.len());
            }
        }
        c2d.push(display.len());

        Self {
            display,
            c2d: Some(c2d),
            inlay_spans: Vec::new(),
        }
    }

    /// Like [`new`], but also splices `inlays` (sorted by `content_col`,
    /// all on char boundaries) into the display string. The content byte at
    /// an inlay's `content_col` maps to the display position *after* the
    /// label, so the caret placed at that column sits between the label and
    /// the value. `to_content` snaps positions inside a label to that column.
    /// Empty `inlays` delegates to `new` (identity fast path preserved).
    pub fn with_inlays(line: &str, tab_width: usize, inlays: &[Inlay<'_>]) -> Self {
        if inlays.is_empty() {
            return Self::new(line, tab_width);
        }
        let tab_width = tab_width.max(1);
        let mut display = String::with_capacity(line.len() + tab_width * 4);
        let mut c2d = Vec::with_capacity(line.len() + 1);
        let mut inlay_spans = Vec::with_capacity(inlays.len());
        let mut display_chars = 0usize;
        let mut inlay_i = 0usize;

        for (byte_idx, ch) in line.char_indices() {
            while inlay_i < inlays.len() && inlays[inlay_i].content_col == byte_idx {
                let start = display.len();
                display.push_str(inlays[inlay_i].text);
                inlay_spans.push((start, display.len()));
                display_chars += inlays[inlay_i].text.chars().count();
                inlay_i += 1;
            }
            c2d.push(display.len());
            if ch == '\t' {
                let spaces = tab_width - (display_chars % tab_width);
                for _ in 0..spaces {
                    display.push(' ');
                }
                display_chars += spaces;
            } else {
                display.push(ch);
                display_chars += 1;
            }
            for _ in 1..ch.len_utf8() {
                c2d.push(display.len());
            }
        }
        while inlay_i < inlays.len() && inlays[inlay_i].content_col == line.len() {
            let start = display.len();
            display.push_str(inlays[inlay_i].text);
            inlay_spans.push((start, display.len()));
            inlay_i += 1;
        }
        c2d.push(display.len());

        Self {
            display,
            c2d: Some(c2d),
            inlay_spans,
        }
    }

    /// Display byte ranges occupied by inlay text (empty when none).
    pub fn inlay_spans(&self) -> &[(usize, usize)] {
        &self.inlay_spans
    }

    /// The tab-expanded text to shape/paint.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Content byte offset → display byte offset.
    pub fn to_display(&self, content_col: usize) -> usize {
        match &self.c2d {
            None => content_col.min(self.display.len()),
            Some(map) => map[content_col.min(map.len() - 1)],
        }
    }

    /// Display byte offset → content byte offset. Offsets inside a tab's
    /// space run snap to whichever edge of the tab is closer, so clicking in
    /// the middle of an indent behaves like a native editor.
    pub fn to_content(&self, display_col: usize) -> usize {
        let Some(map) = &self.c2d else {
            return display_col.min(self.display.len());
        };
        let display_col = display_col.min(self.display.len());
        for &(start, end) in self.inlay_spans() {
            if display_col >= start && display_col < end {
                // Inside a label: snap to the content column it prefixes
                // (the value after the label), not the nearer tab-like edge.
                return map.partition_point(|&d| d <= end).saturating_sub(1);
            }
        }
        // Largest content offset whose display position is <= display_col.
        let idx = map.partition_point(|&d| d <= display_col) - 1;
        let start = map[idx];
        if start == display_col {
            return idx;
        }
        // We are inside an expansion (or mid-char): find the next boundary
        // and snap to the closer edge.
        let next = map[idx..]
            .iter()
            .position(|&d| d > display_col)
            .map(|off| idx + off)
            .unwrap_or(map.len() - 1);
        let end = map[next];
        // Continuation bytes share the character's trailing display offset.
        // Choose the last content offset at that edge, which is the next real
        // UTF-8 boundary (and may also be the following character's start).
        let next = map.partition_point(|&d| d <= end).saturating_sub(1);
        if display_col - start <= end.saturating_sub(display_col) {
            idx
        } else {
            next
        }
    }

    /// Adjusts syntax-highlight runs (lengths in content bytes) to cover the
    /// display string instead.
    pub fn expand_runs(&self, runs: &[TextRun]) -> Vec<TextRun> {
        if self.c2d.is_none() {
            return runs.to_vec();
        }
        if self.inlay_spans().is_empty() {
            let mut out = Vec::with_capacity(runs.len());
            let mut content_offset = 0usize;
            for run in runs {
                let start = self.to_display(content_offset);
                content_offset += run.len;
                let end = self.to_display(content_offset);
                let mut run = run.clone();
                run.len = end - start;
                out.push(run);
            }
            return out;
        }
        let mut out = Vec::with_capacity(runs.len() + self.inlay_spans().len() * 2);
        let mut content_offset = 0usize;
        let mut covered = 0usize;
        for run in runs {
            let start = self.to_display(content_offset);
            content_offset += run.len;
            let end = self.to_display(content_offset);
            self.emit_inlays_until(&mut out, run, &mut covered, start);
            self.emit_run_with_inlays(&mut out, run, start, end);
            covered = end;
        }
        if let Some(run) = runs.first() {
            self.emit_inlays_until(&mut out, run, &mut covered, self.display.len());
        }
        out
    }

    fn emit_inlays_until(
        &self,
        out: &mut Vec<TextRun>,
        template: &TextRun,
        covered: &mut usize,
        until: usize,
    ) {
        for &(is, ie) in self.inlay_spans() {
            if ie <= *covered {
                continue;
            }
            if is >= until {
                break;
            }
            if is > *covered {
                break;
            }
            let end = ie.min(until);
            out.push(inlay_run(template, end - *covered));
            *covered = end;
        }
    }

    fn emit_run_with_inlays(
        &self,
        out: &mut Vec<TextRun>,
        run: &TextRun,
        start: usize,
        end: usize,
    ) {
        let mut cursor = start;
        for &(is, ie) in self.inlay_spans() {
            if ie <= cursor {
                continue;
            }
            if is >= end {
                break;
            }
            if is > cursor {
                let mut piece = run.clone();
                piece.len = is - cursor;
                out.push(piece);
                cursor = is;
            }
            let a = cursor.max(is);
            let b = ie.min(end);
            if b > a {
                out.push(inlay_run(run, b - a));
                cursor = b;
            }
        }
        if end > cursor {
            let mut piece = run.clone();
            piece.len = end - cursor;
            out.push(piece);
        }
    }
}

/// Labels render dim *and* italic so they cannot be mistaken for query text
/// (the paint pass additionally draws a faint chip behind each label). The
/// italic face never desyncs cursor math: every x lookup goes through the
/// same shaped line the label was measured in.
fn inlay_run(template: &TextRun, len: usize) -> TextRun {
    let mut font = template.font.clone();
    font.style = gpui::FontStyle::Italic;
    TextRun {
        len,
        font,
        color: Theme::text_dim(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_without_tabs() {
        let map = TabMap::new("select * from t", 4);
        assert_eq!(map.display(), "select * from t");
        assert_eq!(map.to_display(7), 7);
        assert_eq!(map.to_content(7), 7);
    }

    #[test]
    fn leading_tab_expands_to_tab_stop() {
        let map = TabMap::new("\tselect", 4);
        assert_eq!(map.display(), "    select");
        assert_eq!(map.to_display(0), 0);
        assert_eq!(map.to_display(1), 4); // content 's' starts after 4 spaces
        assert_eq!(map.to_content(4), 1);
    }

    #[test]
    fn reverse_mapping_never_returns_the_middle_of_an_emoji() {
        let line = "\t😊x";
        let map = TabMap::new(line, 4);
        let emoji_start = 1;
        let emoji_end = emoji_start + "😊".len();

        for display_col in 0..=map.display().len() {
            let content_col = map.to_content(display_col);
            assert!(
                line.is_char_boundary(content_col),
                "display column {display_col} mapped to interior byte {content_col}"
            );
        }
        assert_eq!(map.to_content(4), emoji_start);
        assert_eq!(map.to_content(8), emoji_end);
    }

    #[test]
    fn mid_line_tab_expands_to_next_stop() {
        // "ab\tc" with width 4: 'ab' at cols 0-1, tab fills cols 2-3.
        let map = TabMap::new("ab\tc", 4);
        assert_eq!(map.display(), "ab  c");
        assert_eq!(map.to_display(2), 2); // the tab itself
        assert_eq!(map.to_display(3), 4); // 'c' after the 2-space fill
    }

    #[test]
    fn tab_at_stop_boundary_expands_full_width() {
        // "abcd\te": 'abcd' fills the first stop; tab jumps a full stop.
        let map = TabMap::new("abcd\te", 4);
        assert_eq!(map.display(), "abcd    e");
        assert_eq!(map.to_display(5), 8);
    }

    #[test]
    fn click_inside_tab_snaps_to_nearest_edge() {
        let map = TabMap::new("\tx", 4); // display "    x"
        assert_eq!(map.to_content(1), 0); // near the start → before the tab
        assert_eq!(map.to_content(3), 1); // near the end → after the tab
        assert_eq!(map.to_content(4), 1); // exactly at 'x'
    }

    #[test]
    fn multibyte_chars_map_correctly() {
        // 'é' is 2 bytes; tab follows at char column 1.
        let map = TabMap::new("\u{e9}\tz", 4);
        assert_eq!(map.display(), "\u{e9}   z");
        assert_eq!(map.to_display(2), 2); // tab starts after the 2-byte char
        assert_eq!(map.to_display(3), 5); // 'z' at display byte 5 (2 + 3 spaces)
        assert_eq!(map.to_content(5), 3);
    }

    #[test]
    fn expand_runs_grows_run_containing_tab() {
        use gpui::{Font, FontFeatures, FontStyle, FontWeight, font};
        let _ = (
            FontFeatures::default(),
            FontStyle::default(),
            FontWeight::default(),
        );
        let f: Font = font("monospace");
        let mk = |len: usize| TextRun {
            len,
            font: f.clone(),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // "\tab" → runs: [1 (tab), 2 (ab)] → display "    ab" → [4, 2].
        let map = TabMap::new("\tab", 4);
        let out = map.expand_runs(&[mk(1), mk(2)]);
        assert_eq!(out[0].len, 4);
        assert_eq!(out[1].len, 2);
        assert_eq!(
            out.iter().map(|r| r.len).sum::<usize>(),
            map.display().len()
        );
    }

    #[test]
    fn inlay_shifts_display_but_not_content() {
        let map = TabMap::with_inlays(
            "(1, 'x')",
            4,
            &[
                Inlay {
                    content_col: 1,
                    text: "id: ",
                },
                Inlay {
                    content_col: 4,
                    text: "name: ",
                },
            ],
        );
        assert_eq!(map.display(), "(id: 1, name: 'x')");
        assert_eq!(map.to_display(1), 5);
        assert_eq!(map.to_display(4), 14);
        assert_eq!(map.to_display(0), 0);
        assert_eq!(map.to_display("(1, 'x')".len()), map.display().len());
    }

    #[test]
    fn to_content_snaps_out_of_inlay() {
        let line = "(1, 'x')";
        let map = TabMap::with_inlays(
            line,
            4,
            &[
                Inlay {
                    content_col: 1,
                    text: "id: ",
                },
                Inlay {
                    content_col: 4,
                    text: "name: ",
                },
            ],
        );
        assert_eq!(map.to_content(2), 1);
        assert_eq!(map.to_content(5), 1);
        for (c, ch) in line.char_indices() {
            let _ = ch;
            assert_eq!(map.to_content(map.to_display(c)), c);
        }
        assert_eq!(map.to_content(map.to_display(line.len())), line.len());
    }

    #[test]
    fn expand_runs_inserts_dim_runs() {
        use gpui::{Font, FontFeatures, FontStyle, FontWeight, font};
        let _ = (
            FontFeatures::default(),
            FontStyle::default(),
            FontWeight::default(),
        );
        let f: Font = font("monospace");
        let mk = |len: usize| TextRun {
            len,
            font: f.clone(),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = "(1, 'x')";
        let map = TabMap::with_inlays(
            line,
            4,
            &[
                Inlay {
                    content_col: 1,
                    text: "id: ",
                },
                Inlay {
                    content_col: 4,
                    text: "name: ",
                },
            ],
        );
        let out = map.expand_runs(&[mk(1), mk(line.len() - 1)]);
        assert_eq!(
            out.iter().map(|r| r.len).sum::<usize>(),
            map.display().len()
        );
        let dim: Vec<usize> = out
            .iter()
            .filter(|r| r.color == Theme::text_dim())
            .map(|r| r.len)
            .collect();
        assert_eq!(dim, vec!["id: ".len(), "name: ".len()]);
        // Labels are italic as well as dim — a run that kept the upright
        // face would read as query text.
        assert!(
            out.iter()
                .filter(|r| r.color == Theme::text_dim())
                .all(|r| r.font.style == FontStyle::Italic)
        );
        assert!(
            out.iter()
                .filter(|r| r.color != Theme::text_dim())
                .all(|r| r.font.style != FontStyle::Italic)
        );
    }

    #[test]
    fn inlays_and_tabs_compose() {
        let line = "\t(1)";
        let map = TabMap::with_inlays(
            line,
            4,
            &[Inlay {
                content_col: 2,
                text: "id: ",
            }],
        );
        assert_eq!(map.display(), "    (id: 1)");
        assert_eq!(map.to_display(0), 0);
        assert_eq!(map.to_display(1), 4);
        assert_eq!(map.to_display(2), 9);
        assert_eq!(map.to_content(9), 2);
        assert_eq!(map.to_content(6), 2);
    }

    #[test]
    fn empty_inlays_is_identity() {
        let line = "select * from t";
        let with = TabMap::with_inlays(line, 4, &[]);
        let new = TabMap::new(line, 4);
        assert_eq!(with.display(), new.display());
        assert!(with.inlay_spans().is_empty());
        let tabbed = "\tselect";
        let with_tab = TabMap::with_inlays(tabbed, 4, &[]);
        let new_tab = TabMap::new(tabbed, 4);
        assert_eq!(with_tab.display(), new_tab.display());
        assert!(with_tab.inlay_spans().is_empty());
    }
}
