//! Oracle script splitting (moved here from the Oracle driver so the driver
//! and the safety gates share one split — see `seclex::execution_units`).
//!
//! OCI executes one statement per call and rejects a trailing `;` on plain
//! SQL, while a PL/SQL unit (`BEGIN … END;`, `DECLARE … END;`, `CREATE
//! PROCEDURE/FUNCTION/PACKAGE/TRIGGER … END;`) is one statement that keeps
//! every `;` inside it and, by SQL*Plus convention, is closed by a `/` alone
//! on its own line. The shared `sqlly-lang` splitter knows about `/` but
//! splits anonymous blocks on their inner `;`, so this module walks the
//! strict token stream itself:
//!
//! - a statement whose head is `BEGIN`, `DECLARE`, or a routine-creating
//!   `CREATE` runs to the `/` separator; without one it ends at the `;` that
//!   closes its outermost `BEGIN`/`END` pair (a package spec or type body
//!   opens that pair at the header, `CASE` and `END CASE` balance, `END IF`
//!   and `END LOOP` are neutral);
//! - `WITH FUNCTION …` (a SQL statement carrying PL/SQL) runs to the `/`;
//! - everything else ends at the next top-level `;` or `/`;
//! - SQL*Plus `EXEC`/`EXECUTE proc(...)` is sugar for `BEGIN proc(...); END;`.

use crate::sqllex::{StrictLexer, Token, TokenKind};
use crate::sqlmodel::Dialect;

/// One statement ready for OCI: plain SQL has no terminator; PL/SQL keeps
/// its closing `END;`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptStatement {
    pub text: String,
    pub plsql: bool,
}

/// Split an Oracle script into executable statements.
pub fn split_script(src: &str) -> Vec<ScriptStatement> {
    split_script_spans(src)
        .into_iter()
        .map(|(statement, _, _)| statement)
        .collect()
}

/// [`split_script`] with the byte range of the source each statement came
/// from (the statement text may be rewritten: terminators trimmed, `EXEC`
/// sugar expanded).
pub fn split_script_spans(src: &str) -> Vec<(ScriptStatement, usize, usize)> {
    let tokens = StrictLexer::tokenize(src, Dialect::Oracle);
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        // Skip trivia, stray terminators and separators between statements.
        let t = &tokens[i];
        if t.is_trivia() || t.kind == TokenKind::Semicolon || is_slash_separator(&tokens, i) {
            i += 1;
            continue;
        }
        let start = i;
        let unit = classify(&tokens, start);
        let (end, next) = match unit {
            Unit::Sql => end_of_sql(&tokens, start),
            Unit::PlSql { initial_depth } => end_of_plsql(&tokens, start, initial_depth),
            Unit::SqlWithPlSql => end_at_slash(&tokens, start),
        };
        let byte_start = tokens[start].span.start;
        let byte_end = if end < tokens.len() {
            tokens[end].span.start
        } else {
            src.len()
        };
        let raw = &src[byte_start..byte_end];
        let text = raw.trim();
        if !text.is_empty() {
            let lead = raw.len() - raw.trim_start().len();
            out.push((
                render(text, &unit, &tokens[start]),
                byte_start + lead,
                byte_start + lead + text.len(),
            ));
        }
        i = next;
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Sql,
    PlSql {
        initial_depth: i32,
    },
    /// `WITH FUNCTION … SELECT …`: SQL that embeds PL/SQL and needs `/`.
    SqlWithPlSql,
}

fn render(text: &str, unit: &Unit, head: &Token<'_>) -> ScriptStatement {
    match unit {
        Unit::Sql => {
            if head.text.eq_ignore_ascii_case("exec") || head.text.eq_ignore_ascii_case("execute") {
                // SQL*Plus sugar: `EXEC dbms_output.put_line('x')`.
                let body = text[head.text.len()..]
                    .trim()
                    .trim_end_matches(';')
                    .trim_end();
                if !body.is_empty() {
                    return ScriptStatement {
                        text: format!("BEGIN {body}; END;"),
                        plsql: true,
                    };
                }
            }
            ScriptStatement {
                text: text.trim_end_matches(';').trim_end().to_string(),
                plsql: false,
            }
        }
        Unit::PlSql { .. } => {
            // Exactly one closing `;` after the final END, whatever the user
            // (or a caller appending its own terminator) wrote.
            let body = text.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
            ScriptStatement {
                text: format!("{body};"),
                plsql: true,
            }
        }
        Unit::SqlWithPlSql => ScriptStatement {
            text: text.trim_end_matches(';').trim_end().to_string(),
            plsql: true,
        },
    }
}

/// Significant (non-trivia) token index at or after `i`.
fn next_significant(tokens: &[Token<'_>], mut i: usize) -> Option<usize> {
    while i < tokens.len() {
        if !tokens[i].is_trivia() {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn word_eq(t: &Token<'_>, word: &str) -> bool {
    t.is_word() && t.text.eq_ignore_ascii_case(word)
}

/// The routine kinds whose `CREATE` opens a PL/SQL unit, with the block
/// depth the header itself contributes: a package (spec or body) and a type
/// body end with an `END` that has no `BEGIN` of its own.
fn create_unit(tokens: &[Token<'_>], after_create: usize) -> Option<Unit> {
    let mut i = after_create;
    // CREATE [OR REPLACE] [[NON]EDITIONABLE] <kind>
    loop {
        let idx = next_significant(tokens, i)?;
        let t = &tokens[idx];
        if word_eq(t, "or")
            || word_eq(t, "replace")
            || word_eq(t, "editionable")
            || word_eq(t, "noneditionable")
        {
            i = idx + 1;
            continue;
        }
        let kind = t.text.to_ascii_uppercase();
        let next_is_body = next_significant(tokens, idx + 1)
            .map(|n| word_eq(&tokens[n], "body"))
            .unwrap_or(false);
        return match kind.as_str() {
            "PROCEDURE" | "FUNCTION" | "TRIGGER" => Some(Unit::PlSql { initial_depth: 0 }),
            "PACKAGE" => Some(Unit::PlSql { initial_depth: 1 }),
            "TYPE" if next_is_body => Some(Unit::PlSql { initial_depth: 1 }),
            _ => None,
        };
    }
}

fn classify(tokens: &[Token<'_>], head: usize) -> Unit {
    let t = &tokens[head];
    if word_eq(t, "begin") || word_eq(t, "declare") {
        return Unit::PlSql { initial_depth: 0 };
    }
    if word_eq(t, "create") {
        return create_unit(tokens, head + 1).unwrap_or(Unit::Sql);
    }
    if word_eq(t, "with")
        && let Some(n) = next_significant(tokens, head + 1)
        && (word_eq(&tokens[n], "function") || word_eq(&tokens[n], "procedure"))
    {
        return Unit::SqlWithPlSql;
    }
    Unit::Sql
}

/// `/` alone on its line (nothing but whitespace/comments around it on that
/// line) is the SQL*Plus block terminator.
fn is_slash_separator(tokens: &[Token<'_>], i: usize) -> bool {
    let t = &tokens[i];
    if t.kind != TokenKind::Operator || t.text != "/" {
        return false;
    }
    let before_ok = tokens[..i]
        .iter()
        .rev()
        .take_while(|p| p.is_trivia())
        .any(|p| p.kind == TokenKind::Whitespace && p.text.contains('\n'))
        || tokens[..i].iter().all(|p| p.is_trivia());
    let after_ok = tokens[i + 1..]
        .iter()
        .take_while(|n| n.is_trivia())
        .any(|n| n.kind == TokenKind::Whitespace && n.text.contains('\n'))
        || tokens[i + 1..].iter().all(|n| n.is_trivia());
    before_ok && after_ok
}

/// Plain SQL: up to the next top-level `;` or `/` separator. Returns the
/// exclusive end token index and the index to resume scanning from.
fn end_of_sql(tokens: &[Token<'_>], start: usize) -> (usize, usize) {
    let mut depth = 0i32;
    let mut i = start;
    while i < tokens.len() {
        let t = &tokens[i];
        match t.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            TokenKind::Semicolon if depth <= 0 => return (i, i + 1),
            _ if is_slash_separator(tokens, i) => return (i, i + 1),
            _ => {}
        }
        i += 1;
    }
    (tokens.len(), tokens.len())
}

/// A PL/SQL unit: to the `/` separator, or to the `;` that closes its
/// outermost block when no separator follows.
fn end_of_plsql(tokens: &[Token<'_>], start: usize, initial_depth: i32) -> (usize, usize) {
    let mut depth = initial_depth;
    let mut opened = initial_depth > 0;
    let mut i = start;
    while i < tokens.len() {
        let t = &tokens[i];
        if is_slash_separator(tokens, i) {
            return (i, i + 1);
        }
        if t.is_word() {
            let word = t.text.to_ascii_uppercase();
            match word.as_str() {
                "BEGIN" | "CASE" => {
                    depth += 1;
                    opened = true;
                }
                // Compound triggers close each timing section with `END
                // <timing>` and the whole trigger with a final END.
                "COMPOUND" => {
                    if next_significant(tokens, i + 1)
                        .map(|n| word_eq(&tokens[n], "trigger"))
                        .unwrap_or(false)
                    {
                        depth += 1;
                        opened = true;
                    }
                }
                "END" => {
                    // `END IF` / `END LOOP` close constructs that never
                    // opened a block; `END CASE` closes the CASE counted
                    // above. The trailing word is consumed with the END so
                    // it is not re-read as an opener.
                    if let Some(n) = next_significant(tokens, i + 1) {
                        if word_eq(&tokens[n], "if") || word_eq(&tokens[n], "loop") {
                            i = n + 1;
                            continue;
                        }
                        if word_eq(&tokens[n], "case") {
                            depth -= 1;
                            i = n + 1;
                            continue;
                        }
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        if t.kind == TokenKind::Semicolon && opened && depth <= 0 {
            // Include this `;` (it closes the unit), then skip a `/` that
            // may follow so it is not re-read as a stray separator.
            let mut next = i + 1;
            if let Some(n) = next_significant(tokens, next)
                && is_slash_separator(tokens, n)
            {
                next = n + 1;
            }
            return (i + 1, next);
        }
        i += 1;
    }
    (tokens.len(), tokens.len())
}

fn end_at_slash(tokens: &[Token<'_>], start: usize) -> (usize, usize) {
    let mut i = start;
    while i < tokens.len() {
        if is_slash_separator(tokens, i) {
            return (i, i + 1);
        }
        i += 1;
    }
    (tokens.len(), tokens.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str) -> Vec<(String, bool)> {
        split_script(src)
            .into_iter()
            .map(|s| (s.text, s.plsql))
            .collect()
    }

    #[test]
    fn plain_statements_split_on_semicolon_and_lose_the_terminator() {
        assert_eq!(
            texts("SELECT 1 FROM DUAL;\nSELECT 2 FROM DUAL"),
            vec![
                ("SELECT 1 FROM DUAL".to_string(), false),
                ("SELECT 2 FROM DUAL".to_string(), false)
            ]
        );
    }

    #[test]
    fn semicolons_inside_strings_comments_and_parens_do_not_split() {
        let src = "SELECT ';' FROM DUAL; -- a; comment\n/* b; */ SELECT (1) FROM DUAL;";
        assert_eq!(
            texts(src),
            vec![
                ("SELECT ';' FROM DUAL".to_string(), false),
                ("SELECT (1) FROM DUAL".to_string(), false)
            ]
        );
    }

    #[test]
    fn anonymous_block_is_one_statement_closed_by_slash() {
        let src = "BEGIN\n  DBMS_OUTPUT.PUT_LINE('a');\n  NULL;\nEND;\n/\nSELECT 1 FROM DUAL;";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(
            parts[0].0,
            "BEGIN\n  DBMS_OUTPUT.PUT_LINE('a');\n  NULL;\nEND;"
        );
        assert!(parts[0].1);
        assert_eq!(parts[1], ("SELECT 1 FROM DUAL".to_string(), false));
    }

    #[test]
    fn block_without_slash_ends_at_its_outer_end() {
        let src = "DECLARE\n  n NUMBER;\nBEGIN\n  IF n IS NULL THEN\n    n := CASE WHEN 1 = 1 THEN 2 ELSE 3 END;\n  END IF;\n  FOR i IN 1..2 LOOP NULL; END LOOP;\nEND;\nSELECT 1 FROM DUAL";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].0.starts_with("DECLARE"));
        assert!(parts[0].0.ends_with("END LOOP;\nEND;"));
        assert!(parts[0].1);
        assert_eq!(parts[1].0, "SELECT 1 FROM DUAL");
    }

    #[test]
    fn nested_blocks_and_case_statements_balance() {
        let src = "BEGIN\n  BEGIN NULL; END;\n  CASE 1 WHEN 1 THEN NULL; END CASE;\nEND;\n\nSELECT 2 FROM DUAL;";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].0.ends_with("END CASE;\nEND;"));
        assert_eq!(parts[1].0, "SELECT 2 FROM DUAL");
    }

    #[test]
    fn routines_keep_their_bodies() {
        let src = "CREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  INSERT INTO t VALUES (1);\n  DELETE FROM t;\nEND p;\n/\nCREATE OR REPLACE FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;\n/\nCREATE OR REPLACE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN :new.id := 1; END;\n/\nSELECT f FROM DUAL;";
        let parts = texts(src);
        assert_eq!(parts.len(), 4, "{parts:?}");
        assert!(parts[0].0.contains("DELETE FROM t;"));
        assert!(parts[0].0.ends_with("END p;"));
        assert!(parts[0].1 && parts[1].1 && parts[2].1);
        assert_eq!(parts[3].0, "SELECT f FROM DUAL");
    }

    #[test]
    fn package_spec_and_body_open_their_block_at_the_header() {
        let src = "CREATE OR REPLACE PACKAGE pkg AS\n  PROCEDURE a;\n  FUNCTION b RETURN NUMBER;\nEND pkg;\n/\nCREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE a IS BEGIN NULL; END a;\n  FUNCTION b RETURN NUMBER IS BEGIN RETURN 1; END b;\nEND pkg;\n/\nCREATE TYPE BODY ty AS MEMBER FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END; END;\n/";
        let parts = texts(src);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts[0].0.ends_with("END pkg;"));
        assert!(parts[1].0.contains("END b;"));
        assert!(parts[1].0.ends_with("END pkg;"));
        assert!(parts[2].0.ends_with("END; END;"));
        // Without separators the header-opened depth still lets the spec end
        // at its own END.
        let src = "CREATE PACKAGE p AS PROCEDURE a; END p;\nSELECT 1 FROM DUAL;";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[1].0, "SELECT 1 FROM DUAL");
    }

    #[test]
    fn create_table_and_type_specs_are_plain_sql() {
        let src = "CREATE TABLE t (id NUMBER);\nCREATE TYPE ty AS OBJECT (a NUMBER, MEMBER FUNCTION f RETURN NUMBER);\nCREATE INDEX i ON t (id);";
        let parts = texts(src);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts.iter().all(|p| !p.1));
        assert_eq!(parts[0].0, "CREATE TABLE t (id NUMBER)");
    }

    #[test]
    fn with_function_runs_to_the_slash() {
        let src = "WITH FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;\nSELECT f() FROM DUAL\n/\nSELECT 2 FROM DUAL";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].0.starts_with("WITH FUNCTION"));
        assert!(parts[0].0.ends_with("SELECT f() FROM DUAL"));
        assert!(parts[0].1);
        assert_eq!(parts[1].0, "SELECT 2 FROM DUAL");
    }

    #[test]
    fn slash_inside_an_expression_is_division() {
        let src = "SELECT 10 / 2 FROM DUAL;\nSELECT 4\n/ 2 FROM DUAL";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[1].0, "SELECT 4\n/ 2 FROM DUAL");
    }

    #[test]
    fn stray_separators_and_doubled_terminators_are_ignored() {
        let src = ";\n/\nBEGIN NULL; END;;\n/\n/\nSELECT 1 FROM DUAL;;";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].0, "BEGIN NULL; END;");
        assert_eq!(parts[1].0, "SELECT 1 FROM DUAL");
    }

    #[test]
    fn exec_is_sugar_for_an_anonymous_block() {
        let parts = texts("EXEC DBMS_OUTPUT.PUT_LINE('hi');\nexecute p(1)");
        assert_eq!(
            parts,
            vec![
                ("BEGIN DBMS_OUTPUT.PUT_LINE('hi'); END;".to_string(), true),
                ("BEGIN p(1); END;".to_string(), true)
            ]
        );
    }

    #[test]
    fn compound_trigger_sections_do_not_end_the_unit_early() {
        let src = "CREATE TRIGGER ct FOR INSERT ON t COMPOUND TRIGGER\n BEFORE STATEMENT IS BEGIN NULL; END BEFORE STATEMENT;\n AFTER EACH ROW IS BEGIN NULL; END AFTER EACH ROW;\nEND ct;\nSELECT 1 FROM DUAL";
        let parts = texts(src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].0.ends_with("END ct;"));
    }

    #[test]
    fn empty_and_comment_only_scripts_yield_nothing() {
        assert!(split_script("").is_empty());
        assert!(split_script("  \n\t").is_empty());
        assert!(split_script("-- nothing\n/* here */").is_empty());
        assert!(split_script(";;\n/\n").is_empty());
    }
}
