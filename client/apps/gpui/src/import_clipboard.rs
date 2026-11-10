//! Pure helpers for the "Import from clipboard" source of the import wizard:
//! delimiter detection, header-row guess, and the default table name. No gpui.

/// Candidate delimiters in priority order: tab wins ties because spreadsheet
/// copies are tab-separated and tabs almost never appear in plain prose.
pub const CANDIDATES: [char; 4] = ['\t', ',', ';', '|'];

/// Pick the delimiter whose per-line count is consistent (same count on every
/// non-empty line among the first 50) and largest. Falls back to `','`.
pub fn detect_delimiter(text: &str) -> char {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(50)
        .collect();
    if lines.is_empty() {
        return ',';
    }
    let mut best: Option<(char, usize)> = None;
    for ch in CANDIDATES {
        let counts: Vec<usize> = lines.iter().map(|l| count_outside_quotes(l, ch)).collect();
        let first = counts[0];
        if first == 0 || counts.iter().any(|c| *c != first) {
            continue;
        }
        match best {
            Some((_, n)) if n >= first => {}
            _ => best = Some((ch, first)),
        }
        if ch == '\t' {
            break;
        }
    }
    best.map(|(c, _)| c).unwrap_or(',')
}

/// Count `ch` outside double-quoted regions.
fn count_outside_quotes(line: &str, ch: char) -> usize {
    let mut count = 0;
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c == ch && !in_quotes => count += 1,
            _ => {}
        }
    }
    count
}

fn looks_numeric(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    value.parse::<i64>().is_ok()
        || (value.parse::<f64>().is_ok_and(f64::is_finite)
            && value.chars().any(|c| c.is_ascii_digit()))
}

fn looks_like_data_value(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if looks_numeric(value) {
        return true;
    }
    is_iso_date(value) || is_iso_datetime(value)
}

fn all_digits(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
}

fn is_iso_date(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    if !parts.iter().all(|part| all_digits(part)) {
        return false;
    }
    let month: u32 = parts[1].parse().unwrap_or(0);
    let day: u32 = parts[2].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

fn is_iso_datetime(value: &str) -> bool {
    let Some(split) = value.find(['T', ' ']) else {
        return is_iso_date(value);
    };
    let (date, time) = value.split_at(split);
    if !is_iso_date(date) {
        return false;
    }
    let time = &time[1..];
    let time = time.strip_suffix('Z').unwrap_or(time);
    let (time, fraction) = match time.split_once('.') {
        Some((time, fraction)) => (time, Some(fraction)),
        None => (time, None),
    };
    if fraction.is_some_and(|fraction| !all_digits(fraction)) {
        return false;
    }
    let parts: Vec<&str> = time.split(':').collect();
    if !(parts.len() == 2 || parts.len() == 3) || !parts.iter().all(|part| part.len() == 2) {
        return false;
    }
    if !parts.iter().all(|part| all_digits(part)) {
        return false;
    }
    let hour: u32 = parts[0].parse().unwrap_or(99);
    let minute: u32 = parts[1].parse().unwrap_or(99);
    let second: u32 = parts.get(2).map(|s| s.parse().unwrap_or(99)).unwrap_or(0);
    hour < 24 && minute < 60 && second < 60
}

/// Header guess: every cell of row 0 is non-empty, none parses as a number,
/// and at least one column has a numeric/date-looking cell in rows 1..n.
/// Single-row input → false.
pub fn looks_like_header(rows: &[Vec<String>]) -> bool {
    if rows.len() < 2 {
        return false;
    }
    let header = &rows[0];
    if header.is_empty() || header.iter().any(|cell| cell.trim().is_empty()) {
        return false;
    }
    if header.iter().any(|cell| looks_numeric(cell)) {
        return false;
    }
    let col_count = header.len();
    (1..rows.len()).any(|row_idx| {
        rows[row_idx]
            .iter()
            .take(col_count)
            .any(|cell| looks_like_data_value(cell))
    })
}

/// `pasted_data_<yyyymmdd_hhmm>` from a local timestamp (pass
/// `chrono::Local::now().naive_local()`; the parameter keeps the fn testable).
pub fn default_table_name(now: chrono::NaiveDateTime) -> String {
    format!("pasted_data_{}", now.format("%Y%m%d_%H%M"))
}

/// Normalise CRLF/CR to LF and strip one trailing newline.
pub fn normalise(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    text.strip_suffix('\n')
        .map(str::to_string)
        .unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn detect_delimiter_tab_beats_comma_when_both_consistent() {
        assert_eq!(detect_delimiter("a\tb\tc\n1\t2\t3\n"), '\t');
    }

    #[test]
    fn detect_delimiter_comma_text() {
        assert_eq!(detect_delimiter("a,b,c\n1,2,3\n"), ',');
    }

    #[test]
    fn detect_delimiter_semicolon_text() {
        assert_eq!(detect_delimiter("a;b\n1;2\n"), ';');
    }

    #[test]
    fn detect_delimiter_pipe_text() {
        assert_eq!(detect_delimiter("a|b\n1|2\n"), '|');
    }

    #[test]
    fn detect_delimiter_ignores_quoted_commas() {
        assert_eq!(detect_delimiter("\"x, y\",z\n1,2\n"), ',');
    }

    #[test]
    fn detect_delimiter_inconsistent_counts_fall_back_to_comma() {
        assert_eq!(detect_delimiter("a,b\n1,2,3\n"), ',');
    }

    #[test]
    fn detect_delimiter_single_line_falls_back_to_comma() {
        assert_eq!(detect_delimiter("only one line"), ',');
    }

    #[test]
    fn looks_like_header_with_names_and_data_rows() {
        assert!(looks_like_header(&[
            vec!["id".into(), "name".into()],
            vec!["1".into(), "a".into()],
        ]));
    }

    #[test]
    fn looks_like_header_rejects_all_numeric_rows() {
        assert!(!looks_like_header(&[
            vec!["1".into(), "a".into()],
            vec!["2".into(), "b".into()],
        ]));
    }

    #[test]
    fn looks_like_header_single_row_is_false() {
        assert!(!looks_like_header(&[vec!["id".into(), "name".into()]]));
    }

    #[test]
    fn default_table_name_formats_timestamp() {
        let now = NaiveDate::from_ymd_opt(2026, 8, 30)
            .unwrap()
            .and_hms_opt(14, 5, 0)
            .unwrap();
        assert_eq!(default_table_name(now), "pasted_data_20260830_1405");
    }
}
