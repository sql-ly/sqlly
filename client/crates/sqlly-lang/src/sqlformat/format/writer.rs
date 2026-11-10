//! Output writing and alignment: the line [`Writer`], [`AlignTag`]-driven
//! alignment passes, and multi-line/block-comment emission helpers.

use super::Options;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum AlignTag {
    Comparison,
    Alias,
    DataType,
    Nullability,
    SetAssign,
    Comment,
}

#[derive(Default)]
pub(super) struct OutLine {
    /// Indent in units (tabs or `indent_width` spaces each).
    pub(super) indent: usize,
    /// Extra columns of spaces after the unit indent (paren alignment).
    pub(super) extra: usize,
    pub(super) text: String,
    /// (tag, char offset into `text`, alignment scope id)
    pub(super) aligns: Vec<(AlignTag, usize, u32)>,
    /// Set when a trailing line comment was appended after the line was
    /// finished — nothing more may follow it on this line.
    pub(super) ends_in_line_comment: bool,
}

impl OutLine {
    pub(super) fn width(&self, unit_width: usize) -> usize {
        self.indent * unit_width + self.extra + self.text.chars().count()
    }
}

/// Last significant thing written, for unary-operator detection.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum LastTok {
    None,
    Word,     // keyword that ends nothing (SELECT, AND, WHEN, …)
    Operand,  // identifier, number, string, `)`, param — an expression tail
    Operator, // symbol operator
    Open,     // `(` or `,`
}

pub(super) struct Writer<'o, 'p> {
    pub(super) o: &'o Options<'p>,
    pub(super) lines: Vec<OutLine>,
    pub(super) cur: OutLine,
    pub(super) started: bool,
    pub(super) pending_space: bool,
    /// Next push joins with no separator regardless of `pending_space`.
    pub(super) force_join: bool,
    pub(super) pending_blank_lines: usize,
    pub(super) align_scope: u32,
    pub(super) last: LastTok,
}

impl<'o, 'p> Writer<'o, 'p> {
    pub(super) fn new(o: &'o Options<'p>) -> Self {
        Self {
            o,
            lines: Vec::new(),
            cur: OutLine::default(),
            started: false,
            pending_space: false,
            force_join: false,
            pending_blank_lines: 0,
            align_scope: 0,
            last: LastTok::None,
        }
    }

    pub(super) fn cur_width(&self) -> usize {
        self.cur.width(self.o.indent_unit_width())
    }

    pub(super) fn push(&mut self, seg: &str) {
        if !self.force_join && self.pending_space && !self.cur.text.is_empty() {
            self.cur.text.push(' ');
        }
        self.force_join = false;
        self.pending_space = false;
        self.cur.text.push_str(seg);
        self.started = true;
    }

    pub(super) fn space(&mut self) {
        self.pending_space = true;
    }

    /// Suppress the separator before the next push.
    pub(super) fn join(&mut self) {
        self.force_join = true;
    }

    pub(super) fn mark(&mut self, tag: AlignTag) {
        let mut pos = self.cur.text.chars().count();
        if !self.force_join && self.pending_space && !self.cur.text.is_empty() {
            pos += 1;
        }
        self.cur.aligns.push((tag, pos, self.align_scope));
    }

    pub(super) fn new_scope(&mut self) {
        self.align_scope += 1;
    }

    /// Finish the current line and start a new one at `indent` units +
    /// `extra` columns. On a still-empty line this just moves the indent, so
    /// callers can request breaks unconditionally.
    pub(super) fn newline(&mut self, indent: usize, extra: usize) {
        self.pending_space = false;
        self.force_join = false;
        if self.cur.text.is_empty() {
            self.cur.indent = indent;
            self.cur.extra = extra;
            return;
        }
        let done = std::mem::replace(
            &mut self.cur,
            OutLine {
                indent,
                extra,
                ..OutLine::default()
            },
        );
        // Blank lines queued since the last flush belong *before* the line
        // being completed now (they were requested before its content).
        for _ in 0..self.pending_blank_lines {
            self.lines.push(OutLine::default());
        }
        self.pending_blank_lines = 0;
        self.lines.push(done);
    }

    /// Append ` text` to the last finished line (a trailing line comment that
    /// travels with the code it followed), optionally tagged for alignment.
    /// False when there is no finished line or it already ends in a line
    /// comment, which the appended text would swallow.
    pub(super) fn append_to_previous_line(&mut self, text: &str, tag: Option<AlignTag>) -> bool {
        let scope = self.align_scope;
        let Some(prev) = self.lines.last_mut() else {
            return false;
        };
        if prev.text.is_empty() || prev.ends_in_line_comment {
            return false;
        }
        if let Some(tag) = tag {
            prev.aligns
                .push((tag, prev.text.chars().count() + 1, scope));
        }
        prev.text.push(' ');
        prev.text.push_str(text);
        prev.ends_in_line_comment = true;
        true
    }

    /// Queue `n` blank lines to be emitted when the current line is flushed.
    pub(super) fn blank_lines(&mut self, n: usize) {
        if self.started {
            self.pending_blank_lines = self.pending_blank_lines.max(n);
        }
    }

    /// Break before a segment of `len` chars if it would overflow the line.
    pub(super) fn maybe_wrap(&mut self, len: usize, cont_indent: usize, cont_extra: usize) {
        if !self.o.p.wrap_long_lines {
            return;
        }
        let sep = if self.pending_space && !self.force_join {
            1
        } else {
            0
        };
        if !self.cur.text.is_empty() && self.cur_width() + sep + len > self.o.max_line() {
            self.newline(cont_indent, cont_extra);
        }
    }

    pub(super) fn finish(mut self) -> String {
        if !self.cur.text.is_empty() {
            for _ in 0..self.pending_blank_lines {
                self.lines.push(OutLine::default());
            }
            self.pending_blank_lines = 0;
            let done = std::mem::take(&mut self.cur);
            self.lines.push(done);
        }
        apply_alignment(&mut self.lines, self.o.indent_unit_width());
        let unit = self.o.indent_unit();
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if !line.text.is_empty() {
                for _ in 0..line.indent {
                    out.push_str(&unit);
                }
                for _ in 0..line.extra {
                    out.push(' ');
                }
                out.push_str(&line.text);
            }
        }
        out
    }
}

/// Pad tagged line groups so tags line up. Groups are runs of consecutive
/// lines sharing a scope id and carrying the same tag.
fn apply_alignment(lines: &mut [OutLine], unit_width: usize) {
    let tags = [
        AlignTag::Comparison,
        AlignTag::Alias,
        AlignTag::DataType,
        AlignTag::Nullability,
        AlignTag::SetAssign,
        AlignTag::Comment,
    ];
    for tag in tags {
        let mut i = 0;
        while i < lines.len() {
            let Some(scope) = align_entry(&lines[i], tag) else {
                i += 1;
                continue;
            };
            let mut j = i;
            while j < lines.len() && align_entry(&lines[j], tag) == Some(scope) {
                j += 1;
            }
            if j - i > 1 {
                let max_col = (i..j)
                    .map(|k| align_col(&lines[k], tag, unit_width))
                    .max()
                    .unwrap_or(0);
                for line in lines[i..j].iter_mut() {
                    pad_to(line, tag, max_col, unit_width);
                }
            }
            i = j;
        }
    }
}

fn align_entry(line: &OutLine, tag: AlignTag) -> Option<u32> {
    line.aligns
        .iter()
        .find(|(t, _, _)| *t == tag)
        .map(|(_, _, s)| *s)
}

fn align_col(line: &OutLine, tag: AlignTag, unit_width: usize) -> usize {
    let pos = line
        .aligns
        .iter()
        .find(|(t, _, _)| *t == tag)
        .map(|(_, p, _)| *p)
        .unwrap_or(0);
    line.indent * unit_width + line.extra + pos
}

fn pad_to(line: &mut OutLine, tag: AlignTag, col: usize, unit_width: usize) {
    let cur = align_col(line, tag, unit_width);
    if cur >= col {
        return;
    }
    let pad = col - cur;
    let pos = line
        .aligns
        .iter()
        .find(|(t, _, _)| *t == tag)
        .map(|(_, p, _)| *p)
        .unwrap_or(0);
    let byte_pos = char_to_byte(&line.text, pos);
    line.text.insert_str(byte_pos, &" ".repeat(pad));
    for (_, p, _) in line.aligns.iter_mut() {
        if *p >= pos {
            *p += pad;
        }
    }
}

fn char_to_byte(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

pub(super) fn push_multiline(w: &mut Writer, text: &str) {
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            let indent = w.cur.indent;
            let extra = w.cur.extra;
            w.newline(indent, extra);
        }
        w.push(line);
        first = false;
    }
}

pub(super) fn reindent_block_comment(
    text: &str,
    indent: usize,
    extra: usize,
    o: &Options,
) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() < 2 {
        return text.to_string();
    }
    let unit = o.indent_unit();
    let mut prefix = unit.repeat(indent);
    prefix.push_str(&" ".repeat(extra));
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(line.trim_end());
        } else {
            out.push('\n');
            let body = line.trim();
            out.push_str(&prefix);
            if body.starts_with('*') {
                out.push(' ');
            }
            out.push_str(body);
        }
    }
    out
}
