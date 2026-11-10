use super::*;

/// Split a line's syntax runs so the byte span `start..end` renders as a
/// hyperlink: accent color plus underline. Run boundaries outside the span
/// are preserved exactly, so syntax coloring around the link is untouched.
pub(super) fn restyle_link_span(
    runs: Vec<TextRun>,
    start: usize,
    end: usize,
    font: &Font,
) -> Vec<TextRun> {
    if start >= end {
        return runs;
    }
    let link_color = Theme::accent();
    let link_run = |len: usize| TextRun {
        len,
        font: font.clone(),
        color: link_color,
        background_color: None,
        underline: Some(gpui::UnderlineStyle {
            thickness: px(1.0),
            color: Some(link_color),
            wavy: false,
        }),
        strikethrough: None,
    };

    let mut result = Vec::with_capacity(runs.len() + 2);
    let mut offset = 0usize;
    for run in runs {
        let run_start = offset;
        let run_end = offset + run.len;
        offset = run_end;

        if run_end <= start || run_start >= end {
            result.push(run);
            continue;
        }
        if run_start < start {
            let mut before = run.clone();
            before.len = start - run_start;
            result.push(before);
        }
        let overlap_start = run_start.max(start);
        let overlap_end = run_end.min(end);
        result.push(link_run(overlap_end - overlap_start));
        if run_end > end {
            let mut after = run;
            after.len = run_end - end;
            result.push(after);
        }
    }
    result
}

/// Add an underline to the byte span `start..end` of a line's runs, keeping
/// every run's own color and font (unlike `restyle_link_span`, which recolors).
/// Used for the IME composition (preedit) span.
pub(super) fn underline_span(runs: Vec<TextRun>, start: usize, end: usize) -> Vec<TextRun> {
    if start >= end {
        return runs;
    }
    let mut result = Vec::with_capacity(runs.len() + 2);
    let mut offset = 0usize;
    for run in runs {
        let run_start = offset;
        let run_end = offset + run.len;
        offset = run_end;

        if run_end <= start || run_start >= end {
            result.push(run);
            continue;
        }
        if run_start < start {
            let mut before = run.clone();
            before.len = start - run_start;
            result.push(before);
        }
        let overlap_start = run_start.max(start);
        let overlap_end = run_end.min(end);
        let mut marked = run.clone();
        marked.len = overlap_end - overlap_start;
        marked.underline = Some(gpui::UnderlineStyle {
            thickness: px(1.0),
            color: Some(marked.color),
            wavy: false,
        });
        result.push(marked);
        if run_end > end {
            let mut after = run;
            after.len = run_end - end;
            result.push(after);
        }
    }
    result
}

/// Byte offset within `text` of the UTF-16 code-unit offset `utf16` —
/// clamped to the text end, always a char boundary. Used to place the caret
/// inside IME preedit text (`new_selected_range` is relative UTF-16).
pub(super) fn utf16_to_byte_within(text: &str, utf16: usize) -> usize {
    let mut count = 0usize;
    for (i, ch) in text.char_indices() {
        if count >= utf16 {
            return i;
        }
        count += ch.len_utf16();
    }
    text.len()
}

pub(super) fn utf8_safe_edit_range(content: &str, range: Range<usize>) -> Range<usize> {
    let mut start = range.start.min(content.len());
    while start > 0 && !content.is_char_boundary(start) {
        start -= 1;
    }

    let mut end = range.end.min(content.len()).max(start);
    while end < content.len() && !content.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

/// Start of the identifier fragment immediately before `cursor`. Dots stop
/// the scan so a qualified completion replaces `Sal` in `Sales.Sal` at every
/// caret without disturbing the already-committed qualifier.
pub(super) fn word_start_before(content: &str, cursor: usize) -> usize {
    let cursor = cursor.min(content.len());
    let mut start = cursor;
    for (index, character) in content[..cursor].char_indices().rev() {
        if character.is_ascii_alphanumeric() || character == '_' {
            start = index;
        } else {
            break;
        }
    }
    start
}

// ── Word / camel-hump boundary scanning (pure, testable) ───────────────────
//
// Used by the editor's word/hump navigation and by double-click selection.
// "Word" = maximal run of alphanumeric chars (underscore is a separator, so
// snake_case identifiers split into their parts). "Hump" = a camelCase subword
// within a word run; hump boundaries are lower→Upper, the last Upper before an
// Upper→Lower transition (acronym end), and digit↔letter transitions.

/// A navigation word character: alphanumeric only. `_` is intentionally a
/// separator so `first_name` splits into `first` / `_` / `name`.
pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Byte offset of the previous word boundary to the left of `from`. Skips
/// whitespace, then one chunk of word or non-word non-whitespace chars.
pub(super) fn word_boundary_left(content: &str, from: usize) -> usize {
    let end = from.min(content.len());
    let mut iter = content[..end].char_indices().rev();
    // 1. Skip whitespace to the left.
    let mut last = end;
    while let Some((idx, ch)) = iter.next() {
        if ch.is_whitespace() {
            last = idx;
            continue;
        }
        // 2. First non-whitespace char: classify and consume its run.
        let want_word = is_word_char(ch);
        last = idx;
        for (p, c) in iter {
            if c.is_whitespace() || is_word_char(c) != want_word {
                return last;
            }
            last = p;
        }
        return last;
    }
    last
}

/// Clamp the popup scroll offset so the selected index stays inside the
/// visible window of `max_visible` items.
pub(super) fn clamp_scroll_offset(index: usize, scroll_offset: usize, max_visible: usize) -> usize {
    if index < scroll_offset {
        index
    } else if index >= scroll_offset + max_visible {
        index.saturating_sub(max_visible) + 1
    } else {
        scroll_offset
    }
}

/// Byte offset of the next word boundary to the right of `from`. Lands on the
/// start of the next token (word or punctuation run): skips the current
/// same-class chunk, then any whitespace. Symmetric with `word_boundary_left`,
/// which lands on the start of the previous token.
pub(super) fn word_boundary_right(content: &str, from: usize) -> usize {
    let start = from.min(content.len());
    let mut i = start;
    // 1. If not starting on whitespace, skip the current same-class chunk
    //    (a word run or a punctuation run) to its end.
    if let Some(c) = content[start..].chars().next()
        && !c.is_whitespace()
    {
        let want_word = is_word_char(c);
        for (p, ch) in content[start..].char_indices() {
            if !ch.is_whitespace() && is_word_char(ch) == want_word {
                i = start + p + ch.len_utf8();
            } else {
                break;
            }
        }
    }
    // 2. Skip trailing whitespace to land at the start of the next token
    //    (or the end of the content).
    let ws_start = i;
    let mut j = i;
    for (p, ch) in content[ws_start..].char_indices() {
        if ch.is_whitespace() {
            j = ws_start + p + ch.len_utf8();
        } else {
            break;
        }
    }
    i = j;
    i
}

/// Byte offset of the previous camel-hump boundary to the left of `from`.
pub(super) fn hump_boundary_left(content: &str, from: usize) -> usize {
    let end = from.min(content.len());
    let left = &content[..end];
    // 1. Skip whitespace to the left.
    let first_nonws = left.char_indices().rev().find(|(_, c)| !c.is_whitespace());
    let Some((fn_off, fn_ch)) = first_nonws else {
        return 0;
    };
    // 2. Non-word chunk: consume one punctuation run.
    if !is_word_char(fn_ch) {
        let mut last = fn_off;
        for (p, c) in left[..fn_off].char_indices().rev() {
            if c.is_whitespace() || is_word_char(c) {
                break;
            }
            last = p;
        }
        return last;
    }
    // 3. Word run: extend left to run start, and right to run end so acronym
    //    lookahead (the char at/after the cursor) is visible.
    let mut run_start = fn_off;
    for (p, c) in left[..fn_off].char_indices().rev() {
        if is_word_char(c) {
            run_start = p;
        } else {
            break;
        }
    }
    let mut run_end = end;
    for (p, c) in content[end..].char_indices() {
        if is_word_char(c) {
            run_end = end + p + c.len_utf8();
        } else {
            break;
        }
    }
    let run: Vec<(usize, char)> = content[run_start..run_end]
        .char_indices()
        .map(|(r, c)| (run_start + r, c))
        .collect();
    // 4. The destination is the rightmost hump boundary strictly left of `from`,
    //    or the run start if there is none.
    let mut dest = run_start;
    for k in 1..run.len() {
        let (_, p) = run[k - 1];
        let (c_idx, c) = run[k];
        if !is_word_char(c) {
            break;
        }
        let next_lower = run
            .get(k + 1)
            .map(|(_, n)| n.is_lowercase())
            .unwrap_or(false);
        let boundary = (p.is_lowercase() && c.is_uppercase())
            || (p.is_uppercase() && c.is_uppercase() && next_lower)
            || (p.is_ascii_digit() && c.is_alphabetic())
            || (p.is_alphabetic() && c.is_ascii_digit());
        if boundary && c_idx < from {
            dest = c_idx;
        }
    }
    dest
}

/// Byte offset of the next camel-hump boundary to the right of `from`.
pub(super) fn hump_boundary_right(content: &str, from: usize) -> usize {
    let start = from.min(content.len());
    let chars: Vec<(usize, char)> = content[start..]
        .char_indices()
        .map(|(r, c)| (start + r, c))
        .collect();
    let mut k = 0usize;
    // 1. Skip whitespace.
    while k < chars.len() && chars[k].1.is_whitespace() {
        k += 1;
    }
    if k == chars.len() {
        return content.len();
    }
    // 2. Non-word chunk: consume one punctuation run.
    if !is_word_char(chars[k].1) {
        let mut last = chars[k].0 + chars[k].1.len_utf8();
        k += 1;
        while k < chars.len() && !chars[k].1.is_whitespace() && !is_word_char(chars[k].1) {
            last = chars[k].0 + chars[k].1.len_utf8();
            k += 1;
        }
        return last;
    }
    // 3. Word run: walk right, stopping at each hump boundary.
    let mut last = chars[k].0 + chars[k].1.len_utf8();
    k += 1;
    while k < chars.len() {
        let (p, c) = (chars[k - 1].1, chars[k].1);
        let c_idx = chars[k].0;
        if !is_word_char(c) {
            break;
        }
        let next_lower = chars
            .get(k + 1)
            .map(|(_, n)| n.is_lowercase())
            .unwrap_or(false);
        let boundary = (p.is_lowercase() && c.is_uppercase())
            || (p.is_uppercase() && c.is_uppercase() && next_lower)
            || (p.is_ascii_digit() && c.is_alphabetic())
            || (p.is_alphabetic() && c.is_ascii_digit());
        if boundary {
            return c_idx;
        }
        last = c_idx + c.len_utf8();
        k += 1;
    }
    last
}

/// Pure caret-move arithmetic shared by every navigation action.
///
/// Returns the new `(cursor, selection_anchor)`:
/// - selecting (`select` true): keep/seed the anchor at the old cursor and
///   move the cursor to `target`;
/// - not selecting with an existing selection: collapse to `edge` (the near
///   edge of the prior selection — start for leftward moves, end for
///   rightward) and clear the anchor, matching the existing `move_left`/
///   `move_right` collapse behaviour;
/// - not selecting with no selection: move to `target`.
pub(super) fn move_caret(
    cursor: usize,
    anchor: Option<usize>,
    select: bool,
    target: usize,
    edge: usize,
) -> (usize, Option<usize>) {
    if select {
        (target, Some(anchor.unwrap_or(cursor)))
    } else if anchor.is_some() {
        (edge, None)
    } else {
        (target, None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CharClass {
    Word,
    Space,
    Punct,
}

pub(super) fn char_class(c: char) -> CharClass {
    if c.is_alphanumeric() {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Punct
    }
}

/// The `[start, end)` run of same-class characters containing the char at
/// offset `o`. Used by double-click word selection: clicking on a word selects
/// the word, on punctuation selects the punctuation run, on whitespace selects
/// the whitespace run. Returns an empty range at `o` when `o` is past the end.
/// A non-boundary `o` is snapped back to the previous char boundary before slicing.
pub(super) fn token_range_at(content: &str, o: usize) -> (usize, usize) {
    let n = content.len();
    let mut o = o.min(n);
    while o > 0 && !content.is_char_boundary(o) {
        o -= 1;
    }
    if o == n {
        return (n, n);
    }
    let class = char_class(content[o..].chars().next().unwrap());
    let mut start = o;
    for (p, c) in content[..o].char_indices().rev() {
        if char_class(c) == class {
            start = p;
        } else {
            break;
        }
    }
    let mut end = o;
    for (p, c) in content[o..].char_indices() {
        if char_class(c) == class {
            end = o + p + c.len_utf8();
        } else {
            break;
        }
    }
    (start, end)
}

/// The token multi-cursor matching seeds from a bare caret: the same-class
/// run under the caret (double-click semantics), preferring the run ending
/// at the caret when the caret sits just past a token, and never selecting
/// whitespace. Returns an empty range at the caret when nothing qualifies.
pub(super) fn word_selection_range(content: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(content.len());
    let non_ws = |s: usize, e: usize| {
        s < e
            && content[s..e]
                .chars()
                .next()
                .map(|c| !c.is_whitespace())
                .unwrap_or(false)
    };
    let (s, e) = token_range_at(content, offset);
    if non_ws(s, e) {
        return (s, e);
    }
    // Caret at end of buffer / on whitespace: try the char to the left.
    if offset > 0 {
        let mut p = offset - 1;
        while p > 0 && !content.is_char_boundary(p) {
            p -= 1;
        }
        let (ls, le) = token_range_at(content, p);
        if non_ws(ls, le) {
            return (ls, le);
        }
    }
    (offset, offset)
}

/// Byte offset of the first occurrence of `needle` at or after `from`,
/// wrapping to the top of `content` when nothing follows, skipping
/// occurrences whose start is in `claimed` (already carrying a caret).
/// `None` when every occurrence is claimed or `needle` is empty.
pub(super) fn find_next_unclaimed_occurrence(
    content: &str,
    needle: &str,
    from: usize,
    claimed: &[usize],
) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let from = from.min(content.len());
    let mut wrapped: Option<usize> = None;
    for (i, _) in content.match_indices(needle) {
        if claimed.contains(&i) {
            continue;
        }
        if i >= from {
            return Some(i);
        }
        if wrapped.is_none() {
            wrapped = Some(i);
        }
    }
    wrapped
}

/// Find byte ranges in `label` that case-insensitively match the characters
/// in `filter` as a subsequence. Returns contiguous `(start_byte, end_byte)`
/// ranges so the caller can build `TextRun`s that highlight matched portions
/// of the completion label.
///
/// The matching is subsequence-based (not substring): each character of
/// `filter` must appear in order somewhere in `label`, case-insensitively.
/// This mirrors the fuzzy matching the intellisense engine uses for ranking,
/// so the highlighted ranges visually correspond to why each item was offered.
pub(super) fn completion_match_ranges(filter: &str, label: &str) -> Vec<(usize, usize)> {
    if filter.is_empty() {
        return Vec::new();
    }

    let filter_chars: Vec<char> = filter.chars().collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut fi = 0;
    let mut range_start: Option<usize> = None;

    for (byte_idx, ch) in label.char_indices() {
        if fi < filter_chars.len() && ch.eq_ignore_ascii_case(&filter_chars[fi]) {
            if range_start.is_none() {
                range_start = Some(byte_idx);
            }
            fi += 1;
        } else if let Some(start) = range_start {
            ranges.push((start, byte_idx));
            range_start = None;
        }
    }

    if let Some(start) = range_start {
        ranges.push((start, label.len()));
    }

    ranges
}

/// Whether `text` opens with a statement keyword — the guard that keeps
/// automatic formatting (on paste, as you go) off expression fragments it
/// would mangle out of context.
pub(super) fn looks_like_statement(text: &str) -> bool {
    let head = text.trim_start().to_ascii_lowercase();
    [
        "select", "insert", "update", "delete", "with", "create", "alter", "drop", "merge",
        "truncate", "grant", "revoke", "begin", "declare", "exec",
    ]
    .iter()
    .any(|kw| {
        head.starts_with(kw)
            && head[kw.len()..]
                .chars()
                .next()
                .map(|c| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(true)
    })
}

/// The edit "format as you go" makes before Enter: the statement `span`
/// (trailing whitespace excluded) reformatted, and where the caret lands
/// afterwards — after the same meaningful character it followed. `None`
/// when nothing changes, when the caret is outside the statement (Enter on
/// a blank line below it must not reach back and reformat it), or when the
/// reformat does more than move layout characters — the formatter may add
/// or drop identifier quotes and parentheses (Always Quote, Remove
/// Unnecessary Quotes, parenthesize mixed AND/OR), so the caret is mapped
/// by its index among the characters that survive those options, and a
/// reformat that changes anything else is skipped rather than guessed at.
pub(super) fn format_as_you_go_edit(
    content: &str,
    span: (usize, usize),
    cursor: usize,
    prefs: &crate::preferences::SqlFormattingPreferences,
    indent: crate::sql_formatter::EditorIndent,
) -> Option<(Range<usize>, String, usize)> {
    let range = utf8_safe_edit_range(content, span.0..span.1);
    let target = content[range.clone()].trim_end();
    let range = range.start..range.start + target.len();
    if !looks_like_statement(target) || cursor < range.start || cursor > range.end {
        return None;
    }
    let formatted = crate::sql_formatter::format_sql(target, prefs, Some(indent));
    let formatted = formatted.trim_end();
    if formatted == target || formatted.is_empty() {
        return None;
    }
    if reflow_significant(formatted) != reflow_significant(target) {
        return None;
    }
    let before = reflow_significant(&content[range.start..cursor])
        .chars()
        .count();
    let mut caret = match before.checked_sub(1) {
        None => 0,
        Some(last) => {
            let at = reflow_offset(formatted, last)?;
            at + formatted[at..].chars().next()?.len_utf8()
        }
    };
    // The caret was after `t` in `from t`; with Always Quote the formatter
    // wrote `"t"`, and Enter must land after the closing quote, not inside
    // it. Same for a closing parenthesis the formatter added where the
    // statement had none (`b = 2| OR` → `b = 2)| OR`); one the user typed
    // stays ahead of the caret as it was.
    let next_typed = content[cursor..range.end]
        .chars()
        .find(|ch| !ch.is_whitespace());
    while let Some(ch) = formatted[caret..].chars().next() {
        let closing_quote = matches!(ch, '"' | ']' | '`');
        let added_paren = ch == ')' && next_typed != Some(')');
        if closing_quote || added_paren {
            caret += ch.len_utf8();
        } else {
            break;
        }
    }
    let caret = range.start + caret;
    Some((range, formatted.to_string(), caret))
}

/// Characters a reformat may add, drop, or move without changing meaning
/// for format-as-you-go caret mapping: layout plus the parentheses the
/// mixed-boolean option inserts.
fn is_reflow_char(ch: char) -> bool {
    is_layout_char(ch) || matches!(ch, '(' | ')')
}

/// Lowercased meaningful characters of `text` for caret mapping across a
/// format-as-you-go reformat (everything but [`is_reflow_char`]).
fn reflow_significant(text: &str) -> String {
    text.chars()
        .filter(|ch| !is_reflow_char(*ch))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Byte offset of the `index`-th (0-based) character of `text` that
/// [`reflow_significant`] keeps.
fn reflow_offset(text: &str, index: usize) -> Option<usize> {
    text.char_indices()
        .filter(|(_, ch)| !is_reflow_char(*ch))
        .nth(index)
        .map(|(at, _)| at)
}

/// Characters a reformat may add, drop, or move without changing meaning:
/// whitespace and identifier delimiters.
fn is_layout_char(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '[' | ']' | '"' | '`')
}

/// Lowercased meaningful characters of `text` (everything but layout).
fn significant(text: &str) -> String {
    text.chars()
        .filter(|ch| !is_layout_char(*ch))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Byte offset of the `index`-th (0-based) meaningful character of `text`.
fn significant_offset(text: &str, index: usize) -> Option<usize> {
    text.char_indices()
        .filter(|(_, ch)| !is_layout_char(*ch))
        .nth(index)
        .map(|(at, _)| at)
}

/// `insert` as the SQL formatter would lay it out at `replace_start`, so an
/// accepted completion follows the SQL formatting settings (keyword and
/// identifier case, quoting, JOIN/ON placement, …). The whole statement —
/// `statement_start` up to the insertion point, the inserted text, then
/// `rest` (the statement's text after the replaced range) — is formatted
/// together so the formatter sees what it would see on Format SQL; only the
/// inserted part is taken back, re-indented to the statement's column. `None` keeps the
/// raw text: no change, a fragment that is not a statement, or a reformat
/// that no longer lines up character for character with the input.
pub(super) fn format_insert_in_context(
    content: &str,
    statement_start: usize,
    replace_start: usize,
    insert: &str,
    rest: &str,
    prefs: &crate::preferences::SqlFormattingPreferences,
    indent: crate::sql_formatter::EditorIndent,
) -> Option<String> {
    if statement_start > replace_start
        || !content.is_char_boundary(statement_start)
        || !content.is_char_boundary(replace_start)
    {
        return None;
    }
    let core = insert.trim();
    if core.is_empty() {
        return None;
    }
    let prefix = &content[statement_start..replace_start];
    let mut source = String::with_capacity(prefix.len() + core.len() + rest.len() + 1);
    source.push_str(prefix);
    source.push_str(core);
    if !rest.is_empty() {
        // Keep the inserted text's own separation from what follows.
        if insert.ends_with(char::is_whitespace) && !rest.starts_with(char::is_whitespace) {
            source.push(' ');
        }
        source.push_str(rest);
    }
    if !looks_like_statement(&source) {
        return None;
    }
    let formatted = crate::sql_formatter::format_sql(&source, prefs, Some(indent));
    if significant(&formatted) != significant(&source) {
        return None;
    }
    let skip = significant(prefix).chars().count();
    let take = significant(core).chars().count();
    if take == 0 {
        return None;
    }
    let mut start = significant_offset(&formatted, skip)?;
    let last = significant_offset(&formatted, skip + take - 1)?;
    let mut end = last + formatted[last..].chars().next()?.len_utf8();
    // Pull in delimiters hugging the inserted name (`[Orders]`).
    while let Some(ch) = formatted[..start].chars().next_back()
        && matches!(ch, '[' | '"' | '`')
    {
        start -= ch.len_utf8();
    }
    while let Some(ch) = formatted[end..].chars().next()
        && matches!(ch, ']' | '"' | '`')
    {
        end += ch.len_utf8();
    }
    let slice = &formatted[start..end];
    // The formatter lays the statement out from column 0; carry the
    // statement's own indentation onto every line break.
    let line_start = content[..statement_start].rfind('\n').map_or(0, |i| i + 1);
    let lead: String = content[line_start..statement_start]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect();
    let body = if lead.is_empty() {
        slice.to_string()
    } else {
        slice.replace('\n', &format!("\n{lead}"))
    };
    let leading = &insert[..insert.len() - insert.trim_start().len()];
    let trailing = &insert[insert.trim_end().len()..];
    let result = format!("{leading}{body}{trailing}");
    (result != insert).then_some(result)
}
