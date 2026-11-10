//! Map view for locations in results: Redis GEO replies, longitude/latitude
//! columns, and WKT/WKB geometry, drawn over OpenStreetMap tiles (after the
//! user agrees) or a plain sketch, with an optional heatmap.
//!
//! Opened from the results grid's "Show on Map…" action, from the value
//! viewer's Map mode, and automatically when a query's `-- sqlly map:`
//! directive says `open=auto` (or its first line is `-- sqlly view: map`).
//! Pure pieces (projection, column detection, the directive, heatmap,
//! clustering, tile scheduling) live in their own modules with unit tests.

pub mod directive;
pub mod features;
pub mod heatmap;
mod panel;
pub mod projection;
pub mod tiles;
pub mod view;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Bounds, Entity, EntityId, SharedString, TitlebarOptions, WeakEntity,
    WindowBounds, WindowOptions, px, size,
};
use sqlly_core::spatial::Geometry;
use sqlly_core::{CellValue, ResultColumn};

use crate::results::ResultsView;
pub use directive::MapDirective;
pub use features::MapData;
pub use view::{MapContent, MapHooks, MapView, RowSelectHook, WriteHook};

/// A map entity for one geometry value (the value viewer's Map mode).
pub fn value_map(geometry: &Geometry, label: &str) -> MapView {
    MapView::new(
        label.to_string(),
        MapContent::Value(MapData::from_geometry(geometry, label)),
        None,
        MapHooks::default(),
    )
}

/// The value viewer's Map-mode body. The map entity lives in the viewer's
/// element state, so pan and zoom survive re-renders and switching modes
/// back and forth starts fresh.
pub fn viewer_map_body<V: 'static>(
    geometry: Option<Arc<Geometry>>,
    label: &str,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<V>,
) -> gpui::AnyElement {
    use gpui::{IntoElement as _, ParentElement as _, Styled as _};
    let Some(geometry) = geometry else {
        return gpui::div().flex_1().into_any_element();
    };
    let label = label.to_string();
    let map = window.use_keyed_state("structured-data-map", cx, move |_, _| {
        value_map(&geometry, &label)
    });
    gpui::div()
        .flex_1()
        .min_h_0()
        .size_full()
        .child(map)
        .into_any_element()
}

thread_local! {
    /// Open result maps, by (results view, result slot), so a re-run
    /// refreshes the window that is already showing instead of opening
    /// another.
    static OPEN_MAPS: RefCell<HashMap<(EntityId, usize), WeakEntity<MapView>>> =
        RefCell::new(HashMap::new());
}

/// The map settings for result `slot` of `sql`: its `-- sqlly map` lines,
/// merged. A first-line `-- sqlly view: map` with no map line for the
/// first result means "map it, opened automatically".
pub fn directive_for(sql: &str, slot: usize) -> Option<MapDirective> {
    let all = directive::parse(sql);
    let mut d = MapDirective::for_slot(&all, slot);
    if slot == 0 && directive::view_is_map(sql) {
        let d = d.get_or_insert_with(MapDirective::default);
        d.open.get_or_insert(true);
    }
    d
}

/// Open (or refresh) the map window for result `slot` of `results`.
pub fn open_for_result(results: &Entity<ResultsView>, slot: usize, cx: &mut App) {
    let (columns, rows, sql) = {
        let rv = results.read(cx);
        let Some(rs) = rv.result_set_at(slot) else {
            return;
        };
        (
            rs.columns.clone(),
            rs.rows.clone(),
            rv.query_sql().unwrap_or_default().to_string(),
        )
    };
    let directive = directive_for(&sql, slot);
    let hooks = result_hooks(results.downgrade(), slot);
    let title = crate::localization::format("map_view.result_title", &[("number", slot + 1)]);
    let key = (results.entity_id(), slot);
    let existing = OPEN_MAPS.with(|maps| maps.borrow().get(&key).and_then(|w| w.upgrade()));
    let sources = features::map_sources(&columns, &rows);
    let content = MapContent::Rows {
        columns,
        rows: Arc::new(rows),
        sources,
    };
    if let Some(map) = existing {
        map.update(cx, |m, cx| {
            m.replace_content(content, directive);
            cx.notify();
        });
        return;
    }
    if let Some(map) = open_window(&title, content, directive, hooks, cx) {
        OPEN_MAPS.with(|maps| {
            let mut maps = maps.borrow_mut();
            maps.retain(|_, w| w.upgrade().is_some());
            maps.insert(key, map.downgrade());
        });
    }
}

/// After a run: open (or refresh) a map for every result whose settings
/// say `open=auto`.
pub fn auto_open(results: &Entity<ResultsView>, cx: &mut App) {
    let (sql, count) = {
        let rv = results.read(cx);
        (
            rv.query_sql().unwrap_or_default().to_string(),
            rv.result_set_count(),
        )
    };
    if !sql.to_ascii_lowercase().contains("sqlly") {
        return;
    }
    for slot in 0..count {
        if directive_for(&sql, slot).is_some_and(|d| d.open == Some(true)) {
            open_for_result(results, slot, cx);
        }
    }
}

/// Select-a-row and write-to-query hooks for result `slot`.
fn result_hooks(results: WeakEntity<ResultsView>, slot: usize) -> MapHooks {
    let for_select = results.clone();
    let select = grid_row_selector(move |cx| {
        for_select
            .upgrade()
            .and_then(|rv| rv.read(cx).grid_state_for_slot(slot, cx))
    });
    let write: WriteHook = Rc::new(move |mut settings: MapDirective, cx: &mut App| {
        let Some(rv) = results.upgrade() else {
            return;
        };
        rv.update(cx, |view, cx| {
            settings.index = (view.result_set_count() > 1).then_some(slot + 1);
            let mut snapshot = view.lock_snapshot(slot, cx);
            snapshot
                .lines
                .retain(|line| !directive::is_map_line_for(line, slot));
            snapshot.lines.push(directive::directive_line(&settings));
            cx.emit(crate::results::ResultsViewEvent::LockRequested(snapshot));
        });
    });
    MapHooks {
        select: Some(select),
        write: Some(write),
    }
}

fn open_window(
    title: &str,
    content: MapContent,
    directive: Option<MapDirective>,
    hooks: MapHooks,
    cx: &mut App,
) -> Option<Entity<MapView>> {
    let window_title = crate::localization::format("map_view.window_title", &[("title", title)]);
    let bounds = Bounds::centered(None, size(px(960.), px(660.)), cx);
    let title = title.to_string();
    let mut content = Some(content);
    let created: Rc<RefCell<Option<Entity<MapView>>>> = Rc::new(RefCell::new(None));
    let slot = created.clone();
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(window_title)),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        move |window, cx| {
            let content = content
                .take()
                .unwrap_or(MapContent::Value(MapData::default()));
            let map =
                cx.new(|_| MapView::new(title.clone(), content, directive.clone(), hooks.clone()));
            *slot.borrow_mut() = Some(map.clone());
            cx.new(|cx| gpui_component::Root::new(map, window, cx))
        },
    );
    created.borrow_mut().take()
}

/// A row-select hook that selects (and scrolls to) a result row in a grid.
/// `row` is the result-set row; rows outside a windowed grid's loaded
/// window are ignored.
pub fn grid_row_selector(
    grid: impl Fn(&mut App) -> Option<gpui::Entity<sqlly_datatable::GridState>> + 'static,
) -> RowSelectHook {
    Rc::new(move |row, cx| {
        let Some(state) = grid(cx) else {
            return;
        };
        state.update(cx, |s, cx| {
            let offset = s.window.map(|w| w.offset).unwrap_or(0);
            let Some(resident) = row.checked_sub(offset) else {
                return;
            };
            let Some(display) = s.display_indices.iter().position(|&r| r == resident) else {
                return;
            };
            s.selection = sqlly_datatable::Selection::Row(display);
            let view_h = f32::from(s.bounds.size.height) - s.header_height;
            let total = s.display_row_count() as f32 * s.row_height;
            let max_y = (total - view_h).max(0.0);
            let y = (display as f32 * s.row_height - view_h / 2.0).clamp(0.0, max_y);
            let x = s.scroll_handle.offset().x;
            s.scroll_handle.set_offset(gpui::point(x, px(y)));
            cx.notify();
        });
    })
}

/// Open a map over rows that are not a live result (the pivot grid): no
/// directive, no write-back, and no row selection.
pub fn open_rows_map(
    title: &str,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    cx: &mut App,
) {
    let sources = features::map_sources(&columns, &rows);
    let content = MapContent::Rows {
        columns,
        rows: Arc::new(rows),
        sources,
    };
    let _ = open_window(title, content, None, MapHooks::default(), cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_map_opens_the_first_result_automatically() {
        let sql = "-- sqlly view: map\n-- sqlly map 2: mode=heatmap\nSELECT 1; SELECT 2;";
        assert_eq!(directive_for(sql, 0).and_then(|d| d.open), Some(true));
        let second = directive_for(sql, 1).unwrap();
        assert_eq!(
            (second.open, second.mode),
            (None, Some(directive::MapMode::Heatmap))
        );
        assert!(directive_for("SELECT 1", 0).is_none());
        let explicit = "-- sqlly view: map\n-- sqlly map: open=manual\nSELECT 1";
        assert_eq!(directive_for(explicit, 0).and_then(|d| d.open), Some(false));
    }
}
