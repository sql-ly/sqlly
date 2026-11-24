//! The notebook view's side of outputs and visual cells: snapshotting SQL
//! results into the output registry, computing local cells (in the run
//! queue and when their inputs change), building each visual cell's
//! inputs, and applying what visual cells ask for.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::{Hash, Hasher};

use gpui::{AppContext as _, Context, Entity, Window};
use sqlly_core::DisplayMessage;
use sqlly_services::analytics::decimal::Decimal;
use sqlly_services::analytics::scalar::ScalarValue;
use sqlly_services::notebook::cells::scalar as scalar_cell;
use sqlly_services::notebook::run_record::new_run_id;
use sqlly_services::notebook::state::StaleReason;
use sqlly_services::notebook::{
    CellEvent, CellKind, CellOutput, Fingerprint, ParamValue, RandomIds, SourceLanguage,
};

use super::{
    ColumnLineage, InputInfo, LineageEntry, VisualCellView, VisualEvent, VisualInputs, VisualKind,
    VisualSlot,
};
use crate::notebook::cells;
use crate::notebook::doc_adapter::KIND_SQL;
use crate::notebook::outputs::{
    DEFAULT_SNAPSHOT_ROW_CAP, ResultSource, derived_output, output_from_snapshot, provenance,
    read_results,
};
use crate::notebook::view::{NotebookEvent, NotebookView};

/// How deep a change ripples through local dependents in one go.
const MAX_REFRESH_DEPTH: usize = 64;

fn param_scalar(value: &ParamValue) -> ScalarValue {
    match value {
        ParamValue::Null => ScalarValue::Null,
        ParamValue::Text(t) | ParamValue::Identifier(t) => ScalarValue::Text(t.clone()),
        ParamValue::Integer(n) => ScalarValue::Integer(*n),
        ParamValue::Decimal(d) => match Decimal::parse(d) {
            Some(exact) => ScalarValue::exact(exact),
            None => d
                .parse::<f64>()
                .map(ScalarValue::Number)
                .unwrap_or_else(|_| ScalarValue::Text(d.clone())),
        },
        ParamValue::Boolean(b) => ScalarValue::Bool(*b),
        ParamValue::Date(d) => d
            .and_hms_opt(0, 0, 0)
            .map(ScalarValue::Date)
            .unwrap_or(ScalarValue::Null),
        ParamValue::DateTime(d) => ScalarValue::Date(*d),
        ParamValue::DateRange { start, .. } => start
            .and_hms_opt(0, 0, 0)
            .map(ScalarValue::Date)
            .unwrap_or(ScalarValue::Null),
        ParamValue::List(_) | ParamValue::SecretRef(_) => ScalarValue::Null,
    }
}

/// A scalar cell's value from its one-row output (exact numbers stay
/// exact).
pub(crate) fn scalar_of(output: &CellOutput) -> Option<ScalarValue> {
    scalar_cell::value_from_output(&output.columns, &output.rows)
}

impl NotebookView {
    fn cell_kind(&self, id: &str) -> Option<String> {
        self.doc.cell_by_id(id).map(|c| c.kind.to_string())
    }

    fn visual_kind(&self, id: &str) -> Option<VisualKind> {
        self.cell_kind(id).and_then(|k| VisualKind::from_kind(&k))
    }

    /// Whether runs compute `id` here rather than on a connection.
    pub(crate) fn runs_locally(&self, id: &str) -> bool {
        self.cell_kind(id)
            .is_some_and(|k| cells::renderer_for(&k).runs_locally())
    }

    /// The most rows `id`'s output keeps.
    pub(crate) fn output_cap(&self, id: &str) -> usize {
        self.doc
            .cell_by_id(id)
            .and_then(|c| c.policy.max_rows)
            .map(|m| m as usize)
            .unwrap_or(DEFAULT_SNAPSHOT_ROW_CAP)
            .max(1)
    }

    // ── SQL outputs ───────────────────────────────────────────────────────

    /// A SQL cell's run finished: snapshot its result into the registry
    /// (reading a spilled result off the UI thread first), then finish it.
    pub(crate) fn sql_cell_completed(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.drop_stopped_report(id) {
            cx.notify();
            return;
        }
        let Some(results) = self.cells.get(id).and_then(|rt| rt.results.clone()) else {
            self.cell_finished(id, true, cx);
            return;
        };
        let cap = self.output_cap(id);
        let weak = cx.weak_entity();
        let cell = id.to_string();
        let caption = crate::localization::format(
            "notebook.visual.outputs.reading_rows",
            &[("row_count", super::thousands(cap as u64))],
        );
        let deferred = results.update(cx, |r, cx| {
            r.defer_until_spill_prefixes(vec![(0, cap)], caption, cx, move |app| {
                weak.update(app, |nb, cx| nb.finish_sql_cell(&cell, cx))
                    .ok();
            })
        });
        if !deferred {
            self.finish_sql_cell(id, cx);
        }
    }

    pub(crate) fn finish_sql_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.read_full_values(id, self.output_cap(id), cx) {
            return;
        }
        let captured = self.capture_sql_output(id, cx);
        self.cell_finished(id, true, cx);
        if !captured {
            cx.notify();
            return;
        }
        if let Some(ms) = self.cells.get(id).and_then(|rt| rt.elapsed_ms)
            && let Some(output) = self.outputs.get(id)
        {
            let mut output = (*output).clone();
            output.elapsed_ms = ms;
            self.outputs.publish(id, output);
        }
        self.output_published(id, cx);
    }

    /// Snapshot `id`'s result into the registry. A run that returned no
    /// result set leaves no output: the previous run's rows are not this
    /// run's, so they are dropped rather than re-presented as current.
    fn capture_sql_output(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(results) = self.cells.get(id).and_then(|rt| rt.results.clone()) else {
            self.outputs.remove(id);
            return false;
        };
        let cap = self.output_cap(id);
        let (snapshot, sql) = {
            let r = results.read(cx);
            (read_results(r, cap), r.query_sql().map(str::to_string))
        };
        let Some(mut snapshot) = snapshot else {
            self.outputs.remove(id);
            return false;
        };
        if let Some(rows) = self.full_value_rows(id) {
            snapshot.rows = (*rows).clone();
            snapshot.spilled = false;
        }
        let (choice, _) = self.cell_connection(id);
        let slot_database = self
            .doc
            .effective_connection(id)
            .and_then(|slot| slot.database.clone());
        let source = ResultSource {
            connection_label: choice.as_ref().map(|c| c.label.clone()).unwrap_or_default(),
            database: slot_database.or_else(|| choice.as_ref().and_then(|c| c.database.clone())),
            environment: choice.as_ref().map(|c| c.environment),
            sql: sql.map(|ran| self.doc.history_sql_for_run(id).unwrap_or(ran)),
        };
        let rt = self.cells.get(id);
        let run_id = rt
            .and_then(|rt| rt.run_id.clone())
            .unwrap_or_else(|| new_run_id(&mut RandomIds));
        let fingerprint = rt
            .and_then(|rt| rt.run_fingerprint.as_ref().map(|f| f.value.clone()))
            .or_else(|| self.doc.cell_fingerprint(id).map(|f| f.value))
            .unwrap_or_else(|| Fingerprint(String::new()));
        let mut output = output_from_snapshot(
            snapshot,
            cap,
            run_id,
            fingerprint,
            provenance(&source, chrono::Utc::now()),
            0,
        );
        if output.completeness.supports_summaries() && self.binds_partial_scalar(id) {
            output.completeness = sqlly_services::notebook::Completeness::DerivedFromPartial;
        }
        self.outputs.publish(id, output);
        true
    }

    /// The first input of `id` whose output is not current.
    fn stale_input(&self, id: &str) -> Option<String> {
        let cell = self.doc.model().cell(id)?;
        cell.spec_inputs()
            .into_iter()
            .find(|input| !self.doc.cell_state(input).is_presentable_as_current())
    }

    /// A local cell just computed from its inputs is only as current as
    /// they are: built on one that is not, it is marked stale too.
    fn mark_if_built_on_stale(&mut self, id: &str) {
        if let Some(input) = self.stale_input(id) {
            self.doc.apply(
                id,
                CellEvent::Invalidated {
                    reason: StaleReason::UpstreamInvalidated { cell: input },
                },
            );
        }
    }

    // ── Local cells ───────────────────────────────────────────────────────

    /// Run a local cell the queue reached.
    pub(crate) fn run_local_cell(&mut self, id: &str, cx: &mut Context<Self>) {
        self.mark_started(id);
        let Some(kind) = self.visual_kind(id) else {
            self.local_failed(
                id,
                crate::localization::deferred_text("notebook.visual.errors.unsupported"),
                cx,
            );
            return;
        };
        if kind == VisualKind::Transform {
            self.run_transform(id, cx);
            return;
        }
        match self.evaluate_visual(id, kind, cx) {
            Ok(output) => {
                self.outputs.publish(id, output);
                if let Some(rt) = self.cells.get_mut(id) {
                    rt.message = None;
                }
                self.cell_finished(id, true, cx);
                self.mark_if_built_on_stale(id);
                self.output_published(id, cx);
            }
            Err(message) => self.local_failed(id, message, cx),
        }
    }

    fn local_failed(
        &mut self,
        id: &str,
        message: impl Into<DisplayMessage>,
        cx: &mut Context<Self>,
    ) {
        if let Some(rt) = self.cells.get_mut(id) {
            rt.message = Some(message.into());
        }
        self.remeasure_cell(id);
        self.cell_finished(id, false, cx);
    }

    fn evaluate_visual(
        &self,
        id: &str,
        kind: VisualKind,
        cx: &gpui::App,
    ) -> Result<CellOutput, DisplayMessage> {
        let inputs = self.visual_inputs(id, kind, false, cx);
        let (columns, rows) = super::evaluate(kind, &inputs)?;
        let used: Vec<&CellOutput> = inputs
            .input
            .iter()
            .chain(&inputs.others)
            .filter_map(|i| i.output.as_deref())
            .collect();
        let run_id = self
            .cells
            .get(id)
            .and_then(|rt| rt.run_id.clone())
            .unwrap_or_else(|| new_run_id(&mut RandomIds));
        let fingerprint = self
            .doc
            .cell_fingerprint(id)
            .map(|f| f.value)
            .unwrap_or_else(|| Fingerprint(String::new()));
        Ok(derived_output(columns, rows, &used, run_id, fingerprint))
    }

    fn run_transform(&mut self, id: &str, cx: &mut Context<Self>) {
        let inputs = self.visual_inputs(id, VisualKind::Transform, false, cx);
        let job = match super::transform::prepare_job(&inputs) {
            Ok(job) => job,
            Err(message) => {
                self.local_failed(id, message, cx);
                return;
            }
        };
        let used: Vec<CellOutput> = inputs
            .input
            .iter()
            .chain(&inputs.others)
            .filter_map(|i| i.output.as_deref().cloned())
            .collect();
        let mut job = job;
        job.max_rows = job.max_rows.min(self.output_cap(id));
        let cell = id.to_string();
        #[cfg(not(target_arch = "wasm32"))]
        self.with_workspace(cx, move |this, workspace, cx| {
            let workspace = match workspace {
                Ok(workspace) => workspace,
                Err(message) => return this.local_failed(&cell, message, cx),
            };
            let relation = this
                .doc
                .local_relation(&cell)
                .unwrap_or_else(|| cell.clone());
            let (job_id, _progress, cancel) = this.new_job(Some(&cell), &relation);
            this.tick_local_jobs(cx);
            let task =
                cx.background_spawn(
                    async move { super::transform::execute(job, &workspace, &cancel) },
                );
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |nb, cx| {
                    nb.local.jobs.retain(|j| j.id != job_id);
                    nb.finish_transform(&cell, result, used, cx)
                })
                .ok();
            })
            .detach();
        });
        #[cfg(target_arch = "wasm32")]
        {
            let result = super::transform::execute(job);
            self.finish_transform(&cell, result, used, cx);
        }
    }

    fn finish_transform(
        &mut self,
        id: &str,
        result: Result<super::transform::TransformResult, sqlly_core::DisplayMessage>,
        used: Vec<CellOutput>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(done) => {
                let refs: Vec<&CellOutput> = used.iter().collect();
                let run_id = self
                    .cells
                    .get(id)
                    .and_then(|rt| rt.run_id.clone())
                    .unwrap_or_else(|| new_run_id(&mut RandomIds));
                let fingerprint = self
                    .doc
                    .cell_fingerprint(id)
                    .map(|f| f.value)
                    .unwrap_or_else(|| Fingerprint(String::new()));
                let mut output =
                    derived_output(done.columns, done.rows, &refs, run_id, fingerprint);
                if done.truncated {
                    output.completeness = sqlly_services::notebook::Completeness::Truncated {
                        cap: output.rows.len() as u64,
                    };
                }
                self.outputs.publish(id, output);
                if let Some(rt) = self.cells.get_mut(id) {
                    rt.message = None;
                }
                self.cell_finished(id, true, cx);
                self.mark_if_built_on_stale(id);
                self.output_published(id, cx);
            }
            Err(message) => self.local_failed(id, message, cx),
        }
    }

    /// `id` published a new output: recompute the instant local cells that
    /// read it (and theirs), refresh scalar values, repaint.
    pub(crate) fn output_published(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        self.refresh_dependents(id, &mut seen, 0, cx);
        self.refresh_scalar_values();
        cx.notify();
    }

    fn refresh_dependents(
        &mut self,
        id: &str,
        seen: &mut HashSet<String>,
        depth: usize,
        cx: &mut Context<Self>,
    ) {
        if depth > MAX_REFRESH_DEPTH {
            return;
        }
        for dependent in self.doc.dependents(id) {
            if !seen.insert(dependent.clone()) {
                continue;
            }
            let Some(kind) = self.visual_kind(&dependent) else {
                continue;
            };
            if !kind.is_instant() || self.doc.status(&dependent).is_active() {
                continue;
            }
            let run_id = new_run_id(&mut RandomIds);
            self.doc.apply(
                &dependent,
                CellEvent::Started {
                    run_id: run_id.clone(),
                },
            );
            self.cells.entry(dependent.clone()).or_default().run_id = Some(run_id.clone());
            match self.evaluate_visual(&dependent, kind, cx) {
                Ok(output) => {
                    let (rows, completeness) = (output.row_count, output.completeness);
                    self.outputs.publish(&dependent, output);
                    if let Some(fp) = self.doc.cell_fingerprint(&dependent) {
                        self.doc
                            .complete_with(&dependent, &run_id, fp, rows, completeness);
                    }
                    if let Some(rt) = self.cells.get_mut(&dependent) {
                        rt.message = None;
                        rt.run_id = None;
                    }
                    self.mark_if_built_on_stale(&dependent);
                    self.refresh_dependents(&dependent, seen, depth + 1, cx);
                }
                Err(message) => {
                    self.doc.apply(
                        &dependent,
                        CellEvent::Failed {
                            error: message.clone(),
                        },
                    );
                    if let Some(rt) = self.cells.get_mut(&dependent) {
                        rt.message = Some(message);
                        rt.run_id = None;
                    }
                }
            }
            self.remeasure_cell(&dependent);
        }
    }

    /// Hand the scalar cells' current values to the document so SQL that
    /// references them binds them as literals.
    pub(crate) fn refresh_scalar_values(&mut self) {
        let values: BTreeMap<String, ScalarValue> = self
            .doc
            .cells()
            .filter(|c| c.kind == "scalar")
            .filter_map(|c| {
                let output = self.outputs.get(c.id)?;
                Some((c.id.to_string(), scalar_of(&output)?))
            })
            .collect();
        self.doc.set_scalar_values(values);
    }

    /// Whether SQL cell `id` binds a scalar (directly or through a SQL cell
    /// it references) whose value was computed from partial data, so its
    /// result is not the whole truth either.
    pub(crate) fn binds_partial_scalar(&self, id: &str) -> bool {
        let partial: BTreeSet<String> = self
            .doc
            .cells()
            .filter(|c| c.kind == "scalar")
            .filter(|c| {
                self.outputs
                    .get(c.id)
                    .is_some_and(|o| scalar_cell::is_partial(o.completeness))
            })
            .map(|c| c.id.to_string())
            .collect();
        !partial.is_empty()
            && scalar_cell::cells_binding_partial(self.doc.model(), &self.doc.dialects(), &partial)
                .contains(id)
    }

    /// A formula cell's current value as text, formatted as the cell shows
    /// it; `None` for other kinds or before it has a value.
    pub(crate) fn scalar_display(&self, id: &str) -> Option<String> {
        let c = self
            .doc
            .model()
            .cell(id)
            .filter(|c| c.kind == CellKind::Scalar)?;
        let output = self.outputs.get(&c.id)?;
        let value = scalar_of(&output)?;
        let spec: sqlly_services::notebook::cells::scalar::ScalarCellSpec =
            sqlly_services::notebook::cells::spec::read(&c.spec);
        Some(sqlly_services::notebook::cells::scalar::display(
            &value,
            spec.format.as_ref(),
        ))
    }

    /// Markdown with `{{scalar}}` and `{{param.name}}` filled in, or `None`
    /// when it references neither.
    pub(crate) fn interpolate_markdown(&self, id: &str) -> Option<String> {
        let cell = self.doc.cell_by_id(id)?;
        if !cell.source.contains("{{") {
            return None;
        }
        let refs = sqlly_services::notebook::references::find_references(
            cell.source,
            SourceLanguage::Markdown,
            self.doc.database_type(),
        );
        let params = self.doc.resolved();
        let mut out = String::new();
        let mut last = 0;
        let mut changed = false;
        for r in refs {
            let text = match &r.target {
                sqlly_services::notebook::references::RefTarget::Cell(token) => self
                    .doc
                    .model()
                    .resolve(token)
                    .and_then(|c| self.scalar_display(&c.id))
                    // Data, like parameter text: never Markdown.
                    .map(|text| sqlly_services::notebook::cells::parameter::escape_markdown(&text)),
                sqlly_services::notebook::references::RefTarget::Param { name, field } => {
                    sqlly_services::notebook::cells::parameter::param_text(
                        self.doc.param_defs(),
                        &params,
                        name,
                        field.as_deref(),
                    )
                }
            };
            if let Some(text) = text {
                out.push_str(&cell.source[last..r.start]);
                out.push_str(&text);
                last = r.end;
                changed = true;
            }
        }
        if !changed {
            return None;
        }
        out.push_str(&cell.source[last..]);
        Some(out)
    }

    // ── Visual cell views ─────────────────────────────────────────────────

    /// A cheap hash of what `id`'s visual inputs depend on.
    fn visual_key(&self, id: &str, kind: VisualKind, with_columns: bool) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        kind.hash(&mut h);
        with_columns.hash(&mut h);
        self.doc.len().hash(&mut h);
        self.outputs.generation().hash(&mut h);
        self.outputs.filter_generation().hash(&mut h);
        format!("{:?}", self.doc.resolved().values).hash(&mut h);
        let Some(cell) = self.doc.model().cell(id) else {
            return h.finish();
        };
        cell.spec.to_string().hash(&mut h);
        cell.source.hash(&mut h);
        cell.name.hash(&mut h);
        let mut related = cell.spec_inputs();
        related.push(id.to_string());
        for other in &related {
            format!("{:?}", self.doc.status(other)).hash(&mut h);
            self.is_stale(other).hash(&mut h);
            self.doc.shown_fingerprint(other).hash(&mut h);
            if let Some(c) = self.doc.cell_by_id(other) {
                c.display_name().hash(&mut h);
            }
        }
        self.cells
            .get(id)
            .and_then(|rt| rt.message_text())
            .hash(&mut h);
        for c in self.doc.cells() {
            if VisualKind::from_kind(c.kind).is_some() || c.kind == KIND_SQL {
                c.display_name().hash(&mut h);
            }
        }
        h.finish()
    }

    fn input_info(&self, id: &str, cx: &gpui::App) -> Option<InputInfo> {
        let cell = self.doc.cell_by_id(id)?;
        let state = self.doc.cell_state(id);
        let output = self.outputs.get(id).or_else(|| {
            crate::notebook::analysis::is_analysis_kind(cell.kind)
                .then(|| crate::notebook::analysis::inputs::cell_output(self, id, cx))
                .flatten()
                .map(|data| std::sync::Arc::new(data.output))
        });
        let sql = if cell.kind == KIND_SQL {
            self.doc
                .sql_for_run(id)
                .ok()
                .or_else(|| Some(cell.source.to_string()))
        } else {
            None
        };
        Some(InputInfo {
            id: id.to_string(),
            name: cell.display_name().to_string(),
            current: state.is_presentable_as_current() && output.is_some(),
            output,
            version: self.outputs.version(id),
            status: state.status,
            sql,
        })
    }

    /// Everything `id`'s view draws from.
    pub(crate) fn visual_inputs(
        &self,
        id: &str,
        kind: VisualKind,
        with_columns: bool,
        cx: &gpui::App,
    ) -> VisualInputs {
        let Some(cell) = self.doc.model().cell(id) else {
            return VisualInputs::default();
        };
        let index = self.doc.index_of(id).unwrap_or(0);
        let input_id = cell
            .spec
            .get("input")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let input = input_id.as_deref().and_then(|i| self.input_info(i, cx));
        let others: Vec<InputInfo> = cell
            .spec_inputs()
            .into_iter()
            .filter(|o| Some(o) != input_id.as_ref())
            .filter_map(|o| self.input_info(&o, cx))
            .collect();
        let candidates = self
            .doc
            .cells()
            .take(index)
            .filter(|c| {
                c.kind == KIND_SQL
                    || VisualKind::from_kind(c.kind).is_some()
                    || crate::notebook::analysis::is_analysis_kind(c.kind)
            })
            .filter(|c| c.kind != "scalar")
            .map(|c| (c.id.to_string(), c.display_name().to_string()))
            .collect();
        let charts = self
            .doc
            .cells()
            .filter(|c| c.kind == "chart" && c.id != id)
            .map(|c| (c.id.to_string(), c.display_name().to_string()))
            .collect();
        let filter = cell
            .spec
            .get("follows")
            .and_then(|v| v.as_str())
            .and_then(|source| self.outputs.filter(source))
            .cloned();
        let own_filter = self.outputs.filter(id).cloned();
        let named_outputs = self
            .doc
            .model()
            .cells
            .iter()
            .filter(|c| c.id != id)
            .filter_map(|c| Some((c.relation_name().to_string(), self.outputs.get(&c.id)?)))
            .collect();
        let mut values = BTreeMap::new();
        // Sensitive and secret parameters stay out of formulas: a scalar cell
        // shows its value, so reading one would put it on screen.
        let redacted: HashSet<&str> = self
            .doc
            .param_defs()
            .iter()
            .filter(|d| d.is_redacted())
            .map(|d| d.name.as_str())
            .collect();
        for (name, value) in &self.doc.resolved().values {
            if !redacted.contains(name.as_str()) {
                values.insert(format!("param.{name}"), param_scalar(value));
            }
        }
        for c in &self.doc.model().cells {
            if c.kind == CellKind::Scalar
                && c.id != id
                && let Some(output) = self.outputs.get(&c.id)
                && let Some(value) = scalar_of(&output)
            {
                values.insert(c.relation_name().to_string(), value);
            }
        }
        let mut lineage = Vec::new();
        let mut seen = HashSet::new();
        let mut frontier: Vec<(String, usize)> = self
            .doc
            .dependencies(id)
            .into_iter()
            .map(|d| (d, 0))
            .collect();
        frontier.reverse();
        while let Some((up, depth)) = frontier.pop() {
            if !seen.insert(up.clone()) || lineage.len() >= 200 {
                continue;
            }
            if let Some(c) = self.doc.cell_by_id(&up) {
                lineage.push(LineageEntry {
                    id: up.clone(),
                    name: c.display_name().to_string(),
                    kind: crate::localization::text(cells::renderer_for(c.kind).label_key()),
                    depth,
                });
            }
            let mut next: Vec<(String, usize)> = self
                .doc
                .dependencies(&up)
                .into_iter()
                .map(|d| (d, depth + 1))
                .collect();
            next.reverse();
            frontier.extend(next);
        }
        let columns = if with_columns {
            column_lineage(input.as_ref())
        } else {
            Vec::new()
        };
        let mut inputs = VisualInputs {
            cell_id: id.to_string(),
            name: cell.name.clone().unwrap_or_else(|| cell.id.clone()),
            spec: cell.spec.clone(),
            source: cell.source.clone(),
            db: self.doc.database_type(),
            input,
            others,
            candidates,
            charts,
            filter: if kind.can_follow() { filter } else { None },
            own_filter,
            named_outputs,
            values,
            lineage,
            columns,
            own_output: self.outputs.get(id),
            own_status: self.doc.status(id),
            message: self.cells.get(id).and_then(|rt| rt.message_text()),
            key: 0,
        };
        inputs.compute_key();
        inputs
    }

    /// The view for visual cell `id` (made on first draw), with its inputs
    /// brought up to date.
    pub(crate) fn prepare_visual(
        &mut self,
        id: &str,
        kind: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<VisualCellView>> {
        let Some(kind) = VisualKind::from_kind(kind) else {
            self.visual.slots.remove(id);
            return None;
        };
        let with_columns = self
            .visual
            .slots
            .get(id)
            .filter(|slot| slot.kind == kind)
            .is_some_and(|slot| slot.view.read(cx).panel == Some(super::InfoPanel::Lineage));
        let key = self.visual_key(id, kind, with_columns);
        if let Some(slot) = self.visual.slots.get(id).filter(|s| s.kind == kind) {
            if slot.key == key {
                return Some(slot.view.clone());
            }
            let view = slot.view.clone();
            let inputs = self.visual_inputs(id, kind, with_columns, cx);
            view.update(cx, |v, cx| v.set_inputs(inputs, cx));
            if let Some(slot) = self.visual.slots.get_mut(id) {
                slot.key = key;
            }
            return Some(view);
        }
        let inputs = self.visual_inputs(id, kind, with_columns, cx);
        let view = cx.new(|_| VisualCellView::new(kind, inputs));
        let cell = id.to_string();
        let subscription = cx.subscribe(&view, move |this, _view, event: &VisualEvent, cx| {
            this.on_visual_event(&cell, event, cx)
        });
        self.visual.slots.insert(
            id.to_string(),
            VisualSlot {
                view: view.clone(),
                kind,
                key,
                _subscription: subscription,
            },
        );
        self.default_visual_input(id, kind, cx);
        Some(view)
    }

    /// A new visual cell reads the nearest result above it.
    fn default_visual_input(&mut self, id: &str, kind: VisualKind, cx: &mut Context<Self>) {
        if !kind.has_input() {
            return;
        }
        let Some(cell) = self.doc.model().cell(id) else {
            return;
        };
        if cell.spec.get("input").is_some() {
            return;
        }
        let index = self.doc.index_of(id).unwrap_or(0);
        let nearest = self
            .doc
            .cells()
            .take(index)
            .filter(|c| {
                c.kind == KIND_SQL || VisualKind::from_kind(c.kind).is_some_and(|k| k.has_input())
            })
            .last()
            .map(|c| c.id.to_string());
        if let Some(nearest) = nearest {
            let mut spec = match &cell.spec {
                serde_json::Value::Object(_) => cell.spec.clone(),
                _ => serde_json::json!({}),
            };
            spec["input"] = serde_json::Value::String(nearest);
            if self.doc.set_spec(id, spec) {
                self.mark_dirty(cx);
            }
        }
    }

    pub(crate) fn on_visual_event(
        &mut self,
        id: &str,
        event: &VisualEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            VisualEvent::SpecChanged(spec) => {
                if self.doc.set_spec(id, spec.clone()) {
                    self.visual_changed(id, cx);
                }
            }
            VisualEvent::SourceChanged { source, spec } => {
                let mut spec = spec.clone();
                if self.visual_kind(id) == Some(VisualKind::Scalar) {
                    let mut typed: sqlly_services::notebook::cells::scalar::ScalarCellSpec =
                        sqlly_services::notebook::cells::spec::read(&spec);
                    if sqlly_services::notebook::cells::scalar::sync_inputs(
                        self.doc.model(),
                        &mut typed,
                        source,
                    )
                    .is_ok()
                    {
                        spec = sqlly_services::notebook::cells::spec::write(&typed);
                    }
                }
                let a = self.doc.set_source(id, source);
                let b = self.doc.set_spec(id, spec);
                if a || b {
                    self.visual_changed(id, cx);
                }
            }
            VisualEvent::Filter { source, filter } => {
                match filter {
                    Some(filter) => self.outputs.set_filter(filter.clone()),
                    None => {
                        self.outputs.clear_filter(source);
                    }
                }
                self.refilter_followers(source, cx);
                cx.notify();
            }
            VisualEvent::OpenInEditor(sql) => {
                cx.emit(NotebookEvent::OpenSqlInEditor { sql: sql.clone() })
            }
            VisualEvent::RowSelected(row) => self.table_row_selected(id, *row, cx),
            VisualEvent::Run => self.request_run_from_host(id, cx),
            VisualEvent::Remeasure => {
                self.remeasure_cell(id);
                cx.notify();
            }
            VisualEvent::Status { message, error } => cx.emit(NotebookEvent::Status {
                message: message.clone(),
                error: *error,
            }),
        }
    }

    /// A visual cell's settings changed: save, and recompute it when it is
    /// instant and has an input to read.
    fn visual_changed(&mut self, id: &str, cx: &mut Context<Self>) {
        self.mark_dirty(cx);
        self.remeasure_cell(id);
        let Some(kind) = self.visual_kind(id) else {
            return;
        };
        if kind.is_instant()
            && !self.doc.status(id).is_active()
            && self.evaluate_visual(id, kind, cx).is_ok()
        {
            self.recompute_local(id, kind, cx);
        }
    }

    fn recompute_local(&mut self, id: &str, kind: VisualKind, cx: &mut Context<Self>) {
        let run_id = new_run_id(&mut RandomIds);
        self.doc.apply(
            id,
            CellEvent::Started {
                run_id: run_id.clone(),
            },
        );
        self.cells.entry(id.to_string()).or_default().run_id = Some(run_id.clone());
        match self.evaluate_visual(id, kind, cx) {
            Ok(output) => {
                let (rows, completeness) = (output.row_count, output.completeness);
                self.outputs.publish(id, output);
                if let Some(fp) = self.doc.cell_fingerprint(id) {
                    self.doc.complete_with(id, &run_id, fp, rows, completeness);
                }
                if let Some(rt) = self.cells.get_mut(id) {
                    rt.message = None;
                    rt.run_id = None;
                }
                self.output_published(id, cx);
            }
            Err(message) => {
                self.doc.apply(
                    id,
                    CellEvent::Failed {
                        error: message.clone(),
                    },
                );
                if let Some(rt) = self.cells.get_mut(id) {
                    rt.message = Some(message);
                    rt.run_id = None;
                }
            }
        }
    }

    /// Recompute, locally, the cells following chart `source`. Never runs a
    /// query: followers read rows already in the registry.
    fn refilter_followers(&mut self, source: &str, cx: &mut Context<Self>) {
        let followers: Vec<(String, VisualKind)> = self
            .doc
            .model()
            .cells
            .iter()
            .filter(|c| c.spec.get("follows").and_then(|v| v.as_str()) == Some(source))
            .filter_map(|c| Some((c.id.clone(), VisualKind::from_kind(c.kind.as_str())?)))
            .collect();
        for (id, kind) in followers {
            if kind.is_instant()
                && !self.doc.status(&id).is_active()
                && self.outputs.get(&id).is_some()
            {
                self.recompute_local(&id, kind, cx);
            }
            self.remeasure_cell(&id);
        }
    }
}

/// Where each of the input's columns comes from, when its SQL is a plain
/// SELECT whose column resolves to one base-table column.
fn column_lineage(input: Option<&InputInfo>) -> Vec<ColumnLineage> {
    let Some(input) = input else {
        return Vec::new();
    };
    let Some(output) = &input.output else {
        return Vec::new();
    };
    let provenance = input
        .sql
        .as_deref()
        .and_then(sqlly_intel::query::column_provenance::StatementProvenance::parse);
    output
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let occurrences = output.columns[..=index]
                .iter()
                .filter(|c| c.name.eq_ignore_ascii_case(&column.name))
                .count();
            let origin = provenance
                .as_ref()
                .and_then(|p| p.resolve_origin(&column.name, index, occurrences))
                .map(|o| {
                    let mut parts = o.table.clone();
                    parts.push(o.column);
                    parts.join(".")
                });
            ColumnLineage {
                column: column.name.clone(),
                origin,
            }
        })
        .collect()
}
