//! Row editor modal — right-click a result row header, "Edit Row...".
//!
//! Opens in a loading state while the row-editing service resolves whether
//! the clicked row maps to one editable base table (provenance via
//! `sp_describe_first_result_set`), then shows a per-column form backed by
//! the pure [`RowEditorModel`]. Unsupported results show the reason instead
//! of failing silently. Saves run as guarded parameterized updates with
//! optimistic concurrency; conflicts refresh the form with the server's
//! current values.

// Browser build: swap tokio's channels/spawn for the reactor-free shim so
// the `tokio::` paths below compile unchanged.
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;

use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

use gpui::{
    App, Bounds, Context, Entity, EventEmitter, IntoElement, KeyDownEvent, MouseButton,
    MouseMoveEvent, MouseUpEvent, ParentElement, Render, SharedString, Styled, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::calendar::Date as PickerDate;
use gpui_component::checkbox::Checkbox;
use gpui_component::date_picker::{DatePicker, DatePickerEvent, DatePickerState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{Disableable as _, Sizable as _};
use sqlly_core::{CellValue, ConnectionProfile, StudioError};
use sqlly_services::Services;
use sqlly_services::row_editing::{
    DeleteRowOutcome, DeleteRowRequest, FkSearchResult, InsertRowOutcome, InsertRowRequest,
    RelatedRowsRequest, RelatedRowsResult, RowEditOpenOutcome, RowEditOpenRequest, TableTarget,
    UpdateRowOutcome, UpdateRowRequest, build_related_open_sql,
};

use crate::cell_actions::{self, actions_for};
use crate::dialog::{Dialog, DialogFrame};
use crate::fk_autocomplete::{FK_SEARCH_LIMIT, FkFieldState, fk_search_request};
use crate::mutation_review::SafetyFeatures;
use crate::pane_divider::{CollapseDirection, PaneDivider};
use crate::pending_changes::CellEdit;
use crate::row_editor_model::{FieldDraft, FieldKind, RowEditorModel};
use crate::theme::Theme;
use crate::tooltip::AppTooltip as _;

/// What the modal is doing with the resolved table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowEditorIntent {
    /// Edit the clicked row.
    Edit,
    /// Insert a brand-new row (blank form).
    Insert,
    /// Insert a new row prefilled from the clicked row.
    Duplicate,
    /// Insert a new row prefilled from the clicked row with primary-key columns cleared so the insert cannot collide.
    DuplicateWithoutKey,
    /// Open the Batch Insert dialog (never builds a row-editor model).
    BatchInsert,
}

/// Whether Save commits immediately or stages into the pending batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowEditorMode {
    #[default]
    Commit,
    Stage,
}

/// A row operation staged from the modal for batch save.
#[derive(Clone, Debug)]
pub enum StagedRowOp {
    Update {
        key: crate::pending_changes::RowKey,
        original_row: Vec<CellValue>,
        changes: std::collections::BTreeMap<String, crate::pending_changes::CellEdit>,
    },
    Insert {
        values: Vec<(String, crate::pending_changes::CellEdit)>,
    },
    Delete {
        key: crate::pending_changes::RowKey,
        original_row: Vec<CellValue>,
    },
}

impl RowEditorIntent {
    fn is_insert(self) -> bool {
        matches!(
            self,
            RowEditorIntent::Insert
                | RowEditorIntent::Duplicate
                | RowEditorIntent::DuplicateWithoutKey
                | RowEditorIntent::BatchInsert
        )
    }
}

/// Rows to preview inline for a related child table.
const RELATED_ROWS_LIMIT: usize = 20;
const DEFAULT_RELATED_PANEL_HEIGHT: f32 = 190.0;
const MIN_RELATED_PANEL_HEIGHT: f32 = 120.0;
const MAX_RELATED_PANEL_HEIGHT: f32 = 420.0;

#[derive(Clone, Copy)]
struct RelatedPanelResize {
    start_y: f32,
    start_height: f32,
}

fn related_panel_height_for_drag(resize: RelatedPanelResize, pointer_y: f32) -> f32 {
    (resize.start_height + resize.start_y - pointer_y)
        .clamp(MIN_RELATED_PANEL_HEIGHT, MAX_RELATED_PANEL_HEIGHT)
}

/// Events for the host (`AppView`).
#[derive(Clone)]
#[allow(clippy::large_enum_variant)] // Boxing the large variant is deferred; low instance count.
pub enum RowEditorEvent {
    /// The dialog wants to close. `saved` reports whether at least one update
    /// was committed, so the host can refresh the originating result set.
    Dismissed { saved: bool },
    /// A row change was staged into the pending batch (Stage mode only).
    Staged {
        session: sqlly_services::row_editing::RowEditSession,
        op: StagedRowOp,
    },
    /// Open a read-only query (related-rows "Open query" action) in a new tab.
    OpenQuery { sql: String },
    /// Re-open the editor in its own window without losing the current draft.
    PopOut(RowEditorPopOut),
}

/// The state transferred from a modal row editor into a standalone window.
#[derive(Clone)]
pub struct RowEditorPopOut {
    services: Services,
    profile: ConnectionProfile,
    model: RowEditorModel,
    intent: RowEditorIntent,
    mode: RowEditorMode,
}

/// Choices behind a bool/enum select: unique display labels, the business
/// value of each row (`None` = NULL), and the row matching the draft.
struct SelectChoices {
    labels: Vec<String>,
    values: Vec<Option<String>>,
    selected: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowEditorPresentation {
    Modal,
    Window,
}

/// One incoming foreign key rendered as a collapsible related-rows panel.
struct RelatedChild {
    /// Display heading, e.g. `dbo.Orders (via FK_Orders_Customer)`.
    heading: String,
    table: TableTarget,
    /// `child_column = value` predicates identifying the related rows.
    predicates: Vec<(String, CellValue)>,
    /// True when every referencing value is available (non-NULL); NULL keys
    /// cannot have child rows.
    resolvable: bool,
    /// A ready-to-run full query (values inlined) for the "Open query" action.
    open_sql: String,
    expanded: bool,
    loading: bool,
    error: Option<String>,
    result: Option<RelatedRowsResult>,
}

/// Where the dialog is in its lifecycle.
enum Phase {
    Loading,
    NotEditable { reason: String },
    Ready,
    Saving,
}

pub struct RowEditor {
    services: Services,
    profile: ConnectionProfile,
    phase: Phase,
    model: Option<RowEditorModel>,
    banner_error: Option<String>,
    banner_info: Option<String>,
    /// The mutation policy asks for confirmation: the first Save click arms
    /// this, the second executes.
    confirm_pending: bool,
    /// Whether any save has been committed in this dialog's lifetime.
    saved: bool,
    /// Per-field FK autocomplete state, keyed by form-field index. Only
    /// FK-lead editable fields ever have an entry.
    fk_states: HashMap<usize, FkFieldState>,
    /// Read-only related-child panels, one per incoming foreign key.
    related: Vec<RelatedChild>,
    /// Height of the related-rows work area, adjusted by its divider.
    related_panel_height: f32,
    related_panel_resize: Option<RelatedPanelResize>,
    /// Whether this modal edits, inserts, or duplicates.
    intent: RowEditorIntent,
    /// Commit immediately or stage into the grid's pending batch.
    mode: RowEditorMode,
    /// The delete action's two-step confirmation is armed.
    delete_pending: bool,
    /// One optional `InputState` per form field. Bool and binary fields skip
    /// the text input; remaining fields keep an entry so indices line up.
    field_inputs: Vec<Option<Entity<InputState>>>,
    /// Optional Select for bool / enum fields, same indexing as `model.fields`.
    field_selects: Vec<Option<Entity<SelectState<Vec<String>>>>>,
    /// Optional date picker for `Date` fields, same indexing as `model.fields`.
    field_dates: Vec<Option<Entity<DatePickerState>>>,
    /// Change subscriptions for `field_inputs` and `field_selects`.
    _subscriptions: Vec<Subscription>,
    /// Floating-card geometry for the unified dialog chrome.
    frame: DialogFrame,
    presentation: RowEditorPresentation,
}

impl EventEmitter<RowEditorEvent> for RowEditor {}

impl RowEditor {
    /// Creates the dialog in loading state and resolves the session in the
    /// background, for the given `intent` (edit, insert, or duplicate).
    pub fn open_with(
        services: Services,
        profile: ConnectionProfile,
        request: RowEditOpenRequest,
        intent: RowEditorIntent,
        mode: RowEditorMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = Self {
            services: services.clone(),
            profile: profile.clone(),
            phase: Phase::Loading,
            model: None,
            banner_error: None,
            banner_info: None,
            confirm_pending: false,
            saved: false,
            fk_states: HashMap::new(),
            related: Vec::new(),
            related_panel_height: DEFAULT_RELATED_PANEL_HEIGHT,
            related_panel_resize: None,
            intent,
            mode,
            delete_pending: false,
            field_inputs: Vec::new(),
            field_selects: Vec::new(),
            field_dates: Vec::new(),
            _subscriptions: Vec::new(),
            frame: DialogFrame::new(Theme::scaled(640.0), Theme::scaled(560.0)),
            presentation: RowEditorPresentation::Modal,
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<RowEditOpenOutcome, StudioError>>(1);
        cx.background_spawn(async move {
            let outcome = match crate::tokio_handle()
                .spawn(async move {
                    if intent.is_insert() {
                        services
                            .row_editing
                            .open_insert_session(&profile, request)
                            .await
                    } else {
                        services.row_editing.open_session(&profile, request).await
                    }
                })
                .await
            {
                Ok(outcome) => outcome,
                Err(join_err) => Err(StudioError::Other(join_err.to_string())),
            };
            let _ = tx.send(outcome).await;
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            if let Some(outcome) = rx.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.apply_open_outcome(outcome, window, cx);
                });
            }
        })
        .detach();

        this
    }

    fn from_pop_out(payload: RowEditorPopOut, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut editor = Self {
            services: payload.services,
            profile: payload.profile,
            phase: Phase::Ready,
            model: Some(payload.model),
            banner_error: None,
            banner_info: None,
            confirm_pending: false,
            saved: false,
            fk_states: HashMap::new(),
            related: Vec::new(),
            related_panel_height: DEFAULT_RELATED_PANEL_HEIGHT,
            related_panel_resize: None,
            intent: payload.intent,
            mode: payload.mode,
            delete_pending: false,
            field_inputs: Vec::new(),
            field_selects: Vec::new(),
            field_dates: Vec::new(),
            _subscriptions: Vec::new(),
            frame: DialogFrame::new(Theme::scaled(640.0), Theme::scaled(560.0)),
            presentation: RowEditorPresentation::Window,
        };
        editor.rebuild_field_inputs(window, cx);
        if editor.intent == RowEditorIntent::Edit {
            editor.rebuild_related();
        }
        editor
    }

    pub fn open_window(payload: RowEditorPopOut, cx: &mut App) -> Option<Entity<Self>> {
        let bounds = Bounds::centered(None, size(px(760.0), px(640.0)), cx);
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot_in = slot.clone();
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from(crate::localization::text(
                        "results.row_editor.detached_edit_title",
                    ))),
                    ..Default::default()
                }),
                app_id: Some(crate::about::APP_ID.into()),
                is_resizable: true,
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| Self::from_pop_out(payload.clone(), window, cx));
                *slot_in.borrow_mut() = Some(view.clone());
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            },
        );
        if opened.is_err() {
            return None;
        }
        slot.borrow_mut().take()
    }

    fn apply_open_outcome(
        &mut self,
        outcome: Result<RowEditOpenOutcome, StudioError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(RowEditOpenOutcome::Editable(session)) => {
                let database_type = self.profile.database_type;
                let model = match self.intent {
                    RowEditorIntent::Edit => RowEditorModel::new(*session, database_type),
                    RowEditorIntent::Insert | RowEditorIntent::BatchInsert => {
                        RowEditorModel::new_insert(*session, None, database_type)
                    }
                    RowEditorIntent::Duplicate => {
                        let seed = session.original_row.clone();
                        RowEditorModel::new_insert(*session, Some(seed), database_type)
                    }
                    RowEditorIntent::DuplicateWithoutKey => {
                        let seed = session.original_row.clone();
                        RowEditorModel::new_insert_without_key(*session, Some(seed), database_type)
                    }
                };
                self.model = Some(model);
                self.phase = Phase::Ready;
                self.rebuild_field_inputs(window, cx);
                // Insert forms have no persisted row, so no related children.
                if self.intent == RowEditorIntent::Edit {
                    self.rebuild_related();
                }
            }
            Ok(RowEditOpenOutcome::NotEditable { reason, message }) => {
                let reason = crate::localization::stored_message_or(message.as_ref(), reason);
                self.phase = Phase::NotEditable { reason };
            }
            Err(err) => {
                self.phase = Phase::NotEditable {
                    reason: crate::localization::message(err.message()),
                };
            }
        }
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
        if self.presentation == RowEditorPresentation::Window {
            window.remove_window();
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(RowEditorEvent::Dismissed { saved: self.saved });
    }

    fn pop_out(&mut self, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        cx.emit(RowEditorEvent::PopOut(RowEditorPopOut {
            services: self.services.clone(),
            profile: self.profile.clone(),
            model: model.clone(),
            intent: self.intent,
            mode: self.mode,
        }));
    }

    /// The model's current draft text for a field ("" for NULL drafts —
    /// exactly what the field's input should display).
    fn draft_text(field: &crate::row_editor_model::FieldState) -> String {
        match &field.draft {
            FieldDraft::Null => String::new(),
            FieldDraft::Text(text) => text.clone(),
            FieldDraft::Expression(expr) => cell_actions::localized_expression_label(*expr),
            FieldDraft::Bytes(bytes) => crate::localization::format(
                "results.row_editor.binary_byte_count",
                &[("byte_count", &bytes.len().to_string())],
            ),
        }
    }

    /// (Re)creates one `InputState` per form field, seeded from the model's
    /// drafts, with a Change subscription that routes edits into `set_text`.
    /// Called once the open outcome resolves the session (the only time the
    /// field set changes).
    fn rebuild_field_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.field_inputs.clear();
        self.field_selects.clear();
        self.field_dates.clear();
        self._subscriptions.clear();
        let Some(model) = &self.model else { return };
        let db = self.profile.database_type;
        let drafts: Vec<String> = model.fields.iter().map(Self::draft_text).collect();
        for (index, field) in model.fields.iter().enumerate() {
            let actions = actions_for(db, &field.column, field.is_editable());
            let skip_input = actions.bool_choices || field.kind == FieldKind::Binary;
            if skip_input {
                self.field_inputs.push(None);
            } else {
                let text = drafts.get(index).cloned().unwrap_or_default();
                let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
                let subscription = cx.subscribe_in(
                    &input,
                    window,
                    move |this: &mut Self, input, event: &InputEvent, _window, cx| {
                        if let InputEvent::Change = event {
                            let text = input.read(cx).value().to_string();
                            let unchanged = this
                                .model
                                .as_ref()
                                .and_then(|model| model.fields.get(index))
                                .map(|field| Self::draft_text(field) == text)
                                .unwrap_or(true);
                            if !unchanged {
                                this.set_text(index, text, cx);
                            }
                        }
                    },
                );
                self.field_inputs.push(Some(input));
                self._subscriptions.push(subscription);
            }
            if let Some(choices) = Self::select_choices(field, &actions) {
                let select = cx.new(|cx| {
                    SelectState::new(
                        choices.labels,
                        choices.selected.map(gpui_component::IndexPath::new),
                        window,
                        cx,
                    )
                });
                let subscription = cx.subscribe_in(
                    &select,
                    window,
                    move |this: &mut Self, select, event: &SelectEvent<Vec<String>>, window, cx| {
                        let SelectEvent::Confirm(value) = event;
                        if value.is_none() {
                            return;
                        }
                        // Map the confirmed row (labels are unique) back to
                        // the business value: a literal enum value spelled
                        // `NULL` must not be read as the null entry.
                        let Some(row) = crate::connection_editor::select_row(select, cx) else {
                            return;
                        };
                        let choice = this
                            .model
                            .as_ref()
                            .and_then(|model| model.fields.get(index))
                            .and_then(|field| {
                                let actions = actions_for(
                                    this.profile.database_type,
                                    &field.column,
                                    field.is_editable(),
                                );
                                Self::select_choices(field, &actions)
                            })
                            .and_then(|choices| choices.values.get(row).cloned());
                        let Some(choice) = choice else { return };
                        this.apply_select_choice(index, choice, window, cx);
                    },
                );
                self.field_selects.push(Some(select));
                self._subscriptions.push(subscription);
            } else {
                self.field_selects.push(None);
            }
            // Date columns get a calendar picker whose picks write the ISO
            // text the model already validates; the text input stays as a
            // fallback only when no picker could be built.
            if field.kind == FieldKind::Date && field.is_editable() && !field.fk_lead {
                let seed = drafts.get(index).and_then(|text| {
                    chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
                });
                let picker = cx.new(|cx| {
                    let mut state = DatePickerState::new(window, cx);
                    if let Some(date) = seed {
                        state.set_date(date, window, cx);
                    }
                    state
                });
                let subscription = cx.subscribe_in(
                    &picker,
                    window,
                    move |this: &mut Self, _, event: &DatePickerEvent, _window, cx| {
                        // gpui-component 0.7 reports a date-time; this
                        // picker never shows a time field, so only the date
                        // part matters.
                        let DatePickerEvent::Change(value) = event;
                        if let PickerDate::Single(Some(day)) = value.date() {
                            let text = day.format("%Y-%m-%d").to_string();
                            let unchanged = this
                                .model
                                .as_ref()
                                .and_then(|model| model.fields.get(index))
                                .map(|field| Self::draft_text(field) == text)
                                .unwrap_or(true);
                            if !unchanged {
                                this.set_text(index, text, cx);
                            }
                        }
                    },
                );
                self.field_dates.push(Some(picker));
                self._subscriptions.push(subscription);
            } else {
                self.field_dates.push(None);
            }
        }
    }

    fn select_choices(
        field: &crate::row_editor_model::FieldState,
        actions: &cell_actions::CellActionSet,
    ) -> Option<SelectChoices> {
        let values: Vec<Option<String>> = if actions.bool_choices {
            vec![Some("true".to_string()), Some("false".to_string())]
        } else if !actions.enum_choices.is_empty() {
            actions.enum_choices.iter().cloned().map(Some).collect()
        } else {
            return None;
        };
        let mut values = values;
        if field.column.nullable {
            values.push(None);
        }
        let selected = match &field.draft {
            FieldDraft::Null => values.iter().position(Option::is_none),
            FieldDraft::Text(text) if actions.bool_choices => {
                match text.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" => Some(0),
                    "false" | "0" => Some(1),
                    _ => None,
                }
            }
            FieldDraft::Text(text) => values
                .iter()
                .position(|value| value.as_deref() == Some(text.as_str())),
            _ => None,
        };
        Some(SelectChoices {
            labels: Self::choice_labels(&values),
            values,
            selected,
        })
    }

    /// Unique display labels for `values` (`None` is the null entry). The
    /// null entry claims the plain `NULL` label first, so a real value
    /// spelled `NULL` is shown as `NULL (2)` instead of colliding with it.
    fn choice_labels(values: &[Option<String>]) -> Vec<String> {
        let null_at = values.iter().position(Option::is_none);
        let ordered = null_at
            .map(|_| "NULL".to_string())
            .into_iter()
            .chain(values.iter().flatten().cloned());
        let mut unique = crate::connection_editor::unique_select_labels(ordered);
        if let Some(null_at) = null_at {
            let null_label = unique.remove(0);
            unique.insert(null_at, null_label);
        }
        unique
    }

    /// Apply a picked choice; `None` sets the field to NULL.
    fn apply_select_choice(
        &mut self,
        index: usize,
        choice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match choice {
            None => {
                if let Some(model) = &mut self.model {
                    model.set_null(index, true);
                }
            }
            Some(value) => self.set_text(index, value, cx),
        }
        self.sync_field_inputs(window, cx);
        self.confirm_pending = false;
        self.delete_pending = false;
        self.banner_info = None;
        cx.notify();
    }

    /// Pushes the model's current drafts back into the per-field inputs after
    /// a programmatic row change (reset, refresh, save, FK pick, NULL toggle).
    fn sync_field_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let db = self.profile.database_type;
        let drafts: Vec<String> = model.fields.iter().map(Self::draft_text).collect();
        for (index, input) in self.field_inputs.clone().into_iter().enumerate() {
            let Some(input) = input else { continue };
            let Some(text) = drafts.get(index) else {
                continue;
            };
            input.update(cx, |state, cx| {
                if state.value().as_ref() != text.as_str() {
                    state.set_value(text.clone(), window, cx);
                }
            });
        }
        for (index, select) in self.field_selects.clone().into_iter().enumerate() {
            let Some(select) = select else { continue };
            let Some(field) = model.fields.get(index) else {
                continue;
            };
            let actions = actions_for(db, &field.column, field.is_editable());
            let Some(SelectChoices { selected, .. }) = Self::select_choices(field, &actions) else {
                continue;
            };
            select.update(cx, |state, cx| {
                state.set_selected_index(selected.map(gpui_component::IndexPath::new), window, cx);
            });
        }
        for (index, picker) in self.field_dates.clone().into_iter().enumerate() {
            let Some(picker) = picker else { continue };
            let Some(text) = drafts.get(index) else {
                continue;
            };
            let Some(date) = chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok() else {
                continue;
            };
            picker.update(cx, |state, cx| {
                state.set_date(date, window, cx);
            });
        }
    }

    fn set_text(&mut self, index: usize, text: String, cx: &mut Context<Self>) {
        let is_fk_lead = self
            .model
            .as_ref()
            .and_then(|model| model.fields.get(index))
            .map(|field| field.fk_lead && field.is_editable())
            .unwrap_or(false);
        let Some(model) = &mut self.model else { return };
        model.set_text(index, text.clone());
        self.confirm_pending = false;
        self.delete_pending = false;
        self.banner_info = None;
        if is_fk_lead {
            self.fk_schedule_search(index, text, cx);
        }
        cx.notify();
    }

    /// Debounced FK search: opens the dropdown immediately, then fires the
    /// server query after a short quiet period. Rapid typing supersedes pending
    /// searches via the per-field generation guard.
    fn fk_schedule_search(&mut self, index: usize, query: String, cx: &mut Context<Self>) {
        let state = self.fk_states.entry(index).or_default();
        let generation = state.begin_search(query);
        let async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(160))
                .await;
            let mut cx = async_cx.clone();
            let _ = weak.update(&mut cx, |this, cx| {
                let still_current = this
                    .fk_states
                    .get(&index)
                    .map(|state| state.generation == generation)
                    .unwrap_or(false);
                if still_current {
                    this.fk_run_search(index, generation, cx);
                }
            });
        })
        .detach();
    }

    /// Runs the referenced-table search for a field and applies the result when
    /// it is still the newest search for that field.
    fn fk_run_search(&mut self, index: usize, generation: u64, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let Some(field) = model.fields.get(index) else {
            return;
        };
        let Some(fk_index) = field.fk else { return };
        let Some(fk) = model.session.foreign_keys.get(fk_index) else {
            return;
        };
        let query = self
            .fk_states
            .get(&index)
            .map(|state| state.query.clone())
            .unwrap_or_default();
        let request = fk_search_request(fk, &query, FK_SEARCH_LIMIT);

        let services = self.services.clone();
        let profile = self.profile.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<FkSearchResult, StudioError>>(1);
        cx.background_spawn(async move {
            let outcome = match crate::tokio_handle()
                .spawn(async move { services.row_editing.fk_search(&profile, request).await })
                .await
            {
                Ok(outcome) => outcome,
                Err(join_err) => Err(StudioError::Other(join_err.to_string())),
            };
            let _ = tx.send(outcome).await;
        })
        .detach();
        cx.spawn(async move |this, cx| {
            if let Some(outcome) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(state) = this.fk_states.get_mut(&index) {
                        match outcome {
                            Ok(result) => {
                                state.apply_results(
                                    generation,
                                    result.candidates,
                                    result.truncated,
                                );
                            }
                            Err(err) => {
                                state.apply_error(
                                    generation,
                                    crate::localization::message(err.message()),
                                );
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Applies a chosen FK candidate to the model (filling every local column
    /// of the key) and closes the dropdown.
    fn fk_pick(
        &mut self,
        index: usize,
        candidate_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked = self
            .fk_states
            .get(&index)
            .and_then(|state| state.candidates.get(candidate_index).cloned());
        let Some(candidate) = picked else { return };
        let fk_index = self
            .model
            .as_ref()
            .and_then(|model| model.fields.get(index))
            .and_then(|field| field.fk);
        if let (Some(model), Some(fk_index)) = (&mut self.model, fk_index) {
            model.apply_fk_selection(fk_index, candidate.key, Some(candidate.label));
        }
        if let Some(state) = self.fk_states.get_mut(&index) {
            state.close();
        }
        self.sync_field_inputs(window, cx);
        self.confirm_pending = false;
        self.delete_pending = false;
        self.banner_info = None;
        cx.notify();
    }

    fn fk_close(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(state) = self.fk_states.get_mut(&index) {
            state.close();
            cx.notify();
        }
    }

    /// Closes every open FK dropdown (after reset/refresh/save, whose row
    /// changes invalidate the current candidates).
    fn fk_close_all(&mut self) {
        for state in self.fk_states.values_mut() {
            state.close();
        }
    }

    /// Rebuilds the related-child panels from the model's authoritative row.
    /// Called on open and whenever the row is refetched (refresh/save).
    fn rebuild_related(&mut self) {
        let Some(model) = &self.model else {
            self.related.clear();
            return;
        };
        let session = &model.session;
        let value_of = |column: &str| -> Option<CellValue> {
            session
                .columns
                .iter()
                .position(|c| c.name == column)
                .map(|i| session.original_row[i].clone())
        };
        let mut related = Vec::new();
        for fk in &session.incoming_foreign_keys {
            let mut predicates = Vec::new();
            let mut resolvable = true;
            for (child_column, this_column) in &fk.column_pairs {
                match value_of(this_column) {
                    Some(value) if !matches!(value, CellValue::Null) => {
                        predicates.push((child_column.clone(), value));
                    }
                    _ => {
                        resolvable = false;
                        predicates.push((child_column.clone(), CellValue::Null));
                    }
                }
            }
            let request = RelatedRowsRequest {
                table: fk.child.clone(),
                predicates: predicates.clone(),
                limit: RELATED_ROWS_LIMIT,
            };
            related.push(RelatedChild {
                heading: crate::localization::format(
                    "results.row_editor.related_table_heading",
                    &[
                        ("table_name", &fk.child.quoted()),
                        ("constraint_name", &fk.constraint_name),
                    ],
                ),
                table: fk.child.clone(),
                predicates,
                resolvable,
                open_sql: build_related_open_sql(&request),
                expanded: false,
                loading: false,
                error: None,
                result: None,
            });
        }
        self.related = related;
    }

    /// Expands/collapses a related panel, lazily loading its rows on first open.
    fn related_toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(child) = self.related.get_mut(index) else {
            return;
        };
        child.expanded = !child.expanded;
        let needs_load = child.expanded && child.result.is_none() && child.resolvable;
        cx.notify();
        if needs_load {
            self.related_load(index, cx);
        }
    }

    fn related_load(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(child) = self.related.get_mut(index) else {
            return;
        };
        if child.loading || !child.resolvable {
            return;
        }
        child.loading = true;
        child.error = None;
        let request = RelatedRowsRequest {
            table: child.table.clone(),
            predicates: child.predicates.clone(),
            limit: RELATED_ROWS_LIMIT,
        };
        cx.notify();

        let services = self.services.clone();
        let profile = self.profile.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<RelatedRowsResult, StudioError>>(1);
        cx.background_spawn(async move {
            let outcome = match crate::tokio_handle()
                .spawn(async move { services.row_editing.related_rows(&profile, request).await })
                .await
            {
                Ok(outcome) => outcome,
                Err(join_err) => Err(StudioError::Other(join_err.to_string())),
            };
            let _ = tx.send(outcome).await;
        })
        .detach();
        cx.spawn(async move |this, cx| {
            if let Some(outcome) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(child) = this.related.get_mut(index) {
                        child.loading = false;
                        match outcome {
                            Ok(result) => child.result = Some(result),
                            Err(err) => {
                                child.error = Some(crate::localization::message(err.message()))
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Opens the full related-rows query in a new editor tab.
    fn related_open_query(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(child) = self.related.get(index) {
            cx.emit(RowEditorEvent::OpenQuery {
                sql: child.open_sql.clone(),
            });
        }
    }

    fn start_related_panel_resize(&mut self, pointer_y: f32) {
        self.related_panel_resize = Some(RelatedPanelResize {
            start_y: pointer_y,
            start_height: self.related_panel_height,
        });
    }

    fn update_related_panel_resize(&mut self, pointer_y: f32, cx: &mut Context<Self>) {
        let Some(resize) = self.related_panel_resize else {
            return;
        };
        self.related_panel_height = related_panel_height_for_drag(resize, pointer_y);
        cx.notify();
    }

    fn end_related_panel_resize(&mut self, cx: &mut Context<Self>) {
        if self.related_panel_resize.take().is_some() {
            cx.notify();
        }
    }

    fn toggle_null(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = &mut self.model {
            let is_null = matches!(
                model.fields.get(index).map(|f| &f.draft),
                Some(FieldDraft::Null)
            );
            model.set_null(index, !is_null);
            self.sync_field_inputs(window, cx);
            self.delete_pending = false;
            self.confirm_pending = false;
            self.banner_info = None;
            cx.notify();
        }
    }

    /// True when this editor's connection is reached through a relay bridge
    /// that has not advertised the server-evaluated generator expressions
    /// (an older bridge rejects them).
    fn server_expressions_blocked(&self) -> bool {
        cell_actions::server_expressions_blocked(&self.profile, &self.services)
    }

    fn apply_expression(
        &mut self,
        index: usize,
        expr: sqlly_services::row_editing::ColumnExpression,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.server_expressions_blocked() && !cell_actions::expression_allowed_over_relay(expr) {
            self.banner_info = Some(cell_actions::localized_relay_sql_functions_reason());
            cx.notify();
            return;
        }
        if let Some(model) = &mut self.model {
            model.set_expression(index, expr);
            self.sync_field_inputs(window, cx);
            self.delete_pending = false;
            self.confirm_pending = false;
            self.banner_info = None;
            cx.notify();
        }
    }

    fn apply_quick_pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        let Some(field) = model.fields.get(index) else {
            return;
        };
        let Some(text) = cell_actions::quick_pick_now(field.kind) else {
            return;
        };
        self.set_text(index, text, cx);
        self.sync_field_inputs(window, cx);
        cx.notify();
    }

    /// Whether a field offers the transform macros: an editable free-text
    /// field (not a bool/enum select, not binary, not an FK lead). Pure so
    /// the menu enable rule and tests agree.
    pub(crate) fn field_offers_transforms(
        field: &crate::row_editor_model::FieldState,
        actions: &cell_actions::CellActionSet,
        editable: bool,
    ) -> bool {
        // `actions.transforms` carries the plain-text rule: JSON, XML, ENUM,
        // and SET fields are edited as text but never offered macros.
        editable
            && actions.transforms
            && field.kind == FieldKind::Text
            && !actions.bool_choices
            && actions.enum_choices.is_empty()
            && !field.fk_lead
    }

    /// Apply a transform macro to the field's current draft text. A NULL or
    /// expression draft has no text to transform and is left alone.
    fn apply_transform(
        &mut self,
        index: usize,
        transform: crate::cell_transform::CellTransform,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = &self.model else { return };
        let Some(field) = model.fields.get(index) else {
            return;
        };
        let FieldDraft::Text(text) = &field.draft else {
            return;
        };
        let transformed = transform.apply(text);
        if transformed == *text {
            return;
        }
        self.set_text(index, transformed, cx);
        self.sync_field_inputs(window, cx);
        cx.notify();
    }

    /// Fill the field with a fresh random UUID (the generator insert for
    /// Guid columns, same source as the query-params strip's Generate).
    fn generate_uuid(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.set_text(index, cell_actions::new_uuid_text(), cx);
        self.sync_field_inputs(window, cx);
        cx.notify();
    }

    fn load_binary_from_file(&mut self, index: usize, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = index;
            self.banner_error = Some(crate::localization::text(
                "results.row_editor.load_file_browser_unavailable",
            ));
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: None,
            });
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, cx| {
                if let Ok(Ok(Some(paths))) = rx.await
                    && let Some(path) = paths.into_iter().next()
                {
                    let read = cx
                        .background_executor()
                        .spawn(async move { std::fs::read(&path) })
                        .await;
                    let mut cx = async_cx.clone();
                    weak.update(&mut cx, |this, cx| {
                        match read {
                            Ok(bytes) => {
                                if let Some(message) =
                                    cell_actions::blob_too_large(bytes.len() as u64)
                                {
                                    this.banner_error = Some(message);
                                } else if let Some(model) = &mut this.model {
                                    model.set_bytes(index, bytes);
                                    this.banner_error = None;
                                    this.confirm_pending = false;
                                    this.delete_pending = false;
                                }
                            }
                            Err(err) => {
                                this.banner_error = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    fn save_binary_to_file(&mut self, index: usize, column_name: String, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (index, column_name);
            self.banner_error = Some(crate::localization::text(
                "results.row_editor.save_file_browser_unavailable",
            ));
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(model) = &self.model else { return };
            let Some(field) = model.fields.get(index) else {
                return;
            };
            let CellValue::Bytes(bytes) = field.original.clone() else {
                return;
            };
            let hint = format!("{column_name}.bin");
            let rx = cx.prompt_for_new_path(Path::new("."), Some(&hint));
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    let write = cx
                        .background_executor()
                        .spawn(async move { std::fs::write(&path, &bytes) })
                        .await;
                    let mut cx = async_cx.clone();
                    weak.update(&mut cx, |this, cx| {
                        if let Err(err) = write {
                            this.banner_error = Some(err.to_string());
                        }
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = &mut self.model {
            model.reset();
            self.sync_field_inputs(window, cx);
            self.banner_error = None;
            self.banner_info = None;
            self.confirm_pending = false;
            self.delete_pending = false;
            self.fk_close_all();
            cx.notify();
        }
    }

    /// Re-fetches the row by key and adopts the server's current values.
    fn refresh_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &self.model else { return };
        if matches!(self.phase, Phase::Saving | Phase::Loading) {
            return;
        }
        let services = self.services.clone();
        let profile = self.profile.clone();
        let target = model.session.target.clone();
        let key = model.session.key_values();

        type FetchResult = Result<Option<(Vec<String>, Vec<CellValue>)>, StudioError>;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FetchResult>(1);
        cx.background_spawn(async move {
            let outcome = match crate::tokio_handle()
                .spawn(async move {
                    services
                        .row_editing
                        .fetch_row(&profile, &target, &key)
                        .await
                })
                .await
            {
                Ok(outcome) => outcome,
                Err(join_err) => Err(StudioError::Other(join_err.to_string())),
            };
            let _ = tx.send(outcome).await;
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            if let Some(outcome) = rx.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    match outcome {
                        Ok(Some((names, row))) => {
                            this.adopt_named_row(&names, row, window, cx);
                            this.banner_info = Some(crate::localization::text(
                                "results.row_editor.row_refreshed",
                            ));
                            this.banner_error = None;
                        }
                        Ok(None) => {
                            this.banner_error = Some(crate::localization::text(
                                "results.row_editor.row_no_longer_exists",
                            ));
                        }
                        Err(err) => {
                            this.banner_error = Some(crate::localization::message(err.message()))
                        }
                    }
                    this.confirm_pending = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Adopts a fetched row (`SELECT *` column order) into the model by
    /// matching column names against the session's catalog order.
    fn adopt_named_row(
        &mut self,
        names: &[String],
        row: Vec<CellValue>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = &mut self.model else { return };
        let aligned: Option<Vec<CellValue>> = model
            .session
            .columns
            .iter()
            .map(|column| {
                names
                    .iter()
                    .position(|name| name == &column.name)
                    .and_then(|at| row.get(at).cloned())
            })
            .collect();
        if let Some(aligned) = aligned {
            model.adopt_row(aligned);
        }
        self.sync_field_inputs(window, cx);
        self.fk_close_all();
        self.rebuild_related();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.phase, Phase::Saving | Phase::Loading) {
            return;
        }
        if self.intent.is_insert() {
            self.save_insert(cx);
        } else {
            self.save_update(window, cx);
        }
    }

    fn save_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = &mut self.model else { return };

        self.banner_error = None;
        self.banner_info = None;
        if !model.validate() {
            self.banner_error = Some(crate::localization::text(
                "results.row_editor.fix_highlighted_fields",
            ));
            cx.notify();
            return;
        }
        if !model.has_changes() {
            self.banner_info = Some(crate::localization::text(
                "results.row_editor.no_changes_to_save",
            ));
            cx.notify();
            return;
        }
        let Some(request) = model.build_update_request() else {
            cx.notify();
            return;
        };

        if self.mode == RowEditorMode::Stage {
            let session = model.session.clone();
            let key = crate::pending_changes::RowKey::new(
                session
                    .key_columns
                    .iter()
                    .filter_map(|name| {
                        session
                            .columns
                            .iter()
                            .position(|column| &column.name == name)
                            .map(|index| session.original_row[index].clone())
                    })
                    .collect(),
            );
            let mut changes = request
                .changes
                .into_iter()
                .map(|(name, value)| (name, CellEdit::Value(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            for (name, expr) in request.expressions {
                changes.insert(name, CellEdit::Expression(expr));
            }
            let original_row = session.original_row.clone();
            cx.emit(RowEditorEvent::Staged {
                session,
                op: StagedRowOp::Update {
                    key,
                    original_row,
                    changes,
                },
            });
            cx.emit(RowEditorEvent::Dismissed { saved: false });
            return;
        }

        // Row edits are mutations: honor the connection's confirmation
        // policy with an explicit two-step Save.
        let safety = SafetyFeatures::from_profile(&self.profile);
        if safety.use_confirmation_dialog && !self.confirm_pending {
            self.confirm_pending = true;
            self.banner_info = Some(crate::localization::format(
                if request.changes.len() == 1 {
                    "results.row_editor.confirm_update_one"
                } else {
                    "results.row_editor.confirm_update"
                },
                &[
                    ("change_count", &request.changes.len().to_string()),
                    ("table_name", &request.target.quoted()),
                ],
            ));
            cx.notify();
            return;
        }
        self.confirm_pending = false;
        self.phase = Phase::Saving;
        cx.notify();
        self.spawn_update(request, window, cx);
    }

    fn save_insert(&mut self, cx: &mut Context<Self>) {
        let Some(model) = &mut self.model else { return };
        self.banner_error = None;
        self.banner_info = None;
        if !model.validate() {
            self.banner_error = Some(crate::localization::text(
                "results.row_editor.required_fields_for_insert",
            ));
            cx.notify();
            return;
        }
        let Some(request) = model.build_insert_request() else {
            cx.notify();
            return;
        };
        if self.mode == RowEditorMode::Stage {
            let mut values: Vec<(String, CellEdit)> = request
                .values
                .into_iter()
                .map(|(name, value)| (name, CellEdit::Value(value)))
                .collect();
            values.extend(
                request
                    .expressions
                    .into_iter()
                    .map(|(name, expr)| (name, CellEdit::Expression(expr))),
            );
            cx.emit(RowEditorEvent::Staged {
                session: model.session.clone(),
                op: StagedRowOp::Insert { values },
            });
            cx.emit(RowEditorEvent::Dismissed { saved: false });
            return;
        }
        let target = request.target.quoted();

        let safety = SafetyFeatures::from_profile(&self.profile);
        if safety.use_confirmation_dialog && !self.confirm_pending {
            self.confirm_pending = true;
            self.banner_info = Some(crate::localization::format(
                "results.row_editor.confirm_insert",
                &[("table_name", &target)],
            ));
            cx.notify();
            return;
        }
        self.confirm_pending = false;
        self.phase = Phase::Saving;
        cx.notify();
        self.spawn_insert(request, cx);
    }

    fn spawn_insert(&mut self, request: InsertRowRequest, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let profile = self.profile.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<InsertRowOutcome, StudioError>>(1);
        cx.background_spawn(async move {
            let outcome = match crate::tokio_handle()
                .spawn(async move { services.row_editing.insert_row(&profile, request).await })
                .await
            {
                Ok(outcome) => outcome,
                Err(join_err) => Err(StudioError::Other(join_err.to_string())),
            };
            let _ = tx.send(outcome).await;
        })
        .detach();
        cx.spawn(async move |this, cx| {
            if let Some(outcome) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.apply_insert_outcome(outcome, cx);
                });
            }
        })
        .detach();
    }

    fn apply_insert_outcome(
        &mut self,
        outcome: Result<InsertRowOutcome, StudioError>,
        cx: &mut Context<Self>,
    ) {
        self.phase = Phase::Ready;
        match outcome {
            Ok(InsertRowOutcome::Inserted { .. }) => {
                // The grid refreshes on close; a fresh insert is done, so close.
                self.saved = true;
                self.dismiss(cx);
            }
            Err(err) => {
                let raw = err.to_string();
                let message = crate::localization::message(err.message());
                if let Some(model) = &mut self.model {
                    if model.map_db_error_to_fields(&raw) {
                        self.banner_error = Some(crate::localization::format(
                            "results.row_editor.server_rejected_insert",
                            &[("error", &message)],
                        ));
                    } else {
                        self.banner_error = Some(message);
                    }
                } else {
                    self.banner_error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// Scripts the pending edits as a literal-inlined UPDATE (wrapped in a
    /// transaction, same renderer as the staged-batch preview) into a fresh
    /// query tab. Nothing executes, and the editor stays open.
    fn script_update(&mut self, cx: &mut Context<Self>) {
        if self.intent.is_insert() || matches!(self.phase, Phase::Saving | Phase::Loading) {
            return;
        }
        let db = self.profile.database_type;
        let Some(model) = self.model.as_mut() else {
            return;
        };
        let Some(request) = model.build_update_request() else {
            // validate() recorded per-field errors — surface them.
            cx.notify();
            return;
        };
        let batch = sqlly_services::row_editing::StagedBatch {
            target: Some(request.target.clone()),
            key_columns: model.session.key_columns.clone(),
            ops: vec![sqlly_services::row_editing::StagedOp::Update(request)],
        };
        let sql = sqlly_services::row_editing::render_preview_script(db, &batch);
        cx.emit(RowEditorEvent::OpenQuery { sql });
    }

    /// Deletes the current row (edit mode). First click arms confirmation with
    /// a relationship-impact note; the second executes.
    fn delete(&mut self, cx: &mut Context<Self>) {
        if self.intent.is_insert() || matches!(self.phase, Phase::Saving | Phase::Loading) {
            return;
        }
        let Some(model) = &self.model else { return };
        let Some(request) = model.build_delete_request() else {
            self.banner_error = Some(crate::localization::text(
                "results.row_editor.delete_without_primary_key",
            ));
            cx.notify();
            return;
        };

        if self.mode == RowEditorMode::Stage {
            let session = model.session.clone();
            let key = crate::pending_changes::RowKey::new(
                session
                    .key_columns
                    .iter()
                    .filter_map(|name| {
                        session
                            .columns
                            .iter()
                            .position(|column| &column.name == name)
                            .map(|index| session.original_row[index].clone())
                    })
                    .collect(),
            );
            let original_row = session.original_row.clone();
            cx.emit(RowEditorEvent::Staged {
                session,
                op: StagedRowOp::Delete { key, original_row },
            });
            cx.emit(RowEditorEvent::Dismissed { saved: false });
            return;
        }
        let target = request.target.quoted();

        if !self.delete_pending {
            self.delete_pending = true;
            self.confirm_pending = false;
            self.banner_info = Some(if self.related.is_empty() {
                crate::localization::format(
                    "results.row_editor.confirm_delete",
                    &[("table_name", &target)],
                )
            } else {
                let tables: Vec<String> = self
                    .related
                    .iter()
                    .map(|child| child.table.quoted())
                    .collect();
                crate::localization::format(
                    "results.row_editor.confirm_delete_with_children",
                    &[
                        ("table_name", &target),
                        ("referencing_tables", &tables.join(", ")),
                    ],
                )
            });
            self.banner_error = None;
            cx.notify();
            return;
        }
        self.delete_pending = false;
        self.phase = Phase::Saving;
        cx.notify();
        self.spawn_delete(request, cx);
    }

    fn spawn_delete(&mut self, request: DeleteRowRequest, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let profile = self.profile.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<DeleteRowOutcome, StudioError>>(1);
        cx.background_spawn(async move {
            let outcome = match crate::tokio_handle()
                .spawn(async move { services.row_editing.delete_row(&profile, request).await })
                .await
            {
                Ok(outcome) => outcome,
                Err(join_err) => Err(StudioError::Other(join_err.to_string())),
            };
            let _ = tx.send(outcome).await;
        })
        .detach();
        cx.spawn(async move |this, cx| {
            if let Some(outcome) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.apply_delete_outcome(outcome, cx);
                });
            }
        })
        .detach();
    }

    fn apply_delete_outcome(
        &mut self,
        outcome: Result<DeleteRowOutcome, StudioError>,
        cx: &mut Context<Self>,
    ) {
        self.phase = Phase::Ready;
        match outcome {
            Ok(DeleteRowOutcome::Deleted) => {
                self.saved = true;
                self.dismiss(cx);
            }
            Ok(DeleteRowOutcome::Conflict { .. }) => {
                self.banner_error = Some(crate::localization::text(
                    "results.row_editor.delete_conflict_refresh",
                ));
                cx.notify();
            }
            Err(err) => {
                self.banner_error = Some(crate::localization::message(err.message()));
                cx.notify();
            }
        }
    }

    fn spawn_update(
        &mut self,
        request: UpdateRowRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let services = self.services.clone();
        let profile = self.profile.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<UpdateRowOutcome, StudioError>>(1);
        cx.background_spawn(async move {
            let outcome = match crate::tokio_handle()
                .spawn(async move { services.row_editing.update_row(&profile, request).await })
                .await
            {
                Ok(outcome) => outcome,
                Err(join_err) => Err(StudioError::Other(join_err.to_string())),
            };
            let _ = tx.send(outcome).await;
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            if let Some(outcome) = rx.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.apply_update_outcome(outcome, window, cx);
                });
            }
        })
        .detach();
    }

    fn apply_update_outcome(
        &mut self,
        outcome: Result<UpdateRowOutcome, StudioError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.phase = Phase::Ready;
        match outcome {
            Ok(UpdateRowOutcome::Updated { columns, row }) => {
                self.adopt_named_row(&columns, row, window, cx);
                self.saved = true;
                self.banner_info =
                    Some(crate::localization::text("results.row_editor.row_updated"));
            }
            Ok(UpdateRowOutcome::NoChanges) => {
                self.banner_info = Some(crate::localization::text(
                    "results.row_editor.no_changes_to_save",
                ));
            }
            Ok(UpdateRowOutcome::Conflict {
                current_columns,
                current_row,
            }) => match current_row {
                Some(row) => {
                    self.adopt_named_row(&current_columns, row, window, cx);
                    self.banner_error = Some(crate::localization::text(
                        "results.row_editor.update_conflict_current_values",
                    ));
                }
                None => {
                    self.banner_error = Some(crate::localization::text(
                        "results.row_editor.update_conflict_deleted",
                    ));
                }
            },
            Err(err) => {
                let raw = err.to_string();
                let message = crate::localization::message(err.message());
                // Attribute a constraint violation to the offending field(s)
                // when we recognise the constraint.
                if let Some(model) = &mut self.model {
                    if model.map_db_error_to_fields(&raw) {
                        self.banner_error = Some(crate::localization::format(
                            "results.row_editor.server_rejected_change",
                            &[("error", &message)],
                        ));
                    } else {
                        self.banner_error = Some(message);
                    }
                } else {
                    self.banner_error = Some(message);
                }
            }
        }
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An open FK dropdown captures navigation keys first.
        if let Some(index) = self.open_fk_field() {
            match event.keystroke.key.as_str() {
                "down" => {
                    if let Some(state) = self.fk_states.get_mut(&index) {
                        state.move_highlight(1);
                        cx.notify();
                    }
                    return;
                }
                "up" => {
                    if let Some(state) = self.fk_states.get_mut(&index) {
                        state.move_highlight(-1);
                        cx.notify();
                    }
                    return;
                }
                "enter" if !event.keystroke.modifiers.platform => {
                    let highlight = self.fk_states.get(&index).and_then(|state| state.highlight);
                    if let Some(highlight) = highlight {
                        self.fk_pick(index, highlight, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    // Consume the key so the dialog chrome's escape-to-close
                    // handler doesn't also dismiss the dialog.
                    self.fk_close(index, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        // Escape-to-close is handled by the dialog chrome; only the
        // save shortcut remains here.
        if event.keystroke.key.as_str() == "enter" && event.keystroke.modifiers.platform {
            self.save(window, cx);
        }
    }

    /// The form-field index of the single open FK dropdown, if any.
    fn open_fk_field(&self) -> Option<usize> {
        self.fk_states
            .iter()
            .find(|(_, state)| state.open && !state.candidates.is_empty())
            .map(|(index, _)| *index)
    }

    /// Renders the read-only related-child-rows panel, or `None` when the table
    /// has no incoming foreign keys.
    fn render_related_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.related.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        let count_label = crate::localization::format(
            if self.related.len() == 1 {
                "results.row_editor.relationship_count_one"
            } else {
                "results.row_editor.relationship_count"
            },
            &[("relationship_count", &self.related.len().to_string())],
        );
        let mut panel = div().flex().flex_col().gap(px(s(4.0))).p(px(s(12.0)));
        for (index, child) in self.related.iter().enumerate() {
            panel = panel.child(self.render_related_child(index, child, cx));
        }
        Some(
            div()
                .id("row-editor-related")
                .size_full()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .justify_between()
                        .gap(px(s(8.0)))
                        .px(px(s(12.0)))
                        .pt(px(s(10.0)))
                        .pb(px(s(6.0)))
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .child(crate::localization::text(
                                    "results.row_editor.related_rows_title",
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(count_label),
                        ),
                )
                .child(
                    div()
                        .id("row-editor-related-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(panel),
                )
                .into_any_element(),
        )
    }

    fn render_related_child(
        &self,
        index: usize,
        child: &RelatedChild,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let caret = crate::icons::disclosure_chevron(child.expanded);
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .id(("related-toggle", index))
                    .flex_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .cursor_pointer()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .hover(|s| s.text_color(Theme::accent()))
                    .on_click(cx.listener(move |this, _, _, cx| this.related_toggle(index, cx)))
                    .child(div().text_color(Theme::text_dim()).child(caret))
                    .child(child.heading.clone()),
            )
            .child(
                div()
                    .id(("related-open", index))
                    .px(px(s(8.0)))
                    .py(px(s(2.0)))
                    .rounded(px(s(2.0)))
                    .bg(Theme::panel_bg())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::accent()))
                    .on_click(cx.listener(move |this, _, _, cx| this.related_open_query(index, cx)))
                    .child(crate::localization::text(
                        "results.row_editor.open_related_query",
                    )),
            );

        let mut container = div()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .py(px(s(2.0)))
            .child(header);

        if !child.resolvable {
            container = container.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.row_editor.related_null_key",
                    )),
            );
            return container.into_any_element();
        }

        if child.expanded {
            let body: gpui::AnyElement = if child.loading {
                div()
                    .px(px(s(6.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.row_editor.related_loading",
                    ))
                    .into_any_element()
            } else if let Some(error) = &child.error {
                div()
                    .px(px(s(6.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::error())
                    .child(error.clone())
                    .into_any_element()
            } else if let Some(result) = &child.result {
                self.render_related_table(result)
            } else {
                div().into_any_element()
            };
            container = container.child(body);
        }
        container.into_any_element()
    }

    fn render_related_table(&self, result: &RelatedRowsResult) -> gpui::AnyElement {
        let s = Theme::scaled;
        if result.rows.is_empty() {
            return div()
                .px(px(s(6.0)))
                .py(px(s(4.0)))
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.row_editor.no_related_rows",
                ))
                .into_any_element();
        }
        let header =
            result
                .columns
                .iter()
                .fold(div().flex().flex_row().gap(px(s(10.0))), |row, name| {
                    row.child(
                        div()
                            .min_w(px(s(80.0)))
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(Theme::text_dim())
                            .child(name.clone()),
                    )
                });
        let mut table = div()
            .flex()
            .flex_col()
            .gap(px(s(1.0)))
            .p(px(s(6.0)))
            .child(header);
        for row in &result.rows {
            let line = row
                .iter()
                .fold(div().flex().flex_row().gap(px(s(10.0))), |line, value| {
                    line.child(
                        div()
                            .min_w(px(s(80.0)))
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text())
                            .child(sqlly_services::row_editing::render_cell(value)),
                    )
                });
            table = table.child(line);
        }
        if result.truncated {
            table = table.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "results.row_editor.related_truncated",
                        &[("row_count", &result.rows.len().to_string())],
                    )),
            );
        }
        div()
            .id("related-table-scroll")
            .overflow_x_scroll()
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(3.0)))
            .child(table)
            .into_any_element()
    }

    /// Renders the input for an FK-lead field: the text box (raw key), the
    /// resolved display label beside it, and the searchable candidate dropdown
    /// when open.
    fn render_fk_control(
        &self,
        index: usize,
        label: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let input: gpui::AnyElement = self
            .field_inputs
            .get(index)
            .and_then(|state| state.as_ref())
            .map(|state| Input::new(state).into_any_element())
            .unwrap_or_else(|| div().into_any_element());
        let mut value_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .child(div().flex_1().child(input));
        if let Some(label) = label {
            value_row = value_row.child(
                div()
                    .px(px(s(6.0)))
                    .py(px(s(2.0)))
                    .rounded(px(s(2.0)))
                    .bg(Theme::panel_bg())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "results.row_editor.foreign_key_selected_label",
                        &[("label", &label)],
                    )),
            );
        }

        let dropdown = self
            .fk_states
            .get(&index)
            .filter(|state| state.open)
            .map(|state| self.render_fk_dropdown(index, state, cx));

        div()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .child(value_row)
            .children(dropdown)
            .into_any_element()
    }

    /// Renders the candidate list (or loading/empty/error state) below an FK
    /// field's input.
    fn render_fk_dropdown(
        &self,
        index: usize,
        state: &FkFieldState,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let mut list = div()
            .id(("fk-dropdown", index))
            .max_h(px(s(168.0)))
            .overflow_y_scroll()
            .border_1()
            .border_color(Theme::popup_border())
            .rounded(px(s(3.0)))
            .bg(Theme::popup_bg())
            .flex()
            .flex_col();

        if state.loading {
            list = list.child(
                div()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("results.row_editor.fk_searching")),
            );
        } else if let Some(error) = &state.error {
            list = list.child(
                div()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        } else if state.candidates.is_empty() {
            list = list.child(
                div()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.row_editor.fk_no_matches",
                    )),
            );
        } else {
            for (candidate_index, candidate) in state.candidates.iter().enumerate() {
                let entity = cx.entity();
                let highlighted = state.highlight == Some(candidate_index);
                let key_hint = candidate
                    .key
                    .iter()
                    .map(sqlly_services::row_editing::render_cell)
                    .collect::<Vec<_>>()
                    .join(", ");
                list = list.child(
                    div()
                        .id((
                            "fk-candidate",
                            index * (FK_SEARCH_LIMIT + 2) + candidate_index,
                        ))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(s(8.0)))
                        .px(px(s(8.0)))
                        .py(px(s(4.0)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .cursor_pointer()
                        .when(highlighted, |d| d.bg(Theme::completion_selected()))
                        .hover(|s| s.bg(Theme::completion_selected()))
                        .on_mouse_down(gpui::MouseButton::Left, move |_event, window, app| {
                            entity.update(app, |this, cx| {
                                this.fk_pick(index, candidate_index, window, cx);
                            });
                        })
                        .child(div().child(candidate.label.clone()))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(key_hint),
                        ),
                );
            }
            if state.truncated {
                list = list.child(
                    div()
                        .px(px(s(8.0)))
                        .py(px(s(4.0)))
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "results.row_editor.fk_matches_truncated",
                        )),
                );
            }
        }

        list.into_any_element()
    }

    fn render_field_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let s = Theme::scaled;
        let Some(model) = &self.model else {
            return Vec::new();
        };
        let saving = matches!(self.phase, Phase::Saving);
        model
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let editable = field.is_editable() && !saving;
                let is_null = field.draft == FieldDraft::Null;
                let dirty = field.is_editable() && field.is_dirty();

                let mut label_line = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(
                        div()
                            .font(Theme::ui_font())
                            .text_size(px(Theme::font_size()))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(Theme::text())
                            .child(field.column.name.clone()),
                    );
                if dirty {
                    label_line = label_line.child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::warning())
                            .child("●"),
                    );
                }
                if let Some(reason) = field.read_only_reason() {
                    label_line = label_line.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .px(px(s(4.0)))
                            .rounded(px(s(2.0)))
                            .bg(Theme::panel_bg())
                            .text_color(Theme::text_dim())
                            .child(reason),
                    );
                }
                let meta_line = div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(field.type_summary());
                let label_column = div()
                    .w(px(s(190.0)))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .child(label_line)
                    .child(meta_line);

                let actions = actions_for(
                    self.profile.database_type,
                    &field.column,
                    field.is_editable(),
                );
                let value_text = Self::draft_text(field);
                let display_text = if is_null {
                    "NULL".to_string()
                } else {
                    value_text.clone()
                };
                let show_null_placeholder = is_null
                    && !actions.bool_choices
                    && actions.enum_choices.is_empty()
                    && field.kind != FieldKind::Binary;
                let control: gpui::AnyElement = if !editable {
                    div()
                        .flex_1()
                        .px(px(s(8.0)))
                        .py(px(s(4.0)))
                        .rounded(px(s(3.0)))
                        .bg(Theme::input_bg())
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(display_text)
                        .into_any_element()
                } else if actions.bool_choices {
                    // Bools edit as a checkbox; NULL still comes from the NULL
                    // chip, and checking from NULL lands on true/false.
                    let checked =
                        value_text.trim().eq_ignore_ascii_case("true") || value_text.trim() == "1";
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .child(
                            Checkbox::new(("row-edit-bool", index))
                                .checked(checked)
                                .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                                    let value = if *checked { "true" } else { "false" };
                                    this.apply_select_choice(
                                        index,
                                        Some(value.to_string()),
                                        window,
                                        cx,
                                    );
                                })),
                        )
                        .into_any_element()
                } else if field.kind == FieldKind::Binary {
                    div()
                        .flex_1()
                        .px(px(s(8.0)))
                        .py(px(s(4.0)))
                        .rounded(px(s(3.0)))
                        .border_1()
                        .border_color(Theme::border())
                        .bg(Theme::input_bg())
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size()))
                        .italic()
                        .text_color(Theme::text_dim())
                        .child(display_text)
                        .into_any_element()
                } else if show_null_placeholder {
                    div()
                        .flex_1()
                        .px(px(s(8.0)))
                        .py(px(s(4.0)))
                        .rounded(px(s(3.0)))
                        .border_1()
                        .border_color(Theme::border())
                        .bg(Theme::input_bg())
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size()))
                        .italic()
                        .text_color(Theme::text_dim())
                        .child("NULL")
                        .into_any_element()
                } else if field.fk_lead {
                    self.render_fk_control(index, field.fk_label.clone(), cx)
                } else if field.kind == FieldKind::Date
                    && let Some(Some(picker)) = self.field_dates.get(index)
                {
                    div()
                        .flex_1()
                        .child(DatePicker::new(picker))
                        .into_any_element()
                } else {
                    let input = self
                        .field_inputs
                        .get(index)
                        .and_then(|state| state.as_ref())
                        .map(|state| {
                            div()
                                .flex_1()
                                .child(Input::new(state).font_family(Theme::editor_font_family()))
                                .into_any_element()
                        })
                        .unwrap_or_else(|| div().flex_1().into_any_element());
                    if actions.enum_choices.is_empty() {
                        input
                    } else {
                        let select = self
                            .field_selects
                            .get(index)
                            .and_then(|state| state.as_ref())
                            .map(|state| Select::new(state).into_any_element())
                            .unwrap_or_else(|| div().into_any_element());
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(s(4.0)))
                            .child(select)
                            .child(input)
                            .into_any_element()
                    }
                };

                let mut action_row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(control);
                if actions.set_null && field.is_editable() {
                    action_row = action_row.child(
                        div()
                            .id(("row-edit-null", index))
                            .px(px(s(6.0)))
                            .py(px(s(2.0)))
                            .rounded(px(s(2.0)))
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .bg(if is_null {
                                Theme::accent()
                            } else {
                                Theme::panel_bg()
                            })
                            .text_color(Theme::text())
                            .cursor_pointer()
                            .hover(|s| s.bg(Theme::accent()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_null(index, window, cx);
                            }))
                            .child("NULL"),
                    );
                }
                if actions.set_default && editable {
                    action_row = action_row.child(
                        Button::new(("row-edit-default", index))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "results.row_editor.default_button",
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.apply_expression(
                                    index,
                                    sqlly_services::row_editing::ColumnExpression::Default,
                                    window,
                                    cx,
                                );
                            })),
                    );
                }
                if actions.server_time && editable {
                    if matches!(field.kind, FieldKind::Date) {
                        action_row = action_row.child(
                            Button::new(("row-edit-today", index))
                                .ghost()
                                .xsmall()
                                .label(crate::localization::text("results.row_editor.today_button"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.apply_quick_pick(index, window, cx);
                                })),
                        );
                    } else {
                        action_row = action_row.child(
                            Button::new(("row-edit-now", index))
                                .ghost()
                                .xsmall()
                                .label(crate::localization::text("results.row_editor.now_button"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.apply_quick_pick(index, window, cx);
                                })),
                        );
                    }
                    action_row = action_row.child(
                        Button::new(("row-edit-server-time", index))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "results.row_editor.server_time_button",
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.apply_expression(
                                    index,
                                    sqlly_services::row_editing::ColumnExpression::ServerTime,
                                    window,
                                    cx,
                                );
                            })),
                    );
                }
                // Generator inserts: a client-side random UUID plus the
                // engine-evaluated SQL functions the column accepts.
                let offers_uuid = actions.new_uuid && editable && !field.fk_lead;
                let sql_functions: Vec<_> = if editable {
                    actions.sql_functions.clone()
                } else {
                    Vec::new()
                };
                // On a relay connection whose bridge is too old for them, the
                // server-evaluated functions stay listed but disabled, and the
                // tooltip says why.
                let via_relay = self.server_expressions_blocked();
                let relay_blocked = via_relay
                    && sql_functions
                        .iter()
                        .any(|expr| !cell_actions::expression_allowed_over_relay(*expr));
                if offers_uuid || !sql_functions.is_empty() {
                    let this = cx.entity();
                    let db = self.profile.database_type;
                    let tooltip = if relay_blocked {
                        crate::localization::format(
                            "results.row_editor.insert_generated_relay_blocked_tooltip",
                            &[(
                                "reason",
                                &cell_actions::localized_relay_sql_functions_reason(),
                            )],
                        )
                    } else {
                        crate::localization::text("results.row_editor.insert_generated_tooltip")
                    };
                    action_row = action_row.child(
                        Button::new(("row-edit-insert", index))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "results.row_editor.insert_button",
                            ))
                            .app_tooltip_text(tooltip)
                            .dropdown_menu(move |menu, _, _| {
                                let mut menu = menu;
                                if offers_uuid {
                                    let this = this.clone();
                                    menu =
                                        menu.item(
                                            PopupMenuItem::new(crate::localization::text(
                                                "results.row_editor.new_uuid_menu_item",
                                            ))
                                            .on_click(move |_ev, window, app| {
                                                this.update(app, |this, cx| {
                                                    this.generate_uuid(index, window, cx);
                                                });
                                            }),
                                        );
                                }
                                for expr in sql_functions.iter().copied() {
                                    let this = this.clone();
                                    let blocked = via_relay
                                        && !cell_actions::expression_allowed_over_relay(expr);
                                    let label = if blocked {
                                        crate::localization::format(
                                            "results.row_editor.sql_function_needs_newer_bridge",
                                            &[(
                                                "function_label",
                                                &cell_actions::sql_function_menu_label(db, expr),
                                            )],
                                        )
                                    } else {
                                        cell_actions::sql_function_menu_label(db, expr)
                                    };
                                    menu = menu.item(
                                        PopupMenuItem::new(label).disabled(blocked).on_click(
                                            move |_ev, window, app| {
                                                this.update(app, |this, cx| {
                                                    this.apply_expression(index, expr, window, cx);
                                                });
                                            },
                                        ),
                                    );
                                }
                                menu
                            }),
                    );
                }
                // Transform macros for free-text fields, applied to the draft.
                if Self::field_offers_transforms(field, &actions, editable) {
                    let has_text = matches!(&field.draft, FieldDraft::Text(t) if !t.is_empty());
                    let this = cx.entity();
                    action_row = action_row.child(
                        Button::new(("row-edit-transform", index))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "results.row_editor.transform_button",
                            ))
                            .disabled(!has_text)
                            .app_tooltip_text(crate::localization::text(
                                "results.row_editor.transform_tooltip",
                            ))
                            .dropdown_menu(move |menu, _, _| {
                                let mut menu = menu;
                                for transform in crate::cell_transform::CellTransform::all() {
                                    let transform = *transform;
                                    let this = this.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(transform.localized_label()).on_click(
                                            move |_ev, window, app| {
                                                this.update(app, |this, cx| {
                                                    this.apply_transform(
                                                        index, transform, window, cx,
                                                    );
                                                });
                                            },
                                        ),
                                    );
                                }
                                menu
                            }),
                    );
                }
                if actions.binary && editable {
                    action_row = action_row.child(
                        Button::new(("row-edit-load-file", index))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "results.row_editor.load_file_button",
                            ))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.load_binary_from_file(index, cx);
                            })),
                    );
                    if matches!(field.original, CellValue::Bytes(_)) {
                        let column_name = field.column.name.clone();
                        action_row = action_row.child(
                            Button::new(("row-edit-save-file", index))
                                .ghost()
                                .xsmall()
                                .label(crate::localization::text(
                                    "results.row_editor.save_file_button",
                                ))
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.save_binary_to_file(index, column_name.clone(), cx);
                                })),
                        );
                    }
                }

                let mut row = div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(s(8.0)))
                    .child(label_column)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(s(2.0)))
                            .child(action_row),
                    );
                if let Some(error) = &field.error {
                    row = row.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::error())
                            .child(error.clone()),
                    );
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .pb(px(s(6.0)))
                    .child(row)
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for RowEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let saving = matches!(self.phase, Phase::Saving);
        let has_changes = self
            .model
            .as_ref()
            .map(|model| model.has_changes())
            .unwrap_or(false);

        let verb = crate::localization::text(match self.intent {
            RowEditorIntent::Edit => "results.row_editor.edit_title",
            RowEditorIntent::Insert => "results.row_editor.insert_title",
            RowEditorIntent::Duplicate => "results.row_editor.duplicate_title",
            RowEditorIntent::DuplicateWithoutKey => {
                "results.row_editor.duplicate_without_key_title"
            }
            RowEditorIntent::BatchInsert => "results.row_editor.batch_insert_title",
        });
        let title_text = match (&self.phase, &self.model) {
            (Phase::Loading, _) => verb.clone(),
            (Phase::NotEditable { .. }, _) => {
                crate::localization::text("results.row_editor.not_editable_title")
            }
            (_, Some(model)) => crate::localization::format(
                "results.row_editor.table_title",
                &[
                    ("action_name", &verb),
                    ("table_name", &model.session.target.quoted()),
                ],
            ),
            _ => verb,
        };
        let field_body: gpui::AnyElement = match &self.phase {
            Phase::Loading => div()
                .p(px(s(24.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("results.row_editor.loading_row"))
                .into_any_element(),
            Phase::NotEditable { reason } => div()
                .p(px(s(16.0)))
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .text_color(Theme::text())
                        .child(crate::localization::text(
                            "results.row_editor.not_editable_explanation",
                        )),
                )
                .child(div().text_color(Theme::text_dim()).child(reason.clone()))
                .into_any_element(),
            Phase::Ready | Phase::Saving => div()
                .id("row-editor-fields")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .px(px(s(16.0)))
                .pb(px(s(10.0)))
                .children(self.render_field_rows(cx))
                .into_any_element(),
        };

        let related_panel = match self.phase {
            Phase::Ready | Phase::Saving => self.render_related_panel(cx),
            _ => None,
        };

        let field_heading = self.model.as_ref().map(|model| {
            let changed = model
                .fields
                .iter()
                .filter(|field| field.is_editable() && field.is_dirty())
                .count();
            let column_count = model.fields.len().to_string();
            let summary = if changed == 0 {
                crate::localization::format(
                    "results.row_editor.field_count",
                    &[("column_count", &column_count)],
                )
            } else {
                crate::localization::format(
                    if changed == 1 {
                        "results.row_editor.field_change_count_one"
                    } else {
                        "results.row_editor.field_change_count"
                    },
                    &[
                        ("column_count", &column_count),
                        ("change_count", &changed.to_string()),
                    ],
                )
            };
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_baseline()
                .justify_between()
                .gap(px(s(8.0)))
                .px(px(s(16.0)))
                .pt(px(s(10.0)))
                .pb(px(s(6.0)))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(crate::localization::text(
                            "results.row_editor.row_values_title",
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(summary),
                )
                .into_any_element()
        });

        let mut banners = div().flex().flex_col().gap(px(s(4.0)));
        if let Some(error) = &self.banner_error {
            banners = banners.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }
        if let Some(info) = &self.banner_info {
            banners = banners.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::info())
                    .child(info.clone()),
            );
        }

        let editable = matches!(self.phase, Phase::Ready) && self.model.is_some();
        let is_insert = self.intent.is_insert();
        let save_label = if saving {
            crate::localization::text(match self.intent {
                RowEditorIntent::Edit => "results.row_editor.saving_update",
                _ => "results.row_editor.saving_insert",
            })
        } else if self.confirm_pending {
            crate::localization::text(if is_insert {
                "results.row_editor.confirm_insert_button"
            } else {
                "results.row_editor.confirm_update_button"
            })
        } else if self.mode == RowEditorMode::Stage {
            crate::localization::text(if self.intent == RowEditorIntent::Edit {
                "results.row_editor.stage_update_button"
            } else {
                "results.row_editor.stage_insert_button"
            })
        } else if is_insert {
            crate::localization::text("results.row_editor.insert_save_button")
        } else {
            crate::localization::text("results.row_editor.save_button")
        };
        let mut leading_buttons: Vec<gpui::AnyElement> = Vec::new();
        if (editable || saving) && !is_insert {
            // Insert forms have no persisted row: no Reset/Refresh/Delete.
            leading_buttons.push(
                Button::new("row-edit-reset")
                    .label(crate::localization::text("results.row_editor.reset_button"))
                    .disabled(!(editable && has_changes))
                    .on_click(cx.listener(|this, _, window, cx| this.reset(window, cx)))
                    .into_any_element(),
            );
            leading_buttons.push(
                Button::new("row-edit-refresh")
                    .label(crate::localization::text(
                        "results.row_editor.refresh_button",
                    ))
                    .disabled(!editable)
                    .on_click(cx.listener(|this, _, window, cx| this.refresh_row(window, cx)))
                    .into_any_element(),
            );
            // Armed deletion goes bright red so the destructive second click
            // is unmistakable.
            let mut delete_button = Button::new("row-edit-delete")
                .label(crate::localization::text(if self.delete_pending {
                    "results.row_editor.confirm_delete_button"
                } else if self.mode == RowEditorMode::Stage {
                    "results.row_editor.stage_delete_button"
                } else {
                    "results.row_editor.delete_button"
                }))
                .disabled(!editable);
            if self.delete_pending {
                delete_button = delete_button.danger();
            }
            leading_buttons.push(
                delete_button
                    .on_click(cx.listener(|this, _, _, cx| this.delete(cx)))
                    .into_any_element(),
            );
            leading_buttons.push(
                Button::new("row-edit-script-update")
                    .label(crate::localization::text(
                        "results.row_editor.script_update_button",
                    ))
                    .disabled(!(editable && has_changes))
                    .on_click(cx.listener(|this, _, _, cx| this.script_update(cx)))
                    .into_any_element(),
            );
        }
        let save_button = (editable || saving).then(|| {
            // Insert always allows a submit (blank insert uses DEFAULT VALUES);
            // edit requires pending changes.
            let save_enabled = editable && (is_insert || has_changes);
            Button::new("row-edit-save")
                .label(save_label)
                .primary()
                .disabled(!save_enabled)
                .when(save_enabled, |d| {
                    d.on_click(cx.listener(|this, _, window, cx| this.save(window, cx)))
                })
        });

        let mut work_areas = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .children(field_heading);
        if let Some(related_panel) = related_panel {
            let this = cx.entity();
            let divider =
                PaneDivider::horizontal("row-editor-related-divider", CollapseDirection::Down)
                    .on_drag_start(move |pointer_y, _window, app| {
                        this.update(app, |this, cx| {
                            this.start_related_panel_resize(pointer_y);
                            cx.notify();
                        });
                    });
            work_areas = work_areas.child(field_body).child(divider).child(
                div()
                    .h(px(self.related_panel_height))
                    .min_h(px(MIN_RELATED_PANEL_HEIGHT))
                    .max_h(px(MAX_RELATED_PANEL_HEIGHT))
                    .w_full()
                    .flex_shrink_0()
                    .child(related_panel),
            );
        } else {
            work_areas = work_areas.child(field_body);
        }

        // The two scrolling work areas share the available body height. The
        // divider is parent-tracked so the resize remains responsive in both
        // the modal and detached window.
        let content = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    this.update_related_panel_resize(f32::from(event.position.y), cx);
                } else {
                    this.end_related_panel_resize(cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    this.end_related_panel_resize(cx);
                }),
            )
            .child(work_areas)
            .when(
                self.banner_error.is_some() || self.banner_info.is_some(),
                |content| {
                    content
                        .flex_none()
                        .px(px(s(16.0)))
                        .pb(px(s(10.0)))
                        .child(banners)
                },
            );

        // Easy close (Escape / click outside) applies only while the form has
        // no pending edits; the chrome suspends it once the frame is dirty.
        self.frame.set_dirty(has_changes);
        let mut dialog = Dialog::new("row-editor", |this: &mut Self| &mut this.frame)
            .title(title_text)
            .easy_close(true)
            .bare_body()
            // Popped out, the OS window IS the dialog: content fills it and
            // the window's own edges resize it — no inner card chrome.
            .windowed(self.presentation == RowEditorPresentation::Window)
            // The old Close button was disabled while saving: every close
            // path (✕, Close, scrim, Escape) is inert until the save settles.
            .on_close(|this, window, cx| {
                if !matches!(this.phase, Phase::Saving) {
                    this.close(window, cx);
                }
            })
            .child(content);
        if self.presentation == RowEditorPresentation::Modal {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        for button in leading_buttons {
            dialog = dialog.leading_button(button);
        }
        if let Some(save_button) = save_button {
            dialog = dialog.button(save_button);
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use sqlly_core::{ResultColumn, SecretProvider, SecretRef};
    use sqlly_services::row_editing::{
        DeleteRowOutcome, FkSearchResult, InsertRowOutcome, InsertRowRequest, RelatedRowsRequest,
        RelatedRowsResult, RowEditColumn, RowEditOpenOutcome, RowEditSession, RowEditingService,
        TableTarget, UpdateRowOutcome,
    };

    use super::*;

    /// gpui-component widgets read the library's global theme during render;
    /// windowed tests must install it (idempotently) before opening windows.
    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn prepare_row_editor_windowed_test(cx: &mut gpui::TestAppContext) {
        crate::ensure_test_tokio_runtime();
        ensure_components_initialized(cx);
        // open_with and save spawn on the real tokio runtime; allow late wakes
        // from worker threads without tripping the test scheduler.
        cx.dispatcher.allow_parking();
    }

    fn wait_until_session_ready(view: &gpui::Entity<RowEditor>, cx: &mut gpui::VisualTestContext) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if view.read_with(cx, |editor, _| editor.model.is_some()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("row editor session never resolved");
    }

    fn wait_until_update_settled(view: &gpui::Entity<RowEditor>, cx: &mut gpui::VisualTestContext) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if view.read_with(cx, |editor, _| !matches!(editor.phase, Phase::Saving)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("row editor save never settled");
    }

    fn test_profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".to_string(),
            },
        )
    }

    fn edit_column(name: &str, data_type: &str) -> RowEditColumn {
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

    fn person_session() -> RowEditSession {
        let mut id = edit_column("Id", "int");
        id.is_primary_key = true;
        id.is_identity = true;
        id.nullable = false;
        let name = edit_column("Name", "nvarchar");
        RowEditSession {
            target: TableTarget {
                schema: "dbo".into(),
                table: "Person".into(),
            },
            key_columns: vec!["Id".into()],
            rowversion_column: None,
            columns: vec![id, name],
            original_row: vec![CellValue::I64(1), CellValue::String("Ada".into())],
            foreign_keys: Vec::new(),
            fk_display: Vec::new(),
            check_constraints: Vec::new(),
            incoming_foreign_keys: Vec::new(),
        }
    }

    fn open_request() -> RowEditOpenRequest {
        RowEditOpenRequest {
            sql: "SELECT Id, Name FROM dbo.Person".into(),
            result_set_index: 0,
            columns: vec![
                ResultColumn {
                    name: "Id".into(),
                    data_type: "int".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "Name".into(),
                    data_type: "nvarchar".into(),
                    nullable: true,
                },
            ],
            row: vec![CellValue::I64(1), CellValue::String("Ada".into())],
        }
    }

    #[derive(Clone)]
    struct StubRowEditingService {
        session: RowEditSession,
        update_outcome: UpdateRowOutcome,
        last_update: Arc<Mutex<Option<UpdateRowRequest>>>,
    }

    impl StubRowEditingService {
        fn services(self) -> Services {
            let real = Services::new();
            Services::from_parts(
                real.connections.clone(),
                real.intellisense.clone(),
                real.queries.clone(),
                real.catalog.clone(),
                real.metadata.clone(),
                real.admin_tools.clone(),
                Arc::new(self),
            )
        }
    }

    #[async_trait::async_trait]
    impl RowEditingService for StubRowEditingService {
        async fn open_session(
            &self,
            _: &ConnectionProfile,
            _: RowEditOpenRequest,
        ) -> sqlly_core::StudioResult<RowEditOpenOutcome> {
            Ok(RowEditOpenOutcome::Editable(Box::new(self.session.clone())))
        }

        async fn open_insert_session(
            &self,
            _: &ConnectionProfile,
            _: RowEditOpenRequest,
        ) -> sqlly_core::StudioResult<RowEditOpenOutcome> {
            Ok(RowEditOpenOutcome::Editable(Box::new(self.session.clone())))
        }

        async fn fetch_row(
            &self,
            _: &ConnectionProfile,
            _: &TableTarget,
            _: &[(String, CellValue)],
        ) -> sqlly_core::StudioResult<Option<(Vec<String>, Vec<CellValue>)>> {
            Ok(None)
        }

        async fn update_row(
            &self,
            _: &ConnectionProfile,
            request: UpdateRowRequest,
        ) -> sqlly_core::StudioResult<UpdateRowOutcome> {
            *self.last_update.lock().unwrap() = Some(request);
            Ok(self.update_outcome.clone())
        }

        async fn fk_search(
            &self,
            _: &ConnectionProfile,
            _: sqlly_services::row_editing::FkSearchRequest,
        ) -> sqlly_core::StudioResult<FkSearchResult> {
            Ok(FkSearchResult {
                candidates: Vec::new(),
                truncated: false,
            })
        }

        async fn fk_labels(
            &self,
            _: &ConnectionProfile,
            _: sqlly_services::row_editing::FkLabelRequest,
        ) -> sqlly_core::StudioResult<sqlly_services::row_editing::FkLabelResult> {
            Ok(sqlly_services::row_editing::FkLabelResult::default())
        }

        async fn related_rows(
            &self,
            _: &ConnectionProfile,
            _: RelatedRowsRequest,
        ) -> sqlly_core::StudioResult<RelatedRowsResult> {
            Ok(RelatedRowsResult {
                columns: Vec::new(),
                rows: Vec::new(),
                truncated: false,
                open_sql: String::new(),
            })
        }

        async fn insert_row(
            &self,
            _: &ConnectionProfile,
            _: InsertRowRequest,
        ) -> sqlly_core::StudioResult<InsertRowOutcome> {
            Err(StudioError::Other("insert not stubbed".into()))
        }

        async fn delete_row(
            &self,
            _: &ConnectionProfile,
            _: DeleteRowRequest,
        ) -> sqlly_core::StudioResult<DeleteRowOutcome> {
            Err(StudioError::Other("delete not stubbed".into()))
        }

        async fn apply_batch(
            &self,
            _: &ConnectionProfile,
            _: sqlly_services::row_editing::BatchRequest,
        ) -> sqlly_core::StudioResult<sqlly_services::row_editing::BatchOutcome> {
            Err(StudioError::Other("not supported".into()))
        }
    }

    fn name_field_index(editor: &RowEditor) -> usize {
        editor
            .model
            .as_ref()
            .expect("session resolved")
            .fields
            .iter()
            .position(|field| field.column.name == "Name")
            .expect("Name column in session")
    }

    #[test]
    fn related_panel_drag_resizes_and_stays_within_bounds() {
        let resize = RelatedPanelResize {
            start_y: 300.0,
            start_height: 190.0,
        };

        assert_eq!(related_panel_height_for_drag(resize, 250.0), 240.0);
        assert_eq!(
            related_panel_height_for_drag(resize, -1000.0),
            MAX_RELATED_PANEL_HEIGHT
        );
        assert_eq!(
            related_panel_height_for_drag(resize, 1000.0),
            MIN_RELATED_PANEL_HEIGHT
        );
    }

    #[test]
    fn row_editor_event_reports_saved_flag() {
        // The host refreshes the grid only when a save was committed; keep
        // the event payload honest.
        let dismissed = RowEditorEvent::Dismissed { saved: true };
        match dismissed {
            RowEditorEvent::Dismissed { saved } => assert!(saved),
            RowEditorEvent::OpenQuery { .. }
            | RowEditorEvent::Staged { .. }
            | RowEditorEvent::PopOut(_) => unreachable!(),
        }
    }

    #[gpui::test]
    fn row_editor_save_commits_update_and_reports_saved_on_close(cx: &mut gpui::TestAppContext) {
        let session = person_session();
        let last_update = Arc::new(Mutex::new(None));
        let stub = StubRowEditingService {
            session,
            update_outcome: UpdateRowOutcome::Updated {
                columns: vec!["Id".into(), "Name".into()],
                row: vec![CellValue::I64(1), CellValue::String("Bob".into())],
            },
            last_update: last_update.clone(),
        };
        let services = stub.services();
        prepare_row_editor_windowed_test(cx);
        let profile = test_profile();
        let request = open_request();
        let (view, cx) = cx.add_window_view(|window, cx| {
            RowEditor::open_with(
                services,
                profile,
                request,
                RowEditorIntent::Edit,
                RowEditorMode::Commit,
                window,
                cx,
            )
        });
        wait_until_session_ready(&view, cx);

        let dismissed: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let seen = dismissed.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &RowEditorEvent, _cx| {
                if let RowEditorEvent::Dismissed { saved } = event {
                    *seen.borrow_mut() = Some(*saved);
                }
            })
            .detach();
        });

        view.update_in(cx, |editor, window, cx| {
            let name_index = name_field_index(editor);
            editor
                .model
                .as_mut()
                .expect("session resolved")
                .set_text(name_index, "Bob".into());
            editor.sync_field_inputs(window, cx);
            editor.save(window, cx);
        });
        wait_until_update_settled(&view, cx);

        let request = last_update
            .lock()
            .unwrap()
            .clone()
            .expect("save should call update_row");
        assert_eq!(request.key, vec![("Id".to_string(), CellValue::I64(1))]);
        assert_eq!(
            request.changes,
            vec![("Name".to_string(), CellValue::String("Bob".into()))]
        );
        assert_eq!(
            request.original_values,
            vec![("Name".to_string(), CellValue::String("Ada".into()))]
        );

        view.read_with(cx, |editor, _| assert!(editor.saved, "commit sets saved"));

        view.update(cx, |editor, cx| editor.dismiss(cx));
        assert_eq!(
            *dismissed.borrow(),
            Some(true),
            "Dismissed must report saved=true after a committed edit"
        );
    }

    #[gpui::test]
    fn row_editor_conflict_refreshes_drafts_from_server(cx: &mut gpui::TestAppContext) {
        let session = person_session();
        let last_update = Arc::new(Mutex::new(None));
        let stub = StubRowEditingService {
            session,
            update_outcome: UpdateRowOutcome::Conflict {
                current_columns: vec!["Id".into(), "Name".into()],
                current_row: Some(vec![CellValue::I64(1), CellValue::String("Server".into())]),
            },
            last_update: last_update.clone(),
        };
        let services = stub.services();
        prepare_row_editor_windowed_test(cx);
        let profile = test_profile();
        let request = open_request();
        let (view, cx) = cx.add_window_view(|window, cx| {
            RowEditor::open_with(
                services,
                profile,
                request,
                RowEditorIntent::Edit,
                RowEditorMode::Commit,
                window,
                cx,
            )
        });
        wait_until_session_ready(&view, cx);

        view.update_in(cx, |editor, window, cx| {
            let name_index = name_field_index(editor);
            editor
                .model
                .as_mut()
                .expect("session resolved")
                .set_text(name_index, "Bob".into());
            editor.sync_field_inputs(window, cx);
            editor.save(window, cx);
        });
        wait_until_update_settled(&view, cx);

        let request = last_update
            .lock()
            .unwrap()
            .clone()
            .expect("update attempted");
        assert_eq!(
            request.original_values,
            vec![("Name".to_string(), CellValue::String("Ada".into()))],
            "UI must forward original_values for optimistic concurrency"
        );

        view.read_with(cx, |editor, _| {
            let name_index = name_field_index(editor);
            let draft = editor.model.as_ref().expect("session resolved").fields[name_index]
                .draft
                .clone();
            assert_eq!(
                draft,
                FieldDraft::Text("Server".into()),
                "conflict refresh must adopt the server's current value"
            );
            assert!(
                editor
                    .banner_error
                    .as_ref()
                    .is_some_and(|msg| msg.contains("changed on the server")),
                "conflict must surface a banner, got {:?}",
                editor.banner_error
            );
            assert!(!editor.saved, "conflict must not mark the dialog saved");
        });
    }

    fn text_field(data_type: &str) -> crate::row_editor_model::FieldState {
        crate::row_editor_model::FieldState {
            column: edit_column("t", data_type),
            kind: crate::row_editor_model::field_kind_for(
                sqlly_core::DatabaseType::SqlServer,
                data_type,
            ),
            original: CellValue::Null,
            draft: FieldDraft::Text("x".to_string()),
            error: None,
            fk: None,
            fk_lead: false,
            fk_label: None,
            for_insert: false,
        }
    }

    #[test]
    fn transform_menu_only_for_editable_free_text_fields() {
        let db = sqlly_core::DatabaseType::SqlServer;
        let text = text_field("nvarchar");
        let text_actions = actions_for(db, &text.column, true);
        assert!(RowEditor::field_offers_transforms(
            &text,
            &text_actions,
            true
        ));
        assert!(
            !RowEditor::field_offers_transforms(&text, &text_actions, false),
            "read-only fields offer nothing"
        );
        // Bool select, enum select, and Guid fields use their own controls.
        let bool_field = text_field("bit");
        let bool_actions = actions_for(db, &bool_field.column, true);
        assert!(!RowEditor::field_offers_transforms(
            &bool_field,
            &bool_actions,
            true
        ));
        let guid_field = text_field("uniqueidentifier");
        let guid_actions = actions_for(db, &guid_field.column, true);
        assert!(
            !RowEditor::field_offers_transforms(&guid_field, &guid_actions, true),
            "Guid fields get the New UUID generator instead"
        );
        let mut enum_field = text_field("enum");
        enum_field.column.enum_values = vec!["open".into()];
        let enum_actions = actions_for(db, &enum_field.column, true);
        assert!(!RowEditor::field_offers_transforms(
            &enum_field,
            &enum_actions,
            true
        ));
        // XML is edited as text but is structured: no case/diacritic macros.
        let xml_field = text_field("xml");
        let xml_actions = actions_for(db, &xml_field.column, true);
        assert_eq!(xml_field.kind, FieldKind::Text);
        assert!(!RowEditor::field_offers_transforms(
            &xml_field,
            &xml_actions,
            true
        ));
        // FK-lead text fields search the referenced table instead.
        let mut fk_field = text_field("nvarchar");
        fk_field.fk_lead = true;
        let fk_actions = actions_for(db, &fk_field.column, true);
        assert!(!RowEditor::field_offers_transforms(
            &fk_field,
            &fk_actions,
            true
        ));
    }

    #[test]
    fn enum_value_spelled_null_stays_distinct_from_the_null_entry() {
        let db = sqlly_core::DatabaseType::SqlServer;
        let mut field = text_field("enum");
        field.column.nullable = true;
        field.column.enum_values = vec!["open".into(), "NULL".into()];
        field.draft = FieldDraft::Text("NULL".to_string());
        let actions = actions_for(db, &field.column, true);
        let choices = RowEditor::select_choices(&field, &actions).expect("enum select");
        assert_eq!(choices.labels, vec!["open", "NULL (2)", "NULL"]);
        assert_eq!(
            choices.values,
            vec![Some("open".to_string()), Some("NULL".to_string()), None]
        );
        assert_eq!(choices.selected, Some(1), "the literal value, not NULL");

        field.draft = FieldDraft::Null;
        let choices = RowEditor::select_choices(&field, &actions).expect("enum select");
        assert_eq!(choices.selected, Some(2), "the null entry");
    }
}
