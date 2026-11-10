//! SQL notebook tab body: cell list, params bar, Run All, Markdown preview.
//! Never calls `spawn_query_execution` — the host runs cells through
//! `execute_query_with_origin(ExecuteOrigin::NotebookCell)`.

use std::collections::{HashMap, VecDeque};

use crate::editor::{SqlEditor, SqlEditorEvent};
use crate::icon_catalog::AppIcon;
use crate::mutation_review::contains_non_select_statement;
use crate::notebook_model::{self, CellKind, ExpandError, Notebook, expand_sql, run_all_order};
use crate::query_params::{ParamSpec, ParamType, substitute_params};
use crate::results::{ResultsView, ResultsViewEvent};
use crate::theme::Theme;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton, Render,
    SharedString, Window, div, prelude::*, px,
};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::text::TextView;
use sqlly_lang::sqlmodel::DatabaseType;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CellRunState {
    Idle,
    Queued,
    Running,
    Done,
    Failed,
}

pub struct CellEntities {
    pub editor: Entity<SqlEditor>,
    pub results: Entity<ResultsView>,
    pub state: CellRunState,
    pub has_run: bool,
}

struct RunAllProgress {
    remaining: VecDeque<String>,
    ran: usize,
    failed: Option<String>,
}

pub struct NotebookView {
    model: Notebook,
    cells: HashMap<String, CellEntities>,
    editing_markdown: Option<String>,
    param_inputs: Vec<(String, Entity<InputState>)>,
    param_draft_input: Option<Entity<InputState>>,
    run_all: Option<RunAllProgress>,
    db_type: Option<DatabaseType>,
    dirty: bool,
    focus_handle: FocusHandle,
}

#[derive(Clone, Debug)]
pub enum NotebookEvent {
    NeedCellEntities { cell_ids: Vec<String> },
    RunCell { cell_id: String },
    Dirty,
    SaveRequested,
    SaveAsRequested,
    ExportMarkdownRequested,
    ExportJupyterRequested,
    Status { message: String, error: bool },
}

impl EventEmitter<NotebookEvent> for NotebookView {}

impl Focusable for NotebookView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl NotebookView {
    pub fn new(model: Notebook, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            cells: HashMap::new(),
            editing_markdown: None,
            param_inputs: Vec::new(),
            param_draft_input: None,
            run_all: None,
            db_type: None,
            dirty: false,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn model(&self) -> &Notebook {
        &self.model
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn set_database_type(&mut self, db: DatabaseType) {
        self.db_type = Some(db);
    }

    pub fn cell_entities(&self, id: &str) -> Option<(Entity<SqlEditor>, Entity<ResultsView>)> {
        self.cells
            .get(id)
            .map(|e| (e.editor.clone(), e.results.clone()))
    }

    pub fn cell_id_for_editor(&self, editor: &Entity<SqlEditor>) -> Option<String> {
        self.cells
            .iter()
            .find(|(_, e)| &e.editor == editor)
            .map(|(id, _)| id.clone())
    }

    pub fn focused_cell(&self, window: &Window, cx: &App) -> Option<String> {
        for (id, ent) in &self.cells {
            if ent.editor.read(cx).focus_handle(cx).is_focused(window) {
                return Some(id.clone());
            }
        }
        self.editing_markdown.clone()
    }

    pub fn ensure_cell_entities(&mut self, cx: &mut Context<Self>) {
        let missing: Vec<String> = self
            .model
            .cells
            .iter()
            .filter(|c| c.kind == CellKind::Sql && !self.cells.contains_key(&c.id))
            .map(|c| c.id.clone())
            .collect();
        if !missing.is_empty() {
            cx.emit(NotebookEvent::NeedCellEntities { cell_ids: missing });
        }
    }

    pub fn attach_cell_entities(
        &mut self,
        pairs: Vec<(String, Entity<SqlEditor>, Entity<ResultsView>)>,
        cx: &mut Context<Self>,
    ) {
        for (id, editor, results) in pairs {
            let source = self
                .model
                .cell(&id)
                .map(|c| c.source.clone())
                .unwrap_or_default();
            editor.update(cx, |e, cx| e.set_text(&source, cx));
            let edit_id = id.clone();
            cx.subscribe(&editor, move |this, editor, event: &SqlEditorEvent, cx| {
                if matches!(event, SqlEditorEvent::Change) {
                    let text = editor.read(cx).text().to_string();
                    if let Some(cell) = this.model.cells.iter_mut().find(|c| c.id == edit_id) {
                        cell.source = text;
                    }
                    this.dirty = true;
                    cx.emit(NotebookEvent::Dirty);
                    cx.notify();
                }
            })
            .detach();
            let run_id = id.clone();
            cx.subscribe(
                &results,
                move |this, _results, event: &ResultsViewEvent, cx| match event {
                    ResultsViewEvent::QueryCompleted => {
                        if let Some(ent) = this.cells.get_mut(&run_id) {
                            ent.state = CellRunState::Done;
                            ent.has_run = true;
                        }
                        if this.run_all.is_some() {
                            if let Some(progress) = this.run_all.as_mut() {
                                progress.ran += 1;
                            }
                            this.advance_run_queue(cx);
                        }
                        cx.notify();
                    }
                    ResultsViewEvent::QueryFailed => {
                        if let Some(ent) = this.cells.get_mut(&run_id) {
                            ent.state = CellRunState::Failed;
                            ent.has_run = true;
                        }
                        if let Some(progress) = this.run_all.take() {
                            let ran = progress.ran;
                            cx.emit(NotebookEvent::Status {
                                message: crate::localization::format(
                                    "editor.notebook.run_all_stopped",
                                    &[("cell_id", &run_id), ("count", &ran.to_string())],
                                ),
                                error: true,
                            });
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
            self.cells.insert(
                id,
                CellEntities {
                    editor,
                    results,
                    state: CellRunState::Idle,
                    has_run: false,
                },
            );
        }
        cx.notify();
    }

    pub fn sql_for_run(&self, cell_id: &str) -> Result<String, String> {
        let expanded = expand_sql(&self.model, cell_id, &|s| !contains_non_select_statement(s))
            .map_err(expand_error_message)?;
        if self.model.params.is_empty() {
            return Ok(expanded);
        }
        let Some(db) = self.db_type else {
            return Ok(expanded);
        };
        let specs: Vec<ParamSpec> = self
            .model
            .params
            .iter()
            .map(|(name, _)| ParamSpec {
                name: param_at_name(name),
                ty: ParamType::Text,
                default: None,
            })
            .collect();
        let values: Vec<(String, String)> = self
            .model
            .params
            .iter()
            .map(|(name, value)| (param_at_name(name), value.clone()))
            .collect();
        substitute_params(db, &expanded, &specs, &values)
    }

    pub fn result_markdown_tables(&self, cx: &App) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        for cell in &self.model.cells {
            if cell.kind != CellKind::Sql {
                continue;
            }
            let table = self.cells.get(&cell.id).and_then(|ent| {
                let rs = ent.results.read(cx).result_set_at(0)?;
                let n = rs.rows.len().min(50);
                Some(crate::result_export::markdown(
                    &rs.columns,
                    &rs.rows[..n],
                    true,
                ))
            });
            out.push((cell.id.clone(), table));
        }
        out
    }

    pub fn request_run(&mut self, cell_id: String, cx: &mut Context<Self>) {
        let mut queue = VecDeque::new();
        if let Ok(deps) = notebook_model::dependency_order(&self.model, &cell_id) {
            for dep in deps {
                let done = self
                    .cells
                    .get(&dep)
                    .is_some_and(|e| e.state == CellRunState::Done);
                if !done {
                    queue.push_back(dep);
                }
            }
        }
        queue.push_back(cell_id);
        for id in &queue {
            if let Some(ent) = self.cells.get_mut(id) {
                ent.state = CellRunState::Queued;
            }
        }
        self.run_all = Some(RunAllProgress {
            remaining: queue,
            ran: 0,
            failed: None,
        });
        self.advance_run_queue(cx);
    }

    pub fn run_all(&mut self, cx: &mut Context<Self>) {
        let ids = run_all_order(&self.model);
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            if let Some(ent) = self.cells.get_mut(id) {
                ent.state = CellRunState::Queued;
            }
        }
        self.run_all = Some(RunAllProgress {
            remaining: ids.into(),
            ran: 0,
            failed: None,
        });
        self.advance_run_queue(cx);
    }

    pub fn stop_run_all(&mut self, cx: &mut Context<Self>) {
        if let Some(progress) = self.run_all.as_mut() {
            progress.remaining.clear();
        }
        cx.notify();
    }

    fn advance_run_queue(&mut self, cx: &mut Context<Self>) {
        let Some(progress) = self.run_all.as_mut() else {
            return;
        };
        if let Some(id) = progress.remaining.pop_front() {
            if let Some(ent) = self.cells.get_mut(&id) {
                ent.state = CellRunState::Running;
            }
            cx.emit(NotebookEvent::RunCell { cell_id: id });
            cx.notify();
        } else {
            let ran = progress.ran;
            let failed = progress.failed.clone();
            self.run_all = None;
            let summary = crate::notebook_model::RunAllSummary {
                ran,
                skipped: 0,
                failed: failed.clone(),
            };
            let error = summary.failed.is_some();
            let message = if let Some(id) = &summary.failed {
                crate::localization::format(
                    "editor.notebook.run_all_stopped",
                    &[("cell_id", id), ("count", &summary.ran.to_string())],
                )
            } else {
                crate::localization::format(
                    "editor.notebook.run_all_completed",
                    &[("count", &summary.ran.to_string())],
                )
            };
            cx.emit(NotebookEvent::Status { message, error });
            cx.notify();
        }
    }

    fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        cx.emit(NotebookEvent::Dirty);
        cx.notify();
    }

    fn add_cell_at(&mut self, kind: CellKind, after: Option<usize>, cx: &mut Context<Self>) {
        let idx = self.model.add_cell(kind, after);
        let id = self.model.cells[idx].id.clone();
        self.mark_dirty(cx);
        if kind == CellKind::Sql {
            cx.emit(NotebookEvent::NeedCellEntities { cell_ids: vec![id] });
        }
    }

    fn request_markdown_editor(&mut self, id: String, cx: &mut Context<Self>) {
        self.editing_markdown = Some(id.clone());
        if !self.cells.contains_key(&id) {
            cx.emit(NotebookEvent::NeedCellEntities { cell_ids: vec![id] });
        }
        cx.notify();
    }

    fn sync_param_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.param_inputs.len() == self.model.params.len()
            && self
                .param_inputs
                .iter()
                .zip(self.model.params.iter())
                .all(|((n, _), (mn, _))| n == mn)
        {
            return;
        }
        self.param_inputs.clear();
        for (name, value) in self.model.params.clone() {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(format!("@{name}")));
            state.update(cx, |s, cx| {
                s.set_value(value.clone(), window, cx);
            });
            let captured = name.clone();
            cx.subscribe(&state, move |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some((_, stored)) =
                        this.model.params.iter_mut().find(|(n, _)| n == &captured)
                {
                    let next = input.read(cx).value().to_string();
                    if stored != &next {
                        *stored = next;
                        this.dirty = true;
                        cx.emit(NotebookEvent::Dirty);
                    }
                }
            })
            .detach();
            self.param_inputs.push((name, state));
        }
        if self.param_draft_input.is_none() {
            let draft = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "editor.notebook.param_name_placeholder",
                ))
            });
            self.param_draft_input = Some(draft);
        }
    }

    fn add_param_from_draft(&mut self, cx: &mut Context<Self>) {
        let Some(input) = &self.param_draft_input else {
            return;
        };
        let raw = input.read(cx).value().trim().to_string();
        if raw.is_empty() {
            return;
        }
        let name = raw.trim_start_matches('@').to_string();
        if self.model.params.iter().any(|(n, _)| n == &name) {
            return;
        }
        self.model.params.push((name, String::new()));
        input.update(cx, |s, cx| {
            // Window is not available here; empty via set_value needs window.
            let _ = (s, cx);
        });
        self.param_inputs.clear();
        self.mark_dirty(cx);
    }
}

fn param_at_name(name: &str) -> String {
    if name.starts_with('@') {
        name.to_string()
    } else {
        format!("@{name}")
    }
}

fn expand_error_message(err: ExpandError) -> String {
    let (key, id) = match err {
        ExpandError::Cycle(path) => ("editor.notebook.expand_cycle", path.join(" → ")),
        ExpandError::UnknownCell(id) => ("editor.notebook.expand_unknown_cell", id),
        ExpandError::NotSql(id) => ("editor.notebook.expand_not_sql", id),
        ExpandError::NotSelect(id) => ("editor.notebook.expand_not_select", id),
        ExpandError::SelfReference(id) => ("editor.notebook.expand_self_reference", id),
    };
    crate::localization::format(key, &[("cell_id", &id)])
}

fn icon_button(id: impl Into<SharedString>, icon: AppIcon) -> Button {
    Button::new(id.into())
        .ghost()
        .xsmall()
        .icon(Icon::empty().path(icon.path()))
}

impl Render for NotebookView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_param_inputs(window, cx);
        let s = Theme::scaled;
        let running_all = self.run_all.is_some();

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(8.0)))
            .h(px(Theme::row_height()))
            .border_b_1()
            .border_color(Theme::border())
            .when(self.is_dirty(), |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("editor.notebook.unsaved_badge")),
                )
            })
            .child(
                Button::new("nb-run-all")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path(AppIcon::Play.path()))
                    .label(crate::localization::text("editor.notebook.run_all_button"))
                    .on_click(cx.listener(|this, _, _, cx| this.run_all(cx))),
            )
            .when(running_all, |d| {
                d.child(
                    Button::new("nb-stop")
                        .ghost()
                        .xsmall()
                        .label(crate::localization::text("editor.notebook.stop_button"))
                        .on_click(cx.listener(|this, _, _, cx| this.stop_run_all(cx))),
                )
            })
            .child(
                Button::new("nb-save")
                    .ghost()
                    .xsmall()
                    .label(crate::localization::text("editor.notebook.save_button"))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(NotebookEvent::SaveRequested);
                    })),
            )
            .child(
                Button::new("nb-save-as")
                    .ghost()
                    .xsmall()
                    .label(crate::localization::text("editor.notebook.save_as_button"))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(NotebookEvent::SaveAsRequested);
                    })),
            )
            .child(
                Button::new("nb-export")
                    .ghost()
                    .xsmall()
                    .label(crate::localization::text(
                        "editor.notebook.export_markdown_button",
                    ))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(NotebookEvent::ExportMarkdownRequested);
                    })),
            )
            .child(
                Button::new("nb-export-ipynb")
                    .ghost()
                    .xsmall()
                    .label(crate::localization::text(
                        "editor.notebook.export_jupyter_button",
                    ))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(NotebookEvent::ExportJupyterRequested);
                    })),
            );

        let mut params_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(8.0)))
            .py(px(s(4.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "editor.notebook.parameters_label",
                    )),
            );
        for (name, state) in &self.param_inputs {
            params_bar = params_bar
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(format!("@{name}")),
                )
                .child(div().w(px(s(120.0))).child(Input::new(state)));
        }
        if let Some(draft) = &self.param_draft_input {
            params_bar = params_bar
                .child(div().w(px(s(100.0))).child(Input::new(draft)))
                .child(
                    Button::new("nb-add-param")
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path(AppIcon::Add.path()))
                        .label(crate::localization::text(
                            "editor.notebook.add_param_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.add_param_from_draft(cx))),
                );
        }

        let cell_count = self.model.cells.len();
        let mut cells_col = div().flex().flex_col().gap(px(s(8.0))).p(px(s(8.0)));
        for idx in 0..cell_count {
            let cell = &self.model.cells[idx];
            let id = cell.id.clone();
            let kind = cell.kind;
            let collapsed = cell.collapsed;
            let source = cell.source.clone();
            let writes = kind == CellKind::Sql && contains_non_select_statement(&source);
            let ent = self.cells.get(&id);
            let show_results = ent.is_some_and(|e| {
                e.has_run
                    || matches!(
                        e.state,
                        CellRunState::Running | CellRunState::Done | CellRunState::Failed
                    )
            });
            let editing_md = self.editing_markdown.as_deref() == Some(id.as_str());

            let header = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .h(px(Theme::row_height()))
                .child(
                    icon_button(format!("nb-run-{id}"), AppIcon::Play).on_click({
                        let id = id.clone();
                        cx.listener(move |this, _, _, cx| this.request_run(id.clone(), cx))
                    }),
                )
                .child(
                    icon_button(format!("nb-up-{id}"), AppIcon::ChevronUp).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if idx > 0 {
                                this.model.move_cell(idx, idx - 1);
                                this.mark_dirty(cx);
                            }
                        },
                    )),
                )
                .child(
                    icon_button(format!("nb-down-{id}"), AppIcon::ChevronDown).on_click(
                        cx.listener(move |this, _, _, cx| {
                            if idx + 1 < this.model.cells.len() {
                                this.model.move_cell(idx, idx + 1);
                                this.mark_dirty(cx);
                            }
                        }),
                    ),
                )
                .child(
                    icon_button(
                        format!("nb-collapse-{id}"),
                        if collapsed {
                            AppIcon::ChevronRight
                        } else {
                            AppIcon::ChevronDown
                        },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(cell) = this.model.cells.get_mut(idx) {
                            cell.collapsed = !cell.collapsed;
                            this.mark_dirty(cx);
                        }
                    })),
                )
                .child(
                    icon_button(format!("nb-del-{id}"), AppIcon::Close).on_click({
                        let id = id.clone();
                        cx.listener(move |this, _, _, cx| {
                            this.model.remove_cell(idx);
                            this.cells.remove(&id);
                            this.mark_dirty(cx);
                        })
                    }),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(match kind {
                            CellKind::Sql => "SQL",
                            CellKind::Markdown => "Markdown",
                        }),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(id.clone()),
                )
                .when(writes, |d| {
                    d.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.75)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text("editor.notebook.writes_badge")),
                    )
                });

            let mut body = div().flex().flex_col().gap(px(s(4.0)));
            if !collapsed {
                match kind {
                    CellKind::Sql => {
                        if let Some(ent) = ent {
                            body = body.child(
                                div()
                                    .h(px(s(160.0)))
                                    .min_h(px(s(160.0)))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .child(ent.editor.clone()),
                            );
                            if show_results {
                                body = body.child(
                                    div()
                                        .h(px(s(280.0)))
                                        .min_h(px(s(280.0)))
                                        .border_1()
                                        .border_color(Theme::border())
                                        .child(ent.results.clone()),
                                );
                            }
                        }
                    }
                    CellKind::Markdown => {
                        if editing_md {
                            if let Some(ent) = ent {
                                body = body
                                    .child(
                                        div()
                                            .h(px(s(160.0)))
                                            .min_h(px(s(160.0)))
                                            .border_1()
                                            .border_color(Theme::border())
                                            .child(ent.editor.clone()),
                                    )
                                    .child(
                                        Button::new(format!("nb-md-done-{id}"))
                                            .ghost()
                                            .xsmall()
                                            .label(crate::localization::text(
                                                "editor.notebook.markdown_done_button",
                                            ))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.editing_markdown = None;
                                                cx.notify();
                                            })),
                                    );
                            }
                        } else {
                            let click_id = id.clone();
                            body = body.child(
                                div()
                                    .p(px(s(8.0)))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .on_mouse_down(MouseButton::Left, {
                                        let click_id = click_id.clone();
                                        cx.listener(move |this, _, _, cx| {
                                            this.request_markdown_editor(click_id.clone(), cx);
                                        })
                                    })
                                    .child(
                                        TextView::markdown(
                                            SharedString::from(format!("nb-md-{id}")),
                                            if source.is_empty() {
                                                SharedString::from(crate::localization::text(
                                                    "editor.notebook.empty_markdown_cell",
                                                ))
                                            } else {
                                                SharedString::from(source.as_str())
                                            },
                                        )
                                        .selectable(true),
                                    ),
                            );
                        }
                    }
                }
            }

            cells_col = cells_col.child(
                div()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(3.0)))
                    .child(header)
                    .child(body),
            );
        }

        let last = cell_count.checked_sub(1);
        let footer = div()
            .flex()
            .flex_row()
            .gap(px(s(8.0)))
            .px(px(s(8.0)))
            .py(px(s(8.0)))
            .child(
                Button::new("nb-add-sql")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path(AppIcon::Add.path()))
                    .label("SQL")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_cell_at(CellKind::Sql, last, cx);
                    })),
            )
            .child(
                Button::new("nb-add-md")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path(AppIcon::Add.path()))
                    .label("Markdown")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_cell_at(CellKind::Markdown, last, cx);
                    })),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .key_context("Notebook")
            .track_focus(&self.focus_handle)
            .child(toolbar)
            .child(params_bar)
            .child(cells_col)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::QueryEvent;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    #[gpui::test]
    fn notebook_view_requests_entities_for_sql_cells_only(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let mut model = Notebook::new();
        model.add_cell(CellKind::Markdown, None);
        let (view, cx) = cx.add_window_view(|_window, cx| NotebookView::new(model, cx));
        let events: std::rc::Rc<std::cell::RefCell<Vec<NotebookEvent>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &NotebookEvent, _cx| {
                seen.borrow_mut().push(event.clone());
            })
            .detach();
        });
        view.update(cx, |nb, cx| nb.ensure_cell_entities(cx));
        let requested: Vec<String> = events
            .borrow()
            .iter()
            .filter_map(|e| match e {
                NotebookEvent::NeedCellEntities { cell_ids } => Some(cell_ids.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(requested, vec!["cell_1".to_string()]);
    }

    #[gpui::test]
    fn editor_change_updates_model_and_marks_dirty(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let model = Notebook::new();
        let (view, cx) = cx.add_window_view(|_window, cx| NotebookView::new(model, cx));
        let editor = cx.update(|_window, cx| cx.new(SqlEditor::new));
        let results = cx.update(|_window, cx| cx.new(ResultsView::new));
        view.update(cx, |nb, cx| {
            nb.attach_cell_entities(vec![("cell_1".into(), editor.clone(), results)], cx);
        });
        editor.update(cx, |e, cx| {
            e.set_text("SELECT 1", cx);
            cx.emit(SqlEditorEvent::Change);
        });
        view.read_with(cx, |nb, _| {
            assert_eq!(nb.model().cells[0].source, "SELECT 1");
            assert!(nb.is_dirty());
        });
    }

    #[gpui::test]
    fn run_all_chain_advances_on_completion(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let mut model = Notebook::new();
        model.add_cell(CellKind::Sql, Some(0));
        let (view, cx) = cx.add_window_view(|_window, cx| NotebookView::new(model, cx));
        let events: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &NotebookEvent, _cx| {
                if let NotebookEvent::RunCell { cell_id } = event {
                    seen.borrow_mut().push(cell_id.clone());
                }
            })
            .detach();
        });
        let editor1 = cx.update(|_window, cx| cx.new(SqlEditor::new));
        let results1 = cx.update(|_window, cx| cx.new(ResultsView::new));
        let editor2 = cx.update(|_window, cx| cx.new(SqlEditor::new));
        let results2 = cx.update(|_window, cx| cx.new(ResultsView::new));
        view.update(cx, |nb, cx| {
            nb.attach_cell_entities(
                vec![
                    ("cell_1".into(), editor1, results1.clone()),
                    ("cell_2".into(), editor2, results2),
                ],
                cx,
            );
            nb.run_all(cx);
        });
        assert_eq!(events.borrow().as_slice(), ["cell_1"]);
        results1.update(cx, |rv, cx| {
            rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
        });
        assert_eq!(events.borrow().as_slice(), ["cell_1", "cell_2"]);
    }

    #[gpui::test]
    fn run_all_stops_on_failure(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let mut model = Notebook::new();
        model.add_cell(CellKind::Sql, Some(0));
        let (view, cx) = cx.add_window_view(|_window, cx| NotebookView::new(model, cx));
        let events: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &NotebookEvent, _cx| {
                if let NotebookEvent::RunCell { cell_id } = event {
                    seen.borrow_mut().push(cell_id.clone());
                }
            })
            .detach();
        });
        let editor1 = cx.update(|_window, cx| cx.new(SqlEditor::new));
        let results1 = cx.update(|_window, cx| cx.new(ResultsView::new));
        let editor2 = cx.update(|_window, cx| cx.new(SqlEditor::new));
        let results2 = cx.update(|_window, cx| cx.new(ResultsView::new));
        view.update(cx, |nb, cx| {
            nb.attach_cell_entities(
                vec![
                    ("cell_1".into(), editor1, results1.clone()),
                    ("cell_2".into(), editor2, results2),
                ],
                cx,
            );
            nb.run_all(cx);
        });
        results1.update(cx, |rv, cx| {
            rv.handle_event(
                QueryEvent::Error {
                    message: "boom".into(),
                },
                cx,
            );
        });
        assert_eq!(events.borrow().as_slice(), ["cell_1"]);
    }
}
