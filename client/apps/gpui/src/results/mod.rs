use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Instant, SystemTime};
#[cfg(target_arch = "wasm32")]
use web_time::{Instant, SystemTime};

use gpui::{
    Anchor, Animation, AnimationExt as _, App, Bounds, Context, CursorStyle, ElementId, Entity,
    EventEmitter, FontWeight, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Render, ScrollEvent, ScrollHandle, SharedString, Styled, Subscription, Task,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, anchored, deferred, div,
    point, prelude::*, px, size,
};
use gpui_component::animation::cubic_bezier;
use gpui_component::button::{Button, ButtonVariants as _};
use sqlly_core::{CellValue, MaskingOverrides, ResultColumn};
use sqlly_datatable::{
    ChartConfig, ColumnKind, GridData, GridState, GridTab, PivotConfig, ResolvedColumnFormat,
    Selection, SqllyDataTable,
};
use sqlly_services::{QueryEvent, TruncatedCell};

use crate::editor::{SqlEditor, ValidationDiagnostic};
use crate::execution_plan_view::{ExecutionPlanView, ExecutionPlanViewEvent, PlanKind};
use crate::gesture_interpreter::{self, ScrollDeltaAccumulator, ScrollDirection};
use crate::grid_bridge::{
    build_grid_config, build_grid_data, build_grid_data_owned_with_formats, build_grid_theme,
    convert_cell, convert_rows_owned_with_formats, grid_cell_to_core,
};
use crate::grid_filter_builder::{
    BuilderOp, ColumnTarget, Conjunction, FilterBuilderModel, FilterGroup, FilterRow, LayoutItem,
};
use crate::grid_sort::SortSpec;
use crate::masking_rules::MaskingRules;
use crate::pane_divider::{CollapseDirection, PaneDivider};
use crate::preferences::{
    ColumnFormat, CopyFormat, ExportPreferences, GridDensity, ResultFormattingPreferences,
    ResultKind,
};
use crate::result_cell_format::{effective_formatting, infer_result_kind_for};
use crate::result_export;
use crate::result_export_code::{self, CodeLanguage};
use crate::result_export_xlsx;
use crate::result_grid_menu::ResultGridMenuProvider;
use crate::result_pivot::{ResultViewHints, ResultViewKind, parse_view_hints};
use crate::result_set_difference;
use crate::selection_stats;
use crate::theme::Theme;
use gpui_component::Disableable as _;
use gpui_component::Icon;
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::select::{Select, SelectEvent, SelectState};

mod cell_copy;
mod chrome;
mod client_stats;
mod diff;
mod excel_export;
mod export;
mod fk_labels;
mod formula_bar;
mod lock;
mod render;
mod result_settings;
mod spill;
mod spill_drill;
pub(crate) use fk_labels::FkLabelHeaderMenuInfo;
pub use lock::LockSnapshot;
pub(crate) use spill::read_spill_column;

/// The row index a slot's backing store uses for a row the grid reports.
///
/// Every grid-reported row — context-menu clicks, row-action icons,
/// selections, [`GridState::resident_row_for_display`] — is *resident-
/// relative* while the grid is windowed over a spill file (`window_offset`
/// is `Some`), but the spill pager and [`ResultsView::source_row_for_slot`]
/// address absolute rows. Outside windowed mode the index is already a grid
/// source index and is returned unchanged. Pass `state.window.map(|w|
/// w.offset)`; callers inside the grid's own update (the context menu) copy
/// the offset out of the `&mut GridState` they are handed.
pub(crate) fn backing_row_for_grid_row(window_offset: Option<usize>, grid_row: usize) -> usize {
    window_offset.map_or(grid_row, |offset| offset.saturating_add(grid_row))
}

/// The `(rows, columns)` a paste may fill: only a contiguous cell range,
/// counted in data columns (the row-marker column, `marker` wide, is not
/// one). Scattered cells and single cells never fill.
pub(crate) fn paste_fill_range(selection: &Selection, marker: usize) -> Option<(usize, usize)> {
    let Selection::CellRange(r1, c1, r2, c2) = *selection else {
        return None;
    };
    let first_col = c1.max(marker);
    (c2 >= first_col).then(|| (r2 - r1 + 1, c2 - first_col + 1))
}

/// Column auto-sizing progress for one result slot.
#[derive(Default)]
struct AutoSizeTrack {
    /// The grid state this was measured against; a new grid starts over.
    state_id: Option<gpui::EntityId>,
    /// Grid rows already measured.
    scanned: usize,
    /// Widest value seen per result column, in display cells.
    widest: Vec<usize>,
    /// The width auto-sizing last set per column; `None` once something else
    /// sized it.
    applied: Vec<Option<f32>>,
    /// Cells a tab occupies when measuring values (the editor's tab width).
    tab_cells: usize,
}

impl AutoSizeTrack {
    /// Start tracking a grid whose result columns currently have `widths`.
    /// A column is auto-managed when it still shows its name-based default
    /// width (a fresh grid, or a remembered layout that never stored an
    /// auto width) or the width this `previous` track set (a rebuilt grid
    /// carries the old grid's widths over). Anything else — a dragged
    /// column, a directive, an older saved layout — is left alone.
    fn seed(
        state_id: gpui::EntityId,
        columns: &[ResultColumn],
        widths: &[Option<f32>],
        previous: &AutoSizeTrack,
        tab_cells: usize,
    ) -> Self {
        let carried = (previous.applied.len() == columns.len()).then_some(&previous.applied);
        let applied = columns
            .iter()
            .enumerate()
            .map(|(col, def)| {
                let width = widths.get(col).copied().flatten()?;
                let default = crate::grid_bridge::column_width_for_name(&def.name);
                let managed = (width - default).abs() <= 0.5
                    || carried
                        .and_then(|applied| applied[col])
                        .is_some_and(|prev| (width - prev).abs() <= 0.5);
                managed.then_some(width)
            })
            .collect();
        Self {
            state_id: Some(state_id),
            scanned: 0,
            widest: vec![0; columns.len()],
            applied,
            tab_cells,
        }
    }

    /// Whether this track set `width` on result column `col` (so the width
    /// is a measurement, not a choice anyone made).
    fn manages(&self, col: usize, width: f32) -> bool {
        self.applied
            .get(col)
            .copied()
            .flatten()
            .is_some_and(|applied| (width - applied).abs() <= 0.5)
    }
}

/// Re-size the columns a `width=…dp` directive sized at `old_font` to the
/// grid's new font size. A column someone dragged since (no longer at its
/// dp width) is left alone.
fn rescale_dp_widths(s: &mut GridState, dps: &[Option<u32>], old_font: f32) {
    let marker = grid_marker_width(&s.data.columns, dps.len());
    let new_font = s.font_size;
    for (col, dp) in dps.iter().enumerate() {
        let (Some(dp), Some(column)) = (dp, s.data.columns.get_mut(col + marker)) else {
            continue;
        };
        let was = crate::query_format::dp_to_px(*dp, old_font);
        if (column.width - was).abs() <= 0.5 {
            column.width = crate::query_format::dp_to_px(*dp, new_font);
        }
    }
}

/// Width (0 or 1) of the staged-edit marker prefix a grid built from
/// `result_columns` result columns actually shows. Grid column `g` is result
/// column `g - width`. Structural — it reads the grid's own columns — so it
/// stays right in the window where `pending` has changed but the grid has not
/// been rebuilt yet, when [`ResultsView::marker_offset`] would already
/// disagree with what is on screen.
pub(crate) fn grid_marker_width(
    grid_columns: &[sqlly_datatable::Column],
    result_columns: usize,
) -> usize {
    usize::from(
        grid_columns.len() == result_columns + 1
            && grid_columns
                .first()
                .is_some_and(|c| c.name == crate::grid_bridge::MARKER_COLUMN_NAME),
    )
}

/// Line a result-column `GridConfig` up with a grid that leads with `marker`
/// staged-edit marker columns: `ColumnOverride`s are positional, so without
/// this every column would take its left neighbour's format while edits are
/// pending (ids shown with the name column's format, and so on).
pub(crate) fn align_config_to_marker(config: &mut sqlly_datatable::GridConfig, marker: usize) {
    if marker == 0 || config.column_overrides.is_empty() {
        return;
    }
    config.column_overrides.splice(
        0..0,
        std::iter::repeat_n(sqlly_datatable::ColumnOverride::default(), marker),
    );
}

/// The per-result-column type snapshot re-keyed by grid column: a staged-
/// edit marker column at index 0 shifts every result column by one, so the
/// menu's lookups index by what the request calls the column.
fn shift_column_types(types: Vec<String>, marker: usize) -> Vec<String> {
    std::iter::repeat_n(String::new(), marker)
        .chain(types)
        .collect()
}

/// Re-key FK-label header info from result columns to the grid columns the
/// header menu reports (`marker` staged-edit marker columns lead the grid).
fn shift_fk_headers(
    headers: std::collections::HashMap<usize, FkLabelHeaderMenuInfo>,
    marker: usize,
) -> std::collections::HashMap<usize, FkLabelHeaderMenuInfo> {
    headers
        .into_iter()
        .map(|(col, info)| (col + marker, info))
        .collect()
}

/// A grid selection re-expressed in result columns: columns shift left by
/// the marker width and the marker column itself drops out. Rows are
/// untouched (the marker is a column, never a row).
pub(crate) fn selection_in_result_columns(
    selection: &sqlly_datatable::Selection,
    marker: usize,
) -> sqlly_datatable::Selection {
    use sqlly_datatable::Selection;
    if marker == 0 {
        return selection.clone();
    }
    let col = |c: usize| c.checked_sub(marker);
    match selection {
        Selection::Cell(r, c) => match col(*c) {
            Some(c) => Selection::Cell(*r, c),
            None => Selection::None,
        },
        Selection::Column(c) => col(*c).map_or(Selection::None, Selection::Column),
        Selection::Columns(cols) => {
            let cols: Vec<usize> = cols.iter().filter_map(|c| col(*c)).collect();
            if cols.is_empty() {
                Selection::None
            } else {
                Selection::Columns(cols)
            }
        }
        Selection::CellRange(r1, c1, r2, c2) => {
            let (lo, hi) = ((*c1).min(*c2), (*c1).max(*c2));
            match col(hi) {
                None => Selection::None,
                Some(hi) => {
                    let lo = lo.saturating_sub(marker);
                    Selection::CellRange(*r1, lo, *r2, hi)
                }
            }
        }
        Selection::Cells(cells) => {
            let cells: Vec<(usize, usize)> = cells
                .iter()
                .filter_map(|(r, c)| col(*c).map(|c| (*r, c)))
                .collect();
            if cells.is_empty() {
                Selection::None
            } else {
                Selection::Cells(cells)
            }
        }
        other => other.clone(),
    }
}

/// The inverse of [`selection_in_result_columns`]: a selection over result
/// columns, re-expressed for a grid led by `marker` staged-edit columns.
pub(crate) fn selection_in_grid_columns(
    selection: &sqlly_datatable::Selection,
    marker: usize,
) -> sqlly_datatable::Selection {
    use sqlly_datatable::Selection;
    if marker == 0 {
        return selection.clone();
    }
    match selection {
        Selection::Cell(r, c) => Selection::Cell(*r, c + marker),
        Selection::Column(c) => Selection::Column(c + marker),
        Selection::Columns(cols) => Selection::Columns(cols.iter().map(|c| c + marker).collect()),
        Selection::CellRange(r1, c1, r2, c2) => {
            Selection::CellRange(*r1, c1 + marker, *r2, c2 + marker)
        }
        Selection::Cells(cells) => {
            Selection::Cells(cells.iter().map(|(r, c)| (*r, c + marker)).collect())
        }
        other => other.clone(),
    }
}

/// What a grid Replace All did NOT cover, for Messages. `window` is the
/// windowed grid's `(offset, resident_rows, total_rows)` — only resident rows
/// are searched there — and `capped_at` is the match count when the grid's
/// search stopped at its cap. `None` when every row was searched.
fn replace_all_coverage_note(
    window: Option<(usize, usize, usize)>,
    capped_at: Option<usize>,
) -> Option<String> {
    let window = window.and_then(|(offset, resident, total)| {
        let end = offset.saturating_add(resident).min(total);
        (offset > 0 || end < total).then_some((offset, end, total))
    });
    let window_arguments = || {
        let (offset, end, total) = window.unwrap_or((0, 0, 0));
        [
            ("loaded_rows", end.saturating_sub(offset).to_string()),
            ("first_row", (offset + 1).to_string()),
            ("last_row", end.to_string()),
            ("total_rows", total.to_string()),
        ]
    };
    match (window, capped_at) {
        (Some(_), Some(count)) => {
            let [(_, loaded), (_, first), (_, last), (_, total)] = window_arguments();
            Some(crate::localization::format(
                "results.find_replace.window_and_cap_coverage_note",
                &[
                    ("loaded_rows", loaded.as_str()),
                    ("first_row", first.as_str()),
                    ("last_row", last.as_str()),
                    ("total_rows", total.as_str()),
                    ("match_limit", count.to_string().as_str()),
                ],
            ))
        }
        (Some(_), None) => {
            let [(_, loaded), (_, first), (_, last), (_, total)] = window_arguments();
            Some(crate::localization::format(
                "results.find_replace.window_coverage_note",
                &[
                    ("loaded_rows", loaded.as_str()),
                    ("first_row", first.as_str()),
                    ("last_row", last.as_str()),
                    ("total_rows", total.as_str()),
                ],
            ))
        }
        (None, Some(count)) => Some(crate::localization::format(
            "results.find_replace.match_cap_coverage_note",
            &[("match_limit", &count.to_string())],
        )),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests;

pub use chrome::*;
pub use diff::*;
pub use export::*;
pub(crate) use render::*;
use spill::*;

/// Result layout mode — how multiple result sets are displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// One tab per result set (the default).
    #[default]
    Tabbed,
    /// All result sets as collapsible stacked cards.
    Stacked,
    /// Canvas grid placement via SQL comments.
    Canvas,
}

/// Display format for timing values in the results footer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TimingFormat {
    /// Compact: `Nms` or `N.NNs`.
    #[default]
    Duration,
    /// Wall-clock: `hh:MM:ss.SSS`.
    Elapsed,
}

/// State for the "Save as Snapshot…" name prompt overlay.
struct SnapshotPrompt {
    slot: usize,
    input: Option<Entity<InputState>>,
    error: Option<String>,
    _subscription: Option<gpui::Subscription>,
}

/// State for the read-only single-record form overlay.
struct RowFormState {
    slot: usize,
    row_index: usize,
    row_count: usize,
    columns: Vec<ResultColumn>,
    cells: Vec<CellValue>,
}

/// The native-window presentation of a popped-out row form. It keeps the
/// original results entity as its row provider, so previous/next stays
/// spill-aware without copying an entire result set into the new window.
struct PoppedOutRowForm {
    source: Entity<ResultsView>,
    form: RowFormState,
}

impl PoppedOutRowForm {
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(next) = self.form.row_index.checked_add_signed(delta) else {
            return;
        };
        if next >= self.form.row_count {
            return;
        }
        let cells = self.source.read(cx).form_row(self.form.slot, next, cx);
        if let Some(cells) = cells {
            self.form.row_index = next;
            self.form.cells = cells;
            cx.notify();
        }
    }
}

impl Render for PoppedOutRowForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let title = crate::localization::format(
            "results.row_form.detached_row_position",
            &[
                ("row_number", &(self.form.row_index + 1).to_string()),
                ("total_rows", &self.form.row_count.to_string()),
            ],
        );
        let can_prev = self.form.row_index > 0;
        let can_next = self.form.row_index + 1 < self.form.row_count;
        let mut rows = div()
            .id("popped-row-form-fields")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for (i, column) in self.form.columns.iter().enumerate() {
            let cell = self.form.cells.get(i).cloned().unwrap_or(CellValue::Null);
            let is_null = matches!(cell, CellValue::Null);
            let display = crate::procedure_mode::cell_display(&cell);
            let copy_text = display.clone();
            rows = rows.child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(s(10.0)))
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .w(px(s(180.0)))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_color(Theme::text())
                                    .truncate()
                                    .child(column.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.72)))
                                    .text_color(Theme::text_dim())
                                    .child(column.data_type.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_color(if is_null {
                                Theme::text_dim()
                            } else {
                                Theme::text()
                            })
                            .whitespace_normal()
                            .child(if is_null { "NULL".to_string() } else { display }),
                    )
                    .child(
                        Button::new(("popped-row-form-copy", i))
                            .label(crate::localization::text(
                                "results.row_form.copy_value_button",
                            ))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |_this, _ev, _window, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    copy_text.clone(),
                                ));
                            })),
                    ),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Theme::popup_bg())
            .font(Theme::ui_font())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(div().text_color(Theme::text()).child(title))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(
                                Button::new("popped-row-form-prev")
                                    .label("◀")
                                    .ghost()
                                    .xsmall()
                                    .disabled(!can_prev)
                                    .on_click(
                                        cx.listener(|this, _ev, _window, cx| this.step(-1, cx)),
                                    ),
                            )
                            .child(
                                Button::new("popped-row-form-next")
                                    .label("▶")
                                    .ghost()
                                    .xsmall()
                                    .disabled(!can_next)
                                    .on_click(
                                        cx.listener(|this, _ev, _window, cx| this.step(1, cx)),
                                    ),
                            ),
                    ),
            )
            .child(rows)
    }
}

/// Per-row input entities for the visual filter builder overlay.
struct FilterBuilderInputs {
    a: Option<Entity<InputState>>,
    b: Option<Entity<InputState>>,
}

impl FilterBuilderState {
    /// Drop the per-row inputs and group dropdowns after the tree changed
    /// shape; the next render rebuilds them from the model.
    fn reset_tree_widgets(&mut self) {
        self.inputs.clear();
        self.group_conjunction_selects.clear();
        self.root_conjunction_select = None;
    }
}

/// Messages line for a grid export whose result holds cells that were cut to
/// previews at fetch time.
pub(crate) fn preview_cells_export_message(cut: usize) -> String {
    crate::localization::format(
        if cut == 1 {
            "results.export.preview_cell_warning_singular"
        } else {
            "results.export.preview_cells_warning"
        },
        &[("preview_cell_count", &cut.to_string())],
    )
}

fn pending_changes_elsewhere_message(change_count: usize, result_number: usize) -> String {
    crate::localization::format(
        if change_count == 1 {
            "results.staged_changes.resolve_other_result_first_one"
        } else {
            "results.staged_changes.resolve_other_result_first"
        },
        &[
            ("change_count", &change_count.to_string()),
            ("result_number", &result_number.to_string()),
        ],
    )
}

/// What a background column scan of a windowed result is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnScanPurpose {
    /// Copy Distinct Values (`top: None`) or Copy Top N Values.
    CopyDistinct { top: Option<usize> },
    /// Copy Column Profile.
    Profile,
}

impl ColumnScanPurpose {
    /// Rows read from the spill file — the same caps the synchronous
    /// versions used, so the copied text is unchanged.
    fn row_cap(self) -> usize {
        match self {
            Self::CopyDistinct { .. } => result_export::EXPORT_MAX_ROWS,
            Self::Profile => crate::result_profiler::PROFILE_MAX_ROWS.saturating_add(1),
        }
    }

    fn caption(self, column: &str, rows: usize) -> String {
        crate::localization::format(
            match self {
                Self::CopyDistinct { .. } => "results.column_scan.count_distinct_caption",
                Self::Profile => "results.column_scan.profile_caption",
            },
            &[("column_name", column), ("row_count", &rows.to_string())],
        )
    }

    /// Turn the scanned cells into clipboard text (`None` when there is
    /// nothing to copy). Runs on the background executor.
    fn finish(
        self,
        column: &ResultColumn,
        cells: Vec<CellValue>,
        formatting: &crate::preferences::ResultFormattingPreferences,
    ) -> ColumnScanOutcome {
        let read_rows = cells.len();
        let rows: Vec<Vec<CellValue>> = cells.into_iter().map(|cell| vec![cell]).collect();
        match self {
            Self::CopyDistinct { top } => {
                let rows = crate::result_cell_format::apply_export_masks(
                    std::slice::from_ref(column),
                    rows,
                    formatting,
                );
                let values: Vec<CellValue> = rows
                    .into_iter()
                    .map(|mut row| row.pop().unwrap_or(CellValue::Null))
                    .collect();
                let text = match top {
                    Some(n) => crate::result_grid_menu::top_distinct_column_lines(&values, n),
                    None => crate::result_grid_menu::distinct_column_lines(&values),
                };
                ColumnScanOutcome {
                    text: (!text.is_empty()).then_some(text),
                    read_rows,
                    report_unread: true,
                }
            }
            Self::Profile => {
                let mut profiler =
                    crate::result_profiler::ResultProfiler::new(std::slice::from_ref(column));
                profiler.push_rows(&rows);
                ColumnScanOutcome {
                    text: Some(profiler.finish().to_markdown()),
                    read_rows,
                    report_unread: false,
                }
            }
        }
    }
}

/// What a finished background column scan hands back to the UI thread.
#[derive(Debug)]
pub(crate) struct ColumnScanOutcome {
    text: Option<String>,
    read_rows: usize,
    /// Say in Messages when rows past the cap were not read.
    report_unread: bool,
}

/// The one in-flight background spill read — a column scan (Copy Distinct,
/// Profile) or a row prefix for an export, snapshot, profile pane or tab
/// move. A newer read replaces it; its caption shows above the grid.
struct BackgroundRead {
    slot: usize,
    generation: u64,
    caption: String,
    _task: Task<()>,
}

/// The first `limit` rows of windowed `slot`, read in the background while
/// the result at `epoch` held `total` rows — exactly what
/// [`ResultsView::spill_rows_for_export`] would have read synchronously.
struct SpillPrefix {
    slot: usize,
    limit: usize,
    total: usize,
    epoch: u64,
    rows: Vec<Vec<CellValue>>,
}

/// Whether the results view may write column layouts back to disk.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LayoutStoreMode {
    /// Read-modify-write `column-layouts.json`.
    Disk,
    /// The file on disk could not be read or parsed. Layouts still apply for
    /// this session from memory, but nothing is written: the next save would
    /// replace the user's file with only this session's layouts.
    Blocked(String),
    /// Tests: the injected store is the whole truth and nothing touches disk.
    #[cfg(test)]
    MemoryOnly,
}

/// The last filter-builder model applied to a slot, with the result column
/// names its indices point into (so a rebuilt grid can re-target it).
/// Model indices are result-column indices, never grid columns: the staged
/// edit marker column is not part of the model.
#[derive(Clone, Debug)]
struct AppliedFilter {
    model: FilterBuilderModel,
    columns: Vec<String>,
}

/// Caption shown when a quick filter is refused on a windowed result.
fn windowed_quick_filter_notice() -> String {
    crate::localization::text("results.filters.windowed_quick_filter_notice")
}

/// Shown when "Toggle default" is used on a multi-statement batch.
fn batch_default_notice() -> String {
    crate::localization::text("results.filters.batch_default_notice")
}

/// Caption for a saved filter whose conditions named columns this result
/// does not have.
fn missing_columns_notice(name: &str, missing: &[String]) -> String {
    let list = missing
        .iter()
        .map(|column| format!("“{column}”"))
        .collect::<Vec<_>>()
        .join(", ");
    let noun = crate::localization::text(if missing.len() == 1 {
        "results.filters.missing_column_noun"
    } else {
        "results.filters.missing_columns_noun"
    });
    crate::localization::format(
        "results.filters.missing_columns_notice",
        &[
            ("filter_name", name),
            ("column_list", &list),
            ("noun", &noun),
        ],
    )
}

/// Visual filter-builder overlay: AND-joined conditions written into
/// `GridState.filters` (Apply) or the editor WHERE clause (Apply to Query).
pub(crate) struct FilterBuilderState {
    slot: usize,
    pub(crate) model: FilterBuilderModel,
    inputs: Vec<FilterBuilderInputs>,
    root_conjunction_select: Option<Entity<SelectState<Vec<String>>>>,
    /// Conjunction dropdown per nested group, keyed by the group's path
    /// (see [`FilterBuilderModel::layout`]); cleared whenever the tree's
    /// shape changes.
    group_conjunction_selects: HashMap<Vec<usize>, Entity<SelectState<Vec<String>>>>,
    column_kinds: Vec<ColumnKind>,
    column_names: Vec<String>,
    result_columns: Vec<ResultColumn>,
    spill_backed: bool,
    /// Saved-filter scope for the slot; `None` without a connection scope
    /// or a query, which hides the saved-filters row.
    scope: Option<crate::saved_filters::FilterScope>,
    /// Saved-filters dropdown, materialized at render time. Items carry the
    /// filter's name as their value, so labels never have to be parsed back.
    saved_select: Option<Entity<SelectState<Vec<SavedFilterOption>>>>,
    /// Saved-filter store error (unreadable file, failed write), shown in
    /// the saved row. A corrupt store is never saved over.
    saved_error: Option<String>,
    /// The name the saved-filters dropdown last confirmed.
    saved_chosen: Option<String>,
    /// Name field for Save / Rename, materialized at render time.
    saved_name_input: Option<Entity<InputState>>,
    /// Filter-expression field (the DbGate-style mini-language).
    expression_input: Option<Entity<InputState>>,
    /// Grid columns that are dates in a non-ISO display format; date
    /// comparisons on them are flagged in the builder.
    non_iso_date_columns: Vec<usize>,
    /// Parse error shown in red under the expression field; the model is
    /// left untouched while one is present.
    expression_error: Option<String>,
    /// Text the expression field is (re)created with — set when a saved
    /// filter that carries an expression is applied.
    expression_seed: Option<String>,
}

/// One memoized footer-stats result (see `ResultsView::selection_stats_cache`).
struct SelectionStatsEntry {
    selection: sqlly_datatable::Selection,
    row_count: usize,
    text: Option<String>,
}

pub struct ResultsView {
    result_sets: Vec<ResultSet>,
    active_tab: usize,
    messages: Vec<String>,
    /// Show the messages pane instead of the result grid.
    ///
    /// Messages used to render only when there were no result sets at all, so a
    /// batch like `SELECT …; PRINT 'done'` displayed "Messages (1)" in the
    /// footer with no way whatsoever to read it. The count is now a control.
    show_messages: bool,
    /// SQL validation diagnostics (unknown tables, lint warnings, …). These no
    /// longer have their own tab: they are folded into the Messages pane text
    /// alongside the run's execution output. Kept as structured data so the
    /// count badge and editor squiggles can still use them.
    diagnostics: Vec<ValidationDiagnostic>,
    /// Byte offset in the editor of the last located execution error, if any.
    execution_error_offset: Option<usize>,
    elapsed_ms: Option<u64>,
    /// Instant when `start_query` was called — the zero point for
    /// server-round-trip timing.
    query_start: Option<Instant>,
    /// Time from query submit to the first server-data event
    /// (`ResultSet`, `Rows`, `RowCount`, or `Message`). `None` until the
    /// first such event arrives.
    first_response_ms: Option<u64>,
    /// Time from query submit to `Completed` — the full round-trip
    /// including all row streaming. `None` until `Completed` arrives.
    response_finished_ms: Option<u64>,
    /// Instant when `Completed` arrived — used by `render()` to detect the
    /// first paint after completion and record `ui_paint_ms`.
    completed_at: Option<Instant>,
    /// Wall-clock time of `Completed`, used by the run-history strip.
    completed_wall: Option<SystemTime>,
    /// Wall-clock time the current run started, for the Messages pane's
    /// "Query started" line. `query_start` is an `Instant` and cannot be
    /// turned back into a clock reading.
    run_started_wall: Option<SystemTime>,
    /// Time from `completed_at` to the first `render()` after `Completed` —
    /// the time it took to finish rendering the results on screen.
    ui_paint_ms: Option<u64>,
    /// Wall-clock time = Server Total + Paint. `Server Total` is
    /// `first_response_ms + elapsed_ms`. `None` until paint occurs.
    wall_clock_ms: Option<u64>,
    /// Estimated round-trip latency to the server, measured by running a
    /// `SELECT 1` probe. Persists across queries (not reset by `clear()`).
    estimated_latency_ms: Option<u64>,
    /// When true, displayed timing values subtract `estimated_latency_ms`
    /// to show server-side time excluding network overhead.
    subtract_latency: bool,
    /// Display format for timing values — toggled by a checkbox in the
    /// footer. Persists across queries (not reset by `clear()`).
    timing_format: TimingFormat,
    is_running: bool,
    error: Option<String>,
    /// Set while the grid shows restored query-history results: the banner
    /// text ("Viewing results from …"). Historical results are read-only —
    /// the row editor refuses to open and the next run clears the banner.
    historical_banner: Option<String>,
    pub(crate) formatting: Option<ResultFormattingPreferences>,
    masking_rules: Option<MaskingRules>,
    /// Disk-loaded rules used while `masking_rules` is unset. Loaded once:
    /// `effective_formatting_for_slot` runs on render paths, and reading the
    /// rules file on every frame showed up in scroll profiles.
    masking_rules_fallback: std::cell::OnceCell<MaskingRules>,
    /// Footer selection stats per slot, memoized on the grid selection they
    /// were computed from. Render asks every frame; a whole-column selection
    /// otherwise re-clones and re-aggregates every value per frame.
    selection_stats_cache: std::cell::RefCell<HashMap<usize, SelectionStatsEntry>>,
    masking_overrides: MaskingOverrides,
    revealed: Vec<HashSet<usize>>,
    revealed_cells: Vec<HashSet<(usize, usize)>>,
    /// Per-slot semantic column profile from the optional decision model
    /// (plan 294): `None` until the background profile lands. Parallel to
    /// `result_sets`; dropped by [`Self::clear`] with the rows.
    column_profiles: Vec<Option<Vec<(crate::column_profile::SemanticType, f32)>>>,
    /// Slots whose profile has been attempted — a slot is profiled at most
    /// once, whether or not a provider was configured or the request
    /// succeeded.
    column_profile_requested: Vec<bool>,
    /// Per-slot columns masked by accepting the toolbar suggestion. Injected
    /// into [`Self::effective_formatting_for_slot`] as `ResultKind::Mask`
    /// overrides, the same mechanism `masked_columns` reads.
    suggested_mask_accepted: Vec<HashSet<usize>>,
    /// Per-slot dismissal of the mask-suggestion chip.
    mask_suggestion_dismissed: Vec<bool>,
    /// Per-slot chart suggestion from the decision model. Lowest precedence:
    /// a saved sidebar layout or a `-- sqlly chart:` directive always wins.
    chart_suggestions: Vec<Option<ChartConfig>>,
    /// Slots whose chart suggestion has been attempted — at most once per
    /// slot, whether or not a provider was configured or the request
    /// succeeded.
    chart_suggestion_requested: Vec<bool>,
    /// Slots whose suggestion was reverted — pins "no suggestion" so the
    /// chart never re-suggests after the user said no.
    chart_suggestion_reverted: Vec<bool>,
    /// Tests pin a copy format so Cmd+C / Copy assertions don't depend on
    /// the developer's persisted export.json.
    copy_format_override: Option<CopyFormat>,
    /// Results → Performance: when false (the default) the per-query timing
    /// breakdown (First / Query / Server / Paint / Wall / Latency, the
    /// latency-subtract toggle, and the hh:MM:ss toggle) is hidden from the
    /// status bar; only the row counts remain. Pushed from the App.
    show_detailed_timing: bool,
    /// Snapshot of Results → Performance paging prefs. Loaded at construct
    /// and refreshed when the preferences dialog saves — never re-read from
    /// disk on the per-scroll paging path.
    perf_prefs: crate::preferences::PerformancePreferences,
    /// Snapshot of gesture prefs for stacked/tabbed scroll handlers. Loaded at
    /// construct and refreshed when the preferences dialog saves.
    gesture_prefs: crate::gesture_prefs::GesturePreferences,
    /// Parsed `-- sqlly format:` hints for the current `query_sql`.
    format_hints: Vec<crate::query_format::QueryFormatHint>,
    /// `-- sqlly grid` lines from the query (column arrangement per result).
    grid_directives: Vec<crate::grid_directive::GridDirective>,
    /// `-- sqlly conditional:` rules from the query, ahead of saved rules.
    conditional_directives: Vec<sqlly_datatable::ConditionalRule>,
    /// Slots whose grid already received this run's `grid:` / `width=`
    /// directives — later rebuilds keep what the user changed since.
    grid_directives_applied: HashSet<usize>,
    /// The lock was written into the query since these results arrived.
    locked_by_click: bool,
    /// `-- sqlly column:` computed columns from the query.
    computed_specs: Vec<crate::computed_columns::ComputedColumn>,
    /// Per result slot: its computed columns' plan, when any apply.
    computed_plans: Vec<Option<crate::computed_columns::ComputedPlan>>,
    /// The `@name` values formulas read, set by the host for each run.
    formula_variables: Vec<(String, crate::formula::Value)>,
    /// TODAY()/NOW() for this run's formulas.
    formula_now: f64,
    /// The open formula bar (Add Computed Column… / Edit Formula…).
    formula_bar: Option<formula_bar::FormulaBar>,
    /// Canvas layout parse, keyed by result-set count. Cleared when `query_sql` is set.
    canvas_layout: Option<(usize, crate::result_layout::QueryResultLayoutSpec)>,
    /// The script's `-- sqlly layout:` / `-- sqlly lines:` directives, parsed
    /// when `query_sql` is set: the layout the run opens in and the rows each
    /// stacked card is allotted.
    layout_directives: crate::result_layout::LayoutDirectives,
    show_export_menu: bool,
    /// What the last toolbar export reported. Retained because the toast it
    /// is raised in cannot be observed from a test, and every other export
    /// outcome in this view is already inspectable.
    last_export_notice: Option<String>,
    /// One grid view entity per result set, lazily created when columns
    /// arrive. The view wraps an `Entity<GridState>` which carries the data,
    /// config, selection, sort, and filter state.
    grid_views: Vec<Option<Entity<SqllyDataTable>>>,
    /// Font-size multiplier for result grids (1.0 == density's 100% size).
    grid_zoom: f32,
    /// Leftover Cmd/Ctrl-wheel delta in line units.
    zoom_wheel_accumulator: f32,
    /// Leftover pinch magnification until it crosses a zoom step.
    pinch_zoom_accumulator: f32,
    /// Transposed snapshot grid per slot (`Column` + `Row N` columns). Parallel
    /// to `grid_views`; present only while [`Self::transpose_active`] is true.
    pub(crate) transposed: Vec<Option<Entity<SqllyDataTable>>>,
    /// Whether the slot is showing the transposed snapshot instead of the
    /// source grid.
    pub(crate) transpose_active: Vec<bool>,
    /// True when the snapshot dropped source rows past [`crate::grid_transpose::TRANSPOSE_MAX_ROWS`].
    transpose_truncated: Vec<bool>,
    /// Source-row count turned into columns in the snapshot (after the cap).
    transpose_row_count: Vec<usize>,
    /// Per-slot caption for a saved default filter that could not be applied:
    /// spill-backed result sets cannot take client filters, so the skip is
    /// shown here instead of failing silently. Empty = no limitation.
    filter_limitations: Vec<String>,
    /// Slots whose fresh grid already had its saved default filter
    /// consulted. A later rebuild of the same result (staged edits, marker
    /// overlays) restores the filter the user last applied instead of
    /// re-applying the default over it. Cleared with the results.
    default_filter_consulted: HashSet<usize>,
    /// Bumped every time the results are cleared (a new run, a restored
    /// run). Detached helpers such as the Top Values window capture it and
    /// refuse to act on a different result.
    result_epoch: u64,
    /// True while `install_spill_backing` is building the slot's grid. The
    /// backing lands moments after the build, so the saved-default pass must
    /// treat the slot as spill-backed already.
    spill_install_pending: bool,
    /// Number of rows from each result set already pushed into the
    /// corresponding grid's `GridData`. Parallel to `grid_views`. Lets
    /// `refresh_grid_data` append only the rows that arrived since the last
    /// refresh instead of rebuilding the whole `GridData` per batch (which is
    /// O(total_rows) per batch → O(n²) across a streaming result set).
    synced_rows: Vec<usize>,
    /// Per-slot column auto-sizing progress (see [`Self::auto_size_columns`]).
    auto_size: HashMap<usize, AutoSizeTrack>,
    /// Weak reference to the SQL editor, used by the context-menu provider
    /// for WHERE-clause insertion.
    editor: Option<WeakEntity<SqlEditor>>,
    /// Current layout mode (tabbed / stacked / canvas).
    layout_mode: LayoutMode,
    /// The SQL text of the most recent query, used to parse result-set names
    /// and canvas layout from SQL comments.
    query_sql: Option<String>,
    /// Mirror of the owning tab's auto-refresh interval, for the status strip.
    auto_refresh_secs: Option<u64>,
    /// 1s repaint driver while a query is running so the strip elapsed timer ticks.
    elapsed_ticker: Option<Task<()>>,
    /// Lazy COUNT(*) totals keyed by result-set slot.
    total_row_counts: HashMap<usize, TotalCount>,
    /// Parsed result-set names (0-based index → name) from
    /// `-- sqlly name n: text`
    /// SQL comments. Updated when `query_sql` is set.
    result_set_names: BTreeMap<usize, String>,
    /// Fan-out labels keyed by remapped result-set index (`target * 1000 + local`).
    index_labels: BTreeMap<usize, String>,
    /// Collapsed state for each card in stacked layout. Index = result set slot.
    collapsed_cards: Vec<bool>,
    /// The engine of the connection that produced these results, so
    /// result-driven SQL generation (WHERE actions, SQL exports) renders in
    /// the dialect the query actually ran against. Set at execution start;
    /// defaults to SQL Server for views that never ran a query.
    /// The engine of the connection that produced the current results,
    /// once one is known: `None` until [`Self::set_source_db_type`] ran,
    /// which classifies type names engine-independently and renders
    /// generated SQL in the default dialect. One field, read by every
    /// path, so the grid, the WHERE menu and the executed action never
    /// disagree about the engine.
    source_db_type: Option<sqlly_lang::sqlmodel::DatabaseType>,
    /// Server/database/default-schema scope for persisted column layouts.
    layout_scope: Option<crate::column_layout::LayoutScope>,
    /// Test-only replacement for the saved-filter store source, so default-
    /// filter tests never touch the real `saved-filters.json`. Production
    /// paths ignore this and load from disk.
    saved_filter_store_for_test: Option<crate::saved_filters::SavedFilterStore>,
    /// Lazy-loaded column layout store (mirrors disk).
    layout_store: Option<crate::column_layout::ColumnLayoutStore>,
    /// Whether layout changes may be written back to disk.
    layout_store_mode: LayoutStoreMode,
    /// The in-flight background spill read (column scan or row prefix on a
    /// windowed result), shown in the grid caption while it runs.
    background_read: Option<BackgroundRead>,
    /// Per-slot drill-through over a spill-backed result: the parked full
    /// result while a filtered file is shown, and the drill being written.
    spill_drills: Vec<Option<spill_drill::SpillDrill>>,
    /// Generation token for drills; a finished drill applies only if it is
    /// still the slot's current one.
    spill_drill_generation: u64,
    /// Generation token for `background_read`; bumped per read.
    background_read_generation: u64,
    /// Row prefixes read off the UI thread for windowed slots, waiting for
    /// the export/snapshot/profile code that asked for them. Each is taken
    /// by the first matching [`ResultsView::spill_rows_for_export`] call.
    spill_prefixes: std::cell::RefCell<Vec<SpillPrefix>>,
    /// Per-slot baseline layout for change detection and debounced save.
    applied_layouts: Vec<Option<crate::column_layout::ColumnLayout>>,
    /// Per-slot debounce generation for layout saves.
    layout_save_generation: Vec<u64>,
    /// Connection + services for FK label discovery/fetch.
    fk_label_profile: Option<sqlly_core::ConnectionProfile>,
    fk_label_services: Option<sqlly_services::Services>,
    /// Connection-level row-edit verdict for the run behind these results
    /// ([`crate::row_edit_gate::effective_connection_block_reason`]): `None`
    /// when never told, `Some(None)` editable, `Some(Some(reason))` refused.
    /// Hides the grid's row-header pencil where opening the editor would fail.
    row_edit_connection_block: Option<Option<String>>,
    fk_label_cache: crate::fk_labels::LabelCache,
    fk_labels_by_slot: Vec<Option<fk_labels::FkSlotState>>,
    fk_label_fetch_generation: Vec<u64>,
    /// When true, track the previous query's result sets and show a diff
    /// tab after the next execution.
    track_difference: bool,
    /// Saved result sets from the previous execution (for diffing).
    previous_result_sets: Vec<ResultSet>,
    /// The computed diff for the active result set, if difference tracking
    /// is on and a diff has been computed.
    diff: Option<result_set_difference::ResultSetDiff>,
    /// Whether the diff tab is currently shown.
    show_diff: bool,
    /// Execution plan pane (findings-first view). Created on the first
    /// Explain; survives query runs so the user can compare plan vs results.
    execution_plan_view: Option<Entity<ExecutionPlanView>>,
    /// Which plan kinds the active engine can capture, pushed by the app and
    /// handed to the plan pane (now, or when it is first created).
    plan_capture_gate: crate::capabilities::PlanCaptureGate,
    /// Whether the plan pane is shown instead of the grid.
    show_execution_plan: bool,
    /// History output tab (query version timeline). Content is the per-tab
    /// `history_panel` entity bound by the host.
    show_history: bool,
    /// DDL output tab. Content is the per-tab `ddl_panel` entity bound by the host.
    show_ddl: bool,
    #[cfg(not(target_arch = "wasm32"))]
    history_panel: Option<Entity<crate::history_panel::HistoryPanelView>>,
    ddl_panel: Option<Entity<crate::ddl_viewer::DdlViewerView>>,
    /// Scroll handle for the stacked results container, used to track edge
    /// state for gesture-based card snapping.
    stacked_scroll_handle: ScrollHandle,
    /// User-resized total card heights. `None` retains adaptive sizing until
    /// the divider after that result set is dragged.
    stacked_card_heights: Vec<Option<f32>>,
    stacked_resize: Option<StackedResize>,
    /// Accumulated scroll deltas for gesture activation (small trackpad
    /// gestures build up before triggering a snap).
    scroll_accumulator: ScrollDeltaAccumulator,
    /// One-switch-per-gesture latch for edge-triggered result-set switching.
    tab_switch_gate: gesture_interpreter::EdgeSwitchGate,
    /// The open right-click menu on a result's name, if any.
    result_name_menu: Option<result_settings::ResultNameMenu>,
    /// The open Result Settings dialog (the name's pencil), if any.
    result_settings: Option<result_settings::ResultSettingsDialog>,
    diff_direction: DiffDirection,
    diff_grid_previous: Option<Entity<SqllyDataTable>>,
    diff_grid_current: Option<Entity<SqllyDataTable>>,
    diff_grid_diff: Option<Entity<SqllyDataTable>>,
    retained_diff_runs: Vec<RetainedDiffRun>,
    next_diff_run_id: u64,
    diff_baseline_run: Option<u64>,
    diff_comparison_run: Option<u64>,
    /// Sibling of the diff ring: user-facing run history (not merged with
    /// `retained_diff_runs` / `DIFF_MAX_RUNS`).
    retained_runs: Vec<RetainedRun>,
    next_run_id: u64,
    /// `None` means the live / current run is on screen.
    viewing_run: Option<u64>,
    current_run_snapshot: Option<RetainedRun>,
    run_history_limit: usize,
    run_history_budget_bytes: usize,
    run_history_enabled: bool,
    run_history_capture: crate::preferences::RunHistoryCaptureMode,
    pending_keep_results: bool,
    diff_key_columns: Vec<usize>,
    detailed_diff: Option<result_set_difference::DetailedDiff>,
    diff_unavailable_reason: Option<String>,
    detailed_diff_grid: Option<Entity<SqllyDataTable>>,
    /// Spill-file backing per result-set slot (parallel to `grid_views`).
    /// Present for sets past the spill threshold: the grid runs in
    /// windowed-row mode and rows page in/out of the file as the user
    /// scrolls, so only ~(view + 2×window) rows are ever resident. `rs.rows`
    /// stays EMPTY for these slots — full-set consumers must go through
    /// [`Self::visible_rows_for_slot`] or the backing's pager.
    ///
    /// While a query is still RUNNING a backing may be PROVISIONAL
    /// (installed from `SpillWindow` over the growing file's flushed
    /// prefix): its file is service-owned — `spill_manifest` is `None`, so
    /// clear/teardown drops the handle without deleting anything — until
    /// the final manifest reinstalls it under the guard.
    ///
    /// Declared BEFORE `spill_manifest` on purpose: fields drop in
    /// declaration order, so the pagers' open file handles are released
    /// before the manifest guard deletes the files (Windows cannot unlink
    /// an open file).
    spill_backings: Vec<Option<SpillBacking>>,
    /// Async scroll-paging state per slot (parallel to `spill_backings`).
    /// Also declared BEFORE `spill_manifest`: the tasks drop (cancelling any
    /// in-flight read) before the guard deletes the files. Background page
    /// reads hold no long-lived handles — each opens an EPHEMERAL reader
    /// inside the background task (open → page → drop, never across an await
    /// after the read) — and std's Windows `File::open` shares
    /// FILE_SHARE_DELETE, so a read racing a cleanup cannot make the unlink
    /// fail; deletion is merely deferred until that handle drops.
    spill_paging: Vec<SpillPageState>,
    /// The active spill manifest; its guard deletes the spill files when the
    /// view (query tab) goes away or a new manifest replaces it.
    spill_manifest: SpillManifestGuard,
    /// Parsed result-view hints (`-- sqlly view:pivot`, `-- sqlly pivot: …`)
    /// from the most
    /// recent query SQL. Mirrors the Swift `resultViewHints` field on
    /// `QueryWorkspaceController`. Drives the Grid|Pivot toggle.
    view_hints: Option<ResultViewHints>,
    /// Chart layouts saved from each result grid's Chart tab (the Chart
    /// sidebar's save button), keyed by result-set slot so grid rebuilds
    /// restore the config. Cleared with the run when the rows go away.
    chart_configs: HashMap<usize, ChartConfig>,
    /// Column-profile pane over one result set (the grid context menu's
    /// "Profile All Columns"). Snapshot-based like the chart pane: rebuilt on
    /// each action, dropped by [`Self::clear`] when the rows go away.
    result_profile_grid: Option<Entity<SqllyDataTable>>,
    /// Caption above the profile grid: rows profiled + sampling note.
    result_profile_caption: String,
    /// The retained profile behind the grid: the detail pane under the grid
    /// reads the selected column's full statistics and frequent values from
    /// here (the grid rows only carry a preview).
    result_profile: Option<crate::result_profiler::ResultProfile>,
    /// SQL text of the run the profile was taken from, shown under the
    /// detail rows so the profile stays attributable after later edits.
    result_profile_sql: Option<String>,
    /// Active semantic quick filter over the profile grid's rows (nullable /
    /// constant / candidate-key / … column shapes). Reset to All on each
    /// profile action.
    result_profile_filter: crate::result_profiler::ProfileFilter,
    /// For each row of the (possibly filtered) profile grid, the index of
    /// the profiled column it summarizes — the detail pane maps selections
    /// through this so filtering never misattributes a row.
    result_profile_indices: Vec<usize>,
    /// Re-renders the detail pane when the profile grid's selection changes
    /// (the grid state is its own entity; the host doesn't otherwise observe
    /// it).
    _result_profile_watch: Option<gpui::Subscription>,
    /// Read-only single-record view over the active grid ("View Row as
    /// Form"): present while the overlay is open.
    row_form: Option<RowFormState>,
    /// Inline (non-modal) conditional-formatting dialog, with the
    /// subscription that clears it when the dialog closes itself.
    conditional_formatting: Option<(
        Entity<crate::conditional_formatting::ConditionalFormattingDialog>,
        gpui::Subscription,
    )>,
    /// Column awaiting dialog materialization on the next render (the
    /// context-menu handler has no `Window` to build input state with).
    pending_conditional_formatting: Option<String>,
    /// Cell Details dock hosted beside the tabbed grid.
    cell_detail: Option<Entity<crate::cell_detail_panel::CellDetailPanel>>,
    cell_detail_side: crate::cell_detail_model::DockSide,
    cell_detail_size: f32,
    last_focus: Option<(usize, crate::cell_detail_model::FocusedCell)>,
    pane_maximized: bool,
    hydration_requested: Option<(usize, usize, usize)>,
    /// Visual filter builder overlay ("Filter Rows…").
    pub(crate) filter_builder: Option<FilterBuilderState>,
    /// App-side filter tree per slot when the model is not a flat AND of
    /// datatable predicates. Parallel to `result_sets`.
    client_filter: Vec<Option<FilterGroup>>,
    /// Last filter-builder model applied per slot (disabled rows and all), so
    /// reopening the builder or stacking a quick filter keeps conditions the
    /// grid's own `ColumnFilter`s cannot represent. Only trusted while it
    /// still matches what the grid shows (see `builder_seed_model`).
    applied_filter_models: std::collections::HashMap<usize, AppliedFilter>,
    /// Grid-row → `rs.rows` index when a client filter or secondary sort
    /// rebuilt the slot from a subset/pre-order of source rows.
    source_index_map: Vec<Option<Arc<Vec<usize>>>>,
    /// Per-slot multi-column sort spec (`keys[0]` mirrors the grid primary).
    sort_specs: Vec<SortSpec>,
    /// Staged multi-row edits awaiting batch save.
    pending: crate::pending_changes::PendingHistory,
    /// Result-set slot that owns the open pending batch.
    pub(crate) pending_slot: Option<usize>,
    /// Slot for the in-flight stage editor session.
    pub(crate) staging_slot: Option<usize>,
    /// Preview SQL output tab is selected.
    show_preview_sql: bool,
    /// Debounced preview script text for the Preview SQL tab.
    preview_sql_text: String,
    /// Generation counter for debounced preview refresh.
    preview_sql_generation: u64,
    /// Read-only preview panel entity (lazy).
    #[allow(dead_code)] // Retained for future use.
    preview_sql_panel: Option<Entity<crate::ddl_viewer::DdlViewerView>>,
    /// "Save as Snapshot…" name prompt: the slot awaiting a name. The
    /// `InputState` materializes at render time (needs a `Window`).
    snapshot_prompt: Option<SnapshotPrompt>,
    /// Diff change-type filter: None = all classes.
    diff_class_filter: Option<&'static str>,
    /// Named snapshot used as the diff baseline instead of a retained run.
    diff_snapshot_baseline: Option<DiffSnapshotBaseline>,
    /// Index into the snapshot list the cycle button last selected.
    diff_snapshot_index: Option<usize>,
    /// Whether the profile pane is shown instead of the grid.
    show_result_profile: bool,
    /// Show the Client Statistics pane (run metrics: rows, result sets, and the
    /// server/first-response/client/wall-clock timing breakdown) instead of the
    /// grid. Opened from the grid context menu.
    show_statistics: bool,
    /// Whether the Statistics tab is present in the output tab bar. Unlike
    /// `show_statistics` (which tracks the *active* pane and is cleared whenever
    /// another tab is selected), this persists once the pane has been opened —
    /// otherwise switching to Results would remove the tab with no way back.
    /// The pane recomputes from the live run metrics, so it never goes stale
    /// across runs.
    statistics_opened: bool,
    /// Coalesced-repaint state for streaming events (`Rows`, `RowsProgress`):
    /// `pending` marks a repaint owed; the task is the single armed ~16ms
    /// timer that fires it. See [`Self::schedule_streaming_notify`].
    streaming_notify_pending: bool,
    streaming_notify_task: Option<Task<()>>,
    /// Monotonic run stamp, bumped by [`Self::start_query`]. A query pump
    /// captures it at spawn and tags every event it forwards; see
    /// [`Self::handle_event_for_run`].
    run_generation: u64,
    /// Spill files seen via a stale `SpillWindow` (a BORROW — the executor
    /// still owns them). A later stale `SpillManifest` for the same file must
    /// NOT delete it: the service will clean up its own files when the
    /// superseded run finishes or is cancelled.
    service_owned_spill_files: std::collections::HashSet<String>,
    /// The messages pane's text field (mirroring log_console): a real text
    /// field selects by character, spans lines, and knows ⌘A / ⌘C, which the
    /// old line-based hand-rolled selection never could. Created lazily on
    /// first render because the constructor has no `Window`.
    messages_input: Option<Entity<TextareaState>>,
    /// What `messages_input` is supposed to be showing; anything else in it is
    /// a user keystroke.
    messages_rendered: SharedString,
    /// Grid-wide find bar over the active result set's grid. Drives the
    /// datatable's built-in cross-column search (highlight + next/prev).
    grid_find: GridFindBar,
    /// Go-to-column popover over the active result set's grid.
    goto_column: Option<GotoColumnPopover>,
    /// Persistent geometry for the view's unified-chrome dialogs (snapshot
    /// prompt, filter builder, row form), keyed by dialog id so the frame
    /// accessor can never dangle even when the dialog state is an `Option`.
    dialog_frames: crate::dialog::DialogFrames,
}

/// State for the results grid find bar: a value search across every column of
/// the active result set, backed by the datatable crate's grid search. The
/// `InputState` materializes on first open (it needs a `Window`). The replace
/// half turns matches into staged cell edits (see
/// [`ResultsView::grid_find_replace`]).
#[derive(Default)]
struct GridFindBar {
    visible: bool,
    query: String,
    input: Option<Entity<InputState>>,
    subscription: Option<Subscription>,
    /// Set when the bar is opened without a `Window` (e.g. from the grid
    /// context menu); the next render focuses the input and clears this.
    focus_pending: bool,
    replace: String,
    replace_input: Option<Entity<InputState>>,
    replace_subscription: Option<Subscription>,
    /// Bumped per replace request that needed a session opened; only the
    /// latest async session outcome is applied.
    replace_generation: u64,
}

struct GotoColumnPopover {
    model: crate::grid_goto_column::GotoColumnModel,
    input: Option<Entity<InputState>>,
    subscription: Option<Subscription>,
    focus_pending: bool,
}

/// Source-column indices that are all-NULL (`CellValue::None`) in every row.
/// An empty grid has no empty columns — hiding would be a no-op.
pub(crate) fn empty_column_indices(data: &GridData) -> Vec<usize> {
    if data.rows.is_empty() {
        return Vec::new();
    }
    (0..data.columns.len())
        .filter(|&col| {
            data.rows
                .iter()
                .all(|row| matches!(row.get(col), Some(sqlly_datatable::CellValue::None)))
        })
        .collect()
}

pub(crate) fn adjusted_column_width(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(40.0, 4_000.0)
}

/// Row-height / header / font metrics for a density preset. A results font
/// size from Preferences → Display replaces the preset's size and scales its
/// row and header heights by the same ratio, so the density's spacing holds.
fn apply_grid_density(state: &mut GridState, density: GridDensity, zoom: f32, cx: &App) {
    let metrics = grid_metrics(density, crate::theme::Theme::results_font_size(), zoom);
    state.row_height = metrics.row_height;
    state.header_height = metrics.header_height;
    state.font_size = metrics.font_size;
    state.char_width = grid_char_width(metrics.font_size, cx);
}

/// [`apply_grid_density`]'s sizing, free of global state: the density
/// preset, with `results_font_size` (when set) replacing its font size and
/// scaling its heights, then `zoom` applied to the font.
fn grid_metrics(
    density: GridDensity,
    results_font_size: Option<f32>,
    zoom: f32,
) -> crate::preferences::GridDensityMetrics {
    let metrics = density.metrics();
    let base = results_font_size.unwrap_or(metrics.font_size);
    let scale = base / metrics.font_size;
    crate::preferences::GridDensityMetrics {
        row_height: (metrics.row_height * scale).round(),
        header_height: (metrics.header_height * scale).round(),
        font_size: crate::editor_zoom::zoomed_font_size(base, zoom),
    }
}

/// Advance width of one digit in the grid's font, used for column sizing.
/// The built-in platform monospace faces use their known em ratio (as
/// sqlly-datatable's `default_char_width` does); a family picked in
/// Preferences → Display is measured.
fn grid_char_width(font_size: f32, cx: &App) -> f32 {
    let ratio = if cfg!(target_os = "windows") {
        0.55
    } else {
        0.6022
    };
    let fallback = font_size * ratio;
    let Some(family) = sqlly_datatable::grid_font_family() else {
        return fallback;
    };
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&gpui::font(family));
    text_system
        .ch_advance(font_id, px(font_size))
        .map(f32::from)
        .ok()
        .filter(|w| *w > 0.0)
        .unwrap_or(fallback)
}

/// What a text export to the clipboard did. Returned by
/// [`ResultsView::apply_clipboard_export`] so the caller can raise the
/// matching toast, and so the decision itself stays testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardExport {
    /// Rows were written to the clipboard. `truncated` marks a spill-backed
    /// set that stopped at the export cap.
    Copied { rows: usize, truncated: bool },
    /// The tab has no result set to export.
    NothingToExport,
}

pub enum ResultsViewEvent {
    QueryCompleted,
    QueryFailed,
    /// The Chart tab (toolbar chip or grid context menu) asked to keep the
    /// live chart layout in the saved chart library. The host owns the
    /// connection profile, so it opens the naming prompt.
    SaveChartToLibraryRequested {
        spec: crate::result_chart::ResultChartSpec,
        /// The SQL whose execution produced the charted result.
        sql: String,
    },
    /// Gesture request to advance the query tab switcher.
    #[allow(dead_code)]
    AdvanceQuery {
        direction: ScrollDirection,
    },
    /// Row-header context menu asked to edit, insert, or duplicate a row. The
    /// host (`AppView`) owns the connection profile and `Services`, so it opens
    /// the modal.
    EditRowRequested {
        /// Query result-set index (`ResultSet::index`, not the display slot).
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        /// The clicked row's typed values in source (unsorted) order.
        row: Vec<CellValue>,
        /// The SQL whose execution produced the grid.
        sql: String,
        /// Whether to edit the clicked row, insert a new one, or duplicate it.
        intent: crate::row_editor::RowEditorIntent,
    },
    /// Cell context menu asked to stage a typed write (NULL/DEFAULT/server
    /// time/file) through the pending batch.
    CellActionRequested {
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        row: Vec<CellValue>,
        sql: String,
        column_name: String,
        /// Index of the clicked column in `columns`. The host resolves the
        /// table column through provenance with it — never by
        /// `column_name`, which an aliased expression can share.
        result_column: usize,
        action: crate::cell_actions::CellAction,
    },
    /// Row-header menu asked to stage an edit through the modal (no immediate save).
    StageRequested {
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        row: Vec<CellValue>,
        sql: String,
        intent: crate::row_editor::RowEditorIntent,
    },
    /// Cmd+V on the grid: the host resolves the table session and calls
    /// back `ResultsView::apply_paste` with it.
    PasteRequested {
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        /// The anchor row's source values (for range-overwrite key lookup) or
        /// `None` when the selection is empty (append as new rows).
        anchor_row: Option<Vec<CellValue>>,
        sql: String,
        /// Raw clipboard text, parsed by `paste_rows::parse_clipboard`.
        text: String,
        /// Display-space anchor `(row, col)` and whether a multi-cell range was selected.
        anchor: (usize, usize),
        overwrite: bool,
        /// `(rows, columns)` of a contiguous selected range that a smaller
        /// clipboard fills (see `paste_rows::fill_selection`).
        fill_range: Option<(usize, usize)>,
    },
    /// User confirmed saving the staged batch; the host runs apply_batch.
    SaveStagedRequested {
        batch: sqlly_services::row_editing::StagedBatch,
        rollback_sql: String,
        preview_sql: String,
    },
    /// Cell context menu asked to open the row a foreign-key cell references.
    FollowForeignKeyRequested {
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        row: Vec<CellValue>,
        column_index: usize,
        sql: String,
    },
    /// Context menu asked to copy selected rows as SQL UPDATE statements.
    /// The host resolves the primary key via the row-editing service.
    CopyUpdateRequested {
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        rows: Vec<Vec<CellValue>>,
        sample_row: Vec<CellValue>,
        sql: String,
    },
    /// Multi-select Stage Delete: skip the row-editor modal and stage every
    /// selected row as a delete after the host opens one edit session.
    StageDeleteManyRequested {
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        rows: Vec<Vec<CellValue>>,
        sample_row: Vec<CellValue>,
        sql: String,
    },
    /// A grid action needs the full value behind a truncated cell. The host
    /// owns `Services`, so it fetches the row by key and calls back.
    HydrateCellRequested {
        result_set_index: usize,
        columns: Vec<ResultColumn>,
        row: Vec<CellValue>,
        sql: String,
        column_index: usize,
        column_name: String,
        truncated: TruncatedCell,
        purpose: crate::cell_hydration::HydratePurpose,
    },
    /// The plan pane's Re-capture button was pressed. The app owns the
    /// connection profile and services, so it re-runs the capture and feeds
    /// the result back through `set_execution_plan` / `fail_execution_plan`.
    ExecutionPlanCaptureRequested(PlanKind),
    /// Open a saved plan file into the plan pane.
    ExecutionPlanOpenRequested,
    /// Paste a plan document from the clipboard into the plan pane.
    ExecutionPlanPasteRequested,
    /// Open generated finding SQL in a new editor tab without running it.
    ExecutionPlanOpenSqlInNewTab(String),
    /// Result layout (stacked vs tabbed) was toggled from results chrome.
    LayoutModeChanged(LayoutMode),
    /// An output-panel tab that needs host-owned content (History / DDL) or
    /// a pane show (Plan) was selected.
    OutputTabSelected(OutputPanelTab),
    /// The status-strip auto-refresh menu changed the interval.
    AutoRefreshIntervalChanged(Option<u64>),
    /// Status-strip "Calculate total rows" was clicked for a truncated/spilled set.
    TotalRowCountRequested {
        result_set_index: usize,
        sql: String,
    },
    /// Cancel an in-flight COUNT(*) for the current result set.
    TotalRowCountCancelRequested,
    /// Jump the editor caret to a previously located execution-error offset.
    GoToEditorOffset(usize),
    /// The result-name menu or the Result Settings dialog asked to write
    /// `-- sqlly` directives (or comment out the producing statement) in the
    /// query that produced the results. The host owns the editor buffer and
    /// the executed span, so it applies the edit.
    ResultSettingsRequested(crate::result_header_actions::ResultSettingsChange),
    /// The lock in front of a result's name was clicked: write these
    /// directive lines into the query as its `/* sqlly lock` block.
    LockRequested(lock::LockSnapshot),
    /// The formula bar added or changed a computed column: write `line` into
    /// the query, replacing the `-- sqlly column:` line named `replacing`.
    ComputedColumnWritten {
        line: String,
        replacing: Option<String>,
    },
    /// The active result-set tab changed (click or keyboard).
    ActiveResultChanged,
    /// Staged (unsaved) grid edits were added, undone, discarded or saved —
    /// the host's tab strip marks a tab with staged edits as unsaved.
    StagedEditsChanged,
    /// Toolbar/menu asked to save a streamable export format to a file.
    #[cfg(not(target_arch = "wasm32"))]
    ExportToFileRequested {
        format: ExportFormat,
    },
    /// Grid menu asked to persist a per-connection include/exclude pattern.
    /// Open Settings at the masking-rules pane (grid menu "Masking Rules…").
    OpenMaskingSettings,
    MaskingOverrideRequested {
        column_name: String,
        include: bool,
    },
    CellDetailClosed,
    /// The grid cell menu asked to show the Cell Details panel.
    CellDetailOpenRequested,
    CellDetailSideToggled,
    CellDetailDragStarted {
        pos: f32,
    },
    MaximizeToggleRequested,
    DetachRequested,
}

/// Outcome of a lazy "Calculate total rows" COUNT(*) query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TotalCount {
    Pending,
    Ready(u64),
    Failed(String),
}

impl EventEmitter<ResultsViewEvent> for ResultsView {}

#[derive(Default, Clone)]
pub struct ResultSet {
    pub index: usize,
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub rows_affected: Option<u64>,
    /// Logical row count, independent of `rows.len()`: tracks `Rows` batches
    /// and count-only `RowsProgress` events (emitted once payload forwarding
    /// is suppressed, so `rows` stops growing). The spill manifest's
    /// `row_count` overwrites it — the manifest total is authoritative.
    pub row_count_hint: usize,
    pub truncated: std::collections::HashMap<(usize, usize), TruncatedCell>,
}

impl ResultSet {
    /// Whether `rows` holds the complete logical set. False once payload
    /// forwarding was suppressed (`row_count_hint` ran ahead of `rows`) —
    /// spill-backed slots are the common case, but this also catches a
    /// suppressed set whose backing failed to install. Whole-set consumers
    /// that read `rows` directly (pivot, diff) must check this rather than
    /// assume `rows` is the full set.
    pub fn rows_complete(&self) -> bool {
        self.rows.len() >= self.row_count_hint
    }
}

/// Frozen, read-only snapshot of one result set in a retained run.
#[derive(Clone)]
pub(crate) struct RetainedRunSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Option<Vec<Vec<CellValue>>>,
    pub row_count: usize,
    pub rows_affected: Option<u64>,
    pub unavailable_reason: Option<String>,
}

/// Frozen, read-only snapshot of a completed query run for the run strip.
#[derive(Clone)]
pub(crate) struct RetainedRun {
    pub id: u64,
    pub label: String,
    pub completed_at: SystemTime,
    pub elapsed_ms: Option<u64>,
    pub sql: String,
    pub pinned: bool,
    pub sets: Vec<RetainedRunSet>,
    /// Server output for this run (PRINT / rows-affected / DB-context /
    /// cancellation lines). Retained so a run that returned no result sets —
    /// only row counts and messages — still has something to show when it is
    /// restored from the run strip.
    pub messages: Vec<String>,
}

/// Resident-byte estimate: `rows × columns × 32` plus string cell lengths.
pub(crate) fn estimate_set_bytes(rs: &ResultSet) -> usize {
    estimate_rows_bytes(rs.columns.len(), &rs.rows)
}

/// Order of the conjunction select's labels.
const FILTER_CONJUNCTION_CHOICES: [Conjunction; 2] = [Conjunction::And, Conjunction::Or];

fn filter_conjunction_items() -> Vec<String> {
    FILTER_CONJUNCTION_CHOICES
        .iter()
        .map(|conjunction| {
            crate::localization::text(match conjunction {
                Conjunction::And => "results.filters.conjunction_all_and",
                Conjunction::Or => "results.filters.conjunction_any_or",
            })
        })
        .collect()
}

/// Map the conjunction select's confirmed row (never its translated label).
fn conjunction_from_select(row: Option<usize>) -> Conjunction {
    row.and_then(|row| FILTER_CONJUNCTION_CHOICES.get(row).copied())
        .unwrap_or(Conjunction::And)
}

fn conjunction_select_index(conjunction: Conjunction) -> Option<gpui_component::IndexPath> {
    FILTER_CONJUNCTION_CHOICES
        .iter()
        .position(|choice| *choice == conjunction)
        .map(gpui_component::IndexPath::new)
}

/// One entry of the saved-filters dropdown: the label shown (the default
/// carries a marker) and the filter's name as the confirmed value, so a set
/// a user named `x (default)` is still selectable.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SavedFilterOption {
    label: gpui::SharedString,
    name: String,
}

impl SavedFilterOption {
    fn new(filter: &crate::saved_filters::SavedFilter) -> Self {
        let label = if filter.is_default {
            crate::localization::format(
                "results.filters.saved_default_option",
                &[("filter_name", &filter.name)],
            )
        } else {
            filter.name.clone()
        };
        Self {
            label: label.into(),
            name: filter.name.clone(),
        }
    }
}

impl gpui_component::searchable_list::SearchableListItem for SavedFilterOption {
    type Value = String;

    fn title(&self) -> gpui::SharedString {
        self.label.clone()
    }

    fn value(&self) -> &String {
        &self.name
    }
}

fn estimate_rows_bytes(columns: usize, rows: &[Vec<CellValue>]) -> usize {
    let cell_overhead = rows.len().saturating_mul(columns).saturating_mul(32);
    let string_bytes: usize = rows
        .iter()
        .flatten()
        .map(|cell| match cell {
            CellValue::String(s) => s.len(),
            _ => 0,
        })
        .sum();
    cell_overhead.saturating_add(string_bytes)
}

fn estimate_retained_set_bytes(set: &RetainedRunSet) -> usize {
    match &set.rows {
        Some(rows) => estimate_rows_bytes(set.columns.len(), rows),
        None => 0,
    }
}

fn estimate_run_bytes(run: &RetainedRun) -> usize {
    run.sets.iter().map(estimate_retained_set_bytes).sum()
}

/// Structural equality of two runs' snapshotted result sets — used by the
/// run-history capture-mode preference to detect whether results changed.
fn retained_sets_equal(a: &[RetainedRunSet], b: &[RetainedRunSet]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.row_count == y.row_count
                && x.rows_affected == y.rows_affected
                && x.columns == y.columns
                && x.rows == y.rows
        })
}

fn typed_cell_text(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(value) => format!("bool:{value}"),
        CellValue::I64(value) => format!("i64:{value}"),
        CellValue::F64(value) => format!("f64:{value}"),
        CellValue::String(value) => format!("string:{value}"),
        CellValue::Bytes(value) => format!(
            "bytes:{}",
            value
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

/// Visible-order rows from `GridState`: honour sort/filter `display_indices`
/// when present, otherwise the result set (or the grid's own data when the
/// result set's `rows` vec is empty).
fn rows_from_grid_state(
    state: &GridState,
    rs_rows: &[Vec<CellValue>],
    source_index: impl Fn(usize) -> usize,
) -> Vec<Vec<CellValue>> {
    if state.display_indices.is_empty() {
        // The grid always lists every row it shows in `display_indices`
        // (identity when unfiltered), so a grid that holds rows but lists
        // none has a filter hiding all of them: export/copy nothing rather
        // than silently falling back to the whole result.
        if !state.data.rows.is_empty() {
            return Vec::new();
        }
        if rs_rows.is_empty() {
            state
                .data
                .rows
                .iter()
                .map(|row| row.iter().map(grid_cell_to_core).collect())
                .collect()
        } else {
            rs_rows.to_vec()
        }
    } else {
        state
            .display_indices
            .iter()
            .filter_map(|&src_idx| {
                if !rs_rows.is_empty() {
                    rs_rows.get(source_index(src_idx)).cloned()
                } else {
                    state
                        .data
                        .rows
                        .get(src_idx)
                        .map(|row| row.iter().map(grid_cell_to_core).collect::<Vec<_>>())
                }
            })
            .collect()
    }
}

/// One raw row from [`ResultsView::raw_row_at`]: the cells as the
/// engine returned them. Borrowed from a resident result set, owned when
/// read from the spill pager.
pub(crate) struct RawRow<'a> {
    pub(crate) cells: std::borrow::Cow<'a, [CellValue]>,
}

impl ResultsView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let perf_prefs = crate::preferences::PerformancePreferences::load();
        let run_history_limit = perf_prefs.result_run_history_runs();
        let run_history_budget_bytes = perf_prefs.result_memory_budget_bytes();
        let run_history_enabled = perf_prefs.result_run_history_enabled();
        let run_history_capture = perf_prefs.result_run_history_capture();
        Self {
            result_sets: Vec::new(),
            active_tab: 0,
            messages: Vec::new(),
            diagnostics: Vec::new(),
            execution_error_offset: None,
            show_messages: false,
            elapsed_ms: None,
            query_start: None,
            first_response_ms: None,
            response_finished_ms: None,
            completed_at: None,
            completed_wall: None,
            run_started_wall: None,
            ui_paint_ms: None,
            wall_clock_ms: None,
            estimated_latency_ms: None,
            subtract_latency: false,
            timing_format: TimingFormat::default(),
            is_running: false,
            error: None,
            historical_banner: None,
            formatting: None,
            masking_rules: None,
            masking_rules_fallback: std::cell::OnceCell::new(),
            selection_stats_cache: std::cell::RefCell::new(HashMap::new()),
            masking_overrides: MaskingOverrides::default(),
            revealed: Vec::new(),
            revealed_cells: Vec::new(),
            column_profiles: Vec::new(),
            column_profile_requested: Vec::new(),
            suggested_mask_accepted: Vec::new(),
            mask_suggestion_dismissed: Vec::new(),
            chart_suggestions: Vec::new(),
            chart_suggestion_requested: Vec::new(),
            chart_suggestion_reverted: Vec::new(),
            copy_format_override: None,
            show_detailed_timing: false,
            perf_prefs,
            gesture_prefs: crate::gesture_prefs::load_default_preferences(),
            format_hints: Vec::new(),
            grid_directives: Vec::new(),
            conditional_directives: Vec::new(),
            grid_directives_applied: HashSet::new(),
            locked_by_click: false,
            computed_specs: Vec::new(),
            computed_plans: Vec::new(),
            formula_variables: Vec::new(),
            formula_now: 0.0,
            formula_bar: None,
            canvas_layout: None,
            layout_directives: crate::result_layout::LayoutDirectives::default(),
            show_export_menu: false,
            last_export_notice: None,
            grid_views: Vec::new(),
            grid_zoom: crate::editor_zoom::DEFAULT_ZOOM,
            zoom_wheel_accumulator: 0.0,
            pinch_zoom_accumulator: 0.0,
            transposed: Vec::new(),
            transpose_active: Vec::new(),
            transpose_truncated: Vec::new(),
            transpose_row_count: Vec::new(),
            filter_limitations: Vec::new(),
            default_filter_consulted: HashSet::new(),
            result_epoch: 0,
            spill_install_pending: false,
            synced_rows: Vec::new(),
            auto_size: HashMap::new(),
            editor: None,
            layout_mode: LayoutMode::default(),
            query_sql: None,
            auto_refresh_secs: None,
            elapsed_ticker: None,
            total_row_counts: HashMap::new(),
            result_set_names: BTreeMap::new(),
            index_labels: BTreeMap::new(),
            collapsed_cards: Vec::new(),
            source_db_type: None,
            layout_scope: None,
            saved_filter_store_for_test: None,
            layout_store: None,
            layout_store_mode: LayoutStoreMode::Disk,
            background_read: None,
            spill_drills: Vec::new(),
            spill_drill_generation: 0,
            background_read_generation: 0,
            spill_prefixes: std::cell::RefCell::new(Vec::new()),
            applied_layouts: Vec::new(),
            layout_save_generation: Vec::new(),
            fk_label_profile: None,
            fk_label_services: None,
            row_edit_connection_block: None,
            fk_label_cache: crate::fk_labels::LabelCache::default(),
            fk_labels_by_slot: Vec::new(),
            fk_label_fetch_generation: Vec::new(),
            track_difference: false,
            previous_result_sets: Vec::new(),
            diff: None,
            show_diff: false,
            execution_plan_view: None,
            plan_capture_gate: crate::capabilities::PlanCaptureGate::default(),
            show_execution_plan: false,
            show_history: false,
            show_ddl: false,
            #[cfg(not(target_arch = "wasm32"))]
            history_panel: None,
            ddl_panel: None,
            stacked_scroll_handle: ScrollHandle::new(),
            stacked_card_heights: Vec::new(),
            stacked_resize: None,
            scroll_accumulator: ScrollDeltaAccumulator::default(),
            tab_switch_gate: gesture_interpreter::EdgeSwitchGate::default(),
            result_name_menu: None,
            result_settings: None,
            diff_direction: DiffDirection::default(),
            diff_grid_previous: None,
            diff_grid_current: None,
            diff_grid_diff: None,
            retained_diff_runs: Vec::new(),
            next_diff_run_id: 1,
            diff_baseline_run: None,
            diff_comparison_run: None,
            retained_runs: Vec::new(),
            next_run_id: 1,
            viewing_run: None,
            current_run_snapshot: None,
            run_history_limit,
            run_history_budget_bytes,
            run_history_enabled,
            run_history_capture,
            pending_keep_results: false,
            diff_key_columns: Vec::new(),
            detailed_diff: None,
            diff_unavailable_reason: None,
            detailed_diff_grid: None,
            spill_manifest: SpillManifestGuard(None),
            spill_backings: Vec::new(),
            spill_paging: Vec::new(),
            view_hints: None,
            chart_configs: HashMap::new(),
            result_profile_grid: None,
            result_profile_caption: String::new(),
            result_profile: None,
            result_profile_sql: None,
            result_profile_filter: crate::result_profiler::ProfileFilter::default(),
            result_profile_indices: Vec::new(),
            _result_profile_watch: None,
            row_form: None,
            conditional_formatting: None,
            pending_conditional_formatting: None,
            cell_detail: None,
            cell_detail_side: crate::cell_detail_model::DockSide::Right,
            cell_detail_size: crate::workspace_session::DEFAULT_CELL_DETAIL_WIDTH,
            pane_maximized: false,
            last_focus: None,
            hydration_requested: None,
            filter_builder: None,
            client_filter: Vec::new(),
            applied_filter_models: std::collections::HashMap::new(),
            source_index_map: Vec::new(),
            sort_specs: Vec::new(),
            pending: crate::pending_changes::PendingHistory::default(),
            pending_slot: None,
            staging_slot: None,
            show_preview_sql: false,
            preview_sql_text: String::new(),
            preview_sql_generation: 0,
            preview_sql_panel: None,
            snapshot_prompt: None,
            diff_class_filter: None,
            diff_snapshot_baseline: None,
            diff_snapshot_index: None,
            show_result_profile: false,
            show_statistics: false,
            statistics_opened: false,
            streaming_notify_pending: false,
            streaming_notify_task: None,
            run_generation: 0,
            service_owned_spill_files: Default::default(),
            messages_input: None,
            messages_rendered: SharedString::default(),
            grid_find: GridFindBar::default(),
            goto_column: None,
            dialog_frames: crate::dialog::DialogFrames::new(),
        }
    }

    /// Set the weak reference to the SQL editor, used by the context-menu
    /// provider for WHERE-clause insertion.
    pub fn set_editor(&mut self, editor: WeakEntity<SqlEditor>, _cx: &mut Context<Self>) {
        self.editor = Some(editor);
    }

    pub fn set_cell_detail(
        &mut self,
        panel: Option<Entity<crate::cell_detail_panel::CellDetailPanel>>,
        side: crate::cell_detail_model::DockSide,
        size: f32,
        cx: &mut Context<Self>,
    ) {
        self.cell_detail_side = side;
        self.cell_detail_size = size;
        if let Some(panel) = panel.clone() {
            cx.subscribe(&panel, Self::handle_cell_detail_event)
                .detach();
        }
        self.cell_detail = panel;
        self.last_focus = None;
        self.hydration_requested = None;
        if self.cell_detail.is_some() {
            self.sync_cell_detail(self.active_tab, cx);
        }
        cx.notify();
    }

    pub fn set_cell_detail_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.cell_detail_size = size;
        cx.notify();
    }

    pub fn set_cell_detail_side(
        &mut self,
        side: crate::cell_detail_model::DockSide,
        size: f32,
        cx: &mut Context<Self>,
    ) {
        self.cell_detail_side = side;
        self.cell_detail_size = size;
        cx.notify();
    }

    fn handle_cell_detail_event(
        &mut self,
        _panel: Entity<crate::cell_detail_panel::CellDetailPanel>,
        event: &crate::cell_detail_panel::CellDetailEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::cell_detail_panel::CellDetailEvent::Close => {
                cx.emit(ResultsViewEvent::CellDetailClosed);
            }
            crate::cell_detail_panel::CellDetailEvent::ToggleSide => {
                cx.emit(ResultsViewEvent::CellDetailSideToggled);
            }
            crate::cell_detail_panel::CellDetailEvent::OpenAsDialog => {
                self.open_cell_detail_dialog(cx);
            }
            crate::cell_detail_panel::CellDetailEvent::StageEdit { slot, focus, edit } => {
                self.stage_cell_detail_edit(*slot, *focus, edit.clone(), cx);
            }
            crate::cell_detail_panel::CellDetailEvent::FocusColumn(col) => {
                let Some((_, focus)) = self.last_focus else {
                    return;
                };
                if let Some(state) = self.active_grid_state(cx) {
                    // The panel lists result columns; the grid may lead with
                    // the staged-edit marker column.
                    let marker = self.grid_marker_width_of(self.active_tab, state.read(cx));
                    state.update(cx, |s, cx| {
                        s.selection = Selection::Cell(focus.display_row, *col + marker);
                        cx.notify();
                    });
                }
            }
        }
    }

    fn open_cell_detail_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.cell_detail.clone() else {
            return;
        };
        let Some((column_name, preview, raw)) = panel.read(cx).preview_for_open() else {
            return;
        };
        // `focus.source_row` is grid-resident: on a spill-backed (windowed)
        // grid it is relative to the loaded window, while truncation entries
        // and the spill pager are keyed by the absolute row.
        let window_offset = self
            .active_grid_state(cx)
            .and_then(|state| state.read(cx).window.map(|w| w.offset));
        if let Some((_, focus)) = self.last_focus
            && let trunc_row =
                self.truncation_row_for_grid(self.active_tab, focus.source_row, window_offset)
            && self
                .truncated_cell(self.active_tab, trunc_row, focus.column)
                .is_some()
        {
            self.request_cell_hydration(
                self.active_tab,
                trunc_row,
                focus.column,
                crate::cell_hydration::HydratePurpose::Viewer,
                cx,
            );
            return;
        }
        // The byte-exact value feeds the viewer's format picker (a binary
        // cell's hex view can be re-read as MessagePack).
        let payload = panel
            .read(cx)
            .value_for_open()
            .and_then(|cell| crate::structured_data::picker_payload(&cell, &preview));
        crate::structured_data_viewer::open_cell_preview_with_payload(
            &column_name,
            preview,
            &raw,
            payload,
            None,
            cx,
        );
    }

    /// Forget the panel's remembered focus and re-read the active grid. The
    /// panel is cleared first: when the new grid has no focused cell (a
    /// fresh run, a cleared view, another result tab) the "focus changed?"
    /// check sees None twice and would otherwise leave the old row showing.
    /// A grid rebuilt with the same focus (staged edits) re-reads its value.
    pub(crate) fn refresh_cell_detail(&mut self, cx: &mut Context<Self>) {
        self.last_focus = None;
        // A re-read cell that is still a truncated preview asks for its full
        // value again (the panel cannot edit a preview).
        self.hydration_requested = None;
        let slot = self.active_tab;
        if let Some(panel) = self.cell_detail.clone() {
            panel.update(cx, |p, cx| p.set_detail(slot, None, None, cx));
        }
        self.sync_cell_detail(slot, cx);
        // A pinned panel keeps its own cell, which the grid focus no longer
        // names: re-read it too, so it shows (and edits) the staged value.
        if let Some(panel) = self.cell_detail.clone()
            && let Some((pinned_slot, pinned_focus)) = panel.read(cx).pinned()
            && pinned_slot == slot
            && self.last_focus != Some((pinned_slot, pinned_focus))
        {
            self.push_cell_detail(slot, pinned_focus, cx);
        }
    }

    fn sync_cell_detail(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(panel) = self.cell_detail.clone() else {
            return;
        };
        if slot != self.active_tab {
            return;
        }
        let Some(state) = self.active_grid_state(cx) else {
            if crate::cell_detail_model::focus_changed(self.last_focus, None) {
                self.last_focus = None;
                panel.update(cx, |p, cx| p.set_detail(slot, None, None, cx));
            }
            return;
        };
        let (selection, display_indices) = {
            let s = state.read(cx);
            // The panel reads result rows and truncation entries, so its
            // focus is a result column, never the staged-edit marker.
            let marker = self.grid_marker_width_of(slot, s);
            (
                selection_in_result_columns(&s.selection, marker),
                s.display_indices.clone(),
            )
        };
        let focus = crate::cell_detail_model::focused_cell(&selection, &display_indices);
        let next = focus.map(|f| (slot, f));
        if !crate::cell_detail_model::focus_changed(self.last_focus, next) {
            return;
        }
        self.last_focus = next;
        let Some(focus) = focus else {
            panel.update(cx, |p, cx| p.set_detail(slot, None, None, cx));
            return;
        };
        self.push_cell_detail(slot, focus, cx);
    }

    /// Show `focus` of `slot` in the panel — what the grid paints there —
    /// with whether the panel may edit it, and ask for the full value of a
    /// truncated preview.
    fn push_cell_detail(
        &mut self,
        slot: usize,
        focus: crate::cell_detail_model::FocusedCell,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.cell_detail.clone() else {
            return;
        };
        // A panel pinned to another cell ignores this one; do not fetch a
        // full value it would then show in place of the pinned cell's.
        if let Some(pinned) = panel.read(cx).pinned()
            && pinned != (slot, focus)
        {
            return;
        }
        let Some(state) = self.grid_state_for_slot(slot, cx) else {
            return;
        };
        let (window_offset, committed_grid_rows) = {
            let s = state.read(cx);
            let marker = self.grid_marker_width_of(slot, s);
            // Staged inserted rows follow the committed rows in the grid.
            let staged_inserts = if marker > 0 {
                self.pending.current().new_rows.len()
            } else {
                0
            };
            (
                s.window.map(|w| w.offset),
                s.data.rows.len().saturating_sub(staged_inserts),
            )
        };
        let mut source_row = focus.source_row;
        if let Some(offset) = window_offset {
            source_row = offset.saturating_add(focus.source_row);
        }
        // The panel shows what the grid paints: staged inserted rows and
        // staged cell values, not just the committed rows.
        let staged_insert =
            self.pending_slot == Some(slot) && focus.source_row >= committed_grid_rows;
        let mut staged_expression = false;
        let (row, staged_focus) = if staged_insert {
            let row = self.staged_insert_row(slot, focus.source_row - committed_grid_rows);
            (row, true)
        } else {
            match self.source_row_for_slot(slot, source_row, cx) {
                Some(mut row) => {
                    staged_expression = self.staged_expression_at(slot, &row, focus.column);
                    let staged = self.apply_staged_edits_to_row(slot, &mut row);
                    (Some(row), staged.contains(&focus.column))
                }
                None => (None, false),
            }
        };
        let Some(row) = row else {
            panel.update(cx, |p, cx| p.set_detail(slot, None, Some(focus), cx));
            return;
        };
        let Some(rs) = self.result_set_at(slot) else {
            return;
        };
        let total_rows = self.row_count_for_slot(slot);
        let detail = crate::cell_detail_model::build_detail(&rs.columns, &row, focus, total_rows);
        let trunc_row = self.source_index(slot, source_row);
        // A staged value is complete; hydrating would replace it with the
        // committed one.
        let truncated = if staged_focus {
            None
        } else {
            self.truncated_cell(slot, trunc_row, focus.column)
        };
        let gate = self.cell_detail_edit_gate(
            slot,
            focus.column,
            row.get(focus.column),
            crate::cell_detail_model::EditGateInputs {
                staged_insert,
                staged_expression,
                truncated: truncated.is_some(),
                ..Default::default()
            },
        );
        panel.update(cx, |p, cx| {
            p.set_detail(slot, detail, Some(focus), cx);
            p.set_edit_gate(slot, focus, gate, cx);
        });
        if truncated.is_some() {
            let key = (slot, trunc_row, focus.column);
            if self.hydration_requested != Some(key) {
                self.hydration_requested = Some(key);
                self.request_cell_hydration(
                    slot,
                    trunc_row,
                    focus.column,
                    crate::cell_hydration::HydratePurpose::Dock,
                    cx,
                );
            }
        }
    }

    /// Whether the Cell Details panel may edit result column `column` of
    /// `slot` holding `value`: the row editor's synchronous refusals plus
    /// what only the grid knows (formula columns, masking, binary values).
    /// `inputs` carries the per-cell facts the caller already resolved.
    fn cell_detail_edit_gate(
        &self,
        slot: usize,
        column: usize,
        value: Option<&CellValue>,
        inputs: crate::cell_detail_model::EditGateInputs,
    ) -> crate::cell_detail_model::CellEditGate {
        let data_type = self
            .result_sets
            .get(slot)
            .and_then(|rs| rs.columns.get(column))
            .map(|c| c.data_type.as_str())
            .unwrap_or_default();
        let computed = self
            .computed_plans
            .get(slot)
            .and_then(|plan| plan.as_ref())
            .is_some_and(|plan| {
                matches!(
                    plan.layout.get(column),
                    Some(crate::computed_columns::Slot::Computed(_))
                )
            });
        crate::cell_detail_model::edit_gate(crate::cell_detail_model::EditGateInputs {
            row_edit_block: self.row_edit_block_reason_for_slot(slot),
            computed,
            masked: self.masked_columns(slot).contains(&column),
            binary: matches!(value, Some(CellValue::Bytes(_)))
                || crate::cell_actions::is_binary_type_for(self.source_db_type, data_type),
            ..inputs
        })
    }

    /// Whether committed `row` of `slot` has a staged expression (DEFAULT,
    /// server time, …) for result column `column`.
    fn staged_expression_at(&self, slot: usize, row: &[CellValue], column: usize) -> bool {
        if self.pending_slot != Some(slot) || self.pending.is_empty() {
            return false;
        }
        let (Some(session), Some(rs)) = (
            self.pending.current().session.as_ref(),
            self.result_sets.get(slot),
        ) else {
            return false;
        };
        crate::pending_changes::row_key_from_result_row(session, &rs.columns, row)
            .and_then(|key| self.pending.current().dirty.get(&key))
            .is_some_and(|edits| {
                edits.iter().any(|(name, edit)| {
                    matches!(edit, crate::pending_changes::CellEdit::Expression(_))
                        && crate::pending_changes::result_index_for_table_column(session, name)
                            == Some(column)
                })
            })
    }

    /// Grid cell menu "Cell Details": ask the host to show the panel. The
    /// menu has already selected the clicked cell, which the panel follows.
    pub(crate) fn request_cell_detail_open(&mut self, cx: &mut Context<Self>) {
        if self.cell_detail.is_none() {
            cx.emit(ResultsViewEvent::CellDetailOpenRequested);
        } else {
            self.sync_cell_detail(self.active_tab, cx);
        }
    }

    /// Stage an edit committed in the Cell Details panel through the same
    /// path as the grid's cell menu (Set NULL, transforms, …): the host
    /// opens the row's edit session, validates the value for the column,
    /// and stages it, so Save / Review / Discard and the grid's staged
    /// marker work unchanged. `focus` is the panel's cell — the pinned one
    /// when the panel is pinned, not necessarily the grid's focus.
    fn stage_cell_detail_edit(
        &mut self,
        slot: usize,
        focus: crate::cell_detail_model::FocusedCell,
        edit: crate::cell_detail_model::PanelEdit,
        cx: &mut Context<Self>,
    ) {
        let window_offset = self.grid_window_offset(slot, cx);
        let source_row = window_offset.map_or(focus.source_row, |offset| {
            offset.saturating_add(focus.source_row)
        });
        let action = match edit {
            crate::cell_detail_model::PanelEdit::Text(text) => {
                crate::cell_actions::CellAction::SetText(text)
            }
            crate::cell_detail_model::PanelEdit::Null => crate::cell_actions::CellAction::SetNull,
        };
        let grid_column = focus.column + self.marker_offset(slot);
        self.request_cell_action(slot, source_row, grid_column, action, cx);
    }

    pub fn apply_hydrated_cell_value(
        &mut self,
        value: sqlly_core::CellValue,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = &self.cell_detail {
            panel.update(cx, |p, cx| p.apply_hydrated_value(value, cx));
        }
    }

    /// The full value of the panel's truncated cell could not be fetched:
    /// show `banner` in its place and keep the preview read-only.
    pub fn apply_cell_hydration_unavailable(&mut self, banner: String, cx: &mut Context<Self>) {
        if let Some(panel) = &self.cell_detail {
            panel.update(cx, |p, cx| p.apply_hydration_unavailable(banner, cx));
        }
    }

    /// Set the result layout mode (tabbed / stacked / canvas).
    pub fn set_layout_mode(&mut self, mode: LayoutMode, cx: &mut Context<Self>) {
        self.layout_mode = mode;
        cx.notify();
    }

    /// Get the current layout mode.
    #[allow(dead_code)]
    pub fn layout_mode(&self) -> LayoutMode {
        self.layout_mode
    }

    /// Override the display label for a result set by its remapped index.
    pub fn set_index_label(&mut self, index: usize, label: String, cx: &mut Context<Self>) {
        self.index_labels.insert(index, label);
        cx.notify();
    }

    /// Append a line to the Messages pane.
    pub fn push_message(&mut self, text: String, cx: &mut Context<Self>) {
        self.messages.push(text);
        cx.notify();
    }

    /// Set the SQL text of the query about to be executed. Parses result-set
    /// names from `-- sqlly name n: text` comments so the tab/card labels can show
    /// them. Called before `start_query`.
    pub fn set_query_sql(&mut self, sql: &str, cx: &mut Context<Self>) {
        let assignments = crate::result_set_names::parse(sql);
        self.result_set_names = crate::result_set_names::resolved_names(&assignments);
        self.index_labels.clear();
        self.query_sql = Some(sql.to_string());
        self.format_hints = crate::query_format::parse(sql);
        self.grid_directives = crate::grid_directive::parse(sql);
        self.conditional_directives = crate::conditional_directive::parse(sql);
        self.grid_directives_applied.clear();
        self.locked_by_click = false;
        self.computed_specs = crate::computed_columns::parse(sql);
        self.computed_plans.clear();
        self.formula_now = crate::computed_columns::now_serial();
        self.canvas_layout = None;
        // `-- sqlly layout:` picks the layout for this run; the toolbar and
        // session follow it like a click on the layout control would.
        self.layout_directives = crate::result_layout::parse_layout_directives(sql);
        if let Some(requested) = self.layout_directives.layout {
            let mode = match requested {
                crate::result_layout::RequestedLayout::Stacked => LayoutMode::Stacked,
                crate::result_layout::RequestedLayout::Tabbed => LayoutMode::Tabbed,
                crate::result_layout::RequestedLayout::Canvas => LayoutMode::Canvas,
            };
            if mode != self.layout_mode {
                self.layout_mode = mode;
                cx.emit(ResultsViewEvent::LayoutModeChanged(mode));
            }
        }
        // Parse result-view hints, mirroring the Swift
        // `prepareResultViewHints(sql:)`. `-- sqlly view: pivot` opens each
        // grid's own Pivot tab once the run finishes (`set_pivot_loading`).
        self.view_hints = Some(parse_view_hints(sql));
        cx.notify();
    }

    /// The SQL and connection that produced the current results — the source a
    /// CLI-equivalent export command would re-run. `None` before the first
    /// execution (or when the run carried no SQL text).
    pub(crate) fn cli_source(&self) -> Option<(sqlly_core::ConnectionProfile, String)> {
        match (&self.fk_label_profile, &self.query_sql) {
            (Some(profile), Some(sql)) if !sql.trim().is_empty() => {
                Some((profile.clone(), sql.clone()))
            }
            _ => None,
        }
    }

    pub fn set_auto_refresh_secs(&mut self, secs: Option<u64>, cx: &mut Context<Self>) {
        self.auto_refresh_secs = secs;
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn auto_refresh_secs(&self) -> Option<u64> {
        self.auto_refresh_secs
    }

    fn last_sql_is_select_only(&self) -> bool {
        self.query_sql
            .as_deref()
            .is_some_and(|sql| sqlly_intel::query::safety::classify_select_only(sql).select_only)
    }

    pub fn set_total_row_count(&mut self, slot: usize, count: TotalCount, cx: &mut Context<Self>) {
        if let TotalCount::Failed(msg) = &count {
            self.messages.push(msg.clone());
        }
        self.total_row_counts.insert(slot, count);
        cx.notify();
    }

    pub fn clear_total_row_count(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.total_row_counts.remove(&slot);
        cx.notify();
    }

    pub(super) fn clear_pending_total_counts(&mut self, cx: &mut Context<Self>) {
        self.total_row_counts
            .retain(|_, count| !matches!(count, TotalCount::Pending));
        cx.notify();
    }

    /// Whether the status strip should show "Calculate total rows" for `slot`.
    pub(super) fn total_count_link_visible(&self, slot: usize) -> bool {
        if self.is_running {
            return false;
        }
        if self.result_sets.len() != 1 {
            return false;
        }
        if self.total_row_counts.contains_key(&slot) {
            return false;
        }
        let Some(rs) = self.result_sets.get(slot) else {
            return false;
        };
        if rs.columns.is_empty() {
            return false;
        }
        let Some(sql) = self.query_sql.as_deref() else {
            return false;
        };
        if crate::count_query::count_wrapper_sql(self.source_db_type(), sql).is_err() {
            return false;
        }
        let spill_backed = matches!(self.spill_backings.get(slot), Some(Some(_)));
        if self.rows_complete_for_slot(slot) && !spill_backed {
            return false;
        }
        self.row_count_for_slot(slot) >= self.perf_prefs.spill_row_threshold
    }

    fn grid_config_for_columns(
        &self,
        slot: usize,
        columns: &[ResultColumn],
    ) -> sqlly_datatable::GridConfig {
        let effective = self.effective_formatting_for_slot(slot);
        let mut config = build_grid_config(self.classification_db(), &effective, columns);
        config.scroll_repaint_interval = self.perf_prefs.scroll_repaint_interval();
        if !self.format_hints.is_empty() {
            crate::query_format::apply_to_grid_config(&mut config, columns, &self.format_hints);
            // A `mask=` directive masks after the type tints were laid down;
            // a masked column must stay as plain as an override mask.
            if effective.color_cells_by_type {
                for column in columns {
                    if crate::query_format::display_format_for_column(column, &self.format_hints)
                        .is_some_and(|fmt| fmt.kind == crate::preferences::ResultKind::Mask)
                    {
                        crate::grid_bridge::drop_type_color_rule(&mut config, &column.name);
                    }
                }
            }
        }
        // The query's own conditional rules go first: the first matching
        // rule for a column wins.
        if !self.conditional_directives.is_empty() {
            config
                .conditional_rules
                .splice(0..0, self.conditional_directives.iter().cloned());
        }
        // Header tooltips: where each result column's values really come
        // from, resolved from the executed statement.
        crate::grid_bridge::apply_provenance_tooltips(
            &mut config,
            columns,
            self.query_sql.as_deref(),
        );
        config
    }

    /// Scale result-grid font size by `delta` (same 0.1 steps and 0.5..3.0 bounds as editor zoom).
    pub fn adjust_grid_zoom(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.grid_zoom = crate::editor_zoom::clamp_zoom(self.grid_zoom + delta);
        self.apply_density_to_live_grids(cx);
        cx.notify();
    }

    pub fn reset_grid_zoom(&mut self, cx: &mut Context<Self>) {
        self.grid_zoom = crate::editor_zoom::DEFAULT_ZOOM;
        self.zoom_wheel_accumulator = 0.0;
        self.pinch_zoom_accumulator = 0.0;
        self.apply_density_to_live_grids(cx);
        cx.notify();
    }

    /// The row height, header height and font size every live grid is
    /// painted with right now (density, results font size, zoom).
    pub(crate) fn live_grid_metrics(&self) -> crate::preferences::GridDensityMetrics {
        grid_metrics(
            self.formatting_or_load().density,
            crate::theme::Theme::results_font_size(),
            self.grid_zoom,
        )
    }

    #[cfg(test)]
    fn zoomed_grid_font_size(&self) -> f32 {
        let base = crate::theme::Theme::results_font_size()
            .unwrap_or_else(|| self.formatting_or_load().density.metrics().font_size);
        crate::editor_zoom::zoomed_font_size(base, self.grid_zoom)
    }

    fn handle_grid_zoom_wheel(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) -> bool {
        let zoom_mod = if cfg!(target_os = "macos") {
            event.modifiers.platform
        } else {
            event.modifiers.control || event.modifiers.platform
        };
        if !zoom_mod {
            return false;
        }
        let delta_lines = match event.delta {
            gpui::ScrollDelta::Lines(p) => p.y,
            gpui::ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
        };
        let steps =
            crate::editor_zoom::wheel_zoom_steps(&mut self.zoom_wheel_accumulator, delta_lines);
        if steps != 0 {
            self.adjust_grid_zoom(steps as f32 * crate::editor_zoom::ZOOM_STEP, cx);
        }
        cx.stop_propagation();
        true
    }

    fn handle_grid_pinch(&mut self, event: &gpui::PinchEvent, cx: &mut Context<Self>) {
        let steps =
            crate::editor_zoom::pinch_zoom_steps(&mut self.pinch_zoom_accumulator, event.delta);
        if steps != 0 {
            self.adjust_grid_zoom(steps as f32 * crate::editor_zoom::ZOOM_STEP, cx);
        }
    }

    fn canvas_layout_spec(
        &mut self,
        tabs_count: usize,
    ) -> Option<crate::result_layout::QueryResultLayoutSpec> {
        if let Some((n, spec)) = &self.canvas_layout
            && *n == tabs_count
        {
            return Some(spec.clone());
        }
        let spec = self
            .query_sql
            .as_ref()
            .map(|sql| crate::result_layout::parse(sql).assigning_result_sets(tabs_count))?;
        self.canvas_layout = Some((tabs_count, spec.clone()));
        Some(spec)
    }

    /// Toggle the collapsed state of a stacked card.
    pub fn toggle_card_collapsed(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(c) = self.collapsed_cards.get_mut(index) {
            *c = !*c;
        }
        cx.notify();
    }

    /// Return the interactive datatable pivot layout requested for `slot`.
    fn pivot_config_for_slot(&self, slot: usize) -> Option<PivotConfig> {
        let columns = &self.result_sets.get(slot)?.columns;
        self.view_hints
            .as_ref()?
            .pivot
            .as_ref()
            .filter(|spec| spec.result_set_index == slot)?
            .pivot_config(columns)
    }

    /// The chart layout a `-- sqlly chart:` directive requests for `slot`,
    /// when one names it.
    fn chart_directive_config(&self, slot: usize) -> Option<ChartConfig> {
        let columns = &self.result_sets.get(slot)?.columns;
        self.view_hints
            .as_ref()?
            .chart
            .as_ref()
            .filter(|spec| spec.result_set_index == slot)?
            .chart_config(columns)
    }

    /// The chart layout requested for `slot` — a saved layout from the Chart
    /// sidebar (which survives grid rebuilds) takes precedence over a
    /// `-- sqlly chart:` directive, which takes precedence over a decision-
    /// model suggestion.
    fn chart_config_for_slot(&self, slot: usize) -> Option<ChartConfig> {
        if let Some(config) = self.chart_configs.get(&slot) {
            return Some(config.clone());
        }
        if let Some(config) = self.chart_directive_config(slot) {
            return Some(config);
        }
        self.chart_suggestions.get(slot).cloned().flatten()
    }

    /// Whether `slot`'s chart is currently driven by a decision-model
    /// suggestion — a suggestion exists and nothing with higher precedence
    /// (saved layout, directive) overrides it. Drives the "Suggested chart ·
    /// Revert" caption.
    pub(crate) fn chart_suggestion_active(&self, slot: usize) -> bool {
        matches!(self.chart_suggestions.get(slot), Some(Some(_)))
            && !self.chart_configs.contains_key(&slot)
            && self.chart_directive_config(slot).is_none()
    }

    fn pad_chart_suggestion_slots(&mut self) {
        while self.chart_suggestions.len() < self.result_sets.len() {
            self.chart_suggestions.push(None);
            self.chart_suggestion_requested.push(false);
            self.chart_suggestion_reverted.push(false);
        }
    }

    /// Ask the decision model for a first chart of `slot`, once per slot,
    /// off the UI thread. Runs only when nothing configures the chart (no
    /// saved layout, no directive) and the suggestion was never reverted. A
    /// missing provider, a failed request, or a timeout leaves the slot
    /// unsuggested — the Chart tab is never blocked and behaviour is
    /// unchanged from today.
    fn maybe_spawn_chart_suggestion(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.is_running {
            return;
        }
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        if rs.columns.is_empty() {
            return;
        }
        if self
            .chart_suggestion_requested
            .get(slot)
            .copied()
            .unwrap_or(false)
            || self
                .chart_suggestion_reverted
                .get(slot)
                .copied()
                .unwrap_or(false)
        {
            return;
        }
        if self.chart_configs.contains_key(&slot) || self.chart_directive_config(slot).is_some() {
            return;
        }
        self.pad_chart_suggestion_slots();
        if let Some(flag) = self.chart_suggestion_requested.get_mut(slot) {
            *flag = true;
        }
        let Some(provider) = crate::decision_ai::decision_provider() else {
            return;
        };
        // Column names and database types only — never row values.
        let columns = self.result_sets[slot].columns.clone();
        let row_count = self.result_sets[slot].rows.len();
        let generation = self.run_generation;
        cx.spawn(async move |this, cx| {
            let Some(config) =
                crate::chart_suggest::suggest_chart(provider.as_ref(), &columns, row_count).await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.run_generation != generation {
                    return;
                }
                this.pad_chart_suggestion_slots();
                if let Some(entry) = this.chart_suggestions.get_mut(slot) {
                    *entry = Some(config);
                }
                this.apply_chart_suggestion_to_grid(slot, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Spawn a suggestion for the active slot when its Chart tab is in
    /// front. Called from `render`, so a tab click inside the grid widget —
    /// which notifies the grid entity the view observes — reaches it.
    fn maybe_spawn_chart_suggestion_for_active(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        let chart_in_front = self
            .grid_views
            .get(slot)
            .and_then(|view| view.as_ref())
            .is_some_and(|view| view.read(cx).active_tab() == GridTab::Chart);
        if chart_in_front {
            self.maybe_spawn_chart_suggestion(slot, cx);
        }
    }

    /// Push the active suggestion into `slot`'s live chart so it appears
    /// without a grid rebuild. No-op when something with higher precedence
    /// configures the chart.
    fn apply_chart_suggestion_to_grid(&mut self, slot: usize, cx: &mut Context<Self>) {
        if !self.chart_suggestion_active(slot) {
            return;
        }
        let Some(config) = self.chart_suggestions.get(slot).cloned().flatten() else {
            return;
        };
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return;
        };
        view.clone().update(cx, |table, cx| {
            table.enable_chart(config, cx);
        });
    }

    /// The "Revert" side of the suggestion caption: drop the suggestion, pin
    /// "no suggestion" for the slot, and put the chart back to its own
    /// auto-detection.
    pub(crate) fn revert_chart_suggestion(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        self.pad_chart_suggestion_slots();
        let had_suggestion = self.chart_suggestion_active(slot);
        if let Some(entry) = self.chart_suggestions.get_mut(slot) {
            *entry = None;
        }
        if let Some(flag) = self.chart_suggestion_reverted.get_mut(slot) {
            *flag = true;
        }
        if had_suggestion && let Some(Some(view)) = self.grid_views.get(slot) {
            view.clone().update(cx, |table, cx| {
                table.enable_chart(ChartConfig::default(), cx);
            });
        }
        cx.notify();
    }

    /// The live chart layout for `slot` as a name-based spec: the grid's
    /// current chart state when the Chart tab has been enabled (unsaved
    /// sidebar tweaks included), else whatever layout would configure it.
    /// `None` when the slot has no result set or a referenced column index
    /// is out of range.
    pub fn chart_spec_for_slot(
        &self,
        slot: usize,
        cx: &gpui::App,
    ) -> Option<crate::result_chart::ResultChartSpec> {
        let result_set = self.result_sets.get(slot)?;
        let live = self
            .grid_views
            .get(slot)
            .and_then(|view| view.as_ref())
            .and_then(|view| view.read(cx).chart_state().cloned())
            .map(|state| state.read(cx).config.clone());
        let config = live
            .or_else(|| self.chart_config_for_slot(slot))
            .unwrap_or_default();
        crate::result_chart::ResultChartSpec::from_config(
            &config,
            &result_set.columns,
            result_set.index,
        )
    }

    /// Ask the host to save `slot`'s chart to the library — the grid
    /// context menu's "Save Chart to Library…". Nothing to save without a
    /// result set or the SQL that produced it.
    pub fn request_save_chart_to_library(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(spec) = self.chart_spec_for_slot(slot, cx) else {
            return;
        };
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        cx.emit(ResultsViewEvent::SaveChartToLibraryRequested { spec, sql });
    }

    /// The toolbar chip's action: save the active slot's chart.
    pub(crate) fn request_save_active_chart(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        self.request_save_chart_to_library(slot, cx);
    }

    /// Whether `slot`'s grid currently shows its Chart tab.
    pub(crate) fn chart_tab_in_front(&self, slot: usize, cx: &gpui::App) -> bool {
        self.grid_views
            .get(slot)
            .and_then(|view| view.as_ref())
            .is_some_and(|view| view.read(cx).active_tab() == GridTab::Chart)
    }

    /// Whether the Chart tab should auto-open on `slot` — the query asked for
    /// `-- sqlly view: chart` and a chart layout resolves for it.
    fn auto_chart_for_slot(&self, slot: usize) -> bool {
        self.view_hints
            .as_ref()
            .is_some_and(|hints| hints.default_view == ResultViewKind::Chart)
            && self.chart_config_for_slot(slot).is_some()
    }

    /// The Pivot/Chart tab lock for `slot`. Locked while the query streams,
    /// and locked for a spill-backed set whose grid has no row source
    /// (`has_row_source`): without one, the pivot and chart would aggregate
    /// only the resident window and silently show a partial set. With one,
    /// both read the whole spill file in the background.
    fn pivot_chart_lock_for_slot(
        &self,
        slot: usize,
        has_row_source: bool,
    ) -> (bool, Option<String>) {
        if self.is_running {
            return (
                true,
                Some(crate::localization::text(
                    "results.pivot_chart.loading_locked",
                )),
            );
        }
        if matches!(self.spill_backings.get(slot), Some(Some(_))) && !has_row_source {
            return (
                true,
                Some(crate::localization::text(
                    "results.pivot_chart.result_too_large_locked",
                )),
            );
        }
        (false, None)
    }

    fn set_pivot_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if !loading {
            // A finished spill-backed set gets its whole-file row source
            // (sized to the final total) before the lock is recomputed.
            for slot in 0..self.grid_views.len() {
                if matches!(self.spill_backings.get(slot), Some(Some(_))) {
                    self.sync_row_source_for_slot(slot, cx);
                }
            }
        }
        let auto_pivot = !loading
            && self
                .view_hints
                .as_ref()
                .is_some_and(|hints| hints.default_view == ResultViewKind::Pivot);
        for (slot, view) in self.grid_views.iter().enumerate() {
            let Some(view) = view else { continue };
            let (locked, status) = if loading {
                (
                    true,
                    Some(crate::localization::text(
                        "results.pivot_chart.loading_locked",
                    )),
                )
            } else {
                self.pivot_chart_lock_for_slot(slot, view.read(cx).row_source().is_some())
            };
            let open_pivot = auto_pivot && !locked && self.pivot_config_for_slot(slot).is_some();
            let open_chart = !loading && !locked && self.auto_chart_for_slot(slot);
            view.update(cx, |table, cx| {
                table.set_pivot_locked(locked, status.clone());
                table.set_chart_locked(locked, status.clone());
                if open_pivot {
                    table.set_active_tab(GridTab::Pivot, cx);
                } else if open_chart {
                    table.set_active_tab(GridTab::Chart, cx);
                }
                cx.notify();
            });
        }
    }

    /// The pivoted table for `slot` in core export types — what the pivot
    /// grid displays. `None` when the slot has no valid pivot directive (or
    /// the set's rows are incomplete).
    pub fn pivot_export_table(
        &self,
        slot: usize,
        cx: &gpui::App,
    ) -> Option<(Vec<ResultColumn>, Vec<Vec<CellValue>>)> {
        let rs = self.result_sets.get(slot)?;
        if rs.columns.is_empty() || !self.rows_complete_for_slot(slot) {
            return None;
        }
        let spec = self
            .view_hints
            .as_ref()?
            .pivot
            .as_ref()
            .filter(|spec| spec.result_set_index == slot)?;
        let rows = self.resident_source_rows_for_slot(slot, cx);
        let built = crate::result_pivot::build(&rs.columns, &rows, spec).ok()?;
        Some((built.table.columns, built.table.rows))
    }

    /// Enable or disable difference tracking. When enabled, the next
    /// `start_query` saves the current result sets as the "previous" set.
    pub fn set_track_difference(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.track_difference = enabled;
        if !enabled {
            let was_diff = self.active_output_tab() == OutputPanelTab::Diff;
            self.diff = None;
            self.retained_diff_runs.clear();
            self.diff_baseline_run = None;
            self.diff_comparison_run = None;
            self.diff_key_columns.clear();
            self.detailed_diff = None;
            self.diff_unavailable_reason = None;
            self.detailed_diff_grid = None;
            self.show_diff = false;
            if was_diff {
                self.select_output_tab(OutputPanelTab::Results, cx);
                return;
            }
        }
        cx.notify();
    }

    fn retain_completed_diff_run(&mut self, cx: &mut Context<Self>) {
        if !self.track_difference || self.result_sets.is_empty() {
            return;
        }
        let id = self.next_diff_run_id;
        self.next_diff_run_id += 1;
        let sets = self
            .result_sets
            .iter()
            .enumerate()
            .map(|(slot, rs)| {
                let row_count = self.row_count_for_slot(slot);
                let unavailable_reason = if matches!(self.spill_backings.get(slot), Some(Some(_))) {
                    Some(crate::localization::text(
                        "results.diff.spill_backed_run_unavailable",
                    ))
                } else if !self.rows_complete_for_slot(slot) {
                    Some(crate::localization::text(
                        "results.diff.incomplete_run_unavailable",
                    ))
                } else if row_count > result_set_difference::DIFF_MAX_ROWS {
                    Some(crate::localization::format(
                        "results.diff.safe_row_limit_unavailable",
                        &[
                            ("row_count", &row_count.to_string()),
                            (
                                "row_limit",
                                &result_set_difference::DIFF_MAX_ROWS.to_string(),
                            ),
                        ],
                    ))
                } else {
                    None
                };
                RetainedDiffSet {
                    columns: rs.columns.clone(),
                    rows: unavailable_reason
                        .is_none()
                        .then(|| self.resident_source_rows_for_slot(slot, cx)),
                    row_count,
                    unavailable_reason,
                }
            })
            .collect();
        self.retained_diff_runs.push(RetainedDiffRun {
            id,
            label: crate::localization::format(
                "results.diff.run_label",
                &[("run_id", &id.to_string())],
            ),
            sets,
        });
        if self.retained_diff_runs.len() > result_set_difference::DIFF_MAX_RUNS {
            self.retained_diff_runs.remove(0);
        }
        let len = self.retained_diff_runs.len();
        self.diff_comparison_run = self.retained_diff_runs.last().map(|run| run.id);
        self.diff_baseline_run = (len >= 2).then(|| self.retained_diff_runs[len - 2].id);
        self.recompute_detailed_diff(cx);
    }

    fn snapshot_live_sets(&self, cx: &mut Context<Self>) -> Vec<RetainedRunSet> {
        let quarter = self.run_history_budget_bytes / 4;
        self.result_sets
            .iter()
            .enumerate()
            .map(|(slot, rs)| {
                let row_count = self.row_count_for_slot(slot);
                let spill = matches!(self.spill_backings.get(slot), Some(Some(_)));
                let incomplete = !self.rows_complete_for_slot(slot);
                let estimate = estimate_set_bytes(rs);
                let (rows, unavailable_reason) = if spill {
                    (
                        None,
                        Some(crate::localization::text(
                            "results.run_history.spill_backed_unavailable",
                        )),
                    )
                } else if incomplete {
                    (
                        None,
                        Some(crate::localization::text(
                            "results.run_history.incomplete_unavailable",
                        )),
                    )
                } else if quarter > 0 && estimate > quarter {
                    (
                        None,
                        Some(crate::localization::text(
                            "results.run_history.memory_budget_unavailable",
                        )),
                    )
                } else {
                    (Some(self.resident_source_rows_for_slot(slot, cx)), None)
                };
                RetainedRunSet {
                    columns: rs.columns.clone(),
                    rows,
                    row_count,
                    rows_affected: rs.rows_affected,
                    unavailable_reason,
                }
            })
            .collect()
    }

    fn build_live_snapshot(
        &mut self,
        assign_id: bool,
        cx: &mut Context<Self>,
    ) -> Option<RetainedRun> {
        // A run with no result sets but server messages (row counts, PRINT
        // output, DB-context changes) is still worth retaining — the Messages
        // pane is its only content.
        if (self.result_sets.is_empty() && self.messages.is_empty()) || self.is_running {
            return None;
        }
        let id = if assign_id {
            let id = self.next_run_id;
            self.next_run_id += 1;
            id
        } else {
            0
        };
        Some(RetainedRun {
            id,
            label: crate::localization::format(
                "results.run_history.run_label",
                &[("run_id", &id.to_string())],
            ),
            completed_at: self.completed_wall.unwrap_or_else(SystemTime::now),
            elapsed_ms: self.elapsed_ms,
            sql: self.query_sql.clone().unwrap_or_default(),
            pinned: false,
            sets: self.snapshot_live_sets(cx),
            messages: self.messages.clone(),
        })
    }

    fn push_retained_run(&mut self, run: RetainedRun, limit: usize, force: bool) {
        if !force && !self.should_capture_run(&run) {
            return;
        }
        self.retained_runs.push(run);
        self.evict_retained_runs(limit);
    }

    /// Whether `run` differs enough from the most recent retained run to be
    /// worth storing, per the user's capture-mode preference. Forced keeps
    /// (pinning / Execute into New Result Tab) bypass this.
    fn should_capture_run(&self, run: &RetainedRun) -> bool {
        use crate::preferences::RunHistoryCaptureMode as Mode;
        let Some(prev) = self.retained_runs.last() else {
            return true;
        };
        let query_changed = prev.sql != run.sql;
        // Messages are part of "results" for change detection: a message-only
        // run (no grids) still differs from its predecessor when its row counts
        // or PRINT output differ.
        let results_changed =
            !retained_sets_equal(&prev.sets, &run.sets) || prev.messages != run.messages;
        match self.run_history_capture {
            Mode::QueryChange => query_changed,
            Mode::ResultsChange => results_changed,
            Mode::Either => query_changed || results_changed,
            Mode::Both => query_changed && results_changed,
        }
    }

    fn evict_retained_runs(&mut self, limit: usize) {
        loop {
            let over_count = self.retained_runs.len() > limit;
            let total: usize = self.retained_runs.iter().map(estimate_run_bytes).sum();
            let over_budget =
                self.run_history_budget_bytes > 0 && total > self.run_history_budget_bytes;
            if !over_count && !over_budget {
                break;
            }
            let Some(idx) = self.retained_runs.iter().position(|run| !run.pinned) else {
                break;
            };
            self.retained_runs.remove(idx);
        }
    }

    /// Automatic-capture limit: zero when the feature is switched off, so no
    /// runs are retained unless a forced keep overrides it.
    fn effective_history_limit(&self) -> usize {
        if self.run_history_enabled {
            self.run_history_limit
        } else {
            0
        }
    }

    fn retain_current_run(&mut self, force: bool, cx: &mut Context<Self>) {
        if (self.result_sets.is_empty() && self.messages.is_empty()) || self.is_running {
            return;
        }
        let base = self.effective_history_limit();
        let limit = if force { base.max(1) } else { base };
        if limit == 0 {
            return;
        }
        let Some(run) = self.build_live_snapshot(true, cx) else {
            return;
        };
        self.push_retained_run(run, limit, force);
    }

    fn retain_history_before_new_run(&mut self, cx: &mut Context<Self>) {
        let base = self.effective_history_limit();
        let keep = base > 0 || self.pending_keep_results;
        if !keep {
            return;
        }
        if self.viewing_run.is_some() {
            if let Some(mut snap) = self.current_run_snapshot.take() {
                let limit = if self.pending_keep_results {
                    base.max(1)
                } else {
                    base
                };
                if limit > 0 {
                    snap.id = self.next_run_id;
                    self.next_run_id += 1;
                    self.push_retained_run(snap, limit, self.pending_keep_results);
                }
            }
        } else {
            self.retain_current_run(self.pending_keep_results, cx);
        }
    }

    fn retained_run_banner(run: &RetainedRun) -> String {
        let time = format_run_clock(run.completed_at);
        let rows: usize = run.sets.iter().map(|set| set.row_count).sum();
        let elapsed = run
            .elapsed_ms
            .map(format_duration_ms)
            .unwrap_or_else(|| "—".to_string());
        // A run that returned no result sets has only its messages to describe
        // it — "0 rows" would read like an empty grid that was never there.
        let content = if run.sets.is_empty() && !run.messages.is_empty() {
            crate::localization::format(
                if run.messages.len() == 1 {
                    "results.run_history.message_count_content_one"
                } else {
                    "results.run_history.message_count_content"
                },
                &[("message_count", &run.messages.len().to_string())],
            )
        } else {
            crate::localization::format(
                "results.run_history.row_count_content",
                &[("row_count", &format_count(rows))],
            )
        };
        let reason = run
            .sets
            .iter()
            .find_map(|set| set.unavailable_reason.as_deref());
        let mut arguments = vec![
            ("time", time.as_str()),
            ("content", content.as_str()),
            ("elapsed", elapsed.as_str()),
        ];
        if let Some(reason) = reason {
            arguments.push(("unavailable_reason", reason));
            crate::localization::format(
                "results.run_history.banner_with_unavailable_reason",
                &arguments,
            )
        } else {
            crate::localization::format("results.run_history.banner", &arguments)
        }
    }

    fn install_retained_sets(
        &mut self,
        banner: Option<String>,
        sets: &[RetainedRunSet],
        messages: &[String],
        elapsed_ms: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        self.result_sets = sets
            .iter()
            .enumerate()
            .map(|(i, set)| ResultSet {
                index: i,
                columns: set.columns.clone(),
                rows: set.rows.clone().unwrap_or_default(),
                rows_affected: set.rows_affected,
                row_count_hint: set.row_count,
                ..Default::default()
            })
            .collect();
        self.messages = messages.to_vec();
        // A restored run that produced only messages has no grid to fall back
        // to; open the Messages pane so its content is visible instead of a
        // blank content area.
        self.show_messages = self.result_sets.is_empty() && !self.messages.is_empty();
        self.active_tab = 0;
        self.historical_banner = banner;
        self.elapsed_ms = elapsed_ms;
        self.ensure_grid_for_active_tab(cx);
    }

    pub fn switch_to_run(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        match id {
            None => {
                if self.viewing_run.is_none() {
                    return;
                }
                if let Some(snap) = self.current_run_snapshot.clone() {
                    self.install_retained_sets(
                        None,
                        &snap.sets,
                        &snap.messages,
                        snap.elapsed_ms,
                        cx,
                    );
                }
                self.viewing_run = None;
            }
            Some(run_id) => {
                if self.viewing_run == Some(run_id) {
                    return;
                }
                if self.viewing_run.is_none() && self.current_run_snapshot.is_none() {
                    self.current_run_snapshot = self.build_live_snapshot(false, cx);
                }
                let Some(run) = self
                    .retained_runs
                    .iter()
                    .find(|run| run.id == run_id)
                    .cloned()
                else {
                    return;
                };
                let banner = Self::retained_run_banner(&run);
                self.install_retained_sets(
                    Some(banner),
                    &run.sets,
                    &run.messages,
                    run.elapsed_ms,
                    cx,
                );
                self.viewing_run = Some(run_id);
            }
        }
        cx.emit(ResultsViewEvent::ActiveResultChanged);
        cx.notify();
    }

    pub fn pin_run(&mut self, id: u64, pinned: bool, cx: &mut Context<Self>) {
        if let Some(run) = self.retained_runs.iter_mut().find(|run| run.id == id) {
            run.pinned = pinned;
            cx.notify();
        }
    }

    pub fn close_run(&mut self, id: u64, cx: &mut Context<Self>) {
        let was_viewing = self.viewing_run == Some(id);
        self.retained_runs.retain(|run| run.id != id);
        if was_viewing {
            self.switch_to_run(None, cx);
        } else {
            cx.notify();
        }
    }

    pub fn keep_results_on_next_run(&mut self) {
        self.pending_keep_results = true;
    }

    fn cycle_diff_run(&mut self, baseline: bool, cx: &mut Context<Self>) {
        if self.retained_diff_runs.is_empty() {
            return;
        }
        let selected = if baseline {
            self.diff_baseline_run
        } else {
            self.diff_comparison_run
        };
        let current = selected
            .and_then(|id| self.retained_diff_runs.iter().position(|run| run.id == id))
            .unwrap_or(0);
        let next = self.retained_diff_runs[(current + 1) % self.retained_diff_runs.len()].id;
        if baseline {
            self.diff_baseline_run = Some(next);
        } else {
            self.diff_comparison_run = Some(next);
        }
        self.recompute_detailed_diff(cx);
    }

    fn toggle_diff_key_column(&mut self, column: usize, cx: &mut Context<Self>) {
        if let Some(position) = self
            .diff_key_columns
            .iter()
            .position(|item| *item == column)
        {
            self.diff_key_columns.remove(position);
        } else {
            self.diff_key_columns.push(column);
            self.diff_key_columns.sort_unstable();
        }
        self.recompute_detailed_diff(cx);
    }

    fn selected_diff_run_label(&self, baseline: bool) -> String {
        let selected = if baseline {
            self.diff_baseline_run
        } else {
            self.diff_comparison_run
        };
        selected
            .and_then(|id| self.retained_diff_runs.iter().find(|run| run.id == id))
            .map(|run| {
                run.sets
                    .get(self.active_tab)
                    .map(|set| {
                        crate::localization::format(
                            "results.diff.selected_run_with_rows",
                            &[
                                ("run_label", &run.label),
                                ("row_count", &set.row_count.to_string()),
                            ],
                        )
                    })
                    .unwrap_or_else(|| run.label.clone())
            })
            .unwrap_or_else(|| crate::localization::text("results.diff.select_run"))
    }

    fn recompute_detailed_diff(&mut self, cx: &mut Context<Self>) {
        self.detailed_diff = None;
        self.diff_unavailable_reason = None;
        self.detailed_diff_grid = None;
        // A loaded snapshot baseline outranks the retained-run picker.
        if let Some(snapshot) = &self.diff_snapshot_baseline {
            let Some(comparison_id) = self.diff_comparison_run else {
                self.diff_unavailable_reason = Some(crate::localization::text(
                    "results.diff.choose_comparison_run",
                ));
                cx.notify();
                return;
            };
            let slot = self.active_tab;
            let comparison = self
                .retained_diff_runs
                .iter()
                .find(|run| run.id == comparison_id)
                .and_then(|run| run.sets.get(slot))
                .cloned();
            let Some(comparison) = comparison else {
                self.diff_unavailable_reason = Some(crate::localization::text(
                    "results.diff.snapshot_comparison_incompatible",
                ));
                cx.notify();
                return;
            };
            if let Some(reason) = comparison.unavailable_reason {
                self.diff_unavailable_reason = Some(reason);
                cx.notify();
                return;
            }
            let result = result_set_difference::compare_detailed(
                &snapshot.columns,
                &snapshot.rows,
                &comparison.columns,
                comparison.rows.as_deref().unwrap_or_default(),
                &self.diff_key_columns,
            );
            match result {
                Ok(diff) => {
                    self.detailed_diff_grid = self.build_detailed_diff_grid(&diff, cx);
                    self.detailed_diff = Some(diff);
                }
                Err(reason) => self.diff_unavailable_reason = Some(reason),
            }
            cx.notify();
            return;
        }
        let Some(baseline_id) = self.diff_baseline_run else {
            self.diff_unavailable_reason = Some(crate::localization::text(
                "results.diff.complete_another_run",
            ));
            cx.notify();
            return;
        };
        let Some(comparison_id) = self.diff_comparison_run else {
            self.diff_unavailable_reason = Some(crate::localization::text(
                "results.diff.choose_comparison_run",
            ));
            cx.notify();
            return;
        };
        if baseline_id == comparison_id {
            self.diff_unavailable_reason = Some(crate::localization::text(
                "results.diff.choose_different_runs",
            ));
            cx.notify();
            return;
        }
        let slot = self.active_tab;
        let selected = |id| {
            self.retained_diff_runs
                .iter()
                .find(|run| run.id == id)
                .and_then(|run| run.sets.get(slot))
                .cloned()
        };
        let Some(baseline) = selected(baseline_id) else {
            self.diff_unavailable_reason = Some(crate::localization::text(
                "results.diff.baseline_incompatible",
            ));
            cx.notify();
            return;
        };
        let Some(comparison) = selected(comparison_id) else {
            self.diff_unavailable_reason = Some(crate::localization::text(
                "results.diff.retained_comparison_incompatible",
            ));
            cx.notify();
            return;
        };
        if let Some(reason) = baseline
            .unavailable_reason
            .or(comparison.unavailable_reason)
        {
            self.diff_unavailable_reason = Some(reason);
            cx.notify();
            return;
        }
        let result = result_set_difference::compare_detailed(
            &baseline.columns,
            baseline.rows.as_deref().unwrap_or_default(),
            &comparison.columns,
            comparison.rows.as_deref().unwrap_or_default(),
            &self.diff_key_columns,
        );
        match result {
            Ok(diff) => {
                self.detailed_diff_grid = self.build_detailed_diff_grid(&diff, cx);
                self.detailed_diff = Some(diff);
            }
            Err(reason) => self.diff_unavailable_reason = Some(reason),
        }
        cx.notify();
    }

    fn build_detailed_diff_grid(
        &self,
        diff: &result_set_difference::DetailedDiff,
        cx: &mut Context<Self>,
    ) -> Option<Entity<SqllyDataTable>> {
        let (columns, rows) = detailed_diff_table(diff, self.diff_class_filter);
        self.build_diff_grid(&columns, &rows, cx)
    }

    /// Bind host-owned History/DDL panels so the output tabs can render them.
    pub fn bind_output_panels(
        &mut self,
        #[cfg(not(target_arch = "wasm32"))] history: Option<
            Entity<crate::history_panel::HistoryPanelView>,
        >,
        ddl: Option<Entity<crate::ddl_viewer::DdlViewerView>>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.history_panel = history;
        }
        self.ddl_panel = ddl;
    }

    pub fn set_pane_maximized(&mut self, maximized: bool) {
        self.pane_maximized = maximized;
    }

    pub fn active_output_tab(&self) -> OutputPanelTab {
        if self.show_history {
            OutputPanelTab::History
        } else if self.show_ddl {
            OutputPanelTab::Ddl
        } else if self.show_preview_sql {
            OutputPanelTab::PreviewSql
        } else if self.show_execution_plan {
            OutputPanelTab::Plan
        } else if self.show_diff {
            OutputPanelTab::Diff
        } else if self.show_statistics {
            OutputPanelTab::Statistics
        } else if self.show_messages {
            OutputPanelTab::Messages
        } else {
            OutputPanelTab::Results
        }
    }

    fn clear_output_tab_flags(&mut self) {
        self.show_messages = false;
        self.show_diff = false;
        self.show_execution_plan = false;
        self.show_history = false;
        self.show_ddl = false;
        self.show_preview_sql = false;
        self.show_result_profile = false;
        self.show_statistics = false;
    }

    /// Open the Client Statistics pane for the current run, bringing it forward
    /// and making its tab visible. Windowless so the grid context menu can open
    /// it. The tab stays in the bar after switching away (`statistics_opened`),
    /// so it can be reselected like any other output tab. Routed through
    /// `select_output_tab` so `OutputTabSelected` fires and the app-level
    /// per-tab output state stays in sync.
    pub fn open_client_statistics(&mut self, cx: &mut Context<Self>) {
        self.statistics_opened = true;
        self.select_output_tab(OutputPanelTab::Statistics, cx);
    }

    /// Gather the run metrics for the Client Statistics pane.
    pub(crate) fn client_stat_inputs(&self) -> client_stats::ClientStatInputs {
        // Show "Rows affected" whenever the server reported the number at all —
        // a DML run that matched nothing shows an honest 0, not a missing row.
        let has_affected = self.result_sets.iter().any(|rs| rs.rows_affected.is_some());
        let rows_affected: u64 = self
            .result_sets
            .iter()
            .filter_map(|rs| rs.rows_affected)
            .sum();
        client_stats::ClientStatInputs {
            result_sets: self.result_sets.len(),
            total_rows: self.total_rows(),
            rows_affected: has_affected.then_some(rows_affected),
            server_ms: self.elapsed_ms,
            first_response_ms: self.first_response_ms,
            client_paint_ms: self.ui_paint_ms,
            wall_clock_ms: self.wall_clock_ms,
        }
    }

    pub fn select_output_tab(&mut self, tab: OutputPanelTab, cx: &mut Context<Self>) {
        self.clear_output_tab_flags();
        match tab {
            OutputPanelTab::Results => {}
            OutputPanelTab::Messages => self.show_messages = true,
            OutputPanelTab::Plan => {
                self.show_execution_plan = true;
                let _ = self.execution_plan_view(cx);
            }
            OutputPanelTab::History => self.show_history = true,
            OutputPanelTab::Ddl => self.show_ddl = true,
            OutputPanelTab::PreviewSql => self.show_preview_sql = true,
            OutputPanelTab::Diff => self.show_diff = true,
            OutputPanelTab::Statistics => self.show_statistics = true,
        }
        cx.emit(ResultsViewEvent::OutputTabSelected(tab));
        cx.notify();
    }

    /// Show or hide the diff tab.
    #[allow(dead_code)]
    pub fn toggle_diff_view(&mut self, cx: &mut Context<Self>) {
        self.show_diff = !self.show_diff;
        cx.notify();
    }

    /// A bounded AI prompt describing the captured execution plan, or `None`
    /// when no plan has been captured yet. Read by the app's "Explain Execution
    /// Plan (AI)" command.
    pub fn ai_plan_explain_prompt(&self, cx: &App) -> Option<String> {
        self.execution_plan_view
            .as_ref()
            .and_then(|view| view.read(cx).ai_explain_prompt())
    }

    /// Record which plan kinds the active engine can capture and pass it to
    /// the plan pane if one exists. No-op (and no notify) when unchanged, so
    /// the app can push it every frame.
    pub fn set_plan_capture_gate(
        &mut self,
        gate: crate::capabilities::PlanCaptureGate,
        cx: &mut Context<Self>,
    ) {
        if self.plan_capture_gate == gate {
            return;
        }
        self.plan_capture_gate = gate.clone();
        if let Some(view) = &self.execution_plan_view {
            view.update(cx, |view, cx| view.set_capture_gate(gate, cx));
        }
    }

    /// The plan-capture gate last pushed by the app.
    pub fn plan_capture_gate(&self) -> &crate::capabilities::PlanCaptureGate {
        &self.plan_capture_gate
    }

    /// The plan pane entity, created on first use with its event subscription.
    fn execution_plan_view(&mut self, cx: &mut Context<Self>) -> Entity<ExecutionPlanView> {
        if let Some(view) = &self.execution_plan_view {
            return view.clone();
        }
        let view = cx.new(ExecutionPlanView::new);
        let gate = self.plan_capture_gate.clone();
        view.update(cx, |view, cx| view.set_capture_gate(gate, cx));
        cx.subscribe(&view, |this, _emitter, event, cx| match event {
            ExecutionPlanViewEvent::CaptureRequested(kind) => {
                cx.emit(ResultsViewEvent::ExecutionPlanCaptureRequested(*kind));
                this.show_execution_plan = true;
            }
            ExecutionPlanViewEvent::OpenPlanRequested => {
                cx.emit(ResultsViewEvent::ExecutionPlanOpenRequested);
                this.show_execution_plan = true;
            }
            ExecutionPlanViewEvent::PastePlanRequested => {
                cx.emit(ResultsViewEvent::ExecutionPlanPasteRequested);
                this.show_execution_plan = true;
            }
            ExecutionPlanViewEvent::OpenSqlInNewTab(sql) => {
                cx.emit(ResultsViewEvent::ExecutionPlanOpenSqlInNewTab(sql.clone()));
            }
        })
        .detach();
        self.execution_plan_view = Some(view.clone());
        view
    }

    /// Mark a plan capture as in flight and bring the pane forward.
    pub fn begin_execution_plan_capture(&mut self, kind: PlanKind, cx: &mut Context<Self>) {
        let view = self.execution_plan_view(cx);
        view.update(cx, |view, cx| view.set_capturing(kind, cx));
        self.clear_output_tab_flags();
        self.show_execution_plan = true;
        self.show_messages = false;
        self.show_diff = false;
        self.show_result_profile = false;
        cx.notify();
    }

    /// Install plans with the editor SQL that produced them so the plan pane
    /// can reject comparisons across different query-history contexts without
    /// re-executing either statement.
    pub fn set_execution_plan_for_context(
        &mut self,
        kind: PlanKind,
        query_context: String,
        plans: Vec<sqlly_services::plans::ExecutionPlan>,
        cx: &mut Context<Self>,
    ) {
        let view = self.execution_plan_view(cx);
        view.update(cx, |view, cx| {
            view.set_plans_for_context(kind, query_context, plans, cx)
        });
        self.show_execution_plan = true;
        cx.notify();
    }

    /// Surface a capture/parse failure in the pane.
    pub fn fail_execution_plan(&mut self, kind: PlanKind, message: String, cx: &mut Context<Self>) {
        let view = self.execution_plan_view(cx);
        view.update(cx, |view, cx| view.set_failed(kind, message, cx));
        self.show_execution_plan = true;
        cx.notify();
    }

    /// Footer toggle. When the pane is open this returns to the grid; when
    /// closed it opens the pane (creating it — showing the pre-capture
    /// prompt — if no Explain has run yet).
    #[cfg(test)]
    pub fn toggle_execution_plan_pane(&mut self, cx: &mut Context<Self>) {
        self.show_execution_plan = !self.show_execution_plan;
        if self.show_execution_plan {
            self.execution_plan_view(cx);
            self.show_messages = false;
            self.show_diff = false;
            self.show_result_profile = false;
        }
        cx.notify();
    }

    /// Findings count for the footer toggle label, once a plan exists.
    pub fn execution_plan_finding_count(&self, cx: &App) -> Option<usize> {
        self.execution_plan_view
            .as_ref()
            .map(|view| view.read(cx).finding_count())
    }

    /// True while a plan capture is in flight — used to ignore repeat
    /// Explain requests (⌘L, palette, menu) until the current one lands.
    pub fn is_capturing_execution_plan(&self, cx: &App) -> bool {
        self.execution_plan_view
            .as_ref()
            .is_some_and(|view| view.read(cx).is_capturing())
    }

    /// Bring forward the Chart tab on `slot`'s result grid — the grid
    /// context menu's "Chart…". The chart lives on the grid itself and reads
    /// its rows directly, so there is no snapshot to rebuild; the saved
    /// layout (the Chart sidebar's save button) is applied when the grid is
    /// built, and re-picking the menu item must not reset live, unsaved
    /// sidebar tweaks — it only switches tabs.
    pub fn open_result_chart_tab(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.ensure_grid_for_slot(slot, cx);
        self.maybe_spawn_chart_suggestion(slot, cx);
        let Some(grid) = self.grid_views.get(slot).and_then(|view| view.clone()) else {
            return;
        };
        let config = self.chart_config_for_slot(slot).unwrap_or_default();
        grid.update(cx, |table, cx| {
            if table.chart_state().is_none() {
                table.enable_chart(config, cx);
            }
            table.set_active_tab(GridTab::Chart, cx);
        });
        cx.notify();
    }

    /// Profile every column of one result set and show the summary in a real
    /// grid tab. Bounded like the single-column clipboard profile: at most
    /// [`crate::result_profiler::PROFILE_MAX_ROWS`] rows, spill-backed sets
    /// read through the pager instead of materializing.
    pub fn open_result_profile(&mut self, slot: usize, cx: &mut Context<Self>) {
        use crate::result_profiler::{PROFILE_MAX_ROWS, ResultProfiler};
        let Some(result_set) = self.result_sets.get(slot) else {
            return;
        };
        let columns = result_set.columns.clone();
        let limit = PROFILE_MAX_ROWS.saturating_add(1);
        // A windowed result is read from disk in the background first.
        let weak = cx.weak_entity();
        if self.defer_until_spill_prefixes(
            vec![(slot, limit)],
            crate::localization::format(
                "results.profiling.all_columns_reading",
                &[("row_count", &self.spill_read_count(slot, limit).to_string())],
            ),
            cx,
            move |cx| {
                let _ = weak.update(cx, |this, cx| this.open_result_profile(slot, cx));
            },
        ) {
            return;
        }
        let rows: Vec<Vec<CellValue>> = match self.spill_rows_for_slot_capped(slot, limit) {
            Some(rows) => rows,
            None => self
                .result_sets
                .get(slot)
                .map(|rs| {
                    if !rs.rows.is_empty() {
                        rs.rows.iter().take(limit).cloned().collect()
                    } else {
                        self.resident_source_rows_for_slot(slot, cx)
                            .into_iter()
                            .take(limit)
                            .collect()
                    }
                })
                .unwrap_or_default(),
        };
        let mut profiler = ResultProfiler::new(&columns);
        profiler.push_rows(&rows);
        let profile = profiler.finish();
        let label = self.result_set_label(slot);
        self.result_profile_caption = crate::localization::format(
            if profile.rows_truncated {
                "results.profiling.all_columns_truncated_caption"
            } else {
                "results.profiling.all_columns_complete_caption"
            },
            &[
                ("result_label", &label),
                ("profiled_rows", &profile.rows_profiled.to_string()),
            ],
        );
        self.result_profile = Some(profile);
        self.result_profile_sql = self.query_sql.clone();
        self.result_profile_filter = crate::result_profiler::ProfileFilter::default();
        self.rebuild_result_profile_grid(cx);
        self.clear_output_tab_flags();
        self.show_result_profile = true;
        cx.notify();
    }

    /// Drop every cached grid so the next render rebuilds them with fresh
    /// formatting (used when conditional-formatting rules change).
    pub fn refresh_grid_formatting(&mut self, cx: &mut Context<Self>) {
        for slot in self.grid_views.iter_mut() {
            *slot = None;
        }
        cx.notify();
    }

    /// Profile a single column into the same Profile pane "Profile All
    /// Columns" uses, reading only that column's cells (spill-aware, capped).
    pub fn open_result_profile_for_column(
        &mut self,
        slot: usize,
        column_index: usize,
        cx: &mut Context<Self>,
    ) {
        use crate::result_profiler::{PROFILE_MAX_ROWS, ResultProfiler};
        let Some(result_set) = self.result_sets.get(slot) else {
            return;
        };
        let Some(column) = result_set.columns.get(column_index).cloned() else {
            return;
        };
        let limit = PROFILE_MAX_ROWS.saturating_add(1);
        // A windowed result is read from disk in the background first.
        let weak = cx.weak_entity();
        if self.defer_until_spill_prefixes(
            vec![(slot, limit)],
            crate::localization::format(
                "results.profiling.single_column_reading",
                &[
                    ("column_name", &column.name),
                    ("row_count", &self.spill_read_count(slot, limit).to_string()),
                ],
            ),
            cx,
            move |cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.open_result_profile_for_column(slot, column_index, cx)
                });
            },
        ) {
            return;
        }
        let rows: Vec<Vec<CellValue>> = match self.spill_rows_for_slot_capped(slot, limit) {
            Some(rows) => rows
                .into_iter()
                .map(|row| vec![row.get(column_index).cloned().unwrap_or(CellValue::Null)])
                .collect(),
            None => self
                .resident_source_rows_for_slot(slot, cx)
                .into_iter()
                .take(limit)
                .map(|row| vec![row.get(column_index).cloned().unwrap_or(CellValue::Null)])
                .collect(),
        };
        let column_name = column.name.clone();
        let mut profiler = ResultProfiler::new(&[column]);
        profiler.push_rows(&rows);
        let profile = profiler.finish();
        let label = self.result_set_label(slot);
        self.result_profile_caption = crate::localization::format(
            if profile.rows_truncated {
                "results.profiling.single_column_truncated_caption"
            } else {
                "results.profiling.single_column_complete_caption"
            },
            &[
                ("result_label", &label),
                ("column_name", &column_name),
                ("profiled_rows", &profile.rows_profiled.to_string()),
            ],
        );
        self.result_profile = Some(profile);
        self.result_profile_sql = self.query_sql.clone();
        self.result_profile_filter = crate::result_profiler::ProfileFilter::default();
        self.rebuild_result_profile_grid(cx);
        self.clear_output_tab_flags();
        self.show_result_profile = true;
        cx.notify();
    }

    /// (Re)build the profile grid from the retained profile and the active
    /// quick filter, keeping the row → profiled-column mapping the detail
    /// pane reads selections through.
    fn rebuild_result_profile_grid(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = &self.result_profile else {
            self.result_profile_grid = None;
            self.result_profile_indices = Vec::new();
            self._result_profile_watch = None;
            return;
        };
        let filter = self.result_profile_filter;
        let indices: Vec<usize> = profile
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| filter.matches(column))
            .map(|(index, _)| index)
            .collect();
        let filtered = crate::result_profiler::ResultProfile {
            rows_profiled: profile.rows_profiled,
            rows_truncated: profile.rows_truncated,
            columns: indices
                .iter()
                .map(|&index| profile.columns[index].clone())
                .collect(),
        };
        let (profile_columns, profile_rows) = filtered.to_result_set();
        self.result_profile_indices = indices;
        self.result_profile_grid = self.build_diff_grid(&profile_columns, &profile_rows, cx);
        self.watch_result_profile_grid(cx);
    }

    /// Switch the profile quick filter and rebuild the grid behind it.
    fn set_result_profile_filter(
        &mut self,
        filter: crate::result_profiler::ProfileFilter,
        cx: &mut Context<Self>,
    ) {
        if self.result_profile_filter == filter {
            return;
        }
        self.result_profile_filter = filter;
        self.rebuild_result_profile_grid(cx);
        cx.notify();
    }

    /// Observe the profile grid's own state entity so selection changes in
    /// the grid re-render the detail pane below it (the host doesn't
    /// otherwise re-render on grid-internal updates).
    fn watch_result_profile_grid(&mut self, cx: &mut Context<Self>) {
        self._result_profile_watch = self.result_profile_grid.as_ref().map(|grid| {
            let state = grid.read(cx).state.clone();
            cx.observe(&state, |_, _, cx| cx.notify())
        });
    }

    /// The profile row the user selected in the Profile grid, mapped through
    /// the grid's display order (sorting/filtering) back to the profiled
    /// column it summarizes.
    fn selected_profile_column(
        &self,
        cx: &gpui::App,
    ) -> Option<crate::result_profiler::ColumnProfile> {
        let grid = self.result_profile_grid.as_ref()?;
        let state = grid.read(cx).state.read(cx);
        let display = crate::tool_dialog::selected_row(&state.selection)?;
        let source = state.resident_row_for_display(display)?;
        let column_index = self.result_profile_indices.get(source).copied()?;
        self.result_profile
            .as_ref()?
            .columns
            .get(column_index)
            .cloned()
    }

    /// The Profile tab — present only after "Profile All Columns" has been
    /// used on this run's results (the Chart tab's presence rule).
    fn render_result_profile_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.result_profile_grid.is_none() {
            return div().into_any_element();
        }
        let is_active = self.show_result_profile;
        self.tab_button(
            "result-profile-tab",
            crate::localization::text("results.profiling.tab_label"),
            is_active,
            crate::localization::text("results.profiling.tab_tooltip"),
            |this, _cx| {
                this.clear_output_tab_flags();
                this.show_result_profile = true;
            },
            cx,
        )
    }

    fn render_result_profile_view(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use crate::result_profiler::{format_number, single_line};
        let s = Theme::scaled;
        let Some(grid) = &self.result_profile_grid else {
            return div().into_any_element();
        };

        // Detail rows for the column selected in the grid above; the grid
        // cells only carry previews, this pane holds the full statistics.
        let detail: gpui::AnyElement = if let Some(column) = self.selected_profile_column(cx) {
            let percent = |count: usize| {
                if column.rows == 0 {
                    0.0
                } else {
                    count as f64 * 100.0 / column.rows as f64
                }
            };
            let metric =
                |key, arguments: &[(&str, &str)]| crate::localization::format(key, arguments);
            let rows = column.rows.to_string();
            let nulls = column.nulls.to_string();
            let null_percent = format!("{:.1}", percent(column.nulls));
            let distinct = column.distinct.to_string();
            let mut stats: Vec<String> = vec![
                metric(
                    "results.profiling.detail_row_count",
                    &[("row_count", &rows)],
                ),
                metric(
                    "results.profiling.detail_null_count",
                    &[("null_count", &nulls), ("null_percent", &null_percent)],
                ),
                if column.distinct_truncated {
                    metric(
                        "results.profiling.detail_distinct_truncated",
                        &[("distinct_count", &distinct)],
                    )
                } else {
                    metric(
                        "results.profiling.detail_distinct_count",
                        &[("distinct_count", &distinct)],
                    )
                },
            ];
            if column.numeric_min.is_some() || column.numeric_max.is_some() {
                stats.push(metric(
                    "results.profiling.detail_numeric_minimum",
                    &[("value", &format_number(column.numeric_min))],
                ));
                stats.push(metric(
                    "results.profiling.detail_numeric_maximum",
                    &[("value", &format_number(column.numeric_max))],
                ));
                stats.push(metric(
                    "results.profiling.detail_numeric_mean",
                    &[("value", &format_number(column.numeric_mean))],
                ));
            }
            match (column.min_length, column.max_length) {
                (Some(min), Some(max)) if min == max => stats.push(metric(
                    "results.profiling.detail_fixed_text_length",
                    &[("length", &min.to_string())],
                )),
                (Some(min), Some(max)) => stats.push(metric(
                    "results.profiling.detail_text_length_range",
                    &[("minimum", &min.to_string()), ("maximum", &max.to_string())],
                )),
                _ => {}
            }
            let type_label = if column.data_type.trim().is_empty() {
                String::new()
            } else {
                format!(" · {}", column.data_type)
            };
            let frequent: gpui::AnyElement = if column.frequent_values.is_empty() {
                div()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.profiling.no_repeated_values",
                    ))
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .children(column.frequent_values.iter().map(|(value, count)| {
                        let count = *count;
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(8.0)))
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .font_family(Theme::editor_font_family())
                                    .text_color(Theme::text())
                                    .child(single_line(value)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(Theme::text_dim())
                                    .child(format!("× {count} ({:.1}%)", percent(count))),
                            )
                    }))
                    .into_any_element()
            };
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(s(6.0)))
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Theme::text())
                                .child(column.name.clone()),
                        )
                        .child(
                            div()
                                .text_color(Theme::text_dim())
                                .child(format!("{}{type_label}", stats.join(" · "))),
                        ),
                )
                .child(frequent)
                .into_any_element()
        } else {
            div()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.profiling.select_column_detail_prompt",
                ))
                .into_any_element()
        };

        // The SQL body the profiled result came from, syntax-colored like the
        // editor, under the detail rows.
        let query_body = self
            .result_profile_sql
            .as_ref()
            .filter(|sql| !sql.trim().is_empty())
            .map(|sql| {
                let sql = sql.replace("\r\n", "\n").replace('\r', "\n");
                let spans = crate::sql_syntax::classify_for(self.source_db_type().family(), &sql);
                let mut line_start = 0usize;
                let mut lines: Vec<gpui::Div> = Vec::new();
                for line in sql.split('\n') {
                    lines.push(crate::ddl_viewer::render_sql_line(line, line_start, &spans));
                    line_start += line.len() + 1;
                }
                div()
                    .h(px(s(104.0)))
                    .flex_none()
                    .border_t_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(s(10.0)))
                            .pt(px(s(4.0)))
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "results.profiling.query_section_label",
                            )),
                    )
                    .child(
                        div()
                            .id("result-profile-query")
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_scroll()
                            .px(px(s(10.0)))
                            .py(px(s(4.0)))
                            .font_family(Theme::editor_font_family())
                            .flex()
                            .flex_col()
                            .children(lines),
                    )
            });

        // Semantic quick filters over the profile rows: each chip is a
        // data-shape question (nullable? constant? candidate key?) with its
        // match count; shapes no column has are shown but not clickable.
        let filter_chips = self.result_profile.as_ref().map(|profile| {
            let active = self.result_profile_filter;
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(s(4.0)))
                .px(px(s(10.0)))
                .pb(px(s(4.0)))
                .children(crate::result_profiler::ProfileFilter::all().map(|filter| {
                    let count = profile
                        .columns
                        .iter()
                        .filter(|column| filter.matches(column))
                        .count();
                    let is_active = filter == active;
                    let selectable = count > 0 || is_active;
                    div()
                        .px(px(s(8.0)))
                        .py(px(s(2.0)))
                        .rounded(px(s(4.0)))
                        .border_1()
                        .border_color(if is_active {
                            Theme::selection()
                        } else {
                            Theme::border()
                        })
                        .bg(if is_active {
                            Theme::selection()
                        } else {
                            gpui::transparent_black()
                        })
                        .text_color(if is_active {
                            Theme::text()
                        } else if selectable {
                            Theme::text_dim()
                        } else {
                            Theme::text_dim().opacity(0.5)
                        })
                        .child(format!("{} · {count}", filter.label()))
                        .when(selectable, |chip| {
                            chip.cursor_pointer().on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.set_result_profile_filter(filter, cx)
                                }),
                            )
                        })
                }))
        });

        div()
            .size_full()
            .flex()
            .flex_col()
            .font(Theme::ui_font())
            .text_size(px(Theme::font_size_ratio(0.84)))
            .child(
                div()
                    .px(px(s(10.0)))
                    .py(px(s(4.0)))
                    .text_color(Theme::text_dim())
                    .child(self.result_profile_caption.clone()),
            )
            .children(filter_chips)
            .child(div().flex_1().min_h(px(0.0)).child(grid.clone()))
            .child(
                div()
                    .h(px(s(140.0)))
                    .flex_none()
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .id("result-profile-detail")
                            .size_full()
                            .overflow_scroll()
                            .px(px(s(10.0)))
                            .py(px(s(6.0)))
                            .child(detail),
                    ),
            )
            .children(query_body)
            .into_any_element()
    }

    /// Compute the diff between the previous and current result sets for the
    /// active tab. Returns a summary string for display. Also creates the
    /// three diff grid entities (Previous / Current / Difference) so the
    /// diff view can render real interactive grids instead of a text listing.
    pub fn compute_diff(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let active = self.active_tab;
        let prev = self.previous_result_sets.get(active)?;
        let curr = self.result_sets.get(active)?;
        let curr_rows = self.resident_source_rows_for_slot(active, cx);
        self.diff = Some(result_set_difference::diff_optimized(
            &prev.columns,
            &prev.rows,
            &curr.columns,
            &curr_rows,
        ));
        self.diff_direction = if self
            .diff
            .as_ref()
            .is_some_and(|diff| diff.removed.is_empty())
        {
            DiffDirection::InCurrent
        } else {
            DiffDirection::InPrevious
        };

        self.diff_grid_previous = self.build_diff_grid(&prev.columns, &prev.rows, cx);
        self.diff_grid_current = self.build_diff_grid(&curr.columns, &curr_rows, cx);
        self.rebuild_diff_diff_grid(cx);

        let summary = self.diff.as_ref().map(|d| d.summary());
        cx.notify();
        summary
    }

    /// Build a read-only `SqllyDataTable` entity for a diff slot (no context
    /// menu provider — diff grids are display-only, but still support
    /// selection, sort, and column resize via the crate's built-in behavior).
    fn build_diff_grid(
        &self,
        columns: &[ResultColumn],
        rows: &[Vec<CellValue>],
        cx: &mut Context<Self>,
    ) -> Option<Entity<SqllyDataTable>> {
        let data = build_grid_data(self.classification_db(), columns, rows).unwrap_or_else(|_| {
            build_grid_data(self.classification_db(), columns, &[])
                .unwrap_or_else(|_| sqlly_datatable::GridData::new(vec![], vec![]).expect("empty"))
        });
        let config = self
            .formatting
            .as_ref()
            .map(|fmt| build_grid_config(self.classification_db(), fmt, columns))
            .unwrap_or_default();
        let theme = build_grid_theme();
        // Builder-supplied so the grid stays host-themed (see
        // `ensure_grid_for_slot`).
        let view = SqllyDataTable::builder(data)
            .config(config)
            .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
            .theme(theme)
            .build(cx);
        self.apply_density_to_table(&view, cx);
        Some(cx.new(|_cx| view))
    }

    /// Rebuild only the Difference-slot grid from the current diff direction.
    /// `InPrevious` shows rows removed from the previous set; `InCurrent`
    /// shows rows added in the current set.
    fn rebuild_diff_diff_grid(&mut self, cx: &mut Context<Self>) {
        let Some(diff) = &self.diff else {
            self.diff_grid_diff = None;
            return;
        };
        let active = self.active_tab;
        let columns = self
            .result_sets
            .get(active)
            .or_else(|| self.previous_result_sets.get(active))
            .map(|rs| rs.columns.clone())
            .unwrap_or_default();
        let rows = match self.diff_direction {
            DiffDirection::InPrevious => &diff.removed,
            DiffDirection::InCurrent => &diff.added,
        };
        self.diff_grid_diff = self.build_diff_grid(&columns, rows, cx);
    }

    /// Install a spill manifest: every result set it describes becomes
    /// spill-backed — the grid runs in windowed-row mode over the complete
    /// on-disk set (see [`Self::install_spill_backing`]) instead of
    /// materializing the rows in memory. Works both when the result sets
    /// already exist (the streaming executor path — their streamed rows are
    /// freed) and when they don't yet (a manifest-only source).
    pub fn set_spill_manifest(
        &mut self,
        manifest: crate::spill_pager::SpillManifest,
        cx: &mut Context<Self>,
    ) {
        // Cancel in-flight page reads against the OLD manifest's files, then
        // drop old backings (open file handles) BEFORE deleting those files —
        // Windows cannot unlink an open file.
        self.invalidate_spill_paging();
        self.spill_backings.clear();
        self.service_owned_spill_files.clear();
        if let Some(old) = self.spill_manifest.0.take() {
            old.cleanup();
        }
        if self.messages.is_empty() {
            self.messages = manifest.messages.clone();
        }
        if self.error.is_none() {
            self.error = manifest.error.clone();
        }
        if self.elapsed_ms.is_none() {
            self.elapsed_ms = manifest.elapsed_ms.map(|ms| ms as u64);
        }

        // Manifest-only sources: create the result sets the manifest
        // describes. The streaming path already has them — for those the
        // manifest `row_count` overwrites any progress-derived hint (the
        // manifest total is authoritative; progress may have lagged).
        for rs in &manifest.result_sets {
            match self
                .result_sets
                .iter_mut()
                .find(|r| r.index == rs.result_set_index)
            {
                Some(existing) => existing.row_count_hint = rs.row_count,
                None => {
                    self.result_sets.push(ResultSet {
                        index: rs.result_set_index,
                        columns: rs.columns.clone(),
                        rows: Vec::new(),
                        rows_affected: None,
                        row_count_hint: rs.row_count,
                        ..Default::default()
                    });
                    self.grid_views.push(None);
                    self.collapsed_cards.push(false);
                    self.stacked_card_heights.push(None);
                }
            }
        }
        while self.spill_backings.len() < self.result_sets.len() {
            self.spill_backings.push(None);
        }
        while self.synced_rows.len() < self.result_sets.len() {
            self.synced_rows.push(0);
        }

        // Install backings BEFORE handing files to the deletion guard: only
        // sets whose backing installed (or whose streamed rows are still
        // resident as a fallback) become guard-owned. If the final open
        // fails for a slot whose rows were already freed (provisional
        // backing installed mid-query), the guard must NOT take the file —
        // deleting it would destroy the only copy of the rows. The file is
        // left on disk instead and the failure is surfaced.
        let mut owned = manifest.clone();
        owned.result_sets.clear();
        for rs in &manifest.result_sets {
            if self.install_spill_backing(rs.result_set_index, &rs.spill_file, rs.row_count, cx) {
                owned.result_sets.push(rs.clone());
                continue;
            }
            let rows_resident = self
                .result_sets
                .iter()
                .enumerate()
                .find(|(_slot, r)| r.index == rs.result_set_index)
                .is_some_and(|(slot, r)| {
                    !r.rows.is_empty() || self.synced_rows.get(slot).copied().unwrap_or(0) > 0
                });
            if rows_resident || rs.row_count == 0 {
                // Baseline fallback: the in-memory rows still display, so
                // the file is deletable as before.
                owned.result_sets.push(rs.clone());
            } else {
                self.messages.push(crate::localization::format(
                    "results.spill.open_failed_rows_on_disk",
                    &[
                        ("result_number", &(rs.result_set_index + 1).to_string()),
                        ("file_path", &rs.spill_file),
                    ],
                ));
                if self.error.is_none() {
                    self.error = Some(crate::localization::format(
                        "results.spill.open_failed_rows_unavailable",
                        &[("result_number", &(rs.result_set_index + 1).to_string())],
                    ));
                }
            }
        }
        self.spill_manifest.0 = Some(owned);
        if self.track_difference && !self.is_running {
            if let Some(run) = self.retained_diff_runs.last_mut() {
                for (slot, backing) in self.spill_backings.iter().enumerate() {
                    if backing.is_some()
                        && let Some(set) = run.sets.get_mut(slot)
                    {
                        set.rows = None;
                        set.unavailable_reason = Some(crate::localization::text(
                            "results.diff.spill_backed_run_unavailable",
                        ));
                    }
                }
            }
            self.recompute_detailed_diff(cx);
        }
        cx.notify();
    }

    /// Get the computed diff, if any.
    #[allow(dead_code)]
    pub fn current_diff(&self) -> Option<&result_set_difference::ResultSetDiff> {
        self.diff.as_ref()
    }

    /// The display label for a result set: the parsed name if available,
    /// otherwise "Results {n}".
    fn result_set_label(&self, slot_index: usize) -> String {
        if let Some(rs) = self.result_sets.get(slot_index)
            && let Some(label) = self.index_labels.get(&rs.index)
        {
            return label.clone();
        }
        self.result_set_names
            .get(&slot_index)
            .cloned()
            .unwrap_or_else(|| {
                crate::localization::format(
                    "results.result_set.default_label",
                    &[("result_number", &(slot_index + 1).to_string())],
                )
            })
    }

    /// Public accessor for a result set by slot index. Used by the
    /// context-menu provider to read columns and rows for export and
    /// WHERE actions.
    pub fn result_set_at(&self, index: usize) -> Option<&ResultSet> {
        self.result_sets.get(index)
    }

    /// Formatting prefs for this view, falling back to the persisted defaults.
    /// The grid menu lives in another module and cannot read the private field.
    pub(crate) fn formatting_or_load(&self) -> ResultFormattingPreferences {
        self.formatting
            .clone()
            .unwrap_or_else(ResultFormattingPreferences::load)
    }

    fn masking_rules_or_load(&self) -> MaskingRules {
        self.masking_rules.clone().unwrap_or_else(|| {
            self.masking_rules_fallback
                .get_or_init(MaskingRules::load)
                .clone()
        })
    }

    /// Drop memoized footer stats; call whenever rows, formatting, or
    /// masking for any slot change under an unchanged grid selection.
    fn invalidate_selection_stats(&self) {
        self.selection_stats_cache.borrow_mut().clear();
    }

    /// Footer stats for `slot`, from the memo when the grid selection and
    /// resident row count are unchanged since they were last computed.
    fn memoized_selection_stats(
        &self,
        slot: usize,
        selection: &sqlly_datatable::Selection,
        row_count: usize,
        compute: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        if let Some(entry) = self.selection_stats_cache.borrow().get(&slot)
            && entry.row_count == row_count
            && entry.selection == *selection
        {
            return entry.text.clone();
        }
        let text = compute();
        self.selection_stats_cache.borrow_mut().insert(
            slot,
            SelectionStatsEntry {
                selection: selection.clone(),
                row_count,
                text: text.clone(),
            },
        );
        text
    }

    pub fn set_masking_overrides(&mut self, overrides: MaskingOverrides) {
        self.masking_overrides = overrides;
    }

    fn query_table_name(&self) -> Option<String> {
        self.query_sql()
            .and_then(crate::column_layout::single_table_target)
            .map(|(_, table)| table)
    }

    fn revealed_for_slot(&self, slot: usize) -> HashSet<usize> {
        self.revealed.get(slot).cloned().unwrap_or_default()
    }

    /// The formatting preferences with qualified `table.column` Column
    /// overrides resolved for `columns`: a qualified key applies when the
    /// column's source table is known (from the executed statement) and wins
    /// over a bare-name key.
    fn formatting_for_columns(&self, columns: &[ResultColumn]) -> ResultFormattingPreferences {
        let mut base = self.formatting_or_load();
        // The connection's own display time zone replaces the global one.
        crate::result_cell_format::apply_connection_display_zone(
            &mut base,
            self.fk_label_profile
                .as_ref()
                .and_then(|p| p.display_time_zone.as_ref()),
        );
        if base.column_overrides.keys().any(|key| key.contains('.')) {
            let sources = crate::grid_bridge::column_sources(columns, self.query_sql.as_deref());
            crate::result_cell_format::resolve_qualified_overrides(&mut base, columns, &sources);
        }
        base
    }

    pub(crate) fn effective_formatting_for_slot(&self, slot: usize) -> ResultFormattingPreferences {
        let columns = self
            .result_sets
            .get(slot)
            .map(|rs| rs.columns.as_slice())
            .unwrap_or(&[]);
        let table = self.query_table_name();
        let revealed = self.revealed_for_slot(slot);
        let mut fmt = effective_formatting(
            &self.formatting_for_columns(columns),
            &self.masking_rules_or_load(),
            &self.masking_overrides,
            columns,
            table.as_deref(),
            &revealed,
        );
        if let Some(accepted) = self.suggested_mask_accepted.get(slot) {
            inject_suggested_masks(&mut fmt, columns, accepted, &revealed);
        }
        // `mask=` directives mask copies and exports like override masks.
        crate::result_cell_format::inject_directive_masks(&mut fmt, columns, &self.format_hints);
        fmt
    }

    fn pad_reveal_slots(&mut self) {
        while self.revealed.len() < self.result_sets.len() {
            self.revealed.push(HashSet::new());
            self.revealed_cells.push(HashSet::new());
        }
    }

    fn pad_profile_slots(&mut self) {
        while self.column_profiles.len() < self.result_sets.len() {
            self.column_profiles.push(None);
            self.column_profile_requested.push(false);
            self.suggested_mask_accepted.push(HashSet::new());
            self.mask_suggestion_dismissed.push(false);
        }
    }

    /// Columns the masking rules and connection overrides mask, ignoring
    /// temporary reveals and accepted suggestions. These are never sampled —
    /// only their names may be sent to the decision model.
    fn rule_masked_columns(&self, slot: usize) -> HashSet<usize> {
        let Some(rs) = self.result_sets.get(slot) else {
            return HashSet::new();
        };
        let table = self.query_table_name();
        let mut fmt = effective_formatting(
            &self.formatting_for_columns(&rs.columns),
            &self.masking_rules_or_load(),
            &self.masking_overrides,
            &rs.columns,
            table.as_deref(),
            &HashSet::new(),
        );
        crate::result_cell_format::inject_directive_masks(
            &mut fmt,
            &rs.columns,
            &self.format_hints,
        );
        rs.columns
            .iter()
            .enumerate()
            .filter(|(_, column)| {
                fmt.column_overrides
                    .get(&column.name)
                    .is_some_and(|f| f.kind == ResultKind::Mask)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Profile the active slot's columns through the optional decision
    /// model, once per slot, off the UI thread. A missing provider, a failed
    /// request, or a timeout leaves the slot unprofiled — the host action is
    /// never blocked and behaviour is unchanged from today.
    fn maybe_spawn_column_profile(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        if self.is_running {
            return;
        }
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        if rs.columns.is_empty() {
            return;
        }
        if self
            .column_profile_requested
            .get(slot)
            .copied()
            .unwrap_or(false)
        {
            return;
        }
        self.pad_profile_slots();
        if let Some(flag) = self.column_profile_requested.get_mut(slot) {
            *flag = true;
        }
        let Some(provider) = crate::decision_ai::decision_provider() else {
            return;
        };
        let config = crate::decision_ai::load_decision_ai_config();
        let rs = &self.result_sets[slot];
        let columns = rs.columns.clone();
        // Row values are sent only behind the separate `allow_row_data`
        // opt-in, and rule-masked columns are never sampled even then.
        let samples = if config.allow_row_data {
            let rules_masked = self.rule_masked_columns(slot);
            Some(crate::column_profile::collect_samples(
                &columns,
                &self.result_sets[slot].rows,
                &rules_masked,
            ))
        } else {
            None
        };
        let generation = self.run_generation;
        cx.spawn(async move |this, cx| {
            let Some(profile) = crate::column_profile::profile_columns(
                provider.as_ref(),
                &columns,
                samples.as_deref(),
            )
            .await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.run_generation != generation {
                    return;
                }
                this.pad_profile_slots();
                if let Some(entry) = this.column_profiles.get_mut(slot) {
                    *entry = Some(profile);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The columns the toolbar suggests masking for `slot`: personal-looking
    /// per the profile and not already carrying a mask override. Empty while
    /// no profile exists (no provider configured) or after a dismissal.
    pub(crate) fn mask_suggestion_columns(&self, slot: usize) -> Vec<usize> {
        if self
            .mask_suggestion_dismissed
            .get(slot)
            .copied()
            .unwrap_or(false)
        {
            return Vec::new();
        }
        let Some(Some(profile)) = self.column_profiles.get(slot) else {
            return Vec::new();
        };
        let mut masked = self.rule_masked_columns(slot);
        if let Some(accepted) = self.suggested_mask_accepted.get(slot) {
            masked.extend(accepted.iter().copied());
        }
        crate::column_profile::suggestion_targets(profile, &masked)
    }

    /// Accept the suggestion: mask the suggested columns on this result via
    /// per-slot `ResultKind::Mask` overrides.
    pub fn accept_mask_suggestions(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        let targets = self.mask_suggestion_columns(slot);
        if targets.is_empty() {
            return;
        }
        self.pad_profile_slots();
        if let Some(set) = self.suggested_mask_accepted.get_mut(slot) {
            set.extend(targets);
        }
        self.rebuild_slot_from_source(slot, cx);
    }

    /// Append a global `MaskRule` for every suggested column via the
    /// existing rules save path, so they mask on every connection from now
    /// on.
    pub fn always_mask_suggestions(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        let targets = self.mask_suggestion_columns(slot);
        if targets.is_empty() {
            return;
        }
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let names: Vec<String> = targets
            .iter()
            .filter_map(|&i| rs.columns.get(i))
            .map(|column| {
                column
                    .name
                    .rsplit('.')
                    .next()
                    .unwrap_or(&column.name)
                    .to_string()
            })
            .collect();
        let mut rules = self.masking_rules_or_load();
        for name in names {
            let exists = rules.patterns.iter().any(|rule| {
                rule.scope == sqlly_core::RuleScope::ColumnName
                    && rule.pattern.eq_ignore_ascii_case(&name)
            });
            if !exists {
                rules.patterns.push(sqlly_core::MaskRule {
                    pattern: name,
                    scope: sqlly_core::RuleScope::ColumnName,
                });
            }
        }
        crate::preferences::PreferencesStore::new().set_masking_rules(rules.clone());
        self.masking_rules = Some(rules);
        self.rebuild_all_grids_from_source(cx);
    }

    /// Hide the suggestion chip for this result slot.
    pub fn dismiss_mask_suggestion(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        self.pad_profile_slots();
        if let Some(flag) = self.mask_suggestion_dismissed.get_mut(slot) {
            *flag = true;
        }
        cx.notify();
    }

    fn apply_cell_reveals(
        &self,
        slot: usize,
        first_source_row: usize,
        grid_rows: &mut [Vec<sqlly_datatable::CellValue>],
        cx: &gpui::App,
    ) {
        let Some(cells) = self.revealed_cells.get(slot) else {
            return;
        };
        if cells.is_empty() {
            return;
        }
        for (offset, row) in grid_rows.iter_mut().enumerate() {
            let grid_idx = first_source_row + offset;
            let source_row = self.source_index(slot, grid_idx);
            for (col, cell) in row.iter_mut().enumerate() {
                if !cells.contains(&(source_row, col)) {
                    continue;
                }
                if let Some(raw) = self
                    .source_row_for_slot(slot, grid_idx, cx)
                    .and_then(|r| r.get(col).cloned())
                {
                    *cell = convert_cell(&raw);
                }
            }
        }
    }

    fn apply_cell_reveals_to_data(
        &self,
        slot: usize,
        first_source_row: usize,
        data: &mut sqlly_datatable::GridData,
        cx: &gpui::App,
    ) {
        self.apply_cell_reveals(slot, first_source_row, &mut data.rows, cx);
    }

    pub(crate) fn masked_columns(&self, slot: usize) -> Vec<usize> {
        let Some(rs) = self.result_sets.get(slot) else {
            return Vec::new();
        };
        let fmt = self.effective_formatting_for_slot(slot);
        rs.columns
            .iter()
            .enumerate()
            .filter(|(i, column)| {
                !self.revealed_for_slot(slot).contains(i)
                    && fmt
                        .column_overrides
                        .get(&column.name)
                        .is_some_and(|f| f.kind == ResultKind::Mask)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Rebuild a slot's grid from the retained source rows.
    ///
    /// CAUTION: this reads the slot's grid entity, so it must never run
    /// while that entity is being updated — the grid context-menu callbacks
    /// fire from inside the grid's own update and must `defer` before
    /// calling anything that lands here (see the masking arms in
    /// `result_grid_menu::on_action`).
    fn rebuild_slot_from_source(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.invalidate_selection_stats();
        if self.result_sets.get(slot).is_none() {
            return;
        }
        let Some(Some(view)) = self.grid_views.get(slot).cloned() else {
            self.ensure_grid_for_slot(slot, cx);
            return;
        };
        let columns = self.result_sets[slot].columns.clone();
        let rows = self.owned_rows_for_grid(slot, cx);
        let format_hints = self.format_hints.clone();
        let effective = self.effective_formatting_for_slot(slot);
        let mut data = build_grid_data_owned_with_formats(
            self.classification_db(),
            &columns,
            rows,
            &format_hints,
            Some(&effective),
        )
        .unwrap_or_else(|_| {
            build_grid_data(self.classification_db(), &columns, &[])
                .unwrap_or_else(|_| sqlly_datatable::GridData::new(vec![], vec![]).expect("empty"))
        });
        self.apply_truncation_markers(slot, 0, &mut data.rows);
        self.apply_cell_reveals_to_data(slot, 0, &mut data, cx);
        self.rebuild_grid_entity(slot, view, data, cx);
        if let Some(synced) = self.synced_rows.get_mut(slot) {
            *synced = self
                .result_sets
                .get(slot)
                .map(|rs| rs.rows.len())
                .unwrap_or(0);
        }
        self.auto_size_columns(slot, cx);
        cx.notify();
    }

    /// Size each column of `slot` to fit its name and the values shown so
    /// far, up to the Results ▸ General character cap (0 = no cap), scanning
    /// only rows not yet measured. A column whose width no longer matches
    /// what this last set — the user dragged it, or a saved layout or
    /// directive set it — is left alone from then on.
    ///
    /// The widths set here are measurements, not layout choices: the
    /// remembered column layout records such a column at its name-based
    /// default (see [`Self::capture_layout_for_slot`]), so the next run
    /// restores the default and sizes it to the new values again, while a
    /// dragged width is remembered as dragged.
    fn auto_size_columns(&mut self, slot: usize, cx: &mut Context<Self>) {
        /// Rows measured per result: enough to size real data, bounded so a
        /// million-row result never stalls the UI thread.
        const MAX_MEASURED_ROWS: usize = 20_000;
        let Some(Some(view)) = self.grid_views.get(slot).cloned() else {
            return;
        };
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        let cap = self.formatting_or_load().auto_size_max_chars;
        let state = view.read(cx).state.clone();
        let state_id = state.entity_id();
        let track = self.auto_size.entry(slot).or_default();
        if track.state_id != Some(state_id) || track.widest.len() != columns.len() {
            let widths: Vec<Option<f32>> = {
                let s = state.read(cx);
                let marker = grid_marker_width(&s.data.columns, columns.len());
                (0..columns.len())
                    .map(|col| s.data.columns.get(col + marker).map(|c| c.width))
                    .collect()
            };
            let tab_cells =
                usize::try_from(crate::preferences::EditorPreferences::load().spaces_per_tab())
                    .unwrap_or(crate::grid_bridge::DEFAULT_TAB_CELLS);
            *track = AutoSizeTrack::seed(state_id, &columns, &widths, track, tab_cells);
        }
        state.update(cx, |s, cx| {
            let marker = grid_marker_width(&s.data.columns, columns.len());
            let rows = s.data.rows.len();
            if rows < track.scanned {
                // Rebuilt shorter (filtered): the widths measured so far still
                // describe real values; just don't index past the end.
                track.scanned = rows;
            }
            let end = rows.min(MAX_MEASURED_ROWS);
            for row in &s.data.rows[track.scanned.min(end)..end] {
                for (col, widest) in track.widest.iter_mut().enumerate() {
                    let Some(cell) = row.get(col + marker) else {
                        continue;
                    };
                    let fmt = &s.resolved_formats[col + marker];
                    let text = sqlly_datatable::format::format_cell(cell, fmt).0;
                    *widest = (*widest).max(crate::grid_bridge::widest_value_cells(
                        [text.as_str()],
                        track.tab_cells,
                    ));
                }
            }
            track.scanned = track.scanned.max(end);
            let mut changed = false;
            for (col, def) in columns.iter().enumerate() {
                let Some(grid_col) = s.data.columns.get_mut(col + marker) else {
                    continue;
                };
                let Some(applied) = track.applied[col] else {
                    continue;
                };
                if (grid_col.width - applied).abs() > 0.5 {
                    // Sized by someone else; stop managing it.
                    track.applied[col] = None;
                    continue;
                }
                let width =
                    crate::grid_bridge::auto_column_width(&def.name, track.widest[col], cap);
                if (grid_col.width - width).abs() > 0.5 {
                    grid_col.width = width;
                    changed = true;
                }
                track.applied[col] = Some(width);
            }
            if changed {
                s.recompute();
                cx.notify();
            }
        });
    }

    pub fn reveal_column(&mut self, slot: usize, col: usize, cx: &mut Context<Self>) {
        self.pad_reveal_slots();
        if let Some(set) = self.revealed.get_mut(slot) {
            set.insert(col);
        }
        self.rebuild_slot_from_source(slot, cx);
    }

    pub fn reveal_cell(
        &mut self,
        slot: usize,
        source_row: usize,
        col: usize,
        cx: &mut Context<Self>,
    ) {
        self.pad_reveal_slots();
        if let Some(set) = self.revealed_cells.get_mut(slot) {
            set.insert((source_row, col));
        }
        self.rebuild_slot_from_source(slot, cx);
    }

    pub fn remask_all(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.pad_reveal_slots();
        if let Some(set) = self.revealed.get_mut(slot) {
            set.clear();
        }
        if let Some(set) = self.revealed_cells.get_mut(slot) {
            set.clear();
        }
        self.rebuild_slot_from_source(slot, cx);
    }

    pub fn reveal_all_active(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        let cols = self.masked_columns(slot);
        self.pad_reveal_slots();
        if let Some(set) = self.revealed.get_mut(slot) {
            set.extend(cols);
        }
        self.rebuild_slot_from_source(slot, cx);
    }

    pub fn request_open_masking_settings(&mut self, cx: &mut Context<Self>) {
        cx.emit(ResultsViewEvent::OpenMaskingSettings);
    }

    pub fn request_masking_override(
        &mut self,
        column_name: String,
        include: bool,
        cx: &mut Context<Self>,
    ) {
        cx.emit(ResultsViewEvent::MaskingOverrideRequested {
            column_name,
            include,
        });
    }

    pub fn rebuild_all_grids_from_source(&mut self, cx: &mut Context<Self>) {
        let n = self.result_sets.len();
        for slot in 0..n {
            self.rebuild_slot_from_source(slot, cx);
        }
    }

    /// The SQL text of the most recent query, when one has been run.
    pub fn query_sql(&self) -> Option<&str> {
        self.query_sql.as_deref()
    }

    /// Record the connection-level row-edit verdict for the run about to
    /// fill this view (see [`crate::row_edit_gate`]). Set before the run
    /// starts so the grids built for its results show or hide the pencil.
    pub fn set_row_edit_connection_block(&mut self, block: Option<String>) {
        self.row_edit_connection_block = Some(block);
    }

    /// Whether `slot`'s row-header Edit (pencil) action should show: the
    /// same synchronous refusals the editor's open path applies
    /// ([`crate::row_edit_gate::edit_block_reason`]).
    pub(crate) fn row_edit_enabled_for_slot(&self, slot: usize) -> bool {
        self.row_edit_block_reason_for_slot(slot).is_none()
    }

    /// Why `slot`'s rows cannot be edited, from the same synchronous checks
    /// as the pencil; `None` leaves the final word to the edit session.
    pub(crate) fn row_edit_block_reason_for_slot(&self, slot: usize) -> Option<String> {
        crate::row_edit_gate::edit_block_reason(crate::row_edit_gate::ResultEditContext {
            historical: self.historical_banner.is_some(),
            sql: self.query_sql(),
            result_set_index: self.result_sets.get(slot).map_or(slot, |rs| rs.index),
            connection_block: self
                .row_edit_connection_block
                .as_ref()
                .map(|block| block.as_deref()),
        })
    }

    /// Snapshot the complete result sets for query-history storage. Sets
    /// whose rows were suppressed (spill-backed, so `rows` is a prefix) are
    /// skipped — a historical restore must never present a partial set as
    /// the whole.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn snapshot_sets(&self, cx: &gpui::App) -> Vec<crate::query_version_history::SnapshotSet> {
        self.result_sets
            .iter()
            .enumerate()
            // A spill-backed slot counts as "complete" (its rows live in the
            // spill file) but has no resident rows: snapshotting it would
            // store an EMPTY set that restores as "0 rows".
            .filter(|(slot, _rs)| {
                !matches!(self.spill_backings.get(*slot), Some(Some(_)))
                    && self.rows_complete_for_slot(*slot)
            })
            .map(|(slot, rs)| crate::query_version_history::SnapshotSet {
                columns: rs.columns.clone(),
                rows: self.resident_source_rows_for_slot(slot, cx),
                rows_affected: rs.rows_affected,
            })
            .collect()
    }

    /// Snapshot EVERY result set for moving a tab to another window, in slot
    /// order. Spill-backed sets contribute their first `spill_row_cap` rows
    /// read from the spill file (bounded — a million-row spill is never
    /// materialized); the flag is true when any set was cut short that way
    /// (or held only a partial resident prefix), so the target can say so.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn snapshot_sets_for_move(
        &self,
        spill_row_cap: usize,
        cx: &gpui::App,
    ) -> (Vec<crate::query_version_history::SnapshotSet>, bool) {
        let mut truncated = false;
        let sets = self
            .result_sets
            .iter()
            .enumerate()
            .map(|(slot, rs)| {
                let rows = match self.spill_rows_for_export(slot, spill_row_cap) {
                    Some((rows, cut)) => {
                        truncated |= cut;
                        rows
                    }
                    None => {
                        truncated |= !self.rows_complete_for_slot(slot);
                        self.resident_source_rows_for_slot(slot, cx)
                    }
                };
                crate::query_version_history::SnapshotSet {
                    columns: rs.columns.clone(),
                    rows,
                    rows_affected: rs.rows_affected,
                }
            })
            .collect();
        (sets, truncated)
    }

    #[allow(dead_code)]
    pub fn historical_banner(&self) -> Option<&str> {
        self.historical_banner.as_deref()
    }

    /// Install restored query-history result sets for a read-only render
    /// under an informational banner. Clears any live result state first.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_historical_results(
        &mut self,
        banner: String,
        sets: Vec<crate::query_version_history::SnapshotSet>,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        self.result_sets = sets
            .into_iter()
            .enumerate()
            .map(|(i, set)| ResultSet {
                index: i,
                row_count_hint: set.rows.len(),
                columns: set.columns,
                rows: set.rows,
                rows_affected: set.rows_affected,
                ..Default::default()
            })
            .collect();
        self.active_tab = 0;
        self.historical_banner = Some(banner);
        cx.notify();
    }

    /// Dismiss the historical-results banner, dropping the restored sets.
    pub fn clear_historical(&mut self, cx: &mut Context<Self>) {
        if self.historical_banner.take().is_some() {
            self.clear(cx);
        }
    }

    /// Informational banner rendered above the grid while historical
    /// (restored) results are shown. Info signal color for the marker only;
    /// text stays primary per the calibrated-signal rule.
    fn render_historical_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let banner = self.historical_banner.clone()?;
        let s = Theme::scaled;
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .px(px(s(12.0)))
                .py(px(s(6.0)))
                .bg(Theme::bg())
                .border_b_1()
                .border_color(Theme::border())
                .font(Theme::ui_font())
                .child(
                    div()
                        .w(px(s(6.0)))
                        .h(px(s(6.0)))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(Theme::info()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(banner),
                )
                .child(
                    div()
                        .id("historical-banner-dismiss")
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .cursor_pointer()
                        .flex_shrink_0()
                        .hover(|s| s.text_color(Theme::text()))
                        .child(crate::localization::text(
                            "results.run_history.dismiss_banner_button",
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.clear_historical(cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Emits [`ResultsViewEvent::EditRowRequested`] for a row-header context
    /// menu action. `source_row_index` is the index into the underlying
    /// (unsorted/unfiltered) rows; spill-backed slots page the single row
    /// from the file. Emitting (rather than opening a dialog here) keeps the
    /// modal with `AppView`, which owns `Services` and the active profile.
    /// Emits an [`ResultsViewEvent::EditRowRequested`] for a specific intent
    /// (edit, insert, or duplicate).
    pub fn open_row_editor_with(
        &mut self,
        slot: usize,
        source_row_index: usize,
        intent: crate::row_editor::RowEditorIntent,
        cx: &mut Context<Self>,
    ) {
        if self.historical_banner.is_some() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let row = match self.spill_backings.get(slot) {
            Some(Some(backing)) => backing
                .pager
                .borrow_mut()
                .page(source_row_index, 1)
                .into_iter()
                .next(),
            _ => self.source_row_for_slot(slot, source_row_index, cx),
        };
        let Some(row) = row else {
            return;
        };
        cx.emit(ResultsViewEvent::EditRowRequested {
            result_set_index: rs.index,
            columns: rs.columns.clone(),
            row,
            sql,
            intent,
        });
    }

    /// Emits [`ResultsViewEvent::CellActionRequested`] for a cell-menu action.
    /// `source_row_index` is the grid source index (mapped through plan 214's
    /// `source_index` when paging from memory), or the absolute spill row for
    /// a windowed grid — callers translate resident rows with
    /// [`backing_row_for_grid_row`].
    pub fn request_cell_action(
        &mut self,
        slot: usize,
        source_row_index: usize,
        column_index: usize,
        action: crate::cell_actions::CellAction,
        cx: &mut Context<Self>,
    ) {
        if self.historical_banner.is_some() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let marker = self.marker_offset(slot);
        if marker == 1 && column_index == 0 {
            return;
        }
        let result_col = column_index.saturating_sub(marker);
        let Some(column_name) = rs.columns.get(result_col).map(|column| column.name.clone()) else {
            return;
        };
        let row = match self.spill_backings.get(slot) {
            Some(Some(backing)) => backing
                .pager
                .borrow_mut()
                .page(source_row_index, 1)
                .into_iter()
                .next(),
            _ => self.source_row_for_slot(slot, source_row_index, cx),
        };
        let Some(row) = row else {
            return;
        };
        self.staging_slot = Some(slot);
        cx.emit(ResultsViewEvent::CellActionRequested {
            result_set_index: rs.index,
            columns: rs.columns.clone(),
            row,
            sql,
            column_name,
            result_column: result_col,
            action,
        });
    }

    pub fn open_row_editor_stage_with(
        &mut self,
        slot: usize,
        source_row_index: usize,
        intent: crate::row_editor::RowEditorIntent,
        cx: &mut Context<Self>,
    ) {
        if self.historical_banner.is_some() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        if !self.pending.is_empty() && self.pending_slot != Some(slot) {
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            let slot_one = self.pending_slot.map(|s| s + 1).unwrap_or(0);
            self.messages
                .push(pending_changes_elsewhere_message(n, slot_one));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let row = match self.spill_backings.get(slot) {
            Some(Some(backing)) => backing
                .pager
                .borrow_mut()
                .page(source_row_index, 1)
                .into_iter()
                .next(),
            _ => self.source_row_for_slot(slot, source_row_index, cx),
        };
        let Some(row) = row else {
            return;
        };
        self.staging_slot = Some(slot);
        cx.emit(ResultsViewEvent::StageRequested {
            result_set_index: rs.index,
            columns: rs.columns.clone(),
            row,
            sql,
            intent,
        });
    }

    pub fn request_paste(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.historical_banner.is_some() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let Some(grid) = self.grid_views.get(slot).and_then(|g| g.clone()) else {
            return;
        };
        let text = cx
            .read_from_clipboard()
            .as_ref()
            .and_then(|c| c.text())
            .unwrap_or_default()
            .to_string();
        if text.is_empty() {
            self.messages
                .push(crate::localization::text("results.paste.clipboard_empty"));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let state_entity = grid.read(cx).state.clone();
        let (selection, bounds, source_idx) = {
            let state = state_entity.read(cx);
            let selection = state.selection.clone();
            let bounds = selection.normalized_bounds();
            // Resident-relative in windowed (spill) mode; shift to the
            // backing row so the anchor is the row the user selected.
            let source_idx = bounds
                .and_then(|(r1, _, _, _)| state.resident_row_for_display(r1))
                .map(|resident| backing_row_for_grid_row(state.window.map(|w| w.offset), resident));
            (selection, bounds, source_idx)
        };
        let overwrite = matches!(selection, Selection::CellRange(..) | Selection::Cells(..));
        let (r1, c1) = bounds.map(|(r, c, _, _)| (r, c)).unwrap_or((0, 0));
        let marker = self.marker_offset(slot);
        let fill_range = paste_fill_range(&selection, marker);
        let result_col = c1.saturating_sub(marker);
        let columns = rs.columns.clone();
        let result_set_index = rs.index;
        let anchor_row = if bounds.is_none() {
            None
        } else {
            source_idx.and_then(|idx| self.source_row_for_slot(slot, idx, cx))
        };
        self.staging_slot = Some(slot);
        cx.emit(ResultsViewEvent::PasteRequested {
            result_set_index,
            columns,
            anchor_row,
            sql,
            text,
            anchor: (r1, result_col),
            overwrite,
            fill_range,
        });
    }

    pub fn apply_staged_op(
        &mut self,
        slot: usize,
        session: sqlly_services::row_editing::RowEditSession,
        op: crate::row_editor::StagedRowOp,
        cx: &mut Context<Self>,
    ) {
        if !self.pending.is_empty() && self.pending_slot != Some(slot) {
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            let slot_one = self.pending_slot.map(|s| s + 1).unwrap_or(0);
            self.messages
                .push(pending_changes_elsewhere_message(n, slot_one));
            self.show_messages = true;
            cx.notify();
            return;
        }
        self.pending_slot = Some(slot);
        match op {
            crate::row_editor::StagedRowOp::Update {
                key,
                original_row,
                changes,
            } => {
                self.pending
                    .stage_update(session, key, original_row, changes);
            }
            crate::row_editor::StagedRowOp::Insert { values } => {
                self.pending.stage_insert(session, values);
            }
            crate::row_editor::StagedRowOp::Delete { key, original_row } => {
                self.pending.stage_delete(session, key, original_row);
            }
        }
        self.schedule_preview_sql_refresh(cx);
        self.rebuild_slot_with_pending_markers(slot, cx);
        cx.notify();
    }

    pub fn apply_paste(
        &mut self,
        slot: usize,
        session: sqlly_services::row_editing::RowEditSession,
        mapped: crate::paste_rows::MappedRows,
        overwrite_targets: Vec<(
            crate::pending_changes::RowKey,
            Vec<CellValue>,
            BTreeMap<String, CellValue>,
        )>,
        cx: &mut Context<Self>,
    ) {
        if !self.pending.is_empty() && self.pending_slot != Some(slot) {
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            let slot_one = self.pending_slot.map(|s| s + 1).unwrap_or(0);
            self.messages
                .push(pending_changes_elsewhere_message(n, slot_one));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let staged = !overwrite_targets.is_empty() || !mapped.rows.is_empty();
        if staged {
            self.pending_slot = Some(slot);
        }
        if !overwrite_targets.is_empty() {
            for (key, original_row, changes) in overwrite_targets {
                let edits = changes
                    .into_iter()
                    .map(|(name, value)| (name, crate::pending_changes::CellEdit::Value(value)))
                    .collect();
                self.pending
                    .stage_update(session.clone(), key, original_row, edits);
            }
        } else {
            let rows = mapped
                .rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|(name, value)| (name, crate::pending_changes::CellEdit::Value(value)))
                        .collect()
                })
                .collect();
            self.pending.stage_inserts(session, rows);
        }
        for e in &mapped.errors {
            self.messages.push(crate::localization::format(
                "results.paste.row_value_error",
                &[
                    ("row_number", &(e.row + 1).to_string()),
                    ("column_name", &e.column),
                    ("error", &e.reason),
                    ("cell_text", &format!("{:?}", e.text)),
                ],
            ));
            self.show_messages = true;
        }
        if mapped.dropped_columns > 0 {
            self.messages.push(crate::localization::format(
                "results.paste.extra_columns_dropped",
                &[("column_count", &mapped.dropped_columns.to_string())],
            ));
            self.show_messages = true;
        }
        if staged {
            self.schedule_preview_sql_refresh(cx);
            self.rebuild_slot_with_pending_markers(slot, cx);
        }
        cx.notify();
    }

    /// Thin wrapper: stage generated batch rows as inserts (same markers/preview as paste).
    pub fn apply_batch_inserts(
        &mut self,
        slot: usize,
        session: sqlly_services::row_editing::RowEditSession,
        rows: Vec<Vec<(String, CellValue)>>,
        cx: &mut Context<Self>,
    ) {
        self.apply_paste(
            slot,
            session,
            crate::paste_rows::MappedRows {
                rows,
                errors: Vec::new(),
                dropped_columns: 0,
            },
            Vec::new(),
            cx,
        );
    }

    /// Whether the connection behind these results is reached through a
    /// relay bridge that has not advertised the server-evaluated generators
    /// (gates the grid menu's "server date/UUID/current user" entries). With
    /// no services recorded for a relay profile, stay conservative.
    fn results_via_relay(&self) -> bool {
        self.fk_label_profile.as_ref().is_some_and(|profile| {
            match self.fk_label_services.as_ref() {
                Some(services) => {
                    crate::cell_actions::server_expressions_blocked(profile, services)
                }
                None => matches!(
                    profile.route,
                    sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
                ),
            }
        })
    }

    /// Whether a staged save would fail because the connection routes through
    /// a relay bridge that cannot commit a batch (`row_edit.apply_batch`).
    /// With no services recorded for a relay profile, stay conservative.
    pub(crate) fn staged_save_blocked(&self) -> bool {
        self.fk_label_profile.as_ref().is_some_and(|profile| {
            match self.fk_label_services.as_ref() {
                Some(services) => crate::cell_actions::staged_save_blocked(profile, services),
                None => matches!(
                    profile.route,
                    sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
                ),
            }
        })
    }

    /// The grid window offset for `slot` when its grid is windowed over a
    /// spill file. Must not be called from inside that grid state's own
    /// update (the context menu), which holds the state lease.
    pub(crate) fn grid_window_offset(&self, slot: usize, cx: &App) -> Option<usize> {
        let grid = self.grid_views.get(slot)?.as_ref()?;
        grid.read(cx).state.read(cx).window.map(|w| w.offset)
    }

    /// The core row shown at grid display row `display_row`. In windowed
    /// (spill-backed) mode the grid resolves display rows to *resident*
    /// indices, so they are shifted by the window offset before paging the
    /// spill file; otherwise the grid source index maps through
    /// [`Self::source_row_for_slot`].
    pub(crate) fn display_row_source(
        &self,
        slot: usize,
        display_row: usize,
        cx: &App,
    ) -> Option<Vec<CellValue>> {
        let grid = self.grid_views.get(slot).and_then(|g| g.as_ref())?;
        let state = grid.read(cx).state.read(cx);
        let resident = state.resident_row_for_display(display_row)?;
        let row = backing_row_for_grid_row(state.window.map(|w| w.offset), resident);
        self.source_row_for_slot(slot, row, cx)
    }

    pub fn apply_staged_deletes(
        &mut self,
        slot: usize,
        session: sqlly_services::row_editing::RowEditSession,
        rows: Vec<Vec<CellValue>>,
        cx: &mut Context<Self>,
    ) {
        if !self.pending.is_empty() && self.pending_slot != Some(slot) {
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            let slot_one = self.pending_slot.map(|s| s + 1).unwrap_or(0);
            self.messages
                .push(pending_changes_elsewhere_message(n, slot_one));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        let mut keyed = Vec::new();
        for row in rows {
            if let Some(key) =
                crate::pending_changes::row_key_from_result_row(&session, &columns, &row)
            {
                keyed.push((key, row));
            }
        }
        if keyed.is_empty() {
            self.messages.push(crate::localization::text(
                "results.staged_changes.no_keyed_rows_for_delete",
            ));
            self.show_messages = true;
            cx.notify();
            return;
        }
        self.pending_slot = Some(slot);
        self.pending.stage_deletes(session, keyed);
        self.schedule_preview_sql_refresh(cx);
        self.rebuild_slot_with_pending_markers(slot, cx);
        cx.notify();
    }

    /// Width of the staged-edit marker column prefix on this slot's grid (0
    /// or 1) — find & replace maps grid columns through it.
    /// The staged edit for one cell, if any — the grid's Transform actions
    /// rewrite the staged value rather than the original.
    pub fn staged_cell_edit(
        &self,
        key: &crate::pending_changes::RowKey,
        column: &str,
    ) -> Option<crate::pending_changes::CellEdit> {
        self.pending
            .current()
            .dirty
            .get(key)
            .and_then(|edits| edits.get(column))
            .cloned()
    }

    /// The value the grid paints for a staged cell edit.
    fn staged_edit_value(edit: &crate::pending_changes::CellEdit) -> CellValue {
        match edit {
            crate::pending_changes::CellEdit::Value(value) => value.clone(),
            crate::pending_changes::CellEdit::Expression(expr) => {
                CellValue::String(crate::cell_actions::expression_label(*expr).to_string())
            }
        }
    }

    /// Overlay `slot`'s staged cell edits on committed source `row`, as the
    /// grid paints them. Returns the result columns that were overwritten.
    fn apply_staged_edits_to_row(&self, slot: usize, row: &mut [CellValue]) -> Vec<usize> {
        if self.pending_slot != Some(slot) || self.pending.is_empty() {
            return Vec::new();
        }
        let (Some(session), Some(rs)) = (
            self.pending.current().session.as_ref(),
            self.result_sets.get(slot),
        ) else {
            return Vec::new();
        };
        let Some(edits) =
            crate::pending_changes::row_key_from_result_row(session, &rs.columns, row)
                .and_then(|key| self.pending.current().dirty.get(&key))
        else {
            return Vec::new();
        };
        let mut staged = Vec::new();
        for (name, edit) in edits {
            // Provenance, not name — the same rule as the grid overlay.
            if let Some(index) =
                crate::pending_changes::result_index_for_table_column(session, name)
                && let Some(cell) = row.get_mut(index)
            {
                *cell = Self::staged_edit_value(edit);
                staged.push(index);
            }
        }
        staged
    }

    /// Staged inserted row `index` of `slot`, in result-column order, as the
    /// grid paints it (unset columns are NULL).
    fn staged_insert_row(&self, slot: usize, index: usize) -> Option<Vec<CellValue>> {
        if self.pending_slot != Some(slot) {
            return None;
        }
        let session = self.pending.current().session.as_ref()?;
        let width = self.result_sets.get(slot)?.columns.len();
        let values = self.pending.current().new_rows.get(index)?;
        let mut row = vec![CellValue::Null; width];
        for (name, edit) in values {
            if let Some(cell) = crate::pending_changes::result_index_for_table_column(session, name)
                .and_then(|i| row.get_mut(i))
            {
                *cell = Self::staged_edit_value(edit);
            }
        }
        Some(row)
    }

    pub fn marker_offset(&self, slot: usize) -> usize {
        if self.pending.is_empty() || self.pending_slot != Some(slot) {
            0
        } else {
            1
        }
    }

    /// The marker-prefix width of `state`, the live grid of `slot` (see
    /// [`grid_marker_width`]).
    pub(crate) fn grid_marker_width_of(&self, slot: usize, state: &GridState) -> usize {
        self.result_sets.get(slot).map_or(0, |rs| {
            grid_marker_width(&state.data.columns, rs.columns.len())
        })
    }

    pub fn unstage_row_at(&mut self, slot: usize, source_row_index: usize, cx: &mut Context<Self>) {
        if self.pending_slot != Some(slot) {
            return;
        }
        let Some(session) = self.pending.current().session.clone() else {
            return;
        };
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        let row = match self.spill_backings.get(slot) {
            Some(Some(backing)) => backing
                .pager
                .borrow_mut()
                .page(source_row_index, 1)
                .into_iter()
                .next(),
            _ => self.source_row_for_slot(slot, source_row_index, cx),
        };
        let Some(row) = row else {
            return;
        };
        let Some(key) = crate::pending_changes::row_key_from_result_row(&session, &columns, &row)
        else {
            return;
        };
        self.pending.unstage_key(&key);
        if self.pending.is_empty() {
            self.pending_slot = None;
        }
        self.schedule_preview_sql_refresh(cx);
        self.rebuild_slot_with_pending_markers(slot, cx);
        cx.notify();
    }

    fn marker_glyph_for_row(
        &self,
        session: &sqlly_services::row_editing::RowEditSession,
        columns: &[ResultColumn],
        row: &[sqlly_core::CellValue],
    ) -> &'static str {
        let Some(key) = crate::pending_changes::row_key_from_result_row(session, columns, row)
        else {
            return "";
        };
        if self.pending.current().deleted.contains(&key) {
            "\u{2212}"
        } else if self.pending.current().dirty.contains_key(&key) {
            "~"
        } else {
            ""
        }
    }

    fn overlay_pending_markers_on_grid_data(
        &self,
        slot: usize,
        columns: &[ResultColumn],
        data: sqlly_datatable::GridData,
    ) -> sqlly_datatable::GridData {
        use sqlly_datatable::CellValue as GridCell;
        if self.pending_slot != Some(slot) || self.pending.is_empty() {
            return data;
        }
        let Some(session) = self.pending.current().session.as_ref() else {
            return data;
        };
        let marker_cell = |glyph: &str| {
            if glyph.is_empty() {
                GridCell::None
            } else {
                GridCell::Text(glyph.to_string())
            }
        };
        let mut rows = Vec::with_capacity(data.rows.len() + self.pending.current().new_rows.len());
        for row in &data.rows {
            let core: Vec<sqlly_core::CellValue> = row
                .iter()
                .map(crate::grid_bridge::grid_cell_to_core)
                .collect();
            let glyph = self.marker_glyph_for_row(session, columns, &core);
            let mut prefixed = vec![marker_cell(glyph)];
            prefixed.extend(row.clone());
            if let Some(key) =
                crate::pending_changes::row_key_from_result_row(session, columns, &core)
                && let Some(edits) = self.pending.current().dirty.get(&key)
            {
                for (name, edit) in edits {
                    // Provenance, not name: an aliased expression that shares
                    // the column's name must not display the staged value.
                    if let Some(index) =
                        crate::pending_changes::result_index_for_table_column(session, name)
                    {
                        let grid_idx = index + 1;
                        if let Some(cell) = prefixed.get_mut(grid_idx) {
                            *cell = match edit {
                                crate::pending_changes::CellEdit::Value(value) => {
                                    crate::grid_bridge::convert_cell(value)
                                }
                                crate::pending_changes::CellEdit::Expression(expr) => {
                                    GridCell::Text(
                                        crate::cell_actions::expression_label(*expr).to_string(),
                                    )
                                }
                            };
                        }
                    }
                }
            }
            rows.push(prefixed);
        }
        for insert_values in &self.pending.current().new_rows {
            let mut core_row = vec![sqlly_core::CellValue::Null; columns.len()];
            for (name, edit) in insert_values {
                if let Some(index) =
                    crate::pending_changes::result_index_for_table_column(session, name)
                        .filter(|index| *index < core_row.len())
                {
                    core_row[index] = match edit {
                        crate::pending_changes::CellEdit::Value(value) => value.clone(),
                        crate::pending_changes::CellEdit::Expression(expr) => {
                            sqlly_core::CellValue::String(
                                crate::cell_actions::expression_label(*expr).to_string(),
                            )
                        }
                    };
                }
            }
            let grid_cells = crate::grid_bridge::convert_rows_owned_with_formats(
                self.classification_db(),
                vec![core_row],
                columns,
                &self.format_hints,
                Some(&self.effective_formatting_for_slot(slot)),
            )
            .into_iter()
            .next()
            .unwrap_or_default();
            let mut prefixed = vec![marker_cell("+")];
            prefixed.extend(grid_cells);
            rows.push(prefixed);
        }
        let mut grid_columns = vec![crate::grid_bridge::marker_column()];
        grid_columns.extend(data.columns.clone());
        sqlly_datatable::GridData::new(grid_columns, rows).unwrap_or(data)
    }

    fn pending_row_backgrounds(
        &self,
        slot: usize,
        data: &sqlly_datatable::GridData,
    ) -> Vec<Option<gpui::Hsla>> {
        if self.pending_slot != Some(slot) || self.pending.is_empty() {
            return Vec::new();
        }
        let Some(session) = self.pending.current().session.as_ref() else {
            return Vec::new();
        };
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.as_slice()) else {
            return Vec::new();
        };
        let skip_marker = data
            .columns
            .first()
            .is_some_and(|c| c.name == crate::grid_bridge::MARKER_COLUMN_NAME);
        let insert_count = self.pending.current().new_rows.len();
        let source_count = data.rows.len().saturating_sub(insert_count);
        let tint = |color: gpui::Hsla| Some(color.opacity(crate::grid_bridge::MARKER_TINT_ALPHA));
        let mut out = Vec::with_capacity(data.rows.len());
        for (i, row) in data.rows.iter().enumerate() {
            if i >= source_count {
                out.push(tint(Theme::success()));
                continue;
            }
            let cells = if skip_marker && !row.is_empty() {
                &row[1..]
            } else {
                row.as_slice()
            };
            let core: Vec<CellValue> = cells.iter().map(grid_cell_to_core).collect();
            let color =
                match crate::pending_changes::row_key_from_result_row(session, columns, &core)
                    .and_then(|key| self.pending.row_state(&key))
                {
                    Some(crate::pending_changes::RowState::Deleted) => tint(Theme::error()),
                    Some(crate::pending_changes::RowState::Edited) => tint(Theme::warning()),
                    None => None,
                };
            out.push(color);
        }
        out
    }

    pub fn stage_undo(&mut self, cx: &mut Context<Self>) {
        if self.pending.undo() {
            if let Some(slot) = self.pending_slot {
                self.rebuild_slot_with_pending_markers(slot, cx);
            }
            if self.pending.is_empty() {
                self.pending_slot = None;
            }
            self.schedule_preview_sql_refresh(cx);
            cx.notify();
        }
    }

    pub fn stage_redo(&mut self, cx: &mut Context<Self>) {
        if self.pending.redo() {
            if let Some(slot) = self.pending_slot {
                self.rebuild_slot_with_pending_markers(slot, cx);
            }
            self.schedule_preview_sql_refresh(cx);
            cx.notify();
        }
    }

    /// Whether the grid holds staged (uncommitted) row edits.
    pub fn has_staged_changes(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn discard_staged(&mut self, cx: &mut Context<Self>) {
        let slot = self.pending_slot;
        self.pending.clear();
        self.pending_slot = None;
        self.preview_sql_text.clear();
        if let Some(slot) = slot {
            self.rebuild_slot_with_pending_markers(slot, cx);
        }
        cx.notify();
    }

    pub fn preview_staged_sql(&mut self, cx: &mut Context<Self>) {
        self.show_preview_sql = true;
        self.select_output_tab(crate::results::chrome::OutputPanelTab::PreviewSql, cx);
    }

    pub fn save_staged(&mut self, cx: &mut Context<Self>) {
        // The Save button is disabled in this case; the command palette
        // route must not emit a save the bridge would refuse either.
        if self.pending.is_empty() || self.staged_save_blocked() {
            return;
        }
        let batch = self.pending.to_batch(self.source_db_type());
        let preview_sql =
            sqlly_services::row_editing::render_preview_script(self.source_db_type(), &batch);
        let rollback_sql =
            sqlly_services::row_editing::render_rollback_script(self.source_db_type(), &batch);
        cx.emit(ResultsViewEvent::SaveStagedRequested {
            batch,
            rollback_sql,
            preview_sql,
        });
    }

    fn schedule_preview_sql_refresh(&mut self, cx: &mut Context<Self>) {
        self.preview_sql_generation = self.preview_sql_generation.wrapping_add(1);
        let generation = self.preview_sql_generation;
        let db = self.source_db_type();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_sql_generation != generation {
                    return;
                }
                if this.pending.is_empty() {
                    this.preview_sql_text.clear();
                } else {
                    let batch = this.pending.to_batch(db);
                    this.preview_sql_text =
                        sqlly_services::row_editing::render_preview_script(db, &batch);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn rebuild_slot_with_pending_markers(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(view) = self.grid_views.get(slot).and_then(|g| g.clone()) else {
            self.ensure_grid_for_slot(slot, cx);
            return;
        };
        let spill = self
            .spill_backings
            .get(slot)
            .is_some_and(|backing| backing.is_some());
        // `ensure_grid_for_slot` takes `result_sets.rows` on first build, so a
        // later drop-and-rebuild would paint an empty grid. Pull source rows
        // back from the live grid (strip a previous marker overlay) first.
        if !spill
            && self
                .result_sets
                .get(slot)
                .is_some_and(|rs| rs.rows.is_empty())
        {
            let hint = self
                .result_sets
                .get(slot)
                .map(|rs| rs.row_count_hint)
                .unwrap_or(0);
            let restored = {
                let state = view.read(cx).state.read(cx);
                let has_marker = state
                    .data
                    .columns
                    .first()
                    .is_some_and(|c| c.name == crate::grid_bridge::MARKER_COLUMN_NAME);
                let mut rows: Vec<Vec<CellValue>> = state
                    .data
                    .rows
                    .iter()
                    .map(|row| {
                        let cells = if has_marker && !row.is_empty() {
                            &row[1..]
                        } else {
                            row.as_slice()
                        };
                        cells.iter().map(grid_cell_to_core).collect()
                    })
                    .collect();
                if has_marker && hint > 0 && hint <= rows.len() {
                    rows.truncate(hint);
                }
                rows
            };
            if let Some(rs) = self.result_sets.get_mut(slot) {
                rs.rows = restored;
            }
        }
        // The sort spec speaks the old grid's columns; the new grid gains or
        // loses the marker column, so move the keys with it.
        let old_marker = self.grid_marker_width_of(slot, view.read(cx).state.read(cx)) as isize;
        let delta = self.marker_offset(slot) as isize - old_marker;
        if delta != 0 {
            self.pad_sort_specs();
            if let Some(spec) = self.sort_specs.get_mut(slot) {
                *spec = crate::column_layout::shift_sort_spec(spec, delta);
            }
        }
        // The rebuilt grid starts unselected; keep the user's cell (shifted
        // past a marker column that appeared or went away) so the Cell
        // Details panel stays on the cell that was just edited.
        let kept_selection = selection_in_result_columns(
            &view.read(cx).state.read(cx).selection,
            old_marker as usize,
        );
        self.grid_views[slot] = None;
        self.ensure_grid_for_slot(slot, cx);
        if !matches!(kept_selection, Selection::None)
            && let Some(state) = self.grid_state_for_slot(slot, cx)
        {
            let selection = selection_in_grid_columns(&kept_selection, self.marker_offset(slot));
            state.update(cx, |s, _| s.selection = selection);
        }
        // Staging changes what the focused cell paints without moving focus.
        if slot == self.active_tab {
            self.refresh_cell_detail(cx);
        }
        cx.emit(ResultsViewEvent::StagedEditsChanged);
    }

    /// Emits [`ResultsViewEvent::FollowForeignKeyRequested`] for a clicked
    /// cell so the host can resolve FK metadata and open the referenced row.
    pub fn request_follow_foreign_key(
        &mut self,
        slot: usize,
        source_row_index: usize,
        column_index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.historical_banner.is_some() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let row = match self.spill_backings.get(slot) {
            Some(Some(backing)) => backing
                .pager
                .borrow_mut()
                .page(source_row_index, 1)
                .into_iter()
                .next(),
            _ => self.source_row_for_slot(slot, source_row_index, cx),
        };
        let Some(row) = row else {
            return;
        };
        cx.emit(ResultsViewEvent::FollowForeignKeyRequested {
            result_set_index: rs.index,
            columns: rs.columns.clone(),
            row,
            column_index,
            sql,
        });
    }

    /// Emits [`ResultsViewEvent::CopyUpdateRequested`] so the host can
    /// resolve the primary key and render UPDATE statements.
    pub fn request_copy_update(
        &mut self,
        slot: usize,
        rows: Vec<Vec<CellValue>>,
        cx: &mut Context<Self>,
    ) {
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let Some(sample_row) = rows.first().cloned() else {
            return;
        };
        cx.emit(ResultsViewEvent::CopyUpdateRequested {
            result_set_index: rs.index,
            columns: rs.columns.clone(),
            rows,
            sample_row,
            sql: self.query_sql.clone().unwrap_or_default(),
        });
    }

    /// Emits [`ResultsViewEvent::StageDeleteManyRequested`] so the host can
    /// open one edit session and stage every selected row as a delete.
    pub fn request_stage_delete_many(
        &mut self,
        slot: usize,
        source_row_indices: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.historical_banner.is_some() {
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        if !self.pending.is_empty() && self.pending_slot != Some(slot) {
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            let slot_one = self.pending_slot.map(|s| s + 1).unwrap_or(0);
            self.messages
                .push(pending_changes_elsewhere_message(n, slot_one));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut rows = Vec::new();
        for source_row_index in source_row_indices {
            if !seen.insert(source_row_index) {
                continue;
            }
            let row = match self.spill_backings.get(slot) {
                Some(Some(backing)) => backing
                    .pager
                    .borrow_mut()
                    .page(source_row_index, 1)
                    .into_iter()
                    .next(),
                _ => self.source_row_for_slot(slot, source_row_index, cx),
            };
            if let Some(row) = row {
                rows.push(row);
            }
        }
        let Some(sample_row) = rows.first().cloned() else {
            return;
        };
        self.staging_slot = Some(slot);
        cx.emit(ResultsViewEvent::StageDeleteManyRequested {
            result_set_index: rs.index,
            columns: rs.columns.clone(),
            rows,
            sample_row,
            sql,
        });
    }

    /// One source row for the record form, spill-aware (single-row page,
    /// never a whole-set read).
    fn form_row(&self, slot: usize, row_index: usize, cx: &gpui::App) -> Option<Vec<CellValue>> {
        if self.spill_backings.get(slot).is_some_and(|b| b.is_some()) {
            let backing = self.spill_backings.get(slot)?.as_ref()?;
            return backing
                .pager
                .borrow_mut()
                .page(row_index, 1)
                .into_iter()
                .next();
        }
        self.source_row_for_slot(slot, row_index, cx)
    }

    /// Build the row-header View/Edit icon wiring for `slot`.
    ///
    /// View opens the read-only record form (any engine); Edit opens the row
    /// editor. The Edit icon shows only when [`Self::row_edit_enabled_for_slot`]
    /// passes: a connected engine with row editing, no SELECT-only /
    /// auto-rollback policy, the first result set of a live (not restored)
    /// run, and a simple single-table `SELECT`. The editor dialog still runs
    /// the authoritative provenance / primary-key resolution and explains the
    /// remaining per-row refusals (no key, row since deleted).
    #[allow(clippy::type_complexity)]
    fn row_actions_for_slot(
        &self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> (
        bool,
        Box<dyn Fn(sqlly_datatable::RowAction, usize, &mut Window, &mut gpui::App)>,
    ) {
        let edit_enabled = self.row_edit_enabled_for_slot(slot);
        let weak = cx.weak_entity();
        let handler = Box::new(
            move |action: sqlly_datatable::RowAction,
                  source_row_index: usize,
                  _window: &mut Window,
                  cx: &mut gpui::App| {
                let Some(results) = weak.upgrade() else {
                    return;
                };
                results.update(cx, |rv, cx| {
                    // The grid reports a resident row; the handlers below
                    // page absolute rows from a spill backing. The grid's
                    // state borrow is released before this handler runs, so
                    // reading its window here is safe.
                    let row =
                        backing_row_for_grid_row(rv.grid_window_offset(slot, cx), source_row_index);
                    match action {
                        sqlly_datatable::RowAction::View => {
                            rv.open_row_form(slot, row, cx);
                        }
                        sqlly_datatable::RowAction::Edit => {
                            rv.open_row_editor_with(
                                slot,
                                row,
                                crate::row_editor::RowEditorIntent::Edit,
                                cx,
                            );
                        }
                    }
                });
            },
        );
        (edit_enabled, handler)
    }

    /// Open the read-only record form on `slot` at `row_index`. Works on
    /// every engine — it reads the already-fetched result, never the server.
    pub fn open_row_form(&mut self, slot: usize, row_index: usize, cx: &mut Context<Self>) {
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let columns = rs.columns.clone();
        let row_count = self.row_count_for_slot(slot);
        let Some(cells) = self.form_row(slot, row_index, cx) else {
            return;
        };
        self.row_form = Some(RowFormState {
            slot,
            row_index,
            row_count,
            columns,
            cells,
        });
        cx.notify();
    }

    fn row_form_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(form) = &self.row_form else {
            return;
        };
        let Some(next) = form.row_index.checked_add_signed(delta) else {
            return;
        };
        if next >= form.row_count {
            return;
        }
        let slot = form.slot;
        if let Some(cells) = self.form_row(slot, next, cx)
            && let Some(form) = &mut self.row_form
        {
            form.row_index = next;
            form.cells = cells;
            cx.notify();
        }
    }

    pub fn close_row_form(&mut self, cx: &mut Context<Self>) {
        self.row_form = None;
        cx.notify();
    }

    fn pop_out_row_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.row_form.take() else {
            return;
        };
        let source = cx.entity();
        let bounds = Bounds::centered(None, size(px(680.0), px(560.0)), cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from(crate::localization::text(
                        "results.row_form.window_title",
                    ))),
                    ..Default::default()
                }),
                app_id: Some(crate::about::APP_ID.into()),
                is_resizable: true,
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|_| PoppedOutRowForm { source, form });
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            },
        );
        cx.notify();
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    pub fn active_result_index(&self) -> usize {
        self.active_tab
    }

    pub fn result_set_count(&self) -> usize {
        self.result_sets.len()
    }

    /// Column names of every result set, in result order.
    pub fn result_set_column_names(&self) -> Vec<Vec<String>> {
        self.result_sets
            .iter()
            .map(|set| {
                set.columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect()
            })
            .collect()
    }

    fn ensure_transpose_len(&mut self) {
        let n = self.result_sets.len().max(self.grid_views.len());
        while self.transposed.len() < n {
            self.transposed.push(None);
            self.transpose_active.push(false);
            self.transpose_truncated.push(false);
            self.transpose_row_count.push(0);
        }
    }

    /// The grid shown for `slot`: the transposed snapshot when that view is
    /// active, otherwise the source result grid.
    pub(crate) fn grid_entity_for_slot(&self, slot: usize) -> Option<Entity<SqllyDataTable>> {
        if self.transpose_active.get(slot).copied().unwrap_or(false) {
            self.transposed.get(slot).and_then(|e| e.as_ref()).cloned()
        } else {
            self.grid_views.get(slot).and_then(|e| e.as_ref()).cloned()
        }
    }

    /// Toggle a transposed snapshot grid for `slot`. The source grid's
    /// sort/filter stay intact; toggling off drops the snapshot.
    pub fn toggle_transpose(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.ensure_transpose_len();
        if self.transpose_active.get(slot).copied().unwrap_or(false) {
            if let Some(flag) = self.transpose_active.get_mut(slot) {
                *flag = false;
            }
            if let Some(grid) = self.transposed.get_mut(slot) {
                *grid = None;
            }
            cx.notify();
            return;
        }
        self.ensure_grid_for_slot(slot, cx);
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        let cap = crate::grid_transpose::TRANSPOSE_MAX_ROWS;
        // A windowed result is read from disk in the background first; the
        // rerun finds the transpose still off and builds it.
        let weak = cx.weak_entity();
        if self.defer_until_spill_prefixes(
            vec![(slot, cap.saturating_add(1))],
            crate::localization::format(
                "results.transpose.reading_rows",
                &[(
                    "row_count",
                    &self
                        .spill_read_count(slot, cap.saturating_add(1))
                        .to_string(),
                )],
            ),
            cx,
            move |cx| {
                let _ = weak.update(cx, |this, cx| this.toggle_transpose(slot, cx));
            },
        ) {
            return;
        }
        let (rows, truncated): (Vec<Vec<CellValue>>, bool) = if let Some(spill_rows) =
            self.spill_rows_for_slot_capped(slot, cap.saturating_add(1))
        {
            let truncated = spill_rows.len() > cap;
            (spill_rows.into_iter().take(cap).collect(), truncated)
        } else {
            let display_indices: Vec<usize> = self
                .grid_views
                .get(slot)
                .and_then(|v| v.as_ref())
                .map(|view| {
                    view.read(cx)
                        .state
                        .read(cx)
                        .display_indices
                        .iter()
                        .copied()
                        .collect()
                })
                .unwrap_or_default();
            // The grid paints staged values, so transpose its rows — minus
            // the staged-edit marker column, which is not a result column.
            let marker = self
                .grid_views
                .get(slot)
                .and_then(|v| v.as_ref())
                .map_or(0, |view| {
                    self.grid_marker_width_of(slot, view.read(cx).state.read(cx))
                });
            let mut source = self.grid_source_rows(slot, cx).unwrap_or_default();
            if marker > 0 {
                for row in &mut source {
                    row.drain(..marker.min(row.len()));
                }
            }
            let ordered: Vec<Vec<CellValue>> = if display_indices.is_empty() {
                source
            } else {
                display_indices
                    .iter()
                    .filter_map(|&i| source.get(i).cloned())
                    .collect()
            };
            let truncated = ordered.len() > cap;
            (ordered.into_iter().take(cap).collect(), truncated)
        };
        let source_row_count = rows.len();
        let (t_columns, t_rows, _) = crate::grid_transpose::transpose(&columns, &rows, cap);
        let data =
            build_grid_data(self.classification_db(), &t_columns, &t_rows).unwrap_or_else(|_| {
                build_grid_data(self.classification_db(), &t_columns, &[]).unwrap_or_else(|_| {
                    sqlly_datatable::GridData::new(vec![], vec![]).expect("empty")
                })
            });
        let theme = build_grid_theme();
        let weak_self = cx.weak_entity();
        let provider = ResultGridMenuProvider::new_pivot(weak_self, self.editor.clone(), slot)
            .with_column_types(t_columns.iter().map(|c| c.data_type.clone()).collect())
            .with_columns(t_columns.clone(), 0)
            .with_source_db(self.classification_db());
        let view = SqllyDataTable::builder(data)
            .config(sqlly_datatable::GridConfig::default())
            .context_menu_provider(provider)
            .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
            .theme(theme)
            .build(cx);
        self.apply_density_to_table(&view, cx);
        let entity = cx.new(|_cx| view);
        cx.observe(&entity, |_, _, cx| {
            cx.notify();
        })
        .detach();
        self.ensure_transpose_len();
        while self.transposed.len() <= slot {
            self.transposed.push(None);
            self.transpose_active.push(false);
            self.transpose_truncated.push(false);
            self.transpose_row_count.push(0);
        }
        self.transposed[slot] = Some(entity);
        self.transpose_active[slot] = true;
        self.transpose_truncated[slot] = truncated;
        self.transpose_row_count[slot] = source_row_count;
        cx.notify();
    }

    /// Record (or clear, with an empty message) the saved-filter limitation
    /// caption for `slot`, keeping the per-slot vector padded.
    fn set_filter_limitation(&mut self, slot: usize, message: String) {
        while self.filter_limitations.len() <= slot {
            self.filter_limitations.push(String::new());
        }
        self.filter_limitations[slot] = message;
    }

    /// The caption strip above a grid: the transpose note and/or the
    /// saved-filter limitation notice for `slot`.
    fn grid_caption(&self, slot: usize) -> Option<String> {
        let transpose = self.transpose_caption(slot);
        let limitation = self
            .filter_limitations
            .get(slot)
            .filter(|m| !m.is_empty())
            .cloned();
        let parts: Vec<String> = [
            transpose,
            limitation,
            self.background_read_caption(slot),
            self.spill_drill_caption(slot),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    fn transpose_caption(&self, slot: usize) -> Option<String> {
        if !self.transpose_active.get(slot).copied().unwrap_or(false) {
            return None;
        }
        let n = self.transpose_row_count.get(slot).copied().unwrap_or(0);
        let truncated = self.transpose_truncated.get(slot).copied().unwrap_or(false);
        Some(crate::localization::format(
            if truncated {
                "results.transpose.truncated_caption"
            } else {
                "results.transpose.complete_caption"
            },
            &[("row_count", &n.to_string())],
        ))
    }

    /// Open the visual filter builder on `slot`, seeded from the grid's
    /// current operator filters. Header value checklists are not part of the
    /// builder; they stay on the grid and survive every builder apply.
    pub fn open_filter_builder(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.open_filter_builder_with(slot, None, cx);
    }

    fn open_filter_builder_with(
        &mut self,
        slot: usize,
        seed: Option<FilterRow>,
        cx: &mut Context<Self>,
    ) {
        let Some(result_columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        self.ensure_grid_for_slot(slot, cx);
        let Some(view) = self.grid_views.get(slot).and_then(|v| v.as_ref()) else {
            return;
        };
        let spill_backed = matches!(self.spill_backings.get(slot), Some(Some(_)));
        // Result columns only: the builder (and every model it produces)
        // indexes result columns, never the staged-edit marker column.
        let (column_kinds, column_names) = {
            let state = view.read(cx).state.read(cx);
            let marker = grid_marker_width(&state.data.columns, result_columns.len());
            let columns = &state.data.columns[marker..];
            let kinds: Vec<ColumnKind> = columns.iter().map(|c| c.kind).collect();
            let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            (kinds, names)
        };
        let mut model = self.builder_seed_model(slot, cx);
        if let Some(row) = seed {
            model.and_row(row);
        } else if model.flat_rows().is_empty()
            && let Some(kind) = column_kinds.first()
        {
            let op = FilterBuilderModel::ops_for(*kind)
                .first()
                .copied()
                .unwrap_or(BuilderOp::Contains);
            model.push_row(FilterRow::new(0, op));
        }
        let scope = self.current_filter_scope(slot);
        let non_iso_date_columns = self.non_iso_date_columns(slot, &column_names, &column_kinds);
        let inputs = model
            .flat_rows()
            .iter()
            .map(|_| FilterBuilderInputs { a: None, b: None })
            .collect();
        self.filter_builder = Some(FilterBuilderState {
            slot,
            model,
            inputs,
            root_conjunction_select: None,
            group_conjunction_selects: HashMap::new(),
            column_kinds,
            column_names,
            result_columns,
            spill_backed,
            scope,
            saved_select: None,
            saved_error: None,
            saved_chosen: None,
            saved_name_input: None,
            expression_input: None,
            expression_error: None,
            expression_seed: None,
            non_iso_date_columns,
        });
        cx.notify();
    }

    /// Grid columns of `slot` that are dates displayed in a non-ISO format.
    /// Date comparisons read the cell text as `YYYY-MM-DD…`, so on these
    /// columns they match nothing; the builder warns instead of failing
    /// silently.
    fn non_iso_date_columns(
        &self,
        slot: usize,
        names: &[String],
        kinds: &[ColumnKind],
    ) -> Vec<usize> {
        let prefs = self.effective_formatting_for_slot(slot);
        names
            .iter()
            .zip(kinds)
            .enumerate()
            .filter(|(_, (_, kind))| **kind == ColumnKind::Date)
            .filter(|(_, (name, _))| {
                let fmt = prefs
                    .column_overrides
                    .get(name.as_str())
                    .filter(|o| !o.is_inherit())
                    .map(|o| &o.date_format)
                    .unwrap_or(&prefs.defaults.date_format);
                let as_stored =
                    fmt.time_zone_mode == crate::preferences::DateTimeZoneMode::NativeAsStored;
                !as_stored && !fmt.format.trim_start().starts_with("yyyy-MM-dd")
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The model the builder opens with for `slot`: the last model applied
    /// there when it still describes what the grid shows (so disabled rows,
    /// OR groups, and date comparisons survive closing the builder), else
    /// one derived from the grid's current operator filters. Header-menu
    /// filter edits or "Clear Filters" make the remembered model stale.
    /// Value checklists are compared out: they are the header's, not the
    /// builder's, and applying the builder keeps them.
    fn builder_seed_model(&self, slot: usize, cx: &App) -> FilterBuilderModel {
        let Some(view) = self.grid_views.get(slot).and_then(|v| v.as_ref()) else {
            return FilterBuilderModel::default();
        };
        let state = view.read(cx).state.read(cx);
        // Compare and derive in result columns: the grid's filters and
        // formats carry a leading entry for the marker column while edits
        // are staged.
        let marker = self.grid_marker_width_of(slot, state);
        let columns = &state.data.columns[marker..];
        let grid_filters = state.filters.get(marker..).unwrap_or(&[]);
        let kinds: Vec<ColumnKind> = columns.iter().map(|c| c.kind).collect();
        let null_texts: Vec<String> = state
            .resolved_formats
            .iter()
            .skip(marker)
            .map(|fmt| fmt.null.text.clone())
            .collect();
        let tree = self.client_filter.get(slot).and_then(|f| f.as_ref());
        if let Some(applied) = self.applied_filter_models.get(&slot) {
            let applied = &applied.model;
            let still_current = match tree {
                Some(tree) => *tree == applied.root,
                None => {
                    applied.is_flat_and()
                        && applied
                            .to_column_filters(columns, &null_texts)
                            .is_some_and(|filters| {
                                filters.len() == grid_filters.len()
                                    && filters
                                        .iter()
                                        .zip(grid_filters)
                                        .all(|(a, b)| a.predicate == b.predicate)
                            })
                }
            };
            if still_current {
                return applied.clone();
            }
        }
        if let Some(tree) = tree {
            return FilterBuilderModel { root: tree.clone() };
        }
        FilterBuilderModel::from_column_filters(grid_filters, &kinds, &null_texts)
    }

    /// Quick-filter entry point (cell menu, top-values window): AND one
    /// condition onto the slot's current filter (it always narrows, even
    /// under an OR tree), apply it immediately, and surface the builder so
    /// the active conditions are visible and can be toggled or removed.
    /// Windowed (spill-backed) results cannot be filtered in the app; the
    /// refusal is shown in the slot's caption instead of silently doing
    /// nothing. `row` targets a RESULT column (callers translate grid
    /// columns through [`Self::grid_marker_width_of`]).
    pub fn apply_quick_filter(&mut self, slot: usize, row: FilterRow, cx: &mut Context<Self>) {
        if self.spill_install_pending || matches!(self.spill_backings.get(slot), Some(Some(_))) {
            self.set_filter_limitation(slot, windowed_quick_filter_notice());
            cx.notify();
            return;
        }
        if self.filter_builder.as_ref().is_some_and(|b| b.slot != slot) {
            self.filter_builder = None;
        }
        if self.filter_builder.is_none() {
            self.open_filter_builder_with(slot, Some(row), cx);
        } else {
            // Keep text typed into the open builder but not applied yet.
            self.sync_filter_builder_inputs(cx);
            if let Some(builder) = self.filter_builder.as_mut() {
                builder.model.and_row(row);
                builder.reset_tree_widgets();
            }
        }
        self.apply_filter_builder(cx);
    }

    pub fn close_filter_builder(&mut self, cx: &mut Context<Self>) {
        self.filter_builder = None;
        cx.notify();
    }

    fn sync_filter_builder_inputs(&mut self, cx: &App) {
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        for (i, inputs) in builder.inputs.iter().enumerate() {
            let Some(row) = builder.model.leaf_row_mut(i) else {
                break;
            };
            if let Some(input) = &inputs.a {
                row.a = input.read(cx).value().to_string();
            }
            if let Some(input) = &inputs.b {
                row.b = input.read(cx).value().to_string();
            }
        }
    }

    /// Write the builder's conditions into the grid's `ColumnFilter`s.
    pub fn apply_filter_builder(&mut self, cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(builder) = self.filter_builder.as_ref() else {
            return;
        };
        if builder.spill_backed {
            return;
        }
        let slot = builder.slot;
        let model = builder.model.clone();
        self.apply_filter_model_to_slot(slot, &model, cx);
    }

    /// Write a filter model into a slot's grid: flat AND models with only
    /// datatable-representable predicates become `GridState.filters`; trees,
    /// Any-text targets, the new operators, date-column comparisons, and
    /// integers past 2^53 go through the client-side tree predicate instead.
    /// Either way the header value checklists (`ColumnFilter.values`) are
    /// kept: the builder only owns the operator half of each filter.
    fn apply_filter_model_to_slot(
        &mut self,
        slot: usize,
        model: &FilterBuilderModel,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.spill_backings.get(slot), Some(Some(_))) {
            return;
        }
        let Some(view) = self.grid_views.get(slot).and_then(|v| v.as_ref()).cloned() else {
            return;
        };
        let state_entity = view.read(cx).state.clone();
        // The model indexes result columns; the grid's `filters` index grid
        // columns, which lead with the marker column while edits are staged.
        let (marker, columns, null_texts) = {
            let state = state_entity.read(cx);
            let marker = self.grid_marker_width_of(slot, state);
            let columns = state.data.columns[marker..].to_vec();
            let null_texts: Vec<String> = state
                .resolved_formats
                .iter()
                .skip(marker)
                .map(|fmt| fmt.null.text.clone())
                .collect();
            (marker, columns, null_texts)
        };
        let filters = model
            .to_column_filters(&columns, &null_texts)
            .map(|filters| {
                let mut grid_filters = vec![sqlly_datatable::ColumnFilter::default(); marker];
                grid_filters.extend(filters);
                grid_filters
            });
        let is_flat = model.is_flat_and() && filters.is_some();
        let tree = model.root.clone();
        self.applied_filter_models.insert(
            slot,
            AppliedFilter {
                model: model.clone(),
                columns: columns.iter().map(|c| c.name.clone()).collect(),
            },
        );
        self.pad_client_filter_slots();
        if is_flat {
            if slot < self.client_filter.len() {
                self.client_filter[slot] = None;
            }
            self.set_source_index_map(slot, None);
            state_entity.update(cx, |state, cx| {
                let mut filters = filters.unwrap_or_default();
                for (filter, old) in filters.iter_mut().zip(&state.filters) {
                    filter.values = old.values.clone();
                }
                state.filters = filters;
                state.filter_panel = None;
                state.recompute();
                cx.notify();
            });
        } else {
            state_entity.update(cx, |state, cx| {
                for filter in state.filters.iter_mut() {
                    filter.predicate = sqlly_datatable::FilterPredicate::None;
                }
                state.filter_panel = None;
                state.recompute();
                cx.notify();
            });
            if slot < self.client_filter.len() {
                self.client_filter[slot] = Some(tree);
            }
            self.rebuild_slot_from_source(slot, cx);
        }
        cx.notify();
    }

    /// The saved-filter scope for result set `slot`: the connection that ran
    /// the query plus what produced the set (a table, a statement, or one
    /// result of a batch — see [`crate::saved_filters::filter_scope`]).
    /// `None` without a connection scope or a query.
    pub(crate) fn current_filter_scope(
        &self,
        slot: usize,
    ) -> Option<crate::saved_filters::FilterScope> {
        let scope = self.layout_scope.as_ref()?;
        let sql = self.query_sql.as_deref()?;
        if sql.trim().is_empty() {
            return None;
        }
        Some(crate::saved_filters::filter_scope(scope, sql, slot))
    }

    /// The saved-filter store: the test override when one is set (tests
    /// never touch the real `saved-filters.json`), else disk. `Err` is a
    /// store that exists but cannot be read — never to be saved over.
    fn load_saved_filters(&self) -> Result<crate::saved_filters::SavedFilterStore, String> {
        match &self.saved_filter_store_for_test {
            Some(store) => Ok(store.clone()),
            None => crate::saved_filters::SavedFilterStore::load(),
        }
    }

    fn save_saved_filters(
        &mut self,
        store: crate::saved_filters::SavedFilterStore,
    ) -> Result<(), String> {
        if self.saved_filter_store_for_test.is_some() {
            self.saved_filter_store_for_test = Some(store);
            return Ok(());
        }
        store.save()
    }

    /// "Today" for relative-date filters on `slot`, in the time zone the
    /// grid renders that slot's datetimes in (the default is UTC).
    fn filter_today(&self, slot: usize) -> chrono::NaiveDate {
        let fmt = self
            .effective_formatting_for_slot(slot)
            .defaults
            .date_format;
        let zone = crate::result_cell_format::resolve_zone(
            fmt.time_zone_mode.clone(),
            &fmt.time_zone_identifier,
        )
        .unwrap_or(crate::result_cell_format::ResolvedZone::Utc);
        crate::filter_lang::today_in_zone(&zone, chrono::Utc::now())
    }

    /// The limitation message for a windowed (spill-backed) slot: names the
    /// scope's default saved filter when one exists — that is the filter the
    /// user expects to see applied — else the generic form.
    fn windowed_filter_notice(&self, slot: usize) -> String {
        let named = self
            .current_filter_scope(slot)
            .filter(|scope| scope.allows_default)
            .and_then(|scope| {
                self.load_saved_filters()
                    .ok()?
                    .default_for(&scope.key)
                    .map(|saved| saved.name.clone())
            });
        match named {
            Some(name) => crate::localization::format(
                "results.filters.default_filter_windowed_notice",
                &[("filter_name", &name)],
            ),
            None => crate::localization::text("results.filters.windowed_result_notice"),
        }
    }

    fn filter_lang_columns_for_slot(
        &self,
        slot: usize,
        cx: &App,
    ) -> Vec<crate::filter_lang::FilterLangColumn> {
        self.grid_views
            .get(slot)
            .and_then(|v| v.as_ref())
            .map(|view| {
                // Result columns: models resolved against this list must not
                // count the staged-edit marker column.
                let state = view.read(cx).state.read(cx);
                let columns = &state.data.columns[self.grid_marker_width_of(slot, state)..];
                crate::filter_lang::filter_lang_columns(
                    columns.iter().map(|c| c.name.as_str()),
                    columns.iter().map(|c| c.kind),
                )
            })
            .unwrap_or_default()
    }

    /// Runs after every grid build for `slot`. On the first build of a fresh
    /// result it applies the scope's default saved filter; on a rebuild of
    /// the same result (staged edits, marker overlays) it restores the flat
    /// filter the user last applied — tree filters survive rebuilds through
    /// `client_filter` — so a rebuild never re-imposes a default the user
    /// changed or cleared. The default is resolved by column name; conditions
    /// on columns this result lacks are dropped and named in the caption.
    /// Spill-backed slots cannot take client filters, so the skip is shown
    /// as a caption instead of failing silently.
    fn apply_default_saved_filter(&mut self, slot: usize, cx: &mut Context<Self>) {
        if !self.default_filter_consulted.insert(slot) {
            self.restore_applied_filter_after_rebuild(slot, cx);
            return;
        }
        self.set_filter_limitation(slot, String::new());
        let Some(scope) = self.current_filter_scope(slot) else {
            return;
        };
        if !scope.allows_default {
            return;
        }
        let store = match self.load_saved_filters() {
            Ok(store) => store,
            Err(message) => {
                self.set_filter_limitation(slot, message);
                return;
            }
        };
        let Some(saved) = store.default_for(&scope.key).cloned() else {
            return;
        };
        if saved.model.leaf_count() == 0 {
            return;
        }
        if self.spill_install_pending || matches!(self.spill_backings.get(slot), Some(Some(_))) {
            let notice = self.windowed_filter_notice(slot);
            self.set_filter_limitation(slot, notice);
            return;
        }
        let columns = self.filter_lang_columns_for_slot(slot, cx);
        let resolved =
            crate::filter_lang::resolve_saved_model(&saved, &columns, self.filter_today(slot));
        if !resolved.missing_columns.is_empty() {
            self.set_filter_limitation(
                slot,
                missing_columns_notice(&saved.name, &resolved.missing_columns),
            );
        }
        if resolved.model.leaf_count() == 0 {
            return;
        }
        self.apply_filter_model_to_slot(slot, &resolved.model, cx);
    }

    /// A rebuilt grid starts with no `ColumnFilter`s: put back the flat
    /// filter last applied to `slot`. Its indices are result columns, which
    /// a staged edit's marker column does not move, so it applies as is
    /// while the result's columns are unchanged; only a result with
    /// different columns is re-targeted by name.
    fn restore_applied_filter_after_rebuild(&mut self, slot: usize, cx: &mut Context<Self>) {
        if matches!(self.spill_backings.get(slot), Some(Some(_))) {
            return;
        }
        let Some(applied) = self.applied_filter_models.get(&slot).cloned() else {
            return;
        };
        if !applied.model.is_flat_and() || self.client_filter.get(slot).is_some_and(Option::is_some)
        {
            return;
        }
        let same_columns = self.result_sets.get(slot).is_some_and(|rs| {
            rs.columns.len() == applied.columns.len()
                && rs
                    .columns
                    .iter()
                    .zip(&applied.columns)
                    .all(|(column, name)| column.name == *name)
        });
        let model = if same_columns {
            applied.model
        } else {
            let columns = self.filter_lang_columns_for_slot(slot, cx);
            crate::filter_lang::retarget_by_name(&applied.model, &applied.columns, &columns).model
        };
        if model.leaf_count() > 0 {
            self.apply_filter_model_to_slot(slot, &model, cx);
        }
    }

    /// Save the builder's current conditions under the typed name (insert or
    /// replace), then refresh the saved-filters dropdown. Each condition's
    /// column name is stored so the set applies by name later.
    fn filter_builder_save_named(&mut self, cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(slot) = self.filter_builder.as_ref().map(|b| b.slot) else {
            return;
        };
        let today = self.filter_today(slot);
        let (scope, filter) = {
            let Some(builder) = self.filter_builder.as_ref() else {
                return;
            };
            let name = builder
                .saved_name_input
                .as_ref()
                .map(|input| input.read(cx).value().trim().to_string())
                .unwrap_or_default();
            let Some(scope) = builder.scope.clone() else {
                return;
            };
            if name.is_empty() {
                return;
            }
            let expression = builder.expression_input.as_ref().and_then(|input| {
                let columns = crate::filter_lang::filter_lang_columns(
                    builder.column_names.iter().map(String::as_str),
                    builder.column_kinds.iter().copied(),
                );
                crate::filter_lang::expression_for_save(
                    &input.read(cx).value(),
                    &builder.model,
                    &columns,
                    today,
                )
            });
            let filter = crate::saved_filters::SavedFilter {
                name,
                is_default: false,
                model: builder.model.clone(),
                expression,
                columns: builder.column_names.clone(),
            };
            (scope, filter)
        };
        let name = filter.name.clone();
        let result = self.load_saved_filters().and_then(|mut store| {
            store.upsert(&scope.key, filter);
            self.save_saved_filters(store)
        });
        if let Some(builder) = self.filter_builder.as_mut() {
            builder.saved_select = None;
            match result {
                Ok(()) => {
                    builder.saved_chosen = Some(name);
                    builder.saved_error = None;
                }
                Err(message) => builder.saved_error = Some(message),
            }
        }
        cx.notify();
    }

    /// Apply the chosen saved filter: replace the builder's model — every
    /// condition re-targeted by column name — and write it to the grid.
    fn filter_builder_apply_saved(&mut self, cx: &mut Context<Self>) {
        let (slot, scope, name) = {
            let Some(builder) = self.filter_builder.as_ref() else {
                return;
            };
            match (builder.scope.clone(), builder.saved_chosen.clone()) {
                (Some(scope), Some(name)) => (builder.slot, scope, name),
                _ => return,
            }
        };
        let today = self.filter_today(slot);
        let store = match self.load_saved_filters() {
            Ok(store) => store,
            Err(message) => {
                if let Some(builder) = self.filter_builder.as_mut() {
                    builder.saved_error = Some(message);
                }
                cx.notify();
                return;
            }
        };
        let Some(saved) = store.get(&scope.key, &name).cloned() else {
            return;
        };
        if let Some(builder) = self.filter_builder.as_mut() {
            let columns = crate::filter_lang::filter_lang_columns(
                builder.column_names.iter().map(String::as_str),
                builder.column_kinds.iter().copied(),
            );
            // Re-resolve a stored expression so relative dates track today.
            let resolved = crate::filter_lang::resolve_saved_model(&saved, &columns, today);
            builder.model = resolved.model;
            builder.reset_tree_widgets();
            builder.expression_error = None;
            builder.saved_error = (!resolved.missing_columns.is_empty())
                .then(|| missing_columns_notice(&saved.name, &resolved.missing_columns));
            if let Some(text) = saved.expression.clone() {
                builder.expression_seed = Some(text);
                builder.expression_input = None;
            }
        }
        self.apply_filter_builder(cx);
    }

    fn filter_builder_set_default_saved(&mut self, cx: &mut Context<Self>) {
        let Some((scope, name)) = self
            .filter_builder
            .as_ref()
            .and_then(|builder| Some((builder.scope.clone()?, builder.saved_chosen.clone()?)))
        else {
            return;
        };
        if !scope.allows_default {
            if let Some(builder) = self.filter_builder.as_mut() {
                builder.saved_error = Some(batch_default_notice());
            }
            cx.notify();
            return;
        }
        let result = self.load_saved_filters().and_then(|mut store| {
            // Toggling: setting the current default again clears it.
            let already =
                store.default_for(&scope.key).map(|f| f.name.as_str()) == Some(name.as_str());
            if already {
                store.set_default(&scope.key, None);
            } else {
                store.set_default(&scope.key, Some(&name));
            }
            self.save_saved_filters(store)
        });
        if let Some(builder) = self.filter_builder.as_mut() {
            builder.saved_select = None;
            builder.saved_error = result.err();
        }
        cx.notify();
    }

    fn filter_builder_rename_saved(&mut self, cx: &mut Context<Self>) {
        let Some((scope, old_name, new_name)) = self.filter_builder.as_ref().and_then(|builder| {
            let new_name = builder
                .saved_name_input
                .as_ref()
                .map(|input| input.read(cx).value().trim().to_string())
                .unwrap_or_default();
            Some((
                builder.scope.clone()?,
                builder.saved_chosen.clone()?,
                new_name,
            ))
        }) else {
            return;
        };
        let result = self.load_saved_filters().and_then(|mut store| {
            if !store.rename(&scope.key, &old_name, &new_name) {
                return Err(crate::localization::format(
                    "results.filters.rename_failed",
                    &[("old_name", &old_name)],
                ));
            }
            self.save_saved_filters(store)
        });
        if let Some(builder) = self.filter_builder.as_mut() {
            match result {
                Ok(()) => {
                    builder.saved_select = None;
                    builder.saved_chosen = Some(new_name);
                    builder.saved_error = None;
                }
                Err(message) => builder.saved_error = Some(message),
            }
        }
        cx.notify();
    }

    fn filter_builder_delete_saved(&mut self, cx: &mut Context<Self>) {
        let Some((scope, name)) = self
            .filter_builder
            .as_ref()
            .and_then(|builder| Some((builder.scope.clone()?, builder.saved_chosen.clone()?)))
        else {
            return;
        };
        let result = self.load_saved_filters().and_then(|mut store| {
            if !store.delete(&scope.key, &name) {
                return Ok(());
            }
            self.save_saved_filters(store)
        });
        if let Some(builder) = self.filter_builder.as_mut() {
            match result {
                Ok(()) => {
                    builder.saved_select = None;
                    builder.saved_chosen = None;
                    builder.saved_error = None;
                }
                Err(message) => builder.saved_error = Some(message),
            }
        }
        cx.notify();
    }

    /// Parse the typed filter expression and replace the builder's model.
    /// Invalid input shows a red hint and leaves the model untouched.
    fn filter_builder_apply_expression(&mut self, cx: &mut Context<Self>) {
        let (slot, text, columns) = {
            let Some(builder) = self.filter_builder.as_ref() else {
                return;
            };
            let text = builder
                .expression_input
                .as_ref()
                .map(|input| input.read(cx).value().to_string())
                .unwrap_or_default();
            let columns: Vec<crate::filter_lang::FilterLangColumn> = builder
                .column_names
                .iter()
                .zip(builder.column_kinds.iter())
                .map(|(name, kind)| crate::filter_lang::FilterLangColumn::new(name.clone(), *kind))
                .collect();
            (builder.slot, text, columns)
        };
        let today = self.filter_today(slot);
        match crate::filter_lang::parse_filter_expression(&text, &columns, today) {
            Ok(model) => {
                if let Some(builder) = self.filter_builder.as_mut() {
                    builder.model = model;
                    builder.reset_tree_widgets();
                    builder.expression_error = None;
                }
                self.apply_filter_builder(cx);
            }
            Err(message) => {
                if let Some(builder) = self.filter_builder.as_mut() {
                    builder.expression_error = Some(message);
                }
                cx.notify();
            }
        }
    }

    fn apply_filter_builder_to_query(&mut self, cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(builder) = self.filter_builder.as_ref() else {
            return;
        };
        let Some(editor_handle) = &self.editor else {
            return;
        };
        let Some(editor) = editor_handle.upgrade() else {
            return;
        };
        let db = self.source_db_type();
        let clause =
            builder
                .model
                .to_where_clause(db, &builder.result_columns, &builder.column_kinds);
        let sql = editor.read(cx).text().to_string();
        // The grid's sort travels with the filters (Sequel Ace #2193).
        let order_by = self
            .grid_sort_keys_for_query(builder.slot, cx)
            .and_then(|keys| {
                crate::order_by_actions::order_by_clause_for(
                    db,
                    &builder.result_columns,
                    &keys,
                    Some(&sql),
                )
            });
        if clause.is_none() && order_by.is_none() {
            return;
        }
        let mut updated = sql;
        if let Some(clause) = clause {
            let action = crate::where_actions::WhereAction {
                title: crate::localization::text("results.filters.builder_action_title"),
                column_name: String::new(),
                column_index: 0,
                values: vec![],
                operator: crate::where_actions::WhereOperator::Equals,
                mode: crate::where_actions::WhereActionMode::Add,
                join: crate::where_actions::WhereLogicalJoin::And,
                predicate_preview: clause,
                source: None,
            };
            updated =
                crate::where_actions::apply_where_action_for(db, &updated, &action).updated_sql;
        }
        if let Some(order_by) = order_by {
            updated = crate::order_by_actions::apply_order_by(&updated, &order_by);
        }
        editor.update(cx, |ed, cx| {
            ed.set_text(updated, cx);
        });
        self.close_filter_builder(cx);
    }

    /// The grid's sort for `slot` (primary plus secondaries) as keys over
    /// result columns: the staged-edit marker column is subtracted and keys
    /// on grid-only columns are left for [`crate::order_by_actions`] to skip.
    /// `None` when the grid is unsorted.
    pub(crate) fn grid_sort_keys_for_query(
        &self,
        slot: usize,
        cx: &App,
    ) -> Option<Vec<crate::grid_sort::SortKey>> {
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return None;
        };
        let state = view.read(cx).state.read(cx);
        let marker = self.grid_marker_width_of(slot, state);
        let mut spec = self.sort_specs.get(slot).cloned().unwrap_or_default();
        spec.set_primary(state.sort);
        let keys: Vec<crate::grid_sort::SortKey> = spec
            .keys
            .iter()
            .filter(|key| key.column >= marker)
            .map(|key| crate::grid_sort::SortKey {
                column: key.column - marker,
                descending: key.descending,
            })
            .collect();
        (!keys.is_empty()).then_some(keys)
    }

    fn filter_builder_add_row(&mut self, cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        builder.model.add_row_at_root(&builder.column_kinds);
        builder.inputs.clear();
        cx.notify();
    }

    fn filter_builder_add_group(&mut self, cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        builder.model.add_group(&builder.column_kinds);
        builder.reset_tree_widgets();
        cx.notify();
    }

    fn filter_builder_add_row_in_group(&mut self, path: &[usize], cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        builder.model.add_row_in_group(path, &builder.column_kinds);
        builder.inputs.clear();
        cx.notify();
    }

    fn filter_builder_remove_group(&mut self, path: &[usize], cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        builder.model.remove_group(path);
        builder.reset_tree_widgets();
        cx.notify();
    }

    fn filter_builder_clear_rows(&mut self, cx: &mut Context<Self>) {
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        builder.model.clear_items();
        builder.reset_tree_widgets();
        cx.notify();
    }

    fn filter_builder_remove_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_filter_builder_inputs(cx);
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        builder.model.remove_leaf(index);
        builder.reset_tree_widgets();
        cx.notify();
    }

    fn filter_builder_toggle_enabled(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        let Some(row) = builder.model.leaf_row_mut(index) else {
            return;
        };
        row.enabled = !row.enabled;
        // Pausing or resuming a condition re-filters the grid right away —
        // the checkbox is a live toggle, not a draft edit.
        self.apply_filter_builder(cx);
        cx.notify();
    }

    fn filter_builder_cycle_column(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        let n = builder.column_kinds.len();
        let Some(row) = builder.model.leaf_row_mut(index) else {
            return;
        };
        if n == 0 {
            return;
        }
        row.column = match row.column {
            ColumnTarget::AnyText => ColumnTarget::Index(0),
            ColumnTarget::Index(i) if i + 1 >= n => ColumnTarget::AnyText,
            ColumnTarget::Index(i) => ColumnTarget::Index(i + 1),
        };
        let ops = FilterBuilderModel::ops_for_target(row.column, &builder.column_kinds);
        if !ops.contains(&row.op) {
            row.op = ops.first().copied().unwrap_or(BuilderOp::Contains);
        }
        cx.notify();
    }

    fn filter_builder_cycle_op(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(builder) = self.filter_builder.as_mut() else {
            return;
        };
        let Some(row) = builder.model.leaf_row_mut(index) else {
            return;
        };
        let ops = FilterBuilderModel::ops_for_target(row.column, &builder.column_kinds);
        if ops.is_empty() {
            return;
        }
        let pos = ops.iter().position(|op| *op == row.op).unwrap_or(0);
        row.op = ops[(pos + 1) % ops.len()];
        cx.notify();
    }

    /// Stamp of the result currently shown; see `result_epoch`.
    pub(crate) fn result_epoch(&self) -> u64 {
        self.result_epoch
    }

    /// For a windowed (spill-backed) slot: its spill file and the rows safe
    /// to read from it (the backing's `total`). Column-wide tools read the
    /// file on the background executor from this — never on the UI thread.
    /// `None` for resident slots, whose rows live in the grid.
    pub(crate) fn spill_scan_source(&self, slot: usize) -> Option<(String, usize)> {
        match self.spill_backings.get(slot) {
            Some(Some(backing)) => Some((backing.path.clone(), backing.total)),
            _ => None,
        }
    }

    /// Start a background scan of result column `column` of windowed slot
    /// `slot` for Copy Distinct / Profile: the first rows are read from the
    /// spill file off the UI thread while the grid caption says what is
    /// being counted, then the result lands on the clipboard. A newer scan
    /// supersedes an older one (generation token), and a result replaced
    /// meanwhile (result epoch) drops the outcome silently. Returns false —
    /// doing nothing — for resident slots.
    pub(crate) fn start_column_scan(
        &mut self,
        slot: usize,
        column: usize,
        purpose: ColumnScanPurpose,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((path, total)) = self.spill_scan_source(slot) else {
            return false;
        };
        let Some(meta) = self
            .result_sets
            .get(slot)
            .and_then(|rs| rs.columns.get(column))
            .cloned()
        else {
            return false;
        };
        let formatting = self.effective_formatting_for_slot(slot);
        let cap = purpose.row_cap();
        let count = cap.min(total);
        self.background_read_generation = self.background_read_generation.wrapping_add(1);
        let generation = self.background_read_generation;
        let epoch = self.result_epoch;
        let caption = purpose.caption(&meta.name, count);
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let cells = spill::read_spill_column(&path, column, count).await?;
                    Some(purpose.finish(&meta, cells, &formatting))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_column_scan(generation, epoch, total, outcome, cx);
            });
        });
        self.background_read = Some(BackgroundRead {
            slot,
            generation,
            caption,
            _task: task,
        });
        cx.notify();
        true
    }

    /// Foreground end of [`Self::start_column_scan`]: a superseded scan or a
    /// replaced result is dropped silently; otherwise the text goes to the
    /// clipboard and any notice (nothing to copy, rows left unread, a failed
    /// read) goes to Messages.
    fn finish_column_scan(
        &mut self,
        generation: u64,
        epoch: u64,
        total: usize,
        outcome: Option<ColumnScanOutcome>,
        cx: &mut Context<Self>,
    ) {
        if self.background_read.as_ref().map(|scan| scan.generation) != Some(generation) {
            return;
        }
        self.background_read = None;
        cx.notify();
        if epoch != self.result_epoch {
            return;
        }
        let Some(outcome) = outcome else {
            self.note_copy_notice(
                crate::localization::text("results.copy.spill_read_failed"),
                cx,
            );
            return;
        };
        match outcome.text {
            Some(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
            None => {
                self.note_copy_notice(
                    crate::localization::text("results.grid_menu.copy_values.empty"),
                    cx,
                );
                return;
            }
        }
        if outcome.report_unread && total > outcome.read_rows {
            let read_rows = outcome.read_rows;
            self.note_copy_notice(
                crate::localization::format(
                    "results.grid_menu.copy_values.spill_sample_notice",
                    &[
                        ("copied_rows", &read_rows.to_string()),
                        ("total_rows", &total.to_string()),
                    ],
                ),
                cx,
            );
        }
    }

    /// Caption shown while a background column scan runs on `slot`.
    fn background_read_caption(&self, slot: usize) -> Option<String> {
        self.background_read
            .as_ref()
            .filter(|scan| scan.slot == slot)
            .map(|scan| scan.caption.clone())
    }

    /// Total row count of a windowed slot, `None` when resident.
    pub(crate) fn spill_scan_total(&self, slot: usize) -> Option<usize> {
        match self.spill_backings.get(slot) {
            Some(Some(backing)) => Some(backing.total),
            _ => None,
        }
    }

    /// Why a column copy of `slot` must copy nothing: the result has no
    /// rows, or a filter hides every one of them (the visible-rows fallback
    /// would otherwise hand back the whole unfiltered column). `None` when
    /// there are rows to read.
    pub(crate) fn nothing_visible_to_copy(&self, slot: usize, state: &GridState) -> Option<String> {
        if self.row_count_for_slot(slot) == 0 {
            return Some(crate::localization::text("results.copy.no_rows"));
        }
        if state.window.is_none() && state.display_row_count() == 0 {
            return Some(crate::localization::text(
                "results.copy.all_rows_filtered_out",
            ));
        }
        None
    }

    /// Record a column-copy notice (nothing copied, or a partial copy of a
    /// windowed result) in Messages, where the grid menu — which has no
    /// window for a toast — can surface it.
    pub(crate) fn note_copy_notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.messages.push(message);
        self.show_messages = true;
        cx.notify();
    }

    /// Open the "Save as Snapshot…" name prompt for `slot`.
    /// Queue the conditional-formatting dialog for `column`; the next render
    /// (which has the `Window` the input state needs) materializes it.
    pub fn open_conditional_formatting(&mut self, column: String, cx: &mut Context<Self>) {
        self.pending_conditional_formatting = Some(column);
        cx.notify();
    }

    /// Materialize a queued conditional-formatting dialog and mount the open
    /// one, lifted to a viewport-sized overlay like the row form so the
    /// non-modal card can float anywhere in the window.
    fn render_conditional_formatting(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if let Some(column) = self.pending_conditional_formatting.take() {
            let weak = cx.weak_entity();
            let entity = cx.new(|cx| {
                crate::conditional_formatting::ConditionalFormattingDialog::new(
                    column, weak, window, cx,
                )
            });
            let subscription = cx.subscribe(&entity, |this: &mut Self, _, _: &(), cx| {
                this.conditional_formatting = None;
                cx.notify();
            });
            self.conditional_formatting = Some((entity, subscription));
        }
        let (entity, _) = self.conditional_formatting.as_ref()?;
        let viewport = window.viewport_size();
        Some(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .relative()
                        .w(viewport.width)
                        .h(viewport.height)
                        .child(entity.clone()),
                ),
            )
            .into_any_element(),
        )
    }

    pub fn open_snapshot_prompt(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.result_sets.get(slot).is_none() {
            return;
        }
        self.snapshot_prompt = Some(SnapshotPrompt {
            slot,
            input: None,
            error: None,
            _subscription: None,
        });
        cx.notify();
    }

    fn close_snapshot_prompt(&mut self, cx: &mut Context<Self>) {
        self.snapshot_prompt = None;
        cx.notify();
    }

    /// Persist the prompt's slot as a named snapshot (bounded, spill-aware).
    fn save_snapshot_with_name(&mut self, name: String, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = name;
            if let Some(prompt) = &mut self.snapshot_prompt {
                prompt.error = Some(crate::browser_preview::unavailable(
                    &crate::localization::text("results.snapshots.browser_unavailable"),
                ));
            }
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use crate::result_snapshots::{SNAPSHOT_MAX_ROWS, SnapshotStore};
            let Some(prompt) = &self.snapshot_prompt else {
                return;
            };
            let slot = prompt.slot;
            let Some(rs) = self.result_sets.get(slot) else {
                return;
            };
            let columns = rs.columns.clone();
            let limit = SNAPSHOT_MAX_ROWS.saturating_add(1);
            // A windowed result is read from disk in the background first.
            let weak = cx.weak_entity();
            let rerun_name = name.clone();
            if self.defer_until_spill_prefixes(
                vec![(slot, limit)],
                crate::localization::format(
                    "results.snapshots.saving_reading_rows",
                    &[("row_count", &self.spill_read_count(slot, limit).to_string())],
                ),
                cx,
                move |cx| {
                    let _ =
                        weak.update(cx, |this, cx| this.save_snapshot_with_name(rerun_name, cx));
                },
            ) {
                return;
            }
            let mut rows: Vec<Vec<CellValue>> = match self.spill_rows_for_slot_capped(slot, limit) {
                Some(rows) => rows,
                None => self.resident_source_rows_for_slot(slot, cx),
            };
            let truncated = rows.len() > SNAPSHOT_MAX_ROWS;
            rows.truncate(SNAPSHOT_MAX_ROWS);
            let row_count = rows.len();
            let db = self.source_db_type();
            let envelope = crate::result_export::sqlly_export_envelope(db, &columns, &rows);
            let query = self.query_sql.clone().unwrap_or_default();
            let outcome = SnapshotStore::default_store().save_new(
                &name,
                &format!("{db:?}"),
                &query,
                row_count,
                truncated,
                chrono::Utc::now().timestamp_millis(),
                &envelope,
            );
            match outcome {
                Ok(_) => self.close_snapshot_prompt(cx),
                Err(error) => {
                    if let Some(prompt) = &mut self.snapshot_prompt {
                        prompt.error = Some(error);
                    }
                    cx.notify();
                }
            }
        }
    }

    /// The snapshot name prompt overlay; materializes its `InputState` at
    /// render time (renders have the `Window` event handlers lack).
    fn render_snapshot_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        self.snapshot_prompt.as_ref()?;
        if self
            .snapshot_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.input.is_none())
        {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "results.snapshots.name_placeholder",
                ))
            });
            let subscription =
                cx.subscribe(&input, |this: &mut Self, state, event: &InputEvent, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let name = state.read(cx).value().to_string();
                        this.save_snapshot_with_name(name, cx);
                    }
                });
            input.update(cx, |state, cx| state.focus(window, cx));
            if let Some(prompt) = &mut self.snapshot_prompt {
                prompt.input = Some(input);
                prompt._subscription = Some(subscription);
            }
        }
        let prompt = self.snapshot_prompt.as_ref()?;
        let input = prompt.input.clone()?;
        let error = prompt.error.clone();
        // Lifted to the window layer like every results-pane dialog: built
        // inline, the card is laid out against the (short) results pane and
        // painted beneath the grids that follow it, so it never shows.
        let dialog = crate::dialog::Dialog::new("results-snapshot-prompt", |this: &mut Self| {
            this.dialog_frames.get("results-snapshot-prompt", || {
                crate::dialog::DialogFrame::new_auto(Theme::scaled(420.0))
            })
        })
        .title(crate::localization::text(
            "results.snapshots.save_dialog_title",
        ))
        .on_close(|this, _window, cx| this.close_snapshot_prompt(cx))
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.snapshots.dialog_explanation",
                )),
        )
        .child(Input::new(&input))
        .children_body(error.map(|error| {
            div()
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::error())
                .child(error)
        }))
        .button(
            Button::new("snapshot-save")
                .primary()
                .label(crate::localization::text("results.snapshots.save_button"))
                .on_click(cx.listener(|this, _ev, _w, cx| {
                    let name = this
                        .snapshot_prompt
                        .as_ref()
                        .and_then(|p| p.input.as_ref())
                        .map(|input| input.read(cx).value().to_string())
                        .unwrap_or_default();
                    this.save_snapshot_with_name(name, cx);
                })),
        )
        .build(self, window, cx);
        Some(crate::dialog::window_layer(dialog, window))
    }

    /// The record-form overlay: one labeled row per column with the cell's
    /// display text, previous/next navigation, and per-value copy.
    fn render_row_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let form = self.row_form.as_ref()?;
        let s = Theme::scaled;
        let title = crate::localization::format(
            "results.row_form.overlay_row_position",
            &[
                ("row_number", &(form.row_index + 1).to_string()),
                ("total_rows", &form.row_count.to_string()),
            ],
        );
        let can_prev = form.row_index > 0;
        let can_next = form.row_index + 1 < form.row_count;
        let viewport = window.viewport_size();

        let mut rows_el = div()
            .id("row-form-fields")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for (i, column) in form.columns.iter().enumerate() {
            let cell = form.cells.get(i).cloned().unwrap_or(CellValue::Null);
            let is_null = matches!(cell, CellValue::Null);
            let display = crate::procedure_mode::cell_display(&cell);
            let copy_text = display.clone();
            rows_el = rows_el.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(s(10.0)))
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .w(px(s(180.0)))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.9)))
                                    .text_color(Theme::text())
                                    .truncate()
                                    .child(column.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.72)))
                                    .text_color(Theme::text_dim())
                                    .child(column.data_type.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .text_color(if is_null {
                                Theme::text_dim()
                            } else {
                                Theme::text()
                            })
                            .whitespace_normal()
                            .child(if is_null { "NULL".to_string() } else { display }),
                    )
                    .child(
                        Button::new(("row-form-copy", i))
                            .label(crate::localization::text(
                                "results.row_form.overlay_copy_value_button",
                            ))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |_this, _ev, _window, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    copy_text.clone(),
                                ));
                            })),
                    ),
            );
        }

        let dialog = crate::dialog::Dialog::new("results-row-form", |this: &mut Self| {
            this.dialog_frames.get("results-row-form", || {
                crate::dialog::DialogFrame::new(Theme::scaled(560.0), Theme::scaled(480.0))
            })
        })
        .title(title)
        // Read-only view: Escape / click outside always dismiss.
        .easy_close(true)
        .on_close(|this, _window, cx| this.close_row_form(cx))
        .detachable(|this, _window, cx| this.pop_out_row_form(cx))
        // The form owns its scroll region (one bordered row per column), so
        // the chrome body must not add padding or a second scroll container.
        .bare_body()
        .child(rows_el)
        .leading_button(
            Button::new("row-form-prev")
                .label("◀")
                .ghost()
                .xsmall()
                .disabled(!can_prev)
                .on_click(cx.listener(|this, _ev, _w, cx| this.row_form_step(-1, cx))),
        )
        .leading_button(
            Button::new("row-form-next")
                .label("▶")
                .ghost()
                .xsmall()
                .disabled(!can_next)
                .on_click(cx.listener(|this, _ev, _w, cx| this.row_form_step(1, cx))),
        )
        .build(self, window, cx);

        // Lift the whole overlay out of the results pane and anchor it to the
        // window's top-left, sized to the viewport, so the scrim covers the
        // entire window rather than being clipped to the bottom results region.
        Some(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .id("row-form-backdrop")
                        .relative()
                        .w(viewport.width)
                        .h(viewport.height)
                        .child(dialog),
                ),
            )
            .into_any_element(),
        )
    }

    fn filter_builder_row_el(
        &self,
        i: usize,
        row: &FilterRow,
        builder: &FilterBuilderState,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let s = Theme::scaled;
        let col_name = match row.column {
            ColumnTarget::AnyText => crate::localization::text("results.filters.any_text_column"),
            ColumnTarget::Index(idx) => {
                builder.column_names.get(idx).cloned().unwrap_or_else(|| {
                    crate::localization::format(
                        "results.filters.unnamed_column",
                        &[("column_number", &(idx + 1).to_string())],
                    )
                })
            }
        };
        let op_label = row.op.label();
        let show_a = row.op.needs_a();
        let show_b = row.op.needs_b();
        let enabled = row.enabled;
        let a_input = builder.inputs.get(i).and_then(|inp| inp.a.clone());
        let b_input = builder.inputs.get(i).and_then(|inp| inp.b.clone());
        let mut row_el = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .h(px(s(30.0)))
            .child(
                Checkbox::new(("filter-enabled", i))
                    .checked(enabled)
                    .on_click(cx.listener(move |this, _checked, _w, cx| {
                        this.filter_builder_toggle_enabled(i, cx);
                    })),
            )
            .child(
                Button::new(("filter-col", i))
                    .label(col_name)
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.filter_builder_cycle_column(i, cx);
                    })),
            )
            .child(
                Button::new(("filter-op", i))
                    .label(op_label)
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.filter_builder_cycle_op(i, cx);
                    })),
            );
        if show_a && let Some(input) = a_input {
            row_el = row_el.child(div().flex_1().min_w(px(s(80.0))).child(Input::new(&input)));
        }
        if show_b && let Some(input) = b_input {
            row_el = row_el.child(div().flex_1().min_w(px(s(80.0))).child(Input::new(&input)));
        }
        if !enabled {
            row_el = row_el.opacity(0.45);
        }
        row_el.child(
            Button::new(("filter-remove", i))
                .label("×")
                .ghost()
                .xsmall()
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.filter_builder_remove_row(i, cx);
                })),
        )
    }

    /// Visual filter-builder overlay. Materializes per-row `InputState`s at
    /// render time (needs a `Window`), matching the snapshot prompt.
    fn render_filter_builder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        self.filter_builder.as_ref()?;
        let s = Theme::scaled;
        // Read the saved-filter store before borrowing the builder mutably;
        // only needed when the dropdown has to be (re)built.
        let saved_store = self
            .filter_builder
            .as_ref()
            .is_some_and(|b| b.saved_select.is_none() && b.scope.is_some())
            .then(|| self.load_saved_filters());
        {
            let builder = self.filter_builder.as_mut()?;
            let leaf_count = builder.model.leaf_count();
            if builder.inputs.len() != leaf_count {
                builder.inputs.clear();
            }
            while builder.inputs.len() < leaf_count {
                builder
                    .inputs
                    .push(FilterBuilderInputs { a: None, b: None });
            }
            for i in 0..leaf_count {
                let (a_text, b_text, in_list) = builder
                    .model
                    .leaf_row_mut(i)
                    .map(|row| {
                        (
                            row.a.clone(),
                            row.b.clone(),
                            matches!(row.op, BuilderOp::In | BuilderOp::NotIn),
                        )
                    })
                    .unwrap_or_default();
                if builder.inputs[i].a.is_none() {
                    let placeholder = if in_list {
                        crate::localization::text("results.filters.list_value_placeholder")
                    } else {
                        String::new()
                    };
                    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                    input.update(cx, |state, cx| {
                        state.set_value(a_text.clone(), window, cx);
                    });
                    cx.subscribe(&input, move |this, state, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change)
                            && let Some(builder) = this.filter_builder.as_mut()
                            && let Some(row) = builder.model.leaf_row_mut(i)
                        {
                            row.a = state.read(cx).value().to_string();
                        }
                    })
                    .detach();
                    builder.inputs[i].a = Some(input);
                }
                if builder.inputs[i].b.is_none() {
                    let input = cx.new(|cx| InputState::new(window, cx));
                    input.update(cx, |state, cx| {
                        state.set_value(b_text.clone(), window, cx);
                    });
                    cx.subscribe(&input, move |this, state, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change)
                            && let Some(builder) = this.filter_builder.as_mut()
                            && let Some(row) = builder.model.leaf_row_mut(i)
                        {
                            row.b = state.read(cx).value().to_string();
                        }
                    })
                    .detach();
                    builder.inputs[i].b = Some(input);
                }
            }
            if builder.root_conjunction_select.is_none() {
                let idx = conjunction_select_index(builder.model.root.conjunction);
                let select =
                    cx.new(|cx| SelectState::new(filter_conjunction_items(), idx, window, cx));
                cx.subscribe_in(
                    &select,
                    window,
                    |this, select, event: &SelectEvent<Vec<String>>, _w, cx| {
                        let SelectEvent::Confirm(value) = event;
                        if value.is_none() {
                            return;
                        }
                        let row = select.read(cx).selected_index(cx).map(|ix| ix.row);
                        if let Some(builder) = this.filter_builder.as_mut() {
                            builder.model.root.conjunction = conjunction_from_select(row);
                        }
                        cx.notify();
                    },
                )
                .detach();
                builder.root_conjunction_select = Some(select);
            }
            // One conjunction dropdown per nested group, at every depth.
            let groups: Vec<(Vec<usize>, Conjunction)> = builder
                .model
                .layout()
                .into_iter()
                .filter_map(|item| match item {
                    LayoutItem::GroupStart { path } => {
                        let conjunction = builder.model.group_at(&path)?.conjunction;
                        Some((path, conjunction))
                    }
                    _ => None,
                })
                .collect();
            builder
                .group_conjunction_selects
                .retain(|path, _| groups.iter().any(|(p, _)| p == path));
            for (path, conjunction) in groups {
                if builder.group_conjunction_selects.contains_key(&path) {
                    continue;
                }
                let idx = conjunction_select_index(conjunction);
                let select =
                    cx.new(|cx| SelectState::new(filter_conjunction_items(), idx, window, cx));
                let group_path = path.clone();
                cx.subscribe_in(
                    &select,
                    window,
                    move |this, select, event: &SelectEvent<Vec<String>>, _w, cx| {
                        let SelectEvent::Confirm(value) = event;
                        if value.is_none() {
                            return;
                        }
                        let row = select.read(cx).selected_index(cx).map(|ix| ix.row);
                        if let Some(builder) = this.filter_builder.as_mut() {
                            builder
                                .model
                                .set_group_conjunction(&group_path, conjunction_from_select(row));
                        }
                        cx.notify();
                    },
                )
                .detach();
                builder.group_conjunction_selects.insert(path, select);
            }
            if builder.expression_input.is_none() {
                let seed = builder.expression_seed.take().unwrap_or_default();
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(crate::localization::input_placeholder(
                            window,
                            cx,
                            "results.filters.expression_placeholder",
                        ))
                        .default_value(seed)
                });
                cx.subscribe_in(
                    &input,
                    window,
                    |this, _state, event: &InputEvent, _w, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            this.filter_builder_apply_expression(cx);
                        }
                    },
                )
                .detach();
                builder.expression_input = Some(input);
            }
            if builder.saved_name_input.is_none() {
                let input = cx.new(|cx| {
                    InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                        window,
                        cx,
                        "results.filters.saved_name_placeholder",
                    ))
                });
                builder.saved_name_input = Some(input);
            }
            if builder.saved_select.is_none()
                && let Some(scope) = builder.scope.clone()
            {
                match saved_store {
                    Some(Err(message)) => builder.saved_error = Some(message),
                    Some(Ok(store)) => {
                        let options: Vec<SavedFilterOption> = store
                            .filters_for(&scope.key)
                            .iter()
                            .map(SavedFilterOption::new)
                            .collect();
                        if !options.is_empty() {
                            let selected = builder
                                .saved_chosen
                                .as_ref()
                                .and_then(|chosen| {
                                    options.iter().position(|option| option.name == *chosen)
                                })
                                .map(gpui_component::IndexPath::new);
                            let select =
                                cx.new(|cx| SelectState::new(options, selected, window, cx));
                            cx.subscribe_in(
                                &select,
                                window,
                                |this, _, event: &SelectEvent<Vec<SavedFilterOption>>, _w, cx| {
                                    let SelectEvent::Confirm(value) = event;
                                    let Some(name) = value.clone() else {
                                        return;
                                    };
                                    if let Some(builder) = this.filter_builder.as_mut() {
                                        builder.saved_chosen = Some(name);
                                    }
                                    cx.notify();
                                },
                            )
                            .detach();
                            builder.saved_select = Some(select);
                        }
                    }
                    None => {}
                }
            }
        }
        let builder = self.filter_builder.as_ref()?;
        let spill_backed = builder.spill_backed;
        let can_apply_query = self.editor.is_some();
        let mut rows_el = div()
            .id("filter-builder-rows")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .px(px(s(12.0)))
            .py(px(s(8.0)));
        if spill_backed {
            rows_el = rows_el.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.filters.windowed_apply_query_hint",
                    )),
            );
        }
        // Render the tree at every depth from its layout: nested groups are
        // bordered, indented blocks with their own conjunction dropdown, and
        // each row carries the leaf index the model edits by.
        let mut open_groups: Vec<(Vec<usize>, usize, gpui::Div)> = Vec::new();
        let mut group_counter = 0usize;
        for item in builder.model.layout() {
            match item {
                LayoutItem::Row { leaf, row } => {
                    let row_el = self.filter_builder_row_el(leaf, &row, builder, cx);
                    match open_groups.pop() {
                        Some((path, id, group_el)) => {
                            open_groups.push((path, id, group_el.child(row_el)));
                        }
                        None => rows_el = rows_el.child(row_el),
                    }
                }
                LayoutItem::GroupStart { path } => {
                    let mut group_el = div()
                        .flex()
                        .flex_col()
                        .gap(px(s(4.0)))
                        .pl(px(s(16.)))
                        .border_l_1()
                        .border_color(Theme::border());
                    if let Some(select) = builder.group_conjunction_selects.get(&path) {
                        group_el = group_el.child(div().w(px(s(260.0))).child(Select::new(select)));
                    }
                    open_groups.push((path, group_counter, group_el));
                    group_counter += 1;
                }
                LayoutItem::GroupEnd { .. } => {
                    let Some((path, id, group_el)) = open_groups.pop() else {
                        continue;
                    };
                    let add_path = path.clone();
                    let remove_path = path;
                    let group_el = group_el.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(
                                Button::new(("filter-group-add", id))
                                    .label(crate::localization::text(
                                        "results.filters.add_condition_in_group_button",
                                    ))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                                        this.filter_builder_add_row_in_group(&add_path, cx);
                                    })),
                            )
                            .child(
                                Button::new(("filter-group-remove", id))
                                    .label("×")
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                                        this.filter_builder_remove_group(&remove_path, cx);
                                    })),
                            ),
                    );
                    match open_groups.pop() {
                        Some((parent_path, parent_id, parent_el)) => {
                            open_groups.push((parent_path, parent_id, parent_el.child(group_el)));
                        }
                        None => rows_el = rows_el.child(group_el),
                    }
                }
            }
        }
        // Date comparisons read cell text as YYYY-MM-DD; on a column shown
        // in another format they would silently match nothing.
        let date_warning_columns: Vec<String> = {
            let mut names: Vec<String> = builder
                .model
                .flat_rows()
                .into_iter()
                .filter(|row| row.enabled && row.op.is_number())
                .filter_map(|row| match row.column {
                    ColumnTarget::Index(i) if builder.non_iso_date_columns.contains(&i) => {
                        builder.column_names.get(i).cloned()
                    }
                    _ => None,
                })
                .collect();
            let mut seen = HashSet::new();
            names.retain(|name| seen.insert(name.clone()));
            names
        };
        if !date_warning_columns.is_empty() {
            rows_el = rows_el.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::warning())
                    .child(crate::localization::format(
                        "results.filters.non_iso_date_warning",
                        &[("column_names", &date_warning_columns.join(", "))],
                    )),
            );
        }

        let root_select = builder.root_conjunction_select.clone();
        let expression_input = builder.expression_input.clone();
        let expression_error = builder.expression_error.clone();
        let saved_select = builder.saved_select.clone();
        let saved_name_input = builder.saved_name_input.clone();
        let has_scope = builder.scope.is_some();
        let allows_default = builder
            .scope
            .as_ref()
            .is_some_and(|scope| scope.allows_default);
        let saved_error = builder.saved_error.clone();

        let expression_row = expression_input.map(|input| {
            let mut col = div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .px(px(s(12.0)))
                .py(px(s(4.0)));
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(div().flex_1().child(Input::new(&input)))
                    .child(
                        Button::new("filter-expr-apply")
                            .label(crate::localization::text(
                                "results.filters.apply_expression_button",
                            ))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                this.filter_builder_apply_expression(cx);
                            })),
                    ),
            );
            if let Some(error) = &expression_error {
                col = col.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::error())
                        .child(error.clone()),
                );
            }
            col
        });

        let saved_row = if !has_scope {
            None
        } else {
            let mut line = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .px(px(s(12.0)))
                .py(px(s(4.0)))
                .flex_wrap();
            if let Some(select) = &saved_select {
                line = line.child(div().w(px(s(170.0))).child(Select::new(select)));
                line = line.child(
                    Button::new("filter-saved-apply")
                        .label(crate::localization::text(
                            "results.filters.apply_saved_button",
                        ))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _ev, _w, cx| {
                            this.filter_builder_apply_saved(cx);
                        })),
                );
                if allows_default {
                    line = line.child(
                        Button::new("filter-saved-default")
                            .label(crate::localization::text(
                                "results.filters.toggle_default_button",
                            ))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                this.filter_builder_set_default_saved(cx);
                            })),
                    );
                }
                line = line.child(
                    Button::new("filter-saved-rename")
                        .label(crate::localization::text(
                            "results.filters.rename_saved_button",
                        ))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _ev, _w, cx| {
                            this.filter_builder_rename_saved(cx);
                        })),
                );
                line = line.child(
                    Button::new("filter-saved-delete")
                        .label(crate::localization::text(
                            "results.filters.delete_saved_button",
                        ))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _ev, _w, cx| {
                            this.filter_builder_delete_saved(cx);
                        })),
                );
            }
            if let Some(name_input) = &saved_name_input {
                line = line.child(div().w(px(s(130.0))).child(Input::new(name_input)));
                line = line.child(
                    Button::new("filter-saved-save")
                        .label(crate::localization::text(
                            "results.filters.save_named_button",
                        ))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _ev, _w, cx| {
                            this.filter_builder_save_named(cx);
                        })),
                );
            }
            let mut block = div().flex().flex_col().child(line);
            if let Some(message) = saved_error {
                block = block.child(
                    div()
                        .px(px(s(12.0)))
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::warning())
                        .child(message),
                );
            }
            Some(block)
        };

        // The builder is mounted in the results pane but must live on the
        // window layer: inline, it was the pane's first child, so the tab
        // bar and grids painted over it (and took its clicks) and the card
        // was centered on the short pane instead of the window.
        let dialog = crate::dialog::Dialog::new("results-filter-builder", |this: &mut Self| {
            this.dialog_frames.get("results-filter-builder", || {
                crate::dialog::DialogFrame::new_auto(Theme::scaled(640.0))
            })
        })
        .title(crate::localization::text("results.filters.dialog_title"))
        .on_close(|this, _window, cx| this.close_filter_builder(cx))
        // The builder owns its scroll region (rows_el) with its own row
        // padding, so the chrome body must not add a second scroll/pad.
        .bare_body()
        .children_body(root_select.map(|select| {
            div()
                .px(px(s(12.0)))
                .py(px(s(6.0)))
                .w(px(s(280.0)))
                .child(Select::new(&select))
        }))
        .children_body(expression_row)
        .children_body(saved_row)
        .child(rows_el)
        .button(
            Button::new("filter-add")
                .label(crate::localization::text(
                    "results.filters.add_condition_button",
                ))
                .ghost()
                .xsmall()
                .on_click(cx.listener(|this, _ev, _w, cx| this.filter_builder_add_row(cx))),
        )
        .button(
            Button::new("filter-add-group")
                .label(crate::localization::text(
                    "results.filters.add_group_button",
                ))
                .ghost()
                .xsmall()
                .on_click(cx.listener(|this, _ev, _w, cx| this.filter_builder_add_group(cx))),
        )
        // Clear sits in the footer's far-left utility slot, apart from the
        // apply actions, so it is not mistaken for (or clicked as) Apply.
        .leading_button(
            Button::new("filter-clear")
                .label(crate::localization::text("results.filters.clear_button"))
                .ghost()
                .xsmall()
                .on_click(cx.listener(|this, _ev, _w, cx| this.filter_builder_clear_rows(cx))),
        )
        .button(
            Button::new("filter-apply-query")
                .label(crate::localization::text(
                    "results.filters.apply_to_query_button",
                ))
                .ghost()
                .xsmall()
                .disabled(!can_apply_query)
                .on_click(cx.listener(|this, _ev, _w, cx| this.apply_filter_builder_to_query(cx))),
        )
        .button(
            Button::new("filter-apply")
                .label(crate::localization::text(
                    "results.filters.apply_grid_button",
                ))
                .primary()
                .xsmall()
                .disabled(spill_backed)
                .on_click(cx.listener(|this, _ev, _w, cx| this.apply_filter_builder(cx))),
        )
        .build(self, window, cx);
        Some(crate::dialog::window_layer(dialog, window))
    }

    /// Return the rows for a result-set slot in display order (after
    /// sorting and filtering applied by the grid). Falls back to the
    /// source row order when no grid view exists for the slot.
    ///
    /// Spill-backed slots page the complete flushed prefix. User copy/export
    /// must use [`Self::visible_rows_for_slot_capped`] so they cannot load an
    /// unbounded spill file.
    #[cfg(test)]
    pub fn visible_rows_for_slot(&self, slot: usize, cx: &gpui::App) -> Vec<Vec<CellValue>> {
        let Some(rs) = self.result_sets.get(slot) else {
            return Vec::new();
        };
        // Spill-backed sets keep `rs.rows` empty; file order IS the visible
        // order because sort/filter are disabled while windowed.
        if let Some(Some(backing)) = self.spill_backings.get(slot) {
            return backing.all_rows();
        }
        match self.grid_views.get(slot).and_then(|g| g.as_ref()) {
            Some(grid_entity) => {
                let state = grid_entity.read(cx).state.read(cx);
                let indices = &state.display_indices;
                indices
                    .iter()
                    .filter_map(|&src_idx| {
                        if !rs.rows.is_empty() {
                            rs.rows.get(self.source_index(slot, src_idx)).cloned()
                        } else {
                            state
                                .data
                                .rows
                                .get(src_idx)
                                .map(|row| row.iter().map(grid_cell_to_core).collect::<Vec<_>>())
                        }
                    })
                    .collect()
            }
            None => {
                if rs.rows.is_empty() {
                    self.grid_source_rows(slot, cx).unwrap_or_default()
                } else {
                    rs.rows.clone()
                }
            }
        }
    }

    /// Visible rows for `slot`, never reading more than `cap` rows from a
    /// spill backing. In-memory grids are unchanged (already resident). The
    /// `truncated` flag is true only when a spill file holds more than `cap`
    /// rows.
    pub fn visible_rows_for_slot_capped(
        &self,
        slot: usize,
        cap: usize,
        cx: &gpui::App,
    ) -> (Vec<Vec<CellValue>>, bool) {
        let Some(rs) = self.result_sets.get(slot) else {
            return (Vec::new(), false);
        };
        if let Some((rows, truncated)) = self.spill_rows_for_export(slot, cap) {
            return (rows, truncated);
        }
        let rows =
            match self.grid_views.get(slot).and_then(|g| g.as_ref()) {
                Some(grid_entity) => {
                    let state = grid_entity.read(cx).state.read(cx);
                    let indices = &state.display_indices;
                    indices
                        .iter()
                        .filter_map(|&src_idx| {
                            if !rs.rows.is_empty() {
                                rs.rows.get(self.source_index(slot, src_idx)).cloned()
                            } else {
                                state.data.rows.get(src_idx).map(|row| {
                                    row.iter().map(grid_cell_to_core).collect::<Vec<_>>()
                                })
                            }
                        })
                        .collect()
                }
                None => {
                    if rs.rows.is_empty() {
                        self.grid_source_rows(slot, cx).unwrap_or_default()
                    } else {
                        rs.rows.clone()
                    }
                }
            };
        (rows, false)
    }

    /// The complete rows of a spill-backed slot, paged from the file and
    /// clamped to the backing total (the flushed prefix for a provisional
    /// backing). `None` when the slot is not spill-backed. User export uses
    /// [`Self::spill_rows_for_export`] instead so it never loads the whole file.
    #[cfg(test)]
    pub fn spill_rows_for_slot(&self, slot: usize) -> Option<Vec<Vec<CellValue>>> {
        match self.spill_backings.get(slot) {
            Some(Some(backing)) => Some(backing.all_rows()),
            _ => None,
        }
    }

    /// Return a bounded prefix for a spill-backed slot. Unlike
    /// [`Self::spill_rows_for_slot`], this never reads the entire file.
    pub fn spill_rows_for_slot_capped(
        &self,
        slot: usize,
        limit: usize,
    ) -> Option<Vec<Vec<CellValue>>> {
        self.spill_rows_for_export(slot, limit)
            .map(|(rows, _truncated)| rows)
    }

    /// Spill-backed prefix plus whether the file holds more than `limit`
    /// rows. `None` when the slot is not spill-backed.
    pub fn spill_rows_for_export(
        &self,
        slot: usize,
        limit: usize,
    ) -> Option<(Vec<Vec<CellValue>>, bool)> {
        match self.spill_backings.get(slot) {
            Some(Some(backing)) => {
                // A prefix read off the UI thread for this very request
                // (see `defer_until_spill_prefixes`) is used as is; only a
                // caller that did not ask ahead reads the file here.
                let rows = self
                    .take_spill_prefix(slot, limit, backing.total)
                    .unwrap_or_else(|| backing.first_rows(limit));
                Some((rows, backing.total > limit))
            }
            _ => None,
        }
    }

    fn take_spill_prefix(
        &self,
        slot: usize,
        limit: usize,
        total: usize,
    ) -> Option<Vec<Vec<CellValue>>> {
        let mut prefixes = self.spill_prefixes.borrow_mut();
        let at = prefixes.iter().position(|p| {
            p.slot == slot && p.limit == limit && p.total == total && p.epoch == self.result_epoch
        })?;
        Some(prefixes.swap_remove(at).rows)
    }

    /// Whether reading the first `limit` rows of `slot` would hit the disk:
    /// the slot is windowed and no background-read prefix is waiting.
    /// [`Self::defer_until_spill_prefixes`] for an export of the active
    /// result set (the first [`result_export::EXPORT_MAX_ROWS`] rows):
    /// `rerun` repeats the export in `window` once the rows are in memory.
    pub(crate) fn defer_active_export(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        rerun: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        let slot = self.active_tab;
        let limit = result_export::EXPORT_MAX_ROWS;
        let caption = crate::localization::format(
            "results.grid_menu.export.preparing_spill_rows",
            &[("row_count", &self.spill_read_count(slot, limit).to_string())],
        );
        let weak = cx.weak_entity();
        let handle = window.window_handle();
        self.defer_until_spill_prefixes(vec![(slot, limit)], caption, cx, move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                let _ = weak.update(cx, |this, cx| rerun(this, window, cx));
            });
        })
    }

    /// The result tab currently shown.
    pub(crate) fn active_slot(&self) -> usize {
        self.active_tab
    }

    /// Whether there is a result set to export (a cheap check — no rows are
    /// read or rendered).
    pub(crate) fn has_active_result_set(&self) -> bool {
        self.active_result_set().is_some()
    }

    /// Windowed slots whose first `limit` rows are not waiting in memory, as
    /// `(slot, limit)` wants for [`Self::defer_until_spill_prefixes`] — for
    /// actions that read every result set (moving a tab to another window).
    pub(crate) fn spill_prefixes_wanted_for_all(&self, limit: usize) -> Vec<(usize, usize)> {
        (0..self.result_sets.len())
            .filter(|&slot| self.spill_prefix_missing(slot, limit))
            .map(|slot| (slot, limit))
            .collect()
    }

    /// Rows a spill prefix of `limit` would read from `slot` (for captions).
    pub(crate) fn spill_read_count(&self, slot: usize, limit: usize) -> usize {
        self.spill_scan_total(slot)
            .map_or(0, |total| total.min(limit))
    }

    fn spill_prefix_missing(&self, slot: usize, limit: usize) -> bool {
        let Some(Some(backing)) = self.spill_backings.get(slot) else {
            return false;
        };
        !self.spill_prefixes.borrow().iter().any(|p| {
            p.slot == slot
                && p.limit == limit
                && p.total == backing.total
                && p.epoch == self.result_epoch
        })
    }

    /// Make the spill reads an action is about to do come from memory. For
    /// every `(slot, limit)` it will pass to `spill_rows_for_export` whose
    /// rows are not already waiting, the rows are read on the background
    /// executor while the grid caption shows `caption`; then `rerun` runs
    /// (deferred, outside every entity update) and should repeat the action,
    /// which now finds the rows in memory. Returns `false` — nothing to wait
    /// for, carry on — when no read is needed. A newer background read
    /// supersedes this one and a replaced result drops it (generation and
    /// result-epoch tokens); a file that cannot be read is reported in
    /// Messages and the action does not run.
    pub(crate) fn defer_until_spill_prefixes(
        &mut self,
        wants: Vec<(usize, usize)>,
        caption: String,
        cx: &mut Context<Self>,
        rerun: impl FnOnce(&mut App) + 'static,
    ) -> bool {
        let reads: Vec<(usize, usize, usize, String)> = wants
            .into_iter()
            .filter(|&(slot, limit)| self.spill_prefix_missing(slot, limit))
            .filter_map(|(slot, limit)| {
                let (path, total) = self.spill_scan_source(slot)?;
                Some((slot, limit, total, path))
            })
            .collect();
        let Some(&(first_slot, ..)) = reads.first() else {
            return false;
        };
        self.background_read_generation = self.background_read_generation.wrapping_add(1);
        let generation = self.background_read_generation;
        let epoch = self.result_epoch;
        let task = cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    let mut read = Vec::with_capacity(reads.len());
                    for (slot, limit, total, path) in reads {
                        // The same page `SpillBacking::first_rows` reads,
                        // through a private reader.
                        let rows =
                            crate::spill_pager::read_spill_rows(path, 0, limit.min(total)).await;
                        read.push((slot, limit, total, rows));
                    }
                    read
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.background_read.as_ref().map(|r| r.generation) != Some(generation) {
                    return;
                }
                this.background_read = None;
                cx.notify();
                if epoch != this.result_epoch {
                    return;
                }
                let mut prefixes = Vec::with_capacity(read.len());
                for (slot, limit, total, rows) in read {
                    let Some(rows) = rows else {
                        this.messages.push(crate::localization::text(
                            "results.export.spill_read_failed",
                        ));
                        this.show_messages = true;
                        return;
                    };
                    prefixes.push(SpillPrefix {
                        slot,
                        limit,
                        total,
                        epoch,
                        rows,
                    });
                }
                {
                    let mut waiting = this.spill_prefixes.borrow_mut();
                    // One waiting prefix per slot keeps memory bounded.
                    waiting.retain(|old| !prefixes.iter().any(|new| new.slot == old.slot));
                    waiting.extend(prefixes);
                }
                cx.defer(rerun);
            });
        });
        self.background_read = Some(BackgroundRead {
            slot: first_slot,
            generation,
            caption,
            _task: task,
        });
        cx.notify();
        true
    }

    /// Handle a scroll-wheel event in the stacked container. When the
    /// configured gesture modifier is held and the accumulated scroll
    /// exceeds the activation threshold, snap to the next/previous card
    /// instead of free-scrolling.
    pub fn handle_stacked_scroll(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let prefs = &self.gesture_prefs;
        let mod_match = prefs.result_container_scroll_modifier.matches(
            event.modifiers.shift,
            event.modifiers.platform,
            event.modifiers.function,
            event.modifiers.control,
            event.modifiers.alt,
        );
        if !mod_match {
            self.scroll_accumulator.reset();
            return;
        }
        let delta = event.delta.pixel_delta(px(20.0));
        self.scroll_accumulator
            .add(f32::from(delta.x), f32::from(delta.y));
        if self.scroll_accumulator.magnitude() > gesture_interpreter::ACTIVATION_THRESHOLD {
            let dir =
                ScrollDirection::from_delta(self.scroll_accumulator.x, self.scroll_accumulator.y);
            self.snap_to_card(dir, cx);
            self.scroll_accumulator.reset();
        }
    }

    /// Snap the stacked container to the next or previous card by scrolling
    /// the container by one card height.
    fn snap_to_card(&mut self, direction: ScrollDirection, cx: &mut Context<Self>) {
        let offset = self.stacked_scroll_handle.offset();
        let card_height = px(300.0);
        let delta = match direction {
            ScrollDirection::Down | ScrollDirection::Right => card_height,
            ScrollDirection::Up | ScrollDirection::Left => -card_height,
        };
        let new_y = px(f32::from(offset.y) + f32::from(delta)).max(px(0.0));
        self.stacked_scroll_handle
            .set_offset(gpui::Point::new(offset.x, new_y));
        cx.notify();
    }

    /// Handle a scroll-wheel event over the tabbed content area. When
    /// two-finger scroll is enabled and the grid is at its scroll edge,
    /// switch to the next/previous result set tab. The [`EdgeSwitchGate`]
    /// caps this at one switch per gesture — a flick's momentum stream
    /// must go quiet before another event can switch again.
    pub fn handle_tabbed_scroll(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        if !self.gesture_prefs.results_two_finger_scroll_enabled {
            return;
        }
        if self.result_sets.len() <= 1 {
            return;
        }
        if !event.delta.precise() {
            return;
        }
        let delta_y = f32::from(event.delta.pixel_delta(px(20.0)).y);
        let dir = if delta_y > 0.0 {
            ScrollDirection::Down
        } else {
            ScrollDirection::Up
        };
        // A tab whose grid hasn't materialized yet is never "at the edge":
        // right after a switch the new grid may not exist for a frame, and
        // treating that as an edge would chain into the next switch.
        let grid = self
            .grid_views
            .get(self.active_tab)
            .and_then(|v| v.as_ref());
        let at_edge = grid.is_some_and(|g| {
            let s = g.read(cx).state.read(cx);
            match dir {
                // The datatable drives its gpui `ScrollHandle` from its own
                // scroll physics with positive-down offsets, and never
                // attaches it to an element — so `max_offset()` is always
                // zero and useless here. Ask its viewport math instead.
                ScrollDirection::Up => f32::from(s.scroll_handle.offset().y) <= 0.0,
                ScrollDirection::Down => {
                    let (_, last) = s.visible_row_range();
                    last >= s.display_row_count()
                }
                _ => false,
            }
        });
        if self
            .tab_switch_gate
            .admit(dir, at_edge, delta_y, Instant::now())
        {
            self.switch_result_tab(dir, cx);
        }
    }

    /// Switch to the next or previous result set tab.
    fn switch_result_tab(&mut self, direction: ScrollDirection, cx: &mut Context<Self>) {
        let count = self.result_sets.len();
        if count <= 1 {
            return;
        }
        // Clamp at the ends rather than wrap: scrolling past the last
        // result set should stop there, not cycle back to the first.
        let target = match direction {
            ScrollDirection::Down | ScrollDirection::Right => (self.active_tab + 1).min(count - 1),
            ScrollDirection::Up | ScrollDirection::Left => self.active_tab.saturating_sub(1),
        };
        if target == self.active_tab {
            return;
        }
        self.active_tab = target;
        self.ensure_grid_for_active_tab(cx);
        if self.track_difference {
            self.diff_key_columns.clear();
            self.recompute_detailed_diff(cx);
        }
        cx.emit(ResultsViewEvent::ActiveResultChanged);
        self.refresh_cell_detail(cx);
        cx.notify();
    }

    /// Toggle the per-query timing breakdown in the status bar (Results →
    /// Performance ▸ "Show detailed query timing"). Only re-renders when the
    /// value actually changes.
    pub fn set_show_detailed_timing(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_detailed_timing != show {
            self.show_detailed_timing = show;
            cx.notify();
        }
    }

    /// Reload Results → Performance paging prefs after the preferences dialog
    /// saves. The scroll path reads `self.perf_prefs`, not the disk file.
    pub fn reload_performance_prefs(&mut self, cx: &mut Context<Self>) {
        self.perf_prefs = crate::preferences::PerformancePreferences::load();
        // Scroll pacing is read by the grid itself; push it into the grids
        // already on screen so the preference applies without a re-run.
        let interval = self.perf_prefs.scroll_repaint_interval();
        for view in self.grid_views.iter().flatten() {
            let state = view.read(cx).state.clone();
            state.update(cx, |s, _cx| s.config.scroll_repaint_interval = interval);
        }
        self.gesture_prefs = crate::gesture_prefs::load_default_preferences();
        self.run_history_limit = self.perf_prefs.result_run_history_runs();
        self.run_history_budget_bytes = self.perf_prefs.result_memory_budget_bytes();
        self.run_history_enabled = self.perf_prefs.result_run_history_enabled();
        self.run_history_capture = self.perf_prefs.result_run_history_capture();
    }

    pub fn set_formatting(&mut self, fmt: ResultFormattingPreferences, cx: &mut Context<Self>) {
        self.formatting = Some(fmt);
        self.invalidate_selection_stats();
        for i in 0..self.result_sets.len() {
            if self.grid_views.get(i).is_some_and(|g| g.is_some()) {
                self.rebuild_slot_from_source(i, cx);
            }
        }
        self.apply_density_to_live_grids(cx);
        cx.notify();
    }

    pub fn apply_density_to_live_grids(&self, cx: &mut App) {
        let auxiliary = [
            &self.diff_grid_previous,
            &self.diff_grid_current,
            &self.diff_grid_diff,
            &self.detailed_diff_grid,
            &self.result_profile_grid,
        ];
        let views: Vec<Entity<SqllyDataTable>> = self
            .grid_views
            .iter()
            .flatten()
            .chain(self.transposed.iter().flatten())
            .chain(auxiliary.into_iter().flat_map(|g| g.as_ref()))
            .cloned()
            .collect();
        // Columns sized by a `width=…dp` directive follow the font: a zoom
        // or font-size change keeps the same characters in view.
        let dp_widths: HashMap<gpui::EntityId, Vec<Option<u32>>> = self
            .grid_views
            .iter()
            .enumerate()
            .filter_map(|(slot, view)| {
                let view = view.as_ref()?;
                let columns = &self.result_sets.get(slot)?.columns;
                let dps: Vec<Option<u32>> = columns
                    .iter()
                    .map(|c| crate::query_format::width_dp_for_column(&c.name, &self.format_hints))
                    .collect();
                dps.iter()
                    .any(Option::is_some)
                    .then(|| (view.entity_id(), dps))
            })
            .collect();
        for view in views {
            let state = view.read(cx).state.clone();
            let density = self.formatting_or_load().density;
            let dps = dp_widths.get(&view.entity_id());
            state.update(cx, |s, cx| {
                let old_font = s.font_size;
                apply_grid_density(s, density, self.grid_zoom, cx);
                if let Some(dps) = dps
                    && (s.font_size - old_font).abs() > f32::EPSILON
                {
                    rescale_dp_widths(s, dps, old_font);
                }
                cx.notify();
            });
        }
    }

    /// Rebuild and apply grid themes for all existing grid views. Called when
    /// the app theme changes so grids pick up the new color palette.
    pub fn refresh_grid_themes(&mut self, cx: &mut Context<Self>) {
        let theme = build_grid_theme();
        let auxiliary = [
            &self.diff_grid_previous,
            &self.diff_grid_current,
            &self.diff_grid_diff,
            &self.detailed_diff_grid,
            &self.result_profile_grid,
        ];
        let views: Vec<Entity<SqllyDataTable>> = self
            .grid_views
            .iter()
            .flatten()
            .chain(self.transposed.iter().flatten())
            .chain(auxiliary.into_iter().flat_map(|g| g.as_ref()))
            .cloned()
            .collect();
        for view in views {
            let state = view.read(cx).state.clone();
            state.update(cx, |s, cx| {
                s.theme = theme.clone();
                cx.notify();
            });
        }
        // "Color cells by data type" bakes theme colors into the grid
        // config's rules, so a theme switch has to rebuild the live grids or
        // they keep the previous theme's tints.
        if self.formatting_or_load().color_cells_by_type {
            for i in 0..self.result_sets.len() {
                if self.grid_views.get(i).is_some_and(|g| g.is_some()) {
                    self.rebuild_slot_from_source(i, cx);
                }
            }
        }
        cx.notify();
    }

    /// Whether result set `slot` has grouping, filters, or a sort applied —
    /// drives the enabled state of the stacked card header's clear buttons.
    fn slot_toolbar_state(&self, slot: usize, cx: &App) -> (bool, bool, bool) {
        let mut has_grouping = false;
        let mut has_filters = self.client_filter.get(slot).is_some_and(Option::is_some);
        let mut has_sort = self
            .sort_specs
            .get(slot)
            .is_some_and(|spec| !spec.keys.is_empty());
        if let Some(Some(view)) = self.grid_views.get(slot) {
            let state = view.read(cx).state.read(cx);
            has_grouping = state.grouped_column().is_some();
            has_filters |= state
                .filters
                .iter()
                .any(sqlly_datatable::ColumnFilter::is_active);
            has_sort |= state.sort.is_some();
        }
        (has_grouping, has_filters, has_sort)
    }

    fn clear_slot_grouping(&mut self, slot: usize, cx: &mut Context<Self>) {
        if let Some(Some(view)) = self.grid_views.get(slot) {
            let state = view.read(cx).state.clone();
            state.update(cx, |state, cx| {
                state.set_grouped_column(None);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn clear_slot_filters(&mut self, slot: usize, cx: &mut Context<Self>) {
        if let Some(Some(view)) = self.grid_views.get(slot) {
            let state = view.read(cx).state.clone();
            state.update(cx, |state, cx| {
                state.filters.fill(sqlly_datatable::ColumnFilter::default());
                state.filter_panel = None;
                state.recompute();
                cx.notify();
            });
        }
        self.applied_filter_models.remove(&slot);
        if let Some(filter) = self.client_filter.get_mut(slot)
            && filter.take().is_some()
        {
            self.rebuild_slot_from_source(slot, cx);
        }
        cx.notify();
    }

    fn clear_grid_sort(&mut self, cx: &mut Context<Self>) {
        let slots = self.grid_views.len().max(self.sort_specs.len());
        for slot in 0..slots {
            self.clear_slot_sort(slot, cx);
        }
    }

    fn clear_slot_sort(&mut self, slot: usize, cx: &mut Context<Self>) {
        let rebuild = self
            .sort_specs
            .get(slot)
            .is_some_and(|spec| !spec.secondaries().is_empty());
        if let Some(Some(view)) = self.grid_views.get(slot) {
            let state = view.read(cx).state.clone();
            state.update(cx, |state, cx| {
                state.sort = None;
                state.recompute();
                cx.notify();
            });
        }
        if let Some(spec) = self.sort_specs.get_mut(slot) {
            spec.keys.clear();
        }
        if rebuild {
            self.rebuild_slot_from_source(slot, cx);
        }
        if slot < self.grid_views.len() {
            self.note_possible_layout_change(slot, cx);
        }
        cx.notify();
    }

    pub fn sort_active_column(&mut self, descending: bool, cx: &mut Context<Self>) {
        let slot = self.active_tab_index();
        let Some(state) = self.active_grid_state(cx) else {
            return;
        };
        let selection = state.read(cx).selection.clone();
        let column = match selection {
            Selection::Cell(_, column) | Selection::Column(column) => Some(column),
            Selection::CellRange(_, first, _, second) => Some(first.min(second)),
            Selection::Columns(columns) => columns.first().copied(),
            Selection::Cells(cells) => cells.first().map(|(_, column)| *column),
            Selection::None
            | Selection::Row(_)
            | Selection::RowRange(_, _)
            | Selection::Rows(_) => None,
        };
        let Some(column) = column else {
            return;
        };
        let direction = if descending {
            sqlly_datatable::SortDirection::Descending
        } else {
            sqlly_datatable::SortDirection::Ascending
        };
        state.update(cx, |state, cx| {
            state.sort = Some((column, direction));
            state.recompute();
            cx.notify();
        });
        self.pad_sort_specs();
        self.sort_specs[slot].set_primary(Some((column, direction)));
        self.note_possible_layout_change(slot, cx);
        cx.notify();
    }

    pub fn add_to_sort(&mut self, slot: usize, column: usize, cx: &mut Context<Self>) {
        self.pad_sort_specs();
        let Some(Some(view)) = self.grid_views.get(slot).cloned() else {
            return;
        };
        let state = view.read(cx).state.clone();
        let grid_sort = state.read(cx).sort;
        self.sort_specs[slot].set_primary(grid_sort);
        if self.sort_specs[slot].primary().is_none() {
            state.update(cx, |s, cx| {
                s.toggle_sort(column);
                cx.notify();
            });
            self.sort_specs[slot].set_primary(state.read(cx).sort);
        } else if self.sort_specs[slot].primary().map(|(c, _)| c) == Some(column) {
            state.update(cx, |s, cx| {
                s.toggle_sort(column);
                cx.notify();
            });
            self.sort_specs[slot].set_primary(state.read(cx).sort);
        } else {
            self.sort_specs[slot].add_or_toggle(column);
            self.rebuild_slot_from_source(slot, cx);
        }
        self.note_possible_layout_change(slot, cx);
        cx.notify();
    }

    pub fn sort_column_descending(&mut self, slot: usize, column: usize, cx: &mut Context<Self>) {
        self.pad_sort_specs();
        let Some(Some(view)) = self.grid_views.get(slot).cloned() else {
            return;
        };
        let state = view.read(cx).state.clone();
        let grid_sort = state.read(cx).sort;
        self.sort_specs[slot].set_primary(grid_sort);
        if self.sort_specs[slot].primary().is_none()
            || self.sort_specs[slot].primary().map(|(c, _)| c) == Some(column)
        {
            state.update(cx, |s, cx| {
                s.sort = Some((column, sqlly_datatable::SortDirection::Descending));
                s.recompute();
                cx.notify();
            });
            self.sort_specs[slot].set_primary(state.read(cx).sort);
        } else {
            if let Some(key) = self.sort_specs[slot]
                .keys
                .iter_mut()
                .find(|k| k.column == column)
            {
                key.descending = true;
            } else {
                self.sort_specs[slot].keys.push(crate::grid_sort::SortKey {
                    column,
                    descending: true,
                });
            }
            self.rebuild_slot_from_source(slot, cx);
        }
        self.note_possible_layout_change(slot, cx);
        cx.notify();
    }

    pub fn remove_from_sort(&mut self, slot: usize, column: usize, cx: &mut Context<Self>) {
        self.pad_sort_specs();
        let Some(Some(view)) = self.grid_views.get(slot).cloned() else {
            return;
        };
        let state = view.read(cx).state.clone();
        let grid_sort = state.read(cx).sort;
        self.sort_specs[slot].set_primary(grid_sort);
        self.sort_specs[slot].remove(column);
        let primary = self.sort_specs[slot].primary();
        state.update(cx, |s, cx| {
            s.sort = primary;
            s.recompute();
            cx.notify();
        });
        self.rebuild_slot_from_source(slot, cx);
        self.note_possible_layout_change(slot, cx);
        cx.notify();
    }

    fn sync_sort_primary(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.pad_sort_specs();
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return;
        };
        let grid_sort = view.read(cx).state.read(cx).sort;
        let Some(spec) = self.sort_specs.get(slot) else {
            return;
        };
        if spec.primary() == grid_sort {
            return;
        }
        let had_secondaries = !spec.secondaries().is_empty();
        self.sort_specs[slot].set_primary(grid_sort);
        if had_secondaries {
            self.rebuild_slot_from_source(slot, cx);
        }
        self.note_possible_layout_change(slot, cx);
    }

    /// Toggle the export dropdown menu.
    pub fn toggle_export_menu(&mut self, cx: &mut Context<Self>) {
        self.show_export_menu = !self.show_export_menu;
        cx.notify();
    }

    /// Export the active result set in the given format. Text formats copy
    /// to the clipboard — unless `save_as` is set (shift held while picking
    /// the menu item), which routes them through the save dialog like the
    /// grid context menu does. Binary formats (XLSX) always save to a file.
    /// Spill-backed sets are capped at [`result_export::EXPORT_MAX_ROWS`];
    /// truncation is written to Messages and a gpui-component warning toast.
    pub fn copy_as(
        &mut self,
        format: ExportFormat,
        save_as: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_export_menu = false;
        // A streamed save hands the spill file to the writer; every other
        // export reads the first rows, which happens off the UI thread.
        #[cfg(not(target_arch = "wasm32"))]
        let streamed = save_as && crate::export_stream::is_streamable(format);
        #[cfg(target_arch = "wasm32")]
        let streamed = false;
        if !streamed
            && self.defer_active_export(window, cx, move |this, window, cx| {
                this.copy_as(format, save_as, window, cx)
            })
        {
            return;
        }
        if format.is_binary() || save_as {
            #[cfg(not(target_arch = "wasm32"))]
            if save_as && crate::export_stream::is_streamable(format) {
                cx.emit(ResultsViewEvent::ExportToFileRequested { format });
                cx.notify();
                return;
            }
            let (bytes, truncated) = if format.is_binary() {
                self.prepare_excel_export(self.active_tab, cx);
                match self.export_bytes_capped(format, result_export::EXPORT_MAX_ROWS, cx) {
                    Some((bytes, truncated)) => (Some(bytes), truncated),
                    None => (None, false),
                }
            } else {
                match self.export_string_capped(format, result_export::EXPORT_MAX_ROWS, cx) {
                    Some((text, truncated)) => (Some(text.into_bytes()), truncated),
                    None => (None, false),
                }
            };
            if truncated {
                self.note_truncated_export(window, cx);
            }
            if bytes.is_some() {
                self.note_preview_cells_in_export(self.active_tab, cx);
            }
            if let Some(bytes) = bytes {
                let ext = format.extension();
                let filename = format!("results.{}", ext);
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(&filename));
                    cx.spawn_in(window, async move |this, cx| {
                        if let Ok(Ok(Some(path))) = rx.await
                            && let Err(message) = write_export_bytes(&path, &bytes, false)
                        {
                            let _ = this.update_in(cx, |this, window, cx| {
                                this.note_export_write_failed(message, window, cx);
                            });
                        }
                    })
                    .detach();
                }
                // Browser build: no save dialog — the file downloads.
                #[cfg(target_arch = "wasm32")]
                if let Err(message) = crate::browser_download::download_bytes(&filename, &bytes) {
                    self.note_export_write_failed(message, window, cx);
                }
            }
        } else {
            use gpui_component::WindowExt as _;
            match self.apply_clipboard_export(format, cx) {
                ClipboardExport::Copied { truncated, .. } if truncated => {
                    // The truncation warning names both counts and raises its
                    // own toast; a second one for one copy would double up.
                    self.note_truncated_export(window, cx);
                }
                ClipboardExport::Copied { .. } => {
                    let msg = self.last_export_notice.clone().unwrap_or_default();
                    window.push_notification(
                        gpui_component::notification::Notification::success(msg),
                        cx,
                    );
                }
                ClipboardExport::NothingToExport => {
                    let msg = self.last_export_notice.clone().unwrap_or_default();
                    window.push_notification(
                        gpui_component::notification::Notification::info(msg),
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// Export-menu "model" rows: copy a type definition built from the
    /// active result set's column names, types, and nullability. Rows never
    /// enter into it, so this is instant regardless of result size.
    pub fn copy_model_as(
        &mut self,
        language: crate::model_codegen::ModelLanguage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::WindowExt as _;
        self.show_export_menu = false;
        let Some(rs) = self.active_result_set() else {
            window.push_notification(
                gpui_component::notification::Notification::info(crate::localization::text(
                    "results.export.no_result_set",
                )),
                cx,
            );
            cx.notify();
            return;
        };
        let table = crate::model_codegen::ModelTable::from_result_columns("Result", &rs.columns);
        let code = crate::model_codegen::generate_model(language, self.source_db_type(), &table);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code));
        window.push_notification(
            gpui_component::notification::Notification::success(crate::localization::format(
                "results.export.model_copied",
                &[("language_name", language.label())],
            )),
            cx,
        );
        cx.notify();
    }

    /// Record a failed Save Results / export write in Messages and a
    /// gpui-component error toast.
    pub fn note_export_write_failed(
        &mut self,
        message: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let msg = self.push_export_write_failed_message(message, cx);
        use gpui_component::WindowExt as _;
        window.push_notification(gpui_component::notification::Notification::error(msg), cx);
    }

    /// Same as [`Self::note_export_write_failed`] when the caller has no
    /// `Window` (grid context menu).
    pub fn note_export_write_failed_messages(
        &mut self,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.push_export_write_failed_message(message, cx);
    }

    fn push_export_write_failed_message(
        &mut self,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> String {
        let msg = message.into();
        self.messages.push(msg.clone());
        self.show_messages = true;
        cx.notify();
        msg
    }

    /// Copy the active result set to the clipboard in `format` and record what
    /// happened. Everything except the toast lives here: raising a toast needs
    /// a `Root`-backed window, which a gpui test window cannot be, so keeping
    /// the decision out of `copy_as` is what makes it testable.
    pub fn apply_clipboard_export(
        &mut self,
        format: ExportFormat,
        cx: &mut Context<Self>,
    ) -> ClipboardExport {
        match self.export_string_capped_counted(
            format,
            result_export::EXPORT_MAX_ROWS,
            &crate::export_options::ExportOptions::default_for(format),
            cx,
        ) {
            Some((text, truncated, rows)) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.push_export_copied_message(format, rows, cx);
                ClipboardExport::Copied { rows, truncated }
            }
            // No active result set. The Export control still renders on a tab
            // that produced only messages, or one where History or DDL was
            // opened while pristine, so the click has to report something
            // instead of silently doing nothing.
            None => {
                self.push_export_empty_message(cx);
                ClipboardExport::NothingToExport
            }
        }
    }

    /// Record a successful clipboard export. Split from the toast the way
    /// truncation and write failures already are: a toast needs a `Root`-backed
    /// window, which a test window cannot be, so the notice is what tests read.
    pub fn push_export_copied_message(
        &mut self,
        format: ExportFormat,
        rows: usize,
        cx: &mut Context<Self>,
    ) -> String {
        let msg = result_export::export_copied_message(&format.localized_label(), rows);
        self.last_export_notice = Some(msg.clone());
        cx.notify();
        msg
    }

    /// Record an export that had no result set to read.
    pub fn push_export_empty_message(&mut self, cx: &mut Context<Self>) -> String {
        let msg = crate::localization::text("results.export.no_results");
        self.last_export_notice = Some(msg.clone());
        cx.notify();
        msg
    }

    /// The rows "Open in Spreadsheet" exports for the active result set:
    /// its columns, the visible (filtered/sorted) rows with export masks
    /// applied — spill-backed sets capped at the export limit — and whether
    /// that cap truncated them. Gathered on the UI thread; the workbook is
    /// built off it (see [`crate::open_in_spreadsheet::spawn_open`]).
    fn spreadsheet_rows_for_active(
        &self,
        cx: &gpui::App,
    ) -> Option<(Vec<ResultColumn>, Vec<Vec<CellValue>>, bool)> {
        let rs = self.active_result_set()?;
        let (rows, truncated) =
            self.visible_rows_for_slot_capped(self.active_tab, result_export::EXPORT_MAX_ROWS, cx);
        let prefs = self.effective_formatting_for_slot(self.active_tab);
        let rows = crate::result_cell_format::apply_export_masks(&rs.columns, rows, &prefs);
        Some((rs.columns.clone(), rows, truncated))
    }

    /// The bytes "Open in Spreadsheet" would hand the OS for the active
    /// result set, in the format [`crate::open_in_spreadsheet::spreadsheet_format`]
    /// picks for `prefs`. Split from the temp-file write and OS open so the
    /// payload (and its format) is testable without spawning anything.
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spreadsheet_export_for_active(
        &self,
        prefs: &crate::preferences::ExportPreferences,
        cx: &gpui::App,
    ) -> Option<(Vec<u8>, ExportFormat, bool)> {
        let (columns, rows, truncated) = self.spreadsheet_rows_for_active(cx)?;
        let (bytes, format) =
            self.spreadsheet_payload(self.active_tab, prefs, &columns, &rows, cx)?;
        Some((bytes, format, truncated))
    }

    /// Export-menu "Open in Spreadsheet": write the active result set to a
    /// private temp file and open it in the OS default spreadsheet app. The
    /// rows are gathered here; the workbook build, write, and launch run in
    /// the background. The temp file is cleaned up on app quit
    /// (best-effort); failures land in Messages and an error toast like any
    /// other export write.
    pub fn open_active_in_spreadsheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_export_menu = false;
        // Browser build: a page cannot launch the spreadsheet app, so the same
        // workbook downloads (see `open_in_spreadsheet::download`).
        #[cfg(target_arch = "wasm32")]
        {
            use gpui_component::WindowExt as _;
            if self.defer_active_export(window, cx, |this, window, cx| {
                this.open_active_in_spreadsheet(window, cx)
            }) {
                return;
            }
            let prefs = crate::preferences::ExportPreferences::load();
            let Some((columns, rows, truncated)) = self.spreadsheet_rows_for_active(cx) else {
                let msg = self.push_export_empty_message(cx);
                window.push_notification(gpui_component::notification::Notification::info(msg), cx);
                cx.notify();
                return;
            };
            if truncated {
                self.note_truncated_export(window, cx);
            }
            self.note_preview_cells_in_export(self.active_tab, cx);
            self.prepare_excel_export(self.active_tab, cx);
            let result = self
                .spreadsheet_payload(self.active_tab, &prefs, &columns, &rows, cx)
                .ok_or_else(|| crate::localization::text("results.open_in_spreadsheet.empty_error"))
                .and_then(|(bytes, format)| crate::open_in_spreadsheet::download(&bytes, format));
            match result {
                Ok(filename) => window.push_notification(
                    gpui_component::notification::Notification::success(
                        crate::localization::format(
                            "results.open_in_spreadsheet.browser_downloaded",
                            &[("file_name", &filename)],
                        ),
                    ),
                    cx,
                ),
                Err(message) => self.note_export_write_failed(message, window, cx),
            }
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use gpui_component::WindowExt as _;
            if self.defer_active_export(window, cx, |this, window, cx| {
                this.open_active_in_spreadsheet(window, cx)
            }) {
                return;
            }
            let prefs = crate::preferences::ExportPreferences::load();
            let Some((columns, rows, truncated)) = self.spreadsheet_rows_for_active(cx) else {
                let msg = self.push_export_empty_message(cx);
                window.push_notification(gpui_component::notification::Notification::info(msg), cx);
                cx.notify();
                return;
            };
            if truncated {
                self.note_truncated_export(window, cx);
            }
            self.note_preview_cells_in_export(self.active_tab, cx);
            self.prepare_excel_export(self.active_tab, cx);
            let built = self.spreadsheet_payload(self.active_tab, &prefs, &columns, &rows, cx);
            let task = crate::open_in_spreadsheet::spawn_open(cx, move || built);
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                let _ = this.update_in(cx, |this, window, cx| match result {
                    Ok(path) => {
                        window.push_notification(
                            gpui_component::notification::Notification::success(
                                crate::localization::format(
                                    "results.open_in_spreadsheet.opened_native",
                                    &[("file_path", &path.display().to_string())],
                                ),
                            ),
                            cx,
                        );
                    }
                    Err(message) => this.note_export_write_failed(message, window, cx),
                });
            })
            .detach();
            cx.notify();
        }
    }

    /// Copy the staged-changes preview script to the clipboard. Returns the
    /// copied text so tests can assert what would land on the clipboard.
    pub fn copy_preview_sql(&mut self, cx: &mut Context<Self>) -> String {
        // Render from the staged set now rather than the debounced preview
        // text, which lags an edit made in the last half second.
        let text = self.staged_change_script();
        if text.is_empty() {
            self.messages.push(crate::localization::text(
                "results.staged_changes.no_changes_to_copy",
            ));
        } else {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            self.messages.push(crate::localization::format(
                "results.staged_changes.sql_copied",
                &[("change_count", &n.to_string())],
            ));
        }
        // Log only — don't yank the output panel away from Preview SQL.
        cx.notify();
        text
    }

    /// The staged-changes script for "Copy SQL"
    /// ([`sqlly_services::row_editing::render_copy_script`]: the preview's
    /// statements plus, where the engine allows, a row-count check after each
    /// UPDATE/DELETE), or empty when nothing is staged.
    pub fn staged_change_script(&self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let batch = self.pending.to_batch(self.source_db_type());
        sqlly_services::row_editing::render_copy_script(self.source_db_type(), &batch)
    }

    /// Record a spill-export truncation in Messages and a gpui-component toast.
    pub fn note_truncated_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let msg = self.push_truncated_export_message(self.active_tab, cx);
        use gpui_component::WindowExt as _;
        window.push_notification(gpui_component::notification::Notification::warning(msg), cx);
    }

    /// Record truncation when the caller has no `Window` (grid context menu).
    pub fn note_truncated_export_for_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.push_truncated_export_message(slot, cx);
    }

    /// Truncation notice for the active result tab (File ▸ Save Results).
    pub fn note_truncated_export_active(&mut self, cx: &mut Context<Self>) {
        self.push_truncated_export_message(self.active_tab, cx);
    }

    /// Grid exports write the rows the grid holds, and a cell over 8 KiB was
    /// fetched as a preview (the full value is only re-read on demand). Say
    /// so in Messages when `slot` has such cells, and point at the exports
    /// that re-run the query with full values. Returns whether it noted.
    pub fn note_preview_cells_in_export(&mut self, slot: usize, cx: &mut Context<Self>) -> bool {
        let cut = self
            .result_sets
            .get(slot)
            .map_or(0, |rs| rs.truncated.len());
        if cut == 0 {
            return false;
        }
        self.messages.push(preview_cells_export_message(cut));
        self.show_messages = true;
        cx.notify();
        true
    }

    /// [`Self::note_preview_cells_in_export`] for the active result tab
    /// (File ▸ Save Results).
    pub fn note_preview_cells_in_active_export(&mut self, cx: &mut Context<Self>) {
        self.note_preview_cells_in_export(self.active_tab, cx);
    }

    fn push_truncated_export_message(&mut self, slot: usize, cx: &mut Context<Self>) -> String {
        let total = self
            .spill_backings
            .get(slot)
            .and_then(|backing| backing.as_ref().map(|backing| backing.total))
            .unwrap_or_else(|| self.row_count_for_slot(slot));
        let exported = result_export::EXPORT_MAX_ROWS.min(total);
        let msg = result_export::spill_export_truncation_message(exported, total);
        self.messages.push(msg.clone());
        self.show_messages = true;
        cx.notify();
        msg
    }

    /// What a background export should read for the active set. Spill-backed
    /// sets hand over the file path (no rows are materialized); resident sets
    /// hand over the visible (filtered/sorted) rows.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn export_source(
        &self,
        cx: &gpui::App,
    ) -> Option<(
        Vec<ResultColumn>,
        sqlly_lang::sqlmodel::DatabaseType,
        crate::export_stream::ExportSource,
    )> {
        let rs = self.active_result_set()?;
        let source = match self.spill_backings.get(self.active_tab) {
            Some(Some(backing)) => crate::export_stream::ExportSource::Spill {
                path: backing.path.clone(),
                total: backing.total,
            },
            _ => crate::export_stream::ExportSource::Rows(
                self.visible_rows_for_slot_capped(self.active_tab, usize::MAX, cx)
                    .0,
            ),
        };
        Some((rs.columns.clone(), self.source_db_type(), source))
    }

    /// Export the active result set to a string in the given text format.
    /// Uses visible (filtered/sorted) rows from the grid state.
    /// Spill-backed sets stop at [`result_export::EXPORT_MAX_ROWS`].
    /// Returns `None` for binary formats (XLSX) — use `export_bytes` for those.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn export_string(&self, format: ExportFormat, cx: &gpui::App) -> Option<String> {
        self.export_string_capped(format, result_export::EXPORT_MAX_ROWS, cx)
            .map(|(text, _truncated)| text)
    }

    /// Text export of the active set, reading at most `cap` spill rows.
    /// `truncated` is true when the spill file holds more than `cap` rows.
    pub fn export_string_capped(
        &self,
        format: ExportFormat,
        cap: usize,
        cx: &gpui::App,
    ) -> Option<(String, bool)> {
        self.export_string_capped_with(
            format,
            cap,
            &crate::export_options::ExportOptions::default_for(format),
            cx,
        )
    }

    /// Text export with explicit options, reading at most `cap` spill rows.
    pub fn export_string_capped_with(
        &self,
        format: ExportFormat,
        cap: usize,
        opts: &crate::export_options::ExportOptions,
        cx: &gpui::App,
    ) -> Option<(String, bool)> {
        self.export_string_capped_counted(format, cap, opts, cx)
            .map(|(text, truncated, _rows)| (text, truncated))
    }

    /// Same as [`Self::export_string_capped_with`], but also reports how many
    /// rows were written. Confirming an export needs that count, and counting
    /// the rows a second time would re-read a spill file or re-walk the
    /// grid's filtered rows for a number this already knows.
    pub fn export_string_capped_counted(
        &self,
        format: ExportFormat,
        cap: usize,
        opts: &crate::export_options::ExportOptions,
        cx: &gpui::App,
    ) -> Option<(String, bool, usize)> {
        let rs = self.active_result_set()?;
        let (rows, truncated) = self.visible_rows_for_slot_capped(self.active_tab, cap, cx);
        let prefs = self.effective_formatting_for_slot(self.active_tab);
        let rows = crate::result_cell_format::apply_export_masks(&rs.columns, rows, &prefs);
        let exported = rows.len();
        let text =
            export_text_for_format_with(format, self.source_db_type(), &rs.columns, &rows, opts)?;
        Some((text, truncated, exported))
    }

    /// Export the active result set as binary bytes (XLSX).
    /// Uses visible (filtered/sorted) rows from the grid state.
    /// Spill-backed sets stop at [`result_export::EXPORT_MAX_ROWS`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn export_bytes(&self, format: ExportFormat, cx: &gpui::App) -> Option<Vec<u8>> {
        self.export_bytes_capped(format, result_export::EXPORT_MAX_ROWS, cx)
            .map(|(bytes, _truncated)| bytes)
    }

    /// Binary export of the active set, reading at most `cap` spill rows.
    pub fn export_bytes_capped(
        &self,
        format: ExportFormat,
        cap: usize,
        cx: &gpui::App,
    ) -> Option<(Vec<u8>, bool)> {
        let rs = self.active_result_set()?;
        let (rows, truncated) = self.visible_rows_for_slot_capped(self.active_tab, cap, cx);
        let prefs = self.effective_formatting_for_slot(self.active_tab);
        let rows = crate::result_cell_format::apply_export_masks(&rs.columns, rows, &prefs);
        if format == ExportFormat::Xlsx {
            // The grid as shown — formats, widths, formulas — plus its pivot.
            return Some((
                self.excel_workbook(self.active_tab, &rs.columns, &rows, cx),
                truncated,
            ));
        }
        Some((
            export_bytes_for_format(format, &rs.columns, &rows)?,
            truncated,
        ))
    }

    /// A source row for stats/inspection: from memory for normal sets, or
    /// paged from the spill file for spill-backed ones.
    fn source_row_for_slot(
        &self,
        slot: usize,
        row: usize,
        cx: &gpui::App,
    ) -> Option<Vec<CellValue>> {
        if let Some(Some(backing)) = self.spill_backings.get(slot) {
            // Clamp to the backing total: a provisional backing's reader can
            // see index entries past the safely flushed prefix.
            if row >= backing.total {
                return None;
            }
            return backing.pager.borrow_mut().row(row);
        }
        let rs = self.result_sets.get(slot)?;
        if !rs.rows.is_empty() {
            return rs.rows.get(self.source_index(slot, row)).cloned();
        }
        self.grid_source_row(slot, row, cx)
    }

    /// Translate a grid `source_row_index` (index into the slot's `GridData`)
    /// into the underlying `rs.rows` index. Identity when no client filter
    /// or secondary sort has rebuilt the grid from a subset.
    pub(crate) fn source_index(&self, slot: usize, grid_source_row: usize) -> usize {
        self.source_index_map
            .get(slot)
            .and_then(|map| map.as_ref())
            .and_then(|map| map.get(grid_source_row).copied())
            .unwrap_or(grid_source_row)
    }

    fn pad_client_filter_slots(&mut self) {
        while self.client_filter.len() < self.result_sets.len() {
            self.client_filter.push(None);
        }
        while self.source_index_map.len() < self.result_sets.len() {
            self.source_index_map.push(None);
        }
        while self.sort_specs.len() < self.result_sets.len() {
            self.sort_specs.push(SortSpec::default());
        }
    }

    fn pad_sort_specs(&mut self) {
        self.pad_client_filter_slots();
    }

    fn set_source_index_map(&mut self, slot: usize, map: Option<Arc<Vec<usize>>>) {
        self.pad_client_filter_slots();
        if slot < self.source_index_map.len() {
            self.source_index_map[slot] = map;
        }
    }

    /// Column kinds, formats and null texts for evaluating a filter tree
    /// against `slot`'s source rows — indexed by RESULT column, like the
    /// rows and the filter model. The live grid's formats are used when
    /// they line up (minus a staged-edit marker column); without a grid
    /// (a marker rebuild drops it first) they are derived from the slot's
    /// config, so a tree filter never silently switches off mid-rebuild.
    fn predicate_context(
        &self,
        slot: usize,
        cx: &gpui::App,
    ) -> Option<(Vec<ColumnKind>, Vec<ResolvedColumnFormat>, Vec<String>)> {
        let rs = self.result_sets.get(slot)?;
        let n = rs.columns.len();
        let live = self
            .grid_views
            .get(slot)
            .and_then(|v| v.as_ref())
            .and_then(|view| {
                let state = view.read(cx).state.read(cx);
                let marker = grid_marker_width(&state.data.columns, n);
                (state.data.columns.len() == n + marker
                    && state.resolved_formats.len() == n + marker)
                    .then(|| {
                        (
                            state.data.columns[marker..]
                                .iter()
                                .map(|c| c.kind)
                                .collect::<Vec<ColumnKind>>(),
                            state.resolved_formats[marker..].to_vec(),
                        )
                    })
            });
        let (kinds, fmts) = match live {
            Some(live) => live,
            None => {
                let grid_columns = build_grid_data(self.classification_db(), &rs.columns, &[])
                    .ok()?
                    .columns;
                let config = self.grid_config_for_columns(slot, &rs.columns);
                let fmts = config.resolve_all(&grid_columns);
                (grid_columns.iter().map(|c| c.kind).collect(), fmts)
            }
        };
        let nulls: Vec<String> = fmts.iter().map(|fmt| fmt.null.text.clone()).collect();
        Some((kinds, fmts, nulls))
    }

    /// Filtered-and-ordered source row indices, or `None` when the grid can
    /// keep a 1:1 mapping onto `rs.rows`.
    fn visible_source_indices(&self, slot: usize, cx: &gpui::App) -> Option<Vec<usize>> {
        let has_filter = self
            .client_filter
            .get(slot)
            .and_then(|f| f.as_ref())
            .is_some();
        let has_secondary = self
            .sort_specs
            .get(slot)
            .is_some_and(|spec| !spec.secondaries().is_empty());
        if !has_filter && !has_secondary {
            return None;
        }
        let rs = self.result_sets.get(slot)?;
        if rs.rows.is_empty() {
            return None;
        }
        let effective = self.effective_formatting_for_slot(slot);
        let grid_rows = convert_rows_owned_with_formats(
            self.classification_db(),
            rs.rows.clone(),
            &rs.columns,
            &self.format_hints,
            Some(&effective),
        );
        let mut indices: Vec<usize> =
            if let Some(filter) = self.client_filter.get(slot).and_then(|f| f.as_ref()) {
                let (kinds, fmts, nulls) = self.predicate_context(slot, cx)?;
                let model = FilterBuilderModel {
                    root: filter.clone(),
                };
                grid_rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| model.row_passes(row, &kinds, &fmts, &nulls))
                    .map(|(i, _)| i)
                    .collect()
            } else {
                (0..rs.rows.len()).collect()
            };
        if has_secondary && let Some(spec) = self.sort_specs.get(slot) {
            // The spec speaks grid columns; these rows are result rows. Keep
            // the primary in place (the grid applies it) and shift the
            // secondaries this pre-order uses.
            let marker = self.marker_offset(slot) as isize;
            let mut shifted = spec.clone();
            shifted.keys = spec
                .keys
                .first()
                .cloned()
                .into_iter()
                .chain(
                    crate::column_layout::shift_sort_spec(
                        &SortSpec {
                            keys: spec.secondaries().to_vec(),
                        },
                        -marker,
                    )
                    .keys,
                )
                .collect();
            shifted.order_secondary(&grid_rows, &mut indices);
        }
        Some(indices)
    }

    fn owned_rows_for_grid(&mut self, slot: usize, cx: &gpui::App) -> Vec<Vec<CellValue>> {
        match self.visible_source_indices(slot, cx) {
            Some(indices) => {
                self.set_source_index_map(slot, Some(Arc::new(indices.clone())));
                let rs = match self.result_sets.get(slot) {
                    Some(rs) => rs,
                    None => return Vec::new(),
                };
                indices
                    .iter()
                    .filter_map(|&i| rs.rows.get(i).cloned())
                    .collect()
            }
            None => {
                self.set_source_index_map(slot, None);
                self.result_sets
                    .get(slot)
                    .map(|rs| rs.rows.clone())
                    .unwrap_or_default()
            }
        }
    }

    /// Selection-aggregate stats text for the active grid's current selection.
    pub fn selection_stats_text(&self, cx: &gpui::App) -> Option<String> {
        let rs = self.active_result_set()?;
        let view = self.grid_views.get(self.active_tab)?.as_ref()?;
        let state = view.read(cx).state.read(cx);
        let sel = &state.selection;
        let slot = self.active_tab;
        let row_count = if matches!(self.spill_backings.get(slot), Some(Some(_))) {
            0
        } else {
            self.row_count_for_slot(slot)
        };
        let (col, row_range) = selection_column_and_rows(sel, row_count)?;
        let column = rs.columns.get(col)?;
        self.memoized_selection_stats(slot, sel, row_count, || {
            if self.selection_rows_unloaded(slot, row_range.clone(), cx) {
                return None;
            }
            let values: Vec<CellValue> = row_range
                .filter_map(|ri| {
                    self.source_row_for_slot(slot, ri, cx)
                        .and_then(|r| r.get(col).cloned())
                })
                .collect();
            if values.is_empty() {
                return None;
            }
            let kind = infer_result_kind_for(self.classification_db(), Some(column));
            let prefs = self.effective_formatting_for_slot(slot);
            selection_stats::footer(&values, kind, &prefs)
        })
    }

    /// Whether a selection on a spill-backed slot reaches rows the browser's
    /// pager has not loaded: those get no stats rather than partial ones.
    /// Always `false` on the desktop, where the pager reads the file.
    fn selection_rows_unloaded(
        &self,
        slot: usize,
        rows: std::ops::Range<usize>,
        cx: &gpui::App,
    ) -> bool {
        cfg!(target_arch = "wasm32")
            && matches!(self.spill_backings.get(slot), Some(Some(_)))
            && rows
                .into_iter()
                .any(|row| self.source_row_for_slot(slot, row, cx).is_none())
    }

    /// Per-card selection stats for a specific result-set slot (used by
    /// stacked-card footers). Mirrors `selection_stats_text` but operates
    /// on the grid at `slot` rather than the active tab.
    pub fn selection_stats_text_for_slot(&self, slot: usize, cx: &gpui::App) -> Option<String> {
        let rs = self.result_sets.get(slot)?;
        let view = self.grid_views.get(slot)?.as_ref()?;
        let state = view.read(cx).state.read(cx);
        let sel = &state.selection;
        let row_count = if matches!(self.spill_backings.get(slot), Some(Some(_))) {
            0
        } else {
            self.row_count_for_slot(slot)
        };
        let (col, row_range) = selection_column_and_rows(sel, row_count)?;
        let column = rs.columns.get(col)?;
        self.memoized_selection_stats(slot, sel, row_count, || {
            if self.selection_rows_unloaded(slot, row_range.clone(), cx) {
                return None;
            }
            let values: Vec<CellValue> = row_range
                .filter_map(|ri| {
                    self.source_row_for_slot(slot, ri, cx)
                        .and_then(|r| r.get(col).cloned())
                })
                .collect();
            if values.is_empty() {
                return None;
            }
            let kind = infer_result_kind_for(self.classification_db(), Some(column));
            let prefs = self.effective_formatting_for_slot(slot);
            selection_stats::footer(&values, kind, &prefs)
        })
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.result_sets.clear();
        self.revealed.clear();
        self.revealed_cells.clear();
        self.column_profiles.clear();
        self.column_profile_requested.clear();
        self.suggested_mask_accepted.clear();
        self.mask_suggestion_dismissed.clear();
        self.chart_suggestions.clear();
        self.chart_suggestion_requested.clear();
        self.chart_suggestion_reverted.clear();
        self.grid_views.clear();
        self.transposed.clear();
        self.transpose_active.clear();
        self.transpose_truncated.clear();
        self.transpose_row_count.clear();
        self.filter_limitations.clear();
        self.default_filter_consulted.clear();
        // A scan of the old result would copy values from a result that is
        // gone; dropping the task cancels it.
        self.background_read = None;
        self.spill_prefixes.borrow_mut().clear();
        self.result_epoch = self.result_epoch.wrapping_add(1);
        self.spill_install_pending = false;
        self.synced_rows.clear();
        self.auto_size.clear();
        self.applied_layouts.clear();
        self.layout_save_generation.clear();
        self.stacked_card_heights.clear();
        self.stacked_resize = None;
        // Cancel in-flight page reads and invalidate their generations
        // before the files are deleted below, so a read that already left
        // the background executor applies nothing.
        self.invalidate_spill_paging();
        // Backings hold open file handles — drop them before deleting the
        // spill files (Windows cannot unlink an open file).
        self.spill_backings.clear();
        self.service_owned_spill_files.clear();
        if let Some(manifest) = self.spill_manifest.0.take() {
            manifest.cleanup();
        }
        self.messages.clear();
        self.diagnostics.clear();
        self.execution_error_offset = None;
        self.elapsed_ms = None;
        self.query_start = None;
        self.first_response_ms = None;
        self.response_finished_ms = None;
        self.completed_at = None;
        self.completed_wall = None;
        self.run_started_wall = None;
        self.ui_paint_ms = None;
        self.wall_clock_ms = None;
        self.is_running = false;
        self.error = None;
        self.active_tab = 0;
        self.tab_switch_gate.reset();
        // The panel must not keep showing a row that no longer exists.
        self.refresh_cell_detail(cx);
        self.hydration_requested = None;
        self.collapsed_cards.clear();
        // Chart layouts and the profile pane describe rows that no longer
        // exist.
        self.chart_configs.clear();
        self.result_profile_grid = None;
        self.result_profile_caption = String::new();
        self.result_profile = None;
        self.result_profile_sql = None;
        self.result_profile_filter = crate::result_profiler::ProfileFilter::default();
        self.result_profile_indices = Vec::new();
        self._result_profile_watch = None;
        self.show_result_profile = false;
        // A new run should come to the front; the Statistics tab itself stays
        // available (`statistics_opened`) because its metrics recompute live.
        self.show_statistics = false;
        self.filter_builder = None;
        self.client_filter.clear();
        self.applied_filter_models.clear();
        self.source_index_map.clear();
        self.sort_specs.clear();
        self.total_row_counts.clear();
        cx.notify();
    }

    pub fn set_diagnostics(
        &mut self,
        diagnostics: Vec<ValidationDiagnostic>,
        cx: &mut Context<Self>,
    ) {
        self.diagnostics = diagnostics;
        cx.notify();
    }

    pub fn set_execution_error_offset(&mut self, offset: Option<usize>, cx: &mut Context<Self>) {
        self.execution_error_offset = offset;
        cx.notify();
    }

    /// Record the engine behind the connection this view's next results come
    /// from, so result-driven SQL generation speaks that engine's dialect.
    pub fn set_source_db_type(&mut self, db: sqlly_lang::sqlmodel::DatabaseType) {
        self.source_db_type = Some(db);
    }

    /// The engine the shown results came from, when one is known; `None`
    /// for a view that has not run a query (unlike [`Self::source_db_type`],
    /// which fills in the default dialect).
    pub fn results_engine(&self) -> Option<sqlly_lang::sqlmodel::DatabaseType> {
        self.source_db_type
    }

    /// Tag restored results with the engine they ran on, or with none when
    /// that was not recorded: an unknown engine reads type names
    /// engine-independently, which beats reading them as whatever the tab
    /// is connected to now.
    pub fn set_results_engine(&mut self, db: Option<sqlly_lang::sqlmodel::DatabaseType>) {
        self.source_db_type = db;
    }

    /// The engine columns and cells are classified by (grid kinds, the
    /// WHERE menu): the results' engine once one is known, otherwise
    /// `None`, which reads type names engine-independently.
    pub fn classification_db(&self) -> Option<sqlly_lang::sqlmodel::DatabaseType> {
        self.source_db_type
    }

    pub fn set_layout_scope(&mut self, scope: Option<crate::column_layout::LayoutScope>) {
        self.layout_scope = scope;
    }

    /// The connection + database these results came from, for value-format
    /// choices that are remembered across sessions (Redis keys). `None`
    /// when the view has no connection scope.
    pub(crate) fn value_format_scope(&self) -> Option<crate::structured_data::FormatScope> {
        let scope = self.layout_scope.as_ref()?;
        (!scope.connection_id.is_empty()).then(|| crate::structured_data::FormatScope {
            connection_id: scope.connection_id.clone(),
            database: scope.database.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_layout_store_for_test(
        &mut self,
        store: crate::column_layout::ColumnLayoutStore,
    ) {
        self.layout_store = Some(store);
        self.layout_store_mode = LayoutStoreMode::MemoryOnly;
    }

    /// Stop writing layouts because the on-disk store is unreadable, and say
    /// so once in Messages.
    fn block_layout_saves(&mut self, error: String) {
        if !matches!(self.layout_store_mode, LayoutStoreMode::Blocked(_)) {
            self.messages.push(crate::localization::format(
                "results.column_layout.save_blocked",
                &[("error", &error)],
            ));
            self.show_messages = true;
        }
        self.layout_store_mode = LayoutStoreMode::Blocked(error);
    }

    /// Read-modify-write the layout store. The file is re-read first so two
    /// results views never drop each other's layouts. An unreadable file is
    /// never overwritten: the change applies to this view's in-memory copy
    /// only, and the error is surfaced once.
    fn update_layout_store(
        &mut self,
        mutate: impl FnOnce(&mut crate::column_layout::ColumnLayoutStore),
    ) {
        if self.layout_store_mode != LayoutStoreMode::Disk {
            mutate(self.layout_store());
            return;
        }
        match crate::column_layout::ColumnLayoutStore::load() {
            Ok(mut store) => {
                mutate(&mut store);
                if let Err(message) = store.save() {
                    self.messages.push(message);
                    self.show_messages = true;
                }
                self.layout_store = Some(store);
            }
            Err(error) => {
                self.block_layout_saves(error);
                mutate(self.layout_store());
            }
        }
    }

    fn layout_key_for_slot(&self, _slot: usize) -> Option<crate::column_layout::ColumnLayoutKey> {
        let scope = self.layout_scope.as_ref()?;
        let sql = self.query_sql.as_ref()?;
        Some(crate::column_layout::key_for(
            &scope.server,
            &scope.database,
            &scope.default_schema,
            sql,
        ))
    }

    fn layout_store(&mut self) -> &mut crate::column_layout::ColumnLayoutStore {
        if self.layout_store.is_none() {
            let store = match crate::column_layout::ColumnLayoutStore::load() {
                Ok(store) => store,
                Err(error) => {
                    self.block_layout_saves(error);
                    crate::column_layout::ColumnLayoutStore::default()
                }
            };
            self.layout_store = Some(store);
        }
        self.layout_store.as_mut().expect("layout store")
    }

    fn apply_saved_layout(
        &mut self,
        slot: usize,
        state: &Entity<GridState>,
        cx: &mut Context<Self>,
    ) {
        while self.applied_layouts.len() <= slot {
            self.applied_layouts.push(None);
        }
        while self.layout_save_generation.len() <= slot {
            self.layout_save_generation.push(0);
        }
        let Some(key) = self.layout_key_for_slot(slot) else {
            return;
        };
        let sig = crate::column_layout::signature_for(&self.result_sets[slot].columns);
        let marker = self.grid_marker_width_of(slot, state.read(cx));
        if let Some(layout) = self.layout_store().layout_for(&key, &sig) {
            let layout = layout.clone();
            self.pad_sort_specs();
            let n = self.result_sets[slot].columns.len();
            if layout.sort.is_valid_for(n) {
                // Stored in result columns; the view's spec follows the grid.
                self.sort_specs[slot] =
                    crate::column_layout::shift_sort_spec(&layout.sort, marker as isize);
            }
            let primary = self.sort_specs.get(slot).and_then(|spec| spec.primary());
            state.update(cx, |s, cx| {
                crate::column_layout::apply_to_grid(s, &layout, marker);
                s.sort = primary;
                if s.sort.is_some() {
                    s.recompute();
                }
                cx.notify();
            });
            self.applied_layouts[slot] = Some(layout);
            return;
        }
        let baseline = crate::column_layout::capture_from_grid(
            state.read(cx),
            &sig,
            &self.sort_spec_for_slot(slot),
            marker,
        );
        self.applied_layouts[slot] = Some(baseline);
    }

    fn sort_spec_for_slot(&self, slot: usize) -> SortSpec {
        self.sort_specs.get(slot).cloned().unwrap_or_default()
    }

    fn preload_layout_sort(&mut self, slot: usize) {
        self.pad_sort_specs();
        let Some(key) = self.layout_key_for_slot(slot) else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let sig = crate::column_layout::signature_for(&rs.columns);
        let n = rs.columns.len();
        // The grid about to be built carries the marker column exactly when
        // edits are staged on this slot.
        let marker = self.marker_offset(slot) as isize;
        let Some(layout) = self.layout_store().layout_for(&key, &sig) else {
            return;
        };
        if layout.sort.is_valid_for(n) {
            let spec = crate::column_layout::shift_sort_spec(&layout.sort, marker);
            self.sort_specs[slot] = spec;
        }
    }

    fn note_possible_layout_change(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.layout_key_for_slot(slot).is_none() {
            return;
        }
        while self.layout_save_generation.len() <= slot {
            self.layout_save_generation.push(0);
        }
        // A stale GridState observer can fire after a new run cleared
        // `result_sets` (e.g. hovering a detached results window mid-run).
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let sig = crate::column_layout::signature_for(&rs.columns);
        let Some(Some(grid)) = self.grid_views.get(slot) else {
            return;
        };
        let state = grid.read(cx).state.clone();
        let current = self.capture_layout_for_slot(slot, &state, &sig, cx);
        if self.applied_layouts.get(slot).and_then(|l| l.as_ref()) == Some(&current) {
            return;
        }
        self.layout_save_generation[slot] = self.layout_save_generation[slot].wrapping_add(1);
        let generation = self.layout_save_generation[slot];
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.flush_layout_save(slot, generation, cx);
            });
        })
        .detach();
    }

    fn flush_layout_save(&mut self, slot: usize, generation: u64, cx: &mut Context<Self>) {
        if self.layout_save_generation.get(slot) != Some(&generation) {
            return;
        }
        let Some(key) = self.layout_key_for_slot(slot) else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let sig = crate::column_layout::signature_for(&rs.columns);
        let Some(Some(grid)) = self.grid_views.get(slot) else {
            return;
        };
        let state = grid.read(cx).state.clone();
        let current = self.capture_layout_for_slot(slot, &state, &sig, cx);
        while self.applied_layouts.len() <= slot {
            self.applied_layouts.push(None);
        }
        self.applied_layouts[slot] = Some(current.clone());
        self.update_layout_store(|store| store.upsert(key, current));
    }

    /// The slot's layout as it is remembered: the grid as it stands, except
    /// that a column auto-sizing is managing is recorded at its name-based
    /// default width. Auto widths are measurements of this run's values, so
    /// persisting them would both re-save the layout after every pass and,
    /// restored on the next run, read as a width someone chose — which is
    /// how auto-sizing used to work exactly once per query. FK label choices
    /// carry over from the layout applied last.
    fn capture_layout_for_slot(
        &self,
        slot: usize,
        state: &Entity<GridState>,
        sig: &crate::column_layout::ColumnSignature,
        cx: &App,
    ) -> crate::column_layout::ColumnLayout {
        let grid = state.read(cx);
        let columns = self
            .result_sets
            .get(slot)
            .map(|rs| rs.columns.as_slice())
            .unwrap_or_default();
        let marker = grid_marker_width(&grid.data.columns, columns.len());
        let mut layout = crate::column_layout::capture_from_grid(
            grid,
            sig,
            &self.sort_spec_for_slot(slot),
            marker,
        );
        if let Some(track) = self
            .auto_size
            .get(&slot)
            .filter(|track| track.state_id == Some(state.entity_id()))
        {
            for (col, (width, def)) in layout.widths.iter_mut().zip(columns).enumerate() {
                if track.manages(col, *width) {
                    *width = crate::grid_bridge::column_width_for_name(&def.name);
                }
            }
        }
        if let Some(prev) = self.applied_layouts.get(slot).and_then(|l| l.as_ref()) {
            layout.fk_labels = prev.fk_labels.clone();
        }
        layout
    }

    pub fn adjust_column_width(
        &mut self,
        slot: usize,
        grid_column: usize,
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(Some(grid)) = self.grid_views.get(slot) else {
            return;
        };
        let state = grid.read(cx).state.clone();
        let marker = self.grid_marker_width_of(slot, state.read(cx));
        // The staged-edit marker column is not a result column: it has no
        // width to remember and no auto-size entry.
        if grid_column < marker {
            return;
        }
        state.update(cx, |s, cx| {
            if let Some(column) = s.data.columns.get_mut(grid_column) {
                column.width = adjusted_column_width(column.width, delta);
                cx.notify();
            }
        });
        if let Some(track) = self.auto_size.get_mut(&slot)
            && let Some(applied) = track.applied.get_mut(grid_column - marker)
        {
            *applied = None;
        }
        self.note_possible_layout_change(slot, cx);
    }

    pub fn reset_column_layout(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.result_sets.get(slot).is_none() {
            return;
        }
        if let Some(key) = self.layout_key_for_slot(slot) {
            self.update_layout_store(|store| {
                store.forget(&key);
            });
        }
        let columns = self.result_sets[slot].columns.clone();
        let sig = crate::column_layout::signature_for(&columns);
        let Some(Some(grid)) = self.grid_views.get(slot) else {
            return;
        };
        let state = grid.read(cx).state.clone();
        self.pad_sort_specs();
        let had_secondaries = self
            .sort_specs
            .get(slot)
            .is_some_and(|spec| !spec.secondaries().is_empty());
        if slot < self.sort_specs.len() {
            self.sort_specs[slot] = SortSpec::default();
        }
        let marker = grid_marker_width(&state.read(cx).data.columns, columns.len());
        state.update(cx, |s, cx| {
            for (col, def) in s.data.columns[marker..].iter_mut().zip(&columns) {
                col.width = crate::grid_bridge::column_width_for_name(&def.name);
            }
            s.show_all_columns();
            s.set_frozen_columns(0);
            s.sort = None;
            s.recompute();
            cx.notify();
        });
        while self.applied_layouts.len() <= slot {
            self.applied_layouts.push(None);
        }
        while self.layout_save_generation.len() <= slot {
            self.layout_save_generation.push(0);
        }
        self.applied_layouts[slot] = Some(crate::column_layout::capture_from_grid(
            state.read(cx),
            &sig,
            &self.sort_spec_for_slot(slot),
            marker,
        ));
        self.layout_save_generation[slot] = self.layout_save_generation[slot].wrapping_add(1);
        // Back at the defaults, every column is auto-sizing's to manage again.
        self.auto_size.remove(&slot);
        if had_secondaries {
            self.rebuild_slot_from_source(slot, cx);
        } else {
            self.auto_size_columns(slot, cx);
        }
        cx.notify();
    }

    pub fn hide_empty_columns(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(Some(grid)) = self.grid_views.get(slot) else {
            return;
        };
        let state = grid.read(cx).state.clone();
        let mut hidden = 0usize;
        state.update(cx, |s, cx| {
            let empty = empty_column_indices(&s.data);
            hidden = empty.len();
            for col in empty {
                s.hide_column(col);
            }
            cx.notify();
        });
        if hidden == 0 {
            self.messages.push(crate::localization::text(
                "results.column_layout.no_empty_columns",
            ));
        } else {
            self.messages.push(crate::localization::format(
                "results.column_layout.empty_columns_hidden",
                &[("hidden_count", &hidden.to_string())],
            ));
        }
        self.show_messages = true;
        self.note_possible_layout_change(slot, cx);
        cx.notify();
    }

    fn apply_density_to_table(&self, table: &SqllyDataTable, cx: &mut App) {
        let density = self.formatting_or_load().density;
        table.state.update(cx, |s, cx| {
            apply_grid_density(s, density, self.grid_zoom, cx);
            cx.notify();
        });
    }

    /// The dialect generated SQL (row edits, exports, WHERE actions) renders
    /// in: the results' engine, or the default dialect until one is known.
    /// Classification goes through [`Self::classification_db`], which keeps
    /// the "unknown" apart.
    pub fn source_db_type(&self) -> sqlly_lang::sqlmodel::DatabaseType {
        self.source_db_type.unwrap_or_default()
    }

    /// Pin the default copy format for tests (skips `ExportPreferences::load`).
    #[allow(dead_code)] // Retained for future use.
    pub fn set_copy_format(&mut self, format: CopyFormat) {
        self.copy_format_override = Some(format);
    }

    /// The export preferences a clipboard copy runs under. A pinned copy
    /// format (tests) stands in for the on-disk file entirely, so the copy
    /// path never reads the developer's real preferences under test.
    fn effective_export_prefs(&self) -> ExportPreferences {
        match self.copy_format_override {
            Some(format) => ExportPreferences {
                default_copy_format: format,
                ..ExportPreferences::default()
            },
            None => ExportPreferences::load(),
        }
    }

    /// Visible (filtered/sorted) columns and rows for `slot`, honouring spill
    /// paging and the pivot grid when `pivot` is set. `spill_truncated` is
    /// true when a spill-backed export was capped at `EXPORT_MAX_ROWS`.
    pub(crate) fn resolve_visible_rows(
        &self,
        slot: usize,
        state: &GridState,
        pivot: bool,
        cx: &App,
    ) -> Option<(Vec<ResultColumn>, Vec<Vec<CellValue>>, bool)> {
        let (columns, rs_rows) = if pivot {
            self.pivot_export_table(slot, cx)?
        } else {
            let rs = self.result_set_at(slot)?;
            (rs.columns.clone(), rs.rows.clone())
        };
        if pivot {
            let mut rows = rows_from_grid_state(state, &rs_rows, |idx| idx);
            if rows.is_empty() {
                rows = rs_rows;
            }
            return Some((columns, rows, false));
        }
        let mut spill_truncated = false;
        let rows = if let Some((rows, truncated)) =
            self.spill_rows_for_export(slot, result_export::EXPORT_MAX_ROWS)
        {
            spill_truncated = truncated;
            rows
        } else {
            let mut rows =
                rows_from_grid_state(state, &rs_rows, |idx| self.source_index(slot, idx));
            if rows.is_empty() {
                rows = self.resident_source_rows_for_slot(slot, cx);
            }
            rows
        };
        Some((columns, rows, spill_truncated))
    }

    /// Copy the current selection in the user's default copy format.
    /// `Selection::Cell` copies that cell's display text only; every other
    /// selection (including none) follows the export scoping rules.
    pub fn copy_selection_default(
        &mut self,
        slot: usize,
        with_headers: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(grid) = self.grid_views.get(slot).and_then(|g| g.clone()) else {
            return;
        };
        let state_entity = grid.read(cx).state.clone();
        // A windowed result's rows are read from disk in the background first.
        let limit = result_export::EXPORT_MAX_ROWS;
        let weak = cx.weak_entity();
        if self.defer_until_spill_prefixes(
            vec![(slot, limit)],
            crate::localization::format(
                "results.copy.reading_rows",
                &[("row_count", &self.spill_read_count(slot, limit).to_string())],
            ),
            cx,
            move |cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.copy_selection_default(slot, with_headers, cx)
                });
            },
        ) {
            return;
        }
        let (selection, columns, rows) = {
            let state = state_entity.read(cx);
            let Some((columns, rows, _)) = self.resolve_visible_rows(slot, state, false, cx) else {
                return;
            };
            // Rows come back in result columns; so must the selection.
            let marker = self.grid_marker_width_of(slot, state);
            let selection = selection_in_result_columns(&state.selection, marker);
            if matches!(state.selection, sqlly_datatable::Selection::Cell(..))
                && selection == sqlly_datatable::Selection::None
            {
                // The staged-edit marker cell holds no value to copy.
                return;
            }
            (selection, columns, rows)
        };
        let prefs = self.effective_formatting_for_slot(slot);
        let rows = crate::result_cell_format::apply_export_masks(&columns, rows, &prefs);
        if let sqlly_datatable::Selection::Cell(row, col) = selection {
            let source_row = self.source_row_index_for_grid(slot, row, state_entity.read(cx));
            if self.truncated_cell(slot, source_row, col).is_some() {
                self.request_cell_hydration(
                    slot,
                    source_row,
                    col,
                    crate::cell_hydration::HydratePurpose::Copy,
                    cx,
                );
                return;
            }
            let text = rows
                .get(row)
                .and_then(|r| r.get(col))
                .map(result_export::cell_display)
                .unwrap_or_default();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            return;
        }
        let (columns, rows) =
            crate::result_grid_menu::selection_export_data(&columns, rows, &selection);
        let prefs = self.effective_export_prefs();
        let fmt = prefs.default_copy_format;
        let opts = crate::export_options::ExportOptions {
            header_row: with_headers,
            ..adhoc_export_options(fmt.export_format(), &prefs)
        };
        let text = match fmt {
            CopyFormat::Tsv => result_export::tsv_with(&columns, &rows, &opts),
            CopyFormat::Csv => result_export::csv_with(&columns, &rows, &opts),
            other => export_text_for_format_with(
                other.export_format(),
                self.source_db_type(),
                &columns,
                &rows,
                &opts,
            )
            .unwrap_or_default(),
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    pub fn start_query(&mut self, cx: &mut Context<Self>) {
        // Supersede any prior run: its still-draining pump must not land
        // events in this run's view (see `handle_event_for_run`).
        self.run_generation += 1;
        // Save current result sets as "previous" if difference tracking is on.
        if self.track_difference && !self.result_sets.is_empty() {
            self.previous_result_sets = self
                .result_sets
                .iter()
                .enumerate()
                .map(|(slot, rs)| {
                    let mut cloned = rs.clone();
                    if cloned.rows.is_empty()
                        && !matches!(self.spill_backings.get(slot), Some(Some(_)))
                    {
                        cloned.rows = self.resident_source_rows_for_slot(slot, cx);
                    }
                    cloned
                })
                .collect();
            // Bound diff memory: a set past the row cap is never diffed (the
            // Completed gate skips it), so don't hold a full copy of its rows
            // either. Keep the slot for index alignment but free the payload;
            // the preserved logical count makes `rows_complete()` report it
            // incomplete, which the existing gate already refuses to diff.
            for rs in &mut self.previous_result_sets {
                if rs.rows.len() > result_set_difference::DIFF_MAX_ROWS {
                    rs.row_count_hint = rs.row_count_hint.max(rs.rows.len());
                    rs.rows = Vec::new();
                }
            }
        }
        self.retain_history_before_new_run(cx);
        self.pending_keep_results = false;
        self.viewing_run = None;
        self.current_run_snapshot = None;
        self.clear(cx);
        self.execution_error_offset = None;
        self.historical_banner = None;
        self.is_running = true;
        self.query_start = Some(Instant::now());
        self.run_started_wall = Some(SystemTime::now());
        self.elapsed_ticker = None;
        self.schedule_elapsed_ticker(cx);
        self.diff = None;
        self.detailed_diff = None;
        self.diff_unavailable_reason = None;
        self.detailed_diff_grid = None;
        self.show_diff = false;
        // Running the query brings its results forward; the captured plan
        // stays in its pane (SSMS keeps plan tabs around too).
        self.show_execution_plan = false;
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn elapsed_ms(&self) -> Option<u64> {
        self.elapsed_ms
    }

    pub fn execution_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    #[allow(dead_code)]
    pub fn first_response_ms(&self) -> Option<u64> {
        self.first_response_ms
    }

    #[allow(dead_code)]
    pub fn response_finished_ms(&self) -> Option<u64> {
        self.response_finished_ms
    }

    #[allow(dead_code)]
    pub fn ui_paint_ms(&self) -> Option<u64> {
        self.ui_paint_ms
    }

    #[allow(dead_code)]
    pub fn wall_clock_ms(&self) -> Option<u64> {
        self.wall_clock_ms
    }

    /// Server Total = First Response + Server Execution (elapsed_ms).
    pub fn server_total_ms(&self) -> Option<u64> {
        self.first_response_ms
            .zip(self.elapsed_ms)
            .map(|(first, elapsed)| first + elapsed)
    }

    #[allow(dead_code)]
    pub fn estimated_latency_ms(&self) -> Option<u64> {
        self.estimated_latency_ms
    }

    pub fn set_estimated_latency(&mut self, ms: u64, cx: &mut Context<Self>) {
        self.estimated_latency_ms = Some(ms);
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn subtract_latency(&self) -> bool {
        self.subtract_latency
    }

    pub fn toggle_subtract_latency(&mut self, cx: &mut Context<Self>) {
        self.subtract_latency = !self.subtract_latency;
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn timing_format(&self) -> TimingFormat {
        self.timing_format
    }

    pub fn toggle_timing_format(&mut self, cx: &mut Context<Self>) {
        self.timing_format = match self.timing_format {
            TimingFormat::Duration => TimingFormat::Elapsed,
            TimingFormat::Elapsed => TimingFormat::Duration,
        };
        cx.notify();
    }

    /// Apply latency subtraction (if enabled) to a raw timing value,
    /// clamping at 0. Returns `None` if `raw` is `None`.
    fn display_ms(&self, raw: Option<u64>) -> Option<u64> {
        raw.map(|ms| {
            if self.subtract_latency {
                if let Some(lat) = self.estimated_latency_ms {
                    ms.saturating_sub(lat)
                } else {
                    ms
                }
            } else {
                ms
            }
        })
    }

    /// Logical row count for a result-set slot: the spill backing's total
    /// when the slot is spill-backed (`rs.rows` is empty then), otherwise
    /// the larger of the resident rows and the streaming count hint.
    pub fn row_count_for_slot(&self, slot: usize) -> usize {
        let hint = self
            .result_sets
            .get(slot)
            .map(|rs| {
                rs.rows
                    .len()
                    .max(self.synced_rows.get(slot).copied().unwrap_or(0))
                    .max(rs.row_count_hint)
            })
            .unwrap_or(0);
        if let Some(Some(backing)) = self.spill_backings.get(slot) {
            // A provisional backing's total is the flushed prefix; the
            // count-only `RowsProgress` fallback (flush failure) can run
            // ahead of it, so the live count must not freeze at the prefix.
            // The manifest overwrites both, so the authoritative total
            // still wins at completion.
            return backing.total.max(hint);
        }
        hint
    }

    /// Whether `slot` holds its complete logical row set in memory (either
    /// still buffered in `rs.rows` or fully synced into the grid).
    pub fn rows_complete_for_slot(&self, slot: usize) -> bool {
        let Some(rs) = self.result_sets.get(slot) else {
            return false;
        };
        if matches!(self.spill_backings.get(slot), Some(Some(_))) {
            return rs.row_count_hint > 0;
        }
        let resident = if rs.rows.is_empty() {
            self.synced_rows.get(slot).copied().unwrap_or(0)
        } else {
            rs.rows.len()
        };
        resident >= rs.row_count_hint
    }

    /// Source-order rows for a resident in-memory slot: buffered `rs.rows`
    /// when present, otherwise the grid's `GridData`. Spill-backed slots
    /// must use the pager instead.
    pub fn resident_source_rows_for_slot(
        &self,
        slot: usize,
        cx: &gpui::App,
    ) -> Vec<Vec<CellValue>> {
        if matches!(self.spill_backings.get(slot), Some(Some(_))) {
            return Vec::new();
        }
        let Some(rs) = self.result_sets.get(slot) else {
            return Vec::new();
        };
        if !rs.rows.is_empty() {
            return rs.rows.clone();
        }
        self.grid_source_rows(slot, cx).unwrap_or_default()
    }

    fn grid_source_rows(&self, slot: usize, cx: &gpui::App) -> Option<Vec<Vec<CellValue>>> {
        let view = self.grid_views.get(slot)?.as_ref()?;
        let state = view.read(cx).state.read(cx);
        Some(
            state
                .data
                .rows
                .iter()
                .map(|row| row.iter().map(grid_cell_to_core).collect())
                .collect(),
        )
    }

    fn grid_source_row(
        &self,
        slot: usize,
        row_index: usize,
        cx: &gpui::App,
    ) -> Option<Vec<CellValue>> {
        let view = self.grid_views.get(slot)?.as_ref()?;
        let state = view.read(cx).state.read(cx);
        state
            .data
            .rows
            .get(row_index)
            .map(|row| row.iter().map(grid_cell_to_core).collect::<Vec<_>>())
    }

    #[allow(dead_code)]
    pub fn total_rows(&self) -> usize {
        (0..self.result_sets.len())
            .map(|slot| self.row_count_for_slot(slot))
            .sum()
    }

    /// Status-bar label for a result-set slot. A SELECT can carry a server
    /// DONE count in `rows_affected`, so the logical row count wins whenever
    /// the set has rows — "affected" wording is reserved for DML/no-row sets.
    fn result_count_label(&self, slot: usize, fmt: &ResultFormattingPreferences) -> String {
        let Some(rs) = self.result_sets.get(slot) else {
            return String::new();
        };
        let row_count = self.row_count_for_slot(slot);
        if row_count == 0
            && let Some(affected) = rs.rows_affected
        {
            crate::localization::format(
                "results.status_bar.rows_affected",
                &[
                    ("result_number", &(rs.index + 1).to_string()),
                    ("row_count", &fmt.format_integer(affected as i64)),
                ],
            )
        } else {
            let shown = fmt.format_integer(row_count as i64);
            if let Some(TotalCount::Ready(n)) = self.total_row_counts.get(&slot) {
                crate::localization::format(
                    "results.status_bar.shown_of_total_rows",
                    &[
                        ("result_number", &(rs.index + 1).to_string()),
                        ("shown_rows", &shown),
                        ("total_rows", &fmt.format_integer(*n as i64)),
                    ],
                )
            } else {
                crate::localization::format(
                    "results.status_bar.result_row_count",
                    &[
                        ("result_number", &(rs.index + 1).to_string()),
                        ("row_count", &shown),
                    ],
                )
            }
        }
    }

    /// Format the First response time with latency subtraction applied (if
    /// enabled). Only First gets latency subtracted — all other timings
    /// reflect actual measured values.
    #[allow(dead_code)]
    pub fn format_first_response(&self) -> Option<String> {
        self.display_ms(self.first_response_ms)
            .map(|ms| match self.timing_format {
                TimingFormat::Duration => format_duration_ms(ms),
                TimingFormat::Elapsed => format_elapsed(ms),
            })
    }

    /// Format a raw timing value using the active display format.
    /// Does NOT subtract latency — only First uses `display_ms` for that.
    pub fn format_timing(&self, raw: Option<u64>) -> Option<String> {
        raw.map(|ms| match self.timing_format {
            TimingFormat::Duration => format_duration_ms(ms),
            TimingFormat::Elapsed => format_elapsed(ms),
        })
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.is_running
    }

    /// True while the pane has never produced anything: no result sets, no
    /// messages, nothing in flight, and no run on record. The shell uses this
    /// to decide whether the welcome canvas may take over the central area — a
    /// tab that has run (or is running) keeps its workbench no matter the
    /// connection state. A run that completes with neither result sets nor
    /// server messages still counts (`run_started_wall` is set): the Messages
    /// tab must stay reachable to show the run's start/finish clock lines.
    pub fn is_pristine(&self) -> bool {
        self.result_sets.is_empty()
            && self.messages.is_empty()
            && !self.is_running
            && self.run_started_wall.is_none()
    }

    /// Results/Messages/Plan/History/DDL tabs stay hidden until the first run
    /// (or a message) lands — unless the user opened History or DDL from the
    /// command palette.
    pub fn results_chrome_visible(&self) -> bool {
        !self.is_pristine() || self.show_history || self.show_ddl
    }

    /// Record `first_response_ms` from `query_start` if not already set.
    /// Called on the first event that carries server data.
    fn record_first_response(&mut self) {
        if self.first_response_ms.is_none()
            && let Some(start) = self.query_start
        {
            self.first_response_ms = Some(start.elapsed().as_millis() as u64);
        }
    }

    /// Record `response_finished_ms` from `query_start` and stamp
    /// `completed_at` so `render()` can detect the first paint after
    /// completion.
    fn record_response_finished(&mut self) {
        if self.response_finished_ms.is_none()
            && let Some(start) = self.query_start
        {
            self.response_finished_ms = Some(start.elapsed().as_millis() as u64);
        }
        self.completed_at = Some(Instant::now());
    }

    /// The stamp of the current query run, for tagging forwarded events.
    pub fn run_generation(&self) -> u64 {
        self.run_generation
    }

    /// [`Self::handle_event`] gated on the run stamp captured at pump spawn:
    /// events from a superseded run's still-draining pump are dropped
    /// wholesale. Without this, a stale `SpillWindow`/`SpillManifest` from a
    /// cancelled or re-run-over query would install a backing over the OLD
    /// query's spill file onto the NEW query's same-index result set,
    /// presenting the old rows as the new query's results.
    pub fn handle_event_for_run(&mut self, run: u64, event: QueryEvent, cx: &mut Context<Self>) {
        if run != self.run_generation {
            // A stale SpillWindow's file is a BORROW (service-owned). Record
            // it so a later stale SpillManifest for the same file does NOT
            // delete it — the executor cleans up its own files.
            if let QueryEvent::SpillWindow { spill_file, .. } = &event {
                self.service_owned_spill_files.insert(spill_file.clone());
            }
            // A stale manifest is the deletion owner of its spill files — the
            // superseded run's backings were already dropped by `clear()`, so
            // nobody else will ever delete them — UNLESS a stale SpillWindow
            // already marked the file as service-owned.
            if let QueryEvent::SpillManifest { manifest } = event {
                for rs in &manifest.result_sets {
                    if !self.service_owned_spill_files.contains(&rs.spill_file) {
                        crate::spill_pager::remove_spill_files(&rs.spill_file);
                    }
                }
            }
            return;
        }
        self.handle_event(event, cx);
    }

    pub fn handle_event(&mut self, event: QueryEvent, cx: &mut Context<Self>) {
        // `Rows`/`RowsProgress`/`SpillWindow` arrive in bursts during
        // streaming — their repaints are coalesced to ~one per frame (see
        // the tail of this fn).
        let streaming = matches!(
            event,
            QueryEvent::Rows { .. }
                | QueryEvent::TruncatedCells { .. }
                | QueryEvent::RowsProgress { .. }
                | QueryEvent::SpillWindow { .. }
        );
        match event {
            QueryEvent::Started => {
                self.is_running = true;
                self.set_pivot_loading(true, cx);
            }
            QueryEvent::ResultSet { index, columns } => {
                self.record_first_response();
                // Computed columns join the result here; rows are extended
                // as they arrive.
                let plan = crate::computed_columns::ComputedPlan::new(
                    &self.computed_specs,
                    &columns,
                    self.result_sets.len(),
                );
                let columns = match &plan {
                    Some(plan) => plan.output_columns(&columns),
                    None => columns,
                };
                while self.computed_plans.len() < self.result_sets.len() {
                    self.computed_plans.push(None);
                }
                self.computed_plans.push(plan);
                self.result_sets.push(ResultSet {
                    index,
                    columns,
                    rows: Vec::new(),
                    rows_affected: None,
                    row_count_hint: 0,
                    ..Default::default()
                });
                self.active_tab = self.result_sets.len() - 1;
                // `-- sqlly focus: <n>` keeps result n in view: its tab stays
                // selected once it has arrived, and its stacked card is
                // scrolled to the top of the stack when it arrives (cards
                // and their dividers alternate as the stack's children).
                if let Some(focus) = self.layout_directives.focus_slot() {
                    let slot = self.result_sets.len() - 1;
                    if slot > focus {
                        self.active_tab = focus;
                    } else if slot == focus && self.layout_mode == LayoutMode::Stacked {
                        self.stacked_scroll_handle.scroll_to_top_of_item(focus * 2);
                    }
                }
                self.grid_views.push(None);
                self.collapsed_cards.push(false);
                self.stacked_card_heights.push(None);
                self.spill_backings.push(None);
                self.pad_reveal_slots();
                self.pad_client_filter_slots();
                let slot = self.result_sets.len() - 1;
                if slot < self.client_filter.len() {
                    self.client_filter[slot] = None;
                }
                self.applied_filter_models.remove(&slot);
                if slot < self.source_index_map.len() {
                    self.source_index_map[slot] = None;
                }
                if let Some(set) = self.revealed.get_mut(slot) {
                    set.clear();
                }
                if let Some(set) = self.revealed_cells.get_mut(slot) {
                    set.clear();
                }
                if self.layout_mode == LayoutMode::Stacked || self.layout_mode == LayoutMode::Canvas
                {
                    self.ensure_all_grids(cx);
                } else {
                    self.ensure_grid_for_active_tab(cx);
                }
            }
            QueryEvent::Rows { index, rows } => {
                self.record_first_response();
                if let Some(slot) = self.result_sets.iter().position(|r| r.index == index) {
                    let rows = match self.computed_plans.get(slot).and_then(|p| p.as_ref()) {
                        Some(plan) => plan.extend_rows(
                            &self.result_sets[slot].rows,
                            rows,
                            &self.formula_variables,
                            self.formula_now,
                        ),
                        None => rows,
                    };
                    let rs = &mut self.result_sets[slot];
                    rs.row_count_hint += rows.len();
                    rs.rows.extend(rows);
                }
                self.refresh_grid_data(index, cx);
            }
            QueryEvent::TruncatedCells { index, cells } => {
                self.record_first_response();
                if let Some(slot) = self.result_sets.iter().position(|r| r.index == index) {
                    let plan = self.computed_plans.get(slot).and_then(|p| p.as_ref());
                    let rs = &mut self.result_sets[slot];
                    for mut cell in cells {
                        // Truncation is reported in source columns.
                        if let Some(plan) = plan {
                            match plan.output_index_of_source(cell.column) {
                                Some(at) => cell.column = at,
                                None => continue,
                            }
                        }
                        rs.truncated.insert((cell.row, cell.column), cell);
                    }
                }
                if let Some(slot) = self.result_sets.iter().position(|r| r.index == index) {
                    self.rebuild_slot_from_source(slot, cx);
                }
            }
            QueryEvent::RowsProgress { index, rows_seen } => {
                // Count-only progress after payload forwarding stopped: bump
                // the hint and nothing else — no rows are appended, the grid
                // and pivot cache are untouched (the manifest delivers the
                // complete set later). `max` because progress can arrive
                // late/reordered relative to `Rows` batches.
                if let Some(rs) = self.result_sets.iter_mut().find(|r| r.index == index) {
                    rs.row_count_hint = rs.row_count_hint.max(rows_seen);
                }
            }
            QueryEvent::SpillWindow {
                index,
                spill_file,
                rows_available,
            } => {
                // Live paging over the suppressed set's growing spill file:
                // rows 0..rows_available are durably flushed and safe to
                // page. The count duty rides along (SpillWindow subsumes
                // RowsProgress for suppressed sets with a readable file).
                if let Some(rs) = self.result_sets.iter_mut().find(|r| r.index == index) {
                    rs.row_count_hint = rs.row_count_hint.max(rows_available);
                }
                self.handle_spill_window(index, &spill_file, rows_available, cx);
            }
            QueryEvent::RowCount {
                index,
                rows_affected,
            } => {
                self.record_first_response();
                if let Some(rs) = self.result_sets.iter_mut().find(|r| r.index == index) {
                    rs.rows_affected = Some(rows_affected);
                }
            }
            QueryEvent::Message { text } => {
                self.record_first_response();
                self.messages.push(text);
            }
            QueryEvent::DatabaseChanged { name } => {
                // The tab's session db retarget is applied by the app pump; here
                // we just note it in the Messages pane, the way SSMS does.
                self.record_first_response();
                self.messages.push(crate::localization::format(
                    "results.messages.database_context_changed",
                    &[("database_name", &name)],
                ));
            }
            QueryEvent::Error { message } => {
                self.record_first_response();
                self.record_response_finished();
                self.messages.push(crate::localization::format(
                    "results.messages.query_error",
                    &[("error", &message)],
                ));
                self.error = Some(message);
                self.is_running = false;
                self.set_pivot_loading(false, cx);
                cx.emit(ResultsViewEvent::QueryFailed);
            }
            QueryEvent::Completed { elapsed_ms } => {
                self.record_first_response();
                self.record_response_finished();
                self.elapsed_ms = Some(elapsed_ms as u64);
                self.is_running = false;
                self.completed_wall = Some(SystemTime::now());
                self.set_pivot_loading(false, cx);
                if self.track_difference && !self.previous_result_sets.is_empty() {
                    let active = self.active_tab;
                    let spill_backed = matches!(self.spill_backings.get(active), Some(Some(_)));
                    // Both sides must have their full rows resident: a
                    // previous set cloned from a spill-backed slot has empty
                    // rows (its file is gone), and a suppressed current set
                    // without a backing has only preview rows — diffing
                    // either would silently report a bogus difference.
                    let prev_complete = self
                        .previous_result_sets
                        .get(active)
                        .is_some_and(|rs| rs.rows_complete());
                    let curr_complete = self.rows_complete_for_slot(active);
                    // Both sides must also fit the diff row cap: diffing
                    // clones and multiset-groups both sets, so an oversized
                    // side would blow up memory. Sized by the logical count,
                    // so a previous slot whose oversized rows were dropped at
                    // retention still reads as over the cap.
                    let within_cap = |rs: &ResultSet| {
                        rs.rows.len().max(rs.row_count_hint) <= result_set_difference::DIFF_MAX_ROWS
                    };
                    let prev_within_cap = self
                        .previous_result_sets
                        .get(active)
                        .is_some_and(within_cap);
                    let curr_within_cap = self.result_sets.get(active).is_some_and(within_cap);
                    if !spill_backed
                        && prev_complete
                        && curr_complete
                        && prev_within_cap
                        && curr_within_cap
                    {
                        let _ = self.compute_diff(cx);
                        if self.diff.as_ref().is_some_and(|d| !d.is_identical()) {
                            self.show_diff = true;
                        }
                    }
                }
                self.retain_completed_diff_run(cx);
                cx.emit(ResultsViewEvent::QueryCompleted);
                self.hydration_requested = None;
                self.refresh_cell_detail(cx);
            }
            QueryEvent::Cancelled => {
                self.record_response_finished();
                self.is_running = false;
                self.set_pivot_loading(false, cx);
                self.messages.push(crate::localization::text(
                    "results.messages.query_cancelled",
                ));
            }
            QueryEvent::SpillManifest { manifest } => {
                self.record_first_response();
                self.record_response_finished();
                if self.elapsed_ms.is_none() {
                    self.elapsed_ms = manifest.elapsed_ms.map(|ms| ms as u64);
                }
                let gpui_manifest = crate::spill_pager::SpillManifest {
                    result_sets: manifest
                        .result_sets
                        .iter()
                        .map(|rs| crate::spill_pager::SpillResultSet {
                            result_set_index: rs.result_set_index,
                            columns: rs.columns.clone(),
                            row_count: rs.row_count,
                            spill_file: rs.spill_file.clone(),
                        })
                        .collect(),
                    messages: manifest.messages.clone(),
                    error: manifest.error.clone(),
                    elapsed_ms: manifest.elapsed_ms,
                };
                self.set_spill_manifest(gpui_manifest, cx);
            }
        }
        if streaming {
            self.schedule_streaming_notify(cx);
        } else {
            // Terminal/metadata events repaint immediately; clearing the
            // pending flag turns any armed timer's wake into a no-op instead
            // of a duplicate trailing repaint.
            self.streaming_notify_pending = false;
            cx.notify();
        }
    }

    /// Coalesce repaints for high-frequency streaming events: mark a repaint
    /// pending and arm a single ~16ms (one frame) timer if none is armed. The
    /// timer stays armed across a burst — re-arming per event would restart
    /// the window and churn tasks — so a storm of `Rows`/`RowsProgress`
    /// events yields at most one notify per frame.
    fn schedule_streaming_notify(&mut self, cx: &mut Context<Self>) {
        self.streaming_notify_pending = true;
        if self.streaming_notify_task.is_some() {
            return;
        }
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        self.streaming_notify_task = Some(cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.streaming_notify_task = None;
                    if this.streaming_notify_pending {
                        this.streaming_notify_pending = false;
                        cx.notify();
                    }
                })
                .ok();
        }));
    }

    fn schedule_elapsed_ticker(&mut self, cx: &mut Context<Self>) {
        if self.elapsed_ticker.is_some() {
            return;
        }
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        self.elapsed_ticker = Some(cx.spawn(async move |_this, _cx| {
            loop {
                async_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(1000))
                    .await;
                let mut cx = async_cx.clone();
                match weak_self.update(&mut cx, |this, cx| {
                    if this.is_running {
                        cx.notify();
                        true
                    } else {
                        this.elapsed_ticker = None;
                        false
                    }
                }) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        }));
    }

    /// One raw core cell from a spill-backed result set, paging exactly that
    /// one row from the spill. `row` is the ABSOLUTE spill row: callers
    /// translate grid-reported (resident-relative) indices with
    /// [`backing_row_for_grid_row`] first. That translation cannot happen
    /// here — the context menu calls this from inside the grid state's own
    /// update, where reading the state entity would double-lease it. `None`
    /// when the slot has no spill backing or the row is out of range.
    pub(crate) fn spill_cell_for_slot(
        &self,
        slot: usize,
        row: usize,
        col: usize,
    ) -> Option<CellValue> {
        let backing = self.spill_backings.get(slot)?.as_ref()?;
        if row >= backing.total {
            return None;
        }
        let row = backing.pager.borrow_mut().page(row, 1).pop()?;
        row.get(col).cloned()
    }

    pub(crate) fn truncated_cell(
        &self,
        slot: usize,
        source_row: usize,
        col: usize,
    ) -> Option<TruncatedCell> {
        self.result_sets
            .get(slot)
            .and_then(|rs| rs.truncated.get(&(source_row, col)).copied())
    }

    pub(crate) fn result_set_sql_and_index(&self, slot: usize) -> Option<(String, usize)> {
        let sql = self.query_sql.clone()?;
        let index = self.result_sets.get(slot)?.index;
        Some((sql, index))
    }

    fn apply_truncation_markers(
        &self,
        slot: usize,
        first_source_row: usize,
        grid_rows: &mut [Vec<sqlly_datatable::CellValue>],
    ) {
        // Markers are keyed by row in the full result; a drill's filtered
        // file numbers its rows afresh.
        if self.spill_drill_shown(slot) {
            return;
        }
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        for (grid_row, row) in grid_rows.iter_mut().enumerate() {
            let source_row = self.source_index(slot, first_source_row + grid_row);
            for (col, cell) in row.iter_mut().enumerate() {
                let Some(meta) = rs.truncated.get(&(source_row, col)) else {
                    continue;
                };
                // Every cut cell carries the marker, whatever its grid
                // variant: the WHERE menu and the action it builds both
                // read "this is only a preview" from the text, and a cut
                // cell with no marker would offer a filter on the preview.
                let marker = crate::cell_hydration::truncated_marker(meta);
                match cell {
                    sqlly_datatable::CellValue::Text(s) => s.push_str(&marker),
                    sqlly_datatable::CellValue::None => {}
                    // The grid's cell value carries display text only as
                    // `Text`, so a cut number, date or boolean is shown as
                    // its text plus the marker (which also drops its numeric
                    // alignment and sort, fittingly for a value that's only a
                    // preview).
                    other => {
                        let shown = crate::result_grid_menu::format_grid_cell_text(other);
                        *other = sqlly_datatable::CellValue::Text(shown + &marker);
                    }
                }
            }
        }
    }

    fn source_row_index_for_grid(&self, slot: usize, grid_row: usize, state: &GridState) -> usize {
        if let Some(window) = state.window {
            return window.offset.saturating_add(grid_row);
        }
        if matches!(self.spill_backings.get(slot), Some(Some(_))) {
            return grid_row;
        }
        self.source_index(slot, grid_row)
    }

    /// The raw row at an absolute `row` of `slot`, as the engine returned
    /// it: the row the truncation entries are keyed by (see
    /// [`Self::truncation_row_for_grid`] for a grid row). A resident row is
    /// borrowed; a spilled row is one disk read and comes back owned, so
    /// callers that want several cells of a row read it once and index into
    /// it. Reads the host's rows and the spill pager only — never the grid's
    /// own state, so it is safe to call from inside a grid callback.
    pub(crate) fn raw_row_at(&self, slot: usize, row: usize) -> Option<RawRow<'_>> {
        let cells = if let Some(Some(backing)) = self.spill_backings.get(slot) {
            if row >= backing.total {
                return None;
            }
            std::borrow::Cow::Owned(backing.pager.borrow_mut().row(row)?)
        } else {
            std::borrow::Cow::Borrowed(self.result_sets.get(slot)?.rows.get(row)?.as_slice())
        };
        Some(RawRow { cells })
    }

    /// The absolute row that truncation entries and the spill pager use for
    /// a grid-resident source row: on a spill-backed (windowed) grid the
    /// resident row is relative to the loaded window; otherwise it goes
    /// through the client-filter source map.
    pub(crate) fn truncation_row_for_grid(
        &self,
        slot: usize,
        grid_source_row: usize,
        window_offset: Option<usize>,
    ) -> usize {
        match window_offset {
            Some(_) => backing_row_for_grid_row(window_offset, grid_source_row),
            None => self.source_index(slot, grid_source_row),
        }
    }

    pub fn request_cell_hydration(
        &mut self,
        slot: usize,
        source_row: usize,
        col: usize,
        purpose: crate::cell_hydration::HydratePurpose,
        cx: &mut Context<Self>,
    ) {
        if self.historical_banner.is_some() {
            return;
        }
        let Some((sql, result_set_index)) = self.result_set_sql_and_index(slot) else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let Some(truncated) = self.truncated_cell(slot, source_row, col) else {
            return;
        };
        let column_name = rs
            .columns
            .get(col)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| {
                crate::localization::format(
                    "results.hydration.unnamed_column",
                    &[("column_number", &(col + 1).to_string())],
                )
            });
        let row = match self.spill_backings.get(slot) {
            Some(Some(backing)) => backing
                .pager
                .borrow_mut()
                .page(source_row, 1)
                .into_iter()
                .next(),
            _ => self
                .result_sets
                .get(slot)
                .and_then(|rs| rs.rows.get(source_row).cloned()),
        };
        let Some(row) = row else {
            return;
        };
        cx.emit(ResultsViewEvent::HydrateCellRequested {
            result_set_index,
            columns: rs.columns.clone(),
            row,
            sql,
            column_index: col,
            column_name,
            truncated,
            purpose,
        });
    }

    /// The active result set's grid state, if a grid exists for it. This is the
    /// target of the find bar's cross-column search.
    pub(crate) fn active_grid_state(&self, cx: &App) -> Option<Entity<GridState>> {
        self.grid_state_for_slot(self.active_tab, cx)
    }

    /// Move keyboard focus to the active result grid — the editor side of the
    /// F6 editor↔grid jump. No-op until a run has produced a grid.
    pub fn focus_active_grid(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.active_grid_state(cx) {
            let handle = state.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
        }
    }

    /// The grid state of one result-set slot. Stacked and canvas layouts show
    /// every slot at once, so a gesture has to ask the grid it landed in
    /// rather than the active one.
    pub(crate) fn grid_state_for_slot(&self, slot: usize, cx: &App) -> Option<Entity<GridState>> {
        Some(self.grid_views.get(slot)?.as_ref()?.read(cx).state.clone())
    }

    /// Open the grid find bar or, if it is already open, close it and clear the
    /// highlight. Windowless so it can be called from the grid context menu's
    /// action handler (which has no `Window`); the input is created and focused
    /// on the next render.
    pub fn toggle_grid_find(&mut self, cx: &mut Context<Self>) {
        if self.grid_find.visible {
            self.close_grid_find(cx);
            return;
        }
        self.grid_find.visible = true;
        self.grid_find.focus_pending = true;
        self.apply_grid_find(cx);
        cx.notify();
    }

    /// Close the find bar and clear the search highlight from every grid — the
    /// user may have switched result-set tabs since searching, so clearing only
    /// the active grid would leave stale amber highlights on the others.
    pub fn close_grid_find(&mut self, cx: &mut Context<Self>) {
        self.grid_find.visible = false;
        for grid in self.grid_views.iter().flatten() {
            let state = grid.read(cx).state.clone();
            state.update(cx, |grid, _| grid.clear_search());
        }
        cx.notify();
    }

    fn ensure_grid_find_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid_find.input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "results.find_replace.find_placeholder",
                ))
            });
            self.grid_find.subscription = Some(cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                    InputEvent::Change => {
                        this.grid_find.query = input.read(cx).value().to_string();
                        this.apply_grid_find(cx);
                        cx.notify();
                    }
                    // Enter steps to the next match (previous is on the bar button).
                    InputEvent::PressEnter { .. } => {
                        this.grid_find_next(cx);
                        cx.notify();
                    }
                    _ => {}
                },
            ));
            self.grid_find.input = Some(input);
        }
        if self.grid_find.replace_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "results.find_replace.replace_placeholder",
                ))
            });
            self.grid_find.replace_subscription = Some(cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                    InputEvent::Change => {
                        this.grid_find.replace = input.read(cx).value().to_string();
                    }
                    // Enter in the replace field replaces the focused match.
                    InputEvent::PressEnter { .. } => {
                        this.grid_find_replace(false, cx);
                    }
                    _ => {}
                },
            ));
            self.grid_find.replace_input = Some(input);
        }
    }

    /// Push the current query into the active grid's search engine and clear
    /// it from every other grid, so switching result-set tabs never leaves
    /// stale highlights behind and always searches the grid on screen.
    fn apply_grid_find(&mut self, cx: &mut Context<Self>) {
        let query = self.grid_find.query.clone();
        let active_state = self.active_grid_state(cx);
        for grid in self.grid_views.iter().flatten() {
            let state = grid.read(cx).state.clone();
            let is_active = active_state
                .as_ref()
                .is_some_and(|active| active.entity_id() == state.entity_id());
            state.update(cx, |grid, _| {
                if is_active {
                    grid.set_search_query(&query);
                } else {
                    grid.clear_search();
                }
            });
        }
    }

    /// Move to the next / previous match in the active grid.
    pub fn grid_find_next(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.active_grid_state(cx) {
            state.update(cx, |grid, _| grid.search_next());
        }
    }

    pub fn grid_find_prev(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.active_grid_state(cx) {
            state.update(cx, |grid, _| grid.search_prev());
        }
    }

    /// The "N of M" counter for the find bar, read from the active grid.
    pub(crate) fn grid_find_count_label(&self, cx: &App) -> String {
        self.active_grid_state(cx)
            .map(|state| state.read(cx).search_count_label())
            .unwrap_or_default()
    }

    pub(crate) fn grid_find_visible(&self) -> bool {
        self.grid_find.visible
    }

    /// Number of find matches in the active grid — the Replace buttons'
    /// enable rule.
    pub(crate) fn grid_find_match_count(&self, cx: &App) -> usize {
        self.active_grid_state(cx)
            .map(|state| state.read(cx).search_match_count())
            .unwrap_or(0)
    }

    /// Replace the focused match (`all == false`) or every match
    /// (`all == true`) with the replace field's text. Replacements become
    /// staged cell edits through the normal pending-changes pipeline, so they
    /// preview and save in one transaction like manual edits. When no edit
    /// session is open for the slot yet, one is opened first (same service
    /// call the row editor makes), then the plan is applied.
    ///
    /// Coverage follows the grid search: on a spill-backed (windowed) result
    /// only the rows currently loaded are searched, and the search stops at
    /// the grid's match cap. Both limits are reported in Messages so the user
    /// knows exactly which rows a Replace All covered.
    pub fn grid_find_replace(&mut self, all: bool, cx: &mut Context<Self>) {
        if self.historical_banner.is_some() {
            return;
        }
        let slot = self.active_tab;
        // The grid searches the query exactly as typed (only an all-blank
        // query turns search off), so replace must use the same needle —
        // trimming it would rewrite text the find bar never highlighted.
        let needle = self.grid_find.query.clone();
        if needle.trim().is_empty() {
            return;
        }
        let replacement = self.grid_find.replace.clone();
        if !self.pending.is_empty() && self.pending_slot != Some(slot) {
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            let slot_one = self.pending_slot.map(|s| s + 1).unwrap_or(0);
            self.messages
                .push(pending_changes_elsewhere_message(n, slot_one));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        let Some(rs) = self.result_sets.get(slot) else {
            return;
        };
        let result_set_index = rs.index;
        let columns = rs.columns.clone();
        let Some(state) = self.active_grid_state(cx) else {
            return;
        };
        // Match cells in grid coordinates (display row, grid column) →
        // (backing row, result column index), dropping the pending marker
        // column and rows that no longer resolve. In windowed mode the grid
        // resolves display rows to resident indices; the window offset turns
        // them back into the spill rows the pager reads.
        let marker = self.marker_offset(slot);
        let (matched, coverage) = {
            let grid = state.read(cx);
            let window_offset = grid.window.map(|w| w.offset);
            let display_cells: Vec<(usize, usize)> = if all {
                grid.search_matches().to_vec()
            } else {
                grid.search_active_cell().into_iter().collect()
            };
            let matched: Vec<(usize, usize)> = display_cells
                .into_iter()
                .filter(|(_, col)| *col >= marker)
                .filter_map(|(display, col)| {
                    grid.resident_row_for_display(display).map(|resident| {
                        (
                            backing_row_for_grid_row(window_offset, resident),
                            col - marker,
                        )
                    })
                })
                .collect();
            let coverage = all
                .then(|| {
                    replace_all_coverage_note(
                        grid.window
                            .map(|w| (w.offset, grid.data.rows.len(), w.total_rows)),
                        grid.search_truncated().then(|| grid.search_match_count()),
                    )
                })
                .flatten();
            (matched, coverage)
        };
        if matched.is_empty() {
            self.messages.push(crate::localization::text(
                "results.find_replace.no_matches_to_replace",
            ));
            self.show_messages = true;
            cx.notify();
            return;
        }
        // Resolve each matched row's values once; matches against a row that
        // fails to resolve are dropped.
        let mut rows: Vec<Vec<CellValue>> = Vec::new();
        let mut index_of: std::collections::HashMap<usize, Option<usize>> =
            std::collections::HashMap::new();
        let mut local: Vec<(usize, usize)> = Vec::with_capacity(matched.len());
        for (row_index, col) in matched {
            let pos = *index_of.entry(row_index).or_insert_with(|| {
                self.source_row_for_slot(slot, row_index, cx).map(|row| {
                    rows.push(row);
                    rows.len() - 1
                })
            });
            if let Some(pos) = pos {
                local.push((pos, col));
            }
        }
        if local.is_empty() {
            self.messages.push(crate::localization::text(
                "results.find_replace.no_matches_to_replace",
            ));
            self.show_messages = true;
            cx.notify();
            return;
        }
        // A session is already open for this slot: stage immediately.
        if self.pending_slot == Some(slot)
            && let Some(session) = self.pending.current().session.clone()
        {
            self.apply_staged_replace(slot, session, rows, local, needle, replacement, cx);
            self.push_replace_coverage(coverage, cx);
            return;
        }
        // Otherwise open one (the same service call the row editor makes)
        // with the first matched row, then stage the whole plan.
        let Some(profile) = self.fk_label_profile.clone() else {
            self.messages.push(crate::localization::text(
                "results.editing.connect_to_edit_rows",
            ));
            self.show_messages = true;
            cx.notify();
            return;
        };
        let Some(services) = self.fk_label_services.clone() else {
            return;
        };
        if let Some(reason) = crate::capabilities::row_editing(profile.database_type).reason() {
            self.messages.push(reason.to_string());
            self.show_messages = true;
            cx.notify();
            return;
        }
        let request = sqlly_services::row_editing::RowEditOpenRequest {
            sql,
            result_set_index,
            columns,
            row: rows.first().cloned().unwrap_or_default(),
        };
        self.grid_find.replace_generation = self.grid_find.replace_generation.wrapping_add(1);
        let generation = self.grid_find.replace_generation;
        // The rows and match coordinates belong to this run's result: a
        // re-run while the session opens must not apply them to new columns.
        let run_generation = self.run_generation;
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let outcome = crate::tokio_handle()
                .spawn(async move { services.row_editing.open_session(&profile, request).await })
                .await;
            let mut cx = async_cx.clone();
            let _ = weak.update(&mut cx, |this, cx| {
                if this.grid_find.replace_generation != generation
                    || this.run_generation != run_generation
                {
                    return;
                }
                match outcome {
                    Ok(Ok(sqlly_services::row_editing::RowEditOpenOutcome::Editable(session))) => {
                        this.apply_staged_replace(
                            slot,
                            *session,
                            rows,
                            local,
                            needle,
                            replacement,
                            cx,
                        );
                        this.push_replace_coverage(coverage, cx);
                    }
                    Ok(Ok(sqlly_services::row_editing::RowEditOpenOutcome::NotEditable {
                        reason,
                        message,
                    })) => {
                        let reason =
                            crate::localization::stored_message_or(message.as_ref(), reason);
                        this.messages.push(reason);
                        this.show_messages = true;
                        cx.notify();
                    }
                    Ok(Err(err)) => {
                        this.messages
                            .push(crate::localization::message(err.message()));
                        this.show_messages = true;
                        cx.notify();
                    }
                    Err(join_err) => {
                        this.messages.push(join_err.to_string());
                        this.show_messages = true;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn push_replace_coverage(&mut self, coverage: Option<String>, cx: &mut Context<Self>) {
        if let Some(note) = coverage {
            self.messages.push(note);
            self.show_messages = true;
            cx.notify();
        }
    }

    /// Stage a find & replace plan: map matches to value edits (skipping
    /// read-only, non-text, and unprojected columns, rows staged for
    /// deletion, and cells holding a staged expression), then stage them as
    /// one undo step.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_staged_replace(
        &mut self,
        slot: usize,
        session: sqlly_services::row_editing::RowEditSession,
        rows: Vec<Vec<CellValue>>,
        matched: Vec<(usize, usize)>,
        needle: String,
        replacement: String,
        cx: &mut Context<Self>,
    ) {
        if !self.pending.is_empty() && self.pending_slot != Some(slot) {
            let (e, i, d) = self.pending.counts();
            let n = e + i + d;
            let slot_one = self.pending_slot.map(|s| s + 1).unwrap_or(0);
            self.messages
                .push(pending_changes_elsewhere_message(n, slot_one));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let Some(columns) = self.result_sets.get(slot).map(|rs| rs.columns.clone()) else {
            return;
        };
        // Plan against each cell's effective value: a staged edit overlays
        // the original, so replace composes with earlier edits. A cell whose
        // staged edit is an expression (DEFAULT, server UUID, …) has no text
        // to rewrite — it reads NULL here, so replace leaves the expression
        // alone instead of overwriting it with the original's text. Staged
        // edits map back to result columns through provenance only.
        let keys: Vec<Option<crate::pending_changes::RowKey>> = rows
            .iter()
            .map(|row| crate::pending_changes::row_key_from_result_row(&session, &columns, row))
            .collect();
        let effective: Vec<Vec<CellValue>> = rows
            .iter()
            .zip(&keys)
            .map(|(row, key)| {
                let mut row = row.clone();
                let Some(staged) = key
                    .as_ref()
                    .and_then(|k| self.pending.current().dirty.get(k))
                else {
                    return row;
                };
                for (column, edit) in staged {
                    let Some(cell) =
                        crate::pending_changes::result_index_for_table_column(&session, column)
                            .and_then(|ordinal| row.get_mut(ordinal))
                    else {
                        continue;
                    };
                    *cell = match edit {
                        crate::pending_changes::CellEdit::Value(value) => value.clone(),
                        crate::pending_changes::CellEdit::Expression(_) => CellValue::Null,
                    };
                }
                row
            })
            .collect();
        // Rows already staged for deletion are not rewritten: an UPDATE
        // after their DELETE would match nothing and fail the whole save.
        let mut deleted_skipped = 0usize;
        let matched: Vec<(usize, usize)> = matched
            .into_iter()
            .filter(|(row, _)| {
                let deleted = keys
                    .get(*row)
                    .and_then(|key| key.as_ref())
                    .is_some_and(|key| self.pending.current().deleted.contains(key));
                deleted_skipped += usize::from(deleted);
                !deleted
            })
            .collect();
        let edits = sqlly_services::row_editing::plan_replacements(
            &session,
            &columns,
            &effective,
            &matched,
            &needle,
            &replacement,
        );
        if edits.is_empty() {
            let message = if deleted_skipped > 0 {
                crate::localization::format(
                    "results.find_replace.no_replaceable_matches_with_deleted",
                    &[("deleted_count", &deleted_skipped.to_string())],
                )
            } else {
                crate::localization::text("results.find_replace.no_replaceable_matches")
            };
            self.messages.push(message);
            self.show_messages = true;
            cx.notify();
            return;
        }
        // Group the plan by matched row into per-row change maps; rows without
        // a usable key cannot be staged and are reported as skipped.
        let mut by_row: std::collections::BTreeMap<
            usize,
            BTreeMap<String, crate::pending_changes::CellEdit>,
        > = std::collections::BTreeMap::new();
        for edit in edits {
            by_row.entry(edit.row).or_default().insert(
                edit.column,
                crate::pending_changes::CellEdit::Value(edit.value),
            );
        }
        let mut updates = Vec::with_capacity(by_row.len());
        let mut skipped = 0usize;
        for (row_index, changes) in by_row {
            let cells = changes.len();
            let Some(key) = keys[row_index].clone() else {
                skipped += cells;
                continue;
            };
            // Staged originals are aligned with the session's table columns.
            let original =
                sqlly_services::row_editing::session_aligned_row(&session, &rows[row_index]);
            updates.push((key, original, changes));
        }
        let update_rows = updates.len();
        self.pending_slot = Some(slot);
        let staged = self.pending.stage_updates(session, updates);
        let key = match (staged > 0, skipped > 0, deleted_skipped > 0) {
            (false, false, false) => "results.find_replace.nothing_staged",
            (false, true, false) => "results.find_replace.nothing_staged_skipped",
            (false, false, true) => "results.find_replace.nothing_staged_deleted",
            (false, true, true) => "results.find_replace.nothing_staged_skipped_deleted",
            (true, false, false) => "results.find_replace.replacements_staged",
            (true, true, false) => "results.find_replace.replacements_staged_skipped",
            (true, false, true) => "results.find_replace.replacements_staged_deleted",
            (true, true, true) => "results.find_replace.replacements_staged_skipped_deleted",
        };
        let mut arguments = Vec::new();
        if staged > 0 {
            arguments.push(("replacement_count", staged.to_string()));
            arguments.push(("update_row_count", update_rows.to_string()));
        }
        if skipped > 0 {
            arguments.push(("skipped_cell_count", skipped.to_string()));
        }
        if deleted_skipped > 0 {
            arguments.push(("deleted_row_count", deleted_skipped.to_string()));
        }
        let argument_refs = arguments
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        let message = crate::localization::format(key, &argument_refs);
        self.messages.push(message);
        self.show_messages = true;
        self.schedule_preview_sql_refresh(cx);
        self.rebuild_slot_with_pending_markers(slot, cx);
        cx.notify();
    }

    /// Apply a transform macro to every staged string value (the review bar's
    /// Transform menu) and report the outcome in Messages.
    pub fn transform_staged_values(
        &mut self,
        transform: crate::cell_transform::CellTransform,
        cx: &mut Context<Self>,
    ) {
        let changed = self.pending.transform_staged_strings(transform);
        if changed == 0 {
            self.messages.push(crate::localization::format(
                "results.staged_changes.transform_no_plain_text_changes",
                &[("transform_name", &transform.localized_label())],
            ));
        } else {
            self.messages.push(crate::localization::format(
                "results.staged_changes.transform_values_updated",
                &[
                    ("transform_name", &transform.localized_label()),
                    ("changed_count", &changed.to_string()),
                ],
            ));
        }
        self.show_messages = true;
        if changed > 0 {
            self.schedule_preview_sql_refresh(cx);
            if let Some(slot) = self.pending_slot {
                self.rebuild_slot_with_pending_markers(slot, cx);
            }
        }
        cx.notify();
    }

    pub fn open_goto_column(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.active_grid_state(cx) else {
            return;
        };
        let names: Vec<String> = state
            .read(cx)
            .data
            .columns
            .iter()
            .map(|col| col.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        self.goto_column = Some(GotoColumnPopover {
            model: crate::grid_goto_column::GotoColumnModel::new(names),
            input: None,
            subscription: None,
            focus_pending: true,
        });
        cx.notify();
    }

    pub fn close_goto_column(&mut self, cx: &mut Context<Self>) {
        self.goto_column = None;
        cx.notify();
    }

    pub fn confirm_goto_column(&mut self, cx: &mut Context<Self>) {
        let Some(col) = self
            .goto_column
            .as_ref()
            .and_then(|popover| popover.model.chosen())
        else {
            self.close_goto_column(cx);
            return;
        };
        if let Some(state) = self.active_grid_state(cx) {
            state.update(cx, |s, cx| {
                s.selection = Selection::Column(col);
                // TODO(sqlly-datatable): scroll the chosen column into view.
                // As of 10.1.0 the crate has no public way to do that: its
                // `GridState::ensure_visible(row, col)` (what keyboard moves
                // use) is `pub(crate)`. Needs it made `pub` (or a
                // `scroll_column_into_view(col)` wrapper) in the crate.
                let _ = &s.focus_handle;
                cx.notify();
            });
        }
        self.close_goto_column(cx);
    }

    fn ensure_goto_column_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popover) = self.goto_column.as_mut() else {
            return;
        };
        if popover.input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.go_to_column.placeholder",
            ))
        });
        popover.subscription = Some(cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    if let Some(popover) = this.goto_column.as_mut() {
                        popover.model.query = input.read(cx).value().to_string();
                        popover.model.clamp_selection();
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => {
                    this.confirm_goto_column(cx);
                }
                _ => {}
            },
        ));
        popover.input = Some(input);
    }

    fn selected_source_rows(&self, slot: usize, cx: &App) -> Vec<usize> {
        let Some(Some(grid)) = self.grid_views.get(slot) else {
            return Vec::new();
        };
        let state = grid.read(cx).state.read(cx);
        let mut display_rows = state.selected_rows();
        if display_rows.is_empty() {
            for (row, _) in state.selected_cells() {
                display_rows.push(row);
            }
            display_rows.sort_unstable();
            display_rows.dedup();
        }
        // Backing rows, not resident indices: callers page them through
        // `source_row_for_slot`, which addresses absolute spill rows.
        let window_offset = state.window.map(|w| w.offset);
        display_rows
            .into_iter()
            .filter_map(|display_row| state.resident_row_for_display(display_row))
            .map(|resident| backing_row_for_grid_row(window_offset, resident))
            .collect()
    }

    pub fn stage_insert_from_grid(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        let row = self
            .selected_source_rows(slot, cx)
            .into_iter()
            .next()
            .unwrap_or(0);
        self.open_row_editor_stage_with(slot, row, crate::row_editor::RowEditorIntent::Insert, cx);
    }

    pub fn stage_delete_from_grid(&mut self, cx: &mut Context<Self>) {
        let slot = self.active_tab;
        let indices = self.selected_source_rows(slot, cx);
        if indices.is_empty() || self.pending.current().session.is_none() {
            self.messages.push(crate::localization::text(
                "results.staged_changes.select_row_first",
            ));
            self.show_messages = true;
            cx.notify();
            return;
        }
        let Some(session) = self.pending.current().session.clone() else {
            return;
        };
        let rows: Vec<Vec<CellValue>> = indices
            .into_iter()
            .filter_map(|index| self.source_row_for_slot(slot, index, cx))
            .collect();
        self.apply_staged_deletes(slot, session, rows, cx);
    }

    /// Create the messages text field on first use. Read-only is declared on
    /// the rendered `Textarea` (`.readonly(true)`), which keeps full contrast
    /// plus selection/copy/find while rejecting user edits.
    fn ensure_messages_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.messages_input.is_some() {
            return;
        }
        let input = cx.new(|cx| TextareaState::new(window, cx).searchable(true));
        self.messages_input = Some(input);
    }

    /// The full Messages-pane body: validation diagnostics first (formatted the
    /// way the old Diagnostics pane rendered them), then the run's execution
    /// output (PRINT, rows affected, database-context changes, errors,
    /// cancellations). Both streams live in one read-only, selectable field so a
    /// batch that returns rows *and* prints — or a query with lint warnings —
    /// shows everything in one place.
    /// The Messages pane body, in the order the run produced it: when the run
    /// started, the validation diagnostics, whatever the server said, the row
    /// count of every result set, and when the run finished.
    ///
    /// The row counts and the two clock lines are derived here rather than
    /// pushed into `self.messages`, so `messages` stays exactly "what the
    /// server told us" — which is what the tab's superscript counts and what
    /// a retained run replays.
    fn messages_pane_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if let Some(started) = self.run_started_wall {
            lines.push(crate::localization::format(
                "results.messages.query_started",
                &[("timestamp", &render::format_run_stamp(started))],
            ));
            // Two blank lines keep the start stamp visually apart from the
            // run's output, mirroring the gap before the completion stamp.
            lines.push(String::new());
            lines.push(String::new());
        }
        lines.extend(self.diagnostics.iter().map(|d| {
            let severity = crate::localization::text(match d.severity {
                crate::editor::DiagnosticSeverity::Error => "results.messages.severity_error",
                crate::editor::DiagnosticSeverity::Warning => "results.messages.severity_warning",
                crate::editor::DiagnosticSeverity::Info => "results.messages.severity_info",
                crate::editor::DiagnosticSeverity::RequireConfirmation => {
                    "results.messages.severity_confirmation"
                }
            });
            crate::localization::format(
                "results.messages.diagnostic_line",
                &[
                    ("line_number", &(d.line + 1).to_string()),
                    ("severity", &severity),
                    ("diagnostic", &d.message),
                ],
            )
        }));
        lines.extend(self.messages.iter().cloned());

        let fmt = self.formatting.clone().unwrap_or_default();
        for slot in 0..self.result_sets.len() {
            lines.push(self.result_count_label(slot, &fmt));
        }

        if let Some(completed) = self.completed_wall {
            let clock = render::format_run_stamp(completed);
            lines.push(String::new());
            lines.push(String::new());
            lines.push(match self.elapsed_ms {
                // Same integer formatting as the row counts above, so a long
                // run reads "Completed in 12,345 ms" not a digit wall.
                Some(ms) => crate::localization::format(
                    "results.messages.completed_with_elapsed_time",
                    &[
                        ("timestamp", &clock),
                        ("elapsed_ms", &fmt.format_integer(ms as i64)),
                    ],
                ),
                None => crate::localization::format(
                    "results.messages.completed_without_elapsed_time",
                    &[("timestamp", &clock)],
                ),
            });
        }
        lines.join("\n")
    }

    /// Number of lines the Messages tab summarizes in its superscript count:
    /// the combined count of validation diagnostics and execution messages.
    fn messages_pane_count(&self) -> usize {
        self.diagnostics.len() + self.messages.len()
    }

    /// Push the combined Messages body into the text field when it has moved on,
    /// preserving the user's selection where the offsets still line up.
    fn sync_messages_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text: SharedString = self.messages_pane_text().into();
        if text == self.messages_rendered {
            return;
        }
        self.messages_rendered = text.clone();
        let Some(input) = self.messages_input.clone() else {
            return;
        };
        let selection = input.read(cx).selected_range();
        let len = text.len();
        input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            if !selection.is_empty() && selection.end <= len {
                state.set_selected_range(selection, cx);
            }
        });
    }

    fn active_result_set(&self) -> Option<&ResultSet> {
        self.result_sets.get(self.active_tab)
    }

    /// Create the grid view entity for the active tab if it doesn't exist yet.
    fn ensure_grid_for_active_tab(&mut self, cx: &mut Context<Self>) {
        self.ensure_grid_for_slot(self.active_tab, cx);
        // Every result-tab activation routes through here (tab click, tab
        // cycling, lazy creation): keep an open find bar searching the grid
        // that is actually on screen.
        if self.grid_find.visible {
            self.apply_grid_find(cx);
        }
    }

    /// Create grid view entities for all result sets (used in stacked and
    /// canvas modes where every result set is visible simultaneously).
    fn ensure_all_grids(&mut self, cx: &mut Context<Self>) {
        for slot in 0..self.result_sets.len() {
            if self.grid_views.get(slot).is_some_and(|e| e.is_some()) {
                continue;
            }
            self.ensure_grid_for_slot(slot, cx);
        }
        // Stacked/canvas layouts create grids through this path rather than
        // ensure_grid_for_active_tab; an open find bar must still hook them.
        if self.grid_find.visible {
            self.apply_grid_find(cx);
        }
    }

    /// Handler for the pivot sidebar's save button: persist the live pivot
    /// layout and formats back into the query editor as a `-- sqlly pivot:`
    /// comment (replacing the last pivot directive or inserting a new one).
    fn pivot_save_config_handler(
        &self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> impl Fn(&PivotConfig, &mut gpui::App) + 'static {
        let results = cx.weak_entity();
        move |config, app| {
            let Some(results) = results.upgrade() else {
                return;
            };
            // Resolve the editor at save time, not at grid-build time: the
            // grid view is often created before the editor is attached, and
            // a captured `None` would make the save button a silent no-op.
            let (spec, editor) = {
                let results = results.read(app);
                let spec = results.result_set_at(slot).and_then(|rs| {
                    crate::result_pivot::ResultPivotSpec::from_config(config, &rs.columns, rs.index)
                });
                (spec, results.editor.clone())
            };
            let Some(spec) = spec else {
                return;
            };
            let Some(editor) = editor.as_ref().and_then(WeakEntity::upgrade) else {
                return;
            };
            let sql = editor.read(app).text().to_string();
            let updated = crate::result_pivot::update_or_insert_pivot_comment(&sql, &spec);
            editor.update(app, |editor, cx| editor.set_text(updated, cx));
        }
    }

    /// Handler for the chart sidebar's save button. Two jobs, mirroring the
    /// pivot handler: keep the live chart layout for `slot` in memory so grid
    /// rebuilds (streaming appends, spill paging, re-runs) restore it, and
    /// persist it back into the query editor as a `-- sqlly chart:` comment so
    /// it travels with the file and re-applies on the next run.
    fn chart_save_config_handler(
        &self,
        slot: usize,
        cx: &mut Context<Self>,
    ) -> impl Fn(&ChartConfig, &mut gpui::App) + 'static {
        let results = cx.weak_entity();
        move |config, app| {
            let Some(results) = results.upgrade() else {
                return;
            };
            // Same save-time editor resolution as the pivot handler: a
            // grid built before the editor attaches must not capture None.
            let (spec, editor) = {
                let this = results.read(app);
                let spec = this.result_set_at(slot).and_then(|rs| {
                    crate::result_chart::ResultChartSpec::from_config(config, &rs.columns, rs.index)
                });
                (spec, this.editor.clone())
            };
            results.update(app, |this, _cx| {
                this.chart_configs.insert(slot, config.clone());
            });
            let Some(spec) = spec else {
                return;
            };
            let Some(editor) = editor.as_ref().and_then(WeakEntity::upgrade) else {
                return;
            };
            let sql = editor.read(app).text().to_string();
            let updated = crate::result_chart::update_or_insert_chart_comment(&sql, &spec);
            editor.update(app, |editor, cx| editor.set_text(updated, cx));
        }
    }

    /// Create a grid view entity for a specific result set slot.
    fn ensure_grid_for_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.result_sets.get(slot).is_none() {
            return;
        }
        if self.grid_views.get(slot).is_some_and(|e| e.is_some()) {
            return;
        }
        self.preload_layout_sort(slot);
        self.preload_directive_sort(slot);
        let format_hints = self.format_hints.clone();
        let columns = self.result_sets[slot].columns.clone();
        let owned_rows = self.owned_rows_for_grid(slot, cx);
        let effective = self.effective_formatting_for_slot(slot);
        let mut data = build_grid_data_owned_with_formats(
            self.classification_db(),
            &columns,
            owned_rows.clone(),
            &format_hints,
            Some(&effective),
        )
        .unwrap_or_else(|_| {
            build_grid_data(self.classification_db(), &columns, &[])
                .unwrap_or_else(|_| sqlly_datatable::GridData::new(vec![], vec![]).expect("empty"))
        });
        self.apply_truncation_markers(slot, 0, &mut data.rows);
        self.apply_cell_reveals_to_data(slot, 0, &mut data, cx);
        let marker_active = self.pending_slot == Some(slot) && !self.pending.is_empty();
        if marker_active {
            data = self.overlay_pending_markers_on_grid_data(slot, &columns, data);
        }
        let tints = self.pending_row_backgrounds(slot, &data);
        let mut config = self.grid_config_for_columns(slot, &columns);
        self.apply_fk_labels_to_config(&mut config, slot, &owned_rows);
        // Overrides are per result column; the marker column shifts them.
        align_config_to_marker(&mut config, usize::from(marker_active));
        let theme = build_grid_theme();

        let weak_self = cx.weak_entity();
        // The header menu sees grid columns; the snapshot is keyed by result
        // column.
        let fk_headers = shift_fk_headers(
            self.fk_label_header_snapshot(slot),
            usize::from(marker_active),
        );
        let column_types = shift_column_types(
            columns.iter().map(|c| c.data_type.clone()).collect(),
            usize::from(marker_active),
        );
        let provider = ResultGridMenuProvider::new(weak_self, self.editor.clone(), slot)
            .with_fk_label_headers(fk_headers)
            .with_column_types(column_types)
            .with_columns(columns.clone(), usize::from(marker_active))
            .with_computed_columns(
                self.computed_plans
                    .get(slot)
                    .and_then(|p| p.as_ref())
                    .map(|p| p.columns.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default(),
            )
            .with_source_db(self.classification_db())
            .with_relay_route(self.results_via_relay());

        let configured_pivot = self.pivot_config_for_slot(slot);
        let auto_pivot = self
            .view_hints
            .as_ref()
            .is_some_and(|hints| hints.default_view == ResultViewKind::Pivot)
            && configured_pivot.is_some();
        let configured_chart = self.chart_config_for_slot(slot);
        let auto_chart = self.auto_chart_for_slot(slot);
        let (row_edit_enabled, row_action_handler) = self.row_actions_for_slot(slot, cx);
        // The theme must go through the builder: an explicit theme marks the
        // grid as host-themed. Setting `state.theme` after `build` leaves the
        // widget in follow-OS mode, where every render re-resolves its own
        // built-in palette and overwrites the app theme — the grid would
        // ignore the theme picked in Preferences.
        let mut view = SqllyDataTable::builder(data)
            .config(config)
            .context_menu_provider(provider)
            .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
            .theme(theme)
            // Row-header eye/pencil icons: View (any engine) + Edit (only when
            // the rows can be edited — see `row_edit_enabled_for_slot`).
            .row_actions(row_edit_enabled, row_action_handler)
            // Always expose the interactive Pivot tab. A query directive
            // preconfigures it; otherwise the sidebar starts empty.
            .pivot(configured_pivot.unwrap_or_default())
            .pivot_save_config(self.pivot_save_config_handler(slot, cx))
            // The Chart tab rides along the same way, configured from a saved
            // layout or a `-- sqlly chart:` directive when one exists.
            .chart(configured_chart.unwrap_or_default())
            .chart_save_config(self.chart_save_config_handler(slot, cx))
            .build(cx);
        self.apply_density_to_table(&view, cx);
        // A rebuilt spill-backed grid reads the whole file for Pivot/Chart.
        let grid_columns = view.state.read(cx).data.columns.len();
        let row_source = self.row_source_for_slot(slot, grid_columns);
        let has_row_source = row_source.is_some();
        view.set_row_source(row_source, cx);
        if has_row_source {
            view.set_row_source_drill_handler(Some(self.spill_drill_handler(slot, cx)), cx);
            self.restore_spill_drill_banner(slot, &mut view, cx);
        }
        let (locked, lock_status) = self.pivot_chart_lock_for_slot(slot, has_row_source);
        view.set_pivot_locked(locked, lock_status.clone());
        view.set_chart_locked(locked, lock_status);
        if auto_pivot && !locked {
            view.set_active_tab(GridTab::Pivot, cx);
        } else if auto_chart && !locked {
            view.set_active_tab(GridTab::Chart, cx);
        }
        let state = view.state.clone();
        self.apply_saved_layout(slot, &state, cx);
        // A rebuild of the same result (staged edits add or drop the marker
        // column) keeps the sort the user had, even with no saved layout.
        if let Some(primary) = self.sort_specs.get(slot).and_then(SortSpec::primary)
            && primary.0 < state.read(cx).data.columns.len()
            && state.read(cx).sort.is_none()
        {
            state.update(cx, |s, _| {
                s.sort = Some(primary);
                s.recompute();
            });
        }
        if !tints.is_empty() {
            state.update(cx, |s, _cx| s.set_row_backgrounds(tints));
        }
        let entity = cx.new(|_cx| view);
        cx.observe(&entity, move |this, _, cx| {
            this.maybe_page_spill(slot, cx);
            cx.notify();
        })
        .detach();
        // Scrolls (and streaming appends) notify the inner GridState entity,
        // not the wrapper — spill paging must watch the state itself. No
        // cx.notify() here: GridState notifies per batch while streaming, and
        // an unconditional view notify would reintroduce the per-batch
        // repaint storm the streaming coalescer exists to prevent.
        // `maybe_page_spill` is a no-op unless the slot is spill-backed and
        // the viewport is outside the resident window's margin.
        cx.observe(&state, move |this, _, cx| {
            this.maybe_page_spill(slot, cx);
            this.sync_sort_primary(slot, cx);
            this.note_possible_layout_change(slot, cx);
            this.sync_cell_detail(slot, cx);
            this.sync_formula_highlights(slot, cx);
        })
        .detach();
        while self.grid_views.len() <= slot {
            self.grid_views.push(None);
        }
        self.grid_views[slot] = Some(entity);
        self.apply_grid_directives(slot, &state, cx);
        self.maybe_start_fk_labels(slot, cx);
        // The initial build embedded every row currently in the result set, so
        // record that count as the sync watermark for incremental appends.
        let synced = self
            .result_sets
            .get(slot)
            .map(|r| r.row_count_hint)
            .unwrap_or(0);
        while self.synced_rows.len() <= slot {
            self.synced_rows.push(0);
        }
        self.synced_rows[slot] = synced;
        self.apply_default_saved_filter(slot, cx);
        self.auto_size_columns(slot, cx);
    }

    /// Update an existing grid's data after new rows arrive.
    ///
    /// Fast path: when the grid has no active sort or per-column filter and its
    /// `GridData` is in sync with our watermark, only the rows that arrived
    /// since the last refresh are converted and appended in place, and the
    /// `display_indices` are extended for the new rows. This is O(new rows) per
    /// batch instead of the previous O(total rows) full rebuild (which made a
    /// streaming result set O(n²) overall).
    ///
    /// Slow path: when a sort or filter is active — or the grid's row count has
    /// drifted from our watermark for any reason — the whole `GridData` is
    /// rebuilt and `set_config` re-runs the resolved-format + filter/sort
    /// recompute, preserving correctness.
    fn refresh_grid_data(&mut self, result_index: usize, cx: &mut Context<Self>) {
        self.invalidate_selection_stats();
        let slot_idx = self
            .result_sets
            .iter()
            .position(|r| r.index == result_index);
        let Some(slot_idx) = slot_idx else {
            return;
        };
        // Spill-backed slots are windowed — rows page in from the file via
        // `maybe_page_spill`, never through the streaming append path.
        if matches!(self.spill_backings.get(slot_idx), Some(Some(_))) {
            return;
        }
        if self
            .client_filter
            .get(slot_idx)
            .and_then(|f| f.as_ref())
            .is_some()
            || self
                .sort_specs
                .get(slot_idx)
                .is_some_and(|spec| !spec.secondaries().is_empty())
        {
            self.rebuild_slot_from_source(slot_idx, cx);
            return;
        }
        let Some(Some(view)) = self.grid_views.get(slot_idx) else {
            if slot_idx == self.active_tab {
                self.ensure_grid_for_active_tab(cx);
            }
            return;
        };
        let view = view.clone();

        while self.synced_rows.len() <= slot_idx {
            self.synced_rows.push(0);
        }
        let already = self.synced_rows[slot_idx];
        let rows_len = self.result_sets[slot_idx].rows.len();
        if rows_len <= already {
            return;
        }
        let pending = self.result_sets[slot_idx].rows[already..].to_vec();

        let state = view.read(cx).state.clone();
        let columns = self.result_sets[slot_idx].columns.clone();
        let format_hints = self.format_hints.clone();
        let pending_len = pending.len();
        let effective = self.effective_formatting_for_slot(slot_idx);
        let mut grid_rows = crate::grid_bridge::convert_rows_owned_with_formats(
            self.classification_db(),
            pending,
            &columns,
            &format_hints,
            Some(&effective),
        );
        self.apply_truncation_markers(slot_idx, already, &mut grid_rows);
        self.apply_cell_reveals(slot_idx, already, &mut grid_rows, cx);
        let columns_len = columns.len();

        // Fast path (v1.5.0): when the grid's row count agrees with our sync
        // watermark, append the converted batch in place via
        // `GridState::append_rows`. This is O(new rows) per batch — the
        // streaming path that lets results paint as they arrive.
        let grid_row_count = state.read(cx).data.rows.len();
        let try_fast_append =
            grid_row_count == already && grid_rows.iter().all(|row| row.len() == columns_len);
        if try_fast_append {
            let appended = state.update(cx, |s, cx| {
                let ok = s.append_rows(grid_rows).is_ok();
                if ok {
                    cx.notify();
                }
                ok
            });
            if appended {
                self.synced_rows[slot_idx] = already + pending_len;
                self.auto_size_columns(slot_idx, cx);
                return;
            }
            return self.rebuild_slot_from_source(slot_idx, cx);
        }

        self.rebuild_slot_from_source(slot_idx, cx);
    }

    fn rebuild_grid_entity(
        &mut self,
        slot_idx: usize,
        view: Entity<SqllyDataTable>,
        mut data: sqlly_datatable::GridData,
        cx: &mut Context<Self>,
    ) {
        let columns = self.result_sets.get(slot_idx).map(|rs| rs.columns.clone());
        if let Some(columns) = columns.as_ref() {
            let marker_active = self.pending_slot == Some(slot_idx) && !self.pending.is_empty();
            if marker_active {
                data = self.overlay_pending_markers_on_grid_data(slot_idx, columns, data);
            }
        }
        let state = view.read(cx).state.clone();
        let (config, sort, filters, selection, scroll_handle, theme, widths, frozen, order) = {
            let s = state.read(cx);
            (
                s.config.clone(),
                s.sort,
                s.filters.clone(),
                s.selection.clone(),
                s.scroll_handle.clone(),
                s.theme.clone(),
                s.data.columns.iter().map(|c| c.width).collect::<Vec<f32>>(),
                s.frozen_columns(),
                s.column_order().to_vec(),
            )
        };

        let weak_self = cx.weak_entity();
        let marker = usize::from(self.pending_slot == Some(slot_idx) && !self.pending.is_empty());
        let fk_headers = shift_fk_headers(self.fk_label_header_snapshot(slot_idx), marker);
        let result_columns = self
            .result_set_at(slot_idx)
            .map(|rs| rs.columns.clone())
            .unwrap_or_default();
        let column_types = shift_column_types(
            result_columns.iter().map(|c| c.data_type.clone()).collect(),
            marker,
        );
        let provider = ResultGridMenuProvider::new(weak_self, self.editor.clone(), slot_idx)
            .with_fk_label_headers(fk_headers)
            .with_column_types(column_types)
            .with_columns(result_columns, marker)
            .with_source_db(self.classification_db())
            .with_relay_route(self.results_via_relay());
        let previous_tab = view.read(cx).active_tab();
        let pivot_config = self.pivot_config_for_slot(slot_idx).unwrap_or_default();
        // Carry the old grid's theme through the builder (not a post-build
        // state write) so the rebuilt grid stays host-themed — see
        // `ensure_grid_for_slot`.
        let tints = self.pending_row_backgrounds(slot_idx, &data);
        let (row_edit_enabled, row_action_handler) = self.row_actions_for_slot(slot_idx, cx);
        let mut new_view = SqllyDataTable::builder(data)
            .config(config)
            .context_menu_provider(provider)
            .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
            .theme(theme)
            // Row-header eye/pencil icons: View (any engine) + Edit (only when
            // the rows can be edited — see `row_edit_enabled_for_slot`).
            .row_actions(row_edit_enabled, row_action_handler)
            .pivot(pivot_config)
            .pivot_save_config(self.pivot_save_config_handler(slot_idx, cx))
            .chart(self.chart_config_for_slot(slot_idx).unwrap_or_default())
            .chart_save_config(self.chart_save_config_handler(slot_idx, cx))
            .build(cx);
        let grid_columns = new_view.state.read(cx).data.columns.len();
        let row_source = self.row_source_for_slot(slot_idx, grid_columns);
        let has_row_source = row_source.is_some();
        new_view.set_row_source(row_source, cx);
        if has_row_source {
            new_view.set_row_source_drill_handler(Some(self.spill_drill_handler(slot_idx, cx)), cx);
            self.restore_spill_drill_banner(slot_idx, &mut new_view, cx);
        }
        let (locked, lock_status) = self.pivot_chart_lock_for_slot(slot_idx, has_row_source);
        new_view.set_pivot_locked(locked, lock_status.clone());
        new_view.set_chart_locked(locked, lock_status);
        if (previous_tab == GridTab::Pivot || previous_tab == GridTab::Chart) && !locked {
            new_view.set_active_tab(previous_tab, cx);
        }
        let density = self.formatting_or_load().density;
        new_view.state.update(cx, |s, cx| {
            s.sort = sort;
            s.filters = filters;
            s.selection = selection;
            s.scroll_handle = scroll_handle;
            if widths.len() == s.data.columns.len() {
                for (col, w) in s.data.columns.iter_mut().zip(widths) {
                    col.width = w;
                }
            }
            s.set_frozen_columns(frozen);
            s.set_column_order(&order);
            apply_grid_density(s, density, self.grid_zoom, cx);
            if s.sort.is_some() || s.filters.iter().any(|f| f.is_active()) {
                s.recompute();
            }
        });
        if !tints.is_empty() {
            new_view.state.update(cx, |s, _cx| {
                s.set_row_backgrounds(tints);
            });
        }
        let new_state = new_view.state.clone();
        let new_entity = cx.new(|_cx| new_view);
        cx.observe(&new_entity, move |this, _, cx| {
            this.maybe_page_spill(slot_idx, cx);
            cx.notify();
        })
        .detach();
        // Same GridState observer as `ensure_grid_for_slot` — see the note
        // there. Scroll notifies land on the state entity, not the wrapper.
        cx.observe(&new_state, move |this, _, cx| {
            this.maybe_page_spill(slot_idx, cx);
            this.sync_sort_primary(slot_idx, cx);
            this.note_possible_layout_change(slot_idx, cx);
            this.sync_cell_detail(slot_idx, cx);
        })
        .detach();
        self.grid_views[slot_idx] = Some(new_entity);

        self.synced_rows[slot_idx] = self.result_sets[slot_idx].row_count_hint;
    }

    /// Switch a result set to spill-file backing: the grid enters
    /// windowed-row mode over the on-disk set and `rs.rows` is freed.
    /// Called when the executor's spill manifest arrives (`total` = the
    /// COMPLETE authoritative count) and for the PROVISIONAL backing on the
    /// first `SpillWindow` of a still-running query (`total` = the flushed
    /// prefix; the file stays service-owned until the manifest). All reads
    /// here and downstream clamp to `total` — a growing file's on-disk index
    /// can run ahead of its flushed data, so the backing total is the only
    /// safe bound.
    fn install_spill_backing(
        &mut self,
        result_index: usize,
        spill_file: &str,
        total: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(slot) = self
            .result_sets
            .iter()
            .position(|r| r.index == result_index)
        else {
            return false;
        };
        self.register_computed_spill(slot, spill_file);
        let Some(pager) = crate::spill_pager::SpillRowPager::open(spill_file) else {
            tracing::warn!(
                spill_file,
                "spill file failed to open; keeping in-memory rows"
            );
            return false;
        };

        self.spill_install_pending = true;
        self.ensure_grid_for_slot(slot, cx);
        self.spill_install_pending = false;
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return false;
        };
        let state = view.read(cx).state.clone();

        while self.spill_backings.len() <= slot {
            self.spill_backings.push(None);
        }
        // A reinstalled backing (replacement manifest) must invalidate reads
        // spawned against the previous file.
        while self.spill_paging.len() <= slot {
            self.spill_paging.push(SpillPageState::default());
        }
        self.spill_paging[slot].invalidate();
        let backing = SpillBacking {
            pager: std::cell::RefCell::new(pager),
            total,
            path: spill_file.to_string(),
            row_source: std::cell::RefCell::new(None),
            _owned_file: None,
        };

        // Free the streamed in-memory rows — the file holds the full set.
        if let Some(rs) = self.result_sets.get_mut(slot) {
            rs.rows = Vec::new();
        }
        if let Some(synced) = self.synced_rows.get_mut(slot) {
            *synced = 0;
        }

        // Initial resident window around whatever is currently visible
        // (usually the top, but the user may have scrolled mid-stream).
        let prefs = &self.perf_prefs;
        let (first, last) = state.read(cx).visible_row_range();
        let start = first.saturating_sub(prefs.window_rows());
        let end = (last + prefs.window_rows()).min(total).max(start);
        let columns = self
            .result_sets
            .get(slot)
            .map(|result| result.columns.clone())
            .unwrap_or_default();
        let format_hints = self.format_hints.clone();
        let effective = self.effective_formatting_for_slot(slot);
        let page = backing.pager.borrow_mut().page(start, end - start);
        // The browser's pager only has cached rows; read the rest.
        let short = page.len() < end - start;
        let rows = crate::grid_bridge::convert_rows_owned_with_formats(
            self.classification_db(),
            page,
            &columns,
            &format_hints,
            Some(&effective),
        );
        let mut rows = rows;
        self.apply_truncation_markers(slot, start, &mut rows);
        state.update(cx, |s, cx| {
            if let Err(e) = s.set_row_window(total, start, rows) {
                tracing::warn!("spill window rejected: {e}");
            }
            cx.notify();
        });

        // A replacement backing ends any drill-through on the slot.
        if let Some(drill) = self.spill_drills.get_mut(slot) {
            *drill = None;
        }
        self.spill_backings[slot] = Some(backing);
        self.clear_windowed_slot_filters(slot, cx);
        // The grid is windowed from here on: the Pivot and Chart tabs read
        // the whole spill file instead of the resident rows (locked while
        // the query still streams).
        self.sync_row_source_for_slot(slot, cx);
        if short {
            self.schedule_spill_page(slot, start, end, cx);
        }
        cx.notify();
        true
    }

    /// Live growing-file paging for a suppressed set mid-query. First
    /// receipt installs a PROVISIONAL spill backing over the flushed prefix
    /// (freeing the streamed preview rows — the file holds them all); later
    /// receipts grow the virtual total in place without re-supplying rows,
    /// so scroll and selection (virtual-space) stay put. The provisional
    /// backing is NOT under the manifest guard: the executor owns the file
    /// until the final `SpillManifest`, which reinstalls the backing with
    /// the authoritative count and takes deletion ownership.
    fn handle_spill_window(
        &mut self,
        result_index: usize,
        spill_file: &str,
        rows_available: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self
            .result_sets
            .iter()
            .position(|r| r.index == result_index)
        else {
            return;
        };
        let Some(Some(backing)) = self.spill_backings.get_mut(slot) else {
            self.install_spill_backing(result_index, spill_file, rows_available, cx);
            return;
        };
        if rows_available <= backing.total {
            return; // stale/duplicate tick — rows_available is monotonic
        }
        // Reopen so the persistent pager's row-count snapshot covers the
        // grown prefix (sync consumers — selection stats, export — page
        // through it; reads stay clamped to `total`). On failure keep the
        // old pager AND the old total: growing `total` past the pager's
        // snapshot would make whole-set consumers silently truncate below
        // the advertised count. A later tick (or the manifest) grows it.
        let Some(pager) = crate::spill_pager::SpillRowPager::open(spill_file) else {
            return;
        };
        *backing.pager.borrow_mut() = pager;
        backing.total = rows_available;
        if let Some(Some(view)) = self.grid_views.get(slot) {
            let state = view.read(cx).state.clone();
            state.update(cx, |s, cx| {
                if let Some(w) = &mut s.window {
                    // Grow the virtual total in place (what `append_rows`
                    // does internally) — `set_row_window` would re-clone the
                    // resident rows per tick for no benefit.
                    w.total_rows = rows_available;
                    cx.notify();
                }
            });
        }
    }

    /// Client filters cannot run against a windowed (spill-backed) grid: drop
    /// any active filter state for `slot` and surface the limitation instead
    /// of letting a filter computed over resident rows masquerade as a
    /// whole-set filter.
    fn clear_windowed_slot_filters(&mut self, slot: usize, cx: &mut Context<Self>) {
        let had_builder_filter = self.applied_filter_models.remove(&slot).is_some();
        if slot < self.client_filter.len() {
            self.client_filter[slot] = None;
        }
        self.set_source_index_map(slot, None);
        if let Some(Some(view)) = self.grid_views.get(slot) {
            let state = view.read(cx).state.clone();
            state.update(cx, |s, cx| {
                s.filters.fill(sqlly_datatable::ColumnFilter::default());
                s.filter_panel = None;
                s.recompute();
                cx.notify();
            });
        }
        if had_builder_filter {
            let notice = self.windowed_filter_notice(slot);
            self.set_filter_limitation(slot, notice);
        }
        cx.notify();
    }

    /// Scroll-driven paging for spill-backed slots. Called from the grid
    /// observers on every grid notify (scrolls included): when the viewport
    /// comes within the configured margin of the resident window's edge, the
    /// desired window is computed here on the UI thread — this also covers
    /// scrollbar jumps to arbitrary positions — and the file read is
    /// scheduled onto the background executor via
    /// [`Self::schedule_spill_page`]; the UI thread never touches the file.
    /// The far side unloads implicitly because the new window replaces the
    /// old one.
    fn maybe_page_spill(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(Some(backing)) = self.spill_backings.get(slot) else {
            return;
        };
        let total = backing.total;
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return;
        };
        let state = view.read(cx).state.clone();
        let (first, last, offset, resident) = {
            let s = state.read(cx);
            let Some(window) = s.window else {
                return;
            };
            let (first, last) = s.visible_row_range();
            (first, last, window.offset, s.data.rows.len())
        };

        let prefs = &self.perf_prefs;
        let margin = prefs.page_margin_rows();
        let near_low = offset > 0 && first < offset.saturating_add(margin);
        let near_high = offset + resident < total && last + margin > offset + resident;
        if !near_low && !near_high {
            return;
        }
        tracing::debug!(
            slot, first, last, offset, resident, total, near_low, near_high,
        );

        // Quantize the window start to page-chunk boundaries so a steady
        // scroll pages once per chunk instead of on every notify, and the
        // loaded ranges stay stable/predictable.
        let chunk = prefs.page_chunk_rows().max(1);
        let mut start = first.saturating_sub(prefs.window_rows());
        start -= start % chunk;
        let end = (last + prefs.window_rows()).min(total).max(start);
        self.schedule_spill_page(slot, start, end, cx);
    }

    /// Records `(start, end)` as the slot's desired window and arms the
    /// coalescing debounce timer if none is armed. Bursts of requests
    /// (per-notify during a scroll) replace `pending` in place; the timer is
    /// deliberately NOT restarted per request, so a steady scroll pages once
    /// per debounce window instead of starving or spawning per pixel.
    fn schedule_spill_page(
        &mut self,
        slot: usize,
        start: usize,
        end: usize,
        cx: &mut Context<Self>,
    ) {
        while self.spill_paging.len() <= slot {
            self.spill_paging.push(SpillPageState::default());
        }
        let st = &mut self.spill_paging[slot];
        if st.read.is_some() && st.inflight == Some((start, end)) {
            tracing::debug!(slot, start, end,);
            st.pending = None;
            return;
        }
        tracing::debug!(slot, start, end, debounce_armed = st.debounce.is_some(),);
        st.pending = Some((start, end));
        if st.debounce.is_some() {
            return;
        }
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        st.debounce = Some(cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(12))
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| this.spawn_spill_page_read(slot, cx))
                .ok();
        }));
    }

    /// Debounce expiry: take the latest desired window and read it on the
    /// background executor. The reader is EPHEMERAL — opened from the spill
    /// path inside the background task and dropped as soon as the page (and
    /// its pure row conversion) is done — so no file handle outlives a
    /// single read or crosses an await afterwards. Bumping `generation`
    /// here makes any older in-flight read's apply a no-op, which is how
    /// rapid scrolls coalesce to the newest window.
    fn spawn_spill_page_read(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(st) = self.spill_paging.get_mut(slot) else {
            return;
        };
        st.debounce = None;
        let Some((start, end)) = st.pending.take() else {
            return;
        };
        if st.read.is_some() && st.inflight == Some((start, end)) {
            return;
        }
        let Some(Some(backing)) = self.spill_backings.get(slot) else {
            return; // slot lost its backing while the debounce was armed
        };
        let path = backing.path.clone();
        let total = backing.total;
        let columns = self
            .result_sets
            .get(slot)
            .map(|result| result.columns.clone())
            .unwrap_or_default();
        let format_hints = self.format_hints.clone();
        let formatting = Some(self.effective_formatting_for_slot(slot));
        let db = self.classification_db();

        let st = &mut self.spill_paging[slot];
        st.generation += 1;
        let generation = st.generation;
        st.inflight = Some((start, end));
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        st.read = Some(cx.spawn(async move |_this, _cx| {
            let rows = async_cx
                .background_executor()
                .spawn(async move {
                    // Ephemeral reader: open → page → drop within this read
                    // (in the browser, a request to the spill worker). A
                    // failed read yields None (file gone or unreadable). An
                    // empty page is Some(vec![]) — not None.
                    let page =
                        crate::spill_pager::read_spill_window(path, start, end - start).await?;
                    Some(crate::grid_bridge::convert_rows_owned_with_formats(
                        db,
                        page,
                        &columns,
                        &format_hints,
                        formatting.as_ref(),
                    ))
                })
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.apply_spill_page(slot, generation, start, total, rows, cx)
                })
                .ok();
        }));
    }

    /// Foreground apply for a completed background page read. A superseded
    /// generation is silent. A matching-generation `None` (open/IO failure)
    /// is reported in Messages once per slot until the next successful apply
    /// or invalidate — not on every scroll tick. A landing page is
    /// user-visible latency, so it notifies immediately rather than through
    /// the streaming coalescer.
    fn apply_spill_page(
        &mut self,
        slot: usize,
        generation: u64,
        start: usize,
        total: usize,
        rows: Option<Vec<Vec<sqlly_datatable::CellValue>>>,
        cx: &mut Context<Self>,
    ) {
        let rows = {
            let Some(st) = self.spill_paging.get_mut(slot) else {
                return;
            };
            match spill_read_outcome(st.generation == generation, rows.as_ref()) {
                SpillReadUi::Silent => {
                    tracing::debug!(slot, generation, cur = st.generation,);
                    return; // superseded — a newer read owns this slot's state
                }
                SpillReadUi::ReportError => {
                    st.read = None;
                    st.inflight = None;
                    if st.read_error_reported {
                        return;
                    }
                    st.read_error_reported = true;
                    None
                }
                SpillReadUi::Apply => {
                    st.read = None;
                    st.inflight = None;
                    st.read_error_reported = false;
                    rows
                }
            }
        };
        let Some(rows) = rows else {
            self.note_spill_page_read_error(cx);
            return;
        };
        let mut rows = rows;
        self.apply_truncation_markers(slot, start, &mut rows);
        let Some(Some(backing)) = self.spill_backings.get(slot) else {
            return;
        };
        // A `SpillWindow` may have grown the total while this read was in
        // flight — never shrink the virtual set back to the captured value.
        let total = total.max(backing.total);
        let Some(Some(view)) = self.grid_views.get(slot) else {
            return;
        };
        tracing::debug!(slot, generation, start, total, n = rows.len(),);
        let state = view.read(cx).state.clone();
        state.update(cx, |s, cx| {
            if let Err(e) = s.set_row_window(total, start, rows) {
                tracing::warn!("spill window rejected: {e}");
            }
            cx.notify();
        });
        cx.notify();
    }

    /// Record a failed spill-page open in Messages. No Window on this
    /// path, matching truncation's windowless channel.
    fn note_spill_page_read_error(&mut self, cx: &mut Context<Self>) {
        self.messages
            .push(crate::localization::text("results.spill.page_read_failed"));
        self.show_messages = true;
        cx.notify();
    }

    /// Invalidates every slot's async paging state — see
    /// [`SpillPageState::invalidate`]. Entries are retained (not cleared) so
    /// generations stay monotonic when a later query reuses a slot index; an
    /// in-flight apply from the previous query can therefore never match.
    fn invalidate_spill_paging(&mut self) {
        for st in &mut self.spill_paging {
            st.invalidate();
        }
        // Drills over the old result stop, and its parked full backings
        // close, before the backings are dropped and the files deleted.
        self.discard_spill_drills();
    }
}

/// Extract the column index and row range from a grid `Selection` for
/// selection-stats computation.
fn selection_column_and_rows(
    sel: &sqlly_datatable::Selection,
    row_count: usize,
) -> Option<(usize, std::ops::Range<usize>)> {
    use sqlly_datatable::Selection;
    match sel {
        // Whole-column stats scan resident rows only. Spill-backed slots keep
        // `rs.rows` empty → empty range → no stats, DELIBERATELY: paging the
        // entire file for a column selection would materialize the whole set on
        // the UI thread.
        Selection::Column(col) => Some((*col, 0..row_count)),
        Selection::Cell(row, col) => Some((*col, *row..*row + 1)),
        Selection::CellRange(r1, c1, r2, c2) => {
            let col_start = (*c1).min(*c2);
            let row_start = (*r1).min(*r2);
            let row_end = (*r1).max(*r2);
            Some((col_start, row_start..row_end + 1))
        }
        // Multi-column selections have no single stats column; single-column
        // multi-select degrades to that column over the resident rows.
        Selection::Columns(cols) => match cols.as_slice() {
            [col] => Some((*col, 0..row_count)),
            _ => None,
        },
        // Non-contiguous cells: stats only when they all share one column.
        Selection::Cells(cells) => {
            let (_, col) = *cells.first()?;
            if !cells.iter().all(|&(_, c)| c == col) {
                return None;
            }
            let rmin = cells.iter().map(|&(r, _)| r).min()?;
            let rmax = cells.iter().map(|&(r, _)| r).max()?;
            Some((col, rmin..rmax + 1))
        }
        Selection::Row(_) => None,
        Selection::RowRange(_, _) => None,
        Selection::Rows(_) => None,
        Selection::None => None,
    }
}

/// Add a `ResultKind::Mask` override for every accepted-suggestion column,
/// minus the columns the user has temporarily revealed. Explicit user
/// overrides always win, mirroring `effective_formatting`'s rule injection.
pub(crate) fn inject_suggested_masks(
    fmt: &mut ResultFormattingPreferences,
    columns: &[ResultColumn],
    accepted: &HashSet<usize>,
    revealed: &HashSet<usize>,
) {
    for &index in accepted {
        if revealed.contains(&index) {
            continue;
        }
        if let Some(column) = columns.get(index) {
            fmt.column_overrides
                .entry(column.name.clone())
                .or_insert_with(|| ColumnFormat::default_for_kind(ResultKind::Mask));
        }
    }
}
