//! Launch-time behavior driven through the real `AppView::new`: each test
//! writes the session file the app reads on startup, opens the workbench, and
//! checks what the user gets.

use super::AppView;
use crate::workspace_session::{TabState, WorkspaceSession};
use gpui::{AppContext as _, TestAppContext};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Points the session file at a temp dir for this test thread and puts it
/// back when the test ends (pass or fail).
struct SessionFileGuard {
    _dir: tempfile::TempDir,
}

impl SessionFileGuard {
    fn with(session: &WorkspaceSession) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspace-session.json");
        crate::workspace_session::save_session(session, &path).expect("write session");
        crate::workspace_session::set_test_session_file(Some(path));
        Self { _dir: dir }
    }
}

impl Drop for SessionFileGuard {
    fn drop(&mut self) {
        crate::workspace_session::set_test_session_file(None);
    }
}

fn open_app(cx: &mut TestAppContext) -> (gpui::Entity<AppView>, &mut gpui::VisualTestContext) {
    crate::ensure_test_tokio_runtime();
    cx.dispatcher.allow_parking();
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
        // Keep the startup update check off the network.
        let status = crate::update_status::handle(cx);
        status.update(cx, |s, _| {
            s.state = crate::update_status::UpdateCheckState::Checking;
        });
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let services = sqlly_services::Services::new();
        let app = cx.new(|cx| AppView::new(services, window, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<AppView>()
            .expect("root hosts the AppView")
    });
    (app, vcx)
}

fn tab(title: &str) -> TabState {
    TabState {
        title: title.to_string(),
        user_named: true,
        auto_named: Some(false),
        ..TabState::default()
    }
}

/// A TCP server that accepts connections and never answers, counting how many
/// clients dialed it. Each accepted socket is held for `hold`, then dropped.
fn silent_server(hold: Duration) -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let dialed = Arc::new(AtomicUsize::new(0));
    let counter = dialed.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                std::thread::sleep(hold);
                drop(stream);
            });
        }
    });
    (port, dialed)
}

fn silent_profile(port: u16) -> sqlly_core::ConnectionProfile {
    let secret = sqlly_core::SecretRef {
        provider: sqlly_core::SecretProvider::InMemory,
        key: "k".to_string(),
    };
    let mut profile = sqlly_core::ConnectionProfile::sql_login("stuck", "127.0.0.1", "sa", secret);
    profile.port = port;
    profile
}

// https://github.com/microsoft/azuredatastudio/issues/26177
#[gpui::test]
fn restart_restores_saved_tabs_as_sql_editors_with_no_blank_extras(cx: &mut TestAppContext) {
    let files = tempfile::tempdir().expect("tempdir");
    let saved_sql = files.path().join("report.sql");
    std::fs::write(&saved_sql, "SELECT * FROM dbo.Report;").unwrap();

    let mut session = WorkspaceSession::new();
    session.tabs = vec![
        TabState {
            content: Some("SELECT 1;".into()),
            ..tab("Scratch")
        },
        TabState {
            file_path: Some(saved_sql.display().to_string()),
            ..tab("report.sql")
        },
        // A notebook whose file was deleted between sessions: it must not
        // come back as an empty editor.
        TabState {
            notebook_path: Some(files.path().join("gone.sqlnb.json").display().to_string()),
            ..tab("Gone notebook")
        },
        // Pinned to a connection that no longer exists: still a working SQL
        // editor with its text, just unpinned.
        TabState {
            content: Some("SELECT name FROM sys.tables;".into()),
            connection_id: Some(uuid::Uuid::new_v4().to_string()),
            ..tab("Orphaned pin")
        },
    ];
    let _guard = SessionFileGuard::with(&session);

    let (app, cx) = open_app(cx);
    cx.run_until_parked();
    let restored: Vec<(String, String, bool)> = cx.update(|_, cx| {
        app.read(cx)
            .tabs
            .iter()
            .map(|t| {
                (
                    t.name.clone(),
                    t.editor.read(cx).text().to_string(),
                    t.is_notebook() || t.ddl_view.is_some() || t.connection_target.is_some(),
                )
            })
            .collect()
    });
    assert_eq!(
        restored,
        vec![
            ("Scratch".to_string(), "SELECT 1;".to_string(), false),
            (
                "report.sql".to_string(),
                "SELECT * FROM dbo.Report;".to_string(),
                false
            ),
            (
                "Orphaned pin".to_string(),
                "SELECT name FROM sys.tables;".to_string(),
                false
            ),
        ],
        "exactly the saved SQL tabs, each with its own title and text"
    );
}

// https://github.com/dbeaver/dbeaver/issues/42803
#[gpui::test]
fn startup_restore_leaves_pinned_connections_idle(cx: &mut TestAppContext) {
    let (port, dialed) = silent_server(Duration::from_secs(30));
    let profile = silent_profile(port);
    let profile_id = profile.id.0;
    crate::connection::ConnectionStore::global().add_provisioned(vec![profile]);

    let mut session = WorkspaceSession::new();
    session.tabs = vec![
        TabState {
            content: Some("SELECT * FROM dbo.Orders;".into()),
            connection_id: Some(profile_id.to_string()),
            database: Some("sales".into()),
            ..tab("Orders")
        },
        TabState {
            content: Some("SELECT 2;".into()),
            connection_id: Some(profile_id.to_string()),
            ..tab("Second")
        },
    ];
    session.selected_tab = 0;
    let _guard = SessionFileGuard::with(&session);

    // The window opens while the server behind the pinned tab would hang
    // any connect: startup must not wait on it.
    let started = Instant::now();
    let (app, cx) = open_app(cx);
    cx.run_until_parked();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "startup waited on the pinned server ({:?})",
        started.elapsed()
    );
    // Moving between the restored tabs is not "using" them either.
    for index in [1usize, 0] {
        cx.update(|_, cx| app.update(cx, |this, cx| this.select_tab(index, cx)));
        cx.run_until_parked();
    }
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();

    cx.update(|_, cx| {
        let this = app.read(cx);
        assert_eq!(this.tabs.len(), 2);
        for tab in &this.tabs {
            assert_eq!(
                tab.connection_target.as_ref().map(|t| t.profile_id),
                Some(profile_id),
                "the tab keeps its pin"
            );
            assert!(tab.restored_connection_inert, "the pin stays idle");
        }
        assert!(
            this.active_profile.is_none(),
            "no connection was opened at startup"
        );
    });
    assert_eq!(
        dialed.load(Ordering::SeqCst),
        0,
        "startup dialed the pinned server"
    );
}

// https://github.com/pgadmin-org/pgadmin4/issues/10767
#[gpui::test]
fn running_against_a_stuck_server_keeps_the_window_responsive(cx: &mut TestAppContext) {
    // The server accepts the connection and then says nothing for 4 s.
    let hold = Duration::from_secs(4);
    let (port, dialed) = silent_server(hold);
    let _guard = SessionFileGuard::with(&WorkspaceSession::new());
    let (app, cx) = open_app(cx);
    cx.run_until_parked();

    cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            // Keep this run out of the on-disk query version history.
            this.query_history_prefs.enabled = false;
            this.new_tab(cx, Some("SELECT 1;".into()));
            this.active_profile = Some(silent_profile(port));
        })
    });

    // Run: the call that the Run button makes returns at once.
    let started = Instant::now();
    cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let index = this.active_tab;
            this.execute_query(index, cx);
        })
    });
    cx.run_until_parked();
    let run_returned = started.elapsed();
    assert!(
        run_returned < hold / 2,
        "Run blocked the UI thread for {run_returned:?} on a stuck server"
    );

    // The query is in flight on the stuck server ...
    let deadline = Instant::now() + Duration::from_secs(10);
    while dialed.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    assert!(dialed.load(Ordering::SeqCst) > 0, "the query never dialed");
    let running = cx.update(|_, cx| {
        let this = app.read(cx);
        this.tabs[this.active_tab].results.read(cx).is_running()
    });
    assert!(running, "the query should still be waiting on the server");

    // ... and the window still does work: open, switch and type in tabs.
    let started = Instant::now();
    cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            this.new_tab(cx, Some("SELECT 2;".into()));
            this.select_tab(0, cx);
            let last = this.tabs.len() - 1;
            this.select_tab(last, cx);
        })
    });
    cx.run_until_parked();
    assert!(
        started.elapsed() < hold / 2,
        "the UI was stuck behind the query for {:?}",
        started.elapsed()
    );

    cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let running_tab = this.tabs.len() - 2;
            this.cancel_tab_query(running_tab, cx);
        })
    });
}

/// Points notebook autosave at a temp dir for this test thread.
struct NotebookAutosaveGuard {
    _dir: tempfile::TempDir,
}

impl NotebookAutosaveGuard {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::notebook::autosave::set_test_root(Some(dir.path().to_path_buf()));
        Self { _dir: dir }
    }
}

impl Drop for NotebookAutosaveGuard {
    fn drop(&mut self) {
        crate::notebook::autosave::set_test_root(None);
    }
}

#[gpui::test]
fn unsaved_notebook_comes_back_from_its_recovery_copy(cx: &mut TestAppContext) {
    let _autosave = NotebookAutosaveGuard::new();
    let mut model = crate::notebook::files::new_document();
    model.cells[0].source = "SELECT 'not saved yet' AS draft".into();
    // Created in the app, so its trust record exists beside the copy.
    crate::notebook::trust::record_created(&model, None);
    crate::notebook::autosave::write_recovery(&model).expect("recovery copy");

    let mut session = WorkspaceSession::new();
    session.tabs = vec![
        TabState {
            notebook_id: Some(model.notebook_id.clone()),
            ..tab("Draft analysis")
        },
        TabState {
            // A recovery copy that is gone: the tab is skipped, not blank.
            notebook_id: Some("missing0000".into()),
            ..tab("Lost")
        },
    ];
    let _guard = SessionFileGuard::with(&session);
    let (app, cx) = open_app(cx);
    cx.run_until_parked();
    let restored: Vec<(String, Option<String>, bool)> = cx.update(|_, cx| {
        app.read(cx)
            .tabs
            .iter()
            .map(|t| {
                (
                    t.name.clone(),
                    t.notebook
                        .as_ref()
                        .map(|nb| nb.read(cx).model().cells[0].source.clone()),
                    t.notebook
                        .as_ref()
                        .is_some_and(|nb| nb.read(cx).is_trusted()),
                )
            })
            .collect()
    });
    assert_eq!(
        restored,
        vec![(
            "Draft analysis".to_string(),
            Some("SELECT 'not saved yet' AS draft".to_string()),
            true
        )]
    );

    // The next session save records the notebook by id again, and closing
    // the tab discards its recovery copy.
    let session = cx.update(|_, cx| app.read(cx).current_workspace_session(cx));
    assert_eq!(
        session.tabs[0].notebook_id.as_deref(),
        Some(model.notebook_id.as_str())
    );
    cx.update(|_, cx| app.update(cx, |this, cx| this.close_tab_impl(0, cx)));
    assert!(crate::notebook::autosave::load_recovery(&model.notebook_id).is_none());
}

#[gpui::test]
fn open_as_notebook_carries_the_tab_sql_and_convert_returns_it(cx: &mut TestAppContext) {
    let _autosave = NotebookAutosaveGuard::new();
    let _guard = SessionFileGuard::with(&WorkspaceSession::new());
    let (app, cx) = open_app(cx);
    cx.run_until_parked();
    let sql =
        "SELECT OrderID FROM dbo.Orders;\nUPDATE dbo.Orders SET Freight = 0 WHERE OrderID = 1;";
    let cells: Vec<(String, bool)> = cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            this.new_tab(cx, Some(sql.to_string()));
            let index = this.active_tab;
            this.promote_tab_to_notebook(index, true, cx);
            let nb = this.tabs[this.active_tab]
                .notebook
                .clone()
                .expect("notebook tab");
            nb.read(cx)
                .model()
                .cells
                .iter()
                .map(|c| (c.source.clone(), c.execution_policy.read_only))
                .collect()
        })
    });
    assert_eq!(
        cells,
        vec![
            ("SELECT OrderID FROM dbo.Orders;".to_string(), true),
            (
                "UPDATE dbo.Orders SET Freight = 0 WHERE OrderID = 1;".to_string(),
                false
            ),
        ]
    );
    let script = cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let index = this.active_tab;
            this.convert_notebook_to_script(index, cx);
            this.tabs[this.active_tab]
                .editor
                .read(cx)
                .text()
                .to_string()
        })
    });
    assert_eq!(script, format!("{}\n", sql.replace('\n', "\n\n")));
}

#[gpui::test]
fn notebook_cells_resolve_their_own_profile_with_read_only_enforced(cx: &mut TestAppContext) {
    let _autosave = NotebookAutosaveGuard::new();
    let mut prod = silent_profile(1);
    prod.name = "Sales production".into();
    prod.environment = sqlly_core::ConnectionEnvironment::Production;
    let prod_id = prod.id.0;
    crate::connection::ConnectionStore::global().add_provisioned(vec![prod]);
    let _guard = SessionFileGuard::with(&WorkspaceSession::new());
    let (app, cx) = open_app(cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            this.active_profile = Some(silent_profile(2));
            let index = this.new_notebook_tab(crate::notebook::files::new_document(), None, cx);
            let read_only = crate::notebook::connection::CellRunTarget {
                profile_id: Some(prod_id),
                read_only: true,
                timeout_seconds: None,
                local: false,
            };
            let profile = this.notebook_cell_profile(index, &read_only, cx).unwrap();
            assert_eq!(profile.id.0, prod_id);
            assert!(profile.block_all_but_select && profile.enforce_read_only);
            let stored = crate::connection::ConnectionStore::global()
                .get(prod_id)
                .unwrap();
            assert!(
                !stored.block_all_but_select,
                "the saved profile is untouched"
            );

            let elevated = crate::notebook::connection::CellRunTarget {
                profile_id: None,
                read_only: false,
                timeout_seconds: None,
                local: false,
            };
            let profile = this.notebook_cell_profile(index, &elevated, cx).unwrap();
            assert_eq!(Some(profile.id), this.active_profile.as_ref().map(|p| p.id));
            assert!(!profile.enforce_read_only);

            let missing = crate::notebook::connection::CellRunTarget {
                profile_id: Some(uuid::Uuid::new_v4()),
                read_only: true,
                timeout_seconds: None,
                local: false,
            };
            assert!(this.notebook_cell_profile(index, &missing, cx).is_err());
        })
    });
}

fn notebook_source(
    app: &gpui::Entity<AppView>,
    index: usize,
    cx: &mut gpui::VisualTestContext,
) -> Option<String> {
    cx.update(|_, cx| {
        app.read(cx)
            .tabs
            .get(index)
            .and_then(|t| t.notebook.as_ref())
            .map(|nb| nb.read(cx).model().cells[0].source.clone())
    })
}

#[gpui::test]
fn saved_notebooks_keep_unsaved_changes_across_a_quit_and_never_open_twice(
    cx: &mut TestAppContext,
) {
    let _autosave = NotebookAutosaveGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.sqllynb");
    let path_text = path.to_string_lossy().to_string();
    let mut saved = crate::notebook::files::new_document();
    saved.cells[0].source = "SELECT 'saved' AS v".into();
    std::fs::write(&path, saved.to_canonical_json()).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let mut unsaved = saved.clone();
    unsaved.cells[0].source = "SELECT 'unsaved' AS v".into();
    crate::notebook::autosave::write_file_recovery(&unsaved).unwrap();

    // A notebook whose file is gone, with unsaved changes left over.
    let gone = dir.path().join("gone.sqllynb");
    let mut orphan = crate::notebook::files::new_document();
    orphan.cells[0].source = "SELECT 'orphaned' AS v".into();
    crate::notebook::autosave::write_file_recovery(&orphan).unwrap();
    let never = dir.path().join("never.sqllynb");

    let mut session = WorkspaceSession::new();
    let file_tab = |title: &str, path: &std::path::Path, id: Option<&str>| TabState {
        notebook_path: Some(path.to_string_lossy().to_string()),
        notebook_id: id.map(str::to_string),
        ..tab(title)
    };
    session.tabs = vec![
        file_tab("Report", &path, Some(&saved.notebook_id)),
        file_tab("Report again", &path, Some(&saved.notebook_id)),
        file_tab("Gone", &gone, Some(&orphan.notebook_id)),
        file_tab("Never", &never, None),
    ];
    let _guard = SessionFileGuard::with(&session);
    let (app, cx) = open_app(cx);
    cx.run_until_parked();

    let (count, offered, orphan_path, status) = cx.update(|_, cx| {
        let app = app.read(cx);
        (
            app.tabs.len(),
            app.tabs[0]
                .notebook
                .as_ref()
                .is_some_and(|nb| nb.read(cx).recovery_offered()),
            app.tabs.get(1).map(|t| t.notebook_path.clone()),
            app.status_message.clone(),
        )
    });
    assert_eq!(
        count, 2,
        "one tab per file; the missing file without changes is skipped"
    );
    assert!(offered, "the newer unsaved changes are offered");
    assert_eq!(
        notebook_source(&app, 0, cx).as_deref(),
        Some("SELECT 'saved' AS v")
    );
    assert_eq!(
        notebook_source(&app, 1, cx).as_deref(),
        Some("SELECT 'orphaned' AS v"),
        "a missing file's unsaved changes come back as an unsaved notebook"
    );
    assert_eq!(orphan_path, Some(None));
    assert!(status.contains("never.sqllynb"), "{status}");

    // Opening the same file again goes to its tab.
    cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            this.select_tab(1, cx);
            this.open_notebook_from_path(&path_text, cx);
        })
    });
    assert_eq!(
        cx.update(|_, cx| (app.read(cx).tabs.len(), app.read(cx).active_tab)),
        (2, 0)
    );

    // Restoring puts the changes back unsaved; quitting keeps them beside
    // the file (file autosave is off) and leaves the file alone.
    let nb = cx.update(|_, cx| app.read(cx).tabs[0].notebook.clone().unwrap());
    nb.update(cx, |nb, cx| nb.restore_offered(cx));
    assert_eq!(
        notebook_source(&app, 0, cx).as_deref(),
        Some("SELECT 'unsaved' AS v")
    );
    cx.update(|_, cx| app.update(cx, |this, cx| this.flush_notebooks_for_quit(cx)));
    assert!(std::fs::read_to_string(&path).unwrap().contains("'saved'"));
    match crate::notebook::autosave::file_recovery(&saved.notebook_id, &path_text) {
        crate::notebook::autosave::FileRecovery::Newer(doc) => {
            assert_eq!(doc.cells[0].source, "SELECT 'unsaved' AS v")
        }
        other => panic!("expected the unsaved changes, got {other:?}"),
    }

    // Save never overwrites a file someone else changed without a choice.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&path, "{\"theirs\": true}\n").unwrap();
    cx.update(|_, cx| app.update(cx, |this, cx| this.save_notebook(0, false, cx)));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"theirs\": true}\n"
    );
    assert!(nb.read_with(cx, |nb, _| nb.has_conflict()));
    nb.update(cx, |nb, cx| nb.keep_mine(cx));
    cx.update(|_, cx| app.update(cx, |this, cx| this.save_notebook(0, false, cx)));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("'unsaved'")
    );
    assert_eq!(
        crate::notebook::autosave::file_recovery(&saved.notebook_id, &path_text),
        crate::notebook::autosave::FileRecovery::None,
        "saving retires the unsaved copy"
    );
}

#[gpui::test]
fn canceling_a_notebook_tab_cancels_its_cell_queries_and_closes_its_review(
    cx: &mut TestAppContext,
) {
    let _autosave = NotebookAutosaveGuard::new();
    let _guard = SessionFileGuard::with(&WorkspaceSession::new());
    let (app, cx) = open_app(cx);
    cx.run_until_parked();
    let (index, nb) = cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let index = this.new_notebook_tab(crate::notebook::files::new_document(), None, cx);
            (index, this.tabs[index].notebook.clone().unwrap())
        })
    });
    let tab_id = cx.update(|_, cx| app.read(cx).tabs[index].id.clone());
    let entity = nb.entity_id();
    nb.update(cx, |nb, _| nb.set_cell_query(entity, Some("task-7".into())));
    cx.update(|_, cx| {
        app.update(cx, |this, _| {
            this.pending.mutation_review = Some(super::pending_dialogs::PendingMutationReview {
                sql: "UPDATE t SET a = 1".into(),
                tab_id: tab_id.clone(),
                bottom_line: String::new(),
                has_unbounded: false,
                ai_review_required: false,
                production: false,
                ai_explanation: None,
                ai_generation: 0,
                notebook_cell: Some("cell_1".into()),
                notebook_target: None,
                multi_targets: None,
            });
        })
    });
    let stopped = cx.update(|_, cx| app.update(cx, |this, cx| this.cancel_tab_query(index, cx)));
    assert!(
        stopped,
        "a notebook tab's cell queries count as its running query"
    );
    assert!(nb.update(cx, |nb, _| nb.take_cell_queries()).is_empty());
    assert!(
        cx.update(|_, cx| app.read(cx).pending.mutation_review.is_none()),
        "a review a stopped cell waited on can no longer be confirmed"
    );
}

/// Records the task ids it is asked to cancel; runs nothing.
#[derive(Default)]
struct CancelRecorder(std::sync::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl sqlly_services::QueryExecutor for CancelRecorder {
    async fn execute(
        &self,
        _profile: &sqlly_core::ConnectionProfile,
        _sql: &str,
        _row_buffer: usize,
    ) -> sqlly_core::StudioResult<sqlly_services::QueryHandle> {
        Err(sqlly_core::StudioError::Other("not used".into()))
    }

    async fn cancel(&self, task_id: &str) -> sqlly_core::StudioResult<()> {
        self.0.lock().unwrap().push(task_id.to_string());
        Ok(())
    }
}

#[gpui::test]
fn stopping_a_notebook_cancels_relay_queries_through_the_relay(cx: &mut TestAppContext) {
    let _autosave = NotebookAutosaveGuard::new();
    let _guard = SessionFileGuard::with(&WorkspaceSession::new());
    let (app, cx) = open_app(cx);
    cx.run_until_parked();
    let (index, nb) = cx.update(|_, cx| {
        app.update(cx, |this, cx| {
            let index = this.new_notebook_tab(crate::notebook::files::new_document(), None, cx);
            (index, this.tabs[index].notebook.clone().unwrap())
        })
    });
    let relay = Arc::new(CancelRecorder::default());
    let mut relay_services = sqlly_services::remote::unavailable_services("relay");
    relay_services.queries = relay.clone();
    let entity = nb.entity_id();
    nb.update(cx, |nb, _| {
        nb.set_cell_query(entity, Some("relay-task".into()));
        nb.route_query("relay-task", relay_services.clone());
        nb.route_analysis("cell_1", relay_services);
    });
    assert!(cx.update(|_, cx| app.update(cx, |this, cx| this.cancel_tab_query(index, cx))));
    let deadline = Instant::now() + Duration::from_secs(5);
    while relay.0.lock().unwrap().is_empty() && Instant::now() < deadline {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(*relay.0.lock().unwrap(), ["relay-task"]);
    assert!(
        nb.update(cx, |nb, _| nb.take_query_route("relay-task"))
            .is_none(),
        "the route is used once"
    );
}
