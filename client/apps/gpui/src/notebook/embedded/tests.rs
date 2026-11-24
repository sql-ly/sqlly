//! Windowed tests for embedded outputs: an embedded SQL cell draws only its
//! grid (no header, editor or results chrome) and keeps failures and stale
//! marks visible; an embedded pivot is the pivot canvas alone with its
//! sidebar on request and drill-through intact; an embedded chart hides its
//! editor and still cross-filters without running anything; the setting is
//! one undo step, round-trips and leaves the trust hash alone; the notebook
//! default reaches new cells; dashboards draw through the same path.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
    px, size,
};
use serde_json::json;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::GridTab;
use sqlly_services::QueryEvent;
use sqlly_services::notebook::cells::spec::CrossFilter;
use sqlly_services::notebook::display::{self, DisplayMode, LockedView};
use sqlly_services::notebook::{Cell, CellKind, CellStatus, ExecutionPolicy, NotebookDoc};
use sqlly_services::notebook_security::trust;

use super::{BadgeTone, EmbeddedCommand, badge};
use crate::notebook::view::{DispatchOutcome, NotebookEvent, NotebookOpening, NotebookView};
use crate::notebook::visual::VisualEvent;
use crate::results::ResultsView;

struct AutosaveRoot(#[allow(dead_code)] tempfile::TempDir);

impl AutosaveRoot {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(dir.path().to_path_buf()));
        Self(dir)
    }
}

impl Drop for AutosaveRoot {
    fn drop(&mut self) {
        crate::notebook::autosave::set_test_root(None);
    }
}

fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
        crate::editor::register_keybindings(cx);
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| {
            NotebookView::with_choices(model, NotebookOpening::New, Rc::new(Vec::new), cx)
        });
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

fn draw(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(
        gpui::Point::default(),
        size(px(1400.0), px(2400.0)),
        move |_, _| gpui::div().size_full().child(view.clone()),
    );
}

fn drawn(cx: &mut VisualTestContext, selector: String) -> bool {
    cx.debug_bounds(Box::leak(selector.into_boxed_str()))
        .is_some()
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

fn runs(events: &Rc<RefCell<Vec<NotebookEvent>>>) -> usize {
    events
        .borrow()
        .iter()
        .filter(|e| matches!(e, NotebookEvent::RunCell { .. }))
        .count()
}

fn cell(id: &str, kind: CellKind, source: &str, spec: serde_json::Value) -> Cell {
    let mut cell = Cell::new(id, kind, ExecutionPolicy::default());
    cell.source = source.to_string();
    cell.spec = spec;
    cell
}

/// A report: a SQL cell, Markdown, a pivot and a chart over the SQL cell.
fn report(embed_all: bool) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nbembedded", "Report");
    let mut sql = cell(
        "cell_1",
        CellKind::Sql,
        "SELECT region, revenue FROM sales",
        json!(null),
    );
    sql.name = Some("sales".into());
    nb.cells.push(sql);
    nb.cells.push(cell(
        "cell_md",
        CellKind::Markdown,
        "## Regions",
        json!(null),
    ));
    nb.cells.push(cell(
        "cell_pivot",
        CellKind::Pivot,
        "",
        json!({"input": "cell_1", "pivot": {"rows": ["region"], "value": "revenue", "aggregation": "sum"}}),
    ));
    nb.cells.push(cell(
        "cell_chart",
        CellKind::Chart,
        "",
        json!({"input": "cell_1", "chart": {"kind": "bar", "x": {"column": "region"}, "y": [{"column": "revenue"}], "aggregate": "sum"}}),
    ));
    if embed_all {
        display::set_all(&mut nb, DisplayMode::Embedded);
    }
    nb
}

fn run_all_with_rows(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let results = cx.update(|_, cx| cx.new(ResultsView::new));
    view.update(cx, |nb, cx| {
        nb.attach_results("cell_1", results.clone(), cx);
        nb.run_all(cx);
    });
    let col = |name: &str, data_type: &str| ResultColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable: true,
    };
    let rows = [("West", 10.0), ("East", 4.0), ("West", 6.0), ("North", 9.0)]
        .iter()
        .map(|(r, v)| vec![CellValue::String((*r).into()), CellValue::F64(*v)])
        .collect();
    results.update(cx, |r, cx| {
        r.set_query_sql("SELECT region, revenue FROM sales", cx);
        r.handle_event(QueryEvent::Started, cx);
        r.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![col("region", "varchar"), col("revenue", "float")],
            },
            cx,
        );
        r.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        r.handle_event(QueryEvent::Completed { elapsed_ms: 2 }, cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn an_embedded_sql_cell_draws_only_its_grid(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(true));
    draw(&view, cx);
    assert!(drawn(cx, "nb-embedded-cell_1".into()));
    assert!(
        !drawn(cx, "nb-cell-header-cell_1".into()),
        "no header before a run"
    );
    run_all_with_rows(&view, cx);
    draw(&view, cx);
    assert!(
        drawn(cx, "nb-embedded-grid-cell_1".into()),
        "the grid shows"
    );
    assert!(
        !drawn(cx, "nb-cell-header-cell_1".into()),
        "no header toolbar"
    );
    assert!(
        !drawn(cx, "nb-results-pane-cell_1".into()),
        "no results chrome (messages, history, layout, export)"
    );
    assert!(!view.read_with(cx, |nb, _| nb.has_live_editor("cell_1")));
    assert!(
        !drawn(cx, "nb-embedded-badge-cell_1".into()),
        "a current result carries no mark"
    );

    view.update(cx, |nb, cx| nb.toggle_embedded(vec!["cell_1".into()], cx));
    draw(&view, cx);
    assert!(drawn(cx, "nb-cell-header-cell_1".into()), "back in full");
    assert!(drawn(cx, "nb-results-pane-cell_1".into()));
    assert!(!drawn(cx, "nb-embedded-cell_1".into()));
}

#[gpui::test]
fn a_failed_embedded_cell_shows_its_error(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(true));
    view.update(cx, |nb, cx| {
        nb.run_all(cx);
        nb.dispatch_result(
            "cell_1",
            DispatchOutcome::NotStarted("No connection".into()),
            cx,
        );
    });
    assert_eq!(
        view.read_with(cx, |nb, _| nb.cell_state("cell_1")),
        CellStatus::Failed
    );
    draw(&view, cx);
    assert!(drawn(cx, "nb-embedded-error-cell_1".into()), "never hidden");
    assert!(drawn(cx, "nb-embedded-badge-cell_1".into()));
}

#[gpui::test]
fn a_stale_embedded_result_stays_marked(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(true));
    run_all_with_rows(&view, cx);
    view.update(cx, |nb, _| {
        let doc = nb.doc_mut_for_test();
        doc.set_source("cell_1", "SELECT region, revenue FROM sales WHERE 1 = 1");
        doc.refresh_staleness();
    });
    assert!(view.read_with(cx, |nb, _| nb.is_stale("cell_1")));
    draw(&view, cx);
    assert!(drawn(cx, "nb-embedded-grid-cell_1".into()));
    assert!(drawn(cx, "nb-embedded-badge-cell_1".into()));
}

#[test]
fn partial_stale_failed_and_running_outputs_are_marked() {
    assert_eq!(badge(CellStatus::Fresh, false), None);
    assert_eq!(badge(CellStatus::Cached, false), None);
    assert_eq!(badge(CellStatus::Idle, false), None);
    assert_eq!(badge(CellStatus::Partial, false), Some(BadgeTone::Warning));
    assert_eq!(badge(CellStatus::Fresh, true), Some(BadgeTone::Warning));
    assert_eq!(badge(CellStatus::Failed, false), Some(BadgeTone::Error));
    assert_eq!(badge(CellStatus::Streaming, false), Some(BadgeTone::Busy));
}

#[gpui::test]
fn an_embedded_pivot_is_the_pivot_canvas_with_an_optional_sidebar(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(true));
    run_all_with_rows(&view, cx);
    draw(&view, cx);
    assert!(drawn(cx, "nb-pivot-output-cell_pivot".into()));
    assert!(
        !drawn(cx, "nb-pivot-sidebar-cell_pivot".into()),
        "configuration hidden by default"
    );
    let pivot = view.read_with(cx, |nb, _| nb.visual.slots["cell_pivot"].view.clone());
    assert!(pivot.read_with(cx, |v, _| v.output_only));

    view.update(cx, |nb, cx| {
        nb.activate_cell("cell_pivot", cx);
        nb.embedded_command(EmbeddedCommand::ToggleConfig, cx);
    });
    draw(&view, cx);
    assert!(pivot.read_with(cx, |v, _| v.display.config_shown));
    assert!(drawn(cx, "nb-pivot-sidebar-cell_pivot".into()));
    assert!(
        view.read_with(cx, |nb, _| nb.display_of("cell_pivot").config_shown),
        "kept in the document"
    );

    // A double-click on a value drills to the rows behind it in the grid,
    // with a way back to the pivot.
    let grid = pivot.read_with(cx, |v, _| v.pivot.grid.clone().unwrap());
    let state = grid.read_with(cx, |g, _| g.pivot_state().cloned().unwrap());
    state.update(cx, |s, _| s.request_drill_down(0, 0));
    pivot.update(cx, |v, _| v.pivot.drill = Some(false));
    draw(&view, cx);
    draw(&view, cx);
    assert_eq!(grid.read_with(cx, |g, _| g.active_tab()), GridTab::Grid);
    assert_eq!(pivot.read_with(cx, |v, _| v.pivot.drill), Some(true));
    grid.update(cx, |g, cx| g.set_active_tab(GridTab::Pivot, cx));
    draw(&view, cx);
    draw(&view, cx);
    assert_eq!(pivot.read_with(cx, |v, _| v.pivot.drill), None);
    assert!(drawn(cx, "nb-pivot-output-cell_pivot".into()));
}

#[gpui::test]
fn an_embedded_chart_hides_its_editor_and_still_cross_filters(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut model = report(true);
    model.cells[2].spec["follows"] = json!("cell_chart");
    let (view, cx) = open(cx, model);
    let events = record(&view, cx);
    run_all_with_rows(&view, cx);
    draw(&view, cx);
    let chart = view.read_with(cx, |nb, _| nb.visual.slots["cell_chart"].view.clone());
    chart.read_with(cx, |v, _| {
        assert!(v.output_only);
        assert!(!v.display.config_shown, "the editor is hidden");
    });
    let dispatched = runs(&events);
    view.update(cx, |nb, cx| {
        nb.on_visual_event(
            "cell_chart",
            &VisualEvent::Filter {
                source: "cell_chart".into(),
                filter: Some(CrossFilter {
                    source: "cell_chart".into(),
                    column: "region".into(),
                    value: "West".into(),
                    grain: None,
                }),
            },
            cx,
        );
    });
    view.read_with(cx, |nb, _| {
        assert!(nb.outputs.filter("cell_chart").is_some());
        assert!(!nb.is_running());
    });
    assert_eq!(runs(&events), dispatched, "a cross-filter runs nothing");

    view.update(cx, |nb, cx| nb.toggle_embedded_config("cell_chart", cx));
    draw(&view, cx);
    assert!(chart.read_with(cx, |v, _| v.display.config_shown));
}

#[gpui::test]
fn the_display_setting_is_one_undo_step_and_leaves_trust_alone(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(false));
    let hash = view.read_with(cx, |nb, _| trust::executable_hash(nb.model()));
    view.update(cx, |nb, cx| {
        nb.selected = vec!["cell_1".into(), "cell_pivot".into()];
        nb.embedded_command(EmbeddedCommand::Toggle, cx);
    });
    view.read_with(cx, |nb, _| {
        assert!(nb.is_embedded("cell_1"));
        assert!(nb.is_embedded("cell_pivot"));
        assert!(!nb.is_embedded("cell_chart"));
        assert_eq!(trust::executable_hash(nb.model()), hash);
    });
    view.update(cx, |nb, cx| {
        nb.set_locked_view("cell_1", Some(LockedView::Pivot), cx)
    });
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.display_of("cell_1").lock, Some(LockedView::Pivot));
        assert_eq!(trust::executable_hash(nb.model()), hash);
        let text = nb.model().to_canonical_json();
        let back = sqlly_services::notebook::load(
            Some("r.sqllynb"),
            &text,
            &mut sqlly_services::notebook::SeededIds(1),
            None,
        )
        .unwrap()
        .doc;
        assert_eq!(back.cells, nb.model().cells, "round-trips through the file");
    });
    view.update(cx, |nb, cx| nb.undo_structure(cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.display_of("cell_1").lock),
        None
    );
    view.update(cx, |nb, cx| nb.undo_structure(cx));
    view.read_with(cx, |nb, _| {
        assert!(!nb.is_embedded("cell_1"), "the toggle was one step");
        assert!(!nb.is_embedded("cell_pivot"));
    });
    view.update(cx, |nb, cx| nb.redo_structure(cx));
    assert!(view.read_with(cx, |nb, _| nb.is_embedded("cell_pivot")));
}

#[gpui::test]
fn embed_all_reaches_new_cells_and_locks_a_full_grid(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(false));
    view.update(cx, |nb, cx| {
        nb.embedded_command(EmbeddedCommand::EmbedAll, cx)
    });
    let id = view.update(cx, |nb, cx| {
        assert!(nb.embeds_all());
        let end = nb.doc.len();
        nb.insert_cell("sql", end, cx)
    });
    view.read_with(cx, |nb, _| {
        assert!(nb.is_embedded(&id), "the default reaches new cells");
        assert!(nb.is_embedded("cell_md"));
    });
    view.update(cx, |nb, cx| nb.toggle_embed_all(cx));
    assert!(!view.read_with(cx, |nb, _| nb.is_embedded("cell_1")));

    view.update(cx, |nb, cx| {
        nb.set_locked_view("cell_1", Some(LockedView::Pivot), cx)
    });
    run_all_with_rows(&view, cx);
    draw(&view, cx);
    let results = view.read_with(cx, |nb, _| nb.cells["cell_1"].results.clone().unwrap());
    let grid = results.update(cx, |r, cx| r.embedded_grid(cx)).unwrap();
    assert_eq!(
        grid.read_with(cx, |g, _| g.active_tab()),
        GridTab::Pivot,
        "a locked grid stays on its view in full mode too"
    );
    grid.update(cx, |g, cx| g.set_active_tab(GridTab::Grid, cx));
    draw(&view, cx);
    assert_eq!(grid.read_with(cx, |g, _| g.active_tab()), GridTab::Pivot);
}

#[gpui::test]
fn dashboard_tiles_draw_through_the_same_output_path(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut model = report(false);
    model.cells.push(cell(
        "cell_dashboard",
        CellKind::Layout,
        "",
        json!({"sections": [{"id": "s1", "tabs": [{"id": "tab1", "tiles": [
            {"id": "t1", "cell": "cell_pivot", "span": 12, "height": 4}]}]}]}),
    ));
    let (view, cx) = open(cx, model);
    run_all_with_rows(&view, cx);
    view.update(cx, |nb, cx| nb.open_dashboard_viewer(None, cx));
    draw(&view, cx);
    assert!(
        drawn(cx, "nb-pivot-output-cell_pivot".into()),
        "a pivot tile is the same canvas an embedded pivot shows"
    );
    let pivot = view.read_with(cx, |nb, _| nb.visual.slots["cell_pivot"].view.clone());
    pivot.read_with(cx, |v, _| {
        assert!(v.output_only);
        assert!(!v.display.config_shown, "tiles never show configuration");
    });
}

#[test]
fn the_toggle_is_rebindable_and_in_the_palette() {
    let mut defaults = std::collections::HashMap::new();
    super::keys::insert_defaults(&mut defaults);
    for spec in super::keys::COMMANDS {
        assert!(defaults.contains_key(spec.id), "{}", spec.id);
        assert!(
            sqlly_i18n::contains_key(spec.title_key),
            "{}",
            spec.title_key
        );
        assert_eq!(super::keys::command_for(spec.id), Some(spec.command));
    }
    assert_eq!(defaults["notebook.toggleEmbedded"].len(), 1);
}

#[gpui::test]
fn the_report_fixtures_open_embedded(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    for (text, config) in [
        (
            include_str!(
                "../../../../../../scripts/docs-shots/fixtures/notebooks/embedded-report.sqllynb"
            ),
            false,
        ),
        (
            include_str!(
                "../../../../../../scripts/docs-shots/fixtures/notebooks/embedded-report-config.sqllynb"
            ),
            true,
        ),
    ] {
        let doc = crate::notebook::files::load_file("/tmp/report.sqllynb", text, None)
            .expect("fixture loads")
            .doc;
        let (view, vcx) = open(cx, doc);
        draw(&view, vcx);
        view.read_with(vcx, |nb, _| {
            assert!(nb.embeds_all());
            for id in ["cell_intro", "cell_summary", "cell_pivot", "cell_chart"] {
                assert!(nb.is_embedded(id), "{id}");
            }
            assert!(
                !nb.is_embedded("cell_lines"),
                "the rows behind the pivot stay in full"
            );
            assert_eq!(nb.display_of("cell_pivot").config_shown, config);
        });
    }
}

#[gpui::test]
fn an_embedded_grid_has_no_tabs_or_panels_and_gets_them_back_in_full(cx: &mut TestAppContext) {
    use sqlly_datatable::SidebarMode;
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(true));
    run_all_with_rows(&view, cx);
    draw(&view, cx);
    let results = view.read_with(cx, |nb, _| nb.cells["cell_1"].results.clone().unwrap());
    let grid = results.update(cx, |r, cx| r.embedded_grid(cx)).unwrap();
    grid.read_with(cx, |g, _| {
        assert!(!g.tab_bar_visible());
        assert!(!g.tab_switching());
        assert_eq!(g.pivot_sidebar_mode(), SidebarMode::Hidden);
        assert_eq!(g.chart_sidebar_mode(), SidebarMode::Hidden);
    });
    view.update(cx, |nb, cx| {
        nb.set_display_mode(vec!["cell_1".into()], DisplayMode::Full, cx)
    });
    draw(&view, cx);
    grid.read_with(cx, |g, _| {
        assert!(g.tab_bar_visible());
        assert!(g.tab_switching());
        assert_ne!(g.pivot_sidebar_mode(), SidebarMode::Hidden);
    });
    view.update(cx, |nb, cx| {
        nb.set_locked_view("cell_1", Some(LockedView::Rows), cx)
    });
    draw(&view, cx);
    grid.read_with(cx, |g, _| {
        assert!(g.tab_bar_visible(), "a locked full grid keeps its tab bar");
        assert!(!g.tab_switching(), "but cannot switch");
        assert_eq!(g.active_tab(), GridTab::Grid);
    });
}

#[gpui::test]
fn a_drill_in_a_locked_grid_shows_the_rows_behind_it(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, report(false));
    view.update(cx, |nb, cx| {
        nb.set_locked_view("cell_1", Some(LockedView::Pivot), cx)
    });
    run_all_with_rows(&view, cx);
    draw(&view, cx);
    let results = view.read_with(cx, |nb, _| nb.cells["cell_1"].results.clone().unwrap());
    let grid = results.update(cx, |r, cx| r.embedded_grid(cx)).unwrap();
    assert_eq!(grid.read_with(cx, |g, _| g.active_tab()), GridTab::Pivot);
    let state = grid.read_with(cx, |g, _| g.pivot_state().cloned().unwrap());
    state.update(cx, |s, cx| {
        s.config.row_fields = vec![0];
        s.config.value_field = Some(1);
        s.recompute();
        s.request_drill_down(0, 0);
        cx.notify();
    });
    draw(&view, cx);
    cx.run_until_parked();
    draw(&view, cx);
    assert_eq!(
        grid.read_with(cx, |g, _| g.active_tab()),
        GridTab::Pivot,
        "the locked grid stays on its view"
    );
    assert!(
        drawn(cx, "nb-drill-rows-cell_1".into()),
        "the rows show below it"
    );
    let count = view.read_with(cx, |nb, _| nb.embedded.drills["cell_1"].count);
    assert!(count > 0);
}
