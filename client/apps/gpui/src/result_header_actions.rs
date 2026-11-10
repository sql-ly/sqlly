//! Pure logic behind the result-name menu and the Result Settings dialog:
//! the `-- sqlly focus:` / `-- sqlly lines <n>:` / `-- sqlly name <n>:`
//! directive edits, the visible-row count a stacked card is saved with, and
//! commenting out exactly the statement that produced a result set.
//!
//! Everything here is text in, [`TextEdit`]s out — no UI, no I/O — so the
//! host applies the combined result as one undoable editor edit.

use std::ops::Range;

use sqlly_lang::sqlmodel::Dialect;

use crate::sqlly_directive::{Directive, classify, classify_body};

/// A row partially in view counts as visible only when MORE than this
/// fraction of its height shows.
pub const VISIBLE_ROW_THRESHOLD: f32 = 0.25;

/// How many of `total_rows` rows (each `row_height` tall) a viewport
/// `viewport_height` tall, scrolled down by `scroll_y`, shows. A row cut off
/// at the top or bottom counts when more than [`VISIBLE_ROW_THRESHOLD`] of it
/// is visible; 25% or less does not.
pub fn visible_row_count(
    scroll_y: f32,
    viewport_height: f32,
    row_height: f32,
    total_rows: usize,
) -> usize {
    if row_height <= 0.0 || viewport_height <= 0.0 || total_rows == 0 {
        return 0;
    }
    let top = scroll_y.max(0.0);
    let bottom = top + viewport_height;
    let first = (top / row_height).floor() as usize;
    let last = ((bottom / row_height).ceil() as usize).min(total_rows);
    // Tolerance so float noise at an exact 25% boundary never tips it over.
    let threshold = VISIBLE_ROW_THRESHOLD * row_height + 1e-3;
    (first..last)
        .filter(|&row| {
            let row_top = row as f32 * row_height;
            let shown = (row_top + row_height).min(bottom) - row_top.max(top);
            shown > threshold
        })
        .count()
}

/// One replacement over a text buffer (byte offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
}

impl TextEdit {
    pub fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + self.replacement.len());
        out.push_str(&text[..self.range.start]);
        out.push_str(&self.replacement);
        out.push_str(&text[self.range.end..]);
        out
    }

    fn delta(&self) -> isize {
        self.replacement.len() as isize - self.range.len() as isize
    }

    /// Carry a span recorded before this edit across it: an edit at or
    /// before the span moves it, an edit inside it stretches its end.
    pub fn shift_span(&self, span: (usize, usize)) -> (usize, usize) {
        let delta = self.delta();
        let shift = |offset: usize| (offset as isize + delta).max(0) as usize;
        if self.range.end <= span.0 {
            (shift(span.0), shift(span.1))
        } else if self.range.start < span.1 {
            (span.0.min(self.range.start), shift(span.1).max(span.0))
        } else {
            span
        }
    }
}

// ---------------------------------------------------------------------------
// Directive edits
// ---------------------------------------------------------------------------

/// Where a new directive line goes: the start of the line holding
/// `region_start` (the run region's first byte — the buffer top for a
/// whole-buffer run), but below a `view:` / canvas directive on the region's
/// first non-empty line, since those are only honored there.
pub fn directive_insert_offset(sql: &str, region_start: usize) -> usize {
    let offset = directive_insert_offset_unlocked(
        &crate::sqlly_directive::unwrap_lock_blocks(sql),
        region_start,
    );
    // Never inside a lock block: below it, so its own directives keep their
    // first-line position and the next refresh owns only what it wrote.
    match crate::sqlly_directive::lock_blocks(sql)
        .into_iter()
        .find(|block| block.start < offset && offset <= block.end)
    {
        Some(block) if block.end == sql.len() => sql.len(),
        Some(block) => block.end + 1,
        None => offset,
    }
}

fn directive_insert_offset_unlocked(sql: &str, region_start: usize) -> usize {
    let mut at = region_start.min(sql.len());
    while at > 0 && !sql.is_char_boundary(at) {
        at -= 1;
    }
    let mut line_start = sql[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    // Find the first non-empty line at or after the insert point.
    let mut cursor = line_start;
    while cursor < sql.len() {
        let line_end = sql[cursor..]
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(sql.len());
        let line = &sql[cursor..line_end];
        if line.trim().is_empty() {
            cursor = line_end + 1;
            continue;
        }
        if matches!(
            classify(line.trim()),
            Some(Directive::View { .. } | Directive::Canvas { .. })
        ) {
            line_start = (line_end + 1).min(sql.len());
            if line_end == sql.len() {
                // The pinned line is the last one: the new directive needs
                // its own line after it.
                return sql.len();
            }
        }
        break;
    }
    line_start
}

fn insert_directive_line(sql: &str, region_start: usize, line: &str) -> TextEdit {
    let at = directive_insert_offset(sql, region_start);
    let replacement = if at == sql.len() && !sql.is_empty() && !sql.ends_with('\n') {
        format!("\n{line}")
    } else {
        format!("{line}\n")
    };
    TextEdit {
        range: at..at,
        replacement,
    }
}

fn replace_directive_line(sql: &str, start: usize, end: usize, line: &str) -> TextEdit {
    let current = &sql[start..end];
    let indent = &current[..current.len() - current.trim_start().len()];
    TextEdit {
        range: start..end,
        replacement: format!("{indent}{line}"),
    }
}

/// Remove whole lines (with their terminators), as one edit spanning them.
/// `lines` must be in document order; `None` when empty.
fn remove_lines(sql: &str, lines: &[(usize, usize)]) -> Option<TextEdit> {
    let first = lines.first()?;
    let last = lines.last()?;
    let mut start = first.0;
    let mut end = last.1;
    let mut ate_preceding_newline = false;
    if end < sql.len() {
        end += 1; // the '\n'
    } else if start > 0 {
        start -= 1; // last line: eat the preceding '\n' instead
        ate_preceding_newline = true;
    }
    let mut kept = String::new();
    // Keep any non-target lines that sit between the first and last target.
    let mut cursor = first.0;
    for (s, e) in lines {
        if *s > cursor {
            kept.push_str(&sql[cursor..*s]);
        }
        cursor = (*e + 1).min(sql.len());
    }
    if last.1 == sql.len() && !kept.is_empty() {
        // The removed last line had no terminator, so the kept lines must
        // not end with one either — and when the newline before the first
        // target was eaten, they need it back in front.
        let body = kept.strip_suffix('\n').unwrap_or(&kept);
        kept = if ate_preceding_newline {
            format!("\n{body}")
        } else {
            body.to_string()
        };
    }
    Some(TextEdit {
        range: start..end,
        replacement: kept,
    })
}

/// `-- sqlly focus: <slot + 1>`.
pub fn focus_directive_line(slot: usize) -> String {
    format!("-- sqlly focus: {}", slot + 1)
}

/// `-- sqlly lines <slot + 1>: <rows>`.
pub fn lines_directive_line(slot: usize, rows: usize) -> String {
    format!("-- sqlly lines {}: {}", slot + 1, rows)
}

/// Whether a directive line at `start..end` was part of the run: directives
/// are honored from the executed text, so one outside the run region is
/// neither the one in force nor the one to rewrite.
fn in_region(region: (usize, usize), start: usize, end: usize) -> bool {
    region.0 <= start && end <= region.1
}

/// The focus directive lines in `sql[region]`, with their parsed value
/// (`None` when the value is invalid).
fn focus_lines(sql: &str, region: (usize, usize)) -> Vec<(usize, usize, Option<usize>)> {
    crate::result_layout::standalone_comments(sql)
        .into_iter()
        .filter(|c| in_region(region, c.start, c.end))
        .filter_map(|c| match classify_body(&c.body)? {
            Directive::Focus { body } => Some((
                c.start,
                c.end,
                crate::result_layout::parse_focus_value(body),
            )),
            _ => None,
        })
        .collect()
}

/// The indexed `lines <slot + 1>:` directive lines in `sql[region]`.
fn indexed_lines_lines(sql: &str, region: (usize, usize), slot: usize) -> Vec<(usize, usize)> {
    crate::result_layout::standalone_comments(sql)
        .into_iter()
        .filter(|c| in_region(region, c.start, c.end))
        .filter_map(|c| match classify_body(&c.body)? {
            Directive::Lines {
                index: Some(index), ..
            } if index == slot + 1 => Some((c.start, c.end)),
            _ => None,
        })
        .collect()
}

/// Make result set `slot` the default-visible one: rewrite the last focus
/// directive in the run region `region` (the one that wins) or insert a new
/// one at the region's start. `None` when it already says so.
pub fn set_focus_edit(sql: &str, region: (usize, usize), slot: usize) -> Option<TextEdit> {
    let line = focus_directive_line(slot);
    match focus_lines(sql, region).last() {
        Some((_, _, Some(n))) if *n == slot + 1 => None,
        Some((start, end, _)) => Some(replace_directive_line(sql, *start, *end, &line)),
        None => Some(insert_directive_line(sql, region.0, &line)),
    }
}

/// Drop every focus directive in `region` naming result set `slot`.
pub fn clear_focus_edit(sql: &str, region: (usize, usize), slot: usize) -> Option<TextEdit> {
    let lines: Vec<(usize, usize)> = focus_lines(sql, region)
        .into_iter()
        .filter(|(_, _, n)| *n == Some(slot + 1))
        .map(|(s, e, _)| (s, e))
        .collect();
    remove_lines(sql, &lines)
}

/// Allot `rows` rows to result set `slot`: rewrite its last indexed `lines`
/// directive in `region` or insert one at the region's start. `None` when it
/// already says so.
pub fn set_lines_edit(
    sql: &str,
    region: (usize, usize),
    slot: usize,
    rows: usize,
) -> Option<TextEdit> {
    let rows = rows.clamp(1, crate::result_layout::MAX_LINES);
    let line = lines_directive_line(slot, rows);
    match indexed_lines_lines(sql, region, slot).last() {
        Some((start, end)) if sql[*start..*end].trim() == line => None,
        Some((start, end)) => Some(replace_directive_line(sql, *start, *end, &line)),
        None => Some(insert_directive_line(sql, region.0, &line)),
    }
}

/// Drop every indexed `lines` directive in `region` for result set `slot`.
pub fn clear_lines_edit(sql: &str, region: (usize, usize), slot: usize) -> Option<TextEdit> {
    remove_lines(sql, &indexed_lines_lines(sql, region, slot))
}

/// Name result set `slot`: rewrite the winning `name` directive's text in
/// place (the last one in `region`) or insert a new one at the region's
/// start. `None` for a blank name or no change.
pub fn set_name_edit(
    sql: &str,
    region: (usize, usize),
    slot: usize,
    name: &str,
) -> Option<TextEdit> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let assignments = crate::result_set_names::parse(sql);
    match assignments
        .iter()
        .rev()
        .filter(|a| in_region(region, a.comment_range.start, a.comment_range.end))
        .find(|a| a.result_set_index == slot)
    {
        Some(a) if a.name == name => None,
        Some(a) => Some(TextEdit {
            range: a.name_range.clone(),
            replacement: name.to_string(),
        }),
        None => Some(insert_directive_line(
            sql,
            region.0,
            &format!("-- sqlly name {}: {}", slot + 1, name),
        )),
    }
}

// ---------------------------------------------------------------------------
// Statement ↔ result mapping and commenting out
// ---------------------------------------------------------------------------

fn is_go_line(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 2 || !t.is_char_boundary(2) || !t[..2].eq_ignore_ascii_case("go") {
        return false;
    }
    let rest = t[2..].trim();
    rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()) || rest.starts_with("--")
}

/// Byte range of the statement's code inside `span`: from the first byte of
/// the first line carrying code to the last non-blank byte of the last one.
/// Leading/trailing blank, comment-only and `GO` lines (directives that sit
/// above a statement, batch separators) are not part of it. `None` for a
/// span with no code.
pub fn statement_code_range(sql: &str, span: (usize, usize)) -> Option<(usize, usize)> {
    let start = span.0.min(sql.len());
    let end = span.1.min(sql.len()).max(start);
    let text = &sql[start..end];
    let mut in_block = false;
    // A block comment still open at the end of a code line: the lines it
    // runs on to belong to the statement, or commenting it out line by line
    // would leave its `*/` dangling.
    let mut block_from_code = false;
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    let mut offset = 0usize;
    for raw in text.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        let has_code = line_has_code(line, &mut in_block) && !is_go_line(line);
        if has_code {
            let lead = line.len() - line.trim_start().len();
            if first.is_none() {
                first = Some(start + offset + lead);
            }
            last = Some(start + offset + line.trim_end().len());
        } else if block_from_code {
            last = Some(start + offset + line.trim_end().len());
        }
        block_from_code = in_block && (has_code || block_from_code);
        offset += raw.len();
    }
    Some((first?, last?))
}

/// Whether `line` carries anything besides whitespace and comments,
/// threading block-comment state across lines (strings are treated as code).
fn line_has_code(line: &str, in_block: &mut bool) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut code = false;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if *in_block {
            if b == b'*' && next == Some(b'/') {
                *in_block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_string {
            if b == b'\'' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'-' if next == Some(b'-') => break,
            b'/' if next == Some(b'*') => {
                *in_block = true;
                i += 2;
                continue;
            }
            b'\'' => {
                in_string = true;
                code = true;
            }
            b if !b.is_ascii_whitespace() => code = true,
            _ => {}
        }
        i += 1;
    }
    code
}

/// Comment out exactly the statement in `span`: `-- ` at the statement's
/// common indentation on every non-blank line of its code. Code sharing the
/// first or last line with a neighbouring statement is moved onto its own
/// line first, so only this statement stops running. `None` when the span
/// holds no code.
pub fn comment_out_edit(sql: &str, span: (usize, usize)) -> Option<TextEdit> {
    let (cs, ce) = statement_code_range(sql, span)?;
    let line_start = sql[..cs].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = sql[ce..].find('\n').map(|i| ce + i).unwrap_or(sql.len());
    let before = &sql[line_start..cs];
    let after = &sql[ce..line_end];

    let mut replacement = String::new();
    let body = if before.trim().is_empty() {
        &sql[line_start..ce]
    } else {
        replacement.push_str(before.trim_end());
        replacement.push('\n');
        &sql[cs..ce]
    };
    let indent = body
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let commented: Vec<String> = body
        .split('\n')
        .map(|l| {
            if l.trim().is_empty() {
                l.to_string()
            } else {
                format!("{}-- {}", &l[..indent], &l[indent..])
            }
        })
        .collect();
    replacement.push_str(&commented.join("\n"));
    if after.trim().is_empty() {
        replacement.push_str(after);
    } else {
        replacement.push('\n');
        replacement.push_str(after.trim_start());
    }
    Some(TextEdit {
        range: line_start..line_end,
        replacement,
    })
}

/// Leading keyword of a statement's code, uppercased.
fn leading_keyword(code: &str) -> String {
    code.trim_start_matches(|c: char| c == '(' || c.is_whitespace())
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Best-effort: whether a statement returns a result set when run.
pub fn statement_returns_rows(code: &str) -> bool {
    let upper = code.to_ascii_uppercase();
    let has_word = |word: &str| {
        upper
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|w| w == word)
    };
    match leading_keyword(code).as_str() {
        // `SELECT … INTO` writes a table or variables instead of returning rows.
        "SELECT" => !has_word("INTO"),
        "WITH" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "PRAGMA"
        | "EXEC" | "EXECUTE" | "CALL" => true,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => has_word("RETURNING") || has_word("OUTPUT"),
        kw => kw.starts_with("SP_"),
    }
}

/// The code of each statement in `sql[region]`, as `(absolute span, code)`.
fn region_statements(
    sql: &str,
    region: (usize, usize),
    dialect: Dialect,
) -> Vec<((usize, usize), String)> {
    let start = region.0.min(sql.len());
    let end = region.1.min(sql.len()).max(start);
    let text = &sql[start..end];
    // The splitter's own spans (not `statement_spans_for`, whose tiling walk
    // steps over a statement that follows a `GO` line). Oracle has no
    // splitter: the region is one statement.
    let mut spans = crate::mutation_review::tiled_statement_spans(text, dialect);
    if spans.is_empty() {
        spans.push((0, text.len()));
    }
    spans
        .into_iter()
        .filter_map(|(s, e)| {
            let span = (start + s, start + e);
            let (cs, ce) = statement_code_range(sql, span)?;
            Some((span, normalize_code(&sql[cs..ce])))
        })
        .collect()
}

fn normalize_code(code: &str) -> String {
    code.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Index of the statement that produced result `slot` among `codes` (each a
/// statement's code, in order). The statements that return rows are mapped
/// one per result when their count matches; failing that, one statement
/// per result when the counts match AND the slot's statement can return
/// rows (a procedure call that produced two sets next to an `UPDATE` must
/// not pin the second set on the `UPDATE`). A lone statement produced
/// everything. `None` otherwise — guessing would comment out the wrong
/// statement.
fn producing_statement(codes: &[&str], result_count: usize, slot: usize) -> Option<usize> {
    if slot >= result_count {
        return None;
    }
    if codes.len() == 1 {
        return Some(0);
    }
    let producing: Vec<usize> = (0..codes.len())
        .filter(|&i| statement_returns_rows(codes[i]))
        .collect();
    if producing.len() == result_count {
        return Some(producing[slot]);
    }
    (codes.len() == result_count && statement_returns_rows(codes[slot])).then_some(slot)
}

/// What the host knows about the run that produced the results.
#[derive(Debug, Clone)]
pub struct RunContext<'a> {
    /// The buffer span that ran, as recorded at dispatch (and carried across
    /// this module's own edits). `None` falls back to the whole buffer.
    pub span: Option<(usize, usize)>,
    /// The SQL text that actually executed, when known.
    pub run_sql: Option<&'a str>,
    pub result_count: usize,
    pub dialect: Dialect,
}

impl RunContext<'_> {
    fn region(&self, buffer: &str) -> (usize, usize) {
        match self.span {
            Some((s, e))
                if s <= e
                    && e <= buffer.len()
                    && buffer.is_char_boundary(s)
                    && buffer.is_char_boundary(e) =>
            {
                (s, e)
            }
            _ => (0, buffer.len()),
        }
    }
}

/// Span of the buffer statement that produced result `slot`.
///
/// The executed text is split first (it is what the engine numbered result
/// sets against) and the matching statement is then found in the buffer by
/// its code, so earlier edits — a directive inserted above, a sibling
/// statement already commented out — don't throw the mapping off. When the
/// executed text is unknown or no longer matches (e.g. parameters were
/// substituted), the buffer region is mapped directly.
pub fn result_statement_span(
    buffer: &str,
    ctx: &RunContext,
    slot: usize,
) -> Option<(usize, usize)> {
    let region = ctx.region(buffer);
    let in_buffer = region_statements(buffer, region, ctx.dialect);
    if let Some(run_sql) = ctx.run_sql {
        let ran = region_statements(run_sql, (0, run_sql.len()), ctx.dialect);
        let codes: Vec<&str> = ran.iter().map(|(_, c)| c.as_str()).collect();
        if let Some(k) = producing_statement(&codes, ctx.result_count, slot) {
            let target = &ran[k].1;
            let ordinal = ran[..k].iter().filter(|(_, c)| c == target).count();
            if let Some((span, _)) = in_buffer.iter().filter(|(_, c)| c == target).nth(ordinal) {
                return Some(*span);
            }
        }
    }
    let codes: Vec<&str> = in_buffer.iter().map(|(_, c)| c.as_str()).collect();
    producing_statement(&codes, ctx.result_count, slot).map(|k| in_buffer[k].0)
}

/// One request from the result-name menu or the Result Settings dialog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultSettingsChange {
    /// 0-based result slot.
    pub slot: usize,
    /// New name (written when it differs from the current directive).
    pub name: Option<String>,
    /// `Some(true)` makes the result the default; `Some(false)` drops a focus
    /// directive naming it.
    pub focus: Option<bool>,
    /// `Some(Some(rows))` allots rows; `Some(None)` drops the result's own
    /// `lines` directive.
    pub lines: Option<Option<usize>>,
    /// Comment out the statement that produced the result.
    pub comment_out: bool,
}

/// The edited buffer plus the run span carried across the edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedChange {
    pub text: String,
    pub span: Option<(usize, usize)>,
    /// Whether anything changed.
    pub changed: bool,
}

/// Apply every part of `change` to `buffer`, in an order that keeps offsets
/// valid (the statement is commented out first, then the directives are
/// written against the result). Errors carry a user-facing reason.
pub fn apply_result_settings(
    buffer: &str,
    ctx: &RunContext,
    change: &ResultSettingsChange,
) -> Result<AppliedChange, String> {
    let mut text = buffer.to_string();
    let mut span = ctx.span;
    let mut changed = false;
    // Bytes of directives inserted at the run's start. While the edits are
    // applied the span moves past each one, so the next directive lands
    // below it (name, focus, lines, top to bottom); at the end the span is
    // pulled back over them, because directives written for the run belong
    // to it — a later edit must find and rewrite them, not add another.
    let mut prefix = 0usize;
    let mut apply =
        |edit: Option<TextEdit>, text: &mut String, span: &mut Option<(usize, usize)>| {
            if let Some(edit) = edit {
                if span.is_some_and(|s| edit.range.is_empty() && edit.range.start == s.0) {
                    prefix += edit.replacement.len();
                }
                *span = span.map(|s| edit.shift_span(s));
                *text = edit.apply(text);
                changed = true;
            }
        };

    if change.comment_out {
        let target = result_statement_span(&text, ctx, change.slot).ok_or_else(|| {
            crate::localization::format(
                "results.result_settings.unknown_statement_error",
                &[("result_number", &(change.slot + 1).to_string())],
            )
        })?;
        let edit = comment_out_edit(&text, target).ok_or_else(|| {
            crate::localization::format(
                "results.result_settings.empty_statement_error",
                &[("result_number", &(change.slot + 1).to_string())],
            )
        })?;
        apply(Some(edit), &mut text, &mut span);
    }
    // Directives are honored from the executed text, so only the run region
    // is read and written (the span as carried across the edits so far).
    let region = |text: &str, span: Option<(usize, usize)>| {
        RunContext {
            span,
            ..ctx.clone()
        }
        .region(text)
    };
    if let Some(name) = &change.name {
        let edit = set_name_edit(&text, region(&text, span), change.slot, name);
        apply(edit, &mut text, &mut span);
    }
    match change.focus {
        Some(true) => {
            let edit = set_focus_edit(&text, region(&text, span), change.slot);
            apply(edit, &mut text, &mut span);
        }
        Some(false) => {
            let edit = clear_focus_edit(&text, region(&text, span), change.slot);
            apply(edit, &mut text, &mut span);
        }
        None => {}
    }
    match change.lines {
        Some(Some(rows)) => {
            let edit = set_lines_edit(&text, region(&text, span), change.slot, rows);
            apply(edit, &mut text, &mut span);
        }
        Some(None) => {
            let edit = clear_lines_edit(&text, region(&text, span), change.slot);
            apply(edit, &mut text, &mut span);
        }
        None => {}
    }
    let span = span.map(|(s, e)| (s.saturating_sub(prefix), e));
    Ok(AppliedChange {
        text,
        span,
        changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- visible rows ------------------------------------------------------

    #[test]
    fn visible_rows_counts_whole_rows() {
        assert_eq!(visible_row_count(0.0, 220.0, 22.0, 100), 10);
        assert_eq!(visible_row_count(0.0, 220.0, 22.0, 4), 4, "capped by rows");
        assert_eq!(visible_row_count(0.0, 0.0, 22.0, 100), 0);
        assert_eq!(visible_row_count(0.0, 220.0, 0.0, 100), 0);
        assert_eq!(visible_row_count(0.0, 220.0, 22.0, 0), 0);
    }

    #[test]
    fn partial_bottom_row_counts_only_past_a_quarter() {
        // 10 full rows plus 30% of the 11th → 11.
        assert_eq!(visible_row_count(0.0, 220.0 + 6.6, 22.0, 100), 11);
        // Exactly 25% of the 11th → still 10.
        assert_eq!(visible_row_count(0.0, 220.0 + 5.5, 22.0, 100), 10);
        // 20% → 10.
        assert_eq!(visible_row_count(0.0, 220.0 + 4.4, 22.0, 100), 10);
        // 26% → 11.
        assert_eq!(visible_row_count(0.0, 220.0 + 5.72, 22.0, 100), 11);
    }

    #[test]
    fn partial_top_row_follows_the_same_rule() {
        // Scrolled 80% into row 0: 20% of it shows (not counted); viewport
        // then covers rows 1..=9 fully and 80% of row 10 (counted).
        assert_eq!(visible_row_count(17.6, 220.0, 22.0, 100), 10);
        // Scrolled 70% into row 0: 30% shows (counted), rows 1..=9 full, 70%
        // of row 10 (counted) → 11.
        assert_eq!(visible_row_count(15.4, 220.0, 22.0, 100), 11);
        // Scrolled exactly 75% into row 0: 25% shows → not counted.
        assert_eq!(visible_row_count(16.5, 220.0, 22.0, 100), 10);
    }

    // -- directive edits ---------------------------------------------------

    fn applied(sql: &str, edit: Option<TextEdit>) -> String {
        edit.map(|e| e.apply(sql))
            .unwrap_or_else(|| sql.to_string())
    }

    /// The run region of a whole-buffer run.
    fn whole(sql: &str) -> (usize, usize) {
        (0, sql.len())
    }

    #[test]
    fn directive_lines_round_trip_through_the_parser() {
        for slot in [0usize, 1, 6] {
            let focus = focus_directive_line(slot);
            assert_eq!(
                crate::result_layout::parse_layout_directives(&format!("{focus}\nSELECT 1;"))
                    .focus_slot(),
                Some(slot)
            );
            let lines = lines_directive_line(slot, 12);
            let parsed =
                crate::result_layout::parse_layout_directives(&format!("{lines}\nSELECT 1;"));
            assert_eq!(parsed.lines_for(slot), Some(12));
            assert!(
                crate::sqlly_directive::directive_diagnostics(&format!(
                    "{focus}\n{lines}\nSELECT 1;"
                ))
                .is_empty()
            );
        }
    }

    #[test]
    fn focus_is_inserted_at_the_top_then_updated_in_place() {
        let sql = "SELECT 1;\nSELECT 2;";
        let once = applied(sql, set_focus_edit(sql, whole(sql), 1));
        assert_eq!(once, "-- sqlly focus: 2\nSELECT 1;\nSELECT 2;");
        let twice = applied(&once, set_focus_edit(&once, whole(&once), 0));
        assert_eq!(twice, "-- sqlly focus: 1\nSELECT 1;\nSELECT 2;");
        assert!(
            set_focus_edit(&twice, whole(&twice), 0).is_none(),
            "already the default"
        );
        let cleared = applied(&twice, clear_focus_edit(&twice, whole(&twice), 0));
        assert_eq!(cleared, sql);
        assert!(clear_focus_edit(&cleared, whole(&cleared), 0).is_none());
    }

    #[test]
    fn focus_rewrites_the_winning_directive_and_keeps_indent() {
        let sql = "-- sqlly focus: 1\nSELECT 1;\n  --sqlly focus: 3\nSELECT 2;";
        let out = applied(sql, set_focus_edit(sql, whole(sql), 1));
        assert_eq!(
            out,
            "-- sqlly focus: 1\nSELECT 1;\n  -- sqlly focus: 2\nSELECT 2;"
        );
        assert_eq!(
            crate::result_layout::parse_layout_directives(&out).focus_slot(),
            Some(1)
        );
    }

    #[test]
    fn directives_go_below_a_first_line_view_or_canvas_directive() {
        let sql = "-- sqlly view: pivot\nSELECT 1;";
        assert_eq!(
            applied(sql, set_focus_edit(sql, whole(sql), 0)),
            "-- sqlly view: pivot\n-- sqlly focus: 1\nSELECT 1;"
        );
        let sql = "\n-- sqlly 2x2\nSELECT 1;";
        assert_eq!(
            applied(sql, set_lines_edit(sql, whole(sql), 0, 5)),
            "\n-- sqlly 2x2\n-- sqlly lines 1: 5\nSELECT 1;"
        );
        // A run region that starts mid-buffer gets the directive at its top.
        let sql = "SELECT 0;\nSELECT 1;";
        assert_eq!(
            applied(sql, set_focus_edit(sql, (10, sql.len()), 0)),
            "SELECT 0;\n-- sqlly focus: 1\nSELECT 1;"
        );
        assert_eq!(
            applied("", set_focus_edit("", whole(""), 0)),
            "-- sqlly focus: 1\n"
        );
    }

    #[test]
    fn lines_insert_update_and_clear_only_touch_their_result() {
        let sql = "-- sqlly lines: 8\n-- sqlly lines 1: 4\nSELECT 1;\nSELECT 2;";
        let out = applied(sql, set_lines_edit(sql, whole(sql), 1, 15));
        assert_eq!(
            out,
            "-- sqlly lines 2: 15\n-- sqlly lines: 8\n-- sqlly lines 1: 4\nSELECT 1;\nSELECT 2;"
        );
        let out = applied(&out, set_lines_edit(&out, whole(&out), 0, 9));
        assert!(out.contains("-- sqlly lines 1: 9\n"), "{out}");
        assert!(set_lines_edit(&out, whole(&out), 0, 9).is_none());
        let d = crate::result_layout::parse_layout_directives(&out);
        assert_eq!(d.lines_for(0), Some(9));
        assert_eq!(d.lines_for(1), Some(15));
        assert_eq!(d.lines_for(2), Some(8));
        let cleared = applied(&out, clear_lines_edit(&out, whole(&out), 1));
        assert_eq!(
            cleared,
            "-- sqlly lines: 8\n-- sqlly lines 1: 9\nSELECT 1;\nSELECT 2;"
        );
        // Clamped to the parser's accepted range.
        let big = applied(
            "SELECT 1;",
            set_lines_edit("SELECT 1;", whole("SELECT 1;"), 0, 100_000),
        );
        assert!(big.starts_with(&format!(
            "-- sqlly lines 1: {}\n",
            crate::result_layout::MAX_LINES
        )));
    }

    #[test]
    fn clearing_the_last_line_eats_the_preceding_newline() {
        let sql = "SELECT 1;\n-- sqlly focus: 1";
        assert_eq!(
            applied(sql, clear_focus_edit(sql, whole(sql), 0)),
            "SELECT 1;"
        );
    }

    #[test]
    fn clearing_keeps_unrelated_lines_between_targets() {
        let sql = "-- sqlly focus: 1\nSELECT 1;\n-- sqlly focus: 1";
        assert_eq!(
            applied(sql, clear_focus_edit(sql, whole(sql), 0)),
            "SELECT 1;"
        );
        let sql = "X\n-- sqlly lines 1: 3\nSELECT 1;\n-- sqlly lines 1: 4\nY";
        assert_eq!(
            applied(sql, clear_lines_edit(sql, whole(sql), 0)),
            "X\nSELECT 1;\nY"
        );
    }

    #[test]
    fn name_edit_replaces_the_text_or_inserts() {
        let sql = "-- sqlly name 1: Old\nSELECT 1;";
        assert_eq!(
            applied(sql, set_name_edit(sql, whole(sql), 0, " New ")),
            "-- sqlly name 1: New\nSELECT 1;"
        );
        assert!(set_name_edit(sql, whole(sql), 0, "Old").is_none());
        assert!(set_name_edit(sql, whole(sql), 0, "  ").is_none());
        assert_eq!(
            applied(sql, set_name_edit(sql, whole(sql), 1, "Second")),
            "-- sqlly name 2: Second\n-- sqlly name 1: Old\nSELECT 1;"
        );
    }

    /// Directives are honored from the executed text, so when only a
    /// selection ran, a `focus` line above it is not the one in force: the
    /// new directive goes at the selection's start and the old line stays.
    #[test]
    fn directives_outside_the_executed_selection_are_not_rewritten() {
        let buffer = "-- sqlly focus: 1\nSELECT 1;\nSELECT 2;\nSELECT 3;";
        let start = buffer.find("SELECT 2").unwrap();
        let region = (start, buffer.len());
        assert_eq!(
            applied(buffer, set_focus_edit(buffer, region, 1)),
            "-- sqlly focus: 1\nSELECT 1;\n-- sqlly focus: 2\nSELECT 2;\nSELECT 3;"
        );
        // Nothing in the selection names result 1, so there is nothing to clear.
        assert!(clear_focus_edit(buffer, region, 0).is_none());
        // Same for `lines` and `name`: the lines above the selection are
        // someone else's.
        let buffer = "-- sqlly lines 1: 4\n-- sqlly name 1: Top\nSELECT 1;\nSELECT 2;";
        let start = buffer.find("SELECT 2").unwrap();
        let region = (start, buffer.len());
        assert_eq!(
            applied(buffer, set_lines_edit(buffer, region, 0, 9)),
            "-- sqlly lines 1: 4\n-- sqlly name 1: Top\nSELECT 1;\n-- sqlly lines 1: 9\nSELECT 2;"
        );
        assert_eq!(
            applied(buffer, set_name_edit(buffer, region, 0, "Top")),
            "-- sqlly lines 1: 4\n-- sqlly name 1: Top\nSELECT 1;\n-- sqlly name 1: Top\nSELECT 2;"
        );
        assert!(clear_lines_edit(buffer, region, 0).is_none());

        // Through the host path: the selection's span is carried and now
        // holds the directive, so applying the same change again is a no-op
        // instead of a second copy.
        let buffer = "-- sqlly focus: 1\nSELECT 1;\nSELECT 2;\nSELECT 3;";
        let start = buffer.find("SELECT 2").unwrap();
        let ctx = RunContext {
            span: Some((start, buffer.len())),
            run_sql: Some("SELECT 2;\nSELECT 3;"),
            result_count: 2,
            dialect: Dialect::Postgres,
        };
        let change = ResultSettingsChange {
            slot: 1,
            focus: Some(true),
            ..Default::default()
        };
        let out = apply_result_settings(buffer, &ctx, &change).unwrap();
        assert_eq!(
            out.text,
            "-- sqlly focus: 1\nSELECT 1;\n-- sqlly focus: 2\nSELECT 2;\nSELECT 3;"
        );
        let span = out.span.unwrap();
        assert_eq!(
            &out.text[span.0..span.1],
            "-- sqlly focus: 2\nSELECT 2;\nSELECT 3;"
        );
        let again = apply_result_settings(
            &out.text,
            &RunContext {
                span: out.span,
                ..ctx
            },
            &change,
        )
        .unwrap();
        assert!(!again.changed);
    }

    #[test]
    fn shift_span_moves_and_stretches() {
        let insert = TextEdit {
            range: 0..0,
            replacement: "abc\n".into(),
        };
        assert_eq!(insert.shift_span((0, 10)), (4, 14));
        let inside = TextEdit {
            range: 5..6,
            replacement: "xyz".into(),
        };
        assert_eq!(inside.shift_span((2, 10)), (2, 12));
        let after = TextEdit {
            range: 20..20,
            replacement: "x".into(),
        };
        assert_eq!(after.shift_span((2, 10)), (2, 10));
    }

    // -- commenting out ----------------------------------------------------

    fn ctx(sql: &str, results: usize, dialect: Dialect) -> RunContext<'_> {
        RunContext {
            span: Some((0, sql.len())),
            run_sql: Some(sql),
            result_count: results,
            dialect,
        }
    }

    fn comment_out(sql: &str, results: usize, slot: usize, dialect: Dialect) -> String {
        let change = ResultSettingsChange {
            slot,
            comment_out: true,
            ..Default::default()
        };
        apply_result_settings(sql, &ctx(sql, results, dialect), &change)
            .expect("maps")
            .text
    }

    #[test]
    fn comments_out_one_semicolon_statement_among_several() {
        let sql = "SELECT 1;\nSELECT a,\n       b\nFROM t;\nSELECT 3;";
        assert_eq!(
            comment_out(sql, 3, 1, Dialect::Postgres),
            "SELECT 1;\n-- SELECT a,\n--        b\n-- FROM t;\nSELECT 3;"
        );
        assert_eq!(
            comment_out(sql, 3, 0, Dialect::Postgres),
            "-- SELECT 1;\nSELECT a,\n       b\nFROM t;\nSELECT 3;"
        );
        assert_eq!(
            comment_out(sql, 3, 2, Dialect::Postgres),
            "SELECT 1;\nSELECT a,\n       b\nFROM t;\n-- SELECT 3;"
        );
    }

    #[test]
    fn statements_sharing_a_line_are_split_before_commenting() {
        let sql = "SELECT 1; SELECT 2; SELECT 3;";
        assert_eq!(
            comment_out(sql, 3, 1, Dialect::MySql),
            "SELECT 1;\n-- SELECT 2;\nSELECT 3;"
        );
    }

    #[test]
    fn comments_out_one_go_separated_batch() {
        let sql = "SELECT 1\nGO\n  SELECT name\n  FROM sys.tables\nGO\nSELECT 3\nGO\n";
        let out = comment_out(sql, 3, 1, Dialect::SqlServer);
        assert_eq!(
            out,
            "SELECT 1\nGO\n  -- SELECT name\n  -- FROM sys.tables\nGO\nSELECT 3\nGO\n"
        );
    }

    #[test]
    fn leading_directives_and_blank_lines_are_left_alone() {
        let sql = "SELECT 1;\n\n-- sqlly name 2: Orders\nSELECT *\n\n  FROM orders;\n";
        assert_eq!(
            comment_out(sql, 2, 1, Dialect::Sqlite),
            "SELECT 1;\n\n-- sqlly name 2: Orders\n-- SELECT *\n\n--   FROM orders;\n"
        );
    }

    #[test]
    fn non_row_statements_are_skipped_when_counts_differ() {
        let sql = "INSERT INTO t VALUES (1);\nSELECT * FROM t;\nUPDATE t SET a = 2;\nSELECT 2;";
        assert_eq!(
            comment_out(sql, 2, 1, Dialect::Postgres),
            "INSERT INTO t VALUES (1);\nSELECT * FROM t;\nUPDATE t SET a = 2;\n-- SELECT 2;"
        );
    }

    /// `EXEC dbo.report` produced both result sets; the `UPDATE` produced
    /// none. With two statements and two results, "Comment Out Results 2"
    /// must not land on the `UPDATE` — and Results 1 still maps to the EXEC.
    #[test]
    fn one_to_one_mapping_needs_a_row_returning_statement() {
        let sql = "EXEC dbo.report;\nUPDATE t SET a = 1;";
        let ctx = ctx(sql, 2, Dialect::SqlServer);
        assert_eq!(result_statement_span(sql, &ctx, 1), None);
        let change = ResultSettingsChange {
            slot: 1,
            comment_out: true,
            ..Default::default()
        };
        let err = apply_result_settings(sql, &ctx, &change).unwrap_err();
        assert!(err.contains("comment it out by hand"), "{err}");
        assert_eq!(
            comment_out(sql, 2, 0, Dialect::SqlServer),
            "-- EXEC dbo.report;\nUPDATE t SET a = 1;"
        );
        // A lone statement produced everything, whatever it looks like.
        assert_eq!(producing_statement(&["DBCC CHECKDB"], 1, 0), Some(0));
        assert_eq!(producing_statement(&["EXEC dbo.report"], 2, 1), Some(0));
        // Two plain SELECTs and two results still map one to one.
        assert_eq!(
            producing_statement(&["SELECT 1", "SELECT 2"], 2, 1),
            Some(1)
        );
    }

    /// A block comment opened on the statement's last code line runs on to
    /// later lines; those lines are part of the statement, or commenting
    /// it out would leave the closing `*/` behind as a syntax error.
    #[test]
    fn block_comment_opened_on_the_last_code_line_is_commented_with_it() {
        let sql = "SELECT 1 /* note\n   more */\nSELECT 2";
        assert_eq!(
            comment_out(sql, 2, 0, Dialect::Postgres),
            "-- SELECT 1 /* note\n--    more */\nSELECT 2"
        );
        assert_eq!(
            comment_out(sql, 2, 1, Dialect::Postgres),
            "SELECT 1 /* note\n   more */\n-- SELECT 2"
        );
        // Directly: the range reaches the end of the closing line.
        let range = statement_code_range(sql, (0, sql.find("SELECT 2").unwrap())).unwrap();
        assert_eq!(&sql[range.0..range.1], "SELECT 1 /* note\n   more */");
        // A block comment on its own lines after the statement is not code.
        let sql = "SELECT 1\n/* trailing\n */\nSELECT 2";
        let range = statement_code_range(sql, (0, sql.find("SELECT 2").unwrap())).unwrap();
        assert_eq!(&sql[range.0..range.1], "SELECT 1");
        // Code after the block closes still counts, as before.
        let sql = "SELECT 1 /* a\n b */ FROM t;";
        let range = statement_code_range(sql, (0, sql.len())).unwrap();
        assert_eq!(&sql[range.0..range.1], sql);
    }

    #[test]
    fn mapping_survives_earlier_edits_and_refuses_to_guess() {
        let run = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        // A directive was inserted above and statement 1 already commented
        // out: result 3 still maps to `SELECT 3;`.
        let buffer = "-- sqlly focus: 2\n-- SELECT 1;\nSELECT 2;\nSELECT 3;";
        let ctx = RunContext {
            span: Some((18, buffer.len())),
            run_sql: Some(run),
            result_count: 3,
            dialect: Dialect::Postgres,
        };
        let span = result_statement_span(buffer, &ctx, 2).expect("maps");
        assert_eq!(buffer[span.0..span.1].trim(), "SELECT 3;");
        // Two results but three row-returning statements and no run text to
        // go on → refuse.
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let ctx = RunContext {
            span: None,
            run_sql: None,
            result_count: 2,
            dialect: Dialect::Postgres,
        };
        assert!(result_statement_span(sql, &ctx, 0).is_none());
        let change = ResultSettingsChange {
            slot: 0,
            comment_out: true,
            ..Default::default()
        };
        assert!(apply_result_settings(sql, &ctx, &change).is_err());
    }

    #[test]
    fn combined_change_is_one_buffer_and_tracks_the_run_span() {
        let sql = "SELECT 1;\nSELECT 2;";
        let change = ResultSettingsChange {
            slot: 1,
            name: Some("Second".into()),
            focus: Some(true),
            lines: Some(Some(7)),
            comment_out: false,
        };
        let out = apply_result_settings(sql, &ctx(sql, 2, Dialect::Postgres), &change).unwrap();
        assert!(out.changed);
        assert_eq!(
            out.text,
            "-- sqlly name 2: Second\n-- sqlly focus: 2\n-- sqlly lines 2: 7\nSELECT 1;\nSELECT 2;"
        );
        // The directives were written for this run, so the carried span
        // covers them too.
        let span = out.span.unwrap();
        assert_eq!(&out.text[span.0..span.1], out.text);
        // Nothing to do → unchanged.
        let again = apply_result_settings(
            &out.text,
            &RunContext {
                span: out.span,
                ..ctx(sql, 2, Dialect::Postgres)
            },
            &change,
        )
        .unwrap();
        assert!(!again.changed);
    }

    #[test]
    fn returns_rows_heuristic() {
        assert!(statement_returns_rows("select * from t"));
        assert!(statement_returns_rows("(SELECT 1) UNION (SELECT 2)"));
        assert!(statement_returns_rows(
            "WITH x AS (SELECT 1) SELECT * FROM x"
        ));
        assert!(statement_returns_rows("EXEC dbo.p"));
        assert!(statement_returns_rows("sp_who2"));
        assert!(statement_returns_rows("DELETE FROM t RETURNING id"));
        assert!(!statement_returns_rows("SELECT a INTO #tmp FROM t"));
        assert!(!statement_returns_rows("UPDATE t SET a = 1"));
        assert!(!statement_returns_rows("CREATE TABLE t (a int)"));
    }
}
