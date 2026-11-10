//! The grid context menu driven the way a user drives it: a real right-click
//! on the grid, then a real click on the menu row. The grid drains the
//! clicked row from inside its own render, so every host hand-off runs while
//! the `SqllyDataTable` is leased — the path provider-only tests never took.

use gpui::{
    Context, Entity, IntoElement, ParentElement, Pixels, Point, Render, Styled, Window, div, point,
};
use sqlly_datatable::grid::menu::{MENU_BORDER, MENU_INNER_PAD, MENU_ITEM_HEIGHT};

use super::*;

/// Height of the fake editor area above the results pane: the pane is the
/// bottom region of the window, the way the workbench mounts it.
const EDITOR_HEIGHT: f32 = 1100.0;
const WINDOW_W: f32 = 1200.0;
const WINDOW_H: f32 = 1500.0;

/// The workbench shape: an editor-sized block on top, the results pane in
/// the remaining bottom region.
struct PaneHost {
    results: Entity<ResultsView>,
}

impl Render for PaneHost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().h(px(EDITOR_HEIGHT)).flex_shrink_0())
            .child(div().flex_1().min_h(px(0.0)).child(self.results.clone()))
    }
}

fn hosted_results(
    cx: &mut gpui::TestAppContext,
) -> (Entity<ResultsView>, &mut gpui::VisualTestContext) {
    ensure_components_initialized(cx);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let results = cx.new(|cx| {
            let mut v = ResultsView::new(cx);
            v.handle_event(two_column_result_set(), cx);
            v.handle_event(rows_batch(0..6), cx);
            v.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
            v
        });
        PaneHost { results }
    });
    cx.simulate_resize(gpui::size(px(WINDOW_W), px(WINDOW_H)));
    cx.run_until_parked();
    let results = host.read_with(cx, |host, _| host.results.clone());
    (results, cx)
}

fn grid_state_of(
    view: &Entity<ResultsView>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<sqlly_datatable::GridState> {
    view.read_with(cx, |v, cx| {
        v.grid_views[0]
            .as_ref()
            .expect("grid view")
            .read(cx)
            .state
            .clone()
    })
}

/// Window point of a data cell (display row `row`, first data column).
fn cell_point(
    view: &Entity<ResultsView>,
    row: usize,
    cx: &mut gpui::VisualTestContext,
) -> Point<Pixels> {
    let state = grid_state_of(view, cx);
    state.read_with(cx, |s, _| {
        let origin = s.bounds.origin;
        point(
            origin.x + px(90.0),
            origin.y + px(s.header_height + s.row_height * (row as f32 + 0.5)),
        )
    })
}

/// Window point of a column header.
fn header_point(view: &Entity<ResultsView>, cx: &mut gpui::VisualTestContext) -> Point<Pixels> {
    let state = grid_state_of(view, cx);
    state.read_with(cx, |s, _| {
        point(
            s.bounds.origin.x + px(90.0),
            s.bounds.origin.y + px(s.header_height * 0.5),
        )
    })
}

/// Rows (item indices per level) leading to the item labelled `label`: the
/// submenu rows to open, then the item itself.
fn path_to(items: &[sqlly_datatable::MenuItem], label: &str) -> Option<Vec<usize>> {
    for (row, item) in items.iter().enumerate() {
        if item.submenu_items().is_none() && item.label() == Some(label.to_string()) {
            return Some(vec![row]);
        }
        if let Some(children) = item.submenu_items()
            && let Some(mut rest) = path_to(children, label)
        {
            rest.insert(0, row);
            return Some(rest);
        }
    }
    None
}

/// Where the open context menu painted row `row` of menu `level`, using the
/// grid's own menu geometry (each level a bordered, padded column of
/// fixed-height rows, submenus beside their parent row).
fn menu_row_point(
    view: &Entity<ResultsView>,
    level: usize,
    row: usize,
    cx: &mut gpui::VisualTestContext,
) -> Option<Point<Pixels>> {
    let (vw, vh) = cx.update(|window, _| {
        let v = window.viewport_size();
        (f32::from(v.width), f32::from(v.height))
    });
    let state = grid_state_of(view, cx);
    state.read_with(cx, |s, _| {
        let menu = s.context_menu.as_ref()?;
        let ox = f32::from(s.bounds.origin.x);
        let oy = f32::from(s.bounds.origin.y);
        let rect = *menu.level_rects(ox, oy, vw, vh, s.char_width).get(level)?;
        let y = rect.y + MENU_BORDER + MENU_INNER_PAD + MENU_ITEM_HEIGHT * row as f32;
        Some(point(px(rect.x + 24.0), px(y + MENU_ITEM_HEIGHT * 0.5)))
    })
}

fn menu_labels(view: &Entity<ResultsView>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
    fn walk(items: &[sqlly_datatable::MenuItem], out: &mut Vec<String>) {
        for item in items {
            match item.submenu_items() {
                Some(children) => walk(children, out),
                None => out.extend(item.label().as_deref().map(str::to_string)),
            }
        }
    }
    let state = grid_state_of(view, cx);
    state.read_with(cx, |s, _| {
        let mut out = Vec::new();
        if let Some(menu) = s.context_menu.as_ref() {
            walk(&menu.items, &mut out);
        }
        out
    })
}

fn click(at: Point<Pixels>, cx: &mut gpui::VisualTestContext) {
    cx.simulate_mouse_down(at, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_up(at, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.run_until_parked();
}

/// Right-click at `at`, then click through to the menu row labelled `label`:
/// each submenu on the way is opened by clicking its row, then the item is
/// clicked. Returns false when the menu does not offer that row.
fn click_menu_item(
    view: &Entity<ResultsView>,
    at: Point<Pixels>,
    label: &str,
    cx: &mut gpui::VisualTestContext,
) -> bool {
    cx.simulate_mouse_down(at, gpui::MouseButton::Right, gpui::Modifiers::none());
    cx.simulate_mouse_up(at, gpui::MouseButton::Right, gpui::Modifiers::none());
    cx.run_until_parked();
    let state = grid_state_of(view, cx);
    let Some(path) = state.read_with(cx, |s, _| {
        s.context_menu
            .as_ref()
            .and_then(|m| path_to(&m.items, label))
    }) else {
        return false;
    };
    for (level, row) in path.iter().enumerate() {
        let Some(point) = menu_row_point(view, level, *row, cx) else {
            return false;
        };
        // The grid drains the clicked row on its next render; a submenu row
        // opens its submenu instead.
        click(point, cx);
    }
    true
}

/// "Filter Rows…" from the grid's context menu opened the builder inline
/// inside the grid's own update and crashed the app ("cannot read
/// SqllyDataTable while it is already being updated"). Once it opens, the
/// builder must sit on the window layer: centered on the window (not the
/// short results pane) and above the grids, which must not take its clicks.
#[gpui::test]
fn filter_rows_menu_item_opens_the_builder_above_the_grid(cx: &mut gpui::TestAppContext) {
    let (view, cx) = hosted_results(cx);
    let at = cell_point(&view, 1, cx);
    assert!(
        click_menu_item(&view, at, "Filter Rows…", cx),
        "the cell menu offers Filter Rows…: {:?}",
        menu_labels(&view, cx)
    );
    assert!(
        view.read_with(cx, |v, _| v.filter_builder.is_some()),
        "clicking Filter Rows… opens the builder"
    );

    let card = cx
        .debug_bounds("dialog-card-results-filter-builder")
        .expect("the builder card is painted");
    let pane_top = px(EDITOR_HEIGHT);
    // Window layer: the card is centered on the whole window, so it starts
    // well above the results pane it is mounted in.
    assert!(
        card.origin.y < pane_top,
        "the builder is laid out on the window, not inside the results pane: {card:?}"
    );
    let center_x = f32::from(card.origin.x) + f32::from(card.size.width) / 2.0;
    assert!(
        (center_x - WINDOW_W / 2.0).abs() < 2.0,
        "the builder is centered on the window: {card:?}"
    );

    // Above the grid: the modal scrim covers the window, so a click on a
    // grid cell below the card reaches the scrim, not the grid.
    let state = grid_state_of(&view, cx);
    let before = state.read_with(cx, |s, _| s.selection.clone());
    let below_card = cell_point(&view, 4, cx);
    assert!(below_card.y > card.origin.y + card.size.height);
    cx.simulate_mouse_down(below_card, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_up(below_card, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.run_until_parked();
    let after = state.read_with(cx, |s, _| s.selection.clone());
    assert_eq!(
        format!("{before:?}"),
        format!("{after:?}"),
        "the grid must not receive clicks while the builder is open over it"
    );
}

/// The toolbar ▽ button, the palette command, and quick filters open the
/// same builder through `open_filter_builder` outside the grid's update;
/// that builder must reach the window layer too.
#[gpui::test]
fn filter_builder_opened_by_command_paints_on_the_window_layer(cx: &mut gpui::TestAppContext) {
    let (view, cx) = hosted_results(cx);
    view.update(cx, |v, cx| v.open_filter_builder(0, cx));
    cx.run_until_parked();
    let card = cx
        .debug_bounds("dialog-card-results-filter-builder")
        .expect("the builder card is painted");
    assert!(card.origin.y < px(EDITOR_HEIGHT), "{card:?}");
}

/// The snapshot prompt is the other dialog the results pane builds itself;
/// it gets the same window-layer treatment.
#[gpui::test]
fn save_snapshot_menu_item_opens_its_prompt_on_the_window_layer(cx: &mut gpui::TestAppContext) {
    let (view, cx) = hosted_results(cx);
    let at = cell_point(&view, 0, cx);
    assert!(
        click_menu_item(&view, at, "Save as Snapshot…", cx),
        "{:?}",
        menu_labels(&view, cx)
    );
    assert!(view.read_with(cx, |v, _| v.snapshot_prompt.is_some()));
    let card = cx
        .debug_bounds("dialog-card-results-snapshot-prompt")
        .expect("the snapshot prompt is painted");
    assert!(card.origin.y < px(EDITOR_HEIGHT), "{card:?}");
}

/// Every host hand-off in the grid menu, clicked for real. Before the
/// hand-offs were deferred, most of these crashed the app the same way
/// Filter Rows… did (sorting even rewrote the grid state it was called
/// from). Rows that open OS prompts, external apps, or new windows are left
/// out; each label runs in a fresh window.
#[gpui::test]
fn grid_menu_host_actions_survive_a_real_click(cx: &mut gpui::TestAppContext) {
    let header_labels = [
        "Add to sort (ascending)",
        "Sort descending",
        "Remove from sort",
        "Hide Empty (all-NULL) Columns",
        "Go to Column…",
        "Profile Column",
        "Profile All Columns",
        "Conditional Formatting…",
        "Top Values…",
        "Copy Distinct Values",
        "Client Statistics",
        "Find & Replace in Grid…",
    ];
    let cell_labels = [
        "Filter Rows…",
        "Client Statistics",
        "Find & Replace in Grid…",
        "Save as Snapshot…",
        "Chart…",
        "Select All",
        "Copy with Headers",
    ];
    for (on_header, label) in header_labels
        .iter()
        .map(|l| (true, *l))
        .chain(cell_labels.iter().map(|l| (false, *l)))
    {
        let (view, cx) = hosted_results(cx);
        let at = if on_header {
            header_point(&view, cx)
        } else {
            cell_point(&view, 1, cx)
        };
        assert!(
            click_menu_item(&view, at, label, cx),
            "{label} is offered ({}): {:?}",
            if on_header { "header" } else { "cell" },
            menu_labels(&view, cx)
        );
        // The menu closed and the app is still standing.
        let state = grid_state_of(&view, cx);
        assert!(
            state.read_with(cx, |s, _| s.context_menu.is_none()),
            "{label}: the menu closes after the click"
        );
    }
}

/// Transpose left the menu because it crashed the app from there; clicked
/// for real it now swaps in the transposed view.
#[gpui::test]
fn transpose_from_the_cell_menu_toggles_the_transposed_view(cx: &mut gpui::TestAppContext) {
    let (view, cx) = hosted_results(cx);
    let at = cell_point(&view, 0, cx);
    assert!(
        click_menu_item(&view, at, "Transpose", cx),
        "{:?}",
        menu_labels(&view, cx)
    );
    assert!(view.read_with(cx, |v, _| v.transpose_active.first().copied() == Some(true)));
}

/// Sorting from the header menu reaches the grid once the hand-off runs.
#[gpui::test]
fn sort_descending_from_the_header_menu_sorts_the_grid(cx: &mut gpui::TestAppContext) {
    let (view, cx) = hosted_results(cx);
    let at = header_point(&view, cx);
    assert!(click_menu_item(&view, at, "Sort descending", cx));
    let state = grid_state_of(&view, cx);
    let first = state.read_with(cx, |s, _| s.display_indices.first().copied());
    assert_eq!(first, Some(5), "descending on id shows the last row first");
}
