//! What the page shows next to the formatted SQL: whether anything changed
//! and why not when nothing did, what kind of change was made, and the
//! output's tokens for syntax highlighting. All of it comes from the same
//! lexer the formatter reads SQL with, so the colors, the line/column of a
//! problem and the "what changed" counts agree with the formatter exactly.

use serde_json::{Value, json};
use sqlly_lang::sqlformat::{FormatDecline, FormatReport};
use sqlly_lang::sqllex::{IdentQuote, LexErrorKind, StrictLexer, Token, TokenKind};
use sqlly_lang::sqlmodel::Dialect;

/// Token classes for highlighting. The page maps each to a CSS class; plain
/// identifiers, punctuation and whitespace carry no span at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Class {
    Keyword = 1,
    String = 2,
    Number = 3,
    Comment = 4,
    Function = 5,
    Param = 6,
    Operator = 7,
    QuotedName = 8,
}

/// The page's id for a lexer dialect (the formatter setting that selects it).
pub fn dialect_id(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::SqlServer => "tsql",
        Dialect::Postgres => "postgresql",
        Dialect::MySql => "mysql",
        Dialect::Oracle => "oracle",
        Dialect::Sqlite => "sqlite",
    }
}

fn lex_error_code(kind: LexErrorKind) -> &'static str {
    match kind {
        LexErrorKind::UnterminatedString => "unterminatedString",
        LexErrorKind::UnterminatedQuotedIdent => "unterminatedQuotedName",
        LexErrorKind::UnterminatedBlockComment => "unterminatedBlockComment",
        LexErrorKind::IllegalCharacter => "illegalCharacter",
    }
}

/// 1-based line and column of byte `offset` in `text`, the column counted
/// in characters, plus the offset in UTF-16 code units (what a browser's
/// `setSelectionRange` takes).
pub fn position(text: &str, offset: usize) -> (usize, usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = text[line_start..offset].chars().count() + 1;
    let utf16 = before.encode_utf16().count();
    (line, column, utf16)
}

/// Highlight spans for `text` read as `dialect`: a flat
/// `[class, start, end, class, start, end, …]` list, offsets in UTF-16
/// code units so the page can slice its JS string directly.
pub fn highlight(text: &str, dialect: Dialect) -> Vec<u32> {
    let tokens = StrictLexer::tokenize(text, dialect);
    let mut out = Vec::new();
    // Byte offsets climb monotonically, so the UTF-16 conversion is one pass.
    let mut byte = 0usize;
    let mut unit = 0u32;
    let mut to_utf16 = |target: usize| -> u32 {
        if target > byte {
            unit += text[byte..target].encode_utf16().count() as u32;
            byte = target;
        }
        unit
    };
    for (i, t) in tokens.iter().enumerate() {
        let Some(class) = class_of(&tokens, i) else {
            continue;
        };
        let start = to_utf16(t.span.start);
        let end = to_utf16(t.span.end);
        out.extend([class as u32, start, end]);
    }
    out
}

fn class_of(tokens: &[Token<'_>], i: usize) -> Option<Class> {
    let t = &tokens[i];
    // A word written directly against `(` is a call (`COUNT(`, `coalesce(`);
    // the formatter puts a space before the parenthesis of `IN (`,
    // `VALUES (` and friends, so they keep the keyword color.
    let called = matches!(tokens.get(i + 1), Some(n) if n.kind == TokenKind::LParen);
    let after_dot = i > 0 && tokens[i - 1].kind == TokenKind::Dot;
    Some(match t.kind {
        TokenKind::Keyword if called => Class::Function,
        TokenKind::Ident(IdentQuote::None) if called => Class::Function,
        // `o.type`: a keyword spelled behind a qualifier is a name.
        TokenKind::Keyword if after_dot => return None,
        TokenKind::Keyword => Class::Keyword,
        TokenKind::Ident(IdentQuote::None) => return None,
        TokenKind::Ident(_) => Class::QuotedName,
        TokenKind::StringLit(_) => Class::String,
        TokenKind::Number(_) => Class::Number,
        TokenKind::Comment(_) => Class::Comment,
        TokenKind::ParamMarker(_) => Class::Param,
        TokenKind::Operator => Class::Operator,
        _ => return None,
    })
}

/// Counts of what changed between `input` and `output` beyond whitespace,
/// pairing their tokens the way the formatter's safety check does. `None`
/// when the streams do not pair up (they always should for formatter output).
pub fn changes(input: &str, output: &str, dialect: Dialect) -> Option<Value> {
    let significant = |text| {
        StrictLexer::tokenize(text, dialect)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .collect::<Vec<_>>()
    };
    let a = significant(input);
    let b = significant(output);
    let (mut recased, mut names_quoted, mut parens, mut comments) = (0u32, 0u32, 0u32, 0u32);
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() || j < b.len() {
        let (Some(x), Some(y)) = (a.get(i), b.get(j)) else {
            // Only parentheses the formatter added may be left over.
            match b.get(j) {
                Some(y) if matches!(y.kind, TokenKind::LParen | TokenKind::RParen) => {
                    parens += 1;
                    j += 1;
                    continue;
                }
                _ => return None,
            }
        };
        let words = |t: &Token<'_>| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident(_));
        if x.text == y.text {
            // Identical text; nothing to count.
        } else if x.kind == TokenKind::Keyword && y.kind == TokenKind::Keyword {
            if !x.text.eq_ignore_ascii_case(y.text) {
                return None;
            }
            recased += 1;
        } else if words(x) && words(y) {
            let same_quoting = x.ident_quoted() == y.ident_quoted()
                && (!x.ident_quoted() || x.text.as_bytes()[0] == y.text.as_bytes()[0]);
            if !x.ident_text().eq_ignore_ascii_case(&y.ident_text()) {
                return None;
            }
            if !same_quoting {
                names_quoted += 1;
            } else {
                recased += 1;
            }
        } else if matches!(x.kind, TokenKind::Comment(_)) && matches!(y.kind, TokenKind::Comment(_))
        {
            if x.text.trim_end() != y.text.trim_end() {
                comments += 1;
            }
        } else if matches!(y.kind, TokenKind::LParen | TokenKind::RParen) {
            parens += 1;
            j += 1;
            continue;
        } else {
            return None;
        }
        i += 1;
        j += 1;
    }
    Some(json!({
        "wordsRecased": recased,
        "namesRequoted": names_quoted,
        "parenthesesAdded": parens,
        "commentsReflowed": comments,
    }))
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count().max(1)
    }
}

fn decline_json(input: &str, declined: FormatDecline) -> Value {
    match declined {
        FormatDecline::LexError { kind, offset } => {
            let (line, column, utf16) = position(input, offset);
            let ch = input[offset.min(input.len())..].chars().next();
            let mut v = json!({
                "code": "lexError",
                "kind": lex_error_code(kind),
                "line": line,
                "column": column,
                "offset": utf16,
            });
            if kind == LexErrorKind::IllegalCharacter
                && let Some(ch) = ch
            {
                v["character"] = Value::String(ch.to_string());
            }
            v
        }
        FormatDecline::Empty => json!({ "code": "empty" }),
        FormatDecline::SafetyCheck => json!({ "code": "safetyCheck" }),
    }
}

/// The page's view of a format run:
/// `{ output, status: "formatted"|"unchanged"|"declined", reason?, dialect,
///    lines: [before, after], changes?, tokens }`.
pub fn format_report_json(input: &str, report: &FormatReport) -> Value {
    let status = match (report.declined, report.output == input) {
        (Some(_), _) => "declined",
        (None, true) => "unchanged",
        (None, false) => "formatted",
    };
    let mut v = json!({
        "output": report.output,
        "status": status,
        "dialect": dialect_id(report.dialect),
        "lines": [line_count(input), line_count(&report.output)],
        "tokens": highlight(&report.output, report.dialect),
    });
    if let Some(declined) = report.declined {
        v["reason"] = decline_json(input, declined);
    }
    if status == "formatted"
        && let Some(changes) = changes(input, &report.output, report.dialect)
    {
        v["changes"] = changes;
    }
    v
}

/// The page's view of a minify run, same shape as [`format_report_json`]
/// minus `changes` (minifying drops ordinary comments by design).
pub fn minify_report_json(input: &str, output: &str, dialect: Dialect) -> Value {
    let lex_error = StrictLexer::tokenize(input, dialect)
        .iter()
        .find_map(|t| match t.kind {
            TokenKind::Error(kind) => Some(FormatDecline::LexError {
                kind,
                offset: t.span.start,
            }),
            _ => None,
        });
    let declined = lex_error.or_else(|| input.trim().is_empty().then_some(FormatDecline::Empty));
    let status = match (declined, output == input) {
        (Some(_), _) => "declined",
        (None, true) => "unchanged",
        (None, false) => "formatted",
    };
    let mut v = json!({
        "output": output,
        "status": status,
        "dialect": dialect_id(dialect),
        "lines": [line_count(input), line_count(output)],
        "tokens": highlight(output, dialect),
    });
    if let Some(declined) = declined {
        v["reason"] = decline_json(input, declined);
    }
    v
}
