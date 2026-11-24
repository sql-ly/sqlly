//! Plan findings: what is slow in this plan, why, and what to do about it.
//!
//! [`findings_for_plan_with_schema`] runs a fixed set of rules over a parsed
//! plan. Every rule fires only on evidence in the plan (row counts, loop
//! counts, timings, predicates, warnings the engine attached) and, where it
//! matters, only above a threshold that takes both the absolute row count and
//! the operator's share of the statement into account. A rule that is unsure
//! stays silent: a wrong suggestion costs more than a missed one.
//!
//! Each [`PlanFinding`] names the real table, index and predicate from the
//! plan, says why that costs time, and gives the next step, with runnable SQL
//! in the engine's dialect when the plan (or the supplied schema) names
//! everything the statement needs. Findings are ranked so the first one is
//! the one most likely to save the most time.
//!
//! Layout:
//!
//! - `facts`: per-node facts shared by the rules, on top of
//!   [`crate::plans::analysis`].
//! - `predicate`: reads predicate text far enough to find the columns.
//! - `advice`: index DDL, statistics commands and quoting per engine.
//! - `rules_*`: the rules, grouped by subject, each with its English strings.
//! - `strings`: the string table and [`strings::tr`].

mod advice;
mod facts;
mod predicate;
mod rules_access;
mod rules_engines;
mod rules_estimates;
mod rules_join;
mod rules_memory;
mod rules_misc;
mod rules_predicate;
mod rules_schema;
mod strings;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use strings::catalog_entries as finding_catalog_entries;

use crate::plans::schema::SchemaContext;
use crate::plans::{ExecutionPlan, PlanNode};
use advice::{IndexAdvice, IndexSuggestion};
use facts::Ctx;
use sqlly_core::DisplayMessage;
use strings::{Args, tr};

fn message(key: &'static str, fallback: impl Into<String>) -> DisplayMessage {
    DisplayMessage::new(key, fallback)
}

/// How loudly a finding should be signaled. Maps onto the app's semantic
/// status palette (info / warning / error), the only color the plan view
/// is allowed to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingSeverity {
    Info,
    Warning,
    Critical,
}

/// Stable machine id of a finding. [`FindingKind::as_str`] is the id the
/// viewers, the article link table and saved state use; it never changes
/// once shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingKind {
    MissingIndex,
    TableScan,
    FilterDiscardsRows,
    KeyLookup,
    HeapFetches,
    ExpensiveSort,
    AutoIndex,
    IndexMerge,
    LossyBitmap,
    EstimateMismatch,
    StaleStatistics,
    Cardinality,
    ParameterSniffing,
    Spill,
    MemoryGrant,
    NoJoin,
    NestedLoopVolume,
    HashBuildSide,
    JoinFanout,
    DependentSubquery,
    JoinBuffer,
    TemporaryTable,
    CteMaterialization,
    PartitionPruning,
    ImplicitConversion,
    NonSargable,
    ScalarUdf,
    TableVariable,
    Spool,
    RowGoal,
    ParallelWorkers,
    ParallelSkew,
    SerialPlanForced,
    JitOverhead,
    PlanningTime,
    SchemaIndexNotUsed,
    SchemaFkUnindexed,
    SchemaTypeMismatch,
    SchemaNullableNotIn,
    SchemaHeapTable,
    SchemaWideIndex,
    SchemaStale,
    Other,
}

impl FindingKind {
    /// Every kind, in a stable order.
    pub const ALL: &'static [FindingKind] = &[
        Self::MissingIndex,
        Self::TableScan,
        Self::FilterDiscardsRows,
        Self::KeyLookup,
        Self::HeapFetches,
        Self::ExpensiveSort,
        Self::AutoIndex,
        Self::IndexMerge,
        Self::LossyBitmap,
        Self::EstimateMismatch,
        Self::StaleStatistics,
        Self::Cardinality,
        Self::ParameterSniffing,
        Self::Spill,
        Self::MemoryGrant,
        Self::NoJoin,
        Self::NestedLoopVolume,
        Self::HashBuildSide,
        Self::JoinFanout,
        Self::DependentSubquery,
        Self::JoinBuffer,
        Self::TemporaryTable,
        Self::CteMaterialization,
        Self::PartitionPruning,
        Self::ImplicitConversion,
        Self::NonSargable,
        Self::ScalarUdf,
        Self::TableVariable,
        Self::Spool,
        Self::RowGoal,
        Self::ParallelWorkers,
        Self::ParallelSkew,
        Self::SerialPlanForced,
        Self::JitOverhead,
        Self::PlanningTime,
        Self::SchemaIndexNotUsed,
        Self::SchemaFkUnindexed,
        Self::SchemaTypeMismatch,
        Self::SchemaNullableNotIn,
        Self::SchemaHeapTable,
        Self::SchemaWideIndex,
        Self::SchemaStale,
        Self::Other,
    ];

    /// The stable snake_case id.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingIndex => "missing_index",
            Self::TableScan => "table_scan",
            Self::FilterDiscardsRows => "filter_discards_rows",
            Self::KeyLookup => "key_lookup",
            Self::HeapFetches => "heap_fetches",
            Self::ExpensiveSort => "expensive_sort",
            Self::AutoIndex => "auto_index",
            Self::IndexMerge => "index_merge",
            Self::LossyBitmap => "lossy_bitmap",
            Self::EstimateMismatch => "estimate_mismatch",
            Self::StaleStatistics => "stale_statistics",
            Self::Cardinality => "cardinality",
            Self::ParameterSniffing => "parameter_sniffing",
            Self::Spill => "spill",
            Self::MemoryGrant => "memory_grant",
            Self::NoJoin => "no_join",
            Self::NestedLoopVolume => "nested_loop_volume",
            Self::HashBuildSide => "hash_build_side",
            Self::JoinFanout => "join_fanout",
            Self::DependentSubquery => "dependent_subquery",
            Self::JoinBuffer => "join_buffer",
            Self::TemporaryTable => "temporary_table",
            Self::CteMaterialization => "cte_materialization",
            Self::PartitionPruning => "partition_pruning",
            Self::ImplicitConversion => "implicit_conversion",
            Self::NonSargable => "non_sargable",
            Self::ScalarUdf => "scalar_udf",
            Self::TableVariable => "table_variable",
            Self::Spool => "spool",
            Self::RowGoal => "row_goal",
            Self::ParallelWorkers => "parallel_workers",
            Self::ParallelSkew => "parallel_skew",
            Self::SerialPlanForced => "serial_plan_forced",
            Self::JitOverhead => "jit_overhead",
            Self::PlanningTime => "planning_time",
            Self::SchemaIndexNotUsed => "schema_index_not_used",
            Self::SchemaFkUnindexed => "schema_fk_unindexed",
            Self::SchemaTypeMismatch => "schema_type_mismatch",
            Self::SchemaNullableNotIn => "schema_nullable_not_in",
            Self::SchemaHeapTable => "schema_heap_table",
            Self::SchemaWideIndex => "schema_wide_index",
            Self::SchemaStale => "schema_stale",
            Self::Other => "other",
        }
    }

    /// The kind with this id, if any.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.as_str() == id)
    }

    pub fn category(self) -> FindingCategory {
        use FindingCategory as C;
        match self {
            Self::MissingIndex
            | Self::TableScan
            | Self::FilterDiscardsRows
            | Self::KeyLookup
            | Self::HeapFetches
            | Self::ExpensiveSort
            | Self::AutoIndex
            | Self::IndexMerge
            | Self::PartitionPruning => C::Indexing,
            Self::EstimateMismatch
            | Self::ParameterSniffing
            | Self::TableVariable
            | Self::RowGoal => C::Estimates,
            Self::StaleStatistics | Self::Cardinality => C::Statistics,
            Self::Spill | Self::MemoryGrant | Self::LossyBitmap | Self::TemporaryTable => C::Memory,
            Self::NoJoin
            | Self::NestedLoopVolume
            | Self::HashBuildSide
            | Self::JoinFanout
            | Self::JoinBuffer => C::Join,
            Self::ParallelWorkers | Self::ParallelSkew | Self::SerialPlanForced => C::Parallelism,
            Self::ImplicitConversion | Self::NonSargable | Self::CteMaterialization => C::Predicate,
            Self::DependentSubquery | Self::ScalarUdf | Self::Spool => C::RowByRow,
            Self::JitOverhead | Self::PlanningTime => C::Configuration,
            Self::SchemaIndexNotUsed
            | Self::SchemaFkUnindexed
            | Self::SchemaTypeMismatch
            | Self::SchemaNullableNotIn
            | Self::SchemaHeapTable
            | Self::SchemaWideIndex
            | Self::SchemaStale => C::Schema,
            Self::Other => C::Other,
        }
    }
}

/// Broad subject of a finding, for grouping and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingCategory {
    Indexing,
    Estimates,
    Statistics,
    Memory,
    Join,
    Parallelism,
    Predicate,
    RowByRow,
    Configuration,
    Schema,
    Other,
}

impl FindingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexing => "indexing",
            Self::Estimates => "estimates",
            Self::Statistics => "statistics",
            Self::Memory => "memory",
            Self::Join => "join",
            Self::Parallelism => "parallelism",
            Self::Predicate => "predicate",
            Self::RowByRow => "row_by_row",
            Self::Configuration => "configuration",
            Self::Schema => "schema",
            Self::Other => "other",
        }
    }
}

/// How sure the rule is that the finding is real and the advice applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingConfidence {
    /// The plan hints at it; check before acting (estimated plan, alias
    /// instead of a table name, a heuristic about intent).
    Low,
    /// The evidence is there but one piece is inferred.
    Medium,
    /// Measured in the plan or stated by the engine.
    High,
}

impl FindingConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// What a finding's impact share was measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImpactBasis {
    /// Measured run time (executed plans with per-operator timing).
    Time,
    /// The optimizer's cost estimate.
    Cost,
}

impl ImpactBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Cost => "cost",
        }
    }
}

/// How much of the statement the finding is about. This is the part of the
/// plan the finding covers, which is an upper bound on what fixing it can
/// save, not a promise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FindingImpact {
    /// 0..1 share of the statement's time or estimated cost.
    pub share: f64,
    pub basis: ImpactBasis,
    /// The measured time behind `share`, when the basis is time.
    pub time_ms: Option<f64>,
    /// True when `share` is the optimizer's own improvement estimate (SQL
    /// Server missing-index impact) rather than a share of the plan.
    pub optimizer_estimate: bool,
}

/// Another way to fix the same finding, when the trade-off is real.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingAlternative {
    pub label: DisplayMessage,
    pub sql: Option<String>,
}

/// A single finding: what is happening, why it costs, and the next step.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanFinding {
    /// Stable machine id; see [`FindingKind::as_str`].
    pub kind: FindingKind,
    pub category: FindingCategory,
    pub severity: FindingSeverity,
    pub confidence: FindingConfidence,
    /// One sentence: what is happening in this plan, with the real table,
    /// index and numbers.
    pub summary: DisplayMessage,
    /// Why it costs time.
    pub why: DisplayMessage,
    /// The suggested next step.
    pub action: DisplayMessage,
    /// Runnable SQL for the next step, in the plan's dialect, when the plan
    /// or the schema names everything the statement needs.
    pub action_sql: Option<String>,
    /// Other ways to fix it.
    pub alternatives: Vec<FindingAlternative>,
    /// Technical detail shown on expansion: operator, node and the evidence
    /// lines as one text.
    pub detail: DisplayMessage,
    /// The measured values the finding rests on, one message per line.
    pub evidence: Vec<DisplayMessage>,
    /// Caveats and trade-offs (write cost of an index, alias instead of a
    /// table name, what a schema would add).
    pub notes: Vec<DisplayMessage>,
    /// Operator the finding is attached to, when it is about one.
    pub node_id: Option<i32>,
    /// Every operator involved, the primary one first.
    pub node_ids: Vec<i32>,
    /// Share of the statement this finding covers, when the plan carries
    /// timings or costs.
    pub impact: Option<FindingImpact>,
    /// Sort weight; higher ranks first.
    pub rank: f64,
}

impl PlanFinding {
    /// One line describing [`PlanFinding::impact`], or `None` when the plan
    /// gives no basis for one.
    pub fn impact_message(&self) -> Option<DisplayMessage> {
        let impact = self.impact?;
        let percent = ("percent", facts::pct(impact.share));
        // A wrong row estimate costs nothing where it is made: what it puts
        // at risk is the steps that were planned from it.
        let planned = self.kind == FindingKind::EstimateMismatch;
        Some(if impact.optimizer_estimate {
            tr("impact.optimizer", &[percent])
        } else {
            match (impact.basis, impact.time_ms, planned) {
                (ImpactBasis::Time, Some(time), false) => {
                    tr("impact.time", &[percent, ("time", facts::ms(time))])
                }
                (ImpactBasis::Time, Some(time), true) => {
                    tr("impact.planned_time", &[percent, ("time", facts::ms(time))])
                }
                (_, _, false) => tr("impact.cost", &[percent]),
                (_, _, true) => tr("impact.planned_cost", &[percent]),
            }
        })
    }
}

/// One row of the operators list: a compact human summary with the full
/// property set available on expansion.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorRow {
    pub node_id: i32,
    /// Physical operator name ("Index Seek").
    pub title: String,
    /// One-line human summary ("Index Seek — 4 rows estimated, 91% of cost").
    pub summary: DisplayMessage,
    /// Full technical detail for the expanded state.
    pub detail: DisplayMessage,
    pub cost_percent: f64,
    pub estimated_rows: f64,
    pub actual_rows: Option<f64>,
    pub has_warnings: bool,
}

/// Build the ranked findings list for one statement's plan.
pub fn findings_for_plan(plan: &ExecutionPlan) -> Vec<PlanFinding> {
    findings_for_plan_with_schema(plan, None)
}

/// Build the ranked findings list for one statement's plan, using the table
/// definitions in `schema` (when given) to check existing indexes, column
/// types, nullability and keys.
///
/// On MySQL, MariaDB and SQLite, where plans print query aliases, a name the
/// schema or the plan's own statement text settles is resolved first; see
/// [`findings_for_plan_with_context`].
pub fn findings_for_plan_with_schema(
    plan: &ExecutionPlan,
    schema: Option<&SchemaContext>,
) -> Vec<PlanFinding> {
    findings_for_plan_with_context(
        plan,
        &crate::plans::context::PlanContext {
            schema,
            query_text: None,
        },
    )
}

/// Build the ranked findings list for one statement's plan with everything
/// the caller knows about it: table definitions and the statement's SQL
/// text. Query aliases the plan prints in place of table names (MySQL,
/// MariaDB, SQLite) are resolved to real tables first, so index advice names
/// the table to create the index on.
///
/// A plan already passed through
/// [`resolve_plan_aliases`](crate::plans::context::resolve_plan_aliases) is
/// not resolved again.
pub fn findings_for_plan_with_context(
    plan: &ExecutionPlan,
    context: &crate::plans::context::PlanContext<'_>,
) -> Vec<PlanFinding> {
    if crate::plans::context::needs_resolution(plan) {
        let resolved = crate::plans::context::plan_with_context(plan, context);
        findings_with_schema(&resolved, context.schema)
    } else {
        findings_with_schema(plan, context.schema)
    }
}

fn findings_with_schema(plan: &ExecutionPlan, schema: Option<&SchemaContext>) -> Vec<PlanFinding> {
    if plan.nodes.is_empty() && plan.warnings.is_empty() {
        return Vec::new();
    }
    let ctx = Ctx::new(plan, schema.filter(|schema| !schema.is_empty()));
    let mut drafts: Vec<Draft> = Vec::new();
    rules_access::run(&ctx, &mut drafts);
    rules_predicate::run(&ctx, &mut drafts);
    rules_estimates::run(&ctx, &mut drafts);
    rules_memory::run(&ctx, &mut drafts);
    rules_join::run(&ctx, &mut drafts);
    rules_misc::run(&ctx, &mut drafts);
    rules_schema::run(&ctx, &mut drafts);
    rules_engines::run(&ctx, &mut drafts);
    suppress_overlaps(&mut drafts);
    resolve_index_advice(&ctx, &mut drafts);
    // A statement that finishes in under a second has nothing critical in
    // it, whatever share of that time one operator takes.
    if !ctx.long_running() {
        for draft in &mut drafts {
            draft.severity = draft.severity.min(FindingSeverity::Warning);
        }
    }
    let mut findings: Vec<PlanFinding> =
        drafts.into_iter().map(|draft| draft.finish(&ctx)).collect();
    findings.sort_by(|a, b| {
        b.rank
            .total_cmp(&a.rank)
            .then_with(|| a.node_id.cmp(&b.node_id))
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
    });
    findings
}

/// A finding under construction. Rules fill it in; [`Draft::finish`] turns
/// it into a [`PlanFinding`] once overlaps are resolved.
pub(crate) struct Draft {
    kind: FindingKind,
    severity: FindingSeverity,
    confidence: FindingConfidence,
    variant: &'static str,
    action_key: Option<String>,
    args: Args,
    sql: Option<String>,
    alternatives: Vec<FindingAlternative>,
    notes: Vec<DisplayMessage>,
    evidence: Vec<DisplayMessage>,
    /// Position of the primary node in `plan.nodes`.
    node: Option<usize>,
    nodes: Vec<usize>,
    /// Use the node's own share rather than its subtree's.
    own_share: bool,
    share: Option<FindingImpact>,
    /// Index this finding wants, resolved after all rules ran so
    /// overlapping suggestions on one table are merged.
    index: Option<IndexSuggestion>,
    /// Action key to use when the index suggestion cannot produce SQL.
    action_without_sql: Option<&'static str>,
    dropped: bool,
    /// The finding has no share of the statement's time or cost to claim.
    no_impact: bool,
}

impl Draft {
    /// `variant` selects the strings: summary, why and action come from
    /// `<kind>.<variant>.summary|why|action`, all rendered with `args`.
    pub(crate) fn new(
        kind: FindingKind,
        severity: FindingSeverity,
        confidence: FindingConfidence,
        variant: &'static str,
        args: Args,
    ) -> Self {
        Self {
            kind,
            severity,
            confidence,
            variant,
            action_key: None,
            args,
            sql: None,
            alternatives: Vec::new(),
            notes: Vec::new(),
            evidence: Vec::new(),
            node: None,
            nodes: Vec::new(),
            own_share: false,
            share: None,
            index: None,
            action_without_sql: None,
            dropped: false,
            no_impact: false,
        }
    }

    /// Attach to the node at `position` in `plan.nodes`.
    pub(crate) fn at(mut self, position: usize) -> Self {
        self.node = Some(position);
        if !self.nodes.contains(&position) {
            self.nodes.insert(0, position);
        }
        self
    }

    pub(crate) fn also(mut self, position: usize) -> Self {
        if !self.nodes.contains(&position) {
            self.nodes.push(position);
        }
        self
    }

    pub(crate) fn sql(mut self, sql: impl Into<String>) -> Self {
        self.sql = Some(sql.into());
        self
    }

    pub(crate) fn maybe_sql(mut self, sql: Option<String>) -> Self {
        self.sql = sql;
        self
    }

    /// Use `<kind>.<name>` for the action text instead of the variant's.
    pub(crate) fn action(mut self, name: &str) -> Self {
        self.action_key = Some(format!("{}.{name}", self.kind.as_str()));
        self
    }

    pub(crate) fn arg(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.args.retain(|arg| arg.0 != name);
        self.args.push((name, value.into()));
        self
    }

    /// An alternative fix; `name` is `<kind>.<name>` in the string table.
    pub(crate) fn alt(mut self, name: &str, sql: Option<String>) -> Self {
        let key = format!("{}.{name}", self.kind.as_str());
        self.alternatives.push(FindingAlternative {
            label: tr(&key, &self.args),
            sql,
        });
        self
    }

    pub(crate) fn note(mut self, short: &str, args: &[(&'static str, String)]) -> Self {
        self.notes.push(tr(short, args));
        self
    }

    /// One evidence line (`evidence.<name>`).
    pub(crate) fn ev(mut self, name: &str, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.trim().is_empty() {
            self.evidence
                .push(tr(&format!("evidence.{name}"), &[("value", value)]));
        }
        self
    }

    pub(crate) fn ev_opt(self, name: &str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.ev(name, value),
            None => self,
        }
    }

    pub(crate) fn own_share(mut self) -> Self {
        self.own_share = true;
        self
    }

    /// For findings whose cost falls outside this statement's run time.
    pub(crate) fn no_impact(mut self) -> Self {
        self.no_impact = true;
        self
    }

    pub(crate) fn impact(mut self, impact: FindingImpact) -> Self {
        self.share = Some(impact);
        self
    }

    /// The index this finding wants. `action_without_sql` is the action
    /// string (`<kind>.<name>`) used when no statement can be written.
    pub(crate) fn index(
        mut self,
        index: IndexSuggestion,
        action_without_sql: &'static str,
    ) -> Self {
        self.index = Some(index);
        self.action_without_sql = Some(action_without_sql);
        self
    }

    fn key(&self, part: &str) -> String {
        format!("{}.{}.{part}", self.kind.as_str(), self.variant)
    }

    fn finish(self, ctx: &Ctx<'_>) -> PlanFinding {
        let summary = tr(&self.key("summary"), &self.args);
        let why = tr(&self.key("why"), &self.args);
        let action_key = self
            .action_key
            .clone()
            .unwrap_or_else(|| self.key("action"));
        let action = tr(&action_key, &self.args);
        let impact = if self.no_impact { None } else { self.share }.or_else(|| {
            if self.no_impact {
                return None;
            }
            let node = self.node?;
            let (share, time_ms) = if self.own_share {
                (ctx.self_share(node)?, ctx.m(node).self_time_ms)
            } else {
                (ctx.share(node)?, ctx.m(node).inclusive_time_ms)
            };
            let basis = ctx.basis()?;
            Some(FindingImpact {
                share: share.clamp(0.0, 1.0),
                basis,
                time_ms: (basis == ImpactBasis::Time).then_some(time_ms).flatten(),
                optimizer_estimate: false,
            })
        });
        let evidence_text = self
            .evidence
            .iter()
            .map(DisplayMessage::fallback)
            .collect::<Vec<_>>()
            .join("\n");
        let detail = match self.node {
            Some(node) => tr(
                "common.detail",
                &[
                    ("operator", ctx.title(node)),
                    ("node_id", ctx.node(node).id.to_string()),
                    ("evidence", evidence_text),
                ],
            ),
            None => tr("common.detail_statement", &[("evidence", evidence_text)]),
        };
        // "About 0%" tells the reader nothing.
        let impact = impact.filter(|impact| impact.optimizer_estimate || impact.share >= 0.005);
        let rank = rank(self.kind, self.severity, self.confidence, impact);
        PlanFinding {
            kind: self.kind,
            category: self.kind.category(),
            severity: self.severity,
            confidence: self.confidence,
            summary,
            why,
            action,
            action_sql: self.sql,
            alternatives: self.alternatives,
            detail,
            evidence: self.evidence,
            notes: self.notes,
            node_id: self.node.map(|node| ctx.node(node).id),
            node_ids: self.nodes.iter().map(|node| ctx.node(*node).id).collect(),
            impact,
            rank,
        }
    }
}

/// Rank: the share of the statement the finding covers, weighted by how
/// sure the rule is and how directly the advice removes the cost.
fn rank(
    kind: FindingKind,
    severity: FindingSeverity,
    confidence: FindingConfidence,
    impact: Option<FindingImpact>,
) -> f64 {
    let share = match impact {
        Some(impact) => impact.share.max(0.01),
        // No timing or cost to go by: fall back on severity alone.
        None => match severity {
            FindingSeverity::Critical => 0.5,
            FindingSeverity::Warning => 0.2,
            FindingSeverity::Info => 0.03,
        },
    };
    let confidence = match confidence {
        FindingConfidence::High => 1.0,
        FindingConfidence::Medium => 0.8,
        FindingConfidence::Low => 0.5,
    };
    // How directly the suggested step removes the cost. A missing index or a
    // rewritten predicate removes the work; a statistics refresh or a setting
    // changes the plan and may or may not.
    let fixability = match kind {
        FindingKind::MissingIndex
        | FindingKind::TableScan
        | FindingKind::NonSargable
        | FindingKind::ImplicitConversion
        | FindingKind::NoJoin
        | FindingKind::AutoIndex
        | FindingKind::SchemaFkUnindexed => 1.0,
        FindingKind::FilterDiscardsRows
        | FindingKind::KeyLookup
        | FindingKind::NestedLoopVolume
        | FindingKind::DependentSubquery
        | FindingKind::ScalarUdf
        | FindingKind::Spill
        | FindingKind::HeapFetches
        | FindingKind::SchemaTypeMismatch
        | FindingKind::SchemaIndexNotUsed
        | FindingKind::CteMaterialization
        | FindingKind::PartitionPruning => 0.9,
        FindingKind::EstimateMismatch
        | FindingKind::StaleStatistics
        | FindingKind::ParameterSniffing
        | FindingKind::TableVariable
        | FindingKind::HashBuildSide
        | FindingKind::JoinFanout
        | FindingKind::ExpensiveSort
        | FindingKind::IndexMerge
        | FindingKind::JoinBuffer
        | FindingKind::LossyBitmap
        | FindingKind::SchemaNullableNotIn => 0.8,
        _ => 0.6,
    };
    let severity_weight = match severity {
        FindingSeverity::Critical => 1.15,
        FindingSeverity::Warning => 1.0,
        FindingSeverity::Info => 0.25,
    };
    100.0 * share * confidence * fixability * severity_weight
}

/// Removes findings that say the same thing about the same operator, keeping
/// the one with the more specific advice.
fn suppress_overlaps(drafts: &mut Vec<Draft>) {
    use FindingKind as K;
    // (kind to drop, kinds on the same node that make it redundant).
    const RULES: &[(K, &[K])] = &[
        (
            K::TableScan,
            &[
                K::MissingIndex,
                K::NonSargable,
                K::ImplicitConversion,
                K::NestedLoopVolume,
                K::SchemaIndexNotUsed,
                K::SchemaFkUnindexed,
            ],
        ),
        (
            K::FilterDiscardsRows,
            &[K::MissingIndex, K::NonSargable, K::ImplicitConversion],
        ),
        (
            K::EstimateMismatch,
            &[
                K::StaleStatistics,
                K::TableVariable,
                K::ParameterSniffing,
                K::Cardinality,
                K::RowGoal,
                K::NestedLoopVolume,
            ],
        ),
        (K::Spool, &[K::AutoIndex]),
        (
            K::SchemaIndexNotUsed,
            &[K::NonSargable, K::ImplicitConversion],
        ),
        (K::NestedLoopVolume, &[K::DependentSubquery, K::AutoIndex]),
    ];
    let mut drop = vec![false; drafts.len()];
    for (position, draft) in drafts.iter().enumerate() {
        let Some((_, by)) = RULES.iter().find(|(kind, _)| *kind == draft.kind) else {
            continue;
        };
        let Some(node) = draft.node else { continue };
        drop[position] = drafts.iter().any(|other| {
            by.contains(&other.kind)
                && other.severity >= FindingSeverity::Warning
                && (other.node == Some(node) || other.nodes.contains(&node))
        });
    }
    let mut position = 0;
    drafts.retain(|_| {
        let keep = !drop[position];
        position += 1;
        keep
    });
}

/// Turns index suggestions into SQL, one index per table where one index
/// serves several findings, and nothing where the schema shows the index
/// already exists.
fn resolve_index_advice(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    use FindingKind as K;
    let priority = |kind: K| match kind {
        K::MissingIndex => 0,
        K::SchemaFkUnindexed => 1,
        K::TableScan => 2,
        K::NonSargable => 3,
        K::FilterDiscardsRows => 4,
        K::NestedLoopVolume => 5,
        K::KeyLookup => 6,
        _ => 7,
    };
    let mut order: Vec<usize> = (0..drafts.len())
        .filter(|position| drafts[*position].index.is_some())
        .collect();
    order.sort_by_key(|position| (priority(drafts[*position].kind), *position));
    let advised: Vec<(IndexSuggestion, IndexAdvice)> = order
        .iter()
        .map(|position| {
            let suggestion = drafts[*position].index.clone().expect("filtered above");
            let advice = advice::index_advice(ctx, &suggestion);
            (suggestion, advice)
        })
        .collect();
    // A suggestion is served by another finding's index when that index has
    // this one's key as a prefix: the wider index is the one to create. Of
    // two identical suggestions the earlier one keeps the statement.
    let served_elsewhere = |rank: usize| {
        let (suggestion, IndexAdvice::Create(built)) = &advised[rank] else {
            return None;
        };
        let serving = advised
            .iter()
            .enumerate()
            .filter_map(|(other_rank, other)| {
                let (other_suggestion, IndexAdvice::Create(other_built)) = other else {
                    return None;
                };
                let serves = other_rank != rank
                    && other_suggestion.table_key() == suggestion.table_key()
                    && advice::covers(
                        &other_built.key,
                        &other_built.include,
                        &built.key,
                        &built.include,
                    )
                    && (other_rank < rank
                        || !advice::covers(
                            &built.key,
                            &built.include,
                            &other_built.key,
                            &other_built.include,
                        ));
                serves.then(|| other_built.key.clone())
            });
        // The widest serving index is the one that keeps its statement.
        serving.max_by_key(|key| key.len())
    };
    let covered_ranks: Vec<Option<Vec<String>>> =
        (0..advised.len()).map(served_elsewhere).collect();
    // Tables that already carry a statement from an earlier finding.
    let mut suggested: Vec<String> = Vec::new();
    for (rank, ((suggestion, advice), position)) in advised.into_iter().zip(order).enumerate() {
        let table_key = suggestion.table_key();
        let table_label = suggestion.label();
        let draft = &mut drafts[position];
        match advice {
            IndexAdvice::Create(built) => {
                let another = suggested.contains(&table_key);
                if let Some(serving_key) = &covered_ranks[rank] {
                    // The action still names the columns; the statement is
                    // printed once, on the finding that ranks first.
                    draft.args.retain(|arg| arg.0 != "index_columns");
                    draft.args.push(("index_columns", built.key.join(", ")));
                    draft.notes.push(tr(
                        "common.note.same_index",
                        &[
                            ("table", table_label),
                            ("index_columns", serving_key.join(", ")),
                        ],
                    ));
                    continue;
                }
                draft.sql = Some(built.sql.clone());
                draft.args.retain(|arg| arg.0 != "index_columns");
                draft.args.push(("index_columns", built.key.join(", ")));
                if let Some(existing) = &built.extends {
                    draft.notes.push(tr(
                        "common.note.extends_index",
                        &[("index", existing.clone())],
                    ));
                    if let Some(drop_sql) = &built.drop_sql {
                        draft.alternatives.push(FindingAlternative {
                            label: tr("common.alt.drop_index", &[]),
                            sql: Some(drop_sql.clone()),
                        });
                    }
                }
                if let Some(existing) = &built.rebuilds {
                    draft.notes.push(tr(
                        "common.note.rebuilds_index",
                        &[("index", existing.clone())],
                    ));
                }
                if suggestion.alias_only {
                    draft
                        .notes
                        .push(tr("common.note.alias", &[("table", table_label.clone())]));
                }
                if another {
                    draft.notes.push(tr(
                        "common.note.second_index",
                        &[("table", table_label.clone())],
                    ));
                }
                draft
                    .notes
                    .push(tr("common.note.write_cost", &[("table", table_label)]));
                if ctx.schema.is_none() && draft.kind != K::MissingIndex {
                    draft.notes.push(tr("common.note.needs_schema", &[]));
                }
                suggested.push(table_key);
            }
            IndexAdvice::Exists(index) => {
                // The schema already has the index this finding would ask
                // for. The index-specific findings have nothing left to say;
                // the "index exists but is not used" rule covers the rest.
                if matches!(draft.kind, K::MissingIndex | K::SchemaFkUnindexed) {
                    draft.dropped = true;
                } else {
                    draft.action_key = Some("common.action.index_exists".to_string());
                    draft
                        .args
                        .retain(|arg| arg.0 != "index" && arg.0 != "table");
                    draft.args.push(("index", index.clone()));
                    draft.args.push(("table", table_label));
                    draft
                        .evidence
                        .push(tr("evidence.existing_index", &[("value", index)]));
                }
            }
            IndexAdvice::None => {
                if let Some(name) = draft.action_without_sql {
                    draft.action_key = Some(format!("{}.{name}", draft.kind.as_str()));
                }
                if ctx.schema.is_none() {
                    draft.notes.push(tr("common.note.needs_schema", &[]));
                }
            }
        }
    }
    drafts.retain(|draft| !draft.dropped);
}

/// The operators list, most expensive first, with human one-liners.
pub fn operator_rows(plan: &ExecutionPlan) -> Vec<OperatorRow> {
    let mut rows: Vec<OperatorRow> = plan
        .nodes
        .iter()
        .map(|node| {
            let cost_share = cost_percent(plan, node);
            let title = display_op(node);
            let estimated_rows = human_count(node.estimated_rows);
            let cost = format!("{cost_share:.0}");
            let summary = if let Some(actual) = node.actual_rows {
                message(
                    "services.plans.operators.summary_actual",
                    format!(
                        "{} — {} rows estimated, {:.0}% of cost, read {}",
                        title,
                        estimated_rows,
                        cost_share,
                        human_count(actual),
                    ),
                )
                .with_arg("operator", title.clone())
                .with_arg("estimated_rows", estimated_rows)
                .with_arg("cost_percent", cost)
                .with_arg("actual_rows", human_count(actual))
            } else {
                message(
                    "services.plans.operators.summary_estimated",
                    format!(
                        "{} — {} rows estimated, {:.0}% of cost",
                        title, estimated_rows, cost_share,
                    ),
                )
                .with_arg("operator", title.clone())
                .with_arg("estimated_rows", estimated_rows)
                .with_arg("cost_percent", cost)
            };
            OperatorRow {
                node_id: node.id,
                title,
                summary,
                detail: operator_detail(plan, node),
                cost_percent: cost_share,
                estimated_rows: node.estimated_rows,
                actual_rows: node.actual_rows,
                has_warnings: !node.warnings.is_empty(),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.cost_percent
            .total_cmp(&a.cost_percent)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    rows
}

/// Human-readable row counts: exact with grouping below 10k ("8,210"),
/// compact three-significant-figure above ("48.2K", "1.2M", "3.4B").
/// Precise enough to compare, short enough to scan.
pub fn human_count(n: f64) -> String {
    crate::plans::phrases::styled(human_count_plain(n))
}

/// [`human_count`] in English form, for formatters that style the finished
/// text themselves.
pub(crate) fn human_count_plain(n: f64) -> String {
    if !n.is_finite() || n < 0.0 {
        return "0".to_string();
    }
    if n < 10_000.0 {
        return grouped(n.round() as u64);
    }
    let (scaled, suffix) = if n >= 1_000_000_000.0 {
        (n / 1_000_000_000.0, "B")
    } else if n >= 1_000_000.0 {
        (n / 1_000_000.0, "M")
    } else {
        (n / 1_000.0, "K")
    };
    if scaled >= 100.0 {
        format!("{scaled:.0}{suffix}")
    } else if scaled >= 10.0 {
        format!("{scaled:.1}{suffix}")
    } else {
        format!("{scaled:.2}{suffix}")
    }
}

/// A whole "how many times" number written out with digit grouping and
/// rounded to three significant figures: `37`, `1,250`, `10,000`,
/// `1,230,000`. A factor is read as a number of times, so it is never
/// shortened to `10.0K`. English form: callers style the finished text.
pub(crate) fn whole_factor_plain(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "0".to_string();
    }
    let value = value.round().min(1e18);
    let digits = value.log10().floor() as i32 + 1;
    let step = 10f64.powi((digits - 3).max(0));
    grouped(((value / step).round() * step) as u64)
}

fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn cost_percent(plan: &ExecutionPlan, node: &PlanNode) -> f64 {
    if plan.total_subtree_cost > 0.0 {
        (node.estimated_cost / plan.total_subtree_cost * 100.0).max(0.0)
    } else {
        0.0
    }
}

/// Physical op when present, else logical op.
fn display_op(node: &PlanNode) -> String {
    if node.physical_op.is_empty() {
        node.logical_op.clone()
    } else {
        node.physical_op.clone()
    }
}

fn operator_detail(plan: &ExecutionPlan, node: &PlanNode) -> DisplayMessage {
    let cost_share = cost_percent(plan, node);
    let actual = node
        .actual_rows
        .map(human_count)
        .unwrap_or_else(|| "—".into());
    let warnings = if node.warnings.is_empty() {
        "—".into()
    } else {
        format!("{:?}", node.warnings)
    };
    let output = if node.output_list.is_empty() {
        "—".to_string()
    } else {
        node.output_list
            .iter()
            .map(|column| format!("- {column}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let fallback = format!(
        "Node {}\nPhysical op: {}\nLogical op: {}\nEstimated cost: {:.4} ({:.0}% of statement)\nEstimated rows: {}\nActual rows: {}\nEstimated CPU: {:.4}\nEstimated I/O: {:.4}\nWarnings: {}\nOutput\n{}",
        node.id,
        display_op(node),
        node.logical_op,
        node.estimated_cost,
        cost_share,
        human_count(node.estimated_rows),
        actual,
        node.estimated_cpu,
        node.estimated_io,
        warnings,
        output,
    );
    message("services.plans.operators.detail", fallback)
        .with_arg("node_id", node.id.to_string())
        .with_arg("physical_operator", display_op(node))
        .with_arg("logical_operator", node.logical_op.clone())
        .with_arg("estimated_cost", format!("{:.4}", node.estimated_cost))
        .with_arg("cost_percent", format!("{cost_share:.0}"))
        .with_arg("estimated_rows", human_count(node.estimated_rows))
        .with_arg("actual_rows", actual)
        .with_arg("estimated_cpu", format!("{:.4}", node.estimated_cpu))
        .with_arg("estimated_io", format!("{:.4}", node.estimated_io))
        .with_arg("warnings", warnings)
        .with_arg("output_list", output)
}
