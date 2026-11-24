//! Line-at-a-time Markdown colouring for notebook Markdown cells: block
//! markers (headings, lists, task boxes, quotes, tables, dividers, code
//! fences) and inline spans (code, emphasis, strikethrough, links and
//! images, `{{…}}` references, HTML comments). One pass per line; the only
//! state carried across lines is whether a line sits inside a fenced code
//! block ([`fence_states`]).

use gpui::{Font, FontStyle, FontWeight, Hsla, StrikethroughStyle, TextRun, px};

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkdownStyle {
    Heading,
    /// List bullets and numbers, task boxes, quote markers.
    Marker,
    Code,
    /// A code fence line (marker and language).
    Fence,
    Emphasis,
    Strong,
    Strike,
    LinkText,
    LinkTarget,
    Reference,
    /// HTML comments and dividers.
    Comment,
    TablePipe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownSpan {
    pub start: usize,
    pub end: usize,
    pub style: MarkdownStyle,
}

/// The fence character and run length when `line` opens or closes a fenced
/// code block: up to three spaces, then three or more backticks or tildes.
pub(crate) fn fence_marker(line: &str) -> Option<(u8, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let c = *trimmed.as_bytes().first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let run = trimmed.bytes().take_while(|&b| b == c).count();
    (run >= 3).then_some((c, run))
}

/// For each line of `text`: whether it starts inside a fenced code block
/// (the closing fence line does; the opening one does not).
pub(crate) fn fence_states(text: &str) -> Vec<bool> {
    let mut out = Vec::with_capacity(text.bytes().filter(|b| *b == b'\n').count() + 1);
    let mut open: Option<(u8, usize)> = None;
    for line in text.split('\n') {
        out.push(open.is_some());
        match (open, fence_marker(line)) {
            (None, Some(marker)) => open = Some(marker),
            (Some((c, run)), Some((c2, run2)))
                if c == c2 && run2 >= run && closes_fence(line, c) =>
            {
                open = None
            }
            _ => {}
        }
    }
    out
}

fn closes_fence(line: &str, c: u8) -> bool {
    line.trim_start_matches(' ')
        .trim_start_matches(c as char)
        .trim()
        .is_empty()
}

fn heading(rest: &str) -> bool {
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && rest[hashes..].chars().next().is_none_or(|c| c == ' ')
}

fn divider(rest: &str) -> bool {
    let Some(c) = rest.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    let mut count = 0;
    for ch in rest.trim_end().chars() {
        if ch == c {
            count += 1;
        } else if ch != ' ' {
            return false;
        }
    }
    count >= 3
}

/// Byte length of the list marker at the start of `rest` (`-`, `*`, `+`,
/// `1.`, `1)`), when one is there and followed by a space or the line end.
pub(crate) fn list_marker_len(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let len = match bytes.first()? {
        b'-' | b'*' | b'+' => 1,
        b'0'..=b'9' => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return None;
            }
            digits + 1
        }
        _ => return None,
    };
    matches!(bytes.get(len), None | Some(b' ')).then_some(len)
}

/// Byte length of a task box (`[ ]`, `[x]`) at the start of `rest`.
pub(crate) fn task_box_len(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    (bytes.len() >= 3
        && bytes[0] == b'['
        && matches!(bytes[1], b' ' | b'x' | b'X')
        && bytes[2] == b']'
        && matches!(bytes.get(3), None | Some(b' ')))
    .then_some(3)
}

fn table_delimiter_row(rest: &str) -> bool {
    rest.contains('-')
        && rest
            .bytes()
            .all(|b| matches!(b, b'|' | b'-' | b':' | b' ' | b'\t'))
}

fn word_byte(b: Option<&u8>) -> bool {
    b.is_some_and(|b| b.is_ascii_alphanumeric() || *b >= 0x80)
}

/// The styled spans of one line, sorted and non-overlapping.
pub(crate) fn classify(line: &str, in_fence: bool) -> Vec<MarkdownSpan> {
    let whole = |style| {
        vec![MarkdownSpan {
            start: 0,
            end: line.len(),
            style,
        }]
    };
    if line.is_empty() {
        return Vec::new();
    }
    if in_fence {
        return whole(if fence_marker(line).is_some() {
            MarkdownStyle::Fence
        } else {
            MarkdownStyle::Code
        });
    }
    if fence_marker(line).is_some() {
        return whole(MarkdownStyle::Fence);
    }
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let rest = &line[indent..];
    if indent <= 3 && heading(rest) {
        return whole(MarkdownStyle::Heading);
    }
    if indent <= 3 && divider(rest) {
        return whole(MarkdownStyle::Comment);
    }
    let mut spans = Vec::new();
    let mut at = indent;
    while line[at..].starts_with('>') {
        spans.push(MarkdownSpan {
            start: at,
            end: at + 1,
            style: MarkdownStyle::Marker,
        });
        at += 1;
        if line[at..].starts_with(' ') {
            at += 1;
        }
    }
    if let Some(len) = list_marker_len(&line[at..]) {
        spans.push(MarkdownSpan {
            start: at,
            end: at + len,
            style: MarkdownStyle::Marker,
        });
        at += len;
        let after = at + line[at..].len() - line[at..].trim_start_matches(' ').len();
        if let Some(len) = task_box_len(&line[after..]) {
            spans.push(MarkdownSpan {
                start: after,
                end: after + len,
                style: MarkdownStyle::Marker,
            });
            at = after + len;
        }
    }
    let table = line[at..].trim_start().starts_with('|');
    if table && table_delimiter_row(&line[at..]) {
        spans.push(MarkdownSpan {
            start: at,
            end: line.len(),
            style: MarkdownStyle::TablePipe,
        });
        return spans;
    }
    inline(line, at, table, &mut spans);
    spans
}

fn inline(line: &str, from: usize, table: bool, spans: &mut Vec<MarkdownSpan>) {
    let bytes = line.as_bytes();
    let mut push = |start: usize, end: usize, style| {
        spans.push(MarkdownSpan { start, end, style });
    };
    let mut i = from;
    while i < bytes.len() {
        let rest = &line[i..];
        match bytes[i] {
            b'\\' => {
                i += 1 + rest[1..].chars().next().map_or(0, char::len_utf8);
                continue;
            }
            b'`' => {
                let run = rest.bytes().take_while(|&b| b == b'`').count();
                let ticks = &rest[..run];
                let close = rest[run..]
                    .match_indices(ticks)
                    .find(|(at, _)| rest.as_bytes().get(run + at + run) != Some(&b'`'));
                match close {
                    Some((at, _)) => {
                        let end = i + run + at + run;
                        push(i, end, MarkdownStyle::Code);
                        i = end;
                    }
                    None => i += run,
                }
                continue;
            }
            b'{' if rest.starts_with("{{") => {
                if let Some(close) = rest[2..].find("}}") {
                    let end = i + 2 + close + 2;
                    push(i, end, MarkdownStyle::Reference);
                    i = end;
                    continue;
                }
            }
            b'<' if rest.starts_with("<!--") => {
                let end = rest[4..]
                    .find("-->")
                    .map_or(line.len(), |at| i + 4 + at + 3);
                push(i, end, MarkdownStyle::Comment);
                i = end;
                continue;
            }
            b'!' | b'[' => {
                let open = if bytes[i] == b'!' { 2 } else { 1 };
                if (open == 1 || rest.starts_with("!["))
                    && let Some(close) = rest[open..].find(']')
                    && rest[open + close + 1..].starts_with('(')
                    && let Some(paren) = rest[open + close + 2..].find(')')
                {
                    let text_end = i + open + close + 1;
                    let end = text_end + 1 + paren + 1;
                    push(i, text_end, MarkdownStyle::LinkText);
                    push(text_end, end, MarkdownStyle::LinkTarget);
                    i = end;
                    continue;
                }
            }
            b'*' | b'_' | b'~' => {
                let c = bytes[i];
                let double = bytes.get(i + 1) == Some(&c);
                let boundary = c == b'*' || !word_byte(i.checked_sub(1).and_then(|p| bytes.get(p)));
                if double && boundary {
                    let delim = &rest[..2];
                    if let Some(close) = rest[2..].find(delim).filter(|at| *at > 0) {
                        let end = i + 2 + close + 2;
                        let style = if c == b'~' {
                            MarkdownStyle::Strike
                        } else {
                            MarkdownStyle::Strong
                        };
                        push(i, end, style);
                        i = end;
                        continue;
                    }
                } else if c != b'~'
                    && boundary
                    && bytes.get(i + 1).is_some_and(|b| *b != b' ' && *b != c)
                    && let Some(close) = rest[1..].find(c as char)
                {
                    let end = i + 1 + close + 1;
                    let closes =
                        bytes[end - 2] != b' ' && (c == b'*' || !word_byte(bytes.get(end)));
                    if closes {
                        push(i, end, MarkdownStyle::Emphasis);
                        i = end;
                        continue;
                    }
                }
            }
            b'|' if table => push(i, i + 1, MarkdownStyle::TablePipe),
            _ => {}
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
}

fn color(style: MarkdownStyle) -> Hsla {
    match style {
        MarkdownStyle::Heading | MarkdownStyle::Marker => Theme::keyword(),
        MarkdownStyle::Code => Theme::string(),
        MarkdownStyle::Fence | MarkdownStyle::Comment => Theme::comment(),
        MarkdownStyle::Emphasis | MarkdownStyle::Strong => Theme::text(),
        MarkdownStyle::Strike => Theme::text_dim(),
        MarkdownStyle::LinkText => Theme::function(),
        MarkdownStyle::LinkTarget => Theme::type_name(),
        MarkdownStyle::Reference => Theme::variable(),
        MarkdownStyle::TablePipe => Theme::constant(),
    }
}

/// Text runs for one line of Markdown; their lengths always sum to the
/// line's length.
pub(crate) fn runs(line: &str, base_font: &Font, in_fence: bool) -> Vec<TextRun> {
    let run = |len: usize, style: Option<MarkdownStyle>| {
        let mut font = base_font.clone();
        match style {
            Some(MarkdownStyle::Heading | MarkdownStyle::Strong) => font.weight = FontWeight::BOLD,
            Some(MarkdownStyle::Emphasis) => font.style = FontStyle::Italic,
            _ => {}
        }
        TextRun {
            len,
            font,
            color: style.map_or_else(Theme::text, color),
            background_color: None,
            underline: None,
            strikethrough: (style == Some(MarkdownStyle::Strike)).then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: Some(Theme::text_dim()),
            }),
        }
    };
    let mut runs = Vec::new();
    let mut prev = 0;
    for span in classify(line, in_fence) {
        let start = span.start.max(prev);
        let end = span.end.min(line.len());
        if end <= start {
            continue;
        }
        if start > prev {
            runs.push(run(start - prev, None));
        }
        runs.push(run(end - start, Some(span.style)));
        prev = end;
    }
    if prev < line.len() || runs.is_empty() {
        runs.push(run(line.len() - prev, None));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use MarkdownStyle as S;

    fn styles(line: &str) -> Vec<(&str, MarkdownStyle)> {
        classify(line, false)
            .into_iter()
            .map(|s| (&line[s.start..s.end], s.style))
            .collect()
    }

    #[test]
    fn block_markers() {
        assert_eq!(styles("## Revenue"), vec![("## Revenue", S::Heading)]);
        assert_eq!(styles("#hashtag"), Vec::new());
        assert_eq!(styles("---"), vec![("---", S::Comment)]);
        assert_eq!(
            styles("- [x] done"),
            vec![("-", S::Marker), ("[x]", S::Marker)]
        );
        assert_eq!(styles("  12. item"), vec![("12.", S::Marker)]);
        assert_eq!(styles("> quoted"), vec![(">", S::Marker)]);
        assert_eq!(
            styles("| a | b |"),
            vec![
                ("|", S::TablePipe),
                ("|", S::TablePipe),
                ("|", S::TablePipe)
            ]
        );
        assert_eq!(
            styles("| --- | :-: |"),
            vec![("| --- | :-: |", S::TablePipe)]
        );
    }

    #[test]
    fn inline_spans() {
        assert_eq!(
            styles("a `code` and **bold** _it_ ~~gone~~"),
            vec![
                ("`code`", S::Code),
                ("**bold**", S::Strong),
                ("_it_", S::Emphasis),
                ("~~gone~~", S::Strike)
            ]
        );
        assert_eq!(
            styles("see [docs](https://x) ![p](attachment:trend)"),
            vec![
                ("[docs]", S::LinkText),
                ("(https://x)", S::LinkTarget),
                ("![p]", S::LinkText),
                ("(attachment:trend)", S::LinkTarget)
            ]
        );
        assert_eq!(
            styles("total {{avg_order}} <!-- note -->"),
            vec![
                ("{{avg_order}}", S::Reference),
                ("<!-- note -->", S::Comment)
            ]
        );
        assert_eq!(styles("snake_case_name and 2 * 3 * 4"), Vec::new());
        assert_eq!(styles(r"\*not\* emphasis"), Vec::new());
    }

    #[test]
    fn fences_carry_across_lines() {
        let text = "intro\n```sql\nSELECT 1 -- **x**\n```\nafter";
        assert_eq!(fence_states(text), vec![false, false, true, true, false]);
        assert_eq!(styles("```sql"), vec![("```sql", S::Fence)]);
        let inside = classify("SELECT 1 -- **x**", true);
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].style, S::Code);
        assert_eq!(fence_states("````\n```\n````"), vec![false, true, true]);
    }

    #[test]
    fn runs_cover_every_byte() {
        let font = gpui::font("Menlo");
        for line in [
            "",
            "plain",
            "## Héading",
            "- [ ] **b** `c` _é_ [l](u) {{x}} trailing",
            "unclosed `tick and **star",
            "| ü | ß |",
        ] {
            let total: usize = runs(line, &font, false).iter().map(|r| r.len).sum();
            assert_eq!(total, line.len(), "{line:?}");
        }
    }
}
