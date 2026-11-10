//! Cheap SQL-text classification gates used by the execution engines to decide
//! whether to pay for a post-run confirmation round-trip (`DB_NAME()` on SQL
//! Server, `DATABASE()` on MySQL, `@@TRANCOUNT`). These inspect the *text*
//! only; the server's answer stays authoritative. Shared rather than per-engine
//! because the scan is a dialect-parameterised lexer pass, not driver logic
//! (ADR-016).

use sqlly_lang::sqllex::{IdentQuote, StrictLexer, TokenKind};
use sqlly_lang::sqlmodel::Dialect;

/// Whether the SQL could open an explicit transaction that a batch error might
/// leave dangling. A conservative substring test on the lowered text — matching
/// a transaction mentioned only in a string/comment costs one extra
/// `@@TRANCOUNT` round-trip and nothing else.
pub(crate) fn may_open_transaction(sql: &str) -> bool {
    let lower = sql.to_lowercase();
    // "begin tran" also matches "begin transaction"; "save tran" covers savepoints.
    lower.contains("begin tran") || lower.contains("save tran")
}

/// True if the batch text contains a `USE` keyword token (ignoring strings,
/// comments, and quoted identifiers) — an ordinary `USE database`. The dialect
/// decides what counts as a quoted identifier, so a MySQL table named
/// `` `use` `` is an identifier there just as `[use]` is on SQL Server.
fn mentions_use_keyword(sql: &str, dialect: Dialect) -> bool {
    StrictLexer::tokenize(sql, dialect).iter().any(|t| {
        matches!(
            t.kind,
            TokenKind::Keyword | TokenKind::Ident(IdentQuote::None)
        ) && t.text.eq_ignore_ascii_case("use")
    })
}

/// Whether this batch might change the connection's database, gating the
/// post-run `DB_NAME()` confirmation.
///
/// Only a top-level `USE database` changes the *session's* database context. A
/// `USE` inside dynamic SQL — `EXEC(N'USE …')` or `sp_executesql N'USE …'` — is
/// scoped to that dynamic batch and is reverted when it returns, leaving the
/// calling session on its original database (verified against SQL Server; see
/// the live test). So the lexer scan for a real `USE` keyword is both necessary
/// and sufficient: dynamic-exec USE deliberately does NOT trigger a confirmation,
/// because there is nothing to confirm.
pub(crate) fn may_change_database(sql: &str, dialect: Dialect) -> bool {
    mentions_use_keyword(sql, dialect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn may_change_database_detects_top_level_use_only() {
        // A real top-level USE changes the session and must be confirmed.
        assert!(may_change_database("USE tempdb", Dialect::SqlServer));
        assert!(may_change_database(
            "use  Sales\nGO\nSELECT 1",
            Dialect::SqlServer
        ));
        // Dynamic-exec USE is scoped to the dynamic batch and does NOT change
        // the session, so it must NOT trigger a (pointless) confirmation.
        assert!(!may_change_database(
            "EXEC(N'USE tempdb')",
            Dialect::SqlServer
        ));
        assert!(!may_change_database(
            "EXEC sp_executesql N'USE tempdb'",
            Dialect::SqlServer
        ));
        // `USE` in a string or a table named `users` is not a USE statement.
        assert!(!may_change_database(
            "SELECT 'please use the index'",
            Dialect::SqlServer
        ));
        assert!(!may_change_database(
            "SELECT * FROM users",
            Dialect::SqlServer
        ));
        assert!(!may_change_database("SELECT DB_NAME()", Dialect::SqlServer));
    }

    #[test]
    fn use_in_comments_and_bracketed_identifiers_is_ignored() {
        // The lexer strips comments before the keyword scan.
        assert!(!may_change_database(
            "SELECT 1 -- use tempdb",
            Dialect::SqlServer
        ));
        assert!(!may_change_database(
            "SELECT 1 /* use tempdb */",
            Dialect::SqlServer
        ));
        // A bracketed identifier named `use` is an identifier, not the keyword.
        assert!(!may_change_database(
            "SELECT [use] FROM t",
            Dialect::SqlServer
        ));
    }

    #[test]
    fn may_open_transaction_flags_begin_and_save() {
        assert!(may_open_transaction(
            "BEGIN TRANSACTION; UPDATE t SET x = 1"
        ));
        assert!(may_open_transaction("begin tran"));
        assert!(may_open_transaction("SAVE TRANSACTION sp1"));
        // A plain read never opens a transaction; the gate stays cheap.
        assert!(!may_open_transaction("SELECT 1"));
        assert!(!may_open_transaction("UPDATE t SET x = 1 WHERE id = 1"));
    }

    #[test]
    fn may_open_transaction_is_case_insensitive() {
        assert!(may_open_transaction("Begin Transaction"));
        assert!(may_open_transaction("save Tran sp1"));
    }

    /// MySQL reaches this gate too, and it quotes identifiers with backticks
    /// where SQL Server uses brackets. Scanning MySQL text under the SQL Server
    /// dialect would read `` `use` `` as the keyword and pay for a pointless
    /// confirmation round-trip on every such batch.
    #[test]
    fn use_detection_follows_the_dialect() {
        // A real USE is a real USE on either engine.
        assert!(may_change_database("USE ventas_prod", Dialect::MySql));
        assert!(may_change_database(
            "use ventas_prod;\nSELECT 1",
            Dialect::MySql
        ));

        // Backtick-quoted `use` is an identifier in MySQL, not the keyword.
        assert!(!may_change_database("SELECT * FROM `use`", Dialect::MySql));
        assert!(!may_change_database("SELECT `use` FROM t", Dialect::MySql));

        // The existing SQL Server behaviour is unchanged.
        assert!(may_change_database("USE tempdb", Dialect::SqlServer));
        assert!(!may_change_database(
            "SELECT * FROM [use]",
            Dialect::SqlServer
        ));

        // Neither dialect is fooled by strings or comments.
        assert!(!may_change_database("SELECT 'use ventas'", Dialect::MySql));
        assert!(!may_change_database(
            "SELECT 1 -- use ventas",
            Dialect::MySql
        ));
    }
}
