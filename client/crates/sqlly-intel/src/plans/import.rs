//! Reading a plan document someone pasted or opened, whatever produced it.
//!
//! The app's own captures know their engine. A plan pasted from the
//! clipboard, opened from a file, or dropped on the online viewer does not:
//! [`clean_plan_text`] removes what terminal clients wrap around it,
//! [`sniff_plan_document`] recognizes the engine, and
//! [`parse_plan_document`] runs the right parser. The desktop app's Open
//! Plan / Paste Plan and the web viewer share these, so both accept the
//! same documents.

use serde_json::Value;

use crate::plans::dialects::{
    PlanDialect, parse_captured_plan, parse_oracle_xplan_text, parse_sqlite_query_plan_any,
};
use crate::plans::error::PlanResult;
use crate::plans::model::ExecutionPlan;

/// Undo what clients wrap around a plan when it is copied from a terminal:
/// a byte-order mark, CRLF line ends, Markdown code fences, psql's aligned
/// output (`QUERY PLAN` header, dashes, ` +` continuations, `(1 row)`), and
/// the mysql client's `\G` row banner or table borders.
pub fn clean_plan_text(text: &str) -> String {
    let text = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();

    // psql aligned output: every line of the cell but the last ends in `+`.
    // (Border rows like `+-----+` end in `+` too; they are not continuations.)
    let continued = lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.ends_with('+') && !t.chars().all(|c| c == '-' || c == '+')
        })
        .count();
    if continued >= 2 {
        let mut out = Vec::new();
        for line in &lines {
            let t = line.trim();
            if t.eq_ignore_ascii_case("QUERY PLAN")
                || (!t.is_empty() && t.chars().all(|c| c == '-' || c == '+'))
                || is_row_count(t)
            {
                continue;
            }
            let body = line.trim_end();
            let body = body.strip_suffix('+').unwrap_or(body).trim_end();
            out.push(body.strip_prefix(' ').unwrap_or(body).to_string());
        }
        return out.join("\n").trim().to_string();
    }

    // mysql `\G`: `*** 1. row ***` then `EXPLAIN: <plan>`.
    lines.retain(|l| {
        let t = l.trim();
        !(t.starts_with("***") && t.ends_with("***") && t.contains("row"))
    });
    // mysql table mode: `+----+` borders, a `| EXPLAIN |` header, and the
    // cell's text with `| ` before its first line and ` |` after its last.
    let mysql_table = lines.iter().any(|l| {
        let t = l.trim();
        t.starts_with("| EXPLAIN") && t.ends_with('|') && t.len() < 80
    });
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let t = line.trim();
        if mysql_table {
            if t.starts_with('+') && t.chars().all(|c| c == '-' || c == '+') {
                continue;
            }
            if t.starts_with("| EXPLAIN") && t.ends_with('|') {
                continue;
            }
        }
        if is_row_count(t) {
            continue;
        }
        out.push(line.to_string());
    }
    if mysql_table {
        if let Some(first) = out.iter_mut().find(|l| !l.trim().is_empty())
            && let Some(rest) = first.trim_start().strip_prefix("| ")
        {
            *first = rest.to_string();
        }
        if let Some(last) = out.iter_mut().rev().find(|l| !l.trim().is_empty()) {
            let t = last.trim_end();
            *last = t.strip_suffix('|').unwrap_or(t).trim_end().to_string();
        }
    }
    let mut joined = out.join("\n").trim().to_string();
    if let Some(rest) = joined.strip_prefix("EXPLAIN:") {
        joined = rest.trim_start().to_string();
    }
    joined
}

fn is_row_count(t: &str) -> bool {
    // `(1 row)`, `(2 rows)`, `1 row in set (0.01 sec)`
    (t.starts_with('(') && (t.ends_with(" row)") || t.ends_with(" rows)")))
        || t.contains(" row in set")
        || t.contains(" rows in set")
}

/// What a pasted or opened plan document is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanSniff {
    /// A document [`parse_plan_document`] reads for this engine.
    Dialect(PlanDialect),
    /// A recognizable plan in a form the parsers do not read (PostgreSQL's
    /// text EXPLAIN, SQL Server's text showplan, MySQL's version 2 JSON);
    /// the English reason says how to capture a readable one.
    Unsupported(String),
    Unknown,
}

/// Recognize the engine that produced `text` (already [`clean_plan_text`]ed).
/// Shapes are checked most specific first, so ClickHouse's PostgreSQL-like
/// JSON and MariaDB's MySQL-like JSON are told apart.
pub fn sniff_plan_document(text: &str) -> PlanSniff {
    let t = text.trim();
    if t.is_empty() {
        return PlanSniff::Unknown;
    }
    if t.contains("<ShowPlanXML") || (t.starts_with('<') && t.contains("StmtSimple")) {
        return PlanSniff::Dialect(PlanDialect::SqlServer);
    }
    if t.starts_with('[') || t.starts_with('{') {
        return match serde_json::from_str::<Value>(t) {
            Ok(value) => sniff_json(&value),
            Err(_) => PlanSniff::Unknown,
        };
    }
    let first = t.lines().next().unwrap_or("").trim();
    // PostgreSQL's default text EXPLAIN: `Seq Scan on t  (cost=0.00..35.50 rows=2550 width=4)`.
    if t.contains("(cost=") && t.contains("width=") && t.contains("..") {
        return PlanSniff::Unsupported(
            "This looks like PostgreSQL's text EXPLAIN output. SQLly reads the JSON form: run EXPLAIN (ANALYZE, FORMAT JSON) or EXPLAIN (FORMAT JSON) and paste that.".into(),
        );
    }
    // SQL Server SHOWPLAN_TEXT / STATISTICS PROFILE.
    if t.contains("OBJECT:(") || first.starts_with("StmtText") {
        return PlanSniff::Unsupported(
            "This looks like SQL Server's text showplan. SQLly reads ShowPlan XML: use SET STATISTICS XML ON (or SET SHOWPLAN_XML ON for an estimated plan), or save the plan from SSMS with Save Execution Plan As.".into(),
        );
    }
    if first.starts_with("->") {
        return PlanSniff::Dialect(PlanDialect::MySql);
    }
    if t.lines().any(|l| {
        let l = l.trim();
        l.starts_with('|') && l.contains(" Id ") && l.contains("Operation")
    }) {
        return PlanSniff::Dialect(PlanDialect::Oracle);
    }
    let rows: Vec<Vec<&str>> = t
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split('\t').collect())
        .collect();
    if rows.iter().all(|f| f.len() >= 13)
        && rows
            .iter()
            .all(|f| f[0].trim().parse::<i64>().is_ok() && f[1].trim().parse::<i64>().is_ok())
    {
        return PlanSniff::Dialect(PlanDialect::Oracle);
    }
    let sqlite_tree = first.eq_ignore_ascii_case("QUERY PLAN")
        || t.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("|--") || l.starts_with("`--")
        });
    if sqlite_tree {
        return PlanSniff::Dialect(PlanDialect::Sqlite);
    }
    let numbered = t.lines().filter(|l| !l.trim().is_empty()).all(|line| {
        let sep = if line.contains('\t') { '\t' } else { '|' };
        let fields: Vec<&str> = line.split(sep).collect();
        fields.len() >= 3
            && ((fields[0].trim().parse::<i64>().is_ok()
                && fields[1].trim().parse::<i64>().is_ok())
                || fields[0].trim().eq_ignore_ascii_case("id"))
    });
    if numbered {
        return PlanSniff::Dialect(PlanDialect::Sqlite);
    }
    PlanSniff::Unknown
}

fn sniff_json(value: &Value) -> PlanSniff {
    if let Some(block) = value.get("query_block") {
        let mariadb = value.get("query_optimization").is_some()
            || block.get("cost").is_some_and(Value::is_number)
            || block
                .as_object()
                .is_some_and(|m| m.keys().any(|k| k.starts_with("r_")));
        return PlanSniff::Dialect(if mariadb {
            PlanDialect::MariaDb
        } else {
            PlanDialect::MySql
        });
    }
    if value.get("query").is_some() && value.get("inputs").is_some() {
        return PlanSniff::Unsupported(
            "This is MySQL's version 2 JSON EXPLAIN format. SQLly reads version 1: run SET explain_json_format_version = 1 before EXPLAIN FORMAT=JSON, or paste EXPLAIN ANALYZE output instead.".into(),
        );
    }
    let plan = value
        .get(0)
        .and_then(|first| first.get("Plan"))
        .or_else(|| value.get("Plan"));
    if let Some(plan) = plan {
        let postgres = [
            "Parallel Aware",
            "Total Cost",
            "Startup Cost",
            "Plan Rows",
            "Actual Rows",
        ]
        .iter()
        .any(|key| plan.get(key).is_some());
        let clickhouse = ["Node Id", "Indexes", "Description"]
            .iter()
            .any(|key| plan.get(key).is_some());
        return PlanSniff::Dialect(if postgres || !clickhouse {
            PlanDialect::Postgres
        } else {
            PlanDialect::ClickHouse
        });
    }
    let duck_keys = |v: &Value| {
        ["extra_info", "operator_type", "operator", "children"]
            .iter()
            .any(|key| v.get(key).is_some())
    };
    let duck = match value {
        Value::Array(items) => !items.is_empty() && items.iter().all(duck_keys),
        Value::Object(_) => duck_keys(value),
        _ => false,
    };
    if duck {
        return PlanSniff::Dialect(PlanDialect::DuckDb);
    }
    PlanSniff::Unknown
}

/// Parse a document [`sniff_plan_document`] recognized (or the caller
/// chose) as `dialect`. Beyond [`parse_captured_plan`] — the app's own
/// capture formats — this reads what people copy out of other tools: a lone
/// PostgreSQL plan object without its array, the `sqlite3` shell's tree and
/// four-column rows, and Oracle's `DBMS_XPLAN` text.
pub fn parse_plan_document(dialect: PlanDialect, text: &str) -> PlanResult<Vec<ExecutionPlan>> {
    match dialect {
        PlanDialect::Postgres if text.trim().starts_with('{') => {
            parse_captured_plan(PlanDialect::Postgres, &format!("[{}]", text.trim()))
        }
        PlanDialect::Sqlite => parse_sqlite_query_plan_any(text).map(|p| vec![p]),
        PlanDialect::Oracle if text.lines().any(|l| l.trim_start().starts_with('|')) => {
            parse_oracle_xplan_text(text).map(|p| vec![p])
        }
        other => parse_captured_plan(other, text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_every_engine() {
        let cases: [(&str, PlanDialect); 10] = [
            (
                "<ShowPlanXML xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/showplan\">",
                PlanDialect::SqlServer,
            ),
            (
                "[{\"Plan\": {\"Node Type\": \"Result\"}}]",
                PlanDialect::Postgres,
            ),
            (
                "[{\"Plan\": {\"Node Type\": \"Expression\", \"Node Id\": \"Expression_2\"}}]",
                PlanDialect::ClickHouse,
            ),
            (
                "{\"query_block\": {\"select_id\": 1, \"cost_info\": {\"query_cost\": \"1.0\"}}}",
                PlanDialect::MySql,
            ),
            (
                "{\"query_optimization\": {}, \"query_block\": {\"select_id\": 1, \"cost\": 0.5}}",
                PlanDialect::MariaDb,
            ),
            ("-> Nested loop\n    -> Table scan on t", PlanDialect::MySql),
            ("QUERY PLAN\n`--SCAN t", PlanDialect::Sqlite),
            ("2|0|SCAN TABLE t", PlanDialect::Sqlite),
            (
                "{\"operator\": [], \"query\": \"select 1\"}",
                PlanDialect::DuckDb,
            ),
            (
                "| Id  | Operation         | Name |\n|   0 | SELECT STATEMENT  |      |",
                PlanDialect::Oracle,
            ),
        ];
        for (doc, dialect) in cases {
            assert_eq!(
                sniff_plan_document(doc),
                PlanSniff::Dialect(dialect),
                "{doc}"
            );
        }
    }

    #[test]
    fn unreadable_shapes_say_why_and_garbage_is_unknown() {
        assert!(matches!(
            sniff_plan_document("Seq Scan on t  (cost=0.00..1.50 rows=50 width=4)"),
            PlanSniff::Unsupported(why) if why.contains("FORMAT JSON")
        ));
        assert!(matches!(
            sniff_plan_document("  |--Table Scan(OBJECT:([db].[dbo].[t]))"),
            PlanSniff::Unsupported(_)
        ));
        for doc in ["hello world", "{}", "[{}]", ""] {
            assert_eq!(sniff_plan_document(doc), PlanSniff::Unknown, "{doc}");
        }
    }

    #[test]
    fn clean_unwraps_psql_and_mysql_output() {
        let psql = " QUERY PLAN \n------------\n [         +\n   {\"Plan\": {\"Node Type\": \"Result\"}}+\n ]\n(1 row)\n";
        assert_eq!(
            clean_plan_text(psql),
            "[\n  {\"Plan\": {\"Node Type\": \"Result\"}}\n]"
        );
        let mysql = "+---------+\n| EXPLAIN |\n+---------+\n| -> Table scan on t  (cost=1 rows=2)\n |\n+---------+\n1 row in set (0.00 sec)";
        assert_eq!(
            clean_plan_text(mysql),
            "-> Table scan on t  (cost=1 rows=2)"
        );
        let banner = "*************************** 1. row ***************************\nEXPLAIN: {\"query_block\": {}}";
        assert_eq!(clean_plan_text(banner), "{\"query_block\": {}}");
    }

    #[test]
    fn a_lone_postgres_plan_object_parses() {
        let plans = parse_plan_document(
            PlanDialect::Postgres,
            "{\"Plan\": {\"Node Type\": \"Seq Scan\", \"Relation Name\": \"t\", \"Total Cost\": 1.0, \"Plan Rows\": 5}}",
        )
        .unwrap();
        assert_eq!(plans[0].nodes[0].physical_op, "Seq Scan");
    }
}
