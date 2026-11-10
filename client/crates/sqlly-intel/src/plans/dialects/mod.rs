//! Dialect-dispatched execution-plan capture and parsing.
//!
//! SQL Server's ShowPlan XML path is untouched; this module adds the other
//! native engines, mapping every format into the SAME [`ExecutionPlan`] model
//! so the plan pane, findings, and search work identically:
//!
//! - PostgreSQL (`pg`): `EXPLAIN (ANALYZE, FORMAT JSON)` — a JSON array
//!   with a nested `Plan` tree. Costs are cumulative subtree costs like
//!   ShowPlan's.
//! - MySQL 8 (`mysql`): actual plans via `EXPLAIN ANALYZE` (TREE text, the
//!   only format that carries runtime statistics on MySQL); estimated plans
//!   via `EXPLAIN FORMAT=JSON` — a `query_block` tree whose operator
//!   wrappers nest `table` objects; costs live in `cost_info` as strings.
//! - MariaDB (`mysql`): actual plans via `ANALYZE FORMAT=JSON` (the MySQL
//!   JSON shape plus `r_rows` / `r_loops` / `r_total_time_ms` runtime fields
//!   per table); estimated plans via `EXPLAIN FORMAT=JSON`. Costs are absent
//!   on some MariaDB versions.
//! - SQLite (`sqlite`): `EXPLAIN QUERY PLAN` — `(id, parent, detail)` rows
//!   forming a forest; this parser accepts them as `id|parent|detail` lines.
//! - DuckDB (`duckdb`): `EXPLAIN (FORMAT JSON)` / `EXPLAIN ANALYZE (FORMAT
//!   JSON)` — a JSON operator tree (`name`, `children`, `extra_info`) with
//!   estimated cardinalities, plus per-operator timing and row counts on
//!   ANALYZE.
//! - ClickHouse (`clickhouse`): `EXPLAIN json = 1, indexes = 1` — a
//!   PostgreSQL-shaped JSON step tree (`Node Type`, `Plans`) whose read steps
//!   carry the index analysis (parts/granules pruned); estimated only.
//! - Oracle (`oracle`): `EXPLAIN PLAN … FOR` into `PLAN_TABLE`, read back
//!   as tab-separated rows (id, parent, operation, options, object, cost,
//!   cardinality, bytes, predicates). Estimated only: Oracle's runtime plan
//!   statistics live in `V$SQL_PLAN_STATISTICS_ALL` and are not captured.
//!
//! All parsers are recovering and typed: malformed input yields a
//! [`PlanError`](crate::plans::error::PlanError), never a panic, and unknown fields
//! are ignored. Full table scans surface as
//! [`PlanWarning::TableScan`](crate::plans::warning::PlanWarning::TableScan) so the
//! existing findings engine reports them across every dialect.

mod clickhouse;
mod duckdb;
mod mysql;
mod oracle;
mod pg;
mod sqlite;

pub use clickhouse::parse_clickhouse_explain_json;
pub use duckdb::parse_duckdb_explain_json;
pub use mysql::{parse_mysql_explain_analyze_tree, parse_mysql_explain_json};
pub use oracle::{
    actual_capture_sql as oracle_actual_capture_sql, capture_sql as oracle_capture_sql,
    parse_oracle_plan_lines, parse_oracle_xplan_text, plan_rows_sql as oracle_plan_rows_sql,
};
pub use pg::parse_pg_explain_json;
pub use sqlite::{
    parse_sqlite_query_plan_any, parse_sqlite_query_plan_rows, parse_sqlite_query_plan_text,
};

use crate::plans::error::PlanResult;
use crate::plans::model::ExecutionPlan;

/// The plan-capture family of a connection. Mirrors the engine families with
/// native drivers; the caller derives it from its connection kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDialect {
    SqlServer,
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    DuckDb,
    ClickHouse,
    Oracle,
}

/// The SQL to run to capture a plan for `sql` on `dialect`. SQL Server,
/// PostgreSQL, MySQL 8, and MariaDB capture ACTUAL execution statistics (the
/// statement runs); SQLite produces estimated plans without executing.
pub fn explain_capture_sql(dialect: PlanDialect, sql: &str) -> String {
    crate::plans::explain_sql_with_options(
        dialect,
        &crate::plans::ExplainOptions::actual_defaults(),
        true,
        sql,
    )
}

/// Estimated (no-execution) plan capture: the optimizer is asked, the
/// statement never runs. SQL Server uses batch-isolated `SET SHOWPLAN_XML`;
/// PostgreSQL drops `ANALYZE`; MySQL and MariaDB fall back to
/// `EXPLAIN FORMAT=JSON`; SQLite plan captures never execute the statement
/// in the first place, so its form is identical to [`explain_capture_sql`].
pub fn estimated_capture_sql(dialect: PlanDialect, sql: &str) -> String {
    crate::plans::explain_sql_with_options(
        dialect,
        &crate::plans::ExplainOptions::estimated_defaults(),
        false,
        sql,
    )
}

/// Whether capturing an ACTUAL plan on `dialect` (via
/// [`explain_capture_sql`]) executes the statement, as opposed to only
/// asking the optimizer. The UI words its status line with this. Estimated
/// captures ([`estimated_capture_sql`]) never execute on any dialect.
pub fn capture_executes_statement(dialect: PlanDialect) -> bool {
    matches!(
        dialect,
        PlanDialect::SqlServer
            | PlanDialect::Postgres
            | PlanDialect::MySql
            | PlanDialect::MariaDb
            | PlanDialect::DuckDb
            | PlanDialect::Oracle
    )
}

/// Parse one captured plan document for `dialect` into plans. SQL Server
/// documents may contain several statements; the other dialects produce one
/// plan per captured document.
///
/// MySQL routes by document shape: actual captures (`EXPLAIN ANALYZE`) are
/// TREE text starting with `->`, estimated captures (`EXPLAIN FORMAT=JSON`)
/// are a JSON object — so callers pass whatever the capture returned and
/// the right parser runs. MariaDB uses the JSON parser for both; actual
/// captures simply carry extra `r_*` runtime fields.
pub fn parse_captured_plan(dialect: PlanDialect, doc: &str) -> PlanResult<Vec<ExecutionPlan>> {
    match dialect {
        PlanDialect::SqlServer => crate::plans::parse_showplan_xml_multi(doc),
        PlanDialect::Postgres => parse_pg_explain_json(doc).map(|p| vec![p]),
        PlanDialect::MySql => {
            if doc.trim_start().starts_with(['{', '[']) {
                parse_mysql_explain_json(doc).map(|p| vec![p])
            } else {
                parse_mysql_explain_analyze_tree(doc).map(|p| vec![p])
            }
        }
        PlanDialect::MariaDb => parse_mysql_explain_json(doc).map(|p| vec![p]),
        PlanDialect::Sqlite => parse_sqlite_query_plan_text(doc).map(|p| vec![p]),
        PlanDialect::DuckDb => parse_duckdb_explain_json(doc).map(|p| vec![p]),
        PlanDialect::ClickHouse => parse_clickhouse_explain_json(doc).map(|p| vec![p]),
        PlanDialect::Oracle => parse_oracle_plan_lines(doc).map(|p| vec![p]),
    }
}

#[cfg(test)]
mod tests {
    use super::mysql::fixtures::{MYSQL_ANALYZE_TREE, MYSQL_FULL_SCAN};
    use super::*;

    #[test]
    fn estimated_capture_isolates_showplan_batches_on_sql_server() {
        let sql = estimated_capture_sql(PlanDialect::SqlServer, "UPDATE t SET x = 1");
        // SET SHOWPLAN_XML must be alone in its batch: setup and teardown
        // ride their own GO-separated batches around the user SQL.
        assert_eq!(
            sql,
            "SET SHOWPLAN_XML ON;\nGO\nUPDATE t SET x = 1\nGO\nSET SHOWPLAN_XML OFF;"
        );
    }

    #[test]
    fn estimated_capture_drops_analyze_on_postgres() {
        let sql = estimated_capture_sql(PlanDialect::Postgres, "SELECT 1");
        assert_eq!(sql, "EXPLAIN (FORMAT JSON) SELECT 1");
        assert!(!sql.contains("ANALYZE"));
    }

    #[test]
    fn estimated_capture_matches_actual_on_engines_that_never_execute() {
        assert_eq!(
            estimated_capture_sql(PlanDialect::Sqlite, "SELECT 1"),
            explain_capture_sql(PlanDialect::Sqlite, "SELECT 1")
        );
    }

    #[test]
    fn mysql_and_mariadb_actual_captures_execute_and_estimated_stays_json() {
        assert_eq!(
            explain_capture_sql(PlanDialect::MySql, "SELECT 1"),
            "EXPLAIN ANALYZE SELECT 1"
        );
        assert_eq!(
            explain_capture_sql(PlanDialect::MariaDb, "SELECT 1"),
            "ANALYZE FORMAT=JSON SELECT 1"
        );
        for d in [PlanDialect::MySql, PlanDialect::MariaDb] {
            assert_eq!(
                estimated_capture_sql(d, "SELECT 1"),
                "EXPLAIN FORMAT=JSON SELECT 1"
            );
            assert!(capture_executes_statement(d));
        }
    }

    #[test]
    fn capture_sql_matches_each_engine() {
        assert!(
            explain_capture_sql(PlanDialect::Postgres, "SELECT 1")
                .starts_with("EXPLAIN (ANALYZE, FORMAT JSON)")
        );
        assert!(explain_capture_sql(PlanDialect::MySql, "SELECT 1").starts_with("EXPLAIN ANALYZE"));
        assert!(
            explain_capture_sql(PlanDialect::MariaDb, "SELECT 1")
                .starts_with("ANALYZE FORMAT=JSON")
        );
        assert!(
            explain_capture_sql(PlanDialect::Sqlite, "SELECT 1").starts_with("EXPLAIN QUERY PLAN")
        );
        assert!(
            explain_capture_sql(PlanDialect::SqlServer, "SELECT 1")
                .contains("SET STATISTICS XML ON")
        );
        assert!(capture_executes_statement(PlanDialect::SqlServer));
        assert!(capture_executes_statement(PlanDialect::Postgres));
        assert!(capture_executes_statement(PlanDialect::MySql));
        assert!(capture_executes_statement(PlanDialect::MariaDb));
        assert!(!capture_executes_statement(PlanDialect::Sqlite));
        // Oracle: the estimated capture is EXPLAIN PLAN (never runs the
        // statement); the actual capture runs it with row-source statistics
        // and reads V$SQL_PLAN_STATISTICS_ALL.
        assert!(capture_executes_statement(PlanDialect::Oracle));
        let estimated = estimated_capture_sql(PlanDialect::Oracle, "SELECT 1 FROM DUAL");
        assert!(estimated.starts_with("EXPLAIN PLAN SET STATEMENT_ID"));
        let actual = explain_capture_sql(PlanDialect::Oracle, "SELECT 1 FROM DUAL");
        assert!(actual.starts_with("ALTER SESSION SET STATISTICS_LEVEL = ALL;"));
        assert!(actual.contains("v$sql_plan_statistics_all"));
        assert_ne!(actual, estimated);
    }

    #[test]
    fn parse_captured_plan_routes_oracle_plan_lines() {
        let plans =
            parse_captured_plan(PlanDialect::Oracle, super::oracle::fixtures::ORACLE_JOIN).unwrap();
        assert_eq!(plans[0].statement_type, "EXPLAIN PLAN");
        assert_eq!(plans[0].nodes.len(), 5);
    }

    #[test]
    fn parse_captured_plan_routes_mysql_by_document_shape() {
        // Actual capture: TREE text → tree parser.
        let plans = parse_captured_plan(PlanDialect::MySql, MYSQL_ANALYZE_TREE).unwrap();
        assert_eq!(plans[0].statement_type, "EXPLAIN ANALYZE");
        assert_eq!(plans[0].nodes.len(), 4);
        // Estimated capture: JSON → existing JSON parser.
        let plans = parse_captured_plan(PlanDialect::MySql, MYSQL_FULL_SCAN).unwrap();
        assert_eq!(plans[0].statement_type, "EXPLAIN");
        assert_eq!(plans[0].nodes.len(), 2);
    }
}
