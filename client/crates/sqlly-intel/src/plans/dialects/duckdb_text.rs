//! DuckDB: the box-drawing text of `EXPLAIN` and `EXPLAIN ANALYZE`.
//!
//! The CLI and most clients print the plan as a grid of boxes, one per
//! operator, joined by `┬` / `┴` connectors. A box holds the operator name,
//! a rule, then labelled lines (`Table: orders`, `Filters:` followed by one
//! predicate per line), wrapped to the box width, and at the bottom the
//! cardinality: `~80,000 rows` (estimate) in `EXPLAIN`, `300,000 rows` and
//! the operator's own time `(0.07s)` in `EXPLAIN ANALYZE`. DuckDB 1.1 wrote
//! `Rows` without thousands separators and printed a scan's table as a bare
//! line.
//!
//! Title boxes (a box inside a box) name what follows: `Physical Plan`,
//! `Unoptimized Logical Plan` and `Optimized Logical Plan` for `EXPLAIN`
//! (all three with `SET explain_output = 'all'`), `Query Profiling
//! Information` and `Total Time: 0.0198s` for `EXPLAIN ANALYZE`. When
//! several trees are present the physical plan is the one parsed.

use super::duckdb::{
    MAX_DEPTH, Operator, WRAPPER_OPERATORS, finish_duckdb_plan, operator_node, strip_explain_prefix,
};
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};

/// Documents larger than this many lines are not plans anyone pasted whole;
/// the limit keeps the grid scan bounded.
const MAX_LINES: usize = 200_000;

/// Parse the text form of a DuckDB `EXPLAIN` or `EXPLAIN ANALYZE` plan.
pub fn parse_duckdb_explain_text(doc: &str) -> PlanResult<ExecutionPlan> {
    let document = Document::read(doc);
    let Some(tree) = document.plan_tree() else {
        return Err(PlanError::InvalidStructure(unparsed_reason(doc).into()));
    };
    let mut builder = Builder::default();
    for root in &tree.roots {
        builder.visit(&tree, *root, None, 0)?;
    }
    if builder.nodes.is_empty() {
        return Err(PlanError::InvalidStructure(
            "DuckDB EXPLAIN: no operator boxes found".into(),
        ));
    }
    let mut plan = ExecutionPlan {
        statement_text: document
            .query
            .as_deref()
            .map(strip_explain_prefix)
            .unwrap_or_default()
            .to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        source_format: Some("duckdb-text".to_string()),
        execution_time_ms: document.total_time_s.map(|s| s * 1000.0),
        ..Default::default()
    };
    finish_duckdb_plan(&mut plan);
    Ok(plan)
}

/// True when `doc` holds a DuckDB text plan: at least one operator box,
/// that is a box whose first line is an upper-case operator name and which
/// is either followed by a rule line inside the box or is the bare `QUERY`
/// box of an ANALYZE profile.
pub fn looks_like_duckdb_explain_text(doc: &str) -> bool {
    if !doc.contains('\u{250c}') || !doc.contains('\u{2502}') {
        return false;
    }
    Document::read(doc).plan_tree().is_some()
}

/// Why a document that is not a box tree was refused. DuckDB's other
/// `EXPLAIN (FORMAT ...)` outputs get their own message.
fn unparsed_reason(doc: &str) -> &'static str {
    if doc.contains("digraph") && doc.contains("node_0_0") {
        "This is DuckDB's EXPLAIN (FORMAT GRAPHVIZ) output, which is a drawing rather than plan \
         data. Paste the default EXPLAIN text or EXPLAIN (FORMAT JSON) instead."
    } else if doc.contains("tf-tree") && doc.contains("<html") {
        "This is DuckDB's EXPLAIN (FORMAT HTML) page. Paste the default EXPLAIN text or EXPLAIN \
         (FORMAT JSON) instead."
    } else if doc.contains("- name: \"") && doc.contains("children:") {
        "This is DuckDB's EXPLAIN (FORMAT YAML) output, which is not supported. Paste the \
         default EXPLAIN text or EXPLAIN (FORMAT JSON) instead."
    } else if doc.contains("flowchart TD") {
        "This is DuckDB's EXPLAIN (FORMAT MERMAID) output, which is a drawing rather than plan \
         data. Paste the default EXPLAIN text or EXPLAIN (FORMAT JSON) instead."
    } else {
        "DuckDB EXPLAIN: no operator boxes found"
    }
}

/// The document as a character grid plus what its title boxes say.
struct Document {
    lines: Vec<Vec<char>>,
    /// Runs of consecutive box lines, with the title that precedes each.
    blocks: Vec<Block>,
    /// The statement line under `Query Profiling Information`.
    query: Option<String>,
    /// From the `Total Time: 0.0198s` title box.
    total_time_s: Option<f64>,
}

struct Block {
    title: Option<String>,
    /// Line range, end exclusive.
    start: usize,
    end: usize,
}

/// One operator box found in the grid.
struct GridBox {
    row: usize,
    x0: usize,
    /// The top border has a `┴`: the box hangs off one in the row above.
    has_parent: bool,
    /// The lines between the borders, untrimmed.
    content: Vec<String>,
}

struct Tree {
    boxes: Vec<GridBox>,
    children: Vec<Vec<usize>>,
    roots: Vec<usize>,
}

const TOP_LEFT: char = '\u{250c}';
const TOP_RIGHT: char = '\u{2510}';
const BOTTOM_LEFT: char = '\u{2514}';
const VERTICAL: char = '\u{2502}';
const HORIZONTAL: char = '\u{2500}';
const TEE_UP: char = '\u{2534}';
const TEE_RIGHT: char = '\u{251c}';

fn first_char(line: &[char]) -> Option<char> {
    line.iter().copied().find(|c| !c.is_whitespace())
}

fn trimmed(line: &[char]) -> String {
    line.iter().collect::<String>().trim().to_string()
}

impl Document {
    fn read(doc: &str) -> Self {
        let lines: Vec<Vec<char>> = doc
            .lines()
            .take(MAX_LINES)
            .map(|line| line.chars().collect())
            .collect();
        let mut document = Document {
            lines,
            blocks: Vec::new(),
            query: None,
            total_time_s: None,
        };
        let mut title: Option<String> = None;
        let mut index = 0usize;
        while index < document.lines.len() {
            if let Some((text, end)) = document.title_box(index) {
                if let Some(seconds) = text
                    .strip_prefix("Total Time:")
                    .and_then(|t| t.trim().strip_suffix('s'))
                    .and_then(|t| t.trim().parse::<f64>().ok())
                    .filter(|s| s.is_finite())
                {
                    document.total_time_s = Some(seconds);
                }
                title = Some(text);
                index = end;
                continue;
            }
            let line = &document.lines[index];
            match first_char(line) {
                Some(TOP_LEFT) => {
                    let start = index;
                    index += 1;
                    while index < document.lines.len()
                        && matches!(
                            first_char(&document.lines[index]),
                            Some(TOP_LEFT | VERTICAL | BOTTOM_LEFT)
                        )
                        && document.title_box(index).is_none()
                    {
                        index += 1;
                    }
                    document.blocks.push(Block {
                        title: title.clone(),
                        start,
                        end: index,
                    });
                    continue;
                }
                Some(_)
                    if document.query.is_none()
                        && title.as_deref() == Some("Query Profiling Information") =>
                {
                    document.query = Some(trimmed(line));
                }
                _ => {}
            }
            index += 1;
        }
        document
    }

    /// A title box starts at `index`: a border line, then `│┌`, then
    /// `││ text ││`. Returns the text and the line after the box.
    fn title_box(&self, index: usize) -> Option<(String, usize)> {
        if first_char(self.lines.get(index)?) != Some(TOP_LEFT) {
            return None;
        }
        let starts = |line: usize, a: char, b: char| {
            self.lines.get(line).is_some_and(|l| {
                let mut chars = l.iter().copied().filter(|c| !c.is_whitespace());
                chars.next() == Some(a) && chars.next() == Some(b)
            })
        };
        if !starts(index + 1, VERTICAL, TOP_LEFT) || !starts(index + 2, VERTICAL, VERTICAL) {
            return None;
        }
        let text: String = self
            .lines
            .get(index + 2)?
            .iter()
            .filter(|c| **c != VERTICAL)
            .collect();
        let mut end = index + 3;
        // The inner and outer bottom borders.
        for _ in 0..2 {
            if self
                .lines
                .get(end)
                .is_some_and(|l| matches!(first_char(l), Some(VERTICAL | BOTTOM_LEFT)))
            {
                end += 1;
            }
        }
        Some((text.trim().to_string(), end))
    }

    /// The tree to parse: the physical plan when the document names one,
    /// else the profile under `Total Time`, else the first block that holds
    /// operator boxes.
    fn plan_tree(&self) -> Option<Tree> {
        let rank = |block: &Block| match block.title.as_deref() {
            Some(title) if title.contains("Physical Plan") => 0,
            Some(title) if title.starts_with("Total Time") => 1,
            _ => 2,
        };
        let mut order: Vec<&Block> = self.blocks.iter().collect();
        order.sort_by_key(|block| rank(block));
        order.into_iter().find_map(|block| self.tree_in(block))
    }

    fn tree_in(&self, block: &Block) -> Option<Tree> {
        let mut boxes = Vec::new();
        for row in block.start..block.end {
            let line = self.lines.get(row)?;
            let mut x = 0usize;
            while x < line.len() {
                if line[x] != TOP_LEFT {
                    x += 1;
                    continue;
                }
                let Some(width) = line[x + 1..]
                    .iter()
                    .position(|c| *c != HORIZONTAL && *c != TEE_UP)
                else {
                    break;
                };
                let x1 = x + 1 + width;
                if width < 3 || line.get(x1) != Some(&TOP_RIGHT) {
                    x = x1.max(x + 1);
                    continue;
                }
                let has_parent = line[x + 1..x1].contains(&TEE_UP);
                let mut content = Vec::new();
                for below in self.lines.iter().take(block.end).skip(row + 1) {
                    let closes = matches!(below.get(x1), Some(&VERTICAL | &TEE_RIGHT));
                    if below.get(x) != Some(&VERTICAL) || !closes {
                        break;
                    }
                    content.push(below.get(x + 1..x1)?.iter().collect::<String>());
                }
                boxes.push(GridBox {
                    row,
                    x0: x,
                    has_parent,
                    content,
                });
                x = x1 + 1;
            }
        }
        if !boxes.iter().any(is_operator_box) {
            return None;
        }
        // Boxes of one grid row share a top border line. A box hangs off the
        // box in the row above that starts at or left of it, nearest first:
        // DuckDB places the first child under its parent and later children
        // to the right of the earlier children's subtrees.
        // Boxes are in reading order, so the boxes of one grid row are a
        // run of the list, sorted by column.
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); boxes.len()];
        let mut roots = Vec::new();
        // The run of the grid row above the current one, and the current's.
        let mut above = 0..0;
        let mut current = 0..0;
        for (index, grid_box) in boxes.iter().enumerate() {
            if boxes.get(current.start).map(|b| b.row) != Some(grid_box.row) {
                above = current.start..index;
                current = index..index;
            }
            current.end = index + 1;
            let parent = boxes
                .get(above.clone())
                .filter(|_| grid_box.has_parent)
                .and_then(|row| {
                    row.partition_point(|b| b.x0 <= grid_box.x0)
                        .checked_sub(1)
                        .map(|at| above.start + at)
                });
            match parent {
                Some(parent) => match children.get_mut(parent) {
                    Some(list) => list.push(index),
                    None => roots.push(index),
                },
                None => roots.push(index),
            }
        }
        // One plan has one root; anything else in the block (a result
        // table printed right under the plan) is not part of it.
        let root = roots
            .iter()
            .copied()
            .find(|index| is_operator_box(&boxes[*index]))?;
        Some(Tree {
            boxes,
            children,
            roots: vec![root],
        })
    }
}

/// The first content line is an operator name (`HASH_JOIN`, `SEQ_SCAN `,
/// `QUERY`), and the second, when present, is the rule under it.
fn is_operator_box(grid_box: &GridBox) -> bool {
    let Some(name) = grid_box.content.first().map(|line| line.trim()) else {
        return false;
    };
    let named = name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == ' ');
    let ruled = match grid_box.content.get(1) {
        Some(line) => is_rule(line),
        None => name == "QUERY",
    };
    named && ruled
}

fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 3 && line.chars().all(|c| c == HORIZONTAL)
}

#[derive(Default)]
struct Builder {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
}

impl Builder {
    fn visit(
        &mut self,
        tree: &Tree,
        index: usize,
        parent: Option<i32>,
        depth: usize,
    ) -> PlanResult<()> {
        if depth > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "DuckDB EXPLAIN: the plan nests too deeply".into(),
            ));
        }
        let (Some(grid_box), Some(children)) = (tree.boxes.get(index), tree.children.get(index))
        else {
            return Ok(());
        };
        let op = read_box(&grid_box.content);
        // `EXPLAIN_ANALYZE` is the statement's own plumbing, not part of
        // the query.
        let own_id = if WRAPPER_OPERATORS.contains(&op.name.as_str()) {
            parent
        } else {
            let id = self.nodes.len() as i32 + 1;
            self.nodes.push(operator_node(id, op));
            if let Some(parent) = parent {
                self.edges.push(PlanEdge {
                    from_node: parent,
                    to_node: id,
                });
            }
            Some(id)
        };
        for child in children {
            self.visit(tree, *child, own_id, depth + 1)?;
        }
        Ok(())
    }
}

/// Read one box: name, rule, then labelled and unlabelled lines.
fn read_box(content: &[String]) -> Operator {
    let mut op = Operator::default();
    let mut lines = content.iter();
    for line in lines.by_ref() {
        if !line.trim().is_empty() {
            op.name = line.trim().to_string();
            break;
        }
    }
    // Whether the next line may continue the latest value, and how that
    // value's latest line sat in the box.
    let mut open = false;
    let mut previous = BoxLine::default();
    // Brackets and quotes the latest value has left open.
    let mut pending = Pending::default();
    // The label unlabelled lines belong to; reset by a blank line.
    let mut label: Option<String> = None;
    for raw in lines {
        let text = raw.trim();
        if text.is_empty() {
            open = false;
            label = None;
            continue;
        }
        if is_rule(raw) {
            continue;
        }
        if let Some((estimate, rows)) = read_rows(text) {
            if estimate {
                op.estimated_rows = Some(rows);
            } else {
                op.actual_rows = Some(rows);
            }
            open = false;
            continue;
        }
        if let Some(seconds) = text
            .strip_prefix('(')
            .and_then(|t| t.strip_suffix("s)"))
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|s| s.is_finite())
        {
            op.timing_s = Some(seconds);
            open = false;
            continue;
        }
        if open
            && let Some(last) = op
                .entries
                .last_mut()
                .and_then(|(_, values)| values.last_mut())
            && continues(last, text, &previous, &pending)
        {
            if needs_space(last, text, &previous, &pending) {
                last.push(' ');
            }
            last.push_str(text);
            pending.read(text);
            previous = BoxLine::of(raw, false);
            continue;
        }
        if let Some((name, value)) = split_label(text) {
            label = Some(name.to_string());
            if value.is_empty() {
                op.entries.push((name.to_string(), Vec::new()));
                open = false;
            } else {
                op.entries.push((name.to_string(), vec![value.to_string()]));
                pending = Pending::default();
                pending.read(value);
                open = true;
            }
        } else {
            let current = label.clone().unwrap_or_default();
            match op.entries.last_mut() {
                Some((name, values)) if *name == current && (label.is_some() || open) => {
                    values.push(text.to_string());
                }
                _ => op.entries.push((current, vec![text.to_string()])),
            }
            // Later unlabelled lines of this group go to the same entry.
            if label.is_none() {
                label = Some(String::new());
            }
            pending = Pending::default();
            pending.read(text);
            open = true;
        }
        previous = BoxLine::of(raw, true);
    }
    op
}

/// Where DuckDB's renderer may break a line (`CanSplitOnThisChar` in
/// `text_tree_renderer.cpp`): at anything but a letter, a digit, `_` and
/// the few characters after `z`.
fn can_split_at(c: char) -> bool {
    c < '0' || (c > '9' && c < 'A') || (c > 'Z' && c < 'a' && c != '_')
}

/// How one content line sat in its box. The renderer centres every line, so
/// a line with no padding on either side filled the box.
#[derive(Default)]
struct BoxLine {
    /// Characters of the text, and of the box between its borders.
    len: usize,
    width: usize,
    /// The first line of a value, not the continuation of one.
    first: bool,
    full: bool,
    /// The line filled the box and the renderer had nowhere to break it:
    /// it only breaks at a character past the eighth, so a full line with
    /// none of those was cut in the middle of a word.
    hard_cut: bool,
    ascii: bool,
}

impl BoxLine {
    fn of(raw: &str, first: bool) -> Self {
        let text = raw.trim();
        let full = !text.is_empty() && !raw.starts_with(' ') && !raw.ends_with(' ');
        BoxLine {
            len: text.chars().count(),
            width: raw.chars().count(),
            first,
            full,
            hard_cut: full && !text.chars().skip(8).any(can_split_at),
            ascii: raw.is_ascii(),
        }
    }
}

/// What a value has left open so far, kept up to date line by line so a
/// long value is not read again for every line added to it.
#[derive(Default)]
struct Pending {
    brackets: i64,
    single_quotes: usize,
    double_quotes: usize,
}

impl Pending {
    fn read(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '(' | '[' => self.brackets += 1,
                ')' | ']' => self.brackets -= 1,
                '\'' => self.single_quotes += 1,
                '"' => self.double_quotes += 1,
                _ => {}
            }
        }
    }

    /// `quote` would close a string or a quoted name opened earlier.
    fn closes(&self, quote: char) -> bool {
        match quote {
            '\'' => self.single_quotes % 2 == 1,
            '"' => self.double_quotes % 2 == 1,
            _ => false,
        }
    }
}

/// `~80,000 rows`, `~80000 Rows` (estimates) and `300,000 rows` (actual).
/// DuckDB 1.4 writes a count of one as `1 row` / `~1 row`.
fn read_rows(text: &str) -> Option<(bool, f64)> {
    let count = [" rows", " Rows", " row"]
        .into_iter()
        .find_map(|suffix| text.strip_suffix(suffix))?;
    let estimate = count.starts_with('~');
    let digits = count.trim_start_matches('~');
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit() || c == ',') {
        return None;
    }
    super::common::parse_count(digits).map(|rows| (estimate, rows))
}

/// `Join Type: INNER` → (`Join Type`, `INNER`); `Filters:` → (`Filters`, ``).
/// Labels are short capitalised words; anything else is an expression that
/// happens to contain a colon.
fn split_label(text: &str) -> Option<(&str, &str)> {
    let (name, value) = match text.split_once(": ") {
        Some((name, value)) => (name, value.trim()),
        None => (text.strip_suffix(':')?, ""),
    };
    let valid = !name.is_empty()
        && name.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-');
    valid.then_some((name, value))
}

const TRAILING_WORDS: [&str; 10] = [
    "AND", "OR", "NOT", "IS", "AS", "BY", "IN", "ON", "LIKE", "BETWEEN",
];
const LEADING_WORDS: [&str; 11] = [
    "AND", "OR", "IS", "AS", "NOT", "IN", "LIKE", "BETWEEN", "DESC", "ASC", "NULLS",
];

fn last_word(text: &str) -> &str {
    text.rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or("")
}

fn first_word(text: &str) -> &str {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or("")
}

/// Whether `next` is the rest of the wrapped value `so_far` rather than a
/// new list item. `previous` is the value's latest line.
///
/// DuckDB's renderer (`SplitStringBuffer`) fills a line until the next
/// character would not fit, then breaks before the last character it may
/// break at, or right there when that character is among the line's first
/// eight. A break at a space leaves the space at the start of the next
/// line, where centring hides it. Two things follow exactly: a line cut in
/// the middle of a word is always continued, and a line is only continued
/// by text whose first unbreakable run would not have fitted after it. The
/// rest is a judgement from how the two lines read.
fn continues(so_far: &str, next: &str, previous: &BoxLine, pending: &Pending) -> bool {
    if previous.hard_cut {
        return true;
    }
    if previous.ascii && next.is_ascii() {
        // A full first line that had a place to break was not broken.
        if previous.full && previous.first {
            return false;
        }
        // The run after the break character. Two columns of slack cover a
        // space hidden at either end of the previous line.
        let skip = usize::from(next.starts_with(can_split_at));
        let run = next
            .chars()
            .skip(skip)
            .take_while(|c| !can_split_at(*c))
            .count();
        if previous.len + run + 3 < previous.width {
            return false;
        }
    }
    if pending.brackets > 0 || pending.closes('\'') {
        return true;
    }
    if so_far.ends_with(['=', '<', '>', '+', '*', '/', ',', '(', '-'])
        || TRAILING_WORDS.contains(&last_word(so_far))
    {
        return true;
    }
    next.starts_with(['.', ',', ')', '=', '<', '>', '*', '/'])
        || LEADING_WORDS.contains(&first_word(next))
}

/// Whether a space was lost at the wrap point.
fn needs_space(so_far: &str, next: &str, previous: &BoxLine, pending: &Pending) -> bool {
    if previous.hard_cut
        || next.starts_with(['.', ',', ')', ']'])
        || so_far.ends_with(['(', '.', '['])
    {
        return false;
    }
    // The line was broken before the quote that ends a string.
    if next.chars().next().is_some_and(|c| pending.closes(c)) {
        return false;
    }
    if next.starts_with('(') {
        // `DECIMAL` + `(38,0)` is one type name; `AND` + `(a > 1)` is not.
        let word = last_word(so_far);
        return word.is_empty() || TRAILING_WORDS.contains(&word);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::dialects::PlanDialect;
    use crate::plans::warning::PlanWarning;

    const JOIN_EXPLAIN_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-join-explain.txt");
    const JOIN_ANALYZE_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-join-analyze.txt");
    const RANGE_EXPLAIN_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-range-explain.txt");
    const RANGE_EXPLAIN_ALL_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-range-explain-all.txt");
    const RANGE_PROFILE_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-range-profile-with-results.txt");
    const JOIN_EXPLAIN_113: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb113-join-explain.txt");
    const JOIN_ANALYZE_113: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb113-join-analyze.txt");
    const RANGE_EXPLAIN_113: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb113-range-explain.txt");
    const JOIN_JSON_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-join-explain.json");

    fn ops(plan: &ExecutionPlan) -> Vec<&str> {
        plan.nodes.iter().map(|n| n.logical_op.as_str()).collect()
    }

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
    fn estimated_join_plan_reads_the_grid() {
        let plan = parse_duckdb_explain_text(JOIN_EXPLAIN_142).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::DuckDb));
        assert_eq!(plan.source_format.as_deref(), Some("duckdb-text"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(
            ops(&plan),
            [
                "PROJECTION",
                "ORDER_BY",
                "PROJECTION",
                "PROJECTION",
                "HASH_GROUP_BY",
                "PROJECTION",
                "PROJECTION",
                "HASH_JOIN",
                "SEQ_SCAN",
                "HASH_JOIN",
                "SEQ_SCAN",
                "SEQ_SCAN",
            ]
        );
        assert_eq!(
            edges(&plan),
            vec![
                (1, 2),
                (2, 3),
                (3, 4),
                (4, 5),
                (5, 6),
                (6, 7),
                (7, 8),
                (8, 9),
                (8, 10),
                (10, 11),
                (10, 12),
            ]
        );
        assert_eq!(
            plan.nodes[0].output_columns,
            ["__internal_decompress_string(#0)", "#1", "#2"]
        );
        assert_eq!(
            plan.nodes[1].sort_keys,
            ["sum((oi.quantity * oi.unit_price)) DESC"]
        );
        let group = &plan.nodes[4];
        assert_eq!(group.group_keys, ["#0"]);
        assert_eq!(prop(group, "Aggregates"), Some("count_star(), sum(#1)"));
        assert_eq!(group.estimated_rows, 5.0);
        assert_eq!(
            plan.nodes[5].output_columns,
            ["country", "(CAST(quantity AS DECIMAL(38,0)) * unit_price)"]
        );
        let join = &plan.nodes[7];
        assert_eq!(join.join_type.as_deref(), Some("INNER"));
        assert_eq!(join.join_predicate.as_deref(), Some("order_id = id"));
        assert_eq!(join.estimated_rows, 96231.0);
        let items = &plan.nodes[8];
        assert_eq!(items.physical_op, "SEQ_SCAN order_items");
        assert_eq!(items.relation.as_deref(), Some("order_items"));
        assert_eq!(items.estimated_rows, 1_200_000.0);
        assert_eq!(items.output_columns, ["order_id", "quantity", "unit_price"]);
        assert_eq!(prop(items, "Type"), Some("Sequential Scan"));
        assert!(matches!(
            items.warnings.as_slice(),
            [PlanWarning::TableScan { table, .. }] if table == "order_items"
        ));
        assert_eq!(plan.nodes[9].estimated_rows, 83777.0);
        assert_eq!(
            plan.nodes[9].join_predicate.as_deref(),
            Some("customer_id = id")
        );
        let orders = &plan.nodes[10];
        assert_eq!(orders.relation.as_deref(), Some("orders"));
        assert_eq!(orders.filter.as_deref(), Some("status='paid'"));
        assert_eq!(orders.estimated_rows, 100_000.0);
        assert!(orders.warnings.is_empty());
        assert_eq!(plan.nodes[11].relation.as_deref(), Some("customers"));
        assert_eq!(plan.nodes[11].estimated_rows, 50_000.0);
    }

    #[test]
    fn text_and_json_of_one_plan_agree() {
        let text = parse_duckdb_explain_text(JOIN_EXPLAIN_142).unwrap();
        let json = super::super::duckdb::parse_duckdb_explain_json(JOIN_JSON_142).unwrap();
        assert_eq!(ops(&text), ops(&json));
        assert_eq!(edges(&text), edges(&json));
        for (a, b) in text.nodes.iter().zip(&json.nodes) {
            assert_eq!(a.physical_op, b.physical_op);
            assert_eq!(a.relation, b.relation);
            assert_eq!(a.filter, b.filter);
            assert_eq!(a.join_predicate, b.join_predicate);
            assert_eq!(a.output_columns, b.output_columns);
            assert_eq!(a.group_keys, b.group_keys);
            assert_eq!(a.sort_keys, b.sort_keys);
            // The text prints `~0 rows` nowhere: an operator estimated at
            // zero rows has no cardinality line, like one with no estimate.
            assert_eq!(a.estimated_rows, b.estimated_rows);
        }
    }

    #[test]
    fn analyze_fills_rows_timing_and_total_time() {
        let plan = parse_duckdb_explain_text(JOIN_ANALYZE_142).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert!(plan.has_actuals);
        assert!((plan.execution_time_ms.unwrap() - 19.8).abs() < 1e-9);
        assert!(
            plan.statement_text
                .starts_with("SELECT c.country, count(*) AS orders")
        );
        assert!(plan.statement_text.ends_with("ORDER BY revenue DESC"));
        // QUERY stays as the root; EXPLAIN_ANALYZE is dropped.
        assert_eq!(plan.nodes.len(), 13);
        assert_eq!(plan.nodes[0].logical_op, "QUERY");
        assert_eq!(plan.nodes[1].logical_op, "PROJECTION");
        assert!(!ops(&plan).contains(&"EXPLAIN_ANALYZE"));
        assert_eq!(edges(&plan)[0], (1, 2));
        assert_eq!(plan.nodes[1].actual_rows, Some(2.0));
        let join = &plan.nodes[8];
        assert_eq!(join.logical_op, "HASH_JOIN");
        assert_eq!(join.actual_rows, Some(300_000.0));
        let items = &plan.nodes[9];
        assert_eq!(items.physical_op, "TABLE_SCAN order_items");
        assert_eq!(items.actual_rows, Some(1_199_991.0));
        assert_eq!(items.actual_rows_total, Some(1_199_991.0));
        assert!((items.actual_self_time_ms.unwrap() - 70.0).abs() < 1e-9);
        assert!((items.actual_time_ms.unwrap() - 70.0).abs() < 1e-9);
        assert_eq!(items.estimated_cost, 0.07);
        // The join's inclusive time covers the scan under it.
        assert!((join.actual_time_ms.unwrap() - 70.0).abs() < 1e-9);
        assert!((join.actual_self_time_ms.unwrap() - 0.0).abs() < 1e-9);
        assert!((plan.nodes[0].actual_time_ms.unwrap() - 70.0).abs() < 1e-9);
        let orders = &plan.nodes[11];
        assert_eq!(orders.relation.as_deref(), Some("orders"));
        assert_eq!(orders.actual_rows, Some(100_000.0));
        assert_eq!(orders.filter.as_deref(), Some("status='paid'"));
        assert_eq!(plan.nodes[12].actual_rows, Some(50_000.0));
        assert_eq!(edges(&plan)[8..], [(9, 10), (9, 11), (11, 12), (11, 13)]);
    }

    #[test]
    fn wrapped_filter_lists_are_rejoined() {
        let plan = parse_duckdb_explain_text(RANGE_EXPLAIN_142).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "PROJECTION",
                "HASH_GROUP_BY",
                "PROJECTION",
                "PROJECTION",
                "SEQ_SCAN"
            ]
        );
        let scan = &plan.nodes[4];
        assert_eq!(
            scan.filter.as_deref(),
            Some("customer_id>=100 AND customer_id<=200 AND total>800")
        );
        assert_eq!(scan.output_columns, ["status"]);
        assert_eq!(scan.estimated_rows, 80_000.0);
        assert_eq!(plan.nodes[0].estimated_rows, 4.0);
        assert_eq!(
            plan.nodes[3].output_columns,
            ["__internal_compress_string_uhugeint(#0)"]
        );
    }

    #[test]
    fn explain_all_picks_the_physical_plan() {
        let plan = parse_duckdb_explain_text(RANGE_EXPLAIN_ALL_142).unwrap();
        let single = parse_duckdb_explain_text(RANGE_EXPLAIN_142).unwrap();
        assert_eq!(ops(&plan), ops(&single));
        assert_eq!(plan.nodes[4].filter, single.nodes[4].filter);
    }

    #[test]
    fn profile_output_ignores_the_result_tables_after_it() {
        let plan = parse_duckdb_explain_text(RANGE_PROFILE_142).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "QUERY",
                "PROJECTION",
                "HASH_GROUP_BY",
                "PROJECTION",
                "PROJECTION",
                "TABLE_SCAN"
            ]
        );
        assert!((plan.execution_time_ms.unwrap() - 1.7).abs() < 1e-9);
        assert!(
            plan.statement_text
                .starts_with("SELECT status, count(*) FROM orders")
        );
        assert_eq!(plan.nodes[5].actual_rows, Some(97.0));
        assert_eq!(plan.nodes[5].relation.as_deref(), Some("orders"));
    }

    #[test]
    fn duckdb_1_1_spelling_is_read() {
        let plan = parse_duckdb_explain_text(JOIN_EXPLAIN_113).unwrap();
        assert_eq!(plan.nodes.len(), 12);
        let join = &plan.nodes[7];
        assert_eq!(join.estimated_rows, 96231.0);
        assert_eq!(prop(join, "Build Min"), Some("1"));
        assert_eq!(prop(join, "Build Max"), Some("400000"));
        let items = &plan.nodes[8];
        assert_eq!(items.relation.as_deref(), Some("order_items"));
        assert_eq!(items.physical_op, "SEQ_SCAN order_items");
        assert_eq!(items.estimated_rows, 1_200_000.0);
        assert_eq!(items.warnings.len(), 1);
        let orders = &plan.nodes[10];
        assert_eq!(
            orders.filter.as_deref(),
            Some("status='paid' AND status IS NOT NULL")
        );
        assert_eq!(orders.estimated_rows, 100_000.0);
        assert_eq!(
            plan.nodes[6].output_columns,
            [
                "__internal_compress_integral_uinteger(#0, 1)",
                "#1",
                "#2",
                "__internal_compress_integral_uinteger(#3, 1)",
                "__internal_compress_string_uinteger(#4)",
            ]
        );

        let plan = parse_duckdb_explain_text(RANGE_EXPLAIN_113).unwrap();
        assert_eq!(
            plan.nodes[4].filter.as_deref(),
            Some(
                "customer_id>=100 AND customer_id<=200 AND customer_id IS NOT NULL AND \
                 total>800 AND total IS NOT NULL"
            )
        );
        assert_eq!(plan.nodes[4].estimated_rows, 80_000.0);

        let plan = parse_duckdb_explain_text(JOIN_ANALYZE_113).unwrap();
        assert!((plan.execution_time_ms.unwrap() - 13.6).abs() < 1e-9);
        assert_eq!(plan.nodes[0].logical_op, "QUERY");
        assert_eq!(plan.nodes[9].actual_rows, Some(1_199_991.0));
        assert!((plan.nodes[9].actual_self_time_ms.unwrap() - 50.0).abs() < 1e-9);
        assert_eq!(plan.nodes[8].actual_rows, Some(300_000.0));
    }

    #[test]
    fn detector_is_strict() {
        for doc in [
            JOIN_EXPLAIN_142,
            JOIN_ANALYZE_142,
            RANGE_EXPLAIN_142,
            RANGE_EXPLAIN_ALL_142,
            RANGE_PROFILE_142,
            JOIN_EXPLAIN_113,
            JOIN_ANALYZE_113,
            RANGE_EXPLAIN_113,
        ] {
            assert!(looks_like_duckdb_explain_text(doc));
        }
        // The CLI's title box above FORMAT JSON output is not a text plan.
        assert!(!looks_like_duckdb_explain_text(JOIN_JSON_142));
        for doc in [
            "",
            "QUERY PLAN\n|--SCAN o\n`--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)\n",
            include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-box.txt"),
            include_str!("../../../tests/golden/plans/clickhouse/clickhouse269-join-plan.txt"),
            include_str!(
                "../../../tests/golden/plans/clickhouse/clickhouse269-range-plan-pretty.txt"
            ),
            "┌─────────┐\n│ Success │\n│ boolean │\n├─────────┤\n│ 0 rows  │\n└─────────┘\n",
            "Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)\n",
        ] {
            assert!(!looks_like_duckdb_explain_text(doc), "{doc}");
        }
    }

    #[test]
    fn other_explain_formats_are_named_in_the_error() {
        let graphviz =
            "digraph G {\n    node [shape=box]\n    node_0_0 [label=\"PROJECTION\"];\n}\n";
        let error = parse_duckdb_explain_text(graphviz).unwrap_err().to_string();
        assert!(error.contains("GRAPHVIZ"), "{error}");
        let yaml = "- name: \"PROJECTION\"\n  children: \n    - name: \"SEQ_SCAN \"\n";
        let error = parse_duckdb_explain_text(yaml).unwrap_err().to_string();
        assert!(error.contains("YAML"), "{error}");
        assert!(parse_duckdb_explain_text("").is_err());
        assert!(parse_duckdb_explain_text("not a plan").is_err());
    }

    #[test]
    fn duckdb_text_survives_mangled_input() {
        for sample in [
            JOIN_ANALYZE_142,
            RANGE_EXPLAIN_ALL_142,
            RANGE_PROFILE_142,
            JOIN_EXPLAIN_113,
        ] {
            super::super::common::assert_survives_mangling(sample, |doc| {
                let _ = parse_duckdb_explain_text(doc);
                let _ = looks_like_duckdb_explain_text(doc);
            });
        }
    }
}
