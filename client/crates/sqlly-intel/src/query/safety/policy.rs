//! Connection policy application: the SELECT-only gate, rollback wrapping
//! (T-SQL and engine-aware forms), the refusals that keep the rollback wrap's
//! "nothing sticks" promise (implicit commits, transaction control, input the
//! appended ROLLBACK could be swallowed by), and the non-executing preview
//! that mirrors it all for the UI.
//!
//! Everything is judged through the security lexer (`statements`): per
//! execution unit, under every reading the destination engine could apply.

use super::batch_lead::{tsql_batch_lead_violation, tsql_batch_must_lead};
use super::classify::classify;
use super::read_only::{read_only_violation, read_only_wrap_for};
use super::statements::{Destination, is_pure_use_batch, unit_views};
use crate::query::tokenizer::TokenKind;
use sqlly_core::{ConnectionKind, ConnectionProfile, DatabaseType, StudioError, StudioResult};
use sqlly_lang::seclex::{self, LexTarget, View};

/// The database-enforced read-only layer for a profile that carries
/// `enforce_read_only`: screens out statements whose side effect the engine's
/// read-only mode would not stop, then wraps the batch in that mode. Runs
/// after the SELECT-only gate and replaces the plain rollback wrap (nesting
/// the two would only add a second, redundant transaction). A lone `USE` is
/// left raw so the context switch takes effect for the session.
fn apply_read_only_enforcement(profile: &ConnectionProfile, sql: &str) -> StudioResult<String> {
    let db = profile.database_type;
    if let Some(violation) = read_only_violation(db, sql) {
        return Err(StudioError::PermissionDenied(violation.message()));
    }
    let dest = Destination::Engine(db);
    if is_pure_use_batch(sql, dest) {
        return Ok(sql.to_string());
    }
    if let Some(reason) = wrap_hazard(db, sql, "read-only transaction") {
        return Err(StudioError::PermissionDenied(format!(
            "{READ_ONLY_PREFIX}; {reason}",
            READ_ONLY_PREFIX = super::read_only::READ_ONLY_REFUSAL_PREFIX
        )));
    }
    Ok(read_only_wrap_for(db, sql))
}

/// Gate-only view of [`apply_connection_policy_for`]: raises every refusal
/// the driver would raise (SELECT-only, read-only screen) without wrapping.
/// Hosts call this to refuse early with a clear reason and still hand the
/// driver the RAW SQL, so the batch is wrapped exactly once, by the driver.
pub fn check_connection_policy_for(profile: &ConnectionProfile, sql: &str) -> StudioResult<()> {
    apply_connection_policy_for(profile, sql).map(|_| ())
}

/// Computes the effective SQL that will actually run under a profile's policy,
/// for a staging/production preview, without executing it. Mirrors
/// [`apply_connection_policy`] but never errors: a blocked batch is returned
/// unchanged with `allowed = false` so the UI can show why it would be refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSql {
    pub allowed: bool,
    pub reason: Option<String>,
    pub wrapped_in_rollback: bool,
    pub sql: String,
}

impl EffectiveSql {
    pub fn reason_message(&self) -> Option<sqlly_core::DisplayMessage> {
        super::classify::SafetyClassification {
            select_only: self.allowed,
            reason: self.reason.clone(),
            blocked_span: None,
        }
        .reason_message()
    }
}

pub fn preview_effective_sql(profile: &ConnectionProfile, sql: &str) -> EffectiveSql {
    let refused = |reason: String| EffectiveSql {
        allowed: false,
        reason: Some(reason),
        wrapped_in_rollback: false,
        sql: sql.to_string(),
    };
    if profile.database_type.is_command_engine() {
        return match super::redis::apply_redis_policy(profile, sql) {
            Ok(_) => EffectiveSql {
                allowed: true,
                reason: None,
                wrapped_in_rollback: false,
                sql: sql.to_string(),
            },
            Err(error) => refused(error.to_string()),
        };
    }
    let is_tds = profile.database_type.connection_kind() == ConnectionKind::Tds;
    if profile.block_all_but_select {
        let classification = classify(sql, Destination::Engine(profile.database_type));
        if !classification.select_only {
            return EffectiveSql {
                reason: classification.reason,
                ..refused(String::new())
            };
        }
    }
    // Mirror `apply_connection_policy`: a routine definition that is not first
    // in its batch cannot run (SQL Server error 111), so the preview reports it
    // as refused with the same clear reason rather than showing a wrapped batch
    // that would fail.
    if is_tds && let Some(violation) = tsql_batch_lead_violation(sql) {
        return refused(violation.message(profile.auto_wrap_rollback));
    }
    let policy = if is_tds {
        apply_connection_policy(profile, sql)
    } else {
        apply_connection_policy_for(profile, sql)
    };
    match policy {
        Ok(effective) => EffectiveSql {
            allowed: true,
            reason: None,
            wrapped_in_rollback: profile.auto_wrap_rollback && effective != sql,
            sql: effective,
        },
        Err(error) => refused(error.to_string()),
    }
}

pub fn wrap_in_rollback_transaction(sql: &str) -> String {
    let trimmed = sql.trim();
    if tsql_batch_must_lead(trimmed) {
        // CREATE/ALTER PROCEDURE (and friends) must be the first statement of
        // their batch; the transaction preamble would make the server reject
        // the whole batch (Msg 111), so the rollback connection could never
        // deploy them at all. Running the definition through EXEC gives it a
        // batch of its own *inside* the transaction: the deploy executes,
        // is observable, and still rolls back. T-SQL literals have no
        // backslash escapes, so doubling `'` is the complete quoting.
        let quoted = trimmed.replace('\'', "''");
        return format!(
            "SET XACT_ABORT ON;\nBEGIN TRANSACTION;\nEXEC(N'{quoted}');\nROLLBACK TRANSACTION;"
        );
    }
    format!("SET XACT_ABORT ON;\nBEGIN TRANSACTION;\n{trimmed}\nROLLBACK TRANSACTION;")
}

/// Oracle scripts are split on `;` (and `/`) before OCI sees them, so a
/// statement the user left unterminated must be closed before the wrap
/// appends its own `ROLLBACK` — otherwise `ROLLBACK` reads as a table alias.
/// The terminator goes on its own line: a trailing `-- comment` would
/// otherwise swallow it.
pub fn oracle_terminated(sql: &str) -> String {
    let trimmed = sql.trim_end();
    if last_code_is(sql, DatabaseType::Oracle, |t| t == ";" || t == "/") {
        trimmed.to_string()
    } else {
        format!("{trimmed}\n;")
    }
}

/// Whether the last live token of `sql`, under every reading `db` could apply,
/// satisfies `pred` (`false` when the readings disagree or nothing is live).
fn last_code_is(sql: &str, db: DatabaseType, pred: impl Fn(&str) -> bool) -> bool {
    let views = seclex::views(sql, LexTarget::for_database(db));
    views.views.iter().all(|view| {
        view.tokens()
            .into_iter()
            .rev()
            .find(|t| t.kind != TokenKind::Whitespace)
            .is_some_and(|t| pred(t.text))
    })
}

/// The user's SQL with its final statement terminated, so the ROLLBACK the
/// wrap appends is a statement of its own. Without this, `DELETE FROM t`
/// followed by `\nROLLBACK;` reads on MySQL and PostgreSQL as `DELETE FROM t
/// ROLLBACK` — ROLLBACK a table *alias* — and the delete is never undone. The
/// terminator goes on its own line so a trailing `-- comment` cannot swallow
/// it, and is added only when absent (an empty statement is an error on
/// MySQL, which would stop the batch before the ROLLBACK).
pub(super) fn terminated_for_wrap(db: DatabaseType, sql: &str) -> String {
    let trimmed = sql.trim();
    match db.connection_kind() {
        // ROLLBACK is reserved in T-SQL: it can never be an alias.
        ConnectionKind::Tds => trimmed.to_string(),
        ConnectionKind::OracleNet => oracle_terminated(trimmed),
        _ if last_code_is(trimmed, db, |t| t == ";") => trimmed.to_string(),
        _ => format!("{trimmed}\n;"),
    }
}

/// Engine-aware rollback wrap: same intent as
/// [`wrap_in_rollback_transaction`] (run, observe, undo) phrased in the
/// engine's transaction syntax. Falls back to ANSI `BEGIN`/`ROLLBACK` for
/// engines without a distinct form.
pub fn wrap_in_rollback_transaction_for(db: DatabaseType, sql: &str) -> String {
    let body = terminated_for_wrap(db, sql);
    match db.connection_kind() {
        ConnectionKind::Tds => wrap_in_rollback_transaction(sql),
        // The leading ROLLBACK discards anything a previous wrapped run left
        // open (a statement error stops a multi-statement batch before its
        // ROLLBACK); without it, START TRANSACTION would implicitly COMMIT
        // that leftover work.
        ConnectionKind::MySqlWire => format!("ROLLBACK;\nSTART TRANSACTION;\n{body}\nROLLBACK;"),
        // Oracle has no BEGIN statement (that opens a PL/SQL block): a
        // transaction starts implicitly with the first DML, and the driver
        // turns autocommit off under this policy so the trailing ROLLBACK
        // undoes everything.
        ConnectionKind::OracleNet => format!("{body}\nROLLBACK;"),
        // Postgres wire, SQLite, DuckDB and the ANSI-ish rest.
        _ => format!("BEGIN;\n{body}\nROLLBACK;"),
    }
}

/// Engine-aware [`apply_connection_policy`]: the same SELECT-only gate, with
/// the rollback wrap phrased for the profile's engine. The TDS driver keeps
/// calling `apply_connection_policy` (identical result for SQL Server);
/// non-TDS drivers call this.
pub fn apply_connection_policy_for(profile: &ConnectionProfile, sql: &str) -> StudioResult<String> {
    if sql.trim().is_empty() {
        return Err(StudioError::InvalidInput("Empty query".to_string()));
    }

    // Redis is not SQL: the same three gates are judged per command from
    // the shared allowlist, and there is never anything to wrap.
    if profile.database_type.is_command_engine() {
        return super::redis::apply_redis_policy(profile, sql).map(|_| sql.to_string());
    }

    let db = profile.database_type;
    let dest = Destination::Engine(db);
    if profile.block_all_but_select {
        select_only_gate(sql, dest)?;
    }

    if profile.enforce_read_only {
        return apply_read_only_enforcement(profile, sql);
    }

    if profile.auto_wrap_rollback && !is_pure_use_batch(sql, dest) {
        // ClickHouse has no transactions at all: a BEGIN/ROLLBACK wrap would
        // be rejected (or, worse, ignored) and every change would stick.
        if db.connection_kind() == ConnectionKind::ClickHouseHttp {
            return Err(StudioError::PermissionDenied(
                "connection policy wraps execution in a rolled-back transaction, but ClickHouse \
                 has no transactions to roll back — changes could not be undone. Disable the \
                 rollback wrap for this connection to run it."
                    .to_string(),
            ));
        }
        if let Some(reason) = wrap_hazard(db, sql, "rolled-back transaction") {
            return Err(StudioError::PermissionDenied(format!(
                "connection policy wraps execution in a rolled-back transaction, but {reason}. \
                 Disable the rollback wrap for this connection (or run \u{201c}Execute Without \
                 Rollback Wrap\u{201d}) to run it."
            )));
        }
        Ok(wrap_in_rollback_transaction_for(db, sql))
    } else {
        Ok(sql.to_string())
    }
}

fn select_only_gate(sql: &str, dest: Destination) -> StudioResult<()> {
    let classification = classify(sql, dest);
    if classification.select_only {
        return Ok(());
    }
    Err(StudioError::PermissionDenied(format!(
        "connection policy allows SELECT statements only: {}",
        classification
            .reason
            .unwrap_or_else(|| "query is not SELECT-only".to_string())
    )))
}

/// Why wrapping `sql` in a transaction that is rolled back (or read-only)
/// could fail to keep its promise, or `None` when it is safe to wrap:
///
/// - the text is ambiguous, or some reading ends inside an unterminated
///   literal/comment — the appended ROLLBACK could be swallowed by it;
/// - a statement ends or commits the wrapping transaction (COMMIT, ROLLBACK,
///   a transaction-starting BEGIN/START TRANSACTION/END/ABORT, SET
///   autocommit, PREPARE TRANSACTION): everything after it would run outside
///   the wrap and stick;
/// - MySQL/Oracle: a statement commits implicitly (DDL, grants, LOCK
///   TABLES, …).
fn wrap_hazard(db: DatabaseType, sql: &str, what: &str) -> Option<String> {
    for unit in unit_views(sql, Destination::Engine(db)) {
        // A T-SQL routine definition runs to the end of its batch and deploys
        // without executing its body, so COMMIT/ROLLBACK inside the body end
        // nothing (the definition itself runs inside the wrap via EXEC).
        let definition = db.connection_kind() == ConnectionKind::Tds
            && tsql_batch_must_lead(unit.unit.text.trim());
        if let Some(reason) = unit.views.ambiguity() {
            return Some(format!("the {what} cannot be applied safely: {reason}"));
        }
        for view in &unit.views.views {
            if let Some(u) = view.unterminated {
                return Some(format!(
                    "{} reads an unterminated {} at byte {} that could swallow the {what}'s closing ROLLBACK",
                    view.reading,
                    u.what.describe(),
                    u.offset + unit.unit.start
                ));
            }
            if !definition && let Some(verb) = transaction_control_verb(db, view) {
                return Some(format!(
                    "the batch contains {verb}, which would end the {what} early so later statements stick"
                ));
            }
            if let Some(verb) = implicit_commit_verb(db, view) {
                let engine = if db.connection_kind() == ConnectionKind::OracleNet {
                    "Oracle"
                } else {
                    "MySQL"
                };
                return Some(format!(
                    "{engine} implicitly commits {verb} statements — the change could not be undone"
                ));
            }
        }
    }
    None
}

/// Words of each statement (keywords and identifiers, uppercased), in order.
fn statement_words(view: &View) -> Vec<Vec<String>> {
    super::statements::statement_words(view)
        .into_iter()
        .map(|s| s.words)
        .collect()
}

/// A statement that ends, commits or restarts the wrapping transaction.
fn transaction_control_verb(db: DatabaseType, view: &View) -> Option<&'static str> {
    let kind = db.connection_kind();
    let tds = kind == ConnectionKind::Tds;
    let oracle = kind == ConnectionKind::OracleNet;
    for words in statement_words(view) {
        // COMMIT/ROLLBACK anywhere: T-SQL runs statements without
        // separators and Oracle hides them inside PL/SQL blocks.
        for (i, word) in words.iter().enumerate() {
            match word.as_str() {
                "COMMIT" => return Some("COMMIT"),
                // ROLLBACK TO [SAVEPOINT] only unwinds to a savepoint.
                "ROLLBACK" if tds || words.get(i + 1).map(String::as_str) != Some("TO") => {
                    return Some("ROLLBACK");
                }
                _ => {}
            }
        }
        if tds || oracle {
            continue;
        }
        let first = words.first().map(String::as_str);
        let second = words.get(1).map(String::as_str);
        match (first, second) {
            // BEGIN / BEGIN WORK / BEGIN TRANSACTION …: an implicit COMMIT on
            // MySQL, an error inside a transaction on SQLite and DuckDB (which
            // stops the batch before the ROLLBACK).
            (Some("BEGIN"), _) => return Some("BEGIN"),
            (Some("START"), Some("TRANSACTION")) => return Some("START TRANSACTION"),
            // PostgreSQL / SQLite spell COMMIT as END; PostgreSQL ROLLBACK as ABORT.
            (Some("END"), _) => return Some("END (COMMIT)"),
            (Some("ABORT"), _) => return Some("ABORT (ROLLBACK)"),
            (Some("PREPARE"), Some("TRANSACTION")) => return Some("PREPARE TRANSACTION"),
            (Some("SET"), _)
                if words
                    .iter()
                    .any(|w| w.trim_start_matches('@') == "AUTOCOMMIT") =>
            {
                return Some("SET autocommit");
            }
            _ => {}
        }
    }
    None
}

/// The leading verb of the first statement that would trigger an implicit
/// commit on MySQL (DDL, grants, table locks, administrative statements) or
/// Oracle (DDL), or `None`. `CREATE`/`DROP TEMPORARY TABLE` are exempt —
/// MySQL does not commit those.
fn implicit_commit_verb(db: DatabaseType, view: &View) -> Option<&'static str> {
    let kind = db.connection_kind();
    let mysql = kind == ConnectionKind::MySqlWire;
    if !mysql && kind != ConnectionKind::OracleNet {
        return None;
    }
    for words in statement_words(view) {
        let mut it = words.iter().map(String::as_str);
        let Some(first) = it.next() else { continue };
        let second = it.next();
        let verb = match first {
            "CREATE" | "DROP" if second == Some("TEMPORARY") => continue,
            "CREATE" => "CREATE",
            "DROP" => "DROP",
            "ALTER" => "ALTER",
            "TRUNCATE" => "TRUNCATE",
            "RENAME" => "RENAME",
            "GRANT" => "GRANT",
            "REVOKE" => "REVOKE",
            // MySQL "Statements That Cause an Implicit Commit".
            "LOCK" if mysql => "LOCK TABLES",
            "UNLOCK" if mysql => "UNLOCK TABLES",
            "ANALYZE" | "CHECK" | "OPTIMIZE" | "REPAIR" if mysql && second == Some("TABLE") => {
                "table maintenance"
            }
            "CACHE" | "FLUSH" | "RESET" | "INSTALL" | "UNINSTALL" if mysql => "administrative",
            "SET" if mysql && second == Some("PASSWORD") => "SET PASSWORD",
            "LOAD" if mysql && second == Some("INDEX") => "LOAD INDEX",
            "START" | "STOP" | "CHANGE" if mysql && second != Some("TRANSACTION") => {
                "replication control"
            }
            _ => continue,
        };
        return Some(verb);
    }
    None
}

pub fn apply_connection_policy(profile: &ConnectionProfile, sql: &str) -> StudioResult<String> {
    if sql.trim().is_empty() {
        return Err(StudioError::InvalidInput("Empty query".to_string()));
    }

    // The TDS driver calls this per GO batch; a profile that is not SQL
    // Server-typed (tests, legacy callers) is still read as T-SQL.
    let db = if profile.database_type.connection_kind() == ConnectionKind::Tds {
        profile.database_type
    } else {
        DatabaseType::SqlServer
    };
    let dest = Destination::Engine(db);
    if profile.block_all_but_select {
        select_only_gate(sql, dest)?;
    }

    // A routine definition (CREATE/ALTER PROCEDURE, FUNCTION, TRIGGER, VIEW, …)
    // must be the first statement of its batch; when the batch runs something
    // ahead of it SQL Server refuses the whole batch with error 111. Catch that
    // shape here and return the reason plainly — the rollback wrap below would
    // otherwise add its own BEGIN TRANSACTION and bury the cause under a raw
    // server error. Flagged regardless of the wrap, since the batch cannot run
    // either way; the message names the bypass only when the wrap is active.
    if let Some(violation) = tsql_batch_lead_violation(sql) {
        return Err(StudioError::InvalidInput(
            violation.message(profile.auto_wrap_rollback),
        ));
    }

    if profile.enforce_read_only {
        let mut tds = profile.clone();
        tds.database_type = db;
        return apply_read_only_enforcement(&tds, sql);
    }

    if profile.auto_wrap_rollback && !is_pure_use_batch(sql, dest) {
        if let Some(reason) = wrap_hazard(db, sql, "rolled-back transaction") {
            return Err(StudioError::PermissionDenied(format!(
                "connection policy wraps execution in a rolled-back transaction, but {reason}. \
                 Disable the rollback wrap for this connection (or run \u{201c}Execute Without \
                 Rollback Wrap\u{201d}) to run it."
            )));
        }
        Ok(wrap_in_rollback_transaction(sql))
    } else {
        Ok(sql.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{DatabaseType, SecretProvider, SecretRef};

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "password".to_string(),
            },
        )
    }

    #[test]
    fn rollback_wrap_uses_engine_syntax() {
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::SqlServer, "DELETE FROM t");
        assert!(wrapped.starts_with("SET XACT_ABORT ON;"));
        // The user's last statement is terminated on its own line before the
        // ROLLBACK, so ROLLBACK can never read as a table alias
        // (`DELETE FROM t ROLLBACK`) and a trailing `--` comment cannot
        // swallow the terminator. MySQL first discards anything a failed
        // earlier run left open instead of letting START TRANSACTION commit it.
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::MySql, "DELETE FROM t");
        assert_eq!(
            wrapped,
            "ROLLBACK;\nSTART TRANSACTION;\nDELETE FROM t\n;\nROLLBACK;"
        );
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::MySql, "DELETE FROM t;");
        assert_eq!(
            wrapped,
            "ROLLBACK;\nSTART TRANSACTION;\nDELETE FROM t;\nROLLBACK;"
        );
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::Postgres, "DELETE FROM t");
        assert_eq!(wrapped, "BEGIN;\nDELETE FROM t\n;\nROLLBACK;");
        let wrapped =
            wrap_in_rollback_transaction_for(DatabaseType::Postgres, "DELETE FROM t -- done");
        assert_eq!(wrapped, "BEGIN;\nDELETE FROM t -- done\n;\nROLLBACK;");
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::Sqlite, "DELETE FROM t");
        assert_eq!(wrapped, "BEGIN;\nDELETE FROM t\n;\nROLLBACK;");
        // Oracle: BEGIN would open a PL/SQL block, so only the ROLLBACK is
        // appended and the driver holds the transaction open.
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::Oracle, "DELETE FROM t");
        assert_eq!(wrapped, "DELETE FROM t\n;\nROLLBACK;");
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::Oracle, "DELETE FROM t; ");
        assert_eq!(wrapped, "DELETE FROM t;\nROLLBACK;");
        // A trailing line comment used to swallow Oracle's appended `;`.
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::Oracle, "DELETE FROM t -- x");
        assert_eq!(wrapped, "DELETE FROM t -- x\n;\nROLLBACK;");
        let wrapped = wrap_in_rollback_transaction_for(DatabaseType::Oracle, "BEGIN NULL; END;\n/");
        assert_eq!(wrapped, "BEGIN NULL; END;\n/\nROLLBACK;");
    }

    #[test]
    fn oracle_rollback_wrap_refuses_implicitly_committing_ddl() {
        let mut p = profile();
        p.database_type = DatabaseType::Oracle;
        p.auto_wrap_rollback = true;
        let err = apply_connection_policy_for(&p, "CREATE TABLE t (id NUMBER)").unwrap_err();
        assert!(
            err.to_string().contains("Oracle implicitly commits CREATE"),
            "{err}"
        );
        assert_eq!(
            apply_connection_policy_for(&p, "DELETE FROM t").unwrap(),
            "DELETE FROM t\n;\nROLLBACK;"
        );
    }

    #[test]
    fn policy_for_engine_gates_and_wraps() {
        let mut p = profile();
        p.database_type = DatabaseType::Postgres;
        p.block_all_but_select = true;
        assert!(apply_connection_policy_for(&p, "DROP TABLE t").is_err());
        assert!(apply_connection_policy_for(&p, "  ").is_err());
        assert_eq!(
            apply_connection_policy_for(&p, "SELECT 1").unwrap(),
            "SELECT 1"
        );

        p.block_all_but_select = false;
        p.auto_wrap_rollback = true;
        assert_eq!(
            apply_connection_policy_for(&p, "DELETE FROM t").unwrap(),
            "BEGIN;\nDELETE FROM t\n;\nROLLBACK;"
        );
    }

    #[test]
    fn rollback_wrapper_preserves_user_batch_inside_transaction() {
        assert_eq!(
            wrap_in_rollback_transaction(" SELECT 1; "),
            "SET XACT_ABORT ON;\nBEGIN TRANSACTION;\nSELECT 1;\nROLLBACK TRANSACTION;"
        );
    }

    #[test]
    fn connection_policy_blocks_non_select_when_enabled() {
        let mut profile = profile();
        profile.block_all_but_select = true;

        let err = apply_connection_policy(&profile, "DELETE FROM dbo.t").unwrap_err();
        match err {
            StudioError::PermissionDenied(message) => assert!(message.contains("SELECT")),
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[test]
    fn connection_policy_wraps_when_enabled() {
        let mut profile = profile();
        profile.auto_wrap_rollback = true;

        let sql = apply_connection_policy(&profile, "UPDATE dbo.t SET a = 1").unwrap();
        assert!(sql.starts_with("SET XACT_ABORT ON;\nBEGIN TRANSACTION;"));
        assert!(sql.contains("UPDATE dbo.t SET a = 1"));
        assert!(sql.ends_with("ROLLBACK TRANSACTION;"));
    }

    #[test]
    fn connection_policy_can_combine_select_only_and_rollback_wrapper() {
        let mut profile = profile();
        profile.block_all_but_select = true;
        profile.auto_wrap_rollback = true;

        let sql = apply_connection_policy(&profile, "SELECT 1").unwrap();
        assert!(sql.contains("SELECT 1"));
        assert!(sql.contains("ROLLBACK TRANSACTION"));
    }

    #[test]
    fn use_is_allowed_under_select_only_policy() {
        let mut profile = profile();
        profile.block_all_but_select = true;
        // A bare context switch and a switch-then-select both pass.
        assert!(apply_connection_policy(&profile, "USE Sales").is_ok());
        assert!(apply_connection_policy(&profile, "USE Sales\nSELECT 1").is_ok());
        // A mutation after the switch is still refused.
        assert!(apply_connection_policy(&profile, "USE Sales\nDELETE FROM t").is_err());
    }

    #[test]
    fn pure_use_batch_is_not_rollback_wrapped() {
        let mut profile = profile();
        profile.auto_wrap_rollback = true;
        // A lone USE must take effect for the session, so it is left raw.
        assert_eq!(
            apply_connection_policy(&profile, "USE Sales").unwrap(),
            "USE Sales"
        );
        // But a real statement is still wrapped as before.
        let sql = apply_connection_policy(&profile, "SELECT 1").unwrap();
        assert!(sql.contains("ROLLBACK TRANSACTION"));
    }

    #[test]
    fn rollback_wrap_runs_batch_leading_ddl_through_exec() {
        // CREATE/ALTER PROCEDURE must lead its batch; a bare transaction
        // preamble would make the server reject the deploy outright (Msg 111).
        // The wrap must EXEC the definition so it runs *and* rolls back.
        let mut profile = profile();
        profile.auto_wrap_rollback = true;
        for ddl in [
            "CREATE PROCEDURE dbo.P AS BEGIN SELECT 'it''s' END",
            "CREATE OR ALTER PROCEDURE dbo.P AS SELECT 1",
            "ALTER FUNCTION dbo.F() RETURNS int AS BEGIN RETURN 1 END",
            "CREATE TRIGGER trg ON t AFTER INSERT AS SELECT 1",
            "CREATE VIEW v AS SELECT 1",
        ] {
            let sql = apply_connection_policy(&profile, ddl).unwrap();
            assert!(sql.starts_with("SET XACT_ABORT ON;"), "{ddl}: {sql}");
            assert!(sql.contains("EXEC(N'"), "{ddl}: {sql}");
            assert!(sql.ends_with("ROLLBACK TRANSACTION;"), "{ddl}: {sql}");
        }
        // Embedded quotes are doubled so the EXEC literal stays intact.
        let sql =
            apply_connection_policy(&profile, "CREATE PROCEDURE dbo.P AS SELECT 'it''s'").unwrap();
        assert!(sql.contains("SELECT ''it''''s''"), "{sql}");
        // Ordinary statements keep the plain wrap.
        let sql = apply_connection_policy(&profile, "UPDATE t SET x = 1").unwrap();
        assert!(!sql.contains("EXEC(N'"), "{sql}");
        // DROP has no first-in-batch restriction: plain wrap.
        let sql = apply_connection_policy(&profile, "DROP PROCEDURE dbo.P").unwrap();
        assert!(!sql.contains("EXEC(N'"), "{sql}");
    }

    #[test]
    fn policy_reports_batch_lead_violation_clearly() {
        // The clear reason replaces a raw server Msg 111, and names the bypass
        // only when the connection's rollback wrap is active.
        let mut profile = profile();
        let sql = "SET QUOTED_IDENTIFIER ON;\nCREATE PROCEDURE dbo.P AS SELECT 1";

        let err = apply_connection_policy(&profile, sql).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("CREATE PROCEDURE"), "{text}");
        assert!(text.contains("first statement in its batch"), "{text}");
        assert!(text.contains("GO"), "{text}");
        assert!(!text.contains("Without Rollback Wrap"), "{text}");

        profile.auto_wrap_rollback = true;
        let text = apply_connection_policy(&profile, sql)
            .unwrap_err()
            .to_string();
        assert!(text.contains("Without Rollback Wrap"), "{text}");

        // A lone definition still runs (through the EXEC wrap), not blocked.
        assert!(apply_connection_policy(&profile, "CREATE PROCEDURE dbo.P AS SELECT 1").is_ok());
    }

    #[test]
    fn mysql_rollback_wrap_refuses_implicit_commit_ddl() {
        // MySQL implicitly commits DDL — inside the rollback wrap the change
        // would silently persist, so the policy must refuse it instead.
        let mut p = profile();
        p.database_type = DatabaseType::MySql;
        p.auto_wrap_rollback = true;
        for ddl in [
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "ALTER TABLE t ADD b int",
            "TRUNCATE TABLE t",
        ] {
            let err = apply_connection_policy_for(&p, ddl).unwrap_err();
            assert!(
                matches!(err, StudioError::PermissionDenied(ref m) if m.contains("implicitly commits")),
                "{ddl}: {err:?}"
            );
        }
        // Temporary tables do not implicitly commit and stay wrapped.
        let sql = apply_connection_policy_for(&p, "CREATE TEMPORARY TABLE t (a int)").unwrap();
        assert!(sql.starts_with("ROLLBACK;\nSTART TRANSACTION;"), "{sql}");
        // DML still wraps normally.
        let sql = apply_connection_policy_for(&p, "DELETE FROM t").unwrap();
        assert!(sql.ends_with("ROLLBACK;"), "{sql}");
        // Other engines are unaffected (Postgres DDL is transactional).
        p.database_type = DatabaseType::Postgres;
        let sql = apply_connection_policy_for(&p, "CREATE TABLE t (a int)").unwrap();
        assert!(sql.starts_with("BEGIN;"), "{sql}");
    }

    #[test]
    fn enforced_read_only_wraps_each_engine_and_replaces_the_rollback_wrap() {
        let mut p = profile();
        p.enforce_read_only = true;
        // The plain rollback wrap must not nest inside the read-only wrap.
        p.auto_wrap_rollback = true;

        p.database_type = DatabaseType::Postgres;
        assert_eq!(
            apply_connection_policy_for(&p, "SELECT 1").unwrap(),
            "BEGIN READ ONLY;\nSELECT 1\n;\nROLLBACK;"
        );
        p.database_type = DatabaseType::MySql;
        assert_eq!(
            apply_connection_policy_for(&p, "SELECT 1").unwrap(),
            "ROLLBACK;\nSTART TRANSACTION READ ONLY;\nSELECT 1\n;\nROLLBACK;"
        );
        p.database_type = DatabaseType::Sqlite;
        assert_eq!(
            apply_connection_policy_for(&p, "SELECT 1").unwrap(),
            "SELECT 1"
        );
        p.database_type = DatabaseType::SqlServer;
        assert_eq!(
            apply_connection_policy(&p, "SELECT 1").unwrap(),
            "SET XACT_ABORT ON;\nBEGIN TRANSACTION;\nSELECT 1\nROLLBACK TRANSACTION;"
        );
        // A lone USE still takes effect for the session.
        assert_eq!(
            apply_connection_policy(&p, "USE Sales").unwrap(),
            "USE Sales"
        );
    }

    #[test]
    fn enforced_read_only_screens_before_wrapping() {
        let mut p = profile();
        p.enforce_read_only = true;
        p.database_type = DatabaseType::SqlServer;
        for sql in [
            "SELECT * INTO dbo.copy FROM dbo.t",
            "SELECT * FROM OPENQUERY(linked, 'SELECT 1')",
            "EXEC dbo.P",
        ] {
            let err = apply_connection_policy(&p, sql).unwrap_err();
            assert!(
                matches!(err, StudioError::PermissionDenied(ref m) if m.starts_with("blocked: AI queries run read-only")),
                "{sql}: {err:?}"
            );
            assert!(check_connection_policy_for(&p, sql).is_err(), "{sql}");
        }
        p.database_type = DatabaseType::Postgres;
        let err = apply_connection_policy_for(&p, "SELECT pg_terminate_backend(1)").unwrap_err();
        assert!(matches!(err, StudioError::PermissionDenied(_)), "{err:?}");
        // The gate-only check never wraps and passes clean SQL through.
        assert!(check_connection_policy_for(&p, "SELECT 1").is_ok());
    }

    #[test]
    fn enforced_read_only_keeps_the_select_only_gate_first() {
        let mut p = profile();
        p.enforce_read_only = true;
        p.block_all_but_select = true;
        p.database_type = DatabaseType::Postgres;
        let err = apply_connection_policy_for(&p, "DELETE FROM t").unwrap_err();
        assert!(
            matches!(err, StudioError::PermissionDenied(ref m) if m.contains("SELECT statements only")),
            "{err:?}"
        );
    }

    #[test]
    fn preview_effective_sql_shows_block_reason_without_executing() {
        let mut profile = profile();
        profile.block_all_but_select = true;
        let preview = preview_effective_sql(&profile, "DELETE FROM dbo.t");
        assert!(!preview.allowed);
        assert!(preview.reason.unwrap().contains("DELETE"));
        assert_eq!(preview.sql, "DELETE FROM dbo.t");
        assert!(!preview.wrapped_in_rollback);
    }

    #[test]
    fn preview_effective_sql_shows_rollback_wrapping() {
        let mut profile = profile();
        profile.auto_wrap_rollback = true;
        let preview = preview_effective_sql(&profile, "UPDATE dbo.t SET a = 1");
        assert!(preview.allowed);
        assert!(preview.wrapped_in_rollback);
        assert!(preview.sql.contains("ROLLBACK TRANSACTION"));
    }

    #[test]
    fn mysql_rollback_wrap_sees_hidden_implicit_commit_ddl() {
        // The implicit-commit scan must read the same exposed text as the
        // SELECT-only gate: a DROP hidden in an executable comment would
        // otherwise ride into the rollback wrap and silently persist.
        let mut p = profile();
        p.database_type = DatabaseType::MySql;
        p.auto_wrap_rollback = true;
        let err = apply_connection_policy_for(&p, "/*!50003 DROP TABLE t */ SELECT 1").unwrap_err();
        assert!(
            matches!(err, StudioError::PermissionDenied(ref m) if m.contains("implicitly commits")),
            "{err:?}"
        );
        // An ordinary comment before the SELECT still wraps normally.
        let sql = apply_connection_policy_for(&p, "/* note */ SELECT 1").unwrap();
        assert!(sql.starts_with("ROLLBACK;\nSTART TRANSACTION;"), "{sql}");
    }
}
