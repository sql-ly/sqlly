//! What a `SET` statement can set, per engine.
//!
//! A statement-initial `SET` is a session-settings statement, and every engine
//! spells its settings differently: T-SQL has a closed list of documented
//! options (`NOCOUNT`, `IDENTITY_INSERT`, `XACT_ABORT`, …), PostgreSQL sets
//! run-time configuration parameters, and MySQL sets system variables. The
//! lists below are the common, documented ones — enough that the option you
//! want is in the popup, not so many that the popup becomes a manual.
//!
//! SQLite has no `SET` statement at all (it uses `PRAGMA`) and Oracle's `SET`
//! is a SQL*Plus command (the server form is `ALTER SESSION SET`), so each
//! engine also names the statement *head* it wants — see [`native_set_head`]
//! — and a caret after the wrong head is steered to the right one (feedback
//! ifb_0fbc).

use sqlly_lang::sqlmodel::Dialect;

/// How a session-settings statement is spelled: the head keyword(s) the
/// settable name follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetHead {
    /// `SET name …` — T-SQL, PostgreSQL, MySQL/MariaDB (and Oracle's few
    /// server-side `SET` statements: `SET TRANSACTION`, `SET ROLE`, …).
    Set,
    /// `ALTER SESSION SET parameter = value` — Oracle.
    AlterSessionSet,
    /// `PRAGMA name = value` — SQLite.
    Pragma,
}

impl SetHead {
    /// The head as typed.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            SetHead::Set => "SET",
            SetHead::AlterSessionSet => "ALTER SESSION SET",
            SetHead::Pragma => "PRAGMA",
        }
    }

    /// The detail marker the UI groups the names after this head by.
    pub(crate) fn detail(self) -> &'static str {
        match self {
            SetHead::Set => "set option",
            SetHead::AlterSessionSet => "session parameter",
            SetHead::Pragma => "pragma",
        }
    }
}

/// The head `dialect`'s session-settings statement uses.
pub(crate) fn native_set_head(dialect: Dialect) -> SetHead {
    match dialect {
        Dialect::SqlServer | Dialect::Postgres | Dialect::MySql => SetHead::Set,
        Dialect::Oracle => SetHead::AlterSessionSet,
        Dialect::Sqlite => SetHead::Pragma,
    }
}

/// Why [`native_set_head`] is the form to use on `dialect` — the detail of
/// the item that steers a caret after another head to it.
pub(crate) fn set_head_note(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::SqlServer => "SQL Server sets session options with SET option ON | OFF",
        Dialect::Postgres => "PostgreSQL sets run-time parameters with SET parameter TO value",
        Dialect::MySql => {
            "MySQL sets system variables with SET [SESSION | GLOBAL] variable = value"
        }
        Dialect::Oracle => {
            "Oracle's SET is a SQL*Plus command; the server sets session parameters with ALTER SESSION SET parameter = value"
        }
        Dialect::Sqlite => "SQLite has no SET statement; it sets options with PRAGMA name = value",
    }
}

/// The names `head` can be followed by on `dialect`: the `SET` options, the
/// `ALTER SESSION SET` parameters (Oracle), or the pragmas (SQLite). Empty
/// when `head` is not a form `dialect` has at all.
pub(crate) fn settable_names(dialect: Dialect, head: SetHead) -> &'static [&'static str] {
    match head {
        SetHead::Set => set_options(dialect),
        SetHead::AlterSessionSet if dialect == Dialect::Oracle => ORACLE_SESSION_PARAMETERS,
        SetHead::Pragma if dialect == Dialect::Sqlite => SQLITE_PRAGMAS,
        SetHead::AlterSessionSet | SetHead::Pragma => &[],
    }
}

/// Oracle: the session parameters set by hand with `ALTER SESSION SET`.
const ORACLE_SESSION_PARAMETERS: &[&str] = &[
    "CONTAINER",
    "CURRENT_SCHEMA",
    "CURSOR_SHARING",
    "DDL_LOCK_TIMEOUT",
    "EDITION",
    "ISOLATION_LEVEL",
    "MAX_DUMP_FILE_SIZE",
    "NLS_CALENDAR",
    "NLS_COMP",
    "NLS_CURRENCY",
    "NLS_DATE_FORMAT",
    "NLS_DATE_LANGUAGE",
    "NLS_LANGUAGE",
    "NLS_LENGTH_SEMANTICS",
    "NLS_NUMERIC_CHARACTERS",
    "NLS_SORT",
    "NLS_TERRITORY",
    "NLS_TIMESTAMP_FORMAT",
    "NLS_TIMESTAMP_TZ_FORMAT",
    "OPTIMIZER_FEATURES_ENABLE",
    "OPTIMIZER_INDEX_COST_ADJ",
    "OPTIMIZER_MODE",
    "PARALLEL_DEGREE_POLICY",
    "PLSQL_OPTIMIZE_LEVEL",
    "PLSQL_WARNINGS",
    "QUERY_REWRITE_ENABLED",
    "RESULT_CACHE_MODE",
    "SQL_TRACE",
    "STATISTICS_LEVEL",
    "TIME_ZONE",
    "TIMED_STATISTICS",
    "TRACEFILE_IDENTIFIER",
    "WORKAREA_SIZE_POLICY",
];

/// SQLite: the pragmas set (or commonly queried) by hand.
const SQLITE_PRAGMAS: &[&str] = &[
    "application_id",
    "auto_vacuum",
    "automatic_index",
    "busy_timeout",
    "cache_size",
    "cache_spill",
    "case_sensitive_like",
    "cell_size_check",
    "checkpoint_fullfsync",
    "compile_options",
    "database_list",
    "defer_foreign_keys",
    "encoding",
    "foreign_key_check",
    "foreign_key_list",
    "foreign_keys",
    "fullfsync",
    "ignore_check_constraints",
    "index_info",
    "index_list",
    "integrity_check",
    "journal_mode",
    "journal_size_limit",
    "locking_mode",
    "max_page_count",
    "mmap_size",
    "optimize",
    "page_size",
    "query_only",
    "quick_check",
    "read_uncommitted",
    "recursive_triggers",
    "reverse_unordered_selects",
    "secure_delete",
    "synchronous",
    "table_info",
    "table_list",
    "temp_store",
    "threads",
    "trusted_schema",
    "user_version",
    "wal_autocheckpoint",
    "wal_checkpoint",
    "writable_schema",
];

/// The documented `SET` options for T-SQL, grouped as the SQL Server reference
/// groups them (date/time, locking, misc, query execution, ISO, statistics,
/// transactions) and then flattened alphabetically.
const SQL_SERVER: &[&str] = &[
    "ANSI_DEFAULTS",
    "ANSI_NULL_DFLT_OFF",
    "ANSI_NULL_DFLT_ON",
    "ANSI_NULLS",
    "ANSI_PADDING",
    "ANSI_WARNINGS",
    "ARITHABORT",
    "ARITHIGNORE",
    "CONCAT_NULL_YIELDS_NULL",
    "CONTEXT_INFO",
    "CURSOR_CLOSE_ON_COMMIT",
    "DATEFIRST",
    "DATEFORMAT",
    "DEADLOCK_PRIORITY",
    "FIPS_FLAGGER",
    "FMTONLY",
    "FORCEPLAN",
    "IDENTITY_INSERT",
    "IMPLICIT_TRANSACTIONS",
    "LANGUAGE",
    "LOCK_TIMEOUT",
    "NOCOUNT",
    "NOEXEC",
    "NUMERIC_ROUNDABORT",
    "OFFSETS",
    "PARSEONLY",
    "QUERY_GOVERNOR_COST_LIMIT",
    "QUOTED_IDENTIFIER",
    "REMOTE_PROC_TRANSACTIONS",
    "ROWCOUNT",
    "SHOWPLAN_ALL",
    "SHOWPLAN_TEXT",
    "SHOWPLAN_XML",
    "STATISTICS IO",
    "STATISTICS PROFILE",
    "STATISTICS TIME",
    "STATISTICS XML",
    "TEXTSIZE",
    "TRANSACTION ISOLATION LEVEL",
    "XACT_ABORT",
];

/// PostgreSQL: the `SET` forms plus the run-time configuration parameters that
/// actually get set by hand in a session.
const POSTGRES: &[&str] = &[
    "application_name",
    "bytea_output",
    "client_encoding",
    "client_min_messages",
    "CONSTRAINTS",
    "datestyle",
    "default_transaction_deferrable",
    "default_transaction_isolation",
    "default_transaction_read_only",
    "enable_seqscan",
    "extra_float_digits",
    "idle_in_transaction_session_timeout",
    "intervalstyle",
    "LOCAL",
    "lock_timeout",
    "maintenance_work_mem",
    "ROLE",
    "search_path",
    "SESSION",
    "SESSION AUTHORIZATION",
    "standard_conforming_strings",
    "statement_timeout",
    "TIME ZONE",
    "TRANSACTION",
    "work_mem",
];

/// MySQL and MariaDB: the `SET` forms plus the system variables commonly set
/// per session.
const MYSQL: &[&str] = &[
    "autocommit",
    "CHARACTER SET",
    "character_set_client",
    "character_set_connection",
    "character_set_results",
    "collation_connection",
    "foreign_key_checks",
    "GLOBAL",
    "group_concat_max_len",
    "innodb_lock_wait_timeout",
    "NAMES",
    "PASSWORD",
    "profiling",
    "SESSION",
    "sql_mode",
    "sql_safe_updates",
    "sql_select_limit",
    "time_zone",
    "TRANSACTION",
    "transaction_isolation",
    "unique_checks",
    "wait_timeout",
];

/// Oracle's `SET` is a SQL*Plus command rather than a server statement, and the
/// session equivalent is `ALTER SESSION SET`. Only the handful that a parser in
/// this family will see are offered.
const ORACLE: &[&str] = &["CONSTRAINT", "CONSTRAINTS", "ROLE", "TRANSACTION"];

/// The options a statement-initial `SET` can name on `dialect`. Empty means the
/// engine has no `SET` statement worth completing (SQLite).
pub(crate) fn set_options(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::SqlServer => SQL_SERVER,
        Dialect::Postgres => POSTGRES,
        Dialect::MySql => MYSQL,
        Dialect::Oracle => ORACLE,
        Dialect::Sqlite => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dialect_but_sqlite_has_options() {
        for dialect in Dialect::ALL {
            let got = set_options(dialect);
            if dialect == Dialect::Sqlite {
                assert!(got.is_empty(), "SQLite has no SET statement");
            } else {
                assert!(!got.is_empty(), "{dialect:?} has no SET options");
            }
        }
    }

    #[test]
    fn the_reported_option_is_offered_on_sql_server() {
        assert!(set_options(Dialect::SqlServer).contains(&"IDENTITY_INSERT"));
    }

    #[test]
    fn lists_are_sorted_and_free_of_duplicates() {
        for dialect in Dialect::ALL {
            for head in [SetHead::Set, SetHead::AlterSessionSet, SetHead::Pragma] {
                let got = settable_names(dialect, head);
                let mut sorted: Vec<String> = got.iter().map(|o| o.to_ascii_lowercase()).collect();
                sorted.sort();
                sorted.dedup();
                assert_eq!(
                    sorted.len(),
                    got.len(),
                    "{dialect:?} {head:?} names contain a duplicate"
                );
                let as_is: Vec<String> = got.iter().map(|o| o.to_ascii_lowercase()).collect();
                assert_eq!(as_is, sorted, "{dialect:?} {head:?} names are not sorted");
            }
        }
    }

    #[test]
    fn each_engine_names_its_own_form_and_only_that_form_has_names() {
        assert_eq!(native_set_head(Dialect::SqlServer), SetHead::Set);
        assert_eq!(native_set_head(Dialect::Oracle), SetHead::AlterSessionSet);
        assert_eq!(native_set_head(Dialect::Sqlite), SetHead::Pragma);
        assert!(
            settable_names(Dialect::Oracle, SetHead::AlterSessionSet).contains(&"NLS_DATE_FORMAT")
        );
        assert!(settable_names(Dialect::Sqlite, SetHead::Pragma).contains(&"journal_mode"));
        assert!(settable_names(Dialect::SqlServer, SetHead::AlterSessionSet).is_empty());
        assert!(settable_names(Dialect::SqlServer, SetHead::Pragma).is_empty());
        assert!(settable_names(Dialect::Sqlite, SetHead::Set).is_empty());
    }
}
