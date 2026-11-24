//! Working out which engine and output format a cleaned plan document is.
//!
//! Each format has a marker no other format produces. XML and JSON are
//! decided by their root element and keys; the text formats by the strict
//! `looks_like_*` test their parser exports. A text document that passes
//! more than one test is reported with lower confidence.

use serde_json::Value;

use super::{PlanEngine, PlanFormat};
use crate::plans::dialects::{
    looks_like_clickhouse_explain_pipeline, looks_like_clickhouse_explain_text,
    looks_like_cockroach_explain_text, looks_like_duckdb_explain_text,
    looks_like_mysql_explain_table, looks_like_pg_explain_text,
    looks_like_pg_explain_text_without_costs, looks_like_pg_explain_xml,
    looks_like_pg_explain_yaml, looks_like_spark_explain_text, looks_like_sqlite_bytecode,
    looks_like_sqlite_query_plan, looks_like_sqlserver_showplan_text,
    looks_like_trino_explain_text,
};
use crate::plans::error::PlanImportError;

/// How sure [`detect_plan_format`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlanConfidence {
    /// The document has the format's general shape but none of its
    /// distinctive markers (a PostgreSQL text plan with `COSTS OFF`).
    Guess,
    /// The markers fit this format best, but the document is damaged or
    /// also fits another format.
    Likely,
    /// The document carries a marker only this format produces.
    Certain,
}

/// What [`detect_plan_format`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDetection {
    pub format: PlanFormat,
    pub confidence: PlanConfidence,
    /// The marker that decided it, in a few words.
    pub evidence: &'static str,
}

fn found(format: PlanFormat, confidence: PlanConfidence, evidence: &'static str) -> PlanDetection {
    PlanDetection {
        format,
        confidence,
        evidence,
    }
}

/// Recognize the format of `text` (already cleaned with
/// [`clean_plan_text`](super::clean_plan_text)). `None` when it is not a
/// plan in any format the importer reads.
pub fn detect_plan_format(text: &str) -> Option<PlanDetection> {
    use PlanConfidence::{Certain, Guess, Likely};
    let t = text.trim();
    if t.is_empty() {
        return None;
    }

    // XML.
    if t.contains("<ShowPlanXML") {
        return Some(found(
            PlanFormat::SqlServerXml,
            Certain,
            "ShowPlanXML root element",
        ));
    }
    if looks_like_pg_explain_xml(t) {
        return Some(found(
            PlanFormat::PostgresXml,
            Certain,
            "explain element in the PostgreSQL namespace",
        ));
    }
    if t.starts_with('<') && (t.contains("<StmtSimple") || t.contains("<RelOp ")) {
        return Some(found(
            PlanFormat::SqlServerXml,
            Likely,
            "ShowPlan elements without the ShowPlanXML root",
        ));
    }

    // JSON.
    if let Some(json) = json_start(t) {
        return match serde_json::Deserializer::from_str(json)
            .into_iter::<Value>()
            .next()
        {
            Some(Ok(value)) => detect_json(&value),
            _ => detect_broken_json(json),
        };
    }

    // Text. Every test that passes is collected; more than one is a
    // conflict, settled by this order (most distinctive marker first).
    type TextTest = (fn(&str) -> bool, PlanFormat, &'static str);
    let tests: [TextTest; 12] = [
        (
            looks_like_spark_explain_text,
            PlanFormat::SparkText,
            "== Physical Plan == section",
        ),
        (
            looks_like_cockroach_explain_text,
            PlanFormat::CockroachText,
            "bulleted operators with distribution and vectorized lines",
        ),
        (
            looks_like_duckdb_explain_text,
            PlanFormat::DuckDbText,
            "box-drawn operators",
        ),
        (
            looks_like_mysql_explain_table,
            PlanFormat::MySqlTable,
            "id, select_type, table, type, possible_keys, key, rows columns",
        ),
        (
            looks_like_oracle_plan,
            PlanFormat::OracleXplan,
            "Id, Operation, Name table",
        ),
        (
            looks_like_sqlserver_showplan_text,
            PlanFormat::SqlServerText,
            "StmtText column with |-- operator lines",
        ),
        (
            looks_like_trino_explain_text,
            PlanFormat::TrinoText,
            "Fragment headers or Layout and Estimates lines",
        ),
        (
            looks_like_pg_explain_yaml,
            PlanFormat::PostgresYaml,
            "- Plan: with Node Type keys",
        ),
        (
            looks_like_pg_explain_text,
            PlanFormat::PostgresText,
            "operator lines with (cost=a..b rows=N width=W)",
        ),
        (
            looks_like_mysql_tree,
            PlanFormat::MySqlTree,
            "-> operator lines",
        ),
        (
            looks_like_sqlite_query_plan,
            PlanFormat::SqliteQueryPlan,
            "SCAN and SEARCH steps",
        ),
        (
            looks_like_clickhouse_explain_pipeline,
            PlanFormat::ClickHousePipeline,
            "(Step) lines over Transform processors",
        ),
    ];
    let mut matches = tests
        .iter()
        .filter(|(test, _, _)| test(t))
        .map(|(_, format, evidence)| (*format, *evidence));
    if let Some((format, evidence)) = matches.next() {
        let contested = matches.next().is_some();
        let format = refine_text_format(format, t);
        let weak =
            format == PlanFormat::MySqlTree && !(t.contains("(cost=") || t.contains("(actual"));
        return Some(found(
            format,
            if contested || weak { Likely } else { Certain },
            evidence,
        ));
    }
    if looks_like_oracle_rows(t) {
        return Some(found(
            PlanFormat::OracleRows,
            Likely,
            "tab-separated PLAN_TABLE rows",
        ));
    }
    if looks_like_clickhouse_explain_text(t) {
        return Some(found(
            PlanFormat::ClickHouseText,
            Certain,
            "ClickHouse query plan step names",
        ));
    }
    if looks_like_pg_explain_text_without_costs(t) {
        return Some(found(
            PlanFormat::PostgresText,
            Guess,
            "PostgreSQL operator names without cost figures",
        ));
    }
    None
}

/// MariaDB's tabular ANALYZE adds `r_rows` and `r_filtered` columns.
fn refine_text_format(format: PlanFormat, text: &str) -> PlanFormat {
    if format == PlanFormat::MySqlTable && text.contains("r_rows") && text.contains("r_filtered") {
        return PlanFormat::MariaDbTable;
    }
    format
}

/// MySQL `FORMAT=TREE` and `EXPLAIN ANALYZE`: the first line is an operator
/// behind `-> `, and nested operators sit four spaces deeper each level.
fn looks_like_mysql_tree(text: &str) -> bool {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    let first = first.trim_start();
    if !first.starts_with("-> ") || first.starts_with("->  ") {
        return false;
    }
    // CockroachDB and Trino never start a line with `-> `; PostgreSQL
    // writes `->  ` with two spaces and never on the first line.
    lines.all(|line| {
        let t = line.trim_start();
        !t.starts_with("->  ")
    })
}

fn looks_like_oracle_plan(text: &str) -> bool {
    text.lines().take(400).any(|line| {
        let line = line.trim();
        line.starts_with('|')
            && line.contains(" Id ")
            && line.contains("Operation")
            && !line.contains("select_type")
    })
}

/// The app's own Oracle capture: tab-separated `PLAN_TABLE` rows, id and
/// parent id first, thirteen columns or more.
fn looks_like_oracle_rows(text: &str) -> bool {
    let mut rows = 0usize;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        // Trailing empty columns may have lost their tabs to a client that
        // trims lines, so the count is a floor, not the full thirteen.
        let fields: Vec<&str> = line.split('\t').collect();
        let operation = fields.get(2).map(|field| field.trim()).unwrap_or_default();
        if fields.len() < 7
            || fields[0].trim().parse::<i64>().is_err()
            || !(fields[1].trim().is_empty() || fields[1].trim().parse::<i64>().is_ok())
            || operation.is_empty()
            || !operation
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == ' ' || c == '-')
        {
            return false;
        }
        rows += 1;
    }
    rows > 0
}

/// Where a JSON document starts in `text`: at once, or under a few lines of
/// client decoration (DuckDB's `Physical Plan` title box).
fn json_start(text: &str) -> Option<&str> {
    let t = text.trim_start();
    if t.starts_with(['[', '{']) {
        return Some(t);
    }
    let mut offset = 0usize;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index > 12 {
            break;
        }
        let trimmed = line.trim();
        if index > 0
            && (trimmed == "["
                || trimmed == "{"
                || trimmed.starts_with("[{")
                || trimmed.starts_with("{\""))
        {
            // Only decoration may come before it: boxes, rules, a title.
            let before = &text[..offset];
            let decoration = before.lines().all(|l| {
                let l = l.trim();
                l.is_empty()
                    || l.chars().all(|c| !c.is_alphanumeric())
                    || l.chars().filter(|c| c.is_alphanumeric()).count() <= 24
            });
            return decoration.then(|| text[offset..].trim_start());
        }
        offset += line.len();
    }
    None
}

/// The JSON document in `text` without what surrounds it: decoration above
/// (see [`json_start`]) and anything after the value closes.
pub(super) fn json_slice(text: &str) -> &str {
    let Some(json) = json_start(text.trim()) else {
        return text;
    };
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    match stream.next() {
        Some(Ok(_)) => {
            let end = stream.byte_offset();
            json.get(..end).unwrap_or(json)
        }
        _ => json,
    }
}

/// The XML document in `text` without what surrounds it (result-grid
/// headers, messages, a second copy of the prompt).
pub(super) fn xml_slice(text: &str) -> &str {
    let start = ["<?xml", "<ShowPlanXML", "<explain"]
        .iter()
        .filter_map(|tag| text.find(tag))
        .min()
        .unwrap_or(0);
    let end = ["</ShowPlanXML>", "</explain>"]
        .iter()
        .filter_map(|tag| text.rfind(tag).map(|at| at + tag.len()))
        .max()
        .filter(|end| *end > start)
        .unwrap_or(text.len());
    text.get(start..end).unwrap_or(text)
}

fn has_any(value: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|key| value.get(key).is_some())
}

fn detect_json(value: &Value) -> Option<PlanDetection> {
    use PlanConfidence::{Certain, Likely};
    let first = match value {
        Value::Array(items) => items.first()?,
        other => other,
    };

    if let Some(block) = value.get("query_block") {
        let mariadb = value.get("query_optimization").is_some()
            || block.get("cost").is_some_and(Value::is_number)
            || json_has_key_prefix(block, "r_", 0);
        return Some(if mariadb {
            found(
                PlanFormat::MariaDbJson,
                Certain,
                "query_block with MariaDB cost or r_ run-time keys",
            )
        } else {
            found(PlanFormat::MySqlJson, Certain, "query_block root")
        });
    }
    // MySQL version 2: the 9.x shape nests the root under query_plan, the
    // 8.4 shape is the root node itself next to query and query_type.
    let v2 = value.get("query_plan").is_some_and(Value::is_object)
        || value.get("json_schema_version").is_some()
        || (value.get("query").is_some_and(Value::is_string)
            && has_any(value, &["inputs", "query_type", "access_type", "operation"])
            && value.get("operator").is_none());
    if v2 {
        return Some(found(
            PlanFormat::MySqlJsonV2,
            Certain,
            "query and operation keys of explain_json_format_version 2",
        ));
    }

    // SQLite `.mode json` over stored EXPLAIN QUERY PLAN rows.
    if value.is_array() && has_any(first, &["detail"]) && has_any(first, &["parent", "selectid"]) {
        return Some(found(
            PlanFormat::SqliteQueryPlan,
            Certain,
            "id, parent, detail rows of EXPLAIN QUERY PLAN",
        ));
    }

    let plan = first.get("Plan");
    if let Some(plan) = plan {
        let postgres = has_any(
            plan,
            &[
                "Parallel Aware",
                "Async Capable",
                "Total Cost",
                "Startup Cost",
                "Plan Rows",
                "Plan Width",
                "Actual Rows",
                "Actual Loops",
                "Relation Name",
            ],
        ) || has_any(first, &["Planning Time", "Execution Time", "Triggers"]);
        let clickhouse = has_any(plan, &["Node Id", "Indexes", "Description", "Actions"]);
        if postgres {
            return Some(found(
                PlanFormat::PostgresJson,
                Certain,
                "Plan object with PostgreSQL cost and row keys",
            ));
        }
        if clickhouse {
            return Some(found(
                PlanFormat::ClickHouseJson,
                Certain,
                "Plan object with ClickHouse Node Id keys",
            ));
        }
        if plan.get("Node Type").is_some() {
            // COSTS OFF PostgreSQL and ClickHouse without description both
            // reduce to Node Type and Plans. ClickHouse step names are
            // single CamelCase words, PostgreSQL's have spaces or are in a
            // short known list.
            let name = plan
                .get("Node Type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let clickhouse_name = [
                "Expression",
                "ReadFromMergeTree",
                "ReadFromStorage",
                "Aggregating",
                "Sorting",
                "Filter",
                "Join",
                "Union",
                "CreatingSets",
                "Distinct",
                "Limit",
                "LimitBy",
                "MergingAggregated",
                "ReadFromRemote",
                "ReadFromPreparedSource",
                "Window",
                "Offset",
                "Extremes",
                "Rollup",
                "Cube",
                "TotalsHaving",
                "FillingRightJoinSide",
                "ArrayJoin",
                "CreatingSet",
                "DelayedCreatingSets",
                "ReadFromSystemNumbers",
                "ReadFromMemoryStorage",
                "ReadNothing",
                "Projection",
                "ReadFromParallelRemoteReplicasStep",
                "MergingSorted",
            ]
            .contains(&name);
            return Some(if clickhouse_name {
                found(
                    PlanFormat::ClickHouseJson,
                    Likely,
                    "Plan object with a ClickHouse step name",
                )
            } else {
                found(
                    PlanFormat::PostgresJson,
                    Likely,
                    "Plan object with a Node Type",
                )
            });
        }
        return None;
    }
    if is_trino_json(first) {
        return None;
    }
    let duck_explain = |v: &Value| {
        v.get("children").is_some_and(Value::is_array)
            && has_any(v, &["extra_info", "name", "operator_type", "operator_name"])
    };
    let duck_profile = has_any(
        first,
        &[
            "operator_timing",
            "operator_cardinality",
            "cumulative_cardinality",
            "cumulative_rows_scanned",
            "rows_returned",
            "latency",
            "query_name",
        ],
    ) && first.get("children").is_some();
    let duck = match value {
        Value::Array(items) => !items.is_empty() && items.iter().all(duck_explain),
        Value::Object(_) => {
            duck_explain(value)
                || duck_profile
                || has_any(value, &["extra_info", "operator_type", "operator"])
        }
        _ => false,
    };
    if duck {
        return Some(found(
            PlanFormat::DuckDbJson,
            if duck_profile || first.get("extra_info").is_some() {
                Certain
            } else {
                Likely
            },
            "operator tree with children and extra_info",
        ));
    }
    None
}

/// Trino `EXPLAIN (FORMAT JSON)`: nodes carry `descriptor` and `outputs`,
/// and a distributed plan keys fragments by number.
fn is_trino_json(value: &Value) -> bool {
    let node = |v: &Value| v.get("descriptor").is_some() && v.get("outputs").is_some();
    node(value)
        || value
            .as_object()
            .is_some_and(|map| !map.is_empty() && map.values().all(node))
}

fn json_has_key_prefix(value: &Value, prefix: &str, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, v)| key.starts_with(prefix) || json_has_key_prefix(v, prefix, depth + 1)),
        Value::Array(items) => items
            .iter()
            .any(|v| json_has_key_prefix(v, prefix, depth + 1)),
        _ => false,
    }
}

/// JSON that does not parse (cut off, a stray character): name the format
/// by its key names so the parser's error can say where it broke.
fn detect_broken_json(text: &str) -> Option<PlanDetection> {
    use PlanConfidence::Likely;
    let has = |key: &str| text.contains(&format!("\"{key}\""));
    let format = if has("query_block") {
        if has("r_rows") || has("r_loops") || has("query_optimization") {
            PlanFormat::MariaDbJson
        } else {
            PlanFormat::MySqlJson
        }
    } else if has("query_plan")
        || has("json_schema_version")
        || (has("query_type") && has("operation"))
    {
        PlanFormat::MySqlJsonV2
    } else if has("Node Id") && has("Node Type") {
        PlanFormat::ClickHouseJson
    } else if has("Node Type") && (has("Plan") || has("Plans")) {
        PlanFormat::PostgresJson
    } else if has("extra_info") && has("children") {
        PlanFormat::DuckDbJson
    } else {
        return None;
    };
    Some(found(
        format,
        Likely,
        "key names of the format in JSON that does not parse",
    ))
}

/// A recognizable document that is not a plan the importer reads, with the
/// way to get one that is.
pub(super) fn unreadable_shape(text: &str) -> Option<PlanImportError> {
    let t = text.trim();
    let first = t.lines().next().unwrap_or_default().trim();
    let lower_first = first.to_ascii_lowercase();
    let not_a_plan = |what: &str, detail: &str, engine: Option<PlanEngine>, hint: Option<&str>| {
        Some(PlanImportError {
            format_id: None,
            format_label: Some(what.to_string()),
            line: None,
            column: None,
            detail: detail.to_string(),
            hint: hint
                .map(str::to_string)
                .or_else(|| engine.map(|engine| engine.capture_hint().to_string())),
        })
    };

    if looks_like_sqlite_bytecode(t) {
        return not_a_plan(
            "SQLite bytecode from plain EXPLAIN",
            "that is the virtual machine program, not the query plan",
            Some(PlanEngine::Sqlite),
            None,
        );
    }
    if t.contains("<deadlock") && (t.contains("<process-list") || t.contains("<victim-list")) {
        return not_a_plan(
            "a SQL Server deadlock graph",
            "a deadlock graph is not an execution plan",
            Some(PlanEngine::SqlServer),
            None,
        );
    }
    if t.contains("cockroachdb.github.io/distsqlplan") {
        return not_a_plan(
            "a CockroachDB EXPLAIN (DISTSQL) diagram link",
            "the plan is encoded in the URL for the browser diagram",
            Some(PlanEngine::CockroachDb),
            None,
        );
    }
    if first.starts_with("SelectWithUnionQuery") || first.starts_with("SelectQuery (children") {
        return not_a_plan(
            "ClickHouse EXPLAIN AST output",
            "that is the parsed syntax tree, not the query plan",
            Some(PlanEngine::ClickHouse),
            None,
        );
    }
    if first.starts_with("QUERY id:") {
        return not_a_plan(
            "ClickHouse EXPLAIN QUERY TREE output",
            "that is the analyzed query tree, not the query plan",
            Some(PlanEngine::ClickHouse),
            None,
        );
    }
    if lower_first.starts_with("digraph") {
        return not_a_plan(
            "a Graphviz graph",
            "the importer does not read plans drawn as Graphviz (ClickHouse EXPLAIN PIPELINE graph = 1, DuckDB FORMAT GRAPHVIZ)",
            None,
            Some("Use the engine's text or JSON EXPLAIN output instead."),
        );
    }
    if t.contains("Physical Plan") && t.contains('┌') {
        let after_title = t
            .lines()
            .skip_while(|l| !l.contains('└'))
            .nth(2)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if after_title.starts_with("digraph") {
            return not_a_plan(
                "DuckDB EXPLAIN (FORMAT GRAPHVIZ) output",
                "the importer does not read plans drawn as Graphviz",
                Some(PlanEngine::DuckDb),
                None,
            );
        }
        if after_title.starts_with("<!doctype") || after_title.starts_with("<html") {
            return not_a_plan(
                "DuckDB EXPLAIN (FORMAT HTML) output",
                "the importer does not read the HTML rendering",
                Some(PlanEngine::DuckDb),
                None,
            );
        }
    }
    if lower_first.starts_with("<!doctype html") || lower_first.starts_with("<html") {
        return not_a_plan(
            "an HTML page",
            "the importer reads the plan text or file, not a web page of it",
            None,
            Some("Copy the plan text itself, or save the plan to a file and open that."),
        );
    }
    if (t.starts_with('{') || t.starts_with('['))
        && let Ok(value) = serde_json::from_str::<Value>(t)
    {
        let first_value = match &value {
            Value::Array(items) => items.first().unwrap_or(&value),
            other => other,
        };
        if is_trino_json(first_value) {
            return not_a_plan(
                "Trino EXPLAIN (FORMAT JSON) output",
                "the importer reads Trino's text plans, not the JSON form",
                Some(PlanEngine::Trino),
                None,
            );
        }
    }
    if t.contains("Scan count") && t.contains("logical reads") && !t.contains("|--") {
        return not_a_plan(
            "SQL Server SET STATISTICS IO messages",
            "the messages came without the execution plan they belong to",
            Some(PlanEngine::SqlServer),
            None,
        );
    }
    let statement = [
        "select ", "with ", "insert ", "update ", "delete ", "merge ",
    ]
    .iter()
    .any(|word| lower_first.starts_with(word));
    if statement {
        return not_a_plan(
            "a SQL statement",
            "a statement is not its execution plan",
            None,
            Some(
                "Run it with EXPLAIN in front (or capture the plan in your database tool) and paste what comes back.",
            ),
        );
    }
    None
}

/// Find `line N` and `column M` (or a trailing `N:M`) in a parser's message
/// and translate them to the cleaned document the caller saw.
pub(super) fn error_position(
    format: PlanFormat,
    text: &str,
    detail: &str,
) -> (Option<usize>, Option<usize>) {
    let number_after = |word: &str| -> Option<usize> {
        let at = detail.find(word)? + word.len();
        let digits: String = detail[at..]
            .chars()
            .skip_while(|c| *c == ' ' || *c == ':')
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    };
    let mut line = number_after("line");
    let mut column = number_after("column");
    if line.is_none() {
        // roxmltree: `... at 12:34`.
        if let Some(at) = detail.rfind(" at ") {
            let mut parts = detail[at + 4..].trim().split(':');
            let l = parts.next().and_then(|p| p.trim().parse::<usize>().ok());
            let c = parts.next().and_then(|p| p.trim().parse::<usize>().ok());
            if l.is_some() && c.is_some() {
                line = l;
                column = c;
            }
        }
    }
    let Some(line) = line else {
        return (None, None);
    };
    // Parsers of JSON and XML see a slice of the document.
    let slice = match format {
        PlanFormat::PostgresJson
        | PlanFormat::MySqlJson
        | PlanFormat::MySqlJsonV2
        | PlanFormat::MariaDbJson
        | PlanFormat::DuckDbJson
        | PlanFormat::ClickHouseJson => json_slice(text),
        PlanFormat::SqlServerXml | PlanFormat::PostgresXml => xml_slice(text),
        _ => text,
    };
    let skipped = text
        .find(slice.lines().next().unwrap_or_default())
        .filter(|_| !slice.is_empty())
        .map(|offset| text[..offset].matches('\n').count())
        .unwrap_or(0);
    (Some(line + skipped), column.take())
}
