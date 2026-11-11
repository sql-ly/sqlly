//! # sqlly-sqllex
//!
//! Token vocabulary and the two lexers of ADR-011: [`StrictLexer`] (full
//! fidelity, error tokens, never aborts) and the recovering layer used by
//! `sqlly-sqlparse` via [`TokenCursor`] contexts. Dialect differences live in
//! data tables ([`dialect_tables`]), not code branches. No I/O.

#![forbid(unsafe_code)]

use crate::sqlmodel::{Dialect, Span};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

pub mod dialect_tables;
pub mod oracle_script;
pub mod splitter;
pub mod splitter_stream;
pub use splitter::{split_batches, split_statement_tokens, split_statements};
pub use splitter_stream::{
    ScriptUnit, StreamSplitter, strip_leading_comments, strip_leading_comments_in,
};

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// Strips one level of identifier quoting (`[..]`, `".."`, or `` `..` ``)
/// and collapses the doubled closing quote that escapes it inside, so
/// `[a]]b]` → `a]b`, `"a""b"` → `a"b`, `` `a``b` `` → ``a`b``. Text that
/// isn't wrapped in a matching quote pair is returned unchanged (after
/// trimming surrounding whitespace).
pub fn unquote_identifier(raw: &str) -> Cow<'_, str> {
    let s = raw.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return Cow::Borrowed(s);
    }
    let (close, doubled) = match (bytes[0], bytes[bytes.len() - 1]) {
        (b'[', b']') => ("]", "]]"),
        (b'"', b'"') => ("\"", "\"\""),
        (b'`', b'`') => ("`", "``"),
        _ => return Cow::Borrowed(s),
    };
    let inner = &s[1..s.len() - 1];
    if inner.contains(doubled) {
        Cow::Owned(inner.replace(doubled, close))
    } else {
        Cow::Borrowed(inner)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentQuote {
    None,
    Bracket,     // [x]      (SQL Server, SQLite)
    DoubleQuote, // "x"      (standard, pg/oracle/sqlite; mysql w/ ANSI_QUOTES)
    Backtick,    // `x`      (MySQL, SQLite)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumberShape {
    Integer,
    Decimal,
    Scientific,
    Hex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StringShape {
    Plain,        // 'x'
    National,     // N'x'
    Escape,       // E'x' (pg)
    OracleQ,      // q'{x}'
    DollarQuoted, // $tag$x$tag$ (pg)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommentShape {
    Line,  // -- … or # … (mysql)
    Block, // /* … */
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamStyle {
    AtName,    // @p (mssql)
    ColonName, // :p (oracle)
    Question,  // ?  (odbc/mysql/sqlite)
    DollarNum, // $1 (pg)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LexErrorKind {
    UnterminatedString,
    UnterminatedQuotedIdent,
    UnterminatedBlockComment,
    IllegalCharacter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenKind {
    Ident(IdentQuote),
    /// Word matching the dialect's keyword table. The raw text is preserved;
    /// parsers compare via [`Token::keyword_eq`].
    Keyword,
    Number(NumberShape),
    StringLit(StringShape),
    Operator,
    Comma,
    Semicolon,
    LParen,
    RParen,
    Dot,
    ParamMarker(ParamStyle),
    Comment(CommentShape),
    Whitespace,
    Error(LexErrorKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokenKind,
    pub span: Span,
    pub text: &'a str,
}

impl<'a> Token<'a> {
    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokenKind::Whitespace | TokenKind::Comment(_))
    }

    pub fn keyword_eq(&self, word: &str) -> bool {
        matches!(self.kind, TokenKind::Keyword) && self.text.eq_ignore_ascii_case(word)
    }

    pub fn is_word(&self) -> bool {
        matches!(self.kind, TokenKind::Keyword | TokenKind::Ident(_))
    }

    /// Identifier value: quotes stripped and doubled closing quotes
    /// unescaped (`[a]]b]` → `a]b`, `"a""b"` → `a"b`). Borrows unless an
    /// escape had to be collapsed.
    pub fn ident_text(&self) -> Cow<'a, str> {
        match self.kind {
            TokenKind::Ident(IdentQuote::None) | TokenKind::Keyword => Cow::Borrowed(self.text),
            TokenKind::Ident(_) => unquote_identifier(self.text),
            _ => Cow::Borrowed(self.text),
        }
    }

    pub fn ident_quoted(&self) -> bool {
        matches!(
            self.kind,
            TokenKind::Ident(IdentQuote::Bracket)
                | TokenKind::Ident(IdentQuote::DoubleQuote)
                | TokenKind::Ident(IdentQuote::Backtick)
        )
    }
}

// ---------------------------------------------------------------------------
// Strict lexer
// ---------------------------------------------------------------------------

/// Single forward pass over the input. Lexical problems become
/// `TokenKind::Error` tokens with spans; the stream always reaches EOF.
pub struct StrictLexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    dialect: Dialect,
}

impl<'a> StrictLexer<'a> {
    pub fn new(src: &'a str, dialect: Dialect) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            dialect,
        }
    }

    pub fn tokenize(src: &'a str, dialect: Dialect) -> Vec<Token<'a>> {
        let mut lexer = Self::new(src, dialect);
        let mut out = Vec::new();
        while let Some(t) = lexer.next_token() {
            out.push(t);
        }
        out
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.bytes.get(self.pos + off).copied()
    }

    fn token(&self, kind: TokenKind, start: usize) -> Token<'a> {
        Token {
            kind,
            span: Span::new(start, self.pos),
            text: &self.src[start..self.pos],
        }
    }

    fn next_token(&mut self) -> Option<Token<'a>> {
        let start = self.pos;
        let c = self.peek()?;

        // Whitespace
        if c.is_ascii_whitespace() {
            while self
                .peek()
                .map(|b| b.is_ascii_whitespace())
                .unwrap_or(false)
            {
                self.pos += 1;
            }
            return Some(self.token(TokenKind::Whitespace, start));
        }

        // Comments, string literals, dollar quotes and quoted identifiers:
        // where each begins and ends is the security lexer's decision
        // (`seclex`), so the editor, the splitter and the safety gates share
        // one boundary model per engine.
        if (c == b'N' || c == b'n')
            && self.peek_at(1) == Some(b'\'')
            && self.dialect == Dialect::SqlServer
            && let Some(con) =
                crate::seclex::editor_construct_at(self.bytes, start + 1, self.dialect)
        {
            // T-SQL `N'…'`: same boundaries as a plain literal.
            return Some(self.token_from_construct(con, start, Some(StringShape::National)));
        }
        if let Some(con) = crate::seclex::editor_construct_at(self.bytes, start, self.dialect) {
            return Some(self.token_from_construct(con, start, None));
        }

        // T-SQL `#temp` / `##global` temp tables and `$action` /
        // `$partition` pseudo-columns are names.
        if self.dialect == Dialect::SqlServer {
            let prefix = match c {
                b'#' if self.peek_at(1) == Some(b'#') => 2,
                b'#' => 1,
                b'$' if self.peek_at(1).is_some_and(is_ident_start) => 1,
                _ => 0,
            };
            if prefix > 0 && self.peek_at(prefix).is_some_and(is_tsql_name_continue) {
                self.pos += prefix;
                while self.peek().is_some_and(is_tsql_name_continue) {
                    self.pos += 1;
                }
                return Some(self.token(TokenKind::Ident(IdentQuote::None), start));
            }
        }

        // Parameter markers. `@name` is a T-SQL variable, a MySQL / MariaDB
        // user (`@total`) or system (`@@sql_mode`) variable — and a BigQuery
        // query parameter, BigQuery being read with the MySQL rules — and an
        // Oracle database link (`orders@remote_db`).
        if c == b'@'
            && (self.dialect == Dialect::SqlServer
                || (matches!(self.dialect, Dialect::MySql | Dialect::Oracle)
                    && self.peek_at(1).is_some_and(|b| {
                        is_ident_start(b) || (b == b'@' && self.dialect == Dialect::MySql)
                    })))
        {
            self.pos += 1;
            // @@server_vars too
            if self.peek() == Some(b'@') {
                self.pos += 1;
            }
            while self.peek().map(is_ident_continue).unwrap_or(false) {
                self.pos += 1;
            }
            return Some(self.token(TokenKind::ParamMarker(ParamStyle::AtName), start));
        }
        // `:name`: Oracle / SQLite binds, psql variables, and the path steps
        // of Snowflake semi-structured access (`src:customer.name`), Snowflake
        // being read with the PostgreSQL rules. MySQL and SQL Server have no
        // `:name` syntax of their own, but it is the named placeholder every
        // client and driver accepts there; it counts only when the `:` does
        // not touch a name or another `:` on its left, so a MySQL `lbl:LOOP`
        // label and a T-SQL `geography::Point` call are left alone.
        if c == b':'
            && self.peek_at(1).map(is_ident_start).unwrap_or(false)
            && match self.dialect {
                Dialect::Oracle | Dialect::Sqlite | Dialect::Postgres => true,
                Dialect::MySql | Dialect::SqlServer => start
                    .checked_sub(1)
                    .and_then(|i| self.bytes.get(i))
                    .is_none_or(|&b| !is_ident_continue(b) && b != b':'),
            }
        {
            self.pos += 1;
            while self.peek().map(is_ident_continue).unwrap_or(false) {
                self.pos += 1;
            }
            return Some(self.token(TokenKind::ParamMarker(ParamStyle::ColonName), start));
        }
        if c == b'$'
            && self.dialect == Dialect::Postgres
            && self.peek_at(1).map(|b| b.is_ascii_digit()).unwrap_or(false)
        {
            self.pos += 1;
            while self.peek().map(|b| b.is_ascii_digit()).unwrap_or(false) {
                self.pos += 1;
            }
            return Some(self.token(TokenKind::ParamMarker(ParamStyle::DollarNum), start));
        }
        // `?`: the positional placeholder. PostgreSQL reads it as an
        // operator character (`?|`, `jsonb ? 'key'`), so only the others
        // take it as a marker, and SQL Server / Oracle (whose own syntax has
        // no `?`) leave the jsonb `?|` / `?&` spellings unlexed rather than
        // reflow PostgreSQL text that auto-detection misread.
        if c == b'?'
            && match self.dialect {
                Dialect::Postgres => false,
                Dialect::MySql | Dialect::Sqlite => true,
                Dialect::SqlServer | Dialect::Oracle => {
                    !matches!(self.peek_at(1), Some(b'|' | b'&'))
                }
            }
        {
            self.pos += 1;
            // SQLite's numbered form, `?1`.
            if self.dialect == Dialect::Sqlite {
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            return Some(self.token(TokenKind::ParamMarker(ParamStyle::Question), start));
        }

        // Numbers
        if c.is_ascii_digit()
            || (c == b'.' && self.peek_at(1).map(|b| b.is_ascii_digit()).unwrap_or(false))
        {
            return Some(self.lex_number(start));
        }

        // Words
        if is_ident_start(c) {
            while self.peek().map(is_ident_continue).unwrap_or(false) {
                self.pos += 1;
            }
            if let Some(literal) = self.prefixed_literal(start) {
                return Some(literal);
            }
            let text = &self.src[start..self.pos];
            let kind = if dialect_tables::is_keyword(text, self.dialect) {
                TokenKind::Keyword
            } else {
                TokenKind::Ident(IdentQuote::None)
            };
            return Some(self.token(kind, start));
        }

        // Punctuation / operators
        let single = match c {
            b',' => Some(TokenKind::Comma),
            b';' => Some(TokenKind::Semicolon),
            b'(' => Some(TokenKind::LParen),
            b')' => Some(TokenKind::RParen),
            b'.' => Some(TokenKind::Dot),
            _ => None,
        };
        if let Some(kind) = single {
            self.pos += 1;
            return Some(self.token(kind, start));
        }

        // `[` / `]`: PostgreSQL arrays and subscripts (`ARRAY[1, 2]`,
        // `tags[1]`, `int[]`) and BigQuery arrays, BigQuery being read with
        // the MySQL rules. SQL Server and SQLite read `[…]` as a quoted name
        // before this point.
        if matches!(c, b'[' | b']') && matches!(self.dialect, Dialect::Postgres | Dialect::MySql) {
            self.pos += 1;
            return Some(self.token(TokenKind::Operator, start));
        }

        // A PostgreSQL operator is any run of `+ - * / < > = ~ ! @ # % ^ & | ?`
        // (`@>`, `#>>`, `?|`, `||/`, user-defined ones), lexed as one token
        // the way the server does. A run longer than one character ends in
        // `+` / `-` only when it also holds one of `~ ! @ # % ^ & | ?`, so
        // `x>=-1` is `>=` then `-`; `--` and `/*` inside a run start a
        // comment. `@` straight before a name (a Snowflake `@stage`) is left
        // to the illegal-character path below.
        if self.dialect == Dialect::Postgres
            && is_pg_operator_char(c)
            && !(c == b'@' && self.peek_at(1).is_some_and(is_ident_start))
        {
            let mut end = start;
            while let Some(&b) = self.bytes.get(end) {
                if !is_pg_operator_char(b)
                    || (end > start
                        && matches!(&self.bytes[end..], [b'-', b'-', ..] | [b'/', b'*', ..]))
                {
                    break;
                }
                end += 1;
            }
            if end - start > 1
                && !self.bytes[start..end]
                    .iter()
                    .any(|b| b"~!@#%^&|?".contains(b))
            {
                while end - start > 1 && matches!(self.bytes[end - 1], b'+' | b'-') {
                    end -= 1;
                }
            }
            self.pos = end;
            return Some(self.token(TokenKind::Operator, start));
        }

        if dialect_tables::OPERATOR_CHARS.contains(&c) {
            // Greedy 3- then 2-char operator match (`->>`, `<=>`, `<=`, `<>`,
            // `!=`, `||`, `::`), trimmed back to 1 char when no longer
            // operator matches.
            if let Some(three) = self.src.get(start..start + 3)
                && dialect_tables::is_operator(three)
            {
                self.pos += 3;
                return Some(self.token(TokenKind::Operator, start));
            }
            let mut len = 1;
            if self
                .peek_at(1)
                .map(|b| dialect_tables::OPERATOR_CHARS.contains(&b))
                .unwrap_or(false)
            {
                let two = &self.src[start..start + 2];
                if dialect_tables::is_operator(two) {
                    len = 2;
                }
            }
            self.pos += len;
            return Some(self.token(TokenKind::Operator, start));
        }

        // Anything else: illegal character (advance past the full UTF-8
        // scalar so we never split a char boundary).
        let ch_len = self.src[start..]
            .chars()
            .next()
            .map(|ch| ch.len_utf8())
            .unwrap_or(1);
        self.pos += ch_len;
        Some(self.token(TokenKind::Error(LexErrorKind::IllegalCharacter), start))
    }

    /// The word just read (`start..self.pos`) when it prefixes a string
    /// literal written against it, as one string token: the server reads
    /// `x'ff'` as one blob literal, so splitting it would read as a column
    /// `x` with an alias. MySQL / MariaDB `b'…'`, `x'…'`, `n'…'` and
    /// `_charset'…'` introducers (plus BigQuery's `r'…'`, `b'…'`, `rb'…'`,
    /// BigQuery being read with the MySQL rules), PostgreSQL `B'…'`, `X'…'`
    /// and `U&'…'`, SQLite `x'…'`, Oracle `n'…'`. An unterminated literal is
    /// left to the string path, which reports it.
    fn prefixed_literal(&mut self, start: usize) -> Option<Token<'a>> {
        let word = &self.src[start..self.pos];
        let is = |w: &str| word.eq_ignore_ascii_case(w);
        let quote_at = match self.dialect {
            Dialect::MySql
                if ["b", "x", "n", "r", "rb", "br"].iter().any(|w| is(w))
                    || (word.len() > 1 && word.starts_with('_')) =>
            {
                self.pos
            }
            Dialect::Postgres if is("u") && self.peek() == Some(b'&') => self.pos + 1,
            Dialect::Postgres if is("b") || is("x") => self.pos,
            Dialect::Sqlite if is("x") => self.pos,
            Dialect::Oracle if is("n") => self.pos,
            _ => return None,
        };
        let quote = *self.bytes.get(quote_at)?;
        // MySQL also writes strings in double quotes (`_utf8mb4"…"`).
        if !(quote == b'\'' || (quote == b'"' && self.dialect == Dialect::MySql)) {
            return None;
        }
        let con = crate::seclex::editor_construct_at(self.bytes, quote_at, self.dialect)?;
        if !con.terminated || con.start != quote_at {
            return None;
        }
        self.pos = con.end;
        Some(self.token(TokenKind::StringLit(StringShape::Plain), start))
    }

    /// Turn a `seclex` construct into this lexer's token, keeping the
    /// lexer's historical recovery for unterminated input: a string or block
    /// comment runs to the end as an error token; an unterminated quoted
    /// identifier stops at the end of its line so the rest of the buffer
    /// still lexes (the security scan treats all of them as code instead).
    fn token_from_construct(
        &mut self,
        con: crate::seclex::Construct,
        start: usize,
        shape_override: Option<StringShape>,
    ) -> Token<'a> {
        use crate::seclex::ConstructKind as K;
        let open = self.bytes[con.start];
        match con.kind {
            K::LineComment => {
                self.pos = con.end;
                self.token(TokenKind::Comment(CommentShape::Line), start)
            }
            K::BlockComment | K::ExecOpen { .. } => {
                self.pos = con.end;
                if con.terminated {
                    self.token(TokenKind::Comment(CommentShape::Block), start)
                } else {
                    self.token(
                        TokenKind::Error(LexErrorKind::UnterminatedBlockComment),
                        start,
                    )
                }
            }
            K::Variable => {
                self.pos = con.end;
                let style = if open == b'@' {
                    ParamStyle::AtName
                } else {
                    ParamStyle::ColonName
                };
                self.token(TokenKind::ParamMarker(style), start)
            }
            K::QuotedIdent { .. } | K::Str { .. }
                if open == b'"' || open == b'`' || open == b'[' =>
            {
                let quote = match open {
                    b'[' => IdentQuote::Bracket,
                    b'`' => IdentQuote::Backtick,
                    _ => IdentQuote::DoubleQuote,
                };
                if con.terminated {
                    self.pos = con.end;
                    return self.token(TokenKind::Ident(quote), start);
                }
                self.pos = self.bytes[con.start..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|i| con.start + i)
                    .unwrap_or(self.bytes.len());
                self.token(
                    TokenKind::Error(LexErrorKind::UnterminatedQuotedIdent),
                    start,
                )
            }
            K::QuotedIdent { .. } | K::Str { .. } | K::DollarQuoted => {
                self.pos = con.end;
                if !con.terminated {
                    return self.token(TokenKind::Error(LexErrorKind::UnterminatedString), start);
                }
                let shape = shape_override.unwrap_or(match con.kind {
                    K::DollarQuoted => StringShape::DollarQuoted,
                    K::Str { prefix_len: 0 } => StringShape::Plain,
                    _ if matches!(open, b'e' | b'E') => StringShape::Escape,
                    _ if matches!(open, b'q' | b'Q' | b'n' | b'N') => StringShape::OracleQ,
                    _ => StringShape::Plain,
                });
                self.token(TokenKind::StringLit(shape), start)
            }
        }
    }

    fn lex_number(&mut self, start: usize) -> Token<'a> {
        // Hex
        if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x') | Some(b'X')) {
            self.pos += 2;
            while self.peek().map(|b| b.is_ascii_hexdigit()).unwrap_or(false) {
                self.pos += 1;
            }
            return self.token(TokenKind::Number(NumberShape::Hex), start);
        }
        let mut shape = NumberShape::Integer;
        while self.peek().map(|b| b.is_ascii_digit()).unwrap_or(false) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.')
            // `1.e5` is a decimal; `1..2` (range-ish junk) keeps `1` integer.
            && self.peek_at(1) != Some(b'.')
        {
            shape = NumberShape::Decimal;
            self.pos += 1;
            while self.peek().map(|b| b.is_ascii_digit()).unwrap_or(false) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let mut off = 1;
            if matches!(self.peek_at(1), Some(b'+') | Some(b'-')) {
                off = 2;
            }
            if self
                .peek_at(off)
                .map(|b| b.is_ascii_digit())
                .unwrap_or(false)
            {
                shape = NumberShape::Scientific;
                self.pos += off;
                while self.peek().map(|b| b.is_ascii_digit()).unwrap_or(false) {
                    self.pos += 1;
                }
            }
        }
        self.token(TokenKind::Number(shape), start)
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// T-SQL names may also carry `#` and `@` after their first character.
fn is_tsql_name_continue(b: u8) -> bool {
    is_ident_continue(b) || b == b'#' || b == b'@'
}

/// The characters a PostgreSQL operator is made of.
fn is_pg_operator_char(b: u8) -> bool {
    b"+-*/<>=~!@#%^&|?".contains(&b)
}

// ---------------------------------------------------------------------------
// Token cursor (the parser's view; skips trivia, supports recovery contexts)
// ---------------------------------------------------------------------------

/// Recovery context the parser maintains; the innermost context decides which
/// recovery rules apply (ADR-011 §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexContext {
    TableBody,
    ColumnDef,
    ArgList,
    SelectList,
    ParenExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryAction {
    pub kind: RecoveryKind,
    pub span: Span,
    pub assumption: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryKind {
    AssumedComma,
    MissingType,
    UnterminatedStringClosed,
    UnbalancedParenClosed,
    GrabBag,
    SkippedToken,
}

/// Cursor over a strict token stream with trivia skipping, newline awareness,
/// and a context stack. The "recovering lexer" of the spec is this cursor plus
/// the per-context rules the parser invokes — e.g. [`TokenCursor::eat_separator`]
/// treats a newline inside a `TableBody` as an assumed comma.
pub struct TokenCursor<'a> {
    tokens: Vec<Token<'a>>,
    /// Indexes of non-trivia tokens.
    significant: Vec<usize>,
    /// significant index → true when a newline appears in trivia before it.
    newline_before: Vec<bool>,
    pos: usize,
    contexts: Vec<LexContext>,
    pub recovery: Vec<RecoveryAction>,
}

impl<'a> TokenCursor<'a> {
    pub fn new(src: &'a str, dialect: Dialect) -> Self {
        let tokens = StrictLexer::tokenize(src, dialect);
        Self::from_tokens(tokens)
    }

    pub fn from_tokens(tokens: Vec<Token<'a>>) -> Self {
        let mut significant = Vec::new();
        let mut newline_before = Vec::new();
        let mut saw_newline = false;
        for (i, t) in tokens.iter().enumerate() {
            if t.is_trivia() {
                if t.text.contains('\n') {
                    saw_newline = true;
                }
            } else {
                significant.push(i);
                newline_before.push(saw_newline);
                saw_newline = false;
            }
        }
        Self {
            tokens,
            significant,
            newline_before,
            pos: 0,
            contexts: Vec::new(),
            recovery: Vec::new(),
        }
    }

    pub fn push_context(&mut self, ctx: LexContext) {
        self.contexts.push(ctx);
    }

    pub fn pop_context(&mut self) {
        self.contexts.pop();
    }

    pub fn context(&self) -> Option<LexContext> {
        self.contexts.last().copied()
    }

    pub fn is_eof(&self) -> bool {
        self.pos >= self.significant.len()
    }

    pub fn peek(&self) -> Option<&Token<'a>> {
        self.significant.get(self.pos).map(|&i| &self.tokens[i])
    }

    pub fn peek_n(&self, n: usize) -> Option<&Token<'a>> {
        self.significant.get(self.pos + n).map(|&i| &self.tokens[i])
    }

    /// True when a newline separates the previous significant token from the
    /// next one — the trigger for assumed-comma recovery.
    pub fn newline_before_next(&self) -> bool {
        self.newline_before.get(self.pos).copied().unwrap_or(false)
    }

    pub fn advance(&mut self) -> Option<Token<'a>> {
        let t = self.significant.get(self.pos).map(|&i| self.tokens[i]);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// End offset of the most recently consumed significant token (0 if none).
    pub fn prev_end(&self) -> usize {
        if self.pos == 0 {
            return 0;
        }
        self.significant
            .get(self.pos - 1)
            .map(|&i| self.tokens[i].span.end)
            .unwrap_or(0)
    }

    pub fn eat_keyword(&mut self, word: &str) -> bool {
        if self.peek().map(|t| t.keyword_eq(word)).unwrap_or(false) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Eat the next token when it is an unquoted word matching `word`, whether
    /// the lexer classified it as a keyword or an identifier (dialect keyword
    /// tables don't cover statement-specific words like `STATISTICS`).
    pub fn eat_word(&mut self, word: &str) -> bool {
        if self
            .peek()
            .map(|t| t.is_word() && !t.ident_quoted() && t.text.eq_ignore_ascii_case(word))
            .unwrap_or(false)
        {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn eat_kind(&mut self, kind: TokenKind) -> Option<Token<'a>> {
        if self.peek().map(|t| t.kind == kind).unwrap_or(false) {
            self.advance()
        } else {
            None
        }
    }

    /// Either consume a comma, or — in list-like contexts — accept a newline
    /// as an assumed comma, recording the recovery. Returns false when neither
    /// separator is present.
    pub fn eat_separator(&mut self) -> bool {
        if self.eat_kind(TokenKind::Comma).is_some() {
            return true;
        }
        let list_context = matches!(
            self.context(),
            Some(
                LexContext::TableBody
                    | LexContext::ArgList
                    | LexContext::SelectList
                    | LexContext::ColumnDef
            )
        );
        // Only assume a comma when the next token could plausibly start a
        // new list item; a closing paren ends the list.
        if list_context
            && self.newline_before_next()
            && let Some(t) = self.peek()
            && (t.is_word() || matches!(t.kind, TokenKind::ParamMarker(_)))
        {
            let span = t.span;
            self.recovery.push(RecoveryAction {
                kind: RecoveryKind::AssumedComma,
                span,
                assumption: "newline treated as list separator".into(),
            });
            return true;
        }
        false
    }

    pub fn record(&mut self, kind: RecoveryKind, span: Span, assumption: impl Into<String>) {
        self.recovery.push(RecoveryAction {
            kind,
            span,
            assumption: assumption.into(),
        });
    }

    /// Skip tokens until one of `stops` (kind match) or a keyword in
    /// `stop_words`, balancing parens; collects skipped tokens as a grab-bag.
    pub fn skip_to(&mut self, stops: &[TokenKind], stop_words: &[&str]) -> Vec<Token<'a>> {
        let mut bag = Vec::new();
        let mut depth = 0usize;
        while let Some(t) = self.peek().copied() {
            if depth == 0 {
                if stops.contains(&t.kind) {
                    break;
                }
                if stop_words.iter().any(|w| t.keyword_eq(w)) {
                    break;
                }
            }
            match t.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            bag.push(t);
            self.pos += 1;
        }
        if !bag.is_empty() {
            let span = Span::new(bag[0].span.start, bag[bag.len() - 1].span.end);
            self.record(
                RecoveryKind::GrabBag,
                span,
                "tokens preserved without a parsed role",
            );
        }
        bag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, d: Dialect) -> Vec<TokenKind> {
        StrictLexer::tokenize(src, d)
            .into_iter()
            .filter(|t| !t.is_trivia())
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn basic_select_tokens() {
        let ks = kinds("SELECT a.b, 'x' FROM [T];", Dialect::SqlServer);
        assert_eq!(
            ks,
            vec![
                TokenKind::Keyword,
                TokenKind::Ident(IdentQuote::None),
                TokenKind::Dot,
                TokenKind::Ident(IdentQuote::None),
                TokenKind::Comma,
                TokenKind::StringLit(StringShape::Plain),
                TokenKind::Keyword,
                TokenKind::Ident(IdentQuote::Bracket),
                TokenKind::Semicolon,
            ]
        );
    }

    #[test]
    fn unquote_identifier_collapses_each_quote_style() {
        assert_eq!(unquote_identifier("[a]]b]"), "a]b");
        assert_eq!(unquote_identifier("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote_identifier("`a``b`"), "a`b");
        // Only the style's own closing quote is an escape.
        assert_eq!(unquote_identifier("[a\"\"b]"), "a\"\"b");
        assert_eq!(unquote_identifier("\"a]]b\""), "a]]b");
        assert_eq!(unquote_identifier("[]]]]]"), "]]");
        assert_eq!(unquote_identifier("plain"), "plain");
        assert_eq!(unquote_identifier("[x"), "[x");
        assert_eq!(unquote_identifier("[]"), "");
        // No escape → no allocation.
        assert!(matches!(unquote_identifier("[ab]"), Cow::Borrowed("ab")));
    }

    #[test]
    fn ident_text_unescapes_lexed_quoted_identifiers() {
        let lex = |src: &str, d: Dialect| {
            StrictLexer::tokenize(src, d)
                .into_iter()
                .find(|t| !t.is_trivia())
                .expect("token")
                .ident_text()
                .into_owned()
        };
        assert_eq!(lex("[a]]b]", Dialect::SqlServer), "a]b");
        assert_eq!(lex("\"a\"\"b\"", Dialect::Postgres), "a\"b");
        assert_eq!(lex("`a``b`", Dialect::MySql), "a`b");
        assert_eq!(lex("abc", Dialect::MySql), "abc");
    }

    #[test]
    fn dialect_specific_quoting() {
        assert!(matches!(
            kinds("`tbl`", Dialect::MySql)[0],
            TokenKind::Ident(IdentQuote::Backtick)
        ));
        // Backtick is not a quote character in SQL Server: should error.
        assert!(matches!(
            kinds("`tbl`", Dialect::SqlServer)[0],
            TokenKind::Error(_)
        ));
        assert!(matches!(
            kinds("\"tbl\"", Dialect::Postgres)[0],
            TokenKind::Ident(IdentQuote::DoubleQuote)
        ));
        assert!(matches!(
            kinds("[tbl]", Dialect::Sqlite)[0],
            TokenKind::Ident(IdentQuote::Bracket)
        ));
    }

    #[test]
    fn param_markers_per_dialect() {
        assert!(matches!(
            kinds("@p", Dialect::SqlServer)[0],
            TokenKind::ParamMarker(ParamStyle::AtName)
        ));
        assert!(matches!(
            kinds(":p", Dialect::Oracle)[0],
            TokenKind::ParamMarker(ParamStyle::ColonName)
        ));
        assert!(matches!(
            kinds("$1", Dialect::Postgres)[0],
            TokenKind::ParamMarker(ParamStyle::DollarNum)
        ));
        assert!(matches!(
            kinds("?", Dialect::Sqlite)[0],
            TokenKind::ParamMarker(ParamStyle::Question)
        ));
    }

    #[test]
    fn string_shapes() {
        assert!(matches!(
            kinds("N'abc'", Dialect::SqlServer)[0],
            TokenKind::StringLit(StringShape::National)
        ));
        assert!(matches!(
            kinds("E'a\\nb'", Dialect::Postgres)[0],
            TokenKind::StringLit(StringShape::Escape)
        ));
        assert!(matches!(
            kinds("q'{it's}'", Dialect::Oracle)[0],
            TokenKind::StringLit(StringShape::OracleQ)
        ));
        assert!(matches!(
            kinds("$tag$ body $tag$", Dialect::Postgres)[0],
            TokenKind::StringLit(StringShape::DollarQuoted)
        ));
        // Doubled-quote escape stays one literal.
        let ks = kinds("'it''s'", Dialect::SqlServer);
        assert_eq!(ks.len(), 1);
    }

    #[test]
    fn numbers() {
        assert!(matches!(
            kinds("42", Dialect::Sqlite)[0],
            TokenKind::Number(NumberShape::Integer)
        ));
        assert!(matches!(
            kinds("4.2", Dialect::Sqlite)[0],
            TokenKind::Number(NumberShape::Decimal)
        ));
        assert!(matches!(
            kinds("1e10", Dialect::Sqlite)[0],
            TokenKind::Number(NumberShape::Scientific)
        ));
        assert!(matches!(
            kinds("0xFF", Dialect::SqlServer)[0],
            TokenKind::Number(NumberShape::Hex)
        ));
    }

    #[test]
    fn operators_two_char() {
        let ks: Vec<&str> = StrictLexer::tokenize("a >= 1 <> 2 != 3", Dialect::SqlServer)
            .into_iter()
            .filter(|t| t.kind == TokenKind::Operator)
            .map(|t| t.text)
            .collect();
        assert_eq!(ks, vec![">=", "<>", "!="]);
    }

    /// `(kind, text)` of every significant token.
    fn toks(src: &str, d: Dialect) -> Vec<(TokenKind, &str)> {
        StrictLexer::tokenize(src, d)
            .into_iter()
            .filter(|t| !t.is_trivia())
            .map(|t| (t.kind, t.text))
            .collect()
    }

    fn operators(src: &str, d: Dialect) -> Vec<&str> {
        toks(src, d)
            .into_iter()
            .filter(|(k, _)| *k == TokenKind::Operator)
            .map(|(_, t)| t)
            .collect()
    }

    fn no_errors(src: &str, d: Dialect) {
        let errors: Vec<_> = toks(src, d)
            .into_iter()
            .filter(|(k, _)| matches!(k, TokenKind::Error(_)))
            .collect();
        assert!(errors.is_empty(), "{src:?} under {d:?}: {errors:?}");
    }

    #[test]
    fn tsql_temp_tables_and_dollar_pseudo_columns_are_names() {
        let plain = TokenKind::Ident(IdentQuote::None);
        assert_eq!(
            toks(
                "select * from #recent join ##shared on 1=1",
                Dialect::SqlServer
            ),
            vec![
                (TokenKind::Keyword, "select"),
                (TokenKind::Operator, "*"),
                (TokenKind::Keyword, "from"),
                (plain, "#recent"),
                (TokenKind::Keyword, "join"),
                (plain, "##shared"),
                (TokenKind::Keyword, "on"),
                (TokenKind::Number(NumberShape::Integer), "1"),
                (TokenKind::Operator, "="),
                (TokenKind::Number(NumberShape::Integer), "1"),
            ]
        );
        assert_eq!(
            toks("output $action", Dialect::SqlServer)[1],
            (plain, "$action")
        );
        assert_eq!(toks("#t#x@y", Dialect::SqlServer), vec![(plain, "#t#x@y")]);
        // Elsewhere `#` keeps its own meaning (a MySQL comment).
        assert!(
            kinds("#t", Dialect::MySql).is_empty(),
            "a comment is trivia"
        );
    }

    #[test]
    fn mysql_variables_and_oracle_db_links_are_at_names() {
        let at = TokenKind::ParamMarker(ParamStyle::AtName);
        assert_eq!(toks("set @total := 0", Dialect::MySql)[1], (at, "@total"));
        assert_eq!(
            toks("select @@sql_mode", Dialect::MySql)[1],
            (at, "@@sql_mode")
        );
        assert_eq!(
            toks("select * from t@remote", Dialect::Oracle)[4],
            (at, "@remote")
        );
        no_errors("select @a, @@session.time_zone", Dialect::MySql);
        // Not in PostgreSQL, where `@` before a name stays an error (a
        // Snowflake `@stage` path is not safe to re-space).
        assert!(matches!(
            kinds("@stage", Dialect::Postgres)[0],
            TokenKind::Error(_)
        ));
    }

    #[test]
    fn postgres_operators_are_lexed_as_runs() {
        assert_eq!(
            operators(
                "a->>'k' || b#>>'{x}' and c @> d and e <@ f and g ? 'k' and h ?| i and j ?& k",
                Dialect::Postgres
            ),
            vec!["->>", "||", "#>>", "@>", "<@", "?", "?|", "?&"]
        );
        assert_eq!(
            operators(
                "a @@ b, c ~* d, e !~ f, g ||/ 8, h -|- i",
                Dialect::Postgres
            ),
            vec!["@@", "~*", "!~", "||/", "-|-"]
        );
        // A run ends in `-`/`+` only when it holds one of ~!@#%^&|?:
        assert_eq!(
            operators("x>=-1 and y<>-2 and z*-3", Dialect::Postgres),
            vec![">=", "-", "<>", "-", "*", "-"]
        );
        assert_eq!(operators("x @- 1", Dialect::Postgres), vec!["@-"]);
        // Comments still begin inside a run.
        assert_eq!(operators("a=--note\n1", Dialect::Postgres), vec!["="]);
        assert_eq!(operators("a=/*c*/1", Dialect::Postgres), vec!["="]);
        // Casts and assignment keep their own path.
        assert_eq!(
            operators("a::int, b := 1", Dialect::Postgres),
            vec!["::", ":="]
        );
    }

    #[test]
    fn three_char_operators_lex_whole() {
        assert_eq!(
            operators("j->>'$.a' and a<=>b", Dialect::MySql),
            vec!["->>", "<=>"]
        );
        assert_eq!(
            operators("j->>'$.a' and a==b", Dialect::Sqlite),
            vec!["->>", "=="]
        );
        assert_eq!(operators("x := y**2", Dialect::Oracle), vec![":=", "**"]);
    }

    #[test]
    fn brackets_are_operators_where_they_do_not_quote_names() {
        assert_eq!(
            operators("select tags[1], array[1,2]::int[]", Dialect::Postgres),
            vec!["[", "]", "[", "]", "::", "[", "]"]
        );
        assert_eq!(operators("select [1, 2]", Dialect::MySql), vec!["[", "]"]);
        assert_eq!(
            kinds("[x]", Dialect::SqlServer)[0],
            TokenKind::Ident(IdentQuote::Bracket)
        );
        assert_eq!(
            kinds("[x]", Dialect::Sqlite)[0],
            TokenKind::Ident(IdentQuote::Bracket)
        );
    }

    #[test]
    fn colon_names_in_postgres() {
        let colon = TokenKind::ParamMarker(ParamStyle::ColonName);
        // psql variables, and Snowflake paths read with the PostgreSQL rules.
        assert_eq!(toks("where id = :id", Dialect::Postgres)[3], (colon, ":id"));
        assert_eq!(
            toks("src:customer.name::string", Dialect::Postgres),
            vec![
                (TokenKind::Ident(IdentQuote::None), "src"),
                (colon, ":customer"),
                (TokenKind::Dot, "."),
                (TokenKind::Ident(IdentQuote::None), "name"),
                (TokenKind::Operator, "::"),
                (TokenKind::Ident(IdentQuote::None), "string"),
            ]
        );
    }

    #[test]
    fn prefixed_literals_are_one_string() {
        let s = TokenKind::StringLit(StringShape::Plain);
        for (src, d) in [
            ("b'0101'", Dialect::MySql),
            ("X'ff'", Dialect::MySql),
            ("n'abc'", Dialect::MySql),
            ("_utf8mb4'abc'", Dialect::MySql),
            ("_latin1\"abc\"", Dialect::MySql),
            ("r'\\d+'", Dialect::MySql),
            ("B'101'", Dialect::Postgres),
            ("x'1F'", Dialect::Postgres),
            ("U&'d\\0061t'", Dialect::Postgres),
            ("x'ff'", Dialect::Sqlite),
            ("n'abc'", Dialect::Oracle),
        ] {
            assert_eq!(toks(src, d), vec![(s, src)], "{src} under {d:?}");
        }
        // A word before a spaced string is still a word, and an
        // unterminated literal is still reported.
        assert_eq!(
            kinds("x 'ff'", Dialect::Sqlite)[0],
            TokenKind::Ident(IdentQuote::None)
        );
        assert!(matches!(
            kinds("x'ff", Dialect::Sqlite).last(),
            Some(TokenKind::Error(_))
        ));
        // Not a prefix in SQL Server (`N'…'` has its own path).
        assert_eq!(
            kinds("x'ff'", Dialect::SqlServer)[0],
            TokenKind::Ident(IdentQuote::None)
        );
    }

    #[test]
    fn sqlite_numbered_parameters() {
        assert_eq!(
            toks("?1, ?", Dialect::Sqlite),
            vec![
                (TokenKind::ParamMarker(ParamStyle::Question), "?1"),
                (TokenKind::Comma, ","),
                (TokenKind::ParamMarker(ParamStyle::Question), "?"),
            ]
        );
    }

    #[test]
    fn errors_never_abort() {
        let ks = kinds("'unterminated", Dialect::SqlServer);
        assert!(matches!(
            ks[0],
            TokenKind::Error(LexErrorKind::UnterminatedString)
        ));
        let ks = kinds("a \"bad\nb", Dialect::SqlServer);
        // Stream continues past the error to EOF.
        assert!(ks.len() >= 3);
    }

    #[test]
    fn spans_cover_input_exactly() {
        let src = "SELECT * FROM t WHERE a >= 10 -- done";
        let toks = StrictLexer::tokenize(src, Dialect::Postgres);
        let mut pos = 0;
        for t in &toks {
            assert_eq!(t.span.start, pos, "gap before {:?}", t);
            pos = t.span.end;
        }
        assert_eq!(pos, src.len());
    }

    #[test]
    fn cursor_assumed_comma_on_newline() {
        let src = "a int\nb varchar(10)";
        let mut cur = TokenCursor::new(src, Dialect::SqlServer);
        cur.push_context(LexContext::TableBody);
        // consume: a int
        cur.advance();
        cur.advance();
        assert!(
            cur.eat_separator(),
            "newline should act as separator in TableBody"
        );
        assert_eq!(cur.recovery.len(), 1);
        assert_eq!(cur.recovery[0].kind, RecoveryKind::AssumedComma);
        // without context, no assumption
        let mut cur2 = TokenCursor::new(src, Dialect::SqlServer);
        cur2.advance();
        cur2.advance();
        assert!(!cur2.eat_separator());
    }

    /// The editor lexer takes its literal/identifier/comment boundaries from
    /// `seclex`, so the splitter the drivers use can never cut a statement
    /// where the engine sees a literal (or glue one where it sees code).
    #[test]
    fn boundaries_follow_the_security_lexer() {
        use crate::sqllex::split_statements;
        // A quoted identifier may span lines; the `;` inside it is not a
        // terminator (the old lexer stopped the identifier at the newline).
        let parts = split_statements("SELECT \"a\n; DELETE FROM t; --\" FROM x", Dialect::Sqlite);
        assert_eq!(parts.len(), 1, "{parts:?}");
        // T-SQL block comments nest.
        let parts = split_statements("/* /* */ ; DELETE FROM t; */ SELECT 1", Dialect::SqlServer);
        assert_eq!(parts.len(), 1, "{parts:?}");
        // MySQL `--` needs whitespace after it.
        let ks = kinds("SELECT 1--1", Dialect::MySql);
        assert!(
            !ks.iter().any(|k| matches!(k, TokenKind::Comment(_))),
            "{ks:?}"
        );
        // PostgreSQL line comments end at a bare carriage return.
        let ks = kinds("SELECT 1 -- x\rDELETE", Dialect::Postgres);
        assert!(matches!(ks.last(), Some(TokenKind::Keyword)), "{ks:?}");
        // SQLite brackets have no `]]` escape.
        let ks = kinds("[a]]", Dialect::Sqlite);
        assert_eq!(ks[0], TokenKind::Ident(IdentQuote::Bracket));
        // MySQL double-quoted text takes backslash escapes.
        let parts = split_statements("SELECT \"a\\\"; DELETE FROM t; --\"", Dialect::MySql);
        assert_eq!(parts.len(), 1, "{parts:?}");
    }

    #[test]
    fn mysql_hash_comment() {
        let ks = kinds("# note\nselect 1", Dialect::MySql);
        assert!(matches!(ks[0], TokenKind::Keyword));
    }
}
