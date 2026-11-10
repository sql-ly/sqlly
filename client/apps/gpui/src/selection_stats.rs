//! Selection-aggregate footer text for the result grid.
//!
//! Port of `ResultSelectionStatsFormatter.swift`. Builds the
//! "Count / Sum / Avg / Min / Max / Distinct" (numeric), "Count / Min / Max /
//! Distinct" (date), or "Count / Lengths / Distinct" (text-length) footer using
//! the same integer and decimal formatting options the result grid applies to
//! its cells. Pure and unit-tested (per AGENTS.md test mandate).

use sqlly_core::CellValue;

use crate::preferences::{
    DateFormat, DecimalFormat, IntegerFormat, NegativeFormatStyle, ResultFormattingPreferences,
    ResultKind, ThousandsSeparator,
};

/// Maximum number of cells the selection-stats footer will process.
/// Matches Swift's `selectionStatsCellCap = 200_000`. Larger selections
/// are truncated and the footer notes the cap.
pub const SELECTION_STATS_CELL_CAP: usize = 200_000;

/// Render the footer text for a selection, or `None` when there is nothing
/// meaningful to show (no non-null cells). Dispatches on `kind`.
/// When `values.len()` exceeds `SELECTION_STATS_CELL_CAP`, only the first
/// `SELECTION_STATS_CELL_CAP` values are processed and a " (capped at N
/// cells)" suffix is appended.
pub fn footer(
    values: &[CellValue],
    kind: ResultKind,
    prefs: &ResultFormattingPreferences,
) -> Option<String> {
    let (values_slice, capped) = if values.len() > SELECTION_STATS_CELL_CAP {
        (&values[..SELECTION_STATS_CELL_CAP], true)
    } else {
        (values, false)
    };

    let int_fmt = &prefs.defaults.integer_format;
    let dec_fmt = &prefs.defaults.decimal_format;
    let date_fmt = &prefs.defaults.date_format;
    let mut text = match kind {
        ResultKind::Integer | ResultKind::Decimal | ResultKind::Bool | ResultKind::Number => {
            numeric_footer(values_slice, kind, int_fmt, dec_fmt)?
        }
        ResultKind::Date => date_footer(values_slice, date_fmt, int_fmt)?,
        ResultKind::Text
        | ResultKind::Uuid
        | ResultKind::Mask
        | ResultKind::JsonPath
        | ResultKind::Template => text_length_footer(values_slice, int_fmt)?,
        ResultKind::Epoch => numeric_footer(values_slice, kind, int_fmt, dec_fmt)
            .or_else(|| text_length_footer(values_slice, int_fmt))?,
    };
    if capped {
        text.push_str(&crate::localization::format(
            "results.selection_stats.cell_cap_suffix",
            &[("cell_cap", &SELECTION_STATS_CELL_CAP.to_string())],
        ));
    }
    Some(text)
}

/// Numeric footer: "Count: N   Sum: X   Avg: X   Min: X   Max: X   Distinct: N".
/// Returns `None` when there are no non-null cells.
pub fn numeric_footer(
    values: &[CellValue],
    kind: ResultKind,
    int_fmt: &IntegerFormat,
    dec_fmt: &DecimalFormat,
) -> Option<String> {
    let mut count = 0usize;
    let mut distinct = std::collections::HashSet::new();
    let mut sum = 0.0f64;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;

    for value in values {
        if matches!(value, CellValue::Null) {
            continue;
        }
        let num = cell_to_f64(value)?;
        count += 1;
        distinct.insert(num.to_bits());
        sum += num;
        if num < min {
            min = num;
        }
        if num > max {
            max = num;
        }
    }

    if count == 0 {
        return None;
    }

    let avg = sum / count as f64;
    let metric = |key, value: String| crate::localization::format(key, &[("value", &value)]);
    let mut parts = vec![metric(
        "results.selection_stats.numeric_count",
        format_int(count as i64, int_fmt),
    )];

    // Sum only for non-bool kinds (bool sum is nonsensical).
    if kind != ResultKind::Bool {
        parts.push(metric(
            "results.selection_stats.numeric_sum",
            format_numeric(sum, kind, int_fmt, dec_fmt),
        ));
    }
    parts.push(metric(
        "results.selection_stats.numeric_average",
        format_average(avg, kind, int_fmt, dec_fmt),
    ));
    parts.push(metric(
        "results.selection_stats.numeric_minimum",
        format_numeric(min, kind, int_fmt, dec_fmt),
    ));
    parts.push(metric(
        "results.selection_stats.numeric_maximum",
        format_numeric(max, kind, int_fmt, dec_fmt),
    ));
    parts.push(metric(
        "results.selection_stats.numeric_distinct",
        format_int(distinct.len() as i64, int_fmt),
    ));
    Some(parts.join("   "))
}

/// Date footer: "Count: N   Min: D   Max: D   Distinct: N". Values are
/// compared lexically (the grid stores display text). Returns `None` when
/// there are no non-null cells.
pub fn date_footer(
    values: &[CellValue],
    _date_fmt: &DateFormat,
    int_fmt: &IntegerFormat,
) -> Option<String> {
    let mut count = 0usize;
    let mut distinct = std::collections::HashSet::new();
    let mut min: Option<String> = None;
    let mut max: Option<String> = None;

    for value in values {
        if matches!(value, CellValue::Null) {
            continue;
        }
        count += 1;
        let text = cell_display_text(value);
        distinct.insert(text.clone());
        if min.as_ref().is_none_or(|m| &text < m) {
            min = Some(text.clone());
        }
        if max.as_ref().is_none_or(|m| text.as_str() > m.as_str()) {
            max = Some(text);
        }
    }

    if count == 0 {
        return None;
    }

    let metric = |key, value: String| crate::localization::format(key, &[("value", &value)]);
    let mut parts = vec![metric(
        "results.selection_stats.date_count",
        format_int(count as i64, int_fmt),
    )];
    if let Some(mn) = &min {
        parts.push(metric("results.selection_stats.date_minimum", mn.clone()));
    }
    if let Some(mx) = &max {
        parts.push(metric("results.selection_stats.date_maximum", mx.clone()));
    }
    parts.push(metric(
        "results.selection_stats.date_distinct",
        format_int(distinct.len() as i64, int_fmt),
    ));
    Some(parts.join("   "))
}

/// Text-length footer: "Count: N   Lengths: min/avg/max   Distinct: N".
/// Falls back here for non-numeric selections. Returns `None` when there are
/// no non-null cells.
pub fn text_length_footer(values: &[CellValue], int_fmt: &IntegerFormat) -> Option<String> {
    let mut count = 0usize;
    let mut distinct = std::collections::HashSet::new();
    let mut min_len = usize::MAX;
    let mut max_len = 0usize;
    let mut total_len = 0usize;

    for value in values {
        if matches!(value, CellValue::Null) {
            continue;
        }
        count += 1;
        let text = cell_display_text(value);
        let len = text.chars().count();
        distinct.insert(text);
        total_len += len;
        if len < min_len {
            min_len = len;
        }
        if len > max_len {
            max_len = len;
        }
    }

    if count == 0 {
        return None;
    }

    let avg_len = total_len as f64 / count as f64;
    let avg_display = format_trimmed_decimal(avg_len);
    let metric = |key, arguments: &[(&str, &str)]| crate::localization::format(key, arguments);
    let count = format_int(count as i64, int_fmt);
    let mut parts = vec![metric(
        "results.selection_stats.text_count",
        &[("value", &count)],
    )];
    parts.push(metric(
        "results.selection_stats.text_lengths",
        &[
            ("minimum", &format_int(min_len as i64, int_fmt)),
            ("average", &avg_display),
            ("maximum", &format_int(max_len as i64, int_fmt)),
        ],
    ));
    let distinct = format_int(distinct.len() as i64, int_fmt);
    parts.push(metric(
        "results.selection_stats.text_distinct",
        &[("value", &distinct)],
    ));
    Some(parts.join("   "))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cell_to_f64(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        CellValue::I64(v) => Some(*v as f64),
        CellValue::F64(v) => Some(*v),
        CellValue::String(s) => s.trim().parse::<f64>().ok(),
        CellValue::Null => None,
        CellValue::Bytes(_) => None,
    }
}

fn cell_display_text(value: &CellValue) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(v) => v.to_string(),
        CellValue::F64(v) => v.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => crate::localization::format(
            "results.selection_stats.binary_value",
            &[("byte_count", &b.len().to_string())],
        ),
    }
}

/// Format an integer through `IntegerFormat` (reuses the logic from
/// `ResultFormattingPreferences` without needing a full prefs instance).
fn format_int(value: i64, fmt: &IntegerFormat) -> String {
    if value < 0 {
        let body = format_unsigned(value.unsigned_abs(), fmt);
        match fmt.negative_format {
            NegativeFormatStyle::Sign => format!("-{}", body),
            NegativeFormatStyle::Parentheses => format!("({})", body),
        }
    } else {
        format_unsigned(value as u64, fmt)
    }
}

fn format_unsigned(value: u64, fmt: &IntegerFormat) -> String {
    if fmt.use_thousands_separators {
        insert_thousands(value.to_string(), ThousandsSeparator::Comma.literal())
    } else {
        value.to_string()
    }
}

/// Sum / Min / Max rendering: integer formatter for an integer-kind selection
/// when the value is integral and fits `i64`; decimal formatter otherwise.
fn format_numeric(
    value: f64,
    kind: ResultKind,
    int_fmt: &IntegerFormat,
    dec_fmt: &DecimalFormat,
) -> String {
    if kind == ResultKind::Integer && value == value.round() && value.abs() < 9.0e18 {
        return format_int(value as i64, int_fmt);
    }
    format_decimal(value, dec_fmt)
}

/// Average rendering: full decimal format for a decimal-kind selection. For an
/// integer-kind selection it borrows the integer format's grouping and negative
/// styling while trimming trailing zeros so a whole-number mean reads as `6` and
/// a half reads as `5.5`. Metric reduction is intentionally not applied.
fn format_average(
    value: f64,
    kind: ResultKind,
    int_fmt: &IntegerFormat,
    dec_fmt: &DecimalFormat,
) -> String {
    if kind != ResultKind::Integer {
        return format_decimal(value, dec_fmt);
    }
    let trimmed = DecimalFormat {
        decimal_places: significant_decimal_places(value, dec_fmt.decimal_places),
        use_thousands_separators: int_fmt.use_thousands_separators,
        decimal_separator: crate::preferences::DecimalSeparator::Period,
        thousands_separator: ThousandsSeparator::Comma,
        reduce_large_to_metric: false,
        negative_format: int_fmt.negative_format,
        show_red_for_negative: int_fmt.show_red_for_negative,
        ..dec_fmt.clone()
    };
    format_decimal(value, &trimmed)
}

fn format_decimal(value: f64, fmt: &DecimalFormat) -> String {
    let abs_val = value.abs();
    let dec_sep = crate::preferences::DecimalSeparator::Period.literal();
    let thou_sep = if fmt.use_thousands_separators {
        fmt.thousands_separator.literal()
    } else {
        ""
    };

    let formatted = format!("{:.*}", fmt.decimal_places as usize, abs_val);
    let parts: Vec<&str> = formatted.split('.').collect();
    let int_part = if fmt.use_thousands_separators {
        insert_thousands(parts[0].to_string(), thou_sep)
    } else {
        parts[0].to_string()
    };
    let frac_part = if parts.len() > 1 {
        parts[1].replace('.', dec_sep)
    } else {
        String::new()
    };

    let body = if frac_part.is_empty() {
        int_part
    } else {
        format!("{}{}{}", int_part, dec_sep, frac_part)
    };

    if value < 0.0 {
        match fmt.negative_format {
            NegativeFormatStyle::Sign => format!("-{}", body),
            NegativeFormatStyle::Parentheses => format!("({})", body),
        }
    } else {
        body
    }
}

/// Smallest number of fractional digits (0..=max) that represents `value`
/// exactly within a small tolerance.
fn significant_decimal_places(value: f64, max: i32) -> i32 {
    if max <= 0 {
        return 0;
    }
    let abs_value = value.abs();
    for places in 0..max {
        let scaled = abs_value * 10f64.powi(places);
        if (scaled - scaled.round()).abs() < 1e-9 {
            return places;
        }
    }
    max
}

/// Plain trimmed decimal: trims trailing zeros and the decimal point for
/// whole numbers. Used for the text-length average where the value is not
/// column data.
fn format_trimmed_decimal(value: f64) -> String {
    let formatted = format!("{:.6}", value);
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn insert_thousands(s: String, sep: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::new();
    let len = chars.len();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            result.push_str(sep);
        }
        result.push(*c);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v_i(n: i64) -> CellValue {
        CellValue::I64(n)
    }
    fn v_f(n: f64) -> CellValue {
        CellValue::F64(n)
    }
    fn v_s(s: &str) -> CellValue {
        CellValue::String(s.to_string())
    }

    #[test]
    fn numeric_footer_basic() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![v_i(1), v_i(2), v_i(3)];
        let out = numeric_footer(&vals, ResultKind::Integer, &int_fmt, &dec_fmt).unwrap();
        assert!(out.contains("Count: 3"));
        assert!(out.contains("Sum: 6"));
        assert!(out.contains("Avg: 2"));
        assert!(out.contains("Min: 1"));
        assert!(out.contains("Max: 3"));
        assert!(out.contains("Distinct: 3"));
    }

    #[test]
    fn numeric_footer_empty_returns_none() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![CellValue::Null, CellValue::Null];
        assert!(numeric_footer(&vals, ResultKind::Integer, &int_fmt, &dec_fmt).is_none());
    }

    #[test]
    fn numeric_footer_skips_nulls() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![v_i(10), CellValue::Null, v_i(20)];
        let out = numeric_footer(&vals, ResultKind::Integer, &int_fmt, &dec_fmt).unwrap();
        assert!(out.contains("Count: 2"));
        assert!(out.contains("Sum: 30"));
    }

    #[test]
    fn numeric_footer_distinct_collapses_duplicates() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![v_i(5), v_i(5), v_i(5)];
        let out = numeric_footer(&vals, ResultKind::Integer, &int_fmt, &dec_fmt).unwrap();
        assert!(out.contains("Distinct: 1"));
    }

    #[test]
    fn numeric_footer_bool_omits_sum() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![CellValue::Bool(true), CellValue::Bool(false)];
        let out = numeric_footer(&vals, ResultKind::Bool, &int_fmt, &dec_fmt).unwrap();
        assert!(!out.contains("Sum:"));
        assert!(out.contains("Count: 2"));
    }

    #[test]
    fn numeric_footer_decimal_uses_decimal_format() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat {
            decimal_places: 2,
            ..Default::default()
        };
        let vals = vec![v_f(1.5), v_f(2.5)];
        let out = numeric_footer(&vals, ResultKind::Decimal, &int_fmt, &dec_fmt).unwrap();
        assert!(out.contains("Sum: 4.00"));
        assert!(out.contains("Avg: 2.00"));
    }

    #[test]
    fn numeric_footer_integer_avg_trims_zeros() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![v_i(2), v_i(4), v_i(6)];
        let out = numeric_footer(&vals, ResultKind::Integer, &int_fmt, &dec_fmt).unwrap();
        assert!(out.contains("Avg: 4"));
        assert!(!out.contains("Avg: 4.000000"));
    }

    #[test]
    fn numeric_footer_integer_avg_fractional() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![v_i(1), v_i(2)];
        let out = numeric_footer(&vals, ResultKind::Integer, &int_fmt, &dec_fmt).unwrap();
        assert!(out.contains("Avg: 1.5"));
    }

    #[test]
    fn numeric_footer_parses_string_numbers() {
        let int_fmt = IntegerFormat::default();
        let dec_fmt = DecimalFormat::default();
        let vals = vec![v_s("42"), v_s("8")];
        let out = numeric_footer(&vals, ResultKind::Integer, &int_fmt, &dec_fmt).unwrap();
        assert!(out.contains("Sum: 50"));
    }

    #[test]
    fn date_footer_basic() {
        let int_fmt = IntegerFormat::default();
        let date_fmt = DateFormat::default();
        let vals = vec![v_s("2024-01-01"), v_s("2024-06-15"), v_s("2024-03-10")];
        let out = date_footer(&vals, &date_fmt, &int_fmt).unwrap();
        assert!(out.contains("Count: 3"));
        assert!(out.contains("Min: 2024-01-01"));
        assert!(out.contains("Max: 2024-06-15"));
        assert!(out.contains("Distinct: 3"));
    }

    #[test]
    fn date_footer_empty_returns_none() {
        let int_fmt = IntegerFormat::default();
        let date_fmt = DateFormat::default();
        assert!(date_footer(&[CellValue::Null], &date_fmt, &int_fmt).is_none());
    }

    #[test]
    fn text_length_footer_basic() {
        let int_fmt = IntegerFormat::default();
        let vals = vec![v_s("ab"), v_s("abcde"), v_s("xyz")];
        let out = text_length_footer(&vals, &int_fmt).unwrap();
        assert!(out.contains("Count: 3"));
        assert!(out.contains("Lengths: 2/3.333333/5"));
        assert!(out.contains("Distinct: 3"));
    }

    #[test]
    fn text_length_footer_whole_average() {
        let int_fmt = IntegerFormat::default();
        let vals = vec![v_s("ab"), v_s("abcd"), v_s("xyz")];
        let out = text_length_footer(&vals, &int_fmt).unwrap();
        assert!(out.contains("Lengths: 2/3/4"));
    }

    #[test]
    fn text_length_footer_empty_returns_none() {
        let int_fmt = IntegerFormat::default();
        assert!(text_length_footer(&[CellValue::Null], &int_fmt).is_none());
    }

    #[test]
    fn significant_decimal_places_whole_number() {
        assert_eq!(significant_decimal_places(6.0, 6), 0);
    }

    #[test]
    fn significant_decimal_places_half() {
        assert_eq!(significant_decimal_places(5.5, 6), 1);
    }

    #[test]
    fn significant_decimal_places_third() {
        assert_eq!(significant_decimal_places(1.0 / 3.0, 6), 6);
    }

    #[test]
    fn significant_decimal_places_max_zero() {
        assert_eq!(significant_decimal_places(1.5, 0), 0);
    }

    #[test]
    fn footer_dispatches_numeric() {
        let prefs = ResultFormattingPreferences::default();
        let vals = vec![v_i(1), v_i(2)];
        let out = footer(&vals, ResultKind::Integer, &prefs).unwrap();
        assert!(out.contains("Count: 2"));
        assert!(out.contains("Sum: 3"));
    }

    #[test]
    fn footer_dispatches_date() {
        let prefs = ResultFormattingPreferences::default();
        let vals = vec![v_s("2024-01-01"), v_s("2024-12-31")];
        let out = footer(&vals, ResultKind::Date, &prefs).unwrap();
        assert!(out.contains("Min: 2024-01-01"));
        assert!(out.contains("Max: 2024-12-31"));
        assert!(!out.contains("Sum:"));
    }

    #[test]
    fn footer_dispatches_text() {
        let prefs = ResultFormattingPreferences::default();
        let vals = vec![v_s("hello"), v_s("hi")];
        let out = footer(&vals, ResultKind::Text, &prefs).unwrap();
        assert!(out.contains("Lengths:"));
        assert!(!out.contains("Sum:"));
    }

    #[test]
    fn format_int_with_separators() {
        let fmt = IntegerFormat::default();
        assert_eq!(format_int(1234567, &fmt), "1,234,567");
    }

    #[test]
    fn format_int_negative_parens() {
        let fmt = IntegerFormat {
            negative_format: NegativeFormatStyle::Parentheses,
            ..Default::default()
        };
        assert_eq!(format_int(-1234, &fmt), "(1,234)");
    }

    #[test]
    fn footer_capped_at_cell_limit() {
        let prefs = ResultFormattingPreferences::default();
        let vals: Vec<CellValue> = (0..(SELECTION_STATS_CELL_CAP + 100) as i64)
            .map(v_i)
            .collect();
        let out = footer(&vals, ResultKind::Integer, &prefs).unwrap();
        assert!(out.contains("capped at"));
        assert!(out.contains(&SELECTION_STATS_CELL_CAP.to_string()));
    }

    #[test]
    fn footer_under_cap_no_capped_message() {
        let prefs = ResultFormattingPreferences::default();
        let vals = vec![v_i(1), v_i(2), v_i(3)];
        let out = footer(&vals, ResultKind::Integer, &prefs).unwrap();
        assert!(!out.contains("capped"));
    }
}
