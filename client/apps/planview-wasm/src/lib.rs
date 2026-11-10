//! SQLly's execution-plan reader for the browser.
//!
//! The free online plan viewer at `sqlly.app/tools/query-plan-viewer` runs
//! this crate compiled to `wasm32-unknown-unknown`. It is a thin shell over
//! `sqlly_intel::plans` — the parsers, the findings engine, the operator
//! tree and the diagram layout the desktop app's plan pane uses — so a plan
//! pasted on the site reads exactly as it does in the app.
//!
//! Recognizing the engine and unwrapping what terminal clients put around a
//! plan live in `sqlly_intel::plans::import`, shared with the app's Open
//! Plan / Paste Plan. What the shell adds is the page's own vocabulary
//! (format ids, visitor-facing errors) and the one JSON document the page
//! renders. Everything here except the
//! `bindings` module is plain Rust, unit-tested natively.

use serde_json::{Value, json};
use sqlly_intel::plans::warning::PlanWarning;
use sqlly_intel::plans::{
    DIAGRAM_NODE_H, DIAGRAM_NODE_W, ExecutionPlan, FindingSeverity, PlanDialect, PlanFinding,
    PlanSniff, clean_plan_text, findings_for_plan, human_count, operator_rows, parse_plan_document,
    plan_diagram_layout, plan_tree_rows, sniff_plan_document,
};

/// A plan format the viewer reads. The ids are what the page's format
/// picker sends; keep them in sync with `QueryPlanViewerModel.Formats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    SqlServer,
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    DuckDb,
    ClickHouse,
    Oracle,
}

impl Format {
    pub const ALL: [Format; 8] = [
        Format::SqlServer,
        Format::Postgres,
        Format::MySql,
        Format::MariaDb,
        Format::Sqlite,
        Format::DuckDb,
        Format::ClickHouse,
        Format::Oracle,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Format::SqlServer => "sqlserver",
            Format::Postgres => "postgresql",
            Format::MySql => "mysql",
            Format::MariaDb => "mariadb",
            Format::Sqlite => "sqlite",
            Format::DuckDb => "duckdb",
            Format::ClickHouse => "clickhouse",
            Format::Oracle => "oracle",
        }
    }

    /// The shared plan module's engine for this format.
    pub fn dialect(self) -> PlanDialect {
        match self {
            Format::SqlServer => PlanDialect::SqlServer,
            Format::Postgres => PlanDialect::Postgres,
            Format::MySql => PlanDialect::MySql,
            Format::MariaDb => PlanDialect::MariaDb,
            Format::Sqlite => PlanDialect::Sqlite,
            Format::DuckDb => PlanDialect::DuckDb,
            Format::ClickHouse => PlanDialect::ClickHouse,
            Format::Oracle => PlanDialect::Oracle,
        }
    }

    pub fn from_dialect(dialect: PlanDialect) -> Format {
        match dialect {
            PlanDialect::SqlServer => Format::SqlServer,
            PlanDialect::Postgres => Format::Postgres,
            PlanDialect::MySql => Format::MySql,
            PlanDialect::MariaDb => Format::MariaDb,
            PlanDialect::Sqlite => Format::Sqlite,
            PlanDialect::DuckDb => Format::DuckDb,
            PlanDialect::ClickHouse => Format::ClickHouse,
            PlanDialect::Oracle => Format::Oracle,
        }
    }

    pub fn from_id(id: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.id() == id.trim())
    }

    /// The engine, as the page names it.
    pub fn engine(self) -> &'static str {
        match self {
            Format::SqlServer => "SQL Server",
            Format::Postgres => "PostgreSQL",
            Format::MySql => "MySQL",
            Format::MariaDb => "MariaDB",
            Format::Sqlite => "SQLite",
            Format::DuckDb => "DuckDB",
            Format::ClickHouse => "ClickHouse",
            Format::Oracle => "Oracle",
        }
    }
}

/// Undo what clients wrap around a plan (see
/// `sqlly_intel::plans::clean_plan_text`).
pub fn clean_input(text: &str) -> String {
    clean_plan_text(text)
}

/// What the pasted text is, as one of the viewer's formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    Format(Format),
    Unsupported(String),
    Unknown,
}

/// Recognize the engine that produced `text` (already [`clean_input`]ed).
pub fn detect(text: &str) -> Detected {
    match sniff_plan_document(text) {
        PlanSniff::Dialect(dialect) => Detected::Format(Format::from_dialect(dialect)),
        PlanSniff::Unsupported(why) => Detected::Unsupported(why),
        PlanSniff::Unknown => Detected::Unknown,
    }
}

/// Parse `text` as `format` into one plan per statement.
pub fn parse(text: &str, format: Format) -> Result<Vec<ExecutionPlan>, String> {
    let result = parse_plan_document(format.dialect(), text);
    let plans = result.map_err(|e| {
        format!(
            "Couldn't read this as a {} plan: {}",
            format.engine(),
            e.message().fallback()
        )
    })?;
    if plans.iter().all(|p| p.nodes.is_empty()) {
        return Err(format!(
            "The {} plan has no operators to show. If it came from a batch, the statements that ran may have produced no plan (SET and DECLARE do not).",
            format.engine()
        ));
    }
    Ok(plans)
}

/// Analyze `text` and return the page's JSON document. `format_id` is a
/// [`Format::id`] or empty / `"auto"` to recognize the engine.
pub fn analyze(text: &str, format_id: &str) -> Result<Value, String> {
    let cleaned = clean_input(text);
    if cleaned.is_empty() {
        return Err("Paste an execution plan, open a plan file, or pick a sample.".into());
    }
    let (format, detected) = match format_id.trim() {
        "" | "auto" => match detect(&cleaned) {
            Detected::Format(f) => (f, true),
            Detected::Unsupported(why) => return Err(why),
            Detected::Unknown => {
                return Err("This doesn't look like an execution plan SQLly can read. Paste SQL Server ShowPlan XML, PostgreSQL EXPLAIN (FORMAT JSON), MySQL or MariaDB EXPLAIN FORMAT=JSON or EXPLAIN ANALYZE, SQLite EXPLAIN QUERY PLAN, DuckDB or ClickHouse EXPLAIN JSON, or Oracle DBMS_XPLAN output.".into());
            }
        },
        id => (
            Format::from_id(id).ok_or_else(|| format!("unknown plan format \"{id}\""))?,
            false,
        ),
    };
    let plans = parse(&cleaned, format)?;
    let statements: Vec<Value> = plans
        .iter()
        .enumerate()
        .filter(|(_, plan)| !plan.nodes.is_empty())
        .map(|(index, plan)| statement_json(index, plan))
        .collect();
    let skipped = plans.len() - statements.len();
    Ok(json!({
        "format": format.id(),
        "engine": format.engine(),
        "detected": detected,
        "statements": statements,
        "statementsWithoutPlan": skipped,
    }))
}

fn statement_json(index: usize, plan: &ExecutionPlan) -> Value {
    let actual = plan.nodes.iter().any(|n| n.actual_rows.is_some());
    let has_costs = plan.total_subtree_cost > 0.0;
    let title_of = |id: i32| {
        plan.nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| display_op(&n.physical_op, &n.logical_op))
            .unwrap_or_default()
    };
    let cost_percent = |cost: f64| {
        if has_costs {
            (cost / plan.total_subtree_cost * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        }
    };

    let findings: Vec<Value> = findings_for_plan(plan)
        .iter()
        .map(|f| finding_json(f, &title_of))
        .collect();
    let nodes: Vec<Value> = plan
        .nodes
        .iter()
        .map(|n| {
            json!({
                "id": n.id,
                "title": display_op(&n.physical_op, &n.logical_op),
                "physicalOp": n.physical_op,
                "logicalOp": n.logical_op,
                "estimatedRows": finite(n.estimated_rows),
                "estimatedRowsText": human_count(n.estimated_rows),
                "actualRows": n.actual_rows.map(finite),
                "actualRowsText": n.actual_rows.map(human_count),
                "estimatedCost": finite(n.estimated_cost),
                "estimatedCpu": finite(n.estimated_cpu),
                "estimatedIo": finite(n.estimated_io),
                "costPercent": finite(cost_percent(n.estimated_cost)),
                "rebinds": n.actual_rebinds,
                "rewinds": n.actual_rewinds,
                "details": n.output_list,
                "warnings": n.warnings.iter().map(warning_label).collect::<Vec<_>>(),
            })
        })
        .collect();
    let edges: Vec<Value> = plan
        .edges
        .iter()
        .map(|e| json!({ "from": e.from_node, "to": e.to_node }))
        .collect();
    let tree: Vec<Value> = plan_tree_rows(plan)
        .iter()
        .map(|r| {
            json!({
                "nodeId": r.node_id,
                "depth": r.depth,
                "hasChildren": r.has_children,
                "title": r.title,
                "costPercent": finite(r.cost_percent),
                "estimatedRows": human_count(r.estimated_rows),
                "actualRows": r.actual_rows.map(human_count),
                "diverges": r.actual_rows.is_some_and(|a| diverges(r.estimated_rows, a)),
                "warningCount": r.warning_count,
            })
        })
        .collect();
    let layout = plan_diagram_layout(plan);
    let diagram = json!({
        "width": layout.width,
        "height": layout.height,
        "nodeWidth": DIAGRAM_NODE_W,
        "nodeHeight": DIAGRAM_NODE_H,
        "nodes": layout.nodes.iter().map(|n| json!({
            "nodeId": n.node_id,
            "title": n.title,
            "x": n.x,
            "y": n.y,
            "costPercent": finite(n.cost_percent),
            "estimatedRows": human_count(n.estimated_rows),
            "actualRows": n.actual_rows.map(human_count),
            "warningCount": n.warning_count,
        })).collect::<Vec<_>>(),
        "edges": layout.edges.iter().map(|e| json!({ "from": e.from_node, "to": e.to_node })).collect::<Vec<_>>(),
    });
    let operators: Vec<Value> = operator_rows(plan)
        .iter()
        .map(|r| {
            json!({
                "nodeId": r.node_id,
                "title": r.title,
                "summary": r.summary.fallback(),
                "costPercent": finite(r.cost_percent),
                "hasWarnings": r.has_warnings,
            })
        })
        .collect();
    let counts = |severity: FindingSeverity| {
        findings_for_plan(plan)
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    };
    json!({
        "index": index,
        "statementText": plan.statement_text,
        "statementType": plan.statement_type,
        "kind": if actual { "actual" } else { "estimated" },
        "hasCosts": has_costs,
        "totalCost": finite(plan.total_subtree_cost),
        "estimatedRows": human_count(plan.estimated_rows),
        "operatorCount": plan.nodes.len(),
        "findingCounts": {
            "critical": counts(FindingSeverity::Critical),
            "warning": counts(FindingSeverity::Warning),
            "info": counts(FindingSeverity::Info),
        },
        "findings": findings,
        "nodes": nodes,
        "edges": edges,
        "tree": tree,
        "diagram": diagram,
        "operators": operators,
    })
}

fn finding_json(finding: &PlanFinding, title_of: &dyn Fn(i32) -> String) -> Value {
    json!({
        "kind": finding_kind(finding),
        "severity": match finding.severity {
            FindingSeverity::Critical => "critical",
            FindingSeverity::Warning => "warning",
            FindingSeverity::Info => "info",
        },
        "summary": finding.summary.fallback(),
        "why": finding.why.fallback(),
        "action": finding.action.fallback(),
        "actionSql": finding.action_sql,
        "detail": finding.detail.fallback(),
        "nodeId": finding.node_id,
        "nodeTitle": finding.node_id.map(title_of),
    })
}

/// The finding's family from its catalog key
/// (`services.plans.findings.<kind>.summary…`): the page links each kind to
/// its explanation.
pub fn finding_kind(finding: &PlanFinding) -> String {
    finding
        .summary
        .key()
        .and_then(|key| key.strip_prefix("services.plans.findings."))
        .and_then(|rest| rest.split('.').next())
        .unwrap_or("other")
        .to_string()
}

/// The same 10× rule the findings engine uses for an estimate miss.
fn diverges(estimated: f64, actual: f64) -> bool {
    let larger = estimated.max(actual);
    let smaller = estimated.min(actual).max(1.0);
    larger / smaller >= 10.0
}

fn display_op(physical: &str, logical: &str) -> String {
    if physical.is_empty() {
        logical.to_string()
    } else {
        physical.to_string()
    }
}

/// JSON has no NaN or infinity; a malformed plan must not produce them.
fn finite(n: f64) -> f64 {
    if n.is_finite() { n } else { 0.0 }
}

/// One line per operator warning for the tree and the details panel.
pub fn warning_label(warning: &PlanWarning) -> String {
    let qualified = |schema: &str, table: &str| {
        let schema = schema.trim_matches(['[', ']']);
        let table = table.trim_matches(['[', ']']);
        match (schema.is_empty(), table.is_empty()) {
            (_, true) => String::new(),
            (true, false) => table.to_string(),
            (false, false) => format!("{schema}.{table}"),
        }
    };
    match warning {
        PlanWarning::MissingIndex(index) => {
            let table = qualified(&index.schema, &index.table);
            format!(
                "Missing index on {table} (estimated impact {:.0}%)",
                index.impact
            )
        }
        PlanWarning::TableScan { table, schema } => match qualified(schema, table) {
            t if t.is_empty() => "Reads every row".to_string(),
            t => format!("Reads every row of {t}"),
        },
        PlanWarning::ImplicitConversion { expression } if expression.is_empty() => {
            "Implicit conversion prevents an index seek".to_string()
        }
        PlanWarning::ImplicitConversion { expression } => {
            format!("Implicit conversion prevents an index seek: {expression}")
        }
        PlanWarning::KeyLookup { table, .. } if table.is_empty() => "Lookup per row".to_string(),
        PlanWarning::KeyLookup { table, .. } => format!("Lookup per row into {table}"),
        PlanWarning::RowGoal => "Row goal (TOP, EXISTS, FAST n)".to_string(),
        PlanWarning::CardinalityEstimation => "Columns with no statistics".to_string(),
        PlanWarning::Spool => "Spool".to_string(),
        PlanWarning::SpillToTempDb => "Spilled to tempdb".to_string(),
        PlanWarning::NoJoinPredicate => "No join predicate".to_string(),
        PlanWarning::Other(text) => text.clone(),
    }
}

/// A real plan, captured for the page's sample picker.
pub struct Sample {
    pub id: &'static str,
    pub format: Format,
    pub title: &'static str,
    /// The command that captured it.
    pub capture: &'static str,
    /// The SQL it explains.
    pub query: &'static str,
    pub text: &'static str,
}

/// Every sample was captured from the engine named, against a small shop
/// schema (`customers`, `orders`); none is hand-written.
pub const SAMPLES: &[Sample] = &[
    Sample {
        id: "sqlserver",
        format: Format::SqlServer,
        title: "SQL Server 2025, actual plan (3 statements)",
        capture: "SET STATISTICS XML ON;",
        query: "DECLARE @email nvarchar(200) = N'user4242@example.com';\nDECLARE @since date = '2026-06-25';\n\nSELECT c.CustomerID, c.Email, o.OrderID, o.OrderDate, o.Status, o.Total\nFROM dbo.Customers AS c\nJOIN dbo.Orders AS o ON o.CustomerID = c.CustomerID\nWHERE c.Email = @email\nORDER BY o.OrderDate DESC;\n\nSELECT o.Status, COUNT(*) AS Orders, SUM(o.Total) AS Revenue\nFROM dbo.Orders AS o\nWHERE o.OrderDate >= @since\nGROUP BY o.Status;\n\nSELECT TOP (20) OrderID, OrderDate, Status, Total\nFROM dbo.Orders\nWHERE OrderDate >= '2026-03-01'\nORDER BY OrderDate;",
        text: include_str!("../samples/sqlserver-actual.xml"),
    },
    Sample {
        id: "postgresql",
        format: Format::Postgres,
        title: "PostgreSQL 17, EXPLAIN ANALYZE",
        capture: "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)",
        query: "SELECT c.email, o.id, o.order_date, o.total\nFROM customers c\nJOIN orders o ON o.customer_id = c.id\nWHERE lower(c.email) = 'user4242@example.com'\nORDER BY o.order_date DESC;",
        text: include_str!("../samples/postgresql-analyze.json"),
    },
    Sample {
        id: "mysql",
        format: Format::MySql,
        title: "MySQL 8.4, EXPLAIN FORMAT=JSON",
        capture: "EXPLAIN FORMAT=JSON",
        query: "SELECT c.country, COUNT(*) AS orders, SUM(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/mysql-explain.json"),
    },
    Sample {
        id: "mysql-analyze",
        format: Format::MySql,
        title: "MySQL 8.4, EXPLAIN ANALYZE",
        capture: "EXPLAIN ANALYZE",
        query: "SELECT c.country, COUNT(*) AS orders, SUM(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/mysql-analyze.txt"),
    },
    Sample {
        id: "mariadb",
        format: Format::MariaDb,
        title: "MariaDB 11.4, ANALYZE FORMAT=JSON",
        capture: "ANALYZE FORMAT=JSON",
        query: "SELECT c.country, COUNT(*) AS orders, SUM(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/mariadb-analyze.json"),
    },
    Sample {
        id: "sqlite",
        format: Format::Sqlite,
        title: "SQLite 3.51, EXPLAIN QUERY PLAN",
        capture: "EXPLAIN QUERY PLAN",
        query: "SELECT c.country, count(*) AS orders, sum(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\n  AND c.email NOT IN (SELECT email FROM customers WHERE country = 'US')\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/sqlite.txt"),
    },
    Sample {
        id: "duckdb",
        format: Format::DuckDb,
        title: "DuckDB 2.0, EXPLAIN ANALYZE",
        capture: "EXPLAIN (ANALYZE, FORMAT JSON)",
        query: "SELECT c.country, count(*) AS orders, sum(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/duckdb-analyze.json"),
    },
    Sample {
        id: "clickhouse",
        format: Format::ClickHouse,
        title: "ClickHouse 26.9, EXPLAIN json = 1",
        capture: "EXPLAIN json = 1, indexes = 1",
        query: "SELECT status, count() AS orders\nFROM orders\nWHERE customer_id = 4242\nGROUP BY status;",
        text: include_str!("../samples/clickhouse.json"),
    },
    Sample {
        id: "oracle",
        format: Format::Oracle,
        title: "Oracle 23ai Free, DBMS_XPLAN ALLSTATS LAST",
        capture: "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(NULL, NULL, 'ALLSTATS LAST'));",
        query: "SELECT /*+ GATHER_PLAN_STATISTICS */ c.country, COUNT(*) AS orders, SUM(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/oracle-allstats.txt"),
    },
];

/// The sample list as the page's JSON.
pub fn samples_json() -> Value {
    Value::Array(
        SAMPLES
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "format": s.format.id(),
                    "engine": s.format.engine(),
                    "title": s.title,
                    "capture": s.capture,
                    "query": s.query,
                    "text": s.text,
                })
            })
            .collect(),
    )
}

/// The JavaScript surface, generated by wasm-bindgen. Present only on wasm.
#[cfg(target_arch = "wasm32")]
mod bindings {
    use wasm_bindgen::prelude::*;

    /// `analyzePlan(text, format) -> string` (JSON); throws with a
    /// visitor-facing message when the plan cannot be read.
    #[wasm_bindgen(js_name = analyzePlan)]
    pub fn analyze_plan(text: &str, format: &str) -> Result<String, JsError> {
        super::analyze(text, format)
            .map(|value| value.to_string())
            .map_err(|e| JsError::new(&e))
    }

    /// `detectPlanFormat(text) -> string`: a format id, or "" when unknown.
    #[wasm_bindgen(js_name = detectPlanFormat)]
    pub fn detect_plan_format(text: &str) -> String {
        match super::detect(&super::clean_input(text)) {
            super::Detected::Format(f) => f.id().to_string(),
            _ => String::new(),
        }
    }

    /// `samplePlans() -> string`: JSON array of the bundled real plans.
    #[wasm_bindgen(js_name = samplePlans)]
    pub fn sample_plans() -> String {
        super::samples_json().to_string()
    }

    /// Version of this wrapper crate, for bug reports.
    #[wasm_bindgen(js_name = viewerVersion)]
    pub fn viewer_version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

#[cfg(test)]
mod tests;
