//! Slot resolution (ADR-010 generalized, ADR-011 §5.4): walk a statement's
//! token stream and bind typed value targets — procedure arguments, INSERT
//! values, UPDATE SET assignments, WHERE predicate comparisons — to the
//! columns/parameters they feed. The validator and value-mode completion both
//! consume [`Slot`]s; this module only *finds* them, it does not type-check.

use crate::sqllex::{NumberShape, StrictLexer, StringShape, Token, TokenKind};
use crate::sqlmodel::{Dialect, ObjectRef, Span, SqlName};
use crate::sqlparse::{ExecNode, InsertNode, ParsedTree, StatementNode, TableSource, content_hash};
use serde::{Deserialize, Serialize};

/// What kind of typed target a slot is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotKind {
    RoutineArg,     // EXEC p @a = <v> / CALL p(<v>)
    InsertValue,    // INSERT … VALUES (<v>)
    UpdateAssign,   // UPDATE t SET col = <v>
    Predicate,      // WHERE col <op> <v>
    VariableAssign, // DECLARE @x int = <v> / SET @x = <v>
}

/// Shape of the literal found in a slot (lexical, pre-typecheck).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiteralShape {
    Integer,
    Decimal,
    Scientific,
    Hex,
    String { national: bool },
    Null,
    Word,      // bare identifier/keyword (TRUE, GETDATE, column ref…)
    Parameter, // bind variable — never validated
    Other,
    Missing, // slot exists but no value yet (cursor position)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotValue {
    pub shape: LiteralShape,
    pub text: String,
    pub span: Span,
}

impl SlotValue {
    pub fn empty() -> Self {
        SlotValue {
            shape: LiteralShape::Missing,
            text: String::new(),
            span: Span::new(0, 0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.shape == LiteralShape::Missing && self.text.is_empty()
    }

    /// Unquoted text for string literals; raw text otherwise.
    pub fn unquoted(&self) -> String {
        match self.shape {
            LiteralShape::String { national } => {
                let t = if national {
                    &self.text[1..]
                } else {
                    &self.text[..]
                };
                let t = t.trim_start_matches('\'').trim_end_matches('\'');
                t.replace("''", "'")
            }
            _ => self.text.clone(),
        }
    }
}

/// A resolved typed target near/around a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub kind: SlotKind,
    /// Table (or routine) the slot belongs to, as written in the SQL.
    pub container: ObjectRef,
    /// Alias the container was introduced with, if any.
    pub container_alias: Option<String>,
    /// Column name / parameter name / positional index display.
    pub member: SlotMember,
    /// Comparison operator for predicates ("=", ">", "like", …).
    pub operator: Option<String>,
    pub value: SlotValue,
    /// Span of the member reference (for related-info diagnostics).
    pub member_span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotMember {
    Named(SqlName),
    Positional(usize),
}

impl SlotMember {
    pub fn display(&self) -> String {
        match self {
            SlotMember::Named(n) => n.spelling().to_string(),
            SlotMember::Positional(i) => format!("argument {}", i + 1),
        }
    }
}

/// Resolve all slots in a document. Statement structures provide routine and
/// INSERT slots; predicate/assignment slots come from a token-level scan of
/// each statement (which tolerates broken surroundings).
pub fn resolve_slots(text: &str, dialect: Dialect) -> Vec<Slot> {
    let tree = crate::sqlparse::parse(text, dialect);
    resolve_slots_in_tree(&tree, text, dialect)
}

pub fn resolve_slots_in_tree(tree: &ParsedTree, text: &str, dialect: Dialect) -> Vec<Slot> {
    let mut slots = Vec::new();
    let tokens = TokenSource::new(text, dialect);

    for stmt in &tree.statements {
        match stmt {
            StatementNode::Exec(node) => exec_slots(node, &mut slots),
            StatementNode::Insert(node) => {
                insert_slots(node, &tokens.statement_tokens(node.span), &mut slots);
            }
            StatementNode::Update(node) => {
                let sources = vec![node.target.clone()];
                scan_predicates(
                    &tokens.statement_tokens(node.span),
                    dialect,
                    node.span,
                    &sources,
                    &mut slots,
                    true,
                );
            }
            StatementNode::Select(node) | StatementNode::Delete(node) => {
                scan_predicates(
                    &tokens.statement_tokens(node.span),
                    dialect,
                    node.span,
                    &node.tables,
                    &mut slots,
                    false,
                );
            }
            _ => {}
        }
    }
    slots
}

/// Token supply for slot scanning. For dialects on the single-lex pipeline the
/// document is lexed once and statements borrow index ranges of it — the same
/// tokens the parser consumed, so tree spans and slot spans can never drift.
/// MySQL keeps per-statement lexing (mirroring `parse`'s `DELIMITER`-aware
/// split path), re-based to document offsets.
struct TokenSource<'a> {
    text: &'a str,
    dialect: Dialect,
    /// Non-trivia document tokens, absolute spans; `None` for MySQL.
    doc: Option<Vec<Token<'a>>>,
}

impl<'a> TokenSource<'a> {
    fn new(text: &'a str, dialect: Dialect) -> Self {
        let doc = (dialect != Dialect::MySql).then(|| {
            StrictLexer::tokenize(text, dialect)
                .into_iter()
                .filter(|t| !t.is_trivia())
                .collect()
        });
        Self { text, dialect, doc }
    }

    /// Non-trivia tokens of the statement covering `span`, spans absolute.
    fn statement_tokens(&self, span: Span) -> Vec<Token<'a>> {
        match &self.doc {
            Some(tokens) => {
                let start = tokens.partition_point(|t| t.span.start < span.start);
                tokens[start..]
                    .iter()
                    .take_while(|t| t.span.end <= span.end)
                    .copied()
                    .collect()
            }
            None => StrictLexer::tokenize(slice(self.text, span), self.dialect)
                .into_iter()
                .filter(|t| !t.is_trivia())
                .map(|mut t| {
                    t.span = Span::new(t.span.start + span.start, t.span.end + span.start);
                    t
                })
                .collect(),
        }
    }
}

fn exec_slots(node: &ExecNode, out: &mut Vec<Slot>) {
    for (i, (name, value)) in node.args.iter().enumerate() {
        out.push(Slot {
            kind: SlotKind::RoutineArg,
            container: node.target.clone(),
            container_alias: None,
            member: match name {
                Some(n) => SlotMember::Named(SqlName::new(n.clone(), Dialect::SqlServer, false)),
                None => SlotMember::Positional(i),
            },
            operator: None,
            value: value.clone(),
            member_span: value.span,
        });
    }
}

fn insert_slots(node: &InsertNode, tokens: &[Token<'_>], out: &mut Vec<Slot>) {
    // Re-scan the statement's VALUES region to get per-expression values.
    // (The parser recorded arities; for slot typing we need the literals.)

    // Find VALUES, then walk each parenthesized row.
    let values_idx = match tokens.iter().position(|t| t.keyword_eq("values")) {
        Some(i) => i,
        None => return,
    };
    let mut i = values_idx + 1;
    while i < tokens.len() && tokens[i].kind == TokenKind::LParen {
        i += 1;
        let mut position = 0usize;
        let mut depth = 0usize;
        let mut current: Option<SlotValue> = None;
        let mut complex = false; // expression more complex than one literal
        while i < tokens.len() {
            let t = &tokens[i];
            match t.kind {
                TokenKind::LParen => {
                    depth += 1;
                    complex = true;
                }
                TokenKind::RParen if depth > 0 => depth -= 1,
                TokenKind::RParen => {
                    flush_insert_slot(node, position, &mut current, complex, out);
                    i += 1;
                    break;
                }
                TokenKind::Comma if depth == 0 => {
                    flush_insert_slot(node, position, &mut current, complex, out);
                    position += 1;
                    complex = false;
                }
                _ => {
                    let v = token_to_slot_value(t);
                    if current.is_none() {
                        current = Some(v);
                    } else {
                        complex = true;
                    }
                }
            }
            i += 1;
        }
        // Next row?
        if i < tokens.len() && tokens[i].kind == TokenKind::Comma {
            i += 1;
        } else {
            break;
        }
    }
}

fn flush_insert_slot(
    node: &InsertNode,
    position: usize,
    current: &mut Option<SlotValue>,
    complex: bool,
    out: &mut Vec<Slot>,
) {
    let value = current.take().unwrap_or_else(SlotValue::empty);
    // Conservative (ADR-010): only single-literal expressions are validated.
    if complex {
        return;
    }
    let member = match node.columns.get(position) {
        Some((name, span)) => (SlotMember::Named(name.clone()), *span),
        None => (SlotMember::Positional(position), value.span),
    };
    out.push(Slot {
        kind: SlotKind::InsertValue,
        container: node.target.reference.clone(),
        container_alias: None,
        member: member.0,
        operator: None,
        value,
        member_span: member.1,
    });
}

/// Scan a statement's tokens for `[alias.]column <op> <value>` patterns in
/// WHERE/SET clauses. Token-level so it works on partially broken statements.
fn scan_predicates(
    tokens: &[Token<'_>],
    dialect: Dialect,
    span: Span,
    sources: &[TableSource],
    out: &mut Vec<Slot>,
    include_set: bool,
) {
    // Region tracking: only bind slots after WHERE (predicates), ON (join
    // predicates), or SET (assignments when include_set).
    let mut region: Option<SlotKind> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.keyword_eq("where") || t.keyword_eq("on") || t.keyword_eq("having") {
            region = Some(SlotKind::Predicate);
            i += 1;
            continue;
        }
        if t.keyword_eq("set") {
            region = include_set.then_some(SlotKind::UpdateAssign);
            i += 1;
            continue;
        }
        if t.keyword_eq("group") || t.keyword_eq("order") || t.keyword_eq("limit") {
            region = None;
            i += 1;
            continue;
        }
        let Some(kind) = region else {
            i += 1;
            continue;
        };

        // Pattern: word [. word] op value
        if t.is_word() && !is_predicate_noise(t) {
            let (member_tok, alias, consumed) = if i + 2 < tokens.len()
                && tokens[i + 1].kind == TokenKind::Dot
                && tokens[i + 2].is_word()
            {
                (&tokens[i + 2], Some(t.ident_text().to_string()), 3)
            } else {
                (t, None, 1)
            };
            let op_idx = i + consumed;
            let (operator, op_len) = match tokens.get(op_idx) {
                Some(o) if o.kind == TokenKind::Operator => (Some(o.text.to_string()), 1),
                Some(o) if o.keyword_eq("like") => (Some("like".to_string()), 1),
                Some(o) if o.keyword_eq("between") => (Some("between".to_string()), 1),
                Some(o) if o.keyword_eq("in") => (Some("in".to_string()), 1),
                Some(o) if o.keyword_eq("is") => (Some("is".to_string()), 1),
                _ => (None, 0),
            };
            if let Some(op) = operator {
                // Value token (skip NOT after IS/LIKE).
                let mut v_idx = op_idx + op_len;
                if tokens
                    .get(v_idx)
                    .map(|t| t.keyword_eq("not"))
                    .unwrap_or(false)
                {
                    v_idx += 1;
                }
                let value = match tokens.get(v_idx) {
                    Some(vt) if is_value_token(vt) => token_to_slot_value(vt),
                    Some(vt)
                        if vt.kind == TokenKind::Operator && (vt.text == "-" || vt.text == "+") =>
                    {
                        match tokens.get(v_idx + 1) {
                            Some(n) if matches!(n.kind, TokenKind::Number(_)) => {
                                let mut v = token_to_slot_value(n);
                                v.text = format!("{}{}", vt.text, n.text);
                                v.span = Span::new(vt.span.start, n.span.end);
                                v
                            }
                            _ => SlotValue::empty(),
                        }
                    }
                    _ => {
                        // No value yet — still a slot (cursor target for
                        // completion).
                        let mut v = SlotValue::empty();
                        let after = tokens.get(op_idx).map(|o| o.span.end).unwrap_or(span.end);
                        v.span = Span::new(after, after);
                        v
                    }
                };

                // Bind the alias to a source table.
                let (container, container_alias) = bind_source(sources, alias.as_deref());
                if let Some(container) = container {
                    out.push(Slot {
                        kind,
                        container,
                        container_alias,
                        member: SlotMember::Named(SqlName::new(
                            member_tok.ident_text(),
                            dialect,
                            member_tok.ident_quoted(),
                        )),
                        operator: Some(op),
                        value,
                        member_span: member_tok.span,
                    });
                }
                i = v_idx + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Map an alias (or None for unqualified) to a table source.
fn bind_source(
    sources: &[TableSource],
    alias: Option<&str>,
) -> (Option<ObjectRef>, Option<String>) {
    match alias {
        Some(a) => {
            for s in sources {
                if s.alias
                    .as_deref()
                    .map(|al| al.eq_ignore_ascii_case(a))
                    .unwrap_or(false)
                    || s.reference.name.spelling().eq_ignore_ascii_case(a)
                {
                    return (Some(s.reference.clone()), s.alias.clone());
                }
            }
            (None, None)
        }
        None => {
            // Unqualified: bind only when there is exactly one source
            // (conservative — ambiguity produces no slot).
            if sources.len() == 1 {
                (Some(sources[0].reference.clone()), sources[0].alias.clone())
            } else {
                (None, None)
            }
        }
    }
}

fn is_predicate_noise(t: &Token<'_>) -> bool {
    t.keyword_eq("and")
        || t.keyword_eq("or")
        || t.keyword_eq("not")
        || t.keyword_eq("null")
        || t.keyword_eq("exists")
        || t.keyword_eq("case")
        || t.keyword_eq("when")
        || t.keyword_eq("then")
        || t.keyword_eq("else")
        || t.keyword_eq("end")
}

fn is_value_token(t: &Token<'_>) -> bool {
    matches!(
        t.kind,
        TokenKind::Number(_) | TokenKind::StringLit(_) | TokenKind::ParamMarker(_)
    ) || t.keyword_eq("null")
}

fn token_to_slot_value(t: &Token<'_>) -> SlotValue {
    let span = t.span;
    let shape = match t.kind {
        TokenKind::Number(NumberShape::Integer) => LiteralShape::Integer,
        TokenKind::Number(NumberShape::Decimal) => LiteralShape::Decimal,
        TokenKind::Number(NumberShape::Scientific) => LiteralShape::Scientific,
        TokenKind::Number(NumberShape::Hex) => LiteralShape::Hex,
        TokenKind::StringLit(s) => LiteralShape::String {
            national: s == StringShape::National,
        },
        TokenKind::ParamMarker(_) => LiteralShape::Parameter,
        TokenKind::Keyword if t.keyword_eq("null") => LiteralShape::Null,
        _ if t.is_word() => LiteralShape::Word,
        _ => LiteralShape::Other,
    };
    SlotValue {
        shape,
        text: t.text.to_string(),
        span,
    }
}

fn slice(text: &str, span: Span) -> &str {
    let start = span.start.min(text.len());
    let end = span.end.min(text.len()).max(start);
    &text[start..end]
}

/// Find the slot whose value region contains `offset` (cursor position) —
/// the entry point for value-mode completion.
pub fn slot_at(text: &str, dialect: Dialect, offset: usize) -> Option<Slot> {
    let slots = resolve_slots(text, dialect);
    // Prefer a slot whose value span contains the offset; else one whose
    // value region starts at/just before the offset (empty value).
    slots
        .iter()
        .find(|s| !s.value.is_empty() && s.value.span.contains(offset))
        .or_else(|| {
            slots
                .iter()
                .filter(|s| s.value.span.start <= offset)
                .min_by_key(|s| offset - s.value.span.start)
        })
        .cloned()
}

// Used by engine cache keys.
pub fn document_key(text: &str) -> u64 {
    content_hash(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> Dialect {
        Dialect::SqlServer
    }

    #[test]
    fn predicate_slot_with_alias() {
        let slots = resolve_slots("select * from Person p where p.name = 'aa'", d());
        assert_eq!(slots.len(), 1, "{slots:?}");
        let s = &slots[0];
        assert_eq!(s.kind, SlotKind::Predicate);
        assert_eq!(s.container.display(), "Person");
        assert_eq!(s.member.display(), "name");
        assert_eq!(s.operator.as_deref(), Some("="));
        assert!(matches!(s.value.shape, LiteralShape::String { .. }));
        assert_eq!(s.value.unquoted(), "aa");
    }

    #[test]
    fn predicate_slot_unqualified_single_table() {
        let slots = resolve_slots("select * from Person where age > 21", d());
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].member.display(), "age");
        assert_eq!(slots[0].operator.as_deref(), Some(">"));
        assert_eq!(slots[0].value.shape, LiteralShape::Integer);
    }

    #[test]
    fn predicate_slot_empty_value_is_cursor_target() {
        let sql = "select * from Person p where p.name = ";
        let slots = resolve_slots(sql, d());
        assert_eq!(slots.len(), 1);
        assert!(slots[0].value.is_empty());
        // slot_at finds it from the cursor at end of text.
        let s = slot_at(sql, d(), sql.len()).expect("slot at cursor");
        assert_eq!(s.member.display(), "name");
    }

    #[test]
    fn update_set_assignment_slot() {
        let slots = resolve_slots("update Person set age = 'x' where id = 3", d());
        let kinds: Vec<SlotKind> = slots.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SlotKind::UpdateAssign), "{slots:?}");
        assert!(kinds.contains(&SlotKind::Predicate));
        let assign = slots
            .iter()
            .find(|s| s.kind == SlotKind::UpdateAssign)
            .unwrap();
        assert_eq!(assign.member.display(), "age");
        assert!(matches!(assign.value.shape, LiteralShape::String { .. }));
    }

    #[test]
    fn insert_value_slots_named_columns() {
        let slots = resolve_slots("insert into T (A, B) values (1, 'x')", d());
        let ins: Vec<&Slot> = slots
            .iter()
            .filter(|s| s.kind == SlotKind::InsertValue)
            .collect();
        assert_eq!(ins.len(), 2, "{slots:?}");
        assert_eq!(ins[0].member.display(), "A");
        assert_eq!(ins[0].value.shape, LiteralShape::Integer);
        assert_eq!(ins[1].member.display(), "B");
        assert!(matches!(ins[1].value.shape, LiteralShape::String { .. }));
    }

    #[test]
    fn exec_arg_slots() {
        let slots = resolve_slots("EXEC dbo.P @BatchId = 'abc', 42", d());
        let args: Vec<&Slot> = slots
            .iter()
            .filter(|s| s.kind == SlotKind::RoutineArg)
            .collect();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].member.display(), "@BatchId");
        assert_eq!(args[1].member.display(), "argument 2");
    }

    #[test]
    fn ambiguous_unqualified_column_produces_no_slot() {
        let slots = resolve_slots("select * from A join B on A.x = B.x where y = 1", d());
        // y is unqualified with two sources → no slot for it; the join ON
        // comparison is column-to-column (Word value) which is fine.
        assert!(
            !slots.iter().any(|s| s.member.display() == "y"),
            "ambiguous column must not bind: {slots:?}"
        );
    }

    #[test]
    fn slot_binds_in_second_unterminated_statement() {
        // Two SELECTs with no terminator between them: the second statement's
        // unqualified predicate must bind to ITS table, not be dropped as
        // ambiguous because the first statement's table leaked into scope.
        let sql = "SELECT * FROM Inventory.ItemCategory\n\n\nSELECT * FROM Accounting.Account where Name like ";
        let s = slot_at(sql, d(), sql.len()).expect("slot at cursor");
        assert_eq!(s.container.display(), "Accounting.Account");
        assert_eq!(s.member.display(), "Name");
        assert_eq!(s.operator.as_deref(), Some("like"));
        assert!(s.value.is_empty());
    }

    #[test]
    fn between_and_like_operators() {
        let slots = resolve_slots("select * from T where a between 1 and 5", d());
        assert_eq!(slots[0].operator.as_deref(), Some("between"));
        let slots = resolve_slots("select * from T where name like '%x%'", d());
        assert_eq!(slots[0].operator.as_deref(), Some("like"));
    }
}
