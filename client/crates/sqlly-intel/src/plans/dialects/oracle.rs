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
    let is_query = crate::query::safety::classify_select_only(sql).select_only;
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

// ---------------------------------------------------------------------------
// DBMS_XPLAN text
// ---------------------------------------------------------------------------

/// Operations whose `PLAN_TABLE` row splits into `operation` + `options`.
/// DBMS_XPLAN prints the two joined ("TABLE ACCESS FULL"), so the longest
/// known operation prefix is the operation and the rest its options. Order
/// matters: longer prefixes first.
const XPLAN_OPERATIONS: &[&str] = &[
    "TABLE ACCESS",
    "MAT_VIEW ACCESS",
    "BITMAP INDEX",
    "BITMAP CONVERSION",
    "INDEX",
    "HASH JOIN",
    "HASH",
    "MERGE JOIN",
    "NESTED LOOPS",
    "SORT",
    "PARTITION RANGE",
    "PARTITION LIST",
    "PARTITION HASH",
    "PX BLOCK",
    "PX SEND",
    "PX RECEIVE",
    "JOIN FILTER",
    "WINDOW",
    "COUNT",
    "VIEW",
    "BUFFER",
    "LOAD",
];

/// Parse the text `DBMS_XPLAN.DISPLAY` / `DISPLAY_CURSOR` prints — the plan
/// people copy out of SQL*Plus, SQL Developer, or SQLcl — into a plan.
///
/// The operation table is read by its header (`Id | Operation | Name | Rows
/// | Bytes | Cost (%CPU)`, or `E-Rows` / `Starts` / `A-Rows` / `A-Time`
/// with `ALLSTATS LAST`), nesting comes from the Operation column's
/// indentation, and the Predicate Information section is matched back to
/// its operation ids. The rows are then read exactly like the app's own
/// `PLAN_TABLE` capture ([`parse_oracle_plan_lines`]), so the two forms
/// produce the same plan.
pub fn parse_oracle_xplan_text(doc: &str) -> PlanResult<ExecutionPlan> {
    let lines: Vec<&str> = doc.lines().collect();
    let header_at = lines
        .iter()
        .position(|line| {
            let cells = xplan_cells(line);
            cells.iter().any(|c| c.trim() == "Id") && cells.iter().any(|c| c.trim() == "Operation")
        })
        .ok_or_else(|| {
            PlanError::InvalidStructure(
                "DBMS_XPLAN: no `| Id | Operation |` header row found".into(),
            )
        })?;
    let header: Vec<String> = xplan_cells(lines[header_at])
        .iter()
        .map(|c| c.trim().to_string())
        .collect();
    let column = |names: &[&str]| header.iter().position(|h| names.contains(&h.as_str()));
    let (Some(id_col), Some(op_col)) = (column(&["Id"]), column(&["Operation"])) else {
        return Err(PlanError::InvalidStructure(
            "DBMS_XPLAN: header has no Id or Operation".into(),
        ));
    };
    let name_col = column(&["Name"]);
    let rows_col = column(&["Rows", "E-Rows"]);
    let bytes_col = column(&["Bytes", "E-Bytes"]);
    let cost_col = header.iter().position(|h| h.starts_with("Cost"));
    let starts_col = column(&["Starts"]);
    let actual_rows_col = column(&["A-Rows"]);
    let actual_time_col = column(&["A-Time"]);

    struct Step {
        id: i64,
        parent: i64,
        operation: String,
        options: String,
        object: String,
        rows: f64,
        cost: f64,
        bytes: f64,
        actual: Option<(f64, f64, f64)>,
    }
    let mut steps: Vec<Step> = Vec::new();
    // parents[depth] = id of the latest step at that depth.
    let mut parents: Vec<i64> = Vec::new();
    let mut end = header_at + 1;
    for (index, line) in lines.iter().enumerate().skip(header_at + 1) {
        let trimmed = line.trim();
        if trimmed.starts_with('-') && !steps.is_empty() {
            end = index;
            break;
        }
        end = index + 1;
        if !trimmed.starts_with('|') {
            continue;
        }
        let cells = xplan_cells(line);
        let Some(id) = cells
            .get(id_col)
            .and_then(|c| c.trim().trim_start_matches('*').trim().parse::<i64>().ok())
        else {
            continue;
        };
        let raw_op = cells.get(op_col).copied().unwrap_or_default();
        // One space of padding, then one more per level of nesting.
        let depth = raw_op.len() - raw_op.trim_start().len();
        let depth = depth.saturating_sub(1);
        let text = raw_op.trim();
        let (operation, options) = XPLAN_OPERATIONS
            .iter()
            .find(|op| text == **op || text.starts_with(&format!("{op} ")))
            .map(|op| (op.to_string(), text[op.len()..].trim().to_string()))
            .unwrap_or_else(|| (text.to_string(), String::new()));
        let parent = if depth == 0 {
            -1
        } else {
            parents
                .get(depth - 1)
                .copied()
                .or_else(|| parents.last().copied())
                .unwrap_or(-1)
        };
        parents.truncate(depth);
        parents.push(id);
        let cell = |col: Option<usize>| {
            col.and_then(|c| cells.get(c))
                .map(|c| c.trim())
                .unwrap_or("")
        };
        let actual = actual_rows_col.map(|_| {
            (
                xplan_number(cell(actual_rows_col)),
                xplan_number(cell(starts_col)),
                xplan_elapsed_us(cell(actual_time_col)),
            )
        });
        steps.push(Step {
            id,
            parent,
            operation,
            options,
            object: cell(name_col).to_string(),
            rows: xplan_number(cell(rows_col)),
            cost: xplan_number(cell(cost_col).split('(').next().unwrap_or("")),
            bytes: xplan_number(cell(bytes_col)),
            actual,
        });
    }
    if steps.is_empty() {
        return Err(PlanError::InvalidStructure(
            "DBMS_XPLAN: no operation rows".into(),
        ));
    }

    // `   4 - filter("O"."TOTAL">1200)`; long predicates wrap onto
    // further-indented lines, so text is collected first and each entry's
    // closing paren (the one `filter(` opened) dropped at the end.
    let mut predicates: Vec<(i64, bool, String)> = Vec::new();
    if let Some(start) = lines[end..]
        .iter()
        .position(|l| l.trim().starts_with("Predicate Information"))
    {
        for line in &lines[end + start + 1..] {
            let trimmed = line.trim();
            if trimmed.starts_with("---") {
                continue;
            }
            if trimmed.is_empty() {
                if predicates.is_empty() {
                    continue;
                }
                break;
            }
            let parsed = trimmed.split_once(" - ").and_then(|(id, rest)| {
                let id = id.trim().parse::<i64>().ok()?;
                let (kind, body) = rest.split_once('(')?;
                matches!(kind.trim(), "access" | "filter")
                    .then(|| (id, kind.trim() == "access", body.to_string()))
            });
            match (parsed, predicates.last_mut()) {
                (Some(entry), _) => predicates.push(entry),
                (None, Some((_, _, body))) => {
                    body.push(' ');
                    body.push_str(trimmed);
                }
                (None, None) => break,
            }
        }
    }
    let mut access: std::collections::HashMap<i64, String> = Default::default();
    let mut filter: std::collections::HashMap<i64, String> = Default::default();
    for (id, is_access, body) in predicates {
        let body = body.trim_end();
        let body = body.strip_suffix(')').unwrap_or(body);
        let map = if is_access { &mut access } else { &mut filter };
        let entry = map.entry(id).or_default();
        if !entry.is_empty() {
            entry.push_str(" AND ");
        }
        entry.push_str(body);
    }

    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    let plan_lines: Vec<String> = steps
        .iter()
        .map(|s| {
            let mut fields = vec![
                s.id.to_string(),
                s.parent.to_string(),
                clean(&s.operation),
                clean(&s.options),
                String::new(),
                clean(&s.object),
                s.rows.to_string(),
                s.cost.to_string(),
                s.bytes.to_string(),
                "0".into(),
                "0".into(),
                clean(access.get(&s.id).map(String::as_str).unwrap_or("")),
                clean(filter.get(&s.id).map(String::as_str).unwrap_or("")),
            ];
            if let Some((rows, starts, elapsed)) = s.actual {
                fields.extend([rows.to_string(), starts.to_string(), elapsed.to_string()]);
            }
            fields.join("\t")
        })
        .collect();
    let mut plan = parse_oracle_plan_lines(&plan_lines.join("\n"))?;
    plan.statement_text = xplan_statement_text(&lines[..header_at]);
    Ok(plan)
}

/// The cells of one `| a | b |` table row, without the outer bars.
fn xplan_cells(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    let Some(inner) = trimmed.strip_prefix('|') else {
        return Vec::new();
    };
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').collect()
}

/// `5422`, `137K`, `1744K`, `2M` — DBMS_XPLAN abbreviates large values.
fn xplan_number(cell: &str) -> f64 {
    let cell = cell.trim();
    let (digits, scale) = match cell.chars().last() {
        Some('K') => (&cell[..cell.len() - 1], 1e3),
        Some('M') => (&cell[..cell.len() - 1], 1e6),
        Some('G') => (&cell[..cell.len() - 1], 1e9),
        Some('T') => (&cell[..cell.len() - 1], 1e12),
        _ => (cell, 1.0),
    };
    digits
        .trim()
        .parse::<f64>()
        .map(|n| n * scale)
        .unwrap_or(0.0)
}

/// `00:00:00.01` (A-Time) as microseconds.
fn xplan_elapsed_us(cell: &str) -> f64 {
    let parts: Vec<f64> = cell
        .trim()
        .split(':')
        .map(|p| p.trim().parse::<f64>().unwrap_or(0.0))
        .collect();
    let seconds = parts.iter().fold(0.0, |acc, p| acc * 60.0 + p);
    seconds * 1_000_000.0
}

/// `DISPLAY_CURSOR` opens with `SQL_ID …`, a rule, and the statement text
/// wrapped over several lines.
fn xplan_statement_text(preamble: &[&str]) -> String {
    let Some(sql_id) = preamble
        .iter()
        .position(|l| l.trim_start().starts_with("SQL_ID"))
    else {
        return String::new();
    };
    preamble[sql_id + 1..]
        .iter()
        .map(|l| l.trim())
        .skip_while(|l| l.starts_with('-'))
        .take_while(|l| !l.is_empty() && !l.starts_with("Plan hash value"))
        .collect::<Vec<_>>()
        .join(" ")
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

#[cfg(test)]
mod xplan_tests {
    use super::*;
    use crate::plans::findings_for_plan;

    /// `SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)` after `EXPLAIN PLAN`, from
    /// Oracle Database 23ai Free in SQL*Plus.
    const DISPLAY: &str = r#"Plan hash value: 2958302572

----------------------------------------------------------------------------------
| Id  | Operation            | Name      | Rows  | Bytes | Cost (%CPU)| Time     |
----------------------------------------------------------------------------------
|   0 | SELECT STATEMENT     |           |     5 |   130 |   858   (1)| 00:00:01 |
|   1 |  SORT ORDER BY       |           |     5 |   130 |   858   (1)| 00:00:01 |
|   2 |   HASH GROUP BY      |           |     5 |   130 |   858   (1)| 00:00:01 |
|*  3 |    HASH JOIN         |           |  5422 |   137K|   855   (1)| 00:00:01 |
|*  4 |     TABLE ACCESS FULL| ORDERS    |  5422 | 97596 |   787   (1)| 00:00:01 |
|   5 |     TABLE ACCESS FULL| CUSTOMERS | 50000 |   390K|    68   (0)| 00:00:01 |
----------------------------------------------------------------------------------

Predicate Information (identified by operation id):
---------------------------------------------------

   3 - access("C"."ID"="O"."CUSTOMER_ID")
   4 - filter("O"."TOTAL">1200 AND "O"."STATUS"='refunded')

Note
-----
   - this is an adaptive plan
"#;

    /// `DBMS_XPLAN.DISPLAY_CURSOR(NULL, NULL, 'ALLSTATS LAST')` after the
    /// same query ran with `GATHER_PLAN_STATISTICS`.
    const ALLSTATS: &str = r#"SQL_ID  9ruaw8agctzdw, child number 1
-------------------------------------
SELECT /*+ GATHER_PLAN_STATISTICS */ c.country, COUNT(*) AS orders,
SUM(o.total) AS revenue FROM orders o JOIN customers c ON c.id =
o.customer_id WHERE o.status = 'refunded' AND o.total > 1200 GROUP BY
c.country ORDER BY revenue DESC

Plan hash value: 2958302572

-----------------------------------------------------------------------------------------------------------------------
| Id  | Operation            | Name      | Starts | E-Rows | A-Rows |   A-Time   | Buffers |  OMem |  1Mem | Used-Mem |
-----------------------------------------------------------------------------------------------------------------------
|   0 | SELECT STATEMENT     |           |      1 |        |      5 |00:00:00.01 |    3142 |       |       |          |
|   1 |  SORT ORDER BY       |           |      1 |      5 |      5 |00:00:00.01 |    3142 |  2048 |  2048 | 2048  (0)|
|   2 |   HASH GROUP BY      |           |      1 |      5 |      5 |00:00:00.01 |    3142 |  1116K|  1116K|          |
|*  3 |    HASH JOIN         |           |      1 |   5422 |   5409 |00:00:00.01 |    3142 |  1744K|  1744K| 1684K (0)|
|*  4 |     TABLE ACCESS FULL| ORDERS    |      1 |   5422 |   5409 |00:00:00.01 |    2895 |       |       |          |
|   5 |     TABLE ACCESS FULL| CUSTOMERS |      1 |  50000 |  50000 |00:00:00.01 |     247 |       |       |          |
-----------------------------------------------------------------------------------------------------------------------

Predicate Information (identified by operation id):
---------------------------------------------------

   3 - access("C"."ID"="O"."CUSTOMER_ID")
   4 - filter(("O"."TOTAL">1200 AND "O"."STATUS"='refunded'))

Note
-----
   - statistics feedback used for this statement
   - this is an adaptive plan
"#;

    #[test]
    fn display_reads_operations_nesting_and_predicates() {
        let plan = parse_oracle_xplan_text(DISPLAY).unwrap();
        assert_eq!(plan.nodes.len(), 6);
        assert_eq!(plan.statement_type, "EXPLAIN PLAN");
        let edges: Vec<(i32, i32)> = plan
            .edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect();
        assert_eq!(edges, vec![(0, 1), (1, 2), (2, 3), (3, 4), (3, 5)]);
        let orders = &plan.nodes[4];
        assert_eq!(orders.physical_op, "TABLE ACCESS FULL ORDERS");
        assert_eq!(orders.estimated_rows, 5422.0);
        assert_eq!(orders.estimated_cost, 787.0);
        assert!(
            orders
                .output_list
                .iter()
                .any(|o| o == r#"filter: "O"."TOTAL">1200 AND "O"."STATUS"='refunded'"#),
            "{:?}",
            orders.output_list
        );
        assert!(
            plan.nodes[3]
                .output_list
                .iter()
                .any(|o| o.starts_with("access: "))
        );
        // 137K bytes is abbreviated in the text.
        assert_eq!(plan.total_subtree_cost, 858.0);
        let scans = findings_for_plan(&plan)
            .into_iter()
            .filter(|f| f.summary.key() == Some("services.plans.findings.table_scan.summary"))
            .count();
        assert_eq!(scans, 2);
    }

    #[test]
    fn allstats_reads_actual_rows_and_statement_text() {
        let plan = parse_oracle_xplan_text(ALLSTATS).unwrap();
        assert_eq!(plan.statement_type, "ACTUAL PLAN");
        assert!(
            plan.statement_text
                .starts_with("SELECT /*+ GATHER_PLAN_STATISTICS */ c.country")
        );
        assert!(plan.statement_text.ends_with("ORDER BY revenue DESC"));
        assert_eq!(plan.nodes[4].actual_rows, Some(5409.0));
        assert_eq!(plan.nodes[5].actual_rows, Some(50000.0));
        assert_eq!(plan.nodes[3].estimated_rows, 5422.0);
        // Predicates wrapped in an extra pair of parens keep them.
        assert!(
            plan.nodes[4]
                .output_list
                .iter()
                .any(|o| o.starts_with(r#"filter: ("O"."TOTAL">1200"#))
        );
    }

    #[test]
    fn text_without_a_plan_table_is_an_error() {
        assert!(parse_oracle_xplan_text("Plan hash value: 1\nno table here").is_err());
    }
}
