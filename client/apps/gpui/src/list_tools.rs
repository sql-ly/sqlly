//! Clipboard list splitting, paste-as-IN-list rendering, and delimited-list
//! conversion. Pure — no GPUI types.

use sqlly_lang::sqlmodel::DatabaseType;

pub const MAX_LIST_VALUES: usize = 10_000;
pub const MAX_LIST_BYTES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InputDelimiter {
    #[default]
    Auto,
    Comma,
    Newline,
    Tab,
    Semicolon,
    Space,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Quote {
    #[default]
    None,
    Single,
    Double,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvertOptions {
    pub input_delimiter: InputDelimiter,
    pub output_delimiter: String,
    pub quote: Quote,
    pub prefix: String,
    pub suffix: String,
    pub wrap_width: usize,
    pub trim: bool,
    pub drop_empty: bool,
    pub dedupe: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            input_delimiter: InputDelimiter::Auto,
            output_delimiter: ", ".to_string(),
            quote: Quote::None,
            prefix: String::new(),
            suffix: String::new(),
            wrap_width: 0,
            trim: true,
            drop_empty: true,
            dedupe: false,
        }
    }
}

/// Split clipboard text into candidate values on `,`, `\n`, `\r\n`, `\t`;
/// trims each; drops empties.
pub fn split_list(input: &str) -> Result<Vec<String>, String> {
    if input.len() > MAX_LIST_BYTES {
        return Err(crate::localization::format(
            "editor.list_tools.clipboard_too_large",
            &[
                ("actual_bytes", &input.len().to_string()),
                ("limit_bytes", &MAX_LIST_BYTES.to_string()),
            ],
        ));
    }
    let mut values = Vec::new();
    for part in split_on_mixed_delimiters(input) {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            values.push(trimmed.to_string());
        }
    }
    if values.len() > MAX_LIST_VALUES {
        return Err(crate::localization::format(
            "editor.list_tools.clipboard_too_many_values",
            &[
                ("actual_count", &values.len().to_string()),
                ("limit_count", &MAX_LIST_VALUES.to_string()),
            ],
        ));
    }
    Ok(values)
}

fn split_on_mixed_delimiters(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b',' || b == b'\t' || b == b'\n' {
            parts.push(&input[start..i]);
            start = i + 1;
            i += 1;
            continue;
        }
        if b == b'\r' {
            parts.push(&input[start..i]);
            start = if bytes.get(i + 1) == Some(&b'\n') {
                i + 2
            } else {
                i + 1
            };
            i = start;
            continue;
        }
        i += 1;
    }
    if start < input.len() || input.is_empty() {
        parts.push(&input[start..]);
    }
    parts
}

fn detect_delimiter(input: &str) -> InputDelimiter {
    if input.contains('\n') || input.contains('\r') {
        return InputDelimiter::Newline;
    }
    if input.contains('\t') {
        return InputDelimiter::Tab;
    }
    if input.contains(',') {
        return InputDelimiter::Comma;
    }
    if input.contains(';') {
        return InputDelimiter::Semicolon;
    }
    InputDelimiter::Space
}

fn split_with_delimiter(input: &str, delimiter: InputDelimiter) -> Vec<String> {
    match delimiter {
        InputDelimiter::Auto => split_list(input).unwrap_or_default(),
        InputDelimiter::Comma => input.split(',').map(str::to_string).collect::<Vec<_>>(),
        InputDelimiter::Newline => input.lines().map(str::to_string).collect::<Vec<_>>(),
        InputDelimiter::Tab => input.split('\t').map(str::to_string).collect::<Vec<_>>(),
        InputDelimiter::Semicolon => input.split(';').map(str::to_string).collect::<Vec<_>>(),
        InputDelimiter::Space => input
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>(),
    }
}

fn is_bare_number(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.starts_with('+') {
        return false;
    }
    if value.contains(',') {
        return false;
    }
    if value.parse::<i64>().is_ok() {
        return true;
    }
    value
        .parse::<f64>()
        .is_ok_and(|n| n.is_finite() && !value.starts_with('+'))
}

fn unquote_single(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn sql_list_literal(db: DatabaseType, raw: &str) -> String {
    let inner = unquote_single(raw);
    db.string_literal(inner)
}

/// Render `(v1, v2, ...)` for the engine.
pub fn paste_as_in_list(db: DatabaseType, input: &str) -> Result<String, String> {
    let values = split_list(input)?;
    if values.is_empty() {
        return Err(crate::localization::text(
            "editor.list_tools.clipboard_empty",
        ));
    }
    let mut out = String::from("(");
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if value.eq_ignore_ascii_case("null") {
            out.push_str("NULL");
        } else if is_bare_number(value) {
            out.push_str(value.trim());
        } else {
            out.push_str(&sql_list_literal(db, value));
        }
    }
    out.push(')');
    Ok(out)
}

fn apply_quote(value: &str, quote: Quote) -> String {
    match quote {
        Quote::None => value.to_string(),
        Quote::Single => {
            let escaped = value.replace('\'', "''");
            format!("'{escaped}'")
        }
        Quote::Double => {
            let escaped = value.replace('"', "\"\"");
            format!("\"{escaped}\"")
        }
    }
}

/// Reshape a list with the given options.
pub fn convert_list(input: &str, opts: &ConvertOptions) -> Result<String, String> {
    if input.len() > MAX_LIST_BYTES {
        return Err(crate::localization::format(
            "editor.list_tools.selection_too_large",
            &[
                ("actual_bytes", &input.len().to_string()),
                ("limit_bytes", &MAX_LIST_BYTES.to_string()),
            ],
        ));
    }
    let delimiter = match opts.input_delimiter {
        InputDelimiter::Auto => detect_delimiter(input),
        other => other,
    };
    let mut values = split_with_delimiter(input, delimiter);
    if opts.trim {
        for value in &mut values {
            *value = value.trim().to_string();
        }
    }
    if opts.drop_empty {
        values.retain(|value| !value.is_empty());
    }
    if values.len() > MAX_LIST_VALUES {
        return Err(crate::localization::format(
            "editor.list_tools.selection_too_many_values",
            &[
                ("actual_count", &values.len().to_string()),
                ("limit_count", &MAX_LIST_VALUES.to_string()),
            ],
        ));
    }
    if opts.dedupe {
        let mut seen = std::collections::HashSet::new();
        values.retain(|value| seen.insert(value.clone()));
    }
    let mut out = String::new();
    let delimiter = if opts.output_delimiter.is_empty() {
        ", "
    } else {
        &opts.output_delimiter
    };
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            if opts.wrap_width > 0 && out.len() + delimiter.len() > opts.wrap_width {
                out.push('\n');
            } else {
                out.push_str(delimiter);
            }
        }
        let quoted = apply_quote(value, opts.quote);
        out.push_str(&opts.prefix);
        out.push_str(&quoted);
        out.push_str(&opts.suffix);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_on_mixed_delimiters() {
        let values = split_list("a, b\n\rc\td").unwrap();
        assert_eq!(values, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn split_limits_values_and_bytes() {
        let huge = "a,".repeat(MAX_LIST_VALUES + 1);
        let err = split_list(&huge).unwrap_err();
        assert!(err.contains("Too many values"));
        let bytes = "x".repeat(MAX_LIST_BYTES + 1);
        let err = split_list(&bytes).unwrap_err();
        assert!(err.contains("too large"));
    }

    #[test]
    fn in_list_numbers_bare_strings_quoted() {
        let input = "1, 2, abc";
        let tds = paste_as_in_list(DatabaseType::SqlServer, input).unwrap();
        assert_eq!(tds, "(1, 2, N'abc')");
        let pg = paste_as_in_list(DatabaseType::Postgres, input).unwrap();
        assert_eq!(pg, "(1, 2, 'abc')");
    }

    #[test]
    fn in_list_null_and_prequoted() {
        let out = paste_as_in_list(DatabaseType::SqlServer, "null, 'x'").unwrap();
        assert_eq!(out, "(NULL, N'x')");
    }

    #[test]
    fn in_list_mysql_backslash() {
        let out = paste_as_in_list(DatabaseType::MySql, "a\\b").unwrap();
        assert_eq!(out, "('a\\\\b')");
    }

    #[test]
    fn in_list_embedded_quote() {
        let out = paste_as_in_list(DatabaseType::SqlServer, "O'Brien").unwrap();
        assert_eq!(out, "(N'O''Brien')");
    }

    #[test]
    fn in_list_empty_errors() {
        assert_eq!(
            paste_as_in_list(DatabaseType::Postgres, "  ,  ").unwrap_err(),
            "Clipboard has no values."
        );
    }

    #[test]
    fn convert_auto_detects_delimiter() {
        let opts = ConvertOptions::default();
        assert_eq!(convert_list("a\nb\nc", &opts).unwrap(), "a, b, c");
        assert_eq!(convert_list("a\tb", &opts).unwrap(), "a, b");
        assert_eq!(convert_list("a,b", &opts).unwrap(), "a, b");
    }

    #[test]
    fn convert_quotes_and_prefix_suffix() {
        let opts = ConvertOptions {
            quote: Quote::Single,
            prefix: "[".into(),
            suffix: "]".into(),
            ..Default::default()
        };
        assert_eq!(convert_list("a,b,c", &opts).unwrap(), "['a'], ['b'], ['c']");
    }

    #[test]
    fn convert_wraps_at_width() {
        let opts = ConvertOptions {
            output_delimiter: ", ".into(),
            wrap_width: 7,
            ..Default::default()
        };
        assert_eq!(convert_list("aa,bb,cc", &opts).unwrap(), "aa, bb\ncc");
    }

    #[test]
    fn convert_dedupes_when_asked() {
        let opts = ConvertOptions {
            dedupe: true,
            ..Default::default()
        };
        assert_eq!(convert_list("a,a,b", &opts).unwrap(), "a, b");
    }

    #[test]
    fn convert_keeps_order() {
        let opts = ConvertOptions::default();
        assert_eq!(convert_list("z,y,x", &opts).unwrap(), "z, y, x");
    }
}
