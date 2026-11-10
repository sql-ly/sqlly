//! Batch Insert — stage 1–1000 template rows with per-column sources.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::{CellValue, DatabaseType};
use sqlly_services::row_editing::{RowEditColumn, RowEditSession, render_preview_script};

use crate::dialog::{Dialog, DialogFrame};
use crate::pending_changes::PendingChanges;
use crate::row_editor_model::{
    FieldDraft, FieldKind, cell_to_draft, coerce_text, field_kind_for, validate_text,
};
use crate::test_data_generator::{
    Rng, ValueGenerator, generated_cell, infer_generator, type_limits,
};
use crate::theme::Theme;

pub const BATCH_MIN: usize = 1;
pub const BATCH_MAX: usize = 1000;

fn source_kind_labels() -> [String; 6] {
    [
        crate::localization::text("results.batch_insert.source_skip"),
        crate::localization::text("results.batch_insert.source_constant"),
        crate::localization::text("results.batch_insert.source_null"),
        crate::localization::text("results.batch_insert.source_default"),
        crate::localization::text("results.batch_insert.source_sequence"),
        crate::localization::text("results.batch_insert.source_generated"),
    ]
}

fn generator_label(generator: ValueGenerator) -> String {
    let key = match generator {
        ValueGenerator::Name => "results.batch_insert.generator_name",
        ValueGenerator::Word => "results.batch_insert.generator_word",
        ValueGenerator::Text => "results.batch_insert.generator_text",
        ValueGenerator::Email => "results.batch_insert.generator_email",
        ValueGenerator::Integer => "results.batch_insert.generator_integer",
        ValueGenerator::Decimal => "results.batch_insert.generator_decimal",
        ValueGenerator::Boolean => "results.batch_insert.generator_boolean",
        ValueGenerator::Date => "results.batch_insert.generator_date",
        ValueGenerator::DateTime => "results.batch_insert.generator_datetime",
        ValueGenerator::Time => "results.batch_insert.generator_time",
        ValueGenerator::Guid => "results.batch_insert.generator_guid",
        ValueGenerator::Json => "results.batch_insert.generator_json",
        ValueGenerator::Binary => "results.batch_insert.generator_binary",
    };
    crate::localization::text(key)
}

/// How a column is filled for each generated row.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnSource {
    /// Same literal for every row (parsed with `paste_rows` coercion).
    Constant(String),
    Null,
    /// Omit the column so the server applies its DEFAULT.
    Default,
    /// `start + i * step` as I64.
    Sequence {
        start: i64,
        step: i64,
    },
    /// Reuse the test-data generator family.
    Generated(ValueGenerator),
    /// Identity / computed / rowversion — never written.
    Skip,
}

#[derive(Clone, Debug)]
pub struct BatchInsertModel {
    pub session: RowEditSession,
    pub database_type: DatabaseType,
    pub count: usize,
    pub seed: u64,
    /// Aligned with `session.columns`.
    pub sources: Vec<ColumnSource>,
}

impl BatchInsertModel {
    /// Defaults: Skip for db-generated columns; Sequence for a non-identity
    /// integer PK; Constant(seed text) for columns with a template value;
    /// Default for nullable/defaulted columns; Generated(infer_generator(..))
    /// for NOT NULL columns without a template.
    pub fn from_template(
        session: RowEditSession,
        template: Option<Vec<CellValue>>,
        db: DatabaseType,
    ) -> Self {
        let sources = session
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| source_for_column(col, i, template.as_deref(), db))
            .collect();
        Self {
            session,
            database_type: db,
            count: 10,
            seed: 1,
            sources,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.count < BATCH_MIN || self.count > BATCH_MAX {
            return Err(crate::localization::format(
                "results.batch_insert.count_out_of_range",
                &[
                    ("minimum", &BATCH_MIN.to_string()),
                    ("maximum", &BATCH_MAX.to_string()),
                ],
            ));
        }
        for (col, source) in self.session.columns.iter().zip(&self.sources) {
            match source {
                ColumnSource::Null if !col.nullable => {
                    return Err(crate::localization::format(
                        "results.batch_insert.null_not_allowed",
                        &[("column_name", &col.name)],
                    ));
                }
                ColumnSource::Constant(text) => {
                    let kind = field_kind_for(self.database_type, &col.data_type);
                    if let Some(reason) = validate_text(text, kind, col) {
                        return Err(crate::localization::format(
                            "results.batch_insert.column_validation_failed",
                            &[("column_name", &col.name), ("reason", &reason)],
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The staged rows: `count` entries of (column, value) pairs, omitting Skip/Default.
    pub fn rows(&self) -> Result<Vec<Vec<(String, CellValue)>>, String> {
        self.validate()?;
        let mut rng = Rng::new(self.seed);
        let mut out = Vec::with_capacity(self.count);
        for i in 0..self.count {
            let mut row = Vec::new();
            for (col, source) in self.session.columns.iter().zip(&self.sources) {
                match source {
                    ColumnSource::Skip | ColumnSource::Default => {}
                    ColumnSource::Null => {
                        row.push((col.name.clone(), CellValue::Null));
                    }
                    ColumnSource::Constant(text) => {
                        let kind = field_kind_for(self.database_type, &col.data_type);
                        row.push((col.name.clone(), coerce_text(text, kind)));
                    }
                    ColumnSource::Sequence { start, step } => {
                        let value = start.saturating_add((i as i64).saturating_mul(*step));
                        row.push((col.name.clone(), CellValue::I64(value)));
                    }
                    ColumnSource::Generated(generator) => {
                        let limits = type_limits(&col.data_type);
                        row.push((
                            col.name.clone(),
                            generated_cell(self.database_type, *generator, limits, &mut rng),
                        ));
                    }
                }
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Preview: `render_preview_script` of `PendingChanges { session, new_rows }.to_batch()`.
    pub fn preview_sql(&self) -> Result<String, String> {
        let new_rows = self
            .rows()?
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(name, value)| (name, crate::pending_changes::CellEdit::Value(value)))
                    .collect()
            })
            .collect();
        let pending = PendingChanges {
            session: Some(self.session.clone()),
            new_rows,
            ..Default::default()
        };
        Ok(render_preview_script(
            self.database_type,
            &pending.to_batch(self.database_type),
        ))
    }
}

fn source_for_column(
    col: &RowEditColumn,
    ordinal: usize,
    template: Option<&[CellValue]>,
    db: DatabaseType,
) -> ColumnSource {
    let db_generated = col.is_identity || col.is_computed || col.is_rowversion;
    if db_generated {
        return ColumnSource::Skip;
    }
    let kind = field_kind_for(db, &col.data_type);
    if col.is_primary_key && matches!(kind, FieldKind::Integer { .. }) {
        let start = template
            .and_then(|row| row.get(ordinal))
            .and_then(|cell| match cell {
                CellValue::I64(n) => Some(*n),
                _ => None,
            })
            .unwrap_or(1);
        return ColumnSource::Sequence { start, step: 1 };
    }
    if let Some(cell) = template.and_then(|row| row.get(ordinal))
        && !matches!(cell, CellValue::Null)
    {
        let text = match cell_to_draft(cell) {
            FieldDraft::Text(s) => s,
            FieldDraft::Null => String::new(),
            FieldDraft::Expression(_) | FieldDraft::Bytes(_) => String::new(),
        };
        return ColumnSource::Constant(text);
    }
    if col.nullable || col.default_definition.is_some() {
        return ColumnSource::Default;
    }
    ColumnSource::Generated(infer_generator(&col.name, &col.data_type))
}

fn source_kind_index(source: &ColumnSource) -> usize {
    match source {
        ColumnSource::Skip => 0,
        ColumnSource::Constant(_) => 1,
        ColumnSource::Null => 2,
        ColumnSource::Default => 3,
        ColumnSource::Sequence { .. } => 4,
        ColumnSource::Generated(_) => 5,
    }
}

#[derive(Clone, Debug)]
pub enum BatchInsertEvent {
    Staged {
        session: Box<RowEditSession>,
        rows: Vec<Vec<(String, CellValue)>>,
    },
    Dismissed,
}

struct ColumnUi {
    source_select: Entity<SelectState<Vec<String>>>,
    value_input: Entity<InputState>,
    step_input: Entity<InputState>,
}

pub struct BatchInsertDialog {
    model: BatchInsertModel,
    count_input: Entity<InputState>,
    seed_input: Entity<InputState>,
    columns: Vec<ColumnUi>,
    preview: String,
    error: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BatchInsertEvent> for BatchInsertDialog {}

impl BatchInsertDialog {
    pub fn new(model: BatchInsertModel, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let count_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("1–1000");
            state.set_value(model.count.to_string(), window, cx);
            state
        });
        let seed_input = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "results.batch_insert.seed_placeholder",
                ));
            state.set_value(model.seed.to_string(), window, cx);
            state
        });
        let labels: Vec<String> = source_kind_labels().into_iter().collect();
        let mut columns = Vec::with_capacity(model.sources.len());
        for source in model.sources.iter() {
            let idx = source_kind_index(source);
            let source_select = cx.new(|cx| {
                SelectState::new(
                    labels.clone(),
                    Some(gpui_component::IndexPath::new(idx)),
                    window,
                    cx,
                )
            });
            let (value_text, step_text) = match source {
                ColumnSource::Constant(text) => (text.clone(), "1".to_string()),
                ColumnSource::Sequence { start, step } => (start.to_string(), step.to_string()),
                _ => (String::new(), "1".to_string()),
            };
            let value_input = cx.new(|cx| {
                let mut state = InputState::new(window, cx);
                state.set_value(value_text, window, cx);
                state
            });
            let step_input = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(
                    crate::localization::input_placeholder(
                        window,
                        cx,
                        "results.batch_insert.step_placeholder",
                    ),
                );
                state.set_value(step_text, window, cx);
                state
            });
            columns.push(ColumnUi {
                source_select,
                value_input,
                step_input,
            });
        }
        let mut this = Self {
            model,
            count_input,
            seed_input,
            columns,
            preview: String::new(),
            error: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(640.0)),
            _subscriptions: Vec::new(),
        };
        this.refresh_preview();
        this.wire_subscriptions(window, cx);
        this
    }

    fn wire_subscriptions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._subscriptions.push(cx.subscribe_in(
            &self.count_input,
            window,
            |this, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.model.count = state.read(cx).value().parse().unwrap_or(0);
                    this.refresh_preview();
                    cx.notify();
                }
            },
        ));
        self._subscriptions.push(cx.subscribe_in(
            &self.seed_input,
            window,
            |this, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.model.seed = state.read(cx).value().parse().unwrap_or(0);
                    this.refresh_preview();
                    cx.notify();
                }
            },
        ));
        for i in 0..self.columns.len() {
            let source_select = self.columns[i].source_select.clone();
            self._subscriptions.push(cx.subscribe_in(
                &source_select,
                window,
                move |this, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_none() {
                        return;
                    }
                    let Some(kind) = select.read(cx).selected_index(cx).map(|ix| ix.row) else {
                        return;
                    };
                    this.set_source_kind(i, kind);
                    this.refresh_preview();
                    cx.notify();
                },
            ));
            let value_input = self.columns[i].value_input.clone();
            self._subscriptions.push(cx.subscribe_in(
                &value_input,
                window,
                move |this, state, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = state.read(cx).value().to_string();
                        this.set_value_text(i, text);
                        this.refresh_preview();
                        cx.notify();
                    }
                },
            ));
            let step_input = self.columns[i].step_input.clone();
            self._subscriptions.push(cx.subscribe_in(
                &step_input,
                window,
                move |this, state, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = state.read(cx).value().to_string();
                        this.set_step_text(i, text);
                        this.refresh_preview();
                        cx.notify();
                    }
                },
            ));
        }
    }

    /// `kind` is the confirmed row of the source select, in
    /// [`source_kind_labels`] order (never matched by label text, which goes
    /// stale across a language switch).
    fn set_source_kind(&mut self, index: usize, kind: usize) {
        let Some(col) = self.model.session.columns.get(index) else {
            return;
        };
        let previous = self.model.sources.get(index).cloned();
        let next = match kind {
            0 => ColumnSource::Skip,
            1 => match previous {
                Some(ColumnSource::Constant(text)) => ColumnSource::Constant(text),
                _ => ColumnSource::Constant(String::new()),
            },
            2 => ColumnSource::Null,
            3 => ColumnSource::Default,
            4 => match previous {
                Some(ColumnSource::Sequence { start, step }) => {
                    ColumnSource::Sequence { start, step }
                }
                _ => ColumnSource::Sequence { start: 1, step: 1 },
            },
            5 => match previous {
                Some(ColumnSource::Generated(g)) => ColumnSource::Generated(g),
                _ => ColumnSource::Generated(infer_generator(&col.name, &col.data_type)),
            },
            _ => return,
        };
        if let Some(slot) = self.model.sources.get_mut(index) {
            *slot = next;
        }
    }

    fn set_value_text(&mut self, index: usize, text: String) {
        let Some(source) = self.model.sources.get_mut(index) else {
            return;
        };
        match source {
            ColumnSource::Constant(existing) => *existing = text,
            ColumnSource::Sequence { start, .. } => {
                *start = text.parse().unwrap_or(*start);
            }
            _ => {}
        }
    }

    fn set_step_text(&mut self, index: usize, text: String) {
        if let Some(ColumnSource::Sequence { step, .. }) = self.model.sources.get_mut(index) {
            *step = text.parse().unwrap_or(*step);
        }
    }

    fn refresh_preview(&mut self) {
        match self.model.validate() {
            Ok(()) => {
                self.error = None;
                match self.model.preview_sql() {
                    Ok(sql) => self.preview = truncate_preview(&sql),
                    Err(err) => {
                        self.error = Some(err);
                        self.preview.clear();
                    }
                }
            }
            Err(err) => {
                self.error = Some(err);
                self.preview.clear();
            }
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(BatchInsertEvent::Dismissed);
    }

    fn stage(&mut self, cx: &mut Context<Self>) {
        match self.model.rows() {
            Ok(rows) => {
                cx.emit(BatchInsertEvent::Staged {
                    session: Box::new(self.model.session.clone()),
                    rows,
                });
            }
            Err(err) => {
                self.error = Some(err);
                cx.notify();
            }
        }
    }
}

fn truncate_preview(sql: &str) -> String {
    let lines: Vec<&str> = sql.lines().collect();
    if lines.len() <= 20 {
        return sql.to_string();
    }
    let rest = lines.len() - 20;
    crate::localization::format(
        "results.batch_insert.preview_truncation",
        &[
            ("preview", &lines[..20].join("\n")),
            ("hidden_line_count", &rest.to_string()),
        ],
    )
}

impl Focusable for BatchInsertDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BatchInsertDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let invalid = self.error.is_some();
        let stage_label = crate::localization::format(
            "results.batch_insert.stage_button",
            &[("row_count", &self.model.count.to_string())],
        );
        let title = crate::localization::format(
            "results.batch_insert.dialog_title",
            &[("table_name", &self.model.session.target.quoted())],
        );

        let mut column_rows = div()
            .id("batch-insert-columns")
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .max_h(px(s(220.0)))
            .overflow_y_scroll();
        for (i, col) in self.model.session.columns.iter().enumerate() {
            let Some(ui) = self.columns.get(i) else {
                continue;
            };
            let extra = extra_controls(&self.model.sources[i], ui, s);
            column_rows = column_rows.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .w(px(s(120.0)))
                            .flex_shrink_0()
                            .text_color(Theme::text())
                            .child(col.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(s(110.0)))
                            .flex_shrink_0()
                            .text_size(px(Theme::font_size_ratio(0.80)))
                            .text_color(Theme::text_dim())
                            .child(column_type_summary(col)),
                    )
                    .child(div().w(px(s(140.0))).child(Select::new(&ui.source_select)))
                    .child(div().flex_1().child(extra)),
            );
        }

        let focus_handle = self.focus_handle.clone();
        Dialog::new("batch-insert-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .key_context("BatchInsertDialog")
            .track_focus(&focus_handle)
            .dismiss_on_scrim_click(false)
            .on_close(|this, _window, cx| this.close(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(12.0)))
                    .child(labeled_field(
                        crate::localization::text("results.batch_insert.count_label"),
                        Input::new(&self.count_input),
                        s,
                    ))
                    .child(labeled_field(
                        crate::localization::text("results.batch_insert.seed_label"),
                        Input::new(&self.seed_input),
                        s,
                    )),
            )
            .child(column_rows)
            .child(
                div()
                    .id("batch-insert-preview")
                    .max_h(px(s(140.0)))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .p(px(s(8.0)))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(if self.preview.is_empty() {
                        String::new()
                    } else {
                        self.preview.clone()
                    }),
            )
            .children_body(self.error.clone().map(|err| {
                div()
                    .text_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .child(err)
            }))
            .button(
                Button::new("batch-insert-stage")
                    .primary()
                    .label(stage_label)
                    .disabled(invalid)
                    .on_click(cx.listener(|this, _, _window, cx| this.stage(cx))),
            )
            .build(self, window, cx)
    }
}

fn labeled_field(label: String, control: impl IntoElement, s: fn(f32) -> f32) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(8.0)))
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text_dim())
                .child(label),
        )
        .child(div().w(px(s(80.0))).child(control))
}

fn extra_controls(source: &ColumnSource, ui: &ColumnUi, s: fn(f32) -> f32) -> gpui::AnyElement {
    match source {
        ColumnSource::Constant(_) => Input::new(&ui.value_input).into_any_element(),
        ColumnSource::Sequence { .. } => div()
            .flex()
            .flex_row()
            .gap(px(s(6.0)))
            .child(div().w(px(s(72.0))).child(Input::new(&ui.value_input)))
            .child(div().w(px(s(72.0))).child(Input::new(&ui.step_input)))
            .into_any_element(),
        ColumnSource::Generated(g) => div()
            .text_size(px(Theme::font_size_ratio(0.80)))
            .text_color(Theme::text_dim())
            .child(generator_label(*g))
            .into_any_element(),
        _ => div().into_any_element(),
    }
}

fn column_type_summary(col: &RowEditColumn) -> String {
    let nullability = if col.nullable {
        crate::localization::text("results.batch_insert.nullable_summary")
    } else {
        crate::localization::text("results.batch_insert.not_null_summary")
    };
    crate::localization::format(
        "results.batch_insert.column_type_summary",
        &[("data_type", &col.data_type), ("nullability", &nullability)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::row_editing::TableTarget;

    fn col(name: &str, data_type: &str) -> RowEditColumn {
        RowEditColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            char_max_length: None,
            precision: None,
            scale: None,
            nullable: true,
            is_primary_key: false,
            is_identity: false,
            is_computed: false,
            is_rowversion: false,
            default_definition: None,
            result_ordinal: None,

            enum_values: Vec::new(),
        }
    }

    fn sample_session(columns: Vec<RowEditColumn>, row: Vec<CellValue>) -> RowEditSession {
        RowEditSession {
            target: TableTarget {
                schema: "dbo".into(),
                table: "Items".into(),
            },
            key_columns: vec![columns.first().map(|c| c.name.clone()).unwrap_or_default()],
            rowversion_column: None,
            columns,
            original_row: row,
            foreign_keys: Vec::new(),
            fk_display: Vec::new(),
            check_constraints: Vec::new(),
            incoming_foreign_keys: Vec::new(),
        }
    }

    #[test]
    fn validate_rejects_count_outside_range() {
        let mut id = col("id", "int");
        id.is_identity = true;
        id.is_primary_key = true;
        id.nullable = false;
        let session = sample_session(
            vec![id, col("name", "nvarchar")],
            vec![CellValue::I64(1), CellValue::Null],
        );
        let mut model = BatchInsertModel::from_template(session, None, DatabaseType::SqlServer);
        model.count = 0;
        assert!(model.validate().is_err());
        model.count = 1001;
        assert!(model.validate().is_err());
        model.count = 1;
        assert!(model.validate().is_ok());
    }

    #[test]
    fn from_template_defaults_pick_sequence_for_integer_pk_and_skip_for_identity() {
        let mut id = col("id", "int");
        id.is_identity = true;
        id.is_primary_key = true;
        id.nullable = false;
        let mut code = col("code", "int");
        code.is_primary_key = true;
        code.nullable = false;
        let name = col("name", "nvarchar");
        let session = sample_session(
            vec![id, code, name],
            vec![CellValue::I64(1), CellValue::I64(10), CellValue::Null],
        );
        let template = vec![
            CellValue::I64(1),
            CellValue::I64(10),
            CellValue::String("Ada".into()),
        ];
        let model =
            BatchInsertModel::from_template(session, Some(template), DatabaseType::SqlServer);
        assert_eq!(model.sources[0], ColumnSource::Skip);
        assert!(
            matches!(model.sources[1], ColumnSource::Sequence { step: 1, .. }),
            "integer PK should be Sequence, got {:?}",
            model.sources[1]
        );
        assert_eq!(model.sources[2], ColumnSource::Constant("Ada".into()));
    }

    #[test]
    fn rows_apply_sequence_and_constant_and_null() {
        let mut n = col("n", "int");
        n.nullable = false;
        let name = col("name", "nvarchar");
        let note = col("note", "nvarchar");
        let session = sample_session(
            vec![n, name, note],
            vec![CellValue::I64(0), CellValue::Null, CellValue::Null],
        );
        let mut model = BatchInsertModel::from_template(session, None, DatabaseType::SqlServer);
        model.count = 3;
        model.sources = vec![
            ColumnSource::Sequence { start: 1, step: 1 },
            ColumnSource::Constant("x".into()),
            ColumnSource::Null,
        ];
        let rows = model.rows().expect("valid");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            vec![
                ("n".into(), CellValue::I64(1)),
                ("name".into(), CellValue::String("x".into())),
                ("note".into(), CellValue::Null),
            ]
        );
        assert_eq!(rows[2][0], ("n".into(), CellValue::I64(3)));
    }

    #[test]
    fn preview_sql_lists_count_inserts() {
        let mut id = col("id", "int");
        id.is_identity = true;
        id.is_primary_key = true;
        id.nullable = false;
        let mut name = col("name", "nvarchar");
        name.nullable = false;
        let session = sample_session(vec![id, name], vec![CellValue::I64(1), CellValue::Null]);
        let mut model = BatchInsertModel::from_template(session, None, DatabaseType::SqlServer);
        model.count = 3;
        model.sources[1] = ColumnSource::Constant("x".into());
        let sql = model.preview_sql().expect("preview");
        assert_eq!(
            sql.matches("INSERT").count(),
            3,
            "expected 3 INSERT statements, got {sql}"
        );
    }

    #[test]
    fn same_seed_generates_identical_rows() {
        let mut name = col("name", "nvarchar");
        name.nullable = false;
        let session = sample_session(vec![name.clone()], vec![CellValue::Null]);
        let mut a = BatchInsertModel::from_template(session.clone(), None, DatabaseType::SqlServer);
        let mut b = BatchInsertModel::from_template(session, None, DatabaseType::SqlServer);
        a.count = 4;
        b.count = 4;
        a.seed = 42;
        b.seed = 42;
        a.sources = vec![ColumnSource::Generated(ValueGenerator::Word)];
        b.sources = vec![ColumnSource::Generated(ValueGenerator::Word)];
        assert_eq!(a.rows().unwrap(), b.rows().unwrap());
    }
}
