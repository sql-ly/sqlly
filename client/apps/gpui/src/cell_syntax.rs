//! Syntax colors for JSON and XML values inside result-grid cells.
//!
//! The grid (`sqlly-datatable`) asks a [`StructuredCellStyler`] for colored
//! runs over the *visible prefix* of a cell and caches them per cell, so the
//! lexers here only ever see a few hundred characters and never need the
//! whole (possibly huge, possibly cut-off) document to be valid. They are
//! deliberately forgiving: anything they do not recognise keeps the cell's
//! normal color.

use std::collections::HashMap;

use sqlly_core::{DatabaseType, ResultColumn};
use sqlly_datatable::{CellTextStyler, RgbaColor, TextSpan};

use crate::preferences::ResultKind;

/// Token colors, taken from the theme's syntax palette when the grid config
/// is built (so they follow theme switches like the rest of the grid).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntaxPalette {
    /// JSON object keys and XML attribute names.
    pub key: RgbaColor,
    pub string: RgbaColor,
    pub number: RgbaColor,
    /// `true` / `false` / `null`, XML entities.
    pub constant: RgbaColor,
    /// Braces, brackets, commas, colons, `<`, `>`, `=`.
    pub punctuation: RgbaColor,
    /// XML tag names.
    pub tag: RgbaColor,
    pub comment: RgbaColor,
}

impl SyntaxPalette {
    /// The active theme's syntax colors.
    pub fn from_theme() -> Self {
        use crate::theme::Theme;
        Self {
            key: rgba(Theme::type_name()),
            string: rgba(Theme::string()),
            number: rgba(Theme::number()),
            constant: rgba(Theme::constant()),
            punctuation: rgba(Theme::comment()),
            tag: rgba(Theme::keyword()),
            comment: rgba(Theme::comment()),
        }
    }
}

fn rgba(color: gpui::Hsla) -> RgbaColor {
    let rgba: gpui::Rgba = color.into();
    RgbaColor::new(rgba.r, rgba.g, rgba.b, rgba.a)
}

/// What a column holds, as far as coloring goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredHint {
    /// Declared JSON (`json`, `jsonb`): always lexed as JSON.
    Json,
    /// Declared XML: always lexed as XML.
    Xml,
    /// A text column: colored only when a value looks like JSON or XML.
    Sniff,
}

/// The coloring hint for a result column, or `None` when its cells are not
/// text that could hold JSON/XML (numbers, dates, binary, …).
pub fn structured_hint(db: Option<DatabaseType>, column: &ResultColumn) -> Option<StructuredHint> {
    let key = crate::result_cell_format::type_name_key(&column.data_type);
    match key.as_str() {
        "json" | "jsonb" => return Some(StructuredHint::Json),
        "xml" | "xmltype" => return Some(StructuredHint::Xml),
        _ => {}
    }
    (crate::result_cell_format::infer_result_kind_for(db, Some(column)) == ResultKind::Text)
        .then_some(StructuredHint::Sniff)
}

/// The grid's text styler for "Syntax-color structured values in cells".
#[derive(Debug)]
pub struct StructuredCellStyler {
    /// Lowercased column name → hint (the grid matches names
    /// case-insensitively elsewhere too).
    columns: HashMap<String, StructuredHint>,
    palette: SyntaxPalette,
}

impl StructuredCellStyler {
    /// A styler for `columns`, or `None` when none of them can hold JSON/XML.
    pub fn for_columns(
        db: Option<DatabaseType>,
        columns: &[ResultColumn],
        palette: SyntaxPalette,
    ) -> Option<Self> {
        let mut map = HashMap::new();
        for column in columns {
            if let Some(hint) = structured_hint(db, column) {
                map.entry(column.name.to_lowercase()).or_insert(hint);
            }
        }
        (!map.is_empty()).then_some(Self {
            columns: map,
            palette,
        })
    }

    fn hint(&self, column: &str) -> Option<StructuredHint> {
        self.columns.get(&column.to_lowercase()).copied()
    }
}

impl CellTextStyler for StructuredCellStyler {
    fn styles_column(&self, column: &str) -> bool {
        self.hint(column).is_some()
    }

    fn spans(&self, column: &str, text: &str) -> Vec<TextSpan> {
        let lang = match self.hint(column) {
            Some(StructuredHint::Json) => Some(Lang::Json),
            Some(StructuredHint::Xml) => Some(Lang::Xml),
            Some(StructuredHint::Sniff) => sniff(text),
            None => None,
        };
        match lang {
            Some(Lang::Json) => json_spans(text, &self.palette),
            Some(Lang::Xml) => xml_spans(text, &self.palette),
            None => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Json,
    Xml,
}

/// Whether text in a plain text column looks like JSON or XML. Strict
/// enough that `[INFO] started` or `<none>`-style prose rarely qualifies.
fn sniff(text: &str) -> Option<Lang> {
    let trimmed = text.trim_start();
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    let next = chars.find(|c| !c.is_whitespace());
    match first {
        '{' => matches!(next, Some('"') | Some('}')).then_some(Lang::Json),
        '[' => matches!(
            next,
            Some('"' | '{' | '[' | ']' | '-' | '0'..='9' | 't' | 'f' | 'n')
        )
        .then_some(Lang::Json)
        .filter(|_| looks_like_json_array(trimmed)),
        '<' => matches!(next, Some(c) if c.is_alphabetic() || c == '?' || c == '!')
            .then_some(Lang::Xml)
            .filter(|_| trimmed.contains('>')),
        _ => None,
    }
}

/// `[true]`, `[1, 2]`, `["a"]`: rule out log prefixes like `[note] …` and
/// `[1] item` by requiring a scalar first element to be followed by `,` or
/// by a `]` that ends the value (a string/object/array first element is
/// accepted as is — prose almost never opens with one).
fn looks_like_json_array(text: &str) -> bool {
    let inner = text[1..].trim_start();
    match inner.chars().next() {
        Some('"' | '{' | '[' | ']') => true,
        Some(_) => {
            let end = inner
                .find(|c: char| c == ',' || c == ']' || c.is_whitespace())
                .unwrap_or(inner.len());
            let word = &inner[..end];
            let rest = inner[end..].trim_start();
            let scalar = matches!(word, "true" | "false" | "null") || word.parse::<f64>().is_ok();
            // `[1] first item` closes the "array" and goes on as prose.
            let closes_cleanly = rest
                .strip_prefix(']')
                .is_some_and(|tail| tail.trim().is_empty());
            scalar && (rest.starts_with(',') || closes_cleanly || rest.is_empty())
        }
        None => false,
    }
}

fn push(spans: &mut Vec<TextSpan>, start: usize, end: usize, color: RgbaColor) {
    if start < end {
        spans.push(TextSpan::new(start..end, color));
    }
}

/// Tokenize a JSON prefix: keys, strings, numbers, literals, punctuation.
/// Unterminated strings run to the end (the prefix was cut mid-string).
pub fn json_spans(text: &str, palette: &SyntaxPalette) -> Vec<TextSpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let end = i.min(bytes.len());
                let mut j = end;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let is_key = bytes.get(j) == Some(&b':');
                let color = if is_key { palette.key } else { palette.string };
                push(&mut spans, start, end, color);
                i = end;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    i += 1;
                }
                push(&mut spans, start, i, palette.number);
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' => {
                push(&mut spans, i, i + 1, palette.punctuation);
                i += 1;
            }
            b if b.is_ascii_alphabetic() => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                if matches!(&text[start..i], "true" | "false" | "null") {
                    push(&mut spans, start, i, palette.constant);
                }
            }
            _ => {
                // Whitespace, stray characters, and multibyte text outside
                // strings: skip a whole UTF-8 character.
                i += utf8_len(b);
            }
        }
    }
    spans
}

/// Tokenize an XML prefix: tags, attributes, values, comments, entities.
pub fn xml_spans(text: &str, palette: &SyntaxPalette) -> Vec<TextSpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if text[i..].starts_with("<!--") {
            let end = text[i + 4..]
                .find("-->")
                .map_or(bytes.len(), |p| i + 4 + p + 3);
            push(&mut spans, i, end, palette.comment);
            i = end;
            continue;
        }
        if text[i..].starts_with("<![CDATA[") {
            let end = text[i..].find("]]>").map_or(bytes.len(), |p| i + p + 3);
            push(&mut spans, i, end, palette.string);
            i = end;
            continue;
        }
        match bytes[i] {
            b'<' => i = xml_tag(text, i, palette, &mut spans),
            b'&' => {
                let end = text[i..]
                    .find(';')
                    .filter(|p| *p <= 10)
                    .map_or(i + 1, |p| i + p + 1);
                if end > i + 1 {
                    push(&mut spans, i, end, palette.constant);
                }
                i = end;
            }
            b => i += utf8_len(b),
        }
    }
    spans
}

/// Lex one tag starting at the `<` at `start`; returns the index after it.
fn xml_tag(text: &str, start: usize, palette: &SyntaxPalette, spans: &mut Vec<TextSpan>) -> usize {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    if matches!(bytes.get(i), Some(b'/' | b'?' | b'!')) {
        i += 1;
    }
    push(spans, start, i, palette.punctuation);
    let name_start = i;
    while i < bytes.len() && is_xml_name_byte(bytes[i]) {
        i += 1;
    }
    push(spans, name_start, i, palette.tag);
    while i < bytes.len() {
        match bytes[i] {
            b'>' => {
                push(spans, i, i + 1, palette.punctuation);
                return i + 1;
            }
            b'/' | b'?' if bytes.get(i + 1) == Some(&b'>') => {
                push(spans, i, i + 2, palette.punctuation);
                return i + 2;
            }
            b'=' => {
                push(spans, i, i + 1, palette.punctuation);
                i += 1;
            }
            quote @ (b'"' | b'\'') => {
                let value_start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                push(spans, value_start, i, palette.string);
            }
            b if is_xml_name_byte(b) => {
                let attr_start = i;
                while i < bytes.len() && is_xml_name_byte(bytes[i]) {
                    i += 1;
                }
                push(spans, attr_start, i, palette.key);
            }
            b'<' => return i,
            b => i += utf8_len(b),
        }
    }
    i
}

fn is_xml_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':') || b >= 0x80
}

/// Bytes in the UTF-8 character starting with `b` (1 for continuation
/// bytes, so a scan can never stall).
fn utf8_len(b: u8) -> usize {
    match b {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> SyntaxPalette {
        let c = |r: f32| RgbaColor::new(r, 0.0, 0.0, 1.0);
        SyntaxPalette {
            key: c(0.1),
            string: c(0.2),
            number: c(0.3),
            constant: c(0.4),
            punctuation: c(0.5),
            tag: c(0.6),
            comment: c(0.7),
        }
    }

    /// `(token text, color)` pairs for readable assertions.
    fn tokens<'a>(text: &'a str, spans: &[TextSpan]) -> Vec<(&'a str, RgbaColor)> {
        spans
            .iter()
            .map(|s| (&text[s.range.clone()], s.color))
            .collect()
    }

    #[test]
    fn json_keys_strings_numbers_and_literals() {
        let p = palette();
        let text = r#"{"name": "Ada", "age": -36.5e1, "ok": true, "x": null, "tags": ["a\"b"]}"#;
        let toks = tokens(text, &json_spans(text, &p));
        assert!(toks.contains(&(r#""name""#, p.key)));
        assert!(toks.contains(&(r#""Ada""#, p.string)));
        assert!(toks.contains(&("-36.5e1", p.number)));
        assert!(toks.contains(&("true", p.constant)));
        assert!(toks.contains(&("null", p.constant)));
        assert!(toks.contains(&(r#""a\"b""#, p.string)), "escaped quote");
        assert!(toks.contains(&("{", p.punctuation)));
        assert!(toks.contains(&(":", p.punctuation)));
    }

    #[test]
    fn json_cut_mid_string_and_multibyte_stay_on_boundaries() {
        let p = palette();
        let text = r#"{"città": "Zürich, très long"#;
        let spans = json_spans(text, &p);
        for span in &spans {
            assert!(text.is_char_boundary(span.range.start));
            assert!(text.is_char_boundary(span.range.end));
        }
        let toks = tokens(text, &spans);
        assert_eq!(toks.last(), Some(&(r#""Zürich, très long"#, p.string)));
        // Stray multibyte text outside strings never panics.
        let odd = "{é: ü}";
        let _ = json_spans(odd, &p);
    }

    #[test]
    fn xml_tags_attributes_values_comments() {
        let p = palette();
        let text = r#"<?xml version="1.0"?><root id='7'><!-- note --><a:b x="1"/>&amp;</root>"#;
        let toks = tokens(text, &xml_spans(text, &p));
        assert!(toks.contains(&("xml", p.tag)));
        assert!(toks.contains(&("version", p.key)));
        assert!(toks.contains(&(r#""1.0""#, p.string)));
        assert!(toks.contains(&("root", p.tag)));
        assert!(toks.contains(&("'7'", p.string)));
        assert!(toks.contains(&("<!-- note -->", p.comment)));
        assert!(toks.contains(&("a:b", p.tag)));
        assert!(toks.contains(&("/>", p.punctuation)));
        assert!(toks.contains(&("&amp;", p.constant)));
        assert!(toks.contains(&("</", p.punctuation)));
        // A prefix cut inside a tag still lexes what it has.
        let cut = r#"<order status="ope"#;
        let toks = tokens(cut, &xml_spans(cut, &p));
        assert_eq!(toks.last(), Some(&(r#""ope"#, p.string)));
    }

    #[test]
    fn sniffing_accepts_documents_and_rejects_prose() {
        assert_eq!(sniff(r#"  {"a": 1}"#), Some(Lang::Json));
        assert_eq!(sniff("{}"), Some(Lang::Json));
        assert_eq!(sniff("[1, 2, 3]"), Some(Lang::Json));
        assert_eq!(sniff(r#"[{"a":1}]"#), Some(Lang::Json));
        assert_eq!(sniff("[true]"), Some(Lang::Json));
        assert_eq!(sniff("<note><to>Ada</to></note>"), Some(Lang::Xml));
        assert_eq!(sniff("[INFO] started"), None);
        assert_eq!(sniff("[1] first item"), None);
        assert_eq!(sniff("{placeholder}"), None);
        assert_eq!(sniff("< 5 items"), None);
        assert_eq!(sniff("plain text"), None);
        assert_eq!(sniff(""), None);
    }

    #[test]
    fn styler_colors_declared_and_sniffed_columns_only() {
        let col = |name: &str, data_type: &str| ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        };
        let columns = [
            col("Doc", "jsonb"),
            col("feed", "xml"),
            col("note", "text"),
            col("n", "int"),
        ];
        let styler =
            StructuredCellStyler::for_columns(Some(DatabaseType::Postgres), &columns, palette())
                .expect("styled columns");
        assert!(styler.styles_column("doc"));
        assert!(styler.styles_column("FEED"));
        assert!(styler.styles_column("note"));
        assert!(!styler.styles_column("n"));
        // Declared JSON is lexed even when it does not open like an object.
        assert!(!styler.spans("Doc", "42").is_empty());
        assert!(styler.spans("note", "just words").is_empty());
        assert!(!styler.spans("note", r#"{"a":1}"#).is_empty());
        assert!(StructuredCellStyler::for_columns(None, &[col("n", "int")], palette()).is_none());
    }
}
