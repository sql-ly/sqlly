//! "Top Values…" — a clickable frequency list for one result column.
//!
//! The header menu gathers the column's cells — the rows currently visible
//! for a resident set, the first 100,000 rows read from disk for a windowed
//! (spill-backed) one, read and counted on the background executor while the
//! window shows "Counting…" — [`top_column_values`] counts them, and this window
//! lists the most common values with counts. Clicking a value applies an
//! equals quick-filter; a NULL row (when nulls exist) applies IS NULL. The
//! window is bound to the result it was opened on: after a re-run it stops
//! filtering (the column at that position may be a different column now).
//! The aggregation is pure and unit-tested.

use gpui::{
    App, Bounds, Context, MouseButton, Render, ScrollHandle, SharedString, Styled, TitlebarOptions,
    WeakEntity, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use sqlly_datatable::filter::uses_number_ops;
use sqlly_datatable::format::format_cell;
use sqlly_datatable::{CellValue as GridCell, ColumnKind, ResolvedColumnFormat};

use crate::grid_filter_builder::{BuilderOp, FilterRow};
use crate::results::ResultsView;
use crate::theme::Theme;

/// Rows shown in the window (HeidiSQL's "More values" shows 30).
pub const TOP_VALUES_CAP: usize = 30;

/// One row of the list: what the grid shows, how often it occurs, and the
/// sampled cell a click filters on.
#[derive(Clone, Debug, PartialEq)]
pub struct TopValueEntry {
    pub display: String,
    pub count: usize,
    pub sample: GridCell,
}

/// Count `cells` into the top `cap` values by frequency. NULL cells are
/// counted separately (the caller renders a NULL row with an IS NULL filter)
/// and never appear in the entries. Pure, so it is unit-testable.
pub fn top_column_values(
    cells: &[GridCell],
    fmt: &ResolvedColumnFormat,
    cap: usize,
) -> (Vec<TopValueEntry>, usize) {
    let mut nulls = 0usize;
    let mut counts: std::collections::HashMap<String, (GridCell, usize)> =
        std::collections::HashMap::new();
    for cell in cells {
        let Some(key) = frequency_key(cell) else {
            nulls += 1;
            continue;
        };
        let entry = counts.entry(key).or_insert_with(|| (cell.clone(), 0));
        entry.1 += 1;
    }
    let mut entries: Vec<TopValueEntry> = counts
        .into_values()
        .map(|(sample, count)| TopValueEntry {
            display: format_cell(&sample, fmt).0,
            count,
            sample,
        })
        .collect();
    entries.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.display.cmp(&b.display))
    });
    entries.truncate(cap);
    (entries, nulls)
}

fn frequency_key(cell: &GridCell) -> Option<String> {
    match cell {
        GridCell::None => None,
        GridCell::Boolean(b) => Some(format!("bool:{b}")),
        GridCell::Integer(i) => Some(format!("int:{i}")),
        GridCell::Decimal(d) => Some(format!("dec:{}", d.to_bits())),
        GridCell::ExactDecimal(d) => Some(format!("exact:{d}")),
        GridCell::Text(s) => Some(format!("str:{s}")),
        GridCell::Date(t) => Some(format!("date:{t}")),
    }
}

/// The quick-filter row a click applies. Text cells filter with a text
/// equals against the displayed value (that is what the grid compares);
/// native number/date cells filter with a numeric equals on the raw value.
pub fn filter_row_for_entry(column: usize, kind: ColumnKind, entry: &TopValueEntry) -> FilterRow {
    let numeric = uses_number_ops(kind) && !matches!(entry.sample, GridCell::Text(_));
    let mut row = FilterRow::new(
        column,
        if numeric {
            BuilderOp::Eq
        } else {
            BuilderOp::Is
        },
    );
    row.a = match &entry.sample {
        GridCell::Integer(i) if numeric => i.to_string(),
        GridCell::Decimal(d) if numeric => d.to_string(),
        GridCell::ExactDecimal(d) if numeric => d.to_string(),
        GridCell::Date(t) if numeric => t.to_string(),
        _ => entry.display.clone(),
    };
    row
}

/// Where a Top Values window's cells came from, and what a click filters.
pub struct TopValuesSource {
    pub results: WeakEntity<ResultsView>,
    pub slot: usize,
    /// [`ResultsView::result_epoch`] when the cells were read; a click on a
    /// window whose result has since been replaced does nothing.
    pub epoch: u64,
    pub column: usize,
    pub kind: ColumnKind,
    pub column_name: String,
    pub fmt: ResolvedColumnFormat,
    /// Rows the column has in the scanned scope (visible rows, or the whole
    /// windowed set); more than `cells.len()` when the scan was capped.
    pub total_rows: usize,
}

/// The window's header line: how many rows were counted, and — when the
/// scan was capped — that the counts cover only the first rows.
pub fn top_values_footer(counted: usize, total_rows: usize) -> String {
    if total_rows > counted {
        crate::localization::format(
            "results.top_values.sampled_footer",
            &[
                ("counted_rows", &counted.to_string()),
                ("total_rows", &total_rows.to_string()),
            ],
        )
    } else {
        crate::localization::format(
            "results.top_values.complete_footer",
            &[("counted_rows", &counted.to_string())],
        )
    }
}

/// The cells a Top Values window counts.
pub enum TopValuesCells {
    /// Already in memory (a resident result's visible rows), capped.
    Ready(Vec<GridCell>),
    /// A windowed result: result column `column` of the first `count` rows
    /// of the spill file at `path`, read and counted on the background
    /// executor while the window says it is counting.
    Spill {
        path: String,
        column: usize,
        count: usize,
    },
}

/// Footer while a windowed result is still being read.
pub fn top_values_counting_footer(count: usize) -> String {
    crate::localization::format(
        "results.top_values.counting_footer",
        &[("row_count", &count.to_string())],
    )
}

/// Count a column read from a spill file: the cells are converted exactly as
/// the synchronous path did (raw core values, no display formatting), so the
/// result is identical. Pure; runs on the background executor.
fn count_spill_cells(
    cells: Vec<sqlly_core::CellValue>,
    fmt: &ResolvedColumnFormat,
) -> (Vec<TopValueEntry>, usize, usize) {
    let counted = cells.len();
    let cells: Vec<GridCell> = cells
        .into_iter()
        .map(crate::grid_bridge::convert_cell_owned)
        .collect();
    let (entries, nulls) = top_column_values(&cells, fmt, TOP_VALUES_CAP);
    (entries, nulls, counted)
}

/// Open the Top Values window for one column. Ready cells are counted at
/// once; spill cells are counted in the background (see [`TopValuesCells`]).
pub fn open_top_values_window(source: TopValuesSource, cells: TopValuesCells, cx: &mut App) {
    let (entries, nulls, footer, pending) = match cells {
        TopValuesCells::Ready(cells) => {
            let (entries, nulls) = top_column_values(&cells, &source.fmt, TOP_VALUES_CAP);
            let footer = top_values_footer(cells.len(), source.total_rows);
            (entries, nulls, footer, None)
        }
        TopValuesCells::Spill {
            path,
            column,
            count,
        } => (
            Vec::new(),
            0,
            top_values_counting_footer(count),
            Some((path, column, count)),
        ),
    };
    let title: SharedString = crate::localization::format(
        "results.top_values.window_title",
        &[("column_name", &source.column_name)],
    )
    .into();
    let bounds = Bounds::centered(None, size(px(420.), px(480.)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        move |window, cx| {
            let column_name = source.column_name.clone();
            crate::localization::register_window_title(window.window_handle(), move || {
                crate::localization::format(
                    "results.top_values.window_title",
                    &[("column_name", &column_name)],
                )
            });
            let view = cx.new(|cx| {
                let mut view = TopValuesView {
                    footer,
                    source,
                    entries,
                    nulls,
                    stale: false,
                    counting: None,
                    scroll: ScrollHandle::new(),
                };
                if let Some((path, column, count)) = pending {
                    view.start_spill_count(path, column, count, cx);
                }
                view
            });
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        },
    );
}

struct TopValuesView {
    source: TopValuesSource,
    entries: Vec<TopValueEntry>,
    nulls: usize,
    footer: String,
    /// The result was replaced after the window opened; clicks are inert.
    stale: bool,
    /// The background read + count of a windowed result, while it runs.
    /// Dropping the window drops (cancels) it.
    counting: Option<gpui::Task<()>>,
    scroll: ScrollHandle,
}

impl TopValuesView {
    /// Read and count a windowed result's column off the UI thread. The
    /// result applies only if the results view still shows the result the
    /// window was opened on (the epoch is the generation token).
    fn start_spill_count(
        &mut self,
        path: String,
        column: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        let fmt = self.source.fmt.clone();
        self.counting = Some(cx.spawn(async move |this, cx| {
            let counted = cx
                .background_executor()
                .spawn(async move {
                    crate::results::read_spill_column(&path, column, count)
                        .await
                        .map(|cells| count_spill_cells(cells, &fmt))
                })
                .await;
            let _ = this.update(cx, |this, cx| this.finish_spill_count(counted, cx));
        }));
    }

    fn finish_spill_count(
        &mut self,
        counted: Option<(Vec<TopValueEntry>, usize, usize)>,
        cx: &mut Context<Self>,
    ) {
        self.counting = None;
        let current = self
            .source
            .results
            .upgrade()
            .is_some_and(|results| results.read(cx).result_epoch() == self.source.epoch);
        if !current {
            self.stale = true;
        } else {
            match counted {
                Some((entries, nulls, counted)) => {
                    self.entries = entries;
                    self.nulls = nulls;
                    self.footer = top_values_footer(counted, self.source.total_rows);
                }
                None => {
                    self.footer = crate::localization::text("results.top_values.read_error");
                }
            }
        }
        cx.notify();
    }

    fn apply(&mut self, row: FilterRow, window: &mut Window, cx: &mut Context<Self>) {
        if self.counting.is_some() {
            return;
        }
        let Some(results) = self.source.results.upgrade() else {
            self.stale = true;
            cx.notify();
            return;
        };
        if results.read(cx).result_epoch() != self.source.epoch {
            self.stale = true;
            cx.notify();
            return;
        }
        let slot = self.source.slot;
        results.update(cx, |rv, cx| rv.apply_quick_filter(slot, row, cx));
        window.remove_window();
    }

    fn value_row(
        &self,
        id: usize,
        display: String,
        count: usize,
        row: FilterRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .id(("top-value", id))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(10.0)))
            .h(px(s(26.0)))
            .rounded(px(s(4.0)))
            .cursor_pointer()
            .hover(|style| style.bg(Theme::row_hover()))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .font_family(Theme::editor_font_family())
                    .text_color(Theme::text())
                    .child(crate::result_profiler::single_line(&display)),
            )
            .child(div().flex_none().text_color(Theme::text_dim()).child(
                crate::localization::format(
                    "results.top_values.value_count",
                    &[("count", &count.to_string())],
                ),
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, window, cx| {
                    this.apply(row.clone(), window, cx);
                }),
            )
    }
}

impl Render for TopValuesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let mut list = div()
            .id("top-values-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .p(px(s(6.0)));
        if self.nulls > 0 {
            let mut row = FilterRow::new(self.source.column, BuilderOp::IsNull);
            row.enabled = true;
            list = list.child(self.value_row(0, "NULL".to_string(), self.nulls, row, cx));
        }
        if self.entries.is_empty() && self.nulls == 0 && self.counting.is_none() {
            list = list.child(
                div()
                    .px(px(s(10.0)))
                    .py(px(s(6.0)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("results.top_values.empty_values")),
            );
        }
        let offset = usize::from(self.nulls > 0);
        for (i, entry) in self.entries.iter().enumerate() {
            let row = filter_row_for_entry(self.source.column, self.source.kind, entry);
            list =
                list.child(self.value_row(i + offset, entry.display.clone(), entry.count, row, cx));
        }
        let footer = if self.stale {
            crate::localization::text("results.top_values.stale_results")
        } else {
            self.footer.clone()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Theme::panel_bg())
            .child(
                div()
                    .px(px(s(10.0)))
                    .py(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(footer),
            )
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_datatable::config::{
        BooleanFormat, DateFormat, NullFormat, NumberFormat, ReplacementTiming, StringFormat,
    };

    fn fmt(kind: ColumnKind) -> ResolvedColumnFormat {
        ResolvedColumnFormat {
            kind,
            number: NumberFormat::default(),
            date: DateFormat::default(),
            boolean: BooleanFormat::default(),
            string: StringFormat::default(),
            null: NullFormat::default(),
            replacements: vec![],
            replacement_timing: ReplacementTiming::AfterFormat,
            value_labels: None,
            label_mode: sqlly_datatable::config::LabelMode::Beside,
        }
    }

    #[test]
    fn background_spill_count_matches_the_synchronous_count() {
        let core = vec![
            sqlly_core::CellValue::String("b".into()),
            sqlly_core::CellValue::Null,
            sqlly_core::CellValue::I64(7),
            sqlly_core::CellValue::String("b".into()),
        ];
        let sync_cells: Vec<GridCell> = core
            .iter()
            .cloned()
            .map(crate::grid_bridge::convert_cell_owned)
            .collect();
        let fmt = fmt(ColumnKind::Text);
        let (entries, nulls) = top_column_values(&sync_cells, &fmt, TOP_VALUES_CAP);
        assert_eq!(count_spill_cells(core, &fmt), (entries, nulls, 4));
    }

    #[test]
    fn top_values_rank_by_count_then_name() {
        let cells = vec![
            GridCell::Text("b".into()),
            GridCell::Text("a".into()),
            GridCell::Text("b".into()),
            GridCell::Text("c".into()),
            GridCell::Text("a".into()),
            GridCell::Text("b".into()),
        ];
        let (entries, nulls) = top_column_values(&cells, &fmt(ColumnKind::Text), 10);
        assert_eq!(nulls, 0);
        let ranked: Vec<(&str, usize)> = entries
            .iter()
            .map(|e| (e.display.as_str(), e.count))
            .collect();
        assert_eq!(ranked, [("b", 3), ("a", 2), ("c", 1)]);
    }

    #[test]
    fn top_values_counts_nulls_separately() {
        let cells = vec![
            GridCell::None,
            GridCell::Text("x".into()),
            GridCell::None,
            GridCell::Integer(7),
        ];
        let (entries, nulls) = top_column_values(&cells, &fmt(ColumnKind::Text), 10);
        assert_eq!(nulls, 2);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn top_values_truncates_to_cap() {
        let cells: Vec<GridCell> = (0..100).map(GridCell::Integer).collect();
        let (entries, _) = top_column_values(&cells, &fmt(ColumnKind::Integer), 30);
        assert_eq!(entries.len(), 30);
        assert!(entries.iter().all(|e| e.count == 1));
    }

    #[test]
    fn filter_row_uses_text_equals_for_text_cells_even_on_date_columns() {
        let cells = vec![GridCell::Text("2026-09-28".into())];
        let (entries, _) = top_column_values(&cells, &fmt(ColumnKind::Date), 10);
        let row = filter_row_for_entry(2, ColumnKind::Date, &entries[0]);
        assert_eq!(row.op, BuilderOp::Is);
        assert_eq!(row.a, "2026-09-28");
        assert_eq!(
            row.column,
            crate::grid_filter_builder::ColumnTarget::Index(2)
        );
    }

    #[test]
    fn text_equals_uses_the_display_text_even_for_numeric_cells() {
        // A number cell in a text-kind column filters with `is` on what the
        // grid shows, not on the raw value.
        let cells = vec![GridCell::Integer(7)];
        let (entries, _) = top_column_values(&cells, &fmt(ColumnKind::Text), 10);
        let row = filter_row_for_entry(0, ColumnKind::Text, &entries[0]);
        assert_eq!(row.op, BuilderOp::Is);
        assert_eq!(row.a, entries[0].display);
    }

    #[test]
    fn footer_says_when_counts_cover_only_the_first_rows() {
        let capped = top_values_footer(100_000, 250_000);
        assert!(capped.contains("first 100000 of 250000"), "{capped}");
        let whole = top_values_footer(42, 42);
        assert!(whole.starts_with("42 rows counted"), "{whole}");
        assert!(!whole.contains("first"), "{whole}");
    }

    #[test]
    fn filter_row_uses_numeric_equals_for_number_cells() {
        let cells = vec![GridCell::Integer(42)];
        let (entries, _) = top_column_values(&cells, &fmt(ColumnKind::Integer), 10);
        let row = filter_row_for_entry(1, ColumnKind::Integer, &entries[0]);
        assert_eq!(row.op, BuilderOp::Eq);
        assert_eq!(row.a, "42");
        assert!(row.enabled);
    }
}
