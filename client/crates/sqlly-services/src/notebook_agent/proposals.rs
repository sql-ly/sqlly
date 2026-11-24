//! Proposals: the only way the agent changes a notebook, and only after
//! the user accepts each one. Every proposal is checked here when it is
//! made and again when it is applied; a proposal that would write data,
//! touch a cell that may write, or run anything but read-only cells is
//! refused with a label the user sees.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlly_core::StoredMessage;

use super::context::{NotebookContext, writes};
use crate::notebook::cells::ai::{
    AiCellSpec, AiResult, DiffLine, Proposal, ProposalAction, ProposalStatus, ProposedCell,
    mark_generated,
};
use crate::notebook::references::validate_cell_name;
use crate::notebook::{Cell, CellKind, ExecClass, IdSource, NotebookDoc};

use super::tools::PROPOSABLE_KINDS;

/// Why SQLLY refused a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The SQL changes data; the agent can only read.
    WriteStatement {
        cell: Option<String>,
    },
    /// The cell may write (writes were allowed on it); the agent cannot edit or run it.
    WriteCell {
        cell: String,
    },
    UnknownCell {
        cell: String,
    },
    UnknownConnection {
        connection: String,
    },
    UnsupportedKind {
        kind: String,
    },
    /// The agent tried to change its own AI cell.
    SelfEdit,
    /// Only SQL and local cells can be run, never another AI cell.
    NotRunnable {
        cell: String,
    },
    InvalidSpec,
    /// The settings do not read as a cell of that kind.
    InvalidKindSpec {
        kind: String,
    },
    /// The settings would change where the cell runs (local data, another
    /// connection); only the user does that.
    ChangesWhereItRuns {
        cell: String,
    },
}

impl Refusal {
    pub fn refusal_key(&self) -> &'static str {
        match self {
            Refusal::WriteStatement { cell: Some(_) } => "notebook.ai.refusal.write_statement",
            Refusal::WriteStatement { cell: None } => "notebook.ai.refusal.write_statement_sql",
            Refusal::WriteCell { .. } => "notebook.ai.refusal.write_cell",
            Refusal::UnknownCell { .. } => "notebook.ai.refusal.unknown_cell",
            Refusal::UnknownConnection { .. } => "notebook.ai.refusal.unknown_connection",
            Refusal::UnsupportedKind { .. } => "notebook.ai.refusal.unsupported_kind",
            Refusal::SelfEdit => "notebook.ai.refusal.self_edit",
            Refusal::NotRunnable { .. } => "notebook.ai.refusal.not_runnable",
            Refusal::InvalidSpec => "notebook.ai.refusal.invalid_spec",
            Refusal::InvalidKindSpec { .. } => "notebook.ai.refusal.invalid_kind_spec",
            Refusal::ChangesWhereItRuns { .. } => "notebook.ai.refusal.changes_where_it_runs",
        }
    }

    fn arguments(&self) -> Vec<(String, String)> {
        match self {
            Refusal::WriteStatement { cell: Some(cell) }
            | Refusal::WriteCell { cell }
            | Refusal::UnknownCell { cell }
            | Refusal::NotRunnable { cell }
            | Refusal::ChangesWhereItRuns { cell } => vec![("cell".into(), cell.clone())],
            Refusal::UnknownConnection { connection } => {
                vec![("connection".into(), connection.clone())]
            }
            Refusal::UnsupportedKind { kind } | Refusal::InvalidKindSpec { kind } => {
                vec![("kind".into(), kind.clone())]
            }
            Refusal::WriteStatement { cell: None } | Refusal::SelfEdit | Refusal::InvalidSpec => {
                vec![]
            }
        }
    }

    /// English text, for the model and as the stored fallback.
    pub fn english(&self) -> String {
        match self {
            Refusal::WriteStatement { cell: Some(cell) } => format!(
                "Refused: the SQL for {cell} changes data. The notebook agent can only read."
            ),
            Refusal::WriteStatement { cell: None } => {
                "Refused: this SQL changes data. The notebook agent can only read.".into()
            }
            Refusal::WriteCell { cell } => format!(
                "Refused: {cell} is allowed to change data, so the agent cannot edit or run it."
            ),
            Refusal::UnknownCell { cell } => format!("Refused: there is no cell {cell}."),
            Refusal::UnknownConnection { connection } => {
                format!("Refused: {connection} is not a connection this notebook may use.")
            }
            Refusal::UnsupportedKind { kind } => {
                format!("Refused: the agent cannot create {kind} cells.")
            }
            Refusal::SelfEdit => "Refused: the agent cannot change its own AI cell.".into(),
            Refusal::NotRunnable { cell } => format!(
                "Refused: {cell} is not a read-only query or local cell, so it cannot be run for the agent."
            ),
            Refusal::InvalidSpec => "Refused: the cell settings are not a JSON object.".into(),
            Refusal::InvalidKindSpec { kind } => {
                format!("Refused: the settings do not describe a valid {kind} cell.")
            }
            Refusal::ChangesWhereItRuns { cell } => format!(
                "Refused: the new settings would change where {cell} runs. Only you can do that."
            ),
        }
    }

    pub fn stored(&self) -> StoredMessage {
        StoredMessage {
            key: self.refusal_key().to_string(),
            arguments: self.arguments(),
        }
    }
}

fn check_sql(
    ctx: &NotebookContext,
    slot: &str,
    sql: &str,
    cell: Option<&str>,
) -> Result<(), Refusal> {
    if !sql.trim().is_empty() && writes(ctx.engine_of(slot), sql) {
        return Err(Refusal::WriteStatement {
            cell: cell.map(str::to_string),
        });
    }
    Ok(())
}

/// Check proposed cells before they are shown as actionable.
pub fn check_insert(
    ctx: &NotebookContext,
    after: Option<&str>,
    cells: &[ProposedCell],
) -> Result<(), Refusal> {
    normalize_insert(ctx, after, cells).map(|_| ())
}

fn strict<T: serde::de::DeserializeOwned + serde::Serialize>(
    kind: &str,
    spec: &Value,
) -> Result<Value, Refusal> {
    serde_json::from_value::<T>(spec.clone())
        .map(|typed| crate::notebook::cells::spec::write(&typed))
        .map_err(|_| Refusal::InvalidKindSpec {
            kind: kind.to_string(),
        })
}

/// A sweep as written, read strictly (the plan spec drops one that does
/// not read).
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct SweepSpecProbe {
    parameter: String,
    #[serde(default)]
    values: Vec<Value>,
    #[serde(default)]
    repetitions: Option<u32>,
}

/// A proposed cell's spec in the exact shape its kind's owner reads, or
/// why it cannot be one. Charts also accept the short form
/// `{"input": …, "kind": "bar", "x": "col", "y": "col"}`.
pub fn normalize_spec(kind: &str, spec: &Value) -> Result<Value, Refusal> {
    use crate::notebook::cells::{
        assertion::AssertionCellSpec, chart::ChartCellSpec, diff::DiffCellSpec, input_of,
        kpi::KpiCellSpec, pivot::PivotCellSpec, profile::ProfileCellSpec, scalar::ScalarCellSpec,
        table::TableCellSpec, transform::TransformCellSpec,
    };
    if !(spec.is_null() || spec.is_object()) {
        return Err(Refusal::InvalidSpec);
    }
    let invalid = || Refusal::InvalidKindSpec {
        kind: kind.to_string(),
    };
    let needs_input = matches!(
        kind,
        "chart" | "table" | "pivot" | "kpi" | "profile" | "assertion" | "plan"
    );
    if needs_input && input_of(spec).is_empty() {
        return Err(invalid());
    }
    match kind {
        "chart" if spec.get("chart").is_some() => strict::<ChartCellSpec>(kind, spec),
        "chart" => {
            let mut probe = Cell::new("probe", CellKind::Chart, Default::default());
            probe.spec = spec.clone();
            let chart = crate::report::from_notebook::chart_spec(&probe).ok_or_else(invalid)?;
            Ok(crate::notebook::cells::spec::write(&ChartCellSpec {
                input: Some(input_of(spec)),
                chart,
                ..ChartCellSpec::default()
            }))
        }
        "table" => strict::<TableCellSpec>(kind, spec),
        "pivot" => strict::<PivotCellSpec>(kind, spec),
        "kpi" => strict::<KpiCellSpec>(kind, spec),
        "scalar" => strict::<ScalarCellSpec>(kind, spec),
        "transform" => strict::<TransformCellSpec>(kind, spec),
        "assertion" => {
            let typed = AssertionCellSpec::from_spec(spec);
            if typed.assertions.is_empty() || !typed.invalid.is_empty() {
                return Err(invalid());
            }
            Ok(typed.to_spec())
        }
        "profile" => {
            let _ = ProfileCellSpec::from_spec(spec);
            Ok(spec.clone())
        }
        "diff" => {
            let _ = DiffCellSpec::from_spec(spec);
            Ok(spec.clone())
        }
        "plan" => {
            use crate::notebook::cells::plan::{MAX_SWEEP_RUNS, PlanCellSpec, PlanMode};
            if spec
                .get("mode")
                .is_some_and(|m| serde_json::from_value::<PlanMode>(m.clone()).is_err())
                || spec
                    .get("sweep")
                    .is_some_and(|s| serde_json::from_value::<SweepSpecProbe>(s.clone()).is_err())
            {
                return Err(invalid());
            }
            let typed = PlanCellSpec {
                extra: Default::default(),
                ..PlanCellSpec::from_spec(spec)
            };
            if let Some(sweep) = &typed.sweep
                && (sweep.parameter.trim().is_empty()
                    || sweep.values.is_empty()
                    || sweep.total_runs() > MAX_SWEEP_RUNS)
            {
                return Err(invalid());
            }
            Ok(typed.to_spec())
        }
        "entity_detail" => {
            use crate::notebook::cells::entity_detail::{
                EntityDetailSpec, EntitySource, MAX_LIMIT, MAX_RELATIONSHIPS,
            };
            let mut typed = EntityDetailSpec {
                extra: Default::default(),
                ..EntityDetailSpec::from_spec(spec)
            };
            let source_ok = match &typed.source {
                EntitySource::Row { cell, .. } => !cell.trim().is_empty(),
                EntitySource::Parameter { name, column } => {
                    !name.trim().is_empty() && !column.trim().is_empty()
                }
            };
            let listed = spec
                .get("relationships")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if !source_ok
                || typed.table.name.trim().is_empty()
                || typed.relationships.len() != listed
                || typed.relationships.len() > MAX_RELATIONSHIPS
                || typed.relationships.iter().any(|r| {
                    r.table.trim().is_empty()
                        || r.columns.is_empty()
                        || r.columns
                            .iter()
                            .any(|(a, b)| a.trim().is_empty() || b.trim().is_empty())
                })
            {
                return Err(invalid());
            }
            typed.limit = typed.limit.map(|n| n.clamp(1, MAX_LIMIT));
            Ok(typed.to_spec())
        }
        _ => Ok(spec.clone()),
    }
}

/// Check proposed cells and put each spec in its kind's exact shape.
pub fn normalize_insert(
    ctx: &NotebookContext,
    after: Option<&str>,
    cells: &[ProposedCell],
) -> Result<Vec<ProposedCell>, Refusal> {
    if let Some(after) = after
        && ctx.resolve_cell(after).is_none()
    {
        return Err(Refusal::UnknownCell {
            cell: after.to_string(),
        });
    }
    let mut out = Vec::with_capacity(cells.len());
    for cell in cells {
        if !PROPOSABLE_KINDS.contains(&cell.kind.as_str()) {
            return Err(Refusal::UnsupportedKind {
                kind: cell.kind.clone(),
            });
        }
        let spec = normalize_spec(&cell.kind, &cell.spec)?;
        if cell.kind == "sql" {
            let slot = match &cell.connection_ref {
                Some(slot) => {
                    if ctx.doc.connection(slot).is_none() {
                        return Err(Refusal::UnknownConnection {
                            connection: slot.clone(),
                        });
                    }
                    slot.clone()
                }
                None => ctx.slot_of(None),
            };
            check_sql(ctx, &slot, &cell.source, cell.name.as_deref())?;
        }
        out.push(ProposedCell {
            spec,
            ..cell.clone()
        });
    }
    Ok(out)
}

/// Check a proposed edit.
pub fn check_edit(
    ctx: &NotebookContext,
    cell_id: &str,
    source: Option<&str>,
) -> Result<(), Refusal> {
    if cell_id == ctx.ai_cell {
        return Err(Refusal::SelfEdit);
    }
    let Some(cell) = ctx.doc.cell(cell_id) else {
        return Err(Refusal::UnknownCell {
            cell: cell_id.to_string(),
        });
    };
    if cell.kind == CellKind::Ai {
        return Err(Refusal::SelfEdit);
    }
    if cell.kind == CellKind::Sql {
        if !cell.execution_policy.read_only {
            return Err(Refusal::WriteCell {
                cell: cell_id.to_string(),
            });
        }
        if let Some(source) = source {
            check_sql(ctx, &ctx.slot_of(Some(cell_id)), source, Some(cell_id))?;
        }
    }
    Ok(())
}

/// Check a proposed settings change of `cell_id`: it may not move the cell
/// to local data or another connection, nor point a diff side at a
/// connection the notebook does not declare.
pub fn check_spec_edit(ctx: &NotebookContext, cell_id: &str, spec: &Value) -> Result<(), Refusal> {
    let Some(cell) = ctx.doc.cell(cell_id) else {
        return Err(Refusal::UnknownCell {
            cell: cell_id.to_string(),
        });
    };
    let mut edited = cell.clone();
    edited.spec = spec.clone();
    let moved = crate::notebook::cells::local::targets_local(cell)
        != crate::notebook::cells::local::targets_local(&edited)
        || cell.exec_class() != edited.exec_class();
    if moved {
        return Err(Refusal::ChangesWhereItRuns {
            cell: cell_id.to_string(),
        });
    }
    if cell.kind == CellKind::Diff {
        let before = crate::notebook::cells::diff::DiffCellSpec::from_spec(&cell.spec);
        let after = crate::notebook::cells::diff::DiffCellSpec::from_spec(spec);
        if after.connection_slots() != before.connection_slots() {
            return Err(Refusal::ChangesWhereItRuns {
                cell: cell_id.to_string(),
            });
        }
    }
    Ok(())
}

/// Lines of a settings change, for the proposal's diff: every key that
/// changes is visible, never hidden behind an empty source diff.
pub fn spec_diff(before: &Value, after: &Value) -> Vec<DiffLine> {
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    if before == after {
        return Vec::new();
    }
    line_diff(&pretty(before), &pretty(after))
}

/// Check a proposed run: read-only SQL cells and local cells only.
pub fn check_run(ctx: &NotebookContext, cells: &[String]) -> Result<(), Refusal> {
    for id in cells {
        let Some(cell) = ctx.doc.cell(id) else {
            return Err(Refusal::UnknownCell { cell: id.clone() });
        };
        match cell.kind {
            CellKind::Sql if !cell.execution_policy.read_only => {
                return Err(Refusal::WriteCell { cell: id.clone() });
            }
            CellKind::Sql if !ctx.is_read_only_sql(id) => {
                return Err(Refusal::WriteStatement {
                    cell: Some(id.clone()),
                });
            }
            CellKind::Sql => {}
            CellKind::Ai => return Err(Refusal::NotRunnable { cell: id.clone() }),
            ref kind if kind.exec_class() == ExecClass::Local => {}
            _ => return Err(Refusal::NotRunnable { cell: id.clone() }),
        }
    }
    Ok(())
}

/// A line diff of `before` → `after` (longest common subsequence).
pub fn line_diff(before: &str, after: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a.len() * b.len() > 250_000 {
        return a
            .iter()
            .map(|l| DiffLine::Removed(l.to_string()))
            .chain(b.iter().map(|l| DiffLine::Added(l.to_string())))
            .collect();
    }
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = vec![];
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(DiffLine::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine::Removed(a[i].to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Added(b[j].to_string()));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| DiffLine::Removed(l.to_string())));
    out.extend(b[j..].iter().map(|l| DiffLine::Added(l.to_string())));
    out
}

/// What an applied group changed, so one undo reverses all of it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AppliedGroup {
    pub ai_cell: String,
    pub run_id: String,
    pub inserted: Vec<String>,
    pub edits: Vec<EditUndo>,
    /// Accepted run proposals: the host runs these through the normal path.
    pub run: Vec<String>,
    pub proposals: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditUndo {
    pub cell_id: String,
    pub source: String,
    pub spec: Value,
}

fn unique_name(doc: &NotebookDoc, cell_id: &str, wanted: &str) -> Option<String> {
    let base: String = wanted
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let base = base.trim_matches('_').to_ascii_lowercase();
    if base.is_empty() {
        return None;
    }
    std::iter::once(base.clone())
        .chain((2..10).map(|n| format!("{base}_{n}")))
        .find(|name| validate_cell_name(doc, cell_id, name).is_ok())
}

/// Apply every accepted proposal of the AI cell's last run as one group:
/// inserted cells are attributed to the agent and start read-only; edits
/// keep their before-image. Proposals are re-checked against the document
/// as it is now; one that no longer passes is marked refused and skipped.
pub fn apply_accepted(
    ctx: &NotebookContext,
    doc: &mut NotebookDoc,
    ids: &mut dyn IdSource,
) -> AppliedGroup {
    let ai_cell = ctx.ai_cell.clone();
    let Some(cell) = doc.cell(&ai_cell) else {
        return AppliedGroup::default();
    };
    let mut spec = AiCellSpec::of(cell);
    let Some(mut result) = spec.result.take() else {
        return AppliedGroup::default();
    };
    let mut group = AppliedGroup {
        ai_cell: ai_cell.clone(),
        run_id: result.run_id.clone(),
        ..AppliedGroup::default()
    };
    let mut anchor = ai_cell.clone();
    for proposal in result
        .proposals
        .iter_mut()
        .filter(|p| p.status == ProposalStatus::Accepted)
    {
        let check = match &proposal.action {
            ProposalAction::InsertCells { after, cells } => {
                check_insert(ctx, after.as_deref(), cells)
            }
            ProposalAction::EditCell {
                cell_id, source, ..
            } => check_edit(ctx, cell_id, source.as_deref()),
            ProposalAction::RunCells { cells } => check_run(ctx, cells),
        };
        if let Err(refusal) = check {
            refuse(proposal, &refusal);
            continue;
        }
        match &proposal.action {
            ProposalAction::InsertCells { after, cells } => {
                if let Some(after) = after
                    && let Some(target) = ctx.resolve_cell(after)
                {
                    anchor = target.id.clone();
                }
                for proposed in cells {
                    let Some(at) = doc.cell_index(&anchor) else {
                        break;
                    };
                    let kind = CellKind::parse(&proposed.kind);
                    let index = doc.add_cell(kind.clone(), Some(at), ids);
                    let id = doc.cells[index].id.clone();
                    let new: &mut Cell = &mut doc.cells[index];
                    new.source = proposed.source.clone();
                    new.spec = proposed.spec.clone();
                    new.connection_ref = proposed.connection_ref.clone();
                    if kind == CellKind::Sql {
                        new.execution_policy.read_only = true;
                    }
                    mark_generated(new, &ai_cell, &result.run_id, &proposal.id);
                    if let Some(name) = proposed
                        .name
                        .as_deref()
                        .and_then(|wanted| unique_name(doc, &id, wanted))
                    {
                        doc.cells[index].name = Some(name);
                    }
                    group.inserted.push(id.clone());
                    anchor = id;
                }
            }
            ProposalAction::EditCell {
                cell_id,
                source,
                spec: new_spec,
                ..
            } => {
                let Some(target) = doc.cell_mut(cell_id) else {
                    continue;
                };
                group.edits.push(EditUndo {
                    cell_id: cell_id.clone(),
                    source: target.source.clone(),
                    spec: target.spec.clone(),
                });
                if let Some(source) = source {
                    target.source = source.clone();
                }
                if let Some(new_spec) = new_spec {
                    target.spec = new_spec.clone();
                }
            }
            ProposalAction::RunCells { cells } => group.run.extend(cells.iter().cloned()),
        }
        proposal.status = ProposalStatus::Applied;
        group.proposals.push(proposal.id.clone());
    }
    write_result(doc, &ai_cell, spec, result, |spec| {
        spec.created_cells.extend(group.inserted.iter().cloned());
    });
    group
}

fn refuse(proposal: &mut Proposal, refusal: &Refusal) {
    proposal.status = ProposalStatus::Refused;
    proposal.refusal = Some(refusal.stored());
    proposal.refusal_text = Some(refusal.english());
}

fn write_result(
    doc: &mut NotebookDoc,
    ai_cell: &str,
    mut spec: AiCellSpec,
    result: AiResult,
    edit: impl FnOnce(&mut AiCellSpec),
) {
    spec.result = Some(result);
    edit(&mut spec);
    if let Some(cell) = doc.cell_mut(ai_cell) {
        spec.write_to(cell);
    }
}

/// Reverse an applied group: inserted cells removed, edited cells restored,
/// the proposals pending again.
pub fn undo(doc: &mut NotebookDoc, group: &AppliedGroup) {
    for id in &group.inserted {
        if let Some(index) = doc.cell_index(id) {
            doc.remove_cell(index);
        }
    }
    for edit in group.edits.iter().rev() {
        if let Some(cell) = doc.cell_mut(&edit.cell_id) {
            cell.source = edit.source.clone();
            cell.spec = edit.spec.clone();
        }
    }
    let Some(cell) = doc.cell(&group.ai_cell) else {
        return;
    };
    let mut spec = AiCellSpec::of(cell);
    let Some(mut result) = spec.result.take() else {
        return;
    };
    for id in &group.proposals {
        if let Some(p) = result.proposal_mut(id) {
            p.status = ProposalStatus::Accepted;
        }
    }
    write_result(doc, &group.ai_cell, spec, result, |spec| {
        spec.created_cells.retain(|id| !group.inserted.contains(id));
    });
}

/// Set one proposal's status from the user's choice (accept or reject).
pub fn set_status(
    doc: &mut NotebookDoc,
    ai_cell: &str,
    proposal: &str,
    status: ProposalStatus,
) -> bool {
    let Some(cell) = doc.cell(ai_cell) else {
        return false;
    };
    let mut spec = AiCellSpec::of(cell);
    let Some(mut result) = spec.result.take() else {
        return false;
    };
    let Some(p) = result.proposal_mut(proposal) else {
        return false;
    };
    if !p.is_actionable() {
        return false;
    }
    p.status = status;
    write_result(doc, ai_cell, spec, result, |_| {});
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::cells::ai::AgentMode;
    use crate::notebook::{ConnectionRef, ExecutionPolicy, SeededIds};
    use crate::notebook_security::{ConnectionFacts, TrustDecision};
    use serde_json::json;
    use sqlly_core::DatabaseType;
    use std::collections::BTreeMap;

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(9));
        doc.cells.clear();
        doc.connection_refs.push(ConnectionRef {
            id: "conn_1".into(),
            profile_id: Some("p1".into()),
            label: "Sales".into(),
            database: None,
            extra: Default::default(),
        });
        let mut orders = Cell::new("c_orders", CellKind::Sql, ExecutionPolicy::default());
        orders.name = Some("orders".into());
        orders.source = "SELECT * FROM orders".into();
        doc.cells.push(orders);
        let mut fix = Cell::new("c_fix", CellKind::Sql, ExecutionPolicy::default());
        fix.execution_policy.read_only = false;
        fix.source = "UPDATE orders SET total = 0".into();
        doc.cells.push(fix);
        let mut ai = Cell::new("c_ai", CellKind::Ai, ExecutionPolicy::default());
        AiCellSpec::new(AgentMode::Build).write_to(&mut ai);
        doc.cells.push(ai);
        doc
    }

    fn ctx(doc: &NotebookDoc) -> NotebookContext {
        let mut connections = BTreeMap::new();
        connections.insert(
            "conn_1".to_string(),
            ConnectionFacts {
                profile_id: Some("p1".into()),
                engine: Some(DatabaseType::Postgres),
                ..ConnectionFacts::default()
            },
        );
        NotebookContext {
            doc: doc.clone(),
            outputs: BTreeMap::new(),
            runtime: BTreeMap::new(),
            trust: TrustDecision::untrusted(doc),
            connections,
            ai_cell: "c_ai".into(),
        }
    }

    fn sql(source: &str) -> ProposedCell {
        ProposedCell {
            kind: "sql".into(),
            name: Some("by_day".into()),
            source: source.into(),
            ..ProposedCell::default()
        }
    }

    #[test]
    fn spec_edits_cannot_move_a_cell_and_always_show_in_the_diff() {
        let mut doc = doc();
        let mut check = Cell::new("c_check", CellKind::Assertion, ExecutionPolicy::default());
        check.spec = json!({"input": "orders", "assertions": [{"kind": "row_count", "min": 100}]});
        doc.cells.push(check);
        let ctx = ctx(&doc);
        assert_eq!(
            check_spec_edit(&ctx, "c_orders", &json!({"target": "local"})),
            Err(Refusal::ChangesWhereItRuns {
                cell: "c_orders".into()
            })
        );
        assert!(
            check_spec_edit(&ctx, "c_orders", &json!({"cache": {"storage": "memory"}})).is_ok()
        );
        let widened = json!({"input": "orders", "assertions": [{"kind": "row_count", "min": 0}]});
        assert!(check_spec_edit(&ctx, "c_check", &widened).is_ok());
        let diff = spec_diff(&doc.cell("c_check").unwrap().spec, &widened);
        assert!(!diff.is_empty(), "a widened threshold is visible");
        assert!(spec_diff(&widened, &widened).is_empty());
    }

    #[test]
    fn write_sql_and_write_cells_are_refused() {
        let doc = doc();
        let ctx = ctx(&doc);
        assert!(check_insert(&ctx, None, &[sql("SELECT 1")]).is_ok());
        for bad in [
            "DELETE FROM orders",
            "SELECT 1; DROP TABLE orders",
            "WITH x AS (DELETE FROM orders RETURNING *) SELECT * FROM x",
            "INSERT INTO audit SELECT * FROM orders",
        ] {
            assert!(
                matches!(
                    check_insert(&ctx, None, &[sql(bad)]),
                    Err(Refusal::WriteStatement { .. })
                ),
                "{bad}"
            );
            assert!(matches!(
                check_edit(&ctx, "c_orders", Some(bad)),
                Err(Refusal::WriteStatement { .. })
            ));
        }
        assert_eq!(
            check_edit(&ctx, "c_fix", Some("SELECT 1")),
            Err(Refusal::WriteCell {
                cell: "c_fix".into()
            })
        );
        assert_eq!(check_edit(&ctx, "c_ai", Some("x")), Err(Refusal::SelfEdit));
        assert_eq!(
            check_run(&ctx, &["c_fix".into()]),
            Err(Refusal::WriteCell {
                cell: "c_fix".into()
            })
        );
        assert_eq!(
            check_run(&ctx, &["c_ai".into()]),
            Err(Refusal::NotRunnable {
                cell: "c_ai".into()
            })
        );
        assert!(check_run(&ctx, &["c_orders".into()]).is_ok());
        let mut python = sql("x");
        python.kind = "python".into();
        assert_eq!(
            check_insert(&ctx, None, &[python]),
            Err(Refusal::UnsupportedKind {
                kind: "python".into()
            })
        );
        let mut elsewhere = sql("SELECT 1");
        elsewhere.connection_ref = Some("prod".into());
        assert_eq!(
            check_insert(&ctx, None, &[elsewhere]),
            Err(Refusal::UnknownConnection {
                connection: "prod".into()
            })
        );
        assert!(
            Refusal::WriteCell { cell: "x".into() }
                .stored()
                .key
                .starts_with("notebook.ai.refusal.")
        );
    }

    #[test]
    fn accepted_proposals_apply_as_one_group_and_undo_together() {
        let mut doc = doc();
        let mut spec = AiCellSpec::of(doc.cell("c_ai").unwrap());
        let proposal = |id: &str, action: ProposalAction, status| Proposal {
            id: id.into(),
            action,
            rationale: String::new(),
            status,
            refusal: None,
            refusal_text: None,
        };
        spec.result = Some(AiResult {
            run_id: "run_ai".into(),
            proposals: vec![
                proposal(
                    "p1",
                    ProposalAction::InsertCells {
                        after: None,
                        cells: vec![
                            sql("SELECT day, count(*) FROM {{orders}} GROUP BY day"),
                            ProposedCell {
                                kind: "chart".into(),
                                spec: json!({"input": "by_day", "kind": "line"}),
                                ..ProposedCell::default()
                            },
                        ],
                    },
                    ProposalStatus::Accepted,
                ),
                proposal(
                    "p2",
                    ProposalAction::EditCell {
                        cell_id: "c_orders".into(),
                        source: Some("SELECT id, total FROM orders".into()),
                        spec: None,
                        diff: vec![],
                    },
                    ProposalStatus::Accepted,
                ),
                proposal(
                    "p3",
                    ProposalAction::InsertCells {
                        after: None,
                        cells: vec![sql("SELECT 2")],
                    },
                    ProposalStatus::Rejected,
                ),
                proposal(
                    "p4",
                    ProposalAction::EditCell {
                        cell_id: "c_orders".into(),
                        source: Some("TRUNCATE orders".into()),
                        spec: None,
                        diff: vec![],
                    },
                    ProposalStatus::Accepted,
                ),
                proposal(
                    "p5",
                    ProposalAction::RunCells {
                        cells: vec!["c_orders".into()],
                    },
                    ProposalStatus::Accepted,
                ),
            ],
            ..AiResult::default()
        });
        spec.write_to(doc.cell_mut("c_ai").unwrap());
        let before = doc.clone();
        let context = ctx(&doc);
        let group = apply_accepted(&context, &mut doc, &mut SeededIds(4));
        assert_eq!(group.inserted.len(), 2);
        assert_eq!(group.proposals, vec!["p1", "p2", "p5"]);
        assert_eq!(group.run, vec!["c_orders"]);
        let ai_index = doc.cell_index("c_ai").unwrap();
        let first = &doc.cells[ai_index + 1];
        assert_eq!(first.id, group.inserted[0]);
        assert_eq!(first.name.as_deref(), Some("by_day"));
        assert!(first.execution_policy.read_only);
        let attribution = crate::notebook::cells::ai::generated_by(first).unwrap();
        assert_eq!(
            (attribution.ai_cell.as_str(), attribution.proposal.as_str()),
            ("c_ai", "p1")
        );
        assert_eq!(doc.cells[ai_index + 2].kind, CellKind::Chart);
        assert_eq!(
            doc.cell("c_orders").unwrap().source,
            "SELECT id, total FROM orders"
        );
        let result = AiCellSpec::of(doc.cell("c_ai").unwrap()).result.unwrap();
        assert_eq!(
            result.proposal("p4").unwrap().status,
            ProposalStatus::Refused
        );
        assert_eq!(
            result.proposal("p3").unwrap().status,
            ProposalStatus::Rejected
        );
        assert_eq!(
            AiCellSpec::of(doc.cell("c_ai").unwrap()).created_cells,
            group.inserted
        );

        undo(&mut doc, &group);
        assert_eq!(doc.cells.len(), before.cells.len());
        assert_eq!(doc.cell("c_orders").unwrap().source, "SELECT * FROM orders");
        let result = AiCellSpec::of(doc.cell("c_ai").unwrap()).result.unwrap();
        assert_eq!(
            result.proposal("p1").unwrap().status,
            ProposalStatus::Accepted
        );
        assert!(
            AiCellSpec::of(doc.cell("c_ai").unwrap())
                .created_cells
                .is_empty()
        );
        assert!(set_status(&mut doc, "c_ai", "p1", ProposalStatus::Rejected));
        assert!(!set_status(
            &mut doc,
            "c_ai",
            "p4",
            ProposalStatus::Accepted
        ));
    }

    #[test]
    fn proposed_specs_must_read_as_their_kind() {
        let doc = doc();
        let ctx = ctx(&doc);
        let cell = |kind: &str, spec: Value| ProposedCell {
            kind: kind.into(),
            spec,
            ..ProposedCell::default()
        };
        let ok = normalize_insert(
            &ctx,
            None,
            &[
                cell("chart", json!({"input": "orders", "kind": "line", "x": "day", "y": ["total"]})),
                cell("assertion", json!({"input": "orders", "assertions": [{"kind": "unique", "columns": ["id"]}]})),
                cell("profile", json!({"input": "orders"})),
                cell("kpi", json!({"input": "orders", "tiles": []})),
            ],
        )
        .unwrap();
        assert_eq!(ok[0].spec["chart"]["kind"], "line");
        assert_eq!(ok[0].spec["chart"]["y"][0]["column"], "total");
        assert_eq!(ok[0].spec["input"], "orders");
        for (kind, spec) in [
            ("chart", json!({"input": "orders", "kind": "spiral"})),
            ("chart", json!({"kind": "line"})),
            ("assertion", json!({"input": "orders", "assertions": []})),
            (
                "assertion",
                json!({"input": "orders", "assertions": [{"kind": "nonsense"}]}),
            ),
            ("table", json!({"input": "orders", "height": "tall"})),
            ("profile", json!({})),
        ] {
            assert_eq!(
                normalize_insert(&ctx, None, &[cell(kind, spec.clone())]),
                Err(Refusal::InvalidKindSpec { kind: kind.into() }),
                "{kind} {spec}"
            );
        }
    }

    #[test]
    fn plan_and_entity_detail_cells_are_proposable_in_their_own_shape() {
        let doc = doc();
        let ctx = ctx(&doc);
        let cell = |kind: &str, spec: Value| ProposedCell {
            kind: kind.into(),
            spec,
            ..ProposedCell::default()
        };
        let ok = normalize_insert(
            &ctx,
            None,
            &[
                cell(
                    "plan",
                    json!({"input": "orders", "session_stats": true, "note": "dropped",
                           "sweep": {"parameter": "region", "values": ["EU", "US"], "repetitions": 2}}),
                ),
                cell(
                    "entity_detail",
                    json!({"source": {"cell": "orders", "row": 3},
                           "table": {"schema": "dbo", "name": "Customers"},
                           "relationships": [{"direction": "incoming", "table": "Orders",
                                              "columns": [["CustomerID", "CustomerID"]]}],
                           "limit": 9000}),
                ),
                cell(
                    "entity_detail",
                    json!({"source": {"parameter": "customer", "column": "CustomerID"},
                           "table": {"name": "Customers"}}),
                ),
            ],
        )
        .unwrap();
        assert_eq!(ok[0].spec["input"], "orders");
        assert_eq!(ok[0].spec["session_stats"], true);
        assert_eq!(ok[0].spec["sweep"]["repetitions"], 2);
        assert!(
            ok[0].spec.get("note").is_none(),
            "unknown fields are not kept"
        );
        assert_eq!(
            ok[1].spec["input"], "orders",
            "the graph sees the source cell"
        );
        assert_eq!(ok[1].spec["source"]["row"], 3);
        assert_eq!(
            ok[1].spec["limit"],
            crate::notebook::cells::entity_detail::MAX_LIMIT,
            "the row limit is clamped"
        );
        assert_eq!(ok[2].spec["source"]["parameter"], "customer");
        let many: Vec<Value> = (0..crate::notebook::cells::plan::MAX_SWEEP_RUNS + 1)
            .map(|i| json!(i))
            .collect();
        for (kind, spec) in [
            ("plan", json!({})),
            ("plan", json!({"input": "orders", "mode": "sideways"})),
            (
                "plan",
                json!({"input": "orders", "sweep": {"parameter": "", "values": [1]}}),
            ),
            (
                "plan",
                json!({"input": "orders", "sweep": {"parameter": "region", "values": many}}),
            ),
            ("entity_detail", json!({"table": {"name": "Customers"}})),
            (
                "entity_detail",
                json!({"source": {"cell": "orders"}, "table": {"name": ""}}),
            ),
            (
                "entity_detail",
                json!({"source": {"parameter": "customer"}, "table": {"name": "Customers"}}),
            ),
            (
                "entity_detail",
                json!({"source": {"cell": "orders"}, "table": {"name": "Customers"},
                       "relationships": [{"direction": "incoming", "table": "Orders", "columns": []}]}),
            ),
        ] {
            assert_eq!(
                normalize_insert(&ctx, None, &[cell(kind, spec.clone())]),
                Err(Refusal::InvalidKindSpec { kind: kind.into() }),
                "{kind} {spec}"
            );
        }
    }

    #[test]
    fn diffs_mark_changed_lines() {
        let diff = line_diff("SELECT a\nFROM t\nWHERE x", "SELECT a, b\nFROM t");
        assert_eq!(
            diff,
            vec![
                DiffLine::Removed("SELECT a".into()),
                DiffLine::Added("SELECT a, b".into()),
                DiffLine::Same("FROM t".into()),
                DiffLine::Removed("WHERE x".into()),
            ]
        );
    }
}
