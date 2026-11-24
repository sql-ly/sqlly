//! Windowed tests for the analysis cells and run history: each kind runs
//! through the shell's queue, run records are written and redacted, run
//! comparison explains a parameter change, assertions stay inconclusive on
//! cut-short inputs, sweeps refuse production without authorization,
//! actual plans wait for confirmation, and entity detail lookups are
//! read-only and row-limited.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext as _, Entity, ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
    px, size,
};
use serde_json::json;
use sqlly_core::{CellValue, ConnectionEnvironment, ResultColumn};
use sqlly_intel::query::safety::classify_select_only_for;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::QueryEvent;
use sqlly_services::analytics::assertions::AssertionStatus;
use sqlly_services::notebook::params::{ParamType, ParameterDef};
use sqlly_services::notebook::{Cell, CellKind, ConnectionRef, ExecutionPolicy, NotebookDoc};
use sqlly_services::notebook_security::MemoryCacheKey;
use sqlly_services::notebook_security::cache::MasterKey;

use super::{AnalysisReply, AnalysisRequest, AnalysisWork, CellViewHandle, QueryRows};
use crate::notebook::cells::plan::Prompt;
use crate::notebook::connection::ConnectionChoice;
use crate::notebook::history::compare::ChangeCause;
use crate::notebook::history::store::HistoryStores;
use crate::notebook::history::{details_of, run_tests};
use crate::notebook::view::{CellStatus, NotebookEvent, NotebookOpening, NotebookView};
use crate::results::ResultsView;

const PROD_ID: &str = "6b1f3c9e-6a55-4b7e-9b0b-3f8f2d1c0a11";
const DEV_ID: &str = "0c3d2b1a-5f4e-4d3c-8b2a-1f0e9d8c7b6a";

fn choices() -> Rc<dyn Fn() -> Vec<ConnectionChoice>> {
    Rc::new(|| {
        [
            (DEV_ID, "Dev", ConnectionEnvironment::Dev),
            (PROD_ID, "Sales", ConnectionEnvironment::Production),
        ]
        .into_iter()
        .map(|(id, label, environment)| ConnectionChoice {
            profile_id: id.into(),
            label: label.into(),
            environment,
            database_type: DatabaseType::SqlServer,
            database: None,
            ..ConnectionChoice::default()
        })
        .collect()
    })
}

struct Roots {
    _autosave: tempfile::TempDir,
    history: tempfile::TempDir,
}

impl Roots {
    fn new() -> Self {
        let autosave = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(autosave.path().to_path_buf()));
        Self {
            _autosave: autosave,
            history: tempfile::tempdir().unwrap(),
        }
    }

    fn stores(&self) -> Arc<HistoryStores> {
        Arc::new(HistoryStores::open(
            self.history.path(),
            &MemoryCacheKey::with_key(MasterKey::from_bytes([9; 32])),
        ))
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        crate::notebook::autosave::set_test_root(None);
    }
}

fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    open_with(cx, model, choices())
}

/// [`open`] with every connection on `db`.
fn open_on(
    cx: &mut TestAppContext,
    model: NotebookDoc,
    db: DatabaseType,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    let all = choices();
    let choices: Rc<dyn Fn() -> Vec<ConnectionChoice>> = Rc::new(move || {
        all()
            .into_iter()
            .map(|c| ConnectionChoice {
                database_type: db,
                ..c
            })
            .collect()
    });
    open_with(cx, model, choices)
}

fn open_with(
    cx: &mut TestAppContext,
    model: NotebookDoc,
    choices: Rc<dyn Fn() -> Vec<ConnectionChoice>>,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| NotebookView::with_choices(model, NotebookOpening::New, choices, cx));
        gpui_component::Root::new(nb, window, cx)
    });
    let view = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .unwrap()
    });
    (view, vcx)
}

fn draw(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(
        gpui::Point::default(),
        size(px(1400.0), px(900.0)),
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
        cx.subscribe(view, move |_, e: &NotebookEvent, _| {
            seen.borrow_mut().push(e.clone())
        })
        .detach();
    });
    events
}

fn requests(events: &Rc<RefCell<Vec<NotebookEvent>>>) -> Vec<AnalysisRequest> {
    events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            NotebookEvent::AnalysisQuery(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn cell(nb: &mut NotebookDoc, id: &str, kind: CellKind, source: &str, spec: serde_json::Value) {
    let mut c = Cell::new(id, kind, ExecutionPolicy::default());
    c.source = source.into();
    c.spec = spec;
    nb.cells.push(c);
}

fn col(name: &str, ty: &str) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
    }
}

/// Attach a results view to each SQL cell so runs can complete.
fn attach(
    view: &Entity<NotebookView>,
    cx: &mut VisualTestContext,
    ids: &[&str],
) -> Vec<Entity<ResultsView>> {
    let results: Vec<_> = ids
        .iter()
        .map(|_| cx.update(|_, cx| cx.new(ResultsView::new)))
        .collect();
    view.update(cx, |nb, cx| {
        for (id, r) in ids.iter().zip(&results) {
            nb.attach_results(id, r.clone(), cx);
        }
    });
    results
}

fn feed(
    results: &Entity<ResultsView>,
    cx: &mut VisualTestContext,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    seen: Option<usize>,
) {
    results.update(cx, |rv, cx| {
        rv.clear(cx);
        rv.handle_event(QueryEvent::Started, cx);
        rv.handle_event(QueryEvent::ResultSet { index: 0, columns }, cx);
        rv.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        if let Some(rows_seen) = seen {
            rv.handle_event(
                QueryEvent::RowsProgress {
                    index: 0,
                    rows_seen,
                },
                cx,
            );
        }
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 3 }, cx);
    });
    cx.run_until_parked();
}

fn orders() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let rows = (1..=6)
        .map(|i| {
            vec![
                CellValue::I64(if i == 6 { 5 } else { i }),
                CellValue::String(if i % 2 == 0 { "EMEA" } else { "APAC" }.into()),
                CellValue::F64(i as f64 * 10.0),
            ]
        })
        .collect();
    (
        vec![
            col("id", "int"),
            col("region", "nvarchar"),
            col("amount", "float"),
        ],
        rows,
    )
}

#[gpui::test]
fn profile_cells_profile_their_input_and_label_coverage(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let mut nb = NotebookDoc::empty("nb_profile", "Profile");
    cell(
        &mut nb,
        "orders",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders",
        json!(null),
    );
    cell(
        &mut nb,
        "prof",
        CellKind::Profile,
        "",
        json!({"input": "orders", "sort": "null_rate"}),
    );
    let (view, cx) = open(cx, nb);
    draw(&view, cx);
    let results = attach(&view, cx, &["orders"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    feed(&results[0], cx, cols, rows, None);
    draw(&view, cx);
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("prof"), CellStatus::Fresh);
        let Some(CellViewHandle::Profile(p)) = nb.analysis.views.get("prof") else {
            panic!("profile view")
        };
        let p = p.read(cx);
        let profile = p.profile().expect("profiled");
        assert_eq!(profile.columns.len(), 3);
        assert!(p.coverage().unwrap().is_complete());
        assert!(nb.analysis.outputs.contains_key("prof"));
    });
}

#[gpui::test]
fn failing_assertions_show_in_the_top_bar_and_persist_with_evidence(cx: &mut TestAppContext) {
    let roots = Roots::new();
    let stores = roots.stores();
    let mut nb = NotebookDoc::empty("nb_tests", "Tests");
    cell(
        &mut nb,
        "orders",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders",
        json!(null),
    );
    cell(
        &mut nb,
        "checks",
        CellKind::Assertion,
        "",
        json!({"input": "orders", "assertions": [
            {"kind": "row_count", "min": 1},
            {"kind": "unique", "columns": ["id"], "name": "unique id"}
        ]}),
    );
    let (view, cx) = open(cx, nb);
    view.update(cx, |nb, _| nb.set_history_stores(stores.clone()));
    draw(&view, cx);
    let results = attach(&view, cx, &["orders"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    feed(&results[0], cx, cols, rows, None);
    draw(&view, cx);
    view.read_with(cx, |nb, _| {
        let summary = nb.test_summary();
        assert_eq!((summary.passed, summary.failed), (1, 1));
        assert!(!nb.is_running());
    });
    let runs = stores.list("nb_tests", "nb_tests").unwrap();
    assert_eq!(runs.len(), 1, "Run All wrote one record");
    let run = &runs[0];
    assert_eq!(run.cells.len(), 2);
    let details = details_of(run);
    assert_eq!(run_tests(&details).failed, 1);
    let outcomes: Vec<sqlly_services::analytics::assertions::AssertionOutcome> =
        serde_json::from_value(details.assertions["checks"].clone()).unwrap();
    assert_eq!(outcomes[1].status, AssertionStatus::Fail);
    assert!(outcomes[1].evidence.failing_count >= 1);
    let sample = &details.assertion_samples["checks"][&1];
    assert!(sample.rows.iter().all(|r| r[0] == CellValue::I64(5)));
    assert!(details.fingerprints.contains_key("orders"));
    cx.run_until_parked();
    assert_eq!(
        stores
            .run_output(
                "nb_tests",
                details.identities.get("orders"),
                &run.run_id,
                "orders"
            )
            .expect("the run's output is kept")
            .rows
            .len(),
        6
    );
    // A single-cell run is recorded too.
    view.update(cx, |nb, cx| nb.request_run_from_host("checks", cx));
    cx.run_until_parked();
    assert_eq!(stores.list("nb_tests", "nb_tests").unwrap().len(), 2);
}

#[gpui::test]
fn explain_changes_compares_recorded_runs(cx: &mut TestAppContext) {
    use sqlly_services::notebook::cells::ai::AiCellSpec;
    let roots = Roots::new();
    let stores = roots.stores();
    let mut nb = NotebookDoc::empty("nb_explain", "Explain");
    cell(
        &mut nb,
        "orders",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders",
        json!(null),
    );
    let (view, cx) = open(cx, nb);
    view.update(cx, |nb, _| nb.set_history_stores(stores.clone()));
    draw(&view, cx);
    let (cols, rows) = orders();
    for keep in [6, 4] {
        let results = attach(&view, cx, &["orders"]);
        view.update(cx, |nb, cx| nb.run_all(cx));
        feed(&results[0], cx, cols.clone(), rows[..keep].to_vec(), None);
        cx.run_until_parked();
    }
    let runs: Vec<String> = view.update(cx, |nb, _| {
        nb.recorded_runs()
            .iter()
            .map(|r| r.run_id.clone())
            .collect()
    });
    assert_eq!(runs.len(), 2);
    let why = view.update(cx, |nb, cx| {
        nb.insert_ai_cell(
            Some("orders"),
            sqlly_services::notebook::cells::ai::AgentMode::ExplainChanges,
            None,
            "What changed?",
            cx,
        )
    });

    // Nothing chosen: the last AI read stays the baseline.
    assert!(view.update(cx, |nb, _| {
        nb.ai_compare_runs(&Default::default()).is_none()
    }));
    view.update(cx, |nb, cx| {
        nb.ai_set_compare(&why, runs[0].clone(), runs[1].clone(), cx)
    });
    let compare = view
        .read_with(cx, |nb, _| {
            AiCellSpec::of(nb.model().cell(&why).unwrap()).compare
        })
        .expect("saved in the cell");
    assert_eq!(
        (compare.before.as_str(), compare.after.as_str()),
        (runs[0].as_str(), runs[1].as_str())
    );
    let (before, after) = view
        .update(cx, |nb, _| nb.ai_compare_runs(&compare))
        .expect("both runs are recorded");
    assert_eq!(before.outputs["orders"].rows.len(), 6);
    let after = after.expect("a second run");
    assert_eq!(after.outputs["orders"].rows.len(), 4);
    assert!(before.parts.contains_key("orders"));
    let ctx = view.read_with(cx, |nb, cx| {
        nb.ai_run_inputs(&why, cx)
            .unwrap()
            .context(Default::default())
    });
    let findings =
        sqlly_services::notebook_agent::explain::explain_runs(&ctx, &before, Some(&after));
    let keys: Vec<String> = findings
        .iter()
        .filter_map(|f| f.message.as_ref().map(|m| m.key.clone()))
        .collect();
    assert!(
        keys.contains(&"notebook.ai.explain.rows_diff".to_string()),
        "{keys:?}"
    );
    // Against now, and back to the last AI read.
    view.update(cx, |nb, cx| {
        nb.ai_set_compare(&why, runs[0].clone(), String::new(), cx)
    });
    let now = view
        .read_with(cx, |nb, _| {
            AiCellSpec::of(nb.model().cell(&why).unwrap()).compare
        })
        .unwrap();
    assert!(
        view.update(cx, |nb, _| nb.ai_compare_runs(&now))
            .unwrap()
            .1
            .is_none()
    );
    view.update(cx, |nb, cx| {
        nb.ai_set_compare(&why, String::new(), String::new(), cx)
    });
    assert!(
        view.read_with(cx, |nb, _| AiCellSpec::of(nb.model().cell(&why).unwrap())
            .compare)
            .is_none()
    );
}

#[gpui::test]
fn the_top_bar_test_summary_comes_back_from_the_last_run_on_open(cx: &mut TestAppContext) {
    let roots = Roots::new();
    let stores = roots.stores();
    let mut nb = NotebookDoc::empty("nb_reopen", "Tests");
    cell(
        &mut nb,
        "orders",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders",
        json!(null),
    );
    cell(
        &mut nb,
        "checks",
        CellKind::Assertion,
        "",
        json!({"input": "orders", "assertions": [
            {"kind": "row_count", "min": 1},
            {"kind": "unique", "columns": ["id"], "name": "unique id"}
        ]}),
    );
    let saved = nb.clone();
    let (view, vcx) = open(cx, nb);
    view.update(vcx, |nb, _| nb.set_history_stores(stores.clone()));
    draw(&view, vcx);
    let results = attach(&view, vcx, &["orders"]);
    view.update(vcx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    feed(&results[0], vcx, cols, rows, None);
    vcx.run_until_parked();
    let scope = view.read_with(vcx, |nb, _| nb.trust.scope.clone());
    let scope_dir = scope.clone().unwrap();
    assert_eq!(stores.list(&scope_dir, "nb_reopen").unwrap().len(), 1);

    // The same notebook opened again (its trust record, so its data scope)
    // shows the recorded results before anything runs.
    let (again, vcx) = open(cx, saved);
    again.update(vcx, |nb, _| {
        nb.set_history_stores(stores.clone());
        nb.trust.scope = scope.clone();
    });
    assert_eq!(again.read_with(vcx, |nb, _| nb.test_summary().total()), 0);
    draw(&again, vcx);
    let summary = again.read_with(vcx, |nb, _| nb.test_summary());
    assert_eq!((summary.passed, summary.failed), (1, 1));
    assert!(!again.read_with(vcx, |nb, _| nb.is_running()));
}

#[gpui::test]
fn assertions_on_a_cut_short_input_are_inconclusive(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let mut nb = NotebookDoc::empty("nb_cut", "Cut");
    cell(
        &mut nb,
        "orders",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders",
        json!(null),
    );
    cell(
        &mut nb,
        "checks",
        CellKind::Assertion,
        "",
        json!({"input": "orders", "kind": "unique", "columns": ["id"]}),
    );
    let (view, cx) = open(cx, nb);
    let results = attach(&view, cx, &["orders"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    feed(&results[0], cx, cols, rows, Some(50_000));
    view.read_with(cx, |nb, _| {
        let s = nb.test_summary();
        assert_eq!(
            (s.inconclusive, s.failed),
            (1, 0),
            "a truncated input never fails a check"
        );
    });
}

#[gpui::test]
fn run_records_redact_sensitive_parameters_and_compare_explains_the_change(
    cx: &mut TestAppContext,
) {
    let roots = Roots::new();
    let stores = roots.stores();
    let mut nb = NotebookDoc::empty("nb_params", "Params");
    let mut region = ParameterDef::new("region", ParamType::String);
    region.default = Some(json!("EMEA"));
    let mut token = ParameterDef::new("token", ParamType::String);
    token.sensitive = true;
    token.default = Some(json!("hunter2-secret"));
    nb.parameters = vec![region, token];
    cell(
        &mut nb,
        "orders",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders WHERE region = :region AND tenant = :token",
        json!(null),
    );
    let (view, cx) = open(cx, nb);
    view.update(cx, |nb, _| nb.set_history_stores(stores.clone()));
    let results = attach(&view, cx, &["orders"]);
    let (cols, rows) = orders();
    view.update(cx, |nb, cx| nb.run_all(cx));
    feed(&results[0], cx, cols.clone(), rows.clone(), None);
    view.update(cx, |nb, cx| {
        nb.set_param("region", "APAC", cx);
        nb.run_all(cx);
    });
    feed(&results[0], cx, cols, rows[..3].to_vec(), None);
    let file = stores.runs_dir("nb_params").join("nb_params.runs.jsonl");
    let text = std::fs::read_to_string(file).unwrap();
    assert!(
        !text.contains("hunter2-secret"),
        "sensitive values are never written"
    );
    assert!(text.contains("APAC") && text.contains("EMEA"));
    let runs = stores.list("nb_params", "nb_params").unwrap();
    assert_eq!(runs.len(), 2);
    view.update(cx, |nb, cx| {
        for r in &runs {
            nb.toggle_run_selected(&r.run_id, cx);
        }
        nb.compare_selected_runs(cx);
    });
    view.read_with(cx, |nb, _| {
        let cmp = nb.analysis.history.comparison.as_ref().expect("compared");
        let orders = &cmp.cells[0];
        assert_eq!(
            orders.causes,
            vec![ChangeCause::Parameters(vec!["region".into()])]
        );
        assert_eq!((orders.rows_before, orders.rows_after), (Some(6), Some(3)));
        assert!(cmp.params.iter().any(|p| p.name == "region"));
    });
    view.update(cx, |nb, cx| {
        nb.update_layout(
            |l| l.left_page = crate::notebook::layout::LeftRailPage::History,
            cx,
        )
    });
    draw(&view, cx);
}

#[gpui::test]
fn diff_cells_compare_two_results_by_key(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let mut nb = NotebookDoc::empty("nb_diff", "Diff");
    cell(&mut nb, "before", CellKind::Sql, "SELECT 1", json!(null));
    cell(&mut nb, "after", CellKind::Sql, "SELECT 2", json!(null));
    cell(
        &mut nb,
        "cmp",
        CellKind::Diff,
        "",
        json!({"inputs": ["before", "after"], "keys": ["id"]}),
    );
    let (view, cx) = open(cx, nb);
    draw(&view, cx);
    let results = attach(&view, cx, &["before", "after"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    let unique: Vec<_> = rows[..5].to_vec();
    feed(&results[0], cx, cols.clone(), unique.clone(), None);
    let mut changed = unique.clone();
    changed[0][2] = CellValue::F64(999.0);
    changed.pop();
    feed(&results[1], cx, cols, changed, None);
    draw(&view, cx);
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("cmp"), CellStatus::Fresh);
        let Some(CellViewHandle::Diff(d)) = nb.analysis.views.get("cmp") else {
            panic!()
        };
        let diff = d.read(cx).result().unwrap().as_ref().unwrap();
        assert_eq!(
            (
                diff.summary.changed,
                diff.summary.removed,
                diff.summary.added
            ),
            (1, 1, 0)
        );
    });
}

fn prod_doc(spec: serde_json::Value) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nb_plan", "Plan");
    nb.connection_refs.push(ConnectionRef {
        id: "sales".into(),
        profile_id: Some(PROD_ID.into()),
        label: "Sales".into(),
        database: None,
        extra: Default::default(),
    });
    let mut region = ParameterDef::new("region", ParamType::String);
    region.default = Some(json!("EMEA"));
    nb.parameters = vec![region];
    cell(
        &mut nb,
        "q",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders WHERE region = :region",
        json!(null),
    );
    nb.cells[0].connection_ref = Some("sales".into());
    cell(&mut nb, "plan", CellKind::Plan, "", spec);
    nb
}

/// Run All: the query cell completes, then the plan cell starts.
fn run_query(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let results = attach(view, cx, &["q"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    feed(&results[0], cx, cols, rows, None);
}

fn plan_reply() -> AnalysisReply {
    AnalysisReply::Plan {
        plans: Ok(vec![crate::notebook::cells::plan::tests::plan(&[
            ("Select", 0.1),
            ("Index Seek", 0.3),
        ])]),
        compare: None,
        stats: None,
    }
}

#[gpui::test]
fn sweeps_on_production_wait_for_explicit_authorization(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(
        cx,
        prod_doc(
            json!({"input": "q", "sweep": {"parameter": "region", "values": ["EMEA", "APAC"], "repetitions": 2}}),
        ),
    );
    draw(&view, cx);
    let events = record(&view, cx);
    run_query(&view, cx);
    let first = requests(&events);
    assert_eq!(first.len(), 1);
    assert!(matches!(
        first[0].work,
        AnalysisWork::Plan { actual: false, .. }
    ));
    view.update(cx, |nb, cx| {
        nb.analysis_reply(first[0].request_id, plan_reply(), cx)
    });
    assert_eq!(
        requests(&events).len(),
        1,
        "no sweep query runs on production without a yes"
    );
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("plan"), CellStatus::Failed);
        let Some(CellViewHandle::Plan(p)) = nb.analysis.views.get("plan") else {
            panic!()
        };
        assert_eq!(p.read(cx).prompt(), Some(Prompt::AuthorizeProductionSweep));
    });
    view.update(cx, |nb, cx| nb.authorize_and_run("plan", cx));
    let second = requests(&events);
    view.update(cx, |nb, cx| {
        nb.analysis_reply(second[1].request_id, plan_reply(), cx)
    });
    let all = requests(&events);
    let AnalysisWork::Sweep { runs, .. } = &all[2].work else {
        panic!("the authorized run sweeps")
    };
    assert_eq!(runs.len(), 4);
    assert!(runs[0].contains("N'EMEA'") && runs[2].contains("N'APAC'"));
    assert!(all[2].target.read_only);
    view.update(cx, |nb, cx| {
        let timings = (0..4).map(|i| Ok((10 + i as u64, None))).collect();
        nb.analysis_reply(all[2].request_id, AnalysisReply::Sweep(timings), cx)
    });
    draw(&view, cx);
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("plan"), CellStatus::Fresh);
        let Some(CellViewHandle::Plan(p)) = nb.analysis.views.get("plan") else {
            panic!()
        };
        let sweep = &p.read(cx).result().unwrap().sweep;
        assert_eq!(sweep.len(), 2);
        assert_eq!(sweep[0].timing.runs, 2);
    });
    // The authorization was for one run only.
    view.update(cx, |nb, cx| nb.request_run_from_host("plan", cx));
    let n = requests(&events).len();
    view.update(cx, |nb, cx| {
        nb.analysis_reply(requests(&events)[n - 1].request_id, plan_reply(), cx)
    });
    assert_eq!(requests(&events).len(), n);
}

#[gpui::test]
fn actual_plans_wait_for_confirmation_and_run_read_only(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(
        cx,
        prod_doc(json!({"input": "q", "mode": "actual", "compare": {"run": "previous"}})),
    );
    let events = record(&view, cx);
    run_query(&view, cx);
    assert!(
        requests(&events).is_empty(),
        "an actual plan never runs without a yes"
    );
    view.read_with(cx, |nb, cx| {
        let Some(CellViewHandle::Plan(p)) = nb.analysis.views.get("plan") else {
            panic!()
        };
        assert_eq!(p.read(cx).prompt(), Some(Prompt::ConfirmActual));
    });
    view.update(cx, |nb, cx| nb.authorize_and_run("plan", cx));
    let r = requests(&events);
    assert_eq!(r.len(), 1);
    let AnalysisWork::Plan { actual, sql, .. } = &r[0].work else {
        panic!()
    };
    assert!(*actual && sql.contains("STATISTICS XML"));
    assert!(r[0].target.read_only);
    view.update(cx, |nb, cx| {
        nb.analysis_reply(r[0].request_id, plan_reply(), cx)
    });
    draw(&view, cx);
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("plan"), CellStatus::Fresh);
        let Some(CellViewHandle::Plan(p)) = nb.analysis.views.get("plan") else {
            panic!()
        };
        let result = p.read(cx).result().unwrap();
        assert!(result.actual);
        assert!(
            result.baseline.as_ref().unwrap().is_err(),
            "no earlier run to compare with yet"
        );
    });
}

#[gpui::test]
fn entity_detail_lookups_are_read_only_and_limited(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let mut nb = NotebookDoc::empty("nb_entity", "Entity");
    cell(
        &mut nb,
        "customers",
        CellKind::Sql,
        "SELECT * FROM dbo.Customers",
        json!(null),
    );
    cell(
        &mut nb,
        "detail",
        CellKind::EntityDetail,
        "",
        json!({"source": {"cell": "customers", "row": 1}, "input": "customers",
        "table": {"schema": "dbo", "name": "Customers"}, "limit": 5,
        "relationships": [
            {"direction": "incoming", "schema": "dbo", "table": "Orders", "columns": [["CustomerID", "CustomerID"]]},
            {"direction": "outgoing", "schema": "dbo", "table": "Regions", "columns": [["RegionID", "RegionID"]]}
        ]}),
    );
    let (view, cx) = open(cx, nb);
    draw(&view, cx);
    let events = record(&view, cx);
    let results = attach(&view, cx, &["customers"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let s = |t: &str| CellValue::String(t.into());
    feed(
        &results[0],
        cx,
        vec![col("CustomerID", "nchar"), col("RegionID", "int")],
        vec![
            vec![s("ALFKI"), CellValue::I64(1)],
            vec![s("O'BRI"), CellValue::Null],
        ],
        None,
    );
    let r = requests(&events);
    assert_eq!(r.len(), 1);
    assert!(r[0].target.read_only);
    let AnalysisWork::Rows { queries, max_rows } = &r[0].work else {
        panic!()
    };
    assert_eq!(*max_rows, 6);
    assert_eq!(queries.len(), 1, "a NULL key is not looked up");
    assert!(classify_select_only_for(DatabaseType::SqlServer, &queries[0]).select_only);
    assert!(
        queries[0].starts_with("SELECT TOP 6 * FROM [dbo].[Orders]"),
        "{}",
        queries[0]
    );
    assert!(queries[0].contains("N'O''BRI'"));
    let rows: Vec<Vec<CellValue>> = (0..6).map(|i| vec![CellValue::I64(i)]).collect();
    view.update(cx, |nb, cx| {
        nb.analysis_reply(
            r[0].request_id,
            AnalysisReply::Rows(vec![Ok(QueryRows {
                columns: vec![col("OrderID", "int")],
                rows,
                truncated: false,
                elapsed_ms: 1,
            })]),
            cx,
        )
    });
    draw(&view, cx);
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("detail"), CellStatus::Fresh);
        let Some(CellViewHandle::EntityDetail(e)) = nb.analysis.views.get("detail") else {
            panic!()
        };
        let related = e.read(cx).related();
        let orders = related.iter().find(|r| r.index == 0).unwrap();
        assert!(orders.truncated);
        assert_eq!(orders.result.as_ref().unwrap().1.len(), 5);
        assert!(
            related
                .iter()
                .find(|r| r.index == 1)
                .unwrap()
                .result
                .is_err()
        );
    });
}

#[test]
fn docs_shot_fixtures_load_and_their_seeded_runs_compare() {
    use sqlly_services::notebook::cells::{
        assertion::AssertionCellSpec, diff::DiffCellSpec, entity_detail::EntityDetailSpec,
        plan::PlanCellSpec, profile::ProfileCellSpec,
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/docs-shots/fixtures/notebooks");
    for name in ["analysis-checks.sqllynb", "analysis-forensics.sqllynb"] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        let loaded = sqlly_services::notebook::load(
            Some(name),
            &text,
            &mut sqlly_services::notebook::SeededIds(1),
            Some(DatabaseType::SqlServer),
        )
        .unwrap();
        for cell in &loaded.doc.cells {
            match cell.kind {
                CellKind::Profile => {
                    assert!(!ProfileCellSpec::from_spec(&cell.spec).input.is_empty())
                }
                CellKind::Assertion => {
                    let spec = AssertionCellSpec::from_spec(&cell.spec);
                    assert!(
                        spec.invalid.is_empty() && spec.assertions.len() == 5,
                        "{name}"
                    );
                }
                CellKind::Diff => assert!(DiffCellSpec::from_spec(&cell.spec).is_configured()),
                CellKind::Plan => assert!(PlanCellSpec::from_spec(&cell.spec).sweep.is_some()),
                CellKind::EntityDetail => {
                    assert!(
                        !EntityDetailSpec::from_spec(&cell.spec)
                            .relationships
                            .is_empty()
                    )
                }
                _ => {}
            }
        }
    }
    let runs: Vec<sqlly_services::notebook::RunRecord> =
        std::fs::read_to_string(dir.join("analysis-checks.runs.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    let upstream = |c: &str| {
        if c == "cell_4" {
            vec!["cell_2".to_string()]
        } else {
            Vec::new()
        }
    };
    let cmp = crate::notebook::history::compare::compare_runs(&runs[0], &runs[1], &upstream);
    assert_eq!(
        cmp.cells[0].causes,
        vec![ChangeCause::Parameters(vec!["year".into()])]
    );
    assert_eq!(
        cmp.cells[1].causes,
        vec![ChangeCause::Upstream(vec!["cell_2".into()])]
    );
    assert_eq!(run_tests(&details_of(&runs[1])).failed, 1);
}

/// A plan doc whose query `q` is a variant `alt` with `alt_sql` on
/// `alt_slot`, compared from an estimated plan cell.
fn compare_doc(alt_sql: &str, alt_slot: &str) -> NotebookDoc {
    let mut nb = prod_doc(json!({"input": "q", "compare": {"cell": "alt"}}));
    nb.connection_refs.push(ConnectionRef {
        id: "dev".into(),
        profile_id: Some(DEV_ID.into()),
        label: "Dev".into(),
        database: None,
        extra: Default::default(),
    });
    let plan = nb.cells.pop().unwrap();
    cell(&mut nb, "alt", CellKind::Sql, alt_sql, json!(null));
    nb.cells[1].connection_ref = Some(alt_slot.into());
    nb.cells.push(plan);
    nb
}

fn run_with_alt(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let results = attach(view, cx, &["q", "alt"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    feed(&results[0], cx, cols.clone(), rows.clone(), None);
    feed(&results[1], cx, cols, rows, None);
}

#[gpui::test]
fn estimated_plans_run_read_only(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(cx, prod_doc(json!({"input": "q"})));
    let events = record(&view, cx);
    run_query(&view, cx);
    let r = requests(&events);
    assert_eq!(r.len(), 1);
    assert!(matches!(
        r[0].work,
        AnalysisWork::Plan { actual: false, .. }
    ));
    assert!(
        r[0].target.read_only,
        "an estimated plan runs read-only too"
    );
}

/// The profile the host runs `request` on (the cell's read-only clone of
/// a SQL Server connection), as `notebook_cell_profile_message` builds it.
fn sql_server_profile_for(request: &AnalysisRequest) -> sqlly_core::ConnectionProfile {
    let base = sqlly_core::ConnectionProfile::sql_login(
        "Sales",
        "localhost",
        "sa",
        sqlly_core::SecretRef {
            provider: sqlly_core::SecretProvider::InMemory,
            key: "password".into(),
        },
    );
    crate::notebook::connection::profile_for_target(&request.target, |_| Some(base.clone()), None)
        .unwrap()
}

/// What the TDS driver sends for the request, batch by batch.
fn sent_batches(request: &AnalysisRequest) -> Vec<Result<String, String>> {
    let AnalysisWork::Plan { sql, .. } = &request.work else {
        panic!("a plan capture")
    };
    sqlly_engines::tds::planned_batches(&sql_server_profile_for(request), sql)
        .into_iter()
        .map(|b| {
            b.map(|t| t.replace("\r\n", "\n"))
                .map_err(|e| e.to_string())
        })
        .collect()
}

#[gpui::test]
fn estimated_plans_on_sql_server_send_showplan_alone_in_its_batch(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(cx, prod_doc(json!({"input": "q"})));
    let events = record(&view, cx);
    run_query(&view, cx);
    let r = requests(&events);
    let AnalysisWork::Plan { sql, .. } = &r[0].work else {
        panic!()
    };
    assert_eq!(
        sql.replace("\r\n", "\n"),
        "SET SHOWPLAN_XML ON;\nGO\nSELECT * FROM dbo.Orders WHERE region = N'EMEA'\nGO\nSET SHOWPLAN_XML OFF;"
    );
    assert_eq!(
        sent_batches(&r[0]),
        [
            Ok("SET SHOWPLAN_XML ON;".to_string()),
            Ok("SELECT * FROM dbo.Orders WHERE region = N'EMEA'".to_string()),
            Ok("SET SHOWPLAN_XML OFF;".to_string()),
        ],
        "SET SHOWPLAN_XML is the only statement of its batch (Msg 1067 otherwise)"
    );
}

#[gpui::test]
fn actual_plans_on_sql_server_pass_the_read_only_gate(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(cx, prod_doc(json!({"input": "q", "mode": "actual"})));
    let events = record(&view, cx);
    run_query(&view, cx);
    view.update(cx, |nb, cx| nb.authorize_and_run("plan", cx));
    let r = requests(&events);
    let sent = sent_batches(&r[0]);
    assert_eq!(sent.len(), 1);
    let batch = sent[0]
        .as_ref()
        .expect("the read-only gate lets the capture through");
    assert!(
        batch.contains("BEGIN TRANSACTION") && batch.contains("SET STATISTICS XML ON"),
        "{batch}"
    );
}

#[gpui::test]
fn plan_captures_on_other_engines_are_unchanged(cx: &mut TestAppContext) {
    for (db, expected) in [
        (
            DatabaseType::Postgres,
            "EXPLAIN (FORMAT JSON) SELECT * FROM dbo.Orders WHERE region = 'EMEA'",
        ),
        (
            DatabaseType::MySql,
            "EXPLAIN FORMAT=JSON SELECT * FROM dbo.Orders WHERE region = 'EMEA'",
        ),
        (
            DatabaseType::Sqlite,
            "EXPLAIN QUERY PLAN SELECT * FROM dbo.Orders WHERE region = 'EMEA'",
        ),
    ] {
        let _roots = Roots::new();
        let (view, cx) = open_on(cx, prod_doc(json!({"input": "q"})), db);
        let events = record(&view, cx);
        run_query(&view, cx);
        let r = requests(&events);
        let AnalysisWork::Plan { sql, .. } = &r[0].work else {
            panic!("{db:?}")
        };
        assert_eq!(sql, expected, "{db:?}");
        assert!(r[0].target.read_only);
    }
}

#[gpui::test]
fn a_plan_cell_explains_itself_with_ai(cx: &mut TestAppContext) {
    use sqlly_services::notebook::cells::ai::{AgentMode, AiCellSpec};
    let _roots = Roots::new();
    let (view, cx) = open(cx, prod_doc(json!({"input": "q"})));
    let events = record(&view, cx);
    run_query(&view, cx);
    let r = requests(&events);
    view.update(cx, |nb, cx| {
        nb.analysis_reply(r[0].request_id, plan_reply(), cx)
    });
    draw(&view, cx);
    let plan = view.read_with(cx, |nb, _| match nb.analysis.views.get("plan") {
        Some(CellViewHandle::Plan(p)) => p.clone(),
        _ => panic!("a plan view"),
    });
    plan.update(cx, |p, cx| p.explain_with_ai(cx));
    let (ids, spec, source) = view.read_with(cx, |nb, _| {
        let doc = nb.model();
        let at = doc.cells.iter().position(|c| c.id == "plan").unwrap();
        let ai = &doc.cells[at + 1];
        (
            doc.cells.iter().map(|c| c.kind.clone()).collect::<Vec<_>>(),
            AiCellSpec::of(ai),
            ai.source.clone(),
        )
    });
    assert_eq!(
        ids.last(),
        Some(&CellKind::Ai),
        "the AI cell lands below the plan"
    );
    assert_eq!(spec.mode, AgentMode::Assist);
    assert_eq!(spec.target.as_deref(), Some("plan"));
    assert_eq!(
        source,
        crate::localization::text("notebook.ai.explain.plan_prompt")
    );
    assert!(
        events
            .borrow()
            .iter()
            .any(|e| matches!(e, NotebookEvent::AiRunRequested { .. }))
    );
    draw(&view, cx);
}

#[gpui::test]
fn plans_never_wrap_more_than_one_statement(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let mut nb = prod_doc(json!({"input": "q"}));
    nb.cells[0].source = "SELECT 1\nGO\nSET SHOWPLAN_XML OFF\nGO\nSELECT 2".into();
    let (view, cx) = open(cx, nb);
    let events = record(&view, cx);
    run_query(&view, cx);
    assert!(requests(&events).is_empty(), "nothing is sent");
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.cell_state("plan"), CellStatus::Failed)
    });
}

#[gpui::test]
fn plan_compare_cells_pass_their_own_run_checks(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    for (sql, slot) in [
        ("SELECT 1\nGO\nSET SHOWPLAN_XML OFF\nGO\nSELECT 2", "sales"),
        ("SELECT 1", "dev"),
    ] {
        let (view, cx) = open(cx, compare_doc(sql, slot));
        let events = record(&view, cx);
        run_with_alt(&view, cx);
        assert!(
            requests(&events).is_empty(),
            "the compare cell {sql:?} on {slot} is refused"
        );
        view.read_with(cx, |nb, _| {
            assert_eq!(nb.cell_state("plan"), CellStatus::Failed)
        });
    }
    let (view, cx) = open(cx, compare_doc("SELECT 2", "sales"));
    let events = record(&view, cx);
    run_with_alt(&view, cx);
    let r = requests(&events);
    assert_eq!(r.len(), 1);
    let AnalysisWork::Plan { compare_sql, .. } = &r[0].work else {
        panic!()
    };
    assert!(compare_sql.as_deref().unwrap().contains("SELECT 2"));
    assert!(r[0].target.read_only);
}

#[gpui::test]
fn diff_connection_sides_run_only_on_resolved_slots(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    for (profile, expect) in [
        ("9e9e9e9e-0000-4000-8000-000000000000", None),
        (DEV_ID, Some(DEV_ID)),
    ] {
        let mut nb = NotebookDoc::empty("nb_diff_conn", "Diff");
        nb.connection_refs.push(ConnectionRef {
            id: "other".into(),
            profile_id: Some(profile.into()),
            label: "Other".into(),
            database: None,
            extra: Default::default(),
        });
        cell(&mut nb, "q", CellKind::Sql, "SELECT 1", json!(null));
        cell(
            &mut nb,
            "cmp",
            CellKind::Diff,
            "",
            json!({"inputs": ["q", {"cell": "q", "connection_ref": "other"}]}),
        );
        let (view, cx) = open(cx, nb);
        let events = record(&view, cx);
        let results = attach(&view, cx, &["q"]);
        view.update(cx, |nb, cx| nb.run_all(cx));
        let (cols, rows) = orders();
        feed(&results[0], cx, cols, rows, None);
        let r = requests(&events);
        match expect {
            None => {
                assert!(r.is_empty(), "a slot with no local profile runs nothing");
                view.read_with(cx, |nb, _| {
                    assert_eq!(nb.cell_state("cmp"), CellStatus::Failed)
                });
            }
            Some(id) => {
                assert_eq!(r.len(), 1);
                assert_eq!(
                    r[0].target.profile_id,
                    Some(uuid::Uuid::parse_str(id).unwrap())
                );
                assert!(r[0].target.read_only);
            }
        }
    }
}

#[gpui::test]
fn pasted_cells_cannot_smuggle_writes_or_connections(cx: &mut TestAppContext) {
    use sqlly_services::notebook::editing::{Fragment, fragment};
    let _roots = Roots::new();
    let mut nb = NotebookDoc::empty("nb_paste", "Paste");
    cell(&mut nb, "q", CellKind::Sql, "SELECT 1", json!(null));
    let (view, cx) = open(cx, nb);
    let mut write = Cell::new("w", CellKind::Sql, ExecutionPolicy::default());
    write.source = "DELETE FROM dbo.Orders".into();
    write.execution_policy.read_only = false;
    let mut foreign = Cell::new("f", CellKind::Sql, ExecutionPolicy::default());
    foreign.source = "SELECT * FROM dbo.Payroll".into();
    foreign.connection_ref = Some("theirs".into());
    let mut diff = Cell::new("d", CellKind::Diff, ExecutionPolicy::default());
    diff.spec = json!({"inputs": ["q", {"cell": "q", "connection_ref": "theirs"}]});
    let pasted = Fragment {
        format: fragment::FRAGMENT_FORMAT.to_string(),
        format_version: fragment::FRAGMENT_VERSION,
        source_notebook: None,
        connection_refs: vec![ConnectionRef {
            id: "theirs".into(),
            profile_id: Some("9e9e9e9e-0000-4000-8000-000000000000".into()),
            label: "Payroll".into(),
            database: None,
            extra: Default::default(),
        }],
        cells: vec![write, foreign, diff],
    };
    view.update(cx, |nb, cx| {
        nb.paste_fragment(pasted, 1, "notebook.editing.step.paste", cx)
    });
    let ids: Vec<String> = view.read_with(cx, |nb, _| {
        nb.doc.model().cells[1..4]
            .iter()
            .map(|c| c.id.clone())
            .collect()
    });
    view.read_with(cx, |nb, _| {
        let model = nb.doc.model();
        assert!(model.cells.iter().all(|c| c.execution_policy.read_only));
        assert!(
            nb.run_request(&ids[0]).is_err(),
            "a pasted write is read-only and refused"
        );
        assert!(
            nb.run_request(&ids[1]).is_err(),
            "a pasted slot with no local profile runs nothing"
        );
    });
    let events = record(&view, cx);
    let results = attach(&view, cx, &["q"]);
    view.update(cx, |nb, cx| nb.request_run_from_host("q", cx));
    let (cols, rows) = orders();
    feed(&results[0], cx, cols, rows, None);
    view.update(cx, |nb, cx| nb.request_run_from_host(&ids[2], cx));
    cx.run_until_parked();
    assert!(
        requests(&events).is_empty(),
        "the pasted diff side never reaches the other connection"
    );
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.cell_state(&ids[2]), CellStatus::Failed)
    });
}

#[gpui::test]
fn history_belongs_to_the_trust_record_and_the_principal(cx: &mut TestAppContext) {
    let roots = Roots::new();
    let stores = roots.stores();
    let mut nb = NotebookDoc::empty("nb_scope", "Scope");
    cell(
        &mut nb,
        "orders",
        CellKind::Sql,
        "SELECT * FROM dbo.Orders",
        json!(null),
    );
    let (view, cx) = open(cx, nb);
    let signed_in = |login: &str| ConnectionChoice {
        profile_id: DEV_ID.into(),
        label: "Dev".into(),
        environment: ConnectionEnvironment::Dev,
        database_type: DatabaseType::SqlServer,
        login_identity: login.into(),
        ..ConnectionChoice::default()
    };
    view.update(cx, |nb, cx| {
        nb.set_history_stores(stores.clone());
        nb.set_tab_connection(Some(signed_in("alice")), cx);
    });
    let results = attach(&view, cx, &["orders"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = orders();
    feed(&results[0], cx, cols, rows, None);
    cx.run_until_parked();
    let reload = |nb: &mut NotebookView| {
        nb.analysis.history.loaded = false;
        nb.analysis.history.runs.clear();
    };
    view.update(cx, |nb, _| {
        assert_eq!(nb.trust.scope.as_deref(), Some("nb_scope"));
        assert!(nb.previous_output("orders").is_some());
    });
    // A file that only borrows the id has another trust record.
    view.update(cx, |nb, _| {
        nb.trust.scope = Some("s_forged".into());
        reload(nb);
        assert!(nb.recorded_runs().is_empty());
        assert!(nb.previous_output("orders").is_none());
        nb.trust.scope = None;
        reload(nb);
        assert!(nb.recorded_runs().is_empty(), "untrusted: nothing is read");
    });
    // The same notebook signed in as someone else sees the run, not its rows.
    view.update(cx, |nb, cx| {
        nb.trust.scope = Some("nb_scope".into());
        reload(nb);
        nb.set_tab_connection(Some(signed_in("bob")), cx);
        assert_eq!(nb.recorded_runs().len(), 1);
        assert!(nb.previous_output("orders").is_none());
        nb.set_tab_connection(Some(signed_in("alice")), cx);
        assert!(nb.previous_output("orders").is_some());
    });
}

#[gpui::test]
fn untrusted_notebooks_open_no_local_data(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(cx, NotebookDoc::empty("nb_untrusted_ws", "Ws"));
    let opened = Rc::new(RefCell::new(None));
    let seen = opened.clone();
    view.update(cx, |nb, cx| {
        nb.trust.scope = None;
        nb.with_workspace(cx, move |_, ws, _| *seen.borrow_mut() = Some(ws.is_ok()));
    });
    assert_eq!(*opened.borrow(), Some(false));
}

#[gpui::test]
fn host_replies_and_refusals_stay_keys_into_the_run_record(cx: &mut TestAppContext) {
    use sqlly_core::DisplayMessage;
    let roots = Roots::new();
    let stores = roots.stores();
    let message_of = |view: &Entity<NotebookView>, cx: &mut VisualTestContext| {
        view.read_with(cx, |nb, _| {
            nb.cells.get("plan").and_then(|rt| rt.message.clone())
        })
    };

    // A refusal decided after the host's reply.
    let (view, cx) = open(
        cx,
        prod_doc(json!({"input": "q", "sweep": {"parameter": "region", "values": ["EMEA"]}})),
    );
    view.update(cx, |nb, _| nb.set_history_stores(stores.clone()));
    draw(&view, cx);
    let events = record(&view, cx);
    run_query(&view, cx);
    view.update(cx, |nb, cx| {
        nb.analysis_reply(requests(&events)[0].request_id, plan_reply(), cx)
    });
    let refused = message_of(&view, cx).expect("the sweep was refused");
    assert_eq!(
        refused.key(),
        Some("notebook.analysis.sweep.production_needs_authorization")
    );

    // The host's own failure keeps its key; a server's text stays raw.
    view.update(cx, |nb, cx| nb.request_run_from_host("plan", cx));
    let n = requests(&events).len();
    view.update(cx, |nb, cx| {
        nb.analysis_reply(
            requests(&events)[n - 1].request_id,
            AnalysisReply::Plan {
                plans: Err(crate::localization::deferred_text(
                    "notebook.analysis.plan.none_returned",
                )),
                compare: None,
                stats: None,
            },
            cx,
        )
    });
    assert_eq!(
        message_of(&view, cx).and_then(|m| m.key().map(str::to_string)),
        Some("notebook.analysis.plan.none_returned".into())
    );
    view.update(cx, |nb, cx| nb.request_run_from_host("plan", cx));
    let n = requests(&events).len();
    view.update(cx, |nb, cx| {
        nb.analysis_reply(
            requests(&events)[n - 1].request_id,
            AnalysisReply::Failed(DisplayMessage::raw("Msg 297: permission denied")),
            cx,
        )
    });
    let raw = message_of(&view, cx).unwrap();
    assert_eq!(raw.key(), None);
    assert_eq!(raw.fallback(), "Msg 297: permission denied");

    draw(&view, cx);
    cx.run_until_parked();
    let runs = stores.list("nb_plan", "nb_plan").unwrap();
    let keys: Vec<Option<String>> = runs
        .iter()
        .flat_map(|r| r.cells.iter())
        .filter(|c| c.cell_id == "plan")
        .filter_map(|c| c.error.as_ref())
        .map(|e| e.key.clone())
        .collect();
    assert!(
        keys.contains(&Some(
            "notebook.analysis.sweep.production_needs_authorization".into()
        )),
        "{keys:?}"
    );
    assert!(keys.contains(&Some("notebook.analysis.plan.none_returned".into())));
}

/// Two documents that agree on their first 8 KiB and differ after it.
fn long_documents() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let common = "x".repeat(9 * 1024);
    (
        vec![col("id", "int"), col("body", "nvarchar")],
        vec![
            vec![CellValue::I64(1), CellValue::String(format!("{common}-a"))],
            vec![CellValue::I64(2), CellValue::String(format!("{common}-b"))],
        ],
    )
}

/// Feed `rows` the way the grid receives them: long values cut to the
/// 8 KiB preview, with the cut reported beside them.
fn feed_cut(
    results: &Entity<ResultsView>,
    cx: &mut VisualTestContext,
    columns: Vec<ResultColumn>,
    mut rows: Vec<Vec<CellValue>>,
) {
    let cut = sqlly_services::truncate_rows(&mut rows, 0, &sqlly_services::CellLimits::default());
    assert!(!cut.is_empty());
    results.update(cx, |rv, cx| {
        rv.clear(cx);
        rv.handle_event(QueryEvent::Started, cx);
        rv.handle_event(QueryEvent::ResultSet { index: 0, columns }, cx);
        rv.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        rv.handle_event(
            QueryEvent::TruncatedCells {
                index: 0,
                cells: cut,
            },
            cx,
        );
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 3 }, cx);
    });
    cx.run_until_parked();
}

/// A query cell on Sales with long documents, and a test that the
/// documents are unique.
fn documents_doc(sql: &str) -> NotebookDoc {
    let mut nb = prod_doc(json!(null));
    nb.cells.truncate(1);
    nb.cells[0].source = sql.into();
    cell(
        &mut nb,
        "checks",
        CellKind::Assertion,
        "",
        json!({"input": "q", "assertions": [
            {"kind": "unique", "columns": ["body"], "name": "unique body"}
        ]}),
    );
    nb
}

fn output_body_len(view: &Entity<NotebookView>, cx: &mut VisualTestContext) -> usize {
    view.read_with(cx, |nb, _| {
        let output = nb.outputs.get("q").expect("q published its output");
        match &output.rows[0][1] {
            CellValue::String(s) => s.len(),
            other => panic!("{other:?}"),
        }
    })
}

#[gpui::test]
fn cells_reading_a_result_with_cut_values_see_them_whole(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(cx, documents_doc("SELECT id, body FROM dbo.Docs"));
    draw(&view, cx);
    let events = record(&view, cx);
    let results = attach(&view, cx, &["q"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = long_documents();
    feed_cut(&results[0], cx, cols.clone(), rows.clone());

    let r = requests(&events);
    assert_eq!(r.len(), 1, "one read of the whole values");
    let AnalysisWork::Rows { queries, max_rows } = &r[0].work else {
        panic!("a rows request")
    };
    assert_eq!(queries, &["SELECT id, body FROM dbo.Docs".to_string()]);
    assert_eq!(
        *max_rows,
        crate::notebook::outputs::DEFAULT_SNAPSHOT_ROW_CAP
    );
    assert!(r[0].target.read_only);
    assert_eq!(r[0].cell_id, "q");
    view.read_with(cx, |nb, _| {
        assert_ne!(
            nb.cell_state("q"),
            CellStatus::Fresh,
            "q finishes once its values are whole"
        );
        assert!(nb.analysis.tests.is_empty(), "the test waits for q");
    });

    view.update(cx, |nb, cx| {
        nb.analysis_reply(
            r[0].request_id,
            AnalysisReply::Rows(vec![Ok(QueryRows {
                columns: cols,
                rows,
                ..QueryRows::default()
            })]),
            cx,
        )
    });
    cx.run_until_parked();
    draw(&view, cx);
    assert_eq!(output_body_len(&view, cx), 9 * 1024 + 2);
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.cell_state("q"), CellStatus::Fresh);
        assert_eq!(nb.cell_state("checks"), CellStatus::Fresh);
        let summary = nb.test_summary();
        assert_eq!(
            (summary.passed, summary.failed),
            (1, 0),
            "the documents differ after their first 8 KiB"
        );
    });
}

#[gpui::test]
fn different_rows_on_the_second_read_keep_the_previews(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let (view, cx) = open(cx, documents_doc("SELECT TOP 2 id, body FROM dbo.Docs"));
    draw(&view, cx);
    let events = record(&view, cx);
    let results = attach(&view, cx, &["q"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = long_documents();
    feed_cut(&results[0], cx, cols.clone(), rows.clone());
    let r = requests(&events);
    assert_eq!(r.len(), 1);
    let mut other = rows;
    other[0][0] = CellValue::I64(7);
    view.update(cx, |nb, cx| {
        nb.analysis_reply(
            r[0].request_id,
            AnalysisReply::Rows(vec![Ok(QueryRows {
                columns: cols,
                rows: other,
                ..QueryRows::default()
            })]),
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(output_body_len(&view, cx), 8 * 1024);
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.cell_state("q"), CellStatus::Fresh);
        let kept = crate::localization::text("notebook.fix_plan.kept_previews");
        assert!(
            nb.activity
                .iter()
                .any(|a| a.cell.as_deref() == Some("q") && a.message == kept),
            "the activity log says the values stayed cut"
        );
    });
}

#[gpui::test]
fn a_query_that_is_not_read_only_is_never_run_again_for_its_values(cx: &mut TestAppContext) {
    let _roots = Roots::new();
    let mut doc = documents_doc("EXEC dbo.LoadDocs");
    doc.connection_refs[0].profile_id = Some(DEV_ID.into());
    let (view, cx) = open(cx, doc);
    draw(&view, cx);
    let events = record(&view, cx);
    let results = attach(&view, cx, &["q"]);
    view.update(cx, |nb, cx| nb.run_all(cx));
    let (cols, rows) = long_documents();
    feed_cut(&results[0], cx, cols, rows);
    assert!(requests(&events).is_empty(), "nothing is run again");
    assert_eq!(output_body_len(&view, cx), 8 * 1024);
    view.read_with(cx, |nb, _| {
        assert_eq!(
            nb.cell_state("q"),
            CellStatus::Fresh,
            "{:?}",
            nb.cells.get("q").and_then(|rt| rt.message.clone())
        );
        let kept = crate::localization::text("notebook.fix_plan.kept_previews");
        assert!(nb.activity.iter().any(|a| a.message == kept));
    });
}
