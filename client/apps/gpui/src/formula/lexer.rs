//! Formula tokens, with source spans.

use super::{CellRef, ErrorKind, ParseError, RowRef};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Number(f64),
    Text(String),
    Bool(bool),
    Error(ErrorKind),
    Ref(CellRef),
    Var(String),
    Ident(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
    Colon,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

fn err(message: impl Into<String>, at: usize) -> ParseError {
    ParseError {
        message: message.into(),
        at,
    }
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A `"…"` string starting at `at` (the opening quote), `""` escaping a
/// quote. Returns the text and the index after the closing quote.
fn quoted(src: &str, at: usize) -> Result<(String, usize), ParseError> {
    let mut out = String::new();
    let mut chars = src[at + 1..].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            if chars.peek().is_some_and(|(_, n)| *n == '"') {
                out.push('"');
                chars.next();
                continue;
            }
            return Ok((out, at + 1 + i + 1));
        }
        out.push(c);
    }
    Err(err(
        crate::localization::text("results.formula_lexer.unclosed_quote"),
        at,
    ))
}

/// A name after `!` or `@`: quoted, or a run of letters, digits, `_`.
fn name_at(src: &str, at: usize, what: &str) -> Result<(String, usize), ParseError> {
    if src[at..].starts_with('"') {
        return quoted(src, at);
    }
    let len: usize = src[at..]
        .chars()
        .take_while(|c| is_name_char(*c))
        .map(char::len_utf8)
        .sum();
    if len == 0 {
        return Err(err(
            crate::localization::format("results.formula_lexer.missing_name", &[("marker", what)]),
            at,
        ));
    }
    Ok((src[at..at + len].to_string(), at + len))
}

pub fn lex(src: &str) -> Result<Vec<Token>, ParseError> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < src.len() {
        let c = src[i..].chars().next().expect("in bounds");
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        let tok = match c {
            '(' => {
                i += 1;
                Tok::LParen
            }
            ')' => {
                i += 1;
                Tok::RParen
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            ':' => {
                i += 1;
                Tok::Colon
            }
            '"' => {
                let (text, next) = quoted(src, i)?;
                i = next;
                Tok::Text(text)
            }
            '!' => {
                let (column, mut next) = name_at(src, i + 1, "!")?;
                let mut row = RowRef::Relative(0);
                if bytes.get(next) == Some(&b'[') {
                    let close = src[next..].find(']').map(|j| next + j).ok_or_else(|| {
                        err(
                            crate::localization::text(
                                "results.formula_lexer.unclosed_row_reference",
                            ),
                            next,
                        )
                    })?;
                    let inner = src[next + 1..close].trim();
                    row = if let Some(abs) = inner.strip_prefix('$') {
                        let n: i64 = abs.trim().parse().map_err(|_| {
                            err(
                                crate::localization::format(
                                    "results.formula_lexer.invalid_row_number",
                                    &[("reference", inner)],
                                ),
                                next,
                            )
                        })?;
                        if n < 1 {
                            return Err(err(
                                crate::localization::text(
                                    "results.formula_lexer.row_numbers_start_at_one",
                                ),
                                next,
                            ));
                        }
                        RowRef::Absolute(n)
                    } else {
                        RowRef::Relative(inner.parse().map_err(|_| {
                            err(
                                crate::localization::format(
                                    "results.formula_lexer.invalid_row_offset",
                                    &[("reference", inner)],
                                ),
                                next,
                            )
                        })?)
                    };
                    next = close + 1;
                }
                i = next;
                Tok::Ref(CellRef { column, row })
            }
            '@' => {
                let (name, next) = name_at(src, i + 1, "@")?;
                i = next;
                Tok::Var(name)
            }
            '#' => {
                let rest = &src[i..];
                let lit = [
                    "#NULL!", "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A",
                ]
                .into_iter()
                .find(|lit| rest.len() >= lit.len() && rest[..lit.len()].eq_ignore_ascii_case(lit))
                .ok_or_else(|| {
                    err(
                        crate::localization::text("results.formula_lexer.unknown_error_value"),
                        i,
                    )
                })?;
                i += lit.len();
                Tok::Error(ErrorKind::from_literal(lit).expect("listed"))
            }
            '<' | '>' => {
                let two = src.get(i..i + 2).unwrap_or("");
                let op = match two {
                    "<=" => "<=",
                    ">=" => ">=",
                    "<>" => "<>",
                    _ if c == '<' => "<",
                    _ => ">",
                };
                i += op.len();
                Tok::Op(op)
            }
            '=' | '+' | '-' | '*' | '/' | '^' | '&' | '%' => {
                i += 1;
                Tok::Op(match c {
                    '=' => "=",
                    '+' => "+",
                    '-' => "-",
                    '*' => "*",
                    '/' => "/",
                    '^' => "^",
                    '&' => "&",
                    _ => "%",
                })
            }
            c if c.is_ascii_digit()
                || (c == '.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                let mut j = i;
                while j < src.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                    j += 1;
                }
                if j < src.len() && (bytes[j] == b'e' || bytes[j] == b'E') {
                    let mut k = j + 1;
                    if k < src.len() && (bytes[k] == b'+' || bytes[k] == b'-') {
                        k += 1;
                    }
                    if k < src.len() && bytes[k].is_ascii_digit() {
                        while k < src.len() && bytes[k].is_ascii_digit() {
                            k += 1;
                        }
                        j = k;
                    }
                }
                let n: f64 = src[i..j].parse().map_err(|_| {
                    err(
                        crate::localization::format(
                            "results.formula_lexer.invalid_number",
                            &[("value", &src[i..j])],
                        ),
                        i,
                    )
                })?;
                i = j;
                Tok::Number(n)
            }
            c if c.is_alphabetic() || c == '_' => {
                let len: usize = src[i..]
                    .chars()
                    .take_while(|c| is_name_char(*c) || *c == '.')
                    .map(char::len_utf8)
                    .sum();
                let word = &src[i..i + len];
                i += len;
                let next_is_paren = src[i..].trim_start().starts_with('(');
                if !next_is_paren && word.eq_ignore_ascii_case("TRUE") {
                    Tok::Bool(true)
                } else if !next_is_paren && word.eq_ignore_ascii_case("FALSE") {
                    Tok::Bool(false)
                } else {
                    Tok::Ident(word.to_ascii_uppercase())
                }
            }
            other => {
                return Err(err(
                    crate::localization::format(
                        "results.formula_lexer.unexpected_character",
                        &[("character", &other.to_string())],
                    ),
                    i,
                ));
            }
        };
        out.push(Token { tok, start, end: i });
    }
    Ok(out)
}
