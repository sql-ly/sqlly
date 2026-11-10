//! Excel-compatible formulas over result rows — the engine behind
//! `-- sqlly column:` computed columns.
//!
//! The language is Excel's: the same operators and precedence (`-2^2` is
//! 4), the same coercions (blank is 0 in arithmetic and "" in text), the
//! same error values (`#DIV/0!`, `#VALUE!`, …), and the same function names
//! and argument orders. Two things are SQLly's own, because a result grid
//! has named columns instead of lettered ones:
//!
//! - **Cell references** name a column with `!`: `!Total` is this row's
//!   Total, `!Total[1]` the row before, `!Total[-1]` the row after, and
//!   `!Total[$1]` the first row. A name with spaces or punctuation is
//!   quoted: `!"Order Total"[1]`. Two references make a range:
//!   `!Qty[2]:!Qty` is this row and the two before it; `!A:!C` spans the
//!   columns from A to C on this row.
//! - **Variables** declared with `-- sqlly set` or `-- sqlly param:` read as
//!   `@name`.
//!
//! Rows are numbered in the order the query returned them. A reference
//! outside the result (the row before the first) is a blank cell, like an
//! empty cell in Excel. `COALESCE(a, b, …)` is provided as an extension:
//! the first argument that is neither blank nor "".
//!
//! Dates are Excel serial numbers (days since 1899-12-30, time as the
//! fraction), so date arithmetic and the date functions behave as in Excel.

mod eval;
mod excel;
mod functions;
mod lexer;
mod parser;
mod text_format;

pub use eval::{EvalEnv, evaluate};
pub use excel::{ExcelRefContext, to_excel_formula};
pub use functions::FUNCTIONS;
pub use parser::{Expr, parse};
pub use text_format::{format_general, format_text};

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

/// Excel's error values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Null,
    Div0,
    Value,
    Ref,
    Name,
    Num,
    NA,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Null => "#NULL!",
            ErrorKind::Div0 => "#DIV/0!",
            ErrorKind::Value => "#VALUE!",
            ErrorKind::Ref => "#REF!",
            ErrorKind::Name => "#NAME?",
            ErrorKind::Num => "#NUM!",
            ErrorKind::NA => "#N/A",
        }
    }

    fn from_literal(text: &str) -> Option<Self> {
        let upper = text.to_ascii_uppercase();
        [
            ErrorKind::Null,
            ErrorKind::Div0,
            ErrorKind::Value,
            ErrorKind::Ref,
            ErrorKind::Name,
            ErrorKind::Num,
            ErrorKind::NA,
        ]
        .into_iter()
        .find(|e| e.as_str() == upper)
    }
}

/// A formula value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Blank,
    Number(f64),
    Text(String),
    Bool(bool),
    Error(ErrorKind),
    /// A range's cells, row by row.
    Array(Vec<Value>),
}

#[cfg_attr(not(test), allow(dead_code))]
impl Value {
    /// The text Excel shows for this value in a General-formatted cell.
    pub fn display(&self) -> String {
        match self {
            Value::Blank => String::new(),
            Value::Number(n) => format_general(*n),
            Value::Text(t) => t.clone(),
            Value::Bool(true) => "TRUE".into(),
            Value::Bool(false) => "FALSE".into(),
            Value::Error(e) => e.as_str().into(),
            Value::Array(items) => items.first().map(Value::display).unwrap_or_default(),
        }
    }
}

/// Which row a reference reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowRef {
    /// Rows before the current one (`[1]` is the previous row; negative
    /// looks ahead).
    Relative(i64),
    /// A 1-based row of the result (`[$1]` is the first).
    Absolute(i64),
}

/// `!Column[row]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRef {
    pub column: String,
    pub row: RowRef,
}

impl CellRef {
    /// The 0-based result row this reference reads from row `current`.
    pub fn resolve_row(&self, current: i64) -> i64 {
        match self.row {
            RowRef::Relative(back) => current - back,
            RowRef::Absolute(n) => n - 1,
        }
    }
}

/// A token's place in the source, for coloring references the way Excel
/// colors them while you type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSpan {
    pub start: usize,
    pub end: usize,
    /// The column referenced (both ends of a range are separate spans).
    pub column: String,
    pub row: RowRef,
}

/// A parse failure, with the byte offset Excel would put the cursor at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub at: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A parsed formula: the expression plus every reference's source span.
#[derive(Debug, Clone, PartialEq)]
pub struct Formula {
    pub source: String,
    pub expr: Expr,
    pub refs: Vec<RefSpan>,
}

impl Formula {
    /// Every column the formula reads, first mention first.
    pub fn referenced_columns(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in &self.refs {
            if !out.iter().any(|c| c.eq_ignore_ascii_case(&r.column)) {
                out.push(r.column.clone());
            }
        }
        out
    }

    /// Whether any reference reads a row other than the current one (the
    /// value then depends on row order).
    pub fn reads_other_rows(&self) -> bool {
        self.refs
            .iter()
            .any(|r| !matches!(r.row, RowRef::Relative(0)))
            || parser::has_range(&self.expr)
    }
}

// ---------------------------------------------------------------------------
// Dates as Excel serial numbers (1900 date system)
// ---------------------------------------------------------------------------

fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1899, 12, 30).expect("valid epoch")
}

/// The serial number of a date and time.
pub fn serial_from_datetime(dt: NaiveDateTime) -> f64 {
    let days = (dt.date() - epoch()).num_days() as f64;
    let secs =
        dt.time().num_seconds_from_midnight() as f64 + f64::from(dt.time().nanosecond()) / 1e9;
    days + secs / 86_400.0
}

pub fn serial_from_date(date: NaiveDate) -> f64 {
    (date - epoch()).num_days() as f64
}

/// The date and time of a serial number; `None` before 1900 or out of range.
pub fn datetime_from_serial(serial: f64) -> Option<NaiveDateTime> {
    if !serial.is_finite() || serial < 1.0 || serial > 2_958_465.999_999 {
        return None;
    }
    let days = serial.floor();
    let date = epoch().checked_add_signed(chrono::Duration::days(days as i64))?;
    let mut secs = ((serial - days) * 86_400.0).round() as u32;
    let mut date = date;
    if secs >= 86_400 {
        secs -= 86_400;
        date = date.succ_opt()?;
    }
    let time = NaiveTime::from_num_seconds_from_midnight_opt(secs, 0)?;
    Some(NaiveDateTime::new(date, time))
}

/// Read a database date/time text as a serial number: ISO dates, ISO
/// date-times (with `T` or a space, fractional seconds, and an ignored
/// zone suffix), and bare times.
pub fn parse_datetime_text(text: &str) -> Option<f64> {
    let t = text.trim();
    // Strip a trailing zone: Z, +hh:mm, -hh:mm, +hhmm, or " +00:00".
    let core = strip_zone(t);
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(core, fmt) {
            return Some(serial_from_datetime(dt));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(core, "%Y-%m-%d") {
        return Some(serial_from_date(d));
    }
    for fmt in ["%H:%M:%S%.f", "%H:%M"] {
        if let Ok(time) = NaiveTime::parse_from_str(core, fmt) {
            return Some(
                (f64::from(time.num_seconds_from_midnight()) + f64::from(time.nanosecond()) / 1e9)
                    / 86_400.0,
            );
        }
    }
    None
}

fn strip_zone(t: &str) -> &str {
    if let Some(rest) = t.strip_suffix('Z') {
        return rest;
    }
    // Only look for a zone after the time part.
    let Some(time_at) = t.find([' ', 'T']).filter(|i| *i >= 8) else {
        return t;
    };
    let tail = &t[time_at + 1..];
    if let Some(i) = tail.rfind(['+', '-']).filter(|i| *i >= 5) {
        return t[..time_at + 1 + i].trim_end();
    }
    t
}

/// Year, month, day of a serial (for the date functions).
pub(crate) fn ymd(serial: f64) -> Option<(i32, u32, u32)> {
    let dt = datetime_from_serial(serial.max(1.0))?;
    Some((dt.year(), dt.month(), dt.day()))
}

#[cfg(test)]
mod tests;
