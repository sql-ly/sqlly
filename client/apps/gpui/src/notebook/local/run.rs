//! The view side of local data: opening the notebook's workspace, preparing
//! local cells (copying their inputs in first), Materialize Locally,
//! attachment imports, cached outputs, row caps and per-cell query ids.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Context, Entity};
use sqlly_core::ConnectionProfile;
use sqlly_services::QueryExecutor;
use sqlly_services::notebook::CellEvent;
use sqlly_services::notebook::cells::local::CacheStorage;

use super::{LocalPrompt, PendingAction, Truncation};
use crate::notebook::connection::CellRunTarget;
use crate::notebook::doc_adapter::{InputSource, KIND_ATTACHMENT, LocalInput};
use crate::notebook::view::{CellStatus, NotebookView};
use crate::results::ResultsView;

/// Resolves a cell's run target to the profile its query runs under (with
/// the cell's read-only mode applied) and the query service for it. The
/// host installs it; it is only called from tasks, never inside an update.
pub type QueryResolver = Rc<
    dyn Fn(&CellRunTarget, &mut App) -> Result<(ConnectionProfile, Arc<dyn QueryExecutor>), String>,
>;

/// Row and download caps a run of one cell stops at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunCaps {
    pub max_rows: Option<u64>,
    pub max_bytes: Option<u64>,
}

impl RunCaps {
    pub fn is_empty(&self) -> bool {
        self.max_rows.is_none() && self.max_bytes.is_none()
    }
}

/// Why a materialization did not go ahead right away.
pub(crate) enum Gate {
    /// A prompt is now showing; the action continues from it.
    Prompted,
    Denied(String),
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) type WorkspaceWaiter = Box<
    dyn FnOnce(
        &mut NotebookView,
        Result<sqlly_services::local_analytics::NotebookWorkspace, String>,
        &mut Context<NotebookView>,
    ),
>;

#[cfg(not(target_arch = "wasm32"))]
fn analytics(prefs: &super::LocalPrefs) -> Arc<sqlly_services::local_analytics::LocalAnalytics> {
    use sqlly_services::local_analytics::{LocalAnalytics, LocalAnalyticsConfig};
    static ANALYTICS: std::sync::OnceLock<Arc<LocalAnalytics>> = std::sync::OnceLock::new();
    ANALYTICS
        .get_or_init(|| {
            let mut config = LocalAnalyticsConfig::new(LocalAnalyticsConfig::default_root());
            config.quota = prefs.quota();
            Arc::new(LocalAnalytics::new(config))
        })
        .clone()
}

/// Files whose attachment imports tests hold back, and for how long.
#[cfg(test)]
pub(crate) static SLOW_IMPORTS: std::sync::Mutex<Vec<(std::path::PathBuf, u64)>> =
    std::sync::Mutex::new(Vec::new());

/// Run `work` on the tokio runtime and wait for it from a gpui task.
#[cfg(not(target_arch = "wasm32"))]
async fn on_tokio<T, F>(cx: &mut gpui::AsyncApp, work: F) -> Option<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let handle = crate::tokio_handle();
    cx.background_executor()
        .spawn(async move { handle.spawn(work).await.ok() })
        .await
}

fn text(key: &str) -> String {
    crate::localization::text(key)
}

impl NotebookView {
    // ── Host hooks ────────────────────────────────────────────────────────

    /// The attachment pictures Markdown `source` names that are loaded,
    /// asking for the others once; `None` when it names none.
    pub(crate) fn markdown_images(
        &mut self,
        source: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<crate::notebook::cells::markdown::MarkdownImages>> {
        let names = sqlly_services::notebook::cells::attachment::markdown_attachment_refs(source);
        if names.is_empty() {
            return None;
        }
        let mut images = crate::notebook::cells::markdown::MarkdownImages::default();
        for name in names {
            let Some(id) = self.resolve_cell(&name).filter(|id| {
                self.doc
                    .cell_by_id(id)
                    .is_some_and(|c| c.kind == KIND_ATTACHMENT)
            }) else {
                continue;
            };
            match self.local.images.get(&id) {
                Some(image) => {
                    images.0.insert(name, image.image.clone());
                }
                None => {
                    #[cfg(not(target_arch = "wasm32"))]
                    self.load_markdown_image(&id, cx);
                    #[cfg(target_arch = "wasm32")]
                    let _ = &cx;
                }
            }
        }
        Some(Arc::new(images))
    }

    pub fn set_query_resolver(&mut self, resolver: super::run::QueryResolver) {
        self.local.resolver = Some(resolver);
    }

    pub fn has_query_resolver(&self) -> bool {
        self.local.resolver.is_some()
    }

    /// The read-only DuckDB profile local cells run under, once the
    /// workspace is open.
    pub fn local_profile(&self) -> Option<ConnectionProfile> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.local
                .workspace
                .as_ref()
                .map(|ws| ws.connection_profile(&text(super::LOCAL_LABEL_KEY)))
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }

    /// The cell whose results view `results` is.
    pub fn cell_for_results(&self, results: &Entity<ResultsView>) -> Option<String> {
        self.cells
            .iter()
            .find(|(_, rt)| rt.results.as_ref() == Some(results))
            .map(|(id, _)| id.clone())
    }

    /// Remember (or forget) the query running into `results`, so cells on
    /// several connections cancel one by one.
    pub fn set_cell_query(&mut self, results: gpui::EntityId, task_id: Option<String>) {
        match task_id {
            Some(id) => {
                self.local.cell_queries.insert(results, id);
            }
            None => {
                if let Some(id) = self.local.cell_queries.remove(&results) {
                    self.local.query_routes.remove(&id);
                }
            }
        }
    }

    /// The query `task_id` runs on `services` (the relay's, for a Via-Relay
    /// connection): Stop cancels it there.
    pub fn route_query(&mut self, task_id: &str, services: sqlly_services::Services) {
        self.local
            .query_routes
            .insert(task_id.to_string(), services);
    }

    /// The services query `task_id` was started on, forgotten.
    pub fn take_query_route(&mut self, task_id: &str) -> Option<sqlly_services::Services> {
        self.local.query_routes.remove(task_id)
    }

    pub fn cell_query(&self, results: gpui::EntityId) -> Option<String> {
        self.local.cell_queries.get(&results).cloned()
    }

    /// Every running cell query, emptied (Stop).
    pub fn take_cell_queries(&mut self) -> Vec<String> {
        self.local.cell_queries.drain().map(|(_, id)| id).collect()
    }

    /// The caps a run of `id` stops at; none after Fetch All.
    pub fn run_caps(&self, id: &str) -> RunCaps {
        if self.local.fetch_all.contains(id) {
            return RunCaps::default();
        }
        self.doc
            .cell_by_id(id)
            .map(|c| RunCaps {
                max_rows: c.policy.max_rows.filter(|n| *n > 0),
                max_bytes: c.policy.max_download_bytes.filter(|n| *n > 0),
            })
            .unwrap_or_default()
    }

    /// The host stopped `id`'s query at a cap; its output is marked
    /// truncated when it completes.
    pub fn mark_truncated(&mut self, id: &str, truncation: Truncation, cx: &mut Context<Self>) {
        self.local.truncated.insert(id.to_string(), truncation);
        cx.notify();
    }

    pub fn truncation(&self, id: &str) -> Option<Truncation> {
        self.local.truncated.get(id).copied()
    }

    /// Run `id` again without its caps.
    pub(crate) fn fetch_all(&mut self, id: &str, cx: &mut Context<Self>) {
        self.local.fetch_all.insert(id.to_string());
        self.activate_cell(id, cx);
        self.run_scope(
            crate::notebook::doc_adapter::RunScope::Cell,
            Some(id.to_string()),
            cx,
        );
    }

    pub fn session_lane(&self, id: &str) -> Option<String> {
        self.doc.session_group(id)
    }

    // ── Targets ───────────────────────────────────────────────────────────

    pub(crate) fn set_cell_local(&mut self, id: &str, local: bool, cx: &mut Context<Self>) {
        if self.doc.set_targets_local(id, local) {
            if local {
                self.apply_local_completion(cx);
            }
            self.mark_dirty_from_local(cx);
        }
    }

    pub(crate) fn set_auto_materialize(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.doc.set_auto_materialize(on) {
            self.mark_dirty_from_local(cx);
        }
    }

    pub(crate) fn set_cache_settings(
        &mut self,
        id: &str,
        settings: sqlly_services::notebook::cells::local::CacheSettings,
        cx: &mut Context<Self>,
    ) {
        if self.doc.set_cache_settings(id, settings) {
            self.mark_dirty_from_local(cx);
        }
    }

    pub(crate) fn set_cache_policy(
        &mut self,
        id: &str,
        policy: sqlly_services::notebook::CachePolicy,
        cx: &mut Context<Self>,
    ) {
        if self.doc.set_cache_policy(id, policy) {
            self.mark_dirty_from_local(cx);
        }
    }

    pub(crate) fn set_max_rows(&mut self, id: &str, max_rows: Option<u64>, cx: &mut Context<Self>) {
        if self.doc.set_max_rows(id, max_rows) {
            self.mark_dirty_from_local(cx);
        }
    }

    pub(crate) fn set_per_connection(&mut self, n: usize, cx: &mut Context<Self>) {
        let n = n.clamp(1, super::prefs::MAX_PER_CONNECTION);
        if self.local.prefs.per_connection != n {
            self.local.prefs.per_connection = n;
            self.local.prefs.save();
            cx.notify();
        }
    }

    /// Point the live editors of local cells at DuckDB and complete over
    /// the notebook's local relations and their columns.
    pub(crate) fn apply_local_completion(&mut self, cx: &mut Context<Self>) {
        use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject};
        let editors: Vec<_> = self
            .cells
            .iter()
            .filter(|(id, _)| self.doc.targets_local(id))
            .filter_map(|(_, rt)| rt.editor.clone())
            .collect();
        if editors.is_empty() {
            return;
        }
        let catalog = Arc::new(CompletionCatalog {
            objects: self
                .local
                .relations
                .iter()
                .map(|r| {
                    let columns = r
                        .provenance
                        .as_ref()
                        .map(|p| p.columns.iter().map(|(name, _, _)| name.clone()).collect())
                        .unwrap_or_default();
                    CompletionObject::table("main", r.name.clone(), columns)
                })
                .collect(),
            schemas: vec!["main".into()],
            ..Default::default()
        });
        for editor in editors {
            let catalog = catalog.clone();
            editor.update(cx, |e, cx| {
                e.set_database_type(sqlly_core::DatabaseType::DuckDb, cx);
                e.set_completion_catalog(catalog, cx);
            });
        }
    }

    // ── Preparing local runs ──────────────────────────────────────────────

    pub(crate) fn needs_local_preparation(&self, id: &str) -> bool {
        self.doc.targets_local(id)
            || self
                .doc
                .cell_by_id(id)
                .is_some_and(|c| c.kind == KIND_ATTACHMENT)
    }

    /// Whether a copy of `input` in local data still matches it: copied
    /// from that cell, from the same SQL, and not older than its latest
    /// result. Local and attachment inputs are recomputed or kept as is.
    pub(crate) fn input_current(&self, input: &LocalInput) -> bool {
        let Some(prov) = self
            .local
            .relation(&input.relation)
            .and_then(|r| r.provenance.as_ref())
        else {
            return false;
        };
        if prov.cell_id.as_deref() != Some(input.cell_id.as_str()) {
            return false;
        }
        match input.source {
            InputSource::Remote => {
                let hash = self
                    .doc
                    .sql_for_run(&input.cell_id)
                    .ok()
                    .map(|sql| sqlly_services::local_analytics::source_sql_hash(&sql));
                if prov.sql_hash != hash {
                    return false;
                }
                match (self.local.last_runs.get(&input.cell_id), prov.extracted_at) {
                    (Some((_, finished)), Some(extracted)) => *finished <= extracted,
                    _ => true,
                }
            }
            InputSource::LocalSql => false,
            InputSource::Attachment => true,
            InputSource::Other => false,
        }
    }

    /// The security lane's decision on copying `cell_id`'s inputs (or its
    /// own result) into local data. Opens the per-connection opt-in prompt
    /// the first time a connection is used.
    pub(crate) fn gate_materialize(
        &mut self,
        cell_id: &str,
        then: PendingAction,
        cx: &mut Context<Self>,
    ) -> Result<(), Gate> {
        use sqlly_services::notebook_security::{Decision, NotebookAction, Reason, authorize};
        let facts = self.connection_facts();
        let decision = self.with_auth_context(&facts, |ctx| {
            authorize(&NotebookAction::MaterializeLocally { cell_id }, ctx)
        });
        match decision {
            Decision::Allow { .. } => Ok(()),
            Decision::Deny(Reason::MaterializeNotEnabled { slot }) => {
                let Some((profile_id, choice)) = facts
                    .get(&slot)
                    .and_then(|f| f.profile_id.clone())
                    .map(|p| {
                        (
                            p.clone(),
                            self.choices.iter().find(|c| c.profile_id == p).cloned(),
                        )
                    })
                else {
                    return Err(Gate::Denied(text("notebook.local.consent.unmapped")));
                };
                let (label, environment) = match choice {
                    Some(c) => (c.label, c.environment),
                    None => (slot, sqlly_core::ConnectionEnvironment::Unset),
                };
                self.local.prompt = Some(LocalPrompt::Consent {
                    profile_id,
                    label,
                    environment,
                    then,
                });
                cx.notify();
                Err(Gate::Prompted)
            }
            Decision::Deny(reason) | Decision::NeedsApproval(reason) => {
                Err(Gate::Denied(crate::localization::message(reason.message())))
            }
        }
    }

    /// What the security lane needs to know about each connection slot.
    fn connection_facts(
        &self,
    ) -> std::collections::BTreeMap<String, sqlly_services::notebook_security::ConnectionFacts>
    {
        use sqlly_services::notebook_security::ConnectionFacts;
        use sqlly_services::notebook_security::trust::CURRENT_CONNECTION;
        let facts =
            |choice: Option<&crate::notebook::connection::ConnectionChoice>| ConnectionFacts {
                profile_id: choice.map(|c| c.profile_id.clone()),
                engine: choice.map(|c| c.database_type),
                environment: choice
                    .map(|c| c.environment)
                    .unwrap_or(sqlly_core::ConnectionEnvironment::Unset),
                allow_local_materialize: choice
                    .is_some_and(|c| self.local.prefs.allows_materialize(&c.profile_id)),
                writes_unlocked: false,
            };
        let mut out = std::collections::BTreeMap::new();
        out.insert(
            CURRENT_CONNECTION.to_string(),
            facts(self.tab_connection.as_ref()),
        );
        for slot in &self.doc.model().connection_refs {
            let choice = slot
                .profile_id
                .as_deref()
                .and_then(|id| self.choices.iter().find(|c| c.profile_id == id));
            out.insert(slot.id.clone(), facts(choice));
        }
        out
    }

    /// The notebook's live trust and these connection facts as the
    /// security lane's context.
    fn with_auth_context<T>(
        &self,
        facts: &std::collections::BTreeMap<
            String,
            sqlly_services::notebook_security::ConnectionFacts,
        >,
        f: impl FnOnce(&sqlly_services::notebook_security::AuthContext<'_>) -> T,
    ) -> T {
        use sqlly_services::notebook_security::{AuthContext, Consents, DataPolicy};
        let doc = self.doc.model();
        let trust = self.live_trust();
        let policy = DataPolicy::default();
        let consents = Consents::default();
        f(&AuthContext {
            doc,
            trust: &trust,
            connections: facts,
            policy: &policy,
            consents: &consents,
        })
    }

    /// Prepare local cell (or attachment) `id` the schedule started: open
    /// the workspace, copy in what it reads, then hand it to the host.
    pub(crate) fn prepare_local_run(&mut self, id: &str, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.fail_started(
                id,
                sqlly_services::local_analytics::unavailable_message(),
                cx,
            );
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self
                .doc
                .cell_by_id(id)
                .is_some_and(|c| c.kind == KIND_ATTACHMENT)
            {
                self.run_attachment(id, cx);
                return;
            }
            let inputs = match self.doc.local_inputs(id) {
                Ok(inputs) => inputs,
                Err(message) => return self.fail_started(id, message, cx),
            };
            if let Some(bad) = inputs.iter().find(|i| i.source == InputSource::Other) {
                let message = crate::localization::deferred(
                    "notebook.local.run.unsupported_input",
                    &[("cell_name", self.cell_label(&bad.cell_id))],
                );
                return self.fail_started(id, message, cx);
            }
            let id = id.to_string();
            self.with_workspace(cx, move |this, ws, cx| {
                let ws = match ws {
                    Ok(ws) => ws,
                    Err(message) => return this.fail_started(&id, message, cx),
                };
                let stale: Vec<LocalInput> = inputs
                    .into_iter()
                    .filter(|i| !this.input_current(i))
                    .collect();
                let remote: Vec<&LocalInput> = stale
                    .iter()
                    .filter(|i| i.source == InputSource::Remote)
                    .collect();
                if !remote.is_empty() {
                    match this.gate_materialize(
                        &id,
                        PendingAction::RunLocal {
                            cell_id: id.clone(),
                        },
                        cx,
                    ) {
                        Ok(()) => {}
                        Err(Gate::Prompted) => return this.note_waiting(&id, cx),
                        Err(Gate::Denied(message)) => return this.fail_started(&id, message, cx),
                    }
                    if !this.doc.auto_materialize() && !this.local.inputs_approved.contains(&id) {
                        let inputs = remote.iter().map(|i| this.input_line(&i.cell_id)).collect();
                        this.local.prompt = Some(LocalPrompt::Inputs {
                            cell_id: id.clone(),
                            inputs,
                        });
                        return this.note_waiting(&id, cx);
                    }
                }
                this.copy_inputs(Some(id), stale, false, ws, cx);
            });
        }
    }

    fn note_waiting(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(rt) = self.cells.get_mut(id) {
            rt.message = Some(crate::localization::deferred_text(
                "notebook.local.run.waiting_for_you",
            ));
        }
        self.remeasure_cell(id);
        cx.notify();
    }

    pub(crate) fn cell_label(&self, id: &str) -> String {
        self.doc
            .cell_by_id(id)
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| id.to_string())
    }

    pub(crate) fn input_line(&self, id: &str) -> super::InputLine {
        let (choice, _) = self.cell_connection(id);
        super::InputLine {
            cell_name: self.cell_label(id),
            connection: choice
                .as_ref()
                .map(|c| c.label.clone())
                .unwrap_or_else(|| text("notebook.shell.connection.none")),
            environment: choice
                .map(|c| c.environment)
                .unwrap_or(sqlly_core::ConnectionEnvironment::Unset),
        }
    }

    /// The local cells reading `id`'s relation, used for input badges.
    pub(crate) fn local_input_cells(&self, id: &str) -> Vec<String> {
        self.doc
            .local_inputs(id)
            .map(|inputs| inputs.into_iter().map(|i| i.cell_id).collect())
            .unwrap_or_default()
    }

    // ── Prompts ───────────────────────────────────────────────────────────

    pub(crate) fn confirm_local_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.local.prompt.take() else {
            return;
        };
        match prompt {
            LocalPrompt::Consent {
                profile_id, then, ..
            } => {
                self.local.prefs.materialize_allowed.insert(profile_id);
                self.local.prefs.save();
                self.resume(then, cx);
            }
            LocalPrompt::Inputs { cell_id, .. } => {
                self.local.inputs_approved.insert(cell_id.clone());
                self.resume(PendingAction::RunLocal { cell_id }, cx);
            }
            LocalPrompt::Name { cell_id } => {
                let name = self
                    .local_name_input
                    .as_ref()
                    .map(|i| i.read(cx).value().to_string())
                    .unwrap_or_default();
                self.local_name_input = None;
                self.materialize_named(&cell_id, &name, cx);
            }
            LocalPrompt::Refetch { cell_id, relation } => {
                self.copy_cell_now(&cell_id, &relation, true, cx);
            }
            LocalPrompt::Clear => {
                #[cfg(not(target_arch = "wasm32"))]
                self.clear_local_data(cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn cancel_local_prompt(&mut self, cx: &mut Context<Self>) {
        let waiting = match self.local.prompt.take() {
            Some(LocalPrompt::Consent {
                then: PendingAction::RunLocal { cell_id },
                ..
            })
            | Some(LocalPrompt::Inputs { cell_id, .. }) => Some(cell_id),
            _ => None,
        };
        self.local_name_input = None;
        if let Some(id) = waiting
            && self.doc.status(&id) == CellStatus::Queued
        {
            self.doc.apply(
                &id,
                CellEvent::Canceled {
                    server_confirmed: true,
                },
            );
            if let Some(rt) = self.cells.get_mut(&id) {
                rt.message = None;
            }
            self.remeasure_cell(&id);
            self.finish_step(&id, false, cx);
        }
        cx.notify();
    }

    fn resume(&mut self, action: PendingAction, cx: &mut Context<Self>) {
        match action {
            PendingAction::RunLocal { cell_id } => {
                if self.doc.status(&cell_id) == CellStatus::Queued {
                    self.prepare_local_run(&cell_id, cx);
                }
            }
            PendingAction::Materialize { cell_id } => self.request_materialize(&cell_id, cx),
        }
    }

    // ── Materialize Locally ───────────────────────────────────────────────

    /// "Materialize locally as …" from a cell's menu: check the connection's
    /// opt-in, then ask for the relation name.
    pub(crate) fn request_materialize(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.doc.cell_by_id(id).is_some_and(|c| c.is_sql()) {
            return;
        }
        if self.live_trust().level == sqlly_services::notebook_security::TrustLevel::Untrusted {
            self.trust_flash = true;
            self.status_message(text("notebook.shell.trust.run_blocked"), true, cx);
            return;
        }
        match self.gate_materialize(
            id,
            PendingAction::Materialize {
                cell_id: id.to_string(),
            },
            cx,
        ) {
            Ok(()) => {}
            Err(Gate::Prompted) => return,
            Err(Gate::Denied(message)) => return self.status_message(message, true, cx),
        }
        self.local_name_input = None;
        self.local.prompt = Some(LocalPrompt::Name {
            cell_id: id.to_string(),
        });
        cx.notify();
    }

    fn materialize_named(&mut self, id: &str, name: &str, cx: &mut Context<Self>) {
        let name = sqlly_services::notebook::cells::local::sanitize_relation(name.trim());
        #[cfg(not(target_arch = "wasm32"))]
        if let Err(error) = sqlly_services::local_analytics::validate_relation_name(&name) {
            let message = crate::localization::message(
                sqlly_services::local_analytics::error_message(&error),
            );
            return self.status_message(message, true, cx);
        }
        if self.doc.set_materialized_as(id, Some(&name)) {
            self.mark_dirty_from_local(cx);
        }
        let whole = self.whole_snapshot(id, cx).is_some();
        if whole {
            self.copy_cell_now(id, &name, false, cx);
        } else {
            self.local.prompt = Some(LocalPrompt::Refetch {
                cell_id: id.to_string(),
                relation: name,
            });
            cx.notify();
        }
    }

    /// Copy `id`'s output into `relation` now (Materialize Locally).
    fn copy_cell_now(&mut self, id: &str, relation: &str, requery: bool, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (id, relation, requery);
            let message = crate::localization::message(
                sqlly_services::local_analytics::unavailable_message(),
            );
            self.status_message(message, true, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let source = if self.doc.targets_local(id) {
                InputSource::LocalSql
            } else {
                InputSource::Remote
            };
            let input = LocalInput {
                cell_id: id.to_string(),
                relation: relation.to_string(),
                source,
            };
            self.with_workspace(cx, move |this, ws, cx| match ws {
                Ok(ws) => this.copy_inputs(None, vec![input], requery, ws, cx),
                Err(message) => this.status_message(message, true, cx),
            });
        }
    }

    /// `id`'s current output from the notebook's output registry, when it
    /// holds every row of the result with full values (not capped, not
    /// shortened to a preview, not cut by the cell's row limit).
    pub(crate) fn whole_snapshot(
        &self,
        id: &str,
        cx: &App,
    ) -> Option<Arc<sqlly_services::notebook::CellOutput>> {
        use sqlly_services::notebook::Completeness;
        if !self.doc.cell_state(id).is_presentable_as_current() || self.truncation(id).is_some() {
            return None;
        }
        let output = self.outputs.get(id)?;
        let whole = match output.completeness {
            Completeness::Complete => true,
            Completeness::Spilled { total } => output.rows.len() as u64 == total,
            _ => false,
        };
        let shortened = self
            .cells
            .get(id)
            .and_then(|rt| rt.results.as_ref())
            .and_then(|r| {
                r.read(cx)
                    .result_set_at(0)
                    .map(|set| !set.truncated.is_empty())
            })
            .unwrap_or(false);
        (whole && !shortened).then_some(output)
    }

    pub(crate) fn status_message(&mut self, message: String, error: bool, cx: &mut Context<Self>) {
        cx.emit(crate::notebook::view::NotebookEvent::Status { message, error });
        cx.notify();
    }

    // ── Results, caching, completion ──────────────────────────────────────

    /// A cell's run completed: remember it, keep an encrypted copy when the
    /// cell asks for one, and refresh local data after local work.
    pub(crate) fn after_cell_completed(
        &mut self,
        id: &str,
        run_id: &str,
        fingerprint: sqlly_services::notebook::CellFingerprint,
        cx: &mut Context<Self>,
    ) {
        self.local
            .last_runs
            .insert(id.to_string(), (run_id.to_string(), chrono::Utc::now()));
        self.local.fetch_all.remove(id);
        self.local.restored.remove(id);
        let settings = self.doc.cache_settings(id);
        if settings.storage == CacheStorage::EncryptedDisk
            && let Some(identity) = self.doc.connection_identity(id)
            && let Some(published) = self.outputs.get(id)
        {
            let mut output = (*published).clone();
            if let Some(truncation) = self.truncation(id) {
                output.completeness = sqlly_services::notebook::Completeness::Truncated {
                    cap: truncation.rows_kept,
                };
            }
            let key = super::cache::CacheKey {
                identity,
                fingerprint: fingerprint.value.0.clone(),
            };
            let policy = self.doc.cell_by_id(id).map(|c| c.policy.cache);
            let options = super::cache::StoreOptions {
                ttl_seconds: match policy {
                    Some(sqlly_services::notebook::CachePolicy::Ttl { seconds }) => Some(seconds),
                    _ => None,
                },
                pinned: policy == Some(sqlly_services::notebook::CachePolicy::Pinned),
            };
            if let Err(error) = self
                .cache_backend(CacheStorage::EncryptedDisk)
                .store(&key, &output, options)
            {
                self.local.error = Some(error);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.needs_local_preparation(id) {
            self.refresh_local_data(cx);
        }
    }

    /// The backend a storage choice uses; an encrypted store that cannot
    /// open (no keychain) falls back to memory and says why.
    pub(crate) fn cache_backend(
        &mut self,
        storage: CacheStorage,
    ) -> Rc<dyn super::cache::CacheBackend> {
        match storage {
            CacheStorage::Memory => Rc::new(super::cache::MemoryCache),
            CacheStorage::EncryptedDisk => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    if let Some(cache) = &self.encrypted_cache {
                        return cache.clone();
                    }
                    match super::cache::EncryptedCache::open_default() {
                        Ok(cache) => {
                            let cache: Rc<dyn super::cache::CacheBackend> = Rc::new(cache);
                            self.encrypted_cache = Some(cache.clone());
                            cache
                        }
                        Err(error) => {
                            self.local.error = Some(error);
                            Rc::new(super::cache::MemoryCache)
                        }
                    }
                }
                #[cfg(target_arch = "wasm32")]
                {
                    Rc::new(super::cache::MemoryCache)
                }
            }
        }
    }

    /// Show a cached output for `id` in `results` when its cache storage is
    /// encrypted disk and a copy for its current inputs and sign-in exists.
    pub(crate) fn restore_cached_output(
        &mut self,
        id: &str,
        results: &Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        self.restore_cached(id, cx);
        #[cfg(target_arch = "wasm32")]
        let _ = results;
        #[cfg(not(target_arch = "wasm32"))]
        self.show_restored(id, results, cx);
    }

    /// Fill `results` with `id`'s restored cached output.
    #[cfg(not(target_arch = "wasm32"))]
    fn show_restored(&mut self, id: &str, results: &Entity<ResultsView>, cx: &mut Context<Self>) {
        if !self.local.restored.contains(id) {
            return;
        }
        let (Some(output), Some(at)) = (self.outputs.get(id), self.local.restored_at.get(id))
        else {
            return;
        };
        let at = chrono::DateTime::from_timestamp_millis(*at)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default();
        let banner =
            crate::localization::format("notebook.local.cache.restored_banner", &[("time", at)]);
        let set = crate::query_version_history::SnapshotSet {
            columns: output.columns.clone(),
            rows: output.rows.as_ref().clone(),
            rows_affected: None,
        };
        results.update(cx, |r, cx| r.set_historical_results(banner, vec![set], cx));
    }

    /// Every cell kept encrypted on disk gets its saved result back when
    /// the notebook opens, so charts, KPIs and checks over it draw without
    /// a run. Trusted notebooks only.
    pub fn restore_cached_outputs(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .doc
            .cells()
            .filter(|c| c.kind == "sql")
            .map(|c| c.id.to_string())
            .collect();
        for id in ids {
            self.restore_cached(&id, cx);
        }
    }

    /// Restore `id`'s cached output into the notebook (state, registry,
    /// dependents) when its storage is encrypted disk, the notebook is
    /// trusted, and the cell has no output and is not running.
    fn restore_cached(&mut self, id: &str, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.doc.cache_settings(id).storage != CacheStorage::EncryptedDisk
                || self.trust.level == sqlly_services::notebook_security::TrustLevel::Untrusted
                || self.doc.cell_state(id).output.is_some()
                || self.doc.status(id).is_active()
            {
                return;
            }
            let (Some(identity), Some(fingerprint)) = (
                self.doc.connection_identity(id),
                self.doc.cell_fingerprint(id),
            ) else {
                return;
            };
            let key = super::cache::CacheKey {
                identity,
                fingerprint: fingerprint.value.0.clone(),
            };
            let Some(cached) = self.cache_backend(CacheStorage::EncryptedDisk).load(&key) else {
                return;
            };
            let run_id = cached.output.run_id.clone();
            self.doc.restore_cached(
                id,
                &run_id,
                fingerprint,
                cached.output.row_count,
                cached.output.completeness,
            );
            // Cells reading this one see the restored rows, labelled cached
            // and only as complete as the copy that was kept.
            let mut output = cached.output;
            output.freshness = sqlly_services::notebook::Freshness::TtlCached;
            self.outputs.publish(id, output);
            self.local.restored.insert(id.to_string());
            self.local
                .restored_at
                .insert(id.to_string(), cached.stored_at_ms);
            self.remeasure_cell(id);
            self.output_published(id, cx);
            cx.notify();
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (id, cx);
        }
    }

    /// Forget cached outputs fetched through `profile_id` (it disconnected)
    /// for cells that ask for that.
    pub fn connection_disconnected(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let wants = self.doc.cells().any(|c| {
            let settings = self.doc.cache_settings(c.id);
            settings.clear_on_disconnect
                && self
                    .doc
                    .connection_identity(c.id)
                    .is_some_and(|i| i.profile_id == profile_id)
        });
        if wants
            && let Err(error) = self
                .cache_backend(CacheStorage::EncryptedDisk)
                .clear_profile(profile_id)
        {
            self.local.error = Some(error);
        }
        cx.notify();
    }

    pub(crate) fn clear_cached_outputs(&mut self, cx: &mut Context<Self>) {
        match self.cache_backend(CacheStorage::EncryptedDisk).clear_all() {
            Ok(n) => {
                let message = crate::localization::format(
                    if n == 1 {
                        "notebook.local.cache.cleared_one"
                    } else {
                        "notebook.local.cache.cleared"
                    },
                    &[("count", n)],
                );
                self.status_message(message, false, cx);
            }
            Err(error) => self.status_message(error, true, cx),
        }
    }

    // ── Jobs ──────────────────────────────────────────────────────────────

    pub(crate) fn cancel_local_jobs(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        for job in &self.local.jobs {
            job.cancel.cancel();
        }
    }

    pub(crate) fn cancel_local_job(&mut self, job_id: u64, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(job) = self.local.jobs.iter().find(|j| j.id == job_id) {
            job.cancel.cancel();
        }
        #[cfg(target_arch = "wasm32")]
        let _ = job_id;
        cx.notify();
    }

    pub(crate) fn mark_dirty_from_local(&mut self, cx: &mut Context<Self>) {
        self.mark_dirty(cx);
    }

    /// Complete a local step that ran no query (an image attachment).
    pub(crate) fn complete_without_query(&mut self, id: &str, rows: u64, cx: &mut Context<Self>) {
        let rt = self.cells.entry(id.to_string()).or_default();
        let run_id = rt.run_id.clone().unwrap_or_else(|| {
            sqlly_services::notebook::run_record::new_run_id(
                &mut sqlly_services::notebook::RandomIds,
            )
        });
        self.doc.apply(
            id,
            CellEvent::Started {
                run_id: run_id.clone(),
            },
        );
        if let Some(fingerprint) = self.doc.cell_fingerprint(id) {
            self.doc.complete(id, &run_id, fingerprint.clone(), rows);
            self.after_cell_completed(id, &run_id, fingerprint, cx);
        }
        if let Some(rt) = self.cells.get_mut(id) {
            rt.message = None;
            rt.finished_at = Some(chrono::Local::now().format("%H:%M:%S").to_string());
        }
        self.remeasure_cell(id);
        self.finish_step(id, true, cx);
    }

    pub(crate) fn log_local(&mut self, cell: Option<&str>, message: String, error: bool) {
        self.log(cell, message, error, None);
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::path::{Path, PathBuf};

    use sqlly_services::local_analytics::{
        CancelToken, ExportFormat, ExportTarget, ImportFormat, ImportSpec, NotebookWorkspace,
        OpenOptions, SourceInfo, WriteMode, error_message, source_sql_hash,
    };
    use sqlly_services::notebook::cells::attachment::{AttachmentFormat, AttachmentSpec};
    #[cfg(not(test))]
    use sqlly_services::notebook_security::workspace_keys::KeyringWorkspaceKeys;
    #[cfg(test)]
    use sqlly_services::notebook_security::workspace_keys::MemoryWorkspaceKeys;
    use sqlly_services::notebook_security::workspace_keys::{
        WorkspaceKeyStore, generate_workspace_key,
    };

    use crate::localization::deferred_text;
    use sqlly_core::DisplayMessage;

    use super::super::materialize::{self, Progress};
    use super::super::{ImportSummary, LocalJob, provenance_view};

    /// How one input gets into local data. Short-lived, one per input.
    #[allow(clippy::large_enum_variant)]
    enum Copy {
        Snapshot {
            output: Box<sqlly_services::notebook::CellOutput>,
            source: SourceInfo,
        },
        Query {
            executor: Arc<dyn QueryExecutor>,
            profile: ConnectionProfile,
            sql: String,
            source: SourceInfo,
        },
        Local {
            sql: String,
            source: SourceInfo,
        },
    }

    /// An attachment's picture, checked to be a real PNG or JPEG under the
    /// size limits before it is decoded for drawing.
    fn read_attachment_image(
        path: &Path,
    ) -> Result<
        super::super::AttachmentImage,
        sqlly_services::notebook::cells::attachment::ImageRejection,
    > {
        use sqlly_services::notebook::cells::attachment::{
            ImageRejection, MAX_IMAGE_BYTES, image_dimensions, validate_image,
        };
        let too_large = std::fs::metadata(path)
            .map(|m| m.len() > MAX_IMAGE_BYTES)
            .unwrap_or(false);
        if too_large {
            return Err(ImageRejection::TooLarge {
                bytes: 0,
                limit: MAX_IMAGE_BYTES,
            });
        }
        let bytes = std::fs::read(path).map_err(|_| ImageRejection::NotAnImage)?;
        let format = validate_image(&bytes)?;
        let (width, height) = image_dimensions(&bytes).unwrap_or((0, 0));
        let gpui_format = match format {
            AttachmentFormat::Png => gpui::ImageFormat::Png,
            _ => gpui::ImageFormat::Jpeg,
        };
        Ok(super::super::AttachmentImage {
            image: Arc::new(gpui::Image::from_bytes(gpui_format, bytes)),
            width,
            height,
        })
    }

    fn store_message(error: &sqlly_services::local_analytics::LocalStoreError) -> String {
        crate::localization::message(error_message(error))
    }

    enum Switched {
        NotEmpty,
        Failed(DisplayMessage),
    }

    /// Recreate `ws` with or without encryption, keeping the keychain in
    /// step: the new key is stored before the plain file goes (and dropped
    /// again when the switch fails), the old key is forgotten only once the
    /// workspace is plain.
    async fn switch_encryption(
        analytics: Arc<sqlly_services::local_analytics::LocalAnalytics>,
        keys: Arc<dyn WorkspaceKeyStore>,
        ws: NotebookWorkspace,
        scope: String,
        on: bool,
    ) -> Result<NotebookWorkspace, Switched> {
        use sqlly_services::local_analytics::{EncryptionKey, LocalStoreError};
        let failed = |error: LocalStoreError| match error {
            LocalStoreError::WorkspaceNotEmpty(_) => Switched::NotEmpty,
            other => Switched::Failed(error_message(&other)),
        };
        let options = if on {
            let key = generate_workspace_key();
            keep_key(keys.clone(), scope.clone(), Some(key.clone()))
                .await
                .map_err(Switched::Failed)?;
            OpenOptions {
                encryption_key: Some(EncryptionKey::new(key)),
                ..OpenOptions::default()
            }
        } else {
            OpenOptions::default()
        };
        match analytics.recreate(ws, options).await {
            Ok(ws) => {
                if !on {
                    keep_key(keys, scope, None)
                        .await
                        .map_err(Switched::Failed)?;
                }
                Ok(ws)
            }
            Err(error) => {
                if on {
                    let _ = keep_key(keys, scope, None).await;
                }
                Err(failed(error))
            }
        }
    }

    /// Store `key` as `scope`'s workspace key, or forget it with `None`.
    async fn keep_key(
        keys: Arc<dyn WorkspaceKeyStore>,
        scope: String,
        key: Option<String>,
    ) -> Result<(), DisplayMessage> {
        tokio::task::spawn_blocking(move || match key {
            Some(key) => keys.store(&scope, &key),
            None => keys.remove(&scope),
        })
        .await
        .map_err(|e| DisplayMessage::raw(e.to_string()))?
        .map_err(|e| e.message())
    }

    impl NotebookView {
        /// Run `then` with the notebook's workspace, opening it first.
        pub(crate) fn with_workspace(
            &mut self,
            cx: &mut Context<Self>,
            then: impl FnOnce(&mut NotebookView, Result<NotebookWorkspace, String>, &mut Context<Self>)
            + 'static,
        ) {
            if let Some(ws) = self.local.workspace.clone() {
                return then(self, Ok(ws), cx);
            }
            // Local data belongs to the trust record, not to the id a file
            // declares: an untrusted notebook opens none.
            let Some(scope) = self.trust.scope.clone() else {
                return then(self, Err(text("notebook.shell.trust.run_blocked")), cx);
            };
            self.local.waiters.push(Box::new(then));
            // An encryption switch has the workspace closed: opening it now
            // would keep the old file open under the switch, which then
            // refuses. Waiters get the new workspace when the switch ends.
            if self.local.opening || self.local.switching_encryption {
                return;
            }
            self.open_workspace(scope, cx);
        }

        fn open_workspace(&mut self, scope: String, cx: &mut Context<Self>) {
            self.local.opening = true;
            let analytics = analytics(&self.local.prefs);
            let keys = self.workspace_keys();
            let notebook_id = scope;
            cx.spawn(async move |this, cx| {
                let opened = on_tokio(cx, async move {
                    analytics.open_with_keys(&notebook_id, keys).await
                })
                .await;
                let result = match opened {
                    Some(Ok(ws)) => Ok(ws),
                    Some(Err(error)) => Err(store_message(&error)),
                    None => Err(text("notebook.local.workspace.open_failed")),
                };
                this.update(cx, |this, cx| {
                    this.local.opening = false;
                    if let Ok(ws) = &result {
                        this.local.workspace = Some(ws.clone());
                        this.local.encrypted = ws.workspace().is_encrypted();
                    }
                    for waiter in std::mem::take(&mut this.local.waiters) {
                        waiter(this, result.clone(), cx);
                    }
                    this.refresh_local_data(cx);
                })
                .ok();
            })
            .detach();
        }

        /// Hand the workspace to what asked for it during a switch, opening
        /// it when the switch left it closed.
        fn serve_workspace_waiters(&mut self, cx: &mut Context<Self>) {
            if self.local.waiters.is_empty() || self.local.opening {
                return;
            }
            match (self.local.workspace.clone(), self.trust.scope.clone()) {
                (Some(ws), _) => {
                    for waiter in std::mem::take(&mut self.local.waiters) {
                        waiter(self, Ok(ws.clone()), cx);
                    }
                }
                (None, Some(scope)) => self.open_workspace(scope, cx),
                (None, None) => {
                    let blocked = text("notebook.shell.trust.run_blocked");
                    for waiter in std::mem::take(&mut self.local.waiters) {
                        waiter(self, Err(blocked.clone()), cx);
                    }
                }
            }
        }

        /// Where this notebook's workspace key is kept.
        pub(crate) fn workspace_keys(&self) -> Arc<dyn WorkspaceKeyStore> {
            if let Some(keys) = &self.local.keys {
                return keys.clone();
            }
            #[cfg(test)]
            {
                static KEYS: std::sync::OnceLock<Arc<MemoryWorkspaceKeys>> =
                    std::sync::OnceLock::new();
                KEYS.get_or_init(Default::default).clone()
            }
            #[cfg(not(test))]
            Arc::new(KeyringWorkspaceKeys::default())
        }

        /// Turn encryption of this notebook's local data on or off. A
        /// DuckDB file can't be encrypted in place, so the workspace is
        /// recreated, which is only allowed while it holds no tables; with
        /// tables the rail asks to clear local data first. Turning it on
        /// keeps a new key in the system keychain under the notebook's data
        /// scope; turning it off forgets it.
        pub(crate) fn set_local_encryption(&mut self, on: bool, cx: &mut Context<Self>) {
            if self.local.switching_encryption || on == self.local.encrypted {
                return;
            }
            let Some(scope) = self.trust.scope.clone() else {
                return self.status_message(
                    text("notebook.fu_final.encryption.untrusted"),
                    true,
                    cx,
                );
            };
            if !self.local.relations.is_empty() || !self.local.jobs.is_empty() {
                self.local.encryption_needs_clear = true;
                cx.notify();
                return;
            }
            let Some(ws) = self.local.workspace.take() else {
                return self.with_workspace(cx, move |this, ws, cx| match ws {
                    Ok(ws) if ws.workspace().is_encrypted() != on => {
                        this.set_local_encryption(on, cx)
                    }
                    Ok(_) => cx.notify(),
                    Err(message) => {
                        this.local.error = Some(message);
                        cx.notify();
                    }
                });
            };
            self.local.switching_encryption = true;
            self.local.encryption_needs_clear = false;
            self.local.error = None;
            let analytics = analytics(&self.local.prefs);
            let keys = self.workspace_keys();
            cx.notify();
            cx.spawn(async move |this, cx| {
                let switched = on_tokio(cx, switch_encryption(analytics, keys, ws, scope, on))
                    .await
                    .unwrap_or_else(|| {
                        Err(Switched::Failed(crate::localization::deferred_text(
                            "notebook.local.workspace.open_failed",
                        )))
                    });
                this.update(cx, |this, cx| {
                    this.local.switching_encryption = false;
                    match switched {
                        Ok(ws) => {
                            this.local.encrypted = ws.workspace().is_encrypted();
                            this.local.workspace = Some(ws);
                            this.status_message(
                                text(if this.local.encrypted {
                                    "notebook.fu_final.encryption.turned_on"
                                } else {
                                    "notebook.fu_final.encryption.turned_off"
                                }),
                                false,
                                cx,
                            );
                            this.refresh_local_data(cx);
                        }
                        Err(Switched::NotEmpty) => {
                            this.local.encryption_needs_clear = true;
                            this.ensure_local_data_loaded(cx);
                        }
                        Err(Switched::Failed(message)) => {
                            this.local.error = Some(crate::localization::message(message));
                            this.ensure_local_data_loaded(cx);
                        }
                    }
                    this.serve_workspace_waiters(cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        /// Re-read the workspace's relations, provenance and size.
        pub(crate) fn refresh_local_data(&mut self, cx: &mut Context<Self>) {
            self.refresh_local_data_then(cx, |_, _| {});
        }

        /// [`Self::refresh_local_data`], then `then` once the rows are in.
        fn refresh_local_data_then(
            &mut self,
            cx: &mut Context<Self>,
            then: impl FnOnce(&mut NotebookView, &mut Context<Self>) + 'static,
        ) {
            let Some(ws) = self.local.workspace.clone() else {
                return then(self, cx);
            };
            cx.spawn(async move |this, cx| {
                let read = on_tokio(cx, async move {
                    let relations = ws.relations().await?;
                    let disk = ws.disk_usage().await?;
                    Ok::<_, sqlly_services::local_analytics::LocalStoreError>((relations, disk))
                })
                .await;
                this.update(cx, |this, cx| {
                    match read {
                        Some(Ok((relations, disk))) => {
                            this.local.relations = relations
                                .iter()
                                .map(|r| super::super::RelationRow {
                                    name: r.name.clone(),
                                    is_view: r.is_view,
                                    rows: r.row_count,
                                    columns: r.column_count,
                                    bytes: r.storage_bytes,
                                    provenance: r.provenance.as_ref().map(provenance_view),
                                })
                                .collect();
                            this.local.disk_bytes = disk;
                            this.apply_local_completion(cx);
                        }
                        Some(Err(error)) => this.local.error = Some(store_message(&error)),
                        None => {}
                    }
                    then(this, cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        /// Open the workspace for the Local data page when it is shown.
        pub(crate) fn ensure_local_data_loaded(&mut self, cx: &mut Context<Self>) {
            if self.local.workspace.is_none()
                && !self.local.opening
                && !self.local.switching_encryption
            {
                self.with_workspace(cx, |this, ws, cx| {
                    if let Err(message) = ws {
                        this.local.error = Some(message);
                        cx.notify();
                    }
                });
            }
        }

        pub(crate) fn new_job(
            &mut self,
            cell_id: Option<&str>,
            relation: &str,
        ) -> (u64, Arc<Progress>, CancelToken) {
            self.local.next_job += 1;
            let id = self.local.next_job;
            let progress = Arc::new(Progress::default());
            let cancel = CancelToken::new();
            self.local.jobs.push(LocalJob {
                id,
                cell_id: cell_id.map(str::to_string),
                relation: relation.to_string(),
                progress: Arc::clone(&progress),
                cancel: cancel.clone(),
            });
            (id, progress, cancel)
        }

        /// How `input` gets copied: its whole output when it is on screen,
        /// else its query again (always when `requery`); a local input is
        /// recomputed from local data.
        fn plan_copy(
            &self,
            input: &LocalInput,
            requery: bool,
            cx: &App,
        ) -> Result<CopyNeeds, String> {
            match input.source {
                InputSource::LocalSql => {
                    let sql = self.doc.sql_for_run(&input.cell_id)?;
                    let upstream = self
                        .doc
                        .local_inputs(&input.cell_id)?
                        .into_iter()
                        .map(|i| i.relation)
                        .collect();
                    let run_id = self
                        .local
                        .last_runs
                        .get(&input.cell_id)
                        .map(|(r, _)| r.clone());
                    let mut source = materialize::local_source(&input.cell_id, run_id, upstream);
                    source.sql_hash = Some(source_sql_hash(&sql));
                    Ok(CopyNeeds::Ready(Box::new(Copy::Local { sql, source })))
                }
                InputSource::Remote => {
                    let mut request = self.run_request(&input.cell_id)?;
                    let snapshot = (!requery)
                        .then(|| self.whole_snapshot(&input.cell_id, cx))
                        .flatten();
                    if snapshot.is_none()
                        && (!request.target.read_only || self.doc.writes(&input.cell_id))
                    {
                        return Err(crate::localization::format(
                            "notebook.local.run.write_cell_not_rerun",
                            &[("cell_name", self.cell_label(&input.cell_id))],
                        ));
                    }
                    request.target.read_only = true;
                    let last = self.local.last_runs.get(&input.cell_id).cloned();
                    Ok(CopyNeeds::Profile {
                        target: request.target,
                        sql: request.sql,
                        snapshot: snapshot.map(|s| (*s).clone()),
                        last,
                    })
                }
                InputSource::Attachment => Err(crate::localization::format(
                    "notebook.local.run.attachment_not_imported",
                    &[("cell_name", self.cell_label(&input.cell_id))],
                )),
                InputSource::Other => Err(crate::localization::format(
                    "notebook.local.run.unsupported_input",
                    &[("cell_name", self.cell_label(&input.cell_id))],
                )),
            }
        }

        /// Copy `inputs` in order, then hand `then` (a local cell) to the host.
        pub(crate) fn copy_inputs(
            &mut self,
            then: Option<String>,
            inputs: Vec<LocalInput>,
            requery: bool,
            ws: NotebookWorkspace,
            cx: &mut Context<Self>,
        ) {
            if inputs.is_empty() {
                if let Some(id) = then {
                    self.dispatch(&id, cx);
                }
                return;
            }
            if inputs.iter().any(|i| i.source == InputSource::Attachment) {
                let missing = inputs
                    .iter()
                    .find(|i| i.source == InputSource::Attachment)
                    .map(|i| self.cell_label(&i.cell_id))
                    .unwrap_or_default();
                let message = crate::localization::format(
                    "notebook.local.run.attachment_not_imported",
                    &[("cell_name", missing)],
                );
                match then {
                    Some(id) => return self.fail_started(&id, message, cx),
                    None => return self.status_message(message, true, cx),
                }
            }
            self.layout.activity_visible = true;
            self.tick_local_jobs(cx);
            cx.spawn(async move |this, cx| {
                for input in inputs {
                    let needs = this
                        .update(cx, |this, cx| this.plan_copy(&input, requery, cx))
                        .unwrap_or_else(|_| Err(String::new()));
                    let copy = match needs {
                        Ok(CopyNeeds::Ready(copy)) => Ok(*copy),
                        Ok(CopyNeeds::Profile {
                            target,
                            sql,
                            snapshot,
                            last,
                        }) => {
                            let resolver = this
                                .read_with(cx, |this, _| this.local.resolver.clone())
                                .ok()
                                .flatten();
                            match resolver {
                                None => Err(text("notebook.local.materialize.no_host")),
                                Some(resolver) => cx
                                    .update(|cx| resolver(&target, cx))
                                    .map(|(profile, executor)| {
                                        let mut source = SourceInfo::from_profile(&profile);
                                        source.sql_hash = Some(source_sql_hash(&sql));
                                        source.cell_id = Some(input.cell_id.clone());
                                        match snapshot {
                                            Some(output) => {
                                                if let Some((run_id, at)) = last {
                                                    source.run_id = Some(run_id);
                                                    source.extracted_at = Some(at.to_rfc3339());
                                                }
                                                Copy::Snapshot {
                                                    output: Box::new(output),
                                                    source,
                                                }
                                            }
                                            None => {
                                                source.run_id = Some(
                                                    sqlly_services::notebook::run_record::new_run_id(
                                                        &mut sqlly_services::notebook::RandomIds,
                                                    ),
                                                );
                                                Copy::Query {
                                                    executor,
                                                    profile,
                                                    sql,
                                                    source,
                                                }
                                            }
                                        }
                                    }),
                            }
                        }
                        Err(message) => Err(message),
                    };
                    let copy = match copy {
                        Ok(copy) => copy,
                        Err(message) => {
                            this.update(cx, |this, cx| this.copy_failed(then.as_deref(), message, cx))
                                .ok();
                            return;
                        }
                    };
                    let Ok((job, progress, cancel)) = this.update(cx, |this, cx| {
                        cx.notify();
                        this.new_job(Some(&input.cell_id), &input.relation)
                    }) else {
                        return;
                    };
                    let ws = ws.clone();
                    let relation = input.relation.clone();
                    let outcome = on_tokio(cx, async move {
                        match copy {
                            Copy::Snapshot { output, source } => {
                                materialize::from_snapshot(&ws, &relation, &output, source, cancel, progress)
                                    .await
                            }
                            Copy::Query {
                                executor,
                                profile,
                                sql,
                                source,
                            } => {
                                materialize::from_query(
                                    &ws, executor, profile, None, sql, &relation, source, cancel,
                                    progress,
                                )
                                .await
                            }
                            Copy::Local { sql, source } => {
                                materialize::from_local_query(&ws, sql, &relation, source, cancel, progress)
                                    .await
                            }
                        }
                    })
                    .await
                    .unwrap_or_else(|| {
                        Err(crate::localization::deferred_text(
                            "notebook.local.materialize.canceled",
                        ))
                    });
                    let ok = this
                        .update(cx, |this, cx| {
                            this.local.jobs.retain(|j| j.id != job);
                            match outcome {
                                Ok(outcome) => {
                                    let mut message = crate::localization::format(
                                        if outcome.row_count == 1 {
"notebook.local.materialize.done_one"
} else {
"notebook.local.materialize.done"
},
                                        &[
                                            ("relation", outcome.relation.clone()),
                                            ("count", super::super::count_text(outcome.row_count)),
                                        ],
                                    );
                                    if let Some(reason) = &outcome.partial {
                                        message.push(' ');
                                        message.push_str(&super::super::partial_text(reason));
                                    }
                                    this.log_local(Some(&input.cell_id), message, outcome.partial.is_some());
                                    true
                                }
                                Err(message) => {
                                    let message = crate::localization::message(message);
                                    this.copy_failed(then.as_deref(), message, cx);
                                    false
                                }
                            }
                        })
                        .unwrap_or(false);
                    if !ok {
                        return;
                    }
                }
                this.update(cx, |this, cx| {
                    this.refresh_local_data(cx);
                    if let Some(id) = then {
                        if this.doc.status(&id) == CellStatus::Queued {
                            if let Some(rt) = this.cells.get_mut(&id) {
                                rt.message = None;
                            }
                            this.dispatch(&id, cx);
                        } else {
                            this.finish_step(&id, false, cx);
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        fn copy_failed(&mut self, then: Option<&str>, message: String, cx: &mut Context<Self>) {
            self.refresh_local_data(cx);
            match then {
                Some(id) => self.fail_started(id, message, cx),
                None => {
                    self.log_local(None, message.clone(), true);
                    self.status_message(message, true, cx);
                }
            }
        }

        // ── Attachments ───────────────────────────────────────────────────

        fn notebook_dir(&self) -> Option<PathBuf> {
            self.file_path
                .as_deref()
                .and_then(|p| Path::new(p).parent())
                .map(Path::to_path_buf)
        }

        /// The file an attachment reads, if this machine may read it.
        fn attachment_path(&self, spec: &AttachmentSpec) -> Result<PathBuf, DisplayMessage> {
            if !spec.has_file() {
                return Err(deferred_text("notebook.local.attachment.no_file"));
            }
            let dir = self.notebook_dir();
            let Some(path) = spec.resolve(dir.as_deref()) else {
                return Err(deferred_text(if spec.path.is_some() && dir.is_none() {
                    "notebook.local.attachment.save_first"
                } else {
                    "notebook.local.attachment.outside_folder"
                }));
            };
            if spec.path.is_none()
                && !self.local.prefs.is_file_approved(
                    self.trust.scope.as_deref().unwrap_or_default(),
                    &path.to_string_lossy(),
                )
            {
                return Err(deferred_text("notebook.local.attachment.approve_again"));
            }
            Ok(path)
        }

        /// Pick an attachment's file with the system file dialog.
        pub(crate) fn choose_attachment_file(&mut self, id: &str, cx: &mut Context<Self>) {
            let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: None,
            });
            let id = id.to_string();
            cx.spawn(async move |this, cx| {
                let Ok(Ok(Some(paths))) = rx.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let hashed = on_tokio(cx, {
                    let path = path.clone();
                    async move {
                        tokio::task::spawn_blocking(move || {
                            sqlly_services::notebook::cells::attachment::file_hash(&path)
                        })
                        .await
                        .ok()
                    }
                })
                .await
                .flatten();
                this.update(cx, |this, cx| match hashed {
                    Some(Ok((hash, size))) => this.set_attachment_file(&id, &path, hash, size, cx),
                    Some(Err(error)) => this.status_message(error.to_string(), true, cx),
                    None => {}
                })
                .ok();
            })
            .detach();
        }

        /// Record the user's pick: the path (relative to the notebook when
        /// inside its folder), its hash, and this machine's approval.
        pub(crate) fn set_attachment_file(
            &mut self,
            id: &str,
            path: &Path,
            hash: String,
            size: u64,
            cx: &mut Context<Self>,
        ) {
            let Some(mut spec) = self.doc.attachment(id) else {
                return;
            };
            if AttachmentFormat::from_path(path).is_none() {
                return self.status_message(
                    text("notebook.local.attachment.unknown_format"),
                    true,
                    cx,
                );
            }
            let dir = self.notebook_dir();
            spec.set_file(path, dir.as_deref(), hash, size);
            if spec.relation.is_none() && !spec.is_image() {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("attachment");
                let stem = stem.split('.').next().unwrap_or(stem);
                spec.relation = Some(sqlly_services::notebook::cells::local::sanitize_relation(
                    stem,
                ));
            }
            if let (Some(abs), Some(scope)) = (&spec.absolute_path, self.trust.scope.as_deref()) {
                self.local.prefs.approve_file(scope, abs);
                self.local.prefs.save();
            }
            self.local.imports.remove(id);
            self.local.images.remove(id);
            self.local.image_loads.remove(id);
            if self.doc.set_attachment(id, &spec) {
                self.mark_dirty_from_local(cx);
            }
            self.remeasure_cell(id);
            cx.notify();
        }

        pub(crate) fn set_attachment_relation(
            &mut self,
            id: &str,
            name: &str,
            cx: &mut Context<Self>,
        ) {
            let Some(mut spec) = self.doc.attachment(id) else {
                return;
            };
            let name = name.trim();
            spec.relation = (!name.is_empty())
                .then(|| sqlly_services::notebook::cells::local::sanitize_relation(name));
            if self.doc.set_attachment(id, &spec) {
                self.mark_dirty_from_local(cx);
            }
        }

        pub(crate) fn set_attachment_table(
            &mut self,
            id: &str,
            table: &str,
            cx: &mut Context<Self>,
        ) {
            let Some(mut spec) = self.doc.attachment(id) else {
                return;
            };
            let table = table.trim();
            spec.table = (!table.is_empty()).then(|| table.to_string());
            if self.doc.set_attachment(id, &spec) {
                self.mark_dirty_from_local(cx);
            }
        }

        /// Run an attachment cell: show its image, or import its file (again
        /// when it changed) and preview the relation through the host.
        pub(crate) fn run_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
            let Some(spec) = self.doc.attachment(id) else {
                return self.fail_started(id, deferred_text("notebook.shell.run.cell_missing"), cx);
            };
            let path = match self.attachment_path(&spec) {
                Ok(path) => path,
                Err(message) => return self.fail_started(id, message, cx),
            };
            if spec.is_image() {
                return self.load_attachment_image(id, &path, cx);
            }
            let Some(relation) = spec.relation.clone() else {
                return self.fail_started(
                    id,
                    deferred_text("notebook.local.attachment.no_relation"),
                    cx,
                );
            };
            let format = match spec.format {
                Some(AttachmentFormat::Csv) => ImportFormat::Csv,
                Some(AttachmentFormat::Parquet) => ImportFormat::Parquet,
                Some(AttachmentFormat::Json) => ImportFormat::Json,
                Some(AttachmentFormat::Ndjson) => ImportFormat::NdJson,
                Some(AttachmentFormat::Sqlite) => match &spec.table {
                    Some(table) => ImportFormat::Sqlite {
                        table: table.clone(),
                    },
                    None => {
                        return self.fail_started(
                            id,
                            deferred_text("notebook.local.attachment.no_table"),
                            cx,
                        );
                    }
                },
                _ => {
                    return self.fail_started(
                        id,
                        deferred_text("notebook.local.attachment.unknown_format"),
                        cx,
                    );
                }
            };
            let run_id = self.cells.get(id).and_then(|rt| rt.run_id.clone());
            let id = id.to_string();
            self.with_workspace(cx, move |this, ws, cx| {
                let ws = match ws {
                    Ok(ws) => ws,
                    Err(message) => return this.fail_started(&id, message, cx),
                };
                let (job, progress, cancel) = this.new_job(Some(&id), &relation);
                this.tick_local_jobs(cx);
                let mut import = ImportSpec::new(relation.clone(), path.clone(), format);
                import.mode = WriteMode::Replace;
                import.cell_id = Some(id.clone());
                import.run_id = run_id;
                cx.spawn(async move |this, cx| {
                    let hash_path = path.clone();
                    let result = on_tokio(cx, async move {
                        let hashed = tokio::task::spawn_blocking(move || {
                            sqlly_services::notebook::cells::attachment::file_hash(&hash_path)
                        })
                        .await
                        .map_err(|e| DisplayMessage::raw(e.to_string()))?
                        .map_err(|e| DisplayMessage::raw(e.to_string()))?;
                        #[cfg(test)]
                        {
                            let held = super::SLOW_IMPORTS
                                .lock()
                                .unwrap()
                                .iter()
                                .find(|(p, _)| {
                                    p.canonicalize().ok() == import.path.canonicalize().ok()
                                })
                                .map_or(0, |(_, ms)| *ms);
                            tokio::time::sleep(std::time::Duration::from_millis(held)).await;
                        }
                        let outcome = ws
                            .import_file(import, cancel)
                            .await
                            .map_err(|e| error_message(&e))?;
                        Ok::<_, DisplayMessage>((hashed, outcome))
                    })
                    .await
                    .unwrap_or_else(|| {
                        Err(crate::localization::deferred_text(
                            "notebook.local.materialize.canceled",
                        ))
                    });
                    drop(progress);
                    this.update(cx, |this, cx| {
                        this.local.jobs.retain(|j| j.id != job);
                        match result {
                            Ok(((hash, size), outcome)) => {
                                let mut spec = this.doc.attachment(&id).unwrap_or_default();
                                let changed =
                                    spec.content_hash.as_deref().is_some_and(|old| old != hash);
                                if spec.content_hash.as_deref() != Some(hash.as_str()) {
                                    spec.content_hash = Some(hash);
                                    spec.size_bytes = Some(size);
                                    if this.doc.set_attachment(&id, &spec) {
                                        this.mark_dirty_from_local(cx);
                                        // The run imported these bytes, so it
                                        // completes under the spec that names them.
                                        let fingerprint = this.doc.cell_fingerprint(&id);
                                        if let Some(rt) = this.cells.get_mut(&id) {
                                            rt.run_fingerprint = fingerprint;
                                        }
                                    }
                                }
                                this.local.imports.insert(
                                    id.clone(),
                                    ImportSummary {
                                        relation: outcome.relation.clone(),
                                        rows: outcome.row_count,
                                        columns: outcome
                                            .columns
                                            .iter()
                                            .map(|c| {
                                                (
                                                    c.name.clone(),
                                                    c.local_type.clone(),
                                                    c.fell_back_to_text,
                                                )
                                            })
                                            .collect(),
                                        partial: outcome
                                            .partial
                                            .as_ref()
                                            .map(super::super::partial_text),
                                        changed,
                                    },
                                );
                                this.remeasure_cell(&id);
                                // The preview runs once the table is listed,
                                // so the cells after it (a profile reading
                                // the whole table) find it.
                                this.refresh_local_data_then(cx, move |this, cx| {
                                    if this.doc.status(&id) == CellStatus::Queued {
                                        this.dispatch(&id, cx);
                                    } else {
                                        this.finish_step(&id, false, cx);
                                    }
                                });
                            }
                            Err(message) => this.fail_started(&id, message, cx),
                        }
                    })
                    .ok();
                })
                .detach();
            });
        }

        fn load_attachment_image(&mut self, id: &str, path: &Path, cx: &mut Context<Self>) {
            use sqlly_services::notebook::cells::attachment::ImageRejection;
            match read_attachment_image(path) {
                Ok(image) => {
                    self.local.images.insert(id.to_string(), image);
                    self.complete_without_query(id, 0, cx);
                }
                Err(rejection) => {
                    let message = match rejection {
                        ImageRejection::TooLarge { limit, .. } => crate::localization::deferred(
                            "notebook.local.attachment.image_too_large",
                            &[("size", super::super::size_text(limit))],
                        ),
                        ImageRejection::NotAnImage => {
                            deferred_text("notebook.local.attachment.not_an_image")
                        }
                        ImageRejection::TooManyPixels { width, height } => {
                            crate::localization::deferred(
                                "notebook.local.attachment.image_too_big",
                                &[("width", width.to_string()), ("height", height.to_string())],
                            )
                        }
                    };
                    self.fail_started(id, message, cx);
                }
            }
        }

        /// Load attachment `id`'s picture for a Markdown cell that shows it,
        /// once: only in a trusted notebook, only from a file this machine
        /// may read, checked like the attachment cell's own run.
        pub(crate) fn load_markdown_image(&mut self, id: &str, cx: &mut Context<Self>) {
            if self.trust.scope.is_none() || !self.local.image_loads.insert(id.to_string()) {
                return;
            }
            let Some(spec) = self.doc.attachment(id).filter(|s| s.is_image()) else {
                return;
            };
            let Ok(path) = self.attachment_path(&spec) else {
                return;
            };
            let id = id.to_string();
            cx.spawn(async move |this, cx| {
                let read = cx
                    .background_executor()
                    .spawn(async move { read_attachment_image(&path) })
                    .await;
                this.update(cx, |this, cx| {
                    if let Ok(image) = read {
                        this.local.images.insert(id, image);
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }

        // ── Local data page actions ───────────────────────────────────────

        pub(crate) fn drop_local_relation(&mut self, name: &str, cx: &mut Context<Self>) {
            let Some(ws) = self.local.workspace.clone() else {
                return;
            };
            let name = name.to_string();
            cx.spawn(async move |this, cx| {
                let dropped = on_tokio(cx, async move { ws.drop_relation(name).await }).await;
                this.update(cx, |this, cx| {
                    if let Some(Err(error)) = dropped {
                        this.local.error = Some(store_message(&error));
                    }
                    this.refresh_local_data(cx);
                })
                .ok();
            })
            .detach();
        }

        /// Copy the cell a relation came from again (Refresh).
        pub(crate) fn refresh_local_relation(&mut self, name: &str, cx: &mut Context<Self>) {
            let Some(cell_id) = self
                .local
                .relation(name)
                .and_then(|r| r.provenance.as_ref())
                .and_then(|p| p.cell_id.clone())
                .filter(|id| self.doc.cell_by_id(id).is_some())
            else {
                return self.status_message(text("notebook.local.data.refresh_no_cell"), true, cx);
            };
            if self
                .doc
                .cell_by_id(&cell_id)
                .is_some_and(|c| c.kind == KIND_ATTACHMENT)
            {
                self.activate_cell(&cell_id, cx);
                return self.run_scope(
                    crate::notebook::doc_adapter::RunScope::Cell,
                    Some(cell_id),
                    cx,
                );
            }
            match self.gate_materialize(
                &cell_id,
                PendingAction::Materialize {
                    cell_id: cell_id.clone(),
                },
                cx,
            ) {
                Ok(()) => self.copy_cell_now(&cell_id, name, false, cx),
                Err(Gate::Prompted) => {}
                Err(Gate::Denied(message)) => self.status_message(message, true, cx),
            }
        }

        pub(crate) fn export_local_relation(
            &mut self,
            name: &str,
            format: ExportFormat,
            cx: &mut Context<Self>,
        ) {
            let Some(ws) = self.local.workspace.clone() else {
                return;
            };
            let ext = match format {
                ExportFormat::Parquet => "parquet",
                _ => "csv",
            };
            let hint = format!("{name}.{ext}");
            let rx = cx.prompt_for_new_path(Path::new("."), Some(&hint));
            let name = name.to_string();
            cx.spawn(async move |this, cx| {
                let Ok(Ok(Some(path))) = rx.await else {
                    return;
                };
                let exported = on_tokio(cx, async move {
                    ws.export(
                        ExportTarget::Relation(name),
                        format,
                        path,
                        true,
                        CancelToken::new(),
                    )
                    .await
                })
                .await;
                this.update(cx, |this, cx| match exported {
                    Some(Ok(outcome)) => {
                        let message = crate::localization::format(
                            if outcome.rows == 1 {
                                "notebook.local.data.exported_one"
                            } else {
                                "notebook.local.data.exported"
                            },
                            &[
                                ("count", super::super::count_text(outcome.rows)),
                                ("size", super::super::size_text(outcome.bytes_written)),
                            ],
                        );
                        this.status_message(message, false, cx);
                    }
                    Some(Err(error)) => this.status_message(store_message(&error), true, cx),
                    None => {}
                })
                .ok();
            })
            .detach();
        }

        pub(crate) fn clear_local_data(&mut self, cx: &mut Context<Self>) {
            let Some(ws) = self.local.workspace.clone() else {
                return;
            };
            cx.spawn(async move |this, cx| {
                let cleared = on_tokio(cx, async move { ws.clear().await }).await;
                this.update(cx, |this, cx| {
                    match cleared {
                        Some(Err(error)) => this.local.error = Some(store_message(&error)),
                        Some(Ok(())) => this.local.encryption_needs_clear = false,
                        None => {}
                    }
                    this.local.imports.clear();
                    this.refresh_local_data(cx);
                })
                .ok();
            })
            .detach();
        }
    }

    #[allow(clippy::large_enum_variant)]
    enum CopyNeeds {
        Ready(Box<Copy>),
        /// A connection query: the host resolves the profile first.
        Profile {
            target: CellRunTarget,
            sql: String,
            snapshot: Option<sqlly_services::notebook::CellOutput>,
            last: Option<(String, chrono::DateTime<chrono::Utc>)>,
        },
    }
}
