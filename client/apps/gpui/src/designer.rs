//! Designers — guided Table, Index, and Foreign Key builders over the
//! `sqlly-admin` DDL generators, script-first.
//!
//! Each tab is a small form whose state renders a live DDL preview; the only
//! action is scripting the reviewed statement into a new editor tab. The
//! table designer keeps dynamic column rows (name/type inputs plus
//! NULL/IDENTITY/PK toggles) keyed by stable row ids so removing a row can
//! never re-target another row's subscription.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_services::{
    DesignerColumn, IndexDesignerModel, RelationshipDesignerModel, TableDesignerModel,
};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogEvent;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DesignerTab {
    Table,
    Index,
    ForeignKey,
}

impl DesignerTab {
    pub fn all() -> [DesignerTab; 3] {
        [
            DesignerTab::Table,
            DesignerTab::Index,
            DesignerTab::ForeignKey,
        ]
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            DesignerTab::Table => "explorer.designer.tab_table",
            DesignerTab::Index => "explorer.designer.tab_index",
            DesignerTab::ForeignKey => "explorer.designer.tab_foreign_key",
        })
    }
}

/// One column row's business state in the table designer.
#[derive(Clone, Debug)]
pub struct ColumnRow {
    pub id: u64,
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub identity: bool,
    pub primary_key: bool,
}

/// Split a comma-separated column list into trimmed, non-empty names.
pub fn split_columns(text: &str) -> Vec<String> {
    text.split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Build the table model from form state; `Err` explains what's missing.
pub fn table_model(
    schema: &str,
    table: &str,
    rows: &[ColumnRow],
) -> Result<TableDesignerModel, String> {
    if table.trim().is_empty() {
        return Err(crate::localization::text(
            "explorer.designer.table_name_required",
        ));
    }
    let columns: Vec<DesignerColumn> = rows
        .iter()
        .filter(|r| !r.name.trim().is_empty())
        .map(|r| DesignerColumn {
            name: r.name.trim().to_string(),
            data_type: if r.data_type.trim().is_empty() {
                "NVARCHAR(100)".to_string()
            } else {
                r.data_type.trim().to_string()
            },
            nullable: r.nullable,
            identity: r.identity,
        })
        .collect();
    if columns.is_empty() {
        return Err(crate::localization::text(
            "explorer.designer.columns_required",
        ));
    }
    let primary_key = rows
        .iter()
        .filter(|r| r.primary_key && !r.name.trim().is_empty())
        .map(|r| r.name.trim().to_string())
        .collect();
    Ok(TableDesignerModel {
        schema: if schema.trim().is_empty() {
            "dbo".to_string()
        } else {
            schema.trim().to_string()
        },
        table: table.trim().to_string(),
        columns,
        primary_key,
    })
}

struct ColumnRowInputs {
    id: u64,
    name_input: Entity<InputState>,
    type_input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ToolDialogEvent> for DesignerDialog {}

pub struct DesignerDialog {
    tab: DesignerTab,
    // ── Table designer ──
    table_schema_input: Entity<InputState>,
    table_name_input: Entity<InputState>,
    table_schema: String,
    table_name: String,
    column_rows: Vec<ColumnRow>,
    column_inputs: Vec<ColumnRowInputs>,
    columns_dirty: bool,
    next_row_id: u64,
    // ── Index designer ──
    index_inputs: [Entity<InputState>; 7],
    index_values: [String; 7],
    index_unique: bool,
    index_clustered: bool,
    // ── FK designer ──
    fk_inputs: [Entity<InputState>; 7],
    fk_values: [String; 7],
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

const INDEX_FIELDS: [&str; 7] = [
    "explorer.designer.field_schema",
    "explorer.designer.tab_table",
    "explorer.designer.field_index_name",
    "explorer.designer.field_key_columns",
    "explorer.designer.field_include",
    "explorer.designer.field_filter",
    "explorer.designer.field_fill_factor",
];
const FK_FIELDS: [&str; 7] = [
    "explorer.designer.field_constraint_name",
    "explorer.designer.field_child_schema",
    "explorer.designer.field_child_table",
    "explorer.designer.field_child_columns",
    "explorer.designer.field_parent_schema",
    "explorer.designer.field_parent_table",
    "explorer.designer.field_parent_columns",
];

impl DesignerDialog {
    pub fn new(window: &mut Window, cx: &mut gpui::Context<Self>) -> Self {
        let table_schema_input = cx.new(|cx| InputState::new(window, cx).default_value("dbo"));
        let table_name_input = cx.new(|cx| InputState::new(window, cx).placeholder("NewTable"));

        let mut subscriptions = vec![
            cx.subscribe_in(
                &table_schema_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.table_schema = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &table_name_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.table_name = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
        ];

        let index_defaults = [
            "dbo",
            "MyTable",
            "IX_MyTable_Column",
            "MyColumn",
            "",
            "",
            "",
        ];
        let index_inputs: [Entity<InputState>; 7] = std::array::from_fn(|i| {
            cx.new(|cx| InputState::new(window, cx).default_value(index_defaults[i]))
        });
        for (i, input) in index_inputs.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.index_values[i] = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ));
        }

        let fk_defaults = [
            "FK_Child_Parent",
            "dbo",
            "ChildTable",
            "ParentId",
            "dbo",
            "ParentTable",
            "Id",
        ];
        let fk_inputs: [Entity<InputState>; 7] = std::array::from_fn(|i| {
            cx.new(|cx| InputState::new(window, cx).default_value(fk_defaults[i]))
        });
        for (i, input) in fk_inputs.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.fk_values[i] = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ));
        }

        let mut this = Self {
            tab: DesignerTab::Table,
            table_schema_input,
            table_name_input,
            table_schema: "dbo".to_string(),
            table_name: String::new(),
            column_rows: Vec::new(),
            column_inputs: Vec::new(),
            columns_dirty: true,
            next_row_id: 0,
            index_inputs,
            index_values: index_defaults.map(str::to_string),
            index_unique: false,
            index_clustered: false,
            fk_inputs,
            fk_values: fk_defaults.map(str::to_string),
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(680.0), Theme::scaled(560.0)),
            _subscriptions: subscriptions,
        };
        // Start with an id column and one data column, the common shape.
        this.push_column_row(Some(("Id", "INT", false, true, true)));
        this.push_column_row(Some(("Name", "NVARCHAR(100)", true, false, false)));
        this
    }

    fn push_column_row(&mut self, seed: Option<(&str, &str, bool, bool, bool)>) {
        let (name, ty, nullable, identity, pk) =
            seed.unwrap_or(("", "NVARCHAR(100)", true, false, false));
        self.next_row_id += 1;
        self.column_rows.push(ColumnRow {
            id: self.next_row_id,
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable,
            identity,
            primary_key: pk,
        });
        self.columns_dirty = true;
    }

    fn remove_column_row(&mut self, id: u64, cx: &mut gpui::Context<Self>) {
        self.column_rows.retain(|r| r.id != id);
        self.columns_dirty = true;
        cx.notify();
    }

    fn row_mut(&mut self, id: u64) -> Option<&mut ColumnRow> {
        self.column_rows.iter_mut().find(|r| r.id == id)
    }

    /// Rebuild the per-row input entities from the row models (render-time —
    /// InputState creation needs the window).
    fn rebuild_column_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.column_inputs.clear();
        for row in &self.column_rows {
            let id = row.id;
            let name_input =
                cx.new(|cx| InputState::new(window, cx).default_value(row.name.clone()));
            let type_input =
                cx.new(|cx| InputState::new(window, cx).default_value(row.data_type.clone()));
            let subs = vec![
                cx.subscribe_in(
                    &name_input,
                    window,
                    move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                        if let InputEvent::Change = event {
                            let value = state.read(cx).value().to_string();
                            if let Some(row) = this.row_mut(id) {
                                row.name = value;
                            }
                            cx.notify();
                        }
                    },
                ),
                cx.subscribe_in(
                    &type_input,
                    window,
                    move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                        if let InputEvent::Change = event {
                            let value = state.read(cx).value().to_string();
                            if let Some(row) = this.row_mut(id) {
                                row.data_type = value;
                            }
                            cx.notify();
                        }
                    },
                ),
            ];
            self.column_inputs.push(ColumnRowInputs {
                id,
                name_input,
                type_input,
                _subs: subs,
            });
        }
    }

    /// The DDL the current tab's form produces, or why it can't yet.
    pub fn generate_sql(&self) -> Result<String, String> {
        match self.tab {
            DesignerTab::Table => {
                table_model(&self.table_schema, &self.table_name, &self.column_rows)
                    .map(|m| m.create_table_sql())
            }
            DesignerTab::Index => {
                let [schema, table, name, keys, includes, filter, fill_factor] = &self.index_values;
                if table.trim().is_empty() || name.trim().is_empty() {
                    return Err(crate::localization::text(
                        "explorer.designer.index_names_required",
                    ));
                }
                let columns = split_columns(keys);
                if columns.is_empty() {
                    return Err(crate::localization::text(
                        "explorer.designer.key_columns_required",
                    ));
                }
                let fill_factor = if fill_factor.trim().is_empty() {
                    None
                } else {
                    let parsed: u8 = fill_factor.trim().parse().map_err(|_| {
                        crate::localization::text("explorer.designer.fill_factor_invalid")
                    })?;
                    if !(1..=100).contains(&parsed) {
                        return Err(crate::localization::text(
                            "explorer.designer.fill_factor_invalid",
                        ));
                    }
                    Some(parsed)
                };
                let filter = filter.trim();
                Ok(IndexDesignerModel {
                    schema: if schema.trim().is_empty() {
                        "dbo".to_string()
                    } else {
                        schema.trim().to_string()
                    },
                    table: table.trim().to_string(),
                    name: name.trim().to_string(),
                    columns,
                    include_columns: split_columns(includes),
                    unique: self.index_unique,
                    filter: if filter.is_empty() {
                        None
                    } else {
                        Some(filter.to_string())
                    },
                    fill_factor,
                    clustered: self.index_clustered,
                }
                .create_index_sql())
            }
            DesignerTab::ForeignKey => {
                let [
                    name,
                    child_schema,
                    child_table,
                    child_cols,
                    parent_schema,
                    parent_table,
                    parent_cols,
                ] = &self.fk_values;
                if child_table.trim().is_empty() || parent_table.trim().is_empty() {
                    return Err(crate::localization::text(
                        "explorer.designer.fk_tables_required",
                    ));
                }
                let child_columns = split_columns(child_cols);
                let parent_columns = split_columns(parent_cols);
                if child_columns.is_empty() || parent_columns.is_empty() {
                    return Err(crate::localization::text(
                        "explorer.designer.fk_columns_required",
                    ));
                }
                if child_columns.len() != parent_columns.len() {
                    return Err(crate::localization::text(
                        "explorer.designer.fk_column_counts_mismatch",
                    ));
                }
                Ok(RelationshipDesignerModel {
                    name: if name.trim().is_empty() {
                        format!("FK_{}_{}", child_table.trim(), parent_table.trim())
                    } else {
                        name.trim().to_string()
                    },
                    child_schema: child_schema.trim().to_string(),
                    child_table: child_table.trim().to_string(),
                    child_columns,
                    parent_schema: parent_schema.trim().to_string(),
                    parent_table: parent_table.trim().to_string(),
                    parent_columns,
                }
                .add_foreign_key_sql())
            }
        }
    }

    fn script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        match self.generate_sql() {
            Ok(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(reason) => cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            }),
        }
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn row_checkbox(
        &self,
        id: u64,
        kind: &'static str,
        label: &'static str,
        checked: bool,
        set: fn(&mut ColumnRow, bool),
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let this = cx.entity().downgrade();
        Checkbox::new(gpui::SharedString::from(format!("dsg-{kind}-{id}")))
            .checked(checked)
            .label(crate::localization::text(label))
            .on_click(move |new_checked: &bool, _window, app| {
                let checked = *new_checked;
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| {
                        if let Some(row) = this.row_mut(id) {
                            set(row, checked);
                        }
                        cx.notify();
                    });
                }
            })
            .into_any_element()
    }
}

impl gpui::Focusable for DesignerDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DesignerDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.columns_dirty {
            self.columns_dirty = false;
            self.rebuild_column_inputs(window, cx);
        }
        let active = self.tab;
        let preview = self.generate_sql();

        let tabs = div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(DesignerTab::all().map(|tab| {
                let is_active = tab == active;
                div()
                    .px(px(s(10.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .cursor_pointer()
                    .bg(if is_active {
                        Theme::selection()
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if is_active {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(tab.label())
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.tab = tab;
                            cx.notify();
                        }),
                    )
            }));

        let form: gpui::AnyElement = match active {
            DesignerTab::Table => {
                let mut form = div().flex().flex_col().gap(px(s(6.0))).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(8.0)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.87)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text("explorer.designer.tab_table")),
                        )
                        .child(
                            div()
                                .w(px(s(110.0)))
                                .child(Input::new(&self.table_schema_input)),
                        )
                        .child(div().text_color(Theme::text_dim()).child("."))
                        .child(
                            div()
                                .w(px(s(200.0)))
                                .child(Input::new(&self.table_name_input)),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("dsg-add-col")
                                .small()
                                .label(crate::localization::text(
                                    "explorer.designer.add_column_button",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.push_column_row(None);
                                    cx.notify();
                                })),
                        ),
                );
                let rows: Vec<(u64, ColumnRow)> =
                    self.column_rows.iter().map(|r| (r.id, r.clone())).collect();
                for (id, row) in rows {
                    let inputs = self.column_inputs.iter().find(|i| i.id == id);
                    let Some(inputs) = inputs else { continue };
                    let name_input = inputs.name_input.clone();
                    let type_input = inputs.type_input.clone();
                    form = form.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(div().w(px(s(170.0))).child(Input::new(&name_input)))
                            .child(div().w(px(s(150.0))).child(Input::new(&type_input)))
                            .child(self.row_checkbox(
                                id,
                                "null",
                                "NULL",
                                row.nullable,
                                |r, v| r.nullable = v,
                                cx,
                            ))
                            .child(self.row_checkbox(
                                id,
                                "identity",
                                "explorer.designer.identity_checkbox",
                                row.identity,
                                |r, v| r.identity = v,
                                cx,
                            ))
                            .child(self.row_checkbox(
                                id,
                                "pk",
                                "PK",
                                row.primary_key,
                                |r, v| r.primary_key = v,
                                cx,
                            ))
                            .child(
                                div()
                                    .cursor_pointer()
                                    .text_color(Theme::text_dim())
                                    .px(px(s(6.0)))
                                    .child("\u{00D7}")
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.remove_column_row(id, cx)
                                        }),
                                    ),
                            ),
                    );
                }
                form.into_any_element()
            }
            DesignerTab::Index => {
                let mut form = div().flex().flex_col().gap(px(s(6.0)));
                for (i, label) in INDEX_FIELDS.iter().enumerate() {
                    form = form.child(
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
                            .child(div().flex_1().child(Input::new(&self.index_inputs[i]))),
                    );
                }
                let this = cx.entity().downgrade();
                form = form.child(
                    div().pl(px(s(118.0))).child(
                        Checkbox::new("dsg-unique")
                            .checked(self.index_unique)
                            .label(crate::localization::text(
                                "explorer.designer.unique_checkbox",
                            ))
                            .on_click(move |checked: &bool, _window, app| {
                                let checked = *checked;
                                if let Some(this) = this.upgrade() {
                                    this.update(app, |this, cx| {
                                        this.index_unique = checked;
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
                );
                let this_clustered = cx.entity().downgrade();
                form = form.child(
                    div().pl(px(s(118.0))).child(
                        Checkbox::new("dsg-clustered")
                            .checked(self.index_clustered)
                            .label(crate::localization::text(
                                "explorer.designer.clustered_checkbox",
                            ))
                            .on_click(move |checked: &bool, _window, app| {
                                let checked = *checked;
                                if let Some(this) = this_clustered.upgrade() {
                                    this.update(app, |this, cx| {
                                        this.index_clustered = checked;
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
                );
                form.into_any_element()
            }
            DesignerTab::ForeignKey => {
                let mut form = div().flex().flex_col().gap(px(s(6.0)));
                for (i, label) in FK_FIELDS.iter().enumerate() {
                    form = form.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(
                                div()
                                    .w(px(s(130.0)))
                                    .flex_shrink_0()
                                    .text_size(px(Theme::font_size_ratio(0.87)))
                                    .text_color(Theme::text_dim())
                                    .child(crate::localization::text(label)),
                            )
                            .child(div().flex_1().child(Input::new(&self.fk_inputs[i]))),
                    );
                }
                form.into_any_element()
            }
        };

        let preview_el = div()
            .id("designer-preview")
            .flex_1()
            .min_h(px(s(90.0)))
            .overflow_scroll()
            .p(px(s(8.0)))
            .bg(Theme::sidebar_bg())
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.87)))
            .child(match &preview {
                Ok(sql) => div()
                    .text_color(Theme::kind_string())
                    .whitespace_normal()
                    .child(sql.clone()),
                Err(reason) => div().text_color(Theme::text_dim()).child(reason.clone()),
            });

        // Tabs, scrolling form, and live preview manage their own layout, so
        // the chrome body stays bare; the default Close button replaces the
        // old footer Close.
        let content = div()
            .id("designer-content")
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(14.0)))
            .child(tabs)
            .child(
                div()
                    .id("designer-form")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_scroll()
                    .child(form),
            )
            .child(preview_el);

        let focus_handle = self.focus_handle.clone();
        Dialog::new("designer-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("explorer.designer.title"))
            .easy_close(true)
            .key_context("DesignerDialog")
            .track_focus(&focus_handle)
            .bare_body()
            .on_close(|this, _window, cx| this.close(cx))
            .child(content)
            .leading_button(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.designer.review_explanation",
                    )),
            )
            .button(
                Button::new("dsg-script")
                    .primary()
                    .label(crate::localization::text("explorer.designer.script_button"))
                    .disabled(preview.is_err())
                    .on_click(cx.listener(|this, _, _window, cx| this.script_to_editor(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, ty: &str, nullable: bool, identity: bool, pk: bool) -> ColumnRow {
        ColumnRow {
            id: 0,
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable,
            identity,
            primary_key: pk,
        }
    }

    #[test]
    fn table_model_builds_create_table_with_pk() {
        let rows = vec![
            row("Id", "INT", false, true, true),
            row("Name", "NVARCHAR(100)", true, false, false),
        ];
        let sql = table_model("dbo", "People", &rows)
            .unwrap()
            .create_table_sql();
        assert!(sql.starts_with("CREATE TABLE [dbo].[People]"), "{sql}");
        assert!(sql.contains("[Id] INT IDENTITY(1,1) NOT NULL"), "{sql}");
        assert!(sql.contains("[Name] NVARCHAR(100) NULL"), "{sql}");
        assert!(
            sql.contains("CONSTRAINT [PK_People] PRIMARY KEY ([Id])"),
            "{sql}"
        );
    }

    #[test]
    fn table_model_requires_name_and_columns() {
        assert!(table_model("dbo", "", &[row("a", "INT", true, false, false)]).is_err());
        assert!(table_model("dbo", "T", &[]).is_err());
        assert!(table_model("dbo", "T", &[row("", "INT", true, false, false)]).is_err());
    }

    #[test]
    fn empty_schema_defaults_to_dbo() {
        let model = table_model(" ", "T", &[row("a", "INT", true, false, false)]).unwrap();
        assert_eq!(model.schema, "dbo");
    }

    #[test]
    fn split_columns_trims_and_drops_empties() {
        assert_eq!(split_columns(" a , b ,, c "), vec!["a", "b", "c"]);
        assert!(split_columns("  ").is_empty());
    }
}
