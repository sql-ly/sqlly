//! SQLite — `EXPLAIN QUERY PLAN`.

use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};
use crate::plans::warning::PlanWarning;

/// Parse SQLite `EXPLAIN QUERY PLAN` rows rendered as `id|parent|detail`
/// lines (one per row, in result order).
pub fn parse_sqlite_query_plan_text(doc: &str) -> PlanResult<ExecutionPlan> {
    let mut rows = Vec::new();
    for line in doc.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '|');
        let (Some(id), Some(parent), Some(detail)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(PlanError::InvalidStructure(format!(
                "EXPLAIN QUERY PLAN: malformed line `{line}`"
            )));
        };
        let id: i64 = id.trim().parse().map_err(|_| {
            PlanError::InvalidStructure(format!("EXPLAIN QUERY PLAN: bad id in `{line}`"))
        })?;
        let parent: i64 = parent.trim().parse().map_err(|_| {
            PlanError::InvalidStructure(format!("EXPLAIN QUERY PLAN: bad parent in `{line}`"))
        })?;
        rows.push((id, parent, detail.trim().to_string()));
    }
    parse_sqlite_query_plan_rows(&rows)
}

/// Parse `EXPLAIN QUERY PLAN` output in whatever shape a person copied it:
///
/// - the `sqlite3` shell's tree (`QUERY PLAN`, then `|--`/`` `-- `` lines
///   indented three columns per level), which is what the shell prints in
///   every output mode;
/// - result rows with the raw four columns `id, parent, notused, detail`,
///   separated by `|` (the shell's list mode, most exports) or tabs (a grid
///   copied from a GUI), with or without a header row;
/// - the three-column `id|parent|detail` lines
///   [`parse_sqlite_query_plan_text`] reads.
pub fn parse_sqlite_query_plan_any(doc: &str) -> PlanResult<ExecutionPlan> {
    let lines: Vec<&str> = doc.lines().filter(|l| !l.trim().is_empty()).collect();
    let is_tree = lines.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("|--") || t.starts_with("`--")
    }) || lines
        .first()
        .is_some_and(|l| l.trim().eq_ignore_ascii_case("QUERY PLAN"));
    if is_tree {
        return parse_sqlite_query_plan_tree(&lines);
    }
    let mut rows = Vec::new();
    for line in lines {
        let separator = if line.contains('\t') { '\t' } else { '|' };
        let fields: Vec<&str> = line.split(separator).collect();
        if fields.len() < 3 {
            return Err(PlanError::InvalidStructure(format!(
                "EXPLAIN QUERY PLAN: malformed line `{}`",
                line.trim()
            )));
        }
        let (Ok(id), Ok(parent)) = (
            fields[0].trim().parse::<i64>(),
            fields[1].trim().parse::<i64>(),
        ) else {
            // A header row (`id|parent|notused|detail`) is skipped; any
            // other non-numeric row is not a plan row.
            if rows.is_empty() && fields[0].trim().eq_ignore_ascii_case("id") {
                continue;
            }
            return Err(PlanError::InvalidStructure(format!(
                "EXPLAIN QUERY PLAN: bad id or parent in `{}`",
                line.trim()
            )));
        };
        let detail_from = if fields.len() >= 4 && fields[2].trim().parse::<i64>().is_ok() {
            3
        } else {
            2
        };
        let detail = fields[detail_from..].join(&separator.to_string());
        rows.push((id, parent, detail.trim().to_string()));
    }
    parse_sqlite_query_plan_rows(&rows)
}

/// The `sqlite3` shell's tree: each step is `|--detail` or `` `--detail ``
/// after three columns of `|  ` / spaces per level of nesting.
fn parse_sqlite_query_plan_tree(lines: &[&str]) -> PlanResult<ExecutionPlan> {
    let mut rows = Vec::new();
    // ids[depth] = the id of the latest step seen at that depth.
    let mut ids: Vec<i64> = Vec::new();
    for line in lines {
        let line = line.trim_end();
        if line.trim().eq_ignore_ascii_case("QUERY PLAN") {
            continue;
        }
        let Some(marker) = line.find("|--").into_iter().chain(line.find("`--")).min() else {
            return Err(PlanError::InvalidStructure(format!(
                "EXPLAIN QUERY PLAN: malformed line `{}`",
                line.trim()
            )));
        };
        let depth = line[..marker].chars().count() / 3;
        let detail = line[marker + 3..].trim().to_string();
        let id = rows.len() as i64 + 1;
        let parent = if depth == 0 {
            0
        } else {
            *ids.get(depth - 1).ok_or_else(|| {
                PlanError::InvalidStructure(format!(
                    "EXPLAIN QUERY PLAN: `{}` is indented past its parent",
                    line.trim()
                ))
            })?
        };
        ids.truncate(depth);
        ids.push(id);
        rows.push((id, parent, detail));
    }
    parse_sqlite_query_plan_rows(&rows)
}

/// Parse SQLite `EXPLAIN QUERY PLAN` `(id, parent, detail)` rows.
pub fn parse_sqlite_query_plan_rows(rows: &[(i64, i64, String)]) -> PlanResult<ExecutionPlan> {
    if rows.is_empty() {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN QUERY PLAN: no rows".into(),
        ));
    }
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (id, parent, detail) in rows {
        let mut warnings = Vec::new();
        // `SCAN <table>` with no index is SQLite's full table scan;
        // `SEARCH` uses an index, and `SCAN x USING INDEX/COVERING INDEX`
        // walks an index rather than the table.
        if let Some(rest) = detail.strip_prefix("SCAN ")
            && !rest.contains("USING")
        {
            warnings.push(PlanWarning::TableScan {
                table: rest.split_whitespace().next().unwrap_or(rest).to_string(),
                schema: String::new(),
            });
        }
        nodes.push(PlanNode {
            id: *id as i32,
            logical_op: detail
                .split_whitespace()
                .next()
                .unwrap_or("STEP")
                .to_string(),
            physical_op: detail.clone(),
            estimated_rows: 0.0,
            estimated_cost: 0.0,
            estimated_cpu: 0.0,
            estimated_io: 0.0,
            actual_rows: None,
            actual_rewinds: None,
            actual_rebinds: None,
            row_count: None,
            output_list: Vec::new(),
            warnings,
        });
        if *parent != 0 {
            edges.push(PlanEdge {
                from_node: *parent as i32,
                to_node: *id as i32,
            });
        }
    }
    Ok(ExecutionPlan {
        statement_text: String::new(),
        statement_type: "EXPLAIN QUERY PLAN".to_string(),
        nodes,
        edges,
        total_subtree_cost: 0.0,
        estimated_rows: 0.0,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `sqlite3` 3.51 shell's output for a join with an IN subquery.
    const SHELL_TREE: &str = "QUERY PLAN\n|--SEARCH o USING INDEX idx_orders_date (order_date>?)\n|--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)\n|--LIST SUBQUERY 1\n|  |--SCAN customers\n|  `--CREATE BLOOM FILTER\n|--USE TEMP B-TREE FOR GROUP BY\n`--USE TEMP B-TREE FOR ORDER BY\n";

    #[test]
    fn shell_tree_nests_by_indentation() {
        let plan = parse_sqlite_query_plan_any(SHELL_TREE).unwrap();
        assert_eq!(plan.nodes.len(), 7);
        let edges: Vec<(i32, i32)> = plan
            .edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect();
        // Both subquery steps hang off LIST SUBQUERY 1 (node 3); the rest are roots.
        assert_eq!(edges, vec![(3, 4), (3, 5)]);
        assert_eq!(plan.nodes[3].physical_op, "SCAN customers");
        assert!(matches!(
            &plan.nodes[3].warnings[..],
            [PlanWarning::TableScan { table, .. }] if table == "customers"
        ));
        assert!(plan.nodes[0].warnings.is_empty());
    }

    #[test]
    fn shell_tree_rejects_a_step_indented_past_its_parent() {
        assert!(parse_sqlite_query_plan_any("QUERY PLAN\n|  `--SCAN t\n").is_err());
    }

    #[test]
    fn four_column_rows_skip_notused_and_a_header() {
        let doc = "id|parent|notused|detail\n9|0|205|SEARCH o USING INDEX idx_orders_date (order_date>?)\n19|0|0|LIST SUBQUERY 1\n22|19|216|SCAN customers\n";
        let plan = parse_sqlite_query_plan_any(doc).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.nodes[2].physical_op, "SCAN customers");
        assert_eq!(plan.edges.len(), 1);
        // Tab-separated grid copies read the same.
        let tabs = "9\t0\t205\tSEARCH o USING INDEX idx_orders_date (order_date>?)\n22\t0\t216\tSCAN customers\n";
        let plan = parse_sqlite_query_plan_any(tabs).unwrap();
        assert_eq!(plan.nodes[1].physical_op, "SCAN customers");
        // And the three-column form the app's capture produces.
        let plan = parse_sqlite_query_plan_any("2|0|SCAN t\n").unwrap();
        assert_eq!(plan.nodes[0].physical_op, "SCAN t");
    }
    use crate::plans::findings_for_plan;

    #[test]
    fn sqlite_query_plan_parses_scan_vs_search() {
        let doc = "3|0|SCAN t\n8|0|SEARCH u USING INTEGER PRIMARY KEY (rowid=?)";
        let plan = parse_sqlite_query_plan_text(doc).unwrap();
        assert_eq!(plan.nodes.len(), 2);
        let scans: Vec<_> = plan
            .nodes
            .iter()
            .filter(|n| !n.warnings.is_empty())
            .collect();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].physical_op, "SCAN t");
        let findings = findings_for_plan(&plan);
        assert!(
            findings.iter().any(|f| f.summary.contains("t ")),
            "{findings:?}"
        );
    }

    #[test]
    fn malformed_sqlite_documents_yield_typed_errors_not_panics() {
        assert!(parse_sqlite_query_plan_text("garbage-without-pipes").is_err());
        assert!(parse_sqlite_query_plan_text("").is_err());
    }
}
