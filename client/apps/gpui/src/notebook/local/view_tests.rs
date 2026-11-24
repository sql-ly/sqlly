//! Windowed tests for local data and concurrent runs: independent cells on
//! different connections run at once, dependents wait, a failure blocks
//! only its dependents, session groups run one at a time, a change review
//! holds new dispatches, row caps and Fetch All, local cells' SQL, the
//! per-connection opt-in, and a cross-connection join copied into the
//! notebook's DuckDB and run there.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use sqlly_core::{CellValue, ConnectionEnvironment, ConnectionProfile, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::QueryEvent;
use sqlly_services::notebook::cells::local as local_spec;
use sqlly_services::notebook::{
    Cell, CellKind, Completeness, ConnectionRef, ExecutionPolicy, NotebookDoc,
};

use super::{LocalPrompt, Truncation};
use crate::notebook::connection::ConnectionChoice;
use crate::notebook::view::{
    CellStatus, DispatchOutcome, NotebookEvent, NotebookOpening, NotebookView,
};
use crate::results::ResultsView;

const DEV_ID: &str = "0c3d2b1a-5f4e-4d3c-8b2a-1f0e9d8c7b6a";
const PROD_ID: &str = "6b1f3c9e-6a55-4b7e-9b0b-3f8f2d1c0a11";

fn choice(id: &str, label: &str, env: ConnectionEnvironment, db: DatabaseType) -> ConnectionChoice {
    ConnectionChoice {
        profile_id: id.into(),
        label: label.into(),
        environment: env,
        database_type: db,
        database: None,
        ..ConnectionChoice::default()
    }
}

fn choices() -> Vec<ConnectionChoice> {
    vec![
        choice(
            DEV_ID,
            "CRM",
            ConnectionEnvironment::Dev,
            DatabaseType::Postgres,
        ),
        choice(
            PROD_ID,
            "Sales",
            ConnectionEnvironment::Production,
            DatabaseType::SqlServer,
        ),
    ]
}

struct Root(#[allow(dead_code)] tempfile::TempDir);

impl Root {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(dir.path().to_path_buf()));
        Self(dir)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        crate::notebook::autosave::set_test_root(None);
    }
}

fn slot(id: &str, profile: &str, label: &str) -> ConnectionRef {
    ConnectionRef {
        id: id.into(),
        profile_id: Some(profile.into()),
        label: label.into(),
        database: None,
        extra: Default::default(),
    }
}

/// Cells as (name, slot, source); the slots are `crm` (dev) and `sales`
/// (production).
fn notebook(id: &str, cells: &[(&str, &str, &str)]) -> NotebookDoc {
    let mut nb = NotebookDoc::empty(id, "Local");
    nb.connection_refs.push(slot("crm", DEV_ID, "CRM"));
    nb.connection_refs.push(slot("sales", PROD_ID, "Sales"));
    for (name, slot, source) in cells {
        let mut cell = Cell::new(*name, CellKind::Sql, ExecutionPolicy::default());
        cell.name = Some(name.to_string());
        cell.source = source.to_string();
        match *slot {
            "local" => {
                local_spec::set_targets_local(&mut cell, true);
            }
            "" => {}
            other => cell.connection_ref = Some(other.to_string()),
        }
        nb.cells.push(cell);
    }
    nb
}

fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| {
            NotebookView::with_choices(model, NotebookOpening::New, Rc::new(choices), cx)
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

fn fail(results: &Entity<ResultsView>, cx: &mut VisualTestContext) {
    results.update(cx, |rv, cx| {
        rv.handle_event(
            QueryEvent::Error {
                message: "boom".into(),
            },
            cx,
        )
    });
}

fn state(view: &Entity<NotebookView>, id: &str, cx: &mut VisualTestContext) -> CellStatus {
    view.read_with(cx, |nb, _| nb.cell_state(id))
}

#[gpui::test]
fn independent_cells_on_two_connections_run_at_once_and_a_failure_blocks_only_dependents(
    cx: &mut TestAppContext,
) {
    let _root = Root::new();
    let model = notebook(
        "nb_concurrent",
        &[
            ("a", "crm", "SELECT 1 AS x"),
            ("b", "sales", "SELECT 2 AS y"),
            ("c", "crm", "SELECT x FROM {{a}}"),
            ("d", "sales", "SELECT 4 AS z"),
        ],
    );
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let results = attach(&view, &["a", "b", "c", "d"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    assert_eq!(
        *ran.borrow(),
        ["a", "b", "d"],
        "both connections start at once, two cells each; c waits for a"
    );
    fail(&results[0], cx);
    assert_eq!(state(&view, "c", cx), CellStatus::Blocked);
    assert_eq!(state(&view, "b", cx), CellStatus::Queued, "b keeps running");
    complete(&results[1], cx);
    complete(&results[3], cx);
    assert_eq!(
        *ran.borrow(),
        ["a", "b", "d"],
        "the blocked dependent never starts"
    );
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));
    assert_eq!(state(&view, "d", cx), CellStatus::Fresh);
}

#[gpui::test]
fn the_per_connection_limit_and_session_groups_hold_cells_back(cx: &mut TestAppContext) {
    let _root = Root::new();
    let mut model = notebook(
        "nb_limits",
        &[
            ("a", "sales", "SELECT 1"),
            ("b", "sales", "SELECT 2"),
            ("c", "sales", "SELECT 3"),
            ("t1", "crm", "SELECT 4"),
            ("t2", "crm", "SELECT 5"),
        ],
    );
    for id in ["t1", "t2"] {
        model.cell_mut(id).unwrap().session_group = Some("temp".into());
    }
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let results = attach(&view, &["a", "b", "c", "t1", "t2"], cx);
    view.update(cx, |nb, cx| {
        nb.set_per_connection(2, cx);
        nb.run_all(cx)
    });
    assert_eq!(
        *ran.borrow(),
        ["a", "b", "t1"],
        "two per connection; the session group's second cell waits"
    );
    complete(&results[3], cx);
    assert_eq!(*ran.borrow(), ["a", "b", "t1", "t2"]);
    complete(&results[0], cx);
    assert_eq!(*ran.borrow(), ["a", "b", "t1", "t2", "c"]);
}

#[gpui::test]
fn an_open_change_review_holds_new_cells_until_it_is_answered(cx: &mut TestAppContext) {
    let _root = Root::new();
    let mut model = notebook(
        "nb_review",
        &[
            ("w", "crm", "UPDATE t SET a = 1 WHERE id = 1"),
            ("r", "sales", "SELECT 1 AS x"),
            ("r2", "sales", "SELECT x FROM {{r}}"),
        ],
    );
    model.cell_mut("w").unwrap().execution_policy.read_only = false;
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let results = attach(&view, &["w", "r", "r2"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    assert_eq!(*ran.borrow(), ["w"], "r is held while w may open a review");
    view.update(cx, |nb, cx| {
        nb.dispatch_result("w", DispatchOutcome::AwaitingApproval, cx)
    });
    assert_eq!(
        *ran.borrow(),
        ["w"],
        "r and r2 are held while the review is open"
    );
    complete(&results[0], cx);
    assert_eq!(*ran.borrow(), ["w", "r"]);
    complete(&results[1], cx);
    assert_eq!(*ran.borrow(), ["w", "r", "r2"]);
}

#[gpui::test]
fn a_row_cap_marks_the_result_truncated_and_fetch_all_runs_without_it(cx: &mut TestAppContext) {
    let _root = Root::new();
    let model = notebook("nb_caps", &[("a", "sales", "SELECT * FROM big")]);
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let results = attach(&view, &["a"], cx);
    view.update(cx, |nb, cx| nb.set_max_rows("a", Some(100), cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.run_caps("a").max_rows),
        Some(100)
    );
    view.update(cx, |nb, cx| nb.run_all(cx));
    view.update(cx, |nb, cx| {
        nb.mark_truncated(
            "a",
            Truncation {
                rows_kept: 100,
                by_bytes: false,
            },
            cx,
        )
    });
    complete(&results[0], cx);
    let output = view.read_with(cx, |nb, _| nb.doc.cell_state("a").output.clone());
    assert_eq!(
        output.unwrap().completeness,
        Completeness::Truncated { cap: 100 },
        "a capped result is never presented as complete"
    );
    view.update(cx, |nb, cx| nb.fetch_all("a", cx));
    assert!(view.read_with(cx, |nb, _| nb.run_caps("a").is_empty()));
    assert!(view.read_with(cx, |nb, _| nb.truncation("a").is_none()));
    assert_eq!(*ran.borrow(), ["a", "a"]);
    complete(&results[0], cx);
    assert!(
        !view.read_with(cx, |nb, _| nb.run_caps("a").is_empty()),
        "the cap applies again after the uncapped run"
    );
}

#[gpui::test]
fn local_cells_read_inputs_by_relation_and_cross_connection_cells_point_there(
    cx: &mut TestAppContext,
) {
    let _root = Root::new();
    let model = notebook(
        "nb_sql",
        &[
            (
                "orders",
                "sales",
                "SELECT customer_id, total FROM dbo.Orders",
            ),
            ("customers", "crm", "SELECT id, name FROM crm.customers"),
            (
                "revenue",
                "local",
                "SELECT c.name, SUM(o.total) AS total FROM {{orders}} o JOIN {{customers}} c ON c.id = o.customer_id GROUP BY c.name",
            ),
            ("mixed", "sales", "SELECT * FROM {{customers}}"),
        ],
    );
    let (view, cx) = open(cx, model);
    let request = view
        .read_with(cx, |nb, _| nb.run_request("revenue"))
        .unwrap();
    assert!(request.target.local);
    assert!(request.target.read_only);
    assert_eq!(request.target.profile_id, None);
    assert!(request.sql.contains("\"orders\" o"), "{}", request.sql);
    assert!(request.sql.contains("\"customers\" c"), "{}", request.sql);
    let inputs = view
        .read_with(cx, |nb, _| nb.doc.local_inputs("revenue"))
        .unwrap();
    let names: Vec<&str> = inputs.iter().map(|i| i.relation.as_str()).collect();
    assert_eq!(names, ["orders", "customers"]);
    let err = view
        .read_with(cx, |nb, _| nb.run_request("mixed"))
        .unwrap_err();
    assert!(err.contains("customers"), "{err}");
    assert!(
        err.contains(&crate::localization::text("notebook.local.target.label")),
        "{err}"
    );
    view.update(cx, |nb, cx| nb.set_cell_local("mixed", true, cx));
    assert!(
        view.read_with(cx, |nb, _| nb.run_request("mixed"))
            .unwrap()
            .target
            .local
    );
    assert!(view.read_with(cx, |nb, _| nb.is_dirty()));
}

#[gpui::test]
fn materialize_asks_each_connection_once_then_for_a_name(cx: &mut TestAppContext) {
    let _root = Root::new();
    let model = notebook("nb_consent", &[("orders", "sales", "SELECT 1 AS x")]);
    let (view, cx) = open(cx, model);
    view.update(cx, |nb, cx| nb.request_materialize("orders", cx));
    match view.read_with(cx, |nb, _| nb.local.prompt.clone()) {
        Some(LocalPrompt::Consent {
            profile_id,
            environment,
            ..
        }) => {
            assert_eq!(profile_id, PROD_ID);
            assert_eq!(environment, ConnectionEnvironment::Production);
        }
        other => panic!("expected the opt-in prompt, got {other:?}"),
    }
    view.update(cx, |nb, cx| nb.confirm_local_prompt(cx));
    assert!(view.read_with(cx, |nb, _| nb.local.prefs.allows_materialize(PROD_ID)));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.local.prompt.clone()),
        Some(LocalPrompt::Name {
            cell_id: "orders".into()
        })
    );
    view.update(cx, |nb, cx| nb.cancel_local_prompt(cx));
    view.update(cx, |nb, cx| nb.request_materialize("orders", cx));
    assert!(
        matches!(
            view.read_with(cx, |nb, _| nb.local.prompt.clone()),
            Some(LocalPrompt::Name { .. })
        ),
        "the connection is not asked about again"
    );
    assert!(
        super::LocalPrefs::load().allows_materialize(PROD_ID),
        "the opt-in is saved"
    );
}

fn columns(names: &[(&str, &str)]) -> Vec<ResultColumn> {
    super::materialize::columns(names)
}

fn feed(
    results: &Entity<ResultsView>,
    cols: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    cx: &mut VisualTestContext,
) {
    results.update(cx, |rv, cx| {
        rv.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: cols,
            },
            cx,
        );
        rv.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
    });
}

fn wait_until(cx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    // Generous: a loaded CI machine running the whole workspace can take
    // seconds to close and reopen a DuckDB workspace.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for local data");
}

#[gpui::test]
fn a_cross_connection_join_copies_both_inputs_with_provenance_then_runs_locally(
    cx: &mut TestAppContext,
) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let mut model = notebook(
        "nb_cross_join",
        &[
            (
                "orders",
                "sales",
                "SELECT customer_id, total FROM dbo.Orders",
            ),
            ("customers", "crm", "SELECT id, name FROM crm.customers"),
            (
                "revenue",
                "local",
                "SELECT c.name, o.total FROM {{orders}} o JOIN {{customers}} c ON c.id = o.customer_id ORDER BY c.name",
            ),
        ],
    );
    local_spec::set_auto_materialize(&mut model, true);
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let resolver: super::run::QueryResolver = Rc::new(|target, _| {
        let id = target.profile_id.expect("a connection target");
        let (name, env, db) = if id.to_string() == PROD_ID {
            (
                "Sales",
                ConnectionEnvironment::Production,
                DatabaseType::SqlServer,
            )
        } else {
            ("CRM", ConnectionEnvironment::Dev, DatabaseType::Postgres)
        };
        let mut profile = ConnectionProfile::sql_login(
            name,
            "db.example",
            "reader",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "hunter2".into(),
            },
        );
        profile.id = sqlly_core::ConnectionId(id);
        profile.environment = env;
        profile.database_type = db;
        let executor: Arc<dyn sqlly_services::QueryExecutor> =
            super::materialize::fake::ScriptedExecutor::new(Vec::new());
        Ok((profile, executor))
    });
    let results = attach(&view, &["orders", "customers", "revenue"], cx);
    view.update(cx, |nb, cx| {
        nb.set_query_resolver(resolver);
        nb.local.prefs.materialize_allowed.insert(PROD_ID.into());
        nb.local.prefs.materialize_allowed.insert(DEV_ID.into());
        nb.run_all(cx);
    });
    assert_eq!(*ran.borrow(), ["orders", "customers"]);
    feed(
        &results[0],
        columns(&[("customer_id", "int"), ("total", "decimal(10,2)")]),
        vec![
            vec![CellValue::I64(1), CellValue::String("10.50".into())],
            vec![CellValue::I64(2), CellValue::String("7.25".into())],
        ],
        cx,
    );
    feed(
        &results[1],
        columns(&[("id", "int4"), ("name", "text")]),
        vec![
            vec![CellValue::I64(1), CellValue::String("Ada".into())],
            vec![CellValue::I64(2), CellValue::String("Lin".into())],
        ],
        cx,
    );
    wait_until(cx, |_| ran.borrow().len() == 3);
    assert_eq!(
        ran.borrow()[2],
        "revenue",
        "the local cell runs once its inputs are in"
    );
    assert!(view.read_with(cx, |nb, _| nb.local_profile().is_some()));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| {
            let inputs = nb.doc.local_inputs("revenue").unwrap();
            inputs.iter().all(|i| nb.input_current(i))
        })
    });
    view.read_with(cx, |nb, _| {
        let orders = nb.local.relation("orders").unwrap();
        assert_eq!(orders.rows, 2);
        let prov = orders.provenance.as_ref().unwrap();
        assert_eq!(prov.label, "Sales");
        assert_eq!(prov.environment.as_deref(), Some("PRODUCTION"));
        assert_eq!(prov.cell_id.as_deref(), Some("orders"));
        assert!(prov.complete);
        assert!(prov.extracted_at.is_some());
        let customers = nb.local.relation("customers").unwrap();
        assert_eq!(customers.provenance.as_ref().unwrap().label, "CRM");
        let inputs = nb.doc.local_inputs("revenue").unwrap();
        assert!(
            inputs.iter().all(|i| nb.input_current(i)),
            "copies are current"
        );
    });
    let profile = view.read_with(cx, |nb, _| nb.local_profile()).unwrap();
    assert_eq!(profile.database_type, DatabaseType::DuckDb);
    assert!(profile.block_all_but_select && profile.enforce_read_only);
    assert!(!profile.server.contains("hunter2"));

    // Editing an input's SQL makes its copy out of date.
    view.update(cx, |nb, _| {
        nb.doc.set_source(
            "orders",
            "SELECT customer_id, total FROM dbo.Orders WHERE total > 1",
        );
    });
    let inputs = view
        .read_with(cx, |nb, _| nb.doc.local_inputs("revenue"))
        .unwrap();
    assert!(!view.read_with(cx, |nb, _| nb.input_current(&inputs[0])));
    assert!(view.read_with(cx, |nb, _| nb.input_current(&inputs[1])));
}

#[gpui::test]
fn a_local_cell_asks_before_copying_inputs_unless_the_notebook_allows_it(cx: &mut TestAppContext) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let model = notebook(
        "nb_ask_inputs",
        &[
            ("orders", "sales", "SELECT 1 AS id"),
            ("joined", "local", "SELECT * FROM {{orders}}"),
        ],
    );
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let resolver: super::run::QueryResolver = Rc::new(|target, _| {
        let mut profile = ConnectionProfile::sql_login(
            "Sales",
            "db.example",
            "reader",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.id = sqlly_core::ConnectionId(target.profile_id.unwrap());
        let executor: Arc<dyn sqlly_services::QueryExecutor> =
            super::materialize::fake::ScriptedExecutor::new(vec![
                QueryEvent::ResultSet {
                    index: 0,
                    columns: columns(&[("id", "int")]),
                },
                QueryEvent::Rows {
                    index: 0,
                    rows: vec![vec![CellValue::I64(1)]],
                },
                QueryEvent::Completed { elapsed_ms: 1 },
            ]);
        Ok((profile, executor))
    });
    attach(&view, &["orders", "joined"], cx);
    view.update(cx, |nb, cx| {
        nb.set_query_resolver(resolver);
        nb.local.prefs.materialize_allowed.insert(PROD_ID.into());
        nb.activate_cell("joined", cx);
        nb.run_scope(
            crate::notebook::doc_adapter::RunScope::Cell,
            Some("joined".into()),
            cx,
        );
    });
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| {
            matches!(nb.local.prompt, Some(LocalPrompt::Inputs { .. }))
        })
    });
    assert!(ran.borrow().is_empty(), "nothing runs before the answer");
    assert_eq!(state(&view, "joined", cx), CellStatus::Queued);
    view.update(cx, |nb, cx| nb.confirm_local_prompt(cx));
    wait_until(cx, |_| ran.borrow().iter().any(|id| id == "joined"));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.relation("orders").is_some())
    });
    view.read_with(cx, |nb, _| {
        let prov = nb
            .local
            .relation("orders")
            .unwrap()
            .provenance
            .clone()
            .unwrap();
        assert_eq!(prov.label, "Sales");
        assert!(prov.run_id.is_some(), "a query copy gets its own run id");
    });
}

#[gpui::test]
fn an_attachment_imports_its_file_and_notices_when_it_changes(cx: &mut TestAppContext) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("people.csv");
    std::fs::write(&csv, "id,name\n1,Ada\n2,Lin\n").unwrap();
    let mut model = NotebookDoc::empty("nb_attach", "Attach");
    model.cells.push(Cell::new(
        "people",
        CellKind::Attachment,
        ExecutionPolicy::default(),
    ));
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let notebook_path = dir.path().join("demo.sqllynb");
    std::fs::write(&notebook_path, "{}").unwrap();
    view.update(cx, |nb, cx| {
        nb.opened_from(notebook_path.to_string_lossy().into_owned(), cx);
        let (hash, size) = sqlly_services::notebook::cells::attachment::file_hash(&csv).unwrap();
        nb.set_attachment_file("people", &csv, hash, size, cx);
    });
    let spec = view
        .read_with(cx, |nb, _| nb.doc.attachment("people"))
        .unwrap();
    assert_eq!(
        spec.path.as_deref(),
        Some("people.csv"),
        "stored next to the notebook"
    );
    assert_eq!(spec.relation.as_deref(), Some("people"));
    attach(&view, &["people"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    wait_until(cx, |_| ran.borrow().len() == 1);
    let summary = view
        .read_with(cx, |nb, _| nb.local.imports.get("people").cloned())
        .unwrap();
    assert_eq!(summary.rows, 2);
    assert_eq!(summary.columns.len(), 2);
    assert!(!summary.changed);
    let preview = view
        .read_with(cx, |nb, _| nb.run_request("people"))
        .unwrap();
    assert!(preview.target.local);
    assert!(preview.sql.contains("\"people\""), "{}", preview.sql);

    let results = view.read_with(cx, |nb, _| {
        nb.cells.get("people").and_then(|rt| rt.results.clone())
    });
    complete(&results.unwrap(), cx);
    std::fs::write(&csv, "id,name\n1,Ada\n2,Lin\n3,Grace\n").unwrap();
    view.update(cx, |nb, cx| nb.run_all(cx));
    wait_until(cx, |_| ran.borrow().len() == 2);
    let summary = view
        .read_with(cx, |nb, _| nb.local.imports.get("people").cloned())
        .unwrap();
    assert_eq!(summary.rows, 3);
    assert!(summary.changed, "the new content was noticed");
    let after = view
        .read_with(cx, |nb, _| nb.doc.attachment("people"))
        .unwrap();
    assert_ne!(after.content_hash, spec.content_hash);
}

#[gpui::test]
fn an_image_attachment_is_checked_before_it_is_shown(cx: &mut TestAppContext) {
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("chart.png");
    std::fs::write(&fake, b"<svg onload=alert(1)>").unwrap();
    let mut model = NotebookDoc::empty("nb_image", "Image");
    model.cells.push(Cell::new(
        "pic",
        CellKind::Attachment,
        ExecutionPolicy::default(),
    ));
    let (view, cx) = open(cx, model);
    let notebook_path = dir.path().join("demo.sqllynb");
    view.update(cx, |nb, cx| {
        nb.opened_from(notebook_path.to_string_lossy().into_owned(), cx);
        nb.set_attachment_file("pic", &fake, "file-sha256:x".into(), 20, cx);
        nb.run_all(cx);
    });
    assert_eq!(state(&view, "pic", cx), CellStatus::Failed);
    assert!(view.read_with(cx, |nb, _| !nb.local.images.contains_key("pic")));

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&40u32.to_be_bytes());
    png.extend_from_slice(&20u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    std::fs::write(&fake, &png).unwrap();
    view.update(cx, |nb, cx| nb.run_all(cx));
    assert_eq!(state(&view, "pic", cx), CellStatus::Fresh);
    let image = view.read_with(cx, |nb, _| {
        nb.local.images.get("pic").map(|i| (i.width, i.height))
    });
    assert_eq!(image, Some((40, 20)));
}

#[gpui::test]
fn a_write_cell_is_never_run_again_to_copy_its_rows(cx: &mut TestAppContext) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let mut model = notebook(
        "nb_write_copy",
        &[
            (
                "touch",
                "crm",
                "UPDATE crm.customers SET seen = now() RETURNING id",
            ),
            ("joined", "local", "SELECT * FROM {{touch}}"),
        ],
    );
    model.cells[0].execution_policy.read_only = false;
    local_spec::set_auto_materialize(&mut model, true);
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    let resolved = Rc::new(std::cell::Cell::new(0));
    let calls = resolved.clone();
    let resolver: super::run::QueryResolver = Rc::new(move |target, _| {
        calls.set(calls.get() + 1);
        let mut profile = ConnectionProfile::sql_login(
            "CRM",
            "db.example",
            "writer",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.id = sqlly_core::ConnectionId(target.profile_id.unwrap());
        let executor: Arc<dyn sqlly_services::QueryExecutor> =
            super::materialize::fake::ScriptedExecutor::new(Vec::new());
        Ok((profile, executor))
    });
    attach(&view, &["touch", "joined"], cx);
    view.update(cx, |nb, cx| {
        nb.set_query_resolver(resolver);
        nb.local.prefs.materialize_allowed.insert(DEV_ID.into());
        nb.activate_cell("joined", cx);
        nb.run_scope(
            crate::notebook::doc_adapter::RunScope::Cell,
            Some("joined".into()),
            cx,
        );
    });
    wait_until(cx, |cx| state(&view, "joined", cx) == CellStatus::Failed);
    assert_eq!(resolved.get(), 0, "the write cell's SQL is not sent again");
    assert!(!ran.borrow().iter().any(|id| id == "joined"));

    view.update(cx, |nb, cx| {
        nb.local.prompt = Some(LocalPrompt::Refetch {
            cell_id: "touch".into(),
            relation: "touch".into(),
        });
        nb.confirm_local_prompt(cx);
    });
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(resolved.get(), 0, "Refetch never re-runs a write cell");
    assert!(view.read_with(cx, |nb, _| nb.local.relation("touch").is_none()));
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

#[gpui::test]
fn a_profile_of_an_imported_table_takes_column_statistics_from_the_whole_table(
    cx: &mut TestAppContext,
) {
    use crate::notebook::analysis::CellViewHandle;
    use sqlly_services::notebook::cells::profile::Coverage;
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("orders.csv");
    let mut text = String::from("id,amount\n");
    for n in 0..2500 {
        if n % 10 == 9 {
            text.push_str(&format!("{n},\n"));
        } else {
            text.push_str(&format!("{n},{}\n", n * 2));
        }
    }
    std::fs::write(&csv, text).unwrap();
    let mut model = NotebookDoc::empty("nb_profile_whole", "Profile");
    model.cells.push(Cell::new(
        "orders",
        CellKind::Attachment,
        ExecutionPolicy::default(),
    ));
    let mut prof = Cell::new("prof", CellKind::Profile, ExecutionPolicy::default());
    prof.spec = serde_json::json!({"input": "orders"});
    model.cells.push(prof);
    let (view, cx) = open(cx, model);
    let ran = record(&view, cx);
    view.update(cx, |nb, cx| {
        nb.opened_from(
            dir.path()
                .join("demo.sqllynb")
                .to_string_lossy()
                .into_owned(),
            cx,
        );
        let (hash, size) = sqlly_services::notebook::cells::attachment::file_hash(&csv).unwrap();
        nb.set_attachment_file("orders", &csv, hash, size, cx);
    });
    draw(&view, cx);
    let results = attach(&view, &["orders"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    wait_until(cx, |_| ran.borrow().len() == 1);
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| {
            nb.local.relation("orders").is_some_and(|r| r.rows == 2500)
        })
    });
    // The preview the host shows: the table's first 1,000 rows.
    let preview: Vec<Vec<CellValue>> = (0..1000)
        .map(|n| {
            vec![
                CellValue::I64(n),
                if n % 10 == 9 {
                    CellValue::Null
                } else {
                    CellValue::I64(n * 2)
                },
            ]
        })
        .collect();
    feed(
        &results[0],
        columns(&[("id", "BIGINT"), ("amount", "BIGINT")]),
        preview,
        cx,
    );
    wait_until(cx, |cx| {
        draw(&view, cx);
        view.read_with(cx, |nb, cx| match nb.analysis.views.get("prof") {
            Some(CellViewHandle::Profile(p)) => p.read(cx).coverage().is_some(),
            _ => false,
        })
    });
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("prof"), CellStatus::Fresh);
        let Some(CellViewHandle::Profile(p)) = nb.analysis.views.get("prof") else {
            panic!("profile view")
        };
        let p = p.read(cx);
        assert_eq!(
            p.coverage(),
            Some(Coverage::Mixed {
                read: 1000,
                total: 2500
            })
        );
        let whole = p.whole().expect("whole-table statistics");
        let amount = whole.column("amount").unwrap();
        assert_eq!(amount.null_count, 250);
        assert_eq!(amount.max.as_deref(), Some("4996"));
        let profile = p.profile().unwrap();
        let first = profile.column("amount").unwrap();
        assert_eq!(first.null_count, 100, "histograms keep the rows read");
        let (cols, rows) = p.summary().unwrap();
        let max_at = cols.len() - 4;
        let amount_row = rows
            .iter()
            .find(|r| r[0] == CellValue::String("amount".into()))
            .unwrap();
        assert_eq!(amount_row[3], CellValue::F64(10.0));
        assert_eq!(amount_row[max_at], CellValue::String("4996".into()));
        let output = nb.analysis.outputs.get("prof").unwrap();
        let source = output
            .columns
            .iter()
            .position(|c| c.name == "source")
            .unwrap();
        assert!(
            output
                .rows
                .iter()
                .all(|r| r[source] == CellValue::String("table".into()))
        );
        assert!(
            crate::notebook::cells::profile::coverage_text(p.coverage().unwrap()).contains("2500")
        );
    });
}

/// The docs' order-amounts notebook: an attachment importing `rows` orders
/// and a profile of it, loaded the way a file opens.
fn order_amounts_notebook(dir: &std::path::Path, rows: usize) -> NotebookDoc {
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let mut csv = String::from("order_id,region,amount\n");
    for n in 0..rows {
        csv.push_str(&format!(
            "{},{},{}.25\n",
            n + 1,
            ["North", "South"][n % 2],
            n * 3
        ));
    }
    std::fs::write(dir.join("data").join("orders.csv"), csv).unwrap();
    let text = serde_json::json!({
        "format": "sqlly.notebook",
        "format_version": 2,
        "notebook_id": "ordersprofile01",
        "title": "Order amounts",
        "connection_refs": [],
        "parameters": [],
        "cells": [
            {"id": "cell_1", "kind": "attachment", "name": "orders", "spec": {
                "relation": "orders", "path": "data/orders.csv",
                "format": "csv", "file_name": "orders.csv"
            }},
            {"id": "cell_2", "kind": "profile", "name": "order_profile",
             "spec": {"input": "orders"}}
        ]
    })
    .to_string();
    sqlly_services::notebook::load(
        Some("orders.sqllynb"),
        &text,
        &mut sqlly_services::notebook::SeededIds(1),
        None,
    )
    .unwrap()
    .doc
}

#[gpui::test]
fn a_profile_after_an_attachment_on_run_all_reads_the_whole_imported_table(
    cx: &mut TestAppContext,
) {
    use crate::notebook::analysis::CellViewHandle;
    use sqlly_services::notebook::cells::profile::Coverage;
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open(cx, order_amounts_notebook(dir.path(), 2400));
    view.update(cx, |nb, cx| {
        nb.opened_from(
            dir.path()
                .join("orders.sqllynb")
                .to_string_lossy()
                .into_owned(),
            cx,
        )
    });
    draw(&view, cx);
    let results = attach(&view, &["cell_1"], cx);
    // A host that answers the preview at once, the moment it is asked.
    let preview = results[0].clone();
    cx.update(|_, cx| {
        cx.subscribe(&view, move |_, event: &NotebookEvent, cx| {
            if matches!(event, NotebookEvent::RunCell { cell_id } if cell_id == "cell_1") {
                let rows = (0..1000)
                    .map(|n| {
                        vec![
                            CellValue::I64(n + 1),
                            CellValue::String(["North", "South"][n as usize % 2].into()),
                            CellValue::F64(n as f64 * 3.0 + 0.25),
                        ]
                    })
                    .collect();
                preview.update(cx, |rv, cx| {
                    rv.handle_event(
                        QueryEvent::ResultSet {
                            index: 0,
                            columns: columns(&[
                                ("order_id", "BIGINT"),
                                ("region", "VARCHAR"),
                                ("amount", "DOUBLE"),
                            ]),
                        },
                        cx,
                    );
                    rv.handle_event(QueryEvent::Rows { index: 0, rows }, cx);
                    rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
                });
            }
        })
        .detach();
    });
    view.update(cx, |nb, cx| nb.run_all(cx));
    wait_until(cx, |cx| {
        draw(&view, cx);
        view.read_with(cx, |nb, cx| match nb.analysis.views.get("cell_2") {
            Some(CellViewHandle::Profile(p)) => p.read(cx).coverage().is_some(),
            _ => false,
        })
    });
    view.read_with(cx, |nb, cx| {
        assert_eq!(nb.cell_state("cell_2"), CellStatus::Fresh);
        let Some(CellViewHandle::Profile(p)) = nb.analysis.views.get("cell_2") else {
            panic!("profile view")
        };
        assert_eq!(
            p.read(cx).coverage(),
            Some(Coverage::Mixed {
                read: 1000,
                total: 2400
            }),
            "the Whole table (DuckDB) section shows"
        );
    });
}

/// The docs' customer-segments notebook: an attachment importing a CSV and
/// a local cell reading its table, loaded the way a file opens.
fn segments_notebook(dir: &std::path::Path, id: &str, rows: usize) -> NotebookDoc {
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let mut csv = String::from("customer_id,segment,lifetime_value\n");
    for n in 0..rows {
        csv.push_str(&format!(
            "{},{},{}\n",
            n + 1,
            ["Retail", "Wholesale"][n % 2],
            n * 7
        ));
    }
    std::fs::write(dir.join("data").join("segments.csv"), csv).unwrap();
    let text = serde_json::json!({
        "format": "sqlly.notebook",
        "format_version": 2,
        "notebook_id": id,
        "title": "Customer segments",
        "connection_refs": [],
        "parameters": [],
        "cells": [
            {"id": "cell_1", "kind": "attachment", "name": "segments", "spec": {
                "relation": "segments", "path": "data/segments.csv",
                "format": "csv", "file_name": "segments.csv"
            }},
            {"id": "cell_2", "kind": "sql", "name": "by_segment", "spec": {"target": "local"},
             "source": "SELECT segment, COUNT(*) AS customers FROM {{segments}} GROUP BY segment",
             "execution_policy": {"read_only": true}}
        ]
    })
    .to_string();
    sqlly_services::notebook::load(
        Some("segments.sqllynb"),
        &text,
        &mut sqlly_services::notebook::SeededIds(1),
        None,
    )
    .unwrap()
    .doc
}

#[gpui::test]
fn run_all_starts_a_local_cell_only_after_the_attachment_it_reads(cx: &mut TestAppContext) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open(cx, segments_notebook(dir.path(), "segmentsslow01", 400));
    view.update(cx, |nb, cx| {
        nb.opened_from(
            dir.path()
                .join("segments.sqllynb")
                .to_string_lossy()
                .into_owned(),
            cx,
        )
    });
    draw(&view, cx);
    let results = attach(&view, &["cell_1", "cell_2"], cx);
    // A host that answers each query the moment it is asked, noting
    // whether the segments table was listed in local data by then.
    let asked: Rc<RefCell<Vec<(String, bool)>>> = Rc::default();
    let seen = asked.clone();
    let hosted = results.clone();
    let watched = view.clone();
    cx.update(|_, cx| {
        cx.subscribe(&view, move |_, event: &NotebookEvent, cx| {
            let NotebookEvent::RunCell { cell_id } = event else {
                return;
            };
            let listed = watched.read(cx).local.relation("segments").is_some();
            seen.borrow_mut().push((cell_id.clone(), listed));
            let at = if cell_id == "cell_1" { 0 } else { 1 };
            hosted[at].update(cx, |rv, cx| {
                rv.handle_event(
                    QueryEvent::ResultSet {
                        index: 0,
                        columns: columns(&[("segment", "VARCHAR")]),
                    },
                    cx,
                );
                rv.handle_event(
                    QueryEvent::Rows {
                        index: 0,
                        rows: vec![vec![CellValue::String("Retail".into())]],
                    },
                    cx,
                );
                rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx);
            });
        })
        .detach();
    });
    let csv = dir.path().join("data").join("segments.csv");
    super::run::SLOW_IMPORTS
        .lock()
        .unwrap()
        .push((csv.clone(), 300));
    view.update(cx, |nb, cx| nb.run_all(cx));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| {
            matches!(
                nb.cell_state("cell_2"),
                CellStatus::Fresh | CellStatus::Failed
            )
        })
    });
    super::run::SLOW_IMPORTS
        .lock()
        .unwrap()
        .retain(|(p, _)| *p != csv);
    view.read_with(cx, |nb, _| {
        assert_eq!(
            nb.cell_state("cell_2"),
            CellStatus::Fresh,
            "by_segment: {:?}",
            nb.cells.get("cell_2").and_then(|r| r.message.clone())
        );
        assert_eq!(nb.local.relation("segments").map(|r| r.rows), Some(400));
    });
    assert_eq!(
        asked.borrow().clone(),
        vec![("cell_1".to_string(), true), ("cell_2".to_string(), true)],
        "the local cell is asked for only once the import is in local data"
    );
}

/// An attachment whose output is still current but whose table is gone
/// from local data (cleared, evicted, a cached output on another machine)
/// is imported again before the local cell after it runs.
#[gpui::test]
fn run_all_imports_again_an_attachment_whose_table_is_gone(cx: &mut TestAppContext) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open(cx, segments_notebook(dir.path(), "segmentsgone01", 300));
    view.update(cx, |nb, cx| {
        nb.opened_from(
            dir.path()
                .join("segments.sqllynb")
                .to_string_lossy()
                .into_owned(),
            cx,
        )
    });
    draw(&view, cx);
    let ran = record(&view, cx);
    let preview = || vec![vec![CellValue::I64(1), CellValue::String("Retail".into())]];
    let cols = || columns(&[("customer_id", "BIGINT"), ("segment", "VARCHAR")]);
    let results = attach(&view, &["cell_1", "cell_2"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    wait_until(cx, |_| ran.borrow().len() == 1);
    feed(&results[0], cols(), preview(), cx);
    wait_until(cx, |_| ran.borrow().len() == 2);
    feed(&results[1], cols(), preview(), cx);
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.cell_state("cell_2") == CellStatus::Fresh)
    });

    view.update(cx, |nb, cx| nb.clear_local_data(cx));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.relations.is_empty())
    });
    ran.borrow_mut().clear();
    let results = attach(&view, &["cell_1", "cell_2"], cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    wait_until(cx, |cx| {
        !ran.borrow().is_empty()
            || view.read_with(cx, |nb, _| nb.cell_state("cell_2") == CellStatus::Failed)
    });
    assert_eq!(
        ran.borrow().clone(),
        vec!["cell_1".to_string()],
        "the attachment imports first; by_segment: {:?}",
        view.read_with(cx, |nb, _| nb
            .cells
            .get("cell_2")
            .and_then(|r| r.message.clone()))
    );
    feed(&results[0], cols(), preview(), cx);
    wait_until(cx, |_| ran.borrow().len() == 2);
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.local.relation("segments").map(|r| r.rows), Some(300));
    });
}

/// Importing records the file's hash in the attachment's spec, which is
/// part of its fingerprint: the run must complete under that spec, or the
/// attachment reads Out of date the moment it finishes.
#[gpui::test]
fn an_attachment_is_up_to_date_after_run_all_imports_it(cx: &mut TestAppContext) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let (view, cx) = open(cx, segments_notebook(dir.path(), "segmentsfresh01", 200));
    view.update(cx, |nb, cx| {
        nb.opened_from(
            dir.path()
                .join("segments.sqllynb")
                .to_string_lossy()
                .into_owned(),
            cx,
        )
    });
    draw(&view, cx);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.doc.attachment("cell_1"))
            .and_then(|s| s.content_hash),
        None,
        "the fixture has never been imported"
    );
    let ran = record(&view, cx);
    let preview = || vec![vec![CellValue::String("Retail".into())]];
    let cols = || columns(&[("segment", "VARCHAR")]);
    let csv = dir.path().join("data").join("segments.csv");
    let mut hashes = Vec::new();
    for pass in 0..3 {
        if pass == 2 {
            std::fs::write(&csv, "customer_id,segment,lifetime_value\n1,Retail,7\n").unwrap();
        }
        ran.borrow_mut().clear();
        let results = attach(&view, &["cell_1", "cell_2"], cx);
        view.update(cx, |nb, cx| nb.run_all(cx));
        wait_until(cx, |_| ran.borrow().len() == 1);
        feed(&results[0], cols(), preview(), cx);
        wait_until(cx, |_| ran.borrow().len() == 2);
        feed(&results[1], cols(), preview(), cx);
        wait_until(cx, |cx| view.read_with(cx, |nb, _| !nb.is_running()));
        view.read_with(cx, |nb, _| {
            assert_eq!(
                nb.cell_state("cell_1"),
                CellStatus::Fresh,
                "pass {pass}: the attachment that just imported is up to date"
            );
            assert_eq!(nb.cell_state("cell_2"), CellStatus::Fresh, "pass {pass}");
            hashes.push(nb.doc.attachment("cell_1").and_then(|s| s.content_hash));
        });
    }
    assert!(hashes[0].is_some());
    assert_eq!(hashes[0], hashes[1], "the same file keeps its hash");
    assert_ne!(hashes[1], hashes[2], "a changed file records its new hash");
}

#[gpui::test]
fn local_data_encryption_switches_only_while_the_notebook_has_no_tables(cx: &mut TestAppContext) {
    use sqlly_services::notebook_security::workspace_keys::MemoryWorkspaceKeys;
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let (view, cx) = open(cx, NotebookDoc::empty("nb_encrypt_switch", "Encrypt"));
    let keys = Arc::new(MemoryWorkspaceKeys::default());
    let scope = view.update(cx, |nb, cx| {
        nb.local.keys = Some(keys.clone());
        nb.ensure_local_data_loaded(cx);
        nb.trust
            .scope
            .clone()
            .expect("a notebook created here is trusted")
    });
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.workspace.is_some())
    });
    assert!(view.read_with(cx, |nb, _| !nb.local.encrypted));

    view.update(cx, |nb, cx| nb.set_local_encryption(true, cx));
    assert!(view.read_with(cx, |nb, _| nb.local.switching_encryption));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| {
            !nb.local.switching_encryption && nb.local.workspace.is_some()
        })
    });
    let ws = view.read_with(cx, |nb, _| {
        assert!(nb.local.encrypted, "{:?}", nb.local.error);
        nb.local.workspace.clone().unwrap()
    });
    assert!(ws.workspace().is_encrypted());
    assert!(keys.contains(&scope), "the key went to the keychain");

    crate::tokio_handle()
        .block_on(ws.materialize(
            "kept",
            columns(&[("id", "int")]),
            sqlly_services::local_analytics::SnapshotRows::new(Arc::new(vec![vec![
                CellValue::I64(1),
            ]])),
            sqlly_services::local_analytics::SourceInfo::query(DatabaseType::Postgres, "CRM"),
            sqlly_services::local_analytics::WriteMode::FailIfExists,
            sqlly_services::local_analytics::CancelToken::new(),
        ))
        .unwrap();
    drop(ws);
    view.update(cx, |nb, cx| nb.refresh_local_data(cx));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.relations.len() == 1)
    });

    view.update(cx, |nb, cx| nb.set_local_encryption(false, cx));
    view.read_with(cx, |nb, _| {
        assert!(
            nb.local.encryption_needs_clear,
            "tables must be cleared first"
        );
        assert!(nb.local.encrypted);
        assert!(!nb.local.switching_encryption);
    });
    assert!(keys.contains(&scope));
    draw(&view, cx);

    view.update(cx, |nb, cx| nb.clear_local_data(cx));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| {
            nb.local.relations.is_empty() && !nb.local.encryption_needs_clear
        })
    });
    view.update(cx, |nb, cx| nb.set_local_encryption(false, cx));
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| {
            !nb.local.switching_encryption && nb.local.workspace.is_some()
        })
    });
    view.read_with(cx, |nb, _| {
        assert!(!nb.local.encrypted, "{:?}", nb.local.error);
        assert!(
            !nb.local
                .workspace
                .as_ref()
                .unwrap()
                .workspace()
                .is_encrypted()
        );
    });
    assert!(!keys.contains(&scope), "the key is forgotten");
}

#[gpui::test]
fn encrypting_an_empty_workspace_from_the_open_local_data_rail_reopens_it_encrypted(
    cx: &mut TestAppContext,
) {
    use crate::notebook::view::NotebookCommand;
    use sqlly_services::notebook_security::workspace_keys::MemoryWorkspaceKeys;
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let (view, cx) = open(cx, NotebookDoc::empty("nb_encrypt_rail", "Encrypt"));
    let keys = Arc::new(MemoryWorkspaceKeys::default());
    view.update(cx, |nb, cx| {
        nb.local.keys = Some(keys.clone());
        nb.command(NotebookCommand::ShowLocalData, cx);
    });
    draw(&view, cx);
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.workspace.is_some())
    });

    // The switch's click: the rail redraws while the switch runs.
    view.update(cx, |nb, cx| nb.set_local_encryption(true, cx));
    draw(&view, cx);
    wait_until(cx, |cx| {
        draw(&view, cx);
        view.read_with(cx, |nb, _| {
            !nb.local.switching_encryption && !nb.local.opening && nb.local.workspace.is_some()
        })
    });
    let ws = view.read_with(cx, |nb, _| {
        assert_eq!(nb.local.error, None);
        assert!(nb.local.encrypted);
        nb.local.workspace.clone().unwrap()
    });
    assert!(ws.workspace().is_encrypted());
    drop(ws);

    // Closed and opened again, it comes back encrypted with the kept key.
    view.update(cx, |nb, cx| {
        nb.local.workspace = None;
        nb.ensure_local_data_loaded(cx);
    });
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.workspace.is_some())
    });
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.local.error, None);
        assert!(nb.local.encrypted);
        assert!(
            nb.local
                .workspace
                .as_ref()
                .unwrap()
                .workspace()
                .is_encrypted()
        );
    });
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png
}

fn markdown_with_picture(id: &str) -> NotebookDoc {
    let mut model = NotebookDoc::empty(id, "Pictures");
    model.cells.push(Cell::new(
        "pic",
        CellKind::Attachment,
        ExecutionPolicy::default(),
    ));
    let mut text = Cell::new("notes", CellKind::Markdown, ExecutionPolicy::default());
    text.source = "![Trend](attachment:pic) ![remote](https://example.com/beacon.png) \
                   ![gone](attachment:missing)"
        .into();
    model.cells.push(text);
    model
}

#[gpui::test]
fn markdown_shows_attachment_pictures_and_never_loads_other_images(cx: &mut TestAppContext) {
    use crate::notebook::cells::markdown::markdown_image;
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    let picture = dir.path().join("trend.png");
    std::fs::write(&picture, png(40, 20)).unwrap();
    let (view, cx) = open(cx, markdown_with_picture("nb_md_pictures"));
    let source = view.update(cx, |nb, cx| {
        nb.opened_from(
            dir.path()
                .join("demo.sqllynb")
                .to_string_lossy()
                .into_owned(),
            cx,
        );
        let (hash, size) =
            sqlly_services::notebook::cells::attachment::file_hash(&picture).unwrap();
        nb.set_attachment_file("pic", &picture, hash, size, cx);
        nb.doc.cell_by_id("notes").unwrap().source.to_string()
    });
    assert_eq!(
        view.read_with(cx, |nb, _| nb.doc.attachment("pic").unwrap().path),
        Some("trend.png".into())
    );
    draw(&view, cx);
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.images.contains_key("pic"))
    });
    assert_eq!(
        state(&view, "pic", cx),
        CellStatus::Idle,
        "showing the picture does not run the cell"
    );
    let images = view
        .update(cx, |nb, cx| nb.markdown_images(&source, cx))
        .expect("the text names attachments");
    let pic = view.read_with(cx, |nb, _| nb.local.images["pic"].image.clone());
    assert!(Arc::ptr_eq(
        &markdown_image("attachment:pic", Some(&images)),
        &pic
    ));
    let blocked = markdown_image("https://example.com/beacon.png", Some(&images));
    assert!(!Arc::ptr_eq(&blocked, &pic));
    assert!(Arc::ptr_eq(
        &markdown_image("attachment:missing", Some(&images)),
        &blocked
    ));
    assert!(Arc::ptr_eq(
        &markdown_image("file:///etc/hosts", None),
        &blocked
    ));
    let embedded =
        cx.update(|window, cx| view.update(cx, |nb, cx| nb.output_render_cx("notes", window, cx)));
    let embedded = embedded
        .and_then(|render| render.markdown_images)
        .expect("an embedded Markdown output gets the pictures too");
    assert!(Arc::ptr_eq(
        &markdown_image("attachment:pic", Some(&embedded)),
        &pic
    ));
    draw(&view, cx);

    // Not a picture: never shown, and asked for only once.
    std::fs::write(&picture, b"<svg onload=alert(1)>").unwrap();
    view.update(cx, |nb, cx| {
        let (hash, size) =
            sqlly_services::notebook::cells::attachment::file_hash(&picture).unwrap();
        nb.set_attachment_file("pic", &picture, hash, size, cx);
    });
    draw(&view, cx);
    wait_until(cx, |cx| {
        view.read_with(cx, |nb, _| nb.local.image_loads.contains("pic"))
    });
    cx.run_until_parked();
    assert!(view.read_with(cx, |nb, _| !nb.local.images.contains_key("pic")));
}

#[gpui::test]
fn an_untrusted_notebook_reads_no_attachment_pictures(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let _root = Root::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("trend.png"), png(40, 20)).unwrap();
    let mut model = markdown_with_picture("nb_md_untrusted");
    sqlly_services::notebook::cells::attachment::AttachmentSpec {
        path: Some("trend.png".into()),
        format: Some(sqlly_services::notebook::cells::attachment::AttachmentFormat::Png),
        ..Default::default()
    }
    .write(&mut model.cells[0]);
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| {
            NotebookView::with_choices(model, NotebookOpening::File, Rc::new(choices), cx)
        });
        gpui_component::Root::new(nb, window, cx)
    });
    let view = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .unwrap()
    });
    view.update(vcx, |nb, cx| {
        nb.opened_from(
            dir.path()
                .join("shared.sqllynb")
                .to_string_lossy()
                .into_owned(),
            cx,
        );
    });
    assert!(view.read_with(vcx, |nb, _| nb.trust.scope.is_none()));
    draw(&view, vcx);
    vcx.run_until_parked();
    view.read_with(vcx, |nb, _| {
        assert!(nb.local.images.is_empty());
        assert!(nb.local.image_loads.is_empty(), "nothing was even tried");
    });
}
