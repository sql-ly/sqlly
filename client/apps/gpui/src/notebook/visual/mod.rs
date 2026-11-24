//! Visual notebook cells: chart, table, pivot, KPI, scalar and transform.
//!
//! Each visual cell is one [`VisualCellView`] entity. The notebook view
//! builds the cell's [`VisualInputs`] (its spec, the upstream output from
//! the [`super::outputs::OutputRegistry`], the upstream state, the active
//! cross-filter, lineage) while drawing it and pushes them only when they
//! change; the entity draws the shared toolbar (input and follow pickers,
//! View SQL, Lineage, Provenance, Open in Editor), the completeness and
//! staleness notices, and the kind's own editor and body, and reports edits
//! back as [`VisualEvent`]s. Specs are the typed shapes in
//! `sqlly_services::notebook::cells`.

pub mod chart;
pub mod chart_map;
mod host;
pub mod image;
pub mod kpi;
pub mod pivot;
pub mod scalar;
pub mod table;
pub mod transform;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use gpui::AppContext as _;
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use serde_json::Value;
use sqlly_core::{DisplayMessage, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::analytics::scalar::ScalarValue;
use sqlly_services::notebook::cells::spec::CrossFilter;
use sqlly_services::notebook::{CellOutput, CellStatus, Completeness};

use crate::theme::Theme;

pub(crate) fn t(key: &str) -> String {
    crate::localization::text(key)
}

/// The visual cell kinds this module draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VisualKind {
    Chart,
    Table,
    Pivot,
    Kpi,
    Scalar,
    Transform,
}

impl VisualKind {
    pub fn from_kind(kind: &str) -> Option<Self> {
        Some(match kind {
            "chart" => VisualKind::Chart,
            "table" => VisualKind::Table,
            "pivot" => VisualKind::Pivot,
            "kpi" => VisualKind::Kpi,
            "scalar" => VisualKind::Scalar,
            "transform" => VisualKind::Transform,
            _ => return None,
        })
    }

    /// Whether the cell reads one main input (`spec.input`).
    pub fn has_input(self) -> bool {
        self != VisualKind::Scalar
    }

    /// Whether the cell can follow a chart's cross-filter.
    pub fn can_follow(self) -> bool {
        matches!(
            self,
            VisualKind::Chart | VisualKind::Table | VisualKind::Pivot | VisualKind::Kpi
        )
    }

    /// Whether computing the cell is instant (everything but transforms,
    /// which run in the local workspace).
    pub fn is_instant(self) -> bool {
        self != VisualKind::Transform
    }
}

/// Shared result rows.
pub type Rows = Arc<Vec<Vec<sqlly_core::CellValue>>>;

/// Another cell a visual cell reads.
#[derive(Clone, Debug)]
pub struct InputInfo {
    pub id: String,
    pub name: String,
    pub output: Option<Arc<CellOutput>>,
    pub version: u64,
    pub status: CellStatus,
    /// The core says the output may be shown as current.
    pub current: bool,
    /// The SQL the cell runs (SQL cells), as the run builds it when it can.
    pub sql: Option<String>,
}

/// One cell in a visual cell's upstream, nearest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub depth: usize,
}

/// Where one input column comes from, when the SQL says so plainly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnLineage {
    pub column: String,
    /// `schema.table.column`, or `None` when it is computed or unclear.
    pub origin: Option<String>,
}

/// Everything a visual cell draws from, rebuilt by the notebook view.
#[derive(Clone, Default)]
pub struct VisualInputs {
    pub cell_id: String,
    pub name: String,
    pub spec: Value,
    pub source: String,
    /// The notebook's engine (transform provenance; native only).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub db: DatabaseType,
    pub input: Option<InputInfo>,
    /// Further inputs (KPI baseline, transform joins, scalar references).
    pub others: Vec<InputInfo>,
    /// Cells above this one that produce a result, `(id, name)`.
    pub candidates: Vec<(String, String)>,
    /// Chart cells this cell could follow, `(id, name)`.
    pub charts: Vec<(String, String)>,
    /// The selection of the chart this cell follows.
    pub filter: Option<CrossFilter>,
    /// The selection made in this chart.
    pub own_filter: Option<CrossFilter>,
    /// Outputs scalar formulas can name, by cell name.
    pub named_outputs: Vec<(String, Arc<CellOutput>)>,
    /// `param.x` values and other scalar cells' values.
    pub values: BTreeMap<String, ScalarValue>,
    pub lineage: Vec<LineageEntry>,
    pub columns: Vec<ColumnLineage>,
    pub own_output: Option<Arc<CellOutput>>,
    pub own_status: CellStatus,
    pub message: Option<String>,
    pub key: u64,
}

impl VisualInputs {
    /// The input's rows after this cell's followed cross-filter.
    pub fn input_rows(&self) -> Option<(Arc<CellOutput>, Rows)> {
        let output = self.input.as_ref()?.output.clone()?;
        let rows = match &self.filter {
            Some(filter) => Arc::new(filter.apply(&output.columns, &output.rows)),
            None => output.rows.clone(),
        };
        Some((output, rows))
    }

    pub fn input_columns(&self) -> Vec<ResultColumn> {
        self.input
            .as_ref()
            .and_then(|i| i.output.as_ref())
            .map(|o| o.columns.clone())
            .unwrap_or_default()
    }

    /// A hash of everything drawn from, so unchanged inputs are not pushed.
    pub fn compute_key(&mut self) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.spec.to_string().hash(&mut h);
        self.source.hash(&mut h);
        self.name.hash(&mut h);
        let info = |i: &InputInfo, h: &mut std::collections::hash_map::DefaultHasher| {
            i.id.hash(h);
            i.name.hash(h);
            i.version.hash(h);
            format!("{:?}", i.status).hash(h);
            i.current.hash(h);
            i.sql.hash(h);
        };
        if let Some(i) = &self.input {
            info(i, &mut h);
        }
        for i in &self.others {
            info(i, &mut h);
        }
        self.candidates.hash(&mut h);
        self.charts.hash(&mut h);
        format!("{:?}{:?}", self.filter, self.own_filter).hash(&mut h);
        for (name, o) in &self.named_outputs {
            name.hash(&mut h);
            o.run_id.hash(&mut h);
            o.row_count.hash(&mut h);
        }
        format!("{:?}", self.values).hash(&mut h);
        format!("{:?}", self.lineage).hash(&mut h);
        format!("{:?}", self.columns).hash(&mut h);
        self.own_output
            .as_ref()
            .map(|o| (o.run_id.clone(), o.row_count))
            .hash(&mut h);
        format!("{:?}", self.own_status).hash(&mut h);
        self.message.hash(&mut h);
        self.key = h.finish();
    }
}

/// What a visual cell asks of the notebook.
#[derive(Clone, Debug)]
pub enum VisualEvent {
    SpecChanged(Value),
    /// The cell's text (a scalar formula) changed, with its new spec.
    SourceChanged {
        source: String,
        spec: Value,
    },
    /// A chart selection was made or cleared (`None` clears `source`'s).
    Filter {
        source: String,
        filter: Option<CrossFilter>,
    },
    OpenInEditor(String),
    /// The row selected in a table cell's grid changed: an index into the
    /// rows the table shows, `None` when nothing is selected.
    RowSelected(Option<usize>),
    /// Run this cell (and its out-of-date inputs, per the run mode).
    Run,
    Remeasure,
    Status {
        message: String,
        error: bool,
    },
}

/// The visual cells of one notebook, by cell id.
#[derive(Default)]
pub struct VisualCells {
    pub(super) slots: HashMap<String, VisualSlot>,
}

pub(super) struct VisualSlot {
    pub(super) view: Entity<VisualCellView>,
    pub(super) kind: VisualKind,
    pub(super) key: u64,
    _subscription: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoPanel {
    Sql,
    Lineage,
    Provenance,
}

/// One visual cell's view.
pub struct VisualCellView {
    pub(crate) kind: VisualKind,
    pub(crate) inputs: VisualInputs,
    pub(crate) editing: bool,
    pub(crate) panel: Option<InfoPanel>,
    pub(crate) chart: chart::ChartPart,
    pub(crate) table: table::TablePart,
    pub(crate) pivot: pivot::PivotPart,
    pub(crate) kpi: kpi::KpiPart,
    pub(crate) transform: transform::TransformPart,
    /// Text fields of the editors, by field key.
    pub(crate) fields: HashMap<String, (Entity<InputState>, Subscription)>,
    /// Only the output: no toolbar, info panels or editor (dashboard tiles).
    pub(crate) output_only: bool,
    /// How the output-only view draws (an embedded cell's settings).
    pub(crate) display: OutputDisplay,
}

/// How an output-only visual cell is drawn: with its configuration (a
/// pivot's sidebar, a chart's editor) or without, at the height the user
/// chose (else fitted to the content), and the view a table's grid is
/// locked to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OutputDisplay {
    pub config_shown: bool,
    pub height: Option<f32>,
    pub lock: Option<sqlly_services::notebook::display::LockedView>,
}

impl EventEmitter<VisualEvent> for VisualCellView {}

impl VisualCellView {
    pub fn new(kind: VisualKind, inputs: VisualInputs) -> Self {
        Self {
            kind,
            inputs,
            editing: false,
            panel: None,
            chart: chart::ChartPart::default(),
            table: table::TablePart::default(),
            pivot: pivot::PivotPart::default(),
            kpi: kpi::KpiPart::default(),
            transform: transform::TransformPart::default(),
            fields: HashMap::new(),
            output_only: false,
            display: OutputDisplay::default(),
        }
    }

    pub fn set_inputs(&mut self, inputs: VisualInputs, cx: &mut Context<Self>) {
        self.inputs = inputs;
        cx.notify();
    }

    /// Replace the spec locally and tell the notebook.
    pub(crate) fn emit_spec(&mut self, spec: Value, cx: &mut Context<Self>) {
        if spec == self.inputs.spec {
            return;
        }
        self.inputs.spec = spec.clone();
        cx.emit(VisualEvent::SpecChanged(spec));
        cx.notify();
    }

    /// Set (or remove) one top-level spec field.
    pub(crate) fn set_spec_field(
        &mut self,
        key: &str,
        value: Option<Value>,
        cx: &mut Context<Self>,
    ) {
        let mut spec = match &self.inputs.spec {
            Value::Object(_) => self.inputs.spec.clone(),
            _ => Value::Object(Default::default()),
        };
        if let Some(map) = spec.as_object_mut() {
            match value {
                Some(v) => {
                    map.insert(key.to_string(), v);
                }
                None => {
                    map.remove(key);
                }
            }
        }
        self.emit_spec(spec, cx);
    }

    /// A text field for `key`, made once with `initial`; `apply` runs on
    /// every edit with the field's text.
    pub(crate) fn field(
        &mut self,
        key: &str,
        initial: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self, &str, &mut Context<Self>) + 'static,
    ) -> Entity<InputState> {
        if let Some((state, _)) = self.fields.get(key) {
            return state.clone();
        }
        let state = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                placeholder,
            ))
        });
        if !initial.is_empty() {
            let initial = initial.to_string();
            state.update(cx, |s, cx| s.set_value(initial, window, cx));
        }
        let subscription = cx.subscribe(&state, move |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = input.read(cx).value().to_string();
                apply(this, &text, cx);
            }
        });
        self.fields
            .insert(key.to_string(), (state.clone(), subscription));
        state
    }

    /// Forget the text fields whose key starts with `prefix` (their values
    /// came from a spec that changed shape).
    pub(crate) fn drop_fields(&mut self, prefix: &str) {
        self.fields.retain(|key, _| !key.starts_with(prefix));
    }

    pub(crate) fn toggle_editing(&mut self, cx: &mut Context<Self>) {
        self.editing = !self.editing;
        cx.emit(VisualEvent::Remeasure);
        cx.notify();
    }

    fn toggle_panel(&mut self, panel: InfoPanel, cx: &mut Context<Self>) {
        self.panel = if self.panel == Some(panel) {
            None
        } else {
            Some(panel)
        };
        cx.emit(VisualEvent::Remeasure);
        cx.notify();
    }

    /// The SQL behind this cell: the input's (and the other inputs'), plus
    /// a transform's generated query.
    pub(crate) fn upstream_sql(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for info in self.inputs.input.iter().chain(&self.inputs.others) {
            if let Some(sql) = &info.sql {
                out.push((info.name.clone(), sql.clone()));
            }
        }
        if self.kind == VisualKind::Transform
            && let Some(sql) = self.transform_sql()
        {
            out.push((self.inputs.name.clone(), sql));
        }
        out
    }

    /// What Open in Editor opens: a transform's own query, else the input's SQL.
    fn editor_sql(&self) -> Option<String> {
        if self.kind == VisualKind::Transform {
            return self.transform_sql();
        }
        self.upstream_sql().into_iter().next().map(|(_, sql)| sql)
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let this = cx.entity();
        let id = self.inputs.cell_id.clone();
        let mut bar = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(4.0)))
            .px(px(s(8.0)))
            .py(px(s(3.0)))
            .border_b_1()
            .border_color(Theme::border());

        if self.kind.has_input() {
            let current = self.inputs.input.as_ref().map(|i| i.id.clone());
            let label = match &self.inputs.input {
                Some(i) => crate::localization::format(
                    "notebook.visual.toolbar.input_label",
                    &[("cell_name", i.name.as_str())],
                ),
                None => t("notebook.visual.toolbar.input_none"),
            };
            let candidates = self.inputs.candidates.clone();
            let pick = this.clone();
            bar = bar.child(
                small_button(format!("nb-visual-input-{id}"), label)
                    .tooltip(t("notebook.visual.toolbar.input_tooltip"))
                    .dropdown_menu(move |mut menu, _, _| {
                        if candidates.is_empty() {
                            menu = menu.item(
                                PopupMenuItem::new(t("notebook.visual.toolbar.no_candidates"))
                                    .disabled(true),
                            );
                        }
                        for (cell, name) in candidates.clone() {
                            let pick = pick.clone();
                            let checked = current.as_deref() == Some(cell.as_str());
                            menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                                move |_, _, app| {
                                    let cell = cell.clone();
                                    pick.update(app, |v, cx| {
                                        v.set_spec_field("input", Some(Value::String(cell)), cx)
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            );
        }

        if self.kind.can_follow() {
            let following = self
                .inputs
                .spec
                .get("follows")
                .and_then(Value::as_str)
                .map(str::to_string);
            let label = match following
                .as_deref()
                .and_then(|f| self.inputs.charts.iter().find(|(c, _)| c == f))
            {
                Some((_, name)) => crate::localization::format(
                    "notebook.visual.toolbar.follows_label",
                    &[("cell_name", name.as_str())],
                ),
                None => t("notebook.visual.toolbar.follows_none"),
            };
            let charts: Vec<(String, String)> = self
                .inputs
                .charts
                .iter()
                .filter(|(c, _)| *c != id)
                .cloned()
                .collect();
            let pick = this.clone();
            bar = bar.child(
                small_button(format!("nb-visual-follows-{id}"), label)
                    .tooltip(t("notebook.visual.toolbar.follows_tooltip"))
                    .dropdown_menu(move |mut menu, _, _| {
                        let clear = pick.clone();
                        menu = menu.item(
                            PopupMenuItem::new(t("notebook.visual.toolbar.follows_off"))
                                .checked(following.is_none())
                                .on_click(move |_, _, app| {
                                    clear
                                        .update(app, |v, cx| v.set_spec_field("follows", None, cx));
                                }),
                        );
                        if !charts.is_empty() {
                            menu = menu.separator();
                        }
                        for (cell, name) in charts.clone() {
                            let pick = pick.clone();
                            let checked = following.as_deref() == Some(cell.as_str());
                            menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                                move |_, _, app| {
                                    let cell = cell.clone();
                                    pick.update(app, |v, cx| {
                                        v.set_spec_field("follows", Some(Value::String(cell)), cx)
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            );
        }

        bar = bar.child(div().flex_1());
        bar = bar.children(self.render_kind_actions(cx));
        let editing = self.editing;
        bar = bar.child(
            small_button(
                format!("nb-visual-edit-{id}"),
                t(if editing {
                    "notebook.visual.toolbar.done_editing"
                } else {
                    "notebook.visual.toolbar.edit"
                }),
            )
            .when(editing, |b| b.primary())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_editing(cx))),
        );
        for (panel, key) in [
            (InfoPanel::Sql, "notebook.visual.toolbar.view_sql"),
            (InfoPanel::Lineage, "notebook.visual.toolbar.lineage"),
            (InfoPanel::Provenance, "notebook.visual.toolbar.provenance"),
        ] {
            let open = self.panel == Some(panel);
            bar = bar.child(
                small_button(format!("nb-visual-{key}-{id}"), t(key))
                    .when(open, |b| b.primary())
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_panel(panel, cx))),
            );
        }
        let sql = self.editor_sql();
        bar = bar.child(
            small_button(
                format!("nb-visual-open-{id}"),
                t("notebook.visual.toolbar.open_in_editor"),
            )
            .tooltip(t("notebook.visual.toolbar.open_in_editor_tooltip"))
            .disabled(sql.is_none())
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(sql) = sql.clone() {
                    cx.emit(VisualEvent::OpenInEditor(sql));
                }
            })),
        );
        bar.into_any_element()
    }

    fn render_kind_actions(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match self.kind {
            VisualKind::Chart => self.chart_actions(cx),
            _ => Vec::new(),
        }
    }

    /// The input's state, completeness and cross-filter, above the body.
    fn render_notices(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let s = Theme::scaled;
        let mut out = Vec::new();
        let note = |text: String, color: gpui::Hsla| {
            div()
                .px(px(s(10.0)))
                .py(px(s(2.0)))
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(color)
                .child(text)
        };
        if let Some(message) = &self.inputs.message {
            out.push(note(message.clone(), Theme::error()).into_any_element());
        }
        if let Some(input) = &self.inputs.input {
            match (&input.output, input.current) {
                (None, _) => {
                    let name = input.name.clone();
                    out.push(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(note(
                                crate::localization::format(
                                    "notebook.visual.notice.input_not_run",
                                    &[("cell_name", name.as_str())],
                                ),
                                Theme::text_dim(),
                            ))
                            .child(
                                small_button(
                                    format!("nb-visual-run-{}", self.inputs.cell_id),
                                    t("notebook.visual.notice.run_input"),
                                )
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(VisualEvent::Run))),
                            )
                            .into_any_element(),
                    );
                }
                (Some(_), false) => {
                    let key = match input.status {
                        CellStatus::Failed | CellStatus::Blocked | CellStatus::Canceled => {
                            "notebook.visual.notice.input_failed"
                        }
                        CellStatus::Running | CellStatus::Streaming | CellStatus::Queued => {
                            "notebook.visual.notice.input_running"
                        }
                        _ => "notebook.visual.notice.input_stale",
                    };
                    out.push(
                        note(
                            crate::localization::format(key, &[("cell_name", input.name.as_str())]),
                            Theme::warning(),
                        )
                        .into_any_element(),
                    );
                }
                (Some(_), true) => {}
            }
            if let Some(output) = &input.output
                && let Some(badge) = completeness_text(output)
            {
                let mut row = div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(10.0)))
                    .py(px(s(2.0)))
                    .child(badge_chip(badge, Theme::warning()));
                if !output.completeness.supports_summaries() {
                    row = row.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(t("notebook.visual.notice.partial_summary")),
                    );
                }
                out.push(row.into_any_element());
            }
        }
        if let Some(filter) = self.inputs.filter.clone() {
            let chart = self
                .inputs
                .charts
                .iter()
                .find(|(c, _)| *c == filter.source)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| filter.source.clone());
            let source = filter.source.clone();
            out.push(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(10.0)))
                    .py(px(s(2.0)))
                    .child(badge_chip(
                        crate::localization::format(
                            "notebook.visual.filter.chip",
                            &[
                                ("chart_name", chart.as_str()),
                                ("column_name", filter.column.as_str()),
                                ("value", filter.value.as_str()),
                            ],
                        ),
                        Theme::accent(),
                    ))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(t("notebook.visual.filter.local_note")),
                    )
                    .child(
                        small_button(
                            format!("nb-visual-clear-filter-{}", self.inputs.cell_id),
                            t("notebook.visual.filter.clear"),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(VisualEvent::Filter {
                                source: source.clone(),
                                filter: None,
                            })
                        })),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn render_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.panel?;
        let s = Theme::scaled;
        let mut body = div()
            .id(SharedString::from(format!(
                "nb-visual-panel-{}",
                self.inputs.cell_id
            )))
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .mx(px(s(8.0)))
            .my(px(s(4.0)))
            .p(px(s(8.0)))
            .max_h(px(s(320.0)))
            .overflow_y_scroll()
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::border())
            .bg(Theme::popover_bg());
        match panel {
            InfoPanel::Sql => {
                let sql = self.upstream_sql();
                if sql.is_empty() {
                    body = body.child(dim(t("notebook.visual.sql.none")));
                }
                for (i, (name, text)) in sql.into_iter().enumerate() {
                    let copy = text.clone();
                    body = body
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(6.0)))
                                .child(heading(name))
                                .child(
                                    small_button(
                                        format!("nb-visual-copy-sql-{}-{i}", self.inputs.cell_id),
                                        t("notebook.visual.sql.copy"),
                                    )
                                    .on_click(cx.listener(
                                        move |_, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        },
                                    )),
                                ),
                        )
                        .child(code_block(text));
                }
                body = body.child(dim(t("notebook.visual.sql.read_only_note")));
            }
            InfoPanel::Lineage => {
                if self.inputs.lineage.is_empty() {
                    body = body.child(dim(t("notebook.visual.lineage.none")));
                }
                body = body.child(heading(t("notebook.visual.lineage.upstream")));
                for entry in &self.inputs.lineage {
                    body = body.child(
                        div()
                            .pl(px(s(10.0) * entry.depth as f32))
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text())
                            .child(crate::localization::format(
                                "notebook.visual.lineage.entry",
                                &[
                                    ("cell_name", entry.name.as_str()),
                                    ("cell_kind", entry.kind.as_str()),
                                ],
                            )),
                    );
                }
                if !self.inputs.columns.is_empty() {
                    body = body.child(heading(t("notebook.visual.lineage.columns")));
                    for column in &self.inputs.columns {
                        let origin = column
                            .origin
                            .clone()
                            .unwrap_or_else(|| t("notebook.visual.lineage.computed"));
                        body = body.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text())
                                .child(crate::localization::format(
                                    "notebook.visual.lineage.column",
                                    &[
                                        ("column_name", column.column.as_str()),
                                        ("origin", origin.as_str()),
                                    ],
                                )),
                        );
                    }
                }
            }
            InfoPanel::Provenance => {
                let infos: Vec<InputInfo> = self
                    .inputs
                    .input
                    .iter()
                    .chain(&self.inputs.others)
                    .cloned()
                    .collect();
                if infos.iter().all(|i| i.output.is_none()) {
                    body = body.child(dim(t("notebook.visual.provenance.none")));
                }
                for info in infos {
                    let Some(output) = info.output.as_ref() else {
                        continue;
                    };
                    body = body.child(heading(info.name.clone()));
                    for (key, value) in provenance_rows(output) {
                        body = body.child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(s(8.0)))
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .child(
                                    div()
                                        .w(px(s(130.0)))
                                        .flex_none()
                                        .text_color(Theme::text_dim())
                                        .child(t(key)),
                                )
                                .child(div().text_color(Theme::text()).child(value)),
                        );
                    }
                }
            }
        }
        Some(body.into_any_element())
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.kind {
            VisualKind::Chart => self.render_chart(window, cx),
            VisualKind::Table => self.render_table(window, cx),
            VisualKind::Pivot => self.render_pivot(window, cx),
            VisualKind::Kpi => self.render_kpi(window, cx),
            VisualKind::Scalar => self.render_scalar(window, cx),
            VisualKind::Transform => self.render_transform(window, cx),
        }
    }
}

impl Render for VisualCellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.output_only {
            let notices = self.render_notices(cx);
            let config = self.display.config_shown && self.kind == VisualKind::Chart;
            let editing = std::mem::replace(&mut self.editing, config);
            let body = self.render_body(window, cx);
            self.editing = editing;
            return div()
                .flex()
                .flex_col()
                .w_full()
                .children(notices)
                .child(body);
        }
        let toolbar = self.render_toolbar(cx);
        let notices = self.render_notices(cx);
        let panel = self.render_panel(cx);
        let body = self.render_body(window, cx);
        div()
            .flex()
            .flex_col()
            .w_full()
            .child(toolbar)
            .children(notices)
            .children(panel)
            .child(body)
    }
}

// ── Shared pieces ─────────────────────────────────────────────────────────

pub(crate) fn small_button(id: impl Into<SharedString>, label: String) -> Button {
    Button::new(gpui::ElementId::Name(id.into()))
        .ghost()
        .xsmall()
        .label(label.clone())
        .accessibility_label(label)
}

pub(crate) fn dim(text: String) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(text)
}

pub(crate) fn heading(text: String) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(text)
}

pub(crate) fn code_block(text: String) -> gpui::Div {
    let s = Theme::scaled;
    div()
        .p(px(s(6.0)))
        .rounded(px(s(3.0)))
        .bg(Theme::input_bg())
        .font_family(Theme::editor_font_family())
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text())
        .child(text)
}

pub(crate) fn badge_chip(text: String, color: gpui::Hsla) -> gpui::Div {
    let s = Theme::scaled;
    div()
        .px(px(s(6.0)))
        .py(px(s(1.0)))
        .rounded(px(s(3.0)))
        .border_1()
        .border_color(color)
        .text_size(px(Theme::font_size_ratio(0.8)))
        .text_color(color)
        .child(text)
}

pub(crate) fn thousands(n: u64) -> String {
    crate::license::format_thousands(n)
}

/// The badge for an output that is not the whole result.
pub fn completeness_text(output: &CellOutput) -> Option<String> {
    match output.completeness {
        Completeness::Complete => None,
        Completeness::Truncated { cap } => Some(crate::localization::format(
            "notebook.visual.completeness.truncated",
            &[
                ("row_count", thousands(cap)),
                ("total_rows", thousands(output.row_count)),
            ],
        )),
        Completeness::Spilled { total } => Some(crate::localization::format(
            "notebook.visual.completeness.spilled",
            &[("row_count", thousands(total))],
        )),
        Completeness::Sampled { n } => Some(crate::localization::format(
            "notebook.visual.completeness.sampled",
            &[("row_count", thousands(n))],
        )),
        Completeness::Partial => Some(t("notebook.visual.completeness.partial")),
        Completeness::DerivedFromPartial => Some(t("notebook.visual.completeness.derived_partial")),
    }
}

/// Provenance as `(label key, value)` rows.
pub fn provenance_rows(output: &CellOutput) -> Vec<(&'static str, String)> {
    let p = &output.provenance;
    let mut rows = Vec::new();
    let connection = if p.connection_label.is_empty() {
        p.sources
            .iter()
            .map(|s| s.connection_label.clone())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        p.connection_label.clone()
    };
    if !connection.is_empty() {
        rows.push(("notebook.visual.provenance.connection", connection));
    }
    if let Some(db) = &p.database {
        rows.push(("notebook.visual.provenance.database", db.clone()));
    }
    if let Some(env) = &p.environment {
        rows.push(("notebook.visual.provenance.environment", env.clone()));
    }
    if let Some(at) = p.extracted_at {
        rows.push((
            "notebook.visual.provenance.extracted_at",
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
        ));
    }
    if !output.run_id.is_empty() {
        rows.push(("notebook.visual.provenance.run_id", output.run_id.clone()));
    }
    rows.push((
        "notebook.visual.provenance.rows",
        crate::localization::format(
            "notebook.visual.provenance.rows_value",
            &[
                ("row_count", thousands(output.rows.len() as u64)),
                ("total_rows", thousands(output.row_count)),
            ],
        ),
    ));
    rows.push((
        "notebook.visual.provenance.completeness",
        completeness_text(output).unwrap_or_else(|| t("notebook.visual.completeness.complete")),
    ));
    if let Some(hash) = &p.sql_hash {
        rows.push(("notebook.visual.provenance.sql_hash", hash.clone()));
    }
    rows
}

/// A dropdown button that picks one of `options` (`(label, value, checked)`)
/// and hands the value to `pick` on the cell view.
pub(crate) fn choice_button<T: Clone + 'static>(
    id: impl Into<SharedString>,
    label: String,
    options: Vec<(String, T, bool)>,
    view: Entity<VisualCellView>,
    pick: impl Fn(&mut VisualCellView, T, &mut Context<VisualCellView>) + 'static,
) -> impl IntoElement {
    let pick = std::rc::Rc::new(pick);
    small_button(id, label).dropdown_menu(move |mut menu, _, _| {
        for (text, value, checked) in options.clone() {
            let view = view.clone();
            let pick = pick.clone();
            menu = menu.item(PopupMenuItem::new(text).checked(checked).on_click(
                move |_, _, app| {
                    let value = value.clone();
                    view.update(app, |v, cx| pick(v, value, cx));
                },
            ));
        }
        menu
    })
}

/// One labelled row of an editor.
pub(crate) fn editor_row(label: String, control: impl IntoElement) -> gpui::Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(6.0)))
        .child(
            div()
                .w(px(s(110.0)))
                .flex_none()
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::text_dim())
                .child(label),
        )
        .child(control)
}

/// A text field element sized for an editor row.
pub(crate) fn text_field(state: &Entity<InputState>, width: f32) -> gpui::Div {
    div()
        .w(px(Theme::scaled(width)))
        .child(Input::new(state).xsmall())
}

/// The editor panel frame.
pub(crate) fn editor_panel() -> gpui::Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_col()
        .gap(px(s(4.0)))
        .mx(px(s(8.0)))
        .my(px(s(4.0)))
        .p(px(s(8.0)))
        .rounded(px(s(4.0)))
        .border_1()
        .border_color(Theme::border())
        .bg(Theme::input_bg())
}

/// Column choices for a picker, with a leading "none" entry when `none`.
pub(crate) fn column_options(
    columns: &[ResultColumn],
    current: Option<&str>,
    none: Option<String>,
) -> Vec<(String, Option<String>, bool)> {
    let mut out = Vec::new();
    if let Some(none) = none {
        out.push((none, None, current.is_none()));
    }
    for c in columns {
        out.push((
            c.name.clone(),
            Some(c.name.clone()),
            current.is_some_and(|n| n.eq_ignore_ascii_case(&c.name)),
        ));
    }
    out
}

/// The rows a cell computes from its inputs (its output).
pub(crate) fn evaluate(
    kind: VisualKind,
    inputs: &VisualInputs,
) -> Result<(Vec<ResultColumn>, Vec<Vec<sqlly_core::CellValue>>), DisplayMessage> {
    match kind {
        VisualKind::Chart => chart::evaluate(inputs),
        VisualKind::Table => table::evaluate(inputs),
        VisualKind::Pivot => pivot::evaluate(inputs),
        VisualKind::Kpi => kpi::evaluate(inputs),
        VisualKind::Scalar => scalar::evaluate(inputs),
        VisualKind::Transform => Err(crate::localization::deferred_text(
            "notebook.visual.errors.needs_local_run",
        )),
    }
}

/// The input's rows as they are, or the reason there are none.
pub(crate) fn pass_through(
    inputs: &VisualInputs,
) -> Result<(Vec<ResultColumn>, Vec<Vec<sqlly_core::CellValue>>), DisplayMessage> {
    let Some((output, rows)) = inputs.input_rows() else {
        return Err(chart::missing_input(inputs));
    };
    Ok((output.columns.clone(), rows.as_ref().clone()))
}
