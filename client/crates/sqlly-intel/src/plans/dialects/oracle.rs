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

use std::collections::HashMap;

use super::{PlanDialect, common};
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{
    ExecutionPlan, PlanBuffers, PlanEdge, PlanNode, PlanParameter, PlanProperty, PlanSpill,
};
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

/// One operation of a plan, from either document form.
#[derive(Default)]
struct Step {
    id: i64,
    parent: i64,
    operation: String,
    options: String,
    owner: String,
    object: String,
    rows: Option<f64>,
    cost: Option<f64>,
    bytes: Option<f64>,
    cpu_cost: Option<f64>,
    io_cost: Option<f64>,
    access: String,
    filter: String,
    /// Rows produced, executions, and elapsed microseconds.
    actual: Option<(f64, f64, f64)>,
}

fn push_property(properties: &mut Vec<PlanProperty>, name: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        properties.push(PlanProperty {
            name: name.to_string(),
            value: value.to_string(),
        });
    }
}

/// The node for one operation, with everything both document forms carry.
fn step_node(step: &Step) -> PlanNode {
    let operation = step.operation.as_str();
    let options = step.options.as_str();
    let mut physical_op = operation.to_string();
    if !options.is_empty() {
        physical_op.push(' ');
        physical_op.push_str(options);
    }
    if !step.object.is_empty() {
        physical_op.push(' ');
        if !step.owner.is_empty() {
            physical_op.push_str(&step.owner);
            physical_op.push('.');
        }
        physical_op.push_str(&step.object);
    }
    let is_join = matches!(operation, "HASH JOIN" | "MERGE JOIN" | "NESTED LOOPS");
    let mut node = PlanNode {
        id: step.id as i32,
        logical_op: operation.to_string(),
        physical_op,
        estimated_rows: step.rows.unwrap_or_default(),
        estimated_cost: step.cost.unwrap_or_default(),
        estimated_cpu: step.cpu_cost.unwrap_or_default(),
        estimated_io: step.io_cost.unwrap_or_default(),
        ..Default::default()
    };
    if is_join {
        node.join_type = Some(if options.is_empty() {
            "INNER".to_string()
        } else {
            options.to_string()
        });
    }
    if !step.object.is_empty() {
        // The Name of an index operation is the index, not its table.
        if operation.ends_with("INDEX") {
            node.index_name = Some(step.object.clone());
        } else {
            node.relation = Some(step.object.clone());
        }
        if !step.owner.is_empty() {
            node.schema = Some(step.owner.clone());
        }
    }
    if !step.access.is_empty() {
        node.output_list.push(format!("access: {}", step.access));
        if is_join {
            node.join_predicate = Some(step.access.clone());
        } else {
            node.index_cond = Some(step.access.clone());
        }
    }
    if !step.filter.is_empty() {
        node.output_list.push(format!("filter: {}", step.filter));
        node.filter = Some(step.filter.clone());
    }
    if let Some(bytes) = step.bytes.filter(|bytes| *bytes > 0.0) {
        push_property(&mut node.properties, "Bytes", &bytes.to_string());
        if let Some(rows) = step.rows.filter(|rows| *rows > 0.0) {
            node.estimated_row_width = Some(bytes / rows);
        }
    }
    if let Some((rows, starts, elapsed_us)) = step.actual {
        node.output_list.push(format!(
            "actual: {rows} rows, {starts} starts, {:.3} ms",
            elapsed_us / 1000.0
        ));
        // A-Rows and A-Time are totals over every start of the operation.
        node.actual_rows = Some(rows);
        node.actual_rows_total = Some(rows);
        node.loops = Some(starts);
        node.actual_rebinds = Some(starts as i32);
        node.actual_time_ms = Some(elapsed_us / 1000.0);
        node.never_executed = starts == 0.0;
    }
    if operation == "TABLE ACCESS" && options.split_whitespace().any(|word| word == "FULL") {
        node.warnings.push(PlanWarning::TableScan {
            table: step.object.clone(),
            schema: step.owner.clone(),
        });
    }
    if operation == "MERGE JOIN" && options == "CARTESIAN" {
        node.warnings.push(PlanWarning::NoJoinPredicate);
    }
    node
}

fn assemble(nodes: Vec<PlanNode>, parents: &[(i64, i64)], format: &str) -> ExecutionPlan {
    let edges = parents
        .iter()
        .filter(|(id, parent)| *parent >= 0 && parent != id)
        .map(|(id, parent)| PlanEdge {
            from_node: *parent as i32,
            to_node: *id as i32,
        })
        .collect();
    let has_actuals = nodes.iter().any(|node| node.actual_rows.is_some());
    let root = nodes.iter().find(|node| node.id == 0).or(nodes.first());
    let mut plan = ExecutionPlan {
        statement_type: if has_actuals {
            "ACTUAL PLAN".to_string()
        } else {
            "EXPLAIN PLAN".to_string()
        },
        total_subtree_cost: root.map(|node| node.estimated_cost).unwrap_or_default(),
        estimated_rows: root.map(|node| node.estimated_rows).unwrap_or_default(),
        execution_time_ms: root.and_then(|node| node.actual_time_ms),
        nodes,
        edges,
        dialect: Some(PlanDialect::Oracle),
        source_format: Some(format.to_string()),
        ..Default::default()
    };
    common::finish_plan(&mut plan);
    plan
}

/// Lines a client prints around the rows: a column heading, a rule, the
/// `11 rows selected.` footer.
fn is_rows_noise(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || is_rule(trimmed)
        || trimmed.eq_ignore_ascii_case("PLAN_LINE")
        || is_sqlplus_feedback(trimmed)
}

/// Parse the tab-separated `PLAN_TABLE` lines into a plan.
///
/// `PLAN_TABLE` lists every row of an adaptive plan, including the
/// alternative that was not chosen, and these lines do not say which rows
/// are inactive, so an adaptive plan shows both alternatives.
pub fn parse_oracle_plan_lines(doc: &str) -> PlanResult<ExecutionPlan> {
    let mut nodes = Vec::new();
    let mut parents = Vec::new();
    for line in doc.lines() {
        if is_rows_noise(line) {
            continue;
        }
        let fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
        if fields.len() < 3 {
            return Err(PlanError::InvalidStructure(format!(
                "EXPLAIN PLAN: malformed line `{line}`"
            )));
        }
        let field = |i: usize| fields.get(i).map(|s| s.trim()).unwrap_or("");
        let number = |i: usize| field(i).parse::<f64>().ok().filter(|n| n.is_finite());
        let id: i64 = field(0).parse().map_err(|_| {
            PlanError::InvalidStructure(format!("EXPLAIN PLAN: bad id in `{line}`"))
        })?;
        let parent: i64 = field(1).parse().map_err(|_| {
            PlanError::InvalidStructure(format!("EXPLAIN PLAN: bad parent in `{line}`"))
        })?;
        // ACTUAL captures carry three more fields.
        let actual = (fields.len() >= 16).then(|| {
            (
                number(13).unwrap_or_default(),
                number(14).unwrap_or_default(),
                number(15).unwrap_or_default(),
            )
        });
        let step = Step {
            id,
            parent,
            operation: field(2).to_string(),
            options: field(3).to_string(),
            owner: field(4).to_string(),
            object: field(5).to_string(),
            rows: number(6),
            cost: number(7),
            bytes: number(8),
            cpu_cost: number(9),
            io_cost: number(10),
            access: field(11).to_string(),
            filter: field(12).to_string(),
            actual,
        };
        nodes.push(step_node(&step));
        parents.push((step.id, step.parent));
    }
    if nodes.is_empty() {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN PLAN: no PLAN_TABLE rows".into(),
        ));
    }
    Ok(assemble(nodes, &parents, "oracle-rows"))
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
    "MAT_VIEW REWRITE ACCESS",
    "MAT_VIEW ACCESS",
    "BITMAP INDEX",
    "BITMAP CONVERSION",
    "DOMAIN INDEX",
    "INDEX",
    "HASH JOIN",
    "HASH",
    "MERGE JOIN",
    "NESTED LOOPS",
    "SORT",
    "PARTITION RANGE",
    "PARTITION LIST",
    "PARTITION HASH",
    "PARTITION REFERENCE",
    "PARTITION SYSTEM",
    "PX BLOCK",
    "PX SEND",
    "PX RECEIVE",
    "PX COORDINATOR",
    "PART JOIN FILTER",
    "JOIN FILTER",
    "WINDOW",
    "COUNT",
    "CONNECT BY",
    "COLLECTION ITERATOR",
    "VIEW",
    "BUFFER",
    "LOAD AS SELECT",
    "LOAD",
];

/// Longest run of lines a wrapped row or a section is read over.
const MAX_WRAPPED_LINES: usize = 6;

/// Parse the text `DBMS_XPLAN.DISPLAY` / `DISPLAY_CURSOR` prints, or the
/// `Execution Plan` SQL*Plus AUTOTRACE prints: the plan people copy out of
/// SQL*Plus, SQL Developer, or SQLcl. When the document holds several
/// plans this returns the first; [`parse_oracle_xplan_plans`] returns all.
///
/// The operation table is read by its header, whatever columns the format
/// option produced (`Rows` / `Bytes` / `Cost (%CPU)` / `Time`, `TempSpc`,
/// `Pstart` / `Pstop`, `TQ` / `IN-OUT` / `PQ Distrib`, and with `ALLSTATS`
/// `Starts`, `E-Rows`, `A-Rows`, `A-Time`, `Buffers`, `Reads`, `Writes`,
/// `OMem`, `1Mem`, `Used-Mem`, `Used-Tmp`). Nesting comes from the
/// Operation column's indentation. The sections under the table
/// (`Predicate Information`, `Column Projection Information`, `Query Block
/// Name / Object Alias`, `Peeked Binds`, `Hint Report`, `Note`, and
/// AUTOTRACE's `Statistics`) are matched back to their operation ids or
/// kept as plan notes. Rows an adaptive plan marks inactive (`-` before the
/// id) are left out.
pub fn parse_oracle_xplan_text(doc: &str) -> PlanResult<ExecutionPlan> {
    parse_oracle_xplan_plans(doc)?
        .into_iter()
        .next()
        .ok_or_else(|| PlanError::InvalidStructure("DBMS_XPLAN: no operation rows".into()))
}

/// Every plan of a DBMS_XPLAN document. `DISPLAY_CURSOR` called for several
/// cursors, or several AUTOTRACE runs pasted together, give one table each.
pub fn parse_oracle_xplan_plans(doc: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let lines = xplan_lines(doc);
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_xplan_header(line))
        .map(|(index, _)| index)
        .collect();
    if headers.is_empty() {
        return Err(PlanError::InvalidStructure(
            "DBMS_XPLAN: no `| Id | Operation |` header row found".into(),
        ));
    }
    // Each plan starts at the first preamble line (`SQL_ID`, `Plan hash
    // value`, AUTOTRACE's `Execution Plan`) after the previous table.
    let mut starts = Vec::with_capacity(headers.len());
    for (index, header) in headers.iter().enumerate() {
        let previous = match index.checked_sub(1).and_then(|i| headers.get(i)) {
            Some(previous) => previous + 1,
            None => {
                starts.push(0);
                continue;
            }
        };
        let preamble = lines.get(previous..*header).unwrap_or_default();
        let offset = preamble.iter().position(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("SQL_ID")
                || trimmed.starts_with("Plan hash value")
                || trimmed.trim_end() == "Execution Plan"
        });
        starts.push(previous + offset.unwrap_or(preamble.len()));
    }
    let mut plans = Vec::new();
    for (index, header) in headers.iter().enumerate() {
        let start = starts.get(index).copied().unwrap_or(*header);
        let end = starts.get(index + 1).copied().unwrap_or(lines.len());
        let segment = lines.get(start..end).unwrap_or_default();
        if let Some(plan) = parse_xplan_segment(segment, header.saturating_sub(start)) {
            plans.push(plan);
        }
    }
    if plans.is_empty() {
        return Err(PlanError::InvalidStructure(
            "DBMS_XPLAN: no operation rows".into(),
        ));
    }
    Ok(plans)
}

/// True for DBMS_XPLAN or AUTOTRACE text: a `| Id | Operation |` table
/// header and at least one row under it whose first cell is an operation
/// id.
pub fn looks_like_oracle_xplan(doc: &str) -> bool {
    let mut in_table = false;
    for line in doc.lines() {
        if is_xplan_header(line) {
            in_table = true;
        } else if in_table
            && xplan_cells(line)
                .first()
                .is_some_and(|cell| xplan_id(cell).is_some())
        {
            return true;
        }
    }
    false
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-')
}

/// `Explained.`, `25 rows selected.`, a `SQL>` prompt, `Elapsed: ...`.
fn is_sqlplus_feedback(trimmed: &str) -> bool {
    trimmed == "Explained."
        || trimmed.ends_with("rows selected.")
        || trimmed.ends_with("row selected.")
        || trimmed.starts_with("SQL>")
        || trimmed.starts_with("Elapsed:")
}

fn is_xplan_header(line: &str) -> bool {
    let cells = xplan_cells(line);
    cells.first().is_some_and(|cell| cell.trim() == "Id")
        && cells.iter().any(|cell| cell.trim() == "Operation")
}

/// The id of an operation row's first cell: `3`, `*  3`, `- * 10`. The
/// flags are whether the row has predicates (`*`) and whether an adaptive
/// plan marks it inactive (`-`).
fn xplan_id(cell: &str) -> Option<(i64, bool)> {
    let trimmed = cell.trim();
    let digits = trimmed.trim_start_matches(['-', '*', ' ']);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, trimmed.starts_with('-')))
}

/// The document's lines without what SQL*Plus adds to a paged or narrow
/// listing: the `PLAN_TABLE_OUTPUT` heading and its rule, repeated on every
/// page, and table rows broken at the line size (joined back together).
fn xplan_lines(doc: &str) -> Vec<String> {
    let raw: Vec<&str> = doc.lines().collect();
    let mut kept = Vec::with_capacity(raw.len());
    let mut i = 0;
    while let Some(line) = raw.get(i) {
        i += 1;
        if line.trim() == "PLAN_TABLE_OUTPUT" {
            if raw.get(i).is_some_and(|next| is_rule(next)) {
                i += 1;
            }
            continue;
        }
        kept.push(*line);
    }
    let mut lines = Vec::with_capacity(kept.len());
    // Cell count of the current table when its header was wrapped.
    let mut wrapped: Option<usize> = None;
    let mut i = 0;
    while let Some(line) = kept.get(i) {
        i += 1;
        let mut text = line.to_string();
        if line.trim_start().starts_with('|') {
            let first = xplan_cells(line)
                .first()
                .map(|cell| cell.trim().to_string());
            let needs_more = |text: &str| match (first.as_deref(), wrapped) {
                (Some("Id"), _) => !text.trim_end().ends_with('|'),
                (_, Some(count)) => xplan_cells(text).len() < count,
                _ => false,
            };
            let is_header = first.as_deref() == Some("Id");
            let mut joined = false;
            for _ in 0..MAX_WRAPPED_LINES {
                if !needs_more(&text) {
                    break;
                }
                let Some(next) = kept.get(i) else {
                    break;
                };
                if next.trim().is_empty() || is_rule(next) {
                    break;
                }
                text.push_str(next);
                joined = true;
                i += 1;
            }
            if is_header {
                wrapped = joined.then(|| xplan_cells(&text).len());
            }
        }
        lines.push(text);
    }
    lines
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    None,
    Skip,
    QueryBlocks,
    Binds,
    Predicates,
    Projection,
    Hints,
    Notes,
    Statistics,
}

fn section_heading(trimmed: &str) -> Option<Section> {
    let starts = |prefix: &str| trimmed.starts_with(prefix);
    Some(if starts("Predicate Information") {
        Section::Predicates
    } else if starts("Column Projection Information") {
        Section::Projection
    } else if starts("Query Block Name") {
        Section::QueryBlocks
    } else if starts("Peeked Binds") {
        Section::Binds
    } else if starts("Hint Report") {
        Section::Hints
    } else if trimmed == "Note" {
        Section::Notes
    } else if trimmed == "Statistics" {
        Section::Statistics
    } else if starts("Outline Data")
        || starts("Query Block Registry")
        || starts("Remote SQL Information")
        || trimmed.ends_with("(identified by operation id):")
    {
        Section::Skip
    } else {
        return None;
    })
}

/// `   4 - rest` gives `(4, "rest")`.
fn id_entry(trimmed: &str) -> Option<(i64, &str)> {
    let (id, rest) = trimmed.split_once(" - ")?;
    Some((id.trim().parse().ok()?, rest.trim()))
}

fn parse_xplan_segment(lines: &[String], header_at: usize) -> Option<ExecutionPlan> {
    let header: Vec<String> = xplan_cells(lines.get(header_at)?)
        .iter()
        .map(|cell| cell.trim().to_string())
        .collect();
    let op_col = header.iter().position(|name| name == "Operation")?;

    // ---- Operation table -------------------------------------------------
    struct Row {
        step: Step,
        inactive: bool,
        /// Header name and cell text of every column that is not Id,
        /// Operation or Name.
        cells: Vec<(String, String)>,
    }
    let mut rows: Vec<Row> = Vec::new();
    // parents[depth] = id of the latest row at that depth.
    let mut parents: Vec<i64> = Vec::new();
    let mut end = header_at + 1;
    for (index, line) in lines.iter().enumerate().skip(header_at + 1) {
        let trimmed = line.trim();
        if is_rule(trimmed) && !rows.is_empty() {
            end = index;
            break;
        }
        end = index + 1;
        if !trimmed.starts_with('|') {
            continue;
        }
        let cells = xplan_cells(line);
        let Some((id, inactive)) = cells.first().and_then(|cell| xplan_id(cell)) else {
            continue;
        };
        let raw_op = cells.get(op_col).copied().unwrap_or_default();
        // One space of padding, then one more per level of nesting.
        let depth = raw_op
            .chars()
            .take_while(|c| *c == ' ')
            .count()
            .saturating_sub(1);
        let text = raw_op.trim();
        let (operation, options) = XPLAN_OPERATIONS
            .iter()
            .find_map(|op| {
                let rest = text.strip_prefix(op)?;
                (rest.is_empty() || rest.starts_with(' '))
                    .then(|| (op.to_string(), rest.trim().to_string()))
            })
            .unwrap_or_else(|| (text.to_string(), String::new()));
        let parent = match depth.checked_sub(1) {
            None => -1,
            Some(above) => parents.get(above).or(parents.last()).copied().unwrap_or(-1),
        };
        parents.truncate(depth);
        parents.push(id);
        let mut step = Step {
            id,
            parent,
            operation,
            options,
            ..Default::default()
        };
        let mut others = Vec::new();
        for (name, cell) in header.iter().zip(&cells).skip(1) {
            let value = cell.trim();
            match name.as_str() {
                "Operation" => {}
                "Name" => step.object = value.to_string(),
                _ if !value.is_empty() => others.push((name.clone(), value.to_string())),
                _ => {}
            }
        }
        rows.push(Row {
            step,
            inactive,
            cells: others,
        });
    }
    if rows.is_empty() {
        return None;
    }

    // ---- Sections under the table ----------------------------------------
    let mut predicates: Vec<(i64, String, String)> = Vec::new();
    let mut projections: Vec<(i64, String)> = Vec::new();
    let mut blocks: HashMap<i64, String> = HashMap::new();
    let mut parameters = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut statistics: Vec<(String, f64)> = Vec::new();
    let mut section = Section::None;
    // The Note section is open and its last line can still continue.
    for line in lines.get(end..).unwrap_or_default() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_rule(trimmed) {
            continue;
        }
        if let Some(next) = section_heading(trimmed) {
            section = next;
            continue;
        }
        if is_sqlplus_feedback(trimmed) || !line.starts_with([' ', '\t']) {
            // Sections indent their lines; anything else is not theirs.
            section = Section::None;
            continue;
        }
        match section {
            Section::None | Section::Skip => {}
            Section::Predicates => {
                let (id, rest) = match id_entry(trimmed) {
                    Some((id, rest)) => (Some(id), rest),
                    None => (predicates.last().map(|(id, _, _)| *id), trimmed),
                };
                let kind = ["access", "filter", "storage"]
                    .into_iter()
                    .find_map(|kind| Some((kind, rest.strip_prefix(kind)?.strip_prefix('(')?)));
                match (id, kind) {
                    (Some(id), Some((kind, body))) => {
                        predicates.push((id, kind.to_string(), body.to_string()));
                    }
                    _ => {
                        // A long predicate continues on the next line.
                        if let Some((_, _, body)) = predicates.last_mut() {
                            body.push(' ');
                            body.push_str(trimmed);
                        }
                    }
                }
            }
            Section::Projection => match id_entry(trimmed) {
                Some((id, rest)) => projections.push((id, rest.to_string())),
                None => {
                    if let Some((_, text)) = projections.last_mut() {
                        text.push(' ');
                        text.push_str(trimmed);
                    }
                }
            },
            Section::QueryBlocks => {
                if let Some((id, rest)) = id_entry(trimmed) {
                    blocks.insert(id, rest.to_string());
                }
            }
            Section::Binds => {
                // `1 - :B1 (NUMBER): 77`, or without the name.
                if let Some((position, rest)) = id_entry(trimmed) {
                    let (left, value) = match rest.split_once("): ") {
                        Some((left, value)) => (left, Some(value.trim().to_string())),
                        None => (rest.trim_end_matches(')'), None),
                    };
                    let (name, data_type) = match left.split_once('(') {
                        Some((name, data_type)) => (name.trim(), Some(data_type.to_string())),
                        None => (left.trim(), None),
                    };
                    parameters.push(PlanParameter {
                        name: if name.is_empty() {
                            format!(":{position}")
                        } else {
                            name.to_string()
                        },
                        data_type,
                        compiled_value: value,
                        ..Default::default()
                    });
                }
            }
            Section::Hints => {
                let text = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
                // `0 - STATEMENT` and `2 - SEL$1 / "T"@"SEL$1"` say where
                // the hints under them apply.
                if id_entry(trimmed).is_none() {
                    notes.push(format!("Hint report: {text}"));
                }
            }
            Section::Notes => match trimmed.strip_prefix('-') {
                Some(note) => notes.push(note.trim().to_string()),
                None => {
                    if let Some(last) = notes.last_mut() {
                        last.push(' ');
                        last.push_str(trimmed);
                    }
                }
            },
            Section::Statistics => {
                // `       7973  consistent gets`
                let mut words = trimmed.splitn(2, char::is_whitespace);
                let value = words
                    .next()
                    .and_then(|word| word.parse::<f64>().ok())
                    .filter(|value| value.is_finite());
                if let (Some(value), Some(name)) = (value, words.next()) {
                    statistics.push((name.trim().to_string(), value));
                }
            }
        }
    }
    let mut access: HashMap<i64, String> = HashMap::new();
    let mut filter: HashMap<i64, String> = HashMap::new();
    let mut storage: HashMap<i64, String> = HashMap::new();
    for (id, kind, body) in predicates {
        // Drop the parenthesis `filter(` opened.
        let body = body.trim_end();
        let body = body.strip_suffix(')').unwrap_or(body);
        let map = match kind.as_str() {
            "access" => &mut access,
            "filter" => &mut filter,
            _ => &mut storage,
        };
        let entry = map.entry(id).or_default();
        if !entry.is_empty() {
            entry.push_str(" AND ");
        }
        entry.push_str(body);
    }

    // ---- Nodes -----------------------------------------------------------
    // An inactive row's inputs hang under its nearest active ancestor.
    let inactive: HashMap<i64, i64> = rows
        .iter()
        .filter(|row| row.inactive)
        .map(|row| (row.step.id, row.step.parent))
        .collect();
    let left_out = inactive.len();
    let mut nodes = Vec::new();
    let mut parent_of = Vec::new();
    for mut row in rows {
        if row.inactive {
            continue;
        }
        for _ in 0..inactive.len() {
            match inactive.get(&row.step.parent) {
                Some(above) => row.step.parent = *above,
                None => break,
            }
        }
        if inactive.contains_key(&row.step.parent) {
            row.step.parent = -1;
        }
        let id = row.step.id;
        row.step.access = access.remove(&id).unwrap_or_default();
        row.step.filter = filter.remove(&id).unwrap_or_default();
        let cell = |names: &[&str]| {
            row.cells
                .iter()
                .find(|(name, _)| names.contains(&name.as_str()))
                .map(|(_, value)| value.as_str())
        };
        row.step.rows = cell(&["Rows", "E-Rows"]).and_then(common::parse_count);
        row.step.bytes = cell(&["Bytes", "E-Bytes"]).and_then(xplan_size_bytes);
        let cost_cell = row
            .cells
            .iter()
            .find(|(name, _)| name.starts_with("Cost"))
            .map(|(_, value)| value.as_str());
        let (cost, cpu_percent) = match cost_cell.map(|cost| cost.split_once('(')) {
            Some(Some((cost, percent))) => (
                common::parse_count(cost),
                Some(percent.trim_end_matches(')').trim().to_string()),
            ),
            _ => (cost_cell.and_then(common::parse_count), None),
        };
        row.step.cost = cost;
        if let Some(rows) = cell(&["A-Rows"]).and_then(common::parse_count) {
            row.step.actual = Some((
                rows,
                cell(&["Starts"])
                    .and_then(common::parse_count)
                    .unwrap_or_default(),
                cell(&["A-Time"])
                    .and_then(xplan_elapsed_us)
                    .unwrap_or_default(),
            ));
        }
        let mut node = step_node(&row.step);
        if cell(&["A-Time"]).is_none() {
            node.actual_time_ms = None;
        }
        if let Some(percent) = cpu_percent {
            push_property(&mut node.properties, "Cost %CPU", &percent);
        }
        if let Some(storage) = storage.remove(&id) {
            node.output_list.push(format!("storage: {storage}"));
            push_property(&mut node.properties, "storage", &storage);
        }
        let mut buffers = PlanBuffers::default();
        let mut partition = (None, None);
        for (name, value) in &row.cells {
            match name.as_str() {
                "Rows" | "E-Rows" | "Bytes" | "E-Bytes" | "Starts" | "A-Rows" | "A-Time" => {}
                name if name.starts_with("Cost") => {}
                "Buffers" => buffers.logical_reads = common::parse_count(value),
                "Reads" => buffers.physical_reads = common::parse_count(value),
                "Writes" => buffers.writes = common::parse_count(value),
                "Used-Mem" => {
                    // `1542K (0)`: the size, then the number of passes over
                    // temporary space (0 is all in memory).
                    let (size, passes) = value.split_once('(').unwrap_or((value.as_str(), ""));
                    node.peak_memory_kb = xplan_size_bytes(size).map(|bytes| bytes / 1024.0);
                    push_property(&mut node.properties, name, value);
                    push_property(&mut node.properties, "Passes", passes.trim_end_matches(')'));
                }
                "Used-Tmp" | "Max-Tmp" => {
                    // Releases before 12.2 print this column in kB without
                    // a unit; later ones print bytes with a K/M/G suffix.
                    let bare = value.chars().all(|c| c.is_ascii_digit());
                    let written_kb = if bare {
                        common::parse_count(value)
                    } else {
                        xplan_size_bytes(value).map(|bytes| bytes / 1024.0)
                    };
                    if written_kb.is_some() && name == "Used-Tmp" {
                        let kind = match node.logical_op.as_str() {
                            "SORT" | "WINDOW" | "BUFFER" => "sort",
                            op if op.starts_with("HASH") => "hash",
                            _ => "",
                        };
                        node.spill = Some(PlanSpill {
                            kind: kind.to_string(),
                            written_kb,
                            ..Default::default()
                        });
                    }
                    push_property(&mut node.properties, name, value);
                }
                "Pstart" => {
                    partition.0 = Some(value.clone());
                    push_property(&mut node.properties, name, value);
                }
                "Pstop" => {
                    partition.1 = Some(value.clone());
                    push_property(&mut node.properties, name, value);
                }
                "TQ" => {
                    node.parallel = true;
                    push_property(&mut node.properties, name, value);
                }
                _ => push_property(&mut node.properties, name, value),
            }
        }
        if buffers != PlanBuffers::default() {
            node.buffers = Some(buffers);
        }
        node.parallel |= node.logical_op.starts_with("PX ");
        if let (Some(start), Some(stop)) = partition {
            if let (Ok(first), Ok(last)) = (start.parse::<u32>(), stop.parse::<u32>())
                && last >= first
            {
                node.partitions_scanned = Some(last - first + 1);
            }
            node.partitions = vec![start, stop];
        }
        if let Some(block) = blocks.get(&id) {
            // `SEL$1 / "O"@"SEL$1"`: query block, then the object's alias.
            let (name, alias) = match block.split_once(" / ") {
                Some((name, alias)) => (name, Some(alias)),
                None => (block.as_str(), None),
            };
            push_property(&mut node.properties, "Query Block", name);
            if let Some(alias) = alias {
                push_property(&mut node.properties, "Object Alias", alias);
                let short = alias
                    .split('@')
                    .next()
                    .unwrap_or_default()
                    .trim_matches('"');
                if !short.is_empty() {
                    node.alias = Some(short.to_string());
                }
            }
        }
        parent_of.push((id, row.step.parent));
        nodes.push(node);
    }
    if nodes.is_empty() {
        return None;
    }
    for (id, text) in projections {
        let Some(node) = nodes.iter_mut().find(|node| i64::from(node.id) == id) else {
            continue;
        };
        // `(#keys=1; rowset=256) "A"."X"[NUMBER,22], ...`
        let mut columns = text.as_str();
        if let Some(rest) = text.strip_prefix('(')
            && let Some((info, rest)) = rest.split_once(") ")
            && info.contains('=')
        {
            push_property(&mut node.properties, "Projection", info);
            columns = rest;
        }
        node.output_columns = split_projection(columns);
    }

    let mut plan = assemble(nodes, &parent_of, "oracle-xplan");
    let preamble = lines.get(..header_at).unwrap_or_default();
    plan.statement_text = xplan_statement_text(preamble);
    for line in preamble {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("SQL_ID") {
            // `SQL_ID  9ruaw8agctzdw, child number 1`
            let (sql_id, child) = rest.split_once(',').unwrap_or((rest, ""));
            if !sql_id.trim().is_empty() {
                plan.query_hash = Some(sql_id.trim().to_string());
            }
            if !child.trim().is_empty() {
                plan.notes.push(child.trim().to_string());
            }
        } else if let Some(hash) = trimmed.strip_prefix("Plan hash value:") {
            plan.query_plan_hash = Some(hash.trim().to_string());
        }
    }
    plan.parameters = parameters;
    plan.notes.extend(notes);
    if left_out > 0 {
        plan.notes.push(format!(
            "{left_out} inactive rows of the adaptive plan are left out"
        ));
    }
    // AUTOTRACE statistics describe the whole statement.
    let statistic = |name: &str| {
        statistics
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| *value)
    };
    if let Some(gets) = statistic("consistent gets") {
        let root = plan.nodes.iter_mut().find(|node| node.buffers.is_none());
        if let Some(root) = root.filter(|node| node.id == 0) {
            root.buffers = Some(PlanBuffers {
                logical_reads: Some(gets + statistic("db block gets").unwrap_or_default()),
                physical_reads: statistic("physical reads"),
                ..Default::default()
            });
        }
    }
    for (name, value) in &statistics {
        plan.notes.push(format!("{name}: {value}"));
    }
    Some(plan)
}

/// Split a projection list at its top-level commas and drop each column's
/// `[NUMBER,22]` type suffix.
fn split_projection(text: &str) -> Vec<String> {
    let mut columns = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut flush = |current: &mut String| {
        let column = current.trim();
        let column = match column.rfind('[') {
            Some(at) if column.ends_with(']') => column.get(..at).unwrap_or(column).trim(),
            _ => column,
        };
        if !column.is_empty() {
            columns.push(column.to_string());
        }
        current.clear();
    };
    for c in text.chars() {
        match quote {
            Some(open) => {
                if c == open {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    flush(&mut current);
                    continue;
                }
                _ => {}
            },
        }
        current.push(c);
    }
    flush(&mut current);
    columns
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

/// A size DBMS_XPLAN prints in bytes: `2048`, `137K`, `1744K`, `16M`. The
/// suffixes are powers of 1024 here (row counts and costs use powers of
/// 1000).
fn xplan_size_bytes(cell: &str) -> Option<f64> {
    let cell = cell.trim();
    let (digits, scale) = [('K', 1), ('M', 2), ('G', 3), ('T', 4), ('P', 5)]
        .into_iter()
        .find_map(|(suffix, power)| Some((cell.strip_suffix(suffix)?, 1024f64.powi(power))))
        .unwrap_or((cell, 1.0));
    let value = digits.trim().parse::<f64>().ok()? * scale;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// `00:00:00.01` (A-Time) as microseconds.
fn xplan_elapsed_us(cell: &str) -> Option<f64> {
    let mut seconds = 0.0;
    for part in cell.trim().split(':') {
        let value = part.trim().parse::<f64>().ok().filter(|v| v.is_finite())?;
        seconds = seconds * 60.0 + value;
    }
    let micros = seconds * 1_000_000.0;
    micros.is_finite().then_some(micros)
}

/// `DISPLAY_CURSOR` opens with `SQL_ID …`, a rule, and the statement text
/// wrapped over several lines.
fn xplan_statement_text(preamble: &[String]) -> String {
    let Some(sql_id) = preamble
        .iter()
        .position(|l| l.trim_start().starts_with("SQL_ID"))
    else {
        return String::new();
    };
    preamble
        .get(sql_id + 1..)
        .unwrap_or_default()
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
        // The plan is too small for a finding: the rules need measured rows
        // or a meaningful share of the cost before they call out a scan.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
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
        // An estimated Oracle plan prints the rows a scan returns, not the
        // rows it reads, so nothing shows that either full scan is wasteful.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
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

    /// AUTOTRACE statistics and `A-Time` cells too large for an f64 must not
    /// reach the plan as infinity.
    #[test]
    fn numbers_too_large_for_an_f64_are_dropped_not_read_as_infinity() {
        let huge = "9".repeat(400);
        let doc = format!(
            "Execution Plan\n\
             ----------------------------------------------------------\n\
             | Id  | Operation         | Name | Rows  | A-Rows |   A-Time   |\n\
             ----------------------------------------------------------\n\
             |   0 | SELECT STATEMENT  |      |     1 |      1 |{huge}:00:00.01 |\n\
             |   1 |  TABLE ACCESS FULL| EMP  |     1 |      1 |00:00:00.01 |\n\
             ----------------------------------------------------------\n\
             \n\
             Statistics\n\
             ----------------------------------------------------------\n\
             \x20      {huge}  consistent gets\n\
             \x20         7  physical reads\n"
        );
        let plan = parse_oracle_xplan_text(&doc).unwrap();
        assert_eq!(plan.nodes.len(), 2);
        // The unreadable time counts as zero, the readable one stays.
        assert_eq!(plan.nodes[0].actual_time_ms, Some(0.0));
        assert_eq!(plan.nodes[1].actual_time_ms, Some(10.0));
        assert!(plan.nodes[0].buffers.is_none());
        assert_eq!(plan.notes, ["physical reads: 7"]);
        assert_eq!(xplan_elapsed_us("00:00:00.01"), Some(10_000.0));
        assert_eq!(
            xplan_elapsed_us(&format!("{}:00:00", "9".repeat(308))),
            None
        );
    }

    #[test]
    fn text_without_a_plan_table_is_an_error() {
        assert!(parse_oracle_xplan_text("Plan hash value: 1\nno table here").is_err());
    }
}
