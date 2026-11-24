//! Reads the predicate text engines print in plans (PostgreSQL `Filter`,
//! SQL Server `Predicate`, Oracle `filter(...)`, MySQL `attached_condition`)
//! just far enough to tell which columns are compared, how, and whether the
//! column is wrapped in something that stops an index seek.
//!
//! This is not a SQL parser. Anything it does not understand becomes a
//! [`TermKind::Other`] term with no column, and the rules then say less
//! rather than guess.

use crate::plans::PlanDialect;

/// A column as the plan names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnRef {
    /// Alias or table in front of the column, when the plan printed one.
    pub qualifier: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wrap {
    /// `lower(col)`, `YEAR(col)`, `date(col)`.
    Function(String),
    /// `CAST(col AS t)`, `(col)::t`, `CONVERT_IMPLICIT(t, col, 0)`.
    Cast { ty: String, implicit: bool },
    /// `col + 1`, `col * 2`.
    Arithmetic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Side {
    Column(ColumnRef),
    Wrapped {
        column: ColumnRef,
        wrap: Wrap,
        /// The wrapping expression exactly as the plan printed it.
        text: String,
    },
    /// Literals, parameters and expressions with no column in them.
    Constant(String),
    Other,
}

impl Side {
    pub fn column(&self) -> Option<&ColumnRef> {
        match self {
            Side::Column(column) | Side::Wrapped { column, .. } => Some(column),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TermKind {
    Equality,
    Range,
    NotEqual,
    Like,
    In,
    IsNull,
    IsNotNull,
    /// A parenthesised `a OR b` group: no single index key serves it.
    Or,
    Other,
}

/// One top-level `AND` conjunct of a predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Term {
    pub kind: TermKind,
    pub left: Side,
    pub right: Side,
    /// The conjunct as printed.
    pub text: String,
    /// `LIKE` pattern literal (without quotes), when the right side is one.
    pub pattern: Option<String>,
    /// The comparison is negated (`NOT LIKE`, `NOT IN`).
    pub negated: bool,
}

impl Term {
    /// True for `LIKE '%x'` and `LIKE '_x'`: the pattern has no fixed prefix.
    pub fn leading_wildcard(&self) -> bool {
        self.kind == TermKind::Like
            && self
                .pattern
                .as_deref()
                .is_some_and(|pattern| pattern.starts_with('%') || pattern.starts_with('_'))
    }

    /// `col <op> constant` with the bare column on one side.
    pub fn bare_column(&self) -> Option<&ColumnRef> {
        match (&self.left, &self.right) {
            (Side::Column(column), Side::Constant(_))
            | (Side::Constant(_), Side::Column(column)) => Some(column),
            (Side::Column(column), Side::Other)
                if matches!(
                    self.kind,
                    TermKind::In | TermKind::IsNull | TermKind::IsNotNull
                ) =>
            {
                Some(column)
            }
            _ => None,
        }
    }

    /// `col = col`: both sides are bare columns.
    pub fn column_pair(&self) -> Option<(&ColumnRef, &ColumnRef)> {
        match (&self.left, &self.right) {
            (Side::Column(left), Side::Column(right)) if self.kind == TermKind::Equality => {
                Some((left, right))
            }
            _ => None,
        }
    }

    /// The wrapped side, when a column is wrapped and compared to a constant
    /// (or to anything, for a join).
    pub fn wrapped(&self) -> Option<(&ColumnRef, &Wrap, &str)> {
        for side in [&self.left, &self.right] {
            if let Side::Wrapped { column, wrap, text } = side {
                return Some((column, wrap, text.as_str()));
            }
        }
        None
    }
}

/// Split a predicate into its top-level `AND` conjuncts.
pub(crate) fn parse_predicate(text: &str, dialect: Option<PlanDialect>) -> Vec<Term> {
    let tokens = lex(text, dialect);
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut parser = Parser {
        text,
        tokens: &tokens,
        position: 0,
        dialect,
        depth: 0,
    };
    let Some(node) = parser.parse_or() else {
        return vec![other_term(text)];
    };
    if parser.position < tokens.len() {
        return vec![other_term(text)];
    }
    let mut terms = Vec::new();
    flatten(node, &mut terms);
    terms
}

fn other_term(text: &str) -> Term {
    Term {
        kind: TermKind::Other,
        left: Side::Other,
        right: Side::Other,
        text: text.trim().to_string(),
        pattern: None,
        negated: false,
    }
}

fn flatten(node: Bool, out: &mut Vec<Term>) {
    match node {
        Bool::And(items) => {
            for item in items {
                flatten(item, out);
            }
        }
        Bool::Term(term) => out.push(*term),
    }
}

enum Bool {
    And(Vec<Bool>),
    Term(Box<Term>),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Identifier chain part; `quoted` keeps `"select"` from being a keyword.
    Ident {
        text: String,
        quoted: bool,
    },
    Str(String),
    Num,
    Param,
    Op(String),
    LParen,
    RParen,
    Comma,
    Dot,
    Cast,
    /// MySQL `<cache>`, `<in_optimizer>`, `<exists>` markers.
    Marker(String),
    Other,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

fn lex(text: &str, dialect: Option<PlanDialect>) -> Vec<Token> {
    let bytes = text.as_bytes();
    let brackets_quote = matches!(dialect, Some(PlanDialect::SqlServer) | None);
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let tok = match c {
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b'.' if !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                Tok::Dot
            }
            b'\'' => {
                let (value, next) = read_quoted(text, i, b'\'');
                i = next;
                Tok::Str(value)
            }
            b'"' => {
                let (value, next) = read_quoted(text, i, b'"');
                i = next;
                Tok::Ident {
                    text: value,
                    quoted: true,
                }
            }
            b'`' => {
                let (value, next) = read_quoted(text, i, b'`');
                i = next;
                Tok::Ident {
                    text: value,
                    quoted: true,
                }
            }
            b'[' if brackets_quote => {
                let close = text[i + 1..].find(']').map(|offset| i + 1 + offset);
                match close {
                    Some(close) => {
                        let inner = &text[i + 1..close];
                        i = close + 1;
                        if inner.starts_with('@') {
                            Tok::Param
                        } else {
                            Tok::Ident {
                                text: inner.to_string(),
                                quoted: true,
                            }
                        }
                    }
                    None => {
                        i += 1;
                        Tok::Other
                    }
                }
            }
            b':' if bytes.get(i + 1) == Some(&b':') => {
                i += 2;
                Tok::Cast
            }
            b'@' | b'$' | b'?' | b':' => {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                Tok::Param
            }
            b'<' if marker_end(text, i).is_some() => {
                let end = marker_end(text, i).unwrap_or(i + 1);
                let name = text[i + 1..end - 1].to_string();
                i = end;
                Tok::Marker(name)
            }
            b'=' | b'<' | b'>' | b'!' | b'~' | b'+' | b'-' | b'*' | b'/' | b'|' | b'%' | b'&'
            | b'^' | b'#' => {
                i += 1;
                while i < bytes.len() && matches!(bytes[i], b'=' | b'<' | b'>' | b'~' | b'*' | b'|')
                {
                    // `--` never appears; keep `-` and `+` single so `a - -1` lexes.
                    i += 1;
                }
                Tok::Op(text[start..i].to_string())
            }
            _ if c.is_ascii_digit()
                || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.' || bytes[i] == b'_')
                {
                    i += 1;
                }
                Tok::Num
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'_'
                        || bytes[i] == b'$'
                        || bytes[i] == b'#'
                        || bytes[i] >= 0x80)
                {
                    i += 1;
                }
                let word = &text[start..i];
                // N'unicode literal', E'escape literal', B'bits', X'hex'.
                if word.len() == 1
                    && matches!(word, "N" | "n" | "E" | "e" | "B" | "b" | "X" | "x")
                    && bytes.get(i) == Some(&b'\'')
                {
                    let (value, next) = read_quoted(text, i, b'\'');
                    i = next;
                    Tok::Str(value)
                } else {
                    Tok::Ident {
                        text: word.to_string(),
                        quoted: false,
                    }
                }
            }
            _ => {
                i += text[i..].chars().next().map_or(1, char::len_utf8);
                Tok::Other
            }
        };
        tokens.push(Token { tok, start, end: i });
    }
    tokens
}

/// `<cache>` style marker: `<` + lowercase word + `>`.
fn marker_end(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start + 1..];
    let length = rest
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
        .count();
    (length > 1 && rest.as_bytes().get(length) == Some(&b'>')).then_some(start + 1 + length + 1)
}

/// Read a quoted run starting at `start` (which holds the quote). A doubled
/// quote is an escaped quote. Returns the unescaped text and the index after
/// the closing quote.
fn read_quoted(text: &str, start: usize, quote: u8) -> (String, usize) {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = start + 1;
    let mut segment = i;
    while i < bytes.len() {
        if bytes[i] == quote {
            out.push_str(&text[segment..i]);
            if bytes.get(i + 1) == Some(&quote) {
                out.push(quote as char);
                i += 2;
                segment = i;
                continue;
            }
            return (out, i + 1);
        }
        i += 1;
    }
    out.push_str(&text[segment..]);
    (out, bytes.len())
}

#[derive(Debug, Clone)]
enum Expr {
    Column(ColumnRef),
    Constant,
    Str(String),
    Func {
        name: String,
        args: Vec<Expr>,
    },
    Cast {
        inner: Box<Expr>,
        ty: String,
        implicit: bool,
    },
    Arith(Vec<Expr>),
    Unknown,
}

struct Spanned {
    expr: Expr,
    start: usize,
    end: usize,
}

struct Parser<'a> {
    text: &'a str,
    tokens: &'a [Token],
    position: usize,
    dialect: Option<PlanDialect>,
    depth: usize,
}

const MAX_DEPTH: usize = 64;

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.position).map(|token| &token.tok)
    }

    fn peek_at(&self, offset: usize) -> Option<&Tok> {
        self.tokens
            .get(self.position + offset)
            .map(|token| &token.tok)
    }

    fn keyword(&self, word: &str) -> bool {
        self.keyword_at(0, word)
    }

    fn keyword_at(&self, offset: usize, word: &str) -> bool {
        matches!(
            self.peek_at(offset),
            Some(Tok::Ident { text, quoted: false }) if text.eq_ignore_ascii_case(word)
        )
    }

    fn start_of(&self, position: usize) -> usize {
        self.tokens
            .get(position)
            .map_or(self.text.len(), |token| token.start)
    }

    fn end_of_previous(&self) -> usize {
        self.position
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index))
            .map_or(0, |token| token.end)
    }

    fn parse_or(&mut self) -> Option<Bool> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let start_position = self.position;
        let first = self.parse_and()?;
        let mut is_or = false;
        while self.keyword("OR") {
            is_or = true;
            self.position += 1;
            self.parse_and()?;
        }
        self.depth -= 1;
        if is_or {
            let start = self.start_of(start_position);
            let end = self.end_of_previous();
            Some(Bool::Term(Box::new(Term {
                kind: TermKind::Or,
                left: Side::Other,
                right: Side::Other,
                text: self.text[start..end].trim().to_string(),
                pattern: None,
                negated: false,
            })))
        } else {
            Some(first)
        }
    }

    fn parse_and(&mut self) -> Option<Bool> {
        let mut items = vec![self.parse_not()?];
        while self.keyword("AND") {
            self.position += 1;
            items.push(self.parse_not()?);
        }
        if items.len() == 1 {
            items.pop()
        } else {
            Some(Bool::And(items))
        }
    }

    fn parse_not(&mut self) -> Option<Bool> {
        if self.keyword("NOT") {
            let start = self.start_of(self.position);
            self.position += 1;
            self.parse_not()?;
            let end = self.end_of_previous();
            return Some(Bool::Term(Box::new(other_term(&self.text[start..end]))));
        }
        self.parse_comparison()
    }

    /// A comparison, or a parenthesised boolean group.
    fn parse_comparison(&mut self) -> Option<Bool> {
        // A parenthesis may open a boolean group or an operand. Try the group
        // first and fall back when what follows the group is an operator.
        if self.peek() == Some(&Tok::LParen) {
            let saved = self.position;
            self.position += 1;
            if let Some(inner) = self.parse_or()
                && self.peek() == Some(&Tok::RParen)
            {
                self.position += 1;
                if !self.continues_operand() {
                    return Some(inner);
                }
            }
            self.position = saved;
        }
        let start = self.start_of(self.position);
        let left = self.parse_operand()?;
        let mut negated = false;
        if self.keyword("NOT") {
            negated = true;
            self.position += 1;
        }
        let (kind, right, pattern) = match self.peek().cloned() {
            Some(Tok::Op(op)) => {
                let kind = match op.as_str() {
                    "=" | "==" => TermKind::Equality,
                    "<" | ">" | "<=" | ">=" => TermKind::Range,
                    "<>" | "!=" => TermKind::NotEqual,
                    "~~" | "~~*" => TermKind::Like,
                    "!~~" | "!~~*" => {
                        negated = true;
                        TermKind::Like
                    }
                    _ => TermKind::Other,
                };
                self.position += 1;
                // `= ANY (...)` / `= ALL (...)`.
                let any = self.keyword("ANY") || self.keyword("SOME") || self.keyword("ALL");
                if any {
                    self.position += 1;
                }
                let right = self.parse_operand()?;
                let pattern = literal_text(&right.expr);
                let kind = if any && kind == TermKind::Equality {
                    TermKind::In
                } else {
                    kind
                };
                (kind, Some(right), pattern)
            }
            Some(Tok::Ident {
                text,
                quoted: false,
            }) => {
                let upper = text.to_ascii_uppercase();
                match upper.as_str() {
                    "LIKE" | "ILIKE" => {
                        self.position += 1;
                        let right = self.parse_operand()?;
                        if self.keyword("ESCAPE") {
                            self.position += 1;
                            self.parse_operand()?;
                        }
                        let pattern = literal_text(&right.expr);
                        (TermKind::Like, Some(right), pattern)
                    }
                    "IN" => {
                        self.position += 1;
                        self.parse_operand()?;
                        (TermKind::In, None, None)
                    }
                    "BETWEEN" => {
                        self.position += 1;
                        let low = self.parse_operand()?;
                        if !self.keyword("AND") {
                            return None;
                        }
                        self.position += 1;
                        let high = self.parse_operand()?;
                        let constant = matches!(self.side(&low), Side::Constant(_))
                            && matches!(self.side(&high), Side::Constant(_));
                        let right = Spanned {
                            expr: if constant {
                                Expr::Constant
                            } else {
                                Expr::Unknown
                            },
                            start: low.start,
                            end: high.end,
                        };
                        (TermKind::Range, Some(right), None)
                    }
                    "IS" => {
                        self.position += 1;
                        let not = self.keyword("NOT");
                        if not {
                            self.position += 1;
                        }
                        if !self.keyword("NULL") {
                            // IS [NOT] TRUE / DISTINCT FROM ...: not modelled.
                            return None;
                        }
                        self.position += 1;
                        let kind = if not {
                            TermKind::IsNotNull
                        } else {
                            TermKind::IsNull
                        };
                        (kind, None, None)
                    }
                    _ => (TermKind::Other, None, None),
                }
            }
            _ => (TermKind::Other, None, None),
        };
        let end = self.end_of_previous();
        let left_side = self.side(&left);
        let right_side = right.as_ref().map_or(Side::Other, |right| self.side(right));
        Some(Bool::Term(Box::new(Term {
            kind,
            left: left_side,
            right: right_side,
            text: self.text[start..end].trim().to_string(),
            pattern,
            negated,
        })))
    }

    /// True when the token after a closed parenthesis shows the parenthesis
    /// was an operand (`(a + b) > 3`, `(col)::text = ...`), not a group.
    fn continues_operand(&self) -> bool {
        match self.peek() {
            Some(Tok::Op(_) | Tok::Cast) => true,
            Some(Tok::Ident {
                text,
                quoted: false,
            }) => matches!(
                text.to_ascii_uppercase().as_str(),
                "LIKE" | "ILIKE" | "IN" | "BETWEEN" | "IS" | "NOT"
            ),
            _ => false,
        }
    }

    fn parse_operand(&mut self) -> Option<Spanned> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let first = self.parse_unary()?;
        let start = first.start;
        let mut parts = vec![first.expr];
        let mut end = first.end;
        while let Some(Tok::Op(op)) = self.peek() {
            if !matches!(
                op.as_str(),
                "+" | "-" | "*" | "/" | "||" | "%" | "&" | "|" | "^" | "#"
            ) {
                break;
            }
            self.position += 1;
            let next = self.parse_unary()?;
            end = next.end;
            parts.push(next.expr);
        }
        self.depth -= 1;
        let expr = if parts.len() == 1 {
            parts.pop()?
        } else {
            Expr::Arith(parts)
        };
        Some(Spanned { expr, start, end })
    }

    fn parse_unary(&mut self) -> Option<Spanned> {
        if let Some(Tok::Op(op)) = self.peek()
            && matches!(op.as_str(), "-" | "+")
        {
            let start = self.start_of(self.position);
            self.position += 1;
            let inner = self.parse_unary()?;
            return Some(Spanned {
                expr: match inner.expr {
                    Expr::Constant => Expr::Constant,
                    other => Expr::Arith(vec![other]),
                },
                start,
                end: inner.end,
            });
        }
        let mut value = self.parse_primary()?;
        // Postfix casts: `x::type`, `x::character varying(20)`, `x::int[]`.
        while self.peek() == Some(&Tok::Cast) {
            self.position += 1;
            let type_start = self.start_of(self.position);
            let mut words = 0;
            while let Some(Tok::Ident { .. }) = self.peek() {
                self.position += 1;
                words += 1;
                if self.peek() == Some(&Tok::Dot) {
                    self.position += 1;
                }
            }
            if words == 0 {
                return None;
            }
            if self.peek() == Some(&Tok::LParen) {
                self.skip_parenthesised()?;
            }
            while matches!(self.peek(), Some(Tok::Other)) {
                // `[]` array suffix (PostgreSQL only; brackets are not quotes there).
                self.position += 1;
            }
            let end = self.end_of_previous();
            let ty = self.text[type_start..end].trim().to_string();
            value = Spanned {
                start: value.start,
                end,
                expr: match value.expr {
                    Expr::Constant | Expr::Str(_) => match value.expr {
                        Expr::Str(text) => Expr::Str(text),
                        _ => Expr::Constant,
                    },
                    inner => Expr::Cast {
                        inner: Box::new(inner),
                        ty,
                        implicit: false,
                    },
                },
            };
        }
        Some(value)
    }

    fn skip_parenthesised(&mut self) -> Option<()> {
        if self.peek() != Some(&Tok::LParen) {
            return None;
        }
        let mut depth = 0usize;
        while let Some(tok) = self.peek() {
            match tok {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        self.position += 1;
                        return Some(());
                    }
                }
                _ => {}
            }
            self.position += 1;
        }
        None
    }

    fn parse_primary(&mut self) -> Option<Spanned> {
        let start = self.start_of(self.position);
        let tok = self.peek()?.clone();
        match tok {
            Tok::LParen => {
                self.position += 1;
                // A subquery or a value list: keep it opaque.
                if self.keyword("SELECT") || self.keyword("WITH") {
                    self.position -= 1;
                    self.skip_parenthesised()?;
                    return Some(Spanned {
                        expr: Expr::Unknown,
                        start,
                        end: self.end_of_previous(),
                    });
                }
                let first = self.parse_operand()?;
                if self.peek() == Some(&Tok::Comma) {
                    // (a, b, c): a value list.
                    let mut constant = matches!(first.expr, Expr::Constant | Expr::Str(_));
                    while self.peek() == Some(&Tok::Comma) {
                        self.position += 1;
                        let item = self.parse_operand()?;
                        constant &= matches!(item.expr, Expr::Constant | Expr::Str(_));
                    }
                    if self.peek() != Some(&Tok::RParen) {
                        return None;
                    }
                    self.position += 1;
                    return Some(Spanned {
                        expr: if constant {
                            Expr::Constant
                        } else {
                            Expr::Unknown
                        },
                        start,
                        end: self.end_of_previous(),
                    });
                }
                if self.peek() != Some(&Tok::RParen) {
                    return None;
                }
                self.position += 1;
                Some(Spanned {
                    expr: first.expr,
                    start,
                    end: self.end_of_previous(),
                })
            }
            Tok::Str(text) => {
                self.position += 1;
                Some(Spanned {
                    expr: Expr::Str(text),
                    start,
                    end: self.end_of_previous(),
                })
            }
            Tok::Num | Tok::Param => {
                self.position += 1;
                Some(Spanned {
                    expr: Expr::Constant,
                    start,
                    end: self.end_of_previous(),
                })
            }
            Tok::Marker(_) => {
                // <cache>(expr): MySQL evaluated this once; it is a constant.
                self.position += 1;
                if self.peek() == Some(&Tok::LParen) {
                    self.skip_parenthesised()?;
                }
                Some(Spanned {
                    expr: Expr::Constant,
                    start,
                    end: self.end_of_previous(),
                })
            }
            Tok::Ident { text, quoted } => self.parse_identifier(text, quoted, start),
            _ => None,
        }
    }

    fn parse_identifier(&mut self, first: String, quoted: bool, start: usize) -> Option<Spanned> {
        self.position += 1;
        let upper = first.to_ascii_uppercase();
        if !quoted {
            match upper.as_str() {
                "NULL" | "TRUE" | "FALSE" | "CURRENT_DATE" | "CURRENT_TIMESTAMP"
                | "CURRENT_TIME" | "SYSDATE" | "SYSTIMESTAMP" | "LOCALTIMESTAMP"
                | "CURRENT_USER"
                    if self.peek() != Some(&Tok::Dot) && self.peek() != Some(&Tok::LParen) =>
                {
                    return Some(Spanned {
                        expr: Expr::Constant,
                        start,
                        end: self.end_of_previous(),
                    });
                }
                "INTERVAL" | "DATE" | "TIMESTAMP" | "TIME"
                    if matches!(self.peek(), Some(Tok::Str(_) | Tok::Num)) =>
                {
                    // INTERVAL '1' DAY, DATE '2024-01-01', interval 1 day.
                    self.position += 1;
                    if let Some(Tok::Ident {
                        quoted: false,
                        text,
                    }) = self.peek()
                        && is_interval_unit(text)
                    {
                        self.position += 1;
                    }
                    return Some(Spanned {
                        expr: Expr::Constant,
                        start,
                        end: self.end_of_previous(),
                    });
                }
                "CAST" | "TRY_CAST" if self.peek() == Some(&Tok::LParen) => {
                    return self.parse_cast_call(start);
                }
                "ARRAY" if matches!(self.peek(), Some(Tok::Other)) => {
                    while matches!(
                        self.peek(),
                        Some(Tok::Other | Tok::Num | Tok::Str(_) | Tok::Comma | Tok::Param)
                    ) {
                        self.position += 1;
                    }
                    return Some(Spanned {
                        expr: Expr::Constant,
                        start,
                        end: self.end_of_previous(),
                    });
                }
                _ => {}
            }
        }
        let mut parts = vec![first];
        while self.peek() == Some(&Tok::Dot) {
            match self.peek_at(1) {
                Some(Tok::Ident { text, .. }) => {
                    parts.push(text.clone());
                    self.position += 2;
                }
                Some(Tok::Op(op)) if op == "*" => {
                    self.position += 2;
                    return Some(Spanned {
                        expr: Expr::Unknown,
                        start,
                        end: self.end_of_previous(),
                    });
                }
                _ => return None,
            }
        }
        if self.peek() == Some(&Tok::LParen) {
            let name = parts.join(".");
            let args = self.parse_arguments()?;
            return Some(Spanned {
                expr: function_expr(name, args),
                start,
                end: self.end_of_previous(),
            });
        }
        // SQL Server prints `[db].[schema].[table].[col] as [alias].[col]`.
        if self.keyword("AS")
            && matches!(self.peek_at(1), Some(Tok::Ident { .. }))
            && self
                .dialect
                .is_none_or(|dialect| dialect == PlanDialect::SqlServer)
        {
            self.position += 1;
            let mut alias_parts = Vec::new();
            while let Some(Tok::Ident { text, .. }) = self.peek() {
                alias_parts.push(text.clone());
                self.position += 1;
                if self.peek() == Some(&Tok::Dot) {
                    self.position += 1;
                } else {
                    break;
                }
            }
            if alias_parts.len() >= 2 {
                parts = alias_parts;
            }
        }
        let name = parts.pop()?;
        let qualifier = parts.pop();
        Some(Spanned {
            expr: Expr::Column(ColumnRef { qualifier, name }),
            start,
            end: self.end_of_previous(),
        })
    }

    fn parse_cast_call(&mut self, start: usize) -> Option<Spanned> {
        // At `(` after CAST.
        self.position += 1;
        let inner = self.parse_operand()?;
        if !self.keyword("AS") {
            return None;
        }
        self.position += 1;
        let type_start = self.start_of(self.position);
        let mut depth = 0usize;
        loop {
            match self.peek()? {
                Tok::LParen => depth += 1,
                Tok::RParen if depth == 0 => break,
                Tok::RParen => depth -= 1,
                _ => {}
            }
            self.position += 1;
        }
        let ty = self.text[type_start..self.end_of_previous()]
            .trim()
            .to_string();
        self.position += 1;
        Some(Spanned {
            expr: match inner.expr {
                Expr::Constant | Expr::Str(_) => Expr::Constant,
                other => Expr::Cast {
                    inner: Box::new(other),
                    ty,
                    implicit: false,
                },
            },
            start,
            end: self.end_of_previous(),
        })
    }

    fn parse_arguments(&mut self) -> Option<Vec<Expr>> {
        // At `(`.
        self.position += 1;
        let mut args = Vec::new();
        if self.peek() == Some(&Tok::RParen) {
            self.position += 1;
            return Some(args);
        }
        loop {
            if matches!(self.peek(), Some(Tok::Op(op)) if op == "*") {
                self.position += 1;
                args.push(Expr::Unknown);
            } else if self.keyword("SELECT") {
                return None;
            } else {
                let saved = self.position;
                match self.parse_operand() {
                    Some(arg) => args.push(arg.expr),
                    None => {
                        self.position = saved;
                        return None;
                    }
                }
                // Type or keyword arguments: `CONVERT(varchar(10), x)`,
                // `EXTRACT(year FROM x)`, `x AT TIME ZONE 'UTC'`.
                while let Some(Tok::Ident { quoted: false, .. }) = self.peek() {
                    self.position += 1;
                    if let Some(extra) = self.parse_operand() {
                        args.push(extra.expr);
                    }
                }
            }
            match self.peek()? {
                Tok::Comma => self.position += 1,
                Tok::RParen => {
                    self.position += 1;
                    return Some(args);
                }
                _ => return None,
            }
        }
    }

    fn side(&self, spanned: &Spanned) -> Side {
        let text = self.text[spanned.start..spanned.end].trim().to_string();
        classify(&spanned.expr, text, self.dialect)
    }
}

fn is_interval_unit(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "SECOND" | "MINUTE" | "HOUR" | "DAY" | "WEEK" | "MONTH" | "QUARTER" | "YEAR"
    )
}

fn literal_text(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Str(text) => Some(text.clone()),
        _ => None,
    }
}

fn function_expr(name: String, mut args: Vec<Expr>) -> Expr {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        // CONVERT_IMPLICIT(type, expr, style) / CONVERT(type, expr[, style]).
        "CONVERT_IMPLICIT" | "CONVERT" | "TRY_CONVERT" if args.len() >= 2 => {
            let ty = match &args[0] {
                Expr::Column(column) => column.name.clone(),
                Expr::Func { name, .. } => name.clone(),
                _ => String::new(),
            };
            let inner = args.swap_remove(1);
            match inner {
                Expr::Constant | Expr::Str(_) => Expr::Constant,
                inner => Expr::Cast {
                    inner: Box::new(inner),
                    ty,
                    implicit: upper == "CONVERT_IMPLICIT",
                },
            }
        }
        // The first argument is a date part keyword, not a column:
        // `datepart(year, col)`, `dateadd(day, 1, col)`.
        "DATEPART" | "DATENAME" | "DATEADD" | "DATEDIFF" | "DATEDIFF_BIG" | "DATETRUNC"
            if args.len() >= 2
                && matches!(&args[0], Expr::Column(column) if column.qualifier.is_none()) =>
        {
            args[0] = Expr::Constant;
            Expr::Func { name, args }
        }
        _ => Expr::Func { name, args },
    }
}

/// Columns anywhere inside an expression.
fn columns_in<'a>(expr: &'a Expr, out: &mut Vec<&'a ColumnRef>) {
    match expr {
        Expr::Column(column) => out.push(column),
        Expr::Func { args, .. } | Expr::Arith(args) => {
            for arg in args {
                columns_in(arg, out);
            }
        }
        Expr::Cast { inner, .. } => columns_in(inner, out),
        Expr::Constant | Expr::Str(_) | Expr::Unknown => {}
    }
}

fn has_unknown(expr: &Expr) -> bool {
    match expr {
        Expr::Unknown => true,
        Expr::Func { args, .. } | Expr::Arith(args) => args.iter().any(has_unknown),
        Expr::Cast { inner, .. } => has_unknown(inner),
        _ => false,
    }
}

/// PostgreSQL prints a binary-compatible relabel on `varchar` columns
/// (`(status)::text = 'x'::text`); it does not stop an index being used.
fn is_relabel_cast(ty: &str, dialect: Option<PlanDialect>) -> bool {
    if !matches!(
        dialect,
        Some(PlanDialect::Postgres | PlanDialect::DuckDb) | None
    ) {
        return false;
    }
    let ty = ty.to_ascii_lowercase();
    ty == "text"
        || ty == "bpchar"
        || ty.starts_with("character varying")
        || ty == "varchar"
        || ty == "name"
}

fn classify(expr: &Expr, text: String, dialect: Option<PlanDialect>) -> Side {
    match expr {
        Expr::Column(column) => Side::Column(column.clone()),
        Expr::Constant | Expr::Str(_) => Side::Constant(text),
        Expr::Unknown => Side::Other,
        Expr::Cast {
            inner,
            ty,
            implicit,
        } => {
            if let Expr::Column(column) = inner.as_ref() {
                if !implicit && is_relabel_cast(ty, dialect) {
                    return Side::Column(column.clone());
                }
                return Side::Wrapped {
                    column: column.clone(),
                    wrap: Wrap::Cast {
                        ty: ty.clone(),
                        implicit: *implicit,
                    },
                    text,
                };
            }
            // A cast around something else: classify what is inside.
            match classify(inner, text.clone(), dialect) {
                Side::Wrapped { column, wrap, .. } => Side::Wrapped { column, wrap, text },
                Side::Constant(_) => Side::Constant(text),
                other => other,
            }
        }
        Expr::Func { name, args } => {
            let mut columns = Vec::new();
            columns_in(expr, &mut columns);
            if columns.is_empty() {
                return if has_unknown(expr) {
                    Side::Other
                } else {
                    Side::Constant(text)
                };
            }
            if columns.len() == 1 && !has_unknown(expr) {
                // Oracle shows implicit conversions as TO_NUMBER("C") or
                // INTERNAL_FUNCTION("C").
                let upper = name.to_ascii_uppercase();
                let wrap = if upper == "INTERNAL_FUNCTION"
                    || (matches!(dialect, Some(PlanDialect::Oracle))
                        && matches!(upper.as_str(), "TO_NUMBER" | "TO_CHAR" | "TO_NCHAR")
                        && args.len() == 1)
                {
                    Wrap::Cast {
                        ty: upper,
                        implicit: true,
                    }
                } else {
                    Wrap::Function(name.clone())
                };
                return Side::Wrapped {
                    column: columns[0].clone(),
                    wrap,
                    text,
                };
            }
            Side::Other
        }
        Expr::Arith(parts) => {
            let mut columns = Vec::new();
            for part in parts {
                columns_in(part, &mut columns);
            }
            if columns.is_empty() {
                if parts.iter().any(has_unknown) {
                    Side::Other
                } else {
                    Side::Constant(text)
                }
            } else if columns.len() == 1 && !parts.iter().any(has_unknown) {
                Side::Wrapped {
                    column: columns[0].clone(),
                    wrap: Wrap::Arithmetic,
                    text,
                }
            } else {
                Side::Other
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str, dialect: PlanDialect) -> Vec<Term> {
        parse_predicate(text, Some(dialect))
    }

    fn column(term: &Term) -> String {
        let column = term.bare_column().expect("bare column");
        match &column.qualifier {
            Some(qualifier) => format!("{qualifier}.{}", column.name),
            None => column.name.clone(),
        }
    }

    #[test]
    fn postgres_filter_with_relabel_casts() {
        let terms = terms(
            "(((status)::text = 'refunded'::text) AND (total > '1200'::numeric))",
            PlanDialect::Postgres,
        );
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].kind, TermKind::Equality);
        assert_eq!(column(&terms[0]), "status");
        assert_eq!(terms[1].kind, TermKind::Range);
        assert_eq!(column(&terms[1]), "total");
    }

    #[test]
    fn postgres_function_wrapped_column() {
        let terms = terms(
            "(lower(email) = 'user4242@example.com'::text)",
            PlanDialect::Postgres,
        );
        assert_eq!(terms.len(), 1);
        let (column, wrap, text) = terms[0].wrapped().expect("wrapped");
        assert_eq!(column.name, "email");
        assert_eq!(wrap, &Wrap::Function("lower".to_string()));
        assert_eq!(text, "lower(email)");
        assert!(terms[0].bare_column().is_none());
    }

    #[test]
    fn postgres_real_cast_any_and_now() {
        let terms = terms(
            "(((created_at)::date = '2024-01-01'::date) AND (id = ANY ('{1,2,3}'::integer[])) AND (seen_at >= (now() - '1 day'::interval)))",
            PlanDialect::Postgres,
        );
        assert_eq!(terms.len(), 3);
        let (column, wrap, _) = terms[0].wrapped().expect("cast");
        assert_eq!(column.name, "created_at");
        assert_eq!(
            wrap,
            &Wrap::Cast {
                ty: "date".to_string(),
                implicit: false
            }
        );
        assert_eq!(terms[1].kind, TermKind::In);
        assert_eq!(column_name(&terms[1]), "id");
        assert_eq!(terms[2].kind, TermKind::Range);
        assert_eq!(super::tests::column(&terms[2]), "seen_at");
    }

    fn column_name(term: &Term) -> String {
        term.left.column().expect("left column").name.clone()
    }

    #[test]
    fn postgres_like_patterns_and_join_condition() {
        let leading = terms("(name ~~ '%smith'::text)", PlanDialect::Postgres);
        assert!(leading[0].leading_wildcard());
        let prefix = terms("(name ~~ 'smith%'::text)", PlanDialect::Postgres);
        assert_eq!(prefix[0].kind, TermKind::Like);
        assert!(!prefix[0].leading_wildcard());
        let join = terms("(o.customer_id = c.id)", PlanDialect::Postgres);
        let (left, right) = join[0].column_pair().expect("pair");
        assert_eq!(left.qualifier.as_deref(), Some("o"));
        assert_eq!(left.name, "customer_id");
        assert_eq!(right.name, "id");
    }

    #[test]
    fn sql_server_brackets_aliases_and_implicit_conversion() {
        let terms = terms(
            "[Shop].[dbo].[Orders].[OrderDate] as [o].[OrderDate]>=[@since] AND [Shop].[dbo].[Orders].[Status]=N'refunded'",
            PlanDialect::SqlServer,
        );
        assert_eq!(terms.len(), 2);
        assert_eq!(column(&terms[0]), "o.OrderDate");
        assert_eq!(terms[0].kind, TermKind::Range);
        assert_eq!(column(&terms[1]), "Orders.Status");

        let convert = super::parse_predicate(
            "CONVERT_IMPLICIT(nvarchar(200),[c].[Email],0)=[@email]",
            Some(PlanDialect::SqlServer),
        );
        let (column, wrap, text) = convert[0].wrapped().expect("convert");
        assert_eq!(column.name, "Email");
        assert_eq!(column.qualifier.as_deref(), Some("c"));
        assert_eq!(
            wrap,
            &Wrap::Cast {
                ty: "nvarchar".to_string(),
                implicit: true
            }
        );
        assert_eq!(text, "CONVERT_IMPLICIT(nvarchar(200),[c].[Email],0)");
    }

    #[test]
    fn mysql_backticks_and_cache_markers() {
        let terms = terms(
            "((`shop`.`o`.`status` = 'refunded') and (`shop`.`o`.`total` > 1200.00) and (`shop`.`o`.`created` > <cache>((now() - interval 1 day))))",
            PlanDialect::MySql,
        );
        assert_eq!(terms.len(), 3);
        assert_eq!(column(&terms[0]), "o.status");
        assert_eq!(terms[1].kind, TermKind::Range);
        assert_eq!(column(&terms[2]), "o.created");
    }

    #[test]
    fn oracle_quoted_names_binds_and_conversions() {
        let terms = terms(
            "(\"O\".\"TOTAL\">1200 AND \"O\".\"STATUS\"='refunded' AND TO_NUMBER(\"O\".\"REF\")=:B1)",
            PlanDialect::Oracle,
        );
        assert_eq!(terms.len(), 3);
        assert_eq!(column(&terms[0]), "O.TOTAL");
        assert_eq!(column(&terms[1]), "O.STATUS");
        let (column, wrap, _) = terms[2].wrapped().expect("to_number");
        assert_eq!(column.name, "REF");
        assert!(matches!(wrap, Wrap::Cast { implicit: true, .. }));
    }

    #[test]
    fn or_groups_between_and_unreadable_text() {
        let terms = terms(
            "((a = 1) OR (b = 2)) AND c BETWEEN 1 AND 5 AND d IS NULL",
            PlanDialect::Postgres,
        );
        assert_eq!(terms.len(), 3);
        assert_eq!(terms[0].kind, TermKind::Or);
        assert_eq!(terms[1].kind, TermKind::Range);
        assert_eq!(column(&terms[1]), "c");
        assert_eq!(terms[2].kind, TermKind::IsNull);

        let odd = super::parse_predicate("a = = ) (", Some(PlanDialect::Postgres));
        assert_eq!(odd.len(), 1);
        assert_eq!(odd[0].kind, TermKind::Other);
        assert!(super::parse_predicate("", None).is_empty());
    }

    #[test]
    fn constants_on_the_left_and_functions_of_constants() {
        let terms = terms(
            "5 < qty AND created_at > date_trunc('day', now()) AND YEAR(o.created_at) = 2024",
            PlanDialect::Postgres,
        );
        assert_eq!(column(&terms[0]), "qty");
        assert_eq!(column(&terms[1]), "created_at");
        let (column, wrap, text) = terms[2].wrapped().expect("year");
        assert_eq!(column.name, "created_at");
        assert_eq!(wrap, &Wrap::Function("YEAR".to_string()));
        assert_eq!(text, "YEAR(o.created_at)");
    }

    #[test]
    fn sql_server_date_part_keyword_is_not_a_column() {
        let terms = terms(
            "datepart(year,[Shop].[dbo].[Orders].[OrderDate])=(2024)",
            PlanDialect::SqlServer,
        );
        let (column, wrap, _) = terms[0].wrapped().expect("datepart");
        assert_eq!(column.name, "OrderDate");
        assert_eq!(wrap, &Wrap::Function("datepart".to_string()));
    }
}
