//! Excel's General number display and the format codes `TEXT()` takes.

use chrono::{Datelike, Timelike};

use super::eval::{text_to_number, to_text};
use super::{ErrorKind, Value, datetime_from_serial};

/// A number as Excel's General format shows it: whole numbers plainly, up
/// to 15 significant digits otherwise, scientific when very large or small.
pub fn format_general(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    if n == n.trunc() && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    let exp = n.abs().log10().floor() as i32;
    if !(-9..15).contains(&exp) {
        let s = format!("{:.5E}", n);
        // 1.23457E15 → 1.23457E+15, trimming mantissa zeros.
        let (mantissa, e) = s.split_once('E').unwrap_or((&s, "0"));
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let e: i32 = e.parse().unwrap_or(0);
        return format!("{mantissa}E{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    let decimals = (14 - exp).clamp(0, 20) as usize;
    let s = format!("{:.*}", decimals, n);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// `TEXT(value, format)`: number codes (`0`, `#`, `,`, `.`, `%`, quoted
/// literals) and date/time codes (`yyyy`, `mm`, `mmm`, `dd`, `ddd`, `hh`,
/// `mm` as minutes after an hour, `ss`, `AM/PM`). Of `;`-separated sections
/// the first is used for positives and zero, the second for negatives.
pub fn format_text(value: &Value, fmt: &str) -> Result<String, ErrorKind> {
    let n = match value {
        Value::Number(n) => *n,
        Value::Blank => 0.0,
        Value::Bool(_) => return to_text(value),
        Value::Error(e) => return Err(*e),
        Value::Text(t) => match text_to_number(t) {
            Some(n) => n,
            None => return Ok(t.clone()),
        },
        Value::Array(items) => {
            return format_text(items.first().unwrap_or(&Value::Blank), fmt);
        }
    };
    let sections: Vec<&str> = fmt.split(';').collect();
    let (section, n) = if n < 0.0 && sections.len() > 1 {
        (sections[1], -n)
    } else {
        (sections[0], n)
    };
    if section.eq_ignore_ascii_case("general") || section.is_empty() {
        return Ok(format_general(n));
    }
    if is_date_format(section) {
        return format_date(n, section).ok_or(ErrorKind::Value);
    }
    Ok(format_number(n, section))
}

fn is_date_format(fmt: &str) -> bool {
    let mut in_quote = false;
    for c in fmt.chars() {
        match c {
            '"' => in_quote = !in_quote,
            _ if in_quote => {}
            'y' | 'Y' | 'd' | 'D' | 'h' | 'H' | 's' | 'S' => return true,
            'm' | 'M' if !fmt.contains(['0', '#']) => return true,
            _ => {}
        }
    }
    false
}

fn format_number(n: f64, fmt: &str) -> String {
    // Split literals from the numeric pattern.
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut pattern = String::new();
    let mut in_quote = false;
    let mut seen_digit = false;
    for c in fmt.chars() {
        if c == '"' {
            in_quote = !in_quote;
            continue;
        }
        let numeric = !in_quote && matches!(c, '0' | '#' | ',' | '.' | '?');
        if numeric {
            seen_digit = true;
            pattern.push(c);
        } else if c == '\\' {
            continue;
        } else if seen_digit {
            suffix.push(c);
        } else {
            prefix.push(c);
        }
    }
    let percent = fmt.contains('%') as i32;
    let n = n * 100f64.powi(percent);
    let (int_pat, frac_pat) = pattern.split_once('.').unwrap_or((&pattern, ""));
    let decimals = frac_pat
        .chars()
        .filter(|c| matches!(c, '0' | '#' | '?'))
        .count();
    let required_decimals = frac_pat.chars().filter(|c| *c == '0').count();
    let grouping = int_pat.contains(',');
    let min_int = int_pat.chars().filter(|c| *c == '0').count();
    let rounded = format!("{:.*}", decimals, n.abs() + 1e-12 * n.abs());
    let (int_part, frac_part) = rounded.split_once('.').unwrap_or((&rounded, ""));
    let mut int_part = int_part.trim_start_matches('0').to_string();
    while int_part.len() < min_int {
        int_part.insert(0, '0');
    }
    if grouping {
        let digits: Vec<char> = int_part.chars().collect();
        let mut grouped = String::new();
        for (i, c) in digits.iter().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(*c);
        }
        int_part = grouped;
    }
    let mut frac = frac_part.to_string();
    while frac.len() > required_decimals && frac.ends_with('0') {
        frac.pop();
    }
    let negative = n < 0.0
        && (!int_part.trim_matches(['0', ',']).is_empty() || !frac.trim_matches('0').is_empty());
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&prefix);
    out.push_str(&int_part);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(&frac);
    }
    out.push_str(&suffix);
    out
}

fn format_date(serial: f64, fmt: &str) -> Option<String> {
    let dt = if serial.floor() == 0.0 {
        // A pure time: anchor it on any date.
        datetime_from_serial(serial + 1.0)?
    } else {
        datetime_from_serial(serial)?
    };
    let twelve_hour = fmt.to_ascii_uppercase().contains("AM/PM");
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut last_was_hour = false;
    let run = |i: usize, c: char| {
        chars[i..]
            .iter()
            .take_while(|x| x.eq_ignore_ascii_case(&c))
            .count()
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                out.push(chars[j]);
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if chars[i..]
            .iter()
            .collect::<String>()
            .to_ascii_uppercase()
            .starts_with("AM/PM")
        {
            out.push_str(if dt.hour() < 12 { "AM" } else { "PM" });
            i += 5;
            continue;
        }
        let len = run(i, c);
        let lower = c.to_ascii_lowercase();
        let piece = match lower {
            'y' => {
                last_was_hour = false;
                if len <= 2 {
                    format!("{:02}", dt.year() % 100)
                } else {
                    format!("{:04}", dt.year())
                }
            }
            'm' => {
                // Minutes right after an hour or right before seconds.
                let next_is_seconds = chars[i + len..]
                    .iter()
                    .find(|x| x.is_alphabetic())
                    .is_some_and(|x| x.eq_ignore_ascii_case(&'s'));
                if (last_was_hour || next_is_seconds) && len <= 2 {
                    last_was_hour = false;
                    if len == 2 {
                        format!("{:02}", dt.minute())
                    } else {
                        dt.minute().to_string()
                    }
                } else {
                    match len {
                        1 => dt.month().to_string(),
                        2 => format!("{:02}", dt.month()),
                        3 => dt.format("%b").to_string(),
                        4 => dt.format("%B").to_string(),
                        _ => dt.format("%B").to_string().chars().take(1).collect(),
                    }
                }
            }
            'd' => {
                last_was_hour = false;
                match len {
                    1 => dt.day().to_string(),
                    2 => format!("{:02}", dt.day()),
                    3 => dt.format("%a").to_string(),
                    _ => dt.format("%A").to_string(),
                }
            }
            'h' => {
                last_was_hour = true;
                let h = if twelve_hour {
                    let h = dt.hour() % 12;
                    if h == 0 { 12 } else { h }
                } else {
                    dt.hour()
                };
                if len >= 2 {
                    format!("{h:02}")
                } else {
                    h.to_string()
                }
            }
            's' => {
                last_was_hour = false;
                if len >= 2 {
                    format!("{:02}", dt.second())
                } else {
                    dt.second().to_string()
                }
            }
            _ => {
                out.push(c);
                i += 1;
                continue;
            }
        };
        out.push_str(&piece);
        i += len;
    }
    Some(out)
}
