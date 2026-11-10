//! Filter expression language for the grid filter bar (DbGate-style).
//!
//! A typed alternative to the visual builder: `status = active, created >=
//! LAST WEEK` or `amount >=5 <=10`. Comma separates OR terms, whitespace
//! separates AND conditions (AND binds tighter). A condition is a column
//! reference followed by an operator and a value; a condition without a
//! column reference inherits the previous condition's column, so
//! `status = active, pending` reads as `status = active OR status = pending`.
//!
//! Operators: `=` (also `==` or a bare value), `!=` / `<>`, `<`, `>`, `<=`,
//! `>=`, `^` starts-with, `$` ends-with, `~` contains, `!` negation (with
//! `~`, with a value, or with `NULL`). `NULL` / `NOT NULL` test for null.
//! Values are bare tokens or quoted strings (`'it''s'`, `"with spaces"`).
//! Column names quote as `"name"` or `[name]` when they contain spaces.
//!
//! Date columns additionally accept `YYYY-MM-DD`, the partial forms
//! `YYYY-MM` (month) and `YYYY` (year), and the relative keywords `TODAY`,
//! `YESTERDAY`, `THIS WEEK`, `LAST WEEK`, `THIS MONTH`, `LAST MONTH`,
//! `THIS YEAR`, `LAST YEAR` (case-insensitive; weeks start on Monday). The
//! keywords are keywords on date columns only — on a text column `today` or
//! `last` is an ordinary value. `today` is supplied by the caller in the
//! zone the grid renders datetimes in (see [`today_in_zone`]).
//!
//! Numeric operands stay text: they are validated as plain decimal numbers
//! but never round-tripped through `f64`, so large integers keep every digit
//! in the grid comparison and in generated SQL.
//! A day/period equality lowers to a half-open `[start, end)` range so
//! datetime cells with a time component still match "on that day".
//!
//! Negated starts-with / ends-with (`!^`, `!$`) and the `{$raw}` escape are
//! deliberately unsupported: the parser rejects them with a clear error.
//! GPUI-free so the whole language is unit-testable; `today` is injected.

use chrono::{Datelike as _, NaiveDate};
use sqlly_datatable::ColumnKind;

use crate::grid_filter_builder::{
    BuilderOp, Conjunction, FilterBuilderModel, FilterGroup, FilterItem, FilterRow,
};

/// A column the expression may reference.
pub struct FilterLangColumn {
    pub name: String,
    pub kind: ColumnKind,
}

impl FilterLangColumn {
    pub fn new(name: impl Into<String>, kind: ColumnKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// Parse `input` into a builder model. `today` anchors the relative-date
/// keywords. Errors are human-readable and shown in the filter bar; nothing
/// is applied on `Err`.
pub fn parse_filter_expression(
    input: &str,
    columns: &[FilterLangColumn],
    today: NaiveDate,
) -> Result<FilterBuilderModel, String> {
    let (toks, spaced) = tokenize(input)?;
    if toks.is_empty() {
        return Err(crate::localization::text(
            "results.filter_language.empty_expression",
        ));
    }
    let mut parser = Parser {
        toks: &toks,
        spaced: &spaced,
        pos: 0,
        columns,
        today,
        current_column: None,
    };
    let mut or_terms = vec![parser.parse_and_chain()?];
    while parser.eat_comma() {
        or_terms.push(parser.parse_and_chain()?);
    }
    if let Some(tok) = parser.peek() {
        return Err(crate::localization::format(
            "results.filter_language.unexpected_token",
            &[("token", &tok.to_string())],
        ));
    }
    Ok(assemble(or_terms))
}

/// Pair column names with their kinds for the parser.
pub fn filter_lang_columns<'a>(
    names: impl IntoIterator<Item = &'a str>,
    kinds: impl IntoIterator<Item = ColumnKind>,
) -> Vec<FilterLangColumn> {
    names
        .into_iter()
        .zip(kinds)
        .map(|(name, kind)| FilterLangColumn::new(name, kind))
        .collect()
}

/// The expression text worth storing beside a saved filter: `Some` only when
/// `text` still parses (as of `today`) to exactly `model`, i.e. the builder
/// was not edited by hand after the expression was applied. Storing the
/// text is what keeps `LAST WEEK` meaning last week when the filter is
/// reapplied later.
pub fn expression_for_save(
    text: &str,
    model: &FilterBuilderModel,
    columns: &[FilterLangColumn],
    today: NaiveDate,
) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let parsed = parse_filter_expression(text, columns, today).ok()?;
    (parsed == *model).then(|| text.to_string())
}

/// A saved filter resolved against the current result's columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSavedFilter {
    /// The conditions to apply, with column indices into the current result.
    pub model: FilterBuilderModel,
    /// Names of saved columns the current result does not have. Their
    /// conditions were dropped; callers must surface this.
    pub missing_columns: Vec<String>,
}

/// The model to apply for a saved filter: its expression re-resolved
/// against `today` when it has one that still parses (the expression names
/// its columns, so it is projection-independent), else the stored model with
/// every condition re-targeted **by column name** — a saved filter is shared
/// by every query on the table, and `SELECT *` and `SELECT total, status`
/// put the same column at different positions. Conditions on columns the
/// result lacks are dropped and reported in `missing_columns`. Filters saved
/// before names were recorded fall back to their positions.
pub fn resolve_saved_model(
    saved: &crate::saved_filters::SavedFilter,
    columns: &[FilterLangColumn],
    today: NaiveDate,
) -> ResolvedSavedFilter {
    if let Some(model) = saved
        .expression
        .as_deref()
        .and_then(|text| parse_filter_expression(text, columns, today).ok())
    {
        return ResolvedSavedFilter {
            model,
            missing_columns: Vec::new(),
        };
    }
    retarget_by_name(&saved.model, &saved.columns, columns)
}

/// Re-target `model`, whose column indices point into `saved_names`, onto
/// `columns` by name. Conditions whose column is absent are dropped and
/// named in `missing_columns`; groups left empty are pruned. An empty
/// `saved_names` keeps positions (legacy saved filters).
pub fn retarget_by_name(
    model: &FilterBuilderModel,
    saved_names: &[String],
    columns: &[FilterLangColumn],
) -> ResolvedSavedFilter {
    let mut missing = Vec::new();
    let mut root = model.root.clone();
    retarget_group(&mut root, saved_names, columns, &mut missing);
    ResolvedSavedFilter {
        model: FilterBuilderModel { root },
        missing_columns: missing,
    }
}

/// Re-point every indexed condition in `group` from its saved position to
/// the current position of the same column name; drop (and record) the
/// ones whose column is gone, and prune groups left empty.
fn retarget_group(
    group: &mut FilterGroup,
    saved_names: &[String],
    columns: &[FilterLangColumn],
    missing: &mut Vec<String>,
) {
    group.items.retain_mut(|item| match item {
        FilterItem::Row(row) => {
            let crate::grid_filter_builder::ColumnTarget::Index(saved_idx) = row.column else {
                return true;
            };
            let resolved = match saved_names.get(saved_idx) {
                Some(name) => columns
                    .iter()
                    .position(|c| c.name == *name)
                    .or_else(|| {
                        columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(name))
                    })
                    .ok_or_else(|| name.clone()),
                // Saved before column names were recorded: positional.
                None if saved_names.is_empty() && saved_idx < columns.len() => Ok(saved_idx),
                None => Err(crate::localization::format(
                    "results.filter_language.saved_column_position",
                    &[("column_position", &(saved_idx + 1).to_string())],
                )),
            };
            match resolved {
                Ok(idx) => {
                    row.column = crate::grid_filter_builder::ColumnTarget::Index(idx);
                    true
                }
                Err(name) => {
                    if !missing.contains(&name) {
                        missing.push(name);
                    }
                    false
                }
            }
        }
        FilterItem::Group(inner) => {
            retarget_group(inner, saved_names, columns, missing);
            !inner.items.is_empty()
        }
    });
}

/// "Today" for relative-date keywords, in the time zone the grid renders
/// datetimes in. `TODAY` must mean the calendar day the user sees in the
/// grid: with the default *Always UTC* display a local midnight would be
/// hours away from the UTC midnight the cells are drawn against. *As
/// stored* and *Local* use the local calendar (as-stored values carry no
/// zone; the local day is the closest reading).
pub fn today_in_zone(
    zone: &crate::result_cell_format::ResolvedZone,
    now: chrono::DateTime<chrono::Utc>,
) -> NaiveDate {
    use crate::result_cell_format::ResolvedZone;
    match zone {
        ResolvedZone::Utc => now.date_naive(),
        ResolvedZone::Named(tz) => now.with_timezone(tz).date_naive(),
        ResolvedZone::Local | ResolvedZone::AsStored => {
            #[cfg(target_arch = "wasm32")]
            {
                now.date_naive()
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                now.with_timezone(&chrono::Local).date_naive()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Word(String),
    /// `'…'`, `"…"`, or `[…]` content with escapes resolved; position decides
    /// whether it names a column or is a string value.
    Quoted(String),
    Op(&'static str),
    Comma,
}

impl std::fmt::Display for Tok {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tok::Word(w) => write!(f, "'{w}'"),
            Tok::Quoted(s) => write!(f, "\"{s}\""),
            Tok::Op(op) => write!(f, "'{op}'"),
            Tok::Comma => write!(f, "','"),
        }
    }
}

/// Longest-first so `!=` never tokenizes as `!` + `=`.
const OPS: &[&str] = &[
    "!=", "!~", "!^", "!$", "<>", "<=", ">=", "==", "=", "<", ">", "^", "$", "~", "!",
];

fn is_op_char(c: char) -> bool {
    matches!(c, '=' | '!' | '<' | '>' | '^' | '$' | '~')
}

/// Tokens plus, per token, whether whitespace (or the start of input)
/// precedes it — the parser uses that to spot a value that runs straight
/// into an operator character (`url = a=b`).
fn tokenize(input: &str) -> Result<(Vec<Tok>, Vec<bool>), String> {
    let chars: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut spaced = Vec::new();
    let mut i = 0;
    let mut after_space = true;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            after_space = true;
            i += 1;
            continue;
        }
        spaced.push(after_space);
        after_space = false;
        if c == ',' {
            toks.push(Tok::Comma);
            after_space = true;
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            let (text, next) = quoted(&chars, i, c)?;
            toks.push(Tok::Quoted(text));
            i = next;
            continue;
        }
        if c == '[' {
            let (text, next) = bracketed(&chars, i)?;
            toks.push(Tok::Quoted(text));
            i = next;
            continue;
        }
        if is_op_char(c) {
            let rest: String = chars[i..].iter().collect();
            let op = OPS
                .iter()
                .find(|op| rest.starts_with(**op))
                .expect("is_op_char guarantees a match");
            toks.push(Tok::Op(op));
            i += op.len();
            continue;
        }
        let start = i;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() || c == ',' || c == '\'' || c == '"' || c == '[' || is_op_char(c) {
                break;
            }
            i += 1;
        }
        toks.push(Tok::Word(chars[start..i].iter().collect()));
    }
    Ok((toks, spaced))
}

fn quoted(chars: &[char], start: usize, quote: char) -> Result<(String, usize), String> {
    let mut text = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == quote {
            if chars.get(i + 1) == Some(&quote) {
                text.push(quote);
                i += 2;
            } else {
                return Ok((text, i + 1));
            }
        } else {
            text.push(chars[i]);
            i += 1;
        }
    }
    Err(crate::localization::text(
        "results.filter_language.unterminated_string",
    ))
}

fn bracketed(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut text = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == ']' {
            return Ok((text, i + 1));
        }
        text.push(chars[i]);
        i += 1;
    }
    Err(crate::localization::text(
        "results.filter_language.unterminated_column",
    ))
}

/// A parsed operand, before it is matched against a column kind.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Null,
    NotNull,
    Literal(String),
    /// One calendar day (`YYYY-MM-DD`).
    Day(NaiveDate),
    /// Half-open period `[start, end)`: a month, a year, or a relative span.
    Period(NaiveDate, NaiveDate),
}

struct Parser<'a> {
    toks: &'a [Tok],
    /// Parallel to `toks`: whitespace precedes the token.
    spaced: &'a [bool],
    pos: usize,
    columns: &'a [FilterLangColumn],
    today: NaiveDate,
    current_column: Option<usize>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.pos)
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn eat_comma(&mut self) -> bool {
        if matches!(self.peek(), Some(Tok::Comma)) {
            self.advance();
            return true;
        }
        false
    }

    fn column_named(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The column a leading token refers to, or the inherited column when the
    /// token is an operator or a non-column word.
    fn condition_column(&mut self) -> Result<usize, String> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                if let Some(idx) = self.column_named(w) {
                    self.advance();
                    Ok(idx)
                } else if let Some(idx) = self.current_column {
                    Ok(idx)
                } else {
                    Err(crate::localization::format(
                        "results.filter_language.unknown_unquoted_column",
                        &[("column_name", w)],
                    ))
                }
            }
            Some(Tok::Quoted(q)) => {
                if let Some(idx) = self.column_named(q) {
                    self.advance();
                    Ok(idx)
                } else if let Some(idx) = self.current_column {
                    Ok(idx)
                } else {
                    Err(crate::localization::format(
                        "results.filter_language.unknown_quoted_column",
                        &[("column_name", q)],
                    ))
                }
            }
            Some(Tok::Op(_)) => self.current_column.ok_or_else(|| {
                crate::localization::text("results.filter_language.first_condition_needs_column")
            }),
            Some(Tok::Comma) => Err(crate::localization::text(
                "results.filter_language.condition_after_comma",
            )),
            None => Err(crate::localization::text(
                "results.filter_language.condition_expected",
            )),
        }
    }

    fn parse_and_chain(&mut self) -> Result<Vec<FilterItem>, String> {
        let mut items = self.parse_condition()?;
        while !matches!(self.peek(), None | Some(Tok::Comma)) {
            items.extend(self.parse_condition()?);
        }
        Ok(items)
    }

    fn parse_condition(&mut self) -> Result<Vec<FilterItem>, String> {
        let column = self.condition_column()?;
        self.current_column = Some(column);
        let kind = self.columns[column].kind;
        match self.peek() {
            Some(Tok::Op(op)) => {
                let op = *op;
                self.advance();
                self.lower_op(column, kind, op)
            }
            Some(Tok::Word(_)) | Some(Tok::Quoted(_)) => {
                let value = self.parse_value(kind)?;
                lower_equal(column, kind, value)
            }
            Some(Tok::Comma) | None => Err(crate::localization::format(
                "results.filter_language.operator_or_value_required",
                &[("column_name", &self.columns[column].name)],
            )),
        }
    }

    fn lower_op(
        &mut self,
        column: usize,
        kind: ColumnKind,
        op: &str,
    ) -> Result<Vec<FilterItem>, String> {
        match op {
            "=" | "==" => {
                let value = self.parse_value(kind)?;
                lower_equal(column, kind, value)
            }
            "!=" | "<>" => {
                let value = self.parse_value(kind)?;
                lower_not_equal(column, kind, value)
            }
            "<" | "<=" | ">" | ">=" => {
                let value = self.parse_value(kind)?;
                lower_range(column, kind, op, value)
            }
            "^" | "$" | "~" => {
                let value = self.parse_text_operand(kind, op)?;
                let builder_op = match op {
                    "^" => BuilderOp::BeginsWith,
                    "$" => BuilderOp::EndsWith,
                    _ => BuilderOp::Contains,
                };
                Ok(vec![row_item(column, builder_op, &value)])
            }
            "!~" => {
                let value = self.parse_text_operand(kind, "!~")?;
                Ok(vec![row_item(column, BuilderOp::NotContains, &value)])
            }
            "!^" | "!$" => Err(negated_pattern_error(op)),
            "!" => match self.peek() {
                Some(Tok::Op("~")) => {
                    self.advance();
                    let value = self.parse_text_operand(kind, "!~")?;
                    Ok(vec![row_item(column, BuilderOp::NotContains, &value)])
                }
                Some(Tok::Op(inner @ ("^" | "$"))) => Err(negated_pattern_error(inner)),
                _ => {
                    let value = self.parse_value(kind)?;
                    lower_not_equal(column, kind, value)
                }
            },
            _ => Err(crate::localization::format(
                "results.filter_language.unexpected_operator",
                &[("operator", op)],
            )),
        }
    }

    fn parse_text_operand(&mut self, kind: ColumnKind, op: &str) -> Result<String, String> {
        if uses_number_kind(kind) {
            return Err(crate::localization::format(
                "results.filter_language.text_operator_wrong_column",
                &[
                    ("operator", op),
                    ("column_kind", &localized_kind_label(kind)),
                ],
            ));
        }
        match self.parse_value(kind)? {
            Value::Literal(text) => Ok(text),
            Value::Null | Value::NotNull => Err(crate::localization::format(
                "results.filter_language.text_operator_null",
                &[("operator", op)],
            )),
            Value::Day(_) | Value::Period(_, _) => Err(crate::localization::format(
                "results.filter_language.text_operator_date",
                &[("operator", op)],
            )),
        }
    }

    fn parse_value(&mut self, kind: ColumnKind) -> Result<Value, String> {
        let Some(tok) = self.peek() else {
            return Err(crate::localization::text(
                "results.filter_language.missing_value_at_end",
            ));
        };
        match tok {
            Tok::Quoted(text) => {
                let text = text.clone();
                self.advance();
                if kind == ColumnKind::Date {
                    parse_date_value(&text).ok_or_else(|| not_a_date(&text))
                } else {
                    Ok(Value::Literal(text))
                }
            }
            Tok::Word(word) => {
                let word = word.clone();
                self.advance();
                // An unquoted text value glued to an operator character
                // would silently split into two conditions (`url = a=b`
                // reads as `url = a AND url = b`); ask for quotes instead.
                // Number and date values cannot contain those characters,
                // so `qty>=5<=10` keeps meaning a range.
                if !uses_number_kind(kind)
                    && let Some(Tok::Op(op)) = self.peek()
                    && !self.spaced.get(self.pos).copied().unwrap_or(true)
                {
                    return Err(crate::localization::format(
                        "results.filter_language.unquoted_operator_in_value",
                        &[("value", word.as_str()), ("operator", *op)],
                    ));
                }
                if word.eq_ignore_ascii_case("null") {
                    return Ok(Value::Null);
                }
                if word.eq_ignore_ascii_case("not")
                    && let Some(Tok::Word(w)) = self.peek()
                    && w.eq_ignore_ascii_case("null")
                {
                    self.advance();
                    return Ok(Value::NotNull);
                }
                // Relative-date keywords are keywords only on date columns;
                // on a text column `today`, `last`, `this`, and `not` are
                // ordinary values.
                if kind != ColumnKind::Date {
                    return Ok(Value::Literal(word));
                }
                if word.eq_ignore_ascii_case("not") {
                    return Err(crate::localization::text(
                        "results.filter_language.not_must_precede_null",
                    ));
                }
                if let Some(value) = self.parse_relative_keyword(&word)? {
                    return Ok(value);
                }
                parse_date_value(&word).ok_or_else(|| not_a_date(&word))
            }
            Tok::Op(op) => Err(crate::localization::format(
                "results.filter_language.missing_value_after_operator",
                &[("operator", op)],
            )),
            Tok::Comma => Err(crate::localization::text(
                "results.filter_language.missing_value_before_comma",
            )),
        }
    }

    /// Relative-date keywords; `Ok(None)` when `word` starts none of them.
    fn parse_relative_keyword(&mut self, word: &str) -> Result<Option<Value>, String> {
        if word.eq_ignore_ascii_case("today") {
            return Ok(Some(Value::Period(
                self.today,
                self.today + chrono::Days::new(1),
            )));
        }
        if word.eq_ignore_ascii_case("yesterday") {
            let yesterday = self.today - chrono::Days::new(1);
            return Ok(Some(Value::Period(yesterday, self.today)));
        }
        let this = word.eq_ignore_ascii_case("this");
        let last = word.eq_ignore_ascii_case("last");
        if !this && !last {
            return Ok(None);
        }
        let keyword = word.to_uppercase();
        let Some(Tok::Word(unit)) = self.peek() else {
            return Err(crate::localization::format(
                "results.filter_language.relative_period_missing_unit",
                &[("keyword", &keyword)],
            ));
        };
        let unit = unit.clone();
        self.advance();
        let today = self.today;
        let period = if unit.eq_ignore_ascii_case("week") {
            let week_start =
                today - chrono::Days::new(today.weekday().num_days_from_monday() as u64);
            if this {
                (week_start, week_start + chrono::Days::new(7))
            } else {
                (week_start - chrono::Days::new(7), week_start)
            }
        } else if unit.eq_ignore_ascii_case("month") {
            let Some(month_start) = NaiveDate::from_ymd_opt(today.year(), today.month(), 1) else {
                return Err(crate::localization::text(
                    "results.filter_language.month_start_unavailable",
                ));
            };
            if this {
                (month_start, add_month(month_start))
            } else {
                (sub_month(month_start), month_start)
            }
        } else if unit.eq_ignore_ascii_case("year") {
            let (Some(year_start), Some(next_year), Some(prev_year)) = (
                NaiveDate::from_ymd_opt(today.year(), 1, 1),
                NaiveDate::from_ymd_opt(today.year() + 1, 1, 1),
                NaiveDate::from_ymd_opt(today.year() - 1, 1, 1),
            ) else {
                return Err(crate::localization::text(
                    "results.filter_language.year_bounds_unavailable",
                ));
            };
            if this {
                (year_start, next_year)
            } else {
                (prev_year, year_start)
            }
        } else {
            return Err(crate::localization::format(
                "results.filter_language.relative_period_invalid_unit",
                &[("keyword", &keyword), ("unit", &unit)],
            ));
        };
        Ok(Some(Value::Period(period.0, period.1)))
    }
}

fn negated_pattern_error(op: &str) -> String {
    let key = if op.contains('^') {
        "results.filter_language.negated_starts_with"
    } else {
        "results.filter_language.negated_ends_with"
    };
    crate::localization::text(key)
}

fn not_a_date(text: &str) -> String {
    crate::localization::format("results.filter_language.invalid_date", &[("value", text)])
}

fn add_month(first: NaiveDate) -> NaiveDate {
    let (year, month) = if first.month() == 12 {
        (first.year() + 1, 1)
    } else {
        (first.year(), first.month() + 1)
    };
    // Day 1 always exists; only an out-of-range year can fail.
    NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(first)
}

fn sub_month(first: NaiveDate) -> NaiveDate {
    let (year, month) = if first.month() == 1 {
        (first.year() - 1, 12)
    } else {
        (first.year(), first.month() - 1)
    };
    NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(first)
}

/// `YYYY-MM-DD` → a day, `YYYY-MM` → that month, `YYYY` → that year.
fn parse_date_value(text: &str) -> Option<Value> {
    let text = text.trim();
    let parts: Vec<&str> = text.split('-').collect();
    match parts.as_slice() {
        [y] if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) => {
            let year: i32 = y.parse().ok()?;
            let start = NaiveDate::from_ymd_opt(year, 1, 1)?;
            let end = NaiveDate::from_ymd_opt(year + 1, 1, 1)?;
            Some(Value::Period(start, end))
        }
        [y, m]
            if y.len() == 4
                && y.bytes().all(|b| b.is_ascii_digit())
                && (1..=2).contains(&m.len())
                && m.bytes().all(|b| b.is_ascii_digit()) =>
        {
            let year: i32 = y.parse().ok()?;
            let month: u32 = m.parse().ok()?;
            let start = NaiveDate::from_ymd_opt(year, month, 1)?;
            Some(Value::Period(start, add_month(start)))
        }
        [y, m, d]
            if y.len() == 4
                && (1..=2).contains(&m.len())
                && (1..=2).contains(&d.len())
                && [y, m, d]
                    .iter()
                    .all(|p| p.bytes().all(|b| b.is_ascii_digit())) =>
        {
            let year: i32 = y.parse().ok()?;
            let month: u32 = m.parse().ok()?;
            let day: u32 = d.parse().ok()?;
            Some(Value::Day(NaiveDate::from_ymd_opt(year, month, day)?))
        }
        _ => None,
    }
}

fn uses_number_kind(kind: ColumnKind) -> bool {
    matches!(
        kind,
        ColumnKind::Integer | ColumnKind::Decimal | ColumnKind::Date
    )
}

fn localized_kind_label(kind: ColumnKind) -> String {
    let key = match kind {
        ColumnKind::Integer | ColumnKind::Decimal => "results.filter_language.number_column",
        ColumnKind::Date => "results.filter_language.date_column",
        _ => "results.filter_language.text_column",
    };
    crate::localization::text(key)
}

fn date_column_required(kind: ColumnKind) -> String {
    crate::localization::format(
        "results.filter_language.date_value_wrong_column",
        &[("column_kind", &localized_kind_label(kind))],
    )
}

fn row_item(column: usize, op: BuilderOp, a: &str) -> FilterItem {
    let mut row = FilterRow::new(column, op);
    row.a = a.to_string();
    FilterItem::Row(row)
}

fn ymd(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// A validated numeric operand, kept as text so it never loses precision:
/// integers stay exactly as typed (`1234567890123456789`), whole decimals
/// normalize (`5.0` → `5`), and other decimals keep their own digits.
fn number_operand(text: &str) -> Result<String, String> {
    let literal = crate::grid_filter_builder::number_literal(text).ok_or_else(|| {
        crate::localization::format("results.filter_language.invalid_number", &[("value", text)])
    })?;
    if let Ok(n) = literal.parse::<i128>() {
        return Ok(n.to_string());
    }
    match literal.parse::<f64>() {
        Ok(n) if n.fract() == 0.0 && n.abs() < 1e15 => Ok(format!("{}", n as i64)),
        _ => Ok(literal),
    }
}

fn lower_equal(column: usize, kind: ColumnKind, value: Value) -> Result<Vec<FilterItem>, String> {
    match value {
        Value::Null => Ok(vec![row_item(column, BuilderOp::IsNull, "")]),
        Value::NotNull => Ok(vec![row_item(column, BuilderOp::IsNotNull, "")]),
        Value::Literal(text) => match kind {
            ColumnKind::Integer | ColumnKind::Decimal => {
                let n = number_operand(&text)?;
                Ok(vec![row_item(column, BuilderOp::Eq, &n)])
            }
            ColumnKind::Date => Err(not_a_date(&text)),
            _ => Ok(vec![row_item(column, BuilderOp::Is, &text)]),
        },
        Value::Day(day) => match kind {
            ColumnKind::Date => Ok(vec![
                row_item(column, BuilderOp::Ge, &ymd(day)),
                row_item(column, BuilderOp::Lt, &ymd(day + chrono::Days::new(1))),
            ]),
            _ => Err(date_column_required(kind)),
        },
        Value::Period(start, end) => match kind {
            ColumnKind::Date => Ok(vec![
                row_item(column, BuilderOp::Ge, &ymd(start)),
                row_item(column, BuilderOp::Lt, &ymd(end)),
            ]),
            _ => Err(date_column_required(kind)),
        },
    }
}

fn lower_not_equal(
    column: usize,
    kind: ColumnKind,
    value: Value,
) -> Result<Vec<FilterItem>, String> {
    match value {
        Value::Null => Ok(vec![row_item(column, BuilderOp::IsNotNull, "")]),
        Value::NotNull => Ok(vec![row_item(column, BuilderOp::IsNull, "")]),
        Value::Literal(text) => match kind {
            ColumnKind::Integer | ColumnKind::Decimal => {
                let n = number_operand(&text)?;
                Ok(vec![row_item(column, BuilderOp::Ne, &n)])
            }
            ColumnKind::Date => Err(not_a_date(&text)),
            _ => Ok(vec![row_item(column, BuilderOp::IsNot, &text)]),
        },
        Value::Day(day) => match kind {
            ColumnKind::Date => Ok(vec![FilterItem::Group(FilterGroup {
                conjunction: Conjunction::Or,
                items: vec![
                    row_item(column, BuilderOp::Lt, &ymd(day)),
                    row_item(column, BuilderOp::Ge, &ymd(day + chrono::Days::new(1))),
                ],
            })]),
            _ => Err(date_column_required(kind)),
        },
        Value::Period(start, end) => match kind {
            ColumnKind::Date => Ok(vec![FilterItem::Group(FilterGroup {
                conjunction: Conjunction::Or,
                items: vec![
                    row_item(column, BuilderOp::Lt, &ymd(start)),
                    row_item(column, BuilderOp::Ge, &ymd(end)),
                ],
            })]),
            _ => Err(date_column_required(kind)),
        },
    }
}

fn lower_range(
    column: usize,
    kind: ColumnKind,
    op: &str,
    value: Value,
) -> Result<Vec<FilterItem>, String> {
    if !uses_number_kind(kind) {
        return Err(crate::localization::format(
            "results.filter_language.range_operator_wrong_column",
            &[("operator", op)],
        ));
    }
    match value {
        Value::Null | Value::NotNull => Err(crate::localization::format(
            "results.filter_language.range_operator_null",
            &[("operator", op)],
        )),
        Value::Literal(text) => {
            if kind == ColumnKind::Date {
                return Err(not_a_date(&text));
            }
            let n = number_operand(&text)?;
            let builder_op = match op {
                "<" => BuilderOp::Lt,
                "<=" => BuilderOp::Le,
                ">" => BuilderOp::Gt,
                _ => BuilderOp::Ge,
            };
            Ok(vec![row_item(column, builder_op, &n)])
        }
        // `<`/`>=` align to the period start, `<=`/`>` to its end, so
        // `> 2026-09-28` means "after that day" and `>= 2026-09` keeps all of
        // September.
        Value::Day(day) => {
            let (builder_op, bound) = match op {
                ">=" => (BuilderOp::Ge, day),
                ">" => (BuilderOp::Ge, day + chrono::Days::new(1)),
                "<" => (BuilderOp::Lt, day),
                _ => (BuilderOp::Lt, day + chrono::Days::new(1)),
            };
            Ok(vec![row_item(column, builder_op, &ymd(bound))])
        }
        Value::Period(start, end) => {
            let (builder_op, bound) = match op {
                ">=" => (BuilderOp::Ge, start),
                ">" => (BuilderOp::Ge, end),
                "<" => (BuilderOp::Lt, start),
                _ => (BuilderOp::Lt, end),
            };
            Ok(vec![row_item(column, builder_op, &ymd(bound))])
        }
    }
}

fn assemble(mut or_terms: Vec<Vec<FilterItem>>) -> FilterBuilderModel {
    if or_terms.len() == 1 {
        return FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::And,
                items: or_terms.pop().unwrap_or_default(),
            },
        };
    }
    let mut items = Vec::new();
    for chain in or_terms {
        if chain.len() == 1 {
            items.extend(chain);
        } else {
            items.push(FilterItem::Group(FilterGroup {
                conjunction: Conjunction::And,
                items: chain,
            }));
        }
    }
    FilterBuilderModel {
        root: FilterGroup {
            conjunction: Conjunction::Or,
            items,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<FilterLangColumn> {
        vec![
            FilterLangColumn::new("status", ColumnKind::Text),
            FilterLangColumn::new("qty", ColumnKind::Integer),
            FilterLangColumn::new("price", ColumnKind::Decimal),
            FilterLangColumn::new("created", ColumnKind::Date),
            FilterLangColumn::new("my col", ColumnKind::Text),
        ]
    }

    /// 2026-09-28 is a Monday; every relative-date test pins to it.
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 28).unwrap()
    }

    fn parse(input: &str) -> Result<FilterBuilderModel, String> {
        parse_filter_expression(input, &cols(), today())
    }

    fn root_rows(model: &FilterBuilderModel) -> Vec<&FilterRow> {
        model
            .root
            .items
            .iter()
            .filter_map(|item| match item {
                FilterItem::Row(row) => Some(row),
                FilterItem::Group(_) => None,
            })
            .collect()
    }

    fn leaf(column: usize, op: BuilderOp, a: &str) -> FilterRow {
        let mut row = FilterRow::new(column, op);
        row.a = a.into();
        row
    }

    fn and_model(items: Vec<FilterItem>) -> FilterBuilderModel {
        FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::And,
                items,
            },
        }
    }

    #[test]
    fn equality_and_bare_value() {
        let model = parse("status = active").unwrap();
        assert_eq!(
            model,
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, "active"))])
        );
        assert_eq!(parse("status active").unwrap(), model);
        assert_eq!(parse("status == active").unwrap(), model);
    }

    /// A `0x…` value (a crypto wallet address) against a text column is
    /// text: it filters by the string as typed, never decoded as hex bytes.
    #[test]
    fn hex_looking_value_in_a_text_column_filters_as_text() {
        let wallet = "0x52908400098527886E0F7030069857D2E4169EE7";
        let model = parse(&format!("status = {wallet}")).unwrap();
        assert_eq!(
            model,
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, wallet))])
        );
        assert_eq!(parse(&format!("status {wallet}")).unwrap(), model);
    }

    #[test]
    fn comma_is_or_and_inherits_the_column() {
        let model = parse("status = active, pending").unwrap();
        assert_eq!(model.root.conjunction, Conjunction::Or);
        assert_eq!(
            root_rows(&model),
            vec![
                &leaf(0, BuilderOp::Is, "active"),
                &leaf(0, BuilderOp::Is, "pending"),
            ]
        );
    }

    #[test]
    fn space_is_and_and_binds_tighter_than_comma() {
        let model = parse("qty = 1, status = a status = b").unwrap();
        assert_eq!(model.root.conjunction, Conjunction::Or);
        assert_eq!(model.root.items.len(), 2);
        let FilterItem::Row(first) = &model.root.items[0] else {
            panic!("first term is a row");
        };
        assert_eq!(*first, leaf(1, BuilderOp::Eq, "1"));
        let FilterItem::Group(group) = &model.root.items[1] else {
            panic!("second term is a group");
        };
        assert_eq!(group.conjunction, Conjunction::And);
        let rows: Vec<&FilterRow> = group
            .items
            .iter()
            .filter_map(|item| match item {
                FilterItem::Row(row) => Some(row),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            vec![&leaf(0, BuilderOp::Is, "a"), &leaf(0, BuilderOp::Is, "b")]
        );
    }

    #[test]
    fn numeric_range_is_two_conditions() {
        let model = parse("qty >=5 <=10").unwrap();
        assert_eq!(
            model,
            and_model(vec![
                FilterItem::Row(leaf(1, BuilderOp::Ge, "5")),
                FilterItem::Row(leaf(1, BuilderOp::Le, "10")),
            ])
        );
    }

    #[test]
    fn numeric_ops_and_decimal_operands() {
        assert_eq!(
            parse("qty != 3").unwrap(),
            and_model(vec![FilterItem::Row(leaf(1, BuilderOp::Ne, "3"))])
        );
        assert_eq!(
            parse("price > 9.5").unwrap(),
            and_model(vec![FilterItem::Row(leaf(2, BuilderOp::Gt, "9.5"))])
        );
        assert_eq!(
            parse("price < 0.25").unwrap(),
            and_model(vec![FilterItem::Row(leaf(2, BuilderOp::Lt, "0.25"))])
        );
    }

    #[test]
    fn text_pattern_operators() {
        assert_eq!(
            parse("status ^act").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::BeginsWith, "act"))])
        );
        assert_eq!(
            parse("status $ive").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::EndsWith, "ive"))])
        );
        assert_eq!(
            parse("status ~tiv").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Contains, "tiv"))])
        );
        assert_eq!(
            parse("status !~tiv").unwrap(),
            and_model(vec![FilterItem::Row(leaf(
                0,
                BuilderOp::NotContains,
                "tiv"
            ))])
        );
        assert_eq!(
            parse("status ! ~tiv").unwrap(),
            and_model(vec![FilterItem::Row(leaf(
                0,
                BuilderOp::NotContains,
                "tiv"
            ))])
        );
    }

    #[test]
    fn negation_of_value_and_not_equals() {
        assert_eq!(
            parse("status != active").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNot, "active"))])
        );
        assert_eq!(
            parse("status <> active").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNot, "active"))])
        );
        assert_eq!(
            parse("status ! active").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNot, "active"))])
        );
    }

    #[test]
    fn null_tests() {
        assert_eq!(
            parse("status = null").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNull, ""))])
        );
        assert_eq!(
            parse("status = NULL").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNull, ""))])
        );
        assert_eq!(
            parse("status = not null").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNotNull, ""))])
        );
        assert_eq!(
            parse("status != null").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNotNull, ""))])
        );
        assert_eq!(
            parse("status ! null").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNotNull, ""))])
        );
    }

    #[test]
    fn quoted_values_keep_spaces_and_escapes() {
        assert_eq!(
            parse("status = 'in progress'").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, "in progress"))])
        );
        assert_eq!(
            parse("status = 'it''s'").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, "it's"))])
        );
        assert_eq!(
            parse("status = \"with spaces\"").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, "with spaces"))])
        );
    }

    #[test]
    fn quoted_column_names() {
        assert_eq!(
            parse("\"my col\" = x").unwrap(),
            and_model(vec![FilterItem::Row(leaf(4, BuilderOp::Is, "x"))])
        );
        assert_eq!(
            parse("[my col] = x").unwrap(),
            and_model(vec![FilterItem::Row(leaf(4, BuilderOp::Is, "x"))])
        );
    }

    #[test]
    fn full_date_equality_is_a_day_range() {
        assert_eq!(
            parse("created = 2026-09-28").unwrap(),
            and_model(vec![
                FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-09-28")),
                FilterItem::Row(leaf(3, BuilderOp::Lt, "2026-09-29")),
            ])
        );
    }

    #[test]
    fn partial_dates_are_period_ranges() {
        assert_eq!(
            parse("created = 2026-09").unwrap(),
            and_model(vec![
                FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-09-01")),
                FilterItem::Row(leaf(3, BuilderOp::Lt, "2026-10-01")),
            ])
        );
        assert_eq!(
            parse("created = 2026").unwrap(),
            and_model(vec![
                FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-01-01")),
                FilterItem::Row(leaf(3, BuilderOp::Lt, "2027-01-01")),
            ])
        );
    }

    #[test]
    fn relative_dates() {
        let cases: &[(&str, &str, &str)] = &[
            ("created = TODAY", "2026-09-28", "2026-09-29"),
            ("created = YESTERDAY", "2026-09-27", "2026-09-28"),
            ("created = THIS WEEK", "2026-09-28", "2026-10-05"),
            ("created = LAST WEEK", "2026-09-21", "2026-09-28"),
            ("created = THIS MONTH", "2026-09-01", "2026-10-01"),
            ("created = LAST MONTH", "2026-08-01", "2026-09-01"),
            ("created = THIS YEAR", "2026-01-01", "2027-01-01"),
            ("created = LAST YEAR", "2025-01-01", "2026-01-01"),
        ];
        for (input, start, end) in cases {
            assert_eq!(
                parse(input).unwrap(),
                and_model(vec![
                    FilterItem::Row(leaf(3, BuilderOp::Ge, start)),
                    FilterItem::Row(leaf(3, BuilderOp::Lt, end)),
                ]),
                "{input}"
            );
        }
        assert_eq!(
            parse("created = last week").unwrap(),
            parse("created = LAST WEEK").unwrap()
        );
    }

    #[test]
    fn relative_dates_with_range_operators_use_period_edges() {
        assert_eq!(
            parse("created >= LAST WEEK").unwrap(),
            and_model(vec![FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-09-21"))])
        );
        assert_eq!(
            parse("created < this month").unwrap(),
            and_model(vec![FilterItem::Row(leaf(3, BuilderOp::Lt, "2026-09-01"))])
        );
        assert_eq!(
            parse("created > 2026-09").unwrap(),
            and_model(vec![FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-10-01"))])
        );
        assert_eq!(
            parse("created <= 2026-09").unwrap(),
            and_model(vec![FilterItem::Row(leaf(3, BuilderOp::Lt, "2026-10-01"))])
        );
        assert_eq!(
            parse("created > 2026-09-28").unwrap(),
            and_model(vec![FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-09-29"))])
        );
        assert_eq!(
            parse("created <= 2026-09-28").unwrap(),
            and_model(vec![FilterItem::Row(leaf(3, BuilderOp::Lt, "2026-09-29"))])
        );
    }

    #[test]
    fn not_equal_date_is_an_or_group() {
        let model = parse("created != 2026-09-28").unwrap();
        assert_eq!(
            model,
            and_model(vec![FilterItem::Group(FilterGroup {
                conjunction: Conjunction::Or,
                items: vec![
                    FilterItem::Row(leaf(3, BuilderOp::Lt, "2026-09-28")),
                    FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-09-29")),
                ],
            })])
        );
    }

    #[test]
    fn date_and_text_mix_across_comma() {
        let model = parse("status = active, created >= LAST WEEK").unwrap();
        assert_eq!(model.root.conjunction, Conjunction::Or);
        assert_eq!(
            root_rows(&model),
            vec![
                &leaf(0, BuilderOp::Is, "active"),
                &leaf(3, BuilderOp::Ge, "2026-09-21"),
            ]
        );
    }

    #[test]
    fn date_equality_inside_and_chain_splices_rows() {
        let model = parse("status = active created = 2026-09").unwrap();
        assert_eq!(
            model,
            and_model(vec![
                FilterItem::Row(leaf(0, BuilderOp::Is, "active")),
                FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-09-01")),
                FilterItem::Row(leaf(3, BuilderOp::Lt, "2026-10-01")),
            ])
        );
    }

    #[test]
    fn errors_are_human_readable() {
        let err = parse("nosuch = 1").unwrap_err();
        assert!(err.contains("unknown column 'nosuch'"), "{err}");
        let err = parse("status < x").unwrap_err();
        assert!(err.contains("needs a number or date column"), "{err}");
        let err = parse("qty ~ 3").unwrap_err();
        assert!(err.contains("needs a text column"), "{err}");
        let err = parse("qty = abc").unwrap_err();
        assert!(err.contains("is not a number"), "{err}");
        let err = parse("status = 2026-09-28 created = 2026-09-28x").unwrap_err();
        assert!(err.contains("is not a date"), "{err}");
        let err = parse("created = banana").unwrap_err();
        assert!(err.contains("is not a date"), "{err}");
        let err = parse("status = 'open").unwrap_err();
        assert!(err.contains("unterminated string"), "{err}");
        let err = parse("status = a,").unwrap_err();
        assert!(err.contains("expected a condition"), "{err}");
        let err = parse("").unwrap_err();
        assert!(err.contains("type a filter expression"), "{err}");
        let err = parse("= 3").unwrap_err();
        assert!(err.contains("first condition must name a column"), "{err}");
        let err = parse("status !^abc").unwrap_err();
        assert!(err.contains("not supported"), "{err}");
        let err = parse("status =").unwrap_err();
        assert!(err.contains("missing value"), "{err}");
    }

    #[test]
    fn saved_expression_re_resolves_relative_dates_at_apply_time() {
        let text = "created >= LAST WEEK";
        let model = parse(text).unwrap();
        let stored = expression_for_save(text, &model, &cols(), today());
        assert_eq!(stored.as_deref(), Some(text));
        let saved = crate::saved_filters::SavedFilter {
            name: "recent".into(),
            is_default: false,
            model: model.clone(),
            expression: stored,
            columns: names(&cols()),
        };
        // Applied the same day: identical to the stored model.
        assert_eq!(resolve_saved_model(&saved, &cols(), today()).model, model);
        // Applied a week later: LAST WEEK moved forward seven days.
        let next_week = today() + chrono::Days::new(7);
        assert_eq!(
            resolve_saved_model(&saved, &cols(), next_week).model,
            and_model(vec![FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-09-28"))])
        );
    }

    fn names(columns: &[FilterLangColumn]) -> Vec<String> {
        columns.iter().map(|c| c.name.clone()).collect()
    }

    #[test]
    fn saved_expression_falls_back_to_the_stored_model() {
        let model = parse("status = active").unwrap();
        let saved = crate::saved_filters::SavedFilter {
            name: "gone".into(),
            is_default: false,
            model: model.clone(),
            expression: Some("renamed_column = active".into()),
            columns: names(&cols()),
        };
        assert_eq!(resolve_saved_model(&saved, &cols(), today()).model, model);
        let plain = crate::saved_filters::SavedFilter {
            expression: None,
            ..saved
        };
        let resolved = resolve_saved_model(&plain, &cols(), today());
        assert_eq!(resolved.model, model);
        assert!(resolved.missing_columns.is_empty());
    }

    #[test]
    fn saved_model_resolves_columns_by_name_across_projections() {
        // Saved against `SELECT *` (status, qty, price, created, my col) ...
        let model = parse("qty >= 5 status = active").unwrap();
        let saved = crate::saved_filters::SavedFilter {
            name: "busy".into(),
            is_default: true,
            model,
            expression: None,
            columns: names(&cols()),
        };
        // ... applied to `SELECT created, status, qty`: every condition
        // follows its column, not its old position.
        let projected = vec![
            FilterLangColumn::new("created", ColumnKind::Date),
            FilterLangColumn::new("status", ColumnKind::Text),
            FilterLangColumn::new("qty", ColumnKind::Integer),
        ];
        let resolved = resolve_saved_model(&saved, &projected, today());
        assert_eq!(
            resolved.model,
            and_model(vec![
                FilterItem::Row(leaf(2, BuilderOp::Ge, "5")),
                FilterItem::Row(leaf(1, BuilderOp::Is, "active")),
            ])
        );
        assert!(resolved.missing_columns.is_empty());
        // A projection without `qty` drops that condition and says so.
        let narrow = vec![FilterLangColumn::new("status", ColumnKind::Text)];
        let resolved = resolve_saved_model(&saved, &narrow, today());
        assert_eq!(
            resolved.model,
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, "active"))])
        );
        assert_eq!(resolved.missing_columns, vec!["qty".to_string()]);
    }

    #[test]
    fn saved_model_prunes_groups_emptied_by_missing_columns() {
        let model = parse("status = a, qty = 1 price = 2").unwrap();
        let saved = crate::saved_filters::SavedFilter {
            name: "x".into(),
            is_default: false,
            model,
            expression: None,
            columns: names(&cols()),
        };
        let only_status = vec![FilterLangColumn::new("status", ColumnKind::Text)];
        let resolved = resolve_saved_model(&saved, &only_status, today());
        assert_eq!(resolved.model.leaf_count(), 1);
        assert_eq!(
            resolved.model.root.items.len(),
            1,
            "the emptied AND group is gone"
        );
        assert_eq!(
            resolved.missing_columns,
            vec!["qty".to_string(), "price".to_string()]
        );
    }

    #[test]
    fn legacy_saved_filters_without_names_stay_positional() {
        let model = parse("qty = 1").unwrap();
        let saved = crate::saved_filters::SavedFilter {
            name: "old".into(),
            is_default: false,
            model: model.clone(),
            expression: None,
            columns: Vec::new(),
        };
        assert_eq!(resolve_saved_model(&saved, &cols(), today()).model, model);
        let short = vec![FilterLangColumn::new("status", ColumnKind::Text)];
        let resolved = resolve_saved_model(&saved, &short, today());
        assert_eq!(resolved.model.leaf_count(), 0);
        assert_eq!(resolved.missing_columns, vec!["column 2".to_string()]);
    }

    #[test]
    fn relative_keywords_are_plain_values_on_text_columns() {
        for word in ["today", "yesterday", "last", "this", "not"] {
            assert_eq!(
                parse(&format!("status = {word}")).unwrap(),
                and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, word))]),
                "{word}"
            );
        }
        // NULL tests still work on text columns.
        assert_eq!(
            parse("status = not null").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::IsNotNull, ""))])
        );
        // And the keywords still are keywords on date columns.
        assert!(parse("created = today").unwrap().leaf_count() == 2);
        let err = parse("created = not x").unwrap_err();
        assert!(err.contains("'NOT' must be followed by 'NULL'"), "{err}");
    }

    #[test]
    fn large_integers_keep_every_digit() {
        assert_eq!(
            parse("qty = 1234567890123456789").unwrap(),
            and_model(vec![FilterItem::Row(leaf(
                1,
                BuilderOp::Eq,
                "1234567890123456789"
            ))])
        );
        assert_eq!(
            parse("price > 0.1000000000000000055").unwrap(),
            and_model(vec![FilterItem::Row(leaf(
                2,
                BuilderOp::Gt,
                "0.1000000000000000055"
            ))])
        );
    }

    #[test]
    fn values_glued_to_operators_need_quotes() {
        let err = parse("status = a=b").unwrap_err();
        assert!(err.contains("quote values"), "{err}");
        assert_eq!(
            parse("status = 'a=b'").unwrap(),
            and_model(vec![FilterItem::Row(leaf(0, BuilderOp::Is, "a=b"))])
        );
        // Spaced operators still chain, and numeric ranges may be compact.
        assert_eq!(parse("qty>=5<=10").unwrap(), parse("qty >=5 <=10").unwrap());
        assert_eq!(parse("status = a !~b").unwrap().leaf_count(), 2);
    }

    #[test]
    fn dates_need_four_digit_years() {
        for bad in [
            "created = 26-09-28",
            "created = 202-09-28",
            "created = 2026-009-28",
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains("is not a date"), "{bad}: {err}");
        }
        assert!(parse("created = 2026-9-8").is_ok());
    }

    #[test]
    fn today_follows_the_grid_display_zone() {
        use crate::result_cell_format::ResolvedZone;
        use chrono::TimeZone as _;
        // 23:30 UTC on the 28th is already the 29th in Tokyo.
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 9, 28, 23, 30, 0)
            .unwrap();
        assert_eq!(today_in_zone(&ResolvedZone::Utc, now), today());
        let tokyo = ResolvedZone::Named(chrono_tz::Asia::Tokyo);
        assert_eq!(
            today_in_zone(&tokyo, now),
            NaiveDate::from_ymd_opt(2026, 9, 29).unwrap()
        );
    }

    #[test]
    fn date_not_equal_inside_an_or_term_nests_two_levels() {
        // The builder renders any depth (see grid_filter_builder's layout
        // test); this pins the shape the parser produces.
        let model = parse("status = a created != 2026-09-28, qty = 1").unwrap();
        assert_eq!(model.leaf_count(), 4);
        let FilterItem::Group(and_term) = &model.root.items[0] else {
            panic!("first OR term is an AND group");
        };
        assert!(
            matches!(&and_term.items[1], FilterItem::Group(g) if g.conjunction == Conjunction::Or)
        );
    }

    #[test]
    fn expression_is_not_stored_after_hand_edits() {
        let text = "qty >= 5";
        let mut model = parse(text).unwrap();
        assert!(expression_for_save(text, &model, &cols(), today()).is_some());
        // The builder was edited after applying: the text no longer matches.
        model.push_row(leaf(1, BuilderOp::Le, "10"));
        assert_eq!(expression_for_save(text, &model, &cols(), today()), None);
        assert_eq!(expression_for_save("  ", &model, &cols(), today()), None);
        assert_eq!(
            expression_for_save("nosuch = 1", &model, &cols(), today()),
            None
        );
    }

    #[test]
    fn filter_lang_columns_pairs_names_and_kinds() {
        let names = ["a".to_string(), "b".to_string()];
        let columns = filter_lang_columns(
            names.iter().map(String::as_str),
            [ColumnKind::Text, ColumnKind::Integer],
        );
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[1].name, "b");
        assert_eq!(columns[1].kind, ColumnKind::Integer);
    }

    #[test]
    fn relative_dates_across_boundaries() {
        // Sunday 2027-01-03: the week started Monday 2026-12-28, last month
        // is December of the previous year.
        let sunday = NaiveDate::from_ymd_opt(2027, 1, 3).unwrap();
        let p = |input: &str| parse_filter_expression(input, &cols(), sunday).unwrap();
        assert_eq!(
            p("created = THIS WEEK"),
            and_model(vec![
                FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-12-28")),
                FilterItem::Row(leaf(3, BuilderOp::Lt, "2027-01-04")),
            ])
        );
        assert_eq!(
            p("created = LAST MONTH"),
            and_model(vec![
                FilterItem::Row(leaf(3, BuilderOp::Ge, "2026-12-01")),
                FilterItem::Row(leaf(3, BuilderOp::Lt, "2027-01-01")),
            ])
        );
        assert_eq!(
            p("created = THIS MONTH"),
            and_model(vec![
                FilterItem::Row(leaf(3, BuilderOp::Ge, "2027-01-01")),
                FilterItem::Row(leaf(3, BuilderOp::Lt, "2027-02-01")),
            ])
        );
    }

    #[test]
    fn more_validation_errors() {
        let err = parse("created = 2026-13").unwrap_err();
        assert!(err.contains("is not a date"), "{err}");
        let err = parse("created = 2026-02-30").unwrap_err();
        assert!(err.contains("is not a date"), "{err}");
        let err = parse("created = LAST FORTNIGHT").unwrap_err();
        assert!(err.contains("expected WEEK, MONTH, or YEAR"), "{err}");
        let err = parse("created = THIS").unwrap_err();
        assert!(err.contains("must be followed by WEEK"), "{err}");
        let err = parse("created = not x").unwrap_err();
        assert!(err.contains("'NOT' must be followed by 'NULL'"), "{err}");
        let err = parse("[my col = x").unwrap_err();
        assert!(err.contains("unterminated [column]"), "{err}");
        let err = parse("status").unwrap_err();
        assert!(err.contains("needs an operator or a value"), "{err}");
        let err = parse("qty >= null").unwrap_err();
        assert!(err.contains("not NULL"), "{err}");
        let err = parse("status ^ null").unwrap_err();
        assert!(err.contains("needs a text value, not NULL"), "{err}");
        let err = parse("qty = inf").unwrap_err();
        assert!(err.contains("is not a number"), "{err}");
        let err = parse("status $!x").unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn case_insensitive_columns_and_quoted_dates() {
        assert_eq!(parse("STATUS = x").unwrap(), parse("status = x").unwrap());
        assert_eq!(
            parse("created >= '2026-09'").unwrap(),
            parse("created >= 2026-09").unwrap()
        );
    }

    #[test]
    fn negative_numbers_and_numeric_normalization() {
        assert_eq!(
            parse("qty > -5").unwrap(),
            and_model(vec![FilterItem::Row(leaf(1, BuilderOp::Gt, "-5"))])
        );
        assert_eq!(
            parse("qty = 5.0").unwrap(),
            and_model(vec![FilterItem::Row(leaf(1, BuilderOp::Eq, "5"))])
        );
    }

    #[test]
    fn parsed_model_renders_sql() {
        use sqlly_core::ResultColumn;
        use sqlly_lang::sqlmodel::DatabaseType;
        let columns = vec![
            ResultColumn {
                name: "status".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "qty".into(),
                data_type: "int".into(),
                nullable: true,
            },
            ResultColumn {
                name: "price".into(),
                data_type: "decimal".into(),
                nullable: true,
            },
            ResultColumn {
                name: "created".into(),
                data_type: "datetime".into(),
                nullable: true,
            },
            ResultColumn {
                name: "my col".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
        ];
        let kinds: Vec<ColumnKind> = cols().iter().map(|c| c.kind).collect();
        let model = parse("status = active, created >= LAST WEEK").unwrap();
        let sql = model
            .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
            .unwrap();
        assert_eq!(sql, "[status] = N'active' OR [created] >= '20260921'");
    }
}
