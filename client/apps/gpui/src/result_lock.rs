//! The result lock: write the grid's whole on-screen state into the query
//! as one `/* sqlly lock` block of `-- sqlly` directives, so the next run
//! comes back looking exactly the same.
//!
//! Pure text in, text out — the results view gathers the directive lines
//! (see `ResultsView::lock_directive_lines`), the host applies the edit as
//! one undoable step. Clicking the lock again rewrites the block, and every
//! directive of a family the lock owns that sits elsewhere in the code that
//! ran is folded into it, so nothing outside the block can contradict what
//! is on screen.

use crate::sqlly_directive::{Directive, LOCK_OPENER, classify_body, lock_blocks};

/// The opener line the lock writes.
pub fn opener_line() -> String {
    format!(
        "{LOCK_OPENER} — SQLly wrote this from the result grid; click the lock on the result to \
         refresh it."
    )
}

/// The whole block for `lines` (no trailing newline). Lines that would end
/// the comment early are dropped.
pub fn block_text(lines: &[String]) -> String {
    let mut out = opener_line();
    for line in lines.iter().filter(|line| is_block_safe(line)) {
        out.push('\n');
        out.push_str(line);
    }
    out.push_str("\n*/");
    out
}

/// Whether `line` can sit inside the block comment without closing it or
/// opening a nested one.
pub fn is_block_safe(line: &str) -> bool {
    !line.contains("/*") && !line.contains("*/") && !line.contains('\n')
}

/// Whether a directive belongs to a family the lock writes (and therefore
/// rewrites): everything that shapes how results look, not parameters,
/// variables, dates, or canvas placement.
fn lock_owns(directive: &Directive<'_>) -> bool {
    matches!(
        directive,
        Directive::Format { .. }
            | Directive::Column { .. }
            | Directive::Conditional { .. }
            | Directive::Grid { .. }
            | Directive::Pivot { .. }
            | Directive::Chart { .. }
            | Directive::View { .. }
            | Directive::Layout { .. }
            | Directive::Focus { .. }
            | Directive::Lines { .. }
            | Directive::Name { .. }
            | Directive::Map { .. }
    )
}

/// The edited buffer plus the run span carried across the edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockEdit {
    pub text: String,
    pub span: (usize, usize),
    pub changed: bool,
}

/// Write `lines` as the lock block for the run that covered `region` of
/// `buffer`: every lock block and owned directive line inside the region is
/// removed, and one fresh block goes where new directives go (the region's
/// first line, below a first-line canvas directive). The returned span
/// covers the block and the code that ran, so the next refresh finds it.
pub fn apply_lock(buffer: &str, region: (usize, usize), lines: &[String]) -> LockEdit {
    let region = clamp_region(buffer, region);
    let blocks: Vec<_> = lock_blocks(buffer)
        .into_iter()
        .filter(|b| region.0 <= b.start && b.end <= region.1)
        .collect();
    // Whole lines to delete, as (start, end) excluding the terminator.
    let mut doomed: Vec<(usize, usize)> = blocks.iter().map(|b| (b.start, b.end)).collect();
    for comment in crate::result_layout::standalone_comments(buffer) {
        if comment.start < region.0 || comment.end > region.1 {
            continue;
        }
        if blocks
            .iter()
            .any(|b| b.start <= comment.start && comment.end <= b.end)
        {
            continue;
        }
        if classify_body(&comment.body).is_some_and(|d| lock_owns(&d)) {
            // The comment may start after indentation; take its whole line.
            let line_start = buffer[..comment.start]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            doomed.push((line_start, comment.end));
        }
    }
    doomed.sort_unstable();
    doomed.dedup();

    // Delete back to front so earlier offsets stay valid, remembering how
    // far each deletion moved the region's ends.
    let mut text = buffer.to_string();
    let (mut start, mut end) = region;
    for &(line_start, line_end) in doomed.iter().rev() {
        let (del_start, del_end) = if line_end < text.len() {
            (line_start, line_end + 1)
        } else if line_start > 0 {
            (line_start - 1, line_end)
        } else {
            (line_start, line_end)
        };
        let removed = del_end - del_start;
        text.replace_range(del_start..del_end, "");
        if del_end <= start {
            start -= removed;
        } else if del_start < start {
            start = del_start;
        }
        if del_end <= end {
            end -= removed;
        } else if del_start < end {
            end = del_start.max(start);
        }
    }

    let at = crate::result_header_actions::directive_insert_offset(&text, start);
    let block = block_text(lines);
    let inserted = if at == text.len() && !text.is_empty() && !text.ends_with('\n') {
        format!("\n{block}")
    } else {
        format!("{block}\n")
    };
    text.insert_str(at, &inserted);
    let span = (at.min(start), end.max(at) + inserted.len());
    LockEdit {
        changed: text != buffer,
        text,
        span,
    }
}

fn clamp_region(buffer: &str, region: (usize, usize)) -> (usize, usize) {
    let (s, e) = region;
    if s <= e && e <= buffer.len() && buffer.is_char_boundary(s) && buffer.is_char_boundary(e) {
        (s, e)
    } else {
        (0, buffer.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn first_lock_inserts_a_block_and_folds_owned_directives_in() {
        let sql = "-- sqlly view: pivot\n-- sqlly param: @id int\n-- sqlly format: column=A; number=4\nSELECT A FROM t;";
        let edit = apply_lock(
            sql,
            (0, sql.len()),
            &lines(&["-- sqlly format: column=A; number=2; width=40dp"]),
        );
        assert!(edit.changed);
        assert_eq!(
            edit.text,
            format!(
                "{}\n-- sqlly format: column=A; number=2; width=40dp\n*/\n-- sqlly param: @id int\nSELECT A FROM t;",
                opener_line()
            )
        );
        // The parsers see the block's directive and nothing else.
        let hints = crate::query_format::parse(&edit.text);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].width_dp, Some(40));
        assert_eq!(
            crate::result_pivot::parse_view_hints(&edit.text).default_view,
            crate::result_pivot::ResultViewKind::Grid
        );
        assert_eq!(edit.span, (0, edit.text.len()));
    }

    #[test]
    fn relocking_rewrites_the_block_in_place() {
        let sql = "SELECT 1;";
        let first = apply_lock(sql, (0, sql.len()), &lines(&["-- sqlly view: chart"]));
        let second = apply_lock(
            &first.text,
            first.span,
            &lines(&["-- sqlly grid: freeze=1"]),
        );
        assert_eq!(
            second.text,
            format!("{}\n-- sqlly grid: freeze=1\n*/\nSELECT 1;", opener_line())
        );
        let again = apply_lock(
            &second.text,
            second.span,
            &lines(&["-- sqlly grid: freeze=1"]),
        );
        assert!(!again.changed);
    }

    #[test]
    fn a_view_inside_the_block_counts_as_the_first_line() {
        let sql = "SELECT Region, Sales FROM s;";
        let edit = apply_lock(
            sql,
            (0, sql.len()),
            &lines(&[
                "-- sqlly view: pivot",
                "-- sqlly pivot: row:Region; value:Sales:sum",
            ]),
        );
        let hints = crate::result_pivot::parse_view_hints(&edit.text);
        assert_eq!(
            hints.default_view,
            crate::result_pivot::ResultViewKind::Pivot
        );
        assert!(hints.pivot.is_some());
        assert!(
            crate::sqlly_directive::directive_diagnostics(&edit.text).is_empty(),
            "{:?}",
            crate::sqlly_directive::directive_diagnostics(&edit.text)
        );
    }

    #[test]
    fn only_the_region_that_ran_is_touched() {
        let first = "-- sqlly format: column=A; number=1\nSELECT A FROM a;\n";
        let second = "-- sqlly format: column=B; number=1\nSELECT B FROM b;";
        let sql = format!("{first}{second}");
        let region = (first.len(), sql.len());
        let edit = apply_lock(
            &sql,
            region,
            &lines(&["-- sqlly format: column=B; width=10dp"]),
        );
        assert!(edit.text.starts_with(first));
        assert!(!edit.text.contains("column=B; number=1"));
        assert_eq!(edit.span.0, first.len());
        assert_eq!(edit.span.1, edit.text.len());
    }

    #[test]
    fn block_breaking_lines_are_dropped() {
        let text = block_text(&lines(&[
            "-- sqlly format: column=A; template={value} */ x",
            "-- sqlly grid: freeze=1",
        ]));
        assert!(!text.contains("template"));
        assert_eq!(crate::sqlly_directive::lock_blocks(&text).len(), 1);
    }

    #[test]
    fn other_block_comments_still_switch_directives_off() {
        let sql = "/* off\n-- sqlly view: pivot\n*/\nSELECT 1;";
        assert_eq!(
            crate::result_pivot::parse_view_hints(sql).default_view,
            crate::result_pivot::ResultViewKind::Grid
        );
        // And locking leaves a disabled directive alone.
        let edit = apply_lock(sql, (0, sql.len()), &lines(&["-- sqlly grid: freeze=1"]));
        assert!(edit.text.contains("/* off\n-- sqlly view: pivot\n*/"));
    }
}
