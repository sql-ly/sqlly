//! Assertion (test) cells: a list of checks with an editor per kind, each
//! shown after a run as pass / warning / fail / error / inconclusive with
//! the observed figures and a grid of sample failing rows.

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputState};
use serde_json::Value;
use sqlly_datatable::SqllyDataTable;
use sqlly_services::analytics::assertions::{
    AssertionIssue, AssertionOutcome, AssertionSpec, Metric, Severity,
};
use sqlly_services::notebook::cells::assertion::{
    ASSERTION_KINDS, AssertionCellSpec, AssertionRun, BaselineRef, TestSummary, kind_of,
    new_assertion,
};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::analysis::ui::{self, body_text, caption, chip, field, picker, t};
use crate::notebook::history::rail::{status_color, status_key};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct AssertionCell;

impl NotebookCell for AssertionCell {
    fn kind(&self) -> &'static str {
        crate::notebook::analysis::KIND_ASSERTION
    }

    fn label_key(&self) -> &'static str {
        "notebook.analysis.kind.assertion"
    }

    fn runnable(&self) -> bool {
        true
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        _cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        ui::cell_body(cell)
    }
}

/// How an editor field's text becomes JSON.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldType {
    Text,
    /// Text that is left out of the spec when empty.
    OptionalText,
    Unsigned,
    Number,
    List,
}

/// The editable fields of each assertion kind: `(json key, type)`.
pub(crate) fn fields(kind: &str) -> &'static [(&'static str, FieldType)] {
    use FieldType::*;
    match kind {
        "row_count" => &[("min", Unsigned), ("max", Unsigned)],
        "unique" => &[("columns", List)],
        "not_null" => &[("column", Text), ("max_null_rate", Number)],
        "value_range" => &[("column", Text), ("min", Number), ("max", Number)],
        "accepted_values" => &[("column", Text), ("values", List)],
        "regex" => &[("column", Text), ("pattern", Text)],
        "referential" => &[("column", Text), ("parent", Text), ("parent_column", Text)],
        "distribution_drift" => &[("column", Text), ("warn", Number), ("fail", Number)],
        "freshness" => &[
            ("column", Text),
            ("max_age_seconds", Unsigned),
            ("warn_age_seconds", Unsigned),
            ("time_zone", OptionalText),
        ],
        "duration" => &[("max_ms", Unsigned), ("warn_ms", Unsigned)],
        "kpi_threshold" => &[("threshold", Number)],
        _ => &[],
    }
}

/// A field's current value as editor text.
pub(crate) fn field_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// `text` parsed for a field, or `None` when it does not parse.
pub(crate) fn parse_field(ty: FieldType, text: &str) -> Option<Value> {
    let text = text.trim();
    match ty {
        FieldType::Text => Some(Value::String(text.to_string())),
        FieldType::List => Some(Value::Array(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect(),
        )),
        _ if text.is_empty() => Some(Value::Null),
        FieldType::OptionalText => Some(Value::String(text.to_string())),
        FieldType::Unsigned => text.parse::<u64>().ok().map(Value::from),
        FieldType::Number => text
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(Value::from),
    }
}

/// `spec` with field `key` set to `value`, when the result is still valid.
pub(crate) fn with_field(spec: &AssertionSpec, key: &str, value: Value) -> Option<AssertionSpec> {
    let mut json = serde_json::to_value(spec).ok()?;
    let map = json.as_object_mut()?;
    if value.is_null() {
        map.remove(key);
    } else {
        map.insert(key.to_string(), value);
    }
    serde_json::from_value(json).ok()
}

fn issue_text(issue: &AssertionIssue) -> String {
    match issue {
        AssertionIssue::IncompleteInput { .. } => {
            t("notebook.analysis.assertion.issue.incomplete_input")
        }
        AssertionIssue::IncompleteParent { .. } => {
            t("notebook.analysis.assertion.issue.incomplete_parent")
        }
        AssertionIssue::IncompleteBaseline { .. } => {
            t("notebook.analysis.assertion.issue.incomplete_baseline")
        }
        AssertionIssue::MissingColumn { column } => crate::localization::format(
            "notebook.analysis.assertion.issue.missing_column",
            &[("column", column)],
        ),
        AssertionIssue::MissingOutput { name } => crate::localization::format(
            "notebook.analysis.assertion.issue.missing_output",
            &[("name", name)],
        ),
        AssertionIssue::MissingBaseline => t("notebook.analysis.assertion.issue.missing_baseline"),
        AssertionIssue::MissingBaselineColumn { column } => crate::localization::format(
            "notebook.analysis.assertion.issue.missing_baseline_column",
            &[("column", column)],
        ),
        AssertionIssue::UnsupportedMetric => {
            t("notebook.analysis.assertion.issue.unsupported_metric")
        }
        AssertionIssue::NoClock => t("notebook.analysis.assertion.issue.no_clock"),
        AssertionIssue::NoDuration => t("notebook.analysis.assertion.issue.no_duration"),
        AssertionIssue::InvalidPattern { detail } => crate::localization::format(
            "notebook.analysis.assertion.issue.invalid_pattern",
            &[("detail", detail)],
        ),
        AssertionIssue::PatternTooLong { max_chars } => crate::localization::format(
            "notebook.analysis.assertion.issue.pattern_too_long",
            &[("max_chars", &max_chars.to_string())],
        ),
        AssertionIssue::NoTimestamps => t("notebook.analysis.assertion.issue.no_timestamps"),
        AssertionIssue::NoNumericValues => t("notebook.analysis.assertion.issue.no_numeric_values"),
        AssertionIssue::Kpi { .. } => t("notebook.analysis.assertion.issue.kpi"),
        AssertionIssue::InvalidTimeZone { zone } => crate::localization::format(
            "notebook.analysis.assertion.issue.invalid_time_zone",
            &[("zone", zone)],
        ),
        AssertionIssue::FutureTimestamp => t("notebook.analysis.assertion.issue.future_timestamp"),
    }
}

fn metric_label(name: &str) -> String {
    metric_key(name).map_or_else(|| name.replace('_', " "), t)
}

fn metric_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "value" | "measured" => "notebook.analysis.assertion.metric.measured",
        "age_seconds" => "notebook.analysis.assertion.metric.age_seconds",
        "baseline_mean" => "notebook.analysis.assertion.metric.baseline_mean",
        "baseline_values" => "notebook.analysis.assertion.metric.baseline_values",
        "current_mean" => "notebook.analysis.assertion.metric.current_mean",
        "current_values" => "notebook.analysis.assertion.metric.current_values",
        "drift" => "notebook.analysis.assertion.metric.drift",
        "duplicate_keys" => "notebook.analysis.assertion.metric.duplicate_keys",
        "duplicate_rows" => "notebook.analysis.assertion.metric.duplicate_rows",
        "elapsed_ms" => "notebook.analysis.assertion.metric.elapsed_ms",
        "metric" => "notebook.analysis.assertion.metric.metric",
        "non_numeric" => "notebook.analysis.assertion.metric.non_numeric",
        "null_count" => "notebook.analysis.assertion.metric.null_count",
        "null_key_rows" => "notebook.analysis.assertion.metric.null_key_rows",
        "null_rate" => "notebook.analysis.assertion.metric.null_rate",
        "observed_max" => "notebook.analysis.assertion.metric.observed_max",
        "observed_min" => "notebook.analysis.assertion.metric.observed_min",
        "orphan_rows" => "notebook.analysis.assertion.metric.orphan_rows",
        "orphan_values" => "notebook.analysis.assertion.metric.orphan_values",
        "parent_keys" => "notebook.analysis.assertion.metric.parent_keys",
        "row_count" => "notebook.analysis.assertion.metric.row_count",
        "threshold" => "notebook.analysis.assertion.metric.threshold",
        "violations" => "notebook.analysis.assertion.metric.violations",
        _ => return None,
    })
}

fn kind_label(kind: &str) -> String {
    kind_key(kind).map_or_else(|| kind.to_string(), t)
}

fn kind_key(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "row_count" => "notebook.analysis.assertion.kind.row_count",
        "unique" => "notebook.analysis.assertion.kind.unique",
        "not_null" => "notebook.analysis.assertion.kind.not_null",
        "value_range" => "notebook.analysis.assertion.kind.value_range",
        "accepted_values" => "notebook.analysis.assertion.kind.accepted_values",
        "regex" => "notebook.analysis.assertion.kind.regex",
        "referential" => "notebook.analysis.assertion.kind.referential",
        "distribution_drift" => "notebook.analysis.assertion.kind.distribution_drift",
        "freshness" => "notebook.analysis.assertion.kind.freshness",
        "duration" => "notebook.analysis.assertion.kind.duration",
        "kpi_threshold" => "notebook.analysis.assertion.kind.kpi_threshold",
        _ => return None,
    })
}

fn field_label(field: &str) -> String {
    field_key(field).map_or_else(|| field.to_string(), t)
}

fn field_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "min" => "notebook.analysis.assertion.field.min",
        "max" => "notebook.analysis.assertion.field.max",
        "columns" => "notebook.analysis.assertion.field.columns",
        "column" => "notebook.analysis.assertion.field.column",
        "max_null_rate" => "notebook.analysis.assertion.field.max_null_rate",
        "values" => "notebook.analysis.assertion.field.values",
        "pattern" => "notebook.analysis.assertion.field.pattern",
        "parent" => "notebook.analysis.assertion.field.parent",
        "parent_column" => "notebook.analysis.assertion.field.parent_column",
        "warn" => "notebook.analysis.assertion.field.warn",
        "fail" => "notebook.analysis.assertion.field.fail",
        "max_age_seconds" => "notebook.analysis.assertion.field.max_age_seconds",
        "warn_age_seconds" => "notebook.analysis.assertion.field.warn_age_seconds",
        "time_zone" => "notebook.analysis.assertion.field.time_zone",
        "max_ms" => "notebook.analysis.assertion.field.max_ms",
        "warn_ms" => "notebook.analysis.assertion.field.warn_ms",
        "threshold" => "notebook.analysis.assertion.field.threshold",
        _ => return None,
    })
}

fn metric_text(metric: &Metric) -> String {
    match metric {
        Metric::Bool(b) => b.to_string(),
        Metric::Integer(n) => n.to_string(),
        Metric::Number(f) => ui::number(*f),
        Metric::Text(s) => s.clone(),
    }
}

/// The evidence line of an outcome: checked rows, failures and observed figures.
pub(crate) fn evidence_text(outcome: &AssertionOutcome) -> String {
    let e = &outcome.evidence;
    let mut parts = vec![crate::localization::format(
        if e.checked_rows == 1 {
            "notebook.analysis.assertion.checked_one"
        } else {
            "notebook.analysis.assertion.checked"
        },
        &[
            ("failing", e.failing_count.to_string()),
            ("count", e.checked_rows.to_string()),
        ],
    )];
    for (name, value) in &e.observed {
        parts.push(format!("{}: {}", metric_label(name), metric_text(value)));
    }
    if !e.failing_values.is_empty() {
        parts.push(crate::localization::format(
            "notebook.analysis.assertion.failing_values",
            &[("values", e.failing_values.join(", "))],
        ));
    }
    parts.join(" · ")
}

pub struct AssertionView {
    notebook: WeakEntity<NotebookView>,
    cell_id: String,
    run: Option<AssertionRun>,
    /// Inputs per (assertion, field), rebuilt when the list changes shape.
    inputs: HashMap<(usize, &'static str), Entity<InputState>>,
    shape: Vec<&'static str>,
    open_sample: Option<(usize, Option<Entity<SqllyDataTable>>)>,
}

impl AssertionView {
    pub fn new(notebook: WeakEntity<NotebookView>, cell_id: String) -> Self {
        Self {
            notebook,
            cell_id,
            run: None,
            inputs: HashMap::new(),
            shape: Vec::new(),
            open_sample: None,
        }
    }

    pub fn set_run(&mut self, run: AssertionRun, cx: &mut Context<Self>) {
        self.run = Some(run);
        self.open_sample = None;
        cx.notify();
    }

    fn spec(&self, cx: &gpui::App) -> AssertionCellSpec {
        self.notebook
            .upgrade()
            .map(|nb| AssertionCellSpec::from_spec(&nb.read(cx).doc.spec(&self.cell_id)))
            .unwrap_or_default()
    }

    fn edit(&self, cx: &mut gpui::App, change: impl FnOnce(&mut AssertionCellSpec)) {
        Self::edit_at(&self.notebook, &self.cell_id, cx, change);
    }

    fn edit_view(
        view: &Entity<Self>,
        cx: &mut gpui::App,
        change: impl FnOnce(&mut AssertionCellSpec),
    ) {
        let (notebook, id) = {
            let v = view.read(cx);
            (v.notebook.clone(), v.cell_id.clone())
        };
        Self::edit_at(&notebook, &id, cx, change);
    }

    fn edit_at(
        notebook: &WeakEntity<NotebookView>,
        id: &str,
        cx: &mut gpui::App,
        change: impl FnOnce(&mut AssertionCellSpec),
    ) {
        let Some(nb) = notebook.upgrade() else {
            return;
        };
        nb.update(cx, |nb, cx| {
            let mut spec = AssertionCellSpec::from_spec(&nb.doc.spec(id));
            change(&mut spec);
            nb.set_cell_spec(id, spec.to_spec(), cx);
        });
    }

    fn ensure_inputs(
        &mut self,
        spec: &AssertionCellSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shape: Vec<&'static str> = spec
            .assertions
            .iter()
            .map(|a| kind_of(&a.assertion))
            .collect();
        if shape == self.shape {
            return;
        }
        self.shape = shape;
        self.inputs.clear();
        for (i, a) in spec.assertions.iter().enumerate() {
            let json = serde_json::to_value(a).unwrap_or(Value::Null);
            for (key, ty) in fields(kind_of(&a.assertion)) {
                let value = field_text(json.get(*key).unwrap_or(&Value::Null));
                let (key, ty) = (*key, *ty);
                let input = ui::input(
                    window,
                    cx,
                    "notebook.analysis.assertion.field_placeholder",
                    &value,
                    move |this: &mut Self, text, cx| {
                        let Some(value) = parse_field(ty, &text) else {
                            return;
                        };
                        this.edit(cx, |spec| {
                            if let Some(a) = spec.assertions.get(i)
                                && let Some(next) = with_field(a, key, value)
                            {
                                spec.assertions[i] = next;
                            }
                        });
                    },
                );
                self.inputs.insert((i, key), input);
            }
        }
    }

    fn toolbar(&self, spec: &AssertionCellSpec, cx: &mut Context<Self>) -> AnyElement {
        let (choices, snapshots, running) = self
            .notebook
            .upgrade()
            .map(|nb| {
                let nb = nb.read(cx);
                (
                    nb.input_choices(&self.cell_id),
                    nb.snapshot_names(),
                    nb.cell_state(&self.cell_id).is_active(),
                )
            })
            .unwrap_or_default();
        let current = choices
            .iter()
            .find(|(id, label)| *id == spec.input || label.eq_ignore_ascii_case(&spec.input))
            .cloned();
        let this = cx.weak_entity();
        let on_input: ui::OnChoose = std::rc::Rc::new({
            let this = this.clone();
            move |value, app| {
                if let Some(v) = this.upgrade() {
                    Self::edit_view(&v, app, |s| s.input = value);
                }
            }
        });
        let mut baselines = vec![
            (
                "none".to_string(),
                t("notebook.analysis.assertion.baseline_none"),
            ),
            (
                "previous_run".to_string(),
                t("notebook.analysis.assertion.baseline_previous"),
            ),
        ];
        for name in snapshots {
            baselines.push((
                format!("snapshot:{name}"),
                crate::localization::format(
                    "notebook.analysis.assertion.baseline_snapshot",
                    &[("name", name.as_str())],
                ),
            ));
        }
        let baseline_value = match &spec.baseline {
            BaselineRef::None => "none".to_string(),
            BaselineRef::PreviousRun => "previous_run".to_string(),
            BaselineRef::Snapshot(n) => format!("snapshot:{n}"),
        };
        let baseline_label = baselines
            .iter()
            .find(|(v, _)| *v == baseline_value)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| baseline_value.clone());
        let on_baseline: ui::OnChoose = std::rc::Rc::new({
            let this = this.clone();
            move |value, app| {
                if let Some(v) = this.upgrade() {
                    Self::edit_view(&v, app, |s| {
                        s.baseline = match value.as_str() {
                            "previous_run" => BaselineRef::PreviousRun,
                            v if v.starts_with("snapshot:") => {
                                BaselineRef::Snapshot(v["snapshot:".len()..].to_string())
                            }
                            _ => BaselineRef::None,
                        }
                    });
                }
            }
        });
        let first_column = current
            .as_ref()
            .and_then(|(id, _)| {
                let nb = self.notebook.upgrade()?;
                let nb = nb.read(cx);
                let data = crate::notebook::analysis::inputs::cell_output(nb, id, cx)?;
                data.output.columns.first().map(|c| c.name.clone())
            })
            .unwrap_or_default();
        let on_add: ui::OnChoose = std::rc::Rc::new(move |kind, app| {
            if let Some(v) = this.upgrade()
                && let Some(a) = new_assertion(&kind, &first_column)
            {
                Self::edit_view(&v, app, |s| s.assertions.push(AssertionSpec::from(a)));
            }
        });
        let notebook = self.notebook.clone();
        let id = self.cell_id.clone();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::scaled(8.0)))
            .child(field(
                t("notebook.analysis.input_label"),
                picker(
                    "nb-assert-input",
                    current
                        .as_ref()
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| t("notebook.analysis.input_choose")),
                    choices,
                    current.map(|(id, _)| id),
                    on_input,
                ),
            ))
            .child(field(
                t("notebook.analysis.assertion.baseline_label"),
                picker(
                    "nb-assert-baseline",
                    baseline_label,
                    baselines,
                    Some(baseline_value),
                    on_baseline,
                ),
            ))
            .child(picker(
                "nb-assert-add",
                t("notebook.analysis.assertion.add"),
                ASSERTION_KINDS
                    .iter()
                    .map(|k| (k.to_string(), kind_label(k)))
                    .collect(),
                None,
                on_add,
            ))
            .child(
                ui::primary_button("nb-assert-run", t("notebook.analysis.assertion.run"))
                    .disabled(running)
                    .on_click(move |_, _, app| {
                        if let Some(nb) = notebook.upgrade() {
                            nb.update(app, |nb, cx| nb.request_run_from_host(&id, cx));
                        }
                    }),
            )
            .into_any_element()
    }

    fn row(
        &self,
        i: usize,
        a: &AssertionSpec,
        outcome: Option<&AssertionOutcome>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let kind = kind_of(&a.assertion);
        let inputs: Vec<AnyElement> = fields(kind)
            .iter()
            .filter_map(|(key, _)| {
                let input = self.inputs.get(&(i, *key))?.clone();
                Some(
                    field(
                        field_label(key),
                        div().w(px(s(140.0))).child(Input::new(&input).small()),
                    )
                    .into_any_element(),
                )
            })
            .collect();
        let warn = a.severity == Severity::Warn;
        let has_sample = self
            .run
            .as_ref()
            .is_some_and(|r| r.samples.contains_key(&i));
        let sample_open = self.open_sample.as_ref().is_some_and(|(n, _)| *n == i);
        div()
            .flex()
            .flex_col()
            .gap(px(s(3.0)))
            .py(px(s(4.0)))
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(body_text(kind_label(kind)))
                    .children(inputs)
                    .child(
                        ui::button(
                            ("nb-assert-severity", i).into_id(),
                            t(if warn {
                                "notebook.analysis.assertion.severity_warn"
                            } else {
                                "notebook.analysis.assertion.severity_error"
                            }),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit(cx, |spec| {
                                if let Some(a) = spec.assertions.get_mut(i) {
                                    a.severity = if a.severity == Severity::Warn {
                                        Severity::Error
                                    } else {
                                        Severity::Warn
                                    };
                                }
                            })
                        })),
                    )
                    .child(
                        ui::button(
                            ("nb-assert-remove", i).into_id(),
                            t("notebook.analysis.assertion.remove"),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit(cx, |spec| {
                                if i < spec.assertions.len() {
                                    spec.assertions.remove(i);
                                }
                            })
                        })),
                    ),
            )
            .when_some(outcome, |d, o| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap(px(s(6.0)))
                        .child(chip(t(status_key(o.status)), status_color(o.status)))
                        .when_some(o.issue.as_ref(), |d, issue| {
                            d.child(caption(issue_text(issue)))
                        })
                        .child(caption(evidence_text(o)))
                        .when(has_sample, |d| {
                            d.child(
                                ui::button(
                                    ("nb-assert-sample", i).into_id(),
                                    t(if sample_open {
                                        "notebook.analysis.assertion.hide_rows"
                                    } else {
                                        "notebook.analysis.assertion.show_rows"
                                    }),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_sample(i, cx)),
                                ),
                            )
                        }),
                )
            })
            .when(sample_open, |d| {
                d.child(ui::grid_box(
                    self.open_sample.as_ref().and_then(|(_, g)| g.clone()),
                    160.0,
                ))
            })
            .into_any_element()
    }

    fn toggle_sample(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.open_sample.as_ref().is_some_and(|(n, _)| *n == i) {
            self.open_sample = None;
        } else if let Some(sample) = self.run.as_ref().and_then(|r| r.samples.get(&i)).cloned() {
            let grid = ui::grid(&sample.columns, &sample.rows, cx);
            self.open_sample = Some((i, grid));
        }
        if let Some(nb) = self.notebook.upgrade() {
            let id = self.cell_id.clone();
            nb.update(cx, |nb, _| nb.remeasure_cell(&id));
        }
        cx.notify();
    }
}

trait IntoId {
    fn into_id(self) -> String;
}

impl IntoId for (&str, usize) {
    fn into_id(self) -> String {
        format!("{}-{}", self.0, self.1)
    }
}

impl Render for AssertionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let spec = self.spec(cx);
        self.ensure_inputs(&spec, window, cx);
        let output_only =
            crate::notebook::present::cell_output_only(&self.notebook, &self.cell_id, cx);
        let toolbar = self.toolbar(&spec, cx);
        let summary = self.run.as_ref().map(|r| TestSummary::of(&r.outcomes));
        let outcomes: Vec<Option<AssertionOutcome>> = (0..spec.assertions.len())
            .map(|i| {
                self.run
                    .as_ref()
                    .filter(|r| r.outcomes.len() == spec.assertions.len())
                    .and_then(|r| r.outcomes.get(i).cloned())
            })
            .collect();
        let rows: Vec<AnyElement> = spec
            .assertions
            .iter()
            .enumerate()
            .map(|(i, a)| self.row(i, a, outcomes[i].as_ref(), cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .px(px(s(10.0)))
            .py(px(s(8.0)))
            .w_full()
            .when(!output_only, |d| d.child(toolbar))
            .when_some(summary, |d, summary| {
                d.child(caption(crate::localization::format(
                    "notebook.analysis.assertion.summary",
                    &[
                        ("passed", summary.passed.to_string()),
                        ("warned", summary.warned.to_string()),
                        ("failed", summary.failed.to_string()),
                        ("errors", summary.errors.to_string()),
                        ("inconclusive", summary.inconclusive.to_string()),
                    ],
                )))
            })
            .when(spec.assertions.is_empty(), |d| {
                d.child(caption(t("notebook.analysis.assertion.empty")))
            })
            .when(!spec.invalid.is_empty(), |d| {
                d.child(caption(crate::localization::format(
                    if spec.invalid.len() == 1 {
                        "notebook.analysis.assertion.invalid_one"
                    } else {
                        "notebook.analysis.assertion.invalid"
                    },
                    &[("count", spec.invalid.len().to_string())],
                )))
            })
            .children(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_kind_has_editable_fields_that_round_trip() {
        for kind in ASSERTION_KINDS {
            assert!(!fields(kind).is_empty(), "{kind}");
            let spec = AssertionSpec::from(new_assertion(kind, "c").unwrap());
            for (key, ty) in fields(kind) {
                let sample = match ty {
                    FieldType::Text => "x",
                    FieldType::OptionalText => "Europe/Berlin",
                    FieldType::List => "a, b",
                    _ => "3",
                };
                let value = parse_field(*ty, sample).unwrap();
                let next = with_field(&spec, key, value).unwrap_or_else(|| panic!("{kind}.{key}"));
                let json = serde_json::to_value(&next).unwrap();
                assert!(!field_text(&json[*key]).is_empty(), "{kind}.{key}");
            }
            assert!(kind_key(kind).is_some(), "{kind}");
        }
        assert_eq!(parse_field(FieldType::Unsigned, "-1"), None);
        let fresh = AssertionSpec::from(new_assertion("freshness", "updated_at").unwrap());
        let zoned = with_field(
            &fresh,
            "time_zone",
            parse_field(FieldType::OptionalText, " +05:30 ").unwrap(),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&zoned).unwrap()["time_zone"], "+05:30");
        let cleared = with_field(
            &zoned,
            "time_zone",
            parse_field(FieldType::OptionalText, "").unwrap(),
        )
        .unwrap();
        assert!(
            serde_json::to_value(&cleared)
                .unwrap()
                .get("time_zone")
                .is_none(),
            "a blank time zone means UTC and is not written"
        );
        assert_eq!(parse_field(FieldType::Number, ""), Some(Value::Null));
        assert_eq!(field_text(&json!(["a", 2])), "a, 2");
    }

    #[test]
    fn every_issue_and_metric_reads_as_text() {
        for key in [
            "row_count",
            "null_rate",
            "duplicate_rows",
            "orphan_rows",
            "drift",
            "age_seconds",
        ] {
            assert!(metric_key(key).is_some(), "{key}");
        }
        let text = issue_text(&AssertionIssue::MissingColumn {
            column: "id".into(),
        });
        assert!(text.contains("id"), "{text}");
        let text = issue_text(&AssertionIssue::IncompleteInput {
            completeness: sqlly_services::analytics::Completeness::Partial,
        });
        assert!(!text.is_empty());
    }
}
