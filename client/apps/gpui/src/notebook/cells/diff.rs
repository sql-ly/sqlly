//! Diff cells: compare two results (two cells, a cell now against an
//! earlier run or a pinned snapshot, or the same query on another
//! connection) with key columns, column renames and tolerances. Shows the
//! counts, schema changes, and added, removed and changed rows in their own
//! grids.

use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputState};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::analytics::diff::{DiffSideName, OutputDiff, OutputDiffError, Tolerance};
use sqlly_services::notebook::cells::diff::{DiffCellSpec, DiffSide, SideSource};

use super::{CellRenderCx, NotebookCell};
use crate::notebook::analysis::ui::{self, caption, chip, field, picker, t};
use crate::notebook::view::NotebookView;
use crate::theme::Theme;

pub struct DiffCell;

impl NotebookCell for DiffCell {
    fn kind(&self) -> &'static str {
        crate::notebook::analysis::KIND_DIFF
    }

    fn label_key(&self) -> &'static str {
        "notebook.analysis.kind.diff"
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

fn side_text(side: DiffSideName) -> String {
    t(match side {
        DiffSideName::Left => "notebook.analysis.diff.side_left",
        DiffSideName::Right => "notebook.analysis.diff.side_right",
    })
}

pub(crate) fn diff_error_text(error: &OutputDiffError) -> String {
    capitalized(match error {
        OutputDiffError::TooManyRows { side, limit } => crate::localization::format(
            "notebook.analysis.diff.error_too_many",
            &[("side", side_text(*side)), ("count", limit.to_string())],
        ),
        OutputDiffError::MissingKeyColumn { side, column } => crate::localization::format(
            "notebook.analysis.diff.error_missing_key",
            &[("side", side_text(*side)), ("column", column.clone())],
        ),
        OutputDiffError::NullKey { side, row } => crate::localization::format(
            "notebook.analysis.diff.error_null_key",
            &[("side", side_text(*side)), ("row", (row + 1).to_string())],
        ),
        OutputDiffError::DuplicateKey { side, row } => crate::localization::format(
            "notebook.analysis.diff.error_duplicate_key",
            &[("side", side_text(*side)), ("row", (row + 1).to_string())],
        ),
        OutputDiffError::NoCommonColumns => t("notebook.analysis.diff.error_no_columns"),
    })
}

/// The message with its first letter upper-cased (it may start with a side name).
fn capitalized(text: String) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// Changed cells as a table: key, column, before, after, difference.
pub(crate) fn changed_table(diff: &OutputDiff) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let col = |key: &str, ty: &str| ResultColumn {
        name: t(key),
        data_type: ty.into(),
        nullable: true,
    };
    let columns = vec![
        col("notebook.analysis.diff.col_row", "nvarchar"),
        col("notebook.analysis.diff.col_column", "nvarchar"),
        col("notebook.analysis.diff.col_before", "nvarchar"),
        col("notebook.analysis.diff.col_after", "nvarchar"),
        col("notebook.analysis.diff.col_delta", "float"),
    ];
    let mut rows = Vec::new();
    for row in &diff.changed {
        let key = if row.key.is_empty() {
            format!("#{}", row.right_index + 1)
        } else {
            row.key
                .iter()
                .map(|v| {
                    sqlly_services::analytics::value::cell_text(v)
                        .unwrap_or_default()
                        .into_owned()
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        for change in &row.changes {
            rows.push(vec![
                CellValue::String(key.clone()),
                CellValue::String(change.column.clone()),
                change.left.clone(),
                change.right.clone(),
                change.numeric_delta.map_or(CellValue::Null, CellValue::F64),
            ]);
        }
    }
    (columns, rows)
}

/// The schema changes as lines of text.
pub(crate) fn schema_lines(diff: &OutputDiff) -> Vec<String> {
    let s = &diff.schema;
    let mut out = Vec::new();
    for c in &s.added_columns {
        out.push(crate::localization::format(
            "notebook.analysis.diff.schema_added",
            &[("column", c.as_str())],
        ));
    }
    for c in &s.removed_columns {
        out.push(crate::localization::format(
            "notebook.analysis.diff.schema_removed",
            &[("column", c.as_str())],
        ));
    }
    for c in &s.type_changes {
        out.push(crate::localization::format(
            "notebook.analysis.diff.schema_type",
            &[
                ("column", c.left_column.as_str()),
                ("before", c.left_type.as_str()),
                ("after", c.right_type.as_str()),
            ],
        ));
    }
    for (a, b) in &s.renamed {
        out.push(crate::localization::format(
            "notebook.analysis.diff.schema_renamed",
            &[("before", a.as_str()), ("after", b.as_str())],
        ));
    }
    if s.reordered {
        out.push(t("notebook.analysis.diff.schema_reordered"));
    }
    out
}

struct Shown {
    result: Result<OutputDiff, String>,
    added: Option<Entity<SqllyDataTable>>,
    removed: Option<Entity<SqllyDataTable>>,
    changed: Option<Entity<SqllyDataTable>>,
}

pub struct DiffView {
    notebook: WeakEntity<NotebookView>,
    cell_id: String,
    shown: Option<Shown>,
    keys_input: Option<Entity<InputState>>,
    tolerance_input: Option<Entity<InputState>>,
}

fn source_value(source: &SideSource) -> String {
    match source {
        SideSource::Current => "current".into(),
        SideSource::PreviousRun => "previous".into(),
        SideSource::Run(id) => format!("run:{id}"),
        SideSource::Snapshot(n) => format!("snapshot:{n}"),
        SideSource::Connection(s) => format!("connection:{s}"),
    }
}

fn parse_source(value: &str) -> SideSource {
    match value {
        "previous" => SideSource::PreviousRun,
        v if v.starts_with("run:") => SideSource::Run(v[4..].to_string()),
        v if v.starts_with("snapshot:") => SideSource::Snapshot(v[9..].to_string()),
        v if v.starts_with("connection:") => SideSource::Connection(v[11..].to_string()),
        _ => SideSource::Current,
    }
}

impl DiffView {
    pub fn new(notebook: WeakEntity<NotebookView>, cell_id: String) -> Self {
        Self {
            notebook,
            cell_id,
            shown: None,
            keys_input: None,
            tolerance_input: None,
        }
    }

    #[cfg(test)]
    pub fn result(&self) -> Option<&Result<OutputDiff, String>> {
        self.shown.as_ref().map(|s| &s.result)
    }

    pub fn set_result(
        &mut self,
        result: Result<OutputDiff, String>,
        left: Vec<ResultColumn>,
        right: Vec<ResultColumn>,
        cx: &mut Context<Self>,
    ) {
        let (added, removed, changed) = match &result {
            Ok(diff) => {
                let rows = |r: &[sqlly_services::analytics::diff::DiffRow]| -> Vec<Vec<CellValue>> {
                    r.iter().map(|d| d.values.clone()).collect()
                };
                let (cc, cr) = changed_table(diff);
                (
                    (!diff.added.is_empty())
                        .then(|| ui::grid(&right, &rows(&diff.added), cx))
                        .flatten(),
                    (!diff.removed.is_empty())
                        .then(|| ui::grid(&left, &rows(&diff.removed), cx))
                        .flatten(),
                    (!cr.is_empty()).then(|| ui::grid(&cc, &cr, cx)).flatten(),
                )
            }
            Err(_) => (None, None, None),
        };
        self.shown = Some(Shown {
            result,
            added,
            removed,
            changed,
        });
        cx.notify();
    }

    fn spec(&self, cx: &gpui::App) -> DiffCellSpec {
        self.notebook
            .upgrade()
            .map(|nb| DiffCellSpec::from_spec(&nb.read(cx).doc.spec(&self.cell_id)))
            .unwrap_or_default()
    }

    fn edit(&self, cx: &mut gpui::App, change: impl FnOnce(&mut DiffCellSpec)) {
        Self::edit_at(&self.notebook, &self.cell_id, cx, change);
    }

    fn edit_view(view: &Entity<Self>, cx: &mut gpui::App, change: impl FnOnce(&mut DiffCellSpec)) {
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
        change: impl FnOnce(&mut DiffCellSpec),
    ) {
        let Some(nb) = notebook.upgrade() else {
            return;
        };
        nb.update(cx, |nb, cx| {
            let mut spec = DiffCellSpec::from_spec(&nb.doc.spec(id));
            change(&mut spec);
            nb.set_cell_spec(id, spec.to_spec(), cx);
        });
    }

    fn side_controls(
        &self,
        left: bool,
        side: &DiffSide,
        choices: &[(String, String)],
        sources: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.weak_entity();
        let current = choices
            .iter()
            .find(|(id, l)| *id == side.cell || l.eq_ignore_ascii_case(&side.cell))
            .cloned();
        let on_cell: ui::OnChoose = std::rc::Rc::new({
            let this = this.clone();
            move |value, app| {
                if let Some(v) = this.upgrade() {
                    Self::edit_view(&v, app, |s| {
                        if left {
                            s.left.cell = value;
                        } else {
                            s.right.cell = value;
                        }
                    });
                }
            }
        });
        let on_source: ui::OnChoose = std::rc::Rc::new(move |value, app| {
            if let Some(v) = this.upgrade() {
                Self::edit_view(&v, app, |s| {
                    let source = parse_source(&value);
                    if left {
                        s.left.source = source;
                    } else {
                        s.right.source = source;
                    }
                });
            }
        });
        let value = source_value(&side.source);
        let label = sources
            .iter()
            .find(|(v, _)| *v == value)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| value.clone());
        let prefix = if left {
            "nb-diff-left"
        } else {
            "nb-diff-right"
        };
        field(
            t(if left {
                "notebook.analysis.diff.left"
            } else {
                "notebook.analysis.diff.right"
            }),
            div()
                .flex()
                .flex_row()
                .gap(px(Theme::scaled(4.0)))
                .child(picker(
                    format!("{prefix}-cell"),
                    current
                        .as_ref()
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| t("notebook.analysis.input_choose")),
                    choices.to_vec(),
                    current.map(|(id, _)| id),
                    on_cell,
                ))
                .child(picker(
                    format!("{prefix}-source"),
                    label,
                    sources.to_vec(),
                    Some(value),
                    on_source,
                )),
        )
        .into_any_element()
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let output_only =
            crate::notebook::present::cell_output_only(&self.notebook, &self.cell_id, cx);
        let spec = self.spec(cx);
        if self.keys_input.is_none() {
            self.keys_input = Some(ui::input(
                window,
                cx,
                "notebook.analysis.diff.keys_placeholder",
                &spec.keys.join(", "),
                |this: &mut Self, text, cx| {
                    let keys: Vec<String> = text
                        .split(',')
                        .map(str::trim)
                        .filter(|k| !k.is_empty())
                        .map(str::to_string)
                        .collect();
                    this.edit(cx, |s| s.keys = keys);
                },
            ));
        }
        if self.tolerance_input.is_none() {
            let value = spec
                .tolerance
                .map(|t| t.absolute.to_string())
                .unwrap_or_default();
            self.tolerance_input = Some(ui::input(
                window,
                cx,
                "notebook.analysis.diff.tolerance_placeholder",
                &value,
                |this: &mut Self, text, cx| {
                    let text = text.trim().to_string();
                    let tolerance = if text.is_empty() {
                        Some(None)
                    } else {
                        text.parse::<f64>()
                            .ok()
                            .filter(|f| f.is_finite() && *f >= 0.0)
                            .map(|absolute| {
                                Some(Tolerance {
                                    absolute,
                                    relative: 0.0,
                                })
                            })
                    };
                    if let Some(tolerance) = tolerance {
                        this.edit(cx, |s| s.tolerance = tolerance);
                    }
                },
            ));
        }
        let (choices, snapshots, slots, running) = self
            .notebook
            .upgrade()
            .map(|nb| {
                let nb = nb.read(cx);
                (
                    nb.input_choices(&self.cell_id),
                    nb.snapshot_names(),
                    nb.doc
                        .model()
                        .connection_refs
                        .iter()
                        .map(|c| (c.id.clone(), c.label.clone()))
                        .collect::<Vec<_>>(),
                    nb.cell_state(&self.cell_id).is_active(),
                )
            })
            .unwrap_or_default();
        let mut sources = vec![
            (
                "current".to_string(),
                t("notebook.analysis.diff.source_current"),
            ),
            (
                "previous".to_string(),
                t("notebook.analysis.diff.source_previous"),
            ),
        ];
        for name in &snapshots {
            sources.push((
                format!("snapshot:{name}"),
                crate::localization::format(
                    "notebook.analysis.diff.source_snapshot",
                    &[("name", name.as_str())],
                ),
            ));
        }
        let mut right_sources = sources.clone();
        for (slot, label) in &slots {
            right_sources.push((
                format!("connection:{slot}"),
                crate::localization::format(
                    "notebook.analysis.diff.source_connection",
                    &[("connection_name", label.as_str())],
                ),
            ));
        }
        let left = self.side_controls(true, &spec.left, &choices, &sources, cx);
        let right = self.side_controls(false, &spec.right, &choices, &right_sources, cx);
        let notebook = self.notebook.clone();
        let id = self.cell_id.clone();
        let toolbar = div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(s(10.0)))
                    .child(left)
                    .child(right),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(8.0)))
                    .children(self.keys_input.clone().map(|i| {
                        field(
                            t("notebook.analysis.diff.keys"),
                            div().w(px(s(160.0))).child(Input::new(&i).small()),
                        )
                    }))
                    .children(self.tolerance_input.clone().map(|i| {
                        field(
                            t("notebook.analysis.diff.tolerance"),
                            div().w(px(s(90.0))).child(Input::new(&i).small()),
                        )
                    }))
                    .child(
                        ui::checkbox(
                            "nb-diff-case",
                            t("notebook.analysis.diff.ignore_case"),
                            spec.case_insensitive,
                        )
                        .on_click(cx.listener(|this, _: &bool, _, cx| {
                            this.edit(cx, |s| s.case_insensitive = !s.case_insensitive)
                        })),
                    )
                    .child(
                        ui::checkbox(
                            "nb-diff-space",
                            t("notebook.analysis.diff.ignore_whitespace"),
                            spec.normalize_whitespace,
                        )
                        .on_click(cx.listener(|this, _: &bool, _, cx| {
                            this.edit(cx, |s| s.normalize_whitespace = !s.normalize_whitespace)
                        })),
                    )
                    .child(
                        ui::primary_button("nb-diff-run", t("notebook.analysis.diff.run"))
                            .disabled(running)
                            .on_click(move |_, _, app| {
                                if let Some(nb) = notebook.upgrade() {
                                    nb.update(app, |nb, cx| nb.request_run_from_host(&id, cx));
                                }
                            }),
                    ),
            );
        let mut root = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .px(px(s(10.0)))
            .py(px(s(8.0)))
            .w_full()
            .when(!output_only, |d| d.child(toolbar));
        let Some(shown) = &self.shown else {
            return root.child(caption(t("notebook.analysis.diff.empty")));
        };
        let diff = match &shown.result {
            Ok(d) => d,
            Err(e) => {
                return root.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::error())
                        .child(e.clone()),
                );
            }
        };
        let sum = &diff.summary;
        root = root
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(s(6.0)))
                    .child(caption(crate::localization::format(
                        "notebook.analysis.diff.rows",
                        &[
                            ("before", sum.left_rows.to_string()),
                            ("after", sum.right_rows.to_string()),
                        ],
                    )))
                    .child(chip(
                        crate::localization::format(
                            "notebook.analysis.diff.added",
                            &[("count", sum.added.to_string())],
                        ),
                        Theme::success(),
                    ))
                    .child(chip(
                        crate::localization::format(
                            "notebook.analysis.diff.removed",
                            &[("count", sum.removed.to_string())],
                        ),
                        Theme::error(),
                    ))
                    .child(chip(
                        crate::localization::format(
                            "notebook.analysis.diff.changed",
                            &[("count", sum.changed.to_string())],
                        ),
                        Theme::warning(),
                    ))
                    .child(caption(crate::localization::format(
                        "notebook.analysis.diff.unchanged",
                        &[("count", sum.unchanged.to_string())],
                    ))),
            )
            .when(!diff.is_conclusive(), |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::warning())
                        .child(t("notebook.analysis.diff.inconclusive")),
                )
            })
            .when(diff.detail_truncated, |d| {
                d.child(caption(t("notebook.analysis.diff.detail_truncated")))
            })
            .children(schema_lines(diff).into_iter().map(caption));
        for (key, grid) in [
            ("notebook.analysis.diff.added_rows", shown.added.clone()),
            ("notebook.analysis.diff.removed_rows", shown.removed.clone()),
            (
                "notebook.analysis.diff.changed_cells",
                shown.changed.clone(),
            ),
        ] {
            if grid.is_some() {
                root = root
                    .child(ui::section(t(key)))
                    .child(ui::grid_box(grid, 150.0));
            }
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlly_services::analytics::OutputView;
    use sqlly_services::analytics::diff::diff_outputs;

    fn col(n: &str) -> ResultColumn {
        ResultColumn {
            name: n.into(),
            data_type: "int".into(),
            nullable: true,
        }
    }

    #[test]
    fn changed_cells_and_schema_changes_are_listed() {
        let left_cols = [col("id"), col("v"), col("gone")];
        let right_cols = [col("id"), col("v"), col("new")];
        let left = vec![
            vec![CellValue::I64(1), CellValue::I64(10), CellValue::Null],
            vec![CellValue::I64(2), CellValue::I64(20), CellValue::Null],
        ];
        let right = vec![
            vec![CellValue::I64(1), CellValue::I64(11), CellValue::Null],
            vec![CellValue::I64(3), CellValue::I64(30), CellValue::Null],
        ];
        let spec = DiffCellSpec::from_spec(&json!({"inputs": ["a", "b"], "keys": ["id"]}));
        let diff = diff_outputs(
            &OutputView::complete(&left_cols, &left),
            &OutputView::complete(&right_cols, &right),
            &spec.options(),
        )
        .unwrap();
        let (cols, rows) = changed_table(&diff);
        assert_eq!(cols.len(), 5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][2], CellValue::I64(10));
        assert_eq!(rows[0][4], CellValue::F64(1.0));
        let lines = schema_lines(&diff);
        assert!(lines.iter().any(|l| l.contains("new")));
        assert!(lines.iter().any(|l| l.contains("gone")));
        let e = diff_error_text(&OutputDiffError::MissingKeyColumn {
            side: DiffSideName::Right,
            column: "id".into(),
        });
        assert!(e.contains("id"));
        for v in [
            "current",
            "previous",
            "run:r_1",
            "snapshot:x",
            "connection:s",
        ] {
            assert_eq!(source_value(&parse_source(v)), v);
        }
    }
}
