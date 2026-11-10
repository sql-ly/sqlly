//! Execution units: the exact pieces SQLLY hands an engine, one request each.
//!
//! Engines whose protocol takes one statement per request (DuckDB's
//! `prepare`, libSQL's Hrana pipeline, ClickHouse's HTTP body, Oracle's OCI)
//! and SQL Server's client-side `GO` batches are split here, and only here:
//! the drivers send these units and the safety gates judge these units, so a
//! gate can never approve one split while the driver sends another. Engines
//! that split server-side (PostgreSQL simple query, MySQL multi-statement,
//! SQLite's prepare-tail loop) get the whole text as one unit.

use super::LexEngine;
use super::rules;
use super::scan::{ConstructKind, construct_at};
use crate::sqlmodel::{ConnectionKind, DatabaseType, Dialect};

/// One piece an engine receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecUnit {
    /// Exactly what the driver sends.
    pub text: String,
    /// Byte range of the source text the unit came from.
    pub start: usize,
    pub end: usize,
    /// `text` is `sql[start..end]` verbatim (false when the splitter rewrote
    /// it, e.g. Oracle `EXEC` sugar or a regrouped trigger body).
    pub verbatim: bool,
}

impl ExecUnit {
    fn slice(sql: &str, start: usize, end: usize) -> Self {
        ExecUnit {
            text: sql[start..end].to_string(),
            start,
            end,
            verbatim: true,
        }
    }
}

/// The units a connection's driver sends for `sql`, in order. Empty and
/// comment-only pieces are dropped.
pub fn execution_units(sql: &str, db: DatabaseType) -> Vec<ExecUnit> {
    match db.connection_kind() {
        ConnectionKind::Tds => crate::sqllex::split_batches(sql, Dialect::SqlServer)
            .into_iter()
            .map(|b| ExecUnit::slice(sql, b.span.start, b.span.end))
            .collect(),
        ConnectionKind::DuckDbFile => crate::sqllex::split_statements(sql, Dialect::Postgres)
            .into_iter()
            .filter_map(|s| trimmed_unit(sql, s.span.start, s.span.end, LexEngine::DuckDb))
            .collect(),
        ConnectionKind::HranaHttp => libsql_units(sql),
        ConnectionKind::ClickHouseHttp => split_top_level(sql, LexEngine::ClickHouse)
            .into_iter()
            .filter_map(|(start, end)| trimmed_unit(sql, start, end, LexEngine::ClickHouse))
            .collect(),
        ConnectionKind::OracleNet => crate::sqllex::oracle_script::split_script_spans(sql)
            .into_iter()
            .map(|(statement, start, end)| {
                let verbatim = sql.get(start..end).is_some_and(|s| s == statement.text);
                ExecUnit {
                    text: statement.text,
                    start,
                    end,
                    verbatim,
                }
            })
            .collect(),
        _ => {
            if sql.trim().is_empty() {
                Vec::new()
            } else {
                vec![ExecUnit::slice(sql, 0, sql.len())]
            }
        }
    }
}

/// `sql[start..end]` with leading whitespace/comments (per `engine`'s default
/// reading) and trailing whitespace removed; `None` when nothing is left.
fn trimmed_unit(sql: &str, start: usize, end: usize, engine: LexEngine) -> Option<ExecUnit> {
    let piece = &sql[start..end];
    let stripped = strip_leading_trivia(piece, engine);
    let lead = piece.len() - stripped.len();
    let body = stripped.trim_end();
    if body.is_empty() {
        return None;
    }
    Some(ExecUnit::slice(
        sql,
        start + lead,
        start + lead + body.len(),
    ))
}

/// Skip leading whitespace and comments exactly as `engine` reads them
/// (nested block comments where the engine nests them). An unterminated
/// comment is not skipped: the engine will refuse the piece, and nothing
/// after the opener is hidden by pretending it closed.
pub fn strip_leading_trivia(text: &str, engine: LexEngine) -> &str {
    let rules = rules::primary(engine);
    let bytes = text.as_bytes();
    let mut pos = 0;
    loop {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        match construct_at(bytes, pos, &rules) {
            Some(c)
                if c.terminated
                    && matches!(
                        c.kind,
                        ConstructKind::LineComment | ConstructKind::BlockComment
                    ) =>
            {
                pos = c.end;
            }
            _ => return &text[pos..],
        }
    }
}

/// Byte ranges between top-level `;` terminators under `engine`'s default
/// reading (terminators excluded, untrimmed, empty pieces kept).
pub fn split_top_level(sql: &str, engine: LexEngine) -> Vec<(usize, usize)> {
    let rules = rules::primary(engine);
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        if let Some(c) = construct_at(bytes, pos, &rules) {
            // An unterminated construct runs to the end: the engine refuses
            // the piece, and splitting inside it would invent statements.
            pos = c.end.max(pos + 1);
            continue;
        }
        if bytes[pos] == b';' {
            out.push((start, pos));
            start = pos + 1;
        }
        pos += 1;
    }
    out.push((start, bytes.len()));
    out
}

/// libSQL: `;`-separated statements (SQLite reading), with a `CREATE
/// TRIGGER` body's inner statements kept with their trigger — Hrana refuses a
/// request holding two statements, so the body must travel whole.
fn libsql_units(sql: &str) -> Vec<ExecUnit> {
    let mut out: Vec<ExecUnit> = Vec::new();
    let mut open: Option<ExecUnit> = None;
    for slice in crate::sqllex::split_statements(sql, Dialect::Sqlite) {
        let Some(unit) = trimmed_unit(sql, slice.span.start, slice.span.end, LexEngine::Sqlite)
        else {
            continue;
        };
        if let Some(mut pending) = open.take() {
            pending.text.push_str(";\n");
            pending.text.push_str(&unit.text);
            pending.end = unit.end;
            pending.verbatim = false;
            if ends_trigger_body(&unit.text) {
                out.push(pending);
            } else {
                open = Some(pending);
            }
            continue;
        }
        if opens_trigger_body(&unit.text) && !ends_trigger_body(&unit.text) {
            open = Some(unit);
            continue;
        }
        out.push(unit);
    }
    if let Some(pending) = open {
        out.push(pending);
    }
    out
}

fn leading_words(text: &str, count: usize) -> Vec<String> {
    strip_leading_trivia(text, LexEngine::Sqlite)
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .take(count)
        .map(|w| w.to_ascii_uppercase())
        .collect()
}

/// `CREATE [TEMP|TEMPORARY] TRIGGER …`: its body holds `;`-terminated
/// statements between `BEGIN` and `END`, which the splitter would cut.
fn opens_trigger_body(text: &str) -> bool {
    let words = leading_words(text, 3);
    matches!(words.first().map(String::as_str), Some("CREATE"))
        && words.iter().skip(1).take(2).any(|w| w == "TRIGGER")
}

fn ends_trigger_body(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(';')
        .trim_end()
        .to_ascii_uppercase()
        .ends_with("END")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str, db: DatabaseType) -> Vec<String> {
        execution_units(sql, db)
            .into_iter()
            .map(|u| u.text)
            .collect()
    }

    #[test]
    fn libsql_keeps_trigger_bodies_whole() {
        let script = "-- leading\nSELECT ';' AS a; /* x; y */ INSERT INTO t VALUES ('a;b'), (\"c;d\")\n;\n\n  ;\nSELECT 2 -- trailing; comment\n";
        let parts = texts(script, DatabaseType::LibSql);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], "SELECT ';' AS a");
        assert_eq!(parts[1], "INSERT INTO t VALUES ('a;b'), (\"c;d\")");
        assert_eq!(parts[2], "SELECT 2 -- trailing; comment");
        assert!(texts("   \n-- nothing\n", DatabaseType::LibSql).is_empty());

        let trigger = "CREATE TABLE t (a);\nCREATE TRIGGER trg AFTER INSERT ON t BEGIN\n  UPDATE t SET a = 1; DELETE FROM t WHERE a = 2;\nEND;\nSELECT 1";
        let parts = texts(trigger, DatabaseType::LibSql);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts[1].starts_with("CREATE TRIGGER trg"));
        assert!(parts[1].trim_end().ends_with("END"));
        assert_eq!(parts[2], "SELECT 1");
    }

    #[test]
    fn leading_comments_are_stripped_the_way_the_engine_nests_them() {
        // DuckDB nests block comments: the whole construct is one comment, so
        // nothing is left to run. A flat strip would have sent the DELETE.
        let sql = "/* /* */ DELETE FROM t --*/";
        assert!(texts(sql, DatabaseType::DuckDb).is_empty(), "{sql}");
        assert!(texts(sql, DatabaseType::ClickHouse).is_empty(), "{sql}");
        // SQLite does not nest: the DELETE is live and is what gets sent.
        assert_eq!(
            texts(sql, DatabaseType::LibSql),
            vec!["DELETE FROM t --*/".to_string()]
        );
    }

    #[test]
    fn clickhouse_splits_on_its_own_quoting_rules() {
        // Backslash escapes inside backtick identifiers: the `;` is inside.
        let sql = "SELECT 1 AS `a\\`; DELETE FROM t; --`";
        assert_eq!(texts(sql, DatabaseType::ClickHouse).len(), 1, "{sql}");
        let sql = "SELECT 'a;b'; SELECT \"c;d\"; SELECT $x$;$x$";
        assert_eq!(texts(sql, DatabaseType::ClickHouse).len(), 3, "{sql}");
    }

    #[test]
    fn server_split_engines_get_one_unit() {
        for db in [
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Sqlite,
        ] {
            assert_eq!(texts("SELECT 1; DELETE FROM t", db).len(), 1, "{db:?}");
        }
        assert_eq!(
            texts("SELECT 1\nGO\nDELETE FROM t", DatabaseType::SqlServer).len(),
            2
        );
    }
}
