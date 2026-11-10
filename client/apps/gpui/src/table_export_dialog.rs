//! Export Tables — batch-export selected tables to files.

use std::path::PathBuf;

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectState};
use sqlly_core::ObjectKind;

use crate::dialog::{Dialog, DialogFrame};
use crate::export_options::DEFAULT_PATTERN;
use crate::export_stream;
use crate::preferences::{ExportOptionsPreferences, ExportPreferences};
use crate::results::ExportFormat;
use crate::table_export::TableExportPlan;
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogContext;

#[derive(Clone, Debug)]
struct TableRow {
    schema: String,
    name: String,
    checked: bool,
}

#[cfg(test)]
thread_local! {
    /// Set by tests before constructing a dialog so it never starts a real
    /// (cross-thread) listing; the test lands listings itself.
    static OFFLINE_LISTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Check exactly the rows in `preselect`, or every row when there is none.
fn apply_preselect(
    rows: &mut [TableRow],
    preselect: Option<&std::collections::HashSet<(String, String)>>,
) {
    for row in rows {
        row.checked =
            preselect.is_none_or(|set| set.contains(&(row.schema.clone(), row.name.clone())));
    }
}

#[derive(Clone, Debug)]
pub enum TableExportDialogEvent {
    /// Export `plan` on `profile` — the connection the dialog lists tables
    /// from (its own context, which a retarget may have moved), never
    /// whatever connection the workbench happens to have active.
    Run {
        plan: TableExportPlan,
        profile: Box<sqlly_core::ConnectionProfile>,
    },
    Close,
}

pub struct TableExportDialog {
    context: ToolDialogContext,
    database: String,
    schema_filter: Option<String>,
    tables: Vec<TableRow>,
    /// (schema, table) pairs to pre-check — set by the bulk-operations Export
    /// data handoff. `None` checks everything (the standalone behavior).
    preselect: Option<std::collections::HashSet<(String, String)>>,
    loading: bool,
    load_error: Option<String>,
    load_generation: u64,
    format_select: Entity<SelectState<Vec<String>>>,
    folder_input: Entity<InputState>,
    pattern_input: Entity<InputState>,
    null_select: Entity<SelectState<Vec<String>>>,
    null_custom_input: Entity<InputState>,
    line_breaks_select: Entity<SelectState<Vec<String>>>,
    header_row: bool,
    gzip: bool,
    /// Guard CSV/TSV text cells a spreadsheet would run as a formula. Seeded
    /// from the general export preferences and written back on Export, since
    /// it is the same switch as Settings ▸ Results ▸ Export.
    formula_safe: bool,
    error: Option<String>,
    /// The line last copied by Copy CLI command; the button reads "Copied"
    /// while the form still produces exactly that line.
    cli_copied: Option<String>,
    pending_folder: Option<String>,
    /// Tests land listings by hand: no background listing is started.
    #[cfg(test)]
    offline: bool,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TableExportDialogEvent> for TableExportDialog {}

/// Formats offered by the format select, in label order.
fn streamable_formats() -> Vec<ExportFormat> {
    ExportFormat::all()
        .iter()
        .copied()
        .filter(|f| export_stream::is_streamable(*f))
        .collect()
}

fn format_from_row(row: Option<usize>) -> ExportFormat {
    row.and_then(|row| streamable_formats().get(row).copied())
        .unwrap_or(ExportFormat::Csv)
}

/// Stored `null_mode` values, parallel to the NULL select's labels.
const NULL_MODE_CHOICES: [&str; 4] = ["format-default", "empty", "literal", "custom"];

fn null_mode_from_row(row: Option<usize>) -> &'static str {
    row.and_then(|row| NULL_MODE_CHOICES.get(row).copied())
        .unwrap_or("format-default")
}

/// Stored `line_breaks` values, parallel to the line-breaks select's labels.
const LINE_BREAK_CHOICES: [&str; 3] = ["keep", "space", "escape"];

fn line_breaks_from_row(row: Option<usize>) -> &'static str {
    row.and_then(|row| LINE_BREAK_CHOICES.get(row).copied())
        .unwrap_or("keep")
}

impl TableExportDialog {
    /// `preselect` pre-checks exactly those (schema, table) pairs once the
    /// listing lands (the bulk-operations handoff); `None` checks every table.
    pub fn new(
        context: ToolDialogContext,
        database: String,
        schema_filter: Option<String>,
        preselect: Option<std::collections::HashSet<(String, String)>>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let prefs = ExportOptionsPreferences::load();
        let streamable_labels: Vec<String> = streamable_formats()
            .iter()
            .map(|f| f.localized_label())
            .collect();
        let default_format_idx = streamable_formats()
            .iter()
            .position(|f| *f == ExportFormat::Csv)
            .map(gpui_component::IndexPath::new);
        let format_select = cx
            .new(|cx| SelectState::new(streamable_labels.clone(), default_format_idx, window, cx));
        let folder_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/tmp/sqlly-exports"));
        let pattern_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(prefs.filename_pattern.clone(), window, cx);
            state
        });
        let null_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("results.table_export.null_format_default"),
                        crate::localization::text("results.table_export.null_empty"),
                        "NULL".to_string(),
                        crate::localization::text("results.table_export.null_custom"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let null_custom_input = cx.new(|cx| InputState::new(window, cx).placeholder(r"\N"));
        let line_breaks_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("results.table_export.line_breaks_keep"),
                        crate::localization::text("results.table_export.line_breaks_space"),
                        crate::localization::text("results.table_export.line_breaks_escape"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(
                    LINE_BREAK_CHOICES
                        .iter()
                        .position(|mode| *mode == prefs.line_breaks)
                        .unwrap_or(0),
                )),
                window,
                cx,
            )
        });

        let mut this = Self {
            context,
            database: database.clone(),
            schema_filter: schema_filter.clone(),
            tables: Vec::new(),
            preselect,
            loading: true,
            load_error: None,
            load_generation: 0,
            format_select,
            folder_input,
            pattern_input,
            null_select,
            null_custom_input,
            line_breaks_select,
            header_row: prefs.header_row,
            gzip: prefs.gzip,
            formula_safe: ExportPreferences::load().formula_safe,
            error: None,
            cli_copied: None,
            pending_folder: None,
            #[cfg(test)]
            offline: OFFLINE_LISTING.with(|flag| flag.get()),
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(520.0)),
            _subscriptions: Vec::new(),
        };
        this.wire_subscriptions(window, cx);
        this.load_tables(cx);
        this
    }

    fn wire_subscriptions(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self._subscriptions.push(cx.subscribe_in(
            &self.folder_input,
            window,
            |this, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    let _ = state.read(cx).value();
                    this.error = None;
                    cx.notify();
                }
            },
        ));
        self._subscriptions.push(cx.subscribe_in(
            &self.pattern_input,
            window,
            |this, _state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            },
        ));
    }

    /// Pre-check exactly these (schema, table) pairs — the bulk-operations
    /// handoff. Applies to rows already loaded and to the listing still in
    /// flight (the landing handler re-reads `self.preselect`).
    pub fn set_preselect(
        &mut self,
        tables: std::collections::HashSet<(String, String)>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.preselect = Some(tables);
        apply_preselect(&mut self.tables, self.preselect.as_ref());
        cx.notify();
    }

    /// Point an already-open dialog at another connection/database (the bulk
    /// handoff while the dialog shows a different one): reload the listing
    /// there, with `preselect` applied when it lands. Staying on the same
    /// database only re-applies the preselect.
    pub fn retarget(
        &mut self,
        context: ToolDialogContext,
        database: String,
        schema_filter: Option<String>,
        preselect: Option<std::collections::HashSet<(String, String)>>,
        cx: &mut gpui::Context<Self>,
    ) {
        let same_target = context.profile.id == self.context.profile.id
            && database == self.database
            && schema_filter == self.schema_filter;
        if same_target {
            if let Some(preselect) = preselect {
                self.set_preselect(preselect, cx);
            }
            return;
        }
        self.context = context;
        self.database = database;
        self.schema_filter = schema_filter;
        self.preselect = preselect;
        self.tables.clear();
        self.load_error = None;
        self.error = None;
        self.load_tables(cx);
        cx.notify();
    }

    /// Land a table listing for load `generation`: stale generations are
    /// dropped, and the *current* preselect decides the checkmarks.
    fn apply_listing(
        &mut self,
        generation: u64,
        outcome: Result<Vec<TableRow>, String>,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.load_generation != generation {
            return;
        }
        self.loading = false;
        match outcome {
            Ok(mut tables) => {
                apply_preselect(&mut tables, self.preselect.as_ref());
                self.tables = tables;
                self.load_error = None;
            }
            Err(err) => {
                self.load_error = Some(err);
                self.tables.clear();
            }
        }
        cx.notify();
    }

    fn load_tables(&mut self, cx: &mut gpui::Context<Self>) {
        let mut profile = self.context.profile.clone();
        profile.database = Some(self.database.clone());
        let services = self.context.services.clone();
        let schema_filter = self.schema_filter.clone();
        self.loading = true;
        self.load_generation += 1;
        let generation = self.load_generation;
        #[cfg(test)]
        if self.offline {
            return;
        }
        let database = self.database.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let outcome = services
                        .metadata
                        .list_tables(&profile, &database, schema_filter.as_deref())
                        .await
                        .map(|nodes| {
                            nodes
                                .into_iter()
                                .filter(|n| matches!(n.kind, ObjectKind::Table))
                                .map(|n| TableRow {
                                    schema: n.schema.unwrap_or_default(),
                                    name: n.name,
                                    // The landing handler applies the preselect.
                                    checked: true,
                                })
                                .collect::<Vec<_>>()
                        })
                        .map_err(|e| e.to_string());
                    let _ = tx.send((generation, outcome)).await;
                })
                .await
                .ok();
        })
        .detach();
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Some((generation_id, outcome)) = rx.recv().await {
                let mut cx = async_cx.clone();
                let _ = weak.update(&mut cx, |this, cx| {
                    this.apply_listing(generation_id, outcome, cx);
                });
            }
        })
        .detach();
    }

    fn selected_count(&self) -> usize {
        self.tables.iter().filter(|t| t.checked).count()
    }

    fn selected_format(&self, cx: &gpui::App) -> ExportFormat {
        format_from_row(
            self.format_select
                .read(cx)
                .selected_index(cx)
                .map(|ix| ix.row),
        )
    }

    fn build_prefs(&self, cx: &gpui::App) -> ExportOptionsPreferences {
        let null_mode = null_mode_from_row(
            self.null_select
                .read(cx)
                .selected_index(cx)
                .map(|ix| ix.row),
        );
        let line_breaks = line_breaks_from_row(
            self.line_breaks_select
                .read(cx)
                .selected_index(cx)
                .map(|ix| ix.row),
        );
        ExportOptionsPreferences {
            null_mode: null_mode.to_string(),
            null_custom: self.null_custom_input.read(cx).value().to_string(),
            line_breaks: line_breaks.to_string(),
            header_row: self.header_row,
            gzip: self.gzip,
            filename_pattern: self.pattern_input.read(cx).value().to_string(),
        }
    }

    /// The Run event for `plan`, bound to the connection this dialog lists
    /// tables from (after any retarget).
    fn run_event(&self, plan: TableExportPlan) -> TableExportDialogEvent {
        TableExportDialogEvent::Run {
            plan,
            profile: Box::new(self.context.profile.clone()),
        }
    }

    fn start_export(&mut self, cx: &mut gpui::Context<Self>) {
        let folder = self.folder_input.read(cx).value().trim().to_string();
        if folder.is_empty() {
            self.error = Some(crate::localization::text(
                "results.table_export.folder_required",
            ));
            cx.notify();
            return;
        }
        let selected: Vec<(String, String)> = self
            .tables
            .iter()
            .filter(|t| t.checked)
            .map(|t| (t.schema.clone(), t.name.clone()))
            .collect();
        if selected.is_empty() {
            self.error = Some(crate::localization::text(
                "results.table_export.table_required",
            ));
            cx.notify();
            return;
        }
        let format = self.selected_format(cx);
        let prefs = self.build_prefs(cx);
        prefs.save();
        let mut export_prefs = ExportPreferences::load();
        if export_prefs.formula_safe != self.formula_safe {
            export_prefs.formula_safe = self.formula_safe;
            export_prefs.save();
        }
        let plan = TableExportPlan {
            database: self.database.clone(),
            folder: PathBuf::from(folder),
            pattern: prefs.filename_pattern.clone(),
            format,
            options: prefs.to_options_with(format, self.formula_safe),
            tables: selected,
        };
        cx.emit(self.run_event(plan));
        cx.emit(TableExportDialogEvent::Close);
    }

    /// The `sqlly query … > file` lines equivalent to the current selection
    /// (one per table), for cron or a runbook. `None` without a folder, with
    /// nothing selected, or for formats the CLI cannot render.
    fn cli_command_text(&self, cx: &gpui::App) -> Option<String> {
        let folder = self.folder_input.read(cx).value().trim().to_string();
        if folder.is_empty() {
            return None;
        }
        let tables: Vec<(String, String)> = self
            .tables
            .iter()
            .filter(|t| t.checked)
            .map(|t| (t.schema.clone(), t.name.clone()))
            .collect();
        let format = self.selected_format(cx);
        crate::cli_command::cli_format_for(format)?;
        let prefs = self.build_prefs(cx);
        let plan = TableExportPlan {
            database: self.database.clone(),
            folder: PathBuf::from(folder),
            pattern: prefs.filename_pattern.clone(),
            format,
            options: prefs.to_options_with(format, self.formula_safe),
            tables,
        };
        let targets = crate::table_export::plan_targets(
            self.context.profile.database_type,
            &plan,
            crate::cli_command::DATE_TOKEN,
        );
        crate::cli_command::table_export_commands(
            &crate::cli_command::cli_path(),
            &self.context.profile.name,
            &self.database,
            &targets,
            format,
            plan.options.gzip,
        )
    }

    fn copy_cli_command(&mut self, cx: &mut gpui::Context<Self>) {
        match self.cli_command_text(cx) {
            Some(command) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(command.clone()));
                self.error = None;
                self.cli_copied = Some(command);
            }
            None => {
                self.error = Some(crate::localization::text(
                    "results.table_export.cli_copy_requirements",
                ));
            }
        }
        cx.notify();
    }

    fn browse_folder(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let mut cx = async_cx.clone();
                let _ = weak
                    .update(&mut cx, |this, cx| {
                        this.pending_folder = Some(path.display().to_string());
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(TableExportDialogEvent::Close);
    }

    fn checkbox_row(
        &self,
        id: &'static str,
        label: &str,
        checked: bool,
        index: usize,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let this = cx.entity().downgrade();
        Checkbox::new(id)
            .checked(checked)
            .label(label)
            .on_click(move |new_checked: &bool, _window, app| {
                let checked = *new_checked;
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| {
                        if let Some(row) = this.tables.get_mut(index) {
                            row.checked = checked;
                        }
                        cx.notify();
                    });
                }
            })
            .into_any_element()
    }
}

impl Render for TableExportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        if let Some(folder) = self.pending_folder.take() {
            self.folder_input.update(cx, |state, cx| {
                state.set_value(folder, window, cx);
            });
        }
        let s = Theme::scaled;
        let selected = self.selected_count();
        let scope = match &self.schema_filter {
            Some(schema) => crate::localization::format(
                "results.table_export.schema_scope",
                &[("schema_name", schema)],
            ),
            None => crate::localization::text("results.table_export.database_scope"),
        };
        let body = div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.86)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "results.table_export.description",
                        &[("database_name", &self.database), ("scope", &scope)],
                    )),
            )
            .when_some(self.load_error.as_ref(), |el, err| {
                el.child(div().text_color(Theme::error()).child(err.clone()))
            })
            .when_some(self.error.as_ref(), |el, err| {
                el.child(div().text_color(Theme::error()).child(err.clone()))
            })
            .child(if self.loading {
                div()
                    .child(crate::localization::text(
                        "results.table_export.loading_tables",
                    ))
                    .into_any_element()
            } else {
                let table_rows: Vec<gpui::AnyElement> = self
                    .tables
                    .iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let label = if row.schema.is_empty() {
                            row.name.clone()
                        } else {
                            format!("{}.{}", row.schema, row.name)
                        };
                        self.checkbox_row("table-export-row", &label, row.checked, index, cx)
                    })
                    .collect();
                let mut list = div().flex().flex_col().gap(px(s(4.0)));
                if self.tables.is_empty() && self.load_error.is_none() {
                    list = list.child(
                        div().child(crate::localization::text("results.table_export.no_tables")),
                    );
                }
                for row_el in table_rows {
                    list = list.child(row_el);
                }
                list.into_any_element()
            })
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.table_export.format_label",
                    )),
            )
            .child(Select::new(&self.format_select))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.table_export.output_folder_label",
                    )),
            )
            .child(Input::new(&self.folder_input))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "results.table_export.filename_pattern_hint",
                        &[("default_pattern", DEFAULT_PATTERN)],
                    )),
            )
            .child(Input::new(&self.pattern_input))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.table_export.null_text_label",
                    )),
            )
            .child(Select::new(&self.null_select))
            .child(Input::new(&self.null_custom_input))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.table_export.line_breaks_label",
                    )),
            )
            .child(Select::new(&self.line_breaks_select))
            .child(
                Checkbox::new("table-export-header")
                    .checked(self.header_row)
                    .label(crate::localization::text(
                        "results.table_export.include_header",
                    ))
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.header_row = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("table-export-gzip")
                    .checked(self.gzip)
                    .label(crate::localization::text("results.table_export.gzip_files"))
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.gzip = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("table-export-formula-safe")
                    .checked(self.formula_safe)
                    .label(crate::localization::text(
                        "results.table_export.formula_protection",
                    ))
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.formula_safe = *checked;
                        cx.notify();
                    })),
            );

        let primary = crate::localization::format(
            if selected == 1 {
                "results.table_export.run_button_one"
            } else {
                "results.table_export.run_button"
            },
            &[("table_count", &selected.to_string())],
        );

        let focus_handle = self.focus_handle.clone();
        Dialog::new("table-export-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "results.table_export.dialog_title",
                &[("connection_name", &self.context.profile.name)],
            ))
            .dismiss_on_scrim_click(false)
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .leading_button(
                Button::new("table-export-all")
                    .label(crate::localization::text(
                        "results.table_export.select_all_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        for row in &mut this.tables {
                            row.checked = true;
                        }
                        cx.notify();
                    })),
            )
            .leading_button(
                Button::new("table-export-none")
                    .label(crate::localization::text(
                        "results.table_export.select_none_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        for row in &mut this.tables {
                            row.checked = false;
                        }
                        cx.notify();
                    })),
            )
            .leading_button(
                Button::new("table-export-browse")
                    .label(crate::localization::text(
                        "results.table_export.browse_button",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.browse_folder(window, cx);
                    })),
            )
            .button(
                Button::new("table-export-copy-cli")
                    .label(
                        if self.cli_copied.is_some() && self.cli_copied == self.cli_command_text(cx)
                        {
                            crate::localization::text("results.table_export.cli_copied_button")
                        } else {
                            crate::localization::text("results.table_export.copy_cli_button")
                        },
                    )
                    // The CLI renders CSV, TSV, JSON, JSON Lines and Markdown only.
                    .disabled(
                        self.loading
                            || selected == 0
                            || crate::cli_command::cli_format_for(self.selected_format(cx))
                                .is_none(),
                    )
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.copy_cli_command(cx);
                    })),
            )
            .button(
                Button::new("table-export-run")
                    .primary()
                    .label(primary)
                    .disabled(self.loading || selected == 0)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.start_export(cx);
                    })),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn choice_rows_map_to_typed_values() {
        assert_eq!(null_mode_from_row(Some(0)), "format-default");
        assert_eq!(null_mode_from_row(Some(1)), "empty");
        assert_eq!(null_mode_from_row(Some(2)), "literal");
        assert_eq!(null_mode_from_row(Some(3)), "custom");
        assert_eq!(null_mode_from_row(None), "format-default");
        assert_eq!(line_breaks_from_row(Some(0)), "keep");
        assert_eq!(line_breaks_from_row(Some(1)), "space");
        assert_eq!(line_breaks_from_row(Some(2)), "escape");
        assert_eq!(line_breaks_from_row(None), "keep");
        let formats = streamable_formats();
        for (row, format) in formats.iter().enumerate() {
            assert_eq!(format_from_row(Some(row)), *format);
        }
        assert_eq!(format_from_row(None), ExportFormat::Csv);
    }
    use super::*;
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};
    use sqlly_lang::sqlmodel::DatabaseType;
    use sqlly_services::Services;
    use std::collections::HashSet;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        OFFLINE_LISTING.with(|flag| flag.set(true));
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn test_context() -> ToolDialogContext {
        let mut profile = ConnectionProfile::sql_login(
            "test",
            "host",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".to_string(),
            },
        );
        profile.database_type = DatabaseType::Postgres;
        ToolDialogContext::new(profile, Services::new())
    }

    fn rows(names: &[&str]) -> Vec<TableRow> {
        names
            .iter()
            .map(|n| TableRow {
                schema: "dbo".into(),
                name: (*n).into(),
                checked: true,
            })
            .collect()
    }

    fn checked(view: &TableExportDialog) -> Vec<String> {
        view.tables
            .iter()
            .filter(|t| t.checked)
            .map(|t| t.name.clone())
            .collect()
    }

    fn pick(names: &[&str]) -> HashSet<(String, String)> {
        names
            .iter()
            .map(|n| ("dbo".to_string(), (*n).to_string()))
            .collect()
    }

    #[gpui::test]
    fn listing_that_lands_after_the_handoff_checks_only_the_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            TableExportDialog::new(
                test_context(),
                "shop".into(),
                None,
                Some(pick(&["orders"])),
                window,
                cx,
            )
        });
        view.update(cx, |view, cx| {
            let generation = view.load_generation;
            view.apply_listing(generation, Ok(rows(&["customers", "orders", "audit"])), cx);
            // Before the fix every table came back ticked.
            assert_eq!(checked(view), vec!["orders"]);
        });
    }

    #[gpui::test]
    fn preselect_set_while_the_listing_is_in_flight_still_applies(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            TableExportDialog::new(test_context(), "shop".into(), None, None, window, cx)
        });
        view.update(cx, |view, cx| {
            view.set_preselect(pick(&["audit"]), cx);
            let generation = view.load_generation;
            view.apply_listing(generation, Ok(rows(&["customers", "audit"])), cx);
            assert_eq!(checked(view), vec!["audit"]);
        });
    }

    #[gpui::test]
    fn retargeting_an_open_dialog_reloads_the_new_database_and_drops_stale_listings(
        cx: &mut gpui::TestAppContext,
    ) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            TableExportDialog::new(test_context(), "shop".into(), None, None, window, cx)
        });
        view.update(cx, |view, cx| {
            let old_generation = view.load_generation;
            view.retarget(
                test_context(),
                "analytics".into(),
                None,
                Some(pick(&["events"])),
                cx,
            );
            assert_eq!(view.database, "analytics");
            assert!(view.load_generation > old_generation);
            // The old database's listing arrives late: ignored.
            view.apply_listing(old_generation, Ok(rows(&["orders"])), cx);
            assert!(view.tables.is_empty());
            let generation = view.load_generation;
            view.apply_listing(generation, Ok(rows(&["events", "sessions"])), cx);
            assert_eq!(checked(view), vec!["events"]);
        });
    }

    /// Regression: the workbench used to export with its *active* profile,
    /// so a dialog opened on (or retargeted to) another connection exported
    /// from the wrong server. The Run event now carries the dialog's own.
    #[gpui::test]
    fn run_event_carries_the_dialogs_own_connection_after_a_retarget(
        cx: &mut gpui::TestAppContext,
    ) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            TableExportDialog::new(test_context(), "shop".into(), None, None, window, cx)
        });
        let mut other = test_context();
        other.profile.id = sqlly_core::ConnectionId(uuid::Uuid::new_v4());
        other.profile.name = "reporting".into();
        let other_id = other.profile.id;
        view.update(cx, |view, cx| {
            let first_id = view.context.profile.id;
            assert_ne!(first_id, other_id);
            view.retarget(other, "analytics".into(), None, None, cx);
            let plan = TableExportPlan {
                database: view.database.clone(),
                folder: PathBuf::from("out"),
                pattern: DEFAULT_PATTERN.to_string(),
                format: crate::results::ExportFormat::Csv,
                options: crate::export_options::ExportOptions::default_for(
                    crate::results::ExportFormat::Csv,
                ),
                tables: vec![("dbo".into(), "events".into())],
            };
            let TableExportDialogEvent::Run { plan, profile } = view.run_event(plan) else {
                panic!("expected a Run event");
            };
            assert_eq!(profile.id, other_id);
            assert_eq!(profile.name, "reporting");
            assert_eq!(plan.database, "analytics");
        });
    }
}
