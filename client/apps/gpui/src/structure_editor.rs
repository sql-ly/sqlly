//! Edit Structure — load a table snapshot, edit a draft, script ALTER SQL.

use std::collections::BTreeMap;

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::IconName;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::tab::{Tab, TabBar};
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_services::alter::{
    Change, ChangeSet, MysqlAlgorithm, MysqlLock, RenderOptions, diff, render,
};
use sqlly_services::{ColumnDef, DraftColumn, ForeignKeyDef, IndexDef, TableDraft, TableSnapshot};

use crate::app::StatusLevel;
use crate::default_picks;
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogEvent, ToolQuerySet, build_result_grid};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditorTab {
    Columns,
    Indexes,
    ForeignKeys,
}

impl EditorTab {
    const ALL: [EditorTab; 3] = [
        EditorTab::Columns,
        EditorTab::Indexes,
        EditorTab::ForeignKeys,
    ];

    fn label(self) -> String {
        crate::localization::text(match self {
            EditorTab::Columns => "explorer.structure_editor.tab_columns",
            EditorTab::Indexes => "explorer.structure_editor.tab_indexes",
            EditorTab::ForeignKeys => "explorer.structure_editor.tab_foreign_keys",
        })
    }
}

#[derive(Clone, Debug)]
pub struct ColumnRowState {
    pub id: u64,
    pub origin: Option<String>,
    pub name: String,
    pub data_type: String,
    pub default: String,
    pub nullable: bool,
    pub identity: bool,
    pub primary_key: bool,
    pub description: String,
}

/// Why the description fields are hidden for this table, if they are: the
/// engine has no comment store, or the live descriptions could not be read
/// (editing them blind would script add-vs-update wrongly on SQL Server and
/// could clear comments elsewhere).
pub fn descriptions_unavailable(
    db: sqlly_core::DatabaseType,
    snapshot: &TableSnapshot,
) -> Option<String> {
    sqlly_services::alter::unsupported_reason_message(db)
        .map(crate::localization::message)
        .or_else(|| {
            snapshot.comment_read_error.as_ref().map(|err| {
                crate::localization::format(
                    "explorer.structure_editor.descriptions_read_failed",
                    &[("error", err)],
                )
            })
        })
}

pub fn rows_from_snapshot(snapshot: &TableSnapshot) -> Vec<ColumnRowState> {
    snapshot
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| ColumnRowState {
            id: i as u64 + 1,
            origin: Some(col.name.clone()),
            name: col.name.clone(),
            data_type: col.data_type.clone(),
            default: col.default.clone().unwrap_or_default(),
            nullable: col.nullable,
            identity: col.identity,
            primary_key: snapshot.primary_key.contains(&col.name),
            description: snapshot
                .column_comments
                .get(&col.name)
                .cloned()
                .unwrap_or_default(),
        })
        .collect()
}

/// Indexes into `rows` of the columns the Columns-tab filter shows: those whose
/// name, data type, or description contains `query`, ignoring case and
/// surrounding whitespace. A blank query shows every row. The indexes address
/// the full row list, which stays the source of truth for the draft.
pub fn matching_column_rows(rows: &[ColumnRowState], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            needle.is_empty()
                || [&row.name, &row.data_type, &row.description]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&needle))
        })
        .map(|(ix, _)| ix)
        .collect()
}

/// Which long text the expanded editor is working on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LongTextField {
    Default,
    Description,
    /// An ENUM / SET type, edited one value per line.
    ValueList,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LongTextTarget {
    /// A column row, by its stable row id.
    Column {
        id: u64,
        field: LongTextField,
    },
    TableDescription,
}

/// An `ENUM(...)` / `SET(...)` column type split into its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueList {
    /// `enum` / `SET` exactly as written.
    pub keyword: String,
    pub values: Vec<String>,
    /// Anything after the closing parenthesis (`CHARACTER SET …`).
    pub suffix: String,
}

/// Parse a MySQL / MariaDB `ENUM('a','b')` or `SET(...)` type. Values may be
/// single- or double-quoted; a doubled quote or a backslash escapes the next
/// character. `None` for any other type or a malformed list.
pub fn parse_value_list(data_type: &str) -> Option<ValueList> {
    let text = data_type.trim_start();
    let keyword_len = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphabetic())
        .map_or(text.len(), |(i, _)| i);
    let keyword = &text[..keyword_len];
    if !keyword.eq_ignore_ascii_case("enum") && !keyword.eq_ignore_ascii_case("set") {
        return None;
    }
    let mut chars = text[keyword_len..].char_indices().peekable();
    let base = keyword_len;
    while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
        chars.next();
    }
    if chars.next().map(|(_, c)| c) != Some('(') {
        return None;
    }
    let mut values = Vec::new();
    loop {
        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
            chars.next();
        }
        let (_, quote) = chars.next()?;
        if quote == ')' && values.is_empty() {
            break;
        }
        if quote != '\'' && quote != '"' {
            return None;
        }
        let mut value = String::new();
        loop {
            let (_, c) = chars.next()?;
            if c == '\\' {
                value.push(chars.next()?.1);
            } else if c == quote {
                if chars.peek().is_some_and(|(_, next)| *next == quote) {
                    chars.next();
                    value.push(quote);
                } else {
                    break;
                }
            } else {
                value.push(c);
            }
        }
        values.push(value);
        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
            chars.next();
        }
        match chars.next()? {
            (_, ',') => continue,
            (_, ')') => break,
            _ => return None,
        }
    }
    let rest = chars.peek().map_or(text.len(), |(i, _)| base + i);
    Some(ValueList {
        keyword: keyword.to_string(),
        values,
        suffix: text[rest..].to_string(),
    })
}

/// Render a value list back into a column type, single-quoting each value.
pub fn format_value_list(keyword: &str, values: &[String], suffix: &str) -> String {
    let quoted: Vec<String> = values
        .iter()
        .map(|v| format!("'{}'", v.replace('\\', "\\\\").replace('\'', "''")))
        .collect();
    format!("{keyword}({}){suffix}", quoted.join(","))
}

/// The one-value-per-line editor's text as a column type. Blank lines are
/// skipped; everything else, spaces included, is a value.
pub fn value_list_from_lines(keyword: &str, text: &str, suffix: &str) -> String {
    let values: Vec<String> = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    format_value_list(keyword, &values, suffix)
}

/// What the expanded editor opens with, or `None` when the target does not
/// exist (a removed row, or a type that is not a value list).
pub fn long_text_initial(
    rows: &[ColumnRowState],
    table_description: &str,
    target: LongTextTarget,
) -> Option<String> {
    match target {
        LongTextTarget::TableDescription => Some(table_description.to_string()),
        LongTextTarget::Column { id, field } => {
            let row = rows.iter().find(|r| r.id == id)?;
            match field {
                LongTextField::Default => Some(row.default.clone()),
                LongTextField::Description => Some(row.description.clone()),
                LongTextField::ValueList => {
                    parse_value_list(&row.data_type).map(|list| list.values.join("\n"))
                }
            }
        }
    }
}

/// Write the expanded editor's text back into its field. Returns whether
/// anything was written.
pub fn apply_long_text(
    rows: &mut [ColumnRowState],
    table_description: &mut String,
    target: LongTextTarget,
    text: &str,
) -> bool {
    match target {
        LongTextTarget::TableDescription => {
            *table_description = text.to_string();
            true
        }
        LongTextTarget::Column { id, field } => {
            let Some(row) = rows.iter_mut().find(|r| r.id == id) else {
                return false;
            };
            match field {
                LongTextField::Default => row.default = text.to_string(),
                LongTextField::Description => row.description = text.to_string(),
                LongTextField::ValueList => {
                    let Some(list) = parse_value_list(&row.data_type) else {
                        return false;
                    };
                    row.data_type = value_list_from_lines(&list.keyword, text, &list.suffix);
                }
            }
            true
        }
    }
}

/// A multi-line value as the one-line field shows it: line breaks become
/// spaces (the row keeps the real text).
pub fn inline_display(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

pub fn draft_from_rows(
    snapshot: &TableSnapshot,
    rows: &[ColumnRowState],
    indexes: &[IndexDef],
    foreign_keys: &[ForeignKeyDef],
    table_description: &str,
) -> TableDraft {
    let columns: Vec<DraftColumn> = rows
        .iter()
        .map(|row| DraftColumn {
            origin: row.origin.clone(),
            def: ColumnDef {
                name: row.name.clone(),
                data_type: row.data_type.clone(),
                nullable: row.nullable,
                identity: row.identity,
                default: if row.default.trim().is_empty() {
                    None
                } else {
                    Some(row.default.clone())
                },
                default_constraint_name: snapshot
                    .columns
                    .iter()
                    .find(|c| c.name == row.origin.as_deref().unwrap_or(&row.name))
                    .and_then(|c| c.default_constraint_name.clone()),
            },
        })
        .collect();
    let primary_key: Vec<String> = rows
        .iter()
        .filter(|r| r.primary_key && !r.name.trim().is_empty())
        .map(|r| r.name.clone())
        .collect();
    let column_comments = rows
        .iter()
        .filter(|r| !r.description.trim().is_empty() && !r.name.trim().is_empty())
        .map(|r| (r.name.clone(), r.description.clone()))
        .collect();
    TableDraft {
        schema: snapshot.schema.clone(),
        table: snapshot.table.clone(),
        columns,
        primary_key,
        pk_constraint_name: snapshot.pk_constraint_name.clone(),
        indexes: indexes.to_vec(),
        foreign_keys: foreign_keys.to_vec(),
        table_comment: if table_description.trim().is_empty() {
            None
        } else {
            Some(table_description.to_string())
        },
        column_comments,
        source_ddl: snapshot.stored_table_ddl.clone(),
    }
}

pub fn preview_set(change_set: &ChangeSet) -> ToolQuerySet {
    let columns = vec![
        ResultColumn {
            name: crate::localization::text("explorer.structure_editor.column_change"),
            data_type: "nvarchar".into(),
            nullable: false,
        },
        ResultColumn {
            name: crate::localization::text("explorer.structure_editor.column_object"),
            data_type: "nvarchar".into(),
            nullable: false,
        },
        ResultColumn {
            name: crate::localization::text("explorer.structure_editor.column_detail"),
            data_type: "nvarchar".into(),
            nullable: false,
        },
    ];
    let mut rows = Vec::new();
    for change in &change_set.changes {
        let (kind, object, detail) = describe_change(change);
        rows.push(vec![
            CellValue::String(kind),
            CellValue::String(object),
            CellValue::String(detail),
        ]);
    }
    for note in &change_set.manual_review {
        rows.push(vec![
            CellValue::String(crate::localization::text(
                "explorer.structure_editor.manual_review",
            )),
            CellValue::String("".into()),
            CellValue::String(note.clone()),
        ]);
    }
    ToolQuerySet { columns, rows }
}

fn describe_change(change: &Change) -> (String, String, String) {
    match change {
        Change::AddColumn(c) => (
            crate::localization::text("explorer.structure_editor.change_add_column"),
            c.name.clone(),
            format!(
                "{} {}",
                c.data_type,
                if c.nullable { "NULL" } else { "NOT NULL" }
            ),
        ),
        Change::DropColumn { name } => (
            crate::localization::text("explorer.structure_editor.change_drop_column"),
            name.clone(),
            String::new(),
        ),
        Change::RenameColumn { from, to } => (
            crate::localization::text("explorer.structure_editor.change_rename_column"),
            from.clone(),
            format!("→ {to}"),
        ),
        Change::AlterColumnType { name, to, .. } => (
            crate::localization::text("explorer.structure_editor.change_alter_column"),
            name.clone(),
            format!(
                "{} {}",
                to.data_type,
                if to.nullable { "NULL" } else { "NOT NULL" }
            ),
        ),
        Change::SetDefault { name, to, .. } => (
            crate::localization::text("explorer.structure_editor.change_set_default"),
            name.clone(),
            to.clone().unwrap_or_default(),
        ),
        Change::DropIndex { name } => (
            crate::localization::text("explorer.structure_editor.change_drop_index"),
            name.clone(),
            String::new(),
        ),
        Change::AddIndex(i) => (
            crate::localization::text("explorer.structure_editor.change_add_index"),
            i.name.clone(),
            i.columns.join(", "),
        ),
        Change::DropForeignKey { name } => (
            crate::localization::text("explorer.structure_editor.change_drop_foreign_key"),
            name.clone(),
            String::new(),
        ),
        Change::AddForeignKey(f) => (
            crate::localization::text("explorer.structure_editor.change_add_foreign_key"),
            f.name.clone(),
            format!("{} → {}", f.columns.join(", "), f.ref_table),
        ),
        Change::ReplacePrimaryKey { to, .. } => (
            crate::localization::text("explorer.structure_editor.change_replace_primary_key"),
            "PRIMARY KEY".into(),
            to.join(", "),
        ),
        Change::SetComment { column, to, .. } => (
            crate::localization::text("explorer.structure_editor.change_set_description"),
            column.clone().unwrap_or_else(|| {
                crate::localization::text("explorer.structure_editor.table_placeholder")
            }),
            to.clone().unwrap_or_else(|| {
                crate::localization::text("explorer.structure_editor.cleared_placeholder")
            }),
        ),
    }
}

/// The expanded multi-line editor while it is open.
struct LongTextEdit {
    target: LongTextTarget,
    input: Entity<TextareaState>,
}

struct ColumnInputs {
    id: u64,
    name: Entity<InputState>,
    type_input: Entity<InputState>,
    default_input: Entity<InputState>,
    default_pick_select: Entity<SelectState<Vec<String>>>,
    description_input: Option<Entity<InputState>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ToolDialogEvent> for StructureEditorDialog {}

pub struct StructureEditorDialog {
    snapshot: TableSnapshot,
    profile: ConnectionProfile,
    tab: EditorTab,
    column_rows: Vec<ColumnRowState>,
    column_inputs: Vec<ColumnInputs>,
    indexes: Vec<IndexDef>,
    foreign_keys: Vec<ForeignKeyDef>,
    columns_dirty: bool,
    next_row_id: u64,
    table_description: String,
    table_description_input: Option<Entity<InputState>>,
    /// The multi-line editor for a long default, description or value list.
    long_text: Option<LongTextEdit>,
    /// Columns-tab filter text; narrows which rows render, never the draft.
    column_filter: String,
    column_filter_input: Entity<InputState>,
    pg_using: BTreeMap<String, String>,
    mysql_algorithm: Option<MysqlAlgorithm>,
    mysql_lock: Option<MysqlLock>,
    mysql_algorithm_select: Entity<SelectState<Vec<String>>>,
    mysql_lock_select: Entity<SelectState<Vec<String>>>,
    preview_grid: Option<Entity<SqllyDataTable>>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl StructureEditorDialog {
    pub fn new(
        snapshot: TableSnapshot,
        profile: ConnectionProfile,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let column_rows = rows_from_snapshot(&snapshot);
        let next_row_id = column_rows.len() as u64 + 1;
        let indexes = snapshot.indexes.clone();
        let foreign_keys = snapshot.foreign_keys.clone();
        let mysql_algorithm_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("explorer.structure_editor.option_default"),
                        "INSTANT".into(),
                        "INPLACE".into(),
                        "COPY".into(),
                    ]
                }),
                None,
                window,
                cx,
            )
        });
        let mysql_lock_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("explorer.structure_editor.option_default"),
                        "NONE".into(),
                        "SHARED".into(),
                        "EXCLUSIVE".into(),
                    ]
                }),
                None,
                window,
                cx,
            )
        });
        let mysql_algorithm_listener = mysql_algorithm_select.clone();
        let mysql_lock_listener = mysql_lock_select.clone();
        let comments_supported =
            descriptions_unavailable(profile.database_type, &snapshot).is_none();
        let table_description = snapshot.table_comment.clone().unwrap_or_default();
        let table_description_input = if comments_supported {
            Some(cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::localization::input_placeholder(
                        window,
                        cx,
                        "explorer.structure_editor.table_description_placeholder",
                    ))
                    .default_value(inline_display(&table_description))
            }))
        } else {
            None
        };
        let table_description_listener = table_description_input.clone();
        let column_filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.structure_editor.column_filter_placeholder",
            ))
        });
        let column_filter_listener = column_filter_input.clone();
        let mut dialog = Self {
            snapshot,
            profile,
            tab: EditorTab::Columns,
            column_rows,
            column_inputs: Vec::new(),
            indexes,
            foreign_keys,
            columns_dirty: true,
            next_row_id,
            table_description,
            table_description_input,
            long_text: None,
            column_filter: String::new(),
            column_filter_input,
            pg_using: BTreeMap::new(),
            mysql_algorithm: None,
            mysql_lock: None,
            mysql_algorithm_select,
            mysql_lock_select,
            preview_grid: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(900.0), Theme::scaled(640.0)),
            _subscriptions: vec![
                cx.subscribe_in(
                    &mysql_algorithm_listener,
                    window,
                    |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                        let SelectEvent::Confirm(value) = event;
                        let row = value
                            .as_ref()
                            .and_then(|_| select.read(cx).selected_index(cx))
                            .map(|ix| ix.row);
                        this.mysql_algorithm = mysql_algorithm_from_row(row);
                        cx.notify();
                    },
                ),
                cx.subscribe_in(
                    &mysql_lock_listener,
                    window,
                    |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                        let SelectEvent::Confirm(value) = event;
                        let row = value
                            .as_ref()
                            .and_then(|_| select.read(cx).selected_index(cx))
                            .map(|ix| ix.row);
                        this.mysql_lock = mysql_lock_from_row(row);
                        cx.notify();
                    },
                ),
            ],
        };
        dialog._subscriptions.push(cx.subscribe_in(
            &column_filter_listener,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.column_filter = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        if let Some(input) = table_description_listener {
            dialog._subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.table_description = state.read(cx).value().to_string();
                        this.rebuild_preview(cx);
                        cx.notify();
                    }
                },
            ));
        }
        dialog.rebuild_preview(cx);
        dialog
    }

    fn qualified_title(&self) -> String {
        match &self.snapshot.schema {
            Some(s) => format!("{s}.{}", self.snapshot.table),
            None => self.snapshot.table.clone(),
        }
    }

    fn draft(&self) -> TableDraft {
        draft_from_rows(
            &self.snapshot,
            &self.column_rows,
            &self.indexes,
            &self.foreign_keys,
            &self.table_description,
        )
    }

    fn change_set(&self) -> ChangeSet {
        diff(&self.snapshot, &self.draft())
    }

    fn rebuild_preview(&mut self, cx: &mut gpui::Context<Self>) {
        let set = self.change_set();
        for change in &set.changes {
            if let Change::AlterColumnType { name, to, .. } = change {
                self.pg_using
                    .entry(name.clone())
                    .or_insert_with(|| format!("\"{name}\"::{ty}", ty = to.data_type));
            }
        }
        let preview = preview_set(&set);
        self.preview_grid = build_result_grid(&preview, cx);
    }

    fn rebuild_column_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.column_inputs.clear();
        let pick_labels: Vec<String> = default_picks::default_picks(self.profile.database_type)
            .iter()
            .map(|p| p.label.to_string())
            .collect();
        let db = self.profile.database_type;
        for row in &self.column_rows {
            let id = row.id;
            let name_input =
                cx.new(|cx| InputState::new(window, cx).default_value(row.name.clone()));
            let type_input =
                cx.new(|cx| InputState::new(window, cx).default_value(row.data_type.clone()));
            let default_input = cx
                .new(|cx| InputState::new(window, cx).default_value(inline_display(&row.default)));
            let default_pick_select =
                cx.new(|cx| SelectState::new(pick_labels.clone(), None, window, cx));
            let mut subs = Vec::new();
            subs.push(cx.subscribe_in(
                &name_input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        if let Some(row) = this.row_mut(id) {
                            row.name = state.read(cx).value().to_string();
                        }
                        this.rebuild_preview(cx);
                        cx.notify();
                    }
                },
            ));
            subs.push(cx.subscribe_in(
                &type_input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        if let Some(row) = this.row_mut(id) {
                            row.data_type = state.read(cx).value().to_string();
                        }
                        this.rebuild_preview(cx);
                        cx.notify();
                    }
                },
            ));
            subs.push(cx.subscribe_in(
                &default_input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        if let Some(row) = this.row_mut(id) {
                            row.default = state.read(cx).value().to_string();
                        }
                        this.rebuild_preview(cx);
                        cx.notify();
                    }
                },
            ));
            let default_for_pick = default_input.clone();
            subs.push(cx.subscribe_in(
                &default_pick_select,
                window,
                move |_this: &mut Self, state, event: &SelectEvent<Vec<String>>, window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        let row = state.read(cx).selected_index(cx).map(|ix| ix.row);
                        if let Some(pick) =
                            row.and_then(|row| default_picks::default_picks(db).get(row))
                        {
                            default_for_pick.update(cx, |input, cx| {
                                input.set_value(pick.expr, window, cx);
                            });
                        }
                        state.update(cx, |select, cx| {
                            select.set_selected_index(None, window, cx);
                        });
                    }
                },
            ));
            let description_input =
                if descriptions_unavailable(self.profile.database_type, &self.snapshot).is_none() {
                    let input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(crate::localization::input_placeholder(
                                window,
                                cx,
                                "explorer.structure_editor.column_description_placeholder",
                            ))
                            .default_value(inline_display(&row.description))
                    });
                    subs.push(cx.subscribe_in(
                        &input,
                        window,
                        move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                            if matches!(event, InputEvent::Change) {
                                if let Some(row) = this.row_mut(id) {
                                    row.description = state.read(cx).value().to_string();
                                }
                                this.rebuild_preview(cx);
                                cx.notify();
                            }
                        },
                    ));
                    Some(input)
                } else {
                    None
                };
            self.column_inputs.push(ColumnInputs {
                id,
                name: name_input,
                type_input,
                default_input,
                default_pick_select,
                description_input,
                _subs: subs,
            });
        }
    }

    /// Open the multi-line editor on `target`, seeded with its full text.
    fn open_long_text(
        &mut self,
        target: LongTextTarget,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(initial) = long_text_initial(&self.column_rows, &self.table_description, target)
        else {
            return;
        };
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .auto_grow(8, 16)
                .default_value(initial)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.long_text = Some(LongTextEdit { target, input });
        cx.notify();
    }

    /// Apply: write the editor's text back into its field and the one-line
    /// input that shows it.
    fn apply_long_text(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(edit) = self.long_text.take() else {
            return;
        };
        let text = edit.input.read(cx).value().to_string();
        if !apply_long_text(
            &mut self.column_rows,
            &mut self.table_description,
            edit.target,
            &text,
        ) {
            cx.notify();
            return;
        }
        let (input, shown) = match edit.target {
            LongTextTarget::TableDescription => (
                self.table_description_input.clone(),
                inline_display(&self.table_description),
            ),
            LongTextTarget::Column { id, field } => {
                let row = self.column_rows.iter().find(|r| r.id == id);
                let inputs = self.column_inputs.iter().find(|i| i.id == id);
                match (row, inputs) {
                    (Some(row), Some(inputs)) => match field {
                        LongTextField::Default => (
                            Some(inputs.default_input.clone()),
                            inline_display(&row.default),
                        ),
                        LongTextField::Description => (
                            inputs.description_input.clone(),
                            inline_display(&row.description),
                        ),
                        LongTextField::ValueList => {
                            (Some(inputs.type_input.clone()), row.data_type.clone())
                        }
                    },
                    _ => (None, String::new()),
                }
            }
        };
        // `set_value` emits no Change, so the row keeps the real
        // (multi-line) text rather than the one-line rendering.
        if let Some(input) = input {
            input.update(cx, |state, cx| state.set_value(shown, window, cx));
        }
        self.rebuild_preview(cx);
        cx.notify();
    }

    /// The small button beside a field that opens the multi-line editor.
    fn expand_button(
        &self,
        id: impl Into<gpui::SharedString>,
        target: LongTextTarget,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        Button::new(id.into())
            .ghost()
            .xsmall()
            .icon(IconName::Maximize)
            .tooltip(crate::localization::text(
                "explorer.structure_editor.expand_field_tooltip",
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_long_text(target, window, cx);
            }))
            .into_any_element()
    }

    /// The open multi-line editor: what it edits, the text, Apply / Cancel.
    fn render_long_text(
        &self,
        edit: &LongTextEdit,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let column = |id: u64| {
            self.column_rows
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.name.clone())
                .unwrap_or_default()
        };
        let (heading, hint) = match edit.target {
            LongTextTarget::TableDescription => (
                crate::localization::format(
                    "explorer.structure_editor.long_text_table_description",
                    &[("table", &self.qualified_title())],
                ),
                None,
            ),
            LongTextTarget::Column { id, field } => {
                let name = column(id);
                match field {
                    LongTextField::Default => (
                        crate::localization::format(
                            "explorer.structure_editor.long_text_default",
                            &[("column", &name)],
                        ),
                        None,
                    ),
                    LongTextField::Description => (
                        crate::localization::format(
                            "explorer.structure_editor.long_text_description",
                            &[("column", &name)],
                        ),
                        None,
                    ),
                    LongTextField::ValueList => (
                        crate::localization::format(
                            "explorer.structure_editor.long_text_values",
                            &[("column", &name)],
                        ),
                        Some(crate::localization::text(
                            "explorer.structure_editor.long_text_values_hint",
                        )),
                    ),
                }
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(10.0)))
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(4.0)))
            .bg(Theme::panel_bg())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(heading),
            )
            .children(hint.map(|hint| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(hint)
            }))
            .child(Textarea::new(&edit.input))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(s(6.0)))
                    .child(
                        Button::new("str-long-text-cancel")
                            .small()
                            .label(crate::localization::text(
                                "explorer.structure_editor.long_text_cancel",
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.long_text = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("str-long-text-apply")
                            .small()
                            .primary()
                            .label(crate::localization::text(
                                "explorer.structure_editor.long_text_apply",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_long_text(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn row_mut(&mut self, id: u64) -> Option<&mut ColumnRowState> {
        self.column_rows.iter_mut().find(|r| r.id == id)
    }

    fn push_column_row(&mut self) {
        let id = self.next_row_id;
        self.next_row_id += 1;
        self.column_rows.push(ColumnRowState {
            id,
            origin: None,
            name: String::new(),
            data_type: "NVARCHAR(100)".into(),
            default: String::new(),
            nullable: true,
            identity: false,
            primary_key: false,
            description: String::new(),
        });
        self.columns_dirty = true;
    }

    /// Indexes into `column_rows` the Columns tab currently shows.
    fn visible_column_rows(&self) -> Vec<usize> {
        matching_column_rows(&self.column_rows, &self.column_filter)
    }

    fn set_column_filter(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.column_filter = value.to_string();
        self.column_filter_input.update(cx, |input, cx| {
            input.set_value(value.to_string(), window, cx);
        });
        cx.notify();
    }

    /// Add Column clears the filter first so the new blank row is visible
    /// (it would match almost no filter text and silently vanish otherwise).
    fn add_column(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if !self.column_filter.is_empty() {
            self.set_column_filter("", window, cx);
        }
        self.push_column_row();
        cx.notify();
    }

    fn remove_row(&mut self, id: u64) {
        self.column_rows.retain(|r| r.id != id);
        self.columns_dirty = true;
    }

    fn script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        let set = self.change_set();
        if set.changes.is_empty() && set.manual_review.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.structure_editor.no_changes"),
            });
            return;
        }
        let draft = self.draft();
        let opts = RenderOptions {
            mysql_algorithm: self.mysql_algorithm,
            mysql_lock: self.mysql_lock,
            pg_using: self.pg_using.clone(),
        };
        match render(
            self.profile.database_type,
            &self.snapshot,
            &draft,
            &set,
            &opts,
        ) {
            Ok(script) => {
                let profile = Box::new(self.profile.clone());
                cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
                    sql: script.sql,
                    profile,
                });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(err) => cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::message(err.message()),
            }),
        }
    }

    /// The table's own description field, shown only when the engine stores
    /// descriptions (hidden with a reason on the Columns tab otherwise).
    fn render_table_description_facet(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let Some(input) = &self.table_description_input else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .w(px(s(90.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.structure_editor.description_label",
                    )),
            )
            .child(div().flex_1().child(Input::new(input)))
            .child(self.expand_button(
                "str-expand-table-description",
                LongTextTarget::TableDescription,
                cx,
            ))
            .into_any_element()
    }

    fn row_checkbox(
        &self,
        id: u64,
        kind: &'static str,
        label: &'static str,
        checked: bool,
        set: fn(&mut ColumnRowState, bool),
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let this = cx.entity().downgrade();
        Checkbox::new(gpui::SharedString::from(format!("str-{kind}-{id}")))
            .checked(checked)
            .label(label)
            .on_click(move |new_checked: &bool, _window, app| {
                let checked = *new_checked;
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| {
                        if let Some(row) = this.row_mut(id) {
                            set(row, checked);
                        }
                        this.rebuild_preview(cx);
                        cx.notify();
                    });
                }
            })
            .into_any_element()
    }
}

impl gpui::Focusable for StructureEditorDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StructureEditorDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.columns_dirty {
            self.columns_dirty = false;
            self.rebuild_column_inputs(window, cx);
            self.rebuild_preview(cx);
        }
        let active = self.tab;
        let has_changes =
            !self.change_set().changes.is_empty() || !self.change_set().manual_review.is_empty();
        let db_label = self.profile.database_type.display_name();
        let header = crate::localization::format(
            "explorer.structure_editor.header",
            &[
                ("table", &self.qualified_title()),
                ("engine", &db_label.to_string()),
            ],
        );

        let selected_tab = EditorTab::ALL
            .iter()
            .position(|tab| *tab == active)
            .unwrap_or(0);
        let this = cx.entity();
        let tabs = TabBar::new("str-tabs")
            .underline()
            .selected_index(selected_tab)
            .on_click(move |index: &usize, _window, app| {
                let index = *index;
                this.update(app, |view, cx| {
                    if let Some(tab) = EditorTab::ALL.get(index) {
                        view.tab = *tab;
                        cx.notify();
                    }
                });
            })
            .children(EditorTab::ALL.map(|tab| Tab::new().label(tab.label())));

        let body: gpui::AnyElement = match active {
            EditorTab::Columns => {
                let mut col = div().flex().flex_col().gap(px(s(6.0)));
                if let Some(reason) =
                    descriptions_unavailable(self.profile.database_type, &self.snapshot)
                {
                    col = col.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "explorer.structure_editor.descriptions_hidden",
                                &[("reason", &reason)],
                            )),
                    );
                }
                col = col.child(
                    div()
                        .w(px(s(320.0)))
                        .child(Input::new(&self.column_filter_input).cleanable(true)),
                );
                let visible = self.visible_column_rows();
                if visible.is_empty() && !self.column_rows.is_empty() {
                    col = col.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "explorer.structure_editor.column_filter_no_matches",
                                &[("filter", self.column_filter.trim())],
                            )),
                    );
                }
                // Rows are addressed by their stable id (inputs, checkboxes,
                // Remove), so edits made while filtered land on the right
                // underlying column; the draft always uses every row.
                let visible_rows: Vec<ColumnRowState> = visible
                    .iter()
                    .filter_map(|&ix| self.column_rows.get(ix).cloned())
                    .collect();
                for row in visible_rows {
                    let inputs = self.column_inputs.iter().find(|i| i.id == row.id);
                    if let Some(inputs) = inputs {
                        let column = |field| LongTextTarget::Column { id: row.id, field };
                        let type_expand = parse_value_list(&row.data_type).map(|_| {
                            self.expand_button(
                                format!("str-expand-values-{}", row.id),
                                column(LongTextField::ValueList),
                                cx,
                            )
                        });
                        let default_expand = self.expand_button(
                            format!("str-expand-default-{}", row.id),
                            column(LongTextField::Default),
                            cx,
                        );
                        let description_expand = inputs.description_input.as_ref().map(|_| {
                            self.expand_button(
                                format!("str-expand-description-{}", row.id),
                                column(LongTextField::Description),
                                cx,
                            )
                        });
                        let mut line = div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(div().w(px(s(140.0))).child(Input::new(&inputs.name)))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(s(2.0)))
                                    .child(
                                        div().w(px(s(140.0))).child(Input::new(&inputs.type_input)),
                                    )
                                    .children(type_expand),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(s(4.0)))
                                    .child(
                                        div()
                                            .w(px(s(90.0)))
                                            .child(Input::new(&inputs.default_input)),
                                    )
                                    .child(default_expand)
                                    .child(
                                        div().w(px(s(90.0))).child(
                                            Select::new(&inputs.default_pick_select)
                                                .placeholder(crate::localization::text(
                                                    "explorer.structure_editor.default_pick_placeholder",
                                                )),
                                        ),
                                    ),
                            );
                        if let Some(description_input) = &inputs.description_input {
                            line = line.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(s(2.0)))
                                    .child(
                                        div().w(px(s(150.0))).child(Input::new(description_input)),
                                    )
                                    .children(description_expand),
                            );
                        }
                        col =
                            col.child(
                                line.child(self.row_checkbox(
                                    row.id,
                                    "null",
                                    "NULL",
                                    row.nullable,
                                    |r, v| r.nullable = v,
                                    cx,
                                ))
                                .child(self.row_checkbox(
                                    row.id,
                                    "id",
                                    "IDENTITY",
                                    row.identity,
                                    |r, v| r.identity = v,
                                    cx,
                                ))
                                .child(self.row_checkbox(
                                    row.id,
                                    "pk",
                                    "PK",
                                    row.primary_key,
                                    |r, v| r.primary_key = v,
                                    cx,
                                ))
                                .child(
                                    Button::new(gpui::SharedString::from(format!(
                                        "str-remove-{}",
                                        row.id
                                    )))
                                    .small()
                                    .label(crate::localization::text(
                                        "explorer.structure_editor.remove_button",
                                    ))
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.remove_row(row.id);
                                        cx.notify();
                                    })),
                                ),
                            );
                    }
                }
                col = col.child(
                    Button::new("str-add-col")
                        .small()
                        .label(crate::localization::text(
                            "explorer.structure_editor.add_column_button",
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.add_column(window, cx);
                        })),
                );
                col.into_any_element()
            }
            EditorTab::Indexes => div()
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "explorer.structure_editor.indexes_followup",
                    &[("count", &self.indexes.len().to_string())],
                ))
                .into_any_element(),
            EditorTab::ForeignKeys => div()
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "explorer.structure_editor.foreign_keys_followup",
                    &[("count", &self.foreign_keys.len().to_string())],
                ))
                .into_any_element(),
        };

        let body = match &self.long_text {
            Some(edit) => self.render_long_text(edit, cx),
            None => body,
        };

        let engine_opts: gpui::AnyElement =
            if self.profile.database_type.family() == sqlly_lang::sqlmodel::Dialect::MySql {
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(8.0)))
                    .child(Select::new(&self.mysql_algorithm_select))
                    .child(Select::new(&self.mysql_lock_select))
                    .into_any_element()
            } else {
                div().into_any_element()
            };

        let preview = self
            .preview_grid
            .clone()
            .map(|g| {
                div()
                    .h(px(s(160.0)))
                    .flex_shrink_0()
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(g)
                    .into_any_element()
            })
            .unwrap_or_else(|| div().into_any_element());

        // Rendered through the unified dialog chrome: `AppView` stacks this
        // entity after the full-size workbench, so a plain block would be
        // laid out below the window and never seen (the status bar said
        // "Structure editor ready" and nothing appeared).
        let content = div()
            .id("structure-editor-content")
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .size_full()
            .p(px(s(14.0)))
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(header),
            )
            .child(tabs)
            .child(body)
            .child(engine_opts)
            .child(self.render_table_description_facet(cx))
            .child(div().h(px(1.0)).flex_shrink_0().bg(Theme::border()))
            .child(preview);

        let focus_handle = self.focus_handle.clone();
        let title = crate::localization::format(
            "explorer.structure_editor.title",
            &[("table", &self.qualified_title())],
        );
        Dialog::new("structure-editor-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .key_context("StructureEditorDialog")
            .track_focus(&focus_handle)
            .bare_body()
            .use_default_close(false)
            .on_close(|_this, _window, cx| cx.emit(ToolDialogEvent::Close))
            .child(content)
            .leading_button(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.structure_editor.review_explanation",
                    )),
            )
            .button(
                Button::new("str-close")
                    .label(crate::localization::text(
                        "explorer.structure_editor.close_button",
                    ))
                    .on_click(cx.listener(|_this, _, _window, cx| {
                        cx.emit(ToolDialogEvent::Close);
                    })),
            )
            .button(
                Button::new("str-script")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.structure_editor.script_button",
                    ))
                    .disabled(!has_changes)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.script_to_editor(cx);
                    })),
            )
            .build(self, window, cx)
    }
}

/// MySQL ALGORITHM select rows after "Default" (row 0 = no clause).
const MYSQL_ALGORITHM_CHOICES: [MysqlAlgorithm; 3] = [
    MysqlAlgorithm::Instant,
    MysqlAlgorithm::Inplace,
    MysqlAlgorithm::Copy,
];

fn mysql_algorithm_from_row(row: Option<usize>) -> Option<MysqlAlgorithm> {
    row.and_then(|row| row.checked_sub(1))
        .and_then(|ix| MYSQL_ALGORITHM_CHOICES.get(ix).copied())
}

/// MySQL LOCK select rows after "Default" (row 0 = no clause).
const MYSQL_LOCK_CHOICES: [MysqlLock; 3] =
    [MysqlLock::None, MysqlLock::Shared, MysqlLock::Exclusive];

fn mysql_lock_from_row(row: Option<usize>) -> Option<MysqlLock> {
    row.and_then(|row| row.checked_sub(1))
        .and_then(|ix| MYSQL_LOCK_CHOICES.get(ix).copied())
}

#[cfg(test)]
mod tests {
    fn row(id: u64, data_type: &str) -> super::ColumnRowState {
        super::ColumnRowState {
            id,
            origin: Some(format!("c{id}")),
            name: format!("c{id}"),
            data_type: data_type.into(),
            default: String::new(),
            nullable: true,
            identity: false,
            primary_key: false,
            description: String::new(),
        }
    }

    #[test]
    fn value_lists_parse_with_quotes_escapes_and_suffix() {
        use super::*;
        let list = parse_value_list("enum('a','it''s','back\\\\slash', 'x y')").unwrap();
        assert_eq!(list.keyword, "enum");
        assert_eq!(list.values, vec!["a", "it's", "back\\slash", "x y"]);
        assert_eq!(list.suffix, "");
        let list = parse_value_list("SET ('r', \"w\") CHARACTER SET utf8mb4").unwrap();
        assert_eq!(list.keyword, "SET");
        assert_eq!(list.values, vec!["r", "w"]);
        assert_eq!(list.suffix, " CHARACTER SET utf8mb4");
        for not_a_list in [
            "varchar(10)",
            "enumeration",
            "enum",
            "enum('a'",
            "enum(a,b)",
            "settings('a')",
        ] {
            assert!(parse_value_list(not_a_list).is_none(), "{not_a_list}");
        }
    }

    #[test]
    fn value_lists_round_trip_through_the_one_per_line_editor() {
        use super::*;
        for ty in [
            "enum('a','it''s','back\\\\slash','x y')",
            "set('read','write') CHARACTER SET latin1",
        ] {
            let list = parse_value_list(ty).unwrap();
            assert_eq!(
                format_value_list(&list.keyword, &list.values, &list.suffix),
                ty
            );
        }
        assert_eq!(
            value_list_from_lines("enum", "small\r\n\nmedium\n  large  \n", ""),
            "enum('small','medium','  large  ')"
        );
    }

    #[test]
    fn long_text_initial_and_apply_write_back_to_the_right_field() {
        use super::*;
        let mut rows = vec![row(1, "enum('s','m')"), row(2, "int")];
        rows[1].default = "(getdate())".into();
        rows[1].description = "first\nsecond".into();
        let mut table = "Orders table".to_string();

        let desc = LongTextTarget::Column {
            id: 2,
            field: LongTextField::Description,
        };
        assert_eq!(
            long_text_initial(&rows, &table, desc).as_deref(),
            Some("first\nsecond")
        );
        assert!(apply_long_text(
            &mut rows,
            &mut table,
            desc,
            "line one\nline two"
        ));
        assert_eq!(rows[1].description, "line one\nline two");

        let default = LongTextTarget::Column {
            id: 2,
            field: LongTextField::Default,
        };
        assert!(apply_long_text(
            &mut rows,
            &mut table,
            default,
            "CASE WHEN 1 = 1\n  THEN 0 END"
        ));
        assert_eq!(rows[1].default, "CASE WHEN 1 = 1\n  THEN 0 END");

        let values = LongTextTarget::Column {
            id: 1,
            field: LongTextField::ValueList,
        };
        assert_eq!(
            long_text_initial(&rows, &table, values).as_deref(),
            Some("s\nm")
        );
        assert!(apply_long_text(&mut rows, &mut table, values, "s\nm\nl"));
        assert_eq!(rows[0].data_type, "enum('s','m','l')");
        // A column whose type is not a value list has no list editor.
        let not_list = LongTextTarget::Column {
            id: 2,
            field: LongTextField::ValueList,
        };
        assert_eq!(long_text_initial(&rows, &table, not_list), None);
        assert!(!apply_long_text(&mut rows, &mut table, not_list, "x"));

        assert_eq!(
            long_text_initial(&rows, &table, LongTextTarget::TableDescription).as_deref(),
            Some("Orders table")
        );
        assert!(apply_long_text(
            &mut rows,
            &mut table,
            LongTextTarget::TableDescription,
            "Orders\nOne row per order."
        ));
        assert_eq!(table, "Orders\nOne row per order.");

        // A removed row is ignored rather than writing elsewhere.
        let gone = LongTextTarget::Column {
            id: 9,
            field: LongTextField::Default,
        };
        assert!(!apply_long_text(&mut rows, &mut table, gone, "x"));
    }

    #[test]
    fn inline_fields_show_line_breaks_as_spaces() {
        use super::*;
        assert_eq!(inline_display("a\nb\r\nc"), "a b c");
        assert_eq!(inline_display("plain"), "plain");
    }

    #[test]
    fn multi_line_description_reaches_the_draft_unchanged() {
        use super::*;
        let snapshot = sample_snapshot();
        let mut rows = rows_from_snapshot(&snapshot);
        let mut table = String::new();
        let target = LongTextTarget::Column {
            id: rows[0].id,
            field: LongTextField::Description,
        };
        assert!(apply_long_text(
            &mut rows,
            &mut table,
            target,
            "Primary key.\nNever reused."
        ));
        let draft = draft_from_rows(&snapshot, &rows, &[], &[], &table);
        assert_eq!(
            draft.column_comments.get("id").map(String::as_str),
            Some("Primary key.\nNever reused.")
        );
    }

    #[test]
    fn mysql_option_selects_map_rows() {
        use super::*;
        assert_eq!(mysql_algorithm_from_row(None), None);
        assert_eq!(mysql_algorithm_from_row(Some(0)), None);
        assert_eq!(
            mysql_algorithm_from_row(Some(1)),
            Some(MysqlAlgorithm::Instant)
        );
        assert_eq!(
            mysql_algorithm_from_row(Some(3)),
            Some(MysqlAlgorithm::Copy)
        );
        assert_eq!(mysql_algorithm_from_row(Some(4)), None);
        assert_eq!(mysql_lock_from_row(Some(0)), None);
        assert_eq!(mysql_lock_from_row(Some(1)), Some(MysqlLock::None));
        assert_eq!(mysql_lock_from_row(Some(3)), Some(MysqlLock::Exclusive));
    }

    use super::*;
    use sqlly_services::TableSnapshot;
    use sqlly_services::alter::ColumnDef;

    fn sample_snapshot() -> TableSnapshot {
        TableSnapshot {
            schema: Some("dbo".into()),
            table: "t".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
                identity: false,
                default: None,
                default_constraint_name: None,
            }],
            primary_key: vec!["id".into()],
            pk_constraint_name: None,
            indexes: vec![],
            foreign_keys: vec![],
            stored_index_ddl: vec![],
            stored_trigger_ddl: vec![],
            sqlite_foreign_keys_on: false,
            referenced_by: vec![],
            dependent_views: vec![],
            table_comment: None,
            column_comments: Default::default(),
            stored_table_ddl: None,
            ..Default::default()
        }
    }

    /// Edit Structure opened (status bar: "Structure editor ready") but the
    /// editor rendered as a plain block after the full-size workbench, so it
    /// was laid out below the window. In the same stack it must now paint
    /// as a dialog card inside the window.
    #[gpui::test]
    fn structure_editor_paints_as_a_dialog_inside_the_window(cx: &mut gpui::TestAppContext) {
        use sqlly_core::{SecretProvider, SecretRef};
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let profile = ConnectionProfile::sql_login("dev", "127.0.0.1", "sa", secret);
        let (_host, cx) = cx.add_window_view(|window, cx| {
            let editor =
                cx.new(|cx| StructureEditorDialog::new(sample_snapshot(), profile, window, cx));
            crate::tool_dialog::OverlayStack {
                overlay: editor.into(),
            }
        });
        cx.simulate_resize(gpui::size(px(1440.0), px(900.0)));
        cx.run_until_parked();
        let card = crate::tool_dialog::assert_card_inside_window(
            cx,
            "dialog-card-structure-editor-dialog",
            900.0,
        );
        assert!(f32::from(card.size.height) > 200.0, "{card:?}");
    }

    /// The expand button's editor writes its multi-line text into the row
    /// (and so the draft) while the one-line field shows it on one line, and
    /// Cancel leaves the field alone.
    #[gpui::test]
    fn long_text_editor_writes_back_to_the_row_and_inline_field(cx: &mut gpui::TestAppContext) {
        use sqlly_core::{SecretProvider, SecretRef};
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let profile = ConnectionProfile::sql_login("dev", "127.0.0.1", "sa", secret);
        let mut editor = None;
        let (_host, cx) = cx.add_window_view(|window, cx| {
            let dialog =
                cx.new(|cx| StructureEditorDialog::new(sample_snapshot(), profile, window, cx));
            editor = Some(dialog.clone());
            crate::tool_dialog::OverlayStack {
                overlay: dialog.into(),
            }
        });
        let editor = editor.expect("editor");
        cx.simulate_resize(gpui::size(px(1440.0), px(900.0)));
        cx.run_until_parked();
        let id = editor.read_with(cx, |e, _| e.column_rows[0].id);
        let target = LongTextTarget::Column {
            id,
            field: LongTextField::Description,
        };

        cx.update(|window, app| {
            editor.update(app, |e, cx| {
                e.open_long_text(target, window, cx);
                let input = e.long_text.as_ref().expect("editor open").input.clone();
                input.update(cx, |state, cx| {
                    state.set_value("Primary key.\nNever reused.", window, cx)
                });
                e.apply_long_text(window, cx);
            });
        });
        cx.run_until_parked();
        editor.read_with(cx, |e, cx| {
            assert!(e.long_text.is_none());
            assert_eq!(e.column_rows[0].description, "Primary key.\nNever reused.");
            let inline = e.column_inputs[0]
                .description_input
                .as_ref()
                .expect("description field")
                .read(cx)
                .value()
                .to_string();
            assert_eq!(inline, "Primary key. Never reused.");
            assert!(e.change_set().changes.iter().any(|c| matches!(
                c,
                Change::SetComment { column: Some(col), to: Some(to), .. }
                    if col == "id" && to == "Primary key.\nNever reused."
            )));
        });

        // Cancel discards what was typed.
        cx.update(|window, app| {
            editor.update(app, |e, cx| {
                e.open_long_text(target, window, cx);
                let input = e.long_text.as_ref().expect("editor open").input.clone();
                input.update(cx, |state, cx| state.set_value("discard me", window, cx));
                e.long_text = None;
            });
        });
        editor.read_with(cx, |e, _| {
            assert_eq!(e.column_rows[0].description, "Primary key.\nNever reused.");
        });
    }

    #[test]
    fn rows_from_snapshot_round_trip_empty_diff() {
        let snap = sample_snapshot();
        let rows = rows_from_snapshot(&snap);
        let draft = draft_from_rows(&snap, &rows, &[], &[], "");
        let set = diff(&snap, &draft);
        assert!(set.changes.is_empty());
    }

    #[test]
    fn preview_set_lists_each_change() {
        let snap = sample_snapshot();
        let mut rows = rows_from_snapshot(&snap);
        rows[0].data_type = "bigint".into();
        let draft = draft_from_rows(&snap, &rows, &[], &[], "");
        let set = diff(&snap, &draft);
        let preview = preview_set(&set);
        assert_eq!(preview.rows.len(), set.changes.len());
    }

    #[test]
    fn descriptions_round_trip_without_a_phantom_change() {
        let mut snap = sample_snapshot();
        snap.table_comment = Some("the t table".into());
        snap.column_comments
            .insert("id".into(), "identifier".into());
        let rows = rows_from_snapshot(&snap);
        assert_eq!(rows[0].description, "identifier");
        let draft = draft_from_rows(&snap, &rows, &[], &[], "the t table");
        let set = diff(&snap, &draft);
        assert!(set.changes.is_empty());
    }

    #[test]
    fn editing_descriptions_produces_set_comment_changes() {
        let mut snap = sample_snapshot();
        snap.table_comment = Some("old".into());
        let mut rows = rows_from_snapshot(&snap);
        rows[0].description = "identifier".into();
        let draft = draft_from_rows(&snap, &rows, &[], &[], "new");
        let set = diff(&snap, &draft);
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: None, from, to }
                if from.as_deref() == Some("old") && to.as_deref() == Some("new"))
        ));
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: Some(col), from, to }
                if col == "id" && from.is_none() && to.as_deref() == Some("identifier"))
        ));
        let preview = preview_set(&set);
        assert!(
            preview
                .rows
                .iter()
                .any(|row| matches!(&row[0], CellValue::String(kind) if kind == "Set description"))
        );
    }

    #[test]
    fn clearing_descriptions_stages_a_clear() {
        let mut snap = sample_snapshot();
        snap.table_comment = Some("old".into());
        snap.column_comments
            .insert("id".into(), "identifier".into());
        let rows = rows_from_snapshot(&snap);
        let draft = draft_from_rows(&snap, &rows, &[], &[], "   ");
        assert_eq!(draft.table_comment, None);
        // The row description input initialises from the snapshot; blanking it
        // drops the draft entry so the diff sees a clear.
        let mut rows = rows;
        rows[0].description.clear();
        let draft = draft_from_rows(&snap, &rows, &[], &[], "");
        let set = diff(&snap, &draft);
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: None, from, to }
                if from.as_deref() == Some("old") && to.is_none())
        ));
        assert!(set.changes.iter().any(
            |c| matches!(c, Change::SetComment { column: Some(col), to, .. }
                if col == "id" && to.is_none())
        ));
    }

    fn wide_snapshot() -> TableSnapshot {
        let col = |name: &str, ty: &str| ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            identity: false,
            default: None,
            default_constraint_name: None,
        };
        let mut snap = sample_snapshot();
        snap.columns = vec![
            col("id", "int"),
            col("CustomerName", "nvarchar(100)"),
            col("created_at", "datetime2"),
            col("notes", "nvarchar(max)"),
        ];
        snap.column_comments
            .insert("created_at".into(), "When the Order was placed".into());
        snap
    }

    #[test]
    fn column_filter_empty_or_blank_query_shows_every_row() {
        let rows = rows_from_snapshot(&wide_snapshot());
        assert_eq!(matching_column_rows(&rows, ""), vec![0, 1, 2, 3]);
        assert_eq!(matching_column_rows(&rows, "   "), vec![0, 1, 2, 3]);
    }

    #[test]
    fn column_filter_matches_name_type_and_description_case_insensitively() {
        let rows = rows_from_snapshot(&wide_snapshot());
        // Name, ignoring case.
        assert_eq!(matching_column_rows(&rows, "customer"), vec![1]);
        // Data type substring.
        assert_eq!(matching_column_rows(&rows, "NVARCHAR"), vec![1, 3]);
        // Description (column comment) substring.
        assert_eq!(matching_column_rows(&rows, "order was"), vec![2]);
        // Surrounding whitespace is trimmed.
        assert_eq!(matching_column_rows(&rows, "  DateTime2 "), vec![2]);
        assert!(matching_column_rows(&rows, "zzz").is_empty());
    }

    #[test]
    fn editing_while_filtered_changes_the_visible_column_and_scripts_every_column() {
        let snap = wide_snapshot();
        let mut filtered = rows_from_snapshot(&snap);
        let visible = matching_column_rows(&filtered, "notes");
        assert_eq!(visible, vec![3]);
        // The first (and only) visible row is the underlying `notes` column.
        filtered[visible[0]].data_type = "nvarchar(500)".into();

        let mut unfiltered = rows_from_snapshot(&snap);
        unfiltered[3].data_type = "nvarchar(500)".into();

        let draft = draft_from_rows(&snap, &filtered, &[], &[], "");
        assert_eq!(draft.columns.len(), 4, "the draft must keep hidden columns");
        let set = diff(&snap, &draft);
        assert_eq!(set.changes.len(), 1, "{:?}", set.changes);
        assert!(matches!(
            &set.changes[0],
            Change::AlterColumnType { name, .. } if name == "notes"
        ));
        let opts = RenderOptions::default();
        let filtered_sql = render(
            sqlly_core::DatabaseType::SqlServer,
            &snap,
            &draft,
            &set,
            &opts,
        )
        .unwrap()
        .sql;
        let draft_all = draft_from_rows(&snap, &unfiltered, &[], &[], "");
        let set_all = diff(&snap, &draft_all);
        let unfiltered_sql = render(
            sqlly_core::DatabaseType::SqlServer,
            &snap,
            &draft_all,
            &set_all,
            &opts,
        )
        .unwrap()
        .sql;
        assert_eq!(filtered_sql, unfiltered_sql);
    }

    /// Adding a column while the list is filtered clears the filter so the
    /// new (blank) row is visible, and the earlier edits survive.
    #[gpui::test]
    fn adding_a_column_while_filtered_clears_the_filter(cx: &mut gpui::TestAppContext) {
        use sqlly_core::{SecretProvider, SecretRef};
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let profile = ConnectionProfile::sql_login("dev", "127.0.0.1", "sa", secret);
        let mut editor_slot = None;
        let (_host, cx) = cx.add_window_view(|window, cx| {
            let editor =
                cx.new(|cx| StructureEditorDialog::new(wide_snapshot(), profile, window, cx));
            editor_slot = Some(editor.clone());
            crate::tool_dialog::OverlayStack {
                overlay: editor.into(),
            }
        });
        let editor = editor_slot.unwrap();
        cx.run_until_parked();
        editor.update_in(cx, |this, window, cx| {
            this.set_column_filter("notes", window, cx);
            assert_eq!(this.visible_column_rows(), vec![3]);
            this.column_rows[3].description = "free text".into();
            this.add_column(window, cx);
            assert_eq!(this.column_filter, "");
            assert_eq!(this.column_filter_input.read(cx).value().to_string(), "");
            assert_eq!(this.visible_column_rows(), vec![0, 1, 2, 3, 4]);
            assert_eq!(this.column_rows[3].description, "free text");
            assert_eq!(this.draft().columns.len(), 5);
        });
    }

    #[test]
    fn unreadable_descriptions_hide_the_fields_with_the_reason() {
        let mut snap = sample_snapshot();
        assert_eq!(
            descriptions_unavailable(sqlly_core::DatabaseType::SqlServer, &snap),
            None
        );
        snap.comment_read_error = Some("permission denied".into());
        let reason = descriptions_unavailable(sqlly_core::DatabaseType::SqlServer, &snap).unwrap();
        assert!(reason.contains("permission denied"), "{reason}");
        // An engine with no comment store keeps its own reason.
        assert!(
            descriptions_unavailable(sqlly_core::DatabaseType::Sqlite, &snap)
                .unwrap()
                .contains("no server-side comments")
        );
    }
}
