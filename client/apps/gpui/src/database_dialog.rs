//! Create and drop database dialogs with per-engine DDL preview.

use crate::tooltip::AppTooltip;
use gpui::{
    Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::ConnectionProfile;
use sqlly_intel::metadata::database_ddl::{
    CreateDatabaseSpec, DatabaseDdlError, DropDatabaseSpec, collations_sql, create_database_sql,
    drop_database_sql, encodings_sql, fallback_collations, fallback_encodings,
    maintenance_database, owners_sql, templates_sql,
};
use sqlly_lang::sqlmodel::ConnectionKind;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQueryOutcome};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DatabaseDialogMode {
    Create,
    Drop { name: String, is_active: bool },
}

pub struct DatabaseDialog {
    context: ToolDialogContext,
    mode: DatabaseDialogMode,
    production_read_only: bool,
    create_spec: CreateDatabaseSpec,
    drop_spec: DropDatabaseSpec,
    typed_name: String,
    name_input: Entity<InputState>,
    owner_select: Entity<SelectState<Vec<String>>>,
    encoding_select: Entity<SelectState<Vec<String>>>,
    collation_select: Entity<SelectState<Vec<String>>>,
    template_select: Entity<SelectState<Vec<String>>>,
    owners: Vec<String>,
    encodings: Vec<String>,
    collations: Vec<String>,
    templates: Vec<String>,
    pickers_loading: bool,
    picker_error: Option<String>,
    selects_dirty: bool,
    error: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ToolDialogEvent> for DatabaseDialog {}

impl DatabaseDialog {
    pub fn new_create(
        context: ToolDialogContext,
        production_read_only: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self::new_inner(
            context,
            DatabaseDialogMode::Create,
            production_read_only,
            window,
            cx,
        )
    }

    pub fn new_drop(
        context: ToolDialogContext,
        name: String,
        is_active: bool,
        production_read_only: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self::new_inner(
            context,
            DatabaseDialogMode::Drop { name, is_active },
            production_read_only,
            window,
            cx,
        )
    }

    fn new_inner(
        context: ToolDialogContext,
        mode: DatabaseDialogMode,
        production_read_only: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let db = context.profile.database_type;
        let owner_select = cx.new(|cx| {
            SelectState::new(
                with_default(fallback_list(owners_sql(db).is_some(), &[])),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let encoding_select = cx.new(|cx| {
            SelectState::new(
                with_default(fallback_strings(fallback_encodings(db))),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let collation_select = cx.new(|cx| {
            SelectState::new(
                with_default(fallback_strings(fallback_collations(db))),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let template_select = cx.new(|cx| {
            SelectState::new(
                with_default(vec![]),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let name_input = match &mode {
            DatabaseDialogMode::Create => cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.database_dialog.name_placeholder",
                ))
            }),
            DatabaseDialogMode::Drop { name, .. } => cx.new(|cx| {
                let name = name.clone();
                InputState::new(window, cx).placeholder(
                    crate::localization::input_placeholder_with(window, cx, move || {
                        crate::localization::format(
                            "explorer.database_dialog.confirm_placeholder",
                            &[("name", &name)],
                        )
                    }),
                )
            }),
        };
        let mut this = Self::build(
            context,
            mode,
            production_read_only,
            CreateDatabaseSpec::default(),
            DropDatabaseSpec::default(),
            String::new(),
            name_input,
            owner_select,
            encoding_select,
            collation_select,
            template_select,
            vec![],
            fallback_strings(fallback_encodings(db)),
            fallback_strings(fallback_collations(db)),
            vec![],
            window,
            cx,
        );
        if let DatabaseDialogMode::Drop { name, .. } = &this.mode {
            this.drop_spec.name = name.clone();
        }
        this
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        context: ToolDialogContext,
        mode: DatabaseDialogMode,
        production_read_only: bool,
        create_spec: CreateDatabaseSpec,
        drop_spec: DropDatabaseSpec,
        typed_name: String,
        name_input: Entity<InputState>,
        owner_select: Entity<SelectState<Vec<String>>>,
        encoding_select: Entity<SelectState<Vec<String>>>,
        collation_select: Entity<SelectState<Vec<String>>>,
        template_select: Entity<SelectState<Vec<String>>>,
        owners: Vec<String>,
        encodings: Vec<String>,
        collations: Vec<String>,
        templates: Vec<String>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe_in(
                &name_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = state.read(cx).value().to_string();
                        match this.mode {
                            DatabaseDialogMode::Create => {
                                this.create_spec.name = value;
                            }
                            DatabaseDialogMode::Drop { .. } => {
                                this.typed_name = value;
                            }
                        }
                        this.error = None;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &owner_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_none() {
                        return;
                    }
                    let row = crate::connection_editor::select_row(select, cx);
                    this.create_spec.owner = optional_selection(&this.owners, row);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &encoding_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_none() {
                        return;
                    }
                    let row = crate::connection_editor::select_row(select, cx);
                    this.create_spec.encoding = optional_selection(&this.encodings, row);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &collation_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_none() {
                        return;
                    }
                    let row = crate::connection_editor::select_row(select, cx);
                    this.create_spec.collation = optional_selection(&this.collations, row);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &template_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_none() {
                        return;
                    }
                    let row = crate::connection_editor::select_row(select, cx);
                    this.create_spec.template = optional_selection(&this.templates, row);
                    cx.notify();
                },
            ),
            // The default entry is translated: rebuild the pickers (keeping
            // each chosen value) when the language changes.
            cx.observe_global_in::<crate::localization::LocalizationChanged>(
                window,
                |this: &mut Self, _window, cx| {
                    this.selects_dirty = true;
                    cx.notify();
                },
            ),
        ];

        let mut this = Self {
            context,
            mode,
            production_read_only,
            create_spec,
            drop_spec,
            typed_name,
            name_input,
            owner_select,
            encoding_select,
            collation_select,
            template_select,
            owners,
            encodings,
            collations,
            templates,
            pickers_loading: false,
            picker_error: None,
            selects_dirty: true,
            error: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
            _subscriptions: subscriptions,
        };
        this.load_pickers(cx);
        this
    }

    fn db(&self) -> sqlly_core::DatabaseType {
        self.context.profile.database_type
    }

    fn load_pickers(&mut self, cx: &mut gpui::Context<Self>) {
        let db = self.db();
        self.pickers_loading = true;
        let mut pending = 0usize;
        let mut bump = || pending += 1;
        if let Some(sql) = collations_sql(db) {
            bump();
            Self::run_picker_query(self, sql, PickerKind::Collations, cx);
        }
        if let Some(sql) = encodings_sql(db) {
            bump();
            Self::run_picker_query(self, sql, PickerKind::Encodings, cx);
        }
        if let Some(sql) = owners_sql(db) {
            bump();
            Self::run_picker_query(self, sql, PickerKind::Owners, cx);
        }
        if let Some(sql) = templates_sql(db) {
            bump();
            Self::run_picker_query(self, sql, PickerKind::Templates, cx);
        }
        if pending == 0 {
            self.pickers_loading = false;
        }
    }

    fn run_picker_query(
        this: &mut Self,
        sql: String,
        kind: PickerKind,
        cx: &mut gpui::Context<Self>,
    ) {
        let context = this.context.clone();
        crate::tool_dialog::run_query(&context, sql, cx, move |this, outcome, cx| {
            this.apply_picker_outcome(kind, outcome, cx);
        });
    }

    fn apply_picker_outcome(
        &mut self,
        kind: PickerKind,
        outcome: ToolQueryOutcome,
        cx: &mut gpui::Context<Self>,
    ) {
        self.pickers_loading = false;
        if let Some(err) = outcome.error {
            self.picker_error = Some(err);
            cx.notify();
            return;
        }
        let values = column_strings(&outcome);
        match kind {
            PickerKind::Collations => {
                self.collations = values;
                self.selects_dirty = true;
            }
            PickerKind::Encodings => {
                self.encodings = values;
                self.selects_dirty = true;
            }
            PickerKind::Owners => {
                self.owners = values;
                self.selects_dirty = true;
            }
            PickerKind::Templates => {
                self.templates = values;
                self.selects_dirty = true;
            }
        }
        cx.notify();
    }

    fn preview_sql(&self) -> Result<String, DatabaseDdlError> {
        match self.mode {
            DatabaseDialogMode::Create => create_database_sql(self.db(), &self.create_spec),
            DatabaseDialogMode::Drop { .. } => drop_database_sql(self.db(), &self.drop_spec),
        }
    }

    fn maintenance_profile(&self) -> ConnectionProfile {
        let mut profile = self.context.profile.clone();
        if let Some(db) = maintenance_database(self.db()) {
            profile.database = Some(db.to_string());
        }
        profile
    }

    fn drop_confirmed(&self) -> bool {
        match &self.mode {
            DatabaseDialogMode::Drop { name, .. } => self.typed_name.trim() == name.as_str(),
            DatabaseDialogMode::Create => true,
        }
    }

    fn run_allowed(&self) -> bool {
        !self.production_read_only
            && self.drop_confirmed()
            && !matches!(
                self.mode,
                DatabaseDialogMode::Drop {
                    is_active: true,
                    ..
                }
            )
    }

    fn script_allowed(&self) -> bool {
        self.drop_confirmed() && self.preview_sql().is_ok()
    }

    fn script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        match self.preview_sql() {
            Ok(sql) => {
                let profile = Box::new(self.maintenance_profile());
                cx.emit(ToolDialogEvent::ScriptToEditorForProfile { sql, profile });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(err) => {
                self.error = Some(err.to_string());
                cx.notify();
            }
        }
    }

    fn run_sql(&mut self, cx: &mut gpui::Context<Self>) {
        match self.preview_sql() {
            Ok(sql) => {
                let profile = Box::new(self.maintenance_profile());
                cx.emit(ToolDialogEvent::ScriptToEditorForProfileAndRun { sql, profile });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(err) => {
                self.error = Some(err.to_string());
                cx.notify();
            }
        }
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn checkbox(
        &self,
        id: &'static str,
        label: &'static str,
        checked: bool,
        cx: &mut gpui::Context<Self>,
        on_change: impl Fn(&mut Self, bool) + 'static,
    ) -> gpui::AnyElement {
        let this = cx.entity().downgrade();
        Checkbox::new(id)
            .checked(checked)
            .label(crate::localization::text(label))
            .on_click(move |new_checked: &bool, _window, app| {
                let checked = *new_checked;
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| {
                        on_change(this, checked);
                        cx.notify();
                    });
                }
            })
            .into_any_element()
    }

    /// Rebuild every picker from its value list (also after a language
    /// change, so the translated default entry follows). Rows map to values
    /// by position, never by label text.
    fn sync_selects(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let sync = |items: &[String], selected: &Option<String>| {
            let idx = Some(gpui_component::IndexPath::new(selection_row(
                items, selected,
            )));
            (with_default(items.to_vec()), idx)
        };
        let (items, idx) = sync(&self.owners, &self.create_spec.owner);
        self.owner_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(idx, window, cx);
        });
        let (items, idx) = sync(&self.encodings, &self.create_spec.encoding);
        self.encoding_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(idx, window, cx);
        });
        let (items, idx) = sync(&self.collations, &self.create_spec.collation);
        self.collation_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(idx, window, cx);
        });
        let (items, idx) = sync(&self.templates, &self.create_spec.template);
        self.template_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(idx, window, cx);
        });
    }
}

#[derive(Clone, Copy)]
enum PickerKind {
    Collations,
    Encodings,
    Owners,
    Templates,
}

/// Picker labels: the translated default entry at row 0, then the values,
/// made unique so a value spelled like the default entry (or a repeated
/// value) still maps back to its own row.
fn with_default(items: Vec<String>) -> Vec<String> {
    crate::connection_editor::unique_select_labels(
        std::iter::once(crate::localization::text(
            "explorer.database_dialog.default_option",
        ))
        .chain(items),
    )
}

/// Row showing `selected` in a picker built by [`with_default`]: 0 (the
/// default entry) when nothing is chosen or the value is gone.
fn selection_row(items: &[String], selected: &Option<String>) -> usize {
    selected
        .as_ref()
        .and_then(|value| items.iter().position(|item| item == value))
        .map_or(0, |ix| ix + 1)
}

fn fallback_list(has_catalog: bool, loaded: &[String]) -> Vec<String> {
    if loaded.is_empty() && !has_catalog {
        vec![]
    } else {
        loaded.to_vec()
    }
}

fn fallback_strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_string()).collect()
}

/// Business value behind a confirmed picker row: row 0 is the default
/// entry (no explicit value), row n is `items[n - 1]`.
fn optional_selection(items: &[String], row: Option<usize>) -> Option<String> {
    row.and_then(|row| row.checked_sub(1))
        .and_then(|ix| items.get(ix))
        .cloned()
}

fn column_strings(outcome: &ToolQueryOutcome) -> Vec<String> {
    outcome
        .sets
        .first()
        .map(|set| {
            set.rows
                .iter()
                .filter_map(|row| match row.first() {
                    Some(sqlly_core::CellValue::String(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Focusable for DatabaseDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DatabaseDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.selects_dirty {
            self.selects_dirty = false;
            self.sync_selects(window, cx);
        }

        let preview = self.preview_sql().unwrap_or_else(|err| format!("-- {err}"));
        let field = |label: &'static str, control: gpui::AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .w(px(s(110.0)))
                        .flex_shrink_0()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(label)),
                )
                .child(div().flex_1().min_w(px(0.0)).child(control))
        };

        let mut body = div().flex().flex_col().gap(px(s(10.0)));
        let kind = self.db().connection_kind();

        match &self.mode {
            DatabaseDialogMode::Create => {
                body = body.child(field(
                    "explorer.database_dialog.name_label",
                    Input::new(&self.name_input).w_full().into_any_element(),
                ));
                if owners_sql(self.db()).is_some() {
                    body = body.child(field(
                        "explorer.database_dialog.owner_label",
                        Select::new(&self.owner_select).w_full().into_any_element(),
                    ));
                }
                if encodings_sql(self.db()).is_some() {
                    body = body.child(field(
                        if kind == ConnectionKind::MySqlWire {
                            "explorer.database_dialog.charset_label"
                        } else {
                            "explorer.database_dialog.encoding_label"
                        },
                        Select::new(&self.encoding_select)
                            .w_full()
                            .into_any_element(),
                    ));
                }
                if collations_sql(self.db()).is_some() {
                    body = body.child(field(
                        "explorer.database_dialog.collation_label",
                        Select::new(&self.collation_select)
                            .w_full()
                            .into_any_element(),
                    ));
                }
                if templates_sql(self.db()).is_some() {
                    body = body.child(field(
                        "explorer.database_dialog.template_label",
                        Select::new(&self.template_select)
                            .w_full()
                            .into_any_element(),
                    ));
                }
            }
            DatabaseDialogMode::Drop { name, is_active } => {
                if *is_active {
                    let warning = if let Some(database) = maintenance_database(self.db()) {
                        crate::localization::format(
                            "explorer.database_dialog.active_target_warning",
                            &[("database", &database)],
                        )
                    } else {
                        crate::localization::text("explorer.database_dialog.active_switch_warning")
                    };
                    body = body.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .text_color(Theme::warning())
                            .child(warning),
                    );
                }
                body = body
                    .child(field(
                        "explorer.database_dialog.confirm_label",
                        Input::new(&self.name_input).w_full().into_any_element(),
                    ))
                    .child(field(
                        "explorer.database_dialog.database_label",
                        div()
                            .text_size(px(Theme::font_size_ratio(0.95)))
                            .text_color(Theme::text())
                            .child(name.clone())
                            .into_any_element(),
                    ));
                if kind == ConnectionKind::PostgresWire {
                    body = body.child(self.checkbox(
                        "drop-force",
                        "explorer.database_dialog.with_force",
                        self.drop_spec.force,
                        cx,
                        |this, checked| this.drop_spec.force = checked,
                    ));
                }
                if kind == ConnectionKind::Tds {
                    body = body.child(self.checkbox(
                        "drop-single-user",
                        "explorer.database_dialog.single_user",
                        self.drop_spec.single_user,
                        cx,
                        |this, checked| this.drop_spec.single_user = checked,
                    ));
                }
                body = body.child(self.checkbox(
                    "drop-if-exists",
                    "explorer.database_dialog.if_exists",
                    self.drop_spec.if_exists,
                    cx,
                    |this, checked| this.drop_spec.if_exists = checked,
                ));
            }
        }

        if self.pickers_loading {
            body = body.child(div().text_color(Theme::text_dim()).child(
                crate::localization::text("explorer.database_dialog.options_loading"),
            ));
        } else if self.picker_error.is_some() {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.database_dialog.collations_fallback",
                    )),
            );
        }

        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }

        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "explorer.database_dialog.sql_preview",
                        )),
                )
                .child(
                    div()
                        .p(px(s(8.0)))
                        .rounded(px(s(4.0)))
                        .bg(Theme::sidebar_bg())
                        .font_family(Theme::editor_font_family())
                        .border_1()
                        .border_color(Theme::border())
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .child(preview),
                ),
        );

        let run_tip = if self.production_read_only {
            crate::localization::text("explorer.database_dialog.production_read_only")
        } else if matches!(
            self.mode,
            DatabaseDialogMode::Drop {
                is_active: true,
                ..
            }
        ) {
            crate::localization::text("explorer.database_dialog.switch_before_run")
        } else if !self.drop_confirmed() {
            crate::localization::text("explorer.database_dialog.confirmation_required")
        } else {
            String::new()
        };

        let title = match &self.mode {
            DatabaseDialogMode::Create => crate::localization::format(
                "explorer.database_dialog.create_title",
                &[("connection", &self.context.profile.name)],
            ),
            DatabaseDialogMode::Drop { name, .. } => crate::localization::format(
                "explorer.database_dialog.drop_title",
                &[
                    ("database", name),
                    ("connection", &self.context.profile.name),
                ],
            ),
        };

        let script_allowed = self.script_allowed();
        let run_allowed = self.run_allowed() && self.preview_sql().is_ok();

        let focus_handle = self.focus_handle.clone();
        Dialog::new("database-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            // Script-first dialog: nothing here is fragile, so Escape and a
            // click outside always close it, even mid-edit.
            .easy_close(true)
            .easy_close_when_dirty(true)
            .key_context("DatabaseDialog")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .button({
                let mut run = Button::new("database-dialog-run")
                    .label(crate::localization::text(
                        "explorer.database_dialog.run_button",
                    ))
                    .disabled(!run_allowed);
                if !run_tip.is_empty() {
                    run = run.app_tooltip_text(run_tip);
                }
                run.on_click(cx.listener(|this, _, _window, cx| this.run_sql(cx)))
            })
            .button(
                Button::new("database-dialog-script")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.database_dialog.script_button",
                    ))
                    .disabled(!script_allowed)
                    .on_click(cx.listener(|this, _, _window, cx| this.script_to_editor(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_rows_map_to_values_even_when_a_value_matches_the_default_label() {
        let default_label = crate::localization::text("explorer.database_dialog.default_option");
        let items = vec![default_label.clone(), "utf8".to_string()];
        let labels = with_default(items.clone());
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0], default_label);
        assert_ne!(labels[1], labels[0], "labels stay unique");
        assert_eq!(optional_selection(&items, Some(0)), None);
        assert_eq!(
            optional_selection(&items, Some(1)),
            Some(default_label.clone())
        );
        assert_eq!(
            optional_selection(&items, Some(2)),
            Some("utf8".to_string())
        );
        assert_eq!(optional_selection(&items, None), None);
        assert_eq!(selection_row(&items, &Some(default_label)), 1);
        assert_eq!(selection_row(&items, &Some("gone".to_string())), 0);
        assert_eq!(selection_row(&items, &None), 0);
    }
}
