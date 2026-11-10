//! Cursor-context detection: classify the text immediately before the cursor
//! into a [`CursorContext`] that decides which candidate set symbol completion
//! offers. Pure byte/word scanning over the request text — no snapshot access,
//! no allocation beyond the returned context — so it stays per-keystroke safe.

use sqlly_lang::sqlmodel::{Dialect, ObjectKind, ObjectRef, Span, SqlName};

use super::set_options::SetHead;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CursorContext {
    /// After alias-dot: complete columns of the aliased table.
    AliasDot {
        alias: String,
        prefix: String,
        replace: Span,
    },
    /// After `<schema>.` in an object-name position (FROM/JOIN/INTO/UPDATE):
    /// complete the schema-qualified table-like objects in that schema. The
    /// `replace` span covers the whole `<schema>.<prefix>` token so accepting an
    /// item inserts the full `schema.object` (label/insert parity).
    SchemaDot {
        schema: String,
        kinds: Vec<ObjectKind>,
        prefix: String,
        replace: Span,
    },
    /// After FROM/JOIN/INTO/UPDATE/EXEC: complete object names.
    ObjectName {
        kinds: Vec<ObjectKind>,
        prefix: String,
        replace: Span,
    },
    /// Directly after a session-settings statement head — a statement-initial
    /// `SET`, Oracle's `ALTER SESSION SET`, or SQLite's `PRAGMA` — offer the
    /// names that head can set on the engine. `head_span` covers the typed
    /// head, so a caret after the wrong head for the engine can be steered
    /// to the right one in place (feedback ifb_0fbc). The lists are
    /// dialect-bound and resolved downstream, where the request's dialect is
    /// known.
    SetOption {
        head: SetHead,
        head_span: Span,
        prefix: String,
        replace: Span,
    },
    /// After a complete SELECT list with no FROM yet. `FROM` leads, then
    /// `INTO` where it is legal, then the table-like objects — each of which
    /// inserts `FROM`, a newline and one indent ahead of its name, so picking a
    /// table writes the whole clause. No columns: none can follow a finished
    /// select list. See feedback ifb_9f91.
    AfterSelectList {
        keywords: Vec<&'static str>,
        prefix: String,
        replace: Span,
    },
    /// The cursor sits just after a complete `FROM <table> [alias]` source —
    /// offer the post-FROM clause keywords (WHERE first, then the JOIN family
    /// when the table has foreign keys, GROUP BY, and ORDER BY unless inside a
    /// non-final set-operation branch). `has_aggregate` promotes GROUP BY to
    /// just after WHERE.
    ///
    /// `offer_columns` adds the in-scope columns after the keywords, each
    /// inserting `WHERE`, a newline and one indent ahead of the column (see
    /// feedback ifb_92d9). It is off while the prefix may still be the table's
    /// alias being typed — a non-empty word on the table's own line with no
    /// alias yet.
    PostFromTable {
        has_aggregate: bool,
        order_by_allowed: bool,
        offer_columns: bool,
        prefix: String,
        replace: Span,
    },
    /// Just after a complete test in a WHERE / HAVING / ON predicate
    /// (`x > '2000-01-01'▏`). Only a connector or the next clause can
    /// follow, so `keywords` lists those (AND, OR, then the clauses legal
    /// after this one) and nothing else is offered. It is empty while the
    /// caret still touches the finished operand: a separator must come first.
    /// See feedback ifb_6f67.
    AfterPredicate {
        keywords: Vec<&'static str>,
        prefix: String,
        replace: Span,
    },
    /// General: objects + in-scope columns. `columns_only` marks the positions
    /// where an object name cannot legally appear — the item list of an
    /// `ORDER BY` / `GROUP BY` clause — and suppresses the object half.
    General {
        prefix: String,
        replace: Span,
        columns_only: bool,
    },
    /// Inside the argument list of an `EXEC`/`EXECUTE`/`CALL <proc>` statement,
    /// typing a named-parameter marker (`@…`): complete the routine's declared
    /// parameters. `proc` is the raw (possibly schema-qualified) procedure token
    /// lifted from the statement; it is resolved against the snapshot with the
    /// request dialect/defaults at completion time.
    ExecArg {
        proc: String,
        prefix: String,
        replace: Span,
    },
}

/// Object kinds that may appear in a table-source position
/// (after FROM/JOIN/INTO/UPDATE, or after a `schema.` qualifier there).
const TABLE_SOURCE_KINDS: &[ObjectKind] = &[
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::Synonym,
    ObjectKind::TableFunction,
];

/// Object kinds that may follow EXEC/EXECUTE/CALL.
const ROUTINE_KINDS: &[ObjectKind] = &[
    ObjectKind::Procedure,
    ObjectKind::Function,
    ObjectKind::ScalarFunction,
    ObjectKind::TableFunction,
    ObjectKind::AggregateFunction,
];

/// Clause keywords that, once present after the most recent `select`, mean the
/// cursor is no longer in the bare "select-list needs a FROM" position.
const SELECT_TAIL_CLAUSE_KEYWORDS: &[&str] = &[
    "from", "where", "group", "order", "having", "join", "into", "union", "on", "for", "option",
];

/// Substring after the last whole-word `select` in `before` (already
/// lowercased), or `None` when there is no `select`.
fn last_select_tail(before: &str) -> Option<&str> {
    let bytes = before.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut found: Option<usize> = None;
    let mut search_from = 0usize;
    while let Some(rel) = before[search_from..].find("select") {
        let abs = search_from + rel;
        let after = abs + "select".len();
        let left_ok = abs == 0 || !is_ident(bytes[abs - 1]);
        let right_ok = after >= before.len() || !is_ident(bytes[after]);
        if left_ok && right_ok {
            found = Some(after);
        }
        search_from = after;
    }
    found.map(|idx| &before[idx..])
}

/// True when the cursor sits right after a complete SELECT list that has no
/// FROM (or other clause keyword) yet — the natural next token is `FROM`.
/// `before` is the lowercased, right-trimmed text preceding the cursor prefix.
fn expects_from_keyword(before: &str) -> bool {
    let Some(tail) = last_select_tail(before) else {
        return false;
    };
    let tail_words = tail
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty());
    for w in tail_words {
        if SELECT_TAIL_CLAUSE_KEYWORDS.contains(&w) {
            return false;
        }
    }
    select_list_is_complete(tail)
}

/// Words after which a select-list item is still being written, so the next
/// token cannot be `FROM`: the `SELECT` modifiers, an alias-introducing `AS`,
/// and the operators and `CASE` arms of an unfinished expression.
const SELECT_ITEM_CONTINUATION_WORDS: &[&str] = &[
    "select", "distinct", "all", "top", "percent", "ties", "as", "case", "when", "then", "else",
    "and", "or", "not", "in", "like", "between", "is", "escape", "collate",
];

/// True when `tail` (the lowercased text after the last `select`, up to the
/// cursor prefix) ends on a finished select-list item — `select *`,
/// `select a, b`, `select count(*) n` — rather than midway through one. A
/// trailing comma or operator, an open paren or `CASE`, a `TOP n` with no item
/// after it, or a modifier word (`DISTINCT`, `AS`, `ELSE`, …) all mean a column
/// or expression comes next, not `FROM`.
fn select_list_is_complete(tail: &str) -> bool {
    let trimmed = tail.trim_end();
    if trimmed.trim().is_empty() {
        return false;
    }
    let depth = trimmed.bytes().fold(0i32, |d, b| match b {
        b'(' => d + 1,
        b')' => d - 1,
        _ => d,
    });
    if depth > 0 {
        return false;
    }
    let words: Vec<&str> = trimmed
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    let opened = words.iter().filter(|w| **w == "case").count();
    let closed = words.iter().filter(|w| **w == "end").count();
    if opened > closed {
        return false;
    }
    let last = trimmed.as_bytes()[trimmed.len() - 1];
    if last == b'*' {
        // A bare star (`select *`, `select t.*`, `select distinct *`) is an
        // item; a star after an operand is multiplication awaiting its right
        // side.
        let head = trimmed[..trimmed.len() - 1].trim_end();
        return head.ends_with('.')
            || head.ends_with(',')
            || head.is_empty()
            || last_word(head).is_some_and(|w| SELECT_ITEM_CONTINUATION_WORDS.contains(&w));
    }
    // Only an identifier, number, closing bracket/paren or closing quote can
    // end an item; a comma, operator, `@`, `.` or opener means more follows.
    if matches!(last, b')' | b']' | b'"' | b'\'' | b'`') {
        return true;
    }
    if !(last.is_ascii_alphanumeric() || last == b'_') {
        return false;
    }
    let Some(&last_word) = words.last() else {
        return true;
    };
    if SELECT_ITEM_CONTINUATION_WORDS.contains(&last_word) {
        return false;
    }
    // `TOP 10` / `TOP (10)` with nothing after it still needs its first item.
    let prior = words.len().checked_sub(2).map(|i| words[i]);
    !(prior == Some("top") && last_word.bytes().all(|b| b.is_ascii_digit()))
}

/// The object kind named immediately after the first whole-word `create` in
/// `before` (already lowercased), e.g. `Some("view")` for a `CREATE VIEW`
/// body. Skips an optional `OR ALTER`/`OR REPLACE`. `None` when there is no
/// `create` (a bare statement, CTE, or subquery).
fn defining_create_kind(before: &str) -> Option<&str> {
    let words: Vec<&str> = before
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    let pos = words.iter().position(|w| *w == "create")?;
    let mut idx = pos + 1;
    if words.get(idx) == Some(&"or") {
        idx += 2; // skip `or alter` / `or replace`
    }
    words.get(idx).copied()
}

/// Net unmatched `(` depth in the text preceding the last whole-word `select`
/// in `before`. A value > 0 means that `select` is nested inside parentheses
/// (a subquery, CTE body, derived table, or function-return body).
fn paren_depth_before_last_select(before: &str) -> i32 {
    let Some(tail) = last_select_tail(before) else {
        return 0;
    };
    let after_select = before.len() - tail.len();
    let head = &before[..after_select.saturating_sub("select".len())];
    let mut depth = 0i32;
    for b in head.bytes() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// True when a `SELECT ... INTO` is valid in this position, i.e. the SELECT is
/// in statement position (a standalone query or a procedure body) rather than
/// nested in a subquery/CTE/derived table/function-return, and is not the body
/// of a `CREATE VIEW`/`CREATE FUNCTION` (neither permits `SELECT INTO`).
/// `before` is the caret's statement; `document_before` is everything ahead
/// of the prefix, since the defining `CREATE` may open an earlier region.
fn expects_into_keyword(before: &str, document_before: &str) -> bool {
    if !expects_from_keyword(before) {
        return false;
    }
    match defining_create_kind(document_before) {
        Some("view") | Some("function") => return false,
        _ => {}
    }
    paren_depth_before_last_select(before) == 0
}

/// Words that end an `ORDER BY` / `GROUP BY` item list. Anything in this set
/// puts the cursor back in a position where an object name is legal, so the
/// column-only restriction lifts. Being generous here is the safe direction:
/// a miss only restores the older, broader candidate set.
const COLUMN_ONLY_BOUNDARY_WORDS: &[&str] = &[
    "select",
    "from",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "outer",
    "on",
    "into",
    "update",
    "delete",
    "insert",
    "exec",
    "execute",
    "call",
    "union",
    "except",
    "intersect",
    "values",
    "case",
    "when",
    "then",
    "else",
    "set",
    "apply",
    "pivot",
    "unpivot",
    "for",
    "option",
    "over",
    "partition",
    "using",
    "merge",
    "with",
    "declare",
    "if",
    "while",
    "begin",
    "end",
    "return",
    "print",
    "offset",
    "fetch",
    "go",
];

/// True when the cursor sits inside a clause where a catalog object name
/// cannot appear — the item list of an `ORDER BY` or `GROUP BY`, or the
/// predicate of a `WHERE` or `HAVING` (feedback ifb_56ee). Only the in-scope
/// columns are offered there, never a table, procedure, or function.
///
/// `before` is the lowercased text preceding the identifier prefix. The scan
/// walks it left to right so the *last* clause seen wins: a later `select`
/// (subquery, next statement) or a `;` clears the state that an earlier
/// `ORDER BY` or `WHERE` set.
fn in_column_only_clause(before: &str) -> bool {
    let bytes = before.as_bytes();
    let mut column_only = false;
    let mut prev_word: Option<&str> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b';' {
            column_only = false;
            prev_word = None;
            i += 1;
            continue;
        }
        if !is_ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident_byte(bytes[i]) {
            i += 1;
        }
        let word = &before[start..i];
        if (word == "by" && matches!(prev_word, Some("order") | Some("group")))
            || word == "where"
            || word == "having"
        {
            column_only = true;
        } else if COLUMN_ONLY_BOUNDARY_WORDS.contains(&word) {
            column_only = false;
        }
        prev_word = Some(word);
    }
    column_only
}

/// Clause/operator words that mean the cursor has moved past a bare FROM table
/// source (so it is no longer in the "post-FROM table" position).
const FROM_SOURCE_BOUNDARY_WORDS: &[&str] = &[
    "where",
    "group",
    "order",
    "having",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "outer",
    "on",
    "union",
    "except",
    "intersect",
    "select",
    "from",
    "into",
    "pivot",
    "unpivot",
    "apply",
    "for",
    "option",
    "set",
    "values",
    "with",
    "go",
    "as",
];

/// JOIN clause keywords offered after a FROM table source that has FKs.
pub(crate) const POST_FROM_JOIN_KEYWORDS: &[&str] = &[
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "FULL JOIN",
    "FULL OUTER JOIN",
];

const AGGREGATE_FUNCS: &[&str] = &["count", "sum", "avg", "min", "max"];

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Byte index just past the last whole-word `word` in `haystack` (already
/// lowercased), or `None` when absent.
fn index_after_last_word(haystack: &str, word: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut found = None;
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(word) {
        let abs = from + rel;
        let after = abs + word.len();
        let left_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        let right_ok = after >= haystack.len() || !is_ident_byte(bytes[after]);
        if left_ok && right_ok {
            found = Some(after);
        }
        from = after;
    }
    found
}

/// Whole-word membership test (`haystack` already lowercased).
fn contains_whole_word(haystack: &str, word: &str) -> bool {
    index_after_last_word(haystack, word).is_some()
}

/// True when the SELECT list contains an aggregate call like `COUNT(`.
fn contains_aggregate(select_list: &str) -> bool {
    let lower = select_list.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for func in AGGREGATE_FUNCS {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(func) {
            let abs = from + rel;
            let after = abs + func.len();
            let left_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
            let mut j = after;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if left_ok && j < bytes.len() && bytes[j] == b'(' {
                return true;
            }
            from = after;
        }
    }
    false
}

/// Whether a set operator follows the cursor — the SELECT at the cursor is not
/// the final query, so an ORDER BY here would be illegal.
fn set_operator_follows(after: &str) -> bool {
    let lower = after.to_ascii_lowercase();
    ["union", "except", "intersect"]
        .iter()
        .any(|op| contains_whole_word(&lower, op))
}

/// Aggregate/ordering facts about a cursor positioned just after a complete
/// `FROM <table> [alias]` source.
struct PostFromFacts {
    has_aggregate: bool,
    order_by_allowed: bool,
    /// The source already carries its alias (`FROM t x`, `FROM t AS x`).
    has_alias: bool,
}

/// Detect a cursor sitting just after a complete `FROM <table> [alias]` source.
/// `before` is the lowercased, right-trimmed text of the caret's statement
/// preceding the cursor prefix; `after` is the rest of that statement past
/// the cursor.
fn detect_post_from(after: &str, before: &str) -> Option<PostFromFacts> {
    let from_idx = index_after_last_word(before, "from")?;
    let tail = before[from_idx..].trim();
    if tail.is_empty() || tail.contains([',', '(', ')']) {
        return None;
    }
    let toks: Vec<&str> = tail.split_whitespace().collect();
    // First token is the table identifier; it must not be a clause boundary.
    if FROM_SOURCE_BOUNDARY_WORDS.contains(&toks[0]) {
        return None;
    }
    // Remaining tokens: an optional `AS` plus at most one alias, none of which
    // may be a clause boundary word.
    let mut alias_count = 0;
    for t in &toks[1..] {
        if *t == "as" {
            continue;
        }
        if FROM_SOURCE_BOUNDARY_WORDS.contains(t) {
            return None;
        }
        alias_count += 1;
    }
    if alias_count > 1 {
        return None;
    }

    // SELECT list of the current query (between its SELECT and this FROM).
    let head = &before[..from_idx.saturating_sub("from".len())];
    let select_list = index_after_last_word(head, "select")
        .map(|i| &head[i..])
        .unwrap_or("");
    let has_aggregate = contains_aggregate(select_list);

    let order_by_allowed = !set_operator_follows(after);

    Some(PostFromFacts {
        has_aggregate,
        order_by_allowed,
        has_alias: alias_count == 1,
    })
}

/// Find the procedure name of the `EXEC`/`EXECUTE`/`CALL` statement enclosing
/// the cursor, given the caret's statement text up to (but excluding) a typed
/// `@` parameter marker. Returns the raw, possibly schema-qualified, procedure
/// token (e.g. `dbo.ImportAccounts`) or `None` when the caret is not inside an
/// exec argument list. A statement terminator (`;`) between the keyword and the
/// marker ends the search — we are no longer in that statement.
fn enclosing_exec_proc(head: &str) -> Option<String> {
    let lower = head.to_ascii_lowercase();
    let lb = lower.as_bytes();
    // Latest whole-word EXEC/EXECUTE/CALL keyword end offset.
    let mut kw_end: Option<usize> = None;
    for kw in ["execute", "exec", "call"] {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(kw) {
            let idx = from + rel;
            let after = idx + kw.len();
            let before_ok = idx == 0 || !is_ident_byte(lb[idx - 1]);
            let after_ok = after >= lb.len() || !is_ident_byte(lb[after]);
            if before_ok && after_ok {
                kw_end = Some(kw_end.map_or(after, |e| e.max(after)));
            }
            from = idx + kw.len();
        }
    }
    let kw_end = kw_end?;
    // A new statement boundary after the keyword means we left the exec.
    if head[kw_end..].contains(';') {
        return None;
    }
    // First identifier token after the keyword (schema-qualified allowed).
    let token: String = head[kw_end..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if token.is_empty() { None } else { Some(token) }
}

/// Parse a raw (possibly schema-qualified) object token into an [`ObjectRef`].
/// `schema.name` splits on the last dot into namespace + name; a bare token
/// resolves against the request's default namespace at lookup time.
pub(crate) fn parse_object_ref(token: &str, dialect: Dialect) -> ObjectRef {
    match token.rsplit_once('.') {
        Some((ns, name)) if !ns.is_empty() && !name.is_empty() => ObjectRef::new(
            None,
            Some(SqlName::new(ns.to_string(), dialect, false)),
            SqlName::new(name.to_string(), dialect, false),
        ),
        _ => ObjectRef::bare(SqlName::new(token.to_string(), dialect, false)),
    }
}

// ---------------------------------------------------------------------------
// cursor_context: the precedence-ordered decision ladder
// ---------------------------------------------------------------------------

/// The identifier prefix ending at the cursor: `(start, prefix, replace)`
/// where `start` is the byte offset the prefix begins at, `prefix` is the
/// typed identifier text, and `replace` is the span a completion overwrites.
fn ident_prefix(upto: &str, cursor: usize) -> (usize, String, Span) {
    let bytes = upto.as_bytes();
    let mut start = upto.len();
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    (start, upto[start..].to_string(), Span::new(start, cursor))
}

/// Last whitespace/punctuation-delimited word of `before` (already lowercased).
/// `Some("")` when `before` ends in a non-identifier character.
fn last_word(before: &str) -> Option<&str> {
    before
        .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
}

/// Ladder step 1 — dotted qualifier: the prefix is preceded by `<ident>.`.
/// When that identifier sits directly after an object-name keyword
/// (`FROM HR.`, `JOIN HR.`, `INTO HR.`, `UPDATE HR.`), the leading token is a
/// *schema* qualifier, not a table alias: list the table-like objects in that
/// schema. The replace span covers only the name prefix *after* the dot, so
/// the already-typed `schema.` stays put and the completion inserts just the
/// bare object name — never re-emitting the schema (which would produce
/// `Sales.Sales.Order`). Otherwise it is an alias-dot column completion.
fn detect_dotted_qualifier(
    upto: &str,
    start: usize,
    prefix: &str,
    replace: Span,
) -> Option<CursorContext> {
    let bytes = upto.as_bytes();
    if start == 0 || bytes[start - 1] != b'.' {
        return None;
    }
    let mut a_start = start - 1;
    while a_start > 0 && is_ident_byte(bytes[a_start - 1]) {
        a_start -= 1;
    }
    let alias = upto[a_start..start - 1].to_string();
    if alias.is_empty() {
        return None;
    }
    let before_alias = upto[..a_start].trim_end().to_ascii_lowercase();
    if matches!(
        last_word(&before_alias),
        Some("from") | Some("join") | Some("into") | Some("update")
    ) {
        return Some(CursorContext::SchemaDot {
            schema: alias,
            kinds: TABLE_SOURCE_KINDS.to_vec(),
            prefix: prefix.to_string(),
            replace,
        });
    }
    Some(CursorContext::AliasDot {
        alias,
        prefix: prefix.to_string(),
        replace,
    })
}

/// Ladder step 2 — `@prefix` inside an EXEC/EXECUTE/CALL argument list →
/// complete the named parameters of the routine being invoked. The `@` sigil
/// is the trigger; we look back for the enclosing EXEC and its procedure name.
/// The replace span covers the `@` so accepting overwrites the whole marker.
fn detect_exec_arg(
    upto: &str,
    stmt_start: usize,
    start: usize,
    prefix: &str,
    cursor: usize,
) -> Option<CursorContext> {
    let bytes = upto.as_bytes();
    if start == 0 || bytes[start - 1] != b'@' {
        return None;
    }
    let proc = enclosing_exec_proc(&upto[stmt_start.min(start - 1)..start - 1])?;
    Some(CursorContext::ExecArg {
        proc,
        prefix: format!("@{prefix}"),
        replace: Span::new(start - 1, cursor),
    })
}

/// Ladder step 3 — the word immediately before the prefix is an object-name
/// keyword: FROM/JOIN/INTO/UPDATE take table-like objects, EXEC/EXECUTE/CALL
/// take routines.
fn detect_object_name_keyword(
    word_before: Option<&str>,
    prefix: &str,
    replace: Span,
) -> Option<CursorContext> {
    let kinds: &[ObjectKind] = match word_before {
        Some("from") | Some("join") | Some("into") | Some("update") => TABLE_SOURCE_KINDS,
        Some("exec") | Some("execute") | Some("call") => ROUTINE_KINDS,
        _ => return None,
    };
    Some(CursorContext::ObjectName {
        kinds: kinds.to_vec(),
        prefix: prefix.to_string(),
        replace,
    })
}

/// Ladder step 3b — the words before the prefix are a session-settings
/// statement head: a statement-initial `SET`, `ALTER SESSION SET`, or
/// `PRAGMA`. What follows is a settable name rather than a column or an
/// object.
///
/// For `SET` the discriminator is `UPDATE`/`MERGE`: `UPDATE t SET ▏` and
/// `MERGE … WHEN MATCHED THEN UPDATE SET ▏` want columns, and every other
/// `SET` is the settings statement. A `;` or a `GO` between the `UPDATE` and
/// the `SET` ends that statement, so the `SET` after it is a settings
/// statement again.
///
/// `SET @▏` is a variable assignment, not an option — the `@` sigil declines
/// the position.
fn detect_set_option(
    upto: &str,
    stmt_start: usize,
    start: usize,
    before: &str,
    prefix: &str,
    replace: Span,
) -> Option<CursorContext> {
    if start > 0 && upto.as_bytes()[start - 1] == b'@' {
        return None;
    }
    let (head, head_len) = match last_word(before) {
        Some("set") => {
            let ahead = before[..before.len() - "set".len()].trim_end();
            let alter_session = last_word(ahead) == Some("session")
                && last_word(ahead[..ahead.len() - "session".len()].trim_end()) == Some("alter");
            if alter_session {
                let alter_start =
                    ahead[..ahead.len() - "session".len()].trim_end().len() - "alter".len();
                (SetHead::AlterSessionSet, before.len() - alter_start)
            } else {
                (SetHead::Set, "set".len())
            }
        }
        Some("pragma") => (SetHead::Pragma, "pragma".len()),
        _ => return None,
    };
    // `before` ends with the head itself; weigh only what came before it.
    if head == SetHead::Set && in_update_set_list(&before[..before.len() - head_len]) {
        return None;
    }
    // `before` is the statement text lowercased (ASCII only, so byte offsets
    // hold) and right-trimmed: its end is the end of the typed head.
    let head_end = stmt_start + before.len();
    Some(CursorContext::SetOption {
        head,
        head_span: Span::new(head_end - head_len, head_end),
        prefix: prefix.to_string(),
        replace,
    })
}

/// True when an `UPDATE` or `MERGE` is still open in `before` — scanned left to
/// right so the last statement wins, with `;` and `GO` clearing the state.
fn in_update_set_list(before: &str) -> bool {
    let bytes = before.as_bytes();
    let mut open = false;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b';' {
            open = false;
            i += 1;
            continue;
        }
        if !is_ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident_byte(bytes[i]) {
            i += 1;
        }
        match &before[start..i] {
            "update" | "merge" => open = true,
            "go" => open = false,
            _ => {}
        }
    }
    open
}

/// Ladder step 5 — after a complete SELECT list with no FROM yet, the natural
/// next token is FROM (always) and INTO (only in statement position — not in
/// a subquery/CTE/function/view body). The table-like objects that may follow
/// the FROM are offered too, downstream.
fn detect_clause_keyword(
    upto: &str,
    start: usize,
    before: &str,
    prefix: &str,
    replace: Span,
) -> Option<CursorContext> {
    if !expects_from_keyword(before) {
        return None;
    }
    let mut keywords = vec!["FROM"];
    let document_before = upto[..start].to_ascii_lowercase();
    if expects_into_keyword(before, &document_before) {
        keywords.push("INTO");
    }
    Some(CursorContext::AfterSelectList {
        keywords,
        prefix: prefix.to_string(),
        replace,
    })
}

/// Which predicate clause the cursor is in, for [`detect_after_predicate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PredicateClause {
    Where,
    Having,
    On,
}

/// Words that end a WHERE / HAVING / ON predicate.
const PREDICATE_BOUNDARY_WORDS: &[&str] = &[
    "select",
    "from",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "outer",
    "group",
    "order",
    "into",
    "update",
    "delete",
    "insert",
    "exec",
    "execute",
    "call",
    "union",
    "except",
    "intersect",
    "set",
    "values",
    "option",
    "for",
    "go",
    "window",
    "qualify",
    "limit",
];

/// The predicate clause `before` (lowercased text ahead of the prefix) ends
/// in, with the byte offset just past its keyword, or `None` when the text
/// is not inside a WHERE / HAVING / ON predicate.
fn predicate_clause(before: &str) -> Option<(PredicateClause, usize)> {
    let bytes = before.as_bytes();
    let mut clause = None;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b';' {
            clause = None;
            i += 1;
            continue;
        }
        if bytes[i] == b'\'' {
            // Skip string literals so words inside them are not keywords.
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if !is_ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident_byte(bytes[i]) {
            i += 1;
        }
        match &before[start..i] {
            "where" => clause = Some((PredicateClause::Where, i)),
            "having" => clause = Some((PredicateClause::Having, i)),
            "on" => clause = Some((PredicateClause::On, i)),
            word if PREDICATE_BOUNDARY_WORDS.contains(&word) => clause = None,
            _ => {}
        }
    }
    clause
}

/// The last test of a predicate body (`a = 1 and b > '2'` → `b > '2'`):
/// the text after the last top-level AND / OR (an AND that closes a BETWEEN
/// stays inside its test) or after an unmatched `(`. `None` when the body's
/// quotes or parentheses are unbalanced in a way that leaves the test open.
fn last_predicate_test(body: &str) -> Option<&str> {
    let bytes = body.as_bytes();
    // Start offsets of the test at each open-paren depth.
    let mut starts = vec![0usize];
    let mut between_pending = vec![false];
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None; // unterminated string: still typing the value
                }
                i += 1;
            }
            b'(' => {
                i += 1;
                starts.push(i);
                between_pending.push(false);
            }
            b')' => {
                i += 1;
                if starts.len() == 1 {
                    return None; // closes a paren opened before the clause
                }
                starts.pop();
                between_pending.pop();
            }
            b if is_ident_byte(b) => {
                let word_start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                let word = &body[word_start..i];
                let depth = starts.len() - 1;
                if word.eq_ignore_ascii_case("between") {
                    between_pending[depth] = true;
                } else if word.eq_ignore_ascii_case("and") && between_pending[depth] {
                    between_pending[depth] = false;
                } else if word.eq_ignore_ascii_case("and") || word.eq_ignore_ascii_case("or") {
                    starts[depth] = i;
                }
            }
            _ => i += 1,
        }
    }
    if *between_pending.last()? {
        return None; // `x BETWEEN 1` still needs its `AND 2`
    }
    Some(body[*starts.last()?..].trim())
}

/// Words that cannot end a finished test: its right-hand side is missing.
const OPEN_TEST_TAIL_WORDS: &[&str] = &[
    "and", "or", "not", "is", "in", "like", "between", "escape", "exists", "all", "any", "some",
    "collate",
];

/// True when `test` (one predicate test, lowercased) is complete: it has a
/// comparison and a finished right-hand side (`a = 1`, `a > 'x'`,
/// `a IS NULL`, `a IN (1, 2)`, `a BETWEEN 1 AND 2`, `EXISTS (…)`).
fn predicate_test_is_complete(test: &str) -> bool {
    let test = test.trim();
    let Some(last) = test.bytes().last() else {
        return false;
    };
    if matches!(
        last,
        b'=' | b'<' | b'>' | b'!' | b'+' | b'-' | b'*' | b'/' | b'%' | b',' | b'.'
    ) {
        return false;
    }
    if is_ident_byte(last) {
        let tail = last_word(test).unwrap_or("");
        if OPEN_TEST_TAIL_WORDS.contains(&tail) {
            return false;
        }
    }
    let has_operator = test.contains(['=', '<', '>'])
        || [" like ", " in ", " in(", " between ", " is "]
            .iter()
            .any(|op| test.contains(op));
    has_operator || test.starts_with("exists") || test.starts_with("not exists")
}

/// Ladder step 5b — just after a complete test in a WHERE / HAVING / ON
/// predicate. With whitespace between the test and the caret, offer AND / OR
/// and the clauses that may follow; with the caret still touching the test
/// (`'2000-01-01'▏`), offer nothing. A word typed straight onto the test
/// (`= abc▏`) is its right-hand side and is left to the general step.
fn detect_after_predicate(
    upto: &str,
    start: usize,
    before: &str,
    prefix: &str,
    replace: Span,
) -> Option<CursorContext> {
    let head = &upto[..start];
    let separated = head.len() > head.trim_end().len();
    if !separated && !prefix.is_empty() {
        return None;
    }
    let (clause, body_start) = predicate_clause(before)?;
    let test = last_predicate_test(&before[body_start..])?;
    if !predicate_test_is_complete(test) {
        return None;
    }
    let keywords = if !separated {
        Vec::new()
    } else {
        match clause {
            PredicateClause::Where => vec!["AND", "OR", "GROUP BY", "ORDER BY"],
            PredicateClause::Having => vec!["AND", "OR", "ORDER BY"],
            PredicateClause::On => {
                let mut keywords = vec!["AND", "OR", "WHERE"];
                keywords.extend_from_slice(POST_FROM_JOIN_KEYWORDS);
                keywords.extend_from_slice(&["GROUP BY", "ORDER BY"]);
                keywords
            }
        }
    };
    Some(CursorContext::AfterPredicate {
        keywords,
        prefix: prefix.to_string(),
        replace,
    })
}

/// The keywords legal at `cursor` just after a complete predicate test (see
/// [`CursorContext::AfterPredicate`]), per the full ladder, or `None`
/// anywhere else. Empty while the caret still touches the test.
pub(crate) fn after_predicate_keywords(text: &str, cursor: usize) -> Option<Vec<&'static str>> {
    match cursor_context(text, cursor) {
        CursorContext::AfterPredicate { keywords, .. } => Some(keywords),
        _ => None,
    }
}

/// The clause keywords legal at `cursor` when it sits after a finished SELECT
/// list with no FROM yet, per the full ladder (so an earlier step claiming the
/// position wins), or `None` anywhere else.
pub(crate) fn after_select_list_keywords(text: &str, cursor: usize) -> Option<Vec<&'static str>> {
    match cursor_context(text, cursor) {
        CursorContext::AfterSelectList { keywords, .. } => Some(keywords),
        _ => None,
    }
}

/// Facts about a post-FROM clause position at `cursor` (see
/// [`CursorContext::PostFromTable`]), per the full ladder, or `None` anywhere
/// else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PostFromClause {
    pub has_aggregate: bool,
    pub order_by_allowed: bool,
    pub offer_columns: bool,
}

pub(crate) fn post_from_clause(text: &str, cursor: usize) -> Option<PostFromClause> {
    match cursor_context(text, cursor) {
        CursorContext::PostFromTable {
            has_aggregate,
            order_by_allowed,
            offer_columns,
            ..
        } => Some(PostFromClause {
            has_aggregate,
            order_by_allowed,
            offer_columns,
        }),
        _ => None,
    }
}

/// Inspect the text immediately before the cursor.
///
/// This is a precedence-ordered decision ladder: each step either claims the
/// position and returns, or passes to the next. The order is load-bearing —
/// e.g. `FROM HR.` must be claimed as a schema-dot before the bare `from`
/// keyword rule sees it, and a post-FROM position must be claimed before the
/// select-list FROM/INTO keyword rule runs.
///
/// Every look-back is bounded to the statement the caret is in (see
/// [`crate::intellisense::statement_range`]): after `SELECT * FROM t AS x⏎⏎
/// update ▏` the `update` opens a new statement, so the SELECT's FROM source
/// is not the post-FROM position here and its alias does not make `update`
/// a second alias (feedback ifb_3798).
pub(crate) fn cursor_context(text: &str, cursor: usize) -> CursorContext {
    let cursor = cursor.min(text.len());
    let upto = &text[..cursor];

    // Identifier prefix under the cursor.
    let (start, prefix, replace) = ident_prefix(upto, cursor);

    // 1. `qualifier.prefix` — schema-dot (object position) or alias-dot.
    if let Some(ctx) = detect_dotted_qualifier(upto, start, &prefix, replace) {
        return ctx;
    }

    // The caret's statement bounds every look-back and look-ahead below.
    let statement = crate::intellisense::statement_range(text, cursor, Dialect::SqlServer);
    let stmt_start = statement.start.min(start);
    let stmt_end = statement.end.max(cursor);

    // 2. `@prefix` inside an EXEC/EXECUTE/CALL argument list.
    if let Some(ctx) = detect_exec_arg(upto, stmt_start, start, &prefix, cursor) {
        return ctx;
    }

    // Last keyword before the prefix decides the candidate set.
    let before = upto[stmt_start..start].trim_end().to_ascii_lowercase();
    let after = &text[cursor..stmt_end];

    // 3. Directly after FROM/JOIN/INTO/UPDATE/EXEC/EXECUTE/CALL.
    if let Some(ctx) = detect_object_name_keyword(last_word(&before), &prefix, replace) {
        return ctx;
    }

    // 3b. Directly after a statement-initial SET: session options.
    if let Some(ctx) = detect_set_option(upto, stmt_start, start, &before, &prefix, replace) {
        return ctx;
    }

    // 4. Cursor just after a complete `FROM <table> [alias]` source:
    // offer post-FROM clause keywords (JOIN/WHERE/GROUP BY/ORDER BY).
    if let Some(facts) = detect_post_from(after, &before) {
        // A word typed on the table's own line, with no alias yet, may be
        // that alias; a line break (or an alias already in place) rules that
        // out.
        let gap = &upto[upto[..start].trim_end().len()..start];
        let offer_columns = prefix.is_empty() || facts.has_alias || gap.contains('\n');
        return CursorContext::PostFromTable {
            has_aggregate: facts.has_aggregate,
            order_by_allowed: facts.order_by_allowed,
            offer_columns,
            prefix,
            replace,
        };
    }

    // 5. After a complete SELECT list with no FROM yet: FROM (and INTO).
    if let Some(ctx) = detect_clause_keyword(upto, start, &before, &prefix, replace) {
        return ctx;
    }

    // 5b. After a complete predicate test: connectors and next clauses only.
    if let Some(ctx) = detect_after_predicate(upto, start, &before, &prefix, replace) {
        return ctx;
    }

    // 6. Fallback: objects + in-scope columns — columns alone inside an
    // ORDER BY / GROUP BY item list.
    CursorContext::General {
        prefix,
        replace,
        columns_only: in_column_only_clause(&before),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(text: &str) -> CursorContext {
        cursor_context(text, text.len())
    }

    // --- scanning helpers -------------------------------------------------

    #[test]
    fn last_select_tail_requires_whole_word() {
        assert_eq!(last_select_tail("select a, b"), Some(" a, b"));
        assert_eq!(last_select_tail("selection of"), None);
        // The *last* select wins.
        assert_eq!(last_select_tail("select 1; select x"), Some(" x"));
    }

    #[test]
    fn expects_from_needs_a_select_list_and_no_clause_yet() {
        assert!(expects_from_keyword("select a, b"));
        assert!(expects_from_keyword("select *"));
        assert!(!expects_from_keyword("select"));
        assert!(!expects_from_keyword("select a from t"));
        assert!(!expects_from_keyword("update t set"));
    }

    #[test]
    fn defining_create_kind_skips_or_alter() {
        assert_eq!(
            defining_create_kind("create view v as select 1"),
            Some("view")
        );
        assert_eq!(
            defining_create_kind("create or alter procedure p as select 1"),
            Some("procedure")
        );
        assert_eq!(defining_create_kind("select 1"), None);
    }

    #[test]
    fn into_only_in_statement_position() {
        // Statement-position SELECT: INTO allowed.
        let into = |before: &str| expects_into_keyword(before, before);
        assert!(into("select a, b"));
        // Subquery (nested in parens): no INTO.
        assert!(!into("select x from (select a, b"));
        // View/function bodies: no INTO.
        assert!(!into("create view v as select a"));
        assert!(!into("create function f() returns int as select a"));
        // The defining CREATE may open an earlier region than the caret's
        // statement: it is read from the whole document.
        assert!(!expects_into_keyword(
            "select a",
            "create function f() returns int as begin select a"
        ));
    }

    #[test]
    fn enclosing_exec_proc_lifts_qualified_token() {
        assert_eq!(
            enclosing_exec_proc("exec dbo.ImportAccounts "),
            Some("dbo.ImportAccounts".into())
        );
        assert_eq!(
            enclosing_exec_proc("execute P1 @a = 1, "),
            Some("P1".into())
        );
        // A statement terminator ends the enclosing exec.
        assert_eq!(enclosing_exec_proc("exec P1; select "), None);
        assert_eq!(enclosing_exec_proc("select 1 "), None);
    }

    #[test]
    fn detect_post_from_facts() {
        let text = "SELECT COUNT(x) FROM t ";
        let before = text.trim_end().to_ascii_lowercase();
        let facts = detect_post_from("", &before).expect("post-FROM");
        assert!(facts.has_aggregate);
        assert!(facts.order_by_allowed);

        let text = "SELECT a FROM t  UNION SELECT b FROM u";
        let cursor = "SELECT a FROM t ".len();
        let before = text[..cursor].trim_end().to_ascii_lowercase();
        let facts = detect_post_from(&text[cursor..], &before).expect("post-FROM");
        assert!(!facts.has_aggregate);
        assert!(!facts.order_by_allowed, "set operator follows the cursor");

        // Past a clause boundary: no longer post-FROM.
        let text = "SELECT a FROM t WHERE ";
        let before = text.trim_end().to_ascii_lowercase();
        assert!(detect_post_from("", &before).is_none());
    }

    // --- the decision ladder ----------------------------------------------

    #[test]
    fn ladder_alias_dot() {
        match ctx("SELECT * FROM t x WHERE x.na") {
            CursorContext::AliasDot { alias, prefix, .. } => {
                assert_eq!(alias, "x");
                assert_eq!(prefix, "na");
            }
            other => panic!("expected AliasDot, got {other:?}"),
        }
    }

    #[test]
    fn ladder_schema_dot_after_from() {
        match ctx("SELECT * FROM HR.Dep") {
            CursorContext::SchemaDot { schema, prefix, .. } => {
                assert_eq!(schema, "HR");
                assert_eq!(prefix, "Dep");
            }
            other => panic!("expected SchemaDot, got {other:?}"),
        }
    }

    #[test]
    fn ladder_exec_arg_marker() {
        match ctx("EXEC dbo.ImportAccounts @Sou") {
            CursorContext::ExecArg { proc, prefix, .. } => {
                assert_eq!(proc, "dbo.ImportAccounts");
                assert_eq!(prefix, "@Sou");
            }
            other => panic!("expected ExecArg, got {other:?}"),
        }
    }

    #[test]
    fn ladder_set_heads_carry_their_kind_and_span() {
        match ctx("SET NOC") {
            CursorContext::SetOption {
                head,
                head_span,
                prefix,
                ..
            } => {
                assert_eq!(head, SetHead::Set);
                assert_eq!(head_span, Span::new(0, 3));
                assert_eq!(prefix, "NOC");
            }
            other => panic!("expected SetOption, got {other:?}"),
        }
        match ctx("SELECT 1;\nalter session set nls_d") {
            CursorContext::SetOption {
                head, head_span, ..
            } => {
                assert_eq!(head, SetHead::AlterSessionSet);
                assert_eq!(head_span, Span::new(10, 27));
            }
            other => panic!("expected SetOption, got {other:?}"),
        }
        match ctx("PRAGMA ") {
            CursorContext::SetOption {
                head, head_span, ..
            } => {
                assert_eq!(head, SetHead::Pragma);
                assert_eq!(head_span, Span::new(0, 6));
            }
            other => panic!("expected SetOption, got {other:?}"),
        }
        // Assignments and UPDATE … SET lists are not settings positions.
        assert!(!matches!(ctx("SET @x"), CursorContext::SetOption { .. }));
        assert!(!matches!(
            ctx("UPDATE t SET "),
            CursorContext::SetOption { .. }
        ));
    }

    #[test]
    fn ladder_object_name_after_keywords() {
        match ctx("SELECT * FROM Ord") {
            CursorContext::ObjectName { kinds, prefix, .. } => {
                assert_eq!(prefix, "Ord");
                assert!(kinds.contains(&ObjectKind::Table));
                assert!(kinds.contains(&ObjectKind::View));
            }
            other => panic!("expected ObjectName, got {other:?}"),
        }
        match ctx("EXEC Imp") {
            CursorContext::ObjectName { kinds, .. } => {
                assert!(kinds.contains(&ObjectKind::Procedure));
                assert!(!kinds.contains(&ObjectKind::Table));
            }
            other => panic!("expected ObjectName, got {other:?}"),
        }
    }

    /// `ORDER BY` / `GROUP BY` item lists and `WHERE` / `HAVING` predicates
    /// are column-only positions, and the restriction lifts as soon as the
    /// clause ends (feedback ifb_2592…, ifb_56ee…).
    #[test]
    fn general_is_columns_only_inside_order_and_group_by() {
        let columns_only = |text: &str| match ctx(text) {
            CursorContext::General { columns_only, .. } => columns_only,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(columns_only("SELECT * FROM Shared.Terminal ORDER BY "));
        assert!(columns_only(
            "SELECT * FROM Shared.Terminal ORDER BY Name, "
        ));
        assert!(columns_only("SELECT COUNT(*) FROM t GROUP BY "));
        // Not a column-only position: a nested query, past OFFSET/FETCH, or in
        // a later statement.
        assert!(columns_only("SELECT a, b FROM t WHERE "));
        assert!(columns_only("SELECT a, b FROM t WHERE a = 1 AND "));
        assert!(columns_only("SELECT a, COUNT(*) FROM t GROUP BY a HAVING "));
        // A subquery inside the ORDER BY is filtered by its own WHERE.
        assert!(columns_only(
            "SELECT * FROM t ORDER BY (SELECT TOP 1 x FROM u WHERE "
        ));
        // Not a column-only position: a nested query's select list, or a
        // later statement.
        assert!(!columns_only("SELECT * FROM t WHERE a IN (SELECT TOP 1 "));
        assert!(!columns_only("SELECT * FROM t ORDER BY a; SELECT "));
        assert!(!columns_only("SELECT * FROM t WHERE a = 1; SELECT "));
    }

    #[test]
    fn ladder_post_from_beats_general() {
        match ctx("SELECT a FROM t ") {
            CursorContext::PostFromTable {
                order_by_allowed, ..
            } => assert!(order_by_allowed),
            other => panic!("expected PostFromTable, got {other:?}"),
        }
    }

    #[test]
    fn ladder_select_list_offers_from_and_into() {
        match ctx("SELECT a, b ") {
            CursorContext::AfterSelectList { keywords, .. } => {
                assert_eq!(keywords, vec!["FROM", "INTO"]);
            }
            other => panic!("expected AfterSelectList, got {other:?}"),
        }
        // Inside a view body INTO is not offered.
        match ctx("CREATE VIEW v AS SELECT a, b ") {
            CursorContext::AfterSelectList { keywords, .. } => {
                assert_eq!(keywords, vec!["FROM"]);
            }
            other => panic!("expected AfterSelectList, got {other:?}"),
        }
    }

    #[test]
    fn ladder_falls_back_to_general() {
        match ctx("SELECT co") {
            CursorContext::General { prefix, .. } => assert_eq!(prefix, "co"),
            other => panic!("expected General, got {other:?}"),
        }
    }

    #[test]
    fn parse_object_ref_splits_on_last_dot() {
        let r = parse_object_ref("dbo.Proc", Dialect::SqlServer);
        assert_eq!(r.namespace.as_ref().map(|n| n.spelling()), Some("dbo"));
        assert_eq!(r.name.spelling(), "Proc");
        let bare = parse_object_ref("Proc", Dialect::SqlServer);
        assert!(bare.namespace.is_none());
    }
}
