//! Recovering statement parser. [`StatementParser`] is one impl split across
//! sibling modules by concern: statement dispatch and shared name/type
//! parsing live here; `ddl` covers CREATE/ALTER definitions, `query` covers
//! SELECT/WITH scanning and reference harvesting, `dml` covers
//! INSERT/UPDATE/DELETE/EXEC, and `support` holds token- and text-capture
//! helpers.

mod ddl;
mod dml;
mod query;
mod support;
#[cfg(test)]
mod tests;

use crate::sqllex::{TokenCursor, TokenKind};
use crate::sqlmodel::{Dialect, ObjectRef, Span, SqlName, TypeRef};

use crate::sqlparse::tree::{CteDef, StatementNode, SymbolUse};

pub(crate) struct StatementParser<'c, 'a> {
    pub(crate) cur: &'c mut TokenCursor<'a>,
    pub(crate) dialect: Dialect,
    pub(crate) references: &'c mut Vec<SymbolUse>,
    pub(crate) ctes: &'c mut Vec<CteDef>,
}

impl<'c, 'a> StatementParser<'c, 'a> {
    /// Token spans are document-absolute (the splitter lexes the whole
    /// document once), so output spans are the token spans unchanged.
    pub(crate) fn span(&self, s: Span) -> Span {
        s
    }

    pub(crate) fn parse_statement(&mut self) -> Option<StatementNode> {
        let first = self.cur.peek()?.text.to_ascii_lowercase();
        match first.as_str() {
            // ALTER PROCEDURE/FUNCTION/VIEW share CREATE's structure; routing
            // them here lets the routine/view body be harvested for table
            // sources so `alias.`-dot completion works inside an altered body.
            "create" | "alter" => self.parse_create(),
            "with" => self.parse_with(),
            "select" => Some(self.parse_select()),
            "insert" => Some(self.parse_insert()),
            "update" => Some(self.parse_update()),
            "delete" => Some(self.parse_delete()),
            // MySQL/SQLite `REPLACE INTO` has INSERT's shape.
            "replace" => Some(self.parse_insert()),
            // MERGE stays opaque as a statement, but its target and USING
            // sources are harvested tolerantly so they ground and complete.
            "merge" => {
                let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
                let end = self.harvest_routine_body();
                Some(StatementNode::Opaque {
                    span: self.span(Span::new(start, end)),
                    head: "merge".into(),
                })
            }
            "exec" | "execute" | "call" => Some(self.parse_exec()),
            _ => {
                let head = first;
                let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
                let mut end = start;
                while let Some(t) = self.cur.advance() {
                    end = t.span.end;
                }
                Some(StatementNode::Opaque {
                    span: self.span(Span::new(start, end)),
                    head,
                })
            }
        }
    }

    // -- shared pieces ------------------------------------------------------

    /// Parse a possibly dotted object name: [a.][b.]c
    pub(crate) fn parse_object_ref(&mut self) -> Option<(ObjectRef, Span)> {
        let mut parts: Vec<(String, bool, Span)> = Vec::new();
        loop {
            let t = match self.cur.peek() {
                Some(t) if t.is_word() => *t,
                _ => break,
            };
            self.cur.advance();
            parts.push((t.ident_text().to_string(), t.ident_quoted(), t.span));
            if self.cur.eat_kind(TokenKind::Dot).is_none() {
                break;
            }
        }
        if parts.is_empty() {
            return None;
        }
        let span = Span::new(parts[0].2.start, parts[parts.len() - 1].2.end);
        let mk = |p: &(String, bool, Span)| SqlName::new(p.0.clone(), self.dialect, p.1);
        let reference = match parts.len() {
            1 => ObjectRef::bare(mk(&parts[0])),
            2 => ObjectRef::new(None, Some(mk(&parts[0])), mk(&parts[1])),
            _ => ObjectRef::new(Some(mk(&parts[0])), Some(mk(&parts[1])), mk(&parts[2])),
        };
        Some((reference, self.span(span)))
    }

    pub(crate) fn parse_type(&mut self) -> Option<TypeRef> {
        let t = self.cur.peek()?;
        if !t.is_word() {
            return None;
        }
        let mut native = t.text.to_string();
        self.cur.advance();
        // Multi-word types: double precision, character varying, timestamp
        // with time zone …
        loop {
            let next = match self.cur.peek() {
                Some(n) if n.is_word() => n.text.to_ascii_lowercase(),
                _ => break,
            };
            let continues = matches!(
                next.as_str(),
                "precision" | "varying" | "time" | "zone" | "local"
            ) || (next == "with"
                && self
                    .cur
                    .peek_n(1)
                    .map(|n| n.text.eq_ignore_ascii_case("time"))
                    .unwrap_or(false));
            if !continues {
                break;
            }
            native.push(' ');
            native.push_str(self.cur.advance().expect("peeked").text);
        }
        // Type parameters: (n), (p,s), (max)
        if self
            .cur
            .peek()
            .map(|t| t.kind == TokenKind::LParen)
            .unwrap_or(false)
        {
            native.push('(');
            self.cur.advance();
            let mut depth = 1;
            while let Some(t) = self.cur.advance() {
                match t.kind {
                    TokenKind::LParen => depth += 1,
                    TokenKind::RParen => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                native.push_str(t.text);
            }
            native.push(')');
        }
        // MySQL unsigned suffix folds into the native spelling.
        if self
            .cur
            .peek()
            .map(|t| t.keyword_eq("unsigned"))
            .unwrap_or(false)
        {
            native.push(' ');
            native.push_str(self.cur.advance().expect("peeked").text);
        }
        Some(TypeRef::parse(&native, self.dialect))
    }
}
