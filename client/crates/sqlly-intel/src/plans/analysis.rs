//! Per-node facts and derived metrics, computed once for both plan viewers.
//!
//! [`analyze_plan`] walks a parsed plan a fixed number of times (no pass is
//! quadratic) and answers, for every operator: what it is (glossary entry),
//! what it touches (table, index, predicates in readable form), how much of
//! the plan's time, cost and I/O is its own, how many rows went in and out,
//! how far the row estimate was off, and which flags apply. The story, the
//! diagram model, the operator table and the plan comparison all read these
//! numbers instead of recomputing them.
//!
//! Engines report time and cost with different meanings; the rules applied
//! here are:
//!
//! * `PlanNode::actual_time_ms` is inclusive (the node and its inputs, all
//!   loops) on every engine except two cases. DuckDB reports each operator's
//!   own time, so inclusive time is rebuilt bottom-up. SQL Server batch-mode
//!   operators report their own time while row-mode operators report
//!   cumulative time, so the two are told apart by `execution_mode`.
//! * PostgreSQL attaches CTEs and InitPlans as extra children of the node
//!   that owns them, but their time is spent inside whichever node first
//!   reads them (a `CTE Scan`, or the node whose condition uses the InitPlan
//!   result). That time is taken off the reader, not off the owner, so it is
//!   not counted twice.
//! * `PlanNode::estimated_cost` is a cumulative subtree cost. PostgreSQL,
//!   MySQL and Oracle print the inner side of a nested loop per execution,
//!   so it is multiplied by the estimated number of outer rows (never past
//!   what the join itself costs). A node that costs less than its inputs
//!   (a `LIMIT` that stops early) scales its subtree down so shares still add
//!   up to the plan total.
//! * Page counters are inclusive on PostgreSQL and Oracle and per operator
//!   on SQL Server.

use std::collections::HashMap;

use sqlly_core::DisplayMessage;

use crate::plans::dialects::PlanDialect;
use crate::plans::glossary::{
    OperatorCategory, OperatorEntry, OperatorInfo, operator_lookup, operator_match_for,
};
use crate::plans::import::{PlanEngine, PlanFormat};
use crate::plans::model::{ExecutionPlan, PlanNode};
use crate::plans::phrases::say;
use crate::plans::predicate::{
    ExpressionNames, ReadablePredicate, readable_expression, readable_predicate,
};
use crate::plans::warning::PlanWarning;

/// A metric a renderer can colour or size nodes by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PlanMetric {
    /// The node's own (exclusive) time.
    #[default]
    Time,
    /// The node's own (exclusive) estimated cost.
    Cost,
    /// Rows the node produced.
    Rows,
    /// How far the row estimate was from the actual count.
    EstimateError,
    /// Pages the node itself read.
    Io,
}

impl PlanMetric {
    pub const ALL: [PlanMetric; 5] = [
        Self::Time,
        Self::Cost,
        Self::Rows,
        Self::EstimateError,
        Self::Io,
    ];

    /// Stable identifier (`time`, `cost`, `rows`, `estimate`, `io`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Cost => "cost",
            Self::Rows => "rows",
            Self::EstimateError => "estimate",
            Self::Io => "io",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|metric| metric.key() == key)
    }

    /// Short label for a metric picker.
    pub fn label(self) -> DisplayMessage {
        match self {
            Self::Time => say("metric.time", Vec::new()),
            Self::Cost => say("metric.cost", Vec::new()),
            Self::Rows => say("metric.rows", Vec::new()),
            Self::EstimateError => say("metric.estimate", Vec::new()),
            Self::Io => say("metric.io", Vec::new()),
        }
    }
}

/// Which way a row estimate missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EstimateDirection {
    /// Within rounding of the actual count.
    Exact,
    /// The optimizer expected fewer rows than arrived.
    Under,
    /// The optimizer expected more rows than arrived.
    Over,
}

impl EstimateDirection {
    pub fn key(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Under => "under",
            Self::Over => "over",
        }
    }
}

/// 0..1 intensity per metric, for heat-map colouring. 0 means "nothing to
/// show" (including "the plan does not carry this metric").
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NodeHeat {
    /// Own time as a share of the plan's time.
    pub time: f32,
    /// Own cost as a share of the plan's cost.
    pub cost: f32,
    /// Rows produced, on a log scale against the busiest node.
    pub rows: f32,
    /// Estimate error on a log scale: 0 at exact, 1 at 1000x off or worse.
    pub estimate: f32,
    /// Own page reads as a share of the plan's page reads.
    pub io: f32,
}

impl NodeHeat {
    pub fn get(&self, metric: PlanMetric) -> f32 {
        match metric {
            PlanMetric::Time => self.time,
            PlanMetric::Cost => self.cost,
            PlanMetric::Rows => self.rows,
            PlanMetric::EstimateError => self.estimate,
            PlanMetric::Io => self.io,
        }
    }
}

/// Everything numeric about one node. Times are milliseconds, shares are
/// 0..1 fractions of the plan total, row counts are totals across loops
/// unless the name says otherwise.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeMetrics {
    /// Time in this node and everything below it.
    pub inclusive_time_ms: Option<f64>,
    /// Time in this node alone.
    pub self_time_ms: Option<f64>,
    /// `self_time_ms` as a share of the plan's time. Shares of all nodes add
    /// up to 1.
    pub time_share: Option<f64>,
    /// `inclusive_time_ms` as a share of the plan's time, capped at 1.
    pub inclusive_time_share: Option<f64>,
    /// Estimated cost of this node and everything below it, after the
    /// per-engine corrections described in the module docs.
    pub inclusive_cost: Option<f64>,
    /// Estimated cost of this node alone.
    pub self_cost: Option<f64>,
    /// `self_cost` as a share of the plan's cost. Shares add up to 1.
    pub cost_share: Option<f64>,
    pub inclusive_cost_share: Option<f64>,
    /// Times the node ran (actual plans).
    pub loops: Option<f64>,
    /// Rows produced, all loops.
    pub rows_out: Option<f64>,
    /// Rows produced per loop, comparable with `estimated_rows`.
    pub rows_per_loop: Option<f64>,
    /// The actual count `estimate_factor` compares `estimated_rows` with.
    /// It is `rows_per_loop` except on MySQL / MariaDB table nodes that fold
    /// a condition in, where the estimate is for the rows examined.
    pub estimate_compared_rows: Option<f64>,
    /// Set when `loops` counts parallel processes (the leader and its
    /// workers each ran the node once) and not repeated executions.
    pub processes: Option<f64>,
    /// Rows that reached the node: what its inputs produced, or for a leaf
    /// what it read from storage before filtering.
    pub rows_in: Option<f64>,
    /// Estimated rows per execution; `None` when the engine gives none.
    pub estimated_rows: Option<f64>,
    /// Estimated rows across the estimated number of executions.
    pub estimated_rows_total: Option<f64>,
    /// How many times off the estimate was (always >= 1).
    pub estimate_factor: Option<f64>,
    pub estimate_direction: Option<EstimateDirection>,
    /// Rows read and then thrown away by this node's own predicates.
    pub rows_removed: Option<f64>,
    /// `rows_removed` as a share of the rows read.
    pub rows_removed_share: Option<f64>,
    /// Pages read by this node and everything below it.
    pub io_pages_inclusive: Option<f64>,
    /// Pages read by this node alone.
    pub io_pages_self: Option<f64>,
    /// `io_pages_self` as a share of the plan's page reads.
    pub io_share: Option<f64>,
    /// Peak memory the node used, kB.
    pub memory_kb: Option<f64>,
    /// Sort or hash work written to disk, kB, when the engine says how much.
    pub spilled_kb: Option<f64>,
    pub heat: NodeHeat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeFlags {
    /// The node with the largest own time (own cost on estimated plans).
    pub bottleneck: bool,
    /// An executed plan shows the node never ran.
    pub never_executed: bool,
    /// Sort or hash work did not fit in memory and went to disk.
    pub spilled: bool,
    /// The node runs in a parallel part of the plan.
    pub parallel: bool,
    /// On the heaviest root-to-leaf chain.
    pub on_critical_path: bool,
    /// The row estimate is off by 2x or more and no input of this node was
    /// off by a comparable factor in the same direction: the miss starts
    /// here and the operators above only inherit it.
    pub misestimate_origin: bool,
    /// The node filters the output of a window operator directly below it
    /// (a top-N or "rows ranked beyond N" query such as `WHERE rn <= 10`).
    /// Optimizers have no statistics for the result of a window function,
    /// so a row estimate that is off here is not a statistics problem and
    /// is never a `misestimate_origin`.
    pub filters_window_result: bool,
}

/// What kind of object a node reads or writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PlanObjectKind {
    #[default]
    Table,
    /// A CTE result.
    Cte,
    /// An internal temporary table (MySQL `<temporary>`).
    Temporary,
}

/// The table, view or CTE a node reads or writes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanObject {
    pub name: String,
    pub schema: Option<String>,
    /// Alias the statement uses, when it differs from the name.
    pub alias: Option<String>,
    pub kind: PlanObjectKind,
}

impl PlanObject {
    /// `orders`, or `orders o` when the statement aliases it. Predicates in
    /// the plan use the alias, so the label keeps both.
    pub fn label(&self) -> String {
        match &self.alias {
            Some(alias) if !alias.eq_ignore_ascii_case(&self.name) => {
                format!("{} {alias}", self.name)
            }
            _ => self.name.clone(),
        }
    }
}

/// A subtree the diagram can draw a frame around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanGroupKind {
    /// A subquery run as its own unit (PostgreSQL InitPlan / SubPlan).
    Subplan,
    /// The definition of a CTE.
    Cte,
    /// Operators that run on parallel workers.
    Parallel,
}

impl PlanGroupKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Subplan => "subplan",
            Self::Cte => "cte",
            Self::Parallel => "parallel",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanGroup {
    pub kind: PlanGroupKind,
    pub label: DisplayMessage,
    /// Top node of the group.
    pub root_id: i32,
    /// Every node in the group, root first, in plan order.
    pub node_ids: Vec<i32>,
}

/// Facts and metrics for one node.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeAnalysis {
    pub node_id: i32,
    pub parent_id: Option<i32>,
    /// Direct inputs, in plan order.
    pub children: Vec<i32>,
    pub depth: usize,
    /// Nodes in the subtree, this node included.
    pub subtree_size: usize,
    /// Glossary entry: key, display name, category, description, advice.
    pub operator: OperatorInfo,
    pub object: Option<PlanObject>,
    pub index: Option<String>,
    /// Condition the index or access path satisfies.
    pub seek_predicate: Option<ReadablePredicate>,
    /// Condition applied to rows after they are read.
    pub filter: Option<ReadablePredicate>,
    pub join_predicate: Option<ReadablePredicate>,
    /// `Left`, `Semi`, `Anti`, ... as the engine names it; `None` for a
    /// plain inner join.
    pub join_type: Option<String>,
    /// Readable sort keys.
    pub sort_keys: Vec<String>,
    /// Readable grouping keys.
    pub group_keys: Vec<String>,
    /// Name of the subplan or CTE this node is the top of.
    pub subplan_name: Option<String>,
    /// The plan line exactly as the engine printed it, for engines whose
    /// line already names the table and index (SQLite's `SEARCH c USING
    /// INTEGER PRIMARY KEY (rowid=?)`). The title shows it unchanged.
    pub engine_line: Option<String>,
    pub metrics: NodeMetrics,
    pub flags: NodeFlags,
}

impl NodeAnalysis {
    pub fn category(&self) -> OperatorCategory {
        self.operator.category
    }

    /// Operator plus what it touches, for node titles and lists:
    /// `Seq Scan on orders`, `Index Seek on Orders using IX_Orders_Date`.
    pub fn title(&self) -> DisplayMessage {
        if let Some(line) = &self.engine_line {
            return DisplayMessage::raw(line.clone());
        }
        let operator = self.operator.name.clone();
        match (&self.object, &self.index) {
            (Some(object), Some(index)) => say(
                "title.table_index",
                vec![
                    ("operator", operator.into()),
                    ("table", object.label().into()),
                    ("index", index.into()),
                ],
            ),
            (Some(object), None) => say(
                "title.table",
                vec![
                    ("operator", operator.into()),
                    ("table", object.label().into()),
                ],
            ),
            (None, Some(index)) => say(
                "title.index",
                vec![("operator", operator.into()), ("index", index.into())],
            ),
            (None, None) => DisplayMessage::raw(operator),
        }
    }
}

/// The analysed plan. `nodes` is in the same order as `ExecutionPlan::nodes`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanAnalysis {
    /// Engine the numbers were interpreted for: the plan's own dialect, the
    /// one the caller passed, or a guess from the operator vocabulary.
    pub dialect: PlanDialect,
    /// The engine the plan came from. Wider than `dialect`: a CockroachDB,
    /// Trino or Spark plan has its own engine here and the dialect whose
    /// arithmetic it shares (PostgreSQL) in `dialect`.
    pub engine: PlanEngine,
    /// The plan was executed (it carries actual rows or timings).
    pub has_actuals: bool,
    /// Per-operator timings are present and fine enough to use.
    pub has_timing: bool,
    /// The step the source format prints operator times in, when it is
    /// coarse: 10 ms for an Oracle `DBMS_XPLAN` A-Time column and for the
    /// boxes of a DuckDB text profile. `None` when times are printed finer
    /// than anything the analysis would round to.
    pub time_resolution_ms: Option<f64>,
    /// The plan has operator times, but in steps too large to say which
    /// operator took the time (a 10 ms statement timed in 10 ms steps).
    /// Node times and shares are left out, `has_timing` is false and
    /// `primary_metric` falls back to cost or rows.
    pub timing_too_coarse: bool,
    /// The engine reports optimizer costs.
    pub has_costs: bool,
    /// The engine reports row estimates.
    pub has_estimates: bool,
    /// Per-operator page counts are present.
    pub has_io: bool,
    /// Time attributed to operators (the sum of every node's own time).
    pub total_time_ms: Option<f64>,
    /// Wall-clock execution time the document states, when it does.
    pub execution_time_ms: Option<f64>,
    pub planning_time_ms: Option<f64>,
    /// Plan cost after the per-engine corrections.
    pub total_cost: Option<f64>,
    /// Pages read by all operators.
    pub total_io_pages: Option<f64>,
    /// Rows the plan returns (actual when executed, else estimated).
    pub result_rows: Option<f64>,
    /// Largest `rows_out` (or estimate) of any node; scales edge widths.
    pub max_rows: f64,
    /// The metric that best ranks this plan's operators: time when timed,
    /// else cost, else rows.
    pub primary_metric: PlanMetric,
    pub nodes: Vec<NodeAnalysis>,
    /// Top-level nodes, in plan order.
    pub roots: Vec<i32>,
    /// Node ids in the order the work happens: inputs before the operator
    /// that consumes them, the build side of a hash join before its probe.
    pub execution_order: Vec<i32>,
    /// Node with the largest own time (own cost on estimated plans).
    pub bottleneck: Option<i32>,
    /// Heaviest chain from a root down to a leaf.
    pub critical_path: Vec<i32>,
    /// Subplans, CTE definitions and parallel zones.
    pub groups: Vec<PlanGroup>,
    /// Internal names the plan defines (`Expr1004`, `$0`).
    pub expression_names: ExpressionNames,
    index: HashMap<i32, usize>,
}

impl PlanAnalysis {
    pub fn node(&self, node_id: i32) -> Option<&NodeAnalysis> {
        self.index.get(&node_id).map(|index| &self.nodes[*index])
    }

    /// Position of `node_id` in `nodes` (and in `ExecutionPlan::nodes`).
    pub fn position(&self, node_id: i32) -> Option<usize> {
        self.index.get(&node_id).copied()
    }

    /// The metric value a renderer ranks by for `metric` (own time, own
    /// cost, rows out, estimate factor, own pages).
    pub fn value(&self, node_id: i32, metric: PlanMetric) -> Option<f64> {
        let node = self.node(node_id)?;
        match metric {
            PlanMetric::Time => node.metrics.self_time_ms,
            PlanMetric::Cost => node.metrics.self_cost,
            PlanMetric::Rows => node.metrics.rows_out.or(node.metrics.estimated_rows_total),
            PlanMetric::EstimateError => node.metrics.estimate_factor,
            PlanMetric::Io => node.metrics.io_pages_self,
        }
    }
}

/// Analyse `plan` for the engine it says it came from (or the engine its
/// operator names point to when the parser did not record one).
pub fn analyze_plan(plan: &ExecutionPlan) -> PlanAnalysis {
    analyze_plan_for(plan, plan_engine(plan))
}

/// Analyse `plan` as a plan of `dialect`. Use when the caller knows the
/// engine and the plan does not record it. A plan that records an engine
/// without a dialect of its own (CockroachDB, Trino, Spark) keeps that
/// engine when `dialect` is the one it is analysed under.
pub fn analyze_plan_as(plan: &ExecutionPlan, dialect: PlanDialect) -> PlanAnalysis {
    let recorded = plan_engine(plan);
    let engine = if recorded.analysis_dialect() == dialect && plan.dialect.is_none() {
        recorded
    } else {
        PlanEngine::from_dialect(dialect)
    };
    Builder::new(plan, dialect, engine).build()
}

/// Analyse `plan` as a plan of `engine`, whatever the plan records.
pub fn analyze_plan_for(plan: &ExecutionPlan, engine: PlanEngine) -> PlanAnalysis {
    Builder::new(plan, engine.analysis_dialect(), engine).build()
}

/// Engine of a plan: the one its source format names, else its recorded
/// dialect, else the engine whose glossary knows the most of its operators.
/// Unlike [`resolve_dialect`] this tells CockroachDB, Trino and Spark plans
/// apart from PostgreSQL ones.
pub fn plan_engine(plan: &ExecutionPlan) -> PlanEngine {
    if let Some(format) = plan.source_format.as_deref() {
        if let Some(format) = PlanFormat::from_id(format) {
            return format.engine();
        }
        let lowered = format.trim().to_ascii_lowercase();
        if let Some(engine) = PlanEngine::ALL
            .into_iter()
            .find(|engine| lowered.starts_with(engine.id()))
        {
            return engine;
        }
    }
    if let Some(dialect) = plan.dialect {
        return PlanEngine::from_dialect(dialect);
    }
    let mut best = (0usize, PlanEngine::from_dialect(resolve_dialect(plan)));
    let known = |engine: PlanEngine| {
        plan.nodes
            .iter()
            .take(64)
            .filter(|node| {
                crate::plans::glossary::operator_lookup_for(
                    engine,
                    &node.physical_op,
                    &node.logical_op,
                )
                .is_some()
            })
            .count()
    };
    best.0 = known(best.1);
    for engine in [
        PlanEngine::CockroachDb,
        PlanEngine::Trino,
        PlanEngine::Spark,
    ] {
        let hits = known(engine);
        if hits > best.0 {
            best = (hits, engine);
        }
    }
    best.1
}

/// Dialect a plan is analysed under: the recorded dialect, else the source
/// format, else the engine whose glossary knows the most of its operators.
/// CockroachDB, Trino and Spark plans have no dialect of their own and come
/// back as PostgreSQL; [`plan_engine`] names the engine itself.
pub fn resolve_dialect(plan: &ExecutionPlan) -> PlanDialect {
    if let Some(dialect) = plan.dialect {
        return dialect;
    }
    if let Some(format) = plan.source_format.as_deref() {
        let known = [
            ("sqlserver", PlanDialect::SqlServer),
            ("postgresql", PlanDialect::Postgres),
            ("cockroachdb", PlanDialect::Postgres),
            ("trino", PlanDialect::Postgres),
            ("spark", PlanDialect::Postgres),
            ("mysql", PlanDialect::MySql),
            ("mariadb", PlanDialect::MariaDb),
            ("oracle", PlanDialect::Oracle),
            ("sqlite", PlanDialect::Sqlite),
            ("duckdb", PlanDialect::DuckDb),
            ("clickhouse", PlanDialect::ClickHouse),
        ];
        if let Some((_, dialect)) = known.iter().find(|(prefix, _)| format.starts_with(prefix)) {
            return *dialect;
        }
    }
    let candidates = [
        PlanDialect::SqlServer,
        PlanDialect::Postgres,
        PlanDialect::MySql,
        PlanDialect::Oracle,
        PlanDialect::Sqlite,
        PlanDialect::DuckDb,
        PlanDialect::ClickHouse,
    ];
    let mut best = (0usize, PlanDialect::SqlServer);
    for dialect in candidates {
        let hits = plan
            .nodes
            .iter()
            .take(64)
            .filter(|node| operator_lookup(dialect, &node.physical_op, &node.logical_op).is_some())
            .count();
        if hits > best.0 {
            best = (hits, dialect);
        }
    }
    best.1
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Estimate errors below this factor read as "exact".
const EXACT_FACTOR: f64 = 1.05;

struct Builder<'a> {
    plan: &'a ExecutionPlan,
    dialect: PlanDialect,
    engine: PlanEngine,
    n: usize,
    index: HashMap<i32, usize>,
    parent: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    roots: Vec<usize>,
    /// Pre-order positions; reversed it visits children before parents.
    pre_order: Vec<usize>,
    pre_position: Vec<usize>,
    depth: Vec<usize>,
    subtree_size: Vec<usize>,
}

impl<'a> Builder<'a> {
    fn new(plan: &'a ExecutionPlan, dialect: PlanDialect, engine: PlanEngine) -> Self {
        let n = plan.nodes.len();
        let mut index = HashMap::with_capacity(n);
        for (position, node) in plan.nodes.iter().enumerate() {
            index.entry(node.id).or_insert(position);
        }
        // A plan is a forest. An edge is kept only when it gives a node its
        // first parent and does not close a loop, so a malformed document
        // still analyses.
        let mut parent: Vec<Option<usize>> = vec![None; n];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut set: Vec<usize> = (0..n).collect();
        fn find(set: &mut [usize], mut item: usize) -> usize {
            while set[item] != item {
                set[item] = set[set[item]];
                item = set[item];
            }
            item
        }
        for edge in &plan.edges {
            let (Some(&from), Some(&to)) = (index.get(&edge.from_node), index.get(&edge.to_node))
            else {
                continue;
            };
            if parent[to].is_some() {
                continue;
            }
            let (a, b) = (find(&mut set, from), find(&mut set, to));
            if a == b {
                continue;
            }
            set[b] = a;
            parent[to] = Some(from);
            children[from].push(to);
        }
        let roots: Vec<usize> = (0..n).filter(|node| parent[*node].is_none()).collect();
        let mut pre_order = Vec::with_capacity(n);
        let mut depth = vec![0usize; n];
        let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
        while let Some(node) = stack.pop() {
            pre_order.push(node);
            for child in children[node].iter().rev() {
                depth[*child] = depth[node] + 1;
                stack.push(*child);
            }
        }
        let mut pre_position = vec![0usize; n];
        for (position, node) in pre_order.iter().enumerate() {
            pre_position[*node] = position;
        }
        let mut subtree_size = vec![1usize; n];
        for node in pre_order.iter().rev() {
            if let Some(up) = parent[*node] {
                subtree_size[up] += subtree_size[*node];
            }
        }
        Self {
            plan,
            dialect,
            engine,
            n,
            index,
            parent,
            children,
            roots,
            pre_order,
            pre_position,
            depth,
            subtree_size,
        }
    }

    /// True when `node` is `ancestor` or below it.
    fn within(&self, node: usize, ancestor: usize) -> bool {
        let start = self.pre_position[ancestor];
        let position = self.pre_position[node];
        position >= start && position < start + self.subtree_size[ancestor]
    }

    fn build(self) -> PlanAnalysis {
        let plan = self.plan;
        let dialect = self.dialect;
        let engine = self.engine;
        // CockroachDB, Trino and Spark share PostgreSQL's row and cost
        // arithmetic but none of its executor's quirks (per-process loops,
        // InitPlans charged to their reader).
        let postgres = engine == PlanEngine::Postgres;
        let n = self.n;
        let nodes = &plan.nodes;

        // ---- Operators and facts -----------------------------------------
        let matches: Vec<(OperatorInfo, Option<&'static OperatorEntry>)> = nodes
            .iter()
            .map(|node| operator_match_for(engine, node))
            .collect();
        let expression_names = expression_names(plan);
        let facts: Vec<Facts> = nodes
            .iter()
            .zip(&matches)
            .map(|(node, (info, entry))| Facts::read(dialect, node, info, *entry))
            .collect();

        // ---- Rows ---------------------------------------------------------
        let has_actuals = plan.has_actuals
            || nodes.iter().any(|node| {
                node.actual_rows.is_some()
                    || node.actual_rows_total.is_some()
                    || node.actual_time_ms.is_some()
                    || node.never_executed
            });
        let has_estimates = nodes.iter().any(|node| node.estimated_rows > 0.0);
        let mut rows_out: Vec<Option<f64>> =
            nodes.iter().map(|node| rows_out(dialect, node)).collect();
        // MySQL and MariaDB JSON fold a table's attached condition into the
        // table node: the row count is what the scan examined, and the rows
        // the condition removed never leave the node.
        let mut rows_examined: Vec<Option<f64>> = vec![None; n];
        if matches!(dialect, PlanDialect::MySql | PlanDialect::MariaDb) {
            for (node, raw) in nodes.iter().enumerate() {
                if let (Some(out), Some(read), Some(removed)) =
                    (rows_out[node], raw.rows_read, raw.rows_removed_by_filter)
                    && removed > 0.0
                    && removed <= out
                    && (read - out).abs() < 0.5
                {
                    rows_examined[node] = Some(out);
                    rows_out[node] = Some(out - removed);
                }
            }
        }

        // ---- Time ---------------------------------------------------------
        // Engines whose parsers fill `actual_self_time_ms` with the
        // operator's own time: DuckDB prints it, and the CockroachDB and
        // Trino parsers read it from per-operator execution times.
        let duckdb = dialect == PlanDialect::DuckDb
            || matches!(engine, PlanEngine::CockroachDb | PlanEngine::Trino);
        let is_batch = |node: &PlanNode| -> bool {
            node.execution_mode
                .as_deref()
                .is_some_and(|mode| mode.eq_ignore_ascii_case("batch"))
        };
        // SQL Server row-mode operators report `ActualElapsedms` for
        // themselves and everything below them; batch-mode operators report
        // their own time only. The mode is per operator, so one plan can
        // hold both: a row-mode exchange above batch-mode operators includes
        // them, a batch-mode operator above a row-mode input does not
        // include it. The showplan parser keeps a batch-mode operator's own
        // time in `actual_self_time_ms` and makes `actual_time_ms` inclusive;
        // a plan built without the parser carries the engine's number, which
        // for a batch-mode operator is its own time.
        let batch_own = |node: &PlanNode| -> Option<f64> {
            (dialect == PlanDialect::SqlServer && is_batch(node))
                .then(|| node.actual_self_time_ms.or(node.actual_time_ms))
                .flatten()
        };
        let reported_own = |node: &PlanNode| -> Option<f64> {
            if duckdb {
                node.actual_self_time_ms
            } else {
                batch_own(node)
            }
        };
        let has_timing = nodes.iter().any(|node| {
            node.actual_time_ms.is_some() || (duckdb && node.actual_self_time_ms.is_some())
        });
        let mut inclusive_time: Vec<Option<f64>> = vec![None; n];
        let mut self_time: Vec<Option<f64>> = vec![None; n];
        // Time a node spent pulling a CTE or InitPlan that is attached
        // elsewhere in the tree.
        let mut borrowed_time = vec![0.0f64; n];
        let mut borrowed_io = vec![0.0f64; n];
        // Children whose time is NOT inside their parent's reported time
        // unless a reader below the parent pulled it.
        let mut detached = vec![false; n];
        let mut time_reported = vec![false; n];
        let mut own_only = vec![false; n];
        if has_timing {
            for &node in self.pre_order.iter().rev() {
                let raw = &nodes[node];
                if raw.never_executed {
                    inclusive_time[node] = Some(0.0);
                    continue;
                }
                let below: f64 = self.children[node]
                    .iter()
                    .filter_map(|child| inclusive_time[*child])
                    .sum();
                let any_below = self.children[node]
                    .iter()
                    .any(|child| inclusive_time[*child].is_some());
                inclusive_time[node] = if let Some(own) = reported_own(raw) {
                    Some(own.max(0.0) + below)
                } else if let Some(time) = raw.actual_time_ms {
                    time_reported[node] = true;
                    Some(time.max(0.0))
                } else if let Some(own) = raw.actual_self_time_ms {
                    // Only its own time is known (a MariaDB filesort).
                    own_only[node] = true;
                    Some(own.max(0.0) + below)
                } else if any_below {
                    // An operator the engine prints without a time passes
                    // the time of its inputs up, so the operator above it
                    // is not charged for them.
                    Some(below)
                } else {
                    None
                };
            }
        }

        // ---- I/O ----------------------------------------------------------
        let io_inclusive_engine = matches!(dialect, PlanDialect::Postgres | PlanDialect::Oracle);
        let reported_pages: Vec<Option<f64>> = nodes.iter().map(node_pages).collect();
        let has_io = reported_pages.iter().any(Option::is_some);
        let mut inclusive_io: Vec<Option<f64>> = vec![None; n];
        if has_io {
            for &node in self.pre_order.iter().rev() {
                let below: f64 = self.children[node]
                    .iter()
                    .filter_map(|child| inclusive_io[*child])
                    .sum();
                inclusive_io[node] = if io_inclusive_engine {
                    reported_pages[node]
                } else {
                    match reported_pages[node] {
                        Some(pages) => Some(pages + below),
                        None if below > 0.0 => Some(below),
                        None => None,
                    }
                };
            }
        }

        // ---- PostgreSQL CTEs and InitPlans --------------------------------
        if postgres && (has_timing || has_io) {
            self.attribute_init_plans(
                &facts,
                &inclusive_time,
                &inclusive_io,
                &mut detached,
                &mut borrowed_time,
                &mut borrowed_io,
            );
        }

        if has_timing {
            for node in 0..n {
                let Some(inclusive) = inclusive_time[node] else {
                    continue;
                };
                let raw = &nodes[node];
                let own = if raw.never_executed {
                    0.0
                } else if let Some(own) = reported_own(raw) {
                    own
                } else if own_only[node] {
                    raw.actual_self_time_ms.unwrap_or(0.0)
                } else if !time_reported[node] {
                    // Untimed pass-through: its own time is unknown.
                    continue;
                } else {
                    let below: f64 = self.children[node]
                        .iter()
                        .filter(|child| !detached[**child])
                        .filter_map(|child| inclusive_time[*child])
                        .sum();
                    inclusive - below - borrowed_time[node]
                };
                self_time[node] = Some(own.max(0.0));
            }
        }
        let total_time: Option<f64> = has_timing
            .then(|| self_time.iter().flatten().sum::<f64>())
            .filter(|total| *total > 0.0);
        // Some formats print operator times in steps too large for a short
        // statement. When the steps are as large as the differences between
        // operators, the split between them is noise: keep the statement's
        // elapsed time and rank the operators by cost or rows instead.
        let time_resolution = time_resolution_ms(plan);
        let timing_too_coarse = match (time_resolution, total_time) {
            (Some(resolution), Some(total)) => {
                let timed = self_time.iter().flatten().count() as f64;
                let largest = self_time.iter().flatten().copied().fold(0.0f64, f64::max);
                // An own time is off by up to half a step, and the total by
                // that much for every operator.
                largest < resolution * COARSE_MIN_STEPS
                    || (duckdb && timed * resolution / 2.0 > total * COARSE_TOTAL_SLACK)
            }
            _ => false,
        };
        let total_time = if timing_too_coarse {
            inclusive_time.fill(None);
            self_time.fill(None);
            None
        } else {
            total_time
        };

        let mut self_io: Vec<Option<f64>> = vec![None; n];
        if has_io {
            for node in 0..n {
                let Some(inclusive) = inclusive_io[node] else {
                    continue;
                };
                let below: f64 = self.children[node]
                    .iter()
                    .filter(|child| !detached[**child] || !io_inclusive_engine)
                    .filter_map(|child| inclusive_io[*child])
                    .sum();
                let borrowed = if io_inclusive_engine {
                    borrowed_io[node]
                } else {
                    0.0
                };
                self_io[node] = Some((inclusive - below - borrowed).max(0.0));
            }
        }
        let total_io: Option<f64> = has_io
            .then(|| self_io.iter().flatten().sum::<f64>())
            .filter(|total| *total > 0.0);

        // ---- Cost ---------------------------------------------------------
        let has_costs = !matches!(
            dialect,
            PlanDialect::Sqlite | PlanDialect::ClickHouse | PlanDialect::DuckDb
        ) && nodes.iter().any(|node| node.estimated_cost > 0.0);
        let mut inclusive_cost: Vec<Option<f64>> = vec![None; n];
        let mut self_cost: Vec<Option<f64>> = vec![None; n];
        if has_costs {
            self.costs(&matches, &mut inclusive_cost, &mut self_cost);
        }
        let total_cost: Option<f64> = has_costs
            .then(|| self_cost.iter().flatten().sum::<f64>())
            .filter(|total| *total > 0.0);

        // ---- Parallel zones -----------------------------------------------
        let mut parallel = vec![false; n];
        for &node in &self.pre_order {
            let raw = &nodes[node];
            let starts = raw.parallel
                || raw.workers_planned.is_some()
                || matches[node].0.category == OperatorCategory::Parallelism;
            let inherited = self.parent[node].is_some_and(|up| parallel[up]);
            parallel[node] = match dialect {
                // SQL Server flags every parallel operator itself; a serial
                // operator below an exchange is outside the zone.
                PlanDialect::SqlServer => starts,
                // Elsewhere everything below a gather runs on the workers,
                // except subplans the leader evaluates on its own.
                _ => starts || (inherited && !detached[node]),
            };
        }

        // PostgreSQL counts each process of a parallel zone as a loop. Below
        // a Gather, a loop count no larger than the processes that ran is
        // that, not a repeated execution.
        let mut processes: Vec<Option<f64>> = vec![None; n];
        if postgres {
            let mut zone: Vec<Option<f64>> = vec![None; n];
            for &node in &self.pre_order {
                let raw = &nodes[node];
                let inherited = self.parent[node].and_then(|up| zone[up]);
                if let Some(limit) = inherited.filter(|_| !detached[node])
                    && let Some(loops) = raw.loops
                    && loops > 1.0
                    && loops <= limit
                {
                    processes[node] = Some(loops);
                }
                zone[node] = match raw.workers_launched.or(raw.workers_planned) {
                    Some(workers) if matches[node].0.category == OperatorCategory::Parallelism => {
                        Some(f64::from(workers) + 1.0)
                    }
                    _ => inherited.filter(|_| !detached[node]),
                };
            }
        }

        // ---- Assemble node metrics ----------------------------------------
        let max_rows = (0..n)
            .map(|node| {
                rows_out[node]
                    .or_else(|| estimate_total(dialect, &nodes[node]))
                    .unwrap_or(0.0)
            })
            .fold(0.0f64, f64::max);
        let mut analysed: Vec<NodeAnalysis> = Vec::with_capacity(n);
        for node in 0..n {
            let raw = &nodes[node];
            let fact = &facts[node];
            let estimated_rows = (has_estimates && raw.estimated_rows >= 0.0)
                .then(|| estimate_per_execution(dialect, raw))
                .filter(|rows| rows.is_finite());
            let estimated_rows_total = estimated_rows.and_then(|_| estimate_total(dialect, raw));
            let loops = executions_run(dialect, raw);
            let rows_per_loop = rows_out[node].map(|rows| match loops {
                Some(loops) if loops > 0.0 => rows / loops,
                _ => match raw.estimated_executions {
                    Some(executions) if executions > 1.0 && loops.is_none() => rows / executions,
                    _ => rows,
                },
            });
            // The estimate of a folded scan (see `rows_examined`) is for the
            // rows examined, so that is what it is compared with.
            let compared_rows = match rows_examined[node] {
                Some(examined) => Some(match loops {
                    Some(loops) if loops > 0.0 => examined / loops,
                    _ => examined,
                }),
                None => rows_per_loop,
            };
            let (estimate_factor, estimate_direction) = match (estimated_rows, compared_rows) {
                // An estimate of zero is how several formats print "unknown".
                (Some(estimated), Some(actual)) if !raw.never_executed && estimated > 0.0 => {
                    let (estimated, actual) = (estimated.max(1.0), actual.max(1.0));
                    let factor = (estimated / actual).max(actual / estimated);
                    let direction = if factor < EXACT_FACTOR {
                        EstimateDirection::Exact
                    } else if actual > estimated {
                        EstimateDirection::Under
                    } else {
                        EstimateDirection::Over
                    };
                    (Some(factor), Some(direction))
                }
                _ => (None, None),
            };
            let from_inputs: Option<f64> = {
                let mut any = false;
                let mut sum = 0.0;
                for child in &self.children[node] {
                    if let Some(rows) = rows_out[*child] {
                        any = true;
                        sum += rows;
                    }
                }
                any.then_some(sum)
            };
            let explicit_removed: Option<f64> = {
                let parts = [
                    raw.rows_removed_by_filter,
                    raw.rows_removed_by_join_filter,
                    raw.rows_removed_by_index_recheck,
                ];
                parts
                    .iter()
                    .any(Option::is_some)
                    .then(|| parts.iter().flatten().sum::<f64>())
            };
            let is_leaf = self.children[node].is_empty();
            let rows_read = raw.rows_read.or_else(|| {
                (is_leaf && explicit_removed.is_some())
                    .then(|| rows_out[node].map(|rows| rows + explicit_removed.unwrap_or(0.0)))
                    .flatten()
            });
            let rows_in = if is_leaf {
                rows_read.or(rows_out[node])
            } else {
                from_inputs
            };
            let rows_removed = explicit_removed
                .or_else(|| match (raw.rows_read, rows_out[node]) {
                    (Some(read), Some(out)) if read > out => Some(read - out),
                    _ => None,
                })
                .filter(|removed| *removed > 0.0);
            let rows_removed_share = rows_removed.and_then(|removed| {
                let read = rows_read
                    .or(if is_leaf { None } else { from_inputs })
                    .unwrap_or(0.0)
                    .max(removed + rows_out[node].unwrap_or(0.0));
                (read > 0.0).then(|| (removed / read).clamp(0.0, 1.0))
            });
            let memory_kb = raw.peak_memory_kb.or_else(|| {
                raw.sort_space_type
                    .as_deref()
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("memory"))
                    .then_some(raw.sort_space_kb)
                    .flatten()
            });
            let sort_on_disk = raw
                .sort_space_type
                .as_deref()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("disk"));
            let spilled = raw.spill.is_some()
                || sort_on_disk
                || raw.hash_batches.is_some_and(|batches| batches > 1)
                || raw
                    .warnings
                    .iter()
                    .any(|warning| matches!(warning, PlanWarning::SpillToTempDb));
            let spilled_kb = raw
                .spill
                .as_ref()
                .and_then(|spill| spill.written_kb)
                .or_else(|| sort_on_disk.then_some(raw.sort_space_kb).flatten());
            let share = |own: Option<f64>, total: Option<f64>| -> Option<f64> {
                match (own, total) {
                    (Some(own), Some(total)) if total > 0.0 => Some((own / total).clamp(0.0, 1.0)),
                    _ => None,
                }
            };
            let time_share = share(self_time[node], total_time);
            let cost_share = share(self_cost[node], total_cost);
            let io_share = share(self_io[node], total_io);
            let shown_rows = rows_out[node].or(estimated_rows_total).unwrap_or(0.0);
            let heat = NodeHeat {
                time: time_share.unwrap_or(0.0) as f32,
                cost: cost_share.unwrap_or(0.0) as f32,
                rows: if max_rows > 0.0 {
                    ((1.0 + shown_rows.max(0.0)).ln() / (1.0 + max_rows).ln()).clamp(0.0, 1.0)
                        as f32
                } else {
                    0.0
                },
                estimate: estimate_factor
                    .map(|factor| (factor.log10() / 3.0).clamp(0.0, 1.0) as f32)
                    .unwrap_or(0.0),
                io: io_share.unwrap_or(0.0) as f32,
            };
            let metrics = NodeMetrics {
                inclusive_time_ms: inclusive_time[node],
                self_time_ms: self_time[node],
                time_share,
                inclusive_time_share: share(inclusive_time[node], total_time),
                inclusive_cost: inclusive_cost[node],
                self_cost: self_cost[node],
                cost_share,
                inclusive_cost_share: share(inclusive_cost[node], total_cost),
                loops,
                rows_out: rows_out[node],
                rows_per_loop,
                estimate_compared_rows: compared_rows.filter(|_| estimate_factor.is_some()),
                processes: processes[node],
                rows_in,
                estimated_rows,
                estimated_rows_total,
                estimate_factor,
                estimate_direction,
                rows_removed,
                rows_removed_share,
                io_pages_inclusive: inclusive_io[node],
                io_pages_self: self_io[node],
                io_share,
                memory_kb,
                spilled_kb,
                heat,
            };
            let names = &expression_names;
            let readable = |text: &Option<String>| -> Option<ReadablePredicate> {
                text.as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(|text| {
                        readable_predicate(Some(dialect), &engine_text(engine, text), names)
                    })
            };
            analysed.push(NodeAnalysis {
                node_id: raw.id,
                parent_id: self.parent[node].map(|up| nodes[up].id),
                children: self.children[node]
                    .iter()
                    .map(|child| nodes[*child].id)
                    .collect(),
                depth: self.depth[node],
                subtree_size: self.subtree_size[node],
                operator: matches[node].0.clone(),
                object: fact.object.clone(),
                index: fact.index.clone(),
                seek_predicate: readable(&fact.seek),
                filter: readable(&fact.filter),
                join_predicate: readable(&fact.join),
                join_type: fact.join_type.clone(),
                sort_keys: fact
                    .sort_keys
                    .iter()
                    .map(|key| {
                        readable_expression(Some(dialect), &engine_sort_key(engine, key), names)
                    })
                    .filter(|key| !key.is_empty())
                    .collect(),
                // DuckDB prints grouping keys it cannot name as positions
                // (`#0`); those say nothing to a reader.
                group_keys: fact
                    .group_keys
                    .iter()
                    .map(|key| readable_expression(Some(dialect), &engine_text(engine, key), names))
                    .filter(|key| !key.is_empty() && !is_positional(key))
                    .collect(),
                engine_line: (dialect == PlanDialect::Sqlite)
                    .then(|| raw.physical_op.trim().to_string())
                    .filter(|line| !line.is_empty()),
                subplan_name: raw
                    .subplan_name
                    .clone()
                    .filter(|name| !name.trim().is_empty()),
                metrics,
                flags: NodeFlags {
                    bottleneck: false,
                    never_executed: raw.never_executed,
                    spilled,
                    parallel: parallel[node],
                    on_critical_path: false,
                    misestimate_origin: false,
                    filters_window_result: false,
                },
            });
        }

        // ---- Filters on a window function's result --------------------------
        for node in 0..n {
            if analysed[node].filter.is_none()
                || !matches!(
                    analysed[node].operator.category,
                    OperatorCategory::Subquery
                        | OperatorCategory::Filter
                        | OperatorCategory::Compute
                )
            {
                continue;
            }
            // Look through steps that only pass rows along.
            let mut at = node;
            for _ in 0..4 {
                let [child] = self.children[at][..] else {
                    break;
                };
                match analysed[child].operator.category {
                    OperatorCategory::Window => {
                        analysed[node].flags.filters_window_result = true;
                        break;
                    }
                    OperatorCategory::Subquery | OperatorCategory::Compute
                        if analysed[child].filter.is_none() =>
                    {
                        at = child;
                    }
                    _ => break,
                }
            }
        }

        // ---- Where estimate misses start ----------------------------------
        for node in 0..n {
            let (Some(factor), Some(direction)) = (
                analysed[node].metrics.estimate_factor,
                analysed[node].metrics.estimate_direction,
            ) else {
                continue;
            };
            if factor < 2.0
                || direction == EstimateDirection::Exact
                || analysed[node].flags.filters_window_result
            {
                continue;
            }
            // Inputs without an estimate of their own are looked through.
            let mut inherited = false;
            let mut pending: Vec<usize> = self.children[node].clone();
            let mut looked_at = 0usize;
            while let Some(child) = pending.pop() {
                looked_at += 1;
                if looked_at > 16 {
                    break;
                }
                let below = &analysed[child].metrics;
                match below.estimate_factor {
                    Some(other) => {
                        if below.estimate_direction == Some(direction) && other * 2.0 >= factor {
                            inherited = true;
                            break;
                        }
                    }
                    None => pending.extend(self.children[child].iter().copied()),
                }
            }
            analysed[node].flags.misestimate_origin = !inherited;
        }

        // ---- Bottleneck and critical path ---------------------------------
        let primary_metric = if total_time.is_some() {
            PlanMetric::Time
        } else if total_cost.is_some() {
            PlanMetric::Cost
        } else {
            PlanMetric::Rows
        };
        let own = |node: usize| -> f64 {
            match primary_metric {
                PlanMetric::Time => self_time[node].unwrap_or(0.0),
                PlanMetric::Cost => self_cost[node].unwrap_or(0.0),
                _ => 0.0,
            }
        };
        let mut bottleneck: Option<usize> = None;
        if primary_metric != PlanMetric::Rows {
            for node in 0..n {
                if own(node) > 0.0 && bottleneck.is_none_or(|best| own(node) > own(best)) {
                    bottleneck = Some(node);
                }
            }
        }
        if let Some(node) = bottleneck {
            analysed[node].flags.bottleneck = true;
        }
        let weight = |node: usize| -> f64 {
            match primary_metric {
                PlanMetric::Time => inclusive_time[node].unwrap_or(0.0),
                PlanMetric::Cost => inclusive_cost[node].unwrap_or(0.0),
                _ => rows_out[node]
                    .or_else(|| estimate_total(dialect, &nodes[node]))
                    .unwrap_or(0.0),
            }
        };
        let mut critical_path = Vec::new();
        let mut cursor = self
            .roots
            .iter()
            .copied()
            .fold(None, |best: Option<usize>, root| match best {
                Some(best) if weight(best) >= weight(root) => Some(best),
                _ => Some(root),
            });
        while let Some(node) = cursor {
            analysed[node].flags.on_critical_path = true;
            critical_path.push(nodes[node].id);
            cursor =
                self.children[node]
                    .iter()
                    .copied()
                    .fold(None, |best: Option<usize>, child| match best {
                        Some(best) if weight(best) >= weight(child) => Some(best),
                        _ => Some(child),
                    });
        }

        // ---- Execution order ----------------------------------------------
        let build_second = matches!(
            dialect,
            PlanDialect::Postgres
                | PlanDialect::MySql
                | PlanDialect::MariaDb
                | PlanDialect::DuckDb
                | PlanDialect::ClickHouse
        );
        let mut execution_order = Vec::with_capacity(n);
        let mut stack: Vec<(usize, bool)> =
            self.roots.iter().rev().map(|root| (*root, false)).collect();
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                execution_order.push(nodes[node].id);
                continue;
            }
            stack.push((node, true));
            let kids = &self.children[node];
            let swap = build_second
                && kids.len() == 2
                && matches[node].0.category == OperatorCategory::JoinHash;
            if swap {
                stack.push((kids[0], false));
                stack.push((kids[1], false));
            } else {
                for child in kids.iter().rev() {
                    stack.push((*child, false));
                }
            }
        }

        // ---- Groups -------------------------------------------------------
        let mut groups = Vec::new();
        for &node in &self.pre_order {
            let raw = &nodes[node];
            let Some(name) = raw
                .subplan_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            let kind = if is_cte_definition(raw) {
                PlanGroupKind::Cte
            } else {
                PlanGroupKind::Subplan
            };
            groups.push(PlanGroup {
                kind,
                label: DisplayMessage::raw(name.to_string()),
                root_id: raw.id,
                node_ids: self.subtree_ids(node),
            });
        }
        for &node in &self.pre_order {
            if !parallel[node] || self.parent[node].is_some_and(|up| parallel[up]) {
                continue;
            }
            // Top of a parallel zone: collect the connected parallel nodes.
            let mut ids = Vec::new();
            let mut stack = vec![node];
            while let Some(member) = stack.pop() {
                ids.push(nodes[member].id);
                for child in self.children[member].iter().rev() {
                    if parallel[*child] {
                        stack.push(*child);
                    }
                }
            }
            if ids.len() < 2 {
                continue;
            }
            let raw = &nodes[node];
            let label = match (raw.workers_launched, raw.workers_planned) {
                (Some(launched), Some(planned)) => say(
                    "group.parallel_workers",
                    vec![
                        ("launched", launched.to_string().into()),
                        ("planned", planned.to_string().into()),
                    ],
                ),
                (None, Some(planned)) => say(
                    "group.parallel_planned",
                    vec![("planned", planned.to_string().into())],
                ),
                _ => say("group.parallel", Vec::new()),
            };
            groups.push(PlanGroup {
                kind: PlanGroupKind::Parallel,
                label,
                root_id: raw.id,
                node_ids: ids,
            });
        }

        let result_rows = self
            .roots
            .first()
            .and_then(|root| {
                // A wrapper root (`Query Block`, `QUERY`) carries no rows of
                // its own: use the first node below it that does.
                // (Only one level: further down the count is no longer the
                // result.)
                rows_out[*root].or_else(|| match self.children[*root].as_slice() {
                    [only] => rows_out[*only],
                    _ => None,
                })
            })
            .or_else(|| {
                (has_estimates && !has_actuals)
                    .then(|| {
                        let root = *self.roots.first()?;
                        let estimate = |node: usize| {
                            Some(nodes[node].estimated_rows).filter(|rows| *rows > 0.0)
                        };
                        estimate(root).or_else(|| match self.children[root].as_slice() {
                            [only] => estimate(*only),
                            _ => None,
                        })
                    })
                    .flatten()
            });

        PlanAnalysis {
            dialect,
            engine,
            has_actuals,
            has_timing: total_time.is_some(),
            time_resolution_ms: time_resolution,
            timing_too_coarse,
            has_costs: total_cost.is_some(),
            has_estimates,
            has_io: total_io.is_some(),
            total_time_ms: total_time,
            execution_time_ms: plan.execution_time_ms,
            planning_time_ms: plan.planning_time_ms,
            total_cost,
            total_io_pages: total_io,
            result_rows,
            max_rows,
            primary_metric,
            nodes: analysed,
            roots: self.roots.iter().map(|root| nodes[*root].id).collect(),
            execution_order,
            bottleneck: bottleneck.map(|node| nodes[node].id),
            critical_path,
            groups,
            expression_names,
            index: self.index,
        }
    }

    fn subtree_ids(&self, node: usize) -> Vec<i32> {
        let start = self.pre_position[node];
        self.pre_order[start..start + self.subtree_size[node]]
            .iter()
            .map(|member| self.plan.nodes[*member].id)
            .collect()
    }

    /// PostgreSQL: decide which node really paid for each CTE and InitPlan.
    fn attribute_init_plans(
        &self,
        facts: &[Facts],
        inclusive_time: &[Option<f64>],
        inclusive_io: &[Option<f64>],
        detached: &mut [bool],
        borrowed_time: &mut [f64],
        borrowed_io: &mut [f64],
    ) {
        let nodes = &self.plan.nodes;
        let init_plans: Vec<usize> = self
            .pre_order
            .iter()
            .copied()
            .filter(|node| self.parent[*node].is_some() && is_init_plan(&nodes[*node]))
            .collect();
        if init_plans.is_empty() {
            return;
        }
        // CTE name -> scans of it, in plan order.
        let mut cte_scans: HashMap<String, Vec<usize>> = HashMap::new();
        for &node in &self.pre_order {
            let raw = &nodes[node];
            if is_init_plan(raw) {
                continue;
            }
            if let Some(name) = raw.cte_name.as_deref() {
                cte_scans
                    .entry(name.to_ascii_lowercase())
                    .or_default()
                    .push(node);
            }
        }
        // Parameter / InitPlan reference -> first node that mentions it.
        let mut tokens: HashMap<String, usize> = HashMap::new();
        let mut wanted: Vec<(usize, Vec<String>)> = Vec::new();
        for &plan_node in &init_plans {
            let raw = &nodes[plan_node];
            if is_cte_definition(raw) {
                continue;
            }
            wanted.push((
                plan_node,
                init_plan_tokens(raw.subplan_name.as_deref().unwrap_or("")),
            ));
        }
        if !wanted.is_empty() {
            for &node in &self.pre_order {
                let fact = &facts[node];
                let raw = &nodes[node];
                let texts = [&fact.seek, &fact.filter, &fact.join]
                    .into_iter()
                    .flatten()
                    .map(String::as_str)
                    .chain(raw.output_columns.iter().map(String::as_str))
                    .chain(
                        raw.properties
                            .iter()
                            .map(|property| property.value.as_str()),
                    );
                for text in texts {
                    for token in reference_tokens(text) {
                        tokens.entry(token).or_insert(node);
                    }
                }
            }
        }
        for &plan_node in &init_plans {
            let raw = &nodes[plan_node];
            let time = inclusive_time[plan_node].unwrap_or(0.0);
            let io = inclusive_io[plan_node].unwrap_or(0.0);
            let owner = self.parent[plan_node].unwrap_or(plan_node);
            if is_cte_definition(raw) {
                let name = cte_definition_name(raw).to_ascii_lowercase();
                let scans: Vec<usize> = cte_scans
                    .get(&name)
                    .map(|scans| {
                        scans
                            .iter()
                            .copied()
                            .filter(|scan| !self.within(*scan, plan_node))
                            .collect()
                    })
                    .unwrap_or_default();
                if scans.is_empty() {
                    continue;
                }
                detached[plan_node] = true;
                // The first scan to pull the CTE pays for computing it; when
                // that scan is shorter than the CTE the next one takes the
                // rest.
                let (mut time_left, mut io_left) = (time, io);
                for scan in scans {
                    let take = time_left.min(inclusive_time[scan].unwrap_or(0.0));
                    borrowed_time[scan] += take;
                    time_left -= take;
                    let take = io_left.min(inclusive_io[scan].unwrap_or(0.0));
                    borrowed_io[scan] += take;
                    io_left -= take;
                }
                continue;
            }
            let reader =
                wanted
                    .iter()
                    .find(|(node, _)| *node == plan_node)
                    .and_then(|(_, names)| {
                        names
                            .iter()
                            .filter_map(|name| tokens.get(name))
                            .copied()
                            .find(|reader| {
                                !self.within(*reader, plan_node) && self.within(*reader, owner)
                            })
                    });
            match reader {
                Some(reader) if reader != owner => {
                    detached[plan_node] = true;
                    borrowed_time[reader] += time;
                    borrowed_io[reader] += io;
                }
                Some(_) => {}
                // Nothing in the plan says who read the result (without
                // VERBOSE a reference from an output list is not printed).
                // An InitPlan runs when its value is first needed, inside
                // the node that needs it, so its time is somewhere inside
                // the owner's. When the owner's other inputs leave no room
                // for it, one of them read it: charge the largest input that
                // can hold it, so the time is not counted twice.
                None => {
                    let Some(owner_time) = inclusive_time[owner] else {
                        continue;
                    };
                    let inputs: Vec<usize> = self.children[owner]
                        .iter()
                        .copied()
                        .filter(|child| *child != plan_node && !detached[*child])
                        .collect();
                    let others: f64 = inputs
                        .iter()
                        .filter_map(|child| inclusive_time[*child])
                        .sum();
                    if time <= 0.0 || owner_time - others >= time / 2.0 {
                        continue;
                    }
                    let holder = inputs
                        .iter()
                        .copied()
                        .filter(|child| !is_init_plan(&nodes[*child]))
                        .filter(|child| {
                            inclusive_time[*child]
                                .is_some_and(|inside| inside - borrowed_time[*child] >= time)
                        })
                        .max_by(|a, b| {
                            inclusive_time[*a]
                                .partial_cmp(&inclusive_time[*b])
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                    if let Some(holder) = holder {
                        detached[plan_node] = true;
                        borrowed_time[holder] += time;
                        borrowed_io[holder] += io;
                    }
                }
            }
        }
    }

    /// Effective inclusive and own cost for every node.
    fn costs(
        &self,
        matches: &[(OperatorInfo, Option<&'static OperatorEntry>)],
        inclusive: &mut [Option<f64>],
        own: &mut [Option<f64>],
    ) {
        let nodes = &self.plan.nodes;
        let per_execution_inner = matches!(
            self.dialect,
            PlanDialect::Postgres | PlanDialect::MySql | PlanDialect::MariaDb | PlanDialect::Oracle
        );
        let mysql = matches!(self.dialect, PlanDialect::MySql | PlanDialect::MariaDb);
        // Printed cost per node. An operator the engine prints without a
        // cost stands for the sum of its inputs, so the costs below it are
        // not charged to the operator above it.
        let own_printed = |node: usize| -> f64 {
            let cost = nodes[node].estimated_cost;
            if cost.is_finite() && cost > 0.0 {
                cost
            } else {
                0.0
            }
        };
        // MySQL JSON prints a running (prefix) cost on each table of a join:
        // every sibling includes the ones before it, and the last one equals
        // the cost of the query block above. MariaDB prints each table's own
        // cost. Tell the two apart once per plan.
        let running_costs = mysql && {
            let mut above = vec![0.0f64; self.n];
            let mut found = false;
            for &node in &self.pre_order {
                let own = own_printed(node);
                let budget = if own > 0.0 { own } else { above[node] };
                let kids = &self.children[node];
                for kid in kids {
                    above[*kid] = budget;
                }
                if kids.len() >= 2 && budget > 0.0 {
                    let costs: Vec<f64> = kids.iter().map(|kid| own_printed(*kid)).collect();
                    let rising = costs.windows(2).all(|pair| pair[1] >= pair[0]);
                    let last = costs.last().copied().unwrap_or(0.0);
                    if rising && costs[0] > 0.0 && (last - budget).abs() <= budget * 0.01 {
                        found = true;
                        break;
                    }
                }
            }
            found
        };
        let is_running = |costs: &[f64]| -> bool {
            running_costs
                && costs.len() >= 2
                && costs[0] > 0.0
                && costs.windows(2).all(|pair| pair[1] >= pair[0])
        };
        let mut printed_cost = vec![0.0f64; self.n];
        for &node in self.pre_order.iter().rev() {
            let own = own_printed(node);
            printed_cost[node] = if own > 0.0 {
                own
            } else {
                let costs: Vec<f64> = self.children[node]
                    .iter()
                    .map(|kid| printed_cost[*kid])
                    .collect();
                if is_running(&costs) {
                    costs.last().copied().unwrap_or(0.0)
                } else {
                    costs.iter().sum()
                }
            };
        }
        let raw_cost = |node: usize| -> f64 { printed_cost[node] };
        // Top-down: each node's effective inclusive cost is its printed cost
        // times a scale handed down by its parent.
        let mut effective = vec![0.0f64; self.n];
        for &root in &self.roots {
            effective[root] = raw_cost(root);
        }
        for &node in &self.pre_order {
            let kids = &self.children[node];
            if kids.is_empty() {
                own[node] = Some(effective[node]);
                inclusive[node] = Some(effective[node]);
                continue;
            }
            let printed = raw_cost(node);
            // A node with no cost of its own (a wrapper the engine prints
            // without numbers) passes its inputs through.
            let scale = if printed > 0.0 {
                effective[node] / printed
            } else {
                1.0
            };
            let mut parts: Vec<f64> = kids.iter().map(|kid| raw_cost(*kid) * scale).collect();
            if printed <= 0.0 && effective[node] <= 0.0 {
                effective[node] = parts.iter().sum();
            }
            let budget = effective[node];
            if is_running(&parts) {
                for position in (1..parts.len()).rev() {
                    parts[position] -= parts[position - 1];
                }
            } else if per_execution_inner
                && kids.len() == 2
                && matches[node].0.category == OperatorCategory::JoinNestedLoop
            {
                let outer_rows = nodes[kids[0]].estimated_rows.max(1.0);
                let room = (budget - parts[0]).max(0.0);
                parts[1] = (parts[1] * outer_rows).min(room);
            }
            let total: f64 = parts.iter().sum();
            if total > budget && total > 0.0 {
                let shrink = budget / total;
                for part in &mut parts {
                    *part *= shrink;
                }
            }
            for (kid, part) in kids.iter().zip(&parts) {
                effective[*kid] = *part;
            }
            let below: f64 = parts.iter().sum();
            inclusive[node] = Some(budget);
            own[node] = Some((budget - below).max(0.0));
        }
    }
}

/// Rows a node produced across all its executions.
fn rows_out(dialect: PlanDialect, node: &PlanNode) -> Option<f64> {
    if node.never_executed {
        return Some(0.0);
    }
    if let Some(total) = node.actual_rows_total {
        return Some(total.max(0.0));
    }
    let reported = node.actual_rows?;
    let per_loop = matches!(
        dialect,
        PlanDialect::Postgres | PlanDialect::MySql | PlanDialect::MariaDb
    );
    Some(match node.loops {
        Some(loops) if per_loop && loops > 0.0 => reported * loops,
        _ => reported,
    })
    .map(|rows| rows.max(0.0))
}

/// The largest own time has to be at least this many printing steps before
/// operator times are used.
const COARSE_MIN_STEPS: f64 = 5.0;
/// For formats that print own times, the rounding of all operators together
/// may be at most this share of the total.
const COARSE_TOTAL_SLACK: f64 = 0.25;

/// The step operator times are printed in, for the formats where it is
/// large enough to matter.
fn time_resolution_ms(plan: &ExecutionPlan) -> Option<f64> {
    match plan.source_format.as_deref() {
        // A-Time is HH:MM:SS.FF2; the boxes of a text profile say `(0.07s)`.
        Some("oracle-xplan" | "duckdb-text") => Some(10.0),
        _ => None,
    }
}

/// The estimated executions of a node, when the engine gives a usable one.
fn estimated_executions(node: &PlanNode) -> f64 {
    node.estimated_executions
        .filter(|executions| executions.is_finite() && *executions >= 1.0)
        .unwrap_or(1.0)
}

/// True when the parser already multiplied `estimated_rows` by the estimated
/// executions. The SQL Server parsers do that on executed plans, where the
/// row count beside it is the total of every execution.
fn estimate_is_total(dialect: PlanDialect, node: &PlanNode) -> bool {
    dialect == PlanDialect::SqlServer && node.actual_rows.is_some()
}

/// Estimated rows for one execution of the node.
fn estimate_per_execution(dialect: PlanDialect, node: &PlanNode) -> f64 {
    if estimate_is_total(dialect, node) {
        node.estimated_rows / estimated_executions(node)
    } else {
        node.estimated_rows
    }
}

/// Estimated rows across the estimated executions.
fn estimate_total(dialect: PlanDialect, node: &PlanNode) -> Option<f64> {
    if !(node.estimated_rows.is_finite() && node.estimated_rows >= 0.0) {
        return None;
    }
    Some(if estimate_is_total(dialect, node) {
        node.estimated_rows
    } else {
        node.estimated_rows * estimated_executions(node)
    })
}

/// How many times the node ran. SQL Server adds up `ActualExecutions` over
/// the threads of a parallel operator, so one execution on seven threads
/// reads 7; a count no larger than the threads that ran it is one execution.
fn executions_run(dialect: PlanDialect, node: &PlanNode) -> Option<f64> {
    let loops = node.loops.filter(|loops| *loops >= 0.0)?;
    let threads = node.thread_rows.len() as f64;
    if dialect == PlanDialect::SqlServer && threads >= 2.0 && loops > 1.0 && loops <= threads {
        return Some(1.0);
    }
    Some(loops)
}

/// Pages the node reports reading (cache hits and disk reads together).
fn node_pages(node: &PlanNode) -> Option<f64> {
    let buffers = node.buffers.as_ref()?;
    if let Some(reads) = buffers.logical_reads {
        return Some(
            reads.max(0.0) + buffers.local_hit.unwrap_or(0.0) + buffers.local_read.unwrap_or(0.0),
        );
    }
    let parts = [
        buffers.shared_hit,
        buffers.shared_read,
        buffers.local_hit,
        buffers.local_read,
    ];
    if parts.iter().any(Option::is_some) {
        return Some(parts.iter().flatten().sum::<f64>().max(0.0));
    }
    buffers.physical_reads.map(|reads| reads.max(0.0))
}

fn is_init_plan(node: &PlanNode) -> bool {
    node.parent_relationship
        .as_deref()
        .is_some_and(|relationship| relationship.eq_ignore_ascii_case("InitPlan"))
        || node.subplan_name.as_deref().is_some_and(|name| {
            let name = name.trim_start();
            starts_with_ignore_case(name, "InitPlan") || starts_with_ignore_case(name, "CTE ")
        })
}

fn is_cte_definition(node: &PlanNode) -> bool {
    node.subplan_name
        .as_deref()
        .is_some_and(|name| starts_with_ignore_case(name.trim_start(), "CTE "))
}

fn cte_definition_name(node: &PlanNode) -> &str {
    node.subplan_name
        .as_deref()
        .map(str::trim)
        .and_then(|name| name.get(4..))
        .map(str::trim)
        .unwrap_or("")
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Names a PostgreSQL InitPlan can be referred to by: `InitPlan 1 (returns
/// $0,$1)` gives `initplan 1`, `$0`, `$1`.
fn init_plan_tokens(subplan_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let name = subplan_name.trim();
    let head = name.split('(').next().unwrap_or(name).trim();
    if !head.is_empty() {
        out.push(head.to_ascii_lowercase());
    }
    out.extend(
        reference_tokens(name)
            .into_iter()
            .filter(|token| token.starts_with('$')),
    );
    out
}

/// `$n` parameters and `InitPlan n` names mentioned in `text`.
fn reference_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !text.contains('$') && !text.contains("InitPlan") {
        return out;
    }
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'$' {
            let start = at;
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
            if at > start + 1 {
                out.push(text[start..at].to_string());
            }
            continue;
        }
        if bytes[at] == b'I' && text[at..].starts_with("InitPlan ") {
            let start = at;
            at += "InitPlan ".len();
            let digits = at;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
            if at > digits {
                out.push(text[start..at].to_ascii_lowercase());
            }
            continue;
        }
        at += 1;
    }
    out
}

/// Internal names the plan defines, for [`readable_predicate`].
pub fn expression_names(plan: &ExecutionPlan) -> ExpressionNames {
    let mut names = ExpressionNames::new();
    for node in &plan.nodes {
        // PostgreSQL: `InitPlan 1 (returns $0)`.
        if let Some(name) = node.subplan_name.as_deref() {
            let head = name.split('(').next().unwrap_or(name).trim();
            if starts_with_ignore_case(head, "InitPlan") {
                for token in reference_tokens(name) {
                    if token.starts_with('$') {
                        names.entry(token).or_insert_with(|| head.to_string());
                    }
                }
            }
        }
        // SQL Server: `[Expr1004] = Scalar Operator(...)` wherever the parser
        // kept a Compute Scalar's defined values.
        for property in &node.properties {
            let lowered = property.name.to_ascii_lowercase();
            if lowered.contains("defined") {
                collect_defined_values(&property.value, &mut names);
            }
        }
    }
    names
}

/// Read `[Expr1004] = Scalar Operator(<expression>)` definitions, one or
/// several separated by `;` or `, [Expr`.
fn collect_defined_values(text: &str, names: &mut ExpressionNames) {
    for piece in split_definitions(text) {
        let Some((name, value)) = piece.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_start_matches('[').trim_end_matches(']');
        let internal = [
            "Expr",
            "Union",
            "ConstExpr",
            "WindowCount",
            "Segment",
            "RowNumber",
        ]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
        });
        if !internal {
            continue;
        }
        let mut value = value.trim();
        for wrapper in ["Scalar Operator(", "ScalarOperator("] {
            if let Some(inner) = value
                .strip_prefix(wrapper)
                .and_then(|rest| rest.strip_suffix(')'))
            {
                value = inner.trim();
            }
        }
        if !value.is_empty() && value != name {
            names
                .entry(name.to_string())
                .or_insert_with(|| value.to_string());
        }
    }
}

/// Split on `;`, and on `, ` only where the next piece starts a new
/// `[Name] =` definition, staying outside parentheses and string literals.
fn split_definitions(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut start = 0;
    for (at, ch) in text.char_indices() {
        match ch {
            '\'' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth = depth.saturating_sub(1),
            ';' if !in_string && depth == 0 => {
                pieces.push(&text[start..at]);
                start = at + 1;
            }
            ',' if !in_string && depth == 0 => {
                let rest = text[at + 1..].trim_start();
                let next_is_definition = rest.starts_with('[')
                    && rest
                        .find(']')
                        .is_some_and(|close| rest[close + 1..].trim_start().starts_with('='));
                if next_is_definition {
                    pieces.push(&text[start..at]);
                    start = at + 1;
                }
            }
            _ => {}
        }
    }
    pieces.push(&text[start..]);
    pieces
}

// ---------------------------------------------------------------------------
// Facts: what a node touches, from typed fields with text fallbacks
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Facts {
    object: Option<PlanObject>,
    index: Option<String>,
    seek: Option<String>,
    filter: Option<String>,
    join: Option<String>,
    join_type: Option<String>,
    sort_keys: Vec<String>,
    group_keys: Vec<String>,
}

fn non_empty(text: &Option<String>) -> Option<String> {
    text.as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

impl Facts {
    fn read(
        dialect: PlanDialect,
        node: &PlanNode,
        info: &OperatorInfo,
        entry: Option<&'static OperatorEntry>,
    ) -> Self {
        let category = info.category;
        let mut facts = Facts {
            index: non_empty(&node.index_name),
            seek: non_empty(&node.index_cond),
            filter: non_empty(&node.filter),
            join: non_empty(&node.join_predicate),
            join_type: non_empty(&node.join_type).filter(|kind| {
                !kind.eq_ignore_ascii_case("inner") && !kind.eq_ignore_ascii_case("inner join")
            }),
            sort_keys: node.sort_keys.clone(),
            group_keys: node.group_keys.clone(),
            ..Facts::default()
        };
        let mut relation = non_empty(&node.relation);
        let mut schema = non_empty(&node.schema);
        let mut alias = non_empty(&node.alias);
        let mut kind = PlanObjectKind::Table;

        // The prefixed lines the parsers have always written.
        for line in &node.output_list {
            let Some((label, value)) = line.split_once(": ") else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let slot = match label {
                "Filter" | "filter" | "Condition" => &mut facts.filter,
                "Index Cond" | "Recheck Cond" => &mut facts.seek,
                "Hash Cond" | "Merge Cond" | "Join Filter" => &mut facts.join,
                "access" => {
                    if category.is_join() {
                        &mut facts.join
                    } else {
                        &mut facts.seek
                    }
                }
                "Index" | "Key" => &mut facts.index,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(value.to_string());
            }
        }

        let operator = if node.physical_op.trim().is_empty() {
            node.logical_op.trim()
        } else {
            node.physical_op.trim()
        };
        match dialect {
            PlanDialect::MySql | PlanDialect::MariaDb => {
                if let Some(table) = strip_prefix_ignore_case(operator, "Table:") {
                    relation.get_or_insert_with(|| table.trim().to_string());
                } else if let Some(predicate) = strip_prefix_ignore_case(operator, "Filter:") {
                    facts
                        .filter
                        .get_or_insert_with(|| predicate.trim().to_string());
                } else if let Some(keys) = strip_prefix_ignore_case(operator, "Sort:") {
                    if facts.sort_keys.is_empty() {
                        facts.sort_keys = split_top_level(keys)
                            .into_iter()
                            .filter(|key| !key.to_ascii_lowercase().starts_with("limit input"))
                            .collect();
                    }
                } else {
                    let parsed = mysql_tree_target(operator);
                    if relation.is_none() {
                        relation = parsed.table;
                    }
                    if facts.index.is_none() {
                        facts.index = parsed.index;
                    }
                    if facts.seek.is_none() {
                        facts.seek = parsed.condition;
                    }
                }
            }
            PlanDialect::Sqlite => {
                let parsed = sqlite_target(operator);
                if relation.is_none() {
                    relation = parsed.table;
                }
                if alias.is_none() {
                    alias = parsed.alias;
                }
                if facts.index.is_none() {
                    facts.index = parsed.index;
                }
                if facts.seek.is_none() {
                    facts.seek = parsed.condition;
                }
            }
            PlanDialect::Oracle | PlanDialect::DuckDb | PlanDialect::ClickHouse => {
                if let Some(rest) = entry.and_then(|entry| remainder_after_name(operator, entry)) {
                    let is_index_operation = dialect == PlanDialect::Oracle
                        && (starts_with_ignore_case(operator, "INDEX")
                            || starts_with_ignore_case(operator, "BITMAP INDEX"));
                    if is_index_operation {
                        facts.index.get_or_insert_with(|| rest.to_string());
                    } else if relation.is_none()
                        && (category.is_access()
                            || matches!(
                                category,
                                OperatorCategory::Modify | OperatorCategory::Function
                            ))
                    {
                        let mut parts: Vec<&str> = rest.split('.').collect();
                        relation = parts.pop().map(str::to_string);
                        if schema.is_none() {
                            schema = parts.pop().map(str::to_string);
                        }
                    }
                }
            }
            PlanDialect::Postgres => {
                if relation.is_none()
                    && (category.is_access() || category == OperatorCategory::Modify)
                    && let Some(first) = node.output_list.first()
                    && is_plain_identifier(first)
                {
                    relation = Some(first.clone());
                }
            }
            PlanDialect::SqlServer => {}
        }
        // Scan and lookup warnings name the table when nothing else does.
        if relation.is_none() {
            for warning in &node.warnings {
                match warning {
                    PlanWarning::TableScan {
                        table,
                        schema: owner,
                    } if !table.is_empty() => {
                        relation = Some(table.clone());
                        if schema.is_none() && !owner.is_empty() {
                            schema = Some(owner.clone());
                        }
                    }
                    PlanWarning::KeyLookup { table, index } if !table.is_empty() => {
                        relation = Some(table.clone());
                        if facts.index.is_none() && !index.is_empty() {
                            facts.index = Some(index.clone());
                        }
                    }
                    _ => continue,
                }
                break;
            }
        }
        // A CTE scan reads the CTE, whatever alias the statement gave it.
        if let Some(cte) = non_empty(&node.cte_name)
            && !is_cte_definition(node)
            && category.is_access()
        {
            if relation
                .as_deref()
                .is_some_and(|name| !name.eq_ignore_ascii_case(&cte))
                && alias.is_none()
            {
                alias = relation.take();
            }
            relation = Some(cte);
            kind = PlanObjectKind::Cte;
        }
        if let Some(name) = relation {
            let name = unquote_identifier(&name);
            if name.eq_ignore_ascii_case("<temporary>") {
                kind = PlanObjectKind::Temporary;
            }
            let alias = alias
                .map(|alias| unquote_identifier(&alias))
                .filter(|alias| !alias.is_empty() && !alias.eq_ignore_ascii_case(&name));
            if !name.is_empty() {
                facts.object = Some(PlanObject {
                    name,
                    schema: schema.map(|schema| unquote_identifier(&schema)),
                    alias,
                    kind,
                });
            }
        }
        facts.index = facts.index.map(|index| unquote_identifier(&index));
        facts
    }
}

fn strip_prefix_ignore_case<'t>(text: &'t str, prefix: &str) -> Option<&'t str> {
    starts_with_ignore_case(text, prefix).then(|| &text[prefix.len()..])
}

/// `[Orders]`, `"orders"`, `` `orders` `` -> `orders`.
fn unquote_identifier(name: &str) -> String {
    let name = name.trim();
    let pairs = [('[', ']'), ('"', '"'), ('`', '`')];
    for (open, close) in pairs {
        if name.len() >= 2 && name.starts_with(open) && name.ends_with(close) {
            let inner = &name[open.len_utf8()..name.len() - close.len_utf8()];
            if !inner.contains([open, close]) {
                return inner.to_string();
            }
        }
    }
    name.to_string()
}

fn is_plain_identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
}

/// The text after the glossary name the operator starts with:
/// `TABLE ACCESS FULL ORDERS` -> `ORDERS`.
fn remainder_after_name<'t>(operator: &'t str, entry: &OperatorEntry) -> Option<&'t str> {
    let mut best: Option<&str> = None;
    for name in entry.names {
        if operator.len() > name.len()
            && starts_with_ignore_case(operator, name)
            && operator[name.len()..].starts_with(' ')
        {
            let rest = operator[name.len()..].trim();
            if best.is_none_or(|current| rest.len() < current.len()) {
                best = Some(rest);
            }
        }
    }
    best.filter(|rest| {
        !rest.is_empty() && !rest.contains(' ') && !rest.starts_with('(') && !rest.starts_with(':')
    })
}

/// Split on commas that are outside parentheses and quotes.
fn split_top_level(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (at, ch) in text.char_indices() {
        match (quote, ch) {
            (Some(open), _) if ch == open => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                out.push(text[start..at].trim().to_string());
                start = at + 1;
            }
            _ => {}
        }
    }
    out.push(text[start..].trim().to_string());
    out.retain(|piece| !piece.is_empty());
    out
}

#[derive(Debug, Default, PartialEq)]
struct Target {
    table: Option<String>,
    alias: Option<String>,
    index: Option<String>,
    condition: Option<String>,
}

/// MySQL `EXPLAIN FORMAT=TREE` access lines: `Table scan on o`,
/// `Index lookup on c using idx_email (email='a')`,
/// `Index range scan on o using idx_created over ('2026-01-01' <= created_at)`.
fn mysql_tree_target(operator: &str) -> Target {
    let mut target = Target::default();
    let lowered = operator.to_ascii_lowercase();
    let Some(on) = lowered.find(" on ") else {
        return target;
    };
    // Only access lines put a table after `on`.
    let head = &lowered[..on];
    let is_access = ["scan", "lookup", "search", "rows fetched", "index"]
        .iter()
        .any(|word| head.contains(word));
    if !is_access {
        return target;
    }
    let rest = operator[on + 4..].trim_start();
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    if name.is_empty() {
        return target;
    }
    target.table = Some(name.to_string());
    let mut rest = rest[name_end..].trim_start();
    if let Some(after) = strip_prefix_ignore_case(rest, "using ") {
        let after = after.trim_start();
        let end = after
            .find(|ch: char| ch.is_whitespace() || ch == '(')
            .unwrap_or(after.len());
        if end > 0 {
            target.index = Some(after[..end].to_string());
        }
        rest = after[end..].trim_start();
    }
    if let Some(after) = strip_prefix_ignore_case(rest, "over ") {
        rest = after.trim_start();
    }
    if rest.starts_with('(')
        && let Some(close) = matching_paren(rest)
    {
        let condition = rest[1..close].trim();
        if !condition.is_empty() {
            target.condition = Some(condition.to_string());
        }
    }
    target
}

/// Byte offset of the `)` that closes the `(` at the start of `text`.
fn matching_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (at, ch) in text.char_indices() {
        match (quote, ch) {
            (Some(open), _) if ch == open => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// SQLite `EXPLAIN QUERY PLAN` detail lines: `SCAN o`,
/// `SEARCH c USING INTEGER PRIMARY KEY (rowid=?)`,
/// `SEARCH TABLE o AS x USING COVERING INDEX ix (a=? AND b>?)`.
fn sqlite_target(operator: &str) -> Target {
    let mut target = Target::default();
    let (body, condition) = match operator.find('(') {
        Some(open) if operator.trim_end().ends_with(')') => {
            let close = operator.trim_end().len() - 1;
            (&operator[..open], Some(operator[open + 1..close].trim()))
        }
        _ => (operator, None),
    };
    let words: Vec<&str> = body.split_whitespace().collect();
    let Some(verb) = words.first() else {
        return target;
    };
    if !verb.eq_ignore_ascii_case("SCAN") && !verb.eq_ignore_ascii_case("SEARCH") {
        return target;
    }
    let mut at = 1;
    if words
        .get(at)
        .is_some_and(|word| word.eq_ignore_ascii_case("TABLE"))
    {
        at += 1;
    }
    let Some(name) = words.get(at) else {
        return target;
    };
    let reserved = ["SUBQUERY", "CONSTANT", "USING", "AS"];
    let not_a_table = reserved.iter().any(|word| name.eq_ignore_ascii_case(word))
        || name.chars().all(|ch| ch.is_ascii_digit())
        || words
            .get(at + 1)
            .is_some_and(|word| word.eq_ignore_ascii_case("VIRTUAL"));
    if not_a_table {
        // `SCAN SUBQUERY 1 AS x`, `SCAN 3 CONSTANT ROWS`: no table to name.
        return target;
    }
    target.table = Some((*name).to_string());
    at += 1;
    if words
        .get(at)
        .is_some_and(|word| word.eq_ignore_ascii_case("AS"))
    {
        target.alias = words.get(at + 1).map(|alias| (*alias).to_string());
        at += 2;
    }
    if words
        .get(at)
        .is_some_and(|word| word.eq_ignore_ascii_case("USING"))
    {
        let tail: Vec<&str> = words[at + 1..].to_vec();
        let upper: Vec<String> = tail.iter().map(|word| word.to_ascii_uppercase()).collect();
        if let Some(position) = upper.iter().position(|word| word == "INDEX")
            && let Some(index) = tail.get(position + 1)
        {
            target.index = Some((*index).to_string());
        }
        // `USING INTEGER PRIMARY KEY` / `USING PRIMARY KEY` is a lookup by
        // the table's own key, not by a named index.
    }
    target.condition = condition
        .filter(|condition| !condition.is_empty())
        .map(str::to_string);
    target
}

pub(crate) static PHRASES: &[crate::plans::phrases::PlanPhrase] = &[
    crate::plans::phrases::phrase!(
        "metric.time",
        "Time",
        "Execution Plan → metric picker for the diagram heat map and the operator table: each operator's own time."
    ),
    crate::plans::phrases::phrase!(
        "metric.cost",
        "Cost",
        "Execution Plan → metric picker for the diagram heat map and the operator table: each operator's own estimated cost."
    ),
    crate::plans::phrases::phrase!(
        "metric.rows",
        "Rows",
        "Execution Plan → metric picker for the diagram heat map and the operator table: rows each operator produced."
    ),
    crate::plans::phrases::phrase!(
        "metric.estimate",
        "Estimate error",
        "Execution Plan → metric picker for the diagram heat map and the operator table: how far each row estimate was from the actual count."
    ),
    crate::plans::phrases::phrase!(
        "metric.io",
        "Page reads",
        "Execution Plan → metric picker for the diagram heat map and the operator table: pages each operator read."
    ),
    crate::plans::phrases::phrase!(
        "title.table",
        "{operator} on {table}",
        "Execution Plan → Story, diagram and operator table: operator title; {operator} is the engine's operator name (Seq Scan) and {table} the table it reads (orders AS o)."
    ),
    crate::plans::phrases::phrase!(
        "title.table_index",
        "{operator} on {table} using {index}",
        "Execution Plan → Story, diagram and operator table: operator title; {operator} is the engine's operator name, {table} the table and {index} the index it uses."
    ),
    crate::plans::phrases::phrase!(
        "title.index",
        "{operator} using {index}",
        "Execution Plan → Story, diagram and operator table: operator title; {operator} is the engine's operator name and {index} the index it uses."
    ),
    crate::plans::phrases::phrase!(
        "group.parallel",
        "Runs in parallel",
        "Execution Plan → diagram: label on the frame drawn around operators that run on parallel workers."
    ),
    crate::plans::phrases::phrase!(
        "group.parallel_planned",
        "Runs in parallel, {planned} workers planned",
        "Execution Plan → diagram: label on the frame drawn around operators that run on parallel workers; {planned} is a number."
    ),
    crate::plans::phrases::phrase!(
        "group.parallel_workers",
        "Runs in parallel, {launched} of {planned} workers started",
        "Execution Plan → diagram: label on the frame drawn around operators that ran on parallel workers; {launched} and {planned} are numbers."
    ),
];

/// Engine notation that means nothing to a reader, removed before a
/// predicate or key is shown. Spark tags every attribute with an expression
/// id (`customer_id#6L`, `total#7`); the name alone is what the query said.
pub(crate) fn engine_text(engine: PlanEngine, text: &str) -> String {
    if engine != PlanEngine::Spark || !text.contains('#') {
        return text.to_string();
    }
    // `Subquery subquery#224, [id=#1188]`: the plan lists the subquery under
    // the name `subquery#224`, so that is kept and the rest dropped.
    let mut text = text.replace("Subquery subquery#", "subquery#");
    while let Some(start) = text.find(", [id=#") {
        let Some(length) = text[start..].find(']') else {
            break;
        };
        text.replace_range(start..=start + length, "");
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let after_name = at > 0 && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '_');
        if c == '#' && after_name && !out.ends_with("subquery") {
            let mut end = at + 1;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            if end > at + 1 {
                if end < chars.len() && chars[end] == 'L' {
                    end += 1;
                }
                let boundary =
                    end == chars.len() || !(chars[end].is_alphanumeric() || chars[end] == '_');
                if boundary {
                    at = end;
                    continue;
                }
            }
        }
        out.push(c);
        at += 1;
    }
    out
}

/// A sort key in the reader's terms. CockroachDB prints the direction as a
/// sign in front of the column: `-sum` is `sum` descending, `+id` ascending.
pub(crate) fn engine_sort_key(engine: PlanEngine, key: &str) -> String {
    if engine == PlanEngine::CockroachDb {
        let key = key.trim();
        if let Some(column) = key.strip_prefix('-')
            && !column.is_empty()
        {
            return format!("{column} DESC");
        }
        if let Some(column) = key.strip_prefix('+')
            && !column.is_empty()
        {
            return column.to_string();
        }
        return key.to_string();
    }
    engine_text(engine, key)
}

/// `#0`, `#12`: a column named only by its position.
fn is_positional(key: &str) -> bool {
    key.strip_prefix('#')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests;
