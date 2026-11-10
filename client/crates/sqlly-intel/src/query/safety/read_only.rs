//! Database-enforced read-only execution for SQL the AI, the agent tools, or
//! an MCP client composed on its own.
//!
//! The SELECT-only classifier is a parser: it cannot see a `SELECT` that
//! writes through a function, a data-modifying CTE on PostgreSQL, or a
//! `SELECT … INTO`. This module adds the second layer the database itself
//! enforces, per engine:
//!
//! - PostgreSQL wire: `BEGIN READ ONLY; … ROLLBACK;`
//! - MySQL wire: `START TRANSACTION READ ONLY; … ROLLBACK;`
//! - SQLite: `PRAGMA query_only = 1` around the statement (applied by the
//!   driver, which restores it even when the statement fails)
//! - TDS (SQL Server / Azure SQL): there is no server-side read-only
//!   transaction, so the batch runs inside a transaction that is always
//!   rolled back, and [`read_only_violation`] screens out statements whose
//!   side effect survives a rollback (`EXEC`, `sp_`/`xp_` calls outside a
//!   read-only allowlist, `INTO`, `OPENROWSET`/`OPENQUERY`/`BULK`, …).
//!
//! [`explain_read_only_refusal`] turns the engine's raw refusal into the
//! plain message a tool result should carry.

use super::classify::BlockedSpan;
use super::statements::{Destination, unit_views};
use crate::query::tokenizer::TokenKind;
use sqlly_core::{ConnectionKind, DatabaseType};

/// Prefix every read-only refusal message starts with, whether it came from
/// the client-side screen or from the database, so hosts can recognise one.
pub const READ_ONLY_REFUSAL_PREFIX: &str = "blocked: AI queries run read-only";

/// A statement the read-only screen refuses before it reaches the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyViolation {
    /// Uppercase name of the offending token, for the message.
    pub name: String,
    /// Why it is refused even though the batch is rolled back.
    pub reason: &'static str,
    /// Span of the offending token in the original SQL.
    pub span: BlockedSpan,
}

impl ReadOnlyViolation {
    pub fn display_message(&self) -> sqlly_core::DisplayMessage {
        let key = match self.reason {
            "it writes or changes the server (only read-only Redis commands may run)" => {
                "services.safety.read_only.redis_write"
            }
            "a procedure or dynamic batch can have effects a rollback cannot undo" => {
                "services.safety.read_only.dynamic_batch"
            }
            "it reaches another server or the file system outside the transaction" => {
                "services.safety.read_only.external_effect"
            }
            "extended procedures run outside the transaction" => {
                "services.safety.read_only.extended_procedure"
            }
            "only a small allowlist of read-only system procedures may run" => {
                "services.safety.read_only.procedure_allowlist"
            }
            "its effect is not undone by a read-only transaction" => {
                "services.safety.read_only.side_effect_function"
            }
            _ => "services.safety.read_only.other",
        };
        let message = sqlly_core::DisplayMessage::new(key, self.message())
            .with_arg("statement_name", self.name.clone());
        if key == "services.safety.read_only.other" {
            message.with_arg("reason", self.reason)
        } else {
            message
        }
    }

    /// English compatibility fallback; localized callers use [`Self::display_message`].
    pub fn message(&self) -> String {
        format!(
            "{READ_ONLY_REFUSAL_PREFIX}; {} is not allowed because {}",
            self.name, self.reason
        )
    }
}

/// Wrap `sql` in the engine's server-enforced read-only form. SQLite returns
/// the SQL unchanged: its `PRAGMA query_only` is a session switch the driver
/// applies (and restores) around the statement rather than batch text.
/// ClickHouse has no transactions to wrap in, so it too runs the SQL as-is
/// behind the SELECT-only parser gate.
pub fn read_only_wrap_for(db: DatabaseType, sql: &str) -> String {
    // The user's last statement is terminated first so the closing ROLLBACK
    // is a statement of its own (see `policy::terminated_for_wrap`).
    let terminated = super::policy::terminated_for_wrap(db, sql);
    let trimmed = terminated.as_str();
    match db.connection_kind() {
        ConnectionKind::Tds => super::policy::wrap_in_rollback_transaction(trimmed),
        // Leading ROLLBACK: START TRANSACTION would implicitly COMMIT whatever
        // an earlier failed run left open on this connection.
        ConnectionKind::MySqlWire => {
            format!("ROLLBACK;\nSTART TRANSACTION READ ONLY;\n{trimmed}\nROLLBACK;")
        }
        ConnectionKind::PostgresWire => format!("BEGIN READ ONLY;\n{trimmed}\nROLLBACK;"),
        ConnectionKind::SqliteFile | ConnectionKind::ClickHouseHttp => sql.trim().to_string(),
        // `SET TRANSACTION READ ONLY` must be the first statement of the
        // transaction; the driver runs read-only sessions with autocommit
        // off so the trailing ROLLBACK closes it again.
        ConnectionKind::OracleNet => format!("SET TRANSACTION READ ONLY;\n{trimmed}\nROLLBACK;"),
        // Redis: the command allowlist is the whole enforcement.
        ConnectionKind::Resp => sql.trim().to_string(),
        _ => format!("BEGIN;\n{trimmed}\nROLLBACK;"),
    }
}

/// SQL Server system procedures that only read catalog/session state and are
/// safe to call under the read-only screen. Everything else spelled `sp_…` or
/// `xp_…` is refused: a procedure body can write, and extended procedures run
/// outside the transaction entirely.
const TDS_READ_ONLY_PROCEDURES: &[&str] = &[
    "SP_COLUMNS",
    "SP_DATABASES",
    "SP_DATATYPE_INFO",
    "SP_DEPENDS",
    "SP_DESCRIBE_FIRST_RESULT_SET",
    "SP_FKEYS",
    "SP_HELP",
    "SP_HELPCONSTRAINT",
    "SP_HELPDB",
    "SP_HELPFILE",
    "SP_HELPINDEX",
    "SP_HELPTEXT",
    "SP_HELPTRIGGER",
    "SP_PKEYS",
    "SP_SERVER_INFO",
    "SP_SPACEUSED",
    "SP_SPECIAL_COLUMNS",
    "SP_SPROC_COLUMNS",
    "SP_STATISTICS",
    "SP_STORED_PROCEDURES",
    "SP_TABLES",
    "SP_WHO",
    "SP_WHO2",
];

/// SQL Server words whose effect is not undone by rolling the batch back.
fn tds_reason(name: &str) -> Option<&'static str> {
    match name {
        "EXEC" | "EXECUTE" => {
            Some("a procedure or dynamic batch can have effects a rollback cannot undo")
        }
        "INTO" => Some("SELECT INTO creates a table"),
        "OPENROWSET" | "OPENQUERY" | "OPENDATASOURCE" => {
            Some("it reaches another server or the file system outside the transaction")
        }
        "BULK" => Some("BULK operations touch the server's file system"),
        _ => None,
    }
}

/// PostgreSQL functions whose effect is not covered by a read-only
/// transaction (signals, configuration, backups, replication, session locks).
const PG_SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_SWITCH_WAL",
    "PG_PROMOTE",
    "PG_NOTIFY",
    "SET_CONFIG",
    "DBLINK",
    "DBLINK_EXEC",
    "PG_ADVISORY_LOCK",
    "PG_ADVISORY_LOCK_SHARED",
    "PG_TRY_ADVISORY_LOCK",
    "PG_TRY_ADVISORY_LOCK_SHARED",
    "PG_LOGICAL_EMIT_MESSAGE",
    "LO_UNLINK",
];

/// PostgreSQL function-name prefixes with the same problem as
/// [`PG_SIDE_EFFECT_FUNCTIONS`] (families too long to list one by one).
const PG_SIDE_EFFECT_PREFIXES: &[&str] = &[
    "PG_STAT_RESET",
    "PG_BACKUP_",
    "PG_START_BACKUP",
    "PG_STOP_BACKUP",
    "PG_CREATE_",
    "PG_DROP_",
];

/// MySQL functions that change session-visible state a read-only transaction
/// does not cover (named locks outlive the rollback on a pooled connection).
const MYSQL_SIDE_EFFECT_FUNCTIONS: &[&str] = &["GET_LOCK", "RELEASE_LOCK", "RELEASE_ALL_LOCKS"];

/// Screen `sql` for the engine's read-only mode: the first token whose side
/// effect the server-side mode would not stop, or `None` when the batch is
/// fit to run. Read through the security lexer like the dangerous-artifact
/// scan: names in string literals and ordinary comments are inert,
/// executable MySQL comment payloads are live in the readings that run them,
/// and quoted/bracketed spellings still match.
pub fn read_only_violation(db: DatabaseType, sql: &str) -> Option<ReadOnlyViolation> {
    let kind = db.connection_kind();
    if kind == ConnectionKind::Resp {
        // Redis commands are judged by name against the read-only allowlist.
        let (line, name) = super::redis::first_write_command(sql)?;
        return Some(ReadOnlyViolation {
            name,
            reason: "it writes or changes the server (only read-only Redis commands may run)",
            span: BlockedSpan {
                start: 0,
                end: sql.len(),
            },
        })
        .map(|mut v| {
            v.name = format!("line {line}: {}", v.name);
            v
        });
    }
    // Every execution unit, under every reading the engine could apply: a
    // name hidden from one reading (inside what it thinks is a literal) is
    // still judged by the reading that sees it as code.
    for unit in unit_views(sql, Destination::Engine(db)) {
        for view in &unit.views.views {
            // The previous word, so `INTO` is judged as the SELECT INTO it is
            // meant to catch and not as the `INSERT INTO`/`MERGE INTO` the
            // SELECT-only gate already owns.
            let mut previous = String::new();
            for token in view.tokens() {
                if !matches!(token.kind, TokenKind::Identifier | TokenKind::Keyword) {
                    continue;
                }
                let name = normalize_name(token.text);
                let reason = match kind {
                    ConnectionKind::Tds => tds_violation(&name, &previous),
                    ConnectionKind::PostgresWire => pg_violation(&name),
                    ConnectionKind::MySqlWire => mysql_violation(&name),
                    _ => None,
                };
                previous = name.clone();
                if let Some(reason) = reason {
                    let (start, end) = unit.map_span(token.start, token.end);
                    return Some(ReadOnlyViolation {
                        name,
                        reason,
                        span: BlockedSpan { start, end },
                    });
                }
            }
        }
    }
    None
}

fn tds_violation(name: &str, previous: &str) -> Option<&'static str> {
    if name == "INTO" && matches!(previous, "INSERT" | "MERGE") {
        return None;
    }
    if let Some(reason) = tds_reason(name) {
        return Some(reason);
    }
    // Procedure names may arrive schema-qualified as one identifier token
    // (`sys.sp_executesql`, `master..xp_cmdshell`); judge the last segment.
    let last = name.rsplit('.').next().unwrap_or(name);
    if last.starts_with("XP_") {
        return Some("extended procedures run outside the transaction");
    }
    if last.starts_with("SP_") && !TDS_READ_ONLY_PROCEDURES.contains(&last) {
        return Some("only a small allowlist of read-only system procedures may run");
    }
    None
}

fn pg_violation(name: &str) -> Option<&'static str> {
    let last = name.rsplit('.').next().unwrap_or(name);
    if PG_SIDE_EFFECT_FUNCTIONS.contains(&last)
        || PG_SIDE_EFFECT_PREFIXES
            .iter()
            .any(|prefix| last.starts_with(prefix))
    {
        return Some("its effect is not undone by a read-only transaction");
    }
    None
}

fn mysql_violation(name: &str) -> Option<&'static str> {
    let last = name.rsplit('.').next().unwrap_or(name);
    if MYSQL_SIDE_EFFECT_FUNCTIONS.contains(&last) {
        return Some("its effect is not undone by a read-only transaction");
    }
    None
}

/// Uppercase an identifier token with quoting stripped from each segment, so
/// `[xp_cmdshell]`, `"OPENROWSET"` and `` `get_lock` `` still match.
fn normalize_name(text: &str) -> String {
    text.split('.')
        .map(|segment| {
            segment
                .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
                .to_ascii_uppercase()
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Whether an engine error message is the database refusing a write under
/// the read-only mode this module sets up.
pub fn is_read_only_refusal(db: DatabaseType, message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    match db.connection_kind() {
        // SQLSTATE 25006: "cannot execute INSERT in a read-only transaction".
        ConnectionKind::PostgresWire => lower.contains("read-only transaction"),
        // ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION (1792).
        ConnectionKind::MySqlWire => lower.contains("read only transaction"),
        // SQLITE_READONLY under `PRAGMA query_only`.
        ConnectionKind::SqliteFile => lower.contains("readonly database"),
        // A read-only replica, or an ACL user without write permissions.
        ConnectionKind::Resp => {
            lower.contains("readonly you can't write") || lower.contains("noperm")
        }
        _ => false,
    }
}

/// The plain message a tool result carries when the database refused a write:
/// says the statement was blocked because AI queries run read-only instead of
/// surfacing the raw driver error alone. `None` for any other error.
pub fn explain_read_only_refusal_message(
    db: DatabaseType,
    message: &str,
) -> Option<sqlly_core::DisplayMessage> {
    if !is_read_only_refusal(db, message) {
        return None;
    }
    Some(
        sqlly_core::DisplayMessage::new(
            "services.safety.read_only.database_refusal",
            format!("{READ_ONLY_REFUSAL_PREFIX} on this connection, and the database refused a write ({})", message.trim()),
        )
        .with_arg("detail", message.trim()),
    )
}

pub fn explain_read_only_refusal(db: DatabaseType, message: &str) -> Option<String> {
    explain_read_only_refusal_message(db, message).map(sqlly_core::DisplayMessage::into_fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_uses_each_engines_read_only_syntax() {
        assert_eq!(
            read_only_wrap_for(DatabaseType::Postgres, " SELECT 1 "),
            "BEGIN READ ONLY;\nSELECT 1\n;\nROLLBACK;"
        );
        assert_eq!(
            read_only_wrap_for(DatabaseType::MySql, "SELECT 1"),
            "ROLLBACK;\nSTART TRANSACTION READ ONLY;\nSELECT 1\n;\nROLLBACK;"
        );
        assert_eq!(
            read_only_wrap_for(DatabaseType::MariaDb, "SELECT 1;"),
            "ROLLBACK;\nSTART TRANSACTION READ ONLY;\nSELECT 1;\nROLLBACK;"
        );
        assert_eq!(
            read_only_wrap_for(DatabaseType::SqlServer, "SELECT 1"),
            "SET XACT_ABORT ON;\nBEGIN TRANSACTION;\nSELECT 1\nROLLBACK TRANSACTION;"
        );
        assert_eq!(
            read_only_wrap_for(DatabaseType::AzureSqlDatabase, "SELECT 1"),
            "SET XACT_ABORT ON;\nBEGIN TRANSACTION;\nSELECT 1\nROLLBACK TRANSACTION;"
        );
        assert_eq!(
            read_only_wrap_for(DatabaseType::Oracle, "SELECT 1"),
            "SET TRANSACTION READ ONLY;\nSELECT 1\n;\nROLLBACK;"
        );
        // SQLite is a session switch the driver applies, never batch text.
        assert_eq!(
            read_only_wrap_for(DatabaseType::Sqlite, "SELECT 1"),
            "SELECT 1"
        );
    }

    #[test]
    fn sql_server_screen_refuses_side_effects_that_survive_rollback() {
        for (sql, name) in [
            ("EXEC dbo.Purge", "EXEC"),
            ("SELECT 1; EXECUTE sp_executesql N'DELETE t'", "EXECUTE"),
            ("SELECT * INTO dbo.copy FROM dbo.t", "INTO"),
            (
                "SELECT * FROM OPENROWSET('SQLNCLI', 'x', 'SELECT 1')",
                "OPENROWSET",
            ),
            ("SELECT * FROM OPENQUERY(linked, 'DELETE t')", "OPENQUERY"),
            (
                "SELECT * FROM OPENDATASOURCE('x', 'y').db.dbo.t",
                "OPENDATASOURCE",
            ),
            ("SELECT * FROM t WITH (BULK 1)", "BULK"),
            ("SELECT master..xp_cmdshell('dir')", "XP_CMDSHELL"),
            ("SELECT * FROM [xp_dirtree]", "XP_DIRTREE"),
            ("SELECT sys.sp_executesql", "SP_EXECUTESQL"),
            ("SELECT * FROM sp_whatever", "SP_WHATEVER"),
        ] {
            let violation = read_only_violation(DatabaseType::SqlServer, sql)
                .unwrap_or_else(|| panic!("{sql} must be refused"));
            assert!(violation.name.ends_with(name), "{sql}: {}", violation.name);
            assert!(violation.message().starts_with(READ_ONLY_REFUSAL_PREFIX));
        }
    }

    #[test]
    fn sql_server_screen_allows_reads_and_the_allowlist() {
        for sql in [
            "SELECT TOP (10) * FROM dbo.Orders WHERE Status = 'EXEC'",
            "SELECT * FROM sys.dm_exec_requests",
            "SELECT name FROM sys.objects -- EXEC dbo.P",
            "/* INTO */ SELECT 1",
            "sp_help 'dbo.t'",
            "sp_who2",
            "SELECT * FROM dbo.Intolerance",
            "WITH c AS (SELECT 1 AS n) SELECT n FROM c",
        ] {
            assert!(
                read_only_violation(DatabaseType::SqlServer, sql).is_none(),
                "{sql} must pass"
            );
        }
    }

    #[test]
    fn sql_server_screen_leaves_insert_into_to_the_select_only_gate() {
        // The screen is about SELECT INTO; INSERT INTO / MERGE INTO are the
        // classifier's business (and are refused there), so a write that
        // reaches the rollback wrap on purpose — the live proof — is not
        // mis-flagged by the INTO rule.
        assert!(
            read_only_violation(
                DatabaseType::SqlServer,
                "INSERT INTO t VALUES (1); SELECT 1"
            )
            .is_none()
        );
        assert!(
            read_only_violation(DatabaseType::SqlServer, "MERGE INTO t USING s ON 1=1").is_none()
        );
        assert!(read_only_violation(DatabaseType::SqlServer, "SELECT 1 INTO t").is_some());
    }

    #[test]
    fn sql_server_screen_reports_the_offending_span() {
        let sql = "SELECT 1;\nSELECT * INTO x FROM t";
        let violation = read_only_violation(DatabaseType::SqlServer, sql).unwrap();
        assert_eq!(&sql[violation.span.start..violation.span.end], "INTO");
    }

    #[test]
    fn postgres_screen_refuses_functions_a_read_only_transaction_ignores() {
        for sql in [
            "SELECT pg_terminate_backend(123)",
            "SELECT pg_cancel_backend(pid) FROM pg_stat_activity",
            "SELECT pg_reload_conf()",
            "SELECT set_config('work_mem', '1GB', false)",
            "SELECT pg_stat_reset()",
            "SELECT pg_stat_reset_shared('bgwriter')",
            "SELECT pg_create_restore_point('x')",
            "SELECT pg_drop_replication_slot('s')",
            "SELECT pg_advisory_lock(1)",
            "SELECT * FROM dblink('dbname=x', 'DELETE FROM t') AS t(a int)",
            "SELECT \"pg_terminate_backend\"(1)",
        ] {
            assert!(
                read_only_violation(DatabaseType::Postgres, sql).is_some(),
                "{sql} must be refused"
            );
        }
        for sql in [
            "SELECT pg_advisory_xact_lock(1)",
            "SELECT * FROM pg_stat_activity",
            "SELECT 'pg_terminate_backend(1)'",
            "SELECT pg_sleep(1)",
            "SELECT current_setting('work_mem')",
        ] {
            assert!(
                read_only_violation(DatabaseType::Postgres, sql).is_none(),
                "{sql} must pass"
            );
        }
        // The SQL Server list does not apply to PostgreSQL (INTO is judged by
        // the classifier; EXEC does not exist).
        assert!(read_only_violation(DatabaseType::Postgres, "SELECT * FROM exec").is_none());
    }

    #[test]
    fn mysql_screen_refuses_session_locks_and_sees_executable_comments() {
        assert!(read_only_violation(DatabaseType::MySql, "SELECT GET_LOCK('x', 10)").is_some());
        assert!(read_only_violation(DatabaseType::MariaDb, "SELECT `release_lock`('x')").is_some());
        assert!(
            read_only_violation(
                DatabaseType::MySql,
                "SELECT 1 /*!50000 , GET_LOCK('x', 1) */"
            )
            .is_some()
        );
        assert!(read_only_violation(DatabaseType::MySql, "SELECT 1 /* GET_LOCK */").is_none());
        assert!(read_only_violation(DatabaseType::MySql, "SELECT IS_FREE_LOCK('x')").is_none());
    }

    #[test]
    fn sqlite_has_no_client_side_screen() {
        assert!(read_only_violation(DatabaseType::Sqlite, "SELECT get_lock('x')").is_none());
        assert!(read_only_violation(DatabaseType::Sqlite, "EXEC x").is_none());
    }

    #[test]
    fn engine_refusals_are_explained_in_plain_words() {
        let pg = explain_read_only_refusal(
            DatabaseType::Postgres,
            "ERROR: cannot execute INSERT in a read-only transaction",
        )
        .unwrap();
        assert!(pg.starts_with(READ_ONLY_REFUSAL_PREFIX), "{pg}");
        assert!(pg.contains("cannot execute INSERT"), "{pg}");
        assert!(
            explain_read_only_refusal(
                DatabaseType::MySql,
                "Server error: `ERROR 25006 (1792): Cannot execute statement in a READ ONLY transaction.`"
            )
            .is_some()
        );
        assert!(
            explain_read_only_refusal(DatabaseType::Sqlite, "attempt to write a readonly database")
                .is_some()
        );
        // Unrelated errors and engines without a server mode pass through.
        assert!(
            explain_read_only_refusal(DatabaseType::Postgres, "relation t does not exist")
                .is_none()
        );
        assert!(
            explain_read_only_refusal(DatabaseType::SqlServer, "read-only transaction").is_none()
        );
    }
}
