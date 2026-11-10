//! Query-body scanning: SELECT statements, WITH/CTE harvesting, table
//! sources with aliases, qualified and bare predicate columns, and the
//! tolerant routine-body harvest. Iterative throughout — hostile input can
//! never grow the stack.

use crate::sqllex::TokenKind;
use crate::sqlmodel::{Dialect, ObjectRef, Span, SqlName};

use super::StatementParser;
use super::support::{is_clause_keyword, is_keyword_arg_function, is_tsql_table_hint};
use crate::sqlparse::tree::{CteDef, QueryNode, StatementNode, SymbolUse, TableSource, UseKind};

/// What a parsed source name stands for (see `Parser::source_ref_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// A catalog table (or view): grounded and resolved.
    Table,
    /// A table-valued function call: columns the catalog never saw, so an
    /// opaque source.
    Function,
    /// MySQL's `DUAL`: contributes no columns, neither grounded nor opaque.
    Nothing,
}

impl<'c, 'a> StatementParser<'c, 'a> {
    pub(crate) fn parse_select(&mut self) -> StatementNode {
        let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        let mut node = QueryNode::default();
        let end = self.scan_query_body(&mut node);
        node.span = self.span(Span::new(start, end));
        StatementNode::Select(node)
    }

    /// Walk a query's token stream to its end, harvesting FROM/JOIN table
    /// sources, `alias.column` qualifiers, and — inside WHERE / JOIN ON
    /// predicates only — bare column candidates. Tracks the current clause so
    /// predicate uses carry `in_predicate` and bare candidates are never
    /// recorded where select-list aliases could legally appear (ORDER BY,
    /// GROUP BY, HAVING, the select list itself). Iterative throughout;
    /// subqueries in predicates are skipped wholesale so their inner columns
    /// never leak into the outer scope's candidates. Returns the end offset.
    pub(crate) fn scan_query_body(&mut self, node: &mut QueryNode) -> usize {
        let mut end = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        // in_predicate: WHERE / ON / HAVING (tenant checks care about all
        // three). bare_ok: WHERE / ON only (HAVING can reference select
        // aliases on MySQL).
        let mut in_predicate = false;
        let mut bare_ok = false;
        // The previous meaningful token's lowercase text, for the `::` cast
        // and COLLATE exclusions.
        let mut prev_text: Option<String> = None;
        while let Some(t) = self.cur.peek().copied() {
            if t.keyword_eq("from") || t.keyword_eq("join") {
                in_predicate = false;
                bare_ok = false;
                prev_text = Some(t.text.to_ascii_lowercase());
                self.cur.advance();
                self.parse_source_with_operators(node);
                // SQL-92 comma joins: `FROM a, b, (SELECT …) d` lists more
                // sources behind commas.
                while self
                    .cur
                    .peek()
                    .map(|n| n.kind == TokenKind::Comma)
                    .unwrap_or(false)
                {
                    self.cur.advance();
                    self.parse_source_with_operators(node);
                }
                end = self.cur.peek().map(|t| t.span.start).unwrap_or(end);
                continue;
            }
            if t.is_word()
                && !t.ident_quoted()
                && t.text.eq_ignore_ascii_case("apply")
                && matches!(prev_text.as_deref(), Some("cross" | "outer"))
            {
                // T-SQL / Oracle `CROSS APPLY` / `OUTER APPLY`: a source in
                // join position, usually a table-valued function or a
                // correlated subquery whose columns the catalog never saw.
                in_predicate = false;
                bare_ok = false;
                prev_text = Some("apply".to_string());
                self.cur.advance();
                self.parse_source_with_operators(node);
                end = self.cur.peek().map(|t| t.span.start).unwrap_or(end);
                continue;
            }
            if t.keyword_eq("on")
                && self
                    .cur
                    .peek_n(1)
                    .map(|n| {
                        n.text.eq_ignore_ascii_case("conflict")
                            || n.text.eq_ignore_ascii_case("duplicate")
                    })
                    .unwrap_or(false)
            {
                // INSERT upsert tail (`ON CONFLICT …` / `ON DUPLICATE KEY
                // UPDATE …`): its words are conflict targets and assignments,
                // not table sources or checkable predicate columns.
                return self.consume_rest();
            }
            if t.keyword_eq("where") || t.keyword_eq("on") {
                in_predicate = true;
                bare_ok = true;
                prev_text = Some(t.text.to_ascii_lowercase());
                end = t.span.end;
                self.cur.advance();
                continue;
            }
            if t.keyword_eq("having") {
                in_predicate = true;
                bare_ok = false;
                prev_text = Some(t.text.to_ascii_lowercase());
                end = t.span.end;
                self.cur.advance();
                continue;
            }
            if t.keyword_eq("select")
                || t.keyword_eq("group")
                || t.keyword_eq("order")
                || t.keyword_eq("union")
                || t.keyword_eq("set")
            {
                in_predicate = false;
                bare_ok = false;
                prev_text = Some(t.text.to_ascii_lowercase());
                end = t.span.end;
                self.cur.advance();
                continue;
            }
            if t.is_word()
                && is_keyword_arg_function(t.text)
                && self
                    .cur
                    .peek_n(1)
                    .map(|n| n.kind == TokenKind::LParen)
                    .unwrap_or(false)
            {
                // EXTRACT/SUBSTRING/TRIM/OVERLAY carry a FROM keyword inside
                // their argument list; scanning into it would misread the
                // argument as a table source.
                self.cur.advance();
                self.skip_balanced_parens();
                prev_text = None;
                end = self.cur.prev_end();
                continue;
            }
            if in_predicate
                && t.kind == TokenKind::LParen
                && self
                    .cur
                    .peek_n(1)
                    .map(|n| n.keyword_eq("select"))
                    .unwrap_or(false)
            {
                // Predicate subquery: its columns belong to an inner scope.
                self.skip_balanced_parens();
                prev_text = None;
                end = self.cur.prev_end();
                continue;
            }
            if self.try_record_column_qualifier(in_predicate) {
                prev_text = None;
                end = self.cur.prev_end();
                continue;
            }
            if bare_ok && self.try_record_bare_column(prev_text.as_deref()) {
                prev_text = None;
                end = self.cur.prev_end();
                continue;
            }
            prev_text = Some(t.text.to_ascii_lowercase());
            end = t.span.end;
            self.cur.advance();
        }
        end
    }

    /// Record a bare-column candidate in a predicate, or consume a function
    /// call whole. Only fires on plain identifiers that cannot be anything
    /// else: not a qualifier or member of a dotted path, not a function name,
    /// not a special register, not the target of a cast or COLLATE.
    pub(crate) fn try_record_bare_column(&mut self, prev_text: Option<&str>) -> bool {
        let t = match self.cur.peek().copied() {
            Some(t) if matches!(t.kind, TokenKind::Ident(_)) => t,
            _ => return false,
        };
        if matches!(
            prev_text,
            Some(".") | Some("::") | Some("collate") | Some("as")
        ) {
            return false;
        }
        match self.cur.peek_n(1).map(|n| n.kind) {
            // Leading part of a dotted path — the qualifier pass owns it.
            Some(TokenKind::Dot) => return false,
            // Function call: consume the name and its argument list whole.
            // Argument identifiers are excluded on purpose — dateparts,
            // interval units, and other function-keyword arguments lex as
            // identifiers and would be unfixable false positives.
            Some(TokenKind::LParen) => {
                self.cur.advance();
                self.skip_balanced_parens();
                return true;
            }
            _ => {}
        }
        // Special registers that lex as identifiers on some dialects.
        let lower = t.ident_text().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "current_timestamp"
                | "current_date"
                | "current_time"
                | "current_user"
                | "session_user"
                | "system_user"
                | "sysdate"
                | "getdate"
                | "rownum"
        ) {
            self.cur.advance();
            return true;
        }
        self.references.push(SymbolUse {
            reference: ObjectRef::new(
                None,
                None,
                SqlName::new(t.ident_text(), self.dialect, t.ident_quoted()),
            ),
            use_kind: UseKind::ColumnBare,
            span: self.span(t.span),
            via_alias: None,
            statement_index: 0,
            in_predicate: true,
        });
        self.cur.advance();
        true
    }

    /// Parse `WITH [RECURSIVE] name [(cols)] AS (body) [, …] <statement>`.
    /// CTE bodies are harvested (table sources + qualified columns) without
    /// being structured; the trailing statement parses normally. A second
    /// leading WITH (never valid SQL) is consumed opaquely instead of
    /// recursing, so hostile input cannot grow the stack.
    pub(crate) fn parse_with(&mut self) -> Option<StatementNode> {
        let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        self.cur.advance(); // with
        self.cur.eat_keyword("recursive");
        loop {
            let name_tok = match self.cur.peek().copied() {
                Some(t) if matches!(t.kind, TokenKind::Ident(_)) => t,
                _ => break,
            };
            self.cur.advance();
            let name = name_tok.ident_text().to_string();
            let mut columns = Vec::new();
            if self
                .cur
                .peek()
                .map(|t| t.kind == TokenKind::LParen)
                .unwrap_or(false)
            {
                self.cur.advance();
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
                            columns.push(t.ident_text().to_string());
                            self.cur.advance();
                        }
                        _ => {
                            self.cur.advance();
                        }
                    }
                }
            }
            if !self.cur.eat_keyword("as") {
                break;
            }
            if self
                .cur
                .peek()
                .map(|t| t.kind == TokenKind::LParen)
                .unwrap_or(false)
            {
                self.scan_parenthesized_body();
            }
            self.ctes.push(CteDef {
                name,
                columns,
                span: self.span(name_tok.span),
                statement_index: 0,
            });
            if self
                .cur
                .peek()
                .map(|t| t.kind == TokenKind::Comma)
                .unwrap_or(false)
            {
                self.cur.advance();
            } else {
                break;
            }
        }
        if self
            .cur
            .peek()
            .map(|t| t.text.eq_ignore_ascii_case("with"))
            .unwrap_or(false)
        {
            let end = self.consume_rest();
            return Some(StatementNode::Opaque {
                span: self.span(Span::new(start, end)),
                head: "with".into(),
            });
        }
        match self.parse_statement() {
            Some(node) => Some(node),
            None => Some(StatementNode::Opaque {
                span: self.span(Span::new(start, self.cur.prev_end())),
                head: "with".into(),
            }),
        }
    }

    /// Consume a parenthesized query body (CTE or derived table), harvesting
    /// FROM/JOIN table sources and `alias.column` qualifiers at every depth
    /// with an integer depth counter — never recursion. Paren-headed sources
    /// inside the body are left to the depth counter rather than re-entered.
    pub(crate) fn scan_parenthesized_body(&mut self) {
        if !self
            .cur
            .peek()
            .map(|t| t.kind == TokenKind::LParen)
            .unwrap_or(false)
        {
            return;
        }
        self.cur.advance();
        let mut depth: u32 = 1;
        while let Some(t) = self.cur.peek().copied() {
            match t.kind {
                TokenKind::LParen => {
                    depth += 1;
                    self.cur.advance();
                }
                TokenKind::RParen => {
                    depth -= 1;
                    self.cur.advance();
                    if depth == 0 {
                        return;
                    }
                }
                _ if t.is_word()
                    && is_keyword_arg_function(t.text)
                    && self
                        .cur
                        .peek_n(1)
                        .map(|n| n.kind == TokenKind::LParen)
                        .unwrap_or(false) =>
                {
                    // EXTRACT/SUBSTRING/TRIM/OVERLAY: the FROM inside their
                    // argument list is not a table source. The whole group is
                    // consumed here, so the depth counter is unaffected.
                    self.cur.advance();
                    self.skip_balanced_parens();
                }
                _ if t.keyword_eq("from") || t.keyword_eq("join") => {
                    self.cur.advance();
                    if self
                        .cur
                        .peek()
                        .map(|n| n.kind == TokenKind::LParen)
                        .unwrap_or(false)
                    {
                        // A paren-headed source here is handled by the depth
                        // counter on the next iteration.
                        continue;
                    }
                    let mut scratch = QueryNode::default();
                    self.parse_table_source_into(&mut scratch);
                }
                _ => {
                    if !self.try_record_column_qualifier(false) {
                        self.cur.advance();
                    }
                }
            }
        }
    }

    /// Detect a qualified column reference `alias.column` (e.g. in SELECT/ON/
    /// WHERE) and record it as a [`UseKind::Column`] use so model validation can
    /// flag references through an undefined alias. Only a two-part
    /// `<identifier>.<member>` is recorded; a deeper path (`schema.table.col`)
    /// is a fully-qualified column and is left to object resolution. Returns
    /// `true` when it consumed the `alias.column` pair.
    pub(crate) fn try_record_column_qualifier(&mut self, in_predicate: bool) -> bool {
        let qualifier = match self.cur.peek().copied() {
            // The qualifier must be a real identifier, never a keyword.
            Some(t) if matches!(t.kind, TokenKind::Ident(_)) => t,
            _ => return false,
        };
        if self.cur.peek_n(1).map(|t| t.kind) != Some(TokenKind::Dot) {
            return false;
        }
        let member = match self.cur.peek_n(2) {
            Some(t) if t.is_word() => *t,
            _ => return false,
        };
        // A third dot means `schema.table.column`: a full path, not an alias.
        if self.cur.peek_n(3).map(|t| t.kind) == Some(TokenKind::Dot) {
            return false;
        }
        let reference = ObjectRef::new(
            None,
            Some(SqlName::new(
                qualifier.ident_text(),
                self.dialect,
                qualifier.ident_quoted(),
            )),
            SqlName::new(member.ident_text(), self.dialect, member.ident_quoted()),
        );
        self.references.push(SymbolUse {
            reference,
            use_kind: UseKind::Column,
            // Span underlines the qualifier so the squiggle lands on `t`.
            span: self.span(qualifier.span),
            via_alias: Some(qualifier.ident_text().to_string()),
            statement_index: 0,
            in_predicate,
        });
        // Consume `alias . column`.
        self.cur.advance();
        self.cur.advance();
        self.cur.advance();
        true
    }

    /// MySQL's dummy `FROM dual`: not a catalog table, but not an opaque
    /// source either — it contributes no columns, so it must neither be
    /// grounded nor switch the statement's bare-column checks off.
    fn source_ref_is_mysql_dual(&self, reference: &ObjectRef) -> bool {
        self.dialect == Dialect::MySql
            && reference.namespace.is_none()
            && reference.catalog.is_none()
            && reference.name.fold_key() == "dual"
    }

    /// After a source name was parsed: what kind of source it names. The
    /// one place that tells a catalog table from a table-valued function
    /// call (`FROM STRING_SPLIT(…)`, `FROM unnest(…)`: the name is followed
    /// by an argument list) and from MySQL's dummy `DUAL`, which contributes
    /// no columns and is neither grounded nor opaque. Consumes the argument
    /// parens of a function source so the caller can still capture a
    /// trailing alias. A T-SQL legacy table hint (`FROM t (NOLOCK)`) still
    /// counts as a table; its parens are consumed.
    pub(crate) fn source_ref_kind(&mut self, reference: &ObjectRef) -> SourceKind {
        if self.source_ref_is_mysql_dual(reference) {
            return SourceKind::Nothing;
        }
        if !self
            .cur
            .peek()
            .map(|t| t.kind == TokenKind::LParen)
            .unwrap_or(false)
        {
            return SourceKind::Table;
        }
        if self.dialect == Dialect::SqlServer
            && self
                .cur
                .peek_n(1)
                .map(|t| t.is_word() && is_tsql_table_hint(t.text))
                .unwrap_or(false)
        {
            self.skip_balanced_parens();
            return SourceKind::Table;
        }
        self.skip_balanced_parens();
        SourceKind::Function
    }

    /// The alias after a source: `AS name`, or a bare identifier that is not
    /// a clause keyword.
    fn take_source_alias(&mut self) -> Option<String> {
        if self.cur.eat_keyword("as") {
            if let Some(t) = self.cur.peek().copied()
                && t.is_word()
            {
                self.cur.advance();
                return Some(t.ident_text().to_string());
            }
            return None;
        }
        if let Some(t) = self.cur.peek().copied()
            && matches!(t.kind, TokenKind::Ident(_))
            && !is_clause_keyword(t.text)
        {
            self.cur.advance();
            return Some(t.ident_text().to_string());
        }
        None
    }

    /// One table source plus any `PIVOT (…)` / `UNPIVOT [INCLUDE|EXCLUDE
    /// NULLS] (…)` applied to it. A pivot replaces the source's columns with
    /// new ones the catalog never saw, so it is recorded as an opaque source.
    fn parse_source_with_operators(&mut self, node: &mut QueryNode) {
        self.parse_table_source_into(node);
        loop {
            let is_operator = self
                .cur
                .peek()
                .map(|t| {
                    t.is_word()
                        && !t.ident_quoted()
                        && (t.text.eq_ignore_ascii_case("pivot")
                            || t.text.eq_ignore_ascii_case("unpivot"))
                })
                .unwrap_or(false);
            if !is_operator {
                return;
            }
            let mut ahead = 1;
            if self
                .cur
                .peek_n(1)
                .map(|t| {
                    t.text.eq_ignore_ascii_case("include") || t.text.eq_ignore_ascii_case("exclude")
                })
                .unwrap_or(false)
            {
                ahead = 3;
            }
            if self.cur.peek_n(ahead).map(|t| t.kind) != Some(TokenKind::LParen) {
                return;
            }
            for _ in 0..ahead {
                self.cur.advance();
            }
            self.skip_balanced_parens();
            let alias = self.take_source_alias();
            node.derived_aliases.push(alias.unwrap_or_default());
        }
    }

    pub(crate) fn parse_table_source_into(&mut self, node: &mut QueryNode) {
        // PG source modifiers, not source names: `FROM ONLY t`,
        // `JOIN LATERAL (…)`.
        while self.cur.eat_keyword("only") || self.cur.eat_keyword("lateral") {}
        // Derived-table source: harvest the body's own sources and qualifiers,
        // then capture the alias so validation knows an opaque source exists.
        if self
            .cur
            .peek()
            .map(|t| t.kind == TokenKind::LParen)
            .unwrap_or(false)
        {
            self.scan_parenthesized_body();
            // PostgreSQL 16+ allows the subquery without an alias; it is
            // just as opaque, so it is recorded either way.
            let alias = self.take_source_alias();
            node.derived_aliases.push(alias.unwrap_or_default());
            return;
        }
        if let Some((reference, span)) = self.parse_object_ref() {
            let kind = self.source_ref_kind(&reference);
            if kind == SourceKind::Nothing {
                self.take_source_alias();
                return;
            }
            let is_table = kind == SourceKind::Table;
            if !is_table
                && self.dialect == Dialect::SqlServer
                && self
                    .cur
                    .peek()
                    .map(|t| t.keyword_eq("with"))
                    .unwrap_or(false)
                && self
                    .cur
                    .peek_n(1)
                    .map(|t| t.kind == TokenKind::LParen)
                    .unwrap_or(false)
            {
                // `OPENJSON(…) WITH (name type path, …)` / `OPENXML(…) WITH
                // (…)`: the column schema the function returns.
                self.cur.advance();
                self.skip_balanced_parens();
            }
            let alias = self.take_source_alias();
            if !is_table {
                // A function source marks an opaque source, alias or not, so
                // per-statement checks stay conservative.
                node.derived_aliases.push(alias.unwrap_or_default());
                return;
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
    }

    /// Tolerant scan of a routine body (`CREATE PROCEDURE/FUNCTION ... AS
    /// <body>`) that records `TableSource` references for every
    /// `FROM/JOIN/INTO/UPDATE <target> [AS alias | alias]` it encounters. A
    /// routine body may contain control flow (IF/WHILE/BEGIN/END/DECLARE/SET/
    /// RETURN) and statements the parser cannot fully model, so this
    /// deliberately does not structure inner statements — it only harvests the
    /// references IntelliSense needs to resolve in-scope columns and
    /// `alias.column` qualifiers. Returns the end offset of the consumed body,
    /// matching the `consume_rest` contract it replaces. Mirrors
    /// `parse_table_source_into`'s alias logic so aliases resolve identically.
    pub(crate) fn harvest_routine_body(&mut self) -> usize {
        let mut end = self.cur.prev_end();
        // The last word token seen (lowercase), reset at semicolons. Several
        // keywords introduce a table source only in some contexts, and the
        // preceding word tells them apart: `FOR UPDATE`, `ON UPDATE CASCADE`,
        // and `ON DUPLICATE KEY UPDATE` take no table; `FETCH … FROM` names a
        // cursor, not a table.
        let mut prev: Option<String> = None;
        // USING names a table source only inside MERGE/DELETE; elsewhere it
        // introduces an index method (`CREATE INDEX … USING btree`) or a
        // conversion expression (`ALTER … TYPE … USING expr`).
        let mut using_names_source = false;
        while let Some(&t) = self.cur.peek() {
            let lower = t.text.to_ascii_lowercase();
            let introduces = match lower.as_str() {
                "from" => !matches!(
                    prev.as_deref(),
                    Some("fetch" | "next" | "prior" | "first" | "last" | "absolute" | "relative")
                ),
                "join" | "into" => true,
                "update" => !matches!(prev.as_deref(), Some("on" | "for" | "key")),
                "merge" => true,
                "using" => using_names_source,
                _ => false,
            };
            if introduces {
                self.cur.advance();
                match lower.as_str() {
                    "merge" => {
                        using_names_source = true;
                        // `MERGE [INTO] <target>`: eat the optional INTO so
                        // the target isn't harvested twice.
                        self.cur.eat_keyword("into");
                    }
                    // `UPDATE STATISTICS <table>`: the table follows.
                    "update" => {
                        if self.dialect == Dialect::SqlServer {
                            self.cur.eat_word("statistics");
                        }
                    }
                    // MySQL `SELECT … INTO OUTFILE/DUMPFILE '…'` writes a
                    // file, not a table.
                    "into"
                        if self
                            .cur
                            .peek()
                            .map(|n| {
                                n.text.eq_ignore_ascii_case("outfile")
                                    || n.text.eq_ignore_ascii_case("dumpfile")
                            })
                            .unwrap_or(false) =>
                    {
                        prev = Some(lower);
                        continue;
                    }
                    _ => {}
                }
                // Only FROM/JOIN/USING positions can name a table-valued
                // function; after INTO/UPDATE/MERGE a following `(` is a
                // column list, not an argument list.
                let allow_function_sources = matches!(lower.as_str(), "from" | "join" | "using");
                self.harvest_table_source_list(allow_function_sources);
                prev = Some(lower);
                if let Some(&n) = self.cur.peek() {
                    end = n.span.end;
                }
                continue;
            }
            match lower.as_str() {
                "delete" => using_names_source = true,
                // A new DDL statement leaves the MERGE/DELETE USING context
                // even without a terminating semicolon.
                "create" | "alter" => using_names_source = false,
                _ => {}
            }
            if t.kind == TokenKind::Semicolon {
                using_names_source = false;
                prev = None;
            } else if t.is_word() {
                prev = Some(lower);
            }
            if let Some(at) = self.cur.advance() {
                end = at.span.end;
            }
        }
        end
    }

    /// Harvest one table source and any comma-joined siblings following a
    /// FROM/JOIN/INTO/UPDATE keyword, recording a `TableSource` reference with
    /// its alias. A leading `(` is a subquery source and is skipped (balanced
    /// parens); a non-object token ends the list.
    pub(crate) fn harvest_table_source_list(&mut self, allow_function_sources: bool) {
        loop {
            // PG source modifiers: `FROM ONLY t`, `JOIN LATERAL (…)`.
            while self.cur.eat_keyword("only") || self.cur.eat_keyword("lateral") {}
            if self
                .cur
                .peek()
                .map(|t| t.kind == TokenKind::LParen)
                .unwrap_or(false)
            {
                self.skip_balanced_parens();
            } else if self
                .cur
                .peek()
                .map(|t| t.is_word() && !t.ident_quoted() && is_clause_keyword(t.text))
                .unwrap_or(false)
            {
                // No table follows: MERGE's `WHEN MATCHED THEN UPDATE SET …`
                // puts a clause keyword right after UPDATE. Harvesting it
                // would ground `SET` as a table name and flag it unknown.
                return;
            } else if let Some((reference, span)) = self.parse_object_ref() {
                // After INTO/UPDATE/MERGE a `(` is a column list, so only the
                // FROM/JOIN/USING positions ask about function sources; MySQL's
                // `dual` is never a catalog table in either.
                let is_table = if allow_function_sources {
                    self.source_ref_kind(&reference) == SourceKind::Table
                } else {
                    !self.source_ref_is_mysql_dual(&reference)
                };
                let mut alias = None;
                if self.cur.eat_keyword("as") {
                    if let Some(t) = self.cur.peek().copied()
                        && t.is_word()
                    {
                        alias = Some(t.ident_text().to_string());
                        self.cur.advance();
                    }
                } else if let Some(t) = self.cur.peek().copied()
                    && matches!(t.kind, TokenKind::Ident(_))
                    && !is_clause_keyword(t.text)
                {
                    alias = Some(t.ident_text().to_string());
                    self.cur.advance();
                }
                if is_table {
                    self.references.push(SymbolUse {
                        reference: reference.clone(),
                        use_kind: UseKind::TableSource,
                        span,
                        via_alias: alias,
                        statement_index: 0,
                        in_predicate: false,
                    });
                }
            } else {
                return;
            }
            if self
                .cur
                .peek()
                .map(|t| t.kind == TokenKind::Comma)
                .unwrap_or(false)
            {
                self.cur.advance();
                continue;
            }
            return;
        }
    }
}
