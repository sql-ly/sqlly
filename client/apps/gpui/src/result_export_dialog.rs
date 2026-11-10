//! Export Results dialog — one entry point for every result-grid export
//! format.
//!
//! Opened from the result grid's right-click menu (a single "Export…"
//! item) instead of a flat submenu of per-format items. The grid data is
//! resolved once at menu-action time into an [`ExportPayload`] — visible
//! rows after sort/filter, the active selection, and export masks applied —
//! so the dialog renders from a stable snapshot regardless of what the grid
//! does while it is open. The dialog lists every format enabled in the
//! user's export preferences, grouped Standard / Programming, with a
//! clipboard-or-file destination choice, and runs the exact export path the
//! old per-format menu items used.

use crate::tooltip::AppTooltip;

use gpui::{
    App, ClipboardItem, Context, IntoElement, ParentElement, Render, SharedString, Styled,
    WeakEntity, Window, div, prelude::*, px,
};
use gpui_component::Disableable;
use gpui_component::IconName;
use gpui_component::button::{Button, ButtonVariants as _};

use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::preferences::ExportPreferences;
use crate::results::{ExportFormat, ResultsView};
use crate::theme::Theme;

/// Result-grid data resolved at menu-action time, ready to render through
/// any [`ExportFormat`]. `spill_truncated && whole_set` means the rows were
/// capped at `EXPORT_MAX_ROWS` for the whole set — the dialog raises the
/// truncation notice only when the user actually exports.
pub struct ExportPayload {
    /// Selection-narrowed data — what the dialog exports by default. Equal to
    /// the whole visible set when there was no real selection.
    pub(crate) columns: Vec<ResultColumn>,
    pub(crate) rows: Vec<Vec<CellValue>>,
    /// The whole visible set (post sort/filter, masks applied). Used when the
    /// user toggles "export all" instead of the selection.
    pub(crate) all_columns: Vec<ResultColumn>,
    pub(crate) all_rows: Vec<Vec<CellValue>>,
    /// True when a real selection narrowed `columns`/`rows` below the full set,
    /// i.e. the selected/all toggle is meaningful and shown.
    pub(crate) has_selection: bool,
    pub(crate) spill_truncated: bool,
    pub(crate) whole_set: bool,
    pub(crate) slot: usize,
}

impl ExportPayload {
    /// The header's shape line. In the default "selected only" mode it reports
    /// the exported dimensions; toggled to "export all" over a real selection
    /// it reads simply "All".
    pub(crate) fn shape_label(&self, selected_only: bool) -> String {
        if self.has_selection && !selected_only {
            return crate::localization::text("results.export_dialog.all_rows_scope");
        }
        let (rows, cols) = if selected_only {
            (self.rows.len(), self.columns.len())
        } else {
            (self.all_rows.len(), self.all_columns.len())
        };
        let key = match (rows == 1, cols == 1) {
            (true, true) => "results.export_dialog.shape_one_row_one_column",
            (true, false) => "results.export_dialog.shape_one_row_many_columns",
            (false, true) => "results.export_dialog.shape_many_rows_one_column",
            (false, false) => "results.export_dialog.shape_many_rows_many_columns",
        };
        crate::localization::format(
            key,
            &[
                ("row_count", &rows.to_string()),
                ("column_count", &cols.to_string()),
            ],
        )
    }
}

/// Where the export lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    Clipboard,
    File,
}

/// True when a format can be copied to the clipboard. Binary formats (XLSX)
/// are files by nature and always save.
pub(crate) fn destination_allows_clipboard(format: ExportFormat) -> bool {
    !format.is_binary()
}

/// Every export format the dialog should offer, in menu order: the enabled
/// standard formats, then the enabled programming-language code snippets.
/// Pure so the preference filtering stays unit-testable.
pub(crate) fn dialog_formats(prefs: &ExportPreferences) -> Vec<ExportFormat> {
    let mut formats: Vec<ExportFormat> = ExportFormat::all()
        .iter()
        .copied()
        .filter(|f| prefs.standard_enabled(*f))
        .collect();
    for format in ExportFormat::programming() {
        let language = format
            .code_language()
            .expect("programming formats map to a language");
        if prefs.language_enabled(language) {
            formats.push(*format);
        }
    }
    formats
}

/// How many leading entries of [`dialog_formats`] are standard formats —
/// everything after is the Programming group.
pub(crate) fn standard_format_count(formats: &[ExportFormat]) -> usize {
    formats
        .iter()
        .position(|f| f.code_language().is_some())
        .unwrap_or(formats.len())
}

pub struct ResultExportDialog {
    payload: ExportPayload,
    results: WeakEntity<ResultsView>,
    db: DatabaseType,
    formats: Vec<ExportFormat>,
    /// Index into `formats` where the Programming group starts.
    programming_start: usize,
    selected: usize,
    save_to_file: bool,
    /// When true (the default), export only the grid selection; when false,
    /// export the whole visible set. Only meaningful when the payload has a
    /// real selection (`payload.has_selection`).
    selected_only: bool,
    /// Snapshot of the export preferences the dialog was opened with; the
    /// formula guard for CSV/TSV output comes from here.
    prefs: ExportPreferences,
    /// The line last copied by Copy CLI command; the button reads "Copied"
    /// while the dialog still produces exactly that line.
    cli_copied: Option<String>,
}

impl ResultExportDialog {
    pub(crate) fn new(
        payload: ExportPayload,
        prefs: &ExportPreferences,
        db: DatabaseType,
        results: WeakEntity<ResultsView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let formats = dialog_formats(prefs);
        let programming_start = standard_format_count(&formats);
        let save_to_file = formats.first().is_some_and(|f| f.is_binary());
        cx.notify();
        Self {
            payload,
            results,
            db,
            formats,
            programming_start,
            selected: 0,
            save_to_file,
            selected_only: true,
            prefs: *prefs,
            cli_copied: None,
        }
    }

    /// Options for rendering `format`: the format's defaults with the user's
    /// formula-guard preference applied.
    fn options_for(&self, format: ExportFormat) -> crate::export_options::ExportOptions {
        crate::results::adhoc_export_options(format, &self.prefs)
    }

    /// The currently selected format, if any formats are enabled.
    pub(crate) fn selected_format(&self) -> Option<ExportFormat> {
        self.formats.get(self.selected).copied()
    }

    /// The columns the current scope (selection vs. all) will export.
    fn active_columns(&self) -> &[ResultColumn] {
        if self.selected_only {
            &self.payload.columns
        } else {
            &self.payload.all_columns
        }
    }

    /// The rows the current scope (selection vs. all) will export.
    fn active_rows(&self) -> &[Vec<CellValue>] {
        if self.selected_only {
            &self.payload.rows
        } else {
            &self.payload.all_rows
        }
    }

    /// Flip between exporting only the selection and exporting the whole set.
    pub(crate) fn set_selected_only(&mut self, selected_only: bool, cx: &mut Context<Self>) {
        self.selected_only = selected_only;
        cx.notify();
    }

    /// The destination the Export button will use for the current selection.
    pub(crate) fn destination(&self) -> Option<Destination> {
        let format = self.selected_format()?;
        Some(
            if destination_allows_clipboard(format) && !self.save_to_file {
                Destination::Clipboard
            } else {
                Destination::File
            },
        )
    }

    /// True when the clipboard destination row should render as disabled.
    pub(crate) fn clipboard_destination_disabled(&self) -> bool {
        self.selected_format()
            .is_some_and(|f| !destination_allows_clipboard(f))
    }

    pub(crate) fn select_format(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.formats.len() {
            return;
        }
        self.selected = index;
        // A binary workbook cannot go on the clipboard; snap the destination
        // to file. Text formats keep whatever the user last picked.
        if !destination_allows_clipboard(self.formats[index]) {
            self.save_to_file = true;
        }
        cx.notify();
    }

    pub(crate) fn select_destination(&mut self, save_to_file: bool, cx: &mut Context<Self>) {
        if self.clipboard_destination_disabled() && !save_to_file {
            return;
        }
        self.save_to_file = save_to_file;
        cx.notify();
    }

    /// The CLI line that re-runs the query that produced these results into
    /// the selected format. Only formats the CLI renders have an equivalent;
    /// selections and grid filters are client-side, so the line always
    /// exports the full re-run result. `None` for sources that are not
    /// read-only SELECTs: the CLI runs `query` read-only, and re-running a
    /// write to export its output is not something to hand out as a one-liner.
    pub(crate) fn cli_command_text(&self, cx: &App) -> Option<String> {
        let cli_format = crate::cli_command::cli_format_for(self.selected_format()?)?;
        let results = self.results.upgrade()?;
        let (profile, sql) = results.read(cx).cli_source()?;
        if !sqlly_intel::query::safety::classify_select_only_for(profile.database_type, &sql)
            .select_only
        {
            return None;
        }
        Some(crate::cli_command::query_command(
            &crate::cli_command::cli_path(),
            &profile.name,
            profile.database.as_deref(),
            &sql,
            cli_format,
        ))
    }

    pub(crate) fn copy_cli_command(&mut self, cx: &mut Context<Self>) {
        let Some(command) = self.cli_command_text(cx) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
        self.cli_copied = Some(command);
        cx.notify();
    }

    /// Grid rows hold 8 KiB previews of oversized cells; say so when this
    /// export writes any (see [`ResultsView::note_preview_cells_in_export`]).
    fn note_preview_cells(&self, cx: &mut App) {
        if let Some(results) = self.results.upgrade() {
            let slot = self.payload.slot;
            results.update(cx, |view, cx| {
                view.note_preview_cells_in_export(slot, cx);
            });
        }
    }

    /// Run the export for the current selection: raise the spill-truncation
    /// notice when the whole set was capped, then copy to the clipboard or
    /// start the save-to-file flow. The caller closes the dialog window.
    pub(crate) fn execute(&mut self, cx: &mut Context<Self>) {
        let Some(format) = self.selected_format() else {
            return;
        };
        if self.payload.spill_truncated
            && self.payload.whole_set
            && let Some(results) = self.results.upgrade()
        {
            let slot = self.payload.slot;
            results.update(cx, |view, cx| view.note_truncated_export_for_slot(slot, cx));
        }
        self.note_preview_cells(cx);
        if self.destination() == Some(Destination::File) {
            self.save_export(format, cx);
        } else if let Some(text) = crate::results::export_text_for_format_with(
            format,
            self.db,
            self.active_columns(),
            self.active_rows(),
            &self.options_for(format),
        ) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// "Open in Spreadsheet": export the active scope (selection vs. all) to
    /// a temp file and open it in the OS default spreadsheet app, reusing the
    /// same writers as a save-to-file export. Runs the same spill-truncation
    /// notice as a regular export first.
    #[cfg(not(target_arch = "wasm32"))]
    fn open_in_spreadsheet(&self, cx: &mut App) {
        if self.payload.spill_truncated
            && self.payload.whole_set
            && let Some(results) = self.results.upgrade()
        {
            let slot = self.payload.slot;
            results.update(cx, |view, cx| view.note_truncated_export_for_slot(slot, cx));
        }
        self.note_preview_cells(cx);
        // Build, write, and launch off the UI thread (a large XLSX workbook
        // takes a while); the writer and options match a save-to-file export.
        let prefs = self.prefs;
        let db = self.db;
        self.prepare_workbook(cx);
        let workbook = self.grid_workbook(&prefs, cx);
        let columns = self.active_columns().to_vec();
        let rows = self.active_rows().to_vec();
        let task = crate::open_in_spreadsheet::spawn_open(cx, move || match workbook {
            Some(bytes) => Some((bytes, ExportFormat::Xlsx)),
            None => crate::open_in_spreadsheet::spreadsheet_bytes(&prefs, db, &columns, &rows),
        });
        let results = self.results.clone();
        cx.spawn(async move |cx| {
            if let Err(message) = task.await {
                let _ = results.update(cx, |view, cx| {
                    view.note_export_write_failed_messages(message, cx);
                });
            }
        })
        .detach();
    }

    /// Browser build: a page cannot launch the spreadsheet app, so the same
    /// workbook downloads instead (see `open_in_spreadsheet::download`).
    #[cfg(target_arch = "wasm32")]
    fn open_in_spreadsheet(&self, cx: &mut App) {
        if self.payload.spill_truncated
            && self.payload.whole_set
            && let Some(results) = self.results.upgrade()
        {
            let slot = self.payload.slot;
            results.update(cx, |view, cx| view.note_truncated_export_for_slot(slot, cx));
        }
        self.note_preview_cells(cx);
        let prefs = self.prefs;
        self.prepare_workbook(cx);
        let built = match self.grid_workbook(&prefs, cx) {
            Some(bytes) => Some((bytes, ExportFormat::Xlsx)),
            None => crate::open_in_spreadsheet::spreadsheet_bytes(
                &self.prefs,
                self.db,
                self.active_columns(),
                self.active_rows(),
            ),
        };
        let Some((bytes, format)) = built else {
            return;
        };
        if let Err(message) = crate::open_in_spreadsheet::download(&bytes, format)
            && let Some(results) = self.results.upgrade()
        {
            results.update(cx, |view, cx| {
                view.note_export_write_failed_messages(message, cx);
            });
        }
    }

    /// Bring the result's pivot up to date for the workbook's pivot sheet.
    fn prepare_workbook(&self, cx: &mut App) {
        if let Some(results) = self.results.upgrade() {
            let slot = self.payload.slot;
            results.update(cx, |view, cx| view.prepare_excel_export(slot, cx));
        }
    }

    /// The rich workbook for an open-in-spreadsheet export, when the
    /// spreadsheet format is XLSX and the results are still open.
    fn grid_workbook(
        &self,
        prefs: &crate::preferences::ExportPreferences,
        cx: &App,
    ) -> Option<Vec<u8>> {
        if crate::open_in_spreadsheet::spreadsheet_format(prefs) != ExportFormat::Xlsx {
            return None;
        }
        let results = self.results.upgrade()?;
        Some(results.read(cx).excel_workbook(
            self.payload.slot,
            self.active_columns(),
            self.active_rows(),
            cx,
        ))
    }

    /// The bytes a save-to-file export writes: binary formats (XLSX) as
    /// their bytes, text formats (including the programming-language
    /// snippets) as UTF-8.
    fn export_file_bytes(&self, format: ExportFormat, cx: &App) -> Option<Vec<u8>> {
        if format == ExportFormat::Xlsx
            && let Some(results) = self.results.upgrade()
        {
            // The grid as shown — formats, widths, formulas — plus its pivot.
            return Some(results.read(cx).excel_workbook(
                self.payload.slot,
                self.active_columns(),
                self.active_rows(),
                cx,
            ));
        }
        if format.is_binary() {
            crate::results::export_bytes_for_format(
                format,
                self.active_columns(),
                self.active_rows(),
            )
        } else {
            crate::results::export_text_for_format_with(
                format,
                self.db,
                self.active_columns(),
                self.active_rows(),
                &self.options_for(format),
            )
            .map(String::into_bytes)
        }
    }

    /// Save the export to a file chosen by the user.
    #[cfg(not(target_arch = "wasm32"))]
    fn save_export(&self, format: ExportFormat, cx: &mut App) {
        if format == ExportFormat::Xlsx {
            self.prepare_workbook(cx);
        }
        let Some(bytes) = self.export_file_bytes(format, cx) else {
            return;
        };
        let ext = format.extension();
        let filename = format!("results.{ext}");
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(&filename));
        let results = self.results.clone();
        cx.spawn(async move |cx| {
            if let Ok(Ok(Some(path))) = rx.await
                && let Err(message) = crate::results::write_export_bytes(&path, &bytes, false)
            {
                let _ = results.update(cx, |view, cx| {
                    view.note_export_write_failed_messages(message, cx);
                });
            }
        })
        .detach();
    }

    /// Browser build: there is no save dialog, so the file downloads.
    #[cfg(target_arch = "wasm32")]
    fn save_export(&self, format: ExportFormat, cx: &mut App) {
        if format == ExportFormat::Xlsx {
            self.prepare_workbook(cx);
        }
        let Some(bytes) = self.export_file_bytes(format, cx) else {
            return;
        };
        let filename = format!("results.{}", format.extension());
        if let Err(message) = crate::browser_download::download_bytes(&filename, &bytes)
            && let Some(results) = self.results.upgrade()
        {
            results.update(cx, |view, cx| {
                view.note_export_write_failed_messages(message, cx);
            });
        }
    }

    fn format_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let format = self.formats[index];
        let selected = index == self.selected;
        let this = cx.entity().downgrade();
        div()
            .id(SharedString::from(format!("export-dialog-format-{index}")))
            .flex()
            .items_center()
            .gap(px(Theme::scaled(8.0)))
            .px(px(Theme::scaled(8.0)))
            .py(px(Theme::scaled(3.0)))
            .rounded(px(Theme::scaled(4.0)))
            .cursor_pointer()
            .when(selected, |row| row.bg(Theme::selection_fill()))
            .hover(|row| row.bg(Theme::selection_hover_fill()))
            .on_click(move |_click, _window, app| {
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| this.select_format(index, cx));
                }
            })
            .child(
                div()
                    .w(px(Theme::scaled(14.0)))
                    .text_color(if selected {
                        Theme::accent()
                    } else {
                        Theme::text_dim()
                    })
                    .child(if selected { "●" } else { "○" }),
            )
            .child(
                div()
                    .text_color(if selected {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(format.localized_label()),
            )
            .into_any_element()
    }

    fn destination_row(
        &self,
        id: &'static str,
        label: String,
        save_to_file: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.destination()
            == Some(if save_to_file {
                Destination::File
            } else {
                Destination::Clipboard
            });
        let this = cx.entity().downgrade();
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(Theme::scaled(8.0)))
            .px(px(Theme::scaled(8.0)))
            .py(px(Theme::scaled(3.0)))
            .rounded(px(Theme::scaled(4.0)))
            .when(enabled, |row| {
                row.cursor_pointer()
                    .hover(|row| row.bg(Theme::selection_hover_fill()))
            })
            .when(selected, |row| row.bg(Theme::selection_fill()))
            .on_click(move |_click, _window, app| {
                if !enabled {
                    return;
                }
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| this.select_destination(save_to_file, cx));
                }
            })
            .child(
                div()
                    .w(px(Theme::scaled(14.0)))
                    .text_color(if selected {
                        Theme::accent()
                    } else if enabled {
                        Theme::text_dim()
                    } else {
                        Theme::text_disabled()
                    })
                    .child(if selected { "●" } else { "○" }),
            )
            .child(
                div()
                    .text_color(if enabled {
                        if selected {
                            Theme::text()
                        } else {
                            Theme::text_dim()
                        }
                    } else {
                        Theme::text_disabled()
                    })
                    .child(label),
            )
            .into_any_element()
    }
}

/// Open the Export Results dialog in its own window (same hosting pattern
/// as the conditional-formatting dialog).
pub(crate) fn open_result_export_dialog(
    payload: ExportPayload,
    prefs: &ExportPreferences,
    db: DatabaseType,
    results: WeakEntity<ResultsView>,
    cx: &mut App,
) {
    let bounds = gpui::Bounds::centered(None, gpui::size(px(480.0), px(560.0)), cx);
    let options = gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(crate::localization::text("results.export_dialog.window_title").into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let _ = cx.open_window(options, |window, cx| {
        crate::localization::register_window_title_key(
            window.window_handle(),
            "results.export_dialog.window_title",
        );
        let dialog = cx.new(|cx| ResultExportDialog::new(payload, prefs, db, results, cx));
        cx.new(|cx| gpui_component::Root::new(dialog, window, cx))
    });
}

impl Render for ResultExportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = |v: f32| px(Theme::scaled(v));

        let format_list = if self.formats.is_empty() {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.export_dialog.no_formats_enabled",
                ))
                .child(
                    div()
                        .mt(s(4.0))
                        .text_color(Theme::text_disabled())
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .child(crate::localization::text(
                            "results.export_dialog.enable_formats_instructions",
                        )),
                )
                .into_any_element()
        } else {
            div()
                .id("export-dialog-format-list")
                .flex()
                .flex_col()
                .flex_1()
                .overflow_y_scroll()
                .children((0..self.formats.len()).map(|i| {
                    let mut children: Vec<gpui::AnyElement> = Vec::new();
                    if i == 0 {
                        children.push(
                            div()
                                .px(s(8.0))
                                .pb(s(2.0))
                                .text_color(Theme::text_disabled())
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .child(crate::localization::text(
                                    "results.export_dialog.standard_formats_heading",
                                ))
                                .into_any_element(),
                        );
                    } else if i == self.programming_start
                        && self.programming_start < self.formats.len()
                    {
                        children.push(
                            div()
                                .px(s(8.0))
                                .pt(s(8.0))
                                .pb(s(2.0))
                                .text_color(Theme::text_disabled())
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .child(crate::localization::text(
                                    "results.export_dialog.programming_formats_heading",
                                ))
                                .into_any_element(),
                        );
                    }
                    children.push(self.format_row(i, cx));
                    div()
                        .flex()
                        .flex_col()
                        .children(children)
                        .into_any_element()
                }))
                .into_any_element()
        };

        let export_disabled = self.formats.is_empty();

        // Selected/all scope toggle — only shown when a real selection exists.
        // In "selected" mode it offers to switch to the whole set (an ✕); in
        // "all" mode it offers to return to just the selection (an undo).
        let scope_toggle = self.payload.has_selection.then(|| {
            let selected_only = self.selected_only;
            Button::new("export-dialog-scope-toggle")
                .ghost()
                .icon(if selected_only {
                    IconName::Close
                } else {
                    IconName::Undo
                })
                .app_tooltip_text(if selected_only {
                    crate::localization::text("results.export_dialog.export_all_tooltip")
                } else {
                    crate::localization::text("results.export_dialog.export_selection_tooltip")
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_selected_only(!selected_only, cx);
                }))
        });

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::panel_bg())
            .text_color(Theme::text())
            .p(s(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(s(8.0))
                    .pb(s(8.0))
                    .child(
                        div()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .child(self.payload.shape_label(self.selected_only)),
                    )
                    .children(scope_toggle)
                    .when(
                        self.payload.spill_truncated && self.payload.whole_set,
                        |header| {
                            header.child(
                                div()
                                    .text_color(Theme::text_disabled())
                                    .text_size(px(Theme::font_size_ratio(0.9)))
                                    .child(crate::localization::text(
                                        "results.export_dialog.capped_notice",
                                    )),
                            )
                        },
                    ),
            )
            .child(format_list)
            .child(
                div()
                    .pt(s(10.0))
                    .text_color(Theme::text_disabled())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(crate::localization::text(
                        "results.export_dialog.destination_heading",
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(s(2.0))
                    .pt(s(4.0))
                    .child(self.destination_row(
                        "export-dialog-dest-clipboard",
                        crate::localization::text("results.export_dialog.clipboard_destination"),
                        false,
                        !self.clipboard_destination_disabled(),
                        cx,
                    ))
                    .child(self.destination_row(
                        "export-dialog-dest-file",
                        crate::localization::text("results.export_dialog.file_destination"),
                        true,
                        true,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(s(8.0))
                    .pt(s(12.0))
                    .child(
                        Button::new("export-dialog-spreadsheet")
                            .label(crate::localization::text(
                                "results.export_dialog.open_spreadsheet_button",
                            ))
                            .app_tooltip_text(crate::localization::text(
                                "results.export_dialog.open_spreadsheet_tooltip",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_in_spreadsheet(cx);
                                window.remove_window();
                            })),
                    )
                    .child(
                        Button::new("export-dialog-copy-cli")
                            .label(
                                if self.cli_copied.is_some()
                                    && self.cli_copied == self.cli_command_text(cx)
                                {
                                    crate::localization::text(
                                        "results.export_dialog.cli_copied_button",
                                    )
                                } else {
                                    crate::localization::text(
                                        "results.export_dialog.copy_cli_button",
                                    )
                                },
                            )
                            .app_tooltip_text(crate::localization::text(
                                "results.export_dialog.copy_cli_tooltip",
                            ))
                            .disabled(self.cli_command_text(cx).is_none())
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.copy_cli_command(cx);
                            })),
                    )
                    .child(
                        Button::new("export-dialog-cancel")
                            .label(crate::localization::text(
                                "results.export_dialog.cancel_button",
                            ))
                            .on_click(|_, window, _| window.remove_window()),
                    )
                    .child(
                        Button::new("export-dialog-run")
                            .primary()
                            .label(crate::localization::text(
                                "results.export_dialog.export_button",
                            ))
                            .disabled(export_disabled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.execute(cx);
                                window.remove_window();
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs_with(programming_enabled: bool) -> ExportPreferences {
        ExportPreferences {
            programming_enabled,
            ..Default::default()
        }
    }

    #[test]
    fn dialog_formats_default_prefs_list_standard_only() {
        let formats = dialog_formats(&prefs_with(false));
        assert_eq!(formats.len(), 12, "12 standard formats by default");
        assert_eq!(formats[0], ExportFormat::Csv);
        assert_eq!(formats[11], ExportFormat::SqllyExport);
        assert_eq!(standard_format_count(&formats), 12);
    }

    #[test]
    fn dialog_formats_with_programming_list_all_23() {
        let formats = dialog_formats(&prefs_with(true));
        assert_eq!(formats.len(), 23, "12 standard + 11 languages");
        assert_eq!(standard_format_count(&formats), 12);
        assert_eq!(formats[12], ExportFormat::CSharp);
        assert_eq!(formats[22], ExportFormat::Cpp);
    }

    #[test]
    fn dialog_formats_respect_individual_toggles() {
        let prefs = ExportPreferences {
            csv: false,
            xlsx: false,
            programming_enabled: true,
            rust: false,
            ..Default::default()
        };
        let formats = dialog_formats(&prefs);
        assert!(!formats.contains(&ExportFormat::Csv));
        assert!(!formats.contains(&ExportFormat::Xlsx));
        assert!(!formats.contains(&ExportFormat::Rust));
        assert!(formats.contains(&ExportFormat::Go));
        assert_eq!(formats.len(), 23 - 3);
        assert_eq!(standard_format_count(&formats), 10);
    }

    #[test]
    fn dialog_formats_all_disabled_yield_empty_list() {
        let prefs = ExportPreferences {
            csv: false,
            tsv: false,
            json: false,
            json_lines: false,
            markdown: false,
            html: false,
            xml: false,
            insert: false,
            temp_table: false,
            sql_values: false,
            xlsx: false,
            sqlly_export: false,
            ..Default::default()
        };
        let formats = dialog_formats(&prefs);
        assert!(formats.is_empty(), "all standard formats disabled → empty");
        assert_eq!(standard_format_count(&formats), 0);
    }

    #[test]
    fn clipboard_destination_rejected_for_binary_formats() {
        assert!(destination_allows_clipboard(ExportFormat::Csv));
        assert!(destination_allows_clipboard(ExportFormat::CSharp));
        assert!(!destination_allows_clipboard(ExportFormat::Xlsx));
    }

    fn cols(n: usize) -> Vec<ResultColumn> {
        (0..n)
            .map(|i| ResultColumn {
                name: format!("c{i}"),
                data_type: "int".to_string(),
                nullable: true,
            })
            .collect()
    }

    fn rows(n: usize, cols: usize) -> Vec<Vec<CellValue>> {
        vec![vec![CellValue::Null; cols]; n]
    }

    #[test]
    fn shape_label_reads_naturally() {
        // No selection: the whole set, dims either way.
        let payload = ExportPayload {
            columns: cols(3),
            rows: rows(2, 3),
            all_columns: cols(3),
            all_rows: rows(2, 3),
            has_selection: false,
            spill_truncated: false,
            whole_set: true,
            slot: 0,
        };
        assert_eq!(payload.shape_label(true), "2 rows × 3 columns");
    }

    #[test]
    fn shape_label_singular_units() {
        let payload = ExportPayload {
            columns: cols(1),
            rows: rows(1, 1),
            all_columns: cols(1),
            all_rows: rows(1, 1),
            has_selection: false,
            spill_truncated: false,
            whole_set: true,
            slot: 0,
        };
        assert_eq!(payload.shape_label(true), "1 row × 1 column");
    }

    #[test]
    fn shape_label_selection_vs_all() {
        // A real selection (2×2) narrowing a 10×4 visible set.
        let payload = ExportPayload {
            columns: cols(2),
            rows: rows(2, 2),
            all_columns: cols(4),
            all_rows: rows(10, 4),
            has_selection: true,
            spill_truncated: false,
            whole_set: false,
            slot: 0,
        };
        // Selected mode reports the narrowed dims…
        assert_eq!(payload.shape_label(true), "2 rows × 2 columns");
        // …and "all" mode collapses to the word "All".
        assert_eq!(payload.shape_label(false), "All");
    }
}
