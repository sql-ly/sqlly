//! Natural-language dates in date comparisons.
//!
//! Ctrl+Shift+Space after `OrderDate =` (or `<`, `>=`, `<>` …) on a date
//! column opens a guidance card instead of the value lookup. The user types
//! a phrase — "last wed at 3", "november", "ytd" — the card previews what it
//! becomes, and Enter writes the literal (or `BETWEEN` range) followed by a
//! `--sqlly date:<phrase>` marker. Each marker gets a ↻ button that
//! re-evaluates its literal against today, and a statement holding markers
//! gets a "Refresh dates" chip that re-evaluates all of them.
//!
//! Parsing and SQL text live in [`sqlly_intel::natural_date`]; this module
//! owns the editor state, the schema lookup that decides a column's storage
//! kind, and the edits.

use super::info_cards::{CardLine, CardSegment};
use super::*;
use sqlly_intel::natural_date::{self as nd, CompareOp, DateColumnKind, MarkedPredicate};

/// The open date helper: where the phrase being typed starts and what it
/// will be compared against.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DateHelper {
    /// Byte offset of the column expression (the replacement starts here).
    pub column_start: usize,
    /// The column expression as written.
    pub column: String,
    pub op: CompareOp,
    /// Byte offset where the phrase starts; it runs to the caret.
    pub phrase_start: usize,
    pub kind: DateColumnKind,
    /// The declared type, or `None` when the column was taken for a date
    /// by its name.
    pub type_label: Option<String>,
}

/// A clickable ↻ painted by the element, re-stamped every paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DateChip {
    /// Re-evaluate the predicate whose marker starts at this byte offset.
    One(usize),
    /// Re-evaluate every marked predicate in the statement starting here.
    Statement(usize),
}

/// The wall-clock time phrases resolve against.
fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// The phrase as typed, without surrounding quotes or spaces.
fn clean_phrase(raw: &str) -> &str {
    raw.trim().trim_matches(|c| c == '\'' || c == '"').trim()
}

fn seg(text: impl Into<String>, color: Hsla, bold: bool) -> CardSegment {
    CardSegment {
        text: text.into(),
        color,
        bold,
    }
}

fn line(segments: Vec<CardSegment>) -> CardLine {
    CardLine { segments }
}

impl SqlEditor {
    /// The storage kind of `column` (as written at byte `at`), resolving an
    /// alias or bare name to its table through the completion engine. `None`
    /// when the column is known not to be a date — or is unknown and its
    /// name doesn't read like one (unless guessing is off, which also gives
    /// `None` for unknown columns).
    fn date_column_kind(
        &self,
        column: &str,
        at: usize,
    ) -> Option<(DateColumnKind, Option<String>)> {
        let dialect = self.database_type.family();
        let prefs = self.editor_prefs.natural_dates;
        let declared = self.services.as_ref().and_then(|services| {
            // Ask the value-context resolver as if the comparison were an
            // `=`: it maps aliases and bare names onto the FROM tables.
            let probe = format!("{}{column} = ", &self.content[..at.min(self.content.len())]);
            let req = CompletionRequest {
                sql: &probe,
                cursor: probe.len(),
                trigger: IntelliSenseTriggerKind::ManualValueLookup,
                manual: true,
                catalog: &self.completion_catalog,
                procedures: &self.completion_procedures,
                rules: &self.completion_rules,
                parameters: &[],
                model: Some(&self.schema_snapshot),
                database_type: Some(self.database_type),
            };
            let context = services
                .intellisense
                .complete_enriched_response(&req)
                .value_context?;
            sqlly_intel::modelengine::intellisense::column_type_for(
                &self.schema_snapshot,
                context.schema.as_deref(),
                context.table.as_deref()?,
                context.column.as_deref()?,
                dialect,
            )
        });
        match declared {
            Some(ty) if nd::is_date_type(&ty.native) => {
                Some((nd::column_kind(Some(&ty.native), dialect), Some(ty.native)))
            }
            Some(_) => None,
            None if prefs.guess_from_column_names && nd::looks_like_date_column(column) => {
                Some((nd::column_kind(None, dialect), None))
            }
            None => None,
        }
    }

    /// The date comparison the caret is in, when the helper applies: natural
    /// dates are on, the caret follows `<column> <op>`, and the column is a
    /// date (or the phrase typed so far already reads as one).
    pub(super) fn date_helper_at_cursor(&self) -> Option<DateHelper> {
        let prefs = self.editor_prefs.natural_dates;
        if !prefs.enabled || self.database_type.is_command_engine() {
            return None;
        }
        let cursor = self.cursor.min(self.content.len());
        let line_start = self.content[..cursor].rfind('\n').map_or(0, |i| i + 1);
        let prefix = &self.content[line_start..cursor];
        let cmp = nd::comparison_before(prefix)?;
        let column_start = line_start + cmp.column_start;
        let phrase_start = line_start + cmp.phrase_start;
        let (kind, type_label) = match self.date_column_kind(&cmp.column, column_start) {
            Some(found) => found,
            None => {
                // An unknown, oddly named column still qualifies when the
                // user has already typed something that reads as a date.
                let phrase = clean_phrase(&self.content[phrase_start..cursor]);
                let declared_other = self.services.is_some()
                    && nd::parse_natural_date(phrase, now(), &prefs.parse_options()).is_err();
                if phrase.is_empty() || declared_other {
                    return None;
                }
                (nd::column_kind(None, self.database_type.family()), None)
            }
        };
        Some(DateHelper {
            column_start,
            column: cmp.column,
            op: cmp.op,
            phrase_start,
            kind,
            type_label,
        })
    }

    /// Open the helper for the caret's comparison. False when the caret is
    /// not in a date comparison (the caller falls back to value lookup).
    pub(super) fn open_date_helper(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(helper) = self.date_helper_at_cursor() else {
            return false;
        };
        self.close_completions(cx);
        self.signature_help = None;
        self.date_helper = Some(helper);
        cx.notify();
        true
    }

    pub(super) fn close_date_helper(&mut self, cx: &mut Context<Self>) -> bool {
        if self.date_helper.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// After an edit or caret move: close the helper once the caret leaves
    /// the phrase (another line, or before where the phrase starts).
    pub(super) fn sync_date_helper(&mut self) {
        let Some(helper) = &self.date_helper else {
            return;
        };
        let cursor = self.cursor;
        let left = cursor < helper.phrase_start
            || cursor > self.content.len()
            || self.content[helper.phrase_start..cursor].contains('\n');
        if left {
            self.date_helper = None;
        }
    }

    /// The phrase typed so far (empty when the caret left it).
    fn date_helper_phrase(&self, helper: &DateHelper) -> String {
        let cursor = self.cursor.min(self.content.len());
        if cursor < helper.phrase_start {
            return String::new();
        }
        clean_phrase(&self.content[helper.phrase_start..cursor]).to_string()
    }

    /// The predicate a helper writes for `phrase`, or the parse failure.
    fn date_helper_predicate(
        &self,
        helper: &DateHelper,
        phrase: &str,
    ) -> Result<(nd::ParsedDate, String), nd::DateParseError> {
        let prefs = self.editor_prefs.natural_dates;
        let parsed = nd::parse_natural_date(phrase, now(), &prefs.parse_options())?;
        let text = nd::predicate(
            &helper.column,
            helper.op,
            &parsed,
            helper.kind,
            self.database_type.family(),
            prefs.predicate_options(),
        );
        Ok((parsed, text))
    }

    /// Enter / Tab with the helper open: write the date. Returns false when
    /// nothing was typed (the key falls through to its usual meaning); an
    /// unreadable phrase keeps the helper open so it can be fixed.
    pub(super) fn accept_date_helper(&mut self, cx: &mut Context<Self>) -> bool {
        self.sync_date_helper();
        let Some(helper) = self.date_helper.clone() else {
            return false;
        };
        let phrase = self.date_helper_phrase(&helper);
        if phrase.is_empty() {
            self.date_helper = None;
            cx.notify();
            return false;
        }
        let Ok((_, predicate)) = self.date_helper_predicate(&helper, &phrase) else {
            cx.notify();
            return true;
        };
        let cursor = self.cursor.min(self.content.len());
        let line_end = self.content[cursor..]
            .find('\n')
            .map_or(self.content.len(), |i| cursor + i);
        let rest = &self.content[cursor..line_end];
        let mut text = predicate;
        let mut end = cursor;
        if self.editor_prefs.natural_dates.add_marker {
            text.push(' ');
            text.push_str(&nd::marker_comment(&phrase));
            // A line comment runs to the end of the line: whatever followed
            // the phrase moves to the next line, at this line's indent.
            if !rest.trim().is_empty() {
                let line_start = self.content[..helper.column_start]
                    .rfind('\n')
                    .map_or(0, |i| i + 1);
                let indent: String = self.content[line_start..]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                text.push('\n');
                text.push_str(&indent);
                end = cursor + (rest.len() - rest.trim_start().len());
            }
        }
        self.date_helper = None;
        self.selection_anchor = Some(helper.column_start);
        self.cursor = end;
        let inserted_end = helper.column_start + text.find('\n').unwrap_or(text.len());
        self.replace_selection(&text, cx);
        self.cursor = inserted_end;
        self.selection_anchor = None;
        cx.notify();
        true
    }

    /// The guidance / preview card for the open helper.
    pub(super) fn date_helper_card(&self) -> Option<Vec<CardLine>> {
        let helper = self.date_helper.as_ref()?;
        let cursor = self.cursor.min(self.content.len());
        if cursor < helper.phrase_start || self.content[helper.phrase_start..cursor].contains('\n')
        {
            return None;
        }
        let text = Theme::text();
        let dim = Theme::text().opacity(0.65);
        let accent = Theme::accent();
        let what = match &helper.type_label {
            Some(ty) => crate::localization::format(
                "editor.date_helper.column_typed",
                &[("column", &helper.column), ("parameter_type", ty)],
            ),
            None => crate::localization::format(
                "editor.date_helper.column_inferred",
                &[("column", &helper.column)],
            ),
        };
        let mut lines = vec![line(vec![
            seg(
                crate::localization::text("editor.date_helper.instruction"),
                text,
                true,
            ),
            seg(
                crate::localization::format("editor.date_helper.for_column", &[("column", &what)]),
                dim,
                false,
            ),
        ])];
        let examples = |lines: &mut Vec<CardLine>| {
            let all = nd::example_phrases();
            for chunk in all.chunks(5) {
                lines.push(line(vec![seg(chunk.join("  ·  "), dim, false)]));
            }
        };
        let phrase = self.date_helper_phrase(helper);
        if phrase.is_empty() {
            examples(&mut lines);
            lines.push(line(vec![seg(
                crate::localization::text("editor.date_helper.empty_phrase_hint"),
                dim,
                false,
            )]));
            return Some(lines);
        }
        match self.date_helper_predicate(helper, &phrase) {
            Ok((parsed, predicate)) => {
                lines.push(line(vec![seg(
                    nd::describe_relative(&parsed, now()),
                    text,
                    true,
                )]));
                lines.push(line(vec![
                    seg("→ ", dim, false),
                    seg(predicate, accent, false),
                ]));
                lines.push(line(vec![seg(
                    crate::localization::text("editor.date_helper.insert_hint"),
                    dim,
                    false,
                )]));
            }
            Err(_) => {
                lines.push(line(vec![seg(
                    crate::localization::format(
                        "editor.date_helper.unrecognized_phrase",
                        &[("phrase", &phrase)],
                    ),
                    dim,
                    false,
                )]));
                examples(&mut lines);
            }
        }
        Some(lines)
    }

    // ── markers and refresh ────────────────────────────────────────────────

    /// Every marked predicate in the buffer, cached per content revision.
    pub(crate) fn date_markers(&self) -> std::rc::Rc<Vec<MarkedPredicate>> {
        let mut cache = self.date_marker_cache.borrow_mut();
        if cache.0 != Some(self.content_revision) {
            let has_any = self.content.contains("date:");
            cache.1 = std::rc::Rc::new(if has_any {
                nd::find_marked_predicates(&self.content)
            } else {
                Vec::new()
            });
            cache.0 = Some(self.content_revision);
        }
        cache.1.clone()
    }

    /// Whether the refresh buttons are painted.
    pub(crate) fn show_date_refresh_buttons(&self) -> bool {
        let prefs = self.editor_prefs.natural_dates;
        prefs.enabled && prefs.show_refresh_buttons
    }

    /// The statements (by start offset, with their first line) that hold at
    /// least one marker, for the "Refresh dates" chip.
    pub(crate) fn statements_with_date_markers(&self) -> Vec<(usize, usize, usize)> {
        let markers = self.date_markers();
        if markers.is_empty() {
            return Vec::new();
        }
        self.with_statement_spans(|spans| {
            let idx = self.line_index();
            spans
                .iter()
                .filter_map(|span| {
                    let count = markers
                        .iter()
                        .filter(|m| m.span.start >= span.start && m.span.start < span.end)
                        .count();
                    (count > 0).then(|| {
                        let start = statements::span_content_start(&self.content, *span);
                        (span.start, idx.line_col_for_offset(start).0, count)
                    })
                })
                .collect()
        })
    }

    /// The fresh text for `marker`'s predicate, or `None` when its phrase no
    /// longer parses or the text would not change.
    fn refreshed_predicate(&self, marker: &MarkedPredicate) -> Option<String> {
        let prefs = self.editor_prefs.natural_dates;
        let column_at = marker.span.start
            + self.content[marker.span.clone()]
                .find(&marker.column)
                .unwrap_or(0);
        let kind = self
            .date_column_kind(&marker.column, column_at)
            .map_or_else(
                || nd::column_kind(None, self.database_type.family()),
                |(k, _)| k,
            );
        let parsed = nd::parse_natural_date(&marker.phrase, now(), &prefs.parse_options()).ok()?;
        let text = nd::predicate(
            &marker.column,
            marker.op,
            &parsed,
            kind,
            self.database_type.family(),
            prefs.predicate_options(),
        );
        (text != self.content[marker.span.clone()]).then_some(text)
    }

    /// Re-evaluate `markers` as one undoable edit. Returns how many changed.
    fn refresh_date_markers(
        &mut self,
        markers: &[MarkedPredicate],
        cx: &mut Context<Self>,
    ) -> usize {
        let edits: Vec<(Range<usize>, String)> = markers
            .iter()
            .filter_map(|m| Some((m.span.clone(), self.refreshed_predicate(m)?)))
            .collect();
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else {
            return 0;
        };
        let (start, end) = (first.0.start, last.0.end);
        let mut text = String::new();
        let mut at = start;
        for (span, replacement) in &edits {
            text.push_str(&self.content[at..span.start]);
            text.push_str(replacement);
            at = span.end;
        }
        text.push_str(&self.content[at..end]);
        let caret = self.cursor;
        let old_len = end - start;
        self.selection_anchor = Some(start);
        self.cursor = end;
        self.replace_selection(&text, cx);
        // Keep the caret where it was, shifted past a grown/shrunk span.
        self.cursor = if caret >= end {
            (caret + text.len()).saturating_sub(old_len)
        } else {
            caret.min(self.content.len())
        };
        while !self.content.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        self.selection_anchor = None;
        cx.notify();
        edits.len()
    }

    /// Re-evaluate the one marked predicate whose marker starts at `marker`.
    pub(crate) fn refresh_date_marker_at(&mut self, marker: usize, cx: &mut Context<Self>) {
        let markers = self.date_markers();
        if let Some(found) = markers.iter().find(|m| m.marker.start == marker) {
            let found = found.clone();
            self.refresh_date_markers(&[found], cx);
        }
    }

    /// Re-evaluate every marked predicate in the statement starting at
    /// `statement_start`.
    pub(crate) fn refresh_statement_dates(
        &mut self,
        statement_start: usize,
        cx: &mut Context<Self>,
    ) {
        let span = self.with_statement_spans(|spans| {
            spans.iter().find(|s| s.start == statement_start).copied()
        });
        let Some(span) = span else {
            return;
        };
        let markers: Vec<MarkedPredicate> = self
            .date_markers()
            .iter()
            .filter(|m| m.span.start >= span.start && m.span.start < span.end)
            .cloned()
            .collect();
        self.refresh_date_markers(&markers, cx);
    }

    /// Re-evaluate every marked predicate in the buffer (command palette,
    /// and before a run when that preference is on). Returns how many
    /// literals changed.
    pub fn refresh_all_dates(&mut self, cx: &mut Context<Self>) -> usize {
        let markers = self.date_markers();
        if markers.is_empty() {
            return 0;
        }
        let markers: Vec<MarkedPredicate> = markers.iter().cloned().collect();
        self.refresh_date_markers(&markers, cx)
    }

    /// Before a run: refresh every marked date when the preference asks for
    /// it.
    pub fn refresh_dates_before_run(&mut self, cx: &mut Context<Self>) {
        let prefs = self.editor_prefs.natural_dates;
        if prefs.enabled && prefs.refresh_before_run {
            self.refresh_all_dates(cx);
        }
    }

    /// A click on a painted ↻ chip, if the position hits one.
    pub(super) fn date_chip_at(&self, position: Point<Pixels>) -> Option<DateChip> {
        self.date_chip_bounds
            .borrow()
            .iter()
            .find(|(bounds, _)| bounds.contains(&position))
            .map(|(_, chip)| *chip)
    }

    pub(super) fn handle_refresh_dates(
        &mut self,
        _: &RefreshDates,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_all_dates(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::clean_phrase;

    #[test]
    fn phrases_lose_quotes_and_spaces() {
        assert_eq!(clean_phrase("  'last wed at 3' "), "last wed at 3");
        assert_eq!(clean_phrase("\"november\""), "november");
        assert_eq!(clean_phrase("today"), "today");
    }
}
