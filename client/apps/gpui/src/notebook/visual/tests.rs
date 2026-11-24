//! Windowed tests for the visual cells and the output registry: SQL results
//! snapshotted with caps and completeness, staleness reaching visual cells,
//! local cells computed in the run queue without a connection, cross-filters
//! that only filter downloaded rows, scalar values in Markdown and SQL,
//! transforms run in the local workspace, and every kind drawing.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
    px, size,
};
use serde_json::json;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_services::QueryEvent;
use sqlly_services::notebook::cells::spec::CrossFilter;
use sqlly_services::notebook::{
    Cell, CellKind, CellStatus, Completeness, ExecutionPolicy, NotebookDoc,
};

use super::{VisualEvent, VisualKind};
use crate::notebook::view::{NotebookEvent, NotebookOpening, NotebookView};
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
        size(px(1400.0), px(2800.0)),
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

fn run_cells(events: &Rc<RefCell<Vec<NotebookEvent>>>) -> Vec<String> {
    events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            NotebookEvent::RunCell { cell_id } => Some(cell_id.clone()),
            _ => None,
        })
        .collect()
}

fn cell(id: &str, kind: CellKind, source: &str, spec: serde_json::Value) -> Cell {
    let mut cell = Cell::new(id, kind, ExecutionPolicy::default());
    cell.source = source.to_string();
    cell.spec = spec;
    cell
}

fn col(name: &str, data_type: &str) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable: true,
    }
}

fn region_rows() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let columns = vec![
        col("region", "varchar"),
        col("day", "date"),
        col("revenue", "float"),
    ];
    let rows = [
        ("West", "2026-01-04", 10.0),
        ("East", "2026-01-12", 4.0),
        ("West", "2026-02-02", 6.0),
        ("North", "2026-02-20", 9.0),
        ("East", "2026-03-01", 3.0),
    ]
    .iter()
    .map(|(r, d, v)| {
        vec![
            CellValue::String((*r).into()),
            CellValue::String((*d).into()),
            CellValue::F64(*v),
        ]
    })
    .collect();
    (columns, rows)
}

/// A SQL cell `orders` followed by `cells`.
fn notebook(cells: Vec<Cell>) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nbvisual", "Visual");
    let mut sql = cell(
        "cell_1",
        CellKind::Sql,
        "SELECT region, day, revenue FROM sales",
        json!(null),
    );
    sql.name = Some("orders".into());
    nb.cells.push(sql);
    nb.cells.extend(cells);
    nb
}

/// Run every cell, answering the SQL cell's run with `rows`.
fn run_all_with(
    view: &Entity<NotebookView>,
    cx: &mut VisualTestContext,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
) -> Entity<ResultsView> {
    let results = cx.update(|_, cx| cx.new(ResultsView::new));
    view.update(cx, |nb, cx| {
        nb.attach_results("cell_1", results.clone(), cx);
        nb.run_all(cx);
    });
    results.update(cx, |r, cx| {
        r.set_query_sql("SELECT region, day, revenue FROM sales", cx);
        r.handle_event(QueryEvent::Started, cx);
        r.handle_event(QueryEvent::ResultSet { index: 0, columns }, cx);
        r.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        r.handle_event(QueryEvent::Completed { elapsed_ms: 3 }, cx);
    });
    cx.run_until_parked();
    results
}

#[gpui::test]
fn sql_results_feed_the_registry_and_local_cells_run_in_the_queue(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        notebook(vec![cell(
            "cell_2",
            CellKind::Chart,
            "",
            json!({"input": "cell_1", "chart": {"kind": "bar", "x": {"column": "region"}, "y": [{"column": "revenue"}], "aggregate": "sum"}}),
        )]),
    );
    let events = record(&view, cx);
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    assert_eq!(
        run_cells(&events),
        ["cell_1"],
        "the chart never goes to a connection"
    );
    view.read_with(cx, |nb, _| {
        let output = nb
            .outputs
            .get("cell_1")
            .expect("the SQL result is snapshotted");
        assert_eq!(output.rows.len(), 5);
        assert_eq!(output.completeness, Completeness::Complete);
        assert_eq!(output.columns[2].name, "revenue");
        assert!(
            output
                .provenance
                .sql_hash
                .as_deref()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(output.provenance.extracted_at.is_some());
        assert!(
            nb.outputs.get("cell_2").is_some(),
            "the chart computed its output"
        );
        assert_eq!(nb.cell_state("cell_2"), CellStatus::Fresh);
        assert_eq!(nb.cell_state("cell_1"), CellStatus::Fresh);
        assert!(!nb.is_running());
    });
}

#[gpui::test]
fn snapshots_honor_the_cell_row_cap_and_staleness_reaches_visual_cells(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut nb = notebook(vec![cell(
        "cell_2",
        CellKind::Table,
        "",
        json!({"input": "cell_1"}),
    )]);
    nb.cells[0].execution_policy.max_rows = Some(3);
    let (view, cx) = open(cx, nb);
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    view.read_with(cx, |nb, cx| {
        let output = nb.outputs.get("cell_1").unwrap();
        assert_eq!(output.rows.len(), 3);
        assert_eq!(output.row_count, 5);
        assert_eq!(output.completeness, Completeness::Truncated { cap: 3 });
        let table = nb.outputs.get("cell_2").unwrap();
        assert_eq!(
            table.completeness,
            Completeness::Truncated { cap: 3 },
            "a table over a capped input is not complete either"
        );
        let inputs = nb.visual_inputs("cell_2", VisualKind::Table, false, cx);
        assert!(inputs.input.as_ref().unwrap().current);
        let badge =
            super::completeness_text(inputs.input.as_ref().unwrap().output.as_ref().unwrap());
        assert!(badge.unwrap().contains('3'));
        let analysis = crate::notebook::analysis::inputs::cell_output(nb, "cell_1", cx).unwrap();
        assert_eq!(
            analysis.output.completeness,
            Completeness::Truncated { cap: 3 },
            "analysis cells read the same registry snapshot"
        );
    });
    view.update(cx, |nb, cx| {
        nb.doc_mut_for_test()
            .set_source("cell_1", "SELECT region FROM sales");
        nb.doc_mut_for_test().refresh_staleness();
        let inputs = nb.visual_inputs("cell_2", VisualKind::Table, false, cx);
        let input = inputs.input.unwrap();
        assert!(
            !input.current,
            "an edited input is not presented as current"
        );
        assert!(
            input.output.is_some(),
            "its last result is still shown, labelled"
        );
    });
}

#[gpui::test]
fn cross_filters_filter_downloaded_rows_and_never_run_anything(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        notebook(vec![
            cell(
                "cell_2",
                CellKind::Chart,
                "",
                json!({"input": "cell_1", "chart": {"kind": "bar", "x": {"column": "region"}, "y": [{"column": "revenue"}]}}),
            ),
            cell(
                "cell_3",
                CellKind::Table,
                "",
                json!({"input": "cell_1", "follows": "cell_2"}),
            ),
            cell(
                "cell_4",
                CellKind::Kpi,
                "",
                json!({"input": "cell_1", "follows": "cell_2", "tiles": [{"label": "Revenue", "kpi": {"value": {"kind": "aggregate", "column": "revenue", "aggregate": "sum"}}}]}),
            ),
        ]),
    );
    let events = record(&view, cx);
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    let dispatched = run_cells(&events).len();
    view.update(cx, |nb, cx| {
        assert_eq!(nb.outputs.get("cell_3").unwrap().rows.len(), 5);
        nb.on_visual_event(
            "cell_2",
            &VisualEvent::Filter {
                source: "cell_2".into(),
                filter: Some(CrossFilter {
                    source: "cell_2".into(),
                    column: "region".into(),
                    value: "West".into(),
                    grain: None,
                }),
            },
            cx,
        );
    });
    view.read_with(cx, |nb, cx| {
        assert_eq!(
            nb.outputs.get("cell_3").unwrap().rows.len(),
            2,
            "the table shows West only"
        );
        let kpi = nb.outputs.get("cell_4").unwrap();
        assert_eq!(
            kpi.rows[0][1],
            CellValue::F64(16.0),
            "the KPI sums West only"
        );
        assert_eq!(
            nb.outputs.get("cell_1").unwrap().rows.len(),
            5,
            "the source is untouched"
        );
        let inputs = nb.visual_inputs("cell_3", VisualKind::Table, false, cx);
        assert_eq!(inputs.filter.as_ref().unwrap().value, "West");
        assert!(!nb.is_running());
    });
    assert_eq!(
        run_cells(&events).len(),
        dispatched,
        "no query was dispatched"
    );
    view.update(cx, |nb, cx| {
        nb.on_visual_event(
            "cell_2",
            &VisualEvent::Filter {
                source: "cell_2".into(),
                filter: None,
            },
            cx,
        )
    });
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.outputs.get("cell_3").unwrap().rows.len(), 5);
    });
    assert_eq!(run_cells(&events).len(), dispatched);
}

#[gpui::test]
fn scalar_values_show_in_markdown_and_bind_into_sql(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut scalar = cell(
        "cell_2",
        CellKind::Scalar,
        "sum(orders.revenue)",
        json!({"inputs": ["cell_1"]}),
    );
    scalar.name = Some("total".into());
    let (view, cx) = open(
        cx,
        notebook(vec![
            scalar,
            cell(
                "cell_3",
                CellKind::Markdown,
                "Total revenue: {{total}}",
                json!(null),
            ),
            cell(
                "cell_4",
                CellKind::Sql,
                "SELECT * FROM t WHERE amount > {{total}}",
                json!(null),
            ),
        ]),
    );
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    view.read_with(cx, |nb, _| {
        assert_eq!(
            nb.outputs.get("cell_2").unwrap().rows[0][0],
            CellValue::I64(32)
        );
        assert_eq!(
            nb.interpolate_markdown("cell_3").as_deref(),
            Some("Total revenue: 32")
        );
        let request = nb.run_request("cell_4").unwrap();
        assert_eq!(request.sql, "SELECT * FROM t WHERE amount > 32");
        assert!(!nb.binds_partial_scalar("cell_4"));
    });
    view.update(cx, |nb, _| {
        let mut partial = (*nb.outputs.get("cell_2").unwrap()).clone();
        partial.completeness = sqlly_services::notebook::Completeness::Truncated { cap: 2 };
        nb.outputs.publish("cell_2", partial);
        assert!(
            nb.binds_partial_scalar("cell_4"),
            "SQL that binds a total over truncated rows is not a complete result"
        );
        assert!(!nb.binds_partial_scalar("cell_1"));
    });
}

#[gpui::test]
fn scalar_formulas_cannot_read_sensitive_parameters(cx: &mut TestAppContext) {
    use sqlly_services::notebook::params::{ParamType, ParameterDef};
    let _root = AutosaveRoot::new();
    let mut doc = notebook(vec![
        cell(
            "cell_2",
            CellKind::Scalar,
            "param.region",
            json!({"inputs": []}),
        ),
        cell(
            "cell_3",
            CellKind::Scalar,
            "param.token",
            json!({"inputs": []}),
        ),
    ]);
    let mut region = ParameterDef::new("region", ParamType::String);
    region.default = Some(json!("EMEA"));
    let mut token = ParameterDef::new("token", ParamType::String);
    token.default = Some(json!("s3cret"));
    token.sensitive = true;
    doc.parameters = vec![region, token];
    let (view, cx) = open(cx, doc);
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    view.read_with(cx, |nb, _| {
        assert_eq!(
            nb.outputs.get("cell_2").unwrap().rows[0][0],
            CellValue::String("EMEA".into())
        );
        let leaked = nb.outputs.get("cell_3").is_some_and(|o| {
            o.rows
                .iter()
                .flatten()
                .any(|v| format!("{v:?}").contains("s3cret"))
        });
        assert!(!leaked, "a scalar cell displayed a sensitive parameter");
    });
}

#[cfg(not(target_arch = "wasm32"))]
#[gpui::test]
fn transforms_run_in_the_local_workspace(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    crate::ensure_test_tokio_runtime();
    // The notebook's workspace opens on a tokio worker.
    cx.dispatcher.allow_parking();
    let (view, cx) = open(
        cx,
        notebook(vec![cell(
            "cell_2",
            CellKind::Transform,
            "",
            json!({"input": "cell_1", "steps": [
                {"op": "group", "by": ["region"], "aggregates": [{"function": "sum", "column": "revenue"}]},
                {"op": "sort", "by": [{"column": "sum_revenue", "descending": true}]}
            ]}),
        )]),
    );
    let events = record(&view, cx);
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if view.read_with(cx, |nb, _| nb.outputs.get("cell_2").is_some()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    assert_eq!(run_cells(&events), ["cell_1"]);
    view.read_with(cx, |nb, cx| {
        let message = nb
            .visual_inputs("cell_2", VisualKind::Transform, false, cx)
            .message;
        let output = nb
            .outputs
            .get("cell_2")
            .unwrap_or_else(|| panic!("{message:?}"));
        let names: Vec<&str> = output.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["region", "sum_revenue"]);
        assert_eq!(output.rows[0][0], CellValue::String("West".into()));
        assert_eq!(nb.cell_state("cell_2"), CellStatus::Fresh);
    });
}

#[gpui::test]
fn every_visual_kind_draws(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        notebook(vec![
            cell(
                "c_bar",
                CellKind::Chart,
                "",
                json!({"input": "cell_1", "chart": {"kind": "bar", "x": {"column": "region"}, "y": [{"column": "revenue"}]}}),
            ),
            cell(
                "c_line",
                CellKind::Chart,
                "",
                json!({"input": "cell_1", "chart": {"kind": "line", "x": {"column": "day"}, "y": [{"column": "revenue"}], "date_grain": "month", "aggregate": "sum"}, "anomaly": {"window": 2, "k": 2.0}}),
            ),
            cell(
                "c_heat",
                CellKind::Chart,
                "",
                json!({"input": "cell_1", "chart": {"kind": "heatmap", "x": {"column": "region"}, "series": {"column": "day"}, "y": [{"column": "revenue"}]}}),
            ),
            cell("c_new", CellKind::Chart, "", json!({"input": "cell_1"})),
            cell(
                "c_table",
                CellKind::Table,
                "",
                json!({"input": "cell_1", "directives": ["-- sqlly grid: freeze=1"]}),
            ),
            cell(
                "c_pivot",
                CellKind::Pivot,
                "",
                json!({"input": "cell_1", "pivot": {"rows": ["region"], "value": "revenue"}}),
            ),
            cell("c_kpi", CellKind::Kpi, "", json!({"input": "cell_1"})),
            cell(
                "c_scalar",
                CellKind::Scalar,
                "max(orders.revenue)",
                json!({"inputs": ["cell_1"]}),
            ),
            cell(
                "c_transform",
                CellKind::Transform,
                "",
                json!({"input": "cell_1", "steps": [{"op": "limit", "rows": 2}]}),
            ),
        ]),
    );
    draw(&view, cx);
    let (columns, rows) = region_rows();
    let results = cx.update(|_, cx| cx.new(ResultsView::new));
    view.update(cx, |nb, cx| {
        nb.attach_results("cell_1", results.clone(), cx)
    });
    view.update(cx, |nb, cx| nb.request_run_from_host("cell_1", cx));
    results.update(cx, |r, cx| {
        r.handle_event(QueryEvent::Started, cx);
        r.handle_event(QueryEvent::ResultSet { index: 0, columns }, cx);
        r.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        r.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
    cx.run_until_parked();
    draw(&view, cx);
    cx.run_until_parked();
    for id in [
        "c_bar", "c_line", "c_heat", "c_new", "c_table", "c_pivot", "c_kpi", "c_scalar",
    ] {
        let shown = view.read_with(cx, |nb, _| nb.visual.slots.contains_key(id));
        assert!(shown, "{id} drew its view");
    }
    view.update(cx, |nb, cx| {
        for slot in nb.visual.slots.values() {
            slot.view.update(cx, |v, cx| v.toggle_editing(cx));
        }
    });
    draw(&view, cx);
    cx.run_until_parked();
    view.read_with(cx, |nb, cx| {
        let bar = &nb.visual.slots["c_bar"].view;
        assert!(bar.read(cx).chart.is_live(), "the bar chart draws live");
        let heat = &nb.visual.slots["c_heat"].view;
        assert!(
            !heat.read(cx).chart.is_live(),
            "the heatmap draws through the SVG renderer"
        );
        assert!(
            nb.visual.slots["c_table"]
                .view
                .read(cx)
                .table
                .results
                .is_some()
        );
        assert!(
            nb.visual.slots["c_pivot"]
                .view
                .read(cx)
                .pivot
                .grid
                .is_some()
        );
        let kpi = nb
            .outputs
            .get("c_kpi")
            .expect("a KPI defaulted to the first measure");
        assert_eq!(kpi.rows[0][1], CellValue::F64(32.0));
    });
}

#[gpui::test]
fn chart_suggestions_apply_with_one_click(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        notebook(vec![cell(
            "cell_2",
            CellKind::Chart,
            "",
            json!({"input": "cell_1"}),
        )]),
    );
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    draw(&view, cx);
    let chart = view.read_with(cx, |nb, _| nb.visual.slots["cell_2"].view.clone());
    let suggestions = chart.update(cx, |v, _| v.chart_suggestions());
    assert!(!suggestions.is_empty() && suggestions.len() <= 3);
    chart.update(cx, |v, cx| v.apply_suggestion(&suggestions[0], cx));
    cx.run_until_parked();
    view.read_with(cx, |nb, _| {
        let spec = &nb.model().cell("cell_2").unwrap().spec;
        assert!(
            spec["chart"]["y"].as_array().is_some_and(|y| !y.is_empty()),
            "{spec}"
        );
        assert_eq!(spec["input"], "cell_1");
    });
    let svg = chart
        .read_with(cx, |v, _| v.chart_svg(640.0, 360.0))
        .unwrap();
    assert!(svg.starts_with("<svg") && !svg.contains("<script"));
}

#[test]
fn every_dynamic_visual_key_is_translated() {
    use sqlly_services::notebook::cells::transform::{
        AggFunction, FilterTest, JoinKind, TransformStep,
    };
    use sqlly_services::report::chart::ChartKind;
    let mut keys: Vec<String> = Vec::new();
    keys.extend(
        ChartKind::ALL
            .iter()
            .map(|k| format!("notebook.visual.chart.kinds.{}", k.name())),
    );
    for (group, names) in [
        (
            "chart.aggregates",
            &[
                "none",
                "sum",
                "avg",
                "count",
                "count_distinct",
                "min",
                "max",
                "median",
            ][..],
        ),
        (
            "chart.sorts",
            &[
                "natural",
                "category_asc",
                "category_desc",
                "value_desc",
                "value_asc",
            ][..],
        ),
        (
            "chart.grains",
            &["none", "hour", "day", "week", "month", "quarter", "year"][..],
        ),
        ("chart.nulls", &["gap", "zero", "skip"][..]),
        ("chart.bar_modes", &["grouped", "stacked", "normalized"][..]),
        (
            "chart.formats",
            &["plain", "percent", "currency", "compact"][..],
        ),
        (
            "kpi.aggregates",
            &[
                "sum",
                "avg",
                "min",
                "max",
                "median",
                "count",
                "count_distinct",
                "last",
                "first_row",
            ][..],
        ),
        (
            "kpi.direction",
            &["higher_is_better", "lower_is_better", "neutral"][..],
        ),
        (
            "kpi.basis",
            &["level", "delta_absolute", "delta_percent"][..],
        ),
        (
            "kinds",
            &["chart", "table", "pivot", "kpi", "scalar", "transform"][..],
        ),
    ] {
        keys.extend(names.iter().map(|n| format!("notebook.visual.{group}.{n}")));
    }
    keys.extend(
        TransformStep::OPS
            .iter()
            .map(|o| format!("notebook.visual.transform.ops.{o}")),
    );
    keys.extend(
        FilterTest::ALL
            .iter()
            .map(|f| format!("notebook.visual.transform.tests.{}", f.name())),
    );
    keys.extend(
        AggFunction::ALL
            .iter()
            .map(|f| format!("notebook.visual.transform.functions.{}", f.name())),
    );
    keys.extend(
        JoinKind::ALL
            .iter()
            .map(|j| format!("notebook.visual.transform.joins.{}", j.name())),
    );
    for key in keys {
        assert!(sqlly_i18n::contains_key(&key), "{key}");
    }
}

#[test]
fn the_visual_screenshot_fixtures_parse_into_their_typed_specs() {
    use sqlly_services::notebook::cells::{chart, kpi, pivot, scalar, table, transform};
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/docs-shots/fixtures/notebooks");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("fixture folder") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("visual-") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let loaded = crate::notebook::files::load_file(&path.to_string_lossy(), &text, None)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        for cell in &loaded.doc.cells {
            let spec = cell.spec.clone();
            let ok = match cell.kind {
                CellKind::Chart => serde_json::from_value::<chart::ChartCellSpec>(spec).is_ok(),
                CellKind::Table => serde_json::from_value::<table::TableCellSpec>(spec).is_ok(),
                CellKind::Pivot => serde_json::from_value::<pivot::PivotCellSpec>(spec).is_ok(),
                CellKind::Kpi => serde_json::from_value::<kpi::KpiCellSpec>(spec).is_ok(),
                CellKind::Scalar => serde_json::from_value::<scalar::ScalarCellSpec>(spec).is_ok(),
                CellKind::Transform => {
                    let typed: Result<transform::TransformCellSpec, _> =
                        serde_json::from_value(spec);
                    typed.is_ok_and(|t| {
                        transform::compile(&t, &|id| Some(format!("nb_{id}"))).is_ok()
                    })
                }
                _ => true,
            };
            assert!(ok, "{name} {}: {}", cell.id, cell.spec);
        }
        seen += 1;
    }
    assert!(seen >= 8, "{seen}");
}

/// Run `cell_1` again, answering with `rows` (no result set at all when
/// `None`), after `mid_run` edits whatever it likes while it runs.
fn rerun_with(
    view: &Entity<NotebookView>,
    results: &Entity<ResultsView>,
    cx: &mut VisualTestContext,
    rows: Option<(Vec<ResultColumn>, Vec<Vec<CellValue>>)>,
    mid_run: impl FnOnce(&mut NotebookView),
) {
    view.update(cx, |nb, cx| nb.request_run_from_host("cell_1", cx));
    results.update(cx, |r, cx| {
        r.start_query(cx);
        r.handle_event(QueryEvent::Started, cx);
        if let Some((columns, rows)) = rows {
            r.handle_event(QueryEvent::ResultSet { index: 0, columns }, cx);
            r.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        }
    });
    view.update(cx, |nb, cx| {
        nb.dispatch_result(
            "cell_1",
            crate::notebook::view::DispatchOutcome::Started,
            cx,
        );
        mid_run(nb);
    });
    results.update(cx, |r, cx| {
        r.handle_event(QueryEvent::Completed { elapsed_ms: 2 }, cx)
    });
    cx.run_until_parked();
}

fn bar_chart() -> Cell {
    cell(
        "cell_2",
        CellKind::Chart,
        "",
        json!({"input": "cell_1", "chart": {"kind": "bar", "x": {"column": "region"}, "y": [{"column": "revenue"}], "aggregate": "sum"}}),
    )
}

#[gpui::test]
fn a_rerun_without_a_result_set_never_re_presents_the_old_output(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, notebook(vec![bar_chart()]));
    let (columns, rows) = region_rows();
    let results = run_all_with(&view, cx, columns, rows);
    assert!(view.read_with(cx, |nb, _| nb.outputs.get("cell_1").is_some()));

    view.update(cx, |nb, _| {
        nb.doc_mut_for_test()
            .set_source("cell_1", "EXEC dbo.refresh_totals");
    });
    rerun_with(&view, &results, cx, None, |_| {});
    view.read_with(cx, |nb, _| {
        assert!(
            nb.outputs.get("cell_1").is_none(),
            "the old rows are not this run's output"
        );
        assert!(
            !nb.doc().cell_state("cell_2").is_presentable_as_current(),
            "the chart over the old rows is not current"
        );
    });
}

#[gpui::test]
fn a_chart_over_an_input_that_went_stale_mid_run_is_not_current(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, notebook(vec![bar_chart()]));
    let (columns, rows) = region_rows();
    let results = run_all_with(&view, cx, columns.clone(), rows.clone());
    assert_eq!(
        view.read_with(cx, |nb, _| nb.cell_state("cell_2")),
        CellStatus::Fresh
    );

    rerun_with(&view, &results, cx, Some((columns, rows)), |nb| {
        nb.doc_mut_for_test()
            .set_source("cell_1", "SELECT region, day, revenue FROM sales_2026");
    });
    view.read_with(cx, |nb, _| {
        assert!(
            !nb.doc().cell_state("cell_1").is_presentable_as_current(),
            "the input was edited while it ran"
        );
        assert!(
            !nb.doc().cell_state("cell_2").is_presentable_as_current(),
            "a chart built on it is not current either"
        );
    });
}

#[gpui::test]
fn scalar_text_in_markdown_is_never_markup(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut scalar = cell(
        "cell_2",
        CellKind::Scalar,
        "'[win](https://evil.example) <b>x</b>'",
        json!({"inputs": ["cell_1"]}),
    );
    scalar.name = Some("top".into());
    let (view, cx) = open(
        cx,
        notebook(vec![
            scalar,
            cell("cell_3", CellKind::Markdown, "Top: {{top}}", json!(null)),
        ]),
    );
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    view.read_with(cx, |nb, _| {
        let shown = nb.interpolate_markdown("cell_3").unwrap();
        assert!(!shown.contains("[win]("), "{shown}");
        assert!(!shown.contains("<b>"), "{shown}");
        assert!(shown.starts_with("Top: \\[win\\]"), "{shown}");
    });
}

#[gpui::test]
fn entity_detail_cells_follow_the_row_selected_in_a_table(cx: &mut TestAppContext) {
    use sqlly_services::notebook::cells::entity_detail::{EntityDetailSpec, EntitySource};
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        notebook(vec![
            cell("cell_2", CellKind::Table, "", json!({"input": "cell_1"})),
            cell(
                "cell_3",
                CellKind::EntityDetail,
                "",
                json!({"source": {"cell": "cell_2", "row": 0}, "table": {"name": "Sales"}}),
            ),
        ]),
    );
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    draw(&view, cx);
    cx.run_until_parked();
    let grid = view
        .read_with(cx, |nb, cx| {
            let results = nb.visual.slots["cell_2"]
                .view
                .read(cx)
                .table
                .results
                .clone()?;
            results.read(cx).active_grid_state(cx)
        })
        .expect("the table drew its grid");
    let source_row = |cx: &mut VisualTestContext| {
        view.read_with(cx, |nb, _| {
            match EntityDetailSpec::from_spec(&nb.doc().spec("cell_3")).source {
                EntitySource::Row { row, .. } => row,
                other => panic!("{other:?}"),
            }
        })
    };
    assert_eq!(source_row(cx), 0);
    grid.update(cx, |s, cx| {
        s.selection = sqlly_datatable::Selection::Row(3);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(source_row(cx), 3, "the detail follows the selection");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.outputs.selected_row("cell_2")),
        Some(3)
    );
    // The row arrows still step from there.
    grid.update(cx, |s, cx| {
        s.selection = sqlly_datatable::Selection::None;
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |nb, _| nb.outputs.selected_row("cell_2")),
        None
    );
    assert_eq!(
        source_row(cx),
        3,
        "clearing the selection keeps the row shown"
    );
}

#[gpui::test]
fn clicks_on_an_svg_drawn_chart_set_its_cross_filter(cx: &mut TestAppContext) {
    use sqlly_services::report::svg::HitShape;
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        notebook(vec![
            // A reference line keeps the bar chart off the live grid chart.
            cell(
                "cell_2",
                CellKind::Chart,
                "",
                json!({"input": "cell_1", "chart": {"kind": "bar", "x": {"column": "region"},
                    "y": [{"column": "revenue"}], "reference_lines": [{"value": 5.0}]}}),
            ),
            cell(
                "cell_3",
                CellKind::Table,
                "",
                json!({"input": "cell_1", "follows": "cell_2"}),
            ),
        ]),
    );
    let (columns, rows) = region_rows();
    run_all_with(&view, cx, columns, rows);
    draw(&view, cx);
    cx.run_until_parked();
    let chart = view.read_with(cx, |nb, _| nb.visual.slots["cell_2"].view.clone());
    let (point, category) = chart.read_with(cx, |v, _| {
        assert!(!v.chart.is_live(), "drawn through the SVG renderer");
        let (_, scene) = v.chart.scene.clone().expect("the scene is kept");
        let bounds = v.chart.bounds.get();
        let (fw, fh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let scale = (fw / scene.width).min(fh / scene.height);
        let (dx, dy) = (
            (fw - scene.width * scale) / 2.0,
            (fh - scene.height * scale) / 2.0,
        );
        let hit = scene
            .hits
            .iter()
            .find(|h| h.category == "West")
            .expect("the West bar is clickable");
        let HitShape::Rect { x, y, w, h } = &hit.shape else {
            panic!("bars are rectangles")
        };
        let (sx, sy) = (x + w / 2.0, y + h / 2.0);
        let point = gpui::point(
            bounds.origin.x + px(dx + sx * scale),
            bounds.origin.y + px(dy + sy * scale),
        );
        (point, hit.category.clone())
    });
    chart.update(cx, |v, cx| v.static_chart_clicked(point, cx));
    cx.run_until_parked();
    view.read_with(cx, |nb, _| {
        let filter = nb.outputs.filter("cell_2").expect("the click set a filter");
        assert_eq!(filter.column, "region");
        assert_eq!(filter.value, category);
        assert_eq!(
            nb.outputs.get("cell_3").unwrap().rows.len(),
            2,
            "the table that follows shows West only"
        );
    });
    // A second click on the same bar clears it.
    chart.update(cx, |v, cx| v.static_chart_clicked(point, cx));
    cx.run_until_parked();
    assert!(view.read_with(cx, |nb, _| nb.outputs.filter("cell_2").is_none()));
}

#[test]
fn scene_points_undo_the_contain_fit() {
    let scene = sqlly_services::report::svg::Scene::new(400.0, 200.0);
    let (x, y) = super::chart::scene_point(&scene, 800.0, 600.0, 400.0, 300.0).unwrap();
    assert!((x - 200.0).abs() < 1e-3 && (y - 100.0).abs() < 1e-3);
    let (x, y) = super::chart::scene_point(&scene, 800.0, 600.0, 0.0, 100.0).unwrap();
    assert!((x - 0.0).abs() < 1e-3 && (y - 0.0).abs() < 1e-3, "{x} {y}");
    assert!(super::chart::scene_point(&scene, 0.0, 10.0, 1.0, 1.0).is_none());
}
