//! Token- and text-capture helpers shared by the parser's statement modules,
//! plus the pure classification functions they lean on.

use crate::sqllex::{NumberShape, StringShape, Token, TokenKind};
use crate::sqlmodel::{Span, SqlName};

use super::StatementParser;
use crate::sqlparse::slots::{LiteralShape, SlotValue};
use crate::sqlparse::tree::GrabBagToken;

impl<'c, 'a> StatementParser<'c, 'a> {
    pub(crate) fn consume_rest(&mut self) -> usize {
        let mut end = self.cur.prev_end();
        while let Some(t) = self.cur.advance() {
            end = t.span.end;
        }
        end
    }

    pub(crate) fn skip_balanced_parens(&mut self) {
        if self.cur.eat_kind(TokenKind::LParen).is_none() {
            return;
        }
        let mut depth = 1usize;
        while depth > 0 {
            match self.cur.advance() {
                Some(t) => match t.kind {
                    TokenKind::LParen => depth += 1,
                    TokenKind::RParen => depth -= 1,
                    _ => {}
                },
                None => break,
            }
        }
    }

    /// Parse "(a, b, c)" of identifiers.
    pub(crate) fn parse_paren_name_list(&mut self) -> Option<Vec<SqlName>> {
        if self
            .cur
            .peek()
            .map(|t| t.kind != TokenKind::LParen)
            .unwrap_or(true)
        {
            return None;
        }
        self.cur.advance();
        let mut out = Vec::new();
        while let Some(t) = self.cur.peek().copied() {
            match t.kind {
                TokenKind::RParen => {
                    self.cur.advance();
                    break;
                }
                TokenKind::Comma => {
                    self.cur.advance();
                }
                _ if t.is_word() => {
                    out.push(SqlName::new(t.ident_text(), self.dialect, t.ident_quoted()));
                    self.cur.advance();
                }
                _ => {
                    self.cur.advance();
                }
            }
        }
        Some(out)
    }

    /// Capture a default/generated expression as raw text (single token,
    /// balanced parens, or function call).
    pub(crate) fn capture_expression_text(&mut self) -> String {
        let mut out = String::new();
        let t = match self.cur.peek() {
            Some(t) => *t,
            None => return out,
        };
        if t.kind == TokenKind::LParen {
            return self.capture_paren_text();
        }
        out.push_str(t.text);
        self.cur.advance();
        // Function call form: GETDATE()
        if self
            .cur
            .peek()
            .map(|t| t.kind == TokenKind::LParen)
            .unwrap_or(false)
        {
            out.push_str(&self.capture_paren_text());
        }
        out
    }

    pub(crate) fn capture_paren_text(&mut self) -> String {
        let mut out = String::new();
        if self
            .cur
            .peek()
            .map(|t| t.kind != TokenKind::LParen)
            .unwrap_or(true)
        {
            return out;
        }
        self.cur.advance();
        out.push('(');
        let mut depth = 1usize;
        while depth > 0 {
            match self.cur.advance() {
                Some(t) => {
                    match t.kind {
                        TokenKind::LParen => depth += 1,
                        TokenKind::RParen => depth -= 1,
                        _ => {}
                    }
                    out.push_str(t.text);
                    if depth > 0 {
                        out.push(' ');
                    }
                }
                None => break,
            }
        }
        out
    }

    /// Capture the value of an EXEC/CALL argument as a SlotValue.
    pub(crate) fn capture_slot_value(&mut self) -> SlotValue {
        let t = match self.cur.peek() {
            Some(t) => *t,
            None => return SlotValue::empty(),
        };
        let span = self.span(t.span);
        let value = match t.kind {
            TokenKind::StringLit(shape) => SlotValue {
                shape: LiteralShape::String {
                    national: shape == StringShape::National,
                },
                text: t.text.to_string(),
                span,
            },
            TokenKind::Number(shape) => SlotValue {
                shape: number_shape_to_literal(shape),
                text: t.text.to_string(),
                span,
            },
            TokenKind::ParamMarker(_) => SlotValue {
                shape: LiteralShape::Parameter,
                text: t.text.to_string(),
                span,
            },
            TokenKind::Keyword if t.keyword_eq("null") => SlotValue {
                shape: LiteralShape::Null,
                text: t.text.to_string(),
                span,
            },
            TokenKind::Operator if t.text == "-" || t.text == "+" => {
                // Signed number: merge sign into the literal.
                self.cur.advance();
                if let Some(n) = self.cur.peek().copied()
                    && let TokenKind::Number(shape) = n.kind
                {
                    let merged = Span::new(span.start, self.span(n.span).end);
                    self.cur.advance();
                    return SlotValue {
                        shape: number_shape_to_literal(shape),
                        text: format!("{}{}", t.text, n.text),
                        span: merged,
                    };
                }
                return SlotValue {
                    shape: LiteralShape::Other,
                    text: t.text.to_string(),
                    span,
                };
            }
            _ if t.is_word() => SlotValue {
                shape: LiteralShape::Word,
                text: t.text.to_string(),
                span,
            },
            _ => SlotValue {
                shape: LiteralShape::Other,
                text: t.text.to_string(),
                span,
            },
        };
        self.cur.advance();
        value
    }
}

pub(crate) fn number_shape_to_literal(shape: NumberShape) -> LiteralShape {
    match shape {
        NumberShape::Integer => LiteralShape::Integer,
        NumberShape::Decimal => LiteralShape::Decimal,
        NumberShape::Scientific => LiteralShape::Scientific,
        NumberShape::Hex => LiteralShape::Hex,
    }
}

pub(crate) fn is_routine_body_start(t: &Token<'_>) -> bool {
    t.keyword_eq("as")
        || t.keyword_eq("begin")
        || t.keyword_eq("returns")
        || t.keyword_eq("return")
        || t.keyword_eq("is")
        || t.keyword_eq("language")
}

pub(crate) fn is_clause_keyword(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "where"
            | "set"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "full"
            | "cross"
            | "on"
            | "group"
            | "order"
            | "having"
            | "union"
            | "limit"
            | "offset"
            | "returning"
            | "values"
            | "as"
            | "from"
            | "output"
            | "using"
            | "with"
            | "for"
            | "when"
            | "then"
            | "only"
            | "lateral"
            | "tablesample"
            | "pivot"
            | "unpivot"
            | "use"
            | "force"
            | "ignore"
            | "window"
            | "except"
            | "intersect"
            | "fetch"
            | "option"
    )
}

/// Scalar functions whose argument list legally contains a bare `FROM`
/// keyword (`EXTRACT(YEAR FROM ts)`, `SUBSTRING(x FROM 2 FOR 3)`,
/// `TRIM(LEADING 'x' FROM s)`, `OVERLAY(x PLACING y FROM 2)`). Scanning into
/// their parens would misread the argument after FROM as a table source.
pub(crate) fn is_keyword_arg_function(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "extract" | "substring" | "trim" | "overlay"
    )
}

/// T-SQL legacy table hints: `FROM t (NOLOCK)` without WITH. A parenthesized
/// group after a source name is a table hint when it opens with one of these;
/// otherwise the name is a table-valued function call.
pub(crate) fn is_tsql_table_hint(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "nolock"
            | "readuncommitted"
            | "updlock"
            | "xlock"
            | "holdlock"
            | "serializable"
            | "readcommitted"
            | "readcommittedlock"
            | "repeatableread"
            | "rowlock"
            | "paglock"
            | "tablock"
            | "tablockx"
            | "readpast"
            | "snapshot"
            | "forceseek"
            | "forcescan"
            | "noexpand"
            | "index"
    )
}

/// Heuristic: does this word + following token look like "name type" (a new
/// column definition) or a constraint keyword? Used to stop modifier
/// consumption at a line break.
pub(crate) fn looks_like_column_start(t: &Token<'_>, next: Option<&Token<'_>>) -> bool {
    if !t.is_word() {
        return false;
    }
    let w = t.text.to_ascii_lowercase();
    if matches!(
        w.as_str(),
        "constraint" | "primary" | "foreign" | "unique" | "check"
    ) {
        return true;
    }
    match next {
        Some(n) => n.is_word() || n.kind == TokenKind::LParen,
        None => true,
    }
}

pub(crate) fn token_to_grab(t: &Token<'_>) -> GrabBagToken {
    GrabBagToken {
        text: t.text.to_string(),
        span: t.span,
        shape: format!("{:?}", t.kind),
    }
}

#[cfg(test)]
mod tests {
    use crate::sqllex::IdentQuote;

    use super::*;

    fn tok(text: &'static str, kind: TokenKind) -> Token<'static> {
        Token {
            kind,
            span: Span::new(0, text.len()),
            text,
        }
    }

    #[test]
    fn clause_keywords_match_case_insensitively() {
        for word in ["where", "WHERE", "Join", "returning", "OUTPUT"] {
            assert!(is_clause_keyword(word), "{word}");
        }
        for word in ["orders", "wherever", "id", ""] {
            assert!(!is_clause_keyword(word), "{word}");
        }
    }

    #[test]
    fn number_shapes_map_one_to_one() {
        assert_eq!(
            number_shape_to_literal(NumberShape::Integer),
            LiteralShape::Integer
        );
        assert_eq!(
            number_shape_to_literal(NumberShape::Decimal),
            LiteralShape::Decimal
        );
        assert_eq!(
            number_shape_to_literal(NumberShape::Scientific),
            LiteralShape::Scientific
        );
        assert_eq!(number_shape_to_literal(NumberShape::Hex), LiteralShape::Hex);
    }

    #[test]
    fn column_start_heuristic() {
        let ident = |text| tok(text, TokenKind::Ident(IdentQuote::None));
        // Constraint keywords always read as a new element.
        assert!(looks_like_column_start(&ident("constraint"), None));
        assert!(looks_like_column_start(&ident("primary"), None));
        // "name type" shape: word followed by a word or `(`.
        assert!(looks_like_column_start(
            &ident("Name"),
            Some(&ident("varchar"))
        ));
        assert!(looks_like_column_start(
            &ident("Name"),
            Some(&tok("(", TokenKind::LParen))
        ));
        // Word followed by a comma is a continuation, not a new column.
        assert!(!looks_like_column_start(
            &ident("Name"),
            Some(&tok(",", TokenKind::Comma))
        ));
        // Non-words never start a column.
        assert!(!looks_like_column_start(
            &tok("%", TokenKind::Operator),
            Some(&ident("x"))
        ));
    }

    #[test]
    fn routine_body_start_keywords() {
        for text in ["as", "AS", "begin", "returns", "return", "is", "language"] {
            assert!(
                is_routine_body_start(&tok(text, TokenKind::Keyword)),
                "{text}"
            );
        }
        assert!(!is_routine_body_start(&tok("select", TokenKind::Keyword)));
    }
}
