//! Tree building: collecting significant tokens into [`Item`]s and building
//! the paren tree ([`Node`]) that classifies each parenthesized group
//! ([`ParenKind`]) before layout.

use crate::sqllex::{CommentShape, IdentQuote, ParamStyle, StringShape, Token, TokenKind};
use crate::sqlmodel::{Dialect, Span};

/// A significant token plus how many newlines separated it from the previous
/// significant token in the source (0 = same line).
#[derive(Clone, Copy)]
pub(super) struct Item<'a> {
    pub(super) tok: Token<'a>,
    pub(super) newlines_before: u32,
    /// Set on words directly followed by a `(` that reads as a call — drives
    /// `function_case` and `space_before_function_parentheses`.
    pub(super) is_function_name: bool,
    /// Set on structural words the dialect keyword tables don't list but
    /// that read as keywords in context (`INCLUDE (…)` of an index).
    pub(super) is_context_keyword: bool,
    /// Print directly against the previous token, with no separator: the
    /// two were written touching, and a space would change how the server
    /// reads them (Snowflake `src:path`, MySQL `user@host`, Oracle
    /// `t@dblink`) or just read oddly (`arr[1]`, `int[]`).
    pub(super) attach: bool,
}

/// Collect the significant tokens of `src` (lexed as `dialect`) into items.
///
/// A few token runs become one item so layout can never pull them apart:
/// - operator characters written touching (`->` `>`, `<=` `>`, `*` `*`) are
///   one operator, so `j->>'$.a'` can never become `j -> > '$.a'`; a
///   trailing `-` / `+` stays separate so `x>=-1` still reads `x >= -1`;
/// - a `[ … ]` group (PostgreSQL arrays and subscripts, BigQuery arrays) is
///   one verbatim item;
/// - Oracle `name%TYPE` / `cursor%ROWCOUNT` attribute references are one
///   word.
pub(super) fn collect_items<'a>(
    src: &'a str,
    tokens: &[Token<'a>],
    dialect: Dialect,
) -> Vec<Item<'a>> {
    let mut items: Vec<Item<'a>> = Vec::with_capacity(tokens.len());
    let mut newlines = 0u32;
    let mut i = 0usize;
    while i < tokens.len() {
        let t = tokens[i];
        if t.kind == TokenKind::Whitespace {
            newlines += t.text.matches('\n').count() as u32;
            i += 1;
            continue;
        }
        let prev = items.last();
        let touches = prev.is_some_and(|p| {
            p.tok.span.end == t.span.start && !matches!(p.tok.kind, TokenKind::Comment(_))
        });
        let prev_is_operand = touches && prev.is_some_and(|p| is_operand(&p.tok));
        let prev_is_string =
            touches && prev.is_some_and(|p| matches!(p.tok.kind, TokenKind::StringLit(_)));

        // `[ … ]` as one verbatim item.
        if is_operator(&t, "[")
            && matches!(dialect, Dialect::Postgres | Dialect::MySql)
            && let Some(close) = matching_bracket(tokens, i)
        {
            let end = tokens[close].span.end;
            items.push(item(
                Token {
                    kind: TokenKind::StringLit(StringShape::Plain),
                    span: Span::new(t.span.start, end),
                    text: &src[t.span.start..end],
                },
                newlines,
                prev_is_operand,
            ));
            newlines = 0;
            i = close + 1;
            continue;
        }

        // Touching operator characters are one operator.
        if t.kind == TokenKind::Operator
            && touches
            && let Some(p) = items.last_mut()
            && p.tok.kind == TokenKind::Operator
            && !matches!(t.text, "[" | "]")
            && !matches!(p.tok.text, "[" | "]")
            && !(matches!(t.text, "-" | "+") && !operator_touches_next(tokens, i))
        {
            p.tok.span = Span::new(p.tok.span.start, t.span.end);
            p.tok.text = &src[p.tok.span.start..t.span.end];
            i += 1;
            continue;
        }

        // Oracle `emp.sal%TYPE`, `c1%NOTFOUND`, `SQL%ROWCOUNT`.
        if dialect == Dialect::Oracle
            && is_operator(&t, "%")
            && touches
            && let Some(p) = items.last_mut()
            && p.tok.is_word()
            && let Some(next) = tokens.get(i + 1)
            && next.is_word()
            && next.span.start == t.span.end
        {
            p.tok = Token {
                kind: TokenKind::Ident(IdentQuote::None),
                span: Span::new(p.tok.span.start, next.span.end),
                text: &src[p.tok.span.start..next.span.end],
            };
            i += 2;
            continue;
        }

        let attach = prev_is_operand
            && match t.kind {
                // Snowflake semi-structured paths (`src:customer.name`),
                // read with the PostgreSQL rules.
                TokenKind::ParamMarker(ParamStyle::ColonName) => dialect == Dialect::Postgres,
                // MySQL `'user'@host` / `user@host`, Oracle `t@dblink`.
                TokenKind::ParamMarker(ParamStyle::AtName) => {
                    matches!(dialect, Dialect::MySql | Dialect::Oracle)
                }
                _ => false,
            }
            // Two literals written touching (`'a''b'` is one string to the
            // server's lexer in some modes, a triple-quoted BigQuery string
            // lexes as three) stay touching.
            || (prev_is_string && matches!(t.kind, TokenKind::StringLit(_)));
        items.push(item(t, newlines, attach));
        newlines = 0;
        i += 1;
    }
    mark_context_keywords(&mut items);
    items
}

/// Words the dialect keyword tables leave as plain names (they are
/// deliberately the structural subset parsers dispatch on) but that read as
/// keywords where they stand, so keyword casing reaches them:
/// `OVER (`, `PARTITION BY`, `WITHIN GROUP`, `FILTER (WHERE`,
/// `WHEN [NOT] MATCHED`, `IF [NOT] EXISTS`, `CREATE OR REPLACE`,
/// `NULLS FIRST|LAST`, `INTERVAL '…'`, `ARRAY[…]`, and a `QUALIFY` clause.
/// A column that merely shares the name keeps its own casing.
fn mark_context_keywords<'a>(items: &mut [Item<'a>]) {
    let code: Vec<usize> = (0..items.len())
        .filter(|&k| !matches!(items[k].tok.kind, TokenKind::Comment(_)))
        .collect();
    let toks: Vec<Token<'a>> = code.iter().map(|&k| items[k].tok).collect();
    let at = |pos: Option<usize>| pos.and_then(|p| toks.get(p)).copied();
    let word = |t: Option<Token<'a>>, w: &str| t.is_some_and(|t| is_word(&t, w));
    let kind = |t: Option<Token<'a>>, k: TokenKind| t.is_some_and(|t| t.kind == k);
    for p in 0..code.len() {
        let k = code[p];
        if items[k].tok.kind != TokenKind::Ident(IdentQuote::None) {
            continue;
        }
        let prev = at(p.checked_sub(1));
        let prev2 = at(p.checked_sub(2));
        let next = at(Some(p + 1));
        let next2 = at(Some(p + 2));
        let lower = items[k].tok.text.to_ascii_lowercase();
        let hit = match lower.as_str() {
            "over" => kind(next, TokenKind::LParen) && kind(prev, TokenKind::RParen),
            "partition" => word(next, "by"),
            "within" => word(next, "group") && kind(prev, TokenKind::RParen),
            "filter" => {
                kind(next, TokenKind::LParen)
                    && kind(prev, TokenKind::RParen)
                    && word(next2, "where")
            }
            "matched" => word(prev, "when") || (word(prev, "not") && word(prev2, "when")),
            // `MERGE … USING source`, `JOIN … USING (col)` (T-SQL's keyword
            // table does not list it).
            "using" => {
                prev.is_some_and(|t| t.is_word() || t.kind == TokenKind::RParen)
                    && next.is_some_and(|t| t.is_word() || t.kind == TokenKind::LParen)
            }
            "if" => word(next, "exists") || (word(next, "not") && word(next2, "exists")),
            "replace" => word(prev, "or") && word(prev2, "create"),
            "nulls" => word(next, "first") || word(next, "last"),
            // Redshift table attributes after a CREATE TABLE body.
            "diststyle" => next.is_some_and(|t| t.is_word()) && kind(prev, TokenKind::RParen),
            "compound" | "interleaved" => word(next, "sortkey"),
            "distkey" => kind(next, TokenKind::LParen),
            // `ON CONFLICT (…) DO UPDATE | NOTHING` (SQLite; PostgreSQL lists DO).
            "do" => word(next, "update") || word(next, "nothing"),
            "last" => word(prev, "nulls"),
            // `INTERVAL '7 days'`, and MySQL / BigQuery `INTERVAL 7 DAY`.
            "interval" => next
                .is_some_and(|t| matches!(t.kind, TokenKind::StringLit(_) | TokenKind::Number(_))),
            "array" => {
                next.is_some_and(|t| {
                    matches!(t.kind, TokenKind::StringLit(_)) && t.text.starts_with('[')
                }) && items[code[p + 1]].attach
            }
            // A clause, not a column: something before it to qualify and a
            // condition after it.
            "qualify" => {
                prev.is_some_and(|t| {
                    !matches!(
                        t.kind,
                        TokenKind::Comma | TokenKind::Dot | TokenKind::LParen
                    ) && !is_word(&t, "select")
                        && !is_word(&t, "as")
                        && !is_word(&t, "distinct")
                }) && next.is_some_and(|t| {
                    !matches!(
                        t.kind,
                        TokenKind::Comma
                            | TokenKind::RParen
                            | TokenKind::Semicolon
                            | TokenKind::Dot
                    ) && !is_word(&t, "from")
                        && !is_word(&t, "as")
                })
            }
            _ => false,
        };
        if hit {
            items[k].is_context_keyword = true;
        }
    }
}

fn item(tok: Token<'_>, newlines_before: u32, attach: bool) -> Item<'_> {
    Item {
        tok,
        newlines_before,
        is_function_name: false,
        is_context_keyword: false,
        attach,
    }
}

fn is_operator(t: &Token<'_>, text: &str) -> bool {
    t.kind == TokenKind::Operator && t.text == text
}

/// Something an attached suffix can hang off: a name, literal, parameter,
/// closing parenthesis or a `[ … ]` group.
fn is_operand(t: &Token<'_>) -> bool {
    matches!(
        t.kind,
        TokenKind::Ident(_)
            | TokenKind::Keyword
            | TokenKind::StringLit(_)
            | TokenKind::Number(_)
            | TokenKind::ParamMarker(_)
            | TokenKind::RParen
    )
}

/// Whether the token after `tokens[i]` is an operator written touching it.
fn operator_touches_next(tokens: &[Token<'_>], i: usize) -> bool {
    tokens
        .get(i + 1)
        .is_some_and(|n| n.kind == TokenKind::Operator && n.span.start == tokens[i].span.end)
}

/// Index of the `]` closing the `[` at `open`, if it closes before the
/// statement ends.
fn matching_bracket(tokens: &[Token<'_>], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, t) in tokens.iter().enumerate().skip(open) {
        match t.kind {
            TokenKind::Operator if t.text == "[" => depth += 1,
            TokenKind::Operator if t.text == "]" => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            TokenKind::Semicolon => return None,
            _ => {}
        }
    }
    None
}

/// What a parenthesized group is, decided from surrounding tokens. Drives
/// which layout option applies inside.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum ParenKind {
    /// `(SELECT …)`, `(WITH …)` — subquery/derived table.
    Subquery,
    /// `fn(…)` — argument list of a function call.
    FunctionArgs,
    /// `IN (…)` over plain values (an `IN (SELECT …)` is `Subquery`).
    InList,
    /// The body of `CREATE TABLE t (…)`.
    DdlBody,
    /// A `VALUES (…)` tuple (also the follow-up tuples after commas).
    ValuesTuple,
    /// `INSERT INTO t (…)` column list.
    InsertColumns,
    /// An index key/INCLUDE column list: `CREATE INDEX ix ON t (…)`,
    /// `INCLUDE (…)`, and `PRIMARY KEY (…)` / `UNIQUE (…)` / `INDEX ix (…)`
    /// / `KEY ix (…)` inside a table definition.
    IndexColumns,
    /// Anything else: expression grouping, `OVER (…)`, …
    Plain,
}

pub(super) enum Node<'a> {
    Tok(Item<'a>),
    Paren {
        kind: ParenKind,
        children: Vec<Node<'a>>,
        /// False when the source is unbalanced; the emitter prints the `)`
        /// only when true so token counts stay equal.
        has_close: bool,
        /// Newlines before the `(` in the source.
        open_newlines: u32,
    },
}

impl<'a> Node<'a> {
    pub(super) fn newlines_before(&self) -> u32 {
        match self {
            Node::Tok(i) => i.newlines_before,
            Node::Paren { open_newlines, .. } => *open_newlines,
        }
    }

    pub(super) fn is_word(&self, w: &str) -> bool {
        matches!(self, Node::Tok(i) if is_word(&i.tok, w))
    }

    pub(super) fn is_kind(&self, k: TokenKind) -> bool {
        matches!(self, Node::Tok(i) if i.tok.kind == k)
    }

    pub(super) fn is_comment(&self) -> bool {
        matches!(self, Node::Tok(i) if matches!(i.tok.kind, TokenKind::Comment(_)))
    }

    pub(super) fn contains_line_comment(&self) -> bool {
        match self {
            Node::Tok(i) => matches!(i.tok.kind, TokenKind::Comment(CommentShape::Line)),
            Node::Paren { children, .. } => children.iter().any(|c| c.contains_line_comment()),
        }
    }
}

/// Word match on keywords *and* unquoted identifiers — the dialect keyword
/// tables are deliberately narrow, so structural words (`PARTITION`, `OVER`,
/// `USING`, …) can lex as identifiers.
pub(super) fn is_word(t: &Token, w: &str) -> bool {
    matches!(
        t.kind,
        TokenKind::Keyword | TokenKind::Ident(IdentQuote::None)
    ) && t.text.eq_ignore_ascii_case(w)
}

pub(super) fn word_text<'a>(t: &Token<'a>) -> Option<&'a str> {
    match t.kind {
        TokenKind::Keyword | TokenKind::Ident(IdentQuote::None) => Some(t.text),
        _ => None,
    }
}

/// Build the paren tree for one statement's items.
pub(super) fn build_tree<'a>(items: &[Item<'a>]) -> Vec<Node<'a>> {
    let mut history: Vec<Token<'a>> = Vec::new();
    let mut pos = 0usize;
    build_tree_inner(items, &mut pos, &mut history)
}

fn build_tree_inner<'a>(
    items: &[Item<'a>],
    pos: &mut usize,
    history: &mut Vec<Token<'a>>,
) -> Vec<Node<'a>> {
    let mut out: Vec<Node<'a>> = Vec::new();
    while *pos < items.len() {
        let item = items[*pos];
        match item.tok.kind {
            TokenKind::LParen => {
                let open_newlines = item.newlines_before;
                *pos += 1;
                let kind_hint = classify_paren_from_history(history);
                history.push(item.tok);
                let children = build_tree_inner(items, pos, history);
                let has_close = if *pos < items.len() && items[*pos].tok.kind == TokenKind::RParen {
                    history.push(items[*pos].tok);
                    *pos += 1;
                    true
                } else {
                    false
                };
                let kind = refine_paren_kind(kind_hint, &children);
                if kind == ParenKind::FunctionArgs {
                    mark_function_name(&mut out);
                }
                if kind == ParenKind::IndexColumns
                    && let Some(Node::Tok(item)) = out.last_mut()
                    && is_word(&item.tok, "include")
                {
                    item.is_context_keyword = true;
                }
                out.push(Node::Paren {
                    kind,
                    children,
                    has_close,
                    open_newlines,
                });
            }
            TokenKind::RParen => break,
            _ => {
                *pos += 1;
                if !matches!(item.tok.kind, TokenKind::Comment(_)) {
                    history.push(item.tok);
                }
                out.push(Node::Tok(item));
            }
        }
    }
    out
}

/// Set `is_function_name` on the trailing word node (the call target).
fn mark_function_name(out: &mut [Node]) {
    if let Some(Node::Tok(item)) = out.last_mut()
        && matches!(item.tok.kind, TokenKind::Keyword | TokenKind::Ident(_))
    {
        item.is_function_name = true;
    }
}

/// Classify an `(` from the significant-token history preceding it.
fn classify_paren_from_history(history: &[Token]) -> ParenKind {
    let Some((&prev, _)) = history.split_last() else {
        return ParenKind::Plain;
    };
    let i = history.len() - 1;
    if is_word(&prev, "in") {
        return ParenKind::InList;
    }
    if is_word(&prev, "values") {
        return ParenKind::ValuesTuple;
    }
    // Walk back over a dotted name chain to find the word before it. This
    // runs before the index-list check so a table named `key` or `index`
    // (`INSERT INTO key (…)`, `CREATE TABLE index (…)`) keeps its column
    // list / DDL body.
    let is_name = matches!(prev.kind, TokenKind::Keyword | TokenKind::Ident(_));
    if is_name {
        let mut j = i;
        loop {
            if j == 0 {
                break;
            }
            let before = history[j - 1];
            if before.kind == TokenKind::Dot && j >= 2 {
                j -= 2;
                continue;
            }
            // `CREATE TABLE IF NOT EXISTS name (` is still a DDL body.
            if is_word(&before, "exists") {
                let mut m = j - 1;
                if m >= 1 && is_word(&history[m - 1], "not") {
                    m -= 1;
                }
                if m >= 2 && is_word(&history[m - 1], "if") && is_word(&history[m - 2], "table") {
                    return ParenKind::DdlBody;
                }
            }
            if is_word(&before, "into") {
                return ParenKind::InsertColumns;
            }
            if is_word(&before, "table") {
                return ParenKind::DdlBody;
            }
            break;
        }
    }
    if let Some(kind) = classify_index_paren(history) {
        return kind;
    }
    if is_name {
        if is_function_word(&prev) {
            return ParenKind::FunctionArgs;
        }
        return ParenKind::Plain;
    }
    // `, (` right after a VALUES tuple keeps tuple flavor; the VALUES clause
    // emitter treats Plain parens as tuples anyway.
    ParenKind::Plain
}

/// Index key / INCLUDE column lists, decided from the tokens before `(`.
fn classify_index_paren(history: &[Token]) -> Option<ParenKind> {
    let (&prev, rest) = history.split_last()?;
    let create_index = is_create_index(history);
    // `PRIMARY KEY (`, `UNIQUE (`, `UNIQUE KEY (`, optionally followed by
    // `CLUSTERED` / `NONCLUSTERED`.
    let mut k = rest.len();
    let mut last = prev;
    if is_word(&last, "clustered") || is_word(&last, "nonclustered") {
        last = *rest.get(k.checked_sub(1)?)?;
        k -= 1;
    }
    if is_word(&last, "unique") {
        return Some(ParenKind::IndexColumns);
    }
    if is_word(&last, "key") || is_word(&last, "index") {
        // Not `FOREIGN KEY (…)` (referencing columns), MySQL index hints
        // (`USE INDEX (…)`, `FORCE KEY (…)`, …) and T-SQL ones (`WITH
        // (INDEX (…))`), which list index names, nor a table that happens
        // to be named `key` / `index` (`FROM key (NOLOCK)`, `UPDATE index (…)`).
        let before = k.checked_sub(1).and_then(|j| rest.get(j));
        let excluded = before.is_some_and(|b| {
            matches!(
                b.kind,
                TokenKind::LParen | TokenKind::Comma | TokenKind::Dot
            ) || [
                "foreign", "use", "force", "ignore", "for", "into", "table", "from", "join",
                "update",
            ]
            .iter()
            .any(|w| is_word(b, w))
        });
        return (!excluded).then_some(ParenKind::IndexColumns);
    }
    if create_index && is_word(&prev, "include") {
        return Some(ParenKind::IndexColumns);
    }
    if !matches!(prev.kind, TokenKind::Keyword | TokenKind::Ident(_)) {
        return None;
    }
    // A (dotted) name: the word before it decides.
    let mut j = history.len() - 1;
    while j >= 2 && history[j - 1].kind == TokenKind::Dot {
        j -= 2;
    }
    let before = j.checked_sub(1).map(|b| history[b])?;
    let is_index_name = (is_word(&before, "index") || is_word(&before, "key"))
        && !(is_word(&before, "key") && j >= 2 && is_word(&history[j - 2], "foreign"));
    if is_index_name || (create_index && (is_word(&before, "on") || is_word(&before, "using"))) {
        return Some(ParenKind::IndexColumns);
    }
    None
}

/// The statement is `CREATE [UNIQUE] [CLUSTERED|…] INDEX …`: `INDEX` appears
/// among the words before the first `ON`.
fn is_create_index(history: &[Token]) -> bool {
    let Some(first) = history.first() else {
        return false;
    };
    if !is_word(first, "create") {
        return false;
    }
    for t in &history[1..] {
        if is_word(t, "index") {
            return true;
        }
        if is_word(t, "on") || t.kind == TokenKind::LParen || is_word(t, "table") {
            return false;
        }
    }
    false
}

fn refine_paren_kind(hint: ParenKind, children: &[Node]) -> ParenKind {
    let first_word = children.iter().find_map(|n| match n {
        Node::Tok(i) if !matches!(i.tok.kind, TokenKind::Comment(_)) => word_text(&i.tok),
        _ => None,
    });
    if matches!(first_word, Some(w) if w.eq_ignore_ascii_case("select") || w.eq_ignore_ascii_case("with"))
    {
        return ParenKind::Subquery;
    }
    hint
}

/// Words that read as a call target when directly followed by `(`.
fn is_function_word(t: &Token) -> bool {
    const NOT_FUNCTIONS: &[&str] = &[
        "and",
        "or",
        "not",
        "in",
        "exists",
        "between",
        "like",
        "ilike",
        "on",
        "values",
        "from",
        "where",
        "select",
        "with",
        "as",
        "union",
        "intersect",
        "except",
        "minus",
        "all",
        "distinct",
        "set",
        "when",
        "then",
        "else",
        "case",
        "end",
        "join",
        "using",
        "is",
        "any",
        "some",
        "having",
        "group",
        "order",
        "by",
        "limit",
        "offset",
        "top",
        "into",
        "references",
        "check",
        "default",
        "returning",
        "output",
        "over",
        "partition",
        "primary",
        "foreign",
        "key",
        "unique",
        "index",
        "table",
        "add",
        "constraint",
        "asc",
        "desc",
        "recursive",
        "merge",
        "matched",
        "insert",
        "update",
        "delete",
        "'",
    ];
    match word_text(t) {
        Some(w) => !NOT_FUNCTIONS.iter().any(|n| w.eq_ignore_ascii_case(n)),
        None => matches!(t.kind, TokenKind::Ident(_)),
    }
}
