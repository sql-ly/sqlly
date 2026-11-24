//! Per-node facts the rules share: what kind of work each operator does,
//! which table it touches, how many rows it read and kept, and what share
//! of the statement it accounts for. Built on [`crate::plans::analysis`].

use super::ImpactBasis;
use super::predicate::{ColumnRef, Term, parse_predicate};
use crate::plans::analysis::{
    NodeAnalysis, NodeMetrics, PlanAnalysis, PlanObjectKind, analyze_plan,
};
use crate::plans::import::PlanEngine;
use crate::plans::phrases::styled;
use crate::plans::schema::{SchemaContext, SchemaTable, bare_name};
use crate::plans::{ExecutionPlan, PlanDialect, PlanNode};

/// What an operator does, as far as the rules care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// Reads every row of a table (heap or clustered index).
    FullScan,
    /// Reads every entry of a secondary index.
    IndexFullScan,
    /// Reads part of an index through a search condition.
    IndexSeek,
    /// PostgreSQL index-only scan.
    IndexOnly,
    BitmapHeap,
    BitmapIndex,
    BitmapAnd,
    BitmapOr,
    /// Fetches the rest of a row by key or row id after an index found it.
    RowLookup,
    IndexMerge,
    CteScan,
    SubqueryScan,
    FunctionScan,
    NestedLoop,
    HashJoin,
    MergeJoin,
    CrossJoin,
    /// PostgreSQL `Hash`: the build side of a hash join.
    HashBuild,
    Sort,
    Aggregate,
    Distinct,
    Limit,
    /// Gather / Parallelism / PX coordinator.
    Gather,
    Spool,
    Materialize,
    Filter,
    Partition,
    Append,
    Other,
}

impl Op {
    pub fn is_join(self) -> bool {
        matches!(
            self,
            Op::NestedLoop | Op::HashJoin | Op::MergeJoin | Op::CrossJoin
        )
    }

    /// Reads rows from a table or index (not a lookup by row id).
    pub fn is_read(self) -> bool {
        matches!(
            self,
            Op::FullScan | Op::IndexFullScan | Op::IndexSeek | Op::IndexOnly | Op::BitmapHeap
        )
    }
}

/// The table a node reads, with its schema definition when one was supplied.
pub(crate) struct Rel<'a> {
    pub name: String,
    pub schema: Option<String>,
    pub alias: Option<String>,
    pub table: Option<&'a SchemaTable>,
    /// The name may be a query alias: the engine prints only the alias and
    /// no schema was supplied to resolve it.
    pub alias_only: bool,
}

impl Rel<'_> {
    /// `schema.table`, or `table` when the plan gives no schema.
    pub fn display(&self) -> String {
        match &self.schema {
            Some(schema) if !schema.is_empty() => format!("{schema}.{}", self.name),
            _ => self.name.clone(),
        }
    }

    /// True when `column` can belong to this relation: unqualified, or
    /// qualified with the relation's name or alias.
    pub fn owns(&self, column: &ColumnRef) -> bool {
        match &column.qualifier {
            None => true,
            Some(qualifier) => {
                qualifier.eq_ignore_ascii_case(&self.name)
                    || self
                        .alias
                        .as_deref()
                        .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
            }
        }
    }
}

/// A table read together with the filter applied to it, whether the engine
/// prints the filter on the scan or as a separate operator above it.
pub(crate) struct Scan {
    /// The node the finding attaches to: the filter operator when there is
    /// one, else the scan.
    pub top: usize,
    pub read: Option<f64>,
    pub kept: Option<f64>,
    pub filter_raw: Option<String>,
    pub filter_text: Option<String>,
    pub est_read: Option<f64>,
    pub est_kept: Option<f64>,
}

pub(crate) struct Ctx<'a> {
    pub plan: &'a ExecutionPlan,
    pub schema: Option<&'a SchemaContext>,
    pub an: PlanAnalysis,
    pub dialect: PlanDialect,
    /// The engine the plan came from. CockroachDB, Trino and Spark share
    /// PostgreSQL's `dialect` and are told apart here.
    pub engine: PlanEngine,
    pub ops: Vec<Op>,
    pub children: Vec<Vec<usize>>,
    pub parent: Vec<Option<usize>>,
    /// The plan has per-operator timings worth using as the impact basis.
    pub timed: bool,
}

impl<'a> Ctx<'a> {
    pub fn new(plan: &'a ExecutionPlan, schema: Option<&'a SchemaContext>) -> Self {
        let an = analyze_plan(plan);
        let dialect = an.dialect;
        let engine = an.engine;
        let count = plan.nodes.len();
        let mut children = vec![Vec::new(); count];
        let mut parent = vec![None; count];
        for (position, node) in an.nodes.iter().enumerate().take(count) {
            for child in &node.children {
                if let Some(child) = an.position(*child)
                    && child < count
                    && child != position
                {
                    children[position].push(child);
                    parent[child] = Some(position);
                }
            }
        }
        // CockroachDB, Trino and Spark have their own operator names and
        // their own rules (`rules_engines`); the PostgreSQL classification
        // must not guess at them.
        let ops = plan
            .nodes
            .iter()
            .map(|node| {
                if engine.dialect().is_some() {
                    classify(dialect, node)
                } else {
                    Op::Other
                }
            })
            .collect();
        let timed = an.has_timing && an.total_time_ms.is_some_and(|total| total > 0.0);
        Self {
            plan,
            schema,
            an,
            dialect,
            engine,
            ops,
            children,
            parent,
            timed,
        }
    }

    pub fn len(&self) -> usize {
        self.plan.nodes.len().min(self.an.nodes.len())
    }

    pub fn node(&self, position: usize) -> &PlanNode {
        &self.plan.nodes[position]
    }

    pub fn a(&self, position: usize) -> &NodeAnalysis {
        &self.an.nodes[position]
    }

    pub fn m(&self, position: usize) -> &NodeMetrics {
        &self.an.nodes[position].metrics
    }

    pub fn op(&self, position: usize) -> Op {
        self.ops[position]
    }

    pub fn kids(&self, position: usize) -> &[usize] {
        &self.children[position]
    }

    pub fn up(&self, position: usize) -> Option<usize> {
        self.parent[position]
    }

    /// Share of the statement spent in this node and its inputs: measured
    /// time when the plan has it, else the optimizer's cost.
    pub fn share(&self, position: usize) -> Option<f64> {
        let metrics = self.m(position);
        if self.timed {
            metrics.inclusive_time_share
        } else if self.an.has_costs {
            metrics.inclusive_cost_share
        } else {
            None
        }
    }

    /// How many times the node ran, when it ran more than once. A loop count
    /// that only counts parallel processes is not a repeat.
    pub fn executions(&self, position: usize) -> Option<f64> {
        let metrics = self.m(position);
        if metrics.processes.is_some() {
            return None;
        }
        metrics.loops.filter(|loops| *loops > 1.0)
    }

    /// True when the statement is long enough for a finding to be called
    /// critical: unknown, or at least one second.
    pub fn long_running(&self) -> bool {
        !self.timed
            || self
                .an
                .execution_time_ms
                .or(self.an.total_time_ms)
                .is_none_or(|total| total >= 1_000.0)
    }

    /// Share of the statement spent in this node alone.
    pub fn self_share(&self, position: usize) -> Option<f64> {
        let metrics = self.m(position);
        if self.timed {
            metrics.time_share
        } else if self.an.has_costs {
            metrics.cost_share
        } else {
            None
        }
    }

    pub fn basis(&self) -> Option<ImpactBasis> {
        if self.timed {
            Some(ImpactBasis::Time)
        } else if self.an.has_costs {
            Some(ImpactBasis::Cost)
        } else {
            None
        }
    }

    pub fn title(&self, position: usize) -> String {
        self.a(position).title().fallback().to_string()
    }

    /// The operator name in upper case (physical, else logical).
    pub fn op_name(&self, position: usize) -> String {
        op_text(self.node(position)).to_ascii_uppercase()
    }

    pub fn executed(&self, position: usize) -> bool {
        !self.node(position).never_executed && !self.a(position).flags.never_executed
    }

    pub fn has_actuals(&self) -> bool {
        self.an.has_actuals
    }

    /// Every node below `position`.
    pub fn descendants(&self, position: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack: Vec<usize> = self.kids(position).to_vec();
        while let Some(next) = stack.pop() {
            if out.contains(&next) {
                continue;
            }
            out.push(next);
            stack.extend(self.kids(next).iter().copied());
        }
        out
    }

    /// Every node above `position`, nearest first.
    pub fn ancestors(&self, position: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut current = self.up(position);
        while let Some(next) = current {
            if out.contains(&next) {
                break;
            }
            out.push(next);
            current = self.up(next);
        }
        out
    }

    /// The real table this node reads (not a CTE or an internal temporary
    /// table).
    pub fn rel(&self, position: usize) -> Option<Rel<'a>> {
        let object = self.a(position).object.as_ref()?;
        if object.kind != PlanObjectKind::Table {
            return None;
        }
        let name = bare_name(&object.name);
        if name.is_empty() || name.starts_with('<') {
            return None;
        }
        let schema = object
            .schema
            .as_deref()
            .map(bare_name)
            .filter(|schema| !schema.is_empty());
        let alias = object
            .alias
            .as_deref()
            .map(bare_name)
            .filter(|alias| !alias.is_empty() && !alias.eq_ignore_ascii_case(&name));
        let table = self
            .schema
            .and_then(|context| context.table(schema.as_deref(), &name));
        let schema = schema.or_else(|| {
            table
                .map(|table| table.schema.clone())
                .filter(|schema| !schema.is_empty())
        });
        // MySQL, MariaDB and SQLite print the query's alias in place of the
        // table name. Without a schema to confirm the name, say so.
        let alias_only = table.is_none()
            && alias.is_none()
            && !self.node(position).relation_resolved
            && matches!(
                self.dialect,
                PlanDialect::MySql | PlanDialect::MariaDb | PlanDialect::Sqlite
            );
        Some(Rel {
            name,
            schema,
            alias,
            table,
            alias_only,
        })
    }

    /// The read at `position` with the filter that applies to it.
    pub fn scan(&self, position: usize) -> Scan {
        let node = self.node(position);
        let analysis = self.a(position);
        let metrics = &analysis.metrics;
        let est_read = node.estimated_rows_read.filter(|rows| *rows > 0.0);
        if let Some(filter) = &analysis.filter {
            let removed = metrics.rows_removed;
            // Without a "rows read" or "rows removed" figure there is
            // nothing to compare the output with.
            let read = match (node.rows_read, removed) {
                (Some(read), _) => Some(read),
                (None, Some(removed)) => metrics.rows_out.map(|out| out + removed),
                (None, None) => None,
            };
            let kept = match (read, removed) {
                (Some(read), Some(removed)) => Some((read - removed).max(0.0)),
                _ => metrics.rows_out,
            };
            let est_base = est_read;
            let est_kept = match (est_base, node.filtered_percent) {
                (Some(base), Some(percent)) => Some(base * percent / 100.0),
                (Some(_), None) => metrics.estimated_rows,
                _ => None,
            };
            return Scan {
                top: position,
                read,
                kept,
                filter_raw: Some(filter.raw.clone()),
                filter_text: Some(filter.text.clone()),
                est_read: est_base,
                est_kept,
            };
        }
        // MySQL's tree format and DuckDB print the filter as its own
        // operator directly above the scan.
        if let Some(parent) = self.up(position)
            && self.op(parent) == Op::Filter
            && self.kids(parent).len() == 1
            && let Some(filter) = &self.a(parent).filter
        {
            let parent_metrics = self.m(parent);
            return Scan {
                top: parent,
                read: metrics.rows_out,
                kept: parent_metrics.rows_out,
                filter_raw: Some(filter.raw.clone()),
                filter_text: Some(filter.text.clone()),
                est_read: metrics.estimated_rows_total.or(metrics.estimated_rows),
                est_kept: parent_metrics
                    .estimated_rows_total
                    .or(parent_metrics.estimated_rows),
            };
        }
        Scan {
            top: position,
            read: metrics.rows_in.or(metrics.rows_out),
            kept: metrics.rows_out,
            filter_raw: None,
            filter_text: None,
            est_read,
            est_kept: None,
        }
    }

    pub fn terms(&self, text: &str) -> Vec<Term> {
        parse_predicate(text, Some(self.dialect))
    }

    /// True when the size of the work justifies a finding: enough rows and
    /// enough of the statement. Without timings or costs, rows alone decide.
    pub fn matters(&self, rows: f64, share: Option<f64>) -> bool {
        match share {
            Some(share) => {
                (rows >= 10_000.0 && share >= 0.05) || (rows >= 1_000.0 && share >= 0.25)
            }
            None => rows >= 10_000.0,
        }
    }
}

fn op_text(node: &PlanNode) -> &str {
    if node.physical_op.trim().is_empty() {
        node.logical_op.trim()
    } else {
        node.physical_op.trim()
    }
}

fn classify(dialect: PlanDialect, node: &PlanNode) -> Op {
    let upper = op_text(node).to_ascii_uppercase();
    let logical = node.logical_op.to_ascii_uppercase();
    let u = upper.as_str();
    match dialect {
        PlanDialect::SqlServer => {
            let lookup = node
                .property("Lookup")
                .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
            if u.contains("LOOKUP") || lookup {
                Op::RowLookup
            } else if u.contains("SPOOL") {
                Op::Spool
            } else if u.contains("COLUMNSTORE") {
                Op::Other
            } else if u.contains("SEEK") {
                Op::IndexSeek
            } else if u.starts_with("TABLE SCAN") || u.starts_with("CLUSTERED INDEX SCAN") {
                Op::FullScan
            } else if u.starts_with("INDEX SCAN") {
                Op::IndexFullScan
            } else if u.starts_with("NESTED LOOPS") {
                Op::NestedLoop
            } else if u.starts_with("HASH MATCH") {
                if logical.contains("JOIN") {
                    Op::HashJoin
                } else if logical.contains("DISTINCT") {
                    Op::Distinct
                } else if logical.contains("AGGREGATE") {
                    Op::Aggregate
                } else {
                    Op::Other
                }
            } else if u.starts_with("MERGE JOIN") {
                Op::MergeJoin
            } else if u.starts_with("SORT") {
                Op::Sort
            } else if u.contains("AGGREGATE") {
                Op::Aggregate
            } else if u.starts_with("TOP") {
                Op::Limit
            } else if u.starts_with("PARALLELISM") {
                Op::Gather
            } else if u.starts_with("FILTER") {
                Op::Filter
            } else if u.contains("VALUED FUNCTION") {
                Op::FunctionScan
            } else {
                Op::Other
            }
        }
        PlanDialect::Postgres => {
            let u = u.strip_prefix("PARALLEL ").unwrap_or(u);
            if u.starts_with("SEQ SCAN") {
                Op::FullScan
            } else if u.starts_with("INDEX ONLY SCAN") {
                Op::IndexOnly
            } else if u.starts_with("INDEX SCAN") {
                if node
                    .index_cond
                    .as_deref()
                    .is_some_and(|c| !c.trim().is_empty())
                {
                    Op::IndexSeek
                } else {
                    Op::IndexFullScan
                }
            } else if u.starts_with("BITMAP HEAP SCAN") {
                Op::BitmapHeap
            } else if u.starts_with("BITMAP INDEX SCAN") {
                Op::BitmapIndex
            } else if u.starts_with("BITMAPAND") {
                Op::BitmapAnd
            } else if u.starts_with("BITMAPOR") {
                Op::BitmapOr
            } else if u.starts_with("CTE SCAN") {
                Op::CteScan
            } else if u.starts_with("SUBQUERY SCAN") {
                Op::SubqueryScan
            } else if u.starts_with("FUNCTION SCAN") {
                Op::FunctionScan
            } else if u.starts_with("NESTED LOOP") {
                Op::NestedLoop
            } else if u.starts_with("HASH JOIN") {
                Op::HashJoin
            } else if u.starts_with("MERGE JOIN") {
                Op::MergeJoin
            } else if u == "HASH" {
                Op::HashBuild
            } else if u.starts_with("SORT") || u.starts_with("INCREMENTAL SORT") {
                Op::Sort
            } else if u.contains("AGGREGATE") {
                Op::Aggregate
            } else if u.starts_with("UNIQUE") {
                Op::Distinct
            } else if u.starts_with("LIMIT") {
                Op::Limit
            } else if u.starts_with("GATHER") {
                Op::Gather
            } else if u.starts_with("MATERIALIZE") || u.starts_with("MEMOIZE") {
                Op::Materialize
            } else if u.starts_with("APPEND") || u.starts_with("MERGE APPEND") {
                Op::Append
            } else {
                Op::Other
            }
        }
        PlanDialect::MySql | PlanDialect::MariaDb => {
            if u.starts_with("TABLE SCAN ON") {
                if u.contains("<TEMPORARY>") || u.contains("<UNION") {
                    Op::Other
                } else {
                    Op::FullScan
                }
            } else if u.starts_with("TABLE") && logical.starts_with("ACCESS:") {
                match logical.trim_start_matches("ACCESS:").trim() {
                    "ALL" => Op::FullScan,
                    "INDEX" => Op::IndexFullScan,
                    "INDEX_MERGE" => Op::IndexMerge,
                    "RANGE" | "REF" | "EQ_REF" | "CONST" | "REF_OR_NULL" | "SYSTEM" => {
                        Op::IndexSeek
                    }
                    _ => Op::Other,
                }
            } else if u.starts_with("INTERSECT ROWS") {
                Op::IndexMerge
            } else if u.starts_with("INDEX SCAN ON") || u.starts_with("COVERING INDEX SCAN") {
                Op::IndexFullScan
            } else if u.contains("INDEX LOOKUP") || u.contains("INDEX RANGE SCAN") {
                Op::IndexSeek
            } else if u.contains("HASH JOIN") {
                Op::HashJoin
            } else if u.starts_with("NESTED LOOP") {
                Op::NestedLoop
            } else if u.starts_with("FILTER") {
                Op::Filter
            } else if u.starts_with("SORT")
                || u.starts_with("FILESORT")
                || u.starts_with("ORDERING")
            {
                Op::Sort
            } else if u.starts_with("AGGREGATE")
                || u.starts_with("GROUP AGGREGATE")
                || u.starts_with("GROUPING")
            {
                Op::Aggregate
            } else if u.starts_with("LIMIT") {
                Op::Limit
            } else if u.starts_with("MATERIALIZE") {
                Op::Materialize
            } else {
                Op::Other
            }
        }
        PlanDialect::Sqlite => {
            if u.starts_with("SCAN") {
                if node.relation.is_none() {
                    Op::Other
                } else if u.contains("USING INDEX") || u.contains("USING COVERING INDEX") {
                    Op::IndexFullScan
                } else {
                    Op::FullScan
                }
            } else if u.starts_with("SEARCH") {
                Op::IndexSeek
            } else if u.starts_with("USE TEMP B-TREE FOR ORDER BY") {
                Op::Sort
            } else {
                Op::Other
            }
        }
        PlanDialect::DuckDb => {
            if u.starts_with("SEQ_SCAN") || u.starts_with("TABLE_SCAN") {
                Op::FullScan
            } else if u.starts_with("INDEX_SCAN") {
                Op::IndexSeek
            } else if u.starts_with("HASH_JOIN") {
                Op::HashJoin
            } else if u.starts_with("NESTED_LOOP_JOIN") || u.starts_with("BLOCKWISE_NL_JOIN") {
                Op::NestedLoop
            } else if u.starts_with("PIECEWISE_MERGE_JOIN") {
                Op::MergeJoin
            } else if u.starts_with("CROSS_PRODUCT") {
                Op::CrossJoin
            } else if u.starts_with("FILTER") {
                Op::Filter
            } else if u.starts_with("ORDER_BY") || u.starts_with("TOP_N") {
                Op::Sort
            } else if u.contains("GROUP_BY") || u.contains("AGGREGATE") {
                Op::Aggregate
            } else if u.starts_with("LIMIT") {
                Op::Limit
            } else {
                Op::Other
            }
        }
        PlanDialect::ClickHouse => {
            if u.starts_with("READFROMMERGETREE") {
                Op::FullScan
            } else if u.starts_with("SORTING") {
                Op::Sort
            } else if u.starts_with("AGGREGATING") {
                Op::Aggregate
            } else if u.starts_with("FILTER") {
                Op::Filter
            } else if u.starts_with("LIMIT") {
                Op::Limit
            } else {
                Op::Other
            }
        }
        PlanDialect::Oracle => {
            if u.starts_with("TABLE ACCESS") {
                if u.contains("ROWID") {
                    Op::RowLookup
                } else if u.contains("FULL") {
                    Op::FullScan
                } else {
                    Op::Other
                }
            } else if u.starts_with("INDEX") {
                if u.contains("FULL SCAN") {
                    Op::IndexFullScan
                } else {
                    Op::IndexSeek
                }
            } else if u.starts_with("MERGE JOIN CARTESIAN") {
                Op::CrossJoin
            } else if u.starts_with("HASH JOIN") {
                Op::HashJoin
            } else if u.starts_with("NESTED LOOPS") {
                Op::NestedLoop
            } else if u.starts_with("MERGE JOIN") {
                Op::MergeJoin
            } else if u.starts_with("SORT ORDER BY") {
                Op::Sort
            } else if u.starts_with("SORT UNIQUE") || u.starts_with("HASH UNIQUE") {
                Op::Distinct
            } else if u.starts_with("SORT GROUP BY")
                || u.starts_with("HASH GROUP BY")
                || u.starts_with("SORT AGGREGATE")
            {
                Op::Aggregate
            } else if u.starts_with("FILTER") {
                Op::Filter
            } else if u.starts_with("COUNT STOPKEY") {
                Op::Limit
            } else if u.starts_with("PX ") {
                Op::Gather
            } else if u.starts_with("PARTITION") {
                Op::Partition
            } else {
                Op::Other
            }
        }
    }
}

pub(crate) fn count(rows: f64) -> String {
    super::human_count(rows)
}

/// `42%`, `<1%`.
pub(crate) fn pct(share: f64) -> String {
    let percent = share * 100.0;
    if !percent.is_finite() || percent <= 0.0 {
        "0%".to_string()
    } else if percent < 1.0 {
        "<1%".to_string()
    } else {
        format!("{:.0}%", percent.min(100.0))
    }
}

/// `0.42 ms`, `240 ms`, `1.24 s`, `2.1 min`.
pub(crate) fn ms(value: f64) -> String {
    styled(if !value.is_finite() || value < 0.0 {
        "0 ms".to_string()
    } else if value < 10.0 {
        format!("{value:.2} ms")
    } else if value < 1_000.0 {
        format!("{value:.0} ms")
    } else if value < 60_000.0 {
        format!("{:.2} s", value / 1_000.0)
    } else {
        format!("{:.1} min", value / 60_000.0)
    })
}

/// `512 kB`, `64 MB`, `1.2 GB`.
pub(crate) fn mem_kb(kb: f64) -> String {
    styled(if !kb.is_finite() || kb < 0.0 {
        "0 kB".to_string()
    } else if kb < 1_024.0 {
        format!("{kb:.0} kB")
    } else if kb < 1_024.0 * 1_024.0 {
        format!("{:.0} MB", kb / 1_024.0)
    } else {
        format!("{:.1} GB", kb / 1_024.0 / 1_024.0)
    })
}

/// `3.5×`, `37×`, `1,200×`, `10,000×`.
pub(crate) fn ratio(value: f64) -> String {
    styled(if value >= 10.0 {
        format!("{}×", super::whole_factor_plain(value))
    } else {
        format!("{value:.1}×")
    })
}
