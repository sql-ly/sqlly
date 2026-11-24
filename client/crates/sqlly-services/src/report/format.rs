//! Column kinds and value formatting shared by every renderer, so a number
//! reads the same in the HTML table, the PDF and a chart axis, and lands in
//! a spreadsheet as a number with the matching format code.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

/// What a column holds, as the exporters need to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    Integer,
    /// Exact decimals (`decimal`, `numeric`, `money`); often text on the wire.
    Decimal,
    Float,
    Bool,
    Date,
    DateTime,
    Time,
    Text,
    Binary,
    Uuid,
    Json,
}

impl ColumnKind {
    /// The kind of a declared SQL type, by family name: `Nullable(…)`
    /// wrappers and `(…)` size groups are ignored, and a qualified spelling
    /// (`int unsigned`) falls back to its first word. Unknown names are text.
    pub fn infer(data_type: &str) -> ColumnKind {
        let key = type_key(data_type);
        if key.ends_with("[]") || key.starts_with('_') {
            return ColumnKind::Text;
        }
        let lookup = |name: &str| -> Option<ColumnKind> {
            Some(match name {
                "bit" | "bool" | "boolean" => ColumnKind::Bool,
                "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint"
                | "smallserial" | "serial" | "bigserial" | "serial2" | "serial4" | "serial8"
                | "int2" | "int4" | "int8" | "int16" | "int32" | "int64" | "int128" | "uint8"
                | "uint16" | "uint32" | "uint64" | "uint128" | "utinyint" | "usmallint"
                | "uinteger" | "ubigint" | "hugeint" | "uhugeint" | "year" => ColumnKind::Integer,
                "decimal" | "numeric" | "dec" | "fixed" | "money" | "smallmoney" | "number"
                | "decimal32" | "decimal64" | "decimal128" | "decimal256" => ColumnKind::Decimal,
                "float" | "real" | "double" | "double precision" | "float4" | "float8"
                | "float32" | "float64" | "binary_float" | "binary_double" => ColumnKind::Float,
                "date" | "daten" => ColumnKind::Date,
                "datetime"
                | "datetime2"
                | "smalldatetime"
                | "datetimeoffset"
                | "timestamp"
                | "timestamptz"
                | "timestamp with time zone"
                | "timestamp without time zone"
                | "datetimen"
                | "datetimeoffsetn" => ColumnKind::DateTime,
                "time" | "timetz" | "time with time zone" | "time without time zone" | "timen" => {
                    ColumnKind::Time
                }
                "uniqueidentifier" | "uuid" | "guid" => ColumnKind::Uuid,
                "binary" | "varbinary" | "image" | "bytea" | "blob" | "tinyblob" | "mediumblob"
                | "longblob" | "raw" | "long raw" | "rowversion" => ColumnKind::Binary,
                "json" | "jsonb" => ColumnKind::Json,
                _ => return None,
            })
        };
        lookup(&key)
            .or_else(|| key.split_once(' ').and_then(|(first, _)| lookup(first)))
            .unwrap_or(ColumnKind::Text)
    }

    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            ColumnKind::Integer | ColumnKind::Decimal | ColumnKind::Float
        )
    }

    pub fn is_temporal(self) -> bool {
        matches!(
            self,
            ColumnKind::Date | ColumnKind::DateTime | ColumnKind::Time
        )
    }

    /// Right-aligned in tables.
    pub fn is_right_aligned(self) -> bool {
        self.is_numeric()
    }
}

fn type_key(raw: &str) -> String {
    let mut lower = raw.trim().to_lowercase();
    loop {
        let inner = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|p| lower.strip_prefix(p))
            .and_then(|rest| rest.strip_suffix(')'))
            .map(str::to_string);
        match inner {
            Some(inner) => lower = inner.trim().to_string(),
            None => break,
        }
    }
    let mut key = String::with_capacity(lower.len());
    let mut depth = 0usize;
    let mut pending_space = false;
    for c in lower.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            c if c.is_whitespace() => pending_space = !key.is_empty(),
            c => {
                if pending_space {
                    key.push(' ');
                    pending_space = false;
                }
                key.push(c);
            }
        }
    }
    key.replace(" unsigned", "").replace(" zerofill", "")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberStyle {
    #[default]
    Plain,
    /// `0.25` shows as `25%`, as in a spreadsheet.
    Percent,
    Currency,
    /// `1.2K`, `3.4M`; for axes and KPI cards.
    Compact,
}

/// How a column or KPI value is shown. Defaults keep the value as the
/// database sent it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ValueFormat {
    #[serde(default)]
    pub style: NumberStyle,
    /// Fixed fraction digits; `None` keeps the value's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    /// Group thousands (`12,345`).
    #[serde(default)]
    pub thousands: bool,
    /// ISO code (`USD`) or a symbol; used by [`NumberStyle::Currency`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Suffix after the number (`ms`, `GB`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// strftime pattern for dates and times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

impl ValueFormat {
    pub fn decimals(decimals: u8) -> Self {
        Self {
            decimals: Some(decimals),
            ..Default::default()
        }
    }

    pub fn percent(decimals: u8) -> Self {
        Self {
            style: NumberStyle::Percent,
            decimals: Some(decimals),
            ..Default::default()
        }
    }

    pub fn currency(code: impl Into<String>, decimals: u8) -> Self {
        Self {
            style: NumberStyle::Currency,
            decimals: Some(decimals),
            thousands: true,
            currency: Some(code.into()),
            ..Default::default()
        }
    }

    pub fn compact() -> Self {
        Self {
            style: NumberStyle::Compact,
            ..Default::default()
        }
    }

    pub fn is_default(&self) -> bool {
        *self == ValueFormat::default()
    }
}

/// The trimmed text when it is a plain decimal literal (optional sign,
/// digits, optional fraction, optional exponent) that a typed export can
/// write verbatim as a number. Stricter than `f64::from_str`: `inf`, `NaN`,
/// currency symbols, grouping separators and units all stay text. The
/// original digits come back, so a `decimal(38,10)` keeps every digit; a
/// leading `+` is dropped because neither xsd:double nor JSON accepts it.
pub fn numeric_literal(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix(['-', '+']).unwrap_or(trimmed);
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if let Some(exp) = exponent {
        let digits = exp.strip_prefix(['-', '+']).unwrap_or(exp);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(trimmed.strip_prefix('+').unwrap_or(trimmed))
}

/// Significant digits in a numeric literal (leading zeros and the exponent
/// do not count).
pub fn significant_digits(literal: &str) -> usize {
    let mantissa = literal
        .split(['e', 'E'])
        .next()
        .unwrap_or(literal)
        .trim_start_matches(['-', '+']);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let trimmed = digits.trim_start_matches('0');
    let trimmed = if mantissa.contains('.') {
        trimmed.trim_end_matches('0')
    } else {
        trimmed
    };
    trimmed.len().max(1)
}

/// The value of a cell as a number, when the column is numeric (or the
/// cell is typed numeric). Text that is not a plain literal is `None`.
pub fn cell_number(value: &CellValue, kind: ColumnKind) -> Option<f64> {
    match value {
        CellValue::I64(n) => Some(*n as f64),
        CellValue::F64(n) => n.is_finite().then_some(*n),
        CellValue::Bool(b) if kind.is_numeric() => Some(f64::from(u8::from(*b))),
        CellValue::String(s) if kind.is_numeric() => numeric_literal(s)?.parse().ok(),
        _ => None,
    }
}

/// A date, a date-time or a clock time read from a cell's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Temporal {
    Date(NaiveDate),
    DateTime(NaiveDateTime),
    Time(NaiveTime),
}

impl Temporal {
    /// The date-time this value stands for; a time sits on 1970-01-01.
    pub fn as_datetime(self) -> NaiveDateTime {
        match self {
            Temporal::Date(d) => d.and_hms_opt(0, 0, 0).unwrap_or_default(),
            Temporal::DateTime(dt) => dt,
            Temporal::Time(t) => NaiveDate::from_ymd_opt(1970, 1, 1)
                .unwrap_or_default()
                .and_time(t),
        }
    }

    /// Seconds since 1970-01-01 (UTC as written; zones are dropped).
    pub fn timestamp(self) -> f64 {
        let dt = self.as_datetime();
        dt.and_utc().timestamp() as f64 + f64::from(dt.nanosecond()) / 1e9
    }

    /// Excel's serial date: day 1 is 1900-01-01, time as the fraction.
    /// Excel counts a 1900-02-29 that never was, so days before
    /// 1900-03-01 are one lower than a plain day count; `None` before
    /// 1900-01-01, which Excel cannot show as a date.
    pub fn excel_serial(self) -> Option<f64> {
        let day = |y, m, d| {
            NaiveDate::from_ymd_opt(y, m, d)
                .unwrap_or_default()
                .and_hms_opt(0, 0, 0)
                .unwrap_or_default()
        };
        match self {
            Temporal::Time(t) => Some(
                (f64::from(t.num_seconds_from_midnight()) + f64::from(t.nanosecond()) / 1e9)
                    / 86_400.0,
            ),
            other => {
                let dt = other.as_datetime();
                if dt < day(1900, 1, 1) {
                    return None;
                }
                let millis = (dt - day(1899, 12, 30)).num_milliseconds() as f64;
                let serial = millis / 86_400_000.0;
                Some(if dt < day(1900, 3, 1) {
                    serial - 1.0
                } else {
                    serial
                })
            }
        }
    }
}

/// Parses ISO-style dates, date-times (with `T` or a space, optional
/// fraction, an optional trailing zone) and times. The value is the
/// wall-clock time as written; [`parse_temporal_zoned`] also returns the
/// zone's offset.
pub fn parse_temporal(text: &str) -> Option<Temporal> {
    parse_temporal_zoned(text).map(|(t, _)| t)
}

/// [`parse_temporal`] and the UTC offset in seconds the text carried.
/// Dates and date-times go through the analytics parser, so reports and
/// analytics read every timestamp the same way.
pub fn parse_temporal_zoned(text: &str) -> Option<(Temporal, Option<i64>)> {
    let t = text.trim();
    if t.len() < 4 || t.len() > 40 {
        return None;
    }
    if let Some(zoned) = crate::analytics::time::parse_zoned(t) {
        let value = if zoned.date_only {
            Temporal::Date(zoned.local.date())
        } else {
            Temporal::DateTime(zoned.local)
        };
        return Some((value, zoned.offset_seconds));
    }
    for fmt in ["%H:%M:%S%.f", "%H:%M"] {
        if let Ok(time) = NaiveTime::parse_from_str(t, fmt) {
            return Some((Temporal::Time(time), None));
        }
    }
    None
}

/// The instant a cell holds, in UTC when it carried a zone: what charts
/// group and order by, matching the analytics buckets.
pub fn cell_instant(value: &CellValue) -> Option<Temporal> {
    let CellValue::String(s) = value else {
        return None;
    };
    match parse_temporal_zoned(s)? {
        (Temporal::DateTime(local), Some(offset)) => Some(Temporal::DateTime(
            local
                .checked_sub_signed(chrono::Duration::seconds(offset))
                .unwrap_or(local),
        )),
        (other, _) => Some(other),
    }
}

/// The temporal value of a cell in a temporal column.
pub fn cell_temporal(value: &CellValue) -> Option<Temporal> {
    match value {
        CellValue::String(s) => parse_temporal(s),
        _ => None,
    }
}

fn currency_symbol(code: &str) -> &str {
    match code.to_ascii_uppercase().as_str() {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" | "CNY" => "¥",
        "INR" => "₹",
        "KRW" => "₩",
        "BRL" => "R$",
        "RUB" => "₽",
        "CAD" => "CA$",
        "AUD" => "A$",
        _ => code,
    }
}

/// Rounds a plain decimal literal to `decimals` fraction digits, half away
/// from zero, on the digits themselves so no precision is lost.
fn round_literal(literal: &str, decimals: usize) -> String {
    let (negative, body) = match literal.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, literal),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let int_part = if int_part.is_empty() { "0" } else { int_part };
    let mut digits: Vec<u8> = int_part.bytes().map(|b| b - b'0').collect();
    let mut frac: Vec<u8> = frac_part.bytes().map(|b| b - b'0').collect();
    let round_up = frac.get(decimals).is_some_and(|d| *d >= 5);
    frac.resize(decimals, 0);
    digits.extend(frac);
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let mut out: String = digits[..digits.len() - decimals]
        .iter()
        .map(|d| char::from(b'0' + d))
        .collect();
    let out_int = out.trim_start_matches('0');
    out = if out_int.is_empty() {
        "0".into()
    } else {
        out_int.into()
    };
    if decimals > 0 {
        out.push('.');
        out.extend(
            digits[digits.len() - decimals..]
                .iter()
                .map(|d| char::from(b'0' + d)),
        );
    }
    let is_zero = out.bytes().all(|b| b == b'0' || b == b'.');
    if negative && !is_zero {
        out.insert(0, '-');
    }
    out
}

/// Moves the decimal point of a plain literal two places right (×100).
fn shift_percent(literal: &str) -> String {
    let (negative, body) = match literal.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, literal),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let mut frac: String = frac_part.to_string();
    while frac.len() < 2 {
        frac.push('0');
    }
    let int = format!("{int_part}{}", &frac[..2]);
    let int = int.trim_start_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    let rest = &frac[2..];
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(int);
    if !rest.is_empty() {
        out.push('.');
        out.push_str(rest);
    }
    out
}

fn group_thousands(literal: &str) -> String {
    let (sign, body) = match literal.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", literal),
    };
    let (int_part, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    let mut grouped = String::new();
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

/// The shortest text that reads back as `n`, without an exponent for
/// ordinary magnitudes.
pub fn float_literal(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let text = format!("{n}");
    if text.contains('e') {
        return format!("{n:e}");
    }
    text
}

/// `n` rounded to 15 significant digits, as a spreadsheet shows it, so
/// binary noise (`1194.0500000000002`) does not reach a reader. Exports
/// that carry the value itself (CSV, XLSX) keep every bit.
pub fn display_precision(n: f64) -> f64 {
    if !n.is_finite() || n == 0.0 {
        return n;
    }
    format!("{n:.14e}").parse().unwrap_or(n)
}

fn compact(n: f64, decimals: Option<u8>) -> String {
    let abs = n.abs();
    let (scaled, suffix) = if abs >= 1e12 {
        (n / 1e12, "T")
    } else if abs >= 1e9 {
        (n / 1e9, "B")
    } else if abs >= 1e6 {
        (n / 1e6, "M")
    } else if abs >= 1e3 {
        (n / 1e3, "K")
    } else {
        (n, "")
    };
    let digits = usize::from(decimals.unwrap_or(
        if scaled.abs() >= 100.0 || suffix.is_empty() && scaled.fract() == 0.0 {
            0
        } else {
            1
        },
    ));
    let shortest = float_literal(scaled);
    let mut text = if shortest.contains('e') {
        format!("{scaled:.digits$}")
    } else {
        round_literal(&shortest, digits)
    };
    if decimals.is_none() && text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if text == "-0" {
        text = "0".into();
    }
    format!("{text}{suffix}")
}

/// Formats a number given as a plain literal (exact digits) with `format`.
pub fn format_literal(literal: &str, format: &ValueFormat) -> String {
    if format.style == NumberStyle::Compact {
        return match literal.parse::<f64>() {
            Ok(n) => with_unit(compact(n, format.decimals), format),
            Err(_) => literal.to_string(),
        };
    }
    if literal.contains(['e', 'E']) {
        return match literal.parse::<f64>() {
            Ok(n) => format_number(n, format),
            Err(_) => literal.to_string(),
        };
    }
    let mut text = if format.style == NumberStyle::Percent {
        shift_percent(literal)
    } else {
        literal.to_string()
    };
    if let Some(d) = format.decimals {
        text = round_literal(&text, usize::from(d));
    }
    if format.thousands {
        text = group_thousands(&text);
    }
    finish(text, format)
}

fn finish(text: String, format: &ValueFormat) -> String {
    let text = match format.style {
        NumberStyle::Percent => format!("{text}%"),
        NumberStyle::Currency => {
            let symbol = currency_symbol(format.currency.as_deref().unwrap_or("USD"));
            match text.strip_prefix('-') {
                Some(rest) => format!("-{symbol}{rest}"),
                None => format!("{symbol}{text}"),
            }
        }
        _ => text,
    };
    with_unit(text, format)
}

fn with_unit(text: String, format: &ValueFormat) -> String {
    match format.unit.as_deref().filter(|u| !u.is_empty()) {
        Some(unit) => format!("{text} {unit}"),
        None => text,
    }
}

/// Formats a float with `format`.
pub fn format_number(n: f64, format: &ValueFormat) -> String {
    if !n.is_finite() {
        return if n.is_nan() {
            "NaN".into()
        } else if n > 0.0 {
            "∞".into()
        } else {
            "-∞".into()
        };
    }
    if format.style == NumberStyle::Compact {
        return with_unit(compact(n, format.decimals), format);
    }
    let value = if format.style == NumberStyle::Percent {
        n * 100.0
    } else {
        n
    };
    let shortest = float_literal(display_precision(value));
    let mut text = match format.decimals {
        Some(d) if !shortest.contains('e') => round_literal(&shortest, usize::from(d)),
        Some(d) => format!("{value:.*}", usize::from(d)),
        None => shortest,
    };
    if text.contains('e') {
        return finish(text, format);
    }
    if text.starts_with('-') && text[1..].bytes().all(|b| b == b'0' || b == b'.') {
        text.remove(0);
    }
    if format.thousands {
        text = group_thousands(&text);
    }
    finish(text, format)
}

/// Formats an integer with `format` (exact digits).
pub fn format_integer(n: i64, format: &ValueFormat) -> String {
    format_literal(&n.to_string(), format)
}

pub fn format_temporal(value: Temporal, format: &ValueFormat) -> String {
    let pattern = format.date.as_deref();
    match (value, pattern) {
        (_, Some(p)) if !p.is_empty() && valid_strftime(p) => match value {
            Temporal::Date(d) => d.format(p).to_string(),
            Temporal::DateTime(dt) => dt.format(p).to_string(),
            Temporal::Time(t) => t.format(p).to_string(),
        },
        (Temporal::Date(d), _) => d.format("%Y-%m-%d").to_string(),
        (Temporal::DateTime(dt), _) => {
            if dt.nanosecond() == 0 {
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            } else {
                dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
            }
        }
        (Temporal::Time(t), _) => t.format("%H:%M:%S").to_string(),
    }
}

/// Only conversion specifiers chrono knows, so a bad pattern cannot panic
/// the formatter.
fn valid_strftime(pattern: &str) -> bool {
    use chrono::format::{Item, StrftimeItems};
    StrftimeItems::new(pattern).all(|item| !matches!(item, Item::Error))
}

pub fn hex_bytes(bytes: &[u8], max: usize) -> String {
    let mut text = String::with_capacity(2 + bytes.len().min(max) * 2);
    text.push_str("0x");
    for b in bytes.iter().take(max) {
        text.push_str(&format!("{b:02X}"));
    }
    if bytes.len() > max {
        text.push('…');
    }
    text
}

/// A cell as display text for tables, labels and CSV-free text formats.
/// Null is the empty string; callers that show a placeholder pick their own.
pub fn format_cell(value: &CellValue, kind: ColumnKind, format: &ValueFormat) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bool(b) => if *b { "true" } else { "false" }.into(),
        CellValue::I64(n) => format_integer(*n, format),
        CellValue::F64(n) => format_number(*n, format),
        CellValue::Bytes(b) => hex_bytes(b, 32),
        CellValue::String(s) => {
            if kind.is_numeric()
                && let Some(literal) = numeric_literal(s)
            {
                return format_literal(literal, format);
            }
            if kind.is_temporal()
                && (format.date.is_some())
                && let Some(t) = parse_temporal(s)
            {
                return format_temporal(t, format);
            }
            s.clone()
        }
    }
}

/// The Excel number format code for a column, when it needs one.
pub fn excel_format(format: &ValueFormat, kind: ColumnKind) -> Option<String> {
    if kind.is_temporal() {
        if let Some(p) = format.date.as_deref().filter(|p| !p.is_empty()) {
            return Some(strftime_to_excel(p));
        }
        return Some(
            match kind {
                ColumnKind::Date => "yyyy-mm-dd",
                ColumnKind::Time => "hh:mm:ss",
                _ => "yyyy-mm-dd hh:mm:ss",
            }
            .into(),
        );
    }
    if !kind.is_numeric() && !matches!(kind, ColumnKind::Text) {
        return None;
    }
    if format.is_default() {
        return None;
    }
    let mut code = if format.thousands {
        "#,##0".to_string()
    } else {
        "0".to_string()
    };
    match format.decimals {
        Some(d) if d > 0 => {
            code.push('.');
            code.push_str(&"0".repeat(usize::from(d)));
        }
        Some(_) => {}
        None if format.style != NumberStyle::Plain => {}
        None => {
            code = if format.thousands {
                "#,##0.##########".into()
            } else {
                "General".into()
            }
        }
    }
    let code = match format.style {
        NumberStyle::Percent => format!("{code}%"),
        NumberStyle::Currency => {
            let symbol = currency_symbol(format.currency.as_deref().unwrap_or("USD"));
            format!("\"{}\"{code}", symbol.replace('"', ""))
        }
        _ => code,
    };
    Some(match format.unit.as_deref().filter(|u| !u.is_empty()) {
        Some(unit) => format!("{code}\" {}\"", unit.replace('"', "")),
        None => code,
    })
}

/// A strftime pattern as an Excel format code.
pub fn strftime_to_excel(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            if c.is_ascii_alphabetic() {
                out.push('\\');
            }
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str("yyyy"),
            Some('y') => out.push_str("yy"),
            Some('m') => out.push_str("mm"),
            Some('b') | Some('h') => out.push_str("mmm"),
            Some('B') => out.push_str("mmmm"),
            Some('d') => out.push_str("dd"),
            Some('e') => out.push('d'),
            Some('a') => out.push_str("ddd"),
            Some('A') => out.push_str("dddd"),
            Some('H') | Some('I') => out.push_str("hh"),
            Some('M') => out.push_str("mm"),
            Some('S') => out.push_str("ss"),
            Some('p') => out.push_str("AM/PM"),
            Some('F') => out.push_str("yyyy-mm-dd"),
            Some('T') => out.push_str("hh:mm:ss"),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// A label for a date bucket: `2026`, `2026-Q4`, `2026-11`, `2026-W47`,
/// `2026-11-18`, `2026-11-18 14:00`.
pub fn grain_label(dt: NaiveDateTime, grain: super::chart::DateGrain) -> String {
    use super::chart::DateGrain as G;
    match grain {
        G::Year => format!("{}", dt.year()),
        G::Quarter => format!("{}-Q{}", dt.year(), (dt.month() - 1) / 3 + 1),
        G::Month => format!("{}-{:02}", dt.year(), dt.month()),
        G::Week => {
            let week = dt.iso_week();
            format!("{}-W{:02}", week.year(), week.week())
        }
        G::Day => dt.format("%Y-%m-%d").to_string(),
        G::Hour => dt.format("%Y-%m-%d %H:00").to_string(),
    }
}

/// The start of the bucket `dt` falls in.
pub fn truncate_to_grain(dt: NaiveDateTime, grain: super::chart::DateGrain) -> NaiveDateTime {
    use super::chart::DateGrain as G;
    let date = dt.date();
    let start = match grain {
        G::Year => NaiveDate::from_ymd_opt(date.year(), 1, 1),
        G::Quarter => NaiveDate::from_ymd_opt(date.year(), (date.month() - 1) / 3 * 3 + 1, 1),
        G::Month => NaiveDate::from_ymd_opt(date.year(), date.month(), 1),
        G::Week => date.checked_sub_days(chrono::Days::new(u64::from(
            date.weekday().num_days_from_monday(),
        ))),
        G::Day => Some(date),
        G::Hour => return date.and_hms_opt(dt.hour(), 0, 0).unwrap_or(dt),
    };
    start.and_then(|d| d.and_hms_opt(0, 0, 0)).unwrap_or(dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_follow_the_type_family() {
        assert_eq!(ColumnKind::infer("decimal(38, 10)"), ColumnKind::Decimal);
        assert_eq!(ColumnKind::infer("Nullable(Int64)"), ColumnKind::Integer);
        assert_eq!(ColumnKind::infer("int unsigned"), ColumnKind::Integer);
        assert_eq!(
            ColumnKind::infer("timestamp with time zone"),
            ColumnKind::DateTime
        );
        assert_eq!(ColumnKind::infer("varchar(10)"), ColumnKind::Text);
        assert_eq!(ColumnKind::infer("integer[]"), ColumnKind::Text);
        assert_eq!(ColumnKind::infer("double precision"), ColumnKind::Float);
        assert_eq!(ColumnKind::infer("money"), ColumnKind::Decimal);
        assert_eq!(ColumnKind::infer("uniqueidentifier"), ColumnKind::Uuid);
    }

    #[test]
    fn literals_are_strict() {
        assert_eq!(numeric_literal(" +12.50 "), Some("12.50"));
        assert_eq!(numeric_literal("-1e-5"), Some("-1e-5"));
        for bad in ["", "NaN", "inf", "1,234", "$1", "1.2.3", "12 kg", "e5"] {
            assert_eq!(numeric_literal(bad), None, "{bad}");
        }
    }

    #[test]
    fn decimals_round_on_the_digits() {
        let f = ValueFormat::decimals(2);
        assert_eq!(
            format_literal("12345678901234567890.125", &f),
            "12345678901234567890.13"
        );
        assert_eq!(format_literal("0.995", &f), "1.00");
        assert_eq!(format_literal("-0.001", &f), "0.00");
        assert_eq!(format_literal("9.999", &ValueFormat::decimals(0)), "10");
        assert_eq!(format_literal("-12.5", &ValueFormat::decimals(0)), "-13");
    }

    #[test]
    fn styles_render_like_a_spreadsheet() {
        assert_eq!(format_literal("0.256", &ValueFormat::percent(1)), "25.6%");
        assert_eq!(
            format_literal("-1234567.5", &ValueFormat::currency("USD", 2)),
            "-$1,234,567.50"
        );
        assert_eq!(format_number(0.125, &ValueFormat::percent(0)), "13%");
        assert_eq!(format_number(1_250_000.0, &ValueFormat::compact()), "1.3M");
        assert_eq!(format_number(950.0, &ValueFormat::compact()), "950");
        assert_eq!(
            format_number(1085.5 * 1.1, &ValueFormat::default()),
            "1194.05"
        );
        assert_eq!(
            format_integer(
                -1234,
                &ValueFormat {
                    thousands: true,
                    ..Default::default()
                }
            ),
            "-1,234"
        );
        let unit = ValueFormat {
            unit: Some("ms".into()),
            decimals: Some(1),
            ..Default::default()
        };
        assert_eq!(format_number(12.34, &unit), "12.3 ms");
    }

    #[test]
    fn large_integers_keep_every_digit() {
        assert_eq!(
            format_integer(i64::MAX, &ValueFormat::default()),
            "9223372036854775807"
        );
        let cell = CellValue::String("123456789012345678901234567890".into());
        assert_eq!(
            format_cell(&cell, ColumnKind::Decimal, &ValueFormat::default()),
            "123456789012345678901234567890"
        );
    }

    #[test]
    fn temporal_text_parses_and_maps_to_excel_serials() {
        let d = parse_temporal("2026-11-18").unwrap();
        assert_eq!(d.excel_serial(), Some(46344.0));
        let dt = parse_temporal("2026-11-18T12:00:00Z").unwrap();
        assert_eq!(dt.excel_serial(), Some(46344.5));
        let t = parse_temporal("06:00").unwrap();
        assert_eq!(t.excel_serial(), Some(0.25));
        // Excel's day 1 is 1900-01-01 and it counts a fake 1900-02-29.
        let serial = |text: &str| parse_temporal(text).unwrap().excel_serial();
        assert_eq!(serial("1900-01-01"), Some(1.0));
        assert_eq!(serial("1900-02-28"), Some(59.0));
        assert_eq!(serial("1900-03-01"), Some(61.0));
        assert_eq!(serial("1899-12-31"), None);
        assert!(parse_temporal("next tuesday").is_none());
    }

    #[test]
    fn excel_codes_match_the_format() {
        assert_eq!(
            excel_format(&ValueFormat::currency("EUR", 2), ColumnKind::Decimal).unwrap(),
            "\"€\"#,##0.00"
        );
        assert_eq!(
            excel_format(&ValueFormat::percent(1), ColumnKind::Float).unwrap(),
            "0.0%"
        );
        assert_eq!(
            excel_format(&ValueFormat::default(), ColumnKind::Date).unwrap(),
            "yyyy-mm-dd"
        );
        assert_eq!(
            excel_format(&ValueFormat::default(), ColumnKind::Integer),
            None
        );
        assert_eq!(strftime_to_excel("%d %b %Y"), "dd mmm yyyy");
    }

    #[test]
    fn bad_date_patterns_fall_back_instead_of_panicking() {
        let f = ValueFormat {
            date: Some("%Q %".into()),
            ..Default::default()
        };
        let d = parse_temporal("2026-01-02").unwrap();
        assert_eq!(format_temporal(d, &f), "2026-01-02");
    }

    #[test]
    fn significant_digits_ignore_padding() {
        assert_eq!(significant_digits("0.000123"), 3);
        assert_eq!(significant_digits("1234567890123456"), 16);
        assert_eq!(significant_digits("-12.3400"), 4);
    }
}
