//! Scalar (formula) cells: one value from earlier outputs and parameters,
//! shown large with its format. Markdown shows it with `{{name}}`, and SQL
//! that references it gets the value as a literal of the cell's engine
//! ([`bind_scalar_refs`]).

use std::collections::BTreeMap;

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, div, px};
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_services::analytics::scalar::{
    Evaluation, MapEnv, ScalarError, ScalarValue, evaluate_detailed, parse,
};
use sqlly_services::notebook::cells::output_view;
use sqlly_services::notebook::cells::scalar::{
    ScalarCellSpec, display, is_partial, output_rows, sql_literal,
};
use sqlly_services::notebook::cells::spec;
use sqlly_services::notebook::references::{RefTarget, cell_references};
use sqlly_services::notebook::{CellKind, Dialects, NotebookDoc};
use sqlly_services::report::format::{NumberStyle, ValueFormat};

use super::{
    VisualCellView, VisualInputs, choice_button, dim, editor_panel, editor_row, t, text_field,
};
use crate::theme::Theme;

/// `doc` with every SQL cell's `{{scalar}}` reference replaced by the
/// scalar's current value as a literal, or `None` when nothing changed.
pub fn bind_scalar_refs(
    doc: &NotebookDoc,
    dialects: &Dialects,
    values: &BTreeMap<String, ScalarValue>,
) -> Option<NotebookDoc> {
    if values.is_empty() {
        return None;
    }
    let mut out: Option<NotebookDoc> = None;
    for (index, cell) in doc.cells.iter().enumerate() {
        if cell.kind != CellKind::Sql {
            continue;
        }
        let db = dialects.for_cell(doc, cell);
        let mut text = String::new();
        let mut last = 0;
        for r in cell_references(doc, cell, dialects) {
            let RefTarget::Cell(token) = &r.target else {
                continue;
            };
            let Some(target) = doc.resolve(token).filter(|c| c.kind == CellKind::Scalar) else {
                continue;
            };
            let Some(value) = values.get(&target.id) else {
                continue;
            };
            let Ok(literal) = sql_literal(db, target.relation_name(), value) else {
                continue;
            };
            text.push_str(&cell.source[last..r.start]);
            text.push_str(&literal);
            last = r.end;
        }
        if last == 0 {
            continue;
        }
        text.push_str(&cell.source[last..]);
        out.get_or_insert_with(|| doc.clone()).cells[index].source = text;
    }
    out
}

fn scalar_error(error: &ScalarError) -> DisplayMessage {
    use crate::localization::{deferred, deferred_text};
    match error {
        ScalarError::Parse { at, .. } => deferred(
            "notebook.visual.scalar.errors.parse",
            &[("position", (at + 1).to_string())],
        ),
        ScalarError::UnknownReference { name } => deferred(
            "notebook.visual.scalar.errors.unknown_reference",
            &[("name", name.as_str())],
        ),
        ScalarError::UnknownColumn { output, column } => deferred(
            "notebook.visual.scalar.errors.unknown_column",
            &[
                ("cell_name", output.as_str()),
                ("column_name", column.as_str()),
            ],
        ),
        ScalarError::NotScalar { reference, .. } => deferred(
            "notebook.visual.scalar.errors.not_scalar",
            &[("name", reference.as_str())],
        ),
        ScalarError::DivisionByZero => {
            deferred_text("notebook.visual.scalar.errors.division_by_zero")
        }
        _ => deferred_text("notebook.visual.scalar.errors.other"),
    }
}

/// The formula's value over the inputs.
pub(crate) fn value_of(inputs: &VisualInputs) -> Result<ScalarValue, DisplayMessage> {
    evaluation_of(inputs).map(|e| e.value)
}

/// The formula's value, and whether it read partial data or lost digits.
pub(crate) fn evaluation_of(inputs: &VisualInputs) -> Result<Evaluation, DisplayMessage> {
    if inputs.source.trim().is_empty() {
        return Err(crate::localization::deferred_text(
            "notebook.visual.scalar.empty",
        ));
    }
    let expr = parse(&inputs.source).map_err(|e| scalar_error(&e))?;
    let mut env = MapEnv {
        values: inputs.values.clone(),
        ..MapEnv::default()
    };
    for (name, output) in &inputs.named_outputs {
        if inputs.values.contains_key(name) && is_partial(output.completeness) {
            env.partial_values.insert(name.clone());
        }
        env.outputs.insert(name.clone(), output_view(output));
    }
    evaluate_detailed(&expr, &env).map_err(|e| scalar_error(&e))
}

pub(crate) fn evaluate(
    inputs: &VisualInputs,
) -> Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), DisplayMessage> {
    let value = value_of(inputs)?;
    Ok(output_rows(&inputs.name, &value))
}

impl VisualCellView {
    fn scalar_spec(&self) -> ScalarCellSpec {
        spec::read(&self.inputs.spec)
    }

    pub(crate) fn render_scalar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let id = self.inputs.cell_id.clone();
        let cell = self.scalar_spec();
        let expression = self.field(
            &format!("scalar.expression.{id}"),
            &self.inputs.source.clone(),
            "notebook.visual.scalar.expression_placeholder",
            window,
            cx,
            |v, text, cx| {
                v.inputs.source = text.to_string();
                cx.emit(super::VisualEvent::SourceChanged {
                    source: text.to_string(),
                    spec: v.inputs.spec.clone(),
                });
                cx.notify();
            },
        );
        let mut root = div().flex().flex_col().w_full();
        let formats = [
            (NumberStyle::Plain, "plain"),
            (NumberStyle::Percent, "percent"),
            (NumberStyle::Currency, "currency"),
            (NumberStyle::Compact, "compact"),
        ];
        let current = cell.format.as_ref().map(|f| f.style).unwrap_or_default();
        let format_label = |n: NumberStyle| {
            formats
                .iter()
                .find(|(x, _)| *x == n)
                .map(|(_, k)| t(&format!("notebook.visual.chart.formats.{k}")))
                .unwrap_or_default()
        };
        let this = cx.entity();
        let mut editor = editor_panel().child(editor_row(
            t("notebook.visual.scalar.expression"),
            text_field(&expression, 420.0),
        ));
        if self.editing {
            editor = editor
                .child(editor_row(
                    t("notebook.visual.scalar.format"),
                    choice_button(
                        format!("nb-scalar-format-{id}"),
                        format_label(current),
                        formats
                            .iter()
                            .map(|(f, _)| (format_label(*f), *f, *f == current))
                            .collect(),
                        this,
                        |v, style, cx| {
                            let mut cell = v.scalar_spec();
                            cell.format = match style {
                                NumberStyle::Plain => None,
                                NumberStyle::Currency => Some(ValueFormat::currency("USD", 2)),
                                NumberStyle::Percent => Some(ValueFormat::percent(1)),
                                NumberStyle::Compact => Some(ValueFormat::compact()),
                            };
                            v.emit_spec(spec::write(&cell), cx);
                        },
                    ),
                ))
                .child(dim(crate::localization::format(
                    "notebook.visual.scalar.reference_help",
                    &[("reference", format!("{{{{{}}}}}", self.inputs.name))],
                )));
        }
        root = root.child(editor);
        let shown = match evaluation_of(&self.inputs) {
            Ok(evaluation) => {
                let mut shown = div().flex().flex_col().child(
                    div()
                        .text_size(px(Theme::font_size_ratio(2.2)))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Theme::text())
                        .child(display(&evaluation.value, cell.format.as_ref())),
                );
                if evaluation.partial {
                    shown = shown.child(dim(t("notebook.visual.scalar.partial")));
                }
                if evaluation.approximate {
                    shown = shown.child(dim(t("notebook.visual.scalar.approximate")));
                }
                shown
            }
            Err(message) => div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::warning())
                .child(crate::localization::message(message)),
        };
        root.child(div().px(px(s(14.0))).py(px(s(8.0))).child(shown))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;
    use sqlly_services::notebook::{ExecutionPolicy, SeededIds};

    #[test]
    fn sql_references_to_scalars_bind_as_literals() {
        let mut ids = SeededIds(4);
        let mut doc = NotebookDoc::new("t", &mut ids);
        let scalar = doc.add_cell(CellKind::Scalar, None, &mut ids);
        doc.cells[scalar].name = Some("threshold".into());
        doc.cells[0].source =
            "SELECT * FROM t WHERE amount > {{threshold}} AND note <> '{{threshold}}'".into();
        let mut values = BTreeMap::new();
        values.insert(doc.cells[scalar].id.clone(), ScalarValue::Number(250.5));
        let dialects = Dialects::single(DatabaseType::SqlServer);
        let bound = bind_scalar_refs(&doc, &dialects, &values).unwrap();
        assert_eq!(
            bound.cells[0].source,
            "SELECT * FROM t WHERE amount > 250.5 AND note <> '{{threshold}}'"
        );
        assert!(bind_scalar_refs(&doc, &dialects, &BTreeMap::new()).is_none());
        let _ = ExecutionPolicy::default();
    }
}
