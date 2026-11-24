//! Reading a plan document someone pasted or opened, whatever produced it.
//!
//! The app's own captures know their engine. A plan pasted from the
//! clipboard, opened from a file, or dropped on the online viewer does not.
//! [`import_plan_document`] does the whole job: [`clean_plan_text`] removes
//! what clients, chat tools and logs wrap around a plan,
//! [`detect_plan_format`] works out which engine and output format it is,
//! and the matching parser fills the shared model. When the document cannot
//! be read, the error says what it looks like, where it broke, and how to
//! capture a plan that can be read.
//!
//! [`sniff_plan_document`] and [`parse_plan_document`] are the older
//! two-step form of the same thing, kept for callers that work in terms of
//! a [`PlanDialect`].

mod clean;
mod detect;

use crate::plans::dialects::{
    PlanDialect, extract_sqlserver_message_stats, parse_clickhouse_explain_json,
    parse_clickhouse_explain_pipeline, parse_clickhouse_explain_text, parse_cockroach_explain_text,
    parse_duckdb_explain_json, parse_duckdb_explain_text, parse_mysql_explain_analyze_tree,
    parse_mysql_explain_json, parse_mysql_explain_json_v2, parse_mysql_explain_table,
    parse_oracle_plan_lines, parse_oracle_xplan_plans, parse_pg_explain_json_multi,
    parse_pg_explain_text, parse_pg_explain_xml, parse_pg_explain_yaml, parse_spark_explain_text,
    parse_sqlite_query_plan_any, parse_sqlserver_showplan_text, parse_trino_explain_text,
};
use crate::plans::error::{PlanError, PlanImportError, PlanResult};
use crate::plans::model::ExecutionPlan;

pub use clean::{clean_plan_text, decode_plan_bytes};
pub use detect::{PlanConfidence, PlanDetection, detect_plan_format};

/// A database engine whose plans can be imported. Wider than
/// [`PlanDialect`], which only lists engines the app connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanEngine {
    SqlServer,
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    DuckDb,
    ClickHouse,
    Oracle,
    CockroachDb,
    Trino,
    Spark,
}

impl PlanEngine {
    pub const ALL: [PlanEngine; 11] = [
        PlanEngine::SqlServer,
        PlanEngine::Postgres,
        PlanEngine::MySql,
        PlanEngine::MariaDb,
        PlanEngine::Sqlite,
        PlanEngine::DuckDb,
        PlanEngine::ClickHouse,
        PlanEngine::Oracle,
        PlanEngine::CockroachDb,
        PlanEngine::Trino,
        PlanEngine::Spark,
    ];

    /// Stable id, also the id of the online viewer's format picker entry.
    pub fn id(self) -> &'static str {
        match self {
            PlanEngine::SqlServer => "sqlserver",
            PlanEngine::Postgres => "postgresql",
            PlanEngine::MySql => "mysql",
            PlanEngine::MariaDb => "mariadb",
            PlanEngine::Sqlite => "sqlite",
            PlanEngine::DuckDb => "duckdb",
            PlanEngine::ClickHouse => "clickhouse",
            PlanEngine::Oracle => "oracle",
            PlanEngine::CockroachDb => "cockroachdb",
            PlanEngine::Trino => "trino",
            PlanEngine::Spark => "spark",
        }
    }

    pub fn from_id(id: &str) -> Option<PlanEngine> {
        PlanEngine::ALL
            .into_iter()
            .find(|engine| engine.id() == id.trim())
    }

    /// The engine's name as its vendor writes it.
    pub fn name(self) -> &'static str {
        match self {
            PlanEngine::SqlServer => "SQL Server",
            PlanEngine::Postgres => "PostgreSQL",
            PlanEngine::MySql => "MySQL",
            PlanEngine::MariaDb => "MariaDB",
            PlanEngine::Sqlite => "SQLite",
            PlanEngine::DuckDb => "DuckDB",
            PlanEngine::ClickHouse => "ClickHouse",
            PlanEngine::Oracle => "Oracle",
            PlanEngine::CockroachDb => "CockroachDB",
            PlanEngine::Trino => "Trino",
            PlanEngine::Spark => "Spark SQL",
        }
    }

    /// The app's capture dialect for this engine, when it has one.
    pub fn dialect(self) -> Option<PlanDialect> {
        Some(match self {
            PlanEngine::SqlServer => PlanDialect::SqlServer,
            PlanEngine::Postgres => PlanDialect::Postgres,
            PlanEngine::MySql => PlanDialect::MySql,
            PlanEngine::MariaDb => PlanDialect::MariaDb,
            PlanEngine::Sqlite => PlanDialect::Sqlite,
            PlanEngine::DuckDb => PlanDialect::DuckDb,
            PlanEngine::ClickHouse => PlanDialect::ClickHouse,
            PlanEngine::Oracle => PlanDialect::Oracle,
            PlanEngine::CockroachDb | PlanEngine::Trino | PlanEngine::Spark => return None,
        })
    }

    /// The dialect whose row, cost and timing arithmetic the plan analysis
    /// applies to this engine's plans: its own when it has one, PostgreSQL
    /// for CockroachDB, Trino and Spark. It says nothing about the engine's
    /// operators, which have their own glossary.
    pub fn analysis_dialect(self) -> PlanDialect {
        self.dialect().unwrap_or(PlanDialect::Postgres)
    }

    pub fn from_dialect(dialect: PlanDialect) -> PlanEngine {
        match dialect {
            PlanDialect::SqlServer => PlanEngine::SqlServer,
            PlanDialect::Postgres => PlanEngine::Postgres,
            PlanDialect::MySql => PlanEngine::MySql,
            PlanDialect::MariaDb => PlanEngine::MariaDb,
            PlanDialect::Sqlite => PlanEngine::Sqlite,
            PlanDialect::DuckDb => PlanEngine::DuckDb,
            PlanDialect::ClickHouse => PlanEngine::ClickHouse,
            PlanDialect::Oracle => PlanEngine::Oracle,
        }
    }

    /// How to get a plan out of this engine in a form the importer reads.
    pub fn capture_hint(self) -> &'static str {
        match self {
            PlanEngine::SqlServer => {
                "In SSMS, right-click the plan and choose Show Execution Plan XML, or save it with Save Execution Plan As (.sqlplan). SET STATISTICS XML ON returns the same XML next to the results, and SET SHOWPLAN_XML ON returns the estimated plan without running the query."
            }
            PlanEngine::Postgres => {
                "Run EXPLAIN (ANALYZE, BUFFERS) in front of the query and paste everything psql prints. EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) works too, and plain EXPLAIN gives the estimated plan without running the query."
            }
            PlanEngine::MySql => {
                "Run EXPLAIN ANALYZE in front of the query (MySQL 8.0.18 and later), or EXPLAIN FORMAT=JSON, or plain EXPLAIN, and paste the whole result including the table borders."
            }
            PlanEngine::MariaDb => {
                "Run ANALYZE FORMAT=JSON in front of the query for run-time numbers, or EXPLAIN FORMAT=JSON, or plain EXPLAIN, and paste the whole result including the table borders."
            }
            PlanEngine::Sqlite => {
                "Run EXPLAIN QUERY PLAN in front of the query in the sqlite3 shell and paste what it prints. Plain EXPLAIN prints bytecode, which is not a plan."
            }
            PlanEngine::DuckDb => {
                "Run EXPLAIN ANALYZE in front of the query (or EXPLAIN for the estimated plan) and paste the whole box drawing. EXPLAIN (FORMAT JSON) works too."
            }
            PlanEngine::ClickHouse => {
                "Run EXPLAIN indexes = 1, actions = 1 in front of the query and paste the output. EXPLAIN json = 1, indexes = 1 and EXPLAIN PIPELINE work too."
            }
            PlanEngine::Oracle => {
                "Run EXPLAIN PLAN FOR in front of the query, then SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY) and paste the table. For run-time numbers, run the query with the gather_plan_statistics hint and paste SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(FORMAT => 'ALLSTATS LAST'))."
            }
            PlanEngine::CockroachDb => {
                "Run EXPLAIN ANALYZE in front of the query (or EXPLAIN for the estimated plan) in cockroach sql and paste everything it prints."
            }
            PlanEngine::Trino => {
                "Run EXPLAIN ANALYZE in front of the query (or EXPLAIN for the estimated plan) and paste the text it returns."
            }
            PlanEngine::Spark => {
                "Run EXPLAIN FORMATTED in front of the query, or call df.explain(\"formatted\"), and paste the text. Plain EXPLAIN and EXPLAIN EXTENDED work too."
            }
        }
    }
}

/// One output format of one engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanFormat {
    SqlServerXml,
    SqlServerText,
    PostgresText,
    PostgresJson,
    PostgresYaml,
    PostgresXml,
    MySqlTable,
    MySqlTree,
    MySqlJson,
    MySqlJsonV2,
    MariaDbTable,
    MariaDbJson,
    OracleXplan,
    OracleRows,
    SqliteQueryPlan,
    DuckDbText,
    DuckDbJson,
    ClickHouseText,
    ClickHousePipeline,
    ClickHouseJson,
    CockroachText,
    TrinoText,
    SparkText,
}

impl PlanFormat {
    pub const ALL: [PlanFormat; 23] = [
        PlanFormat::SqlServerXml,
        PlanFormat::SqlServerText,
        PlanFormat::PostgresText,
        PlanFormat::PostgresJson,
        PlanFormat::PostgresYaml,
        PlanFormat::PostgresXml,
        PlanFormat::MySqlTable,
        PlanFormat::MySqlTree,
        PlanFormat::MySqlJson,
        PlanFormat::MySqlJsonV2,
        PlanFormat::MariaDbTable,
        PlanFormat::MariaDbJson,
        PlanFormat::OracleXplan,
        PlanFormat::OracleRows,
        PlanFormat::SqliteQueryPlan,
        PlanFormat::DuckDbText,
        PlanFormat::DuckDbJson,
        PlanFormat::ClickHouseText,
        PlanFormat::ClickHousePipeline,
        PlanFormat::ClickHouseJson,
        PlanFormat::CockroachText,
        PlanFormat::TrinoText,
        PlanFormat::SparkText,
    ];

    /// Stable id; parsers write the same string to
    /// [`ExecutionPlan::source_format`].
    pub fn id(self) -> &'static str {
        match self {
            PlanFormat::SqlServerXml => "sqlserver-xml",
            PlanFormat::SqlServerText => "sqlserver-text",
            PlanFormat::PostgresText => "postgresql-text",
            PlanFormat::PostgresJson => "postgresql-json",
            PlanFormat::PostgresYaml => "postgresql-yaml",
            PlanFormat::PostgresXml => "postgresql-xml",
            PlanFormat::MySqlTable => "mysql-table",
            PlanFormat::MySqlTree => "mysql-tree",
            PlanFormat::MySqlJson => "mysql-json",
            PlanFormat::MySqlJsonV2 => "mysql-json-v2",
            PlanFormat::MariaDbTable => "mariadb-table",
            PlanFormat::MariaDbJson => "mariadb-json",
            PlanFormat::OracleXplan => "oracle-xplan",
            PlanFormat::OracleRows => "oracle-rows",
            PlanFormat::SqliteQueryPlan => "sqlite-eqp",
            PlanFormat::DuckDbText => "duckdb-text",
            PlanFormat::DuckDbJson => "duckdb-json",
            PlanFormat::ClickHouseText => "clickhouse-text",
            PlanFormat::ClickHousePipeline => "clickhouse-pipeline",
            PlanFormat::ClickHouseJson => "clickhouse-json",
            PlanFormat::CockroachText => "cockroachdb-text",
            PlanFormat::TrinoText => "trino-text",
            PlanFormat::SparkText => "spark-text",
        }
    }

    pub fn from_id(id: &str) -> Option<PlanFormat> {
        PlanFormat::ALL
            .into_iter()
            .find(|format| format.id() == id.trim())
    }

    /// What the format is called where it comes from.
    pub fn label(self) -> &'static str {
        match self {
            PlanFormat::SqlServerXml => "SQL Server ShowPlan XML",
            PlanFormat::SqlServerText => "SQL Server text showplan",
            PlanFormat::PostgresText => "PostgreSQL EXPLAIN text",
            PlanFormat::PostgresJson => "PostgreSQL EXPLAIN (FORMAT JSON)",
            PlanFormat::PostgresYaml => "PostgreSQL EXPLAIN (FORMAT YAML)",
            PlanFormat::PostgresXml => "PostgreSQL EXPLAIN (FORMAT XML)",
            PlanFormat::MySqlTable => "MySQL EXPLAIN table",
            PlanFormat::MySqlTree => "MySQL EXPLAIN FORMAT=TREE",
            PlanFormat::MySqlJson => "MySQL EXPLAIN FORMAT=JSON",
            PlanFormat::MySqlJsonV2 => "MySQL EXPLAIN FORMAT=JSON (version 2)",
            PlanFormat::MariaDbTable => "MariaDB EXPLAIN table",
            PlanFormat::MariaDbJson => "MariaDB EXPLAIN FORMAT=JSON",
            PlanFormat::OracleXplan => "Oracle DBMS_XPLAN output",
            PlanFormat::OracleRows => "Oracle PLAN_TABLE rows",
            PlanFormat::SqliteQueryPlan => "SQLite EXPLAIN QUERY PLAN",
            PlanFormat::DuckDbText => "DuckDB EXPLAIN text",
            PlanFormat::DuckDbJson => "DuckDB EXPLAIN (FORMAT JSON)",
            PlanFormat::ClickHouseText => "ClickHouse EXPLAIN text",
            PlanFormat::ClickHousePipeline => "ClickHouse EXPLAIN PIPELINE",
            PlanFormat::ClickHouseJson => "ClickHouse EXPLAIN json = 1",
            PlanFormat::CockroachText => "CockroachDB EXPLAIN text",
            PlanFormat::TrinoText => "Trino EXPLAIN text",
            PlanFormat::SparkText => "Spark SQL EXPLAIN text",
        }
    }

    pub fn engine(self) -> PlanEngine {
        match self {
            PlanFormat::SqlServerXml | PlanFormat::SqlServerText => PlanEngine::SqlServer,
            PlanFormat::PostgresText
            | PlanFormat::PostgresJson
            | PlanFormat::PostgresYaml
            | PlanFormat::PostgresXml => PlanEngine::Postgres,
            PlanFormat::MySqlTable
            | PlanFormat::MySqlTree
            | PlanFormat::MySqlJson
            | PlanFormat::MySqlJsonV2 => PlanEngine::MySql,
            PlanFormat::MariaDbTable | PlanFormat::MariaDbJson => PlanEngine::MariaDb,
            PlanFormat::OracleXplan | PlanFormat::OracleRows => PlanEngine::Oracle,
            PlanFormat::SqliteQueryPlan => PlanEngine::Sqlite,
            PlanFormat::DuckDbText | PlanFormat::DuckDbJson => PlanEngine::DuckDb,
            PlanFormat::ClickHouseText
            | PlanFormat::ClickHousePipeline
            | PlanFormat::ClickHouseJson => PlanEngine::ClickHouse,
            PlanFormat::CockroachText => PlanEngine::CockroachDb,
            PlanFormat::TrinoText => PlanEngine::Trino,
            PlanFormat::SparkText => PlanEngine::Spark,
        }
    }

    /// The app's capture dialect for this format's engine, when it has one.
    pub fn dialect(self) -> Option<PlanDialect> {
        self.engine().dialect()
    }
}

/// The result of [`import_plan_document`].
#[derive(Debug, Clone)]
pub struct ImportedPlan {
    pub format: PlanFormat,
    pub confidence: PlanConfidence,
    /// One plan per statement the document holds.
    pub plans: Vec<ExecutionPlan>,
}

/// Read a pasted or opened plan document: clean it, work out the format,
/// parse it. `text` is the raw text; nothing needs to be done to it first.
pub fn import_plan_document(text: &str) -> PlanResult<ImportedPlan> {
    let cleaned = clean_plan_text(text);
    if cleaned.trim().is_empty() {
        return Err(PlanError::EmptyPlan);
    }
    let Some(detection) = detect_plan_format(&cleaned) else {
        return Err(unrecognized(&cleaned));
    };
    let plans = parse_format(detection.format, &cleaned)?;
    Ok(imported(detection.format, detection.confidence, plans))
}

/// Read a plan document the user says came from `engine`. The format within
/// the engine is still detected. MySQL and MariaDB are accepted for each
/// other, since people pick either for both. If the document is certainly
/// another engine's, it is read as that engine and the result's `format`
/// says so.
pub fn import_plan_document_as(engine: PlanEngine, text: &str) -> PlanResult<ImportedPlan> {
    let cleaned = clean_plan_text(text);
    if cleaned.trim().is_empty() {
        return Err(PlanError::EmptyPlan);
    }
    let detection = detect_plan_format(&cleaned);
    if let Some(found) = &detection
        && same_family(found.format.engine(), engine)
    {
        let plans = parse_format(found.format, &cleaned)?;
        return Ok(imported(found.format, found.confidence, plans));
    }
    // Detection did not agree: try the engine's formats in turn.
    let mut first_error = None;
    for format in PlanFormat::ALL
        .into_iter()
        .filter(|format| same_family(format.engine(), engine))
    {
        match parse_format(format, &cleaned) {
            Ok(plans) if plans.iter().any(|plan| !plan.nodes.is_empty()) => {
                return Ok(imported(format, PlanConfidence::Guess, plans));
            }
            Ok(_) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(found) = detection {
        if found.confidence == PlanConfidence::Certain
            && let Ok(plans) = parse_format(found.format, &cleaned)
        {
            return Ok(imported(found.format, found.confidence, plans));
        }
        return Err(PlanError::Import(Box::new(PlanImportError {
            format_id: Some(found.format.id()),
            format_label: Some(found.format.label().to_string()),
            line: None,
            column: None,
            detail: format!(
                "it does not read as a {} plan; it looks like {}",
                engine.name(),
                found.format.label()
            ),
            hint: Some(engine.capture_hint().to_string()),
        })));
    }
    if let Some(reason) = detect::unreadable_shape(&cleaned) {
        return Err(PlanError::Import(Box::new(reason)));
    }
    Err(PlanError::Import(Box::new(PlanImportError {
        format_id: None,
        format_label: None,
        line: None,
        column: None,
        detail: format!(
            "this does not read as a {} plan{}",
            engine.name(),
            first_error
                .map(|error| format!(" ({})", error.message().fallback()))
                .unwrap_or_default()
        ),
        hint: Some(engine.capture_hint().to_string()),
    })))
}

fn same_family(a: PlanEngine, b: PlanEngine) -> bool {
    let mysql = |engine| matches!(engine, PlanEngine::MySql | PlanEngine::MariaDb);
    a == b || (mysql(a) && mysql(b))
}

fn imported(
    format: PlanFormat,
    confidence: PlanConfidence,
    mut plans: Vec<ExecutionPlan>,
) -> ImportedPlan {
    // A parser that reads two engines' output (the MySQL family) knows
    // which one it saw better than detection does.
    let format = plans
        .first()
        .and_then(|plan| plan.source_format.as_deref())
        .and_then(PlanFormat::from_id)
        .filter(|parsed| same_family(parsed.engine(), format.engine()))
        .unwrap_or(format);
    for plan in &mut plans {
        stamp(plan, format);
    }
    ImportedPlan {
        format,
        confidence,
        plans,
    }
}

/// Fill what every imported plan must carry, whatever its parser set.
fn stamp(plan: &mut ExecutionPlan, format: PlanFormat) {
    if plan.source_format.is_none() {
        plan.source_format = Some(format.id().to_string());
    }
    if plan.dialect.is_none() {
        plan.dialect = format.dialect();
    }
    if !plan.has_actuals {
        plan.has_actuals = plan
            .nodes
            .iter()
            .any(|node| node.actual_rows.is_some() || node.actual_time_ms.is_some());
    }
}

/// Run the parser for `format` over cleaned text, and turn a failure into
/// an error that names the format, the position and the way out.
fn parse_format(format: PlanFormat, text: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let one = |plan: PlanResult<ExecutionPlan>| plan.map(|plan| vec![plan]);
    let result = match format {
        PlanFormat::SqlServerXml => {
            let (xml, stats) = extract_sqlserver_message_stats(text);
            crate::plans::parse_showplan_xml_multi(detect::xml_slice(&xml)).map(|mut plans| {
                if !stats.is_empty() {
                    // Messages belong to the batch; attach them to the last
                    // statement that has operators, as SSMS shows them.
                    if let Some(plan) = plans.iter_mut().rev().find(|p| !p.nodes.is_empty()) {
                        stats.apply(plan);
                    }
                }
                plans
            })
        }
        PlanFormat::SqlServerText => {
            let (body, stats) = extract_sqlserver_message_stats(text);
            parse_sqlserver_showplan_text(&body).map(|mut plans| {
                if !stats.is_empty()
                    && let Some(plan) = plans.iter_mut().rev().find(|p| !p.nodes.is_empty())
                {
                    stats.apply(plan);
                }
                plans
            })
        }
        PlanFormat::PostgresText => parse_pg_explain_text(text),
        PlanFormat::PostgresJson => parse_pg_explain_json_multi(detect::json_slice(text)),
        PlanFormat::PostgresYaml => parse_pg_explain_yaml(text),
        PlanFormat::PostgresXml => parse_pg_explain_xml(detect::xml_slice(text)),
        PlanFormat::MySqlTable | PlanFormat::MariaDbTable => one(parse_mysql_explain_table(text)),
        PlanFormat::MySqlTree => one(parse_mysql_explain_analyze_tree(text)),
        PlanFormat::MySqlJson | PlanFormat::MariaDbJson => {
            one(parse_mysql_explain_json(detect::json_slice(text)))
        }
        PlanFormat::MySqlJsonV2 => one(parse_mysql_explain_json_v2(detect::json_slice(text))),
        PlanFormat::OracleXplan => parse_oracle_xplan_plans(text),
        PlanFormat::OracleRows => one(parse_oracle_plan_lines(text)),
        PlanFormat::SqliteQueryPlan => one(parse_sqlite_query_plan_any(text)),
        PlanFormat::DuckDbText => one(parse_duckdb_explain_text(text)),
        PlanFormat::DuckDbJson => one(parse_duckdb_explain_json(detect::json_slice(text))),
        PlanFormat::ClickHouseText => one(parse_clickhouse_explain_text(text)),
        PlanFormat::ClickHousePipeline => one(parse_clickhouse_explain_pipeline(text)),
        PlanFormat::ClickHouseJson => one(parse_clickhouse_explain_json(detect::json_slice(text))),
        PlanFormat::CockroachText => one(parse_cockroach_explain_text(text)),
        PlanFormat::TrinoText => one(parse_trino_explain_text(text)),
        PlanFormat::SparkText => one(parse_spark_explain_text(text)),
    };
    match result {
        Ok(plans) if plans.iter().any(|plan| !plan.nodes.is_empty()) => Ok(plans),
        Ok(_) => Err(import_error(
            format,
            text,
            "it holds no operators. If it came from a batch, the statements that ran may have produced no plan (SET and DECLARE do not)".to_string(),
        )),
        Err(PlanError::Import(error)) => Err(PlanError::Import(error)),
        Err(error) => Err(import_error(format, text, detail_of(&error))),
    }
}

fn detail_of(error: &PlanError) -> String {
    match error {
        PlanError::EmptyPlan => "it holds no operators".to_string(),
        PlanError::XmlParse(detail)
        | PlanError::InvalidStructure(detail)
        | PlanError::Other(detail) => detail.clone(),
        PlanError::Import(inner) => inner.detail.clone(),
    }
}

fn import_error(format: PlanFormat, text: &str, detail: String) -> PlanError {
    let (line, column) = detect::error_position(format, text, &detail);
    PlanError::Import(Box::new(PlanImportError {
        format_id: Some(format.id()),
        format_label: Some(format.label().to_string()),
        line,
        column,
        detail,
        hint: Some(format.engine().capture_hint().to_string()),
    }))
}

fn unrecognized(cleaned: &str) -> PlanError {
    if let Some(reason) = detect::unreadable_shape(cleaned) {
        return PlanError::Import(Box::new(reason));
    }
    PlanError::Import(Box::new(PlanImportError {
        format_id: None,
        format_label: None,
        line: None,
        column: None,
        detail: "this does not look like an execution plan".to_string(),
        hint: Some(
            "Paste the plan exactly as the database tool printed it: SQL Server ShowPlan XML or a .sqlplan file, PostgreSQL EXPLAIN output in any format, MySQL or MariaDB EXPLAIN (table, TREE or JSON), Oracle DBMS_XPLAN output, SQLite EXPLAIN QUERY PLAN, DuckDB, ClickHouse, CockroachDB, Trino or Spark SQL EXPLAIN output.".to_string(),
        ),
    }))
}

/// What a pasted or opened plan document is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanSniff {
    /// A document [`parse_plan_document`] reads for this engine.
    Dialect(PlanDialect),
    /// A recognizable document that cannot be read through a
    /// [`PlanDialect`]: something that is not a plan (SQLite bytecode, a
    /// ClickHouse syntax tree), or a plan from an engine the app has no
    /// dialect for, which [`import_plan_document`] reads. The English
    /// reason says which.
    Unsupported(String),
    Unknown,
}

/// Recognize the engine that produced `text` (already [`clean_plan_text`]ed).
/// CockroachDB plans report as [`PlanDialect::Postgres`], the dialect the
/// app talks to CockroachDB with; [`parse_plan_document`] reads them.
pub fn sniff_plan_document(text: &str) -> PlanSniff {
    match detect_plan_format(text) {
        Some(found) => match found.format.engine() {
            PlanEngine::CockroachDb => PlanSniff::Dialect(PlanDialect::Postgres),
            engine => match engine.dialect() {
                Some(dialect) => PlanSniff::Dialect(dialect),
                // No connection dialect to hand back; callers that want
                // these read them with `import_plan_document`.
                None => PlanSniff::Unsupported(format!(
                    "This is a {} plan, which cannot be opened here yet.",
                    engine.name()
                )),
            },
        },
        None => match detect::unreadable_shape(text) {
            Some(reason) => PlanSniff::Unsupported(reason.to_string()),
            None => PlanSniff::Unknown,
        },
    }
}

/// Parse a document as `dialect` (from [`sniff_plan_document`], or the
/// caller's choice). Reads every format of that engine, not only the one
/// the app captures. Every returned plan has `dialect` and `has_actuals`
/// set.
pub fn parse_plan_document(dialect: PlanDialect, text: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let engine = PlanEngine::from_dialect(dialect);
    let detection = detect_plan_format(text);
    let format = match &detection {
        Some(found) if same_family(found.format.engine(), engine) => Some(found.format),
        Some(found)
            if found.format == PlanFormat::CockroachText && dialect == PlanDialect::Postgres =>
        {
            Some(found.format)
        }
        _ => None,
    };
    let mut plans = match format {
        Some(format) => {
            let mut plans = parse_format(format, text)?;
            for plan in &mut plans {
                stamp(plan, format);
            }
            plans
        }
        None => import_plan_document_as(engine, text)?.plans,
    };
    for plan in &mut plans {
        if plan.dialect.is_none() && plan.source_format.as_deref() != Some("cockroachdb-text") {
            plan.dialect = Some(dialect);
        }
    }
    Ok(plans)
}

#[cfg(test)]
mod tests;
