//! The AI analysis cell (`kind: "ai"`, spec 4.3, 9): the prompt is the
//! cell's `source`; the spec holds the mode, the budget, the last run's
//! plan, findings and proposals, and the cells the agent created.
//!
//! ```json
//! {"mode": "investigate", "target": "c_orders",
//!  "budget": {"max_queries": 8, "max_rows": 5000, "max_runtime_ms": 60000,
//!             "max_tokens": 40000, "max_turns": 12},
//!  "result": {"run_id": "…", "status": "completed", "findings": […], …},
//!  "created_cells": ["c_…"]}
//! ```
//!
//! Generated cells carry `"generated_by": {"agent": "sqlly", "ai_cell": …}`
//! as an unknown top-level cell field ([`mark_generated`]), so they are
//! visibly attributed and stay ordinary, editable cells.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlly_core::StoredMessage;

use crate::notebook::{Cell, CellKind};

/// Prompt-template version recorded with every run and log entry.
pub const TEMPLATE_VERSION: &str = "notebook-agent/1";

/// The cell field that attributes a cell to the agent.
pub const GENERATED_BY_FIELD: &str = "generated_by";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Explain an error, complete SQL, propose a visualization, interpret a plan.
    #[default]
    Assist,
    /// An investigation outline to review; nothing runs.
    Plan,
    /// Proposed cells and edits, each accepted or rejected by the user.
    Build,
    /// Bounded read-only queries, then findings with evidence.
    Investigate,
    /// Deterministic checks first, then the model.
    Review,
    /// What changed between two runs, and why.
    ExplainChanges,
}

impl AgentMode {
    pub const ALL: [AgentMode; 6] = [
        AgentMode::Assist,
        AgentMode::Plan,
        AgentMode::Build,
        AgentMode::Investigate,
        AgentMode::Review,
        AgentMode::ExplainChanges,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentMode::Assist => "assist",
            AgentMode::Plan => "plan",
            AgentMode::Build => "build",
            AgentMode::Investigate => "investigate",
            AgentMode::Review => "review",
            AgentMode::ExplainChanges => "explain_changes",
        }
    }

    /// Whether the mode may run queries at all.
    pub fn executes(self) -> bool {
        matches!(
            self,
            AgentMode::Assist | AgentMode::Investigate | AgentMode::Review
        )
    }
}

/// Caps enforced in Rust for one run (spec 9.2 "explicit caps").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    pub max_queries: u32,
    pub max_rows: u64,
    /// Wall-clock time spent in queries, summed.
    pub max_runtime_ms: u64,
    /// Estimated prompt plus reply tokens over the whole run.
    pub max_tokens: u64,
    pub max_turns: u32,
    /// Only meaningful for cloud providers with a known price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_queries: 8,
            max_rows: 5_000,
            max_runtime_ms: 60_000,
            max_tokens: 40_000,
            max_turns: 12,
            max_cost_usd: None,
        }
    }
}

impl Budget {
    /// Defaults per mode: modes that never execute get no query allowance.
    pub fn for_mode(mode: AgentMode) -> Budget {
        let base = Budget::default();
        match mode {
            AgentMode::Plan | AgentMode::Build | AgentMode::ExplainChanges => Budget {
                max_queries: 0,
                max_rows: 200,
                max_runtime_ms: 0,
                max_turns: 8,
                ..base
            },
            AgentMode::Assist => Budget {
                max_queries: 3,
                max_rows: 1_000,
                max_runtime_ms: 30_000,
                max_turns: 8,
                ..base
            },
            AgentMode::Review => Budget {
                max_queries: 4,
                max_rows: 2_000,
                ..base
            },
            AgentMode::Investigate => base,
        }
    }

    /// Caps tightened into a sane range, so a hand-edited file cannot ask
    /// for an unbounded run.
    pub fn clamped(&self) -> Budget {
        Budget {
            max_queries: self.max_queries.min(50),
            max_rows: self.max_rows.min(100_000),
            max_runtime_ms: self.max_runtime_ms.min(15 * 60_000),
            max_tokens: self.max_tokens.clamp(1_000, 400_000),
            max_turns: self.max_turns.clamp(1, 40),
            max_cost_usd: self.max_cost_usd.map(|c| c.clamp(0.0, 100.0)),
        }
    }
}

/// What a run used, shown against [`Budget`] in the cell.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetUsage {
    pub queries: u32,
    pub rows: u64,
    pub runtime_ms: u64,
    pub tokens: u64,
    pub turns: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Which cap ended or limited a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    Queries,
    Rows,
    Runtime,
    Tokens,
    Turns,
    Cost,
}

impl BudgetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetKind::Queries => "queries",
            BudgetKind::Rows => "rows",
            BudgetKind::Runtime => "runtime",
            BudgetKind::Tokens => "tokens",
            BudgetKind::Turns => "turns",
            BudgetKind::Cost => "cost",
        }
    }
}

/// The four evidence classes of spec 9.2, matching the report callouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    ObservedFact,
    DerivedMetric,
    Hypothesis,
    Recommendation,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::ObservedFact => "observed_fact",
            FindingKind::DerivedMetric => "derived_metric",
            FindingKind::Hypothesis => "hypothesis",
            FindingKind::Recommendation => "recommendation",
        }
    }

    pub fn parse(text: &str) -> Option<FindingKind> {
        let normalized = text.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        match normalized.as_str() {
            "observed_fact" | "fact" | "observation" => Some(FindingKind::ObservedFact),
            "derived_metric" | "derived" | "metric" => Some(FindingKind::DerivedMetric),
            "hypothesis" => Some(FindingKind::Hypothesis),
            "recommendation" | "suggestion" => Some(FindingKind::Recommendation),
            _ => None,
        }
    }

    /// Facts and derived metrics state something about the data; they need
    /// executed evidence to be shown as verified.
    pub fn is_claim(self) -> bool {
        matches!(self, FindingKind::ObservedFact | FindingKind::DerivedMetric)
    }
}

/// A link from a finding to the cell, run and (where known) the row group
/// or aggregate it rests on.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Evidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// An agent query recorded in [`AiResult::queries`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The row group or aggregate, e.g. `region = EU` or `SUM(amount)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// SHA-256 of the rows the evidence read, filled in by the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_hash: Option<String>,
}

/// Why a finding is not shown as verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnverifiedReason {
    /// Nothing ran: the run was a plan or a proposal.
    NotExecuted,
    /// No evidence link was given.
    NoEvidence,
    /// A link names a cell, run or query that does not exist or was not read.
    UnknownEvidence,
    /// The quoted value does not appear in the evidence it cites.
    ValueNotInEvidence,
    /// The evidence covers only part of the data (sampled, truncated, partial).
    IncompleteEvidence,
}

impl UnverifiedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            UnverifiedReason::NotExecuted => "not_executed",
            UnverifiedReason::NoEvidence => "no_evidence",
            UnverifiedReason::UnknownEvidence => "unknown_evidence",
            UnverifiedReason::ValueNotInEvidence => "value_not_in_evidence",
            UnverifiedReason::IncompleteEvidence => "incomplete_evidence",
        }
    }
}

/// The deterministic Review checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCheck {
    FanoutJoin,
    AmbiguousAggregate,
    StaleResult,
    MissingFilter,
    SampledData,
    Timezone,
    Security,
    AmbiguousTerm,
    ChangeExplained,
}

impl ReviewCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewCheck::FanoutJoin => "fanout_join",
            ReviewCheck::AmbiguousAggregate => "ambiguous_aggregate",
            ReviewCheck::StaleResult => "stale_result",
            ReviewCheck::MissingFilter => "missing_filter",
            ReviewCheck::SampledData => "sampled_data",
            ReviewCheck::Timezone => "timezone",
            ReviewCheck::Security => "security",
            ReviewCheck::AmbiguousTerm => "ambiguous_term",
            ReviewCheck::ChangeExplained => "change_explained",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingOrigin {
    #[default]
    Model,
    /// Computed by SQLLY's own checks, not the model.
    Deterministic,
}

/// One labelled conclusion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub kind: FindingKind,
    /// The model's words, or the English text of a deterministic finding.
    pub text: String,
    /// The catalog message behind a deterministic finding, localized at render time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StoredMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified_reason: Option<UnverifiedReason>,
    /// A named quantity the finding states, for contradiction checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Findings stating a different value for the same metric.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts_with: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<ReviewCheck>,
    #[serde(default)]
    pub origin: FindingOrigin,
}

impl Finding {
    pub fn new(id: impl Into<String>, kind: FindingKind, text: impl Into<String>) -> Finding {
        Finding {
            id: id.into(),
            kind,
            text: text.into(),
            message: None,
            evidence: vec![],
            verified: false,
            unverified_reason: None,
            metric: None,
            value: None,
            conflicts_with: vec![],
            check: None,
            origin: FindingOrigin::Model,
        }
    }

    /// The cells the finding's evidence points at, in order, without repeats.
    pub fn evidence_cells(&self) -> Vec<&str> {
        let mut out: Vec<&str> = vec![];
        for cell in self.evidence.iter().filter_map(|e| e.cell.as_deref()) {
            if !out.contains(&cell) {
                out.push(cell);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanStep {
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Cells the step reads or would create.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<String>,
}

/// A Plan-mode outline the user reviews before anything runs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InvestigationPlan {
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub sources: Vec<String>,
    pub assumptions: Vec<String>,
    /// Cost and safety concerns.
    pub concerns: Vec<String>,
    /// Permissions the plan needs (connections, row access).
    pub permissions: Vec<String>,
    pub approved: bool,
}

/// A cell the agent proposes to insert.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposedCell {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub spec: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
}

/// One line of a proposed edit's diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "text", rename_all = "snake_case")]
pub enum DiffLine {
    Same(String),
    Removed(String),
    Added(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ProposalAction {
    InsertCells {
        /// Insert after this cell; `None` is right after the AI cell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        cells: Vec<ProposedCell>,
    },
    EditCell {
        cell_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spec: Option<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        diff: Vec<DiffLine>,
    },
    /// Read-only cells to run; the user's approval starts them.
    RunCells { cells: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    #[default]
    Pending,
    Accepted,
    Rejected,
    /// Applied to the notebook (as part of one undo group).
    Applied,
    /// Refused by SQLLY's policy before the user saw it as actionable.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    #[serde(flatten)]
    pub action: ProposalAction,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
    #[serde(default)]
    pub status: ProposalStatus,
    /// Why it was refused, as a catalog message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<StoredMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_text: Option<String>,
}

impl Proposal {
    pub fn is_actionable(&self) -> bool {
        matches!(
            self.status,
            ProposalStatus::Pending | ProposalStatus::Accepted
        )
    }
}

/// A query the agent ran, kept so findings can cite it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryRecord {
    /// `q1`, `q2`, … within the run.
    pub id: String,
    /// The connection slot it ran on.
    pub connection: String,
    pub sql: String,
    pub sql_hash: String,
    pub rows: u64,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What an output looked like when the agent read it, the baseline for a
/// later Explain changes run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservedOutput {
    pub run_id: String,
    pub fingerprint: String,
    /// The fingerprint's parts, so a later run can say which one moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts: Option<crate::notebook::fingerprint::FingerprintParts>,
    pub row_count: u64,
    pub columns: Vec<String>,
    /// `rows-sha256:` checksum of the snapshot rows.
    pub checksum: String,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Completed,
    /// A plan waits for the user's approval.
    AwaitingApproval,
    /// The agent asked a question instead of guessing.
    NeedsClarification,
    /// A cap, the turn limit or the user stopped the run.
    Stopped,
    Failed,
    /// SQLLY's policy refused the run (consent, trust).
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelLocationTag {
    Local,
    Cloud,
}

/// The outcome of one run, stored in the cell.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AiResult {
    pub run_id: String,
    pub mode: AgentMode,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<BudgetKind>,
    /// Provider error text or another stop detail (English, from the provider).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_location: Option<ModelLocationTag>,
    pub template_version: String,
    /// The model's prose summary.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<InvestigationPlan>,
    pub findings: Vec<Finding>,
    pub proposals: Vec<Proposal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub clarifications: Vec<String>,
    pub queries: Vec<QueryRecord>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub observed: BTreeMap<String, ObservedOutput>,
    pub budget: Budget,
    pub usage: BudgetUsage,
    /// Tools the model was offered.
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

impl AiResult {
    pub fn proposal(&self, id: &str) -> Option<&Proposal> {
        self.proposals.iter().find(|p| p.id == id)
    }

    pub fn proposal_mut(&mut self, id: &str) -> Option<&mut Proposal> {
        self.proposals.iter_mut().find(|p| p.id == id)
    }

    /// Whether any query or output read happened, i.e. findings can be verified.
    pub fn executed(&self) -> bool {
        self.queries.iter().any(|q| q.error.is_none()) || !self.observed.is_empty()
    }
}

/// Which two runs an Explain changes run compares.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompareRef {
    /// A run id; empty means "the last time this AI cell ran".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub before: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub after: String,
}

/// The typed `spec` of an AI cell.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AiCellSpec {
    pub mode: AgentMode,
    /// The cell the request is about (an error to explain, a query to review).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<CompareRef>,
    pub budget: Budget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<AiResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub created_cells: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl AiCellSpec {
    pub fn new(mode: AgentMode) -> AiCellSpec {
        AiCellSpec {
            mode,
            budget: Budget::for_mode(mode),
            ..AiCellSpec::default()
        }
    }

    /// The spec of `cell`, or a default one when the cell is not an AI cell
    /// or its spec does not parse (unknown fields are kept in `extra`).
    pub fn of(cell: &Cell) -> AiCellSpec {
        if cell.kind != CellKind::Ai || cell.spec.is_null() {
            return AiCellSpec::default();
        }
        serde_json::from_value(cell.spec.clone()).unwrap_or_default()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn write_to(&self, cell: &mut Cell) {
        cell.spec = self.to_value();
    }
}

/// Who generated a cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    /// Always `sqlly` today; kept for other agents later.
    pub agent: String,
    pub ai_cell: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proposal: String,
}

/// Attribute `cell` to the agent run that proposed it.
pub fn mark_generated(cell: &mut Cell, ai_cell: &str, run_id: &str, proposal: &str) {
    let attribution = Attribution {
        agent: "sqlly".into(),
        ai_cell: ai_cell.into(),
        run_id: run_id.into(),
        proposal: proposal.into(),
    };
    if let Ok(value) = serde_json::to_value(attribution) {
        cell.extra.insert(GENERATED_BY_FIELD.into(), value);
    }
}

/// The agent attribution of `cell`, when it was generated.
pub fn generated_by(cell: &Cell) -> Option<Attribution> {
    cell.extra
        .get(GENERATED_BY_FIELD)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Report blocks for an AI cell's findings: one callout per finding, with
/// the same four kinds the report renders, so exports keep the labels.
pub fn report_callouts(cell: &Cell) -> Vec<crate::report::doc::Callout> {
    use crate::report::doc::{Callout, CalloutKind};
    let spec = AiCellSpec::of(cell);
    let Some(result) = spec.result else {
        return vec![];
    };
    result
        .findings
        .iter()
        .map(|finding| {
            let kind = match finding.kind {
                FindingKind::ObservedFact => CalloutKind::Fact,
                FindingKind::DerivedMetric => CalloutKind::Derived,
                FindingKind::Hypothesis => CalloutKind::Hypothesis,
                FindingKind::Recommendation => CalloutKind::Recommendation,
            };
            let mut evidence: Vec<String> = vec![];
            for e in &finding.evidence {
                for id in [&e.run, &e.cell, &e.query].into_iter().flatten() {
                    if !evidence.contains(id) {
                        evidence.push(id.clone());
                    }
                }
            }
            Callout {
                kind,
                title: (!finding.verified).then(|| "Unverified".to_string()),
                text: finding.text.clone(),
                evidence,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::{ExecutionPolicy, NotebookDoc, SeededIds};
    use serde_json::json;

    #[test]
    fn spec_round_trips_and_keeps_unknown_fields() {
        let value = json!({
            "mode": "investigate",
            "target": "c_orders",
            "budget": {"max_queries": 3},
            "future_field": {"x": 1},
            "result": {
                "run_id": "r1", "status": "awaiting_approval", "provider": "ollama:llama3",
                "template_version": TEMPLATE_VERSION,
                "findings": [{"id": "f1", "kind": "observed_fact", "text": "t",
                              "evidence": [{"cell": "c1", "run": "r0"}], "verified": true}],
                "proposals": [{"id": "p1", "action": "run_cells", "cells": ["c1"]},
                              {"id": "p2", "action": "insert_cells",
                               "cells": [{"kind": "sql", "source": "SELECT 1"}]}]
            }
        });
        let spec: AiCellSpec = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(spec.mode, AgentMode::Investigate);
        assert_eq!(spec.budget.max_queries, 3);
        assert_eq!(spec.budget.max_rows, Budget::default().max_rows);
        let result = spec.result.as_ref().unwrap();
        assert_eq!(result.status, RunStatus::AwaitingApproval);
        assert_eq!(result.findings[0].evidence_cells(), vec!["c1"]);
        assert!(matches!(
            result.proposals[0].action,
            ProposalAction::RunCells { .. }
        ));
        assert_eq!(result.proposals[1].status, ProposalStatus::Pending);
        let back = spec.to_value();
        assert_eq!(back["future_field"], json!({"x": 1}));
        let again: AiCellSpec = serde_json::from_value(back).unwrap();
        assert_eq!(again, spec);
    }

    #[test]
    fn budgets_per_mode_and_clamping() {
        assert_eq!(Budget::for_mode(AgentMode::Plan).max_queries, 0);
        assert_eq!(Budget::for_mode(AgentMode::Build).max_runtime_ms, 0);
        assert!(Budget::for_mode(AgentMode::Investigate).max_queries > 0);
        let wild = Budget {
            max_queries: 10_000,
            max_rows: u64::MAX,
            max_runtime_ms: u64::MAX,
            max_tokens: 0,
            max_turns: 0,
            max_cost_usd: Some(1e9),
        }
        .clamped();
        assert_eq!(wild.max_queries, 50);
        assert_eq!(wild.max_turns, 1);
        assert_eq!(wild.max_tokens, 1_000);
        assert_eq!(wild.max_cost_usd, Some(100.0));
        for mode in AgentMode::ALL {
            let text = serde_json::to_string(&mode).unwrap();
            assert_eq!(text.trim_matches('"'), mode.as_str());
        }
    }

    #[test]
    fn generated_cells_are_attributed_and_survive_canonical_json() {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(1));
        let mut cell = Cell::new("c_new", CellKind::Sql, ExecutionPolicy::default());
        mark_generated(&mut cell, "c_ai", "run1", "p1");
        doc.cells.push(cell);
        let text = doc.to_canonical_json();
        assert!(text.contains("\"generated_by\""));
        let back: NotebookDoc = serde_json::from_str(&text).unwrap();
        let attribution = generated_by(back.cell("c_new").unwrap()).unwrap();
        assert_eq!(attribution.ai_cell, "c_ai");
        assert_eq!(attribution.proposal, "p1");
        assert!(generated_by(&back.cells[0]).is_none());
    }

    #[test]
    fn finding_kinds_map_onto_report_callouts_with_unverified_titles() {
        let mut cell = Cell::new("c_ai", CellKind::Ai, ExecutionPolicy::default());
        let mut spec = AiCellSpec::new(AgentMode::Investigate);
        let mut fact = Finding::new("f1", FindingKind::ObservedFact, "EU refunds doubled");
        fact.verified = true;
        fact.evidence.push(Evidence {
            cell: Some("c_refunds".into()),
            run: Some("r9".into()),
            ..Evidence::default()
        });
        let guess = Finding::new("f2", FindingKind::Hypothesis, "a payment provider change");
        spec.result = Some(AiResult {
            findings: vec![fact, guess],
            ..AiResult::default()
        });
        spec.write_to(&mut cell);
        let callouts = report_callouts(&cell);
        assert_eq!(callouts.len(), 2);
        assert_eq!(callouts[0].kind, crate::report::doc::CalloutKind::Fact);
        assert_eq!(callouts[0].evidence, vec!["r9", "c_refunds"]);
        assert!(callouts[0].title.is_none());
        assert_eq!(callouts[1].title.as_deref(), Some("Unverified"));
        assert_eq!(
            FindingKind::parse("Derived metric"),
            Some(FindingKind::DerivedMetric)
        );
        assert_eq!(FindingKind::parse("nonsense"), None);
    }
}
