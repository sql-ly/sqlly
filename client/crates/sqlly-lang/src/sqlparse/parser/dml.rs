//! DML statements: INSERT (column list + VALUES arities), UPDATE, DELETE,
//! and EXEC/CALL argument capture.

use crate::sqllex::{LexContext, TokenKind};
use crate::sqlmodel::{Span, SqlName};

use super::StatementParser;
use super::support::is_clause_keyword;
use crate::sqlparse::tree::{
    ExecNode, InsertNode, QueryNode, StatementNode, SymbolUse, TableSource, UpdateNode, UseKind,
};

impl<'c, 'a> StatementParser<'c, 'a> {
    pub(crate) fn parse_insert(&mut self) -> StatementNode {
        let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        self.cur.advance(); // insert
        self.cur.eat_keyword("into");
        self.cur.eat_keyword("ignore"); // mysql

        let target = match self.parse_object_ref() {
            Some((reference, span)) => {
                self.references.push(SymbolUse {
                    reference: reference.clone(),
                    use_kind: UseKind::TableSource,
                    span,
                    via_alias: None,
                    statement_index: 0,
                    in_predicate: false,
                });
                TableSource {
                    reference,
                    alias: None,
                    span,
                }
            }
            None => {
                let end = self.consume_rest();
                return StatementNode::Opaque {
                    span: self.span(Span::new(start, end)),
                    head: "insert".into(),
                };
            }
        };

        // Column list
        let mut columns = Vec::new();
        if self
            .cur
            .peek()
            .map(|t| t.kind == TokenKind::LParen)
            .unwrap_or(false)
        {
            self.cur.advance();
            self.cur.push_context(LexContext::SelectList);
            while let Some(t) = self.cur.peek().copied() {
                if t.kind == TokenKind::RParen {
                    self.cur.advance();
                    break;
                }
                if t.is_word() {
                    columns.push((
                        SqlName::new(t.ident_text(), self.dialect, t.ident_quoted()),
                        self.span(t.span),
                    ));
                    self.cur.advance();
                    if !self.cur.eat_separator() && self.cur.eat_kind(TokenKind::RParen).is_some() {
                        break;
                    }
                } else {
                    self.cur.advance();
                }
            }
            self.cur.pop_context();
        }

        // VALUES rows: count expressions per row.
        let mut value_row_arities = Vec::new();
        if self.cur.eat_keyword("values") {
            while self
                .cur
                .peek()
                .map(|t| t.kind == TokenKind::LParen)
                .unwrap_or(false)
            {
                self.cur.advance();
                let mut arity = 0usize;
                let mut depth = 0usize;
                let mut any = false;
                while let Some(t) = self.cur.peek().copied() {
                    match t.kind {
                        TokenKind::LParen => {
                            depth += 1;
                            any = true;
                            self.cur.advance();
                        }
                        TokenKind::RParen => {
                            if depth == 0 {
                                self.cur.advance();
                                break;
                            }
                            depth -= 1;
                            self.cur.advance();
                        }
                        TokenKind::Comma if depth == 0 => {
                            arity += 1;
                            self.cur.advance();
                        }
                        _ => {
                            any = true;
                            self.cur.advance();
                        }
                    }
                }
                value_row_arities.push(if any { arity + 1 } else { 0 });
                if self.cur.eat_kind(TokenKind::Comma).is_none() {
                    break;
                }
            }
        }

        // An `INSERT … SELECT` tail carries real table sources: scan it like
        // a query so they ground and its predicate columns are checked. Other
        // tails (EXEC, RETURNING, ON CONFLICT after VALUES) stay opaque.
        let end = if self
            .cur
            .peek()
            .map(|t| t.keyword_eq("select"))
            .unwrap_or(false)
        {
            let mut scratch = QueryNode::default();
            self.scan_query_body(&mut scratch)
        } else {
            self.consume_rest()
        };
        StatementNode::Insert(InsertNode {
            target,
            columns,
            value_row_arities,
            span: self.span(Span::new(start, end)),
        })
    }

    pub(crate) fn parse_update(&mut self) -> StatementNode {
        let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        self.cur.advance(); // update
        // T-SQL `UPDATE STATISTICS <table> …`: statistics maintenance, not a
        // data update — modeling it as one would ground `STATISTICS` as the
        // target table and trip the mutation checks. The table still grounds.
        if self.dialect == crate::sqlmodel::Dialect::SqlServer && self.cur.eat_word("statistics") {
            if let Some((reference, span)) = self.parse_object_ref() {
                self.references.push(SymbolUse {
                    reference,
                    use_kind: UseKind::TableSource,
                    span,
                    via_alias: None,
                    statement_index: 0,
                    in_predicate: false,
                });
            }
            let end = self.consume_rest();
            return StatementNode::Opaque {
                span: self.span(Span::new(start, end)),
                head: "update statistics".into(),
            };
        }
        // PG `UPDATE ONLY <table>`: inheritance modifier, not the target.
        self.cur.eat_keyword("only");
        let target = match self.parse_object_ref() {
            Some((reference, span)) => {
                let mut alias = None;
                if let Some(t) = self.cur.peek().copied()
                    && matches!(t.kind, TokenKind::Ident(_))
                    && !is_clause_keyword(t.text)
                {
                    alias = Some(t.ident_text().to_string());
                    self.cur.advance();
                }
                self.references.push(SymbolUse {
                    reference: reference.clone(),
                    use_kind: UseKind::TableSource,
                    span,
                    via_alias: alias.clone(),
                    statement_index: 0,
                    in_predicate: false,
                });
                TableSource {
                    reference,
                    alias,
                    span,
                }
            }
            None => {
                let end = self.consume_rest();
                return StatementNode::Opaque {
                    span: self.span(Span::new(start, end)),
                    head: "update".into(),
                };
            }
        };
        // Scan the tail (SET/FROM/WHERE) so qualified and bare predicate
        // columns are recorded; extra FROM/JOIN sources land in a scratch
        // node (their references still register globally).
        let mut scratch = QueryNode::default();
        let end = self.scan_query_body(&mut scratch);
        StatementNode::Update(UpdateNode {
            target,
            span: self.span(Span::new(start, end)),
        })
    }

    pub(crate) fn parse_delete(&mut self) -> StatementNode {
        let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        self.cur.advance(); // delete
        self.cur.eat_keyword("from");
        // PG `DELETE FROM ONLY <table>`.
        self.cur.eat_keyword("only");
        let mut node = QueryNode::default();
        if let Some((reference, span)) = self.parse_object_ref() {
            let mut alias = None;
            if let Some(t) = self.cur.peek().copied()
                && matches!(t.kind, TokenKind::Ident(_))
                && !is_clause_keyword(t.text)
            {
                alias = Some(t.ident_text().to_string());
                self.cur.advance();
            }
            self.references.push(SymbolUse {
                reference: reference.clone(),
                use_kind: UseKind::TableSource,
                span,
                via_alias: alias.clone(),
                statement_index: 0,
                in_predicate: false,
            });
            node.tables.push(TableSource {
                reference,
                alias,
                span,
            });
        }
        let end = self.scan_query_body(&mut node);
        node.span = self.span(Span::new(start, end));
        StatementNode::Delete(node)
    }

    pub(crate) fn parse_exec(&mut self) -> StatementNode {
        let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        let head = self
            .cur
            .advance()
            .map(|t| t.text.to_ascii_lowercase())
            .unwrap_or_default();
        let target = match self.parse_object_ref() {
            Some((r, span)) => {
                self.references.push(SymbolUse {
                    reference: r.clone(),
                    use_kind: UseKind::RoutineCall,
                    span,
                    via_alias: None,
                    statement_index: 0,
                    in_predicate: false,
                });
                r
            }
            None => {
                let end = self.consume_rest();
                return StatementNode::Opaque {
                    span: self.span(Span::new(start, end)),
                    head,
                };
            }
        };

        // Arguments: T-SQL "@name = value, value"; CALL "(value, …)".
        let mut args = Vec::new();
        let parenthesized = self.cur.eat_kind(TokenKind::LParen).is_some();
        self.cur.push_context(LexContext::ArgList);
        while let Some(&t) = self.cur.peek() {
            if parenthesized && t.kind == TokenKind::RParen {
                self.cur.advance();
                break;
            }
            if t.kind == TokenKind::Semicolon {
                break;
            }
            let mut name = None;
            if matches!(t.kind, TokenKind::ParamMarker(_)) {
                // Named arg if followed by `=`
                if self
                    .cur
                    .peek_n(1)
                    .map(|n| n.kind == TokenKind::Operator && n.text == "=")
                    .unwrap_or(false)
                {
                    name = Some(t.text.to_string());
                    self.cur.advance();
                    self.cur.advance();
                }
            }
            let value = self.capture_slot_value();
            if value.is_empty() && name.is_none() {
                // Nothing consumable; avoid an infinite loop.
                if self.cur.advance().is_none() {
                    break;
                }
                continue;
            }
            args.push((name, value));
            if !self.cur.eat_separator() {
                if parenthesized && self.cur.eat_kind(TokenKind::RParen).is_some() {
                    break;
                }
                break;
            }
        }
        self.cur.pop_context();

        let end = self.consume_rest();
        StatementNode::Exec(ExecNode {
            target,
            span: self.span(Span::new(start, end)),
            args,
        })
    }
}
