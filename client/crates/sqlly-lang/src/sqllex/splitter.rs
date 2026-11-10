//! Statement splitting per dialect rules (ADR-011 §2.4): `GO` batches
//! (SQL Server), `;` terminators, PL/SQL `/` on its own line (Oracle),
//! `DELIMITER` redefinition (MySQL). Operates on the strict token stream so
//! strings/comments never confuse the split.

use crate::sqllex::{StrictLexer, Token, TokenKind};
use crate::sqlmodel::{Dialect, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSlice<'a> {
    pub text: &'a str,
    pub span: Span,
}

/// Split a script into statements. Statement text excludes the terminator.
pub fn split_statements(src: &str, dialect: Dialect) -> Vec<StatementSlice<'_>> {
    match dialect {
        Dialect::MySql => split_mysql(src),
        _ => split_tokenized(src, dialect),
    }
}

/// Split a script into execution **batches**, breaking only on batch
/// separators — `GO` alone on its line (SQL Server) or `/` alone on its line
/// (Oracle PL/SQL blocks) — and **preserving** intra-batch `;` terminators so a
/// multi-statement batch is handed to the server as a single unit. Unlike
/// [`split_statements`], this never splits on `;`. Dialects with no batch
/// separator (MySQL, Postgres, SQLite) yield a single slice covering the whole
/// input. Operates on the strict token stream, so a `GO`/`/` inside a string or
/// comment never triggers a split, and a trailing token on the line (e.g.
/// `GO 5`) leaves the separator un-recognized (repeat counts are out of scope).
pub fn split_batches(src: &str, dialect: Dialect) -> Vec<StatementSlice<'_>> {
    match dialect {
        Dialect::SqlServer | Dialect::Oracle => split_batches_tokenized(src, dialect),
        _ => {
            let mut out = Vec::new();
            push_slice(&mut out, src, 0, src.len());
            out
        }
    }
}

fn split_batches_tokenized(src: &str, dialect: Dialect) -> Vec<StatementSlice<'_>> {
    let tokens = StrictLexer::tokenize(src, dialect);
    let mut out = Vec::new();
    let mut start = 0usize;

    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        match t.kind {
            // SQL Server batch separator: GO alone on its line.
            TokenKind::Ident(crate::sqllex::IdentQuote::None) | TokenKind::Keyword
                if dialect == Dialect::SqlServer
                    && t.text.eq_ignore_ascii_case("go")
                    && alone_on_line(&tokens, i) =>
            {
                push_slice(&mut out, src, start, t.span.start);
                start = t.span.end;
            }
            // Oracle: `/` alone on its line ends a PL/SQL block.
            TokenKind::Operator
                if dialect == Dialect::Oracle && t.text == "/" && alone_on_line(&tokens, i) =>
            {
                push_slice(&mut out, src, start, t.span.start);
                start = t.span.end;
            }
            _ => {}
        }
        i += 1;
    }
    push_slice(&mut out, src, start, src.len());
    out
}

fn push_slice<'a>(out: &mut Vec<StatementSlice<'a>>, src: &'a str, start: usize, end: usize) {
    let text = &src[start..end];
    if !text.trim().is_empty() {
        // Trim leading/trailing whitespace but keep spans source-accurate.
        let lead = text.len() - text.trim_start().len();
        let trail = text.len() - text.trim_end().len();
        out.push(StatementSlice {
            text: text.trim(),
            span: Span::new(start + lead, end - trail),
        });
    }
}

/// One statement region produced by the shared splitting walk: the raw byte
/// range (terminator excluded, untrimmed) plus the token index range covering
/// exactly that byte range.
struct Region {
    byte_start: usize,
    byte_end: usize,
    tok_start: usize,
    tok_end: usize,
}

/// The single splitting walk shared by [`split_tokenized`] (string slices)
/// and [`split_statement_tokens`] (token runs), so the two views can never
/// disagree about where statements begin and end.
///
/// In T-SQL and Oracle PL/SQL, a routine definition (CREATE [OR ALTER]
/// PROCEDURE/FUNCTION/TRIGGER …) runs to the next batch separator (`GO` /
/// `/`), and `;` inside it terminates statements *of the body*. Splitting
/// there would detach the body's statements from their definition and hand
/// them to the parser as free-standing DML. Track whether the current slice
/// began as such a definition and suppress `;` splits until the separator.
fn split_regions(tokens: &[Token<'_>], src_len: usize, dialect: Dialect) -> Vec<Region> {
    let mut out = Vec::new();
    let mut byte_start = 0usize;
    let mut tok_start = 0usize;

    let routine_dialect = matches!(dialect, Dialect::SqlServer | Dialect::Oracle);
    let mut awaiting_head = true;
    let mut in_routine = false;
    let mut paren_depth = 0i32;
    // Head keyword of the current statement, previous significant token, and
    // whether a top-level VALUES has been seen — the facts the
    // unterminated-statement split below needs.
    let mut head: Option<String> = None;
    let mut prev: Option<String> = None;
    let mut seen_values = false;

    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        // Editors hold unterminated scripts: `SELECT … <newline> SELECT …` is
        // two statements even without `;`. A DML head keyword at top level
        // inside an already-open simple DML statement ends that statement;
        // the keyword belongs to the region it opens.
        // A `WITH` statement's CTE list is followed by the DML it feeds
        // (`WITH c AS (…) SELECT …`): that keyword is the statement's real
        // head, not a new statement, so adopt it and keep the region whole.
        if !awaiting_head
            && !in_routine
            && paren_depth == 0
            && head.as_deref() == Some("with")
            && prev.as_deref() == Some(")")
            && t.is_word()
            && is_dml_head(t.text)
        {
            head = Some(t.text.to_ascii_lowercase());
            seen_values = false;
        } else if !awaiting_head
            && !in_routine
            // An unclosed `(` in the open statement (`SELECT COUNT( FROM t`)
            // must not swallow the INSERT/UPDATE/DELETE/MERGE typed after it:
            // those heads never appear bare inside parentheses, so they open
            // a new statement at any depth. SELECT and WITH do nest, and only
            // split at the top level.
            && (paren_depth == 0 || is_hard_dml_head(t.text))
            && starts_unterminated_statement(tokens, i, head.as_deref(), prev.as_deref(), seen_values)
        {
            out.push(Region {
                byte_start,
                byte_end: t.span.start,
                tok_start,
                tok_end: i,
            });
            byte_start = t.span.start;
            tok_start = i;
            awaiting_head = true;
            paren_depth = 0;
        }
        if awaiting_head && !t.is_trivia() && t.kind != TokenKind::Semicolon {
            in_routine = routine_dialect && starts_routine_definition(tokens, i);
            awaiting_head = false;
            head = Some(t.text.to_ascii_lowercase());
            seen_values = false;
        }
        match t.kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = (paren_depth - 1).max(0),
            _ => {}
        }
        if paren_depth == 0 && t.is_word() && t.text.eq_ignore_ascii_case("values") {
            seen_values = true;
        }
        let split = match t.kind {
            TokenKind::Semicolon if !in_routine && paren_depth == 0 => true,
            // SQL Server batch separator: GO alone on its line.
            TokenKind::Ident(crate::sqllex::IdentQuote::None) | TokenKind::Keyword
                if dialect == Dialect::SqlServer
                    && t.text.eq_ignore_ascii_case("go")
                    && alone_on_line(tokens, i) =>
            {
                in_routine = false;
                true
            }
            // Oracle: `/` alone on its line ends a PL/SQL block.
            TokenKind::Operator
                if dialect == Dialect::Oracle && t.text == "/" && alone_on_line(tokens, i) =>
            {
                in_routine = false;
                true
            }
            _ => false,
        };
        if split {
            out.push(Region {
                byte_start,
                byte_end: t.span.start,
                tok_start,
                tok_end: i,
            });
            byte_start = t.span.end;
            tok_start = i + 1;
            awaiting_head = true;
            head = None;
            prev = None;
            seen_values = false;
        } else if !t.is_trivia() {
            prev = Some(t.text.to_ascii_lowercase());
        }
        i += 1;
    }
    out.push(Region {
        byte_start,
        byte_end: src_len,
        tok_start,
        tok_end: tokens.len(),
    });
    out
}

fn split_tokenized(src: &str, dialect: Dialect) -> Vec<StatementSlice<'_>> {
    let tokens = StrictLexer::tokenize(src, dialect);
    let mut out = Vec::new();
    for region in split_regions(&tokens, src.len(), dialect) {
        push_slice(&mut out, src, region.byte_start, region.byte_end);
    }
    out
}

/// Tokenize once and split into per-statement token runs for the parser:
/// terminators excluded, leading/trailing whitespace tokens dropped (matching
/// [`split_statements`]' text trimming — edge comments are kept), interior
/// trivia kept, and spans document-absolute. This is the single-lex path:
/// callers hand each run to `TokenCursor::from_tokens` instead of re-lexing
/// the statement text. MySQL still splits textually first (`DELIMITER`
/// redefinitions cannot be pre-lexed) and re-lexes each statement, with spans
/// re-based to document offsets, so its output follows the same contract.
pub fn split_statement_tokens(src: &str, dialect: Dialect) -> Vec<Vec<Token<'_>>> {
    if dialect == Dialect::MySql {
        return split_mysql(src)
            .into_iter()
            .map(|slice| {
                let base = slice.span.start;
                StrictLexer::tokenize(slice.text, dialect)
                    .into_iter()
                    .map(|mut t| {
                        t.span = Span::new(t.span.start + base, t.span.end + base);
                        t
                    })
                    .collect()
            })
            .collect();
    }

    let tokens = StrictLexer::tokenize(src, dialect);
    split_regions(&tokens, src.len(), dialect)
        .into_iter()
        .filter_map(|region| {
            let run = &tokens[region.tok_start..region.tok_end];
            let first = run.iter().position(|t| t.kind != TokenKind::Whitespace)?;
            let last = run.iter().rposition(|t| t.kind != TokenKind::Whitespace)?;
            Some(run[first..=last].to_vec())
        })
        .collect()
}

/// The DML heads that never appear bare inside parentheses, so they open a
/// new statement even while an earlier `(` is unclosed.
fn is_hard_dml_head(word: &str) -> bool {
    ["insert", "update", "delete", "merge"]
        .iter()
        .any(|w| word.eq_ignore_ascii_case(w))
}

/// The simple DML head keywords.
fn is_dml_head(word: &str) -> bool {
    ["select", "insert", "update", "delete", "merge"]
        .iter()
        .any(|w| word.eq_ignore_ascii_case(w))
}

/// Statement heads a following top-level statement keyword may split away
/// from: the simple DML heads, a CTE prefix (whose body head is adopted as the
/// region's head, see [`split_regions`]), and the one-line statements that
/// commonly precede DML in a script without a `;` (`USE db`, `SET NOCOUNT
/// ON`, `DECLARE @x int`, `BEGIN TRAN`, `PRINT`, `EXEC`, …). Compound heads —
/// `CREATE`/`ALTER`, `IF`/`WHILE`/`ELSE` — keep their whole region so routine
/// bodies and control-flow constructs are never chopped up.
fn head_allows_split(head: Option<&str>) -> bool {
    matches!(
        head,
        Some(
            "select"
                | "insert"
                | "update"
                | "delete"
                | "merge"
                | "with"
                | "set"
                | "declare"
                | "print"
                | "use"
                | "truncate"
                | "exec"
                | "execute"
                | "drop"
                | "grant"
                | "revoke"
                | "deny"
                | "commit"
                | "rollback"
                | "return"
                | "raiserror"
                | "throw"
                | "waitfor"
                | "begin"
        )
    )
}

/// The lowercased text of the `n`th significant token after `idx`, if any
/// (`n == 0` is the token right after `idx`).
fn next_significant(tokens: &[Token<'_>], idx: usize, n: usize) -> Option<String> {
    tokens[idx + 1..]
        .iter()
        .filter(|t| !t.is_trivia())
        .nth(n)
        .map(|t| t.text.to_ascii_lowercase())
}

/// Whether the token at `idx` opens a new statement inside an unterminated
/// one: a bare statement keyword (SELECT/INSERT/UPDATE/DELETE/MERGE, or a
/// `WITH` that introduces a CTE) at top level while a statement it may split
/// from (see [`head_allows_split`]) is already open, excluding every position
/// where that keyword legally *continues* the open statement (set operators
/// before SELECT, `INSERT … SELECT` feeds, `FOR UPDATE` locking clauses,
/// upsert tails, `MERGE … THEN UPDATE/INSERT/DELETE` actions, join hints,
/// trigger event lists, …). Conservative on purpose: a word that is not a
/// bare keyword (quoted, or `x.update`) never splits, and when in doubt the
/// keyword is read as a continuation.
fn starts_unterminated_statement(
    tokens: &[Token<'_>],
    idx: usize,
    head: Option<&str>,
    prev: Option<&str>,
    seen_values: bool,
) -> bool {
    let t = &tokens[idx];
    // Only bare (unquoted) words can be statement keywords.
    if !matches!(
        t.kind,
        TokenKind::Keyword | TokenKind::Ident(crate::sqllex::IdentQuote::None)
    ) {
        return false;
    }
    if !head_allows_split(head) {
        return false;
    }
    let prev = prev.unwrap_or("");
    // A member access (`inserted.update`, `x.select`) is never a keyword.
    if prev == "." {
        return false;
    }
    if t.text.eq_ignore_ascii_case("select") {
        // `INSERT … SELECT` feeds rows into the insert; only a VALUES-form
        // insert is complete before a following SELECT.
        if head == Some("insert") && !seen_values {
            return false;
        }
        // Set-operator continuations, a derived table / CTE body, and a
        // cursor's `FOR SELECT` stay in the same statement.
        return !matches!(
            prev,
            "union" | "except" | "intersect" | "minus" | "all" | "as" | "(" | "for"
        );
    }
    if t.text.eq_ignore_ascii_case("insert") {
        // Trigger event lists and MERGE actions. (A comma-separated event
        // list sits under a CREATE TRIGGER head, which never splits.)
        return !matches!(
            prev,
            "before" | "after" | "of" | "for" | "instead" | "or" | "then"
        );
    }
    if t.text.eq_ignore_ascii_case("update") {
        // FOR UPDATE / FOR NO KEY UPDATE / ON DUPLICATE KEY UPDATE / DO UPDATE,
        // trigger event lists, MERGE actions, and the `UPDATE(col)` trigger
        // function.
        if next_significant(tokens, idx, 0).as_deref() == Some("(") {
            return false;
        }
        return !matches!(
            prev,
            "for" | "key" | "do" | "before" | "after" | "of" | "instead" | "or" | "then"
        );
    }
    if t.text.eq_ignore_ascii_case("delete") {
        return !matches!(
            prev,
            "on" | "before" | "after" | "of" | "for" | "instead" | "do" | "or" | "then"
        );
    }
    if t.text.eq_ignore_ascii_case("merge") {
        // `INNER MERGE JOIN` / `MERGE JOIN` are join hints; a MERGE statement
        // is followed by its target (`INTO`, `TOP`, or the table itself) — or
        // by nothing yet, while it is being typed.
        if let Some(next) = next_significant(tokens, idx, 0)
            && (next == "join"
                || !next.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'@' | b'#' | b'[' | b'"')
                }))
        {
            return false;
        }
        return !matches!(
            prev,
            "inner"
                | "left"
                | "right"
                | "full"
                | "outer"
                | "loop"
                | "hash"
                | "remote"
                | "select"
                | "as"
                | ","
                | "by"
                | "on"
        );
    }
    if t.text.eq_ignore_ascii_case("with") {
        // A CTE prefix: `WITH [RECURSIVE] name [(cols)] AS (`. Everything
        // else — table hints `WITH (NOLOCK)`, `WITH TIES`, `WITH ROLLUP`,
        // `WITH CHECK OPTION`, routine options — continues the statement.
        let Some(first) = next_significant(tokens, idx, 0) else {
            return false;
        };
        if first == "recursive" {
            return true;
        }
        if matches!(
            first.as_str(),
            "(" | "execute"
                | "exec"
                | "schemabinding"
                | "encryption"
                | "recompile"
                | "native_compilation"
                | "view_metadata"
                | "check"
                | "nocheck"
                | "ties"
                | "rollup"
                | "cube"
                | "result"
        ) {
            return false;
        }
        return matches!(
            next_significant(tokens, idx, 1).as_deref(),
            Some("as") | Some("(")
        );
    }
    false
}

/// Whether the significant tokens starting at `idx` begin a routine
/// definition: `CREATE [OR ALTER | OR REPLACE] <kind>` or `ALTER <kind>`,
/// where `<kind>` is PROC/PROCEDURE/FUNCTION/TRIGGER/PACKAGE. `CREATE VIEW`
/// is excluded — a view body is a single query with no internal `;`, so
/// normal statement splitting handles it.
fn starts_routine_definition(tokens: &[Token<'_>], idx: usize) -> bool {
    let mut words = tokens[idx..]
        .iter()
        .filter(|t| !t.is_trivia())
        .take(4)
        .map(|t| t.text);
    let is_kind = |w: &str| {
        w.eq_ignore_ascii_case("proc")
            || w.eq_ignore_ascii_case("procedure")
            || w.eq_ignore_ascii_case("function")
            || w.eq_ignore_ascii_case("trigger")
            || w.eq_ignore_ascii_case("package")
    };
    match words.next() {
        Some(w) if w.eq_ignore_ascii_case("alter") => words.next().is_some_and(is_kind),
        Some(w) if w.eq_ignore_ascii_case("create") => match words.next() {
            // CREATE OR ALTER / CREATE OR REPLACE
            Some(or) if or.eq_ignore_ascii_case("or") => {
                words.next(); // alter | replace
                words.next().is_some_and(is_kind)
            }
            Some(kind) => is_kind(kind),
            None => false,
        },
        _ => false,
    }
}

/// True when the token is the only significant token on its source line.
fn alone_on_line(tokens: &[Token<'_>], idx: usize) -> bool {
    // Begins its line: at the start of input, or a newline appears in the
    // trivia run immediately before it.
    let begins_line = idx == 0
        || tokens[..idx]
            .iter()
            .rev()
            .take_while(|t| t.is_trivia())
            .any(|t| t.text.contains('\n'));
    if !begins_line {
        return false;
    }
    // Ends its line: only trivia (and at most a trailing comment) before the
    // next newline or end of input.
    for t in &tokens[idx + 1..] {
        if !t.is_trivia() {
            return false;
        }
        if t.text.contains('\n') {
            return true;
        }
    }
    true
}

/// MySQL: honor `DELIMITER xx` redefinitions (a client-side construct, but
/// ubiquitous in dump files). Falls back to `;`.
fn split_mysql(src: &str) -> Vec<StatementSlice<'_>> {
    let mut out = Vec::new();
    let mut delimiter = ";".to_string();
    let mut start = 0usize;
    let mut pos = 0usize;
    let bytes = src.as_bytes();

    while pos < src.len() {
        // Line-start DELIMITER directive?
        if at_line_start(src, pos) {
            let rest = &src[pos..];
            let line_end = rest.find('\n').map(|i| pos + i).unwrap_or(src.len());
            let line = &src[pos..line_end];
            let trimmed = line.trim_start();
            if trimmed
                .get(..9)
                .is_some_and(|kw| kw.eq_ignore_ascii_case("delimiter"))
            {
                push_slice(&mut out, src, start, pos);
                delimiter = trimmed[9..].trim().to_string();
                if delimiter.is_empty() {
                    delimiter = ";".into();
                }
                pos = line_end + 1.min(src.len() - line_end);
                start = pos.min(src.len());
                continue;
            }
        }
        // Skip strings/comments so delimiters inside them don't split.
        match bytes[pos] {
            b'\'' | b'"' | b'`' => {
                pos = skip_quoted(src, pos, bytes[pos]);
                continue;
            }
            b'-' if bytes.get(pos + 1) == Some(&b'-') => {
                pos = src[pos..].find('\n').map(|i| pos + i).unwrap_or(src.len());
                continue;
            }
            b'#' => {
                pos = src[pos..].find('\n').map(|i| pos + i).unwrap_or(src.len());
                continue;
            }
            b'/' if bytes.get(pos + 1) == Some(&b'*') => {
                pos = src[pos..]
                    .find("*/")
                    .map(|i| pos + i + 2)
                    .unwrap_or(src.len());
                continue;
            }
            _ => {}
        }
        // Byte-wise match: `pos` walks bytes, so it can sit inside a
        // multibyte character where a `&src[pos..]` slice would panic. A
        // UTF-8 delimiter can only match at a character boundary anyway
        // (its first byte is never a continuation byte).
        if bytes[pos..].starts_with(delimiter.as_bytes()) {
            push_slice(&mut out, src, start, pos);
            pos += delimiter.len();
            start = pos;
            continue;
        }
        pos += 1;
    }
    push_slice(&mut out, src, start, src.len());
    out
}

fn at_line_start(src: &str, pos: usize) -> bool {
    pos == 0 || src.as_bytes()[pos - 1] == b'\n'
}

fn skip_quoted(src: &str, start: usize, quote: u8) -> usize {
    let bytes = src.as_bytes();
    let mut pos = start + 1;
    while pos < bytes.len() {
        if bytes[pos] == b'\\' && quote != b'`' {
            pos += 2;
            continue;
        }
        if bytes[pos] == quote {
            if bytes.get(pos + 1) == Some(&quote) {
                pos += 2;
                continue;
            }
            return pos + 1;
        }
        pos += 1;
    }
    src.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semicolons_basic() {
        let parts = split_statements("select 1; select 2;", Dialect::Postgres);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text, "select 1");
        assert_eq!(parts[1].text, "select 2");
    }

    #[test]
    fn semicolon_inside_string_ignored() {
        let parts = split_statements("select 'a;b'; select 2", Dialect::SqlServer);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text, "select 'a;b'");
    }

    // Window frames (`ROWS BETWEEN … AND CURRENT ROW`) are one statement in
    // every dialect; the safety side of the same query is checked per engine
    // in sqlly-intel's classify tests (which ADR-016 keeps dialect-free).
    #[test]
    fn window_frames_stay_one_statement() {
        let sql = "SELECT id,\n       SUM(x) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running,\n       AVG(x) OVER (PARTITION BY id ROWS UNBOUNDED PRECEDING) AS avg_x\nFROM t;";
        for dialect in Dialect::ALL {
            assert_eq!(split_statements(sql, dialect).len(), 1, "{dialect:?}");
        }
    }

    // HeidiSQL #2469: a doubled apostrophe earlier in a string must not flip
    // the quote state, so a later `;` inside a string never splits.
    #[test]
    fn doubled_quote_then_semicolon_inside_strings_does_not_split() {
        let multi_row = "INSERT INTO import_test (id, note) VALUES\n\
            (1, 'Text with apostrophe: user''s item'),\n\
            (2, 'Text with HTML entity semicolon: A &amp; B');";
        let one_by_one = "INSERT INTO import_test (id, note) VALUES (1, 'user''s item');\n\
            INSERT INTO import_test (id, note) VALUES (2, 'A &amp; B');";
        for dialect in Dialect::ALL {
            let parts = split_statements("SELECT 'abc'';def'", dialect);
            assert_eq!(parts.len(), 1, "{dialect:?}");
            assert_eq!(parts[0].text, "SELECT 'abc'';def'", "{dialect:?}");

            let parts = split_statements(multi_row, dialect);
            assert_eq!(parts.len(), 1, "{dialect:?}");
            assert!(parts[0].text.ends_with("A &amp; B')"), "{dialect:?}");

            let parts = split_statements(one_by_one, dialect);
            assert_eq!(parts.len(), 2, "{dialect:?}");
            assert!(parts[0].text.ends_with("'user''s item')"), "{dialect:?}");
            assert!(parts[1].text.ends_with("'A &amp; B')"), "{dialect:?}");
        }
    }

    #[test]
    fn go_batches() {
        let src = "create table T (a int)\nGO\nselect * from T\ngo\n";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].text.starts_with("create table"));
        assert!(parts[1].text.starts_with("select"));
    }

    #[test]
    fn go_not_alone_is_identifier() {
        let src = "select go from t";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn oracle_slash_block() {
        let src = "begin\n null;\nend;\n/\nselect 1 from dual";
        let parts = split_statements(src, Dialect::Oracle);
        assert_eq!(parts.len(), 3); // "begin null" / "end" via ; then /, then select
        assert!(parts.last().unwrap().text.starts_with("select"));
    }

    #[test]
    fn mysql_delimiter_redefinition() {
        let src =
            "DELIMITER $$\ncreate procedure p()\nbegin\n select 1;\nend$$\nDELIMITER ;\nselect 2;";
        let parts = split_statements(src, Dialect::MySql);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].text.starts_with("create procedure"));
        assert!(
            parts[0].text.contains("select 1;"),
            "inner ; must not split"
        );
        assert_eq!(parts[1].text, "select 2");
    }

    #[test]
    fn tsql_procedure_body_semicolons_do_not_split() {
        // T-SQL: a CREATE PROCEDURE runs to the batch separator; the `;`
        // terminators inside its body must not chop the body into
        // free-standing statements (which would then read as top-level DML).
        let src = "CREATE PROCEDURE dbo.p AS\nBEGIN\n  UPDATE t SET x = 1 WHERE id = @id;\n  DELETE FROM log WHERE dt < @cutoff;\nEND\nGO\nSELECT 1";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2, "{parts:#?}");
        assert!(parts[0].text.contains("DELETE FROM log"));
        assert!(parts[1].text.starts_with("SELECT 1"));
    }

    #[test]
    fn tsql_create_or_alter_and_alter_forms_hold_their_body() {
        for head in [
            "CREATE OR ALTER PROCEDURE dbo.p AS",
            "ALTER PROCEDURE dbo.p AS",
            "CREATE TRIGGER trg ON t AFTER INSERT AS",
            "ALTER FUNCTION dbo.f() RETURNS int AS",
        ] {
            let src = format!("{head}\nBEGIN\n SET NOCOUNT ON;\n SELECT 1;\nEND");
            let parts = split_statements(&src, Dialect::SqlServer);
            assert_eq!(parts.len(), 1, "{head}: {parts:#?}");
        }
    }

    #[test]
    fn statements_before_and_after_routine_batches_still_split() {
        let src = "SELECT 1; SELECT 2\nGO\nCREATE PROCEDURE dbo.p AS\nBEGIN\n SELECT 3;\n SELECT 4;\nEND\nGO\nSELECT 5; SELECT 6";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 5, "{parts:#?}");
        assert!(parts[2].text.starts_with("CREATE PROCEDURE"));
        assert!(parts[2].text.contains("SELECT 4"));
    }

    #[test]
    fn create_view_still_splits_normally() {
        // Views hold a single query — the trailing `;` still terminates them.
        let src = "CREATE VIEW v AS SELECT 1; SELECT 2";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn drop_procedure_is_not_a_definition() {
        // Only CREATE/ALTER routine heads suppress `;` splitting.
        let src = "DROP PROCEDURE dbo.p; SELECT 1";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn dollar_quoted_body_survives_split() {
        let src = "create function f() returns int as $$ select 1; $$ language sql; select 2;";
        let parts = split_statements(src, Dialect::Postgres);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].text.contains("$$ select 1; $$"));
    }

    #[test]
    fn spans_are_source_accurate() {
        let src = "select 1;\nselect 2;";
        let parts = split_statements(src, Dialect::Sqlite);
        for p in &parts {
            assert_eq!(&src[p.span.start..p.span.end], p.text);
        }
    }

    // --- unterminated-statement splits -------------------------------------

    #[test]
    fn unterminated_selects_split_on_new_select_head() {
        // No `;` between the statements — the second SELECT still starts a
        // new statement (editors hold unterminated scripts constantly).
        let src = "SELECT * FROM Inventory.ItemCategory\n\n\nSELECT * FROM Accounting.Account where Name like ";
        for dialect in [Dialect::SqlServer, Dialect::Postgres, Dialect::Sqlite] {
            let parts = split_statements(src, dialect);
            assert_eq!(parts.len(), 2, "{dialect:?}: {parts:#?}");
            assert!(parts[0].text.ends_with("ItemCategory"));
            assert!(
                parts[1]
                    .text
                    .starts_with("SELECT * FROM Accounting.Account")
            );
        }
    }

    #[test]
    fn unterminated_mixed_dml_heads_split() {
        let src = "SELECT * FROM a\nUPDATE b SET x = 1\nDELETE FROM c\nINSERT INTO d VALUES (1)\nSELECT 2";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 5, "{parts:#?}");
        assert!(parts[4].text.starts_with("SELECT 2"));
    }

    #[test]
    fn set_operators_do_not_split() {
        for src in [
            "SELECT a FROM t\nUNION\nSELECT b FROM u",
            "SELECT a FROM t UNION ALL SELECT b FROM u",
            "SELECT a FROM t\nEXCEPT\nSELECT b FROM u",
            "SELECT a FROM t\nINTERSECT\nSELECT b FROM u",
        ] {
            let parts = split_statements(src, Dialect::Postgres);
            assert_eq!(parts.len(), 1, "{src}: {parts:#?}");
        }
    }

    #[test]
    fn insert_select_feed_does_not_split() {
        let src = "INSERT INTO t (a, b)\nSELECT a, b FROM u";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 1, "{parts:#?}");
        // But a complete VALUES-form insert followed by SELECT is two.
        let src = "INSERT INTO t (a) VALUES (1)\nSELECT * FROM u";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2, "{parts:#?}");
    }

    #[test]
    fn locking_and_upsert_clauses_do_not_split() {
        for (src, dialect) in [
            ("SELECT * FROM t WHERE id = 1 FOR UPDATE", Dialect::Postgres),
            ("SELECT * FROM t FOR NO KEY UPDATE", Dialect::Postgres),
            (
                "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = 2",
                Dialect::Postgres,
            ),
        ] {
            let parts = split_statements(src, dialect);
            assert_eq!(parts.len(), 1, "{src}: {parts:#?}");
        }
    }

    #[test]
    fn subquery_and_cte_selects_do_not_split() {
        for src in [
            "SELECT * FROM (SELECT a FROM t) d WHERE a IN (SELECT b FROM u)",
            "WITH c AS (SELECT 1 AS x) SELECT * FROM c",
        ] {
            let parts = split_statements(src, Dialect::Postgres);
            assert_eq!(parts.len(), 1, "{src}: {parts:#?}");
        }
    }

    #[test]
    fn routine_bodies_are_not_split_by_head_keywords() {
        let src =
            "CREATE PROCEDURE dbo.p AS\nBEGIN\n  SELECT 1\n  UPDATE t SET x = 1\n  SELECT 2\nEND";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 1, "{parts:#?}");
    }

    #[test]
    fn unterminated_split_spans_are_source_accurate_and_agree_with_tokens() {
        let src = "SELECT * FROM a\n\nSELECT * FROM b where x like ";
        for dialect in [Dialect::SqlServer, Dialect::Postgres, Dialect::Sqlite] {
            let slices = split_statements(src, dialect);
            let runs = split_statement_tokens(src, dialect);
            assert_eq!(slices.len(), 2);
            assert_eq!(slices.len(), runs.len());
            for (slice, run) in slices.iter().zip(&runs) {
                assert_eq!(&src[slice.span.start..slice.span.end], slice.text);
                let (first, last) = (run.first().unwrap(), run.last().unwrap());
                assert_eq!(
                    (first.span.start, last.span.end),
                    (slice.span.start, slice.span.end)
                );
            }
        }
    }

    #[test]
    fn unterminated_update_after_aliased_select_splits() {
        // Feedback ifb_3798: a finished SELECT, a blank line, then `update`
        // being typed — the `update` is a new statement, not the alias's tail.
        let src = "SELECT\n\t*\nFROM\n\tAccounting.ReportingPeriod AS arp\n\t\nupdate ";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2, "{parts:#?}");
        assert_eq!(parts[1].text, "update");
        assert_eq!(parts[1].span.start, src.len() - "update ".len());
    }

    #[test]
    fn unclosed_paren_does_not_swallow_a_following_hard_dml_head() {
        for (src, expect) in [
            ("SELECT COUNT( FROM a\n\nUPDATE t SET x = 1", 2),
            ("SELECT * FROM t WHERE x IN (SELECT 1\nDELETE FROM u", 2),
            ("SELECT * FROM t WHERE x IN (1,\n\ndelete ", 2),
            // Nested SELECTs and locking clauses still belong to their query.
            ("SELECT (SELECT 1 FROM t FOR UPDATE) AS x", 1),
            ("SELECT * FROM t WHERE x IN (SELECT 1\nSELECT 2", 1),
        ] {
            let parts = split_statements(src, Dialect::SqlServer);
            assert_eq!(parts.len(), expect, "{src}: {parts:#?}");
        }
    }

    #[test]
    fn unterminated_merge_is_a_head_and_a_starter() {
        let src = "SELECT * FROM a\nMERGE INTO t USING s ON t.id = s.id\n  WHEN MATCHED THEN UPDATE SET t.x = s.x\n  WHEN NOT MATCHED THEN INSERT (id) VALUES (s.id)\n  WHEN NOT MATCHED BY SOURCE THEN DELETE\nSELECT 2";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 3, "{parts:#?}");
        assert!(parts[1].text.starts_with("MERGE INTO t"));
        assert!(parts[1].text.ends_with("THEN DELETE"));
        assert_eq!(parts[2].text, "SELECT 2");
    }

    #[test]
    fn a_merge_head_being_typed_splits() {
        for src in ["SELECT * FROM a\n\nMERGE ", "SELECT * FROM a\nmerge"] {
            let parts = split_statements(src, Dialect::SqlServer);
            assert_eq!(parts.len(), 2, "{src:?}: {parts:#?}");
        }
    }

    #[test]
    fn merge_join_hints_do_not_split() {
        for src in [
            "SELECT * FROM a INNER MERGE JOIN b ON a.id = b.id",
            "SELECT * FROM a MERGE JOIN b ON a.id = b.id",
            "SELECT * FROM a JOIN b ON a.id = b.id OPTION (MERGE JOIN)",
        ] {
            let parts = split_statements(src, Dialect::SqlServer);
            assert_eq!(parts.len(), 1, "{src}: {parts:#?}");
        }
    }

    #[test]
    fn simple_one_line_heads_release_the_following_dml() {
        // Scripts without `;` routinely open with USE / SET / DECLARE / BEGIN
        // TRAN; the DML after them is its own statement.
        for (src, expect) in [
            ("USE db\nSELECT * FROM t", 2),
            ("SET NOCOUNT ON\nSELECT * FROM t\n\nUPDATE u SET x = 1", 3),
            (
                "DECLARE @t TABLE (id int)\nINSERT INTO @t SELECT id FROM s",
                2,
            ),
            ("BEGIN TRAN\nUPDATE t SET x = 1\nSELECT 1\nCOMMIT", 3),
            ("PRINT 'x'\nSELECT 1", 2),
            ("EXEC dbo.p @a = 1\nSELECT 1", 2),
            ("DECLARE c CURSOR FOR\nSELECT id FROM t", 1),
            ("SET IDENTITY_INSERT t ON\nINSERT INTO t (id) VALUES (1)", 2),
        ] {
            let parts = split_statements(src, Dialect::SqlServer);
            assert_eq!(parts.len(), expect, "{src}: {parts:#?}");
        }
    }

    #[test]
    fn cte_statement_keeps_its_body_and_releases_the_next_statement() {
        let src =
            "WITH c AS (SELECT 1 AS x), d (y) AS (SELECT 2)\nSELECT * FROM c\n\nUPDATE t SET x = 1";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2, "{parts:#?}");
        assert!(parts[0].text.ends_with("SELECT * FROM c"));
        assert!(parts[1].text.starts_with("UPDATE t"));

        // A CTE feeding an INSERT … SELECT stays whole.
        let src = "WITH c AS (SELECT 1 AS x)\nINSERT INTO t (x)\nSELECT x FROM c";
        let parts = split_statements(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 1, "{parts:#?}");

        // Two unterminated CTE statements split at the second WITH.
        let src = "WITH c AS (SELECT 1 AS x)\nSELECT * FROM c\n\nWITH RECURSIVE d AS (SELECT 2)\nSELECT * FROM d";
        let parts = split_statements(src, Dialect::Postgres);
        assert_eq!(parts.len(), 2, "{parts:#?}");
    }

    #[test]
    fn with_clauses_that_are_not_ctes_do_not_split() {
        for src in [
            "SELECT * FROM t WITH (NOLOCK) WHERE x = 1",
            "SELECT TOP 1 WITH TIES x FROM t ORDER BY x",
            "SELECT a, SUM(b) FROM t GROUP BY a WITH ROLLUP",
        ] {
            let parts = split_statements(src, Dialect::SqlServer);
            assert_eq!(parts.len(), 1, "{src}: {parts:#?}");
        }
    }

    #[test]
    fn keyword_lookalikes_do_not_split() {
        for src in [
            // Literals and comments are not tokens at all.
            "SELECT 'update' FROM t\n-- update later\n/* delete */ WHERE x = 1",
            // Quoted and member-access words are identifiers.
            "SELECT [update], \"delete\", t.insert FROM t",
            // Trigger event lists.
            "CREATE TRIGGER tr ON t AFTER INSERT, UPDATE, DELETE AS SELECT 1",
            // The UPDATE(col) trigger function and FOR UPDATE.
            "SELECT * FROM t WHERE UPDATE(x) = 1 FOR UPDATE",
            // Words that merely start with a keyword.
            "SELECT UpdatedBy, DeleteFlag FROM Audit.UpdateLog",
        ] {
            let parts = split_statements(src, Dialect::SqlServer);
            assert_eq!(parts.len(), 1, "{src}: {parts:#?}");
        }
    }

    // --- split_batches (GO-only, semicolons preserved) ---------------------

    #[test]
    fn batches_split_on_go_only_preserving_semicolons() {
        let src = "select 1; select 2\nGO\nselect 3";
        let parts = split_batches(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 2);
        // The intra-batch `;` must NOT split — batch 0 keeps both statements.
        assert_eq!(parts[0].text, "select 1; select 2");
        assert_eq!(parts[1].text, "select 3");
    }

    #[test]
    fn batches_go_inside_string_not_split() {
        let src = "select '\nGO\n' as x";
        let parts = split_batches(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text, src.trim());
    }

    #[test]
    fn batches_go_as_identifier_not_split() {
        let parts = split_batches("select go from t", Dialect::SqlServer);
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn batches_ignore_empty_leading_trailing_and_consecutive_go() {
        let src = "GO\nGO\nselect 1\nGO\n";
        let parts = split_batches(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text, "select 1");
    }

    #[test]
    fn batches_go_with_repeat_count_is_not_a_separator() {
        // `GO 5` has a trailing token on the line, so it is left intact and
        // sent to the server as-is (repeat counts are out of scope).
        let src = "select 1\nGO 5\nselect 2";
        let parts = split_batches(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].text.contains("GO 5"));
    }

    #[test]
    fn batches_non_sqlserver_is_single_slice() {
        let src = "select 1; select 2; select 3";
        let parts = split_batches(src, Dialect::Postgres);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text, src);
    }

    #[test]
    fn batches_spans_are_source_accurate() {
        let src = "select 1\nGO\nselect 2\nGO\nselect 3";
        let parts = split_batches(src, Dialect::SqlServer);
        assert_eq!(parts.len(), 3);
        for p in &parts {
            assert_eq!(&src[p.span.start..p.span.end], p.text);
        }
    }

    // --- split_statement_tokens (single-lex path) ---------------------------

    /// The token path and the text path must agree exactly: same statement
    /// count, and each token run must cover precisely its slice's trimmed
    /// text at the slice's document offsets.
    #[test]
    fn statement_tokens_agree_with_statement_slices() {
        let scripts = [
            "select 1; select 2;",
            "select 'a;b'; select 2",
            "create table T (a int)\nGO\nselect * from T\ngo\n",
            "CREATE PROCEDURE dbo.p AS\nBEGIN\n  UPDATE t SET x = 1 WHERE id = @id;\n  DELETE FROM log WHERE dt < @cutoff;\nEND\nGO\nSELECT 1",
            "SELECT 1; SELECT 2\nGO\nCREATE PROCEDURE dbo.p AS\nBEGIN\n SELECT 3;\n SELECT 4;\nEND\nGO\nSELECT 5; SELECT 6",
            "-- leading comment\nselect 1;\n/* block */ select 2 -- trailing",
            "begin\n null;\nend;\n/\nselect 1 from dual",
            "create function f() returns int as $$ select 1; $$ language sql; select 2;",
            "",
            "   \n\t  ",
            "; ; ;",
        ];
        for dialect in [
            Dialect::SqlServer,
            Dialect::Postgres,
            Dialect::Oracle,
            Dialect::Sqlite,
        ] {
            for src in scripts {
                let slices = split_statements(src, dialect);
                let runs = split_statement_tokens(src, dialect);
                assert_eq!(slices.len(), runs.len(), "{dialect:?}: {src:?}");
                for (slice, run) in slices.iter().zip(&runs) {
                    let first = run.first().expect("non-empty run");
                    let last = run.last().expect("non-empty run");
                    assert_eq!(
                        (first.span.start, last.span.end),
                        (slice.span.start, slice.span.end),
                        "{dialect:?}: {src:?}"
                    );
                    assert_eq!(&src[first.span.start..last.span.end], slice.text);
                }
            }
        }
    }

    #[test]
    fn statement_tokens_mysql_delimiter_spans_are_document_absolute() {
        let src =
            "DELIMITER $$\ncreate procedure p()\nbegin\n select 1;\nend$$\nDELIMITER ;\nselect 2;";
        let slices = split_statements(src, Dialect::MySql);
        let runs = split_statement_tokens(src, Dialect::MySql);
        assert_eq!(slices.len(), runs.len());
        for (slice, run) in slices.iter().zip(&runs) {
            for t in run {
                assert_eq!(&src[t.span.start..t.span.end], t.text, "{slice:?}");
            }
        }
    }
}
