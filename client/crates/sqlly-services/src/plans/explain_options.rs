//! Version-gated EXPLAIN capture options.
//!
//! PostgreSQL `EXPLAIN` flags are listed only when they differ from the
//! server's defaults so today's capture strings stay byte-identical:
//! `COSTS` defaults true (emit `COSTS OFF` when false); `TIMING` and
//! `SUMMARY` follow `ANALYZE` (emit `TIMING OFF` / `SUMMARY OFF` only when
//! ANALYZE is on and the flag is off). `FORMAT JSON` is always last.

use crate::plans::dialects::PlanDialect;

/// Major.minor of the connected server, parsed leniently from
/// `current_setting('server_version')` ("16.2 (Debian…)"), `VERSION()`
/// ("8.0.36-log", "10.11.6-MariaDB"), or `sqlite_version()`. Unknown = None.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ServerVersion {
    pub major: u32,
    pub minor: u32,
}

impl ServerVersion {
    pub fn parse(raw: &str) -> Option<Self> {
        let start = raw.find(|c: char| c.is_ascii_digit())?;
        let rest = &raw[start..];
        let mut nums = Vec::new();
        let mut current = String::new();
        for ch in rest.chars() {
            if ch.is_ascii_digit() {
                current.push(ch);
            } else if !current.is_empty() {
                nums.push(current.parse::<u32>().ok()?);
                current.clear();
                if !ch.is_ascii_punctuation() && !ch.is_whitespace() && ch != '.' && ch != '-' {
                    break;
                }
            } else if ch != '.' && ch != '-' && !ch.is_whitespace() && !ch.is_ascii_punctuation() {
                break;
            }
        }
        if !current.is_empty() {
            nums.push(current.parse::<u32>().ok()?);
        }
        let major = *nums.first()?;
        let minor = nums.get(1).copied().unwrap_or(0);
        Some(Self { major, minor })
    }
}

const PG12: ServerVersion = ServerVersion {
    major: 12,
    minor: 0,
};
const PG13: ServerVersion = ServerVersion {
    major: 13,
    minor: 0,
};
const PG16: ServerVersion = ServerVersion {
    major: 16,
    minor: 0,
};
const PG17: ServerVersion = ServerVersion {
    major: 17,
    minor: 0,
};

fn effective_pg_version(version: Option<ServerVersion>) -> ServerVersion {
    version.unwrap_or(PG12)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PgExplainOptions {
    pub analyze: bool,
    pub verbose: bool,
    pub costs: bool,
    pub buffers: bool,
    pub timing: bool,
    pub summary: bool,
    pub settings: bool,
    pub wal: bool,
    pub generic_plan: bool,
    pub memory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct MySqlExplainOptions {
    pub analyze: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ExplainOptions {
    pub pg: PgExplainOptions,
    pub mysql: MySqlExplainOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainPreset {
    Actual,
    Estimated,
    Profile,
}

impl ExplainOptions {
    /// Today's fixed strings, expressed as options: PG analyze=true, costs=true, timing=true, summary=true (PG defaults when ANALYZE is on); MySQL analyze=true.
    pub fn actual_defaults() -> Self {
        Self {
            pg: PgExplainOptions {
                analyze: true,
                costs: true,
                timing: true,
                summary: true,
                ..PgExplainOptions::default()
            },
            mysql: MySqlExplainOptions { analyze: true },
        }
    }

    pub fn estimated_defaults() -> Self {
        Self {
            pg: PgExplainOptions {
                costs: true,
                ..PgExplainOptions::default()
            },
            mysql: MySqlExplainOptions { analyze: false },
        }
    }

    /// Profile: actual + buffers + verbose (+ wal/settings when the server allows them).
    pub fn profile_defaults(version: Option<ServerVersion>, dialect: PlanDialect) -> Self {
        let mut opts = Self::actual_defaults();
        opts.pg.buffers = true;
        opts.pg.verbose = true;
        opts.pg.settings = true;
        opts.pg.wal = true;
        opts.clamped(dialect, version)
    }

    /// Drop flags the server version cannot accept: PG SETTINGS < 12, WAL < 13, GENERIC_PLAN < 16, MEMORY < 17; `generic_plan` also forces `analyze=false` (PG rejects the pair). None version = keep only flags valid on PG 12 (the oldest supported).
    pub fn clamped(mut self, dialect: PlanDialect, version: Option<ServerVersion>) -> Self {
        if dialect != PlanDialect::Postgres {
            return self;
        }
        let v = effective_pg_version(version);
        if v < PG12 {
            self.pg.settings = false;
        }
        if v < PG13 {
            self.pg.wal = false;
        }
        if v < PG16 {
            self.pg.generic_plan = false;
        }
        if v < PG17 {
            self.pg.memory = false;
        }
        if self.pg.generic_plan {
            self.pg.analyze = false;
        }
        self
    }

    /// Which PG flags are selectable for `version`, for the popover to disable rows with a reason.
    pub fn pg_flag_availability(
        version: Option<ServerVersion>,
    ) -> Vec<(&'static str, bool, &'static str)> {
        let v = effective_pg_version(version);
        vec![
            ("analyze", true, ""),
            ("verbose", true, ""),
            ("costs", true, ""),
            ("buffers", true, ""),
            ("timing", true, ""),
            ("summary", true, ""),
            ("settings", v >= PG12, "Requires PostgreSQL 12+"),
            ("wal", v >= PG13, "Requires PostgreSQL 13+"),
            ("generic_plan", v >= PG16, "Requires PostgreSQL 16+"),
            ("memory", v >= PG17, "Requires PostgreSQL 17+"),
        ]
    }

    /// Does this capture execute the statement? PG: pg.analyze; MySQL: mysql.analyze; MariaDB: analyze (ANALYZE FORMAT=JSON); SQL Server: actual (kind); SQLite: never.
    pub fn executes_statement(&self, dialect: PlanDialect, kind_is_actual: bool) -> bool {
        match dialect {
            PlanDialect::Postgres => self.pg.analyze,
            PlanDialect::MySql | PlanDialect::MariaDb => self.mysql.analyze,
            PlanDialect::SqlServer => kind_is_actual,
            PlanDialect::Sqlite => false,
            // `EXPLAIN ANALYZE` runs the statement; plain `EXPLAIN` does not.
            PlanDialect::DuckDb => kind_is_actual,
            // ClickHouse has no EXPLAIN ANALYZE: every capture is estimated.
            PlanDialect::ClickHouse => false,
            // Estimated: EXPLAIN PLAN only asks the optimizer. Actual: the
            // statement runs (with row-source statistics) and the cursor's
            // plan is read back from V$SQL_PLAN_STATISTICS_ALL.
            PlanDialect::Oracle => kind_is_actual,
        }
    }
}

/// Build the capture SQL. PG: `EXPLAIN (FLAG [, FLAG]…, FORMAT JSON) {sql}` with only true flags listed (`ANALYZE, VERBOSE, COSTS, BUFFERS, TIMING, SUMMARY, SETTINGS, WAL, GENERIC_PLAN, MEMORY` in that order; `FORMAT JSON` always last). MySQL: analyze → `EXPLAIN ANALYZE {sql}`, else `EXPLAIN FORMAT=JSON {sql}`. MariaDB: analyze → `ANALYZE FORMAT=JSON {sql}`, else `EXPLAIN FORMAT=JSON {sql}`. SQL Server: `kind_is_actual` → `crate::plans::include_actual_plan_sql`, else `crate::plans::estimated_plan_sql`. SQLite: `EXPLAIN QUERY PLAN {sql}`.
///
/// PostgreSQL emits `COSTS`/`TIMING`/`SUMMARY` only when they differ from the
/// server default (`COSTS` default true → `COSTS OFF` when false; `TIMING` and
/// `SUMMARY` follow ANALYZE → `TIMING OFF`/`SUMMARY OFF` only when analyze is
/// on and the flag is off). Actual defaults therefore serialize to
/// `EXPLAIN (ANALYZE, FORMAT JSON)`.
pub fn explain_sql_with_options(
    dialect: PlanDialect,
    opts: &ExplainOptions,
    kind_is_actual: bool,
    sql: &str,
) -> String {
    match dialect {
        PlanDialect::SqlServer => {
            if kind_is_actual {
                crate::plans::include_actual_plan_sql(sql)
            } else {
                crate::plans::estimated_plan_sql(sql)
            }
        }
        PlanDialect::Postgres => pg_explain_sql(&opts.pg, sql),
        PlanDialect::MySql => {
            if opts.mysql.analyze {
                format!("EXPLAIN ANALYZE {sql}")
            } else {
                format!("EXPLAIN FORMAT=JSON {sql}")
            }
        }
        PlanDialect::MariaDb => {
            if opts.mysql.analyze {
                format!("ANALYZE FORMAT=JSON {sql}")
            } else {
                format!("EXPLAIN FORMAT=JSON {sql}")
            }
        }
        PlanDialect::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
        // DuckDB spells the option list PostgreSQL-style: `EXPLAIN (ANALYZE,
        // FORMAT JSON)`; `EXPLAIN ANALYZE (FORMAT JSON)` is a syntax error.
        PlanDialect::DuckDb => {
            if kind_is_actual {
                format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}")
            } else {
                format!("EXPLAIN (FORMAT JSON) {sql}")
            }
        }
        // One JSON document with the index analysis on every read step;
        // the same for both kinds since nothing executes.
        PlanDialect::ClickHouse => format!("EXPLAIN json = 1, indexes = 1 {sql}"),
        // Estimated: EXPLAIN PLAN into PLAN_TABLE, read back as lines.
        // Actual: run the statement with STATISTICS_LEVEL = ALL and read
        // the cursor's row-source statistics back in the same line format.
        PlanDialect::Oracle => {
            if kind_is_actual {
                super::dialects::oracle_actual_capture_sql(sql)
            } else {
                super::dialects::oracle_capture_sql(sql)
            }
        }
    }
}

fn pg_explain_sql(pg: &PgExplainOptions, sql: &str) -> String {
    let mut flags: Vec<String> = Vec::new();
    if pg.analyze {
        flags.push("ANALYZE".to_string());
    }
    if pg.verbose {
        flags.push("VERBOSE".to_string());
    }
    if !pg.costs {
        flags.push("COSTS OFF".to_string());
    }
    if pg.buffers {
        flags.push("BUFFERS".to_string());
    }
    if pg.analyze && !pg.timing {
        flags.push("TIMING OFF".to_string());
    } else if !pg.analyze && pg.timing {
        flags.push("TIMING".to_string());
    }
    if pg.analyze && !pg.summary {
        flags.push("SUMMARY OFF".to_string());
    } else if !pg.analyze && pg.summary {
        flags.push("SUMMARY".to_string());
    }
    if pg.settings {
        flags.push("SETTINGS".to_string());
    }
    if pg.wal {
        flags.push("WAL".to_string());
    }
    if pg.generic_plan {
        flags.push("GENERIC_PLAN".to_string());
    }
    if pg.memory {
        flags.push("MEMORY".to_string());
    }
    flags.push("FORMAT JSON".to_string());
    format!("EXPLAIN ({}) {sql}", flags.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_pg_debian() {
        assert_eq!(
            ServerVersion::parse("16.2 (Debian 16.2-1.pgdg120+1)"),
            Some(ServerVersion {
                major: 16,
                minor: 2
            })
        );
        assert_eq!(
            ServerVersion::parse("14.11"),
            Some(ServerVersion {
                major: 14,
                minor: 11
            })
        );
    }

    #[test]
    fn version_parse_mysql_and_mariadb() {
        assert_eq!(
            ServerVersion::parse("8.0.36-log"),
            Some(ServerVersion { major: 8, minor: 0 })
        );
        assert_eq!(
            ServerVersion::parse("10.11.6-MariaDB"),
            Some(ServerVersion {
                major: 10,
                minor: 11
            })
        );
        assert_eq!(
            ServerVersion::parse("3.45.1"),
            Some(ServerVersion {
                major: 3,
                minor: 45
            })
        );
        assert_eq!(ServerVersion::parse("not a version"), None);
    }

    #[test]
    fn actual_defaults_match_legacy_strings() {
        let sql = "SELECT 1";
        let actual = ExplainOptions::actual_defaults();
        let estimated = ExplainOptions::estimated_defaults();
        assert_eq!(
            explain_sql_with_options(PlanDialect::SqlServer, &actual, true, sql),
            "SET STATISTICS XML ON;\nSELECT 1\nSET STATISTICS XML OFF;"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::SqlServer, &estimated, false, sql),
            "SET SHOWPLAN_XML ON;\nGO\nSELECT 1\nGO\nSET SHOWPLAN_XML OFF;"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::Postgres, &actual, true, sql),
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::Postgres, &estimated, false, sql),
            "EXPLAIN (FORMAT JSON) SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::MySql, &actual, true, sql),
            "EXPLAIN ANALYZE SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::MySql, &estimated, false, sql),
            "EXPLAIN FORMAT=JSON SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::MariaDb, &actual, true, sql),
            "ANALYZE FORMAT=JSON SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::MariaDb, &estimated, false, sql),
            "EXPLAIN FORMAT=JSON SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::Sqlite, &actual, true, sql),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::Sqlite, &estimated, false, sql),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::ClickHouse, &actual, true, sql),
            "EXPLAIN json = 1, indexes = 1 SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::ClickHouse, &estimated, false, sql),
            "EXPLAIN json = 1, indexes = 1 SELECT 1"
        );
        assert_eq!(
            crate::plans::explain_capture_sql(PlanDialect::Postgres, sql),
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1"
        );
        assert_eq!(
            crate::plans::estimated_capture_sql(PlanDialect::Postgres, sql),
            "EXPLAIN (FORMAT JSON) SELECT 1"
        );
    }

    #[test]
    fn pg_flags_ordered_and_format_last() {
        let mut opts = ExplainOptions::actual_defaults();
        opts.pg.verbose = true;
        opts.pg.buffers = true;
        opts.pg.settings = true;
        opts.pg.wal = true;
        opts.pg.memory = true;
        let sql = explain_sql_with_options(PlanDialect::Postgres, &opts, true, "SELECT 1");
        assert_eq!(
            sql,
            "EXPLAIN (ANALYZE, VERBOSE, BUFFERS, SETTINGS, WAL, MEMORY, FORMAT JSON) SELECT 1"
        );
        assert!(sql.ends_with("FORMAT JSON) SELECT 1"));
        assert!(sql.find("ANALYZE").unwrap() < sql.find("VERBOSE").unwrap());
        assert!(sql.find("VERBOSE").unwrap() < sql.find("BUFFERS").unwrap());
        assert!(sql.find("FORMAT JSON").unwrap() > sql.find("MEMORY").unwrap());
    }

    #[test]
    fn clamp_drops_wal_on_pg12() {
        let mut opts = ExplainOptions::actual_defaults();
        opts.pg.wal = true;
        opts.pg.settings = true;
        opts.pg.memory = true;
        opts.pg.generic_plan = true;
        let clamped = opts.clamped(
            PlanDialect::Postgres,
            Some(ServerVersion {
                major: 12,
                minor: 0,
            }),
        );
        assert!(!clamped.pg.wal);
        assert!(clamped.pg.settings);
        assert!(!clamped.pg.memory);
        assert!(!clamped.pg.generic_plan);
        let none_clamped = opts.clamped(PlanDialect::Postgres, None);
        assert!(!none_clamped.pg.wal);
        assert!(none_clamped.pg.settings);
    }

    #[test]
    fn clamp_generic_plan_disables_analyze() {
        let mut opts = ExplainOptions::actual_defaults();
        opts.pg.generic_plan = true;
        let clamped = opts.clamped(
            PlanDialect::Postgres,
            Some(ServerVersion {
                major: 16,
                minor: 0,
            }),
        );
        assert!(clamped.pg.generic_plan);
        assert!(!clamped.pg.analyze);
    }

    #[test]
    fn profile_on_pg16_has_buffers_wal_settings_not_memory() {
        let opts = ExplainOptions::profile_defaults(
            Some(ServerVersion {
                major: 16,
                minor: 2,
            }),
            PlanDialect::Postgres,
        );
        assert!(opts.pg.analyze);
        assert!(opts.pg.buffers);
        assert!(opts.pg.verbose);
        assert!(opts.pg.wal);
        assert!(opts.pg.settings);
        assert!(!opts.pg.memory);
        assert!(!opts.pg.generic_plan);
    }

    #[test]
    fn mariadb_analyze_vs_explain() {
        let actual = ExplainOptions::actual_defaults();
        let estimated = ExplainOptions::estimated_defaults();
        assert_eq!(
            explain_sql_with_options(PlanDialect::MariaDb, &actual, true, "SELECT 1"),
            "ANALYZE FORMAT=JSON SELECT 1"
        );
        assert_eq!(
            explain_sql_with_options(PlanDialect::MariaDb, &estimated, false, "SELECT 1"),
            "EXPLAIN FORMAT=JSON SELECT 1"
        );
    }

    #[test]
    fn executes_statement_matrix() {
        let actual = ExplainOptions::actual_defaults();
        let estimated = ExplainOptions::estimated_defaults();
        assert!(actual.executes_statement(PlanDialect::Postgres, true));
        assert!(!estimated.executes_statement(PlanDialect::Postgres, true));
        assert!(actual.executes_statement(PlanDialect::MySql, true));
        assert!(!estimated.executes_statement(PlanDialect::MySql, true));
        assert!(actual.executes_statement(PlanDialect::MariaDb, true));
        assert!(!estimated.executes_statement(PlanDialect::MariaDb, true));
        assert!(actual.executes_statement(PlanDialect::SqlServer, true));
        assert!(!actual.executes_statement(PlanDialect::SqlServer, false));
        assert!(!actual.executes_statement(PlanDialect::Sqlite, true));
        assert!(!estimated.executes_statement(PlanDialect::Sqlite, false));
        assert!(!actual.executes_statement(PlanDialect::ClickHouse, true));
        assert!(!estimated.executes_statement(PlanDialect::ClickHouse, false));
        assert!(actual.executes_statement(PlanDialect::Oracle, true));
        assert!(!estimated.executes_statement(PlanDialect::Oracle, false));
    }
}
