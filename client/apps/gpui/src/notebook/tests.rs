//! Windowed tests for the notebook shell: virtualization and lazy editors,
//! keyboard command mode, per-cell connection targets and read-only mode,
//! trust, remapping, the run queue, autosave and conflicts.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Focusable as _, ParentElement as _, Styled as _, TestAppContext,
    VisualTestContext, px, size,
};
use sqlly_core::ConnectionEnvironment;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::QueryEvent;

use super::connection::ConnectionChoice;
use super::view::{
    CellStatus, DispatchOutcome, MAX_LIVE_EDITORS, NotebookEvent, NotebookOpening, NotebookView,
};
use crate::editor::SqlEditor;
use crate::results::ResultsView;
use sqlly_services::notebook::{Cell, CellKind, ConnectionRef, ExecutionPolicy, NotebookDoc};

const PROD_ID: &str = "6b1f3c9e-6a55-4b7e-9b0b-3f8f2d1c0a11";
const DEV_ID: &str = "0c3d2b1a-5f4e-4d3c-8b2a-1f0e9d8c7b6a";

fn choice(id: &str, label: &str, environment: ConnectionEnvironment) -> ConnectionChoice {
    ConnectionChoice {
        profile_id: id.to_string(),
        label: label.to_string(),
        environment,
        database_type: DatabaseType::SqlServer,
        database: None,
        ..ConnectionChoice::default()
    }
}

fn local_choices() -> Rc<dyn Fn() -> Vec<ConnectionChoice>> {
    Rc::new(|| {
        vec![
            choice(DEV_ID, "Dev", ConnectionEnvironment::Dev),
            choice(PROD_ID, "Sales", ConnectionEnvironment::Production),
        ]
    })
}

/// Points autosave at a temp dir for the test's lifetime.
struct AutosaveRoot(#[allow(dead_code)] tempfile::TempDir);

impl AutosaveRoot {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        super::autosave::set_test_root(Some(dir.path().to_path_buf()));
        Self(dir)
    }
}

impl Drop for AutosaveRoot {
    fn drop(&mut self) {
        super::autosave::set_test_root(None);
    }
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
        crate::editor::register_keybindings(cx);
        crate::keybindings::register_app_keybindings_from(
            &crate::keybindings::KeybindingSettings::default(),
            cx,
        );
    });
}

fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
    opening: NotebookOpening,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    init(cx);
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| NotebookView::with_choices(model, opening, local_choices(), cx));
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
        size(px(1400.0), px(900.0)),
        move |_, _| gpui::div().size_full().child(view.clone()),
    );
}

/// Every event the view emits, recorded.
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

/// Act as the host: answer every editor request with plain entities.
fn serve_entities(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let target = view.downgrade();
    cx.update(|_, cx| {
        cx.subscribe(view, move |_, event: &NotebookEvent, cx| {
            if let NotebookEvent::NeedCellEntities { cell_ids } = event {
                let ids = cell_ids.clone();
                let target = target.clone();
                cx.defer(move |cx| {
                    let pairs: Vec<_> = ids
                        .into_iter()
                        .map(|id| (id, cx.new(SqlEditor::new), cx.new(ResultsView::new)))
                        .collect();
                    target
                        .update(cx, |nb, cx| nb.attach_cell_entities(pairs, cx))
                        .ok();
                });
            }
        })
        .detach();
    });
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

fn push_cell(nb: &mut NotebookDoc, kind: CellKind, source: String) {
    let id = format!("cell_{}", nb.cells.len() + 1);
    let mut cell = Cell::new(id, kind, ExecutionPolicy::default());
    cell.source = source;
    nb.cells.push(cell);
}

fn mixed(n: usize) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nbmixed", "Mixed");
    for i in 0..n {
        if i % 5 == 4 {
            push_cell(
                &mut nb,
                CellKind::Markdown,
                format!("## Section {i}\n\nNotes for section {i}."),
            );
        } else {
            push_cell(
                &mut nb,
                CellKind::Sql,
                format!(
                    "SELECT o.OrderID, o.CustomerID\nFROM dbo.Orders AS o\nWHERE o.OrderID > {i}\nORDER BY o.OrderID;"
                ),
            );
        }
    }
    nb
}

fn sql_cells(sources: &[&str]) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nbtest", "Test");
    for source in sources {
        push_cell(&mut nb, CellKind::Sql, source.to_string());
    }
    nb
}

fn slot(id: &str, profile: Option<&str>, label: &str, database: Option<&str>) -> ConnectionRef {
    ConnectionRef {
        id: id.into(),
        profile_id: profile.map(str::to_string),
        label: label.into(),
        database: database.map(str::to_string),
        extra: Default::default(),
    }
}

/// Record `nb` in this test's trust store as created on this machine.
fn trust_here(nb: &NotebookDoc) {
    super::trust::record_created(nb, None);
}

/// Tick every connection in the trust dialog and confirm it.
fn grant_all(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    view.update(cx, |nb, cx| {
        nb.trust_notebook(cx);
        nb.confirm_grant(cx);
    });
}

#[gpui::test]
fn five_hundred_cells_render_previews_for_the_viewport_only(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, mixed(500), NotebookOpening::New);
    let events = record(&view, cx);
    draw(&view, cx);
    draw(&view, cx);
    let (live, previews) = view.read_with(cx, |nb, _| (nb.live_editor_count(), nb.preview_count()));
    assert_eq!(live, 0, "no editor exists until a cell is edited");
    assert!(previews > 0, "visible SQL cells draw a highlighted preview");
    assert!(
        previews < 60,
        "only cells near the viewport render, got {previews}"
    );
    assert!(
        !events
            .borrow()
            .iter()
            .any(|e| matches!(e, NotebookEvent::NeedCellEntities { .. })),
        "rendering never asks the host for editors"
    );

    view.update(cx, |nb, cx| {
        nb.reveal_cell("cell_251", cx);
        nb.edit_cell("cell_251", cx);
    });
    draw(&view, cx);
    let requested: Vec<Vec<String>> = events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            NotebookEvent::NeedCellEntities { cell_ids } => Some(cell_ids.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(requested, vec![vec!["cell_251".to_string()]]);
}

#[gpui::test]
fn editors_are_released_beyond_the_budget_and_keep_their_text(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, mixed(60), NotebookOpening::New);
    serve_entities(&view, cx);
    for i in 1..=15 {
        let id = format!("cell_{i}");
        view.update(cx, |nb, cx| nb.edit_cell(&id, cx));
        cx.run_until_parked();
    }
    let (live, active_live) = view.read_with(cx, |nb, _| {
        (nb.live_editor_count(), nb.has_live_editor("cell_15"))
    });
    assert!(live <= MAX_LIVE_EDITORS, "{live} live editors");
    assert!(active_live, "the active cell keeps its editor");
    assert!(view.read_with(cx, |nb, _| !nb.has_live_editor("cell_1")));

    // An edit made in a live editor lands in the document, and survives the
    // editor being released and recreated.
    let editor = view.read_with(cx, |nb, _| nb.cell_entities("cell_15").unwrap().0);
    editor.update(cx, |e, cx| {
        e.set_text("SELECT 15 AS edited", cx);
        cx.emit(crate::editor::SqlEditorEvent::Change);
    });
    view.update(cx, |nb, cx| nb.edit_cell("cell_2", cx));
    cx.run_until_parked();
    view.read_with(cx, |nb, _| {
        assert_eq!(
            nb.doc().cell_by_id("cell_15").unwrap().source,
            "SELECT 15 AS edited"
        );
        assert!(nb.is_dirty());
    });
    view.update(cx, |nb, cx| nb.edit_cell("cell_15", cx));
    cx.run_until_parked();
    let editor = view.read_with(cx, |nb, _| nb.cell_entities("cell_15").unwrap().0);
    assert_eq!(
        editor.read_with(cx, |e, _| e.text().to_string()),
        "SELECT 15 AS edited"
    );
}

#[gpui::test]
fn command_mode_keys_select_insert_delete_and_convert(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        sql_cells(&["SELECT 1", "SELECT 2", "SELECT 3"]),
        NotebookOpening::New,
    );
    draw(&view, cx);
    cx.update(|window, cx| window.focus(&view.read(cx).focus_handle(cx), cx));
    cx.run_until_parked();
    assert!(cx.update(|window, cx| view.read(cx).in_command_mode(window)));

    cx.simulate_keystrokes("j j");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some("cell_3".into())
    );
    cx.simulate_keystrokes("k");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some("cell_2".into())
    );
    cx.simulate_keystrokes("down");
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some("cell_3".into())
    );
    cx.simulate_keystrokes("up");

    cx.simulate_keystrokes("b");
    let ids = |cx: &mut VisualTestContext| {
        view.read_with(cx, |nb, _| {
            nb.doc()
                .cells()
                .map(|c| c.id.to_string())
                .collect::<Vec<_>>()
        })
    };
    let after_b = ids(cx);
    assert_eq!(after_b.len(), 4);
    assert_eq!(
        [&after_b[0], &after_b[1], &after_b[3]],
        ["cell_1", "cell_2", "cell_3"],
        "B inserts below the selected cell"
    );
    let below = after_b[2].clone();
    cx.simulate_keystrokes("a");
    let after_a = ids(cx);
    assert_eq!(after_a.len(), 5);
    assert_eq!(after_a[3], below, "A inserts above the selected cell");
    let above = after_a[2].clone();
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some(above.clone())
    );

    cx.simulate_keystrokes("d d");
    assert_eq!(ids(cx), after_b);
    view.update(cx, |nb, cx| nb.undo_delete(cx));
    assert_eq!(ids(cx), after_a);

    cx.simulate_keystrokes("m");
    assert_eq!(
        view.read_with(cx, |nb, _| nb
            .doc()
            .cell_by_id(&above)
            .unwrap()
            .kind
            .to_string()),
        "markdown"
    );
    cx.simulate_keystrokes("y");
    assert_eq!(
        view.read_with(cx, |nb, _| nb
            .doc()
            .cell_by_id(&above)
            .unwrap()
            .kind
            .to_string()),
        "sql"
    );

    // Typing letters in a cell editor never reaches the command keys.
    serve_entities(&view, cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    draw(&view, cx);
    assert!(view.read_with(cx, |nb, _| nb.has_live_editor(&above)));
    assert!(!cx.update(|window, cx| view.read(cx).in_command_mode(window)));
    cx.simulate_input("jab");
    assert_eq!(ids(cx).len(), 5);
}

#[gpui::test]
fn escape_in_a_cell_editor_switches_to_command_mode(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        sql_cells(&["SELECT 1", "SELECT 2"]),
        NotebookOpening::New,
    );
    serve_entities(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell("cell_2", cx));
    cx.run_until_parked();
    draw(&view, cx);
    draw(&view, cx);
    assert!(!cx.update(|window, cx| view.read(cx).in_command_mode(window)));
    assert_eq!(
        cx.update(|window, cx| view.read(cx).focused_cell(window, cx)),
        Some("cell_2".to_string())
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.update(|window, cx| view.read(cx).in_command_mode(window)));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some("cell_2".into())
    );
}

#[gpui::test]
fn run_keys_run_the_cell_then_advance_or_insert(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        sql_cells(&["SELECT 1", "SELECT 2"]),
        NotebookOpening::New,
    );
    let events = record(&view, cx);
    draw(&view, cx);
    cx.update(|window, cx| window.focus(&view.read(cx).focus_handle(cx), cx));
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(run_cells(&events), ["cell_1"]);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some("cell_2".into())
    );
    view.update(cx, |nb, cx| {
        nb.dispatch_result("cell_1", DispatchOutcome::NotStarted("x".into()), cx)
    });

    cx.simulate_keystrokes("alt-enter");
    assert_eq!(run_cells(&events), ["cell_1", "cell_2"]);
    let ids: Vec<String> = view.read_with(cx, |nb, _| {
        nb.doc().cells().map(|c| c.id.to_string()).collect()
    });
    assert_eq!(ids.len(), 3);
    assert_eq!(&ids[..2], ["cell_1", "cell_2"]);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string)),
        Some(ids[2].clone()),
        "the new cell below is selected"
    );
}

#[gpui::test]
fn cells_route_to_their_own_connection_read_only_until_writes_are_allowed(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut model = sql_cells(&["SELECT 1", "UPDATE dbo.T SET a = 1 WHERE id = 7"]);
    model
        .connection_refs
        .push(slot("dev", Some(DEV_ID), "Dev", None));
    model
        .connection_refs
        .push(slot("prod", Some(PROD_ID), "Sales", None));
    model.cells[0].connection_ref = Some("prod".into());
    let (view, cx) = open(cx, model, NotebookOpening::New);
    view.update(cx, |nb, cx| {
        nb.set_tab_connection(Some(choice(DEV_ID, "Dev", ConnectionEnvironment::Dev)), cx)
    });

    let request = view
        .read_with(cx, |nb, _| nb.run_request("cell_1"))
        .unwrap();
    assert_eq!(request.sql, "SELECT 1");
    assert_eq!(
        request.target.profile_id,
        Some(uuid::Uuid::parse_str(PROD_ID).unwrap())
    );
    assert!(request.target.read_only);
    let (shown, own) = view.read_with(cx, |nb, _| nb.cell_connection("cell_1"));
    assert!(own);
    assert_eq!(
        shown.unwrap().environment,
        ConnectionEnvironment::Production
    );

    // The second cell follows the notebook's connection and is read-only,
    // so its write is refused before it reaches the database.
    let (shown, own) = view.read_with(cx, |nb, _| nb.cell_connection("cell_2"));
    assert!(!own);
    assert_eq!(shown.unwrap().profile_id, DEV_ID);
    assert!(
        view.read_with(cx, |nb, _| nb.run_request("cell_2"))
            .is_err()
    );

    view.update(cx, |nb, cx| nb.request_allow_writes("cell_2", cx));
    assert_eq!(
        view.read_with(cx, |nb, _| nb.elevation_prompt.clone()),
        Some("cell_2".into())
    );
    view.update(cx, |nb, cx| nb.confirm_allow_writes(cx));
    let request = view
        .read_with(cx, |nb, _| nb.run_request("cell_2"))
        .unwrap();
    assert_eq!(
        request.target.profile_id,
        Some(uuid::Uuid::parse_str(DEV_ID).unwrap()),
        "runs on the notebook's connection"
    );
    assert!(!request.target.read_only);

    view.update(cx, |nb, cx| {
        nb.set_cell_connection(
            "cell_2",
            Some(choice(PROD_ID, "Sales", ConnectionEnvironment::Production)),
            cx,
        )
    });
    let request = view
        .read_with(cx, |nb, _| nb.run_request("cell_2"))
        .unwrap();
    assert_eq!(
        request.target.profile_id,
        Some(uuid::Uuid::parse_str(PROD_ID).unwrap())
    );
    view.update(cx, |nb, cx| nb.make_read_only("cell_2", cx));
    assert!(
        view.read_with(cx, |nb, _| nb.run_request("cell_2"))
            .is_err()
    );
}

#[gpui::test]
fn notebooks_from_elsewhere_run_nothing_until_trusted(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    // A marker copied out of a notebook this machine saved under the old
    // scheme no longer means anything.
    let mut model = sql_cells(&["SELECT 1"]);
    model.extra.insert(
        "sqlly_origin".into(),
        serde_json::Value::String("3f2a9c1d0b8e7f65".into()),
    );
    let (view, cx) = open(cx, model, NotebookOpening::File);
    let events = record(&view, cx);
    draw(&view, cx);
    assert!(!view.read_with(cx, |nb, _| nb.is_trusted()));
    view.update(cx, |nb, cx| nb.run_all(cx));
    view.update(cx, |nb, cx| nb.request_run_from_host("cell_1", cx));
    assert!(
        run_cells(&events).is_empty(),
        "opening or running an untrusted notebook runs nothing"
    );
    view.update(cx, |nb, cx| nb.trust_notebook(cx));
    assert!(
        !view.read_with(cx, |nb, _| nb.is_trusted()),
        "Trust opens the dialog; nothing is trusted until it is confirmed"
    );
    draw(&view, cx);
    view.update(cx, |nb, cx| nb.confirm_grant(cx));
    view.update(cx, |nb, cx| nb.run_all(cx));
    assert_eq!(run_cells(&events), ["cell_1"]);

    // New notebooks carry no machine marker at all.
    let fresh = cx.new(|cx| {
        NotebookView::with_choices(
            sql_cells(&["SELECT 2"]),
            NotebookOpening::New,
            local_choices(),
            cx,
        )
    });
    let saved = fresh.read_with(cx, |nb, _| nb.model_for_save());
    assert!(!saved.extra.contains_key("sqlly_origin"));
    assert!(fresh.read_with(cx, |nb, _| nb.is_trusted()));
}

#[gpui::test]
fn trust_follows_in_app_saves_but_not_edits_made_outside(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.sqllynb");
    let model = sql_cells(&["SELECT 1"]);
    trust_here(&model);
    std::fs::write(&path, model.to_canonical_json()).unwrap();
    let (view, cx) = open(cx, model, NotebookOpening::File);
    view.update(cx, |nb, cx| nb.opened_from(path.display().to_string(), cx));
    assert!(view.read_with(cx, |nb, _| nb.is_trusted()));

    // Saved in the app: the new contents stay trusted.
    view.update(cx, |nb, cx| {
        nb.doc_mut_for_test().set_source("cell_1", "SELECT 2");
        nb.mark_dirty_for_test(cx);
    });
    let saved = view.read_with(cx, |nb, _| nb.model_for_save());
    std::fs::write(&path, saved.to_canonical_json()).unwrap();
    view.update(cx, |nb, cx| nb.saved_to(path.display().to_string(), cx));
    let reopened = cx.new(|cx| {
        NotebookView::with_choices(saved.clone(), NotebookOpening::File, local_choices(), cx)
    });
    assert!(reopened.read_with(cx, |nb, _| nb.is_trusted()));

    // Changed outside the app (a git pull, a sync folder): untrusted again.
    let mut pulled = saved.clone();
    pulled.cells[0].source = "DELETE FROM dbo.Orders".into();
    let (outside, cx) = open(cx, pulled, NotebookOpening::File);
    let events = record(&outside, cx);
    draw(&outside, cx);
    outside.read_with(cx, |nb, _| {
        assert!(!nb.is_trusted());
        assert_eq!(
            nb.live_trust().reason,
            sqlly_services::notebook_security::TrustReason::ChangedOutsideApp
        );
    });
    outside.update(cx, |nb, cx| nb.run_all(cx));
    assert!(run_cells(&events).is_empty());

    // Markdown prose is not executable: editing it outside keeps trust.
    let mut notes = saved;
    notes.cells.push(Cell::new(
        "cell_9",
        CellKind::Markdown,
        ExecutionPolicy::default(),
    ));
    let mut prose = notes.clone();
    trust_here(&notes);
    prose.cells[1].source = "Reviewed by finance.".into();
    let annotated =
        cx.new(|cx| NotebookView::with_choices(prose, NotebookOpening::File, local_choices(), cx));
    assert!(annotated.read_with(cx, |nb, _| nb.is_trusted()));
}

#[gpui::test]
fn grants_authorize_only_the_ticked_connections_and_audit_each_run(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut model = sql_cells(&["SELECT 'dev' AS secret_marker", "SELECT 'prod'"]);
    model.connection_refs = vec![
        slot("dev", Some(DEV_ID), "Dev", None),
        slot("prod", Some(PROD_ID), "Sales", None),
    ];
    model.cells[1].connection_ref = Some("prod".into());
    let (view, cx) = open(cx, model, NotebookOpening::File);
    let events = record(&view, cx);
    view.update(cx, |nb, cx| {
        nb.trust_notebook(cx);
        assert_eq!(
            nb.grant.as_ref().unwrap().connections.len(),
            2,
            "every connection starts ticked"
        );
        nb.set_grant_connection("prod", false, cx);
        nb.confirm_grant(cx);
    });
    view.read_with(cx, |nb, _| {
        let trust = nb.live_trust();
        assert_eq!(
            trust.connections.get("dev"),
            Some(&Some(DEV_ID.to_string()))
        );
        assert_eq!(trust.pending_connections, vec!["prod".to_string()]);
        assert!(!trust.allow_writes, "writes stay off unless ticked");
    });
    let results: Vec<Entity<ResultsView>> = (0..2)
        .map(|_| cx.update(|_, cx| cx.new(ResultsView::new)))
        .collect();
    view.update(cx, |nb, cx| {
        nb.attach_results("cell_1", results[0].clone(), cx);
        nb.attach_results("cell_2", results[1].clone(), cx);
        nb.run_all(cx);
    });
    assert_eq!(run_cells(&events), ["cell_1"]);
    results[0].update(cx, |rv, cx| {
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx)
    });
    assert_eq!(
        run_cells(&events),
        ["cell_1"],
        "the unticked connection's cell is never handed to the host"
    );
    draw(&view, cx);
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.cell_state("cell_2"), CellStatus::Failed);
        let notice = nb.trust_notice.clone().expect("the banner explains");
        assert!(notice.contains("Sales"), "{notice}");
        assert!(nb.run_request("cell_2").is_err());
    });

    let log = super::trust::recent_audit(10);
    let lines: Vec<String> = log.iter().map(|e| e.to_json_line()).collect();
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("secret_marker") && !l.contains("SELECT")),
        "the audit log keeps query hashes, never SQL: {lines:?}"
    );
    use sqlly_services::notebook_security::audit::AuditOutcome;
    use sqlly_services::notebook_security::{AuditAction, AuditDecision};
    let cell_1: Vec<_> = log
        .iter()
        .filter(|e| e.cell_id.as_deref() == Some("cell_1"))
        .collect();
    assert_eq!(
        cell_1.len(),
        2,
        "one event when it starts, one when it ends"
    );
    assert!(cell_1.iter().all(|e| e.query_hash.is_some()
        && e.profile_id.as_deref() == Some(DEV_ID)
        && e.decision == AuditDecision::Allow));
    assert!(
        cell_1
            .iter()
            .any(|e| e.outcome == Some(AuditOutcome::Completed) && e.duration_ms.is_some())
    );
    let denied = log
        .iter()
        .find(|e| e.cell_id.as_deref() == Some("cell_2"))
        .expect("the refusal is audited");
    assert_eq!(denied.decision, AuditDecision::NeedsApproval);
    assert!(
        log.iter()
            .any(|e| e.action == AuditAction::TrustGranted && e.cell_id.is_none())
    );
}

#[gpui::test]
fn a_damaged_trust_file_is_reported_and_left_alone(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let file = super::trust::trust_file();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "{\"version\": 1, \"notebooks\": [").unwrap();
    let (view, cx) = open(cx, sql_cells(&["SELECT 1"]), NotebookOpening::File);
    draw(&view, cx);
    view.read_with(cx, |nb, _| {
        assert!(!nb.is_trusted());
        assert!(nb.trust_problem.is_some(), "the banner says why");
    });
    grant_all(&view, cx);
    assert!(
        view.read_with(cx, |nb, _| nb.is_trusted()),
        "trust still holds for this session"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "{\"version\": 1, \"notebooks\": ["
    );
}

#[gpui::test]
fn missing_connections_are_remapped_to_local_profiles(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut model = sql_cells(&["SELECT 1", "SELECT 2"]);
    let foreign = slot(
        "wh",
        Some("11111111-2222-3333-4444-555555555555"),
        "Their warehouse",
        Some("Northwind"),
    );
    model.connection_refs.push(foreign.clone());
    model.cells[1].connection_ref = Some("wh".into());
    trust_here(&model);
    let (view, cx) = open(cx, model, NotebookOpening::File);
    draw(&view, cx);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.unresolved.clone()),
        vec![foreign.clone()]
    );
    let err = view
        .read_with(cx, |nb, _| nb.run_request("cell_1"))
        .unwrap_err();
    assert!(err.contains("Their warehouse"), "{err}");

    view.update(cx, |nb, cx| {
        nb.remap_connection(
            &foreign.id,
            choice(DEV_ID, "Dev", ConnectionEnvironment::Dev),
            cx,
        )
    });
    view.read_with(cx, |nb, _| {
        assert!(nb.unresolved.is_empty());
        let r = nb.doc().effective_connection("cell_2").unwrap();
        assert_eq!(r.profile_id.as_deref(), Some(DEV_ID));
        assert_eq!(
            r.database.as_deref(),
            Some("Northwind"),
            "the database name carries over"
        );
        assert!(nb.is_dirty());
    });
    // The trust covered the old mapping: the remapped slot waits for a new
    // grant, in this session and after a save.
    let err = view
        .read_with(cx, |nb, _| nb.run_request("cell_1"))
        .unwrap_err();
    assert!(
        err.contains("isn't allowed") && err.contains("Dev"),
        "{err}"
    );
    view.update(cx, |nb, cx| nb.autosave_now(cx));
    let saved = view.read_with(cx, |nb, _| nb.model_for_save());
    assert!(
        !super::trust::assess(&saved, None)
            .decision
            .authorizes("wh", Some(DEV_ID))
    );
    draw(&view, cx);
    assert!(view.read_with(cx, |nb, _| nb.live_trust().needs_banner()));
    grant_all(&view, cx);
    let request = view
        .read_with(cx, |nb, _| nb.run_request("cell_1"))
        .unwrap();
    assert_eq!(
        request.target.profile_id,
        Some(uuid::Uuid::parse_str(DEV_ID).unwrap())
    );
    assert!(!view.read_with(cx, |nb, _| nb.live_trust().needs_banner()));
}

#[gpui::test]
fn run_all_advances_on_completion_and_blocks_dependents_on_failure(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        sql_cells(&[
            "SELECT 1 AS x",
            "SELECT x FROM {{cell_1}}",
            "SELECT 3",
            "SELECT x FROM {{cell_2}}",
        ]),
        NotebookOpening::New,
    );
    let events = record(&view, cx);
    let results: Vec<Entity<ResultsView>> = (0..4)
        .map(|_| cx.update(|_, cx| cx.new(ResultsView::new)))
        .collect();
    view.update(cx, |nb, cx| {
        for (i, r) in results.iter().enumerate() {
            nb.attach_results(&format!("cell_{}", i + 1), r.clone(), cx);
        }
        nb.run_all(cx);
    });
    assert_eq!(
        run_cells(&events),
        ["cell_1", "cell_3"],
        "independent cells start together; dependents wait"
    );
    assert!(view.read_with(cx, |nb, _| nb.is_dispatching("cell_1")));
    assert!(!view.read_with(cx, |nb, _| nb.is_dispatching("cell_2")));
    results[0].update(cx, |rv, cx| {
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx)
    });
    assert_eq!(run_cells(&events), ["cell_1", "cell_3", "cell_2"]);
    assert_eq!(
        view.read_with(cx, |nb, _| nb.cell_state("cell_1")),
        CellStatus::Fresh
    );
    results[1].update(cx, |rv, cx| {
        rv.handle_event(
            QueryEvent::Error {
                message: "boom".into(),
            },
            cx,
        )
    });
    assert_eq!(
        run_cells(&events),
        ["cell_1", "cell_3", "cell_2"],
        "a failure starts nothing that depended on it"
    );
    assert!(
        view.read_with(cx, |nb, _| nb.is_running()),
        "the independent cell is still running"
    );
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.cell_state("cell_2"), CellStatus::Failed);
        assert_eq!(
            nb.cell_state("cell_4"),
            CellStatus::Blocked,
            "a dependent of the failed cell is blocked"
        );
    });
    results[2].update(cx, |rv, cx| {
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 1 }, cx)
    });
    assert_eq!(run_cells(&events), ["cell_1", "cell_3", "cell_2"]);
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));

    // Editing an upstream cell makes its dependents' results stale; Run
    // Stale picks up exactly the cells that are not fresh.
    assert!(!view.read_with(cx, |nb, _| nb.is_stale("cell_1")));
    view.update(cx, |nb, cx| {
        nb.doc_mut_for_test().set_source("cell_1", "SELECT 2 AS x");
        nb.run_scope(super::doc_adapter::RunScope::Stale, None, cx)
    });
    assert_eq!(
        view.read_with(cx, |nb, _| nb.cell_state("cell_1")),
        CellStatus::Queued,
        "the edited cell is out of date, so Run Stale picks it up"
    );
    assert_eq!(
        run_cells(&events).last().map(String::as_str),
        Some("cell_1")
    );
}

#[gpui::test]
fn a_cell_that_never_started_fails_without_canceling_the_rest(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(
        cx,
        sql_cells(&["SELECT 1", "SELECT 2"]),
        NotebookOpening::New,
    );
    let events = record(&view, cx);
    view.update(cx, |nb, cx| nb.run_all(cx));
    view.update(cx, |nb, cx| {
        nb.dispatch_result(
            "cell_1",
            DispatchOutcome::NotStarted("No connection".into()),
            cx,
        )
    });
    assert_eq!(run_cells(&events), ["cell_1", "cell_2"]);
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.cell_state("cell_1"), CellStatus::Failed);
        assert_eq!(nb.cell_state("cell_2"), CellStatus::Queued);
        assert!(nb.is_running());
    });
    view.update(cx, |nb, cx| {
        nb.dispatch_result(
            "cell_2",
            DispatchOutcome::NotStarted("No connection".into()),
            cx,
        )
    });
    assert!(!view.read_with(cx, |nb, _| nb.is_running()));
    view.update(cx, |nb, cx| nb.run_all(cx));
    view.update(cx, |nb, cx| {
        nb.dispatch_result("cell_1", DispatchOutcome::AwaitingApproval, cx)
    });
    assert_eq!(
        view.read_with(cx, |nb, _| nb.cell_state("cell_1")),
        CellStatus::AwaitingApproval
    );
    view.update(cx, |nb, cx| nb.stop(cx));
    assert!(
        events
            .borrow()
            .iter()
            .any(|e| matches!(e, NotebookEvent::StopRequested))
    );
    assert_eq!(
        view.read_with(cx, |nb, _| nb.cell_state("cell_1")),
        CellStatus::Canceled
    );
}

#[gpui::test]
fn unsaved_notebooks_autosave_a_recovery_copy_and_reopen_from_it(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, sql_cells(&["SELECT 1"]), NotebookOpening::New);
    let id = view.read_with(cx, |nb, _| nb.doc().id().to_string());
    assert!(
        super::autosave::load_recovery(&id).is_some(),
        "a new notebook is recoverable at once"
    );
    serve_entities(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell("cell_1", cx));
    cx.run_until_parked();
    let editor = view.read_with(cx, |nb, _| nb.cell_entities("cell_1").unwrap().0);
    editor.update(cx, |e, cx| {
        e.set_text("SELECT 'recovered' AS state", cx);
        cx.emit(crate::editor::SqlEditorEvent::Change);
    });
    cx.executor()
        .advance_clock(super::autosave::AUTOSAVE_DEBOUNCE * 2);
    cx.run_until_parked();
    let recovered = super::autosave::load_recovery(&id).expect("recovery copy");
    assert_eq!(recovered.cells[0].source, "SELECT 'recovered' AS state");

    let reopened = cx.new(|cx| {
        NotebookView::with_choices(recovered, NotebookOpening::Recovered, local_choices(), cx)
    });
    reopened.read_with(cx, |nb, _| {
        assert!(
            nb.is_trusted(),
            "this machine's own recovery copy is trusted"
        );
        assert_eq!(nb.save_state, super::view::SaveState::Recovered);
    });

    // Saving to a file retires the recovery copy.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kept.sqlnb.json");
    std::fs::write(
        &path,
        view.read_with(cx, |nb, _| nb.model().to_canonical_json()),
    )
    .unwrap();
    view.update(cx, |nb, cx| nb.saved_to(path.display().to_string(), cx));
    assert!(super::autosave::load_recovery(&id).is_none());
}

#[gpui::test]
fn saved_files_autosave_only_when_enabled_and_never_over_a_changed_file(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.sqllynb");
    let model = sql_cells(&["SELECT 1"]);
    trust_here(&model);
    std::fs::write(&path, model.to_canonical_json()).unwrap();
    let (view, cx) = open(cx, model, NotebookOpening::File);
    let events = record(&view, cx);
    view.update(cx, |nb, cx| nb.opened_from(path.display().to_string(), cx));

    view.update(cx, |nb, cx| {
        nb.doc_mut_for_test().set_source("cell_1", "SELECT 2");
        nb.mark_dirty_for_test(cx);
        nb.autosave_now(cx);
    });
    assert!(
        std::fs::read_to_string(&path).unwrap().contains("SELECT 1"),
        "file autosave is off by default"
    );

    view.update(cx, |nb, cx| {
        nb.set_autosave_files(true, cx);
        nb.autosave_now(cx);
    });
    assert!(std::fs::read_to_string(&path).unwrap().contains("SELECT 2"));
    assert!(
        events
            .borrow()
            .iter()
            .any(|e| matches!(e, NotebookEvent::SavedToFile))
    );

    // Someone else rewrites the file: the notebook flags the conflict and
    // stops autosaving over it until the user decides.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        &path,
        sql_cells(&["SELECT 'theirs'"]).to_canonical_json() + "\n\n",
    )
    .unwrap();
    view.update(cx, |nb, cx| {
        nb.check_disk(cx);
        nb.doc_mut_for_test().set_source("cell_1", "SELECT 3");
        nb.mark_dirty_for_test(cx);
        nb.autosave_now(cx);
    });
    assert!(view.read_with(cx, |nb, _| nb.has_conflict()));
    assert!(std::fs::read_to_string(&path).unwrap().contains("theirs"));
    view.update(cx, |nb, cx| {
        nb.keep_mine(cx);
        nb.autosave_now(cx);
    });
    assert!(!view.read_with(cx, |nb, _| nb.has_conflict()));
    assert!(std::fs::read_to_string(&path).unwrap().contains("SELECT 3"));
    view.update(cx, |nb, cx| nb.set_autosave_files(false, cx));
}

#[gpui::test]
fn unknown_cell_kinds_render_read_only_and_round_trip(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let json = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"nbtest","title":"t",
        "cells":[
            {"id":"cell_1","kind":"sql","source":"SELECT 1"},
            {"id":"cell_2","kind":"heatmap_3d","source":"","spec":{"type":"bar","x":"region"}}
        ],"layout":{"cols":12}}"#;
    let model = super::files::load_file("/tmp/t.sqllynb", json, None)
        .unwrap()
        .doc;
    let (view, cx) = open(cx, model, NotebookOpening::New);
    draw(&view, cx);
    view.update(cx, |nb, cx| nb.edit_cell("cell_2", cx));
    assert!(!view.read_with(cx, |nb, _| nb.has_live_editor("cell_2")));
    let saved: serde_json::Value = serde_json::from_str(
        &view
            .update(cx, |nb, _| nb.model_for_save())
            .to_canonical_json(),
    )
    .unwrap();
    assert_eq!(saved["cells"][1]["kind"], "heatmap_3d");
    assert_eq!(saved["cells"][1]["spec"]["x"], "region");
    assert_eq!(saved["layout"]["cols"], 12);
}

#[gpui::test]
fn layout_changes_are_reported_for_persistence(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, sql_cells(&["SELECT 1"]), NotebookOpening::New);
    let events = record(&view, cx);
    draw(&view, cx);
    view.update(cx, |nb, cx| {
        nb.update_layout(
            |l| {
                l.left_width = 300.0;
                l.activity_visible = true;
            },
            cx,
        )
    });
    draw(&view, cx);
    let reported = events.borrow().iter().find_map(|e| match e {
        NotebookEvent::LayoutChanged(layout) => Some(layout.clone()),
        _ => None,
    });
    let reported = reported.expect("layout change reported");
    assert_eq!(reported.left_width, 300.0);
    assert!(reported.activity_visible);
    assert_eq!(view.read_with(cx, |nb, _| nb.layout().clone()), reported);
}

#[test]
fn docs_shot_fixture_notebooks_open() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/docs-shots/fixtures/notebooks");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("fixture folder") {
        let path = entry.unwrap().path();
        let params_fixture = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("params-"));
        if params_fixture || !super::files::is_notebook_path(&path.to_string_lossy()) {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("{P_DEV}", DEV_ID)
            .replace("{P_PROD}", PROD_ID);
        let loaded = super::files::load_file(&path.to_string_lossy(), &text, None)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(loaded.migrated_from, None, "{} is current", path.display());
        let nb = loaded.doc;
        assert!(!nb.cells.is_empty(), "{}", path.display());
        let mut doc = super::doc_adapter::ShellDoc::new(nb);
        let local = |id: &str, db| ConnectionChoice {
            profile_id: id.into(),
            label: id.into(),
            environment: ConnectionEnvironment::Dev,
            database_type: db,
            database: None,
            ..ConnectionChoice::default()
        };
        doc.set_connections(
            Some(DatabaseType::SqlServer),
            None,
            vec![
                local(DEV_ID, DatabaseType::SqlServer),
                local(PROD_ID, DatabaseType::SqlServer),
            ],
        );
        let ids: Vec<String> = doc
            .cells()
            .filter(|c| c.is_sql())
            .map(|c| c.id.to_string())
            .collect();
        for id in ids {
            if !doc.writes(&id) && doc.unresolved_connections().is_empty() {
                doc.sql_for_run(&id)
                    .unwrap_or_else(|e| panic!("{} {id}: {e}", path.display()));
            }
        }
        seen += 1;
    }
    assert!(seen >= 9, "{seen} fixtures");
}

#[gpui::test]
fn quitting_writes_the_edits_the_autosave_debounce_has_not_yet(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, sql_cells(&["SELECT 1"]), NotebookOpening::New);
    let id = view.read_with(cx, |nb, _| nb.doc().id().to_string());
    view.update(cx, |nb, cx| {
        nb.doc_mut_for_test()
            .set_source("cell_1", "SELECT 'typed just now'");
        nb.mark_dirty_for_test(cx);
        nb.flush_for_quit(cx);
    });
    assert_eq!(
        super::autosave::load_recovery(&id).unwrap().cells[0].source,
        "SELECT 'typed just now'",
        "no waiting for the debounce at quit"
    );

    // A recovery copy older than its file is never offered.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("older.sqllynb");
    let model = sql_cells(&["SELECT 'copy'"]);
    super::autosave::write_file_recovery(&model).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, model.to_canonical_json()).unwrap();
    assert_eq!(
        super::autosave::file_recovery(&model.notebook_id, &path.to_string_lossy()),
        super::autosave::FileRecovery::None
    );
    assert_eq!(
        super::autosave::file_recovery(&model.notebook_id, &path.to_string_lossy()),
        super::autosave::FileRecovery::None,
        "and it is deleted"
    );
}

#[gpui::test]
fn file_autosave_never_rewrites_a_legacy_notebook_file(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.sqlnb.json");
    let legacy = "{\"version\":1,\"cells\":[{\"id\":\"cell_1\",\"kind\":\"sql\",\"source\":\"SELECT 1\"}]}\n";
    std::fs::write(&path, legacy).unwrap();
    let model = sql_cells(&["SELECT 1"]);
    trust_here(&model);
    let (view, cx) = open(cx, model, NotebookOpening::File);
    view.update(cx, |nb, cx| {
        nb.opened_from(path.display().to_string(), cx);
        nb.set_autosave_files(true, cx);
        nb.doc_mut_for_test().set_source("cell_1", "SELECT 2");
        nb.mark_dirty_for_test(cx);
        nb.autosave_now(cx);
        nb.set_autosave_files(false, cx);
    });
    assert_eq!(
        std::fs::read_to_string(&path)
            .unwrap()
            .replace("\r\n", "\n"),
        legacy
    );
}

#[test]
fn canonical_json_sorts_keys_inside_parameter_choices_and_bounds() {
    // The app's build keeps JSON objects in insertion order, so every
    // free-form value has to be sorted when written.
    let mut nb = sql_cells(&["SELECT 1"]);
    let mut def = sqlly_services::notebook::ParameterDef::new(
        "region",
        sqlly_services::notebook::ParamType::Enum,
    );
    def.allowed_values = Some(sqlly_services::notebook::params::AllowedValues::Static(
        vec![serde_json::json!({"value": "w", "label": "West"})],
    ));
    def.validation.min = Some(serde_json::json!({"z": 1, "a": 2}));
    nb.parameters.push(def);
    let text = nb.to_canonical_json();
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {text}"))
    };
    assert!(at("\"label\"") < at("\"value\""), "{text}");
    assert!(at("\"a\": 2") < at("\"z\": 1"), "{text}");
}

#[gpui::test]
fn a_browser_notebook_changed_in_another_tab_is_not_overwritten(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, sql_cells(&["SELECT 1"]), NotebookOpening::New);
    let path = super::browser::library_path("nb_browser_guard", "report.sqllynb");
    let name = super::browser::store_name(&path).unwrap().to_string();
    let store = super::browser::store();
    store.write(&name, "{\"mine\": true}\n").unwrap();
    view.update(cx, |nb, cx| nb.saved_to(path.clone(), cx));
    assert!(view.update(cx, |nb, cx| nb.may_overwrite_file(cx)));

    std::thread::sleep(std::time::Duration::from_millis(5));
    store
        .write(&name, "{\"from another browser tab\": true}\n")
        .unwrap();
    assert!(
        !view.update(cx, |nb, cx| nb.may_overwrite_file(cx)),
        "Save in the browser waits for Reload or Keep mine"
    );
    view.update(cx, |nb, cx| nb.keep_mine(cx));
    assert!(view.update(cx, |nb, cx| nb.may_overwrite_file(cx)));
}

#[gpui::test]
fn a_copied_notebook_file_takes_its_own_id_on_first_save(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("report.sqllynb");
    let copy = dir.path().join("report copy.sqllynb");
    let model = sql_cells(&["SELECT 1"]);
    let id = model.notebook_id.clone();
    super::trust::record_created(&model, Some(&original));
    std::fs::write(&original, model.to_canonical_json()).unwrap();
    std::fs::write(&copy, model.to_canonical_json()).unwrap();

    let (view, cx) = open(cx, model.clone(), NotebookOpening::File);
    let events = record(&view, cx);
    let copy_path = copy.to_string_lossy().to_string();
    view.update(cx, |nb, cx| nb.opened_from(copy_path.clone(), cx));
    view.update(cx, |nb, cx| {
        nb.take_own_identity(cx);
        nb.saved_to(copy_path.clone(), cx);
    });
    let new_id = view.read_with(cx, |nb, _| nb.doc().id().to_string());
    assert_ne!(new_id, id, "the copy gets its own id");
    let told = |events: &Rc<RefCell<Vec<NotebookEvent>>>| {
        events
            .borrow()
            .iter()
            .filter(|e| matches!(e, NotebookEvent::Status { .. }))
            .count()
    };
    assert_eq!(told(&events), 1, "the user is told");
    view.update(cx, |nb, cx| nb.take_own_identity(cx));
    assert_eq!(told(&events), 1, "once");

    let opened_original = super::trust::assess(&model, Some(&original)).decision;
    assert_ne!(
        opened_original.level,
        sqlly_services::notebook_security::TrustLevel::Untrusted,
        "the original keeps its trust record"
    );
    let copy_scope = view.read_with(cx, |nb, _| nb.live_trust().scope);
    assert_ne!(
        copy_scope, opened_original.scope,
        "the copy keeps its history and local data apart"
    );
    assert!(
        !super::trust::belongs_elsewhere(&model, &original),
        "the original is not a copy"
    );
}

#[gpui::test]
fn scan_warnings_show_on_open_and_in_the_activity_panel_until_dismissed(cx: &mut TestAppContext) {
    let (view, cx) = open(cx, sql_cells(&["SELECT 1"]), NotebookOpening::File);
    let text = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","settings":{"run_mode":"reactive"},"cells":[]}"#;
    let warnings = super::files::scan_warnings(text);
    assert!(!warnings.is_empty());
    view.update(cx, |nb, cx| nb.show_scan_warnings(warnings.clone(), cx));
    draw(&view, cx);
    view.read_with(cx, |nb, _| {
        assert_eq!(nb.scan_warnings, warnings);
        assert!(
            nb.activity
                .iter()
                .any(|e| !e.error && e.message.contains(&warnings[0])),
            "the activity panel keeps the warning"
        );
    });
    view.update(cx, |nb, cx| {
        nb.scan_warnings.clear();
        cx.notify();
    });
    draw(&view, cx);
    assert!(view.read_with(cx, |nb, _| nb.scan_warnings.is_empty()));
}

/// Count messages pick their one-form at exactly 1 ("1 test failing",
/// "1 such cell"), and every one-form in every catalog sits next to the
/// form it replaces with the same placeholders.
#[test]
fn count_messages_read_right_at_one() {
    use sqlly_services::notebook::cells::profile::Coverage;
    use sqlly_services::notebook_security::authorize::Reason;
    assert_eq!(
        super::cells::profile::coverage_text(Coverage::Complete { rows: 1 }),
        "Profiled 1 row. Every figure is exact."
    );
    assert_eq!(
        super::cells::profile::coverage_text(Coverage::Complete { rows: 2 }),
        "Profiled all 2 rows. Every figure is exact."
    );
    let publish =
        |count| crate::localization::message(Reason::PublishHasWriteCells { count }.message());
    assert!(publish(1).ends_with("(1 such cell)."), "{}", publish(1));
    assert!(publish(3).ends_with("(3 such cells)."), "{}", publish(3));
    let caps = |rows_kept, by_bytes| {
        super::local::caps_message(super::local::Truncation {
            rows_kept,
            by_bytes,
        })
    };
    assert!(caps(1, true).starts_with("Stopped after 1 row at"));
    assert!(caps(5, true).starts_with("Stopped after 5 rows at"));

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/sqlly-i18n/translations");
    let placeholders = |text: &str| -> std::collections::BTreeSet<String> {
        text.split('{')
            .skip(1)
            .filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_string()))
            .collect()
    };
    fn leaves(value: &serde_json::Value, prefix: &str, out: &mut Vec<(String, String)>) {
        if let Some(text) = value.get("value").and_then(|v| v.as_str()) {
            out.push((prefix.to_string(), text.to_string()));
        }
        if let Some(map) = value.as_object() {
            for (key, child) in map {
                if child.is_object() {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    leaves(child, &path, out);
                }
            }
        }
    }
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut all = Vec::new();
        leaves(&catalog, "", &mut all);
        let by_key: std::collections::BTreeMap<_, _> = all.into_iter().collect();
        for (key, text) in by_key.iter().filter(|(k, _)| k.starts_with("notebook.")) {
            let Some(base) = key.strip_suffix("_one") else {
                continue;
            };
            let Some(other) = by_key.get(base) else {
                continue;
            };
            assert_eq!(
                placeholders(text),
                placeholders(other),
                "{}: {key}",
                path.display()
            );
        }
    }
}

// ── Scrolling a cell into view ───────────────────────────────────────────

/// Where cell `id` sits in the list: its top and bottom measured from the
/// viewport's top, and the viewport's height.
fn placement(view: &Entity<NotebookView>, cx: &mut VisualTestContext, id: &str) -> (f32, f32, f32) {
    view.read_with(cx, |nb, _| {
        let index = nb.doc.index_of(id).expect("the cell exists");
        let viewport = nb.list_state.viewport_bounds();
        let bounds = nb
            .list_state
            .bounds_for_item(index)
            .expect("the cell is measured and not above the viewport");
        (
            f32::from(bounds.top() - viewport.top()),
            f32::from(bounds.bottom() - viewport.top()),
            f32::from(viewport.size.height),
        )
    })
}

fn settle(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    for _ in 0..4 {
        draw(view, cx);
    }
}

fn assert_fully_visible(view: &Entity<NotebookView>, cx: &mut VisualTestContext, id: &str) {
    let (top, bottom, viewport) = placement(view, cx, id);
    assert!(bottom - top > 1.0, "{id} has a height");
    assert!(
        top >= -0.5 && bottom <= viewport + 0.5,
        "{id} is fully in view: top {top}, bottom {bottom}, viewport {viewport}"
    );
}

#[gpui::test]
fn moving_to_a_cell_brings_all_of_it_into_view(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, mixed(200), NotebookOpening::New);
    settle(&view, cx);

    view.update(cx, |nb, cx| nb.reveal_cell("cell_120", cx));
    settle(&view, cx);
    assert_fully_visible(&view, cx, "cell_120");

    // A cell that is already in view stays where it is.
    let before = placement(&view, cx, "cell_120");
    view.update(cx, |nb, cx| nb.reveal_cell("cell_121", cx));
    settle(&view, cx);
    assert_fully_visible(&view, cx, "cell_121");
    assert_eq!(placement(&view, cx, "cell_120"), before);

    // Back up the notebook: the cell's top comes to the top.
    view.update(cx, |nb, cx| nb.reveal_cell("cell_40", cx));
    settle(&view, cx);
    assert_fully_visible(&view, cx, "cell_40");
    let (top, ..) = placement(&view, cx, "cell_40");
    assert!(top.abs() < 1.0, "a cell above lands at the top, got {top}");
}

#[gpui::test]
fn a_new_cell_at_the_end_is_shown_in_full(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, mixed(60), NotebookOpening::New);
    settle(&view, cx);
    view.update(cx, |nb, cx| nb.reveal_cell("cell_60", cx));
    settle(&view, cx);

    let id = view.update(cx, |nb, cx| {
        let end = nb.doc.len();
        nb.insert_cell(super::doc_adapter::KIND_SQL, end, cx)
    });
    view.update(cx, |nb, cx| nb.edit_cell(&id, cx));
    settle(&view, cx);
    assert_fully_visible(&view, cx, &id);
}

#[gpui::test]
fn a_cell_taller_than_the_window_shows_its_top(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut doc = mixed(20);
    let long: String = (0..150).map(|i| format!("Paragraph {i}.\n\n")).collect();
    push_cell(&mut doc, CellKind::Markdown, long);
    for i in 0..10 {
        push_cell(&mut doc, CellKind::Sql, format!("SELECT {i};"));
    }
    let (view, cx) = open(cx, doc, NotebookOpening::New);
    settle(&view, cx);

    // Arriving from above, the cell starts at the top instead of showing
    // its last screenful.
    view.update(cx, |nb, cx| nb.reveal_cell("cell_21", cx));
    settle(&view, cx);
    let (top, bottom, viewport) = placement(&view, cx, "cell_21");
    assert!(
        bottom - top > viewport,
        "the cell is taller than the window"
    );
    assert!(top.abs() < 1.0, "its top is at the top, got {top}");

    // Reading further down the cell is not undone by selecting it again…
    view.update(cx, |nb, cx| {
        nb.list_state.scroll_by(px(300.0));
        nb.reveal_cell("cell_21", cx);
    });
    settle(&view, cx);
    let (top, ..) = placement(&view, cx, "cell_21");
    assert!((top + 300.0).abs() < 1.0, "the page stayed put, got {top}");

    // …but editing it brings its start back.
    view.update(cx, |nb, cx| {
        nb.reveal("cell_21", super::reveal::Reveal::Edit, cx)
    });
    settle(&view, cx);
    let (top, ..) = placement(&view, cx, "cell_21");
    assert!(top.abs() < 1.0, "editing starts at the top, got {top}");

    // Arriving from below also lands on its start.
    view.update(cx, |nb, cx| nb.reveal_cell("cell_30", cx));
    settle(&view, cx);
    assert_fully_visible(&view, cx, "cell_30");
    view.update(cx, |nb, cx| nb.reveal_cell("cell_21", cx));
    settle(&view, cx);
    let (top, ..) = placement(&view, cx, "cell_21");
    assert!(
        top.abs() < 1.0,
        "from below its top is at the top, got {top}"
    );
}
