//! Workbench side of notebook tabs: the shell's events beyond the original
//! set, per-cell connection routing, promote to / convert from SQL tabs,
//! recovery restore, and the notebook keyboard commands from the palette.

use super::*;
use crate::notebook::connection::{CellRunTarget, ConnectionChoice, TargetError};
use crate::notebook::editing::EditingCommand;
use crate::notebook::embedded::EmbeddedCommand;
use crate::notebook::view::{
    DispatchOutcome, NotebookCommand, NotebookEvent, NotebookOpening, NotebookView,
};

/// Palette ids of the notebook keyboard commands, which double as their
/// rebindable shortcut ids.
pub(crate) const NOTEBOOK_COMMAND_IDS: &[(&str, NotebookCommand)] = &[
    ("notebook.runCellAndAdvance", NotebookCommand::RunAndAdvance),
    (
        "notebook.runCellAndInsertBelow",
        NotebookCommand::RunAndInsertBelow,
    ),
    ("notebook.commandMode", NotebookCommand::CommandMode),
    ("notebook.selectNextCell", NotebookCommand::SelectNext),
    (
        "notebook.selectPreviousCell",
        NotebookCommand::SelectPrevious,
    ),
    ("notebook.insertCellAbove", NotebookCommand::InsertAbove),
    ("notebook.insertCellBelow", NotebookCommand::InsertBelow),
    ("notebook.deleteCell", NotebookCommand::Delete),
    ("notebook.convertToMarkdown", NotebookCommand::ToMarkdown),
    ("notebook.convertToSql", NotebookCommand::ToSql),
    ("notebook.editCell", NotebookCommand::Edit),
    ("notebook.stop", NotebookCommand::Stop),
    ("notebook.trust", NotebookCommand::Trust),
    ("notebook.allowWrites", NotebookCommand::AllowWrites),
    (
        "notebook.materializeLocally",
        NotebookCommand::MaterializeLocally,
    ),
    (
        "notebook.toggleLocalTarget",
        NotebookCommand::ToggleLocalTarget,
    ),
    ("notebook.showLocalData", NotebookCommand::ShowLocalData),
    (
        "notebook.insertAttachment",
        NotebookCommand::InsertAttachment,
    ),
    ("notebook.openDashboard", NotebookCommand::OpenDashboard),
    ("notebook.viewDashboard", NotebookCommand::ViewDashboard),
    ("notebook.toggleZen", NotebookCommand::ToggleZen),
    ("notebook.toggleDense", NotebookCommand::ToggleDense),
    (
        "notebook.selectAllCells",
        NotebookCommand::Editing(EditingCommand::SelectAll),
    ),
    (
        "notebook.extendSelectionDown",
        NotebookCommand::Editing(EditingCommand::ExtendDown),
    ),
    (
        "notebook.extendSelectionUp",
        NotebookCommand::Editing(EditingCommand::ExtendUp),
    ),
    (
        "notebook.clearSelection",
        NotebookCommand::Editing(EditingCommand::ClearSelection),
    ),
    (
        "notebook.copyCells",
        NotebookCommand::Editing(EditingCommand::Copy),
    ),
    (
        "notebook.cutCells",
        NotebookCommand::Editing(EditingCommand::Cut),
    ),
    (
        "notebook.pasteCells",
        NotebookCommand::Editing(EditingCommand::PasteBelow),
    ),
    (
        "notebook.pasteCellsAbove",
        NotebookCommand::Editing(EditingCommand::PasteAbove),
    ),
    (
        "notebook.duplicateCell",
        NotebookCommand::Editing(EditingCommand::Duplicate),
    ),
    (
        "notebook.undoStructure",
        NotebookCommand::Editing(EditingCommand::Undo),
    ),
    (
        "notebook.redoStructure",
        NotebookCommand::Editing(EditingCommand::Redo),
    ),
    (
        "notebook.moveCellUp",
        NotebookCommand::Editing(EditingCommand::MoveUp),
    ),
    (
        "notebook.moveCellDown",
        NotebookCommand::Editing(EditingCommand::MoveDown),
    ),
    (
        "notebook.toggleCollapse",
        NotebookCommand::Editing(EditingCommand::ToggleCollapse),
    ),
    (
        "notebook.runSelected",
        NotebookCommand::Editing(EditingCommand::RunSelected),
    ),
    (
        "notebook.splitCell",
        NotebookCommand::Editing(EditingCommand::Split),
    ),
    (
        "notebook.mergeCellBelow",
        NotebookCommand::Editing(EditingCommand::MergeBelow),
    ),
    (
        "notebook.mergeCellAbove",
        NotebookCommand::Editing(EditingCommand::MergeAbove),
    ),
    (
        "notebook.insertCellMenu",
        NotebookCommand::Editing(EditingCommand::InsertMenu),
    ),
    (
        "notebook.findInNotebook",
        NotebookCommand::Editing(EditingCommand::Find),
    ),
    (
        "notebook.replaceInNotebook",
        NotebookCommand::Editing(EditingCommand::Replace),
    ),
    (
        "notebook.toggleBookmark",
        NotebookCommand::Editing(EditingCommand::ToggleBookmark),
    ),
    (
        "notebook.nextBookmark",
        NotebookCommand::Editing(EditingCommand::NextBookmark),
    ),
    (
        "notebook.previousBookmark",
        NotebookCommand::Editing(EditingCommand::PreviousBookmark),
    ),
    (
        "notebook.showBookmarks",
        NotebookCommand::Editing(EditingCommand::ShowBookmarks),
    ),
    (
        "notebook.toggleFoldSection",
        NotebookCommand::Editing(EditingCommand::ToggleFold),
    ),
    (
        "notebook.unfoldAll",
        NotebookCommand::Editing(EditingCommand::UnfoldAll),
    ),
    (
        "notebook.togglePinOutput",
        NotebookCommand::Editing(EditingCommand::TogglePin),
    ),
    (
        "notebook.toggleEmbedded",
        NotebookCommand::Embedded(EmbeddedCommand::Toggle),
    ),
    (
        "notebook.embedAllOutputs",
        NotebookCommand::Embedded(EmbeddedCommand::EmbedAll),
    ),
    (
        "notebook.toggleEmbeddedConfig",
        NotebookCommand::Embedded(EmbeddedCommand::ToggleConfig),
    ),
];

impl AppView {
    /// Show what the file scan flagged in the notebook just opened in tab
    /// `index` (`text` is what was read).
    pub(super) fn note_scan_warnings(&mut self, index: usize, text: &str, cx: &mut Context<Self>) {
        let warnings = crate::notebook::files::scan_warnings(text);
        if warnings.is_empty() {
            return;
        }
        if let Some(nb) = self.tabs.get(index).and_then(|t| t.notebook.clone()) {
            nb.update(cx, |nb, cx| nb.show_scan_warnings(warnings, cx));
        }
    }

    fn notebook_tab_index(&self, notebook: &Entity<NotebookView>) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.notebook.as_ref() == Some(notebook))
    }

    /// The connection a notebook tab's cells inherit: the tab's pinned
    /// connection when it has one, else the active connection.
    pub(super) fn notebook_tab_choice(&self, tab_idx: usize) -> Option<ConnectionChoice> {
        let pinned = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.connection_target.as_ref())
            .map(|target| target.profile_id);
        match (pinned, self.active_profile.as_ref()) {
            (Some(id), Some(active)) if active.id.0 == id => {
                Some(ConnectionChoice::from_profile(active))
            }
            (Some(id), _) => ConnectionStore::global()
                .get(id)
                .map(|p| ConnectionChoice::from_profile(&p)),
            (None, active) => active.map(ConnectionChoice::from_profile),
        }
    }

    /// Push the workbench state a notebook view renders with: the inherited
    /// connection and the editor typography. Cheap when nothing changed.
    pub(super) fn sync_notebook_view(&self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(notebook) = self.tabs.get(tab_idx).and_then(|t| t.notebook.clone()) else {
            return;
        };
        let choice = self.notebook_tab_choice(tab_idx);
        let metrics = crate::notebook::cells::EditorMetrics {
            font_family: self.appearance_prefs.editor_font.clone().into(),
            font_size: Theme::scaled(crate::editor_zoom::zoomed_font_size(
                self.appearance_prefs.editor_font_size,
                self.editor_zoom,
            )),
        };
        let provider = self
            .services
            .catalog
            .llm_provider()
            .provider_id()
            .to_string();
        if !notebook.read(cx).has_query_resolver() {
            let app = cx.weak_entity();
            let target_nb = notebook.downgrade();
            let resolver: crate::notebook::local::run::QueryResolver =
                std::rc::Rc::new(move |target, cx| {
                    let missing =
                        || crate::localization::text("notebook.local.materialize.no_host");
                    let (Some(app), Some(nb)) = (app.upgrade(), target_nb.upgrade()) else {
                        return Err(missing());
                    };
                    app.update(cx, |app, cx| {
                        let tab_idx = app.notebook_tab_index(&nb).ok_or_else(missing)?;
                        let profile = app.notebook_cell_profile(tab_idx, target, cx)?;
                        let queries = app.services_for_profile(&profile).queries.clone();
                        Ok((profile, queries))
                    })
                });
            notebook.update(cx, |nb, _| nb.set_query_resolver(resolver));
        }
        let catalog = self.completion_catalog.clone();
        notebook.update(cx, |nb, cx| {
            nb.set_tab_connection(choice, cx);
            nb.set_editor_metrics(metrics, cx);
            nb.enable_history();
            nb.set_analysis_catalog(catalog);
            nb.set_ai_provider(Some(provider));
        });
    }

    /// The notebook tab already editing the file at `path`.
    pub(super) fn notebook_tab_for_path(&self, path: &str) -> Option<usize> {
        self.tabs.iter().position(|t| {
            t.notebook.is_some()
                && t.notebook_path
                    .as_deref()
                    .is_some_and(|p| super::files::same_notebook_path(p, path))
        })
    }

    /// Stop `index`'s notebook run and cancel every query its cells and
    /// analysis cells still have running, and close a change review one of
    /// its cells is waiting on (Stop, Cancel, closing the tab, disconnect).
    /// Whether anything was stopped.
    pub(super) fn cancel_notebook_queries(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some((nb, tab_id)) = self
            .tabs
            .get(index)
            .and_then(|t| Some((t.notebook.clone()?, t.id.clone())))
        else {
            return false;
        };
        let in_process = self.services.clone();
        let (queries, was_running) = nb.update(cx, |nb, cx| {
            let was_running = nb.is_running();
            nb.stop(cx);
            let mut ids = nb.take_cell_queries();
            ids.extend(nb.take_analysis_queries());
            let queries: Vec<(String, Services)> = ids
                .into_iter()
                .map(|id| {
                    let services = nb
                        .take_query_route(&id)
                        .unwrap_or_else(|| in_process.clone());
                    (id, services)
                })
                .collect();
            (queries, was_running)
        });
        let review = self
            .pending
            .mutation_review
            .as_ref()
            .is_some_and(|r| r.tab_id == tab_id && r.notebook_cell.is_some());
        if review {
            self.pending.mutation_review = None;
            self.execute_rollback_bypass_once = false;
        }
        if !queries.is_empty() {
            cx.background_spawn(async move {
                crate::tokio_handle()
                    .spawn(async move {
                        for (id, services) in queries {
                            let _ = services.queries.cancel(&id).await;
                        }
                    })
                    .await
                    .ok();
            })
            .detach();
            return true;
        }
        was_running || review
    }

    /// Quitting: every notebook writes what it has not saved yet (recovery
    /// copies, file autosave), so the session restores it.
    pub(super) fn flush_notebooks_for_quit(&mut self, cx: &mut Context<Self>) {
        let notebooks: Vec<Entity<NotebookView>> = self
            .tabs
            .iter()
            .filter_map(|t| t.notebook.clone())
            .collect();
        for nb in notebooks {
            nb.update(cx, |nb, cx| nb.flush_for_quit(cx));
        }
    }

    /// A restored file-backed notebook (the last tab): offer the unsaved
    /// changes it had when the app quit, when they are newer than the file.
    pub(super) fn offer_notebook_recovery(
        &mut self,
        tab_state: &crate::workspace_session::TabState,
        cx: &mut Context<Self>,
    ) {
        let (Some(id), Some(path)) = (
            tab_state.notebook_id.as_deref(),
            tab_state.notebook_path.as_deref(),
        ) else {
            return;
        };
        let Some(nb) = self.tabs.last().and_then(|t| t.notebook.clone()) else {
            return;
        };
        if nb.read(cx).doc().id() != id {
            return;
        }
        if let crate::notebook::autosave::FileRecovery::Newer(doc) =
            crate::notebook::autosave::file_recovery(id, path)
        {
            nb.update(cx, |nb, cx| nb.offer_recovery(doc, cx));
        }
    }

    /// A notebook file the session names is gone: reopen its unsaved
    /// changes from the last session when there are any, and say so either
    /// way instead of dropping the tab silently.
    pub(super) fn restore_missing_notebook_file(
        &mut self,
        path: &str,
        tab_state: &crate::workspace_session::TabState,
        cx: &mut Context<Self>,
    ) {
        let recovered = tab_state.notebook_id.as_deref().and_then(|id| {
            match crate::notebook::autosave::file_recovery(id, path) {
                crate::notebook::autosave::FileRecovery::FileMissing(doc) => Some(doc),
                _ => None,
            }
        });
        let Some(doc) = recovered else {
            self.set_status(
                StatusLevel::Warning,
                crate::localization::format(
                    "notebook.fix_runtime.restore.file_missing",
                    &[("path", path)],
                ),
            );
            return;
        };
        let id = doc.notebook_id.clone();
        self.new_notebook_tab_with(doc, None, NotebookOpening::Recovered, cx);
        if let Some(tab) = self.tabs.last_mut() {
            tab.name = tab_state.title.clone();
            tab.pinned = tab_state.pinned;
            tab.user_named = tab_state.user_named;
            tab.auto_named = !tab.user_named;
            tab.dirty = true;
        }
        if let Some(nb) = self.tabs.last().and_then(|t| t.notebook.clone()) {
            nb.update(cx, |nb, cx| nb.autosave_now(cx));
        }
        crate::notebook::autosave::remove_file_recovery(&id);
        self.set_status(
            StatusLevel::Warning,
            crate::localization::format(
                "notebook.fix_runtime.restore.file_missing_recovered",
                &[("path", path)],
            ),
        );
    }

    /// Reopen an unsaved notebook from its recovery autosave.
    pub(super) fn restore_recovered_notebook(
        &mut self,
        id: &str,
        tab_state: &crate::workspace_session::TabState,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(model) = crate::notebook::autosave::load_recovery(id) else {
            tracing::debug!(id, "notebook recovery file missing; skipping restore");
            return false;
        };
        self.new_notebook_tab_with(model, None, NotebookOpening::Recovered, cx);
        if let Some(tab) = self.tabs.last_mut() {
            tab.name = tab_state.title.clone();
            tab.pinned = tab_state.pinned;
            tab.user_named = tab_state.user_named;
            tab.auto_named = tab_state.auto_named.unwrap_or(!tab.user_named);
            tab.auto_number = tab_state.auto_number;
            tab.dirty = true;
        }
        true
    }

    /// The profile a notebook cell runs under: the cell's own (resolved
    /// from the connection store) or the tab's, with the database-enforced
    /// read-only mode when the cell is read-only.
    pub(super) fn notebook_cell_profile(
        &self,
        tab_idx: usize,
        target: &CellRunTarget,
        cx: &App,
    ) -> Result<ConnectionProfile, String> {
        self.notebook_cell_profile_message(tab_idx, target, cx)
            .map_err(crate::localization::message)
    }

    /// [`Self::notebook_cell_profile`] with the failure kept as its key, for
    /// replies a notebook stores.
    pub(super) fn notebook_cell_profile_message(
        &self,
        tab_idx: usize,
        target: &CellRunTarget,
        cx: &App,
    ) -> Result<ConnectionProfile, sqlly_core::DisplayMessage> {
        let pinned = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.connection_target.as_ref())
            .map(|t| t.profile_id);
        if target.local {
            let mut profile = self
                .tabs
                .get(tab_idx)
                .and_then(|t| t.notebook.as_ref())
                .and_then(|nb| nb.read(cx).local_profile())
                .ok_or_else(|| {
                    crate::localization::deferred_text("notebook.local.workspace.not_open")
                })?;
            crate::notebook::connection::apply_policy(&mut profile, target);
            return Ok(profile);
        }
        let tab_profile = match pinned {
            Some(id) if self.active_profile.as_ref().is_none_or(|p| p.id.0 != id) => {
                ConnectionStore::global().get(id)
            }
            _ => self.active_profile.clone(),
        };
        crate::notebook::connection::profile_for_target(
            target,
            |id| ConnectionStore::global().get(id),
            tab_profile.as_ref(),
        )
        .map_err(|error| match error {
            TargetError::MissingProfile(_) => {
                crate::localization::deferred_text("notebook.shell.connection.missing_profile")
            }
            TargetError::NoConnection => {
                crate::localization::deferred_text("shell.execute.no_active_connection")
            }
        })
    }

    /// Run a notebook cell. A request that did not come from the notebook's
    /// own queue goes back through it (trust, run mode, dependencies);
    /// the queued dispatch builds the SQL and runs it on the cell's target.
    pub(super) fn run_notebook_cell(
        &mut self,
        tab_idx: usize,
        cell_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(nb) = self.tabs.get(tab_idx).and_then(|t| t.notebook.clone()) else {
            return;
        };
        if !nb.read(cx).is_dispatching(&cell_id) {
            nb.update(cx, |nb, cx| nb.request_run_from_host(&cell_id, cx));
            return;
        }
        self.dispatch_notebook_cell(tab_idx, cell_id, cx);
    }

    /// Run a cell the notebook's queue handed over. One the queue no longer
    /// dispatches (stopped, blocked or settled since) is dropped, never
    /// planned again.
    pub(super) fn dispatch_notebook_cell(
        &mut self,
        tab_idx: usize,
        cell_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some((nb, fallback_editor)) = self
            .tabs
            .get(tab_idx)
            .and_then(|t| Some((t.notebook.clone()?, t.editor.clone())))
        else {
            return;
        };
        if !nb.read(cx).is_dispatching(&cell_id) {
            return;
        }
        if nb
            .read(cx)
            .doc()
            .cell_by_id(&cell_id)
            .is_some_and(|c| c.kind == crate::notebook::ai::KIND_AI)
        {
            nb.update(cx, |nb, cx| nb.ai_skip_queued(&cell_id, cx));
            return;
        }
        let request = match nb.read(cx).run_request(&cell_id) {
            Ok(request) => request,
            Err(message) => {
                self.set_status(StatusLevel::Error, message.clone());
                nb.update(cx, |nb, cx| {
                    nb.dispatch_result(
                        &cell_id,
                        DispatchOutcome::NotStarted(sqlly_core::DisplayMessage::raw(message)),
                        cx,
                    )
                });
                cx.notify();
                return;
            }
        };
        if let Some(history_sql) = request.history_sql {
            if self.notebook_history_redactions.len() >= 32 {
                self.notebook_history_redactions.clear();
            }
            self.notebook_history_redactions
                .insert(request.sql.clone(), history_sql);
        }
        if !nb.read(cx).has_results(&cell_id) {
            let results = self.make_cell_results(&fallback_editor, cx);
            cx.subscribe(&results, Self::handle_results_event).detach();
            nb.update(cx, |nb, cx| nb.attach_results(&cell_id, results, cx));
        }
        self.execute_query_with_origin(
            tab_idx,
            super::execute::ExecuteOrigin::NotebookCell {
                cell_id: cell_id.clone(),
                sql: request.sql,
                target: request.target,
            },
            cx,
        );
        let awaiting = self
            .pending
            .mutation_review
            .as_ref()
            .is_some_and(|r| r.notebook_cell.as_deref() == Some(cell_id.as_str()));
        let running = nb
            .read(cx)
            .cell_entities(&cell_id)
            .map(|(_, results)| results)
            .or_else(|| {
                nb.read(cx)
                    .cell_run_targets(&cell_id, &fallback_editor)
                    .map(|(_, r)| r)
            })
            .is_some_and(|results| results.read(cx).is_running());
        let outcome = if awaiting {
            DispatchOutcome::AwaitingApproval
        } else if running {
            DispatchOutcome::Started
        } else {
            DispatchOutcome::NotStarted(if self.status_level == StatusLevel::Error {
                sqlly_core::DisplayMessage::raw(self.status_message.clone())
            } else {
                crate::localization::deferred_text("notebook.shell.run.not_started")
            })
        };
        nb.update(cx, |nb, cx| nb.dispatch_result(&cell_id, outcome, cx));
    }

    /// Events from the notebook shell beyond the ones `handle_notebook_event`
    /// routes itself.
    pub(super) fn handle_notebook_shell_event(
        &mut self,
        notebook: Entity<NotebookView>,
        event: &NotebookEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_idx) = self.notebook_tab_index(&notebook) else {
            return;
        };
        match event {
            NotebookEvent::StopRequested => {
                if self.cancel_tab_query(tab_idx, cx) {
                    self.query_cancelled_status(cx);
                }
            }
            NotebookEvent::LayoutChanged(layout) => {
                self.notebook_layout = layout.clone();
                for tab in &self.tabs {
                    if let Some(other) = tab.notebook.as_ref().filter(|n| *n != &notebook) {
                        other.update(cx, |nb, _| nb.set_layout(layout.clone()));
                    }
                }
                self.save_workspace_session(cx);
            }
            NotebookEvent::TitleChanged(title) => {
                if let Some(tab) = self.tabs.get_mut(tab_idx)
                    && !title.is_empty()
                {
                    tab.name = title.clone();
                    tab.auto_named = false;
                    tab.auto_number = None;
                }
                self.schedule_session_autosave(cx);
            }
            NotebookEvent::ConvertToScriptRequested => self.convert_notebook_to_script(tab_idx, cx),
            NotebookEvent::ReloadRequested => self.reload_notebook_from_disk(tab_idx, cx),
            NotebookEvent::AiRunRequested { cell_id, consented } => {
                self.run_notebook_ai(tab_idx, cell_id.clone(), *consented, cx)
            }
            NotebookEvent::SavedToFile => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.dirty = false;
                }
            }
            NotebookEvent::OpenSqlInEditor { sql } => self.new_tab(cx, Some(sql.clone())),
            NotebookEvent::AnalysisQuery(request) => {
                self.run_notebook_analysis(notebook.clone(), tab_idx, request.clone(), cx)
            }
            NotebookEvent::ParamOptionsRequested(request) => {
                self.load_param_options(&notebook, tab_idx, (**request).clone(), cx)
            }
            _ => {}
        }
        cx.notify();
    }

    /// Run a parameter's discovery query read-only on its connection and
    /// hand the options back to the notebook.
    fn load_param_options(
        &mut self,
        notebook: &Entity<NotebookView>,
        tab_idx: usize,
        request: crate::notebook::params::ParamOptionsRequest,
        cx: &mut Context<Self>,
    ) {
        let param = request.param.clone();
        let fingerprint = request.fingerprint.clone();
        let profile = match self.notebook_cell_profile(tab_idx, &request.target, cx) {
            Ok(profile) => profile,
            Err(message) => {
                notebook.update(cx, |nb, cx| {
                    nb.param_options_loaded(&param, &fingerprint, Err(message), cx)
                });
                return;
            }
        };
        let services = self.services_for_profile(&profile);
        let weak = notebook.downgrade();
        cx.spawn(async move |_this, cx| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            crate::tokio_handle().spawn(async move {
                let result = sqlly_services::notebook::cells::parameter::collect_options(
                    services.queries.as_ref(),
                    &profile,
                    &request.bound,
                    &request.query,
                )
                .await;
                let _ = tx.send(result);
            });
            let result = rx.await.ok();
            weak.update(cx, |nb, cx| {
                let result = match result {
                    Some(Ok(page)) => Ok(page),
                    Some(Err(error)) => Err(crate::localization::message(error.message())),
                    None => Err(crate::localization::text(
                        "notebook.params.options.interrupted",
                    )),
                };
                nb.param_options_loaded(&param, &fingerprint, result, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Open as Notebook: a new notebook whose SQL cells carry the tab's SQL
    /// and whose default connection is the tab's.
    pub(super) fn promote_tab_to_notebook(
        &mut self,
        tab_idx: usize,
        split: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        if tab.is_notebook() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("notebook.shell.promote.already_notebook"),
            );
            cx.notify();
            return;
        }
        let sql = tab.editor.read(cx).text().to_string();
        let db = tab.editor.read(cx).database_type();
        let database = tab
            .connection_target
            .as_ref()
            .and_then(|t| t.database.clone());
        let connection = self.notebook_tab_choice(tab_idx).map(|choice| {
            let mut r = choice.to_ref("conn_1");
            if database.is_some() {
                r.database = database;
            }
            r
        });
        let model = crate::notebook::promote::notebook_from_query(&sql, connection, split, db);
        let cells = model.cells.len();
        self.new_notebook_tab_with(model, None, NotebookOpening::New, cx);
        self.set_status(
            StatusLevel::Success,
            crate::localization::format(
                if cells == 1 {
                    "notebook.shell.promote.opened_one"
                } else {
                    "notebook.shell.promote.opened"
                },
                &[("count", cells)],
            ),
        );
        self.save_workspace_session(cx);
        cx.notify();
    }

    /// Status for a notebook command run while no notebook is open.
    pub(super) fn notebook_required_status(&mut self) {
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("shell.commands.notebook_command_requires_notebook"),
        );
    }

    /// Convert to SQL script: the notebook's SQL in a new query tab.
    pub(super) fn convert_notebook_to_script(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(nb) = self.tabs.get(tab_idx).and_then(|t| t.notebook.clone()) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.commands.convert_notebook_requires_notebook"),
            );
            cx.notify();
            return;
        };
        let db = nb.read(cx).doc().database_type();
        let script = crate::notebook::promote::script_from_notebook(nb.read(cx).model(), db);
        self.new_tab(cx, Some(script));
        self.set_status(
            StatusLevel::Success,
            crate::localization::text("notebook.shell.promote.converted"),
        );
        self.save_workspace_session(cx);
        cx.notify();
    }

    fn reload_notebook_from_disk(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Some(path) = self.tabs.get(tab_idx).and_then(|t| t.notebook_path.clone()) else {
            return;
        };
        let text = match crate::notebook::files::read_text(&path) {
            Ok(text) => text,
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.files.notebook_reload_failed",
                        &[("error", &error.to_string())],
                    ),
                );
                cx.notify();
                return;
            }
        };
        let db = self.active_profile.as_ref().map(|p| p.database_type);
        match crate::notebook::files::load_file(&path, &text, db) {
            Ok(loaded) => {
                let model = loaded.doc;
                let opening = NotebookOpening::File;
                let view = cx.new(|cx| NotebookView::open(model, opening, cx));
                cx.subscribe(&view, Self::handle_notebook_event).detach();
                let layout = self.notebook_layout.clone();
                let warnings = crate::notebook::files::scan_warnings(&text);
                view.update(cx, |nb, cx| {
                    nb.set_layout(layout);
                    nb.opened_from(path.clone(), cx);
                    if !warnings.is_empty() {
                        nb.show_scan_warnings(warnings, cx);
                    }
                });
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.notebook = Some(view);
                    tab.dirty = false;
                }
                self.sync_notebook_view(tab_idx, cx);
            }
            Err(message) => self.set_status(StatusLevel::Error, message),
        }
        cx.notify();
    }

    /// Palette entry for a notebook keyboard command.
    pub(super) fn run_notebook_command(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(command) = NOTEBOOK_COMMAND_IDS
            .iter()
            .find(|(cid, _)| *cid == id)
            .map(|(_, c)| *c)
        else {
            return false;
        };
        match self.active_notebook() {
            Some((_, nb)) => nb.update(cx, |nb, cx| nb.command(command, cx)),
            None => {
                self.notebook_required_status();
                cx.notify();
            }
        }
        true
    }

    pub(super) fn handle_stop_notebook_run(
        &mut self,
        _action: &StopNotebookRun,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_notebook_command("notebook.stop", cx);
    }

    pub(super) fn handle_open_as_notebook(
        &mut self,
        _action: &OpenAsNotebook,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.promote_tab_to_notebook(self.active_tab, false, cx);
    }

    pub(super) fn handle_convert_notebook_to_script(
        &mut self,
        _action: &ConvertNotebookToScript,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.convert_notebook_to_script(self.active_tab, cx);
    }
}
