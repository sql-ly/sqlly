//! Running analysis cells: in-process kinds on the background executor,
//! connection-backed kinds through host requests, all finishing through the
//! shell's normal completion path so the queue, state and run record see
//! them like any other cell.

use std::collections::BTreeMap;

use chrono::Utc;
use gpui::Context;
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_intel::query::safety::{classify_select_only_for, is_single_statement_for};
use sqlly_services::analytics::OutputView;
use sqlly_services::analytics::assertions::{AssertionContext, evaluate_all};
use sqlly_services::analytics::diff::{OutputDiff, diff_outputs};
use sqlly_services::analytics::profile::{ProfileOptions, Profiler, ResultProfile, profile_output};
use sqlly_services::notebook::cells::assertion::{
    AssertionCellSpec, AssertionRun, BaselineRef, FailingSample, TestSummary,
};
use sqlly_services::notebook::cells::diff::{DiffCellSpec, DiffSide, SideSource};
use sqlly_services::notebook::cells::entity_detail::{
    DetailQueryError, EntityDetailSpec, EntitySource, MAX_RELATIONSHIPS, parameter_row,
    related_rows_sql,
};
use sqlly_services::notebook::cells::output_view;
use sqlly_services::notebook::cells::plan::{
    PlanCellSpec, PlanCompare, PlanMode, StatDelta, StatsSource, SweepSpec, TimingStats,
    plan_table, session_stats_query, stat_deltas,
};
use sqlly_services::notebook::cells::profile::WholeTable;
use sqlly_services::notebook::output::DEFAULT_OUTPUT_ROW_CAP;
use sqlly_services::notebook::{CellOutput, Provenance};
use sqlly_services::notebook_security::{Decision, NotebookAction, Reason};
use sqlly_services::plans::{ExecutionPlan, ExplainOptions};

use super::inputs::{self, InputData, SpillRows};
use super::{
    AnalysisReply, AnalysisRequest, AnalysisWork, CellViewHandle, KIND_ASSERTION, KIND_DIFF,
    KIND_ENTITY_DETAIL, KIND_PLAN, KIND_PROFILE, QueryRows, StatsPair, is_analysis_kind,
};
use crate::notebook::cells::entity_detail::RelatedRows;
use crate::notebook::cells::plan::{PlanResult, SweepPoint};
use crate::notebook::connection::CellRunTarget;
use crate::notebook::view::{NotebookEvent, NotebookView};

/// A cell error kept as its key, translated when drawn.
fn e(key: &'static str) -> DisplayMessage {
    crate::localization::deferred_text(key)
}

/// What a host request was for, kept until its reply.
pub(super) enum Pending {
    Plan {
        spec: PlanCellSpec,
        engine: String,
        stats_source: Option<StatsSource>,
        input: String,
    },
    Sweep {
        sweep: SweepSpec,
        engine: String,
        stats_source: Option<StatsSource>,
        plan: Box<PlanResult>,
    },
    Entity {
        labels: Vec<usize>,
        skipped: Vec<(usize, DisplayMessage)>,
        key: (Vec<ResultColumn>, Vec<CellValue>),
        limit: usize,
    },
    DiffRight {
        left: Box<CellOutput>,
        spec: Box<DiffCellSpec>,
    },
    /// A SQL cell's cut values read again whole, for its run `run_id`.
    FullValues { run_id: String },
}

/// Rows of an input as an analytics view: streamed from its spill file
/// when it has one, else the in-memory snapshot.
enum Rows {
    Memory(InputData),
    Spill(InputData, SpillRows),
}

impl Rows {
    fn of(input: InputData) -> Self {
        match input.spill.clone() {
            Some((path, total)) => Rows::Spill(input, SpillRows { path, total }),
            None => Rows::Memory(input),
        }
    }

    fn output(&self) -> &CellOutput {
        match self {
            Rows::Memory(i) | Rows::Spill(i, _) => &i.output,
        }
    }

    fn view(&self) -> OutputView<'_> {
        match self {
            Rows::Memory(i) => output_view(&i.output),
            Rows::Spill(i, source) => OutputView::from_source(
                &i.output.columns,
                source,
                sqlly_services::analytics::Completeness::Spilled {
                    total: source.total as u64,
                },
            ),
        }
    }
}

fn view_of(output: &CellOutput) -> OutputView<'_> {
    output_view(output)
}

/// The profile of the columns `spec` names.
pub(crate) fn compute_profile(
    view: &OutputView<'_>,
    keep: &dyn Fn(&str) -> bool,
    options: &ProfileOptions,
) -> ResultProfile {
    let picked: Vec<usize> = view
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| keep(&c.name))
        .map(|(i, _)| i)
        .collect();
    if picked.len() == view.columns.len() {
        return profile_output(view, options);
    }
    let columns: Vec<ResultColumn> = picked.iter().map(|i| view.columns[*i].clone()).collect();
    let mut profiler = Profiler::new(&columns, options.clone());
    view.for_each_batch(&mut |batch| {
        let projected: Vec<Vec<CellValue>> = batch
            .iter()
            .map(|row| {
                picked
                    .iter()
                    .map(|i| row.get(*i).cloned().unwrap_or(CellValue::Null))
                    .collect()
            })
            .collect();
        profiler.push_batch(&projected);
        std::ops::ControlFlow::Continue(())
    });
    profiler.finish(view.completeness)
}

/// One row per profiled column, for cells that read a profile's output.
/// Counts and ranges come from the whole local table when `whole` has the
/// column (`source` = `table`), else from the rows read (`rows`).
fn profile_rows(
    profile: &ResultProfile,
    whole: Option<&WholeTable>,
) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let col = |name: &str, ty: &str| ResultColumn {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
    };
    let columns = vec![
        col("column", "nvarchar"),
        col("type", "nvarchar"),
        col("nulls", "bigint"),
        col("null_rate", "float"),
        col("distinct", "bigint"),
        col("min", "float"),
        col("max", "float"),
        col("mean", "float"),
        col("source", "nvarchar"),
    ];
    let num = |v: Option<f64>| {
        v.filter(|f| f.is_finite())
            .map_or(CellValue::Null, CellValue::F64)
    };
    let rows = profile
        .columns
        .iter()
        .map(|c| {
            let n = c.numeric.as_ref();
            match whole.and_then(|w| w.column(&c.name)) {
                Some(w) => {
                    let parsed = |v: &Option<String>| {
                        num(n.and(v.as_ref()).and_then(|t| t.trim().parse().ok()))
                    };
                    vec![
                        CellValue::String(c.name.clone()),
                        CellValue::String(format!("{:?}", c.semantic_type)),
                        CellValue::I64(w.null_count as i64),
                        CellValue::F64(w.null_rate()),
                        CellValue::I64(w.distinct as i64),
                        parsed(&w.min),
                        parsed(&w.max),
                        num(n.and(w.mean)),
                        CellValue::String("table".into()),
                    ]
                }
                None => vec![
                    CellValue::String(c.name.clone()),
                    CellValue::String(format!("{:?}", c.semantic_type)),
                    CellValue::I64(c.null_count as i64),
                    CellValue::F64(c.null_rate),
                    CellValue::I64(c.distinct.value as i64),
                    num(n.map(|n| n.min)),
                    num(n.map(|n| n.max)),
                    num(n.map(|n| n.mean)),
                    CellValue::String("rows".into()),
                ],
            }
        })
        .collect();
    (columns, rows)
}

/// `sql` as an actual-plan capture wraps it. T-SQL needs no `;`, so an
/// unterminated query would run into the `SET STATISTICS XML OFF` after it
/// and read as one statement the read-only gate refuses.
fn capture_input(dialect: sqlly_services::plans::PlanDialect, actual: bool, sql: &str) -> String {
    if actual
        && dialect == sqlly_services::plans::PlanDialect::SqlServer
        && !sql.trim_end().ends_with(';')
    {
        format!("{}\n;", sql.trim_end())
    } else {
        sql.to_string()
    }
}

fn param_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl NotebookView {
    /// Start `id` when it is an analysis cell. Returns false for other
    /// kinds, which the host runs.
    pub(in crate::notebook) fn start_analysis_cell(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(kind) = self.doc.cell_by_id(id).map(|c| c.kind.to_string()) else {
            return false;
        };
        if !is_analysis_kind(&kind) {
            return false;
        }
        self.mark_started(id);
        let started = match kind.as_str() {
            KIND_PROFILE => self.run_profile(id, cx),
            KIND_ASSERTION => self.run_assertion(id, cx),
            KIND_DIFF => self.run_diff(id, cx),
            KIND_PLAN => self.run_plan(id, cx),
            KIND_ENTITY_DETAIL => self.run_entity_detail(id, cx),
            _ => Ok(()),
        };
        if let Err(message) = started {
            self.finish_analysis(id, Err(message), cx);
        }
        true
    }

    /// The analysis cell `id` finished (or could not run).
    pub(super) fn finish_analysis(
        &mut self,
        id: &str,
        result: Result<(), DisplayMessage>,
        cx: &mut Context<Self>,
    ) {
        self.analysis.tasks.remove(id);
        let ok = result.is_ok();
        if let Some(rt) = self.cells.get_mut(id) {
            rt.message = result.err();
        }
        self.remeasure_cell(id);
        self.cell_finished(id, ok, cx);
        cx.notify();
    }

    // ── Profile ───────────────────────────────────────────────────────────

    fn run_profile(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), DisplayMessage> {
        let spec = sqlly_services::notebook::cells::profile::ProfileCellSpec::from_spec(
            &self.doc.spec(id),
        );
        let input_id = self
            .resolve_cell(&spec.input)
            .ok_or_else(|| e("notebook.analysis.input_missing"))?;
        let input = inputs::cell_output(self, &input_id, cx).ok_or_else(|| {
            crate::localization::deferred(
                "notebook.analysis.input_no_result",
                &[("cell", self.display_name(&input_id))],
            )
        })?;
        let total = input.output.row_count;
        let whole = self.whole_table_stats(&input_id, &input, cx);
        let cell = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let profile = cx
                .background_executor()
                .spawn(async move {
                    let rows = Rows::of(input);
                    let options = spec.options();
                    compute_profile(&rows.view(), &|c| spec.includes(c), &options)
                })
                .await;
            let whole = match whole {
                Some(task) => task.await,
                None => None,
            };
            this.update(cx, |this, cx| {
                this.profile_done(&cell, profile, whole, total, cx)
            })
            .ok();
        });
        self.analysis.tasks.insert(id.to_string(), task);
        Ok(())
    }

    /// `SUMMARIZE` of the local table `input_id`'s result was copied into
    /// (or an attachment imported), when that table is complete, current
    /// and larger than the rows the profile reads.
    #[cfg(not(target_arch = "wasm32"))]
    fn whole_table_stats(
        &self,
        input_id: &str,
        input: &InputData,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Task<Option<WholeTable>>> {
        use crate::notebook::doc_adapter::{InputSource, KIND_ATTACHMENT, LocalInput};
        let ws = self.local.workspace.clone()?;
        let row = self.local.relation_from_cell(input_id)?;
        let prov = row.provenance.as_ref()?;
        let read = input
            .spill
            .as_ref()
            .map_or(input.output.rows.len() as u64, |(_, n)| *n as u64);
        if !prov.complete || row.is_view || row.rows <= read || self.doc.targets_local(input_id) {
            return None;
        }
        let source = if self.doc.cell_by_id(input_id)?.kind == KIND_ATTACHMENT {
            InputSource::Attachment
        } else {
            InputSource::Remote
        };
        let current = self.input_current(&LocalInput {
            cell_id: input_id.to_string(),
            relation: row.name.clone(),
            source,
        });
        if !current {
            return None;
        }
        let relation = row.name.clone();
        let handle = crate::tokio_handle();
        Some(cx.background_executor().spawn(async move {
            handle
                .spawn(async move {
                    let set = ws
                        .summarize(
                            relation.clone(),
                            sqlly_services::local_analytics::CancelToken::new(),
                        )
                        .await
                        .ok()?;
                    WholeTable::from_summarize(&relation, &set.columns, &set.rows)
                })
                .await
                .ok()
                .flatten()
        }))
    }

    #[cfg(target_arch = "wasm32")]
    fn whole_table_stats(
        &self,
        _input_id: &str,
        _input: &InputData,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Task<Option<WholeTable>>> {
        None
    }

    fn profile_done(
        &mut self,
        id: &str,
        profile: ResultProfile,
        whole: Option<WholeTable>,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        let whole = whole.filter(|w| w.rows > profile.row_count);
        let (columns, rows) = profile_rows(&profile, whole.as_ref());
        self.analysis.outputs.insert(
            id.to_string(),
            inputs::derived_output(columns, rows, "", 0, Vec::new()),
        );
        if let Some(CellViewHandle::Profile(view)) = self.analysis.views.get(id).cloned() {
            let spec = sqlly_services::notebook::cells::profile::ProfileCellSpec::from_spec(
                &self.doc.spec(id),
            );
            view.update(cx, |v, cx| v.set_profile(profile, whole, total, &spec, cx));
        }
        self.finish_analysis(id, Ok(()), cx);
    }

    // ── Assertions ────────────────────────────────────────────────────────

    fn run_assertion(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), DisplayMessage> {
        let spec = AssertionCellSpec::from_spec(&self.doc.spec(id));
        if spec.assertions.is_empty() {
            return Err(e("notebook.analysis.assertion.none"));
        }
        let input_id = self
            .resolve_cell(&spec.input)
            .ok_or_else(|| e("notebook.analysis.input_missing"))?;
        let input = inputs::cell_output(self, &input_id, cx).ok_or_else(|| {
            crate::localization::deferred(
                "notebook.analysis.input_no_result",
                &[("cell", self.display_name(&input_id))],
            )
        })?;
        let mut parents: BTreeMap<String, CellOutput> = BTreeMap::new();
        for name in spec.parents() {
            if let Some(pid) = self.resolve_cell(&name)
                && let Some(p) = inputs::cell_output(self, &pid, cx)
            {
                parents.insert(name, p.output);
            }
        }
        let baseline = if spec.needs_baseline() {
            match &spec.baseline {
                BaselineRef::PreviousRun => self.previous_output(&input_id),
                BaselineRef::Snapshot(name) => self.snapshot_output(name),
                BaselineRef::None => None,
            }
        } else {
            None
        };
        let cell = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let run = cx
                .background_executor()
                .spawn(async move {
                    let rows = Rows::of(input);
                    let view = rows.view();
                    let named: BTreeMap<String, OutputView<'_>> = parents
                        .iter()
                        .map(|(k, v)| (k.clone(), view_of(v)))
                        .collect();
                    let baseline_profile = baseline
                        .as_ref()
                        .map(|b| profile_output(&view_of(b), &ProfileOptions::default()));
                    let mut context = AssertionContext::new(view);
                    context.now = Some(Utc::now().naive_utc());
                    context.elapsed_ms = Some(rows.output().elapsed_ms);
                    context.outputs = &named;
                    context.baseline = baseline_profile.as_ref();
                    let outcomes = evaluate_all(&spec.assertions, &context);
                    let samples = outcomes
                        .iter()
                        .enumerate()
                        .filter_map(|(i, o)| Some((i, FailingSample::of(o, &context.output)?)))
                        .collect();
                    AssertionRun { outcomes, samples }
                })
                .await;
            this.update(cx, |this, cx| this.assertion_done(&cell, run, cx))
                .ok();
        });
        self.analysis.tasks.insert(id.to_string(), task);
        Ok(())
    }

    fn assertion_done(&mut self, id: &str, run: AssertionRun, cx: &mut Context<Self>) {
        self.analysis
            .tests
            .insert(id.to_string(), TestSummary::of(&run.outcomes));
        if let Some(CellViewHandle::Assertion(view)) = self.analysis.views.get(id).cloned() {
            let shown = run.clone();
            view.update(cx, |v, cx| v.set_run(shown, cx));
        }
        self.analysis.history.note_assertions(id, run);
        self.finish_analysis(id, Ok(()), cx);
    }

    // ── Diff ──────────────────────────────────────────────────────────────

    fn side_output(
        &mut self,
        side: &DiffSide,
        cx: &mut Context<Self>,
    ) -> Result<CellOutput, DisplayMessage> {
        let cell = self
            .resolve_cell(&side.cell)
            .ok_or_else(|| e("notebook.analysis.input_missing"))?;
        let name = self.display_name(&cell);
        let missing =
            |key: &'static str| crate::localization::deferred(key, &[("cell", name.as_str())]);
        match &side.source {
            SideSource::Current | SideSource::Connection(_) => inputs::cell_output(self, &cell, cx)
                .map(|i| i.output)
                .ok_or_else(|| missing("notebook.analysis.input_no_result")),
            SideSource::PreviousRun => self
                .previous_output(&cell)
                .ok_or_else(|| missing("notebook.analysis.diff.no_previous_run")),
            SideSource::Run(run) => self
                .recorded_output(run, &cell)
                .ok_or_else(|| missing("notebook.analysis.diff.no_recorded_output")),
            SideSource::Snapshot(name) => self
                .snapshot_output(name)
                .ok_or_else(|| missing("notebook.analysis.diff.no_snapshot")),
        }
    }

    fn run_diff(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), DisplayMessage> {
        let spec = DiffCellSpec::from_spec(&self.doc.spec(id));
        if !spec.is_configured() {
            return Err(e("notebook.analysis.diff.not_configured"));
        }
        let left = self.side_output(&spec.left, cx)?;
        if let SideSource::Connection(slot) = spec.right.source.clone() {
            let cell = self
                .resolve_cell(&spec.right.cell)
                .ok_or_else(|| e("notebook.analysis.input_missing"))?;
            let sql = self.doc.sql_for_run(&cell)?;
            let db = self.doc.database_type_for(&cell);
            if !classify_select_only_for(db, &sql).select_only {
                return Err(e("notebook.analysis.read_only_required"));
            }
            let profile_id = self
                .doc
                .model()
                .connection(&slot)
                .and_then(|s| s.profile_id.clone())
                .filter(|p| self.is_resolved(p))
                .and_then(|p| uuid::Uuid::parse_str(&p).ok())
                .ok_or_else(|| e("notebook.analysis.diff.connection_unmapped"))?;
            self.send_request(
                id,
                CellRunTarget {
                    profile_id: Some(profile_id),
                    read_only: true,
                    timeout_seconds: None,
                    local: false,
                },
                AnalysisWork::Rows {
                    queries: vec![sql],
                    max_rows: DEFAULT_OUTPUT_ROW_CAP,
                },
                Pending::DiffRight {
                    left: Box::new(left),
                    spec: Box::new(spec),
                },
                cx,
            );
            return Ok(());
        }
        let right = self.side_output(&spec.right, cx)?;
        self.spawn_diff(id, left, right, spec, cx);
        Ok(())
    }

    fn spawn_diff(
        &mut self,
        id: &str,
        left: CellOutput,
        right: CellOutput,
        spec: DiffCellSpec,
        cx: &mut Context<Self>,
    ) {
        let cell = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let diff = diff_outputs(&view_of(&left), &view_of(&right), &spec.options());
                    (diff, left.columns, right.columns)
                })
                .await;
            this.update(cx, |this, cx| this.diff_done(&cell, result, cx))
                .ok();
        });
        self.analysis.tasks.insert(id.to_string(), task);
    }

    #[allow(clippy::type_complexity)]
    fn diff_done(
        &mut self,
        id: &str,
        result: (
            Result<OutputDiff, sqlly_services::analytics::diff::OutputDiffError>,
            Vec<ResultColumn>,
            Vec<ResultColumn>,
        ),
        cx: &mut Context<Self>,
    ) {
        let (diff, left, right) = result;
        let outcome = diff.map_err(|e| crate::notebook::cells::diff::diff_error_text(&e));
        if let Some(CellViewHandle::Diff(view)) = self.analysis.views.get(id).cloned() {
            let shown = outcome.clone();
            view.update(cx, |v, cx| v.set_result(shown, left, right, cx));
        }
        self.finish_analysis(id, outcome.map(|_| ()).map_err(DisplayMessage::from), cx);
    }

    // ── Host requests ─────────────────────────────────────────────────────

    pub(super) fn send_request(
        &mut self,
        id: &str,
        target: CellRunTarget,
        work: AnalysisWork,
        pending: Pending,
        cx: &mut Context<Self>,
    ) {
        let request_id = self.next_request_id(id, pending);
        cx.emit(NotebookEvent::AnalysisQuery(AnalysisRequest {
            cell_id: id.to_string(),
            request_id,
            target,
            work,
        }));
    }

    /// The host's answer to an [`AnalysisRequest`].
    pub fn analysis_reply(
        &mut self,
        request_id: u64,
        reply: AnalysisReply,
        cx: &mut Context<Self>,
    ) {
        let Some((id, pending)) = self.analysis.pending.remove(&request_id) else {
            return;
        };
        if let Pending::FullValues { run_id } = pending {
            self.full_values_read(&id, run_id, reply, cx);
            return;
        }
        if let AnalysisReply::Failed(message) = reply {
            self.finish_analysis(&id, Err(message), cx);
            return;
        }
        match (pending, reply) {
            (Pending::DiffRight { left, spec }, AnalysisReply::Rows(mut sets)) => {
                match sets.pop() {
                    Some(Ok(rows)) => {
                        let right = CellOutput::capped(
                            rows.columns,
                            rows.rows,
                            None,
                            DEFAULT_OUTPUT_ROW_CAP,
                            rows.elapsed_ms,
                            "",
                            sqlly_services::notebook::Fingerprint(String::new()),
                            Provenance::default(),
                        );
                        let right = if rows.truncated {
                            CellOutput {
                                completeness: sqlly_services::notebook::Completeness::Truncated {
                                    cap: right.rows.len() as u64,
                                },
                                ..right
                            }
                        } else {
                            right
                        };
                        self.spawn_diff(&id, *left, right, *spec, cx);
                    }
                    Some(Err(error)) => self.finish_analysis(&id, Err(error), cx),
                    None => self.finish_analysis(&id, Err(e("notebook.analysis.no_rows")), cx),
                }
            }
            (
                Pending::Entity {
                    labels,
                    skipped,
                    key,
                    limit,
                },
                AnalysisReply::Rows(sets),
            ) => self.entity_done(&id, labels, skipped, key, limit, sets, cx),
            (
                Pending::Plan {
                    spec,
                    engine,
                    stats_source,
                    input,
                },
                AnalysisReply::Plan {
                    plans,
                    compare,
                    stats,
                },
            ) => self.plan_done(
                &id,
                spec,
                engine,
                stats_source,
                input,
                plans,
                compare,
                stats,
                cx,
            ),
            (
                Pending::Sweep {
                    sweep,
                    engine,
                    stats_source,
                    plan,
                },
                AnalysisReply::Sweep(results),
            ) => self.sweep_done(&id, sweep, engine, stats_source, *plan, results, cx),
            _ => self.finish_analysis(&id, Err(e("notebook.analysis.no_rows")), cx),
        }
    }

    // ── Plans and sweeps ──────────────────────────────────────────────────

    fn input_target(&self, input: &str, read_only: bool) -> Result<CellRunTarget, String> {
        let mut target = self.run_request(input)?.target;
        target.read_only = read_only;
        Ok(target)
    }

    fn run_plan(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), DisplayMessage> {
        use crate::execution_plan_view::PlanKind;
        let spec = PlanCellSpec::from_spec(&self.doc.spec(id));
        let input = self
            .resolve_cell(&spec.input)
            .ok_or_else(|| e("notebook.analysis.input_missing"))?;
        let db = self.doc.database_type_for(&input);
        let actual = spec.mode == PlanMode::Actual;
        let kind = if actual {
            PlanKind::Actual
        } else {
            PlanKind::Estimated
        };
        if let Some(reason) = crate::capabilities::plan_unavailable_message(db, kind) {
            return Err(reason);
        }
        let dialect = sqlly_services::plan_dialect(db)
            .ok_or_else(|| e("notebook.analysis.plan.unsupported"))?;
        let target = self.input_target(&input, true)?;
        let sql = self.doc.sql_for_run(&input)?;
        if !is_single_statement_for(db, &sql) {
            return Err(e("notebook.analysis.plan.single_statement"));
        }
        if actual {
            if !classify_select_only_for(db, &sql).select_only {
                return Err(e("notebook.analysis.plan.actual_read_only"));
            }
            if !self.analysis.authorized.remove(id) {
                self.set_plan_prompt(id, crate::notebook::cells::plan::Prompt::ConfirmActual, cx);
                return Err(e("notebook.analysis.plan.actual_needs_confirmation"));
            }
        }
        let opts = if actual {
            ExplainOptions::actual_defaults()
        } else {
            ExplainOptions::estimated_defaults()
        };
        let plan_sql = sqlly_services::plans::explain_sql_with_options(
            dialect,
            &opts,
            actual,
            &capture_input(dialect, actual, &sql),
        );
        let compare_sql = match &spec.compare {
            Some(PlanCompare::Cell(other)) => {
                let other = self
                    .resolve_cell(other)
                    .ok_or_else(|| e("notebook.analysis.input_missing"))?;
                let other_target = self.run_request(&other)?.target;
                if other_target.profile_id != target.profile_id
                    || other_target.local != target.local
                {
                    return Err(e("notebook.analysis.plan.compare_other_connection"));
                }
                let other_sql = self.doc.sql_for_run(&other)?;
                if !is_single_statement_for(db, &other_sql) {
                    return Err(e("notebook.analysis.plan.single_statement"));
                }
                if actual && !classify_select_only_for(db, &other_sql).select_only {
                    return Err(e("notebook.analysis.plan.actual_read_only"));
                }
                Some(sqlly_services::plans::explain_sql_with_options(
                    dialect,
                    &opts,
                    actual,
                    &capture_input(dialect, actual, &other_sql),
                ))
            }
            _ => None,
        };
        let stats = (spec.session_stats
            && actual
            && crate::capabilities::include_statistics(db).is_available())
        .then(|| session_stats_query(db))
        .flatten();
        self.send_request(
            id,
            target,
            AnalysisWork::Plan {
                sql: plan_sql,
                actual,
                dialect,
                compare_sql,
                stats_sql: stats.as_ref().map(|(_, sql)| sql.clone()),
            },
            Pending::Plan {
                spec,
                engine: db.display_name().to_string(),
                stats_source: stats.map(|(s, _)| s),
                input,
            },
            cx,
        );
        Ok(())
    }

    fn set_plan_prompt(
        &mut self,
        id: &str,
        prompt: crate::notebook::cells::plan::Prompt,
        cx: &mut Context<Self>,
    ) {
        if let Some(CellViewHandle::Plan(view)) = self.analysis.views.get(id).cloned() {
            view.update(cx, |v, cx| v.set_prompt(Some(prompt), cx));
        }
    }

    /// The user's yes for the next run of `id` (actual plan, production sweep).
    pub(crate) fn authorize_and_run(&mut self, id: &str, cx: &mut Context<Self>) {
        self.analysis.authorized.insert(id.to_string());
        self.request_run_from_host(id, cx);
    }

    fn previous_plans(&mut self, id: &str, run: Option<&str>) -> Option<Vec<ExecutionPlan>> {
        let runs = self.recorded_runs().to_vec();
        runs.iter()
            .rev()
            .filter(|r| run.is_none_or(|want| r.run_id == want))
            .find_map(|r| crate::notebook::history::details_of(r).plans.remove(id))
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_done(
        &mut self,
        id: &str,
        spec: PlanCellSpec,
        engine: String,
        stats_source: Option<StatsSource>,
        input: String,
        plans: Result<Vec<ExecutionPlan>, DisplayMessage>,
        compare: Option<Result<Vec<ExecutionPlan>, DisplayMessage>>,
        stats: Option<StatsPair>,
        cx: &mut Context<Self>,
    ) {
        let plans = match plans {
            Ok(p) => p,
            Err(error) => {
                self.finish_analysis(id, Err(error), cx);
                return;
            }
        };
        let baseline = match &spec.compare {
            Some(PlanCompare::Cell(_)) => compare,
            Some(PlanCompare::PreviousRun) => Some(
                self.previous_plans(id, None)
                    .ok_or_else(|| e("notebook.analysis.plan.no_previous_plan")),
            ),
            Some(PlanCompare::Run(run)) => Some(
                self.previous_plans(id, Some(run))
                    .ok_or_else(|| e("notebook.analysis.plan.no_previous_plan")),
            ),
            None => None,
        };
        let stat_rows: Vec<StatDelta> = match (stats_source, &stats) {
            (Some(source), Some(pair)) => stat_deltas(
                source,
                (&pair.before.columns, &pair.before.rows),
                (&pair.after.columns, &pair.after.rows),
            ),
            _ => Vec::new(),
        };
        let (columns, rows) = plan_table(&plans);
        self.analysis.outputs.insert(
            id.to_string(),
            inputs::derived_output(columns, rows, "", 0, Vec::new()),
        );
        self.analysis.history.note_plans(id, plans.clone());
        let result = PlanResult {
            actual: spec.mode == PlanMode::Actual,
            engine: engine.clone(),
            plans,
            baseline,
            stats_source,
            stats: stat_rows,
            sweep: Vec::new(),
            sweep_note: None,
        };
        if let Some(sweep) = spec.sweep.clone() {
            match self.start_sweep(id, &input, sweep, engine, result, cx) {
                Ok(()) => return,
                Err((result, message)) => {
                    self.show_plan(id, result, cx);
                    self.finish_analysis(id, Err(message), cx);
                    return;
                }
            }
        }
        self.show_plan(id, result, cx);
        self.finish_analysis(id, Ok(()), cx);
    }

    fn show_plan(&mut self, id: &str, result: PlanResult, cx: &mut Context<Self>) {
        if let Some(CellViewHandle::Plan(view)) = self.analysis.views.get(id).cloned() {
            view.update(cx, |v, cx| v.set_result(result, cx));
        }
    }

    #[allow(clippy::result_large_err)]
    fn start_sweep(
        &mut self,
        id: &str,
        input: &str,
        sweep: SweepSpec,
        engine: String,
        mut result: PlanResult,
        cx: &mut Context<Self>,
    ) -> Result<(), (PlanResult, DisplayMessage)> {
        let authorized = self.analysis.authorized.remove(id);
        let runs = if sweep.parameter.trim().is_empty() {
            0
        } else {
            sweep.total_runs()
        };
        let decision = self.decide(&NotebookAction::Benchmark {
            cell_id: input,
            runs,
            production_authorized: authorized,
        });
        let key = match &decision {
            Decision::Allow { .. } => None,
            Decision::Deny(Reason::BenchmarkNoRuns) => Some("notebook.analysis.sweep.no_values"),
            Decision::Deny(Reason::BenchmarkTooManyRuns { .. }) => {
                Some("notebook.analysis.sweep.too_many")
            }
            Decision::Deny(Reason::BenchmarkWritesData) => Some("notebook.analysis.sweep.writes"),
            Decision::NeedsApproval(Reason::BenchmarkProduction { .. }) => {
                self.set_plan_prompt(
                    id,
                    crate::notebook::cells::plan::Prompt::AuthorizeProductionSweep,
                    cx,
                );
                Some("notebook.analysis.sweep.production_needs_authorization")
            }
            Decision::Deny(reason) | Decision::NeedsApproval(reason) => {
                return Err((result, reason.message()));
            }
        };
        if let Some(key) = key {
            result.sweep_note = Some(key);
            return Err((result, e(key)));
        }
        let db = self.doc.database_type_for(input);
        let mut runs = Vec::new();
        for value in &sweep.values {
            let sql = self
                .doc
                .sql_for_run_with(input, &sweep.parameter, &param_text(value))
                .map_err(|text| (result.clone(), DisplayMessage::from(text)))?;
            if !classify_select_only_for(db, &sql).select_only {
                return Err((result, e("notebook.analysis.read_only_required")));
            }
            for _ in 0..sweep.repetitions() {
                runs.push(sql.clone());
            }
        }
        let stats = (result.stats_source.is_some()
            || (crate::capabilities::include_statistics(db).is_available()
                && PlanCellSpec::from_spec(&self.doc.spec(id)).session_stats))
            .then(|| session_stats_query(db))
            .flatten();
        let target = self
            .input_target(input, true)
            .map_err(|text| (result.clone(), DisplayMessage::from(text)))?;
        self.send_request(
            id,
            target,
            AnalysisWork::Sweep {
                runs,
                stats_sql: stats.as_ref().map(|(_, s)| s.clone()),
            },
            Pending::Sweep {
                sweep,
                engine,
                stats_source: stats.map(|(s, _)| s),
                plan: Box::new(result),
            },
            cx,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn sweep_done(
        &mut self,
        id: &str,
        sweep: SweepSpec,
        _engine: String,
        stats_source: Option<StatsSource>,
        mut plan: PlanResult,
        results: Vec<Result<(u64, Option<StatsPair>), DisplayMessage>>,
        cx: &mut Context<Self>,
    ) {
        let reps = sweep.repetitions() as usize;
        let mut first_error = None;
        for (i, value) in sweep.values.iter().enumerate() {
            let slice = results.iter().skip(i * reps).take(reps);
            let mut timings = Vec::new();
            let mut deltas: Vec<StatDelta> = Vec::new();
            let mut errors = 0;
            for r in slice {
                match r {
                    Ok((ms, stats)) => {
                        timings.push(*ms as f64);
                        if let (Some(source), Some(pair)) = (stats_source, stats) {
                            for d in stat_deltas(
                                source,
                                (&pair.before.columns, &pair.before.rows),
                                (&pair.after.columns, &pair.after.rows),
                            ) {
                                match deltas.iter_mut().find(|x| x.name == d.name) {
                                    Some(x) => x.after += d.delta(),
                                    None => deltas.push(StatDelta {
                                        name: d.name.clone(),
                                        before: 0.0,
                                        after: d.delta(),
                                    }),
                                }
                            }
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        first_error.get_or_insert_with(|| e.clone());
                    }
                }
            }
            plan.sweep.push(SweepPoint {
                value: param_text(value),
                timing: TimingStats::of(&timings),
                errors,
                stats: deltas,
            });
        }
        self.show_plan(id, plan, cx);
        let outcome = match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        };
        self.finish_analysis(id, outcome, cx);
    }

    // ── Entity detail ─────────────────────────────────────────────────────

    fn run_entity_detail(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), DisplayMessage> {
        let spec = EntityDetailSpec::from_spec(&self.doc.spec(id));
        if spec.table.name.is_empty() {
            return Err(e("notebook.analysis.entity.no_table"));
        }
        if spec.relationships.is_empty() {
            return Err(e("notebook.analysis.entity.no_relationships"));
        }
        let (key, db, target) = match &spec.source {
            EntitySource::Row { cell, row } => {
                let source = self
                    .resolve_cell(cell)
                    .ok_or_else(|| e("notebook.analysis.input_missing"))?;
                let input = inputs::cell_output(self, &source, cx).ok_or_else(|| {
                    crate::localization::deferred(
                        "notebook.analysis.input_no_result",
                        &[("cell", self.display_name(&source))],
                    )
                })?;
                let picked = input
                    .output
                    .rows
                    .get(*row)
                    .cloned()
                    .ok_or_else(|| e("notebook.analysis.entity.row_out_of_range"))?;
                (
                    (input.output.columns.clone(), picked),
                    self.doc.database_type_for(&source),
                    self.input_target(&source, true)?,
                )
            }
            EntitySource::Parameter { name, column } => {
                let resolved = self.doc.resolved();
                let value = resolved
                    .get(name)
                    .ok_or_else(|| e("notebook.analysis.entity.parameter_missing"))?;
                let profile_id = self
                    .doc
                    .notebook_connection()
                    .and_then(|s| s.profile_id.clone())
                    .and_then(|p| uuid::Uuid::parse_str(&p).ok());
                (
                    parameter_row(column, value),
                    self.doc.database_type(),
                    CellRunTarget {
                        profile_id,
                        read_only: true,
                        timeout_seconds: None,
                        local: false,
                    },
                )
            }
        };
        let limit = spec.limit();
        let mut queries = Vec::new();
        let mut labels = Vec::new();
        let mut skipped = Vec::new();
        for (i, rel) in spec
            .relationships
            .iter()
            .take(MAX_RELATIONSHIPS)
            .enumerate()
        {
            match related_rows_sql(db, rel, &key.0, &key.1, limit) {
                Ok(sql) => {
                    queries.push(sql);
                    labels.push(i);
                }
                Err(error) => skipped.push((i, detail_error(&error))),
            }
        }
        if queries.is_empty() {
            self.entity_done(id, labels, skipped, key, limit, Vec::new(), cx);
            return Ok(());
        }
        self.send_request(
            id,
            target,
            AnalysisWork::Rows {
                queries,
                max_rows: limit + 1,
            },
            Pending::Entity {
                labels,
                skipped,
                key,
                limit,
            },
            cx,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn entity_done(
        &mut self,
        id: &str,
        labels: Vec<usize>,
        skipped: Vec<(usize, DisplayMessage)>,
        key: (Vec<ResultColumn>, Vec<CellValue>),
        limit: usize,
        sets: Vec<Result<QueryRows, DisplayMessage>>,
        cx: &mut Context<Self>,
    ) {
        let mut related: Vec<RelatedRows> = Vec::new();
        for (index, set) in labels.into_iter().zip(sets) {
            related.push(match set {
                Ok(mut rows) => {
                    let truncated = rows.truncated || rows.rows.len() > limit;
                    rows.rows.truncate(limit);
                    RelatedRows {
                        index,
                        result: Ok((rows.columns, rows.rows)),
                        truncated,
                    }
                }
                Err(e) => RelatedRows {
                    index,
                    result: Err(e),
                    truncated: false,
                },
            });
        }
        for (index, message) in skipped {
            related.push(RelatedRows {
                index,
                result: Err(message),
                truncated: false,
            });
        }
        related.sort_by_key(|r| r.index);
        if let Some(CellViewHandle::EntityDetail(view)) = self.analysis.views.get(id).cloned() {
            view.update(cx, |v, cx| v.set_result(key, related, cx));
        }
        self.finish_analysis(id, Ok(()), cx);
    }
}

fn detail_error(error: &DetailQueryError) -> DisplayMessage {
    match error {
        DetailQueryError::MissingColumn(c) => crate::localization::deferred(
            "notebook.analysis.entity.missing_column",
            &[("column", c.as_str())],
        ),
        DetailQueryError::NullKey(c) => crate::localization::deferred(
            "notebook.analysis.entity.null_key",
            &[("column", c.as_str())],
        ),
        DetailQueryError::UnsupportedValue(c) => crate::localization::deferred(
            "notebook.analysis.entity.unsupported_value",
            &[("column", c.as_str())],
        ),
        DetailQueryError::NotReadOnly => e("notebook.analysis.read_only_required"),
    }
}
