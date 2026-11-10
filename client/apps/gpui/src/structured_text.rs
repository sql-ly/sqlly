//! YAML and Markdown for the value viewer: cautious detection (a plain
//! sentence with a colon in it must stay text), syntax spans for the Raw
//! view, and the YAML parse behind the Tree view.
//!
//! Pure and AppKit-free like `structured_data.rs`, which calls in here from
//! `detect`, `classify`, and `build_tree`.

use crate::structured_data::{SyntaxSpan, SyntaxToken};

/// Values longer than this are never sniffed as YAML or Markdown (the
/// detection runs on context-menu builds and Cell Details selection).
pub const SNIFF_MAX_BYTES: usize = 1024 * 1024;

use std::sync::OnceLock;

use regex::Regex;

fn yaml_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // A plain key (identifier-like, no spaces) or a quoted key, then `:`
    // followed by whitespace or the end of the line.
    RE.get_or_init(|| {
        Regex::new(r#"^(?:[A-Za-z_][A-Za-z0-9_.\-/]*|"[^"\n]*"|'[^'\n]*')[ \t]*:(?:[ \t]|$)"#)
            .unwrap()
    })
}

/// Leading-space count, or `None` when the indentation uses a tab (which
/// YAML forbids).
fn yaml_indent(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if line[indent..].starts_with('\t') {
        return None;
    }
    Some(indent)
}

fn is_list_item(body: &str) -> bool {
    body == "-" || body.starts_with("- ")
}

/// True when `trimmed` is a YAML document with real structure: either it
/// opens with a `---` document marker, or it has at least two top-level
/// `key: value` lines plus a third structural line (another key, a nested
/// block, or a list item). Every top-level line must be a key, a list item,
/// a comment, or a document marker, and the whole text must parse to a
/// mapping (or, after `---`, a mapping or a sequence).
pub fn is_yaml(trimmed: &str) -> bool {
    if trimmed.is_empty() || trimmed.len() > SNIFF_MAX_BYTES {
        return false;
    }
    let first = trimmed.lines().next().unwrap_or("").trim_end();
    let doc_start = first == "---" || first.starts_with("--- ");
    let (mut top_keys, mut structural, mut nested) = (0usize, 0usize, false);
    for line in trimmed.lines() {
        let line = line.trim_end();
        let body = line.trim_start();
        if body.is_empty() || body.starts_with('#') || line == "---" || line == "..." {
            continue;
        }
        let Some(indent) = yaml_indent(line) else {
            return false;
        };
        if indent > 0 {
            nested = true;
            structural += 1;
            continue;
        }
        if is_list_item(body) {
            structural += 1;
            continue;
        }
        if yaml_key_re().is_match(body) {
            top_keys += 1;
            structural += 1;
            continue;
        }
        // Prose at the top level: not a YAML document.
        return false;
    }
    let shaped = doc_start || (top_keys >= 2 && (structural >= 3 || nested));
    if !shaped {
        return false;
    }
    match parse_yaml(trimmed) {
        Some(serde_json::Value::Object(map)) => !map.is_empty(),
        Some(serde_json::Value::Array(items)) => doc_start && !items.is_empty(),
        _ => false,
    }
}

/// Parse YAML text into a JSON value for the tree view. `None` when it does
/// not parse, has keys JSON cannot hold, or exceeds the parser's budget.
pub fn parse_yaml(text: &str) -> Option<serde_json::Value> {
    if text.len() > SNIFF_MAX_BYTES * 16 {
        return None;
    }
    serde_saphyr::from_str::<serde_json::Value>(text).ok()
}

fn md_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^#{1,6}[ \t]+\S").unwrap())
}
fn md_list_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[ \t]*(?:[-*+]|\d{1,3}[.)])[ \t]+\S").unwrap())
}
fn md_table_rule_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\|?[ \t]*:?-{3,}:?[ \t]*(?:\|[ \t]*:?-{3,}:?[ \t]*)+\|?$").unwrap()
    })
}
fn md_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\[[^\]\n]+\])(\([^)\s]+\))").unwrap())
}
fn md_emphasis_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*[^*\n]+\*\*|__[^_\n]+__").unwrap())
}
fn md_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`\n]+`").unwrap())
}

fn is_fence(line: &str) -> bool {
    let body = line.trim_start();
    body.starts_with("```") || body.starts_with("~~~")
}

/// True when `trimmed` carries Markdown structure: at least two non-blank
/// lines and either a fenced code block, a table, or two different kinds of
/// Markdown signal (headings, lists, block quotes, links, emphasis, inline
/// code).
pub fn is_markdown(trimmed: &str) -> bool {
    if trimmed.is_empty() || trimmed.len() > SNIFF_MAX_BYTES {
        return false;
    }
    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        return false;
    }
    let fences = lines.iter().filter(|l| is_fence(l)).count();
    if fences >= 2 {
        return true;
    }
    if lines.iter().any(|l| md_table_rule_re().is_match(l.trim()))
        && lines.iter().filter(|l| l.contains('|')).count() >= 2
    {
        return true;
    }
    let headings = lines.iter().any(|l| md_heading_re().is_match(l));
    let lists = lines.iter().filter(|l| md_list_re().is_match(l)).count() >= 2
        || (lines.iter().any(|l| md_list_re().is_match(l)) && headings);
    let quotes = lines.iter().any(|l| l.trim_start().starts_with('>'));
    let links = md_link_re().is_match(trimmed);
    let emphasis = md_emphasis_re().is_match(trimmed);
    let code = md_code_re().is_match(trimmed);
    [headings, lists, quotes, links, emphasis, code]
        .into_iter()
        .filter(|signal| *signal)
        .count()
        >= 2
}

fn push(spans: &mut Vec<SyntaxSpan>, start: usize, end: usize, token: SyntaxToken) {
    if start < end {
        spans.push(SyntaxSpan { start, end, token });
    }
}

/// Sort and drop any span overlapping an earlier-starting one.
fn tidy(mut spans: Vec<SyntaxSpan>) -> Vec<SyntaxSpan> {
    spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
    let mut out: Vec<SyntaxSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        if out.last().is_none_or(|last| last.end <= span.start) {
            out.push(span);
        }
    }
    out
}

/// Where an unquoted `#` comment starts in a YAML value (`#` at the start
/// or after whitespace, outside quotes).
fn yaml_comment_start(value: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut previous = ' ';
    for (i, c) in value.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' && previous.is_whitespace() => return Some(i),
            None => {}
        }
        previous = c;
    }
    None
}

fn yaml_scalar_token(value: &str) -> Option<SyntaxToken> {
    if value.is_empty() {
        return None;
    }
    if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
    {
        return Some(SyntaxToken::String);
    }
    match value {
        "~" | "null" | "Null" | "NULL" => return Some(SyntaxToken::Null),
        "true" | "false" | "True" | "False" | "TRUE" | "FALSE" | "yes" | "no" | "on" | "off" => {
            return Some(SyntaxToken::Boolean);
        }
        _ => {}
    }
    if value.parse::<f64>().is_ok() || value.starts_with("0x") && value.len() > 2 {
        return Some(SyntaxToken::Number);
    }
    if value.starts_with('&') || value.starts_with('*') || value.starts_with('!') {
        return Some(SyntaxToken::AttributeName);
    }
    None
}

/// Syntax spans for YAML, sorted and non-overlapping.
pub fn classify_yaml(text: &str) -> Vec<SyntaxSpan> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for raw_line in text.split('\n') {
        let line_start = offset;
        offset += raw_line.len() + 1;
        let line = raw_line.trim_end_matches('\r');
        let indent = line.len() - line.trim_start().len();
        let mut at = indent;
        let mut body = &line[at..];
        if body.starts_with('#') {
            push(
                &mut spans,
                line_start + at,
                line_start + line.len(),
                SyntaxToken::Comment,
            );
            continue;
        }
        if body == "---" || body == "..." {
            push(
                &mut spans,
                line_start + at,
                line_start + line.len(),
                SyntaxToken::Punctuation,
            );
            continue;
        }
        // Sequence markers, possibly several (`- - x`).
        while is_list_item(body) {
            push(
                &mut spans,
                line_start + at,
                line_start + at + 1,
                SyntaxToken::Punctuation,
            );
            let rest = &body[1..];
            let skip = 1 + (rest.len() - rest.trim_start().len());
            at += skip;
            body = &line[at..];
        }
        if let Some(found) = yaml_key_re().find(body) {
            let key_text = found.as_str();
            let colon = key_text.rfind(':').unwrap_or(0);
            let key_end = key_text[..colon].trim_end().len();
            push(
                &mut spans,
                line_start + at,
                line_start + at + key_end,
                SyntaxToken::Key,
            );
            push(
                &mut spans,
                line_start + at + colon,
                line_start + at + colon + 1,
                SyntaxToken::Punctuation,
            );
            at += colon + 1;
            body = &line[at..];
        }
        let lead = body.len() - body.trim_start().len();
        at += lead;
        let value_part = &line[at..];
        let (value, comment) = match yaml_comment_start(value_part) {
            Some(i) => (&value_part[..i], Some(i)),
            None => (value_part, None),
        };
        let value_trimmed = value.trim_end();
        if let Some(token) = yaml_scalar_token(value_trimmed) {
            push(
                &mut spans,
                line_start + at,
                line_start + at + value_trimmed.len(),
                token,
            );
        }
        if let Some(i) = comment {
            push(
                &mut spans,
                line_start + at + i,
                line_start + line.len(),
                SyntaxToken::Comment,
            );
        }
    }
    tidy(spans)
}

/// Syntax spans for Markdown, sorted and non-overlapping.
pub fn classify_markdown(text: &str) -> Vec<SyntaxSpan> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    let mut in_fence = false;
    for raw_line in text.split('\n') {
        let line_start = offset;
        offset += raw_line.len() + 1;
        let line = raw_line.trim_end_matches('\r');
        let end = line_start + line.len();
        if is_fence(line) {
            in_fence = !in_fence;
            push(&mut spans, line_start, end, SyntaxToken::Punctuation);
            continue;
        }
        if in_fence {
            push(&mut spans, line_start, end, SyntaxToken::String);
            continue;
        }
        if md_heading_re().is_match(line) {
            push(&mut spans, line_start, end, SyntaxToken::ElementName);
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = &line[indent..];
        if body.starts_with('>') {
            push(
                &mut spans,
                line_start + indent,
                line_start + indent + 1,
                SyntaxToken::Punctuation,
            );
        } else if md_list_re().is_match(line) {
            let marker = body.find([' ', '\t']).unwrap_or(1);
            push(
                &mut spans,
                line_start + indent,
                line_start + indent + marker,
                SyntaxToken::Punctuation,
            );
        } else if md_table_rule_re().is_match(line.trim()) {
            push(&mut spans, line_start, end, SyntaxToken::Punctuation);
            continue;
        }
        for caps in md_link_re().captures_iter(line) {
            if let (Some(label), Some(target)) = (caps.get(1), caps.get(2)) {
                push(
                    &mut spans,
                    line_start + label.start(),
                    line_start + label.end(),
                    SyntaxToken::AttributeName,
                );
                push(
                    &mut spans,
                    line_start + target.start(),
                    line_start + target.end(),
                    SyntaxToken::AttributeValue,
                );
            }
        }
        for m in md_code_re().find_iter(line) {
            push(
                &mut spans,
                line_start + m.start(),
                line_start + m.end(),
                SyntaxToken::String,
            );
        }
        for m in md_emphasis_re().find_iter(line) {
            push(
                &mut spans,
                line_start + m.start(),
                line_start + m.end(),
                SyntaxToken::Boolean,
            );
        }
    }
    tidy(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text<'a>(text: &'a str, spans: &[SyntaxSpan], token: SyntaxToken) -> Vec<&'a str> {
        spans
            .iter()
            .filter(|span| span.token == token)
            .map(|span| &text[span.start..span.end])
            .collect()
    }

    fn sorted_disjoint(spans: &[SyntaxSpan]) -> bool {
        spans
            .windows(2)
            .all(|pair| pair[0].end <= pair[1].start && pair[0].start < pair[0].end)
    }

    #[test]
    fn yaml_detects_documents_with_structure() {
        assert!(is_yaml("---\nname: api\nreplicas: 3"));
        assert!(is_yaml(
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: web\n  labels:\n    app: web"
        ));
        assert!(is_yaml("name: api\nport: 8080\ndebug: false"));
        assert!(is_yaml(
            "# config\nservers:\n  - alpha\n  - beta\nretries: 2"
        ));
        assert!(is_yaml("---\n- one\n- two"));
    }

    #[test]
    fn yaml_leaves_plain_text_alone() {
        // A sentence with a colon, a two-line note, a URL, a log line.
        assert!(!is_yaml("Note: call back tomorrow"));
        assert!(!is_yaml("Name: Ada\nRole: admin"));
        assert!(!is_yaml("see https://example.com/a:b"));
        assert!(!is_yaml("ERROR 2026-10-28 12:00:01 timeout: retrying"));
        assert!(!is_yaml("hello world\nthis is: not yaml\nat all"));
        // Lists without keys are not YAML-detected (they are just lines).
        assert!(!is_yaml("- milk\n- eggs\n- bread"));
        // Broken YAML never qualifies.
        assert!(!is_yaml("a: b: c\nd: e\nf: g"));
        assert!(!is_yaml(""));
    }

    #[test]
    fn yaml_parses_into_a_tree_value() {
        let value = parse_yaml("name: api\nports:\n  - 80\n  - 443\nenabled: true").unwrap();
        assert_eq!(value["name"], "api");
        assert_eq!(value["ports"][1], 443);
        assert_eq!(value["enabled"], true);
        assert!(parse_yaml("a: [unclosed").is_none());
    }

    #[test]
    fn markdown_detects_documents_with_structure() {
        assert!(is_markdown(
            "# Release notes\n\n- Faster grid\n- New viewer"
        ));
        assert!(is_markdown("Some intro\n\n```sql\nselect 1;\n```"));
        assert!(is_markdown("| a | b |\n|---|---|\n| 1 | 2 |"));
        assert!(is_markdown(
            "See the [docs](https://sqlly.app/docs) for **details**.\nThanks!"
        ));
        assert!(is_markdown("## Setup\n> Note: run `make` first"));
    }

    #[test]
    fn markdown_leaves_plain_text_alone() {
        assert!(!is_markdown("just a sentence"));
        assert!(!is_markdown("# only a heading"));
        assert!(!is_markdown("line one\nline two"));
        assert!(!is_markdown("- milk\n- eggs"));
        assert!(!is_markdown("price is 5 * 3 = 15\nok"));
        assert!(!is_markdown(""));
    }

    #[test]
    fn yaml_spans_mark_keys_values_and_comments() {
        let text =
            "# settings\nname: \"api\"\nport: 8080\nenabled: true\nnothing: ~\nlist:\n  - item";
        let spans = classify_yaml(text);
        assert!(sorted_disjoint(&spans));
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::Key),
            vec!["name", "port", "enabled", "nothing", "list"]
        );
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::Comment),
            vec!["# settings"]
        );
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::String),
            vec!["\"api\""]
        );
        assert_eq!(spans_text(text, &spans, SyntaxToken::Number), vec!["8080"]);
        assert_eq!(spans_text(text, &spans, SyntaxToken::Boolean), vec!["true"]);
        assert_eq!(spans_text(text, &spans, SyntaxToken::Null), vec!["~"]);
    }

    #[test]
    fn markdown_spans_mark_headings_code_and_links() {
        let text = "# Title\n- see [docs](https://x.y) and `code`\n```\nlet x = 1;\n```\n**bold**";
        let spans = classify_markdown(text);
        assert!(sorted_disjoint(&spans));
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::ElementName),
            vec!["# Title"]
        );
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::AttributeName),
            vec!["[docs]"]
        );
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::AttributeValue),
            vec!["(https://x.y)"]
        );
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::String),
            vec!["`code`", "let x = 1;"]
        );
        assert_eq!(
            spans_text(text, &spans, SyntaxToken::Boolean),
            vec!["**bold**"]
        );
    }

    #[test]
    fn the_viewer_detects_and_renders_yaml_and_markdown() {
        use crate::structured_data::{
            ContentPreview, StructuredDataKind, ValueFormat, build_tree, classify_cell, detect,
            render_payload,
        };
        use sqlly_core::CellValue;

        let yaml = "name: api\nports:\n  - 80\n  - 443";
        let markdown = "# Notes\n\n- first\n- second";
        assert_eq!(detect(yaml), Some(StructuredDataKind::Yaml));
        assert_eq!(detect(markdown), Some(StructuredDataKind::Markdown));
        // JSON stays JSON even though JSON is also YAML.
        assert_eq!(detect("{\"a\": 1}"), Some(StructuredDataKind::Json));
        assert_eq!(detect("Note: call back"), None);
        assert_eq!(
            classify_cell(&CellValue::String(yaml.into())),
            ContentPreview::Yaml
        );
        assert_eq!(
            classify_cell(&CellValue::String(markdown.into())),
            ContentPreview::Markdown
        );

        let tree = build_tree(yaml, StructuredDataKind::Yaml).unwrap();
        assert_eq!(tree.children.len(), 2);
        assert_eq!(tree.children[1].path, "$.ports");
        assert!(build_tree(markdown, StructuredDataKind::Markdown).is_none());

        let rendered = render_payload(yaml.as_bytes(), ValueFormat::Yaml);
        assert!(rendered.as_yaml && rendered.note.is_none());
        assert_eq!(rendered.decoded, Some(ValueFormat::Yaml));
        let not_yaml = render_payload(b"a: [unclosed", ValueFormat::Yaml);
        assert!(!not_yaml.as_yaml && not_yaml.note.is_some());
        assert_eq!(not_yaml.text, "a: [unclosed");
        let md = render_payload(b"plain words", ValueFormat::Markdown);
        assert!(md.as_markdown);
        assert_eq!(md.text, "plain words");
    }

    #[test]
    fn compressed_yaml_opens_as_yaml() {
        use std::io::Write as _;

        use crate::structured_data::{ValueFormat, render_payload};

        let yaml = "kind: Service\nmetadata:\n  name: web";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(yaml.as_bytes()).unwrap();
        let gz = encoder.finish().unwrap();
        let rendered = render_payload(&gz, ValueFormat::Gzip);
        assert!(rendered.as_yaml, "{:?}", rendered.note);
        assert_eq!(rendered.decoded, Some(ValueFormat::Gzip));
        assert!(rendered.note.unwrap_or_default().contains("YAML"));
    }

    #[test]
    fn spans_survive_multibyte_text() {
        let text = "título: \"café\" # ñ\nclé: été\nweiß: ja";
        let spans = classify_yaml(text);
        assert!(sorted_disjoint(&spans));
        for span in &spans {
            assert!(text.is_char_boundary(span.start) && text.is_char_boundary(span.end));
        }
        let md = "# Café\n- ñandú [enlace](https://é.x) `código`";
        for span in classify_markdown(md) {
            assert!(md.is_char_boundary(span.start) && md.is_char_boundary(span.end));
        }
    }
}
