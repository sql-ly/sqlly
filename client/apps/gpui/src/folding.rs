//! Code folding for the SQL editor: pure fold-region discovery plus the
//! display-line map that hides collapsed ranges.
//!
//! Folding is strictly a *display* concern. The buffer, cursor offsets,
//! selection, undo history, and find/replace all keep operating on the full
//! text; collapsing a region only removes its interior lines from layout and
//! paint. A collapsed region keeps its first line (the header) visible and
//! hides `start_line + 1 ..= end_line`.
//!
//! Region discovery is a single heuristic scan over the text — not a full
//! parse. It understands enough SQL to stay out of strings, quoted/bracketed
//! identifiers, `--` line comments, and (nested) `/* */` block comments, and
//! produces regions for:
//!
//! - `BEGIN … END` blocks (including `BEGIN TRY` / `BEGIN CATCH`), while
//!   excluding `BEGIN TRAN[SACTION]` / `BEGIN DISTRIBUTED …` /
//!   `BEGIN DIALOG …`, which are statements with no matching `END`
//! - `CASE … END` expressions
//! - multi-line parenthesized groups (subqueries, `IN (…)` lists, CTE bodies)
//! - multi-line `/* … */` comments (an unclosed one folds to the last line)
//! - `GO` batches (the region between consecutive `GO` separator lines)
//!
//! Known simplification: collapsed folds are keyed by their header line, so
//! an edit that inserts or removes lines *above* a collapsed fold silently
//! reopens it (the recompute can no longer match the stored key). Edits below
//! a fold, and edits that keep the line structure, preserve collapsed state.

use std::collections::BTreeSet;

/// Buffers larger than this skip fold-region discovery entirely (the scan is
/// O(text) and runs once per edit). Half a megabyte of SQL is far beyond any
/// hand-edited script; giant generated scripts just lose the fold gutter.
pub const FOLD_SCAN_BYTE_CAP: usize = 512 * 1024;

/// What kind of construct a fold region collapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    /// `BEGIN … END` (blocks, TRY/CATCH arms).
    Block,
    /// `CASE … END` expression.
    Case,
    /// Multi-line parenthesized group.
    Paren,
    /// Multi-line `/* … */` comment.
    Comment,
    /// A `GO`-delimited batch.
    Batch,
}

/// A foldable span of whole lines. Collapsing hides
/// `start_line + 1 ..= end_line`; the header line stays visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRegion {
    pub start_line: usize,
    pub end_line: usize,
    pub kind: FoldKind,
}

/// Scanner state for one pass over the text.
struct Scanner {
    line: usize,
    /// `BEGIN`/`CASE` nesting, each with its opening line.
    block_stack: Vec<(FoldKind, usize)>,
    /// Opening line of each unclosed `(`.
    paren_stack: Vec<usize>,
    /// `/* */` nesting depth (T-SQL block comments nest) and the line the
    /// outermost one opened on.
    comment_depth: usize,
    comment_start_line: usize,
    /// A `BEGIN` waiting to learn whether the next word makes it a statement
    /// (`TRAN`, `TRANSACTION`, `DISTRIBUTED`, `DIALOG`) instead of a block.
    pending_begin: Option<usize>,
    regions: Vec<FoldRegion>,
}

impl Scanner {
    fn push_region(&mut self, start_line: usize, end_line: usize, kind: FoldKind) {
        if end_line > start_line {
            self.regions.push(FoldRegion {
                start_line,
                end_line,
                kind,
            });
        }
    }

    /// A word seen outside strings and comments.
    fn word(&mut self, w: &str) {
        // Resolve a pending BEGIN first: statement forms never open a block.
        if let Some(begin_line) = self.pending_begin.take() {
            if matches!(
                w,
                "TRAN" | "TRANSACTION" | "DISTRIBUTED" | "DIALOG" | "CONVERSATION"
            ) {
                // `BEGIN TRAN…` etc — a statement, not a block; fall through
                // so the discarded word still gets normal processing below
                // (none of these open or close anything themselves).
            } else {
                self.block_stack.push((FoldKind::Block, begin_line));
            }
        }
        match w {
            "BEGIN" => self.pending_begin = Some(self.line),
            "CASE" => self.block_stack.push((FoldKind::Case, self.line)),
            "END" => {
                if let Some((kind, start)) = self.block_stack.pop() {
                    self.push_region(start, self.line, kind);
                }
            }
            _ => {}
        }
    }
}

/// Discover fold regions in `text`. Pure and allocation-light; call once per
/// edit (revision-keyed by the caller), not per frame.
pub fn compute_fold_regions(text: &str) -> Vec<FoldRegion> {
    if text.len() > FOLD_SCAN_BYTE_CAP {
        return Vec::new();
    }
    let mut s = Scanner {
        line: 0,
        block_stack: Vec::new(),
        paren_stack: Vec::new(),
        comment_depth: 0,
        comment_start_line: 0,
        pending_begin: None,
        regions: Vec::new(),
    };

    let bytes = text.as_bytes();
    let mut i = 0;
    let mut word_start: Option<usize> = None;
    let mut in_line_comment = false;
    // String/identifier states. SQL string literals may span lines.
    let mut in_single = false; // '…'
    let mut in_double = false; // "…"
    let mut in_bracket = false; // […]
    let mut in_backtick = false; // `…`
    let mut line_start = 0usize;
    // Whether the current line began outside comments/strings — required
    // before its text may count as a `GO` separator.
    let mut line_started_clean = true;
    let mut go_lines: Vec<usize> = Vec::new();

    macro_rules! flush_word {
        ($s:expr, $end:expr) => {
            if let Some(ws) = word_start.take() {
                let raw = &text[ws..$end];
                // Uppercase only short candidate words; anything longer than
                // "TRANSACTION" can't be a keyword we care about.
                if raw.len() <= 11 {
                    let upper = raw.to_ascii_uppercase();
                    $s.word(&upper);
                }
            }
        };
    }

    while i < bytes.len() {
        let b = bytes[i];
        // Line bookkeeping first: newline resets line-scoped states.
        if b == b'\n' {
            flush_word!(s, i);
            if line_started_clean
                && !in_single
                && !in_double
                && !in_bracket
                && !in_backtick
                && is_go_separator(&text[line_start..i])
            {
                go_lines.push(s.line);
            }
            in_line_comment = false;
            s.line += 1;
            i += 1;
            line_start = i;
            line_started_clean =
                s.comment_depth == 0 && !in_single && !in_double && !in_bracket && !in_backtick;
            continue;
        }
        if in_line_comment {
            i += 1;
            continue;
        }
        if s.comment_depth > 0 {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                s.comment_depth -= 1;
                if s.comment_depth == 0 {
                    s.push_region(s.comment_start_line, s.line, FoldKind::Comment);
                }
                i += 2;
                continue;
            }
            if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                s.comment_depth += 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_single {
            if b == b'\'' {
                // '' is an escaped quote inside the literal.
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if in_bracket {
            if b == b']' {
                in_bracket = false;
            }
            i += 1;
            continue;
        }
        if in_backtick {
            if b == b'`' {
                in_backtick = false;
            }
            i += 1;
            continue;
        }

        // Plain code context.
        match b {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                flush_word!(s, i);
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                flush_word!(s, i);
                s.comment_depth = 1;
                s.comment_start_line = s.line;
                i += 2;
            }
            b'\'' => {
                flush_word!(s, i);
                in_single = true;
                i += 1;
            }
            b'"' => {
                flush_word!(s, i);
                in_double = true;
                i += 1;
            }
            b'[' => {
                flush_word!(s, i);
                in_bracket = true;
                i += 1;
            }
            b'`' => {
                flush_word!(s, i);
                in_backtick = true;
                i += 1;
            }
            b'(' => {
                flush_word!(s, i);
                s.paren_stack.push(s.line);
                i += 1;
            }
            b')' => {
                flush_word!(s, i);
                if let Some(start) = s.paren_stack.pop() {
                    s.push_region(start, s.line, FoldKind::Paren);
                }
                i += 1;
            }
            _ if b.is_ascii_alphanumeric() || b == b'_' => {
                if word_start.is_none() {
                    word_start = Some(i);
                }
                i += 1;
            }
            _ => {
                flush_word!(s, i);
                i += 1;
            }
        }
    }
    flush_word!(s, bytes.len());
    // Final line (no trailing newline).
    if line_started_clean
        && !in_single
        && !in_double
        && !in_bracket
        && !in_backtick
        && s.comment_depth == 0
        && is_go_separator(&text[line_start..])
    {
        go_lines.push(s.line);
    }
    // An unclosed block comment folds to the end of the buffer — that is
    // exactly when collapsing the runaway comment is most useful.
    if s.comment_depth > 0 {
        s.push_region(s.comment_start_line, s.line, FoldKind::Comment);
    }
    // GO batches: a region per batch that spans more than one line. Only
    // produced when the script actually uses GO.
    if !go_lines.is_empty() {
        let last_line = s.line;
        let mut batch_start = 0usize;
        for &go_line in &go_lines {
            if go_line > batch_start {
                s.push_region(batch_start, go_line - 1, FoldKind::Batch);
            }
            batch_start = go_line + 1;
        }
        if last_line > batch_start {
            s.push_region(batch_start, last_line, FoldKind::Batch);
        }
    }

    // One region per header line: keep the widest so the collapse key
    // (start line) is unambiguous.
    let mut regions = s.regions;
    regions.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then(b.end_line.cmp(&a.end_line))
    });
    regions.dedup_by_key(|r| r.start_line);
    regions
}

/// The first `max` hidden lines of the collapsed region headed at `region.start_line`
/// (buffer lines `start_line + 1 ..= end_line`), tab-expanded to 4 spaces and
/// trimmed of trailing whitespace, plus how many more lines are hidden beyond the
/// preview. Empty when the region has no hidden interior.
pub fn preview_lines(text: &str, region: FoldRegion, max: usize) -> (Vec<String>, usize) {
    if region.end_line <= region.start_line {
        return (Vec::new(), 0);
    }
    let hidden_count = region.end_line - region.start_line;
    let take = max.min(hidden_count);
    let remainder = hidden_count.saturating_sub(take);
    if take == 0 {
        return (Vec::new(), remainder);
    }
    let first_hidden = region.start_line + 1;
    let mut lines = Vec::with_capacity(take);
    for (i, line) in text.lines().enumerate() {
        if i < first_hidden {
            continue;
        }
        if i > region.end_line {
            break;
        }
        let expanded = line.replace('\t', "    ");
        lines.push(expanded.trim_end().to_string());
        if lines.len() == take {
            break;
        }
    }
    (lines, remainder)
}

/// Whether a line consists solely of a `GO` batch separator (optionally
/// `GO <count>`, optionally ending in `;`), matching the forms SSMS accepts.
fn is_go_separator(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 2 {
        return false;
    }
    let Some(rest) = t
        .get(..2)
        .filter(|p| p.eq_ignore_ascii_case("go"))
        .map(|_| t[2..].trim())
    else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let rest = rest.strip_suffix(';').map(str::trim).unwrap_or(rest);
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) || rest.is_empty()
}

/// Display-line mapping for a set of collapsed regions. Hidden ranges are
/// merged and sorted; all queries are O(number of collapsed folds), which is
/// a handful in practice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldMap {
    /// Merged, sorted, inclusive ranges of hidden buffer lines.
    hidden: Vec<(usize, usize)>,
}

impl FoldMap {
    pub fn from_collapsed_regions(regions: &[(usize, usize)]) -> Self {
        let mut ranges: Vec<(usize, usize)> = regions
            .iter()
            .filter(|(s, e)| e > s)
            .map(|&(s, e)| (s + 1, e))
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (s, e) in ranges {
            match merged.last_mut() {
                // Overlapping or touching ranges collapse into one so that the
                // line before every stored range start is always visible.
                Some((_, le)) if s <= *le + 1 => *le = (*le).max(e),
                _ => merged.push((s, e)),
            }
        }
        Self { hidden: merged }
    }

    /// True when nothing is collapsed (mapping is the identity). Test-facing
    /// today; paint paths always go through the mapping functions, which are
    /// already O(1)-ish when this is true.
    #[allow(dead_code)]
    pub fn is_identity(&self) -> bool {
        self.hidden.is_empty()
    }

    pub fn hidden_line_count(&self) -> usize {
        self.hidden.iter().map(|(s, e)| e - s + 1).sum()
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        self.hidden
            .binary_search_by(|(s, e)| {
                if line < *s {
                    std::cmp::Ordering::Greater
                } else if line > *e {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    pub fn display_line_count(&self, total_lines: usize) -> usize {
        total_lines.saturating_sub(self.hidden_line_count()).max(1)
    }

    /// Display index for a buffer line. Hidden lines map to the display index
    /// of the fold header above them, so cursor-derived positions degrade to
    /// the visible header instead of pointing into hidden space.
    pub fn buffer_to_display(&self, line: usize) -> usize {
        let mut hidden_before = 0usize;
        for &(s, e) in &self.hidden {
            if e < line {
                hidden_before += e - s + 1;
                continue;
            }
            if s <= line {
                // `line` is hidden; its header is `s - 1` (always visible —
                // merged ranges never start at 0 and never touch).
                return (s - 1) - hidden_before;
            }
            break;
        }
        line - hidden_before
    }

    /// Buffer line for a display index (the nth visible line). Indexes past
    /// the end saturate into "beyond the buffer" — callers clamp against
    /// their own line count, exactly as they already do for y-overshoot.
    pub fn display_to_buffer(&self, display_line: usize) -> usize {
        let mut buffer = display_line;
        for &(s, e) in &self.hidden {
            if s <= buffer {
                buffer += e - s + 1;
            } else {
                break;
            }
        }
        buffer
    }
}

/// The editor's complete folding state: discovered regions, which of them are
/// collapsed, and the derived display map.
#[derive(Debug, Default)]
pub struct EditorFolds {
    regions: Vec<FoldRegion>,
    collapsed: BTreeSet<usize>,
    map: FoldMap,
}

impl EditorFolds {
    /// Re-discover regions for `text` and drop collapsed keys that no longer
    /// name a region header. Called once per content revision.
    pub fn reconcile(&mut self, text: &str) {
        self.regions = compute_fold_regions(text);
        let starts: BTreeSet<usize> = self.regions.iter().map(|r| r.start_line).collect();
        self.collapsed.retain(|l| starts.contains(l));
        self.rebuild_map();
    }

    fn rebuild_map(&mut self) {
        let collapsed: Vec<(usize, usize)> = self
            .regions
            .iter()
            .filter(|r| self.collapsed.contains(&r.start_line))
            .map(|r| (r.start_line, r.end_line))
            .collect();
        self.map = FoldMap::from_collapsed_regions(&collapsed);
    }

    pub fn map(&self) -> &FoldMap {
        &self.map
    }

    pub fn regions(&self) -> &[FoldRegion] {
        &self.regions
    }

    /// The region whose header is exactly `line`, if any.
    pub fn region_at(&self, line: usize) -> Option<FoldRegion> {
        self.regions.iter().find(|r| r.start_line == line).copied()
    }

    /// The innermost region containing `line` (header or interior).
    pub fn innermost_containing(&self, line: usize) -> Option<FoldRegion> {
        self.regions
            .iter()
            .filter(|r| r.start_line <= line && line <= r.end_line)
            .max_by_key(|r| r.start_line)
            .copied()
    }

    pub fn is_fold_header(&self, line: usize) -> bool {
        self.regions.iter().any(|r| r.start_line == line)
    }

    pub fn is_collapsed(&self, line: usize) -> bool {
        self.collapsed.contains(&line)
    }

    /// Toggle the fold whose header is `line`. Returns true when state changed.
    pub fn toggle_at(&mut self, line: usize) -> bool {
        if !self.is_fold_header(line) {
            return false;
        }
        if !self.collapsed.remove(&line) {
            self.collapsed.insert(line);
        }
        self.rebuild_map();
        true
    }

    /// Collapse the region at `line`: the one headed there, else the
    /// innermost region containing the line. Returns the collapsed region.
    pub fn collapse_at(&mut self, line: usize) -> Option<FoldRegion> {
        let region = self
            .region_at(line)
            .or_else(|| self.innermost_containing(line))?;
        if self.collapsed.insert(region.start_line) {
            self.rebuild_map();
        }
        Some(region)
    }

    /// Expand the collapsed region headed at `line`. Returns true if expanded.
    pub fn expand_at(&mut self, line: usize) -> bool {
        if self.collapsed.remove(&line) {
            self.rebuild_map();
            true
        } else {
            false
        }
    }

    pub fn collapse_all(&mut self) {
        self.collapsed = self.regions.iter().map(|r| r.start_line).collect();
        self.rebuild_map();
    }

    pub fn expand_all(&mut self) {
        if !self.collapsed.is_empty() {
            self.collapsed.clear();
            self.rebuild_map();
        }
    }

    /// Expand every collapsed region hiding `line`, so the line becomes
    /// visible (used to keep the cursor out of hidden space). Returns true
    /// when anything expanded.
    pub fn reveal_line(&mut self, line: usize) -> bool {
        let before = self.collapsed.len();
        let regions = &self.regions;
        self.collapsed.retain(|start| {
            regions
                .iter()
                .find(|r| r.start_line == *start)
                .is_none_or(|r| !(r.start_line < line && line <= r.end_line))
        });
        if self.collapsed.len() != before {
            self.rebuild_map();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(text: &str) -> Vec<FoldRegion> {
        compute_fold_regions(text)
    }

    fn kinds(text: &str) -> Vec<(usize, usize, FoldKind)> {
        regions(text)
            .into_iter()
            .map(|r| (r.start_line, r.end_line, r.kind))
            .collect()
    }

    #[test]
    fn begin_end_block_folds() {
        let sql = "BEGIN\n  SELECT 1;\nEND";
        assert_eq!(kinds(sql), vec![(0, 2, FoldKind::Block)]);
    }

    #[test]
    fn single_line_begin_end_produces_no_region() {
        assert!(regions("BEGIN SELECT 1; END").is_empty());
    }

    #[test]
    fn nested_begin_end_blocks() {
        let sql = "BEGIN\nBEGIN\nSELECT 1;\nEND\nEND";
        let r = kinds(sql);
        assert!(r.contains(&(0, 4, FoldKind::Block)));
        assert!(r.contains(&(1, 3, FoldKind::Block)));
    }

    #[test]
    fn begin_tran_is_not_a_block() {
        let sql = "BEGIN TRAN\nSELECT 1;\nCOMMIT";
        assert!(regions(sql).is_empty());
        let sql = "BEGIN TRANSACTION\nSELECT 1;\nCOMMIT";
        assert!(regions(sql).is_empty());
        let sql = "BEGIN DISTRIBUTED TRANSACTION\nSELECT 1;\nCOMMIT";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn begin_try_folds_as_block() {
        let sql = "BEGIN TRY\n  SELECT 1;\nEND TRY\nBEGIN CATCH\n  SELECT 2;\nEND CATCH";
        let r = kinds(sql);
        assert!(r.contains(&(0, 2, FoldKind::Block)));
        assert!(r.contains(&(3, 5, FoldKind::Block)));
    }

    #[test]
    fn case_end_folds() {
        let sql = "SELECT CASE\nWHEN 1=1 THEN 'a'\nEND";
        assert_eq!(kinds(sql), vec![(0, 2, FoldKind::Case)]);
    }

    #[test]
    fn multiline_paren_folds() {
        let sql = "SELECT * FROM (\n  SELECT 1 AS x\n) t";
        assert_eq!(kinds(sql), vec![(0, 2, FoldKind::Paren)]);
    }

    #[test]
    fn single_line_paren_no_region() {
        assert!(regions("SELECT COUNT(*) FROM t").is_empty());
    }

    #[test]
    fn multiline_block_comment_folds() {
        let sql = "/*\nheader\n*/\nSELECT 1";
        assert_eq!(kinds(sql), vec![(0, 2, FoldKind::Comment)]);
    }

    #[test]
    fn nested_block_comments_fold_outermost() {
        let sql = "/* outer\n/* inner */\nstill outer */\nSELECT 1";
        assert_eq!(kinds(sql), vec![(0, 2, FoldKind::Comment)]);
    }

    #[test]
    fn unclosed_block_comment_folds_to_end() {
        let sql = "/* runaway\nline\nline";
        assert_eq!(kinds(sql), vec![(0, 2, FoldKind::Comment)]);
    }

    #[test]
    fn keywords_inside_strings_are_ignored() {
        let sql = "SELECT 'BEGIN\nEND' AS s\nFROM t";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn keywords_inside_line_comments_are_ignored() {
        let sql = "-- BEGIN\nSELECT 1\n-- END";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn keywords_inside_bracketed_identifiers_are_ignored() {
        let sql = "SELECT [BEGIN\nEND]\nFROM t";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn doubled_quote_escape_stays_in_string() {
        let sql = "SELECT 'it''s BEGIN\nEND'\nFROM t";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn go_batches_fold() {
        let sql = "SELECT 1\nSELECT 2\nGO\nSELECT 3\nSELECT 4\nGO";
        let r = kinds(sql);
        assert!(r.contains(&(0, 1, FoldKind::Batch)), "{r:?}");
        assert!(r.contains(&(3, 4, FoldKind::Batch)), "{r:?}");
    }

    #[test]
    fn trailing_batch_without_go_folds() {
        let sql = "SELECT 1\nGO\nSELECT 2\nSELECT 3";
        let r = kinds(sql);
        assert!(r.contains(&(2, 3, FoldKind::Batch)), "{r:?}");
    }

    #[test]
    fn no_go_means_no_batch_regions() {
        let sql = "SELECT 1\nSELECT 2";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn go_with_count_is_a_separator() {
        let sql = "SELECT 1\nSELECT 2\nGO 5\nSELECT 3";
        assert!(kinds(sql).contains(&(0, 1, FoldKind::Batch)));
    }

    #[test]
    fn go_in_comment_is_not_a_separator() {
        let sql = "SELECT 1\n-- GO\nSELECT 2";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn go_as_identifier_prefix_is_not_a_separator() {
        let sql = "SELECT 1\nGOTO done\nSELECT 2";
        assert!(regions(sql).is_empty());
    }

    #[test]
    fn one_region_per_header_line_keeps_widest() {
        // BEGIN and ( on the same line: the wider region wins the header key.
        let sql = "BEGIN (\nSELECT 1\n)\nEND";
        let r = kinds(sql);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], (0, 3, FoldKind::Block));
    }

    #[test]
    fn oversized_buffer_disables_folding() {
        let mut sql = String::from("BEGIN\n");
        sql.push_str(&"-- pad\n".repeat(FOLD_SCAN_BYTE_CAP / 7 + 1));
        sql.push_str("END\n");
        assert!(sql.len() > FOLD_SCAN_BYTE_CAP);
        assert!(regions(&sql).is_empty());
    }

    /// Perf smoke: the region scan runs once per edit on buffers up to the
    /// cap; a near-cap buffer must scan in keystroke-compatible time even in
    /// a debug build. Generous bound; guards against accidental O(n^2).
    #[test]
    fn fold_scan_near_cap_smoke() {
        let unit = "BEGIN\n  SELECT 'x' FROM t WHERE a = 1;\nEND\nGO\n";
        let mut sql = String::new();
        while sql.len() + unit.len() < FOLD_SCAN_BYTE_CAP {
            sql.push_str(unit);
        }
        let started = std::time::Instant::now();
        for _ in 0..10 {
            let regions = compute_fold_regions(&sql);
            assert!(!regions.is_empty());
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "10 fold scans of a {}-byte buffer took {:?}",
            sql.len(),
            started.elapsed()
        );
    }

    // ── FoldMap ────────────────────────────────────────────────────────────

    #[test]
    fn empty_map_is_identity() {
        let m = FoldMap::default();
        assert!(m.is_identity());
        assert_eq!(m.buffer_to_display(7), 7);
        assert_eq!(m.display_to_buffer(7), 7);
        assert_eq!(m.display_line_count(10), 10);
    }

    #[test]
    fn map_hides_region_interior() {
        // Region (2,5): hidden lines 3..=5.
        let m = FoldMap::from_collapsed_regions(&[(2, 5)]);
        assert_eq!(m.hidden_line_count(), 3);
        assert!(!m.is_hidden(2));
        assert!(m.is_hidden(3));
        assert!(m.is_hidden(5));
        assert!(!m.is_hidden(6));
        assert_eq!(m.display_line_count(10), 7);
    }

    #[test]
    fn buffer_display_round_trip_for_visible_lines() {
        let m = FoldMap::from_collapsed_regions(&[(1, 3), (6, 8)]);
        // Visible: 0, 1, 4, 5, 6, 9 → display 0..6
        for (d, b) in [(0, 0), (1, 1), (2, 4), (3, 5), (4, 6), (5, 9)] {
            assert_eq!(m.display_to_buffer(d), b, "display {d}");
            assert_eq!(m.buffer_to_display(b), d, "buffer {b}");
        }
    }

    #[test]
    fn hidden_lines_map_to_their_header() {
        let m = FoldMap::from_collapsed_regions(&[(2, 5)]);
        assert_eq!(m.buffer_to_display(3), m.buffer_to_display(2));
        assert_eq!(m.buffer_to_display(5), m.buffer_to_display(2));
    }

    #[test]
    fn overlapping_collapsed_regions_merge() {
        let m = FoldMap::from_collapsed_regions(&[(1, 5), (3, 8)]);
        assert_eq!(m.hidden_line_count(), 7); // 2..=8
        assert!(!m.is_hidden(1));
        assert!(m.is_hidden(8));
    }

    #[test]
    fn display_count_never_zero() {
        let m = FoldMap::from_collapsed_regions(&[(0, 9)]);
        assert_eq!(m.display_line_count(10), 1);
    }

    // ── EditorFolds ────────────────────────────────────────────────────────

    fn folds_for(text: &str) -> EditorFolds {
        let mut f = EditorFolds::default();
        f.reconcile(text);
        f
    }

    #[test]
    fn toggle_collapses_and_expands() {
        let mut f = folds_for("BEGIN\nSELECT 1;\nEND");
        assert!(f.toggle_at(0));
        assert!(f.is_collapsed(0));
        assert_eq!(f.map().display_line_count(3), 1);
        assert!(f.toggle_at(0));
        assert!(!f.is_collapsed(0));
        assert!(f.map().is_identity());
    }

    #[test]
    fn toggle_on_non_header_is_a_no_op() {
        let mut f = folds_for("BEGIN\nSELECT 1;\nEND");
        assert!(!f.toggle_at(1));
        assert!(f.map().is_identity());
    }

    #[test]
    fn collapse_at_prefers_header_then_innermost() {
        let mut f = folds_for("BEGIN\nBEGIN\nSELECT 1;\nEND\nEND");
        // Cursor on line 2 (inside inner block) collapses the inner region.
        let r = f.collapse_at(2).expect("region");
        assert_eq!(r.start_line, 1);
        // Cursor on line 0 collapses the outer region headed there.
        let r = f.collapse_at(0).expect("region");
        assert_eq!(r.start_line, 0);
    }

    #[test]
    fn reveal_line_expands_hiding_regions() {
        let mut f = folds_for("BEGIN\nBEGIN\nSELECT 1;\nEND\nEND");
        f.collapse_all();
        assert!(f.map().is_hidden(2));
        assert!(f.reveal_line(2));
        assert!(!f.map().is_hidden(2));
        // Nothing hides line 0, so revealing it changes nothing.
        assert!(!f.reveal_line(0));
    }

    #[test]
    fn reconcile_keeps_collapsed_folds_with_stable_headers() {
        let mut f = folds_for("BEGIN\nSELECT 1;\nEND\nSELECT 9");
        assert!(f.toggle_at(0));
        // An edit below the fold keeps the header at line 0.
        f.reconcile("BEGIN\nSELECT 1;\nEND\nSELECT 99");
        assert!(f.is_collapsed(0));
        assert!(!f.map().is_identity());
    }

    #[test]
    fn reconcile_drops_collapsed_folds_whose_header_moved() {
        let mut f = folds_for("BEGIN\nSELECT 1;\nEND");
        assert!(f.toggle_at(0));
        // A line inserted above shifts the header; the stale key is dropped.
        f.reconcile("-- new\nBEGIN\nSELECT 1;\nEND");
        assert!(!f.is_collapsed(0));
        assert!(f.map().is_identity());
    }

    #[test]
    fn collapse_all_and_expand_all() {
        let mut f = folds_for("BEGIN\nSELECT 1;\nEND\nGO\nBEGIN\nSELECT 2;\nEND");
        f.collapse_all();
        assert!(!f.map().is_identity());
        f.expand_all();
        assert!(f.map().is_identity());
    }

    fn twelve_line_block() -> String {
        let mut lines = vec!["BEGIN".to_string()];
        for i in 1..=10 {
            lines.push(format!("  SELECT {i};"));
        }
        lines.push("END".to_string());
        lines.join("\n")
    }

    #[test]
    fn preview_returns_first_eight_and_remainder() {
        let sql = twelve_line_block();
        let region = regions(&sql)[0];
        assert_eq!(region.end_line - region.start_line + 1, 12);
        let (lines, remainder) = preview_lines(&sql, region, 8);
        assert_eq!(lines.len(), 8);
        assert_eq!(remainder, 3);
        assert_eq!(lines[0], "  SELECT 1;");
        assert_eq!(lines[7], "  SELECT 8;");
    }

    #[test]
    fn preview_short_region_has_no_remainder() {
        let sql = "BEGIN\n  SELECT 1;\nEND";
        let region = regions(sql)[0];
        let (lines, remainder) = preview_lines(sql, region, 8);
        assert_eq!(lines, vec!["  SELECT 1;".to_string(), "END".to_string()]);
        assert_eq!(remainder, 0);
    }

    #[test]
    fn preview_expands_tabs() {
        let sql = "BEGIN\n\tSELECT 1;\t  \nEND";
        let region = regions(sql)[0];
        let (lines, remainder) = preview_lines(sql, region, 8);
        assert_eq!(lines, vec!["    SELECT 1;".to_string(), "END".to_string()]);
        assert_eq!(remainder, 0);
    }
}
