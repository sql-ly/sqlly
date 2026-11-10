//! CREATE/ALTER definition statements: tables (columns, constraints,
//! grab-bag recovery), views, and routines with their parameter lists.

use crate::sqllex::{LexContext, RecoveryKind, TokenKind};
use crate::sqlmodel::{
    CheckDef, ColumnDef, ForeignKeyDef, KeyDef, ObjectKind, ObjectRef, ParamDef, ParamMode,
    RoutineBody, Span, SqlName, TableBody,
};

use super::StatementParser;
use super::support::{
    is_clause_keyword, is_keyword_arg_function, is_routine_body_start, looks_like_column_start,
    token_to_grab,
};
use crate::sqlparse::tree::{
    CreateRoutineNode, CreateTableNode, CreateViewNode, GrabBagToken, StatementNode, SymbolUse,
    UseKind,
};

impl<'c, 'a> StatementParser<'c, 'a> {
    // -- CREATE -------------------------------------------------------------

    pub(crate) fn parse_create(&mut self) -> Option<StatementNode> {
        let start = self.cur.peek().map(|t| t.span.start).unwrap_or(0);
        // Entered for either CREATE or ALTER; consume whichever leads.
        let is_alter = if self.cur.eat_keyword("create") {
            false
        } else {
            self.cur.eat_keyword("alter")
        };
        // OR REPLACE / OR ALTER
        if self.cur.peek().map(|t| t.keyword_eq("or")).unwrap_or(false) {
            self.cur.advance();
            self.cur.advance();
        }
        while self
            .cur
            .peek()
            .map(|t| {
                t.keyword_eq("temp")
                    || t.keyword_eq("temporary")
                    || t.keyword_eq("unique")
                    || t.keyword_eq("clustered")
                    || t.keyword_eq("nonclustered")
                    || t.keyword_eq("force")
            })
            .unwrap_or(false)
        {
            self.cur.advance();
        }

        let kind_word = self.cur.peek()?.text.to_ascii_lowercase();
        match kind_word.as_str() {
            // `ALTER TABLE` mutates an existing table; it is not a table
            // definition, so don't model it as one (its body is `ADD/DROP/…`,
            // not a column list). Leave it Opaque. `CREATE TABLE` structures.
            "table" if !is_alter => {
                self.cur.advance();
                if self.cur.eat_keyword("if") {
                    self.cur.eat_keyword("not");
                    self.cur.eat_keyword("exists");
                }
                self.parse_create_table(start)
            }
            "view" | "materialized" => {
                self.cur.advance();
                if kind_word == "materialized" {
                    self.cur.eat_keyword("view");
                }
                self.parse_create_view(start)
            }
            "procedure" | "proc" | "function" => {
                let kind = if kind_word == "function" {
                    ObjectKind::Function
                } else {
                    ObjectKind::Procedure
                };
                self.cur.advance();
                self.parse_create_routine(start, kind)
            }
            _ => {
                // CREATE/ALTER we don't structure: index, sequence, trigger,
                // ALTER TABLE, …
                let mut end = start;
                while let Some(t) = self.cur.advance() {
                    end = t.span.end;
                }
                let lead = if is_alter { "alter" } else { "create" };
                Some(StatementNode::Opaque {
                    span: self.span(Span::new(start, end)),
                    head: format!("{lead} {kind_word}"),
                })
            }
        }
    }

    pub(crate) fn parse_create_table(&mut self, start: usize) -> Option<StatementNode> {
        let (reference, _) = self.parse_object_ref()?;
        let mut body = TableBody::default();
        let mut grab_bag = Vec::new();
        let mut fk_spans: Vec<(ObjectRef, Span)> = Vec::new();

        if self.cur.eat_kind(TokenKind::LParen).is_some() {
            self.cur.push_context(LexContext::TableBody);
            loop {
                if self.cur.is_eof() {
                    self.cur.record(
                        RecoveryKind::UnbalancedParenClosed,
                        Span::new(start, start),
                        "table body closed at end of input",
                    );
                    break;
                }
                if self.cur.eat_kind(TokenKind::RParen).is_some() {
                    break;
                }
                self.parse_table_element(&mut body, &mut grab_bag, &mut fk_spans);
                if self
                    .cur
                    .peek()
                    .map(|t| t.kind == TokenKind::RParen)
                    .unwrap_or(false)
                    || self.cur.is_eof()
                {
                    continue;
                }
                if !self.cur.eat_separator() {
                    // Neither comma, newline-recovery, nor `)`: grab-bag until
                    // a separator or the end of the body.
                    let bag = self
                        .cur
                        .skip_to(&[TokenKind::Comma, TokenKind::RParen], &[]);
                    grab_bag.extend(bag.iter().map(|t| token_to_grab(t)));
                    self.cur.eat_kind(TokenKind::Comma);
                }
            }
            self.cur.pop_context();
        }

        for (target, span) in fk_spans {
            self.references.push(SymbolUse {
                reference: target,
                use_kind: UseKind::ForeignKeyTarget,
                span,
                via_alias: None,
                statement_index: 0,
                in_predicate: false,
            });
        }

        let end = self.consume_rest();
        Some(StatementNode::CreateTable(CreateTableNode {
            reference,
            body,
            span: self.span(Span::new(start, end)),
            grab_bag,
        }))
    }

    pub(crate) fn parse_table_element(
        &mut self,
        body: &mut TableBody,
        grab_bag: &mut Vec<GrabBagToken>,
        fk_spans: &mut Vec<(ObjectRef, Span)>,
    ) {
        // Table-level constraint?
        let head = match self.cur.peek() {
            Some(t) => t.text.to_ascii_lowercase(),
            None => return,
        };
        match head.as_str() {
            "constraint" | "primary" | "foreign" | "unique" | "check" => {
                self.parse_table_constraint(body, grab_bag, fk_spans);
                return;
            }
            _ => {}
        }

        // Column definition: name [type] [modifiers…]
        let name_tok = match self.cur.peek() {
            Some(t) if t.is_word() => *t,
            _ => {
                let bag = self
                    .cur
                    .skip_to(&[TokenKind::Comma, TokenKind::RParen], &[]);
                grab_bag.extend(bag.iter().map(|t| token_to_grab(t)));
                return;
            }
        };
        self.cur.advance();
        let name = SqlName::new(name_tok.ident_text(), self.dialect, name_tok.ident_quoted());

        self.cur.push_context(LexContext::ColumnDef);
        // A type only follows when no newline starts a plausible new column
        // (otherwise the line break is an assumed comma and this column has
        // no type).
        let next_is_new_column = self.cur.newline_before_next()
            && self
                .cur
                .peek()
                .map(|t| looks_like_column_start(t, self.cur.peek_n(1)))
                .unwrap_or(false);
        let ty = if next_is_new_column {
            None
        } else {
            self.parse_type()
        };
        if ty.is_none() {
            self.cur.record(
                RecoveryKind::MissingType,
                Span::new(name_tok.span.start, name_tok.span.end),
                format!(
                    "column `{}` has no parseable data type",
                    name_tok.ident_text()
                ),
            );
        }

        let mut col = ColumnDef {
            name,
            ty,
            nullable: None,
            default: None,
            identity: false,
            generated: None,
            collation: None,
            ordinal: body.columns.len(),
        };

        // Modifiers until separator/close.
        while let Some(&t) = self.cur.peek() {
            if matches!(t.kind, TokenKind::Comma | TokenKind::RParen) {
                break;
            }
            // Newline starting what looks like a new column = assumed comma;
            // leave it for eat_separator.
            if self.cur.newline_before_next()
                && t.is_word()
                && looks_like_column_start(&t, self.cur.peek_n(1))
            {
                break;
            }
            let word = t.text.to_ascii_lowercase();
            match word.as_str() {
                "not" => {
                    self.cur.advance();
                    if self.cur.eat_keyword("null") {
                        col.nullable = Some(false);
                    }
                }
                "null" => {
                    self.cur.advance();
                    col.nullable = Some(true);
                }
                "default" => {
                    self.cur.advance();
                    col.default = Some(self.capture_expression_text());
                }
                "identity" | "auto_increment" | "autoincrement" => {
                    self.cur.advance();
                    col.identity = true;
                    // IDENTITY(1,1)
                    if self
                        .cur
                        .peek()
                        .map(|t| t.kind == TokenKind::LParen)
                        .unwrap_or(false)
                    {
                        self.skip_balanced_parens();
                    }
                }
                "primary" => {
                    self.cur.advance();
                    self.cur.eat_keyword("key");
                    body.primary_key = Some(KeyDef {
                        name: None,
                        columns: vec![col.name.clone()],
                    });
                }
                "unique" => {
                    self.cur.advance();
                    body.unique_keys.push(KeyDef {
                        name: None,
                        columns: vec![col.name.clone()],
                    });
                }
                "references" => {
                    self.cur.advance();
                    if let Some((target, span)) = self.parse_object_ref() {
                        let target_columns = self.parse_paren_name_list().unwrap_or_default();
                        body.foreign_keys.push(ForeignKeyDef {
                            name: None,
                            columns: vec![col.name.clone()],
                            target: target.clone(),
                            target_columns,
                        });
                        fk_spans.push((target, span));
                    }
                }
                "check" => {
                    self.cur.advance();
                    let expr = self.capture_paren_text();
                    body.checks.push(CheckDef {
                        name: None,
                        expression: expr,
                    });
                }
                "collate" => {
                    self.cur.advance();
                    if let Some(t) = self.cur.advance() {
                        col.collation = Some(t.text.to_string());
                    }
                }
                "constraint" => {
                    // Named inline constraint: consume name, loop handles rest.
                    self.cur.advance();
                    self.cur.advance();
                }
                "generated" => {
                    self.cur.advance();
                    col.generated = Some(self.capture_expression_text());
                }
                _ => {
                    // Unknown modifier token: preserve and move on.
                    grab_bag.push(token_to_grab(&t));
                    self.cur.advance();
                }
            }
        }
        self.cur.pop_context();
        body.columns.push(col);
    }

    pub(crate) fn parse_table_constraint(
        &mut self,
        body: &mut TableBody,
        grab_bag: &mut Vec<GrabBagToken>,
        fk_spans: &mut Vec<(ObjectRef, Span)>,
    ) {
        let mut name = None;
        if self.cur.eat_keyword("constraint")
            && let Some(t) = self.cur.advance()
        {
            name = Some(SqlName::new(t.ident_text(), self.dialect, t.ident_quoted()));
        }
        let head = match self.cur.peek() {
            Some(t) => t.text.to_ascii_lowercase(),
            None => return,
        };
        match head.as_str() {
            "primary" => {
                self.cur.advance();
                self.cur.eat_keyword("key");
                while self
                    .cur
                    .peek()
                    .map(|t| t.keyword_eq("clustered") || t.keyword_eq("nonclustered"))
                    .unwrap_or(false)
                {
                    self.cur.advance();
                }
                let columns = self.parse_paren_name_list().unwrap_or_default();
                body.primary_key = Some(KeyDef { name, columns });
            }
            "unique" => {
                self.cur.advance();
                let columns = self.parse_paren_name_list().unwrap_or_default();
                body.unique_keys.push(KeyDef { name, columns });
            }
            "foreign" => {
                self.cur.advance();
                self.cur.eat_keyword("key");
                let columns = self.parse_paren_name_list().unwrap_or_default();
                if self.cur.eat_keyword("references")
                    && let Some((target, span)) = self.parse_object_ref()
                {
                    let target_columns = self.parse_paren_name_list().unwrap_or_default();
                    body.foreign_keys.push(ForeignKeyDef {
                        name,
                        columns,
                        target: target.clone(),
                        target_columns,
                    });
                    fk_spans.push((target, span));
                }
                // ON DELETE/UPDATE <action>
                while self.cur.peek().map(|t| t.keyword_eq("on")).unwrap_or(false) {
                    self.cur.advance();
                    self.cur.advance();
                    self.cur.advance();
                }
            }
            "check" => {
                self.cur.advance();
                let expr = self.capture_paren_text();
                body.checks.push(CheckDef {
                    name,
                    expression: expr,
                });
            }
            _ => {
                let bag = self
                    .cur
                    .skip_to(&[TokenKind::Comma, TokenKind::RParen], &[]);
                grab_bag.extend(bag.iter().map(|t| token_to_grab(t)));
            }
        }
    }

    pub(crate) fn parse_create_view(&mut self, start: usize) -> Option<StatementNode> {
        let (reference, _) = self.parse_object_ref()?;
        let mut definition = String::new();
        let mut end = start;
        while let Some(t) = self.cur.peek().copied() {
            definition.push_str(t.text);
            definition.push(' ');
            end = t.span.end;
            if t.is_word()
                && is_keyword_arg_function(t.text)
                && self
                    .cur
                    .peek_n(1)
                    .map(|n| n.kind == TokenKind::LParen)
                    .unwrap_or(false)
            {
                // EXTRACT/SUBSTRING/TRIM/OVERLAY: the FROM inside their
                // argument list is not a table source.
                self.cur.advance();
                let args = self.capture_paren_text();
                definition.push_str(&args);
                definition.push(' ');
                end = self.cur.prev_end();
                continue;
            }
            if t.keyword_eq("from") || t.keyword_eq("join") {
                self.cur.advance();
                // PG source modifiers: `FROM ONLY t`, `JOIN LATERAL (…)`.
                while self.cur.eat_keyword("only") || self.cur.eat_keyword("lateral") {}
                if let Some((r, span)) = self.parse_object_ref() {
                    let is_table = self.source_ref_kind(&r) == super::query::SourceKind::Table;
                    definition.push_str(&r.display());
                    definition.push(' ');
                    end = span.end;
                    // Capture the optional alias so `alias.`-dot and alias-
                    // qualified column completion work inside a view body, the
                    // same as they do inside a procedure body.
                    let mut alias = None;
                    if self.cur.eat_keyword("as") {
                        if let Some(n) = self.cur.peek().copied()
                            && n.is_word()
                        {
                            alias = Some(n.ident_text().to_string());
                            definition.push_str(n.text);
                            definition.push(' ');
                            self.cur.advance();
                        }
                    } else if let Some(n) = self.cur.peek().copied()
                        && matches!(n.kind, TokenKind::Ident(_))
                        && !is_clause_keyword(n.text)
                    {
                        alias = Some(n.ident_text().to_string());
                        definition.push_str(n.text);
                        definition.push(' ');
                        self.cur.advance();
                    }
                    if is_table {
                        self.references.push(SymbolUse {
                            reference: r,
                            use_kind: UseKind::TableSource,
                            span,
                            via_alias: alias,
                            statement_index: 0,
                            in_predicate: false,
                        });
                    }
                }
                continue;
            }
            self.cur.advance();
        }
        Some(StatementNode::CreateView(CreateViewNode {
            reference,
            span: self.span(Span::new(start, end)),
            definition: definition.trim().to_string(),
        }))
    }

    pub(crate) fn parse_create_routine(
        &mut self,
        start: usize,
        kind: ObjectKind,
    ) -> Option<StatementNode> {
        let (reference, _) = self.parse_object_ref()?;
        let mut body = RoutineBody::default();

        // Parameters. T-SQL: bare list "@a int, @b int". Others: "(a int, …)".
        let parenthesized = self.cur.eat_kind(TokenKind::LParen).is_some();
        self.cur.push_context(LexContext::ArgList);
        while let Some(&t) = self.cur.peek() {
            if parenthesized && t.kind == TokenKind::RParen {
                self.cur.advance();
                break;
            }
            if !parenthesized && is_routine_body_start(&t) {
                break;
            }
            let is_param_start = matches!(t.kind, TokenKind::ParamMarker(_)) || t.is_word();
            if !is_param_start {
                self.cur.advance();
                continue;
            }
            // Mode prefix (pg/mysql/oracle): IN/OUT/INOUT before the name.
            let mut mode = ParamMode::In;
            let mut t = t;
            loop {
                let w = t.text.to_ascii_lowercase();
                let is_mode = matches!(w.as_str(), "in" | "out" | "inout")
                    // …but only when another word follows (else it's the name).
                    && self.cur.peek_n(1).map(|n| n.is_word()).unwrap_or(false);
                if is_mode {
                    mode = match w.as_str() {
                        "out" => ParamMode::Out,
                        "inout" => ParamMode::InOut,
                        _ => ParamMode::In,
                    };
                    self.cur.advance();
                    t = match self.cur.peek() {
                        Some(n) => *n,
                        None => break,
                    };
                } else {
                    break;
                }
            }
            let name_text = match t.kind {
                TokenKind::ParamMarker(_) => t.text.to_string(),
                _ if t.is_word() => t.ident_text().to_string(),
                _ => {
                    self.cur.advance();
                    continue;
                }
            };
            self.cur.advance();
            let ty = self.parse_type();
            let mut default = None;
            // Suffix modifiers: = default, OUTPUT/OUT, DEFAULT x
            while let Some(&t) = self.cur.peek() {
                if t.kind == TokenKind::Comma || (parenthesized && t.kind == TokenKind::RParen) {
                    break;
                }
                if !parenthesized && is_routine_body_start(&t) {
                    break;
                }
                let w = t.text.to_ascii_lowercase();
                if t.kind == TokenKind::Operator && t.text == "=" {
                    self.cur.advance();
                    default = Some(self.capture_expression_text());
                    continue;
                }
                match w.as_str() {
                    "output" | "out" => {
                        mode = ParamMode::Out;
                        self.cur.advance();
                    }
                    "default" => {
                        self.cur.advance();
                        default = Some(self.capture_expression_text());
                    }
                    _ => {
                        self.cur.advance();
                    }
                }
            }
            body.params.push(ParamDef {
                name: SqlName::new(name_text, self.dialect, false),
                ty,
                mode,
                default,
            });
            if !self.cur.eat_separator() {
                if parenthesized {
                    if self.cur.eat_kind(TokenKind::RParen).is_some() {
                        break;
                    }
                    // Malformed: avoid stalling.
                    if self.cur.advance().is_none() {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.cur.pop_context();

        // RETURNS <type>. A table-shaped return (`RETURNS TABLE`, PG
        // `RETURNS SETOF`, T-SQL `RETURNS @t TABLE (…)`) marks a
        // table-valued function; other functions keep the generic kind.
        let mut kind = kind;
        if self.cur.eat_keyword("returns") || self.cur.eat_keyword("return") {
            if kind == ObjectKind::Function
                && self.cur.peek().is_some_and(|t| {
                    (t.is_word()
                        && (t.text.eq_ignore_ascii_case("table")
                            || t.text.eq_ignore_ascii_case("setof")))
                        || matches!(t.kind, TokenKind::ParamMarker(_))
                })
            {
                kind = ObjectKind::TableFunction;
            }
            body.returns = self.parse_type();
        }
        if self.cur.eat_keyword("language")
            && let Some(t) = self.cur.advance()
        {
            body.language = Some(t.text.to_string());
        }

        let end = self.harvest_routine_body();
        Some(StatementNode::CreateRoutine(CreateRoutineNode {
            reference,
            kind,
            body,
            span: self.span(Span::new(start, end)),
        }))
    }
}
