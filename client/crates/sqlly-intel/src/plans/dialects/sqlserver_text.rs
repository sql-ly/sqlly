//! SQL Server text showplans and the `SET STATISTICS IO` / `TIME` messages.
//!
//! Three statements print a plan as rows instead of XML:
//!
//! - `SET SHOWPLAN_TEXT ON`: one `StmtText` column. The statement comes back
//!   in one result set and its operator tree (`|--Operator(arguments)`,
//!   five spaces per level) in the next.
//! - `SET SHOWPLAN_ALL ON`: the same tree plus `StmtId`, `NodeId`, `Parent`,
//!   `PhysicalOp`, `LogicalOp`, `Argument`, `DefinedValues`, `EstimateRows`,
//!   `EstimateIO`, `EstimateCPU`, `AvgRowSize`, `TotalSubtreeCost`,
//!   `OutputList`, `Warnings`, `Type`, `Parallel`, `EstimateExecutions`.
//! - `SET STATISTICS PROFILE ON`: the `SHOWPLAN_ALL` columns after `Rows`
//!   and `Executes`, one result set per executed statement.
//!
//! The rows arrive tab separated (a grid copied from SSMS, with or without
//! the header row) or as fixed-width text (sqlcmd, SSMS "results to text"),
//! where the line of dashes under the header gives the column widths.
//!
//! The messages are read in English only. A server running in another
//! language words them differently and they are left alone.

use super::{PlanDialect, common};
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty, PlanTableIo};
use crate::plans::warning::PlanWarning;

/// What `SET STATISTICS IO ON` and `SET STATISTICS TIME ON` printed around a
/// plan: page reads per table, and the CPU and elapsed time of compiling and
/// executing. Times of several statements are added up; reads of the same
/// table are added up.
#[derive(Debug, Clone, Default)]
pub struct SqlServerMessageStats {
    pub table_io: Vec<PlanTableIo>,
    /// `SQL Server Execution Times: CPU time`.
    pub cpu_time_ms: Option<f64>,
    /// `SQL Server Execution Times: elapsed time`.
    pub elapsed_time_ms: Option<f64>,
    /// `SQL Server parse and compile time: CPU time`.
    pub compile_cpu_ms: Option<f64>,
    /// `SQL Server parse and compile time: elapsed time`.
    pub compile_elapsed_ms: Option<f64>,
}

impl SqlServerMessageStats {
    pub fn is_empty(&self) -> bool {
        self.table_io.is_empty()
            && self.cpu_time_ms.is_none()
            && self.elapsed_time_ms.is_none()
            && self.compile_cpu_ms.is_none()
            && self.compile_elapsed_ms.is_none()
    }

    /// Put the numbers on `plan`: the table reads when the plan has none,
    /// and each time only where the plan does not already carry one (a
    /// ShowPlan XML document has its own, more precise times).
    pub fn apply(&self, plan: &mut ExecutionPlan) {
        if plan.table_io.is_empty() {
            plan.table_io = self.table_io.clone();
        }
        if plan.cpu_time_ms.is_none() {
            plan.cpu_time_ms = self.cpu_time_ms;
        }
        if plan.execution_time_ms.is_none() {
            plan.execution_time_ms = self.elapsed_time_ms;
        }
        if plan.planning_time_ms.is_none() {
            plan.planning_time_ms = self.compile_elapsed_ms;
        }
    }

    fn add(&mut self, event: &Message) {
        let sum = |slot: &mut Option<f64>, value: f64| *slot = Some(slot.unwrap_or(0.0) + value);
        match event {
            Message::Io(io) => match self.table_io.iter_mut().find(|t| t.table == io.table) {
                Some(existing) => {
                    let merge = |slot: &mut Option<f64>, value: Option<f64>| {
                        if let Some(value) = value {
                            *slot = Some(slot.unwrap_or(0.0) + value);
                        }
                    };
                    merge(&mut existing.scan_count, io.scan_count);
                    merge(&mut existing.logical_reads, io.logical_reads);
                    merge(&mut existing.physical_reads, io.physical_reads);
                    merge(&mut existing.read_ahead_reads, io.read_ahead_reads);
                    merge(&mut existing.lob_logical_reads, io.lob_logical_reads);
                    merge(&mut existing.lob_physical_reads, io.lob_physical_reads);
                }
                None => self.table_io.push(io.clone()),
            },
            Message::Compile { cpu, elapsed } => {
                sum(&mut self.compile_cpu_ms, *cpu);
                sum(&mut self.compile_elapsed_ms, *elapsed);
            }
            Message::Execution { cpu, elapsed } => {
                sum(&mut self.cpu_time_ms, *cpu);
                sum(&mut self.elapsed_time_ms, *elapsed);
            }
        }
    }
}

/// Take the `SET STATISTICS IO` / `TIME` message lines out of `doc`. Returns
/// the document without those lines and what they said. Works on any
/// document: the rest may be a text showplan, ShowPlan XML, or nothing.
pub fn extract_sqlserver_message_stats(doc: &str) -> (String, SqlServerMessageStats) {
    let (kept, messages) = scan_messages(doc);
    let mut stats = SqlServerMessageStats::default();
    for (_, message) in &messages {
        stats.add(message);
    }
    (kept.join("\n"), stats)
}

#[derive(Debug, Clone)]
enum Message {
    Io(PlanTableIo),
    Compile { cpu: f64, elapsed: f64 },
    Execution { cpu: f64, elapsed: f64 },
}

/// Split `doc` into the lines that are not statistics messages and the
/// messages, each with the number of kept lines before it.
fn scan_messages(doc: &str) -> (Vec<&str>, Vec<(usize, Message)>) {
    #[derive(Clone, Copy)]
    enum Pending {
        Compile,
        Execution,
    }
    let mut kept = Vec::new();
    let mut messages = Vec::new();
    let mut pending: Option<Pending> = None;
    for line in doc.lines() {
        let trimmed = line.trim();
        if let Some(io) = parse_table_io(trimmed) {
            messages.push((kept.len(), Message::Io(io)));
            pending = None;
            continue;
        }
        let header = [
            ("SQL Server parse and compile time:", Pending::Compile),
            ("SQL Server Execution Times:", Pending::Execution),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| Some((trimmed.strip_prefix(prefix)?, kind)));
        if let Some((rest, kind)) = header {
            pending = Some(kind);
            if let Some((cpu, elapsed)) = parse_times(rest) {
                messages.push((kept.len(), time_message(kind, cpu, elapsed)));
                pending = None;
            }
            continue;
        }
        if let Some(kind) = pending {
            if trimmed.is_empty() {
                continue;
            }
            pending = None;
            if let Some((cpu, elapsed)) = parse_times(trimmed) {
                messages.push((kept.len(), time_message(kind, cpu, elapsed)));
                continue;
            }
        }
        kept.push(line);
    }
    return (kept, messages);

    fn time_message(kind: Pending, cpu: f64, elapsed: f64) -> Message {
        match kind {
            Pending::Compile => Message::Compile { cpu, elapsed },
            Pending::Execution => Message::Execution { cpu, elapsed },
        }
    }
}

/// `CPU time = 41 ms,  elapsed time = 26 ms.`
fn parse_times(text: &str) -> Option<(f64, f64)> {
    let value = |label: &str| -> Option<f64> {
        let rest = text.split_once(label)?.1.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number = rest
            .get(..end)?
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())?;
        rest.get(end..)?
            .trim_start()
            .starts_with("ms")
            .then_some(number)
    };
    Some((value("CPU time")?, value("elapsed time")?))
}

/// `Table 'orders'. Scan count 8, logical reads 5751, physical reads 0, ...`
fn parse_table_io(line: &str) -> Option<PlanTableIo> {
    let rest = line.strip_prefix("Table '")?;
    let (table, counters) = rest.split_once("'. ")?;
    let mut io = PlanTableIo {
        table: table.to_string(),
        ..Default::default()
    };
    let mut seen = false;
    for part in counters.trim_end_matches('.').split(',') {
        let Some((label, value)) = part.trim().rsplit_once(' ') else {
            continue;
        };
        let Some(value) = value.parse::<f64>().ok().filter(|v| v.is_finite()) else {
            continue;
        };
        let slot = match label.trim().to_ascii_lowercase().as_str() {
            "scan count" => &mut io.scan_count,
            "logical reads" => &mut io.logical_reads,
            "physical reads" => &mut io.physical_reads,
            "read-ahead reads" => &mut io.read_ahead_reads,
            "lob logical reads" => &mut io.lob_logical_reads,
            "lob physical reads" => &mut io.lob_physical_reads,
            // Page server and LOB read-ahead counters have no field.
            _ => continue,
        };
        *slot = Some(value);
        seen = true;
    }
    (seen && !io.table.is_empty()).then_some(io)
}

// ---- Columns -------------------------------------------------------------

/// Column names in `SET STATISTICS PROFILE` order. `SET SHOWPLAN_ALL` prints
/// the same list without the first two.
const COLUMNS: [&str; 20] = [
    "Rows",
    "Executes",
    "StmtText",
    "StmtId",
    "NodeId",
    "Parent",
    "PhysicalOp",
    "LogicalOp",
    "Argument",
    "DefinedValues",
    "EstimateRows",
    "EstimateIO",
    "EstimateCPU",
    "AvgRowSize",
    "TotalSubtreeCost",
    "OutputList",
    "Warnings",
    "Type",
    "Parallel",
    "EstimateExecutions",
];
const ROWS: usize = 0;
const EXECUTES: usize = 1;
const STMT_TEXT: usize = 2;
const NODE_ID: usize = 4;
const PARENT: usize = 5;
const PHYSICAL_OP: usize = 6;
const LOGICAL_OP: usize = 7;
const ARGUMENT: usize = 8;
const DEFINED_VALUES: usize = 9;
const ESTIMATE_ROWS: usize = 10;
const ESTIMATE_IO: usize = 11;
const ESTIMATE_CPU: usize = 12;
const AVG_ROW_SIZE: usize = 13;
const TOTAL_SUBTREE_COST: usize = 14;
const OUTPUT_LIST: usize = 15;
const WARNINGS: usize = 16;
const TYPE: usize = 17;
const PARALLEL: usize = 18;
const ESTIMATE_EXECUTIONS: usize = 19;

/// Longest run of physical lines read as one row (a statement text with line
/// breaks splits its row).
const MAX_ROW_LINES: usize = 60;

fn column_index(name: &str) -> Option<usize> {
    COLUMNS.iter().position(|column| *column == name.trim())
}

/// One result row: a statement or an operator.
#[derive(Default)]
struct Row {
    /// Index of the row's first line among the kept lines.
    line: usize,
    cells: [Option<String>; 20],
    /// The row is a bare line, not a row of a recognised column layout.
    plain: bool,
}

impl Row {
    /// The cell, unless it is empty or `NULL`.
    fn get(&self, column: usize) -> Option<&str> {
        self.cells
            .get(column)?
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "NULL")
    }

    fn number(&self, column: usize) -> Option<f64> {
        self.get(column)?
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
    }

    /// The statement text cell with its indentation.
    fn stmt_text(&self) -> &str {
        self.cells
            .get(STMT_TEXT)
            .and_then(|cell| cell.as_deref())
            .unwrap_or_default()
    }

    fn is_operator(&self) -> bool {
        match self.get(TYPE) {
            Some(kind) => kind == "PLAN_ROW",
            None => tree_marker(self.stmt_text()).is_some(),
        }
    }
}

enum Item {
    /// A line that separates result sets (blank, header, `(3 rows affected)`).
    Break,
    Row(Box<Row>),
}

/// Byte offset of the `|--` that starts an operator in a `StmtText` cell,
/// when only tree drawing (`|` and spaces) comes before it.
fn tree_marker(cell: &str) -> Option<usize> {
    let at = cell.find("|--")?;
    cell.get(..at)?
        .chars()
        .all(|c| c == ' ' || c == '|' || c == '\t')
        .then_some(at)
}

fn is_dashes(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '-' || c == ' ' || c == '\t')
}

/// `(3 rows affected)`, `(1 row(s) affected)`.
fn is_rows_affected(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('(')
        && trimmed.ends_with("affected)")
        && trimmed.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
}

/// Lines a client prints around result sets that carry nothing.
fn is_noise(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || is_dashes(line)
        || is_rows_affected(line)
        || trimmed == "StmtText"
        || trimmed.starts_with("Changed database context to")
}

/// The columns of a tab-separated header row, when it is a showplan header.
fn tab_header(line: &str) -> Option<Vec<usize>> {
    if !line.contains('\t') {
        return None;
    }
    let columns: Option<Vec<usize>> = line.split('\t').map(column_index).collect();
    columns.filter(|columns| columns.contains(&STMT_TEXT))
}

/// The columns of a fixed-width header and their character ranges, taken
/// from the line of dashes under it. The last column runs to the end of the
/// line.
fn fixed_header(line: &str, dashes: Option<&&str>) -> Option<Vec<(usize, usize, usize)>> {
    let dashes = dashes?;
    if !is_dashes(dashes) || line.contains('\t') {
        return None;
    }
    let names: Option<Vec<usize>> = line.split_whitespace().map(column_index).collect();
    let names = names?;
    if names.len() < 2 || !names.contains(&STMT_TEXT) {
        return None;
    }
    let mut spans = Vec::new();
    let mut start = None;
    let mut count = 0;
    for (index, c) in dashes.chars().enumerate() {
        count = index + 1;
        match (c == '-', start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                spans.push((begin, index));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        spans.push((begin, count));
    }
    (spans.len() == names.len()).then(|| {
        names
            .into_iter()
            .zip(spans)
            .map(|(column, (begin, end))| (column, begin, end))
            .collect()
    })
}

/// The characters `start..end` of `line` (to the end when `end` is `None`).
fn char_slice(line: &str, start: usize, end: Option<usize>) -> &str {
    let offset = |position: usize| {
        line.char_indices()
            .nth(position)
            .map_or(line.len(), |(index, _)| index)
    };
    let begin = offset(start);
    let finish = end.map_or(line.len(), offset).max(begin);
    line.get(begin..finish).unwrap_or_default()
}

/// A headerless grid row: 18 (`SHOWPLAN_ALL`) or 20 (`STATISTICS PROFILE`)
/// tab-separated fields with whole numbers where the ids are.
fn headerless_columns(line: &str) -> Option<Vec<usize>> {
    let fields: Vec<&str> = line.split('\t').collect();
    let offset = match fields.len() {
        18 => 2,
        20 => 0,
        _ => return None,
    };
    let whole = |column: usize| {
        fields
            .get(column - offset)
            .is_some_and(|field| field.trim().parse::<u64>().is_ok())
    };
    let kind = fields.get(TYPE - offset).map(|field| field.trim())?;
    let typed = !kind.is_empty() && kind != "NULL";
    (whole(NODE_ID) && whole(PARENT) && whole(PARALLEL) && typed).then(|| (offset..20).collect())
}

/// Read every showplan row of the document, in order.
fn read_items(lines: &[&str]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(line) = lines.get(i).copied() else {
            break;
        };
        if let Some(columns) = tab_header(line) {
            items.push(Item::Break);
            i += 1;
            if lines.get(i).is_some_and(|next| is_dashes(next)) {
                i += 1;
            }
            i = read_tab_rows(lines, i, &columns, &mut items);
            items.push(Item::Break);
            continue;
        }
        if let Some(spans) = fixed_header(line, lines.get(i + 1)) {
            items.push(Item::Break);
            i = read_fixed_rows(lines, i + 2, &spans, &mut items);
            items.push(Item::Break);
            continue;
        }
        if let Some(columns) = headerless_columns(line) {
            items.push(Item::Row(Box::new(tab_row(line, i, &columns))));
            i += 1;
            continue;
        }
        if is_noise(line) {
            items.push(Item::Break);
        } else {
            let mut row = Row {
                line: i,
                plain: true,
                ..Default::default()
            };
            if let Some(cell) = row.cells.get_mut(STMT_TEXT) {
                *cell = Some(line.trim_end().to_string());
            }
            items.push(Item::Row(Box::new(row)));
        }
        i += 1;
    }
    items
}

fn tab_row(text: &str, line: usize, columns: &[usize]) -> Row {
    let mut row = Row {
        line,
        ..Default::default()
    };
    for (column, field) in columns.iter().zip(text.split('\t')) {
        if let Some(cell) = row.cells.get_mut(*column) {
            *cell = Some(field.to_string());
        }
    }
    row
}

/// Rows under a tab-separated header, up to the end of the result set.
/// Returns the index of the first line after them.
fn read_tab_rows(lines: &[&str], mut i: usize, columns: &[usize], items: &mut Vec<Item>) -> usize {
    while let Some(line) = lines.get(i).copied() {
        if line.trim().is_empty() || is_rows_affected(line) || tab_header(line).is_some() {
            break;
        }
        let fields = |text: &str| text.split('\t').count();
        let mut text = line.to_string();
        let mut used = 1;
        if fields(&text) < columns.len() {
            // A statement text with line breaks: the row continues below.
            let mut joined = text.clone();
            for extra in 1..MAX_ROW_LINES {
                let Some(next) = lines.get(i + extra) else {
                    break;
                };
                if is_rows_affected(next) || tab_header(next).is_some() {
                    break;
                }
                joined.push('\n');
                joined.push_str(next);
                if fields(&joined) >= columns.len() {
                    text = joined;
                    used = extra + 1;
                    break;
                }
            }
        }
        items.push(Item::Row(Box::new(tab_row(&text, i, columns))));
        i += used;
    }
    i
}

/// Rows under a fixed-width header, up to the end of the result set.
/// Returns the index of the first line after them.
fn read_fixed_rows(
    lines: &[&str],
    mut i: usize,
    spans: &[(usize, usize, usize)],
    items: &mut Vec<Item>,
) -> usize {
    // A complete row reaches the last column.
    let needed = spans.last().map_or(0, |(_, begin, _)| *begin);
    while let Some(line) = lines.get(i).copied() {
        if line.trim().is_empty()
            || is_rows_affected(line)
            || fixed_header(line, lines.get(i + 1)).is_some()
        {
            break;
        }
        let mut text = line.to_string();
        let mut used = 1;
        if text.chars().count() <= needed {
            // The statement text holds a line break, which the client
            // printed as one: the row continues on the next line, with the
            // break counted as one character of the column.
            let mut joined = text.clone();
            for extra in 1..MAX_ROW_LINES {
                let Some(next) = lines.get(i + extra) else {
                    break;
                };
                if is_rows_affected(next) || fixed_header(next, lines.get(i + extra + 1)).is_some()
                {
                    break;
                }
                joined.push('\n');
                joined.push_str(next);
                if joined.chars().count() > needed {
                    text = joined;
                    used = extra + 1;
                    break;
                }
            }
        }
        let mut row = Row {
            line: i,
            ..Default::default()
        };
        for (index, (column, begin, end)) in spans.iter().enumerate() {
            let last = index + 1 == spans.len();
            let value = char_slice(&text, *begin, (!last).then_some(*end));
            if let Some(cell) = row.cells.get_mut(*column) {
                *cell = Some(if *column == STMT_TEXT {
                    value.trim_end().to_string()
                } else {
                    value.trim().to_string()
                });
            }
        }
        items.push(Item::Row(Box::new(row)));
        i += used;
    }
    i
}

// ---- Rows to plans -------------------------------------------------------

/// A statement and the operator rows under it.
#[derive(Default)]
struct Draft {
    statement: String,
    kind: Option<String>,
    cost: Option<f64>,
    estimated_rows: Option<f64>,
    /// Kept-line index where the statement starts.
    line: usize,
    operators: Vec<Row>,
}

fn group_statements(items: Vec<Item>) -> Vec<Draft> {
    let mut drafts: Vec<Draft> = Vec::new();
    // Bare lines since the last operator tree: the statement text of the
    // tree that follows. A result-set break ends the paragraph, and the next
    // bare line starts a new one.
    let mut paragraph: Vec<(usize, String)> = Vec::new();
    let mut paragraph_closed = false;
    // The last row added an operator to the last draft.
    let mut in_tree = false;
    for item in items {
        let row = match item {
            Item::Break => {
                paragraph_closed = !paragraph.is_empty();
                continue;
            }
            Item::Row(row) => *row,
        };
        if row.is_operator() {
            let fresh_tree = row.plain
                && (!paragraph.is_empty()
                    || !in_tree && drafts.last().is_none_or(|d| !d.operators.is_empty()));
            if fresh_tree || drafts.is_empty() {
                let (line, statement) = statement_of(&paragraph);
                drafts.push(Draft {
                    kind: statement_keyword(&statement),
                    statement,
                    line: line.unwrap_or(row.line),
                    ..Default::default()
                });
                paragraph.clear();
                paragraph_closed = false;
            }
            if let Some(draft) = drafts.last_mut() {
                draft.operators.push(row);
            }
            in_tree = true;
        } else if row.plain {
            if paragraph_closed {
                paragraph.clear();
                paragraph_closed = false;
            }
            paragraph.push((row.line, row.stmt_text().to_string()));
            in_tree = false;
        } else {
            drafts.push(Draft {
                statement: row.stmt_text().trim().to_string(),
                kind: row.get(TYPE).map(str::to_string),
                cost: row.number(TOTAL_SUBTREE_COST),
                estimated_rows: row.number(ESTIMATE_ROWS),
                line: row.line,
                operators: Vec::new(),
            });
            paragraph.clear();
            in_tree = false;
        }
    }
    drafts.retain(|draft| !draft.operators.is_empty());
    drafts
}

/// The statement a paragraph of bare lines names. `SET SHOWPLAN_TEXT` lists
/// the body of a scalar function (`UDF: [db].[dbo].[fn]` and its indented
/// statements) in the same result set as the next statement; the function
/// body is not that statement.
fn statement_of(paragraph: &[(usize, String)]) -> (Option<usize>, String) {
    let indent = |text: &str| text.len() - text.trim_start().len();
    let mut start = 0;
    let mut udf_indent = None;
    for (index, (_, text)) in paragraph.iter().enumerate() {
        if text.trim_start().starts_with("UDF:") {
            udf_indent = Some(indent(text));
            start = index + 1;
        } else if let Some(level) = udf_indent {
            if indent(text) > level {
                start = index + 1;
            } else {
                udf_indent = None;
            }
        }
    }
    let lines = paragraph.get(start..).unwrap_or_default();
    let text = lines
        .iter()
        .map(|(_, text)| text.trim())
        .collect::<Vec<_>>()
        .join("\n");
    (lines.first().map(|(line, _)| *line), text)
}

/// `SELECT`, `INSERT` ... from the first word of a statement.
fn statement_keyword(statement: &str) -> Option<String> {
    let word = statement.split_whitespace().next()?.to_ascii_uppercase();
    matches!(
        word.as_str(),
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "WITH" | "IF" | "EXEC" | "EXECUTE"
    )
    .then(|| match word.as_str() {
        "WITH" => "SELECT".to_string(),
        "IF" => "COND WITH QUERY".to_string(),
        "EXEC" | "EXECUTE" => "EXECUTE".to_string(),
        _ => word,
    })
}

/// One labelled part of an operator's argument text: `SEEK:(...)`.
struct Argument {
    label: String,
    value: String,
}

const ARGUMENT_LABELS: [&str; 14] = [
    "OUTER REFERENCES",
    "PARTITION COLUMNS",
    "TOP EXPRESSION",
    "ORDER BY",
    "GROUP BY",
    "RESIDUAL",
    "PASSTHRU",
    "OBJECT",
    "DEFINE",
    "VALUES",
    "WHERE",
    "MERGE",
    "SEEK",
    "HASH",
];

/// Split an operator's argument text into its `LABEL:(value)` parts and the
/// bare words between them (`Inner Join`, `ORDERED FORWARD`, `LOOKUP`).
/// Text cut off mid-way (a client truncating a wide column) keeps what is
/// there.
fn split_arguments(text: &str) -> (Vec<Argument>, Vec<String>) {
    let mut arguments = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    let push_flags = |words: &str, flags: &mut Vec<String>| {
        flags.extend(
            words
                .split(',')
                .map(str::trim)
                .filter(|word| !word.is_empty())
                .map(str::to_string),
        );
    };
    let mut word_start = 0;
    let mut group_start = None;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut bracketed = false;
    let mut label = String::new();
    let bytes = text.as_bytes();
    for (index, c) in text.char_indices() {
        if quoted {
            quoted = c != '\'';
            continue;
        }
        if bracketed {
            bracketed = c != ']';
            continue;
        }
        match c {
            '\'' => quoted = true,
            '[' => bracketed = true,
            '(' => {
                if depth == 0 && group_start.is_none() {
                    let before = text.get(word_start..index).unwrap_or_default();
                    let (words, name) = split_label(before);
                    push_flags(words, &mut flags);
                    label = name.to_string();
                    group_start = Some(index);
                }
                depth += 1;
            }
            ')' if depth > 0 => {
                depth -= 1;
                // `HASH:(a)=(b)` is one value.
                let continues =
                    bytes.get(index + 1) == Some(&b'=') && bytes.get(index + 2) == Some(&b'(');
                if depth == 0 && !continues {
                    if let Some(begin) = group_start.take() {
                        let value = text.get(begin..=index).unwrap_or_default();
                        arguments.push(Argument {
                            label: std::mem::take(&mut label),
                            value: strip_outer_parentheses(value).to_string(),
                        });
                    }
                    word_start = index + 1;
                }
            }
            _ => {}
        }
    }
    match group_start {
        // Cut off inside a value.
        Some(begin) => arguments.push(Argument {
            label,
            value: text.get(begin + 1..).unwrap_or_default().trim().to_string(),
        }),
        None => push_flags(text.get(word_start..).unwrap_or_default(), &mut flags),
    }
    (arguments, flags)
}

/// Split the text before a `(` into the bare words and the label the
/// parenthesis belongs to: `Inner Join, OUTER REFERENCES:` gives
/// (`Inner Join,`, `OUTER REFERENCES`).
fn split_label(before: &str) -> (&str, &str) {
    let trimmed = before.trim_end();
    let Some(body) = trimmed.strip_suffix(':') else {
        // A function call or expression, not a labelled argument.
        return (trimmed, "");
    };
    for known in ARGUMENT_LABELS {
        if let Some(words) = body.strip_suffix(known)
            && (words.is_empty() || words.ends_with([' ', ',']))
        {
            return (words, known);
        }
    }
    match body.rfind(',') {
        Some(comma) => (
            body.get(..comma).unwrap_or_default(),
            body.get(comma + 1..).unwrap_or_default().trim(),
        ),
        None => ("", body.trim()),
    }
}

/// `(a)` gives `a`; `(a)=(b)` and unbalanced text come back unchanged.
fn strip_outer_parentheses(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(inner) = trimmed
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return trimmed;
    };
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return trimmed;
                }
            }
            _ => {}
        }
    }
    if depth == 0 { inner.trim() } else { trimmed }
}

/// Split at `separator` outside parentheses, brackets and string literals.
fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut bracketed = false;
    for (index, c) in text.char_indices() {
        if quoted {
            quoted = c != '\'';
        } else if bracketed {
            bracketed = c != ']';
        } else {
            match c {
                '\'' => quoted = true,
                '[' => bracketed = true,
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                c if c == separator && depth == 0 => {
                    parts.push(text.get(start..index).unwrap_or_default().trim());
                    start = index + c.len_utf8();
                }
                _ => {}
            }
        }
    }
    parts.push(text.get(start..).unwrap_or_default().trim());
    parts.retain(|part| !part.is_empty());
    parts
}

/// The parts of a bracketed multi-part name: `[Shop].[dbo].[orders]` gives
/// `Shop`, `dbo`, `orders`. `None` when the text is not just a name.
fn name_parts(text: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    loop {
        let (part, after) = if let Some(inner) = rest.strip_prefix('[') {
            let end = inner.find(']')?;
            (inner.get(..end)?, inner.get(end + 1..)?)
        } else {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$')))
                .unwrap_or(rest.len());
            (rest.get(..end)?, rest.get(end..)?)
        };
        if part.is_empty() {
            return None;
        }
        parts.push(part.to_string());
        match after.strip_prefix('.') {
            Some(next) => rest = next,
            None if after.is_empty() => return Some(parts),
            None => return None,
        }
    }
}

/// A column reference the way the XML parser labels it: `alias.column` or
/// `table.column`. `[Shop].[dbo].[orders].[total] as [o].[total]` gives
/// `o.total`. Anything that is not a plain reference comes back unchanged.
fn column_label(text: &str) -> String {
    let trimmed = text.trim();
    let reference = trimmed
        .rsplit_once(" as ")
        .map_or(trimmed, |(_, alias)| alias);
    match name_parts(reference) {
        Some(parts) => {
            let from = parts.len().saturating_sub(2);
            parts.get(from..).unwrap_or_default().join(".")
        }
        None => trimmed.to_string(),
    }
}

fn column_labels(text: &str) -> Vec<String> {
    split_top_level(text, ',')
        .into_iter()
        .map(column_label)
        .collect()
}

/// `([a].[x])=([b].[y])` gives `a.x = b.y`.
fn key_pairs(value: &str) -> Option<String> {
    let (left, right) = value.split_once(")=(")?;
    let left = column_labels(left.trim_start().strip_prefix('(')?);
    let right = column_labels(right.trim_end().strip_suffix(')')?);
    (!left.is_empty() && left.len() == right.len()).then(|| {
        left.iter()
            .zip(&right)
            .map(|(l, r)| format!("{l} = {r}"))
            .collect::<Vec<_>>()
            .join(" AND ")
    })
}

fn push_property(properties: &mut Vec<PlanProperty>, name: &str, value: &str) {
    if !value.trim().is_empty() {
        properties.push(PlanProperty {
            name: name.to_string(),
            value: value.trim().to_string(),
        });
    }
}

/// A flag that is an operator's logical name in `SHOWPLAN_TEXT`
/// (`Inner Join`, `Gather Streams`, `Lazy Spool`): words in title case.
fn is_logical_name(flag: &str) -> bool {
    flag.chars().all(|c| c.is_ascii_alphabetic() || c == ' ')
        && flag.starts_with(|c: char| c.is_ascii_uppercase())
        && flag.contains(|c: char| c.is_ascii_lowercase())
}

fn build_node(row: &Row, id: i32) -> PlanNode {
    let cell = row.stmt_text();
    let tree_text = tree_marker(cell)
        .and_then(|at| cell.get(at + 3..))
        .map(str::trim);
    let (text_name, text_arguments) = match tree_text {
        Some(text) => match text.split_once('(') {
            Some((name, rest)) => (
                name.trim(),
                Some(rest.trim_end().strip_suffix(')').unwrap_or(rest)),
            ),
            None => (text, None),
        },
        None => ("", None),
    };
    let physical_op = row.get(PHYSICAL_OP).unwrap_or(text_name).trim().to_string();
    let from_column = row.get(ARGUMENT);
    let (arguments, mut flags) =
        split_arguments(from_column.or(text_arguments).unwrap_or_default());
    let logical_op = match row.get(LOGICAL_OP) {
        Some(logical) => logical.to_string(),
        None => match flags.first().filter(|flag| is_logical_name(flag)).cloned() {
            Some(logical) => {
                flags.remove(0);
                logical
            }
            None => physical_op.clone(),
        },
    };
    let is_join = logical_op.ends_with("Join");

    let mut node = PlanNode {
        id,
        join_type: is_join.then(|| logical_op.trim_end_matches("Join").trim().to_string()),
        logical_op,
        physical_op,
        estimated_cost: row.number(TOTAL_SUBTREE_COST).unwrap_or_default(),
        estimated_cpu: row.number(ESTIMATE_CPU).unwrap_or_default(),
        estimated_io: row.number(ESTIMATE_IO).unwrap_or_default(),
        estimated_row_width: row.number(AVG_ROW_SIZE),
        estimated_executions: row.number(ESTIMATE_EXECUTIONS),
        parallel: row.number(PARALLEL).is_some_and(|flag| flag != 0.0),
        ..Default::default()
    };

    let mut index = String::new();
    let mut residual = None;
    let mut lookup = false;
    for argument in &arguments {
        let value = argument.value.as_str();
        match argument.label.as_str() {
            "OBJECT" if node.relation.is_none() => {
                let (name, alias) = match value.rsplit_once(" AS ") {
                    Some((name, alias)) => (name, name_parts(alias)),
                    None => (value, None),
                };
                let mut parts = name_parts(name).unwrap_or_else(|| vec![name.trim().to_string()]);
                // `[db].[schema].[table].[index]`; a heap has no index part.
                if parts.len() >= 4 {
                    index = parts.pop().unwrap_or_default();
                }
                node.relation = parts.pop();
                node.schema = parts.pop();
                node.database = parts.pop();
                node.alias = alias.and_then(|mut alias| alias.pop());
                if !index.is_empty() {
                    node.index_name = Some(index.clone());
                }
            }
            "SEEK" => node.index_cond = Some(value.to_string()),
            "WHERE" if is_join => node.join_predicate = Some(value.to_string()),
            "WHERE" => node.filter = Some(value.to_string()),
            "ORDER BY" => {
                node.sort_keys = split_top_level(value, ',')
                    .into_iter()
                    .map(|key| match key.strip_suffix(" DESC") {
                        Some(column) => format!("{} DESC", column_label(column)),
                        None => column_label(key.strip_suffix(" ASC").unwrap_or(key)),
                    })
                    .collect();
            }
            "GROUP BY" => node.group_keys = column_labels(value),
            "HASH" | "MERGE" if is_join => match key_pairs(value) {
                Some(pairs) => node.join_predicate = Some(pairs),
                None => push_property(&mut node.properties, &argument.label, value),
            },
            "HASH" if node.logical_op.contains("Aggregate") => {
                node.group_keys = column_labels(value);
            }
            "RESIDUAL" => residual = Some(value.to_string()),
            label => {
                let name = if label.is_empty() { "Argument" } else { label };
                push_property(&mut node.properties, name, value);
            }
        }
    }
    if let Some(residual) = residual {
        if is_join && node.join_predicate.is_none() {
            node.join_predicate = Some(residual);
        } else {
            push_property(&mut node.properties, "RESIDUAL", &residual);
        }
    }
    for flag in &flags {
        lookup |= flag.split_whitespace().any(|word| word == "LOOKUP");
    }
    node.extra = flags;
    if node
        .relation
        .as_deref()
        .is_some_and(|name| name.starts_with('@'))
    {
        push_property(&mut node.properties, "TableVariable", "true");
    }
    if let Some(defined) = row.get(DEFINED_VALUES) {
        push_property(&mut node.properties, "Defined Values", defined);
    }
    for (column, name) in [(NODE_ID, "NodeId"), (ESTIMATE_ROWS, "EstimateRows")] {
        if let Some(value) = row.get(column) {
            push_property(&mut node.properties, name, value);
        }
    }
    if let Some(list) = row.get(OUTPUT_LIST) {
        node.output_list = column_labels(list);
        node.output_columns = node.output_list.clone();
    }

    // ---- Estimates and actuals -------------------------------------------
    let per_execution = row.number(ESTIMATE_ROWS).unwrap_or_default();
    let executes = row.number(EXECUTES);
    let rows = row.number(ROWS);
    // A serial Compute Scalar is not instrumented: its zero rows and zero
    // executions say nothing.
    let measured =
        rows.is_some() && !(node.physical_op == "Compute Scalar" && executes == Some(0.0));
    if measured {
        node.actual_rows = rows;
        node.actual_rows_total = rows;
        node.row_count = rows.map(|count| count as i32);
        node.loops = executes;
        node.never_executed = executes == Some(0.0);
        // As in the XML parser: on an executed plan the estimate is compared
        // with the rows of every execution.
        node.estimated_rows = per_execution * node.estimated_executions.unwrap_or(1.0).max(1.0);
    } else {
        node.estimated_rows = per_execution;
    }

    // ---- Warnings --------------------------------------------------------
    let table = node.relation.clone().unwrap_or_default();
    let filtering_scan = matches!(
        node.physical_op.as_str(),
        "Clustered Index Scan" | "Index Scan"
    ) && node.filter.is_some();
    if node.physical_op == "Table Scan" || filtering_scan {
        node.warnings.push(PlanWarning::TableScan {
            table: table.clone(),
            schema: node.schema.clone().unwrap_or_default(),
        });
    }
    if lookup || matches!(node.physical_op.as_str(), "Key Lookup" | "RID Lookup") {
        node.warnings.push(PlanWarning::KeyLookup { table, index });
    }
    if matches!(node.physical_op.as_str(), "Table Spool" | "Index Spool") {
        node.warnings.push(PlanWarning::Spool);
    }
    if let Some(warnings) = row.get(WARNINGS) {
        if warnings.contains("NO JOIN PREDICATE") {
            node.warnings.push(PlanWarning::NoJoinPredicate);
        }
        if let Some((_, columns)) = warnings.split_once("NO STATS:") {
            let (stats, _) = split_arguments(&format!("NO STATS:{columns}"));
            node.columns_without_statistics = stats
                .first()
                .map(|argument| column_labels(&argument.value))
                .unwrap_or_default();
            node.warnings.push(PlanWarning::CardinalityEstimation);
        }
        push_property(&mut node.properties, "Warnings", warnings);
    }
    node
}

fn build_plan(draft: Draft) -> ExecutionPlan {
    let mut nodes = Vec::with_capacity(draft.operators.len());
    let mut edges = Vec::new();
    // NodeId of each operator row, to resolve `Parent`.
    let mut by_node_id: std::collections::HashMap<u64, i32> = std::collections::HashMap::new();
    // Open ancestors by the column their `|--` sits in.
    let mut stack: Vec<(usize, i32)> = Vec::new();
    for (index, row) in draft.operators.iter().enumerate() {
        let id = index as i32 + 1;
        let whole = |column: usize| row.get(column).and_then(|value| value.parse::<u64>().ok());
        let marker = tree_marker(row.stmt_text());
        let by_parent = whole(PARENT).and_then(|parent| by_node_id.get(&parent).copied());
        let by_indent = marker.and_then(|at| {
            while stack.last().is_some_and(|(open, _)| *open >= at) {
                stack.pop();
            }
            stack.last().map(|(_, parent)| *parent)
        });
        if let Some(at) = marker {
            stack.push((at, id));
        }
        // Without ids and without a tree position, hang the operator under
        // the first one so the plan stays one tree.
        let parent = by_parent.or(by_indent).or((index > 0).then_some(1));
        if let Some(parent) = parent.filter(|parent| *parent != id) {
            edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        if let Some(node_id) = whole(NODE_ID) {
            by_node_id.entry(node_id).or_insert(id);
        }
        nodes.push(build_node(row, id));
    }
    let warnings = nodes
        .iter()
        .flat_map(|node: &PlanNode| node.warnings.clone())
        .collect();
    let mut plan = ExecutionPlan {
        statement_text: draft.statement,
        statement_type: draft.kind.unwrap_or_default(),
        nodes,
        edges,
        total_subtree_cost: draft.cost.unwrap_or_default(),
        estimated_rows: draft.estimated_rows.unwrap_or_default(),
        warnings,
        dialect: Some(PlanDialect::SqlServer),
        source_format: Some("sqlserver-text".to_string()),
        ..Default::default()
    };
    common::finish_plan(&mut plan);
    plan
}

/// Give each plan the statistics messages printed for its statement. sqlcmd
/// prints, per statement: compile time, the table reads, the plan rows (with
/// `STATISTICS PROFILE`), then the execution times. So compile times and
/// reads belong to the next plan and execution times to the previous one.
/// With one plan in the document everything is its own.
fn attribute_messages(
    plans: &mut [ExecutionPlan],
    starts: &[usize],
    messages: &[(usize, Message)],
) {
    if messages.is_empty() || plans.is_empty() {
        return;
    }
    let mut stats: Vec<SqlServerMessageStats> = vec![Default::default(); plans.len()];
    let single = plans.len() == 1;
    let executions_after_first = messages.iter().any(|(at, message)| {
        matches!(message, Message::Execution { .. }) && starts.first().is_some_and(|s| at > s)
    });
    for (at, message) in messages {
        let target = match message {
            Message::Execution { .. } => match starts.iter().rposition(|start| start < at) {
                Some(plan) => Some(plan),
                // Times printed before any plan are a `SET` statement's,
                // unless the messages were pasted ahead of a single plan.
                None => (single && !executions_after_first).then_some(0),
            },
            _ => starts
                .iter()
                .position(|start| start >= at)
                .or(single.then_some(0)),
        };
        if let Some(slot) = target.and_then(|plan| stats.get_mut(plan)) {
            slot.add(message);
        }
    }
    for (plan, stats) in plans.iter_mut().zip(&stats) {
        if !stats.is_empty() {
            stats.apply(plan);
            plan.has_actuals |= plan.execution_time_ms.is_some();
        }
    }
}

/// Parse the output of `SET SHOWPLAN_TEXT ON`, `SET SHOWPLAN_ALL ON` or
/// `SET STATISTICS PROFILE ON`: one plan per statement that has operators.
/// `SET STATISTICS IO` / `TIME` messages in the document are read too and
/// put on the plan of their statement.
pub fn parse_sqlserver_showplan_text(doc: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let (lines, messages) = scan_messages(doc);
    let drafts = group_statements(read_items(&lines));
    if drafts.is_empty() {
        return Err(PlanError::InvalidStructure(
            "no showplan operator rows (lines starting with |--) found".to_string(),
        ));
    }
    let starts: Vec<usize> = drafts.iter().map(|draft| draft.line).collect();
    let mut plans: Vec<ExecutionPlan> = drafts.into_iter().map(build_plan).collect();
    attribute_messages(&mut plans, &starts, &messages);
    Ok(plans)
}

/// Physical operators as text showplans name them.
const OPERATORS: [&str; 58] = [
    "Adaptive Join",
    "Assert",
    "Bitmap",
    "Clustered Index Delete",
    "Clustered Index Insert",
    "Clustered Index Merge",
    "Clustered Index Scan",
    "Clustered Index Seek",
    "Clustered Index Update",
    "Collapse",
    "Columnstore Index Delete",
    "Columnstore Index Insert",
    "Columnstore Index Scan",
    "Columnstore Index Update",
    "Compute Scalar",
    "Concatenation",
    "Constant Scan",
    "Deleted Scan",
    "Filter",
    "Foreign Key References Check",
    "Hash Match",
    "Index Delete",
    "Index Insert",
    "Index Scan",
    "Index Seek",
    "Index Spool",
    "Index Update",
    "Inserted Scan",
    "Key Lookup",
    "Log Row Scan",
    "Merge Interval",
    "Merge Join",
    "Nested Loops",
    "Online Index Insert",
    "Parallelism",
    "Parameter Table Scan",
    "RID Lookup",
    "Remote Delete",
    "Remote Insert",
    "Remote Query",
    "Remote Scan",
    "Remote Update",
    "Row Count Spool",
    "Segment",
    "Sequence",
    "Sequence Project",
    "Sort",
    "Split",
    "Stream Aggregate",
    "Switch",
    "Table Delete",
    "Table Insert",
    "Table Scan",
    "Table Spool",
    "Table Update",
    "Table-valued function",
    "Top",
    "Window Aggregate",
];

/// True for a text showplan: operator lines `|--Name(...)` whose names are
/// SQL Server physical operators. SQLite's `EXPLAIN QUERY PLAN` tree also
/// draws `|--`, with upper-case steps (`|--SCAN orders`), which this
/// rejects. A document holding only `STATISTICS IO` / `TIME` messages is not
/// a plan.
pub fn looks_like_sqlserver_showplan_text(doc: &str) -> bool {
    let mut known = 0usize;
    let mut foreign = 0usize;
    for line in doc.lines() {
        let Some(at) = line.find("|--") else {
            continue;
        };
        // Tree drawing, and in grid rows the Rows / Executes cells.
        let leading = line.get(..at).unwrap_or_default();
        if !leading
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ' ' | '\t' | '|'))
        {
            continue;
        }
        let rest = line.get(at + 3..).unwrap_or_default();
        let end = rest
            .find(['(', '\t'])
            .or_else(|| rest.find("  "))
            .unwrap_or(rest.len());
        let name = rest.get(..end).unwrap_or_default().trim();
        if OPERATORS.contains(&name) {
            known += 1;
        } else {
            foreign += 1;
        }
    }
    known > 0 && known >= foreign
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT_MULTI: &str =
        include_str!("../../../tests/golden/plans/sqlserver/q7-showplan-text.txt");
    const TEXT_HEADERLESS: &str =
        include_str!("../../../tests/golden/plans/sqlserver/q1-showplan-text-headerless.txt");
    const ALL_FIXED_MULTI: &str =
        include_str!("../../../tests/golden/plans/sqlserver/q7-showplan-all.txt");
    const ALL_FIXED_CROSS_JOIN: &str = include_str!(
        "../../../tests/golden/plans/sqlserver/q11-showplan-all-no-join-predicate.txt"
    );
    const ALL_TSV_NO_STATS: &str =
        include_str!("../../../tests/golden/plans/sqlserver/q12-showplan-all-no-stats.tsv");
    const PROFILE_TSV_PARALLEL: &str =
        include_str!("../../../tests/golden/plans/sqlserver/q4-profile-parallel.tsv");
    const PROFILE_FIXED_IO: &str =
        include_str!("../../../tests/golden/plans/sqlserver/q2-io-time-profile.txt");

    fn parse(doc: &str) -> Vec<ExecutionPlan> {
        parse_sqlserver_showplan_text(doc).expect("showplan text parses")
    }

    fn children(plan: &ExecutionPlan, id: i32) -> Vec<i32> {
        common::child_ids(&plan.edges, id)
    }

    #[test]
    fn showplan_text_gives_one_plan_per_statement_with_a_tree() {
        let plans = parse(TEXT_MULTI);
        assert_eq!(plans.len(), 5);
        let insert = &plans[0];
        assert_eq!(
            insert.statement_text,
            "DECLARE @t TABLE (id INT PRIMARY KEY, v INT);\nINSERT @t SELECT TOP (2000) order_id, customer_id FROM dbo.orders;"
        );
        assert_eq!(insert.source_format.as_deref(), Some("sqlserver-text"));
        assert_eq!(insert.dialect, Some(PlanDialect::SqlServer));
        assert!(!insert.has_actuals);
        let ops: Vec<&str> = insert
            .nodes
            .iter()
            .map(|n| n.physical_op.as_str())
            .collect();
        assert_eq!(
            ops,
            ["Clustered Index Insert", "Top", "Clustered Index Scan"]
        );
        assert_eq!(insert.nodes[0].relation.as_deref(), Some("@t"));
        assert!(
            insert.nodes[0]
                .properties
                .iter()
                .any(|p| p.name == "TableVariable")
        );
        assert_eq!(children(insert, 1), [2]);
        assert_eq!(children(insert, 2), [3]);
        let scan = &insert.nodes[2];
        assert_eq!(scan.database.as_deref(), Some("Shop"));
        assert_eq!(scan.schema.as_deref(), Some("dbo"));
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(
            scan.index_name.as_deref(),
            Some("PK__orders__46596229A6DBB338")
        );
        assert_eq!(scan.extra, ["ORDERED FORWARD"]);

        let join = &plans[1];
        assert_eq!(
            join.statement_text,
            "SELECT COUNT(*) FROM @t t JOIN dbo.orders o ON o.order_id = t.id;"
        );
        assert_eq!(join.statement_type, "SELECT");
        let hash = &join.nodes[2];
        assert_eq!(hash.physical_op, "Hash Match");
        assert_eq!(hash.logical_op, "Inner Join");
        assert_eq!(hash.join_type.as_deref(), Some("Inner"));
        assert_eq!(hash.join_predicate.as_deref(), Some("t.id = o.order_id"));
        assert_eq!(children(join, 3), [4, 5]);
        assert_eq!(join.nodes[3].relation.as_deref(), Some("@t"));
        assert_eq!(join.nodes[3].alias.as_deref(), Some("t"));
        assert_eq!(join.nodes[4].alias.as_deref(), Some("o"));

        // The function body listed after the third statement is not part of
        // the IF statement's text.
        assert_eq!(
            plans[3].statement_text,
            "IF EXISTS (SELECT 1 FROM dbo.customers WHERE country = 'JP')"
        );
        let semi = &plans[3].nodes[1];
        assert_eq!(semi.logical_op, "Left Semi Join");
        assert_eq!(children(&plans[3], 2), [3, 4]);
        let filtered = &plans[3].nodes[3];
        assert_eq!(
            filtered.filter.as_deref(),
            Some("[Shop].[dbo].[customers].[country]='JP'")
        );
        assert!(
            filtered
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::TableScan { table, .. } if table == "customers"))
        );
        // The statement under the IF is indented, tree and all.
        assert_eq!(
            plans[4].statement_text,
            "SELECT COUNT(*) FROM dbo.customers;"
        );
        assert_eq!(plans[4].nodes.len(), 3);
        assert_eq!(children(&plans[4], 1), [2]);
    }

    #[test]
    fn headerless_showplan_text_reads_the_tree_drawing() {
        let plans = parse(TEXT_HEADERLESS);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert!(
            plan.statement_text
                .starts_with("SELECT c.country, COUNT(*)")
        );
        assert_eq!(plan.nodes.len(), 12);
        assert_eq!(plan.nodes[0].physical_op, "Sort");
        assert_eq!(plan.nodes[0].sort_keys, ["Expr1003 DESC"]);
        assert_eq!(plan.nodes[2].group_keys, ["c.country"]);
        assert_eq!(plan.nodes[4].physical_op, "Parallelism");
        assert_eq!(plan.nodes[4].logical_op, "Gather Streams");
        // The join's two inputs: the bitmap branch drawn with `|` and the
        // repartition below it.
        let join = &plan.nodes[6];
        assert_eq!(join.logical_op, "Inner Join");
        assert_eq!(children(plan, 7), [8, 11]);
        assert_eq!(plan.nodes[7].physical_op, "Bitmap");
        assert_eq!(children(plan, 8), [9]);
        assert_eq!(children(plan, 9), [10]);
        assert_eq!(children(plan, 11), [12]);
    }

    #[test]
    fn showplan_all_fixed_width_reads_columns_and_statements() {
        let plans = parse(ALL_FIXED_MULTI);
        assert_eq!(plans.len(), 5);
        let insert = &plans[0];
        assert_eq!(insert.statement_type, "INSERT");
        assert!(insert.statement_text.starts_with("DECLARE @t TABLE"));
        assert!(insert.statement_text.ends_with("FROM dbo.orders;"));
        assert_eq!(insert.nodes.len(), 3);
        assert_eq!(insert.nodes[0].logical_op, "Insert");
        assert_eq!(insert.nodes[1].physical_op, "Top");
        assert_eq!(children(insert, 1), [2]);

        let join = &plans[1];
        assert_eq!(join.statement_type, "SELECT");
        assert_eq!(join.nodes[1].logical_op, "Aggregate");
        assert_eq!(
            join.nodes[2].join_predicate.as_deref(),
            Some("t.id = o.order_id")
        );
        assert_eq!(children(join, 3), [4, 5]);
        assert!(join.total_subtree_cost > 0.0);
        assert_eq!(join.total_subtree_cost, join.nodes[0].estimated_cost);

        assert_eq!(plans[3].statement_type, "COND WITH QUERY");
        assert_eq!(
            plans[4].statement_text,
            "SELECT COUNT(*) FROM dbo.customers;"
        );
        assert_eq!(plans[4].nodes.len(), 3);
        assert_eq!(plans[4].nodes[2].physical_op, "Index Scan");
        assert_eq!(
            plans[4].nodes[2].index_name.as_deref(),
            Some("IX_customers_email")
        );
    }

    #[test]
    fn showplan_all_reports_missing_join_predicates() {
        let plans = parse(ALL_FIXED_CROSS_JOIN);
        assert_eq!(plans.len(), 2);
        let cross = &plans[1];
        let joins: Vec<&PlanNode> = cross
            .nodes
            .iter()
            .filter(|n| {
                n.warnings
                    .iter()
                    .any(|w| matches!(w, PlanWarning::NoJoinPredicate))
            })
            .collect();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].physical_op, "Nested Loops");
        assert!(
            cross
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::NoJoinPredicate))
        );
    }

    #[test]
    fn showplan_all_tab_separated_reads_estimates_and_missing_statistics() {
        let plans = parse(ALL_TSV_NO_STATS);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.statement_type, "SELECT");
        let scan = plan
            .nodes
            .iter()
            .find(|n| n.relation.as_deref() == Some("nostats2"))
            .expect("the heap scan");
        assert_eq!(scan.physical_op, "Table Scan");
        assert_eq!(scan.columns_without_statistics, ["nostats2.grp"]);
        assert!(
            scan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::CardinalityEstimation))
        );
        assert!(
            scan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::TableScan { .. }))
        );
        assert!(scan.estimated_rows > 0.0);
        assert!(scan.estimated_cost > 0.0);
        assert!(scan.estimated_row_width.is_some());
        assert_eq!(scan.estimated_executions, Some(1.0));
        assert!(scan.actual_rows.is_none());
        assert!(!plan.has_actuals);
        assert!(!scan.output_list.is_empty());
    }

    #[test]
    fn statistics_profile_tab_separated_reads_actual_rows_and_executions() {
        let plans = parse(PROFILE_TSV_PARALLEL);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert!(plan.has_actuals);
        assert_eq!(plan.nodes.len(), 7);
        let gather = &plan.nodes[0];
        assert_eq!(gather.physical_op, "Parallelism");
        assert_eq!(gather.logical_op, "Gather Streams");
        assert_eq!(gather.actual_rows, Some(600.0));
        assert_eq!(gather.loops, Some(1.0));
        assert!(gather.parallel);
        let join = &plan.nodes[3];
        assert_eq!(join.logical_op, "Inner Join");
        assert_eq!(join.actual_rows_total, Some(360_000.0));
        assert_eq!(join.loops, Some(7.0));
        assert_eq!(children(plan, 4), [5, 6]);
        assert_eq!(children(plan, 6), [7]);
        assert_eq!(plan.nodes[4].actual_rows, Some(180_000.0));
        assert_eq!(plan.nodes[2].group_keys, ["oi.product_id"]);
    }

    #[test]
    fn statistics_profile_fixed_width_takes_io_and_time_messages() {
        let plans = parse(PROFILE_FIXED_IO);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.nodes.len(), 3);
        let seek = &plan.nodes[1];
        assert_eq!(seek.physical_op, "Index Seek");
        assert_eq!(seek.index_name.as_deref(), Some("IX_orders_customer"));
        assert_eq!(seek.index_cond.as_deref(), Some("[o].[customer_id]=(4242)"));
        assert_eq!(seek.actual_rows, Some(6.0));
        assert_eq!(seek.loops, Some(1.0));
        let lookup = &plan.nodes[2];
        assert_eq!(lookup.actual_rows, Some(6.0));
        assert_eq!(lookup.loops, Some(6.0));
        // One row per execution, six executions estimated.
        assert_eq!(lookup.estimated_executions, Some(6.0));
        assert_eq!(lookup.estimated_rows, 6.0);
        assert!(
            lookup
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::KeyLookup { .. }))
        );
        assert_eq!(lookup.output_list, ["o.status", "o.total"]);
        assert_eq!(plan.table_io.len(), 1);
        assert_eq!(plan.table_io[0].table, "orders");
        assert_eq!(plan.table_io[0].scan_count, Some(1.0));
        assert_eq!(plan.table_io[0].logical_reads, Some(21.0));
        assert!(plan.execution_time_ms.is_some());
        assert!(plan.cpu_time_ms.is_some());
    }

    #[test]
    fn message_stats_are_extracted_and_removed() {
        let doc = "SQL Server parse and compile time: \n   CPU time = 3 ms, elapsed time = 4 ms.\n<ShowPlanXML/>\nTable 'orders'. Scan count 8, logical reads 5751, physical reads 1, page server reads 0, read-ahead reads 2, page server read-ahead reads 0, lob logical reads 3, lob physical reads 4, lob page server reads 0, lob read-ahead reads 0, lob page server read-ahead reads 0.\n\n SQL Server Execution Times:\n   CPU time = 41 ms,  elapsed time = 26 ms.\n";
        let (rest, stats) = extract_sqlserver_message_stats(doc);
        assert_eq!(rest.trim(), "<ShowPlanXML/>");
        assert!(!stats.is_empty());
        assert_eq!(stats.compile_cpu_ms, Some(3.0));
        assert_eq!(stats.compile_elapsed_ms, Some(4.0));
        assert_eq!(stats.cpu_time_ms, Some(41.0));
        assert_eq!(stats.elapsed_time_ms, Some(26.0));
        let io = &stats.table_io[0];
        assert_eq!(io.table, "orders");
        assert_eq!(io.scan_count, Some(8.0));
        assert_eq!(io.logical_reads, Some(5751.0));
        assert_eq!(io.physical_reads, Some(1.0));
        assert_eq!(io.read_ahead_reads, Some(2.0));
        assert_eq!(io.lob_logical_reads, Some(3.0));
        assert_eq!(io.lob_physical_reads, Some(4.0));

        let mut plan = ExecutionPlan {
            execution_time_ms: Some(1.5),
            ..Default::default()
        };
        stats.apply(&mut plan);
        assert_eq!(plan.execution_time_ms, Some(1.5));
        assert_eq!(plan.cpu_time_ms, Some(41.0));
        assert_eq!(plan.planning_time_ms, Some(4.0));
        assert_eq!(plan.table_io.len(), 1);

        let (untouched, none) = extract_sqlserver_message_stats("SELECT 1;\n  |--Constant Scan");
        assert_eq!(untouched, "SELECT 1;\n  |--Constant Scan");
        assert!(none.is_empty());
    }

    #[test]
    fn detector_accepts_showplans_and_rejects_everything_else() {
        for doc in [
            TEXT_MULTI,
            TEXT_HEADERLESS,
            ALL_FIXED_MULTI,
            ALL_FIXED_CROSS_JOIN,
            ALL_TSV_NO_STATS,
            PROFILE_TSV_PARALLEL,
            PROFILE_FIXED_IO,
        ] {
            assert!(looks_like_sqlserver_showplan_text(doc));
        }
        for doc in [
            "",
            "QUERY PLAN\n|--SCAN orders\n`--SEARCH customers USING INDEX ix (id=?)",
            "Table 'orders'. Scan count 1, logical reads 21, physical reads 0.",
            "Seq Scan on orders  (cost=0.00..18.50 rows=850 width=4)",
            "-> Nested loop inner join  (cost=1.2 rows=3)",
            "<ShowPlanXML xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/showplan\"/>",
            "| Id  | Operation         | Name |\n|   0 | SELECT STATEMENT  |      |",
        ] {
            assert!(!looks_like_sqlserver_showplan_text(doc), "{doc}");
        }
        assert!(parse_sqlserver_showplan_text("SELECT 1").is_err());
        assert!(parse_sqlserver_showplan_text("").is_err());
    }

    #[test]
    fn truncated_arguments_keep_what_is_there() {
        let plans = parse(
            "SELECT 1\n  |--Clustered Index Scan(OBJECT:([Shop].[dbo].[orders].[PK_orders] AS [o]), WHERE:([Shop].[dbo].[orders].[total] as [o].[total]>(1200.0",
        );
        let scan = &plans[0].nodes[0];
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(scan.alias.as_deref(), Some("o"));
        assert_eq!(
            scan.filter.as_deref(),
            Some("[Shop].[dbo].[orders].[total] as [o].[total]>(1200.0")
        );
    }

    /// A run of digits too long for an f64 parses to infinity; such a time
    /// line is not a time.
    #[test]
    fn a_time_too_large_for_a_number_is_not_read_as_infinity() {
        assert_eq!(
            parse_times("CPU time = 41 ms,  elapsed time = 26 ms."),
            Some((41.0, 26.0))
        );
        let huge = "9".repeat(400);
        assert_eq!(
            parse_times(&format!("CPU time = {huge} ms,  elapsed time = 26 ms.")),
            None
        );
        assert_eq!(
            parse_times(&format!("CPU time = 41 ms,  elapsed time = {huge} ms.")),
            None
        );
    }

    #[test]
    fn mangled_documents_never_panic() {
        for sample in [
            TEXT_MULTI,
            TEXT_HEADERLESS,
            ALL_FIXED_CROSS_JOIN,
            ALL_TSV_NO_STATS,
            PROFILE_FIXED_IO,
        ] {
            common::assert_survives_mangling(sample, |doc| {
                let _ = parse_sqlserver_showplan_text(doc);
                let _ = looks_like_sqlserver_showplan_text(doc);
                let _ = extract_sqlserver_message_stats(doc);
            });
        }
    }
}
