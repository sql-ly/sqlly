//! Lightweight SQL scanner — produces a token stream that skips string
//! literals, line/block comments, and tracks parenthesis depth so the
//! clause-context and table-ref extractors can filter to the cursor's
//! lexical scope (subquery nesting, CTE definitions).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanTokenKind {
    Word,
    LeftParen,
    RightParen,
    Comma,
    Dot,
    Semicolon,
    Other,
}

/// Token text borrows from the source SQL when the scanned span is contiguous
/// in the buffer. Stripped bracket/quoted identifiers own their text because
/// delimiters are removed.
#[derive(Debug, Clone)]
pub enum ScanText<'a> {
    Borrowed(&'a str),
    Owned(String),
}

impl<'a> ScanText<'a> {
    pub fn as_str(&self) -> &str {
        match self {
            ScanText::Borrowed(text) => text,
            ScanText::Owned(text) => text,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanToken<'a> {
    pub kind: ScanTokenKind,
    pub text: ScanText<'a>,
    pub byte_offset: usize,
    /// Byte offset one past the end of the token's source span. For
    /// bracketed/quoted identifiers this includes the delimiters, so
    /// `byte_offset..end_byte` always covers the raw source text even
    /// though `text` has delimiters stripped.
    pub end_byte: usize,
    pub paren_depth: usize,
}

/// Scans the full SQL string into a token vector. String literals
/// (`'…'` with `''` escaping), line comments (`-- …`) and block comments
/// (`/* … */`) are skipped entirely. Bracketed identifiers (`[…]`) and
/// double-quoted identifiers (`"…"`, T-SQL `QUOTED_IDENTIFIER`) are
/// emitted as `Word` tokens with delimiters stripped. Each token carries
/// the parenthesis depth at its position so callers can filter to a
/// specific nesting level.
pub fn scan_tokens(sql: &str) -> Vec<ScanToken<'_>> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut depth: usize = 0;

    while i < bytes.len() {
        let ch = bytes[i];

        if ch.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Line comment: -- … \n
        if ch == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment: /* … */
        if ch == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            let mut closed = false;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                i = bytes.len();
            }
            continue;
        }

        // String literal: '…' with '' escaping
        if ch == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }

        // Bracketed identifier: […]
        if ch == b'[' {
            let start = i;
            i += 1;
            // `]]` inside brackets is an escaped `]`.
            while i < bytes.len() && (bytes[i] != b']' || bytes.get(i + 1) == Some(&b']')) {
                i += if bytes[i] == b']' { 2 } else { 1 };
            }
            let text = if i < bytes.len() {
                let inner = &sql[start + 1..i];
                i += 1;
                ScanText::Owned(inner.replace("]]", "]"))
            } else {
                let inner = &sql[start + 1..];
                i = bytes.len();
                ScanText::Owned(inner.to_string())
            };
            tokens.push(ScanToken {
                kind: ScanTokenKind::Word,
                text,
                byte_offset: start,
                end_byte: i,
                paren_depth: depth,
            });
            continue;
        }

        // Double-quoted identifier: "…" with "" escaping (T-SQL
        // QUOTED_IDENTIFIER ON), or a MySQL-family backticked one. Emitted
        // as a Word with quotes stripped, mirroring the bracketed-identifier
        // handling above.
        if ch == b'"' || ch == b'`' {
            let quote = ch;
            let start = i;
            i += 1;
            let inner_start = i;
            while i < bytes.len() {
                if bytes[i] == quote {
                    if i + 1 < bytes.len() && bytes[i + 1] == quote {
                        i += 2;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            let inner_end = i.min(bytes.len());
            if i < bytes.len() {
                i += 1; // consume closing quote
            }
            let doubled = if quote == b'"' { "\"\"" } else { "``" };
            let single = if quote == b'"' { "\"" } else { "`" };
            let text = ScanText::Owned(sql[inner_start..inner_end].replace(doubled, single));
            tokens.push(ScanToken {
                kind: ScanTokenKind::Word,
                text,
                byte_offset: start,
                end_byte: i,
                paren_depth: depth,
            });
            continue;
        }

        match ch {
            b'(' => {
                tokens.push(ScanToken {
                    kind: ScanTokenKind::LeftParen,
                    text: ScanText::Borrowed("("),
                    byte_offset: i,
                    end_byte: i + 1,
                    paren_depth: depth,
                });
                depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                tokens.push(ScanToken {
                    kind: ScanTokenKind::RightParen,
                    text: ScanText::Borrowed(")"),
                    byte_offset: i,
                    end_byte: i + 1,
                    paren_depth: depth,
                });
                i += 1;
                continue;
            }
            b',' => {
                tokens.push(ScanToken {
                    kind: ScanTokenKind::Comma,
                    text: ScanText::Borrowed(","),
                    byte_offset: i,
                    end_byte: i + 1,
                    paren_depth: depth,
                });
                i += 1;
                continue;
            }
            b'.' => {
                tokens.push(ScanToken {
                    kind: ScanTokenKind::Dot,
                    text: ScanText::Borrowed("."),
                    byte_offset: i,
                    end_byte: i + 1,
                    paren_depth: depth,
                });
                i += 1;
                continue;
            }
            b';' => {
                tokens.push(ScanToken {
                    kind: ScanTokenKind::Semicolon,
                    text: ScanText::Borrowed(";"),
                    byte_offset: i,
                    end_byte: i + 1,
                    paren_depth: depth,
                });
                i += 1;
                continue;
            }
            _ => {}
        }

        // Word: alphanumeric, underscore, @, and a leading `#` — a SQL
        // Server temp-table name (`#temp`, `##shared`) is one identifier.
        if ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'@' || ch == b'#' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'#' {
                i += 1;
            }
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'@' {
                    i += 1;
                } else {
                    break;
                }
            }
            tokens.push(ScanToken {
                kind: ScanTokenKind::Word,
                text: ScanText::Borrowed(&sql[start..i]),
                byte_offset: start,
                end_byte: i,
                paren_depth: depth,
            });
            continue;
        }

        // Any other character — including a multi-byte UTF-8 scalar sitting
        // in code position (e.g. an accented identifier). Advance by the
        // char's full width and record boundary-accurate offsets so a
        // downstream `sql[byte_offset..end_byte]` never slices mid-char.
        let other = sql[i..].chars().next();
        let width = other.map_or(1, char::len_utf8);
        tokens.push(ScanToken {
            kind: ScanTokenKind::Other,
            text: ScanText::Owned(other.map(String::from).unwrap_or_default()),
            byte_offset: i,
            end_byte: i + width,
            paren_depth: depth,
        });
        i += width;
    }

    tokens
}

/// Returns the parenthesis depth at `cursor` by counting unmatched
/// left parens in the scanned tokens before the cursor. String-literal
/// and comment parens are already skipped by the scanner, so the count
/// reflects only structural SQL parentheses.
pub fn cursor_depth(tokens: &[ScanToken<'_>], cursor: usize) -> usize {
    let mut depth = 0usize;
    for tok in tokens {
        if tok.byte_offset >= cursor {
            break;
        }
        match tok.kind {
            ScanTokenKind::LeftParen => depth += 1,
            ScanTokenKind::RightParen if depth > 0 => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    depth
}
