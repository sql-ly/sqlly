//! Windowed tests for the run queue against a fake host: re-requests during
//! a run, cells that never start, change reviews holding the cells started
//! with them, write barriers across merged requests, and runs that wait on
//! a review that will not be answered.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use sqlly_core::ConnectionEnvironment;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::QueryEvent;
use sqlly_services::notebook::{Cell, CellKind, ConnectionRef, ExecutionPolicy, NotebookDoc};

use super::connection::ConnectionChoice;
use super::doc_adapter::RunScope;
use super::view::{CellStatus, DispatchOutcome, NotebookEvent, NotebookOpening, NotebookView};
use crate::results::ResultsView;

const CRM_ID: &str = "0c3d2b1a-5f4e-4d3c-8b2a-1f0e9d8c7b6a";
const SALES_ID: &str = "6b1f3c9e-6a55-4b7e-9b0b-3f8f2d1c0a11";

struct Root(#[allow(dead_code)] tempfile::TempDir);

impl Root {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        super::autosave::set_test_root(Some(dir.path().to_path_buf()));
        Self(dir)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        super::autosave::set_test_root(None);
    }
}

fn choices() -> Vec<ConnectionChoice> {
    [
        (CRM_ID, "CRM", ConnectionEnvironment::Dev),
        (SALES_ID, "Sales", ConnectionEnvironment::Production),
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
}

/// Cells as (id, slot, source); slots are `crm` and `sales`, `""` is the
/// notebook's connection. Cells whose SQL writes are allowed to.
fn notebook(cells: &[(&str, &str, &str)]) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nb_runs", "Runs");
    for (slot, profile, label) in [("crm", CRM_ID, "CRM"), ("sales", SALES_ID, "Sales")] {
        nb.connection_refs.push(ConnectionRef {
            id: slot.into(),
            profile_id: Some(profile.into()),
            label: label.into(),
            database: None,
            extra: Default::default(),
        });
    }
    for (id, slot, source) in cells {
        let mut cell = Cell::new(*id, CellKind::Sql, ExecutionPolicy::default());
        cell.name = Some(id.to_string());
        cell.source = source.to_string();
        if !slot.is_empty() {
            cell.connection_ref = Some(slot.to_string());
        }
        if sqlly_services::notebook::migrate::writes_data(Some(DatabaseType::SqlServer), source) {
            cell.execution_policy.read_only = false;
        }
        nb.cells.push(cell);
    }
    nb
}

fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    open_as(cx, model, NotebookOpening::New)
}

fn open_as(
    cx: &mut TestAppContext,
    model: NotebookDoc,
    opening: NotebookOpening,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| NotebookView::with_choices(model, opening, Rc::new(choices), cx));
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

/// The cells handed to the host, in order.
fn record(view: &Entity<NotebookView>, cx: &mut VisualTestContext) -> Rc<RefCell<Vec<String>>> {
    let ran = Rc::new(RefCell::new(Vec::new()));
    let seen = ran.clone();
    cx.update(|_, cx| {
        cx.subscribe(view, move |_, event: &NotebookEvent, _| {
            if let NotebookEvent::RunCell { cell_id } = event {
                seen.borrow_mut().push(cell_id.clone());
            }
        })
        .detach();
    });
    ran
}

fn attach(
    view: &Entity<NotebookView>,
    ids: &[&str],
    cx: &mut VisualTestContext,
) -> Vec<Entity<ResultsView>> {
    ids.iter()
        .map(|id| {
            let results = cx.update(|_, cx| cx.new(ResultsView::new));
            view.update(cx, |nb, cx| nb.attach_results(id, results.clone(), cx));
            results
        })
        .collect()
}

fn complete(results: &Entity<ResultsView>, cx: &mut VisualTestContext) {
    results.update(cx, |rv, cx| {
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx)
    });
}

fn state(view: &Entity<NotebookView>, id: &str, cx: &mut VisualTestContext) -> CellStatus {
    view.read_with(cx, |nb, _| nb.cell_state(id))
}

fn run_cell(view: &Entity<NotebookView>, id: &str, cx: &mut VisualTestContext) {
    view.update(cx, |nb, cx| nb.request_run_from_host(id, cx));
}

#[gpui::test]
fn a_cell_asked_for_again_during_a_run_runs_again(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[("a", "crm", "SELECT 1"), ("b", "sales", "SELECT 2")]),
    );
    let ran = record(&view, cx);
    let results = attach(&view, &["a", "b"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    assert_eq!(*ran.borrow(), ["a", "b"]);
    complete(&results[0], cx);
    assert_eq!(state(&view, "a", cx), CellStatus::Fresh);

    run_cell(&view, "a", cx);
    assert_eq!(
        *ran.borrow(),
        ["a", "b", "a"],
        "a finished, so it runs again"
    );

    run_cell(&view, "b", cx);
    assert_eq!(*ran.borrow(), ["a", "b", "a"], "b is still running");
    assert!(view.read_with(cx, |nb, _| nb.queued().contains(&"b".to_string())));
    complete(&results[1], cx);
    assert_eq!(
        *ran.borrow(),
        ["a", "b", "a", "b"],
        "b runs once more after the run in flight"
    );
    complete(&results[0], cx);
    complete(&results[1], cx);
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));
    assert_eq!(state(&view, "b", cx), CellStatus::Fresh);
}

#[gpui::test]
fn a_cell_that_never_starts_fails_alone(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[
            ("a", "crm", "SELECT 1 AS x"),
            ("b", "sales", "SELECT 2"),
            ("c", "crm", "SELECT x FROM {{a}}"),
        ]),
    );
    let ran = record(&view, cx);
    let results = attach(&view, &["a", "b", "c"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    assert_eq!(*ran.borrow(), ["a", "b"]);
    view.update(cx, |nb, cx| {
        nb.dispatch_result("a", DispatchOutcome::NotStarted("No connection".into()), cx)
    });
    assert_eq!(state(&view, "a", cx), CellStatus::Failed);
    assert_eq!(
        state(&view, "c", cx),
        CellStatus::Blocked,
        "a's dependent is blocked"
    );
    assert_eq!(
        state(&view, "b", cx),
        CellStatus::Queued,
        "b is not canceled"
    );
    assert!(view.read_with(cx, |nb, _| nb.is_running() && nb.is_dispatching("b")));

    // Stop still reaches the cell that is running.
    let stopped = Rc::new(RefCell::new(false));
    let seen = stopped.clone();
    cx.update(|_, cx| {
        cx.subscribe(&view, move |_, event: &NotebookEvent, _| {
            if matches!(event, NotebookEvent::StopRequested) {
                *seen.borrow_mut() = true;
            }
        })
        .detach();
    });
    view.update(cx, |nb, cx| {
        nb.dispatch_result("b", DispatchOutcome::Started, cx);
        nb.stop(cx)
    });
    assert!(*stopped.borrow());
    assert_eq!(state(&view, "b", cx), CellStatus::Canceled);
    assert_eq!(*ran.borrow(), ["a", "b"]);
    drop(results);
}

#[gpui::test]
fn a_write_holds_the_cells_started_with_it_until_its_review_is_answered(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[
            ("w", "crm", "UPDATE t SET a = 1 WHERE id = 1"),
            ("r", "sales", "SELECT 1 AS x"),
        ]),
    );
    let ran = record(&view, cx);
    let results = attach(&view, &["w", "r"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    assert_eq!(
        *ran.borrow(),
        ["w"],
        "r waits: the review may block the tab"
    );
    view.update(cx, |nb, cx| {
        nb.dispatch_result("w", DispatchOutcome::AwaitingApproval, cx)
    });
    assert_eq!(*ran.borrow(), ["w"]);
    assert_eq!(state(&view, "r", cx), CellStatus::Queued);

    // Confirming starts the write; the held read goes ahead beside it.
    view.update(cx, |nb, cx| {
        nb.dispatch_result("w", DispatchOutcome::Started, cx)
    });
    assert_eq!(state(&view, "w", cx), CellStatus::Running);
    assert_eq!(*ran.borrow(), ["w", "r"]);
    complete(&results[0], cx);
    complete(&results[1], cx);
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));
}

#[gpui::test]
fn a_write_asked_for_during_a_read_waits_for_it(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[
            ("r", "crm", "SELECT 1"),
            ("w", "crm", "UPDATE t SET a = 1 WHERE id = 1"),
            ("r2", "crm", "SELECT 2"),
        ]),
    );
    let ran = record(&view, cx);
    let results = attach(&view, &["r", "w", "r2"], cx);
    run_cell(&view, "r", cx);
    run_cell(&view, "w", cx);
    assert_eq!(
        *ran.borrow(),
        ["r"],
        "never beside the read on its connection"
    );
    run_cell(&view, "r2", cx);
    assert_eq!(
        *ran.borrow(),
        ["r"],
        "a read asked for after the write waits for it"
    );
    complete(&results[0], cx);
    assert_eq!(*ran.borrow(), ["r", "w"]);
    view.update(cx, |nb, cx| {
        nb.dispatch_result("w", DispatchOutcome::Started, cx)
    });
    complete(&results[1], cx);
    assert_eq!(*ran.borrow(), ["r", "w", "r2"]);
}

#[gpui::test]
fn a_new_request_settles_a_review_that_went_away_without_wedging_its_dependents(
    cx: &mut TestAppContext,
) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[
            ("w", "crm", "UPDATE t SET a = 1 WHERE id = 1"),
            ("d", "crm", "SELECT * FROM {{w}}"),
            ("x", "sales", "SELECT 3"),
        ]),
    );
    let ran = record(&view, cx);
    let results = attach(&view, &["w", "d", "x"], cx);
    view.update(cx, |nb, cx| {
        nb.run_scope(RunScope::Downstream, Some("w".into()), cx);
        nb.dispatch_result("w", DispatchOutcome::AwaitingApproval, cx);
    });
    assert_eq!(state(&view, "d", cx), CellStatus::Queued);

    // The review disappeared without an answer; another cell is asked for.
    run_cell(&view, "x", cx);
    assert_eq!(*ran.borrow(), ["w", "x"], "the new request goes ahead");
    assert_eq!(state(&view, "w", cx), CellStatus::Canceled);
    assert_eq!(
        state(&view, "d", cx),
        CellStatus::Blocked,
        "w's dependent is blocked, not left queued"
    );
    complete(&results[2], cx);
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));

    // A confirmed write is running, not stuck: asking for another cell
    // keeps the run, and the write's dependent still runs after it.
    view.update(cx, |nb, cx| {
        nb.run_scope(RunScope::Downstream, Some("w".into()), cx);
        nb.dispatch_result("w", DispatchOutcome::AwaitingApproval, cx);
        nb.dispatch_result("w", DispatchOutcome::Started, cx);
    });
    run_cell(&view, "x", cx);
    assert_eq!(*ran.borrow(), ["w", "x", "w", "x"]);
    complete(&results[0], cx);
    assert_eq!(*ran.borrow(), ["w", "x", "w", "x", "d"]);
}

#[gpui::test]
fn a_queued_cell_cannot_be_converted_to_markdown(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[("a", "crm", "SELECT 1"), ("b", "crm", "SELECT 2")]),
    );
    let ran = record(&view, cx);
    let results = attach(&view, &["a", "b"], cx);
    view.update(cx, |nb, cx| {
        nb.set_per_connection(1, cx);
        nb.run_all(cx)
    });
    assert_eq!(*ran.borrow(), ["a"]);
    view.update(cx, |nb, cx| nb.convert_cell("b", "markdown", cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb
            .doc()
            .cell_by_id("b")
            .map(|c| c.kind.to_string())),
        Some("sql".into())
    );
    complete(&results[0], cx);
    assert_eq!(*ran.borrow(), ["a", "b"]);
    complete(&results[1], cx);
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));
}

#[gpui::test]
fn cells_leaving_the_document_during_a_run_never_wedge_it(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[
            ("a", "crm", "SELECT 1 AS x"),
            ("z", "crm", "SELECT 9"),
            ("b", "crm", "SELECT x FROM {{a}}"),
        ]),
    );
    let ran = record(&view, cx);
    let results = attach(&view, &["a", "z", "b"], cx);
    view.update(cx, |nb, cx| {
        nb.run_scope(RunScope::Downstream, Some("a".into()), cx)
    });
    assert_eq!(*ran.borrow(), ["a"]);

    // Merging an idle cell into the running one above it is refused.
    view.update(cx, |nb, cx| {
        nb.activate_cell("z", cx);
        nb.editing_command(super::editing::EditingCommand::MergeAbove, cx);
    });
    assert!(view.read_with(cx, |nb, _| nb.doc().cell_by_id("z").is_some()));

    // A change that takes the running cell out ends its step; its
    // dependent is blocked and the run ends.
    let removed = view.update(cx, |nb, cx| {
        nb.change_doc(
            "notebook.editing.step.cut",
            |doc| {
                let at = doc.cell_index("a").unwrap();
                doc.cells.remove(at)
            },
            cx,
        )
    });
    assert_eq!(state(&view, "b", cx), CellStatus::Blocked);
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));
    assert_eq!(*ran.borrow(), ["a"]);

    // Bringing it back shows it canceled, not running forever.
    view.update(cx, |nb, cx| {
        nb.change_doc(
            "notebook.editing.step.paste",
            move |doc| doc.cells.insert(0, removed),
            cx,
        )
    });
    assert_eq!(state(&view, "a", cx), CellStatus::Canceled);
    drop(results);
}

#[gpui::test]
fn stopping_an_analysis_cell_reaches_its_host_queries(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(cx, notebook(&[("a", "crm", "SELECT 1")]));
    let cancel = super::analysis::AnalysisCancel::default();
    assert!(cancel.record("plan-1"));
    view.update(cx, |nb, _| {
        nb.track_analysis("a", cancel.clone());
        nb.cancel_analysis(Some("a"));
    });
    assert!(cancel.is_canceled());
    assert!(!cancel.record("plan-2"), "nothing new starts after Stop");
    assert_eq!(
        view.update(cx, |nb, _| nb.take_analysis_queries()),
        ["plan-1"],
        "the running query goes to the host to cancel"
    );
}

fn big(n: usize) -> NotebookDoc {
    let cells: Vec<(String, String)> = (0..n)
        .map(|i| {
            let source = if i % 5 == 4 && i > 0 {
                format!("SELECT * FROM {{{{c{}}}}}", i - 1)
            } else {
                format!("SELECT o.OrderID FROM dbo.Orders AS o WHERE o.OrderID > {i}")
            };
            (format!("c{i}"), source)
        })
        .collect();
    let refs: Vec<(&str, &str, &str)> = cells
        .iter()
        .map(|(id, source)| (id.as_str(), "", source.as_str()))
        .collect();
    notebook(&refs)
}

fn draw(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(
        gpui::Point::default(),
        gpui::size(gpui::px(1400.0), gpui::px(900.0)),
        move |_, _| {
            use gpui::{ParentElement as _, Styled as _};
            gpui::div().size_full().child(view.clone())
        },
    );
}

fn passes() -> usize {
    super::doc_adapter::FINGERPRINT_PASSES.with(|n| n.get())
        + super::doc_adapter::GRAPH_BUILDS.with(|n| n.get())
}

/// Type into the middle cell of `n` cells with the inspector showing it.
/// Returns (fingerprint and graph rebuilds while typing, ms per keystroke
/// frame).
fn type_with_inspector(n: usize, cx: &mut TestAppContext) -> (usize, f64) {
    type_into(n, true, cx)
}

fn type_into(n: usize, inspector: bool, cx: &mut TestAppContext) -> (usize, f64) {
    let (view, cx) = open(cx, big(n));
    let id = format!("c{}", n / 2);
    view.update(cx, |nb, cx| {
        nb.activate_cell(&id, cx);
        nb.update_layout(|l| l.right_visible = inspector, cx);
    });
    draw(&view, cx);
    let before = passes();
    let started = std::time::Instant::now();
    for i in 0..20 {
        view.update(cx, |nb, cx| {
            nb.doc_mut_for_test()
                .set_source(&id, &format!("SELECT {i} FROM dbo.Orders"));
            nb.mark_dirty_for_test(cx);
        });
        draw(&view, cx);
    }
    let per_key = started.elapsed().as_secs_f64() * 1000.0 / 20.0;
    (passes() - before, per_key)
}

#[gpui::test]
fn typing_with_the_inspector_open_never_refingerprints_the_notebook(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (rebuilds, _) = type_with_inspector(60, cx);
    assert_eq!(
        rebuilds, 0,
        "fingerprints and the graph wait for the edit debounce"
    );
}

#[gpui::test]
fn cell_headers_read_a_cells_sql_again_only_when_it_changed(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(cx, big(60));
    view.update(cx, |nb, cx| {
        assert!(!nb.source_writes("c1", "SELECT 1"));
        assert!(nb.source_writes("c1", "DELETE FROM dbo.Orders"));
        assert!(
            !nb.source_writes("c1", "SELECT 2"),
            "a new source is read again"
        );
        nb.writes_seen.clear();
        cx.notify();
    });
    draw(&view, cx);
    let seen = view.read_with(cx, |nb, _| nb.writes_seen.len());
    assert!(
        seen > 0 && seen < 60,
        "only the cells in view were read: {seen}"
    );
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_typing_with_the_inspector_open_500_cells(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (rebuilds, per_key) = type_with_inspector(500, cx);
    println!(
        "500 cells, inspector open: {per_key:.1} ms per keystroke frame, {rebuilds} fingerprint and graph rebuilds over 20 keystrokes"
    );
    let (_, hidden) = type_into(500, false, cx);
    println!("500 cells, inspector hidden: {hidden:.1} ms per keystroke frame");
}

#[gpui::test]
fn run_records_keep_cell_errors_as_keys_not_translated_text(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(
        cx,
        notebook(&[
            ("a", "crm", "SELECT 1"),
            ("b", "sales", "SELECT 2"),
            ("c", "sales", "SELECT 3"),
        ]),
    );
    let results = attach(&view, &["a", "b", "c"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    view.update(cx, |nb, _| {
        nb.cells.get_mut("b").unwrap().message = Some(crate::localization::deferred_text(
            "notebook.local.run.waiting_for_you",
        ));
    });
    for r in &results[..2] {
        r.update(cx, |rv, cx| {
            rv.handle_event(
                QueryEvent::Error {
                    message: "boom".into(),
                },
                cx,
            )
        });
    }
    let keys: Vec<Option<String>> = view.read_with(cx, |nb, _| {
        nb.recorded_errors()
            .into_iter()
            .map(|e| e.and_then(|e| e.key))
            .collect()
    });
    assert_eq!(
        keys,
        [
            Some("notebook.shell.activity.failed".to_string()),
            Some("notebook.local.run.waiting_for_you".to_string())
        ]
    );
}

/// Hands back one cached output for any key.
struct OneCached(sqlly_services::notebook::CellOutput);

impl super::local::cache::CacheBackend for OneCached {
    fn store(
        &self,
        _: &super::local::cache::CacheKey,
        _: &sqlly_services::notebook::CellOutput,
        _: super::local::cache::StoreOptions,
    ) -> Result<(), String> {
        Ok(())
    }

    fn load(&self, _: &super::local::cache::CacheKey) -> Option<super::local::cache::CachedOutput> {
        Some(super::local::cache::CachedOutput {
            output: self.0.clone(),
            stored_at_ms: 0,
        })
    }

    fn clear_profile(&self, _: &str) -> Result<usize, String> {
        Ok(0)
    }

    fn clear_all(&self) -> Result<usize, String> {
        Ok(0)
    }
}

#[gpui::test]
fn a_restored_cached_copy_keeps_its_completeness_and_feeds_dependents(cx: &mut TestAppContext) {
    use sqlly_core::{CellValue, ResultColumn};
    use sqlly_services::notebook::{Completeness, Fingerprint};
    let _root = Root::new();
    let mut model = notebook(&[("a", "crm", "SELECT x FROM big")]);
    sqlly_services::notebook::cells::local::set_cache_settings(
        model.cell_mut("a").unwrap(),
        sqlly_services::notebook::cells::local::CacheSettings {
            storage: sqlly_services::notebook::cells::local::CacheStorage::EncryptedDisk,
            clear_on_disconnect: false,
        },
    );
    let (view, cx) = open(cx, model);
    let output = sqlly_services::notebook::CellOutput::capped(
        vec![ResultColumn {
            name: "x".into(),
            data_type: "int".into(),
            nullable: false,
        }],
        (0..5).map(|i| vec![CellValue::I64(i)]).collect(),
        Some(500),
        5,
        1,
        "r_cached",
        Fingerprint("f".into()),
        Default::default(),
    );
    view.update(cx, |nb, _| {
        nb.encrypted_cache = Some(Rc::new(OneCached(output)))
    });
    attach(&view, &["a"], cx);
    view.read_with(cx, |nb, _| {
        let state = nb.doc().cell_state("a");
        assert_eq!(
            state.output.unwrap().completeness,
            Completeness::Truncated { cap: 5 },
            "a capped copy is never presented as complete"
        );
        let published = nb
            .outputs
            .get("a")
            .expect("the restored rows are published");
        assert_eq!(published.rows.len(), 5);
        assert_eq!(
            published.freshness,
            sqlly_services::notebook::Freshness::TtlCached
        );
    });
}

#[gpui::test]
fn an_analysis_input_read_from_a_running_cell_keeps_only_the_capped_prefix(
    cx: &mut TestAppContext,
) {
    use sqlly_core::{CellValue, ResultColumn};
    use sqlly_services::notebook::Completeness;
    use sqlly_services::notebook::output::DEFAULT_OUTPUT_ROW_CAP;
    let _root = Root::new();
    let (view, cx) = open(cx, notebook(&[("a", "crm", "SELECT x FROM big")]));
    let results = attach(&view, &["a"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    results[0].update(cx, |r, cx| {
        r.start_query(cx);
        r.handle_event(QueryEvent::Started, cx);
        r.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "x".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        r.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: (0..DEFAULT_OUTPUT_ROW_CAP as i64 + 500)
                    .map(|i| vec![CellValue::I64(i)])
                    .collect(),
            },
            cx,
        );
    });
    view.update(cx, |nb, cx| {
        nb.dispatch_result("a", DispatchOutcome::Started, cx)
    });
    let data = cx
        .update(|_, cx| super::analysis::inputs::cell_output(view.read(cx), "a", cx))
        .expect("rows so far");
    assert_eq!(data.output.rows.len(), DEFAULT_OUTPUT_ROW_CAP);
    assert_eq!(
        data.output.rows[0],
        vec![CellValue::I64(0)],
        "the start of the result"
    );
    assert!(
        !data.output.completeness.supports_summaries(),
        "a prefix is never summarized as the whole result: {:?}",
        data.output.completeness
    );
    assert!(!matches!(
        data.output.completeness,
        Completeness::Spilled { .. }
    ));
}

#[gpui::test]
fn a_query_that_ignores_stop_never_lands_as_the_result(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(cx, notebook(&[("a", "crm", "SELECT 1")]));
    let ran = record(&view, cx);
    let results = attach(&view, &["a"], cx);
    view.update(cx, |nb, cx| {
        nb.run_all(cx);
        nb.dispatch_result("a", DispatchOutcome::Started, cx);
        nb.stop(cx);
    });
    assert_eq!(state(&view, "a", cx), CellStatus::Canceled);

    complete(&results[0], cx);
    assert_eq!(
        state(&view, "a", cx),
        CellStatus::Canceled,
        "the late completion is dropped"
    );
    view.update(cx, |nb, _| {
        assert!(nb.outputs.get("a").is_none(), "nothing published");
        let landed = nb
            .recorded_runs()
            .iter()
            .flat_map(|r| &r.cells)
            .any(|c| c.cell_id == "a" && c.status == CellStatus::Fresh);
        assert!(!landed, "history never records the late result");
    });

    run_cell(&view, "a", cx);
    assert_eq!(*ran.borrow(), ["a", "a"]);
    complete(&results[0], cx);
    assert_eq!(
        state(&view, "a", cx),
        CellStatus::Fresh,
        "the next run lands as usual"
    );
}

fn cached_model() -> NotebookDoc {
    let mut model = notebook(&[("a", "crm", "SELECT x FROM big")]);
    sqlly_services::notebook::cells::local::set_cache_settings(
        model.cell_mut("a").unwrap(),
        sqlly_services::notebook::cells::local::CacheSettings {
            storage: sqlly_services::notebook::cells::local::CacheStorage::EncryptedDisk,
            clear_on_disconnect: false,
        },
    );
    model
}

fn cached_output() -> sqlly_services::notebook::CellOutput {
    sqlly_services::notebook::CellOutput::capped(
        vec![sqlly_core::ResultColumn {
            name: "x".into(),
            data_type: "int".into(),
            nullable: false,
        }],
        (0..3)
            .map(|i| vec![sqlly_core::CellValue::I64(i)])
            .collect(),
        None,
        10,
        1,
        "r_cached",
        sqlly_services::notebook::Fingerprint("f".into()),
        Default::default(),
    )
}

#[gpui::test]
fn cached_results_come_back_when_a_trusted_notebook_opens(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open(cx, cached_model());
    view.update(cx, |nb, cx| {
        nb.encrypted_cache = Some(Rc::new(OneCached(cached_output())));
        nb.restore_cached_outputs(cx);
    });
    assert_eq!(
        state(&view, "a", cx),
        CellStatus::Cached,
        "restored with no results view open"
    );
    assert!(view.read_with(cx, |nb, _| nb.outputs.get("a").is_some()));
    let results = attach(&view, &["a"], cx);
    let shown = results[0].read_with(cx, |r, _| r.result_set_at(0).map(|rs| rs.rows.len()));
    assert_eq!(
        shown,
        Some(3),
        "the grid shows the restored rows once it opens"
    );
}

#[gpui::test]
fn an_untrusted_notebook_never_restores_cached_results(cx: &mut TestAppContext) {
    let _root = Root::new();
    let (view, cx) = open_as(cx, cached_model(), NotebookOpening::File);
    view.update(cx, |nb, cx| {
        assert_eq!(
            nb.live_trust().level,
            sqlly_services::notebook_security::TrustLevel::Untrusted
        );
        nb.encrypted_cache = Some(Rc::new(OneCached(cached_output())));
        nb.restore_cached_outputs(cx);
    });
    attach(&view, &["a"], cx);
    view.read_with(cx, |nb, _| {
        assert!(nb.outputs.get("a").is_none(), "nothing published");
        assert!(nb.doc().cell_state("a").output.is_none());
    });
    assert_ne!(state(&view, "a", cx), CellStatus::Cached);
}

#[gpui::test]
fn visual_and_analysis_errors_are_kept_as_keys(cx: &mut TestAppContext) {
    let _root = Root::new();
    let mut model = notebook(&[("a", "crm", "SELECT 1 AS x")]);
    let mut chart = Cell::new("chart", CellKind::Chart, ExecutionPolicy::default());
    chart.spec = serde_json::json!({"input": "a", "chart": {"kind": "bar"}});
    model.cells.push(chart);
    let mut profile = Cell::new("prof", CellKind::Profile, ExecutionPolicy::default());
    profile.spec = serde_json::json!({"input": "nowhere"});
    model.cells.push(profile);
    let (view, cx) = open(cx, model);
    view.update(cx, |nb, cx| {
        nb.run_local_cell("chart", cx);
        nb.start_analysis_cell("prof", cx);
    });
    let key = |id: &str, cx: &mut VisualTestContext| {
        view.read_with(cx, |nb, _| {
            nb.cells
                .get(id)
                .and_then(|rt| rt.message.as_ref())
                .and_then(|m| m.key())
        })
    };
    assert_eq!(
        key("chart", cx),
        Some("notebook.visual.notice.input_not_run"),
        "stored as a key and its arguments, translated only when drawn"
    );
    assert_eq!(key("prof", cx), Some("notebook.analysis.input_missing"));
}

#[gpui::test]
fn formula_transform_and_refusal_errors_are_kept_as_keys(cx: &mut TestAppContext) {
    let _root = Root::new();
    let mut model = notebook(&[
        ("a", "crm", "SELECT 1 AS x"),
        ("b", "sales", "SELECT 2 AS y"),
    ]);
    let mut formula = Cell::new("f", CellKind::Scalar, ExecutionPolicy::default());
    formula.source = "1 + ".into();
    model.cells.push(formula);
    let mut unknown = Cell::new("g", CellKind::Scalar, ExecutionPolicy::default());
    unknown.source = "nowhere * 2".into();
    model.cells.push(unknown);
    let mut transform = Cell::new("t", CellKind::Transform, ExecutionPolicy::default());
    transform.spec = serde_json::json!({"input": "a", "steps": []});
    model.cells.push(transform);
    let (view, cx) = open(cx, model);
    view.update(cx, |nb, cx| {
        nb.run_local_cell("f", cx);
        nb.run_local_cell("g", cx);
        nb.run_local_cell("t", cx);
    });
    let message = |id: &str, cx: &mut VisualTestContext| {
        view.read_with(cx, |nb, _| {
            nb.cells.get(id).and_then(|rt| rt.message.clone())
        })
    };
    let parse = message("f", cx).expect("the formula failed");
    assert_eq!(parse.key(), Some("notebook.visual.scalar.errors.parse"));
    assert!(parse.arguments().any(|(name, _)| name == "position"));
    assert_eq!(
        message("g", cx).and_then(|m| m.key().map(str::to_string)),
        Some("notebook.visual.scalar.errors.unknown_reference".into())
    );
    assert_eq!(
        message("t", cx).and_then(|m| m.key().map(str::to_string)),
        Some("notebook.visual.notice.input_not_run".into())
    );

    // Refusals are kept as keys too: they become the cell's message.
    use sqlly_services::notebook_security::{Decision, Reason};
    view.read_with(cx, |nb, _| {
        let refused = nb
            .run_refusal(&Decision::Deny(Reason::ConnectionNotAuthorized {
                slot: "sales".into(),
            }))
            .unwrap();
        assert_eq!(
            refused.key(),
            Some("notebook.shell.trust.connection_not_allowed")
        );
        assert!(
            refused
                .arguments()
                .any(|(name, value)| name == "connection_name" && value == "Sales")
        );
        let untrusted = nb
            .run_refusal(&Decision::NeedsApproval(Reason::TrustRequired))
            .unwrap();
        assert_eq!(untrusted.key(), Some("notebook.shell.trust.run_blocked"));
        assert!(
            nb.run_refusal(&Decision::Allow {
                notices: Vec::new()
            })
            .is_none()
        );
    });
}
