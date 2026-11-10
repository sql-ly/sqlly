//! The `-- sqlly grid [<n>]:` directive: a result grid's column arrangement
//! (order, hidden and frozen columns, sort, grouping, column filters, and
//! foreign-key labels), written by the result's lock button and honored on
//! the next run.
//!
//! Pure text in, [`GridDirective`] out (and back again via
//! [`directive_line`]); applying one to a live grid is the results view's
//! job. Columns are named, not numbered, so a directive survives a reordered
//! `SELECT` list: names match case-insensitively, and a name the result does
//! not have is skipped.
//!
//! Grammar of the body — `;`-separated `key=value` settings:
//! - `order=a, b, c` — display order; unlisted columns follow naturally;
//! - `hide=a, b` — hidden columns;
//! - `freeze=<n>` — leading displayed columns pinned while scrolling;
//! - `sort=a desc, b` — multi-column sort, ascending by default;
//! - `group=a` — group rows by a column;
//! - `filter=<col> <op> <value>` — one column filter (repeatable);
//! - `label=<col>:<display column|off>` — FK label choice (repeatable).
//!
//! A name containing whitespace or any of `,;:="|` is written in double
//! quotes, with `""` for a literal quote.

use sqlly_datatable::{ColumnFilter, FilterPredicate, NumberOp, TextOp};

/// One parsed `-- sqlly grid` line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GridDirective {
    /// 1-based result set; `None` applies to every result set.
    pub index: Option<usize>,
    pub order: Vec<String>,
    pub hide: Vec<String>,
    pub freeze: Option<usize>,
    /// `(column, descending)` in priority order.
    pub sort: Vec<(String, bool)>,
    pub group: Option<String>,
    /// Column filters by column name, one entry per column.
    pub filters: Vec<(String, ColumnFilter)>,
    /// `(key column, display column)`; `None` turns the label off.
    pub labels: Vec<(String, Option<String>)>,
}

impl GridDirective {
    /// Whether the line targets result `slot` (0-based).
    pub fn applies_to(&self, slot: usize) -> bool {
        self.index.is_none_or(|index| index == slot + 1)
    }

    /// Fold a later line into this one: every setting the later line names
    /// replaces this one's (filters and labels per column).
    fn merge(&mut self, later: GridDirective) {
        if !later.order.is_empty() {
            self.order = later.order;
        }
        if !later.hide.is_empty() {
            self.hide = later.hide;
        }
        if later.freeze.is_some() {
            self.freeze = later.freeze;
        }
        if !later.sort.is_empty() {
            self.sort = later.sort;
        }
        if later.group.is_some() {
            self.group = later.group;
        }
        for (column, filter) in later.filters {
            upsert(&mut self.filters, column, filter);
        }
        for (column, label) in later.labels {
            upsert(&mut self.labels, column, label);
        }
    }

    /// The settings for result `slot`: every line that applies to it, in
    /// document order, merged — so an indexed line refines an unindexed one
    /// written above it.
    pub fn for_slot(directives: &[GridDirective], slot: usize) -> Option<GridDirective> {
        let mut out: Option<GridDirective> = None;
        for directive in directives.iter().filter(|d| d.applies_to(slot)) {
            match out.as_mut() {
                Some(acc) => acc.merge(directive.clone()),
                None => out = Some(directive.clone()),
            }
        }
        out.map(|mut d| {
            d.index = Some(slot + 1);
            d
        })
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
            && self.hide.is_empty()
            && self.freeze.is_none()
            && self.sort.is_empty()
            && self.group.is_none()
            && self.filters.is_empty()
            && self.labels.is_empty()
    }
}

fn upsert<T>(list: &mut Vec<(String, T)>, column: String, value: T) {
    if let Some(slot) = list
        .iter_mut()
        .find(|(name, _)| name.eq_ignore_ascii_case(&column))
    {
        slot.1 = value;
    } else {
        list.push((column, value));
    }
}

/// Every `-- sqlly grid` line in `sql` (lock blocks included), in document
/// order. Lines that do not parse are skipped (the editor squiggles them).
pub fn parse(sql: &str) -> Vec<GridDirective> {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    crate::result_layout::standalone_comments(&sql)
        .into_iter()
        .filter_map(
            |comment| match crate::sqlly_directive::classify_body(&comment.body)? {
                crate::sqlly_directive::Directive::Grid { index, body } => {
                    let mut directive = parse_body(body).ok()?;
                    directive.index = index;
                    Some(directive)
                }
                _ => None,
            },
        )
        .collect()
}

/// Parse a `grid:` body (everything after the colon). The index is left
/// `None`; the caller knows it from the lexer.
pub fn parse_body(body: &str) -> Result<GridDirective, String> {
    let mut out = GridDirective::default();
    let mut any = false;
    for segment in split_quoted(body, ';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((key, value)) = segment.split_once('=') else {
            return Err(crate::localization::format(
                "results.grid_directive.setting_not_key_value",
                &[("setting", segment)],
            ));
        };
        let value = value.trim();
        any = true;
        match key.trim().to_ascii_lowercase().as_str() {
            "order" => out.order = name_list(value)?,
            "hide" => out.hide = name_list(value)?,
            "freeze" => {
                out.freeze = Some(value.parse().map_err(|_| {
                    crate::localization::format(
                        "results.grid_directive.invalid_freeze_count",
                        &[("value", value)],
                    )
                })?);
            }
            "sort" => {
                out.sort = split_quoted(value, ',')
                    .into_iter()
                    .map(|key| parse_sort_key(key.trim()))
                    .collect::<Result<_, _>>()?;
            }
            "group" => out.group = Some(single_name(value)?),
            "filter" => {
                let (column, filter) = parse_filter(value)?;
                match out
                    .filters
                    .iter_mut()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&column))
                {
                    // A predicate line and a value-list line combine, like
                    // the filter panel's two halves.
                    Some((_, existing)) => {
                        if filter.values.is_some() {
                            existing.values = filter.values;
                        } else {
                            existing.predicate = filter.predicate;
                        }
                    }
                    None => out.filters.push((column, filter)),
                }
            }
            "label" => {
                let parts = split_quoted(value, ':');
                let [column, display] = parts.as_slice() else {
                    return Err(crate::localization::format(
                        "results.grid_directive.invalid_label_setting",
                        &[("value", value)],
                    ));
                };
                let column = single_name(column)?;
                let display = if display.trim().eq_ignore_ascii_case("off") {
                    None
                } else {
                    Some(single_name(display)?)
                };
                upsert(&mut out.labels, column, display);
            }
            other => {
                return Err(crate::localization::format(
                    "results.grid_directive.unknown_setting",
                    &[("setting", other)],
                ));
            }
        }
    }
    if !any {
        return Err(crate::localization::text(
            "results.grid_directive.no_settings",
        ));
    }
    Ok(out)
}

/// The `-- sqlly grid` line that reproduces `directive`; `None` when it has
/// nothing to say. The index is written only when `Some`.
pub fn directive_line(directive: &GridDirective) -> Option<String> {
    if directive.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let list = |names: &[String]| {
        names
            .iter()
            .map(|n| quote_name(n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !directive.order.is_empty() {
        parts.push(format!("order={}", list(&directive.order)));
    }
    if !directive.hide.is_empty() {
        parts.push(format!("hide={}", list(&directive.hide)));
    }
    if let Some(freeze) = directive.freeze {
        parts.push(format!("freeze={freeze}"));
    }
    if !directive.sort.is_empty() {
        let keys: Vec<String> = directive
            .sort
            .iter()
            .map(|(name, desc)| {
                if *desc {
                    format!("{} desc", quote_name(name))
                } else {
                    quote_name(name)
                }
            })
            .collect();
        parts.push(format!("sort={}", keys.join(", ")));
    }
    if let Some(group) = &directive.group {
        parts.push(format!("group={}", quote_name(group)));
    }
    for (column, filter) in &directive.filters {
        parts.extend(filter_settings(column, filter));
    }
    for (column, display) in &directive.labels {
        let display = display
            .as_deref()
            .map(|d| {
                // A column literally named "off" must not read as the keyword.
                if d.eq_ignore_ascii_case("off") {
                    format!("\"{d}\"")
                } else {
                    quote_name(d)
                }
            })
            .unwrap_or_else(|| "off".into());
        parts.push(format!("label={}:{display}", quote_name(column)));
    }
    let head = match directive.index {
        Some(index) => format!("-- sqlly grid {index}:"),
        None => "-- sqlly grid:".to_string(),
    };
    Some(format!("{head} {}", parts.join("; ")))
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Word operators, longest first so `not contains` wins over `contains`.
const TEXT_OPS: &[(&str, TextOp)] = &[
    ("not contains", TextOp::DoesNotContain),
    ("contains", TextOp::Contains),
    ("begins with", TextOp::BeginsWith),
    ("ends with", TextOp::EndsWith),
    ("is not", TextOp::IsNot),
    ("is", TextOp::Is),
    ("matches", TextOp::Matches),
];

const NUMBER_OPS: &[(&str, NumberOp)] = &[
    ("not between", NumberOp::NotBetween),
    ("between", NumberOp::Between),
    (">=", NumberOp::Ge),
    ("<=", NumberOp::Le),
    ("!=", NumberOp::Ne),
    ("=", NumberOp::Eq),
    (">", NumberOp::Gt),
    ("<", NumberOp::Lt),
];

fn text_op_word(op: TextOp) -> &'static str {
    TEXT_OPS
        .iter()
        .find(|(_, o)| *o == op)
        .map(|(w, _)| *w)
        .unwrap_or("contains")
}

fn number_op_word(op: NumberOp) -> &'static str {
    NUMBER_OPS
        .iter()
        .find(|(_, o)| *o == op)
        .map(|(w, _)| *w)
        .unwrap_or("=")
}

/// `<col> <op> <value>` → the column and its filter.
fn parse_filter(value: &str) -> Result<(String, ColumnFilter), String> {
    let (column, rest) = take_name(value)?;
    let rest = rest.trim_start();
    let lower = rest.to_ascii_lowercase();
    let word_op = |word: &str| -> Option<&str> {
        let after = lower.strip_prefix(word)?;
        // Words need a space after them; `isolated` is not `is olated`.
        (after.starts_with(' ') || after.is_empty()).then(|| rest[word.len()..].trim())
    };
    if let Some(values) = word_op("in") {
        let values: std::collections::HashSet<String> = split_quoted(values, '|')
            .into_iter()
            .map(|v| unquote(v.trim()))
            .collect();
        return Ok((
            column,
            ColumnFilter {
                predicate: FilterPredicate::None,
                values: Some(values),
            },
        ));
    }
    for (word, op) in NUMBER_OPS {
        let operand = if word.starts_with(|c: char| c.is_ascii_alphabetic()) {
            word_op(word)
        } else {
            rest.strip_prefix(word).map(str::trim)
        };
        let Some(operand) = operand else {
            continue;
        };
        let number = |s: &str| {
            s.trim().parse::<f64>().map_err(|_| {
                crate::localization::format(
                    "results.grid_directive.invalid_number",
                    &[("value", s.trim())],
                )
            })
        };
        let (a, b) = if matches!(op, NumberOp::Between | NumberOp::NotBetween) {
            let lower_operand = operand.to_ascii_lowercase();
            let at = lower_operand.find(" and ").ok_or_else(|| {
                crate::localization::format(
                    "results.grid_directive.invalid_between",
                    &[("operand", operand)],
                )
            })?;
            (number(&operand[..at])?, number(&operand[at + 5..])?)
        } else {
            (number(operand)?, 0.0)
        };
        return Ok((
            column,
            ColumnFilter {
                predicate: FilterPredicate::Number { op: *op, a, b },
                values: None,
            },
        ));
    }
    for (word, op) in TEXT_OPS {
        if let Some(operand) = word_op(word) {
            return Ok((
                column,
                ColumnFilter {
                    predicate: FilterPredicate::Text {
                        op: *op,
                        operand: unquote(operand),
                    },
                    values: None,
                },
            ));
        }
    }
    Err(crate::localization::format(
        "results.grid_directive.unknown_filter_operator",
        &[("value", value)],
    ))
}

/// The `filter=` settings that reproduce `filter` (a predicate and a value
/// list are separate settings for one column).
fn filter_settings(column: &str, filter: &ColumnFilter) -> Vec<String> {
    let column = quote_name(column);
    let mut out = Vec::new();
    match &filter.predicate {
        FilterPredicate::None => {}
        FilterPredicate::Text { op, operand } => out.push(format!(
            "filter={column} {} {}",
            text_op_word(*op),
            quote_value(operand)
        )),
        FilterPredicate::Number { op, a, b } => {
            let word = number_op_word(*op);
            if matches!(op, NumberOp::Between | NumberOp::NotBetween) {
                out.push(format!("filter={column} {word} {a} and {b}"));
            } else {
                out.push(format!("filter={column} {word} {a}"));
            }
        }
    }
    if let Some(values) = &filter.values {
        let mut values: Vec<&String> = values.iter().collect();
        values.sort();
        let values: Vec<String> = values.into_iter().map(|v| quote_value(v)).collect();
        out.push(format!("filter={column} in {}", values.join(" | ")));
    }
    out
}

// ---------------------------------------------------------------------------
// Names and quoting
// ---------------------------------------------------------------------------

fn parse_sort_key(key: &str) -> Result<(String, bool), String> {
    let (name, rest) = take_name(key)?;
    match rest.trim().to_ascii_lowercase().as_str() {
        "" | "asc" => Ok((name, false)),
        "desc" => Ok((name, true)),
        other => Err(crate::localization::format(
            "results.grid_directive.invalid_sort_direction",
            &[("direction", other)],
        )),
    }
}

fn name_list(value: &str) -> Result<Vec<String>, String> {
    split_quoted(value, ',')
        .into_iter()
        .map(single_name)
        .collect()
}

/// Exactly one (possibly quoted) name, nothing after it.
fn single_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.starts_with('"') {
        let (name, rest) = take_name(value)?;
        if !rest.trim().is_empty() {
            return Err(crate::localization::format(
                "results.grid_directive.unexpected_after_column",
                &[("unexpected", rest.trim()), ("column_name", &name)],
            ));
        }
        Ok(name)
    } else if value.is_empty() {
        Err(crate::localization::text(
            "results.grid_directive.missing_column_name",
        ))
    } else {
        Ok(value.to_string())
    }
}

/// A leading name — quoted, or up to the first whitespace — and the rest.
fn take_name(value: &str) -> Result<(String, &str), String> {
    let value = value.trim_start();
    if let Some(body) = value.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = body.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if chars.peek().is_some_and(|(_, n)| *n == '"') {
                    name.push('"');
                    chars.next();
                    continue;
                }
                return Ok((name, &body[i + 1..]));
            }
            name.push(c);
        }
        return Err(crate::localization::format(
            "results.grid_directive.unclosed_quote",
            &[("value", value)],
        ));
    }
    let end = value
        .find(|c: char| c.is_whitespace())
        .unwrap_or(value.len());
    if end == 0 {
        return Err(crate::localization::text(
            "results.grid_directive.missing_column_name",
        ));
    }
    Ok((value[..end].to_string(), &value[end..]))
}

fn unquote(value: &str) -> String {
    match take_name(value) {
        Ok((name, rest)) if value.starts_with('"') && rest.trim().is_empty() => name,
        _ => value.to_string(),
    }
}

/// `name` as written in a directive: bare when safe, double-quoted otherwise.
pub fn quote_name(name: &str) -> String {
    let needs = name.is_empty()
        || name.starts_with('"')
        || name
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '=' | '"' | '|'));
    if needs {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_string()
    }
}

/// A filter operand: bare unless it has edge whitespace, a separator, or a
/// leading quote.
fn quote_value(value: &str) -> String {
    let needs = value.is_empty()
        || value.trim() != value
        || value.starts_with('"')
        || value.contains([';', '|']);
    if needs {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Split on `sep` outside double quotes (`""` inside quotes is an escaped
/// quote, which this treats as closing and reopening — same effect).
fn split_quoted(text: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == sep && !in_quotes {
            out.push(&text[start..i]);
            start = i + c.len_utf8();
        }
    }
    out.push(&text[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str) -> GridDirective {
        let mut found = parse(sql);
        assert_eq!(found.len(), 1, "{sql}");
        found.remove(0)
    }

    #[test]
    fn parses_every_setting() {
        let d = one(
            "-- sqlly grid 2: order=Id, \"Order Date\", Total; hide=Notes; freeze=1; \
             sort=Total desc, Name; group=Region; filter=Status is Open; \
             filter=Total between 10 and 20; label=CustomerId:Name\nSELECT 1",
        );
        assert_eq!(d.index, Some(2));
        assert_eq!(d.order, vec!["Id", "Order Date", "Total"]);
        assert_eq!(d.hide, vec!["Notes"]);
        assert_eq!(d.freeze, Some(1));
        assert_eq!(
            d.sort,
            vec![("Total".to_string(), true), ("Name".to_string(), false)]
        );
        assert_eq!(d.group.as_deref(), Some("Region"));
        assert_eq!(d.filters.len(), 2);
        assert_eq!(
            d.filters[0].1.predicate,
            FilterPredicate::Text {
                op: TextOp::Is,
                operand: "Open".into()
            }
        );
        assert_eq!(
            d.filters[1].1.predicate,
            FilterPredicate::Number {
                op: NumberOp::Between,
                a: 10.0,
                b: 20.0
            }
        );
        assert_eq!(
            d.labels,
            vec![("CustomerId".to_string(), Some("Name".to_string()))]
        );
    }

    #[test]
    fn round_trips_through_the_serializer() {
        let mut values = std::collections::HashSet::new();
        values.insert("West; North".to_string());
        values.insert("East".to_string());
        let d = GridDirective {
            index: Some(1),
            order: vec!["a b".into(), "c,d".into(), "e".into()],
            hide: vec!["say \"hi\"".into()],
            freeze: Some(2),
            sort: vec![("e".into(), true)],
            group: Some("Region".into()),
            filters: vec![
                (
                    "Name".into(),
                    ColumnFilter {
                        predicate: FilterPredicate::Text {
                            op: TextOp::DoesNotContain,
                            operand: " padded ".into(),
                        },
                        values: Some(values),
                    },
                ),
                (
                    "Total".into(),
                    ColumnFilter {
                        predicate: FilterPredicate::Number {
                            op: NumberOp::Ge,
                            a: 2.5,
                            b: 0.0,
                        },
                        values: None,
                    },
                ),
            ],
            labels: vec![("Fk".into(), None), ("Other".into(), Some("off".into()))],
        };
        let line = directive_line(&d).expect("line");
        let back = one(&format!("{line}\nSELECT 1"));
        assert_eq!(back, d, "{line}");
        assert!(
            crate::sqlly_directive::directive_diagnostics(&format!("{line}\nSELECT 1")).is_empty()
        );
    }

    #[test]
    fn unindexed_line_applies_to_every_slot_and_indexed_lines_refine_it() {
        let found = parse("-- sqlly grid: freeze=1; hide=A\n-- sqlly grid 2: hide=B\nSELECT 1");
        let first = GridDirective::for_slot(&found, 0).expect("slot 0");
        assert_eq!(first.hide, vec!["A"]);
        let second = GridDirective::for_slot(&found, 1).expect("slot 1");
        assert_eq!(second.hide, vec!["B"]);
        assert_eq!(second.freeze, Some(1));
        assert!(GridDirective::for_slot(&parse("-- sqlly grid 3: freeze=1"), 0).is_none());
    }

    #[test]
    fn bad_settings_are_errors_and_squiggle() {
        for body in [
            "",
            "order",
            "freeze=x",
            "sort=a sideways",
            "bogus=1",
            "filter=a frobs 2",
        ] {
            assert!(parse_body(body).is_err(), "{body}");
        }
        let diags = crate::sqlly_directive::directive_diagnostics("-- sqlly grid: wat=1\nSELECT 1");
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn directives_inside_a_lock_block_count_but_other_block_comments_do_not() {
        let locked = "/* sqlly lock — written by SQLly\n-- sqlly grid: freeze=2\n*/\nSELECT 1";
        assert_eq!(one(locked).freeze, Some(2));
        let commented = "/* switched off\n-- sqlly grid: freeze=2\n*/\nSELECT 1";
        assert!(parse(commented).is_empty());
    }
}
