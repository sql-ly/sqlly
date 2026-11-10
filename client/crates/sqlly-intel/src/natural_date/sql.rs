//! SQL side of natural-language dates: turn a [`ParsedDate`] into the
//! literal or range predicate a comparison needs, in the column's own
//! precision and the engine's own literal spelling, and find the predicates
//! written earlier (by their `--sqlly date:` marker) so they can be
//! re-evaluated.
//!
//! Pure text in, text out — the editor owns cursor spans and the schema
//! lookup that decides a column's [`DateColumnKind`].

use std::ops::Range;

use chrono::{Duration, NaiveDateTime, NaiveTime, Timelike};
use sqlly_lang::sqlmodel::Dialect;

use super::ParsedDate;

/// The marker comment written after a converted date: `--sqlly date:<phrase>`.
pub const MARKER_PREFIX: &str = "--sqlly date:";

/// A comparison operator the date helper understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    /// The operator for its SQL spelling (`!=` and `<>` both mean `Ne`).
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "=" => Self::Eq,
            "<>" | "!=" => Self::Ne,
            "<" => Self::Lt,
            "<=" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            _ => return None,
        })
    }

    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// How a date-only period ("today", "november") becomes a range when the
/// comparison is `=` or `<>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RangeStyle {
    /// `col BETWEEN start AND last-instant-of-the-period`, with the last
    /// instant written at the column's own precision.
    #[default]
    Between,
    /// `(col >= start AND col < start-of-next-period)` — exact for any
    /// precision, at the cost of naming the column twice.
    HalfOpen,
}

/// What the column stores, which decides the literal's shape and how close
/// to the end of a period an inclusive `BETWEEN` end can be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateColumnKind {
    /// A calendar date with no time of day.
    Date,
    /// A date and time with this many fractional-second digits.
    DateTime { fraction_digits: u8 },
    /// SQL Server `datetime`: 1/300 s ticks, so the last instant of a day is
    /// `23:59:59.997` (`.999` rounds up into the next day).
    SqlServerDateTime,
    /// SQL Server `smalldatetime`: minutes; `23:59:59` would round up.
    SmallDateTime,
}

/// Options for writing a predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredicateOptions {
    /// Turn a period ("today", "last week") into a range; when off, the
    /// period's first instant is compared like a point in time.
    pub convert_ranges: bool,
    pub range_style: RangeStyle,
}

impl Default for PredicateOptions {
    fn default() -> Self {
        Self {
            convert_ranges: true,
            range_style: RangeStyle::Between,
        }
    }
}

/// The base type name, lowercased, without its `(precision)`.
fn base_type(native: &str) -> (String, Option<u8>) {
    let lower = native.trim().to_ascii_lowercase();
    let (name, precision) = match lower.find('(') {
        Some(open) => {
            let digits: String = lower[open + 1..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let tail = lower[open..]
                .find(')')
                .map(|close| lower[open + close + 1..].to_string())
                .unwrap_or_default();
            (
                format!("{}{}", lower[..open].trim_end(), tail),
                digits.parse::<u8>().ok(),
            )
        }
        None => (lower.clone(), None),
    };
    (
        name.split_whitespace().collect::<Vec<_>>().join(" "),
        precision,
    )
}

/// True when `native` names a type that holds a calendar date (with or
/// without a time of day). Time-only and interval types are not dates.
pub fn is_date_type(native: &str) -> bool {
    let (name, _) = base_type(native);
    matches!(
        name.as_str(),
        "date"
            | "datetime"
            | "datetime2"
            | "smalldatetime"
            | "datetimeoffset"
            | "timestamp"
            | "timestamptz"
            | "timestamp without time zone"
            | "timestamp with time zone"
            | "timestamp with local time zone"
            | "date32"
            | "datetime64"
    )
}

/// A guess for when the schema doesn't know the column's type: names that
/// read like a date or timestamp (`OrderDate`, `created_at`, `ModifiedOn`,
/// `ship_dt`, `EventTime`).
pub fn looks_like_date_column(name: &str) -> bool {
    let bare = name
        .rsplit('.')
        .next()
        .unwrap_or(name)
        .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'));
    let lower = bare.to_ascii_lowercase();
    if lower.starts_with("date") || lower.ends_with("date") || lower.ends_with("dates") {
        return true;
    }
    if ["time", "timestamp", "_at", "_on", "_dt", "dttm", "_ts"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return true;
    }
    // CamelCase `CreatedAt` / `ModifiedOn`: a capital `At`/`On` after a
    // lowercase letter (so `Format`, `Region` and `Bacon` don't count).
    let chars: Vec<char> = bare.chars().collect();
    let n = chars.len();
    n >= 3
        && chars[n - 3].is_ascii_lowercase()
        && matches!((chars[n - 2], chars[n - 1]), ('A', 't') | ('O', 'n'))
}

/// The storage kind of a column from its native type spelling, or the
/// dialect's safest default when the type is unknown.
pub fn column_kind(native: Option<&str>, dialect: Dialect) -> DateColumnKind {
    let default = match dialect {
        // Safe for both `datetime` and `datetime2` columns: `.997` is the
        // last `datetime` tick and still inside the day for `datetime2`.
        Dialect::SqlServer => DateColumnKind::SqlServerDateTime,
        Dialect::Postgres | Dialect::Oracle => DateColumnKind::DateTime { fraction_digits: 6 },
        Dialect::MySql => DateColumnKind::DateTime { fraction_digits: 0 },
        Dialect::Sqlite => DateColumnKind::DateTime { fraction_digits: 3 },
    };
    let Some(native) = native else {
        return default;
    };
    let (name, precision) = base_type(native);
    let digits = |fallback: u8| DateColumnKind::DateTime {
        fraction_digits: precision.unwrap_or(fallback).min(9),
    };
    match (dialect, name.as_str()) {
        // Oracle's DATE carries a time of day (to the second).
        (Dialect::Oracle, "date") => DateColumnKind::DateTime { fraction_digits: 0 },
        (_, "date" | "date32") => DateColumnKind::Date,
        (Dialect::SqlServer, "datetime") => DateColumnKind::SqlServerDateTime,
        (Dialect::SqlServer, "smalldatetime") => DateColumnKind::SmallDateTime,
        (Dialect::SqlServer, "datetime2" | "datetimeoffset") => digits(7),
        (Dialect::MySql, "datetime" | "timestamp") => digits(0),
        (Dialect::Sqlite, _) => digits(3),
        (_, "datetime64") => digits(3),
        (_, "datetime") => digits(0),
        (_, n) if n.starts_with("timestamp") => digits(6),
        _ => default,
    }
}

/// `dt` as `YYYY-MM-DD<sep>HH:MM:SS[.fff]`, with only as many fractional
/// digits as the value needs (at most `max_digits`).
fn format_datetime(dt: NaiveDateTime, sep: char, max_digits: u8) -> String {
    let mut out = format!("{}{sep}{}", dt.format("%Y-%m-%d"), dt.format("%H:%M:%S"));
    if max_digits > 0 {
        let nanos = dt.nanosecond() % 1_000_000_000;
        if nanos > 0 {
            let full = format!("{nanos:09}");
            let kept = &full[..max_digits.min(9) as usize];
            let trimmed = kept.trim_end_matches('0');
            if !trimmed.is_empty() {
                out.push('.');
                out.push_str(trimmed);
            }
        }
    }
    out
}

/// The SQL literal for `dt` in a column of `kind` on `dialect`.
pub fn literal(dt: NaiveDateTime, kind: DateColumnKind, dialect: Dialect) -> String {
    let date = || dt.format("%Y-%m-%d").to_string();
    match kind {
        DateColumnKind::Date => match dialect {
            Dialect::Oracle => format!("DATE '{}'", date()),
            _ => format!("'{}'", date()),
        },
        DateColumnKind::DateTime { fraction_digits } => match dialect {
            // The `T` form is read the same under every DATEFORMAT and
            // language setting; the space form is not, for `datetime`.
            Dialect::SqlServer => format!("'{}'", format_datetime(dt, 'T', fraction_digits)),
            Dialect::Oracle => format!("TIMESTAMP '{}'", format_datetime(dt, ' ', fraction_digits)),
            _ => format!("'{}'", format_datetime(dt, ' ', fraction_digits)),
        },
        DateColumnKind::SqlServerDateTime => format!("'{}'", format_datetime(dt, 'T', 3)),
        DateColumnKind::SmallDateTime => format!("'{}'", format_datetime(dt, 'T', 0)),
    }
}

/// The last instant inside a period ending (exclusively) at `end`, as close
/// to `end` as `kind` can store without rounding into the next period.
pub fn inclusive_end(end: NaiveDateTime, kind: DateColumnKind) -> NaiveDateTime {
    let step = match kind {
        DateColumnKind::Date => {
            // A date column holds whole days: the period's last day.
            let at_midnight = end.time() == NaiveTime::MIN;
            let day = if at_midnight {
                end.date().pred_opt().unwrap_or(end.date())
            } else {
                end.date()
            };
            return day.and_time(NaiveTime::MIN);
        }
        DateColumnKind::DateTime { fraction_digits } => {
            Duration::nanoseconds(10_i64.pow(9 - u32::from(fraction_digits.min(9))))
        }
        DateColumnKind::SqlServerDateTime => Duration::milliseconds(3),
        DateColumnKind::SmallDateTime => Duration::minutes(1),
    };
    end.checked_sub_signed(step).unwrap_or(end)
}

/// The predicate replacing `column <op> <phrase>`: a plain comparison for a
/// point in time, or (with range conversion on) the period expressed for
/// the operator — `=` covers the whole period, `<>` excludes it, `>` / `<=`
/// compare against its last instant and `>=` / `<` against its first.
pub fn predicate(
    column: &str,
    op: CompareOp,
    parsed: &ParsedDate,
    kind: DateColumnKind,
    dialect: Dialect,
    opts: PredicateOptions,
) -> String {
    let start = literal(parsed.start, kind, dialect);
    if !parsed.is_range() || !opts.convert_ranges {
        return format!("{column} {} {start}", op.as_sql());
    }
    let last = literal(inclusive_end(parsed.end, kind), kind, dialect);
    let next = literal(parsed.end, kind, dialect);
    match (op, opts.range_style) {
        (CompareOp::Eq, RangeStyle::Between) => format!("{column} BETWEEN {start} AND {last}"),
        (CompareOp::Eq, RangeStyle::HalfOpen) => {
            format!("({column} >= {start} AND {column} < {next})")
        }
        (CompareOp::Ne, RangeStyle::Between) => {
            format!("{column} NOT BETWEEN {start} AND {last}")
        }
        (CompareOp::Ne, RangeStyle::HalfOpen) => {
            format!("({column} < {start} OR {column} >= {next})")
        }
        (CompareOp::Gt, _) => format!("{column} > {last}"),
        (CompareOp::Le, _) => format!("{column} <= {last}"),
        (CompareOp::Ge, _) => format!("{column} >= {start}"),
        (CompareOp::Lt, _) => format!("{column} < {start}"),
    }
}

/// The marker comment for `phrase`, on one line.
pub fn marker_comment(phrase: &str) -> String {
    let flat: String = phrase
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!("{MARKER_PREFIX}{}", flat.trim())
}

/// The phrase of a marker comment (given from its `--`), tolerating the
/// spacing variants of the `-- sqlly` namespace. `None` for other comments
/// or an empty phrase.
pub fn marker_phrase(comment: &str) -> Option<&str> {
    let body = comment.strip_prefix("--")?.trim_start();
    let rest = body.get(..5).filter(|w| w.eq_ignore_ascii_case("sqlly"))?;
    let rest = body[rest.len()..].trim_start();
    let keyword = rest.get(..5).filter(|w| w.eq_ignore_ascii_case("date:"))?;
    let phrase = rest[keyword.len()..].trim();
    (!phrase.is_empty()).then_some(phrase)
}

/// A predicate written by the date helper, found again by its marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkedPredicate {
    /// What the user typed, from the marker.
    pub phrase: String,
    /// The column expression as written (`o.OrderDate`, `[Order Date]`).
    pub column: String,
    /// The comparison the predicate stands for (`BETWEEN` reads as `=`).
    pub op: CompareOp,
    /// Byte span of the predicate in the scanned text.
    pub span: Range<usize>,
    /// Byte span of the marker comment.
    pub marker: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Word,
    Str,
    Op,
    Open,
    Close,
    Other,
}

/// A line's tokens up to its first line comment, and that comment's start.
fn scan_line(line: &str) -> (Vec<(Tok, Range<usize>)>, Option<usize>) {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let start = i;
        let closing = match b {
            b'\'' => Some(b'\''),
            b'"' => Some(b'"'),
            b'`' => Some(b'`'),
            b'[' => Some(b']'),
            _ => None,
        };
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            return (out, Some(i));
        } else if let Some(close) = closing {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == close {
                    i += 1;
                    if bytes.get(i) != Some(&close) {
                        break;
                    }
                }
                i += 1;
            }
            let kind = if b == b'\'' { Tok::Str } else { Tok::Word };
            out.push((kind, start..i));
            continue;
        } else if b.is_ascii_alphanumeric() || b == b'_' || b == b'@' || b == b'#' || b >= 0x80 {
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'@' | b'#' | b'$')
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            out.push((Tok::Word, start..i));
            continue;
        } else if matches!(b, b'<' | b'>' | b'=' | b'!') {
            i += 1;
            if matches!(bytes.get(i), Some(b'=' | b'>')) && b != b'=' {
                i += 1;
            }
            out.push((Tok::Op, start..i));
            continue;
        }
        let kind = match b {
            b'(' => Tok::Open,
            b')' => Tok::Close,
            _ => Tok::Other,
        };
        i += line[i..].chars().next().map_or(1, char::len_utf8);
        out.push((kind, start..i));
    }
    (out, None)
}

/// Words joined by `.` read as one column path (`dbo.Orders.OrderDate`).
fn merge_paths(line: &str, toks: Vec<(Tok, Range<usize>)>) -> Vec<(Tok, Range<usize>)> {
    let mut out: Vec<(Tok, Range<usize>)> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let (kind, span) = toks[i].clone();
        if kind == Tok::Word {
            let mut end = span.end;
            let mut j = i + 1;
            while j + 1 < toks.len()
                && toks[j].0 == Tok::Other
                && &line[toks[j].1.clone()] == "."
                && toks[j].1.start == end
                && toks[j + 1].0 == Tok::Word
                && toks[j + 1].1.start == toks[j].1.end
            {
                end = toks[j + 1].1.end;
                j += 2;
            }
            out.push((Tok::Word, span.start..end));
            i = j;
        } else {
            out.push((kind, span));
            i += 1;
        }
    }
    out
}

/// Match a literal (`'…'`, or `DATE '…'` / `TIMESTAMP '…'`) ending at token
/// `end` (inclusive); returns the index of its first token.
fn literal_ending_at(line: &str, toks: &[(Tok, Range<usize>)], end: usize) -> Option<usize> {
    if toks.get(end)?.0 != Tok::Str {
        return None;
    }
    if end > 0
        && toks[end - 1].0 == Tok::Word
        && ["DATE", "TIMESTAMP"]
            .iter()
            .any(|k| line[toks[end - 1].1.clone()].eq_ignore_ascii_case(k))
    {
        return Some(end - 1);
    }
    Some(end)
}

/// Whether token `i` is the word `kw` (case-insensitive).
fn is_kw(line: &str, toks: &[(Tok, Range<usize>)], i: usize, kw: &str) -> bool {
    toks.get(i)
        .is_some_and(|(k, s)| *k == Tok::Word && line[s.clone()].eq_ignore_ascii_case(kw))
}

/// The generated predicate ending at the last token, as
/// `(first token index, column token index, op)`.
fn match_predicate(line: &str, toks: &[(Tok, Range<usize>)]) -> Option<(usize, usize, CompareOp)> {
    let last = toks.len().checked_sub(1)?;
    let op_at = |i: usize| -> Option<CompareOp> {
        let (k, s) = toks.get(i)?;
        (*k == Tok::Op)
            .then(|| CompareOp::parse(&line[s.clone()]))
            .flatten()
    };
    let is_col = |i: usize| toks.get(i).is_some_and(|(k, _)| *k == Tok::Word);

    // Parenthesized half-open forms: `( col >= a AND col < b )` and
    // `( col < a OR col >= b )`.
    if toks[last].0 == Tok::Close {
        let b = literal_ending_at(line, toks, last.checked_sub(1)?)?;
        let op2 = op_at(b.checked_sub(1)?)?;
        let col2 = b.checked_sub(2)?;
        let joiner = col2.checked_sub(1)?;
        let a_end = joiner.checked_sub(1)?;
        let a = literal_ending_at(line, toks, a_end)?;
        let op1 = op_at(a.checked_sub(1)?)?;
        let col1 = a.checked_sub(2)?;
        let open = col1.checked_sub(1)?;
        let same_col = is_col(col1)
            && is_col(col2)
            && line[toks[col1].1.clone()] == line[toks[col2].1.clone()];
        if toks[open].0 != Tok::Open || !same_col {
            return None;
        }
        return match (op1, op2) {
            (CompareOp::Ge, CompareOp::Lt) if is_kw(line, toks, joiner, "AND") => {
                Some((open, col1, CompareOp::Eq))
            }
            (CompareOp::Lt, CompareOp::Ge) if is_kw(line, toks, joiner, "OR") => {
                Some((open, col1, CompareOp::Ne))
            }
            _ => None,
        };
    }

    let b = literal_ending_at(line, toks, last)?;
    // `col [NOT] BETWEEN a AND b`
    if b > 0 && is_kw(line, toks, b - 1, "AND") {
        let a = literal_ending_at(line, toks, b.checked_sub(2)?)?;
        let between = a.checked_sub(1)?;
        if is_kw(line, toks, between, "BETWEEN") {
            let (col, op) = if between > 0 && is_kw(line, toks, between - 1, "NOT") {
                (between.checked_sub(2)?, CompareOp::Ne)
            } else {
                (between.checked_sub(1)?, CompareOp::Eq)
            };
            return is_col(col).then_some((col, col, op));
        }
    }
    // `col <op> lit`
    let op = op_at(b.checked_sub(1)?)?;
    let col = b.checked_sub(2)?;
    is_col(col).then_some((col, col, op))
}

/// Every marked predicate in `sql`, in order. A marker whose line no longer
/// ends in a predicate the helper wrote (edited by hand) is skipped.
pub fn find_marked_predicates(sql: &str) -> Vec<MarkedPredicate> {
    let mut out = Vec::new();
    let mut line_start = 0usize;
    for line in sql.split('\n') {
        let (toks, comment) = scan_line(line);
        if let Some(rel) = comment
            && let Some(phrase) = marker_phrase(line[rel..].trim_end_matches('\r'))
        {
            let toks = merge_paths(line, toks);
            if let Some((first, col, op)) = match_predicate(line, &toks) {
                let span = toks[first].1.start..toks[toks.len() - 1].1.end;
                out.push(MarkedPredicate {
                    phrase: phrase.to_string(),
                    column: line[toks[col].1.clone()].to_string(),
                    op,
                    span: line_start + span.start..line_start + span.end,
                    marker: line_start + rel..line_start + line.trim_end_matches('\r').len(),
                });
            }
        }
        line_start += line.len() + 1;
    }
    out
}

/// A comparison the caret sits in: `<column> <op> <phrase so far>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonAt {
    /// The column expression as written.
    pub column: String,
    /// Byte offset of the column expression in the scanned text.
    pub column_start: usize,
    pub op: CompareOp,
    /// Byte offset where the phrase begins (after the operator and any
    /// spaces); the phrase runs from here to the end of the scanned text.
    pub phrase_start: usize,
}

/// Words that end a comparison's value — a phrase never contains them, so
/// finding one means the caret is past the comparison.
const CLAUSE_WORDS: &[&str] = &[
    "where", "having", "then", "when", "else", "end", "order", "group", "select", "join", "union",
    "limit", "or", "is", "like", "in", "not", "null",
];

/// The comparison `line_prefix` (a line's text up to the caret) ends in, if
/// any: the last comparison operator preceded by a column, with nothing but
/// a phrase after it. `None` inside a comment, after a clause keyword, or
/// when the operator has no column before it (`CAST(x AS date) =`).
pub fn comparison_before(line_prefix: &str) -> Option<ComparisonAt> {
    let (toks, comment) = scan_line(line_prefix);
    if comment.is_some() {
        return None;
    }
    let toks = merge_paths(line_prefix, toks);
    let op_idx = toks.iter().rposition(|(k, _)| *k == Tok::Op)?;
    let op = CompareOp::parse(&line_prefix[toks[op_idx].1.clone()])?;
    let clause_word_after = toks[op_idx + 1..].iter().any(|(k, s)| {
        *k == Tok::Word
            && CLAUSE_WORDS
                .iter()
                .any(|w| line_prefix[s.clone()].eq_ignore_ascii_case(w))
    });
    if clause_word_after {
        return None;
    }
    let col = op_idx.checked_sub(1)?;
    let (kind, col_span) = toks[col].clone();
    let col_text = &line_prefix[col_span.clone()];
    let is_keyword = ["and", "or", "not", "where", "on", "having", "between"]
        .iter()
        .any(|w| col_text.eq_ignore_ascii_case(w));
    if kind != Tok::Word || is_keyword {
        return None;
    }
    let op_end = toks[op_idx].1.end;
    let phrase_start = op_end
        + line_prefix[op_end..]
            .bytes()
            .take_while(u8::is_ascii_whitespace)
            .count();
    Some(ComparisonAt {
        column: col_text.to_string(),
        column_start: col_span.start,
        op,
        phrase_start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::natural_date::Granularity;
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
    }

    fn november() -> ParsedDate {
        ParsedDate {
            start: dt(2025, 11, 1, 0, 0, 0),
            end: dt(2025, 12, 1, 0, 0, 0),
            granularity: Granularity::Month,
        }
    }

    fn instant() -> ParsedDate {
        let at = dt(2026, 10, 21, 15, 0, 0);
        ParsedDate {
            start: at,
            end: at,
            granularity: Granularity::Instant,
        }
    }

    const BETWEEN: PredicateOptions = PredicateOptions {
        convert_ranges: true,
        range_style: RangeStyle::Between,
    };
    const HALF_OPEN: PredicateOptions = PredicateOptions {
        convert_ranges: true,
        range_style: RangeStyle::HalfOpen,
    };

    #[test]
    fn column_kinds_follow_type_and_dialect() {
        use DateColumnKind::*;
        let k = |t: &str, d| column_kind(Some(t), d);
        assert_eq!(k("date", Dialect::SqlServer), Date);
        assert_eq!(k("datetime", Dialect::SqlServer), SqlServerDateTime);
        assert_eq!(k("smalldatetime", Dialect::SqlServer), SmallDateTime);
        assert_eq!(
            k("datetime2(3)", Dialect::SqlServer),
            DateTime { fraction_digits: 3 }
        );
        assert_eq!(
            k("datetime2", Dialect::SqlServer),
            DateTime { fraction_digits: 7 }
        );
        assert_eq!(
            k("timestamp(3) without time zone", Dialect::Postgres),
            DateTime { fraction_digits: 3 }
        );
        assert_eq!(
            k("timestamptz", Dialect::Postgres),
            DateTime { fraction_digits: 6 }
        );
        assert_eq!(
            k("datetime", Dialect::MySql),
            DateTime { fraction_digits: 0 }
        );
        assert_eq!(
            k("datetime(6)", Dialect::MySql),
            DateTime { fraction_digits: 6 }
        );
        assert_eq!(k("DATE", Dialect::Oracle), DateTime { fraction_digits: 0 });
        assert_eq!(column_kind(None, Dialect::SqlServer), SqlServerDateTime);
        assert_eq!(
            column_kind(None, Dialect::MySql),
            DateTime { fraction_digits: 0 }
        );
    }

    #[test]
    fn date_types_exclude_time_only_types() {
        for t in [
            "date",
            "datetime2(7)",
            "DATETIMEOFFSET",
            "timestamp with time zone",
            "smalldatetime",
        ] {
            assert!(is_date_type(t), "{t}");
        }
        for t in ["time", "time(7)", "interval", "varchar(20)", "int"] {
            assert!(!is_date_type(t), "{t}");
        }
    }

    #[test]
    fn date_like_names() {
        for n in [
            "OrderDate",
            "created_at",
            "ModifiedOn",
            "ship_dt",
            "EventTime",
            "o.ShipDate",
            "[Due Date]",
            "CreatedAt",
        ] {
            assert!(looks_like_date_column(n), "{n}");
        }
        for n in [
            "Format",
            "Region",
            "Bacon",
            "Status",
            "CustomerID",
            "location",
        ] {
            assert!(!looks_like_date_column(n), "{n}");
        }
    }

    #[test]
    fn literals_per_dialect() {
        let at = dt(2026, 10, 21, 15, 0, 0);
        assert_eq!(
            literal(at, DateColumnKind::Date, Dialect::SqlServer),
            "'2026-10-21'"
        );
        assert_eq!(
            literal(at, DateColumnKind::Date, Dialect::Oracle),
            "DATE '2026-10-21'"
        );
        assert_eq!(
            literal(at, DateColumnKind::SqlServerDateTime, Dialect::SqlServer),
            "'2026-10-21T15:00:00'"
        );
        assert_eq!(
            literal(
                at,
                DateColumnKind::DateTime { fraction_digits: 6 },
                Dialect::Postgres
            ),
            "'2026-10-21 15:00:00'"
        );
        assert_eq!(
            literal(
                at,
                DateColumnKind::DateTime { fraction_digits: 6 },
                Dialect::Oracle
            ),
            "TIMESTAMP '2026-10-21 15:00:00'"
        );
    }

    #[test]
    fn inclusive_ends_stay_inside_the_period() {
        let end = dt(2025, 12, 1, 0, 0, 0);
        let k = |kind| literal(inclusive_end(end, kind), kind, Dialect::SqlServer);
        assert_eq!(k(DateColumnKind::Date), "'2025-11-30'");
        assert_eq!(
            k(DateColumnKind::SqlServerDateTime),
            "'2025-11-30T23:59:59.997'"
        );
        assert_eq!(k(DateColumnKind::SmallDateTime), "'2025-11-30T23:59:00'");
        assert_eq!(
            k(DateColumnKind::DateTime { fraction_digits: 7 }),
            "'2025-11-30T23:59:59.9999999'"
        );
        assert_eq!(
            k(DateColumnKind::DateTime { fraction_digits: 0 }),
            "'2025-11-30T23:59:59'"
        );
    }

    #[test]
    fn periods_become_ranges_by_operator() {
        let kind = DateColumnKind::Date;
        let p = |op, opts| predicate("OrderDate", op, &november(), kind, Dialect::SqlServer, opts);
        assert_eq!(
            p(CompareOp::Eq, BETWEEN),
            "OrderDate BETWEEN '2025-11-01' AND '2025-11-30'"
        );
        assert_eq!(
            p(CompareOp::Ne, BETWEEN),
            "OrderDate NOT BETWEEN '2025-11-01' AND '2025-11-30'"
        );
        assert_eq!(
            p(CompareOp::Eq, HALF_OPEN),
            "(OrderDate >= '2025-11-01' AND OrderDate < '2025-12-01')"
        );
        assert_eq!(
            p(CompareOp::Ne, HALF_OPEN),
            "(OrderDate < '2025-11-01' OR OrderDate >= '2025-12-01')"
        );
        assert_eq!(p(CompareOp::Gt, BETWEEN), "OrderDate > '2025-11-30'");
        assert_eq!(p(CompareOp::Le, BETWEEN), "OrderDate <= '2025-11-30'");
        assert_eq!(p(CompareOp::Ge, BETWEEN), "OrderDate >= '2025-11-01'");
        assert_eq!(p(CompareOp::Lt, BETWEEN), "OrderDate < '2025-11-01'");
        let off = PredicateOptions {
            convert_ranges: false,
            ..BETWEEN
        };
        assert_eq!(p(CompareOp::Eq, off), "OrderDate = '2025-11-01'");
    }

    #[test]
    fn instants_are_plain_comparisons() {
        let kind = DateColumnKind::DateTime { fraction_digits: 6 };
        assert_eq!(
            predicate(
                "created_at",
                CompareOp::Ge,
                &instant(),
                kind,
                Dialect::Postgres,
                BETWEEN
            ),
            "created_at >= '2026-10-21 15:00:00'"
        );
    }

    #[test]
    fn marker_phrases() {
        assert_eq!(
            marker_comment("last wed\nat 3 "),
            "--sqlly date:last wed at 3"
        );
        assert_eq!(marker_phrase("--sqlly date:november"), Some("november"));
        assert_eq!(marker_phrase("-- SQLLY DATE: last week"), Some("last week"));
        assert_eq!(marker_phrase("--sqlly date:"), None);
        assert_eq!(marker_phrase("-- sqlly name 1: x"), None);
    }

    #[test]
    fn finds_every_generated_shape_again() {
        let kind = DateColumnKind::SqlServerDateTime;
        for (op, opts) in [
            (CompareOp::Eq, BETWEEN),
            (CompareOp::Ne, BETWEEN),
            (CompareOp::Eq, HALF_OPEN),
            (CompareOp::Ne, HALF_OPEN),
            (CompareOp::Gt, BETWEEN),
            (CompareOp::Ge, BETWEEN),
            (CompareOp::Lt, BETWEEN),
            (CompareOp::Le, BETWEEN),
        ] {
            for dialect in [Dialect::SqlServer, Dialect::Oracle] {
                let pred = predicate("o.[Order Date]", op, &november(), kind, dialect, opts);
                let sql = format!(
                    "SELECT *\nFROM Orders o\nWHERE o.Status = 'x' AND {pred} {}\n  AND 1 = 1",
                    marker_comment("november")
                );
                let found = find_marked_predicates(&sql);
                assert_eq!(found.len(), 1, "{sql}");
                let m = &found[0];
                assert_eq!(&sql[m.span.clone()], pred, "{sql}");
                assert_eq!(m.column, "o.[Order Date]");
                assert_eq!(m.op, op, "{sql}");
                assert_eq!(m.phrase, "november");
                assert_eq!(&sql[m.marker.clone()], "--sqlly date:november");
            }
        }
    }

    #[test]
    fn comparisons_before_the_caret() {
        let c = comparison_before("WHERE o.OrderDate >= last wed at 3").unwrap();
        assert_eq!(c.column, "o.OrderDate");
        assert_eq!(c.column_start, 6);
        assert_eq!(c.op, CompareOp::Ge);
        assert_eq!(c.phrase_start, 21);
        let c = comparison_before("  AND [Ship Date] = ").unwrap();
        assert_eq!(c.column, "[Ship Date]");
        assert_eq!(c.op, CompareOp::Eq);
        assert_eq!(c.phrase_start, 20);
        let c = comparison_before("WHERE d <> between monday and friday").unwrap();
        assert_eq!(c.op, CompareOp::Ne);
        assert!(comparison_before("WHERE d = 1 OR x").is_none());
        assert!(comparison_before("WHERE CAST(d AS date) = ").is_none());
        assert!(comparison_before("WHERE d = x -- note").is_none());
        assert!(comparison_before("SELECT a, b").is_none());
        assert!(comparison_before("WHERE AND = ").is_none());
    }

    /// Every phrase the docs page (and blog post) promises must parse.
    #[test]
    fn documented_phrases_parse() {
        use crate::natural_date::{DateParseOptions, parse_natural_date};
        let now = dt(2026, 10, 22, 10, 30, 0);
        let opts = DateParseOptions::default();
        for phrase in [
            "now",
            "today",
            "yesterday",
            "tomorrow",
            "the day before yesterday",
            "wed",
            "last friday",
            "next monday",
            "monday last week",
            "at 3",
            "3pm",
            "15:30",
            "noon",
            "midnight",
            "yesterday at noon",
            "tomorrow morning",
            "last night",
            "end of day",
            "this week",
            "last month",
            "next quarter",
            "this year",
            "last weekend",
            "q3 2025",
            "november",
            "nov 2024",
            "2024",
            "ytd",
            "mtd",
            "qtd",
            "wtd",
            "year to date",
            "this month so far",
            "since march",
            "past 7 days",
            "last 3 months",
            "past 24 hours",
            "the last few days",
            "next 2 weeks",
            "3 days ago",
            "a week ago",
            "in 2 weeks",
            "10 days from now",
            "3 days before christmas",
            "nov 5",
            "november 5th 2024",
            "5th of november",
            "the 5th",
            "2024-11-05",
            "11/5/2024",
            "nov 5 at 3:15pm",
            "start of this week",
            "end of last month",
            "first day of next month",
            "last friday of the month",
            "from mon to fri",
            "between jan and mar 2024",
            "jan - mar",
            "oct 1 through oct 15",
            "this fiscal year",
            "last fiscal year",
            "fytd",
            "fy2025",
            "christmas",
            "new year's day",
            "thanksgiving",
            "labor day",
            "memorial day",
            "july 4th",
            "halloween",
            "last wed at 3",
            "past 7 days",
        ] {
            assert!(parse_natural_date(phrase, now, &opts).is_ok(), "{phrase}");
        }
        for phrase in crate::natural_date::example_phrases() {
            assert!(parse_natural_date(phrase, now, &opts).is_ok(), "{phrase}");
        }
    }

    #[test]
    fn hand_edited_lines_and_plain_comments_are_skipped() {
        let sql = "WHERE OrderDate = GETDATE() --sqlly date:today\n\
                   WHERE x = 'a' -- just a note\n\
                   WHERE '--sqlly date:x' = y";
        assert!(find_marked_predicates(sql).is_empty());
    }
}
