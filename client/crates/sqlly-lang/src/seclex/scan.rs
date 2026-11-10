//! The scanner: finds where each engine's literals, quoted identifiers and
//! comments begin and end under one [`Rules`] reading, and writes the
//! *shadow* — a byte-for-byte same-length copy of the text in which only live
//! code survives.
//!
//! Shadow contract (what consumers may rely on):
//! - Same byte length as the input, valid UTF-8, so spans index both.
//! - Every terminated string literal (any prefix/quoting form) becomes `'`,
//!   spaces (newlines kept), `'`.
//! - Every terminated quoted identifier becomes `[`, its body with `[`/`]`
//!   replaced by `_`, `]` — a bracket identifier the dialect-invariant
//!   tokenizer reads as one identifier token, never as a keyword.
//! - Every comment becomes spaces (newlines kept). An executable comment that
//!   the reading runs has only its markers blanked; its payload is live code.
//! - An unterminated construct has its *opener* blanked and the text after it
//!   is lexed again as code, so nothing can hide inside it.
//! - Raw `[`/`]` in code become spaces, and a `--` that is not a comment
//!   (MySQL) has its second dash blanked, so the shadow never contains a
//!   quote, bracket or comment opener the reading did not see.

use super::rules::{Bracket, Dollar, DoubleQuote, ExecDecision, HashComment, Rules};

/// What kind of construct begins at a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructKind {
    LineComment,
    BlockComment,
    /// A string literal; `prefix_len` bytes (`N`, `E`, `q`, `nq`, `b`)
    /// precede the opening quote.
    Str {
        prefix_len: usize,
    },
    /// `$tag$…$tag$` (PostgreSQL dollar quote, ClickHouse heredoc).
    DollarQuoted,
    /// A quoted identifier opened by this byte (`"`, `` ` ``, `[`).
    QuotedIdent {
        open: u8,
    },
    /// A SQLite variable (`$a`, `@a`, `:a`, `#a`, TCL `$a(…)`).
    Variable,
    /// The marker of an executable comment whose payload is live code.
    ExecOpen {
        marker_len: usize,
    },
}

/// A construct found at a position. `end` is exclusive; when `terminated` is
/// false the construct ran off the end of the input and `end == len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Construct {
    pub kind: ConstructKind,
    pub start: usize,
    pub end: usize,
    pub terminated: bool,
    /// Length of the opening delimiter — what gets blanked when the construct
    /// is unterminated (the rest is then lexed as code).
    pub opener_len: usize,
}

/// An unterminated construct the scanner re-lexed as code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unterminated {
    pub what: Unclosed,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unclosed {
    String,
    QuotedIdentifier,
    BlockComment,
    DollarQuote,
    ExecutableComment,
}

impl Unclosed {
    pub fn describe(self) -> &'static str {
        match self {
            Unclosed::String => "string literal",
            Unclosed::QuotedIdentifier => "quoted identifier",
            Unclosed::BlockComment => "block comment",
            Unclosed::DollarQuote => "dollar-quoted string",
            Unclosed::ExecutableComment => "executable comment",
        }
    }
}

pub(crate) struct Scan {
    pub shadow: String,
    pub unterminated: Option<Unterminated>,
}

/// Find the construct (if any) that begins at `pos`, which must be a token
/// start in code (so a prefix letter here cannot belong to a preceding word:
/// the word would have consumed it).
pub(crate) fn construct_at(bytes: &[u8], pos: usize, rules: &Rules) -> Option<Construct> {
    let b = *bytes.get(pos)?;
    let next = bytes.get(pos + 1).copied();
    let len = bytes.len();
    let mk = |kind, end: Option<usize>, opener_len: usize| {
        Some(Construct {
            kind,
            start: pos,
            end: end.unwrap_or(len),
            terminated: end.is_some(),
            opener_len,
        })
    };
    match b {
        b'-' if next == Some(b'-') => {
            let is_comment = !rules.dash_needs_space
                || bytes.get(pos + 2).is_none_or(|&c| c <= 0x20 || c == 0x7f);
            if is_comment {
                return Some(line_comment(bytes, pos, rules));
            }
            None
        }
        b'#' => {
            let is_comment = match rules.hash {
                HashComment::Never => false,
                HashComment::Always => true,
                HashComment::SpaceOrBang => matches!(next, Some(b' ' | b'!')),
            };
            if is_comment {
                return Some(line_comment(bytes, pos, rules));
            }
            if rules.sqlite_vars {
                return sqlite_variable(bytes, pos);
            }
            None
        }
        b'/' if next == Some(b'*') => {
            match rules.exec_decision(&bytes[pos + 2..]) {
                ExecDecision::Run { marker_len } => mk(
                    ConstructKind::ExecOpen { marker_len },
                    Some(pos + marker_len),
                    marker_len,
                ),
                ExecDecision::Comment { nest_allowance } => {
                    let allowance = if rules.exec.is_some() && nest_allowance > 0 {
                        Some(nest_allowance)
                    } else if rules.nested_comments {
                        None
                    } else {
                        Some(0)
                    };
                    let end = block_comment_end(bytes, pos, allowance);
                    if end.is_none() && rules.open_block_comment_ok {
                        // SQLite accepts it; the caller still treats the tail
                        // as code (it is never hidden).
                        return mk(ConstructKind::BlockComment, None, 2);
                    }
                    mk(ConstructKind::BlockComment, end, 2)
                }
            }
        }
        b'\'' => mk(
            ConstructKind::Str { prefix_len: 0 },
            quoted_end(bytes, pos + 1, b'\'', rules.single_backslash, true),
            1,
        ),
        b'"' => match rules.double {
            DoubleQuote::Str { backslash } => mk(
                ConstructKind::Str { prefix_len: 0 },
                quoted_end(bytes, pos + 1, b'"', backslash, true),
                1,
            ),
            DoubleQuote::Ident {
                backslash,
                doubling,
            } => mk(
                ConstructKind::QuotedIdent { open: b'"' },
                quoted_end(bytes, pos + 1, b'"', backslash, doubling),
                1,
            ),
        },
        b'`' => {
            let backslash = rules.backtick?;
            mk(
                ConstructKind::QuotedIdent { open: b'`' },
                quoted_end(bytes, pos + 1, b'`', backslash, true),
                1,
            )
        }
        b'[' => match rules.bracket {
            Bracket::None => None,
            Bracket::Doubled => mk(
                ConstructKind::QuotedIdent { open: b'[' },
                quoted_end(bytes, pos + 1, b']', false, true),
                1,
            ),
            Bracket::NoEscape => mk(
                ConstructKind::QuotedIdent { open: b'[' },
                bytes[pos + 1..]
                    .iter()
                    .position(|&c| c == b']')
                    .map(|i| pos + 1 + i + 1),
                1,
            ),
        },
        b'$' => {
            if rules.sqlite_vars {
                return sqlite_variable(bytes, pos);
            }
            match rules.dollar {
                Dollar::None => None,
                Dollar::Tagged => pg_dollar_quote(bytes, pos),
                Dollar::Heredoc => clickhouse_heredoc(bytes, pos),
            }
        }
        b'@' | b':' if rules.sqlite_vars => sqlite_variable(bytes, pos),
        b'e' | b'E' if rules.e_strings && next == Some(b'\'') => mk(
            ConstructKind::Str { prefix_len: 1 },
            quoted_end(bytes, pos + 2, b'\'', true, true),
            2,
        ),
        b'b' | b'B' if rules.b_escapes && next == Some(b'\'') => mk(
            ConstructKind::Str { prefix_len: 1 },
            quoted_end(bytes, pos + 2, b'\'', true, true),
            2,
        ),
        b'q' | b'Q' if rules.oracle_q && next == Some(b'\'') => oracle_q(bytes, pos, 1),
        b'n' | b'N'
            if rules.oracle_q
                && matches!(next, Some(b'q' | b'Q'))
                && bytes.get(pos + 2) == Some(&b'\'') =>
        {
            oracle_q(bytes, pos, 2)
        }
        _ => None,
    }
}

fn line_comment(bytes: &[u8], pos: usize, rules: &Rules) -> Construct {
    let end = bytes[pos..]
        .iter()
        .position(|&c| c == b'\n' || (rules.line_ends_at_cr && c == b'\r'))
        .map(|i| pos + i)
        .unwrap_or(bytes.len());
    Construct {
        kind: ConstructKind::LineComment,
        start: pos,
        end,
        terminated: true,
        opener_len: 2,
    }
}

/// End of a `/* … */` comment starting at `pos`. `allowance`: how many
/// nested levels may open inside (`None` = unbounded, standard SQL).
fn block_comment_end(bytes: &[u8], pos: usize, allowance: Option<u32>) -> Option<usize> {
    let mut depth: u32 = 1;
    let mut i = pos + 2;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Some(i);
            }
            continue;
        }
        if bytes[i] == b'/' && bytes[i + 1] == b'*' && allowance.is_none_or(|a| depth < 1 + a) {
            depth += 1;
            i += 2;
            continue;
        }
        i += 1;
    }
    None
}

/// End (exclusive, past the closing quote) of a quoted run whose body starts
/// at `body`. `doubling`: a doubled quote stands for one quote inside.
fn quoted_end(
    bytes: &[u8],
    body: usize,
    quote: u8,
    backslash: bool,
    doubling: bool,
) -> Option<usize> {
    let mut i = body;
    while i < bytes.len() {
        let c = bytes[i];
        if backslash && c == b'\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if doubling && bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn is_dollar_tag_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_dollar_tag_continue(b: u8) -> bool {
    is_dollar_tag_start(b) || b.is_ascii_digit()
}

/// PostgreSQL `$tag$…$tag$`. `None` when the `$` does not open one (a `$1`
/// parameter, a stray `$`).
fn pg_dollar_quote(bytes: &[u8], pos: usize) -> Option<Construct> {
    let mut i = pos + 1;
    if let Some(&first) = bytes.get(i)
        && first != b'$'
    {
        if !is_dollar_tag_start(first) {
            return None;
        }
        while i < bytes.len() && is_dollar_tag_continue(bytes[i]) {
            i += 1;
        }
    }
    if bytes.get(i) != Some(&b'$') {
        return None;
    }
    let tag = &bytes[pos..=i];
    let body = i + 1;
    let end = find(&bytes[body..], tag).map(|at| body + at + tag.len());
    Some(Construct {
        kind: ConstructKind::DollarQuoted,
        start: pos,
        end: end.unwrap_or(bytes.len()),
        terminated: end.is_some(),
        opener_len: tag.len(),
    })
}

/// ClickHouse heredoc: `$` + an ASCII word-character tag (possibly empty,
/// digits allowed first) + `$`, and the construct exists only when the same
/// `$tag$` appears again; otherwise nothing is quoted (the `$` starts an
/// identifier or stands alone).
fn clickhouse_heredoc(bytes: &[u8], pos: usize) -> Option<Construct> {
    let tag_len = bytes[pos + 1..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
        .count();
    let close = pos + 1 + tag_len;
    if bytes.get(close) != Some(&b'$') {
        return None;
    }
    let tag = &bytes[pos..=close];
    let body = close + 1;
    let at = find(&bytes[body..], tag)?;
    Some(Construct {
        kind: ConstructKind::DollarQuoted,
        start: pos,
        end: body + at + tag.len(),
        terminated: true,
        opener_len: tag.len(),
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn is_sqlite_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// SQLite's variable token (`sqlite3GetToken`, CC_DOLLAR / CC_VARALPHA):
/// `$`, `@`, `:` or `#` followed by identifier characters, optionally a TCL
/// array suffix `(…)` running to the next `)` or whitespace — which may hold
/// quote characters — and `::` namespace separators.
fn sqlite_variable(bytes: &[u8], pos: usize) -> Option<Construct> {
    let mut i = pos + 1;
    let mut n = 0usize;
    while let Some(&c) = bytes.get(i) {
        if is_sqlite_id_char(c) {
            n += 1;
            i += 1;
        } else if c == b'(' && n > 0 {
            i += 1;
            while let Some(&c) = bytes.get(i) {
                if c == 0 || c.is_ascii_whitespace() || c == b')' {
                    break;
                }
                i += 1;
            }
            if bytes.get(i) == Some(&b')') {
                i += 1;
            }
            break;
        } else if c == b':' && bytes.get(i + 1) == Some(&b':') {
            i += 2;
        } else {
            break;
        }
    }
    if n == 0 {
        return None;
    }
    Some(Construct {
        kind: ConstructKind::Variable,
        start: pos,
        end: i,
        terminated: true,
        opener_len: i - pos,
    })
}

/// Oracle `q'X…X'` (`prefix_len` 1) or `nq'X…X'` (2).
fn oracle_q(bytes: &[u8], pos: usize, prefix_len: usize) -> Option<Construct> {
    let delim_at = pos + prefix_len + 1;
    let delim = *bytes.get(delim_at)?;
    if delim.is_ascii_whitespace() || delim >= 0x80 {
        return None;
    }
    let close = match delim {
        b'[' => b']',
        b'{' => b'}',
        b'(' => b')',
        b'<' => b'>',
        other => other,
    };
    let mut i = delim_at + 1;
    let mut end = None;
    while i + 1 < bytes.len() {
        if bytes[i] == close && bytes[i + 1] == b'\'' {
            end = Some(i + 2);
            break;
        }
        i += 1;
    }
    Some(Construct {
        kind: ConstructKind::Str { prefix_len },
        start: pos,
        end: end.unwrap_or(bytes.len()),
        terminated: end.is_some(),
        opener_len: prefix_len + 1,
    })
}

/// Scan `src` under one reading and write its shadow.
pub(crate) fn scan(src: &str, rules: &Rules) -> Scan {
    let bytes = src.as_bytes();
    let mut shadow: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut unterminated: Option<Unterminated> = None;
    let mut in_exec = false;
    let mut exec_open_at = 0usize;
    let mut pos = 0usize;
    let note = |slot: &mut Option<Unterminated>, what: Unclosed, offset: usize| {
        if slot.is_none() {
            *slot = Some(Unterminated { what, offset });
        }
    };

    while pos < bytes.len() {
        let b = bytes[pos];
        if in_exec && b == b'*' && bytes.get(pos + 1) == Some(&b'/') {
            shadow.extend_from_slice(b"  ");
            pos += 2;
            in_exec = false;
            continue;
        }
        if b.is_ascii_whitespace() {
            shadow.push(b);
            pos += 1;
            continue;
        }
        if let Some(c) = construct_at(bytes, pos, rules) {
            if let ConstructKind::ExecOpen { marker_len } = c.kind {
                blank(&mut shadow, &bytes[pos..pos + marker_len]);
                pos += marker_len;
                if !in_exec {
                    exec_open_at = c.start;
                }
                in_exec = true;
                continue;
            }
            if !c.terminated {
                let what = match c.kind {
                    ConstructKind::BlockComment => Unclosed::BlockComment,
                    ConstructKind::DollarQuoted => Unclosed::DollarQuote,
                    ConstructKind::QuotedIdent { .. } => Unclosed::QuotedIdentifier,
                    _ => Unclosed::String,
                };
                note(&mut unterminated, what, c.start);
                blank(&mut shadow, &bytes[pos..pos + c.opener_len]);
                pos += c.opener_len;
                continue;
            }
            let text = &bytes[c.start..c.end];
            match c.kind {
                ConstructKind::LineComment | ConstructKind::BlockComment => {
                    blank(&mut shadow, text)
                }
                ConstructKind::Str { .. } | ConstructKind::DollarQuoted => {
                    shadow.push(b'\'');
                    blank(&mut shadow, &text[1..text.len() - 1]);
                    shadow.push(b'\'');
                }
                ConstructKind::QuotedIdent { .. } => {
                    shadow.push(b'[');
                    for &c in &text[1..text.len() - 1] {
                        shadow.push(if matches!(c, b'[' | b']') { b'_' } else { c });
                    }
                    shadow.push(b']');
                }
                ConstructKind::Variable => {
                    // One opaque identifier: the TCL suffix may hold quotes.
                    for &c in text {
                        shadow.push(if c.is_ascii() { b'_' } else { c });
                    }
                }
                ConstructKind::ExecOpen { .. } => unreachable!("handled above"),
            }
            pos = c.end;
            continue;
        }
        if rules.is_word_start(b) {
            let start = pos;
            pos += 1;
            while pos < bytes.len() && rules.is_word_continue(bytes[pos]) {
                pos += 1;
            }
            shadow.extend_from_slice(&bytes[start..pos]);
            continue;
        }
        if b.is_ascii_digit() {
            let start = pos;
            pos += 1;
            while pos < bytes.len()
                && (bytes[pos].is_ascii_alphanumeric() || matches!(bytes[pos], b'_' | b'.'))
            {
                pos += 1;
            }
            shadow.extend_from_slice(&bytes[start..pos]);
            continue;
        }
        match b {
            b'[' | b']' => shadow.push(b' '),
            b'-' if shadow.last() == Some(&b'-') => shadow.push(b' '),
            _ => shadow.push(b),
        }
        pos += 1;
    }
    if in_exec {
        note(&mut unterminated, Unclosed::ExecutableComment, exec_open_at);
    }
    debug_assert_eq!(shadow.len(), bytes.len());
    let shadow = String::from_utf8(shadow).expect("shadow keeps UTF-8 boundaries");
    Scan {
        shadow,
        unterminated,
    }
}

/// Blank a run: ASCII bytes become spaces except line breaks, which are kept
/// so line structure (GO lines, diagnostics) survives; a multibyte character
/// becomes one space per byte.
fn blank(out: &mut Vec<u8>, run: &[u8]) {
    out.extend(
        run.iter()
            .map(|&c| if c == b'\n' || c == b'\r' { c } else { b' ' }),
    );
}
