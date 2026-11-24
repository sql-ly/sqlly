//! Windowed tests for the dashboard surface: editing tiles (add, drag,
//! resize, reorder) writes the layout cell, the viewer never shows SQL or
//! editors and refuses edits, filters respect Apply, drill-throughs set
//! parameters and change tabs, Refresh names its queries and connections,
//! and notebooks with write cells are refused.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
    px, size,
};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_services::QueryEvent;
use sqlly_services::notebook::cells::layout::{Breakpoint, LayoutSpec, SlideRef};

use super::*;
use crate::notebook::params::widget::Surface as WidgetSurface;
use crate::notebook::present::Surface;
use crate::notebook::view::{NotebookEvent, NotebookOpening, NotebookView};
use crate::results::ResultsView;
use sqlly_services::notebook::params::ParamValue;

pub(crate) const REVENUE: &str = include_str!(
    "../../../../../../scripts/docs-shots/fixtures/notebooks/dashboard-revenue.sqllynb"
);
pub(crate) const TYPED: &str = include_str!(
    "../../../../../../scripts/docs-shots/fixtures/notebooks/dashboard-typed-filters.sqllynb"
);
pub(crate) const WRITES: &str = include_str!(
    "../../../../../../scripts/docs-shots/fixtures/notebooks/dashboard-writes.sqllynb"
);

fn no_choices() -> Rc<dyn Fn() -> Vec<crate::notebook::connection::ConnectionChoice>> {
    Rc::new(Vec::new)
}

pub(crate) fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
        crate::editor::register_keybindings(cx);
        crate::keybindings::register_app_keybindings_from(
            &crate::keybindings::KeybindingSettings::default(),
            cx,
        );
        crate::notebook::present::register_keybindings(cx);
    });
}

pub(crate) fn open_fixture<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (Entity<NotebookView>, &'a mut VisualTestContext) {
    init(cx);
    let doc = crate::notebook::files::load_file("/tmp/dashboard.sqllynb", text, None)
        .expect("fixture loads")
        .doc;
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb =
            cx.new(|cx| NotebookView::with_choices(doc, NotebookOpening::New, no_choices(), cx));
        gpui_component::Root::new(nb, window, cx)
    });
    let view = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .expect("root hosts the notebook")
    });
    (view, vcx)
}

pub(crate) fn draw(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(
        gpui::Point::default(),
        size(px(1400.0), px(2000.0)),
        move |_, _| gpui::div().size_full().child(view.clone()),
    );
}

fn record(
    view: &Entity<NotebookView>,
    cx: &mut VisualTestContext,
) -> Rc<RefCell<Vec<NotebookEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_, cx| {
        cx.subscribe(view, move |_, event: &NotebookEvent, _| {
            seen.borrow_mut().push(event.clone());
        })
        .detach();
    });
    events
}

fn spec(view: &Entity<NotebookView>, cx: &mut VisualTestContext) -> LayoutSpec {
    view.read_with(cx, |nb, _| nb.layout_spec("cell_dashboard").unwrap())
}

/// Give `cell` a finished result with `rows` through a results view, as a
/// run would.
fn complete_with(
    view: &Entity<NotebookView>,
    cell: &str,
    columns: &[&str],
    rows: Vec<Vec<CellValue>>,
    cx: &mut VisualTestContext,
) {
    let results = cx.update(|_, cx| cx.new(ResultsView::new));
    view.update(cx, |nb, cx| nb.attach_results(cell, results.clone(), cx));
    results.update(cx, |rv, cx| {
        rv.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: columns
                    .iter()
                    .map(|c| ResultColumn {
                        name: (*c).into(),
                        data_type: "nvarchar".into(),
                        nullable: true,
                    })
                    .collect(),
            },
            cx,
        );
        rv.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
}

#[gpui::test]
fn the_editor_adds_drags_resizes_and_reorders_tiles(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| nb.open_dashboard_editor(None, cx));
    draw(&view, cx);
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Dashboard);
    let before = spec(&view, cx).sections[0].tabs[0].tiles.len();

    view.update(cx, |nb, cx| nb.add_tile("cell_late", None, cx));
    let after = spec(&view, cx);
    let tiles = &after.sections[0].tabs[0].tiles;
    assert_eq!(tiles.len(), before + 1);
    let added = tiles.last().unwrap().id.clone();
    assert_eq!(tiles.last().unwrap().cell, "cell_late");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.dashboard().unwrap().selected_tile.clone()),
        Some(added.clone())
    );

    // Dropping a dragged cell on a tile inserts before it.
    view.update(cx, |nb, cx| nb.add_tile("cell_shippers", Some("t2"), cx));
    let ids: Vec<String> = spec(&view, cx).sections[0].tabs[0]
        .tiles
        .iter()
        .map(|t| t.cell.clone())
        .collect();
    assert_eq!(ids[1], "cell_shippers");

    // Dragging a tile onto another reorders; onto the canvas, to the end.
    view.update(cx, |nb, cx| nb.drop_tile("t3", Some("t1"), cx));
    assert_eq!(spec(&view, cx).sections[0].tabs[0].tiles[0].id, "t3");
    view.update(cx, |nb, cx| nb.drop_tile("t3", None, cx));
    assert_eq!(
        spec(&view, cx).sections[0].tabs[0].tiles.last().unwrap().id,
        "t3"
    );
    view.update(cx, |nb, cx| nb.move_tile("t3", -1, cx));
    let tiles = spec(&view, cx).sections[0].tabs[0].tiles.clone();
    assert_eq!(tiles[tiles.len() - 2].id, "t3");

    // The right edge handle snaps to grid columns: 100 px per column on a
    // 1200 px canvas.
    view.update(cx, |nb, _| {
        nb.dashboard().unwrap().set_width_for_test(1200.0)
    });
    view.update(cx, |nb, cx| {
        nb.begin_tile_resize("t2", true, 500.0);
        nb.resize_tile_to(710.0, cx);
        nb.end_tile_resize(cx);
    });
    assert_eq!(
        spec(&view, cx).tile("t2").unwrap().span(Breakpoint::Wide),
        9
    );
    view.update(cx, |nb, cx| {
        nb.begin_tile_resize("t2", true, 500.0);
        nb.resize_tile_to(-5000.0, cx);
        nb.end_tile_resize(cx);
    });
    assert_eq!(
        spec(&view, cx).tile("t2").unwrap().span(Breakpoint::Wide),
        1
    );
    view.update(cx, |nb, cx| {
        nb.begin_tile_resize("t2", false, 100.0);
        nb.resize_tile_to(100.0 + 2.0 * crate::theme::Theme::scaled(110.0), cx);
        nb.end_tile_resize(cx);
    });
    assert_eq!(spec(&view, cx).tile("t2").unwrap().height(), 6);

    // Narrow canvases use the compact span.
    view.update(cx, |nb, _| {
        nb.dashboard().unwrap().set_width_for_test(600.0)
    });
    assert_eq!(
        view.read_with(cx, |nb, _| nb.dashboard().unwrap().breakpoint()),
        Breakpoint::Compact
    );
    view.update(cx, |nb, cx| {
        nb.begin_tile_resize("t2", true, 0.0);
        nb.resize_tile_to(-50.0 * 6.0, cx);
        nb.end_tile_resize(cx);
    });
    let t2 = spec(&view, cx).tile("t2").unwrap().clone();
    assert_eq!(t2.span.compact, 6, "the compact span changed");
    assert_eq!(t2.span.wide, 1, "the wide span kept");

    view.update(cx, |nb, cx| nb.remove_tile(&added, cx));
    assert!(spec(&view, cx).tile(&added).is_none());
    view.update(cx, |nb, cx| nb.add_dashboard_tab(cx));
    assert_eq!(spec(&view, cx).sections[0].tabs.len(), 3);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.dashboard().unwrap().slide),
        SlideRef { section: 0, tab: 2 }
    );
    draw(&view, cx);

    // Every edit is in the notebook the host saves.
    let saved = view.update(cx, |nb, _| nb.model_for_save());
    let text = saved.to_canonical_json();
    let reloaded = crate::notebook::files::load_file("/tmp/x.sqllynb", &text, None)
        .unwrap()
        .doc;
    let layout = LayoutSpec::from_value(&reloaded.cell("cell_dashboard").unwrap().spec).unwrap();
    assert_eq!(layout, spec(&view, cx));
    assert!(view.read_with(cx, |nb, _| nb.is_dirty()));
}

#[gpui::test]
fn the_viewer_shows_outputs_only_and_refuses_edits(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    let events = record(&view, cx);
    complete_with(
        &view,
        "cell_by_country",
        &["ShipCountry", "Orders", "Revenue"],
        vec![
            vec![
                CellValue::String("USA".into()),
                CellValue::I64(101),
                CellValue::String("232300.10".into()),
            ],
            vec![
                CellValue::String("Germany".into()),
                CellValue::I64(103),
                CellValue::String("217100.55".into()),
            ],
        ],
        cx,
    );
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    draw(&view, cx);
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Dashboard);
    assert!(view.read_with(cx, |nb, _| !nb.dashboard().unwrap().editing));
    assert!(
        events
            .borrow()
            .iter()
            .all(|e| !matches!(e, NotebookEvent::NeedCellEntities { .. })),
        "the viewer never asks for an editor"
    );
    assert!(
        view.read_with(cx, |nb, _| nb
            .dashboard()
            .unwrap()
            .grids
            .contains_key("cell_by_country")),
        "a SQL tile is its result grid"
    );
    assert_eq!(tile_body(Some("sql")), TileBody::Grid);
    assert_eq!(tile_body(Some("markdown")), TileBody::Text);
    assert_eq!(tile_body(None), TileBody::Missing);

    let before = spec(&view, cx);
    let changed = view.update(cx, |nb, cx| {
        nb.add_tile("cell_late", None, cx);
        nb.edit_layout(|s| s.remove_tile("t1"), cx)
    });
    assert!(!changed);
    assert_eq!(
        spec(&view, cx),
        before,
        "the viewer cannot change the dashboard"
    );
    assert_eq!(
        view.read_with(cx, |nb, _| nb.drill_choices("t2")),
        vec!["USA".to_string(), "Germany".to_string()]
    );

    // Esc leaves the viewer.
    view.update(cx, |nb, cx| {
        nb.close_dashboard(cx);
    });
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Notebook);
}

#[gpui::test]
fn filters_wait_for_apply_and_refresh_names_its_queries(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    draw(&view, cx);
    let plan = view.read_with(cx, |nb, _| nb.refresh_plan());
    assert_eq!(
        plan.cells,
        vec![
            "cell_orders",
            "cell_totals",
            "cell_by_country",
            "cell_top_customers"
        ],
        "the shown tab's queries, inputs first"
    );
    assert_eq!(
        plan.connections,
        vec![("Storefront Dev".to_string(), false)]
    );
    assert!(refresh_text(&plan).contains("Storefront Dev"));

    // `country` waits for Apply: typing keeps a draft.
    let detail = spec(&view, cx).tile("t7").unwrap().clone();
    assert!(view.read_with(cx, |nb, _| nb.tile_visible(&detail)));
    view.update(cx, |nb, cx| nb.set_filter_value("country", "", cx));
    assert!(view.read_with(cx, |nb, _| nb.has_pending_filters()));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("country")),
        Some("Germany".to_string())
    );
    assert!(!view.read_with(cx, |nb, _| nb.dashboard().unwrap().refresh_offer));
    view.update(cx, |nb, cx| nb.apply_filters(cx));
    assert!(!view.read_with(cx, |nb, _| nb.has_pending_filters()));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("country")),
        Some(String::new())
    );
    assert!(
        !view.read_with(cx, |nb, _| nb.tile_visible(&detail)),
        "the detail tile hides once no country is applied"
    );
    assert!(view.read_with(cx, |nb, _| nb.dashboard().unwrap().refresh_offer));
    draw(&view, cx);
}

#[gpui::test]
fn the_filter_bar_uses_the_typed_parameter_widgets(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, TYPED);
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    draw(&view, cx);
    let widget = |cx: &mut VisualTestContext, name: &str| {
        view.read_with(cx, |nb, _| {
            nb.param_ui
                .widget(&WidgetSurface::Dashboard, name)
                .cloned()
                .unwrap_or_else(|| panic!("{name} has a filter widget"))
        })
    };
    for name in ["country", "min_freight", "shipped_after"] {
        let def = widget(cx, name).read_with(cx, |w, _| w.def().clone());
        assert_eq!(def.name, name);
    }
    assert!(
        widget(cx, "country").read_with(cx, |w, _| w.input().is_none()),
        "an enum filter is a list, not a text box"
    );

    // `country` reacts at once: picking France stores it and offers a refresh.
    widget(cx, "country").update(cx, |w, cx| w.pick(1, cx));
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("country")),
        Some("France".to_string())
    );
    assert!(view.read_with(cx, |nb, _| nb.dashboard().unwrap().refresh_offer));
    view.update(cx, |nb, _| {
        nb.dashboard_mut().unwrap().refresh_offer = false
    });

    // `min_freight` waits for Apply: the typed number stays a draft, shown
    // as pending in the filter bar and the rail alike.
    let input = widget(cx, "min_freight").read_with(cx, |w, _| w.input().unwrap());
    cx.update(|window, cx| input.update(cx, |s, cx| s.replace_all("250", window, cx)));
    cx.executor()
        .advance_clock(crate::notebook::params::PARAM_DEBOUNCE * 2);
    cx.run_until_parked();
    assert!(view.read_with(cx, |nb, _| nb.has_pending_filters()));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("min_freight")),
        Some("10".to_string())
    );
    assert!(widget(cx, "min_freight").read_with(cx, |w, _| w.status().pending));
    let rail = view.read_with(cx, |nb, _| {
        nb.param_ui
            .widget(&WidgetSurface::Rail, "min_freight")
            .cloned()
    });
    if let Some(rail) = rail {
        assert!(rail.read_with(cx, |w, _| w.status().pending));
    }
    assert!(!view.read_with(cx, |nb, _| nb.dashboard().unwrap().refresh_offer));

    // Out-of-range input shows inline and never becomes a draft.
    cx.update(|window, cx| input.update(cx, |s, cx| s.replace_all("9000", window, cx)));
    cx.executor()
        .advance_clock(crate::notebook::params::PARAM_DEBOUNCE * 2);
    cx.run_until_parked();
    assert!(widget(cx, "min_freight").read_with(cx, |w, _| w.status().error.is_some()));
    cx.update(|window, cx| input.update(cx, |s, cx| s.replace_all("250", window, cx)));
    cx.executor()
        .advance_clock(crate::notebook::params::PARAM_DEBOUNCE * 2);
    cx.run_until_parked();

    view.update(cx, |nb, cx| nb.apply_filters(cx));
    assert!(!view.read_with(cx, |nb, _| nb.has_pending_filters()));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("min_freight")),
        Some("250".to_string())
    );
    assert!(view.read_with(cx, |nb, _| nb.dashboard().unwrap().refresh_offer));
    draw(&view, cx);
}

/// Type `text` into the dashboard's `name` filter box, as a person does
/// (no time passes, so the typing debounce hasn't fired).
fn type_filter(
    view: &Entity<NotebookView>,
    name: &str,
    text: &str,
    cx: &mut VisualTestContext,
) -> Entity<gpui_component::input::InputState> {
    let input = view.read_with(cx, |nb, cx| {
        nb.param_ui
            .widget(&WidgetSurface::Dashboard, name)
            .and_then(|w| w.read(cx).input())
            .expect("a text filter")
    });
    cx.update(|window, cx| input.update(cx, |s, cx| s.replace_all(text, window, cx)));
    input
}

fn shows_refresh_notice(view: &Entity<NotebookView>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |nb, _| {
        nb.dashboard().unwrap().refresh_offer && !nb.refresh_plan().cells.is_empty()
    })
}

#[gpui::test]
fn enter_in_a_filter_box_applies_it_and_offers_the_refresh(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    draw(&view, cx);
    let input = type_filter(&view, "country", "France", cx);
    input.update(cx, |_, cx| {
        cx.emit(gpui_component::input::InputEvent::PressEnter {
            secondary: false,
            shift: false,
        })
    });
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("country")),
        Some("France".to_string())
    );
    assert!(!view.read_with(cx, |nb, _| nb.has_pending_filters()));
    assert!(shows_refresh_notice(&view, cx));
    draw(&view, cx);
}

#[gpui::test]
fn apply_right_after_typing_takes_the_typed_value(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    draw(&view, cx);
    type_filter(&view, "country", "Spain", cx);
    assert!(
        view.read_with(cx, |nb, _| nb.has_pending_filters()),
        "Apply is enabled while the typing is fresh"
    );
    view.update(cx, |nb, cx| nb.apply_filters(cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("country")),
        Some("Spain".to_string())
    );
    assert!(!view.read_with(cx, |nb, _| nb.has_pending_filters()));
    assert!(shows_refresh_notice(&view, cx));
    // The debounce timer finding nothing left to take keeps it applied.
    cx.executor()
        .advance_clock(crate::notebook::params::PARAM_DEBOUNCE * 2);
    cx.run_until_parked();
    assert!(!view.read_with(cx, |nb, _| nb.has_pending_filters()));
}

#[gpui::test]
fn drill_through_sets_the_parameter_and_opens_the_target(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, REVENUE);
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    view.update(cx, |nb, cx| nb.drill("t2", Some("France".into()), cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.param_text("country")),
        Some("France".to_string())
    );
    assert_eq!(
        view.read_with(cx, |nb, _| nb.dashboard().unwrap().slide),
        SlideRef { section: 1, tab: 0 }
    );
    draw(&view, cx);
    let shown = view.read_with(cx, |nb, cx| {
        nb.param_ui
            .widget(&WidgetSurface::Dashboard, "country")
            .and_then(|w| w.read(cx).status().value.clone())
    });
    assert_eq!(
        shown,
        Some(ParamValue::Text("France".into())),
        "the filter bar shows the drilled value"
    );
    let plan = view.read_with(cx, |nb, _| nb.refresh_plan());
    assert_eq!(plan.cells, vec!["cell_orders", "cell_country_orders"]);
    draw(&view, cx);
}

#[gpui::test]
fn notebooks_that_change_data_cannot_be_viewed_or_presented(cx: &mut TestAppContext) {
    let (view, cx) = open_fixture(cx, WRITES);
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    draw(&view, cx);
    let refusal = view
        .read_with(cx, |nb, _| nb.refusal().cloned())
        .expect("refused");
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Refused);
    assert!(!refusal.presenting);
    assert_eq!(refusal.write_cells, vec!["fix_freight".to_string()]);
    assert!(refusal.message.contains('1'), "{}", refusal.message);
    view.update(cx, |nb, cx| nb.dismiss_refusal(cx));
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Notebook);

    view.update_in(cx, |nb, window, cx| nb.start_presentation(None, window, cx));
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Refused);
    assert!(view.read_with(cx, |nb, _| nb.refusal().unwrap().presenting));
    assert!(!cx.update(|window, _| window.is_fullscreen()));
    view.update(cx, |nb, cx| nb.dismiss_refusal(cx));

    // The editor still opens; switching it to the viewer is refused too.
    view.update(cx, |nb, cx| nb.open_dashboard_editor(None, cx));
    view.update(cx, |nb, cx| nb.set_dashboard_editing(false, cx));
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Refused);
    view.update(cx, |nb, cx| nb.dismiss_refusal(cx));
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Dashboard);

    // Once the write cell is read-only the dashboard can be viewed.
    view.update(cx, |nb, cx| nb.make_read_only("cell_3", cx));
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    assert_eq!(view.read_with(cx, |nb, _| nb.surface()), Surface::Dashboard);
}

#[gpui::test]
fn the_top_bar_creates_a_dashboard_when_there_is_none(cx: &mut TestAppContext) {
    init(cx);
    let mut doc = crate::notebook::files::load_file("/tmp/d.sqllynb", REVENUE, None)
        .unwrap()
        .doc;
    doc.cells
        .retain(|c| c.kind != sqlly_services::notebook::CellKind::Layout);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let nb =
            cx.new(|cx| NotebookView::with_choices(doc, NotebookOpening::New, no_choices(), cx));
        gpui_component::Root::new(nb, window, cx)
    });
    let view = cx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .unwrap()
    });
    view.update(cx, |nb, cx| nb.open_dashboard_editor(None, cx));
    let layouts = view.read_with(cx, |nb, _| nb.layout_cells());
    assert_eq!(layouts.len(), 1);
    let spec = view.read_with(cx, |nb, _| nb.layout_spec(&layouts[0].0).unwrap());
    assert_eq!(spec.tiles().count(), STARTER_TILES);
    let model = view.read_with(cx, |nb, _| nb.model().clone());
    assert!(spec.validate(&model).is_empty());
    draw(&view, cx);
}

#[gpui::test]
fn every_kind_of_tile_draws_its_output_without_editors(cx: &mut TestAppContext) {
    init(cx);
    let mut doc = crate::notebook::files::load_file("/tmp/d.sqllynb", REVENUE, None)
        .unwrap()
        .doc;
    let at = doc.cells.len() - 1;
    let mut chart = sqlly_services::notebook::Cell::new(
        "cell_chart",
        sqlly_services::notebook::CellKind::Chart,
        Default::default(),
    );
    chart.spec = serde_json::json!({"input": "cell_by_country",
        "chart": {"kind": "bar", "x": {"column": "ShipCountry"}, "y": [{"column": "Revenue"}]}});
    let mut profile = sqlly_services::notebook::Cell::new(
        "cell_profile",
        sqlly_services::notebook::CellKind::Profile,
        Default::default(),
    );
    profile.spec = serde_json::json!({"input": "cell_by_country"});
    doc.cells.insert(at, chart);
    doc.cells.insert(at + 1, profile);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let nb =
            cx.new(|cx| NotebookView::with_choices(doc, NotebookOpening::New, no_choices(), cx));
        gpui_component::Root::new(nb, window, cx)
    });
    let view = cx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .unwrap()
    });
    complete_with(
        &view,
        "cell_by_country",
        &["ShipCountry", "Revenue"],
        vec![vec![
            CellValue::String("USA".into()),
            CellValue::F64(232300.1),
        ]],
        cx,
    );
    draw(&view, cx);
    let visual = |cx: &mut VisualTestContext| {
        view.read_with(cx, |nb, cx| {
            nb.visual
                .slots
                .get("cell_chart")
                .map(|slot| slot.view.read(cx).output_only)
        })
    };
    assert_eq!(
        visual(cx),
        Some(false),
        "in the notebook the chart has its toolbar"
    );
    view.update(cx, |nb, cx| {
        nb.open_dashboard_editor(None, cx);
        nb.add_tile("cell_chart", None, cx);
        nb.add_tile("cell_profile", None, cx);
        nb.set_dashboard_editing(false, cx);
    });
    draw(&view, cx);
    assert_eq!(
        visual(cx),
        Some(true),
        "on a tile the chart draws only its output"
    );
    let weak = view.downgrade();
    assert!(cx.update(|_, cx| crate::notebook::present::output_only(&weak, cx)));
    assert!(view.read_with(cx, |nb, _| nb.analysis.views.contains_key("cell_profile")));
    view.update(cx, |nb, cx| nb.close_dashboard(cx));
    draw(&view, cx);
    assert_eq!(
        visual(cx),
        Some(false),
        "back in the notebook the toolbar returns"
    );
    assert!(!cx.update(|_, cx| crate::notebook::present::output_only(&weak, cx)));
}
