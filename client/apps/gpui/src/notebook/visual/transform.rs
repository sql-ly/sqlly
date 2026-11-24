//! Transform cells: a list of no-code steps compiled to one DuckDB query
//! over the inputs materialized in the notebook's local workspace. The
//! generated SQL is shown read-only (and opens in a query tab); every edit
//! can be undone step by step; the result is a new local output other
//! cells can read.
//!
//! [`execute`] is the one place a transform runs. It materializes the
//! input outputs into the notebook's workspace through
//! `sqlly_services::local_analytics` and queries them there; the local
//! data lane can replace it with its own routing without touching the rest.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use serde_json::Value;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::{GridConfig, SqllyDataTable};
use sqlly_services::notebook::cells::spec;
use sqlly_services::notebook::cells::transform::{
    AggFunction, AggregateColumn, CompiledTransform, FilterTest, JoinKind, JoinOn, SortKey,
    TransformCellSpec, TransformStep, compile,
};

use super::{
    InputInfo, VisualCellView, VisualEvent, VisualInputs, choice_button, code_block, dim,
    editor_panel, heading, small_button, t, text_field,
};
use crate::theme::Theme;

/// Most rows a transform keeps.
pub const MAX_TRANSFORM_ROWS: usize = 100_000;
/// Most undo steps kept per cell.
const MAX_UNDO: usize = 50;

#[derive(Default)]
pub struct TransformPart {
    undo: Vec<Vec<TransformStep>>,
    preview: Option<Entity<SqllyDataTable>>,
    preview_run: Option<String>,
}

/// The relation an input cell is materialized as.
fn relation_of(info: &InputInfo) -> String {
    let name: String = info
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("nb_{name}")
}

fn compiled(inputs: &VisualInputs) -> Result<CompiledTransform, sqlly_core::DisplayMessage> {
    let cell: TransformCellSpec = spec::read(&inputs.spec);
    let infos: Vec<&InputInfo> = inputs.input.iter().chain(&inputs.others).collect();
    compile(&cell, &|id| {
        infos.iter().find(|i| i.id == id).map(|i| relation_of(i))
    })
    .map_err(|e| e.message())
}

/// Everything a transform run needs, gathered on the UI thread.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct TransformJob {
    pub compiled: CompiledTransform,
    #[cfg(not(target_arch = "wasm32"))]
    pub inputs: Vec<sqlly_services::notebook::cells::transform::LocalInput>,
    pub max_rows: usize,
}

pub struct TransformResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub truncated: bool,
}

pub fn prepare_job(inputs: &VisualInputs) -> Result<TransformJob, sqlly_core::DisplayMessage> {
    let compiled = compiled(inputs)?;
    #[cfg(not(target_arch = "wasm32"))]
    let local = {
        use sqlly_services::local_analytics::{SourceInfo, SourceKind};
        let mut local = Vec::new();
        for (cell, relation) in &compiled.relations {
            let info = inputs
                .input
                .iter()
                .chain(&inputs.others)
                .find(|i| &i.id == cell);
            let Some(output) = info.and_then(|i| i.output.clone()) else {
                let name = info.map(|i| i.name.clone()).unwrap_or_else(|| cell.clone());
                return Err(crate::localization::deferred(
                    "notebook.visual.notice.input_not_run",
                    &[("cell_name", name.as_str())],
                ));
            };
            let p = &output.provenance;
            let mut source = SourceInfo::query(inputs.db, p.connection_label.clone());
            source.kind = SourceKind::Query;
            source.database = p.database.clone();
            source.environment = p.environment.clone();
            source.sql_hash = p.sql_hash.clone();
            source.cell_id = Some(cell.clone());
            source.run_id = Some(output.run_id.clone());
            source.extracted_at = p.extracted_at.map(|at| at.to_rfc3339());
            if !output.completeness.supports_summaries() {
                source.source_incomplete = Some(format!("{:?}", output.completeness));
            }
            local.push(sqlly_services::notebook::cells::transform::LocalInput {
                relation: relation.clone(),
                columns: output.columns.clone(),
                rows: std::sync::Arc::clone(&output.rows),
                source,
            });
        }
        local
    };
    Ok(TransformJob {
        compiled,
        #[cfg(not(target_arch = "wasm32"))]
        inputs: local,
        max_rows: MAX_TRANSFORM_ROWS,
    })
}

/// Run a transform in the notebook's own workspace: materialize its inputs
/// and query them there, stopping when `cancel` fires. Blocking; call off
/// the UI thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn execute(
    job: TransformJob,
    workspace: &sqlly_services::local_analytics::NotebookWorkspace,
    cancel: &sqlly_services::local_analytics::CancelToken,
) -> Result<TransformResult, sqlly_core::DisplayMessage> {
    use sqlly_services::local_analytics::error_message;
    let max_rows = job
        .max_rows
        .min(workspace.workspace().quota().max_query_rows);
    let set = sqlly_services::notebook::cells::transform::run_local(
        workspace.workspace(),
        &job.compiled,
        job.inputs,
        max_rows,
        cancel,
    )
    .map_err(|e| error_message(&e))?;
    Ok(TransformResult {
        columns: set.columns,
        rows: set.rows,
        truncated: set.truncated,
    })
}

#[cfg(target_arch = "wasm32")]
pub fn execute(_job: TransformJob) -> Result<TransformResult, sqlly_core::DisplayMessage> {
    Err(sqlly_services::local_analytics::unavailable_message())
}

/// The value a filter field's text stands for, kept as typed: DuckDB casts
/// a text literal to the column's own type, so `007` still matches a text
/// zip code, `2024` a text year, and a 20-digit id keeps every digit.
pub(crate) fn parse_value(text: &str) -> Value {
    let text = text.trim();
    if text.is_empty() {
        return Value::Null;
    }
    Value::String(text.to_string())
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(value_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// The column names step `index` sees, simulated from the input's columns.
/// A pivot's new columns are named by the values of its `on` column, which
/// `values` reads from the input's rows (sorted, as DuckDB names them).
pub(crate) fn columns_before(
    steps: &[TransformStep],
    index: usize,
    input: &[String],
    other: &dyn Fn(&str) -> Vec<String>,
    values: &dyn Fn(&str) -> Vec<String>,
) -> Vec<String> {
    let mut columns = input.to_vec();
    for step in steps.iter().take(index) {
        match step {
            TransformStep::Select { columns: keep } => columns = keep.clone(),
            TransformStep::Rename { column, to } => {
                for c in columns.iter_mut().filter(|c| *c == column) {
                    *c = to.clone();
                }
            }
            TransformStep::Compute { name, .. } => columns.push(name.clone()),
            TransformStep::Group { by, aggregates } => {
                columns = by.clone();
                columns.extend(aggregates.iter().map(AggregateColumn::output_name));
            }
            TransformStep::Join { with, .. } => {
                for c in other(with) {
                    if !columns.contains(&c) {
                        columns.push(c);
                    }
                }
            }
            TransformStep::Unpivot {
                columns: gone,
                name,
                value,
            } => {
                columns.retain(|c| !gone.contains(c));
                columns.push(name.clone());
                columns.push(value.clone());
            }
            TransformStep::Pivot {
                on,
                value,
                group_by,
                ..
            } => {
                let mut next: Vec<String> = if group_by.is_empty() {
                    columns
                        .iter()
                        .filter(|c| *c != on && *c != value)
                        .cloned()
                        .collect()
                } else {
                    group_by.clone()
                };
                let mut pivoted = values(on);
                pivoted.sort();
                pivoted.dedup();
                for name in pivoted {
                    if !next.contains(&name) {
                        next.push(name);
                    }
                }
                columns = next;
            }
            TransformStep::Filter { .. }
            | TransformStep::Sort { .. }
            | TransformStep::Limit { .. } => {}
        }
    }
    columns
}

/// Distinct values of column `name` in `rows`, as pivot column names.
pub(crate) fn distinct_values(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    name: &str,
) -> Vec<String> {
    let Some(index) = columns.iter().position(|c| c.name == name) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        let label = match row.get(index) {
            None | Some(CellValue::Null) => continue,
            Some(cell) => spec::category_label(cell, None),
        };
        if !out.contains(&label) {
            out.push(label);
            if out.len() > MAX_PIVOT_COLUMNS {
                break;
            }
        }
    }
    out
}

/// Most pivot columns the editor offers.
const MAX_PIVOT_COLUMNS: usize = 200;

impl VisualCellView {
    /// Distinct values of the input's column `name`.
    fn input_values(&self, name: &str) -> Vec<String> {
        match self.inputs.input_rows() {
            Some((output, rows)) => distinct_values(&output.columns, &rows, name),
            None => Vec::new(),
        }
    }

    fn transform_spec(&self) -> TransformCellSpec {
        spec::read(&self.inputs.spec)
    }

    /// The query the steps compile to, when they compile.
    pub(crate) fn transform_sql(&self) -> Option<String> {
        compiled(&self.inputs).ok().map(|c| c.sql)
    }

    fn set_steps(&mut self, steps: Vec<TransformStep>, cx: &mut Context<Self>) {
        let mut cell = self.transform_spec();
        if cell.steps == steps {
            return;
        }
        self.transform.undo.push(cell.steps.clone());
        if self.transform.undo.len() > MAX_UNDO {
            self.transform.undo.remove(0);
        }
        cell.steps = steps;
        cell.sync_inputs();
        self.emit_spec(spec::write(&cell), cx);
    }

    fn update_step(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut TransformStep),
    ) {
        let mut steps = self.transform_spec().steps;
        if let Some(step) = steps.get_mut(index) {
            change(step);
        }
        self.set_steps(steps, cx);
    }

    fn undo_step(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.transform.undo.pop() else {
            return;
        };
        let mut cell = self.transform_spec();
        cell.steps = previous;
        cell.sync_inputs();
        self.drop_fields("transform.");
        self.emit_spec(spec::write(&cell), cx);
    }

    fn other_columns(&self, cell: &str) -> Vec<String> {
        self.inputs
            .others
            .iter()
            .chain(self.inputs.input.iter())
            .find(|i| i.id == cell)
            .and_then(|i| i.output.as_ref())
            .map(|o| o.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn render_transform(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let id = self.inputs.cell_id.clone();
        let this = cx.entity();
        let cell = self.transform_spec();
        let input_columns: Vec<String> = self
            .inputs
            .input_columns()
            .into_iter()
            .map(|c| c.name)
            .collect();
        let mut root = div().flex().flex_col().w_full();

        let mut steps_panel = editor_panel().child(heading(t("notebook.visual.transform.steps")));
        if cell.steps.is_empty() {
            steps_panel = steps_panel.child(dim(t("notebook.visual.transform.no_steps")));
        }
        for (index, step) in cell.steps.iter().enumerate() {
            let others = |c: &str| self.other_columns(c);
            let values = |c: &str| self.input_values(c);
            let columns = columns_before(&cell.steps, index, &input_columns, &others, &values);
            let row = self.render_step(index, step, &columns, window, cx);
            steps_panel = steps_panel.child(row);
        }
        let first = input_columns.first().cloned();
        let ops: Vec<(String, String, bool)> = TransformStep::OPS
            .iter()
            .filter_map(|op| {
                let step = TransformStep::new(op, None)?;
                Some((t(step_key(&step)), op.to_string(), false))
            })
            .collect();
        steps_panel = steps_panel.child(
            div()
                .flex()
                .flex_row()
                .gap(px(s(6.0)))
                .child(choice_button(
                    format!("nb-transform-add-{id}"),
                    t("notebook.visual.transform.add_step"),
                    ops,
                    this.clone(),
                    move |v, op: String, cx| {
                        let mut steps = v.transform_spec().steps;
                        if let Some(step) = TransformStep::new(&op, first.as_deref()) {
                            steps.push(step);
                            v.set_steps(steps, cx);
                        }
                    },
                ))
                .child(
                    small_button(
                        format!("nb-transform-undo-{id}"),
                        t("notebook.visual.transform.undo"),
                    )
                    .disabled(self.transform.undo.is_empty())
                    .on_click(cx.listener(|v, _, _, cx| v.undo_step(cx))),
                )
                .child(
                    small_button(
                        format!("nb-transform-run-{id}"),
                        t("notebook.visual.transform.run"),
                    )
                    .tooltip(t("notebook.visual.transform.run_tooltip"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(VisualEvent::Run))),
                ),
        );
        if !self.output_only {
            root = root.child(steps_panel);
        }

        let sql = match compiled(&self.inputs) {
            Ok(c) => code_block(c.sql),
            Err(message) => div()
                .px(px(s(10.0)))
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::warning())
                .child(crate::localization::message(message)),
        };
        root = root.when(!self.output_only, |root| {
            root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(3.0)))
                    .mx(px(s(8.0)))
                    .child(heading(t("notebook.visual.transform.generated_sql")))
                    .child(sql)
                    .child(dim(t("notebook.visual.transform.local_note"))),
            )
        });

        if let Some(output) = self.inputs.own_output.clone() {
            if self.transform.preview_run.as_deref() != Some(output.run_id.as_str()) {
                self.transform.preview_run = Some(output.run_id.clone());
                self.transform.preview =
                    crate::grid_bridge::build_grid_data(None, &output.columns, &output.rows)
                        .ok()
                        .map(|data| {
                            let table = SqllyDataTable::builder(data)
                                .config(GridConfig::default())
                                .theme(crate::grid_bridge::build_grid_theme())
                                .build(cx);
                            cx.new(|_| table)
                        });
            }
            if let Some(preview) = self.transform.preview.clone() {
                root = root
                    .child(div().px(px(s(10.0))).pt(px(s(4.0))).child(dim(
                        crate::localization::format(
                            "notebook.visual.transform.result_rows",
                            &[("row_count", super::thousands(output.row_count))],
                        ),
                    )))
                    .child(div().h(px(s(240.0))).w_full().min_h_0().child(preview));
            }
        }
        root.into_any_element()
    }

    fn render_step(
        &mut self,
        index: usize,
        step: &TransformStep,
        columns: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let id = format!("{}-{index}", self.inputs.cell_id);
        let this = cx.entity();
        let none = t("notebook.visual.chart.editor.none");
        let column_choice = |tag: &str, current: &str, set: fn(&mut TransformStep, String)| {
            let options = columns
                .iter()
                .map(|c| (c.clone(), c.clone(), c == current))
                .collect();
            choice_button(
                format!("nb-transform-{tag}-{id}"),
                if current.is_empty() {
                    none.clone()
                } else {
                    current.to_string()
                },
                options,
                this.clone(),
                move |v, column: String, cx| v.update_step(index, cx, |st| set(st, column)),
            )
        };
        let toggles = |tag: &str, chosen: &[String], set: fn(&mut TransformStep, String)| {
            let options = columns
                .iter()
                .map(|c| (c.clone(), c.clone(), chosen.contains(c)))
                .collect();
            choice_button(
                format!("nb-transform-{tag}-{id}"),
                if chosen.is_empty() {
                    none.clone()
                } else {
                    chosen.join(", ")
                },
                options,
                this.clone(),
                move |v, column: String, cx| v.update_step(index, cx, |st| set(st, column)),
            )
        };
        fn toggle(list: &mut Vec<String>, column: String) {
            if let Some(at) = list.iter().position(|c| *c == column) {
                list.remove(at);
            } else {
                list.push(column);
            }
        }
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .py(px(s(2.0)))
            .child(
                div()
                    .w(px(s(90.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "notebook.visual.transform.step_label",
                        &[
                            ("step", (index + 1).to_string()),
                            ("operation", t(step_key(step))),
                        ],
                    )),
            );
        match step {
            TransformStep::Filter {
                column,
                test,
                value,
                include_empty,
            } => {
                let tests: Vec<(String, FilterTest, bool)> = FilterTest::ALL
                    .iter()
                    .map(|f| (t(test_key(*f)), *f, f == test))
                    .collect();
                row = row
                    .child(column_choice("filter-column", column, |st, c| {
                        if let TransformStep::Filter { column, .. } = st {
                            *column = c;
                        }
                    }))
                    .child(choice_button(
                        format!("nb-transform-test-{id}"),
                        t(test_key(*test)),
                        tests,
                        this.clone(),
                        move |v, f, cx| {
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Filter { test, .. } = st {
                                    *test = f;
                                }
                            })
                        },
                    ));
                if test.needs_value() {
                    let field = self.field(
                        &format!("transform.value.{id}"),
                        &value_text(value),
                        "notebook.visual.transform.value_placeholder",
                        window,
                        cx,
                        move |v, text, cx| {
                            let parsed = parse_value(text);
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Filter { value, test, .. } = st {
                                    *value = if *test == FilterTest::In {
                                        Value::Array(
                                            text.split(',')
                                                .map(parse_value)
                                                .filter(|v| !v.is_null())
                                                .collect(),
                                        )
                                    } else {
                                        parsed
                                    };
                                }
                            });
                        },
                    );
                    row = row.child(text_field(&field, 160.0));
                }
                if test.offers_include_empty() {
                    let keep = *include_empty;
                    row = row.child(
                        gpui_component::checkbox::Checkbox::new(gpui::ElementId::Name(
                            format!("nb-transform-empty-{id}").into(),
                        ))
                        .label(t("notebook.visual.transform.include_empty"))
                        .checked(keep)
                        .on_click(cx.listener(
                            move |v, checked: &bool, _, cx| {
                                let on = *checked;
                                v.update_step(index, cx, |st| {
                                    if let TransformStep::Filter { include_empty, .. } = st {
                                        *include_empty = on;
                                    }
                                })
                            },
                        )),
                    );
                }
            }
            TransformStep::Select { columns: chosen } => {
                row = row.child(toggles("select", chosen, |st, c| {
                    if let TransformStep::Select { columns } = st {
                        toggle(columns, c);
                    }
                }));
            }
            TransformStep::Rename { column, to } => {
                let field = self.field(
                    &format!("transform.rename.{id}"),
                    to,
                    "notebook.visual.transform.new_name_placeholder",
                    window,
                    cx,
                    move |v, text, cx| {
                        let text = text.to_string();
                        v.update_step(index, cx, |st| {
                            if let TransformStep::Rename { to, .. } = st {
                                *to = text;
                            }
                        });
                    },
                );
                row = row
                    .child(column_choice("rename-column", column, |st, c| {
                        if let TransformStep::Rename { column, .. } = st {
                            *column = c;
                        }
                    }))
                    .child(dim(t("notebook.visual.transform.rename_to")))
                    .child(text_field(&field, 140.0));
            }
            TransformStep::Compute { name, expression } => {
                let name_field = self.field(
                    &format!("transform.compute_name.{id}"),
                    name,
                    "notebook.visual.transform.new_name_placeholder",
                    window,
                    cx,
                    move |v, text, cx| {
                        let text = text.to_string();
                        v.update_step(index, cx, |st| {
                            if let TransformStep::Compute { name, .. } = st {
                                *name = text;
                            }
                        });
                    },
                );
                let expression_field = self.field(
                    &format!("transform.compute_expression.{id}"),
                    expression,
                    "notebook.visual.transform.expression_placeholder",
                    window,
                    cx,
                    move |v, text, cx| {
                        let text = text.to_string();
                        v.update_step(index, cx, |st| {
                            if let TransformStep::Compute { expression, .. } = st {
                                *expression = text;
                            }
                        });
                    },
                );
                row = row
                    .child(text_field(&name_field, 120.0))
                    .child(dim("=".into()))
                    .child(text_field(&expression_field, 260.0));
            }
            TransformStep::Group { by, aggregates } => {
                row = row
                    .child(dim(t("notebook.visual.transform.group_by")))
                    .child(toggles("group-by", by, |st, c| {
                        if let TransformStep::Group { by, .. } = st {
                            toggle(by, c);
                        }
                    }));
                for (a, aggregate) in aggregates.iter().enumerate() {
                    let functions: Vec<(String, AggFunction, bool)> = AggFunction::ALL
                        .iter()
                        .map(|f| (t(function_key(*f)), *f, *f == aggregate.function))
                        .collect();
                    let column_opts: Vec<(String, Option<String>, bool)> = std::iter::once((
                        t("notebook.visual.transform.all_rows"),
                        None,
                        aggregate.column.is_none(),
                    ))
                    .chain(columns.iter().map(|c| {
                        (
                            c.clone(),
                            Some(c.clone()),
                            aggregate.column.as_deref() == Some(c.as_str()),
                        )
                    }))
                    .collect();
                    row = row
                        .child(choice_button(
                            format!("nb-transform-fn-{id}-{a}"),
                            t(function_key(aggregate.function)),
                            functions,
                            this.clone(),
                            move |v, f, cx| {
                                v.update_step(index, cx, |st| {
                                    if let TransformStep::Group { aggregates, .. } = st
                                        && let Some(agg) = aggregates.get_mut(a)
                                    {
                                        agg.function = f;
                                    }
                                })
                            },
                        ))
                        .child(choice_button(
                            format!("nb-transform-fncol-{id}-{a}"),
                            aggregate
                                .column
                                .clone()
                                .unwrap_or_else(|| t("notebook.visual.transform.all_rows")),
                            column_opts,
                            this.clone(),
                            move |v, c, cx| {
                                v.update_step(index, cx, |st| {
                                    if let TransformStep::Group { aggregates, .. } = st
                                        && let Some(agg) = aggregates.get_mut(a)
                                    {
                                        agg.column = c;
                                    }
                                })
                            },
                        ));
                }
                let first = columns.first().cloned();
                row = row.child(
                    small_button(
                        format!("nb-transform-addagg-{id}"),
                        t("notebook.visual.transform.add_aggregate"),
                    )
                    .on_click(cx.listener(move |v, _, _, cx| {
                        let first = first.clone();
                        v.update_step(index, cx, |st| {
                            if let TransformStep::Group { aggregates, .. } = st {
                                aggregates.push(AggregateColumn {
                                    function: AggFunction::Sum,
                                    column: first,
                                    alias: None,
                                });
                            }
                        })
                    })),
                );
            }
            TransformStep::Join { with, kind, on } => {
                let cells: Vec<(String, String, bool)> = self
                    .inputs
                    .candidates
                    .iter()
                    .filter(|(c, _)| *c != self.inputs.cell_id)
                    .map(|(c, n)| (n.clone(), c.clone(), c == with))
                    .collect();
                let with_name = self
                    .inputs
                    .candidates
                    .iter()
                    .find(|(c, _)| c == with)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| none.clone());
                let kinds: Vec<(String, JoinKind, bool)> = JoinKind::ALL
                    .iter()
                    .map(|k| (t(join_key(*k)), *k, k == kind))
                    .collect();
                let key = on.first().cloned().unwrap_or_default();
                let right_columns = self.other_columns(with);
                let right_options: Vec<(String, String, bool)> = right_columns
                    .iter()
                    .map(|c| (c.clone(), c.clone(), *c == key.right))
                    .collect();
                row = row
                    .child(choice_button(
                        format!("nb-transform-with-{id}"),
                        with_name,
                        cells,
                        this.clone(),
                        move |v, c: String, cx| {
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Join { with, .. } = st {
                                    *with = c;
                                }
                            })
                        },
                    ))
                    .child(choice_button(
                        format!("nb-transform-joinkind-{id}"),
                        t(join_key(*kind)),
                        kinds,
                        this.clone(),
                        move |v, k, cx| {
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Join { kind, .. } = st {
                                    *kind = k;
                                }
                            })
                        },
                    ))
                    .child(dim(t("notebook.visual.transform.join_on")))
                    .child(column_choice("join-left", &key.left, |st, c| {
                        if let TransformStep::Join { on, .. } = st {
                            match on.first_mut() {
                                Some(k) => k.left = c,
                                None => on.push(JoinOn {
                                    left: c.clone(),
                                    right: c,
                                }),
                            }
                        }
                    }))
                    .child(dim("=".into()))
                    .child(choice_button(
                        format!("nb-transform-join-right-{id}"),
                        if key.right.is_empty() {
                            none.clone()
                        } else {
                            key.right.clone()
                        },
                        right_options,
                        this.clone(),
                        move |v, c: String, cx| {
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Join { on, .. } = st {
                                    match on.first_mut() {
                                        Some(k) => k.right = c,
                                        None => on.push(JoinOn {
                                            left: c.clone(),
                                            right: c,
                                        }),
                                    }
                                }
                            })
                        },
                    ));
            }
            TransformStep::Sort { by } => {
                let key = by.first().cloned().unwrap_or_default();
                let descending = key.descending;
                row = row
                    .child(column_choice("sort", &key.column, |st, c| {
                        if let TransformStep::Sort { by } = st {
                            match by.first_mut() {
                                Some(k) => k.column = c,
                                None => by.push(SortKey {
                                    column: c,
                                    descending: false,
                                }),
                            }
                        }
                    }))
                    .child(
                        small_button(
                            format!("nb-transform-desc-{id}"),
                            t(if descending {
                                "notebook.visual.transform.descending"
                            } else {
                                "notebook.visual.transform.ascending"
                            }),
                        )
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Sort { by } = st
                                    && let Some(k) = by.first_mut()
                                {
                                    k.descending = !descending;
                                }
                            })
                        })),
                    );
            }
            TransformStep::Limit { rows } => {
                let field = self.field(
                    &format!("transform.limit.{id}"),
                    &rows.to_string(),
                    "notebook.visual.transform.limit_placeholder",
                    window,
                    cx,
                    move |v, text, cx| {
                        if let Ok(n) = text.trim().parse::<u64>() {
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Limit { rows } = st {
                                    *rows = n;
                                }
                            });
                        }
                    },
                );
                row = row.child(text_field(&field, 90.0));
            }
            TransformStep::Pivot {
                on,
                value,
                function,
                group_by,
            } => {
                let functions: Vec<(String, AggFunction, bool)> = AggFunction::ALL
                    .iter()
                    .map(|f| (t(function_key(*f)), *f, f == function))
                    .collect();
                row = row
                    .child(dim(t("notebook.visual.transform.pivot_on")))
                    .child(column_choice("pivot-on", on, |st, c| {
                        if let TransformStep::Pivot { on, .. } = st {
                            *on = c;
                        }
                    }))
                    .child(choice_button(
                        format!("nb-transform-pivotfn-{id}"),
                        t(function_key(*function)),
                        functions,
                        this.clone(),
                        move |v, f, cx| {
                            v.update_step(index, cx, |st| {
                                if let TransformStep::Pivot { function, .. } = st {
                                    *function = f;
                                }
                            })
                        },
                    ))
                    .child(column_choice("pivot-value", value, |st, c| {
                        if let TransformStep::Pivot { value, .. } = st {
                            *value = c;
                        }
                    }))
                    .child(dim(t("notebook.visual.transform.group_by")))
                    .child(toggles("pivot-group", group_by, |st, c| {
                        if let TransformStep::Pivot { group_by, .. } = st {
                            toggle(group_by, c);
                        }
                    }));
            }
            TransformStep::Unpivot {
                columns: chosen,
                name,
                value,
            } => {
                let name_field = self.field(
                    &format!("transform.unpivot_name.{id}"),
                    name,
                    "notebook.visual.transform.new_name_placeholder",
                    window,
                    cx,
                    move |v, text, cx| {
                        let text = text.to_string();
                        v.update_step(index, cx, |st| {
                            if let TransformStep::Unpivot { name, .. } = st {
                                *name = text;
                            }
                        });
                    },
                );
                let value_field = self.field(
                    &format!("transform.unpivot_value.{id}"),
                    value,
                    "notebook.visual.transform.new_name_placeholder",
                    window,
                    cx,
                    move |v, text, cx| {
                        let text = text.to_string();
                        v.update_step(index, cx, |st| {
                            if let TransformStep::Unpivot { value, .. } = st {
                                *value = text;
                            }
                        });
                    },
                );
                row = row
                    .child(toggles("unpivot", chosen, |st, c| {
                        if let TransformStep::Unpivot { columns, .. } = st {
                            toggle(columns, c);
                        }
                    }))
                    .child(dim(t("notebook.visual.transform.into")))
                    .child(text_field(&name_field, 100.0))
                    .child(text_field(&value_field, 100.0));
            }
        }
        row = row.child(
            small_button(
                format!("nb-transform-remove-{id}"),
                t("notebook.visual.transform.remove_step"),
            )
            .on_click(cx.listener(move |v, _, _, cx| {
                let mut steps = v.transform_spec().steps;
                if index < steps.len() {
                    steps.remove(index);
                }
                v.drop_fields("transform.");
                v.set_steps(steps, cx);
            })),
        );
        let _ = (s, &self.inputs);
        row.into_any_element()
    }
}

fn step_key(step: &TransformStep) -> &'static str {
    match step {
        TransformStep::Filter { .. } => "notebook.visual.transform.ops.filter",
        TransformStep::Select { .. } => "notebook.visual.transform.ops.select",
        TransformStep::Rename { .. } => "notebook.visual.transform.ops.rename",
        TransformStep::Compute { .. } => "notebook.visual.transform.ops.compute",
        TransformStep::Group { .. } => "notebook.visual.transform.ops.group",
        TransformStep::Join { .. } => "notebook.visual.transform.ops.join",
        TransformStep::Sort { .. } => "notebook.visual.transform.ops.sort",
        TransformStep::Limit { .. } => "notebook.visual.transform.ops.limit",
        TransformStep::Pivot { .. } => "notebook.visual.transform.ops.pivot",
        TransformStep::Unpivot { .. } => "notebook.visual.transform.ops.unpivot",
    }
}

fn test_key(test: FilterTest) -> &'static str {
    match test {
        FilterTest::Eq => "notebook.visual.transform.tests.eq",
        FilterTest::Ne => "notebook.visual.transform.tests.ne",
        FilterTest::Lt => "notebook.visual.transform.tests.lt",
        FilterTest::Le => "notebook.visual.transform.tests.le",
        FilterTest::Gt => "notebook.visual.transform.tests.gt",
        FilterTest::Ge => "notebook.visual.transform.tests.ge",
        FilterTest::Contains => "notebook.visual.transform.tests.contains",
        FilterTest::StartsWith => "notebook.visual.transform.tests.starts_with",
        FilterTest::IsNull => "notebook.visual.transform.tests.is_null",
        FilterTest::NotNull => "notebook.visual.transform.tests.not_null",
        FilterTest::In => "notebook.visual.transform.tests.in",
    }
}

fn function_key(function: AggFunction) -> &'static str {
    match function {
        AggFunction::Sum => "notebook.visual.transform.functions.sum",
        AggFunction::Avg => "notebook.visual.transform.functions.avg",
        AggFunction::Min => "notebook.visual.transform.functions.min",
        AggFunction::Max => "notebook.visual.transform.functions.max",
        AggFunction::Count => "notebook.visual.transform.functions.count",
        AggFunction::CountDistinct => "notebook.visual.transform.functions.count_distinct",
        AggFunction::Median => "notebook.visual.transform.functions.median",
    }
}

fn join_key(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "notebook.visual.transform.joins.inner",
        JoinKind::Left => "notebook.visual.transform.joins.left",
        JoinKind::Right => "notebook.visual.transform.joins.right",
        JoinKind::Full => "notebook.visual.transform.joins.full",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_as_numbers_booleans_or_text() {
        assert_eq!(parse_value(" 42 "), Value::String("42".into()));
        assert_eq!(parse_value("007"), Value::String("007".into()));
        assert_eq!(
            parse_value("12345678901234567890"),
            Value::String("12345678901234567890".into())
        );
        assert_eq!(parse_value("TRUE"), Value::String("TRUE".into()));
        assert_eq!(parse_value("West"), Value::String("West".into()));
        assert_eq!(parse_value(""), Value::Null);
    }

    #[test]
    fn step_columns_follow_earlier_steps() {
        let steps = vec![
            TransformStep::Compute {
                name: "net".into(),
                expression: "a - b".into(),
            },
            TransformStep::Group {
                by: vec!["region".into()],
                aggregates: vec![AggregateColumn {
                    function: AggFunction::Sum,
                    column: Some("net".into()),
                    alias: None,
                }],
            },
            TransformStep::Rename {
                column: "sum_net".into(),
                to: "revenue".into(),
            },
        ];
        let input = vec!["region".to_string(), "a".into(), "b".into()];
        let none = |_: &str| Vec::new();
        assert_eq!(
            columns_before(&steps, 1, &input, &none, &none),
            ["region", "a", "b", "net"]
        );
        assert_eq!(
            columns_before(&steps, 3, &input, &none, &none),
            ["region", "revenue"]
        );
    }

    #[test]
    fn steps_after_a_pivot_see_the_pivoted_columns() {
        let col = |name: &str| ResultColumn {
            name: name.into(),
            data_type: "varchar".into(),
            nullable: true,
        };
        let columns = vec![col("country"), col("year"), col("amount")];
        let rows = vec![
            vec![
                CellValue::String("DE".into()),
                CellValue::I64(2026),
                CellValue::F64(1.0),
            ],
            vec![
                CellValue::String("FR".into()),
                CellValue::I64(2025),
                CellValue::F64(2.0),
            ],
            vec![
                CellValue::String("DE".into()),
                CellValue::I64(2025),
                CellValue::F64(3.0),
            ],
            vec![
                CellValue::String("DE".into()),
                CellValue::Null,
                CellValue::F64(4.0),
            ],
        ];
        let input: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let values = |c: &str| distinct_values(&columns, &rows, c);
        let none = |_: &str| Vec::new();
        let grouped = vec![
            TransformStep::Pivot {
                on: "year".into(),
                value: "amount".into(),
                function: AggFunction::Sum,
                group_by: vec!["country".into()],
            },
            TransformStep::Limit { rows: 5 },
        ];
        assert_eq!(
            columns_before(&grouped, 1, &input, &none, &values),
            ["country", "2025", "2026"]
        );
        let implicit = vec![
            TransformStep::Pivot {
                on: "year".into(),
                value: "amount".into(),
                function: AggFunction::Sum,
                group_by: Vec::new(),
            },
            TransformStep::Limit { rows: 5 },
        ];
        assert_eq!(
            columns_before(&implicit, 1, &input, &none, &values),
            ["country", "2025", "2026"],
            "without GROUP BY the other columns group the rows"
        );
        assert_eq!(
            columns_before(&grouped, 0, &input, &none, &values),
            ["country", "year", "amount"],
            "the pivot itself still sees its input"
        );
    }
}
