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
