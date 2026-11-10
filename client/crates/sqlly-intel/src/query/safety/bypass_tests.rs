//! Regression corpus for safety-gate bypasses that confused the boundaries of
//! string literals, quoted identifiers and comments. Every case here passed a
//! gate before the gates read SQL through the per-engine security lexer.

use super::*;
use sqlly_core::{ConnectionProfile, DatabaseType, SecretProvider, SecretRef, StudioError};

fn profile(db: DatabaseType) -> ConnectionProfile {
    let mut p = ConnectionProfile::sql_login(
        "local",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "password".to_string(),
        },
    );
    p.database_type = db;
    p
}

fn blocked_for(db: DatabaseType, sql: &str) -> bool {
    !classify_select_only_for(db, sql).select_only
}

/// The user's report: a backtick identifier holding `'` opened a fake
/// literal in the dialect-free tokenizer and hid the DELETE between two.
#[test]
fn reported_backtick_quote_repro_is_blocked() {
    let sql = "SELECT 1 AS `'`; DELETE FROM t; SELECT 1 AS `'`";
    for db in [
        DatabaseType::MySql,
        DatabaseType::MariaDb,
        DatabaseType::TiDb,
        DatabaseType::PlanetScale,
        DatabaseType::Sqlite,
        DatabaseType::LibSql,
        DatabaseType::ClickHouse,
    ] {
        let c = classify_select_only_for(db, sql);
        assert!(!c.select_only, "{db:?}: {c:?}");
        assert!(c.reason.unwrap().contains("DELETE"), "{db:?}");
    }
    assert!(!classify_select_only(sql).select_only);

    // Statements with no separator between them (T-SQL) are all judged.
    for sql in [
        "DELETE FROM t\nSELECT COUNT(*) FROM t",
        "SELECT COUNT(*) FROM t\nDELETE FROM t",
    ] {
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::MySql,
            DatabaseType::Postgres,
        ] {
            assert!(blocked_for(db, sql), "{db:?}: {sql}");
        }
    }
}

#[test]
fn quote_characters_inside_other_quoting_styles_cannot_hide_statements() {
    for (db, sql) in [
        (
            DatabaseType::Postgres,
            "SELECT 1 AS \"'\"; DELETE FROM t; SELECT 1 AS \"'\"",
        ),
        (
            DatabaseType::SqlServer,
            "SELECT 1 AS \"'\"; DELETE FROM t; SELECT 1 AS \"'\"",
        ),
        (
            DatabaseType::SqlServer,
            "SELECT 1 AS [']; DELETE FROM t; SELECT 1 AS [']",
        ),
        (
            DatabaseType::Sqlite,
            "SELECT 1 AS [']; DELETE FROM t; SELECT 1 AS [']",
        ),
        (
            DatabaseType::Oracle,
            "SELECT 1 AS \"'\" FROM dual; DELETE FROM t; SELECT '",
        ),
        (
            DatabaseType::Postgres,
            "SELECT $$'$$; DELETE FROM t; SELECT $$'$$",
        ),
        (
            DatabaseType::DuckDb,
            "SELECT $$'$$; DELETE FROM t; SELECT $$'$$",
        ),
        (DatabaseType::MySql, r"SELECT '\''; DELETE FROM t; SELECT '"),
        (
            DatabaseType::MySql,
            r#"SELECT "\""; DELETE FROM t; SELECT ""#,
        ),
        (
            DatabaseType::Postgres,
            r"SELECT E'\''; DELETE FROM t; SELECT '",
        ),
        (
            DatabaseType::CockroachDb,
            r"SELECT b'\''; DELETE FROM t; SELECT '",
        ),
        (
            DatabaseType::Oracle,
            "SELECT q'[']' FROM dual; DELETE FROM t; SELECT '",
        ),
        (
            DatabaseType::Sqlite,
            "SELECT $a(');DELETE FROM t;SELECT $a(')",
        ),
        (
            DatabaseType::ClickHouse,
            r"SELECT 'a\\'; DELETE FROM t; SELECT '",
        ),
    ] {
        assert!(blocked_for(db, sql), "{db:?}: {sql}");
        // The dialect-free gate holds too (it reads under every engine).
        assert!(!classify_select_only(sql).select_only, "any: {sql}");
    }
}

#[test]
fn comment_rules_cannot_hide_statements() {
    for (db, sql) in [
        // MySQL: `#` comments, and `--` needs a space after it.
        (DatabaseType::MySql, "SELECT 1 # '\nDELETE FROM t; -- '"),
        (DatabaseType::MySql, "SELECT 1 --1; DELETE FROM t"),
        // PostgreSQL: `--` ends at a bare carriage return; comments nest.
        (DatabaseType::Postgres, "SELECT 1 -- '\rDELETE FROM t; -- '"),
        (
            DatabaseType::Postgres,
            "SELECT 1 /* /* */ ' */ DELETE FROM t; -- '",
        ),
        (
            DatabaseType::SqlServer,
            "SELECT 1 /* /* */ ' */ DELETE FROM t; -- '",
        ),
        // Non-nesting engines.
        (
            DatabaseType::MySql,
            "SELECT 1 /* /* */ DELETE FROM t; -- */",
        ),
        (
            DatabaseType::Sqlite,
            "SELECT 1 /* /* */ DELETE FROM t; -- */",
        ),
        // ClickHouse: `# ` comments; nested and flat readings.
        (
            DatabaseType::ClickHouse,
            "SELECT 1 # '\nDELETE FROM t; -- '",
        ),
        (
            DatabaseType::ClickHouse,
            "SELECT 1 /* /* */ ; DELETE FROM t; -- */",
        ),
    ] {
        assert!(blocked_for(db, sql), "{db:?}: {sql:?}");
    }
}

#[test]
fn server_modes_the_client_cannot_see_are_all_judged() {
    // NO_BACKSLASH_ESCAPES: the default reading hides this DELETE.
    assert!(blocked_for(
        DatabaseType::MySql,
        r"SELECT 'a\'; DELETE FROM t; -- '"
    ));
    // ANSI_QUOTES: `"…"` is an identifier without backslash escapes.
    assert!(blocked_for(
        DatabaseType::MySql,
        r#"SELECT "a\"; DELETE FROM t; -- ""#
    ));
    // standard_conforming_strings = off.
    assert!(blocked_for(
        DatabaseType::Postgres,
        r"SELECT 'a\'; DELETE FROM t; -- '"
    ));
    // Executable comments at every server version.
    for sql in [
        "/*!50003 DELETE FROM t */ SELECT 1",
        "/*!99999 ' */ DELETE FROM t; -- '",
        "/*!99999 /* */ ' */ DELETE FROM t; -- '",
        "/*!10000 \"*/\"*/ /*!99999 ' */ ; DELETE FROM t; -- \"'",
        "/*M!100100 DELETE FROM t */ SELECT 1",
    ] {
        assert!(blocked_for(DatabaseType::MySql, sql), "{sql}");
        assert!(blocked_for(DatabaseType::MariaDb, sql), "{sql}");
    }
}

#[test]
fn engine_reading_removes_the_old_dialect_free_false_positives() {
    // A Windows path in a T-SQL literal is complete (no backslash escapes).
    assert!(!blocked_for(
        DatabaseType::SqlServer,
        r"SELECT * FROM t WHERE p = 'C:\dir\' AND x = 'update me'"
    ));
    // PostgreSQL dollar-quoted text is data.
    assert!(!blocked_for(
        DatabaseType::Postgres,
        "SELECT f($$ UPDATE t SET x = 1 $$)"
    ));
    // MySQL backticks and `#` comments.
    assert!(!blocked_for(
        DatabaseType::MySql,
        "SELECT `DELETE` FROM t # drop it later"
    ));
    // T-SQL temp tables stay identifiers.
    assert!(!blocked_for(DatabaseType::SqlServer, "SELECT x FROM #temp"));
    // MySQL `O\'Brien` (backslash-escaped quote) is a SELECT under every mode.
    assert!(!blocked_for(
        DatabaseType::MySql,
        r"SELECT * FROM t WHERE name = 'O\'Brien'"
    ));
}

#[test]
fn unterminated_and_nul_input_fail_closed() {
    for db in [
        DatabaseType::SqlServer,
        DatabaseType::Postgres,
        DatabaseType::MySql,
        DatabaseType::Sqlite,
        DatabaseType::Oracle,
    ] {
        let c = classify_select_only_for(db, "SELECT 'abc");
        assert!(!c.select_only, "{db:?}");
        assert!(c.reason.unwrap().contains("unterminated"), "{db:?}");
        let c = classify_select_only_for(db, "SELECT 1\0; DELETE FROM t");
        assert!(!c.select_only, "{db:?}");
        assert!(c.reason.unwrap().contains("NUL"), "{db:?}");
    }
}

#[test]
fn each_execution_unit_is_judged() {
    // DuckDB and ClickHouse receive one statement per request; the units are
    // what gets judged, split by the engine's own rules.
    assert!(blocked_for(DatabaseType::DuckDb, "SELECT 1; DELETE FROM t"));
    assert!(blocked_for(
        DatabaseType::ClickHouse,
        "SELECT 1; DELETE FROM t"
    ));
    // A leading nested comment is stripped the way DuckDB nests it: nothing
    // is left to run, so nothing can be classified as SELECT either.
    assert!(blocked_for(
        DatabaseType::DuckDb,
        "/* /* */ DELETE FROM t --*/"
    ));
}

#[test]
fn read_only_screen_reads_every_quoting_style() {
    assert!(
        read_only_violation(
            DatabaseType::Postgres,
            "SELECT \"'\", pg_terminate_backend(1), \"'\""
        )
        .is_some()
    );
    assert!(
        read_only_violation(
            DatabaseType::MySql,
            "SELECT 1 AS `'`, GET_LOCK('x', 1), 1 AS `'`"
        )
        .is_some()
    );
    assert!(
        read_only_violation(
            DatabaseType::SqlServer,
            "SELECT 1 AS \"'\"; EXEC dbo.Purge; SELECT 1 AS \"'\""
        )
        .is_some()
    );
}

fn wrapped(db: DatabaseType) -> ConnectionProfile {
    let mut p = profile(db);
    p.auto_wrap_rollback = true;
    p
}

fn refused(db: DatabaseType, sql: &str) -> String {
    let result = if db.connection_kind() == sqlly_core::ConnectionKind::Tds {
        apply_connection_policy(&wrapped(db), sql)
    } else {
        apply_connection_policy_for(&wrapped(db), sql)
    };
    match result {
        Err(StudioError::PermissionDenied(message)) => message,
        other => panic!("{db:?}: {sql:?} must be refused, got {other:?}"),
    }
}

/// Statements that end the wrapping transaction early make everything after
/// them stick; the wrap refuses them instead of silently not rolling back.
#[test]
fn rollback_wrap_refuses_transaction_control() {
    for (db, sql, verb) in [
        (DatabaseType::MySql, "ROLLBACK; DELETE FROM t", "ROLLBACK"),
        (DatabaseType::MySql, "DELETE FROM t; COMMIT", "COMMIT"),
        (DatabaseType::MySql, "BEGIN; DELETE FROM t", "BEGIN"),
        (
            DatabaseType::MySql,
            "START TRANSACTION; DELETE FROM t",
            "START TRANSACTION",
        ),
        (
            DatabaseType::MySql,
            "SET autocommit = 1; DELETE FROM t",
            "autocommit",
        ),
        (DatabaseType::Postgres, "COMMIT; DELETE FROM t", "COMMIT"),
        (DatabaseType::Postgres, "END; DELETE FROM t", "END"),
        (DatabaseType::Postgres, "ABORT; DELETE FROM t", "ABORT"),
        (
            DatabaseType::Postgres,
            "PREPARE TRANSACTION 'x'; DELETE FROM t",
            "PREPARE",
        ),
        (DatabaseType::Sqlite, "BEGIN; DELETE FROM t", "BEGIN"),
        (DatabaseType::DuckDb, "COMMIT; DELETE FROM t", "COMMIT"),
        (DatabaseType::SqlServer, "DELETE FROM t COMMIT", "COMMIT"),
        (
            DatabaseType::SqlServer,
            "ROLLBACK DELETE FROM t",
            "ROLLBACK",
        ),
        (
            DatabaseType::Oracle,
            "BEGIN DELETE FROM t; COMMIT; END;",
            "COMMIT",
        ),
        // Hidden by a quoting trick from the old tokenizer.
        (
            DatabaseType::MySql,
            "SELECT 1 AS `'`; COMMIT; SELECT 1 AS `'`",
            "COMMIT",
        ),
    ] {
        let message = refused(db, sql);
        assert!(message.contains(verb), "{db:?}: {sql}: {message}");
    }
    // Savepoints do not end the transaction.
    let sql = apply_connection_policy_for(
        &wrapped(DatabaseType::Postgres),
        "SAVEPOINT s; DELETE FROM t; ROLLBACK TO SAVEPOINT s",
    )
    .unwrap();
    assert!(sql.starts_with("BEGIN;"), "{sql}");
    // Deploying a T-SQL procedure runs none of its body: a COMMIT there ends
    // nothing, and the definition still deploys (and rolls back) via EXEC.
    let sql = apply_connection_policy(
        &wrapped(DatabaseType::SqlServer),
        "CREATE PROCEDURE dbo.P AS BEGIN BEGIN TRAN; UPDATE t SET a = 1; COMMIT; END",
    )
    .unwrap();
    assert!(sql.contains("EXEC(N'"), "{sql}");
    // T-SQL BEGIN TRAN only nests; BEGIN…END blocks are not transactions.
    let sql = apply_connection_policy(
        &wrapped(DatabaseType::SqlServer),
        "BEGIN TRAN; UPDATE t SET a = 1; IF 1 = 1 BEGIN SELECT 1 END",
    )
    .unwrap();
    assert!(sql.ends_with("ROLLBACK TRANSACTION;"), "{sql}");
}

#[test]
fn rollback_wrap_refuses_input_that_could_swallow_its_rollback() {
    for (db, sql) in [
        (DatabaseType::MySql, "DELETE FROM t; SELECT '"),
        (DatabaseType::MySql, "DELETE FROM t; /*"),
        (DatabaseType::Postgres, "DELETE FROM t; SELECT $$"),
        (DatabaseType::Sqlite, "DELETE FROM t /* note"),
        (DatabaseType::Oracle, "DELETE FROM t; SELECT q'[x"),
        (DatabaseType::SqlServer, "DELETE FROM t; SELECT '"),
        // Only the NO_BACKSLASH_ESCAPES reading ends inside a literal.
        (DatabaseType::MySql, r"DELETE FROM t WHERE a = 'x\'"),
    ] {
        let message = refused(db, sql);
        assert!(message.contains("unterminated"), "{db:?}: {sql}: {message}");
    }
}

#[test]
fn mysql_rollback_wrap_refuses_more_implicit_commits() {
    for sql in [
        "LOCK TABLES t WRITE; DELETE FROM t",
        "UNLOCK TABLES",
        "OPTIMIZE TABLE t",
        "FLUSH PRIVILEGES",
        "SET PASSWORD = 'x'",
        "/*!50003 DROP TABLE t */ SELECT 1",
    ] {
        let message = refused(DatabaseType::MySql, sql);
        assert!(message.contains("implicitly commits"), "{sql}: {message}");
    }
}

/// A SELECT-only connection runs session-scoped SET statements such as
/// `SET NOCOUNT ON;` — they change how the session reports or plans, not
/// data (the reported "statement starts with SET, not SELECT" refusal).
#[test]
fn select_only_allows_session_scoped_set() {
    let mut p = profile(DatabaseType::SqlServer);
    p.block_all_but_select = true;
    for sql in [
        "SET NOCOUNT ON;",
        "SET NOCOUNT ON; SELECT * FROM t",
        "SET NOCOUNT ON\nSELECT a, b FROM t",
        "SET NOCOUNT ON\nSET XACT_ABORT ON\nSELECT 1",
        "SET ANSI_NULLS, ANSI_WARNINGS ON; SELECT 1",
        "SET STATISTICS IO, TIME ON; SELECT 1",
        "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; SELECT 1",
        "SET LOCK_TIMEOUT 5000; SELECT 1",
    ] {
        assert!(
            apply_connection_policy(&p, sql).is_ok(),
            "{sql}: {:?}",
            apply_connection_policy(&p, sql)
        );
    }
    for (db, sql) in [
        (
            DatabaseType::Postgres,
            "SET statement_timeout = '5s'; SELECT 1",
        ),
        (
            DatabaseType::Postgres,
            "SET LOCAL search_path TO sales, public; SELECT 1",
        ),
        (DatabaseType::Postgres, "SET TIME ZONE 'UTC'; SELECT now()"),
        (DatabaseType::Postgres, "SET enable_seqscan = off; SELECT 1"),
        (
            DatabaseType::MySql,
            "SET SESSION max_execution_time = 1000; SELECT 1",
        ),
        (
            DatabaseType::MySql,
            "SET @from = '2026-01-01', @to = '2026-02-01'; SELECT @from",
        ),
        (
            DatabaseType::MySql,
            "SET @@session.time_zone = '+00:00'; SELECT 1",
        ),
    ] {
        assert!(!blocked_for(db, sql), "{db:?}: {sql}");
    }
}

/// SET forms that reach beyond this session's reporting/planning stay
/// refused: server-wide scope, privilege or role changes, lifting a
/// read-only transaction, lexing changes, unknown options, and anything
/// mutating that rides along.
#[test]
fn select_only_still_refuses_other_set_statements() {
    for (db, sql) in [
        (DatabaseType::MySql, "SET GLOBAL max_connections = 1"),
        (DatabaseType::MySql, "SET PERSIST max_execution_time = 1"),
        (DatabaseType::MySql, "SET @@global.time_zone = '+00:00'"),
        (DatabaseType::MySql, "SET PASSWORD = 'x'"),
        (DatabaseType::MySql, "SET DEFAULT ROLE ALL TO u"),
        (DatabaseType::MySql, "SET NAMES gbk; SELECT 1"),
        (
            DatabaseType::MySql,
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 1",
        ),
        (DatabaseType::MySql, "SET SESSION TRANSACTION READ WRITE"),
        (
            DatabaseType::MySql,
            "SET time_zone = '+00:00', sql_mode = ''",
        ),
        (DatabaseType::Postgres, "SET ROLE admin; SELECT 1"),
        (DatabaseType::Postgres, "SET SESSION AUTHORIZATION admin"),
        (DatabaseType::Postgres, "SET TRANSACTION READ WRITE"),
        (
            DatabaseType::Postgres,
            "SET default_transaction_read_only = off",
        ),
        (
            DatabaseType::Postgres,
            "SET session_replication_role = replica",
        ),
        (
            DatabaseType::Postgres,
            "SET standard_conforming_strings = off",
        ),
        (
            DatabaseType::SqlServer,
            "SET QUOTED_IDENTIFIER OFF; SELECT 1",
        ),
        (DatabaseType::SqlServer, "SET IMPLICIT_TRANSACTIONS ON"),
        (DatabaseType::SqlServer, "SET NOCOUNT ON; DELETE FROM t"),
        (DatabaseType::SqlServer, "SET NOCOUNT ON\nDELETE FROM t"),
        (
            DatabaseType::SqlServer,
            "SET NOCOUNT ON\nSET CONTEXT_INFO 0x01\nSELECT 1",
        ),
        (DatabaseType::SqlServer, "SET @x = (SELECT 1 INTO #t)"),
        (
            DatabaseType::SqlServer,
            "SET NOCOUNT /* x */ ON; DROP TABLE t",
        ),
    ] {
        assert!(blocked_for(db, sql), "{db:?}: {sql}");
    }
}
