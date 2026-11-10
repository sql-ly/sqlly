//! MySQL 8 / MariaDB — `EXPLAIN FORMAT=JSON`, `ANALYZE FORMAT=JSON`, and
//! MySQL 8 `EXPLAIN ANALYZE` (FORMAT=TREE text).

use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};
use crate::plans::warning::PlanWarning;
use serde_json::Value;

// ---------------------------------------------------------------------------
// MySQL / MariaDB — EXPLAIN FORMAT=JSON
// ---------------------------------------------------------------------------

/// Parse MySQL/MariaDB `EXPLAIN FORMAT=JSON` output into the shared model.
/// MariaDB `ANALYZE FORMAT=JSON` output is the same shape with `r_*`
/// runtime fields (`r_rows`, `r_loops`, `r_total_time_ms`, `r_filtered`)
/// added per table; when present they populate the actual-statistics side
/// of the node model.
pub fn parse_mysql_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    let value: Value = serde_json::from_str(doc.trim())
        .map_err(|e| PlanError::InvalidStructure(format!("EXPLAIN FORMAT=JSON: {e}")))?;
    let block = value.get("query_block").ok_or_else(|| {
        PlanError::InvalidStructure("EXPLAIN FORMAT=JSON: missing query_block".into())
    })?;

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let root_id = nodes.len() as i32 + 1;
    let mut root_output = Vec::new();
    // MariaDB ANALYZE FORMAT=JSON: total wall time of the whole block.
    if let Some(total_ms) = block.get("r_total_time_ms").map(number_or_string) {
        root_output.push(format!("Actual time: {total_ms} ms"));
    }
    nodes.push(PlanNode {
        id: root_id,
        logical_op: "Query Block".to_string(),
        physical_op: "Query Block".to_string(),
        estimated_rows: 0.0,
        estimated_cost: block
            .get("cost_info")
            .and_then(|c| c.get("query_cost"))
            .map(number_or_string)
            .unwrap_or(0.0),
        estimated_cpu: 0.0,
        estimated_io: 0.0,
        actual_rows: None,
        actual_rewinds: None,
        actual_rebinds: None,
        row_count: None,
        output_list: root_output,
        warnings: Vec::new(),
    });
    walk_mysql_value(block, root_id, &mut nodes, &mut edges);

    let total_subtree_cost = nodes.first().map(|n| n.estimated_cost).unwrap_or(0.0);
    let estimated_rows = nodes
        .iter()
        .skip(1)
        .map(|n| n.estimated_rows)
        .fold(0.0, f64::max);
    Ok(ExecutionPlan {
        statement_text: String::new(),
        statement_type: "EXPLAIN".to_string(),
        nodes,
        edges,
        total_subtree_cost,
        estimated_rows,
        warnings: Vec::new(),
    })
}

/// MySQL cost fields are strings ("0.35"); MariaDB sometimes emits numbers.
fn number_or_string(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Recursively find `table` operator objects under any wrapper key
/// (`nested_loop`, `ordering_operation`, subqueries, unions, …).
fn walk_mysql_value(
    value: &Value,
    parent: i32,
    nodes: &mut Vec<PlanNode>,
    edges: &mut Vec<PlanEdge>,
) {
    match value {
        Value::Object(map) => {
            if let Some(table) = map.get("table").filter(|t| t.is_object()) {
                let id = push_mysql_table_node(table, parent, nodes, edges);
                // Subqueries can hang off the table object itself.
                walk_mysql_value(table, id, nodes, edges);
            }
            for (key, child) in map {
                if key == "table" {
                    continue;
                }
                walk_mysql_value(child, parent, nodes, edges);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_mysql_value(item, parent, nodes, edges);
            }
        }
        _ => {}
    }
}

fn push_mysql_table_node(
    table: &Value,
    parent: i32,
    nodes: &mut Vec<PlanNode>,
    edges: &mut Vec<PlanEdge>,
) -> i32 {
    let id = nodes.len() as i32 + 1;
    let name = table
        .get("table_name")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let access = table
        .get("access_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut warnings = Vec::new();
    // `ALL` is MySQL's full table scan.
    if access == "ALL" {
        warnings.push(PlanWarning::TableScan {
            table: name.to_string(),
            schema: String::new(),
        });
    }
    let mut output_list = vec![name.to_string()];
    if let Some(key) = table.get("key").and_then(Value::as_str) {
        output_list.push(format!("Key: {key}"));
    }
    if let Some(cond) = table.get("attached_condition").and_then(Value::as_str) {
        output_list.push(format!("Condition: {cond}"));
    }
    // MariaDB ANALYZE FORMAT=JSON runtime fields. `r_rows` is the AVERAGE
    // rows read per scan and `r_loops` how many times the table was
    // scanned, so the total actual row count is r_rows × r_loops — that
    // product goes into `actual_rows` to match SQL Server/PG semantics,
    // and the loop count is kept in `actual_rebinds` (the closest model
    // field: how many times the operator was re-driven). `r_rows` is JSON
    // null when the table was never accessed → actual_rows stays None.
    let r_loops = table.get("r_loops").map(number_or_string);
    let actual_rows = table
        .get("r_rows")
        .filter(|v| !v.is_null())
        .map(number_or_string)
        .map(|per_loop| per_loop * r_loops.unwrap_or(1.0));
    // Newer MariaDB splits per-table time into r_table_time_ms +
    // r_other_time_ms; older versions report r_total_time_ms.
    if let Some(time_ms) = table
        .get("r_total_time_ms")
        .or_else(|| table.get("r_table_time_ms"))
        .map(number_or_string)
    {
        output_list.push(format!("Actual time: {time_ms} ms"));
    }
    if let Some(filtered) = table
        .get("r_filtered")
        .filter(|v| !v.is_null())
        .map(number_or_string)
    {
        output_list.push(format!("Actual filtered: {filtered}%"));
    }
    nodes.push(PlanNode {
        id,
        logical_op: format!("Access: {}", if access.is_empty() { "?" } else { access }),
        physical_op: format!("Table: {name}"),
        // MySQL names this rows_examined_per_scan; MariaDB just `rows`.
        estimated_rows: table
            .get("rows_examined_per_scan")
            .or_else(|| table.get("rows"))
            .map(number_or_string)
            .unwrap_or(0.0),
        estimated_cost: table
            .get("cost_info")
            .and_then(|c| c.get("prefix_cost"))
            .map(number_or_string)
            .unwrap_or(0.0),
        estimated_cpu: 0.0,
        estimated_io: table
            .get("cost_info")
            .and_then(|c| c.get("read_cost"))
            .map(number_or_string)
            .unwrap_or(0.0),
        actual_rows,
        actual_rewinds: None,
        actual_rebinds: r_loops.map(|l| l as i32),
        row_count: None,
        output_list,
        warnings,
    });
    edges.push(PlanEdge {
        from_node: parent,
        to_node: id,
    });
    id
}

// ---------------------------------------------------------------------------
// MySQL 8 — EXPLAIN ANALYZE (FORMAT=TREE text)
// ---------------------------------------------------------------------------

/// Parse MySQL 8 `EXPLAIN ANALYZE` TREE output into the shared model. Lines
/// look like
///
/// ```text
/// -> Nested loop inner join  (cost=1.4 rows=2) (actual time=0.055..0.058 rows=2 loops=1)
///     -> Table scan on p  (cost=0.7 rows=2) (actual time=0.033..0.034 rows=2 loops=1)
/// ```
///
/// with indentation depth giving tree depth. The `rows` in the actual
/// section is MySQL's AVERAGE per loop, so the total actual row count is
/// rows × loops — that product goes into [`PlanNode::actual_rows`] to match
/// SQL Server/PG semantics, and the loop count is kept in
/// [`PlanNode::actual_rebinds`] (the closest model field: how many times
/// the operator was re-driven). Operators without an actual section
/// (`(never executed)`, or plain `EXPLAIN FORMAT=TREE` output) keep
/// `actual_rows` as `None`. The actual `time=a..b` range's `b` (total time
/// per loop, ms) is surfaced in the node's output list.
pub fn parse_mysql_explain_analyze_tree(doc: &str) -> PlanResult<ExecutionPlan> {
    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut edges: Vec<PlanEdge> = Vec::new();
    // Ancestors of the node being added, as (indent, node id).
    let mut stack: Vec<(usize, i32)> = Vec::new();
    for line in doc.lines() {
        let line = line.trim_end();
        let stripped = line.trim_start();
        // Tolerate blank lines and stray non-operator lines (headers, the
        // trailing newline the client appends, …).
        let Some(rest) = stripped.strip_prefix("-> ") else {
            continue;
        };
        let indent = line.len() - stripped.len();
        let id = nodes.len() as i32 + 1;

        // Split `rest` into operator name, `(cost=… rows=…)`, and
        // `(actual time=… rows=… loops=…)` sections. Either annotation may
        // be absent (MariaDB-less installs, hypergraph optimizer, never
        // executed operators).
        let cost_at = rest.find("(cost=");
        let actual_at = rest.rfind("(actual ");
        let name_end = [cost_at, actual_at, rest.rfind("(never executed)")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
        let name = rest[..name_end].trim();
        if name.is_empty() {
            return Err(PlanError::InvalidStructure(format!(
                "EXPLAIN ANALYZE: operator line without a name `{line}`"
            )));
        }
        let cost_section =
            cost_at.map(|at| &rest[at..actual_at.filter(|a| *a > at).unwrap_or(rest.len())]);
        let actual_section = actual_at.map(|at| &rest[at..]);

        let estimated_cost = cost_section
            .and_then(|s| tree_field_last(s, "cost="))
            .unwrap_or(0.0);
        let estimated_rows = cost_section
            .and_then(|s| tree_field_last(s, "rows="))
            .unwrap_or(0.0);
        let loops = actual_section.and_then(|s| tree_field_last(s, "loops="));
        let actual_rows = actual_section
            .and_then(|s| tree_field_last(s, "rows="))
            .map(|per_loop| per_loop * loops.unwrap_or(1.0));
        let mut output_list = Vec::new();
        if let Some(time_ms) = actual_section.and_then(|s| tree_field_last(s, "time=")) {
            output_list.push(format!("Actual time: {time_ms} ms"));
        }

        let mut warnings = Vec::new();
        // "Table scan on <t>" is MySQL TREE's full table scan. Reading back
        // the server's own `<temporary>` table (GROUP BY, DISTINCT, UNION)
        // is not a table the user can index.
        if let Some(table) = name
            .strip_prefix("Table scan on ")
            .filter(|table| !table.starts_with("<temporary>"))
        {
            warnings.push(PlanWarning::TableScan {
                table: table.split_whitespace().next().unwrap_or(table).to_string(),
                schema: String::new(),
            });
        }

        nodes.push(PlanNode {
            id,
            logical_op: name.to_string(),
            physical_op: name.to_string(),
            estimated_rows,
            estimated_cost,
            estimated_cpu: 0.0,
            estimated_io: 0.0,
            actual_rows,
            actual_rewinds: None,
            actual_rebinds: loops.map(|l| l as i32),
            row_count: None,
            output_list,
            warnings,
        });
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        if let Some((_, parent)) = stack.last() {
            edges.push(PlanEdge {
                from_node: *parent,
                to_node: id,
            });
        }
        stack.push((indent, id));
    }
    if nodes.is_empty() {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN ANALYZE: no `-> operator` lines found".into(),
        ));
    }
    let total_subtree_cost = nodes[0].estimated_cost;
    let estimated_rows = nodes[0].estimated_rows;
    Ok(ExecutionPlan {
        statement_text: String::new(),
        statement_type: "EXPLAIN ANALYZE".to_string(),
        nodes,
        edges,
        total_subtree_cost,
        estimated_rows,
        warnings: Vec::new(),
    })
}

/// Extract `key<value>` from a TREE annotation section and parse the value,
/// taking the LAST number of an `a..b` range (for `time=0.041..0.045`, the
/// total; `cost=` ranges from the hypergraph optimizer read the same way).
fn tree_field_last(section: &str, key: &str) -> Option<f64> {
    let start = section.find(key)? + key.len();
    let rest = &section[start..];
    let value = &rest[..rest.find([' ', ')']).unwrap_or(rest.len())];
    value.rsplit("..").next()?.parse().ok()
}

/// Real captured MySQL/MariaDB documents shared between this module's tests
/// and the dispatch-routing tests in the parent module.
#[cfg(test)]
pub(crate) mod fixtures {
    /// Real MySQL 8.4 output for `EXPLAIN FORMAT=JSON SELECT * FROM
    /// plan_demo WHERE name = 'x'`.
    pub(crate) const MYSQL_FULL_SCAN: &str = r#"{
  "query_block": {
    "select_id": 1,
    "cost_info": {
      "query_cost": "0.35"
    },
    "table": {
      "table_name": "plan_demo",
      "access_type": "ALL",
      "rows_examined_per_scan": 1,
      "rows_produced_per_join": 1,
      "filtered": "100.00",
      "cost_info": {
        "read_cost": "0.25",
        "eval_cost": "0.10",
        "prefix_cost": "0.35",
        "data_read_per_join": "16"
      },
      "used_columns": [
        "id",
        "name"
      ],
      "attached_condition": "(`appdb`.`plan_demo`.`name` = 'x')"
    }
  }
}"#;

    /// Representative MySQL 8.4 output for `EXPLAIN ANALYZE SELECT * FROM p
    /// JOIN c ON c.parent_id = p.id WHERE c.qty > 1` — a nested loop join
    /// driving an index lookup, with a filter node carrying fractional
    /// estimated rows.
    pub(crate) const MYSQL_ANALYZE_TREE: &str = "\
-> Nested loop inner join  (cost=1.4 rows=2) (actual time=0.0553..0.0576 rows=2 loops=1)
    -> Table scan on p  (cost=0.7 rows=2) (actual time=0.0329..0.0341 rows=2 loops=1)
    -> Filter: (c.qty > 1)  (cost=0.3 rows=0.667) (actual time=0.0104..0.0111 rows=1 loops=2)
        -> Index lookup on c using parent_id (parent_id=p.id)  (cost=0.3 rows=1) (actual time=0.00921..0.00983 rows=1 loops=2)
";

    /// Representative MariaDB 10.11 output for `ANALYZE FORMAT=JSON
    /// SELECT * FROM p JOIN c ON c.parent_id = p.id`: the EXPLAIN
    /// FORMAT=JSON shape plus per-table r_* runtime fields.
    pub(crate) const MARIADB_ANALYZE_JSON: &str = r#"{
  "query_block": {
    "select_id": 1,
    "r_loops": 1,
    "r_total_time_ms": 0.0554,
    "nested_loop": [
      {
        "table": {
          "table_name": "p",
          "access_type": "ALL",
          "r_loops": 1,
          "rows": 2,
          "r_rows": 2,
          "r_table_time_ms": 0.0177,
          "r_other_time_ms": 0.0212,
          "filtered": 100,
          "r_filtered": 100
        }
      },
      {
        "table": {
          "table_name": "c",
          "access_type": "ref",
          "possible_keys": ["parent_id"],
          "key": "parent_id",
          "key_length": "5",
          "used_key_parts": ["parent_id"],
          "ref": ["appdb.p.id"],
          "r_loops": 2,
          "rows": 1,
          "r_rows": 1,
          "r_table_time_ms": 0.0106,
          "r_other_time_ms": 0.008,
          "filtered": 100,
          "r_filtered": 100
        }
      }
    ]
  }
}"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::{MARIADB_ANALYZE_JSON, MYSQL_ANALYZE_TREE, MYSQL_FULL_SCAN};
    use super::*;
    use crate::plans::dialects::{PlanDialect, parse_captured_plan};
    use crate::plans::findings_for_plan;

    #[test]
    fn reading_back_a_temporary_table_is_not_a_table_scan() {
        let tree = "-> Sort: revenue DESC  (actual time=84.3..84.3 rows=5 loops=1)\n    -> Table scan on <temporary>  (actual time=84.3..84.3 rows=5 loops=1)\n        -> Aggregate using temporary table  (actual time=84.3..84.3 rows=5 loops=1)\n            -> Table scan on o  (cost=60294 rows=598688) (actual time=0.0888..53.9 rows=600000 loops=1)\n";
        let plan = parse_mysql_explain_analyze_tree(tree).unwrap();
        let scans: Vec<&str> = plan
            .nodes
            .iter()
            .flat_map(|n| n.warnings.iter())
            .filter_map(|w| match w {
                PlanWarning::TableScan { table, .. } => Some(table.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(scans, vec!["o"]);
    }

    #[test]
    fn mysql_full_scan_parses_and_reports_a_table_scan_finding() {
        let plan = parse_mysql_explain_json(MYSQL_FULL_SCAN).unwrap();
        assert_eq!(plan.nodes.len(), 2, "query block + table");
        assert_eq!(plan.total_subtree_cost, 0.35);
        assert_eq!(plan.nodes[1].estimated_rows, 1.0);
        let findings = findings_for_plan(&plan);
        assert!(
            findings.iter().any(|f| f.summary.contains("plan_demo")),
            "{findings:?}"
        );
    }

    #[test]
    fn mysql_nested_loop_join_yields_one_node_per_table() {
        let doc = r#"{"query_block": {"select_id": 1, "nested_loop": [
            {"table": {"table_name": "p", "access_type": "index", "key": "PRIMARY"}},
            {"table": {"table_name": "c", "access_type": "ref", "key": "parent_id"}}
        ]}}"#;
        let plan = parse_mysql_explain_json(doc).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        assert!(plan.nodes.iter().all(|n| n.warnings.is_empty()));
    }

    #[test]
    fn malformed_mysql_documents_yield_typed_errors_not_panics() {
        assert!(parse_mysql_explain_json("[1,2,3]").is_err());
    }

    #[test]
    fn mysql_explain_analyze_tree_parses_nested_loop_with_actuals() {
        let plan = parse_mysql_explain_analyze_tree(MYSQL_ANALYZE_TREE).unwrap();
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        let root = &plan.nodes[0];
        assert_eq!(root.physical_op, "Nested loop inner join");
        assert_eq!(root.estimated_cost, 1.4);
        assert_eq!(root.estimated_rows, 2.0);
        assert_eq!(root.actual_rows, Some(2.0));
        assert_eq!(root.actual_rebinds, Some(1));
        assert_eq!(root.output_list, vec!["Actual time: 0.0576 ms"]);
        assert_eq!(plan.total_subtree_cost, 1.4);
        assert_eq!(plan.estimated_rows, 2.0);
        // Depth from indentation: join → {table scan, filter}, filter → lookup.
        assert_eq!(
            plan.edges
                .iter()
                .map(|e| (e.from_node, e.to_node))
                .collect::<Vec<_>>(),
            vec![(1, 2), (1, 3), (3, 4)]
        );
        // The filter's fractional estimate survives.
        assert_eq!(plan.nodes[2].estimated_rows, 0.667);
        // Actual rows is the rows×loops TOTAL (rows is a per-loop average);
        // the loop count itself rides in actual_rebinds.
        let lookup = &plan.nodes[3];
        assert_eq!(lookup.actual_rows, Some(2.0));
        assert_eq!(lookup.actual_rebinds, Some(2));
        // The table scan surfaces as a findings-visible warning.
        let findings = findings_for_plan(&plan);
        assert!(
            findings.iter().any(|f| f.summary.contains("p ")),
            "{findings:?}"
        );
        assert_eq!(
            plan.nodes.iter().filter(|n| !n.warnings.is_empty()).count(),
            1
        );
    }

    #[test]
    fn mysql_tree_without_actual_sections_keeps_actual_rows_none() {
        // Plain EXPLAIN FORMAT=TREE output and never-executed operators
        // carry no actual section.
        let doc = "\
-> Nested loop inner join  (cost=1.4 rows=2) (actual time=0.05..0.06 rows=2 loops=1)
    -> Table scan on p  (cost=0.7 rows=2)
    -> Index lookup on c using parent_id (parent_id=p.id)  (cost=0.3 rows=1) (never executed)
";
        let plan = parse_mysql_explain_analyze_tree(doc).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.nodes[0].actual_rows, Some(2.0));
        assert_eq!(plan.nodes[1].actual_rows, None);
        assert_eq!(plan.nodes[1].estimated_cost, 0.7);
        assert_eq!(plan.nodes[2].actual_rows, None);
        assert_eq!(
            plan.nodes[2].physical_op.trim_end(),
            "Index lookup on c using parent_id (parent_id=p.id)"
        );
    }

    #[test]
    fn mysql_tree_with_no_operator_lines_is_a_typed_error() {
        assert!(parse_mysql_explain_analyze_tree("").is_err());
        assert!(parse_mysql_explain_analyze_tree("not a tree at all").is_err());
    }

    #[test]
    fn mariadb_analyze_json_populates_actual_statistics() {
        let plans = parse_captured_plan(PlanDialect::MariaDb, MARIADB_ANALYZE_JSON).unwrap();
        let plan = &plans[0];
        assert_eq!(plan.nodes.len(), 3, "query block + two tables");
        // Root carries the whole block's wall time.
        assert_eq!(plan.nodes[0].output_list, vec!["Actual time: 0.0554 ms"]);
        // p: 2 rows in 1 loop; MariaDB's `rows` maps to estimated_rows.
        let p = &plan.nodes[1];
        assert_eq!(p.estimated_rows, 2.0);
        assert_eq!(p.actual_rows, Some(2.0));
        assert_eq!(p.actual_rebinds, Some(1));
        assert!(p.output_list.iter().any(|o| o == "Actual time: 0.0177 ms"));
        assert!(p.output_list.iter().any(|o| o == "Actual filtered: 100%"));
        assert!(!p.warnings.is_empty(), "ALL access is a table scan");
        // c: r_rows is a per-loop average → actual_rows = r_rows × r_loops.
        let c = &plan.nodes[2];
        assert_eq!(c.actual_rows, Some(2.0));
        assert_eq!(c.actual_rebinds, Some(2));
    }

    #[test]
    fn mariadb_never_accessed_table_keeps_actual_rows_none() {
        // MariaDB emits r_rows: null when a table was never accessed.
        let doc = r#"{"query_block": {"select_id": 1, "r_total_time_ms": 0.01,
            "table": {"table_name": "t", "access_type": "ref", "key": "pk",
                      "rows": 1, "r_loops": 0, "r_rows": null, "r_filtered": null}}}"#;
        let plan = parse_mysql_explain_json(doc).unwrap();
        assert_eq!(plan.nodes[1].actual_rows, None);
        assert_eq!(plan.nodes[1].estimated_rows, 1.0);
    }

    #[test]
    fn mariadb_estimated_json_still_parses_without_r_fields() {
        // MariaDb estimated routes through the same JSON parser; without
        // ANALYZE there are no r_* fields and no actual statistics.
        let plans = parse_captured_plan(PlanDialect::MariaDb, MYSQL_FULL_SCAN).unwrap();
        assert_eq!(plans[0].nodes.len(), 2);
        assert!(plans[0].nodes.iter().all(|n| n.actual_rows.is_none()));
        assert!(plans[0].nodes.iter().all(|n| n.actual_rebinds.is_none()));
    }
}
