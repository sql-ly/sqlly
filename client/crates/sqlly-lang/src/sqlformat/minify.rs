//! SQL minifier: collapse a statement (or script) to as few lines as
//! possible for pasting into logs, JSON, or a CLI. Token-based on the same
//! `sqlly-sqllex` stream the formatter uses, so string literals, quoted
//! identifiers, and dollar-quoted bodies pass through verbatim.
//!
//! Rules:
//! - Ordinary comments are dropped. Comments that carry meaning are kept:
//!   `-- sqlly …` directive comments (any spelling the directive grammar
//!   accepts, e.g. `--sqlly`), optimizer hints (`/*+ … */`, and Oracle's
//!   `--+ …` line form), and MySQL / MariaDB executable comments
//!   (`/*! … */`, `/*!50700 … */`, `/*M! … */`), whose body the server runs.
//!   A kept line comment sits on its own line so it cannot swallow the SQL
//!   after it.
//! - All other whitespace collapses to single spaces, with no space before
//!   `,` / `)` / `;` / `.` and none after `(` / `.`.
//! - Statement separators are kept. `GO` must be alone on a line, so a `GO`
//!   batch separator keeps its surrounding line breaks instead of joining its
//!   neighbours.
//! - Input that does not lex cleanly (unterminated string/comment, illegal
//!   character) is returned unchanged — broken SQL is not rewritten.
//! - Like the formatter, the result is re-lexed and must hold the same
//!   tokens as the input minus the dropped comments; otherwise the input is
//!   returned unchanged.

use crate::sqllex::{CommentShape, IdentQuote, StrictLexer, Token, TokenKind};
use crate::sqlmodel::Dialect;

/// Minify `sql` under `dialect`. Infallible: input that fails to lex cleanly
/// comes back unchanged.
pub fn minify(sql: &str, dialect: Dialect) -> String {
    let tokens = StrictLexer::tokenize(sql, dialect);
    if tokens.iter().any(|t| matches!(t.kind, TokenKind::Error(_))) {
        return sql.to_string();
    }

    let mut out = String::with_capacity(sql.len());
    for (i, t) in tokens.iter().enumerate() {
        match t.kind {
            TokenKind::Whitespace => {}
            TokenKind::Comment(shape) => {
                if !keep_comment(t, shape) {
                    continue;
                }
                match shape {
                    // A line comment ends at a newline; on one line it would
                    // comment out everything after it, so it owns its line.
                    CommentShape::Line => {
                        newline(&mut out);
                        out.push_str(t.text.trim_end());
                        out.push('\n');
                    }
                    CommentShape::Block => {
                        separate(&mut out, t);
                        out.push_str(t.text);
                    }
                }
            }
            _ if is_go_separator(&tokens, i, dialect) => {
                // `GO` must be alone on its line: keep the line breaks.
                newline(&mut out);
                out.push_str(t.text);
                out.push('\n');
            }
            _ => {
                if !keep_touching(&tokens, i) {
                    separate(&mut out, t);
                }
                out.push_str(t.text);
            }
        }
    }
    while out.ends_with('\n') || out.ends_with(' ') {
        out.pop();
    }
    if same_sql(&tokens, &StrictLexer::tokenize(&out, dialect)) {
        out
    } else {
        sql.to_string()
    }
}

/// The minifier's safety net: `out` must hold exactly the input's
/// significant tokens (whitespace and dropped comments aside), byte for byte.
fn same_sql(input: &[Token<'_>], out: &[Token<'_>]) -> bool {
    fn kept(t: &&Token<'_>) -> bool {
        match t.kind {
            TokenKind::Whitespace => false,
            TokenKind::Comment(shape) => keep_comment(t, shape),
            _ => true,
        }
    }
    fn text<'a>(t: &Token<'a>) -> &'a str {
        match t.kind {
            TokenKind::Comment(CommentShape::Line) => t.text.trim_end(),
            _ => t.text,
        }
    }
    input
        .iter()
        .filter(kept)
        .map(text)
        .eq(out.iter().filter(kept).map(text))
}

/// Directive comments (`-- sqlly …`, `--sqlly …`), optimizer hints and
/// executable comments carry meaning and survive minification.
fn keep_comment(t: &Token<'_>, shape: CommentShape) -> bool {
    match shape {
        CommentShape::Line => {
            // Oracle's line-comment hint form: `--+ INDEX(t i)`.
            if t.text.starts_with("--+") {
                return true;
            }
            let body = t.text.trim_start_matches('-');
            body.trim_start()
                .get(..5)
                .is_some_and(|w| w.eq_ignore_ascii_case("sqlly"))
        }
        // Hints, and MySQL / MariaDB executable comments: the server runs
        // what is inside `/*! … */` and `/*M! … */`, so dropping one changes
        // the statement.
        CommentShape::Block => ["/*+", "/*!", "/*M!"].iter().any(|p| t.text.starts_with(p)),
    }
}

/// True when token `i` is a batch separator that must sit alone on its
/// line: T-SQL's `GO` under the SQL Server dialect, or the `/` that ends a
/// PL/SQL block in an Oracle (SQL*Plus / SQLcl) script.
fn is_go_separator(tokens: &[Token<'_>], i: usize, dialect: Dialect) -> bool {
    let t = &tokens[i];
    let separator = match dialect {
        Dialect::SqlServer => {
            matches!(
                t.kind,
                TokenKind::Ident(IdentQuote::None) | TokenKind::Keyword
            ) && t.text.eq_ignore_ascii_case("go")
        }
        Dialect::Oracle => t.kind == TokenKind::Operator && t.text == "/",
        _ => false,
    };
    separator && alone_on_line(tokens, i)
}

/// Same test the batch splitter applies: begins its line (only trivia with a
/// newline before it, or start of input) and ends it (only trivia up to the
/// next newline or end of input).
fn alone_on_line(tokens: &[Token<'_>], idx: usize) -> bool {
    let begins_line = idx == 0
        || tokens[..idx]
            .iter()
            .rev()
            .take_while(|t| t.is_trivia())
            .any(|t| t.text.contains('\n'));
    if !begins_line {
        return false;
    }
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

/// Tokens written touching stay touching. That keeps everything a space
/// would change the meaning of together: operator characters the lexer
/// splits (`**`, or an operator it does not know), MySQL's `COUNT(` (a
/// space before a built-in function's parenthesis is a syntax error there),
/// Snowflake `src:path`, MySQL `user@host`, Oracle `t@dblink` and
/// `name%TYPE`. Only whitespace and dropped comments ever collapse.
fn keep_touching(tokens: &[Token<'_>], i: usize) -> bool {
    i.checked_sub(1).map(|p| &tokens[p]).is_some_and(|prev| {
        !matches!(prev.kind, TokenKind::Whitespace | TokenKind::Comment(_))
            && prev.span.end == tokens[i].span.start
    })
}

/// Push the separator owed before `next`: nothing at a line start or when the
/// pair joins tightly, otherwise a single space.
fn separate(out: &mut String, next: &Token<'_>) {
    let Some(prev) = out.chars().next_back() else {
        return;
    };
    if prev == '\n' {
        return;
    }
    let tight_before = matches!(
        next.kind,
        TokenKind::Comma | TokenKind::RParen | TokenKind::Semicolon | TokenKind::Dot
    ) || (next.kind == TokenKind::Operator && next.text == "]");
    let tight_after = matches!(prev, '(' | '.' | '[');
    if !tight_before && !tight_after {
        out.push(' ');
    }
}

/// End the current line, if the output is mid-line.
fn newline(out: &mut String) {
    if out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minify_preserves_string_literals() {
        let sql = "SELECT  'a   b\n c' AS x ,\n  \"weird   col\"\nFROM t\nWHERE y = 'it''s   ok'";
        let out = minify(sql, Dialect::Postgres);
        assert_eq!(
            out,
            "SELECT 'a   b\n c' AS x, \"weird   col\" FROM t WHERE y = 'it''s   ok'"
        );
    }

    #[test]
    fn minify_keeps_directive_comments() {
        let sql =
            "-- sqlly name 1: Totals\nSELECT 1; -- plain comment\n--sqlly view:grid\nSELECT   2";
        let out = minify(sql, Dialect::Postgres);
        assert_eq!(
            out,
            "-- sqlly name 1: Totals\nSELECT 1;\n--sqlly view:grid\nSELECT 2"
        );
    }

    #[test]
    fn minify_keeps_go_on_own_line() {
        let sql = "CREATE   TABLE t ( id INT )\nGO\nSELECT *\nFROM   t\nGO";
        let out = minify(sql, Dialect::SqlServer);
        assert_eq!(out, "CREATE TABLE t (id INT)\nGO\nSELECT * FROM t\nGO");
        // A `go` that is not alone on its line is an identifier, not a
        // separator, and joins like any other word.
        let sql = "SELECT go FROM plans";
        assert_eq!(minify(sql, Dialect::SqlServer), "SELECT go FROM plans");
    }

    #[test]
    fn minify_idempotent() {
        let cases = [
            (
                "SELECT a.b ,  c( d )\nFROM s.t ;\nDELETE   FROM u",
                Dialect::Postgres,
            ),
            (
                "-- sqlly name 1: x\nSELECT 1\nGO\nSELECT 2",
                Dialect::SqlServer,
            ),
            (
                "SELECT /*+ INDEX(t i) */ *\nFROM t -- drop me",
                Dialect::Oracle,
            ),
        ];
        for (sql, dialect) in cases {
            let once = minify(sql, dialect);
            assert_eq!(minify(&once, dialect), once, "not idempotent for {sql:?}");
        }
    }

    #[test]
    fn minify_returns_broken_input_unchanged() {
        let sql = "SELECT 'unterminated";
        assert_eq!(minify(sql, Dialect::Postgres), sql);
    }

    #[test]
    fn minify_keeps_mysql_executable_comments() {
        let sql = "CREATE TABLE t (id INT) /*!50100 PARTITION BY HASH(id) */ /* note */;\nSELECT /*! STRAIGHT_JOIN */ a FROM b /*M!100400 , c */";
        let out = minify(sql, Dialect::MySql);
        assert_eq!(
            out,
            "CREATE TABLE t (id INT) /*!50100 PARTITION BY HASH(id) */; SELECT /*! STRAIGHT_JOIN */ a FROM b /*M!100400 , c */"
        );
        assert_eq!(minify(&out, Dialect::MySql), out);
    }

    #[test]
    fn minify_keeps_oracle_line_hints() {
        let sql = "SELECT --+ INDEX(t idx)\n  * FROM t -- gone";
        assert_eq!(
            minify(sql, Dialect::Oracle),
            "SELECT\n--+ INDEX(t idx)\n* FROM t"
        );
    }

    #[test]
    fn minify_output_keeps_every_significant_token() {
        let cases = [
            ("select a-- note\n-1 from t", Dialect::Postgres),
            ("select 1/**/as x, $$ a  b $$ from t", Dialect::Postgres),
            ("select [a b] ,\n  @p, N'x' from #t\nGO", Dialect::SqlServer),
            ("select `a`, \"s\" # hash comment\nfrom t", Dialect::MySql),
            ("select q'[it's]',\n  :bind from dual", Dialect::Oracle),
        ];
        for (sql, dialect) in cases {
            let out = minify(sql, dialect);
            let before = StrictLexer::tokenize(sql, dialect);
            let after = StrictLexer::tokenize(&out, dialect);
            assert!(same_sql(&before, &after), "{sql:?} -> {out:?}");
            assert_ne!(out, sql, "{sql:?} should have been minified");
        }
    }

    #[test]
    fn minify_keeps_touching_tokens_touching() {
        // MySQL rejects `COUNT (*)`; `**`, `src:path`, `user@host`,
        // `t@dblink` and `sal%TYPE` all change meaning with a space.
        let cases = [
            (
                "SELECT COUNT(*),\n  j->>'$.a'\nFROM t",
                Dialect::MySql,
                "SELECT COUNT(*), j->>'$.a' FROM t",
            ),
            (
                "CREATE USER bob@localhost",
                Dialect::MySql,
                "CREATE USER bob@localhost",
            ),
            (
                "x := y**2;\n  v emp.sal%TYPE;",
                Dialect::Oracle,
                "x := y**2; v emp.sal%TYPE;",
            ),
            (
                "SELECT *\nFROM orders@hq",
                Dialect::Oracle,
                "SELECT * FROM orders@hq",
            ),
            (
                "SELECT src:customer.name::string\nFROM t",
                Dialect::Postgres,
                "SELECT src:customer.name::string FROM t",
            ),
            (
                "SELECT tags[1],\n  ARRAY[ 1, 2 ]",
                Dialect::Postgres,
                "SELECT tags[1], ARRAY[1, 2]",
            ),
        ];
        for (sql, dialect, want) in cases {
            assert_eq!(minify(sql, dialect), want, "{sql:?}");
        }
    }

    #[test]
    fn minify_keeps_sqlplus_slash_on_its_own_line() {
        let sql = "BEGIN\n  NULL;\nEND;\n/\nSELECT 1\nFROM dual";
        assert_eq!(
            minify(sql, Dialect::Oracle),
            "BEGIN NULL; END;\n/\nSELECT 1 FROM dual"
        );
        // Elsewhere a lone `/` is division.
        assert_eq!(minify("SELECT a\n/\nb", Dialect::Postgres), "SELECT a / b");
    }

    #[test]
    fn minify_keeps_hint_comments() {
        let sql = "SELECT /*+ INDEX(t idx) */ *\nFROM t /* gone */";
        assert_eq!(
            minify(sql, Dialect::Oracle),
            "SELECT /*+ INDEX(t idx) */ * FROM t"
        );
    }
}
