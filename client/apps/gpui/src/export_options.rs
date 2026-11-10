//! Pure export options shared by whole-document renderers, streaming writers
//! and the batch table export. Defaults reproduce the pre-options output of
//! every format byte for byte (see `ExportOptions::default_for`).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::results::ExportFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NullAs {
    Empty,
    Literal,
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineBreaks {
    Keep,
    Space,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub null_as: NullAs,
    pub line_breaks: LineBreaks,
    pub header_row: bool,
    pub gzip: bool,
    /// Guard CSV/TSV text cells that a spreadsheet would evaluate as a
    /// formula (`=`, `+`, `-`, `@`, tab, CR) with a leading apostrophe — see
    /// `sqlly_core::formula_guard`. Only delimited formats consult it; typed
    /// formats (JSON, XLSX, SQL) never write a formula in the first place.
    pub formula_safe: bool,
}

impl ExportOptions {
    /// Today's behaviour per format: CSV writes `NULL`, TSV/Markdown/HTML write
    /// an empty cell, everything keeps line breaks, has a header row, and
    /// guards against spreadsheet formulas.
    pub fn default_for(format: ExportFormat) -> Self {
        use ExportFormat as F;
        let null_as = match format {
            F::Tsv | F::Markdown | F::Html => NullAs::Empty,
            _ => NullAs::Literal,
        };
        Self {
            null_as,
            line_breaks: LineBreaks::Keep,
            header_row: true,
            gzip: false,
            formula_safe: true,
        }
    }

    pub fn null_text(&self) -> &str {
        match &self.null_as {
            NullAs::Empty => "",
            NullAs::Literal => "NULL",
            NullAs::Custom(s) => s,
        }
    }
}

/// Substitute `{schema}`, `{table}`, `{date}`, and `{format}` in `pattern`.
/// Unknown `{tokens}` are left verbatim. Sanitized token values replace
/// path separators and `[^A-Za-z0-9_.-]` with `_`.
pub fn apply_line_breaks(text: &str, mode: LineBreaks) -> Cow<'_, str> {
    match mode {
        LineBreaks::Keep => Cow::Borrowed(text),
        LineBreaks::Space => Cow::Owned(
            text.chars()
                .map(|c| match c {
                    '\n' | '\r' | '\t' => ' ',
                    other => other,
                })
                .collect(),
        ),
        LineBreaks::Escape => {
            let mut out = String::with_capacity(text.len());
            for ch in text.chars() {
                match ch {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    other => out.push(other),
                }
            }
            Cow::Owned(out)
        }
    }
}

fn sanitize_token(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Tokens: `{schema}`, `{table}`, `{date}` (YYYYMMDD), `{format}` (extension).
/// Unknown tokens are left verbatim.
pub struct PatternContext<'a> {
    pub schema: &'a str,
    pub table: &'a str,
    pub date: &'a str,
    pub format: &'a str,
}

pub fn filename_from_pattern(pattern: &str, ctx: &PatternContext<'_>) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut token = String::new();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == '}' {
                    break;
                }
                token.push(next);
            }
            let replacement = match token.as_str() {
                "schema" => sanitize_token(ctx.schema),
                "table" => sanitize_token(ctx.table),
                "date" => sanitize_token(ctx.date),
                "format" => sanitize_token(ctx.format),
                "" => "{}".to_string(),
                other => format!("{{{other}}}"),
            };
            out.push_str(&replacement);
        } else {
            out.push(ch);
        }
    }
    out
}

pub const DEFAULT_PATTERN: &str = "{schema}_{table}_{date}.{format}";

/// Appends `.gz` unless `path` already ends with `.gz`.
pub fn gz_path(path: &Path, gzip: bool) -> PathBuf {
    if !gzip {
        return path.to_path_buf();
    }
    let s = path.to_string_lossy();
    if s.ends_with(".gz") {
        path.to_path_buf()
    } else {
        path.with_extension(format!(
            "{}.gz",
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_for_csv_null_is_literal() {
        assert_eq!(
            ExportOptions::default_for(ExportFormat::Csv).null_text(),
            "NULL"
        );
    }

    #[test]
    fn default_for_tsv_null_is_empty() {
        assert_eq!(
            ExportOptions::default_for(ExportFormat::Tsv).null_text(),
            ""
        );
    }

    #[test]
    fn default_for_every_format_is_formula_safe() {
        for format in ExportFormat::all() {
            assert!(
                ExportOptions::default_for(*format).formula_safe,
                "{format:?} should guard spreadsheet formulas by default"
            );
        }
    }

    #[test]
    fn apply_line_breaks_space_replaces_controls() {
        assert_eq!(apply_line_breaks("a\nb", LineBreaks::Space), "a b");
        assert_eq!(apply_line_breaks("a\rb", LineBreaks::Space), "a b");
        assert_eq!(apply_line_breaks("a\tb", LineBreaks::Space), "a b");
    }

    #[test]
    fn apply_line_breaks_escape() {
        assert_eq!(apply_line_breaks("a\nb", LineBreaks::Escape), "a\\nb");
    }

    #[test]
    fn apply_line_breaks_keep_borrows() {
        let s = "plain";
        match apply_line_breaks(s, LineBreaks::Keep) {
            Cow::Borrowed(b) => assert!(std::ptr::eq(b.as_ptr(), s.as_ptr())),
            Cow::Owned(_) => panic!("expected borrow"),
        }
    }

    #[test]
    fn pattern_default_tokens() {
        let ctx = PatternContext {
            schema: "public",
            table: "orders",
            date: "20260830",
            format: "csv",
        };
        assert_eq!(
            filename_from_pattern(DEFAULT_PATTERN, &ctx),
            "public_orders_20260830.csv"
        );
    }

    #[test]
    fn pattern_unknown_token_kept() {
        let ctx = PatternContext {
            schema: "dbo",
            table: "t",
            date: "20260101",
            format: "tsv",
        };
        assert_eq!(
            filename_from_pattern("{schema}_{unknown}_{table}", &ctx),
            "dbo_{unknown}_t"
        );
    }

    #[test]
    fn pattern_sanitizes_hostile_schema() {
        let ctx = PatternContext {
            schema: "a/b",
            table: "orders",
            date: "20260830",
            format: "csv",
        };
        assert_eq!(
            filename_from_pattern(DEFAULT_PATTERN, &ctx),
            "a_b_orders_20260830.csv"
        );
    }

    #[test]
    fn pattern_empty_schema_yields_underscore() {
        let ctx = PatternContext {
            schema: "",
            table: "orders",
            date: "20260830",
            format: "csv",
        };
        assert_eq!(
            filename_from_pattern(DEFAULT_PATTERN, &ctx),
            "_orders_20260830.csv"
        );
    }

    #[test]
    fn gz_path_appends_once() {
        assert_eq!(gz_path(Path::new("a.csv"), true), PathBuf::from("a.csv.gz"));
        assert_eq!(
            gz_path(Path::new("a.csv.gz"), true),
            PathBuf::from("a.csv.gz")
        );
    }
}
