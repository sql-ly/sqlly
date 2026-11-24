//! What changed between two captures of the same statement, in words.
//!
//! The operator deltas in the parent module answer "which operators moved";
//! this module answers the question a reader actually has: is the new plan
//! faster, and what did the optimizer do differently. Both plans go through
//! [`analyze_plan`], so every number matches the story and the diagram.
//!
//! Tables are aligned by name (and alias, when a table is read twice), joins
//! by the set of tables below them. Engine node ids are never compared.

use std::collections::{HashMap, HashSet};

use sqlly_core::DisplayMessage;

use super::{PlanComparisonError, check_batches};
use crate::plans::ExecutionPlan;
use crate::plans::analysis::{
    EstimateDirection, NodeAnalysis, PlanAnalysis, PlanMetric, PlanObjectKind, analyze_plan,
};
use crate::plans::glossary::OperatorCategory;
use crate::plans::phrases::{
    Arg, PlanPhrase, human_cost, human_duration_ms, human_factor, human_share, phrase, rows_phrase,
    say, say0,
};
use crate::plans::story::list_phrase;

/// Two totals within this factor of each other are "about the same".
const SAME_FACTOR: f64 = 1.1;
/// Below this factor a change is given as a percentage, above it as "N
/// times".
const TIMES_FACTOR: f64 = 2.0;
/// Estimate and row-count changes listed per statement, largest first.
const MAX_DETAIL_CHANGES: usize = 5;
/// Tables named in a join description before it falls back to a count.
const MAX_JOIN_TABLES: usize = 3;

/// What a change is about, so a renderer can group or pick an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanChangeKind {
    /// Measured execution time.
    Time,
    /// The optimizer's total cost.
    Cost,
    /// The step with the largest share is a different one.
    Bottleneck,
    /// Tables are read in a different order.
    JoinOrder,
    /// A join uses a different algorithm.
    JoinAlgorithm,
    /// A table went from a scan to an index search or back.
    AccessPath,
    /// A table is read through a different index.
    IndexChoice,
    /// A per-row lookup into the table appeared or went away.
    Lookup,
    /// A table is read by only one of the plans.
    Table,
    /// A sort appeared or went away.
    Sort,
    /// One plan runs in parallel and the other does not.
    Parallelism,
    /// Sort or hash work goes to disk in only one of the plans.
    Spill,
    /// A row estimate got closer to, or further from, the actual count.
    Estimate,
    /// A table returns a very different number of rows.
    Rows,
    /// One plan was executed and the other is only an estimate, so run
    /// times and actual row counts are not compared. Listed first.
    Capture,
}

impl PlanChangeKind {
    /// Stable id for CSS classes and JSON.
    pub fn key(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Cost => "cost",
            Self::Bottleneck => "bottleneck",
            Self::JoinOrder => "join_order",
            Self::JoinAlgorithm => "join_algorithm",
            Self::AccessPath => "access_path",
            Self::IndexChoice => "index_choice",
            Self::Lookup => "lookup",
            Self::Table => "table",
            Self::Sort => "sort",
            Self::Parallelism => "parallelism",
            Self::Spill => "spill",
            Self::Estimate => "estimate",
            Self::Rows => "rows",
            Self::Capture => "capture",
        }
    }

    /// True for changes to the plan's shape, as opposed to its numbers.
    pub fn is_structural(self) -> bool {
        matches!(
            self,
            Self::JoinOrder
                | Self::JoinAlgorithm
                | Self::AccessPath
                | Self::IndexChoice
                | Self::Lookup
                | Self::Table
                | Self::Sort
                | Self::Parallelism
        )
    }
}

/// Whether a change is good news. Only measured or unambiguous changes are
/// rated; a different join algorithm or index is `Neutral` because which is
/// better depends on the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanChangeEffect {
    Better,
    Worse,
    Neutral,
}

impl PlanChangeEffect {
    pub fn key(self) -> &'static str {
        match self {
            Self::Better => "better",
            Self::Worse => "worse",
            Self::Neutral => "neutral",
        }
    }
}

/// One thing that differs between the two plans.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanChange {
    pub kind: PlanChangeKind,
    pub effect: PlanChangeEffect,
    pub statement_index: usize,
    /// Node of the baseline plan the change is about, when there is one.
    pub baseline_node_id: Option<i32>,
    /// Node of the new plan the change is about, when there is one.
    pub comparison_node_id: Option<i32>,
    pub message: DisplayMessage,
}

/// The changes of one statement.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementChanges {
    pub statement_index: usize,
    /// The verdict: faster, slower, cheaper, or unchanged.
    pub headline: DisplayMessage,
    pub headline_effect: PlanChangeEffect,
    /// No structural change: same tables, access paths, joins and order.
    pub same_shape: bool,
    /// Most important first: totals, then shape, then estimates and rows.
    pub changes: Vec<PlanChange>,
}

/// The worded comparison of two capture batches.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanChangeReport {
    pub headline: DisplayMessage,
    pub statements: Vec<StatementChanges>,
}

impl PlanChangeReport {
    /// Every change of every statement, in statement order.
    pub fn changes(&self) -> impl Iterator<Item = &PlanChange> {
        self.statements
            .iter()
            .flat_map(|statement| statement.changes.iter())
    }
}

/// Explain what changed from `baseline` to `comparison`. The batches must
/// pass the same checks as [`super::compare_plans`].
pub fn explain_plan_changes(
    baseline: &[ExecutionPlan],
    comparison: &[ExecutionPlan],
    node_limit: usize,
) -> Result<PlanChangeReport, PlanComparisonError> {
    check_batches(baseline, comparison, node_limit)?;
    let mut statements = Vec::with_capacity(baseline.len());
    let mut before_total = Some(0.0);
    let mut after_total = Some(0.0);
    for (statement_index, (before, after)) in baseline.iter().zip(comparison).enumerate() {
        let before = analyze_plan(before);
        let after = analyze_plan(after);
        before_total = before_total.zip(elapsed(&before)).map(|(sum, ms)| sum + ms);
        after_total = after_total.zip(elapsed(&after)).map(|(sum, ms)| sum + ms);
        statements.push(explain_statement_changes(statement_index, &before, &after));
    }
    let headline = match statements.as_slice() {
        [only] => only.headline.clone(),
        _ => {
            let count = statements.len();
            let changed = statements
                .iter()
                .filter(|statement| !statement.same_shape)
                .count();
            match (before_total, after_total) {
                (Some(before), Some(after)) => time_message(before, after, "batch_time").0,
                _ if changed == 0 => say("comparison.batch_same", vec![("count", count.into())]),
                _ => say(
                    "comparison.batch_changed",
                    vec![("changed", changed.into()), ("count", count.into())],
                ),
            }
        }
    };
    Ok(PlanChangeReport {
        headline,
        statements,
    })
}

/// Explain the changes of one statement from two analyses the caller already
/// holds.
pub fn explain_statement_changes(
    statement_index: usize,
    before: &PlanAnalysis,
    after: &PlanAnalysis,
) -> StatementChanges {
    let mut changes: Vec<PlanChange> = Vec::new();
    let mut push = |kind: PlanChangeKind,
                    effect: PlanChangeEffect,
                    baseline: Option<i32>,
                    comparison: Option<i32>,
                    message: DisplayMessage| {
        changes.push(PlanChange {
            kind,
            effect,
            statement_index,
            baseline_node_id: baseline,
            comparison_node_id: comparison,
            message,
        });
    };

    // ---- What the two captures have in common ------------------------------
    // An estimated plan against an executed one shares only the optimizer's
    // numbers. Say so first, or the missing times and row counts read as
    // "nothing changed".
    let mixed = match (before.has_actuals, after.has_actuals) {
        (false, true) => Some("comparison.capture_baseline_estimated"),
        (true, false) => Some("comparison.capture_new_estimated"),
        _ => None,
    };
    if let Some(key) = mixed {
        push(
            PlanChangeKind::Capture,
            PlanChangeEffect::Neutral,
            None,
            None,
            say0(key),
        );
    }

    // ---- Totals -----------------------------------------------------------
    let times = elapsed(before).zip(elapsed(after));
    let same_engine = before.dialect == after.dialect;
    let costs = before
        .total_cost
        .zip(after.total_cost)
        .filter(|(before, after)| same_engine && *before > 0.0 && *after > 0.0);
    let mut headline = None;
    if let Some((before_ms, after_ms)) = times {
        headline = Some(time_message(before_ms, after_ms, "time"));
    }
    if let Some((before_cost, after_cost)) = costs {
        let (message, effect) = cost_message(before_cost, after_cost);
        if headline.is_none() {
            headline = Some((message, effect));
        } else if effect != PlanChangeEffect::Neutral {
            push(PlanChangeKind::Cost, effect, None, None, message);
        }
    }

    // ---- Bottleneck -------------------------------------------------------
    if before.primary_metric == after.primary_metric
        && let (Some(old), Some(new)) = (
            before.bottleneck.and_then(|id| before.node(id)),
            after.bottleneck.and_then(|id| after.node(id)),
        )
    {
        let share = |node: &NodeAnalysis, metric: PlanMetric| match metric {
            PlanMetric::Time => node.metrics.time_share,
            _ => node.metrics.cost_share,
        };
        let (old_title, new_title) = (old.title(), new.title());
        if old_title.fallback() != new_title.fallback()
            && let (Some(old_share), Some(new_share)) = (
                share(old, before.primary_metric),
                share(new, after.primary_metric),
            )
        {
            let key = match before.primary_metric {
                PlanMetric::Time => "comparison.bottleneck_time",
                _ => "comparison.bottleneck_cost",
            };
            push(
                PlanChangeKind::Bottleneck,
                PlanChangeEffect::Neutral,
                Some(old.node_id),
                Some(new.node_id),
                say(
                    key,
                    vec![
                        ("before", old_title.into()),
                        ("before_share", human_share(old_share).into()),
                        ("after", new_title.into()),
                        ("after_share", human_share(new_share).into()),
                    ],
                ),
            );
        }
    }

    // ---- Shape ------------------------------------------------------------
    let old_shape = Shape::of(before);
    let new_shape = Shape::of(after);

    let old_order: Vec<&str> = old_shape.order();
    let new_order: Vec<&str> = new_shape.order();
    if old_order.len() >= 2 && old_order != new_order && same_members(&old_order, &new_order) {
        push(
            PlanChangeKind::JoinOrder,
            PlanChangeEffect::Neutral,
            None,
            None,
            say(
                "comparison.join_order",
                vec![
                    ("before", old_order.join(", ").into()),
                    ("after", new_order.join(", ").into()),
                ],
            ),
        );
    }

    for (tables, new_join) in &new_shape.joins {
        let Some(old_join) = old_shape.join_by_keys(&new_shape.keys_of(tables)) else {
            continue;
        };
        if old_join.operator_key == new_join.operator_key {
            continue;
        }
        let names: Vec<String> = tables
            .iter()
            .map(|table| new_shape.accesses[*table].label.clone())
            .collect();
        let mut args: Vec<(&'static str, Arg)> = vec![
            ("before", old_join.operator.clone().into()),
            ("after", new_join.operator.clone().into()),
        ];
        let key = if names.len() <= MAX_JOIN_TABLES {
            args.push(("tables", list_phrase(&names, MAX_JOIN_TABLES).into()));
            "comparison.join_changed"
        } else {
            args.push(("count", names.len().into()));
            "comparison.join_changed_many"
        };
        push(
            PlanChangeKind::JoinAlgorithm,
            PlanChangeEffect::Neutral,
            Some(old_join.node_id),
            Some(new_join.node_id),
            say(key, args),
        );
    }

    let mut estimate_changes: Vec<(f64, PlanChange)> = Vec::new();
    let mut row_changes: Vec<(f64, PlanChange)> = Vec::new();
    for access in &new_shape.accesses {
        let Some(old) = old_shape.access(&access.key) else {
            push(
                PlanChangeKind::Table,
                PlanChangeEffect::Neutral,
                None,
                Some(access.node_id),
                say(
                    "comparison.table_added",
                    vec![("table", access.label.clone().into())],
                ),
            );
            continue;
        };
        let table = || -> Arg { access.label.clone().into() };
        let ids = (Some(old.node_id), Some(access.node_id));
        let path = match (old.seeks, access.seeks, &old.index, &access.index) {
            (false, true, _, Some(index)) => Some((
                PlanChangeKind::AccessPath,
                say(
                    "comparison.scan_to_seek_index",
                    vec![("table", table()), ("index", index.into())],
                ),
            )),
            (false, true, _, None) => Some((
                PlanChangeKind::AccessPath,
                say("comparison.scan_to_seek", vec![("table", table())]),
            )),
            (true, false, Some(index), _) => Some((
                PlanChangeKind::AccessPath,
                say(
                    "comparison.seek_to_scan_index",
                    vec![("table", table()), ("index", index.into())],
                ),
            )),
            (true, false, None, _) => Some((
                PlanChangeKind::AccessPath,
                say("comparison.seek_to_scan", vec![("table", table())]),
            )),
            (_, _, Some(old_index), Some(new_index))
                if !old_index.eq_ignore_ascii_case(new_index) =>
            {
                Some((
                    PlanChangeKind::IndexChoice,
                    say(
                        "comparison.index_changed",
                        vec![
                            ("table", table()),
                            ("before", old_index.into()),
                            ("after", new_index.into()),
                        ],
                    ),
                ))
            }
            _ => None,
        };
        if let Some((kind, message)) = path {
            push(kind, PlanChangeEffect::Neutral, ids.0, ids.1, message);
        }

        // Estimates, when both plans were executed.
        if let (Some(old_factor), Some(new_factor)) = (old.estimate_factor, access.estimate_factor)
        {
            let worst = old_factor.max(new_factor);
            let moved = worst / old_factor.min(new_factor).max(1.0);
            if worst >= 5.0 && moved >= 2.0 {
                let improved = new_factor < old_factor;
                let key = match (improved, old_factor.min(new_factor) < 1.5) {
                    (true, true) => "comparison.estimate_fixed",
                    (true, false) => "comparison.estimate_better",
                    (false, true) => "comparison.estimate_broken",
                    (false, false) => "comparison.estimate_worse",
                };
                estimate_changes.push((
                    moved,
                    PlanChange {
                        kind: PlanChangeKind::Estimate,
                        effect: if improved {
                            PlanChangeEffect::Better
                        } else {
                            PlanChangeEffect::Worse
                        },
                        statement_index,
                        baseline_node_id: ids.0,
                        comparison_node_id: ids.1,
                        message: say(
                            key,
                            vec![
                                ("table", table()),
                                ("before", human_factor(old_factor).into()),
                                ("after", human_factor(new_factor).into()),
                            ],
                        ),
                    },
                ));
            }
        }

        // Row counts: measured against measured, else estimate against
        // estimate.
        let rows = match (old.actual_rows, access.actual_rows) {
            (Some(old_rows), Some(new_rows)) => Some((old_rows, new_rows, "comparison.rows")),
            (None, None) => old
                .estimated_rows
                .zip(access.estimated_rows)
                .map(|(old_rows, new_rows)| (old_rows, new_rows, "comparison.rows_expected")),
            _ => None,
        };
        if let Some((old_rows, new_rows, key)) = rows {
            let factor = old_rows.max(new_rows) / old_rows.min(new_rows).max(1.0);
            if factor >= 2.0 && (old_rows - new_rows).abs() >= 100.0 {
                row_changes.push((
                    factor,
                    PlanChange {
                        kind: PlanChangeKind::Rows,
                        effect: PlanChangeEffect::Neutral,
                        statement_index,
                        baseline_node_id: ids.0,
                        comparison_node_id: ids.1,
                        message: say(
                            key,
                            vec![
                                ("table", table()),
                                ("before", rows_phrase(old_rows).into()),
                                ("after", rows_phrase(new_rows).into()),
                            ],
                        ),
                    },
                ));
            }
        }
    }
    for access in &old_shape.accesses {
        if new_shape.access(&access.key).is_none() {
            push(
                PlanChangeKind::Table,
                PlanChangeEffect::Neutral,
                Some(access.node_id),
                None,
                say(
                    "comparison.table_removed",
                    vec![("table", access.label.clone().into())],
                ),
            );
        }
    }

    for (table, node_id) in &new_shape.lookups {
        if !old_shape.lookups.iter().any(|(old, _)| old == table) {
            push(
                PlanChangeKind::Lookup,
                PlanChangeEffect::Neutral,
                None,
                Some(*node_id),
                say("comparison.lookup_added", vec![("table", table.into())]),
            );
        }
    }
    for (table, node_id) in &old_shape.lookups {
        if !new_shape.lookups.iter().any(|(new, _)| new == table) {
            push(
                PlanChangeKind::Lookup,
                PlanChangeEffect::Neutral,
                Some(*node_id),
                None,
                say("comparison.lookup_removed", vec![("table", table.into())]),
            );
        }
    }

    match (old_shape.sort, new_shape.sort) {
        (None, Some(node_id)) => push(
            PlanChangeKind::Sort,
            PlanChangeEffect::Neutral,
            None,
            Some(node_id),
            say0("comparison.sort_added"),
        ),
        (Some(node_id), None) => push(
            PlanChangeKind::Sort,
            PlanChangeEffect::Neutral,
            Some(node_id),
            None,
            say0("comparison.sort_removed"),
        ),
        _ => {}
    }
    match (old_shape.parallel, new_shape.parallel) {
        (false, true) => push(
            PlanChangeKind::Parallelism,
            PlanChangeEffect::Neutral,
            None,
            None,
            say0("comparison.parallel_added"),
        ),
        (true, false) => push(
            PlanChangeKind::Parallelism,
            PlanChangeEffect::Neutral,
            None,
            None,
            say0("comparison.parallel_removed"),
        ),
        _ => {}
    }
    match (old_shape.spill, new_shape.spill) {
        (None, Some(node_id)) => push(
            PlanChangeKind::Spill,
            PlanChangeEffect::Worse,
            None,
            Some(node_id),
            say0("comparison.spill_added"),
        ),
        (Some(node_id), None) => push(
            PlanChangeKind::Spill,
            PlanChangeEffect::Better,
            Some(node_id),
            None,
            say0("comparison.spill_removed"),
        ),
        _ => {}
    }

    for list in [&mut estimate_changes, &mut row_changes] {
        list.sort_by(|a, b| b.0.total_cmp(&a.0));
        list.truncate(MAX_DETAIL_CHANGES);
    }
    changes.extend(estimate_changes.into_iter().map(|(_, change)| change));
    changes.extend(row_changes.into_iter().map(|(_, change)| change));

    let same_shape = !changes.iter().any(|change| change.kind.is_structural());
    let (headline, headline_effect) = headline.unwrap_or_else(|| {
        let key = if same_shape {
            "comparison.same_shape"
        } else {
            "comparison.shape_changed"
        };
        (say0(key), PlanChangeEffect::Neutral)
    });
    StatementChanges {
        statement_index,
        headline,
        headline_effect,
        same_shape,
        changes,
    }
}

/// Execution time of an analysed plan: the stated wall-clock time, else the
/// sum of the operator times.
fn elapsed(analysis: &PlanAnalysis) -> Option<f64> {
    analysis.execution_time_ms.or(analysis.total_time_ms)
}

/// `stem` is `time` (one statement) or `batch_time` (the whole batch).
fn time_message(before: f64, after: f64, stem: &str) -> (DisplayMessage, PlanChangeEffect) {
    let factor = before.max(after) / before.min(after).max(f64::MIN_POSITIVE);
    let mut args: Vec<(&'static str, Arg)> = vec![
        ("before", human_duration_ms(before).into()),
        ("after", human_duration_ms(after).into()),
    ];
    let (suffix, effect) = if factor < SAME_FACTOR || !factor.is_finite() {
        ("same", PlanChangeEffect::Neutral)
    } else {
        let faster = after < before;
        let effect = if faster {
            PlanChangeEffect::Better
        } else {
            PlanChangeEffect::Worse
        };
        let suffix = if factor >= TIMES_FACTOR {
            args.push(("factor", human_factor(factor).into()));
            if faster { "faster" } else { "slower" }
        } else {
            args.push((
                "percent",
                human_share((after - before).abs() / before).into(),
            ));
            if faster {
                "faster_percent"
            } else {
                "slower_percent"
            }
        };
        (suffix, effect)
    };
    (say(&format!("comparison.{stem}_{suffix}"), args), effect)
}

fn cost_message(before: f64, after: f64) -> (DisplayMessage, PlanChangeEffect) {
    let factor = before.max(after) / before.min(after);
    let mut args: Vec<(&'static str, Arg)> = vec![
        ("before", human_cost(before).into()),
        ("after", human_cost(after).into()),
    ];
    if factor < SAME_FACTOR {
        return (say("comparison.cost_same", args), PlanChangeEffect::Neutral);
    }
    let lower = after < before;
    let suffix = if factor >= TIMES_FACTOR {
        args.push(("factor", human_factor(factor).into()));
        if lower { "lower" } else { "higher" }
    } else {
        args.push((
            "percent",
            human_share((after - before).abs() / before).into(),
        ));
        if lower {
            "lower_percent"
        } else {
            "higher_percent"
        }
    };
    let effect = if lower {
        PlanChangeEffect::Better
    } else {
        PlanChangeEffect::Worse
    };
    (say(&format!("comparison.cost_{suffix}"), args), effect)
}

fn same_members(left: &[&str], right: &[&str]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut left: Vec<&str> = left.to_vec();
    let mut right: Vec<&str> = right.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

/// The index an index-driven fetch gets its rows from: the first index
/// named in the operators below it.
fn index_below(analysis: &PlanAnalysis, node: &NodeAnalysis, look: bool) -> Option<String> {
    if !look {
        return None;
    }
    let mut pending: Vec<i32> = node.children.clone();
    let mut visited = 0usize;
    while let Some(id) = pending.pop() {
        visited += 1;
        if visited > analysis.nodes.len() {
            break;
        }
        let Some(below) = analysis.node(id) else {
            continue;
        };
        if let Some(index) = &below.index {
            return Some(index.clone());
        }
        pending.extend(below.children.iter().rev());
    }
    None
}

/// One read of a table.
struct Access {
    /// Lower-cased name, alias and occurrence: the alignment key.
    key: String,
    /// The table as a sentence names it.
    label: String,
    node_id: i32,
    /// An index search rather than a scan.
    seeks: bool,
    index: Option<String>,
    actual_rows: Option<f64>,
    estimated_rows: Option<f64>,
    estimate_factor: Option<f64>,
}

struct JoinShape {
    node_id: i32,
    operator_key: &'static str,
    operator: String,
}

/// The parts of a plan the comparison aligns.
struct Shape {
    /// Table reads in execution order.
    accesses: Vec<Access>,
    by_key: HashMap<String, usize>,
    /// Joins keyed by the sorted positions (in `accesses`) of the tables
    /// below them, in execution order.
    joins: Vec<(Vec<usize>, JoinShape)>,
    join_index: HashMap<Vec<String>, usize>,
    /// Tables with a per-row lookup, with the lookup's node.
    lookups: Vec<(String, i32)>,
    sort: Option<i32>,
    spill: Option<i32>,
    parallel: bool,
}

impl Shape {
    fn of(analysis: &PlanAnalysis) -> Self {
        let mut accesses: Vec<Access> = Vec::new();
        let mut by_key = HashMap::new();
        let mut occurrences: HashMap<String, usize> = HashMap::new();
        let mut lookups: Vec<(String, i32)> = Vec::new();
        let mut seen_lookup = HashSet::new();
        let mut sort = None;
        let mut spill = None;
        let mut parallel = false;
        // Position in `accesses` of the table each node reads.
        let mut access_of: HashMap<i32, usize> = HashMap::new();

        for node_id in &analysis.execution_order {
            let Some(node) = analysis.node(*node_id) else {
                continue;
            };
            if node.flags.never_executed {
                continue;
            }
            parallel |= node.flags.parallel;
            if node.flags.spilled && spill.is_none() {
                spill = Some(node.node_id);
            }
            let category = node.category();
            if category == OperatorCategory::Sort && sort.is_none() {
                sort = Some(node.node_id);
            }
            let Some(object) = node
                .object
                .as_ref()
                .filter(|object| object.kind == PlanObjectKind::Table)
            else {
                continue;
            };
            let identity = format!(
                "{}|{}",
                object.name.to_ascii_lowercase(),
                object
                    .alias
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
            );
            // A fetch of table rows that an index found is a lookup beside
            // the read of that table (SQL Server Key Lookup after an Index
            // Seek on the same table). Where the plan names the table only on
            // the fetch (PostgreSQL Bitmap Heap Scan over a Bitmap Index
            // Scan, Oracle TABLE ACCESS BY INDEX ROWID over an INDEX RANGE
            // SCAN), the fetch is the read of the table, through the index
            // below it.
            let fetch_is_the_read =
                category == OperatorCategory::Lookup && !occurrences.contains_key(&identity);
            match category {
                OperatorCategory::Lookup if !fetch_is_the_read => {
                    if seen_lookup.insert(object.name.to_ascii_lowercase()) {
                        lookups.push((object.name.clone(), node.node_id));
                    }
                }
                OperatorCategory::Scan | OperatorCategory::Seek | OperatorCategory::Lookup => {
                    let occurrence = occurrences.entry(identity.clone()).or_default();
                    let key = format!("{identity}#{occurrence}");
                    *occurrence += 1;
                    let metrics = &node.metrics;
                    by_key.insert(key.clone(), accesses.len());
                    access_of.insert(node.node_id, accesses.len());
                    accesses.push(Access {
                        key,
                        label: object.label(),
                        node_id: node.node_id,
                        seeks: category != OperatorCategory::Scan,
                        index: node
                            .index
                            .clone()
                            .or_else(|| index_below(analysis, node, fetch_is_the_read)),
                        actual_rows: metrics.rows_out,
                        estimated_rows: metrics.estimated_rows_total,
                        estimate_factor: metrics.estimate_factor.map(|factor| {
                            if metrics.estimate_direction == Some(EstimateDirection::Exact) {
                                1.0
                            } else {
                                factor
                            }
                        }),
                    });
                }
                _ => {}
            }
        }

        // Tables below each join: execution order visits inputs before the
        // operator that consumes them, so one pass merges child sets upward.
        let mut below: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut joins = Vec::new();
        let mut join_index = HashMap::new();
        for node_id in &analysis.execution_order {
            let Some(node) = analysis.node(*node_id) else {
                continue;
            };
            let mut tables: Vec<usize> = access_of.get(node_id).copied().into_iter().collect();
            for child in &node.children {
                if let Some(mut child_tables) = below.remove(child) {
                    if child_tables.len() > tables.len() {
                        std::mem::swap(&mut tables, &mut child_tables);
                    }
                    tables.extend(child_tables);
                }
            }
            let is_join = matches!(
                node.category(),
                OperatorCategory::JoinNestedLoop
                    | OperatorCategory::JoinHash
                    | OperatorCategory::JoinMerge
                    | OperatorCategory::Join
            );
            if is_join && tables.len() >= 2 && !node.flags.never_executed {
                let mut sorted = tables.clone();
                sorted.sort_unstable();
                let mut keys: Vec<String> = sorted
                    .iter()
                    .map(|table: &usize| accesses[*table].key.clone())
                    .collect();
                keys.sort_unstable();
                join_index.entry(keys).or_insert(joins.len());
                joins.push((
                    sorted,
                    JoinShape {
                        node_id: node.node_id,
                        operator_key: node.operator.key,
                        operator: node.operator.name.clone(),
                    },
                ));
            }
            below.insert(*node_id, tables);
        }

        Self {
            accesses,
            by_key,
            joins,
            join_index,
            lookups,
            sort,
            spill,
            parallel,
        }
    }

    fn access(&self, key: &str) -> Option<&Access> {
        self.by_key.get(key).map(|index| &self.accesses[*index])
    }

    /// The join of this shape over the tables with these alignment keys
    /// (sorted), which come from the other shape's [`Shape::keys_of`].
    fn join_by_keys(&self, keys: &[String]) -> Option<&JoinShape> {
        self.join_index.get(keys).map(|index| &self.joins[*index].1)
    }

    fn order(&self) -> Vec<&str> {
        self.accesses
            .iter()
            .map(|access| access.label.as_str())
            .collect()
    }

    fn keys_of(&self, tables: &[usize]) -> Vec<String> {
        let mut keys: Vec<String> = tables
            .iter()
            .map(|table| self.accesses[*table].key.clone())
            .collect();
        keys.sort_unstable();
        keys
    }
}

pub(crate) static PHRASES: &[PlanPhrase] = &[
    phrase!(
        "comparison.time_faster",
        "The new plan ran in {after}, {factor} times faster than the baseline's {before}.",
        "Execution Plan → Compare: headline when the new plan is at least twice as fast; {after} and {before} are durations such as 80 ms, {factor} a number."
    ),
    phrase!(
        "comparison.time_slower",
        "The new plan ran in {after}, {factor} times slower than the baseline's {before}.",
        "Execution Plan → Compare: headline when the new plan is at least twice as slow; {after} and {before} are durations such as 80 ms, {factor} a number."
    ),
    phrase!(
        "comparison.time_faster_percent",
        "The new plan ran in {after}, {percent} less than the baseline's {before}.",
        "Execution Plan → Compare: headline when the new plan is somewhat faster; {after} and {before} are durations, {percent} a percentage such as 30%."
    ),
    phrase!(
        "comparison.time_slower_percent",
        "The new plan ran in {after}, {percent} more than the baseline's {before}.",
        "Execution Plan → Compare: headline when the new plan is somewhat slower; {after} and {before} are durations, {percent} a percentage such as 30%."
    ),
    phrase!(
        "comparison.time_same",
        "Both plans ran in about the same time: {before} for the baseline, {after} for the new plan.",
        "Execution Plan → Compare: headline when the two execution times are within ten percent; {before} and {after} are durations."
    ),
    phrase!(
        "comparison.batch_time_faster",
        "The new batch ran in {after}, {factor} times faster than the baseline's {before}.",
        "Execution Plan → Compare: headline for a batch of several statements that is at least twice as fast; {after} and {before} are durations, {factor} a number."
    ),
    phrase!(
        "comparison.batch_time_slower",
        "The new batch ran in {after}, {factor} times slower than the baseline's {before}.",
        "Execution Plan → Compare: headline for a batch of several statements that is at least twice as slow; {after} and {before} are durations, {factor} a number."
    ),
    phrase!(
        "comparison.batch_time_faster_percent",
        "The new batch ran in {after}, {percent} less than the baseline's {before}.",
        "Execution Plan → Compare: headline for a batch of several statements that is somewhat faster; {after} and {before} are durations, {percent} a percentage."
    ),
    phrase!(
        "comparison.batch_time_slower_percent",
        "The new batch ran in {after}, {percent} more than the baseline's {before}.",
        "Execution Plan → Compare: headline for a batch of several statements that is somewhat slower; {after} and {before} are durations, {percent} a percentage."
    ),
    phrase!(
        "comparison.batch_time_same",
        "Both batches ran in about the same time: {before} for the baseline, {after} for the new one.",
        "Execution Plan → Compare: headline for a batch of several statements whose total times are within ten percent; {before} and {after} are durations."
    ),
    phrase!(
        "comparison.batch_same",
        "None of the {count} statements changed shape.",
        "Execution Plan → Compare: headline for a batch without timings in which no statement's plan changed; {count} is a number of at least 2."
    ),
    phrase!(
        "comparison.batch_changed",
        "{changed} of the {count} statements have a different plan shape.",
        "Execution Plan → Compare: headline for a batch without timings; {changed} and {count} are numbers, {count} at least 2."
    ),
    phrase!(
        "comparison.capture_baseline_estimated",
        "The baseline is an estimated plan and the new plan was executed, so only the optimizer's costs, row estimates and the shape of the plans are compared. Run times and actual row counts exist for the new plan only.",
        "Execution Plan → Compare: first line when the baseline plan was not executed and the new plan was."
    ),
    phrase!(
        "comparison.capture_new_estimated",
        "The baseline was executed and the new plan is an estimated plan, so only the optimizer's costs, row estimates and the shape of the plans are compared. Run times and actual row counts exist for the baseline only.",
        "Execution Plan → Compare: first line when the baseline plan was executed and the new plan was not."
    ),
    phrase!(
        "comparison.cost_lower",
        "The optimizer's cost estimate went from {before} to {after}, {factor} times lower.",
        "Execution Plan → Compare: the estimated cost of the new plan is at most half the baseline's; {before} and {after} are cost numbers, {factor} a number."
    ),
    phrase!(
        "comparison.cost_higher",
        "The optimizer's cost estimate went from {before} to {after}, {factor} times higher.",
        "Execution Plan → Compare: the estimated cost of the new plan is at least twice the baseline's; {before} and {after} are cost numbers, {factor} a number."
    ),
    phrase!(
        "comparison.cost_lower_percent",
        "The optimizer's cost estimate went from {before} to {after}, {percent} lower.",
        "Execution Plan → Compare: the estimated cost of the new plan is somewhat lower; {before} and {after} are cost numbers, {percent} a percentage."
    ),
    phrase!(
        "comparison.cost_higher_percent",
        "The optimizer's cost estimate went from {before} to {after}, {percent} higher.",
        "Execution Plan → Compare: the estimated cost of the new plan is somewhat higher; {before} and {after} are cost numbers, {percent} a percentage."
    ),
    phrase!(
        "comparison.cost_same",
        "The optimizer's cost estimate is about the same: {before} for the baseline, {after} for the new plan.",
        "Execution Plan → Compare: headline for two estimated plans whose costs are within ten percent; {before} and {after} are cost numbers."
    ),
    phrase!(
        "comparison.same_shape",
        "The two plans have the same shape: the same tables, access paths and joins in the same order.",
        "Execution Plan → Compare: headline when nothing structural differs and there are no times or costs to compare."
    ),
    phrase!(
        "comparison.shape_changed",
        "The plan changed shape. Neither capture has times or costs that can be compared.",
        "Execution Plan → Compare: headline when the plans differ structurally but carry no comparable numbers."
    ),
    phrase!(
        "comparison.bottleneck_time",
        "The slowest step was {before} with {before_share} of the time; now it is {after} with {after_share}.",
        "Execution Plan → Compare: the step taking the most time differs between the plans; {before} and {after} are operator titles, {before_share} and {after_share} percentages."
    ),
    phrase!(
        "comparison.bottleneck_cost",
        "The costliest step was {before} with {before_share} of the estimated cost; now it is {after} with {after_share}.",
        "Execution Plan → Compare: the step with the largest estimated cost differs between the plans; {before} and {after} are operator titles, {before_share} and {after_share} percentages."
    ),
    phrase!(
        "comparison.join_order",
        "Tables are now read in the order {after}. The baseline read them in the order {before}.",
        "Execution Plan → Compare: the join order changed; {after} and {before} are comma-separated lists of table names."
    ),
    phrase!(
        "comparison.join_changed",
        "The join of {tables} changed from {before} to {after}.",
        "Execution Plan → Compare: a join uses a different algorithm; {tables} is a comma-separated list of table names, {before} and {after} are operator names such as Nested Loops and Hash Match."
    ),
    phrase!(
        "comparison.join_changed_many",
        "A join over {count} tables changed from {before} to {after}.",
        "Execution Plan → Compare: a join over many tables uses a different algorithm; {count} is a number above 3, {before} and {after} are operator names."
    ),
    phrase!(
        "comparison.scan_to_seek_index",
        "{table} is now searched through the index {index}. The baseline scanned it.",
        "Execution Plan → Compare: a table that was scanned is now read by an index search; {table} is a table name, {index} an index name."
    ),
    phrase!(
        "comparison.scan_to_seek",
        "{table} is now searched by key. The baseline scanned it.",
        "Execution Plan → Compare: a table that was scanned is now read by a key search and the plan names no index; {table} is a table name."
    ),
    phrase!(
        "comparison.seek_to_scan_index",
        "{table} is now scanned. The baseline searched it through the index {index}.",
        "Execution Plan → Compare: a table that was read by an index search is now scanned; {table} is a table name, {index} the index the baseline used."
    ),
    phrase!(
        "comparison.seek_to_scan",
        "{table} is now scanned. The baseline searched it by key.",
        "Execution Plan → Compare: a table that was read by a key search is now scanned; {table} is a table name."
    ),
    phrase!(
        "comparison.index_changed",
        "{table} is now read through the index {after} instead of {before}.",
        "Execution Plan → Compare: a table is read through a different index; {table} is a table name, {after} and {before} index names."
    ),
    phrase!(
        "comparison.table_added",
        "The new plan also reads {table}.",
        "Execution Plan → Compare: the new plan reads a table the baseline did not; {table} is a table name."
    ),
    phrase!(
        "comparison.table_removed",
        "The new plan no longer reads {table}.",
        "Execution Plan → Compare: the baseline read a table the new plan does not; {table} is a table name."
    ),
    phrase!(
        "comparison.lookup_added",
        "The new plan fetches rows from {table} one at a time after an index search. The baseline did not need that lookup.",
        "Execution Plan → Compare: a key or bookmark lookup appears only in the new plan; {table} is a table name."
    ),
    phrase!(
        "comparison.lookup_removed",
        "The new plan no longer fetches rows from {table} one at a time after an index search.",
        "Execution Plan → Compare: a key or bookmark lookup appears only in the baseline plan; {table} is a table name."
    ),
    phrase!(
        "comparison.sort_added",
        "The new plan sorts rows. The baseline had no sort step.",
        "Execution Plan → Compare: a sort operator appears only in the new plan."
    ),
    phrase!(
        "comparison.sort_removed",
        "The new plan has no sort step. The baseline sorted rows.",
        "Execution Plan → Compare: a sort operator appears only in the baseline plan."
    ),
    phrase!(
        "comparison.parallel_added",
        "The new plan runs partly in parallel. The baseline did not.",
        "Execution Plan → Compare: only the new plan has parallel operators."
    ),
    phrase!(
        "comparison.parallel_removed",
        "The new plan does not run in parallel. The baseline did.",
        "Execution Plan → Compare: only the baseline plan has parallel operators."
    ),
    phrase!(
        "comparison.spill_added",
        "The new plan writes sort or hash work to disk. The baseline kept it in memory.",
        "Execution Plan → Compare: only the new plan has an operator that spilled to disk."
    ),
    phrase!(
        "comparison.spill_removed",
        "The new plan keeps its sort and hash work in memory. The baseline wrote some of it to disk.",
        "Execution Plan → Compare: only the baseline plan has an operator that spilled to disk."
    ),
    phrase!(
        "comparison.estimate_fixed",
        "The row estimate for {table} was off by a factor of {before} in the baseline and is close now.",
        "Execution Plan → Compare: a table's row estimate became accurate; {table} is a table name, {before} a number."
    ),
    phrase!(
        "comparison.estimate_better",
        "The row estimate for {table} improved: it is off by a factor of {after}, down from {before}.",
        "Execution Plan → Compare: a table's row estimate got closer to the actual count; {table} is a table name, {after} and {before} numbers."
    ),
    phrase!(
        "comparison.estimate_broken",
        "The row estimate for {table} was close in the baseline and is now off by a factor of {after}.",
        "Execution Plan → Compare: a table's row estimate became inaccurate; {table} is a table name, {after} a number."
    ),
    phrase!(
        "comparison.estimate_worse",
        "The row estimate for {table} got worse: it is off by a factor of {after}, up from {before}.",
        "Execution Plan → Compare: a table's row estimate moved further from the actual count; {table} is a table name, {after} and {before} numbers."
    ),
    phrase!(
        "comparison.rows",
        "{table} returned {before} in the baseline and {after} in the new plan.",
        "Execution Plan → Compare: a table read returns a very different number of rows; {table} is a table name, {before} and {after} row-count phrases such as '50K rows'."
    ),
    phrase!(
        "comparison.rows_expected",
        "The optimizer expected {before} from {table} in the baseline and expects {after} in the new plan.",
        "Execution Plan → Compare: on two estimated plans, a table read is expected to return a very different number of rows; {table} is a table name, {before} and {after} row-count phrases."
    ),
];
