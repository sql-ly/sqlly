//! Excel's grammar and precedence, lowest to highest:
//! comparisons (`= <> < > <= >=`), `&`, `+ -`, `* /`, `^`, `%` (postfix),
//! unary `- +`, and the range operator `:`. Binary operators are left
//! associative — Excel evaluates `2^3^2` as 64 — and negation binds tighter
//! than `^`, so `-2^2` is 4.

use super::lexer::{Tok, Token, lex};
use super::{CellRef, ErrorKind, Formula, ParseError, RefSpan};

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Number(f64),
    Text(String),
    Bool(bool),
    Error(ErrorKind),
    Ref(CellRef),
    Range(CellRef, CellRef),
    Var(String),
    /// An omitted argument: `IF(x,,1)`.
    Missing,
    Neg(Box<Expr>),
    Percent(Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

/// Parse `source`, with or without Excel's leading `=`.
pub fn parse(source: &str) -> Result<Formula, ParseError> {
    let trimmed_start = source.len() - source.trim_start().len();
    let body_start = if source.trim_start().starts_with('=') {
        trimmed_start + 1
    } else {
        0
    };
    let tokens = lex(&source[body_start..]).map_err(|mut e| {
        e.at += body_start;
        e
    })?;
    let tokens: Vec<Token> = tokens
        .into_iter()
        .map(|mut t| {
            t.start += body_start;
            t.end += body_start;
            t
        })
        .collect();
    if tokens.is_empty() {
        return Err(ParseError {
            message: crate::localization::text("results.formula_parser.empty_formula"),
            at: source.len(),
        });
    }
    let mut p = Parser {
        tokens: &tokens,
        pos: 0,
        end: source.len(),
    };
    let expr = p.expr(0)?;
    if let Some(t) = p.peek() {
        return Err(ParseError {
            message: match t.tok {
                Tok::RParen => {
                    crate::localization::text("results.formula_parser.unmatched_close_parenthesis")
                }
                _ => crate::localization::text("results.formula_parser.missing_between_parts"),
            },
            at: t.start,
        });
    }
    let refs = tokens
        .iter()
        .filter_map(|t| match &t.tok {
            Tok::Ref(r) => Some(RefSpan {
                start: t.start,
                end: t.end,
                column: r.column.clone(),
                row: r.row,
            }),
            _ => None,
        })
        .collect();
    Ok(Formula {
        source: source.to_string(),
        expr,
        refs,
    })
}

pub(crate) fn has_range(expr: &Expr) -> bool {
    match expr {
        Expr::Range(..) => true,
        Expr::Neg(e) | Expr::Percent(e) => has_range(e),
        Expr::Binary(_, a, b) => has_range(a) || has_range(b),
        Expr::Call(_, args) => args.iter().any(has_range),
        _ => false,
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    end: usize,
}

/// Binding power of a binary operator (higher binds tighter).
pub(crate) fn binary_power(op: &str) -> Option<u8> {
    Some(match op {
        "=" | "<>" | "<" | ">" | "<=" | ">=" => 1,
        "&" => 2,
        "+" | "-" => 3,
        "*" | "/" => 4,
        "^" => 5,
        _ => return None,
    })
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn at(&self) -> usize {
        self.peek().map(|t| t.start).unwrap_or(self.end)
    }

    fn fail<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            message: message.into(),
            at: self.at(),
        })
    }

    fn expr(&mut self, min: u8) -> Result<Expr, ParseError> {
        let mut left = self.percent()?;
        while let Some(Token {
            tok: Tok::Op(op), ..
        }) = self.peek()
        {
            let op = *op;
            let Some(power) = binary_power(op) else {
                break;
            };
            if power < min {
                break;
            }
            self.pos += 1;
            // Left associative: the right side only takes tighter operators.
            let right = self.expr(power + 1)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn percent(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.unary()?;
        while matches!(
            self.peek(),
            Some(Token {
                tok: Tok::Op("%"),
                ..
            })
        ) {
            self.pos += 1;
            e = Expr::Percent(Box::new(e));
        }
        Ok(e)
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek().map(|t| &t.tok) {
            Some(Tok::Op("-")) => {
                self.pos += 1;
                Ok(Expr::Neg(Box::new(self.unary()?)))
            }
            Some(Tok::Op("+")) => {
                self.pos += 1;
                self.unary()
            }
            _ => self.range(),
        }
    }

    fn range(&mut self) -> Result<Expr, ParseError> {
        let first = self.primary()?;
        if !matches!(
            self.peek(),
            Some(Token {
                tok: Tok::Colon,
                ..
            })
        ) {
            return Ok(first);
        }
        let Expr::Ref(from) = first else {
            return self.fail(crate::localization::text(
                "results.formula_parser.range_needs_references",
            ));
        };
        self.pos += 1;
        match self.primary()? {
            Expr::Ref(to) => Ok(Expr::Range(from, to)),
            _ => self.fail(crate::localization::text(
                "results.formula_parser.range_needs_references",
            )),
        }
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let Some(token) = self.peek().cloned() else {
            return self.fail(crate::localization::text(
                "results.formula_parser.unexpected_end",
            ));
        };
        self.pos += 1;
        Ok(match token.tok {
            Tok::Number(n) => Expr::Number(n),
            Tok::Text(t) => Expr::Text(t),
            Tok::Bool(b) => Expr::Bool(b),
            Tok::Error(e) => Expr::Error(e),
            Tok::Ref(r) => Expr::Ref(r),
            Tok::Var(v) => Expr::Var(v),
            Tok::LParen => {
                let inner = self.expr(0)?;
                if !matches!(
                    self.peek(),
                    Some(Token {
                        tok: Tok::RParen,
                        ..
                    })
                ) {
                    return self.fail(crate::localization::text(
                        "results.formula_parser.unclosed_parenthesis",
                    ));
                }
                self.pos += 1;
                inner
            }
            Tok::Ident(name) => {
                if !matches!(
                    self.peek(),
                    Some(Token {
                        tok: Tok::LParen,
                        ..
                    })
                ) {
                    self.pos -= 1;
                    return self.fail(crate::localization::format(
                        "results.formula_parser.name_without_call",
                        &[("name", &name)],
                    ));
                }
                self.pos += 1;
                let mut args = Vec::new();
                if matches!(
                    self.peek(),
                    Some(Token {
                        tok: Tok::RParen,
                        ..
                    })
                ) {
                    self.pos += 1;
                    return Ok(Expr::Call(name, args));
                }
                loop {
                    if matches!(
                        self.peek(),
                        Some(Token {
                            tok: Tok::Comma | Tok::RParen,
                            ..
                        })
                    ) {
                        args.push(Expr::Missing);
                    } else {
                        args.push(self.expr(0)?);
                    }
                    match self.peek().map(|t| &t.tok) {
                        Some(Tok::Comma) => self.pos += 1,
                        Some(Tok::RParen) => {
                            self.pos += 1;
                            break;
                        }
                        _ => {
                            return self.fail(crate::localization::format(
                                "results.formula_parser.unclosed_function",
                                &[("function_name", &name)],
                            ));
                        }
                    }
                }
                Expr::Call(name, args)
            }
            Tok::RParen => {
                self.pos -= 1;
                return self.fail(crate::localization::text(
                    "results.formula_parser.missing_before_close_parenthesis",
                ));
            }
            Tok::Comma | Tok::Colon => {
                self.pos -= 1;
                return self.fail(crate::localization::text(
                    "results.formula_parser.missing_operand",
                ));
            }
            Tok::Op(op) => {
                self.pos -= 1;
                return self.fail(crate::localization::format(
                    "results.formula_parser.missing_before_operator",
                    &[("operator", op)],
                ));
            }
        })
    }
}
