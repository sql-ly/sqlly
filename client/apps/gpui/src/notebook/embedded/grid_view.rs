//! The one place that tells a results grid (`SqllyDataTable`) which of its
//! own chrome a notebook cell wants. Embedded, a grid has no tab bar, no
//! pivot or chart panel and cannot switch views; a cell locked to rows,
//! pivot or chart stays on that view in full mode too. The grid keeps its
//! honesty notes (partial, capped, filtered, locked) whatever this asks.
//!
//! A grid that cannot switch to its rows sends a pivot drill or chart mark
//! click here instead of filtering itself; [`DrillRows`] shows the rows
//! behind it in a small grid under the cell's output.

use std::collections::HashSet;

use gpui::{
    AnyElement, App, AppContext as _, Entity, EntityId, IntoElement, ParentElement as _,
    Styled as _, div, px,
};
use sqlly_datatable::{
    GridConfig, GridData, GridTab, RowSourceDrill, RowSourceDrillHandler, SidebarMode,
    SqllyDataTable,
};
use sqlly_services::notebook::display::LockedView;

use crate::theme::Theme;

/// Row-count-fitted grids stay between these heights, in logical pixels.
pub const FIT_MIN: f32 = 80.0;
pub const FIT_MAX: f32 = 420.0;
/// The grid's bottom scroll bar and status strip.
const GRID_CHROME: f32 = 20.0;

/// What a cell asks of its grid's chrome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GridChrome {
    /// Embedded: no tab bar, no pivot or chart panel, no switching.
    pub bare: bool,
    /// The view the grid stays on.
    pub lock: Option<LockedView>,
}

impl GridChrome {
    fn is_default(self) -> bool {
        !self.bare && self.lock.is_none()
    }
}

pub fn tab_for(lock: LockedView) -> GridTab {
    match lock {
        LockedView::Rows => GridTab::Grid,
        LockedView::Pivot => GridTab::Pivot,
        LockedView::Chart => GridTab::Chart,
    }
}

/// Bring `grid` in line with `chrome`; `touched` remembers the grids this
/// changed, so they get their own chrome back when the cell asks for none.
/// `drill` handles drills while the grid cannot show its own rows. Cheap
/// when nothing changes, so cells call it on every draw.
pub fn apply(
    grid: &Entity<SqllyDataTable>,
    chrome: GridChrome,
    touched: &mut HashSet<EntityId>,
    drill: impl FnOnce() -> RowSourceDrillHandler,
    cx: &mut App,
) {
    let id = grid.entity_id();
    if chrome.is_default() && !touched.remove(&id) {
        return;
    }
    if !chrome.is_default() {
        touched.insert(id);
    }
    let locked = !chrome.is_default();
    // A spill-backed grid already sends its drills to the results view.
    let own_drill = grid.read(cx).row_source().is_none();
    let drill = (locked && own_drill).then(drill);
    grid.update(cx, |g, cx| {
        let mut changed = false;
        if let Some(lock) = chrome.lock {
            let tab = tab_for(lock);
            if g.active_tab() != tab {
                g.set_active_tab(tab, cx);
                changed = true;
            }
        }
        if g.tab_bar_visible() == chrome.bare {
            g.set_tab_bar_visible(!chrome.bare);
            changed = true;
        }
        if g.tab_switching() == locked {
            if own_drill {
                g.set_row_source_drill_handler(drill, cx);
            }
            g.set_tab_switching(!locked, cx);
            changed = true;
        }
        let want = |mode: SidebarMode| match mode {
            _ if chrome.bare => SidebarMode::Hidden,
            SidebarMode::Hidden => SidebarMode::Expanded,
            mode => mode,
        };
        let pivot = want(g.pivot_sidebar_mode());
        if pivot != g.pivot_sidebar_mode() {
            g.set_pivot_sidebar_mode(pivot);
            changed = true;
        }
        let chart = want(g.chart_sidebar_mode());
        if chart != g.chart_sidebar_mode() {
            g.set_chart_sidebar_mode(chart);
            changed = true;
        }
        if changed {
            cx.notify();
        }
    });
}

/// A height that shows every row of a short result and caps a long one.
pub fn fit_height(grid: &Entity<SqllyDataTable>, cx: &App) -> f32 {
    let state = grid.read(cx).state.read(cx);
    let rows = state.display_row_count() as f32;
    (state.header_height + rows * state.row_height + GRID_CHROME).clamp(FIT_MIN, FIT_MAX)
}

/// The rows behind a pivot value or chart mark of a grid that cannot show
/// them itself (embedded or locked), in a small grid of their own.
pub struct DrillRows {
    pub label: String,
    pub count: usize,
    pub grid: Entity<SqllyDataTable>,
}

impl DrillRows {
    /// The rows of `source` that `drill` keeps; `None` when the grid holds
    /// only a window of its rows.
    pub fn of(
        source: &Entity<SqllyDataTable>,
        drill: &RowSourceDrill,
        cx: &mut App,
    ) -> Option<Self> {
        let (columns, rows) = {
            let state = source.read(cx).state.read(cx);
            if state.window.is_some() {
                return None;
            }
            let rows: Vec<_> = state
                .data
                .rows
                .iter()
                .filter(|row| drill.matches(row))
                .cloned()
                .collect();
            (state.data.columns.clone(), rows)
        };
        let count = rows.len();
        let table = SqllyDataTable::builder(GridData { columns, rows })
            .config(GridConfig::default())
            .theme(crate::grid_bridge::build_grid_theme())
            .available_tabs(&[GridTab::Grid])
            .tab_bar(false)
            .build(cx);
        Some(Self {
            label: drill.label().to_string(),
            count,
            grid: cx.new(|_| table),
        })
    }

    /// The caption and the grid; `close` is the caller's Close button.
    pub fn render(&self, close: impl IntoElement, cx: &App) -> AnyElement {
        let s = Theme::scaled;
        let height = fit_height(&self.grid, cx);
        let count = crate::license::format_thousands(self.count as u64);
        div()
            .flex()
            .flex_col()
            .w_full()
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(10.0)))
                    .py(px(s(3.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "notebook.embedded.drill.title",
                        &[
                            ("selection", self.label.as_str()),
                            ("row_count", count.as_str()),
                        ],
                    ))
                    .child(div().flex_1())
                    .child(close),
            )
            .child(
                div()
                    .h(px(height))
                    .w_full()
                    .min_h_0()
                    .child(self.grid.clone()),
            )
            .into_any_element()
    }
}
