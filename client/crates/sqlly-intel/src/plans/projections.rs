//! Other ways to look at an analysed plan than the tree diagram: a sortable
//! operator table, a flame (icicle) chart of inclusive time, and a short
//! "where the time went" list by operator, by operator family and by table.
//! All of it is plain data computed once from [`PlanAnalysis`].

use std::cmp::Ordering;
use std::collections::HashMap;

use sqlly_core::DisplayMessage;

use crate::plans::analysis::{
    EstimateDirection, NodeFlags, NodeHeat, PlanAnalysis, PlanMetric, PlanObjectKind,
};
use crate::plans::glossary::OperatorCategory;
use crate::plans::phrases::{PlanPhrase, phrase, say0};

// ---------------------------------------------------------------------------
// Operator table
// ---------------------------------------------------------------------------

/// One operator as a table row. Shares are 0..1; times are milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorTableRow {
    pub node_id: i32,
    /// 1-based position in execution order, the same number the story uses.
    pub step: usize,
    pub depth: usize,
    /// Operator name as the engine prints it.
    pub operator: String,
    pub category: OperatorCategory,
    /// Table (with alias) the operator reads or writes.
    pub object: Option<String>,
    pub index: Option<String>,
    pub self_time_ms: Option<f64>,
    pub inclusive_time_ms: Option<f64>,
    pub time_share: Option<f64>,
    pub self_cost: Option<f64>,
    pub inclusive_cost: Option<f64>,
    pub cost_share: Option<f64>,
    /// Rows produced across all loops.
    pub rows: Option<f64>,
    /// Rows that reached the operator.
    pub rows_in: Option<f64>,
    /// Estimated rows per execution.
    pub estimated_rows: Option<f64>,
    /// How many times off the estimate was (>= 1).
    pub estimate_factor: Option<f64>,
    pub estimate_direction: Option<EstimateDirection>,
    pub loops: Option<f64>,
    pub rows_removed: Option<f64>,
    /// Pages the operator read itself.
    pub io_pages: Option<f64>,
    pub io_share: Option<f64>,
    pub memory_kb: Option<f64>,
    pub heat: NodeHeat,
    pub flags: NodeFlags,
}

/// A column of the operator table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorTableColumn {
    Step,
    Operator,
    Object,
    SelfTime,
    TotalTime,
    TimeShare,
    SelfCost,
    TotalCost,
    CostShare,
    Rows,
    EstimatedRows,
    EstimateFactor,
    Loops,
    RowsRemoved,
    Io,
    Memory,
}

impl OperatorTableColumn {
    pub const ALL: [OperatorTableColumn; 16] = [
        Self::Step,
        Self::Operator,
        Self::Object,
        Self::SelfTime,
        Self::TotalTime,
        Self::TimeShare,
        Self::SelfCost,
        Self::TotalCost,
        Self::CostShare,
        Self::Rows,
        Self::EstimatedRows,
        Self::EstimateFactor,
        Self::Loops,
        Self::RowsRemoved,
        Self::Io,
        Self::Memory,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Step => "step",
            Self::Operator => "operator",
            Self::Object => "object",
            Self::SelfTime => "self_time",
            Self::TotalTime => "total_time",
            Self::TimeShare => "time_share",
            Self::SelfCost => "self_cost",
            Self::TotalCost => "total_cost",
            Self::CostShare => "cost_share",
            Self::Rows => "rows",
            Self::EstimatedRows => "estimated_rows",
            Self::EstimateFactor => "estimate_factor",
            Self::Loops => "loops",
            Self::RowsRemoved => "rows_removed",
            Self::Io => "io",
            Self::Memory => "memory",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|column| column.key() == key)
    }

    /// Column header.
    pub fn label(self) -> DisplayMessage {
        say0(match self {
            Self::Step => "table.column.step",
            Self::Operator => "table.column.operator",
            Self::Object => "table.column.object",
            Self::SelfTime => "table.column.self_time",
            Self::TotalTime => "table.column.total_time",
            Self::TimeShare => "table.column.time_share",
            Self::SelfCost => "table.column.self_cost",
            Self::TotalCost => "table.column.total_cost",
            Self::CostShare => "table.column.cost_share",
            Self::Rows => "table.column.rows",
            Self::EstimatedRows => "table.column.estimated_rows",
            Self::EstimateFactor => "table.column.estimate_factor",
            Self::Loops => "table.column.loops",
            Self::RowsRemoved => "table.column.rows_removed",
            Self::Io => "table.column.io",
            Self::Memory => "table.column.memory",
        })
    }

    /// Text columns sort ascending first; number columns largest first.
    pub fn is_numeric(self) -> bool {
        !matches!(self, Self::Operator | Self::Object)
    }

    fn number(self, row: &OperatorTableRow) -> Option<f64> {
        match self {
            Self::Step => Some(row.step as f64),
            Self::Operator | Self::Object => None,
            Self::SelfTime => row.self_time_ms,
            Self::TotalTime => row.inclusive_time_ms,
            Self::TimeShare => row.time_share,
            Self::SelfCost => row.self_cost,
            Self::TotalCost => row.inclusive_cost,
            Self::CostShare => row.cost_share,
            Self::Rows => row.rows,
            Self::EstimatedRows => row.estimated_rows,
            Self::EstimateFactor => row.estimate_factor,
            Self::Loops => row.loops,
            Self::RowsRemoved => row.rows_removed,
            Self::Io => row.io_pages,
            Self::Memory => row.memory_kb,
        }
    }
}

/// One row per operator, in plan order.
pub fn plan_operator_table(analysis: &PlanAnalysis) -> Vec<OperatorTableRow> {
    let mut step_of = vec![0usize; analysis.nodes.len()];
    for (number, node_id) in analysis.execution_order.iter().enumerate() {
        if let Some(position) = analysis.position(*node_id) {
            step_of[position] = number + 1;
        }
    }
    analysis
        .nodes
        .iter()
        .enumerate()
        .map(|(position, node)| {
            let metrics = &node.metrics;
            OperatorTableRow {
                node_id: node.node_id,
                step: step_of[position],
                depth: node.depth,
                operator: node.operator.name.clone(),
                category: node.category(),
                object: node.object.as_ref().map(|object| object.label()),
                index: node.index.clone(),
                self_time_ms: metrics.self_time_ms,
                inclusive_time_ms: metrics.inclusive_time_ms,
                time_share: metrics.time_share,
                self_cost: metrics.self_cost,
                inclusive_cost: metrics.inclusive_cost,
                cost_share: metrics.cost_share,
                rows: metrics.rows_out,
                rows_in: metrics.rows_in,
                estimated_rows: metrics.estimated_rows,
                estimate_factor: metrics.estimate_factor,
                estimate_direction: metrics.estimate_direction,
                loops: metrics.loops,
                rows_removed: metrics.rows_removed,
                io_pages: metrics.io_pages_self,
                io_share: metrics.io_share,
                memory_kb: metrics.memory_kb,
                heat: metrics.heat,
                flags: node.flags,
            }
        })
        .collect()
}

/// Columns worth showing for this plan: the ones at least one operator has a
/// value for. `Step` and `Operator` are always present.
pub fn operator_table_columns(rows: &[OperatorTableRow]) -> Vec<OperatorTableColumn> {
    OperatorTableColumn::ALL
        .into_iter()
        .filter(|column| match column {
            OperatorTableColumn::Step | OperatorTableColumn::Operator => true,
            OperatorTableColumn::Object => rows
                .iter()
                .any(|row| row.object.is_some() || row.index.is_some()),
            column => rows.iter().any(|row| column.number(row).is_some()),
        })
        .collect()
}

/// Sort the table by `column`. Rows without a value for the column go last
/// in both directions, and the sort is stable: rows that tie keep the order
/// they were in, so sorting by one column and then another works as a
/// two-level sort.
pub fn sort_operator_table(
    rows: &mut [OperatorTableRow],
    column: OperatorTableColumn,
    descending: bool,
) {
    let directed = |ordering: Ordering| {
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    };
    rows.sort_by(|left, right| match column {
        OperatorTableColumn::Operator => directed(
            left.operator
                .to_lowercase()
                .cmp(&right.operator.to_lowercase()),
        ),
        OperatorTableColumn::Object => {
            let text = |row: &OperatorTableRow| {
                row.object
                    .as_ref()
                    .or(row.index.as_ref())
                    .map(|text| text.to_lowercase())
            };
            match (text(left), text(right)) {
                (Some(left), Some(right)) => directed(left.cmp(&right)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
        column => match (column.number(left), column.number(right)) {
            (Some(left), Some(right)) => directed(left.total_cmp(&right)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
    });
}

// ---------------------------------------------------------------------------
// Flame (icicle) chart
// ---------------------------------------------------------------------------

/// One bar of the flame chart. `x` and `width` are fractions of the chart
/// width; `depth` is the row (0 at the top).
#[derive(Debug, Clone, PartialEq)]
pub struct FlameCell {
    pub node_id: i32,
    pub depth: usize,
    pub x: f32,
    pub width: f32,
    /// Time (or cost) of the operator and everything below it.
    pub value: f64,
    /// The operator's own part of `value`.
    pub self_value: f64,
    /// `value` as a share of the chart total, 0..1.
    pub share: f64,
    /// Operator and the object it touches.
    pub label: DisplayMessage,
    pub category: OperatorCategory,
    /// 0..1 intensity of the operator's own share.
    pub heat: f32,
    pub flags: NodeFlags,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlameLayout {
    /// `Time` or `Cost`.
    pub metric: PlanMetric,
    /// Sum the chart's full width stands for.
    pub total: f64,
    /// Number of rows.
    pub depth: usize,
    /// Bars in plan order (parents before inputs, left to right).
    pub cells: Vec<FlameCell>,
}

/// Icicle layout: each operator is a bar as wide as its inclusive time,
/// its inputs are laid out below it from the left, and the part of the bar
/// with nothing below it is the operator's own time.
///
/// `metric` may be `Time` or `Cost`; `None` picks time when the plan has
/// timings and cost otherwise. Returns `None` when the plan carries neither.
///
/// Widths are built bottom-up from own values, so a parent is always exactly
/// as wide as itself plus its inputs, including on engines whose reported
/// inclusive numbers do not add up (parallel workers, loops, InitPlans).
pub fn plan_flame_layout(
    analysis: &PlanAnalysis,
    metric: Option<PlanMetric>,
) -> Option<FlameLayout> {
    let metric = match metric {
        Some(PlanMetric::Time) if analysis.has_timing => PlanMetric::Time,
        Some(PlanMetric::Cost) if analysis.has_costs => PlanMetric::Cost,
        Some(_) => return None,
        None if analysis.has_timing => PlanMetric::Time,
        None if analysis.has_costs => PlanMetric::Cost,
        None => return None,
    };
    let count = analysis.nodes.len();
    let own: Vec<f64> = analysis
        .nodes
        .iter()
        .map(|node| {
            match metric {
                PlanMetric::Time => node.metrics.self_time_ms,
                _ => node.metrics.self_cost,
            }
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(0.0)
        })
        .collect();
    // Pre-order over the forest, then fold values upwards in reverse.
    let mut order = Vec::with_capacity(count);
    let mut seen = vec![false; count];
    let mut stack: Vec<usize> = analysis
        .roots
        .iter()
        .rev()
        .filter_map(|id| analysis.position(*id))
        .collect();
    while let Some(position) = stack.pop() {
        if std::mem::replace(&mut seen[position], true) {
            continue;
        }
        order.push(position);
        for child in analysis.nodes[position].children.iter().rev() {
            if let Some(child) = analysis.position(*child)
                && !seen[child]
            {
                stack.push(child);
            }
        }
    }
    let mut inclusive = own.clone();
    for &position in order.iter().rev() {
        if let Some(parent) = analysis.nodes[position]
            .parent_id
            .and_then(|id| analysis.position(id))
        {
            inclusive[parent] += inclusive[position];
        }
    }
    let total: f64 = analysis
        .roots
        .iter()
        .filter_map(|id| analysis.position(*id))
        .map(|position| inclusive[position])
        .sum();
    if total <= 0.0 {
        return None;
    }
    let mut start = vec![0.0_f64; count];
    let mut cursor = 0.0;
    for position in analysis
        .roots
        .iter()
        .filter_map(|id| analysis.position(*id))
    {
        start[position] = cursor;
        cursor += inclusive[position];
    }
    let mut cells = Vec::with_capacity(order.len());
    let mut depth = 0;
    for &position in &order {
        let node = &analysis.nodes[position];
        let mut cursor = start[position];
        for child in &node.children {
            if let Some(child) = analysis.position(*child) {
                start[child] = cursor;
                cursor += inclusive[child];
            }
        }
        if inclusive[position] <= 0.0 {
            continue;
        }
        depth = depth.max(node.depth + 1);
        cells.push(FlameCell {
            node_id: node.node_id,
            depth: node.depth,
            x: (start[position] / total) as f32,
            width: (inclusive[position] / total) as f32,
            value: inclusive[position],
            self_value: own[position],
            share: inclusive[position] / total,
            label: node.title(),
            category: node.category(),
            heat: node.metrics.heat.get(metric),
            flags: node.flags,
        });
    }
    Some(FlameLayout {
        metric,
        total,
        depth,
        cells,
    })
}

// ---------------------------------------------------------------------------
// Where the time went
// ---------------------------------------------------------------------------

/// One line of a breakdown.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakdownEntry {
    pub label: DisplayMessage,
    /// Operators the line stands for.
    pub node_ids: Vec<i32>,
    /// Own time (or cost) of those operators.
    pub value: f64,
    /// `value` as a share of the plan total, 0..1.
    pub share: f64,
    /// Set on by-family lines.
    pub category: Option<OperatorCategory>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanBreakdown {
    /// `Time` on timed plans, else `Cost`.
    pub metric: PlanMetric,
    pub total: f64,
    /// The heaviest single operators, largest first. When operators were
    /// left out, the last entry is "everything else".
    pub by_operator: Vec<BreakdownEntry>,
    /// Totals per operator family (scans, hash joins, sorts...).
    pub by_category: Vec<BreakdownEntry>,
    /// Totals per table, counting the operators that read or write it.
    /// Joins, sorts and aggregates belong to no table, so these shares need
    /// not add up to 1.
    pub by_table: Vec<BreakdownEntry>,
}

/// "Where the time went": the `top_n` heaviest operators, operator families
/// and tables by own time, or by own estimated cost when the plan has no
/// timings. `None` when the plan has neither.
pub fn plan_breakdown(analysis: &PlanAnalysis, top_n: usize) -> Option<PlanBreakdown> {
    let metric = if analysis.has_timing {
        PlanMetric::Time
    } else if analysis.has_costs {
        PlanMetric::Cost
    } else {
        return None;
    };
    let own = |position: usize| -> f64 {
        let metrics = &analysis.nodes[position].metrics;
        match metric {
            PlanMetric::Time => metrics.self_time_ms,
            _ => metrics.self_cost,
        }
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(0.0)
    };
    let total: f64 = (0..analysis.nodes.len()).map(own).sum();
    if total <= 0.0 {
        return None;
    }
    let top_n = top_n.max(1);

    let mut operators: Vec<(usize, f64)> = (0..analysis.nodes.len())
        .map(|position| (position, own(position)))
        .filter(|(_, value)| *value > 0.0)
        .collect();
    operators.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    let mut by_operator: Vec<BreakdownEntry> = operators
        .iter()
        .take(top_n)
        .map(|(position, value)| {
            let node = &analysis.nodes[*position];
            BreakdownEntry {
                label: node.title(),
                node_ids: vec![node.node_id],
                value: *value,
                share: value / total,
                category: Some(node.category()),
            }
        })
        .collect();
    if operators.len() > top_n {
        let rest = &operators[top_n..];
        let value: f64 = rest.iter().map(|(_, value)| value).sum();
        by_operator.push(BreakdownEntry {
            label: say0("breakdown.everything_else"),
            node_ids: rest
                .iter()
                .map(|(position, _)| analysis.nodes[*position].node_id)
                .collect(),
            value,
            share: value / total,
            category: None,
        });
    }

    let mut families: HashMap<OperatorCategory, (f64, Vec<i32>)> = HashMap::new();
    let mut tables: HashMap<String, (String, f64, Vec<i32>)> = HashMap::new();
    for (position, node) in analysis.nodes.iter().enumerate() {
        let value = own(position);
        if value <= 0.0 {
            continue;
        }
        let family = families.entry(node.category()).or_default();
        family.0 += value;
        family.1.push(node.node_id);
        if let Some(object) = &node.object
            && object.kind == PlanObjectKind::Table
        {
            let table = tables
                .entry(object.name.to_lowercase())
                .or_insert_with(|| (object.name.clone(), 0.0, Vec::new()));
            table.1 += value;
            table.2.push(node.node_id);
        }
    }
    let mut by_category: Vec<BreakdownEntry> = families
        .into_iter()
        .map(|(category, (value, node_ids))| BreakdownEntry {
            label: category.label(),
            node_ids,
            value,
            share: value / total,
            category: Some(category),
        })
        .collect();
    by_category.sort_by(|left, right| {
        right
            .value
            .total_cmp(&left.value)
            .then(left.category.cmp(&right.category))
    });
    by_category.truncate(top_n);
    let mut by_table: Vec<BreakdownEntry> = tables
        .into_values()
        .map(|(name, value, node_ids)| BreakdownEntry {
            label: DisplayMessage::raw(name),
            node_ids,
            value,
            share: value / total,
            category: None,
        })
        .collect();
    by_table.sort_by(|left, right| {
        right
            .value
            .total_cmp(&left.value)
            .then(left.label.fallback().cmp(right.label.fallback()))
    });
    by_table.truncate(top_n);

    Some(PlanBreakdown {
        metric,
        total,
        by_operator,
        by_category,
        by_table,
    })
}

pub(crate) static PHRASES: &[PlanPhrase] = &[
    phrase!(
        "table.column.step",
        "Step",
        "Execution Plan → Operator table: column header for the operator's position in execution order (the same number the story uses)."
    ),
    phrase!(
        "table.column.operator",
        "Operator",
        "Execution Plan → Operator table: column header for the operator name."
    ),
    phrase!(
        "table.column.object",
        "Table / index",
        "Execution Plan → Operator table: column header for the table and index an operator reads or writes."
    ),
    phrase!(
        "table.column.self_time",
        "Own time",
        "Execution Plan → Operator table: column header for the time spent in the operator alone, without its inputs."
    ),
    phrase!(
        "table.column.total_time",
        "Time with inputs",
        "Execution Plan → Operator table: column header for the time spent in the operator and everything below it."
    ),
    phrase!(
        "table.column.time_share",
        "% of time",
        "Execution Plan → Operator table: column header for the operator's own time as a percentage of the plan's time."
    ),
    phrase!(
        "table.column.self_cost",
        "Own cost",
        "Execution Plan → Operator table: column header for the optimizer's estimated cost of the operator alone."
    ),
    phrase!(
        "table.column.total_cost",
        "Cost with inputs",
        "Execution Plan → Operator table: column header for the optimizer's estimated cost of the operator and everything below it."
    ),
    phrase!(
        "table.column.cost_share",
        "% of cost",
        "Execution Plan → Operator table: column header for the operator's own estimated cost as a percentage of the plan's cost."
    ),
    phrase!(
        "table.column.rows",
        "Rows",
        "Execution Plan → Operator table: column header for the number of rows the operator actually produced."
    ),
    phrase!(
        "table.column.estimated_rows",
        "Estimated rows",
        "Execution Plan → Operator table: column header for the number of rows the optimizer expected per execution."
    ),
    phrase!(
        "table.column.estimate_factor",
        "Estimate off by",
        "Execution Plan → Operator table: column header for how many times the row estimate was off (shown as a factor such as 12x)."
    ),
    phrase!(
        "table.column.loops",
        "Loops",
        "Execution Plan → Operator table: column header for how many times the operator ran."
    ),
    phrase!(
        "table.column.rows_removed",
        "Rows discarded",
        "Execution Plan → Operator table: column header for rows the operator read and then filtered out."
    ),
    phrase!(
        "table.column.io",
        "Pages read",
        "Execution Plan → Operator table: column header for the number of data pages the operator read itself."
    ),
    phrase!(
        "table.column.memory",
        "Memory",
        "Execution Plan → Operator table: column header for the operator's peak memory use."
    ),
    phrase!(
        "breakdown.everything_else",
        "Everything else",
        "Execution Plan → Where the time went: the last line of the list, summing the operators that did not make the top entries."
    ),
];

#[cfg(test)]
mod tests;
