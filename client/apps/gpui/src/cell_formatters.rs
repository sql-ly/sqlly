//! Pure display formatters for result cells: JSON-path extraction, `{value}` templates, and number styles. No GPUI; unit-tested.

use sqlly_core::CellValue;

use crate::preferences::NumberStyleFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSeg {
    Key(String),
    Index(usize),
}

/// Extract a value from JSON text with a subset of JSONPath: `$`, `.name`, `[n]`, and quoted keys.
///
/// Returns `None` on malformed JSON, a malformed path, or a path that does not resolve.
pub fn json_path_extract(raw: &str, path: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let segs = parse_json_path(path)?;
    let mut current = &value;
    for seg in &segs {
        current = match seg {
            PathSeg::Key(key) => current.get(key)?,
            PathSeg::Index(index) => current.get(*index)?,
        };
    }
    Some(render_json_leaf(current))
}

fn parse_json_path(path: &str) -> Option<Vec<PathSeg>> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "$" {
        return Some(Vec::new());
    }
    let mut rest = trimmed;
    if let Some(after) = rest.strip_prefix('$') {
        rest = after;
    }
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut first = true;
    let mut segs = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'.' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                i += 1;
            }
            if start == i {
                return None;
            }
            segs.push(PathSeg::Key(rest[start..i].to_string()));
            first = false;
        } else if bytes[i] == b'[' {
            i += 1;
            if i >= bytes.len() {
                return None;
            }
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                let key = rest[start..i].to_string();
                i += 1;
                if i >= bytes.len() || bytes[i] != b']' {
                    return None;
                }
                i += 1;
                segs.push(PathSeg::Key(key));
            } else {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if start == i {
                    return None;
                }
                let index: usize = rest[start..i].parse().ok()?;
                if i >= bytes.len() || bytes[i] != b']' {
                    return None;
                }
                i += 1;
                segs.push(PathSeg::Index(index));
            }
            first = false;
        } else if first {
            let start = i;
            while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                i += 1;
            }
            if start == i {
                return None;
            }
            segs.push(PathSeg::Key(rest[start..i].to_string()));
            first = false;
        } else {
            return None;
        }
    }
    Some(segs)
}

fn render_json_leaf(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Expand `{value}` placeholders. `{{` and `}}` are literal braces.
pub fn render_template(value: &str, template: &str) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                out.push('{');
                continue;
            }
            let mut inner = String::new();
            let mut closed = false;
            for ic in chars.by_ref() {
                if ic == '}' {
                    closed = true;
                    break;
                }
                inner.push(ic);
            }
            if !closed {
                out.push('{');
                out.push_str(&inner);
                continue;
            }
            out.push_str(&expand_placeholder(value, &inner));
        } else if c == '}' {
            if chars.peek() == Some(&'}') {
                chars.next();
                out.push('}');
            } else {
                out.push('}');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn expand_placeholder(value: &str, inner: &str) -> String {
    let mut parts = inner.split('|');
    let Some(head) = parts.next() else {
        return format!("{{{inner}}}");
    };
    if head != "value" {
        return format!("{{{inner}}}");
    }
    let mut current = value.to_string();
    for helper in parts {
        let (name, arg) = match helper.split_once(':') {
            Some((name, arg)) => (name, Some(arg)),
            None => (helper, None),
        };
        match (name, arg) {
            ("upper", None) => current = current.to_uppercase(),
            ("lower", None) => current = current.to_lowercase(),
            ("trunc", Some(n)) => {
                let Ok(n) = n.parse::<usize>() else {
                    return format!("{{{inner}}}");
                };
                if current.chars().count() > n {
                    current = current.chars().take(n).collect::<String>() + "…";
                }
            }
            ("prefix", Some(s)) => current = format!("{s}{current}"),
            ("suffix", Some(s)) => current.push_str(s),
            ("pad", Some(n)) => {
                let Ok(n) = n.parse::<usize>() else {
                    return format!("{{{inner}}}");
                };
                let count = current.chars().count();
                if count < n {
                    current = format!("{}{current}", " ".repeat(n - count));
                }
            }
            ("default", Some(s)) => {
                if current.is_empty() {
                    current = s.to_string();
                }
            }
            _ => return format!("{{{inner}}}"),
        }
    }
    current
}

/// Format a finite number as fixed, percent, or binary byte size.
pub fn format_number_style(value: f64, fmt: &NumberStyleFormat) -> String {
    if !value.is_finite() {
        return "NULL".into();
    }
    match fmt.style {
        crate::preferences::NumberStyle::Fixed => {
            format_fixed(value, fmt.decimals, fmt.use_thousands_separators)
        }
        crate::preferences::NumberStyle::Percent => {
            format!(
                "{}%",
                format_fixed(value * 100.0, fmt.decimals, fmt.use_thousands_separators)
            )
        }
        crate::preferences::NumberStyle::Bytes => format_bytes(value, fmt.decimals),
    }
}

fn format_fixed(value: f64, decimals: u8, thousands: bool) -> String {
    let formatted = format!("{:.*}", decimals as usize, value);
    let (int_part, frac) = match formatted.split_once('.') {
        Some((int_part, frac)) => (int_part, Some(frac)),
        None => (formatted.as_str(), None),
    };
    let grouped = if thousands {
        group_thousands(int_part)
    } else {
        int_part.to_string()
    };
    match frac {
        Some(frac) => format!("{grouped}.{frac}"),
        None => grouped,
    }
}

fn group_thousands(int_part: &str) -> String {
    let (sign, digits) = int_part
        .strip_prefix('-')
        .map(|rest| ("-", rest))
        .unwrap_or(("", int_part));
    let mut grouped = String::new();
    for (i, c) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let grouped: String = grouped.chars().rev().collect();
    format!("{sign}{grouped}")
}

fn format_bytes(value: f64, decimals: u8) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let mut abs = value.abs();
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0usize;
    while abs >= 1024.0 && unit + 1 < units.len() {
        abs /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{sign}{:.0} B", abs)
    } else {
        format!("{sign}{:.*} {}", decimals as usize, abs, units[unit])
    }
}

/// Parse a cell as a number. TDS decimal/money values often arrive as strings.
pub fn numeric_value(cell: &CellValue) -> Option<f64> {
    match cell {
        CellValue::I64(v) => Some(*v as f64),
        CellValue::F64(v) => Some(*v),
        CellValue::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preferences::{NumberStyle, NumberStyleFormat};
    use sqlly_core::CellValue;

    #[test]
    fn json_path_dot_and_index() {
        let raw = r#"{"customer":{"name":"Ada"},"items":[{"sku":"A1"},{"sku":"B2"}]}"#;
        assert_eq!(
            json_path_extract(raw, "$.customer.name").as_deref(),
            Some("Ada")
        );
        assert_eq!(
            json_path_extract(raw, "$.items[1].sku").as_deref(),
            Some("B2")
        );
        assert_eq!(
            json_path_extract(raw, "items[0].sku").as_deref(),
            Some("A1")
        );
    }

    #[test]
    fn json_path_quoted_key_and_root() {
        let raw = r#"{"first name":"Ada"}"#;
        assert_eq!(
            json_path_extract(raw, r#"$["first name"]"#).as_deref(),
            Some("Ada")
        );
        assert_eq!(json_path_extract(raw, "$").as_deref(), Some(raw));
    }

    #[test]
    fn json_path_missing_and_invalid() {
        let raw = r#"{"a":1}"#;
        assert_eq!(json_path_extract(raw, "$.missing"), None);
        assert_eq!(json_path_extract(raw, "$.a[9]"), None);
        assert_eq!(json_path_extract("{", "$.a"), None);
        assert_eq!(json_path_extract(raw, "$.a[b]"), None);
        assert_eq!(json_path_extract(r#"{"a":"#, "$.a"), None);
    }

    #[test]
    fn json_path_leaf_rendering() {
        assert_eq!(
            json_path_extract(r#"{"n":42}"#, "$.n").as_deref(),
            Some("42")
        );
        assert_eq!(
            json_path_extract(r#"{"n":null}"#, "$.n").as_deref(),
            Some("null")
        );
        assert_eq!(
            json_path_extract(r#"{"n":{"x":1}}"#, "$.n").as_deref(),
            Some(r#"{"x":1}"#)
        );
    }

    #[test]
    fn template_plain_and_helpers() {
        assert_eq!(render_template("Ada", "{value}"), "Ada");
        assert_eq!(render_template("ada", "{value|upper}"), "ADA");
        assert_eq!(render_template("abcdef", "{value|trunc:3}"), "abc…");
        assert_eq!(render_template("123", "ORD-{value|pad:6}"), "ORD-   123");
        assert_eq!(
            render_template("12", "{value|prefix:$|suffix: USD}"),
            "$12 USD"
        );
        assert_eq!(
            render_template("Ada", "{{literal}} {value}"),
            "{literal} Ada"
        );
        assert_eq!(render_template("", "{value|default:n/a}"), "n/a");
        assert_eq!(render_template("x", "{value|nope}"), "{value|nope}");
    }

    #[test]
    fn number_style_fixed_percent_bytes() {
        let fixed = NumberStyleFormat {
            style: NumberStyle::Fixed,
            decimals: 2,
            use_thousands_separators: true,
        };
        assert_eq!(format_number_style(1_234_567.891, &fixed), "1,234,567.89");
        let percent = NumberStyleFormat {
            style: NumberStyle::Percent,
            decimals: 2,
            use_thousands_separators: true,
        };
        assert_eq!(format_number_style(0.1234, &percent), "12.34%");
        let bytes = NumberStyleFormat {
            style: NumberStyle::Bytes,
            decimals: 2,
            use_thousands_separators: true,
        };
        assert_eq!(format_number_style(512.0, &bytes), "512 B");
        assert_eq!(format_number_style(1536.0, &bytes), "1.50 KiB");
        assert_eq!(
            format_number_style(2.0 * 1024.0 * 1024.0, &bytes),
            "2.00 MiB"
        );
        assert_eq!(format_number_style(-2048.0, &bytes), "-2.00 KiB");
        assert_eq!(format_number_style(f64::NAN, &bytes), "NULL");
    }

    #[test]
    fn numeric_value_parses_strings() {
        assert_eq!(
            numeric_value(&CellValue::String(" 12.5 ".into())),
            Some(12.5)
        );
        assert_eq!(numeric_value(&CellValue::Bool(true)), None);
    }
}
