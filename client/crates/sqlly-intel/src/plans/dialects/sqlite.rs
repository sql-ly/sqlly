//! SQLite: `EXPLAIN QUERY PLAN`.
//!
//! The statement returns rows of `(id, parent, notused, detail)`. The
//! `sqlite3` shell does not print those columns: in every output mode it
//! draws them as a tree under a `QUERY PLAN` line (`|--` / `` `-- `` markers,
//! three columns per level). Other clients show the raw rows, separated by
//! `|`, tabs or commas, or drawn as a grid. SQLite before 3.24 returned
//! `(selectid, order, from, detail)` instead, with no parent column, and
//! spelled scans `SCAN TABLE t`.
//!
//! Plain `EXPLAIN` (without `QUERY PLAN`) lists the statement's bytecode. That
//! is a program listing, not a plan, and is rejected with a message saying so.

use std::collections::{HashMap, HashSet};

use crate::plans::dialects::PlanDialect;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;

/// Nesting deeper than this is treated as a malformed document.
const MAX_DEPTH: usize = 512;

/// Parse SQLite `EXPLAIN QUERY PLAN` rows rendered as `id|parent|detail`
/// lines (one per row, in result order). Node ids are the ids in the text.
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
///   every output mode. Result rows before or after the tree (`.eqp on`,
///   `.scanstats on`) and a bytecode listing after it (`.eqp full`) are
///   skipped. The `.scanstats on` suffix `(loops=N rows=N)` fills `loops`,
///   `actual_rows` and `actual_rows_total`; `est=` fills `estimated_rows`;
/// - result rows with the raw four columns `id, parent, notused, detail`,
///   separated by `|` (list mode, most exports), tabs or commas, drawn as a
///   box / table / Markdown / column grid, or exported as JSON objects or
///   `name = value` lines, with or without a header row;
/// - the four columns `selectid, order, from, detail` of SQLite before 3.24,
///   which carry no parent and so produce a flat list of steps;
/// - the three-column `id|parent|detail` lines
///   [`parse_sqlite_query_plan_text`] reads.
///
/// Nodes are numbered from 1 in document order. A bytecode listing (plain
/// `EXPLAIN`) on its own is an error.
pub fn parse_sqlite_query_plan_any(doc: &str) -> PlanResult<ExecutionPlan> {
    let lines: Vec<&str> = doc.lines().filter(|l| !l.trim().is_empty()).collect();
    let steps = match read_tree(&lines)? {
        Some(steps) => steps,
        None => {
            let steps = read_rows(doc, &lines);
            if steps.is_empty() {
                if looks_like_sqlite_bytecode(doc) {
                    return Err(PlanError::InvalidStructure(
                        "This is the bytecode listing that plain EXPLAIN prints in SQLite \
                         (addr, opcode, p1..p5), not a query plan. Run EXPLAIN QUERY PLAN \
                         on the statement and paste that output instead."
                            .into(),
                    ));
                }
                return Err(PlanError::InvalidStructure(
                    "EXPLAIN QUERY PLAN: no plan rows found".into(),
                ));
            }
            steps
        }
    };
    build_plan(steps)
}

/// True when `doc` is SQLite `EXPLAIN QUERY PLAN` output: the shell's tree,
/// or raw plan rows in any of the layouts [`parse_sqlite_query_plan_any`]
/// reads. Every step must start with a keyword SQLite uses in plan details,
/// so other pipe- or tab-separated tables are not mistaken for a plan.
pub fn looks_like_sqlite_query_plan(doc: &str) -> bool {
    let lines: Vec<&str> = doc.lines().filter(|l| !l.trim().is_empty()).collect();
    let steps = match read_tree(&lines) {
        Ok(Some(steps)) => steps,
        Ok(None) => read_rows(doc, &lines),
        // Only the depth limit fails here. Judge the lines above the limit,
        // so a SQLite plan that is too deep still reaches the parser, which
        // says why it cannot be read.
        Err(_) => match read_tree(&lines[..lines.len().min(MAX_DEPTH)]) {
            Ok(Some(steps)) => steps,
            _ => return false,
        },
    };
    !steps.is_empty() && steps.iter().all(|step| is_plan_detail(&step.detail))
}

/// True when `doc` is the bytecode listing of plain `EXPLAIN`: the header
/// `addr opcode p1 p2 p3 p4 p5 comment`, or at least two rows of
/// `<address> <Opcode> <int> <int> <int>` separated by spaces, tabs or `|`.
pub fn looks_like_sqlite_bytecode(doc: &str) -> bool {
    let mut opcode_rows = 0usize;
    for line in doc.lines().take(400) {
        let fields: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == '|' || c == '\u{2502}')
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() >= 7
            && fields[0].eq_ignore_ascii_case("addr")
            && fields[1].eq_ignore_ascii_case("opcode")
            && fields[2].eq_ignore_ascii_case("p1")
        {
            return true;
        }
        if fields.len() >= 5
            && fields[0].parse::<u32>().is_ok()
            && is_opcode_name(fields[1])
            && fields[2..5].iter().all(|f| f.parse::<i64>().is_ok())
        {
            opcode_rows += 1;
            if opcode_rows >= 2 {
                return true;
            }
        }
    }
    false
}

/// Opcode names are CamelCase words (`Init`, `OpenRead`, `SeekGE`).
fn is_opcode_name(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && word.len() >= 2
        && chars.all(|c| c.is_ascii_alphanumeric())
        && word.chars().any(|c| c.is_ascii_lowercase())
}

/// Parse SQLite `EXPLAIN QUERY PLAN` `(id, parent, detail)` rows. Node ids
/// are the row ids as given.
pub fn parse_sqlite_query_plan_rows(rows: &[(i64, i64, String)]) -> PlanResult<ExecutionPlan> {
    if rows.is_empty() {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN QUERY PLAN: no rows".into(),
        ));
    }
    let steps: Vec<Step> = rows
        .iter()
        .map(|(id, parent, detail)| {
            let (detail, stats) = split_scan_stats(detail);
            Step {
                id: *id,
                parent: *parent,
                detail: detail.to_string(),
                stats,
                properties: Vec::new(),
            }
        })
        .collect();
    Ok(plan_from_steps(&steps))
}

/// One plan row, whatever layout it came from.
#[derive(Debug, Default)]
struct Step {
    id: i64,
    /// 0 for a top-level step.
    parent: i64,
    detail: String,
    stats: Vec<(String, String)>,
    properties: Vec<PlanProperty>,
}

/// Renumber the steps from 1 in document order, drop parent references that
/// do not point at an earlier step, and build the plan.
fn build_plan(mut steps: Vec<Step>) -> PlanResult<ExecutionPlan> {
    let mut new_ids: HashMap<i64, i64> = HashMap::new();
    for (index, step) in steps.iter_mut().enumerate() {
        let new_id = index as i64 + 1;
        step.parent = if step.parent == 0 {
            0
        } else {
            new_ids.get(&step.parent).copied().unwrap_or(0)
        };
        new_ids.insert(step.id, new_id);
        step.id = new_id;
    }
    check_depth(&steps)?;
    Ok(plan_from_steps(&steps))
}

/// Parents always precede their children here, so depth is one pass.
fn check_depth(steps: &[Step]) -> PlanResult<()> {
    let mut depth: HashMap<i64, usize> = HashMap::new();
    for step in steps {
        let own = depth.get(&step.parent).map_or(0, |d| d + 1);
        if own > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "EXPLAIN QUERY PLAN: the plan is nested deeper than 512 levels".into(),
            ));
        }
        depth.insert(step.id, own);
    }
    Ok(())
}

fn plan_from_steps(steps: &[Step]) -> ExecutionPlan {
    // Names that `CO-ROUTINE x` / `MATERIALIZE x` define: a later `SCAN x`
    // reads that intermediate result, not a table.
    let subplans: HashSet<&str> = steps
        .iter()
        .filter_map(|step| {
            step.detail
                .strip_prefix("CO-ROUTINE ")
                .or_else(|| step.detail.strip_prefix("MATERIALIZE "))
        })
        .map(str::trim)
        .collect();
    let mut nodes = Vec::with_capacity(steps.len());
    let mut edges = Vec::new();
    for step in steps {
        let mut node = PlanNode {
            id: step.id as i32,
            logical_op: step
                .detail
                .split_whitespace()
                .next()
                .unwrap_or("STEP")
                .to_string(),
            physical_op: step.detail.clone(),
            ..Default::default()
        };
        read_detail(&mut node, &step.detail, &subplans);
        apply_scan_stats(&mut node, &step.stats);
        node.properties.extend(step.properties.iter().cloned());
        if step.parent != 0 {
            edges.push(PlanEdge {
                from_node: step.parent as i32,
                to_node: step.id as i32,
            });
        }
        nodes.push(node);
    }
    let mut plan = ExecutionPlan {
        statement_type: "EXPLAIN QUERY PLAN".to_string(),
        nodes,
        edges,
        dialect: Some(PlanDialect::Sqlite),
        source_format: Some("sqlite-eqp".to_string()),
        ..Default::default()
    };
    super::common::finish_plan(&mut plan);
    plan
}

// ----- the shell's tree ----------------------------------------------------

/// Byte offset of the `|--` or `` `-- `` marker when `line` is a tree step:
/// everything before the marker must be `|` and spaces, and a detail follows.
fn tree_marker(line: &str) -> Option<usize> {
    let marker = line.find("|--").into_iter().chain(line.find("`--")).min()?;
    // A Markdown separator row (`|----|----|`) is not a step.
    let detail_starts = line
        .get(marker + 3..)?
        .trim_start()
        .starts_with(|c: char| c != '-' && c != '|');
    (detail_starts && line.get(..marker)?.chars().all(|c| c == ' ' || c == '|')).then_some(marker)
}

/// Read the shell's tree out of `lines`. `Ok(None)` when there is no tree.
/// Lines before the first step and after the last one are ignored: the shell
/// prints result rows and bytecode around the tree depending on its settings.
fn read_tree(lines: &[&str]) -> PlanResult<Option<Vec<Step>>> {
    // With several statements in one paste, read the first tree only.
    let Some(first) = lines.iter().position(|l| tree_marker(l).is_some()) else {
        return Ok(None);
    };
    let mut steps: Vec<Step> = Vec::new();
    // ids[depth] = the id of the latest step seen at that depth.
    let mut ids: Vec<i64> = Vec::new();
    for line in lines.iter().skip(first) {
        let line = line.trim_end();
        let Some(marker) = tree_marker(line) else {
            break;
        };
        let depth = line.get(..marker).map_or(0, |p| p.chars().count()) / 3;
        if depth > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "EXPLAIN QUERY PLAN: the plan is nested deeper than 512 levels".into(),
            ));
        }
        let text = line.get(marker + 3..).unwrap_or("").trim();
        let (detail, stats) = split_scan_stats(text);
        let id = steps.len() as i64 + 1;
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
        steps.push(Step {
            id,
            parent,
            detail: detail.to_string(),
            stats,
            properties: Vec::new(),
        });
    }
    Ok(Some(steps))
}

/// Split the `.scanstats on` suffix off a step: `(loops=1 rows=40000)`,
/// `(loops=1 rows=40000 rpl=40000.0 est=9216.0)`, and on builds with cycle
/// counters `(cycles=123 [45%] loops=1 rows=10)`. An index condition such as
/// `(rowid=?)` is not a suffix because its names are not counters.
fn split_scan_stats(text: &str) -> (&str, Vec<(String, String)>) {
    let trimmed = text.trim_end();
    let none = (trimmed, Vec::new());
    if !trimmed.ends_with(')') {
        return none;
    }
    let Some(open) = trimmed.rfind('(') else {
        return none;
    };
    let inner = trimmed
        .get(open + 1..trimmed.len() - 1)
        .unwrap_or("")
        .trim();
    let mut stats = Vec::new();
    for token in inner.split_whitespace() {
        if let Some(percent) = token
            .strip_prefix('[')
            .and_then(|t| t.strip_suffix("%]"))
            .filter(|t| t.parse::<f64>().is_ok())
        {
            stats.push(("percent".to_string(), percent.to_string()));
            continue;
        }
        let Some((name, value)) = token.split_once('=') else {
            return none;
        };
        if !matches!(name, "loops" | "rows" | "rpl" | "est" | "cycles")
            || value.parse::<f64>().is_err()
        {
            return none;
        }
        stats.push((name.to_string(), value.to_string()));
    }
    if stats.is_empty() {
        return none;
    }
    (trimmed.get(..open).unwrap_or("").trim_end(), stats)
}

/// `rows` is the total over all loops; `rpl` is rows per loop and `est` the
/// planner's estimate of rows per loop.
fn apply_scan_stats(node: &mut PlanNode, stats: &[(String, String)]) {
    for (name, value) in stats {
        let number = value.parse::<f64>().ok().filter(|n| n.is_finite());
        match (name.as_str(), number) {
            ("loops", Some(n)) => node.loops = Some(n),
            ("rows", Some(n)) => {
                node.actual_rows = Some(n);
                node.actual_rows_total = Some(n);
            }
            ("est", Some(n)) => node.estimated_rows = n,
            ("rpl", _) => push_property(node, "Rows per loop", value),
            ("cycles", _) => push_property(node, "CPU cycles", value),
            ("percent", _) => push_property(node, "Share of cycles", &format!("{value}%")),
            _ => {}
        }
    }
}

// ----- raw rows ------------------------------------------------------------

/// Read plan rows from any row layout. Lines that are not plan rows (headers,
/// grid borders, prompts, footers) are skipped.
fn read_rows(doc: &str, lines: &[&str]) -> Vec<Step> {
    if let Some(steps) = read_json_rows(doc) {
        return steps;
    }
    let mut raw: Vec<RawRow> = Vec::new();
    let mut pending = LineModeRow::default();
    for line in lines {
        if let Some(row) = pending.feed(line) {
            raw.push(row);
            continue;
        }
        if let Some(row) = split_row(line) {
            raw.push(row);
        }
    }
    steps_from_raw(raw)
}

/// A row before the columns are interpreted.
struct RawRow {
    id: i64,
    second: i64,
    /// The third column of a four-column row.
    third: Option<i64>,
    detail: String,
}

/// Decide between the current columns (`id, parent, notused, detail`) and
/// the pre-3.24 ones (`selectid, order, from, detail`), then build steps.
///
/// Current ids are bytecode addresses: positive, distinct, and every parent
/// is 0 or an earlier id. The old `selectid` starts at 0 and repeats for
/// every table of one SELECT, and the old `order` column counts 0, 1, 2
/// without naming an earlier row, so any of those marks the old layout.
fn steps_from_raw(raw: Vec<RawRow>) -> Vec<Step> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut legacy = false;
    for row in &raw {
        if row.id <= 0 || (row.second != 0 && !seen.contains(&row.second)) || !seen.insert(row.id) {
            legacy = true;
            break;
        }
    }
    let legacy = legacy && raw.iter().all(|row| row.third.is_some());
    raw.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let (detail, stats) = split_scan_stats(&row.detail);
            let mut step = Step {
                id: row.id,
                parent: row.second,
                detail: detail.to_string(),
                stats,
                properties: Vec::new(),
            };
            if legacy {
                step.properties = vec![
                    property("selectid", &row.id.to_string()),
                    property("order", &row.second.to_string()),
                    property("from", &row.third.unwrap_or(0).to_string()),
                ];
                // Unique negative ids keep `build_plan` from linking rows.
                step.id = -(index as i64) - 1;
                step.parent = 0;
            }
            step
        })
        .collect()
}

/// Split one line into a plan row. Handles `|`, `│` (box mode), tab and
/// comma separators and the space-padded column mode.
fn split_row(line: &str) -> Option<RawRow> {
    let line = line.trim();
    let fields: Vec<String> = if line.contains('\u{2502}') {
        trim_edges(line.split('\u{2502}').collect())
    } else if line.contains('\t') {
        line.split('\t').map(|f| f.trim().to_string()).collect()
    } else if line.contains('|') {
        trim_edges(line.split('|').collect())
    } else if let Some(fields) = split_csv_row(line) {
        fields
    } else {
        // Column mode: three numbers, then the detail.
        let mut rest = line;
        let mut fields = Vec::new();
        for _ in 0..3 {
            let end = rest.find(char::is_whitespace)?;
            fields.push(rest.get(..end)?.to_string());
            rest = rest.get(end..)?.trim_start();
        }
        fields.push(rest.to_string());
        fields
    };
    if fields.len() < 3 {
        return None;
    }
    let id = fields.first()?.parse::<i64>().ok()?;
    let second = fields.get(1)?.parse::<i64>().ok()?;
    let third = if fields.len() >= 4 {
        fields.get(2)?.parse::<i64>().ok()
    } else {
        None
    };
    let detail_from = if third.is_some() { 3 } else { 2 };
    let detail = fields.get(detail_from..)?.join("|").trim().to_string();
    if detail.is_empty() {
        return None;
    }
    Some(RawRow {
        id,
        second,
        third,
        detail,
    })
}

/// Trim the cells; a grid row starts and ends with the separator, which
/// leaves an empty first and last cell to drop.
fn trim_edges(cells: Vec<&str>) -> Vec<String> {
    let mut cells: Vec<String> = cells.into_iter().map(|c| c.trim().to_string()).collect();
    if cells.len() >= 2 && cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
        if cells.last().is_some_and(String::is_empty) {
            cells.pop();
        }
    }
    cells
}

/// `1,0,0,"MERGE (UNION ALL)"`: three numbers, then the detail, quoted when
/// it holds a space or comma.
fn split_csv_row(line: &str) -> Option<Vec<String>> {
    let mut parts = line.splitn(4, ',');
    let fields = [parts.next()?, parts.next()?, parts.next()?, parts.next()?];
    if !fields[..3].iter().all(|f| f.trim().parse::<i64>().is_ok()) {
        return None;
    }
    let detail = fields[3].trim();
    let detail = detail
        .strip_prefix('"')
        .and_then(|d| d.strip_suffix('"'))
        .map(|d| d.replace("\"\"", "\""))
        .unwrap_or_else(|| detail.to_string());
    Some(vec![
        fields[0].trim().to_string(),
        fields[1].trim().to_string(),
        fields[2].trim().to_string(),
        detail,
    ])
}

/// The shell's line mode: one `name = value` line per column, rows separated
/// by blank lines. A row is complete at its `detail` line.
#[derive(Default)]
struct LineModeRow {
    id: Option<i64>,
    second: Option<i64>,
    third: Option<i64>,
}

impl LineModeRow {
    fn feed(&mut self, line: &str) -> Option<RawRow> {
        let (name, value) = line.split_once(" = ")?;
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "id" | "selectid" => {
                *self = Self::default();
                self.id = value.parse().ok();
            }
            "parent" | "order" => self.second = value.parse().ok(),
            "notused" | "from" => self.third = value.parse().ok(),
            "detail" => {
                let row = RawRow {
                    id: self.id?,
                    second: self.second?,
                    third: self.third,
                    detail: value.to_string(),
                };
                *self = Self::default();
                return (!row.detail.is_empty()).then_some(row);
            }
            _ => {}
        }
        None
    }
}

/// Rows exported as JSON: an array of objects with `id`, `parent` and
/// `detail` (the shell's `.mode json` over stored rows, most drivers).
fn read_json_rows(doc: &str) -> Option<Vec<Step>> {
    let text = doc.trim();
    if !text.starts_with('[') && !text.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        object @ serde_json::Value::Object(_) => vec![object],
        _ => return None,
    };
    let mut raw = Vec::new();
    for item in &items {
        let object = item.as_object()?;
        let int = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|name| object.get(*name))
                .and_then(serde_json::Value::as_i64)
        };
        raw.push(RawRow {
            id: int(["id", "selectid"])?,
            second: int(["parent", "order"])?,
            third: int(["notused", "from"]),
            detail: object.get("detail")?.as_str()?.trim().to_string(),
        });
    }
    (!raw.is_empty()).then(|| steps_from_raw(raw))
}

// ----- detail text ---------------------------------------------------------

/// Leading words of every detail SQLite writes, across versions.
const DETAIL_PREFIXES: [&str; 27] = [
    "SCAN ",
    "SEARCH ",
    "USE TEMP B-TREE",
    "CO-ROUTINE",
    "MATERIALIZE",
    "COMPOUND ",
    "LEFT-MOST SUBQUERY",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MERGE (",
    "LEFT",
    "RIGHT",
    "LIST SUBQUERY",
    "SCALAR SUBQUERY",
    "CORRELATED ",
    "REUSE ",
    "MULTI-INDEX OR",
    "INDEX ",
    "BLOOM FILTER ON",
    "CREATE BLOOM FILTER",
    "SETUP",
    "RECURSIVE STEP",
    "EXECUTE ",
    "RIGHT-JOIN",
    "LAST ",
    "SUBQUERY ",
];

fn is_plan_detail(detail: &str) -> bool {
    DETAIL_PREFIXES.iter().any(|prefix| {
        detail
            .strip_prefix(prefix.trim_end())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' ') || prefix.ends_with('('))
    })
}

fn property(name: &str, value: &str) -> PlanProperty {
    PlanProperty {
        name: name.to_string(),
        value: value.to_string(),
    }
}

fn push_property(node: &mut PlanNode, name: &str, value: &str) {
    node.properties.push(property(name, value));
}

/// Fill the typed fields a detail string supports.
fn read_detail(node: &mut PlanNode, detail: &str, subplans: &HashSet<&str>) {
    let detail = detail.trim();
    if let Some(rest) = detail.strip_prefix("SCAN ") {
        read_access(node, rest, false, subplans);
    } else if let Some(rest) = detail.strip_prefix("SEARCH ") {
        read_access(node, rest, true, subplans);
    } else if let Some(rest) = detail.strip_prefix("USE TEMP B-TREE FOR ") {
        push_property(node, "Temp B-tree for", rest.trim());
    } else if let Some(name) = detail
        .strip_prefix("CO-ROUTINE ")
        .or_else(|| detail.strip_prefix("MATERIALIZE "))
    {
        node.subplan_name = Some(name.trim().to_string());
    } else if detail.contains("SUBQUERY") && !detail.starts_with("LEFT-MOST") {
        // `LIST SUBQUERY 2`, `CORRELATED SCALAR SUBQUERY 1`,
        // `EXECUTE CORRELATED SCALAR SUBQUERY 1` (before 3.24).
        node.subplan_name = Some(detail.to_string());
    } else if let Some(rest) = detail.strip_prefix("BLOOM FILTER ON ") {
        let (name, cond) = split_condition(rest);
        if !name.is_empty() {
            node.relation = Some(name.to_string());
            node.output_list.push(name.to_string());
        }
        if let Some(cond) = cond {
            node.filter = Some(cond.to_string());
            node.output_list.push(format!("Filter: {cond}"));
        }
    }
}

/// Split `head (cond)` at the last parenthesised group that ends the text.
fn split_condition(text: &str) -> (&str, Option<&str>) {
    let text = text.trim();
    if text.ends_with(')')
        && let Some(open) = text.find(" (")
        && let Some(cond) = text.get(open + 2..text.len() - 1)
    {
        return (text.get(..open).unwrap_or("").trim(), Some(cond.trim()));
    }
    (text, None)
}

/// `SCAN` / `SEARCH` details:
/// `[TABLE] name [AS alias] [USING <access>] [(cond)] [(~N rows)] [LEFT-JOIN]`.
fn read_access(node: &mut PlanNode, rest: &str, search: bool, subplans: &HashSet<&str>) {
    let mut rest = rest.trim();
    // SQLite 3.39 and later mark the inner table of an outer join.
    if let Some(head) = rest.strip_suffix(" LEFT-JOIN") {
        node.extra.push("LEFT-JOIN".to_string());
        rest = head.trim_end();
    }
    // SQLite 3.7 printed its row estimate: `(~10 rows)`.
    if rest.ends_with(" rows)")
        && let Some(open) = rest.rfind("(~")
        && let Some(count) = rest.get(open + 2..rest.len() - " rows)".len())
        && let Some(rows) = super::common::parse_count(count)
    {
        node.estimated_rows = rows;
        rest = rest.get(..open).unwrap_or("").trim_end();
    }
    let legacy_table = rest.starts_with("TABLE ");
    let rest = rest.strip_prefix("TABLE ").unwrap_or(rest);
    let (target, using) = match rest.find(" USING ") {
        Some(at) => (
            rest.get(..at).unwrap_or(""),
            rest.get(at + " USING ".len()..),
        ),
        None => (rest, None),
    };
    let target = target.trim();
    // `SCAN CONSTANT ROW`, `SCAN 3 CONSTANT ROWS`: a VALUES list, no table.
    let constant = target.ends_with("CONSTANT ROW") || target.ends_with("CONSTANT ROWS");
    // `SCAN SUBQUERY 1 AS s` (before 3.24).
    let subquery = !legacy_table && target.starts_with("SUBQUERY ");
    let (name, alias) = match target.split_once(" AS ") {
        Some((name, alias)) => (name.trim(), Some(alias.trim())),
        None => (target, None),
    };
    // `SCAN t VIRTUAL TABLE INDEX 0:` names the virtual table's own plan.
    let (name, virtual_index) = match name.split_once(" VIRTUAL TABLE INDEX ") {
        Some((name, index)) => (name.trim(), Some(index.trim())),
        None => (name, None),
    };
    if let Some(alias) = alias.filter(|a| !a.is_empty()) {
        node.alias = Some(alias.to_string());
    }
    if constant {
        return;
    }
    if subquery {
        node.subplan_name = Some(name.to_string());
    } else if !name.is_empty() {
        node.relation = Some(name.to_string());
        // Before 3.36 the line read `SCAN TABLE orders AS o`: the real name
        // with the alias after it. Newer versions print the alias alone.
        node.relation_resolved = legacy_table;
        node.output_list.push(name.to_string());
    }
    if let Some(index) = virtual_index {
        push_property(node, "Virtual table index", index);
    }
    let Some(using) = using else {
        // A scan with no index reads the whole table, unless the "table" is
        // an intermediate result defined earlier in this plan.
        let reads_subplan = subplans.contains(name) || alias.is_some_and(|a| subplans.contains(a));
        if !search && !subquery && !name.is_empty() && virtual_index.is_none() && !reads_subplan {
            node.warnings.push(PlanWarning::TableScan {
                table: name.to_string(),
                schema: String::new(),
            });
        }
        return;
    };
    let (access, cond) = split_condition(using);
    if let Some(cond) = cond.filter(|c| !c.is_empty()) {
        node.index_cond = Some(cond.to_string());
        node.output_list.push(format!("Index Cond: {cond}"));
    }
    let index_name = access
        .strip_prefix("COVERING INDEX ")
        .or_else(|| access.strip_prefix("INDEX "))
        .map(str::trim)
        .filter(|n| !n.is_empty());
    if let Some(index) = index_name {
        node.index_name = Some(index.to_string());
        node.output_list.push(format!("Index: {index}"));
    }
    let kind = match index_name {
        Some(index) => access
            .strip_suffix(index)
            .unwrap_or(access)
            .trim()
            .to_string(),
        None => access.to_string(),
    };
    if !kind.is_empty() {
        push_property(node, "Access", &kind);
        // `COVERING INDEX`, `AUTOMATIC COVERING INDEX`, `AUTOMATIC PARTIAL
        // COVERING INDEX`, `INTEGER PRIMARY KEY`, `PRIMARY KEY`: worth a
        // flag when it is not a plain index.
        if kind != "INDEX" {
            node.extra.push(kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::findings_for_plan;

    const Q1_TREE: &str = include_str!("../../../tests/golden/plans/sqlite/sqlite351-q1-tree.txt");
    const Q2_TREE: &str = include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-tree.txt");
    const Q2_PIPE: &str = include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-pipe.txt");
    const Q2_TSV: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-tsv-header.txt");
    const Q2_BOX: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-box.txt");
    const Q2_COLUMN: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-column.txt");
    const Q2_TABLE: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-table.txt");
    const Q2_MARKDOWN: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-markdown.txt");
    const Q2_CSV: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-csv.txt");
    const Q2_JSON: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-json.txt");
    const Q2_LINE: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-line.txt");
    const Q3_TREE: &str = include_str!("../../../tests/golden/plans/sqlite/sqlite351-q3-tree.txt");
    const Q3_BYTECODE: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q3-bytecode.txt");
    const Q1_EQP_ON: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q1-eqp-on-with-results.txt");
    const Q1_SCANSTATS: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q1-scanstats.txt");
    const Q1_SCANSTATS_EST: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q1-scanstats-est.txt");
    const Q4_EQP_FULL: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q4-eqp-full.txt");
    const Q5_SCANSTATS: &str =
        include_str!("../../../tests/golden/plans/sqlite/sqlite351-q5-scanstats.txt");
    const Q8_TREE: &str = include_str!("../../../tests/golden/plans/sqlite/sqlite351-q8-tree.txt");

    /// The `sqlite3` 3.51 shell's output for a join with an IN subquery.
    const SHELL_TREE: &str = "QUERY PLAN\n|--SEARCH o USING INDEX idx_orders_date (order_date>?)\n|--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)\n|--LIST SUBQUERY 1\n|  |--SCAN customers\n|  `--CREATE BLOOM FILTER\n|--USE TEMP B-TREE FOR GROUP BY\n`--USE TEMP B-TREE FOR ORDER BY\n";

    /// The four columns of SQLite before 3.24, as printed in the SQLite
    /// documentation of that era (`selectid|order|from|detail`).
    const LEGACY_ROWS: &str = "0|0|0|SEARCH TABLE t1 USING INDEX i1 (a=?)\n0|1|1|SCAN TABLE t2 AS x\n0|0|0|EXECUTE CORRELATED SCALAR SUBQUERY 1\n1|0|0|SEARCH TABLE t3 USING COVERING INDEX i2 (a=? AND b>?)\n0|0|0|USE TEMP B-TREE FOR ORDER BY\n";

    fn edges(plan: &ExecutionPlan) -> Vec<(i32, i32)> {
        plan.edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect()
    }

    fn prop<'a>(node: &'a PlanNode, name: &str) -> Option<&'a str> {
        node.properties
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value.as_str())
    }

    #[test]
    fn shell_tree_nests_by_indentation() {
        let plan = parse_sqlite_query_plan_any(SHELL_TREE).unwrap();
        assert_eq!(plan.nodes.len(), 7);
        // Both subquery steps hang off LIST SUBQUERY 1 (node 3); the rest are roots.
        assert_eq!(edges(&plan), vec![(3, 4), (3, 5)]);
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
        // The plan is too small for a finding: the rules need measured rows
        // or a meaningful share of the cost before they call out a scan.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn malformed_sqlite_documents_yield_typed_errors_not_panics() {
        assert!(parse_sqlite_query_plan_text("garbage-without-pipes").is_err());
        assert!(parse_sqlite_query_plan_text("").is_err());
        assert!(parse_sqlite_query_plan_any("garbage-without-pipes").is_err());
        assert!(parse_sqlite_query_plan_any("").is_err());
        assert!(parse_sqlite_query_plan_any("QUERY PLAN\n").is_err());
    }

    #[test]
    fn tree_fills_relation_index_and_condition() {
        let plan = parse_sqlite_query_plan_any(Q1_TREE).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::Sqlite));
        assert_eq!(plan.source_format.as_deref(), Some("sqlite-eqp"));
        assert!(!plan.has_actuals);
        assert_eq!(plan.nodes.len(), 5);
        assert!(plan.edges.is_empty());
        let scan = &plan.nodes[0];
        assert_eq!(scan.logical_op, "SCAN");
        assert_eq!(scan.relation.as_deref(), Some("o"));
        assert!(matches!(
            &scan.warnings[..],
            [PlanWarning::TableScan { table, .. }] if table == "o"
        ));
        let by_rowid = &plan.nodes[1];
        assert_eq!(by_rowid.relation.as_deref(), Some("c"));
        assert_eq!(by_rowid.index_name, None);
        assert_eq!(by_rowid.index_cond.as_deref(), Some("rowid=?"));
        assert_eq!(prop(by_rowid, "Access"), Some("INTEGER PRIMARY KEY"));
        let by_index = &plan.nodes[2];
        assert_eq!(by_index.relation.as_deref(), Some("oi"));
        assert_eq!(
            by_index.index_name.as_deref(),
            Some("order_items_order_idx")
        );
        assert_eq!(by_index.index_cond.as_deref(), Some("order_id=?"));
        assert!(
            by_index
                .output_list
                .contains(&"Index: order_items_order_idx".to_string())
        );
        assert!(by_index.warnings.is_empty());
        assert_eq!(prop(&plan.nodes[3], "Temp B-tree for"), Some("GROUP BY"));
        assert_eq!(prop(&plan.nodes[4], "Temp B-tree for"), Some("ORDER BY"));
    }

    #[test]
    fn every_row_layout_gives_the_same_tree() {
        let tree = parse_sqlite_query_plan_any(Q2_TREE).unwrap();
        // The tree also prints `CREATE BLOOM FILTER`, which has no row id
        // in the pipe capture of the same statement taken without it.
        assert_eq!(tree.nodes[0].physical_op, "MERGE (UNION ALL)");
        assert_eq!(tree.nodes.len(), 12);
        let expected_edges = vec![
            (1, 2),
            (2, 3),
            (2, 4),
            (4, 5),
            (2, 6),
            (6, 7),
            (2, 8),
            (1, 9),
            (9, 10),
            (9, 11),
        ];
        for (name, doc) in [
            ("pipe", Q2_PIPE),
            ("tsv", Q2_TSV),
            ("box", Q2_BOX),
            ("column", Q2_COLUMN),
            ("table", Q2_TABLE),
            ("markdown", Q2_MARKDOWN),
            ("csv", Q2_CSV),
            ("json", Q2_JSON),
            ("line", Q2_LINE),
        ] {
            let plan = parse_sqlite_query_plan_any(doc).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(plan.nodes.len(), 11, "{name}");
            assert_eq!(edges(&plan), expected_edges, "{name}");
            assert_eq!(plan.nodes[0].physical_op, "MERGE (UNION ALL)", "{name}");
            assert_eq!(plan.nodes[2].relation.as_deref(), Some("c"), "{name}");
            assert_eq!(
                plan.nodes[6].physical_op,
                "SEARCH o USING INDEX orders_customer_idx (customer_id=?)",
                "{name}"
            );
            assert_eq!(
                plan.nodes[6].index_name.as_deref(),
                Some("orders_customer_idx"),
                "{name}"
            );
            assert_eq!(plan.nodes[9].physical_op, "SCAN customers", "{name}");
            assert_eq!(plan.nodes[9].warnings.len(), 1, "{name}");
            assert!(looks_like_sqlite_query_plan(doc), "{name}");
        }
    }

    #[test]
    fn scans_of_coroutines_and_constant_rows_are_not_table_scans() {
        let plan = parse_sqlite_query_plan_any(Q3_TREE).unwrap();
        assert_eq!(plan.nodes.len(), 8);
        assert_eq!(plan.nodes[0].subplan_name.as_deref(), Some("r"));
        assert_eq!(plan.nodes[2].physical_op, "SCAN CONSTANT ROW");
        assert_eq!(plan.nodes[2].relation, None);
        // `SCAN r` reads the co-routine, twice; neither is a table scan.
        assert!(plan.nodes.iter().all(|n| n.warnings.is_empty()));
        let covering = &plan.nodes[7];
        assert_eq!(covering.relation.as_deref(), Some("orders"));
        assert_eq!(covering.index_name.as_deref(), Some("orders_customer_idx"));
        assert_eq!(covering.extra, vec!["COVERING INDEX".to_string()]);
        assert_eq!(edges(&plan), vec![(1, 2), (2, 3), (1, 4), (4, 5), (7, 8)]);
    }

    #[test]
    fn materialize_bloom_filter_and_automatic_index() {
        let plan = parse_sqlite_query_plan_any(Q8_TREE).unwrap();
        assert_eq!(plan.nodes[0].subplan_name.as_deref(), Some("m"));
        let index_scan = &plan.nodes[1];
        assert_eq!(index_scan.relation.as_deref(), Some("orders"));
        assert_eq!(
            index_scan.index_name.as_deref(),
            Some("orders_customer_idx")
        );
        assert!(index_scan.warnings.is_empty());
        let bloom = &plan.nodes[3];
        assert_eq!(bloom.relation.as_deref(), Some("b"));
        assert_eq!(bloom.filter.as_deref(), Some("t=?"));
        let automatic = &plan.nodes[4];
        assert_eq!(automatic.index_name, None);
        assert_eq!(automatic.index_cond.as_deref(), Some("t=?"));
        assert_eq!(
            automatic.extra,
            vec!["AUTOMATIC COVERING INDEX".to_string()]
        );
    }

    #[test]
    fn result_rows_around_the_tree_are_skipped() {
        let plan = parse_sqlite_query_plan_any(Q1_EQP_ON).unwrap();
        assert_eq!(plan.nodes.len(), 5);
        assert_eq!(plan.nodes[4].physical_op, "USE TEMP B-TREE FOR ORDER BY");
        // `.eqp full` prints the bytecode after the tree.
        let plan = parse_sqlite_query_plan_any(Q4_EQP_FULL).unwrap();
        assert_eq!(plan.nodes[0].physical_op, "COMPOUND QUERY");
        assert_eq!(plan.nodes[2].physical_op, "MULTI-INDEX OR");
        assert_eq!(plan.nodes[3].physical_op, "INDEX 1");
        assert!(plan.nodes.iter().all(|n| !n.physical_op.contains("Init")));
        assert!(looks_like_sqlite_query_plan(Q4_EQP_FULL));
    }

    #[test]
    fn scanstats_fill_loops_and_rows() {
        let plan = parse_sqlite_query_plan_any(Q1_SCANSTATS).unwrap();
        assert!(plan.has_actuals);
        assert_eq!(plan.nodes.len(), 5);
        assert_eq!(plan.nodes[0].physical_op, "SCAN o");
        assert_eq!(plan.nodes[0].loops, Some(1.0));
        assert_eq!(plan.nodes[0].actual_rows, Some(40000.0));
        assert_eq!(plan.nodes[1].loops, Some(10000.0));
        assert_eq!(plan.nodes[1].actual_rows_total, Some(10000.0));
        assert_eq!(plan.nodes[1].index_cond.as_deref(), Some("rowid=?"));
        assert_eq!(plan.nodes[2].actual_rows_total, Some(25000.0));
        assert_eq!(plan.nodes[4].actual_rows, Some(2.0));

        let plan = parse_sqlite_query_plan_any(Q1_SCANSTATS_EST).unwrap();
        assert_eq!(plan.nodes[0].estimated_rows, 9216.0);
        assert_eq!(prop(&plan.nodes[0], "Rows per loop"), Some("40000.0"));
        assert_eq!(plan.nodes[2].estimated_rows, 3.0);
        assert_eq!(prop(&plan.nodes[2], "Rows per loop"), Some("2.5"));
        assert_eq!(plan.nodes[3].loops, Some(1.0));

        // Steps without counters stay without them.
        let plan = parse_sqlite_query_plan_any(Q5_SCANSTATS).unwrap();
        assert_eq!(plan.nodes[0].physical_op, "CO-ROUTINE x");
        assert_eq!(plan.nodes[0].actual_rows, None);
        assert_eq!(plan.nodes[1].actual_rows, Some(100000.0));
        assert_eq!(
            plan.nodes[1].index_name.as_deref(),
            Some("order_items_order_idx")
        );
        assert_eq!(plan.nodes[2].physical_op, "SCAN x");
        assert!(plan.nodes[2].warnings.is_empty());
        assert_eq!(plan.nodes[3].loops, Some(40000.0));
    }

    #[test]
    fn pre_3_24_rows_are_flat_and_spell_scan_table() {
        let plan = parse_sqlite_query_plan_any(LEGACY_ROWS).unwrap();
        assert_eq!(plan.nodes.len(), 5);
        assert!(plan.edges.is_empty());
        assert_eq!(
            plan.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(plan.nodes[0].relation.as_deref(), Some("t1"));
        assert_eq!(plan.nodes[0].index_name.as_deref(), Some("i1"));
        assert_eq!(plan.nodes[1].relation.as_deref(), Some("t2"));
        assert_eq!(plan.nodes[1].alias.as_deref(), Some("x"));
        assert!(matches!(
            &plan.nodes[1].warnings[..],
            [PlanWarning::TableScan { table, .. }] if table == "t2"
        ));
        assert_eq!(prop(&plan.nodes[1], "order"), Some("1"));
        assert_eq!(prop(&plan.nodes[3], "selectid"), Some("1"));
        assert_eq!(plan.nodes[3].index_cond.as_deref(), Some("a=? AND b>?"));
        assert!(looks_like_sqlite_query_plan(LEGACY_ROWS));
        // The 3.7 era appended a row estimate.
        let plan =
            parse_sqlite_query_plan_any("0|0|0|SEARCH TABLE t1 USING INDEX i1 (a=?) (~10 rows)\n")
                .unwrap();
        assert_eq!(plan.nodes[0].estimated_rows, 10.0);
        assert_eq!(plan.nodes[0].index_cond.as_deref(), Some("a=?"));
    }

    #[test]
    fn bytecode_is_rejected_with_a_pointer_to_explain_query_plan() {
        assert!(looks_like_sqlite_bytecode(Q3_BYTECODE));
        assert!(!looks_like_sqlite_query_plan(Q3_BYTECODE));
        let error = parse_sqlite_query_plan_any(Q3_BYTECODE).unwrap_err();
        assert!(error.to_string().contains("EXPLAIN QUERY PLAN"), "{error}");
        // List mode of the same listing.
        let list = "0|Init|0|46|0||0|Start at 46\n1|InitCoroutine|1|23|2||0|r\n";
        assert!(looks_like_sqlite_bytecode(list));
        assert!(parse_sqlite_query_plan_any(list).is_err());
        assert!(!looks_like_sqlite_bytecode(Q1_TREE));
        assert!(!looks_like_sqlite_bytecode(Q2_PIPE));
    }

    #[test]
    fn detector_accepts_plans_and_rejects_other_documents() {
        for doc in [
            Q1_TREE,
            Q2_TREE,
            Q3_TREE,
            Q8_TREE,
            Q1_EQP_ON,
            Q1_SCANSTATS,
            Q5_SCANSTATS,
            SHELL_TREE,
        ] {
            assert!(looks_like_sqlite_query_plan(doc), "{doc}");
        }
        for doc in [
            "",
            "hello",
            "1|2|3|four\n5|6|7|eight\n",
            "US|12500|9337125.0\nBR|12500|9337125.0\n",
            "id\tselect_type\ttable\n1\tSIMPLE\torders\n",
            "Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)\n",
            "-> Table scan on orders  (cost=1.0 rows=10)\n",
            "[{\"Plan\": {\"Node Type\": \"Seq Scan\"}}]",
            "| id | select_type | table |\n| 1 | SIMPLE | t |\n",
            "Expression ((Project names + Projection))\n  Aggregating\n    ReadFromMergeTree (default.orders)\n",
        ] {
            assert!(!looks_like_sqlite_query_plan(doc), "{doc}");
        }
    }

    #[test]
    fn sqlite_parsers_survive_mangled_input() {
        for sample in [
            Q2_TREE,
            Q2_PIPE,
            Q2_TSV,
            Q2_BOX,
            Q2_COLUMN,
            Q2_CSV,
            Q2_JSON,
            Q2_LINE,
            Q1_SCANSTATS_EST,
            Q4_EQP_FULL,
            Q3_BYTECODE,
            LEGACY_ROWS,
        ] {
            super::super::common::assert_survives_mangling(sample, |doc| {
                let _ = parse_sqlite_query_plan_any(doc);
                let _ = parse_sqlite_query_plan_text(doc);
                let _ = looks_like_sqlite_query_plan(doc);
                let _ = looks_like_sqlite_bytecode(doc);
            });
        }
    }
}
