//! MySQL and MariaDB classic tabular `EXPLAIN` (`FORMAT=TRADITIONAL`), and
//! MariaDB tabular `ANALYZE`.
//!
//! The same rows reach us in three client layouts:
//!
//! - the `mysql` / `mariadb` client's ASCII box (`| id | select_type | ...`),
//! - the `\G` vertical form (`*** 1. row ***` then one `name: value` per
//!   line),
//! - batch mode: a tab-separated header line followed by one line per row.
//!
//! One row describes one table access. Rows with the same `id` belong to one
//! SELECT and are listed in join order. Other SELECTs (subqueries, derived
//! tables, UNION members) have their own `id`, and the rows that read their
//! result name them: `<derived2>`, `<subquery3>`, `<union1,2>`.

use super::common;
use crate::plans::dialects::PlanDialect;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;

/// Rows read from one document. More than this is not a plan.
const MAX_ROWS: usize = 5_000;
/// Physical lines glued back into one wrapped table row at most.
const MAX_WRAP_JOINS: usize = 64;

/// Parse a classic tabular `EXPLAIN` result (MySQL `EXPLAIN` /
/// `EXPLAIN FORMAT=TRADITIONAL`, MariaDB `EXPLAIN`, `EXPLAIN EXTENDED`,
/// `EXPLAIN PARTITIONS` and tabular `ANALYZE`) in box, `\G` or tab-separated
/// layout.
///
/// The tree is: one `Query Block` node per SELECT `id`, whose inputs are its
/// table rows in join order. A SELECT that another row reads (`<derivedN>`,
/// `<subqueryN>`) hangs under that row, UNION members hang under their
/// `<unionA,B>` result row, and a subquery with no such reference hangs under
/// the first SELECT, because the table does not say where it is used.
///
/// `estimated_rows` is the `rows` column (rows examined per scan, before the
/// `filtered` percentage), the same meaning the `FORMAT=JSON` parser uses.
/// MariaDB `r_rows` is the average per scan and goes to `actual_rows`; the
/// table has no loop count, so no totals are filled.
pub fn parse_mysql_explain_table(doc: &str) -> PlanResult<ExecutionPlan> {
    let table = read_table(doc).ok_or_else(|| {
        PlanError::InvalidStructure(
            "EXPLAIN table: no header with id, select_type, table and type columns".into(),
        )
    })?;
    if table.rows.is_empty() {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN table: the header has no rows under it".into(),
        ));
    }
    let analyze = table.has("r_rows");
    let rows: Vec<ExplainRow> = table
        .rows
        .iter()
        .map(|cells| ExplainRow::read(&table.columns, cells))
        .collect();
    let dialect = table_dialect(&table, &rows);

    let outline = build_outline(&rows);
    let mut nodes: Vec<PlanNode> = Vec::with_capacity(outline.items.len());
    let mut edges: Vec<PlanEdge> = Vec::new();
    // Preorder walk with an explicit stack, so ids follow tree order.
    let mut stack: Vec<(usize, i32)> = vec![(outline.root, 0)];
    let mut visited = vec![false; outline.items.len()];
    while let Some((index, parent)) = stack.pop() {
        let Some(item) = outline.items.get(index) else {
            continue;
        };
        if std::mem::replace(&mut visited[index], true) {
            continue;
        }
        let id = nodes.len() as i32 + 1;
        let node = match item.kind {
            ItemKind::Block(block) => block_node(id, &outline.blocks[block], &rows),
            ItemKind::Row(row) => row_node(id, &rows[row], analyze),
        };
        nodes.push(node);
        if parent > 0 {
            edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        for child in item.children.iter().rev() {
            stack.push((*child, id));
        }
    }

    let estimated_rows = nodes.iter().map(|n| n.estimated_rows).fold(0.0, f64::max);
    let mut plan = ExecutionPlan {
        statement_type: if analyze { "ANALYZE" } else { "EXPLAIN" }.to_string(),
        nodes,
        edges,
        estimated_rows,
        dialect: Some(dialect),
        source_format: Some(
            match dialect {
                PlanDialect::MariaDb => "mariadb-table",
                _ => "mysql-table",
            }
            .to_string(),
        ),
        has_actuals: analyze,
        ..Default::default()
    };
    common::finish_plan(&mut plan);
    Ok(plan)
}

/// True for a classic tabular `EXPLAIN` result of MySQL or MariaDB in box,
/// `\G` or tab-separated layout: a header with `id`, `select_type`, `table`,
/// `type`, `possible_keys`, `key` and `rows`, and at least one row.
pub fn looks_like_mysql_explain_table(doc: &str) -> bool {
    read_table(doc).is_some_and(|table| {
        !table.rows.is_empty()
            && ["possible_keys", "key", "rows"]
                .iter()
                .all(|name| table.has(name))
    })
}

// ---------------------------------------------------------------------------
// Reading the three layouts into rows of cells
// ---------------------------------------------------------------------------

struct Table {
    /// Lowercased column names.
    columns: Vec<String>,
    /// One vector per row, aligned with `columns`.
    rows: Vec<Vec<String>>,
}

impl Table {
    fn has(&self, name: &str) -> bool {
        self.columns.iter().any(|column| column == name)
    }
}

/// The columns every tabular EXPLAIN has had since MySQL 4.
fn is_header(cells: &[String]) -> bool {
    ["id", "select_type", "table", "type"]
        .iter()
        .all(|name| cells.iter().any(|cell| cell == name))
}

fn header_cells(cells: &[String]) -> Vec<String> {
    cells.iter().map(|cell| cell.to_ascii_lowercase()).collect()
}

fn read_table(doc: &str) -> Option<Table> {
    read_box(doc)
        .or_else(|| read_tab_separated(doc))
        .or_else(|| read_vertical(doc))
}

/// Lines of a boxed table with terminal wrapping undone. A box is a
/// rectangle, so when its first border line is split over several lines
/// (every piece made only of `+` and `-`), the border's full width says how
/// many characters each following row takes.
fn box_lines(doc: &str) -> Vec<String> {
    let raw: Vec<&str> = doc.lines().collect();
    let is_border_piece =
        |line: &str| !line.is_empty() && line.chars().all(|c| c == '+' || c == '-');
    let Some(start) = raw
        .iter()
        .position(|line| line.starts_with('+') && is_border_piece(line.trim_end()))
    else {
        return raw.iter().map(|line| line.to_string()).collect();
    };
    let pieces = raw
        .iter()
        .skip(start)
        .take(MAX_WRAP_JOINS)
        .take_while(|line| is_border_piece(line.trim_end()))
        .count();
    if pieces <= 1 {
        return raw.iter().map(|line| line.to_string()).collect();
    }
    let width: usize = raw
        .iter()
        .skip(start)
        .take(pieces)
        .map(|line| line.trim_end().chars().count())
        .sum();

    let mut out: Vec<String> = raw
        .iter()
        .take(start)
        .map(|line| line.to_string())
        .collect();
    let mut index = start;
    while let Some(first) = raw.get(index) {
        index += 1;
        let mut line = first.to_string();
        if first.starts_with(['|', '+']) {
            let mut joins = 0;
            while line.chars().count() < width && joins < MAX_WRAP_JOINS {
                let Some(next) = raw.get(index) else { break };
                if next.trim().is_empty() {
                    break;
                }
                line.push_str(next);
                index += 1;
                joins += 1;
            }
        }
        out.push(line);
    }
    out
}

fn read_box(doc: &str) -> Option<Table> {
    let mut border: Option<Vec<usize>> = None;
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in box_lines(doc) {
        let text = line.trim();
        if text.starts_with('+') && text.chars().all(|c| c == '+' || c == '-') {
            if columns.is_none() {
                border = Some(
                    text.chars()
                        .enumerate()
                        .filter(|(_, c)| *c == '+')
                        .map(|(position, _)| position)
                        .collect(),
                );
            }
            continue;
        }
        if !text.starts_with('|') {
            if columns.is_some() && !rows.is_empty() {
                break;
            }
            continue;
        }
        let cells = split_box_row(text, border.as_deref());
        let Some(names) = &columns else {
            let lowered = header_cells(&cells);
            if is_header(&lowered) {
                columns = Some(lowered);
            }
            continue;
        };
        // Markdown separator row (`|----|:---|`).
        if cells
            .iter()
            .all(|cell| !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':'))
        {
            continue;
        }
        // The same header again: a second result pasted under the first.
        if header_cells(&cells) == *names {
            break;
        }
        if rows.len() >= MAX_ROWS {
            break;
        }
        rows.push(cells);
    }
    Some(Table {
        columns: columns?,
        rows,
    })
}

/// Cells of one `| a | b |` row. With the border's `+` positions known the
/// row is cut at those columns, which keeps a `|` inside a value (MariaDB
/// prints `ref|filter` and `idx_a|idx_b` for a rowid filter) in its cell.
fn split_box_row(text: &str, border: Option<&[usize]>) -> Vec<String> {
    if let Some(positions) = border.filter(|positions| positions.len() >= 2) {
        let chars: Vec<char> = text.chars().collect();
        if positions.iter().all(|p| chars.get(*p) == Some(&'|')) {
            return positions
                .windows(2)
                .map(|pair| {
                    chars
                        .get(pair[0] + 1..pair[1])
                        .unwrap_or(&[])
                        .iter()
                        .collect::<String>()
                        .trim()
                        .to_string()
                })
                .collect();
        }
    }
    let inner = text.strip_prefix('|').unwrap_or(text);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn read_tab_separated(doc: &str) -> Option<Table> {
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in doc.lines() {
        let cells: Vec<String> = line
            .trim_end_matches('\r')
            .split('\t')
            .map(|cell| cell.trim().to_string())
            .collect();
        let Some(names) = &columns else {
            if cells.len() >= 4 {
                let lowered = header_cells(&cells);
                if is_header(&lowered) {
                    columns = Some(lowered);
                }
            }
            continue;
        };
        if cells.len() < 2 || header_cells(&cells) == *names || rows.len() >= MAX_ROWS {
            break;
        }
        rows.push(cells);
    }
    Some(Table {
        columns: columns?,
        rows,
    })
}

fn read_vertical(doc: &str) -> Option<Table> {
    let mut records: Vec<Vec<(String, String)>> = Vec::new();
    for line in doc.lines() {
        let text = line.trim();
        if text.starts_with("***") && text.to_ascii_lowercase().contains(". row") {
            if records.len() >= MAX_ROWS {
                break;
            }
            records.push(Vec::new());
            continue;
        }
        let Some((name, value)) = text.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // A record pasted without its banner starts at its `id` line.
        let starts_record = match records.last() {
            None => name == "id",
            Some(record) => name == "id" && record.iter().any(|(n, _)| n == "id"),
        };
        if starts_record {
            if records.len() >= MAX_ROWS {
                break;
            }
            records.push(Vec::new());
        }
        if let Some(record) = records.last_mut()
            && !record.iter().any(|(n, _)| *n == name)
        {
            record.push((name, value.trim().to_string()));
        }
    }
    let mut columns: Vec<String> = Vec::new();
    for record in &records {
        for (name, _) in record {
            if !columns.contains(name) {
                columns.push(name.clone());
            }
        }
    }
    if !is_header(&columns) {
        return None;
    }
    let rows = records
        .iter()
        .filter(|record| !record.is_empty())
        .map(|record| {
            columns
                .iter()
                .map(|column| {
                    record
                        .iter()
                        .find(|(name, _)| name == column)
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();
    Some(Table { columns, rows })
}

// ---------------------------------------------------------------------------
// One EXPLAIN row
// ---------------------------------------------------------------------------

/// What a `table` cell in angle brackets refers to.
#[derive(Debug, Clone, PartialEq)]
enum TableRef {
    /// `<derivedN>`: reads the result of SELECT N.
    Derived(u64),
    /// `<subqueryN>`: reads the materialized subquery N.
    Subquery(u64),
    /// `<unionA,B>`, `<intersectA,B>`, `<exceptA,B>`: the combined result of
    /// those SELECTs.
    SetResult(&'static str, Vec<u64>),
}

#[derive(Debug, Default)]
struct ExplainRow {
    id: Option<String>,
    select_type: Option<String>,
    table: Option<String>,
    partitions: Option<String>,
    access: Option<String>,
    possible_keys: Option<String>,
    key: Option<String>,
    key_len: Option<String>,
    reference: Option<String>,
    rows: Option<String>,
    r_rows: Option<String>,
    filtered: Option<String>,
    r_filtered: Option<String>,
    extra: Option<String>,
    /// Columns this parser does not know, as `(name, value)`.
    other: Vec<(String, String)>,
}

impl ExplainRow {
    fn read(columns: &[String], cells: &[String]) -> Self {
        let mut row = ExplainRow::default();
        for (column, cell) in columns.iter().zip(cells) {
            let value = cell.trim();
            // The client prints SQL NULL as the word; MariaDB prints an
            // empty `Extra` as an empty cell.
            if value.is_empty() || value == "NULL" {
                continue;
            }
            let value = value.to_string();
            match column.as_str() {
                "id" => row.id = Some(value),
                "select_type" => row.select_type = Some(value),
                "table" => row.table = Some(value),
                "partitions" => row.partitions = Some(value),
                "type" => row.access = Some(value),
                "possible_keys" => row.possible_keys = Some(value),
                "key" => row.key = Some(value),
                "key_len" => row.key_len = Some(value),
                "ref" => row.reference = Some(value),
                "rows" => row.rows = Some(value),
                "r_rows" => row.r_rows = Some(value),
                "filtered" => row.filtered = Some(value),
                "r_filtered" => row.r_filtered = Some(value),
                "extra" => row.extra = Some(value),
                "" => {}
                _ => row.other.push((column.clone(), value)),
            }
        }
        row
    }

    fn table_ref(&self) -> Option<TableRef> {
        parse_table_ref(self.table.as_deref()?)
    }

    fn set_result(&self) -> Option<(&'static str, Vec<u64>)> {
        match self.table_ref() {
            Some(TableRef::SetResult(kind, members)) => Some((kind, members)),
            _ => None,
        }
    }

    fn extra_entries(&self) -> Vec<String> {
        self.extra
            .as_deref()
            .unwrap_or("")
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Read `<derived2>`, `<subquery3>`, `<union1,2>`. Long unions are printed
/// abbreviated (`<union1,2,3,...,9>`); the gap is filled in.
fn parse_table_ref(name: &str) -> Option<TableRef> {
    let inner = name.trim().strip_prefix('<')?.strip_suffix('>')?;
    let digits_at = inner.find(|c: char| c.is_ascii_digit())?;
    let kind = inner.get(..digits_at)?.to_ascii_lowercase();
    let list = inner.get(digits_at..)?;
    let mut ids: Vec<u64> = Vec::new();
    let mut gap = false;
    for part in list.split(',') {
        let part = part.trim();
        if part == "..." {
            gap = true;
            continue;
        }
        let Ok(id) = part.parse::<u64>() else {
            continue;
        };
        if gap {
            if let Some(last) = ids.last().copied() {
                // Bounded so a hostile `<union1,...,99999999999>` stays small.
                let mut missing = last.saturating_add(1);
                while missing < id && ids.len() < 1_000 {
                    ids.push(missing);
                    missing += 1;
                }
            }
            gap = false;
        }
        if ids.len() < 1_001 {
            ids.push(id);
        }
    }
    let first = *ids.first()?;
    match kind.as_str() {
        "derived" => Some(TableRef::Derived(first)),
        "subquery" => Some(TableRef::Subquery(first)),
        "union" => Some(TableRef::SetResult("Union result", ids)),
        "intersect" => Some(TableRef::SetResult("Intersect result", ids)),
        "except" => Some(TableRef::SetResult("Except result", ids)),
        _ => None,
    }
}

/// MariaDB when the columns or cell contents show it, else MySQL. MySQL 5.7
/// and later always print both `partitions` and `filtered`; MariaDB prints
/// them only for `EXPLAIN PARTITIONS` / `EXPLAIN EXTENDED` / `ANALYZE`, and
/// only MariaDB has `r_rows` / `r_filtered`. A table with neither column and
/// no other hint is read as MariaDB (it could also be MySQL 5.6 or older,
/// which printed the same ten columns).
fn table_dialect(table: &Table, rows: &[ExplainRow]) -> PlanDialect {
    if table.has("r_rows") || table.has("r_filtered") {
        return PlanDialect::MariaDb;
    }
    let mysql_hint = rows.iter().any(|row| {
        let key = row.key.as_deref().unwrap_or("");
        let possible = row.possible_keys.as_deref().unwrap_or("");
        let extra = row.extra.as_deref().unwrap_or("");
        key.starts_with("<auto_")
            || possible.starts_with("<auto_")
            || extra.contains("(hash join)")
            || extra.contains("(Block Nested Loop)")
            || extra.contains("(Batched Key Access)")
            || extra.contains("Backward index scan")
    });
    if mysql_hint {
        return PlanDialect::MySql;
    }
    let mariadb_hint = rows.iter().any(|row| {
        let extra = row.extra.as_deref().unwrap_or("");
        row.access.as_deref().is_some_and(|a| a.contains('|'))
            || extra.contains("rowid filter")
            || extra.contains("BNL join")
            || extra.contains("BNLH join")
            || extra.contains("BKA join")
            || extra.contains("BKAH join")
            || matches!(row.key.as_deref(), Some("distinct_key" | "key0"))
    });
    if mariadb_hint || !(table.has("partitions") && table.has("filtered")) {
        PlanDialect::MariaDb
    } else {
        PlanDialect::MySql
    }
}

// ---------------------------------------------------------------------------
// Tree shape
// ---------------------------------------------------------------------------

/// One SELECT: the rows sharing an `id`.
struct Block {
    select_id: Option<u64>,
    /// Index of the block's first row.
    first_row: usize,
}

#[derive(Clone, Copy)]
enum ItemKind {
    Block(usize),
    Row(usize),
}

struct Item {
    kind: ItemKind,
    parent: Option<usize>,
    children: Vec<usize>,
}

struct Outline {
    blocks: Vec<Block>,
    items: Vec<Item>,
    root: usize,
}

fn build_outline(rows: &[ExplainRow]) -> Outline {
    let mut blocks: Vec<Block> = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    let mut block_items: Vec<usize> = Vec::new();
    let mut row_items: Vec<usize> = Vec::with_capacity(rows.len());
    // Which block each ordinary row belongs to (`None` for set results).
    let mut row_blocks: Vec<Option<usize>> = Vec::with_capacity(rows.len());

    for (index, row) in rows.iter().enumerate() {
        if row.set_result().is_some() {
            row_items.push(items.len());
            row_blocks.push(None);
            items.push(Item {
                kind: ItemKind::Row(index),
                parent: None,
                children: Vec::new(),
            });
            continue;
        }
        let existing = blocks
            .iter()
            .position(|block| rows[block.first_row].id == row.id);
        let block = match existing {
            Some(block) => block,
            None => {
                blocks.push(Block {
                    select_id: row.id.as_deref().and_then(|id| id.parse().ok()),
                    first_row: index,
                });
                block_items.push(items.len());
                items.push(Item {
                    kind: ItemKind::Block(blocks.len() - 1),
                    parent: None,
                    children: Vec::new(),
                });
                blocks.len() - 1
            }
        };
        row_items.push(items.len());
        row_blocks.push(Some(block));
        items.push(Item {
            kind: ItemKind::Row(index),
            parent: Some(block_items[block]),
            children: Vec::new(),
        });
    }

    // Every row a set result: nothing to group, chain them under the first.
    if blocks.is_empty() {
        for item in items.iter_mut().skip(1) {
            item.parent = Some(0);
        }
        link_children(&mut items);
        return Outline {
            blocks,
            items,
            root: 0,
        };
    }

    // The row of another SELECT that reads this block's result.
    let reader_of = |block: usize| -> Option<usize> {
        let id = blocks[block].select_id?;
        rows.iter().enumerate().find_map(|(index, row)| {
            if row_blocks[index] == Some(block) {
                return None;
            }
            match row.table_ref() {
                Some(TableRef::Derived(n) | TableRef::Subquery(n)) if n == id => {
                    Some(row_items[index])
                }
                _ => None,
            }
        })
    };

    let first_block_item = block_items[0];
    let mut root = first_block_item;
    let mut claimed = vec![false; blocks.len()];
    for (index, row) in rows.iter().enumerate() {
        let Some((_, members)) = row.set_result() else {
            continue;
        };
        let result_item = row_items[index];
        let member_blocks: Vec<usize> = members
            .iter()
            .filter_map(|id| blocks.iter().position(|b| b.select_id == Some(*id)))
            .filter(|block| !claimed[*block])
            .collect();
        let parent = match member_blocks.first() {
            Some(0) => None,
            Some(first) => reader_of(*first).or(Some(first_block_item)),
            None => Some(first_block_item),
        };
        if parent.is_none() {
            root = result_item;
        }
        items[result_item].parent = parent;
        for block in member_blocks {
            claimed[block] = true;
            items[block_items[block]].parent = Some(result_item);
        }
    }
    for block in 1..blocks.len() {
        if !claimed[block] {
            items[block_items[block]].parent = reader_of(block).or(Some(first_block_item));
        }
    }

    // A malformed document can make the references circular. Anything that
    // does not reach the root is hung directly under it.
    items[root].parent = None;
    for start in 0..items.len() {
        if start == root {
            continue;
        }
        let mut at = start;
        let mut reached = false;
        for _ in 0..=items.len() {
            match items[at].parent {
                Some(parent) if parent == root => {
                    reached = true;
                    break;
                }
                Some(parent) if parent < items.len() => at = parent,
                _ => break,
            }
        }
        if !reached {
            items[start].parent = Some(root);
        }
    }
    link_children(&mut items);
    Outline {
        blocks,
        items,
        root,
    }
}

fn link_children(items: &mut [Item]) {
    for index in 0..items.len() {
        if let Some(parent) = items[index].parent
            && parent < items.len()
            && parent != index
        {
            items[parent].children.push(index);
        }
    }
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

fn property(name: &str, value: &str) -> PlanProperty {
    PlanProperty {
        name: name.to_string(),
        value: value.to_string(),
    }
}

/// The leading number of a cell such as `8`, `40000.00` or MariaDB's
/// `8 (14%)`.
fn leading_number(cell: &str) -> Option<f64> {
    common::parse_count(cell.split_whitespace().next()?)
}

fn block_node(id: i32, block: &Block, rows: &[ExplainRow]) -> PlanNode {
    let first = rows.get(block.first_row);
    let mut properties = Vec::new();
    if let Some(select_id) = first.and_then(|row| row.id.as_deref()) {
        properties.push(property("select_id", select_id));
    }
    if let Some(select_type) = first.and_then(|row| row.select_type.as_deref()) {
        properties.push(property("select_type", select_type));
    }
    PlanNode {
        id,
        logical_op: "Query Block".to_string(),
        physical_op: "Query Block".to_string(),
        subplan_name: block.select_id.map(|n| format!("Select #{n}")),
        properties,
        ..Default::default()
    }
}

fn row_node(id: i32, row: &ExplainRow, analyze: bool) -> PlanNode {
    let extra = row.extra_entries();
    let set_result = row.set_result();
    let internal = row.table.as_deref().is_some_and(|t| t.starts_with('<'));

    // MariaDB rowid filter: `type` is `ref|filter`, `key` is
    // `lookup_index|filter_index`.
    let (access, rowid_filter) = match row.access.as_deref() {
        Some(access) => match access.split_once('|') {
            Some((main, _)) => (Some(main.trim()), true),
            None => (Some(access), false),
        },
        None => (None, false),
    };
    let (index_name, filter_key) = match row.key.as_deref() {
        Some(key) if rowid_filter => match key.split_once('|') {
            Some((main, filter)) => (Some(main.trim()), Some(filter.trim())),
            None => (Some(key), None),
        },
        other => (other, None),
    };

    let mut node = PlanNode {
        id,
        ..Default::default()
    };
    node.logical_op = match access {
        Some(access) => format!("Access: {access}"),
        None => extra.first().cloned().unwrap_or_else(|| "?".to_string()),
    };
    node.physical_op = match (&set_result, row.table.as_deref()) {
        (Some((kind, _)), _) => (*kind).to_string(),
        (None, Some(table)) => format!("Table: {table}"),
        // `No tables used`, `Select tables optimized away`, ...
        (None, None) => extra
            .first()
            .cloned()
            .unwrap_or_else(|| "No table".to_string()),
    };
    if let Some(table) = row.table.as_deref() {
        node.output_list.push(table.to_string());
        if !internal {
            node.relation = Some(table.to_string());
        }
    }
    if let Some(key) = index_name {
        node.output_list.push(format!("Key: {key}"));
        node.index_name = Some(key.to_string());
    }
    // `ALL` is the full table scan. Reading back an internal result
    // (`<derived2>`, `<union1,2>`) is not a table the user can index.
    if access == Some("ALL")
        && !internal
        && let Some(table) = row.table.as_deref()
    {
        node.warnings.push(PlanWarning::TableScan {
            table: table.to_string(),
            schema: String::new(),
        });
    }

    if let Some(rows) = row.rows.as_deref().and_then(leading_number) {
        node.estimated_rows = rows;
        node.estimated_rows_read = Some(rows);
    }
    node.filtered_percent = row.filtered.as_deref().and_then(leading_number);
    if analyze {
        match row.r_rows.as_deref().and_then(leading_number) {
            Some(r_rows) => node.actual_rows = Some(r_rows),
            // `r_rows` is NULL for a table the execution never reached.
            None if !internal && row.table.is_some() => node.never_executed = true,
            None => {}
        }
    }
    if let Some(partitions) = row.partitions.as_deref() {
        node.partitions = partitions
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
    }

    let mut properties = Vec::new();
    if set_result.is_some() {
        if let Some(select_id) = row.id.as_deref() {
            properties.push(property("select_id", select_id));
        }
        if let Some(select_type) = row.select_type.as_deref() {
            properties.push(property("select_type", select_type));
        }
    }
    if row.access.as_deref() != access
        && let Some(raw) = row.access.as_deref()
    {
        properties.push(property("type", raw));
    }
    if let Some(value) = row.possible_keys.as_deref() {
        properties.push(property("possible_keys", value));
    }
    if let Some(filter_key) = filter_key {
        properties.push(property("rowid_filter_key", filter_key));
    }
    if let Some(value) = row.key_len.as_deref() {
        properties.push(property("key_len", value));
    }
    if let Some(value) = row.reference.as_deref() {
        properties.push(property("ref", value));
    }
    // Kept verbatim when the cell says more than a number (`8 (14%)`: the
    // share of rows the rowid filter is expected to keep).
    for (name, cell) in [("rows", &row.rows), ("r_rows", &row.r_rows)] {
        if let Some(cell) = cell.as_deref()
            && cell.split_whitespace().count() > 1
        {
            properties.push(property(name, cell));
        }
    }
    if let Some(value) = row.r_filtered.as_deref() {
        properties.push(property("r_filtered", value));
    }
    for (name, value) in &row.other {
        properties.push(property(name, value));
    }
    node.properties = properties;
    node.extra = extra;
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQL_JOIN: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-table.txt");
    const MYSQL_JOIN_VERTICAL: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-vertical.txt");
    const MYSQL_JOIN_TSV: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-tsv.txt");
    const MYSQL_SUBQUERIES: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q3-subqueries-table.txt");
    const MYSQL_UNION: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q4-union-table.txt");
    const MYSQL_PARTITIONS: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q5-partitions-table.txt");
    const MYSQL_DELETE: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q7-delete-table.txt");
    const MYSQL_DERIVED: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-derived-table.txt");
    const MYSQL_HASH: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-hash-join-table.txt");
    const MARIA_JOIN: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-q1-join-table.txt");
    const MARIA_JOIN_VERTICAL: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-q1-join-vertical.txt");
    const MARIA_SUBQUERIES_TSV: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-q3-subqueries-tsv.txt");
    const MARIA_EXTENDED: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-q1-join-extended-table.txt");
    const MARIA_ANALYZE: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-q1-join-analyze-table.txt");
    const MARIA_SUBQUERIES_ANALYZE: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-q3-subqueries-analyze-table.txt");
    const MARIA_UNION: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-q4-union-table.txt");
    const MARIA_PARTITIONS: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-partitions-table.txt");
    const MARIA_NEVER: &str = include_str!(
        "../../../tests/golden/plans/mariadb/mariadb-never-executed-analyze-table.txt"
    );
    const MARIA_NEVER_TSV: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-never-executed-analyze-tsv.txt");
    const MARIA_DERIVED_ANALYZE: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-derived-analyze-table.txt");
    const MARIA_ROWID_FILTER: &str =
        include_str!("../../../tests/golden/plans/mariadb/mariadb-rowid-filter-analyze-table.txt");

    fn edges(plan: &ExecutionPlan) -> Vec<(i32, i32)> {
        plan.edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect()
    }

    fn ops(plan: &ExecutionPlan) -> Vec<(&str, &str)> {
        plan.nodes
            .iter()
            .map(|n| (n.physical_op.as_str(), n.logical_op.as_str()))
            .collect()
    }

    #[test]
    fn mysql_box_join_fills_every_column() {
        let plan = parse_mysql_explain_table(MYSQL_JOIN).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MySql));
        assert_eq!(plan.source_format.as_deref(), Some("mysql-table"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(
            ops(&plan),
            vec![
                ("Query Block", "Query Block"),
                ("Table: o", "Access: ALL"),
                ("Table: c", "Access: eq_ref"),
                ("Table: oi", "Access: ref"),
            ]
        );
        assert_eq!(edges(&plan), vec![(1, 2), (1, 3), (1, 4)]);
        assert_eq!(plan.estimated_rows, 40131.0);

        let o = &plan.nodes[1];
        assert_eq!(o.relation.as_deref(), Some("o"));
        assert_eq!(o.estimated_rows, 40131.0);
        assert_eq!(o.estimated_rows_read, Some(40131.0));
        assert_eq!(o.filtered_percent, Some(10.0));
        assert_eq!(o.index_name, None);
        assert_eq!(
            o.extra,
            vec!["Using where", "Using temporary", "Using filesort"]
        );
        assert_eq!(
            o.property("possible_keys"),
            Some("PRIMARY,orders_customer_idx")
        );
        assert_eq!(o.property("key_len"), None, "NULL cells are absent");
        assert!(matches!(
            o.warnings.as_slice(),
            [PlanWarning::TableScan { table, .. }] if table == "o"
        ));
        assert!(o.partitions.is_empty());

        let c = &plan.nodes[2];
        assert_eq!(c.index_name.as_deref(), Some("PRIMARY"));
        assert_eq!(c.property("key_len"), Some("4"));
        assert_eq!(c.property("ref"), Some("shop.o.customer_id"));
        assert_eq!(c.estimated_rows, 1.0);
        assert_eq!(c.filtered_percent, Some(100.0));
        assert!(c.extra.is_empty());
        assert!(c.warnings.is_empty());
        assert_eq!(c.output_list, vec!["c", "Key: PRIMARY"]);

        let oi = &plan.nodes[3];
        assert_eq!(oi.index_name.as_deref(), Some("order_items_order_idx"));
        assert_eq!(oi.estimated_rows, 2.0);

        assert_eq!(plan.nodes[0].property("select_type"), Some("SIMPLE"));
        assert_eq!(plan.nodes[0].subplan_name.as_deref(), Some("Select #1"));
    }

    #[test]
    fn vertical_and_tab_separated_layouts_read_the_same_plan() {
        let boxed = parse_mysql_explain_table(MYSQL_JOIN).unwrap();
        for doc in [MYSQL_JOIN_VERTICAL, MYSQL_JOIN_TSV] {
            let plan = parse_mysql_explain_table(doc).unwrap();
            assert_eq!(plan.dialect, Some(PlanDialect::MySql));
            assert_eq!(ops(&plan), ops(&boxed));
            assert_eq!(edges(&plan), edges(&boxed));
            for (a, b) in plan.nodes.iter().zip(&boxed.nodes) {
                assert_eq!(a.estimated_rows, b.estimated_rows);
                assert_eq!(a.filtered_percent, b.filtered_percent);
                assert_eq!(a.index_name, b.index_name);
                assert_eq!(a.extra, b.extra);
                assert_eq!(a.properties, b.properties);
            }
        }
    }

    #[test]
    fn mysql_subqueries_hang_under_the_rows_that_read_them() {
        let plan = parse_mysql_explain_table(MYSQL_SUBQUERIES).unwrap();
        assert_eq!(
            ops(&plan),
            vec![
                ("Query Block", "Query Block"),
                ("Table: c", "Access: ALL"),
                ("Table: <subquery3>", "Access: eq_ref"),
                ("Query Block", "Query Block"),
                ("Table: orders", "Access: ALL"),
                ("Query Block", "Query Block"),
                ("Table: o", "Access: ref"),
            ]
        );
        // Select 3 (MATERIALIZED) under <subquery3>; select 2 (DEPENDENT
        // SUBQUERY) under the first select.
        assert_eq!(
            edges(&plan),
            vec![(1, 2), (1, 3), (3, 4), (4, 5), (1, 6), (6, 7)]
        );
        assert_eq!(plan.nodes[3].property("select_type"), Some("MATERIALIZED"));
        assert_eq!(plan.nodes[3].subplan_name.as_deref(), Some("Select #3"));
        assert_eq!(
            plan.nodes[5].property("select_type"),
            Some("DEPENDENT SUBQUERY")
        );
        let lookup = &plan.nodes[2];
        assert_eq!(lookup.relation, None);
        assert_eq!(lookup.index_name.as_deref(), Some("<auto_distinct_key>"));
        assert_eq!(lookup.property("ref"), Some("shop.c.id"));
        assert_eq!(plan.nodes[4].filtered_percent, Some(33.33));
        assert_eq!(plan.nodes[4].estimated_rows, 40131.0);
        assert_eq!(plan.nodes[6].estimated_rows, 8.0);
        let scans: Vec<&str> = plan
            .nodes
            .iter()
            .flat_map(|n| n.warnings.iter())
            .filter_map(|w| match w {
                PlanWarning::TableScan { table, .. } => Some(table.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(scans, vec!["c", "orders"]);
    }

    #[test]
    fn mysql_union_result_is_the_root_over_its_members() {
        let plan = parse_mysql_explain_table(MYSQL_UNION).unwrap();
        assert_eq!(
            ops(&plan),
            vec![
                ("Union result", "Access: ALL"),
                ("Query Block", "Query Block"),
                ("Table: customers", "Access: ALL"),
                ("Query Block", "Query Block"),
                ("Table: orders", "Access: ALL"),
            ]
        );
        assert_eq!(edges(&plan), vec![(1, 2), (2, 3), (1, 4), (4, 5)]);
        let result = &plan.nodes[0];
        assert_eq!(result.extra, vec!["Using temporary"]);
        assert_eq!(result.estimated_rows, 0.0);
        assert!(result.warnings.is_empty(), "<union1,2> is not a table scan");
        assert_eq!(result.property("select_type"), Some("UNION RESULT"));
        assert_eq!(plan.nodes[3].property("select_type"), Some("UNION"));
        assert_eq!(plan.nodes[2].filtered_percent, Some(10.0));
    }

    #[test]
    fn mysql_partitions_delete_derived_and_hash_join_rows() {
        let plan = parse_mysql_explain_table(MYSQL_PARTITIONS).unwrap();
        assert_eq!(plan.nodes[1].partitions, vec!["p2025"]);
        assert_eq!(plan.nodes[1].estimated_rows, 9602.0);

        let plan = parse_mysql_explain_table(MYSQL_DELETE).unwrap();
        assert_eq!(plan.nodes[0].property("select_type"), Some("DELETE"));
        assert_eq!(plan.nodes[1].logical_op, "Access: range");
        assert_eq!(plan.nodes[1].property("ref"), Some("const"));
        assert_eq!(plan.nodes[1].estimated_rows, 2.0);

        let plan = parse_mysql_explain_table(MYSQL_DERIVED).unwrap();
        assert_eq!(
            ops(&plan),
            vec![
                ("Query Block", "Query Block"),
                ("Table: c", "Access: ALL"),
                ("Table: <derived2>", "Access: ref"),
                ("Query Block", "Query Block"),
                ("Table: orders", "Access: index"),
            ]
        );
        assert_eq!(edges(&plan), vec![(1, 2), (1, 3), (3, 4), (4, 5)]);
        assert_eq!(plan.nodes[2].index_name.as_deref(), Some("<auto_key0>"));
        assert_eq!(plan.nodes[2].estimated_rows, 10.0);
        assert_eq!(plan.dialect, Some(PlanDialect::MySql));

        let plan = parse_mysql_explain_table(MYSQL_HASH).unwrap();
        assert!(
            plan.nodes
                .iter()
                .any(|n| n.extra.iter().any(|e| e == "Using join buffer (hash join)")),
            "{:?}",
            plan.nodes
        );
    }

    #[test]
    fn mariadb_plain_table_has_no_partitions_or_filtered() {
        let plan = parse_mysql_explain_table(MARIA_JOIN).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(plan.source_format.as_deref(), Some("mariadb-table"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert_eq!(plan.nodes.len(), 4);
        let o = &plan.nodes[1];
        assert_eq!(o.estimated_rows, 40131.0);
        assert_eq!(o.filtered_percent, None);
        assert_eq!(
            o.extra,
            vec!["Using where", "Using temporary", "Using filesort"]
        );
        // MariaDB prints an empty Extra cell, not NULL.
        assert!(plan.nodes[2].extra.is_empty());
        assert_eq!(plan.nodes[2].property("ref"), Some("shop.o.customer_id"));

        let vertical = parse_mysql_explain_table(MARIA_JOIN_VERTICAL).unwrap();
        assert_eq!(vertical.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(ops(&vertical), ops(&plan));
        assert!(vertical.nodes[2].extra.is_empty());

        let extended = parse_mysql_explain_table(MARIA_EXTENDED).unwrap();
        assert_eq!(extended.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(extended.nodes[1].filtered_percent, Some(100.0));
        assert!(!extended.has_actuals);

        let partitions = parse_mysql_explain_table(MARIA_PARTITIONS).unwrap();
        assert_eq!(partitions.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(partitions.nodes[1].partitions, vec!["p2025"]);
        assert_eq!(partitions.nodes[1].estimated_rows, 10107.0);
    }

    #[test]
    fn mariadb_tabular_analyze_fills_actual_rows_per_scan() {
        let plan = parse_mysql_explain_table(MARIA_ANALYZE).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(plan.statement_type, "ANALYZE");
        assert!(plan.has_actuals);
        let o = &plan.nodes[1];
        assert_eq!(o.estimated_rows, 40131.0);
        assert_eq!(o.actual_rows, Some(40000.0));
        assert_eq!(o.filtered_percent, Some(100.0));
        assert_eq!(o.property("r_filtered"), Some("25.00"));
        assert_eq!(o.loops, None, "the table has no loop count");
        assert_eq!(o.actual_rows_total, None);
        assert!(!o.never_executed);
    }

    #[test]
    fn mariadb_subqueries_union_and_derived_shapes() {
        let plan = parse_mysql_explain_table(MARIA_SUBQUERIES_ANALYZE).unwrap();
        assert_eq!(
            edges(&plan),
            vec![(1, 2), (1, 3), (3, 4), (4, 5), (1, 6), (6, 7)]
        );
        let lookup = &plan.nodes[2];
        assert_eq!(lookup.physical_op, "Table: <subquery3>");
        assert_eq!(lookup.index_name.as_deref(), Some("distinct_key"));
        assert_eq!(lookup.property("ref"), Some("func"));
        assert_eq!(lookup.actual_rows, Some(0.07));
        assert_eq!(plan.nodes[4].actual_rows, Some(40000.0));
        assert_eq!(plan.nodes[4].property("r_filtered"), Some("0.99"));
        assert_eq!(plan.nodes[1].extra, vec!["Using filesort"]);

        let tsv = parse_mysql_explain_table(MARIA_SUBQUERIES_TSV).unwrap();
        assert_eq!(tsv.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(edges(&tsv), edges(&plan));
        assert_eq!(tsv.nodes[6].estimated_rows, 8.0);
        assert!(tsv.nodes[6].extra.is_empty());

        // MariaDB prints a NULL id for the UNION RESULT row.
        let union = parse_mysql_explain_table(MARIA_UNION).unwrap();
        assert_eq!(union.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(union.nodes[0].physical_op, "Union result");
        assert_eq!(union.nodes[0].property("select_id"), None);
        assert_eq!(edges(&union), vec![(1, 2), (2, 3), (1, 4), (4, 5)]);
        assert_eq!(union.nodes[2].estimated_rows, 4989.0);
        assert_eq!(union.nodes[4].relation.as_deref(), Some("orders"));

        let derived = parse_mysql_explain_table(MARIA_DERIVED_ANALYZE).unwrap();
        assert_eq!(edges(&derived), vec![(1, 2), (1, 3), (3, 4), (4, 5)]);
        assert_eq!(derived.nodes[2].index_name.as_deref(), Some("key0"));
        assert_eq!(derived.nodes[2].actual_rows, Some(0.0));
        assert_eq!(derived.nodes[1].actual_rows, Some(5000.0));
        assert_eq!(
            derived.nodes[4].extra,
            vec!["Using temporary", "Using filesort"]
        );
    }

    #[test]
    fn mariadb_never_executed_table_and_rowid_filter() {
        for doc in [MARIA_NEVER, MARIA_NEVER_TSV] {
            let plan = parse_mysql_explain_table(doc).unwrap();
            assert_eq!(plan.dialect, Some(PlanDialect::MariaDb));
            let c = &plan.nodes[1];
            assert_eq!(c.actual_rows, Some(0.0));
            assert_eq!(c.extra, vec!["Using index condition"]);
            assert_eq!(c.property("key_len"), Some("322"));
            let o = &plan.nodes[2];
            assert_eq!(o.actual_rows, None);
            assert!(o.never_executed);
            assert_eq!(o.property("r_filtered"), None);
            assert_eq!(o.estimated_rows, 8.0);
        }

        let plan = parse_mysql_explain_table(MARIA_ROWID_FILTER).unwrap();
        let o = &plan.nodes[2];
        assert_eq!(o.logical_op, "Access: ref");
        assert_eq!(o.property("type"), Some("ref|filter"));
        assert_eq!(o.index_name.as_deref(), Some("orders_customer_idx"));
        assert_eq!(o.property("rowid_filter_key"), Some("orders_placed_idx"));
        assert_eq!(o.property("key_len"), Some("4|5"));
        assert_eq!(o.estimated_rows, 8.0);
        assert_eq!(o.property("rows"), Some("8 (14%)"));
        assert_eq!(o.actual_rows, Some(1.09));
        assert_eq!(o.property("r_rows"), Some("1.09 (13%)"));
        assert_eq!(o.filtered_percent, Some(13.56));
        assert_eq!(o.extra, vec!["Using where", "Using rowid filter"]);
        assert_eq!(
            o.property("possible_keys"),
            Some("orders_customer_idx,orders_placed_idx")
        );
    }

    #[test]
    fn junk_around_the_table_is_ignored() {
        let doc = format!(
            "mysql> EXPLAIN SELECT 1;\n\n{MYSQL_JOIN}3 rows in set, 1 warning (0.00 sec)\n\nmysql> "
        );
        let plan = parse_mysql_explain_table(&doc).unwrap();
        assert_eq!(plan.nodes.len(), 4);
        assert!(looks_like_mysql_explain_table(&doc));

        // Trailing blanks stripped and the borders dropped by the paste.
        let ragged: String = MYSQL_JOIN
            .lines()
            .filter(|line| !line.starts_with('+'))
            .map(|line| format!("  {}\n", line.trim_end()))
            .collect();
        let plan = parse_mysql_explain_table(&ragged).unwrap();
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.nodes[2].index_name.as_deref(), Some("PRIMARY"));

        // A terminal that wrapped every line at 80 columns.
        let mut wrapped = String::new();
        for line in MYSQL_JOIN.lines() {
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(80) {
                wrapped.extend(chunk.iter());
                wrapped.push('\n');
            }
        }
        let plan = parse_mysql_explain_table(&wrapped).unwrap();
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(
            plan.nodes[1].extra,
            vec!["Using where", "Using temporary", "Using filesort"]
        );
    }

    #[test]
    fn a_row_without_a_table_keeps_its_extra_text() {
        // The shape MySQL prints for `EXPLAIN SELECT 1`.
        let doc = "\
+----+-------------+-------+------------+------+---------------+------+---------+------+------+----------+----------------+
| id | select_type | table | partitions | type | possible_keys | key  | key_len | ref  | rows | filtered | Extra          |
+----+-------------+-------+------------+------+---------------+------+---------+------+------+----------+----------------+
|  1 | SIMPLE      | NULL  | NULL       | NULL | NULL          | NULL | NULL    | NULL | NULL |     NULL | No tables used |
+----+-------------+-------+------------+------+---------------+------+---------+------+------+----------+----------------+
";
        let plan = parse_mysql_explain_table(doc).unwrap();
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[1].physical_op, "No tables used");
        assert!(plan.nodes[1].warnings.is_empty());
    }

    #[test]
    fn circular_references_still_give_a_tree() {
        let doc = "id\tselect_type\ttable\ttype\tpossible_keys\tkey\tkey_len\tref\trows\tExtra\n\
                   2\tDERIVED\t<derived2>\tALL\tNULL\tNULL\tNULL\tNULL\t5\t\n\
                   3\tDERIVED\t<derived2>\tALL\tNULL\tNULL\tNULL\tNULL\t5\t\n\
                   NULL\tUNION RESULT\t<union2,3,9>\tALL\tNULL\tNULL\tNULL\tNULL\tNULL\t\n";
        let plan = parse_mysql_explain_table(doc).unwrap();
        assert_eq!(plan.nodes.len(), 5);
        assert_eq!(plan.edges.len(), 4);
        assert_eq!(plan.nodes[0].physical_op, "Union result");
    }

    #[test]
    fn table_references_are_read() {
        assert_eq!(parse_table_ref("<derived2>"), Some(TableRef::Derived(2)));
        assert_eq!(
            parse_table_ref("<subquery13>"),
            Some(TableRef::Subquery(13))
        );
        assert_eq!(
            parse_table_ref("<union1,2>"),
            Some(TableRef::SetResult("Union result", vec![1, 2]))
        );
        assert_eq!(
            parse_table_ref("<union1,2,...,5>"),
            Some(TableRef::SetResult("Union result", vec![1, 2, 3, 4, 5]))
        );
        assert_eq!(parse_table_ref("orders"), None);
        assert_eq!(parse_table_ref("<temporary>"), None);
        assert_eq!(parse_table_ref("<union>"), None);
    }

    #[test]
    fn detector_accepts_every_layout_and_rejects_other_documents() {
        for doc in [
            MYSQL_JOIN,
            MYSQL_JOIN_VERTICAL,
            MYSQL_JOIN_TSV,
            MYSQL_SUBQUERIES,
            MYSQL_UNION,
            MYSQL_PARTITIONS,
            MYSQL_DELETE,
            MYSQL_DERIVED,
            MYSQL_HASH,
            MARIA_JOIN,
            MARIA_JOIN_VERTICAL,
            MARIA_SUBQUERIES_TSV,
            MARIA_EXTENDED,
            MARIA_ANALYZE,
            MARIA_SUBQUERIES_ANALYZE,
            MARIA_UNION,
            MARIA_PARTITIONS,
            MARIA_NEVER,
            MARIA_NEVER_TSV,
            MARIA_DERIVED_ANALYZE,
            MARIA_ROWID_FILTER,
        ] {
            assert!(looks_like_mysql_explain_table(doc), "{doc}");
        }
        for doc in [
            "",
            "id select_type table type",
            // The mysql client's box around a TREE plan and around JSON.
            include_str!(
                "../../../tests/golden/plans/mysql/mysql84-q2-range-analyze-tree-boxed.txt"
            ),
            include_str!(
                "../../../tests/golden/plans/mysql/mysql84-q2-range-analyze-tree-vertical.txt"
            ),
            include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-analyze-tree.txt"),
            include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-v1.json"),
            include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-v2.json"),
            include_str!("../../../tests/golden/plans/mariadb/mariadb-q1-join-analyze.json"),
            // PostgreSQL text plan in psql's table.
            "                         QUERY PLAN\n------------------------------------------------------------\n Seq Scan on orders  (cost=0.00..755.00 rows=40000 width=27)\n(1 row)\n",
            // Oracle DBMS_XPLAN table.
            "--------------------------------------------------------------------------\n| Id  | Operation         | Name | Rows  | Bytes | Cost (%CPU)| Time     |\n--------------------------------------------------------------------------\n|   0 | SELECT STATEMENT  |      |     1 |    13 |     3   (0)| 00:00:01 |\n|   1 |  TABLE ACCESS FULL| EMP  |     1 |    13 |     3   (0)| 00:00:01 |\n--------------------------------------------------------------------------\n",
            // SQLite EXPLAIN QUERY PLAN.
            "QUERY PLAN\n|--SCAN o\n`--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)\n",
            // A header with no rows.
            "id\tselect_type\ttable\ttype\tpossible_keys\tkey\tkey_len\tref\trows\tExtra\n",
        ] {
            assert!(!looks_like_mysql_explain_table(doc), "{doc}");
        }
    }

    #[test]
    fn documents_that_are_not_an_explain_table_are_errors() {
        for doc in ["", "not a table", "| a | b |\n| 1 | 2 |\n", "{}"] {
            assert!(parse_mysql_explain_table(doc).is_err(), "{doc}");
        }
    }

    #[test]
    fn mangled_tables_never_panic() {
        for sample in [
            MYSQL_JOIN,
            MYSQL_JOIN_VERTICAL,
            MYSQL_JOIN_TSV,
            MYSQL_SUBQUERIES,
            MYSQL_UNION,
            MYSQL_DERIVED,
            MARIA_SUBQUERIES_ANALYZE,
            MARIA_UNION,
            MARIA_ROWID_FILTER,
            MARIA_NEVER_TSV,
            MARIA_JOIN_VERTICAL,
        ] {
            common::assert_survives_mangling(sample, |doc| {
                let _ = parse_mysql_explain_table(doc);
                let _ = looks_like_mysql_explain_table(doc);
            });
        }
    }
}
