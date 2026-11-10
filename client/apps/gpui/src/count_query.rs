//! Lazy `SELECT COUNT(*)` wrapper for a completed result set.
//!
//! Lives in the GPUI crate (not `sqlly-query`) because wrapping needs
//! [`DatabaseType`] and ADR-016 forbids dialect types under `sqlly-query/src/safety/`.
//!
//! The total answers "how many rows would this statement return", so statement-level
//! caps (`TOP n`, `LIMIT n`) are kept. A trailing top-level `ORDER BY` is stripped
//! unless `OFFSET` / `FETCH` / `LIMIT` follow it (those make `ORDER BY` load-bearing,
//! and SQL Server rejects `ORDER BY` inside a derived table without them). CTEs cannot
//! nest in a derived table on SQL Server or MySQL, so they are refused.

use sqlly_lang::sqlmodel::DatabaseType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CountWrapError {
    /// `classify_select_only` refused the statement (reason string from the classifier).
    NotSelectOnly(String),
    /// More than one statement, or an empty batch — a count wrapper needs exactly one SELECT.
    NotSingleStatement,
    /// A top-level `WITH … SELECT` CTE cannot be nested in a derived table on every engine.
    CteUnsupported,
}

/// `SELECT COUNT(*) FROM (<inner>) AS sqlly_count` for the given engine.
pub fn count_wrapper_sql(db: DatabaseType, sql: &str) -> Result<String, CountWrapError> {
    let classified = sqlly_intel::query::safety::classify_select_only_for(db, sql);
    if !classified.select_only {
        return Err(CountWrapError::NotSelectOnly(
            classified.reason.unwrap_or_default(),
        ));
    }

    let inner = prepare_inner(sql)?;
    let _ = db.connection_kind();
    Ok(format!("SELECT COUNT(*) FROM (\n{inner}\n) AS sqlly_count"))
}

fn prepare_inner(sql: &str) -> Result<String, CountWrapError> {
    let mut inner = sql.trim();
    if inner.ends_with(';') {
        inner = inner[..inner.len() - 1].trim_end();
    }
    if inner.is_empty() {
        return Err(CountWrapError::NotSingleStatement);
    }
    if has_semicolon_outside_literals(inner) {
        return Err(CountWrapError::NotSingleStatement);
    }
    if first_keyword(inner).is_some_and(|kw| kw.eq_ignore_ascii_case("WITH")) {
        return Err(CountWrapError::CteUnsupported);
    }
    Ok(strip_trailing_order_by(inner).to_string())
}

fn has_semicolon_outside_literals(sql: &str) -> bool {
    let mut found = false;
    walk_normal(sql, |i, _depth, bytes| {
        if bytes[i] == b';' {
            found = true;
        }
    });
    found
}

fn first_keyword(sql: &str) -> Option<String> {
    let mut i = 0;
    skip_trivia(sql, &mut i);
    while i < sql.len() {
        skip_trivia(sql, &mut i);
        if sql.as_bytes().get(i) == Some(&b'(') {
            i += 1;
            continue;
        }
        break;
    }
    skip_trivia(sql, &mut i);
    read_ident(sql, i).map(|(word, _)| word)
}

fn strip_trailing_order_by(sql: &str) -> &str {
    let mut last_order_by: Option<usize> = None;
    walk_normal(sql, |i, depth, _bytes| {
        if depth == 0 && is_order_by_at(sql, i) {
            last_order_by = Some(i);
        }
    });
    let Some(start) = last_order_by else {
        return sql;
    };
    let after = skip_order_by_keyword(sql, start);
    if has_paging_keyword_at_depth_zero(sql, after) {
        return sql;
    }
    sql[..start].trim_end()
}

fn skip_order_by_keyword(sql: &str, start: usize) -> usize {
    let mut i = start + 5; // "ORDER"
    skip_trivia(sql, &mut i);
    i += 2; // "BY"
    skip_trivia(sql, &mut i);
    i
}

fn is_order_by_at(sql: &str, i: usize) -> bool {
    if !is_word_at(sql, i, "ORDER") {
        return false;
    }
    let mut j = i + 5;
    skip_trivia(sql, &mut j);
    is_word_at(sql, j, "BY")
}

fn has_paging_keyword_at_depth_zero(sql: &str, from: usize) -> bool {
    let mut found = false;
    walk_normal_from(sql, from, |i, depth, _bytes| {
        if depth == 0
            && (is_word_at(sql, i, "OFFSET")
                || is_word_at(sql, i, "FETCH")
                || is_word_at(sql, i, "LIMIT"))
        {
            found = true;
        }
    });
    found
}

fn is_word_at(sql: &str, i: usize, word: &str) -> bool {
    let bytes = sql.as_bytes();
    let w = word.as_bytes();
    if i + w.len() > bytes.len() {
        return false;
    }
    if !bytes[i..i + w.len()].eq_ignore_ascii_case(w) {
        return false;
    }
    let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
    let after = i + w.len();
    let after_ok = after == bytes.len() || !is_ident_byte(bytes[after]);
    before_ok && after_ok
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn read_ident(sql: &str, i: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    if i >= bytes.len() || !is_ident_start(bytes[i]) {
        return None;
    }
    let mut j = i + 1;
    while j < bytes.len() && is_ident_byte(bytes[j]) {
        j += 1;
    }
    Some((sql[i..j].to_string(), j))
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn skip_trivia(sql: &str, i: &mut usize) {
    let bytes = sql.as_bytes();
    loop {
        if *i >= bytes.len() {
            return;
        }
        let b = bytes[*i];
        if b.is_ascii_whitespace() {
            *i += 1;
            continue;
        }
        if b == b'-' && bytes.get(*i + 1) == Some(&b'-') {
            *i += 2;
            while *i < bytes.len() && bytes[*i] != b'\n' {
                *i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(*i + 1) == Some(&b'*') {
            *i += 2;
            while *i + 1 < bytes.len() && !(bytes[*i] == b'*' && bytes[*i + 1] == b'/') {
                *i += 1;
            }
            if *i + 1 < bytes.len() {
                *i += 2;
            }
            continue;
        }
        return;
    }
}

fn walk_normal(sql: &str, mut on_normal: impl FnMut(usize, i32, &[u8])) {
    walk_normal_from(sql, 0, &mut on_normal);
}

fn walk_normal_from(sql: &str, from: usize, mut on_normal: impl FnMut(usize, i32, &[u8])) {
    let bytes = sql.as_bytes();
    let mut i = from;
    let mut depth: i32 = 0;
    let mut state = ScanState::Normal;
    while i < bytes.len() {
        match state {
            ScanState::Normal => {
                on_normal(i, depth, bytes);
                match bytes[i] {
                    b'(' => {
                        depth += 1;
                        i += 1;
                    }
                    b')' => {
                        depth = depth.saturating_sub(1);
                        i += 1;
                    }
                    b'\'' => {
                        state = ScanState::SqString;
                        i += 1;
                    }
                    b'"' => {
                        state = ScanState::DqString;
                        i += 1;
                    }
                    b'[' => {
                        state = ScanState::Bracket;
                        i += 1;
                    }
                    b'-' if bytes.get(i + 1) == Some(&b'-') => {
                        state = ScanState::LineComment;
                        i += 2;
                    }
                    b'/' if bytes.get(i + 1) == Some(&b'*') => {
                        state = ScanState::BlockComment;
                        i += 2;
                    }
                    b'N' | b'n' if bytes.get(i + 1) == Some(&b'\'') => {
                        state = ScanState::SqString;
                        i += 2;
                    }
                    _ => {
                        let ch = sql[i..].chars().next().expect("index on char boundary");
                        i += ch.len_utf8();
                    }
                }
            }
            ScanState::SqString => {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    let ch = sql[i..].chars().next().expect("index on char boundary");
                    i += ch.len_utf8();
                }
            }
            ScanState::DqString => {
                if bytes[i] == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    let ch = sql[i..].chars().next().expect("index on char boundary");
                    i += ch.len_utf8();
                }
            }
            ScanState::Bracket => {
                if bytes[i] == b']' {
                    if bytes.get(i + 1) == Some(&b']') {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    let ch = sql[i..].chars().next().expect("index on char boundary");
                    i += ch.len_utf8();
                }
            }
            ScanState::LineComment => {
                if bytes[i] == b'\n' {
                    state = ScanState::Normal;
                }
                i += 1;
            }
            ScanState::BlockComment => {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = ScanState::Normal;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ScanState {
    Normal,
    LineComment,
    BlockComment,
    SqString,
    DqString,
    Bracket,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;

    fn wrap(sql: &str) -> String {
        count_wrapper_sql(DatabaseType::SqlServer, sql).expect("wrap")
    }

    fn inner_of(wrapped: &str) -> &str {
        wrapped
            .strip_prefix("SELECT COUNT(*) FROM (\n")
            .and_then(|s| s.strip_suffix("\n) AS sqlly_count"))
            .expect("wrapper shape")
    }

    #[test]
    fn wraps_plain_select_sqlserver() {
        assert_eq!(
            wrap("SELECT id FROM t"),
            "SELECT COUNT(*) FROM (\nSELECT id FROM t\n) AS sqlly_count"
        );
    }

    #[test]
    fn strips_trailing_semicolon_and_order_by() {
        assert_eq!(
            inner_of(&wrap("SELECT id FROM t ORDER BY id;")),
            "SELECT id FROM t"
        );
    }

    #[test]
    fn keeps_order_by_when_followed_by_offset_fetch() {
        let sql = "SELECT id FROM t ORDER BY id OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY";
        assert_eq!(inner_of(&wrap(sql)), sql);
    }

    #[test]
    fn keeps_order_by_when_followed_by_limit() {
        let sql = "SELECT id FROM t ORDER BY id LIMIT 5";
        assert_eq!(inner_of(&wrap(sql)), sql);
    }

    #[test]
    fn keeps_top_and_limit() {
        assert_eq!(
            inner_of(&wrap("SELECT TOP 10 id FROM t")),
            "SELECT TOP 10 id FROM t"
        );
        assert_eq!(
            inner_of(&wrap("SELECT id FROM t LIMIT 10")),
            "SELECT id FROM t LIMIT 10"
        );
    }

    #[test]
    fn ignores_order_by_inside_subquery() {
        let sql = "SELECT * FROM (SELECT id FROM t ORDER BY id) x";
        assert_eq!(inner_of(&wrap(sql)), sql);
    }

    #[test]
    fn ignores_order_by_inside_string_literal() {
        let sql = "SELECT 'ORDER BY x' AS s FROM t";
        assert_eq!(inner_of(&wrap(sql)), sql);
    }

    #[test]
    fn refuses_multi_statement() {
        assert_eq!(
            count_wrapper_sql(DatabaseType::SqlServer, "SELECT 1; SELECT 2"),
            Err(CountWrapError::NotSingleStatement)
        );
    }

    #[test]
    fn refuses_mutation() {
        match count_wrapper_sql(DatabaseType::SqlServer, "DELETE FROM t") {
            Err(CountWrapError::NotSelectOnly(_)) => {}
            other => panic!("expected NotSelectOnly, got {other:?}"),
        }
    }

    #[test]
    fn refuses_cte() {
        assert_eq!(
            count_wrapper_sql(
                DatabaseType::SqlServer,
                "WITH x AS (SELECT 1) SELECT * FROM x"
            ),
            Err(CountWrapError::CteUnsupported)
        );
    }

    #[test]
    fn same_shape_for_every_engine() {
        let sql = "SELECT 1 AS a";
        let expected = "SELECT COUNT(*) FROM (\nSELECT 1 AS a\n) AS sqlly_count";
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::MariaDb,
            DatabaseType::Sqlite,
        ] {
            assert_eq!(count_wrapper_sql(db, sql).expect("wrap"), expected);
        }
    }
}
