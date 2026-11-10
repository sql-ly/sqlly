//! Oracle — `EXPLAIN PLAN` into `PLAN_TABLE`.
//!
//! Capture is a three-statement script: `EXPLAIN PLAN SET STATEMENT_ID … FOR
//! <sql>` fills the session-private `PLAN_TABLE` without running the
//! statement, a `SELECT` renders each plan row as one tab-separated line,
//! and the newest `plan_id` for the statement id is what gets read back (no
//! `DELETE` is needed, so the capture stays a read plus an optimizer call).
//! The line format is the plan document this module parses:
//!
//! `id ⇥ parent_id ⇥ operation ⇥ options ⇥ object_owner ⇥ object_name ⇥
//! cardinality ⇥ cost ⇥ bytes ⇥ cpu_cost ⇥ io_cost ⇥ access_predicates ⇥
//! filter_predicates`
//!
//! with `-1` for a NULL parent and empty fields for NULL text. An ACTUAL
//! capture ([`actual_capture_sql`]) appends `last_output_rows ⇥
//! last_starts ⇥ last_elapsed_us` from `V$SQL_PLAN_STATISTICS_ALL`.

use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};
use crate::plans::warning::PlanWarning;

/// The statement id every SQLLY capture uses; the newest plan wins.
pub const STATEMENT_ID: &str = "SQLLY_EXPLAIN";

/// Field separator inside one plan line.
pub const FIELD_SEPARATOR: char = '\t';

/// The capture script for `sql`: explain, then read the rows back as lines.
pub fn capture_sql(sql: &str) -> String {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    format!(
        "EXPLAIN PLAN SET STATEMENT_ID = '{STATEMENT_ID}' FOR {sql};\n{}",
        plan_rows_sql()
    )
}

/// Marker comment appended to a statement run for an actual plan, so its
/// cursor can be found again in `V$SQL`. Deterministic per statement text
/// (the newest matching cursor wins), which keeps the capture SQL pure.
pub fn actual_marker(sql: &str) -> String {
    // FNV-1a over the trimmed statement.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in sql.trim().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("SQLLY_ACTUAL_{hash:016x}")
}

/// The ACTUAL-plan capture script: the statement really runs. Row-source
/// statistics are switched on for the session (`STATISTICS_LEVEL = ALL`,
/// the session-wide form of `/*+ GATHER_PLAN_STATISTICS */`), the tagged
/// statement is executed through `DBMS_SQL` — parsed, executed, and for a
/// query fetched to the end so every row source reports its real row
/// counts — then the cursor's plan is read from
/// `V$SQL_PLAN_STATISTICS_ALL` in the same tab-separated line format as
/// [`plan_rows_sql`], with three extra trailing fields: `last_output_rows`,
/// `last_starts`, `last_elapsed_time` (microseconds). The wrapping PL/SQL
/// block carries the marker too and is skipped by command type (47 =
/// PL/SQL EXECUTE). Needs SELECT on `V_$SQL` / `V_$SQL_PLAN_STATISTICS_ALL`
/// (SELECT_CATALOG_ROLE).
pub fn actual_capture_sql(sql: &str) -> String {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    let marker = actual_marker(sql);
    let is_query = sqlly_intel::query::safety::classify_select_only(sql).select_only;
    let tagged = format!("{sql} /* {marker} */").replace('\'', "''");
    let fetch_loop = if is_query {
        "  LOOP EXIT WHEN DBMS_SQL.FETCH_ROWS(l_cur) = 0; END LOOP;\n"
    } else {
        ""
    };
    format!(
        "ALTER SESSION SET STATISTICS_LEVEL = ALL;\n\
         DECLARE\n\
         \x20 l_cur INTEGER := DBMS_SQL.OPEN_CURSOR;\n\
         \x20 l_rows INTEGER;\n\
         BEGIN\n\
         \x20 DBMS_SQL.PARSE(l_cur, '{tagged}', DBMS_SQL.NATIVE);\n\
         \x20 l_rows := DBMS_SQL.EXECUTE(l_cur);\n\
         {fetch_loop}\
         \x20 DBMS_SQL.CLOSE_CURSOR(l_cur);\n\
         EXCEPTION WHEN OTHERS THEN\n\
         \x20 IF DBMS_SQL.IS_OPEN(l_cur) THEN DBMS_SQL.CLOSE_CURSOR(l_cur); END IF;\n\
         \x20 RAISE;\n\
         END;\n\
         /\n\
         ALTER SESSION SET STATISTICS_LEVEL = TYPICAL;\n\
         {}",
        actual_plan_rows_sql(&marker)
    )
}

/// Renders the newest tagged cursor's plan with its row-source statistics
/// as tab-separated lines (the estimated fields, then actual rows, starts
/// and elapsed microseconds).
pub fn actual_plan_rows_sql(marker: &str) -> String {
    format!(
        "SELECT TO_CHAR(p.id) || CHR(9) || TO_CHAR(NVL(p.parent_id, -1)) || CHR(9) || p.operation || CHR(9) || NVL(p.options, '') || CHR(9) || NVL(p.object_owner, '') || CHR(9) || NVL(p.object_name, '') || CHR(9) || TO_CHAR(NVL(p.cardinality, 0)) || CHR(9) || TO_CHAR(NVL(p.cost, 0)) || CHR(9) || TO_CHAR(NVL(p.bytes, 0)) || CHR(9) || TO_CHAR(NVL(p.cpu_cost, 0)) || CHR(9) || TO_CHAR(NVL(p.io_cost, 0)) || CHR(9) || REPLACE(REPLACE(NVL(p.access_predicates, ''), CHR(9), ' '), CHR(10), ' ') || CHR(9) || REPLACE(REPLACE(NVL(p.filter_predicates, ''), CHR(9), ' '), CHR(10), ' ') || CHR(9) || TO_CHAR(NVL(p.last_output_rows, 0)) || CHR(9) || TO_CHAR(NVL(p.last_starts, 0)) || CHR(9) || TO_CHAR(NVL(p.last_elapsed_time, 0)) AS plan_line FROM v$sql_plan_statistics_all p WHERE (p.sql_id, p.child_number) = (SELECT sql_id, child_number FROM (SELECT s.sql_id, s.child_number FROM v$sql s WHERE s.sql_text LIKE '%{marker}%' AND s.sql_text NOT LIKE '%v$sql%' AND s.command_type <> 47 ORDER BY s.last_active_time DESC, s.child_number DESC) WHERE ROWNUM = 1) ORDER BY p.id"
    )
}

/// Renders the newest plan for [`STATEMENT_ID`] as tab-separated lines.
pub fn plan_rows_sql() -> String {
    format!(
        "SELECT TO_CHAR(p.id) || CHR(9) || TO_CHAR(NVL(p.parent_id, -1)) || CHR(9) || p.operation || CHR(9) || NVL(p.options, '') || CHR(9) || NVL(p.object_owner, '') || CHR(9) || NVL(p.object_name, '') || CHR(9) || TO_CHAR(NVL(p.cardinality, 0)) || CHR(9) || TO_CHAR(NVL(p.cost, 0)) || CHR(9) || TO_CHAR(NVL(p.bytes, 0)) || CHR(9) || TO_CHAR(NVL(p.cpu_cost, 0)) || CHR(9) || TO_CHAR(NVL(p.io_cost, 0)) || CHR(9) || REPLACE(REPLACE(NVL(p.access_predicates, ''), CHR(9), ' '), CHR(10), ' ') || CHR(9) || REPLACE(REPLACE(NVL(p.filter_predicates, ''), CHR(9), ' '), CHR(10), ' ') AS plan_line FROM PLAN_TABLE p WHERE p.statement_id = '{STATEMENT_ID}' AND p.plan_id = (SELECT MAX(q.plan_id) FROM PLAN_TABLE q WHERE q.statement_id = '{STATEMENT_ID}') ORDER BY p.id"
    )
}

/// Parse the tab-separated `PLAN_TABLE` lines into a plan.
pub fn parse_oracle_plan_lines(doc: &str) -> PlanResult<ExecutionPlan> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut saw_actual = false;
    for line in doc.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
        if fields.len() < 3 {
            return Err(PlanError::InvalidStructure(format!(
                "EXPLAIN PLAN: malformed line `{line}`"
            )));
        }
        let field = |i: usize| fields.get(i).map(|s| s.trim()).unwrap_or("");
        let number = |i: usize| field(i).parse::<f64>().unwrap_or(0.0);
        let id: i64 = field(0).parse().map_err(|_| {
            PlanError::InvalidStructure(format!("EXPLAIN PLAN: bad id in `{line}`"))
        })?;
        let parent: i64 = field(1).parse().map_err(|_| {
            PlanError::InvalidStructure(format!("EXPLAIN PLAN: bad parent in `{line}`"))
        })?;
        let operation = field(2);
        let options = field(3);
        let owner = field(4);
        let object = field(5);
        let access = field(11);
        let filter = field(12);
        // ACTUAL captures carry three more fields.
        let has_actual = fields.len() >= 16;
        let actual_rows = has_actual.then(|| number(13));
        let starts = has_actual.then(|| number(14) as i32);
        let elapsed_us = has_actual.then(|| number(15));
        saw_actual |= has_actual;

        let mut physical_op = operation.to_string();
        if !options.is_empty() {
            physical_op.push(' ');
            physical_op.push_str(options);
        }
        if !object.is_empty() {
            physical_op.push(' ');
            if !owner.is_empty() {
                physical_op.push_str(owner);
                physical_op.push('.');
            }
            physical_op.push_str(object);
        }
        let mut output_list = Vec::new();
        if !access.is_empty() {
            output_list.push(format!("access: {access}"));
        }
        if !filter.is_empty() {
            output_list.push(format!("filter: {filter}"));
        }
        if let (Some(rows), Some(starts), Some(elapsed)) = (actual_rows, starts, elapsed_us) {
            output_list.push(format!(
                "actual: {rows} rows, {starts} starts, {:.3} ms",
                elapsed / 1000.0
            ));
        }
        let mut warnings = Vec::new();
        if operation == "TABLE ACCESS" && options == "FULL" {
            warnings.push(PlanWarning::TableScan {
                table: object.to_string(),
                schema: owner.to_string(),
            });
        }
        nodes.push(PlanNode {
            id: id as i32,
            logical_op: operation.to_string(),
            physical_op,
            estimated_rows: number(6),
            estimated_cost: number(7),
            estimated_cpu: number(9),
            estimated_io: number(10),
            actual_rows,
            actual_rewinds: None,
            actual_rebinds: starts,
            row_count: None,
            output_list,
            warnings,
        });
        if parent >= 0 {
            edges.push(PlanEdge {
                from_node: parent as i32,
                to_node: id as i32,
            });
        }
    }
    if nodes.is_empty() {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN PLAN: no PLAN_TABLE rows".into(),
        ));
    }
    let root = nodes
        .iter()
        .find(|n| n.id == 0)
        .or_else(|| nodes.first())
        .expect("non-empty");
    Ok(ExecutionPlan {
        statement_text: String::new(),
        statement_type: if saw_actual {
            "ACTUAL PLAN".to_string()
        } else {
            "EXPLAIN PLAN".to_string()
        },
        total_subtree_cost: root.estimated_cost,
        estimated_rows: root.estimated_rows,
        nodes,
        edges,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// `SELECT e.* FROM emp e JOIN dept d ON d.deptno = e.deptno WHERE
    /// d.dname = 'SALES'` on a schema with a primary key on dept.
    pub const ORACLE_JOIN: &str = "0\t-1\tSELECT STATEMENT\t\t\t\t3\t5\t210\t40000\t5\t\t\n1\t0\tHASH JOIN\t\t\t\t3\t5\t210\t40000\t5\t\"D\".\"DEPTNO\"=\"E\".\"DEPTNO\"\t\n2\t1\tTABLE ACCESS\tFULL\tHR\tDEPT\t1\t2\t20\t10000\t2\t\t\"D\".\"DNAME\"='SALES'\n3\t1\tTABLE ACCESS\tBY INDEX ROWID\tHR\tEMP\t14\t2\t700\t10000\t2\t\t\n4\t3\tINDEX\tRANGE SCAN\tHR\tEMP_DEPT_IX\t14\t1\t0\t7000\t1\t\"E\".\"DEPTNO\"=:B1\t";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::findings_for_plan;

    #[test]
    fn capture_sql_explains_then_reads_the_newest_plan() {
        let sql = capture_sql("SELECT 1 FROM DUAL;");
        assert!(sql.starts_with(
            "EXPLAIN PLAN SET STATEMENT_ID = 'SQLLY_EXPLAIN' FOR SELECT 1 FROM DUAL;\n"
        ));
        assert!(sql.contains("FROM PLAN_TABLE p"));
        assert!(sql.contains("MAX(q.plan_id)"));
        assert!(!sql.to_ascii_uppercase().contains("DELETE"));
        assert!(sql.contains("CHR(9)"));
    }

    #[test]
    fn plan_lines_build_a_tree_with_costs_and_full_scan_warnings() {
        let plan = parse_oracle_plan_lines(fixtures::ORACLE_JOIN).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN PLAN");
        assert_eq!(plan.nodes.len(), 5);
        assert_eq!(plan.edges.len(), 4);
        assert_eq!(plan.total_subtree_cost, 5.0);
        assert_eq!(plan.estimated_rows, 3.0);
        let root = &plan.nodes[0];
        assert_eq!(root.id, 0);
        assert_eq!(root.logical_op, "SELECT STATEMENT");
        let scan = &plan.nodes[2];
        assert_eq!(scan.physical_op, "TABLE ACCESS FULL HR.DEPT");
        assert_eq!(scan.estimated_rows, 1.0);
        assert_eq!(scan.estimated_cost, 2.0);
        assert_eq!(scan.estimated_cpu, 10000.0);
        assert_eq!(scan.estimated_io, 2.0);
        assert_eq!(scan.output_list, vec!["filter: \"D\".\"DNAME\"='SALES'"]);
        assert!(matches!(
            &scan.warnings[0],
            PlanWarning::TableScan { table, schema } if table == "DEPT" && schema == "HR"
        ));
        let index = &plan.nodes[4];
        assert_eq!(index.physical_op, "INDEX RANGE SCAN HR.EMP_DEPT_IX");
        assert!(index.warnings.is_empty());
        assert_eq!(index.output_list, vec!["access: \"E\".\"DEPTNO\"=:B1"]);
        assert!(
            plan.edges
                .iter()
                .any(|e| e.from_node == 1 && e.to_node == 2)
        );
        let findings = findings_for_plan(&plan);
        assert!(
            findings.iter().any(|f| f.summary.contains("DEPT")),
            "{findings:?}"
        );
    }

    /// The actual capture really runs the statement (fetching a query to
    /// the end), tags it so its cursor can be found, and reads the
    /// row-source statistics back; DML is executed but never fetched.
    #[test]
    fn actual_capture_runs_the_tagged_statement_and_reads_v_sql_plan_statistics() {
        let sql = actual_capture_sql("SELECT * FROM emp WHERE name = 'O''Hara';");
        let marker = actual_marker("SELECT * FROM emp WHERE name = 'O''Hara'");
        assert!(sql.starts_with("ALTER SESSION SET STATISTICS_LEVEL = ALL;\nDECLARE"));
        assert!(sql.contains(&format!(
            "DBMS_SQL.PARSE(l_cur, 'SELECT * FROM emp WHERE name = ''O''''Hara'' /* {marker} */', DBMS_SQL.NATIVE);"
        )));
        assert!(sql.contains("DBMS_SQL.FETCH_ROWS(l_cur)"));
        assert!(sql.contains("\nEND;\n/\nALTER SESSION SET STATISTICS_LEVEL = TYPICAL;\n"));
        assert!(sql.contains("FROM v$sql_plan_statistics_all p"));
        assert!(sql.contains(&format!("s.sql_text LIKE '%{marker}%'")));
        assert!(sql.contains("s.command_type <> 47"));
        assert!(sql.contains("last_output_rows"));
        assert_eq!(
            marker,
            actual_marker("  SELECT * FROM emp WHERE name = 'O''Hara'  ")
        );
        assert_ne!(marker, actual_marker("SELECT 2 FROM dual"));
        let dml = actual_capture_sql("UPDATE emp SET sal = sal + 1");
        assert!(!dml.contains("FETCH_ROWS"));
        assert!(dml.contains("l_rows := DBMS_SQL.EXECUTE(l_cur);"));
    }

    #[test]
    fn actual_lines_carry_row_source_statistics() {
        let doc = "0\t-1\tSELECT STATEMENT\t\t\t\t3\t5\t210\t40000\t5\t\t\t3\t1\t1500\n\
                   1\t0\tTABLE ACCESS\tFULL\tHR\tEMP\t14\t2\t700\t10000\t2\t\t\t12\t1\t980";
        let plan = parse_oracle_plan_lines(doc).unwrap();
        assert_eq!(plan.statement_type, "ACTUAL PLAN");
        assert_eq!(plan.nodes[0].actual_rows, Some(3.0));
        assert_eq!(plan.nodes[1].actual_rows, Some(12.0));
        assert_eq!(plan.nodes[1].actual_rebinds, Some(1));
        assert_eq!(plan.nodes[1].estimated_rows, 14.0);
        assert!(
            plan.nodes[1]
                .output_list
                .iter()
                .any(|line| line == "actual: 12 rows, 1 starts, 0.980 ms"),
            "{:?}",
            plan.nodes[1].output_list
        );
        // Estimated captures stay untouched.
        let estimated = parse_oracle_plan_lines(fixtures::ORACLE_JOIN).unwrap();
        assert_eq!(estimated.statement_type, "EXPLAIN PLAN");
        assert!(estimated.nodes.iter().all(|n| n.actual_rows.is_none()));
    }

    #[test]
    fn malformed_documents_yield_typed_errors_not_panics() {
        assert!(parse_oracle_plan_lines("").is_err());
        assert!(parse_oracle_plan_lines("no tabs here").is_err());
        assert!(parse_oracle_plan_lines("x\t-1\tSELECT STATEMENT").is_err());
        // Short rows still parse (missing trailing fields read as empty).
        let plan = parse_oracle_plan_lines("0\t-1\tSELECT STATEMENT").unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert!(plan.edges.is_empty());
    }
}
